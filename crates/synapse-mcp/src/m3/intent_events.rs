//! Intent transition events on the event bus (#855, epic #831/#828).
//!
//! The push-based twin of `intent_current` (#854). Where `intent_current` is a
//! pull-based now-snapshot, this module turns the *changes* between snapshots
//! into first-class events on the shared event bus, so subscriptions
//! (`subscribe` / `observe_delta`) and `on_event` reflexes consume intent
//! transitions uniformly with every other event — and, downstream, the
//! suggestion engine (#858) and the intent feedback loop (#856) get a single
//! authoritative signal stream.
//!
//! # The three transitions
//!
//! A tracked routine moves through a tiny, deterministic state machine:
//!
//! * **`intent-detected`** — a routine first appears as a live candidate
//!   (its recent prefix matched, within the freshness window, above the
//!   detection floor). This is the trigger to *consider* a suggestion.
//! * **`intent-confirmed`** — a detected routine reaches full completion
//!   (every template step observed). The operator actually did the whole
//!   thing: the prediction was right.
//! * **`intent-abandoned`** — a detected routine drops out of the live set
//!   before completing (the operator diverged, or the last matched step went
//!   stale). This is the signal that ends a pending suggestion (#858).
//!
//! A routine that *completes* and then leaves the live set is **not**
//! abandoned — it was already confirmed. Only an as-yet-uncompleted detection
//! that disappears is an abandonment.
//!
//! # Pure core, two drivers
//!
//! [`IntentTracker::reconcile`] is a pure function of the previous tracker
//! state and the current ranked candidate list — no clock, storage, or bus —
//! so it is exhaustively unit-testable. Two thin shells drive it against real
//! snapshots:
//!
//! * [`spawn_intent_detector`] — a periodic in-daemon job (mirrors
//!   [`super::routine_miner_job`]) that ticks on a fixed interval using the
//!   host clock. This is what makes intent detection *live*.
//! * the `intent_detect_tick` MCP tool — forces one tick on demand (and
//!   accepts a replay `now_ts_ns`), the deterministic seam agents, dashboards,
//!   and manual FSV use to drive detection at a known instant.
//!
//! Both share a single [`SharedIntentTracker`] held in `M3State`, so the state
//! machine has exactly one source of truth regardless of which driver advances
//! it.
//!
//! # Failure policy
//!
//! A failed tick is logged loudly (`INTENT_DETECT_PERIODIC_FAILED`) and the
//! periodic job keeps its schedule — one bad storage read must not stop future
//! detection — but the failure is never swallowed, and the tracker is left
//! intact (a transient read error must not spuriously abandon every live
//! intent). A misconfigured schedule is a startup error, not a silent default.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use synapse_core::intent::IntentCandidate;
use synapse_core::types::RoutineLifecycle;
use synapse_core::{Event, EventSource, error_codes};
use synapse_reflex::EventBus;
use synapse_storage::Db;
use tokio_util::sync::CancellationToken;

use super::M3State;
use super::M3ToolStub;
use super::intent::{DEFAULT_LOOKBACK_HOURS, IntentCurrentParams, current_intents};
use super::permissions::{Permission, RequiredPermissions, required};
use crate::m1::mcp_error;

/// Event kind: a routine became a live candidate this tick.
pub const INTENT_DETECTED_KIND: &str = "intent-detected";
/// Event kind: a detected routine reached full completion.
pub const INTENT_CONFIRMED_KIND: &str = "intent-confirmed";
/// Event kind: a detected routine left the live set before completing.
pub const INTENT_ABANDONED_KIND: &str = "intent-abandoned";

/// Environment variable: seconds between periodic detection ticks.
pub const INTERVAL_ENV: &str = "SYNAPSE_INTENT_DETECT_INTERVAL_SECS";
/// Environment variable: seconds before the first tick.
pub const STARTUP_DELAY_ENV: &str = "SYNAPSE_INTENT_DETECT_STARTUP_DELAY_SECS";
/// Environment variable: detection confidence floor (`[0.0, 1.0]`).
pub const MIN_CONFIDENCE_ENV: &str = "SYNAPSE_INTENT_DETECT_MIN_CONFIDENCE";
/// Environment variable: recent-activity lookback in hours.
pub const LOOKBACK_ENV: &str = "SYNAPSE_INTENT_DETECT_LOOKBACK_HOURS";

