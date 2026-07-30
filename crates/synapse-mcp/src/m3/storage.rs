use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_core::error_codes;
use synapse_reflex::ReflexRuntime;
use synapse_storage::{
    CalyxAnchorScanReport as BackendCalyxAnchorScanReport,
    CalyxAnchorValueReadback as BackendCalyxAnchorValueReadback,
    CalyxVaultCollectionInspect as BackendCalyxVaultCollectionInspect,
    CalyxVaultInspect as BackendCalyxVaultInspect, DiskPressureLevel, GcReport, PressureReport,
    STORAGE_METADATA_ONLY_REDACTION_POLICY, cf,
};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    audit_retention::{
        AUDIT_RETENTION_MODE, AuditRetentionPolicy, AuditRetentionReport, AuditRetentionRunConfig,
        audit_retention_policies, run_audit_retention, validate_audit_retention_config,
    },
    permissions::{Permission, RequiredPermissions, required},
};

const MAX_PROBE_ROWS: u32 = 10_000;
const MAX_PROBE_VALUE_BYTES: u32 = 65_536;
const MAX_KEY_PREFIX_BYTES: usize = 128;
const MAX_ROW_CAP: u64 = 1_000_000;
const MAX_INSPECT_SAMPLE_ROWS_PER_CF: usize = 3;
const PROBE_WRITABLE_CFS: [&str; cf::ALL_COLUMN_FAMILIES.len()] = cf::ALL_COLUMN_FAMILIES;
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageInspectParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorsParams {
    pub cf_name: String,
    /// Hex-encoded exact source-row key in `cf_name`.
    pub key_hex: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePutProbeRowsParams {
    pub cf_name: String,
    pub key_prefix: String,
    #[schemars(range(min = 0, max = 10000))]
    pub rows: u32,
    #[schemars(range(min = 0, max = 65536))]
    pub value_bytes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ns_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ns_step: Option<u64>,
    /// Key layout: `prefix_index` (default) writes `{prefix}:{index}` string
    /// keys; `timeline_ts` writes the binary `CF_TIMELINE` codec keys
    /// (`ts_ns BE || seq BE`, requires `ts_ns_start`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcOnceParams {
    pub cf_name: String,
    #[schemars(range(min = 1, max = 1_000_000))]
    pub soft_cap_rows: u64,
    #[schemars(range(min = 1, max = 1_000_000))]
    pub hard_cap_rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_window_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageSearchRebuildParams {
    pub expected_panel_version: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageSearchRebuildSlot {
    pub panel_version: u32,
    pub slot_id: u32,
    pub kind: String,
    pub shape: String,
    pub len: usize,
    pub built_at_seq: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageSearchRawSidecar {
    pub path: String,
    pub layout: String,
    pub len_bytes: u64,
    pub file_count: u64,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageSearchRebuildResponse {
    pub panel_version: u32,
    pub base_seq: u64,
    pub before_manifest_sha256: Option<String>,
    pub manifest_sha256: String,
    pub manifest_path: String,
    pub diskann_build_backend: Option<String>,
    pub slots: Vec<StorageSearchRebuildSlot>,
    pub raw_sidecars: Vec<StorageSearchRawSidecar>,
}

// ---------------------------------------------------------------------------
// Fused find-similar search (#1676): per-slot recall -> RRF fusion -> bounded
// temporal boost -> agree/disagree evidence over the persisted per-slot
// indexes. Read-only; runs off the runtime on the blocking pool.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageFindSimilarParams {
    /// `by_example` (a record key -> its own stored slot vectors) or `by_text`
    /// (measured through the active panel's text lenses).
    pub query_mode: String,
    /// Content-addressed example record id; required when `query_mode=by_example`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cx_id: Option<String>,
    /// Query text; required when `query_mode=by_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Maximum fused hits to return.
    #[schemars(range(min = 1, max = 1000))]
    pub k: u32,
    /// Rank-level fusion: `rrf`, `weighted_rrf`, or `single_slot`.
    pub fusion: String,
    /// Slot id to isolate when `fusion=single_slot` (pure vector or pure BM25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 65535))]
    pub single_slot: Option<u32>,
    /// Optional Sextant filter expression (time-range / app).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Attach the per-lens explain breakdown to each hit.
    #[serde(default)]
    pub explain: bool,
    /// Optional bounded temporal post-boost (#1667).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal: Option<StorageFindTemporalParams>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageFindTemporalParams {
    pub query_time_secs: i64,
    pub tz_offset_secs: i32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageFindLensContribution {
    pub slot: u32,
    pub rank: u64,
    pub raw_score: f32,
    pub weight: f32,
    pub contribution: f32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageFindTemporalScores {
    pub e2_recency: f32,
    pub e3_periodic: f32,
    pub e4_sequence: f32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageFindHit {
    pub cx_id: String,
    pub rank: u64,
    pub score: f32,
    pub per_lens: Vec<StorageFindLensContribution>,
    /// Consulted lenses that ranked this record (support).
    pub agree_slots: Vec<u32>,
    /// Consulted lenses that did not rank this record (dissent).
    pub disagree_slots: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_scores: Option<StorageFindTemporalScores>,
    pub provenance_seq: u64,
    pub provenance_hash: String,
    pub freshness_built_at_seq: u64,
    pub freshness_base_seq: u64,
    pub freshness_policy: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageFindGeneration {
    pub panel_version: u32,
    pub base_seq: u64,
    pub manifest_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diskann_build_backend: Option<String>,
    pub slots: Vec<StorageSearchRebuildSlot>,
}

/// Explicit, evidence-backed state of the Ward guarded-search seam (#1677).
///
/// Reported on every fused result so a caller can tell that hits are UNGUARDED
/// rather than assume a guard filtered them. `applied` is derived from what the
/// substrate physically did (operator tau, dropped candidates, per-hit verdicts),
/// never from the mode the daemon requested.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageFindGuard {
    pub requested_mode: String,
    pub applied: bool,
    pub state_code: String,
    pub operator_tau: Option<f32>,
    pub dropped_candidates: u64,
    pub hits_with_guard_verdict: u64,
    pub disabled_reason: String,
    pub enable_requirements: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageFindSimilarResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub fusion: String,
    pub query_kind: String,
    pub k: u32,
    pub rrf_k: u32,
    /// The exact rank-level fusion law, so a caller can recompute every reported
    /// score from the reported per-lens ranks.
    pub rrf_formula: String,
    pub consulted_slots: Vec<u32>,
    pub temporal_applied: bool,
    pub guard: StorageFindGuard,
    pub maxsim_note: String,
    pub grounding_note: String,
    pub generation: StorageFindGeneration,
    pub hits: Vec<StorageFindHit>,
}

// ---------------------------------------------------------------------------
// Orphan physical slot-CF retirement (#1776) as a maintenance-gated facade op,
// gated exactly like search_rebuild (maintenance profile + single admission).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageRetireOrphanSlotCfsParams {}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageRetiredOrphanSlotCf {
    pub slot_id: u32,
    pub quantized_rows: u64,
    pub quantized_sst_files: u64,
    pub raw_rows: u64,
    pub raw_sst_files: u64,
    pub removed_dirs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageSkippedLiveSlotCf {
    pub slot_id: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StorageRetireOrphanSlotCfsResponse {
    pub source_of_truth: &'static str,
    pub base_rows_scanned: u64,
    /// Slot ids referenced by at least one live Base row (the legitimate set).
    pub live_slot_ids: Vec<u32>,
    /// Physical `cf/slot_*` ids discovered on disk.
    pub present_slot_ids: Vec<u32>,
    pub retired: Vec<StorageRetiredOrphanSlotCf>,
    /// Candidate orphans refused because a key still resolved to a live Base
    /// row (fail-closed) — empty on a healthy vault.
    pub skipped_live: Vec<StorageSkippedLiveSlotCf>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageBackupParams {
    /// Absolute path to a fresh, empty target directory. The restorable vault
    /// copy is written to `<target_dir>/vault` and a hashed manifest to
    /// `<target_dir>/backup_manifest.json`.
    pub target_dir: String,
    /// Include the rebuildable `ann/`, `kernel/`, and `guard/` artifacts. False
    /// (default) backs up only sacred data; regenerable artifacts rebuild via
    /// storage operation=search_rebuild after restore.
    #[serde(default)]
    pub include_regenerable: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageRestoreVerifyParams {
    /// Absolute path to a vault directory to verify read-only (a backup's
    /// `vault/` sub-directory, or a restored daemon data dir's vault).
    pub vault_path: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageBackupFile {
    pub relative_path: String,
    pub len_bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageVerifyReport {
    pub vault_path: String,
    pub success: bool,
    pub chain_intact: bool,
    pub constellation_count: u64,
    pub anchor_count: u64,
    pub ledger_entry_count: u64,
    pub ledger_tip_hash: String,
    pub wal_bytes_present: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_cx_id: Option<String>,
    pub failure_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageBackupResponse {
    pub vault_id: String,
    pub source_vault_dir: String,
    pub target_root: String,
    pub backup_vault_dir: String,
    pub manifest_path: String,
    pub manifest_sha256: String,
    pub durable_seq: u64,
    pub latest_seq: u64,
    pub include_regenerable: bool,
    pub file_count: u64,
    pub total_bytes: u64,
    pub residency_enforced: bool,
    pub files: Vec<StorageBackupFile>,
    pub verify: StorageVerifyReport,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageRestoreVerifyResponse {
    pub verify: StorageVerifyReport,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureSampleParams {
    pub free_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalPanelsParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalRerankParams {
    #[schemars(length(min = 1, max = 1000))]
    pub candidates: Vec<StorageTemporalCandidate>,
    pub query_time_secs: i64,
    pub tz_offset_secs: i32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalBackfillParams {
    pub source_cf: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_physical_hex: Option<String>,
    #[schemars(range(min = 1, max = 1000))]
    pub max_rows: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalCandidate {
    pub cx_id: String,
    pub base_score: f32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageInspectResponse {
    pub schema_version: u32,
    pub storage_backend: String,
    pub pressure_level: StoragePressureLevel,
    pub pressure_transition_codes: Vec<String>,
    pub audit_retention_policies: Vec<AuditRetentionPolicy>,
    pub cf_sizes: BTreeMap<String, u64>,
    pub cf_row_counts: BTreeMap<String, u64>,
    pub cf_row_samples: BTreeMap<String, Vec<StorageRowSample>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault: Option<StorageCalyxVaultInspect>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorsResponse {
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_value_len_bytes: u64,
    pub source_value_sha256: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub cx_id: String,
    pub anchor_count: u64,
    pub anchors: Vec<StorageAnchorRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorRow {
    pub key_hex: String,
    pub cx_id: String,
    pub kind: String,
    pub value: StorageAnchorValue,
    pub source: String,
    pub observed_at_ms: u64,
    pub confidence: f32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageAnchorValue {
    pub value_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bool_value: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub one_hot_values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_sha256: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageSummaryResponse {
    pub schema_version: u32,
    pub storage_backend: String,
    pub pressure_level: StoragePressureLevel,
    pub pressure_transition_codes: Vec<String>,
    pub audit_retention_policy_count: usize,
    pub metrics_mode: String,
    pub cf_sizes: BTreeMap<String, u64>,
    pub cf_row_counts: BTreeMap<String, u64>,
    pub missing_cf_size_estimates: Vec<String>,
    pub missing_cf_row_count_estimates: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageRowSample {
    pub key_len_bytes: u64,
    pub key_sha256: String,
    pub key_material_omitted: bool,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub value_encoding: String,
    pub value_content_omitted: bool,
    pub redaction_policy: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageCalyxVaultInspect {
    pub schema_version: u32,
    pub vault_id: String,
    pub latest_seq: u64,
    pub inspected_at_unix_ms: u64,
    pub collection_count: u64,
    pub raw_row_count: u64,
    pub live_row_count: u64,
    pub expired_row_count: u64,
    pub user_key_bytes: u64,
    pub payload_bytes: u64,
    pub stored_value_bytes: u64,
    pub total_logical_bytes: u64,
    pub collections: BTreeMap<String, StorageCalyxVaultCollectionInspect>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageCalyxVaultCollectionInspect {
    pub collection_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_name: Option<String>,
    pub collection_id_hex: String,
    pub namespace: u64,
    pub raw_row_count: u64,
    pub live_row_count: u64,
    pub expired_row_count: u64,
    pub user_key_bytes: u64,
    pub payload_bytes: u64,
    pub stored_value_bytes: u64,
    pub total_logical_bytes: u64,
    pub expires_at_ms_histogram: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePutProbeRowsResponse {
    pub cf_name: String,
    pub key_prefix: String,
    pub requested_rows: u32,
    pub value_bytes: u32,
    pub before_rows: u64,
    pub after_rows: u64,
    pub rows_added: u64,
    pub after_cf_size_bytes: u64,
    pub pressure_level: StoragePressureLevel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcOnceResponse {
    pub cf_name: String,
    pub before_rows: u64,
    pub after_rows: u64,
    pub total_evicted_rows: u64,
    pub cache_evictions_total_delta: u64,
    pub cf_reports: Vec<StorageGcCfReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_retention_report_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_retention: Option<AuditRetentionReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageGcCfReport {
    pub cf_name: String,
    pub before_value: u64,
    pub after_value: u64,
    pub before_estimated_num_keys: Option<u64>,
    pub after_estimated_num_keys: Option<u64>,
    pub examined_rows: u64,
    pub scan_limited: bool,
    pub evicted_rows: u64,
    pub hard_cap_reached: bool,
    pub hard_cap_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eviction_skipped_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureSampleResponse {
    pub report: StoragePressureReport,
    pub pressure_transition_codes: Vec<String>,
    pub cf_row_counts: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureReport {
    pub free_bytes: u64,
    pub previous_level: StoragePressureLevel,
    pub current_level: StoragePressureLevel,
    pub emitted_code: Option<String>,
    pub compacted_cfs: Vec<String>,
    pub gc_advised: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoragePressureLevel {
    pub name: String,
    pub value: u8,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalPanelsResponse {
    pub registry_cf: String,
    pub registration_count: u64,
    pub panels: Vec<StorageTemporalPanel>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalPanel {
    pub schema_version: u16,
    pub panel_name: String,
    pub panel_version: u32,
    pub registered_at_unix_ms: u64,
    pub temporal_slots: Vec<StorageTemporalSlot>,
    pub policy: StorageTemporalPolicy,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalSlot {
    pub name: String,
    pub runtime: String,
    pub output: String,
    pub retrieval_only: bool,
    pub excluded_from_dedup: bool,
    pub required: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalPolicy {
    pub enabled: bool,
    pub never_dominant: bool,
    pub decay: String,
    pub periodic_target_hour: Option<u8>,
    pub periodic_target_day_of_week: Option<u8>,
    pub periodic_use_query_time: bool,
    pub sequence_direction: String,
    pub sequence_multi_anchor_mode: String,
    pub fusion_recency: f32,
    pub fusion_sequence: f32,
    pub fusion_periodic: f32,
    pub post_retrieval_alpha: f32,
    pub recurrence_boost_enabled: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalRerankResponse {
    pub registry_cf: String,
    pub snapshot_seq: u64,
    pub panel_name: String,
    pub panel_version: u32,
    pub panel_registered_at_unix_ms: u64,
    pub query_time_secs: i64,
    pub tz_offset_secs: i32,
    pub temporal_lenses: Vec<String>,
    pub policy: StorageTemporalPolicy,
    pub pre_boost_ranking: Vec<String>,
    pub hits: Vec<StorageTemporalRankedHit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalBackfillResponse {
    pub source_cf: String,
    pub source_scope: String,
    pub examined_rows: u64,
    pub inserted_rows: u64,
    pub backfilled_rows: u64,
    pub already_current_rows: u64,
    pub latest_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_after_physical_hex: Option<String>,
    pub more: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageTemporalRankedHit {
    pub cx_id: String,
    pub event_time_secs: i64,
    pub original_rank: u64,
    pub rank: u64,
    pub base_score: f32,
    pub score: f32,
    pub e2_recency: f32,
    pub e3_periodic: f32,
    pub e4_sequence: f32,
}

/// Upper bound on records the intelligence weave/abundance pass will scan in one
/// bounded, pressure-aware operation.
const MAX_INTELLIGENCE_RECORDS: u32 = 20_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageIntelligenceOperation {
    Weave,
    Abundance,
    Bits,
    Sufficiency,
    Redundancy,
    Synergy,
    Causality,
    Periodicity,
    Drift,
    Hazard,
    Kernel,
    KernelAnswer,
}

impl StorageIntelligenceOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Weave => "weave",
            Self::Abundance => "abundance",
            Self::Bits => "bits",
            Self::Sufficiency => "sufficiency",
            Self::Redundancy => "redundancy",
            Self::Synergy => "synergy",
            Self::Causality => "causality",
            Self::Periodicity => "periodicity",
            Self::Drift => "drift",
            Self::Hazard => "hazard",
            Self::Kernel => "kernel",
            Self::KernelAnswer => "kernel_answer",
        }
    }

    #[must_use]
    pub const fn mutates_state(self) -> bool {
        // Abundance and kernel_answer are pure reads; every other operation
        // persists derived rows (weave: XTerm/Graph; assay: Assay, including
        // synergy's PairGain rows; temporal: Graph/TemporalXTerm; kernel:
        // Kernel CF).
        !matches!(self, Self::Abundance | Self::KernelAnswer)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceParams {
    /// Which native intelligence action to run over the panel corpus.
    pub operation: StorageIntelligenceOperation,
    /// Exact `Syn*` panel version (domain) to weave/report over.
    pub panel_version: u32,
    /// Bounded cap on records scanned; clamped to `[1, 20000]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 20000))]
    pub max_records: Option<u32>,
    /// k for the between-record nearest-neighbor graph (weave only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 64))]
    pub knn_k: Option<u32>,
    /// Inclusive lower bound (Unix nanoseconds) on a record's server-stamped
    /// `created_at` for the `weave` pass. Calyx stamps `created_at` in
    /// milliseconds, so a record is in the window iff
    /// `since_ts_ns <= created_at_ms * 1_000_000 < until_ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ts_ns: Option<i64>,
    /// Exclusive upper bound (Unix nanoseconds) on `created_at` for `weave`.
    /// Must be strictly greater than `since_ts_ns` when both are given; an
    /// inverted window fails closed rather than returning zero records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ts_ns: Option<i64>,
    /// Grounded outcome anchor kind to measure bits about (bits/sufficiency).
    /// Synapse writes outcome anchors as a label string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_kind: Option<String>,
    /// k for the KSG mutual-information estimator (bits/sufficiency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 32))]
    pub ksg_k: Option<u32>,
    /// Metadata key partitioning the panel into activity streams (the
    /// app/agent/tool identifier). Required for `causality`; optional filter
    /// dimension for `periodicity`/`drift`/`hazard`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
    /// Causality source-stream value under `group_key` (defaults to the most
    /// frequent stream when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_a: Option<String>,
    /// Causality target-stream value under `group_key` (defaults to the second
    /// most frequent stream when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_b: Option<String>,
    /// Restricts `periodicity`/`drift`/`hazard` to one `group_key` stream value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_value: Option<String>,
    /// Occurrence-count bin width in seconds (causality/periodicity/drift).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub bin_seconds: Option<f64>,
    /// Maximum transfer-entropy lag in bins (causality only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 64))]
    pub max_lag: Option<u32>,
    /// Reference "now" (Unix seconds) for the overdue-hazard elapsed time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_secs: Option<i64>,
    /// Survival threshold below which the next occurrence is overdue (hazard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overdue_alpha: Option<f64>,
    /// Dense semantic-lens slot id read per concept as the kernel embedding
    /// (`kernel`/`kernel_answer`). Required for those operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_slot: Option<u32>,
    /// Cosine floor for a kernel-graph association edge (`kernel`/`kernel_answer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_cos_threshold: Option<f32>,
    /// Kernel-only recall gate ratio (`kernel`/`kernel_answer`); default ~0.95.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 1))]
    pub min_recall_ratio: Option<f32>,
    /// Existing query record cx_id (32-hex) answered through the kernel
    /// (`kernel_answer`). Required for that operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_cx_id: Option<String>,
    /// Maximum hops walked from an anchored kernel node to the query
    /// (`kernel_answer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 64))]
    pub max_hops: Option<u32>,
}

#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceNeffEstimate {
    pub value: f32,
    pub provisional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_low: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_high: Option<f32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceAbundanceReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub n_lenses: u64,
    pub n_constellations: u64,
    pub c_n2_upper_bound: u64,
    pub materialized: u64,
    pub measured_count: u64,
    pub derived_count: u64,
    pub meaning_compression_yield: f32,
    /// `n * (N + C(N,2) + 1)`: the derived signals this corpus can carry.
    pub dda_signal_yield: u64,
    pub n_eff: StorageIntelligenceNeffEstimate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi_ceiling_bits: Option<f32>,
    pub dpi_ceiling_provisional: bool,
    /// Anchor kind whose persisted Assay bits pass produced the DPI ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi_ceiling_anchor_kind: Option<String>,
    pub xterm_cf_rows: u64,
    pub graph_cf_rows: u64,
}

/// A lens pair that never co-occurs on a measured record, so no cross-term over
/// it can be materialized. Structural coverage gap, distinct from the drift
/// scan's per-record blind-spot alerts.
#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceWeaveBlindSpotPair {
    pub slot_a: u32,
    pub slot_b: u32,
    pub records_with_a: u64,
    pub records_with_b: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceAgreementEdge {
    pub panel_version: u32,
    pub slot_a: u32,
    pub slot_b: u32,
    pub mean_agreement: f32,
    pub agreement_weight: f32,
    pub n: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceWeaveResponse {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub records_scanned: u64,
    pub records_woven: u64,
    pub n_lenses: u64,
    pub cross_terms_materialized: u64,
    pub agreement_edges_persisted: u64,
    pub between_record_edges_persisted: u64,
    pub xterm_cf_rows_after: u64,
    pub graph_cf_rows_after: u64,
    /// Effective half-open `created_at` window, echoed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ts_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ts_ns: Option<i64>,
    /// Panel rows the time window excluded from this pass.
    pub records_outside_window: u64,
    /// `n * (N + C(N,2) + 1)` over the woven corpus.
    pub dda_signal_yield: u64,
    pub lens_pairs_possible: u64,
    pub lens_pairs_co_present: u64,
    pub blind_spot_pairs: u64,
    pub blind_spot_fraction: f32,
    pub blind_spot_records: u64,
    pub blind_spot_slots: Vec<u32>,
    pub blind_spot_pair_details: Vec<StorageIntelligenceWeaveBlindSpotPair>,
    pub agreement_edges: Vec<StorageIntelligenceAgreementEdge>,
    pub abundance: StorageIntelligenceAbundanceReport,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceSlotBits {
    pub slot: u32,
    pub marginal_bits: f32,
    pub ci_low: f32,
    pub ci_high: f32,
    pub n_samples: u64,
    pub sole_carrier: bool,
    pub provisional: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceBitsReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: u64,
    pub distinct_outcomes: u64,
    pub total_bits: f32,
    /// False when the zeros in this report are the absence of a measurement
    /// rather than a measured zero (#1897).
    pub measurable: bool,
    /// Why nothing could be measured, when `measurable` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmeasurable_reason: Option<String>,
    /// Assay sample-count trust tag for this measurement.
    pub grounded: bool,
    /// #1670 control-doctrine marker: the domain's grounded anchor coverage is
    /// below the floor, so this result may only advise, never control.
    pub domain_provisional: bool,
    pub domain_grounded_fraction: f32,
    pub slots: Vec<StorageIntelligenceSlotBits>,
    pub assay_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceSufficiencyDeficit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    pub deficit_bits: f32,
    pub suggested_action: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceSufficiencyReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: u64,
    pub joint_records: u64,
    pub panel_bits: f32,
    pub anchor_entropy_bits: f32,
    pub sufficient: bool,
    pub deficit_bits: f32,
    /// Assay sample-count trust tag for this measurement.
    pub grounded: bool,
    /// #1670 control-doctrine marker: domain anchor coverage below the floor.
    pub domain_provisional: bool,
    pub domain_grounded_fraction: f32,
    pub deficits: Vec<StorageIntelligenceSufficiencyDeficit>,
    pub assay_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceRedundancyPair {
    pub slot_a: u32,
    pub slot_b: u32,
    pub nmi: f32,
    pub mi_bits: f32,
    pub n_samples: u64,
    pub redundant: bool,
}

/// One lens pair the redundancy pass could not measure, named with its reason
/// and the exact offending slot (#1897).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceRedundancySkip {
    pub slot_a: u32,
    pub slot_b: u32,
    pub lens_a: String,
    pub lens_b: String,
    pub reason: String,
    pub offending_slot: Option<u32>,
    pub detail: String,
    pub n_paired: u64,
}

/// A zero-entropy lens found while measuring redundancy: it carries no
/// information about anything and is recommended for parking (#1897).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceLowSignalLens {
    pub slot: u32,
    pub lens: String,
    pub code: String,
    pub constant_value: f32,
    pub records_observed: u64,
    pub distinct_values: u64,
    pub remediation: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceRedundancyReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub n_lenses: u64,
    pub records_scanned: u64,
    pub effective_rank: f32,
    /// `C(n_lenses, 2)`: every pair the panel could in principle offer.
    pub pairs_possible: u64,
    pub pairs_evaluated: u64,
    pub pairs_skipped: u64,
    pub skipped_details: Vec<StorageIntelligenceRedundancySkip>,
    /// The lenses `effective_rank` was actually computed over, named, so the
    /// rank is never read as covering more lenses than it measured.
    pub effective_rank_slots: Vec<u32>,
    pub effective_rank_lenses: Vec<String>,
    pub low_signal_lenses: Vec<StorageIntelligenceLowSignalLens>,
    /// #1670 control-doctrine marker: domain anchor coverage below the floor.
    pub domain_provisional: bool,
    pub domain_grounded_fraction: f32,
    pub redundant_pairs: Vec<StorageIntelligenceRedundancyPair>,
    pub assay_cf_rows_after: u64,
}

/// One measured lens pair from a synergy pass: the joint bits, both marginals
/// over the same records, and `gain = pair_bits - max(left, right)`.
#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceSynergyPair {
    pub slot_a: u32,
    pub slot_b: u32,
    pub pair_bits: f32,
    pub left_bits: f32,
    pub right_bits: f32,
    pub gain_bits: f32,
    pub n_samples: u64,
    pub synergistic: bool,
    /// True when the pair had too few paired samples to measure at all.
    pub provisional: bool,
}

/// Result of one Assay synergy pass with the physical Assay CF readback.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceSynergyReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: u64,
    pub n_lenses: u64,
    /// Lenses paired under the bounded synergy budget (top marginal bits).
    pub lenses_paired: u64,
    pub pairs_evaluated: u64,
    pub synergistic_pairs: u64,
    pub max_gain_bits: f32,
    /// #1670 control-doctrine marker: domain anchor coverage below the floor.
    pub domain_provisional: bool,
    pub domain_grounded_fraction: f32,
    pub pairs: Vec<StorageIntelligenceSynergyPair>,
    pub assay_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceCausalityLag {
    pub lag: u32,
    pub t_a_to_b: f32,
    pub t_b_to_a: f32,
    pub difference_ci_low: f32,
    pub difference_ci_high: f32,
    pub direction: String,
    pub n_samples: u64,
    pub provisional: bool,
    /// Transfer-entropy estimator this lag ran, or `unresolved`.
    pub estimator: String,
    /// Concrete `CALYX_*` failure code for this lag, when it failed.
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceCausalityReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub group_key: String,
    pub group_a: String,
    pub group_b: String,
    pub bin_seconds: f64,
    pub n_bins: u64,
    pub events_a: u64,
    pub events_b: u64,
    pub best_lag: u32,
    pub t_a_to_b: f32,
    pub t_b_to_a: f32,
    pub difference_ci_low: f32,
    pub difference_ci_high: f32,
    pub dominant_direction: String,
    pub grounded: bool,
    /// Transfer-entropy estimator behind `t_a_to_b` / `t_b_to_a`.
    pub estimator: String,
    /// Why that estimator was used. Never a silent choice.
    pub estimator_reason: String,
    pub lags: Vec<StorageIntelligenceCausalityLag>,
    pub graph_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligencePeriodogramPeak {
    pub period_seconds: f64,
    pub frequency: f64,
    pub power: f64,
    pub false_alarm_probability: f64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligencePeriodicityReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_value: Option<String>,
    pub bin_seconds: f64,
    pub n_samples: u64,
    pub time_span_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_period_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_power: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_false_alarm_probability: Option<f64>,
    pub significant: bool,
    pub peaks: Vec<StorageIntelligencePeriodogramPeak>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acf_dominant_period_seconds: Option<f64>,
    pub temporal_xterm_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceDriftReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_value: Option<String>,
    /// Occurrences read from the panel before simultaneous instants were
    /// collapsed. `n_occurrences - n_distinct_instants == ties_collapsed`
    /// (issue #1893).
    pub n_occurrences: u64,
    /// Distinct instants the gap series was actually built over. Inter-event
    /// gaps exist only between distinct instants.
    pub n_distinct_instants: u64,
    /// Occurrences absorbed into an earlier simultaneous instant. Non-zero is
    /// normal for agent events, which are written several per commit.
    pub ties_collapsed: u64,
    /// Largest number of occurrences sharing one instant (1 when none tied).
    pub max_multiplicity: u64,
    pub n_gaps: u64,
    pub baseline_mean_gap: f64,
    pub baseline_sigma: f64,
    pub cusum_change_detected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cusum_change_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cusum_change_time_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cusum_direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cusum_statistic: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmd_split_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmd_p_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmd_significant: Option<bool>,
    pub temporal_xterm_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceHazardReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_value: Option<String>,
    /// Occurrences read from the panel before simultaneous instants were
    /// collapsed. `n_occurrences - n_distinct_instants == ties_collapsed`
    /// (issue #1893).
    pub n_occurrences: u64,
    /// Distinct instants the gap series was actually built over. Inter-event
    /// gaps exist only between distinct instants.
    pub n_distinct_instants: u64,
    /// Occurrences absorbed into an earlier simultaneous instant. Non-zero is
    /// normal for agent events, which are written several per commit.
    pub ties_collapsed: u64,
    /// Largest number of occurrences sharing one instant (1 when none tied).
    pub max_multiplicity: u64,
    pub n_gaps: u64,
    pub mean_gap_seconds: f64,
    pub coefficient_of_variation: f64,
    pub deterministic: bool,
    pub elapsed_seconds: f64,
    pub survival: f64,
    pub hazard: f64,
    pub empirical_survival: f64,
    pub expected_next_seconds: f64,
    pub overdue_threshold_seconds: f64,
    pub alpha: f64,
    pub overdue: bool,
    pub temporal_xterm_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceKernelReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub content_slot: u32,
    pub kernel_id: String,
    pub corpus_fingerprint: String,
    pub members: u64,
    pub kernel_graph_nodes: u64,
    pub corpus_size: u64,
    pub vault_corpus_size: u64,
    pub recall_kernel_only: f32,
    pub recall_ratio: f32,
    pub min_recall_ratio: f32,
    pub grounded: bool,
    pub reached_anchor: f32,
    pub unanchored_members: u64,
    pub anchored_members: u64,
    pub member_cx_ids: Vec<String>,
    pub kernel_cf_rows_after: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceKernelAnswerHop {
    pub from: String,
    pub to: String,
    pub edge_weight: f32,
    pub hop_index: u32,
    pub hop_score: f32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceKernelAnswerReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub content_slot: u32,
    pub query_cx_id: String,
    pub grounded: bool,
    pub kernel_id: String,
    pub anchor_kernel_node: String,
    pub total_score: f32,
    pub hop_count: u64,
    pub hops: Vec<StorageIntelligenceKernelAnswerHop>,
    pub kernel_members: u64,
    pub recall_ratio: f32,
    pub min_recall_ratio: f32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceResponse {
    pub operation: StorageIntelligenceOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weave: Option<StorageIntelligenceWeaveResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abundance: Option<StorageIntelligenceAbundanceReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<StorageIntelligenceBitsReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sufficiency: Option<StorageIntelligenceSufficiencyReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<StorageIntelligenceRedundancyReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synergy: Option<StorageIntelligenceSynergyReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causality: Option<StorageIntelligenceCausalityReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub periodicity: Option<StorageIntelligencePeriodicityReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift: Option<StorageIntelligenceDriftReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hazard: Option<StorageIntelligenceHazardReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<StorageIntelligenceKernelReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_answer: Option<StorageIntelligenceKernelAnswerReport>,
}

#[must_use]
pub const fn storage_inspect() -> M3ToolStub {
    M3ToolStub::new("storage_inspect")
}

#[must_use]
pub const fn storage_put_probe_rows() -> M3ToolStub {
    M3ToolStub::new("storage_put_probe_rows")
}

#[must_use]
pub const fn storage_gc_once() -> M3ToolStub {
    M3ToolStub::new("storage_gc_once")
}

#[must_use]
pub const fn storage_pressure_sample() -> M3ToolStub {
    M3ToolStub::new("storage_pressure_sample")
}

#[must_use]
pub fn required_permissions_inspect(_params: &StorageInspectParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_anchors(_params: &StorageAnchorsParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_put(_params: &StoragePutProbeRowsParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_gc(_params: &StorageGcOnceParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_search_rebuild(
    _params: &StorageSearchRebuildParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_find_similar(
    _params: &StorageFindSimilarParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_retire_orphan_slot_cfs(
    _params: &StorageRetireOrphanSlotCfsParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

/// Runs one fused find-similar pass and maps the physical Calyx report onto the
/// MCP response. Fusion / query-mode strings are validated here; the heavy
/// index work happens in the caller's blocking-pool offload.
///
/// # Errors
///
/// Returns a structured MCP error for an invalid query mode / fusion strategy,
/// a missing required field, or any fail-closed Calyx find error (missing/stale
/// index, cross-panel example, temporal boost unavailable).
pub fn run_find_similar(
    db: &synapse_storage::Db,
    params: &StorageFindSimilarParams,
) -> Result<StorageFindSimilarResponse, ErrorData> {
    let query = match params.query_mode.trim() {
        "by_example" => {
            let cx_id = params
                .cx_id
                .clone()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "storage operation=find_similar query_mode=by_example requires a non-empty cx_id".to_owned(),
                    )
                })?;
            synapse_calyx::SynapseCalyxFindQuery::ByExample { cx_id }
        }
        "by_text" => {
            let text = params
                .text
                .clone()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "storage operation=find_similar query_mode=by_text requires non-empty text"
                            .to_owned(),
                    )
                })?;
            synapse_calyx::SynapseCalyxFindQuery::ByText { text }
        }
        other => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage operation=find_similar query_mode {other:?} must be by_example or by_text"
                ),
            ));
        }
    };
    let fusion = match params.fusion.trim() {
        "rrf" => synapse_calyx::SynapseCalyxFindFusion::Rrf,
        "weighted_rrf" => synapse_calyx::SynapseCalyxFindFusion::WeightedRrf,
        "single_slot" => {
            let slot = params.single_slot.ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage operation=find_similar fusion=single_slot requires single_slot"
                        .to_owned(),
                )
            })?;
            let slot = u16::try_from(slot).map_err(|_| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("storage operation=find_similar single_slot {slot} exceeds the u16 slot id range"),
                )
            })?;
            synapse_calyx::SynapseCalyxFindFusion::SingleSlot { slot }
        }
        other => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage operation=find_similar fusion {other:?} must be rrf, weighted_rrf, or single_slot"
                ),
            ));
        }
    };
    let temporal =
        params
            .temporal
            .as_ref()
            .map(|temporal| synapse_calyx::SynapseCalyxFindTemporal {
                query_time_secs: temporal.query_time_secs,
                tz_offset_secs: temporal.tz_offset_secs,
            });
    let find_params = synapse_calyx::SynapseCalyxFindParams {
        query,
        k: params.k as usize,
        fusion,
        filter: params.filter.clone(),
        explain: params.explain,
        temporal,
    };
    let report = db
        .find_similar(&find_params)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(storage_find_similar_response(report))
}

fn storage_find_similar_response(
    report: synapse_calyx::SynapseCalyxFindReport,
) -> StorageFindSimilarResponse {
    let generation = StorageFindGeneration {
        panel_version: report.generation.panel_version,
        base_seq: report.generation.base_seq,
        manifest_sha256: report.generation.manifest_sha256,
        diskann_build_backend: report.generation.diskann_build_backend,
        slots: report
            .generation
            .slots
            .into_iter()
            .map(|slot| StorageSearchRebuildSlot {
                panel_version: slot.panel_slot.panel_version(),
                slot_id: u32::from(slot.panel_slot.slot_id().get()),
                kind: slot.kind,
                shape: format!("{:?}", slot.shape),
                len: slot.len,
                built_at_seq: slot.built_at_seq,
            })
            .collect(),
    };
    let hits = report
        .hits
        .into_iter()
        .map(|hit| StorageFindHit {
            cx_id: hit.cx_id,
            rank: hit.rank as u64,
            score: hit.score,
            per_lens: hit
                .per_lens
                .into_iter()
                .map(|lens| StorageFindLensContribution {
                    slot: u32::from(lens.slot),
                    rank: lens.rank as u64,
                    raw_score: lens.raw_score,
                    weight: lens.weight,
                    contribution: lens.contribution,
                })
                .collect(),
            agree_slots: hit.agree_slots.into_iter().map(u32::from).collect(),
            disagree_slots: hit.disagree_slots.into_iter().map(u32::from).collect(),
            event_time_secs: hit.event_time_secs,
            temporal_scores: hit.temporal_scores.map(|scores| StorageFindTemporalScores {
                e2_recency: scores.e2_recency,
                e3_periodic: scores.e3_periodic,
                e4_sequence: scores.e4_sequence,
            }),
            provenance_seq: hit.provenance_seq,
            provenance_hash: hit.provenance_hash,
            freshness_built_at_seq: hit.freshness_built_at_seq,
            freshness_base_seq: hit.freshness_base_seq,
            freshness_policy: hit.freshness_policy,
        })
        .collect();
    StorageFindSimilarResponse {
        source_of_truth: "calyx_vault",
        panel_version: report.panel_version,
        fusion: report.fusion,
        query_kind: report.query_kind,
        k: report.k as u32,
        rrf_k: report.rrf_k,
        rrf_formula: report.rrf_formula,
        consulted_slots: report.consulted_slots.into_iter().map(u32::from).collect(),
        temporal_applied: report.temporal_applied,
        guard: StorageFindGuard {
            requested_mode: report.guard.requested_mode,
            applied: report.guard.applied,
            state_code: report.guard.state_code,
            operator_tau: report.guard.operator_tau,
            dropped_candidates: report.guard.dropped_candidates as u64,
            hits_with_guard_verdict: report.guard.hits_with_guard_verdict as u64,
            disabled_reason: report.guard.disabled_reason,
            enable_requirements: report.guard.enable_requirements,
        },
        maxsim_note: report.maxsim_note,
        grounding_note: report.grounding_note,
        generation,
        hits,
    }
}

/// Maps a physical orphan slot-CF retirement report onto the MCP response.
#[must_use]
pub fn storage_orphan_slot_gc_response(
    report: synapse_calyx::AsterOrphanSlotGcReport,
) -> StorageRetireOrphanSlotCfsResponse {
    StorageRetireOrphanSlotCfsResponse {
        source_of_truth: "calyx_vault",
        base_rows_scanned: report.base_rows_scanned as u64,
        live_slot_ids: report.live_slot_ids.into_iter().map(u32::from).collect(),
        present_slot_ids: report.present_slot_ids.into_iter().map(u32::from).collect(),
        retired: report
            .retired
            .into_iter()
            .map(|retired| StorageRetiredOrphanSlotCf {
                slot_id: u32::from(retired.slot_id),
                quantized_rows: retired.quantized_rows as u64,
                quantized_sst_files: retired.quantized_sst_files as u64,
                raw_rows: retired.raw_rows as u64,
                raw_sst_files: retired.raw_sst_files as u64,
                removed_dirs: retired.removed_dirs,
            })
            .collect(),
        skipped_live: report
            .skipped_live
            .into_iter()
            .map(|skip| StorageSkippedLiveSlotCf {
                slot_id: u32::from(skip.slot_id),
                reason: skip.reason,
            })
            .collect(),
    }
}

#[must_use]
pub fn required_permissions_backup(_params: &StorageBackupParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_restore_verify(
    _params: &StorageRestoreVerifyParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_pressure(_params: &StoragePressureSampleParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_temporal_panels(
    _params: &StorageTemporalPanelsParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_temporal_rerank(
    _params: &StorageTemporalRerankParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_temporal_backfill(
    _params: &StorageTemporalBackfillParams,
) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

pub fn inspect_storage(
    db: &synapse_storage::Db,
    _params: &StorageInspectParams,
) -> Result<StorageInspectResponse, ErrorData> {
    inspect_db(db)
}

pub fn inspect_storage_anchors(
    db: &synapse_storage::Db,
    params: &StorageAnchorsParams,
) -> Result<StorageAnchorsResponse, ErrorData> {
    let key = hex_decode(params.key_hex.trim()).map_err(|detail| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage operation=anchors key_hex invalid: {detail}"),
        )
    })?;
    let cf_name = known_anchor_source_cf_for_key(&params.cf_name, &key)?;
    let source_value = db
        .get_cf(cf_name, &key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some(source_value) = source_value else {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "storage operation=anchors source row not found: cf_name={cf_name} key_hex={}",
                params.key_hex.trim()
            ),
        ));
    };
    let source_value_len_bytes = u64::try_from(source_value.len()).unwrap_or(u64::MAX);
    let report = db
        .calyx_anchor_scan_for_source(cf_name, &key, &source_value)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(storage_anchors_response(report, source_value_len_bytes))
}

pub fn inspect_temporal_panels(
    db: &synapse_storage::Db,
    _params: &StorageTemporalPanelsParams,
) -> Result<StorageTemporalPanelsResponse, ErrorData> {
    let registrations = db
        .list_temporal_panels()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageTemporalPanelsResponse {
        registry_cf: "registry".to_owned(),
        registration_count: registrations.len() as u64,
        panels: registrations
            .into_iter()
            .map(storage_temporal_panel)
            .collect(),
    })
}

pub fn run_temporal_rerank(
    db: &synapse_storage::Db,
    params: &StorageTemporalRerankParams,
) -> Result<StorageTemporalRerankResponse, ErrorData> {
    let candidates = params
        .candidates
        .iter()
        .map(|candidate| synapse_calyx::SynapseCalyxTemporalCandidate {
            cx_id: candidate.cx_id.clone(),
            base_score: candidate.base_score,
        })
        .collect::<Vec<_>>();
    let readback = db
        .temporal_rerank(&candidates, params.query_time_secs, params.tz_offset_secs)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageTemporalRerankResponse {
        registry_cf: "registry".to_owned(),
        snapshot_seq: readback.snapshot_seq,
        panel_name: readback.panel_name,
        panel_version: readback.panel_version,
        panel_registered_at_unix_ms: readback.panel_registered_at_unix_ms,
        query_time_secs: readback.query_time_secs,
        tz_offset_secs: readback.tz_offset_secs,
        temporal_lenses: readback.temporal_lenses,
        policy: storage_temporal_policy(&readback.policy),
        pre_boost_ranking: readback.pre_boost_ranking,
        hits: readback
            .hits
            .into_iter()
            .map(|hit| StorageTemporalRankedHit {
                cx_id: hit.cx_id,
                event_time_secs: hit.event_time_secs,
                original_rank: hit.original_rank as u64,
                rank: hit.rank as u64,
                base_score: hit.base_score,
                score: hit.score,
                e2_recency: hit.temporal_scores.e2_recency,
                e3_periodic: hit.temporal_scores.e3_periodic,
                e4_sequence: hit.temporal_scores.e4_sequence,
            })
            .collect(),
    })
}

pub fn run_temporal_backfill(
    db: &synapse_storage::Db,
    params: &StorageTemporalBackfillParams,
) -> Result<StorageTemporalBackfillResponse, ErrorData> {
    let key = params
        .key_hex
        .as_deref()
        .map(str::trim)
        .map(hex_decode)
        .transpose()
        .map_err(|detail| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("storage operation=temporal_backfill key_hex invalid: {detail}"),
            )
        })?;
    let after_physical = params
        .after_physical_hex
        .as_deref()
        .map(str::trim)
        .map(hex_decode)
        .transpose()
        .map_err(|detail| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("storage operation=temporal_backfill after_physical_hex invalid: {detail}"),
            )
        })?;
    let report = db
        .backfill_temporal_metadata(
            params.source_cf.trim(),
            key.as_deref(),
            after_physical.as_deref(),
            params.max_rows as usize,
        )
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageTemporalBackfillResponse {
        source_cf: report.source_cf,
        source_scope: key
            .map(|key| format!("key_hex={}", hex_encode(&key)))
            .unwrap_or_else(|| "all_rows".to_owned()),
        examined_rows: report.examined_rows,
        inserted_rows: report.inserted_rows,
        backfilled_rows: report.backfilled_rows,
        already_current_rows: report.already_current_rows,
        latest_seq: report.latest_seq,
        resume_after_physical_hex: report.resume_after_physical.as_deref().map(hex_encode),
        more: report.more,
    })
}

#[must_use]
pub fn required_permissions_intelligence(
    params: &StorageIntelligenceParams,
) -> RequiredPermissions {
    if params.operation.mutates_state() {
        required([Permission::ReadStorage, Permission::WriteStorage])
    } else {
        required([Permission::ReadStorage])
    }
}

/// Runs one native Loom weave pass over a panel (mutating: persists derived
/// `XTerm`/`Graph` rows) and returns the physical CF readbacks.
pub fn run_intelligence_weave(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceWeaveResponse, ErrorData> {
    let max_records = clamp_intelligence_records(params.max_records);
    let mut weave = synapse_calyx::SynapseCalyxWeaveParams::new(params.panel_version);
    weave.max_records = max_records;
    if let Some(knn_k) = params.knn_k {
        weave.knn_k = knn_k as usize;
    }
    weave.since_ts_ns = params.since_ts_ns;
    weave.until_ts_ns = params.until_ts_ns;
    let report = db
        .weave_panel_intelligence(weave)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceWeaveResponse {
        source_of_truth: "Calyx XTerm + Graph CF rows",
        panel_version: report.panel_version,
        records_scanned: report.records_scanned as u64,
        records_woven: report.records_woven as u64,
        n_lenses: report.n_lenses as u64,
        cross_terms_materialized: report.cross_terms_materialized as u64,
        agreement_edges_persisted: report.agreement_edges_persisted as u64,
        between_record_edges_persisted: report.between_record_edges_persisted as u64,
        xterm_cf_rows_after: report.xterm_cf_rows_after as u64,
        graph_cf_rows_after: report.graph_cf_rows_after as u64,
        since_ts_ns: report.since_ts_ns,
        until_ts_ns: report.until_ts_ns,
        records_outside_window: report.records_outside_window as u64,
        dda_signal_yield: report.dda_signal_yield as u64,
        lens_pairs_possible: report.lens_pairs_possible as u64,
        lens_pairs_co_present: report.lens_pairs_co_present as u64,
        blind_spot_pairs: report.blind_spot_pairs as u64,
        blind_spot_fraction: report.blind_spot_fraction,
        blind_spot_records: report.blind_spot_records as u64,
        blind_spot_slots: report.blind_spot_slots.into_iter().map(u32::from).collect(),
        blind_spot_pair_details: report
            .blind_spot_pair_details
            .into_iter()
            .map(|pair| StorageIntelligenceWeaveBlindSpotPair {
                slot_a: u32::from(pair.slot_a),
                slot_b: u32::from(pair.slot_b),
                records_with_a: pair.records_with_a as u64,
                records_with_b: pair.records_with_b as u64,
            })
            .collect(),
        agreement_edges: report
            .agreement_edges
            .into_iter()
            .map(storage_intelligence_agreement_edge)
            .collect(),
        abundance: storage_intelligence_abundance(report.abundance),
    })
}

/// Reads the derived-data abundance report for a panel back from the physical
/// `Base`/`XTerm`/`Graph` CFs.
pub fn run_intelligence_abundance(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceAbundanceReport, ErrorData> {
    let max_records = clamp_intelligence_records(params.max_records);
    let report = db
        .abundance_report_intelligence(params.panel_version, max_records)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(storage_intelligence_abundance(report))
}

fn clamp_intelligence_records(requested: Option<u32>) -> usize {
    requested
        .unwrap_or(MAX_INTELLIGENCE_RECORDS)
        .clamp(1, MAX_INTELLIGENCE_RECORDS) as usize
}

fn assay_params(
    params: &StorageIntelligenceParams,
) -> Result<synapse_calyx::SynapseCalyxAssayParams, ErrorData> {
    let anchor_kind = params
        .anchor_kind
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage operation=intelligence sub_operation={} requires a non-empty anchor_kind",
                    params.operation.as_str()
                ),
            )
        })?;
    let mut assay =
        synapse_calyx::SynapseCalyxAssayParams::new(params.panel_version, anchor_kind.to_owned())
            .with_lens_names(synapse_storage::constellations::syn_slot_lens_names());
    assay.max_records = clamp_intelligence_records(params.max_records);
    if let Some(ksg_k) = params.ksg_k {
        assay.ksg_k = ksg_k as usize;
    }
    Ok(assay)
}

/// Measures grounded bits per lens about one outcome anchor and persists the
/// Assay CF rows.
pub fn run_intelligence_bits(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceBitsReport, ErrorData> {
    let report = db
        .assay_bits_intelligence(&assay_params(params)?)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceBitsReport {
        source_of_truth: "Calyx Assay CF rows",
        panel_version: report.panel_version,
        anchor_kind: report.anchor_kind,
        anchored_records: report.anchored_records as u64,
        distinct_outcomes: report.distinct_outcomes as u64,
        total_bits: report.total_bits,
        measurable: report.measurable,
        unmeasurable_reason: report.unmeasurable_reason,
        grounded: report.grounded,
        domain_provisional: report.domain_provisional,
        domain_grounded_fraction: report.domain_grounded_fraction,
        slots: report
            .slots
            .into_iter()
            .map(|slot| StorageIntelligenceSlotBits {
                slot: u32::from(slot.slot),
                marginal_bits: slot.marginal_bits,
                ci_low: slot.ci_low,
                ci_high: slot.ci_high,
                n_samples: slot.n_samples as u64,
                sole_carrier: slot.sole_carrier,
                provisional: slot.provisional,
            })
            .collect(),
        assay_cf_rows_after: report.assay_cf_rows_after as u64,
    })
}

/// Tests panel sufficiency and persists the panel/outcome-entropy Assay rows.
pub fn run_intelligence_sufficiency(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceSufficiencyReport, ErrorData> {
    let report = db
        .assay_sufficiency_intelligence(&assay_params(params)?)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceSufficiencyReport {
        source_of_truth: "Calyx Assay CF rows",
        panel_version: report.panel_version,
        anchor_kind: report.anchor_kind,
        anchored_records: report.anchored_records as u64,
        joint_records: report.joint_records as u64,
        panel_bits: report.panel_bits,
        anchor_entropy_bits: report.anchor_entropy_bits,
        sufficient: report.sufficient,
        deficit_bits: report.deficit_bits,
        grounded: report.grounded,
        domain_provisional: report.domain_provisional,
        domain_grounded_fraction: report.domain_grounded_fraction,
        deficits: report
            .deficits
            .into_iter()
            .map(|deficit| StorageIntelligenceSufficiencyDeficit {
                slot: deficit.slot.map(u32::from),
                deficit_bits: deficit.deficit_bits,
                suggested_action: deficit.suggested_action,
                reason: deficit.reason,
            })
            .collect(),
        assay_cf_rows_after: report.assay_cf_rows_after as u64,
    })
}

/// Measures pairwise lens redundancy + effective rank and persists redundant
/// Assay pairs.
pub fn run_intelligence_redundancy(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceRedundancyReport, ErrorData> {
    let mut assay = synapse_calyx::SynapseCalyxAssayParams::new(params.panel_version, String::new())
        .with_lens_names(synapse_storage::constellations::syn_slot_lens_names());
    assay.max_records = clamp_intelligence_records(params.max_records);
    let report = db
        .assay_redundancy_intelligence(&assay)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceRedundancyReport {
        source_of_truth: "Calyx Assay CF rows",
        panel_version: report.panel_version,
        n_lenses: report.n_lenses as u64,
        records_scanned: report.records_scanned as u64,
        effective_rank: report.effective_rank,
        pairs_possible: report.pairs_possible as u64,
        pairs_evaluated: report.pairs_evaluated as u64,
        pairs_skipped: report.pairs_skipped as u64,
        skipped_details: report
            .skipped_details
            .into_iter()
            .map(|skip| StorageIntelligenceRedundancySkip {
                slot_a: u32::from(skip.slot_a),
                slot_b: u32::from(skip.slot_b),
                lens_a: skip.lens_a,
                lens_b: skip.lens_b,
                reason: skip.reason,
                offending_slot: skip.offending_slot.map(u32::from),
                detail: skip.detail,
                n_paired: skip.n_paired as u64,
            })
            .collect(),
        effective_rank_slots: report
            .effective_rank_slots
            .into_iter()
            .map(u32::from)
            .collect(),
        effective_rank_lenses: report.effective_rank_lenses,
        low_signal_lenses: report
            .low_signal_lenses
            .into_iter()
            .map(|lens| StorageIntelligenceLowSignalLens {
                slot: u32::from(lens.slot),
                lens: lens.lens,
                code: lens.code,
                constant_value: lens.constant_value,
                records_observed: lens.records_observed as u64,
                distinct_values: lens.distinct_values as u64,
                remediation: lens.remediation,
            })
            .collect(),
        domain_provisional: report.domain_provisional,
        domain_grounded_fraction: report.domain_grounded_fraction,
        redundant_pairs: report
            .redundant_pairs
            .into_iter()
            .map(|pair| StorageIntelligenceRedundancyPair {
                slot_a: u32::from(pair.slot_a),
                slot_b: u32::from(pair.slot_b),
                nmi: pair.nmi,
                mi_bits: pair.mi_bits,
                n_samples: pair.n_samples as u64,
                redundant: pair.redundant,
            })
            .collect(),
        assay_cf_rows_after: report.assay_cf_rows_after as u64,
    })
}

/// Measures pairwise lens synergy about one outcome anchor and persists the
/// synergistic pairs as `PairGain` Assay rows.
pub fn run_intelligence_synergy(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceSynergyReport, ErrorData> {
    let report = db
        .assay_synergy_intelligence(&assay_params(params)?)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceSynergyReport {
        source_of_truth: "Calyx Assay CF rows",
        panel_version: report.panel_version,
        anchor_kind: report.anchor_kind,
        anchored_records: report.anchored_records as u64,
        n_lenses: report.n_lenses as u64,
        lenses_paired: report.lenses_paired as u64,
        pairs_evaluated: report.pairs_evaluated as u64,
        synergistic_pairs: report.synergistic_pairs as u64,
        max_gain_bits: report.max_gain_bits,
        domain_provisional: report.domain_provisional,
        domain_grounded_fraction: report.domain_grounded_fraction,
        pairs: report
            .pairs
            .into_iter()
            .map(|pair| StorageIntelligenceSynergyPair {
                slot_a: u32::from(pair.slot_a),
                slot_b: u32::from(pair.slot_b),
                pair_bits: pair.pair_bits,
                left_bits: pair.left_bits,
                right_bits: pair.right_bits,
                gain_bits: pair.gain_bits,
                n_samples: pair.n_samples as u64,
                synergistic: pair.synergistic,
                provisional: pair.provisional,
            })
            .collect(),
        assay_cf_rows_after: report.assay_cf_rows_after as u64,
    })
}

fn temporal_params(
    params: &StorageIntelligenceParams,
) -> synapse_calyx::SynapseCalyxTemporalParams {
    let mut temporal = synapse_calyx::SynapseCalyxTemporalParams::new(params.panel_version);
    temporal.max_records = clamp_intelligence_records(params.max_records);
    temporal.group_key = params.group_key.clone();
    temporal.group_a = params.group_a.clone();
    temporal.group_b = params.group_b.clone();
    temporal.filter_value = params.filter_value.clone();
    if let Some(bin) = params.bin_seconds {
        temporal.bin_seconds = bin;
    }
    if let Some(lag) = params.max_lag {
        temporal.max_lag = lag as usize;
    }
    temporal.now_secs = params.now_secs;
    if let Some(alpha) = params.overdue_alpha {
        temporal.overdue_alpha = alpha;
    }
    temporal
}

/// Measures directed transfer entropy between two activity streams and persists
/// the dominant directed edge to the native Graph CF.
pub fn run_intelligence_causality(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceCausalityReport, ErrorData> {
    let report = db
        .temporal_causality_intelligence(&temporal_params(params))
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceCausalityReport {
        source_of_truth: "Calyx Graph CF rows",
        panel_version: report.panel_version,
        group_key: report.group_key,
        group_a: report.group_a,
        group_b: report.group_b,
        bin_seconds: report.bin_seconds,
        n_bins: report.n_bins as u64,
        events_a: report.events_a as u64,
        events_b: report.events_b as u64,
        best_lag: report.best_lag as u32,
        t_a_to_b: report.t_a_to_b,
        t_b_to_a: report.t_b_to_a,
        difference_ci_low: report.difference_ci_low,
        difference_ci_high: report.difference_ci_high,
        dominant_direction: report.dominant_direction,
        grounded: report.grounded,
        estimator: report.estimator,
        estimator_reason: report.estimator_reason,
        lags: report
            .lags
            .into_iter()
            .map(|lag| StorageIntelligenceCausalityLag {
                lag: lag.lag as u32,
                t_a_to_b: lag.t_a_to_b,
                t_b_to_a: lag.t_b_to_a,
                difference_ci_low: lag.difference_ci_low,
                difference_ci_high: lag.difference_ci_high,
                direction: lag.direction,
                n_samples: lag.n_samples as u64,
                provisional: lag.provisional,
                estimator: lag.estimator,
                error_code: lag.error_code,
            })
            .collect(),
        graph_cf_rows_after: report.graph_cf_rows_after as u64,
    })
}

/// Runs the Lomb-Scargle periodogram + slotted-autocorrelation cross-check and
/// persists the result to the native TemporalXTerm CF.
pub fn run_intelligence_periodicity(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligencePeriodicityReport, ErrorData> {
    let report = db
        .temporal_periodicity_intelligence(&temporal_params(params))
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligencePeriodicityReport {
        source_of_truth: "Calyx TemporalXTerm CF rows",
        panel_version: report.panel_version,
        filter_value: report.filter_value,
        bin_seconds: report.bin_seconds,
        n_samples: report.n_samples as u64,
        time_span_seconds: report.time_span_seconds,
        dominant_period_seconds: report.dominant_period_seconds,
        dominant_power: report.dominant_power,
        dominant_false_alarm_probability: report.dominant_false_alarm_probability,
        significant: report.significant,
        peaks: report
            .peaks
            .into_iter()
            .map(|peak| StorageIntelligencePeriodogramPeak {
                period_seconds: peak.period_seconds,
                frequency: peak.frequency,
                power: peak.power,
                false_alarm_probability: peak.false_alarm_probability,
            })
            .collect(),
        acf_dominant_period_seconds: report.acf_dominant_period_seconds,
        temporal_xterm_cf_rows_after: report.temporal_xterm_cf_rows_after as u64,
    })
}

/// Detects recurrence-rate change (CUSUM) and distribution drift (MMD) and
/// persists the result to the native TemporalXTerm CF.
pub fn run_intelligence_drift(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceDriftReport, ErrorData> {
    let report = db
        .temporal_drift_intelligence(&temporal_params(params))
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceDriftReport {
        source_of_truth: "Calyx TemporalXTerm CF rows",
        panel_version: report.panel_version,
        filter_value: report.filter_value,
        n_occurrences: report.n_occurrences as u64,
        n_distinct_instants: report.n_distinct_instants as u64,
        ties_collapsed: report.ties_collapsed as u64,
        max_multiplicity: report.max_multiplicity as u64,
        n_gaps: report.n_gaps as u64,
        baseline_mean_gap: report.baseline_mean_gap,
        baseline_sigma: report.baseline_sigma,
        cusum_change_detected: report.cusum_change_detected,
        cusum_change_index: report.cusum_change_index.map(|index| index as u64),
        cusum_change_time_seconds: report.cusum_change_time_seconds,
        cusum_direction: report.cusum_direction,
        cusum_statistic: report.cusum_statistic,
        mmd_split_index: report.mmd_split_index.map(|index| index as u64),
        mmd_p_value: report.mmd_p_value,
        mmd_significant: report.mmd_significant,
        temporal_xterm_cf_rows_after: report.temporal_xterm_cf_rows_after as u64,
    })
}

/// Fits the Gamma-renewal inter-event overdue hazard and persists the result to
/// the native TemporalXTerm CF.
pub fn run_intelligence_hazard(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceHazardReport, ErrorData> {
    let report = db
        .temporal_hazard_intelligence(&temporal_params(params))
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceHazardReport {
        source_of_truth: "Calyx TemporalXTerm CF rows",
        panel_version: report.panel_version,
        filter_value: report.filter_value,
        n_occurrences: report.n_occurrences as u64,
        n_distinct_instants: report.n_distinct_instants as u64,
        ties_collapsed: report.ties_collapsed as u64,
        max_multiplicity: report.max_multiplicity as u64,
        n_gaps: report.n_gaps as u64,
        mean_gap_seconds: report.mean_gap_seconds,
        coefficient_of_variation: report.coefficient_of_variation,
        deterministic: report.deterministic,
        elapsed_seconds: report.elapsed_seconds,
        survival: report.survival,
        hazard: report.hazard,
        empirical_survival: report.empirical_survival,
        expected_next_seconds: report.expected_next_seconds,
        overdue_threshold_seconds: report.overdue_threshold_seconds,
        alpha: report.alpha,
        overdue: report.overdue,
        temporal_xterm_cf_rows_after: report.temporal_xterm_cf_rows_after as u64,
    })
}

fn kernel_params(
    params: &StorageIntelligenceParams,
) -> Result<synapse_calyx::SynapseCalyxKernelParams, ErrorData> {
    let content_slot = params.content_slot.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "storage operation=intelligence sub_operation={} requires content_slot (the dense semantic-lens slot id)",
                params.operation.as_str()
            ),
        )
    })?;
    let content_slot = u16::try_from(content_slot).map_err(|_| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("content_slot {content_slot} exceeds the 16-bit slot id range"),
        )
    })?;
    let mut kernel =
        synapse_calyx::SynapseCalyxKernelParams::new(params.panel_version, content_slot);
    kernel.max_records = clamp_intelligence_records(params.max_records);
    if let Some(knn) = params.knn_k {
        kernel.knn = knn as usize;
    }
    if let Some(threshold) = params.edge_cos_threshold {
        kernel.edge_cos_threshold = threshold;
    }
    if let Some(min_recall) = params.min_recall_ratio {
        kernel.min_recall_ratio = min_recall;
    }
    kernel.anchor_kind = params
        .anchor_kind
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(kernel)
}

/// Builds the per-domain grounding kernel and persists it (with fingerprint) to
/// the native Kernel CF (mutating).
pub fn run_intelligence_kernel(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceKernelReport, ErrorData> {
    let report = db
        .build_domain_kernel_intelligence(&kernel_params(params)?)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceKernelReport {
        source_of_truth: "Calyx Kernel CF rows",
        panel_version: report.panel_version,
        content_slot: u32::from(report.content_slot),
        kernel_id: report.kernel_id,
        corpus_fingerprint: report.corpus_fingerprint,
        members: report.members as u64,
        kernel_graph_nodes: report.kernel_graph_nodes as u64,
        corpus_size: report.corpus_size as u64,
        vault_corpus_size: report.vault_corpus_size as u64,
        recall_kernel_only: report.recall_kernel_only,
        recall_ratio: report.recall_ratio,
        min_recall_ratio: report.min_recall_ratio,
        grounded: report.grounded,
        reached_anchor: report.reached_anchor,
        unanchored_members: report.unanchored_members as u64,
        anchored_members: report.anchored_members as u64,
        member_cx_ids: report.member_cx_ids,
        kernel_cf_rows_after: report.kernel_cf_rows_after as u64,
    })
}

/// Answers a grounded query through the domain kernel (read-only), or returns a
/// structured refusal naming the grounding gap.
pub fn run_intelligence_kernel_answer(
    db: &synapse_storage::Db,
    params: &StorageIntelligenceParams,
) -> Result<StorageIntelligenceKernelAnswerReport, ErrorData> {
    let query_cx_id = params
        .query_cx_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "storage operation=intelligence sub_operation=kernel_answer requires query_cx_id"
                    .to_owned(),
            )
        })?;
    let max_hops = params
        .max_hops
        .unwrap_or(synapse_calyx::SYNAPSE_KERNEL_DEFAULT_MAX_HOPS as u32)
        as usize;
    let report = db
        .kernel_answer_intelligence(&kernel_params(params)?, query_cx_id, max_hops)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageIntelligenceKernelAnswerReport {
        source_of_truth: "Calyx Kernel CF + Base CF rows",
        panel_version: report.panel_version,
        content_slot: u32::from(report.content_slot),
        query_cx_id: report.query_cx_id,
        grounded: report.grounded,
        kernel_id: report.kernel_id,
        anchor_kernel_node: report.anchor_kernel_node,
        total_score: report.total_score,
        hop_count: report.hop_count as u64,
        hops: report
            .hops
            .into_iter()
            .map(|hop| StorageIntelligenceKernelAnswerHop {
                from: hop.from,
                to: hop.to,
                edge_weight: hop.edge_weight,
                hop_index: hop.hop_index,
                hop_score: hop.hop_score,
            })
            .collect(),
        kernel_members: report.kernel_members as u64,
        recall_ratio: report.recall_ratio,
        min_recall_ratio: report.min_recall_ratio,
    })
}

fn storage_intelligence_agreement_edge(
    edge: synapse_calyx::SynapseCalyxAgreementEdge,
) -> StorageIntelligenceAgreementEdge {
    StorageIntelligenceAgreementEdge {
        panel_version: edge.panel_version,
        slot_a: u32::from(edge.slot_a),
        slot_b: u32::from(edge.slot_b),
        mean_agreement: edge.mean_agreement,
        agreement_weight: edge.agreement_weight,
        n: edge.n as u64,
    }
}

fn storage_intelligence_abundance(
    report: synapse_calyx::SynapseCalyxAbundanceReport,
) -> StorageIntelligenceAbundanceReport {
    StorageIntelligenceAbundanceReport {
        source_of_truth: "Calyx Base + XTerm + Graph CF rows",
        panel_version: report.panel_version,
        n_lenses: report.n_lenses as u64,
        n_constellations: report.n_constellations as u64,
        c_n2_upper_bound: report.c_n2_upper_bound as u64,
        materialized: report.materialized as u64,
        measured_count: report.measured_count as u64,
        derived_count: report.derived_count as u64,
        meaning_compression_yield: report.meaning_compression_yield,
        dda_signal_yield: report.dda_signal_yield as u64,
        n_eff: StorageIntelligenceNeffEstimate {
            value: report.n_eff.value,
            provisional: report.n_eff.provisional,
            ci_low: report.n_eff.ci_low,
            ci_high: report.n_eff.ci_high,
        },
        dpi_ceiling_bits: report.dpi_ceiling_bits,
        dpi_ceiling_provisional: report.dpi_ceiling_provisional,
        dpi_ceiling_anchor_kind: report.dpi_ceiling_anchor_kind,
        xterm_cf_rows: report.xterm_cf_rows as u64,
        graph_cf_rows: report.graph_cf_rows as u64,
    }
}

pub fn inspect_storage_summary(
    db: &synapse_storage::Db,
) -> Result<StorageSummaryResponse, ErrorData> {
    let (cf_sizes, missing_cf_size_estimates) = db
        .cf_live_data_size_estimates()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let (cf_row_counts, missing_cf_row_count_estimates) = db
        .cf_estimated_row_counts()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageSummaryResponse {
        schema_version: db.schema_version,
        storage_backend: db.backend_name().to_owned(),
        pressure_level: pressure_level(db.pressure_level()),
        pressure_transition_codes: db
            .pressure_transition_codes()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        audit_retention_policy_count: audit_retention_policies().len(),
        metrics_mode: storage_metrics_mode(),
        cf_sizes,
        cf_row_counts,
        missing_cf_size_estimates,
        missing_cf_row_count_estimates,
    })
}

fn storage_metrics_mode() -> String {
    "calyx_exact_scan_sizes_counts".to_owned()
}

pub fn put_probe_rows(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &StoragePutProbeRowsParams,
) -> Result<StoragePutProbeRowsResponse, ErrorData> {
    validate_probe_params(params)?;
    let cf_name = probe_writable_cf(&params.cf_name)?;
    let rows = build_probe_rows(params);
    let runtime = lock_runtime(runtime)?;
    let pressure = runtime.storage_pressure_level();
    if params.rows > 0 && !runtime.storage_pressure_permits_write(cf_name) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "storage diagnostic write refused under disk pressure: cf_name={cf_name} pressure_level={pressure:?}"
            ),
        ));
    }
    let before = cf_count(
        &runtime
            .storage_cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        cf_name,
    );
    runtime
        .storage_put_probe_rows(cf_name, rows)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let after_counts = runtime
        .storage_cf_row_counts()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let after_sizes = runtime
        .storage_cf_sizes()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let after = cf_count(&after_counts, cf_name);
    Ok(StoragePutProbeRowsResponse {
        cf_name: cf_name.to_owned(),
        key_prefix: params.key_prefix.trim().to_owned(),
        requested_rows: params.rows,
        value_bytes: params.value_bytes,
        before_rows: before,
        after_rows: after,
        rows_added: after.saturating_sub(before),
        after_cf_size_bytes: cf_count(&after_sizes, cf_name),
        pressure_level: pressure_level(runtime.storage_pressure_level()),
    })
}

pub fn run_storage_gc_once(
    db: &synapse_storage::Db,
    params: &StorageGcOnceParams,
) -> Result<StorageGcOnceResponse, ErrorData> {
    validate_gc_params(params)?;
    if params.cf_name.trim() == AUDIT_RETENTION_MODE {
        let before_counts = db
            .cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let before = audit_rows_total(&before_counts);
        let result = run_audit_retention(
            db,
            &AuditRetentionRunConfig {
                run_id: params.run_id.clone(),
                now_ns: params.now_ns,
                max_age_ns: params.max_age_ns,
                dedupe_window_ns: params.dedupe_window_ns,
                profile_id: params.profile_id.clone(),
                soft_cap_rows: params.soft_cap_rows,
                hard_cap_rows: params.hard_cap_rows,
            },
        )?;
        let after_counts = db
            .cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let after = audit_rows_total(&after_counts);
        return Ok(StorageGcOnceResponse {
            cf_name: AUDIT_RETENTION_MODE.to_owned(),
            before_rows: before,
            after_rows: after,
            total_evicted_rows: result.readback_report.total_deleted_rows,
            cache_evictions_total_delta: result.readback_report.total_deleted_rows,
            cf_reports: Vec::new(),
            audit_retention_report_key: Some(result.report_key),
            audit_retention: Some(result.readback_report),
        });
    }
    reject_audit_retention_fields(params)?;
    let cf_name = probe_writable_cf(&params.cf_name)?;
    let report = db
        .run_gc_once_with_row_caps(cf_name, params.soft_cap_rows, params.hard_cap_rows)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let (before, after) = report
        .cf(cf_name)
        .map(|cf_report| (cf_report.before_value, cf_report.after_value))
        .unwrap_or((0, 0));
    Ok(gc_response(cf_name, before, after, report))
}

pub fn run_storage_backup(
    db: &synapse_storage::Db,
    params: &StorageBackupParams,
) -> Result<StorageBackupResponse, ErrorData> {
    let target = validate_fs_path(&params.target_dir, "target_dir")?;
    let report = db
        .backup_calyx_vault(&target, params.include_regenerable)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(storage_backup_response(report))
}

pub fn run_storage_restore_verify(
    db: &synapse_storage::Db,
    params: &StorageRestoreVerifyParams,
) -> Result<StorageRestoreVerifyResponse, ErrorData> {
    let vault_path = validate_fs_path(&params.vault_path, "vault_path")?;
    let report = db
        .verify_calyx_restore(&vault_path)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(StorageRestoreVerifyResponse {
        verify: storage_verify_report(&report),
    })
}

fn validate_fs_path(raw: &str, field: &str) -> Result<std::path::PathBuf, ErrorData> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage {field} must not be empty"),
        ));
    }
    let path = std::path::PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage {field} must be an absolute path; got {trimmed:?}"),
        ));
    }
    Ok(path)
}

