use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::tool_profiles::CodexClientSurfaceSnapshot;

use crate::m3::{
    hygiene::{
        HygieneBlindSpotParams, HygieneBlindSpotResponse, HygieneDriftParams, HygieneDriftResponse,
        HygieneFlagsParams, HygieneFlagsResponse, HygieneGroundingGapParams,
        HygieneGroundingGapResponse, HygieneReportParams, HygieneReportResponse,
        HygieneScanStorageParams, HygieneScanStorageResponse, HygieneScanTextParams,
        HygieneScanTextResponse,
    },
    local_models::{
        LocalModelListParams, LocalModelListResponse, LocalModelProbeParams,
        LocalModelProbeResponse, LocalModelRegisterParams, LocalModelRegisterResponse,
        LocalModelRemoveParams, LocalModelRemoveResponse, LocalModelUpdateParams,
        LocalModelUpdateResponse,
    },
    storage::{
        StorageAnchorsParams, StorageAnchorsResponse, StorageBackupParams, StorageBackupResponse,
        StorageGcOnceParams, StorageGcOnceResponse, StorageInspectParams, StorageInspectResponse,
        StorageIntelligenceParams, StorageIntelligenceResponse, StorageRestoreVerifyParams,
        StorageRestoreVerifyResponse, StorageSearchRebuildParams, StorageSearchRebuildResponse,
        StorageSummaryResponse, StorageTemporalBackfillParams, StorageTemporalBackfillResponse,
        StorageTemporalPanelsParams, StorageTemporalPanelsResponse, StorageTemporalRerankParams,
        StorageTemporalRerankResponse,
    },
};
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageOperation {
    Inspect,
    Summary,
    GcOnce,
    Anchors,
    TemporalPanels,
    TemporalRerank,
    TemporalBackfill,
    SearchRebuild,
    Backup,
    RestoreVerify,
    Intelligence,
}

impl StorageOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Summary => "summary",
            Self::GcOnce => "gc_once",
            Self::Anchors => "anchors",
            Self::TemporalPanels => "temporal_panels",
            Self::TemporalRerank => "temporal_rerank",
            Self::TemporalBackfill => "temporal_backfill",
            Self::SearchRebuild => "search_rebuild",
            Self::Backup => "backup",
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
    pub gc_once: Option<StorageGcOnceParams>,
    #[serde(default)]
    pub anchors: Option<StorageAnchorsParams>,
    #[serde(default)]
    pub temporal_panels: Option<StorageTemporalPanelsParams>,
    #[serde(default)]
    pub temporal_rerank: Option<StorageTemporalRerankParams>,
    #[serde(default)]
    pub temporal_backfill: Option<StorageTemporalBackfillParams>,
    #[serde(default)]
    pub search_rebuild: Option<StorageSearchRebuildParams>,
    #[serde(default)]
    pub backup: Option<StorageBackupParams>,
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
    pub gc_once: Option<StorageGcOnceResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchors: Option<StorageAnchorsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_panels: Option<StorageTemporalPanelsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_rerank: Option<StorageTemporalRerankResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_backfill: Option<StorageTemporalBackfillResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_rebuild: Option<StorageSearchRebuildResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<StorageBackupResponse>,
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
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupOperation {
    Status,
    Doctor,
    Repair,
}

impl SetupOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Doctor => "doctor",
            Self::Repair => "repair",
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
