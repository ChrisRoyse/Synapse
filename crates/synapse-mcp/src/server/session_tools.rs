//! Cross-session registry MCP tools for multi-agent coordination (#794).
//!
//! The registry is a read model: HTTP lifecycle/heartbeat state is joined with
//! the existing active-target registry and input lease snapshot at read time.
//! It does not gate any action/perception path.

use std::collections::{BTreeMap, BTreeSet};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_action::lease;
use synapse_core::error_codes;
use synapse_storage::cf;

use super::{
    ErrorData, Json, Parameters, SessionTarget, SynapseService, TargetWire,
    agent_state::{AgentAttentionClass, AgentLifecycleState, AgentStateRead},
    mcp_error,
    session_registry::{SessionRegistryRead, SpawnedAgentRead, unix_time_ms_now},
    target_claims::{self, TargetClaimRead},
    tool, tool_router,
    url_redaction::redact_url_for_public_readback,
};

const ATTACHED_AGENT_REGISTRY_SOURCE_OF_TRUTH: &str = "session_registry spawned_agent rows + agent_state tracker rows + OS process table live-pid probe + visible top-level window enumeration";
const SESSION_TARGET_ROW_PREFIX: &str = "mcp/session-target/v1/";
const SESSION_LIST_DEFAULT_LIMIT: usize = 50;
const SESSION_LIST_MAX_LIMIT: usize = 500;
const DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS: u64 = 60_000;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    /// Include explicitly closed sessions. Live and stale sessions are always
    /// included because stale peers are part of the crash/disconnect readback.
    #[serde(default)]
    #[schemars(default)]
    pub include_closed: bool,
    /// Return only sessions whose registry lifecycle is `live`.
    #[serde(default)]
    #[schemars(default)]
    pub live_only: bool,
    /// Response projection. `compact` is the MCP default to keep orchestrator
    /// reads under response limits; `full` preserves the verbose session rows.
    #[serde(default = "default_session_list_view")]
    #[schemars(default = "default_session_list_view")]
    pub view: SessionListView,
    /// Zero-based cursor into the filtered, sorted session list.
    #[serde(default)]
    #[schemars(default)]
    pub cursor: usize,
    /// Maximum sessions to return. Omit for the default compact page size.
    /// Use `view=full` with an explicit higher limit only when needed.
    #[serde(default)]
    #[schemars(default)]
    pub limit: Option<usize>,
    /// Include verbose attached-agent registry rows. Counts are always present.
    #[serde(default)]
    #[schemars(default)]
    pub include_attached_agent_rows: bool,
    /// Include terminal/dead unbound agent history. Counts are always present.
    #[serde(default)]
    #[schemars(default)]
    pub include_terminal_unbound_history: bool,
}

impl Default for SessionListParams {
    fn default() -> Self {
        Self {
            include_closed: false,
            live_only: false,
            view: default_session_list_view(),
            cursor: 0,
            limit: None,
            include_attached_agent_rows: false,
            include_terminal_unbound_history: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionListView {
    Compact,
    Full,
}

const fn default_session_list_view() -> SessionListView {
    SessionListView::Compact
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusParams {
    /// MCP Streamable HTTP session id to inspect.
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionEndParams {
    /// Optional explicit session id. Cross-session teardown is restricted to
    /// cleanup-required stale rows or verified dead/quiet live resource owners.
    #[serde(default)]
    #[schemars(default)]
    pub session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionOperation {
    List,
}

impl SessionOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
        }
    }
}

impl Default for SessionOperation {
    fn default() -> Self {
        default_session_operation()
    }
}

const fn default_session_operation() -> SessionOperation {
    SessionOperation::List
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    #[serde(default = "default_session_operation")]
    #[schemars(default = "default_session_operation")]
    pub operation: SessionOperation,
    #[serde(default, flatten)]
    pub list: SessionListParams,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionResponse {
    pub operation: SessionOperation,
    pub source_of_truth: &'static str,
    pub list: SessionListResponse,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionLeaseReadback {
    pub held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    pub is_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired_at_ms_ago: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renewed_at_ms_ago: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersistedCdpTargetOwnerReadback {
    pub source_of_truth: String,
    pub row_key: String,
    pub owner_key: String,
    pub owner_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_client_name: Option<String>,
    pub owner_agent_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_started_at_unix_ms: Option<u64>,
    pub stored_at_unix_ms: u64,
    pub target: TargetWire,
    pub window_hwnd: i64,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_window_hwnd: Option<i64>,
    pub cdp_target_id: String,
    pub requested_url: String,
    pub target_url: String,
    pub created_at_unix_ms: u64,
    pub target_live: PersistedCdpTargetOwnerLiveReadback,
    pub cleanup_action: String,
    pub recovery_guidance: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersistedCdpTargetOwnerLiveReadback {
    pub source_of_truth: String,
    pub status: String,
    pub stale_orphan: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    #[serde(flatten)]
    pub registry: SessionRegistryRead,
    /// Legacy alias for agent_logical_foreground.target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_target: Option<TargetWire>,
    pub agent_logical_foreground: AgentLogicalForegroundReadback,
    pub foreground_lane: ForegroundLaneReadback,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_claims: Vec<TargetClaimRead>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persisted_cdp_target_owners: Vec<PersistedCdpTargetOwnerReadback>,
    pub lease: SessionLeaseReadback,
    /// #898 lifecycle state machine read for this session's agent: state,
    /// reason code, heartbeat, waiting_for detail, runaway flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<AgentStateRead>,
    #[serde(default, skip_serializing_if = "AgentAttentionClass::is_none")]
    pub attention_class: AgentAttentionClass,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListResponse {
    pub now_unix_ms: u64,
    pub stale_after_ms: u64,
    pub view: SessionListView,
    pub include_closed: bool,
    pub live_only: bool,
    pub cursor: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    pub total_count: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_sections: Vec<&'static str>,
    pub human_os_foreground: HumanOsForegroundReadback,
    pub foreground_lane_capacity: ForegroundLaneCapacityReadback,
    pub registry_entry_count: usize,
    pub target_session_count: usize,
    pub returned_count: usize,
    pub input_lease_held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_lease_owner_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_sessions: Vec<SessionListCompactRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionSummary>,
    /// #1035 K1: authoritative live attached-terminal/agent registry. The
    /// exact count is OS-probed live process rows only; observed ambient rows
    /// without a process handle stay visible but cannot inflate the count.
    pub attached_agent_registry: AttachedAgentRegistryReadback,
    /// #898: agents tracked by the state machine that have no MCP session
    /// (in-flight spawns and active attention rows before registration).
    /// Terminal/dead history is split out below so default consumers do not
    /// page on already-ended agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unbound_agent_states: Vec<AgentStateRead>,
    /// Terminal unbound history retained for diagnostics. These rows are not
    /// actionable attention and must not be counted as stuck/live work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_unbound_agent_states: Vec<AgentStateRead>,
    pub unbound_agent_filter: SessionUnboundAgentFilterReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListCompactRow {
    pub session_id: String,
    pub agent_kind: String,
    pub lifecycle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    pub transport: String,
    pub last_seen_ms_ago: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_agent_id: Option<String>,
    pub agent_logical_foreground_status: String,
    pub foreground_lane_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_lane_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_target_key: Option<String>,
    pub target_claim_count: usize,
    pub persisted_cdp_target_owner_count: usize,
    pub lease_is_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "AgentAttentionClass::is_none")]
    pub attention_class: AgentAttentionClass,
}

#[derive(Clone, Copy, Debug)]
struct SessionListOptions {
    include_closed: bool,
    live_only: bool,
    view: SessionListView,
    cursor: usize,
    limit: Option<usize>,
    include_attached_agent_rows: bool,
    include_terminal_unbound_history: bool,
}

impl SessionListOptions {
    fn from_tool_params(params: SessionListParams) -> Result<Self, ErrorData> {
        let limit = params.limit.unwrap_or(SESSION_LIST_DEFAULT_LIMIT);
        if limit == 0 {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "session_list limit must be greater than zero",
            ));
        }
        if limit > SESSION_LIST_MAX_LIMIT {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("session_list limit must be <= {SESSION_LIST_MAX_LIMIT}, got {limit}"),
            ));
        }
        Ok(Self {
            include_closed: params.include_closed,
            live_only: params.live_only,
            view: params.view,
            cursor: params.cursor,
            limit: Some(limit),
            include_attached_agent_rows: params.include_attached_agent_rows,
            include_terminal_unbound_history: params.include_terminal_unbound_history,
        })
    }

    const fn full_internal(include_closed: bool) -> Self {
        Self {
            include_closed,
            live_only: false,
            view: SessionListView::Full,
            cursor: 0,
            limit: None,
            include_attached_agent_rows: true,
            include_terminal_unbound_history: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUnboundAgentFilterReadback {
    pub source_of_truth: &'static str,
    pub active_unbound_agent_count: usize,
    pub terminal_unbound_agent_count: usize,
    pub terminal_states: Vec<&'static str>,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachedAgentRegistryReadback {
    pub source_of_truth: &'static str,
    pub count_basis: &'static str,
    pub generated_at_unix_ms: u64,
    pub exact_live_count: usize,
    pub fleet_stop_managed_count: usize,
    pub unmanaged_live_count: usize,
    pub row_count: usize,
    pub killable_live_count: usize,
    pub unprobeable_observed_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<AttachedAgentRegistryRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_lookup_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachedAgentRegistryRow {
    pub registry_id: String,
    pub kind: String,
    pub source: String,
    pub lifecycle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "AgentAttentionClass::is_none")]
    pub attention_class: AgentAttentionClass,
    pub counts_as_live: bool,
    pub fleet_stop_managed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_counted_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms_ago: Option<u64>,
    pub process: AttachedAgentProcessReadback,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_window: Option<AttachedAgentWindowReadback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controlling_terminal_window: Option<AttachedAgentWindowReadback>,
    pub kill_handle: AttachedAgentKillHandleReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachedAgentProcessReadback {
    pub probeable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub process_tree_ids: Vec<u32>,
    pub live_process_ids: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachedAgentWindowReadback {
    pub window_hwnd: i64,
    pub process_id: u32,
    pub process_name: String,
    pub window_title: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachedAgentKillHandleReadback {
    pub available: bool,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusResponse {
    pub now_unix_ms: u64,
    pub stale_after_ms: u64,
    pub human_os_foreground: HumanOsForegroundReadback,
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSummary>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentLogicalForegroundReadback {
    pub source_of_truth: String,
    pub session_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_row_key: Option<String>,
    pub no_human_os_foreground_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForegroundLaneReadback {
    pub source_of_truth: String,
    pub session_id: String,
    pub status: String,
    pub capacity_model: String,
    pub capacity_exhausted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_claim: Option<TargetClaimRead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    pub explicit_real_foreground_lease: bool,
    pub no_human_os_foreground_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForegroundLaneCapacityReadback {
    pub source_of_truth: &'static str,
    pub capacity_model: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_lane_pool_limit: Option<usize>,
    pub active_agent_logical_foreground_count: usize,
    pub active_foreground_lane_count: usize,
    pub claimed_target_lane_count: usize,
    pub explicit_real_foreground_lease_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_daemon_lane_slots: Option<usize>,
    pub capacity_exhausted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausted_reason: Option<String>,
    pub target_backed_lane_kinds: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HumanOsForegroundReadback {
    pub source_of_truth: &'static str,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionEndResponse {
    pub report: crate::server::session_lifecycle::SessionTeardownReport,
}

#[tool_router(router = session_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Public session facade. operation=list reads the MCP session registry joined with session-target rows, target claims, agent logical foreground, input lease, and compact pagination. Unknown operations fail schema validation; this facade is read-only and never ends sessions."
    )]
    pub async fn session(
        &self,
        params: Parameters<SessionParams>,
    ) -> Result<Json<SessionResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session",
            operation = params.operation.as_str(),
            "tool.invocation kind=session"
        );
        match params.operation {
            SessionOperation::List => Ok(Json(SessionResponse {
                operation: SessionOperation::List,
                source_of_truth: ATTACHED_AGENT_REGISTRY_SOURCE_OF_TRUTH,
                list: self.session_list_impl_with_options(SessionListOptions::from_tool_params(
                    params.list,
                )?)?,
            })),
        }
    }

    #[tool(
        description = "List MCP sessions as a non-blocking cross-session read model. Defaults to a compact paginated projection for orchestrators; pass view=full and explicit include_* flags for verbose diagnostics. Supports include_closed, live_only, cursor, and limit. Stale sessions are reported unless filtered; agent logical foreground never falls back to the human OS foreground."
    )]
    pub async fn session_list(
        &self,
        params: Parameters<SessionListParams>,
    ) -> Result<Json<SessionListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session_list",
            "tool.invocation kind=session_list"
        );
        self.session_list_impl_with_options(SessionListOptions::from_tool_params(params.0)?)
            .map(Json)
    }

    #[tool(
        description = "Return one MCP session's registry row joined with agent_logical_foreground, foreground_lane, human_os_foreground, target claims, and input-lease state. Unknown sessions return found=false instead of blocking or scanning external state; missing agent logical foreground is reported explicitly and never replaced with the human OS foreground."
    )]
    pub async fn session_status(
        &self,
        params: Parameters<SessionStatusParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SessionStatusResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session_status",
            "tool.invocation kind=session_status"
        );
        validate_session_id(&params.0.session_id)?;
        self.session_status_impl(&params.0.session_id).map(Json)
    }

    #[tool(
        description = "Explicitly end this MCP session and atomically reclaim all resources owned by it: held inputs, input lease, active target, virtual clipboard buffer, CDP targets, durable shell jobs, launched process resources, event subscriptions, persisted session row, and registry lifecycle. The optional session_id may target this caller's session, a stale/non-live cleanup_required session, or a live peer only when its attached agent is terminal/dead, the registry row is quiet, it owns cleanup resources, has no target claim/input lease/conflicting owner, and the daemon in-flight ledger is empty; healthy live peers and terminal-no-resource peers fail closed."
    )]
    pub async fn session_end(
        &self,
        params: Parameters<SessionEndParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SessionEndResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "session_end",
            "tool.invocation kind=session_end"
        );
        let current_session_id = super::context::mcp_session_id_from_request_context(
            &request_context,
        )?
        .ok_or_else(|| {
            mcp_error(
                error_codes::HTTP_SESSION_INVALID,
                "session_end requires an MCP session id",
            )
        })?;
        let params = params.0;
        let requested_session_id = params.session_id.clone();
        let target_session_id = match requested_session_id.clone() {
            Some(session_id) => {
                validate_session_id(&session_id)?;
                if session_id != current_session_id {
                    let status = self.session_status_impl(&session_id)?;
                    ensure_cross_session_cleanup_allowed(
                        &current_session_id,
                        &session_id,
                        &status,
                    )?;
                }
                session_id
            }
            None => current_session_id.clone(),
        };
        let command_payload = json!({
            "requested_session_id": &requested_session_id,
            "target_session_id": &target_session_id,
        });
        let command_before = json!({
            "source_of_truth": "session lifecycle registry, input lease, target/session-owned resources",
            "target_session_id": &target_session_id,
            "session_status": self.session_status_impl(&target_session_id).ok(),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "session_end",
            "kill",
            Some(current_session_id.clone()),
            Some(target_session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let lifecycle = self.session_lifecycle_state()?;
        let report = match lifecycle
            .teardown_session(&target_session_id, "explicit_session_end")
            .await
        {
            Ok(report) => report,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "session_end",
                        "kill",
                        Some(current_session_id.clone()),
                        Some(target_session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": "session lifecycle registry, input lease, target/session-owned resources",
                            "session_status": self.session_status_impl(&target_session_id).ok(),
                        }),
                        "error",
                    )
                    .with_error(super::command_audit::command_audit_error_from_error_data(
                        &error,
                    )),
                )?;
                return Err(error);
            }
        };
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "session_end",
            "kill",
            Some(current_session_id.clone()),
            Some(target_session_id.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": "session lifecycle registry, input lease, target/session-owned resources",
                "report": &report,
                "session_status": self.session_status_impl(&target_session_id).ok(),
            }),
            "ok",
        ))?;
        Ok(Json(SessionEndResponse { report }))
    }
}

