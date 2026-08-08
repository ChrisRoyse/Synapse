use std::str;
use std::sync::Arc;

use cudarc::driver::{CudaModule, CudaSlice, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;

use crate::cuda::kernels::{TOPK_CUBIN, TOPK_PTX};
use crate::{CUDA_EXACT_TOPK_MAX_K, CudaContext, ForgeError, Result};

const TOPK_BLOCK: usize = CUDA_EXACT_TOPK_MAX_K;
/// Candidate pairs collapsed by one device merge block. Must match
/// `TOPK_MERGE_TILE` in `kernels/topk.cu`.
const TOPK_MERGE_TILE: usize = 2 * TOPK_BLOCK;
const TOPK_FLAG_NONFINITE: u32 = 1;
/// Grid `y` is the row (query) axis; CUDA caps it at 65535.
pub(crate) const TOPK_MAX_ROWS_PER_LAUNCH: usize = 65_535;
const TOPK_REMEDIATION: &str =
    "Reject non-finite scores and keep deterministic score/index ordering";
const DEVICE_REMEDIATION: &str = "Check CUDA, embedded topk PTX, and CUDA GPU device availability";

/// Ranked device top-k for a batch of score rows.
///
/// `indices` and `scores` are row-major `rows * k`; index values are relative
/// to their own row, matching the single-row contract.
pub(crate) struct TopkBatched {
    pub indices: Vec<usize>,
    pub scores: Vec<f32>,
}

/// Sort direction the device kernels apply before ranking.
///
/// `MinK` is the exact device equivalent of what `cpu::rank_scores` does for
/// `KnnMetric::L2Squared`: negate, take the top-k, negate back. Negation of a
/// finite `f32` is exact, so the two paths select the same rows in the same
/// order and return bit-identical scores.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TopkOrder {
    MaxK,
    MinK,
}

impl TopkOrder {
    const fn sign(self) -> f32 {
        match self {
            Self::MaxK => 1.0,
            Self::MinK => -1.0,
        }
    }
}

pub fn topk_gpu(
    ctx: &CudaContext,
    scores: &CudaSlice<f32>,
    k: usize,
    n: usize,
) -> Result<Vec<(usize, f32)>> {
    check_device_len(scores.len(), n)?;
    let batch = topk_batched_gpu(ctx, scores, 1, n, k, TopkOrder::MaxK)?;
    Ok(batch
        .indices
        .into_iter()
        .zip(batch.scores)
        .collect::<Vec<_>>())
}

pub fn topk_host(ctx: &CudaContext, scores: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
    if k == 0 || scores.is_empty() {
        return Ok(Vec::new());
    }
    let stream = ctx.inner().default_stream();
    let scores_dev = stream
        .clone_htod(scores)
        .map_err(|err| device_unavailable(ctx, format!("topk scores copy failed: {err}")))?;
    topk_gpu(ctx, &scores_dev, k, scores.len())
}

