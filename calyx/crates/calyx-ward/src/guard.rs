//! Per-slot Ward guard math.

use std::collections::BTreeMap;

use calyx_core::{SlotId, dense_cosine};

use crate::error::WardError;
use crate::profile::{GuardPolicy, GuardProfile};
use crate::verdict::{GuardVerdict, SlotVerdict};

pub const DEFAULT_TAU: f32 = 0.7;

pub type ProducedSlots = BTreeMap<SlotId, Vec<f32>>;
pub type MatchedSlots = BTreeMap<SlotId, Vec<f32>>;

// INVARIANT A3: required slots are scored independently; no aggregate vector gate.
/// Evaluates every required slot independently under the profile policy.
///
/// Missing required slots fail closed with `WardError::MissingSlot`. Slots with
/// invalid vectors produce a failed slot verdict instead of panicking, preserving
/// the full decomposition for callers and FSV readback.
pub fn guard(
    profile: &GuardProfile,
    produced: &ProducedSlots,
    matched: &MatchedSlots,
    high_stakes: bool,
) -> Result<GuardVerdict, WardError> {
    let required = required_slots(profile);
    validate_non_inert_required(profile, &required)?;
    if high_stakes {
        validate_high_stakes_profile(profile, &required)?;
    }

    let mut per_slot = Vec::new();
    for slot in required {
        let produced_vec = produced.get(&slot).ok_or(WardError::MissingSlot { slot })?;
        let matched_vec = matched.get(&slot).ok_or(WardError::MissingSlot { slot })?;
        let tau = profile.tau_for(&slot).unwrap_or(DEFAULT_TAU);
        let (cos, pass) = match dense_cosine(produced_vec, matched_vec) {
            Some(cos) => (cos, cos >= tau),
            None => (0.0, false),
        };
        per_slot.push(SlotVerdict {
            slot,
            cos,
            tau,
            pass,
        });
    }

    let pass_count = per_slot.iter().filter(|slot| slot.pass).count();
    let overall_pass = match profile.policy {
        GuardPolicy::AllRequired => pass_count == per_slot.len(),
        GuardPolicy::KofN { k } => pass_count >= k,
    };
    let action = (!overall_pass).then(|| profile.novelty_action.clone());
    Ok(GuardVerdict {
        guard_id: profile.guard_id,
        overall_pass,
        provisional: !profile.is_calibrated(),
        per_slot,
        action,
    })
}

/// Rejects guard profiles that cannot require evidence from any slot.
pub fn validate_non_inert_profile(profile: &GuardProfile) -> Result<(), WardError> {
    let required = required_slots(profile);
    validate_non_inert_required(profile, &required)
}

fn validate_non_inert_required(
    profile: &GuardProfile,
    required: &[SlotId],
) -> Result<(), WardError> {
    if required.is_empty() {
        return Err(WardError::InertProfile {
            guard_id: profile.guard_id,
            reason: "empty_required_slots",
        });
    }
    if let GuardPolicy::KofN { k } = profile.policy {
        if k == 0 {
            return Err(WardError::InertProfile {
                guard_id: profile.guard_id,
                reason: "kofn_zero",
            });
        }
        if k > required.len() {
            return Err(WardError::PolicyViolation {
                k,
                n_required: required.len(),
            });
        }
    }
    Ok(())
}

/// Validates that every required slot carries current, finite calibration and
/// a serving-score parity envelope. Profiles written before the envelope was
/// introduced remain readable history but are never high-stakes authority.
pub fn validate_high_stakes_profile(
    profile: &GuardProfile,
    required: &[SlotId],
) -> Result<(), WardError> {
    let calibration = profile.calibration.as_ref().ok_or(WardError::Provisional {
        guard_id: profile.guard_id,
    })?;
    for slot in required {
        let Some(slot_meta) = calibration.per_slot.get(slot) else {
            return Err(WardError::MissingSlotCalibration {
                guard_id: profile.guard_id,
                slot: *slot,
            });
        };
        if profile.tau_for(slot).is_none() {
            return Err(WardError::MissingSlotCalibration {
                guard_id: profile.guard_id,
                slot: *slot,
            });
        }
        let score_tolerance = slot_meta.score_tolerance;
        let scoring_engine = slot_meta.scoring_engine.as_deref();
        if scoring_engine != Some(calyx_core::DENSE_COSINE_SCORING_ENGINE)
            || score_tolerance
                .is_none_or(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(WardError::CalibrationScoreContract {
                guard_id: profile.guard_id,
                slot: *slot,
                estimator: slot_meta.estimator.clone(),
                score_tolerance,
                scoring_engine: slot_meta.scoring_engine.clone(),
            });
        }
    }
    Ok(())
}

/// Runs `guard()` for non-critical embeddings that may use provisional profiles.
pub fn guard_non_high_stakes(
    profile: &GuardProfile,
    produced: &ProducedSlots,
    matched: &MatchedSlots,
) -> Result<GuardVerdict, WardError> {
    guard(profile, produced, matched, false)
}

/// Runs `guard()` and returns `WardError::Ood` when the verdict does not pass.
pub fn guard_result(
    profile: &GuardProfile,
    produced: &ProducedSlots,
    matched: &MatchedSlots,
) -> Result<GuardVerdict, WardError> {
    guard_result_with_stakes(profile, produced, matched, false)
}

/// Runs `guard()` with the caller's stake level and wraps non-pass as OOD.
pub fn guard_result_with_stakes(
    profile: &GuardProfile,
    produced: &ProducedSlots,
    matched: &MatchedSlots,
    high_stakes: bool,
) -> Result<GuardVerdict, WardError> {
    let verdict = guard(profile, produced, matched, high_stakes)?;
    if verdict.overall_pass {
        Ok(verdict)
    } else {
        Err(WardError::Ood {
            guard_id: verdict.guard_id,
            failing: verdict
                .per_slot
                .iter()
                .filter(|slot| !slot.pass)
                .cloned()
                .collect(),
        })
    }
}

fn required_slots(profile: &GuardProfile) -> Vec<SlotId> {
    let mut slots = profile.required_slots.clone();
    slots.sort_unstable();
    slots.dedup();
    slots
}
