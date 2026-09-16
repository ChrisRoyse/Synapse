use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Envelope schema version for [`AgentEventRecord`] rows.
pub const AGENT_EVENT_RECORD_VERSION: u32 = 1;

/// Maximum visible-ASCII identifier length accepted on an agent event row
/// (matches the MCP session-id validation bound).
pub const AGENT_EVENT_MAX_ID_CHARS: usize = 512;

/// Maximum reason-code length. Reason codes are machine-readable,
/// low-cardinality labels, never prose or stack traces.
pub const AGENT_EVENT_MAX_REASON_CHARS: usize = 128;

/// One agent lifecycle/telemetry event persisted in `CF_AGENT_EVENTS` (#897).
///
/// `ts_ns` must stay a required top-level field because the storage TTL
/// compaction filter extracts it from the JSON bytes (ADR
/// 2026-06-11-timeline-data-model contract); a row without it would never
/// expire by age.
///
/// Attribute names align with the OpenTelemetry `GenAI` semantic conventions
/// (Development stability as of semconv v1.37) so downstream views never need
/// a private vocabulary. Prompt/completion/tool-argument CONTENT is opt-in
/// per those conventions and is NOT written by default; writers record
/// bounded metadata (byte counts, hashes, ids) instead.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentEventRecord {
    pub record_version: u32,
    pub ts_ns: u64,
    pub kind: AgentEventKind,
    /// MCP session id (`Mcp-Session-Id`) the event is attributed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `act_spawn_agent` spawn id (`agent-spawn-*`) when the event belongs to
    /// a spawned-agent lifecycle that may not have a session yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    /// Machine-readable reason code for state changes and exits, e.g.
    /// `session_initialized`, `stale_session_reclaimed`, `handoff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// `AgentOps`-style terminal state, aligned with `OTel` `StatusCode`:
    /// a process that dies without reporting maps to `indeterminate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_state: Option<AgentEndState>,
    /// Previous lifecycle state for `state_changed` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_from: Option<String>,
    /// New lifecycle state for `state_changed` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_to: Option<String>,
    #[serde(default, skip_serializing_if = "GenAiAttributes::is_empty")]
    pub attributes: GenAiAttributes,
    /// Bounded event-kind-specific metadata. Never raw prompt, completion,
    /// keystroke, or tool-argument content.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// Discriminant for agent event rows (#897 minimum set).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentEventKind {
    SpawnRequested,
    SpawnReady,
    StateChanged,
    ToolCallStarted,
    ToolCallFinished,
    TurnStarted,
    TurnFinished,
    MessageSent,
    MessageReceived,
    LeaseAcquired,
    LeaseReleased,
    Interrupted,
    Killed,
    Exited,
}

/// Terminal agent outcome, aligned with `OTel` `StatusCode` and the `AgentOps`
/// session end-state taxonomy (`Success | Fail | Indeterminate`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentEndState {
    Success,
    Error,
    Indeterminate,
}

/// `gen_ai.operation.name` enum values from the `OTel` `GenAI` semantic
/// conventions registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GenAiOperationName {
    Chat,
    CreateAgent,
    Embeddings,
    ExecuteTool,
    GenerateContent,
    InvokeAgent,
    InvokeWorkflow,
    Retrieval,
}

