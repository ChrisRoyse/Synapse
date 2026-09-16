//! `CF_AGENT_TRANSCRIPTS` ingester (#900): tails spawned-agent stdout JSONL
//! streams into durable normalized transcript rows.
//!
//! # Source of truth and identity
//!
//! Every `act_spawn_agent` run owns a log dir under the spawn root
//! (`%LOCALAPPDATA%\Synapse\agent-spawns\<spawn-id>`) whose `stdout.jsonl`
//! is the agent CLI's own event stream — Claude Code
//! `--output-format stream-json` or Codex `exec --json`. That file is the
//! authoritative transcript for the spawn; rows are keyed
//! `(spawn_id, line_no)` so they reconcile line-for-line against it.
//!
//! # Tailing contract (Filebeat/Fluent Bit-style checkpointing)
//!
//! A durable per-spawn cursor row in `CF_KV` records the byte offset, line
//! number, and parser state. Each cycle reads only bytes past the offset,
//! consumes complete lines (a trailing partial line waits for the next
//! cycle unless the source is being finalized), and advances the cursor
//! only after the transcript rows are durable. Re-ingesting a line is
//! idempotent: the same line always lands on the same key. A source file
//! that shrinks below the cursor offset is a `TRANSCRIPT_SOURCE_TRUNCATED`
//! sticky error — surfaced loudly, never silently re-read.
//!
//! # Fail-loud parsing
//!
//! Parsers are version-pinned to the event vocabularies verified against
//! real captured streams (both formats are known to drift across CLI
//! releases). An unparseable or unknown line still writes a row — status
//! `invalid`, carrying the structured parse error, raw-line hash, and byte
//! count — and bumps `TRANSCRIPT_LINES_INVALID_TOTAL`, so the line-for-line
//! reconciliation holds and format drift surfaces as a counted, logged
//! defect instead of a silent skip.
//!
//! # Pressure
//!
//! `CF_AGENT_TRANSCRIPTS` sheds at disk-pressure Level3 (rows are
//! re-ingestable from the files on disk). The ingester checks
//! `pressure_permits_write` BEFORE writing and defers the whole cycle —
//! cursor untouched, deferral logged — so shedding is an explicit delay,
//! never silent loss.

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
    AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS, AgentTranscriptRecord, TranscriptModelUsage,
    TranscriptParseStatus, TranscriptRole, TranscriptSource, TranscriptToolCall, TranscriptUsage,
};
use synapse_storage::{
    Db,
    agent_transcripts::{
        agent_transcript_key, agent_transcript_spawn_prefix, agent_transcript_ts_index_key,
    },
    cf, decode_json, encode_json,
};
use tokio_util::sync::CancellationToken;

use crate::m3::{M3State, default_daemon_db_path, default_db_path};

/// Environment variable: seconds between periodic ingest cycles.
pub(crate) const INTERVAL_ENV: &str = "SYNAPSE_TRANSCRIPT_INGEST_INTERVAL_SECS";
/// Environment variable: delay before the first cycle.
pub(crate) const STARTUP_DELAY_ENV: &str = "SYNAPSE_TRANSCRIPT_INGEST_STARTUP_DELAY_SECS";
/// Environment variable: explicit spawn-root scope for custom/scratch DB runs.
pub(crate) const ROOT_ENV: &str = "SYNAPSE_TRANSCRIPT_INGEST_SPAWN_ROOT";
const DEFAULT_INTERVAL_SECS: u64 = 15;
const DEFAULT_STARTUP_DELAY_SECS: u64 = 10;

/// `CF_KV` key prefix for per-spawn ingest cursors.
pub(crate) const CURSOR_KV_PREFIX: &str = "agent-transcripts/cursor/";

/// Envelope version for [`TranscriptCursor`] rows.
const TRANSCRIPT_CURSOR_VERSION: u32 = 1;

/// Hard cap on one encoded transcript row. The per-field bounds keep real
/// rows far below this; exceeding it means an ingester bug, surfaced as a
/// sticky error for the spawn.
pub(crate) const MAX_AGENT_TRANSCRIPT_VALUE_BYTES: usize = 32 * 1024;

static LINES_PARSED_TOTAL: AtomicU64 = AtomicU64::new(0);
static LINES_INVALID_TOTAL: AtomicU64 = AtomicU64::new(0);
static SOURCES_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
static INGEST_ERRORS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PRESSURE_DEFERRALS_TOTAL: AtomicU64 = AtomicU64::new(0);
static CYCLES_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime ingest counters for `GET /agent-transcripts/stats`.
pub(crate) fn ingest_stats() -> Value {
    json!({
        "lines_parsed_total": LINES_PARSED_TOTAL.load(Ordering::Relaxed),
        "lines_invalid_total": LINES_INVALID_TOTAL.load(Ordering::Relaxed),
        "sources_completed_total": SOURCES_COMPLETED_TOTAL.load(Ordering::Relaxed),
        "ingest_errors_total": INGEST_ERRORS_TOTAL.load(Ordering::Relaxed),
        "pressure_deferrals_total": PRESSURE_DEFERRALS_TOTAL.load(Ordering::Relaxed),
        "cycles_total": CYCLES_TOTAL.load(Ordering::Relaxed),
    })
}

/// Durable per-spawn tail state, stored in `CF_KV` under the
/// [`CURSOR_KV_PREFIX`] key namespace (one row per spawn id).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TranscriptCursor {
    pub record_version: u32,
    pub spawn_id: String,
    pub source: TranscriptSource,
    pub source_path: String,
    /// Byte offset of the first unconsumed byte in the source file.
    pub offset_bytes: u64,
    /// Count of source lines ingested so far (== highest `line_no` written).
    pub lines_ingested: u64,
    pub parsed_rows: u64,
    pub invalid_rows: u64,
    /// Current turn counter (Claude: distinct assistant message ids; Codex:
    /// `turn.started` events).
    pub turn_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// True once the source reached its terminal state and the tail was
    /// fully consumed; complete spawns are skipped by later cycles.
    pub source_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_reason: Option<String>,
    /// Sticky structured error. A spawn with a sticky error is skipped (and
    /// counted) until an operator clears the cursor row; ingestion never
    /// guesses past a corrupt source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_ts_ns: u64,
}

/// Outcome of one ingest pass over one spawn dir.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SpawnIngestOutcome {
    pub new_parsed_rows: u64,
    pub new_invalid_rows: u64,
    pub lines_ingested_total: u64,
    pub source_complete: bool,
    pub deferred_for_pressure: bool,
    pub skipped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TranscriptRootDecision {
    root: PathBuf,
    scope: TranscriptRootScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptRootScope {
    ExplicitEnv,
    ConfiguredDaemonDb,
}

impl TranscriptRootScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitEnv => "explicit_env",
            Self::ConfiguredDaemonDb => "configured_daemon_db",
        }
    }
}

fn cursor_kv_key(spawn_id: &str) -> Vec<u8> {
    format!("{CURSOR_KV_PREFIX}{spawn_id}").into_bytes()
}

fn unix_time_ns_now() -> u64 {
    super::agent_events::unix_time_ns_now()
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

/// Truncates `text` to at most `max_chars` characters on a char boundary.
/// Returns the bounded text and whether truncation occurred.
fn bounded_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

/// Validates a directory name as a spawn id (same path-safety invariant as
/// the push-telemetry ingress, #899).
fn validate_spawn_id_shape(spawn_id: &str) -> Result<(), String> {
    if !spawn_id.starts_with("agent-spawn-") {
        return Err(format!(
            "spawn id must start with \"agent-spawn-\", got {spawn_id:?}"
        ));
    }
    if spawn_id.len() > 128 {
        return Err(format!("spawn id exceeds 128 chars ({})", spawn_id.len()));
    }
    if !spawn_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err("spawn id must contain only ASCII alphanumerics and dashes".to_owned());
    }
    Ok(())
}

