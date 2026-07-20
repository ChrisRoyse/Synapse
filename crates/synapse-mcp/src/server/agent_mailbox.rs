//! Per-session mailbox tools for explicit multi-agent handoff (#795).
//!
//! The mailbox is intentionally a small durable queue over the existing
//! daemon-owned `CF_KV` storage handle. Sends fail if the recipient is not a
//! live MCP session, messages are TTL-bounded, and inbox drains delete only the
//! exact rows returned to the recipient.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;
use synapse_storage::{Db, RevisionGuard, agent_events::agent_event_key, cf};

use super::{
    ErrorData, Json, Parameters, SynapseService, mcp_error,
    session_registry::{SessionRegistryRead, unix_time_ms_now},
    session_tools::validate_session_id,
    tool, tool_router,
};

type MailboxDrainJournalRows = Vec<(Vec<u8>, Vec<u8>)>;

const SCHEMA_VERSION: u32 = 1;
const MESSAGE_PREFIX: &str = "agent-mailbox/v1/recipient_hex/";
const MAILBOX_GLOBAL_STATE_KEY: &str = "agent-mailbox/v2/meta/global_state";
const MAILBOX_STATE_SCHEMA_VERSION: u32 = 1;
const MAILBOX_MAX_CONFLICT_RETRIES: usize = 64;
/// CF_KV key prefix for sender-visible receipt rows (#908). Distinct from the
/// recipient message prefix so receipts never appear in an agent's own inbox.
const RECEIPT_PREFIX: &str = "agent-mailbox/v1/receipt";
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const DRAIN_OUTBOX_PREFIX: &str = "agent-mailbox/v3/drain-outbox/recipient_hex/";
const DRAIN_OUTBOX_SCHEMA_VERSION: u32 = 1;
const DRAIN_OUTBOX_OPERATION_FIELD: &str = "mailbox_drain_operation_id";
const DRAIN_OUTBOX_SCAN_PAGE_ROWS: usize = 256;
const MAX_DRAIN_OUTBOX_TIMESTAMP_CANDIDATES: usize = 4 * 1024;
const DEFAULT_MESSAGE_TTL_MS: u64 = 5 * 60 * 1000;
const MAX_MESSAGE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Read-receipt rows live long enough for an orchestrator to poll for them,
/// independent of the original message's TTL (the message is gone once read).
const RECEIPT_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const DEFAULT_MAX_MESSAGES: usize = 100;
const MAX_MESSAGES_PER_READ: usize = 1000;
const MAX_PAYLOAD_BYTES: usize = 65_536;
const MAX_KIND_CHARS: usize = 128;
const MAX_KIND_FILTER_ENTRIES: usize = 64;
const MAX_BROADCAST_RECIPIENTS: usize = 1024;
const MAX_ARTIFACT_HANDLE_CHARS: usize = 1024;
const MAX_INBOX_ROWS_PER_RECIPIENT: usize = 10_000;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 1000;
const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;

static MAILBOX_DRAIN_RECONCILIATION_LATCH: AtomicBool = AtomicBool::new(false);

fn mailbox_drain_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// The reserved **steering-inbox contract** kind (#908): a well-behaved agent
/// drains `steer`-kind messages between tool calls and splices their payload
/// into context at the next safe point. The cooperative tier of `agent_steer`
/// (#905) delivers through this kind; it is filterable via the `kinds` inbox
/// filter so an agent can poll only its steering channel.
pub(crate) const STEER_KIND: &str = "steer";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendParams {
    /// Live recipient MCP Streamable HTTP session id, the well-known
    /// `orchestrator` alias, or a known stale session id whose live successor
    /// can be resolved from the session registry.
    pub to_session: String,
    /// Caller-defined message kind, such as "handoff", "ready", or "finding".
    pub kind: String,
    /// Opaque JSON payload. It is persisted as-is, bounded to 64 KiB.
    pub payload: Value,
    /// Optional handle to a file/artifact row managed by another tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_handle: Option<String>,
    /// Message retention in milliseconds. Expired messages are removed on
    /// send/read for the addressed recipient.
    #[serde(default = "default_message_ttl_ms")]
    #[schemars(default = "default_message_ttl_ms", range(min = 1, max = 86_400_000))]
    pub ttl_ms: u64,
    /// Request a read receipt: when the recipient drains this message, a
    /// receipt row is written to the sender's receipt box, readable via
    /// `agent_receipts`. Lets an orchestrating agent prove the message was
    /// actually consumed (#908).
    #[serde(default)]
    #[schemars(default)]
    pub request_receipt: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentInboxParams {
    /// Drain deletes returned messages after reading; set false to peek.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub drain: bool,
    /// Maximum non-expired messages to return in enqueue order.
    #[serde(default = "default_max_messages")]
    #[schemars(default = "default_max_messages", range(min = 1, max = 1000))]
    pub max_messages: usize,
    /// Optional server-side kind filter (#908): when non-empty, only messages
    /// whose `kind` is in this set are returned, and a drain deletes only those
    /// matching rows — non-matching messages stay queued. Empty = all kinds.
    /// Pass `["steer"]` to drain only the steering channel.
    #[serde(default)]
    #[schemars(default)]
    pub kinds: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitParams {
    /// Maximum time to wait for the caller's inbox before returning empty.
    #[serde(default = "default_wait_timeout_ms")]
    #[schemars(default = "default_wait_timeout_ms", range(min = 0, max = 60_000))]
    pub timeout_ms: u64,
    /// Drain deletes returned messages after reading; set false to peek.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub drain: bool,
    /// Maximum non-expired messages to return in enqueue order.
    #[serde(default = "default_max_messages")]
    #[schemars(default = "default_max_messages", range(min = 1, max = 1000))]
    pub max_messages: usize,
    /// Optional server-side kind filter, same semantics as `agent_inbox.kinds`.
    #[serde(default)]
    #[schemars(default)]
    pub kinds: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentMailboxMessage {
    pub schema_version: u32,
    pub message_id: String,
    pub row_key: String,
    pub from_session: String,
    pub to_session: String,
    pub kind: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_handle: Option<String>,
    pub sent_at_unix_ms: u64,
    pub ttl_ms: u64,
    pub expires_at_unix_ms: u64,
    pub delivery_attempts: u32,
    /// Sender asked for a read receipt (#908). Persisted so the draining
    /// recipient knows to write one. Defaults false for v1 rows.
    #[serde(default)]
    pub request_receipt: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailboxRowReadback {
    pub cf_name: String,
    pub row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendResponse {
    pub ok: bool,
    pub message_id: String,
    pub from_session: String,
    pub to_session: String,
    pub kind: String,
    pub row_key: String,
    pub sent_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub queue_depth_after: usize,
    pub storage_readback: MailboxRowReadback,
    /// Whether a read receipt was armed on this message (#908).
    pub request_receipt: bool,
}

/// Broadcast addressing selector (#908). Exactly one selector must be active:
/// `all`, a non-empty `agent_kinds`, or a non-empty `sessions`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BroadcastTarget {
    /// Every live MCP session except the sender.
    #[serde(default)]
    #[schemars(default)]
    pub all: bool,
    /// Every live session whose registry `agent_kind` is in this set.
    #[serde(default)]
    #[schemars(default)]
    pub agent_kinds: Vec<String>,
    /// An explicit list of session ids. Unknown/stale sessions are reported as
    /// skipped, not silently dropped.
    #[serde(default)]
    #[schemars(default)]
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendBroadcastParams {
    /// Who to fan out to.
    pub to: BroadcastTarget,
    /// Message kind (e.g. `steer`, `finding`, `stop`).
    pub kind: String,
    /// Opaque JSON payload, persisted as-is, bounded to 64 KiB per recipient.
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_handle: Option<String>,
    #[serde(default = "default_message_ttl_ms")]
    #[schemars(default = "default_message_ttl_ms", range(min = 1, max = 86_400_000))]
    pub ttl_ms: u64,
    /// Arm a read receipt on every fanned-out copy.
    #[serde(default)]
    #[schemars(default)]
    pub request_receipt: bool,
}

/// One recipient's outcome in a broadcast fan-out.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipientOutcome {
    pub to_session: String,
    /// `delivered` when a durable row was written; `skipped` otherwise.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_readback: Option<MailboxRowReadback>,
    /// Why the recipient was skipped (e.g. queue full), when `status=skipped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendBroadcastResponse {
    pub ok: bool,
    pub from_session: String,
    pub kind: String,
    pub sent_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub request_receipt: bool,
    /// Live recipients the selector resolved to (before per-recipient outcome).
    pub resolved_recipients: usize,
    pub delivered_count: usize,
    pub skipped_count: usize,
    pub recipients: Vec<RecipientOutcome>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentReceiptsParams {
    /// Drain deletes returned receipts after reading; set false to peek.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    pub drain: bool,
    #[serde(default = "default_max_messages")]
    #[schemars(default = "default_max_messages", range(min = 1, max = 1000))]
    pub max_receipts: usize,
}

/// One read-receipt row: proof a recipient drained a `request_receipt` message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailboxReceipt {
    pub schema_version: u32,
    pub receipt_id: String,
    pub row_key: String,
    /// The original sender (and receipt-box owner).
    pub from_session: String,
    /// The recipient that read the message.
    pub recipient_session: String,
    pub message_id: String,
    pub message_kind: String,
    /// `read` for now; `delivered` reserved for a future delivery receipt.
    pub status: String,
    pub read_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentReceiptsResponse {
    pub ok: bool,
    pub this_session_id: String,
    pub mode: String,
    pub now_unix_ms: u64,
    pub scanned_rows: usize,
    pub expired_rows_deleted: usize,
    pub returned_count: usize,
    pub deleted_count: usize,
    pub receipts: Vec<MailboxReceipt>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentInboxResponse {
    pub ok: bool,
    pub this_session_id: String,
    pub mode: String,
    pub now_unix_ms: u64,
    pub scanned_rows: usize,
    pub expired_rows_deleted: usize,
    pub returned_count: usize,
    pub deleted_count: usize,
    pub queue_depth_after: usize,
    pub messages: Vec<AgentMailboxMessage>,
    pub readback_rows: Vec<MailboxRowReadback>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentMailboxRepairParams {
    pub recipient_session_id: String,
    /// Operator-observed monotonic lower bound used when global state bytes are
    /// malformed and therefore cannot safely supply their prior value.
    pub minimum_last_enqueue_seq: u64,
    /// Operator-observed monotonic lower bound used when recipient state bytes
    /// are malformed. The repair never writes a generation below this value.
    pub minimum_mutation_generation: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentMailboxRepairResponse {
    pub ok: bool,
    pub recipient_session_id: String,
    pub observed_global_message_rows: usize,
    pub observed_global_max_sequence: u64,
    pub observed_recipient_rows: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_decodable_global_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_decodable_recipient_generation: Option<u64>,
    pub repaired_global_sequence: u64,
    pub repaired_recipient_generation: u64,
    pub repaired_recipient_count: u64,
    pub committed_seq: u64,
    pub global_state_readback: MailboxRowReadback,
    pub recipient_state_readback: MailboxRowReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitResponse {
    pub ok: bool,
    pub waited_ms: u64,
    pub timed_out: bool,
    pub inbox: AgentInboxResponse,
}

struct InboxScan {
    scanned_rows: usize,
    expired_keys: Vec<Vec<u8>>,
    messages: Vec<DecodedMailboxRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MailboxGlobalState {
    schema_version: u32,
    last_enqueue_seq: u64,
    updated_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MailboxRecipientState {
    schema_version: u32,
    recipient_session_id: String,
    physical_row_count: u64,
    mutation_generation: u64,
    updated_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct RevisionedMailboxGlobalState {
    state: MailboxGlobalState,
    revision_sha256: [u8; 32],
}

#[derive(Clone, Debug)]
struct RevisionedMailboxRecipientState {
    state: MailboxRecipientState,
    revision_sha256: [u8; 32],
}

struct StableInboxSnapshot {
    state: RevisionedMailboxRecipientState,
    scan: InboxScan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailboxDrainMessageIntent {
    message_id: String,
    row_key: String,
    value_len_bytes: u64,
    value_sha256: String,
    from_session: String,
    message_kind: String,
    sent_at_unix_ms: u64,
    request_receipt: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailboxDrainOutbox {
    schema_version: u32,
    row_key: String,
    operation_id: String,
    recipient_session_id: String,
    created_at_unix_ms: u64,
    event_ts_ns: u64,
    committed_recipient_generation: u64,
    physical_row_count_after: u64,
    messages: Vec<MailboxDrainMessageIntent>,
    receipts: Vec<MailboxReceipt>,
    records: Vec<synapse_core::AgentEventRecord>,
}

#[derive(Serialize)]
struct MailboxDrainOutboxIdentity<'a> {
    schema_version: u32,
    recipient_session_id: &'a str,
    created_at_unix_ms: u64,
    event_ts_ns: u64,
    committed_recipient_generation: u64,
    physical_row_count_after: u64,
    messages: &'a [MailboxDrainMessageIntent],
    receipts: &'a [MailboxReceipt],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MailboxDrainJournalState {
    Absent,
    Exact,
}

enum MailboxDrainCommitOutcome {
    Conflict,
    Committed {
        deleted_count: usize,
        outbox: MailboxDrainOutbox,
    },
}

struct MailboxEnqueueCommit {
    message: AgentMailboxMessage,
    storage_readback: MailboxRowReadback,
    queue_depth_before: usize,
    queue_depth_after: usize,
    expired_rows_deleted_before: usize,
}

struct MailboxEnqueueRequest<'a> {
    from_session: &'a str,
    to_session: &'a str,
    kind: &'a str,
    payload: &'a Value,
    artifact_handle: Option<&'a str>,
    ttl_ms: u64,
    request_receipt: bool,
}

enum MailboxEnqueueOutcome {
    Committed(Box<MailboxEnqueueCommit>),
    Full {
        queue_depth: usize,
        expired_rows_deleted_before: usize,
    },
}

#[derive(Clone, Debug)]
struct MailboxRecipientResolution {
    requested_to_session: String,
    resolved_to_session: String,
    resolution_source: String,
    recipient: SessionRegistryRead,
    replaced_recipient: Option<SessionRegistryRead>,
}

#[derive(Clone)]
struct DecodedMailboxRow {
    key: Vec<u8>,
    encoded: Vec<u8>,
    message: AgentMailboxMessage,
}

#[tool_router(router = agent_mailbox_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Send a bounded durable JSON message to a live MCP peer. `to_session` accepts an exact live MCP session id, the stable `orchestrator` alias, or a known stale session id that resolves to a live same-client successor after MCP reconnect. Fails with RECIPIENT_UNKNOWN when no live physical recipient can be proven instead of queueing to nowhere. The message is persisted under CF_KV for the resolved session id and returned with an exact row readback."
    )]
    pub async fn agent_send(
        &self,
        params: Parameters<AgentSendParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSendResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_send",
            "tool.invocation kind=agent_send"
        );
        let from_session = require_mailbox_session_id("agent_send", &request_context)?;
        let response = self.agent_send_impl(params.0, &from_session)?;
        self.mailbox_notify_handle().notify_waiters();
        Ok(Json(response))
    }

    #[tool(
        description = "Read this MCP session's durable agent mailbox in enqueue order. By default this drains returned rows from CF_KV; set drain=false to peek without deleting."
    )]
    pub async fn agent_inbox(
        &self,
        params: Parameters<AgentInboxParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentInboxResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_inbox",
            "tool.invocation kind=agent_inbox"
        );
        let session_id = require_mailbox_session_id("agent_inbox", &request_context)?;
        self.agent_inbox_impl(params.0, &session_id).map(Json)
    }

    #[tool(
        description = "Wait up to timeout_ms for this MCP session's durable mailbox to receive a message, then return the same inbox shape. Timeout is hard-bounded and returns an empty inbox rather than blocking indefinitely."
    )]
    pub async fn agent_wait(
        &self,
        params: Parameters<AgentWaitParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentWaitResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_wait",
            "tool.invocation kind=agent_wait"
        );
        let session_id = require_mailbox_session_id("agent_wait", &request_context)?;
        self.agent_wait_impl(params.0, &session_id).await.map(Json)
    }

    #[tool(
        description = "Broadcast one durable message to many live MCP sessions at once (#908): address `to: {all}` for every live peer, `to: {agent_kinds: [..]}` to filter by registry agent kind, or `to: {sessions: [..]}` for an explicit list. Fans out one durable CF_KV row per recipient (the sender is always excluded), returning a per-recipient delivered/skipped outcome. Reserve kind=\"steer\" for the steering-inbox contract."
    )]
    pub async fn agent_send_broadcast(
        &self,
        params: Parameters<AgentSendBroadcastParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSendBroadcastResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_send_broadcast",
            "tool.invocation kind=agent_send_broadcast"
        );
        let from_session = require_mailbox_session_id("agent_send_broadcast", &request_context)?;
        let response = self.agent_send_broadcast_impl(params.0, &from_session)?;
        self.mailbox_notify_handle().notify_waiters();
        Ok(Json(response))
    }

    #[tool(
        description = "Read this session's durable read-receipt box: proof that recipients drained the messages this session sent with request_receipt=true (#908). By default this drains returned receipt rows from CF_KV; set drain=false to peek."
    )]
    pub async fn agent_receipts(
        &self,
        params: Parameters<AgentReceiptsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentReceiptsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_receipts",
            "tool.invocation kind=agent_receipts"
        );
        let session_id = require_mailbox_session_id("agent_receipts", &request_context)?;
        self.agent_receipts_impl(params.0, &session_id).map(Json)
    }
}

