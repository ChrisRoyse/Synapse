//! `calyx-search` — the shared search index + query stack extracted from the CLI
//! (issue #573) so BOTH `calyx` (CLI) and `calyx-web-api` (`/v1/search`) run the
//! exact same Sextant recall → fusion → rerank → provenance path. No mocks, no
//! duplicated logic.
#![deny(warnings)]

pub mod engine;
mod engine_fusion;
mod engine_measure;
mod engine_slot_cache;
mod engine_trace;
pub mod error;
pub mod filters;
pub mod persisted;
mod provenance;

pub use engine::{
    DEFAULT_IN_REGION_GUARD_TAU, EXACT_QUERY_LENS_NOT_EXACT_VALUE_QUERYABLE,
    EXACT_QUERY_SLOT_NOT_ON_PANEL, ExactValueMeasurement, FusionChoice, FusionTuning, GuardChoice,
    MAX_RECONCILED_DELTA_KEYS, PanelDeltaComposition, QUERY_SKIP_LENS_NOT_REGISTERED,
    QUERY_SKIP_LENS_NOT_TEXT_QUERYABLE, QUERY_SKIP_NOT_SELECTED, QUERY_SKIP_RETRIEVAL_ONLY,
    QUERY_SKIP_SLOT_NOT_ACTIVE, QUERY_SKIP_VECTOR_NOT_INDEXABLE, QueryMeasurement, QuerySlotSkip,
    SearchBudget, SearchFreshness, SearchOutcome, SearchSlotCache, SearchSlotCacheDiagnostic,
    SearchTraceEvent, measure_exact_value, measure_panel_delta, measure_query,
    measure_query_vectors, measure_query_vectors_with_slots, search_outcome,
    search_outcome_with_freshness, search_outcome_with_query_vectors,
    search_outcome_with_query_vectors_freshness,
    search_outcome_with_query_vectors_freshness_cached, search_outcome_with_slots,
    search_outcome_with_slots_traced,
};
pub use error::{CliResult, SearchError};
pub use persisted::{
    CandidateSearchGeneration, MarkerClearOutcome, PersistedDenseIndexConfig,
    PersistedDenseQuantization, PersistedSearchGeneration, PersistedSearchIndexes,
    PersistedSearchManifestArtifact, PersistedSearchSlot, REBUILD_REQUIRED_REMEDIATION,
    REBUILD_REQUIRED_SCHEMA, RebuildProgress, RebuildRequiredMarker, clear_rebuild_required_marker,
    clear_rebuild_required_marker_if_owned, load_docs, manifest_path,
    publish_live_manifest_artifact, read_candidate_manifest_artifact, read_live_manifest_artifact,
    read_rebuild_required_marker, rebuild_candidate_for_vault_with_panel_state_and_dense_config,
    rebuild_candidate_for_vault_with_panel_state_and_dense_config_at_snapshot, rebuild_for_vault,
    rebuild_for_vault_with_fallible_progress, rebuild_for_vault_with_panel_state,
    rebuild_for_vault_with_panel_state_and_dense_config,
    rebuild_for_vault_with_panel_state_and_dense_config_at_snapshot,
    rebuild_for_vault_with_panel_state_dense_config_progress,
    rebuild_for_vault_with_panel_state_fallible_progress,
    rebuild_for_vault_with_panel_state_progress, rebuild_for_vault_with_progress,
    rebuild_required_marker_path, validate_rebuild_config, write_rebuild_required_marker,
};