fn storage_backup_response(
    report: synapse_calyx::SynapseCalyxBackupReport,
) -> StorageBackupResponse {
    StorageBackupResponse {
        vault_id: report.vault_id,
        source_vault_dir: report.source_vault_dir.display().to_string(),
        target_root: report.target_root.display().to_string(),
        backup_vault_dir: report.backup_vault_dir.display().to_string(),
        manifest_path: report.manifest_path.display().to_string(),
        manifest_sha256: report.manifest_sha256,
        durable_seq: report.durable_seq,
        latest_seq: report.latest_seq,
        include_regenerable: report.include_regenerable,
        file_count: report.file_count,
        total_bytes: report.total_bytes,
        residency_enforced: report.residency_enforced,
        files: report
            .files
            .into_iter()
            .map(|file| StorageBackupFile {
                relative_path: file.relative_path,
                len_bytes: file.len_bytes,
                sha256: file.sha256,
            })
            .collect(),
        verify: storage_verify_report(&report.verify),
    }
}

fn storage_verify_report(report: &synapse_calyx::SynapseCalyxVerifyReport) -> StorageVerifyReport {
    StorageVerifyReport {
        vault_path: report.vault_path.display().to_string(),
        success: report.success,
        chain_intact: report.chain_intact,
        constellation_count: report.constellation_count,
        anchor_count: report.anchor_count,
        ledger_entry_count: report.ledger_entry_count,
        ledger_tip_hash: report.ledger_tip_hash.clone(),
        wal_bytes_present: report.wal_bytes_present,
        first_cx_id: report.first_cx_id.clone(),
        failure_reasons: report.failure_reasons.clone(),
    }
}