impl SynapseService {
    pub(crate) fn dashboard_agent_send(
        &self,
        to_session: String,
        kind: String,
        payload: Value,
        request_receipt: bool,
    ) -> Result<Value, ErrorData> {
        let response = self.agent_send_impl(
            AgentSendParams {
                to_session,
                kind,
                payload,
                artifact_handle: None,
                ttl_ms: default_message_ttl_ms(),
                request_receipt,
            },
            "dashboard-context",
        )?;
        self.mailbox_notify_handle().notify_waiters();
        dashboard_json_readback(response)
    }

    pub(crate) fn dashboard_agent_broadcast(
        &self,
        selector: String,
        agent_kinds: Vec<String>,
        sessions: Vec<String>,
        kind: String,
        payload: Value,
        ttl_ms: Option<u64>,
        request_receipt: bool,
    ) -> Result<Value, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_AGENT_BROADCAST_REQUESTED",
            kind = "agent_send_broadcast",
            selector = %selector,
            agent_kind_count = agent_kinds.len(),
            session_count = sessions.len(),
            "dashboard.invocation kind=agent_send_broadcast"
        );
        let selector = selector.trim().to_ascii_lowercase();
        let target = match selector.as_str() {
            "all" => BroadcastTarget {
                all: true,
                ..BroadcastTarget::default()
            },
            "agent_kinds" => BroadcastTarget {
                agent_kinds,
                ..BroadcastTarget::default()
            },
            "sessions" => BroadcastTarget {
                sessions,
                ..BroadcastTarget::default()
            },
            other => {
                return Err(params_error(format!(
                    "dashboard agent broadcast selector {other:?} is not one of all|agent_kinds|sessions"
                )));
            }
        };
        let now_unix_ms = unix_time_ms_now();
        let live = self.live_spawned_agent_session_reads(now_unix_ms)?;
        let response = self.agent_send_broadcast_impl_at_with_live(
            AgentSendBroadcastParams {
                to: target,
                kind,
                payload,
                artifact_handle: None,
                ttl_ms: ttl_ms.unwrap_or_else(default_message_ttl_ms),
                request_receipt,
            },
            "dashboard-fleet",
            now_unix_ms,
            live,
        )?;
        self.mailbox_notify_handle().notify_waiters();
        dashboard_json_readback(response)
    }

    pub(crate) fn dashboard_agent_inbox_snapshot(
        &self,
        session_id: &str,
        max_messages: usize,
        kinds: Vec<String>,
    ) -> Result<Value, ErrorData> {
        dashboard_json_readback(self.agent_inbox_impl(
            AgentInboxParams {
                drain: false,
                max_messages,
                kinds,
            },
            session_id,
        )?)
    }

    pub(super) fn agent_send_impl(
        &self,
        params: AgentSendParams,
        from_session: &str,
    ) -> Result<AgentSendResponse, ErrorData> {
        self.agent_send_impl_at(params, from_session, unix_time_ms_now())
    }

    fn agent_send_impl_at(
        &self,
        params: AgentSendParams,
        from_session: &str,
        now_unix_ms: u64,
    ) -> Result<AgentSendResponse, ErrorData> {
        validate_session_id(from_session)?;
        validate_send_params(&params)?;
        let db = self.mailbox_db()?;
        let resolution = self.recipient_live_read(from_session, &params.to_session, now_unix_ms)?;
        let to_session = resolution.resolved_to_session.clone();
        let observed = stable_inbox_snapshot(&db, &to_session, now_unix_ms)?;
        let observed_live_depth = observed.scan.messages.len();
        let command_payload = json!({
            "requested_to_session": &resolution.requested_to_session,
            "to_session": &to_session,
            "recipient_resolution_source": &resolution.resolution_source,
            "kind": &params.kind,
            "payload": &params.payload,
            "artifact_handle": &params.artifact_handle,
            "ttl_ms": params.ttl_ms,
        });
        let command_before = json!({
            "source_of_truth": cf::CF_KV,
            "recipient_lifecycle": &resolution.recipient.lifecycle,
            "recipient_session_id": &resolution.recipient.session_id,
            "replaced_recipient": &resolution.replaced_recipient,
            "observed_live_queue_depth_before": observed_live_depth,
            "observed_physical_rows_before": observed.scan.scanned_rows,
            "observed_expired_rows_before": observed.scan.expired_keys.len(),
            "recipient_counter_generation": observed.state.state.mutation_generation,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "agent_send",
            "steer",
            Some(from_session.to_owned()),
            Some(to_session.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        let enqueue = enqueue_mailbox_message(
            &db,
            MailboxEnqueueRequest {
                from_session,
                to_session: &to_session,
                kind: &params.kind,
                payload: &params.payload,
                artifact_handle: params.artifact_handle.as_deref(),
                ttl_ms: params.ttl_ms,
                request_receipt: params.request_receipt,
            },
            now_unix_ms,
        );
        let enqueue = match enqueue {
            Ok(enqueue) => enqueue,
            Err(error) => {
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "agent_send",
                        "steer",
                        Some(from_session.to_owned()),
                        Some(to_session),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": cf::CF_KV,
                            "mutation_commit_proven": false,
                            "remediation": "inspect the exact mailbox global/recipient state and message rows named by the structured error before retrying",
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
        let enqueue = match enqueue {
            MailboxEnqueueOutcome::Committed(enqueue) => *enqueue,
            MailboxEnqueueOutcome::Full {
                queue_depth,
                expired_rows_deleted_before,
            } => {
                let error = mailbox_full_error(from_session, &to_session, queue_depth);
                self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        "agent_send",
                        "steer",
                        Some(from_session.to_owned()),
                        Some(to_session),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": cf::CF_KV,
                            "queue_depth": queue_depth,
                            "max_rows": MAX_INBOX_ROWS_PER_RECIPIENT,
                            "expired_rows_deleted_before": expired_rows_deleted_before,
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
        let message = enqueue.message;
        let message_id = message.message_id.clone();
        let row_key = message.row_key.clone();
        let storage_readback = enqueue.storage_readback;
        let queue_depth_before = enqueue.queue_depth_before;
        let queue_depth_after = enqueue.queue_depth_after;
        let expired_rows_deleted_before = enqueue.expired_rows_deleted_before;

        // Journal the delivery fact (#897). The mailbox row is already
        // committed, so a journal failure is surfaced with that context.
        let mut journal_record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::MessageSent,
        );
        journal_record.session_id = Some(from_session.to_owned());
        journal_record.attributes.conversation_id = Some(from_session.to_owned());
        journal_record.payload = json!({
            "requested_to_session": &resolution.requested_to_session,
            "to_session": &to_session,
            "recipient_resolution_source": &resolution.resolution_source,
            "message_id": &message_id,
            "message_kind": &message.kind,
            "payload_bytes": storage_readback.value_len_bytes,
            "value_sha256": &storage_readback.value_sha256,
            "expires_at_unix_ms": message.expires_at_unix_ms,
            "queue_depth_before": queue_depth_before,
            "queue_depth_after": queue_depth_after,
        });
        if let Err(error) = super::agent_events::record_agent_event(&db, &journal_record) {
            let tool_error =
                super::agent_events::agent_event_tool_error("agent_send", &error, true);
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "agent_send",
                    "steer",
                    Some(from_session.to_owned()),
                    Some(message.to_session),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": cf::CF_KV,
                        "message_id": &message_id,
                        "row_key": &row_key,
                        "queue_depth_after": queue_depth_after,
                        "storage_readback": &storage_readback,
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&tool_error),
                ),
            )?;
            return Err(tool_error);
        }

        tracing::info!(
            code = "AGENT_MAILBOX_SEND_COMMITTED",
            from_session,
            requested_to_session = %resolution.requested_to_session,
            to_session = %to_session,
            recipient_resolution_source = %resolution.resolution_source,
            recipient_lifecycle = %resolution.recipient.lifecycle,
            message_id,
            row_key,
            kind = %message.kind,
            is_steer = message.kind == STEER_KIND,
            request_receipt = message.request_receipt,
            queue_depth_before,
            queue_depth_after,
            expired_rows_deleted_before,
            value_sha256 = %storage_readback.value_sha256,
            "readback=agent_mailbox edge=send_committed"
        );

        let response = AgentSendResponse {
            ok: true,
            message_id,
            from_session: from_session.to_owned(),
            to_session,
            kind: message.kind,
            row_key,
            sent_at_unix_ms: now_unix_ms,
            expires_at_unix_ms: message.expires_at_unix_ms,
            queue_depth_after,
            storage_readback,
            request_receipt: message.request_receipt,
        };
        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "agent_send",
            "steer",
            Some(from_session.to_owned()),
            Some(response.to_session.clone()),
            command_payload,
            command_before,
            json!({
                "source_of_truth": cf::CF_KV,
                "message_id": &response.message_id,
                "row_key": &response.row_key,
                "queue_depth_before": queue_depth_before,
                "queue_depth_after": response.queue_depth_after,
                "storage_readback": &response.storage_readback,
                "request_receipt": response.request_receipt,
            }),
            "ok",
        ))?;
        Ok(response)
    }

    pub(super) fn agent_inbox_impl(
        &self,
        params: AgentInboxParams,
        session_id: &str,
    ) -> Result<AgentInboxResponse, ErrorData> {
        self.agent_inbox_impl_at(params, session_id, unix_time_ms_now())
    }

    fn agent_inbox_impl_at(
        &self,
        params: AgentInboxParams,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<AgentInboxResponse, ErrorData> {
        validate_inbox_params(params.max_messages)?;
        validate_kind_filter(&params.kinds)?;
        validate_session_id(session_id)?;
        let db = self.mailbox_db()?;
        let _drain_guard = mailbox_drain_lock().lock().map_err(|poisoned| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "AGENT_MAILBOX_DRAIN_LOCK_POISONED: recipient={session_id:?}: {poisoned}; \
                     remediation=restart the daemon and reconcile the recipient's durable drain \
                     outbox against CF_AGENT_EVENTS before retrying"
                ),
            )
        })?;
        if MAILBOX_DRAIN_RECONCILIATION_LATCH.load(Ordering::Acquire) {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "AGENT_MAILBOX_DRAIN_RECONCILIATION_REQUIRED: recipient={session_id:?} a \
                     prior drain event or acknowledgement commit returned an unresolved error \
                     in this process; remediation=stop the daemon, reopen Calyx, and reconcile \
                     the persisted drain outbox against CF_AGENT_EVENTS before retrying"
                ),
            ));
        }
        // Sequence migration must complete before any legacy message can be
        // deleted, otherwise the highest historical allocation could vanish
        // while the durable high-watermark is being derived.
        initialize_mailbox_global_state(&db, now_unix_ms)?;
        relay_pending_mailbox_drain_outbox(&db, session_id)?;
        let mut expired_rows_deleted =
            cleanup_expired_recipient_rows(&db, session_id, now_unix_ms)?;

        for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
            let snapshot = stable_inbox_snapshot(&db, session_id, now_unix_ms)?;
            if !snapshot.scan.expired_keys.is_empty() {
                expired_rows_deleted = expired_rows_deleted
                    .checked_add(cleanup_expired_recipient_rows(
                        &db,
                        session_id,
                        now_unix_ms,
                    )?)
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            "expired mailbox cleanup count overflows usize",
                        )
                    })?;
                continue;
            }
            let guarded_state = snapshot.state;
            let mut scan = snapshot.scan;

            // Server-side kind filter (#908): keep only matching kinds, so a
            // drain never claims messages the caller did not ask for.
            if !params.kinds.is_empty() {
                scan.messages
                    .retain(|row| params.kinds.iter().any(|kind| kind == &row.message.kind));
            }
            if scan.messages.len() > params.max_messages {
                scan.messages.truncate(params.max_messages);
            }
            let readback_rows = scan
                .messages
                .iter()
                .map(|row| MailboxRowReadback {
                    cf_name: cf::CF_KV.to_owned(),
                    row_key: row.message.row_key.clone(),
                    value_len_bytes: row.encoded.len() as u64,
                    value_sha256: hash_bytes(&row.encoded),
                })
                .collect::<Vec<_>>();

            let deleted_count = if params.drain && !scan.messages.is_empty() {
                match commit_mailbox_drain(
                    &db,
                    session_id,
                    &guarded_state,
                    &scan.messages,
                    now_unix_ms,
                )? {
                    MailboxDrainCommitOutcome::Conflict => {
                        tracing::warn!(
                            code = "AGENT_MAILBOX_DRAIN_REVISION_CONFLICT",
                            session_id,
                            retry,
                            "atomic drain intent conflicted; rereading mailbox state and rows"
                        );
                        continue;
                    }
                    MailboxDrainCommitOutcome::Committed {
                        deleted_count,
                        outbox,
                    } => {
                        relay_mailbox_drain_outbox(&db, &outbox)?;
                        deleted_count
                    }
                }
            } else {
                0
            };

            let queue_depth_after = usize::try_from(
                stable_inbox_snapshot(&db, session_id, now_unix_ms)?
                    .state
                    .state
                    .physical_row_count,
            )
            .map_err(|_error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("mailbox counter overflows usize for {session_id:?}"),
                )
            })?;
            let messages = scan
                .messages
                .into_iter()
                .map(|mut row| {
                    row.message.delivery_attempts = row.message.delivery_attempts.saturating_add(1);
                    row.message
                })
                .collect::<Vec<_>>();
            let response = AgentInboxResponse {
                ok: true,
                this_session_id: session_id.to_owned(),
                mode: if params.drain { "drain" } else { "peek" }.to_owned(),
                now_unix_ms,
                scanned_rows: scan.scanned_rows,
                expired_rows_deleted,
                returned_count: messages.len(),
                deleted_count,
                queue_depth_after,
                messages,
                readback_rows,
            };
            tracing::info!(
                code = "AGENT_MAILBOX_INBOX_READ",
                session_id,
                mode = %response.mode,
                returned_count = response.returned_count,
                expired_rows_deleted = response.expired_rows_deleted,
                deleted_count = response.deleted_count,
                queue_depth_after = response.queue_depth_after,
                "readback=agent_mailbox edge=inbox_read"
            );
            return Ok(response);
        }

        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_MAILBOX_DRAIN_CONTENTION: recipient {session_id:?} changed during \
                 {MAILBOX_MAX_CONFLICT_RETRIES} consecutive guarded drain attempts"
            ),
        ))
    }

    async fn agent_wait_impl(
        &self,
        params: AgentWaitParams,
        session_id: &str,
    ) -> Result<AgentWaitResponse, ErrorData> {
        validate_wait_params(&params)?;
        validate_session_id(session_id)?;
        let started = Instant::now();
        let timeout = Duration::from_millis(params.timeout_ms);
        let notify = self.mailbox_notify_handle();
        loop {
            let notified = notify.notified();
            let inbox = self.agent_inbox_impl(AgentInboxParams::from(&params), session_id)?;
            if inbox.returned_count > 0 {
                return Ok(wait_response(started, false, inbox));
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Ok(wait_response(started, true, inbox));
            }
            let remaining = timeout.saturating_sub(elapsed);
            if remaining.is_zero() {
                return Ok(wait_response(started, true, inbox));
            }
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }

    fn mailbox_db(&self) -> Result<Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent mailbox storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn recipient_live_read(
        &self,
        from_session: &str,
        to_session: &str,
        now_unix_ms: u64,
    ) -> Result<MailboxRecipientResolution, ErrorData> {
        validate_session_id(to_session)?;
        let reads = {
            let guard = self.session_registry_ref().lock().map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "session registry lock poisoned while validating mailbox recipient",
                )
            })?;
            guard.reads(now_unix_ms)
        };

        if let Some(read) = reads
            .iter()
            .find(|entry| entry.session_id == to_session)
            .cloned()
        {
            if read.lifecycle == "live" {
                return Ok(MailboxRecipientResolution {
                    requested_to_session: to_session.to_owned(),
                    resolved_to_session: read.session_id.clone(),
                    resolution_source: "exact_live_session".to_owned(),
                    recipient: read,
                    replaced_recipient: None,
                });
            }
            if let Some(successor) = successor_for_rotated_session(&reads, &read) {
                return Ok(MailboxRecipientResolution {
                    requested_to_session: to_session.to_owned(),
                    resolved_to_session: successor.session_id.clone(),
                    resolution_source: "successor_same_client_identity".to_owned(),
                    recipient: successor,
                    replaced_recipient: Some(read),
                });
            }
            return Err(recipient_unknown_error(
                from_session,
                to_session,
                Some(&read),
            ));
        }

        if is_orchestrator_alias(to_session) {
            if let Some(read) = orchestrator_alias_session(&reads, from_session) {
                return Ok(MailboxRecipientResolution {
                    requested_to_session: to_session.to_owned(),
                    resolved_to_session: read.session_id.clone(),
                    resolution_source: "well_known_orchestrator_alias".to_owned(),
                    recipient: read,
                    replaced_recipient: None,
                });
            }
            return Err(recipient_unknown_error(from_session, to_session, None));
        }

        Err(recipient_unknown_error(from_session, to_session, None))
    }

    /// Live MCP sessions other than `exclude_session`, as `(session_id,
    /// agent_kind)` pairs, read from the session registry.
    fn live_session_reads(
        &self,
        exclude_session: &str,
        now_unix_ms: u64,
    ) -> Result<Vec<(String, String)>, ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while resolving broadcast recipients",
            )
        })?;
        let live = guard
            .reads(now_unix_ms)
            .into_iter()
            .filter(|entry| entry.lifecycle == "live" && entry.session_id != exclude_session)
            .map(|entry| (entry.session_id, entry.agent_kind))
            .collect::<Vec<_>>();
        drop(guard);
        Ok(live)
    }

    /// Live spawned-agent MCP sessions, as `(session_id, agent_kind)` pairs,
    /// read from the session registry. Dashboard fleet controls use this
    /// narrower SoT so "all live agents" cannot fan out to the orchestrator
    /// session or stale non-fleet MCP sessions.
    fn live_spawned_agent_session_reads(
        &self,
        now_unix_ms: u64,
    ) -> Result<Vec<(String, String)>, ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while resolving dashboard fleet recipients",
            )
        })?;
        let live = guard
            .reads(now_unix_ms)
            .into_iter()
            .filter(|entry| entry.lifecycle == "live" && entry.spawned_agent.is_some())
            .map(|entry| (entry.session_id, entry.agent_kind))
            .collect::<Vec<_>>();
        drop(guard);
        Ok(live)
    }

    fn agent_send_broadcast_impl(
        &self,
        params: AgentSendBroadcastParams,
        from_session: &str,
    ) -> Result<AgentSendBroadcastResponse, ErrorData> {
        let now_unix_ms = unix_time_ms_now();
        let live = self.live_session_reads(from_session, now_unix_ms)?;
        self.agent_send_broadcast_impl_at_with_live(params, from_session, now_unix_ms, live)
    }

    fn agent_send_broadcast_impl_at_with_live(
        &self,
        params: AgentSendBroadcastParams,
        from_session: &str,
        now_unix_ms: u64,
        live: Vec<(String, String)>,
    ) -> Result<AgentSendBroadcastResponse, ErrorData> {
        validate_session_id(from_session)?;
        validate_broadcast_target(&params.to)?;
        validate_kind(&params.kind)?;
        validate_ttl_ms(params.ttl_ms)?;
        validate_payload_size(&params.payload)?;
        if let Some(artifact_handle) = &params.artifact_handle {
            validate_artifact_handle(artifact_handle)?;
        }

        let mut outcomes = Vec::new();
        let mut skipped_count = 0_usize;

        // Resolve the recipient set from the caller-supplied live read model,
        // applying the selector. Explicit non-live recipients stay visible as
        // skipped rows in the response/audit instead of disappearing.
        let recipients: Vec<String> = if params.to.all {
            live.into_iter().map(|(session, _kind)| session).collect()
        } else if !params.to.agent_kinds.is_empty() {
            live.into_iter()
                .filter(|(_session, kind)| params.to.agent_kinds.iter().any(|k| k == kind))
                .map(|(session, _kind)| session)
                .collect()
        } else {
            let live_set: std::collections::BTreeSet<String> =
                live.into_iter().map(|(session, _kind)| session).collect();
            let mut seen = std::collections::BTreeSet::new();
            let mut explicit = Vec::new();
            for session in &params.to.sessions {
                if !seen.insert(session.clone()) {
                    skipped_count += 1;
                    outcomes.push(skipped_recipient(
                        session.clone(),
                        "duplicate explicit broadcast recipient",
                    ));
                    continue;
                }
                if session == from_session {
                    skipped_count += 1;
                    outcomes.push(skipped_recipient(
                        session.clone(),
                        "broadcast sender is excluded from recipients",
                    ));
                    continue;
                }
                if live_set.contains(session) {
                    explicit.push(session.clone());
                } else {
                    skipped_count += 1;
                    outcomes.push(skipped_recipient(
                        session.clone(),
                        "explicit broadcast recipient is not a live MCP session",
                    ));
                }
            }
            explicit
        };

        if recipients.len() > MAX_BROADCAST_RECIPIENTS {
            return Err(params_error(format!(
                "agent_send_broadcast resolved {} recipients, over the {MAX_BROADCAST_RECIPIENTS} cap; \
                 narrow the selector",
                recipients.len()
            )));
        }

        let resolved_recipients = recipients.len();
        let expires_at_unix_ms = now_unix_ms.saturating_add(params.ttl_ms);
        let db = self.mailbox_db()?;

        let target_selector = if params.to.all {
            json!({ "all": true })
        } else if !params.to.agent_kinds.is_empty() {
            json!({ "agent_kinds": &params.to.agent_kinds })
        } else {
            json!({ "sessions": &params.to.sessions })
        };
        let command_payload = json!({
            "to": target_selector,
            "kind": params.kind.trim(),
            "payload": &params.payload,
            "artifact_handle": &params.artifact_handle,
            "ttl_ms": params.ttl_ms,
            "request_receipt": params.request_receipt,
        });
        let command_before = json!({
            "source_of_truth": cf::CF_KV,
            "resolved_recipients": resolved_recipients,
            "expires_at_unix_ms": expires_at_unix_ms,
            "capacity_source_of_truth": "per-recipient durable counter plus physical row audit",
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "agent_send_broadcast",
            "broadcast",
            Some(from_session.to_owned()),
            None,
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;

        outcomes.reserve(recipients.len());
        let mut delivered_count = 0_usize;
        for to_session in recipients {
            let enqueue = match enqueue_mailbox_message(
                &db,
                MailboxEnqueueRequest {
                    from_session,
                    to_session: &to_session,
                    kind: &params.kind,
                    payload: &params.payload,
                    artifact_handle: params.artifact_handle.as_deref(),
                    ttl_ms: params.ttl_ms,
                    request_receipt: params.request_receipt,
                },
                now_unix_ms,
            ) {
                Ok(enqueue) => enqueue,
                Err(error) => {
                    self.command_audit_final(
                        super::command_audit::CommandAuditInput::mcp(
                            "agent_send_broadcast",
                            "broadcast",
                            Some(from_session.to_owned()),
                            None,
                            command_payload,
                            command_before,
                            json!({
                                "source_of_truth": cf::CF_KV,
                                "to_session": &to_session,
                                "delivered_count": delivered_count,
                                "skipped_count": skipped_count,
                                "partial_recipients": outcomes,
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
            let enqueue = match enqueue {
                MailboxEnqueueOutcome::Committed(enqueue) => *enqueue,
                MailboxEnqueueOutcome::Full {
                    queue_depth,
                    expired_rows_deleted_before,
                } => {
                    skipped_count += 1;
                    outcomes.push(RecipientOutcome {
                        to_session,
                        status: "skipped".to_owned(),
                        message_id: None,
                        row_key: None,
                        storage_readback: None,
                        skip_reason: Some(format!(
                            "recipient mailbox full ({queue_depth} rows); \
                             expired_rows_deleted_before={expired_rows_deleted_before}"
                        )),
                    });
                    continue;
                }
            };
            delivered_count += 1;
            outcomes.push(RecipientOutcome {
                to_session,
                status: "delivered".to_owned(),
                message_id: Some(enqueue.message.message_id),
                row_key: Some(enqueue.message.row_key),
                storage_readback: Some(enqueue.storage_readback),
                skip_reason: None,
            });
        }

        tracing::info!(
            code = "AGENT_MAILBOX_BROADCAST_COMMITTED",
            from_session,
            kind = %params.kind,
            resolved_recipients,
            delivered_count,
            skipped_count,
            "readback=agent_mailbox edge=broadcast_committed"
        );

        let response = AgentSendBroadcastResponse {
            ok: true,
            from_session: from_session.to_owned(),
            kind: params.kind.trim().to_owned(),
            sent_at_unix_ms: now_unix_ms,
            expires_at_unix_ms,
            request_receipt: params.request_receipt,
            resolved_recipients,
            delivered_count,
            skipped_count,
            recipients: outcomes,
        };

        let mut journal_record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::MessageSent,
        );
        journal_record.session_id = Some(from_session.to_owned());
        journal_record.attributes.conversation_id = Some(from_session.to_owned());
        journal_record.payload = json!({
            "broadcast": true,
            "message_kind": &response.kind,
            "resolved_recipients": response.resolved_recipients,
            "delivered_count": response.delivered_count,
            "skipped_count": response.skipped_count,
            "expires_at_unix_ms": response.expires_at_unix_ms,
            "request_receipt": response.request_receipt,
        });
        if let Err(error) = super::agent_events::record_agent_event(&db, &journal_record) {
            let tool_error =
                super::agent_events::agent_event_tool_error("agent_send_broadcast", &error, true);
            self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "agent_send_broadcast",
                    "broadcast",
                    Some(from_session.to_owned()),
                    None,
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": cf::CF_KV,
                        "resolved_recipients": response.resolved_recipients,
                        "delivered_count": response.delivered_count,
                        "skipped_count": response.skipped_count,
                        "recipients": &response.recipients,
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&tool_error),
                ),
            )?;
            return Err(tool_error);
        }

        self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
            "agent_send_broadcast",
            "broadcast",
            Some(from_session.to_owned()),
            None,
            command_payload,
            command_before,
            json!({
                "source_of_truth": cf::CF_KV,
                "resolved_recipients": response.resolved_recipients,
                "delivered_count": response.delivered_count,
                "skipped_count": response.skipped_count,
                "recipients": &response.recipients,
            }),
            "ok",
        ))?;

        Ok(response)
    }

    fn agent_receipts_impl(
        &self,
        params: AgentReceiptsParams,
        session_id: &str,
    ) -> Result<AgentReceiptsResponse, ErrorData> {
        self.agent_receipts_impl_at(params, session_id, unix_time_ms_now())
    }

    fn agent_receipts_impl_at(
        &self,
        params: AgentReceiptsParams,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<AgentReceiptsResponse, ErrorData> {
        if params.max_receipts == 0 || params.max_receipts > MAX_MESSAGES_PER_READ {
            return Err(params_error(format!(
                "agent_receipts max_receipts must be between 1 and {MAX_MESSAGES_PER_READ}"
            )));
        }
        validate_session_id(session_id)?;
        let db = self.mailbox_db()?;
        let (mut receipts, expired_keys, scanned_rows) =
            scan_receipts(&db, session_id, now_unix_ms)?;
        if !expired_keys.is_empty() {
            db.delete_batch(cf::CF_KV, expired_keys.clone())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("delete expired receipt rows for {session_id}: {error}"),
                    )
                })?;
        }
        if receipts.len() > params.max_receipts {
            receipts.truncate(params.max_receipts);
        }
        let delete_keys: Vec<Vec<u8>> = if params.drain {
            receipts
                .iter()
                .map(|receipt| receipt.row_key.as_bytes().to_vec())
                .collect()
        } else {
            Vec::new()
        };
        if !delete_keys.is_empty() {
            db.delete_batch(cf::CF_KV, delete_keys.clone())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("delete drained receipt rows for {session_id}: {error}"),
                    )
                })?;
        }
        Ok(AgentReceiptsResponse {
            ok: true,
            this_session_id: session_id.to_owned(),
            mode: if params.drain { "drain" } else { "peek" }.to_owned(),
            now_unix_ms,
            scanned_rows,
            expired_rows_deleted: expired_keys.len(),
            returned_count: receipts.len(),
            deleted_count: delete_keys.len(),
            receipts,
        })
    }

    pub(crate) fn mailbox_repair_state_impl(
        &self,
        params: AgentMailboxRepairParams,
    ) -> Result<AgentMailboxRepairResponse, ErrorData> {
        validate_session_id(&params.recipient_session_id)?;
        if params.reason.trim().is_empty() {
            return Err(params_error(
                "mailbox state repair requires a non-empty audit reason",
            ));
        }
        if params.reason.chars().count() > MAX_ARTIFACT_HANDLE_CHARS {
            return Err(params_error(format!(
                "mailbox state repair reason must be <= {MAX_ARTIFACT_HANDLE_CHARS} characters"
            )));
        }
        let db = self.mailbox_db()?;
        let now_unix_ms = unix_time_ms_now();
        let recipient_state_key = mailbox_recipient_state_key(&params.recipient_session_id);
        for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
            let raw_global = db
                .get_cf_revisioned(cf::CF_KV, MAILBOX_GLOBAL_STATE_KEY.as_bytes())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("read raw mailbox global state for repair: {error}"),
                    )
                })?;
            let raw_recipient = db
                .get_cf_revisioned(cf::CF_KV, recipient_state_key.as_bytes())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("read raw mailbox recipient state for repair: {error}"),
                    )
                })?;
            let previous_global = raw_global
                .as_ref()
                .and_then(|row| row.value.as_deref())
                .and_then(|value| synapse_storage::decode_json::<MailboxGlobalState>(value).ok())
                .filter(|state| state.schema_version == MAILBOX_STATE_SCHEMA_VERSION);
            let previous_recipient = raw_recipient
                .as_ref()
                .and_then(|row| row.value.as_deref())
                .and_then(|value| synapse_storage::decode_json::<MailboxRecipientState>(value).ok())
                .filter(|state| {
                    state.schema_version == MAILBOX_STATE_SCHEMA_VERSION
                        && state.recipient_session_id == params.recipient_session_id
                });
            if raw_global.is_some()
                && previous_global.is_none()
                && params.minimum_last_enqueue_seq == 0
            {
                return Err(params_error(
                    "mailbox repair found undecodable global state; \
                     minimum_last_enqueue_seq must be the non-zero last known-good physical \
                     sequence so repair cannot reuse an unknown allocation",
                ));
            }
            if raw_recipient.is_some()
                && previous_recipient.is_none()
                && params.minimum_mutation_generation == 0
            {
                return Err(params_error(
                    "mailbox repair found undecodable recipient state; \
                     minimum_mutation_generation must be the non-zero last known-good physical \
                     generation so repair cannot introduce an ABA revision",
                ));
            }

            let all_rows = db
                .scan_cf_prefix(cf::CF_KV, MESSAGE_PREFIX.as_bytes())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("scan physical mailbox rows for explicit state repair: {error}"),
                    )
                })?;
            let observed_global_message_rows = all_rows.len();
            let mut observed_global_max_sequence = 0_u64;
            for (key, encoded) in &all_rows {
                decode_mailbox_row(key, encoded)?;
                observed_global_max_sequence =
                    observed_global_max_sequence.max(mailbox_sequence_from_row_key(key)?);
            }
            let recipient_scan = scan_inbox(&db, &params.recipient_session_id, now_unix_ms)?;
            let repaired_recipient_count =
                u64::try_from(recipient_scan.scanned_rows).map_err(|_error| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        "recipient mailbox row count overflows u64 during repair",
                    )
                })?;
            let previous_decodable_global_sequence =
                previous_global.as_ref().map(|state| state.last_enqueue_seq);
            let previous_decodable_recipient_generation = previous_recipient
                .as_ref()
                .map(|state| state.mutation_generation);
            let repaired_global_sequence = params
                .minimum_last_enqueue_seq
                .max(observed_global_max_sequence)
                .max(previous_decodable_global_sequence.unwrap_or(0));
            let generation_after_previous = previous_decodable_recipient_generation
                .map(|generation| {
                    generation.checked_add(1).ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            "mailbox recipient mutation generation is exhausted at u64::MAX",
                        )
                    })
                })
                .transpose()?
                .unwrap_or(0);
            let repaired_recipient_generation = params
                .minimum_mutation_generation
                .max(generation_after_previous);
            let repaired_global = MailboxGlobalState {
                schema_version: MAILBOX_STATE_SCHEMA_VERSION,
                last_enqueue_seq: repaired_global_sequence,
                updated_unix_ms: now_unix_ms,
            };
            let repaired_recipient = MailboxRecipientState {
                schema_version: MAILBOX_STATE_SCHEMA_VERSION,
                recipient_session_id: params.recipient_session_id.clone(),
                physical_row_count: repaired_recipient_count,
                mutation_generation: repaired_recipient_generation,
                updated_unix_ms: now_unix_ms,
            };
            let encoded_global = encode_mailbox_global_state(&repaired_global)?;
            let encoded_recipient = encode_mailbox_recipient_state(&repaired_recipient)?;
            let outcome = db
                .mutate_batch_if_revisions_pressure_bypass(
                    cf::CF_KV,
                    [
                        RevisionGuard::new(
                            MAILBOX_GLOBAL_STATE_KEY.as_bytes(),
                            raw_global
                                .as_ref()
                                .map(|revisioned| revisioned.revision_sha256),
                        ),
                        RevisionGuard::new(
                            recipient_state_key.as_bytes(),
                            raw_recipient
                                .as_ref()
                                .map(|revisioned| revisioned.revision_sha256),
                        ),
                    ],
                    std::iter::empty::<Vec<u8>>(),
                    [
                        (
                            MAILBOX_GLOBAL_STATE_KEY.as_bytes().to_vec(),
                            encoded_global.clone(),
                        ),
                        (
                            recipient_state_key.as_bytes().to_vec(),
                            encoded_recipient.clone(),
                        ),
                    ],
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "AGENT_MAILBOX_REPAIR_COMMIT_FAILED: recipient={:?} guarded repair \
                             failed: {error}",
                            params.recipient_session_id
                        ),
                    )
                })?;
            if !outcome.applied {
                tracing::warn!(
                    code = "AGENT_MAILBOX_REPAIR_CONFLICT",
                    recipient_session_id = %params.recipient_session_id,
                    retry,
                    "mailbox state changed during explicit repair; rereading every repair SoT"
                );
                continue;
            }
            let global_readback = read_mailbox_global_state_revisioned(&db)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    "AGENT_MAILBOX_REPAIR_GLOBAL_READBACK_MISSING",
                )
            })?;
            let recipient_readback =
                read_mailbox_recipient_state_revisioned(&db, &params.recipient_session_id)?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            "AGENT_MAILBOX_REPAIR_RECIPIENT_READBACK_MISSING",
                        )
                    })?;
            if global_readback.state != repaired_global
                || recipient_readback.state != repaired_recipient
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_REPAIR_READBACK_DRIFT: committed_seq={} \
                         global_matches={} recipient_matches={}",
                        outcome.committed_seq,
                        global_readback.state == repaired_global,
                        recipient_readback.state == repaired_recipient
                    ),
                ));
            }
            stable_inbox_snapshot(&db, &params.recipient_session_id, now_unix_ms)?;
            tracing::warn!(
                code = "AGENT_MAILBOX_STATE_REPAIRED",
                recipient_session_id = %params.recipient_session_id,
                reason = %params.reason,
                observed_global_message_rows,
                observed_global_max_sequence,
                observed_recipient_rows = recipient_scan.scanned_rows,
                previous_decodable_global_sequence,
                previous_decodable_recipient_generation,
                repaired_global_sequence,
                repaired_recipient_generation,
                repaired_recipient_count,
                committed_seq = outcome.committed_seq,
                "explicit guarded mailbox-state repair completed with separate physical readback"
            );
            return Ok(AgentMailboxRepairResponse {
                ok: true,
                recipient_session_id: params.recipient_session_id,
                observed_global_message_rows,
                observed_global_max_sequence,
                observed_recipient_rows: recipient_scan.scanned_rows,
                previous_decodable_global_sequence,
                previous_decodable_recipient_generation,
                repaired_global_sequence,
                repaired_recipient_generation,
                repaired_recipient_count,
                committed_seq: outcome.committed_seq,
                global_state_readback: MailboxRowReadback {
                    cf_name: cf::CF_KV.to_owned(),
                    row_key: MAILBOX_GLOBAL_STATE_KEY.to_owned(),
                    value_len_bytes: encoded_global.len() as u64,
                    value_sha256: hash_bytes(&encoded_global),
                },
                recipient_state_readback: MailboxRowReadback {
                    cf_name: cf::CF_KV.to_owned(),
                    row_key: recipient_state_key,
                    value_len_bytes: encoded_recipient.len() as u64,
                    value_sha256: hash_bytes(&encoded_recipient),
                },
            });
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_MAILBOX_REPAIR_CONTENTION: repair for {:?} conflicted \
                 {MAILBOX_MAX_CONFLICT_RETRIES} times",
                params.recipient_session_id
            ),
        ))
    }
}

