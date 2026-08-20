//! Ward error catalog with fail-closed Calyx codes.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use calyx_core::{CxId, SlotId};

use crate::profile::GuardId;
use crate::verdict::SlotVerdict;

pub const CALYX_GUARD_OOD: &str = "CALYX_GUARD_OOD";
pub const CALYX_GUARD_PROVISIONAL: &str = "CALYX_GUARD_PROVISIONAL";
pub const CALYX_GUARD_INERT_PROFILE: &str = "CALYX_GUARD_INERT_PROFILE";
pub const CALYX_GUARD_MISSING_SLOT: &str = "CALYX_GUARD_MISSING_SLOT";
pub const CALYX_GUARD_POLICY_VIOLATION: &str = "CALYX_GUARD_POLICY_VIOLATION";
pub const CALYX_GUARD_NOT_A_FAILURE: &str = "CALYX_GUARD_NOT_A_FAILURE";
pub const CALYX_GUARD_NOVELTY_SINK: &str = "CALYX_GUARD_NOVELTY_SINK";
pub const CALYX_GUARD_ID_MISMATCH: &str = "CALYX_GUARD_ID_MISMATCH";
pub const CALYX_GUARD_IDENTITY_SLOT_NOT_REQUIRED: &str = "CALYX_GUARD_IDENTITY_SLOT_NOT_REQUIRED";
pub const CALYX_GUARD_CALIBRATION_SLOT_SHAPE: &str = "CALYX_GUARD_CALIBRATION_SLOT_SHAPE";
pub const CALYX_GUARD_CALIBRATION_SLOT_UNKNOWN: &str = "CALYX_GUARD_CALIBRATION_SLOT_UNKNOWN";
pub const CALYX_GUARD_CALIBRATION_SLOT_STATE: &str = "CALYX_GUARD_CALIBRATION_SLOT_STATE";
pub const CALYX_GUARD_CALIBRATION_SCORE_CONTRACT: &str = "CALYX_GUARD_CALIBRATION_SCORE_CONTRACT";
pub const CALYX_WARD_MODEL_NOT_FOUND: &str = "CALYX_WARD_MODEL_NOT_FOUND";
pub const CALYX_WARD_INVALID_INPUT: &str = "CALYX_WARD_INVALID_INPUT";
pub const CALYX_WARD_MODEL_DIM_MISMATCH: &str = "CALYX_WARD_MODEL_DIM_MISMATCH";
pub const CALYX_WARD_RUNTIME_ERROR: &str = "CALYX_WARD_RUNTIME_ERROR";
pub const CALYX_WARD_MISSING_FREQUENCY: &str = "CALYX_WARD_MISSING_FREQUENCY";
pub const CALYX_WARD_INVALID_FREQUENCY: &str = "CALYX_WARD_INVALID_FREQUENCY";
pub const CALYX_WARD_INVALID_DOMAIN: &str = "CALYX_WARD_INVALID_DOMAIN";

/// Fail-closed errors emitted by Ward guard policy checks.
#[derive(Clone, Debug, PartialEq)]
pub enum WardError {
    Ood {
        guard_id: GuardId,
        failing: Vec<SlotVerdict>,
    },
    Provisional {
        guard_id: GuardId,
    },
    MissingSlotCalibration {
        guard_id: GuardId,
        slot: SlotId,
    },
    /// A legacy or malformed profile has no finite serving-score envelope for
    /// a required slot, so its calibrated tau can flip across otherwise valid
    /// floating-point reduction orders.
    CalibrationScoreContract {
        guard_id: GuardId,
        slot: SlotId,
        estimator: String,
        score_tolerance: Option<f32>,
        scoring_engine: Option<String>,
    },
    InertProfile {
        guard_id: GuardId,
        reason: &'static str,
    },
    MissingSlot {
        slot: SlotId,
    },
    PolicyViolation {
        k: usize,
        n_required: usize,
    },
    InsufficientCalibrationData {
        n: usize,
        min: usize,
    },
    InvalidCalibrationInput {
        reason: &'static str,
    },
    /// No tau inside the legal cosine range `[-1, 1]` achieves the requested
    /// false-accept rate, so the corpus is not separable at this target on this
    /// slot (#1925).
    ///
    /// This used to be silently reported as success. `conformal_tau` fell
    /// through to `next_above(max_bad_score)`, and when the worst bad score was
    /// `1.0` — which is guaranteed the moment any bad record shares a slot
    /// vector with a good one, and duplicate vectors are the norm on one-hot,
    /// cyclic and any categorical-derived lens — the tau became
    /// `1.0000001192092896`. No cosine can reach that, so the profile rejected
    /// **everything** while reporting `achieved_far = 0` and a passing
    /// Clopper-Pearson tail: the degenerate outcome wearing the best possible
    /// numbers.
    TauUnreachable {
        slot: SlotId,
        tau: f32,
        max_bad_score: f32,
        target_far: f32,
    },
    InvalidRequiredSlotDerivation {
        reason: &'static str,
    },
    NotAFailure {
        guard_id: GuardId,
    },
    GuardIdMismatch {
        profile_guard_id: GuardId,
        verdict_guard_id: GuardId,
    },
    IdentitySlotNotRequired {
        slot: SlotId,
    },
    /// A calibration input names a panel slot whose vector shape the Ward
    /// guard cannot compare (the guard is dense-cosine only, #1120).
    CalibrationSlotShape {
        slot: SlotId,
        shape: String,
    },
    /// A calibration input names a slot that does not exist in the panel the
    /// profile is being calibrated for (#1120).
    CalibrationSlotUnknown {
        slot: SlotId,
        panel_version: u32,
    },
    /// A calibration input names a slot that is not active, so guarded
    /// queries will never produce a vector for it (#1120).
    CalibrationSlotState {
        slot: SlotId,
        state: String,
    },
    ModelNotFound {
        path: PathBuf,
    },
    InvalidInput {
        reason: String,
    },
    ModelDimMismatch {
        expected: usize,
        actual: usize,
    },
    Runtime {
        reason: String,
    },
    NoveltySink {
        reason: String,
    },
    MissingFrequency {
        cx_id: CxId,
        detail: &'static str,
    },
    InvalidFrequency {
        cx_id: CxId,
        value: f64,
    },
    InvalidDomain {
        reason: String,
    },
}

