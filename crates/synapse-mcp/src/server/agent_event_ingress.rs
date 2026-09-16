//! Push-telemetry ingress for spawned agents (#899).
//!
//! `POST /agent-events?spawn_id=<agent-spawn-*>&source=<sender>` receives the
//! sender's NATIVE payload — Claude Code hook events (delivered by the CLI's
//! built-in `type: "http"` hooks) or the Codex `notify` program payload — and
//! normalizes it into one typed [`AgentEventRecord`] row in `CF_AGENT_EVENTS`.
//! Normalization happens here, at the trusted boundary, so senders stay dumb
//! and the daemon owns the event vocabulary.
//!
//! # Refusal contract (#899 acceptance)
//!
//! Nothing is dropped silently. Every refusal increments a counter exposed by
//! [`ingress_stats`], logs a structured `AGENT_EVENT_INGRESS_REJECTED` entry,
//! and returns a structured HTTP error to the sender:
//!
//! - unknown/forged `spawn_id` (no spawn directory issued by
//!   `act_spawn_agent`) → 404
//! - malformed payloads, unsubscribed hook events, unknown notification or
//!   notify types → 422 (an unsubscribed event arriving here means the
//!   injected hook config and this validator drifted — that must surface)
//! - journal write failures → 500 (the storage layer also logs
//!   `AGENT_EVENT_WRITE_FAILED`)
//!
//! # Content policy
//!
//! Prompts, tool inputs/outputs, and assistant messages are CONTENT and are
//! never stored (OTel GenAI conventions mark them opt-in/sensitive; #897
//! journal rows are bounded metadata). The ingress records byte counts and
//! sha256 digests instead, so rows stay within the 16 KiB journal cap no
//! matter what the agent did.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};
use synapse_core::{AgentEndState, AgentEventRecord, GenAiOperationName};
use synapse_storage::{Db, StorageError};

use super::agent_events::{AgentEventWriteReadback, record_agent_event, unix_time_ns_now};
use super::agent_mailbox::hash_bytes;

/// Claude Code hook events the spawn-injected settings subscribe to. The
/// settings generator in `m4_tools` and the validator allowlist both read
/// this constant so they can never drift apart.
pub(crate) const CLAUDE_HOOK_SUBSCRIBED_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionDenied",
    "Notification",
    "Stop",
    "SessionEnd",
    "TaskCreated",
    "TaskCompleted",
];

/// `notification_type` values from the Claude Code hooks reference. An
/// unknown value is refused loudly: it cannot be mapped to an attention
/// state, and silently guessing would corrupt the needs-input signal.
const CLAUDE_NOTIFICATION_TYPES: &[&str] = &[
    "permission_prompt",
    "idle_prompt",
    "auth_success",
    "elicitation_dialog",
    "elicitation_complete",
    "elicitation_response",
];

/// Notification types that mean the agent is blocked on a human.
const NEEDS_INPUT_NOTIFICATION_TYPES: &[&str] =
    &["permission_prompt", "idle_prompt", "elicitation_dialog"];

/// Spawn ids are daemon-issued (`agent-spawn-<reflex-id>`); anything longer
/// is fabricated.
const MAX_SPAWN_ID_CHARS: usize = 128;

/// Request bodies are sender-native payloads; `tool_input` can carry whole
/// file contents (e.g. a `Write` call), so the cap is generous. The journal
/// row produced from it stays bounded regardless.
pub(crate) const MAX_AGENT_EVENT_INGRESS_BODY_BYTES: usize = 8 * 1024 * 1024;

static ACCEPTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REJECTED_UNKNOWN_SPAWN_TOTAL: AtomicU64 = AtomicU64::new(0);
static REJECTED_MALFORMED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REJECTED_STORAGE_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Validated query identity of one ingress request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentEventIngressIdentity {
    pub spawn_id: String,
    pub source: AgentEventIngressSource,
}

/// Who is POSTing and therefore which native schema the body must satisfy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentEventIngressSource {
    ClaudeCodeHooks,
    CodexNotify,
    LocalModelRunner,
}

impl AgentEventIngressSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCodeHooks => "claude_code_hooks",
            Self::CodexNotify => "codex_notify",
            Self::LocalModelRunner => "local_model_runner",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude_code_hooks" => Some(Self::ClaudeCodeHooks),
            "codex_notify" => Some(Self::CodexNotify),
            "local_model_runner" => Some(Self::LocalModelRunner),
            _ => None,
        }
    }
}

/// A counted, logged, structured refusal. Construction increments the
/// matching counter exactly once.
#[derive(Clone, Debug)]
pub(crate) struct AgentEventIngressRefusal {
    pub http_status: u16,
    pub code: &'static str,
    pub detail: String,
}

impl AgentEventIngressRefusal {
    fn unknown_spawn(detail: String) -> Self {
        REJECTED_UNKNOWN_SPAWN_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self {
            http_status: 404,
            code: "AGENT_EVENT_INGRESS_UNKNOWN_SPAWN",
            detail,
        }
    }

    fn malformed(detail: String) -> Self {
        REJECTED_MALFORMED_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self {
            http_status: 422,
            code: "AGENT_EVENT_INGRESS_MALFORMED",
            detail,
        }
    }

    fn storage(error: &StorageError) -> Self {
        REJECTED_STORAGE_TOTAL.fetch_add(1, Ordering::Relaxed);
        Self {
            http_status: 500,
            code: "AGENT_EVENT_INGRESS_WRITE_FAILED",
            detail: format!("journal write failed: {error}"),
        }
    }

