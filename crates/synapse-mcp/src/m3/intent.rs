//! `intent_current` MCP tool (#854, epic #831/#828).
//!
//! The pull-based twin of the `INTENT_DETECTED` event (#855): a read-only
//! now-snapshot of which mined routines the operator appears to be executing
//! right now, ranked with evidence. An agent calls this to decide whether —
//! and what — to suggest.
//!
//! This module is the storage glue around the pure matcher
//! ([`synapse_core::intent::match_intents`]): it reads the three durable
//! sources of truth (`CF_EPISODES` for the recent activity stream,
//! `CF_ROUTINES` for the mined templates, `CF_ROUTINE_STATE` for operator
//! lifecycle) and feeds them to the clock-free engine with a [`NowContext`]
//! derived from the host clock (or an explicit `now_ts_ns` for replay #857).
//!
//! Failure policy mirrors the routine/episode tools: undecodable derived rows
//! and scan-budget exhaustion are loud, structured errors — never a silently
//! truncated, falsely-empty snapshot. The matcher never matches disabled or
//! archived routines (#849).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{Datelike, Local, TimeZone};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_core::intent::{
    IntentCandidate, IntentMatchConfig, NowContext, RoutineForMatch, match_intents,
};
use synapse_core::routines::MINUTES_PER_DAY;
use synapse_core::types::{
    EpisodeRecord, ROUTINE_RECORD_VERSION, ROUTINE_STATE_RECORD_VERSION, RoutineLifecycle,
    RoutineRecord, RoutineStateRecord,
};
use synapse_storage::{Db, cf, decode_json, routines as routine_codec};

use crate::m1::mcp_error;

use super::episodes::{decode_episode_row, hex_encode, key_after, local_day_start, now_ts_ns};
use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

/// Maximum rows scanned per call across all three column families. A truncated
/// scan is a loud error, never a silently-incomplete snapshot.
pub const MAX_SCAN_ROWS_PER_CALL: usize = 200_000;
/// Chunk size for bounded storage reads inside one call.
const SCAN_CHUNK_ROWS: usize = 4_096;

/// Default / bounds for the recent-activity lookback window.
pub const DEFAULT_LOOKBACK_HOURS: u32 = 6;
pub const MAX_LOOKBACK_HOURS: u32 = 168;
/// Default / max returned candidates.
pub const DEFAULT_MAX_CANDIDATES: u32 = 10;
pub const MAX_MAX_CANDIDATES: u32 = 50;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntentCurrentParams {
    /// "As of" instant (ns since epoch) the snapshot is evaluated at. Defaults
    /// to now. Pass an explicit instant to replay a past moment (#857).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ts_ns: Option<u64>,
    /// How far back to gather recent episodes (default 6h, max 168h). The
    /// matcher only needs enough history to cover the longest routine prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_hours: Option<u32>,
    /// Keep only candidates whose combined confidence is at least this
    /// (default 0.0 — return every honest match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,
    /// Maximum candidates to return (default 10, max 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_candidates: Option<u32>,
    /// Match agent-actor episodes too (default false: human intents only).
    #[serde(default)]
    pub include_agent_activity: bool,
}

/// The wall-clock the snapshot was evaluated against, echoed for auditability.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NowEcho {
    pub ts_ns: u64,
    /// 0 = Monday … 6 = Sunday (local).
    pub weekday: u8,
    /// Minute of the local day (0..1440).
    pub minute_of_day: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntentCurrentResponse {
    /// Ranked candidate intents, strongest first. Empty is the honest "nothing
    /// matches" (no forced top-1).
    pub candidates: Vec<IntentCandidate>,
    /// The instant/weekday/minute the match was evaluated against.
    pub now: NowEcho,
    /// Inclusive recent-activity window `[window_start_ns, now_ts_ns]`.
    pub window_start_ns: u64,
    /// `CF_ROUTINES` rows in the store.
    pub total_mined_routines: u64,
    /// `CF_ROUTINE_STATE` rows in the store.
    pub total_state_rows: u64,
    /// Routines fed to the matcher (mined rows joined with lifecycle).
    pub evaluated_routines: u64,
    /// Episodes inside the lookback window passed to the matcher.
    pub considered_episodes: u64,
    /// Rows examined across CF_EPISODES, CF_ROUTINES, CF_ROUTINE_STATE.
    pub scanned_rows: u64,
}

#[must_use]
pub const fn intent_current() -> M3ToolStub {
    M3ToolStub::new("intent_current")
}

