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
    time::Duration,
};

use chrono::Utc;
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::{DataPredicate, Event, EventFilter, EventSource, error_codes};
use synapse_reflex::PublishReport;
use synapse_storage::{Db, RevisionGuard, cf};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

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
/// Lower/upper bounds for the async `wait` budget. Values outside the range fail
/// closed (rejected, never silently clamped).
const MIN_WORKSPACE_WAIT_TIMEOUT_MS: u64 = 1;
const MAX_WORKSPACE_WAIT_TIMEOUT_MS: u64 = 60_000;
/// Default and bounds for the `wait` poll cadence against CF_KV.
const DEFAULT_WORKSPACE_WAIT_POLL_INTERVAL_MS: u64 = 50;
const MIN_WORKSPACE_WAIT_POLL_INTERVAL_MS: u64 = 1;
const MAX_WORKSPACE_WAIT_POLL_INTERVAL_MS: u64 = 5_000;
/// One cancellable wait poll performs one exact CF_KV read on Tokio's blocking
/// pool. Expired-row mutation is admitted separately and then retained through
/// its guarded delete/readback; a normal read cannot retain the routed request
/// indefinitely.
const MAX_WORKSPACE_WAIT_STORAGE_POLL_MS: u64 = 1_000;
const MAX_CONCURRENT_WORKSPACE_BLOCKING_OPERATIONS: usize = 32;
/// Physical namespace scans are streamed in fixed pages. The response retains
/// at most the public list limit; expired cleanup is committed per page.
const WORKSPACE_SCAN_PAGE_ROWS: usize = 128;

static NEXT_WORKSPACE_EVENT_SEQ: AtomicU64 = AtomicU64::new(1);
static WORKSPACE_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static WORKSPACE_BLOCKING_OPERATION_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| {
        Arc::new(tokio::sync::Semaphore::new(
            MAX_CONCURRENT_WORKSPACE_BLOCKING_OPERATIONS,
        ))
    });

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
    /// Compatibility field retained for response stability. Always zero:
    /// authoritative corrupt rows now fail the operation explicitly.
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
    /// Exact Calyx generation used for every row in this multi-page list.
    pub snapshot_seq: u64,
    /// Retention-evaluation time frozen when the coherent scan was pinned.
    pub snapshot_read_at_unix_ms: u64,
    pub scanned_rows: usize,
    pub expired_rows_deleted: usize,
    /// Compatibility field retained for response stability. Always empty:
    /// authoritative corrupt rows now return `STORAGE_CORRUPTED` with exact
    /// physical key/revision diagnostics instead of a partial list.
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
    revision_sha256: [u8; 32],
}

#[derive(Default)]
struct WorkspaceCleanupReport {
    expired_rows_deleted: usize,
}

enum WorkspaceWaitPoll {
    Present(Box<DecodedWorkspaceRow>),
    Absent(WorkspaceAbsentReadback),
    Expired(WorkspaceRawRow),
}

#[derive(Clone, Copy)]
enum WorkspaceBlockingMode {
    ReadOnly,
    MutationCapable { stage: &'static str },
}

struct AbortBlockingTaskOnDrop<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortBlockingTaskOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self { handle }
    }

    fn handle_mut(&mut self) -> &mut tokio::task::JoinHandle<T> {
        &mut self.handle
    }

    fn abort(&self) {
        self.handle.abort();
    }
}

impl<T> Drop for AbortBlockingTaskOnDrop<T> {
    fn drop(&mut self) {
        // `spawn_blocking` work cannot be stopped after it begins, but aborting
        // the handle prevents queued work from starting. A running closure
        // retains its moved semaphore permit until physical completion.
        self.handle.abort();
    }
}

