//! Run-scoped shared workspace blackboard tools (#796).
//!
//! The blackboard is the cooperative data plane for primary agents. Durable
//! truth is a `CF_KV` row keyed by run id + structured workspace key; SSE is
//! only the notification path.

use std::{
    fs::File,
    io::Read as _,
    path::Path,
    sync::{
        Arc, LazyLock, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::Utc;
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::{DataPredicate, Event, EventFilter, EventSource, error_codes};
use synapse_reflex::PublishReport;
use synapse_storage::{Db, cf};

use super::{
    ErrorData, Json, Parameters, SynapseService, mcp_error, session_registry::unix_time_ms_now,
    session_tools::validate_session_id, tool, tool_router,
};

const SCHEMA_VERSION: u32 = 1;
const WORKSPACE_PREFIX: &str = "workspace-blackboard/v1";
const WORKSPACE_PUT_EVENT_KIND: &str = "workspace.put";
const DEFAULT_WORKSPACE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_WORKSPACE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1000;
const MAX_RUN_ID_BYTES: usize = 128;
const MAX_KEY_BYTES: usize = 512;
const MAX_INLINE_VALUE_BYTES: usize = 256 * 1024;
const MAX_ARTIFACT_HANDLE_CHARS: usize = 1024;
const MAX_ARTIFACT_TEXT_CHARS: usize = 512;
const WORKSPACE_TOOL: &str = "workspace";
const WORKSPACE_KEY_ABSENT: &str = "WORKSPACE_KEY_ABSENT";
/// Detail/error code returned when a blocking `wait` reaches its deadline before
/// the key becomes present. Mirrors the `WORKSPACE_KEY_ABSENT` convention: a
/// workspace-local structured code string (not a `synapse_core::error_codes`
/// entry) so callers can branch on an expected, typed timeout rather than a
/// generic storage failure (#1552).
const WORKSPACE_WAIT_TIMEOUT: &str = "WORKSPACE_WAIT_TIMEOUT";
const WORKSPACE_SOURCE_OF_TRUTH: &str = "CF_KV workspace-blackboard exact row";
/// Default blocking budget for `wait` when the caller omits `timeout_ms`.
const DEFAULT_WORKSPACE_WAIT_TIMEOUT_MS: u64 = 5_000;
/// Lower/upper bounds for the `wait` blocking budget. `wait` blocks a request
/// thread with a bounded poll loop, so the ceiling is deliberately small; values
/// outside the range fail closed (rejected, never silently clamped).
const MIN_WORKSPACE_WAIT_TIMEOUT_MS: u64 = 1;
const MAX_WORKSPACE_WAIT_TIMEOUT_MS: u64 = 60_000;
/// Default and bounds for the `wait` poll cadence against CF_KV.
const DEFAULT_WORKSPACE_WAIT_POLL_INTERVAL_MS: u64 = 50;
const MIN_WORKSPACE_WAIT_POLL_INTERVAL_MS: u64 = 1;
const MAX_WORKSPACE_WAIT_POLL_INTERVAL_MS: u64 = 5_000;

static NEXT_WORKSPACE_EVENT_SEQ: AtomicU64 = AtomicU64::new(1);
static WORKSPACE_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOperation {
    Get,
    Put,
    List,
    Subscribe,
    Exists,
    Delete,
    Wait,
}

impl WorkspaceOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Put => "put",
            Self::List => "list",
            Self::Subscribe => "subscribe",
            Self::Exists => "exists",
            Self::Delete => "delete",
            Self::Wait => "wait",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceParams {
    pub operation: WorkspaceOperation,
    #[serde(default)]
    pub get: Option<WorkspaceGetParams>,
    #[serde(default)]
    pub put: Option<WorkspacePutParams>,
    #[serde(default)]
    pub list: Option<WorkspaceListParams>,
    #[serde(default)]
    pub subscribe: Option<WorkspaceSubscribeParams>,
    #[serde(default)]
    pub exists: Option<WorkspaceExistsParams>,
    #[serde(default)]
    pub delete: Option<WorkspaceDeleteParams>,
    #[serde(default)]
    pub wait: Option<WorkspaceWaitParams>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePutParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Structured key such as "findings/page-1/text" or "artifacts/shot-1".
    pub key: String,
    /// Required when replacing an existing key. Omit to create only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
    /// Small JSON value to store inline. Omit when publishing only an artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Optional large-artifact handle. If `path` is supplied, the file is read
    /// and size/hash are verified before the row is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<WorkspaceArtifactRef>,
    /// Retention in milliseconds. Expired rows are removed on get/list/put.
    #[serde(default = "default_workspace_ttl_ms")]
    #[schemars(
        default = "default_workspace_ttl_ms",
        range(min = 1, max = 604_800_000)
    )]
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGetParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub key: String,
    /// When true, an absent (or expired) key is a SUCCESS with `found=false` and
    /// an `absent_readback` proof from CF_KV, instead of the fail-closed
    /// `WORKSPACE_KEY_ABSENT` error. Expected-absence polling (a peer will write
    /// the key later) is a normal outcome, not a failure (#1552). Defaults to
    /// false, preserving the historical fail-closed behavior exactly.
    #[serde(default = "default_false")]
    #[schemars(default = "default_false")]
    pub absent_ok: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceWaitParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Structured key to block on until a peer publishes it via workspace put.
    pub key: String,
    /// Bounded blocking budget in milliseconds. CF_KV is polled until the key is
    /// present or this deadline elapses. Out-of-range values fail closed with a
    /// structured TOOL_PARAMS_INVALID error rather than being clamped.
    #[serde(default = "default_workspace_wait_timeout_ms")]
    #[schemars(
        default = "default_workspace_wait_timeout_ms",
        range(min = 1, max = 60_000)
    )]
    pub timeout_ms: u64,
    /// Poll cadence in milliseconds between CF_KV reads. The final sleep before
    /// the deadline is shortened so the loop never overshoots the budget by more
    /// than one interval.
    #[serde(default = "default_workspace_wait_poll_interval_ms")]
    #[schemars(
        default = "default_workspace_wait_poll_interval_ms",
        range(min = 1, max = 5_000)
    )]
    pub poll_interval_ms: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceListParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Key prefix to return. Empty means the whole run namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default = "default_list_limit")]
    #[schemars(default = "default_list_limit", range(min = 1, max = 1000))]
    pub limit: usize,
    /// Include inline JSON values in entries. Set false for a metadata-only scan.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub include_values: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSubscribeParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Non-empty key prefix, for example "findings/".
    pub prefix: String,
    #[serde(default = "default_false")]
    #[schemars(default = "default_false")]
    pub snapshot_first: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExistsParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteParams {
    /// Optional logical run id. Defaults to the current daemon lifecycle run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub key: String,
    /// Exact CF_KV row key for corrupt-row remediation. This is accepted only
    /// with expected_corrupt_sha256, must be under the resolved workspace run
    /// prefix, and is refused for decodable rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_row_key: Option<String>,
    /// Compare-and-delete guard for a decodable row. Read the row first, then
    /// pass its version so deletes cannot silently remove a concurrently
    /// updated row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
    /// Exact SHA-256 guard for a corrupt row reported by workspace list/get.
    /// This is mutually exclusive with expected_version and exists so corrupt
    /// rows can be manually remediated without broad arbitrary storage writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_corrupt_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceArtifactRef {
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_len: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEntry {
    pub schema_version: u32,
    pub run_id: String,
    pub key: String,
    pub row_key: String,
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<WorkspaceArtifactRef>,
    pub writer_session_id: String,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub ttl_ms: u64,
    pub expires_at_unix_ms: u64,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRowReadback {
    pub cf_name: String,
    pub row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAbsentReadback {
    pub cf_name: String,
    pub row_key: String,
    pub exists: bool,
    pub exact_match_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceArtifactReadback {
    pub path: String,
    pub exists: bool,
    pub is_file: bool,
    pub bytes_len: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEventPublishReport {
    pub event_kind: String,
    pub event_seq: u64,
    pub matched: usize,
    pub queued: usize,
    pub dropped: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePutResponse {
    pub ok: bool,
    pub run_id: String,
    pub key: String,
    pub row_key: String,
    pub writer_session_id: String,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<u64>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub expired_rows_deleted_before: usize,
    pub corrupt_rows_skipped_before: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_readback: Option<WorkspaceArtifactReadback>,
    pub storage_readback: WorkspaceRowReadback,
    pub event_publish_report: WorkspaceEventPublishReport,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGetResponse {
    pub ok: bool,
    pub run_id: String,
    pub key: String,
    pub now_unix_ms: u64,
    /// Unambiguous present/absent discriminator. True iff a live row was read;
    /// false only on the `absent_ok=true` tolerated-absence path.
    pub found: bool,
    /// The live row. Present iff `found` is true; omitted on tolerated absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<WorkspaceEntry>,
    /// Exact CF_KV row hash readback. Present iff `found` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_readback: Option<WorkspaceRowReadback>,
    /// CF_KV proof of absence for the exact row key. Present iff `found` is
    /// false (the `absent_ok=true` tolerated-absence path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_readback: Option<WorkspaceAbsentReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceWaitResponse {
    pub ok: bool,
    pub run_id: String,
    pub key: String,
    /// Always true on the success path: `wait` only returns Ok once the key is
    /// present; a deadline reached first returns the `WORKSPACE_WAIT_TIMEOUT`
    /// error instead.
    pub found: bool,
    pub now_unix_ms: u64,
    /// Wall-clock milliseconds elapsed from the first poll until the key resolved.
    pub waited_ms: u64,
    /// Number of CF_KV poll iterations performed before the key resolved.
    pub poll_count: u64,
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub entry: WorkspaceEntry,
    pub storage_readback: WorkspaceRowReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceListResponse {
    pub ok: bool,
    pub run_id: String,
    pub prefix: String,
    pub values_included: bool,
    pub now_unix_ms: u64,
    pub scanned_rows: usize,
    pub expired_rows_deleted: usize,
    pub corrupt_rows_skipped: Vec<WorkspaceCorruptRow>,
    pub returned_count: usize,
    pub entries: Vec<WorkspaceEntry>,
    pub readback_rows: Vec<WorkspaceRowReadback>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCorruptRow {
    pub row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub error: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSubscribeResponse {
    pub ok: bool,
    pub subscription_id: String,
    pub run_id: String,
    pub prefix: String,
    pub event_kind: String,
    pub started_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceExistenceState {
    Present,
    Absent,
    Expired,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExistsResponse {
    pub ok: bool,
    pub run_id: String,
    pub key: String,
    pub row_key: String,
    pub now_unix_ms: u64,
    pub exists: bool,
    pub physical_row_present: bool,
    pub state: WorkspaceExistenceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_readback: Option<WorkspaceRowReadback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_readback: Option<WorkspaceAbsentReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDeleteResponse {
    pub ok: bool,
    pub run_id: String,
    pub key: String,
    pub row_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_corrupt_row: Option<WorkspaceCorruptRow>,
    pub writer_session_id: String,
    pub deleted_row_readback: WorkspaceRowReadback,
    pub post_delete_readback: WorkspaceAbsentReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceResponse {
    pub operation: WorkspaceOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub get: Option<WorkspaceGetResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub put: Option<WorkspacePutResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<WorkspaceListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<WorkspaceSubscribeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<WorkspaceExistsResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<WorkspaceDeleteResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WorkspaceWaitResponse>,
}

#[derive(Clone)]
struct DecodedWorkspaceRow {
    key: Vec<u8>,
    encoded: Vec<u8>,
    entry: WorkspaceEntry,
}

struct WorkspaceRawRow {
    key: Vec<u8>,
    encoded: Vec<u8>,
}

#[derive(Default)]
struct WorkspaceCleanupReport {
    expired_keys: Vec<Vec<u8>>,
    corrupt_rows: Vec<WorkspaceCorruptRow>,
}

#[tool_router(router = workspace_blackboard_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for run-scoped workspace blackboard operations in the <=40 public MCP surface. operation is one of get, put, list, subscribe, exists, delete, or wait. Exactly one matching operation spec is accepted. Mutating operations return CF_KV or subscription readback metadata; absent keys are reported as WORKSPACE_KEY_ABSENT instead of generic storage corruption/read failures. get.absent_ok=true (default false) turns tolerated absence into a SUCCESS with found=false and a CF_KV absent_readback proof instead of the WORKSPACE_KEY_ABSENT error, for expected-absence polling. wait blocks until the key becomes present (returning the same entry/value readback as get) or its bounded timeout_ms elapses, in which case it fails with a typed WORKSPACE_WAIT_TIMEOUT error carrying key + waited_ms + poll_count."
    )]
    pub async fn workspace(
        &self,
        params: Parameters<WorkspaceParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<WorkspaceResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = WORKSPACE_TOOL,
            operation = params.0.operation.as_str(),
            "tool.invocation kind=workspace"
        );
        validate_workspace_facade_params(&params.0)?;
        let session_id = require_workspace_session_id(WORKSPACE_TOOL, &request_context)?;
        match params.0.operation {
            WorkspaceOperation::Get => {
                let response = self.workspace_get_impl(
                    params.0.get.ok_or_else(|| missing_workspace_spec("get"))?,
                    &session_id,
                )?;
                let readback = match response.storage_readback.as_ref() {
                    Some(storage_readback) => format!(
                        "CF_KV row={} bytes={} sha256={} found=true",
                        storage_readback.row_key,
                        storage_readback.value_len_bytes,
                        storage_readback.value_sha256
                    ),
                    None => {
                        let absent = response.absent_readback.as_ref();
                        format!(
                            "CF_KV row={} exact_match_count={} found=false",
                            absent.map_or("", |absent| absent.row_key.as_str()),
                            absent.map_or(0, |absent| absent.exact_match_count)
                        )
                    }
                };
                Ok(Json(workspace_response(
                    WorkspaceOperation::Get,
                    readback,
                    |out| out.get = Some(response),
                )))
            }
            WorkspaceOperation::Put => {
                let response = self.workspace_put_impl(
                    params.0.put.ok_or_else(|| missing_workspace_spec("put"))?,
                    &session_id,
                )?;
                Ok(Json(workspace_response(
                    WorkspaceOperation::Put,
                    format!(
                        "CF_KV row={} version={} bytes={} sha256={} event_seq={}",
                        response.storage_readback.row_key,
                        response.version,
                        response.storage_readback.value_len_bytes,
                        response.storage_readback.value_sha256,
                        response.event_publish_report.event_seq
                    ),
                    |out| out.put = Some(response),
                )))
            }
            WorkspaceOperation::List => {
                let response = self.workspace_list_impl(
                    params
                        .0
                        .list
                        .ok_or_else(|| missing_workspace_spec("list"))?,
                    &session_id,
                )?;
                Ok(Json(workspace_response(
                    WorkspaceOperation::List,
                    format!(
                        "CF_KV run={} prefix={} returned={} corrupt_rows={}",
                        response.run_id,
                        response.prefix,
                        response.returned_count,
                        response.corrupt_rows_skipped.len()
                    ),
                    |out| out.list = Some(response),
                )))
            }
            WorkspaceOperation::Subscribe => {
                let response = self.workspace_subscribe_impl(
                    params
                        .0
                        .subscribe
                        .ok_or_else(|| missing_workspace_spec("subscribe"))?,
                    &session_id,
                )?;
                Ok(Json(workspace_response(
                    WorkspaceOperation::Subscribe,
                    format!(
                        "SSE subscription_id={} event_kind={} run={} prefix={}",
                        response.subscription_id,
                        response.event_kind,
                        response.run_id,
                        response.prefix
                    ),
                    |out| out.subscribe = Some(response),
                )))
            }
            WorkspaceOperation::Exists => {
                let response = self.workspace_exists_impl(
                    params
                        .0
                        .exists
                        .ok_or_else(|| missing_workspace_spec("exists"))?,
                    &session_id,
                )?;
                Ok(Json(workspace_response(
                    WorkspaceOperation::Exists,
                    format!(
                        "CF_KV row={} state={:?} exists={} physical_row_present={}",
                        response.row_key,
                        response.state,
                        response.exists,
                        response.physical_row_present
                    ),
                    |out| out.exists = Some(response),
                )))
            }
            WorkspaceOperation::Delete => {
                let response = self.workspace_delete_impl(
                    params
                        .0
                        .delete
                        .ok_or_else(|| missing_workspace_spec("delete"))?,
                    &session_id,
                )?;
                Ok(Json(workspace_response(
                    WorkspaceOperation::Delete,
                    format!(
                        "CF_KV row={} deleted_version={:?} deleted_corrupt={} after_exists={}",
                        response.row_key,
                        response.deleted_version,
                        response.deleted_corrupt_row.is_some(),
                        response.post_delete_readback.exists
                    ),
                    |out| out.delete = Some(response),
                )))
            }
            WorkspaceOperation::Wait => {
                let response = self.workspace_wait_impl(
                    params
                        .0
                        .wait
                        .ok_or_else(|| missing_workspace_spec("wait"))?,
                    &session_id,
                )?;
                Ok(Json(workspace_response(
                    WorkspaceOperation::Wait,
                    format!(
                        "CF_KV row={} bytes={} sha256={} waited_ms={} poll_count={}",
                        response.storage_readback.row_key,
                        response.storage_readback.value_len_bytes,
                        response.storage_readback.value_sha256,
                        response.waited_ms,
                        response.poll_count
                    ),
                    |out| out.wait = Some(response),
                )))
            }
        }
    }

    #[tool(
        description = "Publish one run-scoped blackboard entry into durable CF_KV storage, with optional inline JSON and artifact handle. Creates fail closed if the key already exists unless expected_version matches the current row. If artifact.path is provided, Synapse reads the file and verifies size/hash before accepting the row. The write is accepted only after an exact CF_KV row readback, then a workspace.put SSE event is published."
    )]
    pub async fn workspace_put(
        &self,
        params: Parameters<WorkspacePutParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<WorkspacePutResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "workspace_put",
            "tool.invocation kind=workspace_put"
        );
        let session_id = require_workspace_session_id("workspace_put", &request_context)?;
        self.workspace_put_impl(params.0, &session_id).map(Json)
    }

    #[tool(
        description = "Read one run-scoped blackboard entry by key from durable CF_KV storage. Missing, expired, or corrupt exact rows fail closed with structured error data; the returned entry includes exact row hash readback."
    )]
    pub async fn workspace_get(
        &self,
        params: Parameters<WorkspaceGetParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<WorkspaceGetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "workspace_get",
            "tool.invocation kind=workspace_get"
        );
        let session_id = require_workspace_session_id("workspace_get", &request_context)?;
        self.workspace_get_impl(params.0, &session_id).map(Json)
    }

    #[tool(
        description = "List run-scoped blackboard entries from durable CF_KV storage. Scans are isolated: one corrupt row is reported and skipped rather than breaking reads of other keys."
    )]
    pub async fn workspace_list(
        &self,
        params: Parameters<WorkspaceListParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<WorkspaceListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "workspace_list",
            "tool.invocation kind=workspace_list"
        );
        let session_id = require_workspace_session_id("workspace_list", &request_context)?;
        self.workspace_list_impl(params.0, &session_id).map(Json)
    }

    #[tool(
        description = "Create a per-session SSE subscription for workspace.put events in one run and key prefix. The response returns the subscription id; read it through the HTTP SSE events endpoint."
    )]
    pub async fn workspace_subscribe(
        &self,
        params: Parameters<WorkspaceSubscribeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<WorkspaceSubscribeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "workspace_subscribe",
            "tool.invocation kind=workspace_subscribe"
        );
        let session_id = require_workspace_session_id("workspace_subscribe", &request_context)?;
        self.workspace_subscribe_impl(params.0, &session_id)
            .map(Json)
    }
}

