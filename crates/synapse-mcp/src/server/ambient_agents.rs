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
//! (`mode`/`file-history-snapshot`/`file-history-delta`/`summary`/`ai-title`/...).
//! Hence a dedicated parser and the [`TranscriptSource::ClaudeSessionJsonl`] tag. Parsing is
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
//! Registration/lifecycle evidence uses a transactional outbox embedded in the
//! same guarded cursor row as its covered source checkpoint. Relay stamps a
//! stable operation identity on the bounded primary event batch, reads the
//! physical journal independently, and acknowledges only an exact set. An
//! unresolved event or acknowledgement commit latches relay fail-stop until
//! daemon/vault reopen rebuilds state from durable WAL reality (#1771). When a
//! retained exact batch predates the global 24-hour startup projection window,
//! relay quietly replays those exact physical rows into the live tracker under
//! its normal locks, verifies the singleton and journal again, and never
//! re-journals replacement evidence (#1772).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    retention::{DEFAULTS as RETENTION_DEFAULTS, RetentionTtl},
};

type AmbientOutboxScanReadback = (Vec<(Vec<u8>, Vec<u8>)>, usize, u64, u64);
use synapse_storage::{
    Db,
    agent_events::agent_event_key,
    agent_transcripts::agent_transcript_key,
    cf,
    constellations::{TRANSCRIPT_TOOL_STATUS_ERROR, TRANSCRIPT_TOOL_STATUS_OK},
    decode_json, encode_json,
};
use tokio_util::sync::CancellationToken;

use super::{
    agent_events::{
        provider_for_agent_kind, record_agent_events, unix_time_ns_now, validate_and_encode,
    },
    agent_state::AmbientExactJournalRow,
    agent_transcripts::{
        BoundedTailRead, BoundedTranscriptTailReader, MAX_AGENT_TRANSCRIPT_COMMIT_ROWS,
        MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES, PreparedTranscriptRow,
        TranscriptSourceBoundaryState, commit_transcript_chunk, ensure_transcript_source_boundary,
        ensure_transcript_source_fingerprint, refresh_transcript_source_boundary,
        transcript_source_ts_ns,
    },
};
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
const AMBIENT_OUTBOX_VERSION: u32 = 1;
const AMBIENT_PROJECTION_CHECKPOINT_VERSION: u32 = 1;
const AMBIENT_OUTBOX_OPERATION_FIELD: &str = "ambient_outbox_operation_id";
const MAX_AMBIENT_OUTBOX_RECORDS: usize = 3;
const AMBIENT_OUTBOX_SCAN_PAGE_ROWS: usize = 256;
const MAX_AMBIENT_OUTBOX_TIMESTAMP_CANDIDATES: usize = 4 * 1024;
const NS_PER_MS: u64 = 1_000_000;

/// Hard cap on one encoded transcript row (matches the #900 ingester). Per-field
/// bounds keep real rows far below this; exceeding it is an ingester bug.
const MAX_VALUE_BYTES: usize = 32 * 1024;
const MAX_ROWS_PER_PASS: usize = 4 * MAX_AGENT_TRANSCRIPT_COMMIT_ROWS;
const MAX_SOURCE_BYTES_PER_PASS: u64 = 32 * 1024 * 1024;

static LINES_PARSED_TOTAL: AtomicU64 = AtomicU64::new(0);
static LINES_INVALID_TOTAL: AtomicU64 = AtomicU64::new(0);
static SESSIONS_REGISTERED_TOTAL: AtomicU64 = AtomicU64::new(0);
static INGEST_ERRORS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PRESSURE_DEFERRALS_TOTAL: AtomicU64 = AtomicU64::new(0);
static CYCLES_TOTAL: AtomicU64 = AtomicU64::new(0);
static OUTBOX_ACKNOWLEDGED_TOTAL: AtomicU64 = AtomicU64::new(0);
static OUTBOX_RECOVERED_TOTAL: AtomicU64 = AtomicU64::new(0);
static OUTBOX_PROJECTION_RECOVERED_TOTAL: AtomicU64 = AtomicU64::new(0);
static AMBIENT_OUTBOX_RECONCILIATION_LATCH: AtomicBool = AtomicBool::new(false);

fn ambient_outbox_relay_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

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
        "outbox_acknowledged_total": OUTBOX_ACKNOWLEDGED_TOTAL.load(Ordering::Relaxed),
        "outbox_recovered_total": OUTBOX_RECOVERED_TOTAL.load(Ordering::Relaxed),
        "outbox_projection_recovered_total": OUTBOX_PROJECTION_RECOVERED_TOTAL.load(Ordering::Relaxed),
        "outbox_reconciliation_latched": AMBIENT_OUTBOX_RECONCILIATION_LATCH.load(Ordering::Acquire),
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

/// Bounded transactional-outbox payload carried inside the authoritative
/// ambient cursor. The source checkpoint and its event intent therefore share
/// one CAS-guarded Calyx row; relay acknowledgement is reconstructed from the
/// physical journal after any process-stop or cursor-write ambiguity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AmbientEventOutbox {
    record_version: u32,
    operation_id: String,
    checkpoint_offset_bytes: u64,
    checkpoint_lines_ingested: u64,
    event_ts_ns: u64,
    records: Vec<AgentEventRecord>,
    acknowledge_registration: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acknowledge_state: Option<String>,
}

#[derive(Serialize)]
struct AmbientEventOutboxIdentity<'a> {
    record_version: u32,
    spawn_id: &'a str,
    session_id: &'a str,
    checkpoint_offset_bytes: u64,
    checkpoint_lines_ingested: u64,
    event_ts_ns: u64,
    records: &'a [AgentEventRecord],
    acknowledge_registration: bool,
    acknowledge_state: Option<&'a str>,
}