/// `OTel` `GenAI` semantic-convention attributes carried on an agent event.
///
/// Serialized field names are the exact convention attribute names so stored
/// rows are self-describing for any `OTel`-aware consumer. All fields are
/// optional; only the ones meaningful for the event kind are populated.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenAiAttributes {
    #[serde(
        rename = "gen_ai.operation.name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub operation_name: Option<GenAiOperationName>,
    /// Provider behind the agent (`anthropic`, `openai`, custom). Replaces
    /// the deprecated `gen_ai.system` attribute.
    #[serde(
        rename = "gen_ai.provider.name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_name: Option<String>,
    #[serde(
        rename = "gen_ai.agent.id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_id: Option<String>,
    #[serde(
        rename = "gen_ai.agent.name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_name: Option<String>,
    #[serde(
        rename = "gen_ai.conversation.id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub conversation_id: Option<String>,
    #[serde(
        rename = "gen_ai.request.model",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_model: Option<String>,
    #[serde(
        rename = "gen_ai.response.model",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_model: Option<String>,
    #[serde(
        rename = "gen_ai.usage.input_tokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_input_tokens: Option<u64>,
    #[serde(
        rename = "gen_ai.usage.output_tokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_output_tokens: Option<u64>,
    /// Input tokens served from a provider-managed cache (core registry
    /// attribute, not a vendor extension).
    #[serde(
        rename = "gen_ai.usage.cache_read.input_tokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_cache_read_input_tokens: Option<u64>,
    /// Input tokens written to a provider-managed cache.
    #[serde(
        rename = "gen_ai.usage.cache_creation.input_tokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_cache_creation_input_tokens: Option<u64>,
    #[serde(
        rename = "gen_ai.tool.name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_name: Option<String>,
    #[serde(
        rename = "gen_ai.tool.call.id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_call_id: Option<String>,
    /// Tool-call arguments. Opt-in per `OTel` `GenAI` conventions (sensitive
    /// content); default writers leave this `None` and record byte counts in
    /// the event payload instead.
    #[serde(
        rename = "gen_ai.tool.call.arguments",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_call_arguments: Option<Value>,
    /// Tool-call result. Opt-in for the same reason as the arguments.
    #[serde(
        rename = "gen_ai.tool.call.result",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_call_result: Option<Value>,
    /// Stable `OTel` attribute: low-cardinality error class when the operation
    /// the event describes ended in error.
    #[serde(
        rename = "error.type",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub error_type: Option<String>,
}

impl GenAiAttributes {
    /// True when no attribute is set (the record omits the object entirely).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.operation_name.is_none()
            && self.provider_name.is_none()
            && self.agent_id.is_none()
            && self.agent_name.is_none()
            && self.conversation_id.is_none()
            && self.request_model.is_none()
            && self.response_model.is_none()
            && self.usage_input_tokens.is_none()
            && self.usage_output_tokens.is_none()
            && self.usage_cache_read_input_tokens.is_none()
            && self.usage_cache_creation_input_tokens.is_none()
            && self.tool_name.is_none()
            && self.tool_call_id.is_none()
            && self.tool_call_arguments.is_none()
            && self.tool_call_result.is_none()
            && self.error_type.is_none()
    }
}

impl AgentEventRecord {
    /// Builds a version-stamped record. Callers must attach at least one of
    /// `session_id` / `spawn_id` before persisting; [`Self::validate`]
    /// enforces it.
    #[must_use]
    pub const fn new(ts_ns: u64, kind: AgentEventKind) -> Self {
        Self {
            record_version: AGENT_EVENT_RECORD_VERSION,
            ts_ns,
            kind,
            session_id: None,
            spawn_id: None,
            reason_code: None,
            end_state: None,
            state_from: None,
            state_to: None,
            attributes: GenAiAttributes {
                operation_name: None,
                provider_name: None,
                agent_id: None,
                agent_name: None,
                conversation_id: None,
                request_model: None,
                response_model: None,
                usage_input_tokens: None,
                usage_output_tokens: None,
                usage_cache_read_input_tokens: None,
                usage_cache_creation_input_tokens: None,
                tool_name: None,
                tool_call_id: None,
                tool_call_arguments: None,
                tool_call_result: None,
                error_type: None,
            },
            payload: Value::Null,
        }
    }

    /// Structural validity check enforced by the journal writer before any
    /// row is persisted. Violations are caller bugs, surfaced as errors, not
    /// silently repaired.
    ///
    /// # Errors
    ///
    /// Returns a structured detail string naming the first violated
    /// constraint: anonymous events (no `session_id` and no `spawn_id`), a stale
    /// `record_version`, `ts_ns == 0`, or over-long/non-ASCII identifiers.
    pub fn validate(&self) -> Result<(), String> {
        if self.record_version != AGENT_EVENT_RECORD_VERSION {
            return Err(format!(
                "AGENT_EVENT_INVALID: record_version {} != {AGENT_EVENT_RECORD_VERSION}",
                self.record_version
            ));
        }
        if self.ts_ns == 0 {
            return Err("AGENT_EVENT_INVALID: ts_ns must be a positive unix nanosecond timestamp (the TTL filter keys on it)".to_owned());
        }
        if self.session_id.is_none() && self.spawn_id.is_none() {
            return Err(format!(
                "AGENT_EVENT_INVALID: kind={:?} carries neither session_id nor spawn_id; anonymous agent events are unattributable and refused",
                self.kind
            ));
        }
        validate_id_field("session_id", self.session_id.as_deref())?;
        validate_id_field("spawn_id", self.spawn_id.as_deref())?;
        if let Some(reason) = self.reason_code.as_deref()
            && (reason.is_empty() || reason.chars().count() > AGENT_EVENT_MAX_REASON_CHARS)
        {
            return Err(format!(
                "AGENT_EVENT_INVALID: reason_code must be 1..={AGENT_EVENT_MAX_REASON_CHARS} chars, got {}",
                reason.chars().count()
            ));
        }
        Ok(())
    }
}