pub fn apply_storage_pressure_sample(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &StoragePressureSampleParams,
) -> Result<StoragePressureSampleResponse, ErrorData> {
    let runtime = lock_runtime(runtime)?;
    let report = runtime
        .storage_run_pressure_sample(params.free_bytes)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let pressure_transition_codes = runtime
        .storage_pressure_transition_codes()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (cf_row_counts, _missing_cf_row_count_estimates) = runtime
        .storage_cf_estimated_row_counts()
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    drop(runtime);
    Ok(StoragePressureSampleResponse {
        report: pressure_report(report),
        pressure_transition_codes,
        cf_row_counts,
    })
}

fn inspect_db(db: &synapse_storage::Db) -> Result<StorageInspectResponse, ErrorData> {
    Ok(StorageInspectResponse {
        schema_version: db.schema_version,
        storage_backend: db.backend_name().to_owned(),
        pressure_level: pressure_level(db.pressure_level()),
        pressure_transition_codes: db
            .pressure_transition_codes()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .into_iter()
            .map(str::to_owned)
            .collect(),
        audit_retention_policies: audit_retention_policies(),
        cf_sizes: db
            .cf_sizes()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        cf_row_counts: db
            .cf_row_counts()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?,
        cf_row_samples: cf_row_samples(db)?,
        calyx_vault: db
            .calyx_vault_inspect()
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .map(storage_calyx_vault_inspect),
    })
}

