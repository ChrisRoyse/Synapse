//! `episode_segment` (#846), `episode_list`, and `episode_get` (#847) MCP
//! tools (epic #830).
//!
//! `episode_segment` runs the deterministic segmentation engine
//! ([`synapse_core::episodes::segment_range`]) over `CF_TIMELINE` rows and
//! persists the resulting episodes in `CF_EPISODES`. `episode_list` and
//! `episode_get` are the read surface the digest, miner, dashboard, and
//! labeling tasks build on.
//!
//! Idempotency model: the requested range is snapped OUTWARD to whole local
//! days, and each day is segmented and replaced atomically (delete that
//! day's episode keys + write the new ones in one synchronous flushed
//! batch). Because the engine never lets an episode span local midnight, a
//! day is a closed replacement unit: re-segmenting any day range converges
//! to the same physical rows, byte for byte.
//!
//! Failure policy: disk pressure refusal, undecodable rows, and engine
//! errors are loud and structured. The tool never deletes rows it could not
//! re-derive (a failed day leaves storage untouched for that day).

use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{Days, Local, TimeZone};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use synapse_core::episodes::{SegmentationConfig, segment_range};
use synapse_core::error_codes;
use synapse_core::types::{EpisodeBoundary, EpisodeRecord, TimelineActor, TimelineRecord};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{
    cf, decode_json, encode_json, episodes as episode_codec, timeline as timeline_codec,
};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

/// Maximum timeline rows scanned per call; a larger range pauses at a day
/// boundary and returns `next_start_ts_ns`.
pub const MAX_SCAN_ROWS_PER_CALL: usize = 200_000;
/// Maximum local days replaced per call.
pub const MAX_DAYS_PER_CALL: u32 = 92;
/// Chunk size for bounded storage reads inside one call.
const SCAN_CHUNK_ROWS: usize = 4_096;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeSegmentParams {
    /// Inclusive lower bound; snapped DOWN to its local midnight. Defaults
    /// to the first timeline row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Exclusive upper bound; snapped UP to the next local midnight.
    /// Defaults to the end of the current local day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    /// Segment agent-actor rows too (default false: human activity only).
    #[serde(default)]
    pub include_agent_activity: bool,
    /// Compute everything but mutate nothing.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeSegmentDay {
    /// Local-midnight day start (ns since epoch, UTC clock).
    pub day_start_ns: u64,
    pub day_end_ns: u64,
    /// Decodable timeline rows fed to the engine for this day.
    pub timeline_rows: u64,
    pub episodes_written: u64,
    pub episodes_deleted: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeSegmentResponse {
    /// Effective day-snapped range this call covered.
    pub range_start_ns: u64,
    pub range_end_ns: u64,
    pub days_processed: u32,
    /// Timeline rows examined (decodable or not).
    pub scanned_rows: u64,
    /// Rows whose key or value failed to decode; details are in daemon logs
    /// under code `TIMELINE_ROW_DECODE_FAILED`. Never segmented, never
    /// silently dropped from this count.
    pub invalid_rows: u64,
    /// Agent-actor rows ignored (include_agent_activity=false).
    pub ignored_agent_rows: u64,
    /// Rows with missing/anomalous payload fields (counted by the engine).
    pub payload_anomalies: u64,
    pub episodes_written: u64,
    pub episodes_deleted: u64,
    pub dry_run: bool,
    /// Per-day breakdown for days that had rows or stale episodes.
    pub days: Vec<EpisodeSegmentDay>,
    /// Present when the scan budget or day cap paused the run; pass back as
    /// `start_ts_ns` to continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_start_ts_ns: Option<u64>,
    /// `range_complete`, `scan_budget_exhausted`, `day_cap_reached`, or
    /// `empty_timeline`.
    pub stopped_because: String,
}

#[must_use]
pub const fn episode_segment() -> M3ToolStub {
    M3ToolStub::new("episode_segment")
}

#[must_use]
pub fn required_permissions(_params: &EpisodeSegmentParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
}

fn internal(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail.into())
}

fn lock_runtime(
    runtime: &Arc<Mutex<ReflexRuntime>>,
) -> Result<MutexGuard<'_, ReflexRuntime>, ErrorData> {
    runtime.lock().map_err(|_err| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned",
        )
    })
}

pub(crate) fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    output
}

/// Local midnight at or before `ts_ns`, as ns since epoch.
///
/// Uses the host timezone database via `chrono::Local`; DST transitions are
/// resolved to the earliest valid local instant, and an unresolvable local
/// time is a structured error, never a guess.
pub(crate) fn local_day_start(ts_ns: u64) -> Result<u64, ErrorData> {
    let ts = i64::try_from(ts_ns)
        .map_err(|_e| invalid(format!("timestamp {ts_ns} exceeds the representable range")))?;
    let instant = Local
        .timestamp_nanos(ts)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| internal("midnight is unrepresentable for the local date"))?;
    let midnight = Local
        .from_local_datetime(&instant)
        .earliest()
        .or_else(|| Local.from_local_datetime(&instant).latest())
        .ok_or_else(|| {
            internal(format!(
                "EPISODE_DAY_BOUNDARY_UNRESOLVABLE: no valid local instant for midnight of ts_ns {ts_ns}"
            ))
        })?;
    let nanos = midnight.timestamp_nanos_opt().ok_or_else(|| {
        internal(format!(
            "EPISODE_DAY_BOUNDARY_UNRESOLVABLE: midnight of ts_ns {ts_ns} overflows nanoseconds"
        ))
    })?;
    u64::try_from(nanos).map_err(|_e| {
        invalid(format!(
            "ts_ns {ts_ns} predates the epoch after day snapping"
        ))
    })
}

