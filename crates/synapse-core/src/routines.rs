//! Routine mining engine (#848, epic #830).
//!
//! Mines recurring routines from the episode stream (#846): frequent
//! contiguous episode-identity sequences combined with temporal regularity.
//! The engine is a PURE, DETERMINISTIC function of its inputs — same days +
//! same config produce identical routines including their ids — so mining
//! can be re-run whenever episodes change and the routine store is always
//! exactly one mining run's output.
//!
//! Method (grounded in periodic frequent-pattern mining practice — support
//! plus periodicity gates, PAMI/RPGrowth lineage — and circular statistics
//! for time-of-day data):
//!
//! - Each local day's eligible episodes become one identity sequence per
//!   granularity (`App`, `AppDocument`); consecutive identical identities
//!   collapse into one step (heartbeat-style coalescing).
//! - All contiguous n-grams (1..=`max_pattern_len`) are candidate patterns;
//!   occurrences carry their day, weekday, start minute, and episode ids.
//! - Occurrence start minutes live on the 1440-minute circle. Clusters are
//!   contiguous arcs split at circular gaps larger than
//!   [`RoutineMiningConfig::cluster_split_gap_minutes`]; a cluster wider
//!   than [`RoutineMiningConfig::max_cluster_spread_minutes`] has no stable
//!   time and is rejected (temporal-regularity gate). Cluster center is the
//!   circular mean; tolerance is the maximum circular deviation from it.
//! - Support is DISTINCT DAYS inside the cluster, gated by
//!   [`RoutineMiningConfig::min_support_days`]. Confidence is the Wilson
//!   95% lower bound of `support_days / opportunity_days`, where the
//!   denominator is active days matching the routine's day-of-week class —
//!   honest at low support by construction (single-user data is sparse by
//!   design, #828).
//! - Closed-pattern suppression: a routine whose every occurrence is
//!   episode-id-contained in a same-day occurrence of a more specific
//!   routine (longer template, or `AppDocument` over `App`) with identical
//!   support adds no information and is dropped — the classic closed
//!   sequential pattern reduction, keyed on physical episode ids rather
//!   than means so an independent same-template cluster at another time of
//!   day is never falsely suppressed.
//!
//! Everything not promoted is accounted for in [`RoutineMining`] counters —
//! nothing is silently skipped, and every cap that fires is visible.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::types::{
    EpisodeRecord, ROUTINE_RECORD_VERSION, RoutineDowClass, RoutineEvidence, RoutineGranularity,
    RoutineRecord, RoutineStep, TimelineActor,
};

/// Minutes in the time-of-day circle.
///
/// Local days stretched or shrunk by DST transitions are folded onto this
/// circle (start minutes are taken modulo `MINUTES_PER_DAY`), trading one
/// hour of jitter on two days a year for a single well-defined circular
/// domain.
pub const MINUTES_PER_DAY: u32 = 1_440;

/// Wilson 95% z (matches the profile-quality scorer, #774).
const WILSON_Z_95: f64 = 1.959_963_984_540_054;

/// Evidence occurrences persisted per routine (newest kept).
pub const MAX_EVIDENCE_OCCURRENCES: usize = 8;

/// Tuning knobs. Every field is an explicit deterministic input: nothing in
/// the engine reads clocks, locales, or environment.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutineMiningConfig {
    /// Episodes shorter than this are noise, not routine steps.
    pub min_episode_duration_ns: u64,
    /// Same-identity episodes separated by no more than this merge into one
    /// step (idle-split work sessions); a wider separation is a genuine
    /// revisit and stays a distinct step.
    pub collapse_gap_ns: u64,
    /// A template never spans a between-step pause longer than this:
    /// routines are temporally compact behavioral units, not day-spanning
    /// coincidences (TSpan-style intra-pattern compactness).
    pub max_step_gap_ns: u64,
    /// Longest mined template, in steps.
    pub max_pattern_len: usize,
    /// A time cluster needs at least this many distinct days of support.
    pub min_support_days: u32,
    /// Circular gap (minutes) that separates two time clusters.
    pub cluster_split_gap_minutes: u32,
    /// A cluster spanning a wider arc than this has no stable time.
    pub max_cluster_spread_minutes: u32,
    /// Reported tolerances never shrink below this floor.
    pub min_tolerance_minutes: u32,
    /// Distinct candidate patterns tracked before new ones are counted as
    /// truncated instead of mined (complexity bound).
    pub max_candidates: usize,
    /// Routines kept after sorting; the remainder is counted as dropped.
    pub max_routines: usize,
    /// Occurrences of one pattern recorded per day (defends against
    /// pathological repetition inflating one day).
    pub max_occurrences_per_day: u32,
    /// Promotion floor on the Wilson lower bound: clusters that frequency
    /// coincidence alone can explain (low support over many opportunity
    /// days) never become routines.
    pub min_confidence: f64,
    /// Mine agent-actor episodes too (default false: human routines only).
    pub include_agent_activity: bool,
}

impl Default for RoutineMiningConfig {
    fn default() -> Self {
        Self {
            min_episode_duration_ns: 60_000_000_000, // 60 s
            collapse_gap_ns: 900_000_000_000,        // 15 min
            max_step_gap_ns: 1_800_000_000_000,      // 30 min
            max_pattern_len: 6,
            min_support_days: 3,
            cluster_split_gap_minutes: 120,
            max_cluster_spread_minutes: 180,
            min_tolerance_minutes: 5,
            max_candidates: 50_000,
            max_routines: 256,
            max_occurrences_per_day: 32,
            min_confidence: 0.15,
            include_agent_activity: false,
        }
    }
}

/// One local day of episodes, grouped by the caller.
///
/// The caller owns calendar math (local midnights, weekdays) so the engine
/// stays clock- and locale-free; `episode_segment`'s day snapping already
/// guarantees episodes never span local midnight (#846 invariant).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiningDay {
    /// Local-midnight day start (ns since epoch).
    pub day_start_ns: u64,
    /// Next local midnight.
    pub day_end_ns: u64,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u8,
    /// Episodes starting inside `[day_start_ns, day_end_ns)`, chronological.
    pub episodes: Vec<EpisodeRecord>,
}

