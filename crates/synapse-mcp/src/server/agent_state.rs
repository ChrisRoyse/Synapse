//! Per-agent lifecycle state machine + liveness detection (#898).
//!
//! A daemon-side projection over the `CF_AGENT_EVENTS` journal (#897): every
//! journal write flows through [`super::agent_events::record_agent_events`],
//! which feeds this tracker, so the machine sees exactly the events the
//! journal persists — spawn lifecycle, session lifecycle, and the hook pushes
//! delivered by the #899 ingress. No pane scraping, no transcript heuristics.
//!
//! States: `spawning, working, idle, needs_input, awaiting_approval,
//! ready_for_review, stuck, dead`. Each real transition writes an
//! authoritative `state_changed` journal row (`state_from`/`state_to` +
//! machine-readable reason code, payload tagged `origin =
//! "agent_state_machine"`) and is published on the SSE event bus as an
//! `agent_state_changed` event so dashboards update live.
//!
//! # Liveness (research-backed heuristics)
//!
//! A periodic sweep ([`liveness_sweep_once`]) cross-checks heartbeat silence
//! with a process-table probe — silence alone cannot distinguish a stuck
//! agent from a dead one (the agent process may have been killed without any
//! exit event reaching the journal):
//!
//! - `working`/`spawning` and silent past the threshold (default 120 s):
//!   process alive + no fresh spawn artifact output → `stuck`
//!   (`silent_timeout`), process gone → `dead`
//!   (`process_gone_without_exit_event`).
//! - any non-dead agent whose known PID has vanished → `dead`.
//! - runaway: the same tool called with identical argument digests N times
//!   consecutively (default 5) → `stuck` with `runaway = true`
//!   (`runaway_tool_loop`). Never auto-killed: flagged and surfaced only.
//!   Token-burn-based runaway detection needs per-turn usage data and lands
//!   with #901.
//!
//! # Rules that keep the journal honest
//!
//! - First sight of an agent initializes its state silently — the triggering
//!   journal row already documents it; only subsequent changes emit
//!   `state_changed` rows, so the journal never carries duplicate facts.
//! - Events arriving for a `dead` agent (hook delivered after a kill) are
//!   refused with a structured `AGENT_STATE_EVENT_AFTER_DEATH` log — a dead
//!   agent is never resurrected by a straggler hook.
//! - A failed transition-row write logs `AGENT_STATE_ROW_WRITE_FAILED` but
//!   never fails the already-committed primary write; the machine state is
//!   re-derivable from the primary events on rebuild, so nothing is lost.
//! - On daemon start [`rebuild_from_journal`] replays the recent journal
//!   (24 h lookback) so states survive restarts; undecodable rows are
//!   surfaced (`AGENT_STATE_REBUILD_ROW_INVALID` + counter), never skipped
//!   silently.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::UNIX_EPOCH,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{AgentEventKind, AgentEventRecord, Event, EventSource};
use synapse_reflex::EventBus;
use synapse_storage::{Db, StorageResult, agent_events::agent_event_scan_start, cf, decode_json};

use super::agent_events::{record_agent_events_unobserved, unix_time_ns_now};

/// Payload marker distinguishing machine-emitted `state_changed` rows from
/// sender-pushed ones. The tracker never consumes its own output live, and
/// the rebuild path applies marked rows authoritatively instead of reducing.
pub(crate) const STATE_MACHINE_ORIGIN: &str = "agent_state_machine";

/// SSE event kind for live dashboard consumption.
pub(crate) const AGENT_STATE_EVENT_KIND: &str = "agent_state_changed";

/// Silent-for-N default while `working`/`spawning` (#898 spec: 120 s).
pub(crate) const DEFAULT_STUCK_AFTER_MS: u64 = 120_000;

/// Default sweep cadence. Detection latency is bounded by
/// `stuck_after_ms + sweep_interval_ms`.
pub(crate) const DEFAULT_SWEEP_INTERVAL_MS: u64 = 15_000;

/// Consecutive identical `(tool_name, tool_input_sha256)` calls before the
/// runaway flag raises. Industry heuristics use 3–6; 5 keeps false positives
/// low while catching real loops within one sweep window.
pub(crate) const DEFAULT_RUNAWAY_IDENTICAL_CALLS: u32 = 5;

/// Dead/exited agents older than this are pruned from the in-memory tracker
/// (their journal rows remain the durable record).
const DEAD_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

/// An UNPROBEABLE agent — one with no OS pid the daemon can liveness-check,
/// e.g. an observed/ambient session tailed from a transcript on disk — that has
/// gone silent this long is treated as ENDED, not merely stuck. With no pid and
/// no progress there is no live process for an operator to act on, so it
/// transitions straight to `Dead` (auto-reaped after `DEAD_RETENTION_MS`),
/// keeping un-actionable dormant sessions out of the attention queue instead of
/// piling up forever. A resumed session re-registers and revives (see
/// `apply_event`). Mirrors Filebeat `ignore_older`, k8s TTL-after-finished, and
/// Kestra's DISCONNECTED→TERMINATED. Env-overridable; default 30 min.
pub(crate) const DEFAULT_UNPROBEABLE_DEAD_AFTER_MS: u64 = 30 * 60 * 1000;

/// While an unprobeable agent's most recent journal event is an in-flight
/// `ToolCallStarted` with no matching finish, it is *executing a tool* — a long
/// shell job, a browser wait, or a web fetch legitimately emits nothing until
/// the tool returns. Silence there is work in progress, not death, so the
/// end-of-life verdict is deferred to this multiple of
/// `unprobeable_dead_after_ms` (a work-aware deadline, mirroring Conductor's
/// long-task `timeoutSeconds` vs heartbeat `responseTimeoutSeconds` split and
/// comis's activity-resetting stall budget with an outer makespan ceiling). The
/// resurrection guard still recovers the agent if this outer bound is exceeded
/// and real activity later resumes, so the multiplier only trades a longer
/// dormant-but-visible window for far fewer false reaps of working agents (#1594).
pub(crate) const UNPROBEABLE_INFLIGHT_TOOL_GRACE_MULT: u64 = 4;

/// The `reason_code` the unprobeable-silence reaper stamps on an *inferred*
/// death. Unlike a confirmed terminal event (`Killed`/`Exited`/process-gone),
/// this death is a heuristic guess from silence alone; a subsequent real
/// agent-loop event overturns it (see [`AgentStateTracker::apply_event`]).
const UNPROBEABLE_SILENT_ENDED_REASON: &str = "unprobeable_silent_ended";

/// Audit `reason_code` for a resurrection: a dead agent transitioned back to a
/// live state because real agent-loop activity proved the inferred death wrong.
const RESURRECTED_REASON: &str = "resurrected_by_live_evidence";

