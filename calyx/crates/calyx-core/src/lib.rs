//! Core Calyx identifiers, model contracts, and shared types.

/// The Reciprocal Rank Fusion rank constant, declared **once** for the whole
/// workspace.
///
/// Cormack, Clarke & Buettcher (SIGIR 2009) define
/// `RRFscore(d) = Σ 1/(k + r(d))` with `k = 60` and 1-based `r(d)`; Calyx adds
/// a per-lens weight `w_s`. This is the *default* only — the value that
/// actually scores a query is carried on `calyx_sextant::FusionContext::rrf_k`
/// and recorded on every reproducible fusion payload, so a tuned vault and its
/// own replay path cannot disagree.
///
/// It lives here because `calyx-sextant` (which fuses) and `calyx-ledger`
/// (which reproduces a fusion) share no other ancestor, and independently
/// redeclaring it is exactly how the two silently desynchronised (issue #1883).
pub const RRF_K_DEFAULT: u32 = 60;

/// The only place in the workspace that converts a configured RRF rank constant
/// to the `f32` the scoring law runs in.
///
/// `f32` represents every integer up to `2^24` exactly, and nothing above it.
/// A `k` outside that range would be silently rounded, so two vaults configured
/// differently could score identically and a reported `rrf_k` would not describe
/// the arithmetic that ran. Returns `None` rather than rounding.
///
/// `k = 0` is rejected separately: under 1-based ranks it is merely extreme, but
/// it is the boundary at which the law stops being the published one, so the
/// domain starts at 1.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "the range check immediately above proves this u32 is exactly representable in f32; this is the single audited conversion point"
)]
pub const fn rrf_k_as_f32(rrf_k: u32) -> Option<f32> {
    if rrf_k == 0 || rrf_k > (1 << 24) {
        return None;
    }
    Some(rrf_k as f32)
}

pub mod alloc;
pub mod cache;
pub mod cold_start;
pub mod consent;
pub mod cosine;
pub mod enums;
pub mod error;
pub mod ids;
pub mod media;
pub mod model;
pub mod security;
pub mod temporal;
pub mod time;
pub mod traits;

pub use alloc::{
    AllocStats, AnnNode, AnnNodePool, Arena, ArenaVec, CALYX_ALLOC_CAP_EXCEEDED, DEFAULT_EMBED_DIM,
    PageAlignedSlabPool, PageSlabGuard, SlabGuard, SlabPool, VecBlockPool,
};
pub use cache::{CALYX_CACHE_EVICTED, InsertResult, LruTtlCache};
pub use cold_start::{CALYX_PROVISIONAL_VAULT, ColdStartGuard, VaultTrustState};
pub use consent::{
    CALYX_CONSENT_VIOLATION, ConsentTag, LawfulBasis, Purpose, Timestamp, check_consent,
    consent_expired,
};
pub use cosine::{
    COSINE_ROUNDING_TOLERANCE, DENSE_COSINE_SCORING_ENGINE, GuardTauProfile, clamp_cosine_quotient,
    dense_cosine,
};
pub use enums::{AbsentReason, AnchorKind, Asymmetry, Modality, QuantPolicy, SlotShape, SlotState};
pub use error::{CALYX_ERROR_CODES, CalyxError, CalyxErrorCode, CalyxWarning, Result};
pub use ids::{CxId, LensId, PanelSlotId, ParseIdError, SlotId, SlotKey, VaultId, content_address};
pub use media::{
    CALYX_MEDIA_ARTIFACT_COLLISION, CALYX_MEDIA_ARTIFACT_INVALID, CALYX_MEDIA_DERIVED_TEXT_FAILED,
    CALYX_MEDIA_DERIVED_TEXT_INVALID, CALYX_MEDIA_DERIVED_TEXT_RUNTIME_MISSING,
    DERIVED_KIND_CAPTION, DERIVED_KIND_TRANSCRIPT, DERIVED_TEXT_MODE,
    LEDGER_FIELD_DERIVED_ARTIFACT_ID, LEDGER_FIELD_DERIVED_KIND, LEDGER_FIELD_MODE,
    LEDGER_FIELD_MODEL, LEDGER_FIELD_MODEL_ID, LEDGER_FIELD_RUNTIME, LEDGER_FIELD_RUNTIME_ID,
    LEDGER_FIELD_SOURCE_CX_ID, LEDGER_FIELD_SOURCE_INPUT_HASH, LEDGER_FIELD_SOURCE_MODALITY,
    LEDGER_FIELD_SOURCE_POINTER, LEDGER_FIELD_SOURCE_SHA256, LEDGER_FIELD_TARGET_CX_ID,
    LEDGER_FIELD_TARGET_POINTER, LEDGER_FIELD_TARGET_TEXT_SHA256, MEDIA_DERIVED_TEXT_ENV,
    METADATA_DERIVED_CONFIDENCE, METADATA_DERIVED_KIND, METADATA_DERIVED_LANGUAGE,
    METADATA_DERIVED_MODEL, METADATA_DERIVED_POINTER, METADATA_DERIVED_RUNTIME,
    METADATA_DERIVED_SOURCE_CX_ID, METADATA_DERIVED_SOURCE_INPUT_HASH,
    METADATA_DERIVED_SOURCE_MODALITY, METADATA_DERIVED_SOURCE_POINTER,
    METADATA_DERIVED_SOURCE_SHA256, METADATA_DERIVED_TEXT_BYTES, METADATA_DERIVED_TEXT_SHA256,
    media_modality_name, required_derived_kind,
};
pub use model::{
    Anchor, AnchorValue, CALYX_RECORD_SCHEMA_VIOLATION, ConfidenceInterval, Constellation, CxFlags,
    InputRef, LedgerRef, LensCost, METADATA_CHUNK_ID, METADATA_DATABASE_NAME,
    METADATA_SOURCE_EVENT_TIME_RAW, METADATA_SOURCE_EVENT_TIME_SECS, METADATA_SOURCE_SEQUENCE,
    METADATA_TEMPORAL_INACTIVE_REASON, METADATA_TEMPORAL_LANE_STATE, Panel, Placement, Signal,
    Slot, SlotResource, SlotVector, SparseEntry, TEMPORAL_LANE_ACTIVE, TEMPORAL_LANE_INACTIVE,
    TEMPORAL_MISSING_CREATED_AT,
};
pub use security::{
    AuthN, CALYX_AUTHN_REQUIRED, CALYX_TLS_CONFIG_INVALID, MtlsConfig, TlsConfig,
    no_anonymous_write,
};
pub use temporal::{
    BoostConfig, CALYX_TEMPORAL_AP60_VIOLATION, CALYX_TEMPORAL_INVALID_BOOST_CONFIG,
    CALYX_TEMPORAL_INVALID_PERIOD, CALYX_TEMPORAL_INVALID_WINDOW, CALYX_TEMPORAL_NEGATIVE_WEIGHT,
    CALYX_TEMPORAL_WEIGHT_SUM, DecayFunction, FusionWeights, MultiAnchorMode, PeriodicOptions,
    RecurrenceBoostConfig, SequenceDirection, SequenceOptions, TemporalPolicy,
};
pub use time::{Clock, FixedClock, Seq, SystemClock, Ts};
pub use traits::{
    Estimator, GroupedLensRequest, Index, Input, Lens, MeasurementGroupKey, VaultStore,
};