impl From<&AgentWaitParams> for AgentInboxParams {
    fn from(value: &AgentWaitParams) -> Self {
        Self {
            drain: value.drain,
            max_messages: value.max_messages,
            kinds: value.kinds.clone(),
        }
    }
}

fn wait_response(
    started: Instant,
    timed_out: bool,
    inbox: AgentInboxResponse,
) -> AgentWaitResponse {
    AgentWaitResponse {
        ok: true,
        waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        timed_out,
        inbox,
    }
}

fn encode_mailbox_global_state(state: &MailboxGlobalState) -> Result<Vec<u8>, ErrorData> {
    synapse_storage::encode_json(state).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("encode durable mailbox global state: {error}"),
        )
    })
}

fn encode_mailbox_recipient_state(state: &MailboxRecipientState) -> Result<Vec<u8>, ErrorData> {
    synapse_storage::encode_json(state).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "encode durable mailbox state for recipient {:?}: {error}",
                state.recipient_session_id
            ),
        )
    })
}

fn read_mailbox_global_state_revisioned(
    db: &Db,
) -> Result<Option<RevisionedMailboxGlobalState>, ErrorData> {
    let revisioned = db
        .get_cf_revisioned(cf::CF_KV, MAILBOX_GLOBAL_STATE_KEY.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("read revisioned durable mailbox global state: {error}"),
            )
        })?;
    let Some(revisioned) = revisioned else {
        return Ok(None);
    };
    let value = revisioned.value.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_GLOBAL_STATE_EXPIRED: {MAILBOX_GLOBAL_STATE_KEY} has a physical \
                 expired envelope; all mailbox enqueue operations are disabled"
            ),
        )
    })?;
    let state: MailboxGlobalState = synapse_storage::decode_json(&value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_GLOBAL_STATE_CORRUPTED: decode {MAILBOX_GLOBAL_STATE_KEY}: {error}; \
                 all mailbox enqueue operations are disabled"
            ),
        )
    })?;
    if state.schema_version != MAILBOX_STATE_SCHEMA_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_GLOBAL_STATE_VERSION_INVALID: {MAILBOX_GLOBAL_STATE_KEY} has \
                 schema_version={}, expected {MAILBOX_STATE_SCHEMA_VERSION}",
                state.schema_version
            ),
        ));
    }
    Ok(Some(RevisionedMailboxGlobalState {
        state,
        revision_sha256: revisioned.revision_sha256,
    }))
}