impl WardError {
    /// Returns the stable Calyx error code for this error.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Ood { .. } => CALYX_GUARD_OOD,
            Self::Provisional { .. } | Self::MissingSlotCalibration { .. } => {
                CALYX_GUARD_PROVISIONAL
            }
            Self::CalibrationScoreContract { .. } => CALYX_GUARD_CALIBRATION_SCORE_CONTRACT,
            Self::InsufficientCalibrationData { .. }
            | Self::InvalidCalibrationInput { .. }
            | Self::TauUnreachable { .. }
            | Self::InvalidRequiredSlotDerivation { .. } => CALYX_GUARD_PROVISIONAL,
            Self::InertProfile { .. } => CALYX_GUARD_INERT_PROFILE,
            Self::MissingSlot { .. } => CALYX_GUARD_MISSING_SLOT,
            Self::PolicyViolation { .. } => CALYX_GUARD_POLICY_VIOLATION,
            Self::NotAFailure { .. } => CALYX_GUARD_NOT_A_FAILURE,
            Self::GuardIdMismatch { .. } => CALYX_GUARD_ID_MISMATCH,
            Self::IdentitySlotNotRequired { .. } => CALYX_GUARD_IDENTITY_SLOT_NOT_REQUIRED,
            Self::CalibrationSlotShape { .. } => CALYX_GUARD_CALIBRATION_SLOT_SHAPE,
            Self::CalibrationSlotUnknown { .. } => CALYX_GUARD_CALIBRATION_SLOT_UNKNOWN,
            Self::CalibrationSlotState { .. } => CALYX_GUARD_CALIBRATION_SLOT_STATE,
            Self::ModelNotFound { .. } => CALYX_WARD_MODEL_NOT_FOUND,
            Self::InvalidInput { .. } => CALYX_WARD_INVALID_INPUT,
            Self::ModelDimMismatch { .. } => CALYX_WARD_MODEL_DIM_MISMATCH,
            Self::Runtime { .. } => CALYX_WARD_RUNTIME_ERROR,
            Self::NoveltySink { .. } => CALYX_GUARD_NOVELTY_SINK,
            Self::MissingFrequency { .. } => CALYX_WARD_MISSING_FREQUENCY,
            Self::InvalidFrequency { .. } => CALYX_WARD_INVALID_FREQUENCY,
            Self::InvalidDomain { .. } => CALYX_WARD_INVALID_DOMAIN,
        }
    }
}

