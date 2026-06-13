use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Envelope schema version for [`AgentTranscriptRecord`] rows.
pub const AGENT_TRANSCRIPT_RECORD_VERSION: u32 = 1;

/// Bound on the normalized content summary carried on one transcript row.
///
/// The full content length and hash are always recorded alongside, so a
/// truncated summary is an honest, detectable reduction — never silent.
pub const AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS: usize = 2048;

/// Bound on serialized tool-call arguments carried on one transcript row.
pub const AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS: usize = 8192;

/// Bound on the tool result summary carried on one transcript row.
pub const AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS: usize = 8192;

/// One normalized transcript event ingested from a spawned agent's
/// authoritative JSONL stream (#900).
///
/// Exactly one row exists per source line, keyed `(spawn_id, line_no)` in
/// `CF_AGENT_TRANSCRIPTS`, so transcript rows reconcile line-for-line
/// against the raw file: `parsed` rows plus `invalid` rows always equals
/// total source lines ingested.
///
/// `ts_ns` is the ingestion timestamp and must stay a required top-level
/// field: the storage TTL compaction filter extracts it from the JSON bytes
/// (same contract as `CF_TIMELINE` / `CF_AGENT_EVENTS`).
///
/// Unlike the `CF_AGENT_EVENTS` journal (bounded metadata, never content),
/// transcript rows DO carry bounded content: they exist so the Command
/// Center can render what an agent said and did (#917) without re-parsing
/// version-drifting CLI log files. Every content field is capped with the
/// full byte length and SHA-256 recorded, so truncation is visible.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTranscriptRecord {
    pub record_version: u32,
    /// Ingestion time (unix nanoseconds); the TTL anchor.
    pub ts_ns: u64,
    /// `act_spawn_agent` spawn id (`agent-spawn-*`) — the identity anchor.
    pub spawn_id: String,
    /// 1-based line number in the source JSONL file.
    pub line_no: u64,
    /// Which version-pinned parser produced this row.
    pub source: TranscriptSource,
    /// Parse outcome. `invalid` rows preserve the line-for-line contract:
    /// the unparseable line still gets a row carrying the error detail.
    pub status: TranscriptParseStatus,
    /// Normalized speaker/category for the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<TranscriptRole>,
    /// Source-native event discriminator, e.g. `assistant`,
    /// `system/init`, `item.completed/mcp_tool_call`, `turn.completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_kind: Option<String>,
    /// 1-based turn index within the session, derived from the stream
    /// (Claude: distinct assistant message ids; Codex: `turn.started`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<u64>,
    /// Provider conversation/thread id (Claude `session_id`, Codex
    /// `thread_id`). NEVER an MCP session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Bounded text content (assistant text/thinking, agent message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_summary: Option<String>,
    /// Full byte length of the content the summary was derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_bytes: Option<u64>,
    /// SHA-256 (lowercase hex) of the full content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// True when `content_summary` was truncated to the cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_truncated: bool,
    /// Tool calls observed on this line (Claude `tool_use` blocks, Codex
    /// `mcp_tool_call` / `command_execution` items).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<TranscriptToolCall>,
    /// Token usage reported on this line, verbatim from the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TranscriptUsage>,
    /// Source-reported error detail (Codex `turn.failed` / `error` events,
    /// Claude `result` with `is_error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_error: Option<String>,
    /// Structured parse failure detail for `invalid` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    /// Byte length of the raw source line.
    pub raw_line_bytes: u64,
    /// SHA-256 (lowercase hex) of the raw source line (without newline).
    pub raw_line_sha256: String,
}

/// Version-pinned source format of a transcript line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptSource {
    /// Claude Code `-p --output-format stream-json` stdout (verified against
    /// CLI 2.1.x event vocabulary).
    ClaudeStreamJson,
    /// Codex `exec --json` stdout (verified against the
    /// `thread.*`/`turn.*`/`item.*` event vocabulary).
    CodexExecJson,
}

/// Parse outcome for one source line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptParseStatus {
    Parsed,
    Invalid,
}

/// Normalized role taxonomy across CLI vocabularies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    /// Stream/system metadata (init, rate limits, thread/turn markers).
    System,
    /// Model output: text, thinking/reasoning, tool-use requests.
    Assistant,
    /// Tool execution and results (Claude `user`/`tool_result` lines,
    /// Codex `mcp_tool_call`/`command_execution` items).
    Tool,
    /// Terminal accounting events (Claude `result`, Codex `turn.completed`).
    Result,
}

