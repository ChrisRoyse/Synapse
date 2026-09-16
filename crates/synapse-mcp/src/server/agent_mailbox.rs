//! Per-session mailbox tools for explicit multi-agent handoff (#795).
//!
//! The mailbox is intentionally a small durable queue over the existing
//! daemon-owned `CF_KV` storage handle. Sends fail if the recipient is not a
//! live MCP session, messages are TTL-bounded, and inbox drains delete only the
//! exact rows returned to the recipient.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;
use synapse_storage::{Db, cf};

use super::{
    ErrorData, Json, Parameters, SynapseService, mcp_error,
    session_registry::{SessionRegistryRead, unix_time_ms_now},
    session_tools::validate_session_id,
    tool, tool_router,
};

const SCHEMA_VERSION: u32 = 1;
#[cfg(test)]
const MAILBOX_PREFIX: &str = "agent-mailbox/v1";
const MESSAGE_PREFIX: &str = "agent-mailbox/v1/recipient_hex/";
/// CF_KV key prefix for sender-visible receipt rows (#908). Distinct from the
/// recipient message prefix so receipts never appear in an agent's own inbox.
const RECEIPT_PREFIX: &str = "agent-mailbox/v1/receipt";
const RECEIPT_SCHEMA_VERSION: u32 = 1;
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

/// The reserved **steering-inbox contract** kind (#908): a well-behaved agent
/// drains `steer`-kind messages between tool calls and splices their payload
/// into context at the next safe point. The cooperative tier of `agent_steer`
/// (#905) delivers through this kind; it is filterable via the `kinds` inbox
/// filter so an agent can poll only its steering channel.
pub(crate) const STEER_KIND: &str = "steer";