/// Determines which version-pinned parser owns a spawn dir from the
/// CLI-specific config artifacts `act_spawn_agent` writes at launch.
///
/// # Errors
///
/// Returns a structured detail when the markers are absent or ambiguous —
/// an unattributable dir is a surfaced defect, never a guessed format.
fn detect_source(log_dir: &Path) -> Result<TranscriptSource, String> {
    let claude = log_dir.join("claude-mcp-config.json").is_file()
        || log_dir.join("claude-hook-settings.json").is_file()
        || log_dir.join("claude-debug.log").is_file();
    let codex_app_server = log_dir.join("codex-app-server-runner.ps1").is_file()
        || log_dir.join("codex-control.json").is_file()
        || log_dir.join("codex-app-server-events.jsonl").is_file();
    let codex = !codex_app_server && log_dir.join("codex-notify.ps1").is_file();
    let local = log_dir.join("local-model-runner.json").is_file();
    let mut matches = Vec::new();
    if claude {
        matches.push(TranscriptSource::ClaudeStreamJson);
    }
    if codex_app_server {
        matches.push(TranscriptSource::CodexAppServerJsonRpc);
    }
    if codex {
        matches.push(TranscriptSource::CodexExecJson);
    }
    if local {
        matches.push(TranscriptSource::LocalModelJson);
    }
    match matches.as_slice() {
        [source] => Ok(*source),
        [] => Err(
            "TRANSCRIPT_SOURCE_FORMAT_UNKNOWN: spawn dir carries neither Claude, Codex, nor local-model launch artifacts"
                .to_owned(),
        ),
        _ => Err(
            "TRANSCRIPT_SOURCE_AMBIGUOUS: spawn dir carries multiple agent launch artifact families"
                .to_owned(),
        ),
    }
}

/// Reads the model id recorded in the spawn manifest, if present. This is the
/// authoritative model source for Codex spawns, whose `exec --json` stream
/// carries no model id (#949); for Claude it merely seeds the cursor until the
/// stream's own (more specific) model id supersedes it. A missing or malformed
/// manifest is not an error here — the spawn simply has no pinned model, and a
/// model-less spawn is honestly reported as `unknown`/unpriced downstream.
fn read_spawn_manifest_model(log_dir: &Path) -> Option<String> {
    let path = log_dir.join(super::m4_tools::AGENT_SPAWN_MANIFEST_FILENAME);
    let bytes = std::fs::read(&path).ok()?;
    let manifest: Value = serde_json::from_slice(&bytes).ok()?;
    let model = manifest.get("model")?.as_str()?.trim();
    if model.is_empty() {
        return None;
    }
    Some(model.to_owned())
}

fn load_cursor(db: &Db, spawn_id: &str) -> Result<Option<TranscriptCursor>, String> {
    let key = cursor_kv_key(spawn_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, &key)
        .map_err(|error| format!("TRANSCRIPT_CURSOR_READ_FAILED: {error}"))?;
    for (row_key, value) in rows {
        if row_key == key {
            let cursor: TranscriptCursor = decode_json(&value)
                .map_err(|error| format!("TRANSCRIPT_CURSOR_DECODE_FAILED: {error}"))?;
            return Ok(Some(cursor));
        }
    }
    Ok(None)
}

fn store_cursor(db: &Db, cursor: &TranscriptCursor) -> Result<(), String> {
    let encoded =
        encode_json(cursor).map_err(|error| format!("TRANSCRIPT_CURSOR_ENCODE_FAILED: {error}"))?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(cursor_kv_key(&cursor.spawn_id), encoded)])
        .map_err(|error| format!("TRANSCRIPT_CURSOR_WRITE_FAILED: {error}"))
}

/// Marks a spawn's cursor with a sticky error and persists it. The error is
/// logged once here (with full context) and the spawn is skipped by later
/// cycles until the cursor row is cleared.
fn stick_cursor_error(db: &Db, cursor: &mut TranscriptCursor, detail: String) -> String {
    INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
    tracing::error!(
        code = "TRANSCRIPT_INGEST_ERROR",
        spawn_id = %cursor.spawn_id,
        source_path = %cursor.source_path,
        offset_bytes = cursor.offset_bytes,
        lines_ingested = cursor.lines_ingested,
        detail = %detail,
        "transcript ingestion hit a sticky error; spawn is parked until the cursor is cleared"
    );
    cursor.error = Some(detail.clone());
    cursor.updated_ts_ns = unix_time_ns_now();
    if let Err(store_error) = store_cursor(db, cursor) {
        tracing::error!(
            code = "TRANSCRIPT_INGEST_ERROR",
            spawn_id = %cursor.spawn_id,
            detail = %store_error,
            "failed to persist the sticky cursor error itself"
        );
    }
    detail
}

/// True when `completion-status.json` exists with a terminal status.
fn completion_is_terminal(log_dir: &Path) -> bool {
    let path = log_dir.join("completion-status.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|status| {
            status
                .get("status")
                .and_then(Value::as_str)
                .map(|value| value != "running")
        })
        .unwrap_or(false)
}

