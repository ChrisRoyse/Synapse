//! Binary outcome logistic-probe MI estimator.

mod calibration;
mod conditioning;
mod cuda;
mod pipeline;
mod train;

use calyx_core::{Anchor, CalyxError, Result};
use serde::{Deserialize, Serialize};

use crate::calibration::{
    DEFAULT_MIN_POWER_RECOVERY_RATIO, PowerCalibration, ensure_informative_binary_labels,
};
#[cfg(not(feature = "cuda"))]
use crate::cuda_strict::cuda_unavailable;
use crate::estimate::{MiEstimate, TrustTag, trust_for_anchor};
use crate::ksg::MIN_ASSAY_SAMPLES;

use self::calibration::{logistic_power_calibration, logistic_power_calibration_cuda_strict};
pub(crate) use self::conditioning::LogisticBlock;
pub(crate) use self::conditioning::validate_conditioning_provenance;
pub use self::conditioning::{
    LOGISTIC_CONDITIONING_METHOD, LOGISTIC_CONDITIONING_SCHEMA_VERSION, LogisticConditioningBlock,
    LogisticConditioningProvenance, LogisticConditioningScale, LogisticFoldConditioning,
};
pub(crate) use self::pipeline::{
    logistic_probe_mi_multiseed_blocks, logistic_probe_mi_multiseed_calibrated_blocks,
};
use self::pipeline::{
    logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples,
    logistic_probe_mi_multiseed_blocks_with_trust_and_min_samples_cuda_strict,
    logistic_probe_mi_multiseed_calibrated_with_trust,
    logistic_probe_mi_multiseed_calibrated_with_trust_cuda_strict,
    logistic_probe_mi_multiseed_with_trust_and_min_samples,
    logistic_probe_mi_multiseed_with_trust_and_min_samples_cuda_strict,
};

pub const DEFAULT_ASSAY_SEEDS: [u64; 5] = [20_260_612, 7, 101, 2_024, 99_999];
pub const DEFAULT_HOLDOUT_FRACTION: f32 = 0.2;
const LOGISTIC_MAX_ITERATIONS: usize = 2_048;
const LOGISTIC_CONVERGENCE_CHECK_INTERVAL: usize = 16;
const LOGISTIC_PROVENANCE_PPM_SCALE: u32 = 1_000_000;
const LOGISTIC_RELATIVE_PARAMETER_TOLERANCE_PPM: u32 = 1_000;
const LOGISTIC_RELATIVE_PARAMETER_EVIDENCE_MAX_PPM: u32 =
    LOGISTIC_RELATIVE_PARAMETER_TOLERANCE_PPM + 1;
const LOGISTIC_RELATIVE_PARAMETER_TOLERANCE: f32 =
    LOGISTIC_RELATIVE_PARAMETER_TOLERANCE_PPM as f32 / LOGISTIC_PROVENANCE_PPM_SCALE as f32;
const LOGISTIC_LEARNING_RATE_CAP: f32 = 0.35;
const LOGISTIC_LIPSCHITZ_SAFETY: f32 = 0.95;
const LOGISTIC_L2: f32 = 1.0e-4;

fn logistic_learning_rate(block_count: usize) -> f32 {
    let smoothness_bound = 0.25 * block_count.max(1) as f32 + LOGISTIC_L2;
    LOGISTIC_LEARNING_RATE_CAP.min(LOGISTIC_LIPSCHITZ_SAFETY / smoothness_bound)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogisticProbeReport {
    pub estimate: MiEstimate,
    pub accuracy: f32,
    pub selected_field: &'static str,
    pub conditioning: LogisticConditioningProvenance,
}

pub fn logistic_probe_mi(samples: &[Vec<f32>], labels: &[bool]) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust(samples, labels, TrustTag::Provisional)
}

pub fn logistic_probe_mi_calibrated(
    samples: &[Vec<f32>],
    labels: &[bool],
) -> Result<LogisticProbeReport> {
    ensure_informative_binary_labels(labels)?;
    let calibration = logistic_power_calibration(samples, labels, None, TrustTag::Provisional)?;
    let mut report = logistic_probe_mi_with_trust(samples, labels, TrustTag::Provisional)?;
    report.estimate = report.estimate.with_power_calibration(calibration);
    Ok(report)
}

pub fn logistic_probe_mi_with_anchor(
    samples: &[Vec<f32>],
    labels: &[bool],
    anchor: &Anchor,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust(samples, labels, trust_for_anchor(Some(anchor)))
}

