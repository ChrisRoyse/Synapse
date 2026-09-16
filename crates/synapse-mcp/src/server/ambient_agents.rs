//! Ambient agent discovery: tracks Claude Code sessions Synapse did **not**
//! spawn (#fleet-ambient).
//!
//! # Why this exists (root cause)
//!
//! Until now an agent only "existed" to Synapse when `act_spawn_agent` wrote
//! the first `SpawnRequested` row to `CF_AGENT_EVENTS` and created a spawn dir
//! under the spawn root. A `claude` a human launches in a VS Code terminal hits
//! three independent gates that all exclude it: the `/agent-events` ingress
//! refuses any event without a pre-issued spawn dir, the #900 transcript
//! ingester only scans the spawn root (and only parses the `stream-json`
//! stdout vocabulary), and nothing ever journals it — so it never reaches the
//! state machine or the dashboard. Observability was hard-coupled to the spawn
//! lifecycle.
//!
//! This module decouples them. Every interactive `claude` session — spawned or
//! not — writes a persisted transcript at
//! `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`, appended one JSON record
//! per message. That file is the source of truth for an agent Synapse never
//! launched, and it already exists on disk for sessions running right now. We
//! discover those files, register each session as an **ambient agent** in the
//! existing journal → state-machine → `unbound_reads` → dashboard read path,
//! and tail the transcript into `CF_AGENT_TRANSCRIPTS`.
//!
//! # Identity
//!
//! An ambient agent's anchor is a synthetic, stable spawn id
//! `agent-spawn-ambient-claude-<session-id>` (the session UUID is path-safe and
//! satisfies the `agent-spawn-` shape every downstream reader validates). We
//! deliberately journal it with `session_id = None`: it has no MCP session, so
//! it must surface through `agent_state::unbound_reads`, exactly like an
//! in-flight spawn. Binding it to the Claude session UUID would hide it (the
//! session-list read only walks the MCP session registry).
//!
//! # Vocabulary
//!
//! The persisted session file is a **different** schema from the `stream-json`
//! stdout the #900 ingester parses: each line is an enveloped record
//! (`parentUuid`/`sessionId`/`cwd`/`gitBranch`/`timestamp`) whose `message` is
//! the raw Anthropic API message, interleaved with session-metadata records
//! (`mode`/`file-history-snapshot`/`summary`/`ai-title`/...). Hence a dedicated
//! parser and the [`TranscriptSource::ClaudeSessionJsonl`] tag. Parsing is
//! fail-loud: an unknown record type still writes an `invalid` row carrying the
//! structured reason, so format drift is a counted, logged defect — never a
//! silent skip.
//!
//! # Tailing contract
//!
//! Identical Filebeat-style checkpointing to #900: a durable per-session cursor
//! in `CF_KV` records the byte offset / line number / parser state; each cycle
//! reads only past the offset and advances only after rows commit; a file that
//! shrinks below the cursor is a sticky `AMBIENT_SOURCE_TRUNCATED` error. Disk
//! pressure defers a whole cycle (cursor untouched) rather than dropping rows.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{
    AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS,
    AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS, AgentEventKind, AgentEventRecord,
    AgentTranscriptRecord, GenAiOperationName, TranscriptParseStatus, TranscriptRole,
    TranscriptSource, TranscriptToolCall, TranscriptUsage,
};
use synapse_storage::{
    Db,
    agent_transcripts::{agent_transcript_key, agent_transcript_ts_index_key},
    cf, decode_json, encode_json,
};
use tokio_util::sync::CancellationToken;

use super::agent_events::{provider_for_agent_kind, record_agent_events, unix_time_ns_now};
use crate::m3::{M3State, default_daemon_db_path, default_db_path};

/// Seconds between ambient ingest cycles.
pub(crate) const INTERVAL_ENV: &str = "SYNAPSE_AMBIENT_INGEST_INTERVAL_SECS";
/// Delay before the first cycle.
pub(crate) const STARTUP_DELAY_ENV: &str = "SYNAPSE_AMBIENT_INGEST_STARTUP_DELAY_SECS";
/// Only register/tail sessions whose transcript was modified within this many
/// seconds. Keeps the daemon from resurrecting weeks of dead sessions (Claude
/// keeps transcripts 30 days by default) while still catching every session a
/// human is actually using.
pub(crate) const MAX_IDLE_ENV: &str = "SYNAPSE_AMBIENT_MAX_IDLE_SECS";
/// Test/override hook: point discovery straight at a `projects`-shaped dir.
pub(crate) const ROOT_ENV: &str = "SYNAPSE_AMBIENT_CLAUDE_PROJECTS_DIR";

const DEFAULT_INTERVAL_SECS: u64 = 5;
const DEFAULT_STARTUP_DELAY_SECS: u64 = 8;
const DEFAULT_MAX_IDLE_SECS: u64 = 24 * 3600;

const CURSOR_KV_PREFIX: &str = "ambient-agents/cursor/";
const SPAWN_ID_PREFIX: &str = "agent-spawn-ambient-claude-";
const AGENT_KIND: &str = "claude";
const CURSOR_VERSION: u32 = 1;

/// Hard cap on one encoded transcript row (matches the #900 ingester). Per-field
/// bounds keep real rows far below this; exceeding it is an ingester bug.
const MAX_VALUE_BYTES: usize = 32 * 1024;

static LINES_PARSED_TOTAL: AtomicU64 = AtomicU64::new(0);
static LINES_INVALID_TOTAL: AtomicU64 = AtomicU64::new(0);
static SESSIONS_REGISTERED_TOTAL: AtomicU64 = AtomicU64::new(0);
static INGEST_ERRORS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PRESSURE_DEFERRALS_TOTAL: AtomicU64 = AtomicU64::new(0);
static CYCLES_TOTAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
enum AmbientRootScope {
    ExplicitEnv,
    ConfiguredDaemonDb,
}