/// Structured engine failures. Every variant names the offending input so a
/// failed mining run is diagnosable without re-running it.
#[derive(Debug, Error)]
pub enum RoutineMiningError {
    #[error("ROUTINE_CONFIG_INVALID: {detail}")]
    InvalidConfig { detail: String },
    #[error(
        "ROUTINE_DAY_INVALID: day {index} has day_start_ns {day_start_ns} >= day_end_ns {day_end_ns}"
    )]
    InvalidDay {
        index: usize,
        day_start_ns: u64,
        day_end_ns: u64,
    },
    #[error("ROUTINE_DAY_WEEKDAY_INVALID: day {index} weekday {weekday} is not in 0..=6")]
    InvalidWeekday { index: usize, weekday: u8 },
    #[error(
        "ROUTINE_DAYS_NOT_CHRONOLOGICAL: day {index} day_start_ns {day_start_ns} is not after predecessor end {previous_end_ns}"
    )]
    DaysNotChronological {
        index: usize,
        day_start_ns: u64,
        previous_end_ns: u64,
    },
    #[error(
        "ROUTINE_EPISODE_OUTSIDE_DAY: day {day_index} episode {episode_id} start_ts_ns {start_ts_ns} outside [{day_start_ns}, {day_end_ns})"
    )]
    EpisodeOutsideDay {
        day_index: usize,
        episode_id: String,
        start_ts_ns: u64,
        day_start_ns: u64,
        day_end_ns: u64,
    },
    #[error(
        "ROUTINE_EPISODES_NOT_CHRONOLOGICAL: day {day_index} episode {episode_id} start_ts_ns {start_ts_ns} is earlier than predecessor {previous_ts_ns}"
    )]
    EpisodesNotChronological {
        day_index: usize,
        episode_id: String,
        start_ts_ns: u64,
        previous_ts_ns: u64,
    },
}

/// Engine output: routines plus loud accounting of everything that was not
/// promoted and why.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoutineMining {
    pub routines: Vec<RoutineRecord>,
    /// Episodes examined across all days.
    pub considered_episodes: u64,
    /// Episodes that survived the eligibility filter.
    pub eligible_episodes: u64,
    pub filtered_agent_episodes: u64,
    pub filtered_short_episodes: u64,
    pub filtered_no_app_episodes: u64,
    /// Distinct candidate patterns tracked.
    pub candidates_evaluated: u64,
    /// New patterns ignored after `max_candidates` was reached.
    pub candidates_truncated: u64,
    /// Occurrences ignored after a pattern hit `max_occurrences_per_day`.
    pub occurrences_skipped_over_cap: u64,
    /// Time clusters rejected for support below `min_support_days`.
    pub clusters_rejected_low_support: u64,
    /// Time clusters rejected for arcs wider than
    /// `max_cluster_spread_minutes` (no stable time of day).
    pub clusters_rejected_dispersed: u64,
    /// Time clusters rejected for Wilson confidence below
    /// `min_confidence` (frequency coincidence, not a routine).
    pub clusters_rejected_low_confidence: u64,
    /// Promoted candidates removed by closed-pattern suppression.
    pub candidates_rejected_as_subpattern: u64,
    /// Promoted routines dropped past `max_routines` after sorting.
    pub routines_dropped_over_cap: u64,
    /// Days in the window with at least one eligible episode.
    pub active_days: u32,
}

/// Wilson 95% lower bound of `successes / sample_size`; 0.0 for an empty
/// sample. Same construction the profile-quality scorer uses, so routine and
/// profile confidence are comparable.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn wilson_lower_bound(successes: u64, sample_size: u64) -> f64 {
    if sample_size == 0 {
        return 0.0;
    }
    let n = sample_size as f64;
    let p = successes as f64 / n;
    let z2 = WILSON_Z_95 * WILSON_Z_95;
    let center = p + z2 / (2.0 * n);
    let margin = WILSON_Z_95 * (p.mul_add(1.0 - p, z2 / (4.0 * n)) / n).sqrt();
    ((center - margin) / (1.0 + z2 / n)).clamp(0.0, 1.0)
}

/// One collapsed identity step in a day's sequence.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct StepKey {
    app: String,
    document: Option<String>,
}

#[derive(Clone, Debug)]
struct SeqItem {
    key: StepKey,
    start_minute: u32,
    episode_id: String,
    has_document: bool,
    /// First episode start (step-gap bookkeeping only; never mined).
    start_ts_ns: u64,
    /// End of the most recent episode merged into this step (collapse and
    /// step-gap bookkeeping only; never mined).
    last_end_ts_ns: u64,
}

#[derive(Clone, Debug)]
struct Occurrence {
    day_index: u32,
    day_start_ns: u64,
    weekday: u8,
    minute: u32,
    episode_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct Candidate {
    granularity: RoutineGranularity,
    steps: Vec<RoutineStep>,
    occurrences: Vec<Occurrence>,
    per_day_counts: BTreeMap<u32, u32>,
}

/// A candidate × accepted-time-cluster pair, pre-record.
#[derive(Clone, Debug)]
struct Promoted {
    granularity: RoutineGranularity,
    steps: Vec<RoutineStep>,
    dow_class: RoutineDowClass,
    mean_minute: u32,
    tolerance_minutes: u32,
    support_days: u32,
    occurrence_count: u32,
    opportunity_days: u32,
    confidence: f64,
    day_set: BTreeSet<u32>,
    occurrences: Vec<Occurrence>,
    cluster_ordinal: u32,
}

const fn granularity_token(granularity: RoutineGranularity) -> &'static str {
    match granularity {
        RoutineGranularity::App => "app",
        RoutineGranularity::AppDocument => "app_document",
    }
}

fn dow_token(dow: &RoutineDowClass) -> String {
    match dow {
        RoutineDowClass::Daily => "daily".to_owned(),
        RoutineDowClass::Weekdays => "weekdays".to_owned(),
        RoutineDowClass::Weekend => "weekend".to_owned(),
        RoutineDowClass::Days { days } => {
            let list: Vec<String> = days.iter().map(u8::to_string).collect();
            format!("days:{}", list.join(","))
        }
    }
}

/// Deterministic stable id: `rt1-` + first 16 hex chars of SHA-256 over the
/// routine's identity tuple.
///
/// Excludes the mining timestamp and all derived statistics so re-mining
/// the same episodes reproduces the same ids — the property lifecycle state
/// (#849) and intent feedback (#856) anchor on.
#[must_use]
pub fn routine_id(
    granularity: RoutineGranularity,
    steps: &[RoutineStep],
    dow_class: &RoutineDowClass,
    cluster_ordinal: u32,
) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hasher = Sha256::new();
    hasher.update(granularity_token(granularity).as_bytes());
    for step in steps {
        hasher.update([0x1F]);
        hasher.update(step.app.as_bytes());
        hasher.update([0x1E]);
        hasher.update(step.document.as_deref().unwrap_or_default().as_bytes());
    }
    hasher.update([0x1F]);
    hasher.update(dow_token(dow_class).as_bytes());
    hasher.update([0x1F]);
    hasher.update(cluster_ordinal.to_be_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    format!("rt1-{hex}")
}

