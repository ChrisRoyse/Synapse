//! `CF_AGENT_EVENTS` journal writer (#897).
//!
//! One row per agent lifecycle/telemetry event, keyed `(ts_ns, seq)` through
//! [`synapse_storage::agent_events`]. Writers: HTTP session store (session
//! initialized/restored/deleted), session lifecycle teardown (exited),
//! `act_spawn_agent` (spawn requested/ready/failed), the agent mailbox
//! (message sent/received), the input-lease tools (acquired/released), and
//! the push-telemetry ingress (#899, [`super::agent_event_ingress`]) through
//! which spawned agents self-report turn/tool-call/attention events.
//!
//! # Durability contract (#897 acceptance)
//!
//! [`record_agent_event`] uses `Db::put_batch`, which returns only after
//! the row reaches the Calyx vault and its WAL. [`record_agent_event_durable`]
//! additionally calls `Db::flush()` at terminal lifecycle boundaries
//! (exited, spawn failure, session deleted).
//!
//! # Failure contract
//!
//! A journal write failure is never swallowed: it logs a structured
//! `AGENT_EVENT_WRITE_FAILED` error with the full event context and is
//! returned to the caller. Tool handlers journal *after* the primary state
//! mutation commits (except inbox drains, which journal *before* deleting
//! rows so a failure can never lose messages); their errors carry
//! `operation_committed` so callers know whether the primary effect stands.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU32, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::model::ErrorCode;
use serde_json::{Value, json};
use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord, AgentTranscriptRecord};
use synapse_storage::{
    Db, GroundingAnchor, GroundingAnchorSource, StorageError, StorageResult,
    agent_events::agent_event_key, agent_transcripts::agent_transcript_spawn_prefix, cf,
    decode_json, encode_json,
};

use crate::m3::grounding::{self, SOURCE_AGENT_EVENT};

use super::ErrorData;
use super::session_registry::{SessionRegistry, SharedSessionRegistry, unix_time_ms_now};

/// Hard cap on one encoded journal row. Agent events are bounded metadata;
/// anything larger indicates a writer leaking content into the journal.
pub(crate) const MAX_AGENT_EVENT_VALUE_BYTES: usize = 16 * 1024;

/// Process-wide tie-breaker for same-nanosecond events. Ordering authority
/// within one clock tick; wraps harmlessly because `ts_ns` dominates the key.
static NEXT_AGENT_EVENT_SEQ: AtomicU32 = AtomicU32::new(0);

static SESSION_REGISTRY_ACTIVITY_SINK: OnceLock<Mutex<Option<Weak<Mutex<SessionRegistry>>>>> =
    OnceLock::new();

/// Physical readback of one persisted journal row.
#[derive(Clone, Debug)]
pub(crate) struct AgentEventWriteReadback {
    pub ts_ns: u64,
    pub seq: u32,
    pub value_len_bytes: usize,
}

/// Current unix time in nanoseconds. A clock before the epoch yields 0,
/// which [`AgentEventRecord::validate`] refuses — the failure surfaces
/// instead of journaling rows the TTL filter could never expire correctly.
pub(crate) fn unix_time_ns_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
        .unwrap_or_default()
}

pub(crate) fn install_session_registry_activity_sink(registry: SharedSessionRegistry) {
    let slot = SESSION_REGISTRY_ACTIVITY_SINK.get_or_init(|| Mutex::new(None));
    match slot.lock() {
        Ok(mut guard) => {
            // The process-global projection hook must not keep a failed or
            // stopped daemon's session/service graph alive. Each event upgrades
            // the current generation only while applying its refresh.
            *guard = Some(Arc::downgrade(&registry));
            tracing::info!(
                code = "AGENT_EVENT_SESSION_REGISTRY_ACTIVITY_SINK_INSTALLED",
                "agent activity rows will refresh SessionRegistry last_seen"
            );
        }
        Err(_poisoned) => {
            tracing::error!(
                code = "AGENT_EVENT_SESSION_LAST_SEEN_REFRESH_FAILED",
                "could not install session-registry activity sink because the sink lock is poisoned"
            );
        }
    }
}