fn cf_row_samples(
    db: &synapse_storage::Db,
) -> Result<BTreeMap<String, Vec<StorageRowSample>>, ErrorData> {
    let mut samples = BTreeMap::new();
    for cf_name in cf::ALL_COLUMN_FAMILIES {
        let rows = db
            .scan_cf_tail(cf_name, MAX_INSPECT_SAMPLE_ROWS_PER_CF)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        samples.insert(
            cf_name.to_owned(),
            rows.into_iter()
                .map(|(key, value)| storage_row_sample(&key, &value))
                .collect(),
        );
    }
    Ok(samples)
}

fn storage_row_sample(key: &[u8], value: &[u8]) -> StorageRowSample {
    StorageRowSample {
        key_len_bytes: key.len() as u64,
        key_sha256: sha256_hex(key),
        key_material_omitted: true,
        value_len_bytes: value.len() as u64,
        value_sha256: sha256_hex(value),
        value_encoding: classify_value_encoding(value),
        value_content_omitted: true,
        redaction_policy: STORAGE_METADATA_ONLY_REDACTION_POLICY.to_owned(),
    }
}

fn storage_calyx_vault_inspect(report: BackendCalyxVaultInspect) -> StorageCalyxVaultInspect {
    StorageCalyxVaultInspect {
        schema_version: report.schema_version,
        vault_id: report.vault_id,
        latest_seq: report.latest_seq,
        inspected_at_unix_ms: report.inspected_at_unix_ms,
        collection_count: report.collection_count,
        raw_row_count: report.raw_row_count,
        live_row_count: report.live_row_count,
        expired_row_count: report.expired_row_count,
        user_key_bytes: report.user_key_bytes,
        payload_bytes: report.payload_bytes,
        stored_value_bytes: report.stored_value_bytes,
        total_logical_bytes: report.total_logical_bytes,
        collections: report
            .collections
            .into_iter()
            .map(|(name, collection)| (name, storage_calyx_vault_collection(collection)))
            .collect(),
    }
}

