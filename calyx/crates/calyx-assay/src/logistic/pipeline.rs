use calyx_core::{CalyxError, Result};

use crate::calibration::ensure_informative_binary_labels;
use crate::cuda_strict::strict_cuda_requested;
use crate::estimate::{EstimateReliability, EstimatorKind, MiEstimate, TrustTag};
use crate::group_split::{group_holdout_split, row_groups};
use crate::ksg::MIN_ASSAY_SAMPLES;

use super::calibration;
use super::conditioning::{
    LogisticBlock, LogisticConditioningProvenance, fit_conditioning, validate_blocks,
};
use super::cuda::{
    LogisticCudaInputs, flatten_logistic_blocks, logistic_summaries_cuda_strict_impl,
    split_buffers_for_cuda,
};
use super::train::{LogisticSummary, logistic_heldout_summary, mean, sample_sigma, seed_ci};
use super::{DEFAULT_ASSAY_SEEDS, DEFAULT_HOLDOUT_FRACTION, LogisticProbeReport};

pub(super) fn logistic_probe_mi_multiseed_with_trust_and_min_samples(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    let blocks = [LogisticBlock::unscoped(samples)];
    logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples(
        &blocks,
        labels,
        groups,
        trust,
        min_samples,
    )
}

// An *uncalibrated* multi-block entry point deliberately does not exist
// (#1942). Every multi-block estimate on the ensemble path is differenced
// against another one, and the calibrated wrapper is the positive control that
// proves the probe can recover a planted signal at that call's own dimension.
// A caller that skips it gets a number whose weakness is indistinguishable from
// an absence of signal, which is exactly the defect #1942 records.

pub(super) fn logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    if strict_cuda_requested() {
        return logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples_cuda_strict(
            blocks,
            labels,
            groups,
            trust,
            min_samples,
        );
    }
    let (rows, _) = validate_blocks("logistic", blocks)?;
    if rows != labels.len() || rows < min_samples {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "need at least {min_samples} labeled samples"
        )));
    }
    let owned_groups;
    let groups = match groups {
        Some(groups) => groups,
        None => {
            owned_groups = row_groups(labels.len());
            &owned_groups
        }
    };
    let mut splits = Vec::with_capacity(DEFAULT_ASSAY_SEEDS.len());
    for seed in DEFAULT_ASSAY_SEEDS {
        splits.push(group_holdout_split(
            labels,
            groups,
            DEFAULT_HOLDOUT_FRACTION,
            seed,
        )?);
    }
    let conditioning = fit_conditioning(blocks, &splits)?;
    let mut seed_summaries = Vec::with_capacity(DEFAULT_ASSAY_SEEDS.len());
    for ((split, scales), seed) in splits
        .iter()
        .zip(&conditioning.block_scales)
        .zip(DEFAULT_ASSAY_SEEDS)
    {
        seed_summaries.push(logistic_heldout_summary(
            blocks, labels, split, scales, seed,
        )?);
    }
    report_from_seed_summaries(seed_summaries, labels.len(), trust, conditioning.provenance)
}

pub(super) fn logistic_probe_mi_multiseed_with_trust_and_min_samples_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    let blocks = [LogisticBlock::unscoped(samples)];
    logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples_cuda_strict(
        &blocks,
        labels,
        groups,
        trust,
        min_samples,
    )
}

pub(super) fn logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples_cuda_strict(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    let (rows, dim) = validate_blocks("logistic CUDA", blocks)?;
    if rows != labels.len() || rows < min_samples {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "need at least {min_samples} labeled samples"
        )));
    }
    let owned_groups;
    let groups = match groups {
        Some(groups) => groups,
        None => {
            owned_groups = row_groups(labels.len());
            &owned_groups
        }
    };
    let mut splits = Vec::with_capacity(DEFAULT_ASSAY_SEEDS.len());
    for seed in DEFAULT_ASSAY_SEEDS {
        splits.push(group_holdout_split(
            labels,
            groups,
            DEFAULT_HOLDOUT_FRACTION,
            seed,
        )?);
    }
    let conditioning = fit_conditioning(blocks, &splits)?;
    let flat = flatten_logistic_blocks(blocks, rows, dim)?;
    let cuda_labels = labels
        .iter()
        .map(|label| i32::from(*label))
        .collect::<Vec<_>>();
    let (train_offsets, train_indices, test_offsets, test_indices) =
        split_buffers_for_cuda(&splits, labels.len())?;
    let summaries = logistic_summaries_cuda_strict_impl(LogisticCudaInputs {
        samples: &flat,
        labels: &cuda_labels,
        rows,
        dim,
        blocks,
        block_scales: &conditioning.block_scales,
        train_offsets: &train_offsets,
        train_indices: &train_indices,
        test_offsets: &test_offsets,
        test_indices: &test_indices,
    })?;
    if summaries.bits.len() != DEFAULT_ASSAY_SEEDS.len()
        || summaries.accuracy.len() != DEFAULT_ASSAY_SEEDS.len()
        || summaries.iterations.len() != DEFAULT_ASSAY_SEEDS.len()
        || summaries.final_relative_parameter_changes.len() != DEFAULT_ASSAY_SEEDS.len()
        || summaries.converged.len() != DEFAULT_ASSAY_SEEDS.len()
    {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "logistic CUDA returned bits={} accuracies={} iterations={} final_relative_changes={} convergence_flags={} for {} seeds",
            summaries.bits.len(),
            summaries.accuracy.len(),
            summaries.iterations.len(),
            summaries.final_relative_parameter_changes.len(),
            summaries.converged.len(),
            DEFAULT_ASSAY_SEEDS.len()
        )));
    }
    let seed_summaries = summaries
        .bits
        .iter()
        .zip(summaries.accuracy.iter())
        .zip(summaries.iterations.iter())
        .zip(summaries.final_relative_parameter_changes.iter())
        .zip(summaries.converged.iter())
        .map(
            |(
                (((&bits, &accuracy), &solver_iterations), &solver_final_relative_parameter_change),
                &solver_converged,
            )| {
                LogisticSummary {
                    bits,
                    accuracy,
                    solver_iterations,
                    solver_final_relative_parameter_change,
                    solver_converged,
                }
            },
        )
        .collect::<Vec<_>>();
    report_from_seed_summaries(seed_summaries, labels.len(), trust, conditioning.provenance)
}

