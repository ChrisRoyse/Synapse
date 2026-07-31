//! Per-slot conformal tau calibration for Ward guard profiles.

use std::collections::BTreeMap;

use calyx_core::{Clock, Panel, SlotId, SlotShape, SlotState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::WardError;
use crate::guard::DEFAULT_TAU;
use crate::profile::{CalibrationMeta, GuardProfile, SlotCalibrationMeta};

pub const TAU_COLD_START: f32 = DEFAULT_TAU;
pub const MIN_BAD_SCORES: usize = 50;
pub const ESTIMATOR: &str = "conformal_quantile_v1";

/// Coarse slot role used to choose stricter or looser FAR targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    Identity,
    Stylistic,
    Content,
}

impl SlotKind {
    pub const fn default_target_far(self) -> f32 {
        match self {
            Self::Identity => 0.01,
            Self::Stylistic => 0.05,
            Self::Content => 0.03,
        }
    }

    /// Stable lowercase wire label for the aspect (`/v1/guard perSlot.aspect`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Stylistic => "stylistic",
            Self::Content => "content",
        }
    }
}

/// Grounded calibration scores for one slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationInput {
    pub slot: SlotId,
    pub good_scores: Vec<f32>,
    pub bad_scores: Vec<f32>,
    pub slot_kind: SlotKind,
    pub target_far: f32,
}

/// Fail-closed gate that every calibration input names a slot the calibrated
/// profile can actually guard (#1120). `calibrate` copies every input slot
/// into `required_slots`, and the Ward guard compares dense vectors only, so
/// a sparse/multi, unknown, or non-active slot would persist a profile that
/// fails every guarded query at query time instead of failing here.
///
/// Callers that persist a profile for a concrete vault panel (CLI and MCP
/// `guard calibrate`) must call this before `calibrate`.
pub fn validate_calibration_slots(
    inputs: &[CalibrationInput],
    panel: &Panel,
) -> Result<(), WardError> {
    for input in inputs {
        let slot = panel
            .slots
            .iter()
            .find(|slot| slot.slot_id == input.slot)
            .ok_or(WardError::CalibrationSlotUnknown {
                slot: input.slot,
                panel_version: panel.version,
            })?;
        let inactive = match slot.state {
            SlotState::Active => None,
            SlotState::Parked => Some("parked"),
            SlotState::Retired => Some("retired"),
        };
        if let Some(state) = inactive {
            return Err(WardError::CalibrationSlotState {
                slot: input.slot,
                state: state.to_string(),
            });
        }
        match slot.shape {
            SlotShape::Dense(_) => {}
            SlotShape::Sparse(dim) => {
                return Err(WardError::CalibrationSlotShape {
                    slot: input.slot,
                    shape: format!("sparse({dim})"),
                });
            }
            SlotShape::Multi { token_dim } => {
                return Err(WardError::CalibrationSlotShape {
                    slot: input.slot,
                    shape: format!("multi(token_dim={token_dim})"),
                });
            }
        }
    }
    Ok(())
}

/// Calibrates one slot's tau from known-bad scores and reports achieved FAR/FRR.
pub fn calibrate_slot(
    input: &CalibrationInput,
    alpha: f32,
    clock: &dyn Clock,
) -> Result<(f32, CalibrationMeta), WardError> {
    validate_input(input, alpha)?;
    if input.bad_scores.len() < MIN_BAD_SCORES {
        return Err(WardError::InsufficientCalibrationData {
            n: input.bad_scores.len(),
            min: MIN_BAD_SCORES,
        });
    }

    let mut bad_scores = sorted_scores(&input.bad_scores)?;
    let good_scores = sorted_scores(&input.good_scores)?;
    let tau = conformal_tau(input.slot, &bad_scores, input.target_far, alpha)?;
    let far = fraction(
        input
            .bad_scores
            .iter()
            .filter(|score| **score >= tau)
            .count(),
        input.bad_scores.len(),
    );
    let frr = if input.good_scores.is_empty() {
        0.0
    } else {
        fraction(
            input
                .good_scores
                .iter()
                .filter(|score| **score < tau)
                .count(),
            input.good_scores.len(),
        )
    };
    let corpus_hash = corpus_hash(
        input.slot,
        input.slot_kind,
        input.target_far,
        alpha,
        &good_scores,
        &bad_scores,
    );
    bad_scores.clear();

    Ok((
        tau,
        CalibrationMeta::new(corpus_hash, ESTIMATOR, far, frr, 1.0 - alpha, clock),
    ))
}