fn storage_calyx_vault_collection(
    collection: BackendCalyxVaultCollectionInspect,
) -> StorageCalyxVaultCollectionInspect {
    StorageCalyxVaultCollectionInspect {
        collection_name: collection.collection_name,
        cf_name: collection.cf_name,
        collection_id_hex: collection.collection_id_hex,
        namespace: collection.namespace,
        raw_row_count: collection.raw_row_count,
        live_row_count: collection.live_row_count,
        expired_row_count: collection.expired_row_count,
        user_key_bytes: collection.user_key_bytes,
        payload_bytes: collection.payload_bytes,
        stored_value_bytes: collection.stored_value_bytes,
        total_logical_bytes: collection.total_logical_bytes,
        expires_at_ms_histogram: collection.expires_at_ms_histogram,
    }
}

fn storage_anchors_response(
    report: BackendCalyxAnchorScanReport,
    source_value_len_bytes: u64,
) -> StorageAnchorsResponse {
    StorageAnchorsResponse {
        source_cf: report.source_cf,
        source_key_hex: report.source_key_hex,
        source_value_len_bytes,
        source_value_sha256: report.source_value_sha256,
        panel_name: report.panel_name,
        panel_version: report.panel_version,
        cx_id: report.cx_id,
        anchor_count: u64::try_from(report.anchors.len()).unwrap_or(u64::MAX),
        anchors: report
            .anchors
            .into_iter()
            .map(|row| StorageAnchorRow {
                key_hex: row.key_hex,
                cx_id: row.cx_id,
                kind: row.kind,
                value: storage_anchor_value(row.value),
                source: row.source,
                observed_at_ms: row.observed_at_ms,
                confidence: row.confidence,
            })
            .collect(),
    }
}