impl AmbientRootScope {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::ExplicitEnv => "explicit_env",
            Self::ConfiguredDaemonDb => "configured_daemon_db",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AmbientRootDecision {
    root: PathBuf,
    scope: AmbientRootScope,
}

/// Process-lifetime ambient ingest counters for `GET /agent-transcripts/stats`.
pub(crate) fn ingest_stats() -> Value {
    json!({
        "lines_parsed_total": LINES_PARSED_TOTAL.load(Ordering::Relaxed),
        "lines_invalid_total": LINES_INVALID_TOTAL.load(Ordering::Relaxed),
        "sessions_registered_total": SESSIONS_REGISTERED_TOTAL.load(Ordering::Relaxed),
        "ingest_errors_total": INGEST_ERRORS_TOTAL.load(Ordering::Relaxed),
        "pressure_deferrals_total": PRESSURE_DEFERRALS_TOTAL.load(Ordering::Relaxed),
        "cycles_total": CYCLES_TOTAL.load(Ordering::Relaxed),
    })
}

/// One lifecycle signal derived from a parsed transcript line. The ingester
/// coalesces a cycle's signals down to the last one and emits at most one state
/// event per cycle, so a backfill of thousands of lines never floods the
/// journal or trips the runaway detector.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Lifecycle {
    /// Assistant requested a tool — the agent is working. Carries the tool's
    /// name and an input hash for the state machine's runaway signature.
    ToolUse {
        tool_name: String,
        input_sha256: String,
    },
    /// Assistant is mid-turn (a `thinking`/`text` partial whose `stop_reason`
    /// is `tool_use` or still streaming) — working, but the tool name lives on a
    /// sibling record. Persisted session messages are split one record per
    /// content block, so the tool name is not always on the same line.
    Working,
    /// Assistant ended its turn (`stop_reason` end_turn/stop_sequence/...) with
    /// no tool request — the agent is idle, waiting for the human.
    Idle,
    /// A fresh human prompt — a new turn is starting.
    TurnStarted,
}

/// Durable per-session tail state in `CF_KV` under [`CURSOR_KV_PREFIX`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AmbientCursor {
    record_version: u32,
    spawn_id: String,
    session_id: String,
    source_path: String,
    offset_bytes: u64,
    lines_ingested: u64,
    parsed_rows: u64,
    invalid_rows: u64,
    turn_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_assistant_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_branch: Option<String>,
    /// True once the `SpawnRequested`/`SpawnReady` registration rows are
    /// journaled. Restart-safe: the journal rebuild restores the agent, so a
    /// registered cursor never re-emits registration.
    registered: bool,
    /// The last lifecycle state event we emitted, so a cycle that produces the
    /// same signal does not re-journal it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_emitted_state: Option<String>,
    /// Sticky structured error; a parked session is skipped (and counted) until
    /// the cursor row is cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    updated_ts_ns: u64,
}

/// Outcome of one ingest pass over one session file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SessionIngestOutcome {
    new_parsed_rows: u64,
    new_invalid_rows: u64,
    newly_registered: bool,
    deferred_for_pressure: bool,
    skipped: bool,
}

fn cursor_kv_key(spawn_id: &str) -> Vec<u8> {
    format!("{CURSOR_KV_PREFIX}{spawn_id}").into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn bounded_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

fn bounded_json_string(value: &Value, cap: usize) -> (String, u64, bool) {
    let serialized = if let Value::String(text) = value {
        text.clone()
    } else {
        value.to_string()
    };
    let full_bytes = serialized.len() as u64;
    let (bounded, truncated) = bounded_chars(&serialized, cap);
    (bounded, full_bytes, truncated)
}

/// Resolves the `~/.claude/projects` directory the running user's `claude`
/// writes its session transcripts into.
///
/// # Errors
///
/// Returns a structured detail when no home anchor can be found — the daemon
/// must say *why* discovery is impossible rather than silently watch nothing.
fn claude_projects_root() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os(ROOT_ENV) {
        return Ok(PathBuf::from(dir));
    }
    claude_projects_root_from_host_env()
}

fn claude_projects_root_from_host_env() -> Result<PathBuf, String> {
    if let Some(cfg) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        // CLAUDE_CONFIG_DIR may list several dirs; the first is the writable one.
        let raw = cfg.to_string_lossy().into_owned();
        let first = raw
            .split([';', ':'])
            .map(str::trim)
            .find(|part| !part.is_empty())
            .unwrap_or(raw.as_str());
        return Ok(PathBuf::from(first).join("projects"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile).join(".claude").join("projects"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".claude").join("projects"));
    }
    Err(
        "AMBIENT_HOME_UNRESOLVED: none of CLAUDE_CONFIG_DIR, USERPROFILE, or HOME is set; \
         cannot locate ~/.claude/projects to discover ambient agents"
            .to_owned(),
    )
}

fn ambient_projects_root_for_db(db_path: &Path) -> Result<Option<AmbientRootDecision>, String> {
    if let Some(dir) = std::env::var_os(ROOT_ENV) {
        return Ok(Some(AmbientRootDecision {
            root: PathBuf::from(dir),
            scope: AmbientRootScope::ExplicitEnv,
        }));
    }
    if !ambient_host_root_allowed_for_db(db_path) {
        return Ok(None);
    }
    Ok(Some(AmbientRootDecision {
        root: claude_projects_root()?,
        scope: AmbientRootScope::ConfiguredDaemonDb,
    }))
}

