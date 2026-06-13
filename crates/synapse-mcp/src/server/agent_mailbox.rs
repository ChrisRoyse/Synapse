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
const MAILBOX_PREFIX: &str = "agent-mailbox/v1";
const DEFAULT_MESSAGE_TTL_MS: u64 = 5 * 60 * 1000;
const MAX_MESSAGE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const DEFAULT_MAX_MESSAGES: usize = 100;
const MAX_MESSAGES_PER_READ: usize = 1000;
const MAX_PAYLOAD_BYTES: usize = 65_536;
const MAX_KIND_CHARS: usize = 128;
const MAX_ARTIFACT_HANDLE_CHARS: usize = 1024;
const MAX_INBOX_ROWS_PER_RECIPIENT: usize = 10_000;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 1000;
const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;

static NEXT_MAILBOX_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSendParams {
    /// Live recipient MCP Streamable HTTP session id.
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

#[derive(Clone)]
struct DecodedMailboxRow {
    key: Vec<u8>,
    encoded: Vec<u8>,
    message: AgentMailboxMessage,
}

#[tool_router(router = agent_mailbox_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Send a bounded durable JSON message to a live MCP peer session. Fails with RECIPIENT_UNKNOWN for stale/closed/unknown recipients instead of queueing to nowhere. The message is persisted under CF_KV and returned with an exact row readback."
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
}

impl SynapseService {
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
        let recipient = self.recipient_live_read(from_session, &params.to_session, now_unix_ms)?;
        let depth_before = queue_depth_for_recipient(&db, &params.to_session, now_unix_ms)?;
        if depth_before >= MAX_INBOX_ROWS_PER_RECIPIENT {
            return Err(mailbox_full_error(
                from_session,
                &params.to_session,
                depth_before,
            ));
        }

        let seq = NEXT_MAILBOX_SEQ.fetch_add(1, Ordering::Relaxed);
        let message_id = format!("agentmsg-{now_unix_ms:020}-{seq:020}");
        let row_key = mailbox_row_key(&params.to_session, now_unix_ms, seq, &message_id);
        let message = AgentMailboxMessage {
            schema_version: SCHEMA_VERSION,
            message_id: message_id.clone(),
            row_key: row_key.clone(),
            from_session: from_session.to_owned(),
            to_session: params.to_session.clone(),
            kind: params.kind.trim().to_owned(),
            payload: params.payload,
            artifact_handle: params.artifact_handle.map(|value| value.trim().to_owned()),
            sent_at_unix_ms: now_unix_ms,
            ttl_ms: params.ttl_ms,
            expires_at_unix_ms: now_unix_ms.saturating_add(params.ttl_ms),
            delivery_attempts: 0,
        };
        let encoded = encode_mailbox_message(&message)?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("write agent mailbox row {}: {error}", row_key),
                )
            })?;
        let storage_readback = readback_exact_mailbox_row(&db, &row_key)?;
        let queue_depth_after = queue_depth_for_recipient(&db, &params.to_session, now_unix_ms)?;

        // Journal the delivery fact (#897). The mailbox row is already
        // committed, so a journal failure is surfaced with that context.
        let mut journal_record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::MessageSent,
        );
        journal_record.session_id = Some(from_session.to_owned());
        journal_record.attributes.conversation_id = Some(from_session.to_owned());
        journal_record.payload = json!({
            "to_session": &params.to_session,
            "message_id": &message_id,
            "message_kind": &message.kind,
            "payload_bytes": storage_readback.value_len_bytes,
            "value_sha256": &storage_readback.value_sha256,
            "expires_at_unix_ms": message.expires_at_unix_ms,
        });
        super::agent_events::record_agent_event(&db, &journal_record).map_err(|error| {
            super::agent_events::agent_event_tool_error("agent_send", &error, true)
        })?;

        tracing::info!(
            code = "AGENT_MAILBOX_SEND_COMMITTED",
            from_session,
            to_session = %params.to_session,
            recipient_lifecycle = %recipient.lifecycle,
            message_id,
            row_key,
            queue_depth_after,
            expired_rows_deleted_before,
            value_sha256 = %storage_readback.value_sha256,
            "readback=agent_mailbox edge=send_committed"
        );

        Ok(AgentSendResponse {
            ok: true,
            message_id,
            from_session: from_session.to_owned(),
            to_session: params.to_session,
            kind: message.kind,
            row_key,
            sent_at_unix_ms: now_unix_ms,
            expires_at_unix_ms: message.expires_at_unix_ms,
            queue_depth_after,
            storage_readback,
        })
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
    ) -> Result<SessionRegistryRead, ErrorData> {
        validate_session_id(to_session)?;
        let recipient = {
            let guard = self.session_registry_ref().lock().map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "session registry lock poisoned while validating mailbox recipient",
                )
            })?;
            guard
                .reads(now_unix_ms)
                .into_iter()
                .find(|entry| entry.session_id == to_session)
        };
        match recipient {
            Some(read) if read.lifecycle == "live" => Ok(read),
            other => Err(recipient_unknown_error(
                from_session,
                to_session,
                other.as_ref(),
            )),
        }
    }
}

impl From<&AgentWaitParams> for AgentInboxParams {
    fn from(value: &AgentWaitParams) -> Self {
        Self {
            drain: value.drain,
            max_messages: value.max_messages,
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
        .scan_cf_prefix(cf::CF_KV, MAILBOX_PREFIX.as_bytes())
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
    validate_inbox_params(params.max_messages)
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
    format!(
        "{MAILBOX_PREFIX}/recipient_hex/{}/msg/",
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

    use serde_json::json;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig,
        server::session_registry::SessionRegistry,
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
            },
            "sender",
            2_001,
        )?;

        let peek = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: false,
                max_messages: 10,
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
            },
            "recipient",
            2_030,
        )?;
        assert_eq!(after.returned_count, 0);
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
            },
            "journal-sender",
            2_000,
        )?;
        let drained = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
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
            },
            "sender",
            2_000,
        )?;

        let read = service.agent_inbox_impl_at(
            AgentInboxParams {
                drain: true,
                max_messages: 10,
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
}
