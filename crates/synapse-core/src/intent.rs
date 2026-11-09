//! Live intent matcher (#853, epic #831/#828).
//!
//! A pure, deterministic function over the recent activity stream and the
//! mined routine library: given the operator's recent episodes and the
//! routines they were mined into, decide which routines the operator appears
//! to be **executing right now** and rank them with evidence.
//!
//! # Why prefix-match
//!
//! A routine is an ordered episode template plus a schedule signature
//! ([`RoutineRecord`]). An operator who has just started a routine has
//! produced its opening steps and nothing past them. So "currently executing
//! routine R" means: the operator's most recent collapsed activity is exactly
//! R's first `k` steps (`k ≥ 1`), the last of those steps is still recent, and
//! the wall-clock falls near R's usual time. The match is anchored at the
//! routine's first step and aligned to the *end* of observed activity — the
//! tail, because that is where "now" is.
//!
//! # Mirroring the miner
//!
//! The observed episodes must be collapsed into steps **exactly** the way the
//! miner collapsed them, or a freshly-performed routine would not match the
//! template it produced. So this module reuses the miner's two shaping rules
//! ([`crate::routines`]): an eligibility floor (episodes shorter than
//! [`IntentMatchConfig::min_episode_duration_ns`], or without an app, are
//! noise) and gap-bounded collapse of consecutive same-identity episodes
//! ([`IntentMatchConfig::collapse_gap_ns`]). Identity is projected to each
//! routine's own granularity before matching, again like the miner.
//!
//! # Clock-free
//!
//! Like the mining and segmentation engines, this function reads no clock and
//! no locale: the caller passes [`NowContext`] (instant, weekday, minute of
//! local day). That makes every match deterministic and replayable (#857).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::routines::MINUTES_PER_DAY;
use crate::types::{
    EpisodeRecord, RoutineDowClass, RoutineGranularity, RoutineLifecycle, RoutineRecord,
    RoutineStep, TimelineActor,
};

/// Tuning knobs for the matcher. Every field is an explicit deterministic
/// input — the engine reads no clock, locale, or environment.
#[derive(Clone, Debug, PartialEq)]
pub struct IntentMatchConfig {
    /// Observed episodes shorter than this are noise, never routine steps.
    /// Mirrors [`crate::routines::RoutineMiningConfig::min_episode_duration_ns`]
    /// so observed activity is filtered the way it was mined.
    pub min_episode_duration_ns: u64,
    /// Consecutive same-identity episodes separated by no more than this merge
    /// into one observed step. Mirrors
    /// [`crate::routines::RoutineMiningConfig::collapse_gap_ns`].
    pub collapse_gap_ns: u64,
    /// The last matched observed step must have ended within this of "now",
    /// or the operator has moved on and is no longer *in* the routine. A
    /// pull-based "current intent" must not resurrect this morning's routine
    /// this evening.
    pub freshness_ns: u64,
    /// Beyond a routine's tolerance, the time-of-day factor decays linearly to
    /// zero over this many minutes of additional circular distance.
    pub schedule_decay_minutes: u32,
    /// Schedule factor multiplier applied when today's weekday is not in the
    /// routine's day-of-week class. A penalty, not an exclusion: an
    /// off-schedule routine can still be underway, just less plausibly.
    pub off_dow_factor: f64,
    /// Candidates whose combined confidence is below this are dropped. The
    /// honest-empty contract (#854): nothing forced to the top.
    pub min_combined_confidence: f64,
    /// Hard cap on returned candidates after ranking.
    pub max_candidates: usize,
    /// Match agent-actor episodes too (default false: human intents only).
    pub include_agent_activity: bool,
}