/// Durable bounded replay pointer retained after outbox acknowledgement. The
/// journal keys plus the original operation intent let every later daemon
/// process restore the same live projection while the 30-day source rows are
/// retained, without appending replacement evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AmbientProjectionCheckpoint {
    record_version: u32,
    outbox: AmbientEventOutbox,
    journal_keys: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AmbientOutboxJournalState {
    Absent,
    Exact(Vec<AmbientExactJournalRow>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AmbientProjectionCheckpointJournalState {
    Absent,
    Exact(Vec<AmbientExactJournalRow>),
}

#[derive(Debug)]
struct AmbientOutboxRelayOutcome {
    cursor: AmbientCursor,
    revision_sha256: Option<[u8; 32]>,
    newly_registered: bool,
}

#[derive(Debug)]
struct AmbientProjectionCheckpointOutcome {
    cursor: AmbientCursor,
    revision_sha256: Option<[u8; 32]>,
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
    /// Stable source-time seed for rows whose session-file JSON lacks a
    /// timestamp/UUIDv7 time anchor. This keeps reingest idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_epoch_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_fingerprint_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_fingerprint_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_boundary_start_offset_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_boundary_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_boundary_sha256: Option<String>,
    /// True once the `SpawnRequested`/`SpawnReady` registration rows are
    /// journaled. Restart-safe: the journal rebuild restores the agent, so a
    /// registered cursor never re-emits registration.
    registered: bool,
    /// The last lifecycle state event we emitted, so a cycle that produces the
    /// same signal does not re-journal it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_emitted_state: Option<String>,
    /// Exact bounded primary-event intent committed in the same authoritative
    /// cursor row as the source checkpoint it represents. It is cleared only
    /// after exact `CF_AGENT_EVENTS` readback proves delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_event_outbox: Option<AmbientEventOutbox>,
    /// Latest acknowledged lifecycle operation and its exact physical keys.
    /// This is a bounded materialized-view replay checkpoint, not pending work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection_checkpoint: Option<AmbientProjectionCheckpoint>,
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
    cancelled: bool,
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

#[derive(Debug)]
struct LoadedAmbientCursor {
    cursor: Option<AmbientCursor>,
    revision_sha256: Option<[u8; 32]>,
}

fn load_cursor(db: &Db, spawn_id: &str) -> Result<LoadedAmbientCursor, String> {
    let key = cursor_kv_key(spawn_id);
    let Some(revisioned) = db.get_cf_revisioned(cf::CF_KV, &key).map_err(|error| {
        format!(
            "AMBIENT_CURSOR_READ_FAILED: spawn_id={spawn_id} key={CURSOR_KV_PREFIX}{spawn_id}: {error}; remediation=repair the exact Calyx cursor point-read before ingesting source bytes"
        )
    })? else {
        return Ok(LoadedAmbientCursor {
            cursor: None,
            revision_sha256: None,
        });
    };
    let value = revisioned.value.ok_or_else(|| {
        format!(
            "AMBIENT_CURSOR_EXPIRED: spawn_id={spawn_id} key={CURSOR_KV_PREFIX}{spawn_id}; remediation=restore the non-expiring cursor row or reconcile all physical transcript rows before rebuilding it"
        )
    })?;
    let cursor: AmbientCursor = decode_json(&value).map_err(|error| {
        format!(
            "AMBIENT_CURSOR_DECODE_FAILED: spawn_id={spawn_id} key={CURSOR_KV_PREFIX}{spawn_id}: {error}; remediation=repair the cursor bytes from the physical source/transcript SoTs"
        )
    })?;
    if cursor.record_version != CURSOR_VERSION
        || cursor.spawn_id != spawn_id
        || cursor.source_path.trim().is_empty()
    {
        return Err(format!(
            "AMBIENT_CURSOR_IDENTITY_INVALID: requested_spawn_id={spawn_id} stored_spawn_id={} record_version={} expected_version={CURSOR_VERSION} source_path={:?}; remediation=repair the cursor identity from the physical source/transcript SoTs",
            cursor.spawn_id, cursor.record_version, cursor.source_path
        ));
    }
    if let Some(outbox) = &cursor.pending_event_outbox {
        validate_event_outbox(&cursor, outbox)?;
    }
    if let Some(checkpoint) = &cursor.projection_checkpoint {
        validate_projection_checkpoint(&cursor, checkpoint)?;
    }
    Ok(LoadedAmbientCursor {
        cursor: Some(cursor),
        revision_sha256: Some(revisioned.revision_sha256),
    })
}

fn store_cursor(
    db: &Db,
    cursor: &AmbientCursor,
    expected_revision_sha256: &mut Option<[u8; 32]>,
) -> Result<(), String> {
    let key = cursor_kv_key(&cursor.spawn_id);
    let encoded =
        encode_json(cursor).map_err(|error| format!("AMBIENT_CURSOR_ENCODE_FAILED: {error}"))?;
    let outcome = db
        .put_batch_if_revision_pressure_bypass(
            cf::CF_KV,
            &key,
            *expected_revision_sha256,
            [(key.clone(), encoded.clone())],
        )
        .map_err(|error| {
            format!(
                "AMBIENT_CURSOR_WRITE_FAILED: spawn_id={} source_path={} offset_bytes={}: {error}; remediation=repair the guarded Calyx cursor write and retry from the last persisted cursor",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            )
        })?;
    if !outcome.applied {
        return Err(format!(
            "AMBIENT_CURSOR_REVISION_CONFLICT: spawn_id={} source_path={} offset_bytes={} expected_revision_sha256={} actual_revision_sha256={}; remediation=discard this stale ingest pass and reload the authoritative cursor",
            cursor.spawn_id,
            cursor.source_path,
            cursor.offset_bytes,
            (*expected_revision_sha256).map_or_else(
                || "absent".to_owned(),
                |value| synapse_storage::constellations::hex_encode(&value),
            ),
            outcome.previous_revision_sha256.map_or_else(
                || "absent".to_owned(),
                |value| { synapse_storage::constellations::hex_encode(&value) }
            )
        ));
    }
    let committed_revision = outcome.committed_revision_sha256.ok_or_else(|| {
        format!(
            "AMBIENT_CURSOR_WRITE_OUTCOME_INVALID: spawn_id={} offset_bytes={} applied write omitted committed revision; remediation=repair the Calyx guarded-write outcome contract",
            cursor.spawn_id, cursor.offset_bytes
        )
    })?;
    let readback = db
        .get_cf_revisioned(cf::CF_KV, &key)
        .map_err(|error| {
            format!(
                "AMBIENT_CURSOR_READBACK_FAILED: spawn_id={} source_path={} offset_bytes={}: {error}; remediation=repair the Calyx point-read path and reconcile the committed cursor",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            )
        })?
        .ok_or_else(|| {
            format!(
                "AMBIENT_CURSOR_READBACK_MISSING: spawn_id={} source_path={} offset_bytes={}; remediation=repair the missing committed cursor before ingest resumes",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            )
        })?;
    if readback.revision_sha256 != committed_revision
        || readback.value.as_deref() != Some(encoded.as_slice())
    {
        return Err(format!(
            "AMBIENT_CURSOR_READBACK_MISMATCH: spawn_id={} source_path={} offset_bytes={} expected_value_sha256={} actual_value_sha256={} expected_revision_sha256={} actual_revision_sha256={}; remediation=quarantine and repair the divergent cursor before ingest resumes",
            cursor.spawn_id,
            cursor.source_path,
            cursor.offset_bytes,
            sha256_hex(&encoded),
            readback
                .value
                .as_deref()
                .map_or_else(|| "absent_or_expired".to_owned(), sha256_hex),
            synapse_storage::constellations::hex_encode(&committed_revision),
            synapse_storage::constellations::hex_encode(&readback.revision_sha256)
        ));
    }
    *expected_revision_sha256 = Some(committed_revision);
    Ok(())
}

/// Marks a session's cursor with a sticky error, logs it once with full
/// context, and persists it so later cycles skip the session.
fn stick_cursor_error(
    db: &Db,
    cursor: &mut AmbientCursor,
    cursor_revision_sha256: &mut Option<[u8; 32]>,
    detail: String,
) -> String {
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
    if let Err(store_error) = store_cursor(db, cursor, cursor_revision_sha256) {
        tracing::error!(
            code = "AMBIENT_INGEST_ERROR",
            spawn_id = %cursor.spawn_id,
            detail = %store_error,
            "failed to persist the sticky ambient cursor error itself"
        );
    }
    detail
}

fn lifecycle_state_tag(lifecycle: &Lifecycle) -> String {
    match lifecycle {
        Lifecycle::ToolUse { tool_name, .. } => format!("tool:{tool_name}"),
        Lifecycle::Working => "working".to_owned(),
        Lifecycle::Idle => "idle".to_owned(),
        Lifecycle::TurnStarted => "turn_started".to_owned(),
    }
}

/// Builds the two-row registration (`SpawnRequested` -> `SpawnReady`) without
/// writing it. The exact records live in the cursor outbox before relay.
fn registration_records(cursor: &AmbientCursor, event_ts_ns: u64) -> Vec<AgentEventRecord> {
    let provider = provider_for_agent_kind(AGENT_KIND);

    let mut requested = AgentEventRecord::new(event_ts_ns, AgentEventKind::SpawnRequested);
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

    let mut ready = AgentEventRecord::new(event_ts_ns, AgentEventKind::SpawnReady);
    ready.spawn_id = Some(cursor.spawn_id.clone());
    ready.reason_code = Some("ambient_observed".to_owned());
    ready.attributes.agent_name = Some(AGENT_KIND.to_owned());
    ready.attributes.provider_name = provider;
    ready.attributes.conversation_id = Some(cursor.session_id.clone());
    // No owned process: ambient agents are observed, not launched, so there is
    // no launcher/agent pid or log dir to record here.
    ready.payload = json!({ "source": "ambient_transcript", "ambient": true });

    vec![requested, ready]
}

fn lifecycle_record(
    cursor: &AmbientCursor,
    lifecycle: &Lifecycle,
    event_ts_ns: u64,
) -> AgentEventRecord {
    let mut record = match lifecycle {
        Lifecycle::ToolUse {
            tool_name,
            input_sha256,
        } => {
            let mut record = AgentEventRecord::new(event_ts_ns, AgentEventKind::ToolCallStarted);
            record.reason_code = Some("ambient_tool_activity".to_owned());
            record.attributes.operation_name = Some(GenAiOperationName::ExecuteTool);
            record.attributes.tool_name = Some(tool_name.clone());
            record.payload = json!({ "tool_input_sha256": input_sha256, "ambient": true });
            record
        }
        // Mid-turn activity with no tool name: `ToolCallFinished` reduces to
        // Working without resetting the turn or runaway counters.
        Lifecycle::Working => {
            let mut record = AgentEventRecord::new(event_ts_ns, AgentEventKind::ToolCallFinished);
            record.reason_code = Some("ambient_active".to_owned());
            record
        }
        Lifecycle::Idle => {
            let mut record = AgentEventRecord::new(event_ts_ns, AgentEventKind::TurnFinished);
            record.reason_code = Some("ambient_turn_finished".to_owned());
            record
        }
        Lifecycle::TurnStarted => {
            let mut record = AgentEventRecord::new(event_ts_ns, AgentEventKind::TurnStarted);
            record.reason_code = Some("ambient_turn_started".to_owned());
            record
        }
    };
    record.spawn_id = Some(cursor.spawn_id.clone());
    record.attributes.agent_name = Some(AGENT_KIND.to_owned());
    record.attributes.conversation_id = Some(cursor.session_id.clone());
    record
}

fn normalize_outbox_payload(record: &mut AgentEventRecord) -> Result<(), String> {
    if record.payload.is_null() {
        record.payload = Value::Object(Map::new());
    }
    if !record.payload.is_object() {
        return Err(format!(
            "AMBIENT_OUTBOX_PAYLOAD_INVALID: kind={:?} payload must be an object; remediation=repair the ambient event builder before advancing its cursor",
            record.kind
        ));
    }
    Ok(())
}

fn outbox_operation_id(
    cursor: &AmbientCursor,
    event_ts_ns: u64,
    records: &[AgentEventRecord],
    acknowledge_registration: bool,
    acknowledge_state: Option<&str>,
) -> Result<String, String> {
    let identity = AmbientEventOutboxIdentity {
        record_version: AMBIENT_OUTBOX_VERSION,
        spawn_id: &cursor.spawn_id,
        session_id: &cursor.session_id,
        checkpoint_offset_bytes: cursor.offset_bytes,
        checkpoint_lines_ingested: cursor.lines_ingested,
        event_ts_ns,
        records,
        acknowledge_registration,
        acknowledge_state,
    };
    let encoded = encode_json(&identity).map_err(|error| {
        format!(
            "AMBIENT_OUTBOX_IDENTITY_ENCODE_FAILED: spawn_id={} offset_bytes={} lines_ingested={}: {error}; remediation=repair the bounded outbox identity serializer before cursor advancement",
            cursor.spawn_id, cursor.offset_bytes, cursor.lines_ingested
        )
    })?;
    Ok(sha256_hex(&encoded))
}

fn stamp_outbox_operation(
    records: &mut [AgentEventRecord],
    operation_id: &str,
) -> Result<(), String> {
    for record in records {
        normalize_outbox_payload(record)?;
        let payload = record.payload.as_object_mut().ok_or_else(|| {
            format!(
                "AMBIENT_OUTBOX_PAYLOAD_INVALID: kind={:?} normalized payload is not an object",
                record.kind
            )
        })?;
        payload.insert(
            AMBIENT_OUTBOX_OPERATION_FIELD.to_owned(),
            Value::String(operation_id.to_owned()),
        );
    }
    Ok(())
}

fn build_event_outbox(
    cursor: &AmbientCursor,
    lifecycle: Option<&Lifecycle>,
) -> Result<Option<AmbientEventOutbox>, String> {
    let acknowledge_registration = !cursor.registered;
    let acknowledge_state = lifecycle
        .map(lifecycle_state_tag)
        .filter(|state| cursor.last_emitted_state.as_deref() != Some(state.as_str()));
    if !acknowledge_registration && acknowledge_state.is_none() {
        return Ok(None);
    }

    let event_ts_ns = unix_time_ns_now();
    let mut records = if acknowledge_registration {
        registration_records(cursor, event_ts_ns)
    } else {
        Vec::new()
    };
    if acknowledge_state.is_some() {
        let lifecycle = lifecycle.ok_or_else(|| {
            "AMBIENT_OUTBOX_STATE_WITHOUT_SIGNAL: lifecycle acknowledgement has no source signal; remediation=repair the ambient outbox builder".to_owned()
        })?;
        records.push(lifecycle_record(cursor, lifecycle, event_ts_ns));
    }
    for record in &mut records {
        normalize_outbox_payload(record)?;
    }
    let operation_id = outbox_operation_id(
        cursor,
        event_ts_ns,
        &records,
        acknowledge_registration,
        acknowledge_state.as_deref(),
    )?;
    stamp_outbox_operation(&mut records, &operation_id)?;
    let outbox = AmbientEventOutbox {
        record_version: AMBIENT_OUTBOX_VERSION,
        operation_id,
        checkpoint_offset_bytes: cursor.offset_bytes,
        checkpoint_lines_ingested: cursor.lines_ingested,
        event_ts_ns,
        records,
        acknowledge_registration,
        acknowledge_state,
    };
    validate_event_outbox(cursor, &outbox)?;
    Ok(Some(outbox))
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn event_matches_state(record: &AgentEventRecord, state: &str) -> bool {
    match state {
        "working" => record.kind == AgentEventKind::ToolCallFinished,
        "idle" => record.kind == AgentEventKind::TurnFinished,
        "turn_started" => record.kind == AgentEventKind::TurnStarted,
        _ => state.strip_prefix("tool:").is_some_and(|tool_name| {
            !tool_name.is_empty()
                && record.kind == AgentEventKind::ToolCallStarted
                && record.attributes.tool_name.as_deref() == Some(tool_name)
        }),
    }
}

fn validate_event_outbox_identity(
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
) -> Result<(), String> {
    if outbox.record_version != AMBIENT_OUTBOX_VERSION
        || !is_lower_sha256(&outbox.operation_id)
        || outbox.checkpoint_offset_bytes != cursor.offset_bytes
        || outbox.checkpoint_lines_ingested != cursor.lines_ingested
        || outbox.event_ts_ns == 0
        || outbox.records.is_empty()
        || outbox.records.len() > MAX_AMBIENT_OUTBOX_RECORDS
    {
        return Err(format!(
            "AMBIENT_OUTBOX_ENVELOPE_INVALID: spawn_id={} operation_id={:?} version={} expected_version={AMBIENT_OUTBOX_VERSION} checkpoint=({}, {}) cursor=({}, {}) event_ts_ns={} records={}; remediation=repair the cursor/outbox from the physical source and CF_AGENT_EVENTS SoTs",
            cursor.spawn_id,
            outbox.operation_id,
            outbox.record_version,
            outbox.checkpoint_offset_bytes,
            outbox.checkpoint_lines_ingested,
            cursor.offset_bytes,
            cursor.lines_ingested,
            outbox.event_ts_ns,
            outbox.records.len()
        ));
    }
    let mut requested = 0_usize;
    let mut ready = 0_usize;
    let mut state_records = 0_usize;
    let mut clean_records = Vec::with_capacity(outbox.records.len());
    for record in &outbox.records {
        if record.ts_ns != outbox.event_ts_ns
            || record.spawn_id.as_deref() != Some(cursor.spawn_id.as_str())
            || record.session_id.is_some()
            || record.attributes.conversation_id.as_deref() != Some(cursor.session_id.as_str())
        {
            return Err(format!(
                "AMBIENT_OUTBOX_RECORD_IDENTITY_INVALID: spawn_id={} operation_id={} kind={:?} record_ts_ns={} expected_ts_ns={} record_spawn_id={:?} record_session_id={:?} conversation_id={:?}; remediation=repair the corrupt cursor outbox before relay",
                cursor.spawn_id,
                outbox.operation_id,
                record.kind,
                record.ts_ns,
                outbox.event_ts_ns,
                record.spawn_id,
                record.session_id,
                record.attributes.conversation_id
            ));
        }
        let payload = record.payload.as_object().ok_or_else(|| {
            format!(
                "AMBIENT_OUTBOX_RECORD_PAYLOAD_INVALID: spawn_id={} operation_id={} kind={:?}; remediation=repair the corrupt cursor outbox before relay",
                cursor.spawn_id, outbox.operation_id, record.kind
            )
        })?;
        if payload
            .get(AMBIENT_OUTBOX_OPERATION_FIELD)
            .and_then(Value::as_str)
            != Some(outbox.operation_id.as_str())
        {
            return Err(format!(
                "AMBIENT_OUTBOX_OPERATION_STAMP_MISMATCH: spawn_id={} operation_id={} kind={:?}; remediation=repair the corrupt cursor outbox before relay",
                cursor.spawn_id, outbox.operation_id, record.kind
            ));
        }
        let _encoded = validate_and_encode(record).map_err(|error| {
            format!(
                "AMBIENT_OUTBOX_RECORD_INVALID: spawn_id={} operation_id={} kind={:?}: {error}; remediation=repair the invalid cursor outbox before relay",
                cursor.spawn_id, outbox.operation_id, record.kind
            )
        })?;

        match record.kind {
            AgentEventKind::SpawnRequested => requested += 1,
            AgentEventKind::SpawnReady => ready += 1,
            _ if outbox
                .acknowledge_state
                .as_deref()
                .is_some_and(|state| event_matches_state(record, state)) =>
            {
                state_records += 1;
            }
            _ => {
                return Err(format!(
                    "AMBIENT_OUTBOX_RECORD_KIND_INVALID: spawn_id={} operation_id={} kind={:?} acknowledge_state={:?}; remediation=repair the corrupt cursor outbox before relay",
                    cursor.spawn_id, outbox.operation_id, record.kind, outbox.acknowledge_state
                ));
            }
        }

        let mut clean = record.clone();
        let clean_payload = clean.payload.as_object_mut().ok_or_else(|| {
            "AMBIENT_OUTBOX_RECORD_PAYLOAD_INVALID: validated payload lost object shape".to_owned()
        })?;
        clean_payload.remove(AMBIENT_OUTBOX_OPERATION_FIELD);
        clean_records.push(clean);
    }
    let expected_registration_rows = usize::from(outbox.acknowledge_registration);
    let expected_state_rows = usize::from(outbox.acknowledge_state.is_some());
    if requested != expected_registration_rows
        || ready != expected_registration_rows
        || state_records != expected_state_rows
        || outbox.records.len() != expected_registration_rows * 2 + expected_state_rows
    {
        return Err(format!(
            "AMBIENT_OUTBOX_RECORD_SET_INVALID: spawn_id={} operation_id={} requested={} ready={} state_records={} expected_registration={} expected_state={}; remediation=repair the corrupt bounded event set before relay",
            cursor.spawn_id,
            outbox.operation_id,
            requested,
            ready,
            state_records,
            outbox.acknowledge_registration,
            outbox.acknowledge_state.is_some()
        ));
    }
    let expected_operation_id = outbox_operation_id(
        cursor,
        outbox.event_ts_ns,
        &clean_records,
        outbox.acknowledge_registration,
        outbox.acknowledge_state.as_deref(),
    )?;
    if expected_operation_id != outbox.operation_id {
        return Err(format!(
            "AMBIENT_OUTBOX_OPERATION_ID_MISMATCH: spawn_id={} stored_operation_id={} expected_operation_id={expected_operation_id}; remediation=repair the corrupt cursor outbox from the exact source/journal SoTs",
            cursor.spawn_id, outbox.operation_id
        ));
    }
    Ok(())
}

fn validate_event_outbox(
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
) -> Result<(), String> {
    validate_event_outbox_identity(cursor, outbox)?;
    if (!outbox.acknowledge_registration && outbox.acknowledge_state.is_none())
        || (outbox.acknowledge_registration && cursor.registered)
        || (!outbox.acknowledge_registration && !cursor.registered)
        || outbox
            .acknowledge_state
            .as_deref()
            .is_some_and(|state| cursor.last_emitted_state.as_deref() == Some(state))
    {
        return Err(format!(
            "AMBIENT_OUTBOX_EFFECT_INVALID: spawn_id={} operation_id={} cursor_registered={} acknowledge_registration={} cursor_state={:?} acknowledge_state={:?}; remediation=repair the cursor/outbox acknowledgement state from the exact journal rows",
            cursor.spawn_id,
            outbox.operation_id,
            cursor.registered,
            outbox.acknowledge_registration,
            cursor.last_emitted_state,
            outbox.acknowledge_state
        ));
    }
    Ok(())
}

fn validate_projection_checkpoint(
    cursor: &AmbientCursor,
    checkpoint: &AmbientProjectionCheckpoint,
) -> Result<(), String> {
    if checkpoint.record_version != AMBIENT_PROJECTION_CHECKPOINT_VERSION
        || checkpoint.journal_keys.len() != checkpoint.outbox.records.len()
        || checkpoint.journal_keys.is_empty()
        || checkpoint.journal_keys.len() > MAX_AMBIENT_OUTBOX_RECORDS
    {
        return Err(format!(
            "AMBIENT_PROJECTION_CHECKPOINT_ENVELOPE_INVALID: spawn_id={} version={} expected_version={AMBIENT_PROJECTION_CHECKPOINT_VERSION} journal_keys={} records={}; remediation=repair the bounded cursor checkpoint from exact CF_AGENT_EVENTS rows",
            cursor.spawn_id,
            checkpoint.record_version,
            checkpoint.journal_keys.len(),
            checkpoint.outbox.records.len()
        ));
    }
    let mut identity_cursor = cursor.clone();
    identity_cursor.offset_bytes = checkpoint.outbox.checkpoint_offset_bytes;
    identity_cursor.lines_ingested = checkpoint.outbox.checkpoint_lines_ingested;
    validate_event_outbox_identity(&identity_cursor, &checkpoint.outbox)?;
    let mut previous_key: Option<&[u8]> = None;
    for key in &checkpoint.journal_keys {
        if previous_key.is_some_and(|previous| previous >= key.as_slice()) {
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_KEY_ORDER_INVALID: spawn_id={} operation_id={}; remediation=repair the checkpoint from the exact ordered journal scan",
                cursor.spawn_id, checkpoint.outbox.operation_id
            ));
        }
        let (key_ts_ns, _key_seq) =
            synapse_storage::agent_events::decode_agent_event_key(key).map_err(|error| {
                format!(
                    "AMBIENT_PROJECTION_CHECKPOINT_KEY_INVALID: spawn_id={} operation_id={} key_hex={}: {error}; remediation=repair the invalid physical journal identity",
                    cursor.spawn_id,
                    checkpoint.outbox.operation_id,
                    synapse_storage::constellations::hex_encode(key)
                )
            })?;
        if key_ts_ns != checkpoint.outbox.event_ts_ns {
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_TIMESTAMP_MISMATCH: spawn_id={} operation_id={} key_ts_ns={key_ts_ns} expected_event_ts_ns={}; remediation=repair the cursor checkpoint from exact primary rows",
                cursor.spawn_id, checkpoint.outbox.operation_id, checkpoint.outbox.event_ts_ns
            ));
        }
        previous_key = Some(key);
    }
    Ok(())
}