    pub(crate) fn response_body(&self) -> Value {
        json!({
            "accepted": false,
            "code": self.code,
            "detail": self.detail,
        })
    }
}

/// Snapshot of the ingress counters (process lifetime). The acceptance
/// criterion "no silent drops" is auditable here and on `/agent-events/stats`.
pub(crate) fn ingress_stats() -> Value {
    json!({
        "accepted_total": ACCEPTED_TOTAL.load(Ordering::Relaxed),
        "rejected_unknown_spawn_total": REJECTED_UNKNOWN_SPAWN_TOTAL.load(Ordering::Relaxed),
        "rejected_malformed_total": REJECTED_MALFORMED_TOTAL.load(Ordering::Relaxed),
        "rejected_storage_total": REJECTED_STORAGE_TOTAL.load(Ordering::Relaxed),
    })
}

/// Validates the query-string identity of an ingress request.
///
/// # Errors
///
/// Refuses (422) missing/duplicate-free-form params and malformed spawn ids
/// before any filesystem probe, and (404) spawn ids that `act_spawn_agent`
/// never issued — the spawn log directory is created before launch, so a
/// legitimate sender always has one.
pub(crate) fn validate_ingress_identity(
    query_pairs: &[(String, String)],
) -> Result<AgentEventIngressIdentity, AgentEventIngressRefusal> {
    let mut spawn_id: Option<&str> = None;
    let mut source: Option<&str> = None;
    for (key, value) in query_pairs {
        match key.as_str() {
            "spawn_id" => {
                if spawn_id.replace(value.as_str()).is_some() {
                    return Err(refuse_malformed(
                        None,
                        "query parameter spawn_id appears more than once".to_owned(),
                    ));
                }
            }
            "source" => {
                if source.replace(value.as_str()).is_some() {
                    return Err(refuse_malformed(
                        None,
                        "query parameter source appears more than once".to_owned(),
                    ));
                }
            }
            other => {
                return Err(refuse_malformed(
                    None,
                    format!("unexpected query parameter: {other}"),
                ));
            }
        }
    }
    let Some(spawn_id) = spawn_id else {
        return Err(refuse_malformed(
            None,
            "query parameter spawn_id is required".to_owned(),
        ));
    };
    let Some(source_raw) = source else {
        return Err(refuse_malformed(
            Some(spawn_id),
            "query parameter source is required".to_owned(),
        ));
    };
    let Some(source) = AgentEventIngressSource::parse(source_raw) else {
        return Err(refuse_malformed(
            Some(spawn_id),
            format!(
                "unknown source {source_raw:?}; expected claude_code_hooks, codex_notify, or local_model_runner"
            ),
        ));
    };
    validate_spawn_id_shape(spawn_id).map_err(|detail| refuse_malformed(Some(spawn_id), detail))?;
    let spawn_dir = super::m4_tools::agent_spawn_root_dir()
        .map_err(|error| {
            // Daemon-side misconfiguration (e.g. LOCALAPPDATA unset), not a
            // sender fault: surface as 500, still counted and logged.
            let mut refusal = AgentEventIngressRefusal::malformed(format!(
                "agent spawn root unavailable: {}",
                error.message
            ));
            refusal.http_status = 500;
            log_refusal(Some(spawn_id), &refusal);
            refusal
        })?
        .join(spawn_id);
    if !spawn_dir.is_dir() {
        let refusal = AgentEventIngressRefusal::unknown_spawn(format!(
            "spawn_id {spawn_id:?} was never issued by act_spawn_agent on this daemon (no spawn directory at {})",
            spawn_dir.display()
        ));
        log_refusal(Some(spawn_id), &refusal);
        return Err(refusal);
    }
    Ok(AgentEventIngressIdentity {
        spawn_id: spawn_id.to_owned(),
        source,
    })
}

/// Normalizes and journals one sender-native payload. Returns the physical
/// write readback (`ts_ns`, `seq`) so the HTTP response can prove the row.
///
/// # Errors
///
/// Refuses malformed bodies (422) and journal failures (500); both are
/// counted and logged.
pub(crate) fn ingest_agent_event(
    db: &Db,
    identity: &AgentEventIngressIdentity,
    body: &[u8],
) -> Result<(AgentEventWriteReadback, AgentEventRecord), AgentEventIngressRefusal> {
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        refuse_malformed(
            Some(&identity.spawn_id),
            format!("body is not valid JSON: {error}"),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        refuse_malformed(
            Some(&identity.spawn_id),
            "body must be a JSON object".to_owned(),
        )
    })?;
    let record = match identity.source {
        AgentEventIngressSource::ClaudeCodeHooks => {
            normalize_claude_hook_event(&identity.spawn_id, object)
        }
        AgentEventIngressSource::CodexNotify => {
            normalize_codex_notify_event(&identity.spawn_id, object)
        }
        AgentEventIngressSource::LocalModelRunner => {
            normalize_local_model_runner_event(&identity.spawn_id, object)
        }
    }
    .map_err(|detail| refuse_malformed(Some(&identity.spawn_id), detail))?;
    let readback = record_agent_event(db, &record).map_err(|error| {
        let refusal = AgentEventIngressRefusal::storage(&error);
        log_refusal(Some(&identity.spawn_id), &refusal);
        refusal
    })?;
    ACCEPTED_TOTAL.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        code = "AGENT_EVENT_INGRESS_ACCEPTED",
        spawn_id = %identity.spawn_id,
        source = identity.source.as_str(),
        kind = ?record.kind,
        ts_ns = readback.ts_ns,
        seq = readback.seq,
        "readback=CF_AGENT_EVENTS edge=ingress"
    );
    Ok((readback, record))
}

