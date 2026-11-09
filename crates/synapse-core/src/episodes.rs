//! Episode segmentation engine (#846, epic #830).
//!
//! Chunks `CF_TIMELINE` rows into EPISODES: contiguous spans of focused work
//! (app + document + start/end + interaction summary). The engine is a PURE,
//! DETERMINISTIC function of its inputs — same rows + same config produce
//! byte-identical episodes including their ids — so the timeline can be
//! re-segmented whenever the heuristics improve.
//!
//! Boundary heuristics (grounded in `ActivityWatch`'s AFK-split model and
//! field practice in `aw-export-timewarrior` / `OpenChronicle`):
//!
//! - App switch and document switch close one episode and open the next at
//!   the same instant. Browser documents use URL host; non-browser foreground
//!   documents use the foreground window title.
//! - `idle_start` closes the episode at the row's (backdated-to-last-input)
//!   timestamp; activity after `idle_end` opens the next one.
//! - `session_start` / `session_end` recorder boundaries always split.
//! - A silent gap (no evidence rows for longer than
//!   [`SegmentationConfig::silent_gap_ns`]) closes the episode at the last
//!   evidence timestamp — the defense against recorder death without a
//!   `session_end` row (power loss, kill -9).
//! - Episodes never span the segmented range: the caller segments one local
//!   day at a time, which is what makes day-aligned re-segmentation
//!   idempotent (an episode can never straddle two replacement windows).
//! - Rapid alt-tab noise: a foreign focus span shorter than
//!   [`SegmentationConfig::min_focus_ns`] sandwiched between two spans of
//!   the same (actor, app, document) is absorbed into the surrounding
//!   episode and accounted as an interruption — the "stickiness" rule that
//!   keeps flicker from fragmenting real work.
//!
//! Human activity only by default: agent-actor rows are counted and ignored
//! unless [`SegmentationConfig::include_agent_activity`] is set.

use std::collections::BTreeSet;

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::types::{
    EPISODE_RECORD_VERSION, EpisodeBoundary, EpisodeRecord, TimelineActor, TimelineKind,
    TimelineRecord,
};

/// Tuning knobs. Every field is an explicit deterministic input: nothing in
/// the engine reads clocks, locales, or environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentationConfig {
    /// Foreign focus spans shorter than this are absorbed as interruptions.
    pub min_focus_ns: u64,
    /// No evidence for longer than this closes the open episode.
    pub silent_gap_ns: u64,
    /// Include agent-actor rows (default false: human activity only).
    pub include_agent_activity: bool,
    /// Lowercased executable names treated as browsers for URL-host
    /// document identity.
    pub browser_apps: Vec<String>,
}

impl Default for SegmentationConfig {
    fn default() -> Self {
        Self {
            min_focus_ns: 5_000_000_000,    // 5 s
            silent_gap_ns: 600_000_000_000, // 10 min
            include_agent_activity: false,
            browser_apps: [
                "chrome.exe",
                "msedge.exe",
                "firefox.exe",
                "brave.exe",
                "opera.exe",
                "vivaldi.exe",
                "arc.exe",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }
}

/// Structured engine failures. Every variant names the offending input so a
/// failed segmentation is diagnosable without re-running it.
#[derive(Debug, Error)]
pub enum SegmentationError {
    #[error(
        "EPISODE_RANGE_INVALID: range_start_ns {range_start_ns} must be < range_end_ns {range_end_ns}"
    )]
    InvalidRange {
        range_start_ns: u64,
        range_end_ns: u64,
    },
    #[error(
        "EPISODE_ROW_OUT_OF_RANGE: row {index} ts_ns {ts_ns} outside [{range_start_ns}, {range_end_ns})"
    )]
    RowOutOfRange {
        index: usize,
        ts_ns: u64,
        range_start_ns: u64,
        range_end_ns: u64,
    },
    #[error(
        "EPISODE_ROWS_NOT_CHRONOLOGICAL: row {index} ts_ns {ts_ns} is earlier than predecessor {previous_ts_ns}"
    )]
    RowsNotChronological {
        index: usize,
        ts_ns: u64,
        previous_ts_ns: u64,
    },
    #[error("EPISODE_CONFIG_INVALID: {detail}")]
    InvalidConfig { detail: String },
}

