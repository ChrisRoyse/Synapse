use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::tool_profiles::CodexClientSurfaceSnapshot;

use crate::m3::{
    hygiene::{
        HygieneBlindSpotParams, HygieneBlindSpotResponse, HygieneDriftParams, HygieneDriftResponse,
        HygieneFlagsParams, HygieneFlagsResponse, HygieneGroundingGapParams,
        HygieneGroundingGapResponse, HygieneGuardCalibrateParams, HygieneGuardCalibrateResponse,
        HygieneGuardVerifyParams, HygieneGuardVerifyResponse, HygieneKernelParams,
        HygieneKernelRebuildParams, HygieneKernelRebuildResponse, HygieneKernelResponse,
        HygieneReportParams, HygieneReportResponse, HygieneScanStorageParams,
        HygieneScanStorageResponse, HygieneScanTextParams, HygieneScanTextResponse,
        HygieneVaultVerifyParams, HygieneVaultVerifyResponse,
    },
    local_models::{
        LocalModelListParams, LocalModelListResponse, LocalModelProbeParams,
        LocalModelProbeResponse, LocalModelRegisterParams, LocalModelRegisterResponse,
        LocalModelRemoveParams, LocalModelRemoveResponse, LocalModelUpdateParams,
        LocalModelUpdateResponse,
    },
    storage::{
        StorageAnchorsParams, StorageAnchorsResponse, StorageBackupParams, StorageBackupResponse,
        StorageBackupStatusParams, StorageBackupStatusResponse, StorageCorpusHistogramParams,
        StorageCorpusHistogramResponse, StorageFindSimilarParams, StorageFindSimilarResponse,
        StorageGcOnceParams, StorageGcOnceResponse, StorageInspectParams, StorageInspectResponse,
        StorageIntelligenceParams, StorageIntelligenceResponse, StoragePanelCoverageParams,
        StoragePanelCoverageResponse, StoragePanelLifecycleParams, StoragePanelLifecycleResponse,
        StorageRestoreVerifyParams, StorageRestoreVerifyResponse, StorageRetireOrphanSlotCfsParams,
        StorageRetireOrphanSlotCfsResponse, StorageRetireSearchGenerationParams,
        StorageRetireSearchGenerationResponse, StorageRowReadParams, StorageRowReadResponse,
        StorageSearchRebuildParams, StorageSearchRebuildResponse, StorageSnapshotGcObservation,
        StorageSummaryResponse, StorageTemporalBackfillParams, StorageTemporalBackfillResponse,
        StorageTemporalPanelsParams, StorageTemporalPanelsResponse, StorageTemporalRerankParams,
        StorageTemporalRerankResponse, StorageTranscriptOrderRebuildParams,
        StorageTranscriptOrderRebuildResponse, StorageTranscriptOrderStatusParams,
        StorageTranscriptOrderStatusResponse,
    },
};
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageOperation {
    Inspect,
    Summary,
    SnapshotGcStatus,
    GcOnce,
    Anchors,
    RowRead,
    TemporalPanels,
    CorpusHistogram,
    PanelCoverage,
    TemporalRerank,
    TemporalBackfill,
    SearchRebuild,
    TranscriptOrderStatus,
    TranscriptOrderRebuild,
    PanelLifecycle,
    FindSimilar,
    RetireOrphanSlotCfs,
    RetireSearchGeneration,
    Backup,
    BackupStatus,
    RestoreVerify,
    Intelligence,
}