/// Counted refusal for a body that could not be read within the size cap
/// (HTTP 413). Called by the transport layer so even transport-level drops
/// hit the rejection counters.
pub(crate) fn refuse_oversized_or_unreadable_body(
    spawn_id: &str,
    detail: &str,
) -> AgentEventIngressRefusal {
    let mut refusal = AgentEventIngressRefusal::malformed(format!(
        "request body unreadable or larger than {MAX_AGENT_EVENT_INGRESS_BODY_BYTES} bytes: {detail}"
    ));
    refusal.http_status = 413;
    log_refusal(Some(spawn_id), &refusal);
    refusal
}

fn refuse_malformed(spawn_id: Option<&str>, detail: String) -> AgentEventIngressRefusal {
    let refusal = AgentEventIngressRefusal::malformed(detail);
    log_refusal(spawn_id, &refusal);
    refusal
}

fn log_refusal(spawn_id: Option<&str>, refusal: &AgentEventIngressRefusal) {
    tracing::error!(
        code = "AGENT_EVENT_INGRESS_REJECTED",
        refusal_code = refusal.code,
        http_status = refusal.http_status,
        spawn_id = ?spawn_id,
        detail = %refusal.detail,
        "agent-event ingress refused a request"
    );
}

fn validate_spawn_id_shape(spawn_id: &str) -> Result<(), String> {
    if !spawn_id.starts_with("agent-spawn-") {
        return Err(format!(
            "spawn_id must start with \"agent-spawn-\", got {spawn_id:?}"
        ));
    }
    if spawn_id.len() > MAX_SPAWN_ID_CHARS {
        return Err(format!(
            "spawn_id exceeds {MAX_SPAWN_ID_CHARS} chars ({})",
            spawn_id.len()
        ));
    }
    if !spawn_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(
            "spawn_id must contain only ASCII alphanumerics and dashes (path-safety invariant)"
                .to_owned(),
        );
    }
    Ok(())
}

fn normalize_claude_hook_event(
    spawn_id: &str,
    object: &Map<String, Value>,
) -> Result<AgentEventRecord, String> {
    use synapse_core::AgentEventKind as Kind;

    let event_name = required_str(object, "hook_event_name")?;
    if !CLAUDE_HOOK_SUBSCRIBED_EVENTS.contains(&event_name) {
        return Err(format!(
            "hook_event_name {event_name:?} is not in the subscribed set {CLAUDE_HOOK_SUBSCRIBED_EVENTS:?}; \
             the injected hook settings and this validator drifted"
        ));
    }
    let cli_session_id = required_str(object, "session_id")?;
    let mut record = AgentEventRecord::new(unix_time_ns_now(), Kind::StateChanged);
    record.spawn_id = Some(spawn_id.to_owned());
    record.attributes.provider_name = Some("anthropic".to_owned());
    // The hook's session_id is the Claude CLI conversation UUID, not an MCP
    // session id, so it travels as gen_ai.conversation.id.
    record.attributes.conversation_id = Some(cli_session_id.to_owned());
    let mut payload = Map::new();

    match event_name {
        "SessionStart" => {
            record.reason_code = Some("cli_session_start".to_owned());
            payload.insert(
                "source".to_owned(),
                Value::from(required_str(object, "source")?),
            );
            insert_optional_str(&mut payload, object, "transcript_path");
        }
        "UserPromptSubmit" => {
            record.kind = Kind::TurnStarted;
            let prompt = required_str(object, "prompt")?;
            insert_content_digest(&mut payload, "prompt", prompt.as_bytes());
            insert_optional_str(&mut payload, object, "transcript_path");
        }
        "PreToolUse" => {
            record.kind = Kind::ToolCallStarted;
            apply_tool_call_common(&mut record, &mut payload, object)?;
        }
        "PostToolUse" => {
            record.kind = Kind::ToolCallFinished;
            apply_tool_call_common(&mut record, &mut payload, object)?;
            insert_value_digest(&mut payload, object, "tool_response");
            if let Some(duration_ms) = object.get("duration_ms").and_then(Value::as_u64) {
                payload.insert("duration_ms".to_owned(), Value::from(duration_ms));
            }
        }
        "PostToolUseFailure" => {
            record.kind = Kind::ToolCallFinished;
            apply_tool_call_common(&mut record, &mut payload, object)?;
            record.attributes.error_type = Some("tool_failure".to_owned());
            insert_value_digest(&mut payload, object, "error");
        }
        "PermissionRequest" => {
            record.reason_code = Some("permission_request".to_owned());
            record.state_to = Some("awaiting_approval".to_owned());
            record.attributes.tool_name = Some(required_str(object, "tool_name")?.to_owned());
        }
        "PermissionDenied" => {
            record.reason_code = Some("permission_denied".to_owned());
            record.attributes.tool_name = Some(required_str(object, "tool_name")?.to_owned());
        }
        "Notification" => {
            let notification_type = required_str(object, "notification_type")?;
            if !CLAUDE_NOTIFICATION_TYPES.contains(&notification_type) {
                return Err(format!(
                    "notification_type {notification_type:?} is not a known Claude Code value \
                     {CLAUDE_NOTIFICATION_TYPES:?}; refusing rather than guessing its attention semantics"
                ));
            }
            record.reason_code = Some(notification_type.to_owned());
            if NEEDS_INPUT_NOTIFICATION_TYPES.contains(&notification_type) {
                record.state_to = Some("needs_input".to_owned());
            }
            if let Some(message) = object.get("message").and_then(Value::as_str) {
                insert_content_digest(&mut payload, "message", message.as_bytes());
            }
        }
        "Stop" => {
            record.kind = Kind::TurnFinished;
            let stop_hook_active = object
                .get("stop_hook_active")
                .and_then(Value::as_bool)
                .ok_or("Stop event requires boolean stop_hook_active")?;
            payload.insert("stop_hook_active".to_owned(), Value::from(stop_hook_active));
            if let Some(message) = object.get("last_assistant_message").and_then(Value::as_str) {
                insert_content_digest(&mut payload, "last_assistant_message", message.as_bytes());
            }
        }
        "SessionEnd" => {
            record.reason_code = Some("cli_session_end".to_owned());
            payload.insert(
                "reason".to_owned(),
                Value::from(required_str(object, "reason")?),
            );
        }
        "TaskCreated" => {
            record.reason_code = Some("task_created".to_owned());
        }
        "TaskCompleted" => {
            record.reason_code = Some("task_completed".to_owned());
        }
        other => {
            // Unreachable while the allowlist check above is exhaustive; keep
            // the refusal so a future allowlist addition cannot fall through
            // to a silently empty mapping.
            return Err(format!(
                "subscribed event {other:?} has no normalization arm"
            ));
        }
    }

    if !payload.is_empty() {
        record.payload = Value::Object(payload);
    }
    Ok(record)
}