/// Process-wide count of journal events discarded because they arrived for an
/// agent in a *confirmed*-dead state (`AGENT_STATE_EVENT_AFTER_DEATH`). Surfaced
/// as a running total on every drop so a pile-up (#1594: 198 events for one
/// agent) is a visible signal rather than 198 disconnected INFO lines. Never
/// reset in production; readable in tests via [`events_dropped_after_death_count`].
static EVENTS_DROPPED_AFTER_DEATH: AtomicU64 = AtomicU64::new(0);

/// Rebuild lookback window over `CF_AGENT_EVENTS`.
const REBUILD_LOOKBACK_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// Rebuild scan page size.
const REBUILD_PAGE_ROWS: usize = 4096;

static NEXT_BUS_EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Lifecycle states (#898). Serialized snake_case everywhere: journal rows,
/// `session_list` reads, SSE events.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentLifecycleState {
    Spawning,
    Working,
    Idle,
    NeedsInput,
    AwaitingApproval,
    ReadyForReview,
    Stuck,
    Dead,
}

impl AgentLifecycleState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::NeedsInput => "needs_input",
            Self::AwaitingApproval => "awaiting_approval",
            Self::ReadyForReview => "ready_for_review",
            Self::Stuck => "stuck",
            Self::Dead => "dead",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "spawning" => Some(Self::Spawning),
            "working" => Some(Self::Working),
            "idle" => Some(Self::Idle),
            "needs_input" => Some(Self::NeedsInput),
            "awaiting_approval" => Some(Self::AwaitingApproval),
            "ready_for_review" => Some(Self::ReadyForReview),
            "stuck" => Some(Self::Stuck),
            "dead" => Some(Self::Dead),
            _ => None,
        }
    }
}

/// Fleet/dashboard attention class for agent lifecycle rows. This is separate
/// from lifecycle state: `dead` remains the durable terminal state, while the
/// attention class tells dashboards whether that state is actionable now.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentAttentionClass {
    #[default]
    None,
    ActionableLiveStuck,
    TerminalSetupFailure,
    TerminalRuntimeFailure,
    CleanupRequired,
}

impl AgentAttentionClass {
    pub(crate) const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub(crate) const fn is_terminal_history(self) -> bool {
        matches!(
            self,
            Self::TerminalSetupFailure | Self::TerminalRuntimeFailure
        )
    }

    pub(crate) fn for_lifecycle(state: AgentLifecycleState, reason_code: Option<&str>) -> Self {
        match state {
            AgentLifecycleState::Stuck => Self::ActionableLiveStuck,
            AgentLifecycleState::Dead if terminal_setup_failure_reason(reason_code) => {
                Self::TerminalSetupFailure
            }
            AgentLifecycleState::Dead if normal_terminal_reason(reason_code) => Self::None,
            AgentLifecycleState::Dead => Self::TerminalRuntimeFailure,
            AgentLifecycleState::Spawning
            | AgentLifecycleState::Working
            | AgentLifecycleState::Idle
            | AgentLifecycleState::NeedsInput
            | AgentLifecycleState::AwaitingApproval
            | AgentLifecycleState::ReadyForReview => Self::None,
        }
    }
}

fn normal_terminal_reason(reason_code: Option<&str>) -> bool {
    matches!(
        reason_code,
        Some("spawn_completed" | "local_agent_completed")
    )
}

fn terminal_setup_failure_reason(reason_code: Option<&str>) -> bool {
    matches!(
        reason_code,
        Some(
            "local_model_model_ref_missing"
                | "local_model_registry_row_missing"
                | "local_model_registry_row_disabled"
                | "local_model_api_shape_unsupported"
                | "local_model_registry_row_unprobed"
                | "local_model_registry_row_unhealthy"
                | "local_model_api_key_decrypt_failed"
                | "local_model_api_key_missing"
                | "session_registry_readback_timeout"
                | "task_start_readiness_readback_failed"
                | "process_history_record_failed"
                | "agent_spawn_shell_env_not_unicode"
                | "agent_spawn_shell_env_empty"
                | "agent_spawn_shell_env_target_missing"
                | "agent_spawn_shell_target_missing"
                | "agent_spawn_shell_not_found"
                | "agent_spawn_shell_not_executable"
        )
    )
}

/// One agent's state as exposed on `session_list` / `session_status` rows.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentStateRead {
    /// Attribution anchor: the spawn id for spawned agents, otherwise the
    /// MCP session id.
    pub anchor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    pub state: AgentLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "AgentAttentionClass::is_none")]
    pub attention_class: AgentAttentionClass,
    pub since_unix_ms: u64,
    pub last_event_unix_ms: u64,
    pub last_event_kind: AgentEventKind,
    pub silent_ms: u64,
    /// What the agent is blocked on while `needs_input`/`awaiting_approval`
    /// (notification type or `tool:<name>`), or the loop signature while
    /// runaway-stuck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    pub runaway: bool,
    pub consecutive_identical_tool_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
}

/// One emitted transition, journaled as an authoritative `state_changed` row
/// and published on the event bus.
#[derive(Clone, Debug)]
pub(crate) struct StateTransition {
    pub anchor: String,
    pub spawn_id: Option<String>,
    pub session_id: Option<String>,
    pub state_from: AgentLifecycleState,
    pub state_to: AgentLifecycleState,
    pub reason_code: String,
    pub waiting_for: Option<String>,
    pub runaway: bool,
    pub evidence: Value,
}

#[derive(Clone, Debug)]
struct AgentEntry {
    anchor: String,
    spawn_id: Option<String>,
    session_id: Option<String>,
    agent_kind: Option<String>,
    state: AgentLifecycleState,
    reason_code: Option<String>,
    since_unix_ms: u64,
    last_event_unix_ms: u64,
    last_event_kind: AgentEventKind,
    waiting_for: Option<String>,
    runaway: bool,
    last_tool_signature: Option<(String, Option<String>)>,
    identical_tool_calls: u32,
    launcher_process_id: Option<u32>,
    agent_process_id: Option<u32>,
    log_dir: Option<String>,
}

#[derive(Clone, Debug)]
struct AgentArtifactActivity {
    source: &'static str,
    path: String,
    modified_unix_ms: u64,
    len_bytes: u64,
}

impl AgentEntry {
    fn read(&self, now_unix_ms: u64) -> AgentStateRead {
        AgentStateRead {
            anchor: self.anchor.clone(),
            spawn_id: self.spawn_id.clone(),
            session_id: self.session_id.clone(),
            agent_kind: self.agent_kind.clone(),
            state: self.state,
            reason_code: self.reason_code.clone(),
            attention_class: AgentAttentionClass::for_lifecycle(
                self.state,
                self.reason_code.as_deref(),
            ),
            since_unix_ms: self.since_unix_ms,
            last_event_unix_ms: self.last_event_unix_ms,
            last_event_kind: self.last_event_kind,
            silent_ms: now_unix_ms.saturating_sub(self.last_event_unix_ms),
            waiting_for: self.waiting_for.clone(),
            runaway: self.runaway,
            consecutive_identical_tool_calls: self.identical_tool_calls,
            last_tool_name: self
                .last_tool_signature
                .as_ref()
                .map(|(tool, _digest)| tool.clone()),
            launcher_process_id: self.launcher_process_id,
            agent_process_id: self.agent_process_id,
            log_dir: self.log_dir.clone(),
        }
    }

