//! Routine MCP tools (#848 `routine_mine`; #849 `routine_list`,
//! `routine_inspect`, `routine_update`) for epic #830.
//!
//! `routine_mine` runs the deterministic routine mining engine
//! ([`synapse_core::routines::mine_routines`]) over `CF_EPISODES` and
//! replaces `CF_ROUTINES` with the result in one atomic flushed batch.
//! Routines are derived state: the store always holds exactly one mining
//! run's complete output, so re-mining is idempotent by construction.
//!
//! `CF_ROUTINE_STATE` (#849) is the opposite: operator-owned lifecycle
//! state (candidate → confirmed → disabled/archived, labels, transition
//! audit trail, confidence history) keyed by the same stable routine id.
//! The miner reconciles it after every replace-all — creating candidate
//! rows for new routines, appending confidence change-points, and flagging
//! rows whose routine vanished — but NEVER changes a lifecycle the operator
//! set, so a disabled routine stays disabled across every re-mine.
//!
//! The same mining entry point serves the on-demand MCP tool and the
//! periodic in-daemon batch job ([`super::routine_miner_job`]); a
//! process-wide mining lock serializes the two so concurrent replace-alls
//! can never interleave.
//!
//! Failure policy: disk pressure refusal, undecodable derived rows, scan
//! budget exhaustion, and engine errors are loud and structured. The tool
//! never replaces rows it could not fully re-derive. Lifecycle writes are
//! synchronous flushed batches followed by a physical read-back check —
//! never the sheddable async write path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Local, TimeZone};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_core::routines::{MiningDay, RoutineMiningConfig, mine_routines};
use synapse_core::types::{
    ROUTINE_STATE_MAX_CONFIDENCE_POINTS, ROUTINE_STATE_MAX_FEEDBACK_EVENTS,
    ROUTINE_STATE_MAX_TRANSITIONS, ROUTINE_STATE_RECORD_VERSION, RoutineConfidencePoint,
    RoutineDowClass, RoutineFeedbackEvent, RoutineFeedbackOutcome, RoutineGranularity,
    RoutineLifecycle, RoutineRecord, RoutineStateAction, RoutineStateRecord, RoutineStep,
    RoutineTransition,
};
use synapse_storage::{Db, cf, decode_json, encode_json, routines as routine_codec};

use crate::m1::mcp_error;

use super::armed_routines::{
    ArmRoutineConfig, ArmedRoutineRecord, arm_routine, disarm_routine, load_armed_routine_record,
    reindex_armed_routine_schedule_due_indexes,
};
use super::episodes::{
    decode_episode_row, hex_encode, key_after, local_day_start, next_local_day_start, now_ts_ns,
};
use super::hygiene::{HygieneTaintRecord, read_taint_record_from_db};
use super::profile_authoring::{RoutineAutomationRecord, load_routine_automation_record};
use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

/// Maximum `CF_EPISODES`/`CF_ROUTINES` rows scanned per call. Exceeding it
/// is a structured error, never a partial mine: routine support counts
/// derived from a truncated episode scan would be silently wrong.
pub const MAX_SCAN_ROWS_PER_CALL: usize = 200_000;
/// Chunk size for bounded storage reads inside one call.
const SCAN_CHUNK_ROWS: usize = 4_096;
/// Upper bound for the `max_pattern_len` parameter.
pub const MAX_PATTERN_LEN_LIMIT: u32 = 12;
/// Upper bound for the `min_support_days` parameter (the mining window is
/// at most the 90-day episode retention horizon).
pub const MIN_SUPPORT_DAYS_LIMIT: u32 = 92;

/// Process-wide mining serialization: the on-demand tool and the periodic
/// job must never run replace-all concurrently.
static MINE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineMineParams {
    /// Inclusive lower bound; snapped DOWN to its local midnight. Defaults
    /// to the first episode row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Exclusive upper bound; snapped UP to the next local midnight.
    /// Defaults to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    /// Distinct-day support floor (default 3, max 92).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_support_days: Option<u32>,
    /// Episodes shorter than this are excluded (default 60000 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_episode_duration_ms: Option<u64>,
    /// Longest mined template in steps (default 6, max 12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pattern_len: Option<u32>,
    /// Mine agent-actor episodes too (default false: human routines only).
    #[serde(default)]
    pub include_agent_activity: bool,
    /// Compute everything but mutate nothing.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineMineResponse {
    /// Effective day-snapped range this call mined.
    pub range_start_ns: u64,
    pub range_end_ns: u64,
    /// `CF_EPISODES` rows examined.
    pub scanned_episode_rows: u64,
    /// Episodes fed to the engine across all days.
    pub considered_episodes: u64,
    /// Episodes that survived the eligibility filter.
    pub eligible_episodes: u64,
    pub filtered_agent_episodes: u64,
    pub filtered_short_episodes: u64,
    pub filtered_no_app_episodes: u64,
    /// Days in the window with at least one eligible episode.
    pub active_days: u32,
    /// Distinct candidate patterns tracked.
    pub candidates_evaluated: u64,
    /// New patterns ignored after the candidate cap was reached.
    pub candidates_truncated: u64,
    /// Occurrences ignored after a pattern hit its per-day cap.
    pub occurrences_skipped_over_cap: u64,
    pub clusters_rejected_low_support: u64,
    pub clusters_rejected_dispersed: u64,
    pub clusters_rejected_low_confidence: u64,
    pub candidates_rejected_as_subpattern: u64,
    pub routines_dropped_over_cap: u64,
    /// Rows written to `CF_ROUTINES` (0 on dry runs).
    pub routines_written: u64,
    /// Stale rows deleted from `CF_ROUTINES` (0 on dry runs).
    pub routines_deleted: u64,
    /// New candidate `CF_ROUTINE_STATE` rows created for routines mined for
    /// the first time (0 on dry runs).
    pub state_rows_created: u64,
    /// Existing `CF_ROUTINE_STATE` rows refreshed for re-mined routines
    /// (0 on dry runs).
    pub state_rows_updated: u64,
    /// `CF_ROUTINE_STATE` rows flagged `present_in_last_mine=false` because
    /// this run no longer derived their routine (0 on dry runs).
    pub state_rows_marked_unmined: u64,
    pub dry_run: bool,
    /// The mined routines, strongest first (full persisted records).
    pub routines: Vec<RoutineRecord>,
}

#[must_use]
pub const fn routine_mine() -> M3ToolStub {
    M3ToolStub::new("routine_mine")
}

#[must_use]
pub fn required_permissions(_params: &RoutineMineParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
}

fn internal(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail.into())
}

fn build_config(params: &RoutineMineParams) -> Result<RoutineMiningConfig, ErrorData> {
    let mut config = RoutineMiningConfig::default();
    if let Some(min_support_days) = params.min_support_days {
        if min_support_days == 0 || min_support_days > MIN_SUPPORT_DAYS_LIMIT {
            return Err(invalid(format!(
                "routine_mine min_support_days must be between 1 and {MIN_SUPPORT_DAYS_LIMIT}; \
                 got {min_support_days}"
            )));
        }
        config.min_support_days = min_support_days;
    }
    if let Some(max_pattern_len) = params.max_pattern_len {
        if max_pattern_len == 0 || max_pattern_len > MAX_PATTERN_LEN_LIMIT {
            return Err(invalid(format!(
                "routine_mine max_pattern_len must be between 1 and {MAX_PATTERN_LEN_LIMIT}; \
                 got {max_pattern_len}"
            )));
        }
        config.max_pattern_len = max_pattern_len as usize;
    }
    if let Some(min_episode_duration_ms) = params.min_episode_duration_ms {
        if min_episode_duration_ms > 86_400_000 {
            return Err(invalid(format!(
                "routine_mine min_episode_duration_ms must be at most one day (86400000); \
                 got {min_episode_duration_ms}"
            )));
        }
        config.min_episode_duration_ns = min_episode_duration_ms.saturating_mul(1_000_000);
    }
    config.include_agent_activity = params.include_agent_activity;
    Ok(config)
}

/// First decodable `CF_EPISODES` key timestamp, if any.
fn first_episode_ts(db: &Db, scanned_rows: &mut u64) -> Result<Option<u64>, ErrorData> {
    let (rows, _more) = db
        .scan_cf_from(cf::CF_EPISODES, &[], 1)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some((key, value)) = rows.first() else {
        return Ok(None);
    };
    *scanned_rows += 1;
    let (key_ts_ns, _ordinal, _record) = decode_episode_row(key, value)?;
    Ok(Some(key_ts_ns))
}

/// 0 = Monday … 6 = Sunday for a local-midnight timestamp.
fn weekday_of_day_start(day_start_ns: u64) -> Result<u8, ErrorData> {
    let ts = i64::try_from(day_start_ns).map_err(|_e| {
        internal(format!(
            "day_start_ns {day_start_ns} exceeds the representable range"
        ))
    })?;
    let weekday = Local.timestamp_nanos(ts).weekday().num_days_from_monday();
    u8::try_from(weekday).map_err(|_e| internal("weekday outside 0..=6"))
}

