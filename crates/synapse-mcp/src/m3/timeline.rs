//! `timeline_search` (#841) and `timeline_purge` (#843) MCP tools
//! (ADR 2026-06-11-timeline-data-model).
//!
//! Searches `CF_TIMELINE` rows by time range, app, record kind, actor, and
//! case-insensitive text over the record's app and payload string values
//! (titles, paths, URLs, clipboard snippets). Results page via an opaque
//! cursor; per-call scan work is budgeted so one query can never pin the
//! runtime lock on an arbitrarily large timeline. Undecodable rows are
//! counted and logged, never silently skipped.
//!
//! Purge shares the same filter machinery (what you can find is exactly what
//! you can delete), hard-deletes via `delete_batch`, compacts the purged key
//! range (tombstone reclamation per the ADR §6 / RocksDB guidance), and
//! writes a `kind = purge` audit row carrying counts and the filters — never
//! deleted content. Blanket purges skip `purge` audit rows so a purge can
//! never consume its own audit trail; deleting audit rows requires naming
//! `kinds: ["purge"]` explicitly.

use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU32, Ordering},
};

use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;
use synapse_core::types::{TimelineActor, TimelineKind, TimelineRecord};
use synapse_reflex::ReflexRuntime;
use synapse_storage::{cf, decode_json, timeline as timeline_codec};

use crate::m1::mcp_error;
use crate::server::url_redaction::redact_url_fields_for_public_readback;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