fn storage_anchor_value(value: BackendCalyxAnchorValueReadback) -> StorageAnchorValue {
    StorageAnchorValue {
        value_type: value.value_type,
        bool_value: value.bool_value,
        text_value: value.text_value,
        number_value: value.number_value,
        one_hot_values: value.one_hot_values,
        vector_len: value.vector_len,
        vector_sha256: value.vector_sha256,
    }
}

fn classify_value_encoding(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "empty".to_owned();
    }
    if std::str::from_utf8(bytes).is_err() {
        return "binary_or_invalid_utf8".to_owned();
    }
    if serde_json::from_slice::<Value>(bytes).is_ok() {
        return "json".to_owned();
    }
    "utf8_non_json".to_owned()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty hex string".to_owned());
    }
    if !value.len().is_multiple_of(2) {
        return Err(format!("hex length must be even; got {}", value.len()));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for index in (0..bytes.len()).step_by(2) {
        let hi = hex_digit(bytes[index])
            .ok_or_else(|| format!("invalid hex digit at byte offset {index}"))?;
        let lo = hex_digit(bytes[index + 1])
            .ok_or_else(|| format!("invalid hex digit at byte offset {}", index + 1))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn known_anchor_source_cf_for_key(raw: &str, key: &[u8]) -> Result<&'static str, ErrorData> {
    let trimmed = raw.trim();
    let Some(cf_name) = cf::ALL_COLUMN_FAMILIES
        .iter()
        .copied()
        .find(|cf_name| *cf_name == trimmed)
    else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage operation=anchors cf_name is not known: {trimmed:?}"),
        ));
    };
    synapse_storage::constellations::anchor_panel_for_source_row(cf_name, key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    Ok(cf_name)
}