fn read_mailbox_recipient_state_revisioned(
    db: &Db,
    session_id: &str,
) -> Result<Option<RevisionedMailboxRecipientState>, ErrorData> {
    let key = mailbox_recipient_state_key(session_id);
    let revisioned = db
        .get_cf_revisioned(cf::CF_KV, key.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("read revisioned durable mailbox recipient state {key}: {error}"),
            )
        })?;
    let Some(revisioned) = revisioned else {
        return Ok(None);
    };
    let value = revisioned.value.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_RECIPIENT_STATE_EXPIRED: {key} has a physical expired envelope; \
                 mailbox mutations for {session_id:?} are disabled"
            ),
        )
    })?;
    let state: MailboxRecipientState = synapse_storage::decode_json(&value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_RECIPIENT_STATE_CORRUPTED: decode {key}: {error}; mailbox \
                     mutations for {session_id:?} are disabled"
            ),
        )
    })?;
    if state.schema_version != MAILBOX_STATE_SCHEMA_VERSION
        || state.recipient_session_id != session_id
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_RECIPIENT_STATE_IDENTITY_INVALID: key={key} schema_version={} \
                 expected_schema={MAILBOX_STATE_SCHEMA_VERSION} stored_recipient={:?} \
                 expected_recipient={session_id:?}",
                state.schema_version, state.recipient_session_id
            ),
        ));
    }
    Ok(Some(RevisionedMailboxRecipientState {
        state,
        revision_sha256: revisioned.revision_sha256,
    }))
}

fn decode_mailbox_row(key: &[u8], encoded: &[u8]) -> Result<AgentMailboxMessage, ErrorData> {
    let key_text = std::str::from_utf8(key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("agent mailbox row key is not UTF-8: {error}"),
        )
    })?;
    let message: AgentMailboxMessage = synapse_storage::decode_json(encoded).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("decode agent mailbox row {key_text}: {error}"),
        )
    })?;
    if message.schema_version != SCHEMA_VERSION || message.row_key != key_text {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_ROW_IDENTITY_INVALID: key={key_text} schema_version={} \
                 expected_schema={SCHEMA_VERSION} embedded_row_key={:?}",
                message.schema_version, message.row_key
            ),
        ));
    }
    Ok(message)
}

fn mailbox_sequence_from_row_key(key: &[u8]) -> Result<u64, ErrorData> {
    let key_text = std::str::from_utf8(key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("agent mailbox row key is not UTF-8: {error}"),
        )
    })?;
    let sequence_text = key_text.rsplit('/').nth(1).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("AGENT_MAILBOX_ROW_KEY_INVALID: missing sequence component in {key_text}"),
        )
    })?;
    sequence_text.parse::<u64>().map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_ROW_KEY_INVALID: sequence {sequence_text:?} in {key_text} is not \
                 a u64: {error}"
            ),
        )
    })
}

fn initialize_mailbox_global_state(
    db: &Db,
    now_unix_ms: u64,
) -> Result<RevisionedMailboxGlobalState, ErrorData> {
    for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
        if let Some(state) = read_mailbox_global_state_revisioned(db)? {
            return Ok(state);
        }
        let rows = db
            .scan_cf_prefix(cf::CF_KV, MESSAGE_PREFIX.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("scan legacy mailbox rows while initializing sequence state: {error}"),
                )
            })?;
        let mut max_sequence = 0_u64;
        for (key, encoded) in rows {
            decode_mailbox_row(&key, &encoded)?;
            max_sequence = max_sequence.max(mailbox_sequence_from_row_key(&key)?);
        }
        let state = MailboxGlobalState {
            schema_version: MAILBOX_STATE_SCHEMA_VERSION,
            last_enqueue_seq: max_sequence,
            updated_unix_ms: now_unix_ms,
        };
        let encoded = encode_mailbox_global_state(&state)?;
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                [RevisionGuard::new(
                    MAILBOX_GLOBAL_STATE_KEY.as_bytes(),
                    None,
                )],
                std::iter::empty::<Vec<u8>>(),
                [(MAILBOX_GLOBAL_STATE_KEY.as_bytes().to_vec(), encoded)],
            )
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("initialize guarded mailbox global sequence state: {error}"),
                )
            })?;
        if !outcome.applied {
            tracing::warn!(
                code = "AGENT_MAILBOX_GLOBAL_STATE_INIT_CONFLICT",
                retry,
                "mailbox global-state initialization conflicted; rereading physical state"
            );
            continue;
        }
        let readback = read_mailbox_global_state_revisioned(db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_GLOBAL_STATE_READBACK_MISSING: committed_seq={} but \
                     {MAILBOX_GLOBAL_STATE_KEY} is absent",
                    outcome.committed_seq
                ),
            )
        })?;
        if readback.state != state {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_GLOBAL_STATE_READBACK_DRIFT: committed_seq={} \
                     expected_last_enqueue_seq={max_sequence} actual_last_enqueue_seq={}",
                    outcome.committed_seq, readback.state.last_enqueue_seq
                ),
            ));
        }
        return Ok(readback);
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "AGENT_MAILBOX_GLOBAL_STATE_CONTENTION: initialization conflicted \
             {MAILBOX_MAX_CONFLICT_RETRIES} times"
        ),
    ))
}

fn initialize_mailbox_recipient_state(
    db: &Db,
    session_id: &str,
    now_unix_ms: u64,
) -> Result<RevisionedMailboxRecipientState, ErrorData> {
    let state_key = mailbox_recipient_state_key(session_id);
    for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
        if let Some(state) = read_mailbox_recipient_state_revisioned(db, session_id)? {
            return Ok(state);
        }
        let scan = scan_inbox(db, session_id, now_unix_ms)?;
        let physical_row_count = u64::try_from(scan.scanned_rows).map_err(|_error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("mailbox physical row count overflows u64 for {session_id:?}"),
            )
        })?;
        let state = MailboxRecipientState {
            schema_version: MAILBOX_STATE_SCHEMA_VERSION,
            recipient_session_id: session_id.to_owned(),
            physical_row_count,
            mutation_generation: 0,
            updated_unix_ms: now_unix_ms,
        };
        let encoded = encode_mailbox_recipient_state(&state)?;
        let outcome = db
            .mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                [RevisionGuard::new(state_key.as_bytes(), None)],
                std::iter::empty::<Vec<u8>>(),
                [(state_key.as_bytes().to_vec(), encoded)],
            )
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("initialize guarded mailbox recipient state {state_key}: {error}"),
                )
            })?;
        if !outcome.applied {
            tracing::warn!(
                code = "AGENT_MAILBOX_RECIPIENT_STATE_INIT_CONFLICT",
                session_id,
                retry,
                "mailbox recipient-state initialization conflicted; rereading physical state"
            );
            continue;
        }
        let readback =
            read_mailbox_recipient_state_revisioned(db, session_id)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_RECIPIENT_STATE_READBACK_MISSING: committed_seq={} but \
                     {state_key} is absent",
                        outcome.committed_seq
                    ),
                )
            })?;
        if readback.state != state {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_RECIPIENT_STATE_READBACK_DRIFT: committed_seq={} \
                     expected_count={physical_row_count} actual_count={}",
                    outcome.committed_seq, readback.state.physical_row_count
                ),
            ));
        }
        return Ok(readback);
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "AGENT_MAILBOX_RECIPIENT_STATE_CONTENTION: initialization for {session_id:?} \
             conflicted {MAILBOX_MAX_CONFLICT_RETRIES} times"
        ),
    ))
}