/// Ingests new source bytes for one spawn dir. `finalize` forces the tail
/// (including a trailing unterminated line) to be consumed and the cursor
/// marked complete — used at session teardown and when the completion
/// artifact is terminal.
///
/// # Errors
///
/// Returns the structured sticky-error detail when the source is missing,
/// truncated, unattributable, or a row exceeds the encoded-size cap. The
/// same detail is persisted on the cursor so later cycles skip the spawn.
pub(crate) fn ingest_spawn_dir_once(
    db: &Db,
    spawn_id: &str,
    log_dir: &Path,
    finalize: bool,
) -> Result<SpawnIngestOutcome, String> {
    validate_spawn_id_shape(spawn_id)?;
    let stdout_path = log_dir.join("stdout.jsonl");

    let mut cursor = match load_cursor(db, spawn_id)? {
        Some(cursor) => cursor,
        None => {
            let source = match detect_source(log_dir) {
                Ok(source) => source,
                Err(detail) => {
                    // No cursor exists yet; create one purely to park the
                    // error so the defect is counted once, not every cycle.
                    let mut cursor = TranscriptCursor {
                        record_version: TRANSCRIPT_CURSOR_VERSION,
                        spawn_id: spawn_id.to_owned(),
                        source: TranscriptSource::ClaudeStreamJson,
                        source_path: stdout_path.display().to_string(),
                        offset_bytes: 0,
                        lines_ingested: 0,
                        parsed_rows: 0,
                        invalid_rows: 0,
                        turn_index: 0,
                        last_assistant_message_id: None,
                        conversation_id: None,
                        model: None,
                        source_complete: false,
                        completed_reason: None,
                        error: None,
                        updated_ts_ns: unix_time_ns_now(),
                    };
                    return Err(stick_cursor_error(db, &mut cursor, detail));
                }
            };
            TranscriptCursor {
                record_version: TRANSCRIPT_CURSOR_VERSION,
                spawn_id: spawn_id.to_owned(),
                source,
                source_path: stdout_path.display().to_string(),
                offset_bytes: 0,
                lines_ingested: 0,
                parsed_rows: 0,
                invalid_rows: 0,
                turn_index: 0,
                last_assistant_message_id: None,
                conversation_id: None,
                // Seed from the spawn manifest. For Codex this is the only model
                // source; for Claude the stream supersedes it (#949).
                model: read_spawn_manifest_model(log_dir),
                source_complete: false,
                completed_reason: None,
                error: None,
                updated_ts_ns: unix_time_ns_now(),
            }
        }
    };

    if cursor.source_complete {
        return Ok(SpawnIngestOutcome {
            lines_ingested_total: cursor.lines_ingested,
            source_complete: true,
            skipped: true,
            ..SpawnIngestOutcome::default()
        });
    }
    if let Some(error) = &cursor.error {
        tracing::debug!(
            code = "TRANSCRIPT_INGEST_PARKED",
            spawn_id,
            detail = %error,
            "skipping spawn with sticky ingest error"
        );
        return Ok(SpawnIngestOutcome {
            lines_ingested_total: cursor.lines_ingested,
            skipped: true,
            ..SpawnIngestOutcome::default()
        });
    }

    let metadata = match std::fs::metadata(&stdout_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                format!(
                    "TRANSCRIPT_SOURCE_MISSING: cannot stat {}: {error}",
                    stdout_path.display()
                ),
            ));
        }
    };
    let file_size = metadata.len();
    if file_size < cursor.offset_bytes {
        let detail = format!(
            "TRANSCRIPT_SOURCE_TRUNCATED: file is {file_size} bytes but the cursor consumed {} — the source shrank underneath the tail",
            cursor.offset_bytes
        );
        return Err(stick_cursor_error(db, &mut cursor, detail));
    }

    let finalize = finalize || completion_is_terminal(log_dir);
    let mut new_bytes = Vec::new();
    if file_size > cursor.offset_bytes {
        let mut file = std::fs::File::open(&stdout_path).map_err(|error| {
            format!(
                "TRANSCRIPT_SOURCE_OPEN_FAILED: {}: {error}",
                stdout_path.display()
            )
        })?;
        file.seek(SeekFrom::Start(cursor.offset_bytes))
            .map_err(|error| format!("TRANSCRIPT_SOURCE_SEEK_FAILED: {error}"))?;
        file.read_to_end(&mut new_bytes)
            .map_err(|error| format!("TRANSCRIPT_SOURCE_READ_FAILED: {error}"))?;
    }

    // Split into complete lines; a trailing chunk without a newline is left
    // for the next cycle unless this pass finalizes the source. A single
    // trailing CR is part of the line terminator (the spawn wrapper writes
    // CRLF on Windows), so it is excluded from the recorded bytes and hash —
    // hashes stay reproducible from the logical line text regardless of the
    // producer's line-ending convention.
    fn trim_line_terminator(line: &[u8]) -> &[u8] {
        line.strip_suffix(b"\r").unwrap_or(line)
    }
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut consumed_bytes = 0_usize;
    let mut start = 0_usize;
    for (index, byte) in new_bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(trim_line_terminator(&new_bytes[start..index]));
            start = index + 1;
            consumed_bytes = start;
        }
    }
    if finalize && start < new_bytes.len() {
        lines.push(trim_line_terminator(&new_bytes[start..]));
        consumed_bytes = new_bytes.len();
    }

    if lines.is_empty() && !finalize {
        return Ok(SpawnIngestOutcome {
            lines_ingested_total: cursor.lines_ingested,
            ..SpawnIngestOutcome::default()
        });
    }

    // Explicit pressure gate: rows below ride a bypass write, so this check
    // is the single authority on whether this cycle may write at all.
    if !db.pressure_permits_write(cf::CF_AGENT_TRANSCRIPTS) {
        PRESSURE_DEFERRALS_TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            code = "TRANSCRIPT_INGEST_PRESSURE_DEFERRED",
            spawn_id,
            pending_lines = lines.len(),
            "disk pressure defers transcript ingestion; cursor not advanced"
        );
        return Ok(SpawnIngestOutcome {
            lines_ingested_total: cursor.lines_ingested,
            deferred_for_pressure: true,
            ..SpawnIngestOutcome::default()
        });
    }

    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(lines.len());
    let mut ts_index_rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(lines.len());
    let mut new_parsed = 0_u64;
    let mut new_invalid = 0_u64;
    for raw_line in &lines {
        let line_no = cursor.lines_ingested + 1;
        let record = parse_line(raw_line, line_no, &mut cursor);
        match record.status {
            TranscriptParseStatus::Parsed => new_parsed += 1,
            TranscriptParseStatus::Invalid => {
                new_invalid += 1;
                tracing::error!(
                    code = "TRANSCRIPT_LINE_INVALID",
                    spawn_id,
                    line_no,
                    offset_bytes = cursor.offset_bytes,
                    raw_line_bytes = record.raw_line_bytes,
                    detail = record.parse_error.as_deref().unwrap_or("unknown"),
                    "source line refused by the version-pinned parser; invalid row written"
                );
            }
        }
        record
            .validate()
            .map_err(|detail| stick_cursor_error(db, &mut cursor, detail))?;
        let encoded = encode_json(&record)
            .map_err(|error| format!("TRANSCRIPT_ROW_ENCODE_FAILED: {error}"))?;
        if encoded.len() > MAX_AGENT_TRANSCRIPT_VALUE_BYTES {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                format!(
                    "TRANSCRIPT_ROW_OVERSIZED: line {line_no} encoded to {} bytes, cap is {MAX_AGENT_TRANSCRIPT_VALUE_BYTES}; the per-field bounds failed",
                    encoded.len()
                ),
            ));
        }
        let transcript_key = agent_transcript_key(spawn_id, line_no);
        ts_index_rows.push((
            agent_transcript_ts_index_key(record.ts_ns, &transcript_key),
            transcript_key.clone(),
        ));
        rows.push((transcript_key, encoded));
        cursor.lines_ingested = line_no;
    }

    if !rows.is_empty() {
        db.put_cf_batches_pressure_bypass(vec![
            (cf::CF_AGENT_TRANSCRIPTS, rows),
            (cf::CF_KV, ts_index_rows),
        ])
        .map_err(|error| {
            format!(
                "TRANSCRIPT_ROWS_WRITE_FAILED: {error} (cursor not advanced; lines will re-ingest)"
            )
        })?;
    }

    cursor.offset_bytes += consumed_bytes as u64;
    cursor.parsed_rows += new_parsed;
    cursor.invalid_rows += new_invalid;
    cursor.updated_ts_ns = unix_time_ns_now();
    LINES_PARSED_TOTAL.fetch_add(new_parsed, Ordering::Relaxed);
    LINES_INVALID_TOTAL.fetch_add(new_invalid, Ordering::Relaxed);

    if finalize {
        cursor.source_complete = true;
        cursor.completed_reason = Some(if completion_is_terminal(log_dir) {
            "completion_status_terminal".to_owned()
        } else {
            "finalized_at_teardown".to_owned()
        });
        store_cursor(db, &cursor)?;
        db.flush()
            .map_err(|error| format!("TRANSCRIPT_FINAL_FLUSH_FAILED: {error}"))?;
        // Physical read-back at completion: the row count under the spawn's
        // key prefix must equal the lines ingested. A mismatch is a defect.
        let physical_rows = db
            .scan_cf_prefix(
                cf::CF_AGENT_TRANSCRIPTS,
                &agent_transcript_spawn_prefix(spawn_id),
            )
            .map_err(|error| format!("TRANSCRIPT_READBACK_FAILED: {error}"))?;
        if physical_rows.len() as u64 != cursor.lines_ingested {
            let detail = format!(
                "TRANSCRIPT_READBACK_MISMATCH: {} physical rows but cursor ingested {} lines",
                physical_rows.len(),
                cursor.lines_ingested
            );
            return Err(stick_cursor_error(db, &mut cursor, detail));
        }
        SOURCES_COMPLETED_TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            code = "TRANSCRIPT_SOURCE_COMPLETED",
            spawn_id,
            lines = cursor.lines_ingested,
            parsed_rows = cursor.parsed_rows,
            invalid_rows = cursor.invalid_rows,
            physical_rows = physical_rows.len(),
            reason = cursor.completed_reason.as_deref().unwrap_or("unknown"),
            "readback=CF_AGENT_TRANSCRIPTS edge=source_complete"
        );
    } else {
        store_cursor(db, &cursor)?;
    }

    Ok(SpawnIngestOutcome {
        new_parsed_rows: new_parsed,
        new_invalid_rows: new_invalid,
        lines_ingested_total: cursor.lines_ingested,
        source_complete: cursor.source_complete,
        ..SpawnIngestOutcome::default()
    })
}