impl SynapseService {
    pub(crate) fn session_list_impl(
        &self,
        include_closed: bool,
    ) -> Result<SessionListResponse, ErrorData> {
        self.session_list_impl_with_options(SessionListOptions::full_internal(include_closed))
    }

    fn session_list_impl_with_options(
        &self,
        options: SessionListOptions,
    ) -> Result<SessionListResponse, ErrorData> {
        let now_unix_ms = unix_time_ms_now();
        let (registry_reads, stale_after_ms, registry_entry_count) =
            self.session_registry_reads(now_unix_ms)?;
        let memory_targets = self.session_targets()?;
        let persisted_target_session_ids = self.persisted_session_target_session_ids()?;
        let persisted_cdp_owner_session_ids = self.persisted_cdp_target_owner_session_ids()?;
        let persisted_cdp_owners_by_session =
            self.persisted_cdp_target_owner_readbacks_by_session(&persisted_cdp_owner_session_ids)?;
        let all_target_claims = self.target_claim_status_snapshot()?.claims;
        let target_claims_by_owner = target_claim_reads_by_owner(&all_target_claims);
        let lease_status = lease::status();
        let mut session_ids = registry_reads
            .keys()
            .chain(memory_targets.keys())
            .chain(persisted_target_session_ids.iter())
            .chain(persisted_cdp_owner_session_ids.iter())
            .chain(target_claims_by_owner.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(owner) = lease_status.owner_session_id.as_ref() {
            session_ids.insert(owner.clone());
        }
        let mut targets = BTreeMap::new();
        for session_id in &session_ids {
            if let Some(target) = self.agent_logical_foreground_read_model(session_id)? {
                targets.insert(session_id.clone(), target);
            }
        }
        let mut sessions = Vec::new();
        for session_id in session_ids {
            let persisted_cdp_target_owners = persisted_cdp_owners_by_session
                .get(&session_id)
                .cloned()
                .unwrap_or_default();
            let Some(mut summary) = build_session_summary(
                &session_id,
                registry_reads.get(&session_id).cloned(),
                targets.get(&session_id).cloned(),
                target_claims_by_owner
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default(),
                &all_target_claims,
                &lease_status,
                now_unix_ms,
                stale_after_ms,
                !persisted_cdp_target_owners.is_empty(),
            ) else {
                continue;
            };
            summary.persisted_cdp_target_owners = persisted_cdp_target_owners;
            if !options.include_closed && summary.registry.lifecycle == "closed" {
                continue;
            }
            if options.live_only && summary.registry.lifecycle != "live" {
                continue;
            }
            sessions.push(summary);
        }
        sessions.sort_by(|a, b| a.registry.session_id.cmp(&b.registry.session_id));
        let total_count = sessions.len();
        let cursor = options.cursor.min(total_count);
        let end = options
            .limit
            .map(|limit| cursor.saturating_add(limit).min(total_count))
            .unwrap_or(total_count);
        let page_sessions = sessions[cursor..end].to_vec();
        let returned_count = page_sessions.len();
        let has_more = end < total_count;
        let next_cursor = has_more.then_some(end);
        let foreground_lane_capacity =
            build_foreground_lane_capacity(&sessions, &all_target_claims, &lease_status);
        let raw_unbound_agent_states = super::agent_state::unbound_reads(now_unix_ms);
        let (unbound_agent_states, terminal_unbound_agent_states, unbound_agent_filter) =
            split_unbound_agent_states(raw_unbound_agent_states);
        let mut attached_agent_registry =
            build_attached_agent_registry(&sessions, &unbound_agent_states, now_unix_ms);
        if !options.include_attached_agent_rows {
            attached_agent_registry.rows.clear();
        }
        let terminal_rows_omitted =
            !options.include_terminal_unbound_history && !terminal_unbound_agent_states.is_empty();
        let returned_terminal_unbound_agent_states = if options.include_terminal_unbound_history {
            terminal_unbound_agent_states
        } else {
            Vec::new()
        };
        let compact_sessions = if options.view == SessionListView::Compact {
            page_sessions.iter().map(compact_session_row).collect()
        } else {
            Vec::new()
        };
        let full_sessions = if options.view == SessionListView::Full {
            page_sessions
        } else {
            Vec::new()
        };
        let mut omitted_sections = Vec::new();
        if options.view == SessionListView::Compact {
            omitted_sections.push("sessions");
        } else {
            omitted_sections.push("compact_sessions");
        }
        if !options.include_attached_agent_rows {
            omitted_sections.push("attached_agent_registry.rows");
        }
        if terminal_rows_omitted {
            omitted_sections.push("terminal_unbound_agent_states");
        }
        Ok(SessionListResponse {
            now_unix_ms,
            stale_after_ms,
            view: options.view,
            include_closed: options.include_closed,
            live_only: options.live_only,
            cursor,
            limit: options.limit,
            total_count,
            has_more,
            next_cursor,
            omitted_sections,
            human_os_foreground: self.human_os_foreground_readback(),
            foreground_lane_capacity,
            registry_entry_count,
            target_session_count: targets.len(),
            returned_count,
            input_lease_held: lease_status.held,
            input_lease_owner_session_id: lease_status.owner_session_id,
            compact_sessions,
            sessions: full_sessions,
            attached_agent_registry,
            unbound_agent_states,
            terminal_unbound_agent_states: returned_terminal_unbound_agent_states,
            unbound_agent_filter,
        })
    }

    pub(crate) fn session_status_impl(
        &self,
        session_id: &str,
    ) -> Result<SessionStatusResponse, ErrorData> {
        let now_unix_ms = unix_time_ms_now();
        let (registry_reads, stale_after_ms, _registry_entry_count) =
            self.session_registry_reads(now_unix_ms)?;
        let active_target = self.agent_logical_foreground_read_model(session_id)?;
        let all_target_claims = self.target_claim_status_snapshot()?.claims;
        let target_claims = target_claim_reads_by_owner(&all_target_claims)
            .remove(session_id)
            .unwrap_or_default();
        let persisted_cdp_target_owners =
            self.persisted_cdp_target_owner_readbacks_for_session(session_id)?;
        let has_persisted_cdp_owner = !persisted_cdp_target_owners.is_empty();
        let lease_status = lease::status();
        let mut session = build_session_summary(
            session_id,
            registry_reads.get(session_id).cloned(),
            active_target,
            target_claims,
            &all_target_claims,
            &lease_status,
            now_unix_ms,
            stale_after_ms,
            has_persisted_cdp_owner,
        );
        if let Some(summary) = session.as_mut() {
            summary.persisted_cdp_target_owners = persisted_cdp_target_owners;
        }
        Ok(SessionStatusResponse {
            now_unix_ms,
            stale_after_ms,
            human_os_foreground: self.human_os_foreground_readback(),
            found: session.is_some(),
            session,
        })
    }

    fn session_registry_reads(
        &self,
        now_unix_ms: u64,
    ) -> Result<(BTreeMap<String, SessionRegistryRead>, u64, usize), ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned",
            )
        })?;
        let stale_after_ms = guard.stale_after_ms();
        let reads = guard
            .reads(now_unix_ms)
            .into_iter()
            .map(|entry| (entry.session_id.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let count = reads.len();
        drop(guard);
        Ok((reads, stale_after_ms, count))
    }

    fn session_targets(&self) -> Result<BTreeMap<String, SessionTarget>, ErrorData> {
        let guard = self.session_targets_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session target registry lock poisoned",
            )
        })?;
        let targets = guard
            .iter()
            .map(|(session_id, target)| (session_id.clone(), target.clone()))
            .collect::<BTreeMap<_, _>>();
        drop(guard);
        Ok(targets)
    }

    pub(crate) fn agent_logical_foreground_read_model(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        if let Some(target) = self.memory_session_target(session_id)? {
            return Ok(Some(target));
        }
        self.persisted_session_target_read_model(session_id)
    }

    fn persisted_session_target_session_ids(&self) -> Result<BTreeSet<String>, ErrorData> {
        let db = self.m3_storage()?;
        let rows = db
            .scan_cf_prefix(cf::CF_SESSIONS, SESSION_TARGET_ROW_PREFIX.as_bytes())
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let mut session_ids = BTreeSet::new();
        for (row_key, _value) in rows {
            if let Some(session_id) = session_id_from_target_row_key(&row_key)? {
                session_ids.insert(session_id);
            }
        }
        Ok(session_ids)
    }

    fn persisted_cdp_target_owner_readbacks_by_session(
        &self,
        session_ids: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Vec<PersistedCdpTargetOwnerReadback>>, ErrorData> {
        let mut by_session = BTreeMap::new();
        for session_id in session_ids {
            let readbacks = self.persisted_cdp_target_owner_readbacks_for_session(session_id)?;
            if !readbacks.is_empty() {
                by_session.insert(session_id.clone(), readbacks);
            }
        }
        Ok(by_session)
    }

    fn persisted_cdp_target_owner_readbacks_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedCdpTargetOwnerReadback>, ErrorData> {
        let m3_state = self.m3_state_handle();
        let rows = super::session_continuity::read_persisted_cdp_target_owners_for_session(
            &m3_state, session_id,
        )
        .map_err(|detail| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "read persisted CDP target owner rows for session {session_id:?}: {detail}"
                ),
            )
        })?;
        Ok(rows
            .into_iter()
            .map(|(owner_key, row)| persisted_cdp_target_owner_readback(owner_key, row))
            .collect())
    }

    pub(crate) fn human_os_foreground_readback(&self) -> HumanOsForegroundReadback {
        match self.current_audit_foreground() {
            Ok(foreground) => HumanOsForegroundReadback {
                source_of_truth: "GetForegroundWindow + foreground process/window context; human OS foreground only",
                status: "observed".to_owned(),
                hwnd: Some(foreground.hwnd),
                pid: Some(foreground.pid),
                process_name: Some(foreground.process_name),
                process_path: Some(foreground.process_path),
                window_title: Some(foreground.window_title),
                read_error_code: None,
                read_error_message: None,
            },
            Err(error) => HumanOsForegroundReadback {
                source_of_truth: "GetForegroundWindow + foreground process/window context; human OS foreground only",
                status: "read_error".to_owned(),
                hwnd: None,
                pid: None,
                process_name: None,
                process_path: None,
                window_title: None,
                read_error_code: error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                read_error_message: Some(error.message.to_string()),
            },
        }
    }
}

fn persisted_cdp_target_owner_readback(
    owner_key: String,
    row: super::session_continuity::PersistedCdpTargetOwner,
) -> PersistedCdpTargetOwnerReadback {
    let row_key = super::session_continuity::persisted_cdp_target_owner_row_key_string(
        &owner_key,
        &row.owner.cdp_target_id,
    );
    let target_live =
        persisted_cdp_target_owner_live_readback(row.owner.window_hwnd, &row.owner.cdp_target_id);
    let stale_orphan = target_live.stale_orphan;
    let cleanup_action = if stale_orphan {
        format!(
            "stale_orphan: session list is read-only and did not delete this row; public close/cleanup may delete CF_SESSIONS:{row_key} only after a separate Chrome bridge tabs.query/readback proves target {} is absent",
            row.owner.cdp_target_id
        )
    } else {
        format!(
            "call session_end with session_id={} while the Chrome bridge is healthy; browser_tabs/cdp_close_tab recovery requires an exact target_claim, active CDP session target, or exact explicit CDP target authority",
            row.owner_session_id
        )
    };
    let recovery_guidance = if stale_orphan {
        "row points at an absent or unreadable browser window; do not silently drop it from read-only session list. Retry public close only with the exact original window_hwnd/cdp_target_id after Chrome bridge readback is available, or leave the row visible for forensic storage cleanup."
            .to_owned()
    } else {
        "durable owner row remains until cleanup closes the tab or proves it already absent; use the already-open authenticated Chrome bridge, never foreground the human browser or launch a second Chrome profile"
            .to_owned()
    };
    PersistedCdpTargetOwnerReadback {
        source_of_truth: format!("CF_SESSIONS:{row_key}"),
        row_key,
        owner_key,
        owner_session_id: row.owner_session_id.clone(),
        owner_client_name: row.owner_client_name,
        owner_agent_kind: row.owner_agent_kind,
        owner_started_at_unix_ms: row.owner_started_at_unix_ms,
        stored_at_unix_ms: row.stored_at_unix_ms,
        target: TargetWire::Cdp {
            window_hwnd: row.owner.window_hwnd,
            cdp_target_id: row.owner.cdp_target_id.clone(),
        },
        window_hwnd: row.owner.window_hwnd,
        endpoint: row.owner.endpoint,
        chrome_window_id: row.owner.chrome_window_id,
        capture_window_hwnd: row.owner.capture_window_hwnd,
        cdp_target_id: row.owner.cdp_target_id,
        requested_url: redact_url_for_public_readback(&row.owner.requested_url),
        target_url: redact_url_for_public_readback(&row.owner.target_url),
        created_at_unix_ms: row.owner.created_at_unix_ms,
        target_live,
        cleanup_action,
        recovery_guidance,
    }
}

fn persisted_cdp_target_owner_live_readback(
    window_hwnd: i64,
    _cdp_target_id: &str,
) -> PersistedCdpTargetOwnerLiveReadback {
    match super::m1_tools::validate_target_window(window_hwnd) {
        Ok((window_title, process_name)) => PersistedCdpTargetOwnerLiveReadback {
            source_of_truth:
                "synapse_capture::validate_hwnd + UI foreground_context for owner window; target existence is not checked by read-only session list"
                    .to_owned(),
            status: "window_present_target_not_checked".to_owned(),
            stale_orphan: false,
            window_title: Some(window_title),
            process_name: Some(process_name),
            read_error_code: None,
            read_error_message: None,
        },
        Err(error) => PersistedCdpTargetOwnerLiveReadback {
            source_of_truth:
                "synapse_capture::validate_hwnd + UI foreground_context for owner window"
                    .to_owned(),
            status: "window_absent_or_unreadable".to_owned(),
            stale_orphan: true,
            window_title: None,
            process_name: None,
            read_error_code: error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            read_error_message: Some(error.message.to_string()),
        },
    }
}

