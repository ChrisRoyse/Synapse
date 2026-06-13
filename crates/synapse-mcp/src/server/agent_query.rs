//! `agent_query` (#911) — a Temporal `__stack_trace`-style now-snapshot for a
//! single agent.
//!
//! Answers "what is this agent doing right now, and why" with **zero agent
//! cooperation** by reading three durable sources of truth and naming each in
//! the response so every claim is auditable:
//!
//! 1. **Lifecycle state + reason + waiting_for** — reconstructed from the
//!    `CF_AGENT_EVENTS` journal (#897) through the exact reducer the live
//!    `#898` tracker uses. The journal is the source of truth; the in-memory
//!    tracker is only a cache rebuilt from it, so deriving from the rows this
//!    call already scanned makes the answer deterministic, restart-robust, and
//!    self-consistent with the events it returns.
//! 2. **Current/last tool call + recent events** — the `CF_AGENT_EVENTS`
//!    journal rows themselves, keyed `(ts_ns, seq)`, in compact form.
//! 3. **Tokens this turn + context-window estimate + one-line activity
//!    summary** — `CF_AGENT_TRANSCRIPTS` rows (#900), keyed `(spawn_id,
//!    line_no)`. Content is already bounded and capped at ingest, matching the
//!    OTel GenAI convention of keeping prompt/completion content out of the
//!    metadata layer.
//!
//! The default path is a **pure read** — like a Temporal Query it never writes
//! and never blocks on the agent. The optional `deep: true` path is the
//! cooperative analog (a write-then-poll, like a Temporal Update/Signal): it
//! sends a `status_request` to the agent's mailbox (#908) and polls the
//! caller's inbox for a correlated reply until a timeout. An absent reply is
//! reported as `not_answered` — **never fabricated**.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{
    AgentEventKind, AgentEventRecord, AgentTranscriptRecord, TranscriptRole, error_codes,
};
use synapse_storage::{
    Db, agent_events::agent_event_scan_start, agent_transcripts::agent_transcript_spawn_prefix, cf,
    decode_json,
};

use rmcp::{RoleServer, service::RequestContext};

use super::agent_state::{AgentLifecycleState, AgentStateRead};
use super::{
    ErrorData, Json, Parameters, SynapseService, agent_events::unix_time_ns_now, mcp_error, tool,
    tool_router,
};

// ----------------------------------------------------------------------------
// Tunables
// ----------------------------------------------------------------------------

/// Default count of recent journal events returned in `recent_events`.
const DEFAULT_MAX_EVENTS: usize = 20;
/// Hard ceiling on `max_events` so one call cannot return an unbounded list.
const MAX_MAX_EVENTS: usize = 200;

/// Default journal lookback window. The snapshot only needs the agent's recent
/// history; older rows expire from the journal by TTL anyway.
const DEFAULT_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1_000;
/// Hard ceiling on the lookback window.
const MAX_LOOKBACK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// Upper bound on journal rows scanned in one call. A truncated scan is a loud
/// error, never a silently-incomplete snapshot. Mirrors `agent_cost`.
const MAX_SCAN_ROWS_PER_CALL: usize = 200_000;
/// Rows pulled per `scan_cf_from` page.
const SCAN_CHUNK_ROWS: usize = 4_096;

/// Default cooperative-status wait for `deep: true`.
const DEFAULT_DEEP_TIMEOUT_MS: u64 = 2_000;
/// Hard ceiling on the cooperative wait.
const MAX_DEEP_TIMEOUT_MS: u64 = 60_000;
/// Poll cadence while waiting for a cooperative reply.
const DEEP_POLL_INTERVAL_MS: u64 = 50;
/// Mailbox message kind for the cooperative status request.
const DEEP_REQUEST_KIND: &str = "status_request";
/// Mailbox message kind a cooperating agent uses to answer.
const DEEP_RESPONSE_KIND: &str = "status_response";
/// TTL for the cooperative request row (kept short — a stale request is noise).
const DEEP_REQUEST_TTL_MS: u64 = 60_000;

/// Single-line activity-summary cap (characters), so a long assistant message
/// never floods the snapshot. The full text + its hash live in the transcript.
const ACTIVITY_SUMMARY_MAX_CHARS: usize = 200;

