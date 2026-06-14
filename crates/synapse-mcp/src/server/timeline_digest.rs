//! `timeline_digest` MCP tool (#850, epic #830).
//!
//! A read-only daily/weekly activity summary derived **entirely** from the
//! authoritative episode store (`CF_EPISODES`, #846/#847) plus the mined
//! routine store (`CF_ROUTINES`, #848). It answers "where did my time go" for
//! the dashboard and a future notify path: time by app, top documents/sites,
//! per-day active/idle split, and the mined routines whose own recorded
//! evidence episodes fall inside the period.
//!
//! ## Why it reconciles exactly with the episode store
//!
//! Every number is a pure aggregation of the same `EpisodeView` rows that
//! `episode_list` returns — read through the identical
//! [`crate::m3::episodes::list_episodes`] scan, never a parallel cache. The
//! digest therefore reconciles with `CF_EPISODES` by construction (the #850
//! FSV requirement):
//!
//! - `active_ms` == Σ episode `duration_ms`
//! - `active_ms` == Σ(`by_app[*].active_ms`) + `by_app_other.active_ms`
//!   (and identically for `top_documents`)
//! - `active_ms` == Σ(`per_day[*].active_ms`); `idle_ms` == Σ(`per_day[*].idle_ms`)
//! - `episode_count` == Σ(`per_day[*].episode_count`) == Σ(group counts + residual)
//!
//! A document/app field that is absent on an episode is bucketed under the
//! literal key `"(unknown)"` (parentheses cannot collide with a real
//! lowercase exe name or host) so no active time is silently dropped from the
//! reconciliation — the same "no unallocated total" discipline `agent_cost`
//! uses.
//!
//! ## Day attribution
//!
//! Episodes never span local midnight (#846 `DayBoundary` invariant), so each
//! episode belongs to exactly one local day, attributed by `start_ts_ns`.
//! `period=day` covers the one local day containing the anchor; `period=week`
//! covers the seven local days ending on (and including) the anchor day.
//!
//! ## Failure policy
//!
//! Loud and structured: a corrupt `CF_ROUTINES` row, a scan-budget overrun, an
//! unresolvable local-day boundary, or an oversized period error out with a
//! diagnostic code — never a partial or guessed digest.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use chrono::{Local, NaiveDate, TimeZone};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_core::types::RoutineRecord;
use synapse_reflex::ReflexRuntime;
use synapse_storage::{cf, decode_json};

use super::{ErrorData, Json, Parameters, SynapseService, mcp_error, tool, tool_router};
use crate::m3::episodes::{
    EpisodeListParams, EpisodeView, MAX_LIST_LIMIT, hex_encode, key_after, list_episodes,
    local_day_start, next_local_day_start,
};
use crate::m3::permissions::{Permission, RequiredPermissions, required};

/// Default number of apps/documents broken out individually.
pub const DEFAULT_TOP_N: u32 = 10;
/// Hard upper bound for `top_n`.
pub const MAX_TOP_N: u32 = 100;
/// Refuse a period that would aggregate more episodes than this (a runaway
/// guard; a real human day is dozens–hundreds of episodes).
pub const MAX_DIGEST_EPISODES: u64 = 250_000;
/// Refuse a routine scan longer than this many `CF_ROUTINES` rows.
pub const MAX_ROUTINE_SCAN_ROWS: u64 = 100_000;
/// Bounded read window for the `CF_ROUTINES` scan.
const ROUTINE_CHUNK_ROWS: usize = 4_096;
/// Bucket key for episodes whose app/document field is absent.
const UNKNOWN_KEY: &str = "(unknown)";

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
}

fn internal(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail.into())
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineDigestParams {
    /// `"day"` or `"week"`.
    pub period: String,
    /// Local calendar date `"YYYY-MM-DD"` to summarize. Mutually exclusive
    /// with `anchor_ts_ns`. If neither is given, the current local day is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Any instant (ns since the Unix epoch) inside the target local day.
    /// Mutually exclusive with `date`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_ts_ns: Option<u64>,
    /// Include agent-actor episodes too (default false: human activity only).
    /// Agent episodes exist in `CF_EPISODES` only when `episode_segment` was
    /// run with `include_agent_activity=true`.
    #[serde(default)]
    pub include_agent_activity: bool,
    /// Maximum apps/documents broken out individually (default 10, max 100);
    /// the rest roll into the `*_other` residual so totals still reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u32>,
}

/// One app or document usage row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupUsage {
    /// App exe name or document host/title; `"(unknown)"` when the episode
    /// carried no value for the field.
    pub key: String,
    /// Representative (first seen) full URL for document rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub active_ms: u64,
    pub episode_count: u64,
    pub keystroke_count: u64,
    pub click_count: u64,
    /// Share of `active_ms` in parts-per-thousand (0..=1000); integer so the
    /// row stays exactly comparable and carries no float rounding into FSV.
    pub active_share_permille: u32,
}