fn projection_checkpoint_from_exact_rows(
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
    exact_rows: &[AmbientExactJournalRow],
) -> Result<AmbientProjectionCheckpoint, String> {
    let checkpoint = AmbientProjectionCheckpoint {
        record_version: AMBIENT_PROJECTION_CHECKPOINT_VERSION,
        outbox: outbox.clone(),
        journal_keys: exact_rows.iter().map(|row| row.key.clone()).collect(),
    };
    validate_projection_checkpoint(cursor, &checkpoint)?;
    Ok(checkpoint)
}

fn encoded_record_multiset(
    records: &[AgentEventRecord],
) -> Result<BTreeMap<Vec<u8>, usize>, String> {
    let mut multiset = BTreeMap::new();
    for record in records {
        let encoded = validate_and_encode(record)
            .map_err(|error| format!("AMBIENT_OUTBOX_RECORD_ENCODE_FAILED: {error}"))?;
        *multiset.entry(encoded).or_insert(0) += 1;
    }
    Ok(multiset)
}

fn inspect_outbox_journal(
    db: &Db,
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
) -> Result<AmbientOutboxJournalState, String> {
    validate_event_outbox(cursor, outbox)?;
    let end_ts_ns = outbox.event_ts_ns.checked_add(1).ok_or_else(|| {
        format!(
            "AMBIENT_OUTBOX_TIMESTAMP_EXHAUSTED: spawn_id={} operation_id={} event_ts_ns=u64::MAX; remediation=repair the corrupt cursor outbox timestamp",
            cursor.spawn_id, outbox.operation_id
        )
    })?;
    let start_key = agent_event_key(outbox.event_ts_ns, 0);
    let end_key = agent_event_key(end_ts_ns, 0);
    let (rows, candidate_rows_examined, snapshot_seq, lease_id) =
        scan_outbox_journal_rows(db, cursor, outbox, &start_key, &end_key)?;
    tracing::debug!(
        code = "AMBIENT_OUTBOX_JOURNAL_SNAPSHOT_COMPLETE",
        spawn_id = %cursor.spawn_id,
        operation_id = %outbox.operation_id,
        lease_id,
        snapshot_seq,
        candidate_rows_examined,
        matched_range_rows = rows.len(),
        "completed one-generation ambient journal audit and released its lease"
    );
    let expected = encoded_record_multiset(&outbox.records)?;
    let mut actual = BTreeMap::new();
    let mut exact_rows = Vec::with_capacity(outbox.records.len());
    for (key, encoded) in rows {
        let record: AgentEventRecord = decode_json(&encoded).map_err(|error| {
            format!(
                "AMBIENT_OUTBOX_JOURNAL_ROW_INVALID: spawn_id={} operation_id={} event_ts_ns={} key_hex={}: {error}; remediation=repair the corrupt same-timestamp journal row before retrying relay",
                cursor.spawn_id,
                outbox.operation_id,
                outbox.event_ts_ns,
                synapse_storage::constellations::hex_encode(&key)
            )
        })?;
        if record
            .payload
            .get(AMBIENT_OUTBOX_OPERATION_FIELD)
            .and_then(Value::as_str)
            == Some(outbox.operation_id.as_str())
        {
            *actual.entry(encoded.clone()).or_insert(0) += 1;
            exact_rows.push(AmbientExactJournalRow {
                key,
                value: encoded,
            });
        }
    }
    if actual.is_empty() {
        return Ok(AmbientOutboxJournalState::Absent);
    }
    if actual != expected {
        return Err(format!(
            "AMBIENT_OUTBOX_JOURNAL_DIVERGED: spawn_id={} operation_id={} event_ts_ns={} expected_records={} actual_records={}; remediation=quarantine and reconcile the duplicate/partial primary event rows before acknowledging the cursor",
            cursor.spawn_id,
            outbox.operation_id,
            outbox.event_ts_ns,
            expected.values().sum::<usize>(),
            actual.values().sum::<usize>()
        ));
    }
    Ok(AmbientOutboxJournalState::Exact(exact_rows))
}