// ----------------------------------------------------------------------------
// Params
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentQueryParams {
    /// The agent to inspect: its MCP session id, or its `agent-spawn-*` spawn
    /// id. Either resolves to the same agent once a `spawn_ready` row has
    /// linked them.
    pub session_id: String,
    /// How many of the most recent journal events to include in
    /// `recent_events` (compact form). Capped server-side.
    #[serde(default = "default_max_events")]
    #[schemars(default = "default_max_events", range(min = 1, max = 200))]
    pub max_events: usize,
    /// Journal lookback window in milliseconds. Older rows are not scanned.
    #[serde(default = "default_lookback_ms")]
    #[schemars(default = "default_lookback_ms", range(min = 1, max = 604_800_000))]
    pub lookback_ms: u64,
    /// When true, also send a cooperative `status_request` to the agent's
    /// mailbox and wait up to `deep_timeout_ms` for the agent's own answer.
    /// Requires the daemon to run in HTTP mode (so the caller has a session
    /// id) and the target to have a live MCP session. An absent answer is
    /// reported as `not_answered`, never fabricated.
    #[serde(default)]
    pub deep: bool,
    /// Cooperative-status wait budget for `deep: true`, in milliseconds.
    #[serde(default = "default_deep_timeout_ms")]
    #[schemars(default = "default_deep_timeout_ms", range(min = 1, max = 60_000))]
    pub deep_timeout_ms: u64,
}

const fn default_max_events() -> usize {
    DEFAULT_MAX_EVENTS
}
const fn default_lookback_ms() -> u64 {
    DEFAULT_LOOKBACK_MS
}
const fn default_deep_timeout_ms() -> u64 {
    DEFAULT_DEEP_TIMEOUT_MS
}

// ----------------------------------------------------------------------------
// Response
// ----------------------------------------------------------------------------

/// One tool call as reconstructed from journal `tool_call_started` /
/// `tool_call_finished` rows.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolCallSnapshot {
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_unix_ms: Option<u64>,
    /// Wall-clock elapsed. For an in-flight call this is `now - started_at`;
    /// for a finished call it is `finished_at - started_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// True when a `tool_call_started` row has no matching
    /// `tool_call_finished` — the agent is inside this tool right now.
    pub in_flight: bool,
    /// Bounded argument metadata from the journal payload (byte length + hash);
    /// never the raw arguments (OTel GenAI content stays out of the journal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_digest: Option<Value>,
    /// Low-cardinality error class when the tool call ended in error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

/// One journal row in compact form for `recent_events`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactEvent {
    pub ts_unix_ms: u64,
    pub kind: AgentEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// Token usage for the latest usage-bearing transcript row.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnSnapshot {
    /// 1-based turn index this usage was reported on (when the source carries
    /// one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub reasoning_output_tokens: u64,
    /// `input + output + cache_read + cache_creation` for this row.
    pub total_tokens: u64,
    /// The transcript line number this usage came from (its physical anchor).
    pub source_line_no: u64,
}

/// The cooperative `deep: true` outcome.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CooperativeAnswer {
    /// `answered` | `not_answered` | `channel_unavailable`.
    pub status: String,
    /// How long the caller waited for a reply.
    pub waited_ms: u64,
    /// The id of the `status_request` message sent to the agent, when one was
    /// sent (absent for `channel_unavailable`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_id: Option<String>,
    /// The agent's own answer payload, present only on `answered`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
    /// Why the channel was unavailable, present only on `channel_unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Auditing readback of the journal scan that produced this snapshot.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanReadback {
    pub lookback_ms: u64,
    pub events_scanned: usize,
    pub events_matched: usize,
    pub transcript_rows_scanned: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentQueryResponse {
    pub query_ts_unix_ms: u64,
    /// False when no registry row and no journal row resolve to the requested
    /// id; all detail fields are then empty.
    pub found: bool,
    /// Attribution anchor (spawn id for spawned agents, else the session id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,

    // ---- Lifecycle (SoT: CF_AGENT_EVENTS journal via the #898 reducer) ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentLifecycleState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent_ms: Option<u64>,
    pub runaway: bool,
    pub consecutive_identical_tool_calls: u32,
    /// What the agent is blocked on (notification type or `tool:<name>`), or
    /// the loop signature while runaway-stuck.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,

    // ---- Activity (SoT: CF_AGENT_EVENTS journal rows) ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool_call: Option<ToolCallSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_call: Option<ToolCallSnapshot>,
    pub recent_events: Vec<CompactEvent>,

    // ---- Tokens & activity summary (SoT: CF_AGENT_TRANSCRIPTS) ----
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnSnapshot>,
    /// Estimate of the live context-window occupancy: the latest reported
    /// `input + cache_read + cache_creation + output` (the working set the
    /// model just processed). An estimate, not an authoritative count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_estimate_tokens: Option<u64>,
    /// One line derived locally from the most recent assistant transcript row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_summary: Option<String>,

    /// Durable task/attempt link. Always null until #910 (durable agent task
    /// queue) lands; reported null rather than fabricated.
    pub task: Option<Value>,

    /// Cooperative answer, present only when `deep: true` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooperative: Option<CooperativeAnswer>,

    pub scan: ScanReadback,
    /// Field-group → source-of-truth, so every claim above is auditable.
    pub sources: BTreeMap<String, String>,
}