/// Default interval: 60 s. Short enough that a routine's first step is detected
/// well inside the matcher's 30-min freshness window, cheap enough to run on a
/// laptop (one bounded CF scan per tick).
pub const DEFAULT_INTERVAL_SECS: u64 = 60;
/// Default startup delay: 45 s, so the first tick lands after the recorder has
/// warmed up and never on the daemon's cold-start spike.
pub const DEFAULT_STARTUP_DELAY_SECS: u64 = 45;
/// Default detection floor. Below this an honest match is too weak to surface
/// as an intent — it would be noise to the suggestion engine.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.30;

/// Per-publisher monotonic sequence for intent events, mirroring the
/// profile-transition publisher in `server::context`.
static NEXT_INTENT_EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Which transition fired. Serializes to the matching event kind string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IntentTransitionKind {
    Detected,
    Confirmed,
    Abandoned,
}

impl IntentTransitionKind {
    /// The event-bus `kind` string this transition publishes under.
    #[must_use]
    pub const fn event_kind(self) -> &'static str {
        match self {
            Self::Detected => INTENT_DETECTED_KIND,
            Self::Confirmed => INTENT_CONFIRMED_KIND,
            Self::Abandoned => INTENT_ABANDONED_KIND,
        }
    }

    /// A short machine-stable reason, carried in the event payload, explaining
    /// why the transition fired.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Detected => "prefix_match",
            Self::Confirmed => "all_steps_completed",
            Self::Abandoned => "diverged_or_stale",
        }
    }
}

/// One published transition. Carries enough decomposed evidence for a consumer
/// (suggestion engine, feedback loop, a human) to act without re-querying.
///
/// Response/event-payload shape only (no `Deserialize`): `reason` is a
/// `&'static str` drawn from [`IntentTransitionKind::reason`].
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntentTransition {
    pub kind: IntentTransitionKind,
    pub routine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub schedule_label: String,
    pub lifecycle: RoutineLifecycle,
    /// Combined confidence at the moment of the transition (last-known value
    /// for an abandonment, since the candidate is no longer live).
    pub confidence: f64,
    pub matched_prefix_len: usize,
    pub total_steps: usize,
    /// Machine-stable reason ([`IntentTransitionKind::reason`]).
    pub reason: &'static str,
}

/// Where in its lifecycle a tracked intent is. `Detected` intents can still be
/// confirmed or abandoned; `Confirmed` intents only leave silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntentPhase {
    Detected,
    Confirmed,
}

/// The last-known state of one live intent, kept between ticks so a snapshot
/// diff can decide what changed.
#[derive(Clone, Debug, PartialEq)]
struct TrackedIntent {
    phase: IntentPhase,
    label: Option<String>,
    schedule_label: String,
    lifecycle: RoutineLifecycle,
    confidence: f64,
    matched_prefix_len: usize,
    total_steps: usize,
}

impl TrackedIntent {
    fn from_candidate(candidate: &IntentCandidate, phase: IntentPhase) -> Self {
        Self {
            phase,
            label: candidate.label.clone(),
            schedule_label: candidate.schedule_label.clone(),
            lifecycle: candidate.lifecycle,
            confidence: candidate.confidence,
            matched_prefix_len: candidate.matched_prefix_len,
            total_steps: candidate.total_steps,
        }
    }
}

/// True once every template step has been observed.
fn is_complete(candidate: &IntentCandidate) -> bool {
    candidate.total_steps > 0 && candidate.matched_prefix_len >= candidate.total_steps
}

fn detected_transition(candidate: &IntentCandidate) -> IntentTransition {
    transition_from_candidate(IntentTransitionKind::Detected, candidate)
}

fn confirmed_transition(candidate: &IntentCandidate) -> IntentTransition {
    transition_from_candidate(IntentTransitionKind::Confirmed, candidate)
}

fn transition_from_candidate(
    kind: IntentTransitionKind,
    candidate: &IntentCandidate,
) -> IntentTransition {
    IntentTransition {
        kind,
        routine_id: candidate.routine_id.clone(),
        label: candidate.label.clone(),
        schedule_label: candidate.schedule_label.clone(),
        lifecycle: candidate.lifecycle,
        confidence: candidate.confidence,
        matched_prefix_len: candidate.matched_prefix_len,
        total_steps: candidate.total_steps,
        reason: kind.reason(),
    }
}