/// Validates, encodes, and enqueues one event row (batched write path).
///
/// # Errors
///
/// Returns [`StorageError::WriteFailed`] when the record fails validation,
/// exceeds [`MAX_AGENT_EVENT_VALUE_BYTES`], or the storage batcher rejects
/// the write. Every failure is also logged with `AGENT_EVENT_WRITE_FAILED`.
pub(crate) fn record_agent_event(
    db: &Db,
    record: &AgentEventRecord,
) -> StorageResult<AgentEventWriteReadback> {
    let mut readbacks = record_agent_events(db, std::slice::from_ref(record))?;
    readbacks.pop().ok_or_else(|| StorageError::WriteFailed {
        cf_name: cf::CF_AGENT_EVENTS.to_owned(),
        detail: "AGENT_EVENT_WRITE_FAILED: single-record write returned no readback".to_owned(),
    })
}

/// Validates, encodes, and enqueues a batch of event rows in one storage
/// batch. All-or-nothing: any invalid record refuses the whole batch before
/// anything is written.
///
/// This is also the projection choke point for the #898 agent state machine:
/// after the rows commit, they feed [`super::agent_state`], so every journal
/// writer drives lifecycle states and none can bypass them.
///
/// # Errors
///
/// Returns [`StorageError::WriteFailed`] under the same conditions as
/// [`record_agent_event`].
pub(crate) fn record_agent_events(
    db: &Db,
    records: &[AgentEventRecord],
) -> StorageResult<Vec<AgentEventWriteReadback>> {
    let readbacks = record_agent_events_unobserved(db, records)?;
    super::agent_state::observe_recorded_events(db, records);
    refresh_installed_session_registry_activity(records);
    Ok(readbacks)
}

fn refresh_installed_session_registry_activity(records: &[AgentEventRecord]) {
    let Some(registry) = installed_session_registry_activity_sink() else {
        return;
    };
    let refreshed =
        refresh_session_registry_activity_from_agent_events(&registry, records, unix_time_ms_now());
    if !refreshed.is_empty() {
        tracing::debug!(
            code = "AGENT_EVENT_SESSION_LAST_SEEN_REFRESHED",
            refreshed_session_count = refreshed.len(),
            session_ids = ?refreshed,
            "readback=SessionRegistry edge=agent_activity_heartbeat"
        );
    }
}

fn installed_session_registry_activity_sink() -> Option<SharedSessionRegistry> {
    let slot = SESSION_REGISTRY_ACTIVITY_SINK.get()?;
    match slot.lock() {
        Ok(guard) => guard.as_ref().and_then(Weak::upgrade),
        Err(_poisoned) => {
            tracing::error!(
                code = "AGENT_EVENT_SESSION_LAST_SEEN_REFRESH_FAILED",
                "could not read session-registry activity sink because the sink lock is poisoned"
            );
            None
        }
    }
}

pub(crate) fn refresh_session_registry_activity_from_agent_events(
    registry: &SharedSessionRegistry,
    records: &[AgentEventRecord],
    now_unix_ms: u64,
) -> Vec<String> {
    let mut guard = match registry.lock() {
        Ok(guard) => guard,
        Err(_poisoned) => {
            tracing::error!(
                code = "AGENT_EVENT_SESSION_LAST_SEEN_REFRESH_FAILED",
                "could not lock session registry while refreshing activity heartbeat"
            );
            return Vec::new();
        }
    };
    let mut refreshed = BTreeSet::new();
    let mut activity_record_count = 0usize;
    for record in records {
        if !agent_event_counts_as_session_activity(record.kind) {
            continue;
        }
        activity_record_count += 1;
        refreshed.extend(guard.record_agent_activity(
            record.session_id.as_deref(),
            record.spawn_id.as_deref(),
            now_unix_ms,
        ));
    }
    let refreshed: Vec<String> = refreshed.into_iter().collect();
    if activity_record_count > 0 {
        tracing::debug!(
            code = "AGENT_EVENT_SESSION_LAST_SEEN_REFRESH_READBACK",
            activity_record_count,
            refreshed_session_count = refreshed.len(),
            session_ids = ?refreshed,
            "readback=SessionRegistry edge=agent_activity_heartbeat"
        );
    }
    refreshed
}