    fn probe_pid(&self) -> Option<u32> {
        self.agent_process_id.or(self.launcher_process_id)
    }
}

fn late_exit_reconciles_process_probe_death(entry: &AgentEntry, record: &AgentEventRecord) -> bool {
    record.kind == AgentEventKind::Exited
        && entry.reason_code.as_deref() == Some("process_gone_without_exit_event")
}

/// True when the entry's dead state is an *inferred* liveness-sweep death (the
/// unprobeable-silence reaper's guess from silence alone), as opposed to a
/// confirmed terminal event. Only inferred deaths may be overturned by later
/// live evidence; a `Killed`/`Exited`/process-gone death is authoritative and a
/// straggler never resurrects it.
fn is_liveness_inferred_death(entry: &AgentEntry) -> bool {
    entry.state == AgentLifecycleState::Dead
        && entry.reason_code.as_deref() == Some(UNPROBEABLE_SILENT_ENDED_REASON)
}

/// True for journal events that are direct proof the agent loop is running:
/// turn boundaries and tool-call activity. Mailbox/lease/state-changed traffic
/// is deliberately excluded — it can originate from arbitrary sessions and is a
/// weaker liveness signal than the agent's own turn/tool events.
fn is_proof_of_life(kind: AgentEventKind) -> bool {
    matches!(
        kind,
        AgentEventKind::TurnStarted
            | AgentEventKind::ToolCallStarted
            | AgentEventKind::ToolCallFinished
            | AgentEventKind::TurnFinished
    )
}

fn newest_spawn_artifact_activity(entry: &AgentEntry) -> Option<AgentArtifactActivity> {
    let log_dir = Path::new(entry.log_dir.as_deref()?);
    [
        ("stdout_jsonl", "stdout.jsonl"),
        ("codex_app_server_stdout", "codex-app-server.stdout.log"),
        ("codex_app_server_events", "codex-app-server-events.jsonl"),
        ("codex_control", "codex-control.json"),
    ]
    .into_iter()
    .filter_map(|(source, file_name)| artifact_activity(log_dir, source, file_name))
    .max_by_key(|activity| (activity.modified_unix_ms, activity.len_bytes))
}

fn artifact_activity(
    log_dir: &Path,
    source: &'static str,
    file_name: &str,
) -> Option<AgentArtifactActivity> {
    let path = log_dir.join(file_name);
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let modified_unix_ms = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())?;
    Some(AgentArtifactActivity {
        source,
        path: path.display().to_string(),
        modified_unix_ms,
        len_bytes: metadata.len(),
    })
}

/// Liveness knobs, env-overridable. Loaded once at daemon startup via
/// [`load_liveness_config`]; invalid values refuse daemon start instead of
/// being silently replaced.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LivenessConfig {
    pub stuck_after_ms: u64,
    pub sweep_interval_ms: u64,
    pub runaway_identical_calls: u32,
    pub unprobeable_dead_after_ms: u64,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self {
            stuck_after_ms: DEFAULT_STUCK_AFTER_MS,
            sweep_interval_ms: DEFAULT_SWEEP_INTERVAL_MS,
            runaway_identical_calls: DEFAULT_RUNAWAY_IDENTICAL_CALLS,
            unprobeable_dead_after_ms: DEFAULT_UNPROBEABLE_DEAD_AFTER_MS,
        }
    }
}

static LIVENESS_CONFIG: OnceLock<LivenessConfig> = OnceLock::new();

/// Parses the liveness env knobs (`SYNAPSE_AGENT_STUCK_AFTER_MS`,
/// `SYNAPSE_AGENT_LIVENESS_SWEEP_MS`, `SYNAPSE_AGENT_RUNAWAY_TOOL_CALLS`)
/// and installs them process-wide.
///
/// # Errors
///
/// Returns a message naming the offending variable when a value is set but
/// not a positive integer — the daemon must refuse to start rather than run
/// with a misconfigured liveness monitor.
pub(crate) fn load_liveness_config() -> Result<LivenessConfig, String> {
    fn parse_env_u64(name: &str, default: u64) -> Result<u64, String> {
        match std::env::var(name) {
            Ok(raw) => raw
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or_else(|| {
                    format!("{name} must be a positive integer (milliseconds), got {raw:?}")
                }),
            Err(std::env::VarError::NotPresent) => Ok(default),
            Err(error) => Err(format!("{name} is not valid unicode: {error}")),
        }
    }
    let config = LivenessConfig {
        stuck_after_ms: parse_env_u64("SYNAPSE_AGENT_STUCK_AFTER_MS", DEFAULT_STUCK_AFTER_MS)?,
        sweep_interval_ms: parse_env_u64(
            "SYNAPSE_AGENT_LIVENESS_SWEEP_MS",
            DEFAULT_SWEEP_INTERVAL_MS,
        )?,
        runaway_identical_calls: u32::try_from(parse_env_u64(
            "SYNAPSE_AGENT_RUNAWAY_TOOL_CALLS",
            u64::from(DEFAULT_RUNAWAY_IDENTICAL_CALLS),
        )?)
        .map_err(|_error| "SYNAPSE_AGENT_RUNAWAY_TOOL_CALLS exceeds u32 range".to_owned())?,
        unprobeable_dead_after_ms: parse_env_u64(
            "SYNAPSE_AGENT_UNPROBEABLE_DEAD_AFTER_MS",
            DEFAULT_UNPROBEABLE_DEAD_AFTER_MS,
        )?,
    };
    Ok(*LIVENESS_CONFIG.get_or_init(|| config))
}

pub(crate) fn liveness_config() -> LivenessConfig {
    LIVENESS_CONFIG.get().copied().unwrap_or_default()
}

/// The in-memory projection. Pure with respect to its inputs so unit tests
/// drive planted event sequences directly; the daemon uses one process-wide
/// instance behind [`tracker`].
#[derive(Debug, Default)]
pub(crate) struct AgentStateTracker {
    agents: BTreeMap<String, AgentEntry>,
    session_to_anchor: BTreeMap<String, String>,
}

