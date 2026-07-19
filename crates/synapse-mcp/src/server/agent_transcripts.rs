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
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synapse_core::{
    AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS, AGENT_TRANSCRIPT_MAX_TOOL_ARGS_CHARS,
    AGENT_TRANSCRIPT_MAX_TOOL_RESULT_CHARS, AgentTranscriptRecord, TranscriptModelUsage,
    TranscriptParseStatus, TranscriptRole, TranscriptSource, TranscriptToolCall, TranscriptUsage,
};
use synapse_storage::{
    CfRevisionGuard, Db,
    agent_transcripts::{agent_transcript_key, agent_transcript_ts_index_key},
    cf, decode_json, encode_json,
};
use tokio_util::sync::CancellationToken;

use crate::m3::{M3State, default_daemon_db_path, default_db_path};

use super::agent_events::anchor_spawn_end_state_from_storage;

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
const NS_PER_MS: u64 = 1_000_000;
const TIMESTAMP_LINE_OFFSET_NS: u64 = NS_PER_MS;
const STABLE_TS_BASE_MS: u64 = 1_600_000_000_000;
const STABLE_TS_SPAN_MS: u64 = 20 * 365 * 24 * 60 * 60 * 1_000;

/// Hard cap on one encoded transcript row. The per-field bounds keep real
/// rows far below this; exceeding it means an ingester bug, surfaced as a
/// sticky error for the spawn.
pub(crate) const MAX_AGENT_TRANSCRIPT_VALUE_BYTES: usize = 32 * 1024;

/// Maximum bytes in one logical JSONL record, excluding CR/LF terminators.
/// A missing delimiter can therefore never grow the ingester's memory without
/// bound. Ten MiB matches the established production log-harvester ceiling
/// while leaving ample room for CLI tool-result envelopes that normalize into
/// the much smaller durable row above.
pub(crate) const MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES: usize = 10 * 1024 * 1024;

/// The source/index mutation for one chunk is at most 64 * 32 KiB of durable
/// values plus small deterministic keys and retention envelopes. This remains
/// a small fraction of Calyx's 64 MiB WAL-record ceiling; the storage layer
/// independently rejects the mutation if that physical invariant ever drifts.
pub(crate) const MAX_AGENT_TRANSCRIPT_COMMIT_ROWS: usize = 64;
const MAX_AGENT_TRANSCRIPT_ROWS_PER_PASS: usize = 4 * MAX_AGENT_TRANSCRIPT_COMMIT_ROWS;
const MAX_AGENT_TRANSCRIPT_SOURCE_BYTES_PER_PASS: u64 = 32 * 1024 * 1024;
const AGENT_TRANSCRIPT_READ_BUFFER_BYTES: usize = 64 * 1024;
const AGENT_TRANSCRIPT_SOURCE_FINGERPRINT_BYTES: u64 = 64 * 1024;
const AGENT_TRANSCRIPT_SOURCE_BOUNDARY_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub(super) struct PreparedTranscriptRow {
    pub source_key: Vec<u8>,
    pub encoded: Vec<u8>,
    pub ts_index_key: Vec<u8>,
    pub record: AgentTranscriptRecord,
    pub source_offset_bytes: u64,
    pub consumed_bytes: u64,
}

#[derive(Debug)]
pub(super) struct TranscriptSourceFingerprint {
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub(super) struct TranscriptSourceBoundary {
    pub start_offset_bytes: u64,
    pub bytes: u64,
    pub sha256: String,
}

pub(super) struct TranscriptSourceBoundaryState<'a> {
    pub cursor_offset_bytes: u64,
    pub lines_ingested: u64,
    pub start_offset_bytes: &'a mut Option<u64>,
    pub bytes: &'a mut Option<u64>,
    pub sha256: &'a mut Option<String>,
}

#[derive(Debug)]
pub(super) enum BoundedTailRead {
    Line {
        bytes: Vec<u8>,
        consumed_bytes: u64,
        source_offset_bytes: u64,
    },
    SnapshotEof,
    IncompleteTail {
        source_offset_bytes: u64,
        buffered_bytes: usize,
    },
    Cancelled,
}

/// A reader bounded to the file length observed before the pass. Appends after
/// that snapshot are deliberately left for the next pass; truncation during a
/// read is surfaced instead of being mistaken for a clean EOF.
pub(super) struct BoundedTranscriptTailReader {
    reader: BufReader<std::io::Take<std::fs::File>>,
    source_path: PathBuf,
    next_offset_bytes: u64,
    code_prefix: &'static str,
}

impl BoundedTranscriptTailReader {
    pub(super) fn open(
        source_path: &Path,
        offset_bytes: u64,
        snapshot_size_bytes: u64,
        code_prefix: &'static str,
    ) -> Result<Self, String> {
        let remaining = snapshot_size_bytes.checked_sub(offset_bytes).ok_or_else(|| {
            format!(
                "{code_prefix}_SOURCE_TRUNCATED: path={} offset_bytes={offset_bytes} snapshot_size_bytes={snapshot_size_bytes}; remediation=restore the original append-only source or clear the cursor only after reconciling its durable transcript rows",
                source_path.display()
            )
        })?;
        let mut file = std::fs::File::open(source_path).map_err(|error| {
            format!(
                "{code_prefix}_SOURCE_OPEN_FAILED: path={} offset_bytes={offset_bytes}: {error}; remediation=restore read access to the exact source file and retry without changing the cursor",
                source_path.display()
            )
        })?;
        file.seek(SeekFrom::Start(offset_bytes)).map_err(|error| {
            format!(
                "{code_prefix}_SOURCE_SEEK_FAILED: path={} offset_bytes={offset_bytes}: {error}; remediation=repair the source filesystem/file handle and retry from the persisted cursor",
                source_path.display()
            )
        })?;
        Ok(Self {
            reader: BufReader::with_capacity(
                AGENT_TRANSCRIPT_READ_BUFFER_BYTES,
                file.take(remaining),
            ),
            source_path: source_path.to_path_buf(),
            next_offset_bytes: offset_bytes,
            code_prefix,
        })
    }

    pub(super) fn next_line(
        &mut self,
        finalize: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<BoundedTailRead, String> {
        let source_offset_bytes = self.next_offset_bytes;
        let mut line = Vec::with_capacity(AGENT_TRANSCRIPT_READ_BUFFER_BYTES);
        loop {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Ok(BoundedTailRead::Cancelled);
            }

            let available = self.reader.fill_buf().map_err(|error| {
                format!(
                    "{}_SOURCE_READ_FAILED: path={} offset_bytes={}: {error}; remediation=repair source read access and retry from the persisted cursor",
                    self.code_prefix,
                    self.source_path.display(),
                    self.next_offset_bytes
                )
            })?;
            if available.is_empty() {
                if self.reader.get_ref().limit() != 0 {
                    return Err(format!(
                        "{}_SOURCE_CHANGED_DURING_READ: path={} line_start_offset_bytes={source_offset_bytes} unread_snapshot_bytes={}; remediation=stop replacing/truncating the append-only source and restore bytes matching the persisted cursor",
                        self.code_prefix,
                        self.source_path.display(),
                        self.reader.get_ref().limit()
                    ));
                }
                if line.is_empty() {
                    return Ok(BoundedTailRead::SnapshotEof);
                }
                if !finalize {
                    return Ok(BoundedTailRead::IncompleteTail {
                        source_offset_bytes,
                        buffered_bytes: line.len(),
                    });
                }
                if line.len() > MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES {
                    return Err(self.oversized_detail(source_offset_bytes, line.len()));
                }
                return Ok(BoundedTailRead::Line {
                    consumed_bytes: u64::try_from(line.len()).unwrap_or(u64::MAX),
                    source_offset_bytes,
                    bytes: line,
                });
            }

            let newline_at = available.iter().position(|byte| *byte == b'\n');
            let take = newline_at.map_or(available.len(), |index| index + 1);
            line.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            self.next_offset_bytes = self
                .next_offset_bytes
                .checked_add(u64::try_from(take).unwrap_or(u64::MAX))
                .ok_or_else(|| {
                    format!(
                        "{}_SOURCE_OFFSET_OVERFLOW: path={} line_start_offset_bytes={source_offset_bytes}; remediation=quarantine the impossible-size source and reconcile the cursor",
                        self.code_prefix,
                        self.source_path.display()
                    )
                })?;

            if newline_at.is_some() {
                let consumed_bytes = u64::try_from(line.len()).unwrap_or(u64::MAX);
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.len() > MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES {
                    return Err(self.oversized_detail(source_offset_bytes, line.len()));
                }
                return Ok(BoundedTailRead::Line {
                    bytes: line,
                    consumed_bytes,
                    source_offset_bytes,
                });
            }

            let over_limit = line.len() > MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES
                && !(line.len() == MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES + 1
                    && line.last() == Some(&b'\r'));
            if over_limit {
                return Err(self.oversized_detail(source_offset_bytes, line.len()));
            }
        }
    }

    fn oversized_detail(&self, source_offset_bytes: u64, observed_bytes: usize) -> String {
        format!(
            "{}_SOURCE_RECORD_OVERSIZED: path={} line_start_offset_bytes={source_offset_bytes} observed_bytes={observed_bytes} max_line_bytes={MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES}; remediation=fix or rotate the producer output without skipping/truncating this record, then clear the sticky cursor only after reconciling the source bytes",
            self.code_prefix,
            self.source_path.display()
        )
    }
}

pub(super) fn read_transcript_source_fingerprint(
    source_path: &Path,
    snapshot_size_bytes: u64,
    expected_bytes: Option<u64>,
    code_prefix: &'static str,
) -> Result<TranscriptSourceFingerprint, String> {
    let bytes = expected_bytes
        .unwrap_or_else(|| snapshot_size_bytes.min(AGENT_TRANSCRIPT_SOURCE_FINGERPRINT_BYTES));
    if bytes == 0 || bytes > AGENT_TRANSCRIPT_SOURCE_FINGERPRINT_BYTES {
        return Err(format!(
            "{code_prefix}_SOURCE_FINGERPRINT_LENGTH_INVALID: path={} fingerprint_bytes={bytes} max_fingerprint_bytes={AGENT_TRANSCRIPT_SOURCE_FINGERPRINT_BYTES}; remediation=repair the corrupt cursor fingerprint from the source/transcript SoTs",
            source_path.display()
        ));
    }
    if snapshot_size_bytes < bytes {
        return Err(format!(
            "{code_prefix}_SOURCE_FINGERPRINT_TRUNCATED: path={} snapshot_size_bytes={snapshot_size_bytes} fingerprint_bytes={bytes}; remediation=restore the original append-only source bytes before clearing the cursor",
            source_path.display()
        ));
    }
    let buffer_len = usize::try_from(bytes).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_FINGERPRINT_LENGTH_INVALID: path={} fingerprint_bytes={bytes}: {error}; remediation=repair the corrupt cursor fingerprint",
            source_path.display()
        )
    })?;
    let mut file = std::fs::File::open(source_path).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_FINGERPRINT_OPEN_FAILED: path={}: {error}; remediation=restore read access to the exact append-only source",
            source_path.display()
        )
    })?;
    let mut buffer = vec![0_u8; buffer_len];
    file.read_exact(&mut buffer).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_FINGERPRINT_READ_FAILED: path={} fingerprint_bytes={bytes}: {error}; remediation=restore the original append-only source bytes/read access",
            source_path.display()
        )
    })?;
    Ok(TranscriptSourceFingerprint {
        bytes,
        sha256: sha256_hex(&buffer),
    })
}