fn validate_id_field(field: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty() || value.chars().count() > AGENT_EVENT_MAX_ID_CHARS {
        return Err(format!(
            "AGENT_EVENT_INVALID: {field} must be 1..={AGENT_EVENT_MAX_ID_CHARS} chars, got {}",
            value.chars().count()
        ));
    }
    if !value.chars().all(|ch| ('!'..='~').contains(&ch)) {
        return Err(format!(
            "AGENT_EVENT_INVALID: {field} must contain only visible ASCII characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_record() -> AgentEventRecord {
        let mut record = AgentEventRecord::new(1_000, AgentEventKind::StateChanged);
        record.session_id = Some("session-1".to_owned());
        record
    }

    #[test]
    fn serializes_otel_attribute_names_verbatim() {
        let mut record = valid_record();
        record.kind = AgentEventKind::ToolCallFinished;
        record.attributes.operation_name = Some(GenAiOperationName::ExecuteTool);
        record.attributes.tool_name = Some("observe".to_owned());
        record.attributes.usage_input_tokens = Some(120);
        record.attributes.usage_cache_read_input_tokens = Some(64);
        record.attributes.error_type = Some("timeout".to_owned());

        let json = serde_json::to_value(&record).expect("record must serialize");
        assert_eq!(json["kind"], "tool_call_finished");
        assert_eq!(json["attributes"]["gen_ai.operation.name"], "execute_tool");
        assert_eq!(json["attributes"]["gen_ai.tool.name"], "observe");
        assert_eq!(json["attributes"]["gen_ai.usage.input_tokens"], 120);
        assert_eq!(
            json["attributes"]["gen_ai.usage.cache_read.input_tokens"],
            64
        );
        assert_eq!(json["attributes"]["error.type"], "timeout");
        assert_eq!(json["ts_ns"], 1_000);
    }

    #[test]
    fn empty_attributes_are_omitted_from_json() {
        let record = valid_record();
        let json = serde_json::to_value(&record).expect("record must serialize");
        assert!(
            json.get("attributes").is_none(),
            "empty attributes must be skipped: {json}"
        );
        let roundtrip: AgentEventRecord =
            serde_json::from_value(json).expect("record must roundtrip");
        assert_eq!(roundtrip, record);
    }

    #[test]
    fn validate_rejects_anonymous_events() {
        let record = AgentEventRecord::new(1_000, AgentEventKind::Exited);
        let error = record.validate().expect_err("anonymous event must fail");
        assert!(error.contains("AGENT_EVENT_INVALID"), "{error}");
        assert!(error.contains("neither session_id nor spawn_id"), "{error}");
    }

    #[test]
    fn validate_rejects_zero_ts_ns() {
        let mut record = valid_record();
        record.ts_ns = 0;
        let error = record.validate().expect_err("ts_ns=0 must fail");
        assert!(error.contains("ts_ns"), "{error}");
    }

    #[test]
    fn validate_rejects_overlong_and_non_ascii_ids() {
        let mut record = valid_record();
        record.session_id = Some("x".repeat(AGENT_EVENT_MAX_ID_CHARS + 1));
        assert!(record.validate().is_err(), "overlong session_id must fail");

        let mut record = valid_record();
        record.spawn_id = Some("spawn id with spaces".to_owned());
        assert!(record.validate().is_err(), "non-visible-ASCII must fail");
    }

    #[test]
    fn validate_accepts_minimum_spawn_only_event() {
        let mut record = AgentEventRecord::new(1, AgentEventKind::SpawnRequested);
        record.spawn_id = Some("agent-spawn-abc123".to_owned());
        record.validate().expect("spawn-only event must validate");
    }
}