fn abandoned_transition(routine_id: &str, tracked: &TrackedIntent) -> IntentTransition {
    IntentTransition {
        kind: IntentTransitionKind::Abandoned,
        routine_id: routine_id.to_owned(),
        label: tracked.label.clone(),
        schedule_label: tracked.schedule_label.clone(),
        lifecycle: tracked.lifecycle,
        confidence: tracked.confidence,
        matched_prefix_len: tracked.matched_prefix_len,
        total_steps: tracked.total_steps,
        reason: IntentTransitionKind::Abandoned.reason(),
    }
}

/// The intent state machine: a `routine_id -> TrackedIntent` map advanced by
/// snapshot diffs. Held behind a [`SharedIntentTracker`] so the periodic job
/// and the on-demand tool advance one shared state.
#[derive(Debug, Default)]
pub struct IntentTracker {
    tracked: BTreeMap<String, TrackedIntent>,
}

impl IntentTracker {
    /// Number of currently-tracked live intents.
    #[must_use]
    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    /// Diffs `candidates` (the current ranked live set, already filtered to the
    /// detection floor by the matcher) against the tracked state and returns
    /// the transitions that fired, advancing the tracker in place.
    ///
    /// Determinism: detected/confirmed transitions are emitted in candidate
    /// rank order (strongest first); abandonments follow in `routine_id` order
    /// (the `BTreeMap` key order). Same inputs ⇒ same transition sequence.
    pub fn reconcile(&mut self, candidates: &[IntentCandidate]) -> Vec<IntentTransition> {
        let mut transitions = Vec::new();
        let mut live: BTreeSet<&str> = BTreeSet::new();

        for candidate in candidates {
            // Defensive against a duplicate routine_id within one snapshot: the
            // matcher emits one candidate per routine, but treat a repeat as an
            // update of the just-inserted entry, never a second detection.
            live.insert(candidate.routine_id.as_str());
            let complete = is_complete(candidate);
            match self.tracked.get_mut(&candidate.routine_id) {
                None => {
                    transitions.push(detected_transition(candidate));
                    let phase = if complete {
                        transitions.push(confirmed_transition(candidate));
                        IntentPhase::Confirmed
                    } else {
                        IntentPhase::Detected
                    };
                    self.tracked.insert(
                        candidate.routine_id.clone(),
                        TrackedIntent::from_candidate(candidate, phase),
                    );
                }
                Some(existing) => {
                    if existing.phase == IntentPhase::Detected && complete {
                        transitions.push(confirmed_transition(candidate));
                        existing.phase = IntentPhase::Confirmed;
                    }
                    existing.label.clone_from(&candidate.label);
                    existing
                        .schedule_label
                        .clone_from(&candidate.schedule_label);
                    existing.lifecycle = candidate.lifecycle;
                    existing.confidence = candidate.confidence;
                    existing.matched_prefix_len = candidate.matched_prefix_len;
                    existing.total_steps = candidate.total_steps;
                }
            }
        }

        // Anything tracked but no longer live: a still-`Detected` intent was
        // abandoned; a `Confirmed` one already completed and leaves silently.
        let gone: Vec<String> = self
            .tracked
            .keys()
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            // `id` came from this map's own key iteration, so the remove always
            // yields a value; `if let` keeps the pure core panic-free regardless.
            if let Some(tracked) = self.tracked.remove(&id)
                && tracked.phase == IntentPhase::Detected
            {
                transitions.push(abandoned_transition(&id, &tracked));
            }
        }

        transitions
    }
}

/// A tracker shared between the periodic detector and the on-demand tool.
pub type SharedIntentTracker = Arc<Mutex<IntentTracker>>;

/// Builds a fresh shared tracker.
#[must_use]
pub fn new_shared_tracker() -> SharedIntentTracker {
    Arc::new(Mutex::new(IntentTracker::default()))
}

/// Resolved detection knobs for one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntentDetectConfig {
    pub min_confidence: f64,
    pub lookback_hours: u32,
    pub max_candidates: u32,
}

impl Default for IntentDetectConfig {
    fn default() -> Self {
        Self {
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            lookback_hours: DEFAULT_LOOKBACK_HOURS,
            // Track the whole honest live set, not just the top few, so a
            // lower-ranked routine never flickers detected/abandoned as ranking
            // shuffles near the truncation edge.
            max_candidates: super::intent::MAX_MAX_CANDIDATES,
        }
    }
}

