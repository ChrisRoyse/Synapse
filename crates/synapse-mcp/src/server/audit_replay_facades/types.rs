use rmcp::schemars::{self, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

use crate::m3::{
    audit_export::{AuditExportBundleParams, AuditExportBundleResponse},
    demo_recording::{
        DemoRecordStartParams, DemoRecordStartResponse, DemoRecordStatusResponse,
        DemoRecordStopParams, DemoRecordStopResponse,
    },
    profile_registry::{AuditIntelligenceQueryParams, AuditIntelligenceQueryResponse},
    replay::{ReplayRecordParams, ReplayRecordResponse},
};
use crate::server::ErrorData;

use super::{
    AUDIT_SOT, AUDIT_TOOL, DEFAULT_ARTIFACT_MAX_BYTES, DEFAULT_ARTIFACT_MAX_RECORDS,
    DEFAULT_LIFECYCLE_LIMIT, DEFAULT_MAX_LINE_BYTES, REPLAY_SOT, REPLAY_TOOL,
    errors::invalid_operation,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditOperation {
    CommandQuery,
    LifecycleEvents,
    LifecycleExits,
    ProfileIntelligence,
    ExportBundle,
}

impl AuditOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::CommandQuery => "command_query",
            Self::LifecycleEvents => "lifecycle_events",
            Self::LifecycleExits => "lifecycle_exits",
            Self::ProfileIntelligence => "profile_intelligence",
            Self::ExportBundle => "export_bundle",
        }
    }

    pub(super) fn parse(raw: &str) -> Result<Self, ErrorData> {
        match raw {
            "command_query" => Ok(Self::CommandQuery),
            "lifecycle_events" => Ok(Self::LifecycleEvents),
            "lifecycle_exits" => Ok(Self::LifecycleExits),
            "profile_intelligence" => Ok(Self::ProfileIntelligence),
            "export_bundle" => Ok(Self::ExportBundle),
            other => Err(invalid_operation(
                AUDIT_TOOL,
                other,
                &[
                    "command_query",
                    "lifecycle_events",
                    "lifecycle_exits",
                    "profile_intelligence",
                    "export_bundle",
                ],
                AUDIT_SOT,
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayOperation {
    Record,
    DemoStatus,
    DemoStart,
    DemoStop,
    ArtifactInspect,
}

impl ReplayOperation {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::DemoStatus => "demo_status",
            Self::DemoStart => "demo_start",
            Self::DemoStop => "demo_stop",
            Self::ArtifactInspect => "artifact_inspect",
        }
    }

    pub(super) fn parse(raw: &str) -> Result<Self, ErrorData> {
        match raw {
            "record" => Ok(Self::Record),
            "demo_status" => Ok(Self::DemoStatus),
            "demo_start" => Ok(Self::DemoStart),
            "demo_stop" => Ok(Self::DemoStop),
            "artifact_inspect" => Ok(Self::ArtifactInspect),
            other => Err(invalid_operation(
                REPLAY_TOOL,
                other,
                &[
                    "record",
                    "demo_status",
                    "demo_start",
                    "demo_stop",
                    "artifact_inspect",
                ],
                REPLAY_SOT,
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditParams {
    #[schemars(schema_with = "audit_operation_schema")]
    pub operation: String,
    #[serde(default)]
    pub command_query: Option<AuditCommandQueryParams>,
    #[serde(default)]
    pub lifecycle_events: Option<AuditLifecycleTailParams>,
    #[serde(default)]
    pub lifecycle_exits: Option<AuditLifecycleTailParams>,
    #[serde(default)]
    pub profile_intelligence: Option<AuditIntelligenceQueryParams>,
    #[serde(default)]
    pub export_bundle: Option<AuditExportBundleParams>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayParams {
    #[schemars(schema_with = "replay_operation_schema")]
    pub operation: String,
    #[serde(default)]
    pub record: Option<ReplayRecordParams>,
    #[serde(default)]
    pub demo_status: Option<DemoStatusParams>,
    #[serde(default)]
    pub demo_start: Option<DemoRecordStartParams>,
    #[serde(default)]
    pub demo_stop: Option<DemoRecordStopParams>,
    #[serde(default)]
    pub artifact_inspect: Option<ReplayArtifactInspectParams>,
}

fn audit_operation_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": [
            "command_query",
            "lifecycle_events",
            "lifecycle_exits",
            "profile_intelligence",
            "export_bundle"
        ]
    })
}

fn replay_operation_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": [
            "record",
            "demo_status",
            "demo_start",
            "demo_stop",
            "artifact_inspect"
        ]
    })
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditCommandQueryParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 250))]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 5000))]
    pub scan_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_kind: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditLifecycleTailParams {
    #[serde(default = "default_lifecycle_limit")]
    #[schemars(default = "default_lifecycle_limit", range(min = 1, max = 100))]
    pub limit: usize,
    #[serde(default = "default_max_line_bytes")]
    #[schemars(default = "default_max_line_bytes", range(min = 1, max = 524288))]
    pub max_line_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_kind: Option<String>,
}