fn stable_inbox_snapshot(
    db: &Db,
    session_id: &str,
    now_unix_ms: u64,
) -> Result<StableInboxSnapshot, ErrorData> {
    for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
        let before = initialize_mailbox_recipient_state(db, session_id, now_unix_ms)?;
        let scan = scan_inbox(db, session_id, now_unix_ms)?;
        let after = read_mailbox_recipient_state_revisioned(db, session_id)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_RECIPIENT_STATE_MISSING: durable state for {session_id:?} \
                     disappeared during a physical queue audit"
                ),
            )
        })?;
        if before.revision_sha256 != after.revision_sha256 {
            tracing::debug!(
                code = "AGENT_MAILBOX_SNAPSHOT_CONFLICT",
                session_id,
                retry,
                "mailbox state changed during physical row scan; rereading both SoTs"
            );
            continue;
        }
        let physical_row_count = u64::try_from(scan.scanned_rows).map_err(|_error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("mailbox physical row count overflows u64 for {session_id:?}"),
            )
        })?;
        if after.state.physical_row_count != physical_row_count {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_COUNTER_DRIFT: recipient={session_id:?} durable_count={} \
                     physical_count={physical_row_count} generation={}; mailbox mutations are \
                     disabled until the counter is explicitly repaired from physical rows",
                    after.state.physical_row_count, after.state.mutation_generation
                ),
            ));
        }
        return Ok(StableInboxSnapshot { state: after, scan });
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "AGENT_MAILBOX_SNAPSHOT_CONTENTION: recipient {session_id:?} changed during \
             {MAILBOX_MAX_CONFLICT_RETRIES} consecutive physical queue audits"
        ),
    ))
}

fn next_recipient_state(
    current: &MailboxRecipientState,
    physical_row_count: u64,
    now_unix_ms: u64,
) -> Result<MailboxRecipientState, ErrorData> {
    let mutation_generation = current.mutation_generation.checked_add(1).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_GENERATION_EXHAUSTED: recipient {:?} reached u64::MAX",
                current.recipient_session_id
            ),
        )
    })?;
    Ok(MailboxRecipientState {
        schema_version: MAILBOX_STATE_SCHEMA_VERSION,
        recipient_session_id: current.recipient_session_id.clone(),
        physical_row_count,
        mutation_generation,
        updated_unix_ms: now_unix_ms,
    })
}

fn cleanup_expired_recipient_rows(
    db: &Db,
    session_id: &str,
    now_unix_ms: u64,
) -> Result<usize, ErrorData> {
    let state_key = mailbox_recipient_state_key(session_id);
    for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
        let snapshot = stable_inbox_snapshot(db, session_id, now_unix_ms)?;
        if snapshot.scan.expired_keys.is_empty() {
            return Ok(0);
        }
        let deleted = u64::try_from(snapshot.scan.expired_keys.len()).map_err(|_error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                "expired mailbox key count overflows u64",
            )
        })?;
        let physical_row_count = snapshot
            .state
            .state
            .physical_row_count
            .checked_sub(deleted)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_COUNTER_UNDERFLOW: recipient={session_id:?} durable_count={} \
                         expired_rows={deleted}",
                        snapshot.state.state.physical_row_count
                    ),
                )
            })?;
        let next_state =
            next_recipient_state(&snapshot.state.state, physical_row_count, now_unix_ms)?;
        let encoded_state = encode_mailbox_recipient_state(&next_state)?;
        let outcome = db.mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [RevisionGuard::new(
                state_key.as_bytes(),
                Some(snapshot.state.revision_sha256),
            )],
            snapshot.scan.expired_keys.clone(),
            [(state_key.as_bytes().to_vec(), encoded_state)],
        );
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let readback = stable_inbox_snapshot(db, session_id, now_unix_ms)?;
                let mut all_absent = true;
                for key in &snapshot.scan.expired_keys {
                    let value = db.get_cf(cf::CF_KV, key).map_err(|read_error| {
                        mcp_error(
                            read_error.code(),
                            format!(
                                "AGENT_MAILBOX_EXPIRY_COMMIT_AMBIGUOUS: cleanup failed ({error}) \
                                 and exact row readback failed for {}: {read_error}",
                                String::from_utf8_lossy(key)
                            ),
                        )
                    })?;
                    all_absent &= value.is_none();
                }
                if all_absent
                    && readback.state.state.mutation_generation >= next_state.mutation_generation
                {
                    tracing::warn!(
                        code = "AGENT_MAILBOX_EXPIRY_AMBIGUOUS_COMMIT_RECONCILED",
                        session_id,
                        deleted,
                        "separate physical row/counter readback proved expiry cleanup committed"
                    );
                    return usize::try_from(deleted).map_err(|_error| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            "deleted count overflows usize",
                        )
                    });
                }
                return Err(mcp_error(
                    error.code(),
                    format!(
                        "AGENT_MAILBOX_EXPIRY_COMMIT_AMBIGUOUS: guarded expiry cleanup for \
                         {session_id:?} failed ({error}); exact row/counter readback did not prove \
                         the intended delete committed"
                    ),
                ));
            }
        };
        if !outcome.applied {
            tracing::warn!(
                code = "AGENT_MAILBOX_EXPIRY_REVISION_CONFLICT",
                session_id,
                retry,
                "guarded expiry cleanup conflicted; rereading queue state and rows"
            );
            continue;
        }
        stable_inbox_snapshot(db, session_id, now_unix_ms)?;
        return usize::try_from(deleted).map_err(|_error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                "deleted count overflows usize",
            )
        });
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "AGENT_MAILBOX_EXPIRY_CONTENTION: cleanup for {session_id:?} conflicted \
             {MAILBOX_MAX_CONFLICT_RETRIES} times"
        ),
    ))
}

fn commit_mailbox_drain(
    db: &Db,
    session_id: &str,
    current: &RevisionedMailboxRecipientState,
    rows: &[DecodedMailboxRow],
    now_unix_ms: u64,
) -> Result<MailboxDrainCommitOutcome, ErrorData> {
    if rows.is_empty() || rows.len() > MAX_MESSAGES_PER_READ {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_BATCH_INVALID: recipient={session_id:?} rows={} \
                 permitted=1..={MAX_MESSAGES_PER_READ}",
                rows.len()
            ),
        ));
    }
    let delete_count = u64::try_from(rows.len()).map_err(|_error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            "mailbox drain delete count overflows u64",
        )
    })?;
    let physical_row_count = current
        .state
        .physical_row_count
        .checked_sub(delete_count)
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_COUNTER_UNDERFLOW: context=agent_inbox_drain \
                     recipient={session_id:?} \
                     durable_count={} delete_count={delete_count}",
                    current.state.physical_row_count
                ),
            )
        })?;
    let next_state = next_recipient_state(&current.state, physical_row_count, now_unix_ms)?;
    let messages = rows
        .iter()
        .map(|row| {
            let value_len_bytes = u64::try_from(row.encoded.len()).map_err(|_error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "mailbox row length overflows u64 for {:?}",
                        row.message.row_key
                    ),
                )
            })?;
            Ok(MailboxDrainMessageIntent {
                message_id: row.message.message_id.clone(),
                row_key: row.message.row_key.clone(),
                value_len_bytes,
                value_sha256: hash_bytes(&row.encoded),
                from_session: row.message.from_session.clone(),
                message_kind: row.message.kind.clone(),
                sent_at_unix_ms: row.message.sent_at_unix_ms,
                request_receipt: row.message.request_receipt,
            })
        })
        .collect::<Result<Vec<_>, ErrorData>>()?;
    let receipts = build_mailbox_drain_receipts(session_id, &messages, now_unix_ms);
    let event_ts_ns = super::agent_events::unix_time_ns_now();
    let operation_id = mailbox_drain_operation_id(
        session_id,
        now_unix_ms,
        event_ts_ns,
        next_state.mutation_generation,
        physical_row_count,
        &messages,
        &receipts,
    )?;
    let records = messages
        .iter()
        .map(|message| mailbox_drain_event_record(session_id, message, event_ts_ns, &operation_id))
        .collect::<Vec<_>>();
    let outbox = MailboxDrainOutbox {
        schema_version: DRAIN_OUTBOX_SCHEMA_VERSION,
        row_key: drain_outbox_row_key(session_id, &operation_id),
        operation_id,
        recipient_session_id: session_id.to_owned(),
        created_at_unix_ms: now_unix_ms,
        event_ts_ns,
        committed_recipient_generation: next_state.mutation_generation,
        physical_row_count_after: physical_row_count,
        messages,
        receipts,
        records,
    };
    validate_mailbox_drain_outbox(&outbox, Some(session_id))?;
    let encoded_outbox = encode_mailbox_drain_outbox(&outbox)?;

    let state_key = mailbox_recipient_state_key(session_id);
    let mut guards = Vec::with_capacity(2 + rows.len() + outbox.receipts.len());
    guards.push(RevisionGuard::new(
        state_key.as_bytes(),
        Some(current.revision_sha256),
    ));
    let mut delete_keys = Vec::with_capacity(rows.len());
    for row in rows {
        if row.key.as_slice() != row.message.row_key.as_bytes() {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_ROW_IDENTITY_INVALID: physical_key={} \
                     embedded_row_key={:?}",
                    String::from_utf8_lossy(&row.key),
                    row.message.row_key
                ),
            ));
        }
        let revisioned = db
            .get_cf_revisioned(cf::CF_KV, &row.key)
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "read exact mailbox row revision before drain {}: {error}",
                        row.message.row_key
                    ),
                )
            })?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_DRAIN_ROW_MISSING: selected row {} disappeared \
                         without a recipient-state transition",
                        row.message.row_key
                    ),
                )
            })?;
        if revisioned.value.as_deref() != Some(row.encoded.as_slice()) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_ROW_REVISION_DRIFT: selected row {} no longer \
                     contains the exact scanned bytes",
                    row.message.row_key
                ),
            ));
        }
        guards.push(RevisionGuard::new(
            row.key.clone(),
            Some(revisioned.revision_sha256),
        ));
        delete_keys.push(row.key.clone());
    }
    if db
        .get_cf_revisioned(cf::CF_KV, outbox.row_key.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("read mailbox drain outbox absence guard: {error}"),
            )
        })?
        .is_some()
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_OUTBOX_COLLISION: operation={} row={} already exists",
                outbox.operation_id, outbox.row_key
            ),
        ));
    }
    guards.push(RevisionGuard::new(outbox.row_key.as_bytes(), None));

    let mut puts = Vec::with_capacity(2 + outbox.receipts.len());
    puts.push((
        state_key.as_bytes().to_vec(),
        encode_mailbox_recipient_state(&next_state)?,
    ));
    puts.push((outbox.row_key.as_bytes().to_vec(), encoded_outbox.clone()));
    let mut encoded_receipts = Vec::with_capacity(outbox.receipts.len());
    for receipt in &outbox.receipts {
        if db
            .get_cf_revisioned(cf::CF_KV, receipt.row_key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("read receipt absence guard {}: {error}", receipt.row_key),
                )
            })?
            .is_some()
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_RECEIPT_COLLISION: message_id={} receipt_row={} \
                     already exists while its source message remains queued; repair the \
                     pre-existing read evidence before draining",
                    receipt.message_id, receipt.row_key
                ),
            ));
        }
        let encoded = synapse_storage::encode_json(receipt).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("encode mailbox drain receipt {}: {error}", receipt.row_key),
            )
        })?;
        guards.push(RevisionGuard::new(receipt.row_key.as_bytes(), None));
        puts.push((receipt.row_key.as_bytes().to_vec(), encoded.clone()));
        encoded_receipts.push((receipt.row_key.clone(), encoded));
    }

    let outcome =
        db.mutate_batch_if_revisions_pressure_bypass(cf::CF_KV, guards, delete_keys.clone(), puts);
    let committed_seq = match outcome {
        Ok(outcome) => {
            if !outcome.applied {
                return Ok(MailboxDrainCommitOutcome::Conflict);
            }
            outcome.committed_seq
        }
        Err(error) => {
            match verify_mailbox_drain_commit(
                db,
                &outbox,
                &encoded_outbox,
                &encoded_receipts,
                now_unix_ms,
            ) {
                Ok(()) => {
                    tracing::warn!(
                        code = "AGENT_MAILBOX_DRAIN_AMBIGUOUS_COMMIT_RECONCILED",
                        session_id,
                        operation_id = %outbox.operation_id,
                        deleted = delete_count,
                        "separate physical message/state/receipt/outbox readback proved the \
                         atomic drain committed"
                    );
                }
                Err(readback_error) => {
                    MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
                    return Err(mcp_error(
                        error.code(),
                        format!(
                            "AGENT_MAILBOX_DRAIN_COMMIT_RECONCILIATION_REQUIRED: \
                             recipient={session_id:?} operation={} commit_error={error}; exact \
                             atomic readback failed: {}; remediation=leave the durable state \
                             untouched, stop the daemon, reopen Calyx, and reconcile the outbox, \
                             message, receipt, and recipient-state rows before retrying",
                            outbox.operation_id, readback_error.message
                        ),
                    ));
                }
            }
            0
        }
    };
    verify_mailbox_drain_commit(db, &outbox, &encoded_outbox, &encoded_receipts, now_unix_ms)?;
    tracing::info!(
        code = "AGENT_MAILBOX_DRAIN_INTENT_COMMITTED",
        session_id,
        operation_id = %outbox.operation_id,
        outbox_row = %outbox.row_key,
        message_count = outbox.messages.len(),
        receipt_count = outbox.receipts.len(),
        committed_recipient_generation = outbox.committed_recipient_generation,
        physical_row_count_after = outbox.physical_row_count_after,
        committed_seq,
        "readback=CF_KV edge=mailbox_drain_intent"
    );
    Ok(MailboxDrainCommitOutcome::Committed {
        deleted_count: delete_keys.len(),
        outbox,
    })
}