impl SynapseService {
    pub(crate) fn dashboard_workspace_list_snapshot(
        &self,
        prefix: Option<String>,
        limit: usize,
        include_values: bool,
    ) -> Result<Value, ErrorData> {
        dashboard_json_readback(self.workspace_list_impl(
            WorkspaceListParams {
                run_id: None,
                prefix,
                limit,
                include_values,
            },
            "dashboard-context",
        )?)
    }

    pub(crate) fn dashboard_workspace_put(
        &self,
        key: String,
        expected_version: Option<u64>,
        value: Value,
    ) -> Result<Value, ErrorData> {
        dashboard_json_readback(self.workspace_put_impl(
            WorkspacePutParams {
                run_id: None,
                key,
                expected_version,
                value: Some(value),
                artifact: None,
                ttl_ms: DEFAULT_WORKSPACE_TTL_MS,
            },
            "dashboard-context",
        )?)
    }

    fn workspace_put_impl(
        &self,
        params: WorkspacePutParams,
        writer_session_id: &str,
    ) -> Result<WorkspacePutResponse, ErrorData> {
        self.workspace_put_impl_at(params, writer_session_id, unix_time_ms_now())
    }

    fn workspace_put_impl_at(
        &self,
        params: WorkspacePutParams,
        writer_session_id: &str,
        now_unix_ms: u64,
    ) -> Result<WorkspacePutResponse, ErrorData> {
        validate_session_id(writer_session_id)?;
        let request_params = params.clone();
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let key = normalize_workspace_key(&params.key)?;
        let expected_version = params.expected_version;
        validate_workspace_ttl_ms(params.ttl_ms)?;
        validate_inline_value_size(params.value.as_ref())?;
        if params.value.is_none() && params.artifact.is_none() {
            return Err(params_error(
                "workspace_put requires at least one of value or artifact",
            ));
        }
        let (artifact, artifact_readback) = match params.artifact {
            Some(artifact) => {
                let (normalized, readback) = validate_workspace_artifact(artifact)?;
                (Some(normalized), readback)
            }
            None => (None, None),
        };

        let _write_guard = workspace_write_lock()?;
        let db = self.workspace_db()?;
        let cleanup = cleanup_expired_workspace_rows(&db, &run_id, now_unix_ms)?;
        delete_workspace_rows(
            &db,
            cleanup.expired_keys.clone(),
            "delete expired workspace rows",
        )?;

        let row_key = workspace_row_key(&run_id, &key);
        let existing = read_workspace_row_optional(&db, &row_key, now_unix_ms)?;
        let previous_version = existing.as_ref().map(|row| row.entry.version);
        validate_workspace_expected_version(
            expected_version,
            previous_version,
            &run_id,
            &key,
            &row_key,
        )?;
        let (created_at_unix_ms, version) = existing.as_ref().map_or((now_unix_ms, 1), |row| {
            (
                row.entry.created_at_unix_ms,
                row.entry.version.saturating_add(1),
            )
        });
        let command_payload = json!({
            "run_id": &request_params.run_id,
            "resolved_run_id": &run_id,
            "key": &request_params.key,
            "normalized_key": &key,
            "expected_version": request_params.expected_version,
            "value": &request_params.value,
            "artifact": &request_params.artifact,
            "ttl_ms": request_params.ttl_ms,
        });
        let command_before = json!({
            "source_of_truth": cf::CF_KV,
            "row_key": &row_key,
            "had_existing_row": existing.is_some(),
            "previous_version": previous_version,
            "expired_rows_deleted_before": cleanup.expired_keys.len(),
            "corrupt_rows_skipped_before": cleanup.corrupt_rows.len(),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "workspace_put",
            "plan_edit",
            Some(writer_session_id.to_owned()),
            Some(writer_session_id.to_owned()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let entry = WorkspaceEntry {
            schema_version: SCHEMA_VERSION,
            run_id: run_id.clone(),
            key: key.clone(),
            row_key: row_key.clone(),
            value: params.value,
            artifact,
            writer_session_id: writer_session_id.to_owned(),
            created_at_unix_ms,
            updated_at_unix_ms: now_unix_ms,
            ttl_ms: params.ttl_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(params.ttl_ms),
            version,
        };
        let encoded = encode_workspace_entry(&entry)?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("write workspace blackboard row {row_key}: {error}"),
                )
            })?;
        let storage_readback = readback_exact_workspace_row(&db, &row_key)?;
        let event_publish_report = match self.publish_workspace_put_event(&entry, &storage_readback)
        {
            Ok(report) => report,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "workspace_put",
                        "plan_edit",
                        Some(writer_session_id.to_owned()),
                        Some(writer_session_id.to_owned()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": cf::CF_KV,
                            "row_key": &row_key,
                            "version": version,
                            "storage_readback": &storage_readback,
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                return Err(error);
            }
        };

        tracing::info!(
            code = "WORKSPACE_BLACKBOARD_PUT_COMMITTED",
            run_id,
            key,
            row_key,
            writer_session_id,
            version,
            value_sha256 = %storage_readback.value_sha256,
            event_matched = event_publish_report.matched,
            event_queued = event_publish_report.queued,
            expired_rows_deleted_before = cleanup.expired_keys.len(),
            corrupt_rows_skipped_before = cleanup.corrupt_rows.len(),
            "readback=workspace_blackboard edge=put_committed"
        );

        let response = WorkspacePutResponse {
            ok: true,
            run_id,
            key,
            row_key,
            writer_session_id: writer_session_id.to_owned(),
            version,
            previous_version,
            created_at_unix_ms,
            updated_at_unix_ms: now_unix_ms,
            expires_at_unix_ms: entry.expires_at_unix_ms,
            expired_rows_deleted_before: cleanup.expired_keys.len(),
            corrupt_rows_skipped_before: cleanup.corrupt_rows.len(),
            artifact_readback,
            storage_readback,
            event_publish_report,
        };
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "workspace_put",
            "plan_edit",
            Some(writer_session_id.to_owned()),
            Some(writer_session_id.to_owned()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": cf::CF_KV,
                "row_key": &response.row_key,
                "version": response.version,
                "previous_version": response.previous_version,
                "storage_readback": &response.storage_readback,
                "event_publish_report": &response.event_publish_report,
            }),
            "ok",
        ))?;
        Ok(response)
    }

    fn workspace_get_impl(
        &self,
        params: WorkspaceGetParams,
        session_id: &str,
    ) -> Result<WorkspaceGetResponse, ErrorData> {
        self.workspace_get_impl_at(params, session_id, unix_time_ms_now())
    }

    fn workspace_get_impl_at(
        &self,
        params: WorkspaceGetParams,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<WorkspaceGetResponse, ErrorData> {
        validate_session_id(session_id)?;
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let key = normalize_workspace_key(&params.key)?;
        let db = self.workspace_db()?;
        let row_key = workspace_row_key(&run_id, &key);
        let Some(row) = read_workspace_row_optional(&db, &row_key, now_unix_ms)? else {
            // Truly missing exact row. Under absent_ok this is a tolerated,
            // successful absence; otherwise it stays the historical fail-closed
            // WORKSPACE_KEY_ABSENT error (#1552).
            if params.absent_ok {
                return self.workspace_get_absent_ok_response(
                    &db,
                    run_id,
                    key,
                    &row_key,
                    now_unix_ms,
                    session_id,
                    "get_absent_ok_missing",
                );
            }
            return Err(workspace_missing_error(&run_id, &key, &row_key));
        };
        if row.entry.expires_at_unix_ms <= now_unix_ms {
            delete_workspace_rows(
                &db,
                vec![row.key.clone()],
                "delete expired workspace row on get",
            )?;
            // An expired row is semantically absent once deleted. absent_ok
            // callers (e.g. pollers) treat it as not-yet-present rather than an
            // error; fail-closed callers keep the typed expiry error.
            if params.absent_ok {
                return self.workspace_get_absent_ok_response(
                    &db,
                    run_id,
                    key,
                    &row_key,
                    now_unix_ms,
                    session_id,
                    "get_absent_ok_expired",
                );
            }
            return Err(workspace_expired_error(
                &run_id,
                &key,
                &row_key,
                row.entry.expires_at_unix_ms,
                now_unix_ms,
            ));
        }
        let storage_readback = WorkspaceRowReadback {
            cf_name: cf::CF_KV.to_owned(),
            row_key: row.entry.row_key.clone(),
            value_len_bytes: row.encoded.len() as u64,
            value_sha256: hash_bytes(&row.encoded),
        };
        tracing::info!(
            code = "WORKSPACE_BLACKBOARD_GET_READ",
            run_id,
            key,
            row_key,
            reader_session_id = session_id,
            version = row.entry.version,
            value_sha256 = %storage_readback.value_sha256,
            "readback=workspace_blackboard edge=get_read"
        );
        Ok(WorkspaceGetResponse {
            ok: true,
            run_id,
            key,
            now_unix_ms,
            found: true,
            entry: Some(row.entry),
            storage_readback: Some(storage_readback),
            absent_readback: None,
        })
    }

    /// Build the tolerated-absence success response for `get` with
    /// `absent_ok=true`, attaching the CF_KV proof-of-absence readback.
    fn workspace_get_absent_ok_response(
        &self,
        db: &Db,
        run_id: String,
        key: String,
        row_key: &str,
        now_unix_ms: u64,
        session_id: &str,
        edge: &'static str,
    ) -> Result<WorkspaceGetResponse, ErrorData> {
        let absent_readback = readback_absent_workspace_row(db, row_key)?;
        tracing::info!(
            code = "WORKSPACE_BLACKBOARD_GET_ABSENT_OK",
            run_id,
            key,
            row_key,
            reader_session_id = session_id,
            exact_match_count = absent_readback.exact_match_count,
            edge,
            "readback=workspace_blackboard edge=get_absent_ok"
        );
        Ok(WorkspaceGetResponse {
            ok: true,
            run_id,
            key,
            now_unix_ms,
            found: false,
            entry: None,
            storage_readback: None,
            absent_readback: Some(absent_readback),
        })
    }

    fn workspace_wait_impl(
        &self,
        params: WorkspaceWaitParams,
        session_id: &str,
    ) -> Result<WorkspaceWaitResponse, ErrorData> {
        validate_session_id(session_id)?;
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let key = normalize_workspace_key(&params.key)?;
        validate_workspace_wait_timeout_ms(params.timeout_ms)?;
        validate_workspace_wait_poll_interval_ms(params.poll_interval_ms)?;
        let db = self.workspace_db()?;
        let row_key = workspace_row_key(&run_id, &key);
        let timeout_ms = params.timeout_ms;
        let poll_interval_ms = params.poll_interval_ms;
        let started_at_unix_ms = unix_time_ms_now();
        let deadline_unix_ms = started_at_unix_ms.saturating_add(timeout_ms);
        tracing::info!(
            code = "WORKSPACE_BLACKBOARD_WAIT_STARTED",
            run_id,
            key,
            row_key,
            waiter_session_id = session_id,
            timeout_ms,
            poll_interval_ms,
            deadline_unix_ms,
            "readback=workspace_blackboard edge=wait_started"
        );
        let mut poll_count: u64 = 0;
        loop {
            poll_count = poll_count.saturating_add(1);
            let now_unix_ms = unix_time_ms_now();
            if let Some(row) = read_workspace_row_optional(&db, &row_key, now_unix_ms)? {
                if row.entry.expires_at_unix_ms > now_unix_ms {
                    let storage_readback = workspace_row_readback(&row);
                    let waited_ms = now_unix_ms.saturating_sub(started_at_unix_ms);
                    tracing::info!(
                        code = "WORKSPACE_BLACKBOARD_WAIT_RESOLVED",
                        run_id,
                        key,
                        row_key,
                        waiter_session_id = session_id,
                        version = row.entry.version,
                        poll_count,
                        waited_ms,
                        value_sha256 = %storage_readback.value_sha256,
                        "readback=workspace_blackboard edge=wait_resolved"
                    );
                    return Ok(WorkspaceWaitResponse {
                        ok: true,
                        run_id,
                        key,
                        found: true,
                        now_unix_ms,
                        waited_ms,
                        poll_count,
                        timeout_ms,
                        poll_interval_ms,
                        entry: row.entry,
                        storage_readback,
                    });
                }
                // A physically present but expired row is not a resolution;
                // delete it (source-of-truth cleanup) and keep polling.
                delete_workspace_rows(
                    &db,
                    vec![row.key.clone()],
                    "delete expired workspace row on wait",
                )?;
            }
            let now_unix_ms = unix_time_ms_now();
            if now_unix_ms >= deadline_unix_ms {
                let waited_ms = now_unix_ms.saturating_sub(started_at_unix_ms);
                let absent_readback = readback_absent_workspace_row(&db, &row_key)?;
                tracing::warn!(
                    code = WORKSPACE_WAIT_TIMEOUT,
                    run_id,
                    key,
                    row_key,
                    waiter_session_id = session_id,
                    poll_count,
                    waited_ms,
                    timeout_ms,
                    "readback=workspace_blackboard edge=wait_timeout"
                );
                return Err(workspace_wait_timeout_error(
                    &run_id,
                    &key,
                    &row_key,
                    timeout_ms,
                    waited_ms,
                    poll_count,
                    &absent_readback,
                ));
            }
            // Sleep for the poll interval, but never past the deadline (shorten
            // the final nap so the effective wait stays within one interval of
            // the requested budget). now_unix_ms < deadline here, so remaining
            // is always >= 1.
            let remaining_ms = deadline_unix_ms.saturating_sub(now_unix_ms);
            let sleep_ms = poll_interval_ms.min(remaining_ms).max(1);
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
    }

    fn workspace_list_impl(
        &self,
        params: WorkspaceListParams,
        session_id: &str,
    ) -> Result<WorkspaceListResponse, ErrorData> {
        self.workspace_list_impl_at(params, session_id, unix_time_ms_now())
    }

    fn workspace_list_impl_at(
        &self,
        params: WorkspaceListParams,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<WorkspaceListResponse, ErrorData> {
        validate_session_id(session_id)?;
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let prefix =
            normalize_workspace_prefix(params.prefix.as_deref().unwrap_or_default(), true)?;
        validate_workspace_list_limit(params.limit)?;
        let db = self.workspace_db()?;
        let scan = scan_workspace_run(&db, &run_id, now_unix_ms)?;
        delete_workspace_rows(
            &db,
            scan.expired_keys.clone(),
            "delete expired workspace rows on list",
        )?;

        let mut rows = scan
            .rows
            .into_iter()
            .filter(|row| row.entry.key.starts_with(&prefix))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.entry.key.cmp(&right.entry.key));
        if rows.len() > params.limit {
            rows.truncate(params.limit);
        }
        let readback_rows = rows
            .iter()
            .map(|row| WorkspaceRowReadback {
                cf_name: cf::CF_KV.to_owned(),
                row_key: row.entry.row_key.clone(),
                value_len_bytes: row.encoded.len() as u64,
                value_sha256: hash_bytes(&row.encoded),
            })
            .collect::<Vec<_>>();
        let entries = rows
            .into_iter()
            .map(|mut row| {
                if !params.include_values {
                    row.entry.value = None;
                }
                row.entry
            })
            .collect::<Vec<_>>();

        tracing::info!(
            code = "WORKSPACE_BLACKBOARD_LIST_READ",
            run_id,
            prefix,
            reader_session_id = session_id,
            scanned_rows = scan.scanned_rows,
            expired_rows_deleted = scan.expired_keys.len(),
            corrupt_rows_skipped = scan.corrupt_rows.len(),
            returned_count = entries.len(),
            "readback=workspace_blackboard edge=list_read"
        );

        Ok(WorkspaceListResponse {
            ok: true,
            run_id,
            prefix,
            values_included: params.include_values,
            now_unix_ms,
            scanned_rows: scan.scanned_rows,
            expired_rows_deleted: scan.expired_keys.len(),
            corrupt_rows_skipped: scan.corrupt_rows,
            returned_count: entries.len(),
            entries,
            readback_rows,
        })
    }

    fn workspace_subscribe_impl(
        &self,
        params: WorkspaceSubscribeParams,
        session_id: &str,
    ) -> Result<WorkspaceSubscribeResponse, ErrorData> {
        validate_session_id(session_id)?;
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let prefix = normalize_workspace_prefix(&params.prefix, false)?;
        let filter = EventFilter::And {
            args: vec![
                EventFilter::Data {
                    path: "/run_id".to_owned(),
                    predicate: DataPredicate::Eq {
                        value: Value::String(run_id.clone()),
                    },
                },
                EventFilter::Data {
                    path: "/key".to_owned(),
                    predicate: DataPredicate::Regex {
                        pattern: format!("^{}", regex::escape(&prefix)),
                    },
                },
            ],
        };
        let started_at_unix_ms = unix_time_ms_now();
        let subscription_id = self
            .sse_state()?
            .subscribe(
                filter,
                vec![WORKSPACE_PUT_EVENT_KIND.to_owned()],
                params.snapshot_first,
                Some(session_id.to_owned()),
            )
            .map_err(|error| mcp_error(error.code(), error.message()))?;
        tracing::info!(
            code = "WORKSPACE_BLACKBOARD_SUBSCRIBE_REGISTERED",
            run_id,
            prefix,
            subscription_id,
            owner_session_id = session_id,
            "readback=workspace_blackboard edge=subscribe_registered"
        );
        Ok(WorkspaceSubscribeResponse {
            ok: true,
            subscription_id,
            run_id,
            prefix,
            event_kind: WORKSPACE_PUT_EVENT_KIND.to_owned(),
            started_at_unix_ms,
        })
    }

    fn workspace_exists_impl(
        &self,
        params: WorkspaceExistsParams,
        session_id: &str,
    ) -> Result<WorkspaceExistsResponse, ErrorData> {
        self.workspace_exists_impl_at(params, session_id, unix_time_ms_now())
    }

    fn workspace_exists_impl_at(
        &self,
        params: WorkspaceExistsParams,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<WorkspaceExistsResponse, ErrorData> {
        validate_session_id(session_id)?;
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let key = normalize_workspace_key(&params.key)?;
        let db = self.workspace_db()?;
        let row_key = workspace_row_key(&run_id, &key);
        match read_workspace_row_optional(&db, &row_key, now_unix_ms)? {
            Some(row) if row.entry.expires_at_unix_ms > now_unix_ms => {
                let storage_readback = workspace_row_readback(&row);
                Ok(WorkspaceExistsResponse {
                    ok: true,
                    run_id,
                    key,
                    row_key,
                    now_unix_ms,
                    exists: true,
                    physical_row_present: true,
                    state: WorkspaceExistenceState::Present,
                    current_version: Some(row.entry.version),
                    expires_at_unix_ms: Some(row.entry.expires_at_unix_ms),
                    storage_readback: Some(storage_readback),
                    absent_readback: None,
                })
            }
            Some(row) => {
                let storage_readback = workspace_row_readback(&row);
                Ok(WorkspaceExistsResponse {
                    ok: true,
                    run_id,
                    key,
                    row_key,
                    now_unix_ms,
                    exists: false,
                    physical_row_present: true,
                    state: WorkspaceExistenceState::Expired,
                    current_version: Some(row.entry.version),
                    expires_at_unix_ms: Some(row.entry.expires_at_unix_ms),
                    storage_readback: Some(storage_readback),
                    absent_readback: None,
                })
            }
            None => Ok(WorkspaceExistsResponse {
                ok: true,
                run_id,
                key,
                row_key: row_key.clone(),
                now_unix_ms,
                exists: false,
                physical_row_present: false,
                state: WorkspaceExistenceState::Absent,
                current_version: None,
                expires_at_unix_ms: None,
                storage_readback: None,
                absent_readback: Some(readback_absent_workspace_row(&db, &row_key)?),
            }),
        }
    }

    fn workspace_delete_impl(
        &self,
        params: WorkspaceDeleteParams,
        session_id: &str,
    ) -> Result<WorkspaceDeleteResponse, ErrorData> {
        self.workspace_delete_impl_at(params, session_id, unix_time_ms_now())
    }

    fn workspace_delete_impl_at(
        &self,
        params: WorkspaceDeleteParams,
        session_id: &str,
        _now_unix_ms: u64,
    ) -> Result<WorkspaceDeleteResponse, ErrorData> {
        validate_session_id(session_id)?;
        let run_id = resolve_workspace_run_id(params.run_id.as_deref())?;
        let key = normalize_workspace_key(&params.key)?;
        let row_key = match params.raw_row_key.as_deref() {
            Some(raw_row_key) => validate_workspace_raw_delete_row_key(&run_id, raw_row_key)?,
            None => workspace_row_key(&run_id, &key),
        };
        let _write_guard = workspace_write_lock()?;
        let db = self.workspace_db()?;
        let raw_row = read_workspace_raw_row_optional(&db, &row_key)?
            .ok_or_else(|| workspace_missing_error(&run_id, &key, &row_key))?;
        let deleted_row_readback = WorkspaceRowReadback {
            cf_name: cf::CF_KV.to_owned(),
            row_key: row_key.clone(),
            value_len_bytes: raw_row.encoded.len() as u64,
            value_sha256: hash_bytes(&raw_row.encoded),
        };
        let (deleted_version, deleted_corrupt_row) =
            match decode_workspace_row(raw_row.key.clone(), raw_row.encoded.clone()) {
                Ok(row) => {
                    validate_workspace_delete_version_guard(
                        params.expected_version,
                        params.expected_corrupt_sha256.as_deref(),
                        params.raw_row_key.as_deref(),
                        row.entry.version,
                        &run_id,
                        &key,
                        &row_key,
                    )?;
                    (Some(row.entry.version), None)
                }
                Err(error) => {
                    validate_workspace_delete_corrupt_guard(
                        params.expected_version,
                        params.expected_corrupt_sha256.as_deref(),
                        params.raw_row_key.as_deref(),
                        &deleted_row_readback,
                        &run_id,
                        &key,
                    )?;
                    (
                        None,
                        Some(WorkspaceCorruptRow {
                            row_key: row_key.clone(),
                            value_len_bytes: deleted_row_readback.value_len_bytes,
                            value_sha256: deleted_row_readback.value_sha256.clone(),
                            error,
                        }),
                    )
                }
            };
        let command_payload = json!({
            "run_id": &params.run_id,
            "resolved_run_id": &run_id,
            "key": &params.key,
            "normalized_key": &key,
            "raw_row_key": &params.raw_row_key,
            "expected_version": params.expected_version,
            "expected_corrupt_sha256": &params.expected_corrupt_sha256,
        });
        let command_before = json!({
            "source_of_truth": cf::CF_KV,
            "row_key": &row_key,
            "current_version": deleted_version,
            "deleted_corrupt_row": &deleted_corrupt_row,
            "deleted_row_readback": &deleted_row_readback,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "workspace",
            "delete",
            Some(session_id.to_owned()),
            Some(session_id.to_owned()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        delete_workspace_rows(&db, vec![raw_row.key], "delete exact workspace row")?;
        let post_delete_readback = readback_absent_workspace_row(&db, &row_key)?;
        if post_delete_readback.exists {
            let error = mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("workspace delete readback still found row {row_key}"),
            );
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "workspace",
                    "delete",
                    Some(session_id.to_owned()),
                    Some(session_id.to_owned()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": cf::CF_KV,
                        "row_key": &row_key,
                        "post_delete_readback": &post_delete_readback,
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&error),
                ),
            )?;
            return Err(error);
        }
        let response = WorkspaceDeleteResponse {
            ok: true,
            run_id,
            key,
            row_key,
            deleted_version,
            deleted_corrupt_row,
            writer_session_id: session_id.to_owned(),
            deleted_row_readback,
            post_delete_readback,
        };
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "workspace",
            "delete",
            Some(session_id.to_owned()),
            Some(session_id.to_owned()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": cf::CF_KV,
                "row_key": &response.row_key,
                "deleted_version": response.deleted_version,
                "deleted_corrupt_row": &response.deleted_corrupt_row,
                "post_delete_readback": &response.post_delete_readback,
            }),
            "ok",
        ))?;
        Ok(response)
    }

    fn workspace_db(&self) -> Result<Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening workspace blackboard storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn publish_workspace_put_event(
        &self,
        entry: &WorkspaceEntry,
        readback: &WorkspaceRowReadback,
    ) -> Result<WorkspaceEventPublishReport, ErrorData> {
        let event_seq = NEXT_WORKSPACE_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
        let event = Event {
            seq: event_seq,
            at: Utc::now(),
            source: EventSource::System,
            kind: WORKSPACE_PUT_EVENT_KIND.to_owned(),
            data: json!({
                "run_id": entry.run_id,
                "key": entry.key,
                "row_key": entry.row_key,
                "writer_session_id": entry.writer_session_id,
                "version": entry.version,
                "previous_version": if entry.version > 1 { Some(entry.version - 1) } else { None },
                "updated_at_unix_ms": entry.updated_at_unix_ms,
                "expires_at_unix_ms": entry.expires_at_unix_ms,
                "has_value": entry.value.is_some(),
                "artifact_handle": entry.artifact.as_ref().map(|artifact| artifact.handle.clone()),
                "cf_name": readback.cf_name,
                "value_len_bytes": readback.value_len_bytes,
                "value_sha256": readback.value_sha256,
            }),
            correlations: Vec::new(),
        };
        let report = self.sse_state()?.event_bus().publish(event);
        Ok(workspace_publish_report(event_seq, report))
    }
}

