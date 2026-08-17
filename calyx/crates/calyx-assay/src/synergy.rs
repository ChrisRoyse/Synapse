//! Pairwise lens synergy: the bits a lens *pair* carries about a grounded
//! outcome anchor beyond what the better of the two lenses carries alone.
//!
//! The measure is the `WholeMinusMax` synergy of Griffith & Koch
//! (*Quantifying synergistic mutual information*, arXiv:1205.4265):
//!
//! ```text
//! gain(a,b ; Y) = I(a,b ; Y) - max( I(a;Y), I(b;Y) )
//! ```
//!
//! It is the honest, estimator-agnostic form of the pair-gain quantity Loom's
//! materialization gate consumes: a cross-term over `(a,b)` is only worth
//! storing when the pair says something neither lens says on its own.
//!
//! # Two conditions make that difference a measurement (#1941)
//!
//! **Same samples.** All three terms must be measured over the same paired
//! sample set, or the difference is not a synergy at all.
//!
//! **Same instrument.** All three terms must come from the *same* estimator.
//! A difference of information quantities is only interpretable when the
//! estimators' biases cancel, and that cancellation is a property of one
//! estimator applied at one scale — never of two. Kraskov, Stögbauer &
//! Grassberger (*Estimating mutual information*, Phys. Rev. E 69 066138, 2004)
//! make the point in the construction of KSG itself: estimating the joint and
//! the marginal entropies at independently chosen scales means "the biases in
//! Ĥ(X), Ĥ(Y), and in Ĥ(X,Y) would be very different and would thus not
//! cancel". KSG exists to force that cancellation. Subtracting a
//! Miller-Madow-corrected contingency-table estimate from a KSG estimate
//! reintroduces exactly the non-cancelling bias KSG was built to remove: both
//! numbers are valid estimates of their own quantity, and their difference is
//! not an estimate of anything.
//!
//! So a heterogeneous pair is **refused**, not reported — see
//! [`SynergyPairState::CrossEstimatorUnpinnable`]. The refusal is not
//! conditioned on the sign of the raw difference: a cross-instrument *positive*
//! gain is exactly as unfounded as a negative one, and only the negative one
//! happens to be visibly impossible.
//!
//! # The monotonicity floor
//!
//! Even within one instrument the raw difference can land below zero. It cannot
//! be true: `[a‖b]` determines `a`, so the data-processing inequality gives
//! `I([a‖b];Y) >= max(I(a;Y), I(b;Y))` for the true quantities, and a negative
//! measurement is a finite-sample artefact of the instrument. The gain is
//! therefore floored at zero and the row says so — [`SynergyPair::raw_gain_bits`]
//! keeps the unclamped difference and
//! [`SynergyPair::monotonicity_floor_applied`] marks the row — the same
//! treatment `panel_floor_applied` already gets on the sufficiency path. A
//! clamped row is visibly clamped, never silently rounded.

use calyx_core::{CalyxError, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::mi_estimator::MiEstimator;

/// Gain floor (bits) at which a pair counts as carrying genuine synergy. Same
/// floor as the lens admission contract's signal floor (handbook section 13).
pub const MIN_SYNERGY_GAIN_BITS: f32 = crate::contract::MIN_SIGNAL_BITS;

/// Error code raised when a synergy term is not a usable bit measurement.
pub const CALYX_ASSAY_INVALID_SYNERGY: &str = "CALYX_ASSAY_INVALID_SYNERGY";

/// Error code raised when the three terms of a pair did not come from one
/// estimator, so their difference is not a measurement.
pub const CALYX_ASSAY_SYNERGY_CROSS_ESTIMATOR: &str = "CALYX_ASSAY_SYNERGY_CROSS_ESTIMATOR";

/// Why a pair row does or does not carry a measured gain.
///
/// Every non-`Measured` state is reported *per pair with a named reason*, never
/// by dropping the pair from the report — the same discipline #1915 established
/// for per-slot estimator refusals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynergyPairState {
    /// All three terms measured by one estimator over one paired sample set.
    #[default]
    Measured,
    /// Too few paired samples to measure the pair at all.
    InsufficientSamples,
    /// The estimator refused at least one of the three columns.
    EstimatorRefused,
    /// The three columns could not all be measured by a single instrument, so
    /// their difference has no estimator whose bias would cancel.
    CrossEstimatorUnpinnable,
}

impl SynergyPairState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::InsufficientSamples => "insufficient_samples",
            Self::EstimatorRefused => "estimator_refused",
            Self::CrossEstimatorUnpinnable => "cross_estimator_unpinnable",
        }
    }
}

/// The instrument each of a pair's three terms was measured with.
///
/// Carried explicitly rather than collapsed to a single "homogeneous" flag so a
/// consumer can *see* that the three agree instead of taking the producer's word
/// for it (#1941 ask 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynergyEstimators {
    pub pair: MiEstimator,
    pub left: MiEstimator,
    pub right: MiEstimator,
}