fn verify_mailbox_drain_commit(
    db: &Db,
    outbox: &MailboxDrainOutbox,
    encoded_outbox: &[u8],
    encoded_receipts: &[(String, Vec<u8>)],
    now_unix_ms: u64,
) -> Result<(), ErrorData> {
    let stored_outbox = db
        .get_cf(cf::CF_KV, outbox.row_key.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "read exact mailbox drain outbox {}: {error}",
                    outbox.row_key
                ),
            )
        })?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "AGENT_MAILBOX_DRAIN_OUTBOX_READBACK_MISSING: operation={} row={}",
                    outbox.operation_id, outbox.row_key
                ),
            )
        })?;
    if stored_outbox != encoded_outbox {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_OUTBOX_READBACK_DRIFT: operation={} row={} \
                 stored bytes differ from the committed intent",
                outbox.operation_id, outbox.row_key
            ),
        ));
    }
    for message in &outbox.messages {
        if db
            .get_cf(cf::CF_KV, message.row_key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("read drained mailbox row {}: {error}", message.row_key),
                )
            })?
            .is_some()
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_READBACK_PRESENT: operation={} message_id={} row={} \
                     remains physically queued beside its durable read intent",
                    outbox.operation_id, message.message_id, message.row_key
                ),
            ));
        }
    }
    for (row_key, expected) in encoded_receipts {
        if let Some(actual) = db.get_cf(cf::CF_KV, row_key.as_bytes()).map_err(|error| {
            mcp_error(
                error.code(),
                format!("read exact mailbox receipt {row_key}: {error}"),
            )
        })? && actual != *expected
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_RECEIPT_READBACK_DRIFT: operation={} receipt_row={} \
                     contains bytes not authorized by the drain intent",
                    outbox.operation_id, row_key
                ),
            ));
        }
        // An absent receipt is valid here: the sender can consume and delete
        // it immediately after the atomic drain. The still-durable outbox is
        // the exact proof that the receipt was committed in that same batch.
    }
    let state = read_mailbox_recipient_state_revisioned(db, &outbox.recipient_session_id)?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_STATE_MISSING: operation={} recipient={:?}",
                    outbox.operation_id, outbox.recipient_session_id
                ),
            )
        })?;
    if state.state.mutation_generation < outbox.committed_recipient_generation {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_STATE_DRIFT: operation={} expected_generation_at_least={} \
                 actual_generation={}",
                outbox.operation_id,
                outbox.committed_recipient_generation,
                state.state.mutation_generation
            ),
        ));
    }
    if state.state.mutation_generation == outbox.committed_recipient_generation
        && state.state.physical_row_count != outbox.physical_row_count_after
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_COUNT_DRIFT: operation={} generation={} \
                 expected_physical_count={} actual_physical_count={}",
                outbox.operation_id,
                outbox.committed_recipient_generation,
                outbox.physical_row_count_after,
                state.state.physical_row_count
            ),
        ));
    }
    stable_inbox_snapshot(db, &outbox.recipient_session_id, now_unix_ms)?;
    Ok(())
}

fn build_mailbox_drain_receipts(
    recipient_session: &str,
    messages: &[MailboxDrainMessageIntent],
    now_unix_ms: u64,
) -> Vec<MailboxReceipt> {
    messages
        .iter()
        .filter(|message| message.request_receipt)
        .map(|message| MailboxReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            receipt_id: format!(
                "receipt-{}-{}",
                from_session_tag(&message.from_session),
                message.message_id
            ),
            row_key: receipt_row_key(&message.from_session, &message.message_id),
            from_session: message.from_session.clone(),
            recipient_session: recipient_session.to_owned(),
            message_id: message.message_id.clone(),
            message_kind: message.message_kind.clone(),
            status: "read".to_owned(),
            read_at_unix_ms: now_unix_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(RECEIPT_TTL_MS),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn mailbox_drain_operation_id(
    recipient_session_id: &str,
    created_at_unix_ms: u64,
    event_ts_ns: u64,
    committed_recipient_generation: u64,
    physical_row_count_after: u64,
    messages: &[MailboxDrainMessageIntent],
    receipts: &[MailboxReceipt],
) -> Result<String, ErrorData> {
    let identity = MailboxDrainOutboxIdentity {
        schema_version: DRAIN_OUTBOX_SCHEMA_VERSION,
        recipient_session_id,
        created_at_unix_ms,
        event_ts_ns,
        committed_recipient_generation,
        physical_row_count_after,
        messages,
        receipts,
    };
    let encoded = synapse_storage::encode_json(&identity).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_MAILBOX_DRAIN_IDENTITY_ENCODE_FAILED: \
                 recipient={recipient_session_id:?}: {error}"
            ),
        )
    })?;
    Ok(sha256_hex(&encoded))
}

fn mailbox_drain_event_record(
    recipient_session_id: &str,
    message: &MailboxDrainMessageIntent,
    event_ts_ns: u64,
    operation_id: &str,
) -> synapse_core::AgentEventRecord {
    let mut record = synapse_core::AgentEventRecord::new(
        event_ts_ns,
        synapse_core::AgentEventKind::MessageReceived,
    );
    record.session_id = Some(recipient_session_id.to_owned());
    record.attributes.conversation_id = Some(recipient_session_id.to_owned());
    let mut payload = serde_json::Map::new();
    payload.insert(
        "from_session".to_owned(),
        Value::String(message.from_session.clone()),
    );
    payload.insert(
        "message_id".to_owned(),
        Value::String(message.message_id.clone()),
    );
    payload.insert(
        "message_kind".to_owned(),
        Value::String(message.message_kind.clone()),
    );
    payload.insert("payload_bytes".to_owned(), json!(message.value_len_bytes));
    payload.insert("sent_at_unix_ms".to_owned(), json!(message.sent_at_unix_ms));
    payload.insert(
        DRAIN_OUTBOX_OPERATION_FIELD.to_owned(),
        Value::String(operation_id.to_owned()),
    );
    record.payload = Value::Object(payload);
    record
}

fn encode_mailbox_drain_outbox(outbox: &MailboxDrainOutbox) -> Result<Vec<u8>, ErrorData> {
    synapse_storage::encode_json(outbox).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "encode mailbox drain outbox operation {}: {error}",
                outbox.operation_id
            ),
        )
    })
}

fn validate_mailbox_drain_outbox(
    outbox: &MailboxDrainOutbox,
    expected_recipient: Option<&str>,
) -> Result<(), ErrorData> {
    if outbox.schema_version != DRAIN_OUTBOX_SCHEMA_VERSION
        || !is_lower_sha256(&outbox.operation_id)
        || outbox.created_at_unix_ms == 0
        || outbox.event_ts_ns == 0
        || outbox.committed_recipient_generation == 0
        || outbox.physical_row_count_after > MAX_INBOX_ROWS_PER_RECIPIENT as u64
        || outbox.messages.is_empty()
        || outbox.messages.len() > MAX_MESSAGES_PER_READ
        || outbox.records.len() != outbox.messages.len()
        || expected_recipient
            .is_some_and(|expected| expected != outbox.recipient_session_id.as_str())
        || outbox.row_key
            != drain_outbox_row_key(&outbox.recipient_session_id, &outbox.operation_id)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_OUTBOX_ENVELOPE_INVALID: operation={:?} row={:?} \
                 recipient={:?} expected_recipient={expected_recipient:?} schema={} \
                 expected_schema={DRAIN_OUTBOX_SCHEMA_VERSION} messages={} records={} \
                 generation={} created_at_unix_ms={} event_ts_ns={}; remediation=repair the \
                 durable drain intent from exact mailbox/receipt/event SoTs",
                outbox.operation_id,
                outbox.row_key,
                outbox.recipient_session_id,
                outbox.schema_version,
                outbox.messages.len(),
                outbox.records.len(),
                outbox.committed_recipient_generation,
                outbox.created_at_unix_ms,
                outbox.event_ts_ns
            ),
        ));
    }
    let mut row_keys = BTreeSet::new();
    let mut message_ids = BTreeSet::new();
    let recipient_prefix = mailbox_recipient_prefix(&outbox.recipient_session_id);
    for message in &outbox.messages {
        if message.message_id.trim().is_empty()
            || message.row_key.trim().is_empty()
            || !message.row_key.starts_with(&recipient_prefix)
            || !message
                .row_key
                .ends_with(&format!("/{}", message.message_id))
            || message.value_len_bytes == 0
            || !is_sha256_readback(&message.value_sha256)
            || message.from_session.trim().is_empty()
            || message.message_kind.trim().is_empty()
            || !row_keys.insert(message.row_key.as_str())
            || !message_ids.insert(message.message_id.as_str())
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_MESSAGE_IDENTITY_INVALID: operation={} \
                     message_id={:?} row_key={:?} value_len={} value_sha256={:?}",
                    outbox.operation_id,
                    message.message_id,
                    message.row_key,
                    message.value_len_bytes,
                    message.value_sha256
                ),
            ));
        }
    }
    let expected_receipts = build_mailbox_drain_receipts(
        &outbox.recipient_session_id,
        &outbox.messages,
        outbox.created_at_unix_ms,
    );
    if outbox.receipts != expected_receipts {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_RECEIPT_SET_INVALID: operation={} expected_receipts={} \
                 actual_receipts={}",
                outbox.operation_id,
                expected_receipts.len(),
                outbox.receipts.len()
            ),
        ));
    }
    for (message, actual) in outbox.messages.iter().zip(&outbox.records) {
        let expected = mailbox_drain_event_record(
            &outbox.recipient_session_id,
            message,
            outbox.event_ts_ns,
            &outbox.operation_id,
        );
        if *actual != expected {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_EVENT_SET_INVALID: operation={} message_id={} \
                     stored event differs from its deterministic intent",
                    outbox.operation_id, message.message_id
                ),
            ));
        }
        super::agent_events::validate_and_encode(actual).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_EVENT_INVALID: operation={} message_id={}: {error}",
                    outbox.operation_id, message.message_id
                ),
            )
        })?;
    }
    let expected_operation_id = mailbox_drain_operation_id(
        &outbox.recipient_session_id,
        outbox.created_at_unix_ms,
        outbox.event_ts_ns,
        outbox.committed_recipient_generation,
        outbox.physical_row_count_after,
        &outbox.messages,
        &outbox.receipts,
    )?;
    if expected_operation_id != outbox.operation_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_OPERATION_ID_MISMATCH: stored={} expected={expected_operation_id}",
                outbox.operation_id
            ),
        ));
    }
    Ok(())
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_sha256_readback(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_lower_sha256)
}

fn relay_pending_mailbox_drain_outbox(db: &Db, session_id: &str) -> Result<(), ErrorData> {
    let prefix = drain_outbox_recipient_prefix(session_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("scan pending mailbox drain outbox for {session_id:?}: {error}"),
            )
        })?;
    if rows.len() > 1 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_OUTBOX_MULTIPLE_PENDING: recipient={session_id:?} \
                 pending_rows={}; remediation=reconcile each exact operation against \
                 CF_AGENT_EVENTS before another drain",
                rows.len()
            ),
        ));
    }
    let Some((key, encoded)) = rows.into_iter().next() else {
        return Ok(());
    };
    let outbox = decode_mailbox_drain_outbox(&key, &encoded, Some(session_id))?;
    tracing::warn!(
        code = "AGENT_MAILBOX_DRAIN_OUTBOX_RECOVERY",
        session_id,
        operation_id = %outbox.operation_id,
        message_count = outbox.messages.len(),
        "a durable mailbox drain intent survived without event acknowledgement; reconciling it"
    );
    relay_mailbox_drain_outbox(db, &outbox)
}

fn decode_mailbox_drain_outbox(
    key: &[u8],
    encoded: &[u8],
    expected_recipient: Option<&str>,
) -> Result<MailboxDrainOutbox, ErrorData> {
    let key_text = std::str::from_utf8(key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("mailbox drain outbox key is not UTF-8: {error}"),
        )
    })?;
    let outbox: MailboxDrainOutbox = synapse_storage::decode_json(encoded).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("decode mailbox drain outbox {key_text}: {error}"),
        )
    })?;
    if outbox.row_key != key_text {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_OUTBOX_ROW_IDENTITY_INVALID: key={key_text} \
                 embedded_row_key={:?}",
                outbox.row_key
            ),
        ));
    }
    validate_mailbox_drain_outbox(&outbox, expected_recipient)?;
    Ok(outbox)
}

fn encoded_mailbox_drain_record_multiset(
    records: &[synapse_core::AgentEventRecord],
) -> Result<BTreeMap<Vec<u8>, usize>, ErrorData> {
    let mut multiset = BTreeMap::new();
    for record in records {
        let encoded = super::agent_events::validate_and_encode(record).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("encode validated mailbox drain event intent: {error}"),
            )
        })?;
        *multiset.entry(encoded).or_insert(0) += 1;
    }
    Ok(multiset)
}

fn inspect_mailbox_drain_journal(
    db: &Db,
    outbox: &MailboxDrainOutbox,
) -> Result<MailboxDrainJournalState, ErrorData> {
    validate_mailbox_drain_outbox(outbox, Some(&outbox.recipient_session_id))?;
    let end_ts_ns = outbox.event_ts_ns.checked_add(1).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_EVENT_TIMESTAMP_EXHAUSTED: operation={} event_ts_ns=u64::MAX",
                outbox.operation_id
            ),
        )
    })?;
    let start_key = agent_event_key(outbox.event_ts_ns, 0);
    let end_key = agent_event_key(end_ts_ns, 0);
    let rows = scan_mailbox_drain_journal_rows(db, outbox, &start_key, &end_key)?;
    let expected = encoded_mailbox_drain_record_multiset(&outbox.records)?;
    let mut actual = BTreeMap::new();
    for (key, encoded) in rows {
        let record: synapse_core::AgentEventRecord = synapse_storage::decode_json(&encoded)
            .map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_DRAIN_JOURNAL_ROW_INVALID: operation={} event_ts_ns={} \
                         key_hex={}: {error}",
                        outbox.operation_id,
                        outbox.event_ts_ns,
                        synapse_storage::constellations::hex_encode(&key)
                    ),
                )
            })?;
        if record
            .payload
            .get(DRAIN_OUTBOX_OPERATION_FIELD)
            .and_then(Value::as_str)
            == Some(outbox.operation_id.as_str())
        {
            *actual.entry(encoded).or_insert(0) += 1;
        }
    }
    if actual.is_empty() {
        return Ok(MailboxDrainJournalState::Absent);
    }
    if actual != expected {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_JOURNAL_DIVERGED: operation={} event_ts_ns={} \
                 expected_records={} actual_records={}; remediation=quarantine and reconcile the \
                 partial/duplicate primary event rows before acknowledging the outbox",
                outbox.operation_id,
                outbox.event_ts_ns,
                expected.values().sum::<usize>(),
                actual.values().sum::<usize>()
            ),
        ));
    }
    Ok(MailboxDrainJournalState::Exact)
}

fn scan_mailbox_drain_journal_rows(
    db: &Db,
    outbox: &MailboxDrainOutbox,
    start_key: &[u8],
    end_key: &[u8],
) -> Result<MailboxDrainJournalRows, ErrorData> {
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_AGENT_EVENTS,
            start_key,
            end_key,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "AGENT_MAILBOX_DRAIN_JOURNAL_SNAPSHOT_PIN_FAILED: operation={} \
                     event_ts_ns={}: {error}",
                    outbox.operation_id, outbox.event_ts_ns
                ),
            )
        })?;
    let snapshot_seq = lease.snapshot_seq;
    let lease_id = lease.lease_id;
    let mut rows = Vec::new();
    let mut candidate_rows_examined = 0_usize;
    let scan_result = (|| -> Result<(), ErrorData> {
        loop {
            let remaining = MAX_DRAIN_OUTBOX_TIMESTAMP_CANDIDATES
                .checked_sub(candidate_rows_examined)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_MAILBOX_DRAIN_JOURNAL_RANGE_OVERSIZED: operation={} \
                         event_ts_ns={} candidates={} cap={MAX_DRAIN_OUTBOX_TIMESTAMP_CANDIDATES}",
                            outbox.operation_id, outbox.event_ts_ns, candidate_rows_examined
                        ),
                    )
                })?;
            if remaining == 0 {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_DRAIN_JOURNAL_RANGE_OVERSIZED: operation={} event_ts_ns={} \
                     candidates={candidate_rows_examined} \
                     cap={MAX_DRAIN_OUTBOX_TIMESTAMP_CANDIDATES}",
                        outbox.operation_id, outbox.event_ts_ns
                    ),
                ));
            }
            let cursor_before = lease.next_after().map(ToOwned::to_owned);
            let page = db
                .scan_cf_fixed_width_range_page_coherent(
                    &mut lease,
                    remaining.min(DRAIN_OUTBOX_SCAN_PAGE_ROWS),
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "AGENT_MAILBOX_DRAIN_JOURNAL_SCAN_FAILED: operation={} event_ts_ns={} \
                         after_key_hex={}: {error}",
                            outbox.operation_id,
                            outbox.event_ts_ns,
                            cursor_before.as_deref().map_or_else(
                                || "none".to_owned(),
                                synapse_storage::constellations::hex_encode
                            )
                        ),
                    )
                })?;
            candidate_rows_examined = candidate_rows_examined
                .checked_add(page.candidate_rows_examined)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_MAILBOX_DRAIN_JOURNAL_SCAN_COUNTER_OVERFLOW: operation={}",
                            outbox.operation_id
                        ),
                    )
                })?;
            rows.extend(page.rows);
            if !page.more {
                break;
            }
        }
        Ok(())
    })();
    let release_result = db.release_coherent_scan(&mut lease);
    match (scan_result, release_result) {
        (Err(scan_error), Ok(_)) => return Err(scan_error),
        (Err(scan_error), Err(release_error)) => {
            return Err(mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "{}; additionally failed to release coherent lease_id={lease_id} \
                     snapshot_seq={snapshot_seq}: {release_error}",
                    scan_error.message
                ),
            ));
        }
        (Ok(()), Err(error)) => {
            return Err(mcp_error(
                error.code(),
                format!(
                    "AGENT_MAILBOX_DRAIN_JOURNAL_SNAPSHOT_RELEASE_FAILED: operation={} \
                     lease_id={lease_id} snapshot_seq={snapshot_seq}: {error}",
                    outbox.operation_id
                ),
            ));
        }
        (Ok(()), Ok(false)) => {
            return Err(mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "AGENT_MAILBOX_DRAIN_JOURNAL_SNAPSHOT_EXPIRED_AT_RELEASE: operation={} \
                     lease_id={lease_id} snapshot_seq={snapshot_seq}; repeat the bounded exact audit",
                    outbox.operation_id
                ),
            ));
        }
        (Ok(()), Ok(true)) => {}
    }
    tracing::debug!(
        code = "AGENT_MAILBOX_DRAIN_JOURNAL_SNAPSHOT_COMPLETE",
        operation_id = %outbox.operation_id,
        lease_id,
        snapshot_seq,
        candidate_rows_examined,
        matched_range_rows = rows.len(),
        "completed one-generation mailbox drain journal audit and released its lease"
    );
    Ok(rows)
}