pub(super) fn ensure_transcript_source_fingerprint(
    db: &Db,
    source_id: &str,
    source_path: &Path,
    snapshot_size_bytes: u64,
    cursor_offset_bytes: u64,
    fingerprint_bytes: &mut Option<u64>,
    fingerprint_sha256: &mut Option<String>,
    code_prefix: &'static str,
) -> Result<bool, String> {
    match (*fingerprint_bytes, fingerprint_sha256.as_deref()) {
        (Some(bytes), Some(expected_sha256)) => {
            if !is_lower_sha256(expected_sha256) {
                return Err(format!(
                    "{code_prefix}_SOURCE_FINGERPRINT_INVALID: source_id={source_id} path={} fingerprint_bytes={bytes} fingerprint_sha256={expected_sha256:?}; remediation=repair the corrupt cursor fingerprint from the source/transcript SoTs",
                    source_path.display()
                ));
            }
            let actual = read_transcript_source_fingerprint(
                source_path,
                snapshot_size_bytes,
                Some(bytes),
                code_prefix,
            )?;
            if actual.sha256 != expected_sha256 {
                return Err(format!(
                    "{code_prefix}_SOURCE_IDENTITY_MISMATCH: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} fingerprint_bytes={bytes} expected_sha256={expected_sha256} actual_sha256={}; remediation=restore the original append-only source or reconcile every durable transcript row before rebuilding the cursor",
                    source_path.display(),
                    actual.sha256
                ));
            }
            Ok(false)
        }
        (None, None) if cursor_offset_bytes == 0 => Ok(false),
        (None, None) => {
            // Legacy cursors predate the persisted fingerprint. Establish one
            // only after the first physical source line exactly matches the
            // deterministic row already covered by that cursor.
            let mut reader = BoundedTranscriptTailReader::open(
                source_path,
                0,
                snapshot_size_bytes,
                code_prefix,
            )?;
            let first_line = match reader.next_line(true, None)? {
                BoundedTailRead::Line { bytes, .. } => bytes,
                BoundedTailRead::SnapshotEof | BoundedTailRead::IncompleteTail { .. } => {
                    return Err(format!(
                        "{code_prefix}_SOURCE_FINGERPRINT_MIGRATION_FAILED: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} source has no first record; remediation=restore the original source before migrating its cursor",
                        source_path.display()
                    ));
                }
                BoundedTailRead::Cancelled => {
                    return Err(format!(
                        "{code_prefix}_SOURCE_FINGERPRINT_MIGRATION_CANCELLED: source_id={source_id} path={}; remediation=retry migration from the unchanged cursor",
                        source_path.display()
                    ));
                }
            };
            let first_key = agent_transcript_key(source_id, 1);
            let first_row = db
                .get_cf(cf::CF_AGENT_TRANSCRIPTS, &first_key)
                .map_err(|error| {
                    format!(
                        "{code_prefix}_SOURCE_FINGERPRINT_ROW_READ_FAILED: source_id={source_id} path={} key_hex={}: {error}; remediation=repair the first transcript row before migrating the cursor",
                        source_path.display(),
                        synapse_storage::constellations::hex_encode(&first_key)
                    )
                })?
                .ok_or_else(|| {
                    format!(
                        "{code_prefix}_SOURCE_FINGERPRINT_ROW_MISSING: source_id={source_id} path={} key_hex={}; remediation=restore the first deterministic transcript row before migrating the cursor",
                        source_path.display(),
                        synapse_storage::constellations::hex_encode(&first_key)
                    )
                })?;
            let first_record: AgentTranscriptRecord = decode_json(&first_row).map_err(|error| {
                format!(
                    "{code_prefix}_SOURCE_FINGERPRINT_ROW_INVALID: source_id={source_id} path={} key_hex={}: {error}; remediation=repair the first transcript row before migrating the cursor",
                    source_path.display(),
                    synapse_storage::constellations::hex_encode(&first_key)
                )
            })?;
            let first_sha256 = sha256_hex(&first_line);
            if first_record.spawn_id != source_id
                || first_record.line_no != 1
                || first_record.raw_line_bytes
                    != u64::try_from(first_line.len()).unwrap_or(u64::MAX)
                || first_record.raw_line_sha256 != first_sha256
            {
                return Err(format!(
                    "{code_prefix}_SOURCE_FINGERPRINT_ROW_MISMATCH: source_id={source_id} path={} expected_first_line_sha256={} stored_first_line_sha256={} expected_first_line_bytes={} stored_first_line_bytes={}; remediation=restore the original append-only source or reconcile every durable transcript row before rebuilding the cursor",
                    source_path.display(),
                    first_sha256,
                    first_record.raw_line_sha256,
                    first_line.len(),
                    first_record.raw_line_bytes
                ));
            }
            let fingerprint = read_transcript_source_fingerprint(
                source_path,
                snapshot_size_bytes,
                None,
                code_prefix,
            )?;
            *fingerprint_bytes = Some(fingerprint.bytes);
            *fingerprint_sha256 = Some(fingerprint.sha256);
            Ok(true)
        }
        _ => Err(format!(
            "{code_prefix}_SOURCE_FINGERPRINT_PARTIAL: source_id={source_id} path={} fingerprint_bytes={fingerprint_bytes:?} fingerprint_sha256_present={}; remediation=repair the corrupt cursor fingerprint pair from the source/transcript SoTs",
            source_path.display(),
            fingerprint_sha256.is_some()
        )),
    }
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn read_transcript_source_range(
    source_path: &Path,
    snapshot_size_bytes: u64,
    start_offset_bytes: u64,
    bytes: u64,
    code_prefix: &'static str,
    purpose: &'static str,
) -> Result<Vec<u8>, String> {
    if bytes == 0 {
        return Err(format!(
            "{code_prefix}_SOURCE_{purpose}_LENGTH_INVALID: path={} start_offset_bytes={start_offset_bytes} bytes=0; remediation=repair the corrupt cursor source-boundary state",
            source_path.display()
        ));
    }
    let end_offset_bytes = start_offset_bytes.checked_add(bytes).ok_or_else(|| {
        format!(
            "{code_prefix}_SOURCE_{purpose}_OFFSET_OVERFLOW: path={} start_offset_bytes={start_offset_bytes} bytes={bytes}; remediation=repair the corrupt cursor source-boundary state",
            source_path.display()
        )
    })?;
    if end_offset_bytes > snapshot_size_bytes {
        return Err(format!(
            "{code_prefix}_SOURCE_{purpose}_TRUNCATED: path={} snapshot_size_bytes={snapshot_size_bytes} start_offset_bytes={start_offset_bytes} bytes={bytes}; remediation=restore the original append-only source bytes before clearing the cursor",
            source_path.display()
        ));
    }
    let buffer_len = usize::try_from(bytes).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_{purpose}_LENGTH_INVALID: path={} bytes={bytes}: {error}; remediation=repair the corrupt cursor source-boundary state",
            source_path.display()
        )
    })?;
    let mut file = std::fs::File::open(source_path).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_{purpose}_OPEN_FAILED: path={} start_offset_bytes={start_offset_bytes}: {error}; remediation=restore read access to the exact append-only source",
            source_path.display()
        )
    })?;
    file.seek(SeekFrom::Start(start_offset_bytes))
        .map_err(|error| {
            format!(
                "{code_prefix}_SOURCE_{purpose}_SEEK_FAILED: path={} start_offset_bytes={start_offset_bytes}: {error}; remediation=repair the source filesystem/file handle and retry from the persisted cursor",
                source_path.display()
            )
        })?;
    let mut buffer = vec![0_u8; buffer_len];
    file.read_exact(&mut buffer).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_{purpose}_READ_FAILED: path={} start_offset_bytes={start_offset_bytes} bytes={bytes}: {error}; remediation=restore the original append-only source bytes/read access",
            source_path.display()
        )
    })?;
    Ok(buffer)
}

fn read_transcript_source_boundary(
    source_path: &Path,
    snapshot_size_bytes: u64,
    cursor_offset_bytes: u64,
    code_prefix: &'static str,
) -> Result<TranscriptSourceBoundary, String> {
    if cursor_offset_bytes == 0 {
        return Err(format!(
            "{code_prefix}_SOURCE_BOUNDARY_OFFSET_INVALID: path={} cursor_offset_bytes=0; remediation=repair the corrupt non-empty cursor boundary state",
            source_path.display()
        ));
    }
    let bytes = cursor_offset_bytes.min(AGENT_TRANSCRIPT_SOURCE_BOUNDARY_BYTES);
    let start_offset_bytes = cursor_offset_bytes.checked_sub(bytes).ok_or_else(|| {
        format!(
            "{code_prefix}_SOURCE_BOUNDARY_OFFSET_OVERFLOW: path={} cursor_offset_bytes={cursor_offset_bytes} bytes={bytes}; remediation=repair the corrupt cursor boundary state",
            source_path.display()
        )
    })?;
    let buffer = read_transcript_source_range(
        source_path,
        snapshot_size_bytes,
        start_offset_bytes,
        bytes,
        code_prefix,
        "BOUNDARY",
    )?;
    Ok(TranscriptSourceBoundary {
        start_offset_bytes,
        bytes,
        sha256: sha256_hex(&buffer),
    })
}

