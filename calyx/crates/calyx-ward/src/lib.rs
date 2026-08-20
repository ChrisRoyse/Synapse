//! Ward guard profile types for per-slot cosine policy enforcement.

pub mod calibrate;
pub mod drift;
pub mod error;
pub mod generate;
pub mod guard;
pub mod identity;
#[cfg(feature = "model-lenses")]
pub mod injection_lens;
pub mod ledger;
pub mod novelty;
#[cfg(feature = "model-lenses")]
mod ort_runtime;
pub mod polis;
pub mod profile;
pub mod query;
pub mod required;
#[cfg(feature = "model-lenses")]
pub mod speaker_lens;
#[cfg(feature = "model-lenses")]
pub mod style_lens;
pub mod verdict;

/// Whether this build contains the optional ONNX/tokenizer-backed Ward lenses.
pub const MODEL_LENSES_COMPILED: bool = cfg!(feature = "model-lenses");

pub use calibrate::{
    CalibrationInput, ESTIMATOR, MIN_BAD_SCORES, SlotKind, TAU_COLD_START, calibrate,
    calibrate_slot, validate_calibration_slots,
};
pub use drift::{
    AnnealHook, DEFAULT_DRIFT_CHANNEL_CAPACITY, DEFAULT_DRIFT_WINDOW, DriftEvent, DriftMonitor,
    GuardHealth, REJECTION_RATE_DRIFT_MULTIPLIER, guard_health,
};
pub use error::{
    CALYX_GUARD_CALIBRATION_SCORE_CONTRACT, CALYX_GUARD_CALIBRATION_SLOT_SHAPE,
    CALYX_GUARD_CALIBRATION_SLOT_STATE, CALYX_GUARD_CALIBRATION_SLOT_UNKNOWN,
    CALYX_GUARD_ID_MISMATCH, CALYX_GUARD_IDENTITY_SLOT_NOT_REQUIRED, CALYX_GUARD_INERT_PROFILE,
    CALYX_GUARD_MISSING_SLOT, CALYX_GUARD_NOT_A_FAILURE, CALYX_GUARD_NOVELTY_SINK, CALYX_GUARD_OOD,
    CALYX_GUARD_POLICY_VIOLATION, CALYX_GUARD_PROVISIONAL, CALYX_WARD_INVALID_DOMAIN,
    CALYX_WARD_INVALID_FREQUENCY, CALYX_WARD_INVALID_INPUT, CALYX_WARD_MISSING_FREQUENCY,
    CALYX_WARD_MODEL_DIM_MISMATCH, CALYX_WARD_MODEL_NOT_FOUND, CALYX_WARD_RUNTIME_ERROR, WardError,
};
pub use generate::{
    GUARDED_PASS_TAG, GUARDED_REJECT_TAG, GUARDED_REJECT_UNPROVENANCED_TAG, GenerateInput,
    GenerateOutput, WAVLM_SAMPLE_RATE, guard_generate, guard_generate_with_ledger,
};
pub use guard::{
    DEFAULT_TAU, MatchedSlots, ProducedSlots, guard, guard_non_high_stakes, guard_result,
    guard_result_with_stakes, validate_high_stakes_profile, validate_non_inert_profile,
};
pub use identity::{IdentityProfile, IdentitySlotConfig};
#[cfg(feature = "model-lenses")]
pub use injection_lens::{
    DEFAULT_INJECTION_MODEL_PATH, DEFAULT_INJECTION_TOKENIZER_PATH,
    INJECTION_INPUT_COVERAGE_POLICY, INJECTION_LABELS, INJECTION_MAX_TOKENS, InjectionLens,
    InjectionProviderPolicy, InjectionScoreBackend,
};
pub use ledger::{
    WardLedgerError, WardLedgerResult, append_calibration_provenance, append_guard_verdict,
    calibrate_with_ledger, guard_with_ledger,
};
pub use novelty::{
    Domain, NovelId, NoveltyHandler, NoveltyRecord, NoveltySignal, NoveltyStatus, SurpriseScore,
    VaultSink, classify_novelty, novel_regions, novelty_action_for_signal, overdue_recurrence_scan,
    surprise_bits,
};
pub use polis::{
    CALYX_POLIS_EMPTY_PERSONA_SET, CALYX_POLIS_INVALID_AXIS, CALYX_POLIS_SLOT_COUNT_MISMATCH,
    CALYX_POLIS_TIE_MISMATCH, CIVIC_SLOT_COUNT, CIVIC_TAU, CivicPersona, CivicPersonaPair,
    PolisCivicError, PolisCivicProof, evaluate_polis_civic_pairs, synthetic_polis_persona_pairs,
};
pub use profile::{
    CalibrationMeta, GuardId, GuardPolicy, GuardProfile, NoveltyAction, SlotCalibrationMeta,
};
pub use query::{
    KernelFirstQueryVerdict, QueryVerdict, RegionSource, TrustedRegion, guard_query,
    guard_query_kernel_first,
};
pub use required::{
    LOAD_BEARING_MIN_BITS, RequiredSlotDerivation, RequiredSlotEvidence, RequiredSlotObservation,
    derive_required_profile, derive_required_slots, derive_required_slots_for_observations,
};
#[cfg(feature = "model-lenses")]
pub use speaker_lens::{
    DEFAULT_WAVLM_MODEL_PATH, SpeakerEmbeddingBackend, SpeakerLens, SpeakerProviderPolicy,
    WAVLM_DIM,
};
#[cfg(feature = "model-lenses")]
pub use style_lens::{
    DEFAULT_STYLE_MODEL_PATH, DEFAULT_STYLE_TOKENIZER_PATH, STYLE_DIM, STYLE_INPUT_COVERAGE_POLICY,
    STYLE_MAX_TOKENS, StyleEmbeddingBackend, StyleLens, StyleProviderPolicy,
};
pub use verdict::{GuardVerdict, SlotVerdict};