/// One tool invocation observed on a transcript line. Arguments and result
/// are bounded with honest truncation accounting, same as content.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptToolCall {
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Serialized arguments JSON, capped at
    /// [`AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub arguments_truncated: bool,
    /// Bounded result text, capped at
    /// [`AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub result_truncated: bool,
    /// Source-native execution status (`completed`, `failed`,
    /// `in_progress`) when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Exit code for command executions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

/// Token usage as reported on one source line, normalized onto the `OTel`
/// `GenAI` vocabulary used by `CF_AGENT_EVENTS` (#897/#901).
///
/// Codex `cached_input_tokens` maps to `cache_read_input_tokens`;
/// `reasoning_output_tokens` is carried separately because no core `OTel`
/// attribute exists for it yet.
///
/// `cache_creation_input_tokens` is the *aggregate* cache-write count.
/// Anthropic additionally splits it by TTL tier
/// (`cache_creation.{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`),
/// which are billed at different multipliers (1.25x vs 2x base input, #949).
/// Those tier counts are captured here when present so the cost engine can
/// price each TTL exactly; their sum is always a subset of (normally equal to)
/// the aggregate. They are absent on Codex rows, which have no cache writes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Anthropic 5-minute-TTL cache-write tokens (subset of the aggregate),
    /// billed at 1.25x base input. `None` when the stream did not report a
    /// tier split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_5m_input_tokens: Option<u64>,
    /// Anthropic 1-hour-TTL cache-write tokens (subset of the aggregate),
    /// billed at 2x base input. `None` when the stream did not report a split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_1h_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    /// Claude `result` lines report a total cost; stored in micro-USD so the
    /// record stays integer-exact (`total_cost_usd * 1_000_000`, rounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_micro_usd: Option<u64>,
    /// Claude `result.modelUsage` per-model breakdown. A single session can use
    /// several models (e.g. a primary model plus a fast sub-agent model); the
    /// top-level `usage` reflects only the primary, so multi-model sessions
    /// undercount without this map (#949). Empty on every non-`result` row and
    /// on Codex rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_usage: Vec<TranscriptModelUsage>,
}

impl TranscriptUsage {
    /// True when no usage figure is present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_input_tokens.is_none()
            && self.cache_creation_input_tokens.is_none()
            && self.cache_creation_5m_input_tokens.is_none()
            && self.cache_creation_1h_input_tokens.is_none()
            && self.reasoning_output_tokens.is_none()
            && self.total_cost_micro_usd.is_none()
            && self.model_usage.is_empty()
    }
}

/// One model's slice of a Claude `result.modelUsage` map (#949).
///
/// Token counts are disjoint exactly as the top-level Claude usage is;
/// `cost_micro_usd` is the CLI's own per-model cost (`costUSD`), stored
/// integer-exact in micro-USD for reconciliation cross-check.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptModelUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micro_usd: Option<u64>,
}

impl AgentTranscriptRecord {
    /// Builds a version-stamped row shell; the ingester fills in the parsed
    /// fields before persisting. [`Self::validate`] enforces invariants.
    #[must_use]
    pub const fn new(
        ts_ns: u64,
        spawn_id: String,
        line_no: u64,
        source: TranscriptSource,
        raw_line_bytes: u64,
        raw_line_sha256: String,
    ) -> Self {
        Self {
            record_version: AGENT_TRANSCRIPT_RECORD_VERSION,
            ts_ns,
            spawn_id,
            line_no,
            source,
            status: TranscriptParseStatus::Parsed,
            role: None,
            event_kind: None,
            turn_index: None,
            conversation_id: None,
            model: None,
            content_summary: None,
            content_bytes: None,
            content_sha256: None,
            content_truncated: false,
            tool_calls: Vec::new(),
            usage: None,
            source_error: None,
            parse_error: None,
            raw_line_bytes,
            raw_line_sha256,
        }
    }