/// What one detection tick did, for logging and the tool response.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntentDetectOutcome {
    /// Instant the snapshot was evaluated at (echoed for auditability/replay).
    pub now_ts_ns: u64,
    /// Live candidates this tick (after the detection floor).
    pub candidates: u32,
    /// Tracked intents after the reconcile.
    pub tracked: u32,
    /// Transitions published this tick, in publish order.
    pub transitions: Vec<IntentTransition>,
    /// Events accepted for publication.
    pub events_published: u32,
    /// Total subscriber deliveries across all published events.
    pub events_matched_subscribers: u32,
    /// Total events dropped for slow subscribers across all published events.
    pub events_dropped: u64,
}

/// Builds the event-bus `Event` for one transition. Mirrors the
/// profile-transition publisher: per-publisher seq, host wall-clock, `System`
/// source, hyphenated kind, JSON payload, no correlations.
fn transition_event(transition: &IntentTransition) -> Event {
    Event {
        seq: NEXT_INTENT_EVENT_SEQ.fetch_add(1, Ordering::Relaxed),
        at: Utc::now(),
        source: EventSource::System,
        kind: transition.kind.event_kind().to_owned(),
        data: serde_json::json!({
            "routine_id": transition.routine_id,
            "label": transition.label,
            "schedule_label": transition.schedule_label,
            "lifecycle": transition.lifecycle,
            "confidence": transition.confidence,
            "matched_prefix_len": transition.matched_prefix_len,
            "total_steps": transition.total_steps,
            "reason": transition.reason,
        }),
        correlations: Vec::new(),
    }
}

/// Publishes every transition to the bus, returning aggregate delivery counts.
fn publish_transitions(event_bus: &EventBus, transitions: &[IntentTransition]) -> (u32, u64) {
    let mut matched = 0_u32;
    let mut dropped = 0_u64;
    for transition in transitions {
        let report = event_bus.publish(transition_event(transition));
        matched = matched.saturating_add(u32::try_from(report.matched).unwrap_or(u32::MAX));
        dropped = dropped.saturating_add(report.dropped);
        tracing::info!(
            code = "INTENT_TRANSITION_PUBLISHED",
            kind = transition.kind.event_kind(),
            routine_id = transition.routine_id,
            confidence = transition.confidence,
            matched_prefix_len = transition.matched_prefix_len,
            total_steps = transition.total_steps,
            reason = transition.reason,
            matched = report.matched,
            queued = report.queued,
            dropped = report.dropped,
            "intent transition published"
        );
    }
    (matched, dropped)
}

/// Runs one detection tick: snapshot → reconcile (under the shared tracker
/// lock) → publish. The single reusable core both drivers call.
///
/// # Errors
///
/// Propagates `current_intents` failures (undecodable derived rows, scan-budget
/// exhaustion, invalid `now_ts_ns`) and a poisoned tracker lock — never a
/// silently-empty tick.
pub fn detect_and_publish(
    db: &Arc<Db>,
    event_bus: &EventBus,
    tracker: &SharedIntentTracker,
    config: IntentDetectConfig,
    now_ts_ns: Option<u64>,
) -> Result<IntentDetectOutcome, ErrorData> {
    let params = IntentCurrentParams {
        now_ts_ns,
        lookback_hours: Some(config.lookback_hours),
        min_confidence: Some(config.min_confidence),
        max_candidates: Some(config.max_candidates),
        include_agent_activity: false,
    };
    let snapshot = current_intents(db, &params)?;
    let transitions = {
        let mut guard = tracker.lock().map_err(|_poisoned| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "INTENT_TRACKER_POISONED: the intent tracker lock was poisoned by a panic; \
                 restart the daemon",
            )
        })?;
        guard.reconcile(&snapshot.candidates)
    };
    let (matched, dropped) = publish_transitions(event_bus, &transitions);
    let tracked = {
        let guard = tracker.lock().map_err(|_poisoned| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "INTENT_TRACKER_POISONED: the intent tracker lock was poisoned by a panic; \
                 restart the daemon",
            )
        })?;
        u32::try_from(guard.tracked_count()).unwrap_or(u32::MAX)
    };
    Ok(IntentDetectOutcome {
        now_ts_ns: snapshot.now.ts_ns,
        candidates: u32::try_from(snapshot.candidates.len()).unwrap_or(u32::MAX),
        tracked,
        events_published: u32::try_from(transitions.len()).unwrap_or(u32::MAX),
        events_matched_subscribers: matched,
        events_dropped: dropped,
        transitions,
    })
}