struct WorkspaceRunScan {
    scanned_rows: usize,
    expired_keys: Vec<Vec<u8>>,
    corrupt_rows: Vec<WorkspaceCorruptRow>,
    rows: Vec<DecodedWorkspaceRow>,
}

fn dashboard_json_readback(value: impl Serialize) -> Result<Value, ErrorData> {
    serde_json::to_value(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("serialize dashboard workspace readback: {error}"),
        )
    })
}

fn scan_workspace_run(
    db: &Db,
    run_id: &str,
    now_unix_ms: u64,
) -> Result<WorkspaceRunScan, ErrorData> {
    let prefix = workspace_run_prefix(run_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let scanned_rows = rows.len();
    let mut expired_keys = Vec::new();
    let mut corrupt_rows = Vec::new();
    let mut decoded_rows = Vec::new();
    for (key, encoded) in rows {
        match decode_workspace_row(key.clone(), encoded.clone()) {
            Ok(row) if row.entry.expires_at_unix_ms <= now_unix_ms => expired_keys.push(key),
            Ok(row) => decoded_rows.push(row),
            Err(error) => corrupt_rows.push(WorkspaceCorruptRow {
                row_key: String::from_utf8_lossy(&key).to_string(),
                value_len_bytes: encoded.len() as u64,
                value_sha256: hash_bytes(&encoded),
                error,
            }),
        }
    }
    Ok(WorkspaceRunScan {
        scanned_rows,
        expired_keys,
        corrupt_rows,
        rows: decoded_rows,
    })
}

fn cleanup_expired_workspace_rows(
    db: &Db,
    run_id: &str,
    now_unix_ms: u64,
) -> Result<WorkspaceCleanupReport, ErrorData> {
    let scan = scan_workspace_run(db, run_id, now_unix_ms)?;
    Ok(WorkspaceCleanupReport {
        expired_keys: scan.expired_keys,
        corrupt_rows: scan.corrupt_rows,
    })
}

fn read_workspace_row_optional(
    db: &Db,
    row_key: &str,
    _now_unix_ms: u64,
) -> Result<Option<DecodedWorkspaceRow>, ErrorData> {
    let Some(row) = read_workspace_raw_row_optional(db, row_key)? else {
        return Ok(None);
    };
    decode_workspace_row(row.key, row.encoded)
        .map(Some)
        .map_err(|error| workspace_corrupt_error(row_key, error))
}

fn read_workspace_raw_row_optional(
    db: &Db,
    row_key: &str,
) -> Result<Option<WorkspaceRawRow>, ErrorData> {
    Ok(db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .find(|(key, _value)| key == row_key.as_bytes())
        .map(|(key, encoded)| WorkspaceRawRow { key, encoded }))
}

fn decode_workspace_row(key: Vec<u8>, encoded: Vec<u8>) -> Result<DecodedWorkspaceRow, String> {
    let row_key = String::from_utf8_lossy(&key).to_string();
    let entry: WorkspaceEntry = synapse_storage::decode_json(&encoded)
        .map_err(|error| format!("decode workspace row {row_key}: {error}"))?;
    if entry.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "workspace row {row_key} has schema_version {}, expected {SCHEMA_VERSION}",
            entry.schema_version
        ));
    }
    if entry.row_key != row_key {
        return Err(format!(
            "workspace row key mismatch: stored entry.row_key={} actual={row_key}",
            entry.row_key
        ));
    }
    if entry.run_id.trim().is_empty() || entry.key.trim().is_empty() {
        return Err(format!(
            "workspace row {row_key} has empty run_id or key fields"
        ));
    }
    Ok(DecodedWorkspaceRow {
        key,
        encoded,
        entry,
    })
}