    /// Structural validity check enforced before any row is persisted.
    /// Violations are ingester bugs, surfaced as errors, never repaired.
    ///
    /// # Errors
    ///
    /// Returns a structured detail string naming the first violated
    /// constraint.
    pub fn validate(&self) -> Result<(), String> {
        if self.record_version != AGENT_TRANSCRIPT_RECORD_VERSION {
            return Err(format!(
                "AGENT_TRANSCRIPT_INVALID: record_version {} != {AGENT_TRANSCRIPT_RECORD_VERSION}",
                self.record_version
            ));
        }
        if self.ts_ns == 0 {
            return Err(
                "AGENT_TRANSCRIPT_INVALID: ts_ns must be a positive unix nanosecond timestamp (the TTL filter keys on it)"
                    .to_owned(),
            );
        }
        if self.spawn_id.is_empty() || !self.spawn_id.starts_with("agent-spawn-") {
            return Err(format!(
                "AGENT_TRANSCRIPT_INVALID: spawn_id must start with `agent-spawn-`, got {:?}",
                self.spawn_id
            ));
        }
        if !self
            .spawn_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(
                "AGENT_TRANSCRIPT_INVALID: spawn_id must contain only ASCII alphanumerics and dashes"
                    .to_owned(),
            );
        }
        if self.line_no == 0 {
            return Err("AGENT_TRANSCRIPT_INVALID: line_no is 1-based; 0 is unaddressable".to_owned());
        }
        if self.status == TranscriptParseStatus::Invalid && self.parse_error.is_none() {
            return Err(
                "AGENT_TRANSCRIPT_INVALID: invalid rows must carry parse_error detail".to_owned(),
            );
        }
        if self.status == TranscriptParseStatus::Parsed && self.parse_error.is_some() {
            return Err(
                "AGENT_TRANSCRIPT_INVALID: parsed rows must not carry parse_error".to_owned(),
            );
        }
        if self.raw_line_sha256.len() != 64
            || !self
                .raw_line_sha256
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
        {
            return Err(format!(
                "AGENT_TRANSCRIPT_INVALID: raw_line_sha256 must be 64 lowercase hex chars, got {:?}",
                self.raw_line_sha256
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_record() -> AgentTranscriptRecord {
        AgentTranscriptRecord::new(
            1_000,
            "agent-spawn-test1".to_owned(),
            1,
            TranscriptSource::ClaudeStreamJson,
            42,
            "a".repeat(64),
        )
    }

    #[test]
    fn serializes_and_roundtrips() {
        let mut record = valid_record();
        record.role = Some(TranscriptRole::Assistant);
        record.event_kind = Some("assistant".to_owned());
        record.usage = Some(TranscriptUsage {
            input_tokens: Some(10),
            output_tokens: Some(20),
            cache_read_input_tokens: Some(5),
            ..TranscriptUsage::default()
        });
        let json = serde_json::to_value(&record).expect("must serialize");
        assert_eq!(json["status"], "parsed");
        assert_eq!(json["source"], "claude_stream_json");
        assert_eq!(json["usage"]["input_tokens"], 10);
        assert_eq!(json["ts_ns"], 1_000);
        let roundtrip: AgentTranscriptRecord =
            serde_json::from_value(json).expect("must roundtrip");
        assert_eq!(roundtrip, record);
    }

    #[test]
    fn validate_rejects_invalid_row_without_parse_error() {
        let mut record = valid_record();
        record.status = TranscriptParseStatus::Invalid;
        let error = record.validate().expect_err("must refuse");
        assert!(error.contains("parse_error"), "{error}");
    }

    #[test]
    fn validate_rejects_parsed_row_with_parse_error() {
        let mut record = valid_record();
        record.parse_error = Some("boom".to_owned());
        let error = record.validate().expect_err("must refuse");
        assert!(error.contains("must not carry parse_error"), "{error}");
    }

    #[test]
    fn validate_rejects_bad_spawn_id_and_line_no() {
        let mut record = valid_record();
        record.spawn_id = "not-a-spawn".to_owned();
        assert!(record.validate().is_err(), "bad prefix must fail");

        let mut record = valid_record();
        record.spawn_id = "agent-spawn-Ã¼nicode".to_owned();
        assert!(record.validate().is_err(), "non-ASCII must fail");

        let mut record = valid_record();
        record.line_no = 0;
        assert!(record.validate().is_err(), "line_no 0 must fail");
    }

    #[test]
    fn validate_rejects_bad_sha() {
        let mut record = valid_record();
        record.raw_line_sha256 = "ABC".to_owned();
        assert!(record.validate().is_err(), "short sha must fail");
        let mut record = valid_record();
        record.raw_line_sha256 = "A".repeat(64);
        assert!(record.validate().is_err(), "uppercase sha must fail");
    }
}