fn last_logical_line_before_cursor(
    source_path: &Path,
    snapshot_size_bytes: u64,
    cursor_offset_bytes: u64,
    code_prefix: &'static str,
) -> Result<Vec<u8>, String> {
    let max_window_bytes = u64::try_from(MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(2);
    let bytes = cursor_offset_bytes.min(max_window_bytes);
    let start_offset_bytes = cursor_offset_bytes.checked_sub(bytes).ok_or_else(|| {
        format!(
            "{code_prefix}_SOURCE_BOUNDARY_OFFSET_OVERFLOW: path={} cursor_offset_bytes={cursor_offset_bytes} bytes={bytes}; remediation=repair the corrupt legacy cursor",
            source_path.display()
        )
    })?;
    let buffer = read_transcript_source_range(
        source_path,
        snapshot_size_bytes,
        start_offset_bytes,
        bytes,
        code_prefix,
        "BOUNDARY_MIGRATION",
    )?;
    let mut end = buffer.len();
    if buffer.get(end.wrapping_sub(1)) == Some(&b'\n') {
        end -= 1;
        if buffer.get(end.wrapping_sub(1)) == Some(&b'\r') {
            end -= 1;
        }
    }
    let start = buffer[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let line = buffer[start..end].to_vec();
    if line.len() > MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES {
        return Err(format!(
            "{code_prefix}_SOURCE_BOUNDARY_MIGRATION_RECORD_OVERSIZED: path={} cursor_offset_bytes={cursor_offset_bytes} observed_bytes={} max_line_bytes={MAX_AGENT_TRANSCRIPT_SOURCE_LINE_BYTES}; remediation=restore the original bounded source record before migrating the cursor",
            source_path.display(),
            line.len()
        ));
    }
    Ok(line)
}

fn verify_legacy_boundary_row(
    db: &Db,
    source_id: &str,
    source_path: &Path,
    snapshot_size_bytes: u64,
    cursor_offset_bytes: u64,
    lines_ingested: u64,
    code_prefix: &'static str,
) -> Result<(), String> {
    if lines_ingested == 0 {
        return Err(format!(
            "{code_prefix}_SOURCE_BOUNDARY_MIGRATION_COUNTER_INVALID: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} lines_ingested=0; remediation=repair the corrupt cursor from the physical source/transcript SoTs",
            source_path.display()
        ));
    }
    let line = last_logical_line_before_cursor(
        source_path,
        snapshot_size_bytes,
        cursor_offset_bytes,
        code_prefix,
    )?;
    let key = agent_transcript_key(source_id, lines_ingested);
    let encoded = db
        .get_cf(cf::CF_AGENT_TRANSCRIPTS, &key)
        .map_err(|error| {
            format!(
                "{code_prefix}_SOURCE_BOUNDARY_MIGRATION_ROW_READ_FAILED: source_id={source_id} path={} line_no={lines_ingested} key_hex={}: {error}; remediation=repair the last cursor-covered transcript row before migrating the boundary",
                source_path.display(),
                synapse_storage::constellations::hex_encode(&key)
            )
        })?
        .ok_or_else(|| {
            format!(
                "{code_prefix}_SOURCE_BOUNDARY_MIGRATION_ROW_MISSING: source_id={source_id} path={} line_no={lines_ingested} key_hex={}; remediation=restore the last cursor-covered transcript row before migrating the boundary",
                source_path.display(),
                synapse_storage::constellations::hex_encode(&key)
            )
        })?;
    let record: AgentTranscriptRecord = decode_json(&encoded).map_err(|error| {
        format!(
            "{code_prefix}_SOURCE_BOUNDARY_MIGRATION_ROW_INVALID: source_id={source_id} path={} line_no={lines_ingested} key_hex={}: {error}; remediation=repair the corrupt transcript row before migrating the boundary",
            source_path.display(),
            synapse_storage::constellations::hex_encode(&key)
        )
    })?;
    let line_sha256 = sha256_hex(&line);
    if record.spawn_id != source_id
        || record.line_no != lines_ingested
        || record.raw_line_bytes != u64::try_from(line.len()).unwrap_or(u64::MAX)
        || record.raw_line_sha256 != line_sha256
    {
        return Err(format!(
            "{code_prefix}_SOURCE_BOUNDARY_MIGRATION_ROW_MISMATCH: source_id={source_id} path={} line_no={lines_ingested} cursor_offset_bytes={cursor_offset_bytes} expected_line_sha256={line_sha256} stored_line_sha256={} expected_line_bytes={} stored_line_bytes={}; remediation=restore the original append-only source or reconcile every durable transcript row before rebuilding the cursor",
            source_path.display(),
            record.raw_line_sha256,
            line.len(),
            record.raw_line_bytes
        ));
    }
    Ok(())
}

pub(super) fn ensure_transcript_source_boundary(
    db: &Db,
    source_id: &str,
    source_path: &Path,
    snapshot_size_bytes: u64,
    state: TranscriptSourceBoundaryState<'_>,
    code_prefix: &'static str,
) -> Result<bool, String> {
    let TranscriptSourceBoundaryState {
        cursor_offset_bytes,
        lines_ingested,
        start_offset_bytes: boundary_start_offset_bytes,
        bytes: boundary_bytes,
        sha256: boundary_sha256,
    } = state;
    if cursor_offset_bytes == 0 {
        if lines_ingested != 0
            || boundary_start_offset_bytes.is_some()
            || boundary_bytes.is_some()
            || boundary_sha256.is_some()
        {
            return Err(format!(
                "{code_prefix}_SOURCE_BOUNDARY_ZERO_OFFSET_INVALID: source_id={source_id} path={} lines_ingested={lines_ingested} boundary_start_offset_bytes={boundary_start_offset_bytes:?} boundary_bytes={boundary_bytes:?} boundary_sha256_present={}; remediation=repair the corrupt cursor from the physical source/transcript SoTs",
                source_path.display(),
                boundary_sha256.is_some()
            ));
        }
        return Ok(false);
    }
    if lines_ingested == 0 {
        return Err(format!(
            "{code_prefix}_SOURCE_BOUNDARY_COUNTER_INVALID: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} lines_ingested=0; remediation=repair the corrupt cursor from the physical source/transcript SoTs",
            source_path.display()
        ));
    }

    match (
        *boundary_start_offset_bytes,
        *boundary_bytes,
        boundary_sha256.as_deref(),
    ) {
        (Some(stored_start), Some(stored_bytes), Some(stored_sha256)) => {
            let actual = read_transcript_source_boundary(
                source_path,
                snapshot_size_bytes,
                cursor_offset_bytes,
                code_prefix,
            )?;
            if stored_start != actual.start_offset_bytes
                || stored_bytes != actual.bytes
                || !is_lower_sha256(stored_sha256)
            {
                return Err(format!(
                    "{code_prefix}_SOURCE_BOUNDARY_INVALID: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} stored_start_offset_bytes={stored_start} expected_start_offset_bytes={} stored_bytes={stored_bytes} expected_bytes={} stored_sha256={stored_sha256:?}; remediation=repair the corrupt cursor boundary from the physical source/transcript SoTs",
                    source_path.display(),
                    actual.start_offset_bytes,
                    actual.bytes
                ));
            }
            if actual.sha256 != stored_sha256 {
                return Err(format!(
                    "{code_prefix}_SOURCE_BOUNDARY_MISMATCH: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} boundary_start_offset_bytes={stored_start} boundary_bytes={stored_bytes} expected_sha256={stored_sha256} actual_sha256={}; remediation=restore the original append-only source bytes at the persisted cursor boundary or reconcile every durable transcript row before rebuilding the cursor",
                    source_path.display(),
                    actual.sha256
                ));
            }
            Ok(false)
        }
        (None, None, None) => {
            verify_legacy_boundary_row(
                db,
                source_id,
                source_path,
                snapshot_size_bytes,
                cursor_offset_bytes,
                lines_ingested,
                code_prefix,
            )?;
            refresh_transcript_source_boundary(
                source_path,
                snapshot_size_bytes,
                TranscriptSourceBoundaryState {
                    cursor_offset_bytes,
                    lines_ingested,
                    start_offset_bytes: boundary_start_offset_bytes,
                    bytes: boundary_bytes,
                    sha256: boundary_sha256,
                },
                code_prefix,
            )?;
            Ok(true)
        }
        _ => Err(format!(
            "{code_prefix}_SOURCE_BOUNDARY_PARTIAL: source_id={source_id} path={} cursor_offset_bytes={cursor_offset_bytes} boundary_start_offset_bytes={boundary_start_offset_bytes:?} boundary_bytes={boundary_bytes:?} boundary_sha256_present={}; remediation=repair the corrupt cursor boundary tuple from the physical source/transcript SoTs",
            source_path.display(),
            boundary_sha256.is_some()
        )),
    }
}

pub(super) fn refresh_transcript_source_boundary(
    source_path: &Path,
    snapshot_size_bytes: u64,
    state: TranscriptSourceBoundaryState<'_>,
    code_prefix: &'static str,
) -> Result<(), String> {
    let TranscriptSourceBoundaryState {
        cursor_offset_bytes,
        start_offset_bytes: boundary_start_offset_bytes,
        bytes: boundary_bytes,
        sha256: boundary_sha256,
        ..
    } = state;
    let boundary = read_transcript_source_boundary(
        source_path,
        snapshot_size_bytes,
        cursor_offset_bytes,
        code_prefix,
    )?;
    *boundary_start_offset_bytes = Some(boundary.start_offset_bytes);
    *boundary_bytes = Some(boundary.bytes);
    *boundary_sha256 = Some(boundary.sha256);
    Ok(())
}

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
    /// Stable source-time seed in Unix milliseconds, normally from
    /// spawn-manifest.json. Used only when a transcript line lacks its own
    /// timestamp/UUIDv7 time anchor so retries encode the same row bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_epoch_unix_ms: Option<u64>,
    /// Stable content identity for the append-only source. The prefix length
    /// is frozen when the first chunk commits; later growth does not alter it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint_sha256: Option<String>,
    /// Exact bounded bytes ending at `offset_bytes`. Unlike the stable prefix
    /// identity above, this moves after every committed chunk and proves that
    /// resume still lands on the same physical source boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary_start_offset_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary_sha256: Option<String>,
    /// True once the source reached its terminal state and the tail was
    /// fully consumed; complete spawns are skipped by later cycles.
    pub source_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_reason: Option<String>,
    /// End-state outcome last grounded from the durable terminal event. Older
    /// cursor rows omit this, so completed sources without it are rechecked
    /// once instead of being permanently skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_state_anchor_outcome: Option<String>,
    /// Physical transcript row count covered by the last end-state grounding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_state_anchor_rows: Option<u64>,
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
    pub cancelled: bool,
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

/// Publishes one bounded transcript chunk, then performs independent point
/// reads of every source and timestamp-index row before any cursor may cover
/// the corresponding source bytes. Constellation publication performs its own
/// native Calyx readback and remains before the cursor commit marker.
pub(super) fn commit_transcript_chunk(
    db: &Db,
    code_prefix: &'static str,
    source_id: &str,
    source_path: &Path,
    rows: &[PreparedTranscriptRow],
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    if rows.len() > MAX_AGENT_TRANSCRIPT_COMMIT_ROWS {
        return Err(format!(
            "{code_prefix}_COMMIT_INVARIANT_FAILED: source_id={source_id} path={} rows={} max_rows={MAX_AGENT_TRANSCRIPT_COMMIT_ROWS}; remediation=repair the ingester chunk planner before retrying",
            source_path.display(),
            rows.len()
        ));
    }
    let encoded_value_bytes = rows
        .iter()
        .try_fold(0_usize, |total, row| total.checked_add(row.encoded.len()));
    let max_encoded_value_bytes =
        MAX_AGENT_TRANSCRIPT_COMMIT_ROWS.saturating_mul(MAX_AGENT_TRANSCRIPT_VALUE_BYTES);
    let Some(encoded_value_bytes) = encoded_value_bytes else {
        return Err(format!(
            "{code_prefix}_COMMIT_SIZE_OVERFLOW: source_id={source_id} path={}; remediation=repair the ingester size accounting before retrying",
            source_path.display()
        ));
    };
    if encoded_value_bytes > max_encoded_value_bytes {
        return Err(format!(
            "{code_prefix}_COMMIT_INVARIANT_FAILED: source_id={source_id} path={} encoded_value_bytes={encoded_value_bytes} max_encoded_value_bytes={max_encoded_value_bytes}; remediation=repair the row-size/chunk invariant before retrying",
            source_path.display()
        ));
    }

    let mut guards = Vec::with_capacity(rows.len().saturating_mul(2));
    for row in rows {
        let source_revision = db
            .get_cf_revisioned(cf::CF_AGENT_TRANSCRIPTS, &row.source_key)
            .map_err(|error| {
                format!(
                    "{code_prefix}_ROW_PRECONDITION_READ_FAILED: source_id={source_id} path={} line_no={} source_offset_bytes={} key_hex={}: {error}; remediation=repair the Calyx revisioned point-read before retrying",
                    source_path.display(),
                    row.record.line_no,
                    row.source_offset_bytes,
                    synapse_storage::constellations::hex_encode(&row.source_key)
                )
            })?;
        let expected_source_revision = match source_revision {
            Some(revisioned) => {
                if revisioned.value.as_deref() != Some(row.encoded.as_slice()) {
                    return Err(format!(
                        "{code_prefix}_ROW_IDENTITY_CONFLICT: source_id={source_id} path={} line_no={} source_offset_bytes={} key_hex={} expected_sha256={} actual_sha256={}; remediation=restore the original source/cursor or reconcile the conflicting deterministic transcript row without overwriting it",
                        source_path.display(),
                        row.record.line_no,
                        row.source_offset_bytes,
                        synapse_storage::constellations::hex_encode(&row.source_key),
                        sha256_hex(&row.encoded),
                        revisioned
                            .value
                            .as_deref()
                            .map_or_else(|| "expired".to_owned(), sha256_hex)
                    ));
                }
                Some(revisioned.revision_sha256)
            }
            None => None,
        };
        guards.push(CfRevisionGuard::new(
            cf::CF_AGENT_TRANSCRIPTS,
            row.source_key.clone(),
            expected_source_revision,
        ));

        let index_revision = db
            .get_cf_revisioned(cf::CF_KV, &row.ts_index_key)
            .map_err(|error| {
                format!(
                    "{code_prefix}_INDEX_PRECONDITION_READ_FAILED: source_id={source_id} path={} line_no={} source_offset_bytes={} index_key_hex={}: {error}; remediation=repair the Calyx revisioned point-read before retrying",
                    source_path.display(),
                    row.record.line_no,
                    row.source_offset_bytes,
                    synapse_storage::constellations::hex_encode(&row.ts_index_key)
                )
            })?;
        let expected_index_revision = match index_revision {
            Some(revisioned) => {
                if revisioned.value.as_deref() != Some(row.source_key.as_slice()) {
                    return Err(format!(
                        "{code_prefix}_INDEX_IDENTITY_CONFLICT: source_id={source_id} path={} line_no={} source_offset_bytes={} index_key_hex={} expected_source_key_hex={} actual_sha256={}; remediation=reconcile the conflicting deterministic timestamp index without overwriting it",
                        source_path.display(),
                        row.record.line_no,
                        row.source_offset_bytes,
                        synapse_storage::constellations::hex_encode(&row.ts_index_key),
                        synapse_storage::constellations::hex_encode(&row.source_key),
                        revisioned
                            .value
                            .as_deref()
                            .map_or_else(|| "expired".to_owned(), sha256_hex)
                    ));
                }
                Some(revisioned.revision_sha256)
            }
            None => None,
        };
        guards.push(CfRevisionGuard::new(
            cf::CF_KV,
            row.ts_index_key.clone(),
            expected_index_revision,
        ));
    }

    let transcript_rows = rows
        .iter()
        .map(|row| (row.source_key.clone(), row.encoded.clone()))
        .collect();
    let timestamp_rows = rows
        .iter()
        .map(|row| (row.ts_index_key.clone(), row.source_key.clone()))
        .collect();
    let outcome = db
        .put_cf_batches_if_revisions_pressure_bypass(
            guards,
            vec![
                (cf::CF_AGENT_TRANSCRIPTS, transcript_rows),
                (cf::CF_KV, timestamp_rows),
            ],
        )
        .map_err(|error| {
            format!(
                "{code_prefix}_ROWS_WRITE_FAILED: source_id={source_id} path={} first_offset_bytes={} rows={} encoded_value_bytes={encoded_value_bytes}: {error}; remediation=repair the Calyx guarded write failure and retry from the unchanged cursor",
                source_path.display(),
                rows[0].source_offset_bytes,
                rows.len()
            )
        })?;
    if !outcome.applied {
        let conflict = outcome.conflict.map_or_else(
            || "missing_conflict_detail".to_owned(),
            |conflict| {
                format!(
                    "guard_index={} key_hex={} expected_revision={} actual_revision={}",
                    conflict.guard_index,
                    synapse_storage::constellations::hex_encode(&conflict.key),
                    conflict.expected_revision_sha256.map_or_else(
                        || "absent".to_owned(),
                        |value| synapse_storage::constellations::hex_encode(&value)
                    ),
                    conflict.actual_revision_sha256.map_or_else(
                        || "absent".to_owned(),
                        |value| synapse_storage::constellations::hex_encode(&value)
                    )
                )
            },
        );
        return Err(format!(
            "{code_prefix}_ROWS_REVISION_CONFLICT: source_id={source_id} path={} first_offset_bytes={} rows={} conflict={conflict}; remediation=discard this stale prepared chunk and reload the authoritative cursor/source state",
            source_path.display(),
            rows[0].source_offset_bytes,
            rows.len()
        ));
    }

    for row in rows {
        let actual = db
            .get_cf(cf::CF_AGENT_TRANSCRIPTS, &row.source_key)
            .map_err(|error| {
                format!(
                    "{code_prefix}_ROW_READBACK_FAILED: source_id={source_id} path={} line_no={} source_offset_bytes={} key_hex={}: {error}; remediation=repair the Calyx point-read path and retry from the unchanged cursor",
                    source_path.display(),
                    row.record.line_no,
                    row.source_offset_bytes,
                    synapse_storage::constellations::hex_encode(&row.source_key)
                )
            })?;
        if actual.as_deref() != Some(row.encoded.as_slice()) {
            return Err(format!(
                "{code_prefix}_ROW_READBACK_MISMATCH: source_id={source_id} path={} line_no={} source_offset_bytes={} key_hex={} expected_sha256={} actual_sha256={}; remediation=quarantine and repair the divergent Calyx row before clearing the cursor",
                source_path.display(),
                row.record.line_no,
                row.source_offset_bytes,
                synapse_storage::constellations::hex_encode(&row.source_key),
                sha256_hex(&row.encoded),
                actual
                    .as_deref()
                    .map_or_else(|| "absent".to_owned(), sha256_hex)
            ));
        }

        let actual_index = db.get_cf(cf::CF_KV, &row.ts_index_key).map_err(|error| {
            format!(
                "{code_prefix}_INDEX_READBACK_FAILED: source_id={source_id} path={} line_no={} source_offset_bytes={} index_key_hex={}: {error}; remediation=repair the Calyx point-read path and retry from the unchanged cursor",
                source_path.display(),
                row.record.line_no,
                row.source_offset_bytes,
                synapse_storage::constellations::hex_encode(&row.ts_index_key)
            )
        })?;
        if actual_index.as_deref() != Some(row.source_key.as_slice()) {
            return Err(format!(
                "{code_prefix}_INDEX_READBACK_MISMATCH: source_id={source_id} path={} line_no={} source_offset_bytes={} index_key_hex={} expected_source_key_hex={} actual_sha256={}; remediation=quarantine and repair the divergent timestamp index before clearing the cursor",
                source_path.display(),
                row.record.line_no,
                row.source_offset_bytes,
                synapse_storage::constellations::hex_encode(&row.ts_index_key),
                synapse_storage::constellations::hex_encode(&row.source_key),
                actual_index
                    .as_deref()
                    .map_or_else(|| "absent".to_owned(), sha256_hex)
            ));
        }

        db.put_agent_transcript_constellation(&row.source_key, &row.encoded, &row.record)
            .map_err(|error| {
                format!(
                    "{code_prefix}_CONSTELLATION_MEASUREMENT_FAILED: source_id={source_id} path={} line_no={} source_offset_bytes={} source_key_hex={}: {error}; remediation=repair native Calyx constellation publication and retry from the unchanged cursor",
                    source_path.display(),
                    row.record.line_no,
                    row.source_offset_bytes,
                    synapse_storage::constellations::hex_encode(&row.source_key)
                )
            })?;
    }

    tracing::debug!(
        code = "TRANSCRIPT_CHUNK_COMMITTED",
        source_kind = code_prefix,
        source_id,
        source_path = %source_path.display(),
        first_offset_bytes = rows[0].source_offset_bytes,
        rows = rows.len(),
        encoded_value_bytes,
        "bounded transcript source/index rows and native constellations have exact physical readback"
    );
    Ok(())
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
    let marker_exists = |name: &str| -> Result<bool, String> {
        let path = log_dir.join(name);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Err(format!(
                "TRANSCRIPT_SOURCE_MARKER_INVALID: path={} is not a regular file; remediation=restore the spawn artifact as a regular file",
                path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "TRANSCRIPT_SOURCE_MARKER_STAT_FAILED: path={}: {error}; remediation=restore metadata/read access to the spawn directory",
                path.display()
            )),
        }
    };
    let claude_mcp = marker_exists("claude-mcp-config.json")?;
    let claude_hooks = marker_exists("claude-hook-settings.json")?;
    let claude_debug = marker_exists("claude-debug.log")?;
    let claude = claude_mcp || claude_hooks || claude_debug;
    let codex_runner = marker_exists("codex-app-server-runner.ps1")?;
    let codex_control = marker_exists("codex-control.json")?;
    let codex_events = marker_exists("codex-app-server-events.jsonl")?;
    let codex_app_server = codex_runner || codex_control || codex_events;
    let codex = !codex_app_server && marker_exists("codex-notify.ps1")?;
    let local = marker_exists("local-model-runner.json")?;
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

#[derive(Clone, Debug, Default)]
struct SpawnManifestSeed {
    model: Option<String>,
    created_unix_ms: Option<u64>,
}

/// Reads stable spawn metadata recorded at launch.
///
/// `model` is the authoritative model seed for Codex spawns, whose stream may
/// omit the model id (#949). `created_unix_ms` is the stable timestamp seed used
/// only when an individual transcript line has no timestamp/UUIDv7 time anchor.
/// Missing, unreadable, or malformed launch metadata is fatal and parked on
/// the cursor. Ingestion must not silently substitute guessed identity/time
/// seeds when the spawn owner promised an authoritative manifest.
fn read_spawn_manifest_seed(log_dir: &Path) -> Result<SpawnManifestSeed, String> {
    let path = log_dir.join(super::m4_tools::AGENT_SPAWN_MANIFEST_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "TRANSCRIPT_SPAWN_MANIFEST_MISSING: path={}; remediation=restore the launch-time spawn manifest before ingesting the transcript",
                path.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "TRANSCRIPT_SPAWN_MANIFEST_READ_FAILED: path={}: {error}; remediation=restore read access to the spawn manifest before ingesting the source",
                path.display()
            ));
        }
    };
    let manifest = serde_json::from_slice::<Value>(&bytes).map_err(|error| {
        format!(
            "TRANSCRIPT_SPAWN_MANIFEST_INVALID: path={}: {error}; remediation=repair the manifest JSON from the spawn launch SoT before ingesting the source",
            path.display()
        )
    })?;
    let object = manifest.as_object().ok_or_else(|| {
        format!(
            "TRANSCRIPT_SPAWN_MANIFEST_INVALID: path={} root must be a JSON object; remediation=repair the manifest from the spawn launch SoT",
            path.display()
        )
    })?;
    let model = manifest
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned);
    let created_unix_ms = manifest.get("created_unix_ms").and_then(Value::as_u64);
    if object.contains_key("model") && model.is_none() {
        return Err(format!(
            "TRANSCRIPT_SPAWN_MANIFEST_INVALID: path={} model must be a non-empty string when present; remediation=repair the manifest from the spawn launch SoT",
            path.display()
        ));
    }
    if object.contains_key("created_unix_ms") && created_unix_ms.is_none() {
        return Err(format!(
            "TRANSCRIPT_SPAWN_MANIFEST_INVALID: path={} created_unix_ms must be an unsigned integer when present; remediation=repair the manifest from the spawn launch SoT",
            path.display()
        ));
    }
    Ok(SpawnManifestSeed {
        model,
        created_unix_ms,
    })
}

