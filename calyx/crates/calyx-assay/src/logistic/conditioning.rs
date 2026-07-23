use calyx_core::SlotId;

use crate::group_split::GroupSplit;

use super::*;

pub const LOGISTIC_CONDITIONING_SCHEMA_VERSION: u32 = 4;
pub const LOGISTIC_CONDITIONING_METHOD: &str =
    "train_fold_lens_block_total_second_moment_no_center_converged_solver_v4";
const LOGISTIC_CONDITIONING_TRANSFORM: &str =
    "x / sqrt(sum(per_coordinate_mean_square(training_fold_lens_block)))";
const LOGISTIC_ZERO_VARIANCE_CHECK: &str =
    "sum(per_coordinate_population_variance(training_fold_lens_block)) > 0";
const LOGISTIC_SOLVER_METHOD: &str =
    "deterministic_full_batch_gradient_descent_l2_max_relative_parameter_change";
const LOGISTIC_NON_CONVERGENCE_POLICY: &str = "fail_closed:CALYX_ASSAY_ESTIMATOR_UNDERPOWERED";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogisticConditioningBlock {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<SlotId>,
    pub dim: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogisticConditioningScale {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<SlotId>,
    pub scale: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogisticFoldConditioning {
    pub seed: u64,
    pub train_rows: usize,
    pub heldout_rows: usize,
    pub block_scales: Vec<LogisticConditioningScale>,
    pub solver_iterations: usize,
    pub solver_final_relative_parameter_change_ppm_upper_bound: u32,
    pub solver_converged: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogisticConditioningProvenance {
    pub schema_version: u32,
    pub method: String,
    pub fit_scope: String,
    pub transform: String,
    pub centering: bool,
    pub zero_variance_check: String,
    pub zero_variance_policy: String,
    pub non_finite_policy: String,
    pub no_collapse: bool,
    pub solver: String,
    pub solver_max_iterations: usize,
    pub solver_convergence_check_interval: usize,
    pub solver_relative_parameter_tolerance: f32,
    pub solver_relative_parameter_tolerance_ppm: u32,
    pub solver_relative_parameter_evidence: String,
    pub solver_relative_parameter_evidence_max_ppm: u32,
    pub solver_learning_rate_policy: String,
    pub solver_learning_rate: f32,
    pub solver_learning_rate_cap: f32,
    pub solver_lipschitz_safety: f32,
    pub solver_l2_penalty: f32,
    pub solver_non_convergence_policy: String,
    pub blocks: Vec<LogisticConditioningBlock>,
    pub folds: Vec<LogisticFoldConditioning>,
}

impl Default for LogisticConditioningProvenance {
    fn default() -> Self {
        Self {
            schema_version: 0,
            method: "missing".to_string(),
            fit_scope: "missing".to_string(),
            transform: "missing".to_string(),
            centering: false,
            zero_variance_check: "missing".to_string(),
            zero_variance_policy: "missing".to_string(),
            non_finite_policy: "missing".to_string(),
            no_collapse: false,
            solver: "missing".to_string(),
            solver_max_iterations: 0,
            solver_convergence_check_interval: 0,
            solver_relative_parameter_tolerance: 0.0,
            solver_relative_parameter_tolerance_ppm: 0,
            solver_relative_parameter_evidence: "missing".to_string(),
            solver_relative_parameter_evidence_max_ppm: 0,
            solver_learning_rate_policy: "missing".to_string(),
            solver_learning_rate: 0.0,
            solver_learning_rate_cap: 0.0,
            solver_lipschitz_safety: 0.0,
            solver_l2_penalty: 0.0,
            solver_non_convergence_policy: "missing".to_string(),
            blocks: Vec::new(),
            folds: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LogisticBlock<'a> {
    pub(crate) name: &'a str,
    pub(crate) slot: Option<SlotId>,
    pub(crate) vectors: &'a [Vec<f32>],
}

impl<'a> LogisticBlock<'a> {
    pub(crate) fn unscoped(vectors: &'a [Vec<f32>]) -> Self {
        Self {
            name: "input",
            slot: None,
            vectors,
        }
    }

    pub(crate) fn new(name: &'a str, slot: SlotId, vectors: &'a [Vec<f32>]) -> Self {
        Self {
            name,
            slot: Some(slot),
            vectors,
        }
    }

    pub(crate) fn dim(self) -> usize {
        self.vectors.first().map(Vec::len).unwrap_or(0)
    }
}

pub(super) struct FittedConditioning {
    pub(super) block_scales: Vec<Vec<f32>>,
    pub(super) provenance: LogisticConditioningProvenance,
}

pub(crate) fn validate_conditioning_provenance(
    provenance: &LogisticConditioningProvenance,
    roster: &[(SlotId, &str)],
    n_samples: usize,
) -> Result<()> {
    if provenance.schema_version != LOGISTIC_CONDITIONING_SCHEMA_VERSION
        || provenance.method != LOGISTIC_CONDITIONING_METHOD
        || provenance.fit_scope != "training_fold_only"
        || provenance.transform != LOGISTIC_CONDITIONING_TRANSFORM
        || provenance.centering
        || provenance.zero_variance_check != LOGISTIC_ZERO_VARIANCE_CHECK
        || provenance.zero_variance_policy != "fail_closed:CALYX_ASSAY_DEGENERATE_INPUT"
        || provenance.non_finite_policy != "fail_closed:CALYX_FORGE_NUMERICAL_INVARIANT"
        || !provenance.no_collapse
        || provenance.solver != LOGISTIC_SOLVER_METHOD
        || provenance.solver_max_iterations != LOGISTIC_MAX_ITERATIONS
        || provenance.solver_convergence_check_interval != LOGISTIC_CONVERGENCE_CHECK_INTERVAL
        || provenance.solver_relative_parameter_tolerance != LOGISTIC_RELATIVE_PARAMETER_TOLERANCE
        || provenance.solver_relative_parameter_tolerance_ppm
            != LOGISTIC_RELATIVE_PARAMETER_TOLERANCE_PPM
        || provenance.solver_relative_parameter_evidence
            != "ceil(relative_parameter_change * 1000000):conservative_ppm_upper_bound"
        || provenance.solver_relative_parameter_evidence_max_ppm
            != LOGISTIC_RELATIVE_PARAMETER_EVIDENCE_MAX_PPM
        || provenance.solver_learning_rate_policy
            != "min(cap, lipschitz_safety / (0.25 * lens_block_count + l2_penalty))"
        || provenance.solver_learning_rate != logistic_learning_rate(roster.len())
        || provenance.solver_learning_rate_cap != LOGISTIC_LEARNING_RATE_CAP
        || provenance.solver_lipschitz_safety != LOGISTIC_LIPSCHITZ_SAFETY
        || provenance.solver_l2_penalty != LOGISTIC_L2
        || provenance.solver_non_convergence_policy != LOGISTIC_NON_CONVERGENCE_POLICY
    {
        return Err(CalyxError::assay_degenerate_input(format!(
            "EnsembleCard logistic conditioning provenance is not the current fail-closed no-collapse converged-solver contract: schema={} method={} fit_scope={} transform={} centering={} no_collapse={} solver={} max_iterations={} check_interval={} relative_parameter_tolerance={}",
            provenance.schema_version,
            provenance.method,
            provenance.fit_scope,
            provenance.transform,
            provenance.centering,
            provenance.no_collapse,
            provenance.solver,
            provenance.solver_max_iterations,
            provenance.solver_convergence_check_interval,
            provenance.solver_relative_parameter_tolerance
        )));
    }
    if provenance.blocks.len() != roster.len() {
        return Err(CalyxError::assay_degenerate_input(format!(
            "EnsembleCard conditioning block count {} != lens roster {}",
            provenance.blocks.len(),
            roster.len()
        )));
    }
    for (index, (block, &(slot, name))) in provenance.blocks.iter().zip(roster).enumerate() {
        if block.slot != Some(slot) || block.name != name || block.dim == 0 {
            return Err(CalyxError::assay_degenerate_input(format!(
                "EnsembleCard conditioning block {index} name={} slot={:?} dim={} does not match lens {} slot {}",
                block.name, block.slot, block.dim, name, slot
            )));
        }
    }
    if provenance.folds.len() != DEFAULT_ASSAY_SEEDS.len() {
        return Err(CalyxError::assay_degenerate_input(format!(
            "EnsembleCard conditioning folds {} != deterministic seeds {}",
            provenance.folds.len(),
            DEFAULT_ASSAY_SEEDS.len()
        )));
    }
    for (fit, (fold, &seed)) in provenance
        .folds
        .iter()
        .zip(&DEFAULT_ASSAY_SEEDS)
        .enumerate()
    {
        if fold.seed != seed
            || fold.train_rows == 0
            || fold.heldout_rows == 0
            || fold.train_rows.saturating_add(fold.heldout_rows) != n_samples
            || fold.block_scales.len() != roster.len()
            || !fold.solver_converged
            || fold.solver_iterations == 0
            || fold.solver_iterations > LOGISTIC_MAX_ITERATIONS
            || fold.solver_iterations % LOGISTIC_CONVERGENCE_CHECK_INTERVAL != 0
            || fold.solver_final_relative_parameter_change_ppm_upper_bound
                > LOGISTIC_RELATIVE_PARAMETER_EVIDENCE_MAX_PPM
        {
            return Err(CalyxError::assay_degenerate_input(format!(
                "EnsembleCard conditioning fold {fit} has invalid seed/row/scale/solver-convergence evidence"
            )));
        }
        for (index, (scale, &(slot, name))) in fold.block_scales.iter().zip(roster).enumerate() {
            if scale.slot != Some(slot)
                || scale.name != name
                || !scale.scale.is_finite()
                || scale.scale <= 0.0
            {
                return Err(CalyxError::assay_degenerate_input(format!(
                    "EnsembleCard conditioning fold {fit} block {index} has invalid scale/identity evidence"
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_blocks(
    context: &str,
    blocks: &[LogisticBlock<'_>],
) -> Result<(usize, usize)> {
    let Some(first) = blocks.first() else {
        return Err(CalyxError::assay_degenerate_input(format!(
            "{context} requires at least one distinct lens block"
        )));
    };
    let rows = first.vectors.len();
    if rows == 0 {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "{context} requires at least one sample row"
        )));
    }
    let mut total_dim = 0_usize;
    for (block_idx, block) in blocks.iter().copied().enumerate() {
        if block.name.trim().is_empty() {
            return Err(CalyxError::assay_degenerate_input(format!(
                "{context} lens block {block_idx} has a blank name"
            )));
        }
        if block.vectors.len() != rows {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "{context} lens block {} rows {} != {rows}",
                block.name,
                block.vectors.len()
            )));
        }
        let dim = block.dim();
        if dim == 0 {
            return Err(CalyxError::assay_degenerate_input(format!(
                "{context} lens block {} has zero dimensions",
                block.name
            )));
        }
        total_dim = total_dim.checked_add(dim).ok_or_else(|| {
            CalyxError::assay_degenerate_input(format!(
                "{context} total lens-block dimension overflow"
            ))
        })?;
        for (row_idx, row) in block.vectors.iter().enumerate() {
            if row.len() != dim {
                return Err(CalyxError::lens_dim_mismatch(format!(
                    "{context} lens block {} row {row_idx} dim {} != {dim}",
                    block.name,
                    row.len()
                )));
            }
            for (col_idx, &value) in row.iter().enumerate() {
                if !value.is_finite() {
                    return Err(CalyxError::forge_numerical_invariant(format!(
                        "{context} lens block {} row {row_idx} col {col_idx} is non-finite: {value}",
                        block.name
                    )));
                }
            }
        }
    }
    Ok((rows, total_dim))
}

pub(super) fn fit_conditioning(
    blocks: &[LogisticBlock<'_>],
    splits: &[GroupSplit],
) -> Result<FittedConditioning> {
    if splits.len() != DEFAULT_ASSAY_SEEDS.len() {
        return Err(CalyxError::assay_degenerate_input(format!(
            "logistic conditioning split count {} != seed count {}",
            splits.len(),
            DEFAULT_ASSAY_SEEDS.len()
        )));
    }
    let mut all_scales = Vec::with_capacity(splits.len());
    let mut folds = Vec::with_capacity(splits.len());
    for ((fit_idx, split), seed) in splits.iter().enumerate().zip(DEFAULT_ASSAY_SEEDS) {
        let mut scales = Vec::with_capacity(blocks.len());
        let mut evidence = Vec::with_capacity(blocks.len());
        for block in blocks {
            let scale = fit_block_scale(block, &split.train, seed, fit_idx)?;
            validate_transformed_block(block, scale, seed, fit_idx)?;
            scales.push(scale);
            evidence.push(LogisticConditioningScale {
                name: block.name.to_string(),
                slot: block.slot,
                scale,
            });
        }
        all_scales.push(scales);
        folds.push(LogisticFoldConditioning {
            seed,
            train_rows: split.train.len(),
            heldout_rows: split.test.len(),
            block_scales: evidence,
            solver_iterations: 0,
            solver_final_relative_parameter_change_ppm_upper_bound: u32::MAX,
            solver_converged: false,
        });
    }
    Ok(FittedConditioning {
        block_scales: all_scales,
        provenance: LogisticConditioningProvenance {
            schema_version: LOGISTIC_CONDITIONING_SCHEMA_VERSION,
            method: LOGISTIC_CONDITIONING_METHOD.to_string(),
            fit_scope: "training_fold_only".to_string(),
            transform: LOGISTIC_CONDITIONING_TRANSFORM.to_string(),
            centering: false,
            zero_variance_check: LOGISTIC_ZERO_VARIANCE_CHECK.to_string(),
            zero_variance_policy: "fail_closed:CALYX_ASSAY_DEGENERATE_INPUT".to_string(),
            non_finite_policy: "fail_closed:CALYX_FORGE_NUMERICAL_INVARIANT".to_string(),
            no_collapse: true,
            solver: LOGISTIC_SOLVER_METHOD.to_string(),
            solver_max_iterations: LOGISTIC_MAX_ITERATIONS,
            solver_convergence_check_interval: LOGISTIC_CONVERGENCE_CHECK_INTERVAL,
            solver_relative_parameter_tolerance: LOGISTIC_RELATIVE_PARAMETER_TOLERANCE,
            solver_relative_parameter_tolerance_ppm: LOGISTIC_RELATIVE_PARAMETER_TOLERANCE_PPM,
            solver_relative_parameter_evidence:
                "ceil(relative_parameter_change * 1000000):conservative_ppm_upper_bound".to_string(),
            solver_relative_parameter_evidence_max_ppm:
                LOGISTIC_RELATIVE_PARAMETER_EVIDENCE_MAX_PPM,
            solver_learning_rate_policy:
                "min(cap, lipschitz_safety / (0.25 * lens_block_count + l2_penalty))".to_string(),
            solver_learning_rate: logistic_learning_rate(blocks.len()),
            solver_learning_rate_cap: LOGISTIC_LEARNING_RATE_CAP,
            solver_lipschitz_safety: LOGISTIC_LIPSCHITZ_SAFETY,
            solver_l2_penalty: LOGISTIC_L2,
            solver_non_convergence_policy: LOGISTIC_NON_CONVERGENCE_POLICY.to_string(),
            blocks: blocks
                .iter()
                .map(|block| LogisticConditioningBlock {
                    name: block.name.to_string(),
                    slot: block.slot,
                    dim: block.dim(),
                })
                .collect(),
            folds,
        },
    })
}

fn fit_block_scale(
    block: &LogisticBlock<'_>,
    train: &[usize],
    seed: u64,
    fit_idx: usize,
) -> Result<f32> {
    if train.is_empty() {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "logistic conditioning seed {seed} fit {fit_idx} has an empty training fold"
        )));
    }
    if train.len() < 2 {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "logistic conditioning seed {seed} lens block {} needs at least two training rows",
            block.name
        )));
    }
    let dim = block.dim();
    let mut means = vec![0.0_f64; dim];
    let mut m2 = vec![0.0_f64; dim];
    for (sample_idx, &row_idx) in train.iter().enumerate() {
        let row = block.vectors.get(row_idx).ok_or_else(|| {
            CalyxError::assay_insufficient_samples(format!(
                "logistic conditioning seed {seed} lens block {} training row {row_idx} is out of range",
                block.name
            ))
        })?;
        let count = (sample_idx + 1) as f64;
        for (col, &value) in row.iter().enumerate() {
            let value = f64::from(value);
            let delta = value - means[col];
            means[col] += delta / count;
            let delta_two = value - means[col];
            m2[col] += delta * delta_two;
        }
    }
    let value_count = train.len().checked_mul(dim).ok_or_else(|| {
        CalyxError::assay_degenerate_input(format!(
            "logistic conditioning sample count overflow for lens block {}",
            block.name
        ))
    })?;
    let total_variation = m2.iter().sum::<f64>() / train.len() as f64;
    if !total_variation.is_finite() {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "logistic conditioning seed {seed} lens block {} produced non-finite total variation {total_variation}",
            block.name
        )));
    }
    if total_variation <= 0.0 {
        return Err(CalyxError::assay_degenerate_input(format!(
            "logistic conditioning seed {seed} lens block {}{} has zero variance across {} training values",
            block.name,
            block
                .slot
                .map(|slot| format!(" slot {slot}"))
                .unwrap_or_default(),
            value_count
        )));
    }
    let squared_mean_energy = means.iter().map(|mean| mean * mean).sum::<f64>();
    let total_second_moment = total_variation + squared_mean_energy;
    if !total_second_moment.is_finite() || total_second_moment <= 0.0 {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "logistic conditioning seed {seed} lens block {} produced invalid total second moment {total_second_moment} from variation {total_variation} and squared-mean energy {squared_mean_energy}",
            block.name
        )));
    }
    let scale64 = total_second_moment.sqrt();
    let scale = scale64 as f32;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "logistic conditioning seed {seed} lens block {} scale {scale64} is not representable as positive finite f32",
            block.name
        )));
    }
    Ok(scale)
}

fn validate_transformed_block(
    block: &LogisticBlock<'_>,
    scale: f32,
    seed: u64,
    fit_idx: usize,
) -> Result<()> {
    for (row_idx, row) in block.vectors.iter().enumerate() {
        for (col_idx, &value) in row.iter().enumerate() {
            let conditioned = value / scale;
            if !conditioned.is_finite() {
                return Err(CalyxError::forge_numerical_invariant(format!(
                    "logistic conditioning seed {seed} fit {fit_idx} lens block {} row {row_idx} col {col_idx} produced non-finite {value}/{scale}",
                    block.name
                )));
            }
        }
    }
    Ok(())
}