/// One pass over every spawn dir under `root`. Per-spawn errors are sticky
/// and already logged; the cycle continues so one corrupt spawn can never
/// stall the fleet's transcripts.
pub(crate) fn ingest_all_spawn_dirs_once(db: &Db, root: &Path) -> Value {
    CYCLES_TOTAL.fetch_add(1, Ordering::Relaxed);
    let mut dirs_seen = 0_u64;
    let mut new_rows = 0_u64;
    let mut completed = 0_u64;
    let mut errors = 0_u64;
    let mut deferred = 0_u64;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            // Root absence is normal before the first spawn on a machine.
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::error!(
                    code = "TRANSCRIPT_INGEST_CYCLE_FAILED",
                    root = %root.display(),
                    detail = %error,
                    "transcript ingest cycle could not list the spawn root"
                );
            }
            return json!({"dirs_seen": 0, "error": error.to_string()});
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(spawn_id) = name.to_str() else {
            continue;
        };
        if validate_spawn_id_shape(spawn_id).is_err() {
            continue;
        }
        let log_dir = entry.path();
        if !log_dir.is_dir() {
            continue;
        }
        dirs_seen += 1;
        match ingest_spawn_dir_once(db, spawn_id, &log_dir, false) {
            Ok(outcome) => {
                new_rows += outcome.new_parsed_rows + outcome.new_invalid_rows;
                if outcome.source_complete && !outcome.skipped {
                    completed += 1;
                }
                if outcome.deferred_for_pressure {
                    deferred += 1;
                }
            }
            Err(_detail) => {
                // Already logged with full context by stick_cursor_error.
                errors += 1;
            }
        }
    }
    let summary = json!({
        "dirs_seen": dirs_seen,
        "new_rows": new_rows,
        "sources_completed": completed,
        "errors": errors,
        "pressure_deferred": deferred,
    });
    if new_rows > 0 || completed > 0 || errors > 0 || deferred > 0 {
        tracing::info!(
            code = "TRANSCRIPT_INGEST_CYCLE_OK",
            dirs_seen,
            new_rows,
            sources_completed = completed,
            errors,
            pressure_deferred = deferred,
            "transcript ingest cycle finished"
        );
    } else {
        tracing::debug!(
            code = "TRANSCRIPT_INGEST_CYCLE_IDLE",
            dirs_seen,
            "transcript ingest cycle found nothing new"
        );
    }
    summary
}

/// Final transcript flush for one spawn at session teardown (#900
/// "rotation/teardown handled"): consumes the tail (the processes are dead
/// by the time this runs) and marks the source complete.
pub(crate) fn finalize_spawn_transcripts(db: &Db, spawn_id: &str, log_dir: &Path) {
    match ingest_spawn_dir_once(db, spawn_id, log_dir, true) {
        Ok(outcome) => {
            tracing::info!(
                code = "TRANSCRIPT_TEARDOWN_FLUSH_OK",
                spawn_id,
                new_rows = outcome.new_parsed_rows + outcome.new_invalid_rows,
                lines_total = outcome.lines_ingested_total,
                "teardown transcript flush completed"
            );
        }
        Err(detail) => {
            // Already logged with context; teardown carries on — the
            // periodic cycle keeps the sticky error visible.
            tracing::error!(
                code = "TRANSCRIPT_TEARDOWN_FLUSH_FAILED",
                spawn_id,
                detail = %detail,
                "teardown transcript flush failed"
            );
        }
    }
}