impl Default for IntentMatchConfig {
    fn default() -> Self {
        Self {
            // The two shaping knobs match RoutineMiningConfig::default().
            min_episode_duration_ns: 60_000_000_000, // 60 s
            collapse_gap_ns: 900_000_000_000,        // 15 min
            // One inter-step gap (RoutineMiningConfig::max_step_gap_ns): once
            // the last step is older than the widest gap a routine tolerates
            // between its own steps, the routine is no longer plausibly live.
            freshness_ns: 1_800_000_000_000, // 30 min
            // Matches RoutineMiningConfig::max_cluster_spread_minutes so the
            // decay tail spans the same arc the miner considered "one cluster".
            schedule_decay_minutes: 180,
            off_dow_factor: 0.3,
            min_combined_confidence: 0.0,
            max_candidates: 10,
            include_agent_activity: false,
        }
    }
}

impl IntentMatchConfig {
    fn validate(&self) -> Result<(), IntentMatchError> {
        if !(0.0..=1.0).contains(&self.off_dow_factor) {
            return Err(IntentMatchError::InvalidConfig {
                detail: format!(
                    "off_dow_factor must be within [0.0, 1.0]; got {}",
                    self.off_dow_factor
                ),
            });
        }
        if !(0.0..=1.0).contains(&self.min_combined_confidence) {
            return Err(IntentMatchError::InvalidConfig {
                detail: format!(
                    "min_combined_confidence must be within [0.0, 1.0]; got {}",
                    self.min_combined_confidence
                ),
            });
        }
        Ok(())
    }
}

/// The wall-clock the match is evaluated against. The caller owns calendar
/// math (local midnight, weekday) so the engine stays clock- and locale-free,
/// exactly like [`crate::routines::MiningDay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NowContext {
    /// Instant the query is "as of" (ns since epoch).
    pub ts_ns: u64,
    /// 0 = Monday … 6 = Sunday for `ts_ns` in local time.
    pub weekday: u8,
    /// Minute of the local day for `ts_ns` (0..1440).
    pub minute_of_day: u32,
}

/// A routine plus the operator lifecycle state matching gates on. Disabled and
/// archived routines never match (#849: a disabled routine is invisible to
/// intent and suggestion surfaces).
#[derive(Clone, Debug, PartialEq)]
pub struct RoutineForMatch {
    pub record: RoutineRecord,
    pub lifecycle: RoutineLifecycle,
    /// Operator display label, if one was set.
    pub label: Option<String>,
}

/// One observed step matched onto a routine template step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MatchedStep {
    /// Index into the routine template (0-based).
    pub step_index: usize,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// Stable id (`ep1-…`) of the first episode of the collapsed observed
    /// step, resolvable via `episode_get`.
    pub episode_id: String,
    pub episode_start_ts_ns: u64,
    /// End of the last episode merged into this collapsed observed step.
    pub episode_end_ts_ns: u64,
}

/// The schedule alignment of a candidate: why its time-of-day factor is what
/// it is, in inspectable terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduleContext {
    pub dow_class: RoutineDowClass,
    pub mean_minute_of_day: u32,
    pub tolerance_minutes: u32,
    pub now_weekday: u8,
    pub now_minute_of_day: u32,
    /// Local minute-of-day the first matched step started at — the routine's
    /// observed start time, which is what the schedule signature describes.
    pub started_minute_of_day: u32,
    /// Whether today's weekday is in the routine's day-of-week class.
    pub dow_match: bool,
    /// Circular distance (minutes) between the observed start and the routine's
    /// mean start minute.
    pub minutes_from_mean: u32,
    /// Whether `minutes_from_mean <= tolerance_minutes`.
    pub within_tolerance: bool,
}

/// One ranked intent candidate.
///
/// The operator appears to be executing this routine right now. Carries
/// evidence (matched steps), a preview (remaining steps), and a decomposed
/// confidence so an agent can justify a suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IntentCandidate {
    pub routine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub schedule_label: String,
    pub lifecycle: RoutineLifecycle,
    pub granularity: RoutineGranularity,
    /// Combined confidence in [0,1]: `routine_confidence * prefix_factor *
    /// schedule_factor`. The ranking key.
    pub confidence: f64,
    /// The routine's own Wilson lower bound (base reliability).
    pub routine_confidence: f64,
    /// How committed the operator is: rises with matched-prefix depth.
    pub prefix_factor: f64,
    /// Time-of-day and day-of-week alignment.
    pub schedule_factor: f64,
    /// Number of leading template steps the observed tail matched.
    pub matched_prefix_len: usize,
    /// Total steps in the routine template.
    pub total_steps: usize,
    /// The matched observed steps, template order.
    pub matched_steps: Vec<MatchedStep>,
    /// The not-yet-observed template steps, in order — what to expect next.
    pub remaining_steps: Vec<RoutineStep>,
    /// End of the most recent matched observed step (freshness anchor).
    pub last_matched_end_ts_ns: u64,
    pub schedule: ScheduleContext,
}