/// Engine output: episodes plus loud accounting of everything that was not
/// segmented and why — nothing is silently skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Segmentation {
    pub episodes: Vec<EpisodeRecord>,
    /// Rows examined (all rows passed in).
    pub considered_rows: u64,
    /// Agent-actor rows ignored because `include_agent_activity` is false.
    pub ignored_agent_rows: u64,
    /// Rows whose payload was missing an expected field (e.g. an
    /// `interaction_summary` without counts) or whose attribution was
    /// anomalous (a `browser_nav` row from a non-browser executable); the
    /// caller must surface this count.
    pub payload_anomalies: u64,
}

/// Close metadata parked on a span until materialization, so interruption
/// absorption can merge spans without losing their working state.
struct ClosedMeta {
    end_ts_ns: u64,
    ended_because: EpisodeBoundary,
}

/// In-flight span; materialized into an [`EpisodeRecord`] at the end.
struct Span {
    actor: TimelineActor,
    app: Option<String>,
    document: Option<String>,
    url: Option<String>,
    start_ts_ns: u64,
    last_evidence_ts_ns: u64,
    titles: BTreeSet<String>,
    title_first: Option<String>,
    title_last: Option<String>,
    row_count: u64,
    keystroke_count: u64,
    click_count: u64,
    interruption_count: u32,
    interrupted_ms: u64,
    started_because: EpisodeBoundary,
    closed: Option<ClosedMeta>,
}

impl Span {
    const fn open(
        actor: TimelineActor,
        app: Option<String>,
        start_ts_ns: u64,
        started_because: EpisodeBoundary,
    ) -> Self {
        Self {
            actor,
            app,
            document: None,
            url: None,
            start_ts_ns,
            last_evidence_ts_ns: start_ts_ns,
            titles: BTreeSet::new(),
            title_first: None,
            title_last: None,
            row_count: 0,
            keystroke_count: 0,
            click_count: 0,
            interruption_count: 0,
            interrupted_ms: 0,
            started_because,
            closed: None,
        }
    }

    fn note_title(&mut self, title: &str) {
        if title.is_empty() {
            return;
        }
        if self.title_first.is_none() {
            self.title_first = Some(title.to_owned());
        }
        self.title_last = Some(title.to_owned());
        self.titles.insert(title.to_owned());
    }

    /// Identity key for interruption absorption: two spans merge across a
    /// flicker only when actor, app, and document all match.
    fn merge_key(&self) -> (String, Option<&str>, Option<&str>) {
        (
            actor_token(&self.actor),
            self.app.as_deref(),
            self.document.as_deref(),
        )
    }

    fn end_ts_ns(&self) -> u64 {
        self.closed
            .as_ref()
            .map_or(self.last_evidence_ts_ns, |meta| meta.end_ts_ns)
            .max(self.start_ts_ns)
    }

    fn ended_because(&self) -> EpisodeBoundary {
        self.closed
            .as_ref()
            .map_or(EpisodeBoundary::RangeEdge, |meta| meta.ended_because)
    }

    fn into_record(self) -> EpisodeRecord {
        let end_ts_ns = self.end_ts_ns();
        let ended_because = self.ended_because();
        let episode_id = episode_id(
            self.start_ts_ns,
            &self.actor,
            self.app.as_deref(),
            self.document.as_deref(),
        );
        EpisodeRecord {
            record_version: EPISODE_RECORD_VERSION,
            ts_ns: self.start_ts_ns,
            episode_id,
            start_ts_ns: self.start_ts_ns,
            end_ts_ns,
            actor: self.actor,
            app: self.app,
            document: self.document,
            url: self.url,
            title_first: self.title_first,
            title_last: self.title_last,
            distinct_title_count: u32::try_from(self.titles.len()).unwrap_or(u32::MAX),
            row_count: self.row_count,
            keystroke_count: self.keystroke_count,
            click_count: self.click_count,
            interruption_count: self.interruption_count,
            interrupted_ms: self.interrupted_ms,
            started_because: self.started_because,
            ended_because,
        }
    }
}