fn relay_mailbox_drain_outbox(db: &Db, outbox: &MailboxDrainOutbox) -> Result<(), ErrorData> {
    validate_mailbox_drain_outbox(outbox, Some(&outbox.recipient_session_id))?;
    let encoded_outbox = encode_mailbox_drain_outbox(outbox)?;
    let encoded_receipts = outbox
        .receipts
        .iter()
        .map(|receipt| {
            synapse_storage::encode_json(receipt)
                .map(|encoded| (receipt.row_key.clone(), encoded))
                .map_err(|error| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "encode receipt {} from mailbox drain outbox: {error}",
                            receipt.row_key
                        ),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    verify_mailbox_drain_commit(
        db,
        outbox,
        &encoded_outbox,
        &encoded_receipts,
        unix_time_ms_now(),
    )?;

    let prior_state = inspect_mailbox_drain_journal(db, outbox)?;
    if prior_state == MailboxDrainJournalState::Absent {
        if let Err(error) = super::agent_events::record_agent_events(db, &outbox.records) {
            MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(mcp_error(
                error.code(),
                format!(
                    "AGENT_MAILBOX_DRAIN_EVENT_COMMIT_RECONCILIATION_REQUIRED: recipient={:?} \
                     operation={} records={} event_error={error}; remediation=leave the durable \
                     outbox intact, stop the daemon, reopen Calyx, then inspect this operation in \
                     CF_AGENT_EVENTS before retrying",
                    outbox.recipient_session_id,
                    outbox.operation_id,
                    outbox.records.len()
                ),
            ));
        }
        match inspect_mailbox_drain_journal(db, outbox) {
            Ok(MailboxDrainJournalState::Exact) => {}
            Ok(MailboxDrainJournalState::Absent) => {
                MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "AGENT_MAILBOX_DRAIN_EVENT_READBACK_MISSING: operation={} event writer \
                         returned success but exact primary rows are absent; remediation=leave the \
                         outbox intact, stop the daemon, and reconcile CF_AGENT_EVENTS",
                        outbox.operation_id
                    ),
                ));
            }
            Err(error) => {
                MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "AGENT_MAILBOX_DRAIN_EVENT_READBACK_RECONCILIATION_REQUIRED: \
                         operation={} event writer returned success but exact journal readback \
                         failed: {}; remediation=leave the outbox intact, stop the daemon, reopen \
                         Calyx, and reconcile CF_AGENT_EVENTS",
                        outbox.operation_id, error.message
                    ),
                ));
            }
        }
    }
    acknowledge_mailbox_drain_outbox(db, outbox)?;
    tracing::info!(
        code = "AGENT_MAILBOX_DRAIN_OUTBOX_ACKNOWLEDGED",
        recipient_session_id = %outbox.recipient_session_id,
        operation_id = %outbox.operation_id,
        message_count = outbox.messages.len(),
        receipt_count = outbox.receipts.len(),
        recovered_existing_journal = prior_state == MailboxDrainJournalState::Exact,
        "readback=CF_AGENT_EVENTS+CF_KV edge=mailbox_drain_outbox_acknowledged"
    );
    Ok(())
}

fn acknowledge_mailbox_drain_outbox(db: &Db, outbox: &MailboxDrainOutbox) -> Result<(), ErrorData> {
    let revisioned = db
        .get_cf_revisioned(cf::CF_KV, outbox.row_key.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("read mailbox drain outbox before acknowledgement: {error}"),
            )
        })?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_DRAIN_ACK_OUTBOX_MISSING: operation={} row={} disappeared \
                     before its guarded acknowledgement",
                    outbox.operation_id, outbox.row_key
                ),
            )
        })?;
    let encoded = revisioned.value.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_ACK_OUTBOX_EXPIRED: operation={} row={} has an expired \
                 physical envelope",
                outbox.operation_id, outbox.row_key
            ),
        )
    })?;
    let stored = decode_mailbox_drain_outbox(
        outbox.row_key.as_bytes(),
        &encoded,
        Some(&outbox.recipient_session_id),
    )?;
    if stored != *outbox {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_ACK_OUTBOX_DRIFT: operation={} durable intent changed \
                 before acknowledgement",
                outbox.operation_id
            ),
        ));
    }
    let outcome = db.mutate_batch_if_revisions_pressure_bypass(
        cf::CF_KV,
        [RevisionGuard::new(
            outbox.row_key.as_bytes(),
            Some(revisioned.revision_sha256),
        )],
        [outbox.row_key.as_bytes().to_vec()],
        std::iter::empty::<(Vec<u8>, Vec<u8>)>(),
    );
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            let readback = db
                .get_cf_revisioned(cf::CF_KV, outbox.row_key.as_bytes())
                .map_err(|read_error| {
                    MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
                    mcp_error(
                        read_error.code(),
                        format!(
                            "AGENT_MAILBOX_DRAIN_ACK_RECONCILIATION_REQUIRED: operation={} \
                             ack_error={error}; exact outbox readback failed: {read_error}; \
                             remediation=stop the daemon, reopen Calyx, and reconcile this \
                             operation against CF_AGENT_EVENTS",
                            outbox.operation_id
                        ),
                    )
                })?;
            if readback.is_none() {
                tracing::warn!(
                    code = "AGENT_MAILBOX_DRAIN_ACK_AMBIGUOUS_COMMIT_RECONCILED",
                    operation_id = %outbox.operation_id,
                    "separate physical outbox readback proved acknowledgement committed"
                );
                return Ok(());
            }
            MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(mcp_error(
                error.code(),
                format!(
                    "AGENT_MAILBOX_DRAIN_ACK_RECONCILIATION_REQUIRED: operation={} \
                     journal_state=exact ack_error={error}; durable outbox remains present; \
                     remediation=stop the daemon, reopen Calyx, and reconcile before relay",
                    outbox.operation_id
                ),
            ));
        }
    };
    if !outcome.applied {
        MAILBOX_DRAIN_RECONCILIATION_LATCH.store(true, Ordering::Release);
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_MAILBOX_DRAIN_ACK_CONFLICT: operation={} exact outbox revision changed \
                 after journal publication; remediation=stop the daemon and reconcile the \
                 durable outbox before relay",
                outbox.operation_id
            ),
        ));
    }
    if db
        .get_cf_revisioned(cf::CF_KV, outbox.row_key.as_bytes())
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("read mailbox drain outbox after acknowledgement: {error}"),
            )
        })?
        .is_some()
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_MAILBOX_DRAIN_ACK_READBACK_PRESENT: operation={} committed_seq={} \
                 outbox row remains physically present",
                outbox.operation_id, outcome.committed_seq
            ),
        ));
    }
    Ok(())
}

fn enqueue_mailbox_message(
    db: &Db,
    request: MailboxEnqueueRequest<'_>,
    now_unix_ms: u64,
) -> Result<MailboxEnqueueOutcome, ErrorData> {
    let MailboxEnqueueRequest {
        from_session,
        to_session,
        kind,
        payload,
        artifact_handle,
        ttl_ms,
        request_receipt,
    } = request;
    // Establish the durable historical high-watermark before expiry cleanup is
    // allowed to delete any legacy row that may contain the maximum sequence.
    initialize_mailbox_global_state(db, now_unix_ms)?;
    let mut expired_rows_deleted_before =
        cleanup_expired_recipient_rows(db, to_session, now_unix_ms)?;
    let recipient_state_key = mailbox_recipient_state_key(to_session);
    for retry in 0..MAILBOX_MAX_CONFLICT_RETRIES {
        let snapshot = stable_inbox_snapshot(db, to_session, now_unix_ms)?;
        if !snapshot.scan.expired_keys.is_empty() {
            let deleted = cleanup_expired_recipient_rows(db, to_session, now_unix_ms)?;
            expired_rows_deleted_before = expired_rows_deleted_before
                .checked_add(deleted)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "expired mailbox cleanup count overflow for recipient {to_session:?}"
                        ),
                    )
                })?;
            continue;
        }
        let queue_depth_before =
            usize::try_from(snapshot.state.state.physical_row_count).map_err(|_error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("mailbox counter overflows usize for {to_session:?}"),
                )
            })?;
        if queue_depth_before > MAX_INBOX_ROWS_PER_RECIPIENT {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_CAPACITY_INVARIANT_VIOLATED: recipient={to_session:?} \
                     durable_and_physical_count={queue_depth_before} exceeds \
                     hard_cap={MAX_INBOX_ROWS_PER_RECIPIENT}; \
                     enqueue is disabled until existing rows are drained or expired",
                ),
            ));
        }
        if queue_depth_before >= MAX_INBOX_ROWS_PER_RECIPIENT {
            return Ok(MailboxEnqueueOutcome::Full {
                queue_depth: queue_depth_before,
                expired_rows_deleted_before,
            });
        }
        let global = initialize_mailbox_global_state(db, now_unix_ms)?;
        let next_sequence = global
            .state
            .last_enqueue_seq
            .checked_add(1)
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    "AGENT_MAILBOX_SEQUENCE_EXHAUSTED: global mailbox sequence reached u64::MAX",
                )
            })?;
        let message_id = format!("agentmsg-{now_unix_ms:020}-{next_sequence:020}");
        let row_key = mailbox_row_key(to_session, now_unix_ms, next_sequence, &message_id);
        let message = AgentMailboxMessage {
            schema_version: SCHEMA_VERSION,
            message_id,
            row_key: row_key.clone(),
            from_session: from_session.to_owned(),
            to_session: to_session.to_owned(),
            kind: kind.trim().to_owned(),
            payload: payload.clone(),
            artifact_handle: artifact_handle.map(str::trim).map(ToOwned::to_owned),
            sent_at_unix_ms: now_unix_ms,
            ttl_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
            delivery_attempts: 0,
            request_receipt,
        };
        let encoded_message = encode_mailbox_message(&message)?;
        let next_global = MailboxGlobalState {
            schema_version: MAILBOX_STATE_SCHEMA_VERSION,
            last_enqueue_seq: next_sequence,
            updated_unix_ms: now_unix_ms,
        };
        let next_recipient = next_recipient_state(
            &snapshot.state.state,
            snapshot
                .state
                .state
                .physical_row_count
                .checked_add(1)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!("mailbox counter overflow for {to_session:?}"),
                    )
                })?,
            now_unix_ms,
        )?;
        let outcome = db.mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [
                RevisionGuard::new(
                    MAILBOX_GLOBAL_STATE_KEY.as_bytes(),
                    Some(global.revision_sha256),
                ),
                RevisionGuard::new(
                    recipient_state_key.as_bytes(),
                    Some(snapshot.state.revision_sha256),
                ),
                RevisionGuard::new(row_key.as_bytes(), None),
            ],
            std::iter::empty::<Vec<u8>>(),
            [
                (
                    MAILBOX_GLOBAL_STATE_KEY.as_bytes().to_vec(),
                    encode_mailbox_global_state(&next_global)?,
                ),
                (
                    recipient_state_key.as_bytes().to_vec(),
                    encode_mailbox_recipient_state(&next_recipient)?,
                ),
                (row_key.as_bytes().to_vec(), encoded_message.clone()),
            ],
        );
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let stored_message = db.get_cf(cf::CF_KV, row_key.as_bytes()).map_err(
                    |read_error| {
                        mcp_error(
                            read_error.code(),
                            format!(
                                "AGENT_MAILBOX_ENQUEUE_COMMIT_AMBIGUOUS: enqueue of {row_key} \
                                 failed ({error}) and exact message readback failed \
                                 ({read_error}); inspect message/global/recipient state before retry"
                            ),
                        )
                    },
                )?;
                let global_readback = read_mailbox_global_state_revisioned(db)?;
                // A generation read alone cannot prove that the durable counter
                // still describes the physical queue. Reconcile an ambiguous
                // commit only after a stable state/row audit proves those two
                // Sources of Truth are coherent.
                let recipient_readback = stable_inbox_snapshot(db, to_session, now_unix_ms)?;
                if stored_message.as_deref() == Some(encoded_message.as_slice())
                    && global_readback
                        .as_ref()
                        .is_some_and(|readback| readback.state.last_enqueue_seq >= next_sequence)
                    && recipient_readback.state.state.mutation_generation
                        >= next_recipient.mutation_generation
                {
                    tracing::warn!(
                        code = "AGENT_MAILBOX_ENQUEUE_AMBIGUOUS_COMMIT_RECONCILED",
                        from_session,
                        to_session,
                        message_id = %message.message_id,
                        durable_physical_count = recipient_readback.state.state.physical_row_count,
                        queue_depth_after = queue_depth_before + 1,
                        "separate physical message/counter readback proved enqueue committed"
                    );
                    return Ok(MailboxEnqueueOutcome::Committed(Box::new(
                        MailboxEnqueueCommit {
                            message,
                            storage_readback: MailboxRowReadback {
                                cf_name: cf::CF_KV.to_owned(),
                                row_key,
                                value_len_bytes: encoded_message.len() as u64,
                                value_sha256: hash_bytes(&encoded_message),
                            },
                            queue_depth_before,
                            queue_depth_after: queue_depth_before + 1,
                            expired_rows_deleted_before,
                        },
                    )));
                }
                return Err(mcp_error(
                    error.code(),
                    format!(
                        "AGENT_MAILBOX_ENQUEUE_NOT_COMMITTED: guarded enqueue of {row_key} \
                         failed: {error}; exact message/counter readback did not prove commit, so \
                         no stale counter or sequence was retried"
                    ),
                ));
            }
        };
        if !outcome.applied {
            tracing::warn!(
                code = "AGENT_MAILBOX_ENQUEUE_REVISION_CONFLICT",
                from_session,
                to_session,
                retry,
                conflict_guard_index = outcome
                    .conflict
                    .as_ref()
                    .map(|conflict| conflict.guard_index),
                "guarded mailbox enqueue conflicted; rereading all queue SoTs before recomputing"
            );
            continue;
        }
        let storage_readback = readback_exact_mailbox_row(db, &row_key)?;
        let stored_message = db
            .get_cf(cf::CF_KV, row_key.as_bytes())
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_ENQUEUE_READBACK_MISSING: committed_seq={} row={row_key}",
                        outcome.committed_seq
                    ),
                )
            })?;
        let global_readback = read_mailbox_global_state_revisioned(db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_GLOBAL_STATE_MISSING_AFTER_ENQUEUE: committed_seq={}",
                    outcome.committed_seq
                ),
            )
        })?;
        let recipient_readback = read_mailbox_recipient_state_revisioned(db, to_session)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_MAILBOX_RECIPIENT_STATE_MISSING_AFTER_ENQUEUE: committed_seq={} \
                         recipient={to_session:?}",
                        outcome.committed_seq
                    ),
                )
            })?;
        if stored_message != encoded_message
            || global_readback.state.last_enqueue_seq < next_sequence
            || recipient_readback.state.mutation_generation < next_recipient.mutation_generation
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_ENQUEUE_READBACK_DRIFT: committed_seq={} message_bytes_match={} \
                     expected_sequence={next_sequence} actual_sequence={} \
                     expected_recipient_generation={} actual_recipient_generation={}",
                    outcome.committed_seq,
                    stored_message == encoded_message,
                    global_readback.state.last_enqueue_seq,
                    next_recipient.mutation_generation,
                    recipient_readback.state.mutation_generation
                ),
            ));
        }
        return Ok(MailboxEnqueueOutcome::Committed(Box::new(
            MailboxEnqueueCommit {
                message,
                storage_readback,
                queue_depth_before,
                queue_depth_after: queue_depth_before + 1,
                expired_rows_deleted_before,
            },
        )));
    }
    Err(mcp_error(
        error_codes::STORAGE_WRITE_FAILED,
        format!(
            "AGENT_MAILBOX_ENQUEUE_CONTENTION: enqueue to {to_session:?} conflicted \
             {MAILBOX_MAX_CONFLICT_RETRIES} times"
        ),
    ))
}