/// Structured engine failures. Every variant names the offending input.
#[derive(Debug, thiserror::Error)]
pub enum IntentMatchError {
    #[error("INTENT_CONFIG_INVALID: {detail}")]
    InvalidConfig { detail: String },
    #[error("INTENT_NOW_INVALID: weekday {weekday} is not in 0..=6")]
    InvalidWeekday { weekday: u8 },
    #[error("INTENT_NOW_INVALID: minute_of_day {minute} is not in 0..{MINUTES_PER_DAY}")]
    InvalidMinute { minute: u32 },
}

/// One collapsed observed step: a run of consecutive same-identity eligible
/// episodes, projected to a routine's granularity.
#[derive(Clone, Debug)]
struct ObservedStep {
    app: String,
    document: Option<String>,
    first_episode_id: String,
    first_start_ts_ns: u64,
    last_end_ts_ns: u64,
}

/// Circular distance between two minutes on the day circle. A standalone copy
/// of the miner's private helper so this engine adds no coupling.
const fn circular_distance(a: u32, b: u32) -> u32 {
    let diff = a.abs_diff(b);
    if diff > MINUTES_PER_DAY / 2 {
        MINUTES_PER_DAY - diff
    } else {
        diff
    }
}

/// Whether `weekday` (0=Mon..6=Sun) is in the routine's day-of-week class.
fn dow_class_contains(class: &RoutineDowClass, weekday: u8) -> bool {
    match class {
        RoutineDowClass::Daily => true,
        RoutineDowClass::Weekdays => weekday <= 4,
        RoutineDowClass::Weekend => weekday >= 5,
        RoutineDowClass::Days { days } => days.contains(&weekday),
    }
}

/// Collapses eligible episodes into observed steps at one granularity,
/// mirroring [`crate::routines`]: identity is lowercased app (+ lowercased
/// document for `AppDocument`), and consecutive same-identity episodes merge
/// only when the pause between them is at most `collapse_gap_ns`.
fn collapse_observed(
    eligible: &[&EpisodeRecord],
    granularity: RoutineGranularity,
    collapse_gap_ns: u64,
) -> Vec<ObservedStep> {
    let mut steps: Vec<ObservedStep> = Vec::new();
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
        if let Some(last) = steps.last_mut()
            && last.app == app
            && last.document == document
            && episode.start_ts_ns.saturating_sub(last.last_end_ts_ns) <= collapse_gap_ns
        {
            last.last_end_ts_ns = last.last_end_ts_ns.max(episode.end_ts_ns);
            continue;
        }
        steps.push(ObservedStep {
            app,
            document,
            first_episode_id: episode.episode_id.clone(),
            first_start_ts_ns: episode.start_ts_ns,
            last_end_ts_ns: episode.end_ts_ns,
        });
    }
    steps
}

/// True when an observed step has the same identity as a routine template step
/// at the granularity the observed steps were collapsed to. After collapse,
/// `App`-granularity observed steps carry `document == None`, so a plain
/// equality on `(app, document)` is exact for both granularities.
fn step_matches(observed: &ObservedStep, template: &RoutineStep) -> bool {
    observed.app == template.app && observed.document == template.document
}

/// The largest `k` (`1..=min(observed, template)`) such that the last `k`
/// observed steps equal the first `k` template steps. `0` means no prefix of
/// the routine is currently underway.
fn maximal_prefix(observed: &[ObservedStep], template: &[RoutineStep]) -> usize {
    let max_k = observed.len().min(template.len());
    for k in (1..=max_k).rev() {
        let tail = &observed[observed.len() - k..];
        if tail
            .iter()
            .zip(&template[..k])
            .all(|(obs, step)| step_matches(obs, step))
        {
            return k;
        }
    }
    0
}