#[tool_router(router = workspace_blackboard_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for run-scoped workspace blackboard operations in the <=40 public MCP surface. operation is one of get, put, list, subscribe, exists, delete, or wait. Exactly one matching operation spec is accepted. Mutating operations return CF_KV or subscription readback metadata; absent keys are reported as WORKSPACE_KEY_ABSENT instead of generic storage corruption/read failures. get.absent_ok=true (default false) turns tolerated absence into a SUCCESS with found=false and a CF_KV absent_readback proof instead of the WORKSPACE_KEY_ABSENT error, for expected-absence polling. wait asynchronously suspends until the key becomes present (returning the same entry/value readback as get), routed cancellation arrives, or its bounded timeout_ms elapses; every exact storage poll and concurrent poll count is bounded. list/cleanup page CF_KV and fail with STORAGE_CORRUPTED plus exact key/revision diagnostics rather than omitting an authoritative corrupt row."
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
                let request = params.0.get.ok_or_else(|| missing_workspace_spec("get"))?;
                let worker = self.clone();
                let worker_session_id = session_id.clone();
                let response = workspace_blocking_call(
                    "get",
                    WorkspaceBlockingMode::MutationCapable {
                        stage: "workspace_get_expiry_cleanup_before_blocking_storage",
                    },
                    move || worker.workspace_get_impl(request, &worker_session_id),
                )
                .await?;
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
                let request = params.0.put.ok_or_else(|| missing_workspace_spec("put"))?;
                let worker = self.clone();
                let worker_session_id = session_id.clone();
                let response = workspace_blocking_call(
                    "put",
                    WorkspaceBlockingMode::MutationCapable {
                        stage: "workspace_put_before_blocking_storage",
                    },
                    move || worker.workspace_put_impl(request, &worker_session_id),
                )
                .await?;
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
                let request = params
                    .0
                    .list
                    .ok_or_else(|| missing_workspace_spec("list"))?;
                let worker = self.clone();
                let worker_session_id = session_id.clone();
                let response = workspace_blocking_call(
                    "list",
                    WorkspaceBlockingMode::MutationCapable {
                        stage: "workspace_list_expiry_cleanup_before_blocking_storage",
                    },
                    move || worker.workspace_list_impl(request, &worker_session_id),
                )
                .await?;
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
                let request = params
                    .0
                    .exists
                    .ok_or_else(|| missing_workspace_spec("exists"))?;
                let worker = self.clone();
                let worker_session_id = session_id.clone();
                let response =
                    workspace_blocking_call("exists", WorkspaceBlockingMode::ReadOnly, move || {
                        worker.workspace_exists_impl(request, &worker_session_id)
                    })
                    .await?;
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
                let request = params
                    .0
                    .delete
                    .ok_or_else(|| missing_workspace_spec("delete"))?;
                let worker = self.clone();
                let worker_session_id = session_id.clone();
                let response = workspace_blocking_call(
                    "delete",
                    WorkspaceBlockingMode::MutationCapable {
                        stage: "workspace_delete_before_blocking_storage",
                    },
                    move || worker.workspace_delete_impl(request, &worker_session_id),
                )
                .await?;
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
                let response = self
                    .workspace_wait_impl(
                        params
                            .0
                            .wait
                            .ok_or_else(|| missing_workspace_spec("wait"))?,
                        &session_id,
                    )
                    .await?;
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
        let worker = self.clone();
        workspace_blocking_call(
            "put",
            WorkspaceBlockingMode::MutationCapable {
                stage: "workspace_put_before_blocking_storage",
            },
            move || worker.workspace_put_impl(params.0, &session_id).map(Json),
        )
        .await
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
        let worker = self.clone();
        workspace_blocking_call(
            "get",
            WorkspaceBlockingMode::MutationCapable {
                stage: "workspace_get_expiry_cleanup_before_blocking_storage",
            },
            move || worker.workspace_get_impl(params.0, &session_id).map(Json),
        )
        .await
    }

    #[tool(
        description = "List run-scoped blackboard entries from durable CF_KV storage using candidate-bounded physical Calyx pages. Any authoritative corrupt row fails the operation with STORAGE_CORRUPTED plus exact key/revision diagnostics; corrupt rows are never omitted from a partial success."
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
        let worker = self.clone();
        workspace_blocking_call(
            "list",
            WorkspaceBlockingMode::MutationCapable {
                stage: "workspace_list_expiry_cleanup_before_blocking_storage",
            },
            move || worker.workspace_list_impl(params.0, &session_id).map(Json),
        )
        .await
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
            "expired_rows_deleted_before": cleanup.expired_rows_deleted,
            "corrupt_rows_skipped_before": 0,
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
            expired_rows_deleted_before = cleanup.expired_rows_deleted,
            corrupt_rows_skipped_before = 0,
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
            expired_rows_deleted_before: cleanup.expired_rows_deleted,
            corrupt_rows_skipped_before: 0,
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

    async fn workspace_wait_impl(
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
        let cancellation = workspace_request_cancellation_token()?;
        let row_key = workspace_row_key(&run_id, &key);
        let timeout_ms = params.timeout_ms;
        let poll_interval_ms = params.poll_interval_ms;
        let started_at_unix_ms = unix_time_ms_now();
        let started_at = Instant::now();
        let deadline = started_at + Duration::from_millis(timeout_ms);
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
            let poll = workspace_wait_storage_poll(
                Arc::clone(&db),
                row_key.clone(),
                now_unix_ms,
                deadline,
                &cancellation,
                &run_id,
                &key,
                session_id,
                started_at,
                poll_count,
            )
            .await?;
            match poll {
                WorkspaceWaitPoll::Present(row) => {
                    let row = *row;
                    let storage_readback = workspace_row_readback(&row);
                    let waited_ms = monotonic_elapsed_ms(started_at);
                    let resolved_at_unix_ms = unix_time_ms_now();
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
                        now_unix_ms: resolved_at_unix_ms,
                        waited_ms,
                        poll_count,
                        timeout_ms,
                        poll_interval_ms,
                        entry: row.entry,
                        storage_readback,
                    });
                }
                WorkspaceWaitPoll::Absent(absent_readback) => {
                    if Instant::now() >= deadline {
                        let waited_ms = monotonic_elapsed_ms(started_at);
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
                }
                WorkspaceWaitPoll::Expired(_raw) => {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "workspace wait expired-row state escaped its guarded cleanup boundary",
                    ));
                }
            }
            let next_poll =
                (Instant::now() + Duration::from_millis(poll_interval_ms)).min(deadline);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(workspace_wait_cancelled_error(
                        &run_id,
                        &key,
                        &row_key,
                        session_id,
                        "poll_interval",
                        monotonic_elapsed_ms(started_at),
                        poll_count,
                    ));
                }
                () = tokio::time::sleep_until(next_poll) => {}
            }
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
        let scan = scan_workspace_run(
            &db,
            &run_id,
            now_unix_ms,
            Some(&prefix),
            params.limit,
            "workspace list",
        )?;
        let mut rows = scan.rows;
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
            snapshot_seq = scan.snapshot_seq,
            snapshot_read_at_unix_ms = scan.snapshot_read_at_unix_ms,
            scanned_rows = scan.scanned_rows,
            expired_rows_deleted = scan.expired_rows_deleted,
            corrupt_rows_skipped = 0,
            returned_count = entries.len(),
            "readback=workspace_blackboard edge=list_read"
        );

        Ok(WorkspaceListResponse {
            ok: true,
            run_id,
            prefix,
            values_included: params.include_values,
            now_unix_ms,
            snapshot_seq: scan.snapshot_seq,
            snapshot_read_at_unix_ms: scan.snapshot_read_at_unix_ms,
            scanned_rows: scan.scanned_rows,
            expired_rows_deleted: scan.expired_rows_deleted,
            corrupt_rows_skipped: Vec::new(),
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

fn workspace_request_cancellation_token() -> Result<CancellationToken, ErrorData> {
    super::operator_panic_boundary::MCP_REQUEST_CANCELLATION
        .try_with(Clone::clone)
        .map_err(|_missing_task_local| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "workspace wait reached the MCP router without its request/daemon cancellation token",
            )
        })
}