fn validate_config(config: &RoutineMiningConfig) -> Result<(), RoutineMiningError> {
    let mut problems = Vec::new();
    if config.max_pattern_len == 0 {
        problems.push("max_pattern_len must be > 0".to_owned());
    }
    if config.min_support_days == 0 {
        problems.push("min_support_days must be > 0".to_owned());
    }
    if config.cluster_split_gap_minutes == 0 || config.cluster_split_gap_minutes >= MINUTES_PER_DAY
    {
        problems.push(format!(
            "cluster_split_gap_minutes must be in 1..{MINUTES_PER_DAY}; got {}",
            config.cluster_split_gap_minutes
        ));
    }
    if config.max_cluster_spread_minutes == 0
        || config.max_cluster_spread_minutes >= MINUTES_PER_DAY
    {
        problems.push(format!(
            "max_cluster_spread_minutes must be in 1..{MINUTES_PER_DAY}; got {}",
            config.max_cluster_spread_minutes
        ));
    }
    if config.max_candidates == 0 {
        problems.push("max_candidates must be > 0".to_owned());
    }
    if config.max_routines == 0 {
        problems.push("max_routines must be > 0".to_owned());
    }
    if config.max_occurrences_per_day == 0 {
        problems.push("max_occurrences_per_day must be > 0".to_owned());
    }
    if config.max_step_gap_ns == 0 {
        problems.push("max_step_gap_ns must be > 0".to_owned());
    }
    if !(0.0..1.0).contains(&config.min_confidence) {
        problems.push(format!(
            "min_confidence must be in [0.0, 1.0); got {}",
            config.min_confidence
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(RoutineMiningError::InvalidConfig {
            detail: problems.join("; "),
        })
    }
}

fn validate_days(days: &[MiningDay]) -> Result<(), RoutineMiningError> {
    let mut previous_end: Option<u64> = None;
    for (index, day) in days.iter().enumerate() {
        if day.day_start_ns >= day.day_end_ns {
            return Err(RoutineMiningError::InvalidDay {
                index,
                day_start_ns: day.day_start_ns,
                day_end_ns: day.day_end_ns,
            });
        }
        if day.weekday > 6 {
            return Err(RoutineMiningError::InvalidWeekday {
                index,
                weekday: day.weekday,
            });
        }
        if let Some(previous_end_ns) = previous_end
            && day.day_start_ns < previous_end_ns
        {
            return Err(RoutineMiningError::DaysNotChronological {
                index,
                day_start_ns: day.day_start_ns,
                previous_end_ns,
            });
        }
        previous_end = Some(day.day_end_ns);
        let mut previous_ts: Option<u64> = None;
        for episode in &day.episodes {
            if episode.start_ts_ns < day.day_start_ns || episode.start_ts_ns >= day.day_end_ns {
                return Err(RoutineMiningError::EpisodeOutsideDay {
                    day_index: index,
                    episode_id: episode.episode_id.clone(),
                    start_ts_ns: episode.start_ts_ns,
                    day_start_ns: day.day_start_ns,
                    day_end_ns: day.day_end_ns,
                });
            }
            if let Some(previous_ts_ns) = previous_ts
                && episode.start_ts_ns < previous_ts_ns
            {
                return Err(RoutineMiningError::EpisodesNotChronological {
                    day_index: index,
                    episode_id: episode.episode_id.clone(),
                    start_ts_ns: episode.start_ts_ns,
                    previous_ts_ns,
                });
            }
            previous_ts = Some(episode.start_ts_ns);
        }
    }
    Ok(())
}

/// Builds one granularity's collapsed identity sequence for a day from the
/// day's pre-filtered eligible episodes.
///
/// Consecutive same-identity episodes merge only when the pause between
/// them is at most `collapse_gap_ns` — an idle-split work session is one
/// step, while a morning and an evening visit to the same identity are two
/// genuine revisits.
fn collapsed_sequence(
    eligible: &[&EpisodeRecord],
    day_start_ns: u64,
    granularity: RoutineGranularity,
    collapse_gap_ns: u64,
) -> Vec<SeqItem> {
    let mut sequence: Vec<SeqItem> = Vec::new();
    for episode in eligible {
        let app = episode
            .app
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let document = match granularity {
            RoutineGranularity::App => None,
            RoutineGranularity::AppDocument => {
                episode.document.as_deref().map(str::to_ascii_lowercase)
            }
        };
        let key = StepKey { app, document };
        if let Some(last) = sequence.last_mut()
            && last.key == key
            && episode.start_ts_ns.saturating_sub(last.last_end_ts_ns) <= collapse_gap_ns
        {
            last.last_end_ts_ns = last.last_end_ts_ns.max(episode.end_ts_ns);
            continue;
        }
        let minute_raw =
            u32::try_from(episode.start_ts_ns.saturating_sub(day_start_ns) / 60_000_000_000)
                .unwrap_or(0);
        sequence.push(SeqItem {
            has_document: key.document.is_some(),
            key,
            start_minute: minute_raw % MINUTES_PER_DAY,
            episode_id: episode.episode_id.clone(),
            start_ts_ns: episode.start_ts_ns,
            last_end_ts_ns: episode.end_ts_ns,
        });
    }
    sequence
}

fn candidate_key(granularity: RoutineGranularity, steps: &[SeqItem]) -> String {
    let mut key = String::from(granularity_token(granularity));
    for item in steps {
        key.push('\u{1F}');
        key.push_str(&item.key.app);
        key.push('\u{1E}');
        key.push_str(item.key.document.as_deref().unwrap_or_default());
    }
    key
}

/// Circular distance between two minutes on the day circle.
const fn circular_distance(a: u32, b: u32) -> u32 {
    let diff = a.abs_diff(b);
    if diff > MINUTES_PER_DAY / 2 {
        MINUTES_PER_DAY - diff
    } else {
        diff
    }
}

/// Circular mean minute of a non-empty set, via unit-vector averaging. Falls
/// back to the arc midpoint when the resultant vector degenerates (only
/// possible for pathological antipodal sets, which the spread gate rejects
/// before promotion anyway).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn circular_mean_minute(minutes: &[u32], arc_start: u32, arc_span: u32) -> u32 {
    let tau = std::f64::consts::TAU;
    let scale = tau / f64::from(MINUTES_PER_DAY);
    let (mut sin_sum, mut cos_sum) = (0.0_f64, 0.0_f64);
    for &minute in minutes {
        let angle = f64::from(minute) * scale;
        sin_sum += angle.sin();
        cos_sum += angle.cos();
    }
    let magnitude = sin_sum.hypot(cos_sum);
    if magnitude < 1e-9 {
        return (arc_start + arc_span / 2) % MINUTES_PER_DAY;
    }
    let mean_angle = sin_sum.atan2(cos_sum).rem_euclid(tau);
    let mean = (mean_angle / scale).round() as u32;
    mean % MINUTES_PER_DAY
}

/// One contiguous arc of occurrence indices on the day circle.
struct Arc {
    occurrence_indices: Vec<usize>,
    start_minute: u32,
    span_minutes: u32,
}

/// Splits occurrences into contiguous circular arcs at gaps wider than
/// `split_gap_minutes`. With no such gap, the whole set is one arc whose
/// span excludes the largest gap.
fn circular_arcs(occurrences: &[Occurrence], split_gap_minutes: u32) -> Vec<Arc> {
    if occurrences.is_empty() {
        return Vec::new();
    }
    // Sort indices by minute, tie-broken by occurrence order (stable).
    let mut order: Vec<usize> = (0..occurrences.len()).collect();
    order.sort_by_key(|&index| (occurrences[index].minute, index));
    let n = order.len();
    if n == 1 {
        return vec![Arc {
            occurrence_indices: order,
            start_minute: occurrences[0].minute,
            span_minutes: 0,
        }];
    }
    // gap[i] = circular distance from sorted point i to its successor.
    let mut gaps = Vec::with_capacity(n);
    for position in 0..n {
        let current = occurrences[order[position]].minute;
        let next = occurrences[order[(position + 1) % n]].minute;
        let gap = if position + 1 == n {
            next + MINUTES_PER_DAY - current
        } else {
            next - current
        };
        gaps.push(gap);
    }
    let split_positions: Vec<usize> = (0..n)
        .filter(|&position| gaps[position] > split_gap_minutes)
        .collect();
    if split_positions.is_empty() {
        // One arc covering everything; its span excludes the largest gap so
        // a tight cluster that happens to wrap midnight is measured fairly.
        let (max_gap_position, max_gap) = gaps
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|&(position, gap)| (gap, n - position))
            .unwrap_or((n - 1, 0));
        let start_position = (max_gap_position + 1) % n;
        let rotated: Vec<usize> = (0..n)
            .map(|offset| order[(start_position + offset) % n])
            .collect();
        let start_minute = occurrences[rotated[0]].minute;
        return vec![Arc {
            occurrence_indices: rotated,
            start_minute,
            span_minutes: MINUTES_PER_DAY.saturating_sub(max_gap),
        }];
    }
    // Each arc runs from just after one split gap to the next split gap.
    let mut arcs = Vec::with_capacity(split_positions.len());
    for (split_index, &split_position) in split_positions.iter().enumerate() {
        let start_position = (split_position + 1) % n;
        let end_position = split_positions[(split_index + 1) % split_positions.len()];
        let mut indices = Vec::new();
        let mut span = 0_u32;
        let mut position = start_position;
        loop {
            indices.push(order[position]);
            if position == end_position {
                break;
            }
            span += gaps[position];
            position = (position + 1) % n;
        }
        let start_minute = occurrences[indices[0]].minute;
        arcs.push(Arc {
            occurrence_indices: indices,
            start_minute,
            span_minutes: span,
        });
    }
    // Deterministic order: by arc start minute.
    arcs.sort_by_key(|arc| (arc.start_minute, arc.span_minutes));
    arcs
}