// ----------------------------------------------------------------------------
// Tool
// ----------------------------------------------------------------------------

#[tool_router(router = agent_query_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Answer 'what is this agent doing right now and why' with zero agent cooperation: a now-snapshot of lifecycle state + reason, current/last tool call, recent journal events, tokens this turn, a context-window estimate, and a one-line activity summary. Every field names its source of truth. Optional deep=true additionally sends a cooperative status request via the agent mailbox and waits for the agent's own answer (absent = not_answered, never fabricated)."
    )]
    pub async fn agent_query(
        &self,
        params: Parameters<AgentQueryParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentQueryResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_query",
            "tool.invocation kind=agent_query"
        );
        let caller_session =
            super::context::mcp_session_id_from_request_context(&request_context)?;
        self.agent_query_impl(params.0, caller_session.as_deref())
            .await
            .map(Json)
    }
}

impl SynapseService {
    async fn agent_query_impl(
        &self,
        params: AgentQueryParams,
        caller_session: Option<&str>,
    ) -> Result<AgentQueryResponse, ErrorData> {
        validate_params(&params)?;
        let now_unix_ms = unix_time_ms_now();
        let db = self.agent_query_db()?;

        let lookup_id = params.session_id.trim().to_owned();
        let lookback_ns = params.lookback_ms.saturating_mul(1_000_000);

        // ---- Scan the journal once: collect this agent's rows in order. ----
        let scan = scan_agent_journal(&db, &lookup_id, now_unix_ms, lookback_ns)?;

        // Resolve identity from the matched rows (a spawn_ready row carries
        // both ids and links them).
        let mut spawn_id: Option<String> = None;
        let mut session_id: Option<String> = None;
        for record in &scan.matched {
            if spawn_id.is_none() {
                if let Some(value) = record.spawn_id.as_deref() {
                    spawn_id = Some(value.to_owned());
                }
            }
            if session_id.is_none() {
                if let Some(value) = record.session_id.as_deref() {
                    session_id = Some(value.to_owned());
                }
            }
        }
        // The lookup id might itself be a spawn id we never saw in a payload
        // (e.g. only state_changed rows so far).
        if spawn_id.is_none() && lookup_id.starts_with("agent-spawn-") {
            spawn_id = Some(lookup_id.clone());
        }

        // ---- Lifecycle state, reconstructed from the same scanned rows. ----
        let lifecycle: Option<AgentStateRead> =
            super::agent_state::read_from_journal_records(&scan.matched, &lookup_id, now_unix_ms);

        // ---- Transcripts (spawn-only): tokens + activity summary. ----
        let (turn, context_window_estimate_tokens, activity_summary, transcript_rows_scanned) =
            if let Some(spawn) = spawn_id.as_deref() {
                read_transcript_snapshot(&db, spawn)?
            } else {
                (None, None, None, 0)
            };

        let found = lifecycle.is_some() || !scan.matched.is_empty();

        if !found {
            return Ok(empty_response(
                now_unix_ms,
                &scan,
                transcript_rows_scanned,
                params.lookback_ms,
            ));
        }

        // ---- Tool-call reconstruction + compact recent events. ----
        let (current_tool_call, last_tool_call) = reconstruct_tool_calls(&scan.matched, now_unix_ms);
        let recent_events = compact_recent_events(&scan.matched, params.max_events);

        let anchor = lifecycle
            .as_ref()
            .map(|read| read.anchor.clone())
            .or_else(|| spawn_id.clone())
            .or_else(|| session_id.clone())
            .unwrap_or_else(|| lookup_id.clone());

        // Prefer the lifecycle read's resolved ids (they reflect the reducer's
        // session↔spawn linkage); fall back to what the scan saw.
        if let Some(read) = &lifecycle {
            if read.spawn_id.is_some() {
                spawn_id.clone_from(&read.spawn_id);
            }
            if read.session_id.is_some() {
                session_id.clone_from(&read.session_id);
            }
        }
        let agent_kind = lifecycle.as_ref().and_then(|read| read.agent_kind.clone());

        // ---- Optional cooperative answer (deep mode). ----
        let cooperative = if params.deep {
            Some(
                self.cooperative_answer(
                    caller_session,
                    session_id.as_deref(),
                    params.deep_timeout_ms,
                )
                .await,
            )
        } else {
            None
        };

        Ok(AgentQueryResponse {
            query_ts_unix_ms: now_unix_ms,
            found: true,
            anchor: Some(anchor),
            spawn_id,
            session_id,
            agent_kind,
            state: lifecycle.as_ref().map(|read| read.state),
            reason_code: lifecycle.as_ref().and_then(|read| read.reason_code.clone()),
            since_unix_ms: lifecycle.as_ref().map(|read| read.since_unix_ms),
            silent_ms: lifecycle.as_ref().map(|read| read.silent_ms),
            runaway: lifecycle.as_ref().is_some_and(|read| read.runaway),
            consecutive_identical_tool_calls: lifecycle
                .as_ref()
                .map_or(0, |read| read.consecutive_identical_tool_calls),
            waiting_for: lifecycle.as_ref().and_then(|read| read.waiting_for.clone()),
            current_tool_call,
            last_tool_call,
            recent_events,
            turn,
            context_window_estimate_tokens,
            activity_summary,
            task: None,
            cooperative,
            scan: ScanReadback {
                lookback_ms: params.lookback_ms,
                events_scanned: scan.events_scanned,
                events_matched: scan.matched.len(),
                transcript_rows_scanned,
            },
            sources: sources_map(),
        })
    }

