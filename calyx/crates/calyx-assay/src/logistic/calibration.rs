use super::conditioning::validate_blocks;
use super::*;

const PLANTED_SIGNAL_BLOCK_NAME: &str = "__calyx_assay_planted_binary_v1";

pub(super) fn logistic_power_calibration(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<PowerCalibration> {
    let blocks = [LogisticBlock::unscoped(samples)];
    logistic_power_calibration_blocks(&blocks, labels, groups, trust)
}

pub(super) fn logistic_power_calibration_blocks(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<PowerCalibration> {
    logistic_power_calibration_blocks_impl(blocks, labels, groups, trust, false)
}

pub(super) fn logistic_power_calibration_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<PowerCalibration> {
    let blocks = [LogisticBlock::unscoped(samples)];
    logistic_power_calibration_blocks_cuda_strict(&blocks, labels, groups, trust)
}

pub(super) fn logistic_power_calibration_blocks_cuda_strict(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<PowerCalibration> {
    logistic_power_calibration_blocks_impl(blocks, labels, groups, trust, true)
}

fn logistic_power_calibration_blocks_impl(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
    cuda_strict: bool,
) -> Result<PowerCalibration> {
    let planted_bits = ensure_informative_binary_labels(labels)?;
    let (_, total_dim) = validate_blocks("logistic power calibration", blocks)?;
    let planted_column = total_dim;
    let calibrated_dim = total_dim.checked_add(1).ok_or_else(|| {
        crate::calibration::underpowered(
            "power calibration feature count overflow while adding the diagnostic signal block",
        )
    })?;
    let planted_vectors = plant_binary_signal(labels);
    let mut planted_blocks = blocks.to_vec();
    planted_blocks.push(LogisticBlock {
        name: PLANTED_SIGNAL_BLOCK_NAME,
        slot: None,
        vectors: &planted_vectors,
    });
    let report = if cuda_strict {
        logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples_cuda_strict(
            &planted_blocks,
            labels,
            groups,
            trust,
            MIN_ASSAY_SAMPLES,
        )?
    } else {
        logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples(
            &planted_blocks,
            labels,
            groups,
            trust,
            MIN_ASSAY_SAMPLES,
        )?
    };
    let calibration = PowerCalibration::new(
        planted_bits,
        report.estimate.bits,
        DEFAULT_MIN_POWER_RECOVERY_RATIO,
        labels.len(),
        calibrated_dim,
        planted_column,
    )?;
    calibration.require_passed()?;
    Ok(calibration)
}

fn plant_binary_signal(labels: &[bool]) -> Vec<Vec<f32>> {
    labels
        .iter()
        .map(|label| vec![if *label { 1.0 } else { -1.0 }])
        .collect()
}