fn scan_outbox_journal_rows(
    db: &Db,
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
    start_key: &[u8],
    end_key: &[u8],
) -> Result<AmbientOutboxScanReadback, String> {
    let mut lease = db
        .pin_cf_fixed_width_range_scan(
            cf::CF_AGENT_EVENTS,
            start_key,
            end_key,
            synapse_storage::COHERENT_SCAN_DEFAULT_MAX_AGE_MS,
        )
        .map_err(|error| {
            format!(
                "AMBIENT_OUTBOX_JOURNAL_SNAPSHOT_PIN_FAILED: spawn_id={} operation_id={} \
                 event_ts_ns={}: {error}; remediation=repair the bounded Calyx reader-lease path",
                cursor.spawn_id, outbox.operation_id, outbox.event_ts_ns
            )
        })?;
    let snapshot_seq = lease.snapshot_seq;
    let lease_id = lease.lease_id;
    let mut rows = Vec::new();
    let mut candidate_rows_examined = 0_usize;
    let scan_result = (|| -> Result<(), String> {
        loop {
            let remaining = MAX_AMBIENT_OUTBOX_TIMESTAMP_CANDIDATES
            .checked_sub(candidate_rows_examined)
            .ok_or_else(|| {
                format!(
                    "AMBIENT_OUTBOX_JOURNAL_RANGE_OVERSIZED: spawn_id={} operation_id={} event_ts_ns={} candidates={} cap={MAX_AMBIENT_OUTBOX_TIMESTAMP_CANDIDATES}; remediation=inspect the pathological same-nanosecond journal partition before retrying relay",
                    cursor.spawn_id,
                    outbox.operation_id,
                    outbox.event_ts_ns,
                    candidate_rows_examined
                )
            })?;
            if remaining == 0 {
                return Err(format!(
                    "AMBIENT_OUTBOX_JOURNAL_RANGE_OVERSIZED: spawn_id={} operation_id={} event_ts_ns={} candidates={candidate_rows_examined} cap={MAX_AMBIENT_OUTBOX_TIMESTAMP_CANDIDATES}; remediation=inspect the pathological same-nanosecond journal partition before retrying relay",
                    cursor.spawn_id, outbox.operation_id, outbox.event_ts_ns
                ));
            }
            let page_rows = remaining.min(AMBIENT_OUTBOX_SCAN_PAGE_ROWS);
            let cursor_before = lease.next_after().map(ToOwned::to_owned);
            let page = db
            .scan_cf_fixed_width_range_page_coherent(&mut lease, page_rows)
            .map_err(|error| {
                format!(
                    "AMBIENT_OUTBOX_JOURNAL_SCAN_FAILED: spawn_id={} operation_id={} event_ts_ns={} after_key_hex={}: {error}; remediation=repair the candidate-bounded CF_AGENT_EVENTS timestamp-range read before retrying relay",
                    cursor.spawn_id,
                    outbox.operation_id,
                    outbox.event_ts_ns,
                    cursor_before.as_deref().map_or_else(
                        || "none".to_owned(),
                        synapse_storage::constellations::hex_encode
                    )
                )
            })?;
            candidate_rows_examined = candidate_rows_examined
            .checked_add(page.candidate_rows_examined)
            .ok_or_else(|| {
                format!(
                    "AMBIENT_OUTBOX_JOURNAL_SCAN_COUNTER_OVERFLOW: spawn_id={} operation_id={}; remediation=repair the Calyx page accounting contract",
                    cursor.spawn_id, outbox.operation_id
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
            return Err(format!(
                "{scan_error}; additionally failed to release coherent lease_id={lease_id} \
                 snapshot_seq={snapshot_seq}: {release_error}"
            ));
        }
        (Ok(()), Err(error)) => {
            return Err(format!(
                "AMBIENT_OUTBOX_JOURNAL_SNAPSHOT_RELEASE_FAILED: spawn_id={} operation_id={} \
                 lease_id={lease_id} snapshot_seq={snapshot_seq}: {error}",
                cursor.spawn_id, outbox.operation_id
            ));
        }
        (Ok(()), Ok(false)) => {
            return Err(format!(
                "AMBIENT_OUTBOX_JOURNAL_SNAPSHOT_EXPIRED_AT_RELEASE: spawn_id={} operation_id={} \
                 lease_id={lease_id} snapshot_seq={snapshot_seq}; remediation=repeat the bounded exact audit",
                cursor.spawn_id, outbox.operation_id
            ));
        }
        (Ok(()), Ok(true)) => {}
    }
    Ok((rows, candidate_rows_examined, snapshot_seq, lease_id))
}

fn inspect_projection_checkpoint_journal(
    db: &Db,
    cursor: &AmbientCursor,
    checkpoint: &AmbientProjectionCheckpoint,
) -> Result<AmbientProjectionCheckpointJournalState, String> {
    validate_projection_checkpoint(cursor, checkpoint)?;
    let mut exact_rows = Vec::with_capacity(checkpoint.journal_keys.len());
    let mut missing_rows = 0_usize;
    for (key, record) in checkpoint
        .journal_keys
        .iter()
        .zip(&checkpoint.outbox.records)
    {
        let expected = validate_and_encode(record).map_err(|error| {
            format!(
                "AMBIENT_PROJECTION_CHECKPOINT_RECORD_INVALID: spawn_id={} operation_id={} key_hex={}: {error}; remediation=repair the cursor checkpoint before recovery",
                cursor.spawn_id,
                checkpoint.outbox.operation_id,
                synapse_storage::constellations::hex_encode(key)
            )
        })?;
        let revisioned = db
            .get_cf_revisioned(cf::CF_AGENT_EVENTS, key)
            .map_err(|error| {
                format!(
                    "AMBIENT_PROJECTION_CHECKPOINT_ROW_READ_FAILED: spawn_id={} operation_id={} key_hex={}: {error}; remediation=repair the exact Calyx point-read before recovery",
                    cursor.spawn_id,
                    checkpoint.outbox.operation_id,
                    synapse_storage::constellations::hex_encode(key)
                )
            })?;
        let Some(actual) = revisioned.and_then(|row| row.value) else {
            missing_rows += 1;
            continue;
        };
        if actual != expected {
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_ROW_DIVERGED: spawn_id={} operation_id={} key_hex={} expected_value_sha256={} actual_value_sha256={}; remediation=quarantine and repair the divergent primary journal row before recovery",
                cursor.spawn_id,
                checkpoint.outbox.operation_id,
                synapse_storage::constellations::hex_encode(key),
                sha256_hex(&expected),
                sha256_hex(&actual)
            ));
        }
        exact_rows.push(AmbientExactJournalRow {
            key: key.clone(),
            value: actual,
        });
    }
    if missing_rows == checkpoint.journal_keys.len() {
        return Ok(AmbientProjectionCheckpointJournalState::Absent);
    }
    if missing_rows != 0 {
        return Err(format!(
            "AMBIENT_PROJECTION_CHECKPOINT_JOURNAL_PARTIAL: spawn_id={} operation_id={} expected_rows={} present_rows={} missing_rows={missing_rows}; remediation=leave the checkpoint intact and reconcile the partially retained operation before recovery",
            cursor.spawn_id,
            checkpoint.outbox.operation_id,
            checkpoint.journal_keys.len(),
            exact_rows.len()
        ));
    }
    Ok(AmbientProjectionCheckpointJournalState::Exact(exact_rows))
}

fn inspect_outbox_live_projection(
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
) -> Result<(), String> {
    let event_unix_ms = outbox.event_ts_ns / NS_PER_MS;
    let now_unix_ms = unix_time_ns_now() / NS_PER_MS;
    let read = super::agent_state::unbound_reads(now_unix_ms)
        .into_iter()
        .find(|read| read.spawn_id.as_deref() == Some(cursor.spawn_id.as_str()))
        .ok_or_else(|| {
            format!(
                "AMBIENT_OUTBOX_PROJECTION_MISSING: spawn_id={} operation_id={} journal_state=exact event_unix_ms={event_unix_ms}; remediation=stop the daemon and repair/rebuild the agent-state projection from CF_AGENT_EVENTS before acknowledging the cursor outbox",
                cursor.spawn_id, outbox.operation_id
            )
        })?;
    if read.last_event_unix_ms < event_unix_ms {
        return Err(format!(
            "AMBIENT_OUTBOX_PROJECTION_STALE: spawn_id={} operation_id={} journal_state=exact event_unix_ms={event_unix_ms} projected_last_event_unix_ms={}; remediation=rebuild the agent-state projection from the exact journal before acknowledging the cursor outbox",
            cursor.spawn_id, outbox.operation_id, read.last_event_unix_ms
        ));
    }
    tracing::debug!(
        code = "AMBIENT_OUTBOX_PROJECTION_READBACK",
        spawn_id = %cursor.spawn_id,
        operation_id = %outbox.operation_id,
        event_unix_ms,
        projected_last_event_unix_ms = read.last_event_unix_ms,
        projected_state = read.state.as_str(),
        projected_last_event_kind = ?read.last_event_kind,
        "readback=AgentStateTracker edge=ambient_outbox_projection"
    );
    Ok(())
}

fn agent_event_retention_ns() -> Result<u64, String> {
    let retention = RETENTION_DEFAULTS
        .iter()
        .find(|retention| retention.cf == cf::CF_AGENT_EVENTS)
        .ok_or_else(|| {
            "AMBIENT_OUTBOX_RETENTION_CONTRACT_MISSING: CF_AGENT_EVENTS has no retention default; remediation=restore the journal retention contract before relay"
                .to_owned()
        })?;
    let hours = match retention.ttl {
        RetentionTtl::Hours(hours) => hours,
        RetentionTtl::Days(days) => days.checked_mul(24).ok_or_else(|| {
            "AMBIENT_OUTBOX_RETENTION_CONTRACT_OVERFLOW: CF_AGENT_EVENTS day retention does not fit hours; remediation=repair the retention default"
                .to_owned()
        })?,
        RetentionTtl::None | RetentionTtl::LruOnly => {
            return Err(format!(
                "AMBIENT_OUTBOX_RETENTION_CONTRACT_INVALID: CF_AGENT_EVENTS retention is {:?}; remediation=configure a finite time retention so absence ambiguity has a physical boundary",
                retention.ttl
            ));
        }
    };
    hours
        .checked_mul(60 * 60)
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .ok_or_else(|| {
            "AMBIENT_OUTBOX_RETENTION_CONTRACT_OVERFLOW: CF_AGENT_EVENTS retention does not fit nanoseconds; remediation=repair the retention default"
                .to_owned()
        })
}

fn ensure_absent_outbox_is_within_journal_retention(
    cursor: &AmbientCursor,
    outbox: &AmbientEventOutbox,
) -> Result<(), String> {
    let retention_ns = agent_event_retention_ns()?;
    let now_ns = unix_time_ns_now();
    let outbox_age_ns = now_ns.saturating_sub(outbox.event_ts_ns);
    if outbox_age_ns >= retention_ns {
        return Err(format!(
            "AMBIENT_OUTBOX_JOURNAL_ABSENCE_AMBIGUOUS: spawn_id={} operation_id={} event_ts_ns={} now_ns={now_ns} age_ns={outbox_age_ns} retention_ns={retention_ns}; the operation is absent after its journal-retention horizon, so relay cannot distinguish never-committed from committed-then-expired; remediation=leave the outbox unacknowledged and reconcile from retained external/source evidence instead of appending a replacement event",
            cursor.spawn_id, outbox.operation_id, outbox.event_ts_ns
        ));
    }
    Ok(())
}

fn reconcile_projection_checkpoint(
    db: &Db,
    spawn_id: &str,
) -> Result<AmbientProjectionCheckpointOutcome, String> {
    let _relay_guard = ambient_outbox_relay_lock().lock().map_err(|poisoned| {
        format!(
            "AMBIENT_PROJECTION_CHECKPOINT_LOCK_POISONED: spawn_id={spawn_id}: {poisoned}; remediation=restart the daemon and reconcile the cursor checkpoint against CF_AGENT_EVENTS"
        )
    })?;
    if AMBIENT_OUTBOX_RECONCILIATION_LATCH.load(Ordering::Acquire) {
        return Err(format!(
            "AMBIENT_OUTBOX_COMMIT_RECONCILIATION_REQUIRED: spawn_id={spawn_id} a prior event, projection, or acknowledgement outcome is unresolved in this process; remediation=stop the daemon, reopen Calyx, and reconcile the persisted cursor evidence before recovery"
        ));
    }
    let loaded = load_cursor(db, spawn_id)?;
    let mut cursor = loaded.cursor.ok_or_else(|| {
        format!(
            "AMBIENT_PROJECTION_CHECKPOINT_CURSOR_MISSING: spawn_id={spawn_id}; remediation=restore the authoritative ambient cursor before projection recovery"
        )
    })?;
    let mut revision_sha256 = loaded.revision_sha256;
    if cursor.pending_event_outbox.is_some() {
        return Err(format!(
            "AMBIENT_PROJECTION_CHECKPOINT_PENDING_OUTBOX: spawn_id={spawn_id}; remediation=relay the newer pending operation before reconciling its predecessor checkpoint"
        ));
    }
    let Some(checkpoint) = cursor.projection_checkpoint.clone() else {
        return Ok(AmbientProjectionCheckpointOutcome {
            cursor,
            revision_sha256,
        });
    };
    let live_projection = inspect_outbox_live_projection(&cursor, &checkpoint.outbox);

    let exact_rows = match inspect_projection_checkpoint_journal(db, &cursor, &checkpoint) {
        Ok(AmbientProjectionCheckpointJournalState::Exact(rows)) => rows,
        Ok(AmbientProjectionCheckpointJournalState::Absent) => {
            let retention_ns = match agent_event_retention_ns() {
                Ok(retention_ns) => retention_ns,
                Err(error) => {
                    AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                    return Err(format!(
                        "AMBIENT_PROJECTION_CHECKPOINT_RETENTION_UNRESOLVED: spawn_id={} operation_id={} error={error}; remediation=leave the checkpoint intact and repair the journal retention contract before deciding whether missing rows expired",
                        cursor.spawn_id, checkpoint.outbox.operation_id
                    ));
                }
            };
            let now_ns = unix_time_ns_now();
            let checkpoint_age_ns = now_ns.saturating_sub(checkpoint.outbox.event_ts_ns);
            if checkpoint_age_ns < retention_ns {
                AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(format!(
                    "AMBIENT_PROJECTION_CHECKPOINT_ROWS_MISSING: spawn_id={} operation_id={} age_ns={checkpoint_age_ns} retention_ns={retention_ns}; acknowledged primary rows disappeared before their retention boundary; remediation=leave the checkpoint intact, stop the daemon, and repair/reconcile CF_AGENT_EVENTS",
                    cursor.spawn_id, checkpoint.outbox.operation_id
                ));
            }
            cursor.projection_checkpoint = None;
            cursor.updated_ts_ns = now_ns;
            if let Err(error) = store_cursor(db, &cursor, &mut revision_sha256) {
                AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(format!(
                    "AMBIENT_PROJECTION_CHECKPOINT_RETIRE_RECONCILIATION_REQUIRED: spawn_id={} operation_id={} checkpoint_age_ns={checkpoint_age_ns} retention_ns={retention_ns} cursor_error={error}; remediation=stop the daemon and inspect the cursor WAL outcome before retrying retirement",
                    cursor.spawn_id, checkpoint.outbox.operation_id
                ));
            }
            tracing::info!(
                code = "AMBIENT_PROJECTION_CHECKPOINT_RETIRED",
                spawn_id = %cursor.spawn_id,
                operation_id = %checkpoint.outbox.operation_id,
                checkpoint_age_ns,
                retention_ns,
                "all exact primary rows are outside retention; bounded replay checkpoint cleared without re-journaling"
            );
            return Ok(AmbientProjectionCheckpointOutcome {
                cursor,
                revision_sha256,
            });
        }
        Err(error) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(error);
        }
    };
    let initial_projection_error = match live_projection {
        Ok(()) => {
            // Verification passed, so any earlier recovery for this spawn did
            // converge; the streak that guards against endless retry resets
            // here rather than accumulating across unrelated incidents (#1937).
            clear_projection_recovery_streak(&cursor.spawn_id);
            tracing::debug!(
                code = "AMBIENT_PROJECTION_CHECKPOINT_VERIFIED",
                spawn_id = %cursor.spawn_id,
                operation_id = %checkpoint.outbox.operation_id,
                row_count = exact_rows.len(),
                "readback=CF_AGENT_EVENTS+AgentStateTracker edge=ambient_projection_checkpoint"
            );
            return Ok(AmbientProjectionCheckpointOutcome {
                cursor,
                revision_sha256,
            });
        }
        Err(error) => error,
    };
    let recovery = match super::agent_state::recover_ambient_projection_from_exact_journal_rows(
        db,
        &cursor.spawn_id,
        &exact_rows,
    ) {
        Ok(recovery) => recovery,
        Err(error) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_RECOVERY_FAILED: spawn_id={} operation_id={} initial_projection_error={initial_projection_error} recovery_error={error}; remediation=leave the checkpoint intact and repair the exact journal-to-singleton replay before restart",
                cursor.spawn_id, checkpoint.outbox.operation_id,
            ));
        }
    };
    if let Err(error) = inspect_outbox_live_projection(&cursor, &checkpoint.outbox) {
        AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
        return Err(format!(
            "AMBIENT_PROJECTION_CHECKPOINT_READBACK_FAILED: spawn_id={} operation_id={} recovered_rows={} already_current={} recovery_error={error}; remediation=leave the checkpoint intact and repair the singleton readback before restart",
            cursor.spawn_id,
            checkpoint.outbox.operation_id,
            recovery.rows_applied,
            recovery.already_current
        ));
    }
    match inspect_projection_checkpoint_journal(db, &cursor, &checkpoint) {
        Ok(AmbientProjectionCheckpointJournalState::Exact(final_rows))
            if final_rows == exact_rows => {}
        Ok(AmbientProjectionCheckpointJournalState::Exact(final_rows)) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_FINAL_ROWS_CHANGED: spawn_id={} operation_id={} expected_rows={} actual_rows={}; remediation=leave the checkpoint intact and reconcile the changed physical journal evidence",
                cursor.spawn_id,
                checkpoint.outbox.operation_id,
                exact_rows.len(),
                final_rows.len()
            ));
        }
        Ok(AmbientProjectionCheckpointJournalState::Absent) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_FINAL_ROWS_MISSING: spawn_id={} operation_id={}; remediation=leave the checkpoint intact and reconcile the disappeared physical rows",
                cursor.spawn_id, checkpoint.outbox.operation_id
            ));
        }
        Err(error) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(error);
        }
    }
    if recovery.rows_applied > 0 {
        OUTBOX_PROJECTION_RECOVERED_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
    tracing::info!(
        code = "AMBIENT_PROJECTION_CHECKPOINT_RECOVERED",
        spawn_id = %cursor.spawn_id,
        operation_id = %checkpoint.outbox.operation_id,
        recovered_rows = recovery.rows_applied,
        already_current = recovery.already_current,
        projected_state = recovery.readback.state.as_str(),
        projected_last_event_unix_ms = recovery.readback.last_event_unix_ms,
        // The error that TRIGGERED this recovery (#1937 ask 1). It was already
        // in scope and was surfaced only when the recovery itself failed, so on
        // the common path — recovery "succeeds" and the loop repeats — the one
        // field that says which condition fired was the one field not recorded.
        // 4,947 recoveries across one log file produced zero occurrences of
        // either AMBIENT_OUTBOX_PROJECTION_MISSING or _STALE, which is why the
        // issue could not discriminate its own candidate causes.
        initial_projection_error = %initial_projection_error,
        consecutive_recoveries = consecutive_projection_recoveries(&cursor.spawn_id),
        "readback=CF_AGENT_EVENTS+AgentStateTracker edge=ambient_projection_checkpoint"
    );

    // The projection legitimately does not retain a long-dead agent
    // (`agent_state::prune_dead`), so re-creating this entry is work the very
    // next sweep is guaranteed to undo — which is exactly the loop #1937
    // recorded: 22 spawns, 238 recoveries each, `already_current=false` every
    // time. Recovery and pruning were enforcing contradictory retention
    // policies, so the checkpoint could never converge.
    //
    // The durable journal rows remain the record; only the in-memory cache
    // declines to hold them. That satisfies the checkpoint, so it retires.
    let now_unix_ms = unix_time_ns_now() / NS_PER_MS;
    if super::agent_state::beyond_dead_retention(
        recovery.readback.state,
        recovery.readback.since_unix_ms,
        now_unix_ms,
    ) {
        cursor.projection_checkpoint = None;
        cursor.updated_ts_ns = unix_time_ns_now();
        if let Err(error) = store_cursor(db, &cursor, &mut revision_sha256) {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_PROJECTION_CHECKPOINT_RETIRE_RECONCILIATION_REQUIRED: spawn_id={} operation_id={} reason=beyond_projection_dead_retention cursor_error={error}; remediation=stop the daemon and inspect the cursor WAL outcome before retrying retirement",
                cursor.spawn_id, checkpoint.outbox.operation_id
            ));
        }
        clear_projection_recovery_streak(&cursor.spawn_id);
        tracing::info!(
            code = "AMBIENT_PROJECTION_CHECKPOINT_RETIRED_BEYOND_PROJECTION_RETENTION",
            spawn_id = %cursor.spawn_id,
            operation_id = %checkpoint.outbox.operation_id,
            projected_state = recovery.readback.state.as_str(),
            since_unix_ms = recovery.readback.since_unix_ms,
            dead_retention_ms = super::agent_state::DEAD_RETENTION_MS,
            initial_projection_error = %initial_projection_error,
            "the in-memory projection is not required to retain this agent, so its absence is the projection working; checkpoint retired against the durable journal"
        );
        return Ok(AmbientProjectionCheckpointOutcome {
            cursor,
            revision_sha256,
        });
    }

    // A recovery that has to run again, and again, for the same spawn is a
    // distinct condition from "needed recovery once", and it must not be
    // retried forever (#1937 ask 3). Convergence means the next cycle verifies;
    // if it does not, something the recovery cannot fix is undoing it.
    let streak = record_projection_recovery(&cursor.spawn_id);
    if streak > MAX_CONSECUTIVE_PROJECTION_RECOVERIES {
        AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
        return Err(format!(
            "AMBIENT_PROJECTION_CHECKPOINT_NOT_CONVERGING: spawn_id={} operation_id={} consecutive_recoveries={streak} limit={MAX_CONSECUTIVE_PROJECTION_RECOVERIES} projected_state={} already_current={} initial_projection_error={initial_projection_error}; the same checkpoint has been recovered repeatedly and the next verification still failed, so recovery is not the repair; remediation=stop the daemon and reconcile the agent-state projection against CF_AGENT_EVENTS before ingestion resumes",
            cursor.spawn_id,
            checkpoint.outbox.operation_id,
            recovery.readback.state.as_str(),
            recovery.already_current
        ));
    }
    Ok(AmbientProjectionCheckpointOutcome {
        cursor,
        revision_sha256,
    })
}

