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
//! storing when the pair says something neither lens says on its own. All three
//! terms **must** be measured over the same paired sample set, or the
//! difference is not a synergy at all; [`synergy_gain`] therefore refuses
//! non-finite or negative inputs rather than returning a number that cannot be
//! interpreted.
//!
//! The gain may be negative: that is the redundant regime (the pair carries no
//! more than its better half), and it is reported as measured, never clamped.

use calyx_core::{CalyxError, Result, SlotId};
use serde::{Deserialize, Serialize};

/// Gain floor (bits) at which a pair counts as carrying genuine synergy. Same
/// floor as the lens admission contract's signal floor (handbook section 13).
pub const MIN_SYNERGY_GAIN_BITS: f32 = crate::contract::MIN_SIGNAL_BITS;

/// Error code raised when a synergy term is not a usable bit measurement.
pub const CALYX_ASSAY_INVALID_SYNERGY: &str = "CALYX_ASSAY_INVALID_SYNERGY";

/// One measured lens pair: the joint bits, both marginals, and the resulting
/// `WholeMinusMax` gain, all over the *same* paired sample set.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynergyPair {
    pub a: SlotId,
    pub b: SlotId,
    /// `I(a,b ; Y)` over the records where both lenses and the anchor are present.
    pub pair_bits: f32,
    /// `I(a ; Y)` over that same record set.
    pub left_bits: f32,
    /// `I(b ; Y)` over that same record set.
    pub right_bits: f32,
    /// `pair_bits - max(left_bits, right_bits)`; negative means redundant.
    pub gain_bits: f32,
    pub n_samples: usize,
    /// True when the gain reaches [`MIN_SYNERGY_GAIN_BITS`].
    pub synergistic: bool,
    /// True when the pair had too few paired samples to be trusted.
    pub provisional: bool,
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
    /// Pairs whose gain reached [`MIN_SYNERGY_GAIN_BITS`].
    pub synergistic_pairs: usize,
    /// Largest measured gain over every evaluated pair; `0.0` when no pair was
    /// measurable.
    pub max_gain_bits: f32,
    pub pairs: Vec<SynergyPair>,
}

/// Computes the `WholeMinusMax` synergy gain for one lens pair.
///
/// # Errors
///
/// Returns [`CALYX_ASSAY_INVALID_SYNERGY`] when any term is non-finite or
/// negative — a mutual information in bits is neither.
pub fn synergy_gain(pair_bits: f32, left_bits: f32, right_bits: f32) -> Result<f32> {
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
    Ok(pair_bits - left_bits.max(right_bits))
}

/// Builds one measured pair record from its three same-sample-set terms.
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
) -> Result<SynergyPair> {
    let gain_bits = synergy_gain(pair_bits, left_bits, right_bits)?;
    Ok(SynergyPair {
        a,
        b,
        pair_bits,
        left_bits,
        right_bits,
        gain_bits,
        n_samples,
        synergistic: gain_bits >= MIN_SYNERGY_GAIN_BITS,
        provisional: false,
    })
}

/// A pair that could not be measured: too few paired samples. Reported with
/// zeroed terms and `provisional = true` so it is never mistaken for a measured
/// zero gain.
#[must_use]
pub fn unmeasured_synergy_pair(a: SlotId, b: SlotId, n_samples: usize) -> SynergyPair {
    SynergyPair {
        a,
        b,
        pair_bits: 0.0,
        left_bits: 0.0,
        right_bits: 0.0,
        gain_bits: 0.0,
        n_samples,
        synergistic: false,
        provisional: true,
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
    let pairs_evaluated = pairs.iter().filter(|pair| !pair.provisional).count();
    let synergistic_pairs = pairs.iter().filter(|pair| pair.synergistic).count();
    let max_gain_bits = pairs
        .iter()
        .filter(|pair| !pair.provisional)
        .map(|pair| pair.gain_bits)
        .fold(0.0_f32, f32::max);
    SynergyReport {
        panel_version,
        n_lenses,
        lenses_paired,
        anchored_records,
        pairs_evaluated,
        synergistic_pairs,
        max_gain_bits,
        pairs,
    }
}