/// Parameters for the `intent_detect_tick` MCP tool. Every field is optional
/// and falls back to the periodic detector's default; out-of-range values are a
/// loud error (validated downstream by `current_intents`), never a silent clamp.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntentDetectTickParams {
    /// "As of" instant (ns since epoch) to evaluate at. Defaults to now; pass
    /// an explicit instant to drive detection at a known moment (replay/manual FSV).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ts_ns: Option<u64>,
    /// Detection confidence floor (`[0.0, 1.0]`). Defaults to the periodic
    /// detector floor so on-demand and live ticks agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_confidence: Option<f64>,
    /// Recent-activity lookback in hours (`1..=168`). Defaults to the detector
    /// lookback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_hours: Option<u32>,
}

/// Tool stub for `intent_detect_tick` (the `M3ToolStub` surface registry).
#[must_use]
pub const fn intent_detect_tick() -> M3ToolStub {
    M3ToolStub::new("intent_detect_tick")
}

/// Permissions for `intent_detect_tick`: it reads the durable activity and
/// routine stores to compute the snapshot, exactly like `intent_current`.
#[must_use]
pub fn required_permissions_detect_tick(_params: &IntentDetectTickParams) -> RequiredPermissions {
    required([Permission::ReadStorage])
}

/// Resolves tool params into a detection config (filling detector defaults) and
/// runs one tick against the shared tracker, publishing any transitions.
///
/// # Errors
///
/// Propagates `current_intents` validation/scan failures and a poisoned tracker
/// lock — never a silently-empty tick.
pub fn detect_tick(
    db: &Arc<Db>,
    event_bus: &EventBus,
    tracker: &SharedIntentTracker,
    params: &IntentDetectTickParams,
) -> Result<IntentDetectOutcome, ErrorData> {
    let defaults = IntentDetectConfig::default();
    let config = IntentDetectConfig {
        min_confidence: params.min_confidence.unwrap_or(defaults.min_confidence),
        lookback_hours: params.lookback_hours.unwrap_or(defaults.lookback_hours),
        max_candidates: defaults.max_candidates,
    };
    detect_and_publish(db, event_bus, tracker, config, params.now_ts_ns)
}

fn parse_secs_env(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => anyhow::bail!("{name} is not valid unicode: {error}"),
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(default)
            } else {
                trimmed.parse::<u64>().map_err(|error| {
                    anyhow::anyhow!(
                        "{name} must be an unsigned integer of seconds; got {value:?}: {error}"
                    )
                })
            }
        }
    }
}

/// Pure validation for the detection floor: empty/absent → default, otherwise a
/// float strictly within `[0.0, 1.0]` — out of range is a loud error.
fn parse_min_confidence(value: Option<&str>, default: f64) -> anyhow::Result<f64> {
    let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    let parsed = trimmed.parse::<f64>().map_err(|error| {
        anyhow::anyhow!(
            "{MIN_CONFIDENCE_ENV} must be a float in [0.0, 1.0]; got {trimmed:?}: {error}"
        )
    })?;
    if !(0.0..=1.0).contains(&parsed) {
        anyhow::bail!("{MIN_CONFIDENCE_ENV} must be within [0.0, 1.0]; got {parsed}");
    }
    Ok(parsed)
}

/// Pure validation for the lookback window: empty/absent → default, otherwise an
/// integer hours within `1..=MAX_LOOKBACK_HOURS` — out of range is a loud error.
fn parse_lookback(value: Option<&str>, default: u32) -> anyhow::Result<u32> {
    let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    let parsed = trimmed.parse::<u32>().map_err(|error| {
        anyhow::anyhow!(
            "{LOOKBACK_ENV} must be an unsigned integer of hours; got {trimmed:?}: {error}"
        )
    })?;
    if !(1..=super::intent::MAX_LOOKBACK_HOURS).contains(&parsed) {
        anyhow::bail!(
            "{LOOKBACK_ENV} must be within 1..={}; got {parsed}",
            super::intent::MAX_LOOKBACK_HOURS
        );
    }
    Ok(parsed)
}

fn env_value(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => anyhow::bail!("{name} is not valid unicode: {error}"),
    }
}