impl Default for AuditLifecycleTailParams {
    fn default() -> Self {
        Self {
            limit: default_lifecycle_limit(),
            max_line_bytes: default_max_line_bytes(),
            tool: None,
            status: None,
            event_kind: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoStatusParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayArtifactInspectParams {
    pub path: String,
    #[serde(default = "default_artifact_max_bytes")]
    #[schemars(
        default = "default_artifact_max_bytes",
        range(min = 1, max = 134217728)
    )]
    pub max_bytes: u64,
    #[serde(default = "default_artifact_max_records")]
    #[schemars(default = "default_artifact_max_records", range(min = 0, max = 50000))]
    pub max_records: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditResponse {
    pub operation: AuditOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_query: Option<AuditCommandQueryResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_events: Option<AuditLifecycleTailResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_exits: Option<AuditLifecycleTailResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_intelligence: Option<AuditIntelligenceQueryResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_bundle: Option<AuditExportBundleResponse>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayResponse {
    pub operation: ReplayOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<ReplayRecordResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demo_status: Option<DemoRecordStatusResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demo_start: Option<DemoRecordStartResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demo_stop: Option<DemoRecordStopResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_readback: Option<ReplayArtifactInspectResponse>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditCommandQueryResponse {
    pub source_of_truth: String,
    pub cf_name: String,
    pub limit: usize,
    pub scan_limit: usize,
    pub scanned_rows: usize,
    pub matched_rows: usize,
    pub returned_count: usize,
    pub corrupt_row_count: usize,
    pub complete: bool,
    /// Iteration direction applied: `"newest_first"` (unwindowed default) or
    /// `"oldest_first"` (explicit forward paging). #1550.
    pub scan_order: String,
    /// True when matches older than this page exist. For newest-first this is an
    /// honest "more history available", not a failure; page older with
    /// `end_ts_ns = oldest_returned_ts_ns`.
    pub has_older: bool,
    /// Timestamp of the oldest returned row (newest-first only), for paging older.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_returned_ts_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_key_hex: Option<String>,
    pub rows: Vec<AuditCommandQueryRowSummary>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditCommandQueryRowSummary {
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub row_kind: String,
    pub audit_id: String,
    pub ts_ns: u64,
    pub phase: Option<String>,
    pub status: Option<String>,
    pub outcome: Option<String>,
    pub tool: String,
    pub verb: Option<String>,
    pub channel: Option<String>,
    pub error_code: Option<String>,
    pub payload_sha256: Option<String>,
    pub payload_truncated: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditLifecycleTailResponse {
    pub path: String,
    pub limit: usize,
    pub max_line_bytes: usize,
    pub total_lines_read: u64,
    pub matched_lines_seen: u64,
    pub oversized_lines_seen: u64,
    pub oversized_lines_skipped: u64,
    pub returned_count: usize,
    pub rows: Vec<AuditLifecycleRowSummary>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditLifecycleRowSummary {
    pub line_no: u64,
    pub raw_len_bytes: u64,
    pub raw_sha256: String,
    pub schema_version: Option<u64>,
    pub run_id: Option<String>,
    pub pid: Option<u64>,
    pub seq: Option<u64>,
    pub event_kind: Option<String>,
    pub tool: Option<String>,
    pub status: Option<String>,
    pub cause: Option<String>,
    pub started_at_unix_ms: Option<u64>,
    pub finished_at_unix_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub recorded_at_unix_ms: Option<u64>,
    pub mcp_session_id_present: bool,
    pub mcp_session_id_sha256: Option<String>,
    pub error_code: Option<String>,
    pub panic_present: bool,
    pub detail_code: Option<String>,
    pub in_flight_count: Option<u64>,
    pub last_tool: Option<String>,
    pub last_tool_status: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayArtifactInspectResponse {
    pub path: String,
    pub source_of_truth: String,
    pub exists: bool,
    pub bytes: u64,
    pub sha256: String,
    pub records_read: usize,
    pub max_records: usize,
    pub max_bytes: u64,
    pub empty: bool,
    pub lines: Vec<ReplayArtifactLineSummary>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayArtifactLineSummary {
    pub line_no: u64,
    pub len_bytes: u64,
    pub sha256: String,
    pub target: Option<String>,
    pub record_type: Option<String>,
    pub demo_id_present: bool,
    pub profile_id_present: bool,
}

const fn default_lifecycle_limit() -> usize {
    DEFAULT_LIFECYCLE_LIMIT
}

const fn default_max_line_bytes() -> usize {
    DEFAULT_MAX_LINE_BYTES
}

const fn default_artifact_max_bytes() -> u64 {
    DEFAULT_ARTIFACT_MAX_BYTES
}

const fn default_artifact_max_records() -> usize {
    DEFAULT_ARTIFACT_MAX_RECORDS
}