#[must_use]
pub fn required_permissions(_params: &IntentCurrentParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
}

fn internal(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail.into())
}

/// Derives the clock-free [`NowContext`] from a local instant.
fn now_context(ts_ns: u64) -> Result<NowContext, ErrorData> {
    let ts = i64::try_from(ts_ns)
        .map_err(|_e| invalid(format!("now_ts_ns {ts_ns} exceeds the representable range")))?;
    let weekday = Local.timestamp_nanos(ts).weekday().num_days_from_monday();
    let weekday = u8::try_from(weekday).map_err(|_e| internal("weekday outside 0..=6"))?;
    let day_start = local_day_start(ts_ns)?;
    let minute_of_day = u32::try_from(ts_ns.saturating_sub(day_start) / 60_000_000_000)
        .unwrap_or(0)
        % MINUTES_PER_DAY;
    Ok(NowContext {
        ts_ns,
        weekday,
        minute_of_day,
    })
}

/// Collects episodes whose start lies in `[window_start_ns, now_ts_ns]`, in key
/// order. Fails loudly on undecodable derived rows and scan-budget exhaustion.
fn recent_episodes(
    db: &Db,
    window_start_ns: u64,
    now_ts_ns: u64,
    scanned_rows: &mut u64,
) -> Result<Vec<EpisodeRecord>, ErrorData> {
    let mut episodes = Vec::new();
    // Episodes never span local midnight (#846), so the scan floor is the
    // window-start's local midnight: no earlier key can start in the window.
    let floor = local_day_start(window_start_ns)?;
    let mut start = synapse_storage::episodes::episode_scan_start(floor);
    'scan: loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(format!(
                "INTENT_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} CF_EPISODES rows; \
                 narrow lookback_hours — matching over a truncated scan would be falsely empty"
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
            if key_ts_ns > now_ts_ns {
                break 'scan;
            }
            if record.start_ts_ns >= window_start_ns && record.start_ts_ns <= now_ts_ns {
                episodes.push(record);
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
    Ok(episodes)
}

/// Decodes one `CF_ROUTINES` row, failing loudly: derived state we own, so a
/// bad key/value/version is corruption to surface, never a row to skip.
fn decode_routine_row(key: &[u8], value: &[u8]) -> Result<RoutineRecord, ErrorData> {
    let routine_id = routine_codec::decode_routine_key(key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "ROUTINE_KEY_INVALID in CF_ROUTINES at {}: {error}; CF_ROUTINES is derived \
                 state — re-run routine_mine",
                hex_encode(key)
            ),
        )
    })?;
    let record = decode_json::<RoutineRecord>(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_ROW_DECODE_FAILED in CF_ROUTINES at {routine_id}: {error}; \
                 CF_ROUTINES is derived state — re-run routine_mine"
            ),
        )
    })?;
    if record.record_version != ROUTINE_RECORD_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_VERSION_UNSUPPORTED in CF_ROUTINES at {routine_id}: record_version {} \
                 (this binary supports {ROUTINE_RECORD_VERSION})",
                record.record_version
            ),
        ));
    }
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

/// Decodes one `CF_ROUTINE_STATE` row, failing loudly: this CF holds operator
/// lifecycle decisions, so corruption is surfaced, never skipped.
fn decode_state_row(key: &[u8], value: &[u8]) -> Result<RoutineStateRecord, ErrorData> {
    let routine_id = routine_codec::decode_routine_state_key(key).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "ROUTINE_STATE_KEY_INVALID in CF_ROUTINE_STATE at {}: {error}",
                hex_encode(key)
            ),
        )
    })?;
    let record = decode_json::<RoutineStateRecord>(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("ROUTINE_STATE_ROW_DECODE_FAILED in CF_ROUTINE_STATE at {routine_id}: {error}"),
        )
    })?;
    if record.record_version != ROUTINE_STATE_RECORD_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_STATE_VERSION_UNSUPPORTED in CF_ROUTINE_STATE at {routine_id}: \
                 record_version {} (this binary supports {ROUTINE_STATE_RECORD_VERSION})",
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