fn agent_event_counts_as_session_activity(kind: AgentEventKind) -> bool {
    matches!(
        kind,
        AgentEventKind::ToolCallStarted
            | AgentEventKind::ToolCallFinished
            | AgentEventKind::TurnStarted
            | AgentEventKind::TurnFinished
            | AgentEventKind::MessageSent
            | AgentEventKind::MessageReceived
    )
}

/// The raw journal write path, without the state-machine projection. Only
/// the state machine itself uses this directly (its own transition rows must
/// not re-enter the reducer).
pub(crate) fn record_agent_events_unobserved(
    db: &Db,
    records: &[AgentEventRecord],
) -> StorageResult<Vec<AgentEventWriteReadback>> {
    let mut rows = Vec::with_capacity(records.len());
    let mut constellation_rows = Vec::with_capacity(records.len());
    let mut readbacks = Vec::with_capacity(records.len());
    for record in records {
        let encoded = validate_and_encode(record).inspect_err(|error| {
            tracing::error!(
                code = "AGENT_EVENT_WRITE_FAILED",
                kind = ?record.kind,
                session_id = ?record.session_id,
                spawn_id = ?record.spawn_id,
                reason_code = ?record.reason_code,
                detail = %error,
                "agent event refused before write"
            );
        })?;
        let seq = NEXT_AGENT_EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
        readbacks.push(AgentEventWriteReadback {
            ts_ns: record.ts_ns,
            seq,
            value_len_bytes: encoded.len(),
        });
        let key = agent_event_key(record.ts_ns, seq);
        constellation_rows.push((key.clone(), encoded.clone()));
        rows.push((key, encoded));
    }
    if rows.is_empty() {
        return Ok(readbacks);
    }
    db.put_batch(cf::CF_AGENT_EVENTS, rows)
        .inspect_err(|error| {
            tracing::error!(
                code = "AGENT_EVENT_WRITE_FAILED",
                record_count = records.len(),
                first_kind = ?records.first().map(|record| record.kind),
                detail = %error,
                "agent event batch enqueue failed"
            );
        })?;
    for ((record, readback), (source_key, raw_bytes)) in
        records.iter().zip(&readbacks).zip(&constellation_rows)
    {
        let constellation = db
            .put_agent_event_constellation(source_key, raw_bytes, record)
            .inspect_err(|error| {
                tracing::error!(
                    code = "CALYX_AGENT_EVENT_CONSTELLATION_MEASUREMENT_FAILED",
                    kind = ?record.kind,
                    ts_ns = readback.ts_ns,
                    seq = readback.seq,
                    source_key_hex = %synapse_storage::constellations::hex_encode(source_key),
                    detail = %error,
                    "agent event row was written but native Calyx constellation measurement failed"
                );
            })?;
        tracing::debug!(
            code = "AGENT_EVENT_RECORDED",
            kind = ?record.kind,
            ts_ns = readback.ts_ns,
            seq = readback.seq,
            session_id = ?record.session_id,
            spawn_id = ?record.spawn_id,
            value_len_bytes = readback.value_len_bytes,
            constellation_panel = constellation.panel_name,
            constellation_disposition = constellation.disposition.as_str(),
            constellation_cx_id = %constellation.cx_id,
            "readback=CF_AGENT_EVENTS edge=enqueued"
        );
    }
    anchor_agent_event_outcomes(db, records, &constellation_rows)?;
    Ok(readbacks)
}