/// Ranks `rows` independent score rows of length `n` entirely on the device.
///
/// The only host transfer is the final `rows * k` result — every intermediate
/// merge level stays in device memory. This is the fix for #2107 H1/H2: the
/// previous implementation read `chunks * k` candidates back per query and
/// merged them in a host `BinaryHeap`, and the L2 metric read *every* candidate
/// score back and ran a full `Vec::sort_by`.
pub(crate) fn topk_batched_gpu(
    ctx: &CudaContext,
    scores: &CudaSlice<f32>,
    rows: usize,
    n: usize,
    k: usize,
    order: TopkOrder,
) -> Result<TopkBatched> {
    let expected = rows.checked_mul(n).ok_or_else(|| {
        shape_error(
            vec![rows, n],
            vec![scores.len()],
            "cuda topk score matrix shape overflows usize",
        )
    })?;
    check_device_len(scores.len(), expected)?;
    let k_eff = k.min(n);
    if rows == 0 || k_eff == 0 || n == 0 {
        return Ok(TopkBatched {
            indices: Vec::new(),
            scores: Vec::new(),
        });
    }
    if k_eff > TOPK_BLOCK {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![TOPK_BLOCK],
            got: vec![k_eff],
            remediation: format!(
                "cuda topk is exact only for global k <= {CUDA_EXACT_TOPK_MAX_K}; use CPU topk or add a multi-pass exact CUDA merge"
            ),
        });
    }
    if rows > TOPK_MAX_ROWS_PER_LAUNCH {
        return Err(shape_error(
            vec![TOPK_MAX_ROWS_PER_LAUNCH],
            vec![rows],
            "cuda topk row batch exceeds the CUDA grid.y limit; tile the query batch",
        ));
    }

    let stream = ctx.inner().default_stream();
    let mut flags: CudaSlice<u32> = stream
        .alloc_zeros(1)
        .map_err(|err| device_unavailable(ctx, format!("topk flag allocation failed: {err}")))?;

    let mut chunks = n.div_ceil(TOPK_BLOCK);
    let mut level_len = checked_level_len(rows, chunks, k_eff)?;
    let mut cur_indices = stream
        .alloc_zeros(level_len)
        .map_err(|err| device_unavailable(ctx, format!("topk index allocation failed: {err}")))?;
    let mut cur_scores = stream
        .alloc_zeros(level_len)
        .map_err(|err| device_unavailable(ctx, format!("topk score allocation failed: {err}")))?;

    launch_topk_rank(
        ctx,
        scores,
        n,
        k_eff,
        order.sign(),
        chunks,
        rows,
        &mut cur_indices,
        &mut cur_scores,
        &mut flags,
    )?;

    while chunks > 1 {
        let count = chunks.checked_mul(k_eff).ok_or_else(|| {
            shape_error(
                vec![chunks, k_eff],
                vec![usize::MAX],
                "cuda topk merge level shape overflows usize",
            )
        })?;
        let next_chunks = count.div_ceil(TOPK_MERGE_TILE);
        if next_chunks >= chunks {
            return Err(shape_error(
                vec![chunks],
                vec![next_chunks],
                "cuda topk device merge failed to reduce the candidate level",
            ));
        }
        let next_len = checked_level_len(rows, next_chunks, k_eff)?;
        let mut next_indices = stream.alloc_zeros(next_len).map_err(|err| {
            device_unavailable(ctx, format!("topk merge index allocation failed: {err}"))
        })?;
        let mut next_scores = stream.alloc_zeros(next_len).map_err(|err| {
            device_unavailable(ctx, format!("topk merge score allocation failed: {err}"))
        })?;
        launch_topk_merge(
            ctx,
            &cur_scores,
            &cur_indices,
            count,
            k_eff,
            next_chunks,
            rows,
            &mut next_indices,
            &mut next_scores,
        )?;
        cur_indices = next_indices;
        cur_scores = next_scores;
        chunks = next_chunks;
        level_len = next_len;
    }

    stream
        .synchronize()
        .map_err(|err| device_unavailable(ctx, format!("topk stream sync failed: {err}")))?;

    // Refuse before any result is produced: the non-finite flag is read and
    // decoded ahead of the ranked readback.
    let flag_values = stream
        .clone_dtoh(&flags)
        .map_err(|err| device_unavailable(ctx, format!("topk flag readback failed: {err}")))?;
    if flag_values.first().copied().unwrap_or(0) & TOPK_FLAG_NONFINITE != 0 {
        return Err(numerical(
            "topk_gpu",
            "device rank kernel reported a non-finite score in the input".to_string(),
        ));
    }

    debug_assert_eq!(level_len, rows * k_eff);
    let raw_indices = stream
        .clone_dtoh(&cur_indices)
        .map_err(|err| device_unavailable(ctx, format!("topk index readback failed: {err}")))?;
    let raw_scores = stream
        .clone_dtoh(&cur_scores)
        .map_err(|err| device_unavailable(ctx, format!("topk score readback failed: {err}")))?;

    let sign = order.sign();
    let mut indices = Vec::with_capacity(raw_indices.len());
    let mut scores_out = Vec::with_capacity(raw_scores.len());
    for (position, (index, score)) in raw_indices.iter().copied().zip(raw_scores).enumerate() {
        if index < 0 {
            return Err(device_unavailable(
                ctx,
                format!("topk kernel returned negative index {index} at output {position}"),
            ));
        }
        let index = index as usize;
        if index >= n {
            return Err(device_unavailable(
                ctx,
                format!("topk kernel returned out-of-range index {index}"),
            ));
        }
        if !score.is_finite() {
            return Err(numerical(
                "topk_gpu",
                format!("non-finite score at output {position}: {score}"),
            ));
        }
        indices.push(index);
        scores_out.push(score * sign);
    }
    Ok(TopkBatched {
        indices,
        scores: scores_out,
    })
}