/// Collects episodes in `[range_start_ns, range_end_ns)` grouped into local
/// mining days, in chronological order. Fails loudly on undecodable derived
/// rows and on scan budget exhaustion — never a partial mine.
fn mining_days(
    db: &Db,
    range_start_ns: u64,
    range_end_ns: u64,
    scanned_rows: &mut u64,
) -> Result<Vec<MiningDay>, ErrorData> {
    let mut days: Vec<MiningDay> = Vec::new();
    let mut current_day_start: Option<u64> = None;
    let mut start = synapse_storage::episodes::episode_scan_start(range_start_ns);
    'scan: loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(format!(
                "ROUTINE_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} CF_EPISODES rows; \
                 pass a narrower start_ts_ns/end_ts_ns range — mining over a truncated scan \
                 would fabricate support counts"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_EPISODES, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            *scanned_rows += 1;
            let (key_ts_ns, _ordinal, record) = decode_episode_row(key, value)?;
            if key_ts_ns >= range_end_ns {
                break 'scan;
            }
            let day_start = local_day_start(record.start_ts_ns)?;
            if current_day_start != Some(day_start) {
                let day_end = next_local_day_start(day_start)?;
                let weekday = weekday_of_day_start(day_start)?;
                days.push(MiningDay {
                    day_start_ns: day_start,
                    day_end_ns: day_end,
                    weekday,
                    episodes: Vec::new(),
                });
                current_day_start = Some(day_start);
            }
            if let Some(day) = days.last_mut() {
                day.episodes.push(record);
            }
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(days)
}

/// Existing `CF_ROUTINES` keys. A malformed key in derived state we own is
/// corruption to surface, never a row to skip or silently overwrite around.
fn existing_routine_keys(db: &Db, scanned_rows: &mut u64) -> Result<Vec<Vec<u8>>, ErrorData> {
    let mut keys = Vec::new();
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(format!(
                "ROUTINE_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} CF_ROUTINES rows; \
                 the routine store should hold at most a few hundred rows — inspect CF_ROUTINES"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_ROUTINES, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, _value) in &rows {
            *scanned_rows += 1;
            routine_codec::decode_routine_key(key).map_err(|error| {
                tracing::error!(
                    code = "ROUTINE_KEY_INVALID",
                    key_hex = %hex_encode(key),
                    %error,
                    "CF_ROUTINES holds a key its codec cannot decode"
                );
                mcp_error(
                    error_codes::STORAGE_READ_FAILED,
                    format!(
                        "ROUTINE_KEY_INVALID in CF_ROUTINES at {}: {error}; refusing to \
                         replace a store containing keys this codec cannot account for",
                        hex_encode(key)
                    ),
                )
            })?;
            keys.push(key.clone());
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(keys)
}

/// Actor recorded on state transitions performed by the mining engine.
pub const MINER_ACTOR: &str = "miner";

/// Decodes one `CF_ROUTINE_STATE` row, failing loudly on key/value/id
/// mismatches: this CF holds operator decisions, so corruption is surfaced,
/// never skipped.
fn decode_state_row(key: &[u8], value: &[u8]) -> Result<RoutineStateRecord, ErrorData> {
    let routine_id = routine_codec::decode_routine_state_key(key).map_err(|error| {
        tracing::error!(
            code = "ROUTINE_STATE_KEY_INVALID",
            key_hex = %hex_encode(key),
            %error,
            "CF_ROUTINE_STATE holds a key its codec cannot decode"
        );
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "ROUTINE_STATE_KEY_INVALID in CF_ROUTINE_STATE at {}: {error}; this CF holds \
                 operator lifecycle decisions — inspect the row before removing anything",
                hex_encode(key)
            ),
        )
    })?;
    let record = decode_json::<RoutineStateRecord>(value).map_err(|error| {
        tracing::error!(
            code = "ROUTINE_STATE_ROW_DECODE_FAILED",
            key_hex = %hex_encode(key),
            %error,
            "CF_ROUTINE_STATE holds a value that does not decode as a RoutineStateRecord"
        );
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_STATE_ROW_DECODE_FAILED in CF_ROUTINE_STATE at {}: {error}; this CF \
                 holds operator lifecycle decisions — inspect the row before removing anything",
                hex_encode(key)
            ),
        )
    })?;
    // Forward-compatible read: older rows deserialize via the field-level
    // serde defaults (e.g. a v1 row decodes as "no feedback yet" — see #856)
    // and are upgraded to the current version the next time they are written
    // (feedback / lifecycle update / miner reconcile). Only a row written by a
    // NEWER binary than this one is unsupported — silently truncating its
    // unknown fields would lose operator state, so that is the loud refusal.
    if record.record_version > ROUTINE_STATE_RECORD_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_STATE_VERSION_UNSUPPORTED in CF_ROUTINE_STATE at {routine_id}: \
                 record_version {} is newer than this binary supports \
                 ({ROUTINE_STATE_RECORD_VERSION}); upgrade the daemon — do not downgrade-write \
                 this row or operator state will be lost",
                record.record_version
            ),
        ));
    }
    if record.routine_id != routine_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_STATE_ID_MISMATCH in CF_ROUTINE_STATE: row key {routine_id} holds a \
                 record claiming routine_id {}",
                record.routine_id
            ),
        ));
    }
    Ok(record)
}

/// Loads every `CF_ROUTINE_STATE` row, budget-guarded.
fn load_all_state_rows(
    db: &Db,
    scanned_rows: &mut u64,
) -> Result<BTreeMap<String, RoutineStateRecord>, ErrorData> {
    let mut rows_out: BTreeMap<String, RoutineStateRecord> = BTreeMap::new();
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(format!(
                "ROUTINE_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} CF_ROUTINE_STATE \
                 rows; the state store should hold at most a few hundred rows — inspect \
                 CF_ROUTINE_STATE"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_ROUTINE_STATE, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            *scanned_rows += 1;
            let record = decode_state_row(key, value)?;
            rows_out.insert(record.routine_id.clone(), record);
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(rows_out)
}

/// Point lookup of one `CF_ROUTINE_STATE` row by routine id.
pub(crate) fn load_state_row(
    db: &Db,
    routine_id: &str,
) -> Result<Option<RoutineStateRecord>, ErrorData> {
    let key =
        routine_codec::routine_state_key(routine_id).map_err(|error| invalid(error.to_string()))?;
    let rows = db
        .scan_cf_prefix(cf::CF_ROUTINE_STATE, &key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some((row_key, value)) = rows.first() else {
        return Ok(None);
    };
    if rows.len() > 1 || row_key != &key {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_STATE_KEY_COLLISION in CF_ROUTINE_STATE: prefix lookup for \
                 {routine_id} returned {} rows, first key {}",
                rows.len(),
                hex_encode(row_key)
            ),
        ));
    }
    decode_state_row(row_key, value).map(Some)
}

/// Point lookup of one `CF_ROUTINES` row by routine id.
pub(crate) fn load_routine_record(
    db: &Db,
    routine_id: &str,
) -> Result<Option<RoutineRecord>, ErrorData> {
    let key = routine_codec::routine_key(routine_id).map_err(|error| invalid(error.to_string()))?;
    let rows = db
        .scan_cf_prefix(cf::CF_ROUTINES, &key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let Some((row_key, value)) = rows.first() else {
        return Ok(None);
    };
    if rows.len() > 1 || row_key != &key {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_KEY_COLLISION in CF_ROUTINES: prefix lookup for {routine_id} \
                 returned {} rows, first key {}",
                rows.len(),
                hex_encode(row_key)
            ),
        ));
    }
    decode_routine_record_row(row_key, value).map(Some)
}

/// Decodes one `CF_ROUTINES` row (key + JSON value) into a [`RoutineRecord`].
fn decode_routine_record_row(key: &[u8], value: &[u8]) -> Result<RoutineRecord, ErrorData> {
    let routine_id = routine_codec::decode_routine_key(key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let record = decode_json::<RoutineRecord>(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_ROW_DECODE_FAILED in CF_ROUTINES at {routine_id}: {error}; \
                 CF_ROUTINES is derived state — re-run routine_mine"
            ),
        )
    })?;
    if record.routine_id != routine_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_ID_MISMATCH in CF_ROUTINES: row key {routine_id} holds a record \
                 claiming routine_id {}",
                record.routine_id
            ),
        ));
    }
    Ok(record)
}

/// Writes state rows in one synchronous flushed batch (never the sheddable
/// async path: these are operator decisions and miner bookkeeping that must
/// not vanish silently). Callers gate on disk pressure first.
fn put_state_rows(db: &Db, records: &[RoutineStateRecord]) -> Result<(), ErrorData> {
    if records.is_empty() {
        return Ok(());
    }
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(records.len());
    for record in records {
        let key = routine_codec::routine_state_key(&record.routine_id)
            .map_err(|error| internal(format!("state row has an invalid routine id: {error}")))?;
        let value =
            encode_json(record).map_err(|error| mcp_error(error.code(), error.to_string()))?;
        rows.push((key, value));
    }
    db.mutate_batch_pressure_bypass(cf::CF_ROUTINE_STATE, Vec::<Vec<u8>>::new(), rows)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("failed to write CF_ROUTINE_STATE rows: {error}"),
            )
        })
}