/// Spawns the periodic ingest task (daemon HTTP startup), mirroring the
/// routine-miner job contract: invalid env overrides are a startup error,
/// `0` disables the job.
///
/// # Errors
///
/// Returns an error when an environment override is present but
/// unparseable — a misconfigured daemon must fail at startup, not run with
/// a silently substituted schedule.
pub(crate) fn spawn_periodic_transcript_ingest(
    m3_state: Arc<Mutex<M3State>>,
    cancel: CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = parse_secs_env(INTERVAL_ENV, DEFAULT_INTERVAL_SECS)?;
    let startup_delay_secs = parse_secs_env(STARTUP_DELAY_ENV, DEFAULT_STARTUP_DELAY_SECS)?;
    if interval_secs == 0 {
        tracing::info!(
            code = "TRANSCRIPT_INGEST_PERIODIC_DISABLED",
            "periodic transcript ingestion disabled via {INTERVAL_ENV}=0"
        );
        return Ok(None);
    }
    let db_path = configured_db_path(&m3_state)?;
    let Some(root_decision) =
        transcript_spawn_root_for_db(&db_path).map_err(|detail| anyhow::anyhow!(detail))?
    else {
        tracing::warn!(
            code = "TRANSCRIPT_INGEST_CUSTOM_DB_UNSCOPED",
            db_path = %db_path.display(),
            default_db_path = %default_db_path().display(),
            default_daemon_db_path = %default_daemon_db_path().display(),
            explicit_root_env = ROOT_ENV,
            remediation = "set SYNAPSE_TRANSCRIPT_INGEST_SPAWN_ROOT for this run or use the configured daemon DB path",
            "periodic transcript ingestion disabled for custom DB without an explicit spawn root"
        );
        return Ok(None);
    };
    let TranscriptRootDecision { root, scope } = root_decision;
    tracing::info!(
        code = "TRANSCRIPT_INGEST_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        root = %root.display(),
        root_scope = scope.as_str(),
        db_path = %db_path.display(),
        "periodic transcript ingestion scheduled"
    );
    let handle = tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "TRANSCRIPT_INGEST_PERIODIC_STOPPED",
                        "periodic transcript ingestion stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            run_cycle(&m3_state, &root);
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

fn transcript_spawn_root_for_db(db_path: &Path) -> Result<Option<TranscriptRootDecision>, String> {
    let explicit_root = std::env::var_os(ROOT_ENV).map(PathBuf::from);
    transcript_spawn_root_for_db_with(db_path, explicit_root, || {
        super::m4_tools::agent_spawn_root_dir().map_err(|error| error.message.to_string())
    })
}

fn transcript_spawn_root_for_db_with(
    db_path: &Path,
    explicit_root: Option<PathBuf>,
    host_spawn_root: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<Option<TranscriptRootDecision>, String> {
    if let Some(root) = explicit_root {
        return Ok(Some(TranscriptRootDecision {
            root,
            scope: TranscriptRootScope::ExplicitEnv,
        }));
    }
    if !transcript_host_root_allowed_for_db(db_path) {
        return Ok(None);
    }
    Ok(Some(TranscriptRootDecision {
        root: host_spawn_root()?,
        scope: TranscriptRootScope::ConfiguredDaemonDb,
    }))
}

fn transcript_host_root_allowed_for_db(db_path: &Path) -> bool {
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

fn run_cycle(m3_state: &Arc<Mutex<M3State>>, root: &Path) {
    let db = {
        let mut state = match m3_state.lock() {
            Ok(state) => state,
            Err(_poisoned) => {
                tracing::error!(
                    code = "TRANSCRIPT_INGEST_CYCLE_FAILED",
                    detail = "m3 state lock poisoned",
                    "transcript ingest cycle could not access storage"
                );
                return;
            }
        };
        match state.ensure_storage() {
            Ok(db) => db,
            Err(error) => {
                tracing::error!(
                    code = "TRANSCRIPT_INGEST_CYCLE_FAILED",
                    detail = %error,
                    "transcript ingest cycle could not open storage"
                );
                return;
            }
        }
    };
    let _summary = ingest_all_spawn_dirs_once(&db, root);
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
// Version-pinned line parsers
// ---------------------------------------------------------------------------

/// Parses one raw source line into exactly one transcript row. Never fails:
/// a line the pinned vocabulary cannot place becomes an `invalid` row that
/// carries the structured reason.
fn parse_line(
    raw_line: &[u8],
    line_no: u64,
    cursor: &mut TranscriptCursor,
) -> AgentTranscriptRecord {
    let mut record = AgentTranscriptRecord::new(
        unix_time_ns_now(),
        cursor.spawn_id.clone(),
        line_no,
        cursor.source,
        raw_line.len() as u64,
        sha256_hex(raw_line),
    );
    let text = match std::str::from_utf8(raw_line) {
        Ok(text) => text,
        Err(error) => {
            record.status = TranscriptParseStatus::Invalid;
            record.parse_error = Some(format!("LINE_NOT_UTF8: {error}"));
            return record;
        }
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            record.status = TranscriptParseStatus::Invalid;
            record.parse_error = Some(format!("LINE_NOT_JSON: {error}"));
            return record;
        }
    };
    let Some(object) = value.as_object() else {
        record.status = TranscriptParseStatus::Invalid;
        record.parse_error = Some("LINE_NOT_JSON_OBJECT".to_owned());
        return record;
    };
    let result = match cursor.source {
        TranscriptSource::ClaudeStreamJson => parse_claude_object(object, &mut record, cursor),
        TranscriptSource::CodexExecJson => parse_codex_object(object, &mut record, cursor),
        TranscriptSource::CodexAppServerJsonRpc => {
            parse_codex_app_server_object(object, &mut record, cursor)
        }
        TranscriptSource::LocalModelJson => parse_local_model_object(object, &mut record, cursor),
        // The spawn-dir ingester never owns a session-file cursor; that
        // vocabulary is tailed by `ambient_agents`. Seeing it here is a routing
        // bug, surfaced as a fail-loud invalid row rather than a guessed parse.
        TranscriptSource::ClaudeSessionJsonl => Err(
            "CLAUDE_SESSION_JSONL_MISROUTED: ambient session transcripts are tailed by \
             ambient_agents, not the spawn-dir ingester"
                .to_owned(),
        ),
    };
    if let Err(detail) = result {
        record.status = TranscriptParseStatus::Invalid;
        record.parse_error = Some(detail);
        // A line the vocabulary rejects must not half-populate normalized
        // fields it guessed at.
        record.role = None;
        record.event_kind = None;
        record.tool_calls.clear();
        record.usage = None;
        record.content_summary = None;
        record.content_bytes = None;
        record.content_sha256 = None;
        record.content_truncated = false;
        record.source_error = None;
    } else {
        // Stamp stream-level identity onto every parsed row.
        record.conversation_id.clone_from(&cursor.conversation_id);
        if record.model.is_none() {
            record.model.clone_from(&cursor.model);
        }
        if cursor.turn_index > 0 {
            record.turn_index = Some(cursor.turn_index);
        }
    }
    record
}

fn set_content(record: &mut AgentTranscriptRecord, content: &str) {
    let (summary, truncated) = bounded_chars(content, AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS);
    record.content_bytes = Some(content.len() as u64);
    record.content_sha256 = Some(sha256_hex(content.as_bytes()));
    record.content_summary = Some(summary);
    record.content_truncated = truncated;
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

/// Claude Code `--output-format stream-json` vocabulary, pinned to the
/// event shapes captured from CLI 2.1.x real runs (see the fixture
/// `tests/fixtures/claude_stream_real.jsonl`): `system/<subtype>`,
/// `assistant`, `user`, `result`, `rate_limit_event`.
fn parse_claude_object(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
    cursor: &mut TranscriptCursor,
) -> Result<(), String> {
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "MISSING_TYPE: line has no string `type` field".to_owned())?;
    match event_type {
        "system" => {
            let subtype = object
                .get("subtype")
                .and_then(Value::as_str)
                .ok_or_else(|| "SYSTEM_MISSING_SUBTYPE".to_owned())?;
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some(format!("system/{subtype}"));
            if subtype == "init" {
                if let Some(session_id) = object.get("session_id").and_then(Value::as_str) {
                    cursor.conversation_id = Some(session_id.to_owned());
                }
                if let Some(model) = object.get("model").and_then(Value::as_str) {
                    cursor.model = Some(model.to_owned());
                }
            }
            Ok(())
        }
        "rate_limit_event" => {
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some("rate_limit_event".to_owned());
            Ok(())
        }
        "assistant" => {
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
            if let Some(message_id) = message.get("id").and_then(Value::as_str) {
                if cursor.last_assistant_message_id.as_deref() != Some(message_id) {
                    cursor.turn_index += 1;
                    cursor.last_assistant_message_id = Some(message_id.to_owned());
                }
            }
            let content = message
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| "ASSISTANT_MISSING_CONTENT_ARRAY".to_owned())?;
            let mut text_parts: Vec<String> = Vec::new();
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
                    // API-redacted reasoning: the content is opaque by
                    // design; the block's presence is the information.
                    "redacted_thinking" => {}
                    // Model-fallback notice (witnessed in real streams,
                    // 2026-06-12: claude-fable-5 -> claude-opus-4-8). The
                    // block is small and self-describing; carry it verbatim.
                    "fallback" => {
                        text_parts.push(Value::Object(block_object.clone()).to_string());
                    }
                    // `server_tool_use` is the API-side sibling of
                    // `tool_use` (web search etc.); same shape.
                    "tool_use" | "server_tool_use" => {
                        let tool_name = block_object
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "TOOL_USE_MISSING_NAME".to_owned())?
                            .to_owned();
                        let (arguments, arguments_bytes, arguments_truncated) = block_object
                            .get("input")
                            .map(|input| {
                                bounded_json_string(input, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS)
                            })
                            .unwrap_or_default();
                        record.tool_calls.push(TranscriptToolCall {
                            tool_name,
                            tool_call_id: block_object
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            arguments: Some(arguments),
                            arguments_bytes: Some(arguments_bytes),
                            arguments_truncated,
                            ..TranscriptToolCall::default()
                        });
                    }
                    // API-side tool results delivered inside the assistant
                    // message (web search results etc.).
                    "web_search_tool_result" => {
                        let (result_summary, result_bytes, result_truncated) = block_object
                            .get("content")
                            .map(|content| {
                                bounded_json_string(content, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS)
                            })
                            .unwrap_or_default();
                        record.tool_calls.push(TranscriptToolCall {
                            tool_name: "web_search_tool_result".to_owned(),
                            tool_call_id: block_object
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            result_summary: Some(result_summary),
                            result_bytes: Some(result_bytes),
                            result_truncated,
                            ..TranscriptToolCall::default()
                        });
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
            Ok(())
        }
        "user" => {
            let message = object
                .get("message")
                .and_then(Value::as_object)
                .ok_or_else(|| "USER_MISSING_MESSAGE".to_owned())?;
            record.role = Some(TranscriptRole::Tool);
            record.event_kind = Some("user/tool_result".to_owned());
            let content = message
                .get("content")
                .ok_or_else(|| "USER_MISSING_CONTENT".to_owned())?;
            match content {
                Value::String(text) => set_content(record, text),
                Value::Array(blocks) => {
                    for block in blocks {
                        let block_object = block
                            .as_object()
                            .ok_or_else(|| "USER_CONTENT_BLOCK_NOT_OBJECT".to_owned())?;
                        let block_type = block_object
                            .get("type")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "USER_CONTENT_BLOCK_MISSING_TYPE".to_owned())?;
                        if block_type != "tool_result" {
                            return Err(format!("UNKNOWN_USER_CONTENT_BLOCK: {block_type}"));
                        }
                        let (result_summary, result_bytes, result_truncated) = block_object
                            .get("content")
                            .map(|content| {
                                bounded_json_string(content, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS)
                            })
                            .unwrap_or_default();
                        record.tool_calls.push(TranscriptToolCall {
                            tool_name: "tool_result".to_owned(),
                            tool_call_id: block_object
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            result_summary: Some(result_summary),
                            result_bytes: Some(result_bytes),
                            result_truncated,
                            ..TranscriptToolCall::default()
                        });
                    }
                }
                _ => return Err("USER_CONTENT_NOT_STRING_OR_ARRAY".to_owned()),
            }
            Ok(())
        }
        "result" => {
            let subtype = object
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            record.role = Some(TranscriptRole::Result);
            record.event_kind = Some(format!("result/{subtype}"));
            if let Some(result_text) = object.get("result").and_then(Value::as_str) {
                set_content(record, result_text);
            }
            let mut usage = object.get("usage").map(claude_usage).unwrap_or_default();
            if let Some(cost) = object.get("total_cost_usd").and_then(Value::as_f64) {
                // Stored integer-exact in micro-USD.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let micro = (cost * 1_000_000.0).round().max(0.0) as u64;
                usage.total_cost_micro_usd = Some(micro);
            }
            // The per-model breakdown lets the cost engine attribute a
            // multi-model session exactly; the top-level `usage` above reflects
            // only the primary model (#949).
            if let Some(model_usage) = object.get("modelUsage") {
                usage.model_usage = claude_model_usage(model_usage)?;
            }
            if !usage.is_empty() {
                record.usage = Some(usage);
            }
            if object.get("is_error").and_then(Value::as_bool) == Some(true) {
                record.source_error = Some(format!("result/{subtype}"));
            }
            Ok(())
        }
        other => Err(format!("UNKNOWN_EVENT_TYPE: {other}")),
    }
}

fn claude_usage(usage: &Value) -> TranscriptUsage {
    // The cache-creation TTL split lives in a nested `cache_creation` object
    // (`ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`); the two tiers
    // are billed at 1.25x vs 2x base input, so capturing them lets the cost
    // engine price a mixed-TTL run exactly (#949).
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

/// Parses a Claude `result.modelUsage` map into the per-model breakdown.
/// Keys are model ids; values carry camelCase token counts and a per-model
/// `costUSD`. Returns the entries sorted by model id for deterministic rows.
fn claude_model_usage(model_usage: &Value) -> Result<Vec<TranscriptModelUsage>, String> {
    let Some(map) = model_usage.as_object() else {
        return Err("RESULT_MODEL_USAGE_NOT_OBJECT".to_owned());
    };
    let mut out = Vec::with_capacity(map.len());
    for (model, entry) in map {
        let field = |name: &str| -> u64 { entry.get(name).and_then(Value::as_u64).unwrap_or(0) };
        let cost_micro_usd = entry.get("costUSD").and_then(Value::as_f64).map(|cost| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let micro = (cost * 1_000_000.0).round().max(0.0) as u64;
            micro
        });
        out.push(TranscriptModelUsage {
            model: model.clone(),
            input_tokens: field("inputTokens"),
            output_tokens: field("outputTokens"),
            cache_read_input_tokens: field("cacheReadInputTokens"),
            cache_creation_input_tokens: field("cacheCreationInputTokens"),
            cost_micro_usd,
        });
    }
    out.sort_by(|a, b| a.model.cmp(&b.model));
    Ok(out)
}

/// Codex `exec --json` vocabulary, pinned to the event shapes captured from
/// real runs (see the fixture `tests/fixtures/codex_exec_real.jsonl`):
/// `thread.started`, `turn.started|completed|failed`,
/// `item.started|updated|completed`, `error`.
fn parse_codex_object(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
    cursor: &mut TranscriptCursor,
) -> Result<(), String> {
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "MISSING_TYPE: line has no string `type` field".to_owned())?;
    match event_type {
        "thread.started" => {
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some("thread.started".to_owned());
            if let Some(thread_id) = object.get("thread_id").and_then(Value::as_str) {
                cursor.conversation_id = Some(thread_id.to_owned());
            }
            Ok(())
        }
        "turn.started" => {
            cursor.turn_index += 1;
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some("turn.started".to_owned());
            Ok(())
        }
        "turn.completed" => {
            record.role = Some(TranscriptRole::Result);
            record.event_kind = Some("turn.completed".to_owned());
            let usage = object
                .get("usage")
                .ok_or_else(|| "TURN_COMPLETED_MISSING_USAGE".to_owned())?;
            record.usage = Some(TranscriptUsage {
                input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
                output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
                // Codex reports cache hits as `cached_input_tokens`.
                cache_read_input_tokens: usage.get("cached_input_tokens").and_then(Value::as_u64),
                cache_creation_input_tokens: None,
                // Codex (OpenAI) does not bill cache writes, so there is no
                // cache-creation tier split and no per-model breakdown.
                cache_creation_5m_input_tokens: None,
                cache_creation_1h_input_tokens: None,
                reasoning_output_tokens: usage
                    .get("reasoning_output_tokens")
                    .and_then(Value::as_u64),
                total_cost_micro_usd: None,
                model_usage: Vec::new(),
            });
            Ok(())
        }
        "turn.failed" => {
            record.role = Some(TranscriptRole::Result);
            record.event_kind = Some("turn.failed".to_owned());
            record.source_error = Some(
                object
                    .get("error")
                    .map(|error| {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .map_or_else(|| error.to_string(), ToOwned::to_owned)
                    })
                    .unwrap_or_else(|| "turn.failed without error detail".to_owned()),
            );
            Ok(())
        }
        "error" => {
            record.role = Some(TranscriptRole::System);
            record.event_kind = Some("error".to_owned());
            record.source_error = Some(object.get("message").and_then(Value::as_str).map_or_else(
                || Value::Object(object.clone()).to_string(),
                ToOwned::to_owned,
            ));
            Ok(())
        }
        "item.started" | "item.updated" | "item.completed" => {
            let item = object
                .get("item")
                .and_then(Value::as_object)
                .ok_or_else(|| format!("{event_type}: missing `item` object"))?;
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{event_type}: item has no string `type`"))?;
            record.event_kind = Some(format!("{event_type}/{item_type}"));
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            match item_type {
                "agent_message" => {
                    record.role = Some(TranscriptRole::Assistant);
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        set_content(record, text);
                    }
                    Ok(())
                }
                "reasoning" => {
                    record.role = Some(TranscriptRole::Assistant);
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        set_content(record, text);
                    }
                    Ok(())
                }
                "mcp_tool_call" => {
                    record.role = Some(TranscriptRole::Tool);
                    let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                    let tool = item
                        .get("tool")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "MCP_TOOL_CALL_MISSING_TOOL".to_owned())?;
                    let tool_name = if server.is_empty() {
                        tool.to_owned()
                    } else {
                        format!("{server}.{tool}")
                    };
                    let (arguments, arguments_bytes, arguments_truncated) = item
                        .get("arguments")
                        .map(|arguments| {
                            bounded_json_string(arguments, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS)
                        })
                        .unwrap_or_default();
                    let result =
                        item.get("result")
                            .filter(|value| !value.is_null())
                            .map(|result| {
                                bounded_json_string(result, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS)
                            });
                    record.tool_calls.push(TranscriptToolCall {
                        tool_name,
                        tool_call_id: item
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        arguments: Some(arguments),
                        arguments_bytes: Some(arguments_bytes),
                        arguments_truncated,
                        result_summary: result.as_ref().map(|(text, _, _)| text.clone()),
                        result_bytes: result.as_ref().map(|(_, bytes, _)| *bytes),
                        result_truncated: result
                            .as_ref()
                            .is_some_and(|(_, _, truncated)| *truncated),
                        status,
                        exit_code: None,
                    });
                    Ok(())
                }
                "command_execution" => {
                    record.role = Some(TranscriptRole::Tool);
                    let (arguments, arguments_bytes, arguments_truncated) = item
                        .get("command")
                        .map(|command| {
                            bounded_json_string(command, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS)
                        })
                        .unwrap_or_default();
                    let result = item
                        .get("aggregated_output")
                        .filter(|value| !value.is_null())
                        .map(|output| {
                            bounded_json_string(output, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS)
                        });
                    record.tool_calls.push(TranscriptToolCall {
                        tool_name: "command_execution".to_owned(),
                        tool_call_id: item
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        arguments: Some(arguments),
                        arguments_bytes: Some(arguments_bytes),
                        arguments_truncated,
                        result_summary: result.as_ref().map(|(text, _, _)| text.clone()),
                        result_bytes: result.as_ref().map(|(_, bytes, _)| *bytes),
                        result_truncated: result
                            .as_ref()
                            .is_some_and(|(_, _, truncated)| *truncated),
                        status,
                        exit_code: item.get("exit_code").and_then(Value::as_i64),
                    });
                    Ok(())
                }
                // Documented Codex item kinds we have not field-verified:
                // carried generically (full item JSON, bounded) rather than
                // refused, because they are part of the published vocabulary.
                "file_change" | "web_search" | "todo_list" => {
                    record.role = Some(TranscriptRole::Tool);
                    set_content(record, &Value::Object(item.clone()).to_string());
                    Ok(())
                }
                other => Err(format!("UNKNOWN_ITEM_TYPE: {other}")),
            }
        }
        other => Err(format!("UNKNOWN_EVENT_TYPE: {other}")),
    }
}