/// The long tail collapsed into one residual so Σ still equals the total.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupResidual {
    /// Number of distinct apps/documents folded into this residual.
    pub group_count: u64,
    pub active_ms: u64,
    pub episode_count: u64,
    pub keystroke_count: u64,
    pub click_count: u64,
}

/// Per-local-day breakdown. Days inside the period with no activity appear
/// with zero counters so a week view has a row per day.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DayDigest {
    pub day_start_ns: u64,
    pub day_end_ns: u64,
    pub episode_count: u64,
    pub active_ms: u64,
    /// Wall-clock between the day's first episode start and last episode end
    /// minus active time — the gaps the operator was away inside the active
    /// envelope. Zero when the day has zero or one episode.
    pub idle_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ns: Option<u64>,
    pub keystroke_count: u64,
    pub click_count: u64,
}

/// A mined routine whose recorded evidence episodes fall inside the period.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineTouched {
    pub routine_id: String,
    pub schedule_label: String,
    /// Mined confidence in parts-per-thousand (0..=1000).
    pub confidence_permille: u32,
    /// How many of this routine's evidence episode ids appear in this period's
    /// episodes — the deep-link reconciliation anchor.
    pub matched_episode_count: u64,
    /// The apps of the routine's steps, in step order.
    pub step_apps: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineDigestResponse {
    pub period: String,
    pub period_start_ns: u64,
    pub period_end_ns: u64,
    pub days_covered: u32,
    /// `"human"` or `"human+agent"`.
    pub actor_filter: String,
    pub episode_count: u64,
    pub active_ms: u64,
    pub idle_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ns: Option<u64>,
    pub total_keystrokes: u64,
    pub total_clicks: u64,
    pub total_interruptions: u64,
    pub total_interrupted_ms: u64,
    pub by_app: Vec<GroupUsage>,
    pub by_app_other: GroupResidual,
    pub top_documents: Vec<GroupUsage>,
    pub top_documents_other: GroupResidual,
    pub per_day: Vec<DayDigest>,
    pub routines_touched: Vec<RoutineTouched>,
    /// `CF_EPISODES` rows scanned to build this digest (across pagination).
    pub episodes_scanned_rows: u64,
    /// `CF_ROUTINES` rows scanned for routine attribution.
    pub routines_scanned_rows: u64,
}

#[must_use]
pub fn required_permissions(_params: &TimelineDigestParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

/// The resolved local-day window a digest covers.
#[derive(Clone, Debug)]
struct PeriodWindow {
    period: String,
    period_start_ns: u64,
    period_end_ns: u64,
    /// Sorted local-midnight day starts, contiguous and covering the period.
    day_starts: Vec<u64>,
}

fn parse_local_date_midnight(date: &str) -> Result<u64, ErrorData> {
    let naive = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map_err(|error| invalid(format!("date must be YYYY-MM-DD: {error}")))?;
    let midnight = naive
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| internal("midnight unrepresentable for the parsed date"))?;
    let resolved = Local
        .from_local_datetime(&midnight)
        .earliest()
        .or_else(|| Local.from_local_datetime(&midnight).latest())
        .ok_or_else(|| invalid(format!("no valid local instant for midnight of {date}")))?;
    let nanos = resolved
        .timestamp_nanos_opt()
        .ok_or_else(|| invalid(format!("date {date} is outside the representable range")))?;
    u64::try_from(nanos).map_err(|_e| invalid(format!("date {date} predates the Unix epoch")))
}

fn resolve_window(params: &TimelineDigestParams) -> Result<PeriodWindow, ErrorData> {
    let period = params.period.trim().to_lowercase();
    let days_back: u32 = match period.as_str() {
        "day" => 0,
        "week" => 6,
        other => {
            return Err(invalid(format!(
                "period must be \"day\" or \"week\"; got {other:?}"
            )));
        }
    };
    if params.date.is_some() && params.anchor_ts_ns.is_some() {
        return Err(invalid(
            "pass at most one of date or anchor_ts_ns, not both",
        ));
    }
    let anchor_ns = match (&params.date, params.anchor_ts_ns) {
        (Some(date), _) => parse_local_date_midnight(date)?,
        (None, Some(anchor)) => anchor,
        (None, None) => now_ts_ns(),
    };
    let anchor_day_start = local_day_start(anchor_ns)?;
    let period_end_ns = next_local_day_start(anchor_day_start)?;

    // Walk back DST-safely: previous local midnight is the local day of the
    // instant one nanosecond before this day's midnight.
    let mut day_starts = vec![anchor_day_start];
    let mut cursor = anchor_day_start;
    for _ in 0..days_back {
        let prev = local_day_start(cursor.checked_sub(1).ok_or_else(|| {
            internal("period extends before the Unix epoch")
        })?)?;
        day_starts.push(prev);
        cursor = prev;
    }
    day_starts.sort_unstable();
    day_starts.dedup();
    let period_start_ns = *day_starts
        .first()
        .ok_or_else(|| internal("resolved an empty period window"))?;
    Ok(PeriodWindow {
        period,
        period_start_ns,
        period_end_ns,
        day_starts,
    })
}

