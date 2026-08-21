//! Per-slot conformal tau calibration for Ward guard profiles.

use std::collections::BTreeMap;

use calyx_core::{Clock, Panel, SlotId, SlotShape, SlotState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::WardError;
use crate::guard::DEFAULT_TAU;
use crate::profile::{CalibrationMeta, GuardPolicy, GuardProfile, SlotCalibrationMeta};

pub const TAU_COLD_START: f32 = DEFAULT_TAU;
pub const MIN_BAD_SCORES: usize = 50;
pub const ESTIMATOR: &str = "conformal_quantile_score_contract_v2";
pub const JOINT_POLICY_ESTIMATOR: &str = "conformal_joint_policy_score_contract_v1";

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
    /// Physical row identities in exactly the same order as `good_scores`.
    /// Joint calibration refuses when slots do not name the same aligned
    /// corpus; equal vector counts alone cannot prove row alignment.
    pub good_case_ids: Vec<String>,
    /// Physical row identities in exactly the same order as `bad_scores`.
    pub bad_case_ids: Vec<String>,
    pub slot_kind: SlotKind,
    pub target_far: f32,
    /// Maximum absolute difference between the score reduction used to build
    /// this corpus and the score reduction used by the serving guard. Ward
    /// expands bad scores upward and good scores downward by this amount before
    /// choosing tau, so a legal backend/reduction-order difference cannot turn
    /// a calibrated rejection into a serving acceptance.
    pub score_tolerance: f32,
    /// Exact numeric scorer used to build this corpus. Ward serves with
    /// `calyx_core::dense_cosine`; a threshold from another scorer is a
    /// different calibration domain and is refused rather than assumed equal.
    pub scoring_engine: String,
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

    let mut bad_scores = conservative_bad_scores(&input.bad_scores, input.score_tolerance)?;
    let good_scores = conservative_good_scores(&input.good_scores, input.score_tolerance)?;
    let tau = conformal_tau(input.slot, &bad_scores, input.target_far, alpha)?;
    let far = fraction(
        bad_scores.iter().filter(|score| **score >= tau).count(),
        bad_scores.len(),
    );
    let frr = if good_scores.is_empty() {
        0.0
    } else {
        fraction(
            good_scores.iter().filter(|score| **score < tau).count(),
            good_scores.len(),
        )
    };
    let corpus_hash = corpus_hash(input, alpha, &good_scores, &bad_scores);
    bad_scores.clear();

    Ok((tau, {
        let mut meta = CalibrationMeta::new(corpus_hash, ESTIMATOR, far, frr, 1.0 - alpha, clock);
        meta.score_tolerance = Some(input.score_tolerance);
        meta.scoring_engine = Some(input.scoring_engine.clone());
        meta
    }))
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

    if inputs.len() > 1 {
        return calibrate_joint_policy(profile_template, &inputs, alpha, clock);
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
    if !input.score_tolerance.is_finite() || !(0.0..=1.0).contains(&input.score_tolerance) {
        return Err(WardError::InvalidCalibrationInput {
            reason: "score_tolerance must be finite and in [0,1]",
        });
    }
    if input.scoring_engine != calyx_core::DENSE_COSINE_SCORING_ENGINE {
        return Err(WardError::InvalidCalibrationInput {
            reason: "scoring_engine must name the exact dense-cosine scorer Ward serves with",
        });
    }
    validate_case_ids(&input.good_case_ids, input.good_scores.len())?;
    validate_case_ids(&input.bad_case_ids, input.bad_scores.len())?;
    Ok(())
}

fn validate_case_ids(ids: &[String], score_count: usize) -> Result<(), WardError> {
    if ids.len() != score_count {
        return Err(WardError::InvalidCalibrationInput {
            reason: "calibration case-id count must equal its score count",
        });
    }
    let mut seen = BTreeMap::new();
    for id in ids {
        if id.trim().is_empty() || id.trim() != id {
            return Err(WardError::InvalidCalibrationInput {
                reason: "calibration case ids must be non-blank and trimmed",
            });
        }
        if seen.insert(id, ()).is_some() {
            return Err(WardError::InvalidCalibrationInput {
                reason: "calibration case ids must be unique within each role",
            });
        }
    }
    Ok(())
}

fn calibrate_joint_policy(
    mut profile: GuardProfile,
    inputs: &[CalibrationInput],
    alpha: f32,
    clock: &dyn Clock,
) -> Result<GuardProfile, WardError> {
    let first = inputs.first().ok_or(WardError::InvalidCalibrationInput {
        reason: "no calibration inputs",
    })?;
    for input in inputs {
        validate_input(input, alpha)?;
        if input.bad_scores.len() < MIN_BAD_SCORES {
            return Err(WardError::InsufficientCalibrationData {
                n: input.bad_scores.len(),
                min: MIN_BAD_SCORES,
            });
        }
        if input.good_scores.is_empty() {
            return Err(WardError::InvalidCalibrationInput {
                reason: "joint calibration requires at least one aligned good score per slot",
            });
        }
        if input.good_case_ids != first.good_case_ids || input.bad_case_ids != first.bad_case_ids {
            return Err(WardError::InvalidCalibrationInput {
                reason: "joint calibration slots must carry the exact same ordered physical good/bad identities",
            });
        }
    }
    validate_policy_cardinality(&profile.policy, inputs.len())?;

    let target_far = inputs
        .iter()
        .map(|input| input.target_far)
        .reduce(f32::min)
        .ok_or(WardError::InvalidCalibrationInput {
            reason: "joint calibration has no FAR target",
        })?;
    let bad_by_slot = inputs
        .iter()
        .map(|input| conservative_bad_scores_aligned(&input.bad_scores, input.score_tolerance))
        .collect::<Result<Vec<_>, _>>()?;
    let good_by_slot = inputs
        .iter()
        .map(|input| conservative_good_scores_aligned(&input.good_scores, input.score_tolerance))
        .collect::<Result<Vec<_>, _>>()?;
    let mut joint_bad = joint_policy_scores(&profile.policy, &bad_by_slot)?;
    let joint_good = joint_policy_scores(&profile.policy, &good_by_slot)?;
    joint_bad.sort_by(f32::total_cmp);
    let tau = conformal_tau(first.slot, &joint_bad, target_far, alpha)?;
    let joint_bad_accepts = joint_bad.iter().filter(|score| **score >= tau).count();
    let joint_far = fraction(joint_bad_accepts, joint_bad.len());
    let joint_frr = fraction(
        joint_good.iter().filter(|score| **score < tau).count(),
        joint_good.len(),
    );

    let mut per_slot = BTreeMap::new();
    let mut joint_hasher = Sha256::new();
    joint_hasher.update(JOINT_POLICY_ESTIMATOR.as_bytes());
    joint_hasher.update(policy_fingerprint(&profile.policy));
    joint_hasher.update(target_far.to_le_bytes());
    joint_hasher.update(alpha.to_le_bytes());
    joint_hasher.update(tau.to_le_bytes());
    for ((input, bad_scores), good_scores) in inputs.iter().zip(&bad_by_slot).zip(&good_by_slot) {
        profile.tau.insert(input.slot, tau);
        if !profile.required_slots.contains(&input.slot) {
            profile.required_slots.push(input.slot);
        }
        let far = fraction(
            bad_scores.iter().filter(|score| **score >= tau).count(),
            bad_scores.len(),
        );
        let frr = fraction(
            good_scores.iter().filter(|score| **score < tau).count(),
            good_scores.len(),
        );
        let slot_hash = corpus_hash(input, alpha, good_scores, bad_scores);
        joint_hasher.update(input.slot.get().to_be_bytes());
        joint_hasher.update(slot_hash);
        let mut slot_meta = CalibrationMeta::new(
            slot_hash,
            JOINT_POLICY_ESTIMATOR,
            far,
            frr,
            1.0 - alpha,
            clock,
        );
        slot_meta.score_tolerance = Some(input.score_tolerance);
        slot_meta.scoring_engine = Some(input.scoring_engine.clone());
        per_slot.insert(
            input.slot,
            SlotCalibrationMeta::from_calibration(&slot_meta, input.slot_kind),
        );
    }
    profile.required_slots.sort_unstable();
    profile.required_slots.dedup();
    let hash = joint_hasher.finalize();
    let mut corpus_hash = [0_u8; 32];
    corpus_hash.copy_from_slice(&hash);
    let mut meta = CalibrationMeta::new(
        corpus_hash,
        JOINT_POLICY_ESTIMATOR,
        joint_far,
        joint_frr,
        1.0 - alpha,
        clock,
    );
    meta.score_tolerance = inputs
        .iter()
        .map(|input| input.score_tolerance)
        .reduce(f32::max);
    meta.scoring_engine = Some(first.scoring_engine.clone());
    meta.per_slot = per_slot;
    profile.calibration = Some(meta);
    Ok(profile)
}

fn validate_policy_cardinality(policy: &GuardPolicy, slots: usize) -> Result<(), WardError> {
    match policy {
        GuardPolicy::AllRequired => Ok(()),
        GuardPolicy::KofN { k } if *k > 0 && *k <= slots => Ok(()),
        GuardPolicy::KofN { k } => Err(WardError::PolicyViolation {
            k: *k,
            n_required: slots,
        }),
    }
}

fn joint_policy_scores(
    policy: &GuardPolicy,
    scores_by_slot: &[Vec<f32>],
) -> Result<Vec<f32>, WardError> {
    let rows = scores_by_slot
        .first()
        .map(Vec::len)
        .ok_or(WardError::InvalidCalibrationInput {
            reason: "joint calibration has no score rows",
        })?;
    if scores_by_slot.iter().any(|scores| scores.len() != rows) {
        return Err(WardError::InvalidCalibrationInput {
            reason: "joint calibration score matrices are not row-aligned",
        });
    }
    validate_policy_cardinality(policy, scores_by_slot.len())?;
    let mut out = Vec::with_capacity(rows);
    let mut row = Vec::with_capacity(scores_by_slot.len());
    for index in 0..rows {
        row.clear();
        row.extend(scores_by_slot.iter().map(|scores| scores[index]));
        row.sort_by(|left, right| right.total_cmp(left));
        let score = match policy {
            GuardPolicy::AllRequired => *row.last().ok_or(WardError::InvalidCalibrationInput {
                reason: "joint all-required policy has no slots",
            })?,
            GuardPolicy::KofN { k } => row[*k - 1],
        };
        out.push(score);
    }
    Ok(out)
}

fn policy_fingerprint(policy: &GuardPolicy) -> Vec<u8> {
    match policy {
        GuardPolicy::AllRequired => b"all_required".to_vec(),
        GuardPolicy::KofN { k } => format!("k_of_n:{k}").into_bytes(),
    }
}

fn conservative_bad_scores(scores: &[f32], tolerance: f32) -> Result<Vec<f32>, WardError> {
    sorted_scores(scores).map(|scores| {
        scores
            .into_iter()
            .map(|score| (score + tolerance).min(1.0))
            .collect()
    })
}

fn conservative_good_scores(scores: &[f32], tolerance: f32) -> Result<Vec<f32>, WardError> {
    sorted_scores(scores).map(|scores| {
        scores
            .into_iter()
            .map(|score| (score - tolerance).max(-1.0))
            .collect()
    })
}

fn conservative_bad_scores_aligned(scores: &[f32], tolerance: f32) -> Result<Vec<f32>, WardError> {
    validated_scores_in_order(scores).map(|scores| {
        scores
            .into_iter()
            .map(|score| (score + tolerance).min(1.0))
            .collect()
    })
}

fn conservative_good_scores_aligned(scores: &[f32], tolerance: f32) -> Result<Vec<f32>, WardError> {
    validated_scores_in_order(scores).map(|scores| {
        scores
            .into_iter()
            .map(|score| (score - tolerance).max(-1.0))
            .collect()
    })
}

fn validated_scores_in_order(scores: &[f32]) -> Result<Vec<f32>, WardError> {
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
    Ok(scores.to_vec())
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
    merged.score_tolerance = metas
        .iter()
        .filter_map(|(_, _, meta)| meta.score_tolerance)
        .reduce(f32::max);
    merged.scoring_engine = metas
        .first()
        .and_then(|(_, _, meta)| meta.scoring_engine.clone());
    merged.per_slot = per_slot;
    Ok(merged)
}

fn corpus_hash(
    input: &CalibrationInput,
    alpha: f32,
    good_scores: &[f32],
    bad_scores: &[f32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input.slot.get().to_be_bytes());
    hasher.update([input.slot_kind as u8]);
    hasher.update(input.target_far.to_le_bytes());
    hasher.update(alpha.to_le_bytes());
    hasher.update(input.score_tolerance.to_le_bytes());
    hasher.update(input.scoring_engine.as_bytes());
    for id in &input.good_case_ids {
        hasher.update((id.len() as u64).to_be_bytes());
        hasher.update(id.as_bytes());
    }
    hasher.update([0xfe]);
    for id in &input.bad_case_ids {
        hasher.update((id.len() as u64).to_be_bytes());
        hasher.update(id.as_bytes());
    }
    hasher.update([0xfd]);
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