/// Reads the detection config from the environment. Invalid values are a
/// startup error, never a silently substituted default.
fn detect_config_from_env() -> anyhow::Result<IntentDetectConfig> {
    Ok(IntentDetectConfig {
        min_confidence: parse_min_confidence(
            env_value(MIN_CONFIDENCE_ENV)?.as_deref(),
            DEFAULT_MIN_CONFIDENCE,
        )?,
        lookback_hours: parse_lookback(
            env_value(LOOKBACK_ENV)?.as_deref(),
            DEFAULT_LOOKBACK_HOURS,
        )?,
        max_candidates: IntentDetectConfig::default().max_candidates,
    })
}

/// Spawns the periodic intent detector. Returns `Ok(None)` when disabled by
/// configuration (interval `0`); the decision is logged either way.
///
/// # Errors
///
/// Returns an error when an environment override is present but unparseable, or
/// out of range — a misconfigured daemon must fail at startup, not run with a
/// silently substituted schedule or detection floor.
pub fn spawn_intent_detector(
    m3_state: Arc<Mutex<M3State>>,
    cancel: CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = parse_secs_env(INTERVAL_ENV, DEFAULT_INTERVAL_SECS)?;
    let startup_delay_secs = parse_secs_env(STARTUP_DELAY_ENV, DEFAULT_STARTUP_DELAY_SECS)?;
    let config = detect_config_from_env()?;
    if interval_secs == 0 {
        tracing::info!(
            code = "INTENT_DETECT_PERIODIC_DISABLED",
            "periodic intent detection disabled via {INTERVAL_ENV}=0"
        );
        return Ok(None);
    }
    tracing::info!(
        code = "INTENT_DETECT_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        min_confidence = config.min_confidence,
        lookback_hours = config.lookback_hours,
        "periodic intent detection scheduled"
    );
    let handle = tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "INTENT_DETECT_PERIODIC_STOPPED",
                        "periodic intent detection stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            run_once(&m3_state, config);
            delay = std::time::Duration::from_secs(interval_secs);
        }
    });
    Ok(Some(handle))
}