fn apply_tool_call_common(
    record: &mut AgentEventRecord,
    payload: &mut Map<String, Value>,
    object: &Map<String, Value>,
) -> Result<(), String> {
    record.attributes.operation_name = Some(GenAiOperationName::ExecuteTool);
    record.attributes.tool_name = Some(required_str(object, "tool_name")?.to_owned());
    if let Some(tool_use_id) = object.get("tool_use_id").and_then(Value::as_str) {
        record.attributes.tool_call_id = Some(tool_use_id.to_owned());
    }
    insert_value_digest(payload, object, "tool_input");
    Ok(())
}

fn normalize_codex_notify_event(
    spawn_id: &str,
    object: &Map<String, Value>,
) -> Result<AgentEventRecord, String> {
    let notify_type = required_str(object, "type")?;
    if notify_type != "agent-turn-complete" {
        return Err(format!(
            "codex notify type {notify_type:?} is not supported; expected \"agent-turn-complete\""
        ));
    }
    let thread_id = required_str(object, "thread-id")?;
    let turn_id = required_str(object, "turn-id")?;
    let mut record = AgentEventRecord::new(
        unix_time_ns_now(),
        synapse_core::AgentEventKind::TurnFinished,
    );
    record.spawn_id = Some(spawn_id.to_owned());
    record.attributes.provider_name = Some("openai".to_owned());
    record.attributes.conversation_id = Some(thread_id.to_owned());
    let mut payload = Map::new();
    payload.insert("turn_id".to_owned(), Value::from(turn_id));
    if let Some(input_messages) = object.get("input-messages").and_then(Value::as_array) {
        payload.insert(
            "input_message_count".to_owned(),
            Value::from(input_messages.len()),
        );
    }
    if let Some(message) = object.get("last-assistant-message").and_then(Value::as_str) {
        insert_content_digest(&mut payload, "last_assistant_message", message.as_bytes());
    }
    if let Some(client) = object.get("client").and_then(Value::as_str) {
        payload.insert("client".to_owned(), Value::from(client));
    }
    record.payload = Value::Object(payload);
    Ok(record)
}