/// Local midnight strictly after `day_start_ns` (the next local day).
pub(crate) fn next_local_day_start(day_start_ns: u64) -> Result<u64, ErrorData> {
    let ts = i64::try_from(day_start_ns).map_err(|_e| {
        invalid(format!(
            "timestamp {day_start_ns} exceeds the representable range"
        ))
    })?;
    let date = Local
        .timestamp_nanos(ts)
        .date_naive()
        .checked_add_days(Days::new(1))
        .ok_or_else(|| internal("next local day overflows the calendar"))?;
    let instant = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| internal("midnight is unrepresentable for the next local date"))?;
    let midnight = Local
        .from_local_datetime(&instant)
        .earliest()
        .or_else(|| Local.from_local_datetime(&instant).latest())
        .ok_or_else(|| {
            internal(format!(
                "EPISODE_DAY_BOUNDARY_UNRESOLVABLE: no valid local instant for the day after {day_start_ns}"
            ))
        })?;
    let nanos = midnight.timestamp_nanos_opt().ok_or_else(|| {
        internal(format!(
            "EPISODE_DAY_BOUNDARY_UNRESOLVABLE: day after {day_start_ns} overflows nanoseconds"
        ))
    })?;
    u64::try_from(nanos).map_err(|_e| internal("next local day precedes the epoch"))
}

pub(crate) fn now_ts_ns() -> u64 {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

/// First decodable timeline codec key timestamp, if any.
fn first_timeline_ts(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    scanned_rows: &mut u64,
    invalid_rows: &mut u64,
) -> Result<Option<u64>, ErrorData> {
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(*scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            return Err(internal(
                "EPISODE_SCAN_BUDGET_EXHAUSTED while locating the first timeline row; \
                 pass an explicit start_ts_ns",
            ));
        }
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_TIMELINE, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            return Ok(None);
        }
        for (key, _value) in &rows {
            *scanned_rows += 1;
            if let Ok((ts_ns, _seq)) = timeline_codec::decode_timeline_key(key) {
                return Ok(Some(ts_ns));
            }
            *invalid_rows += 1;
            tracing::warn!(
                code = "TIMELINE_ROW_DECODE_FAILED",
                key_hex = %hex_encode(key),
                "episode_segment skipped a non-codec CF_TIMELINE key while locating the first row"
            );
        }
        if !more {
            return Ok(None);
        }
        let last = rows.last().map(|(key, _value)| key.clone());
        let Some(last) = last else { return Ok(None) };
        start = key_after(&last);
    }
}

/// Collects and decodes the timeline rows of one local day, in key order.
fn day_timeline_rows(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    day_start_ns: u64,
    day_end_ns: u64,
    scanned_rows: &mut u64,
    invalid_rows: &mut u64,
) -> Result<(Vec<TimelineRecord>, Option<u64>), ErrorData> {
    let mut records = Vec::new();
    let mut next_populated_ts: Option<u64> = None;
    let mut start = timeline_codec::timeline_scan_start(day_start_ns);
    'scan: loop {
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_TIMELINE, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            *scanned_rows += 1;
            match timeline_codec::decode_timeline_key(key) {
                Ok((ts_ns, _seq)) => {
                    if ts_ns >= day_end_ns {
                        next_populated_ts = Some(ts_ns);
                        break 'scan;
                    }
                    match decode_json::<TimelineRecord>(value) {
                        Ok(record) => records.push(record),
                        Err(error) => {
                            *invalid_rows += 1;
                            tracing::warn!(
                                code = "TIMELINE_ROW_DECODE_FAILED",
                                key_hex = %hex_encode(key),
                                %error,
                                "episode_segment skipped an undecodable CF_TIMELINE row"
                            );
                        }
                    }
                }
                Err(error) => {
                    *invalid_rows += 1;
                    tracing::warn!(
                        code = "TIMELINE_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(key),
                        %error,
                        "episode_segment skipped a non-codec CF_TIMELINE key"
                    );
                }
            }
        }
        if !more {
            break;
        }
        let last = rows.last().map(|(key, _value)| key.clone());
        let Some(last) = last else { break };
        start = key_after(&last);
    }
    Ok((records, next_populated_ts))
}

/// Existing `CF_EPISODES` keys with start timestamps in `[start_ns, end_ns)`.
fn existing_episode_keys(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    start_ns: u64,
    end_ns: u64,
) -> Result<Vec<Vec<u8>>, ErrorData> {
    let mut keys = Vec::new();
    let mut start = episode_codec::episode_scan_start(start_ns);
    'scan: loop {
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_EPISODES, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, _value) in &rows {
            match episode_codec::decode_episode_key(key) {
                Ok((ts_ns, _ordinal)) => {
                    if ts_ns >= end_ns {
                        break 'scan;
                    }
                    keys.push(key.clone());
                }
                Err(error) => {
                    // A malformed derived-state key is corruption we own:
                    // refuse to replace around it rather than strand it.
                    return Err(mcp_error(
                        error_codes::STORAGE_READ_FAILED,
                        format!(
                            "EPISODE_KEY_INVALID in CF_EPISODES at {}: {error}; refusing to \
                             replace a range containing keys this codec cannot account for",
                            hex_encode(key)
                        ),
                    ));
                }
            }
        }
        if !more {
            break;
        }
        let last = rows.last().map(|(key, _value)| key.clone());
        let Some(last) = last else { break };
        start = key_after(&last);
    }
    Ok(keys)
}

