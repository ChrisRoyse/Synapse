//! Shared Assay estimate types.

use calyx_core::{Anchor, CalyxError, Result};
use serde::{Deserialize, Serialize};

use crate::calibration::PowerCalibration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTag {
    Trusted,
    Provisional,
}

/// Source prefix for a resolved UMA outcome anchor.
pub const RESOLVED_UMA_ANCHOR_SOURCE_PREFIX: &str = "uma:";
/// Source prefix for a live-market proxy anchor.
pub const PROXY_ANCHOR_SOURCE_PREFIX: &str = "proxy:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimatorKind {
    Ksg,
    /// Contingency-table plug-in entropy decomposition with a Miller-Madow
    /// first-order bias correction — the exact instrument for a column that is
    /// categorical by construction, where KSG's k-th radius is zero
    /// ([`crate::mi_estimator`]).
    DiscretePlugin,
    HistogramNmi,
    LogisticProbe,
    Bootstrap,
    PanelSufficiency,
    OutcomeEntropy,
    PairGain,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateBound {
    #[default]
    LowerBound,
    Point,
    UpperBound,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EstimateReliability {
    pub seed_count: usize,
    pub seed_sigma: f32,
    pub unresolved: bool,
}

impl EstimateReliability {
    pub fn new(seed_count: usize, seed_sigma: f32, unresolved: bool) -> Result<Self> {
        if seed_count == 0 {
            return Err(CalyxError::assay_insufficient_samples(
                "assay reliability requires at least one seed",
            ));
        }
        if !seed_sigma.is_finite() || seed_sigma < 0.0 {
            return Err(CalyxError::assay_low_signal(
                "assay seed_sigma must be finite and non-negative",
            ));
        }
        Ok(Self {
            seed_count,
            seed_sigma,
            unresolved,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MiEstimate {
    pub bits: f32,
    pub ci_low: f32,
    pub ci_high: f32,
    pub n_samples: usize,
    pub estimator: EstimatorKind,
    pub trust: TrustTag,
    #[serde(default)]
    pub bound: EstimateBound,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_calibration: Option<PowerCalibration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reliability: Option<EstimateReliability>,
}

impl MiEstimate {
    pub fn new(
        bits: f32,
        ci_low: f32,
        ci_high: f32,
        n_samples: usize,
        estimator: EstimatorKind,
        trust: TrustTag,
    ) -> Self {
        let bits = bits.max(0.0);
        let ci_low = ci_low.min(bits).max(0.0);
        let ci_high = ci_high.max(bits);
        Self {
            bits,
            ci_low,
            ci_high,
            n_samples,
            estimator,
            trust,
            bound: EstimateBound::LowerBound,
            power_calibration: None,
            reliability: None,
        }
    }

    pub fn point(bits: f32, n_samples: usize, estimator: EstimatorKind, trust: TrustTag) -> Self {
        let band = (bits.abs() * 0.15).max(0.02);
        Self::new(bits, bits - band, bits + band, n_samples, estimator, trust)
            .with_bound(EstimateBound::Point)
    }

    pub fn with_reliability(mut self, reliability: EstimateReliability) -> Self {
        self.reliability = Some(reliability);
        self
    }

    pub fn with_power_calibration(mut self, calibration: PowerCalibration) -> Self {
        self.power_calibration = Some(calibration);
        self
    }

    pub fn with_bound(mut self, bound: EstimateBound) -> Self {
        self.bound = bound;
        self
    }
}

pub fn trust_for_anchor(anchor: Option<&Anchor>) -> TrustTag {
    if anchor.is_some_and(is_grounded_anchor) {
        TrustTag::Trusted
    } else {
        TrustTag::Provisional
    }
}

pub fn provisional_without_anchor(_requested: TrustTag) -> TrustTag {
    TrustTag::Provisional
}

pub fn require_grounded_anchor(anchor: &Anchor) -> Result<TrustTag> {
    if is_grounded_anchor(anchor) {
        Ok(TrustTag::Trusted)
    } else {
        Err(CalyxError::assay_insufficient_samples(
            "trusted assay estimates require grounded anchor evidence",
        ))
    }
}

fn is_grounded_anchor(anchor: &Anchor) -> bool {
    let source = anchor.source.trim();
    if source.is_empty()
        || !anchor.confidence.is_finite()
        || anchor.confidence <= 0.0
        || anchor.confidence > 1.0
    {
        return false;
    }

    // Until Anchor carries a first-class origin field, market anchors stamp origin in source.
    if source.starts_with(PROXY_ANCHOR_SOURCE_PREFIX) {
        return false;
    }

    if matches!(source, "uma" | "uma-onchain")
        || source.starts_with(RESOLVED_UMA_ANCHOR_SOURCE_PREFIX)
    {
        return anchor.confidence == 1.0;
    }

    false
}