fn now_ts_ns() -> u64 {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

/// Pages [`list_episodes`] across the whole period and returns the episodes
/// whose `start_ts_ns` is contained in `[period_start, period_end)`, plus the
/// total `CF_EPISODES` rows scanned. Start-containment (not just overlap)
/// gives exact, non-double-counting day attribution at midnight edges.
fn collect_period_episodes(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    window: &PeriodWindow,
    include_agent_activity: bool,
) -> Result<(Vec<EpisodeView>, u64), ErrorData> {
    let actor = if include_agent_activity {
        None
    } else {
        Some("human".to_owned())
    };
    let mut episodes: Vec<EpisodeView> = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut cursor: Option<String> = None;
    loop {
        let params = EpisodeListParams {
            // Inclusive lower bound; the upper bound is the last instant of the
            // period so an episode starting at the next day's midnight (the
            // next period) is never read.
            start_ts_ns: Some(window.period_start_ns),
            end_ts_ns: Some(window.period_end_ns.saturating_sub(1)),
            apps: None,
            actor: actor.clone(),
            min_duration_ms: None,
            limit: Some(MAX_LIST_LIMIT),
            cursor: cursor.clone(),
        };
        let page = list_episodes(runtime, &params)?;
        scanned_rows = scanned_rows.saturating_add(page.scanned_rows);
        for view in page.episodes {
            if view.start_ts_ns >= window.period_start_ns
                && view.start_ts_ns < window.period_end_ns
            {
                episodes.push(view);
                if u64::try_from(episodes.len()).unwrap_or(u64::MAX) > MAX_DIGEST_EPISODES {
                    return Err(internal(format!(
                        "DIGEST_TOO_MANY_EPISODES: period holds more than {MAX_DIGEST_EPISODES} \
                         episodes; narrow the period"
                    )));
                }
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok((episodes, scanned_rows))
}

/// Reads `CF_ROUTINES` and returns the routines whose evidence episode ids
/// intersect `period_episode_ids`, plus the rows scanned.
fn collect_routines_touched(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    period_episode_ids: &BTreeSet<String>,
) -> Result<(Vec<RoutineTouched>, u64), ErrorData> {
    let guard = runtime
        .lock()
        .map_err(|_e| internal("reflex runtime lock poisoned"))?;
    let mut start: Vec<u8> = Vec::new();
    let mut scanned_rows = 0_u64;
    let mut touched: Vec<RoutineTouched> = Vec::new();
    loop {
        if scanned_rows >= MAX_ROUTINE_SCAN_ROWS {
            return Err(internal(format!(
                "DIGEST_ROUTINE_SCAN_BUDGET_EXHAUSTED after {MAX_ROUTINE_SCAN_ROWS} CF_ROUTINES rows"
            )));
        }
        let (rows, more) = guard
            .storage_cf_rows_from(cf::CF_ROUTINES, &start, ROUTINE_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            scanned_rows += 1;
            // CF_ROUTINES is derived state we own and holds only mined
            // RoutineRecord rows (operator lifecycle lives in the separate
            // CF_ROUTINE_STATE), so an undecodable value is corruption to
            // surface loudly, never a row to skip.
            let routine: RoutineRecord = decode_json(value).map_err(|error| {
                tracing::error!(
                    code = "ROUTINE_ROW_DECODE_FAILED",
                    key_hex = %hex_encode(key),
                    %error,
                    "CF_ROUTINES holds a value that does not decode as a RoutineRecord"
                );
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "ROUTINE_ROW_DECODE_FAILED in CF_ROUTINES at {}: {error}; CF_ROUTINES is \
                         derived state — re-run routine_mine after removing the row",
                        hex_encode(key)
                    ),
                )
            })?;
            let matched = routine
                .evidence
                .iter()
                .flat_map(|evidence| evidence.episode_ids.iter())
                .filter(|episode_id| period_episode_ids.contains(*episode_id))
                .collect::<BTreeSet<_>>()
                .len();
            if matched > 0 {
                touched.push(RoutineTouched {
                    routine_id: routine.routine_id,
                    schedule_label: routine.schedule_label,
                    confidence_permille: permille(routine.confidence),
                    matched_episode_count: u64::try_from(matched).unwrap_or(u64::MAX),
                    step_apps: routine.steps.into_iter().map(|step| step.app).collect(),
                });
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
    drop(guard);
    // Strongest signal first, then stable by id.
    touched.sort_by(|a, b| {
        b.matched_episode_count
            .cmp(&a.matched_episode_count)
            .then_with(|| a.routine_id.cmp(&b.routine_id))
    });
    Ok((touched, scanned_rows))
}

/// `confidence` (a 0.0..=1.0 fraction) as integer parts-per-thousand.
fn permille(fraction: f64) -> u32 {
    if !fraction.is_finite() || fraction <= 0.0 {
        return 0;
    }
    let scaled = (fraction * 1000.0).round();
    if scaled >= 1000.0 {
        1000
    } else {
        // 0.0 < scaled < 1000.0 ⇒ fits u32 without precision loss.
        scaled as u32
    }
}

/// `numerator/total` as integer parts-per-thousand, rounded half-up.
fn share_permille(numerator: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    let scaled = (u128::from(numerator) * 1000 + u128::from(total) / 2) / u128::from(total);
    u32::try_from(scaled.min(1000)).unwrap_or(1000)
}

#[derive(Default)]
struct GroupAccum {
    url: Option<String>,
    active_ms: u64,
    episode_count: u64,
    keystroke_count: u64,
    click_count: u64,
}

impl GroupAccum {
    fn add(&mut self, view: &EpisodeView) {
        self.active_ms = self.active_ms.saturating_add(view.duration_ms);
        self.episode_count = self.episode_count.saturating_add(1);
        self.keystroke_count = self.keystroke_count.saturating_add(view.keystroke_count);
        self.click_count = self.click_count.saturating_add(view.click_count);
        if self.url.is_none() {
            if let Some(url) = view.url.as_ref() {
                self.url = Some(url.clone());
            }
        }
    }
}

/// Splits a grouped map into the top-`n` rows (by active time, ties broken by
/// key) and a residual that preserves the reconciliation totals.
fn split_groups(
    groups: BTreeMap<String, GroupAccum>,
    total_active_ms: u64,
    top_n: usize,
    with_url: bool,
) -> (Vec<GroupUsage>, GroupResidual) {
    let mut ordered: Vec<(String, GroupAccum)> = groups.into_iter().collect();
    ordered.sort_by(|a, b| {
        b.1.active_ms
            .cmp(&a.1.active_ms)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut top = Vec::new();
    let mut residual = GroupResidual::default();
    for (index, (key, accum)) in ordered.into_iter().enumerate() {
        if index < top_n {
            top.push(GroupUsage {
                key,
                url: if with_url { accum.url } else { None },
                active_ms: accum.active_ms,
                episode_count: accum.episode_count,
                keystroke_count: accum.keystroke_count,
                click_count: accum.click_count,
                active_share_permille: share_permille(accum.active_ms, total_active_ms),
            });
        } else {
            residual.group_count = residual.group_count.saturating_add(1);
            residual.active_ms = residual.active_ms.saturating_add(accum.active_ms);
            residual.episode_count = residual.episode_count.saturating_add(accum.episode_count);
            residual.keystroke_count =
                residual.keystroke_count.saturating_add(accum.keystroke_count);
            residual.click_count = residual.click_count.saturating_add(accum.click_count);
        }
    }
    (top, residual)
}

/// Per-day running accumulator (envelope tracked to derive idle).
#[derive(Default)]
struct DayAccum {
    episode_count: u64,
    active_ms: u64,
    keystroke_count: u64,
    click_count: u64,
    first_activity_ns: Option<u64>,
    last_activity_ns: Option<u64>,
}

/// Pure aggregation over the period's start-contained episodes. `episodes` is
/// the exact set counted; `day_ends` maps each `day_start` to its end bound.
fn aggregate_digest(
    window: &PeriodWindow,
    episodes: &[EpisodeView],
    include_agent_activity: bool,
    top_n: usize,
    routines_touched: Vec<RoutineTouched>,
) -> TimelineDigestResponse {
    let mut day_accums: BTreeMap<u64, DayAccum> = window
        .day_starts
        .iter()
        .map(|&day_start| (day_start, DayAccum::default()))
        .collect();
    let mut by_app: BTreeMap<String, GroupAccum> = BTreeMap::new();
    let mut by_doc: BTreeMap<String, GroupAccum> = BTreeMap::new();

    let mut episode_count = 0_u64;
    let mut active_ms = 0_u64;
    let mut total_keystrokes = 0_u64;
    let mut total_clicks = 0_u64;
    let mut total_interruptions = 0_u64;
    let mut total_interrupted_ms = 0_u64;
    let mut first_activity_ns: Option<u64> = None;
    let mut last_activity_ns: Option<u64> = None;

    for view in episodes {
        episode_count += 1;
        active_ms = active_ms.saturating_add(view.duration_ms);
        total_keystrokes = total_keystrokes.saturating_add(view.keystroke_count);
        total_clicks = total_clicks.saturating_add(view.click_count);
        total_interruptions =
            total_interruptions.saturating_add(u64::from(view.interruption_count));
        total_interrupted_ms = total_interrupted_ms.saturating_add(view.interrupted_ms);
        first_activity_ns = Some(min_opt(first_activity_ns, view.start_ts_ns));
        last_activity_ns = Some(max_opt(last_activity_ns, view.end_ts_ns));

        let app_key = view
            .app
            .clone()
            .unwrap_or_else(|| UNKNOWN_KEY.to_owned());
        by_app.entry(app_key).or_default().add(view);
        let doc_key = view
            .document
            .clone()
            .unwrap_or_else(|| UNKNOWN_KEY.to_owned());
        by_doc.entry(doc_key).or_default().add(view);

        // Day bucket: the largest day_start <= the episode start. Episodes are
        // start-contained in the period and never span midnight, so exactly
        // one day matches.
        if let Some((_, accum)) = day_accums
            .range_mut(..=view.start_ts_ns)
            .next_back()
        {
            accum.episode_count += 1;
            accum.active_ms = accum.active_ms.saturating_add(view.duration_ms);
            accum.keystroke_count = accum.keystroke_count.saturating_add(view.keystroke_count);
            accum.click_count = accum.click_count.saturating_add(view.click_count);
            accum.first_activity_ns = Some(min_opt(accum.first_activity_ns, view.start_ts_ns));
            accum.last_activity_ns = Some(max_opt(accum.last_activity_ns, view.end_ts_ns));
        }
    }

    let mut per_day = Vec::with_capacity(window.day_starts.len());
    let mut idle_ms = 0_u64;
    for (index, &day_start) in window.day_starts.iter().enumerate() {
        let day_end = window
            .day_starts
            .get(index + 1)
            .copied()
            .unwrap_or(window.period_end_ns);
        let accum = day_accums.remove(&day_start).unwrap_or_default();
        let envelope_ms = match (accum.first_activity_ns, accum.last_activity_ns) {
            (Some(first), Some(last)) => last.saturating_sub(first) / 1_000_000,
            _ => 0,
        };
        let day_idle = envelope_ms.saturating_sub(accum.active_ms);
        idle_ms = idle_ms.saturating_add(day_idle);
        per_day.push(DayDigest {
            day_start_ns: day_start,
            day_end_ns: day_end,
            episode_count: accum.episode_count,
            active_ms: accum.active_ms,
            idle_ms: day_idle,
            first_activity_ns: accum.first_activity_ns,
            last_activity_ns: accum.last_activity_ns,
            keystroke_count: accum.keystroke_count,
            click_count: accum.click_count,
        });
    }

    let (by_app_rows, by_app_other) = split_groups(by_app, active_ms, top_n, false);
    let (top_documents, top_documents_other) = split_groups(by_doc, active_ms, top_n, true);

    TimelineDigestResponse {
        period: window.period.clone(),
        period_start_ns: window.period_start_ns,
        period_end_ns: window.period_end_ns,
        days_covered: u32::try_from(window.day_starts.len()).unwrap_or(u32::MAX),
        actor_filter: if include_agent_activity {
            "human+agent".to_owned()
        } else {
            "human".to_owned()
        },
        episode_count,
        active_ms,
        idle_ms,
        first_activity_ns,
        last_activity_ns,
        total_keystrokes,
        total_clicks,
        total_interruptions,
        total_interrupted_ms,
        by_app: by_app_rows,
        by_app_other,
        top_documents,
        top_documents_other,
        per_day,
        routines_touched,
        episodes_scanned_rows: 0,
        routines_scanned_rows: 0,
    }
}

fn min_opt(current: Option<u64>, candidate: u64) -> u64 {
    current.map_or(candidate, |value| value.min(candidate))
}

fn max_opt(current: Option<u64>, candidate: u64) -> u64 {
    current.map_or(candidate, |value| value.max(candidate))
}

pub fn build_digest(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: &TimelineDigestParams,
) -> Result<TimelineDigestResponse, ErrorData> {
    let top_n = params.top_n.unwrap_or(DEFAULT_TOP_N);
    if top_n == 0 || top_n > MAX_TOP_N {
        return Err(invalid(format!(
            "top_n must be between 1 and {MAX_TOP_N}; got {top_n}"
        )));
    }
    let window = resolve_window(params)?;
    let (episodes, episodes_scanned_rows) =
        collect_period_episodes(runtime, &window, params.include_agent_activity)?;
    let period_episode_ids: BTreeSet<String> = episodes
        .iter()
        .map(|view| view.episode_id.clone())
        .collect();
    let (routines_touched, routines_scanned_rows) =
        collect_routines_touched(runtime, &period_episode_ids)?;

    let mut response = aggregate_digest(
        &window,
        &episodes,
        params.include_agent_activity,
        top_n as usize,
        routines_touched,
    );
    response.episodes_scanned_rows = episodes_scanned_rows;
    response.routines_scanned_rows = routines_scanned_rows;
    Ok(response)
}

#[tool_router(router = timeline_digest_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Summarize a local day or week of operator activity from the episode store (CF_EPISODES, #846/#847): active vs idle time, time by app, top documents/sites, a per-day breakdown, and the mined routines (CF_ROUTINES, #848) whose evidence episodes fall in the period. Read-only and derived entirely from the same rows episode_list returns, so every total reconciles exactly with the episode store (active_ms == Σ episode durations == Σ by_app == Σ per_day). period is \"day\" or \"week\"; target the period with date (YYYY-MM-DD local) or anchor_ts_ns, defaulting to today. Human activity only unless include_agent_activity=true."
    )]
    pub async fn timeline_digest(
        &self,
        params: Parameters<TimelineDigestParams>,
    ) -> Result<Json<TimelineDigestResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_digest",
            period = %params.0.period,
            include_agent_activity = params.0.include_agent_activity,
            "tool.invocation kind=timeline_digest"
        );
        self.require_m3_permissions("timeline_digest", &required_permissions(&params.0))?;
        let runtime = self.reflex_runtime()?;
        build_digest(&runtime, &params.0).map(Json)
    }
}

#[cfg(test)]
mod tests {
    use synapse_core::types::{
        EPISODE_RECORD_VERSION, EpisodeBoundary, RoutineDowClass, RoutineEvidence,
        RoutineGranularity, RoutineRecord, RoutineStep,
    };

    use super::*;

    const SEC_NS: u64 = 1_000_000_000;

    fn view(
        start_ns: u64,
        dur_ms: u64,
        app: Option<&str>,
        document: Option<&str>,
        url: Option<&str>,
        keystrokes: u64,
        clicks: u64,
    ) -> EpisodeView {
        EpisodeView {
            key_hex: format!("k{start_ns:016x}"),
            ordinal: 0,
            episode_id: format!("ep1-{start_ns:016x}"),
            start_ts_ns: start_ns,
            end_ts_ns: start_ns + dur_ms * 1_000_000,
            duration_ms: dur_ms,
            actor: "human".to_owned(),
            app: app.map(str::to_owned),
            document: document.map(str::to_owned),
            url: url.map(str::to_owned),
            title_first: None,
            title_last: None,
            distinct_title_count: 0,
            row_count: 1,
            keystroke_count: keystrokes,
            click_count: clicks,
            interruption_count: 0,
            interrupted_ms: 0,
            started_because: EpisodeBoundary::AppSwitch,
            ended_because: EpisodeBoundary::AppSwitch,
        }
    }

    /// A single-day window anchored at an arbitrary local midnight.
    fn day_window(day_start: u64) -> PeriodWindow {
        PeriodWindow {
            period: "day".to_owned(),
            period_start_ns: day_start,
            period_end_ns: day_start + 86_400 * SEC_NS,
            day_starts: vec![day_start],
        }
    }

    fn reconciles(resp: &TimelineDigestResponse) {
        // active == Σ per_day == Σ by_app(+residual) == Σ by_doc(+residual)
        let day_active: u64 = resp.per_day.iter().map(|d| d.active_ms).sum();
        let day_idle: u64 = resp.per_day.iter().map(|d| d.idle_ms).sum();
        let day_eps: u64 = resp.per_day.iter().map(|d| d.episode_count).sum();
        assert_eq!(resp.active_ms, day_active, "active_ms == Σ per_day.active_ms");
        assert_eq!(resp.idle_ms, day_idle, "idle_ms == Σ per_day.idle_ms");
        assert_eq!(resp.episode_count, day_eps, "episode_count == Σ per_day");
        let app_active: u64 =
            resp.by_app.iter().map(|g| g.active_ms).sum::<u64>() + resp.by_app_other.active_ms;
        let app_eps: u64 = resp.by_app.iter().map(|g| g.episode_count).sum::<u64>()
            + resp.by_app_other.episode_count;
        assert_eq!(resp.active_ms, app_active, "active_ms == Σ by_app + residual");
        assert_eq!(resp.episode_count, app_eps, "episode_count == Σ by_app + residual");
        let doc_active: u64 = resp.top_documents.iter().map(|g| g.active_ms).sum::<u64>()
            + resp.top_documents_other.active_ms;
        assert_eq!(resp.active_ms, doc_active, "active_ms == Σ documents + residual");
    }

    #[test]
    fn empty_period_is_all_zero_and_reconciles() {
        let resp = aggregate_digest(&day_window(0), &[], false, 10, Vec::new());
        assert_eq!(resp.episode_count, 0);
        assert_eq!(resp.active_ms, 0);
        assert_eq!(resp.idle_ms, 0);
        assert!(resp.first_activity_ns.is_none());
        assert!(resp.by_app.is_empty());
        assert_eq!(resp.per_day.len(), 1, "one day row even when empty");
        assert_eq!(resp.actor_filter, "human");
        reconciles(&resp);
    }

    #[test]
    fn single_episode_totals_match_and_idle_is_zero() {
        let base = 100 * SEC_NS;
        let episodes = vec![view(base, 120_000, Some("code.exe"), None, None, 100, 5)];
        let resp = aggregate_digest(&day_window(0), &episodes, false, 10, Vec::new());
        assert_eq!(resp.episode_count, 1);
        assert_eq!(resp.active_ms, 120_000);
        assert_eq!(resp.idle_ms, 0, "one episode ⇒ envelope == active ⇒ idle 0");
        assert_eq!(resp.total_keystrokes, 100);
        assert_eq!(resp.total_clicks, 5);
        assert_eq!(resp.by_app.len(), 1);
        assert_eq!(resp.by_app[0].key, "code.exe");
        assert_eq!(resp.by_app[0].active_ms, 120_000);
        assert_eq!(resp.by_app[0].active_share_permille, 1000);
        reconciles(&resp);
    }

    #[test]
    fn gap_between_two_episodes_is_idle() {
        let base = 100 * SEC_NS;
        // [100s,160s) code (60s), 30s gap, [190s,250s) code (60s).
        let episodes = vec![
            view(base, 60_000, Some("code.exe"), None, None, 10, 1),
            view(base + 90 * SEC_NS, 60_000, Some("code.exe"), None, None, 20, 2),
        ];
        let resp = aggregate_digest(&day_window(0), &episodes, false, 10, Vec::new());
        assert_eq!(resp.active_ms, 120_000, "2 x 60s active");
        // envelope = 250s - 100s = 150s; idle = 150s - 120s = 30s.
        assert_eq!(resp.idle_ms, 30_000, "the 30s gap is idle");
        assert_eq!(resp.by_app.len(), 1, "same app folds together");
        assert_eq!(resp.by_app[0].episode_count, 2);
        reconciles(&resp);
    }

    #[test]
    fn missing_app_and_document_bucket_under_unknown() {
        let base = 100 * SEC_NS;
        let episodes = vec![view(base, 40_000, None, None, None, 0, 0)];
        let resp = aggregate_digest(&day_window(0), &episodes, false, 10, Vec::new());
        assert_eq!(resp.by_app[0].key, UNKNOWN_KEY);
        assert_eq!(resp.top_documents[0].key, UNKNOWN_KEY);
        reconciles(&resp);
    }

    #[test]
    fn top_n_truncation_preserves_totals_in_residual() {
        let base = 100 * SEC_NS;
        // Three apps with distinct durations; top_n=1 keeps the biggest.
        let episodes = vec![
            view(base, 30_000, Some("a.exe"), None, None, 0, 0),
            view(base + 60 * SEC_NS, 20_000, Some("b.exe"), None, None, 0, 0),
            view(base + 120 * SEC_NS, 10_000, Some("c.exe"), None, None, 0, 0),
        ];
        let resp = aggregate_digest(&day_window(0), &episodes, false, 1, Vec::new());
        assert_eq!(resp.by_app.len(), 1, "only the top app is broken out");
        assert_eq!(resp.by_app[0].key, "a.exe", "biggest active time first");
        assert_eq!(resp.by_app_other.group_count, 2, "b.exe + c.exe folded");
        assert_eq!(resp.by_app_other.active_ms, 30_000, "20s + 10s residual");
        reconciles(&resp);
    }

    #[test]
    fn document_url_is_representative_and_documents_aggregate() {
        let base = 100 * SEC_NS;
        let episodes = vec![
            view(base, 50_000, Some("chrome.exe"), Some("github.com"), Some("https://github.com/a"), 0, 0),
            view(base + 60 * SEC_NS, 70_000, Some("chrome.exe"), Some("github.com"), Some("https://github.com/b"), 0, 0),
        ];
        let resp = aggregate_digest(&day_window(0), &episodes, false, 10, Vec::new());
        assert_eq!(resp.top_documents.len(), 1, "same host folds together");
        assert_eq!(resp.top_documents[0].key, "github.com");
        assert_eq!(resp.top_documents[0].active_ms, 120_000);
        assert_eq!(
            resp.top_documents[0].url.as_deref(),
            Some("https://github.com/a"),
            "first-seen url is representative"
        );
        reconciles(&resp);
    }

    #[test]
    fn week_window_buckets_episodes_into_their_days() {
        let day0 = 7 * 86_400 * SEC_NS; // an arbitrary local midnight multiple
        let day1 = day0 + 86_400 * SEC_NS;
        let day2 = day1 + 86_400 * SEC_NS;
        let window = PeriodWindow {
            period: "week".to_owned(),
            period_start_ns: day0,
            period_end_ns: day2 + 86_400 * SEC_NS,
            day_starts: vec![day0, day1, day2],
        };
        let episodes = vec![
            view(day0 + 3_600 * SEC_NS, 60_000, Some("code.exe"), None, None, 0, 0),
            view(day2 + 7_200 * SEC_NS, 90_000, Some("chrome.exe"), None, None, 0, 0),
        ];
        let resp = aggregate_digest(&window, &episodes, false, 10, Vec::new());
        assert_eq!(resp.days_covered, 3);
        assert_eq!(resp.per_day.len(), 3);
        assert_eq!(resp.per_day[0].active_ms, 60_000, "day0 has the code episode");
        assert_eq!(resp.per_day[1].active_ms, 0, "day1 empty");
        assert_eq!(resp.per_day[2].active_ms, 90_000, "day2 has the chrome episode");
        assert_eq!(resp.active_ms, 150_000);
        reconciles(&resp);
    }

    fn routine(id: &str, episode_ids: &[&str], apps: &[&str], confidence: f64) -> RoutineRecord {
        RoutineRecord {
            record_version: EPISODE_RECORD_VERSION,
            ts_ns: 0,
            routine_id: id.to_owned(),
            granularity: RoutineGranularity::App,
            steps: apps
                .iter()
                .map(|app| RoutineStep {
                    app: (*app).to_owned(),
                    document: None,
                })
                .collect(),
            dow_class: RoutineDowClass::Daily,
            mean_minute_of_day: 540,
            tolerance_minutes: 30,
            schedule_label: "every day ~09:00".to_owned(),
            support_days: 5,
            occurrence_count: 5,
            opportunity_days: 5,
            confidence,
            window_start_ns: 0,
            window_end_ns: 0,
            active_days_in_window: 5,
            first_seen_day_start_ns: 0,
            last_seen_day_start_ns: 0,
            evidence: vec![RoutineEvidence {
                day_start_ns: 0,
                minute_of_day: 540,
                episode_ids: episode_ids.iter().map(|id| (*id).to_owned()).collect(),
            }],
        }
    }

    #[test]
    fn routine_matching_is_by_episode_id_intersection() {
        // Two routines: one whose evidence overlaps the period, one that does not.
        let in_period: BTreeSet<String> =
            ["ep1-a", "ep1-b", "ep1-c"].iter().map(|s| (*s).to_owned()).collect();
        let routines = vec![
            routine("rt-match", &["ep1-b", "ep1-zzz"], &["code.exe", "chrome.exe"], 0.812),
            routine("rt-miss", &["ep1-x", "ep1-y"], &["slack.exe"], 0.5),
        ];
        // Exercise the same selection logic the storage reader applies.
        let mut touched: Vec<RoutineTouched> = routines
            .into_iter()
            .filter_map(|r| {
                let matched = r
                    .evidence
                    .iter()
                    .flat_map(|e| e.episode_ids.iter())
                    .filter(|id| in_period.contains(*id))
                    .collect::<BTreeSet<_>>()
                    .len();
                (matched > 0).then(|| RoutineTouched {
                    routine_id: r.routine_id,
                    schedule_label: r.schedule_label,
                    confidence_permille: permille(r.confidence),
                    matched_episode_count: matched as u64,
                    step_apps: r.steps.into_iter().map(|s| s.app).collect(),
                })
            })
            .collect();
        touched.sort_by_key(|t| std::cmp::Reverse(t.matched_episode_count));
        assert_eq!(touched.len(), 1, "only the overlapping routine is touched");
        assert_eq!(touched[0].routine_id, "rt-match");
        assert_eq!(touched[0].matched_episode_count, 1, "only ep1-b overlaps");
        assert_eq!(touched[0].confidence_permille, 812);
        assert_eq!(touched[0].step_apps, ["code.exe", "chrome.exe"]);
    }

    #[test]
    fn share_and_permille_round_half_up_and_clamp() {
        assert_eq!(share_permille(0, 0), 0);
        assert_eq!(share_permille(1, 3), 333, "1/3 -> 333");
        assert_eq!(share_permille(2, 3), 667, "2/3 -> 667 (rounds up)");
        assert_eq!(share_permille(5, 5), 1000);
        assert_eq!(permille(-0.1), 0);
        assert_eq!(permille(0.0005), 1, "rounds to 1");
        assert_eq!(permille(1.5), 1000, "clamps");
    }

    #[test]
    fn resolve_window_rejects_bad_period_and_double_anchor() {
        let bad = TimelineDigestParams {
            period: "fortnight".to_owned(),
            ..TimelineDigestParams::default()
        };
        assert!(resolve_window(&bad).is_err());
        let both = TimelineDigestParams {
            period: "day".to_owned(),
            date: Some("2026-06-13".to_owned()),
            anchor_ts_ns: Some(1),
            ..TimelineDigestParams::default()
        };
        assert!(resolve_window(&both).is_err());
    }

    #[test]
    fn resolve_window_week_covers_seven_local_days() {
        let params = TimelineDigestParams {
            period: "week".to_owned(),
            date: Some("2026-06-13".to_owned()),
            ..TimelineDigestParams::default()
        };
        let window = resolve_window(&params).expect("valid week");
        assert_eq!(window.day_starts.len(), 7, "seven local days");
        assert_eq!(
            *window.day_starts.last().expect("last") + 86_400 * SEC_NS,
            window.period_end_ns,
            "period ends the midnight after the anchor day"
        );
    }
}
