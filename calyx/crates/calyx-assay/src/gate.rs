//! AssayGate facade for lens signal and pair gain.

use calyx_core::{Anchor, Result};
use serde::{Deserialize, Serialize};

use crate::estimate::{EstimatorKind, MiEstimate, TrustTag};
use crate::logistic::{
    logistic_probe_mi_with_anchor_and_min_samples, logistic_probe_mi_with_min_samples,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LensSignal {
    pub estimate: MiEstimate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairGain {
    pub left_bits: f32,
    pub right_bits: f32,
    pub pair_bits: f32,
    pub gain_bits: f32,
    pub ci_low: f32,
    pub ci_high: f32,
    pub n_samples: usize,
    /// `gain_bits` before the data-processing-inequality floor.
    ///
    /// A negative raw gain is an instrument fault, not a corpus fact: `[a‖b]`
    /// determines `a`, so the pair cannot carry less about the outcome than
    /// either half. Carrying it means a clamped row is visibly clamped rather
    /// than indistinguishable from a genuine zero.
    #[serde(default)]
    pub raw_gain_bits: f32,
    /// Whether the floor moved [`Self::gain_bits`] away from
    /// [`Self::raw_gain_bits`].
    #[serde(default)]
    pub monotonicity_floor_applied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssayGate {
    pub min_samples: usize,
}

impl Default for AssayGate {
    fn default() -> Self {
        Self { min_samples: 50 }
    }
}

impl AssayGate {
    pub fn lens_signal(&self, samples: &[Vec<f32>], labels: &[bool]) -> Result<LensSignal> {
        let report = logistic_probe_mi_with_min_samples(samples, labels, self.min_samples)?;
        Ok(LensSignal {
            estimate: report.estimate,
        })
    }

    pub fn lens_signal_with_anchor(
        &self,
        samples: &[Vec<f32>],
        labels: &[bool],
        anchor: &Anchor,
    ) -> Result<LensSignal> {
        let report = logistic_probe_mi_with_anchor_and_min_samples(
            samples,
            labels,
            anchor,
            self.min_samples,
        )?;
        Ok(LensSignal {
            estimate: report.estimate,
        })
    }

    pub fn pair_gain(
        &self,
        left: &[Vec<f32>],
        right: &[Vec<f32>],
        labels: &[bool],
    ) -> Result<PairGain> {
        let left_signal = self.lens_signal(left, labels)?.estimate;
        let right_signal = self.lens_signal(right, labels)?.estimate;
        let combined: Vec<Vec<f32>> = left
            .iter()
            .zip(right)
            .map(|(a, b)| a.iter().chain(b).copied().collect())
            .collect();
        let pair_signal = self.lens_signal(&combined, labels)?.estimate;
        pair_gain_from_estimates(&left_signal, &right_signal, &pair_signal)
    }

    pub fn pair_gain_with_anchor(
        &self,
        left: &[Vec<f32>],
        right: &[Vec<f32>],
        labels: &[bool],
        anchor: &Anchor,
    ) -> Result<PairGain> {
        let left_signal = self.lens_signal_with_anchor(left, labels, anchor)?.estimate;
        let right_signal = self
            .lens_signal_with_anchor(right, labels, anchor)?
            .estimate;
        let combined: Vec<Vec<f32>> = left
            .iter()
            .zip(right)
            .map(|(a, b)| a.iter().chain(b).copied().collect())
            .collect();
        let pair_signal = self
            .lens_signal_with_anchor(&combined, labels, anchor)?
            .estimate;
        pair_gain_from_estimates(&left_signal, &right_signal, &pair_signal)
    }

    pub fn pair_gain_estimate(&self, gain: &PairGain) -> MiEstimate {
        self.pair_gain_estimate_with_trust(gain, TrustTag::Provisional)
    }

    pub fn pair_gain_estimate_with_anchor(&self, gain: &PairGain, anchor: &Anchor) -> MiEstimate {
        self.pair_gain_estimate_with_trust(gain, crate::estimate::trust_for_anchor(Some(anchor)))
    }

    fn pair_gain_estimate_with_trust(&self, gain: &PairGain, trust: TrustTag) -> MiEstimate {
        MiEstimate::new(
            gain.gain_bits,
            gain.ci_low,
            gain.ci_high,
            gain.n_samples,
            EstimatorKind::PairGain,
            trust,
        )
    }
}

/// Builds a [`PairGain`] from three measured estimates.
///
/// The `pair - max(left, right)` arithmetic and its data-processing-inequality
/// floor come from [`crate::synergy::whole_minus_max_gain`], which #1942 ask 4
/// established as the **single** implementation of that quantity. This function
/// previously computed it inline as
/// `(pair.bits - left.bits.max(right.bits)).max(0.0)`, which was the same
/// defect #1942 was filed about, on a different path, with two consequences the
/// shared function does not have:
///
/// * **A non-finite term was silently absorbed.** Rust's `f32::max` ignores
///   `NaN`, so `NaN.max(0.0)` is `0.0`: a broken measurement reported "no
///   synergy between these lenses" instead of failing. The shared function
///   rejects non-finite and negative terms with `CALYX_ASSAY_INVALID_SYNERGY`.
/// * **The floor was invisible.** A raw gain below zero is a data-processing-
///   inequality violation and therefore an instrument fault, not a corpus
///   fact. Clamping it without saying so makes the fault unobservable. The
///   shared function reports whether the floor moved the value, and that now
///   travels on [`PairGain::raw_gain_bits`] /
///   [`PairGain::monotonicity_floor_applied`] — carried on the value rather
///   than logged, because `calyx-assay` has no `tracing` dependency and a
///   clamp a caller can inspect beats one it has to find in a log.
///
/// # Errors
///
/// Returns [`crate::synergy::CALYX_ASSAY_INVALID_SYNERGY`] when any bit term is
/// non-finite or negative.
pub(crate) fn pair_gain_from_estimates(
    left: &MiEstimate,
    right: &MiEstimate,
    pair: &MiEstimate,
) -> Result<PairGain> {
    let (gain_bits, raw_gain_bits, monotonicity_floor_applied) =
        crate::synergy::whole_minus_max_gain(pair.bits, left.bits, right.bits)?;
    let baseline_low = left.ci_low.max(right.ci_low);
    let baseline_high = left.ci_high.max(right.ci_high);
    let ci_low = (pair.ci_low - baseline_high).max(0.0);
    let ci_high = (pair.ci_high - baseline_low).max(gain_bits);
    Ok(PairGain {
        left_bits: left.bits,
        right_bits: right.bits,
        pair_bits: pair.bits,
        gain_bits,
        ci_low,
        ci_high,
        n_samples: pair.n_samples,
        raw_gain_bits,
        monotonicity_floor_applied,
    })
}