/// Codex app-server stdout is a JSON-RPC-style event stream, not Codex
/// `exec --json`. Notifications carry `method` + `params`; responses carry
/// `id` plus either `result` or `error`. The method namespace is intentionally
/// open, so unknown app-server methods are preserved as generic system rows
/// instead of treated as parser drift.
fn parse_codex_app_server_object(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
    cursor: &mut TranscriptCursor,
) -> Result<(), String> {
    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").unwrap_or(&Value::Null);
        stamp_codex_app_server_identity(params, cursor);
        record.event_kind = Some(format!("codex_app_server/{method}"));
        match method {
            "turn/started" => {
                cursor.turn_index += 1;
                record.role = Some(TranscriptRole::System);
                set_json_content(record, params);
            }
            "turn/completed" => {
                record.role = Some(TranscriptRole::Result);
                set_json_content(record, params);
                if let Some(error) = params
                    .get("turn")
                    .and_then(|turn| turn.get("error"))
                    .filter(|error| !error.is_null())
                {
                    record.source_error = Some(error.to_string());
                }
            }
            "thread/tokenUsage/updated" => {
                record.role = Some(TranscriptRole::Result);
                if let Some(usage) = params.get("tokenUsage") {
                    record.usage = Some(codex_app_server_usage(usage));
                }
                set_json_content(record, params);
            }
            "item/agentMessage/delta" => {
                record.role = Some(TranscriptRole::Assistant);
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    set_content(record, delta);
                } else {
                    set_json_content(record, params);
                }
            }
            "item/commandExecution/outputDelta" => {
                record.role = Some(TranscriptRole::Tool);
                let (result_summary, result_bytes, result_truncated) = params
                    .get("delta")
                    .map(|delta| bounded_json_string(delta, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS))
                    .unwrap_or_default();
                record.tool_calls.push(TranscriptToolCall {
                    tool_name: "command_execution".to_owned(),
                    tool_call_id: params
                        .get("itemId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    result_summary: Some(result_summary),
                    result_bytes: Some(result_bytes),
                    result_truncated,
                    ..TranscriptToolCall::default()
                });
            }
            "item/started" | "item/completed" => {
                parse_codex_app_server_item(method, params, record)?;
            }
            _ => {
                record.role = Some(TranscriptRole::System);
                set_json_content(record, params);
            }
        }
        return Ok(());
    }

    if !object.contains_key("id") {
        return Err(
            "CODEX_APP_SERVER_MESSAGE_MISSING_METHOD_OR_ID: expected notification or response"
                .to_owned(),
        );
    }
    if let Some(result) = object.get("result") {
        record.role = Some(TranscriptRole::System);
        record.event_kind = Some("codex_app_server/response/result".to_owned());
        set_json_content(record, result);
        if let Some(model) = result.get("model").and_then(Value::as_str) {
            cursor.model = Some(model.to_owned());
            record.model = Some(model.to_owned());
        }
        if let Some(thread_id) = result
            .get("thread")
            .and_then(|thread| thread.get("id").or_else(|| thread.get("sessionId")))
            .and_then(Value::as_str)
        {
            cursor.conversation_id = Some(thread_id.to_owned());
        }
        return Ok(());
    }
    if let Some(error) = object.get("error") {
        record.role = Some(TranscriptRole::Result);
        record.event_kind = Some("codex_app_server/response/error".to_owned());
        record.source_error = Some(error.to_string());
        set_json_content(record, error);
        return Ok(());
    }
    Err("CODEX_APP_SERVER_RESPONSE_MISSING_RESULT_OR_ERROR".to_owned())
}