fn normalize_local_model_runner_event(
    spawn_id: &str,
    object: &Map<String, Value>,
) -> Result<AgentEventRecord, String> {
    use synapse_core::AgentEventKind as Kind;

    let event_name = required_str(object, "event")?;
    let mut record = AgentEventRecord::new(unix_time_ns_now(), Kind::StateChanged);
    record.spawn_id = Some(spawn_id.to_owned());
    if let Some(session_id) = object
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        record.session_id = Some(session_id.to_owned());
    }
    record.attributes.provider_name = Some("local".to_owned());
    record.attributes.agent_name = Some("synapse-local-model-agent".to_owned());
    if let Some(conversation_id) = object
        .get("conversation_id")
        .and_then(Value::as_str)
        .or_else(|| object.get("session_id").and_then(Value::as_str))
    {
        record.attributes.conversation_id = Some(conversation_id.to_owned());
    }
    if let Some(model) = object.get("model").and_then(Value::as_str) {
        record.attributes.request_model = Some(model.to_owned());
        record.attributes.response_model = Some(model.to_owned());
    }

    let mut payload = Map::new();
    insert_optional_str(&mut payload, object, "registry_name");
    if let Some(turn_index) = object.get("turn_index").and_then(Value::as_u64) {
        payload.insert("turn_index".to_owned(), Value::from(turn_index));
    }

    match event_name {
        "state_changed" => {
            record.kind = Kind::StateChanged;
            insert_optional_str(&mut payload, object, "state_from");
            if let Some(state_from) = object.get("state_from").and_then(Value::as_str) {
                record.state_from = Some(state_from.to_owned());
            }
            if let Some(state_to) = object.get("state_to").and_then(Value::as_str) {
                record.state_to = Some(state_to.to_owned());
            }
            if let Some(reason) = object.get("reason_code").and_then(Value::as_str) {
                record.reason_code = Some(reason.to_owned());
            }
            for field in ["before_chars", "after_chars", "limit_chars", "tool_count"] {
                if let Some(value) = object.get(field).and_then(Value::as_u64) {
                    payload.insert(field.to_owned(), Value::from(value));
                }
            }
        }
        "turn_started" => {
            record.kind = Kind::TurnStarted;
            record.attributes.operation_name = Some(GenAiOperationName::Chat);
            record.reason_code = Some("local_turn_started".to_owned());
        }
        "turn_finished" => {
            record.kind = Kind::TurnFinished;
            record.attributes.operation_name = Some(GenAiOperationName::Chat);
            record.reason_code = Some("local_turn_finished".to_owned());
            apply_local_usage(&mut record, &mut payload, object)?;
            insert_optional_str(&mut payload, object, "finish_reason");
        }
        "tool_call_started" => {
            record.kind = Kind::ToolCallStarted;
            record.attributes.operation_name = Some(GenAiOperationName::ExecuteTool);
            record.attributes.tool_name = Some(required_str(object, "tool_name")?.to_owned());
            if let Some(call_id) = object.get("tool_call_id").and_then(Value::as_str) {
                record.attributes.tool_call_id = Some(call_id.to_owned());
            }
            insert_value_digest(&mut payload, object, "tool_arguments");
        }
        "tool_call_finished" => {
            record.kind = Kind::ToolCallFinished;
            record.attributes.operation_name = Some(GenAiOperationName::ExecuteTool);
            record.attributes.tool_name = Some(required_str(object, "tool_name")?.to_owned());
            if let Some(call_id) = object.get("tool_call_id").and_then(Value::as_str) {
                record.attributes.tool_call_id = Some(call_id.to_owned());
            }
            if let Some(error_code) = object
                .get("error_code")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                record.attributes.error_type = Some(error_code.to_owned());
                record.reason_code = Some(error_code.to_owned());
            }
            insert_value_digest(&mut payload, object, "tool_response");
        }
        "exited" => {
            record.kind = Kind::Exited;
            record.end_state = Some(match object.get("end_state").and_then(Value::as_str) {
                Some("success") => AgentEndState::Success,
                Some("error") => AgentEndState::Error,
                _ => AgentEndState::Indeterminate,
            });
            if let Some(reason) = object.get("reason_code").and_then(Value::as_str) {
                record.reason_code = Some(reason.to_owned());
            }
            if let Some(error_code) = object
                .get("error_code")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                record.attributes.error_type = Some(error_code.to_owned());
            }
        }
        other => {
            return Err(format!(
                "local model runner event {other:?} is not supported"
            ));
        }
    }

    if !payload.is_empty() {
        record.payload = Value::Object(payload);
    }
    Ok(record)
}

fn apply_local_usage(
    record: &mut AgentEventRecord,
    payload: &mut Map<String, Value>,
    object: &Map<String, Value>,
) -> Result<(), String> {
    let usage = object
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(|| "local turn_finished requires object usage".to_owned())?;
    record.attributes.usage_input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64);
    record.attributes.usage_output_tokens = usage.get("completion_tokens").and_then(Value::as_u64);
    if let Some(total) = usage.get("total_tokens").and_then(Value::as_u64) {
        payload.insert("usage_total_tokens".to_owned(), Value::from(total));
    }
    Ok(())
}

fn required_str<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("required string field {field:?} is missing or empty"))
}

fn insert_optional_str(payload: &mut Map<String, Value>, object: &Map<String, Value>, field: &str) {
    if let Some(value) = object.get(field).and_then(Value::as_str) {
        payload.insert(field.to_owned(), Value::from(value));
    }
}

/// Records `<name>_bytes` + `<name>_sha256` for content the journal must
/// never store verbatim.
fn insert_content_digest(payload: &mut Map<String, Value>, name: &str, content: &[u8]) {
    payload.insert(format!("{name}_bytes"), Value::from(content.len()));
    payload.insert(format!("{name}_sha256"), Value::from(hash_bytes(content)));
}

