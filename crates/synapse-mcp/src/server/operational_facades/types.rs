use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::m3::{
    hygiene::{
        HygieneFlagsParams, HygieneFlagsResponse, HygieneReportParams, HygieneReportResponse,
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
        StorageGcOnceParams, StorageGcOnceResponse, StorageInspectParams, StorageInspectResponse,
        StoragePutProbeRowsParams, StoragePutProbeRowsResponse, StorageSummaryResponse,
    },
};
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageOperation {
    Inspect,
    Summary,
    PutProbeRows,
    GcOnce,
}

impl StorageOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Summary => "summary",
            Self::PutProbeRows => "put_probe_rows",
            Self::GcOnce => "gc_once",
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
    pub put_probe_rows: Option<StoragePutProbeRowsParams>,
    #[serde(default)]
    pub gc_once: Option<StorageGcOnceParams>,
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
    pub put_probe_rows: Option<StoragePutProbeRowsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_once: Option<StorageGcOnceResponse>,
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
}

impl HygieneOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ScanText => "scan_text",
            Self::ScanStorage => "scan_storage",
            Self::Flags => "flags",
            Self::Report => "report",
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
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryStatusResponse {
    pub source_of_truth: &'static str,
    pub tool_surface: ToolSurfaceTelemetry,
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
