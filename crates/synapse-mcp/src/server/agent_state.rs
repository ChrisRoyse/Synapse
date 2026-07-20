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
//!   (`process_gone_without_exit_event`). Observed ambient transcript agents
//!   with no process handle are not actionable stuck work from silence alone;
//!   they stay visible until the unprobeable-dead threshold below reaps them.
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
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::UNIX_EPOCH,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{
    AgentEndState, AgentEventKind, AgentEventRecord, Event, EventSource,
    retention::{DEFAULTS as RETENTION_DEFAULTS, RetentionTtl},
};
use synapse_reflex::EventBus;
use synapse_storage::{
    Db, StorageError, StorageResult,
    agent_events::{agent_event_scan_start, decode_agent_event_key},
    cf, decode_json,
};

use super::agent_events::{
    AgentEventWriteReadback, TransitionJournalIntent, commit_agent_event_records_with_intents,
    project_committed_agent_event_artifacts, provider_for_agent_kind, unix_time_ns_now,
};

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

const AMBIENT_SPAWN_ID_PREFIX: &str = "agent-spawn-ambient-";

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

/// The ambient cursor outbox carries at most registration's two rows plus one
/// coalesced lifecycle row. Exact-recovery refuses any broader replay surface.
const MAX_AMBIENT_EXACT_RECOVERY_ROWS: usize = 3;

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

    pub(crate) fn parse(raw: &str) -> Option<Self> {
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
        Some("spawn_completed" | "local_agent_completed" | UNPROBEABLE_SILENT_ENDED_REASON)
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

#[derive(Clone, Debug, Default)]
struct LivenessSweepActions {
    terminal_events: Vec<AgentEventRecord>,
    transitions: Vec<StateTransition>,
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

fn is_ambient_without_process_handle(entry: &AgentEntry) -> bool {
    entry.probe_pid().is_none()
        && entry
            .spawn_id
            .as_deref()
            .unwrap_or(entry.anchor.as_str())
            .starts_with(AMBIENT_SPAWN_ID_PREFIX)
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

fn process_gone_terminal_event(
    entry: &AgentEntry,
    probed_pid: u32,
    now_unix_ms: u64,
) -> AgentEventRecord {
    let completion = entry
        .log_dir
        .as_deref()
        .map(read_spawn_completion_for_process_gone)
        .unwrap_or_else(|| SpawnCompletionForProcessGone {
            read_error: Some("agent state entry had no log_dir".to_owned()),
            ..SpawnCompletionForProcessGone::default()
        });
    let reason_code = if completion.status.as_deref() == Some("ok")
        && matches!(completion.exit_code, Some(0) | None)
    {
        "spawn_completed"
    } else {
        "process_gone_without_exit_event"
    };
    let mut record = AgentEventRecord::new(
        now_unix_ms.saturating_mul(1_000_000),
        AgentEventKind::Exited,
    );
    record.spawn_id.clone_from(&entry.spawn_id);
    record.session_id.clone_from(&entry.session_id);
    record.reason_code = Some(reason_code.to_owned());
    record.end_state = Some(completion.end_state());
    record.attributes.agent_name.clone_from(&entry.agent_kind);
    record.attributes.provider_name = entry
        .agent_kind
        .as_deref()
        .and_then(provider_for_agent_kind);
    record
        .attributes
        .conversation_id
        .clone_from(&entry.session_id);
    record.payload = json!({
        "source_of_truth": "OS process table + agent spawn completion-status.json",
        "probed_pid": probed_pid,
        "silent_ms": now_unix_ms.saturating_sub(entry.last_event_unix_ms),
        "last_event_kind": entry.last_event_kind,
        "log_dir": entry.log_dir,
        "completion_status_path": completion.path,
        "completion_status": completion.status,
        "completion_status_read_error": completion.read_error,
        "exit_code": completion.exit_code,
        "error_message": completion.error_message,
        "final_message_bytes": completion.final_message_bytes,
        "fallback_final_message_written": completion.fallback_final_message_written,
    });
    record
}

#[derive(Clone, Debug, Default)]
struct SpawnCompletionForProcessGone {
    path: Option<String>,
    status: Option<String>,
    read_error: Option<String>,
    exit_code: Option<i64>,
    error_message: Option<String>,
    final_message_bytes: Option<u64>,
    fallback_final_message_written: Option<bool>,
}

impl SpawnCompletionForProcessGone {
    fn end_state(&self) -> AgentEndState {
        match (self.status.as_deref(), self.exit_code) {
            (Some("ok"), Some(0) | None) => AgentEndState::Success,
            (Some("running") | None, _) => AgentEndState::Indeterminate,
            (Some("ok"), Some(_)) => AgentEndState::Error,
            (Some(_), _) => AgentEndState::Error,
        }
    }
}

fn read_spawn_completion_for_process_gone(log_dir: &str) -> SpawnCompletionForProcessGone {
    let path = Path::new(log_dir).join("completion-status.json");
    let path_display = path.display().to_string();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SpawnCompletionForProcessGone {
                path: Some(path_display),
                read_error: Some(format!("read completion-status.json: {error}")),
                ..SpawnCompletionForProcessGone::default()
            };
        }
    };
    let status = match serde_json::from_slice::<Value>(&bytes) {
        Ok(status) => status,
        Err(error) => {
            return SpawnCompletionForProcessGone {
                path: Some(path_display),
                read_error: Some(format!("parse completion-status.json: {error}")),
                ..SpawnCompletionForProcessGone::default()
            };
        }
    };
    SpawnCompletionForProcessGone {
        path: Some(path_display),
        status: status
            .get("status")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        read_error: None,
        exit_code: status.get("exit_code").and_then(Value::as_i64),
        error_message: status
            .get("error_message")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.chars().take(512).collect::<String>()),
        final_message_bytes: status.get("final_message_bytes").and_then(Value::as_u64),
        fallback_final_message_written: status
            .get("fallback_final_message_written")
            .and_then(Value::as_bool),
    }
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
/// `SYNAPSE_AGENT_LIVENESS_SWEEP_MS`, `SYNAPSE_AGENT_RUNAWAY_TOOL_CALLS`,
/// `SYNAPSE_AGENT_UNPROBEABLE_DEAD_AFTER_MS`) and installs them process-wide.
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
#[derive(Clone, Debug, Default)]
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
    fn sweep(
        &mut self,
        now_unix_ms: u64,
        stuck_after_ms: u64,
        unprobeable_dead_after_ms: u64,
        process_alive: &dyn Fn(u32) -> bool,
    ) -> LivenessSweepActions {
        let mut actions = LivenessSweepActions::default();
        for entry in self.agents.values_mut() {
            if entry.state == AgentLifecycleState::Dead {
                continue;
            }
            // Process-alive cross-check applies to every live state: a kill
            // that never produced an exit event must still surface.
            if let Some(pid) = entry.probe_pid()
                && !process_alive(pid)
            {
                actions
                    .terminal_events
                    .push(process_gone_terminal_event(entry, pid, now_unix_ms));
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
                    actions.transitions.push(force_transition(
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
                    actions.transitions.push(force_transition(
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
                if is_ambient_without_process_handle(entry) {
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
            actions.transitions.push(force_transition(
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
        actions
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

fn transition_pipeline() -> &'static Mutex<()> {
    static PIPELINE: OnceLock<Mutex<()>> = OnceLock::new();
    PIPELINE.get_or_init(|| Mutex::new(()))
}

/// Exact owner for the global agent-transition linearization boundary. It is
/// exposed only to side-effect executors that must keep the boundary across an
/// immediate physical operation on the same thread.
pub(crate) struct TransitionPipelineGuard {
    _guard: MutexGuard<'static, ()>,
}

pub(crate) fn acquire_transition_pipeline_lock() -> Result<TransitionPipelineGuard, String> {
    transition_pipeline()
        .lock()
        .map(|guard| TransitionPipelineGuard { _guard: guard })
        .map_err(|poisoned| format!("agent transition pipeline lock poisoned: {poisoned}"))
}

pub(crate) fn with_transition_pipeline_lock<T>(operation: impl FnOnce() -> T) -> Result<T, String> {
    let _guard = acquire_transition_pipeline_lock()?;
    Ok(operation())
}

static EVENT_BUS: OnceLock<EventBus> = OnceLock::new();

/// Installs the SSE event bus so transitions reach live dashboards. Called
/// once during HTTP transport startup; later calls are ignored.
pub(crate) fn install_event_bus(bus: EventBus) {
    let _already_installed = EVENT_BUS.set(bus);
}

/// Atomically journals primary events, all derived state transitions, and the
/// final per-anchor Pending escalation cursor before publishing any in-memory
/// state. The tracker is staged on a clone and swapped only after independent
/// physical readback proves the complete Calyx transaction.
pub(crate) fn record_agent_events_transactionally(
    db: &Db,
    records: &[AgentEventRecord],
) -> StorageResult<Vec<AgentEventWriteReadback>> {
    for record in records {
        let _encoded = super::agent_events::validate_and_encode(record)?;
    }
    let pipeline_guard = transition_pipeline().lock().map_err(|poisoned| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: cf::CF_AGENT_EVENTS.to_owned(),
            detail: format!(
                "AGENT_STATE_TRANSITION_PIPELINE_POISONED: atomic journal/projection coordinator is unavailable: {poisoned}"
            ),
        }
    })?;
    let mut live = tracker().lock().map_err(|poisoned| {
        synapse_storage::StorageError::WriteFailed {
            cf_name: cf::CF_AGENT_EVENTS.to_owned(),
            detail: format!(
                "AGENT_STATE_TRACKER_POISONED: cannot stage atomic journal projection: {poisoned}"
            ),
        }
    })?;
    let mut candidate = live.clone();
    let mut staged_transitions = Vec::<(usize, StateTransition)>::new();
    for (record_index, record) in records.iter().enumerate() {
        if !is_state_machine_row(record)
            && let Some(transition) = candidate.apply_event(record)
        {
            staged_transitions.push((record_index, transition));
        }
    }

    let now_ns = unix_time_ns_now();
    let mut per_anchor_floor = BTreeMap::<String, u64>::new();
    let mut transition_rows = Vec::with_capacity(staged_transitions.len());
    for (trigger_index, transition) in &staged_transitions {
        let floor = match per_anchor_floor.get(&transition.anchor).copied() {
            Some(floor) => floor,
            None => super::escalation::projection_generation_for_anchor(db, &transition.anchor)
                .map_err(|error| synapse_storage::StorageError::ReadFailed {
                    cf_name: cf::CF_KV.to_owned(),
                    detail: format!(
                        "AGENT_STATE_PROJECTION_CURSOR_READ_FAILED: anchor={:?} detail={}",
                        transition.anchor, error.message
                    ),
                })?
                .map(|generation| generation.journal_ts_ns)
                .unwrap_or_default(),
        };
        let proposed = now_ns.max(records[*trigger_index].ts_ns);
        let transition_ts_ns = if proposed > floor {
            proposed
        } else {
            floor.checked_add(1).ok_or_else(|| {
                synapse_storage::StorageError::WriteFailed {
                    cf_name: cf::CF_AGENT_EVENTS.to_owned(),
                    detail: format!(
                        "AGENT_STATE_GENERATION_EXHAUSTED: anchor={:?} durable journal timestamp reached u64::MAX",
                        transition.anchor
                    ),
                }
            })?
        };
        per_anchor_floor.insert(transition.anchor.clone(), transition_ts_ns);
        transition_rows.push(transition_record(transition, transition_ts_ns));
    }

    let primary_count = records.len();
    let mut combined_records = records.to_vec();
    combined_records.extend(transition_rows);
    let mut final_transition_by_anchor = BTreeMap::<String, (usize, StateTransition)>::new();
    for (transition_index, (_trigger_index, transition)) in staged_transitions.iter().enumerate() {
        final_transition_by_anchor.insert(
            transition.anchor.clone(),
            (primary_count + transition_index, transition.clone()),
        );
    }
    let intents = final_transition_by_anchor
        .values()
        .map(|(record_index, transition)| TransitionJournalIntent {
            record_index: *record_index,
            transition: transition.clone(),
        })
        .collect::<Vec<_>>();
    let committed =
        commit_agent_event_records_with_intents(db, &combined_records, &intents).inspect_err(
            |error| {
                tracing::error!(
                    code = "AGENT_EVENT_WRITE_FAILED",
                    primary_record_count = records.len(),
                    transition_count = staged_transitions.len(),
                    cursor_count = intents.len(),
                    detail = %error,
                    "atomic primary-event/state-transition/projection-cursor commit failed; live tracker was not advanced"
                );
            },
        )?;
    *live = candidate;
    drop(live);
    drop(pipeline_guard);

    project_committed_agent_event_artifacts(db, &committed);
    publish_committed_transitions(
        db,
        &staged_transitions,
        &final_transition_by_anchor,
        primary_count,
        &committed.readbacks,
    );
    Ok(committed.readbacks[..primary_count].to_vec())
}

/// One liveness pass over the process-wide tracker: process probes + silence
/// thresholds. Returns the number of transitions emitted.
pub(crate) fn liveness_sweep_once(db: &Db, now_unix_ms: u64) -> usize {
    let config = liveness_config();
    let pipeline_guard = match transition_pipeline().lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!(
                code = "AGENT_STATE_TRANSITION_PIPELINE_POISONED",
                detail = %poisoned,
                "liveness sweep could not acquire the atomic transition pipeline"
            );
            return 0;
        }
    };
    let mut live = match tracker().lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!(
                code = "AGENT_STATE_TRACKER_POISONED",
                detail = %poisoned,
                "agent state tracker lock poisoned; liveness sweep skipped"
            );
            return 0;
        }
    };
    let mut candidate = live.clone();
    let actions = candidate.sweep(
        now_unix_ms,
        config.stuck_after_ms,
        config.unprobeable_dead_after_ms,
        &|pid| crate::m4::process_exists(pid),
    );
    let mut transitions = actions.transitions;
    for event in &actions.terminal_events {
        if let Some(transition) = candidate.apply_event(event) {
            transitions.push(transition);
        }
    }
    if actions.terminal_events.is_empty() && transitions.is_empty() {
        *live = candidate;
        return 0;
    }
    for record in &actions.terminal_events {
        if let Err(error) = super::agent_events::validate_and_encode(record) {
            tracing::error!(
                code = "AGENT_STATE_TERMINAL_EVENT_WRITE_FAILED",
                detail = %error,
                "liveness terminal event failed validation; staged tracker was discarded"
            );
            return 0;
        }
    }

    let now_ns = now_unix_ms.saturating_mul(1_000_000);
    let mut per_anchor_floor = BTreeMap::<String, u64>::new();
    let mut transition_rows = Vec::with_capacity(transitions.len());
    for transition in &transitions {
        let floor = match per_anchor_floor.get(&transition.anchor).copied() {
            Some(floor) => floor,
            None => {
                match super::escalation::projection_generation_for_anchor(db, &transition.anchor) {
                    Ok(generation) => generation
                        .map(|generation| generation.journal_ts_ns)
                        .unwrap_or_default(),
                    Err(error) => {
                        tracing::error!(
                            code = "AGENT_STATE_PROJECTION_CURSOR_READ_FAILED",
                            anchor = %transition.anchor,
                            detail = %error.message,
                            "liveness sweep could not stage a durable transition generation"
                        );
                        return 0;
                    }
                }
            }
        };
        let transition_ts_ns = if now_ns > floor {
            now_ns
        } else if let Some(next) = floor.checked_add(1) {
            next
        } else {
            tracing::error!(
                code = "AGENT_STATE_GENERATION_EXHAUSTED",
                anchor = %transition.anchor,
                "liveness transition generation reached u64::MAX"
            );
            return 0;
        };
        per_anchor_floor.insert(transition.anchor.clone(), transition_ts_ns);
        transition_rows.push(transition_record(transition, transition_ts_ns));
    }
    let primary_count = actions.terminal_events.len();
    let mut combined_records = actions.terminal_events;
    combined_records.extend(transition_rows);
    let mut final_transition_by_anchor = BTreeMap::<String, (usize, StateTransition)>::new();
    for (transition_index, transition) in transitions.iter().enumerate() {
        final_transition_by_anchor.insert(
            transition.anchor.clone(),
            (primary_count + transition_index, transition.clone()),
        );
    }
    let intents = final_transition_by_anchor
        .values()
        .map(|(record_index, transition)| TransitionJournalIntent {
            record_index: *record_index,
            transition: transition.clone(),
        })
        .collect::<Vec<_>>();
    let committed = match commit_agent_event_records_with_intents(db, &combined_records, &intents) {
        Ok(committed) => committed,
        Err(error) => {
            tracing::error!(
                code = "AGENT_STATE_TERMINAL_EVENT_WRITE_FAILED",
                terminal_event_count = primary_count,
                transition_count = transitions.len(),
                detail = %error,
                "liveness journal/transition/cursor transaction failed; staged tracker was discarded for retry"
            );
            return 0;
        }
    };
    *live = candidate;
    drop(live);
    drop(pipeline_guard);
    project_committed_agent_event_artifacts(db, &committed);
    let staged = transitions
        .into_iter()
        .map(|transition| (0_usize, transition))
        .collect::<Vec<_>>();
    publish_committed_transitions(
        db,
        &staged,
        &final_transition_by_anchor,
        primary_count,
        &committed.readbacks,
    );
    primary_count.saturating_add(staged.len())
}

/// Publishes transition side effects only after the primary rows, derived
/// transition rows, and final Pending cursors have committed atomically and
/// been independently read back.
fn publish_committed_transitions(
    db: &Db,
    transitions: &[(usize, StateTransition)],
    final_transition_by_anchor: &BTreeMap<String, (usize, StateTransition)>,
    primary_count: usize,
    readbacks: &[AgentEventWriteReadback],
) {
    if transitions.is_empty() {
        return;
    }
    for (transition_index, (_trigger_index, transition)) in transitions.iter().enumerate() {
        let Some(readback) = readbacks.get(primary_count + transition_index) else {
            tracing::error!(
                code = "AGENT_STATE_COMMIT_READBACK_INVALID",
                transition_index,
                primary_count,
                readback_count = readbacks.len(),
                "committed transition has no matching journal readback; projection publication stopped"
            );
            return;
        };
        tracing::info!(
            code = "AGENT_STATE_CHANGED",
            anchor = %transition.anchor,
            state_from = transition.state_from.as_str(),
            state_to = transition.state_to.as_str(),
            reason_code = %transition.reason_code,
            runaway = transition.runaway,
            journal_ts_ns = readback.ts_ns,
            seq = readback.seq,
            committed_seq = readback.committed_seq,
            committed_revision = %synapse_storage::constellations::hex_encode(&readback.committed_revision_sha256),
            journal_key = %synapse_storage::constellations::hex_encode(&readback.key),
            "readback=CF_AGENT_EVENTS edge=state_machine"
        );
    }
    // Only the final transition for an anchor is externally projected from a
    // multi-event batch. Every intermediate transition remains in the
    // append-only journal, while the atomic cursor names the final reality.
    for (record_index, transition) in final_transition_by_anchor.values() {
        let Some(readback) = readbacks.get(*record_index) else {
            tracing::error!(
                code = "AGENT_STATE_COMMIT_READBACK_INVALID",
                anchor = %transition.anchor,
                record_index,
                readback_count = readbacks.len(),
                "final transition cursor has no matching journal readback"
            );
            continue;
        };
        super::escalation::note_transition(
            db,
            transition,
            readback.ts_ns,
            readback.seq,
            readback.ts_ns / 1_000_000,
        );
    }
    if let Some(bus) = EVENT_BUS.get() {
        for (_trigger_index, transition) in transitions {
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
        "anchor": transition.anchor,
        "waiting_for": transition.waiting_for,
        "runaway": transition.runaway,
        "evidence": transition.evidence,
    });
    record
}

pub(crate) fn validate_transition_record_identity(
    record: &AgentEventRecord,
    transition: &StateTransition,
) -> StorageResult<()> {
    let expected = transition_record(transition, record.ts_ns);
    let actual_bytes = synapse_storage::encode_json(record)?;
    let expected_bytes = synapse_storage::encode_json(&expected)?;
    if actual_bytes != expected_bytes {
        return Err(synapse_storage::StorageError::WriteFailed {
            cf_name: cf::CF_AGENT_EVENTS.to_owned(),
            detail: format!(
                "AGENT_STATE_TRANSITION_RECORD_MISMATCH: anchor={:?} state_from={} state_to={} reason_code={:?}; refusing to bind a cursor to non-identical journal bytes",
                transition.anchor,
                transition.state_from.as_str(),
                transition.state_to.as_str(),
                transition.reason_code
            ),
        });
    }
    Ok(())
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

/// One exact physical `CF_AGENT_EVENTS` row supplied by the ambient
/// transactional-outbox reconciler. `value` is the canonical encoded event
/// bytes read from `key`; recovery independently point-reads both again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AmbientExactJournalRow {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// Readback from a quiet live-projection recovery. Recovery never appends a
/// primary or derived journal row and never publishes transition side effects.
#[derive(Clone, Debug)]
pub(crate) struct AmbientProjectionRecoveryReadback {
    pub readback: AgentStateRead,
    pub rows_applied: usize,
    pub already_current: bool,
}

fn ambient_recovery_read_error(detail: impl Into<String>) -> StorageError {
    StorageError::ReadFailed {
        cf_name: cf::CF_AGENT_EVENTS.to_owned(),
        detail: detail.into(),
    }
}

fn ambient_agent_event_retention_ns() -> StorageResult<u64> {
    let retention = RETENTION_DEFAULTS
        .iter()
        .find(|retention| retention.cf == cf::CF_AGENT_EVENTS)
        .ok_or_else(|| {
            ambient_recovery_read_error(
                "AGENT_STATE_AMBIENT_RECOVERY_RETENTION_MISSING: CF_AGENT_EVENTS has no retention default; remediation=restore the finite journal retention contract before witness-only recovery",
            )
        })?;
    let hours = match retention.ttl {
        RetentionTtl::Hours(hours) => hours,
        RetentionTtl::Days(days) => days.checked_mul(24).ok_or_else(|| {
            ambient_recovery_read_error(
                "AGENT_STATE_AMBIENT_RECOVERY_RETENTION_OVERFLOW: CF_AGENT_EVENTS day retention does not fit hours; remediation=repair the retention default",
            )
        })?,
        RetentionTtl::None | RetentionTtl::LruOnly => {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_RETENTION_INVALID: CF_AGENT_EVENTS retention is {:?}; remediation=configure a finite time retention before witness-only recovery",
                retention.ttl
            )));
        }
    };
    hours
        .checked_mul(60 * 60)
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .ok_or_else(|| {
            ambient_recovery_read_error(
                "AGENT_STATE_AMBIENT_RECOVERY_RETENTION_OVERFLOW: CF_AGENT_EVENTS retention does not fit nanoseconds; remediation=repair the retention default",
            )
        })
}

fn point_read_exact_ambient_journal_row(
    db: &Db,
    spawn_id: &str,
    row: &AmbientExactJournalRow,
) -> StorageResult<[u8; 32]> {
    let key_hex = synapse_storage::constellations::hex_encode(&row.key);
    let revisioned = db
        .get_cf_revisioned(cf::CF_AGENT_EVENTS, &row.key)
        .map_err(|error| {
            ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_POINT_READ_FAILED: spawn_id={spawn_id} key_hex={key_hex}: {error}; remediation=repair the exact Calyx journal point-read before retrying projection recovery"
            ))
        })?
        .ok_or_else(|| {
            ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_ROW_MISSING: spawn_id={spawn_id} key_hex={key_hex}; remediation=restore/reconcile the exact retained journal row and leave the ambient outbox unacknowledged"
            ))
        })?;
    let actual = revisioned.value.ok_or_else(|| {
        ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_ROW_EXPIRED: spawn_id={spawn_id} key_hex={key_hex}; remediation=leave the ambient outbox unacknowledged and reconcile the retention-expired operation from durable evidence"
        ))
    })?;
    if actual != row.value {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_ROW_DIVERGED: spawn_id={spawn_id} key_hex={key_hex} expected_value_sha256={} actual_value_sha256={}; remediation=quarantine and repair the divergent journal row before projection recovery",
            synapse_storage::constellations::sha256_hex(&row.value),
            synapse_storage::constellations::sha256_hex(&actual)
        )));
    }
    Ok(revisioned.revision_sha256)
}