impl StorageOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Summary => "summary",
            Self::SnapshotGcStatus => "snapshot_gc_status",
            Self::GcOnce => "gc_once",
            Self::Anchors => "anchors",
            Self::RowRead => "row_read",
            Self::TemporalPanels => "temporal_panels",
            Self::CorpusHistogram => "corpus_histogram",
            Self::PanelCoverage => "panel_coverage",
            Self::TemporalRerank => "temporal_rerank",
            Self::TemporalBackfill => "temporal_backfill",
            Self::SearchRebuild => "search_rebuild",
            Self::TranscriptOrderStatus => "transcript_order_status",
            Self::TranscriptOrderRebuild => "transcript_order_rebuild",
            Self::PanelLifecycle => "panel_lifecycle",
            Self::FindSimilar => "find_similar",
            Self::RetireOrphanSlotCfs => "retire_orphan_slot_cfs",
            Self::RetireSearchGeneration => "retire_search_generation",
            Self::Backup => "backup",
            Self::BackupStatus => "backup_status",
            Self::RestoreVerify => "restore_verify",
            Self::Intelligence => "intelligence",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageParams {
    pub operation: StorageOperation,
    #[serde(default)]
    pub inspect: Option<StorageInspectParams>,
    #[serde(default)]
    pub summary: Option<StorageInspectParams>,
    #[serde(default)]
    pub snapshot_gc_status: Option<StorageInspectParams>,
    #[serde(default)]
    pub gc_once: Option<StorageGcOnceParams>,
    #[serde(default)]
    pub anchors: Option<StorageAnchorsParams>,
    #[serde(default)]
    pub row_read: Option<StorageRowReadParams>,
    #[serde(default)]
    pub temporal_panels: Option<StorageTemporalPanelsParams>,
    #[serde(default)]
    pub corpus_histogram: Option<StorageCorpusHistogramParams>,
    #[serde(default)]
    pub panel_coverage: Option<StoragePanelCoverageParams>,
    #[serde(default)]
    pub temporal_rerank: Option<StorageTemporalRerankParams>,
    #[serde(default)]
    pub temporal_backfill: Option<StorageTemporalBackfillParams>,
    #[serde(default)]
    pub search_rebuild: Option<StorageSearchRebuildParams>,
    #[serde(default)]
    pub transcript_order_status: Option<StorageTranscriptOrderStatusParams>,
    #[serde(default)]
    pub transcript_order_rebuild: Option<StorageTranscriptOrderRebuildParams>,
    #[serde(default)]
    pub panel_lifecycle: Option<StoragePanelLifecycleParams>,
    #[serde(default)]
    pub find_similar: Option<StorageFindSimilarParams>,
    #[serde(default)]
    pub retire_orphan_slot_cfs: Option<StorageRetireOrphanSlotCfsParams>,
    #[serde(default)]
    pub retire_search_generation: Option<StorageRetireSearchGenerationParams>,
    #[serde(default)]
    pub backup: Option<StorageBackupParams>,
    #[serde(default)]
    pub backup_status: Option<StorageBackupStatusParams>,
    #[serde(default)]
    pub restore_verify: Option<StorageRestoreVerifyParams>,
    #[serde(default)]
    pub intelligence: Option<StorageIntelligenceParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageResponse {
    pub operation: StorageOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect: Option<StorageInspectResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<StorageSummaryResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_gc_status: Option<StorageSnapshotGcObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_once: Option<StorageGcOnceResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchors: Option<StorageAnchorsResponse>,
    /// Boxed: this variant carries a decoded `StoredObservation` projection,
    /// which is by far the widest arm of `StorageResponse`. Inline it and the
    /// whole facade handler's stack frame crosses clippy's 512 KB ceiling for
    /// every operation, including the ones that never touch a row body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_read: Option<Box<StorageRowReadResponse>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_panels: Option<StorageTemporalPanelsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus_histogram: Option<StorageCorpusHistogramResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_coverage: Option<StoragePanelCoverageResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_rerank: Option<StorageTemporalRerankResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_backfill: Option<StorageTemporalBackfillResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_rebuild: Option<StorageSearchRebuildResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_order_status: Option<Box<StorageTranscriptOrderStatusResponse>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_order_rebuild: Option<Box<StorageTranscriptOrderRebuildResponse>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_lifecycle: Option<StoragePanelLifecycleResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub find_similar: Option<StorageFindSimilarResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retire_orphan_slot_cfs: Option<StorageRetireOrphanSlotCfsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retire_search_generation: Option<StorageRetireSearchGenerationResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<StorageBackupResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_status: Option<StorageBackupStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_verify: Option<StorageRestoreVerifyResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence: Option<StorageIntelligenceResponse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOperation {
    List,
    Status,
    Probe,
    Register,
    Update,
    Remove,
    Recommend,
    Override,
}

impl ModelOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Status => "status",
            Self::Probe => "probe",
            Self::Register => "register",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::Recommend => "recommend",
            Self::Override => "override",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelStatusParams {
    #[serde(default)]
    pub include_disabled: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRecommendParams {
    /// Explicit task class. Agent-task dispatch uses the immutable template id
    /// as its class; free-text prompt inference is deliberately unsupported.
    pub task_class: String,
    /// Minimum terminal observations required before the result is grounded.
    #[serde(default = "default_model_recommend_min_evidence")]
    pub min_evidence: usize,
}

fn default_model_recommend_min_evidence() -> usize {
    4
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRecommendationEvidence {
    pub model: String,
    pub successes: u64,
    pub failures: u64,
    pub evidence_count: u64,
    pub expected_success: f64,
    pub success_ci95_low: f64,
    pub success_ci95_high: f64,
    pub outcome_information_bits: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_cost_micro_usd: Option<u64>,
    pub priced_observations: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelRecommendResponse {
    pub task_class: String,
    pub grounding: String,
    pub evidence_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_model: Option<String>,
    pub candidates: Vec<ModelRecommendationEvidence>,
    pub excluded_legacy_attempts: u64,
    pub decision_id: String,
    pub decision_row_key: String,
    pub decision_row_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_override: Option<ModelOverrideReadback>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelOverrideParams {
    pub task_class: String,
    pub decision_id: String,
    pub decision_row_key: String,
    pub selected_model: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelOverrideReadback {
    pub schema: String,
    pub task_class: String,
    pub decision_id: String,
    pub selected_model: String,
    pub reason: String,
    pub observed_unix_ns: u128,
    pub current_row_key: String,
    pub history_row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelParams {
    pub operation: ModelOperation,
    #[serde(default)]
    pub list: Option<LocalModelListParams>,
    #[serde(default)]
    pub status: Option<ModelStatusParams>,
    #[serde(default)]
    pub probe: Option<LocalModelProbeParams>,
    #[serde(default)]
    pub register: Option<LocalModelRegisterParams>,
    #[serde(default)]
    pub update: Option<LocalModelUpdateParams>,
    #[serde(default)]
    pub remove: Option<LocalModelRemoveParams>,
    #[serde(default)]
    pub recommend: Option<ModelRecommendParams>,
    #[serde(default)]
    pub r#override: Option<ModelOverrideParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelStatusResponse {
    pub source_of_truth: &'static str,
    pub scanned_rows: usize,
    pub visible_rows: usize,
    pub corrupt_rows: usize,
    pub enabled_rows: usize,
    pub disabled_rows: usize,
    pub probed_rows: usize,
    pub healthy_rows: usize,
    pub unhealthy_rows: usize,
    pub rows_with_api_key_secret: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelResponse {
    pub operation: ModelOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<LocalModelListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ModelStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<LocalModelProbeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub register: Option<LocalModelRegisterResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<LocalModelUpdateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<LocalModelRemoveResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommend: Option<ModelRecommendResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#override: Option<ModelOverrideReadback>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HygieneOperation {
    ScanText,
    ScanStorage,
    Flags,
    Report,
    GroundingGap,
    BlindSpot,
    Drift,
    VaultVerify,
    /// Read the persisted grounding-kernel artifact's health (#1675).
    Kernel,
    /// COLD per-domain grounding-kernel rebuild sweep (#1675).
    KernelRebuild,
    /// Calibrate and persist the Ward guard profile (#1677).
    GuardCalibrate,
    /// Evaluate one record against the persisted Ward guard profile (#1677).
    GuardVerify,
    /// Read native Anneal pointer, tripwire, budget, and ledger state (#1681).
    AnnealStatus,
    /// Build and measure an isolated persisted-search candidate (#1681).
    AnnealSearchPropose,
    /// Restore a prior tuning artifact and its bound search manifests (#1681).
    AnnealRollback,
}

impl HygieneOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ScanText => "scan_text",
            Self::ScanStorage => "scan_storage",
            Self::Flags => "flags",
            Self::Report => "report",
            Self::GroundingGap => "grounding_gap",
            Self::BlindSpot => "blind_spot",
            Self::Drift => "drift",
            Self::VaultVerify => "vault_verify",
            Self::Kernel => "kernel",
            Self::KernelRebuild => "kernel_rebuild",
            Self::GuardCalibrate => "guard_calibrate",
            Self::GuardVerify => "guard_verify",
            Self::AnnealStatus => "anneal_status",
            Self::AnnealSearchPropose => "anneal_search_propose",
            Self::AnnealRollback => "anneal_rollback",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneParams {
    pub operation: HygieneOperation,
    #[serde(default)]
    pub scan_text: Option<HygieneScanTextParams>,
    #[serde(default)]
    pub scan_storage: Option<HygieneScanStorageParams>,
    #[serde(default)]
    pub flags: Option<HygieneFlagsParams>,
    #[serde(default)]
    pub report: Option<HygieneReportParams>,
    #[serde(default)]
    pub grounding_gap: Option<HygieneGroundingGapParams>,
    #[serde(default)]
    pub blind_spot: Option<HygieneBlindSpotParams>,
    #[serde(default)]
    pub drift: Option<HygieneDriftParams>,
    #[serde(default)]
    pub vault_verify: Option<HygieneVaultVerifyParams>,
    #[serde(default)]
    pub kernel: Option<HygieneKernelParams>,
    #[serde(default)]
    pub kernel_rebuild: Option<HygieneKernelRebuildParams>,
    #[serde(default)]
    pub guard_calibrate: Option<HygieneGuardCalibrateParams>,
    #[serde(default)]
    pub guard_verify: Option<HygieneGuardVerifyParams>,
    #[serde(default)]
    pub anneal_status: Option<HygieneAnnealStatusParams>,
    #[serde(default)]
    pub anneal_search_propose: Option<HygieneAnnealSearchProposeParams>,
    #[serde(default)]
    pub anneal_rollback: Option<HygieneAnnealRollbackParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneResponse {
    pub operation: HygieneOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_text: Option<HygieneScanTextResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_storage: Option<HygieneScanStorageResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<HygieneFlagsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<HygieneReportResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grounding_gap: Option<HygieneGroundingGapResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blind_spot: Option<HygieneBlindSpotResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift: Option<HygieneDriftResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify: Option<HygieneVaultVerifyResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<HygieneKernelResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_rebuild: Option<HygieneKernelRebuildResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_calibrate: Option<HygieneGuardCalibrateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_verify: Option<HygieneGuardVerifyResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anneal_status: Option<HygieneAnnealStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anneal_search_propose: Option<HygieneAnnealSearchProposeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anneal_rollback: Option<HygieneAnnealRollbackResponse>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneAnnealStatusParams {}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct HygieneAnnealStatusResponse {
    pub live_artifact_sha256: String,
    pub live_artifact_bytes: usize,
    pub rollback_rows: usize,
    pub recent_changes: usize,
    pub fusion_k: u32,
    pub index_m_max: usize,
    pub index_ef_construction: usize,
    pub index_beamwidth: usize,
    pub index_ef_search: usize,
    pub index_alpha: f32,
    pub budget_cpu_used_fraction: f64,
    pub budget_vram_used_bytes: u64,
    pub budget_warning_code: Option<String>,
    pub tripwire_count: usize,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneAnnealSearchProposeParams {
    pub panel_version: u32,
    pub index_m_max: usize,
    pub index_ef_construction: usize,
    pub index_beamwidth: usize,
    pub index_ef_search: usize,
    pub index_alpha: f32,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct HygieneAnnealSearchProposeResponse {
    pub outcome: String,
    pub change_id: Option<u64>,
    pub panel_version: u32,
    pub source_base_seq: u64,
    pub query_count: usize,
    pub prior_artifact_sha256: String,
    pub candidate_artifact_sha256: String,
    pub live_artifact_sha256_after: String,
    pub incumbent_manifest_sha256: String,
    pub candidate_manifest_sha256: String,
    pub live_manifest_sha256_after: String,
    pub candidate_slot_metrics: Vec<HygieneAnnealSearchSlotMetrics>,
    pub incumbent_slot_metrics: Vec<HygieneAnnealSearchSlotMetrics>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct HygieneAnnealSearchSlotMetrics {
    pub slot: u16,
    pub query_count: usize,
    pub recall_mean: f64,
    pub recall_min: f64,
    pub search_p99_ms_mean: f64,
    pub search_p99_ms_max: f64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HygieneAnnealRollbackParams {
    pub change_id: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct HygieneAnnealRollbackResponse {
    pub change_id: u64,
    pub candidate_artifact_sha256: String,
    pub restored_artifact_sha256: String,
    pub restored_artifact_bytes: usize,
    pub rollback_rows_after: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupOperation {
    Status,
    Doctor,
    Repair,
    LaunchdService,
    HostTransition,
}

impl SetupOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Doctor => "doctor",
            Self::Repair => "repair",
            Self::LaunchdService => "launchd_service",
            Self::HostTransition => "host_transition",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupStatusParams {}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupRepairParams {
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupLaunchdServiceAction {
    Status,
    Restart,
}

impl SetupLaunchdServiceAction {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Restart => "restart",
        }
    }
}

/// Lifecycle request for Synapse's one installed macOS LaunchAgent.
///
/// The label and executable are intentionally not parameters: accepting either
/// from the caller would turn this typed capability back into generic shell.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupLaunchdServiceParams {
    pub action: SetupLaunchdServiceAction,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confirmation: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupLaunchdRestartState {
    Requested,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupLaunchdCommandReadback {
    pub executable: String,
    pub args: Vec<String>,
    pub exit_code: i32,
    pub stdout_len_bytes: usize,
    pub stdout_sha256: String,
    pub stderr_len_bytes: usize,
    pub stderr_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupLaunchdServiceProbe {
    pub label: String,
    pub service_target: String,
    pub effective_uid: u32,
    pub registered: bool,
    pub state: Option<String>,
    pub pid: Option<u32>,
    pub query: SetupLaunchdCommandReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupLaunchdRestartReadback {
    pub schema: String,
    pub request_id: String,
    pub state: SetupLaunchdRestartState,
    pub service_target: String,
    pub requesting_pid: u32,
    pub completed_pid: Option<u32>,
    pub requested_at_unix_ms: u128,
    pub completed_at_unix_ms: Option<u128>,
    pub reason_len_bytes: usize,
    pub reason_sha256: String,
    pub command_executable: String,
    pub command_args: Vec<String>,
    pub command_exit_code: Option<i32>,
    pub before_query_sha256: String,
    pub after_query_sha256: Option<String>,
    pub failure_code: Option<String>,
    pub manifest_path: String,
    pub manifest_len_bytes: u64,
    pub manifest_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupLaunchdServiceResponse {
    pub action: SetupLaunchdServiceAction,
    pub source_of_truth: String,
    pub service: SetupLaunchdServiceProbe,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<SetupLaunchdRestartReadback>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupHostTransitionAction {
    Status,
    Configure,
    Preflight,
    Execute,
}

impl SetupHostTransitionAction {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Configure => "configure",
            Self::Preflight => "preflight",
            Self::Execute => "execute",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupHostTransitionKind {
    Restart,
    Poweroff,
}

impl SetupHostTransitionKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Poweroff => "poweroff",
        }
    }
}

/// A cross-project production lease whose physical kernel lock and application
/// checkpoint must both be inspected before a planned host transition.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionGuardSpec {
    pub id: String,
    pub distribution: String,
    pub lease_path: String,
    pub lease_active_phase: String,
    pub checkpoint_path: String,
    pub checkpoint_complete_field: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionGuardAcceptance {
    pub guard_id: String,
    pub checkpoint_sha256: String,
}

/// Explicit destructive override. It is accepted only when every named
/// checkpoint is complete and its separately read bytes match the supplied
/// digest. The acceptance is persisted independently from reboot intent.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionOverride {
    pub accepted_job_ids: Vec<String>,
    pub guard_acceptances: Vec<SetupHostTransitionGuardAcceptance>,
    pub reason: String,
    pub confirmation: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionParams {
    pub action: SetupHostTransitionAction,
    #[serde(default)]
    pub transition: Option<SetupHostTransitionKind>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub preflight_id: Option<String>,
    #[serde(default)]
    pub confirmation: Option<String>,
    #[serde(default)]
    pub guards: Option<Vec<SetupHostTransitionGuardSpec>>,
    #[serde(default)]
    pub override_acceptance: Option<SetupHostTransitionOverride>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupParams {
    pub operation: SetupOperation,
    #[serde(default)]
    pub status: Option<SetupStatusParams>,
    #[serde(default)]
    pub doctor: Option<SetupStatusParams>,
    #[serde(default)]
    pub repair: Option<SetupRepairParams>,
    #[serde(default)]
    pub launchd_service: Option<SetupLaunchdServiceParams>,
    #[serde(default)]
    pub host_transition: Option<SetupHostTransitionParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileReadback {
    pub path: String,
    pub exists: bool,
    pub len_bytes: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupStatusResponse {
    pub source_of_truth: &'static str,
    pub pid: u32,
    pub bind: String,
    pub source_dir: String,
    pub setup_script_file: FileReadback,
    pub setup_repair_command_args: Vec<String>,
    pub setup_repair_mcp_tool: String,
    pub token_file: FileReadback,
    pub daemon_run_file: FileReadback,
    pub shared_daemon_run_file: FileReadback,
    pub codex_config_file: FileReadback,
    pub token_env_present: bool,
    pub token_env_len_bytes: Option<usize>,
    pub codex_mcp_config_mentions_synapse: bool,
    pub codex_mcp_config_mentions_bearer_env: bool,
    /// Physical state of the daemon autostart task (#1862).
    pub autostart: SetupAutostartReadback,
}

/// Whether the registered autostart task can actually start the daemon (#1862).
///
/// `Get-ScheduledTask` reports `State=Ready` for a task whose action points at a
/// deleted file, so task state alone is not evidence of a working autostart.
/// This reads the registered action and independently stats the file it names.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupAutostartReadback {
    /// Scheduled task inspected.
    pub task_name: String,
    /// Whether the task is registered at all.
    pub task_registered: bool,
    /// Task state as Windows reports it (`Ready`, `Running`, `Disabled`, ...).
    pub task_state: Option<String>,
    /// Executable the registered action runs.
    pub action_execute: Option<String>,
    /// Arguments the registered action passes.
    pub action_arguments: Option<String>,
    /// Launcher script parsed out of the action arguments.
    pub launcher_path: Option<String>,
    /// Independent stat of that exact path.
    pub launcher_file: Option<FileReadback>,
    /// True only when the task is registered, names a launcher, and that
    /// launcher physically exists.
    pub can_start_daemon: bool,
    /// True when the launcher sits inside the log directory, where routine log
    /// cleanup deletes it. Always a defect (#1862).
    pub launcher_in_log_dir: bool,
    /// Empty when nominal; otherwise the exact defects with remediation.
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionGuardReadback {
    pub id: String,
    pub distribution: String,
    pub lease_path: String,
    pub lease_active: bool,
    pub lease_owner_pid: Option<u32>,
    pub lease_owner_command: Option<String>,
    pub lease_phase: String,
    pub lease_released_unix_ns_present: bool,
    pub lease_sha256: String,
    pub checkpoint_path: String,
    pub checkpoint_complete_field: String,
    pub checkpoint_complete: bool,
    pub checkpoint_len_bytes: u64,
    pub checkpoint_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionIntentReadback {
    pub intent_id: String,
    pub transition: SetupHostTransitionKind,
    pub status: String,
    pub intent_path: String,
    pub prior_host_boot_id: String,
    pub current_host_boot_id: String,
    pub event_1074_record_id: Option<u64>,
    pub event_1074_sha256: Option<String>,
    pub event_1074_time_created_utc: Option<String>,
    pub event_1074_provider: Option<String>,
    /// Exact reason the transition could not be attributed to this intent.
    /// Present iff `status == "reconciliation_failed"`, in which case further
    /// planned host transitions are refused.
    pub reconciliation_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupHostTransitionResponse {
    pub action: SetupHostTransitionAction,
    pub source_of_truth: String,
    pub state_root: String,
    pub current_host_boot_id: String,
    #[cfg(windows)]
    pub host_boot_identity_evidence: crate::m4::HostBootIdentityEvidence,
    pub guard_config_file: FileReadback,
    pub durable_jobs: crate::m4::ShellJobHostTransitionSnapshot,
    pub guards: Vec<SetupHostTransitionGuardReadback>,
    pub authorized: bool,
    pub safety_digest: String,
    pub preflight_id: Option<String>,
    pub preflight_path: Option<String>,
    pub override_path: Option<String>,
    pub intent: Option<SetupHostTransitionIntentReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupResponse {
    pub operation: SetupOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SetupStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor: Option<SetupStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launchd_service: Option<SetupLaunchdServiceResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_transition: Option<SetupHostTransitionResponse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryOperation {
    Status,
}

impl TelemetryOperation {
    pub(super) const fn as_str(self) -> &'static str {
        "status"
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryStatusParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryParams {
    pub operation: TelemetryOperation,
    #[serde(default)]
    pub status: Option<TelemetryStatusParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentEventIngressStats {
    pub accepted_total: u64,
    pub rejected_unknown_spawn_total: u64,
    pub rejected_malformed_total: u64,
    pub rejected_storage_total: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsRecorderTelemetry {
    pub source_of_truth: &'static str,
    pub installed: bool,
    pub recorder: String,
    pub registry_metric_count: usize,
    pub rendered_bytes: usize,
    pub recorded_metric_names: Vec<String>,
    pub recorded_metric_samples: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolSurfacePayloadContributor {
    pub name: String,
    pub openai_tool_bytes: usize,
    pub input_schema_bytes: usize,
    pub description_bytes: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolSurfacePayloadTelemetry {
    pub source_of_truth: &'static str,
    pub tool_count: usize,
    pub openai_tools_bytes: usize,
    pub openai_tools_chars: usize,
    pub approx_tokens_chars_div_4: u64,
    pub approx_tokens_chars_div_3_5: u64,
    pub input_schema_bytes: usize,
    pub output_schema_bytes: usize,
    pub budget_openai_tools_bytes: usize,
    pub over_budget_by_bytes: usize,
    pub top_contributors: Vec<ToolSurfacePayloadContributor>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolSurfaceTelemetry {
    pub source_of_truth: &'static str,
    pub profile: String,
    pub profile_label: String,
    pub profile_source: String,
    pub visible_tool_count: usize,
    pub visible_public_tool_count: usize,
    pub implementation_tool_count: usize,
    pub hidden_implementation_tool_count: usize,
    pub public_tool_count: usize,
    pub max_public_tool_count: usize,
    pub over_public_tool_limit_by: usize,
    pub profile_gated_public_tool_count: usize,
    pub registered_public_tool_count: usize,
    pub missing_public_tool_count: usize,
    pub denied_break_glass_tool_count: usize,
    pub hidden_tool_route_count: usize,
    pub last_tool_surface_sha256: String,
    pub visible_tool_sha256: String,
    pub public_tool_sha256: String,
    pub facade_contract_sha256: String,
    pub facade_contract_tool_count: usize,
    pub facade_contract_operation_count: usize,
    pub facade_contract_mutating_operation_count: usize,
    pub model_payload: ToolSurfacePayloadTelemetry,
    pub codex_client_surface: CodexClientSurfaceSnapshot,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryStatusResponse {
    pub source_of_truth: &'static str,
    pub metrics_recorder: MetricsRecorderTelemetry,
    pub tool_surface: ToolSurfaceTelemetry,
    pub tool_usage: crate::daemon_lifecycle::ToolUsageTelemetry,
    pub storage_summary: StorageSummaryResponse,
    pub agent_event_ingress: AgentEventIngressStats,
    pub cf_row_counts: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryResponse {
    pub operation: TelemetryOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    pub status: TelemetryStatusResponse,
}