fn parse_codex_app_server_item(
    method: &str,
    params: &Value,
    record: &mut AgentTranscriptRecord,
) -> Result<(), String> {
    let item = params
        .get("item")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{method}: missing `item` object"))?;
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method}: item has no string `type`"))?;
    record.event_kind = Some(format!("codex_app_server/{method}/{item_type}"));
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    match item_type {
        "agentMessage" | "reasoning" => {
            record.role = Some(TranscriptRole::Assistant);
            if let Some(text) = codex_app_server_item_text(item) {
                set_content(record, &text);
            } else {
                set_json_content(record, &Value::Object(item.clone()));
            }
        }
        "userMessage" => {
            record.role = Some(TranscriptRole::System);
            if let Some(text) = codex_app_server_item_text(item) {
                set_content(record, &text);
            } else {
                set_json_content(record, &Value::Object(item.clone()));
            }
        }
        "mcpToolCall" => {
            record.role = Some(TranscriptRole::Tool);
            let server = item.get("server").and_then(Value::as_str).unwrap_or("");
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .ok_or_else(|| "CODEX_APP_SERVER_MCP_TOOL_CALL_MISSING_TOOL".to_owned())?;
            let tool_name = if server.is_empty() {
                tool.to_owned()
            } else {
                format!("{server}.{tool}")
            };
            let (arguments, arguments_bytes, arguments_truncated) = item
                .get("arguments")
                .map(|arguments| {
                    bounded_json_string(arguments, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS)
                })
                .unwrap_or_default();
            let result = item
                .get("result")
                .filter(|value| !value.is_null())
                .map(|result| bounded_json_string(result, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS));
            record.tool_calls.push(TranscriptToolCall {
                tool_name,
                tool_call_id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                arguments: Some(arguments),
                arguments_bytes: Some(arguments_bytes),
                arguments_truncated,
                result_summary: result.as_ref().map(|(text, _, _)| text.clone()),
                result_bytes: result.as_ref().map(|(_, bytes, _)| *bytes),
                result_truncated: result.as_ref().is_some_and(|(_, _, truncated)| *truncated),
                status,
                exit_code: None,
            });
        }
        "commandExecution" => {
            record.role = Some(TranscriptRole::Tool);
            let (arguments, arguments_bytes, arguments_truncated) = item
                .get("command")
                .map(|command| bounded_json_string(command, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS))
                .unwrap_or_default();
            let result = item
                .get("aggregatedOutput")
                .or_else(|| item.get("aggregated_output"))
                .filter(|value| !value.is_null())
                .map(|output| bounded_json_string(output, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS));
            record.tool_calls.push(TranscriptToolCall {
                tool_name: "command_execution".to_owned(),
                tool_call_id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                arguments: Some(arguments),
                arguments_bytes: Some(arguments_bytes),
                arguments_truncated,
                result_summary: result.as_ref().map(|(text, _, _)| text.clone()),
                result_bytes: result.as_ref().map(|(_, bytes, _)| *bytes),
                result_truncated: result.as_ref().is_some_and(|(_, _, truncated)| *truncated),
                status,
                exit_code: item
                    .get("exitCode")
                    .or_else(|| item.get("exit_code"))
                    .and_then(Value::as_i64),
            });
        }
        _ => {
            record.role = Some(TranscriptRole::System);
            set_json_content(record, &Value::Object(item.clone()));
        }
    }
    Ok(())
}