#[derive(Debug)]
struct LoadedTranscriptCursor {
    cursor: Option<TranscriptCursor>,
    revision_sha256: Option<[u8; 32]>,
}

fn load_cursor(db: &Db, spawn_id: &str) -> Result<LoadedTranscriptCursor, String> {
    let key = cursor_kv_key(spawn_id);
    let Some(revisioned) = db.get_cf_revisioned(cf::CF_KV, &key).map_err(|error| {
        format!(
            "TRANSCRIPT_CURSOR_READ_FAILED: spawn_id={spawn_id} key={CURSOR_KV_PREFIX}{spawn_id}: {error}; remediation=repair the exact Calyx cursor point-read before ingesting source bytes"
        )
    })? else {
        return Ok(LoadedTranscriptCursor {
            cursor: None,
            revision_sha256: None,
        });
    };
    let value = revisioned.value.ok_or_else(|| {
        format!(
            "TRANSCRIPT_CURSOR_EXPIRED: spawn_id={spawn_id} key={CURSOR_KV_PREFIX}{spawn_id}; remediation=restore the non-expiring cursor row or reconcile all physical transcript rows before rebuilding it"
        )
    })?;
    let cursor: TranscriptCursor = decode_json(&value).map_err(|error| {
        format!(
            "TRANSCRIPT_CURSOR_DECODE_FAILED: spawn_id={spawn_id} key={CURSOR_KV_PREFIX}{spawn_id}: {error}; remediation=repair the cursor bytes from the physical source/transcript SoTs"
        )
    })?;
    if cursor.record_version != TRANSCRIPT_CURSOR_VERSION
        || cursor.spawn_id != spawn_id
        || cursor.source_path.trim().is_empty()
    {
        return Err(format!(
            "TRANSCRIPT_CURSOR_IDENTITY_INVALID: requested_spawn_id={spawn_id} stored_spawn_id={} record_version={} expected_version={TRANSCRIPT_CURSOR_VERSION} source_path={:?}; remediation=repair the cursor identity from the physical source/transcript SoTs",
            cursor.spawn_id, cursor.record_version, cursor.source_path
        ));
    }
    Ok(LoadedTranscriptCursor {
        cursor: Some(cursor),
        revision_sha256: Some(revisioned.revision_sha256),
    })
}