static NEXT_MAILBOX_SEQ: AtomicU64 = AtomicU64::new(1);

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
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
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
        let expired_rows_deleted_before = cleanup_expired_mailbox_rows(&db, now_unix_ms)?;
        let resolution = self.recipient_live_read(from_session, &params.to_session, now_unix_ms)?;
        let to_session = resolution.resolved_to_session.clone();
        let depth_before = queue_depth_for_recipient(&db, &to_session, now_unix_ms)?;
        if depth_before >= MAX_INBOX_ROWS_PER_RECIPIENT {
            return Err(mailbox_full_error(from_session, &to_session, depth_before));
        }
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
            "queue_depth_before": depth_before,
            "expired_rows_deleted_before": expired_rows_deleted_before,
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

        let seq = NEXT_MAILBOX_SEQ.fetch_add(1, Ordering::Relaxed);
        let message_id = format!("agentmsg-{now_unix_ms:020}-{seq:020}");
        let row_key = mailbox_row_key(&to_session, now_unix_ms, seq, &message_id);
        let message = AgentMailboxMessage {
            schema_version: SCHEMA_VERSION,
            message_id: message_id.clone(),
            row_key: row_key.clone(),
            from_session: from_session.to_owned(),
            to_session: to_session.clone(),
            kind: params.kind.trim().to_owned(),
            payload: params.payload,
            artifact_handle: params.artifact_handle.map(|value| value.trim().to_owned()),
            sent_at_unix_ms: now_unix_ms,
            ttl_ms: params.ttl_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(params.ttl_ms),
            delivery_attempts: 0,
            request_receipt: params.request_receipt,
        };
        let encoded = encode_mailbox_message(&message)?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("write agent mailbox row {row_key}: {error}"),
                )
            })?;
        let storage_readback = readback_exact_mailbox_row(&db, &row_key)?;
        let queue_depth_after = queue_depth_for_recipient(&db, &to_session, now_unix_ms)?;

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
        let mut scan = scan_inbox(&db, session_id, now_unix_ms)?;
        if !scan.expired_keys.is_empty() {
            db.delete_batch(cf::CF_KV, scan.expired_keys.clone())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("delete expired mailbox rows for {session_id}: {error}"),
                    )
                })?;
        }

        // Server-side kind filter (#908): keep only matching kinds, so a drain
        // never deletes messages the caller did not ask for.
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
        let delete_keys = if params.drain {
            scan.messages
                .iter()
                .map(|row| row.key.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        // Journal receipts (#897) BEFORE deleting the drained rows: if the
        // journal refuses, the inbox rows survive and the drain can retry —
        // a message can never vanish unjournaled.
        if params.drain && !scan.messages.is_empty() {
            let receipt_ts_ns = super::agent_events::unix_time_ns_now();
            let receipts = scan
                .messages
                .iter()
                .map(|row| {
                    let mut record = synapse_core::AgentEventRecord::new(
                        receipt_ts_ns,
                        synapse_core::AgentEventKind::MessageReceived,
                    );
                    record.session_id = Some(session_id.to_owned());
                    record.attributes.conversation_id = Some(session_id.to_owned());
                    record.payload = json!({
                        "from_session": &row.message.from_session,
                        "message_id": &row.message.message_id,
                        "message_kind": &row.message.kind,
                        "payload_bytes": row.encoded.len(),
                        "sent_at_unix_ms": row.message.sent_at_unix_ms,
                    });
                    record
                })
                .collect::<Vec<_>>();
            super::agent_events::record_agent_events(&db, &receipts).map_err(|error| {
                super::agent_events::agent_event_tool_error("agent_inbox", &error, false)
            })?;
        }
        // Write sender-visible read receipts (#908) for drained messages that
        // asked for one — BEFORE deleting, so a receipt-write failure leaves the
        // message queued and the drain is retry-safe. A message is acked exactly
        // once: the row is deleted in the same drain that writes its receipt.
        if params.drain {
            write_read_receipts(&db, session_id, &scan.messages, now_unix_ms)?;
        }
        if !delete_keys.is_empty() {
            db.delete_batch(cf::CF_KV, delete_keys.clone())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("delete drained mailbox rows for {session_id}: {error}"),
                    )
                })?;
        }
        let queue_depth_after = queue_depth_for_recipient(&db, session_id, now_unix_ms)?;
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
            expired_rows_deleted: scan.expired_keys.len(),
            returned_count: messages.len(),
            deleted_count: delete_keys.len(),
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
        Ok(response)
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

    #[cfg(test)]
    fn agent_send_broadcast_impl_at(
        &self,
        params: AgentSendBroadcastParams,
        from_session: &str,
        now_unix_ms: u64,
    ) -> Result<AgentSendBroadcastResponse, ErrorData> {
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
        let expired_rows_deleted_before = cleanup_expired_mailbox_rows(&db, now_unix_ms)?;

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
            "expired_rows_deleted_before": expired_rows_deleted_before,
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
            let depth = match queue_depth_for_recipient(&db, &to_session, now_unix_ms) {
                Ok(depth) => depth,
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
            if depth >= MAX_INBOX_ROWS_PER_RECIPIENT {
                skipped_count += 1;
                outcomes.push(RecipientOutcome {
                    to_session,
                    status: "skipped".to_owned(),
                    message_id: None,
                    row_key: None,
                    storage_readback: None,
                    skip_reason: Some(format!("recipient mailbox full ({depth} rows)")),
                });
                continue;
            }
            let seq = NEXT_MAILBOX_SEQ.fetch_add(1, Ordering::Relaxed);
            let message_id = format!("agentmsg-{now_unix_ms:020}-{seq:020}");
            let row_key = mailbox_row_key(&to_session, now_unix_ms, seq, &message_id);
            let message = AgentMailboxMessage {
                schema_version: SCHEMA_VERSION,
                message_id: message_id.clone(),
                row_key: row_key.clone(),
                from_session: from_session.to_owned(),
                to_session: to_session.clone(),
                kind: params.kind.trim().to_owned(),
                payload: params.payload.clone(),
                artifact_handle: params.artifact_handle.clone().map(|v| v.trim().to_owned()),
                sent_at_unix_ms: now_unix_ms,
                ttl_ms: params.ttl_ms,
                expires_at_unix_ms,
                delivery_attempts: 0,
                request_receipt: params.request_receipt,
            };
            let encoded = match encode_mailbox_message(&message) {
                Ok(encoded) => encoded,
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
            if let Err(error) =
                db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
            {
                let error = mcp_error(
                    error.code(),
                    format!("write broadcast row {row_key}: {error}"),
                );
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
            let storage_readback = match readback_exact_mailbox_row(&db, &row_key) {
                Ok(readback) => readback,
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
                                "row_key": &row_key,
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
            delivered_count += 1;
            outcomes.push(RecipientOutcome {
                to_session,
                status: "delivered".to_owned(),
                message_id: Some(message_id),
                row_key: Some(row_key),
                storage_readback: Some(storage_readback),
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

fn scan_inbox(db: &Db, session_id: &str, now_unix_ms: u64) -> Result<InboxScan, ErrorData> {
    let prefix = mailbox_recipient_prefix(session_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let scanned_rows = rows.len();
    let mut expired_keys = Vec::new();
    let mut messages = Vec::new();
    for (key, encoded) in rows {
        let message: AgentMailboxMessage =
            synapse_storage::decode_json(&encoded).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "decode agent mailbox row {}: {error}",
                        String::from_utf8_lossy(&key)
                    ),
                )
            })?;
        if message.schema_version != SCHEMA_VERSION {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "agent mailbox row {} has schema_version {}, expected {SCHEMA_VERSION}",
                    String::from_utf8_lossy(&key),
                    message.schema_version
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

fn cleanup_expired_mailbox_rows(db: &Db, now_unix_ms: u64) -> Result<usize, ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, MESSAGE_PREFIX.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let mut expired_keys = Vec::new();
    for (key, encoded) in rows {
        let message: AgentMailboxMessage =
            synapse_storage::decode_json(&encoded).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "decode agent mailbox row {} during cleanup: {error}",
                        String::from_utf8_lossy(&key)
                    ),
                )
            })?;
        if message.schema_version != SCHEMA_VERSION {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "agent mailbox row {} has schema_version {}, expected {SCHEMA_VERSION}",
                    String::from_utf8_lossy(&key),
                    message.schema_version
                ),
            ));
        }
        if message.expires_at_unix_ms <= now_unix_ms {
            expired_keys.push(key);
        }
    }
    if expired_keys.is_empty() {
        return Ok(0);
    }
    let count = expired_keys.len();
    db.delete_batch(cf::CF_KV, expired_keys).map_err(|error| {
        mcp_error(
            error.code(),
            format!("delete expired agent mailbox rows: {error}"),
        )
    })?;
    Ok(count)
}