pub fn segment_episodes(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &EpisodeSegmentParams,
) -> Result<EpisodeSegmentResponse, ErrorData> {
    if let (Some(start), Some(end)) = (params.start_ts_ns, params.end_ts_ns)
        && start >= end
    {
        return Err(invalid(format!(
            "episode_segment start_ts_ns {start} must be < end_ts_ns {end}"
        )));
    }

    let config = SegmentationConfig {
        include_agent_activity: params.include_agent_activity,
        ..SegmentationConfig::default()
    };

    let runtime = lock_runtime(runtime)?;
    if !params.dry_run && !runtime.storage_pressure_permits_write(cf::CF_EPISODES) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "episode_segment refused under disk pressure: cf_name={} pressure_level={:?}; \
                 nothing was deleted or written",
                cf::CF_EPISODES,
                runtime.storage_pressure_level()
            ),
        ));
    }

    let mut scanned_rows = 0_u64;
    let mut invalid_rows = 0_u64;

    // Effective range: explicit bounds, else first row → end of today.
    let range_start = match params.start_ts_ns {
        Some(start) => start,
        None => match first_timeline_ts(&runtime, &mut scanned_rows, &mut invalid_rows)? {
            Some(ts_ns) => ts_ns,
            None => {
                return Ok(EpisodeSegmentResponse {
                    range_start_ns: 0,
                    range_end_ns: 0,
                    days_processed: 0,
                    scanned_rows,
                    invalid_rows,
                    ignored_agent_rows: 0,
                    payload_anomalies: 0,
                    episodes_written: 0,
                    episodes_deleted: 0,
                    dry_run: params.dry_run,
                    days: Vec::new(),
                    next_start_ts_ns: None,
                    stopped_because: "empty_timeline".to_owned(),
                });
            }
        },
    };
    let range_end = params.end_ts_ns.unwrap_or_else(now_ts_ns);
    if range_start >= range_end {
        return Err(invalid(format!(
            "episode_segment effective range is empty: start {range_start} >= end {range_end}"
        )));
    }
    let range_start_snapped = local_day_start(range_start)?;
    let range_end_snapped = next_local_day_start(local_day_start(range_end.saturating_sub(1))?)?;

    let mut response = EpisodeSegmentResponse {
        range_start_ns: range_start_snapped,
        range_end_ns: range_end_snapped,
        days_processed: 0,
        scanned_rows: 0,
        invalid_rows: 0,
        ignored_agent_rows: 0,
        payload_anomalies: 0,
        episodes_written: 0,
        episodes_deleted: 0,
        dry_run: params.dry_run,
        days: Vec::new(),
        next_start_ts_ns: None,
        stopped_because: "range_complete".to_owned(),
    };

    let mut day_start = range_start_snapped;
    while day_start < range_end_snapped {
        if response.days_processed >= MAX_DAYS_PER_CALL {
            response.next_start_ts_ns = Some(day_start);
            response.stopped_because = "day_cap_reached".to_owned();
            break;
        }
        if usize::try_from(scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
            response.next_start_ts_ns = Some(day_start);
            response.stopped_because = "scan_budget_exhausted".to_owned();
            break;
        }
        let day_end = next_local_day_start(day_start)?;
        let end_is_day_boundary = day_end < range_end_snapped;

        let (records, next_populated_ts) = day_timeline_rows(
            &runtime,
            day_start,
            day_end,
            &mut scanned_rows,
            &mut invalid_rows,
        )?;
        let timeline_rows = u64::try_from(records.len()).unwrap_or(u64::MAX);

        let segmentation =
            segment_range(&records, day_start, day_end, end_is_day_boundary, &config).map_err(
                |error| {
                    internal(format!(
                        "episode_segment engine failed for day starting {day_start}: {error}"
                    ))
                },
            )?;
        response.ignored_agent_rows += segmentation.ignored_agent_rows;
        response.payload_anomalies += segmentation.payload_anomalies;

        // Replacement unit: this day. Collect stale keys, build new rows,
        // and swap them in one atomic flushed batch.
        let stale_keys = existing_episode_keys(&runtime, day_start, day_end)?;
        let mut new_rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(segmentation.episodes.len());
        for (ordinal, episode) in segmentation.episodes.iter().enumerate() {
            let ordinal = u32::try_from(ordinal).map_err(|_e| {
                internal(format!(
                    "episode ordinal {ordinal} overflows u32 for day starting {day_start}"
                ))
            })?;
            let key = episode_codec::episode_key(episode.start_ts_ns, ordinal);
            let value =
                encode_json(episode).map_err(|error| mcp_error(error.code(), error.to_string()))?;
            new_rows.push((key, value));
        }
        let deleted = u64::try_from(stale_keys.len()).unwrap_or(u64::MAX);
        let written = u64::try_from(new_rows.len()).unwrap_or(u64::MAX);
        if !params.dry_run && (deleted > 0 || written > 0) {
            runtime
                .storage_replace_rows(cf::CF_EPISODES, stale_keys, new_rows)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "episode_segment failed to replace day starting {day_start}: {error}; \
                             that day's previous episodes are unchanged"
                        ),
                    )
                })?;
        }
        response.episodes_deleted += deleted;
        response.episodes_written += written;
        response.days_processed += 1;
        if timeline_rows > 0 || deleted > 0 || written > 0 {
            response.days.push(EpisodeSegmentDay {
                day_start_ns: day_start,
                day_end_ns: day_end,
                timeline_rows,
                episodes_written: written,
                episodes_deleted: deleted,
            });
        }
        tracing::info!(
            code = "EPISODE_DAY_SEGMENTED",
            day_start_ns = day_start,
            day_end_ns = day_end,
            timeline_rows,
            episodes_written = written,
            episodes_deleted = deleted,
            dry_run = params.dry_run,
            "episode_segment replaced one local day"
        );

        // Skip empty stretches fast, but never past days holding stale
        // episodes: jump to the next populated timeline day (clamped to the
        // range) unless a skipped day still holds episode rows to clean.
        let target_day = match next_populated_ts {
            Some(ts_ns) => local_day_start(ts_ns)?.clamp(day_end, range_end_snapped),
            None => range_end_snapped,
        };
        day_start = if target_day > day_end {
            let stale_between = existing_episode_keys(&runtime, day_end, target_day)?;
            if stale_between.is_empty() {
                target_day
            } else {
                day_end
            }
        } else {
            day_end
        };
    }

    response.scanned_rows = scanned_rows;
    response.invalid_rows = invalid_rows;
    Ok(response)
}