fn anchor_agent_event_outcomes(
    db: &Db,
    records: &[AgentEventRecord],
    source_rows: &[(Vec<u8>, Vec<u8>)],
) -> StorageResult<()> {
    for (record, (source_key, source_value)) in records.iter().zip(source_rows) {
        if record.kind == AgentEventKind::ToolCallFinished {
            let error_present = tool_call_error_present(record);
            put_agent_grounding_anchor(
                db,
                cf::CF_AGENT_EVENTS,
                source_key,
                source_value,
                grounding::bool_anchor(
                    "synapse:agent_tool_call_success",
                    !error_present,
                    SOURCE_AGENT_EVENT,
                    grounding::observed_at_ms_from_ns(record.ts_ns),
                ),
                "agent tool-call outcome anchor",
            )?;
            tracing::debug!(
                code = "AGENT_TOOL_CALL_OUTCOME_ANCHORED",
                ts_ns = record.ts_ns,
                session_id = ?record.session_id,
                spawn_id = ?record.spawn_id,
                tool_name = ?record.attributes.tool_name,
                error_present,
                source_key_hex = %synapse_storage::constellations::hex_encode(source_key),
                "tool-call error presence grounded on agent-event constellation"
            );
        }

        let Some(_outcome) = terminal_agent_outcome(record) else {
            continue;
        };
        let Some(spawn_id) = nonblank_option(record.spawn_id.as_deref()) else {
            tracing::warn!(
                code = "AGENT_END_STATE_ANCHOR_SKIPPED_NO_SPAWN_ID",
                ts_ns = record.ts_ns,
                kind = ?record.kind,
                end_state = ?record.end_state,
                "terminal agent event has no spawn_id, so no spawn event/transcript constellation set can be grounded"
            );
            continue;
        };
        finalize_spawn_transcripts_for_terminal_event(db, spawn_id, record)?;
        let Some(canonical) = canonical_spawn_terminal_event(db, spawn_id)? else {
            continue;
        };
        anchor_spawn_terminal_event_rows(db, spawn_id)?;
        anchor_spawn_transcript_rows(db, spawn_id, canonical.outcome, canonical.observed_ts_ns)?;
    }
    Ok(())
}

pub(crate) fn anchor_spawn_end_state_from_storage(
    db: &Db,
    spawn_id: &str,
) -> StorageResult<Option<&'static str>> {
    let Some(canonical) = canonical_spawn_terminal_event(db, spawn_id)? else {
        return Ok(None);
    };
    anchor_spawn_terminal_event_rows(db, spawn_id)?;
    anchor_spawn_transcript_rows(db, spawn_id, canonical.outcome, canonical.observed_ts_ns)?;
    Ok(Some(canonical.outcome))
}

#[derive(Clone, Copy, Debug)]
struct SpawnTerminalObservation {
    outcome: &'static str,
    observed_ts_ns: u64,
}

fn canonical_spawn_terminal_event(
    db: &Db,
    spawn_id: &str,
) -> StorageResult<Option<SpawnTerminalObservation>> {
    let mut canonical: Option<SpawnTerminalObservation> = None;
    for (key, value) in db.scan_cf(cf::CF_AGENT_EVENTS)? {
        let record: AgentEventRecord = decode_json(&value)?;
        if nonblank_option(record.spawn_id.as_deref()) != Some(spawn_id) {
            continue;
        }
        let Some(outcome) = terminal_agent_outcome(&record) else {
            continue;
        };
        let (key_ts_ns, _seq) = synapse_storage::agent_events::decode_agent_event_key(&key)
            .map_err(|error| StorageError::ReadFailed {
                cf_name: cf::CF_AGENT_EVENTS.to_owned(),
                detail: format!("agent end-state anchor scan found corrupt event key: {error}"),
            })?;
        if key_ts_ns != record.ts_ns {
            return Err(StorageError::ReadFailed {
                cf_name: cf::CF_AGENT_EVENTS.to_owned(),
                detail: format!(
                    "agent end-state anchor scan found event key/record timestamp drift for spawn {spawn_id}: key_ts_ns={key_ts_ns} record_ts_ns={}",
                    record.ts_ns
                ),
            });
        }
        match canonical {
            Some(existing) if existing.outcome != outcome => {
                return Err(StorageError::ReadFailed {
                    cf_name: cf::CF_AGENT_EVENTS.to_owned(),
                    detail: format!(
                        "agent end-state anchor scan found conflicting terminal outcomes for spawn {spawn_id}: {} vs {outcome}",
                        existing.outcome
                    ),
                });
            }
            Some(existing) if existing.observed_ts_ns <= record.ts_ns => {}
            _ => {
                canonical = Some(SpawnTerminalObservation {
                    outcome,
                    observed_ts_ns: record.ts_ns,
                });
            }
        }
    }
    Ok(canonical)
}