impl SynergyEstimators {
    /// All three terms from one instrument.
    #[must_use]
    pub const fn homogeneous(estimator: MiEstimator) -> Self {
        Self {
            pair: estimator,
            left: estimator,
            right: estimator,
        }
    }

    /// True when all three terms came from the same instrument, which is the
    /// precondition for their difference to be a measurement.
    #[must_use]
    pub fn is_homogeneous(&self) -> bool {
        self.pair == self.left && self.left == self.right
    }
}

/// One measured lens pair: the joint bits, both marginals, and the resulting
/// `WholeMinusMax` gain, all over the *same* paired sample set and from the
/// *same* estimator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynergyPair {
    pub a: SlotId,
    pub b: SlotId,
    /// `I(a,b ; Y)` over the records where both lenses and the anchor are present.
    pub pair_bits: f32,
    /// `I(a ; Y)` over that same record set.
    pub left_bits: f32,
    /// `I(b ; Y)` over that same record set.
    pub right_bits: f32,
    /// `max(0, raw_gain_bits)` — the reported gain, floored by the
    /// data-processing inequality.
    pub gain_bits: f32,
    /// `pair_bits - max(left_bits, right_bits)` exactly as measured, before the
    /// monotonicity floor. Negative here is an instrument artefact, not a
    /// finding.
    pub raw_gain_bits: f32,
    /// True when `raw_gain_bits` was below zero and the floor moved it.
    pub monotonicity_floor_applied: bool,
    /// The instrument behind each of the three terms; `None` when the pair was
    /// not measured.
    pub estimators: Option<SynergyEstimators>,
    pub n_samples: usize,
    /// True when the gain reaches [`MIN_SYNERGY_GAIN_BITS`].
    pub synergistic: bool,
    /// True when the pair carries no trustworthy measurement — either it was
    /// not measured at all, or the floor had to move a physically impossible
    /// value.
    pub provisional: bool,
    pub state: SynergyPairState,
    /// Operator-facing explanation, present exactly when the pair is not
    /// `Measured`.
    pub unmeasured_reason: Option<String>,
}

/// Result of one synergy pass over a panel's lens pairs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynergyReport {
    pub panel_version: u32,
    /// Lenses present in the panel corpus.
    pub n_lenses: usize,
    /// Lenses actually paired in this pass (the corpus may be truncated to a
    /// bounded top-`k` by marginal bits; the difference is reported, never
    /// silently dropped).
    pub lenses_paired: usize,
    /// Records carrying the requested anchor.
    pub anchored_records: usize,
    pub pairs_evaluated: usize,
    /// Pairs reported without a measured gain, for any reason.
    pub pairs_unmeasured: usize,
    /// Pairs refused because no single instrument could measure all three terms.
    pub pairs_cross_estimator_unpinnable: usize,
    /// Measured pairs whose raw gain was negative and got floored at zero.
    pub pairs_monotonicity_floored: usize,
    /// Pairs whose gain reached [`MIN_SYNERGY_GAIN_BITS`].
    pub synergistic_pairs: usize,
    /// Largest measured gain over every evaluated pair; `0.0` when no pair was
    /// measurable.
    pub max_gain_bits: f32,
    pub pairs: Vec<SynergyPair>,
}

/// The `WholeMinusMax` arithmetic and its data-processing-inequality floor —
/// the **single** implementation of `pair - max(left, right)` in the engine.
///
/// Every path that computes a pair gain calls this one function (#1942 ask 4):
/// two implementations of the same information-theoretic quantity with
/// different discipline is exactly how #1941 came to exist on one path and not
/// the other. The instrument check belongs to the *caller*, because the type of
/// "instrument" differs by path — [`MiEstimator`] on the [`synergy_gain`] path,
/// [`crate::EstimatorKind`] on the ensemble path — but the arithmetic, the
/// validation and the visible floor are shared.
///
/// Returns `(gain_bits, raw_gain_bits, monotonicity_floor_applied)`, where
/// `gain_bits` is `max(0, raw_gain_bits)` and the flag says whether the floor
/// moved it. A clamped row is always visibly clamped.
///
/// # Errors
///
/// Returns [`CALYX_ASSAY_INVALID_SYNERGY`] when any term is non-finite or
/// negative — a mutual information in bits is neither.
pub fn whole_minus_max_gain(
    pair_bits: f32,
    left_bits: f32,
    right_bits: f32,
) -> Result<(f32, f32, bool)> {
    for (name, value) in [
        ("pair_bits", pair_bits),
        ("left_bits", left_bits),
        ("right_bits", right_bits),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(CalyxError {
                code: CALYX_ASSAY_INVALID_SYNERGY,
                message: format!(
                    "synergy term {name}={value} must be a finite, non-negative bit measurement"
                ),
                remediation: "re-measure the pair and both marginals over the same paired sample set",
            });
        }
    }
    let raw = pair_bits - left_bits.max(right_bits);
    // The data-processing inequality is a law: `[a‖b]` determines `a`, so the
    // pair cannot carry less about the outcome than either half. A negative
    // raw value is the instrument, not the corpus.
    let floored = raw < 0.0;
    Ok((if floored { 0.0 } else { raw }, raw, floored))
}

