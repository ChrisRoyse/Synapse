//! Loom DDA cross-term and agreement-graph engine.

pub mod abundance;
pub mod agreement_graph;
pub mod blind_spot;
pub mod cross_term;
pub mod error;
pub mod lru_cache;
pub mod materialization;
pub mod reactive;
pub mod recurrence;

pub use abundance::{
    AbundanceReport, CeilingEstimate, NeffEstimate, cross_term_upper_bound, dda_signal_yield,
    meaning_compression_yield,
};
pub use agreement_graph::{
    AgreementEdge, LoomStore, MAX_RECORDED_ZERO_NORM_SKIPS, ZeroNormAgreementSkip,
};
pub use blind_spot::{
    BlindSpotAlert, BlindSpotCalibration, BlindSpotCalibrationEvidence, BlindSpotCalibrationParams,
    MAX_DISCRIMINATIVE_MODAL_SHARE, MIN_DISCRIMINATIVE_DISTINCT, SIMILARITY_DISTINCT_TOLERANCE,
    Severity, SimilarityDiscrimination, detect_blind_spot, detect_blind_spot_calibrated,
};
pub use cross_term::{
    AgreementOutcome, CrossTermKey, CrossTermKind, CrossTermValue, SignalProvenanceTag,
    ZeroNormSide, agreement_batch_cpu, agreement_batch_gpu, agreement_scalar,
    agreement_scalar_classified, agreement_weight, concat_vec, delta_vec, interaction_vec,
};
pub use error::{
    CALYX_LOOM_DIM_MISMATCH, CALYX_LOOM_FORGE_UNAVAILABLE, CALYX_LOOM_NON_FINITE_VECTOR,
    CALYX_LOOM_PANEL_SCOPE_REQUIRED, CALYX_LOOM_SERIES_READ_ERROR, CALYX_LOOM_SLOT_MISSING,
    CALYX_LOOM_TEMPORAL_XTERM_CORRUPT, CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
    CALYX_LOOM_XTERM_SCHEMA_UNSUPPORTED, CALYX_LOOM_ZERO_NORM_VECTOR,
    CALYX_REACTIVE_DRAIN_OVERFLOW, CALYX_REACTIVE_QUEUE_FULL, CALYX_REACTIVE_REGISTRY_FULL,
    CALYX_REACTIVE_ROW_CORRUPT, CALYX_REACTIVE_SIGNAL_UNAVAILABLE,
    CALYX_REACTIVE_SUBSCRIPTION_NOT_FOUND, CALYX_RECURRENCE_CONTEXT_TOO_LARGE,
    CALYX_RECURRENCE_INVALID_RETENTION, loom_error,
};
pub use lru_cache::LruCache;
pub use materialization::{
    MaterializationAction, MaterializationPlan, PairGainGate, StaticPairGainGate, plan_cross_terms,
    plan_cross_terms_checked,
};
pub use reactive::{
    AgreementDriftSignals, AgreementDriftTracker, AuditEntry, AuditLog, BoundedQueue,
    DEFAULT_MAX_DRAIN_BUF, DEFAULT_MAX_SUBSCRIPTIONS, NoveltyVerdict, ReactiveEngine,
    ReactiveRowKey, ReactiveRowKind, ReactiveSignalSet, ReactiveSignals, RecurrenceSignals,
    SubscriptionDelta, SubscriptionHandle, SubscriptionId, SubscriptionStore, TriggerCondition,
    TriggerDef, TriggerFired, TriggerId, TriggerRegistry, WardNoveltySignals, decode_audit_entry,
    decode_trigger_fired, reactive_audit_key, reactive_audit_prefix, reactive_fired_key,
    reactive_row_key,
};
pub use recurrence::{
    LeadLagResult, Occurrence, OccurrenceContext, PeriodicFit, PeriodicRecallHit,
    PeriodicRecallQuery, PeriodicRecallReadback, PeriodicRecallStats, RecurrenceRead,
    RecurrenceReadStats, RecurrenceSeries, RecurrenceSeriesReadback, RetentionPolicy,
    RollupSummary, SeriesStore, SignatureResult, StoredRecurrenceRow, co_occurrence_pairs,
    decode_lead_lag_result, decode_recurrence_row, detect_recurrence_signature,
    encode_lead_lag_result, encode_recurrence_row, lead_lag_secs, periodic_fit,
    periodic_fit_with_tz_offset, periodic_recall, periodic_recall_readback, periodic_time_bucket,
    recurrence_series, recurrence_series_with_tz_offset, recurrence_summary_key,
    temporal_cross_term, temporal_slot_ids_for_panel,
};