fn ambient_host_root_allowed_for_db(db_path: &Path) -> bool {
    [default_db_path(), default_daemon_db_path()]
        .iter()
        .any(|allowed| paths_equivalent(db_path, allowed))
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_key(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| PathBuf::from(path));
    let mut raw = path.to_string_lossy().replace('/', "\\");
    while raw.ends_with('\\') {
        raw.pop();
    }
    #[cfg(windows)]
    {
        raw.make_ascii_lowercase();
    }
    raw
}

/// True when `stem` is a canonical 8-4-4-4-12 hex UUID — the shape of a Claude
/// session id. Filters out sidecar files that share the `.jsonl` extension.
fn is_session_stem(stem: &str) -> bool {
    let groups = [8_usize, 4, 4, 4, 12];
    let mut parts = stem.split('-');
    for expected in groups {
        match parts.next() {
            Some(part)
                if part.len() == expected && part.chars().all(|ch| ch.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

fn spawn_id_for_session(session_id: &str) -> String {
    format!("{SPAWN_ID_PREFIX}{session_id}")
}

fn load_cursor(db: &Db, spawn_id: &str) -> Result<Option<AmbientCursor>, String> {
    let key = cursor_kv_key(spawn_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, &key)
        .map_err(|error| format!("AMBIENT_CURSOR_READ_FAILED: {error}"))?;
    for (row_key, value) in rows {
        if row_key == key {
            let cursor: AmbientCursor = decode_json(&value)
                .map_err(|error| format!("AMBIENT_CURSOR_DECODE_FAILED: {error}"))?;
            return Ok(Some(cursor));
        }
    }
    Ok(None)
}

fn store_cursor(db: &Db, cursor: &AmbientCursor) -> Result<(), String> {
    let encoded =
        encode_json(cursor).map_err(|error| format!("AMBIENT_CURSOR_ENCODE_FAILED: {error}"))?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(cursor_kv_key(&cursor.spawn_id), encoded)])
        .map_err(|error| format!("AMBIENT_CURSOR_WRITE_FAILED: {error}"))
}

/// Marks a session's cursor with a sticky error, logs it once with full
/// context, and persists it so later cycles skip the session.
fn stick_cursor_error(db: &Db, cursor: &mut AmbientCursor, detail: String) -> String {
    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
    tracing::error!(
        code = "AMBIENT_INGEST_ERROR",
        spawn_id = %cursor.spawn_id,
        session_id = %cursor.session_id,
        source_path = %cursor.source_path,
        offset_bytes = cursor.offset_bytes,
        lines_ingested = cursor.lines_ingested,
        detail = %detail,
        "ambient transcript ingestion hit a sticky error; session parked until the cursor is cleared"
    );
    cursor.error = Some(detail.clone());
    cursor.updated_ts_ns = unix_time_ns_now();
    if let Err(store_error) = store_cursor(db, cursor) {
        tracing::error!(
            code = "AMBIENT_INGEST_ERROR",
            spawn_id = %cursor.spawn_id,
            detail = %store_error,
            "failed to persist the sticky ambient cursor error itself"
        );
    }
    detail
}

/// Journals the two-row registration (`SpawnRequested` → `SpawnReady`) that
/// makes an ambient agent exist for the state machine and the dashboard.
fn emit_registration(db: &Db, cursor: &AmbientCursor) -> Result<(), String> {
    let now = unix_time_ns_now();
    let provider = provider_for_agent_kind(AGENT_KIND);

    let mut requested = AgentEventRecord::new(now, AgentEventKind::SpawnRequested);
    requested.spawn_id = Some(cursor.spawn_id.clone());
    requested.reason_code = Some("ambient_discovered".to_owned());
    requested.attributes.operation_name = Some(GenAiOperationName::CreateAgent);
    requested.attributes.agent_name = Some(AGENT_KIND.to_owned());
    requested.attributes.provider_name = provider.clone();
    requested.attributes.conversation_id = Some(cursor.session_id.clone());
    requested.attributes.response_model = cursor.model.clone();
    requested.payload = json!({
        "source": "ambient_transcript",
        "cli": AGENT_KIND,
        "discovered_via": "claude_projects_tail",
        "session_id": cursor.session_id,
        "transcript_path": cursor.source_path,
        "working_dir": cursor.cwd,
        "git_branch": cursor.git_branch,
    });

    let mut ready = AgentEventRecord::new(now, AgentEventKind::SpawnReady);
    ready.spawn_id = Some(cursor.spawn_id.clone());
    ready.reason_code = Some("ambient_observed".to_owned());
    ready.attributes.agent_name = Some(AGENT_KIND.to_owned());
    ready.attributes.provider_name = provider;
    ready.attributes.conversation_id = Some(cursor.session_id.clone());
    // No owned process: ambient agents are observed, not launched, so there is
    // no launcher/agent pid or log dir to record here.
    ready.payload = json!({ "source": "ambient_transcript", "ambient": true });

    record_agent_events(db, &[requested, ready])
        .map_err(|error| format!("AMBIENT_REGISTER_WRITE_FAILED: {error}"))?;
    SESSIONS_REGISTERED_TOTAL.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        code = "AMBIENT_AGENT_REGISTERED",
        spawn_id = %cursor.spawn_id,
        session_id = %cursor.session_id,
        working_dir = ?cursor.cwd,
        git_branch = ?cursor.git_branch,
        model = ?cursor.model,
        "readback=CF_AGENT_EVENTS edge=ambient_registered"
    );
    Ok(())
}

/// Emits one lifecycle state event for an ambient agent, deduplicated against
/// the last emitted state so a steady stream of same-kind lines journals once.
fn emit_lifecycle(
    db: &Db,
    cursor: &mut AmbientCursor,
    lifecycle: &Lifecycle,
) -> Result<(), String> {
    let state_tag = match lifecycle {
        Lifecycle::ToolUse { tool_name, .. } => format!("tool:{tool_name}"),
        Lifecycle::Working => "working".to_owned(),
        Lifecycle::Idle => "idle".to_owned(),
        Lifecycle::TurnStarted => "turn_started".to_owned(),
    };
    if cursor.last_emitted_state.as_deref() == Some(state_tag.as_str()) {
        return Ok(());
    }
    let now = unix_time_ns_now();
    let mut record = match lifecycle {
        Lifecycle::ToolUse {
            tool_name,
            input_sha256,
        } => {
            let mut record = AgentEventRecord::new(now, AgentEventKind::ToolCallStarted);
            record.reason_code = Some("ambient_tool_activity".to_owned());
            record.attributes.operation_name = Some(GenAiOperationName::ExecuteTool);
            record.attributes.tool_name = Some(tool_name.clone());
            record.payload = json!({ "tool_input_sha256": input_sha256, "ambient": true });
            record
        }
        // Mid-turn activity with no tool name: `ToolCallFinished` reduces to
        // Working without resetting the turn or runaway counters.
        Lifecycle::Working => {
            let mut record = AgentEventRecord::new(now, AgentEventKind::ToolCallFinished);
            record.reason_code = Some("ambient_active".to_owned());
            record
        }
        Lifecycle::Idle => {
            let mut record = AgentEventRecord::new(now, AgentEventKind::TurnFinished);
            record.reason_code = Some("ambient_turn_finished".to_owned());
            record
        }
        Lifecycle::TurnStarted => {
            let mut record = AgentEventRecord::new(now, AgentEventKind::TurnStarted);
            record.reason_code = Some("ambient_turn_started".to_owned());
            record
        }
    };
    record.spawn_id = Some(cursor.spawn_id.clone());
    record.attributes.agent_name = Some(AGENT_KIND.to_owned());
    record.attributes.conversation_id = Some(cursor.session_id.clone());
    record_agent_events(db, std::slice::from_ref(&record))
        .map_err(|error| format!("AMBIENT_LIFECYCLE_WRITE_FAILED: {error}"))?;
    cursor.last_emitted_state = Some(state_tag);
    Ok(())
}

/// Ingests new bytes for one session file. Returns the structured sticky-error
/// detail (also persisted on the cursor) when the source is missing, truncated,
/// or a row exceeds the encoded-size cap.
fn ingest_session_file(
    db: &Db,
    session_id: &str,
    source_path: &Path,
) -> Result<SessionIngestOutcome, String> {
    let spawn_id = spawn_id_for_session(session_id);

    let mut cursor = match load_cursor(db, &spawn_id)? {
        Some(cursor) => cursor,
        None => seed_cursor(&spawn_id, session_id, source_path),
    };

    if let Some(error) = &cursor.error {
        tracing::debug!(
            code = "AMBIENT_INGEST_PARKED",
            spawn_id = %spawn_id,
            detail = %error,
            "skipping ambient session with sticky ingest error"
        );
        return Ok(SessionIngestOutcome {
            skipped: true,
            ..SessionIngestOutcome::default()
        });
    }

    let metadata = std::fs::metadata(source_path).map_err(|error| {
        stick_cursor_error(
            db,
            &mut cursor,
            format!(
                "AMBIENT_SOURCE_MISSING: cannot stat {}: {error}",
                source_path.display()
            ),
        )
    })?;
    let file_size = metadata.len();
    if file_size < cursor.offset_bytes {
        let detail = format!(
            "AMBIENT_SOURCE_TRUNCATED: file is {file_size} bytes but the cursor consumed {} — the source shrank underneath the tail",
            cursor.offset_bytes
        );
        return Err(stick_cursor_error(db, &mut cursor, detail));
    }

    let mut new_bytes = Vec::new();
    if file_size > cursor.offset_bytes {
        let mut file = std::fs::File::open(source_path).map_err(|error| {
            stick_cursor_error(
                db,
                &mut cursor,
                format!(
                    "AMBIENT_SOURCE_OPEN_FAILED: {}: {error}",
                    source_path.display()
                ),
            )
        })?;
        file.seek(SeekFrom::Start(cursor.offset_bytes))
            .map_err(|error| {
                stick_cursor_error(
                    db,
                    &mut cursor,
                    format!("AMBIENT_SOURCE_SEEK_FAILED: {error}"),
                )
            })?;
        file.read_to_end(&mut new_bytes).map_err(|error| {
            stick_cursor_error(
                db,
                &mut cursor,
                format!("AMBIENT_SOURCE_READ_FAILED: {error}"),
            )
        })?;
    }

    // Split complete lines; a trailing chunk without a newline waits for the
    // next cycle (the session is appended live, so a partial line is normal).
    // A trailing CR is part of a CRLF terminator and excluded from the hash.
    fn trim_terminator(line: &[u8]) -> &[u8] {
        line.strip_suffix(b"\r").unwrap_or(line)
    }
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut consumed_bytes = 0_usize;
    let mut start = 0_usize;
    for (index, byte) in new_bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(trim_terminator(&new_bytes[start..index]));
            start = index + 1;
            consumed_bytes = start;
        }
    }

    let first_registration = !cursor.registered;
    if lines.is_empty() && !first_registration {
        return Ok(SessionIngestOutcome::default());
    }

    // Single pressure authority for the cycle: rows below ride a bypass write.
    if !lines.is_empty() && !db.pressure_permits_write(cf::CF_AGENT_TRANSCRIPTS) {
        PRESSURE_DEFERRALS_TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            code = "AMBIENT_INGEST_PRESSURE_DEFERRED",
            spawn_id = %spawn_id,
            pending_lines = lines.len(),
            "disk pressure defers ambient ingestion; cursor not advanced"
        );
        return Ok(SessionIngestOutcome {
            deferred_for_pressure: true,
            ..SessionIngestOutcome::default()
        });
    }

    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(lines.len());
    let mut ts_index_rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(lines.len());
    let mut new_parsed = 0_u64;
    let mut new_invalid = 0_u64;
    let mut last_lifecycle: Option<Lifecycle> = None;
    for raw_line in &lines {
        let line_no = cursor.lines_ingested + 1;
        let (record, lifecycle) = parse_session_line(raw_line, line_no, &mut cursor);
        match record.status {
            TranscriptParseStatus::Parsed => new_parsed += 1,
            TranscriptParseStatus::Invalid => {
                new_invalid += 1;
                tracing::error!(
                    code = "AMBIENT_LINE_INVALID",
                    spawn_id = %spawn_id,
                    line_no,
                    detail = record.parse_error.as_deref().unwrap_or("unknown"),
                    "ambient source line refused by the session-file parser; invalid row written"
                );
            }
        }
        record
            .validate()
            .map_err(|detail| stick_cursor_error(db, &mut cursor, detail))?;
        let encoded =
            encode_json(&record).map_err(|error| format!("AMBIENT_ROW_ENCODE_FAILED: {error}"))?;
        if encoded.len() > MAX_VALUE_BYTES {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                format!(
                    "AMBIENT_ROW_OVERSIZED: line {line_no} encoded to {} bytes, cap is {MAX_VALUE_BYTES}",
                    encoded.len()
                ),
            ));
        }
        let transcript_key = agent_transcript_key(&spawn_id, line_no);
        ts_index_rows.push((
            agent_transcript_ts_index_key(record.ts_ns, &transcript_key),
            transcript_key.clone(),
        ));
        rows.push((transcript_key, encoded));
        cursor.lines_ingested = line_no;
        if let Some(signal) = lifecycle {
            last_lifecycle = Some(signal);
        }
    }

    if !rows.is_empty() {
        db.put_cf_batches_pressure_bypass(vec![
            (cf::CF_AGENT_TRANSCRIPTS, rows),
            (cf::CF_KV, ts_index_rows),
        ])
        .map_err(|error| {
            format!("AMBIENT_ROWS_WRITE_FAILED: {error} (cursor not advanced; lines re-ingest)")
        })?;
    }

    cursor.offset_bytes += consumed_bytes as u64;
    cursor.parsed_rows += new_parsed;
    cursor.invalid_rows += new_invalid;
    cursor.updated_ts_ns = unix_time_ns_now();
    LINES_PARSED_TOTAL.fetch_add(new_parsed, Ordering::Relaxed);
    LINES_INVALID_TOTAL.fetch_add(new_invalid, Ordering::Relaxed);

    // Registration first (so the agent exists), then at most one coalesced
    // lifecycle event reflecting the cycle's final observed activity.
    if first_registration {
        emit_registration(db, &cursor)?;
        cursor.registered = true;
    }
    if let Some(lifecycle) = &last_lifecycle {
        emit_lifecycle(db, &mut cursor, lifecycle)?;
    }
    store_cursor(db, &cursor)?;

    Ok(SessionIngestOutcome {
        new_parsed_rows: new_parsed,
        new_invalid_rows: new_invalid,
        newly_registered: first_registration,
        ..SessionIngestOutcome::default()
    })
}