/// Calibrates a complete profile by updating tau for every supplied slot.
pub fn calibrate(
    mut profile_template: GuardProfile,
    inputs: Vec<CalibrationInput>,
    alpha: f32,
    clock: &dyn Clock,
) -> Result<GuardProfile, WardError> {
    if inputs.is_empty() {
        return Err(WardError::InvalidCalibrationInput {
            reason: "no calibration inputs",
        });
    }

    let mut metas = Vec::new();
    for input in &inputs {
        let (tau, meta) = calibrate_slot(input, alpha, clock)?;
        profile_template.tau.insert(input.slot, tau);
        if !profile_template.required_slots.contains(&input.slot) {
            profile_template.required_slots.push(input.slot);
        }
        metas.push((input.slot, input.slot_kind, meta));
    }
    profile_template.required_slots.sort_unstable();
    profile_template.required_slots.dedup();
    profile_template.calibration = Some(merge_meta(&metas, alpha, clock)?);
    Ok(profile_template)
}

fn validate_input(input: &CalibrationInput, alpha: f32) -> Result<(), WardError> {
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return Err(WardError::InvalidCalibrationInput {
            reason: "alpha must be finite and in [0,1]",
        });
    }
    if !input.target_far.is_finite() || !(0.0..=1.0).contains(&input.target_far) {
        return Err(WardError::InvalidCalibrationInput {
            reason: "target_far must be finite and in [0,1]",
        });
    }
    if input.target_far > input.slot_kind.default_target_far() {
        return Err(WardError::InvalidCalibrationInput {
            reason: "target_far exceeds slot_kind maximum",
        });
    }
    Ok(())
}

fn sorted_scores(scores: &[f32]) -> Result<Vec<f32>, WardError> {
    if scores.iter().any(|score| !score.is_finite()) {
        return Err(WardError::InvalidCalibrationInput {
            reason: "scores must be finite",
        });
    }
    if scores.iter().any(|score| !(-1.0..=1.0).contains(score)) {
        return Err(WardError::InvalidCalibrationInput {
            reason: "scores must be cosine values in [-1,1]",
        });
    }
    let mut scores = scores.to_vec();
    scores.sort_by(|left, right| left.total_cmp(right));
    Ok(scores)
}

/// Largest tau a cosine can ever reach.
///
/// A guard scores with `dense_cosine`, which returns a value in `[-1, 1]`
/// (enforced since #1923). A tau above `1.0` is therefore not a strict
/// threshold — it is an unsatisfiable one, and a profile carrying it rejects
/// every input that will ever be presented to it. See
/// [`WardError::TauUnreachable`] (#1925).
const MAX_REACHABLE_TAU: f32 = 1.0;

/// Rejects a tau no cosine can reach, instead of returning it as a threshold.
fn reachable_tau(
    slot: SlotId,
    tau: f32,
    sorted_bad_scores: &[f32],
    target_far: f32,
) -> Result<f32, WardError> {
    if tau > MAX_REACHABLE_TAU {
        return Err(WardError::TauUnreachable {
            slot,
            tau,
            max_bad_score: *sorted_bad_scores.last().expect("non-empty"),
            target_far,
        });
    }
    Ok(tau)
}