fn scan_inbox(db: &Db, session_id: &str, now_unix_ms: u64) -> Result<InboxScan, ErrorData> {
    let prefix = mailbox_recipient_prefix(session_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let scanned_rows = rows.len();
    let mut expired_keys = Vec::new();
    let mut messages = Vec::new();
    for (key, encoded) in rows {
        let message = decode_mailbox_row(&key, &encoded)?;
        if message.to_session != session_id {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_RECIPIENT_IDENTITY_DRIFT: row={} embedded_recipient={:?} \
                     expected_recipient={session_id:?}",
                    String::from_utf8_lossy(&key),
                    message.to_session
                ),
            ));
        }
        if message.expires_at_unix_ms <= now_unix_ms {
            expired_keys.push(key);
        } else {
            messages.push(DecodedMailboxRow {
                key,
                encoded,
                message,
            });
        }
    }
    Ok(InboxScan {
        scanned_rows,
        expired_keys,
        messages,
    })
}

fn readback_exact_mailbox_row(db: &Db, row_key: &str) -> Result<MailboxRowReadback, ErrorData> {
    let stored = db
        .get_cf(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!("agent mailbox row missing after write: {row_key}"),
            )
        })?;
    Ok(MailboxRowReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        value_len_bytes: stored.len() as u64,
        value_sha256: hash_bytes(&stored),
    })
}

fn encode_mailbox_message(message: &AgentMailboxMessage) -> Result<Vec<u8>, ErrorData> {
    synapse_storage::encode_json(message).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("encode agent mailbox message: {error}"),
        )
    })
}

fn dashboard_json_readback(value: impl Serialize) -> Result<Value, ErrorData> {
    serde_json::to_value(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("serialize dashboard mailbox readback: {error}"),
        )
    })
}

fn validate_send_params(params: &AgentSendParams) -> Result<(), ErrorData> {
    validate_session_id(&params.to_session)?;
    validate_kind(&params.kind)?;
    validate_ttl_ms(params.ttl_ms)?;
    validate_payload_size(&params.payload)?;
    if let Some(artifact_handle) = &params.artifact_handle {
        validate_artifact_handle(artifact_handle)?;
    }
    Ok(())
}

fn validate_kind(kind: &str) -> Result<(), ErrorData> {
    let trimmed = kind.trim();
    if trimmed.is_empty() {
        return Err(params_error("agent_send kind must not be empty"));
    }
    if trimmed.chars().count() > MAX_KIND_CHARS {
        return Err(params_error(format!(
            "agent_send kind must be at most {MAX_KIND_CHARS} Unicode scalar values"
        )));
    }
    if !trimmed.chars().all(|ch| !ch.is_control()) {
        return Err(params_error(
            "agent_send kind must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_ttl_ms(ttl_ms: u64) -> Result<(), ErrorData> {
    if ttl_ms == 0 || ttl_ms > MAX_MESSAGE_TTL_MS {
        return Err(params_error(format!(
            "agent_send ttl_ms must be between 1 and {MAX_MESSAGE_TTL_MS}"
        )));
    }
    Ok(())
}

fn validate_payload_size(payload: &Value) -> Result<(), ErrorData> {
    let encoded = synapse_storage::encode_json(payload).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("agent_send payload must be JSON-encodable: {error}"),
        )
    })?;
    if encoded.len() > MAX_PAYLOAD_BYTES {
        return Err(params_error(format!(
            "agent_send payload must encode to <= {MAX_PAYLOAD_BYTES} bytes; got {}",
            encoded.len()
        )));
    }
    Ok(())
}

fn validate_artifact_handle(value: &str) -> Result<(), ErrorData> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(params_error(
            "agent_send artifact_handle must not be empty when provided",
        ));
    }
    if trimmed.chars().count() > MAX_ARTIFACT_HANDLE_CHARS {
        return Err(params_error(format!(
            "agent_send artifact_handle must be at most {MAX_ARTIFACT_HANDLE_CHARS} Unicode scalar values"
        )));
    }
    if !trimmed.chars().all(|ch| !ch.is_control()) {
        return Err(params_error(
            "agent_send artifact_handle must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_inbox_params(max_messages: usize) -> Result<(), ErrorData> {
    if max_messages == 0 || max_messages > MAX_MESSAGES_PER_READ {
        return Err(params_error(format!(
            "agent_inbox max_messages must be between 1 and {MAX_MESSAGES_PER_READ}"
        )));
    }
    Ok(())
}

fn validate_wait_params(params: &AgentWaitParams) -> Result<(), ErrorData> {
    if params.timeout_ms > MAX_WAIT_TIMEOUT_MS {
        return Err(params_error(format!(
            "agent_wait timeout_ms must be <= {MAX_WAIT_TIMEOUT_MS}"
        )));
    }
    validate_kind_filter(&params.kinds)?;
    validate_inbox_params(params.max_messages)
}

fn validate_kind_filter(kinds: &[String]) -> Result<(), ErrorData> {
    if kinds.len() > MAX_KIND_FILTER_ENTRIES {
        return Err(params_error(format!(
            "kinds filter must have at most {MAX_KIND_FILTER_ENTRIES} entries; got {}",
            kinds.len()
        )));
    }
    for kind in kinds {
        validate_kind(kind)?;
    }
    Ok(())
}

fn validate_broadcast_target(target: &BroadcastTarget) -> Result<(), ErrorData> {
    let selectors = u8::from(target.all)
        + u8::from(!target.agent_kinds.is_empty())
        + u8::from(!target.sessions.is_empty());
    if selectors == 0 {
        return Err(params_error(
            "agent_send_broadcast `to` must set exactly one selector: all=true, a non-empty \
             agent_kinds, or a non-empty sessions list",
        ));
    }
    if selectors > 1 {
        return Err(params_error(
            "agent_send_broadcast `to` selectors are mutually exclusive: set only one of all / \
             agent_kinds / sessions",
        ));
    }
    for kind in &target.agent_kinds {
        if kind.trim().is_empty() {
            return Err(params_error(
                "agent_send_broadcast agent_kinds entries must not be empty",
            ));
        }
    }
    for session in &target.sessions {
        validate_session_id(session)?;
    }
    Ok(())
}

fn from_session_tag(session_id: &str) -> String {
    hex_bytes(session_id.as_bytes())
}

fn receipt_recipient_prefix(session_id: &str) -> String {
    format!(
        "{RECEIPT_PREFIX}/owner_hex/{}/rcpt/",
        hex_bytes(session_id.as_bytes())
    )
}

fn receipt_row_key(owner_session: &str, message_id: &str) -> String {
    format!("{}{message_id}", receipt_recipient_prefix(owner_session))
}

#[allow(clippy::type_complexity)]
fn scan_receipts(
    db: &Db,
    owner_session: &str,
    now_unix_ms: u64,
) -> Result<(Vec<MailboxReceipt>, Vec<Vec<u8>>, usize), ErrorData> {
    let prefix = receipt_recipient_prefix(owner_session);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let scanned_rows = rows.len();
    let mut receipts = Vec::new();
    let mut expired_keys = Vec::new();
    for (key, encoded) in rows {
        let key_text = std::str::from_utf8(&key).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("receipt row key is not UTF-8: {error}"),
            )
        })?;
        let receipt: MailboxReceipt = synapse_storage::decode_json(&encoded).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("decode receipt row {key_text}: {error}"),
            )
        })?;
        let expected_row_key = receipt_row_key(owner_session, &receipt.message_id);
        let expected_receipt_id = format!(
            "receipt-{}-{}",
            from_session_tag(owner_session),
            receipt.message_id
        );
        if receipt.schema_version != RECEIPT_SCHEMA_VERSION
            || receipt.row_key != key_text
            || receipt.row_key != expected_row_key
            || receipt.receipt_id != expected_receipt_id
            || receipt.from_session != owner_session
            || receipt.recipient_session.trim().is_empty()
            || receipt.message_id.trim().is_empty()
            || receipt.message_kind.trim().is_empty()
            || receipt.status != "read"
            || receipt.expires_at_unix_ms < receipt.read_at_unix_ms
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_MAILBOX_RECEIPT_IDENTITY_INVALID: key={key_text} schema={} \
                     expected_schema={RECEIPT_SCHEMA_VERSION} embedded_row={:?} \
                     expected_row={expected_row_key:?} receipt_id={:?} \
                     expected_receipt_id={expected_receipt_id:?} stored_owner={:?} \
                     expected_owner={owner_session:?} recipient={:?} message_id={:?} \
                     kind={:?} status={:?} read_at={} expires_at={}",
                    receipt.schema_version,
                    receipt.row_key,
                    receipt.receipt_id,
                    receipt.from_session,
                    receipt.recipient_session,
                    receipt.message_id,
                    receipt.message_kind,
                    receipt.status,
                    receipt.read_at_unix_ms,
                    receipt.expires_at_unix_ms
                ),
            ));
        }
        if receipt.expires_at_unix_ms <= now_unix_ms {
            expired_keys.push(key);
        } else {
            receipts.push(receipt);
        }
    }
    receipts.sort_by_key(|receipt| receipt.read_at_unix_ms);
    Ok((receipts, expired_keys, scanned_rows))
}

fn mailbox_full_error(from_session: &str, to_session: &str, queue_depth: usize) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("agent mailbox for {to_session:?} is full ({queue_depth} rows)"),
        Some(json!({
            "code": error_codes::ACTION_QUEUE_FULL,
            "from_session": from_session,
            "to_session": to_session,
            "queue_depth": queue_depth,
            "max_rows": MAX_INBOX_ROWS_PER_RECIPIENT,
            "source_of_truth": "CF_KV agent-mailbox recipient prefix",
        })),
    )
}

fn skipped_recipient(to_session: String, reason: &str) -> RecipientOutcome {
    RecipientOutcome {
        to_session,
        status: "skipped".to_owned(),
        message_id: None,
        row_key: None,
        storage_readback: None,
        skip_reason: Some(reason.to_owned()),
    }
}

fn recipient_unknown_error(
    from_session: &str,
    to_session: &str,
    recipient: Option<&SessionRegistryRead>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("agent mailbox recipient session {to_session:?} is not live"),
        Some(json!({
            "code": error_codes::RECIPIENT_UNKNOWN,
            "from_session": from_session,
            "to_session": to_session,
            "recipient": recipient,
            "resolution": "start or reconnect the recipient agent so it registers a live MCP session, then retry agent_send",
            "source_of_truth": "session registry read model",
        })),
    )
}

fn is_orchestrator_alias(value: &str) -> bool {
    value.eq_ignore_ascii_case("orchestrator")
}

fn orchestrator_alias_session(
    reads: &[SessionRegistryRead],
    from_session: &str,
) -> Option<SessionRegistryRead> {
    let live_primary = reads
        .iter()
        .filter(|entry| entry.lifecycle == "live")
        .filter(|entry| entry.spawned_agent.is_none())
        .filter(|entry| entry.agent_kind != "local-model");
    latest_session_read(
        live_primary
            .clone()
            .filter(|entry| entry.session_id != from_session),
    )
    .or_else(|| latest_session_read(live_primary))
}

fn successor_for_rotated_session(
    reads: &[SessionRegistryRead],
    old: &SessionRegistryRead,
) -> Option<SessionRegistryRead> {
    if old.lifecycle == "live" {
        return None;
    }
    if let Some(old_spawn) = old.spawned_agent.as_ref() {
        return latest_session_read(reads.iter().filter(|entry| {
            entry.lifecycle == "live"
                && entry.session_id != old.session_id
                && entry
                    .spawned_agent
                    .as_ref()
                    .is_some_and(|spawned| spawned.spawn_id == old_spawn.spawn_id)
        }));
    }

    let old_client_name = old.client_name.as_deref()?;
    latest_session_read(reads.iter().filter(|entry| {
        entry.lifecycle == "live"
            && entry.session_id != old.session_id
            && entry.spawned_agent.is_none()
            && entry.client_name.as_deref() == Some(old_client_name)
            && entry.agent_kind == old.agent_kind
            && (entry.started_at_unix_ms >= old.started_at_unix_ms
                || entry.last_seen_unix_ms >= old.last_seen_unix_ms)
    }))
}

fn latest_session_read<'a>(
    reads: impl Iterator<Item = &'a SessionRegistryRead>,
) -> Option<SessionRegistryRead> {
    reads
        .max_by(|left, right| {
            (
                left.last_seen_unix_ms,
                left.started_at_unix_ms,
                &left.session_id,
            )
                .cmp(&(
                    right.last_seen_unix_ms,
                    right.started_at_unix_ms,
                    &right.session_id,
                ))
        })
        .cloned()
}

fn params_error(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

fn require_mailbox_session_id(
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

fn mailbox_recipient_prefix(session_id: &str) -> String {
    format!("{MESSAGE_PREFIX}{}/msg/", hex_bytes(session_id.as_bytes()))
}

fn drain_outbox_recipient_prefix(session_id: &str) -> String {
    format!(
        "{DRAIN_OUTBOX_PREFIX}{}/op/",
        hex_bytes(session_id.as_bytes())
    )
}

fn drain_outbox_row_key(session_id: &str, operation_id: &str) -> String {
    format!(
        "{}{operation_id}",
        drain_outbox_recipient_prefix(session_id)
    )
}

fn mailbox_recipient_state_key(session_id: &str) -> String {
    format!(
        "agent-mailbox/v2/recipient_hex/{}/queue_state",
        hex_bytes(session_id.as_bytes())
    )
}

fn mailbox_row_key(session_id: &str, sent_at_unix_ms: u64, seq: u64, message_id: &str) -> String {
    format!(
        "{}{sent_at_unix_ms:020}/{seq:020}/{message_id}",
        mailbox_recipient_prefix(session_id)
    )
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

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_bytes(digest.as_ref())
}

const fn default_message_ttl_ms() -> u64 {
    DEFAULT_MESSAGE_TTL_MS
}

const fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

const fn default_max_messages() -> usize {
    DEFAULT_MAX_MESSAGES
}

const fn default_true() -> bool {
    true
}