fn seed_cursor(spawn_id: &str, session_id: &str, source_path: &Path) -> AmbientCursor {
    AmbientCursor {
        record_version: CURSOR_VERSION,
        spawn_id: spawn_id.to_owned(),
        session_id: session_id.to_owned(),
        source_path: source_path.display().to_string(),
        offset_bytes: 0,
        lines_ingested: 0,
        parsed_rows: 0,
        invalid_rows: 0,
        turn_index: 0,
        last_assistant_message_id: None,
        model: None,
        cwd: None,
        git_branch: None,
        registered: false,
        last_emitted_state: None,
        error: None,
        updated_ts_ns: unix_time_ns_now(),
    }
}

/// One discovery + ingest pass over every session file under `root`. Per-session
/// errors are sticky and already logged; the cycle continues so one corrupt
/// session can never stall the rest of the fleet.
pub(crate) fn ingest_all_once(db: &Db, root: &Path, max_idle_secs: u64) -> Value {
    CYCLES_TOTAL.fetch_add(1, Ordering::Relaxed);
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let project_dirs = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::error!(
                    code = "AMBIENT_INGEST_CYCLE_FAILED",
                    root = %root.display(),
                    detail = %error,
                    "ambient ingest cycle could not list the projects root"
                );
            }
            return json!({"sessions_seen": 0, "error": error.to_string()});
        }
    };

    let mut sessions_seen = 0_u64;
    let mut new_rows = 0_u64;
    let mut registered = 0_u64;
    let mut errors = 0_u64;
    let mut deferred = 0_u64;
    let mut skipped_stale = 0_u64;

    for project in project_dirs.flatten() {
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let files = match std::fs::read_dir(&project_path) {
            Ok(files) => files,
            Err(error) => {
                tracing::warn!(
                    code = "AMBIENT_PROJECT_DIR_UNREADABLE",
                    project = %project_path.display(),
                    detail = %error,
                    "ambient ingest could not list a project dir; skipping it this cycle"
                );
                continue;
            }
        };
        for file in files.flatten() {
            let path = file.path();
            // Main session transcripts only: `<project>/<uuid>.jsonl`. Subagent
            // sidecars live under `<uuid>/subagents/` and are a follow-up.
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !is_session_stem(stem) {
                continue;
            }
            let metadata = match file.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(
                        code = "AMBIENT_SESSION_STAT_FAILED",
                        path = %path.display(),
                        detail = %error,
                        "could not stat an ambient session file; skipping it this cycle"
                    );
                    continue;
                }
            };
            // Skip sessions idle longer than the window — unless we already
            // track them (a registered session must keep tailing its tail).
            let modified_secs = metadata
                .modified()
                .ok()
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let idle_secs = now_secs.saturating_sub(modified_secs);
            let already_tracked = load_cursor(db, &spawn_id_for_session(stem))
                .ok()
                .flatten()
                .is_some();
            if idle_secs > max_idle_secs && !already_tracked {
                skipped_stale += 1;
                continue;
            }

            sessions_seen += 1;
            match ingest_session_file(db, stem, &path) {
                Ok(outcome) => {
                    new_rows += outcome.new_parsed_rows + outcome.new_invalid_rows;
                    if outcome.newly_registered {
                        registered += 1;
                    }
                    if outcome.deferred_for_pressure {
                        deferred += 1;
                    }
                }
                Err(_detail) => errors += 1,
            }
        }
    }

    let summary = json!({
        "sessions_seen": sessions_seen,
        "new_rows": new_rows,
        "sessions_registered": registered,
        "errors": errors,
        "pressure_deferred": deferred,
        "skipped_stale": skipped_stale,
    });
    if new_rows > 0 || registered > 0 || errors > 0 {
        tracing::info!(
            code = "AMBIENT_INGEST_CYCLE_OK",
            sessions_seen,
            new_rows,
            sessions_registered = registered,
            errors,
            pressure_deferred = deferred,
            skipped_stale,
            "ambient ingest cycle finished"
        );
    } else {
        tracing::debug!(
            code = "AMBIENT_INGEST_CYCLE_IDLE",
            sessions_seen,
            skipped_stale,
            "ambient ingest cycle found nothing new"
        );
    }
    summary
}