/// Default number of matches returned when `limit` is omitted.
pub const DEFAULT_LIMIT: u32 = 100;
/// Hard upper bound for `limit`.
pub const MAX_LIMIT: u32 = 500;
/// Maximum rows scanned per call before the search pauses with a cursor.
pub const MAX_SCAN_ROWS_PER_CALL: usize = 100_000;
/// Chunk size for bounded storage reads inside one call.
const SCAN_CHUNK_ROWS: usize = 4_096;
/// Maximum accepted `text` filter length in bytes.
const MAX_TEXT_FILTER_BYTES: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineSearchParams {
    /// Inclusive lower bound on the record `ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Inclusive upper bound on the record `ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    /// Case-insensitive exact matches on the record `app` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// Case-insensitive substring over app + payload string values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Snake-case record kinds (e.g. `focus_change`, `browser_nav`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// `human` or `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Maximum matches to return (default 100, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque continuation cursor from a previous response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineSearchResponse {
    pub matches: Vec<TimelineSearchMatch>,
    /// Rows examined this call (matching or not).
    pub scanned_rows: u64,
    /// Rows whose value failed to decode as a `TimelineRecord`; details are
    /// in daemon logs under code `TIMELINE_ROW_DECODE_FAILED`.
    pub invalid_rows: u64,
    /// Present when more rows may match; pass back as `cursor` to continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Why the call stopped: `limit_reached`, `scan_budget_exhausted`,
    /// `end_ts_reached`, or `end_of_timeline`.
    pub stopped_because: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineSearchMatch {
    /// Hex-encoded storage key (stable row identity).
    pub key_hex: String,
    pub ts_ns: u64,
    /// Key sequence component; absent for rows with non-codec keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    pub kind: String,
    /// `human` or `agent:<session_id>`.
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub payload: Value,
}

#[must_use]
pub const fn timeline_search() -> M3ToolStub {
    M3ToolStub::new("timeline_search")
}

#[must_use]
pub const fn timeline_purge() -> M3ToolStub {
    M3ToolStub::new("timeline_purge")
}

#[must_use]
pub fn required_permissions(_params: &TimelineSearchParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

#[derive(Debug)]
struct Filters {
    start_ts_ns: u64,
    end_ts_ns: u64,
    apps_lower: Vec<String>,
    text_lower: Option<String>,
    kinds: Vec<TimelineKind>,
    actor: Option<ActorFilter>,
    limit: usize,
    start_key: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorFilter {
    Human,
    Agent,
}

pub fn search_timeline(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &TimelineSearchParams,
) -> Result<TimelineSearchResponse, ErrorData> {
    let filters = validate(params)?;
    let runtime = lock_runtime(runtime)?;

    let mut matches = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut invalid_rows = 0_u64;
    let mut next_start = filters.start_key.clone();
    let mut last_key: Option<Vec<u8>> = None;
    let mut stopped_because = "end_of_timeline";
    let mut storage_has_more = false;

    'scan: loop {
        let remaining_budget = MAX_SCAN_ROWS_PER_CALL - usize::try_from(scanned_rows).unwrap_or(0);
        if remaining_budget == 0 {
            stopped_because = "scan_budget_exhausted";
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
        for (key, value) in rows {
            scanned_rows += 1;
            last_key = Some(key.clone());
            let codec_ts = timeline_codec::decode_timeline_key(&key).ok();
            // Codec keys iterate in ts order, so the first codec key past the
            // end bound proves no later codec row can match (ADR key scheme).
            if let Some((key_ts, _seq)) = codec_ts
                && key_ts > filters.end_ts_ns
            {
                stopped_because = "end_ts_reached";
                storage_has_more = false;
                break 'scan;
            }
            match decode_json::<TimelineRecord>(&value) {
                Ok(record) => {
                    if record_matches(&record, &filters) {
                        matches.push(to_match(&key, codec_ts.map(|(_ts, seq)| seq), record));
                        if matches.len() >= filters.limit {
                            stopped_because = "limit_reached";
                            break 'scan;
                        }
                    }
                }
                Err(error) => {
                    invalid_rows += 1;
                    tracing::warn!(
                        code = "TIMELINE_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(&key),
                        %error,
                        "timeline_search skipped undecodable CF_TIMELINE row"
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

    let resume_possible = matches!(stopped_because, "limit_reached" | "scan_budget_exhausted")
        && (storage_has_more || stopped_because == "limit_reached");
    let next_cursor = if resume_possible {
        last_key.as_deref().map(hex_encode)
    } else {
        None
    };
    Ok(TimelineSearchResponse {
        matches,
        scanned_rows,
        invalid_rows,
        next_cursor,
        stopped_because: stopped_because.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// timeline_get (#842): raw ordered slice for the dashboard day-view / agents
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineGetParams {
    /// Inclusive lower bound on the record `ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Inclusive upper bound on the record `ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    /// Snake-case record kinds (e.g. `focus_change`, `browser_nav`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// `human` or `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Maximum rows to return (default 100, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Opaque continuation cursor from a previous response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineGetResponse {
    /// Timeline rows in ascending `(ts_ns, seq)` storage order. Public
    /// readback redacts URL-bearing payload fields while keeping stable
    /// `key_hex` identity.
    pub rows: Vec<TimelineSearchMatch>,
    /// Rows examined this call (matching or not).
    pub scanned_rows: u64,
    /// Rows whose value failed to decode (counted + logged, never silently
    /// dropped); details under log code `TIMELINE_ROW_DECODE_FAILED`.
    pub invalid_rows: u64,
    /// Present when more rows remain; pass back as `cursor` to continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// `limit_reached`, `scan_budget_exhausted`, `end_ts_reached`, or
    /// `end_of_timeline`.
    pub stopped_because: String,
}

#[must_use]
pub fn required_permissions_get(_params: &TimelineGetParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

/// Raw ordered timeline retrieval (#842): a time-range + kind/actor read with no
/// text/app search semantics — the primitive the dashboard day-view and agents
/// render from. Delegates to the proven [`search_timeline`] scan (identical
/// paging, scan budget, and stable hex cursor) with the search-only `text`/`apps`
/// filters disabled, so there is exactly one CF_TIMELINE scan implementation to
/// trust and maintain. The cursor is the physical storage key, so paging is
/// stable under concurrent writes (a new row gets a later key and is never
/// skipped or double-counted across pages).
pub fn get_timeline(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &TimelineGetParams,
) -> Result<TimelineGetResponse, ErrorData> {
    let search_params = TimelineSearchParams {
        start_ts_ns: params.start_ts_ns,
        end_ts_ns: params.end_ts_ns,
        apps: None,
        text: None,
        kinds: params.kinds.clone(),
        actor: params.actor.clone(),
        limit: params.limit,
        cursor: params.cursor.clone(),
    };
    let response = search_timeline(runtime, &search_params)?;
    Ok(TimelineGetResponse {
        rows: response.matches,
        scanned_rows: response.scanned_rows,
        invalid_rows: response.invalid_rows,
        next_cursor: response.next_cursor,
        stopped_because: response.stopped_because,
    })
}

// ---------------------------------------------------------------------------
// timeline_stats (#842): recorder state + timeline data statistics
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineStatsParams {
    /// Optional inclusive lower bound for the by-kind / by-day aggregation.
    /// Omit for the whole timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Optional inclusive upper bound. Omit for the whole timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
}

/// Recorder gate + feed state — exactly what the write-path consults, read from
/// the same shared [`RecorderControl`](super::timeline_control::RecorderControl)
/// gate so a status read can never diverge from reality.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecorderStatus {
    /// Paused — zero new rows across all feeds until `timeline_resume`.
    pub paused: bool,
    /// Auto-resume deadline (epoch ns), when one is armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until_ns: Option<u64>,
    /// Whether the clipboard timeline feed is enabled (build/env gated).
    pub clipboard_feed_enabled: bool,
    /// Whether the file-activity timeline feed is enabled (build/env gated).
    pub file_activity_feed_enabled: bool,
    /// Immutable env-baseline executable exclusions (`SYNAPSE_TIMELINE_EXCLUDE`).
    pub env_exclusions: Vec<String>,
    /// Runtime exclusions mutable via `timeline_exclusions`.
    pub runtime_exclusions: Vec<String>,
}

impl RecorderStatus {
    /// Builds the status readback from the live recorder control gate (the exact
    /// gate the recorder write-path consults) plus the build/env feed-enable
    /// config. Keeping this in one place means `timeline_stats` and any future
    /// status surface report identical, never-divergent recorder state.
    #[must_use]
    pub fn from_control(control: &super::timeline_control::RecorderControl) -> Self {
        Self {
            paused: control.is_paused(),
            paused_until_ns: control.paused_until_ns(),
            clipboard_feed_enabled: crate::m1::timeline_clipboard_enabled(),
            file_activity_feed_enabled: crate::m1::timeline_file_activity_enabled(),
            env_exclusions: control.env_exclusions(),
            runtime_exclusions: control.runtime_exclusions(),
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineStatsResponse {
    /// Live recorder gate + feed state (truthful pause/feed/exclusion readback).
    pub recorder: RecorderStatus,
    /// Exact count of decoded rows in the aggregation window (== the sum of
    /// `rows_by_kind`). Authoritative only when `scan_complete` is true.
    pub total_rows: u64,
    /// CF_TIMELINE on-disk footprint in bytes, when storage exposes it. This is
    /// RocksDB's SST size estimate; freshly-written rows still in the memtable
    /// may not be reflected until a flush/compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_bytes: Option<u64>,
    /// Exact row counts by timeline kind over the scanned window.
    pub rows_by_kind: BTreeMap<String, u64>,
    /// Exact row counts by UTC calendar day (`YYYY-MM-DD`) over the window.
    pub rows_by_day_utc: BTreeMap<String, u64>,
    /// Oldest / newest row `ts_ns` observed in the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_ts_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_ts_ns: Option<u64>,
    /// Rows examined for the aggregation (matching or not).
    pub scanned_rows: u64,
    /// Rows whose value failed to decode (counted + logged, never silently
    /// dropped); details under log code `TIMELINE_ROW_DECODE_FAILED`.
    pub invalid_rows: u64,
    /// `false` when the scan budget paused before the whole window was read —
    /// the by-kind/by-day breakdown is then partial. Never a silent truncation.
    pub scan_complete: bool,
}

#[must_use]
pub fn required_permissions_stats(_params: &TimelineStatsParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

/// Computes timeline data statistics (#842): exact by-kind / by-day row counts,
/// oldest/newest ts, and on-disk footprint, over an optional time window. The
/// `recorder` state is supplied by the caller (read from the shared control gate
/// and feed config) so this function is a pure storage aggregation. The scan is
/// budget-guarded exactly like `timeline_search`; exhausting the budget sets
/// `scan_complete = false` rather than silently returning partial counts as if
/// whole.
pub fn timeline_stats_data(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    recorder: RecorderStatus,
    params: &TimelineStatsParams,
) -> Result<TimelineStatsResponse, ErrorData> {
    let start_ts_ns = params.start_ts_ns.unwrap_or(0);
    let end_ts_ns = params.end_ts_ns.unwrap_or(u64::MAX);
    if start_ts_ns > end_ts_ns {
        return Err(invalid(format!(
            "timeline_stats start_ts_ns ({start_ts_ns}) must be <= end_ts_ns ({end_ts_ns})"
        )));
    }

    let runtime = lock_runtime(runtime)?;
    let storage_bytes = runtime
        .storage_cf_live_data_size_estimates()
        .ok()
        .and_then(|(sizes, _missing)| sizes.get(cf::CF_TIMELINE).copied());

    let mut rows_by_kind: BTreeMap<String, u64> = BTreeMap::new();
    let mut rows_by_day_utc: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_rows: u64 = 0;
    let mut scanned_rows: u64 = 0;
    let mut invalid_rows: u64 = 0;
    let mut oldest_ts_ns: Option<u64> = None;
    let mut newest_ts_ns: Option<u64> = None;
    let mut scan_complete = true;
    let mut next_start = timeline_codec::timeline_scan_start(start_ts_ns);

    'scan: loop {
        let remaining_budget = MAX_SCAN_ROWS_PER_CALL - usize::try_from(scanned_rows).unwrap_or(0);
        if remaining_budget == 0 {
            scan_complete = false;
            break;
        }
        let chunk_rows = SCAN_CHUNK_ROWS.min(remaining_budget);
        let (rows, more) = runtime
            .storage_cf_rows_from(cf::CF_TIMELINE, &next_start, chunk_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let mut last_key: Option<Vec<u8>> = None;
        for (key, value) in rows {
            scanned_rows += 1;
            last_key = Some(key.clone());
            // Codec keys iterate in ts order, so the first key past the upper
            // bound proves no later row can fall in the window (ADR key scheme).
            if let Ok((key_ts, _seq)) = timeline_codec::decode_timeline_key(&key)
                && key_ts > end_ts_ns
            {
                break 'scan;
            }
            match decode_json::<TimelineRecord>(&value) {
                Ok(record) => {
                    if record.ts_ns < start_ts_ns || record.ts_ns > end_ts_ns {
                        continue;
                    }
                    total_rows += 1;
                    *rows_by_kind.entry(kind_name(record.kind)).or_insert(0) += 1;
                    *rows_by_day_utc.entry(utc_day(record.ts_ns)).or_insert(0) += 1;
                    oldest_ts_ns = Some(oldest_ts_ns.map_or(record.ts_ns, |o| o.min(record.ts_ns)));
                    newest_ts_ns = Some(newest_ts_ns.map_or(record.ts_ns, |n| n.max(record.ts_ns)));
                }
                Err(error) => {
                    invalid_rows += 1;
                    tracing::warn!(
                        code = "TIMELINE_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(&key),
                        %error,
                        "timeline_stats skipped undecodable CF_TIMELINE row"
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

    Ok(TimelineStatsResponse {
        recorder,
        total_rows,
        storage_bytes,
        rows_by_kind,
        rows_by_day_utc,
        oldest_ts_ns,
        newest_ts_ns,
        scanned_rows,
        invalid_rows,
        scan_complete,
    })
}

/// UTC calendar day (`YYYY-MM-DD`) for an epoch-nanosecond timestamp.
fn utc_day(ts_ns: u64) -> String {
    let nanos = i64::try_from(ts_ns).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp_nanos(nanos)
        .format("%Y-%m-%d")
        .to_string()
}

/// Monotonic per-process sequence for purge-audit keys, offset away from the
/// recorder's own sequence space so a same-nanosecond collision is
/// unrepresentable in practice.
static PURGE_AUDIT_SEQ: AtomicU32 = AtomicU32::new(0xFFFF_0000);

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelinePurgeParams {
    /// Inclusive lower bound on the record `ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ns: Option<u64>,
    /// Inclusive upper bound on the record `ts_ns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ts_ns: Option<u64>,
    /// Case-insensitive exact matches on the record `app` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// Case-insensitive substring over app + payload string values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Snake-case record kinds. `purge` audit rows are only deleted when
    /// this explicitly contains `"purge"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// `human` or `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Hard delete the exact `CF_TIMELINE` rows named by these hygiene flag ids
    /// (#875). Mutually exclusive with every scan filter and with `all`: flag
    /// ids resolve to exact physical keys, so no scan is performed. Every id
    /// must resolve to a `CF_TIMELINE` flag — a flag on another source CF is
    /// rejected (use `timeline_redact` to mask those). Deleting poisoned rows
    /// invalidates derived state (impacted routines/episodes/candidates are
    /// tainted), exactly like `timeline_redact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag_ids: Option<Vec<String>>,
    /// Explicit full-timeline purge. Mutually exclusive with every filter;
    /// without it, at least one filter is required.
    #[serde(default)]
    pub all: bool,
    /// Count matches without deleting anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Opaque continuation cursor from a previous purge response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelinePurgeResponse {
    /// Rows that matched the filters this call.
    pub matched_rows: u64,
    /// Rows physically deleted (0 on `dry_run`).
    pub deleted_rows: u64,
    /// Rows examined this call (matching or not).
    pub scanned_rows: u64,
    /// Undecodable rows: counted, logged, and never deleted (a row that
    /// cannot be decoded cannot be proven to match the filters).
    pub invalid_rows: u64,
    /// Matching `purge` audit rows protected because `kinds` did not
    /// explicitly include `"purge"`.
    pub protected_audit_rows: u64,
    pub dry_run: bool,
    /// Hex storage key of the audit row written for this purge; absent on
    /// `dry_run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_key_hex: Option<String>,
    /// Whether the purged key range was compacted (tombstone reclamation).
    pub compacted: bool,
    /// Present when the scan budget paused the purge; pass back as `cursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// `scan_budget_exhausted`, `end_ts_reached`, or `end_of_timeline`.
    pub stopped_because: String,
}

#[must_use]
pub fn required_permissions_purge(_params: &TimelinePurgeParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

pub fn purge_timeline(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &TimelinePurgeParams,
    by_session: &str,
) -> Result<TimelinePurgeResponse, ErrorData> {
    let has_filter = params.start_ts_ns.is_some()
        || params.end_ts_ns.is_some()
        || params.apps.is_some()
        || params.text.is_some()
        || params.kinds.is_some()
        || params.actor.is_some();
    if let Some(flag_ids) = params.flag_ids.as_deref() {
        if has_filter || params.all {
            return Err(invalid(
                "timeline_purge flag_ids is mutually exclusive with scan filters and all=true; \
                 flag ids resolve to exact rows",
            ));
        }
        return purge_timeline_by_flags(runtime, flag_ids, params.dry_run, by_session);
    }
    if params.all && has_filter {
        return Err(invalid(
            "timeline_purge all=true is mutually exclusive with filters; drop the filters or drop all",
        ));
    }
    if !params.all && !has_filter {
        return Err(invalid(
            "timeline_purge requires at least one filter (start_ts_ns/end_ts_ns/apps/text/kinds/actor) or an explicit all=true",
        ));
    }
    let search_equivalent = TimelineSearchParams {
        start_ts_ns: params.start_ts_ns,
        end_ts_ns: params.end_ts_ns,
        apps: params.apps.clone(),
        text: params.text.clone(),
        kinds: params.kinds.clone(),
        actor: params.actor.clone(),
        limit: None,
        cursor: params.cursor.clone(),
    };
    let mut filters = validate(&search_equivalent)?;
    // Purge has no match cap: everything matched inside the scan budget is
    // deleted; the budget plus cursor bound one call's work.
    filters.limit = usize::MAX;
    let purge_kind_explicit = filters.kinds.contains(&TimelineKind::Purge);

    let runtime_guard = lock_runtime(runtime)?;
    let mut keys_to_delete: Vec<Vec<u8>> = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut invalid_rows = 0_u64;
    let mut protected_audit_rows = 0_u64;
    let mut next_start = filters.start_key.clone();
    let mut last_key: Option<Vec<u8>> = None;
    let mut stopped_because = "end_of_timeline";
    let mut storage_has_more = false;

    'scan: loop {
        let remaining_budget = MAX_SCAN_ROWS_PER_CALL - usize::try_from(scanned_rows).unwrap_or(0);
        if remaining_budget == 0 {
            stopped_because = "scan_budget_exhausted";
            break;
        }
        let chunk_rows = SCAN_CHUNK_ROWS.min(remaining_budget);
        let (rows, more) = runtime_guard
            .storage_cf_rows_from(cf::CF_TIMELINE, &next_start, chunk_rows)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        storage_has_more = more;
        if rows.is_empty() {
            break;
        }
        for (key, value) in rows {
            scanned_rows += 1;
            last_key = Some(key.clone());
            let codec_ts = timeline_codec::decode_timeline_key(&key).ok();
            if let Some((key_ts, _seq)) = codec_ts
                && key_ts > filters.end_ts_ns
            {
                stopped_because = "end_ts_reached";
                storage_has_more = false;
                break 'scan;
            }
            match decode_json::<TimelineRecord>(&value) {
                Ok(record) => {
                    if record_matches(&record, &filters) {
                        if record.kind == TimelineKind::Purge && !purge_kind_explicit {
                            // A purge must never consume its own audit trail:
                            // audit rows are deleted only by naming the kind.
                            protected_audit_rows += 1;
                        } else {
                            keys_to_delete.push(key);
                        }
                    }
                }
                Err(error) => {
                    invalid_rows += 1;
                    tracing::warn!(
                        code = "TIMELINE_ROW_DECODE_FAILED",
                        key_hex = %hex_encode(&key),
                        %error,
                        "timeline_purge left undecodable CF_TIMELINE row in place"
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

    let matched_rows = u64::try_from(keys_to_delete.len()).unwrap_or(u64::MAX);
    let mut deleted_rows = 0_u64;
    let mut compacted = false;
    let mut audit_key_hex = None;
    if !params.dry_run {
        if let (Some(first), Some(last)) = (keys_to_delete.first(), keys_to_delete.last()) {
            let compact_start = first.clone();
            let compact_end = key_after(last);
            deleted_rows = matched_rows;
            runtime_guard
                .storage_delete_rows(cf::CF_TIMELINE, keys_to_delete)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "timeline_purge delete_batch failed; no audit row was written: {error}"
                        ),
                    )
                })?;
            runtime_guard
                .storage_compact_cf_range(cf::CF_TIMELINE, &compact_start, &compact_end)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "timeline_purge deleted {deleted_rows} rows but compacting the purged range failed: {error}"
                        ),
                    )
                })?;
            compacted = true;
        }
        let resume_cursor_pending = matches!(stopped_because, "scan_budget_exhausted");
        let audit_payload = json!({
            "op": "timeline_purge",
            "deleted_rows": deleted_rows,
            "matched_rows": matched_rows,
            "scanned_rows": scanned_rows,
            "invalid_rows": invalid_rows,
            "protected_audit_rows": protected_audit_rows,
            "by_session": by_session,
            "continued_from_cursor": params.cursor.is_some(),
            "more_pending": resume_cursor_pending,
            "filters": {
                "start_ts_ns": params.start_ts_ns,
                "end_ts_ns": params.end_ts_ns,
                "apps": params.apps,
                "text": params.text,
                "kinds": params.kinds,
                "actor": params.actor,
                "all": params.all,
            },
        });
        audit_key_hex = Some(write_cleaning_audit_row(&runtime_guard, audit_payload)?);
    }
    drop(runtime_guard);

    let next_cursor = if stopped_because == "scan_budget_exhausted" && storage_has_more {
        last_key.as_deref().map(hex_encode)
    } else {
        None
    };
    tracing::info!(
        code = "TIMELINE_PURGE_COMPLETED",
        deleted_rows,
        matched_rows,
        scanned_rows,
        invalid_rows,
        protected_audit_rows,
        dry_run = params.dry_run,
        by_session,
        stopped_because,
        "timeline purge completed"
    );
    Ok(TimelinePurgeResponse {
        matched_rows,
        deleted_rows,
        scanned_rows,
        invalid_rows,
        protected_audit_rows,
        dry_run: params.dry_run,
        audit_key_hex,
        compacted,
        next_cursor,
        stopped_because: stopped_because.to_owned(),
    })
}

/// Hard deletes the exact `CF_TIMELINE` rows named by hygiene flag ids (#875),
/// then invalidates derived state and writes one audit row. Reuses the same
/// delete → compact → audit mechanics as the scan-based purge; the only
/// difference is the rows are selected by resolving flag ids to physical keys
/// rather than by a filter scan.
fn purge_timeline_by_flags(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    flag_ids: &[String],
    dry_run: bool,
    by_session: &str,
) -> Result<TimelinePurgeResponse, ErrorData> {
    let guard = lock_runtime(runtime)?;
    let flags = crate::m3::hygiene::resolve_clean_flags(
        &guard,
        &crate::m3::hygiene::CleanFlagSelector::Ids(flag_ids.to_vec()),
    )?;
    // Purge only deletes timeline rows; a flag on another source CF must be
    // masked with timeline_redact, never silently ignored.
    for flag in &flags {
        if flag.record.source_cf != cf::CF_TIMELINE {
            return Err(invalid(format!(
                "timeline_purge flag {} targets {} (not CF_TIMELINE); use timeline_redact to mask \
                 non-timeline sources",
                flag.record.flag_id, flag.record.source_cf
            )));
        }
    }

    // Resolve to distinct physical keys, confirming each row's presence.
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut seen: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    let mut absent_rows = 0_u64;
    for flag in &flags {
        let key = hex_decode(&flag.record.source_key_hex).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "TIMELINE_PURGE_FLAG_KEY_INVALID: flag {} has non-hex source_key_hex {}",
                    flag.record.flag_id, flag.record.source_key_hex
                ),
            )
        })?;
        // Confirm the key is a real CF_TIMELINE codec key before deleting it.
        timeline_codec::decode_timeline_key(&key).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "TIMELINE_PURGE_TIMELINE_KEY_INVALID: flag {} key is not a CF_TIMELINE codec \
                     key: {error}",
                    flag.record.flag_id
                ),
            )
        })?;
        if !seen.insert(key.clone()) {
            continue;
        }
        let rows = guard
            .storage_cf_prefix_rows(cf::CF_TIMELINE, &key, 1)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.iter().any(|(row_key, _value)| row_key == &key) {
            keys.push(key);
        } else {
            absent_rows += 1;
        }
    }

    let matched_rows = keys.len() as u64;
    let scanned_rows = flags.len() as u64;
    let mut deleted_rows = 0_u64;
    let mut compacted = false;
    let mut audit_key_hex = None;
    if !dry_run {
        if let (Some(first), Some(last)) = (keys.first().cloned(), keys.last().cloned()) {
            // Keys are collected in flag (resolution) order; sort so the compact
            // range brackets the full deleted span.
            let mut sorted = keys.clone();
            sorted.sort();
            let compact_start = sorted.first().cloned().unwrap_or(first);
            let compact_end = key_after(sorted.last().unwrap_or(&last));
            deleted_rows = matched_rows;
            guard
                .storage_delete_rows(cf::CF_TIMELINE, keys)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("timeline_purge flag delete_batch failed; no audit row was written: {error}"),
                    )
                })?;
            guard
                .storage_compact_cf_range(cf::CF_TIMELINE, &compact_start, &compact_end)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "timeline_purge deleted {deleted_rows} rows by flag but compacting failed: {error}"
                        ),
                    )
                })?;
            compacted = true;
        }
        let audit_payload = json!({
            "op": crate::m3::hygiene::CLEAN_OP_PURGE,
            "deleted_rows": deleted_rows,
            "matched_rows": matched_rows,
            "scanned_rows": scanned_rows,
            "absent_rows": absent_rows,
            "by_session": by_session,
            "flag_ids": flags
                .iter()
                .map(|flag| flag.record.flag_id.clone())
                .collect::<Vec<_>>(),
        });
        audit_key_hex = Some(write_cleaning_audit_row(&guard, audit_payload)?);
        // Invalidate derived state from the purged rows.
        crate::m3::hygiene::invalidate_cleaned_flags(
            &guard,
            &flags,
            crate::m3::hygiene::CLEAN_OP_PURGE,
            audit_key_hex.as_deref(),
            by_session,
        )?;
    }
    drop(guard);

    tracing::info!(
        code = "TIMELINE_PURGE_BY_FLAGS_COMPLETED",
        deleted_rows,
        matched_rows,
        scanned_rows,
        absent_rows,
        dry_run,
        by_session,
        "timeline purge by flag ids completed"
    );

    Ok(TimelinePurgeResponse {
        matched_rows,
        deleted_rows,
        scanned_rows,
        invalid_rows: absent_rows,
        protected_audit_rows: 0,
        dry_run,
        audit_key_hex,
        compacted,
        next_cursor: None,
        stopped_because: "flag_ids".to_owned(),
    })
}

/// Writes a cleaning audit row (purge or redact) with the pressure bypass (an
/// audit obligation must not shed), flushes it, and proves it by reading the
/// exact key back. The payload's `op` field distinguishes the cleaning tool;
/// the row is a [`TimelineKind::Purge`] record so the existing self-purge
/// protection covers every cleaning audit, not just `timeline_purge`.
pub(crate) fn write_cleaning_audit_row(
    runtime: &ReflexRuntime,
    payload: Value,
) -> Result<String, ErrorData> {
    let ts_ns = now_ts_ns();
    let seq = PURGE_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
    let key = timeline_codec::timeline_key(ts_ns, seq);
    let mut record = TimelineRecord::new(ts_ns, TimelineKind::Purge, TimelineActor::Human);
    record.payload = payload;
    let value = serde_json::to_vec(&record).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("encode timeline purge audit row: {error}"),
        )
    })?;
    runtime
        .storage_put_rows_pressure_bypass(cf::CF_TIMELINE, vec![(key.clone(), value)])
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("rows were purged but writing the purge audit row failed: {error}"),
            )
        })?;
    runtime.storage_flush().map_err(|error| {
        mcp_error(
            error.code(),
            format!("rows were purged but flushing the purge audit row failed: {error}"),
        )
    })?;
    // Internal consistency readback: the audit row must be physically present,
    // not just acked.
    let (rows, _more) = runtime
        .storage_cf_rows_from(cf::CF_TIMELINE, &key, 1)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    if rows.first().map(|(row_key, _value)| row_key.as_slice()) != Some(key.as_slice()) {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "rows were purged but the purge audit row is absent on readback",
        ));
    }
    Ok(hex_encode(&key))
}