fn readback_exact_workspace_row(db: &Db, row_key: &str) -> Result<WorkspaceRowReadback, ErrorData> {
    let stored = db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .find_map(|(key, value)| (key == row_key.as_bytes()).then_some(value))
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!("workspace blackboard row missing after write: {row_key}"),
            )
        })?;
    Ok(WorkspaceRowReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        value_len_bytes: stored.len() as u64,
        value_sha256: hash_bytes(&stored),
    })
}

fn readback_absent_workspace_row(
    db: &Db,
    row_key: &str,
) -> Result<WorkspaceAbsentReadback, ErrorData> {
    let exact_match_count = db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .filter(|(key, _value)| key == row_key.as_bytes())
        .count();
    Ok(WorkspaceAbsentReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        exists: exact_match_count > 0,
        exact_match_count,
    })
}

fn workspace_row_readback(row: &DecodedWorkspaceRow) -> WorkspaceRowReadback {
    WorkspaceRowReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row.entry.row_key.clone(),
        value_len_bytes: row.encoded.len() as u64,
        value_sha256: hash_bytes(&row.encoded),
    }
}

fn encode_workspace_entry(entry: &WorkspaceEntry) -> Result<Vec<u8>, ErrorData> {
    synapse_storage::encode_json(entry).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("encode workspace blackboard entry: {error}"),
        )
    })
}