impl fmt::Display for WardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ood { guard_id, failing } => {
                write!(f, "{CALYX_GUARD_OOD}: guard {guard_id} out of distribution")?;
                for slot in failing {
                    write!(f, "; slot {} cos={} tau={}", slot.slot, slot.cos, slot.tau)?;
                }
                Ok(())
            }
            Self::Provisional { guard_id } => write!(
                f,
                "{CALYX_GUARD_PROVISIONAL}: guard {guard_id} is uncalibrated; calibrate before high-stakes use -- run calibrate() with an anchored set >=50 examples"
            ),
            Self::MissingSlotCalibration { guard_id, slot } => write!(
                f,
                "{CALYX_GUARD_PROVISIONAL}: guard {guard_id} missing high-stakes calibration provenance for required slot {slot}; calibrate every required slot before high-stakes use"
            ),
            Self::CalibrationScoreContract {
                guard_id,
                slot,
                estimator,
                score_tolerance,
                scoring_engine,
            } => write!(
                f,
                "{CALYX_GUARD_CALIBRATION_SCORE_CONTRACT}: guard {guard_id} required slot {slot} has no supported serving-score contract (estimator={estimator:?}, scoring_engine={scoring_engine:?}, score_tolerance={score_tolerance:?}); recalibrate this profile with {} and {} before high-stakes use",
                crate::calibrate::ESTIMATOR,
                calyx_core::DENSE_COSINE_SCORING_ENGINE
            ),
            Self::InertProfile { guard_id, reason } => write!(
                f,
                "{CALYX_GUARD_INERT_PROFILE}: guard {guard_id} is inert ({reason}); trusted guard surfaces require at least one required slot and a non-zero pass policy"
            ),
            Self::MissingSlot { slot } => {
                write!(
                    f,
                    "{CALYX_GUARD_MISSING_SLOT}: required slot {slot} is missing"
                )
            }
            Self::PolicyViolation { k, n_required } => write!(
                f,
                "{CALYX_GUARD_POLICY_VIOLATION}: KofN k={k} exceeds required slot count n_required={n_required}"
            ),
            Self::InsufficientCalibrationData { n, min } => write!(
                f,
                "{CALYX_GUARD_PROVISIONAL}: insufficient calibration data n={n} min={min}"
            ),
            Self::TauUnreachable {
                slot,
                tau,
                max_bad_score,
                target_far,
            } => write!(
                f,
                "{CALYX_GUARD_PROVISIONAL}: slot {slot} is not separable at target_far={target_far}: \
                 the only tau achieving it is {tau}, which is outside the cosine range [-1,1] \
                 (the worst adjudicated bad case scores {max_bad_score}, so no threshold can admit \
                 a good record without admitting it). This slot carries no signal separating good \
                 from bad on this corpus; a profile persisted here would reject every input while \
                 reporting a perfect false-accept rate"
            ),
            Self::InvalidCalibrationInput { reason } => write!(
                f,
                "{CALYX_GUARD_PROVISIONAL}: invalid calibration input: {reason}"
            ),
            Self::InvalidRequiredSlotDerivation { reason } => write!(
                f,
                "{CALYX_GUARD_PROVISIONAL}: invalid required-slot derivation: {reason}"
            ),
            Self::NotAFailure { guard_id } => write!(
                f,
                "{CALYX_GUARD_NOT_A_FAILURE}: guard {guard_id} verdict already passed; novelty handling requires a failed verdict"
            ),
            Self::GuardIdMismatch {
                profile_guard_id,
                verdict_guard_id,
            } => write!(
                f,
                "{CALYX_GUARD_ID_MISMATCH}: profile guard {profile_guard_id} does not match verdict guard {verdict_guard_id}"
            ),
            Self::IdentitySlotNotRequired { slot } => write!(
                f,
                "{CALYX_GUARD_IDENTITY_SLOT_NOT_REQUIRED}: identity slot {slot} is not present in guard_profile.required_slots"
            ),
            Self::CalibrationSlotShape { slot, shape } => write!(
                f,
                "{CALYX_GUARD_CALIBRATION_SLOT_SHAPE}: calibration slot {slot} has shape {shape}; the Ward in-region guard compares dense vectors only, so a profile requiring this slot would fail every guarded query with CALYX_SEXTANT_VECTOR_SHAPE — calibrate dense slots only"
            ),
            Self::CalibrationSlotUnknown {
                slot,
                panel_version,
            } => write!(
                f,
                "{CALYX_GUARD_CALIBRATION_SLOT_UNKNOWN}: calibration slot {slot} does not exist in panel version {panel_version}; fix the slot ids in the calibration set"
            ),
            Self::CalibrationSlotState { slot, state } => write!(
                f,
                "{CALYX_GUARD_CALIBRATION_SLOT_STATE}: calibration slot {slot} is {state}, not active; guarded queries only measure active slots, so a profile requiring this slot could never pass"
            ),
            Self::ModelNotFound { path } => write!(
                f,
                "{CALYX_WARD_MODEL_NOT_FOUND}: Ward model not found at {}",
                path.display()
            ),
            Self::InvalidInput { reason } => {
                write!(f, "{CALYX_WARD_INVALID_INPUT}: {reason}")
            }
            Self::ModelDimMismatch { expected, actual } => write!(
                f,
                "{CALYX_WARD_MODEL_DIM_MISMATCH}: model output dim {actual} != expected {expected}"
            ),
            Self::Runtime { reason } => {
                write!(f, "{CALYX_WARD_RUNTIME_ERROR}: {reason}")
            }
            Self::NoveltySink { reason } => {
                write!(f, "{CALYX_GUARD_NOVELTY_SINK}: {reason}")
            }
            Self::MissingFrequency { cx_id, detail } => write!(
                f,
                "{CALYX_WARD_MISSING_FREQUENCY}: {detail} for {cx_id}; Ward novelty classification fails closed"
            ),
            Self::InvalidFrequency { cx_id, value } => write!(
                f,
                "{CALYX_WARD_INVALID_FREQUENCY}: recurrence.frequency for {cx_id} must be a non-negative integer, found {value}"
            ),
            Self::InvalidDomain { reason } => {
                write!(f, "{CALYX_WARD_INVALID_DOMAIN}: {reason}")
            }
        }
    }
}

impl Error for WardError {}
