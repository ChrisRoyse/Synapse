use crate::group_split::GroupSplit;

use super::*;

pub(super) fn flatten_logistic_blocks(
    blocks: &[LogisticBlock<'_>],
    rows: usize,
    total_dim: usize,
) -> Result<Vec<f32>> {
    let capacity = rows
        .checked_mul(total_dim)
        .ok_or_else(|| CalyxError::forge_vram_budget("logistic CUDA flat sample overflow"))?;
    let mut flat = Vec::with_capacity(capacity);
    for block in blocks {
        let dim = block.dim();
        for (row_idx, row) in block.vectors.iter().enumerate() {
            if row.len() != dim {
                return Err(CalyxError::lens_dim_mismatch(format!(
                    "logistic CUDA lens block {} row {row_idx} has dim {}, expected {dim}",
                    block.name,
                    row.len()
                )));
            }
            for (col_idx, &value) in row.iter().enumerate() {
                if !value.is_finite() {
                    return Err(CalyxError::forge_numerical_invariant(format!(
                        "logistic CUDA lens block {} row {row_idx} col {col_idx} is non-finite: {value}",
                        block.name
                    )));
                }
                flat.push(value);
            }
        }
    }
    if flat.len() != capacity {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "logistic CUDA block-major values {} != rows {rows} * total_dim {total_dim}",
            flat.len()
        )));
    }
    Ok(flat)
}

type LogisticCudaSplitBuffers = (Vec<i32>, Vec<i32>, Vec<i32>, Vec<i32>);

pub(super) fn split_buffers_for_cuda(
    splits: &[GroupSplit],
    n_samples: usize,
) -> Result<LogisticCudaSplitBuffers> {
    let mut train_offsets = vec![0];
    let mut train_indices = Vec::new();
    let mut test_offsets = vec![0];
    let mut test_indices = Vec::new();
    for (fit, split) in splits.iter().enumerate() {
        push_split_for_cuda(
            "train",
            fit,
            &split.train,
            n_samples,
            &mut train_offsets,
            &mut train_indices,
        )?;
        push_split_for_cuda(
            "test",
            fit,
            &split.test,
            n_samples,
            &mut test_offsets,
            &mut test_indices,
        )?;
    }
    Ok((train_offsets, train_indices, test_offsets, test_indices))
}

fn push_split_for_cuda(
    name: &'static str,
    fit: usize,
    rows: &[usize],
    n_samples: usize,
    offsets: &mut Vec<i32>,
    indices: &mut Vec<i32>,
) -> Result<()> {
    if rows.is_empty() {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "logistic CUDA {name} split {fit} is empty"
        )));
    }
    for &row in rows {
        if row >= n_samples {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "logistic CUDA {name} split {fit} contains row {row}, n_samples={n_samples}"
            )));
        }
        indices.push(usize_to_i32_for_cuda(row, "logistic row index")?);
    }
    offsets.push(usize_to_i32_for_cuda(
        indices.len(),
        "logistic split offset",
    )?);
    Ok(())
}

fn usize_to_i32_for_cuda(value: usize, name: &'static str) -> Result<i32> {
    i32::try_from(value).map_err(|_| {
        CalyxError::assay_insufficient_samples(format!(
            "{name} exceeds CUDA i32 index range: {value}"
        ))
    })
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LogisticCudaInputs<'a> {
    pub(super) samples: &'a [f32],
    pub(super) labels: &'a [i32],
    pub(super) rows: usize,
    pub(super) dim: usize,
    pub(super) blocks: &'a [LogisticBlock<'a>],
    pub(super) block_scales: &'a [Vec<f32>],
    pub(super) train_offsets: &'a [i32],
    pub(super) train_indices: &'a [i32],
    pub(super) test_offsets: &'a [i32],
    pub(super) test_indices: &'a [i32],
}