fn ambient_projection_journal_witness(
    db: &Db,
    spawn_id: &str,
) -> StorageResult<Option<super::escalation::TransitionProjectionJournalWitness>> {
    super::escalation::projection_journal_witness_for_anchor(db, spawn_id).map_err(|error| {
        ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_PROJECTION_WITNESS_READ_FAILED: spawn_id={spawn_id}: {}; remediation=repair the Applied transition projection cursor, Pending index, and exact audit evidence before recovery",
            error.message
        ))
    })
}

fn ambient_authoritative_projection_row(
    spawn_id: &str,
    witness: &super::escalation::TransitionProjectionJournalWitness,
) -> StorageResult<AmbientExactJournalRow> {
    let (key_ts_ns, key_seq) = decode_agent_event_key(&witness.journal_key)?;
    let key_hex = synapse_storage::constellations::hex_encode(&witness.journal_key);
    let record: AgentEventRecord = decode_json(&witness.journal_value)?;
    let canonical = super::agent_events::validate_and_encode(&record)?;
    if key_ts_ns != witness.generation.journal_ts_ns
        || key_seq != witness.generation.journal_seq
        || canonical != witness.journal_value
        || record.ts_ns != witness.generation.journal_ts_ns
        || record.spawn_id.as_deref() != Some(spawn_id)
        || !is_state_machine_row(&record)
        || record
            .state_to
            .as_deref()
            .and_then(AgentLifecycleState::parse)
            .is_none()
    {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_PROJECTION_WITNESS_INVALID: spawn_id={spawn_id} generation=({},{}) key_hex={key_hex} key_generation=({key_ts_ns},{key_seq}) record_ts_ns={} record_spawn_id={:?} kind={:?} state_to={:?} canonical_bytes_match={} machine_row={}; remediation=repair the validated Applied projection witness before recovery",
            witness.generation.journal_ts_ns,
            witness.generation.journal_seq,
            record.ts_ns,
            record.spawn_id,
            record.kind,
            record.state_to,
            canonical == witness.journal_value,
            is_state_machine_row(&record)
        )));
    }
    Ok(AmbientExactJournalRow {
        key: witness.journal_key.clone(),
        value: witness.journal_value.clone(),
    })
}