fn anchor_spawn_terminal_event_rows(db: &Db, spawn_id: &str) -> StorageResult<()> {
    let mut anchored = 0_usize;
    for (source_key, source_value) in db.scan_cf(cf::CF_AGENT_EVENTS)? {
        let record: AgentEventRecord = decode_json(&source_value)?;
        if nonblank_option(record.spawn_id.as_deref()) != Some(spawn_id) {
            continue;
        }
        let Some(outcome) = terminal_agent_outcome(&record) else {
            continue;
        };
        db.put_agent_event_constellation(&source_key, &source_value, &record)?;
        put_agent_grounding_anchor(
            db,
            cf::CF_AGENT_EVENTS,
            &source_key,
            &source_value,
            grounding::enum_anchor(
                "synapse:agent_end_state",
                outcome,
                SOURCE_AGENT_EVENT,
                grounding::observed_at_ms_from_ns(record.ts_ns),
            ),
            "agent end-state event anchor",
        )?;
        anchored = anchored.saturating_add(1);
    }
    tracing::info!(
        code = "AGENT_END_STATE_EVENT_ROWS_ANCHORED",
        spawn_id,
        event_rows = anchored,
        "terminal agent outcomes grounded on terminal spawn event constellations"
    );
    Ok(())
}

fn anchor_spawn_transcript_rows(
    db: &Db,
    spawn_id: &str,
    outcome: &'static str,
    observed_ts_ns: u64,
) -> StorageResult<()> {
    let mut anchor_sources = Vec::new();
    let prefix = agent_transcript_spawn_prefix(spawn_id);
    for (source_key, source_value) in db.scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &prefix)? {
        let record: AgentTranscriptRecord = decode_json(&source_value)?;
        db.put_agent_transcript_constellation(&source_key, &source_value, &record)?;
        anchor_sources.push(GroundingAnchorSource {
            source_cf: cf::CF_AGENT_TRANSCRIPTS,
            source_key,
            raw_bytes: source_value,
            anchor: grounding::enum_anchor(
                "synapse:agent_end_state",
                outcome,
                SOURCE_AGENT_EVENT,
                grounding::observed_at_ms_from_ns(observed_ts_ns),
            ),
        });
    }
    let requested = anchor_sources.len();
    let report = if anchor_sources.is_empty() {
        None
    } else {
        let payload = transcript_end_state_anchor_batch_payload(
            spawn_id,
            outcome,
            observed_ts_ns,
            &anchor_sources,
        );
        Some(
            db.put_grounding_anchors_for_sources(anchor_sources, &payload)
                .map_err(|error| StorageError::WriteFailed {
                    cf_name: cf::CF_AGENT_TRANSCRIPTS.to_owned(),
                    detail: format!("agent end-state transcript anchor batch failed: {error}"),
                })?,
        )
    };
    if let Some(report) = &report {
        if report.readback_exact_match_count != u64::try_from(requested).unwrap_or(u64::MAX) {
            return Err(StorageError::WriteFailed {
                cf_name: cf::CF_AGENT_TRANSCRIPTS.to_owned(),
                detail: format!(
                    "agent end-state transcript anchor batch readback mismatch: requested={requested} readback_exact_match_count={}",
                    report.readback_exact_match_count
                ),
            });
        }
    }
    tracing::info!(
        code = "AGENT_END_STATE_TRANSCRIPT_ROWS_ANCHORED",
        spawn_id,
        outcome,
        transcript_rows = requested,
        written_anchor_count = report.as_ref().map(|report| report.written_anchor_count),
        existing_anchor_count = report.as_ref().map(|report| report.existing_anchor_count),
        readback_exact_match_count = report
            .as_ref()
            .map(|report| report.readback_exact_match_count),
        ledger_seq = ?report.as_ref().and_then(|report| report.ledger_seq),
        "terminal agent outcome grounded on all current spawn transcript constellations"
    );
    Ok(())
}