    /// Opens the shared M3 storage handle (same path `agent_cost` uses).
    fn agent_query_db(&self) -> Result<std::sync::Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent query storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    /// `deep: true` cooperative path: send a `status_request` to the target's
    /// mailbox and poll the caller's inbox for a correlated `status_response`.
    async fn cooperative_answer(
        &self,
        caller_session: Option<&str>,
        target_session: Option<&str>,
        timeout_ms: u64,
    ) -> CooperativeAnswer {
        let Some(caller) = caller_session else {
            return CooperativeAnswer {
                status: "channel_unavailable".to_owned(),
                waited_ms: 0,
                request_message_id: None,
                answer: None,
                detail: Some(
                    "deep mode needs the caller's MCP session id; run the daemon in HTTP mode so \
                     each agent has its own Mcp-Session-Id"
                        .to_owned(),
                ),
            };
        };
        let Some(target) = target_session else {
            return CooperativeAnswer {
                status: "channel_unavailable".to_owned(),
                waited_ms: 0,
                request_message_id: None,
                answer: None,
                detail: Some(
                    "the target agent has no live MCP session to address (spawn not yet linked, \
                     or it died before registering)"
                        .to_owned(),
                ),
            };
        };

        match self
            .agent_query_deep_request(caller, target, timeout_ms)
            .await
        {
            Ok(answer) => answer,
            Err(error) => CooperativeAnswer {
                status: "channel_unavailable".to_owned(),
                waited_ms: 0,
                request_message_id: None,
                answer: None,
                detail: Some(error),
            },
        }
    }