impl AgentStateTracker {
    /// Applies one journal event, returning the transition when the agent's
    /// state actually changed. First sight initializes silently (no
    /// transition) — the triggering journal row documents it.
    pub(crate) fn apply_event(&mut self, record: &AgentEventRecord) -> Option<StateTransition> {
        let event_unix_ms = record.ts_ns / 1_000_000;
        let key = self.resolve_anchor(record)?;
        let runaway_calls = liveness_config().runaway_identical_calls;

        let entry = match self.agents.entry(key) {
            std::collections::btree_map::Entry::Vacant(vacant) => {
                if let Some(initial) = initial_entry(vacant.key(), record, event_unix_ms) {
                    vacant.insert(initial);
                }
                return None;
            }
            std::collections::btree_map::Entry::Occupied(occupied) => occupied.into_mut(),
        };

        // A dead agent stays dead for straggler exits/hooks — a kill or late
        // exit must never resurrect it. A late explicit `Exited` row may still
        // reconcile a provisional process-probe terminal reason without
        // changing the terminal state. The ONE state-changing exception is a fresh
        // re-registration (`SpawnRequested`): an observed/ambient session that
        // was reaped for dormancy resumes by appending to the same transcript,
        // and the ingester re-registers it. Re-binding to the same anchor
        // (rather than leaving it dead or forking a duplicate) is the explicit
        // resurrection guard the dormancy reap requires — it falls through to
        // `reduce`, which maps `SpawnRequested` → `Spawning`.
        // Live evidence overturns an INFERRED liveness-sweep death. The
        // unprobeable-silence reaper only *guesses* an ambient agent ended
        // because it went quiet with no pid to probe (#1594); a subsequent real
        // agent-loop event (turn/tool) is direct proof the guess was wrong.
        // Rather than discard the agent's real activity, resurrect it with an
        // audited RESURRECTED transition. A CONFIRMED death
        // (killed/exited/process-gone) is authoritative and is never overturned
        // by a straggler — see `hook_after_kill_never_resurrects_a_dead_agent`.
        let resurrecting_on_evidence =
            is_liveness_inferred_death(entry) && is_proof_of_life(record.kind);
        if entry.state == AgentLifecycleState::Dead
            && !matches!(record.kind, AgentEventKind::SpawnRequested)
            && !late_exit_reconciles_process_probe_death(entry, record)
            && !resurrecting_on_evidence
        {
            if !matches!(record.kind, AgentEventKind::Exited | AgentEventKind::Killed) {
                let dropped = EVENTS_DROPPED_AFTER_DEATH.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    code = "AGENT_STATE_EVENT_AFTER_DEATH",
                    anchor = %entry.anchor,
                    kind = ?record.kind,
                    reason_code = ?entry.reason_code,
                    ts_ns = record.ts_ns,
                    events_dropped_after_death_total = dropped,
                    "journal event arrived for a confirmed-dead agent; event discarded (state stays dead)"
                );
            }
            return None;
        }
        // Captured before bookkeeping so the RESURRECTED audit can name the
        // death reason the live evidence just overturned.
        let death_reason_before_resurrection = if resurrecting_on_evidence {
            entry.reason_code.clone()
        } else {
            None
        };

        // Bookkeeping that never changes state by itself.
        if entry.session_id.is_none() && record.session_id.is_some() {
            entry.session_id.clone_from(&record.session_id);
        }
        if entry.agent_kind.is_none() && record.attributes.agent_name.is_some() {
            entry.agent_kind.clone_from(&record.attributes.agent_name);
        }
        if record.kind == AgentEventKind::SpawnReady {
            entry.launcher_process_id = payload_u32(&record.payload, "launcher_process_id");
            entry.agent_process_id = payload_u32(&record.payload, "agent_process_id");
            entry.log_dir = payload_string(&record.payload, "log_dir");
        }
        entry.last_event_unix_ms = entry.last_event_unix_ms.max(event_unix_ms);
        entry.last_event_kind = record.kind;