fn compact_session_row(summary: &SessionSummary) -> SessionListCompactRow {
    SessionListCompactRow {
        session_id: summary.registry.session_id.clone(),
        agent_kind: summary.registry.agent_kind.clone(),
        lifecycle: summary.registry.lifecycle.clone(),
        client_name: summary.registry.client_name.clone(),
        transport: summary.registry.transport.clone(),
        last_seen_ms_ago: summary.registry.last_seen_ms_ago,
        last_action: summary.registry.last_action.clone(),
        last_reason_code: summary.registry.last_reason_code.clone(),
        spawned_agent_id: summary
            .registry
            .spawned_agent
            .as_ref()
            .map(|spawned| spawned.spawn_id.clone()),
        agent_logical_foreground_status: summary.agent_logical_foreground.status.clone(),
        foreground_lane_status: summary.foreground_lane.status.clone(),
        foreground_lane_kind: summary.foreground_lane.lane_kind.clone(),
        foreground_target_key: summary.foreground_lane.target_key.clone(),
        target_claim_count: summary.target_claims.len(),
        persisted_cdp_target_owner_count: summary.persisted_cdp_target_owners.len(),
        lease_is_owner: summary.lease.is_owner,
        agent_state: summary
            .agent_state
            .as_ref()
            .map(|state| state.state.as_str().to_owned()),
        agent_reason_code: summary
            .agent_state
            .as_ref()
            .and_then(|state| state.reason_code.clone()),
        attention_class: summary.attention_class,
    }
}

fn split_unbound_agent_states(
    rows: Vec<AgentStateRead>,
) -> (
    Vec<AgentStateRead>,
    Vec<AgentStateRead>,
    SessionUnboundAgentFilterReadback,
) {
    let mut active_rows = Vec::new();
    let mut terminal_rows = Vec::new();
    for row in rows {
        if unbound_agent_row_is_terminal(&row) {
            terminal_rows.push(row);
        } else {
            active_rows.push(row);
        }
    }
    let filter = SessionUnboundAgentFilterReadback {
        source_of_truth: "agent_state::unbound_reads split by lifecycle state",
        active_unbound_agent_count: active_rows.len(),
        terminal_unbound_agent_count: terminal_rows.len(),
        terminal_states: vec!["dead", "ambient_without_process_handle"],
        reason: "terminal unbound history is diagnostic history, not actionable attention",
    };
    (active_rows, terminal_rows, filter)
}

fn unbound_agent_row_is_terminal(row: &AgentStateRead) -> bool {
    matches!(row.state, AgentLifecycleState::Dead)
        || row.attention_class.is_terminal_history()
        || unbound_ambient_agent_row_has_no_process_handle(row)
}

fn unbound_ambient_agent_row_has_no_process_handle(row: &AgentStateRead) -> bool {
    agent_state_is_ambient(row) && !agent_state_has_process_handle(row)
}

fn build_attached_agent_registry(
    sessions: &[SessionSummary],
    unbound_agent_states: &[AgentStateRead],
    now_unix_ms: u64,
) -> AttachedAgentRegistryReadback {
    let (windows_by_pid, window_lookup_error) = attached_agent_window_index();
    let ambient_process_candidates =
        ambient_agent_process_candidates(&windows_by_pid, &BTreeSet::new());
    build_attached_agent_registry_with_process_probe(
        sessions,
        unbound_agent_states,
        now_unix_ms,
        &|pid| crate::m4::owned_process_tree_ids(pid),
        &|process_ids| crate::m4::owned_live_process_ids(process_ids),
        &windows_by_pid,
        window_lookup_error,
        ambient_process_candidates,
    )
}

fn build_attached_agent_registry_with_process_probe(
    sessions: &[SessionSummary],
    unbound_agent_states: &[AgentStateRead],
    now_unix_ms: u64,
    process_tree_ids: &dyn Fn(u32) -> Vec<u32>,
    live_process_ids: &dyn Fn(&[u32]) -> Vec<u32>,
    windows_by_pid: &BTreeMap<u32, AttachedAgentWindowReadback>,
    window_lookup_error: Option<String>,
    ambient_process_candidates: Vec<AmbientAgentProcessCandidate>,
) -> AttachedAgentRegistryReadback {
    let mut rows = BTreeMap::<String, AttachedAgentRegistryRow>::new();
    for summary in sessions {
        if let Some(spawned) = summary.registry.spawned_agent.as_ref() {
            insert_attached_agent_row(
                &mut rows,
                attached_row_from_spawned_session(
                    &summary.registry,
                    spawned,
                    summary.agent_state.as_ref(),
                    process_tree_ids,
                    live_process_ids,
                    windows_by_pid,
                ),
            );
        } else if let Some(agent_state) = summary.agent_state.as_ref()
            && agent_state_has_process_handle(agent_state)
        {
            insert_attached_agent_row(
                &mut rows,
                attached_row_from_agent_state(
                    agent_state,
                    Some(&summary.registry),
                    "session_agent_state",
                    process_tree_ids,
                    live_process_ids,
                    windows_by_pid,
                ),
            );
        }
    }
    for agent_state in unbound_agent_states {
        if !agent_state_has_process_handle(agent_state) && !agent_state_is_ambient(agent_state) {
            continue;
        }
        insert_attached_agent_row(
            &mut rows,
            attached_row_from_agent_state(
                agent_state,
                None,
                if agent_state_is_ambient(agent_state) {
                    "ambient_transcript"
                } else {
                    "unbound_agent_state"
                },
                process_tree_ids,
                live_process_ids,
                windows_by_pid,
            ),
        );
    }
    let represented_process_ids = rows
        .values()
        .flat_map(|row| {
            row.process
                .process_tree_ids
                .iter()
                .chain(row.process.live_process_ids.iter())
                .copied()
        })
        .collect::<BTreeSet<_>>();
    insert_ambient_agent_process_rows(
        &mut rows,
        ambient_process_candidates
            .into_iter()
            .filter(|candidate| !represented_process_ids.contains(&candidate.process_id))
            .collect(),
    );

    let rows = rows.into_values().collect::<Vec<_>>();
    let exact_live_count = rows.iter().filter(|row| row.counts_as_live).count();
    let fleet_stop_managed_count = rows.iter().filter(|row| row.fleet_stop_managed).count();
    let unmanaged_live_count = rows
        .iter()
        .filter(|row| row.counts_as_live && !row.fleet_stop_managed)
        .count();
    let killable_live_count = rows
        .iter()
        .filter(|row| row.counts_as_live && row.kill_handle.available)
        .count();
    let unprobeable_observed_count = rows.iter().filter(|row| !row.process.probeable).count();
    AttachedAgentRegistryReadback {
        source_of_truth: ATTACHED_AGENT_REGISTRY_SOURCE_OF_TRUTH,
        count_basis: "exact_live_count counts rows whose live_process_ids are non-empty; fleet_stop_managed_count/killable_live_count count rows with a Synapse-owned kill handle; unmanaged_live_count is live OS evidence outside fleet_stop control",
        generated_at_unix_ms: now_unix_ms,
        exact_live_count,
        fleet_stop_managed_count,
        unmanaged_live_count,
        row_count: rows.len(),
        killable_live_count,
        unprobeable_observed_count,
        rows,
        window_lookup_error,
    }
}

fn attached_row_from_spawned_session(
    registry: &SessionRegistryRead,
    spawned: &SpawnedAgentRead,
    agent_state: Option<&AgentStateRead>,
    process_tree_ids: &dyn Fn(u32) -> Vec<u32>,
    live_process_ids: &dyn Fn(&[u32]) -> Vec<u32>,
    windows_by_pid: &BTreeMap<u32, AttachedAgentWindowReadback>,
) -> AttachedAgentRegistryRow {
    let process = attached_process_readback(
        Some(spawned.launcher_process_id),
        spawned.agent_process_id,
        process_tree_ids,
        live_process_ids,
    );
    let visible_window = attached_visible_window(&process, windows_by_pid);
    let state = agent_state.map(|row| row.state.as_str().to_owned());
    let reason_code = agent_state.and_then(|row| row.reason_code.clone());
    let (counts_as_live, not_counted_reason) = attached_count_decision(&process);
    let attention_class =
        attached_attention_class(agent_state, &registry.lifecycle, counts_as_live);
    let target_id = Some(spawned.spawn_id.clone());
    let fleet_stop_managed = counts_as_live;
    AttachedAgentRegistryRow {
        registry_id: spawned.spawn_id.clone(),
        kind: spawned.cli.clone(),
        source: "session_registry.spawned_agent".to_owned(),
        lifecycle: registry.lifecycle.clone(),
        state,
        reason_code,
        attention_class,
        counts_as_live,
        fleet_stop_managed,
        not_counted_reason,
        session_id: Some(registry.session_id.clone()),
        spawn_id: Some(spawned.spawn_id.clone()),
        spawn_dir: Some(spawned.log_dir.clone()),
        last_seen_unix_ms: Some(registry.last_seen_unix_ms),
        last_seen_ms_ago: Some(registry.last_seen_ms_ago),
        process,
        visible_window: visible_window.clone(),
        controlling_terminal_window: visible_window,
        kill_handle: attached_kill_handle(counts_as_live, target_id, true),
    }
}

fn attached_row_from_agent_state(
    row: &AgentStateRead,
    registry: Option<&SessionRegistryRead>,
    source: &str,
    process_tree_ids: &dyn Fn(u32) -> Vec<u32>,
    live_process_ids: &dyn Fn(&[u32]) -> Vec<u32>,
    windows_by_pid: &BTreeMap<u32, AttachedAgentWindowReadback>,
) -> AttachedAgentRegistryRow {
    let process = attached_process_readback(
        row.launcher_process_id,
        row.agent_process_id,
        process_tree_ids,
        live_process_ids,
    );
    let visible_window = attached_visible_window(&process, windows_by_pid);
    let (counts_as_live, not_counted_reason) = attached_count_decision(&process);
    let lifecycle = registry
        .map(|registry| registry.lifecycle.clone())
        .unwrap_or_else(|| "unbound".to_owned());
    let attention_class = attached_attention_class(Some(row), &lifecycle, counts_as_live);
    let target_id = row
        .spawn_id
        .clone()
        .or_else(|| row.session_id.clone())
        .or_else(|| (!agent_state_is_ambient(row)).then(|| row.anchor.clone()));
    let agent_kill_can_resolve = row.session_id.is_some() || registry.is_some();
    let fleet_stop_managed = counts_as_live && agent_kill_can_resolve;
    AttachedAgentRegistryRow {
        registry_id: row
            .spawn_id
            .clone()
            .or_else(|| row.session_id.clone())
            .unwrap_or_else(|| row.anchor.clone()),
        kind: row
            .agent_kind
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        source: source.to_owned(),
        lifecycle,
        state: Some(row.state.as_str().to_owned()),
        reason_code: row.reason_code.clone(),
        attention_class,
        counts_as_live,
        fleet_stop_managed,
        not_counted_reason,
        session_id: row.session_id.clone(),
        spawn_id: row.spawn_id.clone(),
        spawn_dir: row.log_dir.clone(),
        last_seen_unix_ms: Some(row.last_event_unix_ms),
        last_seen_ms_ago: Some(row.silent_ms),
        process,
        visible_window: visible_window.clone(),
        controlling_terminal_window: visible_window,
        kill_handle: attached_kill_handle(counts_as_live, target_id, agent_kill_can_resolve),
    }
}

fn attached_process_readback(
    launcher_process_id: Option<u32>,
    agent_process_id: Option<u32>,
    process_tree_ids: &dyn Fn(u32) -> Vec<u32>,
    live_process_ids: &dyn Fn(&[u32]) -> Vec<u32>,
) -> AttachedAgentProcessReadback {
    let launcher_process_id = non_zero_pid(launcher_process_id);
    let agent_process_id = non_zero_pid(agent_process_id);
    let mut seed_pids = Vec::new();
    if let Some(pid) = launcher_process_id {
        seed_pids.push(pid);
    }
    if let Some(pid) = agent_process_id {
        seed_pids.push(pid);
    }
    seed_pids.sort_unstable();
    seed_pids.dedup();
    let mut tree = Vec::new();
    for pid in &seed_pids {
        tree.extend(process_tree_ids(*pid));
    }
    tree.sort_unstable();
    tree.dedup();
    let live = live_process_ids(&tree);
    AttachedAgentProcessReadback {
        probeable: !seed_pids.is_empty(),
        launcher_process_id,
        agent_process_id,
        parent_process_id: None,
        process_name: None,
        command_line: None,
        cwd: None,
        process_tree_ids: tree,
        live_process_ids: live,
    }
}

#[derive(Clone, Debug)]
struct AmbientAgentProcessCandidate {
    cli: &'static str,
    process_id: u32,
    parent_process_id: Option<u32>,
    process_name: String,
    command_line: String,
    cwd: Option<String>,
    controlling_terminal_window: Option<AttachedAgentWindowReadback>,
}

fn insert_ambient_agent_process_rows(
    rows: &mut BTreeMap<String, AttachedAgentRegistryRow>,
    candidates: Vec<AmbientAgentProcessCandidate>,
) {
    for candidate in candidates {
        let mut process_ids = vec![candidate.process_id];
        if let Some(parent) = candidate.parent_process_id {
            process_ids.push(parent);
        }
        if let Some(window) = candidate.controlling_terminal_window.as_ref() {
            process_ids.push(window.process_id);
        }
        process_ids.sort_unstable();
        process_ids.dedup();
        let process = AttachedAgentProcessReadback {
            probeable: true,
            launcher_process_id: candidate
                .controlling_terminal_window
                .as_ref()
                .map(|window| window.process_id)
                .or(candidate.parent_process_id),
            agent_process_id: Some(candidate.process_id),
            parent_process_id: candidate.parent_process_id,
            process_name: Some(candidate.process_name),
            command_line: Some(candidate.command_line),
            cwd: candidate.cwd,
            process_tree_ids: process_ids.clone(),
            live_process_ids: process_ids,
        };
        let visible_window = candidate.controlling_terminal_window;
        insert_attached_agent_row(
            rows,
            AttachedAgentRegistryRow {
                registry_id: format!(
                    "agent-spawn-ambient-process-{}-{}",
                    candidate.cli, candidate.process_id
                ),
                kind: "ambient".to_owned(),
                source: format!("ambient_process_scan:{}", candidate.cli),
                lifecycle: "live".to_owned(),
                state: Some("working".to_owned()),
                reason_code: Some("ambient_process_observed".to_owned()),
                attention_class: AgentAttentionClass::None,
                counts_as_live: true,
                fleet_stop_managed: false,
                not_counted_reason: None,
                session_id: None,
                spawn_id: None,
                spawn_dir: process.cwd.clone(),
                last_seen_unix_ms: None,
                last_seen_ms_ago: Some(0),
                process,
                visible_window: visible_window.clone(),
                controlling_terminal_window: visible_window,
                kill_handle: AttachedAgentKillHandleReadback {
                    available: false,
                    kind: "process_tree_pending_k2".to_owned(),
                    target_id: Some(format!("pid:{}", candidate.process_id)),
                    reason: "ambient live process has no linked Synapse spawn/session; hard process-tree kill lands with #1036".to_owned(),
                },
            },
        );
    }
}