async fn workspace_blocking_call<T, F>(
    operation: &'static str,
    mode: WorkspaceBlockingMode,
    work: F,
) -> Result<T, ErrorData>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ErrorData> + Send + 'static,
{
    let cancellation = workspace_request_cancellation_token()?;
    if cancellation.is_cancelled() {
        return Err(workspace_operation_cancelled_error(
            operation,
            "before_blocking_capacity",
        ));
    }
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            return Err(workspace_operation_cancelled_error(
                operation,
                "waiting_for_blocking_capacity",
            ));
        }
        acquired = Arc::clone(&WORKSPACE_BLOCKING_OPERATION_PERMITS).acquire_owned() => {
            acquired.map_err(|_closed| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "workspace {operation} blocking-operation semaphore was unexpectedly closed"
                    ),
                )
            })?
        }
    };
    if cancellation.is_cancelled() {
        return Err(workspace_operation_cancelled_error(
            operation,
            "before_blocking_dispatch",
        ));
    }
    if let WorkspaceBlockingMode::MutationCapable { stage } = mode {
        // Reserve routed ownership only after scarce blocking capacity exists
        // and immediately before dispatch. Caller/daemon cancellation can
        // still win before this point; after it, the outer routed authority
        // retains this exact task through physical mutation and readback.
        super::operator_panic_boundary::ensure_mcp_mutation(stage)?;
    }
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    let join_error = |error: tokio::task::JoinError| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("workspace {operation} blocking storage task failed: {error}"),
        )
    };
    match mode {
        WorkspaceBlockingMode::MutationCapable { .. } => task.await.map_err(join_error)?,
        WorkspaceBlockingMode::ReadOnly => {
            let mut task = AbortBlockingTaskOnDrop::new(task);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    task.abort();
                    Err(workspace_operation_cancelled_error(
                        operation,
                        "during_blocking_storage",
                    ))
                }
                joined = task.handle_mut() => joined.map_err(join_error)?,
            }
        }
    }
}