fn storage_temporal_panel(
    registration: synapse_calyx::VaultTemporalPanelRegistration,
) -> StorageTemporalPanel {
    StorageTemporalPanel {
        schema_version: registration.schema_version,
        panel_name: registration.template.name,
        panel_version: registration.source_panel_version,
        registered_at_unix_ms: registration.registered_at_unix_ms,
        temporal_slots: registration
            .template
            .slots
            .into_iter()
            .map(|slot| StorageTemporalSlot {
                name: slot.name,
                runtime: format!("{:?}", slot.runtime),
                output: format!("{:?}", slot.output),
                retrieval_only: slot.retrieval_only,
                excluded_from_dedup: slot.excluded_from_dedup,
                required: slot.required,
            })
            .collect(),
        policy: storage_temporal_policy(&registration.policy),
    }
}

fn storage_temporal_policy(policy: &synapse_calyx::TemporalPolicy) -> StorageTemporalPolicy {
    StorageTemporalPolicy {
        enabled: policy.enabled,
        never_dominant: policy.never_dominant,
        decay: format!("{:?}", policy.decay),
        periodic_target_hour: policy.periodic.target_hour,
        periodic_target_day_of_week: policy.periodic.target_day_of_week,
        periodic_use_query_time: policy.periodic.use_now,
        sequence_direction: format!("{:?}", policy.sequence.direction),
        sequence_multi_anchor_mode: format!("{:?}", policy.sequence.multi_anchor_mode),
        fusion_recency: policy.fusion_weights.recency,
        fusion_sequence: policy.fusion_weights.sequence,
        fusion_periodic: policy.fusion_weights.periodic,
        post_retrieval_alpha: policy.boost.post_retrieval_alpha,
        recurrence_boost_enabled: policy.recurrence_boost.is_some(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(digest.as_ref()))
}

fn validate_probe_params(params: &StoragePutProbeRowsParams) -> Result<(), ErrorData> {
    if params.rows > MAX_PROBE_ROWS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_put_probe_rows rows must be <= {MAX_PROBE_ROWS}"),
        ));
    }
    if params.value_bytes > MAX_PROBE_VALUE_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_put_probe_rows value_bytes must be <= {MAX_PROBE_VALUE_BYTES}"),
        ));
    }
    let key_prefix = params.key_prefix.trim();
    if key_prefix.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_put_probe_rows key_prefix must not be empty",
        ));
    }
    if key_prefix.len() > MAX_KEY_PREFIX_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_put_probe_rows key_prefix must be <= {MAX_KEY_PREFIX_BYTES} bytes"),
        ));
    }
    if let Some(value_json) = &params.value_json
        && !value_json.is_object()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_put_probe_rows value_json must be a JSON object",
        ));
    }
    match params.key_mode.as_deref().map(str::trim) {
        None | Some("prefix_index") => {}
        Some("timeline_ts") => {
            if params.ts_ns_start.is_none() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage_put_probe_rows key_mode=timeline_ts requires ts_ns_start",
                ));
            }
            if params.cf_name.trim() != cf::CF_TIMELINE {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage_put_probe_rows key_mode=timeline_ts is only valid for CF_TIMELINE",
                ));
            }
            // Timeline-keyed probes must be valid TimelineRecord envelopes:
            // this mode exists to seed realistic timeline rows, and the
            // envelope rejects unknown fields, so the generic probe_id/seq
            // diagnostics are not injected. Validate the merged row-0 value
            // up front so a bad template fails closed instead of writing
            // rows that every consumer counts as invalid.
            let Some(template) = &params.value_json else {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "storage_put_probe_rows key_mode=timeline_ts requires value_json",
                ));
            };
            let merged = timeline_record_value(template, params.ts_ns_start.unwrap_or_default());
            if let Err(error) =
                serde_json::from_value::<synapse_core::types::TimelineRecord>(merged)
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "storage_put_probe_rows key_mode=timeline_ts value_json is not a valid TimelineRecord: {error}"
                    ),
                ));
            }
        }
        Some(other) => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage_put_probe_rows key_mode must be \"prefix_index\" or \"timeline_ts\"; got {other:?}"
                ),
            ));
        }
    }
    Ok(())
}