fn ambient_agent_process_candidates(
    windows_by_pid: &BTreeMap<u32, AttachedAgentWindowReadback>,
    represented_process_ids: &BTreeSet<u32>,
) -> Vec<AmbientAgentProcessCandidate> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let process_id = pid.as_u32();
            if represented_process_ids.contains(&process_id) {
                return None;
            }
            let process_name = process.name().to_string_lossy().into_owned();
            let command_line = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let cli = ambient_agent_cli(&process_name, &command_line)?;
            let parent_process_id = process.parent().map(|parent| parent.as_u32());
            if parent_process_id
                .and_then(|parent| system.process(sysinfo::Pid::from_u32(parent)))
                .is_some_and(|parent| {
                    let parent_name = parent.name().to_string_lossy();
                    let parent_command_line = parent
                        .cmd()
                        .iter()
                        .map(|part| part.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" ");
                    ambient_agent_child_is_covered_by_parent(
                        cli,
                        &process_name,
                        parent_name.as_ref(),
                        &parent_command_line,
                    )
                })
            {
                return None;
            }
            let controlling_terminal_window =
                ambient_controlling_window(&system, process_id, windows_by_pid);
            Some(AmbientAgentProcessCandidate {
                cli,
                process_id,
                parent_process_id,
                process_name,
                command_line,
                cwd: process.cwd().map(|path| path.display().to_string()),
                controlling_terminal_window,
            })
        })
        .collect()
}

fn ambient_agent_cli(process_name: &str, command_line: &str) -> Option<&'static str> {
    let name = ambient_process_name(process_name);
    if name == "claude" {
        return Some("claude");
    }
    if name == "codex" || name == "codex-cli" {
        return Some("codex");
    }
    if name != "node" {
        return None;
    }
    let cmd = ambient_command_line(command_line);
    if ambient_command_line_is_claude_entrypoint(&cmd) {
        return Some("claude");
    }
    if ambient_command_line_is_codex_entrypoint(&cmd) {
        return Some("codex");
    }
    None
}

fn ambient_agent_child_is_covered_by_parent(
    cli: &str,
    process_name: &str,
    parent_process_name: &str,
    parent_command_line: &str,
) -> bool {
    if cli != "codex" || ambient_process_name(process_name) != "codex" {
        return false;
    }
    ambient_agent_cli(parent_process_name, parent_command_line) == Some("codex")
}

fn ambient_process_name(process_name: &str) -> String {
    process_name
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .trim_matches('"')
        .to_ascii_lowercase()
}

fn ambient_command_line(command_line: &str) -> String {
    let mut normalized = command_line.replace('\\', "/").to_ascii_lowercase();
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized
}

fn ambient_command_line_is_claude_entrypoint(cmd: &str) -> bool {
    cmd.contains("@anthropic-ai/claude-code/bin/claude")
        || cmd.contains("@anthropic-ai/claude-code/cli.js")
}

fn ambient_command_line_is_codex_entrypoint(cmd: &str) -> bool {
    cmd.contains("@openai/codex/bin/codex.js") || cmd.contains("openai-codex/bin/codex.js")
}

fn ambient_controlling_window(
    system: &sysinfo::System,
    process_id: u32,
    windows_by_pid: &BTreeMap<u32, AttachedAgentWindowReadback>,
) -> Option<AttachedAgentWindowReadback> {
    let mut current = Some(process_id);
    let mut visited = BTreeSet::new();
    while let Some(pid) = current {
        if !visited.insert(pid) {
            break;
        }
        if let Some(window) = windows_by_pid.get(&pid) {
            return Some(window.clone());
        }
        current = system
            .process(sysinfo::Pid::from_u32(pid))
            .and_then(|process| process.parent())
            .map(|parent| parent.as_u32());
    }
    None
}

fn attached_visible_window(
    process: &AttachedAgentProcessReadback,
    windows_by_pid: &BTreeMap<u32, AttachedAgentWindowReadback>,
) -> Option<AttachedAgentWindowReadback> {
    for pid in &process.live_process_ids {
        if let Some(window) = windows_by_pid.get(pid) {
            return Some(window.clone());
        }
    }
    None
}

fn attached_agent_window_index() -> (BTreeMap<u32, AttachedAgentWindowReadback>, Option<String>) {
    match synapse_a11y::visible_top_level_window_contexts() {
        Ok(contexts) => (
            contexts
                .into_iter()
                .map(|context| {
                    (
                        context.pid,
                        AttachedAgentWindowReadback {
                            window_hwnd: context.hwnd,
                            process_id: context.pid,
                            process_name: context.process_name,
                            window_title: context.window_title,
                        },
                    )
                })
                .collect(),
            None,
        ),
        Err(error) => (BTreeMap::new(), Some(error.to_string())),
    }
}

fn attached_count_decision(process: &AttachedAgentProcessReadback) -> (bool, Option<String>) {
    if !process.probeable {
        return (false, Some("no_process_handle".to_owned()));
    }
    if process.live_process_ids.is_empty() {
        return (false, Some("os_process_not_live".to_owned()));
    }
    (true, None)
}

fn attached_attention_class(
    agent_state: Option<&AgentStateRead>,
    lifecycle: &str,
    counts_as_live: bool,
) -> AgentAttentionClass {
    if counts_as_live && !matches!(lifecycle, "live" | "unbound") {
        return AgentAttentionClass::CleanupRequired;
    }
    let Some(agent_state) = agent_state else {
        return AgentAttentionClass::None;
    };
    match agent_state.attention_class {
        AgentAttentionClass::TerminalSetupFailure | AgentAttentionClass::TerminalRuntimeFailure
            if counts_as_live =>
        {
            AgentAttentionClass::CleanupRequired
        }
        AgentAttentionClass::ActionableLiveStuck if counts_as_live => {
            AgentAttentionClass::ActionableLiveStuck
        }
        AgentAttentionClass::TerminalSetupFailure | AgentAttentionClass::TerminalRuntimeFailure => {
            agent_state.attention_class
        }
        AgentAttentionClass::CleanupRequired => AgentAttentionClass::CleanupRequired,
        AgentAttentionClass::ActionableLiveStuck | AgentAttentionClass::None => {
            AgentAttentionClass::None
        }
    }
}

fn attached_kill_handle(
    counts_as_live: bool,
    target_id: Option<String>,
    agent_kill_can_resolve: bool,
) -> AttachedAgentKillHandleReadback {
    if !counts_as_live {
        return AttachedAgentKillHandleReadback {
            available: false,
            kind: "unavailable".to_owned(),
            target_id,
            reason: "no live OS process to kill".to_owned(),
        };
    }
    if agent_kill_can_resolve {
        return AttachedAgentKillHandleReadback {
            available: true,
            kind: "agent_kill".to_owned(),
            target_id,
            reason: "agent_kill can resolve this session/spawn id and owns the process tree"
                .to_owned(),
        };
    }
    AttachedAgentKillHandleReadback {
        available: false,
        kind: "process_tree_pending_k2".to_owned(),
        target_id,
        reason: "live process tree is known, but no MCP session is linked for agent_kill yet"
            .to_owned(),
    }
}

fn insert_attached_agent_row(
    rows: &mut BTreeMap<String, AttachedAgentRegistryRow>,
    row: AttachedAgentRegistryRow,
) {
    match rows.get(&row.registry_id) {
        Some(existing)
            if existing.counts_as_live
                || (!row.counts_as_live && existing.kill_handle.available) => {}
        _ => {
            rows.insert(row.registry_id.clone(), row);
        }
    }
}

fn agent_state_has_process_handle(row: &AgentStateRead) -> bool {
    non_zero_pid(row.launcher_process_id).is_some() || non_zero_pid(row.agent_process_id).is_some()
}

fn agent_state_is_ambient(row: &AgentStateRead) -> bool {
    row.spawn_id
        .as_deref()
        .unwrap_or(row.anchor.as_str())
        .starts_with("agent-spawn-ambient-")
}

fn non_zero_pid(pid: Option<u32>) -> Option<u32> {
    pid.filter(|pid| *pid != 0)
}

fn build_session_summary(
    session_id: &str,
    registry: Option<SessionRegistryRead>,
    active_target: Option<SessionTarget>,
    target_claims: Vec<TargetClaimRead>,
    all_target_claims: &[TargetClaimRead],
    lease_status: &synapse_action::LeaseStatus,
    now_unix_ms: u64,
    stale_after_ms: u64,
    has_persisted_cdp_owner: bool,
) -> Option<SessionSummary> {
    let active_target_wire = active_target.as_ref().map(session_target_wire);
    let registry = registry.or_else(|| {
        (active_target_wire.is_some()
            || !target_claims.is_empty()
            || lease_status.owner_session_id.as_deref() == Some(session_id)
            || has_persisted_cdp_owner)
            .then(|| synthetic_registry_read(session_id, now_unix_ms, stale_after_ms))
    })?;
    let raw_agent_state = super::agent_state::read_for_session(session_id, now_unix_ms);
    let lease = SessionLeaseReadback {
        held: lease_status.held,
        owner_session_id: lease_status.owner_session_id.clone(),
        is_owner: lease_status.owner_session_id.as_deref() == Some(session_id),
        acquired_at_ms_ago: lease_status.acquired_at_ms_ago,
        renewed_at_ms_ago: lease_status.renewed_at_ms_ago,
        ttl_ms: lease_status.ttl_ms,
        expires_in_ms: lease_status.expires_in_ms,
    };
    let attention_class = session_attention_class(
        session_id,
        &registry,
        active_target.as_ref(),
        &target_claims,
        all_target_claims,
        &lease,
        raw_agent_state.as_ref(),
        has_persisted_cdp_owner,
    );
    let agent_state = session_agent_state_readback(&registry, raw_agent_state);
    Some(SessionSummary {
        registry,
        active_target: active_target_wire,
        agent_logical_foreground: build_agent_logical_foreground(
            session_id,
            active_target.as_ref(),
        ),
        foreground_lane: build_foreground_lane(
            session_id,
            active_target.as_ref(),
            all_target_claims,
            lease_status,
        ),
        target_claims,
        persisted_cdp_target_owners: Vec::new(),
        lease,
        agent_state,
        attention_class,
    })
}

fn session_attention_class(
    session_id: &str,
    registry: &SessionRegistryRead,
    active_target: Option<&SessionTarget>,
    target_claims: &[TargetClaimRead],
    all_target_claims: &[TargetClaimRead],
    lease: &SessionLeaseReadback,
    agent_state: Option<&AgentStateRead>,
    has_persisted_cdp_owner: bool,
) -> AgentAttentionClass {
    let owns_cleanup_resource = active_target.is_some()
        || !target_claims.is_empty()
        || lease.is_owner
        || has_persisted_cdp_owner;
    if registry.lifecycle != "live" && owns_cleanup_resource {
        return AgentAttentionClass::CleanupRequired;
    }
    if dead_live_cleanup_candidate_parts(
        session_id,
        registry,
        active_target,
        target_claims,
        all_target_claims,
        lease,
        agent_state,
        has_persisted_cdp_owner,
    ) {
        return AgentAttentionClass::CleanupRequired;
    }
    let owns_orphan_cleanup_resource = active_target.is_some() || has_persisted_cdp_owner;
    if registry.lifecycle == "live"
        && registry.agent_kind == "local-model"
        && registry.spawned_agent.is_none()
        && registry.last_seen_ms_ago >= DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS
        && owns_orphan_cleanup_resource
        && target_claims.is_empty()
        && !lease.is_owner
        && !active_target_claimed_by_other(session_id, active_target, all_target_claims)
    {
        return AgentAttentionClass::CleanupRequired;
    }
    if recent_live_session_supersedes_terminal_history(registry, agent_state) {
        return AgentAttentionClass::None;
    }
    agent_state
        .map(|read| read.attention_class)
        .unwrap_or_default()
}

fn recent_live_session_supersedes_terminal_history(
    registry: &SessionRegistryRead,
    agent_state: Option<&AgentStateRead>,
) -> bool {
    registry.lifecycle == "live"
        && registry.last_action.is_some()
        && registry.last_seen_ms_ago < DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS
        && agent_state.is_some_and(|read| read.attention_class.is_terminal_history())
}

fn session_agent_state_readback(
    registry: &SessionRegistryRead,
    agent_state: Option<AgentStateRead>,
) -> Option<AgentStateRead> {
    if recent_live_session_supersedes_terminal_history(registry, agent_state.as_ref()) {
        None
    } else {
        agent_state
    }
}

fn terminal_dead_agent_state(agent_state: Option<&AgentStateRead>) -> bool {
    let Some(agent_state) = agent_state else {
        return false;
    };
    matches!(agent_state.state, AgentLifecycleState::Dead)
        && agent_state.attention_class.is_terminal_history()
}

fn cleanup_verified_dead_agent_state(agent_state: Option<&AgentStateRead>) -> bool {
    if !terminal_dead_agent_state(agent_state) {
        return false;
    }
    let Some(reason_code) = agent_state.and_then(|read| read.reason_code.as_deref()) else {
        return false;
    };
    matches!(
        reason_code,
        "process_gone_without_exit_event"
            | "http_stale"
            | "http_session_store_deleted"
            | "spawned_agent_process_exited"
    )
}

fn quiet_orphan_local_model_resource_session(summary: &SessionSummary) -> bool {
    let owns_cleanup_resource =
        summary.active_target.is_some() || !summary.persisted_cdp_target_owners.is_empty();
    summary.registry.lifecycle == "live"
        && summary.registry.agent_kind == "local-model"
        && summary.registry.spawned_agent.is_none()
        && summary.registry.last_seen_ms_ago >= DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS
        && owns_cleanup_resource
        && summary.target_claims.is_empty()
        && !summary.lease.is_owner
        && summary.foreground_lane.status != "conflicting_owner"
}

fn active_target_claimed_by_other(
    session_id: &str,
    active_target: Option<&SessionTarget>,
    all_target_claims: &[TargetClaimRead],
) -> bool {
    let Some(active_target) = active_target else {
        return false;
    };
    let target_key = target_claims::target_key(active_target);
    all_target_claims
        .iter()
        .any(|claim| claim.target_key == target_key && claim.owner_session_id != session_id)
}