/// Spawns the periodic ambient ingest task. Invalid env overrides are a startup
/// error (never a silently substituted schedule); `INTERVAL=0` disables it.
///
/// # Errors
///
/// Returns an error when an env override is present but unparseable.
pub(crate) fn spawn_periodic_ambient_ingest(
    m3_state: Arc<Mutex<M3State>>,
    cancel: CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = parse_secs_env(INTERVAL_ENV, DEFAULT_INTERVAL_SECS)?;
    let startup_delay_secs = parse_secs_env(STARTUP_DELAY_ENV, DEFAULT_STARTUP_DELAY_SECS)?;
    let max_idle_secs = parse_secs_env(MAX_IDLE_ENV, DEFAULT_MAX_IDLE_SECS)?;
    if interval_secs == 0 {
        tracing::info!(
            code = "AMBIENT_INGEST_PERIODIC_DISABLED",
            "periodic ambient agent discovery disabled via {INTERVAL_ENV}=0"
        );
        return Ok(None);
    }
    let db_path = configured_db_path(&m3_state)?;
    let Some(root_decision) =
        ambient_projects_root_for_db(&db_path).map_err(|detail| anyhow::anyhow!(detail))?
    else {
        tracing::warn!(
            code = "AMBIENT_INGEST_CUSTOM_DB_UNSCOPED",
            db_path = %db_path.display(),
            default_db_path = %default_db_path().display(),
            default_daemon_db_path = %default_daemon_db_path().display(),
            explicit_root_env = ROOT_ENV,
            remediation = "set SYNAPSE_AMBIENT_CLAUDE_PROJECTS_DIR for this run or use the configured daemon DB path",
            "periodic ambient agent discovery disabled for custom DB without an explicit ambient root"
        );
        return Ok(None);
    };
    let AmbientRootDecision { root, scope } = root_decision;
    tracing::info!(
        code = "AMBIENT_INGEST_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        max_idle_secs,
        root = %root.display(),
        root_scope = scope.as_str(),
        db_path = %db_path.display(),
        "periodic ambient agent discovery scheduled"
    );
    let handle = tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "AMBIENT_INGEST_PERIODIC_STOPPED",
                        "periodic ambient agent discovery stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            run_cycle(&m3_state, &root, max_idle_secs);
            delay = std::time::Duration::from_secs(interval_secs);
        }
    });
    Ok(Some(handle))
}