    /// Sends a `status_request` to the target's mailbox and polls the caller's
    /// inbox for a correlated `status_response` until `timeout_ms`. The
    /// responding agent correlates by echoing the request message's
    /// `message_id` into its reply payload as `in_reply_to`.
    ///
    /// Returns `Err(detail)` only when the request itself cannot be delivered
    /// (e.g. the target is not a live session) — the caller maps that to
    /// `channel_unavailable`. A delivered-but-unanswered request returns an
    /// `Ok(not_answered)` after the timeout.
    async fn agent_query_deep_request(
        &self,
        caller: &str,
        target: &str,
        timeout_ms: u64,
    ) -> Result<CooperativeAnswer, String> {
        let send = self
            .agent_send_impl(
                super::agent_mailbox::AgentSendParams {
                    to_session: target.to_owned(),
                    kind: DEEP_REQUEST_KIND.to_owned(),
                    payload: json!({
                        "request": "status",
                        "from": caller,
                        "reply_kind": DEEP_RESPONSE_KIND,
                        "reply_to_session": caller,
                        "instructions": "reply with agent_send kind=status_response, \
                                         payload.in_reply_to set to this message's message_id",
                    }),
                    artifact_handle: None,
                    ttl_ms: DEEP_REQUEST_TTL_MS,
                },
                caller,
            )
            .map_err(|error| format!("status_request could not be delivered: {error}"))?;
        let request_message_id = send.message_id;

        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let db = self.agent_query_db().map_err(|error| error.to_string())?;
        loop {
            // Peek (never drain) so unrelated mail is preserved; consume only
            // the correlated reply.
            let inbox = self
                .agent_inbox_impl(
                    super::agent_mailbox::AgentInboxParams {
                        drain: false,
                        max_messages: 1_000,
                    },
                    caller,
                )
                .map_err(|error| error.to_string())?;
            if let Some(reply) = inbox.messages.into_iter().find(|message| {
                message.kind == DEEP_RESPONSE_KIND
                    && message.payload.get("in_reply_to").and_then(Value::as_str)
                        == Some(request_message_id.as_str())
            }) {
                // Consume exactly the correlated reply row.
                if let Err(error) =
                    db.delete_batch(cf::CF_KV, [reply.row_key.as_bytes().to_vec()])
                {
                    tracing::warn!(
                        code = "AGENT_QUERY_DEEP_REPLY_CLEANUP_FAILED",
                        row_key = %reply.row_key,
                        detail = %error,
                        "consumed cooperative reply but failed to delete its mailbox row"
                    );
                }
                return Ok(CooperativeAnswer {
                    status: "answered".to_owned(),
                    waited_ms: elapsed_ms(started),
                    request_message_id: Some(request_message_id),
                    answer: Some(reply.payload),
                    detail: None,
                });
            }
            if started.elapsed() >= timeout {
                return Ok(CooperativeAnswer {
                    status: "not_answered".to_owned(),
                    waited_ms: elapsed_ms(started),
                    request_message_id: Some(request_message_id),
                    answer: None,
                    detail: Some(format!(
                        "no status_response within {timeout_ms}ms; the agent did not (or could \
                         not) answer cooperatively"
                    )),
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(DEEP_POLL_INTERVAL_MS)).await;
        }
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ----------------------------------------------------------------------------
// Journal scan
// ----------------------------------------------------------------------------

struct JournalScan {
    matched: Vec<AgentEventRecord>,
    events_scanned: usize,
}

/// Scans `CF_AGENT_EVENTS` over the lookback window, collecting every row that
/// belongs to the same agent as `lookup_id`.
///
/// A query by session id must still find rows emitted *before* the session was
/// linked to its spawn (e.g. `spawn_requested`, which carries only the spawn
/// id). Those rows do not name the session id, so a single flat pass would
/// miss them. Two passes solve it: pass 1 finds the rows that name `lookup_id`
/// directly and learns the agent's full id set (spawn id ⇆ session id are both
/// present on the linking `spawn_ready` row); pass 2 re-collects against that
/// id set. The second pass runs only when pass 1 actually discovered a partner
/// id, so the common case stays single-pass.
fn scan_agent_journal(
    db: &Db,
    lookup_id: &str,
    now_unix_ms: u64,
    lookback_ns: u64,
) -> Result<JournalScan, ErrorData> {
    let now_ns = now_unix_ms.saturating_mul(1_000_000);
    let start_key = agent_event_scan_start(now_ns.saturating_sub(lookback_ns));

    let mut related: Vec<String> = vec![lookup_id.to_owned()];
    let first = scan_pass(db, &start_key, &related)?;
    // Learn any partner ids the matched rows revealed.
    for record in &first.matched {
        push_unique(&mut related, record.spawn_id.as_deref());
        push_unique(&mut related, record.session_id.as_deref());
    }
    if related.len() == 1 {
        return Ok(first);
    }
    // A partner id surfaced — re-scan so rows that named only the partner id
    // (pre-link rows) are captured too.
    scan_pass(db, &start_key, &related)
}

/// One forward scan of the window, collecting rows whose spawn or session id is
/// in `related`. Rows arrive in ascending `(ts_ns, seq)` order and are kept in
/// that order.
fn scan_pass(db: &Db, start_key: &[u8], related: &[String]) -> Result<JournalScan, ErrorData> {
    let mut start = start_key.to_vec();
    let mut matched: Vec<AgentEventRecord> = Vec::new();
    let mut events_scanned: usize = 0;

    loop {
        if events_scanned >= MAX_SCAN_ROWS_PER_CALL {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_QUERY_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} \
                     CF_AGENT_EVENTS rows; narrow lookback_ms — a truncated snapshot would \
                     under-report the agent's history"
                ),
            ));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            events_scanned += 1;
            match decode_json::<AgentEventRecord>(value) {
                Ok(record) => {
                    if record_matches(&record, related) {
                        matched.push(record);
                    }
                }
                Err(error) => {
                    // A corrupt journal row is logged and skipped — never
                    // silently fabricated into the snapshot.
                    tracing::error!(
                        code = "AGENT_QUERY_JOURNAL_ROW_INVALID",
                        key = ?key,
                        detail = %error,
                        "CF_AGENT_EVENTS row failed to decode during agent_query; row skipped"
                    );
                }
            }
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }

    Ok(JournalScan {
        matched,
        events_scanned,
    })
}

fn record_matches(record: &AgentEventRecord, related: &[String]) -> bool {
    let in_set = |id: Option<&str>| id.is_some_and(|value| related.iter().any(|r| r == value));
    in_set(record.spawn_id.as_deref()) || in_set(record.session_id.as_deref())
}

fn push_unique(set: &mut Vec<String>, value: Option<&str>) {
    if let Some(value) = value {
        if !set.iter().any(|existing| existing == value) {
            set.push(value.to_owned());
        }
    }
}

// ----------------------------------------------------------------------------
// Tool-call reconstruction
// ----------------------------------------------------------------------------

/// Reconstructs `(current_tool_call, last_tool_call)` from the ordered journal
/// rows. Pairing is by `tool_call_id` when present, else positional (a started
/// row is closed by the next finished row).
fn reconstruct_tool_calls(
    records: &[AgentEventRecord],
    now_unix_ms: u64,
) -> (Option<ToolCallSnapshot>, Option<ToolCallSnapshot>) {
    // Open starts not yet matched to a finish, in encounter order.
    let mut open: Vec<ToolCallSnapshot> = Vec::new();
    // The most recently completed call.
    let mut last_finished: Option<ToolCallSnapshot> = None;

    for record in records {
        let ts_ms = record.ts_ns / 1_000_000;
        match record.kind {
            AgentEventKind::ToolCallStarted => {
                open.push(ToolCallSnapshot {
                    tool_name: record
                        .attributes
                        .tool_name
                        .clone()
                        .unwrap_or_else(|| "unknown".to_owned()),
                    tool_call_id: record.attributes.tool_call_id.clone(),
                    started_at_unix_ms: Some(ts_ms),
                    finished_at_unix_ms: None,
                    elapsed_ms: None,
                    in_flight: true,
                    args_digest: tool_args_digest(&record.payload),
                    error_type: None,
                });
            }
            AgentEventKind::ToolCallFinished => {
                // Find the matching open start: by id, else the oldest open.
                let idx = record
                    .attributes
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| {
                        open.iter()
                            .position(|call| call.tool_call_id.as_ref() == Some(id))
                    })
                    .or_else(|| (!open.is_empty()).then_some(0));
                let mut snap = match idx {
                    Some(i) => open.remove(i),
                    None => ToolCallSnapshot {
                        tool_name: record
                            .attributes
                            .tool_name
                            .clone()
                            .unwrap_or_else(|| "unknown".to_owned()),
                        tool_call_id: record.attributes.tool_call_id.clone(),
                        started_at_unix_ms: None,
                        finished_at_unix_ms: None,
                        elapsed_ms: None,
                        in_flight: false,
                        args_digest: None,
                        error_type: None,
                    },
                };
                snap.in_flight = false;
                snap.finished_at_unix_ms = Some(ts_ms);
                snap.error_type.clone_from(&record.attributes.error_type);
                // Prefer the source-reported duration when present, else derive.
                snap.elapsed_ms = record
                    .payload
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| snap.started_at_unix_ms.map(|start| ts_ms.saturating_sub(start)));
                if record.attributes.tool_name.is_some() && snap.tool_name == "unknown" {
                    snap.tool_name = record.attributes.tool_name.clone().unwrap_or(snap.tool_name);
                }
                last_finished = Some(snap);
            }
            _ => {}
        }
    }

    // The current (in-flight) call is the most recent still-open start.
    let mut current = open.pop();
    if let Some(call) = current.as_mut() {
        call.elapsed_ms = call
            .started_at_unix_ms
            .map(|start| now_unix_ms.saturating_sub(start));
    }

    // `last_tool_call` is the most recent *completed* call. When no call has
    // finished yet but one is in flight, fall back to that so a freshly-started
    // agent still reports a tool name.
    let last = last_finished.or_else(|| current.clone());
    (current, last)
}