// === episode_list / episode_get (#847) ===

/// Default number of episodes returned when `limit` is omitted.
pub const DEFAULT_LIST_LIMIT: u32 = 100;
/// Hard upper bound for `episode_list` `limit`.
pub const MAX_LIST_LIMIT: u32 = 500;
/// Default number of timeline row refs returned by `episode_get`.
pub const DEFAULT_REFS_LIMIT: u32 = 500;
/// Hard upper bound for `episode_get` `refs_limit`.
pub const MAX_REFS_LIMIT: u32 = 5_000;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeListParams {
    /// Inclusive lower bound: episodes whose span overlaps
    /// `[start_ts_ns, end_ts_ns]` match. Defaults to the start of the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Inclusive upper bound of the overlap window. Defaults to unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    /// Case-insensitive exact matches on the episode `app` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// `human` or `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Only episodes at least this long (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_duration_ms: Option<u64>,
    /// Maximum episodes to return (default 100, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque continuation cursor from a previous response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// One episode as returned by the query surface: the full persisted record
/// plus its physical row identity and derived duration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeView {
    /// Hex-encoded `CF_EPISODES` storage key (stable row identity; also the
    /// pagination anchor).
    pub key_hex: String,
    /// Episode index within its segmentation day (key tie-breaker).
    pub ordinal: u32,
    /// Stable deterministic id (`ep1-` + 16 hex chars).
    pub episode_id: String,
    pub start_ts_ns: u64,
    pub end_ts_ns: u64,
    pub duration_ms: u64,
    /// `human` or `agent:<session_id>`.
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_first: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_last: Option<String>,
    pub distinct_title_count: u32,
    /// Timeline evidence rows that fed this episode.
    pub row_count: u64,
    pub keystroke_count: u64,
    pub click_count: u64,
    pub interruption_count: u32,
    pub interrupted_ms: u64,
    pub started_because: EpisodeBoundary,
    pub ended_because: EpisodeBoundary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeListResponse {
    /// Matching episodes in chronological (start, ordinal) order.
    pub episodes: Vec<EpisodeView>,
    /// `CF_EPISODES` rows examined this call (matching or not).
    pub scanned_rows: u64,
    /// Present when more episodes may match; pass back as `cursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// `limit_reached`, `scan_budget_exhausted`, `end_ts_reached`, or
    /// `end_of_episodes`.
    pub stopped_because: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeGetParams {
    /// Stable episode id from `episode_list`/`episode_segment` rows
    /// (`ep1-` + 16 hex chars).
    pub episode_id: String,
    /// Optional seek hint: scanning for the id starts at this timestamp
    /// instead of the beginning of `CF_EPISODES`. Pass the episode's
    /// `start_ts_ns` (or any earlier instant) when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Maximum timeline row refs to return (default 500, max 5000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs_limit: Option<u32>,
    /// Opaque continuation cursor from a previous response's
    /// `next_refs_cursor`, to page through a long episode's evidence rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs_cursor: Option<String>,
}

/// One underlying `CF_TIMELINE` evidence row reference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineRowRef {
    /// Hex-encoded `CF_TIMELINE` storage key (fetch the full row via
    /// `timeline_search`).
    pub key_hex: String,
    pub ts_ns: u64,
    /// Key sequence component.
    pub seq: u32,
    /// Snake-case record kind (e.g. `focus_change`).
    pub kind: String,
    /// `human` or `agent:<session_id>`.
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeGetResponse {
    pub episode: EpisodeView,
    /// `CF_EPISODES` rows examined while locating the id.
    pub episode_scanned_rows: u64,
    /// `CF_TIMELINE` rows inside the episode span `[start_ts_ns, end_ts_ns]`,
    /// in chronological order. Includes the boundary row that closed the
    /// episode; agent rows are included even when segmentation excluded them.
    pub timeline_refs: Vec<TimelineRowRef>,
    /// `CF_TIMELINE` rows examined for refs this call.
    pub refs_scanned_rows: u64,
    /// Timeline rows in the span whose value failed to decode; details are
    /// in daemon logs under code `TIMELINE_ROW_DECODE_FAILED`.
    pub refs_invalid_rows: u64,
    /// Present when more refs remain; pass back as `refs_cursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_refs_cursor: Option<String>,
    /// `range_complete`, `refs_limit_reached`, or `scan_budget_exhausted`.
    pub refs_stopped_because: String,
}

#[must_use]
pub const fn episode_list() -> M3ToolStub {
    M3ToolStub::new("episode_list")
}

#[must_use]
pub const fn episode_get() -> M3ToolStub {
    M3ToolStub::new("episode_get")
}

#[must_use]
pub fn required_permissions_list(_params: &EpisodeListParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[must_use]
pub fn required_permissions_get(_params: &EpisodeGetParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

/// `human` or `agent:<session_id>`, matching `timeline_search` output.
fn actor_name(actor: &TimelineActor) -> String {
    match actor {
        TimelineActor::Human => "human".to_owned(),
        TimelineActor::Agent { session_id } => format!("agent:{session_id}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EpisodeActorFilter {
    Human,
    Agent,
}

/// Decodes a `CF_EPISODES` row or fails loudly: this is derived state we
/// own, so an undecodable key or value is corruption to surface, never a row
/// to skip.
pub(crate) fn decode_episode_row(
    key: &[u8],
    value: &[u8],
) -> Result<(u64, u32, EpisodeRecord), ErrorData> {
    let (key_ts_ns, ordinal) = episode_codec::decode_episode_key(key).map_err(|error| {
        tracing::error!(
            code = "EPISODE_KEY_INVALID",
            key_hex = %hex_encode(key),
            %error,
            "CF_EPISODES holds a key its codec cannot decode"
        );
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "EPISODE_KEY_INVALID in CF_EPISODES at {}: {error}; CF_EPISODES is derived \
                 state — re-run episode_segment for the affected day after removing the row",
                hex_encode(key)
            ),
        )
    })?;
    let record = decode_json::<EpisodeRecord>(value).map_err(|error| {
        tracing::error!(
            code = "EPISODE_ROW_DECODE_FAILED",
            key_hex = %hex_encode(key),
            %error,
            "CF_EPISODES holds a value that does not decode as an EpisodeRecord"
        );
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "EPISODE_ROW_DECODE_FAILED in CF_EPISODES at {}: {error}; CF_EPISODES is \
                 derived state — re-run episode_segment for the affected day",
                hex_encode(key)
            ),
        )
    })?;
    Ok((key_ts_ns, ordinal, record))
}

fn episode_view(key: &[u8], ordinal: u32, record: EpisodeRecord) -> EpisodeView {
    EpisodeView {
        key_hex: hex_encode(key),
        ordinal,
        duration_ms: record.duration_ms(),
        actor: actor_name(&record.actor),
        episode_id: record.episode_id,
        start_ts_ns: record.start_ts_ns,
        end_ts_ns: record.end_ts_ns,
        app: record.app,
        document: record.document,
        url: record.url,
        title_first: record.title_first,
        title_last: record.title_last,
        distinct_title_count: record.distinct_title_count,
        row_count: record.row_count,
        keystroke_count: record.keystroke_count,
        click_count: record.click_count,
        interruption_count: record.interruption_count,
        interrupted_ms: record.interrupted_ms,
        started_because: record.started_because,
        ended_because: record.ended_because,
    }
}

/// Local midnight at or before `ts_ns`, clamped to the epoch.
///
/// This is the overlap-scan floor: an episode never spans local midnight
/// (#846 invariant), so any episode overlapping an instant must have started
/// at or after that instant's local midnight. A midnight that precedes the
/// epoch clamps to key 0 — the scan bound cannot go lower anyway.
fn overlap_scan_floor(ts_ns: u64) -> Result<u64, ErrorData> {
    if ts_ns == 0 {
        return Ok(0);
    }
    let ts = i64::try_from(ts_ns)
        .map_err(|_e| invalid(format!("timestamp {ts_ns} exceeds the representable range")))?;
    let instant = Local
        .timestamp_nanos(ts)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| internal("midnight is unrepresentable for the local date"))?;
    let midnight = Local
        .from_local_datetime(&instant)
        .earliest()
        .or_else(|| Local.from_local_datetime(&instant).latest())
        .ok_or_else(|| {
            internal(format!(
                "EPISODE_DAY_BOUNDARY_UNRESOLVABLE: no valid local instant for midnight of ts_ns {ts_ns}"
            ))
        })?;
    let nanos = midnight.timestamp_nanos_opt().ok_or_else(|| {
        internal(format!(
            "EPISODE_DAY_BOUNDARY_UNRESOLVABLE: midnight of ts_ns {ts_ns} overflows nanoseconds"
        ))
    })?;
    Ok(u64::try_from(nanos).unwrap_or(0))
}

#[derive(Debug)]
struct ListFilters {
    start_ts_ns: u64,
    end_ts_ns: u64,
    apps_lower: Vec<String>,
    actor: Option<EpisodeActorFilter>,
    min_duration_ms: u64,
    limit: usize,
    start_key: Vec<u8>,
}

fn validate_list(params: &EpisodeListParams) -> Result<ListFilters, ErrorData> {
    let start_ts_ns = params.start_ts_ns.unwrap_or(0);
    let end_ts_ns = params.end_ts_ns.unwrap_or(u64::MAX);
    if start_ts_ns > end_ts_ns {
        return Err(invalid(format!(
            "episode_list start_ts_ns {start_ts_ns} must be <= end_ts_ns {end_ts_ns}"
        )));
    }
    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(invalid(format!(
            "episode_list limit must be between 1 and {MAX_LIST_LIMIT}; got {limit}"
        )));
    }
    let apps_lower = params
        .apps
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|app| {
            let trimmed = app.trim();
            if trimmed.is_empty() {
                Err(invalid("episode_list apps entries must not be empty"))
            } else {
                Ok(trimmed.to_lowercase())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let actor = params
        .actor
        .as_deref()
        .map(|actor| match actor.trim().to_lowercase().as_str() {
            "human" => Ok(EpisodeActorFilter::Human),
            "agent" => Ok(EpisodeActorFilter::Agent),
            other => Err(invalid(format!(
                "episode_list actor must be \"human\" or \"agent\"; got {other:?}"
            ))),
        })
        .transpose()?;
    let start_key = match params.cursor.as_deref() {
        Some(cursor) => {
            let decoded = hex_decode(cursor).ok_or_else(|| {
                invalid("episode_list cursor is not a valid hex key from a prior response")
            })?;
            episode_codec::decode_episode_key(&decoded).map_err(|error| {
                invalid(format!(
                    "episode_list cursor does not decode as a CF_EPISODES key: {error}"
                ))
            })?;
            key_after(&decoded)
        }
        None => episode_codec::episode_scan_start(overlap_scan_floor(start_ts_ns)?),
    };
    Ok(ListFilters {
        start_ts_ns,
        end_ts_ns,
        apps_lower,
        actor,
        min_duration_ms: params.min_duration_ms.unwrap_or(0),
        limit: limit as usize,
        start_key,
    })
}

fn episode_matches(record: &EpisodeRecord, filters: &ListFilters) -> bool {
    // Inclusive interval overlap: the span touches [start, end].
    if record.end_ts_ns < filters.start_ts_ns || record.start_ts_ns > filters.end_ts_ns {
        return false;
    }
    if record.duration_ms() < filters.min_duration_ms {
        return false;
    }
    if let Some(actor) = filters.actor {
        let is_human = matches!(record.actor, TimelineActor::Human);
        if (actor == EpisodeActorFilter::Human) != is_human {
            return false;
        }
    }
    if !filters.apps_lower.is_empty() {
        let Some(app) = record.app.as_deref() else {
            return false;
        };
        if !filters.apps_lower.contains(&app.to_lowercase()) {
            return false;
        }
    }
    true
}

pub fn list_episodes(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &EpisodeListParams,
) -> Result<EpisodeListResponse, ErrorData> {
    let filters = validate_list(params)?;
    let runtime = lock_runtime(runtime)?;

    let mut episodes = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut next_start = filters.start_key.clone();
    let mut last_key: Option<Vec<u8>> = None;
    let mut stopped_because = "end_of_episodes";
    let mut storage_has_more = false;

    'scan: loop {
        let remaining_budget = MAX_SCAN_ROWS_PER_CALL - usize::try_from(scanned_rows).unwrap_or(0);
        if remaining_budget == 0 {
            stopped_because = "scan_budget_exhausted";
            break;
        }
        let chunk_rows = SCAN_CHUNK_ROWS.min(remaining_budget);
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_EPISODES, &next_start, chunk_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        storage_has_more = more;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            scanned_rows += 1;
            last_key = Some(key.clone());
            let (key_ts_ns, ordinal, record) = decode_episode_row(key, value)?;
            // Keys iterate in start-timestamp order, so the first start past
            // the end bound proves no later episode can overlap the range.
            if key_ts_ns > filters.end_ts_ns {
                stopped_because = "end_ts_reached";
                storage_has_more = false;
                break 'scan;
            }
            if episode_matches(&record, &filters) {
                episodes.push(episode_view(key, ordinal, record));
                if episodes.len() >= filters.limit {
                    stopped_because = "limit_reached";
                    break 'scan;
                }
            }
        }
        if !more {
            break;
        }
        let Some(last) = last_key.as_ref() else { break };
        next_start = key_after(last);
    }
    drop(runtime);

    let resume_possible = matches!(stopped_because, "limit_reached" | "scan_budget_exhausted")
        && (storage_has_more || stopped_because == "limit_reached");
    let next_cursor = if resume_possible {
        last_key.as_deref().map(hex_encode)
    } else {
        None
    };
    Ok(EpisodeListResponse {
        episodes,
        scanned_rows,
        next_cursor,
        stopped_because: stopped_because.to_owned(),
    })
}