fn monotonic_elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[allow(clippy::too_many_arguments)]
async fn workspace_wait_storage_poll(
    db: Arc<Db>,
    row_key: String,
    now_unix_ms: u64,
    wait_deadline: Instant,
    cancellation: &CancellationToken,
    run_id: &str,
    key: &str,
    session_id: &str,
    started_at: Instant,
    poll_count: u64,
) -> Result<WorkspaceWaitPoll, ErrorData> {
    if cancellation.is_cancelled() {
        return Err(workspace_wait_cancelled_error(
            run_id,
            key,
            &row_key,
            session_id,
            "before_storage_poll",
            monotonic_elapsed_ms(started_at),
            poll_count,
        ));
    }
    let now = Instant::now();
    let storage_ceiling = now + Duration::from_millis(MAX_WORKSPACE_WAIT_STORAGE_POLL_MS);
    let poll_deadline = if wait_deadline > now {
        wait_deadline.min(storage_ceiling)
    } else {
        storage_ceiling
    };
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            return Err(workspace_wait_cancelled_error(
                run_id,
                key,
                &row_key,
                session_id,
                "waiting_for_storage_poll_capacity",
                monotonic_elapsed_ms(started_at),
                poll_count,
            ));
        }
        acquired = Arc::clone(&WORKSPACE_BLOCKING_OPERATION_PERMITS).acquire_owned() => {
            acquired.map_err(|_closed| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "workspace wait storage-poll semaphore was unexpectedly closed",
                )
            })?
        }
        () = tokio::time::sleep_until(poll_deadline) => {
            return Err(workspace_wait_storage_timeout_error(
                run_id,
                key,
                &row_key,
                session_id,
                "waiting_for_poll_capacity",
                monotonic_elapsed_ms(started_at),
                poll_count,
            ));
        }
    };
    let blocking_db = Arc::clone(&db);
    let blocking_row_key = row_key.clone();
    let mut task = AbortBlockingTaskOnDrop::new(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        poll_workspace_wait_storage_sync(&blocking_db, &blocking_row_key, now_unix_ms)
    }));
    let poll = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            task.abort();
            Err(workspace_wait_cancelled_error(
                run_id,
                key,
                &row_key,
                session_id,
                "during_storage_poll",
                monotonic_elapsed_ms(started_at),
                poll_count,
            ))
        }
        joined = task.handle_mut() => {
            joined.map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "workspace wait bounded storage poll task failed: row_key={row_key:?} poll_count={poll_count}: {error}"
                    ),
                )
            })?
        }
        () = tokio::time::sleep_until(poll_deadline) => {
            task.abort();
            Err(workspace_wait_storage_timeout_error(
                run_id,
                key,
                &row_key,
                session_id,
                "executing_exact_poll",
                monotonic_elapsed_ms(started_at),
                poll_count,
            ))
        }
    }?;
    match poll {
        WorkspaceWaitPoll::Expired(raw) => {
            cleanup_expired_workspace_row_for_wait(
                db,
                raw,
                wait_deadline,
                cancellation,
                run_id,
                key,
                session_id,
                started_at,
                poll_count,
            )
            .await
        }
        resolved => Ok(resolved),
    }
}

fn poll_workspace_wait_storage_sync(
    db: &Db,
    row_key: &str,
    now_unix_ms: u64,
) -> Result<WorkspaceWaitPoll, ErrorData> {
    let Some(raw) = read_workspace_raw_row_optional(db, row_key)? else {
        return Ok(WorkspaceWaitPoll::Absent(workspace_absent_readback(
            row_key,
        )));
    };
    let row = decode_workspace_row(raw.key.clone(), raw.encoded.clone())
        .map_err(|detail| workspace_corrupt_error(&raw, "workspace wait poll", detail))?;
    if row.entry.expires_at_unix_ms > now_unix_ms {
        return Ok(WorkspaceWaitPoll::Present(Box::new(row)));
    }
    Ok(WorkspaceWaitPoll::Expired(raw))
}

#[allow(clippy::too_many_arguments)]
async fn cleanup_expired_workspace_row_for_wait(
    db: Arc<Db>,
    raw: WorkspaceRawRow,
    wait_deadline: Instant,
    cancellation: &CancellationToken,
    run_id: &str,
    key: &str,
    session_id: &str,
    started_at: Instant,
    poll_count: u64,
) -> Result<WorkspaceWaitPoll, ErrorData> {
    let row_key_display = String::from_utf8_lossy(&raw.key).to_string();
    let now = Instant::now();
    let capacity_deadline = if wait_deadline > now {
        wait_deadline.min(now + Duration::from_millis(MAX_WORKSPACE_WAIT_STORAGE_POLL_MS))
    } else {
        now + Duration::from_millis(MAX_WORKSPACE_WAIT_STORAGE_POLL_MS)
    };
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            return Err(workspace_wait_cancelled_error(
                run_id,
                key,
                &row_key_display,
                session_id,
                "before_expired_cleanup_admission",
                monotonic_elapsed_ms(started_at),
                poll_count,
            ));
        }
        acquired = Arc::clone(&WORKSPACE_BLOCKING_OPERATION_PERMITS).acquire_owned() => {
            acquired.map_err(|_closed| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "workspace wait storage-poll semaphore closed before expired-row cleanup",
                )
            })?
        }
        () = tokio::time::sleep_until(capacity_deadline) => {
            return Err(workspace_wait_storage_timeout_error(
                run_id,
                key,
                &row_key_display,
                session_id,
                "waiting_for_expired_cleanup_capacity",
                monotonic_elapsed_ms(started_at),
                poll_count,
            ));
        }
    };
    if cancellation.is_cancelled() {
        return Err(workspace_wait_cancelled_error(
            run_id,
            key,
            &row_key_display,
            session_id,
            "before_expired_cleanup_mutation",
            monotonic_elapsed_ms(started_at),
            poll_count,
        ));
    }
    super::operator_panic_boundary::ensure_mcp_mutation(
        "workspace_wait_expired_row_guarded_cleanup",
    )?;
    let blocking_db = Arc::clone(&db);
    let row_key = row_key_display;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        cleanup_expired_workspace_row_for_wait_sync(&blocking_db, &raw)
    });
    // Mutation ownership is now armed. The routed request must retain this
    // exact guarded delete/readback to terminal even if cancellation arrives;
    // dropping a spawn_blocking handle cannot stop an already-running commit.
    task.await.map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "workspace wait expired-row cleanup task failed after mutation admission: row_key={row_key:?}: {error}"
            ),
        )
    })?
}