fn validate_gc_params(params: &StorageGcOnceParams) -> Result<(), ErrorData> {
    if params.soft_cap_rows == 0 || params.soft_cap_rows > MAX_ROW_CAP {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_gc_once soft_cap_rows must be between 1 and {MAX_ROW_CAP}"),
        ));
    }
    if params.hard_cap_rows == 0 || params.hard_cap_rows > MAX_ROW_CAP {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("storage_gc_once hard_cap_rows must be between 1 and {MAX_ROW_CAP}"),
        ));
    }
    if params.hard_cap_rows < params.soft_cap_rows {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_gc_once hard_cap_rows must be >= soft_cap_rows",
        ));
    }
    if params.cf_name.trim() == AUDIT_RETENTION_MODE {
        validate_audit_retention_config(&AuditRetentionRunConfig {
            run_id: params.run_id.clone(),
            now_ns: params.now_ns,
            max_age_ns: params.max_age_ns,
            dedupe_window_ns: params.dedupe_window_ns,
            profile_id: params.profile_id.clone(),
            soft_cap_rows: params.soft_cap_rows,
            hard_cap_rows: params.hard_cap_rows,
        })?;
    }
    Ok(())
}

fn reject_audit_retention_fields(params: &StorageGcOnceParams) -> Result<(), ErrorData> {
    if params.run_id.is_some()
        || params.now_ns.is_some()
        || params.max_age_ns.is_some()
        || params.dedupe_window_ns.is_some()
        || params.profile_id.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "storage_gc_once audit retention fields require cf_name=\"AUDIT_RETENTION\"",
        ));
    }
    Ok(())
}

fn probe_writable_cf(raw: &str) -> Result<&'static str, ErrorData> {
    let trimmed = raw.trim();
    PROBE_WRITABLE_CFS
        .into_iter()
        .find(|name| *name == trimmed)
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "storage diagnostic writes support only {}; got {trimmed:?}",
                    PROBE_WRITABLE_CFS.join(", ")
                ),
            )
        })
}

fn build_probe_rows(params: &StoragePutProbeRowsParams) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prefix = params.key_prefix.trim();
    let timeline_keys = params.key_mode.as_deref().map(str::trim) == Some("timeline_ts");
    (0..params.rows)
        .map(|index| {
            let key = if timeline_keys {
                let ts_ns = params.ts_ns_start.unwrap_or_default().saturating_add(
                    params
                        .ts_ns_step
                        .unwrap_or_default()
                        .saturating_mul(u64::from(index)),
                );
                synapse_storage::timeline::timeline_key(ts_ns, index)
            } else {
                format!("{prefix}:{index:020}").into_bytes()
            };
            let value = probe_value(params, prefix, index);
            (key, value)
        })
        .collect()
}

fn probe_value(params: &StoragePutProbeRowsParams, prefix: &str, index: u32) -> Vec<u8> {
    if let Some(template) = &params.value_json {
        if params.key_mode.as_deref().map(str::trim) == Some("timeline_ts") {
            let ts_ns = params.ts_ns_start.unwrap_or_default().saturating_add(
                params
                    .ts_ns_step
                    .unwrap_or_default()
                    .saturating_mul(u64::from(index)),
            );
            let merged = timeline_record_value(template, ts_ns);
            return synapse_storage::encode_json(&merged)
                .unwrap_or_else(|_error| byte_probe_value(prefix, index, 0));
        }
        return json_probe_value(params, template, prefix, index);
    }
    byte_probe_value(prefix, index, params.value_bytes as usize)
}

/// Merges the per-row timestamp into a `TimelineRecord` template without the
/// generic probe diagnostics (the envelope rejects unknown fields).
fn timeline_record_value(template: &Value, ts_ns: u64) -> Value {
    let mut value = template.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("ts_ns".to_owned(), Value::from(ts_ns));
    }
    value
}

fn byte_probe_value(prefix: &str, index: u32, len: usize) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    let seed = format!("synapse-storage-probe:{prefix}:{index}:").into_bytes();
    let mut value = Vec::with_capacity(len);
    while value.len() < len {
        value.extend_from_slice(&seed);
    }
    value.truncate(len);
    value
}

fn json_probe_value(
    params: &StoragePutProbeRowsParams,
    template: &Value,
    prefix: &str,
    index: u32,
) -> Vec<u8> {
    let mut value = template.clone();
    if let Some(object) = value.as_object_mut() {
        object
            .entry("probe_id")
            .or_insert_with(|| Value::String(format!("{prefix}:{index:020}")));
        object
            .entry("seq")
            .or_insert_with(|| Value::from(u64::from(index)));
        if let Some(start) = params.ts_ns_start {
            let ts_ns = start.saturating_add(
                params
                    .ts_ns_step
                    .unwrap_or_default()
                    .saturating_mul(u64::from(index)),
            );
            object.entry("ts_ns").or_insert_with(|| Value::from(ts_ns));
            object
                .entry("audit_id")
                .or_insert_with(|| Value::String(format!("{ts_ns:020}-{index:010}")));
        }
    }
    synapse_storage::encode_json(&value)
        .unwrap_or_else(|_error| byte_probe_value(prefix, index, params.value_bytes as usize))
}

fn gc_response(
    cf_name: &str,
    before_rows: u64,
    after_rows: u64,
    report: GcReport,
) -> StorageGcOnceResponse {
    let total_evicted_rows = report.total_evicted_rows();
    StorageGcOnceResponse {
        cf_name: cf_name.to_owned(),
        before_rows,
        after_rows,
        total_evicted_rows,
        cache_evictions_total_delta: total_evicted_rows,
        cf_reports: report
            .cf_reports
            .into_iter()
            .map(|report| StorageGcCfReport {
                cf_name: report.cf_name,
                before_value: report.before_value,
                after_value: report.after_value,
                before_estimated_num_keys: report.before_estimated_num_keys,
                after_estimated_num_keys: report.after_estimated_num_keys,
                examined_rows: report.examined_rows,
                scan_limited: report.scan_limited,
                evicted_rows: report.evicted_rows,
                hard_cap_reached: report.hard_cap_reached,
                hard_cap_code: report.hard_cap_code.map(str::to_owned),
                eviction_skipped_reason: report.eviction_skipped_reason.map(str::to_owned),
            })
            .collect(),
        audit_retention_report_key: None,
        audit_retention: None,
    }
}

fn audit_rows_total(counts: &BTreeMap<String, u64>) -> u64 {
    [
        cf::CF_ACTION_LOG,
        cf::CF_REFLEX_AUDIT,
        cf::CF_EVENTS,
        cf::CF_OBSERVATIONS,
        cf::CF_SESSIONS,
        cf::CF_PROFILES,
        cf::CF_KV,
    ]
    .into_iter()
    .map(|cf_name| cf_count(counts, cf_name))
    .sum()
}

fn pressure_report(report: PressureReport) -> StoragePressureReport {
    StoragePressureReport {
        free_bytes: report.free_bytes,
        previous_level: pressure_level(report.previous_level),
        current_level: pressure_level(report.current_level),
        emitted_code: report.emitted_code.map(str::to_owned),
        compacted_cfs: report
            .compacted_cfs
            .into_iter()
            .map(str::to_owned)
            .collect(),
        gc_advised: report.gc_advised,
    }
}

fn pressure_level(level: DiskPressureLevel) -> StoragePressureLevel {
    StoragePressureLevel {
        name: format!("{level:?}"),
        value: level as u8,
    }
}

fn cf_count(counts: &BTreeMap<String, u64>, cf_name: &str) -> u64 {
    counts.get(cf_name).copied().unwrap_or_default()
}

fn lock_runtime(
    runtime: &Arc<Mutex<ReflexRuntime>>,
) -> Result<MutexGuard<'_, ReflexRuntime>, ErrorData> {
    runtime.lock().map_err(|_err| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned",
        )
    })
}