        let decision = reduce(entry, record, runaway_calls)?;
        let state_from = entry.state;
        if decision.state == state_from {
            // Same state: refresh the supporting detail (e.g. a new
            // needs_input reason) without emitting a duplicate transition.
            entry.reason_code = Some(decision.reason_code);
            entry.waiting_for = decision.waiting_for;
            return None;
        }
        entry.state = decision.state;
        let (reason_code, evidence) = if resurrecting_on_evidence {
            tracing::warn!(
                code = "AGENT_STATE_RESURRECTED",
                anchor = %entry.anchor,
                kind = ?record.kind,
                prior_death_reason = ?death_reason_before_resurrection,
                state_to = decision.state.as_str(),
                ts_ns = record.ts_ns,
                "inferred-dead agent produced live agent-loop activity; resurrected"
            );
            (
                RESURRECTED_REASON.to_owned(),
                json!({
                    "resurrected": true,
                    "prior_death_reason": death_reason_before_resurrection,
                    "trigger_event_kind": record.kind,
                    "reduced_reason": decision.reason_code,
                }),
            )
        } else {
            (decision.reason_code, decision.evidence)
        };
        entry.reason_code = Some(reason_code.clone());
        entry.waiting_for = decision.waiting_for.clone();
        entry.since_unix_ms = event_unix_ms;
        Some(StateTransition {
            anchor: entry.anchor.clone(),
            spawn_id: entry.spawn_id.clone(),
            session_id: entry.session_id.clone(),
            state_from,
            state_to: decision.state,
            reason_code,
            waiting_for: decision.waiting_for,
            runaway: entry.runaway,
            evidence,
        })
    }

    /// Applies a machine-emitted `state_changed` row authoritatively (rebuild
    /// path): the row already names the resulting state, so it is restored
    /// verbatim instead of re-reduced.
    fn apply_authoritative(&mut self, record: &AgentEventRecord) {
        let Some(state) = record
            .state_to
            .as_deref()
            .and_then(AgentLifecycleState::parse)
        else {
            tracing::error!(
                code = "AGENT_STATE_REBUILD_ROW_INVALID",
                state_to = ?record.state_to,
                ts_ns = record.ts_ns,
                "machine-origin state_changed row carries no parseable state_to"
            );
            return;
        };
        let event_unix_ms = record.ts_ns / 1_000_000;
        let Some(key) = self.resolve_anchor(record) else {
            return;
        };
        let entry = self
            .agents
            .entry(key.clone())
            .or_insert_with(|| AgentEntry {
                anchor: key,
                spawn_id: record.spawn_id.clone(),
                session_id: record.session_id.clone(),
                agent_kind: None,
                state,
                reason_code: record.reason_code.clone(),
                since_unix_ms: event_unix_ms,
                last_event_unix_ms: event_unix_ms,
                last_event_kind: record.kind,
                waiting_for: None,
                runaway: false,
                last_tool_signature: None,
                identical_tool_calls: 0,
                launcher_process_id: None,
                agent_process_id: None,
                log_dir: None,
            });
        entry.state = state;
        entry.reason_code.clone_from(&record.reason_code);
        entry.since_unix_ms = event_unix_ms;
        entry.last_event_unix_ms = entry.last_event_unix_ms.max(event_unix_ms);
        entry.runaway = record
            .payload
            .get("runaway")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        entry.waiting_for = record
            .payload
            .get("waiting_for")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    /// Heartbeat-silence + process-table liveness pass (#898).
    pub(crate) fn sweep(
        &mut self,
        now_unix_ms: u64,
        stuck_after_ms: u64,
        unprobeable_dead_after_ms: u64,
        process_alive: &dyn Fn(u32) -> bool,
    ) -> Vec<StateTransition> {
        let mut transitions = Vec::new();
        for entry in self.agents.values_mut() {
            if entry.state == AgentLifecycleState::Dead {
                continue;
            }
            // Process-alive cross-check applies to every live state: a kill
            // that never produced an exit event must still surface.
            if let Some(pid) = entry.probe_pid()
                && !process_alive(pid)
            {
                transitions.push(force_transition(
                    entry,
                    AgentLifecycleState::Dead,
                    "process_gone_without_exit_event",
                    None,
                    json!({
                        "probed_pid": pid,
                        "silent_ms": now_unix_ms.saturating_sub(entry.last_event_unix_ms),
                        "last_event_kind": entry.last_event_kind,
                    }),
                    now_unix_ms,
                ));
                continue;
            }
            if matches!(
                entry.state,
                AgentLifecycleState::Working
                    | AgentLifecycleState::Spawning
                    | AgentLifecycleState::Stuck
            ) && !entry.runaway
                && let Some(activity) = newest_spawn_artifact_activity(entry)
                && activity.modified_unix_ms > entry.last_event_unix_ms
            {
                let observed_at_unix_ms = activity.modified_unix_ms.min(now_unix_ms);
                entry.last_event_unix_ms = entry.last_event_unix_ms.max(observed_at_unix_ms);
                if entry.state == AgentLifecycleState::Stuck {
                    transitions.push(force_transition(
                        entry,
                        AgentLifecycleState::Working,
                        "artifact_activity_resumed",
                        None,
                        json!({
                            "artifact_source": activity.source,
                            "artifact_path": activity.path,
                            "artifact_modified_unix_ms": activity.modified_unix_ms,
                            "artifact_len_bytes": activity.len_bytes,
                        }),
                        now_unix_ms,
                    ));
                }
                continue;
            }
            // Unprobeable end-of-life: an agent with no pid to liveness-check
            // (an observed/ambient session tailed from disk) that has gone
            // silent past the ended threshold has no live process left to
            // attend to. Transition straight to Dead (reaped after retention)
            // so dormant, un-actionable sessions leave the attention queue
            // instead of accumulating forever. Covers working/idle/stuck alike:
            // an idle observed session that stopped appending has ended just as
            // surely as a working one. A resume re-registers and revives.
            if entry.probe_pid().is_none() {
                let silent_ms = now_unix_ms.saturating_sub(entry.last_event_unix_ms);
                // Work-aware deadline: an in-flight `ToolCallStarted` with no
                // matching finish means the agent is executing a tool (a long
                // shell job / browser wait / web fetch is legitimately silent),
                // so defer the end-of-life verdict with an extended grace rather
                // than reap a working agent mid-call (#1594). Idle-between-turns
                // ambient agents keep the sane base deadline.
                let in_flight_tool_call = entry.last_event_kind == AgentEventKind::ToolCallStarted;
                let dead_after_ms = if in_flight_tool_call {
                    unprobeable_dead_after_ms.saturating_mul(UNPROBEABLE_INFLIGHT_TOOL_GRACE_MULT)
                } else {
                    unprobeable_dead_after_ms
                };
                if silent_ms >= dead_after_ms {
                    transitions.push(force_transition(
                        entry,
                        AgentLifecycleState::Dead,
                        UNPROBEABLE_SILENT_ENDED_REASON,
                        None,
                        json!({
                            "silent_ms": silent_ms,
                            "unprobeable_dead_after_ms": dead_after_ms,
                            "in_flight_tool_call": in_flight_tool_call,
                            "last_event_kind": entry.last_event_kind,
                        }),
                        now_unix_ms,
                    ));
                    continue;
                }
            }
            // Silence applies only while the agent claims to be making
            // progress; waiting states legitimately sit quiet for hours.
            if !matches!(
                entry.state,
                AgentLifecycleState::Working | AgentLifecycleState::Spawning
            ) {
                continue;
            }
            let silent_ms = now_unix_ms.saturating_sub(entry.last_event_unix_ms);
            if silent_ms < stuck_after_ms {
                continue;
            }
            let reason = if entry.state == AgentLifecycleState::Spawning {
                "spawn_silent_timeout"
            } else if entry.probe_pid().is_some() {
                "silent_timeout"
            } else {
                "silent_timeout_unprobeable"
            };
            transitions.push(force_transition(
                entry,
                AgentLifecycleState::Stuck,
                reason,
                Some(format!("silent_for_ms:{silent_ms}")),
                json!({
                    "silent_ms": silent_ms,
                    "stuck_after_ms": stuck_after_ms,
                    "last_event_kind": entry.last_event_kind,
                    "probed_pid": entry.probe_pid(),
                }),
                now_unix_ms,
            ));
        }
        self.prune_dead(now_unix_ms);
        transitions
    }

    fn prune_dead(&mut self, now_unix_ms: u64) {
        let expired: Vec<String> = self
            .agents
            .values()
            .filter(|entry| {
                entry.state == AgentLifecycleState::Dead
                    && now_unix_ms.saturating_sub(entry.since_unix_ms) > DEAD_RETENTION_MS
            })
            .map(|entry| entry.anchor.clone())
            .collect();
        for anchor in expired {
            self.agents.remove(&anchor);
            self.session_to_anchor
                .retain(|_session, mapped| *mapped != anchor);
            tracing::debug!(
                code = "AGENT_STATE_PRUNED",
                anchor = %anchor,
                retention_ms = DEAD_RETENTION_MS,
                "dead agent entry pruned from the in-memory tracker"
            );
        }
    }

    pub(crate) fn read_for_session(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Option<AgentStateRead> {
        let key = self
            .session_to_anchor
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| session_id.to_owned());
        self.agents.get(&key).map(|entry| entry.read(now_unix_ms))
    }

    pub(crate) fn reads(&self, now_unix_ms: u64) -> Vec<AgentStateRead> {
        self.agents
            .values()
            .map(|entry| entry.read(now_unix_ms))
            .collect()
    }

    /// Agents not (yet) linked to any MCP session: in-flight spawns and
    /// spawns that died before registering.
    pub(crate) fn unbound_reads(&self, now_unix_ms: u64) -> Vec<AgentStateRead> {
        self.agents
            .values()
            .filter(|entry| entry.session_id.is_none())
            .map(|entry| entry.read(now_unix_ms))
            .collect()
    }

    /// Anchor resolution: spawned agents key by spawn id; session-only events
    /// follow the session→spawn link established by `spawn_ready`.
    fn resolve_anchor(&mut self, record: &AgentEventRecord) -> Option<String> {
        match (&record.spawn_id, &record.session_id) {
            (Some(spawn_id), Some(session_id)) => {
                let previous = self
                    .session_to_anchor
                    .insert(session_id.clone(), spawn_id.clone());
                if previous.as_deref() != Some(spawn_id.as_str()) {
                    // The session may have accumulated a standalone entry
                    // before the link existed; fold it away so one agent has
                    // exactly one row.
                    if let Some(stale) = self.agents.remove(session_id) {
                        tracing::debug!(
                            code = "AGENT_STATE_SESSION_LINKED",
                            spawn_id = %spawn_id,
                            session_id = %session_id,
                            stale_state = stale.state.as_str(),
                            "session entry merged into its spawn anchor"
                        );
                    }
                }
                if let Some(entry) = self.agents.get_mut(spawn_id)
                    && entry.session_id.is_none()
                {
                    entry.session_id = Some(session_id.clone());
                }
                Some(spawn_id.clone())
            }
            (Some(spawn_id), None) => Some(spawn_id.clone()),
            (None, Some(session_id)) => Some(
                self.session_to_anchor
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| session_id.clone()),
            ),
            (None, None) => None,
        }
    }
}