fn store_cursor(
    db: &Db,
    cursor: &TranscriptCursor,
    expected_revision_sha256: &mut Option<[u8; 32]>,
) -> Result<(), String> {
    let key = cursor_kv_key(&cursor.spawn_id);
    let encoded =
        encode_json(cursor).map_err(|error| format!("TRANSCRIPT_CURSOR_ENCODE_FAILED: {error}"))?;
    let outcome = db
        .put_batch_if_revision_pressure_bypass(
            cf::CF_KV,
            &key,
            *expected_revision_sha256,
            [(key.clone(), encoded.clone())],
        )
        .map_err(|error| {
            format!(
                "TRANSCRIPT_CURSOR_WRITE_FAILED: spawn_id={} source_path={} offset_bytes={}: {error}; remediation=repair the guarded Calyx cursor write and retry from the last persisted cursor",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            )
        })?;
    if !outcome.applied {
        return Err(format!(
            "TRANSCRIPT_CURSOR_REVISION_CONFLICT: spawn_id={} source_path={} offset_bytes={} expected_revision_sha256={} actual_revision_sha256={}; remediation=discard this stale ingest pass and reload the authoritative cursor",
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
            "TRANSCRIPT_CURSOR_WRITE_OUTCOME_INVALID: spawn_id={} offset_bytes={} applied write omitted committed revision; remediation=repair the Calyx guarded-write outcome contract",
            cursor.spawn_id, cursor.offset_bytes
        )
    })?;
    let readback = db
        .get_cf_revisioned(cf::CF_KV, &key)
        .map_err(|error| {
            format!(
                "TRANSCRIPT_CURSOR_READBACK_FAILED: spawn_id={} source_path={} offset_bytes={}: {error}; remediation=repair the Calyx point-read path and reconcile the committed cursor",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            )
        })?
        .ok_or_else(|| {
            format!(
                "TRANSCRIPT_CURSOR_READBACK_MISSING: spawn_id={} source_path={} offset_bytes={}; remediation=repair the missing committed cursor before ingest resumes",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            )
        })?;
    if readback.revision_sha256 != committed_revision
        || readback.value.as_deref() != Some(encoded.as_slice())
    {
        return Err(format!(
            "TRANSCRIPT_CURSOR_READBACK_MISMATCH: spawn_id={} source_path={} offset_bytes={} expected_value_sha256={} actual_value_sha256={} expected_revision_sha256={} actual_revision_sha256={}; remediation=quarantine and repair the divergent cursor before ingest resumes",
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

/// Marks a spawn's cursor with a sticky error and persists it. The error is
/// logged once here (with full context) and the spawn is skipped by later
/// cycles until the cursor row is cleared.
fn stick_cursor_error(
    db: &Db,
    cursor: &mut TranscriptCursor,
    cursor_revision_sha256: &mut Option<[u8; 32]>,
    detail: String,
) -> String {
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
    if let Err(store_error) = store_cursor(db, cursor, cursor_revision_sha256) {
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
fn completion_is_terminal(log_dir: &Path) -> Result<bool, String> {
    let path = log_dir.join("completion-status.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "TRANSCRIPT_COMPLETION_STATUS_READ_FAILED: path={}: {error}; remediation=restore read access to the completion artifact before deciding source finality",
                path.display()
            ));
        }
    };
    let status: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "TRANSCRIPT_COMPLETION_STATUS_INVALID: path={}: {error}; remediation=repair the completion artifact JSON before deciding source finality",
            path.display()
        )
    })?;
    let value = status.get("status").and_then(Value::as_str).ok_or_else(|| {
        format!(
            "TRANSCRIPT_COMPLETION_STATUS_INVALID: path={} missing string status; remediation=repair the completion artifact from the session lifecycle SoT",
            path.display()
        )
    })?;
    Ok(value != "running")
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
    ingest_spawn_dir_once_with_cancel(db, spawn_id, log_dir, finalize, None)
}