/// One periodic detection tick against the live store and shared tracker.
fn run_once(m3_state: &Arc<Mutex<M3State>>, config: IntentDetectConfig) {
    // Acquire the db, event bus, and shared tracker under a brief lock, then
    // release it before the (bounded but non-trivial) storage scan — exactly as
    // the routine miner does, so a tick never blocks tool calls.
    let acquired = {
        let mut state = match m3_state.lock() {
            Ok(state) => state,
            Err(_poisoned) => {
                tracing::error!(
                    code = "INTENT_DETECT_PERIODIC_FAILED",
                    detail = "m3 state lock poisoned",
                    "periodic intent detection could not access state"
                );
                return;
            }
        };
        let event_bus = state.sse_state.event_bus();
        let tracker = state.intent_tracker();
        match state.ensure_storage() {
            Ok(db) => Some((db, event_bus, tracker)),
            Err(error) => {
                tracing::error!(
                    code = "INTENT_DETECT_PERIODIC_FAILED",
                    detail = %error,
                    "periodic intent detection could not open storage"
                );
                None
            }
        }
    };
    let Some((db, event_bus, tracker)) = acquired else {
        return;
    };
    match detect_and_publish(&db, &event_bus, &tracker, config, None) {
        Ok(outcome) => {
            tracing::info!(
                code = "INTENT_DETECT_PERIODIC_OK",
                now_ts_ns = outcome.now_ts_ns,
                candidates = outcome.candidates,
                tracked = outcome.tracked,
                transitions = outcome.events_published,
                matched = outcome.events_matched_subscribers,
                dropped = outcome.events_dropped,
                "periodic intent detection completed"
            );
        }
        Err(error) => {
            tracing::error!(
                code = "INTENT_DETECT_PERIODIC_FAILED",
                error_code = %error.code.0,
                detail = %error.message,
                "periodic intent detection failed; next tick keeps the schedule"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_core::intent::ScheduleContext;
    use synapse_core::types::{RoutineDowClass, RoutineGranularity, RoutineStep};

    /// A minimal candidate for a routine with `total` steps, `matched` of them
    /// observed, at the given confidence. The non-diffed fields (schedule
    /// context, granularity) are fixed: the state machine ignores them.
    fn candidate(
        routine_id: &str,
        matched: usize,
        total: usize,
        confidence: f64,
    ) -> IntentCandidate {
        IntentCandidate {
            routine_id: routine_id.to_owned(),
            label: None,
            schedule_label: "test".to_owned(),
            lifecycle: RoutineLifecycle::Confirmed,
            granularity: RoutineGranularity::App,
            confidence,
            routine_confidence: confidence,
            prefix_factor: 1.0,
            schedule_factor: 1.0,
            matched_prefix_len: matched,
            total_steps: total,
            matched_steps: Vec::new(),
            remaining_steps: vec![
                RoutineStep {
                    app: "x".to_owned(),
                    document: None
                };
                total - matched
            ],
            last_matched_end_ts_ns: 0,
            schedule: ScheduleContext {
                dow_class: RoutineDowClass::Daily,
                mean_minute_of_day: 0,
                tolerance_minutes: 0,
                now_weekday: 0,
                now_minute_of_day: 0,
                started_minute_of_day: 0,
                dow_match: true,
                minutes_from_mean: 0,
                within_tolerance: true,
            },
        }
    }

    fn kinds(transitions: &[IntentTransition]) -> Vec<IntentTransitionKind> {
        transitions.iter().map(|t| t.kind).collect()
    }

    #[test]
    fn first_partial_match_emits_detected_only() {
        let mut tracker = IntentTracker::default();
        let out = tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.8)]);
        assert_eq!(kinds(&out), vec![IntentTransitionKind::Detected]);
        assert_eq!(out[0].routine_id, "rt1-a");
        assert_eq!(out[0].reason, "prefix_match");
        assert_eq!(tracker.tracked_count(), 1);
    }

    #[test]
    fn steady_partial_match_is_idle_no_event() {
        let mut tracker = IntentTracker::default();
        let _ = tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.8)]);
        // Same prefix depth next tick (confidence drift only): nothing fires.
        let out = tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.7)]);
        assert!(
            out.is_empty(),
            "an unchanged detection must not re-fire: {out:?}"
        );
        assert_eq!(tracker.tracked_count(), 1);
    }

    #[test]
    fn deepening_then_completion_emits_confirmed_once() {
        let mut tracker = IntentTracker::default();
        assert_eq!(
            kinds(&tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.8)])),
            vec![IntentTransitionKind::Detected]
        );
        // Deepens but not complete: no event.
        assert!(
            tracker
                .reconcile(&[candidate("rt1-a", 2, 3, 0.8)])
                .is_empty()
        );
        // Completes: confirmed fires once.
        let confirmed = tracker.reconcile(&[candidate("rt1-a", 3, 3, 0.8)]);
        assert_eq!(kinds(&confirmed), vec![IntentTransitionKind::Confirmed]);
        assert_eq!(confirmed[0].reason, "all_steps_completed");
        // Staying complete does not re-confirm.
        assert!(
            tracker
                .reconcile(&[candidate("rt1-a", 3, 3, 0.8)])
                .is_empty()
        );
    }

    #[test]
    fn single_tick_full_match_emits_detected_then_confirmed() {
        let mut tracker = IntentTracker::default();
        let out = tracker.reconcile(&[candidate("rt1-a", 2, 2, 0.9)]);
        assert_eq!(
            kinds(&out),
            vec![
                IntentTransitionKind::Detected,
                IntentTransitionKind::Confirmed
            ]
        );
    }

    #[test]
    fn detected_then_disappears_is_abandoned() {
        let mut tracker = IntentTracker::default();
        let _ = tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.8)]);
        // Operator diverged: the candidate is gone from the live set.
        let out = tracker.reconcile(&[]);
        assert_eq!(kinds(&out), vec![IntentTransitionKind::Abandoned]);
        assert_eq!(out[0].routine_id, "rt1-a");
        assert_eq!(out[0].reason, "diverged_or_stale");
        // Carries the last-known evidence, not zeroes.
        assert_eq!(out[0].matched_prefix_len, 1);
        assert_eq!(out[0].total_steps, 3);
        assert!((out[0].confidence - 0.8).abs() < 1e-9);
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn confirmed_then_disappears_is_silent_not_abandoned() {
        let mut tracker = IntentTracker::default();
        let _ = tracker.reconcile(&[candidate("rt1-a", 2, 2, 0.9)]); // detected + confirmed
        let out = tracker.reconcile(&[]);
        assert!(
            out.is_empty(),
            "a completed routine that leaves the live set is not abandoned: {out:?}"
        );
        assert_eq!(tracker.tracked_count(), 0);
    }

    #[test]
    fn reabandoned_routine_can_be_detected_again() {
        let mut tracker = IntentTracker::default();
        let _ = tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.8)]);
        let _ = tracker.reconcile(&[]); // abandoned, removed
        let out = tracker.reconcile(&[candidate("rt1-a", 1, 3, 0.8)]);
        assert_eq!(kinds(&out), vec![IntentTransitionKind::Detected]);
    }

    #[test]
    fn mixed_tick_orders_detected_by_rank_then_abandoned_by_id() {
        let mut tracker = IntentTracker::default();
        // Two live intents detected first.
        let _ = tracker.reconcile(&[candidate("rt1-old", 1, 3, 0.8)]);
        // Next tick: rt1-old gone (abandon), two new ones detected in rank order
        // (rt1-bbb stronger than rt1-aaa), so detected order follows the slice.
        let out = tracker.reconcile(&[
            candidate("rt1-bbb", 1, 2, 0.9),
            candidate("rt1-aaa", 1, 2, 0.7),
        ]);
        assert_eq!(
            out.iter()
                .map(|t| (t.kind, t.routine_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (IntentTransitionKind::Detected, "rt1-bbb"),
                (IntentTransitionKind::Detected, "rt1-aaa"),
                (IntentTransitionKind::Abandoned, "rt1-old"),
            ]
        );
    }

    #[test]
    fn min_confidence_parser_defaults_accepts_and_rejects_out_of_range() {
        assert!((parse_min_confidence(None, 0.3).expect("absent -> default") - 0.3).abs() < 1e-9);
        assert!(
            (parse_min_confidence(Some("  "), 0.3).expect("blank -> default") - 0.3).abs() < 1e-9
        );
        assert!((parse_min_confidence(Some("0.75"), 0.3).expect("valid") - 0.75).abs() < 1e-9);
        assert!((parse_min_confidence(Some("0"), 0.3).expect("floor edge") - 0.0).abs() < 1e-9);
        assert!((parse_min_confidence(Some("1"), 0.3).expect("ceil edge") - 1.0).abs() < 1e-9);
        // Out of range and non-numeric are loud errors, never a silent clamp.
        assert!(
            parse_min_confidence(Some("1.5"), 0.3).is_err(),
            "above 1.0 must error"
        );
        assert!(
            parse_min_confidence(Some("-0.1"), 0.3).is_err(),
            "below 0.0 must error"
        );
        assert!(
            parse_min_confidence(Some("high"), 0.3).is_err(),
            "garbage must error"
        );
    }

    #[test]
    fn lookback_parser_defaults_accepts_and_rejects_out_of_range() {
        assert_eq!(parse_lookback(None, 6).expect("absent -> default"), 6);
        assert_eq!(parse_lookback(Some(""), 6).expect("blank -> default"), 6);
        assert_eq!(parse_lookback(Some("12"), 6).expect("valid"), 12);
        assert_eq!(parse_lookback(Some("1"), 6).expect("min edge"), 1);
        assert_eq!(
            parse_lookback(Some("168"), 6).expect("max edge"),
            super::super::intent::MAX_LOOKBACK_HOURS
        );
        assert!(
            parse_lookback(Some("0"), 6).is_err(),
            "zero hours must error"
        );
        assert!(
            parse_lookback(Some("169"), 6).is_err(),
            "above max must error"
        );
        assert!(
            parse_lookback(Some("six"), 6).is_err(),
            "garbage must error"
        );
    }

    #[test]
    fn event_kinds_and_payload_are_stable() {
        assert_eq!(
            IntentTransitionKind::Detected.event_kind(),
            "intent-detected"
        );
        assert_eq!(
            IntentTransitionKind::Confirmed.event_kind(),
            "intent-confirmed"
        );
        assert_eq!(
            IntentTransitionKind::Abandoned.event_kind(),
            "intent-abandoned"
        );
        let event = transition_event(&detected_transition(&candidate("rt1-a", 1, 2, 0.5)));
        assert_eq!(event.kind, "intent-detected");
        assert_eq!(event.source, EventSource::System);
        assert_eq!(event.data["routine_id"], "rt1-a");
        assert_eq!(event.data["reason"], "prefix_match");
        assert_eq!(event.data["total_steps"], 2);
    }
}