fn delete_workspace_rows(
    db: &Db,
    keys: Vec<Vec<u8>>,
    operation: &'static str,
) -> Result<(), ErrorData> {
    if keys.is_empty() {
        return Ok(());
    }
    db.delete_batch(cf::CF_KV, keys)
        .map_err(|error| mcp_error(error.code(), format!("{operation}: {error}")))
}

fn workspace_write_lock() -> Result<MutexGuard<'static, ()>, ErrorData> {
    WORKSPACE_WRITE_LOCK.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "workspace blackboard write lock poisoned",
        )
    })
}

fn validate_workspace_expected_version(
    expected_version: Option<u64>,
    current_version: Option<u64>,
    run_id: &str,
    key: &str,
    row_key: &str,
) -> Result<(), ErrorData> {
    match (expected_version, current_version) {
        (None, None) => Ok(()),
        (Some(expected), Some(current)) if expected == current => Ok(()),
        (expected, current) => Err(workspace_version_conflict_error(
            run_id, key, row_key, expected, current,
        )),
    }
}

fn resolve_workspace_run_id(raw: Option<&str>) -> Result<String, ErrorData> {
    let value = match raw {
        Some(value) => value.trim().to_owned(),
        None => crate::daemon_lifecycle::current_run_id().ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "workspace blackboard requires daemon lifecycle run_id; pass run_id explicitly in tests or start the configured daemon",
            )
        })?,
    };
    validate_workspace_run_id(&value)?;
    Ok(value)
}

fn validate_workspace_run_id(value: &str) -> Result<(), ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_RUN_ID_BYTES {
        return Err(params_error(format!(
            "workspace run_id must be non-empty and <= {MAX_RUN_ID_BYTES} bytes"
        )));
    }
    if !trimmed.chars().all(|ch| !ch.is_control()) {
        return Err(params_error(
            "workspace run_id must not contain control characters",
        ));
    }
    Ok(())
}

fn normalize_workspace_key(value: &str) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_KEY_BYTES {
        return Err(params_error(format!(
            "workspace key must be non-empty and <= {MAX_KEY_BYTES} bytes"
        )));
    }
    if !trimmed.chars().all(|ch| !ch.is_control()) {
        return Err(params_error(
            "workspace key must not contain control characters",
        ));
    }
    Ok(trimmed.to_owned())
}

fn normalize_workspace_prefix(value: &str, allow_empty: bool) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if !allow_empty && trimmed.is_empty() {
        return Err(params_error("workspace prefix must not be empty"));
    }
    if trimmed.len() > MAX_KEY_BYTES {
        return Err(params_error(format!(
            "workspace prefix must be <= {MAX_KEY_BYTES} bytes"
        )));
    }
    if !trimmed.chars().all(|ch| !ch.is_control()) {
        return Err(params_error(
            "workspace prefix must not contain control characters",
        ));
    }
    Ok(trimmed.to_owned())
}

fn validate_workspace_ttl_ms(ttl_ms: u64) -> Result<(), ErrorData> {
    if ttl_ms == 0 || ttl_ms > MAX_WORKSPACE_TTL_MS {
        return Err(params_error(format!(
            "workspace ttl_ms must be between 1 and {MAX_WORKSPACE_TTL_MS}"
        )));
    }
    Ok(())
}

fn validate_workspace_list_limit(limit: usize) -> Result<(), ErrorData> {
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(params_error(format!(
            "workspace_list limit must be between 1 and {MAX_LIST_LIMIT}"
        )));
    }
    Ok(())
}

fn validate_workspace_wait_timeout_ms(timeout_ms: u64) -> Result<(), ErrorData> {
    if !(MIN_WORKSPACE_WAIT_TIMEOUT_MS..=MAX_WORKSPACE_WAIT_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(params_error(format!(
            "workspace wait timeout_ms must be between {MIN_WORKSPACE_WAIT_TIMEOUT_MS} and {MAX_WORKSPACE_WAIT_TIMEOUT_MS}"
        )));
    }
    Ok(())
}

fn validate_workspace_wait_poll_interval_ms(poll_interval_ms: u64) -> Result<(), ErrorData> {
    if !(MIN_WORKSPACE_WAIT_POLL_INTERVAL_MS..=MAX_WORKSPACE_WAIT_POLL_INTERVAL_MS)
        .contains(&poll_interval_ms)
    {
        return Err(params_error(format!(
            "workspace wait poll_interval_ms must be between {MIN_WORKSPACE_WAIT_POLL_INTERVAL_MS} and {MAX_WORKSPACE_WAIT_POLL_INTERVAL_MS}"
        )));
    }
    Ok(())
}

fn validate_inline_value_size(value: Option<&Value>) -> Result<(), ErrorData> {
    let Some(value) = value else {
        return Ok(());
    };
    let encoded = synapse_storage::encode_json(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("workspace_put value must be JSON-encodable: {error}"),
        )
    })?;
    if encoded.len() > MAX_INLINE_VALUE_BYTES {
        return Err(params_error(format!(
            "workspace_put value must encode to <= {MAX_INLINE_VALUE_BYTES} bytes; got {}",
            encoded.len()
        )));
    }
    Ok(())
}

fn validate_workspace_artifact(
    artifact: WorkspaceArtifactRef,
) -> Result<(WorkspaceArtifactRef, Option<WorkspaceArtifactReadback>), ErrorData> {
    let handle = normalize_artifact_text(
        artifact.handle,
        "workspace artifact handle",
        MAX_ARTIFACT_HANDLE_CHARS,
    )?;
    let path = normalize_optional_artifact_text(artifact.path, "workspace artifact path")?;
    let media_type =
        normalize_optional_artifact_text(artifact.media_type, "workspace artifact media_type")?;
    let kind = normalize_optional_artifact_text(artifact.kind, "workspace artifact kind")?;
    let sha256 = artifact.sha256.map(normalize_sha256).transpose()?;
    let bytes_len = artifact.bytes_len;
    let readback = if let Some(path_value) = path.as_deref() {
        let readback = readback_artifact_path(path_value)?;
        if let Some(expected_len) = bytes_len
            && expected_len != readback.bytes_len
        {
            return Err(params_error(format!(
                "workspace artifact bytes_len mismatch for {path_value}: expected {expected_len}, read {}",
                readback.bytes_len
            )));
        }
        if let Some(expected_sha) = sha256.as_deref()
            && expected_sha != readback.sha256
        {
            return Err(params_error(format!(
                "workspace artifact sha256 mismatch for {path_value}: expected {expected_sha}, read {}",
                readback.sha256
            )));
        }
        Some(readback)
    } else {
        None
    };

    Ok((
        WorkspaceArtifactRef {
            handle,
            path,
            media_type,
            kind,
            sha256,
            bytes_len,
        },
        readback,
    ))
}

fn normalize_artifact_text(
    value: String,
    field: &'static str,
    max_chars: usize,
) -> Result<String, ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(params_error(format!("{field} must not be empty")));
    }
    if trimmed.chars().count() > max_chars {
        return Err(params_error(format!(
            "{field} must be at most {max_chars} Unicode scalar values"
        )));
    }
    if !trimmed.chars().all(|ch| !ch.is_control()) {
        return Err(params_error(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional_artifact_text(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<String>, ErrorData> {
    value
        .map(|value| normalize_artifact_text(value, field, MAX_ARTIFACT_TEXT_CHARS))
        .transpose()
}

fn normalize_sha256(value: String) -> Result<String, ErrorData> {
    let trimmed = value.trim().to_ascii_lowercase();
    let hex = trimmed.strip_prefix("sha256:").unwrap_or(&trimmed);
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(params_error(
            "workspace artifact sha256 must be a 64-char hex digest or sha256:<hex>",
        ));
    }
    Ok(format!("sha256:{hex}"))
}

fn readback_artifact_path(path_value: &str) -> Result<WorkspaceArtifactReadback, ErrorData> {
    let path = Path::new(path_value);
    let metadata = path.metadata().map_err(|error| {
        params_error(format!(
            "workspace artifact path {} is not readable: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(params_error(format!(
            "workspace artifact path {} must be a file",
            path.display()
        )));
    }
    let sha256 = sha256_file(path)?;
    Ok(WorkspaceArtifactReadback {
        path: path.display().to_string(),
        exists: true,
        is_file: true,
        bytes_len: metadata.len(),
        sha256,
    })
}

fn sha256_file(path: &Path) -> Result<String, ErrorData> {
    let mut file = File::open(path).map_err(|error| {
        params_error(format!(
            "workspace artifact path {} could not be opened for hashing: {error}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            params_error(format!(
                "workspace artifact path {} could not be read for hashing: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", hex_bytes(&hasher.finalize())))
}

fn workspace_publish_report(event_seq: u64, report: PublishReport) -> WorkspaceEventPublishReport {
    WorkspaceEventPublishReport {
        event_kind: WORKSPACE_PUT_EVENT_KIND.to_owned(),
        event_seq,
        matched: report.matched,
        queued: report.queued,
        dropped: report.dropped,
    }
}

fn workspace_missing_error(run_id: &str, key: &str, row_key: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("workspace blackboard key {key:?} was not found for run {run_id:?}"),
        Some(json!({
            "code": WORKSPACE_KEY_ABSENT,
            "detail_code": WORKSPACE_KEY_ABSENT,
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "remediation": "create the key with workspace operation=put or check presence with workspace operation=exists",
        })),
    )
}

fn workspace_expired_error(
    run_id: &str,
    key: &str,
    row_key: &str,
    expires_at_unix_ms: u64,
    now_unix_ms: u64,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("workspace blackboard key {key:?} for run {run_id:?} expired and was deleted"),
        Some(json!({
            "code": error_codes::STORAGE_READ_FAILED,
            "detail_code": "WORKSPACE_ROW_EXPIRED",
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "expires_at_unix_ms": expires_at_unix_ms,
            "now_unix_ms": now_unix_ms,
            "source_of_truth": "CF_KV workspace-blackboard exact row",
        })),
    )
}

fn workspace_wait_timeout_error(
    run_id: &str,
    key: &str,
    row_key: &str,
    timeout_ms: u64,
    waited_ms: u64,
    poll_count: u64,
    absent_readback: &WorkspaceAbsentReadback,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "workspace blackboard wait for key {key:?} in run {run_id:?} timed out after {waited_ms}ms (timeout {timeout_ms}ms, {poll_count} polls) without the key becoming present"
        ),
        Some(json!({
            "code": WORKSPACE_WAIT_TIMEOUT,
            "detail_code": WORKSPACE_WAIT_TIMEOUT,
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "timeout_ms": timeout_ms,
            "waited_ms": waited_ms,
            "poll_count": poll_count,
            "absent_readback": absent_readback,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "remediation": "increase wait timeout_ms, poll with workspace operation=get absent_ok=true, or ensure a peer publishes the key with workspace operation=put",
        })),
    )
}

fn workspace_version_conflict_error(
    run_id: &str,
    key: &str,
    row_key: &str,
    expected_version: Option<u64>,
    current_version: Option<u64>,
) -> ErrorData {
    let detail = match (expected_version, current_version) {
        (None, Some(current)) => {
            format!(
                "key already exists at version {current}; read it first and retry with expected_version={current}"
            )
        }
        (Some(expected), None) => {
            format!("expected_version={expected} was supplied, but the key does not exist")
        }
        (Some(expected), Some(current)) => {
            format!("expected_version={expected} did not match current version {current}")
        }
        (None, None) => "no version conflict".to_owned(),
    };
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "workspace blackboard key {key:?} for run {run_id:?} version precondition failed: {detail}"
        ),
        Some(json!({
            "code": error_codes::STORAGE_WRITE_FAILED,
            "detail_code": "WORKSPACE_VERSION_CONFLICT",
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "expected_version": expected_version,
            "current_version": current_version,
            "source_of_truth": "CF_KV workspace-blackboard exact row",
        })),
    )
}

fn validate_workspace_delete_version_guard(
    expected_version: Option<u64>,
    expected_corrupt_sha256: Option<&str>,
    raw_row_key: Option<&str>,
    current_version: u64,
    run_id: &str,
    key: &str,
    row_key: &str,
) -> Result<(), ErrorData> {
    if raw_row_key.is_some() {
        return Err(workspace_delete_guard_error(
            error_codes::TOOL_PARAMS_INVALID,
            "WORKSPACE_DELETE_RAW_ROW_KEY_ON_DECODABLE_ROW",
            "workspace delete received raw_row_key for a decodable row",
            run_id,
            key,
            row_key,
            expected_version,
            expected_corrupt_sha256,
            None,
            "omit raw_row_key and pass expected_version for decodable rows",
        ));
    }
    if let Some(hash) = expected_corrupt_sha256 {
        return Err(workspace_delete_guard_error(
            error_codes::TOOL_PARAMS_INVALID,
            "WORKSPACE_DELETE_CORRUPT_GUARD_ON_DECODABLE_ROW",
            "workspace delete received expected_corrupt_sha256 for a decodable row",
            run_id,
            key,
            row_key,
            expected_version,
            Some(hash),
            None,
            "read the row version and pass expected_version for decodable rows",
        ));
    }
    validate_workspace_expected_version(
        expected_version,
        Some(current_version),
        run_id,
        key,
        row_key,
    )
}