/// Time-of-day factor in `(0, 1]`: 1.0 at the routine's mean start, easing to
/// 0.7 at the tolerance edge, then decaying linearly to 0 over
/// `schedule_decay_minutes` of additional circular distance.
#[allow(clippy::cast_precision_loss)]
fn time_factor(minutes_from_mean: u32, tolerance: u32, decay_minutes: u32) -> f64 {
    let d = f64::from(minutes_from_mean);
    let tol = f64::from(tolerance.max(1));
    if minutes_from_mean <= tolerance {
        // Graded even inside tolerance: closer to the mean reads as a stronger
        // schedule signal. Ranges 1.0 (at the mean) down to 0.7 (at the edge).
        return 0.3f64.mul_add(-(d / tol), 1.0);
    }
    let decay = f64::from(decay_minutes.max(1));
    let beyond = d - f64::from(tolerance);
    (0.7 * (1.0 - beyond / decay)).max(0.0)
}

/// Prefix factor in `[0.5, 1.0]`: 0.5 of a one-step match's plausibility plus
/// 0.5 scaled by how deep into the routine the operator is. Deeper prefix ⇒
/// more committed ⇒ higher; a near-miss that matches fewer steps scores lower.
#[allow(clippy::cast_precision_loss)]
fn prefix_factor(matched: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    0.5f64.mul_add((matched as f64) / (total as f64), 0.5)
}

/// Tries to match one routine against the (already eligibility-filtered,
/// chronological) observed episodes, returning a scored candidate or `None`
/// when the routine is ineligible, no prefix is underway, the last step is
/// stale/future, or the combined confidence is below the floor.
fn try_match_routine(
    eligible: &[&EpisodeRecord],
    routine: &RoutineForMatch,
    now: NowContext,
    config: &IntentMatchConfig,
) -> Option<IntentCandidate> {
    if !matches!(
        routine.lifecycle,
        RoutineLifecycle::Candidate | RoutineLifecycle::Confirmed
    ) {
        return None;
    }
    let template = &routine.record.steps;
    if template.is_empty() {
        return None;
    }

    let observed = collapse_observed(eligible, routine.record.granularity, config.collapse_gap_ns);
    let k = maximal_prefix(&observed, template);
    if k == 0 {
        return None;
    }

    // Freshness: the last matched step must still be recent, and cannot lie in
    // the future relative to the as-of instant.
    let matched_obs = &observed[observed.len() - k..];
    let last_end = matched_obs[k - 1].last_end_ts_ns;
    if last_end > now.ts_ns || now.ts_ns.saturating_sub(last_end) > config.freshness_ns {
        return None;
    }

    // Schedule alignment is about when the routine STARTED, not "now" — by the
    // time a multi-step routine is observed, now is always minutes past its
    // start. Derive the first matched step's local minute-of-day from now's
    // local coordinates plus the elapsed offset (no timezone needed: the
    // freshness gate keeps the start within ~an hour of now, the same local
    // day and UTC offset in the overwhelming common case).
    let first_start = matched_obs[0].first_start_ts_ns;
    let elapsed_min = now.ts_ns.saturating_sub(first_start) / 60_000_000_000;
    let elapsed_mod = u32::try_from(elapsed_min % u64::from(MINUTES_PER_DAY)).unwrap_or(0);
    let started_minute_of_day =
        (now.minute_of_day + MINUTES_PER_DAY - elapsed_mod) % MINUTES_PER_DAY;

    let dow_match = dow_class_contains(&routine.record.dow_class, now.weekday);
    let minutes_from_mean =
        circular_distance(started_minute_of_day, routine.record.mean_minute_of_day);
    let within_tolerance = minutes_from_mean <= routine.record.tolerance_minutes;
    let dow_factor = if dow_match {
        1.0
    } else {
        config.off_dow_factor
    };
    let schedule_factor = dow_factor
        * time_factor(
            minutes_from_mean,
            routine.record.tolerance_minutes,
            config.schedule_decay_minutes,
        );
    let prefix = prefix_factor(k, template.len());
    let confidence = (routine.record.confidence * prefix * schedule_factor).clamp(0.0, 1.0);
    if confidence < config.min_combined_confidence {
        return None;
    }

    let matched_steps = matched_obs
        .iter()
        .enumerate()
        .map(|(index, obs)| MatchedStep {
            step_index: index,
            app: obs.app.clone(),
            document: obs.document.clone(),
            episode_id: obs.first_episode_id.clone(),
            episode_start_ts_ns: obs.first_start_ts_ns,
            episode_end_ts_ns: obs.last_end_ts_ns,
        })
        .collect();

    Some(IntentCandidate {
        routine_id: routine.record.routine_id.clone(),
        label: routine.label.clone(),
        schedule_label: routine.record.schedule_label.clone(),
        lifecycle: routine.lifecycle,
        granularity: routine.record.granularity,
        confidence,
        routine_confidence: routine.record.confidence,
        prefix_factor: prefix,
        schedule_factor,
        matched_prefix_len: k,
        total_steps: template.len(),
        matched_steps,
        remaining_steps: template[k..].to_vec(),
        last_matched_end_ts_ns: last_end,
        schedule: ScheduleContext {
            dow_class: routine.record.dow_class.clone(),
            mean_minute_of_day: routine.record.mean_minute_of_day,
            tolerance_minutes: routine.record.tolerance_minutes,
            now_weekday: now.weekday,
            now_minute_of_day: now.minute_of_day,
            started_minute_of_day,
            dow_match,
            minutes_from_mean,
            within_tolerance,
        },
    })
}

