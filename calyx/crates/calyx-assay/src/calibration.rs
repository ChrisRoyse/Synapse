//! Power calibration for Assay estimators.

use calyx_core::{CalyxError, Result};
use serde::{Deserialize, Serialize};

pub const CALYX_ASSAY_ESTIMATOR_UNDERPOWERED: &str = "CALYX_ASSAY_ESTIMATOR_UNDERPOWERED";
pub const CALYX_ASSAY_DEGENERATE_TARGET_ENTROPY: &str = "CALYX_ASSAY_DEGENERATE_TARGET_ENTROPY";
pub const MIN_INFORMATIVE_TARGET_ENTROPY_BITS: f32 = 0.30;
pub const DEFAULT_MIN_POWER_RECOVERY_RATIO: f32 = 0.50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerCalibrationStatus {
    Passed,
    Underpowered,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PowerCalibration {
    pub status: PowerCalibrationStatus,
    pub planted_bits: f32,
    pub recovered_bits: f32,
    pub recovery_ratio: f32,
    pub min_recovery_ratio: f32,
    pub n_samples: usize,
    pub n_features: usize,
    pub planted_column: usize,
}

impl PowerCalibration {
    pub fn new(
        planted_bits: f32,
        recovered_bits: f32,
        min_recovery_ratio: f32,
        n_samples: usize,
        n_features: usize,
        planted_column: usize,
    ) -> Result<Self> {
        if !planted_bits.is_finite() || planted_bits <= 0.0 {
            return Err(degenerate_entropy(format!(
                "planted entropy must be positive and finite; got {planted_bits}"
            )));
        }
        if !recovered_bits.is_finite() || recovered_bits < 0.0 {
            return Err(underpowered(format!(
                "planted-signal recovery must be finite and non-negative; got {recovered_bits}"
            )));
        }
        if !min_recovery_ratio.is_finite() || !(0.0..=1.0).contains(&min_recovery_ratio) {
            return Err(underpowered(format!(
                "min recovery ratio must be in [0,1]; got {min_recovery_ratio}"
            )));
        }
        let recovery_ratio = (recovered_bits / planted_bits).min(1.0);
        let status = if recovery_ratio >= min_recovery_ratio {
            PowerCalibrationStatus::Passed
        } else {
            PowerCalibrationStatus::Underpowered
        };
        Ok(Self {
            status,
            planted_bits,
            recovered_bits,
            recovery_ratio,
            min_recovery_ratio,
            n_samples,
            n_features,
            planted_column,
        })
    }

    pub fn require_passed(&self) -> Result<()> {
        if self.status == PowerCalibrationStatus::Passed {
            return Ok(());
        }
        Err(underpowered(format!(
            "planted signal recovered {:.6}/{:.6} bits ({:.3}) below required ratio {:.3}; samples={} calibrated_features={} diagnostic_column={}",
            self.recovered_bits,
            self.planted_bits,
            self.recovery_ratio,
            self.min_recovery_ratio,
            self.n_samples,
            self.n_features,
            self.planted_column
        )))
    }
}

pub fn ensure_informative_binary_labels(labels: &[bool]) -> Result<f32> {
    if labels.is_empty() {
        return Err(degenerate_entropy(
            "informative target requires at least one label",
        ));
    }
    let positives = labels.iter().filter(|&&label| label).count();
    let negatives = labels.len() - positives;
    let entropy = binary_entropy_bits(positives, negatives);
    if entropy < MIN_INFORMATIVE_TARGET_ENTROPY_BITS {
        return Err(degenerate_entropy(format!(
            "target entropy {:.6} bits below minimum {:.6}; positives={} negatives={}",
            entropy, MIN_INFORMATIVE_TARGET_ENTROPY_BITS, positives, negatives
        )));
    }
    Ok(entropy)
}

fn binary_entropy_bits(positives: usize, negatives: usize) -> f32 {
    let total = (positives + negatives).max(1) as f32;
    [positives, negatives]
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let p = count as f32 / total;
            -p * p.log2()
        })
        .sum()
}

pub fn underpowered(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ASSAY_ESTIMATOR_UNDERPOWERED,
        message: message.into(),
        remediation: "increase samples, lower dimensionality, or replace the estimator before issuing assay verdicts",
    }
}

pub fn degenerate_entropy(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ASSAY_DEGENERATE_TARGET_ENTROPY,
        message: message.into(),
        remediation: "use a target with enough outcome entropy to support an MI verdict",
    }
}