/// Consecutive recoveries the same spawn's checkpoint may need before the
/// condition is escalated instead of retried (#1937 ask 3).
///
/// A recovery that worked is followed by a verification that passes, which
/// clears the streak. More than a couple in a row means the recovery is not the
/// repair — on the live daemon this ran 238 times for each of 22 spawns.
const MAX_CONSECUTIVE_PROJECTION_RECOVERIES: u32 = 3;

static PROJECTION_RECOVERY_STREAKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, u32>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

fn record_projection_recovery(spawn_id: &str) -> u32 {
    let mut guard = match PROJECTION_RECOVERY_STREAKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let streak = guard.entry(spawn_id.to_owned()).or_insert(0);
    *streak = streak.saturating_add(1);
    *streak
}

fn consecutive_projection_recoveries(spawn_id: &str) -> u32 {
    let guard = match PROJECTION_RECOVERY_STREAKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.get(spawn_id).copied().unwrap_or(0)
}

fn clear_projection_recovery_streak(spawn_id: &str) {
    let mut guard = match PROJECTION_RECOVERY_STREAKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.remove(spawn_id);
}

fn relay_pending_event_outbox(
    db: &Db,
    spawn_id: &str,
) -> Result<AmbientOutboxRelayOutcome, String> {
    let _relay_guard = ambient_outbox_relay_lock().lock().map_err(|poisoned| {
        format!(
            "AMBIENT_OUTBOX_RELAY_LOCK_POISONED: spawn_id={spawn_id}: {poisoned}; remediation=restart the daemon and reconcile the cursor outbox against CF_AGENT_EVENTS before ingestion resumes"
        )
    })?;
    if AMBIENT_OUTBOX_RECONCILIATION_LATCH.load(Ordering::Acquire) {
        return Err(format!(
            "AMBIENT_OUTBOX_COMMIT_RECONCILIATION_REQUIRED: spawn_id={spawn_id} a prior event or acknowledgement commit returned an unresolved error in this process; remediation=stop the daemon, reopen Calyx so durable WAL truth and the agent-state projection rebuild together, then reconcile the persisted outbox operation against CF_AGENT_EVENTS and its cursor before relay"
        ));
    }
    let loaded = load_cursor(db, spawn_id)?;
    let mut cursor = loaded.cursor.ok_or_else(|| {
        format!(
            "AMBIENT_OUTBOX_CURSOR_MISSING: spawn_id={spawn_id}; remediation=restore the authoritative cursor/outbox before relay"
        )
    })?;
    let mut revision_sha256 = loaded.revision_sha256;
    let Some(outbox) = cursor.pending_event_outbox.clone() else {
        return Ok(AmbientOutboxRelayOutcome {
            cursor,
            revision_sha256,
            newly_registered: false,
        });
    };
    validate_event_outbox(&cursor, &outbox)?;
    let prior_state = inspect_outbox_journal(db, &cursor, &outbox)?;
    let recovered_existing_journal = matches!(&prior_state, AmbientOutboxJournalState::Exact(_));
    let exact_rows = match prior_state {
        AmbientOutboxJournalState::Absent => {
            if let Err(error) = ensure_absent_outbox_is_within_journal_retention(&cursor, &outbox) {
                AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(error);
            }
            if let Err(error) = record_agent_events(db, &outbox.records) {
                AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(format!(
                    "AMBIENT_OUTBOX_EVENT_COMMIT_RECONCILIATION_REQUIRED: spawn_id={} operation_id={} checkpoint_offset_bytes={} records={} event_error={error}; remediation=leave the durable cursor outbox intact, stop the daemon, reopen Calyx so WAL truth and the agent-state projection rebuild together, then inspect CF_AGENT_EVENTS before retrying this logical operation",
                    cursor.spawn_id,
                    outbox.operation_id,
                    outbox.checkpoint_offset_bytes,
                    outbox.records.len()
                ));
            }
            match inspect_outbox_journal(db, &cursor, &outbox) {
                Ok(AmbientOutboxJournalState::Exact(rows)) => rows,
                Ok(AmbientOutboxJournalState::Absent) => {
                    AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                    return Err(format!(
                        "AMBIENT_OUTBOX_RELAY_READBACK_MISSING: spawn_id={} operation_id={} event writer returned success but exact primary rows are absent; remediation=stop the daemon and reconcile CF_AGENT_EVENTS before retry",
                        cursor.spawn_id, outbox.operation_id
                    ));
                }
                Err(readback_error) => {
                    AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                    return Err(format!(
                        "AMBIENT_OUTBOX_RELAY_READBACK_RECONCILIATION_REQUIRED: spawn_id={} operation_id={} event writer returned success but the independent primary-row readback failed: {readback_error}; remediation=leave the durable cursor outbox intact, stop the daemon, reopen Calyx, and reconcile CF_AGENT_EVENTS before any logical retry",
                        cursor.spawn_id, outbox.operation_id
                    ));
                }
            }
        }
        AmbientOutboxJournalState::Exact(rows) => rows,
    };
    let mut projection_recovered = false;
    if let Err(initial_projection_error) = inspect_outbox_live_projection(&cursor, &outbox) {
        if !recovered_existing_journal {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(initial_projection_error);
        }
        let recovery = match super::agent_state::recover_ambient_projection_from_exact_journal_rows(
            db,
            &cursor.spawn_id,
            &exact_rows,
        ) {
            Ok(recovery) => recovery,
            Err(recovery_error) => {
                AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
                return Err(format!(
                    "AMBIENT_OUTBOX_PROJECTION_RECOVERY_FAILED: spawn_id={} operation_id={} initial_projection_error={initial_projection_error} recovery_error={recovery_error}; remediation=leave the durable outbox intact and repair the exact journal-to-singleton replay path before daemon restart",
                    cursor.spawn_id, outbox.operation_id
                ));
            }
        };
        if let Err(readback_error) = inspect_outbox_live_projection(&cursor, &outbox) {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_OUTBOX_PROJECTION_RECOVERY_READBACK_FAILED: spawn_id={} operation_id={} recovered_rows={} already_current={} recovered_state={} recovery_last_event_unix_ms={} initial_projection_error={initial_projection_error} readback_error={readback_error}; remediation=leave the durable outbox intact and repair the singleton readback before daemon restart",
                cursor.spawn_id,
                outbox.operation_id,
                recovery.rows_applied,
                recovery.already_current,
                recovery.readback.state.as_str(),
                recovery.readback.last_event_unix_ms
            ));
        }
        projection_recovered = recovery.rows_applied > 0;
        if projection_recovered {
            OUTBOX_PROJECTION_RECOVERED_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
    }
    match inspect_outbox_journal(db, &cursor, &outbox) {
        Ok(AmbientOutboxJournalState::Exact(final_rows)) if final_rows == exact_rows => {}
        Ok(AmbientOutboxJournalState::Exact(final_rows)) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_OUTBOX_FINAL_JOURNAL_READBACK_CHANGED: spawn_id={} operation_id={} expected_rows={} actual_rows={}; remediation=leave the durable outbox intact and reconcile the changed exact journal keys/bytes before restart",
                cursor.spawn_id,
                outbox.operation_id,
                exact_rows.len(),
                final_rows.len()
            ));
        }
        Ok(AmbientOutboxJournalState::Absent) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_OUTBOX_FINAL_JOURNAL_READBACK_MISSING: spawn_id={} operation_id={}; remediation=leave the durable outbox intact and reconcile the disappeared physical rows before restart",
                cursor.spawn_id, outbox.operation_id
            ));
        }
        Err(readback_error) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_OUTBOX_FINAL_JOURNAL_READBACK_FAILED: spawn_id={} operation_id={} readback_error={readback_error}; remediation=leave the durable outbox intact and repair the exact journal read before restart",
                cursor.spawn_id, outbox.operation_id
            ));
        }
    }

    let projection_checkpoint = match projection_checkpoint_from_exact_rows(
        &cursor,
        &outbox,
        &exact_rows,
    ) {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
            return Err(format!(
                "AMBIENT_OUTBOX_PROJECTION_CHECKPOINT_BUILD_FAILED: spawn_id={} operation_id={} error={error}; remediation=leave the durable outbox intact and repair the exact journal-key checkpoint before acknowledgement",
                cursor.spawn_id, outbox.operation_id
            ));
        }
    };

    let newly_registered = outbox.acknowledge_registration && !cursor.registered;
    if outbox.acknowledge_registration {
        cursor.registered = true;
    }
    if let Some(state) = &outbox.acknowledge_state {
        cursor.last_emitted_state = Some(state.clone());
    }
    cursor.projection_checkpoint = Some(projection_checkpoint);
    cursor.pending_event_outbox = None;
    cursor.updated_ts_ns = unix_time_ns_now();
    if let Err(error) = store_cursor(db, &cursor, &mut revision_sha256) {
        AMBIENT_OUTBOX_RECONCILIATION_LATCH.store(true, Ordering::Release);
        return Err(format!(
            "AMBIENT_OUTBOX_ACK_RECONCILIATION_REQUIRED: spawn_id={} operation_id={} journal_state=exact cursor_error={error}; remediation=stop the daemon, reopen Calyx so the cursor WAL outcome becomes authoritative, then inspect this same operation id in CF_AGENT_EVENTS before any relay retry",
            cursor.spawn_id, outbox.operation_id
        ));
    }
    if newly_registered {
        SESSIONS_REGISTERED_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
    OUTBOX_ACKNOWLEDGED_TOTAL.fetch_add(1, Ordering::Relaxed);
    if recovered_existing_journal {
        OUTBOX_RECOVERED_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
    tracing::info!(
        code = "AMBIENT_OUTBOX_ACKNOWLEDGED",
        spawn_id = %cursor.spawn_id,
        session_id = %cursor.session_id,
        operation_id = %outbox.operation_id,
        record_count = outbox.records.len(),
        recovered_existing_journal,
        projection_recovered,
        acknowledge_registration = outbox.acknowledge_registration,
        acknowledge_state = ?outbox.acknowledge_state,
        checkpoint_offset_bytes = outbox.checkpoint_offset_bytes,
        checkpoint_lines_ingested = outbox.checkpoint_lines_ingested,
        "readback=CF_AGENT_EVENTS+CF_KV edge=ambient_outbox_acknowledged"
    );
    Ok(AmbientOutboxRelayOutcome {
        cursor,
        revision_sha256,
        newly_registered,
    })
}

fn relay_pending_event_outbox_observed(
    db: &Db,
    spawn_id: &str,
) -> Result<AmbientOutboxRelayOutcome, String> {
    relay_pending_event_outbox(db, spawn_id).inspect_err(|detail| {
        INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            code = "AMBIENT_OUTBOX_RELAY_FAILED",
            spawn_id,
            detail,
            remediation = "leave the durable outbox intact and follow the nested physical journal/cursor remediation before retrying",
            "ambient lifecycle outbox could not reach an exact journal-plus-cursor acknowledgement"
        );
    })
}

