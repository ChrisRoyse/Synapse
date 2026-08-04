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