fn point_read_optional_ambient_authoritative_row(
    db: &Db,
    spawn_id: &str,
    row: &AmbientExactJournalRow,
) -> StorageResult<Option<[u8; 32]>> {
    let key_hex = synapse_storage::constellations::hex_encode(&row.key);
    let Some(revisioned) = db
        .get_cf_revisioned(cf::CF_AGENT_EVENTS, &row.key)
        .map_err(|error| {
            ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_AUTHORITATIVE_ROW_READ_FAILED: spawn_id={spawn_id} key_hex={key_hex}: {error}; remediation=repair the exact transition journal point-read before recovery"
            ))
        })?
    else {
        let (journal_ts_ns, _journal_seq) = decode_agent_event_key(&row.key)?;
        let retention_ns = ambient_agent_event_retention_ns()?;
        let now_ns = unix_time_ns_now();
        let age_ns = now_ns.saturating_sub(journal_ts_ns);
        if age_ns < retention_ns {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_AUTHORITATIVE_ROW_MISSING_BEFORE_RETENTION: spawn_id={spawn_id} key_hex={key_hex} journal_ts_ns={journal_ts_ns} now_ns={now_ns} age_ns={age_ns} retention_ns={retention_ns}; remediation=repair the prematurely missing physical transition row instead of masking it with the durable witness"
            )));
        }
        return Ok(None);
    };
    let Some(actual) = revisioned.value else {
        // `get_cf_revisioned` exposes `value=None` only for a logically expired
        // retention envelope. The validated Applied projection cursor is the
        // durable exact witness once that older journal value retires.
        return Ok(None);
    };
    if actual != row.value {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_AUTHORITATIVE_ROW_DIVERGED: spawn_id={spawn_id} key_hex={key_hex} expected_value_sha256={} actual_value_sha256={}; remediation=quarantine and repair the live journal row that diverges from the Applied projection witness",
            synapse_storage::constellations::sha256_hex(&row.value),
            synapse_storage::constellations::sha256_hex(&actual)
        )));
    }
    Ok(Some(revisioned.revision_sha256))
}