/// Locates one episode by id with a bounded scan.
fn find_episode_by_id(
    runtime: &MutexGuard<'_, ReflexRuntime>,
    episode_id: &str,
    seek_from_ts_ns: u64,
    scanned_rows: &mut u64,
) -> Result<(Vec<u8>, u32, EpisodeRecord), ErrorData> {
    let mut next_start = episode_codec::episode_scan_start(seek_from_ts_ns);
    loop {
        let remaining_budget = MAX_SCAN_ROWS_PER_CALL - usize::try_from(*scanned_rows).unwrap_or(0);
        if remaining_budget == 0 {
            return Err(internal(format!(
                "EPISODE_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} CF_EPISODES rows \
                 without finding {episode_id:?}; pass start_ts_ns to seek closer to the episode"
            )));
        }
        let chunk_rows = SCAN_CHUNK_ROWS.min(remaining_budget);
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_EPISODES, &next_start, chunk_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            *scanned_rows += 1;
            let (_key_ts_ns, ordinal, record) = decode_episode_row(key, value)?;
            if record.episode_id == episode_id {
                return Ok((key.clone(), ordinal, record));
            }
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        next_start = key_after(last);
    }
    Err(mcp_error(
        error_codes::EPISODE_NOT_FOUND,
        format!(
            "EPISODE_NOT_FOUND: no CF_EPISODES row has episode_id {episode_id:?} \
             (scanned {scanned} rows from ts {seek_from_ts_ns}); if a start_ts_ns hint was \
             passed, the episode may start earlier — retry without the hint",
            scanned = *scanned_rows
        ),
    ))
}