/// Ingests new bytes for one session file. Returns the structured sticky-error
/// detail (also persisted on the cursor) when the source is missing, truncated,
/// or a row exceeds the encoded-size cap.
fn ingest_session_file_with_cancel(
    db: &Db,
    session_id: &str,
    source_path: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<SessionIngestOutcome, String> {
    let spawn_id = spawn_id_for_session(session_id);

    let loaded = load_cursor(db, &spawn_id)?;
    let mut cursor_revision_sha256 = loaded.revision_sha256;
    let mut cursor = match loaded.cursor {
        Some(cursor) => cursor,
        None => seed_cursor(&spawn_id, session_id, source_path),
    };
    let mut newly_registered = false;

    if Path::new(&cursor.source_path) != source_path || cursor.session_id != session_id {
        let detail = format!(
            "AMBIENT_CURSOR_SOURCE_IDENTITY_MISMATCH: spawn_id={spawn_id} cursor_session_id={} discovered_session_id={session_id} cursor_path={} discovered_path={}; remediation=restore the original session source or reconcile and rebuild the cursor without reusing its identity",
            cursor.session_id,
            cursor.source_path,
            source_path.display()
        );
        return Err(stick_cursor_error(
            db,
            &mut cursor,
            &mut cursor_revision_sha256,
            detail,
        ));
    }

    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Ok(SessionIngestOutcome {
            cancelled: true,
            ..SessionIngestOutcome::default()
        });
    }

    if cursor.pending_event_outbox.is_some() {
        let relayed = relay_pending_event_outbox_observed(db, &spawn_id)?;
        cursor = relayed.cursor;
        cursor_revision_sha256 = relayed.revision_sha256;
        newly_registered |= relayed.newly_registered;
    }

    if cursor.projection_checkpoint.is_some() {
        let reconciled = reconcile_projection_checkpoint(db, &spawn_id)?;
        cursor = reconciled.cursor;
        cursor_revision_sha256 = reconciled.revision_sha256;
    }

    if let Some(error) = &cursor.error {
        tracing::debug!(
            code = "AMBIENT_INGEST_PARKED",
            spawn_id = %spawn_id,
            detail = %error,
            "skipping ambient session with sticky ingest error"
        );
        return Ok(SessionIngestOutcome {
            newly_registered,
            skipped: true,
            ..SessionIngestOutcome::default()
        });
    }

    let metadata = match std::fs::metadata(source_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            let detail = format!(
                "AMBIENT_SOURCE_STAT_FAILED: path={} offset_bytes={}: {error}; remediation=restore the exact append-only session source and metadata access before clearing the sticky cursor",
                source_path.display(),
                cursor.offset_bytes
            );
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
    };
    if !metadata.is_file() {
        let detail = format!(
            "AMBIENT_SOURCE_NOT_FILE: path={} offset_bytes={}; remediation=restore the Claude session JSONL source as a regular append-only file",
            source_path.display(),
            cursor.offset_bytes
        );
        return Err(stick_cursor_error(
            db,
            &mut cursor,
            &mut cursor_revision_sha256,
            detail,
        ));
    }
    let file_size = metadata.len();
    if file_size < cursor.offset_bytes {
        let detail = format!(
            "AMBIENT_SOURCE_TRUNCATED: path={} file_size_bytes={file_size} cursor_offset_bytes={}; remediation=restore the original append-only source bytes or reconcile all durable transcript rows before clearing the cursor",
            source_path.display(),
            cursor.offset_bytes
        );
        return Err(stick_cursor_error(
            db,
            &mut cursor,
            &mut cursor_revision_sha256,
            detail,
        ));
    }

    let fingerprint_migrated = match ensure_transcript_source_fingerprint(
        db,
        &spawn_id,
        source_path,
        file_size,
        cursor.offset_bytes,
        &mut cursor.source_fingerprint_bytes,
        &mut cursor.source_fingerprint_sha256,
        "AMBIENT",
    ) {
        Ok(migrated) => migrated,
        Err(detail) => {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
    };
    let boundary_migrated = match ensure_transcript_source_boundary(
        db,
        &spawn_id,
        source_path,
        file_size,
        TranscriptSourceBoundaryState {
            cursor_offset_bytes: cursor.offset_bytes,
            lines_ingested: cursor.lines_ingested,
            start_offset_bytes: &mut cursor.source_boundary_start_offset_bytes,
            bytes: &mut cursor.source_boundary_bytes,
            sha256: &mut cursor.source_boundary_sha256,
        },
        "AMBIENT",
    ) {
        Ok(migrated) => migrated,
        Err(detail) => {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
    };
    if fingerprint_migrated || boundary_migrated {
        cursor.updated_ts_ns = unix_time_ns_now();
        store_cursor(db, &cursor, &mut cursor_revision_sha256)?;
    }

    if file_size == cursor.offset_bytes && cursor.registered {
        return Ok(SessionIngestOutcome {
            newly_registered,
            ..SessionIngestOutcome::default()
        });
    }

    // Single pressure authority for the pass: rows below ride bypass writes.
    if file_size > cursor.offset_bytes && !db.pressure_permits_write(cf::CF_AGENT_TRANSCRIPTS) {
        PRESSURE_DEFERRALS_TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            code = "AMBIENT_INGEST_PRESSURE_DEFERRED",
            spawn_id = %spawn_id,
            source_path = %source_path.display(),
            cursor_offset_bytes = cursor.offset_bytes,
            snapshot_size_bytes = file_size,
            "disk pressure defers ambient ingestion; cursor not advanced"
        );
        return Ok(SessionIngestOutcome {
            newly_registered,
            deferred_for_pressure: true,
            ..SessionIngestOutcome::default()
        });
    }

    let mut reader = match BoundedTranscriptTailReader::open(
        source_path,
        cursor.offset_bytes,
        file_size,
        "AMBIENT",
    ) {
        Ok(reader) => reader,
        Err(detail) => {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
    };
    let mut new_parsed = 0_u64;
    let mut new_invalid = 0_u64;
    let mut pending_lifecycle: Option<Lifecycle> = None;
    let mut pass_rows = 0_usize;
    let mut pass_source_bytes = 0_u64;
    let mut cancelled = false;
    let mut terminal_error: Option<String> = None;
    let mut reached_snapshot_boundary = false;

    'pass: while pass_rows < MAX_ROWS_PER_PASS && pass_source_bytes < MAX_SOURCE_BYTES_PER_PASS {
        let mut working_cursor = cursor.clone();
        let mut chunk = Vec::with_capacity(MAX_AGENT_TRANSCRIPT_COMMIT_ROWS);
        let mut chunk_parsed = 0_u64;
        let mut chunk_invalid = 0_u64;
        let mut chunk_last_lifecycle: Option<Lifecycle> = None;
        while chunk.len() < MAX_AGENT_TRANSCRIPT_COMMIT_ROWS
            && pass_rows + chunk.len() < MAX_ROWS_PER_PASS
        {
            match reader.next_line(false, cancel) {
                Ok(BoundedTailRead::Line {
                    bytes,
                    consumed_bytes,
                    source_offset_bytes,
                }) => {
                    let Some(line_no) = working_cursor.lines_ingested.checked_add(1) else {
                        terminal_error = Some(format!(
                            "AMBIENT_LINE_NUMBER_OVERFLOW: path={} source_offset_bytes={source_offset_bytes}; remediation=quarantine the impossible-size source and reconcile its cursor",
                            source_path.display()
                        ));
                        break;
                    };
                    let cursor_before_line = working_cursor.clone();
                    let (record, lifecycle) =
                        parse_session_line(&bytes, line_no, &mut working_cursor);
                    if let Err(detail) = record.validate() {
                        working_cursor = cursor_before_line;
                        terminal_error = Some(format!(
                            "AMBIENT_ROW_VALIDATION_FAILED: path={} line_no={line_no} source_offset_bytes={source_offset_bytes}: {detail}; remediation=repair the parser/record invariant before clearing the cursor",
                            source_path.display()
                        ));
                        break;
                    }
                    let encoded = match encode_json(&record) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            working_cursor = cursor_before_line;
                            terminal_error = Some(format!(
                                "AMBIENT_ROW_ENCODE_FAILED: path={} line_no={line_no} source_offset_bytes={source_offset_bytes}: {error}; remediation=repair transcript row serialization before clearing the cursor",
                                source_path.display()
                            ));
                            break;
                        }
                    };
                    if encoded.len() > MAX_VALUE_BYTES {
                        working_cursor = cursor_before_line;
                        terminal_error = Some(format!(
                            "AMBIENT_ROW_OVERSIZED: path={} line_no={line_no} source_offset_bytes={source_offset_bytes} encoded_bytes={} max_encoded_bytes={MAX_VALUE_BYTES}; remediation=repair the per-field normalization bounds before clearing the cursor",
                            source_path.display(),
                            encoded.len()
                        ));
                        break;
                    }
                    match record.status {
                        TranscriptParseStatus::Parsed => chunk_parsed += 1,
                        TranscriptParseStatus::Invalid => {
                            chunk_invalid += 1;
                            tracing::error!(
                                code = "AMBIENT_LINE_INVALID",
                                spawn_id = %spawn_id,
                                source_path = %source_path.display(),
                                line_no,
                                source_offset_bytes,
                                raw_line_bytes = record.raw_line_bytes,
                                raw_line_sha256 = %record.raw_line_sha256,
                                detail = record.parse_error.as_deref().unwrap_or("unknown"),
                                remediation = "repair the producer's UTF-8 JSONL record; the invalid evidence row remains durable and is never silently skipped",
                                "ambient source line refused by the session-file parser; invalid row will be written"
                            );
                        }
                    }
                    let source_key = agent_transcript_key(&spawn_id, line_no);
                    chunk.push(PreparedTranscriptRow {
                        source_key,
                        encoded,
                        record,
                        source_offset_bytes,
                        consumed_bytes,
                    });
                    working_cursor.lines_ingested = line_no;
                    working_cursor.offset_bytes = working_cursor
                        .offset_bytes
                        .checked_add(consumed_bytes)
                        .ok_or_else(|| {
                            format!(
                                "AMBIENT_SOURCE_OFFSET_OVERFLOW: path={} cursor_offset_bytes={} consumed_bytes={consumed_bytes}; remediation=quarantine the impossible-size source and reconcile its cursor",
                                source_path.display(), working_cursor.offset_bytes
                            )
                        })?;
                    pass_source_bytes = pass_source_bytes.saturating_add(consumed_bytes);
                    if let Some(signal) = lifecycle {
                        chunk_last_lifecycle = Some(signal);
                    }
                    if pass_source_bytes >= MAX_SOURCE_BYTES_PER_PASS {
                        break;
                    }
                }
                Ok(BoundedTailRead::SnapshotEof) => {
                    reached_snapshot_boundary = true;
                    break;
                }
                Ok(BoundedTailRead::IncompleteTail {
                    source_offset_bytes,
                    buffered_bytes,
                }) => {
                    reached_snapshot_boundary = true;
                    tracing::debug!(
                        code = "AMBIENT_INCOMPLETE_TAIL_DEFERRED",
                        spawn_id = %spawn_id,
                        source_path = %source_path.display(),
                        source_offset_bytes,
                        buffered_bytes,
                        max_line_bytes = MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES,
                        "unterminated live JSONL record remains behind the durable cursor"
                    );
                    break;
                }
                Ok(BoundedTailRead::Cancelled) => {
                    cancelled = true;
                    break;
                }
                Err(detail) => {
                    terminal_error = Some(detail);
                    break;
                }
            }
        }

        if cancelled {
            tracing::info!(
                code = "AMBIENT_INGEST_CYCLE_CANCELLED",
                spawn_id = %spawn_id,
                source_path = %source_path.display(),
                committed_rows = pass_rows,
                discarded_prepared_rows = chunk.len(),
                cursor_offset_bytes = cursor.offset_bytes,
                "daemon shutdown cancelled ambient ingestion before the durable cursor commit"
            );
            break 'pass;
        }
        if chunk.is_empty() {
            break 'pass;
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            break 'pass;
        }
        if let Err(detail) = commit_transcript_chunk(db, "AMBIENT", &spawn_id, source_path, &chunk)
        {
            tracing::error!(
                code = "AMBIENT_CHUNK_COMMIT_FAILED",
                spawn_id = %spawn_id,
                source_path = %source_path.display(),
                cursor_offset_bytes = cursor.offset_bytes,
                rows = chunk.len(),
                detail = %detail,
                "bounded ambient transcript chunk failed; durable cursor remains unchanged"
            );
            return Err(detail);
        }
        working_cursor.parsed_rows = cursor.parsed_rows.checked_add(chunk_parsed).ok_or_else(
            || {
                "AMBIENT_PARSED_COUNTER_OVERFLOW: remediation=reconcile the impossible-size cursor"
                    .to_owned()
            },
        )?;
        working_cursor.invalid_rows = cursor.invalid_rows.checked_add(chunk_invalid).ok_or_else(
            || {
                "AMBIENT_INVALID_COUNTER_OVERFLOW: remediation=reconcile the impossible-size cursor"
                    .to_owned()
            },
        )?;
        if let Err(detail) = ensure_transcript_source_fingerprint(
            db,
            &spawn_id,
            source_path,
            file_size,
            working_cursor.offset_bytes,
            &mut working_cursor.source_fingerprint_bytes,
            &mut working_cursor.source_fingerprint_sha256,
            "AMBIENT",
        ) {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
        if let Err(detail) = refresh_transcript_source_boundary(
            source_path,
            file_size,
            TranscriptSourceBoundaryState {
                cursor_offset_bytes: working_cursor.offset_bytes,
                lines_ingested: working_cursor.lines_ingested,
                start_offset_bytes: &mut working_cursor.source_boundary_start_offset_bytes,
                bytes: &mut working_cursor.source_boundary_bytes,
                sha256: &mut working_cursor.source_boundary_sha256,
            },
            "AMBIENT",
        ) {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
        if let Some(signal) = chunk_last_lifecycle {
            pending_lifecycle = Some(signal);
        }
        working_cursor.pending_event_outbox =
            match build_event_outbox(&working_cursor, pending_lifecycle.as_ref()) {
                Ok(outbox) => outbox,
                Err(detail) => {
                    return Err(stick_cursor_error(
                        db,
                        &mut cursor,
                        &mut cursor_revision_sha256,
                        detail,
                    ));
                }
            };
        if working_cursor.pending_event_outbox.is_some() {
            tracing::debug!(
                code = "AMBIENT_OUTBOX_CHECKPOINT_STAGED",
                spawn_id = %working_cursor.spawn_id,
                checkpoint_offset_bytes = working_cursor.offset_bytes,
                checkpoint_lines_ingested = working_cursor.lines_ingested,
                operation_id = working_cursor
                    .pending_event_outbox
                    .as_ref()
                    .map(|outbox| outbox.operation_id.as_str()),
                "ambient cursor advancement carries its bounded event intent"
            );
        }
        working_cursor.updated_ts_ns = unix_time_ns_now();
        store_cursor(db, &working_cursor, &mut cursor_revision_sha256)?;
        cursor = working_cursor;
        pass_rows += chunk.len();
        new_parsed = new_parsed.checked_add(chunk_parsed).ok_or_else(|| {
            "AMBIENT_PARSED_COUNTER_OVERFLOW: remediation=reconcile the impossible-size pass"
                .to_owned()
        })?;
        new_invalid = new_invalid.checked_add(chunk_invalid).ok_or_else(|| {
            "AMBIENT_INVALID_COUNTER_OVERFLOW: remediation=reconcile the impossible-size pass"
                .to_owned()
        })?;
        LINES_PARSED_TOTAL.fetch_add(chunk_parsed, Ordering::Relaxed);
        LINES_INVALID_TOTAL.fetch_add(chunk_invalid, Ordering::Relaxed);
        if terminal_error.is_some() {
            break 'pass;
        }
        if reached_snapshot_boundary {
            break 'pass;
        }
    }

    // An empty source still represents a real observed agent. Its exact
    // registration intent is checkpointed first; a later append starts at 0.
    if !cancelled && !cursor.registered && cursor.pending_event_outbox.is_none() {
        cursor.pending_event_outbox = match build_event_outbox(&cursor, None) {
            Ok(Some(outbox)) => Some(outbox),
            Ok(None) => {
                let detail = format!(
                    "AMBIENT_OUTBOX_REGISTRATION_MISSING: spawn_id={} unregistered cursor produced no registration intent; remediation=repair the outbox builder before cursor advancement",
                    cursor.spawn_id
                );
                return Err(stick_cursor_error(
                    db,
                    &mut cursor,
                    &mut cursor_revision_sha256,
                    detail,
                ));
            }
            Err(detail) => {
                return Err(stick_cursor_error(
                    db,
                    &mut cursor,
                    &mut cursor_revision_sha256,
                    detail,
                ));
            }
        };
        cursor.updated_ts_ns = unix_time_ns_now();
        store_cursor(db, &cursor, &mut cursor_revision_sha256)?;
    }

    // Relay only after the checkpoint carrying the exact intent is durable.
    // This also drains intent from committed chunks after cancellation, matching
    // the pre-existing promise not to lose already-checkpointed lifecycle state.
    if cursor.pending_event_outbox.is_some() {
        let relayed = relay_pending_event_outbox_observed(db, &spawn_id)?;
        cursor = relayed.cursor;
        cursor_revision_sha256 = relayed.revision_sha256;
        newly_registered |= relayed.newly_registered;
    }

    if let Some(detail) = terminal_error {
        return Err(stick_cursor_error(
            db,
            &mut cursor,
            &mut cursor_revision_sha256,
            detail,
        ));
    }

    Ok(SessionIngestOutcome {
        new_parsed_rows: new_parsed,
        new_invalid_rows: new_invalid,
        newly_registered,
        cancelled,
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
        source_epoch_unix_ms: None,
        source_fingerprint_bytes: None,
        source_fingerprint_sha256: None,
        source_boundary_start_offset_bytes: None,
        source_boundary_bytes: None,
        source_boundary_sha256: None,
        registered: false,
        last_emitted_state: None,
        pending_event_outbox: None,
        projection_checkpoint: None,
        error: None,
        updated_ts_ns: unix_time_ns_now(),
    }
}

/// One discovery + ingest pass over every session file under `root`. Per-session
/// errors are sticky and already logged; the cycle continues so one corrupt
/// session can never stall the rest of the fleet.
fn ingest_all_once_with_cancel(
    db: &Db,
    root: &Path,
    max_idle_secs: u64,
    cancel: Option<&CancellationToken>,
) -> Value {
    CYCLES_TOTAL.fetch_add(1, Ordering::Relaxed);
    let now_secs = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(error) => {
            INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "AMBIENT_SYSTEM_TIME_INVALID",
                root = %root.display(),
                detail = %error,
                remediation = "repair the host clock before ambient transcript discovery",
                "ambient ingest cannot compute a trustworthy staleness boundary"
            );
            return json!({
                "sessions_seen": 0,
                "errors": 1,
                "error": "AMBIENT_SYSTEM_TIME_INVALID",
            });
        }
    };

    let project_dirs = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                code = "AMBIENT_PROJECTS_ROOT_ABSENT",
                root = %root.display(),
                "Claude projects root does not exist yet; physical ambient inventory is empty"
            );
            return json!({"sessions_seen": 0, "new_rows": 0, "sessions_registered": 0, "errors": 0});
        }
        Err(error) => {
            INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "AMBIENT_INGEST_CYCLE_FAILED",
                root = %root.display(),
                detail = %error,
                remediation = "restore directory enumeration access; this cycle is explicitly incomplete",
                "ambient ingest cycle could not list the projects root"
            );
            return json!({"sessions_seen": 0, "errors": 1, "error": error.to_string()});
        }
    };

    let mut sessions_seen = 0_u64;
    let mut new_rows = 0_u64;
    let mut registered = 0_u64;
    let mut errors = 0_u64;
    let mut deferred = 0_u64;
    let mut skipped_stale = 0_u64;
    let mut cancelled = false;

    for project_result in project_dirs {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            break;
        }
        let project = match project_result {
            Ok(project) => project,
            Err(error) => {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "AMBIENT_PROJECT_ENTRY_FAILED",
                    root = %root.display(),
                    detail = %error,
                    remediation = "repair directory enumeration/permissions; this cycle is explicitly incomplete",
                    "ambient ingest could not enumerate one project-root entry"
                );
                continue;
            }
        };
        let project_path = project.path();
        let project_type = match project.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "AMBIENT_PROJECT_TYPE_FAILED",
                    project = %project_path.display(),
                    detail = %error,
                    remediation = "repair metadata access to the project entry; this cycle is explicitly incomplete",
                    "ambient ingest could not determine a project entry's type"
                );
                continue;
            }
        };
        if !project_type.is_dir() {
            continue;
        }
        let files = match std::fs::read_dir(&project_path) {
            Ok(files) => files,
            Err(error) => {
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    code = "AMBIENT_PROJECT_DIR_UNREADABLE",
                    project = %project_path.display(),
                    detail = %error,
                    remediation = "restore directory enumeration access; this cycle is explicitly incomplete",
                    "ambient ingest could not list a project directory"
                );
                errors += 1;
                continue;
            }
        };
        for file_result in files {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                cancelled = true;
                break;
            }
            let file = match file_result {
                Ok(file) => file,
                Err(error) => {
                    errors += 1;
                    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        code = "AMBIENT_SESSION_ENTRY_FAILED",
                        project = %project_path.display(),
                        detail = %error,
                        remediation = "repair directory enumeration/permissions; this cycle is explicitly incomplete",
                        "ambient ingest could not enumerate one session entry"
                    );
                    continue;
                }
            };
            let path = file.path();
            // Main session transcripts only: `<project>/<uuid>.jsonl`. Subagent
            // sidecars live under `<uuid>/subagents/` and are a follow-up.
            let Some(extension) = path.extension() else {
                continue;
            };
            let Some(extension) = extension.to_str() else {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "AMBIENT_SESSION_EXTENSION_NOT_UTF8",
                    path = %path.display(),
                    remediation = "rename or remove the non-UTF-8 entry after reconciling whether it owns a transcript source",
                    "ambient session entry extension is not valid UTF-8"
                );
                continue;
            };
            if extension != "jsonl" {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "AMBIENT_SESSION_STEM_NOT_UTF8",
                    path = %path.display(),
                    remediation = "rename or remove the non-UTF-8 entry after reconciling whether it owns a transcript source",
                    "ambient session entry stem is not valid UTF-8"
                );
                continue;
            };
            if !is_session_stem(stem) {
                continue;
            }
            let metadata = match file.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        code = "AMBIENT_SESSION_STAT_FAILED",
                        path = %path.display(),
                        detail = %error,
                        remediation = "restore metadata access to the exact session source; this cycle is explicitly incomplete",
                        "could not stat an ambient session file"
                    );
                    errors += 1;
                    continue;
                }
            };
            if !metadata.is_file() {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "AMBIENT_SESSION_SOURCE_NOT_FILE",
                    path = %path.display(),
                    session_id = stem,
                    remediation = "restore the canonical UUID.jsonl session source as a regular append-only file",
                    "canonical ambient session identity is not backed by a regular file"
                );
                continue;
            }
            // Skip sessions idle longer than the window — unless we already
            // track them (a registered session must keep tailing its tail).
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    errors += 1;
                    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        code = "AMBIENT_SESSION_MODIFIED_TIME_FAILED",
                        path = %path.display(),
                        detail = %error,
                        remediation = "repair source metadata timestamps; this cycle cannot classify staleness safely",
                        "ambient session modified time is unreadable"
                    );
                    continue;
                }
            };
            let modified_secs = match modified.duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => duration.as_secs(),
                Err(error) => {
                    errors += 1;
                    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        code = "AMBIENT_SESSION_MODIFIED_TIME_INVALID",
                        path = %path.display(),
                        detail = %error,
                        remediation = "repair the source timestamp/host clock; this cycle cannot classify staleness safely",
                        "ambient session modified time predates the Unix epoch"
                    );
                    continue;
                }
            };
            let idle_secs = now_secs.saturating_sub(modified_secs);
            let already_tracked = match load_cursor(db, &spawn_id_for_session(stem)) {
                Ok(loaded) => loaded.cursor.is_some(),
                Err(detail) => {
                    errors += 1;
                    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        code = "AMBIENT_DISCOVERY_CURSOR_READ_FAILED",
                        path = %path.display(),
                        session_id = stem,
                        detail = %detail,
                        remediation = "repair the exact durable cursor read; this session is not classified as absent or stale",
                        "ambient discovery could not read the session cursor"
                    );
                    continue;
                }
            };
            if idle_secs > max_idle_secs && !already_tracked {
                skipped_stale += 1;
                continue;
            }

            sessions_seen += 1;
            match ingest_session_file_with_cancel(db, stem, &path, cancel) {
                Ok(outcome) => {
                    new_rows += outcome.new_parsed_rows + outcome.new_invalid_rows;
                    if outcome.newly_registered {
                        registered += 1;
                    }
                    if outcome.deferred_for_pressure {
                        deferred += 1;
                    }
                    if outcome.cancelled {
                        cancelled = true;
                        break;
                    }
                }
                Err(detail) => {
                    errors += 1;
                    tracing::error!(
                        code = "AMBIENT_SESSION_INGEST_FAILED",
                        session_id = stem,
                        source_path = %path.display(),
                        detail = %detail,
                        remediation = "follow the structured error remediation; the cursor is never advanced past unverified state",
                        "one ambient transcript failed during this explicitly incomplete cycle"
                    );
                }
            }
        }
        if cancelled {
            break;
        }
    }

    let summary = json!({
        "sessions_seen": sessions_seen,
        "new_rows": new_rows,
        "sessions_registered": registered,
        "errors": errors,
        "pressure_deferred": deferred,
        "skipped_stale": skipped_stale,
        "cancelled": cancelled,
    });
    if cancelled {
        tracing::info!(
            code = "AMBIENT_INGEST_CYCLE_CANCELLED",
            sessions_seen,
            new_rows,
            sessions_registered = registered,
            errors,
            pressure_deferred = deferred,
            skipped_stale,
            "ambient ingest cycle stopped early for daemon shutdown"
        );
        return summary;
    }
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
            run_cycle(&m3_state, &root, max_idle_secs, &cancel);
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