fn report_from_seed_summaries(
    seed_summaries: Vec<LogisticSummary>,
    n_samples: usize,
    trust: TrustTag,
    mut conditioning: LogisticConditioningProvenance,
) -> Result<LogisticProbeReport> {
    if seed_summaries.len() != conditioning.folds.len() {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "logistic solver summaries {} != conditioning folds {}",
            seed_summaries.len(),
            conditioning.folds.len()
        )));
    }
    for (fit, (summary, fold)) in seed_summaries
        .iter()
        .zip(&mut conditioning.folds)
        .enumerate()
    {
        if !summary.solver_converged
            || summary.solver_iterations == 0
            || summary.solver_iterations > super::LOGISTIC_MAX_ITERATIONS
            || summary.solver_iterations % super::LOGISTIC_CONVERGENCE_CHECK_INTERVAL != 0
            || !summary.solver_final_relative_parameter_change.is_finite()
            || summary.solver_final_relative_parameter_change < 0.0
            || summary.solver_final_relative_parameter_change
                > super::LOGISTIC_RELATIVE_PARAMETER_TOLERANCE
        {
            return Err(crate::calibration::underpowered(format!(
                "logistic fit {fit} seed {} lacks valid convergence evidence: converged={} iterations={} final_relative_parameter_change={:.9} tolerance={:.9} max_iterations={}",
                fold.seed,
                summary.solver_converged,
                summary.solver_iterations,
                summary.solver_final_relative_parameter_change,
                super::LOGISTIC_RELATIVE_PARAMETER_TOLERANCE,
                super::LOGISTIC_MAX_ITERATIONS
            )));
        }
        fold.solver_iterations = summary.solver_iterations;
        fold.solver_final_relative_parameter_change_ppm_upper_bound =
            (f64::from(summary.solver_final_relative_parameter_change)
                * f64::from(super::LOGISTIC_PROVENANCE_PPM_SCALE))
            .ceil() as u32;
        fold.solver_converged = true;
    }
    let seed_bits = seed_summaries
        .iter()
        .map(|summary| summary.bits)
        .collect::<Vec<_>>();
    let bits = mean(&seed_bits);
    let seed_sigma = sample_sigma(&seed_bits);
    let (ci_low, ci_high) = seed_ci(bits, seed_sigma, seed_bits.len());
    let reliability =
        EstimateReliability::new(seed_bits.len(), seed_sigma, seed_sigma >= bits.abs())?;
    Ok(LogisticProbeReport {
        estimate: MiEstimate::new(
            bits,
            ci_low,
            ci_high,
            n_samples,
            EstimatorKind::LogisticProbe,
            trust,
        )
        .with_reliability(reliability),
        accuracy: mean(
            &seed_summaries
                .iter()
                .map(|summary| summary.accuracy)
                .collect::<Vec<_>>(),
        ),
        selected_field: "logistic_probe_multiseed_group_holdout_train_fold_lens_block_conditioned",
        conditioning,
    })
}

pub(super) fn logistic_probe_mi_multiseed_calibrated_with_trust(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<LogisticProbeReport> {
    let blocks = [LogisticBlock::unscoped(samples)];
    logistic_probe_mi_multiseed_calibrated_blocks_with_trust(&blocks, labels, groups, trust)
}

pub(crate) fn logistic_probe_mi_multiseed_calibrated_blocks(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_calibrated_blocks_with_trust(
        blocks,
        labels,
        groups,
        TrustTag::Provisional,
    )
}

fn logistic_probe_mi_multiseed_calibrated_blocks_with_trust(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<LogisticProbeReport> {
    if strict_cuda_requested() {
        return logistic_probe_mi_multiseed_calibrated_blocks_with_trust_cuda_strict(
            blocks, labels, groups, trust,
        );
    }
    ensure_informative_binary_labels(labels)?;
    let calibration =
        calibration::logistic_power_calibration_blocks(blocks, labels, groups, trust)?;
    let mut report = logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples(
        blocks,
        labels,
        groups,
        trust,
        MIN_ASSAY_SAMPLES,
    )?;
    report.estimate = report.estimate.with_power_calibration(calibration);
    Ok(report)
}

pub(super) fn logistic_probe_mi_multiseed_calibrated_with_trust_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<LogisticProbeReport> {
    let blocks = [LogisticBlock::unscoped(samples)];
    logistic_probe_mi_multiseed_calibrated_blocks_with_trust_cuda_strict(
        &blocks, labels, groups, trust,
    )
}

fn logistic_probe_mi_multiseed_calibrated_blocks_with_trust_cuda_strict(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<LogisticProbeReport> {
    ensure_informative_binary_labels(labels)?;
    let calibration =
        calibration::logistic_power_calibration_blocks_cuda_strict(blocks, labels, groups, trust)?;
    let mut report = logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples_cuda_strict(
        blocks,
        labels,
        groups,
        trust,
        MIN_ASSAY_SAMPLES,
    )?;
    report.estimate = report.estimate.with_power_calibration(calibration);
    Ok(report)
}