fn now_ts_ns() -> u64 {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

fn validate(params: &TimelineSearchParams) -> Result<Filters, ErrorData> {
    let start_ts_ns = params.start_ts_ns.unwrap_or(0);
    let end_ts_ns = params.end_ts_ns.unwrap_or(u64::MAX);
    if start_ts_ns > end_ts_ns {
        return Err(invalid(format!(
            "timeline_search start_ts_ns {start_ts_ns} must be <= end_ts_ns {end_ts_ns}"
        )));
    }
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(invalid(format!(
            "timeline_search limit must be between 1 and {MAX_LIMIT}; got {limit}"
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
                Err(invalid("timeline_search apps entries must not be empty"))
            } else {
                Ok(trimmed.to_lowercase())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let text_lower = params
        .text
        .as_deref()
        .map(|text| {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(invalid("timeline_search text must not be empty"));
            }
            if trimmed.len() > MAX_TEXT_FILTER_BYTES {
                return Err(invalid(format!(
                    "timeline_search text must be <= {MAX_TEXT_FILTER_BYTES} bytes"
                )));
            }
            Ok(trimmed.to_lowercase())
        })
        .transpose()?;
    let kinds = params
        .kinds
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|kind| parse_kind(kind))
        .collect::<Result<Vec<_>, _>>()?;
    let actor = params
        .actor
        .as_deref()
        .map(|actor| match actor.trim().to_lowercase().as_str() {
            "human" => Ok(ActorFilter::Human),
            "agent" => Ok(ActorFilter::Agent),
            other => Err(invalid(format!(
                "timeline_search actor must be \"human\" or \"agent\"; got {other:?}"
            ))),
        })
        .transpose()?;
    let start_key = match params.cursor.as_deref() {
        Some(cursor) => {
            let decoded = hex_decode(cursor).ok_or_else(|| {
                invalid("timeline_search cursor is not a valid hex key from a prior response")
            })?;
            key_after(&decoded)
        }
        None => timeline_codec::timeline_scan_start(start_ts_ns),
    };
    Ok(Filters {
        start_ts_ns,
        end_ts_ns,
        apps_lower,
        text_lower,
        kinds,
        actor,
        limit: limit as usize,
        start_key,
    })
}

fn parse_kind(raw: &str) -> Result<TimelineKind, ErrorData> {
    serde_json::from_value::<TimelineKind>(Value::String(raw.trim().to_owned())).map_err(|_error| {
        invalid(format!(
            "timeline_search kinds entry {raw:?} is not a known timeline kind"
        ))
    })
}

fn record_matches(record: &TimelineRecord, filters: &Filters) -> bool {
    if record.ts_ns < filters.start_ts_ns || record.ts_ns > filters.end_ts_ns {
        return false;
    }
    if !filters.kinds.is_empty() && !filters.kinds.contains(&record.kind) {
        return false;
    }
    if let Some(actor) = filters.actor {
        let is_human = matches!(record.actor, TimelineActor::Human);
        if (actor == ActorFilter::Human) != is_human {
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
    if let Some(needle) = filters.text_lower.as_deref() {
        let in_app = record
            .app
            .as_deref()
            .is_some_and(|app| app.to_lowercase().contains(needle));
        if !in_app && !value_contains(&record.payload, needle) {
            return false;
        }
    }
    true
}

/// Case-insensitive substring search over every string value in a JSON tree.
fn value_contains(value: &Value, needle_lower: &str) -> bool {
    match value {
        Value::String(text) => text.to_lowercase().contains(needle_lower),
        Value::Array(items) => items.iter().any(|item| value_contains(item, needle_lower)),
        Value::Object(map) => map
            .values()
            .any(|entry| value_contains(entry, needle_lower)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn to_match(key: &[u8], seq: Option<u32>, record: TimelineRecord) -> TimelineSearchMatch {
    let mut payload = record.payload;
    redact_url_fields_for_public_readback(&mut payload);
    TimelineSearchMatch {
        key_hex: hex_encode(key),
        ts_ns: record.ts_ns,
        seq,
        kind: kind_name(record.kind),
        actor: match &record.actor {
            TimelineActor::Human => "human".to_owned(),
            TimelineActor::Agent { session_id } => format!("agent:{session_id}"),
        },
        app: record.app,
        payload,
    }
}

fn kind_name(kind: TimelineKind) -> String {
    serde_json::to_value(kind).map_or_else(
        |_error| format!("{kind:?}"),
        |value| value.as_str().unwrap_or_default().to_owned(),
    )
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
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

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
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