/// Bounded argument metadata from a `tool_call_*` journal payload — byte length
/// and hash only, never raw arguments.
fn tool_args_digest(payload: &Value) -> Option<Value> {
    let bytes = payload.get("tool_input_bytes");
    let sha = payload.get("tool_input_sha256");
    match (bytes, sha) {
        (None, None) => None,
        _ => Some(json!({
            "tool_input_bytes": bytes.cloned().unwrap_or(Value::Null),
            "tool_input_sha256": sha.cloned().unwrap_or(Value::Null),
        })),
    }
}

fn compact_recent_events(records: &[AgentEventRecord], max_events: usize) -> Vec<CompactEvent> {
    let start = records.len().saturating_sub(max_events);
    records[start..]
        .iter()
        .map(|record| CompactEvent {
            ts_unix_ms: record.ts_ns / 1_000_000,
            kind: record.kind,
            reason_code: record.reason_code.clone(),
            state_from: record.state_from.clone(),
            state_to: record.state_to.clone(),
            tool_name: record.attributes.tool_name.clone(),
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Transcript snapshot
// ----------------------------------------------------------------------------

/// Reads `CF_AGENT_TRANSCRIPTS` for one spawn and derives the latest token
/// usage (tokens this turn), a context-window estimate, and a one-line
/// activity summary. Returns `(turn, context_estimate, activity, rows_scanned)`.
#[allow(clippy::type_complexity)]
fn read_transcript_snapshot(
    db: &Db,
    spawn_id: &str,
) -> Result<(Option<TurnSnapshot>, Option<u64>, Option<String>, usize), ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &agent_transcript_spawn_prefix(spawn_id))
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let rows_scanned = rows.len();

    let mut latest_usage: Option<(u64, AgentTranscriptRecord)> = None;
    let mut latest_assistant: Option<(u64, String)> = None;

    for (_key, value) in &rows {
        let record = match decode_json::<AgentTranscriptRecord>(value) {
            Ok(record) => record,
            Err(error) => {
                tracing::error!(
                    code = "AGENT_QUERY_TRANSCRIPT_ROW_INVALID",
                    spawn_id,
                    detail = %error,
                    "CF_AGENT_TRANSCRIPTS row failed to decode during agent_query; row skipped"
                );
                continue;
            }
        };
        let line_no = record.line_no;
        if record.usage.as_ref().is_some_and(|usage| !usage.is_empty())
            && latest_usage
                .as_ref()
                .is_none_or(|(seen, _)| line_no >= *seen)
        {
            latest_usage = Some((line_no, record.clone()));
        }
        if record.role == Some(TranscriptRole::Assistant) {
            if let Some(summary) = record.content_summary.as_ref() {
                let trimmed = summary.trim();
                if !trimmed.is_empty()
                    && latest_assistant
                        .as_ref()
                        .is_none_or(|(seen, _)| line_no >= *seen)
                {
                    latest_assistant = Some((line_no, trimmed.to_owned()));
                }
            }
        }
    }

    let (turn, context_estimate) = match latest_usage {
        Some((line_no, record)) => {
            let usage = record.usage.unwrap_or_default();
            let input = usage.input_tokens.unwrap_or(0);
            let output = usage.output_tokens.unwrap_or(0);
            let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
            let cache_creation = usage.cache_creation_input_tokens.unwrap_or(0);
            let reasoning = usage.reasoning_output_tokens.unwrap_or(0);
            let total = input
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_creation);
            let estimate = input
                .saturating_add(cache_read)
                .saturating_add(cache_creation)
                .saturating_add(output);
            (
                Some(TurnSnapshot {
                    turn_index: record.turn_index,
                    model: record.model,
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_input_tokens: cache_read,
                    cache_creation_input_tokens: cache_creation,
                    reasoning_output_tokens: reasoning,
                    total_tokens: total,
                    source_line_no: line_no,
                }),
                Some(estimate),
            )
        }
        None => (None, None),
    };

    let activity_summary = latest_assistant.map(|(_line, text)| one_line(&text));

    Ok((turn, context_estimate, activity_summary, rows_scanned))
}