/// Day-of-week classification from per-weekday DISTINCT-DAY counts.
///
/// `Daily`, `Weekdays`, and `Weekend` are canonical classes. An arbitrary
/// `Days{…}` claim is selection-prone (any three observations define some
/// day set), so it additionally requires at least two distinct days of
/// evidence per member weekday; thinner evidence is evaluated as `Daily`
/// against all active days, which keeps the confidence denominator honest.
fn classify_dow(day_counts_by_weekday: &BTreeMap<u8, u32>) -> RoutineDowClass {
    let weekdays: BTreeSet<u8> = day_counts_by_weekday.keys().copied().collect();
    let weekday_set: BTreeSet<u8> = (0..=4).collect();
    let weekend_set: BTreeSet<u8> = [5, 6].into_iter().collect();
    if weekdays.len() >= 6 {
        RoutineDowClass::Daily
    } else if weekdays == weekday_set {
        RoutineDowClass::Weekdays
    } else if weekdays == weekend_set {
        RoutineDowClass::Weekend
    } else if !weekdays.is_empty() && day_counts_by_weekday.values().all(|&count| count >= 2) {
        RoutineDowClass::Days {
            days: weekdays.iter().copied().collect(),
        }
    } else {
        RoutineDowClass::Daily
    }
}

fn opportunity_days(dow_class: &RoutineDowClass, active_by_weekday: &[u32; 7]) -> u32 {
    match dow_class {
        RoutineDowClass::Daily => active_by_weekday.iter().sum(),
        RoutineDowClass::Weekdays => active_by_weekday[..5].iter().sum(),
        RoutineDowClass::Weekend => active_by_weekday[5] + active_by_weekday[6],
        RoutineDowClass::Days { days } => days
            .iter()
            .map(|&day| {
                active_by_weekday
                    .get(usize::from(day))
                    .copied()
                    .unwrap_or(0)
            })
            .sum(),
    }
}

fn format_minute(minute: u32) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

fn dow_label(dow: &RoutineDowClass) -> String {
    const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    match dow {
        RoutineDowClass::Daily => "daily".to_owned(),
        RoutineDowClass::Weekdays => "weekdays".to_owned(),
        RoutineDowClass::Weekend => "weekend".to_owned(),
        RoutineDowClass::Days { days } => {
            let list: Vec<&str> = days
                .iter()
                .map(|&day| NAMES.get(usize::from(day)).copied().unwrap_or("?"))
                .collect();
            format!("days {}", list.join(","))
        }
    }
}

fn schedule_label(dow: &RoutineDowClass, mean_minute: u32, tolerance_minutes: u32) -> String {
    format!(
        "{} {}±{}m",
        dow_label(dow),
        format_minute(mean_minute),
        tolerance_minutes
    )
}

/// True when `inner`'s episode ids all appear in a same-day occurrence of
/// `outer` — the physical-containment test closed-pattern suppression uses.
fn occurrence_contained(inner: &Occurrence, outer_occurrences: &[Occurrence]) -> bool {
    outer_occurrences.iter().any(|outer| {
        outer.day_index == inner.day_index
            && inner
                .episode_ids
                .iter()
                .all(|episode_id| outer.episode_ids.contains(episode_id))
    })
}

fn suppressed_by(inner: &Promoted, outer: &Promoted) -> bool {
    let more_specific = outer.steps.len() > inner.steps.len()
        || (outer.steps.len() == inner.steps.len()
            && outer.granularity == RoutineGranularity::AppDocument
            && inner.granularity == RoutineGranularity::App);
    if !more_specific {
        return false;
    }
    if inner.support_days != outer.support_days
        || inner.occurrence_count != outer.occurrence_count
        || inner.dow_class != outer.dow_class
        || inner.day_set != outer.day_set
    {
        return false;
    }
    inner
        .occurrences
        .iter()
        .all(|occurrence| occurrence_contained(occurrence, &outer.occurrences))
}