fn actor_token(actor: &TimelineActor) -> String {
    match actor {
        TimelineActor::Human => "human".to_owned(),
        TimelineActor::Agent { session_id } => format!("agent:{session_id}"),
    }
}

/// Deterministic stable id: `ep1-` + first 16 hex chars of SHA-256 over the
/// episode's identity tuple.
///
/// Re-segmenting the same timeline reproduces the same ids, so downstream
/// references (routine mining #848, feedback #856) survive re-segmentation.
#[must_use]
pub fn episode_id(
    start_ts_ns: u64,
    actor: &TimelineActor,
    app: Option<&str>,
    document: Option<&str>,
) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hasher = Sha256::new();
    hasher.update(start_ts_ns.to_be_bytes());
    hasher.update([0]);
    hasher.update(actor_token(actor).as_bytes());
    hasher.update([0]);
    hasher.update(app.unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(document.unwrap_or_default().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    format!("ep1-{hex}")
}

/// Extracts the lowercased host (with port, if any) from an http(s) URL.
/// Non-web schemes (`chrome://`, `file://`, `about:`) have no site identity
/// and return `None`.
fn url_host(url: &str) -> Option<String> {
    let (scheme, after_scheme) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Document identity for a browser navigation: URL host for web pages,
/// falling back to the whole URL for non-web schemes (`about:blank`,
/// `chrome://settings`).
fn browser_document(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    url_host(url).or_else(|| Some(url.to_ascii_lowercase()))
}

/// Document identity from foreground/title evidence. Browsers use
/// `browser_nav` URL rows instead because page titles are too volatile.
fn foreground_document(
    app: Option<&str>,
    title: Option<&str>,
    browser_apps: &[String],
) -> Option<String> {
    let app = app?;
    if browser_apps.contains(&app.to_ascii_lowercase()) {
        return None;
    }
    let title = title?.trim().trim_start_matches('*').trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_owned())
    }
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn payload_u64(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

fn close_span(
    spans: &mut Vec<Span>,
    current: &mut Option<Span>,
    end_ts_ns: u64,
    reason: EpisodeBoundary,
) {
    if let Some(mut span) = current.take() {
        span.closed = Some(ClosedMeta {
            end_ts_ns: end_ts_ns.max(span.start_ts_ns),
            ended_because: reason,
        });
        spans.push(span);
    }
}

/// Segments one contiguous range of timeline rows into episodes.
///
/// `rows` must be the complete set of `CF_TIMELINE` rows in `[range_start_ns,
/// range_end_ns)` in storage key order (chronological; the recorder keys rows
/// by their — possibly backdated — record `ts_ns`). The caller segments one
/// local day per call; `end_is_day_boundary` labels a span still open at the
/// end of the rows as `day_boundary` (interior day) instead of `range_edge`
/// (the in-progress day).
///
/// # Errors
///
/// Returns a [`SegmentationError`] when the range is empty/inverted, a row
/// falls outside the range, rows are not chronological, or the config is
/// internally inconsistent. The engine never skips bad input silently.
#[allow(clippy::too_many_lines)]
pub fn segment_range(
    rows: &[TimelineRecord],
    range_start_ns: u64,
    range_end_ns: u64,
    end_is_day_boundary: bool,
    config: &SegmentationConfig,
) -> Result<Segmentation, SegmentationError> {
    if range_start_ns >= range_end_ns {
        return Err(SegmentationError::InvalidRange {
            range_start_ns,
            range_end_ns,
        });
    }
    if config.min_focus_ns == 0 || config.silent_gap_ns == 0 {
        return Err(SegmentationError::InvalidConfig {
            detail: format!(
                "min_focus_ns ({}) and silent_gap_ns ({}) must both be > 0",
                config.min_focus_ns, config.silent_gap_ns
            ),
        });
    }
    let browser_apps: Vec<String> = config
        .browser_apps
        .iter()
        .map(|app| app.to_ascii_lowercase())
        .collect();

    let mut out = Segmentation {
        considered_rows: u64::try_from(rows.len()).unwrap_or(u64::MAX),
        ..Segmentation::default()
    };
    let mut spans: Vec<Span> = Vec::new();
    let mut current: Option<Span> = None;
    // Last known foreground (app, title) independent of episode state, so
    // activity after idle_end can reopen an episode without a focus row.
    let mut last_foreground: Option<(Option<String>, Option<String>)> = None;
    // Why the next span will have opened: the reason its predecessor closed.
    let mut next_open_reason = EpisodeBoundary::RangeEdge;
    let mut previous_ts_ns: Option<u64> = None;

    for (index, record) in rows.iter().enumerate() {
        let ts_ns = record.ts_ns;
        if ts_ns < range_start_ns || ts_ns >= range_end_ns {
            return Err(SegmentationError::RowOutOfRange {
                index,
                ts_ns,
                range_start_ns,
                range_end_ns,
            });
        }
        if let Some(previous) = previous_ts_ns
            && ts_ns < previous
        {
            return Err(SegmentationError::RowsNotChronological {
                index,
                ts_ns,
                previous_ts_ns: previous,
            });
        }
        previous_ts_ns = Some(ts_ns);

        // Non-activity rows are never evidence.
        if matches!(record.kind, TimelineKind::Purge | TimelineKind::DemoMarker) {
            continue;
        }
        // Agent rows: excluded by default, invisible to episode state.
        if !config.include_agent_activity && record.actor != TimelineActor::Human {
            out.ignored_agent_rows += 1;
            continue;
        }

        // Silent-gap defense: evidence arriving long after the last evidence
        // closes the stale span at its last evidence instant.
        if let Some(span) = current.as_ref()
            && ts_ns.saturating_sub(span.last_evidence_ts_ns) > config.silent_gap_ns
        {
            let end = span.last_evidence_ts_ns;
            close_span(&mut spans, &mut current, end, EpisodeBoundary::SilentGap);
            next_open_reason = EpisodeBoundary::SilentGap;
        }

        match record.kind {
            TimelineKind::FocusChange | TimelineKind::TitleChange => {
                let title = payload_str(&record.payload, "title").map(str::to_owned);
                let document =
                    foreground_document(record.app.as_deref(), title.as_deref(), &browser_apps);
                last_foreground = Some((record.app.clone(), title.clone()));
                let switched = current
                    .as_ref()
                    .is_none_or(|span| span.app != record.app || span.actor != record.actor);
                if switched {
                    close_span(&mut spans, &mut current, ts_ns, EpisodeBoundary::AppSwitch);
                    current = Some(Span::open(
                        record.actor.clone(),
                        record.app.clone(),
                        ts_ns,
                        next_open_reason,
                    ));
                    if let Some(span) = current.as_mut() {
                        span.document.clone_from(&document);
                    }
                    next_open_reason = EpisodeBoundary::AppSwitch;
                } else if current.as_ref().is_some_and(|span| {
                    span.document.is_some() && document.is_some() && span.document != document
                }) {
                    close_span(
                        &mut spans,
                        &mut current,
                        ts_ns,
                        EpisodeBoundary::DocumentSwitch,
                    );
                    let mut span = Span::open(
                        record.actor.clone(),
                        record.app.clone(),
                        ts_ns,
                        EpisodeBoundary::DocumentSwitch,
                    );
                    span.document.clone_from(&document);
                    current = Some(span);
                    next_open_reason = EpisodeBoundary::AppSwitch;
                }
                if let Some(span) = current.as_mut() {
                    if span.document.is_none() {
                        span.document = document;
                    }
                    span.row_count += 1;
                    span.last_evidence_ts_ns = ts_ns;
                    if let Some(title) = title.as_deref() {
                        span.note_title(title);
                    }
                }
            }
            TimelineKind::BrowserNav => {
                let url = payload_str(&record.payload, "url")
                    .unwrap_or_default()
                    .to_owned();
                let is_browser_app = record
                    .app
                    .as_deref()
                    .is_some_and(|app| browser_apps.contains(&app.to_ascii_lowercase()));
                if !is_browser_app {
                    // Navigation without a recognized browser executable is a
                    // producer anomaly worth counting, not acting on.
                    out.payload_anomalies += 1;
                    continue;
                }
                let document = browser_document(&url);
                if document.is_none() {
                    out.payload_anomalies += 1;
                }
                let same_app = current.as_ref().is_some_and(|span| {
                    span.app
                        .as_deref()
                        .zip(record.app.as_deref())
                        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                });
                if same_app {
                    let doc_switch = current.as_ref().is_some_and(|span| {
                        span.document.is_some() && document.is_some() && span.document != document
                    });
                    if doc_switch {
                        close_span(
                            &mut spans,
                            &mut current,
                            ts_ns,
                            EpisodeBoundary::DocumentSwitch,
                        );
                        let mut span = Span::open(
                            record.actor.clone(),
                            record.app.clone(),
                            ts_ns,
                            EpisodeBoundary::DocumentSwitch,
                        );
                        span.document = document;
                        span.url = Some(url);
                        span.row_count = 1;
                        if let Some(title) = payload_str(&record.payload, "title") {
                            span.note_title(title);
                        }
                        current = Some(span);
                        next_open_reason = EpisodeBoundary::AppSwitch;
                    } else if let Some(span) = current.as_mut() {
                        if span.document.is_none() {
                            span.document = document;
                        }
                        span.url = Some(url);
                        span.row_count += 1;
                        span.last_evidence_ts_ns = ts_ns;
                        if let Some(title) = payload_str(&record.payload, "title") {
                            span.note_title(title);
                        }
                    }
                } else if current.is_none() {
                    // Human navigation with no open episode (e.g. right after
                    // idle_end without a focus row): open from it.
                    let mut span = Span::open(
                        record.actor.clone(),
                        record.app.clone(),
                        ts_ns,
                        next_open_reason,
                    );
                    span.document = document;
                    span.url = Some(url);
                    span.row_count = 1;
                    if let Some(title) = payload_str(&record.payload, "title") {
                        span.note_title(title);
                    }
                    last_foreground = Some((record.app.clone(), None));
                    current = Some(span);
                    next_open_reason = EpisodeBoundary::AppSwitch;
                }
                // Background-tab navigation while another app holds focus is
                // deliberately not foreground evidence: ignored.
            }
            TimelineKind::InteractionSummary => {
                let keystrokes = payload_u64(&record.payload, "keystroke_count");
                let clicks = payload_u64(&record.payload, "click_count");
                if keystrokes.is_none() || clicks.is_none() {
                    out.payload_anomalies += 1;
                }
                let same_app = current.as_ref().is_some_and(|span| span.app == record.app);
                if !same_app {
                    // Input attributed to a different app than the open span
                    // means a missed focus event; trust the cadence row.
                    close_span(&mut spans, &mut current, ts_ns, EpisodeBoundary::AppSwitch);
                    current = Some(Span::open(
                        record.actor.clone(),
                        record.app.clone(),
                        ts_ns,
                        next_open_reason,
                    ));
                    next_open_reason = EpisodeBoundary::AppSwitch;
                    last_foreground = Some((record.app.clone(), None));
                }
                if let Some(span) = current.as_mut() {
                    span.row_count += 1;
                    span.keystroke_count += keystrokes.unwrap_or(0);
                    span.click_count += clicks.unwrap_or(0);
                    span.last_evidence_ts_ns = ts_ns;
                }
            }
            TimelineKind::Clipboard | TimelineKind::FileActivity => {
                // Extend-only evidence: keeps a quiet span alive but never
                // opens or switches one (file events can be background noise;
                // clipboard rows lack reliable foreground identity).
                if let Some(span) = current.as_mut() {
                    span.row_count += 1;
                    span.last_evidence_ts_ns = ts_ns;
                }
            }
            TimelineKind::IdleStart => {
                close_span(&mut spans, &mut current, ts_ns, EpisodeBoundary::IdleGap);
                next_open_reason = EpisodeBoundary::IdleGap;
            }
            TimelineKind::IdleEnd => {
                next_open_reason = EpisodeBoundary::IdleGap;
                if current.is_none()
                    && let Some((app, title)) = last_foreground.clone()
                {
                    let document =
                        foreground_document(app.as_deref(), title.as_deref(), &browser_apps);
                    let mut span =
                        Span::open(TimelineActor::Human, app, ts_ns, EpisodeBoundary::IdleGap);
                    span.document = document;
                    span.row_count = 1;
                    if let Some(title) = title.as_deref() {
                        span.note_title(title);
                    }
                    current = Some(span);
                    next_open_reason = EpisodeBoundary::AppSwitch;
                }
            }
            TimelineKind::SessionStart | TimelineKind::SessionEnd => {
                close_span(
                    &mut spans,
                    &mut current,
                    ts_ns,
                    EpisodeBoundary::SessionBoundary,
                );
                next_open_reason = EpisodeBoundary::SessionBoundary;
                last_foreground = None;
            }
            TimelineKind::Purge | TimelineKind::DemoMarker => {}
        }

        absorb_interruption(&mut spans, config.min_focus_ns);
    }

    // Whatever is still open closes at its last evidence instant.
    let tail_reason = if end_is_day_boundary {
        EpisodeBoundary::DayBoundary
    } else {
        EpisodeBoundary::RangeEdge
    };
    if let Some(span) = current.as_ref() {
        let end = span.last_evidence_ts_ns;
        close_span(&mut spans, &mut current, end, tail_reason);
    }
    absorb_interruption(&mut spans, config.min_focus_ns);

    out.episodes = spans.into_iter().map(Span::into_record).collect();
    Ok(out)
}

/// Stack-style absorption: when the three most recent closed spans form
/// [X, b, X'] with `b` shorter than `min_focus_ns`, `b` bounded by plain
/// focus switches on both sides, and X/X' sharing an identity key, the trio
/// merges into one span. Left-to-right application makes A-B-A-B-A chains
/// collapse deterministically.
fn absorb_interruption(spans: &mut Vec<Span>, min_focus_ns: u64) {
    while spans.len() >= 3 {
        let n = spans.len();
        let absorbable = {
            let left = &spans[n - 3];
            let mid = &spans[n - 2];
            let right = &spans[n - 1];
            let mid_duration = mid.end_ts_ns().saturating_sub(mid.start_ts_ns);
            let mid_flicker = mid.started_because == EpisodeBoundary::AppSwitch
                && mid.ended_because() == EpisodeBoundary::AppSwitch
                && mid_duration < min_focus_ns;
            let contiguous =
                left.end_ts_ns() == mid.start_ts_ns && mid.end_ts_ns() == right.start_ts_ns;
            mid_flicker && contiguous && left.merge_key() == right.merge_key()
        };
        if !absorbable {
            return;
        }
        let (Some(right), Some(mid)) = (spans.pop(), spans.pop()) else {
            return;
        };
        let Some(left) = spans.last_mut() else {
            return;
        };
        let mid_duration = mid.end_ts_ns().saturating_sub(mid.start_ts_ns);
        left.closed = right.closed;
        left.last_evidence_ts_ns = right.last_evidence_ts_ns.max(left.last_evidence_ts_ns);
        left.row_count += mid.row_count + right.row_count;
        // Sub-threshold counts from the flicker stay conserved in the
        // absorbing episode rather than vanishing with the span.
        left.keystroke_count += mid.keystroke_count + right.keystroke_count;
        left.click_count += mid.click_count + right.click_count;
        left.interruption_count += 1 + mid.interruption_count + right.interruption_count;
        left.interrupted_ms += mid_duration / 1_000_000 + mid.interrupted_ms + right.interrupted_ms;
        if right.url.is_some() {
            left.url = right.url;
        }
        if left.title_first.is_none() {
            left.title_first = right.title_first.clone();
        }
        if right.title_last.is_some() {
            left.title_last = right.title_last;
        }
        for title in right.titles {
            left.titles.insert(title);
        }
    }
}