/// Peak simultaneous device entries (index + score pairs) the merge ladder
/// holds for one launch batch. Used by the VRAM admission measurement so the
/// reserved shape is the shape the kernels actually allocate.
pub(crate) fn topk_device_entry_peak(rows: usize, n: usize, k: usize) -> usize {
    let k_eff = k.min(n);
    if rows == 0 || k_eff == 0 || n == 0 {
        return 0;
    }
    let mut chunks = n.div_ceil(TOPK_BLOCK);
    let mut peak = rows.saturating_mul(chunks).saturating_mul(k_eff);
    let mut current = peak;
    while chunks > 1 {
        let count = chunks.saturating_mul(k_eff);
        let next_chunks = count.div_ceil(TOPK_MERGE_TILE);
        if next_chunks >= chunks {
            break;
        }
        let next = rows.saturating_mul(next_chunks).saturating_mul(k_eff);
        peak = peak.max(current.saturating_add(next));
        current = next;
        chunks = next_chunks;
    }
    peak
}

fn checked_level_len(rows: usize, chunks: usize, k: usize) -> Result<usize> {
    rows.checked_mul(chunks)
        .and_then(|value| value.checked_mul(k))
        .ok_or_else(|| {
            shape_error(
                vec![rows, chunks, k],
                vec![usize::MAX],
                "cuda topk output shape overflows usize",
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn launch_topk_rank(
    ctx: &CudaContext,
    scores: &CudaSlice<f32>,
    n: usize,
    k: usize,
    sign: f32,
    chunks: usize,
    rows: usize,
    out_indices: &mut CudaSlice<i32>,
    out_scores: &mut CudaSlice<f32>,
    flags: &mut CudaSlice<u32>,
) -> Result<()> {
    let n_i32 = to_i32(n, "count")?;
    let k_i32 = to_i32(k, "k")?;
    let grid = grid_dims(chunks, rows)?;
    let module = topk_module(ctx)?;
    let func = ctx
        .cached_function(&module, "topk.bitonic_topk_f32", "bitonic_topk_f32")
        .map_err(|err| device_unavailable(ctx, format!("topk load function failed: {err}")))?;
    let stream = ctx.inner().default_stream();
    let cfg = LaunchConfig {
        grid_dim: grid,
        block_dim: (TOPK_BLOCK as u32, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(scores)
            .arg(&n_i32)
            .arg(&k_i32)
            .arg(&sign)
            .arg(out_indices)
            .arg(out_scores)
            .arg(flags)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("topk kernel launch failed: {err}")))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn launch_topk_merge(
    ctx: &CudaContext,
    in_scores: &CudaSlice<f32>,
    in_indices: &CudaSlice<i32>,
    count: usize,
    k: usize,
    chunks: usize,
    rows: usize,
    out_indices: &mut CudaSlice<i32>,
    out_scores: &mut CudaSlice<f32>,
) -> Result<()> {
    let count_i32 = to_i32(count, "merge count")?;
    let k_i32 = to_i32(k, "k")?;
    let grid = grid_dims(chunks, rows)?;
    let module = topk_module(ctx)?;
    let func = ctx
        .cached_function(
            &module,
            "topk.bitonic_topk_merge_f32",
            "bitonic_topk_merge_f32",
        )
        .map_err(|err| {
            device_unavailable(ctx, format!("topk merge load function failed: {err}"))
        })?;
    let stream = ctx.inner().default_stream();
    let cfg = LaunchConfig {
        grid_dim: grid,
        block_dim: (TOPK_BLOCK as u32, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(in_scores)
            .arg(in_indices)
            .arg(&count_i32)
            .arg(&k_i32)
            .arg(out_indices)
            .arg(out_scores)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("topk merge kernel launch failed: {err}")))?;
    Ok(())
}

fn grid_dims(chunks: usize, rows: usize) -> Result<(u32, u32, u32)> {
    let chunks_u32 = u32::try_from(chunks).map_err(|_| {
        shape_error(
            vec![u32::MAX as usize],
            vec![chunks],
            "cuda topk chunk count exceeds grid dimension limit",
        )
    })?;
    let rows_u32 = u32::try_from(rows).map_err(|_| {
        shape_error(
            vec![TOPK_MAX_ROWS_PER_LAUNCH],
            vec![rows],
            "cuda topk row count exceeds grid dimension limit",
        )
    })?;
    Ok((chunks_u32, rows_u32, 1))
}

fn topk_module(ctx: &CudaContext) -> Result<Arc<CudaModule>> {
    if let Some(module) = ctx.topk_module_cache().get() {
        return Ok(module.clone());
    }
    match ctx
        .inner()
        .load_module(Ptx::from_binary(TOPK_CUBIN.to_vec()))
    {
        Ok(module) => {
            let _ = ctx.topk_module_cache().set(module.clone());
            Ok(module)
        }
        Err(cubin_err) => {
            let module = topk_ptx_module(ctx, cubin_err)?;
            let _ = ctx.topk_module_cache().set(module.clone());
            Ok(module)
        }
    }
}

fn topk_ptx_module(
    ctx: &CudaContext,
    cubin_err: cudarc::driver::DriverError,
) -> Result<Arc<CudaModule>> {
    let ptx = str::from_utf8(TOPK_PTX)
        .map_err(|err| device_unavailable(ctx, format!("topk PTX is not UTF-8: {err}")))?;
    ctx.inner()
        .load_module(Ptx::from_src(ptx))
        .map_err(|ptx_err| {
            device_unavailable(
                ctx,
                format!("topk CUBIN load failed: {cubin_err}; PTX fallback load failed: {ptx_err}"),
            )
        })
}

fn check_device_len(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(ForgeError::ShapeMismatch {
        expected: vec![expected],
        got: vec![actual],
        remediation: "cuda topk scores length must equal rows*n".to_string(),
    })
}

fn to_i32(value: usize, name: &str) -> Result<i32> {
    i32::try_from(value).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![i32::MAX as usize],
        got: vec![value],
        remediation: format!("cuda topk {name} exceeds i32 kernel argument limit"),
    })
}

fn shape_error(expected: Vec<usize>, got: Vec<usize>, remediation: &str) -> ForgeError {
    ForgeError::ShapeMismatch {
        expected,
        got,
        remediation: remediation.to_string(),
    }
}

fn numerical(op: &'static str, detail: String) -> ForgeError {
    ForgeError::NumericalInvariant {
        op: op.to_string(),
        detail,
        remediation: TOPK_REMEDIATION.to_string(),
    }
}

fn device_unavailable(ctx: &CudaContext, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: format!("cuda:{}", ctx.device_idx()),
        detail,
        remediation: DEVICE_REMEDIATION.to_string(),
    }
}