fn cleanup_expired_workspace_row_for_wait_sync(
    db: &Db,
    raw: &WorkspaceRawRow,
) -> Result<WorkspaceWaitPoll, ErrorData> {
    let row_key = String::from_utf8_lossy(&raw.key).to_string();
    let deleted = delete_expired_workspace_row_for_wait(db, raw)?;
    let Some(current) = read_workspace_raw_row_optional(db, &row_key)? else {
        return Ok(WorkspaceWaitPoll::Absent(workspace_absent_readback(
            &row_key,
        )));
    };
    let current_row =
        decode_workspace_row(current.key.clone(), current.encoded.clone()).map_err(|detail| {
            workspace_corrupt_error(
                &current,
                "workspace wait expired-row cleanup readback",
                detail,
            )
        })?;
    let now_unix_ms = unix_time_ms_now();
    if current_row.entry.expires_at_unix_ms > now_unix_ms {
        return Ok(WorkspaceWaitPoll::Present(Box::new(current_row)));
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        "workspace wait expired-row cleanup did not converge to an absent or live exact row",
        Some(json!({
            "code": error_codes::STORAGE_WRITE_FAILED,
            "detail_code": "WORKSPACE_WAIT_EXPIRED_CLEANUP_NOT_CONVERGED",
            "row_key": &row_key,
            "row_key_hex": hex_bytes(row_key.as_bytes()),
            "guarded_delete_applied": deleted,
            "current_revision_sha256": hex_bytes(&current.revision_sha256),
            "current_value_sha256": hash_bytes(&current.encoded),
            "current_expires_at_unix_ms": current_row.entry.expires_at_unix_ms,
            "now_unix_ms": now_unix_ms,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "remediation": "inspect the exact CF_KV row and retry after the concurrent workspace writer or cleanup operation completes",
        })),
    ))
}

fn delete_expired_workspace_row_for_wait(
    db: &Db,
    raw: &WorkspaceRawRow,
) -> Result<bool, ErrorData> {
    let outcome = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [RevisionGuard::new(
                raw.key.clone(),
                Some(raw.revision_sha256),
            )],
            [raw.key.clone()],
            std::iter::empty::<(Vec<u8>, Vec<u8>)>(),
        )
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "workspace wait guarded expired-row cleanup failed: row_key_hex={} revision_sha256={}: {error}",
                    hex_bytes(&raw.key),
                    hex_bytes(&raw.revision_sha256)
                ),
            )
        })?;
    Ok(outcome.applied)
}

fn workspace_absent_readback(row_key: &str) -> WorkspaceAbsentReadback {
    WorkspaceAbsentReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        exists: false,
        exact_match_count: 0,
    }
}

struct WorkspaceRunScan {
    snapshot_seq: u64,
    snapshot_read_at_unix_ms: u64,
    scanned_rows: usize,
    expired_rows_deleted: usize,
    rows: Vec<DecodedWorkspaceRow>,
}