fn transcript_end_state_anchor_batch_payload(
    spawn_id: &str,
    outcome: &str,
    observed_ts_ns: u64,
    sources: &[GroundingAnchorSource],
) -> Value {
    let rows = sources
        .iter()
        .map(|source| {
            json!({
                "source_key_sha256": synapse_storage::constellations::sha256_hex(&source.source_key),
                "source_value_sha256": synapse_storage::constellations::sha256_hex(&source.raw_bytes),
                "anchor_kind_sha256": synapse_storage::constellations::sha256_hex(source.anchor.kind_label.as_bytes()),
                "anchor_source_sha256": synapse_storage::constellations::sha256_hex(source.anchor.source.as_bytes()),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schema": "synapse.grounding_anchor_batch.v1",
        "source_cf": cf::CF_AGENT_TRANSCRIPTS,
        "spawn_id_sha256": synapse_storage::constellations::sha256_hex(spawn_id.as_bytes()),
        "outcome_sha256": synapse_storage::constellations::sha256_hex(outcome.as_bytes()),
        "observed_at_ms": grounding::observed_at_ms_from_ns(observed_ts_ns),
        "source_count": sources.len(),
        "sources": rows,
    })
}

fn finalize_spawn_transcripts_for_terminal_event(
    db: &Db,
    spawn_id: &str,
    record: &AgentEventRecord,
) -> StorageResult<()> {
    let Some(log_dir) = terminal_event_log_dir(record) else {
        return Ok(());
    };
    let log_dir = Path::new(&log_dir);
    if !log_dir.is_dir() {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_TRANSCRIPTS.to_owned(),
            detail: format!(
                "terminal agent event for spawn {spawn_id} named missing transcript log dir {}",
                log_dir.display()
            ),
        });
    }
    let outcome = super::agent_transcripts::finalize_spawn_transcripts_result(db, spawn_id, log_dir)
        .map_err(|error| StorageError::WriteFailed {
            cf_name: cf::CF_AGENT_TRANSCRIPTS.to_owned(),
            detail: format!(
                "terminal agent event for spawn {spawn_id} could not finalize transcript rows before anchoring: {error}"
            ),
        })?;
    tracing::info!(
        code = "AGENT_END_STATE_TRANSCRIPT_FINALIZED_BEFORE_ANCHOR",
        spawn_id,
        new_rows = outcome.new_parsed_rows + outcome.new_invalid_rows,
        lines_total = outcome.lines_ingested_total,
        skipped = outcome.skipped,
        source_complete = outcome.source_complete,
        "terminal agent event finalized transcript source before grounding transcript rows"
    );
    Ok(())
}

fn terminal_event_log_dir(record: &AgentEventRecord) -> Option<String> {
    record
        .payload
        .get("log_dir")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            let completion_path = record
                .payload
                .get("completion_status_path")
                .and_then(Value::as_str)?;
            Path::new(completion_path)
                .parent()
                .map(|path| path.display().to_string())
        })
}

fn put_agent_grounding_anchor(
    db: &Db,
    source_cf: &'static str,
    source_key: &[u8],
    source_value: &[u8],
    anchor: GroundingAnchor,
    context: &'static str,
) -> StorageResult<()> {
    let payload = grounding::anchor_ledger_payload(source_cf, source_key, source_value, &anchor);
    db.put_grounding_anchor_for_source(source_cf, source_key, source_value, anchor, &payload)
        .map(|_report| ())
        .map_err(|error| StorageError::WriteFailed {
            cf_name: source_cf.to_owned(),
            detail: format!("{context} failed: {error}"),
        })
}