/// Quietly installs the lifecycle projection of an already committed ambient
/// outbox batch into the process-wide tracker (#1772).
///
/// This is deliberately not a general replay API. It accepts only the bounded
/// one-operation ambient shape, requires strictly ordered journal keys, rejects
/// machine-derived rows, validates canonical event bytes and spawn identity,
/// joins the anchor's latest Applied transition watermark/audit witness,
/// point-reads every live physical row before and after ordered reduction, and
/// requires the durable witness to remain byte/revision-identical when its
/// older journal row has retired. It takes the normal transition-pipeline ->
/// tracker lock order, and swaps the candidate only after the second exact
/// evidence read. No journal write, cursor write, SSE publication, or
/// transition side effect occurs here.
pub(crate) fn recover_ambient_projection_from_exact_journal_rows(
    db: &Db,
    spawn_id: &str,
    rows: &[AmbientExactJournalRow],
) -> StorageResult<AmbientProjectionRecoveryReadback> {
    if spawn_id.trim().is_empty() || rows.is_empty() || rows.len() > MAX_AMBIENT_EXACT_RECOVERY_ROWS
    {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_INPUT_INVALID: spawn_id={spawn_id:?} rows={} max_rows={MAX_AMBIENT_EXACT_RECOVERY_ROWS}; remediation=provide one exact bounded ambient outbox batch",
            rows.len()
        )));
    }

    let mut records = Vec::with_capacity(rows.len());
    let mut previous_key: Option<&[u8]> = None;
    for row in rows {
        if previous_key.is_some_and(|previous| previous >= row.key.as_slice()) {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_ORDER_INVALID: spawn_id={spawn_id} journal keys are not strictly ascending; remediation=repeat the exact ordered CF_AGENT_EVENTS scan"
            )));
        }
        let (key_ts_ns, _key_seq) = decode_agent_event_key(&row.key)?;
        let record: AgentEventRecord = decode_json(&row.value)?;
        let canonical = super::agent_events::validate_and_encode(&record)?;
        if canonical != row.value
            || record.ts_ns != key_ts_ns
            || record.spawn_id.as_deref() != Some(spawn_id)
            || is_state_machine_row(&record)
        {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_IDENTITY_INVALID: spawn_id={spawn_id} key_hex={} key_ts_ns={key_ts_ns} record_ts_ns={} record_spawn_id={:?} kind={:?} canonical_bytes_match={} machine_row={}; remediation=repair the exact ambient primary-row set before projection recovery",
                synapse_storage::constellations::hex_encode(&row.key),
                record.ts_ns,
                record.spawn_id,
                record.kind,
                canonical == row.value,
                is_state_machine_row(&record)
            )));
        }
        previous_key = Some(&row.key);
        records.push(record);
    }
    let expected_last = records.last().ok_or_else(|| {
        ambient_recovery_read_error(
            "AGENT_STATE_AMBIENT_RECOVERY_INPUT_INVALID: validated record set became empty",
        )
    })?;
    let expected_last_event_unix_ms = expected_last.ts_ns / 1_000_000;
    let expected_last_event_kind = expected_last.kind;
    let now_unix_ms = unix_time_ns_now() / 1_000_000;

    let _pipeline_guard = transition_pipeline().lock().map_err(|poisoned| {
        ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_PIPELINE_POISONED: spawn_id={spawn_id}: {poisoned}; remediation=restart the daemon before exact projection recovery"
        ))
    })?;
    let mut live = tracker().lock().map_err(|poisoned| {
        ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_TRACKER_POISONED: spawn_id={spawn_id}: {poisoned}; remediation=restart the daemon before exact projection recovery"
        ))
    })?;
    let first_projection_witness = ambient_projection_journal_witness(db, spawn_id)?;
    let authoritative_row = first_projection_witness
        .as_ref()
        .map(|witness| ambient_authoritative_projection_row(spawn_id, witness))
        .transpose()?;
    let mut recovery_rows = rows.to_vec();
    if let Some(authoritative) = &authoritative_row {
        recovery_rows.push(authoritative.clone());
    }
    recovery_rows.sort_by(|left, right| left.key.cmp(&right.key));
    if recovery_rows
        .windows(2)
        .any(|window| window[0].key == window[1].key)
    {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_KEY_DUPLICATE: spawn_id={spawn_id}; the outbox batch overlaps its authoritative projection generation; remediation=repair the cursor/journal key identities before recovery"
        )));
    }
    let recovery_records = recovery_rows
        .iter()
        .map(|row| decode_json::<AgentEventRecord>(&row.value))
        .collect::<StorageResult<Vec<_>>>()?;
    let expected_authoritative_state = recovery_records
        .iter()
        .rev()
        .find(|record| is_state_machine_row(record))
        .and_then(|record| {
            record
                .state_to
                .as_deref()
                .and_then(AgentLifecycleState::parse)
        });
    if authoritative_row.is_some() != expected_authoritative_state.is_some() {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_AUTHORITATIVE_STATE_MISSING: spawn_id={spawn_id} witness_present={} parsed_state={expected_authoritative_state:?}; remediation=repair the Applied transition witness before recovery",
            authoritative_row.is_some()
        )));
    }
    let first_primary_revisions = rows
        .iter()
        .map(|row| point_read_exact_ambient_journal_row(db, spawn_id, row))
        .collect::<StorageResult<Vec<_>>>()?;
    let first_authoritative_revision = match &authoritative_row {
        Some(row) => point_read_optional_ambient_authoritative_row(db, spawn_id, row)?,
        None => None,
    };

    let current = live.read_for_session(spawn_id, now_unix_ms);
    if let (Some(readback), Some(expected_state)) = (current.as_ref(), expected_authoritative_state)
        && readback.last_event_unix_ms >= expected_last_event_unix_ms
        && readback.state != expected_state
    {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_LIVE_STATE_DIVERGED: spawn_id={spawn_id} expected_authoritative_state={} actual_state={} actual_last_event_unix_ms={}; remediation=rebuild the singleton from the stable Applied transition witness before acknowledging the ambient operation",
            expected_state.as_str(),
            readback.state.as_str(),
            readback.last_event_unix_ms
        )));
    }
    let already_current = match current.as_ref() {
        Some(readback) if readback.last_event_unix_ms > expected_last_event_unix_ms => true,
        Some(readback) if readback.last_event_unix_ms == expected_last_event_unix_ms => {
            if expected_authoritative_state.is_none()
                && readback.last_event_kind != expected_last_event_kind
            {
                return Err(ambient_recovery_read_error(format!(
                    "AGENT_STATE_AMBIENT_RECOVERY_GENERATION_AMBIGUOUS: spawn_id={spawn_id} expected_last_event_unix_ms={expected_last_event_unix_ms} expected_last_event_kind={expected_last_event_kind:?} actual_last_event_kind={:?}; remediation=rebuild the complete ordered agent stream instead of applying an ambiguous same-millisecond fragment",
                    readback.last_event_kind
                )));
            }
            true
        }
        _ => false,
    };

    let mut candidate = live.clone();
    let rows_applied = if already_current {
        0
    } else {
        for record in &recovery_records {
            if is_state_machine_row(record) {
                candidate.apply_authoritative(record);
            } else {
                let _quiet_transition = candidate.apply_event(record);
            }
        }
        let staged = candidate
            .read_for_session(spawn_id, now_unix_ms)
            .ok_or_else(|| {
                ambient_recovery_read_error(format!(
                    "AGENT_STATE_AMBIENT_RECOVERY_STAGED_PROJECTION_MISSING: spawn_id={spawn_id}; remediation=repair the reducer/ambient identity contract before retrying"
                ))
            })?;
        if staged.last_event_unix_ms < expected_last_event_unix_ms
            || expected_authoritative_state
                .is_some_and(|expected_state| staged.state != expected_state)
            || (expected_authoritative_state.is_none()
                && staged.last_event_unix_ms == expected_last_event_unix_ms
                && staged.last_event_kind != expected_last_event_kind)
        {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_STAGED_PROJECTION_MISMATCH: spawn_id={spawn_id} expected_last_event_unix_ms={expected_last_event_unix_ms} expected_last_event_kind={expected_last_event_kind:?} expected_authoritative_state={:?} actual_state={} actual_last_event_unix_ms={} actual_last_event_kind={:?}; remediation=repair the reducer before installing this projection",
                expected_authoritative_state.map(AgentLifecycleState::as_str),
                staged.state.as_str(),
                staged.last_event_unix_ms,
                staged.last_event_kind
            )));
        }
        recovery_records.len()
    };

    for (row, first_revision) in rows.iter().zip(&first_primary_revisions) {
        let second_revision = point_read_exact_ambient_journal_row(db, spawn_id, row)?;
        if &second_revision != first_revision {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_ROW_REVISION_CHANGED: spawn_id={spawn_id} key_hex={} first_revision_sha256={} second_revision_sha256={}; remediation=leave the outbox unacknowledged and reconcile the changed physical journal row",
                synapse_storage::constellations::hex_encode(&row.key),
                synapse_storage::constellations::hex_encode(first_revision),
                synapse_storage::constellations::hex_encode(&second_revision)
            )));
        }
    }
    if let Some(authoritative) = &authoritative_row {
        let second_authoritative_revision =
            point_read_optional_ambient_authoritative_row(db, spawn_id, authoritative)?;
        if second_authoritative_revision != first_authoritative_revision {
            return Err(ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_AUTHORITATIVE_ROW_REVISION_CHANGED: spawn_id={spawn_id} key_hex={} first_revision_sha256={} second_revision_sha256={}; remediation=leave the outbox unacknowledged and retry only after the physical authoritative-row retention boundary is stable",
                synapse_storage::constellations::hex_encode(&authoritative.key),
                first_authoritative_revision.map_or_else(
                    || "witness_only".to_owned(),
                    |revision| synapse_storage::constellations::hex_encode(&revision)
                ),
                second_authoritative_revision.map_or_else(
                    || "witness_only".to_owned(),
                    |revision| synapse_storage::constellations::hex_encode(&revision)
                )
            )));
        }
    }
    let second_projection_witness = ambient_projection_journal_witness(db, spawn_id)?;
    if second_projection_witness != first_projection_witness {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_PROJECTION_WITNESS_CHANGED: spawn_id={spawn_id} first_revision_sha256={} second_revision_sha256={}; remediation=leave the outbox unacknowledged and repeat recovery from one stable Applied transition projection generation",
            first_projection_witness.as_ref().map_or_else(
                || "absent".to_owned(),
                |witness| synapse_storage::constellations::hex_encode(
                    &witness.watermark_revision_sha256
                )
            ),
            second_projection_witness.as_ref().map_or_else(
                || "absent".to_owned(),
                |witness| synapse_storage::constellations::hex_encode(
                    &witness.watermark_revision_sha256
                )
            )
        )));
    }
    if !already_current {
        *live = candidate;
    }
    drop(live);

    // Separate singleton read operation while the transition pipeline remains
    // held prevents another writer from making the recovery verdict ambiguous.
    let readback = tracker()
        .lock()
        .map_err(|poisoned| {
            ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_READBACK_LOCK_POISONED: spawn_id={spawn_id}: {poisoned}; remediation=restart the daemon and repeat exact recovery"
            ))
        })?
        .read_for_session(spawn_id, now_unix_ms)
        .ok_or_else(|| {
            ambient_recovery_read_error(format!(
                "AGENT_STATE_AMBIENT_RECOVERY_READBACK_MISSING: spawn_id={spawn_id}; remediation=leave the outbox unacknowledged and repair the live singleton projection"
            ))
        })?;
    if readback.last_event_unix_ms < expected_last_event_unix_ms
        || expected_authoritative_state
            .is_some_and(|expected_state| readback.state != expected_state)
        || (expected_authoritative_state.is_none()
            && readback.last_event_unix_ms == expected_last_event_unix_ms
            && readback.last_event_kind != expected_last_event_kind)
    {
        return Err(ambient_recovery_read_error(format!(
            "AGENT_STATE_AMBIENT_RECOVERY_READBACK_MISMATCH: spawn_id={spawn_id} expected_last_event_unix_ms={expected_last_event_unix_ms} expected_last_event_kind={expected_last_event_kind:?} expected_authoritative_state={:?} actual_state={} actual_last_event_unix_ms={} actual_last_event_kind={:?}; remediation=leave the outbox unacknowledged and repair the singleton projection",
            expected_authoritative_state.map(AgentLifecycleState::as_str),
            readback.state.as_str(),
            readback.last_event_unix_ms,
            readback.last_event_kind
        )));
    }
    tracing::warn!(
        code = "AGENT_STATE_AMBIENT_PROJECTION_RECOVERED",
        spawn_id,
        rows_verified = recovery_rows.len(),
        primary_rows_verified = rows.len(),
        authoritative_witness_present = first_projection_witness.is_some(),
        authoritative_physical_row_present = first_authoritative_revision.is_some(),
        rows_applied,
        already_current,
        projected_state = readback.state.as_str(),
        projected_last_event_unix_ms = readback.last_event_unix_ms,
        projected_last_event_kind = ?readback.last_event_kind,
        "readback=CF_AGENT_EVENTS+AgentStateTracker edge=ambient_exact_projection_recovery"
    );
    Ok(AmbientProjectionRecoveryReadback {
        readback,
        rows_applied,
        already_current,
    })
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