/// Loads every row of a column family via the shared decoder, budget-guarded.
fn load_cf<T, F>(
    db: &Db,
    cf_name: &str,
    scanned_rows: &mut u64,
    decode: F,
) -> Result<Vec<T>, ErrorData>
where
    F: Fn(&[u8], &[u8]) -> Result<T, ErrorData>,
{
    let mut out = Vec::new();
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(format!(
                "INTENT_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} {cf_name} rows; \
                 the store should hold at most a few hundred — inspect {cf_name}"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf_name, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            *scanned_rows += 1;
            out.push(decode(key, value)?);
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(out)
}

/// Computes the current ranked intents. Shared by the MCP tool and (later) the
/// `INTENT_DETECTED` daemon component (#855).
pub fn current_intents(
    db: &Arc<Db>,
    params: &IntentCurrentParams,
) -> Result<IntentCurrentResponse, ErrorData> {
    let lookback_hours = match params.lookback_hours {
        None => DEFAULT_LOOKBACK_HOURS,
        Some(hours) if (1..=MAX_LOOKBACK_HOURS).contains(&hours) => hours,
        Some(hours) => {
            return Err(invalid(format!(
                "intent_current lookback_hours must be between 1 and {MAX_LOOKBACK_HOURS}; \
                 got {hours}"
            )));
        }
    };
    let max_candidates = match params.max_candidates {
        None => DEFAULT_MAX_CANDIDATES,
        Some(value) if (1..=MAX_MAX_CANDIDATES).contains(&value) => value,
        Some(value) => {
            return Err(invalid(format!(
                "intent_current max_candidates must be between 1 and {MAX_MAX_CANDIDATES}; \
                 got {value}"
            )));
        }
    };
    if let Some(min_confidence) = params.min_confidence
        && !(0.0..=1.0).contains(&min_confidence)
    {
        return Err(invalid(format!(
            "intent_current min_confidence must be within [0.0, 1.0]; got {min_confidence}"
        )));
    }

    let now_ts = params.now_ts_ns.unwrap_or_else(now_ts_ns);
    let now = now_context(now_ts)?;
    let lookback_ns = u64::from(lookback_hours).saturating_mul(3_600_000_000_000);
    let window_start_ns = now_ts.saturating_sub(lookback_ns);

    let config = IntentMatchConfig {
        min_combined_confidence: params.min_confidence.unwrap_or(0.0),
        max_candidates: max_candidates as usize,
        include_agent_activity: params.include_agent_activity,
        ..IntentMatchConfig::default()
    };

    let mut scanned_rows = 0_u64;
    let episodes = recent_episodes(db, window_start_ns, now_ts, &mut scanned_rows)?;
    let routines = load_cf(db, cf::CF_ROUTINES, &mut scanned_rows, decode_routine_row)?;
    let states = load_cf(db, cf::CF_ROUTINE_STATE, &mut scanned_rows, decode_state_row)?;

    let total_mined_routines = u64::try_from(routines.len()).unwrap_or(u64::MAX);
    let total_state_rows = u64::try_from(states.len()).unwrap_or(u64::MAX);
    let considered_episodes = u64::try_from(episodes.len()).unwrap_or(u64::MAX);

    // Join lifecycle onto each mined routine. A routine with no state row is an
    // unreviewed candidate (#849: synthesized default), still eligible to
    // match — disabled/archived are filtered inside the matcher.
    let state_by_id: BTreeMap<&str, &RoutineStateRecord> = states
        .iter()
        .map(|state| (state.routine_id.as_str(), state))
        .collect();
    let for_match: Vec<RoutineForMatch> = routines
        .into_iter()
        .map(|record| {
            let state = state_by_id.get(record.routine_id.as_str());
            let lifecycle = state.map_or(RoutineLifecycle::Candidate, |state| state.lifecycle);
            let label = state.and_then(|state| state.label.clone());
            RoutineForMatch {
                record,
                lifecycle,
                label,
            }
        })
        .collect();
    let evaluated_routines = u64::try_from(for_match.len()).unwrap_or(u64::MAX);

    let candidates = match_intents(&episodes, &for_match, now, &config).map_err(|error| {
        internal(format!("intent_current matcher failed: {error}"))
    })?;

    tracing::info!(
        code = "INTENT_CURRENT_COMPUTED",
        now_ts_ns = now_ts,
        weekday = now.weekday,
        minute_of_day = now.minute_of_day,
        window_start_ns,
        evaluated_routines,
        considered_episodes,
        candidates = candidates.len(),
        scanned_rows,
        "intent_current computed a ranked snapshot"
    );

    Ok(IntentCurrentResponse {
        candidates,
        now: NowEcho {
            ts_ns: now.ts_ns,
            weekday: now.weekday,
            minute_of_day: now.minute_of_day,
        },
        window_start_ns,
        total_mined_routines,
        total_state_rows,
        evaluated_routines,
        considered_episodes,
        scanned_rows,
    })
}