/// Appends a transition, enforcing the newest-last cap loudly via the
/// truncation counter.
fn push_transition(record: &mut RoutineStateRecord, transition: RoutineTransition) {
    record.transitions.push(transition);
    while record.transitions.len() > ROUTINE_STATE_MAX_TRANSITIONS {
        record.transitions.remove(0);
        record.transitions_truncated = record.transitions_truncated.saturating_add(1);
    }
}

/// Appends a confidence change-point if it differs from the latest one.
/// Returns whether the history changed.
fn push_confidence_point(record: &mut RoutineStateRecord, point: RoutineConfidencePoint) -> bool {
    let unchanged = record.confidence_history.last().is_some_and(|last| {
        (last.confidence - point.confidence).abs() < f64::EPSILON
            && last.support_days == point.support_days
            && last.opportunity_days == point.opportunity_days
    });
    if unchanged {
        return false;
    }
    record.confidence_history.push(point);
    while record.confidence_history.len() > ROUTINE_STATE_MAX_CONFIDENCE_POINTS {
        record.confidence_history.remove(0);
        record.confidence_history_truncated = record.confidence_history_truncated.saturating_add(1);
    }
    true
}

#[derive(Clone, Copy, Debug, Default)]
struct StateReconcileCounters {
    created: u64,
    updated: u64,
    marked_unmined: u64,
}

/// Reconciles `CF_ROUTINE_STATE` with the routines a mining run just
/// persisted. Creates candidate rows for first-time routines, refreshes
/// presence/confidence bookkeeping for re-mined ones, and flags rows whose
/// routine vanished. Lifecycle and labels are operator property: this
/// function never changes them — a disabled routine cannot be re-promoted
/// by mining.
fn reconcile_state_rows(
    db: &Db,
    mined: &[RoutineRecord],
    mined_at: u64,
    scanned_rows: &mut u64,
) -> Result<StateReconcileCounters, ErrorData> {
    let mut existing = load_all_state_rows(db, scanned_rows)?;
    let mut counters = StateReconcileCounters::default();
    let mut writes: Vec<RoutineStateRecord> = Vec::new();
    for routine in mined {
        let point = RoutineConfidencePoint {
            ts_ns: mined_at,
            confidence: routine.confidence,
            support_days: routine.support_days,
            opportunity_days: routine.opportunity_days,
        };
        if let Some(mut state) = existing.remove(&routine.routine_id) {
            state.present_in_last_mine = true;
            state.last_mined_ts_ns = Some(mined_at);
            state.updated_ts_ns = mined_at;
            push_confidence_point(&mut state, point);
            counters.updated += 1;
            writes.push(state);
        } else {
            let mut state = RoutineStateRecord {
                record_version: ROUTINE_STATE_RECORD_VERSION,
                routine_id: routine.routine_id.clone(),
                lifecycle: RoutineLifecycle::Candidate,
                label: None,
                created_ts_ns: mined_at,
                updated_ts_ns: mined_at,
                last_mined_ts_ns: Some(mined_at),
                present_in_last_mine: true,
                transitions: vec![RoutineTransition {
                    ts_ns: mined_at,
                    action: RoutineStateAction::Discovered,
                    from: None,
                    to: RoutineLifecycle::Candidate,
                    by: MINER_ACTOR.to_owned(),
                    label_before: None,
                    label_after: None,
                    note: None,
                }],
                transitions_truncated: 0,
                confidence_history: Vec::new(),
                confidence_history_truncated: 0,
                feedback_events: Vec::new(),
                feedback_events_truncated: 0,
                accept_count: 0,
                decline_count: 0,
                ignore_count: 0,
                abandon_count: 0,
                consecutive_declines: 0,
                cooldown_level: 0,
                cooldown_until_ts_ns: None,
            };
            push_confidence_point(&mut state, point);
            counters.created += 1;
            writes.push(state);
        }
    }
    for (_routine_id, mut state) in existing {
        if state.present_in_last_mine {
            state.present_in_last_mine = false;
            state.updated_ts_ns = mined_at;
            counters.marked_unmined += 1;
            writes.push(state);
        }
    }
    put_state_rows(db, &writes)?;
    Ok(counters)
}

/// Mines routines from `CF_EPISODES` and (unless `dry_run`) replaces
/// `CF_ROUTINES` atomically. Shared by the MCP tool and the periodic job.
#[allow(clippy::too_many_lines)]
pub fn mine_and_store_routines(
    db: &Arc<Db>,
    params: &RoutineMineParams,
) -> Result<RoutineMineResponse, ErrorData> {
    if let (Some(start), Some(end)) = (params.start_ts_ns, params.end_ts_ns)
        && start >= end
    {
        return Err(invalid(format!(
            "routine_mine start_ts_ns {start} must be < end_ts_ns {end}"
        )));
    }
    let config = build_config(params)?;

    let _mining = MINE_LOCK
        .lock()
        .map_err(|_poisoned| internal("routine mining lock poisoned"))?;

    if !params.dry_run
        && !(db.pressure_permits_write(cf::CF_ROUTINES)
            && db.pressure_permits_write(cf::CF_ROUTINE_STATE))
    {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "routine_mine refused under disk pressure: cf_names={}/{} pressure_level={:?}; \
                 nothing was deleted or written",
                cf::CF_ROUTINES,
                cf::CF_ROUTINE_STATE,
                db.pressure_level()
            ),
        ));
    }

    let mut scanned_rows = 0_u64;
    let range_start = match params.start_ts_ns {
        Some(start) => start,
        None => match first_episode_ts(db, &mut scanned_rows)? {
            Some(ts_ns) => ts_ns,
            None => {
                // Empty episode store: an honest empty mine. A non-dry run
                // still clears stale routines (derived state must reflect
                // its source) and reconciles the state store so no row
                // still claims to be present in the last mine.
                let stale_keys = existing_routine_keys(db, &mut scanned_rows)?;
                let deleted = u64::try_from(stale_keys.len()).unwrap_or(u64::MAX);
                let state_counters = if params.dry_run {
                    StateReconcileCounters::default()
                } else {
                    if !stale_keys.is_empty() {
                        db.mutate_batch_pressure_bypass(
                            cf::CF_ROUTINES,
                            stale_keys,
                            Vec::<(Vec<u8>, Vec<u8>)>::new(),
                        )
                        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
                    }
                    let counters = reconcile_state_rows(db, &[], now_ts_ns(), &mut scanned_rows)?;
                    reindex_armed_routine_schedule_due_indexes(db)?;
                    counters
                };
                return Ok(RoutineMineResponse {
                    range_start_ns: 0,
                    range_end_ns: 0,
                    scanned_episode_rows: scanned_rows,
                    considered_episodes: 0,
                    eligible_episodes: 0,
                    filtered_agent_episodes: 0,
                    filtered_short_episodes: 0,
                    filtered_no_app_episodes: 0,
                    active_days: 0,
                    candidates_evaluated: 0,
                    candidates_truncated: 0,
                    occurrences_skipped_over_cap: 0,
                    clusters_rejected_low_support: 0,
                    clusters_rejected_dispersed: 0,
                    clusters_rejected_low_confidence: 0,
                    candidates_rejected_as_subpattern: 0,
                    routines_dropped_over_cap: 0,
                    routines_written: 0,
                    routines_deleted: if params.dry_run { 0 } else { deleted },
                    state_rows_created: state_counters.created,
                    state_rows_updated: state_counters.updated,
                    state_rows_marked_unmined: state_counters.marked_unmined,
                    dry_run: params.dry_run,
                    routines: Vec::new(),
                });
            }
        },
    };
    let range_end = params.end_ts_ns.unwrap_or_else(now_ts_ns);
    if range_start >= range_end {
        return Err(invalid(format!(
            "routine_mine effective range is empty: start {range_start} >= end {range_end}"
        )));
    }
    let range_start_snapped = local_day_start(range_start)?;
    let range_end_snapped = next_local_day_start(local_day_start(range_end.saturating_sub(1))?)?;

    let days = mining_days(
        db,
        range_start_snapped,
        range_end_snapped,
        &mut scanned_rows,
    )?;
    let mined_at = now_ts_ns();
    let mining = mine_routines(&days, mined_at, &config).map_err(|error| {
        internal(format!(
            "routine_mine engine failed for range [{range_start_snapped}, {range_end_snapped}): {error}"
        ))
    })?;

    let mut new_rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(mining.routines.len());
    for routine in &mining.routines {
        let key = routine_codec::routine_key(&routine.routine_id)
            .map_err(|error| internal(format!("engine produced an invalid routine id: {error}")))?;
        let value =
            encode_json(routine).map_err(|error| mcp_error(error.code(), error.to_string()))?;
        new_rows.push((key, value));
    }
    let written = u64::try_from(new_rows.len()).unwrap_or(u64::MAX);
    let mut deleted = 0_u64;
    let mut state_counters = StateReconcileCounters::default();
    if params.dry_run {
        tracing::info!(
            code = "ROUTINE_MINE_DRY_RUN",
            range_start_ns = range_start_snapped,
            range_end_ns = range_end_snapped,
            routines = written,
            "routine_mine dry run computed without mutating CF_ROUTINES"
        );
    } else {
        let stale_keys = existing_routine_keys(db, &mut scanned_rows)?;
        deleted = u64::try_from(stale_keys.len()).unwrap_or(u64::MAX);
        db.mutate_batch_pressure_bypass(cf::CF_ROUTINES, stale_keys, new_rows)
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "routine_mine failed to replace CF_ROUTINES: {error}; \
                         the previous routines are unchanged"
                    ),
                )
            })?;
        // CF_ROUTINES and CF_ROUTINE_STATE are separate flushed batches; if
        // this reconcile fails the error is loud, lifecycle rows are intact,
        // and the next mining run repairs the presence bookkeeping.
        state_counters = reconcile_state_rows(db, &mining.routines, mined_at, &mut scanned_rows)?;
        reindex_armed_routine_schedule_due_indexes(db)?;
        tracing::info!(
            code = "ROUTINE_MINE_REPLACED",
            range_start_ns = range_start_snapped,
            range_end_ns = range_end_snapped,
            routines_written = written,
            routines_deleted = deleted,
            active_days = mining.active_days,
            candidates = mining.candidates_evaluated,
            state_rows_created = state_counters.created,
            state_rows_updated = state_counters.updated,
            state_rows_marked_unmined = state_counters.marked_unmined,
            "routine_mine replaced the routine store"
        );
    }

    Ok(RoutineMineResponse {
        range_start_ns: range_start_snapped,
        range_end_ns: range_end_snapped,
        scanned_episode_rows: scanned_rows,
        considered_episodes: mining.considered_episodes,
        eligible_episodes: mining.eligible_episodes,
        filtered_agent_episodes: mining.filtered_agent_episodes,
        filtered_short_episodes: mining.filtered_short_episodes,
        filtered_no_app_episodes: mining.filtered_no_app_episodes,
        active_days: mining.active_days,
        candidates_evaluated: mining.candidates_evaluated,
        candidates_truncated: mining.candidates_truncated,
        occurrences_skipped_over_cap: mining.occurrences_skipped_over_cap,
        clusters_rejected_low_support: mining.clusters_rejected_low_support,
        clusters_rejected_dispersed: mining.clusters_rejected_dispersed,
        clusters_rejected_low_confidence: mining.clusters_rejected_low_confidence,
        candidates_rejected_as_subpattern: mining.candidates_rejected_as_subpattern,
        routines_dropped_over_cap: mining.routines_dropped_over_cap,
        routines_written: if params.dry_run { 0 } else { written },
        routines_deleted: deleted,
        state_rows_created: state_counters.created,
        state_rows_updated: state_counters.updated,
        state_rows_marked_unmined: state_counters.marked_unmined,
        dry_run: params.dry_run,
        routines: mining.routines,
    })
}