fn configured_db_path(m3_state: &Arc<Mutex<M3State>>) -> anyhow::Result<PathBuf> {
    let state = m3_state
        .lock()
        .map_err(|_poisoned| anyhow::anyhow!("m3 state lock poisoned"))?;
    Ok(state.db_path.clone().unwrap_or_else(default_db_path))
}

fn run_cycle(m3_state: &Arc<Mutex<M3State>>, root: &Path, max_idle_secs: u64) {
    let db = {
        let mut state = match m3_state.lock() {
            Ok(state) => state,
            Err(_poisoned) => {
                tracing::error!(
                    code = "AMBIENT_INGEST_CYCLE_FAILED",
                    detail = "m3 state lock poisoned",
                    "ambient ingest cycle could not access storage"
                );
                return;
            }
        };
        match state.ensure_storage() {
            Ok(db) => db,
            Err(error) => {
                tracing::error!(
                    code = "AMBIENT_INGEST_CYCLE_FAILED",
                    detail = %error,
                    "ambient ingest cycle could not open storage"
                );
                return;
            }
        }
    };
    let _summary = ingest_all_once(&db, root, max_idle_secs);
}

fn parse_secs_env(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Ok(raw) => raw.trim().parse::<u64>().map_err(|error| {
            anyhow::anyhow!("{name} must be a non-negative integer (seconds), got {raw:?}: {error}")
        }),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("{name} is not valid unicode: {error}")),
    }
}

// ---------------------------------------------------------------------------
// Session-file (~/.claude/projects) line parser
// ---------------------------------------------------------------------------