/// Outcome of reducing one event against one entry.
struct ReduceDecision {
    state: AgentLifecycleState,
    reason_code: String,
    waiting_for: Option<String>,
    evidence: Value,
}

fn decision(state: AgentLifecycleState, reason_code: &str) -> ReduceDecision {
    ReduceDecision {
        state,
        reason_code: reason_code.to_owned(),
        waiting_for: None,
        evidence: Value::Null,
    }
}

/// The reducer: maps one journal event onto the entry's next state. Returns
/// `None` for pure heartbeats (message/lease traffic keeps `last_event`
/// fresh without forcing a state).
fn reduce(
    entry: &mut AgentEntry,
    record: &AgentEventRecord,
    runaway_identical_calls: u32,
) -> Option<ReduceDecision> {
    use AgentEventKind as Kind;
    use AgentLifecycleState as State;
    match record.kind {
        Kind::SpawnRequested => Some(decision(State::Spawning, "spawn_requested")),
        Kind::SpawnReady => Some(decision(State::Working, "spawn_ready")),
        Kind::TurnStarted => {
            entry.runaway = false;
            entry.identical_tool_calls = 0;
            entry.last_tool_signature = None;
            Some(decision(State::Working, "turn_started"))
        }
        Kind::ToolCallStarted => {
            let signature = (
                record
                    .attributes
                    .tool_name
                    .clone()
                    .unwrap_or_else(|| "unknown_tool".to_owned()),
                record
                    .payload
                    .get("tool_input_sha256")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            );
            if entry.last_tool_signature.as_ref() == Some(&signature) {
                entry.identical_tool_calls = entry.identical_tool_calls.saturating_add(1);
            } else {
                entry.last_tool_signature = Some(signature.clone());
                entry.identical_tool_calls = 1;
                entry.runaway = false;
            }
            if entry.identical_tool_calls >= runaway_identical_calls {
                entry.runaway = true;
                let (tool_name, digest) = &signature;
                return Some(ReduceDecision {
                    state: State::Stuck,
                    reason_code: "runaway_tool_loop".to_owned(),
                    waiting_for: Some(format!(
                        "runaway:{tool_name}x{}",
                        entry.identical_tool_calls
                    )),
                    evidence: json!({
                        "tool_name": tool_name,
                        "tool_input_sha256": digest,
                        "consecutive_identical_calls": entry.identical_tool_calls,
                        "threshold": runaway_identical_calls,
                    }),
                });
            }
            Some(decision(State::Working, "tool_activity"))
        }
        Kind::ToolCallFinished => Some(decision(State::Working, "tool_activity")),
        Kind::TurnFinished => {
            entry.runaway = false;
            entry.identical_tool_calls = 0;
            entry.last_tool_signature = None;
            Some(decision(State::Idle, "turn_finished"))
        }
        Kind::StateChanged => reduce_state_changed(record),
        Kind::Interrupted => {
            entry.runaway = false;
            entry.identical_tool_calls = 0;
            Some(decision(
                State::Idle,
                record.reason_code.as_deref().unwrap_or("interrupted"),
            ))
        }
        Kind::Killed => Some(decision(
            State::Dead,
            record.reason_code.as_deref().unwrap_or("killed"),
        )),
        Kind::Exited => Some(decision(
            State::Dead,
            record.reason_code.as_deref().unwrap_or("exited"),
        )),
        // Mailbox/lease traffic proves liveness (heartbeat already recorded
        // by the caller) and recovers a silence-stuck agent, but does not
        // force a state on agents that are legitimately waiting.
        Kind::MessageSent | Kind::MessageReceived | Kind::LeaseAcquired | Kind::LeaseReleased => {
            if entry.state == AgentLifecycleState::Stuck && !entry.runaway {
                Some(decision(State::Working, "activity_resumed"))
            } else {
                None
            }
        }
    }
}

/// Reduces sender-pushed `state_changed` rows (#899 ingress + HTTP session
/// lifecycle) onto attention states.
fn reduce_state_changed(record: &AgentEventRecord) -> Option<ReduceDecision> {
    use AgentLifecycleState as State;
    let reason = record.reason_code.as_deref().unwrap_or("state_changed");
    match record.state_to.as_deref() {
        Some("needs_input") => Some(ReduceDecision {
            state: State::NeedsInput,
            reason_code: reason.to_owned(),
            waiting_for: Some(reason.to_owned()),
            evidence: Value::Null,
        }),
        Some("awaiting_approval") => Some(ReduceDecision {
            state: State::AwaitingApproval,
            reason_code: reason.to_owned(),
            waiting_for: Some(
                record
                    .attributes
                    .tool_name
                    .as_deref()
                    .map_or_else(|| "approval".to_owned(), |tool| format!("tool:{tool}")),
            ),
            evidence: Value::Null,
        }),
        _ => match reason {
            // The CLI conversation finished cleanly: the agent's work is
            // ready for review until its MCP session tears down (Exited).
            "cli_session_end" => Some(decision(State::ReadyForReview, reason)),
            // Approval/elicitation resolved or denied: the agent runs again.
            "permission_denied"
            | "elicitation_complete"
            | "elicitation_response"
            | "auth_success" => Some(decision(State::Working, reason)),
            // Session lifecycle visibility; an existing state is better
            // information than "it is alive", so this only matters for
            // first-sight initialization (handled in `initial_entry`).
            _ => None,
        },
    }
}