/// Default and maximum `routine_list` page sizes.
pub const DEFAULT_ROUTINE_LIST_LIMIT: u32 = 100;
pub const MAX_ROUTINE_LIST_LIMIT: u32 = 500;
/// Operator label and note bounds for `routine_update`.
pub const MAX_LABEL_CHARS: usize = 120;
pub const MAX_NOTE_CHARS: usize = 500;

/// A mined routine with no state row yet (older binary mined it, or a
/// reconcile failed): present it honestly as an unreviewed candidate.
fn synthesized_default_state(routine: &RoutineRecord) -> RoutineStateRecord {
    RoutineStateRecord {
        record_version: ROUTINE_STATE_RECORD_VERSION,
        routine_id: routine.routine_id.clone(),
        lifecycle: RoutineLifecycle::Candidate,
        label: None,
        created_ts_ns: routine.ts_ns,
        updated_ts_ns: routine.ts_ns,
        last_mined_ts_ns: Some(routine.ts_ns),
        present_in_last_mine: true,
        transitions: Vec::new(),
        transitions_truncated: 0,
        confidence_history: Vec::new(),
        confidence_history_truncated: 0,
        feedback_events: Vec::new(),
        feedback_events_truncated: 0,
        accept_count: 0,
        decline_count: 0,
        ignore_count: 0,
        abandon_count: 0,
        consecutive_declines: 0,
        cooldown_level: 0,
        cooldown_until_ts_ns: None,
    }
}