fn finish_workspace_coherent_scan<T>(
    db: &Db,
    lease: &mut synapse_storage::CoherentScanLease,
    operation: &'static str,
    scan_result: Result<T, ErrorData>,
) -> Result<T, ErrorData> {
    let lease_id = lease.lease_id;
    let snapshot_seq = lease.snapshot_seq;
    let release_result = db.release_coherent_scan(lease);
    match (scan_result, release_result) {
        (Err(scan_error), Ok(_)) => Err(scan_error),
        (Err(scan_error), Err(release_error)) => Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "{operation}: {}; additionally failed to release coherent workspace scan lease_id={lease_id} snapshot_seq={snapshot_seq}: {release_error}",
                scan_error.message
            ),
        )),
        (Ok(_), Err(release_error)) => Err(mcp_error(
            release_error.code(),
            format!(
                "{operation}: failed to release coherent workspace scan lease_id={lease_id} snapshot_seq={snapshot_seq}: {release_error}"
            ),
        )),
        (Ok(_), Ok(false)) => Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "{operation}: coherent workspace snapshot expired before release lease_id={lease_id} snapshot_seq={snapshot_seq}; repeat the bounded operation"
            ),
        )),
        (Ok(value), Ok(true)) => Ok(value),
    }
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
    result_prefix: Option<&str>,
    result_limit: usize,
    operation: &'static str,
) -> Result<WorkspaceRunScan, ErrorData> {
    let prefix = workspace_run_prefix(run_id).into_bytes();
    let mut lease = db
        .pin_cf_physical_scan(cf::CF_KV, synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{operation} could not pin a coherent workspace scan: {error}"),
            )
        })?;
    let lease_id = lease.lease_id;
    let snapshot_seq = lease.snapshot_seq;
    let snapshot_read_at_unix_ms = lease.read_at_unix_ms;
    let mut scanned_rows = 0_usize;
    let mut expired_rows_deleted = 0_usize;
    let mut decoded_rows = Vec::new();
    let scan_result = (|| -> Result<(), ErrorData> {
        loop {
            let page = db
                .scan_cf_physical_page_coherent(&mut lease, WORKSPACE_SCAN_PAGE_ROWS)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "{operation} coherent physical workspace page read failed: lease_id={lease_id} snapshot_seq={snapshot_seq} page_rows={WORKSPACE_SCAN_PAGE_ROWS}: {error}"
                        ),
                    )
                })?;
            let page_snapshot_seq = page.snapshot_seq.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "{operation} coherent workspace page omitted its pinned snapshot sequence: lease_id={lease_id} expected_snapshot_seq={snapshot_seq}"
                    ),
                )
            })?;
            if page_snapshot_seq != snapshot_seq {
                return Err(mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "{operation} coherent workspace page escaped its pinned generation: lease_id={lease_id} expected_snapshot_seq={snapshot_seq} actual_snapshot_seq={page_snapshot_seq}"
                    ),
                ));
            }
            let mut expired_rows = Vec::new();
            for (key, scanned_value) in page.rows {
                if !key.starts_with(&prefix) {
                    continue;
                }
                scanned_rows = scanned_rows.saturating_add(1);
                let snapshot_raw = WorkspaceRawRow {
                    key: key.clone(),
                    revision_sha256: Sha256::digest(&scanned_value).into(),
                    encoded: scanned_value.clone(),
                };
                let row = decode_workspace_row(key.clone(), scanned_value.clone())
                    .map_err(|detail| workspace_corrupt_error(&snapshot_raw, operation, detail))?;
                if row.entry.expires_at_unix_ms <= now_unix_ms {
                    if let Some(current) = read_workspace_raw_row_by_key(db, &key, operation)? {
                        if current.encoded == scanned_value {
                            expired_rows.push((row.key, current.revision_sha256));
                        } else {
                            tracing::debug!(
                                code = "WORKSPACE_EXPIRED_CLEANUP_SNAPSHOT_SUPERSEDED",
                                operation,
                                lease_id,
                                snapshot_seq,
                                row_key_hex = %hex_bytes(&key),
                                snapshot_value_sha256 = %hash_bytes(&scanned_value),
                                current_value_sha256 = %hash_bytes(&current.encoded),
                                current_revision_sha256 = %hex_bytes(&current.revision_sha256),
                                "did not delete a workspace row superseded after the pinned list snapshot"
                            );
                        }
                    }
                } else if result_limit != 0
                    && result_prefix.is_none_or(|prefix| row.entry.key.starts_with(prefix))
                {
                    decoded_rows.push(row);
                }
            }
            // Physical Calyx order includes the encoded user-key length and is
            // deliberately opaque. Retain the lexicographically smallest
            // bounded logical result set from this one pinned generation so
            // public ordering/limit semantics do not depend on physical layout.
            if decoded_rows.len() > result_limit {
                decoded_rows.sort_by(|left, right| left.entry.key.cmp(&right.entry.key));
                decoded_rows.truncate(result_limit);
            }
            expired_rows_deleted = expired_rows_deleted.saturating_add(
                delete_workspace_rows_if_revisions(db, expired_rows, operation)?,
            );
            if !page.more {
                break;
            }
        }
        Ok(())
    })();
    finish_workspace_coherent_scan(db, &mut lease, operation, scan_result)?;
    tracing::debug!(
        code = "WORKSPACE_COHERENT_SCAN_COMPLETED",
        operation,
        run_id,
        lease_id,
        snapshot_seq,
        snapshot_read_at_unix_ms,
        scanned_rows,
        expired_rows_deleted,
        "workspace multi-page enumeration completed and released one pinned Calyx generation"
    );
    Ok(WorkspaceRunScan {
        snapshot_seq,
        snapshot_read_at_unix_ms,
        scanned_rows,
        expired_rows_deleted,
        rows: decoded_rows,
    })
}