/// Initial state for a first-sight agent. `None` for event kinds that may
/// not create entries (mailbox/lease traffic from arbitrary sessions).
fn initial_entry(
    anchor: &str,
    record: &AgentEventRecord,
    event_unix_ms: u64,
) -> Option<AgentEntry> {
    use AgentEventKind as Kind;
    use AgentLifecycleState as State;
    let (state, reason_code, waiting_for) = match record.kind {
        Kind::SpawnRequested => (State::Spawning, "spawn_requested".to_owned(), None),
        Kind::SpawnReady => (State::Working, "spawn_ready".to_owned(), None),
        Kind::TurnStarted | Kind::ToolCallStarted | Kind::ToolCallFinished => {
            (State::Working, "tool_activity".to_owned(), None)
        }
        Kind::TurnFinished => (State::Idle, "turn_finished".to_owned(), None),
        Kind::Interrupted => (
            State::Idle,
            record
                .reason_code
                .clone()
                .unwrap_or_else(|| "interrupted".to_owned()),
            None,
        ),
        Kind::Killed | Kind::Exited => (
            State::Dead,
            record
                .reason_code
                .clone()
                .unwrap_or_else(|| "exited".to_owned()),
            None,
        ),
        Kind::StateChanged => {
            let reason = record
                .reason_code
                .clone()
                .unwrap_or_else(|| "state_changed".to_owned());
            match record.state_to.as_deref() {
                Some("needs_input") => (State::NeedsInput, reason.clone(), Some(reason)),
                Some("awaiting_approval") => (
                    State::AwaitingApproval,
                    reason,
                    record
                        .attributes
                        .tool_name
                        .as_deref()
                        .map(|tool| format!("tool:{tool}")),
                ),
                _ if reason == "cli_session_end" => (State::ReadyForReview, reason, None),
                _ => (State::Idle, reason, None),
            }
        }
        Kind::MessageSent | Kind::MessageReceived | Kind::LeaseAcquired | Kind::LeaseReleased => {
            return None;
        }
    };
    Some(AgentEntry {
        anchor: anchor.to_owned(),
        spawn_id: record.spawn_id.clone(),
        session_id: record.session_id.clone(),
        agent_kind: record.attributes.agent_name.clone(),
        state,
        reason_code: Some(reason_code),
        since_unix_ms: event_unix_ms,
        last_event_unix_ms: event_unix_ms,
        last_event_kind: record.kind,
        waiting_for,
        runaway: false,
        last_tool_signature: None,
        identical_tool_calls: 0,
        launcher_process_id: if record.kind == Kind::SpawnReady {
            payload_u32(&record.payload, "launcher_process_id")
        } else {
            None
        },
        agent_process_id: if record.kind == Kind::SpawnReady {
            payload_u32(&record.payload, "agent_process_id")
        } else {
            None
        },
        log_dir: if matches!(record.kind, Kind::SpawnRequested | Kind::SpawnReady) {
            payload_string(&record.payload, "log_dir")
        } else {
            None
        },
    })
}

fn force_transition(
    entry: &mut AgentEntry,
    state_to: AgentLifecycleState,
    reason_code: &str,
    waiting_for: Option<String>,
    evidence: Value,
    now_unix_ms: u64,
) -> StateTransition {
    let state_from = entry.state;
    entry.state = state_to;
    entry.reason_code = Some(reason_code.to_owned());
    entry.waiting_for.clone_from(&waiting_for);
    entry.since_unix_ms = now_unix_ms;
    StateTransition {
        anchor: entry.anchor.clone(),
        spawn_id: entry.spawn_id.clone(),
        session_id: entry.session_id.clone(),
        state_from,
        state_to,
        reason_code: reason_code.to_owned(),
        waiting_for,
        runaway: entry.runaway,
        evidence,
    }
}

fn payload_u32(payload: &Value, field: &str) -> Option<u32> {
    payload
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

/// True for rows this machine emitted itself.
pub(crate) fn is_state_machine_row(record: &AgentEventRecord) -> bool {
    record.kind == AgentEventKind::StateChanged
        && record.payload.get("origin").and_then(Value::as_str) == Some(STATE_MACHINE_ORIGIN)
}

/// The real process-global agent-state tracker. The tracker is one shared
/// singleton for the daemon process.
fn tracker() -> &'static Mutex<AgentStateTracker> {
    static TRACKER: OnceLock<Mutex<AgentStateTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(AgentStateTracker::default()))
}

static EVENT_BUS: OnceLock<EventBus> = OnceLock::new();

/// Installs the SSE event bus so transitions reach live dashboards. Called
/// once during HTTP transport startup; later calls are ignored.
pub(crate) fn install_event_bus(bus: EventBus) {
    let _already_installed = EVENT_BUS.set(bus);
}

/// Feeds journal events into the process-wide tracker and journals any
/// resulting transitions. Called by the `record_agent_events` choke point
/// after the primary rows committed, so every writer feeds the machine and
/// no writer can bypass it.
pub(crate) fn observe_recorded_events(db: &Db, records: &[AgentEventRecord]) {
    let transitions = {
        let mut guard = match tracker().lock() {
            Ok(guard) => guard,
            Err(_poisoned) => {
                tracing::error!(
                    code = "AGENT_STATE_TRACKER_POISONED",
                    record_count = records.len(),
                    "agent state tracker lock poisoned; journal events not projected"
                );
                return;
            }
        };
        records
            .iter()
            .filter(|record| !is_state_machine_row(record))
            .filter_map(|record| guard.apply_event(record))
            .collect::<Vec<_>>()
    };
    emit_transitions(db, &transitions);
}

/// One liveness pass over the process-wide tracker: process probes + silence
/// thresholds. Returns the number of transitions emitted.
pub(crate) fn liveness_sweep_once(db: &Db, now_unix_ms: u64) -> usize {
    let config = liveness_config();
    let transitions = {
        let mut guard = match tracker().lock() {
            Ok(guard) => guard,
            Err(_poisoned) => {
                tracing::error!(
                    code = "AGENT_STATE_TRACKER_POISONED",
                    "agent state tracker lock poisoned; liveness sweep skipped"
                );
                return 0;
            }
        };
        guard.sweep(
            now_unix_ms,
            config.stuck_after_ms,
            config.unprobeable_dead_after_ms,
            &|pid| crate::m4::process_exists(pid),
        )
    };
    emit_transitions(db, &transitions);
    transitions.len()
}

