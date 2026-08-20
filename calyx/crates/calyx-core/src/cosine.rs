//! Shared dense cosine helpers.

use std::collections::BTreeMap;

use crate::SlotId;

/// Stable identifier for the exact dense-cosine implementation and reduction
/// order used by Ward at both calibration and serving time. A new numeric
/// implementation is a new scoring engine, never a silent reuse of thresholds
/// calibrated by this one.
pub const DENSE_COSINE_SCORING_ENGINE: &str = "calyx_core::dense_cosine:f32-sequential-v1";

/// Per-slot tau lookup used by guard-like policies without coupling crates.
pub trait GuardTauProfile {
    fn tau_for(&self, slot: &SlotId) -> Option<f32>;
}

impl GuardTauProfile for BTreeMap<SlotId, f32> {
    fn tau_for(&self, slot: &SlotId) -> Option<f32> {
        self.get(slot).copied()
    }
}

/// How far outside `[-1, 1]` a computed cosine may land before it stops being
/// f32 rounding and starts being a defect.
///
/// `dot / (|a| * |b|)` is not algebraically forced into `[-1, 1]` in floating
/// point: for two identical or near-parallel vectors the numerator and the
/// denominator are computed by different fold orders, and the quotient can
/// round to just over `1.0`. The error is on the order of a few f32 epsilons
/// (~1.2e-7 each); `1e-4` is three orders of magnitude of headroom above that
/// and still far below any difference that could carry meaning.
pub const COSINE_ROUNDING_TOLERANCE: f32 = 1e-4;

/// Applies the shared cosine range contract to an already-computed quotient.
///
/// Single-sourced so every cosine site in the workspace enforces the *same*
/// contract rather than each restating it (#1922). Returns `None` for a value
/// far enough outside `[-1, 1]` to be a defect rather than rounding, and clamps
/// anything inside that tolerance to the range a cosine mathematically has.
#[must_use]
pub fn clamp_cosine_quotient(cosine: f32) -> Option<f32> {
    if !cosine.is_finite() {
        return None;
    }
    if !(-1.0 - COSINE_ROUNDING_TOLERANCE..=1.0 + COSINE_ROUNDING_TOLERANCE).contains(&cosine) {
        return None;
    }
    Some(cosine.clamp(-1.0, 1.0))
}

/// Computes cosine for two dense vectors, failing closed on invalid vectors.
///
/// The result is snapped into `[-1, 1]`, the range a cosine mathematically has.
/// This is not papering over an error — it is enforcing the function's own
/// contract against the one way floating point can violate it. Anything further
/// than [`COSINE_ROUNDING_TOLERANCE`] outside the range is *not* rounding, and
/// returns `None` rather than a clamped value that would hide a real defect.
///
/// # Why this matters (#1919)
///
/// Ward's conformal calibration rejects the entire input with
/// `"scores must be cosine values in [-1,1]"` if any single score falls outside
/// the range, and `dense_cosine` is the exact function it scores with. A
/// leave-one-out score compares a record against its nearest neighbour, so the
/// moment any two records share a guarded slot vector the comparison is
/// self-against-identical and lands on this boundary.
///
/// Duplicate slot vectors are not an edge case, they are the norm: a cyclic
/// hour lens has 24 distinct values, a one-hot has as many as it has
/// categories, and any dense lens derived from a categorical field repeats by
/// construction. So an unclamped cosine made guard calibration fail on
/// realistic corpora — and fail wholesale, with a message naming neither the
/// score nor the slot.
pub fn dense_cosine(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (left, right) in left.iter().zip(right) {
        if !left.is_finite() || !right.is_finite() {
            return None;
        }
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    let denom = left_norm.sqrt() * right_norm.sqrt();
    if !denom.is_finite() || denom <= 0.0 {
        return None;
    }
    clamp_cosine_quotient(dot / denom)
}