fn conformal_tau(
    slot: SlotId,
    sorted_bad_scores: &[f32],
    target_far: f32,
    alpha: f32,
) -> Result<f32, WardError> {
    if sorted_bad_scores.is_empty() {
        return Err(WardError::InsufficientCalibrationData {
            n: 0,
            min: MIN_BAD_SCORES,
        });
    }
    if target_far == 0.0 {
        return reachable_tau(
            slot,
            next_above(*sorted_bad_scores.last().expect("non-empty")),
            sorted_bad_scores,
            target_far,
        );
    }
    let mut candidates = Vec::with_capacity(sorted_bad_scores.len() * 2);
    for score in sorted_bad_scores {
        if candidates.last().copied() != Some(*score) {
            candidates.push(*score);
            candidates.push(next_above(*score));
        }
    }
    candidates.sort_by(|left, right| left.total_cmp(right));
    candidates.dedup();
    for candidate in candidates {
        let bad_accepts = sorted_bad_scores
            .iter()
            .filter(|score| **score >= candidate)
            .count();
        let candidate_far = fraction(bad_accepts, sorted_bad_scores.len());
        if candidate_far <= target_far + f32::EPSILON
            && confidence_bound_satisfied(bad_accepts, sorted_bad_scores.len(), target_far, alpha)
        {
            return reachable_tau(slot, candidate, sorted_bad_scores, target_far);
        }
    }
    // No candidate inside the corpus satisfied both the FAR target and the
    // confidence bound. The fallback is the smallest threshold that admits no
    // bad case at all — which is only a *threshold* if it is reachable. When
    // the worst bad case scores 1.0 it is not, and returning it would report
    // the degenerate "rejects everything" profile as a perfect FAR (#1925).
    reachable_tau(
        slot,
        next_above(*sorted_bad_scores.last().expect("non-empty")),
        sorted_bad_scores,
        target_far,
    )
}

fn confidence_bound_satisfied(
    bad_accepts: usize,
    bad_count: usize,
    target_far: f32,
    alpha: f32,
) -> bool {
    binomial_cdf_at_most(bad_accepts, bad_count, f64::from(target_far))
        <= f64::from(alpha) + f64::EPSILON
}

fn binomial_cdf_at_most(successes: usize, trials: usize, probability: f64) -> f64 {
    if successes >= trials {
        return 1.0;
    }
    if probability <= 0.0 {
        return 1.0;
    }
    if probability >= 1.0 {
        return if successes >= trials { 1.0 } else { 0.0 };
    }
    let complement = 1.0 - probability;
    let mut term = complement.powf(trials as f64);
    let mut sum = term;
    for index in 0..successes {
        term *= (trials - index) as f64 / (index + 1) as f64 * probability / complement;
        sum += term;
        if sum > 1.0 {
            return 1.0;
        }
    }
    sum
}

fn merge_meta(
    metas: &[(SlotId, SlotKind, CalibrationMeta)],
    alpha: f32,
    clock: &dyn Clock,
) -> Result<CalibrationMeta, WardError> {
    if metas.is_empty() {
        return Err(WardError::InvalidCalibrationInput {
            reason: "no calibration metadata",
        });
    }
    let mut hasher = Sha256::new();
    let mut far = 0.0_f32;
    let mut frr = 0.0_f32;
    let mut per_slot = BTreeMap::new();
    for (slot, slot_kind, meta) in metas {
        hasher.update(slot.get().to_be_bytes());
        hasher.update(meta.corpus_hash);
        far = far.max(meta.far);
        frr = frr.max(meta.frr);
        per_slot.insert(
            *slot,
            SlotCalibrationMeta::from_calibration(meta, *slot_kind),
        );
    }
    let hash = hasher.finalize();
    let mut corpus_hash = [0_u8; 32];
    corpus_hash.copy_from_slice(&hash);
    let mut merged = CalibrationMeta::new(corpus_hash, ESTIMATOR, far, frr, 1.0 - alpha, clock);
    merged.per_slot = per_slot;
    Ok(merged)
}

fn corpus_hash(
    slot: SlotId,
    slot_kind: SlotKind,
    target_far: f32,
    alpha: f32,
    good_scores: &[f32],
    bad_scores: &[f32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(slot.get().to_be_bytes());
    hasher.update([slot_kind as u8]);
    hasher.update(target_far.to_le_bytes());
    hasher.update(alpha.to_le_bytes());
    for score in good_scores {
        hasher.update(score.to_le_bytes());
    }
    hasher.update([0xff]);
    for score in bad_scores {
        hasher.update(score.to_le_bytes());
    }
    let hash = hasher.finalize();
    let mut out = [0_u8; 32];
    out.copy_from_slice(&hash);
    out
}

fn fraction(count: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        count as f32 / total as f32
    }
}

fn next_above(value: f32) -> f32 {
    if value == 0.0 {
        f32::from_bits(1)
    } else if value > 0.0 {
        f32::from_bits(value.to_bits() + 1)
    } else {
        f32::from_bits(value.to_bits() - 1)
    }
}