pub fn get_episode(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &EpisodeGetParams,
) -> Result<EpisodeGetResponse, ErrorData> {
    let episode_id = params.episode_id.trim();
    if episode_id.is_empty() {
        return Err(invalid("episode_get episode_id must not be empty"));
    }
    let refs_limit = params.refs_limit.unwrap_or(DEFAULT_REFS_LIMIT);
    if refs_limit == 0 || refs_limit > MAX_REFS_LIMIT {
        return Err(invalid(format!(
            "episode_get refs_limit must be between 1 and {MAX_REFS_LIMIT}; got {refs_limit}"
        )));
    }
    let refs_cursor_key = params
        .refs_cursor
        .as_deref()
        .map(|cursor| {
            let decoded = hex_decode(cursor).ok_or_else(|| {
                invalid("episode_get refs_cursor is not a valid hex key from a prior response")
            })?;
            timeline_codec::decode_timeline_key(&decoded).map_err(|error| {
                invalid(format!(
                    "episode_get refs_cursor does not decode as a CF_TIMELINE key: {error}"
                ))
            })?;
            Ok::<Vec<u8>, ErrorData>(decoded)
        })
        .transpose()?;

    let runtime = lock_runtime(runtime)?;
    let mut episode_scanned_rows = 0_u64;
    let (episode_key, ordinal, record) = find_episode_by_id(
        &runtime,
        episode_id,
        params.start_ts_ns.unwrap_or(0),
        &mut episode_scanned_rows,
    )?;

    // Evidence window: every CF_TIMELINE row with ts inside the inclusive
    // episode span, including the boundary row that closed it and agent rows
    // segmentation may have excluded.
    let span_start = record.start_ts_ns;
    let span_end = record.end_ts_ns;
    let mut timeline_refs: Vec<TimelineRowRef> = Vec::new();
    let mut refs_scanned_rows = 0_u64;
    let mut refs_invalid_rows = 0_u64;
    let mut last_key: Option<Vec<u8>> = None;
    let mut refs_stopped_because = "range_complete";
    let mut storage_has_more = false;
    let mut next_start = match refs_cursor_key {
        Some(cursor_key) => key_after(&cursor_key),
        None => timeline_codec::timeline_scan_start(span_start),
    };

    'scan: loop {
        let remaining_budget =
            MAX_SCAN_ROWS_PER_CALL - usize::try_from(refs_scanned_rows).unwrap_or(0);
        if remaining_budget == 0 {
            refs_stopped_because = "scan_budget_exhausted";
            break;
        }
        let chunk_rows = SCAN_CHUNK_ROWS.min(remaining_budget);
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_TIMELINE, &next_start, chunk_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        storage_has_more = more;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            refs_scanned_rows += 1;
            last_key = Some(key.clone());
            match timeline_codec::decode_timeline_key(key) {
                Ok((ts_ns, seq)) => {
                    if ts_ns > span_end {
                        storage_has_more = false;
                        break 'scan;
                    }
                    match decode_json::<TimelineRecord>(value) {
                        Ok(timeline_record) => {
                            timeline_refs.push(TimelineRowRef {
                                key_hex: hex_encode(key),
                                ts_ns,
                                seq,
                                kind: timeline_kind_name(timeline_record.kind),
                                actor: actor_name(&timeline_record.actor),
                                app: timeline_record.app,
                            });
                            if timeline_refs.len() >= refs_limit as usize {
                                refs_stopped_because = "refs_limit_reached";
                                break 'scan;
                            }
                        }
                        Err(error) => {
                            refs_invalid_rows += 1;
                            tracing::warn!(
                                code = "TIMELINE_ROW_DECODE_FAILED",
                                key_hex = %hex_encode(key),
                                %error,
                                "episode_get skipped an undecodable CF_TIMELINE row"
                            );
                        }
                    }
                }
                Err(error) => {
                    refs_invalid_rows += 1;
                    tracing::warn!(
                        code = "TIMELINE_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(key),
                        %error,
                        "episode_get skipped a non-codec CF_TIMELINE key"
                    );
                }
            }
        }
        if !more {
            break;
        }
        let Some(last) = last_key.as_ref() else { break };
        next_start = key_after(last);
    }
    drop(runtime);

    let resume_possible = matches!(
        refs_stopped_because,
        "refs_limit_reached" | "scan_budget_exhausted"
    ) && (storage_has_more || refs_stopped_because == "refs_limit_reached");
    let next_refs_cursor = if resume_possible {
        last_key.as_deref().map(hex_encode)
    } else {
        None
    };
    Ok(EpisodeGetResponse {
        episode: episode_view(&episode_key, ordinal, record),
        episode_scanned_rows,
        timeline_refs,
        refs_scanned_rows,
        refs_invalid_rows,
        next_refs_cursor,
        refs_stopped_because: refs_stopped_because.to_owned(),
    })
}