#[cfg(feature = "cuda")]
pub(super) fn logistic_summaries_cuda_strict_impl(
    input: LogisticCudaInputs<'_>,
) -> Result<calyx_forge::CudaLogisticSummaries> {
    let (block_value_offsets, block_feature_offsets, block_dims) =
        cuda_block_layout(input.blocks, input.rows)?;
    let block_scales = flatten_block_scales(input.block_scales, input.blocks.len())?;
    let backend = calyx_forge::CudaBackend::new()
        .map_err(|err| crate::cuda_strict::forge_to_calyx("logistic probe", err))?;
    let summaries = calyx_forge::logistic_summaries_host(
        backend.context(),
        calyx_forge::CudaLogisticDataset {
            samples: input.samples,
            labels: input.labels,
            rows: input.rows,
            dim: input.dim,
            block_value_offsets: &block_value_offsets,
            block_feature_offsets: &block_feature_offsets,
            block_dims: &block_dims,
            block_scales: &block_scales,
        },
        calyx_forge::CudaLogisticSplits {
            train_offsets: input.train_offsets,
            train_indices: input.train_indices,
            test_offsets: input.test_offsets,
            test_indices: input.test_indices,
        },
        calyx_forge::CudaLogisticConfig {
            max_iterations: LOGISTIC_MAX_ITERATIONS,
            convergence_check_interval: LOGISTIC_CONVERGENCE_CHECK_INTERVAL,
            relative_parameter_tolerance: LOGISTIC_RELATIVE_PARAMETER_TOLERANCE,
            learning_rate: logistic_learning_rate(input.blocks.len()),
            l2_penalty: LOGISTIC_L2,
        },
    )
    .map_err(|err| crate::cuda_strict::forge_to_calyx("logistic probe", err))?;
    if summaries.iterations.len() != summaries.converged.len()
        || summaries.iterations.len() != summaries.final_relative_parameter_changes.len()
    {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "logistic CUDA convergence readback lengths disagree: iterations={} flags={} gradients={}",
            summaries.iterations.len(),
            summaries.converged.len(),
            summaries.final_relative_parameter_changes.len()
        )));
    }
    for fit in 0..summaries.iterations.len() {
        if !summaries.converged[fit] {
            let seed = DEFAULT_ASSAY_SEEDS.get(fit).copied().unwrap_or_default();
            return Err(crate::calibration::underpowered(format!(
                "logistic CUDA seed {seed} fit {fit} did not converge: final max relative parameter change {:.9} exceeds tolerance {:.9} after {} iterations; lens_blocks={} total_features={}",
                summaries.final_relative_parameter_changes[fit],
                LOGISTIC_RELATIVE_PARAMETER_TOLERANCE,
                summaries.iterations[fit],
                input.blocks.len(),
                input.dim
            )));
        }
    }
    Ok(summaries)
}

#[cfg(feature = "cuda")]
fn cuda_block_layout(
    blocks: &[LogisticBlock<'_>],
    rows: usize,
) -> Result<(Vec<i32>, Vec<i32>, Vec<i32>)> {
    let mut value_offsets = Vec::with_capacity(blocks.len() + 1);
    let mut feature_offsets = Vec::with_capacity(blocks.len() + 1);
    let mut dims = Vec::with_capacity(blocks.len());
    value_offsets.push(0);
    feature_offsets.push(0);
    let mut values = 0_usize;
    let mut features = 0_usize;
    for block in blocks {
        let dim = block.dim();
        values = values
            .checked_add(rows.checked_mul(dim).ok_or_else(|| {
                CalyxError::forge_vram_budget("logistic CUDA block value span overflow")
            })?)
            .ok_or_else(|| {
                CalyxError::forge_vram_budget("logistic CUDA block value offset overflow")
            })?;
        features = features.checked_add(dim).ok_or_else(|| {
            CalyxError::forge_vram_budget("logistic CUDA block feature offset overflow")
        })?;
        dims.push(usize_to_i32_for_cuda(dim, "logistic block dimension")?);
        value_offsets.push(usize_to_i32_for_cuda(
            values,
            "logistic block value offset",
        )?);
        feature_offsets.push(usize_to_i32_for_cuda(
            features,
            "logistic block feature offset",
        )?);
    }
    Ok((value_offsets, feature_offsets, dims))
}

#[cfg(feature = "cuda")]
fn flatten_block_scales(scales: &[Vec<f32>], block_count: usize) -> Result<Vec<f32>> {
    let mut flat = Vec::with_capacity(
        scales
            .len()
            .checked_mul(block_count)
            .ok_or_else(|| CalyxError::forge_vram_budget("logistic CUDA scale span overflow"))?,
    );
    for (fit, fit_scales) in scales.iter().enumerate() {
        if fit_scales.len() != block_count {
            return Err(CalyxError::assay_degenerate_input(format!(
                "logistic CUDA fit {fit} scale count {} != block count {block_count}",
                fit_scales.len()
            )));
        }
        for (block, &scale) in fit_scales.iter().enumerate() {
            if !scale.is_finite() || scale <= 0.0 {
                return Err(CalyxError::forge_numerical_invariant(format!(
                    "logistic CUDA fit {fit} block {block} has invalid scale {scale}"
                )));
            }
            flat.push(scale);
        }
    }
    Ok(flat)
}

#[cfg(not(feature = "cuda"))]
pub(super) fn logistic_summaries_cuda_strict_impl(
    input: LogisticCudaInputs<'_>,
) -> Result<UnavailableCudaLogisticSummaries> {
    Err(cuda_unavailable(&format!(
        "logistic probe (rows={}, dim={}, blocks={}, block_scale_fits={}, sample_values={}, labels={}, train_offsets={}, train_indices={}, test_offsets={}, test_indices={})",
        input.rows,
        input.dim,
        input.blocks.len(),
        input.block_scales.len(),
        input.samples.len(),
        input.labels.len(),
        input.train_offsets.len(),
        input.train_indices.len(),
        input.test_offsets.len(),
        input.test_indices.len()
    )))
}

#[cfg(not(feature = "cuda"))]
pub(super) struct UnavailableCudaLogisticSummaries {
    pub(super) bits: Vec<f32>,
    pub(super) accuracy: Vec<f32>,
    pub(super) iterations: Vec<usize>,
    pub(super) final_relative_parameter_changes: Vec<f32>,
    pub(super) converged: Vec<bool>,
}