fn ingest_spawn_dir_once_with_cancel(
    db: &Db,
    spawn_id: &str,
    log_dir: &Path,
    finalize: bool,
    cancel: Option<&CancellationToken>,
) -> Result<SpawnIngestOutcome, String> {
    validate_spawn_id_shape(spawn_id)?;
    let stdout_path = log_dir.join("stdout.jsonl");

    let loaded = load_cursor(db, spawn_id)?;
    let mut cursor_revision_sha256 = loaded.revision_sha256;
    let mut cursor = match loaded.cursor {
        Some(cursor) => cursor,
        None => {
            let manifest_seed = match read_spawn_manifest_seed(log_dir) {
                Ok(seed) => seed,
                Err(detail) => {
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
                        source_epoch_unix_ms: None,
                        source_fingerprint_bytes: None,
                        source_fingerprint_sha256: None,
                        source_boundary_start_offset_bytes: None,
                        source_boundary_bytes: None,
                        source_boundary_sha256: None,
                        source_complete: false,
                        completed_reason: None,
                        end_state_anchor_outcome: None,
                        end_state_anchor_rows: None,
                        error: None,
                        updated_ts_ns: unix_time_ns_now(),
                    };
                    return Err(stick_cursor_error(
                        db,
                        &mut cursor,
                        &mut cursor_revision_sha256,
                        detail,
                    ));
                }
            };
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
                        source_epoch_unix_ms: manifest_seed.created_unix_ms,
                        source_fingerprint_bytes: None,
                        source_fingerprint_sha256: None,
                        source_boundary_start_offset_bytes: None,
                        source_boundary_bytes: None,
                        source_boundary_sha256: None,
                        source_complete: false,
                        completed_reason: None,
                        end_state_anchor_outcome: None,
                        end_state_anchor_rows: None,
                        error: None,
                        updated_ts_ns: unix_time_ns_now(),
                    };
                    return Err(stick_cursor_error(
                        db,
                        &mut cursor,
                        &mut cursor_revision_sha256,
                        detail,
                    ));
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
                model: manifest_seed.model,
                source_epoch_unix_ms: manifest_seed.created_unix_ms,
                source_fingerprint_bytes: None,
                source_fingerprint_sha256: None,
                source_boundary_start_offset_bytes: None,
                source_boundary_bytes: None,
                source_boundary_sha256: None,
                source_complete: false,
                completed_reason: None,
                end_state_anchor_outcome: None,
                end_state_anchor_rows: None,
                error: None,
                updated_ts_ns: unix_time_ns_now(),
            }
        }
    };
    let lines_ingested_before_cycle = cursor.lines_ingested;

    if Path::new(&cursor.source_path) != stdout_path {
        let detail = format!(
            "TRANSCRIPT_CURSOR_SOURCE_PATH_MISMATCH: cursor_path={} discovered_path={}; remediation=restore the original source path or reconcile and rebuild the cursor without reusing its identity",
            cursor.source_path,
            stdout_path.display()
        );
        return Err(stick_cursor_error(
            db,
            &mut cursor,
            &mut cursor_revision_sha256,
            detail,
        ));
    }

    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Ok(SpawnIngestOutcome {
            lines_ingested_total: lines_ingested_before_cycle,
            cancelled: true,
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
            let offset_bytes = cursor.offset_bytes;
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                format!(
                    "TRANSCRIPT_SOURCE_STAT_FAILED: path={} offset_bytes={}: {error}; remediation=restore the exact append-only source and metadata access before clearing the sticky cursor",
                    stdout_path.display(),
                    offset_bytes
                ),
            ));
        }
    };
    if !metadata.is_file() {
        let detail = format!(
            "TRANSCRIPT_SOURCE_NOT_FILE: path={} offset_bytes={}; remediation=restore stdout.jsonl as the regular append-only file created by the spawn owner",
            stdout_path.display(),
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
            "TRANSCRIPT_SOURCE_TRUNCATED: path={} file_size_bytes={file_size} cursor_offset_bytes={}; remediation=restore the original append-only source bytes or reconcile all durable transcript rows before clearing the cursor",
            stdout_path.display(),
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
        spawn_id,
        &stdout_path,
        file_size,
        cursor.offset_bytes,
        &mut cursor.source_fingerprint_bytes,
        &mut cursor.source_fingerprint_sha256,
        "TRANSCRIPT",
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
        spawn_id,
        &stdout_path,
        file_size,
        TranscriptSourceBoundaryState {
            cursor_offset_bytes: cursor.offset_bytes,
            lines_ingested: cursor.lines_ingested,
            start_offset_bytes: &mut cursor.source_boundary_start_offset_bytes,
            bytes: &mut cursor.source_boundary_bytes,
            sha256: &mut cursor.source_boundary_sha256,
        },
        "TRANSCRIPT",
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

    if cursor.source_complete {
        if file_size > cursor.offset_bytes {
            tracing::warn!(
                code = "TRANSCRIPT_SOURCE_COMPLETE_DRIFT",
                spawn_id,
                cursor_offset_bytes = cursor.offset_bytes,
                file_size,
                completed_reason = cursor.completed_reason.as_deref().unwrap_or("unknown"),
                "completed transcript source grew after cursor completion; reopening ingestion before end-state grounding"
            );
            cursor.source_complete = false;
            cursor.completed_reason = None;
            cursor.end_state_anchor_outcome = None;
            cursor.end_state_anchor_rows = None;
            cursor.updated_ts_ns = unix_time_ns_now();
            store_cursor(db, &cursor, &mut cursor_revision_sha256)?;
        } else {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Ok(SpawnIngestOutcome {
                    lines_ingested_total: lines_ingested_before_cycle,
                    source_complete: true,
                    cancelled: true,
                    ..SpawnIngestOutcome::default()
                });
            }
            ensure_completed_spawn_end_state_anchored(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
            )?;
            return Ok(SpawnIngestOutcome {
                lines_ingested_total: cursor.lines_ingested,
                source_complete: true,
                skipped: true,
                ..SpawnIngestOutcome::default()
            });
        }
    }

    let completion_terminal = match completion_is_terminal(log_dir) {
        Ok(value) => value,
        Err(detail) => {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
    };
    let finalize = finalize || completion_terminal;

    // Explicit pressure gate: rows below ride a bypass write, so this check
    // is the single authority on whether this pass may write transcript rows.
    if file_size > cursor.offset_bytes && !db.pressure_permits_write(cf::CF_AGENT_TRANSCRIPTS) {
        PRESSURE_DEFERRALS_TOTAL.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            code = "TRANSCRIPT_INGEST_PRESSURE_DEFERRED",
            spawn_id,
            source_path = %stdout_path.display(),
            cursor_offset_bytes = cursor.offset_bytes,
            snapshot_size_bytes = file_size,
            "disk pressure defers transcript ingestion; cursor not advanced"
        );
        return Ok(SpawnIngestOutcome {
            lines_ingested_total: cursor.lines_ingested,
            deferred_for_pressure: true,
            ..SpawnIngestOutcome::default()
        });
    }

    let mut reader = match BoundedTranscriptTailReader::open(
        &stdout_path,
        cursor.offset_bytes,
        file_size,
        "TRANSCRIPT",
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
    let mut pass_rows = 0_usize;
    let mut pass_source_bytes = 0_u64;
    let mut reached_snapshot_eof = false;
    let mut cancelled = false;

    'pass: while pass_rows < MAX_AGENT_TRANSCRIPT_ROWS_PER_PASS
        && pass_source_bytes < MAX_AGENT_TRANSCRIPT_SOURCE_BYTES_PER_PASS
    {
        let mut working_cursor = cursor.clone();
        let mut chunk = Vec::with_capacity(MAX_AGENT_TRANSCRIPT_COMMIT_ROWS);
        let mut chunk_parsed = 0_u64;
        let mut chunk_invalid = 0_u64;
        let mut pending_error: Option<String> = None;
        let mut incomplete_tail = false;

        while chunk.len() < MAX_AGENT_TRANSCRIPT_COMMIT_ROWS
            && pass_rows + chunk.len() < MAX_AGENT_TRANSCRIPT_ROWS_PER_PASS
        {
            match reader.next_line(finalize, cancel) {
                Ok(BoundedTailRead::Line {
                    bytes,
                    consumed_bytes,
                    source_offset_bytes,
                }) => {
                    let Some(line_no) = working_cursor.lines_ingested.checked_add(1) else {
                        pending_error = Some(format!(
                            "TRANSCRIPT_LINE_NUMBER_OVERFLOW: path={} source_offset_bytes={source_offset_bytes}; remediation=quarantine the impossible-size source and reconcile its cursor",
                            stdout_path.display()
                        ));
                        break;
                    };
                    let cursor_before_line = working_cursor.clone();
                    let record = parse_line(&bytes, line_no, &mut working_cursor);
                    if let Err(detail) = record.validate() {
                        working_cursor = cursor_before_line;
                        pending_error = Some(format!(
                            "TRANSCRIPT_ROW_VALIDATION_FAILED: path={} line_no={line_no} source_offset_bytes={source_offset_bytes}: {detail}; remediation=repair the parser/record invariant before clearing the cursor",
                            stdout_path.display()
                        ));
                        break;
                    }
                    let encoded = match encode_json(&record) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            working_cursor = cursor_before_line;
                            pending_error = Some(format!(
                                "TRANSCRIPT_ROW_ENCODE_FAILED: path={} line_no={line_no} source_offset_bytes={source_offset_bytes}: {error}; remediation=repair transcript row serialization before clearing the cursor",
                                stdout_path.display()
                            ));
                            break;
                        }
                    };
                    if encoded.len() > MAX_AGENT_TRANSCRIPT_VALUE_BYTES {
                        working_cursor = cursor_before_line;
                        pending_error = Some(format!(
                            "TRANSCRIPT_ROW_OVERSIZED: path={} line_no={line_no} source_offset_bytes={source_offset_bytes} encoded_bytes={} max_encoded_bytes={MAX_AGENT_TRANSCRIPT_VALUE_BYTES}; remediation=repair the per-field normalization bounds before clearing the cursor",
                            stdout_path.display(),
                            encoded.len()
                        ));
                        break;
                    }
                    match record.status {
                        TranscriptParseStatus::Parsed => chunk_parsed += 1,
                        TranscriptParseStatus::Invalid => {
                            chunk_invalid += 1;
                            tracing::error!(
                                code = "TRANSCRIPT_LINE_INVALID",
                                spawn_id,
                                source_path = %stdout_path.display(),
                                line_no,
                                source_offset_bytes,
                                raw_line_bytes = record.raw_line_bytes,
                                raw_line_sha256 = %record.raw_line_sha256,
                                detail = record.parse_error.as_deref().unwrap_or("unknown"),
                                remediation = "repair the producer's UTF-8 JSONL record; the invalid evidence row remains durable and is never silently skipped",
                                "source line refused by the version-pinned parser; invalid row will be written"
                            );
                        }
                    }
                    let source_key = agent_transcript_key(spawn_id, line_no);
                    let ts_index_key = agent_transcript_ts_index_key(record.ts_ns, &source_key);
                    chunk.push(PreparedTranscriptRow {
                        source_key,
                        encoded,
                        ts_index_key,
                        record,
                        source_offset_bytes,
                        consumed_bytes,
                    });
                    working_cursor.lines_ingested = line_no;
                    pass_source_bytes = pass_source_bytes.saturating_add(consumed_bytes);
                    if pass_source_bytes >= MAX_AGENT_TRANSCRIPT_SOURCE_BYTES_PER_PASS {
                        break;
                    }
                }
                Ok(BoundedTailRead::SnapshotEof) => {
                    reached_snapshot_eof = true;
                    break;
                }
                Ok(BoundedTailRead::IncompleteTail {
                    source_offset_bytes,
                    buffered_bytes,
                }) => {
                    incomplete_tail = true;
                    tracing::debug!(
                        code = "TRANSCRIPT_INCOMPLETE_TAIL_DEFERRED",
                        spawn_id,
                        source_path = %stdout_path.display(),
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
                    pending_error = Some(detail);
                    break;
                }
            }
        }

        if cancelled {
            tracing::info!(
                code = "TRANSCRIPT_INGEST_CYCLE_CANCELLED",
                spawn_id,
                source_path = %stdout_path.display(),
                committed_rows = pass_rows,
                discarded_prepared_rows = chunk.len(),
                cursor_offset_bytes = cursor.offset_bytes,
                "daemon shutdown cancelled transcript ingestion before the next bounded commit"
            );
            break 'pass;
        }

        if chunk.is_empty() {
            if let Some(detail) = pending_error {
                return Err(stick_cursor_error(
                    db,
                    &mut cursor,
                    &mut cursor_revision_sha256,
                    detail,
                ));
            }
            break 'pass;
        }

        if cancel.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            break 'pass;
        }
        if let Err(detail) =
            commit_transcript_chunk(db, "TRANSCRIPT", spawn_id, &stdout_path, &chunk)
        {
            tracing::error!(
                code = "TRANSCRIPT_CHUNK_COMMIT_FAILED",
                spawn_id,
                source_path = %stdout_path.display(),
                cursor_offset_bytes = cursor.offset_bytes,
                rows = chunk.len(),
                detail = %detail,
                "bounded transcript chunk failed; durable cursor remains unchanged"
            );
            return Err(detail);
        }

        let consumed_bytes = chunk
            .iter()
            .try_fold(0_u64, |total, row| total.checked_add(row.consumed_bytes));
        let Some(consumed_bytes) = consumed_bytes else {
            let detail = format!(
                "TRANSCRIPT_SOURCE_OFFSET_OVERFLOW: path={} cursor_offset_bytes={}; remediation=quarantine the impossible-size source and reconcile the cursor",
                stdout_path.display(),
                cursor.offset_bytes
            );
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        };
        working_cursor.offset_bytes = cursor.offset_bytes.checked_add(consumed_bytes).ok_or_else(|| {
            format!(
                "TRANSCRIPT_SOURCE_OFFSET_OVERFLOW: path={} cursor_offset_bytes={} consumed_bytes={consumed_bytes}; remediation=quarantine the impossible-size source and reconcile the cursor",
                stdout_path.display(), cursor.offset_bytes
            )
        })?;
        working_cursor.parsed_rows = cursor.parsed_rows.checked_add(chunk_parsed).ok_or_else(|| {
            "TRANSCRIPT_PARSED_COUNTER_OVERFLOW: remediation=reconcile the impossible-size cursor"
                .to_owned()
        })?;
        working_cursor.invalid_rows = cursor
            .invalid_rows
            .checked_add(chunk_invalid)
            .ok_or_else(|| {
                "TRANSCRIPT_INVALID_COUNTER_OVERFLOW: remediation=reconcile the impossible-size cursor"
                    .to_owned()
            })?;
        if let Err(detail) = ensure_transcript_source_fingerprint(
            db,
            spawn_id,
            &stdout_path,
            file_size,
            working_cursor.offset_bytes,
            &mut working_cursor.source_fingerprint_bytes,
            &mut working_cursor.source_fingerprint_sha256,
            "TRANSCRIPT",
        ) {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
        if let Err(detail) = refresh_transcript_source_boundary(
            &stdout_path,
            file_size,
            TranscriptSourceBoundaryState {
                cursor_offset_bytes: working_cursor.offset_bytes,
                lines_ingested: working_cursor.lines_ingested,
                start_offset_bytes: &mut working_cursor.source_boundary_start_offset_bytes,
                bytes: &mut working_cursor.source_boundary_bytes,
                sha256: &mut working_cursor.source_boundary_sha256,
            },
            "TRANSCRIPT",
        ) {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
        working_cursor.updated_ts_ns = unix_time_ns_now();
        store_cursor(db, &working_cursor, &mut cursor_revision_sha256)?;
        cursor = working_cursor;
        pass_rows += chunk.len();
        new_parsed += chunk_parsed;
        new_invalid += chunk_invalid;
        LINES_PARSED_TOTAL.fetch_add(chunk_parsed, Ordering::Relaxed);
        LINES_INVALID_TOTAL.fetch_add(chunk_invalid, Ordering::Relaxed);

        if let Some(detail) = pending_error {
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
        if reached_snapshot_eof || incomplete_tail {
            break 'pass;
        }
    }

    if finalize && reached_snapshot_eof && !cancelled {
        let final_size = match std::fs::metadata(&stdout_path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let detail = format!(
                    "TRANSCRIPT_SOURCE_FINAL_STAT_FAILED: path={} cursor_offset_bytes={}: {error}; remediation=restore source metadata access before marking it complete",
                    stdout_path.display(),
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
        if final_size < cursor.offset_bytes {
            let detail = format!(
                "TRANSCRIPT_SOURCE_TRUNCATED: path={} final_size_bytes={final_size} cursor_offset_bytes={}; remediation=restore the original append-only source bytes or reconcile all durable transcript rows before clearing the cursor",
                stdout_path.display(),
                cursor.offset_bytes
            );
            return Err(stick_cursor_error(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
                detail,
            ));
        }
        if final_size > cursor.offset_bytes {
            tracing::info!(
                code = "TRANSCRIPT_SOURCE_FINALIZATION_REBASED",
                spawn_id,
                source_path = %stdout_path.display(),
                cursor_offset_bytes = cursor.offset_bytes,
                final_size_bytes = final_size,
                "source grew after the read snapshot; completion is deferred to the next bounded pass"
            );
        } else {
            cursor.source_complete = true;
            cursor.completed_reason = Some(if completion_terminal {
                "completion_status_terminal".to_owned()
            } else {
                "finalized_at_teardown".to_owned()
            });
            cursor.updated_ts_ns = unix_time_ns_now();
            store_cursor(db, &cursor, &mut cursor_revision_sha256)?;
            if let Err(detail) = verify_transcript_cursor_boundary(db, &cursor) {
                return Err(stick_cursor_error(
                    db,
                    &mut cursor,
                    &mut cursor_revision_sha256,
                    detail,
                ));
            }
            match ensure_completed_spawn_end_state_anchored(
                db,
                &mut cursor,
                &mut cursor_revision_sha256,
            )? {
                Some(outcome) => {
                    tracing::info!(
                        code = "TRANSCRIPT_END_STATE_ANCHORED",
                        spawn_id,
                        outcome,
                        physical_rows = cursor.lines_ingested,
                        "completed transcript source rows grounded from durable terminal event"
                    );
                }
                None => {
                    tracing::warn!(
                        code = "TRANSCRIPT_END_STATE_ANCHOR_DEFERRED",
                        spawn_id,
                        physical_rows = cursor.lines_ingested,
                        "completed transcript source has no durable terminal agent event yet; terminal event writer will anchor transcripts when it arrives"
                    );
                }
            }
            SOURCES_COMPLETED_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                code = "TRANSCRIPT_SOURCE_COMPLETED",
                spawn_id,
                lines = cursor.lines_ingested,
                parsed_rows = cursor.parsed_rows,
                invalid_rows = cursor.invalid_rows,
                physical_rows = cursor.lines_ingested,
                reason = cursor.completed_reason.as_deref().unwrap_or("unknown"),
                "readback=exact CF_AGENT_TRANSCRIPTS boundary rows + exact guarded CF_KV cursor edge=source_complete"
            );
        }
    }

    Ok(SpawnIngestOutcome {
        new_parsed_rows: new_parsed,
        new_invalid_rows: new_invalid,
        lines_ingested_total: cursor.lines_ingested,
        source_complete: cursor.source_complete,
        cancelled,
        ..SpawnIngestOutcome::default()
    })
}

fn ensure_completed_spawn_end_state_anchored(
    db: &Db,
    cursor: &mut TranscriptCursor,
    cursor_revision_sha256: &mut Option<[u8; 32]>,
) -> Result<Option<String>, String> {
    if let Err(detail) = verify_transcript_cursor_boundary(db, cursor) {
        return Err(stick_cursor_error(
            db,
            cursor,
            cursor_revision_sha256,
            detail,
        ));
    }
    if cursor
        .end_state_anchor_rows
        .is_some_and(|rows| rows == cursor.lines_ingested)
        && cursor.end_state_anchor_outcome.is_some()
    {
        return Ok(cursor.end_state_anchor_outcome.clone());
    }

    let anchor_outcome = match anchor_spawn_end_state_from_storage(db, &cursor.spawn_id) {
        Ok(outcome) => outcome,
        Err(error) => {
            let detail = format!(
                "TRANSCRIPT_END_STATE_ANCHOR_FAILED: spawn_id={} source_path={} cursor_offset_bytes={}: {error}; remediation=repair the terminal event/transcript anchor publication and retry from the unchanged completed cursor",
                cursor.spawn_id, cursor.source_path, cursor.offset_bytes
            );
            return Err(stick_cursor_error(
                db,
                cursor,
                cursor_revision_sha256,
                detail,
            ));
        }
    };
    match anchor_outcome {
        Some(outcome) => {
            cursor.end_state_anchor_outcome = Some(outcome.to_owned());
            cursor.end_state_anchor_rows = Some(cursor.lines_ingested);
            cursor.updated_ts_ns = unix_time_ns_now();
            store_cursor(db, cursor, cursor_revision_sha256)?;
            Ok(Some(outcome.to_owned()))
        }
        None => Ok(None),
    }
}

fn verify_transcript_cursor_boundary(db: &Db, cursor: &TranscriptCursor) -> Result<(), String> {
    let counted_rows = cursor.parsed_rows.checked_add(cursor.invalid_rows).ok_or_else(|| {
        format!(
            "TRANSCRIPT_CURSOR_COUNTER_OVERFLOW: spawn_id={} parsed_rows={} invalid_rows={}; remediation=repair the corrupt cursor from physical transcript rows",
            cursor.spawn_id, cursor.parsed_rows, cursor.invalid_rows
        )
    })?;
    if counted_rows != cursor.lines_ingested {
        return Err(format!(
            "TRANSCRIPT_CURSOR_COUNTER_MISMATCH: spawn_id={} lines_ingested={} parsed_plus_invalid={counted_rows}; remediation=repair the corrupt cursor from physical transcript rows",
            cursor.spawn_id, cursor.lines_ingested
        ));
    }
    if cursor.lines_ingested == 0 {
        return Ok(());
    }

    for line_no in [1, cursor.lines_ingested] {
        let key = agent_transcript_key(&cursor.spawn_id, line_no);
        let encoded = db
            .get_cf(cf::CF_AGENT_TRANSCRIPTS, &key)
            .map_err(|error| {
                format!(
                    "TRANSCRIPT_BOUNDARY_READBACK_FAILED: spawn_id={} source_path={} line_no={line_no} key_hex={}: {error}; remediation=repair the Calyx point-read path before accepting completion",
                    cursor.spawn_id,
                    cursor.source_path,
                    synapse_storage::constellations::hex_encode(&key)
                )
            })?
            .ok_or_else(|| {
                format!(
                    "TRANSCRIPT_BOUNDARY_READBACK_MISSING: spawn_id={} source_path={} line_no={line_no} key_hex={}; remediation=restore the missing deterministic transcript row before accepting completion",
                    cursor.spawn_id,
                    cursor.source_path,
                    synapse_storage::constellations::hex_encode(&key)
                )
            })?;
        let record: AgentTranscriptRecord = decode_json(&encoded).map_err(|error| {
            format!(
                "TRANSCRIPT_BOUNDARY_READBACK_INVALID: spawn_id={} source_path={} line_no={line_no} key_hex={}: {error}; remediation=repair the corrupt transcript row before accepting completion",
                cursor.spawn_id,
                cursor.source_path,
                synapse_storage::constellations::hex_encode(&key)
            )
        })?;
        if record.spawn_id != cursor.spawn_id || record.line_no != line_no {
            return Err(format!(
                "TRANSCRIPT_BOUNDARY_IDENTITY_MISMATCH: expected_spawn_id={} actual_spawn_id={} expected_line_no={line_no} actual_line_no={}; remediation=repair the divergent deterministic transcript row before accepting completion",
                cursor.spawn_id, record.spawn_id, record.line_no
            ));
        }
        record.validate().map_err(|detail| {
            format!(
                "TRANSCRIPT_BOUNDARY_RECORD_INVALID: spawn_id={} line_no={line_no}: {detail}; remediation=repair the invalid durable transcript row before accepting completion",
                cursor.spawn_id
            )
        })?;
    }
    Ok(())
}

/// One pass over every spawn dir under `root`. Per-spawn errors are sticky
/// and already logged; the cycle continues so one corrupt spawn can never
/// stall the fleet's transcripts.
fn ingest_all_spawn_dirs_once_with_cancel(
    db: &Db,
    root: &Path,
    cancel: Option<&CancellationToken>,
) -> Value {
    CYCLES_TOTAL.fetch_add(1, Ordering::Relaxed);
    let mut dirs_seen = 0_u64;
    let mut new_rows = 0_u64;
    let mut completed = 0_u64;
    let mut errors = 0_u64;
    let mut deferred = 0_u64;
    let mut cancelled = false;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                code = "TRANSCRIPT_SPAWN_ROOT_ABSENT",
                root = %root.display(),
                "spawn root does not exist yet; physical inventory is empty"
            );
            return json!({"dirs_seen": 0, "new_rows": 0, "sources_completed": 0, "errors": 0});
        }
        Err(error) => {
            INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "TRANSCRIPT_INGEST_CYCLE_FAILED",
                root = %root.display(),
                detail = %error,
                remediation = "restore directory enumeration access; this cycle is explicitly incomplete",
                "transcript ingest cycle could not list the spawn root"
            );
            return json!({"dirs_seen": 0, "errors": 1, "error": error.to_string()});
        }
    };
    for entry_result in entries {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            break;
        }
        let entry = match entry_result {
            Ok(entry) => entry,
            Err(error) => {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TRANSCRIPT_SPAWN_DIR_ENTRY_FAILED",
                    root = %root.display(),
                    detail = %error,
                    remediation = "repair directory enumeration/permissions; this cycle is explicitly incomplete",
                    "transcript ingest could not enumerate one spawn-root entry"
                );
                continue;
            }
        };
        let name = entry.file_name();
        let Some(spawn_id) = name.to_str() else {
            errors += 1;
            INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "TRANSCRIPT_SPAWN_DIR_NAME_NOT_UTF8",
                root = %root.display(),
                name_bytes = ?name,
                remediation = "rename or remove the non-UTF-8 entry after reconciling whether it owns a transcript source",
                "spawn-root entry cannot be represented as a Synapse spawn id"
            );
            continue;
        };
        if let Err(detail) = validate_spawn_id_shape(spawn_id) {
            if spawn_id.starts_with("agent-spawn-") {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TRANSCRIPT_SPAWN_ID_INVALID",
                    root = %root.display(),
                    entry_name = spawn_id,
                    detail = %detail,
                    remediation = "rename/remove the malformed spawn entry only after reconciling whether it owns transcript bytes",
                    "spawn-root entry looks owned by Synapse but has an invalid identity"
                );
            }
            continue;
        }
        let log_dir = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors += 1;
                INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "TRANSCRIPT_SPAWN_DIR_TYPE_FAILED",
                    spawn_id,
                    path = %log_dir.display(),
                    detail = %error,
                    remediation = "repair metadata access to the spawn entry; this cycle is explicitly incomplete",
                    "transcript ingest could not determine a spawn entry's type"
                );
                continue;
            }
        };
        if !file_type.is_dir() {
            errors += 1;
            INGEST_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                code = "TRANSCRIPT_SPAWN_ENTRY_NOT_DIRECTORY",
                spawn_id,
                path = %log_dir.display(),
                remediation = "restore the Synapse spawn entry as its owned directory before ingesting transcript state",
                "valid spawn identity is not backed by a directory; this cycle is explicitly incomplete"
            );
            continue;
        }
        dirs_seen += 1;
        match ingest_spawn_dir_once_with_cancel(db, spawn_id, &log_dir, false, cancel) {
            Ok(outcome) => {
                new_rows += outcome.new_parsed_rows + outcome.new_invalid_rows;
                if outcome.source_complete && !outcome.skipped {
                    completed += 1;
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
                    code = "TRANSCRIPT_SPAWN_INGEST_FAILED",
                    spawn_id,
                    source_path = %log_dir.join("stdout.jsonl").display(),
                    detail = %detail,
                    remediation = "follow the structured error remediation; the cursor is never advanced past unverified state",
                    "one spawn transcript failed during this explicitly incomplete cycle"
                );
            }
        }
    }
    let summary = json!({
        "dirs_seen": dirs_seen,
        "new_rows": new_rows,
        "sources_completed": completed,
        "errors": errors,
        "pressure_deferred": deferred,
        "cancelled": cancelled,
    });
    if cancelled {
        tracing::info!(
            code = "TRANSCRIPT_INGEST_CYCLE_CANCELLED",
            dirs_seen,
            new_rows,
            sources_completed = completed,
            errors,
            pressure_deferred = deferred,
            "transcript ingest cycle stopped early for daemon shutdown"
        );
        return summary;
    }
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
    match finalize_spawn_transcripts_result(db, spawn_id, log_dir) {
        Ok(outcome) => {
            if outcome.source_complete {
                tracing::info!(
                    code = "TRANSCRIPT_TEARDOWN_FLUSH_OK",
                    spawn_id,
                    new_rows = outcome.new_parsed_rows + outcome.new_invalid_rows,
                    lines_total = outcome.lines_ingested_total,
                    "teardown transcript flush reached and verified the physical source boundary"
                );
            } else {
                tracing::info!(
                    code = "TRANSCRIPT_TEARDOWN_FLUSH_BOUNDED",
                    spawn_id,
                    new_rows = outcome.new_parsed_rows + outcome.new_invalid_rows,
                    lines_total = outcome.lines_ingested_total,
                    max_rows_per_pass = MAX_AGENT_TRANSCRIPT_ROWS_PER_PASS,
                    max_source_bytes_per_pass = MAX_AGENT_TRANSCRIPT_SOURCE_BYTES_PER_PASS,
                    remediation = "the periodic ingester will continue bounded finalization from the exact cursor",
                    "teardown transcript flush committed bounded progress without claiming source completion"
                );
            }
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

pub(crate) fn finalize_spawn_transcripts_result(
    db: &Db,
    spawn_id: &str,
    log_dir: &Path,
) -> Result<SpawnIngestOutcome, String> {
    ingest_spawn_dir_once(db, spawn_id, log_dir, true)
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
    // Every cycle performs synchronous filesystem and Calyx storage work. Run
    // the exact owned task on Tokio's blocking pool so a long physical scan
    // cannot starve cancellation, recorder shutdown, or the MCP dispatcher.
    //
    // Returning the spawn_blocking JoinHandle directly is intentional:
    // aborting a blocking task does not make it disappear once it has started,
    // so the HTTP owner ledger continues to reflect the physical worker until
    // its cooperative cancellation checks actually reach a terminal join.
    let runtime = tokio::runtime::Handle::current();
    let handle = tokio::task::spawn_blocking(move || {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            let cancelled = runtime.block_on(async {
                tokio::select! {
                    () = cancel.cancelled() => true,
                    () = tokio::time::sleep(delay) => false,
                }
            });
            if cancelled {
                tracing::info!(
                    code = "TRANSCRIPT_INGEST_PERIODIC_STOPPED",
                    "periodic transcript ingestion stopped by daemon shutdown"
                );
                return;
            }
            run_cycle(&m3_state, &root, &cancel);
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

fn run_cycle(m3_state: &Arc<Mutex<M3State>>, root: &Path, cancel: &CancellationToken) {
    if cancel.is_cancelled() {
        tracing::info!(
            code = "TRANSCRIPT_INGEST_CYCLE_CANCELLED",
            "daemon shutdown cancelled transcript ingestion before storage open"
        );
        return;
    }
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
    let _summary = ingest_all_spawn_dirs_once_with_cancel(&db, root, Some(cancel));
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
        transcript_source_ts_ns(None, &cursor.spawn_id, line_no, cursor.source_epoch_unix_ms),
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
    record.ts_ns = transcript_source_ts_ns(
        Some(object),
        &cursor.spawn_id,
        line_no,
        cursor.source_epoch_unix_ms,
    );
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

pub(crate) fn transcript_source_ts_ns(
    object: Option<&Map<String, Value>>,
    source_id: &str,
    line_no: u64,
    source_epoch_unix_ms: Option<u64>,
) -> u64 {
    let Some(object) = object else {
        return stable_transcript_ts_ns(source_id, line_no, source_epoch_unix_ms);
    };
    let value = Value::Object(object.clone());
    if let Some(ts_ns) = find_named_u64(&value, TIMESTAMP_NS_FIELDS) {
        return ts_ns;
    }
    if let Some(ts_ms) = find_named_u64(&value, TIMESTAMP_MS_FIELDS) {
        return timestamp_ms_with_line_offset(ts_ms, line_no);
    }
    if let Some(ts_ms) = find_named_rfc3339_ms(&value) {
        return timestamp_ms_with_line_offset(ts_ms, line_no);
    }
    if let Some(ts_ms) = find_named_uuid_v7_ms(&value) {
        return timestamp_ms_with_line_offset(ts_ms, line_no);
    }
    stable_transcript_ts_ns(source_id, line_no, source_epoch_unix_ms)
}

const TIMESTAMP_NS_FIELDS: &[&str] = &[
    "ts_ns",
    "timestamp_ns",
    "timestampUnixNs",
    "timestamp_unix_ns",
    "created_unix_ns",
    "createdAtNs",
    "startedAtNs",
    "completedAtNs",
    "updatedAtNs",
];
const TIMESTAMP_MS_FIELDS: &[&str] = &[
    "ts_unix_ms",
    "timestamp_ms",
    "timestampUnixMs",
    "timestamp_unix_ms",
    "created_unix_ms",
    "createdAtMs",
    "startedAtMs",
    "completedAtMs",
    "updatedAtMs",
];
const TIMESTAMP_RFC3339_FIELDS: &[&str] = &[
    "timestamp",
    "created_at",
    "createdAt",
    "startedAt",
    "completedAt",
    "updatedAt",
    "time",
];
const UUID_V7_TIME_FIELDS: &[&str] = &[
    "id",
    "threadId",
    "thread_id",
    "turnId",
    "turn_id",
    "itemId",
    "item_id",
    "message_id",
    "sessionId",
    "session_id",
    "spawn_id",
    "spawnId",
];

fn timestamp_ms_with_line_offset(ts_ms: u64, line_no: u64) -> u64 {
    ts_ms
        .saturating_mul(NS_PER_MS)
        .saturating_add(line_no % TIMESTAMP_LINE_OFFSET_NS)
}

fn stable_transcript_ts_ns(
    source_id: &str,
    line_no: u64,
    source_epoch_unix_ms: Option<u64>,
) -> u64 {
    let ts_ms = source_epoch_unix_ms
        .or_else(|| uuid_v7_unix_ms(source_id))
        .unwrap_or_else(|| stable_source_epoch_ms(source_id));
    timestamp_ms_with_line_offset(ts_ms, line_no)
}

fn stable_source_epoch_ms(source_id: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(source_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    STABLE_TS_BASE_MS + (u64::from_be_bytes(bytes) % STABLE_TS_SPAN_MS)
}

fn find_named_u64(value: &Value, names: &[&str]) -> Option<u64> {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                if field_matches(name, names)
                    && let Some(number) = value_as_u64(value)
                {
                    return Some(number);
                }
                if let Some(found) = find_named_u64(value, names) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(|value| find_named_u64(value, names)),
        _ => None,
    }
}

fn find_named_rfc3339_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                if field_matches(name, TIMESTAMP_RFC3339_FIELDS)
                    && let Some(text) = value.as_str()
                    && let Some(ms) = parse_rfc3339_unix_ms(text)
                {
                    return Some(ms);
                }
                if let Some(found) = find_named_rfc3339_ms(value) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(find_named_rfc3339_ms),
        _ => None,
    }
}

fn find_named_uuid_v7_ms(value: &Value) -> Option<u64> {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                if field_matches(name, UUID_V7_TIME_FIELDS)
                    && let Some(text) = value.as_str()
                    && let Some(ms) = uuid_v7_unix_ms(text)
                {
                    return Some(ms);
                }
                if let Some(found) = find_named_uuid_v7_ms(value) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(find_named_uuid_v7_ms),
        _ => None,
    }
}

fn field_matches(name: &str, candidates: &[&str]) -> bool {
    candidates.contains(&name)
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn parse_rfc3339_unix_ms(text: &str) -> Option<u64> {
    let millis = DateTime::parse_from_rfc3339(text).ok()?.timestamp_millis();
    u64::try_from(millis).ok()
}

fn uuid_v7_unix_ms(value: &str) -> Option<u64> {
    let mut candidate = value.trim();
    for prefix in ["agent-spawn-", "ambient-claude-", "msg_"] {
        if let Some(stripped) = candidate.strip_prefix(prefix) {
            candidate = stripped;
        }
    }
    let hex = candidate
        .chars()
        .filter(|ch| *ch != '-')
        .take(32)
        .collect::<String>();
    if hex.len() < 13 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    if !hex
        .as_bytes()
        .get(12)
        .is_some_and(|version| *version == b'7')
    {
        return None;
    }
    u64::from_str_radix(&hex[..12], 16).ok()
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
