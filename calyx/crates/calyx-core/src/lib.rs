//! Core Calyx identifiers, model contracts, and shared types.

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
pub use cosine::{GuardTauProfile, dense_cosine};
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