fn codex_app_server_item_text(item: &Map<String, Value>) -> Option<String> {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let content = item.get("content")?.as_array()?;
    let parts: Vec<&str> = content
        .iter()
        .filter_map(|block| {
            block
                .as_object()
                .and_then(|object| object.get("text"))
                .and_then(Value::as_str)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn codex_app_server_usage(usage: &Value) -> TranscriptUsage {
    let total = usage.get("total").unwrap_or(usage);
    TranscriptUsage {
        input_tokens: u64_field(total, &["inputTokens", "input_tokens"]),
        output_tokens: u64_field(total, &["outputTokens", "output_tokens"]),
        cache_read_input_tokens: u64_field(total, &["cachedInputTokens", "cached_input_tokens"]),
        cache_creation_input_tokens: None,
        cache_creation_5m_input_tokens: None,
        cache_creation_1h_input_tokens: None,
        reasoning_output_tokens: u64_field(
            total,
            &["reasoningOutputTokens", "reasoning_output_tokens"],
        ),
        total_cost_micro_usd: None,
        model_usage: Vec::new(),
    }
}

fn u64_field(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
}

fn stamp_codex_app_server_identity(params: &Value, cursor: &mut TranscriptCursor) {
    if let Some(thread_id) = params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("thread")
                .and_then(|thread| thread.get("id").or_else(|| thread.get("sessionId")))
                .and_then(Value::as_str)
        })
    {
        cursor.conversation_id = Some(thread_id.to_owned());
    }
    if let Some(model) = params.get("model").and_then(Value::as_str).or_else(|| {
        params
            .get("thread")
            .and_then(|thread| thread.get("model"))
            .and_then(Value::as_str)
    }) {
        cursor.model = Some(model.to_owned());
    }
}

fn set_json_content(record: &mut AgentTranscriptRecord, value: &Value) {
    set_content(record, &value.to_string());
}

fn parse_local_model_object(
    object: &Map<String, Value>,
    record: &mut AgentTranscriptRecord,
    cursor: &mut TranscriptCursor,
) -> Result<(), String> {
    let event_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "MISSING_TYPE: line has no string `type` field".to_owned())?;
    record.event_kind = Some(event_type.to_owned());
    if let Some(conversation_id) = object.get("conversation_id").and_then(Value::as_str) {
        cursor.conversation_id = Some(conversation_id.to_owned());
    }
    if let Some(model) = object.get("model").and_then(Value::as_str) {
        cursor.model = Some(model.to_owned());
        record.model = Some(model.to_owned());
    }
    if let Some(turn) = object.get("turn_index").and_then(Value::as_u64) {
        cursor.turn_index = turn;
        record.turn_index = Some(turn);
    }
    match event_type {
        "local.thread.started" => {
            record.role = Some(TranscriptRole::System);
            Ok(())
        }
        "local.turn.started" => {
            record.role = Some(TranscriptRole::System);
            Ok(())
        }
        "local.assistant.message" => {
            record.role = Some(TranscriptRole::Assistant);
            if let Some(content) = object.get("content").and_then(Value::as_str) {
                set_content(record, content);
            }
            Ok(())
        }
        "local.turn.finished" => {
            record.role = Some(TranscriptRole::Result);
            let usage = object
                .get("usage")
                .ok_or_else(|| "LOCAL_TURN_FINISHED_MISSING_USAGE".to_owned())?;
            record.usage = Some(TranscriptUsage {
                input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
                output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cache_creation_5m_input_tokens: None,
                cache_creation_1h_input_tokens: None,
                reasoning_output_tokens: None,
                total_cost_micro_usd: None,
                model_usage: Vec::new(),
            });
            Ok(())
        }
        "local.tool_call.started" => {
            record.role = Some(TranscriptRole::Tool);
            let tool_name = required_local_str(object, "tool_name")?.to_owned();
            let (arguments, arguments_bytes, arguments_truncated) = object
                .get("arguments")
                .map(|arguments| {
                    bounded_json_string(arguments, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS)
                })
                .unwrap_or_default();
            record.tool_calls.push(TranscriptToolCall {
                tool_name,
                tool_call_id: object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                arguments: Some(arguments),
                arguments_bytes: Some(arguments_bytes),
                arguments_truncated,
                status: Some("started".to_owned()),
                ..TranscriptToolCall::default()
            });
            Ok(())
        }
        "local.tool_call.finished" => {
            record.role = Some(TranscriptRole::Tool);
            let tool_name = required_local_str(object, "tool_name")?.to_owned();
            let result = object
                .get("result")
                .map(|value| bounded_json_string(value, AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS));
            let status = object
                .get("status")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            record.tool_calls.push(TranscriptToolCall {
                tool_name,
                tool_call_id: object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                result_summary: result.as_ref().map(|(text, _, _)| text.clone()),
                result_bytes: result.as_ref().map(|(_, bytes, _)| *bytes),
                result_truncated: result.as_ref().is_some_and(|(_, _, truncated)| *truncated),
                status,
                ..TranscriptToolCall::default()
            });
            Ok(())
        }
        "local.tool_call.gate_bypassed" => {
            // A local autonomous agent recorded that a permission-gated tool call
            // proceeded without an interactive approval gate (e.g. trusted
            // unattended exact-contract authorization). This is an expected
            // local-model lifecycle event, not schema drift — give it a typed
            // path so it parses cleanly instead of landing as an invalid row
            // (#1327). The bypass reason is preserved on the tool call status.
            record.role = Some(TranscriptRole::Tool);
            let tool_name = required_local_str(object, "tool_name")?.to_owned();
            let reason_code = object
                .get("reason_code")
                .and_then(Value::as_str)
                .unwrap_or("gate_bypassed");
            record.tool_calls.push(TranscriptToolCall {
                tool_name,
                tool_call_id: object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                status: Some(format!("gate_bypassed:{reason_code}")),
                ..TranscriptToolCall::default()
            });
            Ok(())
        }
        "local.tool_call.arguments_normalized" => {
            record.role = Some(TranscriptRole::Tool);
            let tool_name = required_local_str(object, "tool_name")?.to_owned();
            let reason_code = required_local_str(object, "reason_code")?;
            let normalized_arguments = required_local_normalized_arguments(object)?;
            let (arguments, arguments_bytes, arguments_truncated) =
                bounded_json_string(normalized_arguments, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS);
            record.tool_calls.push(TranscriptToolCall {
                tool_name,
                tool_call_id: object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                arguments: Some(arguments),
                arguments_bytes: Some(arguments_bytes),
                arguments_truncated,
                status: Some(format!("arguments_normalized:{reason_code}")),
                ..TranscriptToolCall::default()
            });
            Ok(())
        }
        "local.tool_parse_error" => {
            record.role = Some(TranscriptRole::Tool);
            record.source_error = Some(
                object
                    .get("error_detail")
                    .and_then(Value::as_str)
                    .unwrap_or("MODEL_TOOL_ARGUMENTS_INVALID")
                    .to_owned(),
            );
            let tool_name = object
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            record.tool_calls.push(TranscriptToolCall {
                tool_name,
                tool_call_id: object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                status: Some("error".to_owned()),
                ..TranscriptToolCall::default()
            });
            Ok(())
        }
        "local.context.truncated" => {
            record.role = Some(TranscriptRole::System);
            set_content(record, &Value::Object(object.clone()).to_string());
            Ok(())
        }
        "local.steering.received" => {
            record.role = Some(TranscriptRole::System);
            let _message_id = required_local_str(object, "message_id")?;
            let _kind = required_local_str(object, "kind")?;
            if let Some(payload_summary) = object.get("payload_summary").and_then(Value::as_str) {
                set_content(record, payload_summary);
            } else {
                set_content(record, &Value::Object(object.clone()).to_string());
            }
            Ok(())
        }
        "local.hold_open.started" | "local.hold_open.finished" => {
            record.role = Some(TranscriptRole::System);
            let _session_id = required_local_str(object, "session_id")?;
            let _hold_open_ms = required_local_u64(object, "hold_open_ms")?;
            let _started_at_unix_ms = required_local_u64(object, "started_at_unix_ms")?;
            if event_type == "local.hold_open.finished" {
                let _finished_at_unix_ms = required_local_u64(object, "finished_at_unix_ms")?;
            }
            set_content(record, &Value::Object(object.clone()).to_string());
            Ok(())
        }
        "local.agent.completed" => {
            record.role = Some(TranscriptRole::Result);
            let final_message = required_local_str(object, "final_message")?;
            set_content(record, final_message);
            Ok(())
        }
        "local.error" => {
            record.role = Some(TranscriptRole::Result);
            record.source_error = Some(
                object
                    .get("error_detail")
                    .and_then(Value::as_str)
                    .or_else(|| object.get("error_code").and_then(Value::as_str))
                    .unwrap_or("local model runner error")
                    .to_owned(),
            );
            Ok(())
        }
        other => Err(format!("UNKNOWN_EVENT_TYPE: {other}")),
    }
}

fn required_local_normalized_arguments(object: &Map<String, Value>) -> Result<&Value, String> {
    const FIELDS: [&str; 3] = [
        "contract_arguments",
        "attributed_arguments",
        "model_arguments",
    ];
    for field in FIELDS {
        if let Some(value) = object.get(field) {
            if value.as_object().is_some() {
                return Ok(value);
            }
            return Err(format!(
                "required object field {field:?} is present but not an object"
            ));
        }
    }
    Err(format!(
        "one of required object fields {FIELDS:?} is missing"
    ))
}

fn required_local_str<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("required string field {field:?} is missing or empty"))
}

fn required_local_u64(object: &Map<String, Value>, field: &str) -> Result<u64, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("required u64 field {field:?} is missing or invalid"))
}

#[cfg(test)]
mod tests;