/// Journals + publishes transitions. A journal failure here is logged loudly
/// (`AGENT_STATE_ROW_WRITE_FAILED`) but never unwinds the caller: the primary
/// event rows already committed and the machine state is re-derivable from
/// them, so refusing the committed write would be dishonest.
fn emit_transitions(db: &Db, transitions: &[StateTransition]) {
    if transitions.is_empty() {
        return;
    }
    let now_ns = unix_time_ns_now();
    let rows: Vec<AgentEventRecord> = transitions
        .iter()
        .map(|transition| transition_record(transition, now_ns))
        .collect();
    match record_agent_events_unobserved(db, &rows) {
        Ok(readbacks) => {
            let terminal = transitions
                .iter()
                .any(|transition| transition.state_to == AgentLifecycleState::Dead);
            if terminal && let Err(error) = db.flush() {
                tracing::error!(
                    code = "AGENT_STATE_ROW_WRITE_FAILED",
                    detail = %error,
                    "terminal state row flush failed; row is batched but not yet crash-durable"
                );
            }
            for (transition, readback) in transitions.iter().zip(&readbacks) {
                tracing::info!(
                    code = "AGENT_STATE_CHANGED",
                    anchor = %transition.anchor,
                    state_from = transition.state_from.as_str(),
                    state_to = transition.state_to.as_str(),
                    reason_code = %transition.reason_code,
                    runaway = transition.runaway,
                    ts_ns = readback.ts_ns,
                    seq = readback.seq,
                    "readback=CF_AGENT_EVENTS edge=state_machine"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                code = "AGENT_STATE_ROW_WRITE_FAILED",
                transition_count = transitions.len(),
                detail = %error,
                "state transition rows could not be journaled; in-memory state advanced and is re-derivable from the primary events"
            );
        }
    }
    // #948: feed live attention-state transitions to the escalation engine
    // after the authoritative rows commit. Replayed transitions go through
    // `apply_event` directly (rebuild_from_journal), never here, so restart
    // never re-fires historical escalations. A failure inside the engine is
    // logged loudly there and never unwinds this committed write.
    let now_unix_ms = now_ns / 1_000_000;
    for transition in transitions {
        super::escalation::note_transition(db, transition, now_unix_ms);
    }
    if let Some(bus) = EVENT_BUS.get() {
        for transition in transitions {
            let report = bus.publish(Event {
                seq: NEXT_BUS_EVENT_SEQ.fetch_add(1, Ordering::Relaxed),
                at: chrono::Utc::now(),
                source: EventSource::System,
                kind: AGENT_STATE_EVENT_KIND.to_owned(),
                data: json!({
                    "anchor": transition.anchor,
                    "spawn_id": transition.spawn_id,
                    "session_id": transition.session_id,
                    "state_from": transition.state_from.as_str(),
                    "state_to": transition.state_to.as_str(),
                    "reason_code": transition.reason_code,
                    "waiting_for": transition.waiting_for,
                    "runaway": transition.runaway,
                }),
                correlations: Vec::new(),
            });
            tracing::debug!(
                code = "AGENT_STATE_EVENT_PUBLISHED",
                anchor = %transition.anchor,
                state_to = transition.state_to.as_str(),
                matched = report.matched,
                queued = report.queued,
                dropped = report.dropped,
                "agent_state_changed event published"
            );
        }
    }
}

fn transition_record(transition: &StateTransition, ts_ns: u64) -> AgentEventRecord {
    let mut record = AgentEventRecord::new(ts_ns, AgentEventKind::StateChanged);
    record.spawn_id.clone_from(&transition.spawn_id);
    record.session_id.clone_from(&transition.session_id);
    record.reason_code = Some(transition.reason_code.clone());
    record.state_from = Some(transition.state_from.as_str().to_owned());
    record.state_to = Some(transition.state_to.as_str().to_owned());
    record.payload = json!({
        "origin": STATE_MACHINE_ORIGIN,
        "waiting_for": transition.waiting_for,
        "runaway": transition.runaway,
        "evidence": transition.evidence,
    });
    record
}

/// Readback of one journal replay.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RebuildReadback {
    pub rows_scanned: usize,
    pub rows_applied: usize,
    pub invalid_rows: usize,
}

/// Rebuilds the process-wide tracker from the recent journal (24 h lookback)
/// so agent states survive daemon restarts. Replay is quiet: it emits no new
/// rows and no bus events — the journal already contains this history.
///
/// # Errors
///
/// Returns the storage error when the journal cannot be scanned; the daemon
/// must refuse to start over unreadable storage rather than serve empty
/// state as if it were truth.
pub(crate) fn rebuild_from_journal(db: &Db) -> StorageResult<RebuildReadback> {
    let now_ns = unix_time_ns_now();
    let mut start_key = agent_event_scan_start(now_ns.saturating_sub(REBUILD_LOOKBACK_NS));
    let mut readback = RebuildReadback::default();
    let mut guard = match tracker().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    loop {
        let (rows, more) = db.scan_cf_from(cf::CF_AGENT_EVENTS, &start_key, REBUILD_PAGE_ROWS)?;
        let Some((last_key, _last_value)) = rows.last() else {
            break;
        };
        let mut next_start = last_key.clone();
        next_start.push(0);
        for (key, value) in &rows {
            readback.rows_scanned += 1;
            match decode_json::<AgentEventRecord>(value) {
                Ok(record) => {
                    if is_state_machine_row(&record) {
                        guard.apply_authoritative(&record);
                    } else {
                        let _quiet_transition = guard.apply_event(&record);
                    }
                    readback.rows_applied += 1;
                }
                Err(error) => {
                    readback.invalid_rows += 1;
                    tracing::error!(
                        code = "AGENT_STATE_REBUILD_ROW_INVALID",
                        key = ?key,
                        detail = %error,
                        "journal row failed to decode during state rebuild; row skipped, count surfaced"
                    );
                }
            }
        }
        if !more {
            break;
        }
        start_key = next_start;
    }
    tracing::info!(
        code = "AGENT_STATE_REBUILT",
        rows_scanned = readback.rows_scanned,
        rows_applied = readback.rows_applied,
        invalid_rows = readback.invalid_rows,
        tracked_agents = guard.agents.len(),
        "agent state tracker rebuilt from CF_AGENT_EVENTS"
    );
    Ok(readback)
}

/// Read joins for `session_list` / `session_status` (process-wide tracker).
pub(crate) fn read_for_session(session_id: &str, now_unix_ms: u64) -> Option<AgentStateRead> {
    tracker()
        .lock()
        .ok()?
        .read_for_session(session_id, now_unix_ms)
}

fn payload_string(payload: &Value, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn reads(now_unix_ms: u64) -> Vec<AgentStateRead> {
    tracker()
        .lock()
        .map(|guard| guard.reads(now_unix_ms))
        .unwrap_or_default()
}

/// Agents with no MCP session yet (or ever) — in-flight or failed spawns.
pub(crate) fn unbound_reads(now_unix_ms: u64) -> Vec<AgentStateRead> {
    tracker()
        .lock()
        .map(|guard| guard.unbound_reads(now_unix_ms))
        .unwrap_or_default()
}

/// Derive one agent's lifecycle read from an explicit set of journal records,
/// using the exact reducer the live tracker uses, without touching the
/// process-wide singleton (#911).
///
/// `records` must be in ascending `(ts_ns, seq)` order — the order journal
/// scans return rows in. `lookup_id` is the MCP session id or spawn id the
/// caller is interested in; anchor resolution follows the same session↔spawn
/// linking the live tracker performs, so passing either id resolves the same
/// agent once a `spawn_ready` row has linked them.
///
/// This is what makes `agent_query` deterministic and restart-robust: the
/// CF_AGENT_EVENTS journal is the source of truth, and the live in-memory
/// tracker is only a cache rebuilt from it. Reconstructing from the same rows
/// the query already scanned guarantees the reported state is self-consistent
/// with the events the query returns. Returns `None` when no scanned row
/// resolves to `lookup_id`.
pub(crate) fn read_from_journal_records(
    records: &[AgentEventRecord],
    lookup_id: &str,
    now_unix_ms: u64,
) -> Option<AgentStateRead> {
    let mut local = AgentStateTracker::default();
    for record in records {
        if is_state_machine_row(record) {
            local.apply_authoritative(record);
        } else {
            let _quiet_transition = local.apply_event(record);
        }
    }
    local.read_for_session(lookup_id, now_unix_ms)
}