fn cleanup_expired_workspace_rows(
    db: &Db,
    run_id: &str,
    now_unix_ms: u64,
) -> Result<WorkspaceCleanupReport, ErrorData> {
    let scan = scan_workspace_run(
        db,
        run_id,
        now_unix_ms,
        None,
        0,
        "workspace put expired-row cleanup",
    )?;
    Ok(WorkspaceCleanupReport {
        expired_rows_deleted: scan.expired_rows_deleted,
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
    decode_workspace_row(row.key.clone(), row.encoded.clone())
        .map(Some)
        .map_err(|detail| workspace_corrupt_error(&row, "workspace exact-row read", detail))
}

fn read_workspace_raw_row_optional(
    db: &Db,
    row_key: &str,
) -> Result<Option<WorkspaceRawRow>, ErrorData> {
    read_workspace_raw_row_by_key(db, row_key.as_bytes(), "workspace exact-row read")
}

fn read_workspace_raw_row_by_key(
    db: &Db,
    key: &[u8],
    operation: &'static str,
) -> Result<Option<WorkspaceRawRow>, ErrorData> {
    let Some(physical) = db.get_cf_revisioned(cf::CF_KV, key).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "{operation} physical workspace point-read failed: row_key_hex={}: {error}",
                hex_bytes(key)
            ),
        )
    })?
    else {
        return Ok(None);
    };
    let Some(encoded) = physical.value else {
        return Ok(None);
    };
    Ok(Some(WorkspaceRawRow {
        key: key.to_vec(),
        encoded,
        revision_sha256: physical.revision_sha256,
    }))
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
    let expected_row_key = workspace_row_key(&entry.run_id, &entry.key);
    if expected_row_key != row_key {
        return Err(format!(
            "workspace row identity does not derive from its payload: actual={row_key} expected={expected_row_key} run_id={:?} key={:?}",
            entry.run_id, entry.key
        ));
    }
    if entry.version == 0
        || entry.ttl_ms == 0
        || entry.ttl_ms > MAX_WORKSPACE_TTL_MS
        || entry.updated_at_unix_ms < entry.created_at_unix_ms
        || entry.expires_at_unix_ms != entry.updated_at_unix_ms.saturating_add(entry.ttl_ms)
        || entry.writer_session_id.trim().is_empty()
        || (entry.value.is_none() && entry.artifact.is_none())
    {
        return Err(format!(
            "workspace row {row_key} violates payload invariants: version={} ttl_ms={} created_at={} updated_at={} expires_at={} writer_session_empty={} payload_empty={}",
            entry.version,
            entry.ttl_ms,
            entry.created_at_unix_ms,
            entry.updated_at_unix_ms,
            entry.expires_at_unix_ms,
            entry.writer_session_id.trim().is_empty(),
            entry.value.is_none() && entry.artifact.is_none()
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
        .get_cf(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
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
    let exists = db
        .get_cf(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .is_some();
    Ok(WorkspaceAbsentReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        exists,
        exact_match_count: usize::from(exists),
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

fn delete_workspace_rows_if_revisions(
    db: &Db,
    rows: Vec<(Vec<u8>, [u8; 32])>,
    operation: &'static str,
) -> Result<usize, ErrorData> {
    if rows.is_empty() {
        return Ok(0);
    }
    let guards = rows
        .iter()
        .map(|(key, revision)| RevisionGuard::new(key.clone(), Some(*revision)))
        .collect::<Vec<_>>();
    let deletes = rows
        .iter()
        .map(|(key, _revision)| key.clone())
        .collect::<Vec<_>>();
    let outcome = db
        .mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            guards,
            deletes,
            std::iter::empty::<(Vec<u8>, Vec<u8>)>(),
        )
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{operation} guarded expired-row cleanup failed for {} rows: {error}",
                    rows.len()
                ),
            )
        })?;
    if !outcome.applied {
        let conflict = outcome.conflict.as_ref();
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "{operation} refused stale expired-row cleanup because an authoritative CF_KV revision changed"
            ),
            Some(json!({
                "code": error_codes::STORAGE_WRITE_FAILED,
                "detail_code": "WORKSPACE_EXPIRED_CLEANUP_REVISION_CONFLICT",
                "operation": operation,
                "row_count": rows.len(),
                "conflict_guard_index": conflict.map(|value| value.guard_index),
                "conflict_row_key_hex": conflict.map(|value| hex_bytes(&value.key)),
                "expected_revision_sha256": conflict
                    .and_then(|value| value.expected_revision_sha256)
                    .map(|value| hex_bytes(&value)),
                "actual_revision_sha256": conflict
                    .and_then(|value| value.actual_revision_sha256)
                    .map(|value| hex_bytes(&value)),
                "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
                "remediation": "retry the workspace operation so cleanup rereads the current exact row revision; no stale row was deleted",
            })),
        ));
    }
    Ok(rows.len())
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