fn validate_workspace_delete_corrupt_guard(
    expected_version: Option<u64>,
    expected_corrupt_sha256: Option<&str>,
    raw_row_key: Option<&str>,
    readback: &WorkspaceRowReadback,
    run_id: &str,
    key: &str,
) -> Result<(), ErrorData> {
    if expected_version.is_some() {
        return Err(workspace_delete_guard_error(
            error_codes::STORAGE_CORRUPTED,
            "WORKSPACE_DELETE_VERSION_GUARD_ON_CORRUPT_ROW",
            "workspace delete cannot use expected_version for a corrupt row because no trusted version can be decoded",
            run_id,
            key,
            &readback.row_key,
            expected_version,
            expected_corrupt_sha256,
            Some(&readback.value_sha256),
            "read the corrupt row hash from workspace list/get, then retry with expected_corrupt_sha256 and no expected_version",
        ));
    }
    if raw_row_key.is_some_and(|raw| raw.trim() != readback.row_key) {
        return Err(workspace_delete_guard_error(
            error_codes::TOOL_PARAMS_INVALID,
            "WORKSPACE_DELETE_RAW_ROW_KEY_MISMATCH",
            "workspace delete raw_row_key did not match the physical row selected for deletion",
            run_id,
            key,
            &readback.row_key,
            expected_version,
            expected_corrupt_sha256,
            Some(&readback.value_sha256),
            "pass the exact corrupt row_key returned by workspace list/get",
        ));
    }
    let Some(expected_hash) = expected_corrupt_sha256.map(str::trim) else {
        return Err(workspace_delete_guard_error(
            error_codes::STORAGE_CORRUPTED,
            "WORKSPACE_CORRUPT_ROW_REQUIRES_HASH_GUARD",
            "workspace delete found a corrupt row and requires expected_corrupt_sha256 before deleting it",
            run_id,
            key,
            &readback.row_key,
            expected_version,
            None,
            Some(&readback.value_sha256),
            "read the corrupt row hash from workspace list/get, then retry with expected_corrupt_sha256",
        ));
    };
    if expected_hash != readback.value_sha256 {
        return Err(workspace_delete_guard_error(
            error_codes::STORAGE_WRITE_FAILED,
            "WORKSPACE_CORRUPT_HASH_CONFLICT",
            "workspace delete corrupt-row hash precondition failed",
            run_id,
            key,
            &readback.row_key,
            expected_version,
            Some(expected_hash),
            Some(&readback.value_sha256),
            "retry only after reading the current corrupt row hash from the source of truth",
        ));
    }
    Ok(())
}

fn validate_workspace_raw_delete_row_key(
    run_id: &str,
    raw_row_key: &str,
) -> Result<String, ErrorData> {
    let trimmed = raw_row_key.trim();
    if trimmed.is_empty() {
        return Err(params_error(
            "workspace delete raw_row_key must not be empty",
        ));
    }
    if trimmed != raw_row_key {
        return Err(params_error(
            "workspace delete raw_row_key must not contain leading or trailing whitespace",
        ));
    }
    if trimmed.len() > MAX_ARTIFACT_HANDLE_CHARS {
        return Err(params_error(format!(
            "workspace delete raw_row_key must be <= {MAX_ARTIFACT_HANDLE_CHARS} bytes"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(params_error(
            "workspace delete raw_row_key must not contain control characters",
        ));
    }
    let run_prefix = workspace_run_prefix(run_id);
    if !trimmed.starts_with(&run_prefix) || trimmed == run_prefix {
        return Err(workspace_facade_error(
            error_codes::TOOL_PARAMS_INVALID,
            "delete",
            "workspace delete raw_row_key must be under the resolved workspace run prefix",
            "pass the exact corrupt row_key returned by workspace list/get for the same run_id",
        ));
    }
    Ok(trimmed.to_owned())
}

fn workspace_delete_guard_error(
    code: &'static str,
    detail_code: &'static str,
    message: impl Into<String>,
    run_id: &str,
    key: &str,
    row_key: &str,
    expected_version: Option<u64>,
    expected_corrupt_sha256: Option<&str>,
    actual_sha256: Option<&str>,
    remediation: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": code,
            "detail_code": detail_code,
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "expected_version": expected_version,
            "expected_corrupt_sha256": expected_corrupt_sha256,
            "actual_sha256": actual_sha256,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "remediation": remediation.into(),
        })),
    )
}

fn workspace_corrupt_error(row_key: &str, detail: String) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("workspace blackboard row {row_key} is corrupt: {detail}"),
        Some(json!({
            "code": error_codes::STORAGE_CORRUPTED,
            "row_key": row_key,
            "detail": detail,
            "source_of_truth": "CF_KV workspace-blackboard exact row",
        })),
    )
}

fn params_error(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

fn validate_workspace_facade_params(params: &WorkspaceParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        WORKSPACE_TOOL,
        params.operation.as_str(),
        &[
            ("get", params.get.is_some()),
            ("put", params.put.is_some()),
            ("list", params.list.is_some()),
            ("subscribe", params.subscribe.is_some()),
            ("exists", params.exists.is_some()),
            ("delete", params.delete.is_some()),
            ("wait", params.wait.is_some()),
        ],
    )
}

fn validate_exact_operation_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
) -> Result<(), ErrorData> {
    let present = specs
        .iter()
        .filter_map(|(name, is_present)| is_present.then_some(*name))
        .collect::<Vec<_>>();
    if !present.contains(&operation) {
        return Err(workspace_facade_error(
            error_codes::TOOL_PARAMS_INVALID,
            operation,
            format!("{tool} operation={operation} requires a matching {operation} spec"),
            format!("pass {operation}={{...}} and no other operation spec"),
        ));
    }
    if present.len() != 1 {
        return Err(workspace_facade_error(
            error_codes::TOOL_PARAMS_INVALID,
            operation,
            format!("{tool} operation={operation} received invalid operation specs {present:?}"),
            format!("pass exactly one operation-specific spec matching {operation}"),
        ));
    }
    Ok(())
}

fn missing_workspace_spec(operation: &'static str) -> ErrorData {
    workspace_facade_error(
        error_codes::TOOL_PARAMS_INVALID,
        operation,
        format!("workspace operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn workspace_facade_error(
    code: &'static str,
    operation: &'static str,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": code,
            "tool": WORKSPACE_TOOL,
            "operation": operation,
            "source_of_truth": "typed workspace facade params before delegated workspace operation",
            "remediation": remediation.into(),
        })),
    )
}

fn workspace_response(
    operation: WorkspaceOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut WorkspaceResponse),
) -> WorkspaceResponse {
    let mut response = WorkspaceResponse {
        operation,
        source_of_truth: format!(
            "{WORKSPACE_SOURCE_OF_TRUTH} + delegated workspace operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        get: None,
        put: None,
        list: None,
        subscribe: None,
        exists: None,
        delete: None,
        wait: None,
    };
    populate(&mut response);
    response
}

fn require_workspace_session_id(
    tool_name: &str,
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool_name} requires an MCP session id (run the daemon in HTTP mode so each agent has its own Mcp-Session-Id)"
            ),
        )
    })
}

fn workspace_run_prefix(run_id: &str) -> String {
    format!(
        "{WORKSPACE_PREFIX}/run_hex/{}/key_hex/",
        hex_bytes(run_id.as_bytes())
    )
}

fn workspace_row_key(run_id: &str, key: &str) -> String {
    format!(
        "{}{}",
        workspace_run_prefix(run_id),
        hex_bytes(key.as_bytes())
    )
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_bytes(&digest))
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

const fn default_workspace_ttl_ms() -> u64 {
    DEFAULT_WORKSPACE_TTL_MS
}

const fn default_list_limit() -> usize {
    DEFAULT_LIST_LIMIT
}

const fn default_workspace_wait_timeout_ms() -> u64 {
    DEFAULT_WORKSPACE_WAIT_TIMEOUT_MS
}

const fn default_workspace_wait_poll_interval_ms() -> u64 {
    DEFAULT_WORKSPACE_WAIT_POLL_INTERVAL_MS
}

const fn default_true() -> bool {
    true
}