/// Parses one raw session-file line into exactly one transcript row plus an
/// optional lifecycle signal. Never fails: a line the vocabulary cannot place
/// becomes an `invalid` row carrying the structured reason.
fn parse_session_line(
    raw_line: &[u8],
    line_no: u64,
    cursor: &mut AmbientCursor,
) -> (AgentTranscriptRecord, Option<Lifecycle>) {
    let mut record = AgentTranscriptRecord::new(
        unix_time_ns_now(),
        cursor.spawn_id.clone(),
        line_no,
        TranscriptSource::ClaudeSessionJsonl,
        raw_line.len() as u64,
        sha256_hex(raw_line),
    );
    // The session id is the file identity; stamp it as the conversation id.
    record.conversation_id = Some(cursor.session_id.clone());

    let text = match std::str::from_utf8(raw_line) {
        Ok(text) => text,
        Err(error) => {
            record.status = TranscriptParseStatus::Invalid;
            record.parse_error = Some(format!("LINE_NOT_UTF8: {error}"));
            return (record, None);
        }
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            record.status = TranscriptParseStatus::Invalid;
            record.parse_error = Some(format!("LINE_NOT_JSON: {error}"));
            return (record, None);
        }
    };
    let Some(object) = value.as_object() else {
        record.status = TranscriptParseStatus::Invalid;
        record.parse_error = Some("LINE_NOT_JSON_OBJECT".to_owned());
        return (record, None);
    };

    match classify_session_object(object, &mut record, cursor) {
        Ok(lifecycle) => {
            if record.model.is_none() {
                record.model.clone_from(&cursor.model);
            }
            if cursor.turn_index > 0 {
                record.turn_index = Some(cursor.turn_index);
            }
            (record, lifecycle)
        }
        Err(detail) => {
            record.status = TranscriptParseStatus::Invalid;
            record.parse_error = Some(detail);
            record.role = None;
            record.event_kind = None;
            record.tool_calls.clear();
            record.usage = None;
            record.content_summary = None;
            record.content_bytes = None;
            record.content_sha256 = None;
            record.content_truncated = false;
            record.source_error = None;
            (record, None)
        }
    }
}

fn set_content(record: &mut AgentTranscriptRecord, content: &str) {
    let (summary, truncated) = bounded_chars(content, AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS);
    record.content_bytes = Some(content.len() as u64);
    record.content_sha256 = Some(sha256_hex(content.as_bytes()));
    record.content_summary = Some(summary);
    record.content_truncated = truncated;
}

/// The `~/.claude/projects/<slug>/<uuid>.jsonl` record vocabulary, pinned to the
/// shapes captured from real session files. Records carry an outer envelope
/// (`cwd`/`gitBranch`/`sessionId`) and, for conversational records, a `message`
/// that is the raw Anthropic API message.
fn classify_session_object(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
    cursor: &mut AmbientCursor,
) -> Result<Option<Lifecycle>, String> {
    // Harvest envelope context wherever it appears (not every record has it).
    if cursor.cwd.is_none()
        && let Some(cwd) = object.get("cwd").and_then(Value::as_str)
        && !cwd.is_empty()
    {
        cursor.cwd = Some(cwd.to_owned());
    }
    if let Some(branch) = object.get("gitBranch").and_then(Value::as_str)
        && !branch.is_empty()
    {
        cursor.git_branch = Some(branch.to_owned());
    }

    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "MISSING_TYPE: line has no string `type` field".to_owned())?;

    match event_type {
        "assistant" => classify_assistant(object, record, cursor),
        "user" => classify_user(object, record),
        "system" => {
            record.role = Some(TranscriptRole::System);
            let subtype = object.get("subtype").and_then(Value::as_str);
            record.event_kind = Some(subtype.map_or_else(
                || "system".to_owned(),
                |subtype| format!("system/{subtype}"),
            ));
            if let Some(content) = object.get("content").and_then(Value::as_str) {
                set_content(record, content);
            }
            Ok(None)
        }
        "summary" => {
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some("summary".to_owned());
            if let Some(summary) = object.get("summary").and_then(Value::as_str) {
                set_content(record, summary);
            }
            Ok(None)
        }
        // Documented session-metadata records that carry no conversational
        // content we normalize. They are part of the real vocabulary (verified
        // by enumerating every record type across the live ~/.claude/projects
        // transcripts), so they are carried as recognized system rows — never
        // refused as unknown.
        "mode"
        | "file-history-snapshot"
        | "ai-title"
        | "attachment"
        | "last-prompt"
        | "queue-operation"
        | "result"
        | "permission-mode"
        | "pr-link"
        | "worktree-state"
        | "agent-name" => {
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some(event_type.to_owned());
            Ok(None)
        }
        other => Err(format!("UNKNOWN_RECORD_TYPE: {other}")),
    }
}