/// Matches the recent activity stream against the routine library and returns
/// ranked intent candidates. Pure and deterministic: same inputs ⇒ same
/// output, byte for byte.
///
/// `episodes` need not be sorted; the engine sorts a working copy by
/// `(start_ts_ns, episode_id)`. Disabled/archived routines and routines with
/// no steps are skipped. An empty result is the honest "nothing matches".
///
/// # Errors
///
/// Returns [`IntentMatchError`] if the config or [`NowContext`] is out of
/// range — never a silent clamp.
pub fn match_intents(
    episodes: &[EpisodeRecord],
    routines: &[RoutineForMatch],
    now: NowContext,
    config: &IntentMatchConfig,
) -> Result<Vec<IntentCandidate>, IntentMatchError> {
    config.validate()?;
    if now.weekday > 6 {
        return Err(IntentMatchError::InvalidWeekday {
            weekday: now.weekday,
        });
    }
    if now.minute_of_day >= MINUTES_PER_DAY {
        return Err(IntentMatchError::InvalidMinute {
            minute: now.minute_of_day,
        });
    }

    // Eligibility filter, mirroring the miner: actor, duration floor, has-app.
    // Sorted so collapse sees true chronological order regardless of caller.
    let mut eligible: Vec<&EpisodeRecord> = episodes
        .iter()
        .filter(|episode| {
            (config.include_agent_activity || matches!(episode.actor, TimelineActor::Human))
                && episode.app.as_deref().is_some_and(|app| !app.is_empty())
                && episode.end_ts_ns.saturating_sub(episode.start_ts_ns)
                    >= config.min_episode_duration_ns
        })
        .collect();
    eligible.sort_by(|a, b| {
        a.start_ts_ns
            .cmp(&b.start_ts_ns)
            .then_with(|| a.episode_id.cmp(&b.episode_id))
    });

    let mut candidates: Vec<IntentCandidate> = Vec::new();
    for routine in routines {
        if let Some(candidate) = try_match_routine(&eligible, routine, now, config) {
            candidates.push(candidate);
        }
    }

    // Strongest first; deeper prefix breaks ties; routine id is the
    // deterministic final tiebreaker.
    candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.matched_prefix_len.cmp(&a.matched_prefix_len))
            .then_with(|| a.routine_id.cmp(&b.routine_id))
    });
    candidates.truncate(config.max_candidates);
    Ok(candidates)
}