const fn default_false() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use serde_json::json;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

    fn service_with_db(path: &Path) -> anyhow::Result<SynapseService> {
        SynapseService::try_with_m2_shutdown_reason_and_m3_config(
            CancellationToken::new(),
            "test",
            CancellationToken::new(),
            &M2ServiceConfig::default(),
            M3ServiceConfig::from_cli_parts(
                Some(path.join("db")),
                Some(path.to_path_buf()),
                false,
                "127.0.0.1:0".to_owned(),
                NonZeroUsize::new(8)
                    .ok_or_else(|| anyhow::anyhow!("max subscriptions must be nonzero"))?,
                false,
                true,
                None,
                false,
                None,
            ),
            M4ServiceConfig::default(),
        )
    }

    fn error_code(error: &rmcp::ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    fn error_detail_code(error: &rmcp::ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("detail_code"))
            .and_then(Value::as_str)
    }

    fn error_remediation(error: &rmcp::ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("remediation"))
            .and_then(Value::as_str)
    }

    #[test]
    fn put_records_command_audit_rows_readable_by_snapshot() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        let put = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-audit".to_owned()),
                key: "plans/next-step".to_owned(),
                expected_version: None,
                value: Some(json!({"known": "audit-happy"})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-audit",
            9_000,
        )?;
        assert_eq!(put.version, 1);

        let snapshot = service.command_audit_snapshot()?;
        assert!(
            snapshot.rows.iter().any(|row| {
                row.tool == "workspace_put"
                    && row.verb == "plan_edit"
                    && row.phase == "intent"
                    && row.actor_session_id.as_deref() == Some("session-audit")
            }),
            "workspace_put intent row should be projected from CF_ACTION_LOG"
        );
        assert!(
            snapshot.rows.iter().any(|row| {
                row.tool == "workspace_put"
                    && row.verb == "plan_edit"
                    && row.phase == "final"
                    && row.outcome == "ok"
                    && row.actor_session_id.as_deref() == Some("session-audit")
            }),
            "workspace_put final row should be projected from CF_ACTION_LOG"
        );
        Ok(())
    }

    #[test]
    fn put_fails_before_storage_write_when_command_audit_intent_fails() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        crate::server::command_audit::set_command_audit_force_fail_for_tests(true);
        let result = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-audit-fail".to_owned()),
                key: "plans/blocked".to_owned(),
                expected_version: None,
                value: Some(json!({"must_not_write": true})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-audit",
            9_100,
        );
        crate::server::command_audit::set_command_audit_force_fail_for_tests(false);

        let error = match result {
            Ok(response) => anyhow::bail!("workspace_put unexpectedly succeeded: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), Some(error_codes::TOOL_INTERNAL_ERROR));
        assert!(error.message.contains("command audit forced failure"));

        let db = service.workspace_db()?;
        assert!(
            db.scan_cf_prefix(cf::CF_KV, workspace_run_prefix("run-audit-fail").as_bytes())?
                .is_empty(),
            "workspace row must not exist when command audit intent write fails"
        );
        Ok(())
    }

    #[test]
    fn put_and_get_round_trip_value_and_artifact_file_readback() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let artifact_path = temp.path().join("artifact.txt");
        std::fs::write(&artifact_path, b"issue796-artifact-bytes")?;
        let artifact_hash = sha256_file(&artifact_path)
            .map_err(|error| anyhow::anyhow!("artifact hash failed: {error:?}"))?;
        let service = service_with_db(temp.path())?;

        let put = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-796".to_owned()),
                key: "findings/k1".to_owned(),
                expected_version: None,
                value: Some(json!({"known": 4, "marker": "ISSUE796"})),
                artifact: Some(WorkspaceArtifactRef {
                    handle: "artifact://issue796/screenshot".to_owned(),
                    path: Some(artifact_path.display().to_string()),
                    media_type: Some("text/plain".to_owned()),
                    kind: Some("screenshot".to_owned()),
                    sha256: Some(artifact_hash.clone()),
                    bytes_len: Some(23),
                }),
                ttl_ms: 60_000,
            },
            "session-a",
            10_000,
        )?;
        assert_eq!(put.run_id, "run-796");
        assert_eq!(put.version, 1);
        assert_eq!(
            put.artifact_readback
                .as_ref()
                .map(|row| row.sha256.as_str()),
            Some(artifact_hash.as_str())
        );

        let get = service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-796".to_owned()),
                key: "findings/k1".to_owned(),
                absent_ok: false,
            },
            "session-b",
            10_001,
        )?;
        assert!(get.found);
        let get_entry = get.entry.as_ref().expect("present get returns an entry");
        assert_eq!(
            get_entry.value,
            Some(json!({"known": 4, "marker": "ISSUE796"}))
        );
        assert_eq!(
            get_entry
                .artifact
                .as_ref()
                .map(|artifact| artifact.handle.as_str()),
            Some("artifact://issue796/screenshot")
        );
        assert_eq!(get.storage_readback.as_ref(), Some(&put.storage_readback));
        assert!(get.absent_readback.is_none());
        Ok(())
    }

    #[test]
    fn list_skips_corrupt_row_without_breaking_good_reader() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-corrupt".to_owned()),
                key: "findings/good".to_owned(),
                expected_version: None,
                value: Some(json!({"ok": true})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-a",
            20_000,
        )?;
        let db = service.workspace_db()?;
        let corrupt_key = workspace_row_key("run-corrupt", "findings/bad");
        db.put_batch_pressure_bypass(
            cf::CF_KV,
            [(corrupt_key.as_bytes().to_vec(), b"{not-json".to_vec())],
        )?;

        let list = service.workspace_list_impl_at(
            WorkspaceListParams {
                run_id: Some("run-corrupt".to_owned()),
                prefix: Some("findings/".to_owned()),
                limit: 10,
                include_values: true,
            },
            "session-b",
            20_001,
        )?;
        assert_eq!(list.scanned_rows, 2);
        assert_eq!(list.returned_count, 1);
        assert_eq!(list.entries[0].key, "findings/good");
        assert_eq!(list.corrupt_rows_skipped.len(), 1);
        assert_eq!(list.corrupt_rows_skipped[0].row_key, corrupt_key);
        Ok(())
    }

    #[test]
    fn subscribe_filter_matches_later_put_event() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let subscription = service.workspace_subscribe_impl(
            WorkspaceSubscribeParams {
                run_id: Some("run-sub".to_owned()),
                prefix: "findings/".to_owned(),
                snapshot_first: false,
            },
            "session-b",
        )?;
        assert_eq!(service.sse_state()?.active_subscription_count(), 1);

        let put = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-sub".to_owned()),
                key: "findings/x".to_owned(),
                expected_version: None,
                value: Some(json!({"event": "expected"})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-a",
            30_000,
        )?;
        assert_eq!(subscription.event_kind, WORKSPACE_PUT_EVENT_KIND);
        assert_eq!(put.event_publish_report.matched, 1);
        assert_eq!(put.event_publish_report.queued, 1);
        Ok(())
    }

    #[test]
    fn expired_rows_are_deleted_on_get_and_list() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-ttl".to_owned()),
                key: "findings/old".to_owned(),
                expected_version: None,
                value: Some(json!({"ttl": 1})),
                artifact: None,
                ttl_ms: 1,
            },
            "session-a",
            40_000,
        )?;

        let error = match service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-ttl".to_owned()),
                key: "findings/old".to_owned(),
                absent_ok: false,
            },
            "session-b",
            40_002,
        ) {
            Ok(response) => anyhow::bail!("expired get unexpectedly succeeded: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), Some(error_codes::STORAGE_READ_FAILED));
        let db = service.workspace_db()?;
        assert!(
            db.scan_cf_prefix(cf::CF_KV, workspace_run_prefix("run-ttl").as_bytes())?
                .is_empty()
        );

        service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-ttl".to_owned()),
                key: "findings/old-list".to_owned(),
                expected_version: None,
                value: Some(json!({"ttl": 1})),
                artifact: None,
                ttl_ms: 1,
            },
            "session-a",
            50_000,
        )?;
        let list = service.workspace_list_impl_at(
            WorkspaceListParams {
                run_id: Some("run-ttl".to_owned()),
                prefix: Some("findings/".to_owned()),
                limit: 10,
                include_values: true,
            },
            "session-b",
            50_002,
        )?;
        assert_eq!(list.expired_rows_deleted, 1);
        assert_eq!(list.returned_count, 0);
        assert!(
            db.scan_cf_prefix(cf::CF_KV, workspace_run_prefix("run-ttl").as_bytes())?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn put_existing_key_requires_expected_version() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let first = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-cas".to_owned()),
                key: "findings/shared".to_owned(),
                expected_version: None,
                value: Some(json!({"marker": "first"})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-a",
            70_000,
        )?;
        assert_eq!(first.version, 1);
        assert_eq!(first.previous_version, None);

        let conflict = match service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-cas".to_owned()),
                key: "findings/shared".to_owned(),
                expected_version: None,
                value: Some(json!({"marker": "blind-overwrite"})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-b",
            70_001,
        ) {
            Ok(response) => anyhow::bail!("blind overwrite unexpectedly succeeded: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(
            error_code(&conflict),
            Some(error_codes::STORAGE_WRITE_FAILED)
        );
        assert_eq!(
            error_detail_code(&conflict),
            Some("WORKSPACE_VERSION_CONFLICT")
        );

        let still_first = service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-cas".to_owned()),
                key: "findings/shared".to_owned(),
                absent_ok: false,
            },
            "session-c",
            70_002,
        )?;
        let still_first_entry = still_first.entry.as_ref().expect("present get entry");
        assert_eq!(still_first_entry.value, Some(json!({"marker": "first"})));
        assert_eq!(still_first_entry.version, 1);

        let second = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-cas".to_owned()),
                key: "findings/shared".to_owned(),
                expected_version: Some(1),
                value: Some(json!({"marker": "second"})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-b",
            70_003,
        )?;
        assert_eq!(second.version, 2);
        assert_eq!(second.previous_version, Some(1));

        let updated = service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-cas".to_owned()),
                key: "findings/shared".to_owned(),
                absent_ok: false,
            },
            "session-c",
            70_004,
        )?;
        let updated_entry = updated.entry.as_ref().expect("present get entry");
        assert_eq!(updated_entry.value, Some(json!({"marker": "second"})));
        assert_eq!(updated_entry.version, 2);
        Ok(())
    }

    #[test]
    fn parameter_edges_fail_closed() -> anyhow::Result<()> {
        assert!(normalize_workspace_key("").is_err());
        assert!(normalize_workspace_key("bad\nkey").is_err());
        assert!(validate_workspace_ttl_ms(0).is_err());
        assert!(validate_workspace_ttl_ms(MAX_WORKSPACE_TTL_MS + 1).is_err());
        assert!(validate_workspace_list_limit(0).is_err());
        assert!(validate_workspace_list_limit(MAX_LIST_LIMIT + 1).is_err());
        assert!(
            validate_inline_value_size(Some(&json!({"blob": "x".repeat(MAX_INLINE_VALUE_BYTES)})))
                .is_err()
        );
        assert!(normalize_sha256("not-a-digest".to_owned()).is_err());

        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let error = match service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-edge".to_owned()),
                key: "findings/no-payload".to_owned(),
                expected_version: None,
                value: None,
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-a",
            60_000,
        ) {
            Ok(response) => anyhow::bail!("payload-less put unexpectedly succeeded: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(error_code(&error), Some(error_codes::TOOL_PARAMS_INVALID));
        Ok(())
    }

    #[test]
    fn workspace_get_absent_key_has_typed_absent_error() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        let error = match service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-absent".to_owned()),
                key: "missing/key".to_owned(),
                absent_ok: false,
            },
            "session-a",
            80_000,
        ) {
            Ok(response) => anyhow::bail!("absent get unexpectedly succeeded: {response:?}"),
            Err(error) => error,
        };

        assert_eq!(error_code(&error), Some(WORKSPACE_KEY_ABSENT));
        assert_eq!(error_detail_code(&error), Some(WORKSPACE_KEY_ABSENT));
        assert!(
            error_remediation(&error)
                .is_some_and(|text| text.contains("workspace operation=exists"))
        );
        Ok(())
    }

    #[test]
    fn workspace_get_absent_ok_true_on_absent_key_is_success_found_false() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        // before: the synthetic key has no CF_KV row.
        let db = service.workspace_db()?;
        let row_key = workspace_row_key("run-1552", "fsv-1552-k1");
        assert_eq!(
            readback_absent_workspace_row(&db, &row_key)?.exact_match_count,
            0,
            "precondition: key must be absent before the tolerant get"
        );

        let get = service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                absent_ok: true,
            },
            "session-absent-ok",
            120_000,
        )?;

        // after: tolerated absence is a SUCCESS, not the WORKSPACE_KEY_ABSENT error.
        assert!(get.ok);
        assert!(!get.found);
        assert!(get.entry.is_none());
        assert!(get.storage_readback.is_none());
        let absent = get
            .absent_readback
            .as_ref()
            .expect("absent_ok get must carry the CF_KV absent_readback proof");
        assert!(!absent.exists);
        assert_eq!(absent.exact_match_count, 0);
        assert_eq!(absent.row_key, row_key);
        println!(
            "readback=absent_ok_get_absent found={} exists={} exact_match_count={} row_key={}",
            get.found, absent.exists, absent.exact_match_count, absent.row_key
        );
        Ok(())
    }

    #[test]
    fn workspace_get_absent_ok_true_on_present_key_returns_value_found_true() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        // before: publish the known synthetic value "2+2=4".
        service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                expected_version: None,
                value: Some(json!("2+2=4")),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-writer",
            130_000,
        )?;

        let get = service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                absent_ok: true,
            },
            "session-reader",
            130_001,
        )?;

        // after: a present key under absent_ok reads exactly like a normal get.
        assert!(get.found);
        let entry = get
            .entry
            .as_ref()
            .expect("present absent_ok get must return the entry");
        assert_eq!(entry.value, Some(json!("2+2=4")));
        assert!(get.storage_readback.is_some());
        assert!(get.absent_readback.is_none());
        println!(
            "readback=absent_ok_get_present found={} value={:?} sha256={:?}",
            get.found,
            entry.value,
            get.storage_readback.as_ref().map(|row| &row.value_sha256)
        );
        Ok(())
    }

    #[test]
    fn workspace_get_absent_ok_false_on_absent_key_still_errors() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        // backward-compat: default fail-closed behavior is unchanged.
        let error = match service.workspace_get_impl_at(
            WorkspaceGetParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-missing".to_owned(),
                absent_ok: false,
            },
            "session-fail-closed",
            140_000,
        ) {
            Ok(response) => {
                anyhow::bail!("fail-closed absent get unexpectedly succeeded: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(error_code(&error), Some(WORKSPACE_KEY_ABSENT));
        assert_eq!(error_detail_code(&error), Some(WORKSPACE_KEY_ABSENT));
        println!(
            "readback=absent_ok_false_error code={:?} detail_code={:?}",
            error_code(&error),
            error_detail_code(&error)
        );
        Ok(())
    }

    #[test]
    fn workspace_wait_resolves_when_key_present_found_true() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        // before: publish the key against a real-clock timestamp so its TTL is in
        // the real future (wait polls against the wall clock, unlike the
        // synthetic-now *_impl_at helpers).
        let put_now = unix_time_ms_now();
        service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                expected_version: None,
                value: Some(json!("2+2=4")),
                artifact: None,
                ttl_ms: 600_000,
            },
            "session-writer",
            put_now,
        )?;

        let wait = service.workspace_wait_impl(
            WorkspaceWaitParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                timeout_ms: 1_000,
                poll_interval_ms: 5,
            },
            "session-waiter",
        )?;

        // after: wait resolves immediately with the same readback as get.
        assert!(wait.ok);
        assert!(wait.found);
        assert_eq!(wait.key, "fsv-1552-k1");
        assert_eq!(wait.entry.value, Some(json!("2+2=4")));
        assert!(wait.poll_count >= 1);
        println!(
            "readback=wait_resolved found={} poll_count={} waited_ms={} value={:?} sha256={}",
            wait.found,
            wait.poll_count,
            wait.waited_ms,
            wait.entry.value,
            wait.storage_readback.value_sha256
        );
        Ok(())
    }

    #[test]
    fn workspace_wait_times_out_with_typed_error() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        // before: the key is never written.
        let db = service.workspace_db()?;
        let row_key = workspace_row_key("run-1552", "fsv-1552-never");
        assert_eq!(
            readback_absent_workspace_row(&db, &row_key)?.exact_match_count,
            0
        );

        let error = match service.workspace_wait_impl(
            WorkspaceWaitParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-never".to_owned(),
                timeout_ms: 5,
                poll_interval_ms: 1,
            },
            "session-waiter",
        ) {
            Ok(response) => {
                anyhow::bail!("wait on a never-written key unexpectedly resolved: {response:?}")
            }
            Err(error) => error,
        };

        // after: a typed timeout, not a generic storage error.
        assert_eq!(error_code(&error), Some(WORKSPACE_WAIT_TIMEOUT));
        assert_eq!(error_detail_code(&error), Some(WORKSPACE_WAIT_TIMEOUT));
        let waited_ms = error
            .data
            .as_ref()
            .and_then(|data| data.get("waited_ms"))
            .and_then(Value::as_u64);
        let poll_count = error
            .data
            .as_ref()
            .and_then(|data| data.get("poll_count"))
            .and_then(Value::as_u64);
        assert!(
            poll_count.is_some_and(|count| count >= 1),
            "timeout payload must report at least one poll"
        );
        println!(
            "readback=wait_timeout code={:?} waited_ms={:?} poll_count={:?}",
            error_code(&error),
            waited_ms,
            poll_count
        );
        Ok(())
    }

    #[test]
    fn workspace_wait_rejects_out_of_range_timeout() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        let too_large = match service.workspace_wait_impl(
            WorkspaceWaitParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                timeout_ms: MAX_WORKSPACE_WAIT_TIMEOUT_MS + 1,
                poll_interval_ms: 5,
            },
            "session-waiter",
        ) {
            Ok(response) => {
                anyhow::bail!("over-max wait timeout unexpectedly accepted: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(
            error_code(&too_large),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );

        let zero = match service.workspace_wait_impl(
            WorkspaceWaitParams {
                run_id: Some("run-1552".to_owned()),
                key: "fsv-1552-k1".to_owned(),
                timeout_ms: 0,
                poll_interval_ms: 5,
            },
            "session-waiter",
        ) {
            Ok(response) => anyhow::bail!("zero wait timeout unexpectedly accepted: {response:?}"),
            Err(error) => error,
        };
        assert_eq!(error_code(&zero), Some(error_codes::TOOL_PARAMS_INVALID));
        println!(
            "readback=wait_reject over_max={:?} zero={:?}",
            error_code(&too_large),
            error_code(&zero)
        );
        Ok(())
    }

    #[test]
    fn workspace_facade_put_get_exists_delete_round_trip() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        let put = service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-facade".to_owned()),
                key: "findings/facade".to_owned(),
                expected_version: None,
                value: Some(json!({"marker": "facade", "known": 4})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-a",
            90_000,
        )?;
        assert_eq!(put.version, 1);

        let exists = service.workspace_exists_impl_at(
            WorkspaceExistsParams {
                run_id: Some("run-facade".to_owned()),
                key: "findings/facade".to_owned(),
            },
            "session-b",
            90_001,
        )?;
        assert!(exists.exists);
        assert!(exists.physical_row_present);
        assert_eq!(exists.state, WorkspaceExistenceState::Present);
        assert_eq!(exists.current_version, Some(1));

        let delete = service.workspace_delete_impl_at(
            WorkspaceDeleteParams {
                run_id: Some("run-facade".to_owned()),
                key: "findings/facade".to_owned(),
                raw_row_key: None,
                expected_version: Some(1),
                expected_corrupt_sha256: None,
            },
            "session-c",
            90_002,
        )?;
        assert_eq!(delete.deleted_version, Some(1));
        assert!(delete.deleted_corrupt_row.is_none());
        assert!(!delete.post_delete_readback.exists);
        assert_eq!(delete.post_delete_readback.exact_match_count, 0);

        let absent = service.workspace_exists_impl_at(
            WorkspaceExistsParams {
                run_id: Some("run-facade".to_owned()),
                key: "findings/facade".to_owned(),
            },
            "session-d",
            90_003,
        )?;
        assert!(!absent.exists);
        assert_eq!(absent.state, WorkspaceExistenceState::Absent);
        assert_eq!(
            absent
                .absent_readback
                .as_ref()
                .map(|readback| readback.exact_match_count),
            Some(0)
        );
        Ok(())
    }

    #[test]
    fn workspace_delete_requires_current_expected_version() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;

        service.workspace_put_impl_at(
            WorkspacePutParams {
                run_id: Some("run-delete-cas".to_owned()),
                key: "findings/protected".to_owned(),
                expected_version: None,
                value: Some(json!({"version": 1})),
                artifact: None,
                ttl_ms: 60_000,
            },
            "session-a",
            100_000,
        )?;

        let conflict = match service.workspace_delete_impl_at(
            WorkspaceDeleteParams {
                run_id: Some("run-delete-cas".to_owned()),
                key: "findings/protected".to_owned(),
                raw_row_key: None,
                expected_version: Some(2),
                expected_corrupt_sha256: None,
            },
            "session-b",
            100_001,
        ) {
            Ok(response) => {
                anyhow::bail!("wrong-version delete unexpectedly succeeded: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(
            error_code(&conflict),
            Some(error_codes::STORAGE_WRITE_FAILED)
        );
        assert_eq!(
            error_detail_code(&conflict),
            Some("WORKSPACE_VERSION_CONFLICT")
        );

        let still_present = service.workspace_exists_impl_at(
            WorkspaceExistsParams {
                run_id: Some("run-delete-cas".to_owned()),
                key: "findings/protected".to_owned(),
            },
            "session-c",
            100_002,
        )?;
        assert!(still_present.exists);
        assert_eq!(still_present.current_version, Some(1));
        Ok(())
    }

    #[test]
    fn workspace_delete_corrupt_row_requires_exact_hash_guard() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let db = service.workspace_db()?;
        let corrupt_key = workspace_row_key("run-delete-corrupt", "findings/bad");
        let corrupt_value = b"{not-json".to_vec();
        let corrupt_hash = hash_bytes(&corrupt_value);
        db.put_batch_pressure_bypass(
            cf::CF_KV,
            [(corrupt_key.as_bytes().to_vec(), corrupt_value.clone())],
        )?;

        let listed = service.workspace_list_impl_at(
            WorkspaceListParams {
                run_id: Some("run-delete-corrupt".to_owned()),
                prefix: Some("findings/".to_owned()),
                limit: 10,
                include_values: false,
            },
            "session-a",
            110_000,
        )?;
        assert_eq!(listed.returned_count, 0);
        assert_eq!(listed.corrupt_rows_skipped.len(), 1);
        assert_eq!(listed.corrupt_rows_skipped[0].row_key, corrupt_key);
        assert_eq!(listed.corrupt_rows_skipped[0].value_sha256, corrupt_hash);

        let missing_guard = match service.workspace_delete_impl_at(
            WorkspaceDeleteParams {
                run_id: Some("run-delete-corrupt".to_owned()),
                key: "findings/bad".to_owned(),
                raw_row_key: None,
                expected_version: None,
                expected_corrupt_sha256: None,
            },
            "session-b",
            110_001,
        ) {
            Ok(response) => {
                anyhow::bail!("unguarded corrupt delete unexpectedly succeeded: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(
            error_code(&missing_guard),
            Some(error_codes::STORAGE_CORRUPTED)
        );
        assert_eq!(
            error_detail_code(&missing_guard),
            Some("WORKSPACE_CORRUPT_ROW_REQUIRES_HASH_GUARD")
        );
        assert_eq!(
            readback_exact_workspace_row(&db, &corrupt_key)?.value_sha256,
            corrupt_hash
        );

        let wrong_hash = match service.workspace_delete_impl_at(
            WorkspaceDeleteParams {
                run_id: Some("run-delete-corrupt".to_owned()),
                key: "findings/bad".to_owned(),
                raw_row_key: None,
                expected_version: None,
                expected_corrupt_sha256: Some("sha256:0000".to_owned()),
            },
            "session-c",
            110_002,
        ) {
            Ok(response) => {
                anyhow::bail!("wrong-hash corrupt delete unexpectedly succeeded: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(
            error_code(&wrong_hash),
            Some(error_codes::STORAGE_WRITE_FAILED)
        );
        assert_eq!(
            error_detail_code(&wrong_hash),
            Some("WORKSPACE_CORRUPT_HASH_CONFLICT")
        );
        assert_eq!(
            readback_exact_workspace_row(&db, &corrupt_key)?.value_sha256,
            corrupt_hash
        );

        let deleted = service.workspace_delete_impl_at(
            WorkspaceDeleteParams {
                run_id: Some("run-delete-corrupt".to_owned()),
                key: "findings/bad".to_owned(),
                raw_row_key: None,
                expected_version: None,
                expected_corrupt_sha256: Some(corrupt_hash.clone()),
            },
            "session-d",
            110_003,
        )?;
        assert_eq!(deleted.deleted_version, None);
        assert_eq!(
            deleted
                .deleted_corrupt_row
                .as_ref()
                .map(|row| row.value_sha256.as_str()),
            Some(corrupt_hash.as_str())
        );
        assert!(!deleted.post_delete_readback.exists);
        assert_eq!(
            readback_absent_workspace_row(&db, &corrupt_key)?.exact_match_count,
            0
        );

        let indexed_corrupt_key = format!(
            "{}:00000000000000000000",
            workspace_row_key("run-delete-corrupt", "findings/indexed")
        );
        let indexed_corrupt_value = b"{indexed-not-json".to_vec();
        let indexed_corrupt_hash = hash_bytes(&indexed_corrupt_value);
        db.put_batch_pressure_bypass(
            cf::CF_KV,
            [(
                indexed_corrupt_key.as_bytes().to_vec(),
                indexed_corrupt_value,
            )],
        )?;

        let indexed_list = service.workspace_list_impl_at(
            WorkspaceListParams {
                run_id: Some("run-delete-corrupt".to_owned()),
                prefix: Some("findings/".to_owned()),
                limit: 10,
                include_values: false,
            },
            "session-e",
            110_004,
        )?;
        assert_eq!(indexed_list.corrupt_rows_skipped.len(), 1);
        assert_eq!(
            indexed_list.corrupt_rows_skipped[0].row_key,
            indexed_corrupt_key
        );
        assert_eq!(
            indexed_list.corrupt_rows_skipped[0].value_sha256,
            indexed_corrupt_hash
        );

        let indexed_deleted = service.workspace_delete_impl_at(
            WorkspaceDeleteParams {
                run_id: Some("run-delete-corrupt".to_owned()),
                key: "findings/indexed".to_owned(),
                raw_row_key: Some(indexed_corrupt_key.clone()),
                expected_version: None,
                expected_corrupt_sha256: Some(indexed_corrupt_hash.clone()),
            },
            "session-f",
            110_005,
        )?;
        assert_eq!(indexed_deleted.deleted_version, None);
        assert_eq!(
            indexed_deleted
                .deleted_corrupt_row
                .as_ref()
                .map(|row| row.value_sha256.as_str()),
            Some(indexed_corrupt_hash.as_str())
        );
        assert_eq!(
            readback_absent_workspace_row(&db, &indexed_corrupt_key)?.exact_match_count,
            0
        );
        Ok(())
    }

    #[test]
    fn workspace_facade_params_require_exact_matching_spec() {
        let missing = validate_workspace_facade_params(&WorkspaceParams {
            operation: WorkspaceOperation::Put,
            get: Some(WorkspaceGetParams {
                run_id: Some("run".to_owned()),
                key: "k".to_owned(),
                absent_ok: false,
            }),
            put: None,
            list: None,
            subscribe: None,
            exists: None,
            delete: None,
            wait: None,
        })
        .expect_err("operation=put without put spec must fail");
        assert_eq!(error_code(&missing), Some(error_codes::TOOL_PARAMS_INVALID));

        let extra = validate_workspace_facade_params(&WorkspaceParams {
            operation: WorkspaceOperation::Get,
            get: Some(WorkspaceGetParams {
                run_id: Some("run".to_owned()),
                key: "k".to_owned(),
                absent_ok: false,
            }),
            put: Some(WorkspacePutParams {
                run_id: Some("run".to_owned()),
                key: "k".to_owned(),
                expected_version: None,
                value: Some(json!({"bad": true})),
                artifact: None,
                ttl_ms: 60_000,
            }),
            list: None,
            subscribe: None,
            exists: None,
            delete: None,
            wait: None,
        })
        .expect_err("multiple specs must fail");
        assert_eq!(error_code(&extra), Some(error_codes::TOOL_PARAMS_INVALID));
    }
}