fn dead_live_cleanup_candidate_parts(
    session_id: &str,
    registry: &SessionRegistryRead,
    active_target: Option<&SessionTarget>,
    target_claims: &[TargetClaimRead],
    all_target_claims: &[TargetClaimRead],
    lease: &SessionLeaseReadback,
    agent_state: Option<&AgentStateRead>,
    has_persisted_cdp_owner: bool,
) -> bool {
    let owns_cleanup_resource = active_target.is_some() || has_persisted_cdp_owner;
    registry.lifecycle == "live"
        && registry.last_seen_ms_ago >= DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS
        && owns_cleanup_resource
        && cleanup_verified_dead_agent_state(agent_state)
        && target_claims.is_empty()
        && !lease.is_owner
        && !active_target_claimed_by_other(session_id, active_target, all_target_claims)
}

fn dead_live_cleanup_candidate_summary(summary: &SessionSummary) -> bool {
    let owns_cleanup_resource =
        summary.active_target.is_some() || !summary.persisted_cdp_target_owners.is_empty();
    summary.registry.lifecycle == "live"
        && summary.registry.last_seen_ms_ago >= DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS
        && owns_cleanup_resource
        && cleanup_verified_dead_agent_state(summary.agent_state.as_ref())
        && summary.target_claims.is_empty()
        && !summary.lease.is_owner
        && summary.foreground_lane.status != "conflicting_owner"
}

fn ensure_cross_session_cleanup_allowed(
    current_session_id: &str,
    requested_session_id: &str,
    status: &SessionStatusResponse,
) -> Result<(), ErrorData> {
    let Some(summary) = status.session.as_ref() else {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_end cross-session cleanup target was not found",
            Some(json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "current_session_id": current_session_id,
                "requested_session_id": requested_session_id,
                "required_attention_class": "cleanup_required",
            })),
        ));
    };
    if summary.registry.lifecycle != "live"
        && summary.attention_class == AgentAttentionClass::CleanupRequired
    {
        return Ok(());
    }
    if dead_live_cleanup_candidate_summary(summary) {
        let owner_in_flight =
            match crate::daemon_lifecycle::in_flight_tool_calls_for_session(requested_session_id) {
                Ok(calls) => calls,
                #[cfg(test)]
                Err(error) if error.to_string().contains("ledger is not configured") => Vec::new(),
                Err(error) => {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("read daemon lifecycle in-flight tool calls: {error:#}"),
                    ));
                }
            };
        if owner_in_flight.is_empty() {
            return Ok(());
        }
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_end cross-session cleanup refused because the target session has in-flight tool calls",
            Some(json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "current_session_id": current_session_id,
                "requested_session_id": requested_session_id,
                "target_lifecycle": summary.registry.lifecycle,
                "target_attention_class": summary.attention_class,
                "owner_in_flight": owner_in_flight,
                "required": "dead quiet live cleanup target with empty daemon in-flight ledger",
            })),
        ));
    }
    if quiet_orphan_local_model_resource_session(summary) {
        let owner_in_flight =
            match crate::daemon_lifecycle::in_flight_tool_calls_for_session(requested_session_id) {
                Ok(calls) => calls,
                #[cfg(test)]
                Err(error) if error.to_string().contains("ledger is not configured") => Vec::new(),
                Err(error) => {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("read daemon lifecycle in-flight tool calls: {error:#}"),
                    ));
                }
            };
        if owner_in_flight.is_empty() {
            return Ok(());
        }
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_end cross-session cleanup refused because the orphan local-model session has in-flight tool calls",
            Some(json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "current_session_id": current_session_id,
                "requested_session_id": requested_session_id,
                "target_lifecycle": summary.registry.lifecycle,
                "target_attention_class": summary.attention_class,
                "owner_in_flight": owner_in_flight,
                "required": "quiet orphan local-model resource owner with empty daemon in-flight ledger",
            })),
        ));
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        "session_end cross-session cleanup is allowed only for non-live cleanup_required sessions, verified dead quiet live resource owners, or quiet orphan local-model resource owners",
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "current_session_id": current_session_id,
            "requested_session_id": requested_session_id,
            "target_lifecycle": summary.registry.lifecycle,
            "target_attention_class": summary.attention_class,
            "required": "non-live cleanup_required, live dead quiet no-claim no-lease no-in-flight resource owner, or quiet orphan local-model no-claim no-lease no-in-flight resource owner",
        })),
    ))
}

fn target_claim_reads_by_owner(
    claims: &[TargetClaimRead],
) -> BTreeMap<String, Vec<TargetClaimRead>> {
    let mut by_owner = BTreeMap::new();
    for claim in claims {
        by_owner
            .entry(claim.owner_session_id.clone())
            .or_insert_with(Vec::new)
            .push(claim.clone());
    }
    by_owner
}

fn build_agent_logical_foreground(
    session_id: &str,
    active_target: Option<&SessionTarget>,
) -> AgentLogicalForegroundReadback {
    let persisted_row_key = session_target_row_key(session_id);
    match active_target {
        Some(target) => AgentLogicalForegroundReadback {
            source_of_truth: format!(
                "CF_SESSIONS row {persisted_row_key} + daemon session target registry; never human OS foreground fallback"
            ),
            session_id: session_id.to_owned(),
            status: "set".to_owned(),
            target: Some(session_target_wire(target)),
            persisted_row_key: Some(persisted_row_key),
            no_human_os_foreground_fallback: true,
            missing_reason: None,
        },
        None => AgentLogicalForegroundReadback {
            source_of_truth: format!(
                "CF_SESSIONS row {persisted_row_key} + daemon session target registry; never human OS foreground fallback"
            ),
            session_id: session_id.to_owned(),
            status: "missing".to_owned(),
            target: None,
            persisted_row_key: Some(persisted_row_key),
            no_human_os_foreground_fallback: true,
            missing_reason: Some("no session-owned logical foreground target is set".to_owned()),
        },
    }
}

fn build_foreground_lane(
    session_id: &str,
    active_target: Option<&SessionTarget>,
    all_target_claims: &[TargetClaimRead],
    lease_status: &synapse_action::LeaseStatus,
) -> ForegroundLaneReadback {
    if let Some(target) = active_target {
        let target_key = target_claims::target_key(target);
        let target_claim = all_target_claims
            .iter()
            .find(|claim| claim.target_key == target_key)
            .cloned();
        let owner_session_id = target_claim
            .as_ref()
            .map(|claim| claim.owner_session_id.clone())
            .unwrap_or_else(|| session_id.to_owned());
        let status = match target_claim.as_ref() {
            Some(claim) if claim.owner_session_id != session_id => "conflicting_owner",
            Some(_) => "claimed_by_session",
            None => "unclaimed_session_target",
        };
        return ForegroundLaneReadback {
            source_of_truth: "daemon session target registry + CF_SESSIONS session-target row + daemon target-claim registry + synapse_action input lease".to_owned(),
            session_id: session_id.to_owned(),
            status: status.to_owned(),
            capacity_model: "target_owned_lane_not_daemon_pool_limited".to_owned(),
            capacity_exhausted: false,
            lane_kind: Some(match target {
                SessionTarget::Window { .. } => "owned_window_target".to_owned(),
                SessionTarget::Cdp { .. } => "owned_chrome_tab_target".to_owned(),
            }),
            target_key: Some(target_key),
            target: Some(session_target_wire(target)),
            target_claim,
            owner_session_id: Some(owner_session_id),
            explicit_real_foreground_lease: false,
            no_human_os_foreground_fallback: true,
            missing_reason: None,
        };
    }

    if lease_status.owner_session_id.as_deref() == Some(session_id) {
        return ForegroundLaneReadback {
            source_of_truth:
                "synapse_action input lease; explicit real OS foreground lease only, no implicit fallback"
                    .to_owned(),
            session_id: session_id.to_owned(),
            status: "explicit_real_foreground_lease".to_owned(),
            capacity_model: "serialized_real_os_foreground_singleton".to_owned(),
            capacity_exhausted: false,
            lane_kind: Some("real_os_foreground_lease".to_owned()),
            target_key: None,
            target: None,
            target_claim: None,
            owner_session_id: Some(session_id.to_owned()),
            explicit_real_foreground_lease: true,
            no_human_os_foreground_fallback: true,
            missing_reason: None,
        };
    }

    ForegroundLaneReadback {
        source_of_truth:
            "CF_SESSIONS session-target row + daemon session target registry + synapse_action input lease"
                .to_owned(),
        session_id: session_id.to_owned(),
        status: "missing".to_owned(),
        capacity_model: "no_lane_acquired".to_owned(),
        capacity_exhausted: false,
        lane_kind: None,
        target_key: None,
        target: None,
        target_claim: None,
        owner_session_id: None,
        explicit_real_foreground_lease: false,
        no_human_os_foreground_fallback: true,
        missing_reason: Some(
            "no agent logical foreground target and no explicit real foreground lease".to_owned(),
        ),
    }
}

fn build_foreground_lane_capacity(
    sessions: &[SessionSummary],
    all_target_claims: &[TargetClaimRead],
    lease_status: &synapse_action::LeaseStatus,
) -> ForegroundLaneCapacityReadback {
    let active_agent_logical_foreground_count = sessions
        .iter()
        .filter(|session| session.agent_logical_foreground.status == "set")
        .count();
    let active_foreground_lane_count = sessions
        .iter()
        .filter(|session| session.foreground_lane.status != "missing")
        .count();
    let explicit_real_foreground_lease_count = usize::from(lease_status.held);

    ForegroundLaneCapacityReadback {
        source_of_truth: "session_list read model over CF_SESSIONS session-target rows, daemon session target registry, daemon target-claim registry, and synapse_action input lease",
        capacity_model: "target_owned_lanes_not_daemon_pool_limited; real_os_foreground_lease_is_singleton_break_glass",
        daemon_lane_pool_limit: None,
        active_agent_logical_foreground_count,
        active_foreground_lane_count,
        claimed_target_lane_count: all_target_claims.len(),
        explicit_real_foreground_lease_count,
        remaining_daemon_lane_slots: None,
        capacity_exhausted: false,
        exhausted_reason: None,
        target_backed_lane_kinds: vec!["owned_window_target", "owned_chrome_tab_target"],
    }
}

fn session_target_row_key(session_id: &str) -> String {
    format!("{SESSION_TARGET_ROW_PREFIX}{session_id}")
}

fn session_id_from_target_row_key(row_key: &[u8]) -> Result<Option<String>, ErrorData> {
    let key = std::str::from_utf8(row_key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("CF_SESSIONS session-target row key is not UTF-8: {error}"),
        )
    })?;
    let Some(session_id) = key.strip_prefix(SESSION_TARGET_ROW_PREFIX) else {
        return Ok(None);
    };
    if session_id.is_empty()
        || session_id.chars().count() > 512
        || !session_id.chars().all(|ch| ('!'..='~').contains(&ch))
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("CF_SESSIONS session-target row key has invalid session id: {key}"),
        ));
    }
    Ok(Some(session_id.to_owned()))
}

fn synthetic_registry_read(
    session_id: &str,
    now_unix_ms: u64,
    stale_after_ms: u64,
) -> SessionRegistryRead {
    SessionRegistryRead {
        session_id: session_id.to_owned(),
        transport: "unknown".to_owned(),
        client_name: None,
        client_version: None,
        protocol_version: None,
        agent_kind: "unknown".to_owned(),
        lifecycle: "unregistered".to_owned(),
        started_at_unix_ms: now_unix_ms,
        last_seen_unix_ms: now_unix_ms,
        last_seen_ms_ago: 0,
        stale_after_ms,
        closed_at_unix_ms: None,
        last_action: None,
        last_reason_code: None,
        spawned_agent: None,
    }
}

fn session_target_wire(target: &SessionTarget) -> TargetWire {
    match target {
        SessionTarget::Window { hwnd } => TargetWire::Window { window_hwnd: *hwnd },
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => TargetWire::Cdp {
            window_hwnd: *window_hwnd,
            cdp_target_id: cdp_target_id.clone(),
        },
    }
}