fn classify_assistant(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
    cursor: &mut AmbientCursor,
) -> Result<Option<Lifecycle>, String> {
    let message = object
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| "ASSISTANT_MISSING_MESSAGE".to_owned())?;
    record.role = Some(TranscriptRole::Assistant);
    record.event_kind = Some("assistant".to_owned());

    if let Some(model) = message.get("model").and_then(Value::as_str) {
        record.model = Some(model.to_owned());
        cursor.model = Some(model.to_owned());
    }
    if let Some(message_id) = message.get("id").and_then(Value::as_str)
        && cursor.last_assistant_message_id.as_deref() != Some(message_id)
    {
        cursor.turn_index += 1;
        cursor.last_assistant_message_id = Some(message_id.to_owned());
    }

    let content = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "ASSISTANT_MISSING_CONTENT_ARRAY".to_owned())?;
    let mut text_parts: Vec<String> = Vec::new();
    let mut last_tool: Option<(String, String)> = None;
    for block in content {
        let block_object = block
            .as_object()
            .ok_or_else(|| "ASSISTANT_CONTENT_BLOCK_NOT_OBJECT".to_owned())?;
        let block_type = block_object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "ASSISTANT_CONTENT_BLOCK_MISSING_TYPE".to_owned())?;
        match block_type {
            "text" => {
                if let Some(text) = block_object.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_owned());
                }
            }
            "thinking" => {
                if let Some(text) = block_object.get("thinking").and_then(Value::as_str) {
                    text_parts.push(text.to_owned());
                }
            }
            "redacted_thinking" => {}
            // Model-fallback notice (e.g. claude-fable-5 -> claude-opus-4-8).
            // Small and self-describing; carry it verbatim like the #900 stream
            // parser does, rather than refusing the row.
            "fallback" => {
                text_parts.push(Value::Object(block_object.clone()).to_string());
            }
            "tool_use" | "server_tool_use" => {
                let tool_name = block_object
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "TOOL_USE_MISSING_NAME".to_owned())?
                    .to_owned();
                let input = block_object.get("input").cloned().unwrap_or(Value::Null);
                let (arguments, arguments_bytes, arguments_truncated) =
                    bounded_json_string(&input, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS);
                let input_sha = sha256_hex(input.to_string().as_bytes());
                record.tool_calls.push(TranscriptToolCall {
                    tool_name: tool_name.clone(),
                    tool_call_id: block_object
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    arguments: Some(arguments),
                    arguments_bytes: Some(arguments_bytes),
                    arguments_truncated,
                    ..TranscriptToolCall::default()
                });
                last_tool = Some((tool_name, input_sha));
            }
            other => {
                return Err(format!("UNKNOWN_ASSISTANT_CONTENT_BLOCK: {other}"));
            }
        }
    }
    if !text_parts.is_empty() {
        set_content(record, &text_parts.join("\n"));
    }
    record.usage = message.get("usage").map(claude_usage);

    // Persisted assistant messages are split one record per content block, so
    // a record may carry only `thinking`/`text` while its turn still issues a
    // tool on a sibling record. Derive the signal from the tool block when
    // present, else from `stop_reason`:
    //   - a tool block             -> ToolUse (named, with input hash)
    //   - stop_reason == tool_use  -> Working (tool name is on a sibling record)
    //   - stop_reason missing/null -> Working (still streaming)
    //   - any other stop_reason    -> Idle (end_turn/stop_sequence/max_tokens/…)
    let lifecycle = if let Some((tool_name, input_sha256)) = last_tool {
        Lifecycle::ToolUse {
            tool_name,
            input_sha256,
        }
    } else {
        match message.get("stop_reason").and_then(Value::as_str) {
            Some("tool_use") | None => Lifecycle::Working,
            Some(_) => Lifecycle::Idle,
        }
    };
    Ok(Some(lifecycle))
}

fn classify_user(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
) -> Result<Option<Lifecycle>, String> {
    let is_meta = object
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = object
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| "USER_MISSING_MESSAGE".to_owned())?;
    let content = message
        .get("content")
        .ok_or_else(|| "USER_MISSING_CONTENT".to_owned())?;

    // A `user` record is either a real human prompt (string, or array of text)
    // or a tool_result fed back to the model (array of tool_result blocks).
    let mut tool_results = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    match content {
        Value::String(text) => text_parts.push(text.clone()),
        Value::Array(blocks) => {
            for block in blocks {
                let block_object = block
                    .as_object()
                    .ok_or_else(|| "USER_CONTENT_BLOCK_NOT_OBJECT".to_owned())?;
                match block_object.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        let (result_summary, result_bytes, result_truncated) = block_object
                            .get("content")
                            .map(|content| {
                                bounded_json_string(content, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS)
                            })
                            .unwrap_or_default();
                        tool_results.push(TranscriptToolCall {
                            tool_name: "tool_result".to_owned(),
                            tool_call_id: block_object
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            result_summary: Some(result_summary),
                            result_bytes: Some(result_bytes),
                            result_truncated,
                            status: block_object
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .and_then(|is_error| is_error.then(|| "error".to_owned())),
                            ..TranscriptToolCall::default()
                        });
                    }
                    Some("text") => {
                        if let Some(text) = block_object.get("text").and_then(Value::as_str) {
                            text_parts.push(text.to_owned());
                        }
                    }
                    // Images and other prompt attachments: presence noted, body
                    // not normalized.
                    Some(_) | None => {}
                }
            }
        }
        _ => return Err("USER_CONTENT_NOT_STRING_OR_ARRAY".to_owned()),
    }

    if !tool_results.is_empty() {
        record.role = Some(TranscriptRole::Tool);
        record.event_kind = Some("user/tool_result".to_owned());
        record.tool_calls = tool_results;
        return Ok(None);
    }

    record.role = Some(TranscriptRole::System);
    if !text_parts.is_empty() {
        set_content(record, &text_parts.join("\n"));
    }
    if is_meta {
        record.event_kind = Some("user/meta".to_owned());
        Ok(None)
    } else {
        record.event_kind = Some("user/prompt".to_owned());
        Ok(Some(Lifecycle::TurnStarted))
    }
}

/// Normalizes an Anthropic `message.usage` object onto [`TranscriptUsage`],
/// including the 5m/1h cache-creation TTL split (#949).
fn claude_usage(usage: &Value) -> TranscriptUsage {
    let cache_creation = usage.get("cache_creation");
    let tier = |field: &str| -> Option<u64> {
        cache_creation
            .and_then(|cc| cc.get(field))
            .and_then(Value::as_u64)
    };
    TranscriptUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cache_read_input_tokens: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        cache_creation_5m_input_tokens: tier("ephemeral_5m_input_tokens"),
        cache_creation_1h_input_tokens: tier("ephemeral_1h_input_tokens"),
        reasoning_output_tokens: None,
        total_cost_micro_usd: None,
        model_usage: Vec::new(),
    }
}

#[cfg(test)]
mod tests;