/// Collapses whitespace to single spaces and caps length, so a multi-line
/// assistant message renders as one readable line.
fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > ACTIVITY_SUMMARY_MAX_CHARS {
        let truncated: String = collapsed.chars().take(ACTIVITY_SUMMARY_MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

// ----------------------------------------------------------------------------
// Shared helpers
// ----------------------------------------------------------------------------

fn validate_params(params: &AgentQueryParams) -> Result<(), ErrorData> {
    if params.session_id.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "AGENT_QUERY_SESSION_ID_EMPTY: session_id must be a non-empty MCP session id or \
             agent-spawn-* id",
        ));
    }
    if params.max_events == 0 || params.max_events > MAX_MAX_EVENTS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "AGENT_QUERY_MAX_EVENTS_INVALID: max_events must be 1..={MAX_MAX_EVENTS}, got {}",
                params.max_events
            ),
        ));
    }
    if params.lookback_ms == 0 || params.lookback_ms > MAX_LOOKBACK_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "AGENT_QUERY_LOOKBACK_INVALID: lookback_ms must be 1..={MAX_LOOKBACK_MS}, got {}",
                params.lookback_ms
            ),
        ));
    }
    if params.deep && (params.deep_timeout_ms == 0 || params.deep_timeout_ms > MAX_DEEP_TIMEOUT_MS) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "AGENT_QUERY_DEEP_TIMEOUT_INVALID: deep_timeout_ms must be 1..={MAX_DEEP_TIMEOUT_MS}, got {}",
                params.deep_timeout_ms
            ),
        ));
    }
    Ok(())
}