fn workspace_operation_cancelled_error(operation: &'static str, stage: &'static str) -> ErrorData {
    tracing::info!(
        code = "WORKSPACE_OPERATION_CANCELLED",
        operation,
        stage,
        "workspace operation stopped at routed request/daemon cancellation before physical mutation admission"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("workspace {operation} was cancelled during {stage}"),
        Some(json!({
            "code": error_codes::DAEMON_RESTARTING,
            "detail_code": "WORKSPACE_OPERATION_CANCELLED",
            "operation": operation,
            "stage": stage,
            "source_of_truth": "MCP_REQUEST_CANCELLATION + WORKSPACE_BLOCKING_OPERATION_PERMITS",
            "remediation": "issue a new workspace request after routed authority is live; cancellation won before mutation admission, so no physical workspace mutation was dispatched. A read-only blocking call already running may finish while retaining its bounded semaphore permit, but its result is discarded",
        })),
    )
}

#[allow(clippy::too_many_arguments)]
fn workspace_wait_cancelled_error(
    run_id: &str,
    key: &str,
    row_key: &str,
    session_id: &str,
    stage: &'static str,
    waited_ms: u64,
    poll_count: u64,
) -> ErrorData {
    tracing::info!(
        code = "WORKSPACE_WAIT_CANCELLED",
        run_id,
        key,
        row_key,
        waiter_session_id = session_id,
        stage,
        waited_ms,
        poll_count,
        "workspace wait stopped at routed request/daemon cancellation"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "workspace blackboard wait for key {key:?} in run {run_id:?} was cancelled during {stage} after {waited_ms}ms"
        ),
        Some(json!({
            "code": error_codes::DAEMON_RESTARTING,
            "detail_code": "WORKSPACE_WAIT_CANCELLED",
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "waiter_session_id": session_id,
            "stage": stage,
            "waited_ms": waited_ms,
            "poll_count": poll_count,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "remediation": "after the daemon/request authority is live again, issue a new workspace wait; no new poll is scheduled after cancellation. An exact read or revision-guarded expired-row cleanup already running on Tokio's bounded blocking pool may finish, and its physical CF_KV result remains authoritative",
        })),
    )
}

#[allow(clippy::too_many_arguments)]
fn workspace_wait_storage_timeout_error(
    run_id: &str,
    key: &str,
    row_key: &str,
    session_id: &str,
    stage: &'static str,
    waited_ms: u64,
    poll_count: u64,
) -> ErrorData {
    tracing::error!(
        code = error_codes::STORAGE_READ_FAILED,
        detail_code = "WORKSPACE_WAIT_STORAGE_POLL_TIMEOUT",
        run_id,
        key,
        row_key,
        waiter_session_id = session_id,
        stage,
        waited_ms,
        poll_count,
        storage_poll_timeout_ms = MAX_WORKSPACE_WAIT_STORAGE_POLL_MS,
        "workspace wait exact CF_KV poll exceeded its bounded blocking budget"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "workspace blackboard wait bounded CF_KV poll exceeded its deadline during {stage} for key {key:?} in run {run_id:?}"
        ),
        Some(json!({
            "code": error_codes::STORAGE_READ_FAILED,
            "detail_code": "WORKSPACE_WAIT_STORAGE_POLL_TIMEOUT",
            "run_id": run_id,
            "key": key,
            "row_key": row_key,
            "waiter_session_id": session_id,
            "stage": stage,
            "waited_ms": waited_ms,
            "poll_count": poll_count,
            "storage_poll_timeout_ms": MAX_WORKSPACE_WAIT_STORAGE_POLL_MS,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "remediation": "inspect Calyx process/socket/disk health and the exact CF_KV row; do not treat this as key absence or retry until storage reads respond",
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

fn workspace_corrupt_error(
    row: &WorkspaceRawRow,
    operation: &'static str,
    detail: String,
) -> ErrorData {
    let row_key = String::from_utf8_lossy(&row.key).to_string();
    let row_key_utf8 = std::str::from_utf8(&row.key).ok();
    let value_sha256 = hash_bytes(&row.encoded);
    let revision_sha256 = hex_bytes(&row.revision_sha256);
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{operation} found an authoritative corrupt workspace blackboard row {row_key:?} at physical revision {revision_sha256}: {detail}"
        ),
        Some(json!({
            "code": error_codes::STORAGE_CORRUPTED,
            "detail_code": "WORKSPACE_AUTHORITATIVE_ROW_CORRUPTED",
            "operation": operation,
            "row_key": row_key,
            "row_key_utf8": row_key_utf8,
            "row_key_hex": hex_bytes(&row.key),
            "physical_revision_sha256": revision_sha256,
            "value_len_bytes": row.encoded.len(),
            "value_sha256": value_sha256,
            "detail": detail,
            "source_of_truth": WORKSPACE_SOURCE_OF_TRUTH,
            "required_action": "inspect_and_repair_or_delete_exact_physical_row",
            "remediation": "inspect CF_KV using row_key_hex and physical_revision_sha256; if the bytes are irreparable and row_key_utf8 is present, delete only that exact row through workspace operation=delete with raw_row_key and expected_corrupt_sha256 equal to value_sha256; otherwise use the storage repair surface for the exact binary key. Do not trust a partial list or retry a put until this row is resolved",
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