fn queue_depth_for_recipient(
    db: &Db,
    session_id: &str,
    now_unix_ms: u64,
) -> Result<usize, ErrorData> {
    Ok(scan_inbox(db, session_id, now_unix_ms)?.messages.len())
}

fn readback_exact_mailbox_row(db: &Db, row_key: &str) -> Result<MailboxRowReadback, ErrorData> {
    let stored = db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .find_map(|(key, value)| (key == row_key.as_bytes()).then_some(value))
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

/// Writes a sender-visible read receipt for every drained message that asked
/// for one. Receipts go to the *sender's* receipt box, never the reader's
/// inbox. Idempotent at the message level: the row key embeds the message id,
/// so re-draining the same message id overwrites rather than duplicates.
fn write_read_receipts(
    db: &Db,
    recipient_session: &str,
    rows: &[DecodedMailboxRow],
    now_unix_ms: u64,
) -> Result<(), ErrorData> {
    let receipt_rows = rows
        .iter()
        .filter(|row| row.message.request_receipt)
        .map(|row| {
            let from_session = row.message.from_session.clone();
            let receipt_id = format!(
                "receipt-{}-{}",
                from_session_tag(&from_session),
                row.message.message_id
            );
            let row_key = receipt_row_key(&from_session, &row.message.message_id);
            let receipt = MailboxReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                receipt_id,
                row_key: row_key.clone(),
                from_session,
                recipient_session: recipient_session.to_owned(),
                message_id: row.message.message_id.clone(),
                message_kind: row.message.kind.clone(),
                status: "read".to_owned(),
                read_at_unix_ms: now_unix_ms,
                expires_at_unix_ms: now_unix_ms.saturating_add(RECEIPT_TTL_MS),
            };
            synapse_storage::encode_json(&receipt)
                .map(|encoded| (row_key.into_bytes(), encoded))
                .map_err(|error| {
                    mcp_error(
                        error_codes::STORAGE_WRITE_FAILED,
                        format!("encode read receipt: {error}"),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if receipt_rows.is_empty() {
        return Ok(());
    }
    db.put_batch_pressure_bypass(cf::CF_KV, receipt_rows)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("write read receipts for sender of {recipient_session}'s drained messages: {error}"),
            )
        })
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
        let receipt: MailboxReceipt = synapse_storage::decode_json(&encoded).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "decode receipt row {}: {error}",
                    String::from_utf8_lossy(&key)
                ),
            )
        })?;
        if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "receipt row {} has schema_version {}, expected {RECEIPT_SCHEMA_VERSION}",
                    String::from_utf8_lossy(&key),
                    receipt.schema_version
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
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_bytes(digest.as_ref()))
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

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
    use rmcp::transport::streamable_http_server::session::SessionState;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        m2::M2ServiceConfig,
        m3::M3ServiceConfig,
        m4::M4ServiceConfig,
        server::session_registry::{SessionRegistry, SpawnedAgentRead},
    };

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

    fn register_session(
        service: &SynapseService,
        session_id: &str,
        now: u64,
    ) -> anyhow::Result<()> {
        let mut registry = service
            .session_registry_ref()
            .lock()
            .map_err(|_error| anyhow::anyhow!("session registry lock poisoned"))?;
        registry.record_seen(session_id, Some("test".to_owned()), now);
        Ok(())
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

    fn register_spawned_session(
        service: &SynapseService,
        session_id: &str,
        agent_kind: &str,
        spawn_id: &str,
        log_root: &Path,
        now: u64,
    ) -> anyhow::Result<()> {
        let mut registry = service
            .session_registry_ref()
            .lock()
            .map_err(|_error| anyhow::anyhow!("session registry lock poisoned"))?;
        registry.record_seen(session_id, Some("test".to_owned()), now);
        registry.record_spawned_agent(
            session_id,
            SpawnedAgentRead {
                spawn_id: spawn_id.to_owned(),
                cli: agent_kind.to_owned(),
                launcher_process_id: 0,
                agent_process_id: None,
                started_by_session_id: Some("test-controller".to_owned()),
                launched_at_unix_ms: now,
                launch_target: "test".to_owned(),
                log_dir: log_root.join(spawn_id).display().to_string(),
                template_id: Some("test-template".to_owned()),
                template_version: Some(1),
                control: None,
            },
            now,
        );
        Ok(())
    }

    fn error_code(error: &rmcp::ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn send_and_peek_then_drain_preserves_order_and_storage_rows() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "sender", 1_000)?;
        register_session(&service, "recipient", 1_000)?;

        let first = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "recipient".to_owned(),
                kind: "handoff".to_owned(),
                payload: json!({"n": 1}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            2_000,
        )?;
        let second = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "recipient".to_owned(),
                kind: "handoff".to_owned(),
                payload: json!({"n": 2}),
                artifact_handle: Some("artifact://known".to_owned()),
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            2_001,
        )?;

        let peek = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: false,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "recipient",
            2_010,
        )?;
        assert_eq!(peek.returned_count, 2);
        assert_eq!(peek.deleted_count, 0);
        assert_eq!(peek.queue_depth_after, 2);
        assert_eq!(peek.messages[0].message_id, first.message_id);
        assert_eq!(peek.messages[1].message_id, second.message_id);

        let drained = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "recipient",
            2_020,
        )?;
        assert_eq!(drained.returned_count, 2);
        assert_eq!(drained.deleted_count, 2);
        assert_eq!(drained.queue_depth_after, 0);
        assert_eq!(drained.readback_rows.len(), 2);

        let after = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "recipient",
            2_030,
        )?;
        assert_eq!(after.returned_count, 0);
        Ok(())
    }

    #[test]
    fn send_to_orchestrator_alias_resolves_latest_live_primary_session() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "worker-session", 500)?;
        register_initialized_session(&service, "orchestrator-old", "codex-mcp-client", 1_000)?;
        register_initialized_session(&service, "orchestrator-new", "codex-mcp-client", 2_000)?;

        let sent = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "orchestrator".to_owned(),
                kind: "finding".to_owned(),
                payload: json!({"case": "orchestrator-alias"}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "worker-session",
            3_000,
        )?;
        assert_eq!(sent.to_session, "orchestrator-new");

        let inbox = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: false,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "orchestrator-new",
            3_001,
        )?;
        assert_eq!(inbox.returned_count, 1);
        assert_eq!(inbox.messages[0].to_session, "orchestrator-new");
        assert_eq!(inbox.messages[0].payload["case"], "orchestrator-alias");
        Ok(())
    }

    #[test]
    fn send_to_closed_session_id_resolves_same_client_successor() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "worker-session", 500)?;
        register_initialized_session(&service, "old-session", "codex-mcp-client", 1_000)?;
        {
            let mut registry = service
                .session_registry_ref()
                .lock()
                .map_err(|_error| anyhow::anyhow!("session registry lock poisoned"))?;
            registry.record_closed("old-session", 1_500);
        }
        register_initialized_session(&service, "new-session", "codex-mcp-client", 2_000)?;

        let sent = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "old-session".to_owned(),
                kind: "finding".to_owned(),
                payload: json!({"case": "successor"}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "worker-session",
            3_000,
        )?;
        assert_eq!(sent.to_session, "new-session");

        let old_inbox = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: false,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "old-session",
            3_001,
        )?;
        assert_eq!(old_inbox.returned_count, 0);

        let new_inbox = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: false,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "new-session",
            3_001,
        )?;
        assert_eq!(new_inbox.returned_count, 1);
        assert_eq!(new_inbox.messages[0].payload["case"], "successor");
        Ok(())
    }

    #[test]
    fn send_and_drain_journal_physical_agent_event_rows() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "journal-sender", 1_000)?;
        register_session(&service, "journal-recipient", 1_000)?;

        let db = service
            .m3_storage()
            .map_err(|error| anyhow::anyhow!("open storage: {error:?}"))?;
        let before = db.scan_cf(synapse_storage::cf::CF_AGENT_EVENTS)?;
        println!(
            "regression_state=cf_scan cf=CF_AGENT_EVENTS case=mailbox_journal before={}",
            before.len()
        );
        assert!(before.is_empty(), "fresh DB must hold no agent events");

        let sent = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "journal-recipient".to_owned(),
                kind: "finding".to_owned(),
                payload: json!({"n": 1}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "journal-sender",
            2_000,
        )?;
        let drained = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "journal-recipient",
            2_010,
        )?;
        assert_eq!(drained.returned_count, 1);
        db.flush()
            .map_err(|error| anyhow::anyhow!("flush journal batch: {error}"))?;

        let rows = db.scan_cf(synapse_storage::cf::CF_AGENT_EVENTS)?;
        let decoded: Vec<synapse_core::AgentEventRecord> = rows
            .iter()
            .map(|(key, value)| {
                synapse_storage::agent_events::decode_agent_event_key(key)
                    .map_err(|error| anyhow::anyhow!("journal key must decode: {error}"))?;
                synapse_storage::decode_json(value)
                    .map_err(|error| anyhow::anyhow!("journal row must decode: {error}"))
            })
            .collect::<anyhow::Result<_>>()?;
        println!(
            "regression_state=cf_scan cf=CF_AGENT_EVENTS case=mailbox_journal after={} kinds={:?}",
            decoded.len(),
            decoded.iter().map(|record| record.kind).collect::<Vec<_>>()
        );
        assert_eq!(decoded.len(), 2, "one message_sent + one message_received");

        let sent_event = &decoded[0];
        assert_eq!(sent_event.kind, synapse_core::AgentEventKind::MessageSent);
        assert_eq!(sent_event.session_id.as_deref(), Some("journal-sender"));
        assert_eq!(sent_event.payload["to_session"], "journal-recipient");
        assert_eq!(sent_event.payload["message_id"], sent.message_id.as_str());
        assert!(
            sent_event.payload.get("payload").is_none(),
            "journal must carry metadata, never the message content"
        );

        let received_event = &decoded[1];
        assert_eq!(
            received_event.kind,
            synapse_core::AgentEventKind::MessageReceived
        );
        assert_eq!(
            received_event.session_id.as_deref(),
            Some("journal-recipient")
        );
        assert_eq!(received_event.payload["from_session"], "journal-sender");
        assert_eq!(
            received_event.payload["message_id"],
            sent.message_id.as_str()
        );
        Ok(())
    }

    #[test]
    fn unknown_or_stale_recipient_is_refused_without_storage_write() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        {
            let mut registry = service
                .session_registry_ref()
                .lock()
                .map_err(|_error| anyhow::anyhow!("session registry lock poisoned"))?;
            registry.set_stale_after(Some(Duration::from_millis(10)));
            registry.record_seen("sender", Some("test".to_owned()), 1_000);
            registry.record_seen("stale", Some("test".to_owned()), 1_000);
        }

        let error = match service.agent_send_impl_at(
            AgentSendParams {
                to_session: "missing".to_owned(),
                kind: "handoff".to_owned(),
                payload: json!({"n": 1}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            2_000,
        ) {
            Ok(response) => {
                anyhow::bail!("missing recipient send unexpectedly succeeded: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(error_code(&error), Some(error_codes::RECIPIENT_UNKNOWN));

        let error = match service.agent_send_impl_at(
            AgentSendParams {
                to_session: "stale".to_owned(),
                kind: "handoff".to_owned(),
                payload: json!({"n": 1}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            2_000,
        ) {
            Ok(response) => {
                anyhow::bail!("stale recipient send unexpectedly succeeded: {response:?}")
            }
            Err(error) => error,
        };
        assert_eq!(error_code(&error), Some(error_codes::RECIPIENT_UNKNOWN));

        let db = service.mailbox_db()?;
        assert!(
            db.scan_cf_prefix(cf::CF_KV, MAILBOX_PREFIX.as_bytes())?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn expired_messages_are_deleted_on_inbox_read() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "sender", 1_000)?;
        register_session(&service, "recipient", 1_000)?;
        service.agent_send_impl_at(
            AgentSendParams {
                to_session: "recipient".to_owned(),
                kind: "ttl".to_owned(),
                payload: json!({"ttl": 1}),
                artifact_handle: None,
                ttl_ms: 1,
                request_receipt: false,
            },
            "sender",
            2_000,
        )?;

        let read = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "recipient",
            2_002,
        )?;
        assert_eq!(read.returned_count, 0);
        assert_eq!(read.expired_rows_deleted, 1);
        assert_eq!(read.queue_depth_after, 0);
        Ok(())
    }

    #[test]
    fn expired_rows_for_other_recipients_are_cleaned_before_send() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "sender", 1_000)?;
        register_session(&service, "dead-recipient", 1_000)?;
        register_session(&service, "live-recipient", 1_000)?;
        service.agent_send_impl_at(
            AgentSendParams {
                to_session: "dead-recipient".to_owned(),
                kind: "ttl".to_owned(),
                payload: json!({"ttl": 1}),
                artifact_handle: None,
                ttl_ms: 1,
                request_receipt: false,
            },
            "sender",
            2_000,
        )?;

        let db = service.mailbox_db()?;
        assert_eq!(
            db.scan_cf_prefix(
                cf::CF_KV,
                mailbox_recipient_prefix("dead-recipient").as_bytes()
            )?
            .len(),
            1
        );

        service.agent_send_impl_at(
            AgentSendParams {
                to_session: "live-recipient".to_owned(),
                kind: "cleanup".to_owned(),
                payload: json!({"expected": "old recipient row removed"}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            2_002,
        )?;

        assert!(
            db.scan_cf_prefix(
                cf::CF_KV,
                mailbox_recipient_prefix("dead-recipient").as_bytes()
            )?
            .is_empty()
        );
        assert_eq!(
            db.scan_cf_prefix(
                cf::CF_KV,
                mailbox_recipient_prefix("live-recipient").as_bytes()
            )?
            .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn parameter_edges_fail_closed() {
        assert!(validate_kind("").is_err());
        assert!(validate_kind("line\nbreak").is_err());
        assert!(validate_ttl_ms(0).is_err());
        assert!(validate_inbox_params(0).is_err());
        assert!(
            validate_wait_params(&AgentWaitParams {
                timeout_ms: MAX_WAIT_TIMEOUT_MS + 1,
                drain: true,
                max_messages: 1,
                kinds: Vec::new(),
            })
            .is_err()
        );
        assert!(validate_payload_size(&json!({"blob": "x".repeat(MAX_PAYLOAD_BYTES)})).is_err());
    }

    #[tokio::test]
    async fn wait_timeout_returns_empty_without_deadlocking() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        register_session(&service, "recipient", 1_000)?;
        let waited = tokio::time::timeout(
            Duration::from_secs(5),
            service.agent_wait_impl(
                AgentWaitParams {
                    timeout_ms: 10,
                    drain: true,
                    max_messages: 10,
                    kinds: Vec::new(),
                },
                "recipient",
            ),
        )
        .await
        .expect("agent_wait timeout path should complete without deadlocking")?;
        assert!(waited.timed_out);
        assert_eq!(waited.inbox.returned_count, 0);
        Ok(())
    }

    #[test]
    fn mailbox_key_hexes_session_ids_with_slashes() {
        let key = mailbox_row_key("session/with/slash", 1, 2, "m");
        assert!(key.contains("73657373696f6e2f776974682f736c617368"));
        assert!(!key.contains("session/with/slash"));
    }

    #[test]
    fn registry_default_is_live_for_recent_record_seen() {
        let mut registry = SessionRegistry::default();
        registry.record_seen("recipient", Some("test".to_owned()), 1_000);
        let read = registry.reads(1_001).remove(0);
        assert_eq!(read.lifecycle, "live");
    }

    // ---- #908 mailbox v2 ----

    fn inbox_params(drain: bool, kinds: &[&str]) -> AgentInboxParams {
        AgentInboxParams {
            drain,
            max_messages: 100,
            kinds: kinds.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    fn broadcast_params(
        to: BroadcastTarget,
        kind: &str,
        request_receipt: bool,
    ) -> AgentSendBroadcastParams {
        AgentSendBroadcastParams {
            to,
            kind: kind.to_owned(),
            payload: json!({"hello": kind}),
            artifact_handle: None,
            ttl_ms: 60_000,
            request_receipt,
        }
    }

    #[test]
    fn broadcast_all_fans_out_one_row_per_live_recipient_excluding_sender() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 10_000;
        for who in ["sender", "a", "b", "c"] {
            register_session(&service, who, now)?;
        }

        let response = service.agent_send_broadcast_impl_at(
            broadcast_params(
                BroadcastTarget {
                    all: true,
                    agent_kinds: Vec::new(),
                    sessions: Vec::new(),
                },
                "finding",
                false,
            ),
            "sender",
            now,
        )?;
        assert_eq!(response.resolved_recipients, 3, "a/b/c, sender excluded");
        assert_eq!(response.delivered_count, 3);
        assert_eq!(response.skipped_count, 0);

        // Supporting regression readback: each recipient has exactly one
        // physical row; sender has none. Manual FSV remains separate.
        for who in ["a", "b", "c"] {
            let inbox = service.agent_inbox_impl_at(inbox_params(false, &[]), who, now + 1)?;
            assert_eq!(inbox.returned_count, 1, "recipient {who}");
            assert_eq!(inbox.messages[0].from_session, "sender");
            assert_eq!(inbox.messages[0].kind, "finding");
        }
        let sender_inbox =
            service.agent_inbox_impl_at(inbox_params(false, &[]), "sender", now + 1)?;
        assert_eq!(
            sender_inbox.returned_count, 0,
            "sender must not receive its own broadcast"
        );
        Ok(())
    }

    #[test]
    fn dashboard_broadcast_all_targets_only_live_spawned_agents() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = unix_time_ms_now();
        register_initialized_session(&service, "controller", "codex-mcp-client", now)?;
        register_session(&service, "ambient", now)?;
        register_spawned_session(
            &service,
            "spawned-codex-session",
            "codex",
            "agent-spawn-dashboard-codex",
            temp.path(),
            now,
        )?;
        register_spawned_session(
            &service,
            "spawned-claude-session",
            "claude",
            "agent-spawn-dashboard-claude",
            temp.path(),
            now,
        )?;

        let response = service.dashboard_agent_broadcast(
            "all".to_owned(),
            Vec::new(),
            Vec::new(),
            "steer".to_owned(),
            json!({"message": "dashboard-fleet-only"}),
            Some(60_000),
            true,
        )?;
        let recipients = response
            .get("recipients")
            .and_then(Value::as_array)
            .expect("recipients array");
        let to_sessions = recipients
            .iter()
            .map(|row| row.get("to_session").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>();
        assert_eq!(to_sessions.len(), 2);
        assert!(to_sessions.contains(&"spawned-codex-session"));
        assert!(to_sessions.contains(&"spawned-claude-session"));
        assert!(!to_sessions.contains(&"controller"));
        assert!(!to_sessions.contains(&"ambient"));

        for session in ["spawned-codex-session", "spawned-claude-session"] {
            let inbox = service.agent_inbox_impl_at(inbox_params(false, &[]), session, now + 1)?;
            assert_eq!(inbox.returned_count, 1, "spawned recipient {session}");
            assert_eq!(inbox.messages[0].from_session, "dashboard-fleet");
        }
        for session in ["controller", "ambient"] {
            let inbox = service.agent_inbox_impl_at(inbox_params(false, &[]), session, now + 1)?;
            assert_eq!(inbox.returned_count, 0, "non-fleet recipient {session}");
        }
        Ok(())
    }

    #[test]
    fn broadcast_writes_command_audit_intent_and_final_rows() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 11_000;
        for who in ["sender", "a", "b"] {
            register_session(&service, who, now)?;
        }

        let response = service.agent_send_broadcast_impl_at(
            broadcast_params(
                BroadcastTarget {
                    all: true,
                    agent_kinds: Vec::new(),
                    sessions: Vec::new(),
                },
                "steer",
                true,
            ),
            "sender",
            now,
        )?;
        assert_eq!(response.delivered_count, 2);

        let snapshot = service.command_audit_snapshot()?;
        assert!(
            snapshot.rows.iter().any(|row| {
                row.tool == "agent_send_broadcast"
                    && row.verb == "broadcast"
                    && row.phase == "intent"
                    && row.outcome == "pending"
                    && row.actor_session_id.as_deref() == Some("sender")
                    && row
                        .before
                        .as_ref()
                        .and_then(|v| v.get("resolved_recipients"))
                        == Some(&json!(2))
            }),
            "broadcast intent row should record the resolved recipient count before storage writes"
        );
        assert!(
            snapshot.rows.iter().any(|row| {
                row.tool == "agent_send_broadcast"
                    && row.verb == "broadcast"
                    && row.phase == "final"
                    && row.outcome == "ok"
                    && row.actor_session_id.as_deref() == Some("sender")
                    && row.after.as_ref().and_then(|v| v.get("delivered_count")) == Some(&json!(2))
                    && row
                        .after
                        .as_ref()
                        .and_then(|v| v.get("recipients"))
                        .is_some()
            }),
            "broadcast final row should record the physical delivery readback"
        );
        Ok(())
    }

    #[test]
    fn broadcast_agent_kinds_filter_discriminates() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 10_000;
        for who in ["sender", "a", "b"] {
            register_session(&service, who, now)?;
        }

        // record_seen yields agent_kind "unknown"; ["unknown"] matches both.
        let matched = service.agent_send_broadcast_impl_at(
            broadcast_params(
                BroadcastTarget {
                    all: false,
                    agent_kinds: vec!["unknown".to_owned()],
                    sessions: Vec::new(),
                },
                "steer",
                false,
            ),
            "sender",
            now,
        )?;
        assert_eq!(matched.delivered_count, 2);

        // A kind no live session has matches nobody — honest zero.
        let none = service.agent_send_broadcast_impl_at(
            broadcast_params(
                BroadcastTarget {
                    all: false,
                    agent_kinds: vec!["codex".to_owned()],
                    sessions: Vec::new(),
                },
                "steer",
                false,
            ),
            "sender",
            now,
        )?;
        assert_eq!(none.resolved_recipients, 0);
        assert_eq!(none.delivered_count, 0);
        Ok(())
    }

    #[test]
    fn broadcast_explicit_sessions_report_non_live_skips() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 10_000;
        register_session(&service, "sender", now)?;
        register_session(&service, "live-one", now)?;
        // "ghost" is never registered -> not live.

        let response = service.agent_send_broadcast_impl_at(
            broadcast_params(
                BroadcastTarget {
                    all: false,
                    agent_kinds: Vec::new(),
                    sessions: vec![
                        "live-one".to_owned(),
                        "ghost".to_owned(),
                        "live-one".to_owned(),
                        "sender".to_owned(),
                    ],
                },
                "finding",
                false,
            ),
            "sender",
            now,
        )?;
        // Only the live one is resolved; invalid explicit entries remain
        // visible as skipped outcomes instead of being silently dropped.
        assert_eq!(response.resolved_recipients, 1);
        assert_eq!(response.delivered_count, 1);
        assert_eq!(response.skipped_count, 3);
        assert!(response.recipients.iter().any(|row| {
            row.to_session == "live-one" && row.status == "delivered" && row.message_id.is_some()
        }));
        assert!(response.recipients.iter().any(|row| {
            row.to_session == "ghost"
                && row.status == "skipped"
                && row
                    .skip_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("not a live MCP session"))
        }));
        assert!(response.recipients.iter().any(|row| {
            row.to_session == "live-one"
                && row.status == "skipped"
                && row
                    .skip_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("duplicate"))
        }));
        assert!(response.recipients.iter().any(|row| {
            row.to_session == "sender"
                && row.status == "skipped"
                && row
                    .skip_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("sender is excluded"))
        }));
        let inbox = service.agent_inbox_impl_at(inbox_params(false, &[]), "live-one", now + 1)?;
        assert_eq!(inbox.returned_count, 1);
        let ghost_inbox =
            service.agent_inbox_impl_at(inbox_params(false, &[]), "ghost", now + 1)?;
        assert_eq!(ghost_inbox.returned_count, 0);
        Ok(())
    }

    #[test]
    fn broadcast_target_validation_rejects_zero_or_multiple_selectors() {
        let none = validate_broadcast_target(&BroadcastTarget::default());
        assert!(none.is_err(), "no selector must be rejected");
        let multi = validate_broadcast_target(&BroadcastTarget {
            all: true,
            agent_kinds: vec!["x".to_owned()],
            sessions: Vec::new(),
        });
        assert!(multi.is_err(), "two selectors must be rejected");
    }

    #[test]
    fn inbox_kind_filter_returns_exactly_matching_and_drains_only_those() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 20_000;
        register_session(&service, "sender", now)?;
        register_session(&service, "recipient", now)?;

        for (i, kind) in ["steer", "finding", "steer"].iter().enumerate() {
            service.agent_send_impl_at(
                AgentSendParams {
                    to_session: "recipient".to_owned(),
                    kind: (*kind).to_owned(),
                    payload: json!({"i": i}),
                    artifact_handle: None,
                    ttl_ms: 60_000,
                    request_receipt: false,
                },
                "sender",
                now + i as u64,
            )?;
        }

        // Drain only steer -> exactly the 2 steer messages, finding untouched.
        let steer =
            service.agent_inbox_impl_at(inbox_params(true, &["steer"]), "recipient", now + 10)?;
        assert_eq!(steer.returned_count, 2);
        assert!(steer.messages.iter().all(|m| m.kind == "steer"));
        assert_eq!(steer.deleted_count, 2);

        // The non-matching finding survived the filtered drain.
        let rest = service.agent_inbox_impl_at(inbox_params(false, &[]), "recipient", now + 11)?;
        assert_eq!(rest.returned_count, 1);
        assert_eq!(rest.messages[0].kind, "finding");
        Ok(())
    }

    #[test]
    fn read_receipt_is_written_to_sender_on_drain_and_readable() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 30_000;
        register_session(&service, "sender", now)?;
        register_session(&service, "recipient", now)?;

        let sent = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "recipient".to_owned(),
                kind: "task".to_owned(),
                payload: json!({"do": "the thing"}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: true,
            },
            "sender",
            now,
        )?;
        assert!(sent.request_receipt);

        // Before the recipient reads it, there is no receipt.
        let before = service.agent_receipts_impl_at(
            AgentReceiptsParams {
                drain: false,
                max_receipts: 10,
            },
            "sender",
            now + 1,
        )?;
        assert_eq!(
            before.returned_count, 0,
            "no receipt before the message is read"
        );

        // Recipient drains -> a read receipt lands in the sender's receipt box.
        let drained = service.agent_inbox_impl_at(inbox_params(true, &[]), "recipient", now + 2)?;
        assert_eq!(drained.returned_count, 1);

        let after = service.agent_receipts_impl_at(
            AgentReceiptsParams {
                drain: true,
                max_receipts: 10,
            },
            "sender",
            now + 3,
        )?;
        assert_eq!(
            after.returned_count, 1,
            "sender sees exactly one read receipt"
        );
        let receipt = &after.receipts[0];
        assert_eq!(receipt.recipient_session, "recipient");
        assert_eq!(receipt.message_id, sent.message_id);
        assert_eq!(receipt.message_kind, "task");
        assert_eq!(receipt.status, "read");
        assert_eq!(receipt.from_session, "sender");

        // Drained: the receipt box is now empty.
        let empty = service.agent_receipts_impl_at(
            AgentReceiptsParams {
                drain: false,
                max_receipts: 10,
            },
            "sender",
            now + 4,
        )?;
        assert_eq!(empty.returned_count, 0);
        Ok(())
    }

    #[test]
    fn receipt_rows_do_not_poison_message_cleanup_before_send() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 35_000;
        register_session(&service, "sender", now)?;
        register_session(&service, "receipt-recipient", now)?;
        register_session(&service, "live-recipient", now)?;

        service.agent_send_impl_at(
            AgentSendParams {
                to_session: "receipt-recipient".to_owned(),
                kind: "task".to_owned(),
                payload: json!({"receipt": true}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: true,
            },
            "sender",
            now,
        )?;

        let drained = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "receipt-recipient",
            now + 1,
        )?;
        assert_eq!(drained.returned_count, 1);

        let db = service.mailbox_db()?;
        assert_eq!(
            db.scan_cf_prefix(cf::CF_KV, RECEIPT_PREFIX.as_bytes())?
                .len(),
            1,
            "receipt row must remain present before the next send"
        );

        let sent = service.agent_send_impl_at(
            AgentSendParams {
                to_session: "live-recipient".to_owned(),
                kind: "after-receipt".to_owned(),
                payload: json!({"ok": true}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            now + 2,
        )?;
        assert_eq!(sent.to_session, "live-recipient");

        let live_inbox = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
                kinds: Vec::new(),
            },
            "live-recipient",
            now + 3,
        )?;
        assert_eq!(live_inbox.returned_count, 1);
        assert_eq!(live_inbox.messages[0].kind, "after-receipt");

        let receipts = service.agent_receipts_impl_at(
            AgentReceiptsParams {
                drain: true,
                max_receipts: 10,
            },
            "sender",
            now + 4,
        )?;
        assert_eq!(receipts.returned_count, 1);
        assert_eq!(receipts.receipts[0].message_kind, "task");
        Ok(())
    }

    #[test]
    fn no_receipt_written_when_not_requested() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service = service_with_db(temp.path())?;
        let now = 40_000;
        register_session(&service, "sender", now)?;
        register_session(&service, "recipient", now)?;
        service.agent_send_impl_at(
            AgentSendParams {
                to_session: "recipient".to_owned(),
                kind: "task".to_owned(),
                payload: json!({}),
                artifact_handle: None,
                ttl_ms: 60_000,
                request_receipt: false,
            },
            "sender",
            now,
        )?;
        service.agent_inbox_impl_at(inbox_params(true, &[]), "recipient", now + 1)?;
        let receipts = service.agent_receipts_impl_at(
            AgentReceiptsParams {
                drain: false,
                max_receipts: 10,
            },
            "sender",
            now + 2,
        )?;
        assert_eq!(
            receipts.returned_count, 0,
            "no receipt without request_receipt"
        );
        Ok(())
    }
}