fn run_cycle(
    m3_state: &Arc<Mutex<M3State>>,
    root: &Path,
    max_idle_secs: u64,
    cancel: &CancellationToken,
) {
    if cancel.is_cancelled() {
        tracing::info!(
            code = "AMBIENT_INGEST_CYCLE_CANCELLED",
            "daemon shutdown cancelled ambient ingestion before storage open"
        );
        return;
    }
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
    let _summary = ingest_all_once_with_cancel(&db, root, max_idle_secs, Some(cancel));
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

const CLAUDE_SESSION_METADATA_TYPES: &[&str] = &[
    "mode",
    "file-history-snapshot",
    "file-history-delta",
    "ai-title",
    "attachment",
    "last-prompt",
    "queue-operation",
    "result",
    "permission-mode",
    "pr-link",
    "worktree-state",
    "agent-name",
];

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
        transcript_source_ts_ns(None, &cursor.spawn_id, line_no, cursor.source_epoch_unix_ms),
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
    record.ts_ns = transcript_source_ts_ns(
        Some(object),
        &cursor.spawn_id,
        line_no,
        cursor.source_epoch_unix_ms,
    );

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
        event_type if CLAUDE_SESSION_METADATA_TYPES.contains(&event_type) => {
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
                            // #1926: `is_error` is the only adjudicated outcome
                            // the ambient path ever observes, and it was being
                            // collapsed — `false` and *absent* both became
                            // `None`, which threw away the difference between
                            // "the provider declared success" and "the provider
                            // said nothing". Both still adjudicate to success
                            // (see `agent_transcript_tool_outcome`), but they
                            // are different observations and #1918 exists
                            // because two different things were once allowed to
                            // read as one. A present-but-non-boolean `is_error`
                            // is refused rather than silently dropped.
                            status: match block_object.get("is_error") {
                                None | Some(Value::Null) => None,
                                Some(Value::Bool(true)) => {
                                    Some(TRANSCRIPT_TOOL_STATUS_ERROR.to_owned())
                                }
                                Some(Value::Bool(false)) => {
                                    Some(TRANSCRIPT_TOOL_STATUS_OK.to_owned())
                                }
                                Some(other) => {
                                    return Err(format!(
                                        "TOOL_RESULT_IS_ERROR_NOT_BOOLEAN: is_error={other}"
                                    ));
                                }
                            },
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