fn empty_response(
    now_unix_ms: u64,
    scan: &JournalScan,
    transcript_rows_scanned: usize,
    lookback_ms: u64,
) -> AgentQueryResponse {
    AgentQueryResponse {
        query_ts_unix_ms: now_unix_ms,
        found: false,
        anchor: None,
        spawn_id: None,
        session_id: None,
        agent_kind: None,
        state: None,
        reason_code: None,
        since_unix_ms: None,
        silent_ms: None,
        runaway: false,
        consecutive_identical_tool_calls: 0,
        waiting_for: None,
        current_tool_call: None,
        last_tool_call: None,
        recent_events: Vec::new(),
        turn: None,
        context_window_estimate_tokens: None,
        activity_summary: None,
        task: None,
        cooperative: None,
        scan: ScanReadback {
            lookback_ms,
            events_scanned: scan.events_scanned,
            events_matched: 0,
            transcript_rows_scanned,
        },
        sources: sources_map(),
    }
}

/// Field-group → source-of-truth, satisfying the acceptance requirement that
/// every response field name where its value came from.
fn sources_map() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let lifecycle = "CF_AGENT_EVENTS journal (keys (ts_ns,seq)) reduced by the #898 lifecycle \
                     reducer; the in-memory tracker is a cache of these rows";
    for field in [
        "state",
        "reason_code",
        "since_unix_ms",
        "silent_ms",
        "runaway",
        "consecutive_identical_tool_calls",
        "waiting_for",
        "agent_kind",
    ] {
        map.insert(field.to_owned(), lifecycle.to_owned());
    }
    for field in ["current_tool_call", "last_tool_call", "recent_events"] {
        map.insert(
            field.to_owned(),
            "CF_AGENT_EVENTS journal rows (tool_call_started/finished); keys (ts_ns,seq)".to_owned(),
        );
    }
    for field in [
        "turn",
        "context_window_estimate_tokens",
        "activity_summary",
    ] {
        map.insert(
            field.to_owned(),
            "CF_AGENT_TRANSCRIPTS rows (#900); keys (spawn_id,line_no)".to_owned(),
        );
    }
    map.insert(
        "task".to_owned(),
        "#910 durable agent task queue — NOT YET IMPLEMENTED; reported null, never fabricated"
            .to_owned(),
    );
    map.insert(
        "cooperative".to_owned(),
        "agent mailbox (#908) status_request/status_response rows in CF_KV".to_owned(),
    );
    map
}

/// Smallest key strictly greater than `key`.
fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn unix_time_ms_now() -> u64 {
    unix_time_ns_now() / 1_000_000
}

#[cfg(test)]
mod tests;