/// Loads every `CF_ROUTINES` row, budget-guarded.
fn load_all_routine_records(
    db: &Db,
    scanned_rows: &mut u64,
) -> Result<BTreeMap<String, RoutineRecord>, ErrorData> {
    let mut records: BTreeMap<String, RoutineRecord> = BTreeMap::new();
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(format!(
                "ROUTINE_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} CF_ROUTINES rows; \
                 the routine store should hold at most a few hundred rows — inspect CF_ROUTINES"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_ROUTINES, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            *scanned_rows += 1;
            let record = decode_routine_record_row(key, value)?;
            records.insert(record.routine_id.clone(), record);
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(records)
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineListParams {
    /// Lifecycle states to include. Defaults to candidate, confirmed, and
    /// disabled (everything except archived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Vec<RoutineLifecycle>>,
    /// Keep only routines whose confidence (Wilson lower bound) is at least
    /// this value. Unmined entries use their last recorded confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,
    /// Keep only routines with a template step in this app (lowercased
    /// process executable name, exact match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granularity: Option<RoutineGranularity>,
    /// Also list state rows whose routine the last mining run no longer
    /// derived (lifecycle decisions outlive derived rows).
    #[serde(default)]
    pub include_unmined: bool,
    /// Maximum entries returned (default 100, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One `routine_list` entry: lifecycle joined onto the mined record.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineListEntry {
    pub routine_id: String,
    pub lifecycle: RoutineLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether the last mining run derived this routine.
    pub mined: bool,
    /// Whether a physical `CF_ROUTINE_STATE` row exists (false means the
    /// lifecycle shown is the synthesized candidate default).
    pub state_row_exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granularity: Option<RoutineGranularity>,
    /// Ordered template steps (empty for unmined entries).
    pub steps: Vec<RoutineStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_label: Option<String>,
    /// Mined entries: the current record's confidence. Unmined entries: the
    /// last recorded confidence change-point, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_mined_ts_ns: Option<u64>,
    pub updated_ts_ns: u64,
    /// True when the hygiene cleaning path has marked this derived routine as
    /// poisoned by a cleaned prompt-injection source row.
    pub tainted: bool,
    /// Exact `hygiene/taint/v1/routine/<routine_id>` ledger row, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<HygieneTaintRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineListResponse {
    /// Rows currently in `CF_ROUTINES` (the last mining run's output).
    pub total_mined: u64,
    /// Rows currently in `CF_ROUTINE_STATE`.
    pub total_state_rows: u64,
    /// Entries matching the filters before the limit was applied.
    pub matched: u64,
    pub returned: u64,
    /// True when `matched > returned` (raise `limit` to see the rest).
    pub truncated: bool,
    pub entries: Vec<RoutineListEntry>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineInspectParams {
    /// Stable routine id (`rt1-` + 16 hex chars).
    pub routine_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineInspectResponse {
    pub routine_id: String,
    /// Whether the last mining run derived this routine.
    pub mined: bool,
    /// Whether a physical `CF_ROUTINE_STATE` row exists.
    pub state_row_exists: bool,
    /// Full mined record, including schedule signature and support evidence
    /// (contributing episode ids resolvable via `episode_get`). `None` when
    /// the routine is no longer derived by the current mine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RoutineRecord>,
    /// Lifecycle state, transition audit trail, and confidence history.
    pub state: RoutineStateRecord,
    /// True when the hygiene cleaning path has marked this derived routine as
    /// poisoned by a cleaned prompt-injection source row.
    pub tainted: bool,
    /// Exact `hygiene/taint/v1/routine/<routine_id>` ledger row, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<HygieneTaintRecord>,
    /// Automation candidate/install state written by `routine_automate` and
    /// updated by `profile_authoring_decide`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<RoutineAutomationRecord>,
    /// Armed auto-run state written by `routine_update action=arm|disarm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed: Option<ArmedRoutineRecord>,
}

/// Lifecycle operations accepted by `routine_update`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoutineUpdateAction {
    /// candidate → confirmed.
    Confirm,
    /// candidate or confirmed → disabled.
    Disable,
    /// disabled or archived → candidate (re-earns confirmation).
    Enable,
    /// candidate, confirmed, or disabled → archived.
    Archive,
    /// Set the operator label; lifecycle unchanged.
    Rename,
    /// Arm an installed routine automation for schedule and/or intent triggers.
    Arm,
    /// Disarm an armed routine automation; lifecycle unchanged.
    Disarm,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineUpdateParams {
    /// Stable routine id (`rt1-` + 16 hex chars).
    pub routine_id: String,
    pub action: RoutineUpdateAction,
    /// New display name; required for rename, rejected otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Free-form audit note recorded on the transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// For action=arm: whether the routine should run on its schedule.
    /// Defaults to true when both trigger fields are omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm_schedule: Option<bool>,
    /// For action=arm: whether live intent matches should trigger the routine.
    /// Defaults to true when both trigger fields are omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm_intent: Option<bool>,
    /// For action=arm: consecutive failed runs before automatic disarm.
    /// Defaults to 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_threshold: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineUpdateResponse {
    pub routine_id: String,
    pub action: RoutineUpdateAction,
    pub lifecycle_before: RoutineLifecycle,
    pub lifecycle_after: RoutineLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_after: Option<String>,
    /// True when this call materialized the state row (first operator
    /// action on a routine the miner had not yet reconciled).
    pub state_row_created: bool,
    /// The post-write state row, read back from `CF_ROUTINE_STATE` after
    /// the flush — physical storage truth, not the in-memory value.
    pub state: RoutineStateRecord,
    /// Post-write armed auto-run state for this routine, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed: Option<ArmedRoutineRecord>,
}

#[must_use]
pub const fn routine_list() -> M3ToolStub {
    M3ToolStub::new("routine_list")
}

#[must_use]
pub const fn routine_inspect() -> M3ToolStub {
    M3ToolStub::new("routine_inspect")
}

#[must_use]
pub const fn routine_update() -> M3ToolStub {
    M3ToolStub::new("routine_update")
}

#[must_use]
pub fn required_permissions_list(_params: &RoutineListParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_inspect(_params: &RoutineInspectParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_update(_params: &RoutineUpdateParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

pub(crate) fn validate_routine_id_param(tool: &str, routine_id: &str) -> Result<(), ErrorData> {
    routine_codec::routine_key(routine_id)
        .map(|_key| ())
        .map_err(|_error| {
            invalid(format!(
                "{tool} routine_id is invalid: ROUTINE_KEY_INVALID: expected \
             {:?} + 16 lowercase hex chars, got {routine_id:?}",
                routine_codec::ROUTINE_ID_PREFIX
            ))
        })
}

/// Builds one list entry from a mined record and its (possibly synthesized)
/// state row.
fn list_entry_mined(
    record: &RoutineRecord,
    state: &RoutineStateRecord,
    exists: bool,
    taint: Option<HygieneTaintRecord>,
) -> RoutineListEntry {
    let tainted = taint.is_some();
    RoutineListEntry {
        routine_id: record.routine_id.clone(),
        lifecycle: state.lifecycle,
        label: state.label.clone(),
        mined: true,
        state_row_exists: exists,
        granularity: Some(record.granularity),
        steps: record.steps.clone(),
        schedule_label: Some(record.schedule_label.clone()),
        confidence: Some(record.confidence),
        support_days: Some(record.support_days),
        occurrence_count: Some(record.occurrence_count),
        last_mined_ts_ns: state.last_mined_ts_ns.or(Some(record.ts_ns)),
        updated_ts_ns: state.updated_ts_ns,
        tainted,
        taint,
    }
}

fn list_entry_unmined(
    state: &RoutineStateRecord,
    taint: Option<HygieneTaintRecord>,
) -> RoutineListEntry {
    let last_point = state.confidence_history.last();
    let tainted = taint.is_some();
    RoutineListEntry {
        routine_id: state.routine_id.clone(),
        lifecycle: state.lifecycle,
        label: state.label.clone(),
        mined: false,
        state_row_exists: true,
        granularity: None,
        steps: Vec::new(),
        schedule_label: None,
        confidence: last_point.map(|point| point.confidence),
        support_days: last_point.map(|point| point.support_days),
        occurrence_count: None,
        last_mined_ts_ns: state.last_mined_ts_ns,
        updated_ts_ns: state.updated_ts_ns,
        tainted,
        taint,
    }
}

/// Lists routines with lifecycle state joined onto mined records.
pub fn list_routines(
    db: &Arc<Db>,
    params: &RoutineListParams,
) -> Result<RoutineListResponse, ErrorData> {
    if let Some(min_confidence) = params.min_confidence
        && !(0.0..=1.0).contains(&min_confidence)
    {
        return Err(invalid(format!(
            "routine_list min_confidence must be within [0.0, 1.0]; got {min_confidence}"
        )));
    }
    if let Some(app) = &params.app
        && app.trim().is_empty()
    {
        return Err(invalid("routine_list app filter must not be blank"));
    }
    if let Some(states) = &params.lifecycle
        && states.is_empty()
    {
        return Err(invalid(
            "routine_list lifecycle filter must not be an empty list; omit it for the default",
        ));
    }
    let limit = match params.limit {
        None => DEFAULT_ROUTINE_LIST_LIMIT,
        Some(limit) if (1..=MAX_ROUTINE_LIST_LIMIT).contains(&limit) => limit,
        Some(limit) => {
            return Err(invalid(format!(
                "routine_list limit must be between 1 and {MAX_ROUTINE_LIST_LIMIT}; got {limit}"
            )));
        }
    };

    let mut scanned_rows = 0_u64;
    let records = load_all_routine_records(db, &mut scanned_rows)?;
    let states = load_all_state_rows(db, &mut scanned_rows)?;
    let total_mined = u64::try_from(records.len()).unwrap_or(u64::MAX);
    let total_state_rows = u64::try_from(states.len()).unwrap_or(u64::MAX);

    let lifecycle_allowed = |lifecycle: RoutineLifecycle| match &params.lifecycle {
        Some(states) => states.contains(&lifecycle),
        None => lifecycle != RoutineLifecycle::Archived,
    };
    let app_filter = params.app.as_deref().map(str::trim).map(str::to_lowercase);

    let mut entries: Vec<RoutineListEntry> = Vec::new();
    for (routine_id, record) in &records {
        let (state, exists) = match states.get(routine_id) {
            Some(state) => (state.clone(), true),
            None => (synthesized_default_state(record), false),
        };
        let taint = read_taint_record_from_db(db, "routine", routine_id)?;
        let entry = list_entry_mined(record, &state, exists, taint);
        if !lifecycle_allowed(entry.lifecycle) {
            continue;
        }
        if let Some(granularity) = params.granularity
            && record.granularity != granularity
        {
            continue;
        }
        if let Some(app) = &app_filter
            && !record.steps.iter().any(|step| &step.app == app)
        {
            continue;
        }
        if let Some(min_confidence) = params.min_confidence
            && record.confidence < min_confidence
        {
            continue;
        }
        entries.push(entry);
    }
    if params.include_unmined {
        for (routine_id, state) in &states {
            if records.contains_key(routine_id) {
                continue;
            }
            let taint = read_taint_record_from_db(db, "routine", routine_id)?;
            let entry = list_entry_unmined(state, taint);
            if !lifecycle_allowed(entry.lifecycle) {
                continue;
            }
            // Unmined entries carry no template, so app/granularity filters
            // exclude them; min_confidence applies to the last known value.
            if params.granularity.is_some() || app_filter.is_some() {
                continue;
            }
            if let Some(min_confidence) = params.min_confidence
                && entry.confidence.is_none_or(|value| value < min_confidence)
            {
                continue;
            }
            entries.push(entry);
        }
    }
    // Mined first, strongest first; unmined afterwards, newest first; id as
    // the deterministic tiebreaker.
    entries.sort_by(|a, b| {
        b.mined.cmp(&a.mined).then_with(|| {
            if a.mined {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.routine_id.cmp(&b.routine_id))
            } else {
                b.updated_ts_ns
                    .cmp(&a.updated_ts_ns)
                    .then_with(|| a.routine_id.cmp(&b.routine_id))
            }
        })
    });
    let matched = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    entries.truncate(limit as usize);
    let returned = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    Ok(RoutineListResponse {
        total_mined,
        total_state_rows,
        matched,
        returned,
        truncated: matched > returned,
        entries,
    })
}

/// Fetches one routine: full mined record plus lifecycle state.
pub fn inspect_routine(
    db: &Arc<Db>,
    params: &RoutineInspectParams,
) -> Result<RoutineInspectResponse, ErrorData> {
    validate_routine_id_param("routine_inspect", &params.routine_id)?;
    let record = load_routine_record(db, &params.routine_id)?;
    let state = load_state_row(db, &params.routine_id)?;
    let state_row_exists = state.is_some();
    let state = match (state, &record) {
        (Some(state), _) => state,
        (None, Some(record)) => synthesized_default_state(record),
        (None, None) => {
            return Err(invalid(format!(
                "ROUTINE_NOT_FOUND: routine_id {} exists in neither CF_ROUTINES nor \
                 CF_ROUTINE_STATE; run routine_list to see what exists",
                params.routine_id
            )));
        }
    };
    let taint = read_taint_record_from_db(db, "routine", &params.routine_id)?;
    let tainted = taint.is_some();
    let automation = load_routine_automation_record(db, &params.routine_id)?;
    let armed = load_armed_routine_record(db, &params.routine_id)?;
    Ok(RoutineInspectResponse {
        routine_id: params.routine_id.clone(),
        mined: record.is_some(),
        state_row_exists,
        record,
        state,
        tainted,
        taint,
        automation,
        armed,
    })
}

/// Default and maximum sample occurrences carried in a label export.
pub const DEFAULT_LABEL_EXPORT_SAMPLES: u32 = 3;
pub const MAX_LABEL_EXPORT_SAMPLES: u32 = 10;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineLabelExportParams {
    /// Routine to export naming evidence for. Must be a mined routine present
    /// in `CF_ROUTINES` (an operator-only state row with no mined template
    /// cannot be labelled — there is nothing to name).
    pub routine_id: String,
    /// Recent sample occurrences to include (default 3, max 10), newest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_samples: Option<u32>,
}

pub fn required_permissions_label_export(
    _params: &RoutineLabelExportParams,
) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelStep {
    /// Zero-based template position.
    pub index: u32,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// `app` or `app:document` machine identity for this step.
    pub identity: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelSample {
    pub day_start_ns: u64,
    pub minute_of_day: u32,
    /// Rendered local start, e.g. `Mon 2026-06-09 08:47`.
    pub local_start: String,
    /// Stable episode ids of this occurrence; resolve full content (titles,
    /// urls, keystrokes) via `episode_get`.
    pub episode_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineLabelExportResponse {
    pub routine_id: String,
    /// Whether a `CF_ROUTINE_STATE` row already exists for this routine.
    pub state_row_exists: bool,
    pub lifecycle: RoutineLifecycle,
    /// Current operator label, `None` if the routine was never named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_label: Option<String>,
    pub granularity: RoutineGranularity,
    /// Whole-routine machine identity, e.g.
    /// `chrome:mail.google.com → excel:report.xlsx → teams`.
    pub machine_identity: String,
    pub schedule_label: String,
    pub dow_class: RoutineDowClass,
    pub mean_minute_of_day: u32,
    pub support_days: u32,
    pub occurrence_count: u32,
    pub opportunity_days: u32,
    pub confidence: f64,
    pub steps: Vec<LabelStep>,
    /// Newest-first sample occurrences (capped by `max_samples`).
    pub samples: Vec<LabelSample>,
    /// Total occurrences the mined record carries (samples are a suffix).
    pub total_evidence_occurrences: u32,
    /// Ready-to-use compact prompt block for an LLM to name/describe this
    /// routine without any other context.
    pub prompt: String,
    /// Exact `routine_update` call to persist the agent's chosen label.
    pub writeback_hint: String,
    /// Character count of `prompt` (a coarse token-budget proxy).
    pub prompt_chars: u32,
}

fn render_minute_of_day(minute_of_day: u32) -> String {
    let minute = minute_of_day % 1440;
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

fn render_local_start(day_start_ns: u64, minute_of_day: u32) -> String {
    let secs = i64::try_from(day_start_ns / 1_000_000_000).unwrap_or(0)
        + i64::from(minute_of_day % 1440) * 60;
    match Local.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%a %Y-%m-%d %H:%M").to_string(),
        _ => format!("ts {day_start_ns}+{minute_of_day}m"),
    }
}

fn step_identity(step: &RoutineStep) -> String {
    match &step.document {
        Some(document) => format!("{}:{document}", step.app),
        None => step.app.clone(),
    }
}

fn render_dow_class(dow: &RoutineDowClass) -> String {
    match dow {
        RoutineDowClass::Daily => "daily".to_owned(),
        RoutineDowClass::Weekdays => "weekdays".to_owned(),
        RoutineDowClass::Weekend => "weekend".to_owned(),
        RoutineDowClass::Days { days } => {
            const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let list = days
                .iter()
                .map(|day| NAMES.get(usize::from(*day)).copied().unwrap_or("?"))
                .collect::<Vec<_>>()
                .join(",");
            format!("days[{list}]")
        }
    }
}

/// Builds a compact, prompt-ready naming bundle for a mined routine (#851).
///
/// Read-only: it reuses the mined `CF_ROUTINES` template (the machine
/// identity, schedule signature, and capped sample evidence) plus the current
/// `CF_ROUTINE_STATE` label/lifecycle. The mined evidence already carries
/// stable `episode_id`s, so an agent can pull richer per-episode content via
/// `episode_get` if the compact bundle is not enough to name the routine.
/// A routine with no mined record is an honest, structured refusal — there is
/// no template to name.
pub fn export_routine_label(
    db: &Arc<Db>,
    params: &RoutineLabelExportParams,
) -> Result<RoutineLabelExportResponse, ErrorData> {
    validate_routine_id_param("routine_label_export", &params.routine_id)?;
    let max_samples = params.max_samples.unwrap_or(DEFAULT_LABEL_EXPORT_SAMPLES);
    if max_samples == 0 || max_samples > MAX_LABEL_EXPORT_SAMPLES {
        return Err(invalid(format!(
            "routine_label_export max_samples must be between 1 and \
             {MAX_LABEL_EXPORT_SAMPLES}; got {max_samples}"
        )));
    }

    let Some(record) = load_routine_record(db, &params.routine_id)? else {
        return Err(invalid(format!(
            "ROUTINE_NOT_MINED: routine_id {} is not present in CF_ROUTINES, so it has no \
             mined template (apps/documents/schedule) to name. Run routine_mine, or \
             routine_list to see what currently exists",
            params.routine_id
        )));
    };

    let state = load_state_row(db, &params.routine_id)?;
    let state_row_exists = state.is_some();
    let (lifecycle, current_label) = match &state {
        Some(state) => (state.lifecycle, state.label.clone()),
        None => (RoutineLifecycle::Candidate, None),
    };

    let steps: Vec<LabelStep> = record
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| LabelStep {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            app: step.app.clone(),
            document: step.document.clone(),
            identity: step_identity(step),
        })
        .collect();

    // `evidence` is newest-last; take the most recent `max_samples`, newest first.
    let samples: Vec<LabelSample> = record
        .evidence
        .iter()
        .rev()
        .take(max_samples as usize)
        .map(|evidence| LabelSample {
            day_start_ns: evidence.day_start_ns,
            minute_of_day: evidence.minute_of_day,
            local_start: render_local_start(evidence.day_start_ns, evidence.minute_of_day),
            episode_ids: evidence.episode_ids.clone(),
        })
        .collect();

    let machine_identity = record
        .steps
        .iter()
        .map(step_identity)
        .collect::<Vec<_>>()
        .join(" → ");
    let dow = render_dow_class(&record.dow_class);

    use std::fmt::Write as _;
    let mut prompt = String::new();
    prompt.push_str("Name and describe this recurring computer routine for its operator.\n");
    // Writing to a String is infallible; the discarded Result is the idiomatic
    // way to satisfy `write!`'s must-use without an unwrap that can never fire.
    let _ = writeln!(prompt, "Machine identity: {machine_identity}");
    let _ = writeln!(
        prompt,
        "Schedule: {} ({dow}), typically around {}.",
        record.schedule_label,
        render_minute_of_day(record.mean_minute_of_day)
    );
    let _ = writeln!(
        prompt,
        "Seen on {} of {} eligible days (confidence {:.2}); {} occurrences total.",
        record.support_days, record.opportunity_days, record.confidence, record.occurrence_count
    );
    prompt.push_str("Steps in order:\n");
    for step in &steps {
        let _ = writeln!(prompt, "  {}. {}", step.index + 1, step.identity);
    }
    if !samples.is_empty() {
        prompt.push_str("Recent occurrences:\n");
        for sample in &samples {
            let _ = writeln!(prompt, "  - {}", sample.local_start);
        }
    }
    if let Some(label) = &current_label {
        let _ = writeln!(prompt, "Current label: {label}");
    }
    prompt
        .push_str("Reply with a short human name (<=120 chars) and a one-sentence description.\n");

    let writeback_hint = format!(
        "routine_update {{ \"routine_id\": \"{}\", \"action\": \"rename\", \
         \"label\": \"<chosen name>\", \"note\": \"<optional one-sentence description>\" }}",
        params.routine_id
    );
    let prompt_chars = u32::try_from(prompt.chars().count()).unwrap_or(u32::MAX);

    Ok(RoutineLabelExportResponse {
        routine_id: params.routine_id.clone(),
        state_row_exists,
        lifecycle,
        current_label,
        granularity: record.granularity,
        machine_identity,
        schedule_label: record.schedule_label.clone(),
        dow_class: record.dow_class.clone(),
        mean_minute_of_day: record.mean_minute_of_day,
        support_days: record.support_days,
        occurrence_count: record.occurrence_count,
        opportunity_days: record.opportunity_days,
        confidence: record.confidence,
        steps,
        samples,
        total_evidence_occurrences: u32::try_from(record.evidence.len()).unwrap_or(u32::MAX),
        prompt,
        writeback_hint,
        prompt_chars,
    })
}

// ---------------------------------------------------------------------------
// #856 — suggestion feedback loop (Wilson-bound confidence + escalating cooldown)
// ---------------------------------------------------------------------------

/// Cooldown applied at the first consecutive decline (1 hour).
pub const FEEDBACK_COOLDOWN_BASE_SECS: u64 = 3_600;
/// Per-consecutive-decline growth factor for the cooldown (geometric backoff,
/// the proven anti-fatigue pattern — cf. Chrome web-push escalation).
pub const FEEDBACK_COOLDOWN_MULTIPLIER: u64 = 6;
/// Hard cap on a single cooldown window (14 days), so a long decline streak
/// can never silence a routine effectively forever.
pub const FEEDBACK_COOLDOWN_CAP_SECS: u64 = 14 * 24 * 3_600;

/// Escalating cooldown duration for `consecutive` consecutive non-accept
/// outcomes: `base * multiplier^(consecutive-1)`, capped. `consecutive == 0`
/// means no cooldown. Pure and saturating so it is unit-testable and can never
/// panic on overflow.
#[must_use]
pub fn feedback_cooldown_secs(consecutive: u32) -> u64 {
    if consecutive == 0 {
        return 0;
    }
    let mut secs = FEEDBACK_COOLDOWN_BASE_SECS;
    for _ in 1..consecutive {
        secs = secs.saturating_mul(FEEDBACK_COOLDOWN_MULTIPLIER);
        if secs >= FEEDBACK_COOLDOWN_CAP_SECS {
            return FEEDBACK_COOLDOWN_CAP_SECS;
        }
    }
    secs.min(FEEDBACK_COOLDOWN_CAP_SECS)
}

/// Trials that count toward the acceptance Wilson bound: accepts + declines +
/// ignored-timeouts. Abandonments are provenance only and excluded.
#[must_use]
pub const fn feedback_trials(state: &RoutineStateRecord) -> u64 {
    state.accept_count as u64 + state.decline_count as u64 + state.ignore_count as u64
}

/// Wilson 95% lower bound of the accept rate, or `None` when there are no
/// trials yet (honest "unknown", never a forced 0 that would suppress an
/// un-judged routine).
#[must_use]
pub fn feedback_acceptance_lower_bound(state: &RoutineStateRecord) -> Option<f64> {
    let trials = feedback_trials(state);
    if trials == 0 {
        return None;
    }
    Some(synapse_core::routines::wilson_lower_bound(
        u64::from(state.accept_count),
        trials,
    ))
}

/// True when `now_ns` is before the routine's cooldown deadline.
#[must_use]
pub fn feedback_suppressed(state: &RoutineStateRecord, now_ns: u64) -> bool {
    state
        .cooldown_until_ts_ns
        .is_some_and(|until| now_ns < until)
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineFeedbackParams {
    /// Routine the surfaced suggestion was for.
    pub routine_id: String,
    /// How the suggestion resolved.
    pub outcome: RoutineFeedbackOutcome,
    /// Optional operator/agent note recorded with the outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Replay/test seam: evaluate as of this instant instead of the wall clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ts_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineFeedbackResponse {
    pub routine_id: String,
    pub outcome: RoutineFeedbackOutcome,
    pub state_row_created: bool,
    pub accept_count: u32,
    pub decline_count: u32,
    pub ignore_count: u32,
    pub abandon_count: u32,
    pub consecutive_declines: u32,
    pub cooldown_level: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until_ts_ns: Option<u64>,
    /// Seconds of cooldown remaining as of evaluation (0 when not suppressed).
    pub cooldown_remaining_secs: u64,
    /// Whether the suggestion engine must suppress this routine right now.
    pub suppressed: bool,
    /// Wilson lower bound of the accept rate, `None` until the first trial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_lower_bound: Option<f64>,
    /// Mined confidence (`None` when only an operator state row exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mined_confidence: Option<f64>,
    /// Mined confidence folded with the acceptance lower bound (`mined *
    /// acceptance`); equals `mined_confidence` while there are no trials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_confidence: Option<f64>,
    /// Read-your-write of the persisted `CF_ROUTINE_STATE` row.
    pub state: RoutineStateRecord,
}

pub fn required_permissions_feedback(_params: &RoutineFeedbackParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

fn validate_feedback_fields(params: &RoutineFeedbackParams) -> Result<(), ErrorData> {
    if let Some(note) = &params.note {
        if note.trim().is_empty() {
            return Err(invalid("routine_feedback note must not be blank when set"));
        }
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(invalid(format!(
                "routine_feedback note must be at most {MAX_NOTE_CHARS} characters; got {}",
                note.chars().count()
            )));
        }
    }
    Ok(())
}

/// Appends a feedback event, enforcing the newest-last cap via the truncation
/// counter (mirrors [`push_transition`]).
fn push_feedback_event(record: &mut RoutineStateRecord, event: RoutineFeedbackEvent) {
    record.feedback_events.push(event);
    while record.feedback_events.len() > ROUTINE_STATE_MAX_FEEDBACK_EVENTS {
        record.feedback_events.remove(0);
        record.feedback_events_truncated = record.feedback_events_truncated.saturating_add(1);
    }
}

/// Records one suggestion outcome against a routine and folds it into the
/// routine's effective confidence and escalating decline cooldown (#856).
///
/// Accepts reset the decline streak and clear the cooldown (recovery); declines
/// and ignored-timeouts escalate the cooldown geometrically; abandonments are
/// recorded for provenance but never suppress. Synchronous flushed write
/// followed by a physical read-back, exactly like [`update_routine`].
pub fn record_routine_feedback(
    db: &Arc<Db>,
    params: &RoutineFeedbackParams,
    by_session: &str,
) -> Result<RoutineFeedbackResponse, ErrorData> {
    validate_routine_id_param("routine_feedback", &params.routine_id)?;
    validate_feedback_fields(params)?;

    if !db.pressure_permits_write(cf::CF_ROUTINE_STATE) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "routine_feedback refused under disk pressure: cf_name={} pressure_level={:?}; \
                 the feedback is unchanged",
                cf::CF_ROUTINE_STATE,
                db.pressure_level()
            ),
        ));
    }

    let mined = load_routine_record(db, &params.routine_id)?;
    let existing_state = load_state_row(db, &params.routine_id)?;
    let state_row_created = existing_state.is_none();
    let mut state = match existing_state {
        Some(state) => state,
        None => match &mined {
            Some(record) => synthesized_default_state(record),
            None => {
                return Err(invalid(format!(
                    "ROUTINE_NOT_FOUND: routine_id {} exists in neither CF_ROUTINES nor \
                     CF_ROUTINE_STATE; run routine_list to see what exists",
                    params.routine_id
                )));
            }
        },
    };

    let now = params.now_ts_ns.unwrap_or_else(now_ts_ns);
    state.record_version = ROUTINE_STATE_RECORD_VERSION;
    push_feedback_event(
        &mut state,
        RoutineFeedbackEvent {
            ts_ns: now,
            outcome: params.outcome,
            by: by_session.to_owned(),
            note: params.note.clone(),
        },
    );

    match params.outcome {
        RoutineFeedbackOutcome::Accepted => {
            state.accept_count = state.accept_count.saturating_add(1);
            state.consecutive_declines = 0;
            state.cooldown_level = 0;
            state.cooldown_until_ts_ns = None;
        }
        RoutineFeedbackOutcome::Declined | RoutineFeedbackOutcome::IgnoredTimeout => {
            if params.outcome == RoutineFeedbackOutcome::Declined {
                state.decline_count = state.decline_count.saturating_add(1);
            } else {
                state.ignore_count = state.ignore_count.saturating_add(1);
            }
            state.consecutive_declines = state.consecutive_declines.saturating_add(1);
            state.cooldown_level = state.consecutive_declines;
            let cooldown = feedback_cooldown_secs(state.consecutive_declines);
            state.cooldown_until_ts_ns =
                Some(now.saturating_add(cooldown.saturating_mul(1_000_000_000)));
        }
        RoutineFeedbackOutcome::Abandoned => {
            state.abandon_count = state.abandon_count.saturating_add(1);
        }
    }
    state.updated_ts_ns = now;

    put_state_rows(db, std::slice::from_ref(&state))?;
    let readback = load_state_row(db, &params.routine_id)?.ok_or_else(|| {
        internal(format!(
            "ROUTINE_STATE_READBACK_MISSING: CF_ROUTINE_STATE row for {} vanished immediately \
             after a flushed feedback write",
            params.routine_id
        ))
    })?;
    if readback != state {
        return Err(internal(format!(
            "ROUTINE_STATE_READBACK_MISMATCH: CF_ROUTINE_STATE row for {} does not match the \
             value just written after feedback",
            params.routine_id
        )));
    }

    let acceptance_lower_bound = feedback_acceptance_lower_bound(&readback);
    let mined_confidence = mined.as_ref().map(|record| record.confidence);
    let effective_confidence = mined_confidence.map(|mined_c| match acceptance_lower_bound {
        Some(acceptance) => mined_c * acceptance,
        None => mined_c,
    });
    let suppressed = feedback_suppressed(&readback, now);
    let cooldown_remaining_secs = readback
        .cooldown_until_ts_ns
        .filter(|until| now < *until)
        .map_or(0, |until| (until - now) / 1_000_000_000);

    tracing::info!(
        code = "ROUTINE_FEEDBACK_RECORDED",
        routine_id = %params.routine_id,
        outcome = ?params.outcome,
        consecutive_declines = readback.consecutive_declines,
        cooldown_until_ts_ns = readback.cooldown_until_ts_ns,
        suppressed,
        by = by_session,
        "routine suggestion feedback persisted and read back"
    );

    Ok(RoutineFeedbackResponse {
        routine_id: params.routine_id.clone(),
        outcome: params.outcome,
        state_row_created,
        accept_count: readback.accept_count,
        decline_count: readback.decline_count,
        ignore_count: readback.ignore_count,
        abandon_count: readback.abandon_count,
        consecutive_declines: readback.consecutive_declines,
        cooldown_level: readback.cooldown_level,
        cooldown_until_ts_ns: readback.cooldown_until_ts_ns,
        cooldown_remaining_secs,
        suppressed,
        acceptance_lower_bound,
        mined_confidence,
        effective_confidence,
        state: readback,
    })
}

fn action_kind(action: RoutineUpdateAction) -> RoutineStateAction {
    match action {
        RoutineUpdateAction::Confirm => RoutineStateAction::Confirm,
        RoutineUpdateAction::Disable => RoutineStateAction::Disable,
        RoutineUpdateAction::Enable => RoutineStateAction::Enable,
        RoutineUpdateAction::Archive => RoutineStateAction::Archive,
        RoutineUpdateAction::Rename => RoutineStateAction::Rename,
        RoutineUpdateAction::Arm | RoutineUpdateAction::Disarm => {
            unreachable!("arm/disarm do not write routine lifecycle transitions")
        }
    }
}

const fn is_arm_action(action: RoutineUpdateAction) -> bool {
    matches!(
        action,
        RoutineUpdateAction::Arm | RoutineUpdateAction::Disarm
    )
}

/// The lifecycle a legal transition lands in, or a structured refusal.
fn transition_target(
    action: RoutineUpdateAction,
    current: RoutineLifecycle,
) -> Result<RoutineLifecycle, ErrorData> {
    use RoutineLifecycle::{Archived, Candidate, Confirmed, Disabled};
    let target = match (action, current) {
        (RoutineUpdateAction::Confirm, Candidate) => Confirmed,
        (RoutineUpdateAction::Disable, Candidate | Confirmed) => Disabled,
        (RoutineUpdateAction::Enable, Disabled | Archived) => Candidate,
        (RoutineUpdateAction::Archive, Candidate | Confirmed | Disabled) => Archived,
        (RoutineUpdateAction::Rename, current) => current,
        (RoutineUpdateAction::Arm | RoutineUpdateAction::Disarm, current) => current,
        (action, current) => {
            return Err(invalid(format!(
                "ROUTINE_TRANSITION_INVALID: action {action:?} is not legal from lifecycle \
                 {current:?} (confirm: candidate→confirmed; disable: candidate|confirmed→\
                 disabled; enable: disabled|archived→candidate; archive: candidate|confirmed|\
                 disabled→archived)"
            )));
        }
    };
    Ok(target)
}

fn validate_update_fields(params: &RoutineUpdateParams) -> Result<(), ErrorData> {
    match params.action {
        RoutineUpdateAction::Rename => {
            let Some(label) = params.label.as_deref().map(str::trim) else {
                return Err(invalid("routine_update rename requires a label"));
            };
            if label.is_empty() {
                return Err(invalid("routine_update label must not be blank"));
            }
            if label.chars().count() > MAX_LABEL_CHARS {
                return Err(invalid(format!(
                    "routine_update label must be at most {MAX_LABEL_CHARS} characters; got {}",
                    label.chars().count()
                )));
            }
            if label.chars().any(char::is_control) {
                return Err(invalid(
                    "routine_update label must not contain control characters",
                ));
            }
        }
        _ if params.label.is_some() => {
            return Err(invalid(format!(
                "routine_update label is only valid for action=rename; got action={:?}",
                params.action
            )));
        }
        _ => {}
    }
    match params.action {
        RoutineUpdateAction::Arm => {
            let config = ArmRoutineConfig::from_optional(
                params.arm_schedule,
                params.arm_intent,
                params.failure_threshold,
            );
            super::armed_routines::validate_arm_config(config)?;
        }
        _ => {
            if params.arm_schedule.is_some() {
                return Err(invalid(format!(
                    "routine_update arm_schedule is only valid for action=arm; got action={:?}",
                    params.action
                )));
            }
            if params.arm_intent.is_some() {
                return Err(invalid(format!(
                    "routine_update arm_intent is only valid for action=arm; got action={:?}",
                    params.action
                )));
            }
            if params.failure_threshold.is_some() {
                return Err(invalid(format!(
                    "routine_update failure_threshold is only valid for action=arm; got action={:?}",
                    params.action
                )));
            }
        }
    }
    if let Some(note) = &params.note {
        if note.trim().is_empty() {
            return Err(invalid("routine_update note must not be blank when set"));
        }
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(invalid(format!(
                "routine_update note must be at most {MAX_NOTE_CHARS} characters; got {}",
                note.chars().count()
            )));
        }
    }
    Ok(())
}

fn routine_state_for_update_response(
    db: &Arc<Db>,
    routine_id: &str,
) -> Result<(RoutineStateRecord, bool), ErrorData> {
    let existing_state = load_state_row(db, routine_id)?;
    let state_row_created = existing_state.is_none();
    let state = match existing_state {
        Some(state) => state,
        None => {
            let Some(record) = load_routine_record(db, routine_id)? else {
                return Err(invalid(format!(
                    "ROUTINE_NOT_FOUND: routine_id {routine_id} exists in neither CF_ROUTINES nor \
                     CF_ROUTINE_STATE; run routine_list to see what exists"
                )));
            };
            synthesized_default_state(&record)
        }
    };
    Ok((state, state_row_created))
}

/// Applies one lifecycle mutation: legality check, audit-trail append,
/// synchronous flushed write, and a physical read-back verification.
pub fn update_routine(
    db: &Arc<Db>,
    params: &RoutineUpdateParams,
    by_session: &str,
) -> Result<RoutineUpdateResponse, ErrorData> {
    validate_routine_id_param("routine_update", &params.routine_id)?;
    validate_update_fields(params)?;

    if is_arm_action(params.action) {
        let (state, _state_row_created) =
            routine_state_for_update_response(db, &params.routine_id)?;
        let armed = match params.action {
            RoutineUpdateAction::Arm => {
                let config = ArmRoutineConfig::from_optional(
                    params.arm_schedule,
                    params.arm_intent,
                    params.failure_threshold,
                );
                Some(arm_routine(
                    db,
                    &params.routine_id,
                    config,
                    by_session,
                    params.note.clone(),
                )?)
            }
            RoutineUpdateAction::Disarm => Some(disarm_routine(
                db,
                &params.routine_id,
                by_session,
                params.note.clone(),
            )?),
            _ => None,
        };
        tracing::info!(
            code = "ROUTINE_ARMING_UPDATED",
            routine_id = %params.routine_id,
            action = ?params.action,
            lifecycle = ?state.lifecycle,
            by = by_session,
            "routine armed automation state persisted and read back"
        );
        return Ok(RoutineUpdateResponse {
            routine_id: params.routine_id.clone(),
            action: params.action,
            lifecycle_before: state.lifecycle,
            lifecycle_after: state.lifecycle,
            label_before: state.label.clone(),
            label_after: state.label.clone(),
            state_row_created: false,
            state,
            armed,
        });
    }

    if !db.pressure_permits_write(cf::CF_ROUTINE_STATE) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "routine_update refused under disk pressure: cf_name={} pressure_level={:?}; \
                 the lifecycle is unchanged",
                cf::CF_ROUTINE_STATE,
                db.pressure_level()
            ),
        ));
    }

    let (mut state, state_row_created) = routine_state_for_update_response(db, &params.routine_id)?;

    let lifecycle_before = state.lifecycle;
    let label_before = state.label.clone();
    let lifecycle_after = transition_target(params.action, lifecycle_before)?;
    let label_after = match params.action {
        RoutineUpdateAction::Rename => params.label.as_deref().map(str::trim).map(str::to_owned),
        _ => label_before.clone(),
    };

    let now = now_ts_ns();
    state.lifecycle = lifecycle_after;
    state.label.clone_from(&label_after);
    state.updated_ts_ns = now;
    push_transition(
        &mut state,
        RoutineTransition {
            ts_ns: now,
            action: action_kind(params.action),
            from: Some(lifecycle_before),
            to: lifecycle_after,
            by: by_session.to_owned(),
            label_before: if params.action == RoutineUpdateAction::Rename {
                label_before.clone()
            } else {
                None
            },
            label_after: if params.action == RoutineUpdateAction::Rename {
                label_after.clone()
            } else {
                None
            },
            note: params.note.clone(),
        },
    );

    put_state_rows(db, std::slice::from_ref(&state))?;

    // Read-your-write against the physical row: the response carries what
    // storage actually holds, never just the in-memory value.
    let readback = load_state_row(db, &params.routine_id)?.ok_or_else(|| {
        internal(format!(
            "ROUTINE_STATE_READBACK_MISSING: CF_ROUTINE_STATE row for {} vanished immediately \
             after a flushed write",
            params.routine_id
        ))
    })?;
    if readback != state {
        return Err(internal(format!(
            "ROUTINE_STATE_READBACK_MISMATCH: CF_ROUTINE_STATE row for {} does not match the \
             value just written; expected lifecycle {:?}, found {:?}",
            params.routine_id, state.lifecycle, readback.lifecycle
        )));
    }

    tracing::info!(
        code = "ROUTINE_LIFECYCLE_TRANSITION",
        routine_id = %params.routine_id,
        action = ?params.action,
        lifecycle_before = ?lifecycle_before,
        lifecycle_after = ?lifecycle_after,
        label_before = label_before.as_deref(),
        label_after = label_after.as_deref(),
        by = by_session,
        state_row_created,
        "routine lifecycle transition persisted and read back"
    );

    let armed = load_armed_routine_record(db, &params.routine_id)?;
    Ok(RoutineUpdateResponse {
        routine_id: params.routine_id.clone(),
        action: params.action,
        lifecycle_before,
        lifecycle_after,
        label_before,
        label_after,
        state_row_created,
        state: readback,
        armed,
    })
}