/// Computes the `WholeMinusMax` synergy gain for one lens pair, and reports
/// whether the data-processing-inequality floor had to move it.
///
/// Returns `(gain_bits, raw_gain_bits, monotonicity_floor_applied)`.
///
/// # Errors
///
/// Returns [`CALYX_ASSAY_INVALID_SYNERGY`] when any term is non-finite or
/// negative — a mutual information in bits is neither — and
/// [`CALYX_ASSAY_SYNERGY_CROSS_ESTIMATOR`] when the three terms did not come
/// from one instrument.
pub fn synergy_gain(
    pair_bits: f32,
    left_bits: f32,
    right_bits: f32,
    estimators: SynergyEstimators,
) -> Result<(f32, f32, bool)> {
    let gain = whole_minus_max_gain(pair_bits, left_bits, right_bits)?;
    if !estimators.is_homogeneous() {
        return Err(CalyxError {
            code: CALYX_ASSAY_SYNERGY_CROSS_ESTIMATOR,
            message: format!(
                "synergy gain subtracts estimates produced by different instruments: pair={} left={} right={}. A difference of information quantities is only interpretable when the estimators' biases cancel, which is a property of one estimator at one scale (Kraskov et al. 2004)",
                estimators.pair.as_str(),
                estimators.left.as_str(),
                estimators.right.as_str()
            ),
            remediation: "pin all three columns to one estimator (MiEstimatorChoice::DiscretePlugin, ::ContinuousKsg, or ::LogisticProbe) and re-measure, or report the pair unmeasured",
        });
    }
    Ok(gain)
}

/// Builds one measured pair record from its three same-sample-set,
/// same-instrument terms.
///
/// # Errors
///
/// Propagates [`synergy_gain`]'s validation failure.
pub fn synergy_pair(
    a: SlotId,
    b: SlotId,
    pair_bits: f32,
    left_bits: f32,
    right_bits: f32,
    n_samples: usize,
    estimators: SynergyEstimators,
) -> Result<SynergyPair> {
    let (gain_bits, raw_gain_bits, monotonicity_floor_applied) =
        synergy_gain(pair_bits, left_bits, right_bits, estimators)?;
    Ok(SynergyPair {
        a,
        b,
        pair_bits,
        left_bits,
        right_bits,
        gain_bits,
        raw_gain_bits,
        monotonicity_floor_applied,
        estimators: Some(estimators),
        n_samples,
        synergistic: gain_bits >= MIN_SYNERGY_GAIN_BITS,
        // A floored row is a measurement whose instrument disagreed with a law.
        // The clamped `0.0` is the honest reading, but it is not something the
        // estimator said, so the row is not sold as clean.
        provisional: monotonicity_floor_applied,
        state: SynergyPairState::Measured,
        unmeasured_reason: None,
    })
}

/// A pair that could not be measured. Reported with zeroed terms, a named
/// `state`, and the operator-facing `reason`, so it is never mistaken for a
/// measured zero gain and never silently dropped from the report.
#[must_use]
pub fn unmeasured_synergy_pair(
    a: SlotId,
    b: SlotId,
    n_samples: usize,
    state: SynergyPairState,
    reason: String,
) -> SynergyPair {
    SynergyPair {
        a,
        b,
        pair_bits: 0.0,
        left_bits: 0.0,
        right_bits: 0.0,
        gain_bits: 0.0,
        raw_gain_bits: 0.0,
        monotonicity_floor_applied: false,
        estimators: None,
        n_samples,
        synergistic: false,
        provisional: true,
        state,
        unmeasured_reason: Some(reason),
    }
}

/// Assembles a synergy report, deriving the summary counters from the measured
/// pairs so the report can never disagree with its own rows.
#[must_use]
pub fn synergy_report(
    panel_version: u32,
    n_lenses: usize,
    lenses_paired: usize,
    anchored_records: usize,
    pairs: Vec<SynergyPair>,
) -> SynergyReport {
    let measured = |pair: &&SynergyPair| pair.state == SynergyPairState::Measured;
    let pairs_evaluated = pairs.iter().filter(measured).count();
    let pairs_unmeasured = pairs.len() - pairs_evaluated;
    let pairs_cross_estimator_unpinnable = pairs
        .iter()
        .filter(|pair| pair.state == SynergyPairState::CrossEstimatorUnpinnable)
        .count();
    let pairs_monotonicity_floored = pairs
        .iter()
        .filter(|pair| pair.monotonicity_floor_applied)
        .count();
    let synergistic_pairs = pairs.iter().filter(|pair| pair.synergistic).count();
    let max_gain_bits = pairs
        .iter()
        .filter(measured)
        .map(|pair| pair.gain_bits)
        .fold(0.0_f32, f32::max);
    SynergyReport {
        panel_version,
        n_lenses,
        lenses_paired,
        anchored_records,
        pairs_evaluated,
        pairs_unmeasured,
        pairs_cross_estimator_unpinnable,
        pairs_monotonicity_floored,
        synergistic_pairs,
        max_gain_bits,
        pairs,
    }
}