fn timeline_kind_name(kind: synapse_core::types::TimelineKind) -> String {
    serde_json::to_value(kind).map_or_else(
        |_error| format!("{kind:?}"),
        |value| value.as_str().unwrap_or_default().to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use synapse_core::types::EPISODE_RECORD_VERSION;

    use super::*;

    fn episode(start_ts_ns: u64, end_ts_ns: u64, app: &str) -> EpisodeRecord {
        EpisodeRecord {
            record_version: EPISODE_RECORD_VERSION,
            ts_ns: start_ts_ns,
            episode_id: format!("ep1-{start_ts_ns:016x}"),
            start_ts_ns,
            end_ts_ns,
            actor: TimelineActor::Human,
            app: Some(app.to_owned()),
            document: None,
            url: None,
            title_first: None,
            title_last: None,
            distinct_title_count: 0,
            row_count: 1,
            keystroke_count: 0,
            click_count: 0,
            interruption_count: 0,
            interrupted_ms: 0,
            started_because: EpisodeBoundary::AppSwitch,
            ended_because: EpisodeBoundary::AppSwitch,
        }
    }

    fn filters() -> ListFilters {
        ListFilters {
            start_ts_ns: 0,
            end_ts_ns: u64::MAX,
            apps_lower: Vec::new(),
            actor: None,
            min_duration_ms: 0,
            limit: 10,
            start_key: Vec::new(),
        }
    }

    #[test]
    fn overlap_matches_inclusive_boundaries_and_rejects_disjoint_spans() {
        let row = episode(1_000, 2_000, "code.exe");
        let mut window = filters();
        // Episode end touching the window start is an overlap.
        window.start_ts_ns = 2_000;
        window.end_ts_ns = 3_000;
        assert!(episode_matches(&row, &window));
        // Episode start touching the window end is an overlap.
        window.start_ts_ns = 500;
        window.end_ts_ns = 1_000;
        assert!(episode_matches(&row, &window));
        // Disjoint on either side is not.
        window.start_ts_ns = 2_001;
        window.end_ts_ns = 3_000;
        assert!(!episode_matches(&row, &window));
        window.start_ts_ns = 0;
        window.end_ts_ns = 999;
        assert!(!episode_matches(&row, &window));
    }

    #[test]
    fn app_actor_and_min_duration_filters_apply() {
        let row = episode(0, 5_000_000_000, "Excel.EXE");
        let mut all = filters();
        all.apps_lower = vec!["excel.exe".to_owned()];
        all.actor = Some(EpisodeActorFilter::Human);
        all.min_duration_ms = 5_000;
        assert!(episode_matches(&row, &all));
        all.min_duration_ms = 5_001;
        assert!(!episode_matches(&row, &all));
        all.min_duration_ms = 0;
        all.actor = Some(EpisodeActorFilter::Agent);
        assert!(!episode_matches(&row, &all));
        all.actor = None;
        all.apps_lower = vec!["chrome.exe".to_owned()];
        assert!(!episode_matches(&row, &all));
    }

    #[test]
    fn validate_list_rejects_bad_ranges_limits_apps_actor_and_cursor() {
        let reject = |params: EpisodeListParams, fragment: &str| {
            let error = validate_list(&params).expect_err(fragment);
            assert!(
                error.message.contains(fragment),
                "expected {fragment:?} in {:?}",
                error.message
            );
        };
        reject(
            EpisodeListParams {
                start_ts_ns: Some(10),
                end_ts_ns: Some(5),
                ..EpisodeListParams::default()
            },
            "must be <=",
        );
        reject(
            EpisodeListParams {
                limit: Some(0),
                ..EpisodeListParams::default()
            },
            "limit",
        );
        reject(
            EpisodeListParams {
                limit: Some(MAX_LIST_LIMIT + 1),
                ..EpisodeListParams::default()
            },
            "limit",
        );
        reject(
            EpisodeListParams {
                apps: Some(vec!["  ".to_owned()]),
                ..EpisodeListParams::default()
            },
            "apps entries",
        );
        reject(
            EpisodeListParams {
                actor: Some("alien".to_owned()),
                ..EpisodeListParams::default()
            },
            "actor",
        );
        reject(
            EpisodeListParams {
                cursor: Some("zz-not-hex".to_owned()),
                ..EpisodeListParams::default()
            },
            "cursor",
        );
        // A hex cursor that is not a CF_EPISODES codec key is rejected too.
        reject(
            EpisodeListParams {
                cursor: Some("0011".to_owned()),
                ..EpisodeListParams::default()
            },
            "cursor",
        );
    }

    #[test]
    fn list_cursor_roundtrips_and_resumes_after_key() {
        let key = episode_codec::episode_key(42_000_000_000, 7);
        let cursor = hex_encode(&key);
        let decoded = hex_decode(&cursor).expect("hex roundtrip");
        assert_eq!(decoded, key);
        let params = EpisodeListParams {
            cursor: Some(cursor),
            ..EpisodeListParams::default()
        };
        let parsed = validate_list(&params).expect("cursor accepted");
        assert_eq!(parsed.start_key, key_after(&key));
    }

    #[test]
    fn overlap_scan_floor_clamps_to_epoch_and_snaps_to_local_midnight() {
        assert_eq!(overlap_scan_floor(0).expect("zero"), 0);
        // Sub-day timestamps near the epoch never error, even west of UTC.
        let floor = overlap_scan_floor(1_000).expect("epoch-adjacent");
        assert_eq!(floor, 0, "midnight before the epoch must clamp to key 0");
        // A modern timestamp snaps down to its local midnight, never above
        // the input and never more than 24h+DST below it.
        let now_ns = u64::try_from(chrono::Utc::now().timestamp_nanos_opt().expect("now"))
            .expect("non-negative");
        let snapped = overlap_scan_floor(now_ns).expect("today");
        assert!(snapped <= now_ns);
        assert!(now_ns - snapped < 25 * 3_600 * 1_000_000_000);
    }

    #[test]
    fn episode_view_carries_identity_duration_and_actor() {
        let key = episode_codec::episode_key(1_000_000_000, 3);
        let mut record = episode(1_000_000_000, 4_500_000_000, "code.exe");
        record.actor = TimelineActor::Agent {
            session_id: "sess-9".to_owned(),
        };
        let view = episode_view(&key, 3, record);
        assert_eq!(view.key_hex, hex_encode(&key));
        assert_eq!(view.ordinal, 3);
        assert_eq!(view.duration_ms, 3_500);
        assert_eq!(view.actor, "agent:sess-9");
        assert_eq!(view.app.as_deref(), Some("code.exe"));
    }
}