/// Digest of an arbitrary JSON field (object/array/scalar), measured over its
/// canonical serde_json serialization.
fn insert_value_digest(payload: &mut Map<String, Value>, object: &Map<String, Value>, field: &str) {
    let Some(value) = object.get(field) else {
        return;
    };
    match serde_json::to_vec(value) {
        Ok(encoded) => insert_content_digest(payload, field, &encoded),
        Err(error) => {
            // serde_json::Value always serializes; reaching this is a serde
            // invariant break worth surfacing, not hiding.
            payload.insert(
                format!("{field}_digest_error"),
                Value::from(format!("{error}")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use synapse_core::AgentEventKind;

    use super::*;

    const SPAWN_ID: &str = "agent-spawn-ingress-test";

    /// Captured verbatim from Claude Code 2.1.176 HTTP hooks on 2026-06-12
    /// (probe run in C:\code\hookprobe); real data, not mocks.
    const REAL_USER_PROMPT_SUBMIT: &str = r#"{"session_id":"75346593-bb30-4162-ac6d-4e548101f7b8","transcript_path":"C:\\Users\\hotra\\.claude\\projects\\C--code-hookprobe\\75346593-bb30-4162-ac6d-4e548101f7b8.jsonl","cwd":"C:\\code\\hookprobe","permission_mode":"default","hook_event_name":"UserPromptSubmit","prompt":"Use the Bash tool to run: echo probe-marker-42. Then reply done.\r\n"}"#;
    const REAL_PRE_TOOL_USE: &str = r#"{"session_id":"75346593-bb30-4162-ac6d-4e548101f7b8","transcript_path":"C:\\Users\\hotra\\.claude\\projects\\C--code-hookprobe\\75346593-bb30-4162-ac6d-4e548101f7b8.jsonl","cwd":"C:\\code\\hookprobe","permission_mode":"default","effort":{"level":"high"},"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo probe-marker-42","description":"Echo probe marker"},"tool_use_id":"toolu_01HwXsLynP3Wh9b6VPBQWLcf"}"#;
    const REAL_POST_TOOL_USE: &str = r#"{"session_id":"75346593-bb30-4162-ac6d-4e548101f7b8","transcript_path":"C:\\Users\\hotra\\.claude\\projects\\C--code-hookprobe\\75346593-bb30-4162-ac6d-4e548101f7b8.jsonl","cwd":"C:\\code\\hookprobe","permission_mode":"default","effort":{"level":"high"},"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"echo probe-marker-42","description":"Echo probe marker"},"tool_response":{"stdout":"probe-marker-42","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false},"tool_use_id":"toolu_01HwXsLynP3Wh9b6VPBQWLcf","duration_ms":8668}"#;
    const REAL_STOP: &str = r#"{"session_id":"75346593-bb30-4162-ac6d-4e548101f7b8","transcript_path":"C:\\Users\\hotra\\.claude\\projects\\C--code-hookprobe\\75346593-bb30-4162-ac6d-4e548101f7b8.jsonl","cwd":"C:\\code\\hookprobe","permission_mode":"default","effort":{"level":"high"},"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"done","background_tasks":[],"session_crons":[]}"#;
    const REAL_SESSION_END: &str = r#"{"session_id":"75346593-bb30-4162-ac6d-4e548101f7b8","transcript_path":"C:\\Users\\hotra\\.claude\\projects\\C--code-hookprobe\\75346593-bb30-4162-ac6d-4e548101f7b8.jsonl","cwd":"C:\\code\\hookprobe","hook_event_name":"SessionEnd","reason":"other"}"#;
    /// Shape documented in the Claude Code hooks reference (notification_type
    /// is the discriminator; permission_prompt only fires interactively).
    const NOTIFICATION_PERMISSION_PROMPT: &str = r#"{"session_id":"75346593-bb30-4162-ac6d-4e548101f7b8","transcript_path":"C:\\x.jsonl","cwd":"C:\\code\\hookprobe","hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash"}"#;
    /// Shape from the Codex docs/config.md notify example (kebab-case keys).
    const CODEX_TURN_COMPLETE: &str = r#"{"type":"agent-turn-complete","thread-id":"b5f6c1c2-1111-2222-3333-444455556666","turn-id":"12345","cwd":"C:\\code\\hookprobe","input-messages":["Rename foo to bar."],"last-assistant-message":"Rename complete."}"#;
    const LOCAL_TOOL_FINISHED: &str = r#"{"event":"tool_call_finished","session_id":"local-model-test-session","conversation_id":"local-model-test-thread","model":"gemma4:e4b","registry_name":"ollama-gemma4-e4b","turn_index":1,"tool_name":"workspace_put","tool_call_id":"call_1","tool_response":{"ok":true,"value":{"actual":4}},"error_code":""}"#;
    const LOCAL_TURN_FINISHED: &str = r#"{"event":"turn_finished","session_id":"local-model-test-session","conversation_id":"local-model-test-thread","model":"gemma4:e4b","registry_name":"ollama-gemma4-e4b","turn_index":1,"finish_reason":"tool_calls","usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}"#;

    fn parse(body: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(body)
            .expect("test payload must parse")
            .as_object()
            .expect("test payload must be an object")
            .clone()
    }

    #[test]
    fn real_claude_payloads_normalize_to_expected_kinds_without_content() {
        let cases: &[(&str, AgentEventKind)] = &[
            (REAL_USER_PROMPT_SUBMIT, AgentEventKind::TurnStarted),
            (REAL_PRE_TOOL_USE, AgentEventKind::ToolCallStarted),
            (REAL_POST_TOOL_USE, AgentEventKind::ToolCallFinished),
            (REAL_STOP, AgentEventKind::TurnFinished),
            (REAL_SESSION_END, AgentEventKind::StateChanged),
        ];
        for (body, expected_kind) in cases {
            let record = normalize_claude_hook_event(SPAWN_ID, &parse(body))
                .unwrap_or_else(|detail| panic!("{expected_kind:?} must normalize: {detail}"));
            assert_eq!(record.kind, *expected_kind);
            assert_eq!(record.spawn_id.as_deref(), Some(SPAWN_ID));
            assert_eq!(
                record.attributes.conversation_id.as_deref(),
                Some("75346593-bb30-4162-ac6d-4e548101f7b8")
            );
            assert_eq!(
                record.attributes.provider_name.as_deref(),
                Some("anthropic")
            );
            record.validate().expect("normalized record must validate");
            let encoded = serde_json::to_string(&record).expect("record serializes");
            for content in ["probe-marker-42", "echo", "Rename", "done"] {
                assert!(
                    !encoded.contains(content),
                    "journal row must not contain content {content:?}: {encoded}"
                );
            }
        }
    }

    #[test]
    fn tool_call_rows_carry_otel_attributes_and_digests() {
        let record = normalize_claude_hook_event(SPAWN_ID, &parse(REAL_POST_TOOL_USE))
            .expect("PostToolUse must normalize");
        assert_eq!(record.attributes.tool_name.as_deref(), Some("Bash"));
        assert_eq!(
            record.attributes.tool_call_id.as_deref(),
            Some("toolu_01HwXsLynP3Wh9b6VPBQWLcf")
        );
        assert_eq!(
            record.attributes.operation_name,
            Some(GenAiOperationName::ExecuteTool)
        );
        let payload = record.payload.as_object().expect("payload object");
        assert_eq!(payload["duration_ms"], 8668);
        for key in [
            "tool_input_bytes",
            "tool_input_sha256",
            "tool_response_bytes",
            "tool_response_sha256",
        ] {
            assert!(payload.contains_key(key), "missing {key}: {payload:?}");
        }
        let digest = payload["tool_input_sha256"].as_str().expect("digest str");
        assert!(digest.starts_with("sha256:"), "{digest}");
    }

    #[test]
    fn permission_prompt_notification_maps_to_needs_input() {
        let record = normalize_claude_hook_event(SPAWN_ID, &parse(NOTIFICATION_PERMISSION_PROMPT))
            .expect("Notification must normalize");
        assert_eq!(record.kind, AgentEventKind::StateChanged);
        assert_eq!(record.reason_code.as_deref(), Some("permission_prompt"));
        assert_eq!(record.state_to.as_deref(), Some("needs_input"));
        let payload = record.payload.as_object().expect("payload");
        assert!(payload.contains_key("message_sha256"));
        assert!(
            !serde_json::to_string(&record)
                .expect("serializes")
                .contains("needs your permission"),
            "notification message text is content and must not be stored"
        );
    }

    #[test]
    fn codex_turn_complete_normalizes_to_turn_finished() {
        let record = normalize_codex_notify_event(SPAWN_ID, &parse(CODEX_TURN_COMPLETE))
            .expect("codex notify must normalize");
        assert_eq!(record.kind, AgentEventKind::TurnFinished);
        assert_eq!(record.attributes.provider_name.as_deref(), Some("openai"));
        assert_eq!(
            record.attributes.conversation_id.as_deref(),
            Some("b5f6c1c2-1111-2222-3333-444455556666")
        );
        let payload = record.payload.as_object().expect("payload");
        assert_eq!(payload["turn_id"], "12345");
        assert_eq!(payload["input_message_count"], 1);
        assert!(payload.contains_key("last_assistant_message_sha256"));
        record.validate().expect("record validates");
    }

    #[test]
    fn local_model_runner_events_normalize_to_local_provider_metadata() {
        let record = normalize_local_model_runner_event(SPAWN_ID, &parse(LOCAL_TOOL_FINISHED))
            .expect("local tool event must normalize");
        assert_eq!(record.kind, AgentEventKind::ToolCallFinished);
        assert_eq!(record.spawn_id.as_deref(), Some(SPAWN_ID));
        assert_eq!(
            record.session_id.as_deref(),
            Some("local-model-test-session")
        );
        assert_eq!(record.attributes.provider_name.as_deref(), Some("local"));
        assert_eq!(
            record.attributes.conversation_id.as_deref(),
            Some("local-model-test-thread")
        );
        assert_eq!(
            record.attributes.tool_name.as_deref(),
            Some("workspace_put")
        );
        assert_eq!(record.attributes.tool_call_id.as_deref(), Some("call_1"));
        let payload = record.payload.as_object().expect("payload");
        assert_eq!(payload["turn_index"], 1);
        assert!(payload.contains_key("tool_response_sha256"));
        assert!(
            !serde_json::to_string(&record)
                .expect("serialize")
                .contains("\"actual\":4"),
            "tool response content must be digested, not stored verbatim"
        );
        record.validate().expect("record validates");

        let turn = normalize_local_model_runner_event(SPAWN_ID, &parse(LOCAL_TURN_FINISHED))
            .expect("local turn event must normalize");
        assert_eq!(turn.kind, AgentEventKind::TurnFinished);
        assert_eq!(turn.attributes.usage_input_tokens, Some(100));
        assert_eq!(turn.attributes.usage_output_tokens, Some(20));
        assert_eq!(
            turn.payload.as_object().expect("payload")["usage_total_tokens"],
            120
        );
    }

    #[test]
    fn unsubscribed_unknown_and_malformed_events_are_refused_with_detail() {
        // Unsubscribed hook event = config drift.
        let mut drifted = parse(REAL_STOP);
        drifted.insert("hook_event_name".to_owned(), Value::from("PostToolBatch"));
        let error = normalize_claude_hook_event(SPAWN_ID, &drifted)
            .expect_err("unsubscribed event must refuse");
        assert!(error.contains("PostToolBatch"), "{error}");

        // Unknown notification_type cannot be mapped to attention semantics.
        let mut unknown_notification = parse(NOTIFICATION_PERMISSION_PROMPT);
        unknown_notification.insert("notification_type".to_owned(), Value::from("hyperspace"));
        let error = normalize_claude_hook_event(SPAWN_ID, &unknown_notification)
            .expect_err("unknown notification_type must refuse");
        assert!(error.contains("hyperspace"), "{error}");

        // Missing required field.
        let mut missing_prompt = parse(REAL_USER_PROMPT_SUBMIT);
        missing_prompt.remove("prompt");
        let error = normalize_claude_hook_event(SPAWN_ID, &missing_prompt)
            .expect_err("missing prompt must refuse");
        assert!(error.contains("prompt"), "{error}");

        // Unsupported codex notify type.
        let mut unknown_notify = parse(CODEX_TURN_COMPLETE);
        unknown_notify.insert("type".to_owned(), Value::from("approval-requested"));
        let error = normalize_codex_notify_event(SPAWN_ID, &unknown_notify)
            .expect_err("unknown notify type must refuse");
        assert!(error.contains("approval-requested"), "{error}");
    }

    #[test]
    fn spawn_id_shape_rejects_traversal_and_foreign_prefixes() {
        assert!(validate_spawn_id_shape("agent-spawn-019ebdee-7996").is_ok());
        for bad in [
            "reflex-12345",
            "agent-spawn-..",
            "agent-spawn-a/../b",
            "agent-spawn-a\\b",
            "agent-spawn-",
        ] {
            // "agent-spawn-" alone has no suffix but passes prefix+charset;
            // the directory probe is what rejects it. Everything else must
            // fail the shape check.
            if bad == "agent-spawn-" {
                continue;
            }
            assert!(
                validate_spawn_id_shape(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    /// Exercises the unknown-spawn gate against the REAL spawn root used by
    /// `act_spawn_agent` (no mocked filesystem): a directory that exists is
    /// accepted, a fabricated id is refused with 404 and counted.
    #[test]
    fn identity_validation_uses_the_real_spawn_root() {
        let spawn_id = format!("agent-spawn-ingress-itest-{}", std::process::id());
        let spawn_dir = super::super::m4_tools::agent_spawn_root_dir()
            .expect("spawn root must resolve on a configured machine")
            .join(&spawn_id);
        std::fs::create_dir_all(&spawn_dir).expect("create real spawn dir");
        let query = vec![
            ("spawn_id".to_owned(), spawn_id.clone()),
            ("source".to_owned(), "claude_code_hooks".to_owned()),
        ];
        let identity = validate_ingress_identity(&query).expect("issued spawn id must be accepted");
        assert_eq!(identity.spawn_id, spawn_id);
        assert_eq!(identity.source, AgentEventIngressSource::ClaudeCodeHooks);
        std::fs::remove_dir_all(&spawn_dir).expect("cleanup spawn dir");

        let rejected_before = REJECTED_UNKNOWN_SPAWN_TOTAL.load(Ordering::Relaxed);
        let refusal = validate_ingress_identity(&query).expect_err("removed spawn dir must refuse");
        assert_eq!(refusal.http_status, 404);
        assert_eq!(refusal.code, "AGENT_EVENT_INGRESS_UNKNOWN_SPAWN");
        assert!(REJECTED_UNKNOWN_SPAWN_TOTAL.load(Ordering::Relaxed) > rejected_before);

        let bad_source = vec![
            ("spawn_id".to_owned(), spawn_id),
            ("source".to_owned(), "carrier_pigeon".to_owned()),
        ];
        let refusal =
            validate_ingress_identity(&bad_source).expect_err("unknown source must refuse");
        assert_eq!(refusal.http_status, 422);
    }

    #[test]
    fn ingest_writes_physical_row_and_counts_acceptance() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db =
            Db::open(&temp.path().join("db"), synapse_core::SCHEMA_VERSION).expect("temp db opens");
        let identity = AgentEventIngressIdentity {
            spawn_id: SPAWN_ID.to_owned(),
            source: AgentEventIngressSource::ClaudeCodeHooks,
        };
        let accepted_before = ACCEPTED_TOTAL.load(Ordering::Relaxed);
        let (readback, record) = ingest_agent_event(&db, &identity, REAL_PRE_TOOL_USE.as_bytes())
            .expect("real payload must ingest");
        db.flush().expect("flush");
        let rows = db
            .scan_cf(synapse_storage::cf::CF_AGENT_EVENTS)
            .expect("scan");
        assert_eq!(rows.len(), 1, "exactly one physical row");
        let (key, value) = &rows[0];
        let (ts_ns, seq) =
            synapse_storage::agent_events::decode_agent_event_key(key).expect("key decodes");
        assert_eq!((ts_ns, seq), (readback.ts_ns, readback.seq));
        let stored: AgentEventRecord = synapse_storage::decode_json(value).expect("row decodes");
        assert_eq!(stored, record);
        assert!(ACCEPTED_TOTAL.load(Ordering::Relaxed) > accepted_before);

        let rejected_before = REJECTED_MALFORMED_TOTAL.load(Ordering::Relaxed);
        let refusal =
            ingest_agent_event(&db, &identity, b"not json").expect_err("garbage body must refuse");
        assert_eq!(refusal.http_status, 422);
        assert!(REJECTED_MALFORMED_TOTAL.load(Ordering::Relaxed) > rejected_before);
        db.flush().expect("flush");
        assert_eq!(
            db.scan_cf(synapse_storage::cf::CF_AGENT_EVENTS)
                .expect("scan")
                .len(),
            1,
            "refused body must not add rows"
        );
    }
}