pub fn logistic_probe_mi_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust_and_min_samples_cuda_strict(
        samples,
        labels,
        TrustTag::Provisional,
        MIN_ASSAY_SAMPLES,
    )
}

pub fn logistic_probe_mi_with_anchor_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    anchor: &Anchor,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust_and_min_samples_cuda_strict(
        samples,
        labels,
        trust_for_anchor(Some(anchor)),
        MIN_ASSAY_SAMPLES,
    )
}

pub fn logistic_probe_mi_calibrated_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
) -> Result<LogisticProbeReport> {
    ensure_informative_binary_labels(labels)?;
    let calibration =
        logistic_power_calibration_cuda_strict(samples, labels, None, TrustTag::Provisional)?;
    let mut report = logistic_probe_mi_cuda_strict(samples, labels)?;
    report.estimate = report.estimate.with_power_calibration(calibration);
    Ok(report)
}

pub fn logistic_probe_mi_multiseed(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust(samples, labels, groups, TrustTag::Provisional)
}

pub fn logistic_probe_mi_multiseed_calibrated(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_calibrated_with_trust(
        samples,
        labels,
        groups,
        TrustTag::Provisional,
    )
}

pub fn logistic_probe_mi_multiseed_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust_and_min_samples_cuda_strict(
        samples,
        labels,
        groups,
        TrustTag::Provisional,
        MIN_ASSAY_SAMPLES,
    )
}

pub fn logistic_probe_mi_multiseed_calibrated_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_calibrated_with_trust_cuda_strict(
        samples,
        labels,
        groups,
        TrustTag::Provisional,
    )
}

pub fn logistic_probe_mi_multiseed_with_anchor(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    anchor: &Anchor,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust(samples, labels, groups, trust_for_anchor(Some(anchor)))
}

pub fn logistic_probe_mi_multiseed_calibrated_with_anchor(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    anchor: &Anchor,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_calibrated_with_trust(
        samples,
        labels,
        groups,
        trust_for_anchor(Some(anchor)),
    )
}

pub fn logistic_probe_mi_multiseed_with_anchor_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    anchor: &Anchor,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust_and_min_samples_cuda_strict(
        samples,
        labels,
        groups,
        trust_for_anchor(Some(anchor)),
        MIN_ASSAY_SAMPLES,
    )
}

pub fn logistic_probe_mi_multiseed_calibrated_with_anchor_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    anchor: &Anchor,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_calibrated_with_trust_cuda_strict(
        samples,
        labels,
        groups,
        trust_for_anchor(Some(anchor)),
    )
}

pub(crate) fn logistic_probe_mi_with_min_samples(
    samples: &[Vec<f32>],
    labels: &[bool],
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust_and_min_samples(
        samples,
        labels,
        TrustTag::Provisional,
        min_samples,
    )
}

pub(crate) fn logistic_probe_mi_with_anchor_and_min_samples(
    samples: &[Vec<f32>],
    labels: &[bool],
    anchor: &Anchor,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust_and_min_samples(
        samples,
        labels,
        trust_for_anchor(Some(anchor)),
        min_samples,
    )
}

fn logistic_probe_mi_with_trust(
    samples: &[Vec<f32>],
    labels: &[bool],
    trust: TrustTag,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_with_trust_and_min_samples(samples, labels, trust, MIN_ASSAY_SAMPLES)
}

fn logistic_probe_mi_multiseed_with_trust(
    samples: &[Vec<f32>],
    labels: &[bool],
    groups: Option<&[String]>,
    trust: TrustTag,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust_and_min_samples(
        samples,
        labels,
        groups,
        trust,
        MIN_ASSAY_SAMPLES,
    )
}

fn logistic_probe_mi_with_trust_and_min_samples(
    samples: &[Vec<f32>],
    labels: &[bool],
    trust: TrustTag,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust_and_min_samples(
        samples,
        labels,
        None,
        trust,
        min_samples,
    )
}

fn logistic_probe_mi_with_trust_and_min_samples_cuda_strict(
    samples: &[Vec<f32>],
    labels: &[bool],
    trust: TrustTag,
    min_samples: usize,
) -> Result<LogisticProbeReport> {
    logistic_probe_mi_multiseed_with_trust_and_min_samples_cuda_strict(
        samples,
        labels,
        None,
        trust,
        min_samples,
    )
}
