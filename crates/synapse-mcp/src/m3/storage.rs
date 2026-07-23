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
        }
    }

    #[must_use]
    pub const fn mutates_state(self) -> bool {
        // Abundance is a pure read; weave persists XTerm/Graph rows and the
        // assay operations persist Assay rows.
        !matches!(self, Self::Abundance)
    }

    #[must_use]
    pub const fn requires_anchor(self) -> bool {
        matches!(self, Self::Bits | Self::Sufficiency)
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
    /// Grounded outcome anchor kind to measure bits about (bits/sufficiency).
    /// Synapse writes outcome anchors as a label string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_kind: Option<String>,
    /// k for the KSG mutual-information estimator (bits/sufficiency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 32))]
    pub ksg_k: Option<u32>,
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
    pub n_eff: StorageIntelligenceNeffEstimate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi_ceiling_bits: Option<f32>,
    pub dpi_ceiling_provisional: bool,
    pub xterm_cf_rows: u64,
    pub graph_cf_rows: u64,
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
    pub grounded: bool,
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
    pub grounded: bool,
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

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageIntelligenceRedundancyReport {
    pub source_of_truth: &'static str,
    pub panel_version: u32,
    pub n_lenses: u64,
    pub records_scanned: u64,
    pub effective_rank: f32,
    pub pairs_evaluated: u64,
    pub redundant_pairs: Vec<StorageIntelligenceRedundancyPair>,
    pub assay_cf_rows_after: u64,
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
        synapse_calyx::SynapseCalyxAssayParams::new(params.panel_version, anchor_kind.to_owned());
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
        grounded: report.grounded,
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
    let mut assay =
        synapse_calyx::SynapseCalyxAssayParams::new(params.panel_version, String::new());
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
        pairs_evaluated: report.pairs_evaluated as u64,
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
        n_eff: StorageIntelligenceNeffEstimate {
            value: report.n_eff.value,
            provisional: report.n_eff.provisional,
            ci_low: report.n_eff.ci_low,
            ci_high: report.n_eff.ci_high,
        },
        dpi_ceiling_bits: report.dpi_ceiling_bits,
        dpi_ceiling_provisional: report.dpi_ceiling_provisional,
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