fn terminal_agent_outcome(record: &AgentEventRecord) -> Option<&'static str> {
    match record.kind {
        AgentEventKind::Killed => Some("killed"),
        AgentEventKind::Exited => match record.end_state {
            Some(AgentEndState::Success) => Some("completed"),
            Some(AgentEndState::Error | AgentEndState::Indeterminate) | None => Some("failed"),
        },
        _ => None,
    }
}

fn tool_call_error_present(record: &AgentEventRecord) -> bool {
    record
        .attributes
        .error_type
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || matches!(record.end_state, Some(AgentEndState::Error))
        || payload_has_error(&record.payload)
}

fn payload_has_error(payload: &Value) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    object
        .get("error")
        .is_some_and(|value| !value.is_null() && value != "")
        || object.get("is_error").and_then(Value::as_bool) == Some(true)
        || object.get("ok").and_then(Value::as_bool) == Some(false)
        || object.get("success").and_then(Value::as_bool) == Some(false)
        || object
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| {
                let status = status.trim().to_ascii_lowercase();
                matches!(status.as_str(), "error" | "failed" | "failure")
            })
        || object
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|exit_code| exit_code != 0)
}

fn nonblank_option(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then_some(value)
    })
}

/// [`record_agent_event`] plus an explicit `Db::flush()` so the row is
/// readable and crash-durable before this returns. Reserved for terminal
/// lifecycle events (exited, killed, spawn failure, session deleted).
///
/// # Errors
///
/// Returns [`StorageError::WriteFailed`] from the write or the flush.
pub(crate) fn record_agent_event_durable(
    db: &Db,
    record: &AgentEventRecord,
) -> StorageResult<AgentEventWriteReadback> {
    let readback = record_agent_event(db, record)?;
    db.flush().inspect_err(|error| {
        tracing::error!(
            code = "AGENT_EVENT_WRITE_FAILED",
            kind = ?record.kind,
            ts_ns = readback.ts_ns,
            seq = readback.seq,
            detail = %error,
            "agent event terminal flush failed"
        );
    })?;
    Ok(readback)
}

fn validate_and_encode(record: &AgentEventRecord) -> StorageResult<Vec<u8>> {
    record
        .validate()
        .map_err(|detail| StorageError::WriteFailed {
            cf_name: cf::CF_AGENT_EVENTS.to_owned(),
            detail,
        })?;
    let encoded = encode_json(record)?;
    if encoded.len() > MAX_AGENT_EVENT_VALUE_BYTES {
        return Err(StorageError::WriteFailed {
            cf_name: cf::CF_AGENT_EVENTS.to_owned(),
            detail: format!(
                "AGENT_EVENT_INVALID: encoded row is {} bytes, cap is {MAX_AGENT_EVENT_VALUE_BYTES}; journal rows are bounded metadata, never content",
                encoded.len()
            ),
        });
    }
    Ok(encoded)
}

/// Maps a registry `agent_kind` onto the OTel `gen_ai.provider.name`
/// well-known values. Unknown kinds stay unattributed rather than guessed.
pub(crate) fn provider_for_agent_kind(agent_kind: &str) -> Option<String> {
    match agent_kind {
        "claude" => Some("anthropic".to_owned()),
        "codex" => Some("openai".to_owned()),
        "local-model" => Some("local".to_owned()),
        _ => None,
    }
}

/// Maps a journal write failure into a tool error that states whether the
/// primary operation already committed before the journal refused.
pub(crate) fn agent_event_tool_error(
    tool: &'static str,
    error: &StorageError,
    operation_committed: bool,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} could not journal its agent event to CF_AGENT_EVENTS: {error}{}",
            if operation_committed {
                " (the underlying operation already committed; storage needs attention before its effects are auditable)"
            } else {
                ""
            }
        ),
        Some(json!({
            "code": error.code(),
            "reason": "agent_event_journal_write_failed",
            "tool": tool,
            "operation_committed": operation_committed,
        })),
    )
}