/// Mines routines from day-grouped episodes.
///
/// `mined_at_ts_ns` stamps the produced records' `ts_ns` (provenance only;
/// ids never include it).
///
/// # Errors
///
/// Returns a [`RoutineMiningError`] when the config is internally
/// inconsistent, days are malformed or out of order, or an episode falls
/// outside its day. The engine never skips bad input silently.
#[allow(clippy::too_many_lines)]
pub fn mine_routines(
    days: &[MiningDay],
    mined_at_ts_ns: u64,
    config: &RoutineMiningConfig,
) -> Result<RoutineMining, RoutineMiningError> {
    validate_config(config)?;
    validate_days(days)?;

    let mut out = RoutineMining::default();
    let mut active_by_weekday = [0_u32; 7];
    let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();

    for (day_index, day) in days.iter().enumerate() {
        let day_index_u32 = u32::try_from(day_index).unwrap_or(u32::MAX);
        out.considered_episodes += u64::try_from(day.episodes.len()).unwrap_or(u64::MAX);
        let mut eligible: Vec<&EpisodeRecord> = Vec::with_capacity(day.episodes.len());
        for episode in &day.episodes {
            if !config.include_agent_activity && episode.actor != TimelineActor::Human {
                out.filtered_agent_episodes += 1;
                continue;
            }
            if episode.end_ts_ns.saturating_sub(episode.start_ts_ns)
                < config.min_episode_duration_ns
            {
                out.filtered_short_episodes += 1;
                continue;
            }
            if episode.app.as_deref().is_none_or(str::is_empty) {
                out.filtered_no_app_episodes += 1;
                continue;
            }
            eligible.push(episode);
        }
        if eligible.is_empty() {
            continue;
        }
        out.eligible_episodes += u64::try_from(eligible.len()).unwrap_or(u64::MAX);
        out.active_days += 1;
        active_by_weekday[usize::from(day.weekday)] += 1;

        for granularity in [RoutineGranularity::App, RoutineGranularity::AppDocument] {
            let sequence = collapsed_sequence(
                &eligible,
                day.day_start_ns,
                granularity,
                config.collapse_gap_ns,
            );
            let len = sequence.len();
            // chainable[i]: step i+1 may extend a template ending at step i
            // (the pause between them is within the compactness bound).
            let chainable: Vec<bool> = sequence
                .windows(2)
                .map(|pair| {
                    pair[1].start_ts_ns.saturating_sub(pair[0].last_end_ts_ns)
                        <= config.max_step_gap_ns
                })
                .collect();
            for start in 0..len {
                let max_n = config.max_pattern_len.min(len - start);
                for n in 1..=max_n {
                    if n >= 2 && !chainable[start + n - 2] {
                        break;
                    }
                    let window = &sequence[start..start + n];
                    // The AppDocument pass only mines grams that carry
                    // document identity somewhere; doc-less grams are exact
                    // duplicates of the App pass.
                    if granularity == RoutineGranularity::AppDocument
                        && !window.iter().any(|item| item.has_document)
                    {
                        continue;
                    }
                    let key = candidate_key(granularity, window);
                    if !candidates.contains_key(&key) && candidates.len() >= config.max_candidates {
                        out.candidates_truncated += 1;
                        continue;
                    }
                    let candidate = candidates.entry(key).or_insert_with(|| Candidate {
                        granularity,
                        steps: window
                            .iter()
                            .map(|item| RoutineStep {
                                app: item.key.app.clone(),
                                document: item.key.document.clone(),
                            })
                            .collect(),
                        occurrences: Vec::new(),
                        per_day_counts: BTreeMap::new(),
                    });
                    let day_count = candidate.per_day_counts.entry(day_index_u32).or_insert(0);
                    if *day_count >= config.max_occurrences_per_day {
                        out.occurrences_skipped_over_cap += 1;
                        continue;
                    }
                    *day_count += 1;
                    candidate.occurrences.push(Occurrence {
                        day_index: day_index_u32,
                        day_start_ns: day.day_start_ns,
                        weekday: day.weekday,
                        minute: window[0].start_minute,
                        episode_ids: window.iter().map(|item| item.episode_id.clone()).collect(),
                    });
                }
            }
        }
    }
    out.candidates_evaluated = u64::try_from(candidates.len()).unwrap_or(u64::MAX);

    // Promote candidate × time-cluster pairs that pass the support and
    // regularity gates.
    let mut promoted: Vec<Promoted> = Vec::new();
    for candidate in candidates.values() {
        let distinct_days: BTreeSet<u32> = candidate
            .occurrences
            .iter()
            .map(|occurrence| occurrence.day_index)
            .collect();
        if u32::try_from(distinct_days.len()).unwrap_or(u32::MAX) < config.min_support_days {
            // Below support before clustering; cheaper to reject here, and
            // accounted under the same counter as per-cluster rejections.
            out.clusters_rejected_low_support += 1;
            continue;
        }
        let arcs = circular_arcs(&candidate.occurrences, config.cluster_split_gap_minutes);
        let mut accepted: Vec<Promoted> = Vec::new();
        for arc in arcs {
            if arc.span_minutes > config.max_cluster_spread_minutes {
                out.clusters_rejected_dispersed += 1;
                continue;
            }
            let cluster: Vec<&Occurrence> = arc
                .occurrence_indices
                .iter()
                .map(|&index| &candidate.occurrences[index])
                .collect();
            let day_set: BTreeSet<u32> = cluster
                .iter()
                .map(|occurrence| occurrence.day_index)
                .collect();
            let support_days = u32::try_from(day_set.len()).unwrap_or(u32::MAX);
            if support_days < config.min_support_days {
                out.clusters_rejected_low_support += 1;
                continue;
            }
            let minutes: Vec<u32> = cluster.iter().map(|occurrence| occurrence.minute).collect();
            let mean_minute = circular_mean_minute(&minutes, arc.start_minute, arc.span_minutes);
            let tolerance = minutes
                .iter()
                .map(|&minute| circular_distance(minute, mean_minute))
                .max()
                .unwrap_or(0)
                .max(config.min_tolerance_minutes);
            let mut day_counts_by_weekday: BTreeMap<u8, BTreeSet<u32>> = BTreeMap::new();
            for occurrence in &cluster {
                day_counts_by_weekday
                    .entry(occurrence.weekday)
                    .or_default()
                    .insert(occurrence.day_index);
            }
            let day_counts_by_weekday: BTreeMap<u8, u32> = day_counts_by_weekday
                .into_iter()
                .map(|(weekday, day_indices)| {
                    (
                        weekday,
                        u32::try_from(day_indices.len()).unwrap_or(u32::MAX),
                    )
                })
                .collect();
            let dow_class = classify_dow(&day_counts_by_weekday);
            let opportunities = opportunity_days(&dow_class, &active_by_weekday).max(support_days);
            let confidence = wilson_lower_bound(u64::from(support_days), u64::from(opportunities));
            if confidence < config.min_confidence {
                out.clusters_rejected_low_confidence += 1;
                continue;
            }
            // Chronological occurrence order for evidence and containment.
            let mut occurrences: Vec<Occurrence> = cluster.into_iter().cloned().collect();
            occurrences.sort_by_key(|occurrence| (occurrence.day_index, occurrence.minute));
            accepted.push(Promoted {
                granularity: candidate.granularity,
                steps: candidate.steps.clone(),
                dow_class,
                mean_minute,
                tolerance_minutes: tolerance,
                support_days,
                occurrence_count: u32::try_from(occurrences.len()).unwrap_or(u32::MAX),
                opportunity_days: opportunities,
                confidence,
                day_set,
                occurrences,
                cluster_ordinal: 0,
            });
        }
        // Ordinals follow cluster mean order so an id never depends on
        // which clusters happened to be rejected.
        accepted.sort_by_key(|cluster| cluster.mean_minute);
        for (ordinal, mut cluster) in accepted.into_iter().enumerate() {
            cluster.cluster_ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
            promoted.push(cluster);
        }
    }

    // Closed-pattern suppression on physical episode-id containment.
    let keep: Vec<bool> = promoted
        .iter()
        .map(|inner| {
            !promoted
                .iter()
                .any(|outer| !std::ptr::eq(inner, outer) && suppressed_by(inner, outer))
        })
        .collect();
    let mut survivors: Vec<Promoted> = promoted
        .into_iter()
        .zip(&keep)
        .filter_map(|(cluster, &kept)| {
            if kept {
                Some(cluster)
            } else {
                out.candidates_rejected_as_subpattern += 1;
                None
            }
        })
        .collect();

    // Deterministic rank: strongest support, then confidence, then identity.
    survivors.sort_by(|a, b| {
        b.support_days
            .cmp(&a.support_days)
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| a.steps.len().cmp(&b.steps.len()).reverse())
            .then_with(|| {
                routine_id(a.granularity, &a.steps, &a.dow_class, a.cluster_ordinal).cmp(
                    &routine_id(b.granularity, &b.steps, &b.dow_class, b.cluster_ordinal),
                )
            })
    });
    if survivors.len() > config.max_routines {
        out.routines_dropped_over_cap =
            u64::try_from(survivors.len() - config.max_routines).unwrap_or(u64::MAX);
        survivors.truncate(config.max_routines);
    }

    let window_start_ns = days.first().map_or(0, |day| day.day_start_ns);
    let window_end_ns = days.last().map_or(0, |day| day.day_end_ns);
    out.routines = survivors
        .into_iter()
        .map(|cluster| {
            let id = routine_id(
                cluster.granularity,
                &cluster.steps,
                &cluster.dow_class,
                cluster.cluster_ordinal,
            );
            let evidence_start = cluster
                .occurrences
                .len()
                .saturating_sub(MAX_EVIDENCE_OCCURRENCES);
            let evidence: Vec<RoutineEvidence> = cluster.occurrences[evidence_start..]
                .iter()
                .map(|occurrence| RoutineEvidence {
                    day_start_ns: occurrence.day_start_ns,
                    minute_of_day: occurrence.minute,
                    episode_ids: occurrence.episode_ids.clone(),
                })
                .collect();
            let first_seen = cluster
                .occurrences
                .first()
                .map_or(0, |occurrence| occurrence.day_start_ns);
            let last_seen = cluster
                .occurrences
                .last()
                .map_or(0, |occurrence| occurrence.day_start_ns);
            RoutineRecord {
                record_version: ROUTINE_RECORD_VERSION,
                ts_ns: mined_at_ts_ns,
                routine_id: id,
                granularity: cluster.granularity,
                steps: cluster.steps,
                schedule_label: schedule_label(
                    &cluster.dow_class,
                    cluster.mean_minute,
                    cluster.tolerance_minutes,
                ),
                dow_class: cluster.dow_class,
                mean_minute_of_day: cluster.mean_minute,
                tolerance_minutes: cluster.tolerance_minutes,
                support_days: cluster.support_days,
                occurrence_count: cluster.occurrence_count,
                opportunity_days: cluster.opportunity_days,
                confidence: cluster.confidence,
                window_start_ns,
                window_end_ns,
                active_days_in_window: out.active_days,
                first_seen_day_start_ns: first_seen,
                last_seen_day_start_ns: last_seen,
                evidence,
            }
        })
        .collect();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::types::{EPISODE_RECORD_VERSION, EpisodeBoundary};

    use super::*;

    /// Synthetic local days: day k starts at BASE + k days (UTC-aligned for
    /// test simplicity; the engine never does calendar math itself).
    const BASE: u64 = 1_700_000_000_000_000_000;
    const DAY_NS: u64 = 86_400_000_000_000;
    const MIN_NS: u64 = 60_000_000_000;

    fn episode(
        day_start: u64,
        minute: u32,
        duration_min: u32,
        app: &str,
        document: Option<&str>,
    ) -> EpisodeRecord {
        let start = day_start + u64::from(minute) * MIN_NS;
        let end = start + u64::from(duration_min) * MIN_NS;
        EpisodeRecord {
            record_version: EPISODE_RECORD_VERSION,
            ts_ns: start,
            episode_id: crate::episodes::episode_id(
                start,
                &TimelineActor::Human,
                Some(app),
                document,
            ),
            start_ts_ns: start,
            end_ts_ns: end,
            actor: TimelineActor::Human,
            app: Some(app.to_owned()),
            document: document.map(str::to_owned),
            url: None,
            title_first: None,
            title_last: None,
            distinct_title_count: 1,
            row_count: 4,
            keystroke_count: 10,
            click_count: 2,
            interruption_count: 0,
            interrupted_ms: 0,
            started_because: EpisodeBoundary::AppSwitch,
            ended_because: EpisodeBoundary::AppSwitch,
        }
    }

    fn day(index: u64, weekday: u8, episodes: Vec<EpisodeRecord>) -> MiningDay {
        let day_start_ns = BASE + index * DAY_NS;
        MiningDay {
            day_start_ns,
            day_end_ns: day_start_ns + DAY_NS,
            weekday,
            episodes,
        }
    }

    /// Ten simulated working days (Mon–Fri twice) with a planted morning
    /// routine — outlook → excel(report.xlsx) → teams around 08:45 — plus
    /// non-recurring noise apps at scattered times.
    fn planted_week(jitter: &[i64]) -> Vec<MiningDay> {
        let mut days = Vec::new();
        let noise_apps = [
            "spotify.exe",
            "calc.exe",
            "paint.exe",
            "zoom.exe",
            "vlc.exe",
            "gimp.exe",
            "obs64.exe",
            "blender.exe",
            "audacity.exe",
            "krita.exe",
        ];
        for k in 0..10 {
            let weekday = u8::try_from(k % 5).unwrap_or(0);
            let day_start = BASE + u64::try_from(k).unwrap_or(0) * DAY_NS;
            let offset = jitter[k % jitter.len()];
            let start_minute = u32::try_from(525 + offset).unwrap_or(525);
            let mut episodes = vec![
                episode(day_start, start_minute, 10, "outlook.exe", Some("inbox")),
                episode(
                    day_start,
                    start_minute + 10,
                    20,
                    "excel.exe",
                    Some("report.xlsx"),
                ),
                episode(day_start, start_minute + 30, 5, "teams.exe", Some("chat")),
            ];
            // One unique noise app per day at a scattered afternoon time.
            episodes.push(episode(
                day_start,
                700 + u32::try_from(k).unwrap_or(0) * 37,
                15,
                noise_apps[k],
                None,
            ));
            days.push(MiningDay {
                day_start_ns: day_start,
                day_end_ns: day_start + DAY_NS,
                weekday,
                episodes,
            });
        }
        days
    }

    #[test]
    fn planted_routine_is_mined_with_correct_signature() {
        let days = planted_week(&[0, 5, -10, 15, -5, 10, 0, -15, 5, -5]);
        let result = mine_routines(&days, BASE, &RoutineMiningConfig::default())
            .expect("mining must succeed");
        println!(
            "planted candidates={} routines={} active_days={}",
            result.candidates_evaluated,
            result.routines.len(),
            result.active_days
        );
        for routine in &result.routines {
            println!(
                "  {} {:?} steps={:?} label={} support={} conf={:.3}",
                routine.routine_id,
                routine.granularity,
                routine
                    .steps
                    .iter()
                    .map(|step| step.app.as_str())
                    .collect::<Vec<_>>(),
                routine.schedule_label,
                routine.support_days,
                routine.confidence
            );
        }
        // The maximal three-step document-level template must be the top
        // routine: support 10/10 weekday-active days.
        let top = result.routines.first().expect("at least one routine");
        assert_eq!(top.granularity, RoutineGranularity::AppDocument);
        assert_eq!(
            top.steps
                .iter()
                .map(|step| step.app.as_str())
                .collect::<Vec<_>>(),
            ["outlook.exe", "excel.exe", "teams.exe"]
        );
        assert_eq!(
            top.steps[1].document.as_deref(),
            Some("report.xlsx"),
            "document identity must survive into the template"
        );
        assert_eq!(top.support_days, 10);
        assert_eq!(top.opportunity_days, 10);
        assert_eq!(top.dow_class, RoutineDowClass::Weekdays);
        // Mean within the jitter envelope of 08:45 (minute 525).
        assert!(
            circular_distance(top.mean_minute_of_day, 525) <= 15,
            "mean {} should be near 525",
            top.mean_minute_of_day
        );
        assert!(top.tolerance_minutes <= 30, "tol={}", top.tolerance_minutes);
        assert!(
            top.confidence > 0.7,
            "ten-for-ten support must score high: {}",
            top.confidence
        );
        assert!(
            top.schedule_label.starts_with("weekdays "),
            "{}",
            top.schedule_label
        );
        // Noise apps occurred once each: never promoted.
        for routine in &result.routines {
            assert!(
                !routine.steps.iter().any(|step| step.app == "spotify.exe"),
                "single-occurrence noise must not be promoted"
            );
        }
        // Sub-patterns of the planted routine carry no extra information
        // and must be suppressed: every survivor that mentions excel.exe is
        // the maximal template.
        for routine in &result.routines {
            if routine.steps.iter().any(|step| step.app == "excel.exe") {
                assert_eq!(routine.steps.len(), 3, "{:?}", routine.steps);
            }
        }
        assert!(result.candidates_rejected_as_subpattern > 0);
    }

    #[test]
    fn mining_is_deterministic_including_ids() {
        let days = planted_week(&[0, 5, -10, 15, -5]);
        let first = mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("first");
        let second = mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("second");
        assert_eq!(first, second);
        for routine in &first.routines {
            assert!(
                routine.routine_id.starts_with("rt1-") && routine.routine_id.len() == 20,
                "id format: {}",
                routine.routine_id
            );
        }
        // A different mining timestamp changes ts_ns but never ids.
        let restamped =
            mine_routines(&days, BASE + DAY_NS, &RoutineMiningConfig::default()).expect("restamp");
        let ids = |mining: &RoutineMining| {
            mining
                .routines
                .iter()
                .map(|routine| routine.routine_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&first), ids(&restamped));
        println!("determinism ids={:?}", ids(&first));
    }

    #[test]
    fn noise_below_support_is_not_promoted() {
        // Two days of identical sequence: below min_support_days=3.
        let days: Vec<MiningDay> = (0..2)
            .map(|k| {
                day(
                    k,
                    u8::try_from(k).unwrap_or(0),
                    vec![
                        episode(BASE + k * DAY_NS, 540, 10, "outlook.exe", None),
                        episode(BASE + k * DAY_NS, 550, 10, "excel.exe", None),
                    ],
                )
            })
            .collect();
        let result =
            mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("mining succeeds");
        println!(
            "below_support routines={} rejected_low_support={}",
            result.routines.len(),
            result.clusters_rejected_low_support
        );
        assert!(result.routines.is_empty());
        assert!(result.clusters_rejected_low_support > 0);
    }

    #[test]
    fn frequent_but_time_dispersed_pattern_is_rejected() {
        // Same app daily, but at wildly different times: frequency without
        // temporal regularity must not produce a scheduled routine.
        let minutes = [10, 300, 600, 900, 1200, 60, 400, 800, 1100, 1350];
        let days: Vec<MiningDay> = (0..10)
            .map(|k| {
                day(
                    k,
                    u8::try_from(k % 7).unwrap_or(0),
                    vec![episode(
                        BASE + k * DAY_NS,
                        minutes[usize::try_from(k).unwrap_or(0)],
                        15,
                        "notepad.exe",
                        None,
                    )],
                )
            })
            .collect();
        let result =
            mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("mining succeeds");
        println!(
            "dispersed routines={} rejected_dispersed={} rejected_low_confidence={}",
            result.routines.len(),
            result.clusters_rejected_dispersed,
            result.clusters_rejected_low_confidence
        );
        assert!(result.routines.is_empty());
        // Scattered times either form a wide arc (dispersed) or accidental
        // micro-clusters whose honest Daily-denominator confidence is below
        // the promotion floor — both gates must be visible in the counters.
        assert!(result.clusters_rejected_dispersed + result.clusters_rejected_low_confidence > 0);
    }

    #[test]
    fn two_independent_time_clusters_become_two_routines() {
        // notepad at ~09:00 and ~17:30 every day: two distinct routines
        // with distinct ids, neither suppressing the other.
        let days: Vec<MiningDay> = (0..7)
            .map(|k| {
                day(
                    k,
                    u8::try_from(k % 7).unwrap_or(0),
                    vec![
                        episode(BASE + k * DAY_NS, 540, 10, "notepad.exe", None),
                        episode(BASE + k * DAY_NS, 1050, 10, "notepad.exe", None),
                    ],
                )
            })
            .collect();
        let result =
            mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("mining succeeds");
        for routine in &result.routines {
            println!(
                "two_clusters {} label={} support={}",
                routine.routine_id, routine.schedule_label, routine.support_days
            );
        }
        assert_eq!(result.routines.len(), 2);
        assert_eq!(result.routines[0].dow_class, RoutineDowClass::Daily);
        let mut means: Vec<u32> = result
            .routines
            .iter()
            .map(|routine| routine.mean_minute_of_day)
            .collect();
        means.sort_unstable();
        assert_eq!(means, vec![540, 1050]);
        let ids: BTreeSet<&str> = result
            .routines
            .iter()
            .map(|routine| routine.routine_id.as_str())
            .collect();
        assert_eq!(ids.len(), 2, "two clusters must have distinct ids");
    }

    #[test]
    fn cluster_wrapping_midnight_is_measured_fairly() {
        // 23:50 / 00:10 alternating: a tight cluster across the wrap.
        let days: Vec<MiningDay> = (0..6)
            .map(|k| {
                let minute = if k % 2 == 0 { 1430 } else { 10 };
                day(
                    k,
                    u8::try_from(k % 7).unwrap_or(0),
                    vec![episode(BASE + k * DAY_NS, minute, 9, "backup.exe", None)],
                )
            })
            .collect();
        let result =
            mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("mining succeeds");
        assert_eq!(result.routines.len(), 1, "wrap cluster must promote once");
        let routine = &result.routines[0];
        println!(
            "wrap mean={} tol={} label={}",
            routine.mean_minute_of_day, routine.tolerance_minutes, routine.schedule_label
        );
        assert!(
            routine.mean_minute_of_day >= 1430 || routine.mean_minute_of_day <= 10,
            "mean {} must sit on the midnight wrap",
            routine.mean_minute_of_day
        );
        assert!(routine.tolerance_minutes <= 25);
    }

    #[test]
    fn agent_episodes_are_excluded_by_default() {
        let days: Vec<MiningDay> = (0..5)
            .map(|k| {
                let day_start = BASE + k * DAY_NS;
                let mut agent = episode(day_start, 540, 10, "agentapp.exe", None);
                agent.actor = TimelineActor::Agent {
                    session_id: "sess-1".to_owned(),
                };
                day(k, u8::try_from(k % 7).unwrap_or(0), vec![agent])
            })
            .collect();
        let result =
            mine_routines(&days, BASE, &RoutineMiningConfig::default()).expect("mining succeeds");
        println!(
            "agent_excluded routines={} filtered_agent={} active_days={}",
            result.routines.len(),
            result.filtered_agent_episodes,
            result.active_days
        );
        assert!(result.routines.is_empty());
        assert_eq!(result.filtered_agent_episodes, 5);
        assert_eq!(result.active_days, 0, "agent-only days are not active");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let result = mine_routines(&[], BASE, &RoutineMiningConfig::default())
            .expect("empty input is valid");
        println!("empty routines={}", result.routines.len());
        assert_eq!(result, RoutineMining::default());
    }

    #[test]
    fn structural_errors_are_loud() {
        let config = RoutineMiningConfig::default();
        let mut bad_config = config.clone();
        bad_config.min_support_days = 0;
        let error = mine_routines(&[], BASE, &bad_config).expect_err("bad config");
        assert!(
            error.to_string().contains("ROUTINE_CONFIG_INVALID"),
            "{error}"
        );

        let inverted = MiningDay {
            day_start_ns: BASE + DAY_NS,
            day_end_ns: BASE,
            weekday: 0,
            episodes: Vec::new(),
        };
        let error = mine_routines(&[inverted], BASE, &config).expect_err("inverted day");
        assert!(error.to_string().contains("ROUTINE_DAY_INVALID"), "{error}");

        let bad_weekday = MiningDay {
            day_start_ns: BASE,
            day_end_ns: BASE + DAY_NS,
            weekday: 7,
            episodes: Vec::new(),
        };
        let error = mine_routines(&[bad_weekday], BASE, &config).expect_err("weekday 7");
        assert!(
            error.to_string().contains("ROUTINE_DAY_WEEKDAY_INVALID"),
            "{error}"
        );

        let out_of_day = day(0, 0, vec![episode(BASE + DAY_NS, 10, 10, "x.exe", None)]);
        let error = mine_routines(&[out_of_day], BASE, &config).expect_err("episode outside day");
        assert!(
            error.to_string().contains("ROUTINE_EPISODE_OUTSIDE_DAY"),
            "{error}"
        );

        let disordered_days = vec![day(1, 0, Vec::new()), day(0, 1, Vec::new())];
        let error = mine_routines(&disordered_days, BASE, &config).expect_err("days out of order");
        assert!(
            error.to_string().contains("ROUTINE_DAYS_NOT_CHRONOLOGICAL"),
            "{error}"
        );

        let mut disordered_episodes = day(
            0,
            0,
            vec![
                episode(BASE, 100, 10, "a.exe", None),
                episode(BASE, 50, 10, "b.exe", None),
            ],
        );
        disordered_episodes.episodes[1].start_ts_ns = BASE + 40 * MIN_NS;
        let error = mine_routines(std::slice::from_ref(&disordered_episodes), BASE, &config)
            .expect_err("episodes out of order");
        assert!(
            error
                .to_string()
                .contains("ROUTINE_EPISODES_NOT_CHRONOLOGICAL"),
            "{error}"
        );
    }

    #[test]
    fn caps_are_loud_not_silent() {
        let config = RoutineMiningConfig {
            max_candidates: 3,
            max_occurrences_per_day: 1,
            ..RoutineMiningConfig::default()
        };
        // Many distinct apps per day → candidate cap; repeated pattern in
        // one day → per-day occurrence cap.
        let days: Vec<MiningDay> = (0..3)
            .map(|k| {
                let day_start = BASE + k * DAY_NS;
                day(
                    k,
                    u8::try_from(k % 7).unwrap_or(0),
                    vec![
                        episode(day_start, 100, 5, "a.exe", None),
                        episode(day_start, 110, 5, "b.exe", None),
                        episode(day_start, 120, 5, "a.exe", None),
                        episode(day_start, 130, 5, "b.exe", None),
                        episode(day_start, 140, 5, "c.exe", None),
                    ],
                )
            })
            .collect();
        let result = mine_routines(&days, BASE, &config).expect("mining succeeds");
        println!(
            "caps truncated={} skipped={}",
            result.candidates_truncated, result.occurrences_skipped_over_cap
        );
        assert!(result.candidates_truncated > 0);
        assert!(result.occurrences_skipped_over_cap > 0);
        assert_eq!(result.candidates_evaluated, 3);
    }

    #[test]
    fn wilson_bound_is_honest_at_low_support() {
        assert!((wilson_lower_bound(0, 0) - 0.0).abs() < f64::EPSILON);
        let three_of_three = wilson_lower_bound(3, 3);
        let ten_of_ten = wilson_lower_bound(10, 10);
        let three_of_ten = wilson_lower_bound(3, 10);
        println!("wilson 3/3={three_of_three:.3} 10/10={ten_of_ten:.3} 3/10={three_of_ten:.3}");
        assert!(three_of_three > 0.38 && three_of_three < 0.5);
        assert!(ten_of_ten > 0.7);
        assert!(
            ten_of_ten > three_of_three,
            "more evidence, more confidence"
        );
        assert!(three_of_ten < 0.15);
    }

    #[test]
    fn circular_helpers_handle_wrap() {
        assert_eq!(circular_distance(10, 1430), 20);
        assert_eq!(circular_distance(0, 720), 720);
        assert_eq!(circular_distance(700, 700), 0);
        let mean = circular_mean_minute(&[1430, 10], 1430, 20);
        assert!(mean >= 1430 || mean <= 10, "wrap mean: {mean}");
    }
}