pub(crate) fn validate_session_id(session_id: &str) -> Result<(), ErrorData> {
    if session_id.trim().is_empty() {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_id must not be empty",
            Some(json!({"code": error_codes::TOOL_PARAMS_INVALID})),
        ));
    }
    if session_id.chars().count() > 512 {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_id must be at most 512 Unicode scalar values",
            Some(json!({"code": error_codes::TOOL_PARAMS_INVALID})),
        ));
    }
    if !session_id.chars().all(|ch| ('!'..='~').contains(&ch)) {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "session_id must contain only visible ASCII characters",
            Some(json!({"code": error_codes::TOOL_PARAMS_INVALID})),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
    use rmcp::transport::streamable_http_server::session::SessionState;
    use std::num::NonZeroUsize;
    use synapse_core::AgentEventKind;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

    #[test]
    fn session_status_rejects_empty_or_non_visible_ascii_ids() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("abc def").is_err());
        assert!(validate_session_id("abc\n").is_err());
        assert!(validate_session_id("session-1").is_ok());
    }

    #[test]
    fn synthetic_entries_cover_target_or_lease_only_sessions() {
        let session_id = "lease-only";
        let lease_status = synapse_action::LeaseStatus {
            held: true,
            owner_session_id: Some(session_id.to_owned()),
            acquired_at_ms_ago: Some(1),
            renewed_at_ms_ago: Some(1),
            ttl_ms: Some(30_000),
            expires_in_ms: Some(29_999),
        };
        let summary = build_session_summary(
            session_id,
            None,
            None,
            Vec::new(),
            &[],
            &lease_status,
            1_000,
            500,
            false,
        )
        .unwrap();
        assert_eq!(summary.registry.lifecycle, "unregistered");
        assert!(summary.lease.is_owner);
        assert_eq!(
            summary.agent_logical_foreground.status, "missing",
            "a real foreground lease must not be reported as an agent target"
        );
        assert_eq!(
            summary.foreground_lane.status,
            "explicit_real_foreground_lease"
        );
        assert_eq!(
            summary.foreground_lane.capacity_model,
            "serialized_real_os_foreground_singleton"
        );
        assert!(!summary.foreground_lane.capacity_exhausted);
        assert!(summary.foreground_lane.explicit_real_foreground_lease);
        assert!(summary.foreground_lane.no_human_os_foreground_fallback);
    }

    #[test]
    fn persisted_cdp_owner_only_session_is_visible_cleanup_required() {
        let session_id = "cdp-owner-only";
        let lease_status = synapse_action::LeaseStatus::unheld();
        let summary = build_session_summary(
            session_id,
            None,
            None,
            Vec::new(),
            &[],
            &lease_status,
            1_000,
            500,
            true,
        )
        .unwrap();

        assert_eq!(summary.registry.lifecycle, "unregistered");
        assert_eq!(
            summary.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        assert_eq!(summary.agent_logical_foreground.status, "missing");
        assert_eq!(summary.foreground_lane.status, "missing");
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                session_id,
                &status_response(Some(summary))
            )
            .is_ok()
        );
    }

    #[test]
    fn persisted_cdp_owner_readback_exposes_cleanup_source_of_truth() {
        let readback = persisted_cdp_target_owner_readback(
            "owner-key-1".to_owned(),
            crate::server::session_continuity::PersistedCdpTargetOwner {
                schema_version: 1,
                owner_key: "owner-key-1".to_owned(),
                stored_at_unix_ms: 1_100,
                owner_session_id: "session-cdp".to_owned(),
                owner_client_name: Some("codex-mcp-client".to_owned()),
                owner_agent_kind: "codex".to_owned(),
                owner_started_at_unix_ms: Some(900),
                owner: crate::server::CdpTargetOwner {
                    session_id: "session-cdp".to_owned(),
                    window_hwnd: -1,
                    endpoint: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs"
                        .to_owned(),
                    chrome_window_id: Some(7),
                    capture_window_hwnd: Some(0x1234),
                    cdp_target_id: "chrome-tab:600751746".to_owned(),
                    requested_url: "https://example.test/issue1268?body=SYNAPSE_SECRET_1484&token=SYNAPSE_TOKEN_1484#frag=SYNAPSE_HASH_1484".to_owned(),
                    target_url: "https://example.test/issue1268?body=SYNAPSE_SECRET_1484&token=SYNAPSE_TOKEN_1484#frag=SYNAPSE_HASH_1484".to_owned(),
                    created_at_unix_ms: 1_000,
                },
            },
        );

        assert_eq!(
            readback.source_of_truth,
            "CF_SESSIONS:mcp/session-cdp-target-owner/v1/20:chrome-tab:600751746/owner-key-1"
        );
        match readback.target {
            TargetWire::Cdp {
                window_hwnd,
                cdp_target_id,
            } => {
                assert_eq!(window_hwnd, -1);
                assert_eq!(cdp_target_id, "chrome-tab:600751746");
            }
            TargetWire::Window { .. } => panic!("expected CDP target readback"),
        }
        assert_eq!(readback.owner_session_id, "session-cdp");
        assert_eq!(readback.chrome_window_id, Some(7));
        assert_eq!(
            readback.requested_url,
            "https://example.test/redacted?redacted#redacted"
        );
        assert_eq!(
            readback.target_url,
            "https://example.test/redacted?redacted#redacted"
        );
        assert!(!readback.requested_url.contains("SYNAPSE_SECRET_1484"));
        assert!(!readback.target_url.contains("SYNAPSE_TOKEN_1484"));
        assert!(!readback.target_url.contains("SYNAPSE_HASH_1484"));
        assert_eq!(readback.target_live.status, "window_absent_or_unreadable");
        assert!(readback.target_live.stale_orphan);
        assert_eq!(
            readback.target_live.read_error_code.as_deref(),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert!(
            readback
                .cleanup_action
                .contains("stale_orphan: session list is read-only")
        );
        assert!(
            readback
                .recovery_guidance
                .contains("do not silently drop it from read-only session list")
        );
    }

    #[test]
    fn session_summary_exposes_agent_logical_foreground_lane() {
        let session_id = "session-a";
        let target = SessionTarget::Window { hwnd: 0x1234 };
        let claim = TargetClaimRead {
            target_key: "window:0x1234".to_owned(),
            target: TargetWire::Window {
                window_hwnd: 0x1234,
            },
            owner_session_id: session_id.to_owned(),
            claimed_at_unix_ms: 1_000,
            renewed_at_unix_ms: 1_000,
            ttl_ms: 120_000,
            expires_at_unix_ms: 121_000,
            expires_in_ms: 120_000,
            generation: 1,
            source_of_truth: "test target claim registry".to_owned(),
        };
        let lease_status = synapse_action::LeaseStatus::unheld();
        let summary = build_session_summary(
            session_id,
            None,
            Some(target),
            vec![claim.clone()],
            &[claim],
            &lease_status,
            1_000,
            500,
            false,
        )
        .unwrap();

        assert_eq!(summary.agent_logical_foreground.status, "set");
        assert_eq!(
            summary
                .agent_logical_foreground
                .persisted_row_key
                .as_deref(),
            Some("mcp/session-target/v1/session-a")
        );
        assert!(
            summary
                .agent_logical_foreground
                .no_human_os_foreground_fallback
        );
        assert_eq!(summary.foreground_lane.status, "claimed_by_session");
        assert_eq!(
            summary.foreground_lane.capacity_model,
            "target_owned_lane_not_daemon_pool_limited"
        );
        assert!(!summary.foreground_lane.capacity_exhausted);
        assert_eq!(
            summary.foreground_lane.lane_kind.as_deref(),
            Some("owned_window_target")
        );
        assert_eq!(
            summary.foreground_lane.target_key.as_deref(),
            Some("window:0x1234")
        );
        assert_eq!(
            summary.foreground_lane.owner_session_id.as_deref(),
            Some(session_id)
        );
    }

    #[test]
    fn session_list_includes_persisted_target_only_session() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let session_id = "persisted-target-only";
        let db_path = profiles.path().join("db");
        {
            let db = synapse_storage::Db::open(&db_path, synapse_core::SCHEMA_VERSION)?;
            let row = json!({
                "schema_version": 1,
                "session_id": session_id,
                "stored_at_unix_ms": 1_000,
                "target": {
                    "kind": "window",
                    "hwnd": 0x1234
                }
            });
            db.put_batch_pressure_bypass(
                cf::CF_SESSIONS,
                [(
                    session_target_row_key(session_id).into_bytes(),
                    synapse_storage::encode_json(&row)?,
                )],
            )?;
        }
        let service = test_service_with_profiles(profiles.path())?;

        let list = service.session_list_impl(true)?;

        assert_eq!(list.target_session_count, 1);
        assert_eq!(
            list.foreground_lane_capacity
                .active_agent_logical_foreground_count,
            1
        );
        let summary = list
            .sessions
            .iter()
            .find(|summary| summary.registry.session_id == session_id)
            .expect("persisted target-only session should be listed");
        assert_eq!(summary.registry.lifecycle, "unregistered");
        assert_eq!(
            summary.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        assert_eq!(summary.agent_logical_foreground.status, "set");
        assert_eq!(
            summary
                .agent_logical_foreground
                .persisted_row_key
                .as_deref(),
            Some("mcp/session-target/v1/persisted-target-only")
        );
        assert_eq!(summary.foreground_lane.status, "unclaimed_session_target");
        assert_eq!(
            summary.foreground_lane.target_key.as_deref(),
            Some("window:0x1234")
        );
        Ok(())
    }

    #[test]
    fn session_list_tool_defaults_to_compact_paginated_projection() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = test_service_with_profiles(profiles.path())?;
        register_initialized_session(&service, "session-a", "codex-mcp-client", 1_000)?;
        register_initialized_session(&service, "session-b", "codex-mcp-client", 1_100)?;
        register_initialized_session(&service, "session-c", "codex-mcp-client", 1_200)?;

        let first_page = service.session_list_impl_with_options(
            SessionListOptions::from_tool_params(SessionListParams {
                limit: Some(2),
                ..SessionListParams::default()
            })?,
        )?;

        assert_eq!(first_page.view, SessionListView::Compact);
        assert_eq!(first_page.total_count, 3);
        assert_eq!(first_page.returned_count, 2);
        assert!(first_page.has_more);
        assert_eq!(first_page.next_cursor, Some(2));
        assert!(first_page.sessions.is_empty());
        assert_eq!(
            first_page
                .compact_sessions
                .iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["session-a", "session-b"]
        );
        assert!(first_page.attached_agent_registry.rows.is_empty());
        assert!(
            first_page
                .omitted_sections
                .contains(&"attached_agent_registry.rows")
        );

        let second_page = service.session_list_impl_with_options(
            SessionListOptions::from_tool_params(SessionListParams {
                cursor: 2,
                limit: Some(2),
                ..SessionListParams::default()
            })?,
        )?;

        assert_eq!(second_page.cursor, 2);
        assert_eq!(second_page.total_count, 3);
        assert_eq!(second_page.returned_count, 1);
        assert!(!second_page.has_more);
        assert_eq!(second_page.next_cursor, None);
        assert_eq!(second_page.compact_sessions[0].session_id, "session-c");
        Ok(())
    }

    /// Deterministic regression guard for issue #1574: a foreign holder of the
    /// process-global input lease (exactly what a parallel lease-mutating test
    /// does) must not leak into this service's `session_list` as a phantom
    /// session. Before per-test lease isolation this folded the foreign owner in
    /// and `total_count` intermittently became 4. Here we reproduce the leak
    /// mechanism deterministically — hold the real global lease with a foreign
    /// owner on an unisolated thread — and prove the isolated service ignores it.
    #[test]
    fn session_list_ignores_foreign_global_input_lease_owner() -> anyhow::Result<()> {
        use synapse_action::lease;

        let profiles = TempDir::new()?;
        // The constructor installs this thread's hermetic lease override.
        let service = test_service_with_profiles(profiles.path())?;
        register_initialized_session(&service, "session-a", "codex-mcp-client", 1_000)?;
        register_initialized_session(&service, "session-b", "codex-mcp-client", 1_100)?;
        register_initialized_session(&service, "session-c", "codex-mcp-client", 1_200)?;

        // A thread with no isolation override acquires the *process-global*
        // lease — the precise contention that used to contaminate this test.
        let phantom = "phantom-zzz-foreign-owner";
        let global_status = std::thread::spawn(move || {
            let outcome = lease::try_acquire(phantom, lease::ttl_from_ms(60_000));
            assert!(
                matches!(outcome, lease::LeaseOutcome::Acquired(_)),
                "expected to acquire the process-global lease, got {outcome:?}"
            );
            lease::status()
        })
        .join()
        .map_err(|_error| anyhow::anyhow!("global-lease acquisition thread panicked"))?;

        // Source-of-truth check: the process-global lease really is held by the
        // foreign owner at the moment we read the list below.
        assert!(
            global_status.held,
            "the process-global lease should be held by the phantom owner"
        );
        assert_eq!(
            global_status.owner_session_id.as_deref(),
            Some(phantom),
            "the process-global lease owner should be the phantom session"
        );

        // The isolated service must NOT fold the foreign global owner in.
        let page = service.session_list_impl_with_options(SessionListOptions::from_tool_params(
            SessionListParams {
                limit: Some(10),
                ..SessionListParams::default()
            },
        )?)?;
        assert_eq!(
            page.total_count, 3,
            "foreign global lease owner leaked into session_list (issue #1574 regression)"
        );
        assert!(
            page.compact_sessions
                .iter()
                .all(|row| row.session_id != phantom),
            "the phantom lease owner must not appear as a session row"
        );

        // Good-citizen cleanup: release the process-global lease from an
        // unisolated thread so no residue reaches other tests in this process.
        std::thread::spawn(move || {
            let _released = lease::release_if_owner(phantom);
        })
        .join()
        .map_err(|_error| anyhow::anyhow!("global-lease cleanup thread panicked"))?;

        Ok(())
    }

    #[test]
    fn session_list_full_view_and_live_only_filter_are_explicit() -> anyhow::Result<()> {
        let profiles = TempDir::new()?;
        let service = test_service_with_profiles(profiles.path())?;
        let now = unix_time_ms_now();
        register_initialized_session(&service, "session-live", "codex-mcp-client", now)?;
        register_initialized_session(&service, "session-closed", "codex-mcp-client", now + 1)?;
        {
            let mut registry = service
                .session_registry_ref()
                .lock()
                .map_err(|_error| anyhow::anyhow!("session registry lock poisoned"))?;
            registry.record_closed("session-closed", now + 2);
        }

        let live_only = service.session_list_impl_with_options(
            SessionListOptions::from_tool_params(SessionListParams {
                include_closed: true,
                live_only: true,
                view: SessionListView::Full,
                include_attached_agent_rows: true,
                include_terminal_unbound_history: true,
                ..SessionListParams::default()
            })?,
        )?;

        assert_eq!(live_only.view, SessionListView::Full);
        assert!(live_only.compact_sessions.is_empty());
        assert_eq!(live_only.total_count, 1);
        assert_eq!(live_only.returned_count, 1);
        assert_eq!(live_only.sessions[0].registry.session_id, "session-live");
        assert_eq!(live_only.sessions[0].registry.lifecycle, "live");
        assert!(
            !live_only
                .omitted_sections
                .contains(&"attached_agent_registry.rows")
        );
        Ok(())
    }

    #[test]
    fn session_list_rejects_invalid_page_limits() {
        let zero = SessionListOptions::from_tool_params(SessionListParams {
            limit: Some(0),
            ..SessionListParams::default()
        })
        .expect_err("zero limit should fail closed");
        assert_eq!(error_code(&zero), Some(error_codes::TOOL_PARAMS_INVALID));

        let too_large = SessionListOptions::from_tool_params(SessionListParams {
            limit: Some(SESSION_LIST_MAX_LIMIT + 1),
            ..SessionListParams::default()
        })
        .expect_err("oversized limit should fail closed");
        assert_eq!(
            error_code(&too_large),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn session_summary_reports_conflicting_foreground_lane_owner() {
        let session_id = "session-a";
        let other_session_id = "session-b";
        let target = SessionTarget::Cdp {
            window_hwnd: 0x2222,
            cdp_target_id: "target-1".to_owned(),
        };
        let claim = TargetClaimRead {
            target_key: "cdp:0x2222:target-1".to_owned(),
            target: TargetWire::Cdp {
                window_hwnd: 0x2222,
                cdp_target_id: "target-1".to_owned(),
            },
            owner_session_id: other_session_id.to_owned(),
            claimed_at_unix_ms: 1_000,
            renewed_at_unix_ms: 1_000,
            ttl_ms: 120_000,
            expires_at_unix_ms: 121_000,
            expires_in_ms: 120_000,
            generation: 1,
            source_of_truth: "test target claim registry".to_owned(),
        };
        let lease_status = synapse_action::LeaseStatus::unheld();
        let summary = build_session_summary(
            session_id,
            None,
            Some(target),
            Vec::new(),
            &[claim],
            &lease_status,
            1_000,
            500,
            false,
        )
        .unwrap();

        assert_eq!(summary.foreground_lane.status, "conflicting_owner");
        assert_eq!(
            summary.foreground_lane.capacity_model,
            "target_owned_lane_not_daemon_pool_limited"
        );
        assert!(!summary.foreground_lane.capacity_exhausted);
        assert_eq!(
            summary.foreground_lane.lane_kind.as_deref(),
            Some("owned_chrome_tab_target")
        );
        assert_eq!(
            summary.foreground_lane.owner_session_id.as_deref(),
            Some(other_session_id)
        );
    }

    #[test]
    fn foreground_lane_capacity_reports_target_backed_unbounded_model() {
        let session_id = "session-a";
        let target = SessionTarget::Window { hwnd: 0x1234 };
        let claim = TargetClaimRead {
            target_key: "window:0x1234".to_owned(),
            target: TargetWire::Window {
                window_hwnd: 0x1234,
            },
            owner_session_id: session_id.to_owned(),
            claimed_at_unix_ms: 1_000,
            renewed_at_unix_ms: 1_000,
            ttl_ms: 120_000,
            expires_at_unix_ms: 121_000,
            expires_in_ms: 120_000,
            generation: 1,
            source_of_truth: "test target claim registry".to_owned(),
        };
        let lease_status = synapse_action::LeaseStatus::unheld();
        let summary = build_session_summary(
            session_id,
            None,
            Some(target),
            vec![claim.clone()],
            std::slice::from_ref(&claim),
            &lease_status,
            1_000,
            500,
            false,
        )
        .unwrap();

        let capacity =
            build_foreground_lane_capacity(&[summary], std::slice::from_ref(&claim), &lease_status);

        assert_eq!(
            capacity.capacity_model,
            "target_owned_lanes_not_daemon_pool_limited; real_os_foreground_lease_is_singleton_break_glass"
        );
        assert_eq!(capacity.daemon_lane_pool_limit, None);
        assert_eq!(capacity.remaining_daemon_lane_slots, None);
        assert!(!capacity.capacity_exhausted);
        assert_eq!(capacity.active_agent_logical_foreground_count, 1);
        assert_eq!(capacity.active_foreground_lane_count, 1);
        assert_eq!(capacity.claimed_target_lane_count, 1);
        assert_eq!(capacity.explicit_real_foreground_lease_count, 0);
        assert_eq!(
            capacity.target_backed_lane_kinds,
            vec!["owned_window_target", "owned_chrome_tab_target"]
        );
    }

    #[test]
    fn target_row_key_parser_rejects_corrupt_session_ids() {
        assert_eq!(
            session_id_from_target_row_key(b"mcp/session-target/v1/session-a")
                .unwrap()
                .as_deref(),
            Some("session-a")
        );
        assert!(
            session_id_from_target_row_key(b"other/session-a")
                .unwrap()
                .is_none()
        );
        assert!(session_id_from_target_row_key(b"mcp/session-target/v1/").is_err());
        assert!(session_id_from_target_row_key(b"mcp/session-target/v1/bad session").is_err());
        assert!(session_id_from_target_row_key(&[0xff]).is_err());
    }

    #[test]
    fn cross_session_end_policy_allows_only_cleanup_required_rows() {
        let lease_status = synapse_action::LeaseStatus::unheld();
        let cleanup_summary = build_session_summary(
            "cleanup-target",
            None,
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &lease_status,
            1_000,
            500,
            false,
        )
        .expect("cleanup summary");
        assert_eq!(
            cleanup_summary.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "cleanup-target",
                &status_response(Some(cleanup_summary))
            )
            .is_ok()
        );

        let mut live_registry = synthetic_registry_read("live-peer", 1_000, 500);
        live_registry.lifecycle = "live".to_owned();
        let live_summary = build_session_summary(
            "live-peer",
            Some(live_registry),
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &lease_status,
            1_000,
            500,
            false,
        )
        .expect("live summary");
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "live-peer",
                &status_response(Some(live_summary))
            )
            .is_err()
        );

        let mut closed_registry = synthetic_registry_read("closed-no-resource", 1_000, 500);
        closed_registry.lifecycle = "closed".to_owned();
        let closed_summary = build_session_summary(
            "closed-no-resource",
            Some(closed_registry),
            None,
            Vec::new(),
            &[],
            &lease_status,
            1_000,
            500,
            false,
        )
        .expect("closed summary");
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "closed-no-resource",
                &status_response(Some(closed_summary))
            )
            .is_err()
        );

        assert!(
            ensure_cross_session_cleanup_allowed("caller", "missing", &status_response(None))
                .is_err()
        );
    }

    #[test]
    fn cross_session_end_policy_allows_dead_quiet_live_resource_rows() {
        let lease_status = synapse_action::LeaseStatus::unheld();
        let summary = dead_live_target_summary(
            "dead-live-target",
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1,
            None,
            false,
        );

        assert_eq!(summary.registry.lifecycle, "live");
        assert_eq!(
            summary.agent_state.as_ref().unwrap().state,
            AgentLifecycleState::Dead
        );
        assert_eq!(
            summary.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "dead-live-target",
                &status_response(Some(summary))
            )
            .is_ok()
        );

        let stale_action_summary = dead_live_target_summary(
            "dead-live-target-with-stale-action",
            Some(SessionTarget::Window { hwnd: 0x5678 }),
            Vec::new(),
            &[],
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1,
            Some("tools/call:act"),
            false,
        );

        assert_eq!(stale_action_summary.registry.lifecycle, "live");
        assert_eq!(
            stale_action_summary.agent_state.as_ref().unwrap().state,
            AgentLifecycleState::Dead
        );
        assert_eq!(
            stale_action_summary.registry.last_action.as_deref(),
            Some("tools/call:act")
        );
        assert_eq!(
            stale_action_summary.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "dead-live-target-with-stale-action",
                &status_response(Some(stale_action_summary))
            )
            .is_ok()
        );

        let ambiguous_unprobeable_summary = dead_live_target_summary_with_reason(
            "dead-live-target-unprobeable",
            Some(SessionTarget::Window { hwnd: 0x9abc }),
            Vec::new(),
            &[],
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1,
            Some("tools/call:target"),
            false,
            Some("unprobeable_silent_ended"),
        );

        assert_eq!(
            ambiguous_unprobeable_summary
                .agent_state
                .as_ref()
                .unwrap()
                .reason_code
                .as_deref(),
            Some("unprobeable_silent_ended")
        );
        assert_eq!(
            ambiguous_unprobeable_summary.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "dead-live-target-unprobeable",
                &status_response(Some(ambiguous_unprobeable_summary))
            )
            .is_err()
        );
    }

    #[test]
    fn cross_session_end_policy_allows_quiet_orphan_local_model_resource_rows() {
        let lease_status = synapse_action::LeaseStatus::unheld();
        let now_unix_ms = 1_000_000;
        let mut registry = synthetic_registry_read("orphan-local-model", now_unix_ms, 500);
        registry.lifecycle = "live".to_owned();
        registry.agent_kind = "local-model".to_owned();
        registry.last_seen_ms_ago = DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1;
        registry.last_seen_unix_ms = now_unix_ms.saturating_sub(registry.last_seen_ms_ago);
        registry.spawned_agent = None;
        registry.last_action = None;

        let summary = build_session_summary(
            "orphan-local-model",
            Some(registry),
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &lease_status,
            now_unix_ms,
            500,
            false,
        )
        .expect("quiet orphan summary");

        assert_eq!(summary.registry.lifecycle, "live");
        assert_eq!(summary.registry.agent_kind, "local-model");
        assert!(summary.registry.spawned_agent.is_none());
        assert!(summary.agent_state.is_none());
        assert_eq!(
            summary.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "orphan-local-model",
                &status_response(Some(summary))
            )
            .is_ok()
        );
    }

    #[test]
    fn cross_session_end_policy_refuses_recent_orphan_local_model_resource_rows() {
        let lease_status = synapse_action::LeaseStatus::unheld();
        let now_unix_ms = 1_000_000;
        let mut registry = synthetic_registry_read("recent-orphan-local-model", now_unix_ms, 500);
        registry.lifecycle = "live".to_owned();
        registry.agent_kind = "local-model".to_owned();
        registry.last_seen_ms_ago = DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS - 1;
        registry.last_seen_unix_ms = now_unix_ms.saturating_sub(registry.last_seen_ms_ago);
        registry.spawned_agent = None;
        registry.last_action = None;

        let summary = build_session_summary(
            "recent-orphan-local-model",
            Some(registry),
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &lease_status,
            now_unix_ms,
            500,
            false,
        )
        .expect("recent orphan summary");

        assert_eq!(summary.registry.lifecycle, "live");
        assert_eq!(summary.registry.agent_kind, "local-model");
        assert!(summary.registry.spawned_agent.is_none());
        assert_eq!(summary.attention_class, AgentAttentionClass::None);
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "recent-orphan-local-model",
                &status_response(Some(summary))
            )
            .is_err()
        );
    }

    #[test]
    fn cross_session_end_policy_refuses_guarded_dead_live_rows() {
        let lease_status = synapse_action::LeaseStatus::unheld();
        let recent = dead_live_target_summary(
            "recent-dead-live",
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS - 1,
            None,
            false,
        );
        assert_eq!(
            recent.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "recent-dead-live",
                &status_response(Some(recent))
            )
            .is_err()
        );

        let active = dead_live_target_summary(
            "active-dead-live",
            None,
            Vec::new(),
            &[],
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS - 1,
            Some("tools/call:session_list"),
            false,
        );
        assert_eq!(active.attention_class, AgentAttentionClass::None);

        let own_claim = TargetClaimRead {
            target_key: "window:0x1234".to_owned(),
            target: TargetWire::Window {
                window_hwnd: 0x1234,
            },
            owner_session_id: "claimed-dead-live".to_owned(),
            claimed_at_unix_ms: 1_000,
            renewed_at_unix_ms: 1_000,
            ttl_ms: 120_000,
            expires_at_unix_ms: 121_000,
            expires_in_ms: 120_000,
            generation: 1,
            source_of_truth: "test target claim registry".to_owned(),
        };
        let claimed = dead_live_target_summary(
            "claimed-dead-live",
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            vec![own_claim.clone()],
            std::slice::from_ref(&own_claim),
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1,
            None,
            false,
        );
        assert_eq!(
            claimed.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "claimed-dead-live",
                &status_response(Some(claimed))
            )
            .is_err()
        );

        let conflicting_claim = TargetClaimRead {
            owner_session_id: "other-session".to_owned(),
            ..own_claim.clone()
        };
        let conflicting = dead_live_target_summary(
            "conflicting-dead-live",
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            std::slice::from_ref(&conflicting_claim),
            &lease_status,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1,
            None,
            false,
        );
        assert_eq!(
            conflicting.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
        assert_eq!(conflicting.foreground_lane.status, "conflicting_owner");
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "conflicting-dead-live",
                &status_response(Some(conflicting))
            )
            .is_err()
        );

        let held_lease = synapse_action::LeaseStatus {
            held: true,
            owner_session_id: Some("leased-dead-live".to_owned()),
            acquired_at_ms_ago: Some(1),
            renewed_at_ms_ago: Some(1),
            ttl_ms: Some(30_000),
            expires_in_ms: Some(29_999),
        };
        let leased = dead_live_target_summary(
            "leased-dead-live",
            Some(SessionTarget::Window { hwnd: 0x1234 }),
            Vec::new(),
            &[],
            &held_lease,
            DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS + 1,
            None,
            false,
        );
        assert_eq!(
            leased.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
        assert!(leased.lease.is_owner);
        assert!(
            ensure_cross_session_cleanup_allowed(
                "caller",
                "leased-dead-live",
                &status_response(Some(leased))
            )
            .is_err()
        );
    }

    #[test]
    fn session_agent_state_readback_hides_terminal_history_for_recent_live_sessions() {
        let now_unix_ms = 1_000_000;
        let mut recent_live = synthetic_registry_read("recent-live", now_unix_ms, 500);
        recent_live.lifecycle = "live".to_owned();
        recent_live.last_seen_ms_ago = DEAD_LIVE_SESSION_CLEANUP_MIN_QUIET_MS - 1;
        recent_live.last_seen_unix_ms = now_unix_ms.saturating_sub(recent_live.last_seen_ms_ago);
        recent_live.last_action = Some("tools/call:session_list".to_owned());

        let mut terminal = agent_read(
            "recent-live",
            AgentLifecycleState::Dead,
            Some("unprobeable_silent_ended"),
        );
        terminal.session_id = Some("recent-live".to_owned());
        terminal.spawn_id = None;

        assert!(recent_live_session_supersedes_terminal_history(
            &recent_live,
            Some(&terminal)
        ));
        assert!(session_agent_state_readback(&recent_live, Some(terminal)).is_none());

        let mut quiet_live = recent_live;
        quiet_live.last_action = None;
        let mut quiet_terminal = agent_read(
            "quiet-live",
            AgentLifecycleState::Dead,
            Some("unprobeable_silent_ended"),
        );
        quiet_terminal.session_id = Some("quiet-live".to_owned());
        quiet_terminal.spawn_id = None;

        let preserved = session_agent_state_readback(&quiet_live, Some(quiet_terminal))
            .expect("quiet live terminal history remains visible for cleanup decisions");
        assert_eq!(preserved.state, AgentLifecycleState::Dead);
        assert_eq!(
            preserved.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
    }

    #[test]
    fn terminal_unbound_agents_are_split_from_actionable_session_list_rows() {
        let rows = vec![
            agent_read("active-working", AgentLifecycleState::Working, None),
            agent_read(
                "active-stuck",
                AgentLifecycleState::Stuck,
                Some("silent_timeout"),
            ),
            agent_read(
                "dead-local-model",
                AgentLifecycleState::Dead,
                Some("local_model_registry_row_missing"),
            ),
            agent_read(
                "active-needs-input",
                AgentLifecycleState::NeedsInput,
                Some("permission_prompt"),
            ),
            agent_read(
                "agent-spawn-ambient-claude-transcript-only",
                AgentLifecycleState::Stuck,
                Some("silent_timeout_unprobeable"),
            ),
        ];
        let (active, terminal, filter) = split_unbound_agent_states(rows);

        assert_eq!(
            active
                .iter()
                .map(|row| row.anchor.as_str())
                .collect::<Vec<_>>(),
            vec!["active-working", "active-stuck", "active-needs-input"]
        );
        assert_eq!(terminal.len(), 2);
        assert_eq!(terminal[0].anchor, "dead-local-model");
        assert_eq!(terminal[0].state, AgentLifecycleState::Dead);
        assert_eq!(
            terminal[0].attention_class,
            AgentAttentionClass::TerminalSetupFailure
        );
        assert_eq!(
            terminal[1].anchor,
            "agent-spawn-ambient-claude-transcript-only"
        );
        assert_eq!(terminal[1].state, AgentLifecycleState::Stuck);
        assert_eq!(
            active
                .iter()
                .find(|row| row.anchor == "active-stuck")
                .map(|row| row.attention_class),
            Some(AgentAttentionClass::ActionableLiveStuck)
        );
        assert_eq!(filter.active_unbound_agent_count, 3);
        assert_eq!(filter.terminal_unbound_agent_count, 2);
        assert!(
            filter
                .terminal_states
                .contains(&"ambient_without_process_handle")
        );
        assert!(filter.reason.contains("not actionable attention"));
    }

    #[test]
    fn attached_registry_counts_only_os_live_process_rows() {
        let mut live_session = spawned_summary(
            "session-live",
            "agent-spawn-live",
            "local-model",
            100,
            Some(101),
        );
        let mut dead_session =
            spawned_summary("session-dead", "agent-spawn-dead", "local-model", 200, None);
        let mut cleanup_session = spawned_summary(
            "session-cleanup",
            "agent-spawn-cleanup",
            "local-model",
            300,
            None,
        );
        if let Some(agent_state) = live_session.agent_state.as_mut() {
            agent_state.state = AgentLifecycleState::Stuck;
            agent_state.reason_code = Some("silent_timeout".to_owned());
            agent_state.attention_class = AgentAttentionClass::ActionableLiveStuck;
        }
        if let Some(agent_state) = dead_session.agent_state.as_mut() {
            agent_state.state = AgentLifecycleState::Dead;
            agent_state.reason_code = Some("process_gone_without_exit_event".to_owned());
            agent_state.attention_class = AgentAttentionClass::TerminalRuntimeFailure;
        }
        if let Some(agent_state) = cleanup_session.agent_state.as_mut() {
            agent_state.state = AgentLifecycleState::Dead;
            agent_state.reason_code = Some("process_gone_without_exit_event".to_owned());
            agent_state.attention_class = AgentAttentionClass::TerminalRuntimeFailure;
        }
        let ambient = agent_read(
            "agent-spawn-ambient-claude-test",
            AgentLifecycleState::Idle,
            Some("ambient_turn_finished"),
        );
        let process_tree_ids = |pid| match pid {
            100 => vec![100, 101],
            101 => vec![101],
            200 => vec![200],
            300 => vec![300],
            other => vec![other],
        };
        let live_process_ids = |ids: &[u32]| {
            ids.iter()
                .copied()
                .filter(|pid| matches!(pid, 100 | 101 | 300))
                .collect::<Vec<_>>()
        };
        let windows = BTreeMap::from([(
            100,
            AttachedAgentWindowReadback {
                window_hwnd: 0x1234,
                process_id: 100,
                process_name: "WindowsTerminal.exe".to_owned(),
                window_title: "agent terminal".to_owned(),
            },
        )]);

        let registry = build_attached_agent_registry_with_process_probe(
            &[live_session, dead_session, cleanup_session],
            &[ambient],
            2_000,
            &process_tree_ids,
            &live_process_ids,
            &windows,
            None,
            Vec::new(),
        );

        assert_eq!(registry.exact_live_count, 2);
        assert_eq!(registry.fleet_stop_managed_count, 2);
        assert_eq!(registry.unmanaged_live_count, 0);
        assert_eq!(registry.row_count, 4);
        assert_eq!(registry.killable_live_count, 2);
        assert_eq!(registry.unprobeable_observed_count, 1);
        let live = registry
            .rows
            .iter()
            .find(|row| row.registry_id == "agent-spawn-live")
            .expect("live spawned row");
        assert!(live.counts_as_live);
        assert!(live.fleet_stop_managed);
        assert!(live.kill_handle.available);
        assert_eq!(
            live.attention_class,
            AgentAttentionClass::ActionableLiveStuck
        );
        assert_eq!(
            live.visible_window
                .as_ref()
                .map(|window| window.window_hwnd),
            Some(0x1234)
        );
        assert_eq!(
            live.controlling_terminal_window
                .as_ref()
                .map(|window| window.window_hwnd),
            Some(0x1234)
        );
        let dead = registry
            .rows
            .iter()
            .find(|row| row.registry_id == "agent-spawn-dead")
            .expect("dead spawned row");
        assert!(!dead.counts_as_live);
        assert!(!dead.fleet_stop_managed);
        assert_eq!(
            dead.not_counted_reason.as_deref(),
            Some("os_process_not_live")
        );
        assert_eq!(
            dead.attention_class,
            AgentAttentionClass::TerminalRuntimeFailure
        );
        let cleanup = registry
            .rows
            .iter()
            .find(|row| row.registry_id == "agent-spawn-cleanup")
            .expect("cleanup spawned row");
        assert!(cleanup.counts_as_live);
        assert!(cleanup.fleet_stop_managed);
        assert_eq!(
            cleanup.attention_class,
            AgentAttentionClass::CleanupRequired
        );
        let ambient = registry
            .rows
            .iter()
            .find(|row| row.registry_id == "agent-spawn-ambient-claude-test")
            .expect("ambient row");
        assert!(!ambient.counts_as_live);
        assert!(!ambient.fleet_stop_managed);
        assert_eq!(
            ambient.not_counted_reason.as_deref(),
            Some("no_process_handle")
        );
        assert!(!ambient.kill_handle.available);
    }

    fn test_service_with_profiles(profile_dir: &std::path::Path) -> anyhow::Result<SynapseService> {
        SynapseService::try_with_m2_shutdown_reason_and_m3_config(
            CancellationToken::new(),
            "test",
            CancellationToken::new(),
            &M2ServiceConfig::default(),
            M3ServiceConfig::from_cli_parts(
                Some(profile_dir.join("db")),
                Some(profile_dir.to_path_buf()),
                false,
                "127.0.0.1:0".to_owned(),
                NonZeroUsize::new(4)
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

    fn register_initialized_session(
        service: &SynapseService,
        session_id: &str,
        client_name: &str,
        now: u64,
    ) -> anyhow::Result<()> {
        let state = SessionState::new(InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new(client_name, "0.0.0-test"),
        ));
        let mut registry = service
            .session_registry_ref()
            .lock()
            .map_err(|_error| anyhow::anyhow!("session registry lock poisoned"))?;
        registry.record_initialized(session_id, &state, "http", now);
        Ok(())
    }

    fn error_code(error: &ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    fn status_response(session: Option<SessionSummary>) -> SessionStatusResponse {
        SessionStatusResponse {
            now_unix_ms: 1_000,
            stale_after_ms: 500,
            human_os_foreground: HumanOsForegroundReadback {
                source_of_truth: "test",
                status: "not_observed".to_owned(),
                hwnd: None,
                pid: None,
                process_name: None,
                process_path: None,
                window_title: None,
                read_error_code: None,
                read_error_message: None,
            },
            found: session.is_some(),
            session,
        }
    }

    #[test]
    fn ambient_process_rows_are_live_but_not_agent_killable() {
        let terminal_window = AttachedAgentWindowReadback {
            window_hwnd: 0x7777,
            process_id: 300,
            process_name: "WindowsTerminal.exe".to_owned(),
            window_title: "ambient claude".to_owned(),
        };
        let mut rows = BTreeMap::new();
        insert_ambient_agent_process_rows(
            &mut rows,
            vec![AmbientAgentProcessCandidate {
                cli: "claude",
                process_id: 333,
                parent_process_id: Some(322),
                process_name: "node.exe".to_owned(),
                command_line: "node C:\\Users\\hotra\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.js".to_owned(),
                cwd: Some("C:\\code\\Synapse".to_owned()),
                controlling_terminal_window: Some(terminal_window),
            }],
        );

        let row = rows
            .get("agent-spawn-ambient-process-claude-333")
            .expect("ambient process row");
        assert_eq!(row.kind, "ambient");
        assert_eq!(row.source, "ambient_process_scan:claude");
        assert!(row.counts_as_live);
        assert!(!row.fleet_stop_managed);
        assert_eq!(row.spawn_dir.as_deref(), Some("C:\\code\\Synapse"));
        assert_eq!(row.process.agent_process_id, Some(333));
        assert_eq!(row.process.parent_process_id, Some(322));
        assert_eq!(row.process.process_name.as_deref(), Some("node.exe"));
        assert_eq!(row.process.live_process_ids, vec![300, 322, 333]);
        assert_eq!(
            row.controlling_terminal_window
                .as_ref()
                .map(|window| window.window_hwnd),
            Some(0x7777)
        );
        assert!(!row.kill_handle.available);
        assert_eq!(row.kill_handle.kind, "process_tree_pending_k2");
        assert_eq!(
            ambient_agent_cli("node.exe", "node @anthropic-ai/claude-code/bin/claude.js"),
            Some("claude")
        );
        assert_eq!(
            ambient_agent_cli("powershell.exe", "powershell.exe -NoProfile"),
            None
        );
    }

    #[test]
    fn ambient_agent_cli_ignores_helper_process_false_positives() {
        assert_eq!(
            ambient_agent_cli(
                "claude.exe",
                "\"C:\\Users\\hotra\\AppData\\Roaming\\npm\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe\" --resume"
            ),
            Some("claude")
        );
        assert_eq!(
            ambient_agent_cli(
                "node.exe",
                "\"C:\\Program Files\\nodejs\\node.exe\" C:\\Users\\hotra\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js resume --yolo"
            ),
            Some("codex")
        );
        assert_eq!(
            ambient_agent_cli(
                "codex.exe",
                "C:\\Users\\hotra\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\bin\\codex.exe resume --yolo"
            ),
            Some("codex")
        );
        assert!(ambient_agent_child_is_covered_by_parent(
            "codex",
            "codex.exe",
            "node.exe",
            "node C:\\Users\\hotra\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js resume --yolo",
        ));
        assert_eq!(
            ambient_agent_cli(
                "node.exe",
                "node C:\\Users\\hotra\\.claude\\tools\\claude-image-gen\\mcp-server\\build\\bundle.js",
            ),
            None
        );
        assert_eq!(
            ambient_agent_cli(
                "cmd.exe",
                "cmd /c C:\\Users\\hotra\\.claude\\tools\\claude-image-gen\\launch-mcp.cmd",
            ),
            None
        );
        assert_eq!(
            ambient_agent_cli(
                "pwsh.exe",
                "pwsh -File C:\\Users\\hotra\\.claude\\statusline.ps1",
            ),
            None
        );
        assert_eq!(
            ambient_agent_cli(
                "bash.exe",
                "bash -c source /c/Users/hotra/.claude/shell-snapshots/snapshot.sh && codex",
            ),
            None
        );
    }

    fn agent_read(
        anchor: &str,
        state: AgentLifecycleState,
        reason_code: Option<&str>,
    ) -> AgentStateRead {
        AgentStateRead {
            anchor: anchor.to_owned(),
            spawn_id: Some(anchor.to_owned()),
            session_id: None,
            agent_kind: Some("test-agent".to_owned()),
            state,
            reason_code: reason_code.map(str::to_owned),
            attention_class: AgentAttentionClass::for_lifecycle(state, reason_code),
            since_unix_ms: 1_000,
            last_event_unix_ms: 1_000,
            last_event_kind: AgentEventKind::StateChanged,
            silent_ms: 0,
            waiting_for: None,
            runaway: false,
            consecutive_identical_tool_calls: 0,
            last_tool_name: None,
            launcher_process_id: None,
            agent_process_id: None,
            log_dir: None,
        }
    }

    fn dead_live_target_summary(
        session_id: &str,
        active_target: Option<SessionTarget>,
        target_claims: Vec<TargetClaimRead>,
        all_target_claims: &[TargetClaimRead],
        lease_status: &synapse_action::LeaseStatus,
        last_seen_ms_ago: u64,
        last_action: Option<&str>,
        has_persisted_cdp_owner: bool,
    ) -> SessionSummary {
        dead_live_target_summary_with_reason(
            session_id,
            active_target,
            target_claims,
            all_target_claims,
            lease_status,
            last_seen_ms_ago,
            last_action,
            has_persisted_cdp_owner,
            Some("process_gone_without_exit_event"),
        )
    }

    fn dead_live_target_summary_with_reason(
        session_id: &str,
        active_target: Option<SessionTarget>,
        target_claims: Vec<TargetClaimRead>,
        all_target_claims: &[TargetClaimRead],
        lease_status: &synapse_action::LeaseStatus,
        last_seen_ms_ago: u64,
        last_action: Option<&str>,
        has_persisted_cdp_owner: bool,
        death_reason_code: Option<&str>,
    ) -> SessionSummary {
        let now_unix_ms = 1_000_000;
        let mut registry = synthetic_registry_read(session_id, now_unix_ms, 500);
        registry.lifecycle = "live".to_owned();
        registry.last_seen_unix_ms = now_unix_ms.saturating_sub(last_seen_ms_ago);
        registry.last_seen_ms_ago = last_seen_ms_ago;
        registry.last_action = last_action.map(ToOwned::to_owned);
        let attention_target = active_target.clone();
        let mut summary = build_session_summary(
            session_id,
            Some(registry),
            active_target,
            target_claims,
            all_target_claims,
            lease_status,
            now_unix_ms,
            500,
            has_persisted_cdp_owner,
        )
        .expect("dead live summary");
        let mut agent_state = agent_read(session_id, AgentLifecycleState::Dead, death_reason_code);
        agent_state.session_id = Some(session_id.to_owned());
        agent_state.spawn_id = None;
        summary.agent_state = Some(agent_state);
        summary.attention_class = session_attention_class(
            session_id,
            &summary.registry,
            attention_target.as_ref(),
            &summary.target_claims,
            all_target_claims,
            &summary.lease,
            summary.agent_state.as_ref(),
            has_persisted_cdp_owner,
        );
        summary
    }

    fn spawned_summary(
        session_id: &str,
        spawn_id: &str,
        cli: &str,
        launcher_process_id: u32,
        agent_process_id: Option<u32>,
    ) -> SessionSummary {
        let registry = SessionRegistryRead {
            session_id: session_id.to_owned(),
            transport: "http".to_owned(),
            client_name: Some(format!("synapse-{cli}-agent")),
            client_version: Some("test".to_owned()),
            protocol_version: Some("test".to_owned()),
            agent_kind: cli.to_owned(),
            lifecycle: "live".to_owned(),
            started_at_unix_ms: 1_000,
            last_seen_unix_ms: 1_900,
            last_seen_ms_ago: 100,
            stale_after_ms: 300_000,
            closed_at_unix_ms: None,
            last_action: None,
            last_reason_code: None,
            spawned_agent: Some(SpawnedAgentRead {
                spawn_id: spawn_id.to_owned(),
                cli: cli.to_owned(),
                launcher_process_id,
                agent_process_id,
                started_by_session_id: Some("caller".to_owned()),
                launched_at_unix_ms: 1_000,
                launch_target: "background".to_owned(),
                log_dir: format!("C:\\test\\{spawn_id}"),
                template_id: None,
                template_version: None,
                control: None,
            }),
        };
        SessionSummary {
            registry,
            active_target: None,
            agent_logical_foreground: build_agent_logical_foreground(session_id, None),
            foreground_lane: build_foreground_lane(
                session_id,
                None,
                &[],
                &synapse_action::LeaseStatus::unheld(),
            ),
            target_claims: Vec::new(),
            persisted_cdp_target_owners: Vec::new(),
            lease: SessionLeaseReadback {
                held: false,
                owner_session_id: None,
                is_owner: false,
                acquired_at_ms_ago: None,
                renewed_at_ms_ago: None,
                ttl_ms: None,
                expires_in_ms: None,
            },
            agent_state: Some(AgentStateRead {
                anchor: spawn_id.to_owned(),
                spawn_id: Some(spawn_id.to_owned()),
                session_id: Some(session_id.to_owned()),
                agent_kind: Some(cli.to_owned()),
                state: AgentLifecycleState::Working,
                reason_code: Some("spawn_ready".to_owned()),
                attention_class: AgentAttentionClass::None,
                since_unix_ms: 1_000,
                last_event_unix_ms: 1_900,
                last_event_kind: AgentEventKind::SpawnReady,
                silent_ms: 100,
                waiting_for: None,
                runaway: false,
                consecutive_identical_tool_calls: 0,
                last_tool_name: None,
                launcher_process_id: Some(launcher_process_id),
                agent_process_id,
                log_dir: Some(format!("C:\\test\\{spawn_id}")),
            }),
            attention_class: AgentAttentionClass::None,
        }
    }
}
