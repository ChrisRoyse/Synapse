//! Durable agent task queue (#910).
//!
//! A crash-safe work queue agents are dispatched from — the board the fleet
//! kanban renders. Tasks live in `CF_KV` (same durable handle as templates #909
//! and the mailbox #908); each `task_*` mutation flushes so a row is on disk and
//! visible to the Calyx vault read path before the tool returns (config/operational
//! state must be read-after-write consistent, never left in the batcher's
//! pending queue — see [[storage-batcher-and-winevent-truth]]).
//!
//! State machine (Vibe-Kanban-style): `todo → in_progress → review → done`, with
//! `cancelled` reachable from any non-terminal state and an explicit re-queue
//! back to `todo`. Invalid transitions are a structured error, never a silent
//! no-op.
//!
//! Dispatch ordering follows Temporal's priority+fairness model:
//!  1. **Priority tier (strict)** — a lower `priority` number dispatches first
//!     (1 = highest, 5 = lowest).
//!  2. **Per-template fairness** — within the top tier, the template with the
//!     fewest in-flight attempts is chosen (join-shortest-queue), so one greedy
//!     template cannot starve the fleet.
//!  3. **FIFO within a template** — ties break by enqueue order.
//!
//! **Attempts**: each claim/dispatch appends an attempt linked to the agent's
//! session, so parallel attempts (different templates) are recorded and the UI
//! can compare-and-pick. **Crash safety**: `task_reconcile` (run explicitly and
//! lazily on `task_list`/`task_dispatch_once`) checks every `in_progress` task's
//! live attempt against the session registry; a completed spawned agent is
//! settled from its terminal artifact and moved to `review`, while an attempt
//! whose session is gone without terminal evidence is flagged `orphaned` and
//! moved to `review` — never silently re-queued.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use rmcp::{RoleServer, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;
use synapse_storage::{Db, RevisionGuard, cf};

use super::{
    ErrorData, Json, Parameters, SynapseService, mcp_error, session_registry::unix_time_ms_now,
    tool, tool_router,
};
use crate::m4::{
    ActSpawnAgentRequest, default_agent_spawn_hold_open_ms, default_agent_spawn_mcp_url,
    default_agent_spawn_wait_timeout_ms,
};

/// CF_KV key namespace for task rows. Versioned prefix so a format change is a
/// clean re-key, never an in-place migration.
const TASK_NAMESPACE: &str = "agent-task/v1";
const TASK_SCHEMA_VERSION: u32 = 1;
const TASK_SEQUENCE_KEY: &str = "agent-task/v1/meta/last_enqueue_seq";
const TASK_QUEUE_STATE_KEY: &str = "agent-task/v2/meta/queue_state";
const TASK_QUEUE_STATE_SCHEMA_VERSION: u32 = 1;
const TASK_CREATE_MAX_CONFLICT_RETRIES: usize = 64;
const TASK_DISPATCH_RESERVATION_SCHEMA_VERSION: u32 = 1;
const TASK_DISPATCH_RESERVATION_MIN_LEASE_MS: u64 = 15 * 60 * 1_000;
const TASK_DISPATCH_RESERVATION_GRACE_MS: u64 = 5 * 60 * 1_000;

const MAX_TASK_ID_CHARS: usize = 200;
const MAX_TITLE_CHARS: usize = 500;
const MAX_TEXT_CHARS: usize = 16 * 1024;
const MAX_PARAM_VALUE_BYTES: usize = 16 * 1024;
const MIN_PRIORITY: u8 = 1;
const MAX_PRIORITY: u8 = 5;
const MAX_LIST_TASKS: usize = 1000;
const SCAN_CHUNK_ROWS: usize = 4_096;
const TERMINAL_TASK_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const TERMINAL_TASK_RETAIN_ROWS: usize = 5_000;
const DELETE_BATCH_ROWS: usize = 512;
/// Default global cap on concurrently in-flight (`in_progress`) tasks the
/// dispatcher will allow. Operators override per call.
const DEFAULT_CONCURRENCY_CAP: usize = 8;
/// Dashboard dispatches are often approval-gated by a human; keep them above
/// the generic MCP spawn default so permission prompts do not exhaust readback.
const DASHBOARD_TASK_DISPATCH_WAIT_TIMEOUT_MS: u64 = 600_000;

fn task_queue_mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// The lifecycle states a task moves through. `done`/`cancelled` are terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Todo,
    InProgress,
    Review,
    Done,
    Cancelled,
}

impl TaskState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Review => "review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// The explicit transition matrix. Any pair not listed here is rejected with
    /// a structured `AGENT_TASK_INVALID_TRANSITION` error.
    fn can_transition_to(self, to: Self) -> bool {
        // Terminal states are sinks: no outgoing transitions, ever.
        if self.is_terminal() {
            return false;
        }
        matches!(
            (self, to),
            (Self::Todo, Self::InProgress | Self::Cancelled)
                | (
                    Self::InProgress,
                    Self::Review | Self::Done | Self::Cancelled | Self::Todo
                )
                | (
                    Self::Review,
                    Self::Done | Self::Todo | Self::InProgress | Self::Cancelled
                )
        )
    }

    fn allowed_targets(self) -> Vec<&'static str> {
        [
            Self::Todo,
            Self::InProgress,
            Self::Review,
            Self::Done,
            Self::Cancelled,
        ]
        .into_iter()
        .filter(|target| self.can_transition_to(*target))
        .map(Self::as_str)
        .collect()
    }
}

/// The outcome of a single dispatch attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// The attempt is live: its session is expected to be working the task.
    Pending,
    Succeeded,
    Failed,
    /// The attempt's session vanished before completion (found by reconcile).
    Orphaned,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskAttempt {
    /// 1-based index within the task's attempt list.
    pub attempt_id: u32,
    /// MCP session id (or dispatch-spawned session id) bound to this attempt —
    /// the identity reconcile checks against the live session registry.
    pub session_id: String,
    /// Set when the attempt was created by auto-dispatch spawning an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    /// Template version this attempt was dispatched with, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
    pub outcome: AttemptOutcome,
    pub started_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Durable pre-spawn admission owned by one dispatch call. It is persisted in
/// the task row before any agent process can be launched, so it already counts
/// toward the global concurrency cap and survives a daemon/process failure.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskDispatchReservation {
    pub schema_version: u32,
    pub reservation_id: String,
    pub reserved_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

/// The durable task record. One CF_KV row per task, mutated in place (with a
/// flush) — operational state, not versioned config.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentTask {
    pub schema_version: u32,
    pub task_id: String,
    pub state: TaskState,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
    /// 1 (highest) .. 5 (lowest). Strict tier for dispatch ordering.
    pub priority: u8,
    /// The template a dispatcher spawns this task's agent from (also the
    /// fairness key). Required so a task is always dispatchable and auditable.
    pub template_id: String,
    /// Parameters passed to the template at dispatch time.
    #[serde(default)]
    pub template_params: BTreeMap<String, String>,
    /// Global monotonic enqueue sequence — strict FIFO order within a
    /// (priority, template) bucket, stable across restarts.
    pub enqueue_seq: u64,
    /// Queue-wide mutation generation written atomically with this row. Zero
    /// identifies a legacy row that predates guarded post-create mutations.
    #[serde(default)]
    pub mutation_generation: u64,
    pub attempts: Vec<TaskAttempt>,
    /// Present only between guarded dispatch admission and guarded binding (or
    /// failure). Ordinary claim/update/cancel operations reject a live
    /// reservation instead of overwriting its in-flight external effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_reservation: Option<TaskDispatchReservation>,
    /// Set when the task entered `review` for a non-success reason (e.g. an
    /// orphaned attempt), so the attention queue can explain why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_reason: Option<String>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

impl AgentTask {
    /// The live (`Pending`) attempt, if any — the one reconcile validates.
    fn live_attempt(&self) -> Option<&TaskAttempt> {
        self.attempts
            .iter()
            .find(|attempt| attempt.outcome == AttemptOutcome::Pending)
    }

    /// Count of `in_progress` tasks this one contributes (0 or 1) — used by the
    /// fairness selector.
    const fn is_in_flight(&self) -> bool {
        matches!(self.state, TaskState::InProgress)
    }
}

// ---- params / responses ---------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCreateParams {
    /// Stable task id (`[a-z0-9._-]`).
    pub task_id: String,
    pub title: String,
    #[serde(default)]
    #[schemars(default)]
    pub description: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub acceptance: Option<String>,
    /// 1 (highest) .. 5 (lowest).
    #[serde(default = "default_priority")]
    #[schemars(default = "default_priority", range(min = 1, max = 5))]
    pub priority: u8,
    /// Template to dispatch this task's agent from (must exist at dispatch time).
    pub template_id: String,
    #[serde(default)]
    #[schemars(default)]
    pub template_params: BTreeMap<String, String>,
}

fn default_priority() -> u8 {
    3
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskMutationResponse {
    pub ok: bool,
    pub task: AgentTask,
    pub written_row: TaskRowReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskRowReadback {
    pub cf_name: String,
    pub row_key: String,
    pub value_len_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskIdParams {
    pub task_id: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskGetResponse {
    pub ok: bool,
    pub task: AgentTask,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdateParams {
    pub task_id: String,
    /// Move the task to this state (validated against the transition matrix).
    #[serde(default)]
    #[schemars(default)]
    pub state: Option<TaskState>,
    /// Optional reason recorded for the transition (e.g. why it was re-queued).
    #[serde(default)]
    #[schemars(default)]
    pub reason: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    #[schemars(default)]
    pub title: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub description: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub acceptance: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskClaimParams {
    pub task_id: String,
    /// Session id of the agent claiming the task. The attempt is bound to it so
    /// reconcile can detect if that agent later vanishes.
    pub session_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCancelParams {
    pub task_id: String,
    #[serde(default)]
    #[schemars(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListParams {
    /// Optional state filter.
    #[serde(default)]
    #[schemars(default)]
    pub state: Option<TaskState>,
    #[serde(default = "default_max_list")]
    #[schemars(default = "default_max_list", range(min = 1, max = 1000))]
    pub max: usize,
}

fn default_max_list() -> usize {
    MAX_LIST_TASKS
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListResponse {
    pub ok: bool,
    pub count: usize,
    /// Tasks in dispatch order (priority, fairness, FIFO) for the queue; other
    /// states ordered by enqueue sequence.
    pub tasks: Vec<AgentTask>,
    /// Tasks reconcile flagged as orphaned during this read.
    pub reconciled_orphans: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskNextParams {
    /// Max concurrently in-flight tasks; the selector returns nothing when the
    /// queue is already at this cap.
    #[serde(default = "default_cap")]
    #[schemars(default = "default_cap", range(min = 1))]
    pub concurrency_cap: usize,
}

pub(crate) fn default_cap() -> usize {
    DEFAULT_CONCURRENCY_CAP
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskNextResponse {
    pub ok: bool,
    /// Why the selector returned (or didn't return) a task.
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<AgentTask>,
    pub in_flight: usize,
    pub concurrency_cap: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskReconcileResponse {
    pub ok: bool,
    pub scanned_in_progress: usize,
    pub flagged_orphans: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskSequenceRepairParams {
    /// Operator-observed lower bound. Required so repairing malformed bytes can
    /// never reuse a sequence that may have existed before corruption.
    pub minimum_last_enqueue_seq: u64,
    /// Audit reason for the explicit destructive metadata repair.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskSequenceRepairResponse {
    pub ok: bool,
    pub observed_task_rows: usize,
    pub unobservable_task_sequences: usize,
    pub observed_max_task_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_decodable_sequence: Option<u64>,
    pub requested_minimum_sequence: u64,
    pub repaired_sequence: u64,
    pub committed_seq: u64,
    pub written_row: TaskRowReadback,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskQueueStateRepairParams {
    /// Exact SHA-256 revision observed through a separate physical read. `None`
    /// is accepted only when the coordination row is physically absent.
    #[serde(default)]
    #[schemars(default)]
    pub expected_revision_sha256: Option<String>,
    /// Operator-observed lower bound for the last durable queue generation.
    /// This is mandatory when any task row is too corrupt to expose its own
    /// generation, so repair never silently rewinds coordination state.
    pub minimum_mutation_generation: u64,
    /// Audit reason for the explicit destructive metadata repair.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskQueueStateRepairResponse {
    pub ok: bool,
    pub observed_task_rows: usize,
    pub unobservable_task_generations: usize,
    pub observed_max_task_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_decodable_generation: Option<u64>,
    pub requested_minimum_generation: u64,
    pub repaired_generation: u64,
    pub previous_revision_sha256: Option<String>,
    pub committed_seq: Option<u64>,
    pub written_row: TaskRowReadback,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskRowRepairParams {
    /// Physical task identity to repair.
    pub task_id: String,
    /// Exact SHA-256 revision observed through a separate physical read.
    pub expected_revision_sha256: String,
    /// Complete valid replacement. The repair assigns the next durable queue
    /// generation; callers must provide `mutation_generation = 0` as an
    /// explicit acknowledgement that no stale generation is being preserved.
    pub replacement: AgentTask,
    /// Audit reason for the explicit destructive row repair.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskRowRepairResponse {
    pub ok: bool,
    pub previous_revision_sha256: String,
    pub previous_value_len_bytes: u64,
    pub unobservable_other_task_rows: usize,
    pub committed_seq: Option<u64>,
    pub task: AgentTask,
    pub written_row: TaskRowReadback,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskDispatchOnceParams {
    /// Max concurrently in-flight tasks; no spawn occurs when already at cap.
    #[serde(default = "default_cap")]
    #[schemars(default = "default_cap", range(min = 1))]
    pub concurrency_cap: usize,
    /// Streamable HTTP MCP endpoint forwarded to `act_spawn_agent`.
    #[serde(default = "default_agent_spawn_mcp_url")]
    #[schemars(default = "default_agent_spawn_mcp_url")]
    pub mcp_url: String,
    /// Spawn readback wait budget forwarded to `act_spawn_agent`.
    #[serde(default = "default_agent_spawn_wait_timeout_ms")]
    #[schemars(
        default = "default_agent_spawn_wait_timeout_ms",
        range(min = 1, max = 1_800_000)
    )]
    pub wait_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchSpawnReadback {
    pub spawn_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_process_id: Option<u32>,
    pub launched_at_unix_ms: u64,
    pub task_started_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskDispatchOnceResponse {
    pub ok: bool,
    /// `dispatched`, `empty`, or `at_capacity:N`.
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<AgentTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<DispatchSpawnReadback>,
    pub in_flight: usize,
    pub concurrency_cap: usize,
}

/// Dashboard-specific cancel readback. The durable row transition is still the
/// source of truth, with an optional physical agent interrupt/kill readback
/// when the task had a live pending spawned attempt.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DashboardTaskCancelResponse {
    pub ok: bool,
    pub cancel: TaskMutationResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt: Option<super::agent_control::AgentKillResponse>,
}

// ---- key encoding & validation -------------------------------------------

fn task_key(task_id: &str) -> String {
    format!("{TASK_NAMESPACE}/task/{task_id}")
}

fn task_prefix() -> String {
    format!("{TASK_NAMESPACE}/task/")
}

fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn params_error(message: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, message.into())
}

fn task_not_found(task_id: &str) -> ErrorData {
    mcp_error(
        error_codes::AGENT_TASK_NOT_FOUND,
        format!("agent_task not found: no task with id {task_id:?}"),
    )
}

fn error_code_str(error: &ErrorData) -> &str {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("UNKNOWN")
}

fn is_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        })
}

fn validate_text(field: &str, value: &str, max: usize) -> Result<(), ErrorData> {
    if value.chars().count() > max {
        return Err(params_error(format!(
            "agent_task {field} must be <= {max} characters"
        )));
    }
    Ok(())
}

fn validate_priority(priority: u8) -> Result<(), ErrorData> {
    if !(MIN_PRIORITY..=MAX_PRIORITY).contains(&priority) {
        return Err(params_error(format!(
            "agent_task priority must be {MIN_PRIORITY}..={MAX_PRIORITY} (1 = highest), got {priority}"
        )));
    }
    Ok(())
}

fn validate_template_params(params: &BTreeMap<String, String>) -> Result<(), ErrorData> {
    for (name, value) in params {
        if value.len() > MAX_PARAM_VALUE_BYTES {
            return Err(params_error(format!(
                "agent_task template_params value for {name:?} must be <= {MAX_PARAM_VALUE_BYTES} bytes"
            )));
        }
        if value.contains('\0') {
            return Err(params_error(format!(
                "agent_task template_params value for {name:?} must not contain NUL"
            )));
        }
    }
    Ok(())
}

// ---- dispatch selection ---------------------------------------------------

/// The dispatcher's decision over a set of tasks. Returns the `task_id` of the
/// next task to dispatch, or a reason it dispatched nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DispatchDecision {
    Empty,
    AtCapacity { in_flight: usize },
    Dispatch { task_id: String },
}

/// Selects the next task to dispatch from `tasks` honoring the Temporal
/// priority+fairness model: strict priority tier, then the template with the
/// fewest in-flight attempts (join-shortest-queue fairness — a greedy template
/// cannot starve others), then FIFO by `enqueue_seq`.
pub(crate) fn dispatch_decision(tasks: &[AgentTask], concurrency_cap: usize) -> DispatchDecision {
    let in_flight = tasks.iter().filter(|task| task.is_in_flight()).count();
    if in_flight >= concurrency_cap {
        return DispatchDecision::AtCapacity { in_flight };
    }

    // Per-template in-flight counts drive the fairness key choice.
    let mut in_flight_by_template: BTreeMap<&str, usize> = BTreeMap::new();
    for task in tasks.iter().filter(|task| task.is_in_flight()) {
        *in_flight_by_template
            .entry(task.template_id.as_str())
            .or_default() += 1;
    }

    let todo: Vec<&AgentTask> = tasks
        .iter()
        .filter(|task| task.state == TaskState::Todo)
        .collect();
    let Some(top_priority) = todo.iter().map(|task| task.priority).min() else {
        return DispatchDecision::Empty;
    };

    // The winner: among the top priority tier, the task whose template has the
    // fewest in-flight attempts; ties break by oldest enqueue_seq.
    let winner = todo
        .iter()
        .filter(|task| task.priority == top_priority)
        .min_by(|a, b| {
            let a_load = in_flight_by_template
                .get(a.template_id.as_str())
                .copied()
                .unwrap_or(0);
            let b_load = in_flight_by_template
                .get(b.template_id.as_str())
                .copied()
                .unwrap_or(0);
            a_load
                .cmp(&b_load)
                .then_with(|| a.enqueue_seq.cmp(&b.enqueue_seq))
        });

    winner.map_or(DispatchDecision::Empty, |task| DispatchDecision::Dispatch {
        task_id: task.task_id.clone(),
    })
}

const fn dashboard_task_dispatch_wait_timeout_ms(requested: u64) -> u64 {
    if requested < DASHBOARD_TASK_DISPATCH_WAIT_TIMEOUT_MS {
        DASHBOARD_TASK_DISPATCH_WAIT_TIMEOUT_MS
    } else {
        requested
    }
}

/// Orders tasks for the queue view: todo tasks in dispatch order, then the rest
/// by enqueue sequence.
fn order_for_list(mut tasks: Vec<AgentTask>) -> Vec<AgentTask> {
    tasks.sort_by(|a, b| {
        // todo first, ordered by (priority asc, enqueue_seq asc); others after,
        // by enqueue_seq.
        let a_todo = a.state == TaskState::Todo;
        let b_todo = b.state == TaskState::Todo;
        b_todo.cmp(&a_todo).then_with(|| {
            if a_todo {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| a.enqueue_seq.cmp(&b.enqueue_seq))
            } else {
                a.enqueue_seq.cmp(&b.enqueue_seq)
            }
        })
    });
    tasks
}

// ---- storage --------------------------------------------------------------

fn encode_task(task: &AgentTask) -> Result<Vec<u8>, ErrorData> {
    serde_json::to_vec(task).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("agent_task failed to encode row: {error}"),
        )
    })
}

fn decode_task(row_key: &str, bytes: &[u8]) -> Result<AgentTask, ErrorData> {
    let task: AgentTask = serde_json::from_slice(bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("agent_task row {row_key} is corrupt and could not be decoded: {error}"),
        )
    })?;
    let prefix = task_prefix();
    let expected_task_id = row_key.strip_prefix(&prefix).ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_TASK_ROW_KEY_INVALID: physical row {row_key:?} is outside the expected \
                 task namespace {prefix:?}"
            ),
        )
    })?;
    if expected_task_id.is_empty()
        || expected_task_id.contains('/')
        || task.schema_version != TASK_SCHEMA_VERSION
        || task.task_id != expected_task_id
        || task.enqueue_seq == 0
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_TASK_ROW_IDENTITY_INVALID: physical_row={row_key:?} \
                 expected_task_id={expected_task_id:?} stored_task_id={:?} schema_version={} \
                 expected_schema={TASK_SCHEMA_VERSION} enqueue_seq={}; task queue operations are \
                 disabled until the exact corrupt row is repaired",
                task.task_id, task.schema_version, task.enqueue_seq
            ),
        ));
    }
    validate_decoded_task(row_key, &task)?;
    Ok(task)
}

fn validate_decoded_task(row_key: &str, task: &AgentTask) -> Result<(), ErrorData> {
    let oversized_description = task
        .description
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_TEXT_CHARS);
    let oversized_acceptance = task
        .acceptance
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_TEXT_CHARS);
    let invalid_template_param = task
        .template_params
        .values()
        .any(|value| value.len() > MAX_PARAM_VALUE_BYTES || value.contains('\0'));
    let oversized_review_reason = task
        .review_reason
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_TEXT_CHARS);
    if !is_kebab_id(&task.task_id)
        || task.task_id.len() > MAX_TASK_ID_CHARS
        || task.title.trim().is_empty()
        || task.title.chars().count() > MAX_TITLE_CHARS
        || oversized_description
        || oversized_acceptance
        || !is_kebab_id(&task.template_id)
        || invalid_template_param
        || !(MIN_PRIORITY..=MAX_PRIORITY).contains(&task.priority)
        || oversized_review_reason
        || task.created_unix_ms == 0
        || task.updated_unix_ms < task.created_unix_ms
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_TASK_ROW_FIELDS_INVALID: row={row_key:?} task_id={:?} title_chars={} \
                 template_id={:?} priority={} oversized_description={} oversized_acceptance={} \
                 invalid_template_param={} oversized_review_reason={} created={} updated={}; \
                 task mutations are disabled until explicit repair",
                task.task_id,
                task.title.chars().count(),
                task.template_id,
                task.priority,
                oversized_description,
                oversized_acceptance,
                invalid_template_param,
                oversized_review_reason,
                task.created_unix_ms,
                task.updated_unix_ms
            ),
        ));
    }
    let mut pending = Vec::new();
    for (index, attempt) in task.attempts.iter().enumerate() {
        let expected_attempt_id = u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("AGENT_TASK_ATTEMPT_ID_EXHAUSTED: row={row_key:?}"),
                )
            })?;
        let pending_shape_valid = attempt.outcome != AttemptOutcome::Pending
            || (!attempt.session_id.trim().is_empty()
                && attempt.ended_unix_ms.is_none()
                && attempt.reason.is_none());
        let terminal_shape_valid = attempt.outcome == AttemptOutcome::Pending
            || attempt
                .ended_unix_ms
                .is_some_and(|ended| ended >= attempt.started_unix_ms);
        let reason_size_valid = attempt
            .reason
            .as_ref()
            .is_none_or(|reason| reason.chars().count() <= MAX_TEXT_CHARS);
        if attempt.attempt_id != expected_attempt_id
            || attempt.started_unix_ms == 0
            || !pending_shape_valid
            || !terminal_shape_valid
            || !reason_size_valid
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_ATTEMPT_INVALID: row={row_key:?} task_id={:?} index={} \
                     attempt_id={} expected_attempt_id={expected_attempt_id} session_id={:?} \
                     outcome={:?} started={} ended={:?} reason={:?}",
                    task.task_id,
                    index,
                    attempt.attempt_id,
                    attempt.session_id,
                    attempt.outcome,
                    attempt.started_unix_ms,
                    attempt.ended_unix_ms,
                    attempt.reason
                ),
            ));
        }
        if attempt.outcome == AttemptOutcome::Pending {
            pending.push(attempt);
        }
    }
    let expected_pending = usize::from(task.state == TaskState::InProgress);
    if pending.len() != expected_pending {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "AGENT_TASK_PENDING_ATTEMPT_INVARIANT: row={row_key:?} task_id={:?} state={} \
                 pending_attempts={} expected_pending={expected_pending}",
                task.task_id,
                task.state.as_str(),
                pending.len()
            ),
        ));
    }
    if let Some(reservation) = &task.dispatch_reservation {
        let pending_attempt = pending.first().copied().ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_RESERVATION_ATTEMPT_MISSING: row={row_key:?} reservation={}",
                    reservation.reservation_id
                ),
            )
        })?;
        if reservation.schema_version != TASK_DISPATCH_RESERVATION_SCHEMA_VERSION
            || !is_lower_sha256(&reservation.reservation_id)
            || reservation.reserved_unix_ms == 0
            || reservation.expires_at_unix_ms <= reservation.reserved_unix_ms
            || pending_attempt.session_id != reservation_session_id(&reservation.reservation_id)
            || pending_attempt.started_unix_ms != reservation.reserved_unix_ms
            || pending_attempt.spawn_id.is_some()
            || pending_attempt.template_version.is_some()
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_RESERVATION_INVALID: row={row_key:?} task_id={:?} \
                     reservation={reservation:?} pending_attempt={pending_attempt:?}; task \
                     mutations are disabled until explicit repair",
                    task.task_id
                ),
            ));
        }
    }
    Ok(())
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn reservation_session_id(reservation_id: &str) -> String {
    format!("task-dispatch-reservation-{reservation_id}")
}

fn next_attempt_id(task: &AgentTask) -> Result<u32, ErrorData> {
    u32::try_from(task.attempts.len())
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_ATTEMPT_ID_EXHAUSTED: task {:?} has no remaining u32 attempt id",
                    task.task_id
                ),
            )
        })
}

fn active_dispatch_reservation_error(task: &AgentTask, operation: &str) -> ErrorData {
    let reservation = task
        .dispatch_reservation
        .as_ref()
        .map(|reservation| reservation.reservation_id.as_str())
        .unwrap_or("corrupt-missing-reservation");
    mcp_error(
        error_codes::AGENT_TASK_INVALID_TRANSITION,
        format!(
            "AGENT_TASK_DISPATCH_RESERVATION_ACTIVE: task {:?} cannot {operation} while durable \
             dispatch reservation {reservation:?} owns the pre-spawn transition; wait for guarded \
             bind/failure or reconcile it after its lease expires",
            task.task_id
        ),
    )
}

fn task_dispatch_reservation_lease_ms(wait_timeout_ms: u64) -> Result<u64, ErrorData> {
    wait_timeout_ms
        .checked_add(TASK_DISPATCH_RESERVATION_GRACE_MS)
        .map(|lease_ms| lease_ms.max(TASK_DISPATCH_RESERVATION_MIN_LEASE_MS))
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "AGENT_TASK_RESERVATION_LEASE_EXHAUSTED: wait_timeout_ms={wait_timeout_ms} \
                     grace_ms={TASK_DISPATCH_RESERVATION_GRACE_MS}"
                ),
            )
        })
}

fn build_task_dispatch_reservation(
    task: &AgentTask,
    queue_generation: u64,
    reserved_unix_ms: u64,
    wait_timeout_ms: u64,
) -> Result<TaskDispatchReservation, ErrorData> {
    let expires_at_unix_ms = reserved_unix_ms
        .checked_add(task_dispatch_reservation_lease_ms(wait_timeout_ms)?)
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_RESERVATION_TIMESTAMP_EXHAUSTED: task={:?} reserved={reserved_unix_ms} \
                     wait_timeout_ms={wait_timeout_ms}",
                    task.task_id
                ),
            )
        })?;
    let identity = TaskDispatchReservationIdentity {
        schema_version: TASK_DISPATCH_RESERVATION_SCHEMA_VERSION,
        task_id: &task.task_id,
        enqueue_seq: task.enqueue_seq,
        queue_generation,
        reserved_unix_ms,
        expires_at_unix_ms,
    };
    let encoded = synapse_storage::encode_json(&identity).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_RESERVATION_IDENTITY_ENCODE_FAILED: task={:?}: {error}",
                task.task_id
            ),
        )
    })?;
    let reservation_id = sha256_hex(&encoded);
    Ok(TaskDispatchReservation {
        schema_version: TASK_DISPATCH_RESERVATION_SCHEMA_VERSION,
        reservation_id,
        reserved_unix_ms,
        expires_at_unix_ms,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn parse_revision_sha256(value: &str, field: &str) -> Result<[u8; 32], ErrorData> {
    if !is_lower_sha256(value) {
        return Err(params_error(format!(
            "agent_task {field} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    let mut revision = [0_u8; 32];
    for (index, slot) in revision.iter_mut().enumerate() {
        let start = index.checked_mul(2).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "agent_task revision parser index overflow",
            )
        })?;
        let end = start.checked_add(2).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "agent_task revision parser range overflow",
            )
        })?;
        let pair = value.get(start..end).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "agent_task revision parser lost a validated hexadecimal pair",
            )
        })?;
        *slot = u8::from_str_radix(pair, 16).map_err(|error| {
            params_error(format!(
                "agent_task {field} contains an invalid hexadecimal pair at byte {start}: {error}"
            ))
        })?;
    }
    Ok(revision)
}

fn revision_sha256_hex(revision: &[u8; 32]) -> String {
    synapse_storage::constellations::hex_encode(revision)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpawnTerminalCompletion {
    path: PathBuf,
    status: String,
    error_message: Option<String>,
}

#[derive(Clone, Debug)]
struct AgentTaskRow {
    key: Vec<u8>,
    revision_sha256: [u8; 32],
    task: AgentTask,
}

struct ScannedTaskRow {
    key: Vec<u8>,
    encoded: Vec<u8>,
    task: AgentTask,
}

type RawTaskRow = (Vec<u8>, Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskQueueState {
    schema_version: u32,
    mutation_generation: u64,
    updated_unix_ms: u64,
}

#[derive(Serialize)]
struct TaskDispatchReservationIdentity<'a> {
    schema_version: u32,
    task_id: &'a str,
    enqueue_seq: u64,
    queue_generation: u64,
    reserved_unix_ms: u64,
    expires_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct RevisionedTaskQueueState {
    state: TaskQueueState,
    revision_sha256: [u8; 32],
}

struct StableTaskSnapshot {
    queue_state: RevisionedTaskQueueState,
    rows: Vec<AgentTaskRow>,
}

enum GuardedTaskMutationOutcome {
    Conflict,
    Committed {
        task: Box<AgentTask>,
        written_row: TaskRowReadback,
    },
}

enum DispatchReservationOutcome {
    Empty {
        in_flight: usize,
    },
    AtCapacity {
        in_flight: usize,
    },
    Reserved {
        task: Box<AgentTask>,
        reservation_id: String,
        in_flight_before: usize,
    },
}

#[derive(Clone, Copy, Debug)]
struct RevisionedTaskSequence {
    value: u64,
    revision_sha256: [u8; 32],
}

impl SpawnTerminalCompletion {
    fn is_success(&self) -> bool {
        self.status == "ok"
    }

    fn reason(&self) -> String {
        match self
            .error_message
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            Some(error) => format!(
                "spawned agent terminal artifact status={} at {} ({error})",
                self.status,
                self.path.display()
            ),
            None => format!(
                "spawned agent terminal artifact status={} at {}",
                self.status,
                self.path.display()
            ),
        }
    }
}

fn default_agent_spawn_log_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Synapse").join("agent-spawns"))
}

fn is_spawn_id_shape(value: &str) -> bool {
    value.starts_with("agent-spawn-")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

fn read_spawn_terminal_completion(
    spawn_log_root: Option<&Path>,
    spawn_id: &str,
) -> Option<SpawnTerminalCompletion> {
    if !is_spawn_id_shape(spawn_id) {
        return None;
    }
    let root = spawn_log_root?;
    let path = root.join(spawn_id).join("completion-status.json");
    let bytes = fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let status = value.get("status").and_then(Value::as_str)?.to_owned();
    if status == "running" {
        return None;
    }
    let error_message = value
        .get("error_message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Some(SpawnTerminalCompletion {
        path,
        status,
        error_message,
    })
}

impl SynapseService {
    fn agent_task_db(&self) -> Result<std::sync::Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent task storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn read_task(db: &Db, task_id: &str) -> Result<Option<AgentTask>, ErrorData> {
        Self::read_task_row_revisioned(db, task_id).map(|row| row.map(|row| row.task))
    }

    fn read_task_row_revisioned(db: &Db, task_id: &str) -> Result<Option<AgentTaskRow>, ErrorData> {
        let key = task_key(task_id);
        let revisioned = db
            .get_cf_revisioned(cf::CF_KV, key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_task failed to read revisioned row {key}: {error}"),
                )
            })?;
        let Some(revisioned) = revisioned else {
            return Ok(None);
        };
        let encoded = revisioned.value.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_ROW_EXPIRED: {key} has a physical expired envelope; task \
                     mutations are disabled until the exact row is repaired"
                ),
            )
        })?;
        let task = decode_task(&key, &encoded)?;
        Ok(Some(AgentTaskRow {
            key: key.into_bytes(),
            revision_sha256: revisioned.revision_sha256,
            task,
        }))
    }

    pub(crate) fn read_all_tasks(db: &Db) -> Result<Vec<AgentTask>, ErrorData> {
        Self::stable_task_snapshot(db, unix_time_ms_now())
            .map(|snapshot| snapshot.rows.into_iter().map(|row| row.task).collect())
    }

    fn scan_raw_task_values(db: &Db) -> Result<Vec<RawTaskRow>, ErrorData> {
        let prefix = task_prefix();
        let mut start = prefix.as_bytes().to_vec();
        let mut out = Vec::new();
        loop {
            let (rows, more) = db
                .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("agent_task failed to scan tasks: {error}"),
                    )
                })?;
            if rows.is_empty() {
                break;
            }
            let mut stop = false;
            let mut last_key = None;
            for (raw_key, raw_value) in rows {
                if !raw_key.starts_with(prefix.as_bytes()) {
                    stop = true;
                    break;
                }
                out.push((raw_key.clone(), raw_value));
                last_key = Some(raw_key);
            }
            if stop || !more {
                break;
            }
            let Some(key) = last_key else {
                break;
            };
            start = key_after(&key);
        }
        Ok(out)
    }

    fn scan_task_values(db: &Db) -> Result<Vec<ScannedTaskRow>, ErrorData> {
        Self::scan_raw_task_values(db)?
            .into_iter()
            .map(|(key, encoded)| {
                let key_text = String::from_utf8_lossy(&key).into_owned();
                let task = decode_task(&key_text, &encoded)?;
                Ok(ScannedTaskRow { key, encoded, task })
            })
            .collect()
    }

    fn scan_task_rows_revisioned(db: &Db) -> Result<Option<Vec<AgentTaskRow>>, ErrorData> {
        let scanned = Self::scan_task_values(db)?;
        let mut rows = Vec::with_capacity(scanned.len());
        for row in scanned {
            let revisioned = db.get_cf_revisioned(cf::CF_KV, &row.key).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "read exact revision during task queue snapshot {}: {error}",
                        String::from_utf8_lossy(&row.key)
                    ),
                )
            })?;
            let Some(revisioned) = revisioned else {
                return Ok(None);
            };
            if revisioned.value.as_deref() != Some(row.encoded.as_slice()) {
                return Ok(None);
            }
            rows.push(AgentTaskRow {
                key: row.key,
                revision_sha256: revisioned.revision_sha256,
                task: row.task,
            });
        }
        Ok(Some(rows))
    }

    fn encode_task_queue_state(state: &TaskQueueState) -> Result<Vec<u8>, ErrorData> {
        synapse_storage::encode_json(state).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("encode durable task queue state: {error}"),
            )
        })
    }

    fn read_task_queue_state_revisioned(
        db: &Db,
    ) -> Result<Option<RevisionedTaskQueueState>, ErrorData> {
        let revisioned = db
            .get_cf_revisioned(cf::CF_KV, TASK_QUEUE_STATE_KEY.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("read revisioned durable task queue state: {error}"),
                )
            })?;
        let Some(revisioned) = revisioned else {
            return Ok(None);
        };
        let value = revisioned.value.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_QUEUE_STATE_EXPIRED: {TASK_QUEUE_STATE_KEY} has an expired \
                     physical envelope; task mutations are disabled until explicit repair"
                ),
            )
        })?;
        let state: TaskQueueState = synapse_storage::decode_json(&value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_QUEUE_STATE_CORRUPTED: decode {TASK_QUEUE_STATE_KEY}: {error}; \
                     task mutations are disabled until explicit repair"
                ),
            )
        })?;
        if state.schema_version != TASK_QUEUE_STATE_SCHEMA_VERSION {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_QUEUE_STATE_VERSION_INVALID: {TASK_QUEUE_STATE_KEY} has \
                     schema_version={}, expected {TASK_QUEUE_STATE_SCHEMA_VERSION}",
                    state.schema_version
                ),
            ));
        }
        Ok(Some(RevisionedTaskQueueState {
            state,
            revision_sha256: revisioned.revision_sha256,
        }))
    }

    fn initialize_task_queue_state(
        db: &Db,
        now_unix_ms: u64,
    ) -> Result<RevisionedTaskQueueState, ErrorData> {
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            if let Some(state) = Self::read_task_queue_state_revisioned(db)? {
                return Ok(state);
            }
            // Decode every legacy row before establishing the generation. A
            // malformed row cannot be hidden by initializing coordination
            // metadata around it.
            let rows = Self::scan_task_values(db)?;
            let initial_generation = rows
                .iter()
                .map(|row| row.task.mutation_generation)
                .max()
                .unwrap_or(0);
            let state = TaskQueueState {
                schema_version: TASK_QUEUE_STATE_SCHEMA_VERSION,
                mutation_generation: initial_generation,
                updated_unix_ms: now_unix_ms,
            };
            let encoded = Self::encode_task_queue_state(&state)?;
            let outcome = db.mutate_batch_if_revisions_pressure_bypass(
                cf::CF_KV,
                [RevisionGuard::new(TASK_QUEUE_STATE_KEY.as_bytes(), None)],
                std::iter::empty::<Vec<u8>>(),
                [(TASK_QUEUE_STATE_KEY.as_bytes().to_vec(), encoded.clone())],
            );
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    let readback = Self::read_task_queue_state_revisioned(db)?;
                    if let Some(readback) = readback.filter(|readback| readback.state == state) {
                        tracing::warn!(
                            code = "AGENT_TASK_QUEUE_STATE_INIT_AMBIGUOUS_COMMIT_RECONCILED",
                            initial_generation,
                            "separate physical queue-state readback proved initialization committed"
                        );
                        return Ok(readback);
                    }
                    return Err(mcp_error(
                        error.code(),
                        format!(
                            "AGENT_TASK_QUEUE_STATE_INIT_COMMIT_AMBIGUOUS: initialization failed \
                             ({error}) and exact queue-state readback did not prove commit"
                        ),
                    ));
                }
            };
            if !outcome.applied {
                tracing::warn!(
                    code = "AGENT_TASK_QUEUE_STATE_INIT_CONFLICT",
                    retry,
                    "task queue-state initialization conflicted; rereading physical state"
                );
                continue;
            }
            let readback = Self::read_task_queue_state_revisioned(db)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_QUEUE_STATE_INIT_READBACK_MISSING: committed_seq={} key={TASK_QUEUE_STATE_KEY}",
                        outcome.committed_seq
                    ),
                )
            })?;
            if readback.state != state {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_QUEUE_STATE_INIT_READBACK_DRIFT: committed_seq={} \
                         expected_generation={initial_generation} actual_generation={}",
                        outcome.committed_seq, readback.state.mutation_generation
                    ),
                ));
            }
            return Ok(readback);
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_QUEUE_STATE_INIT_CONTENTION: initialization conflicted \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} times"
            ),
        ))
    }

    fn next_task_queue_state(
        current: &TaskQueueState,
        now_unix_ms: u64,
    ) -> Result<TaskQueueState, ErrorData> {
        let mutation_generation = current.mutation_generation.checked_add(1).ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                "AGENT_TASK_QUEUE_GENERATION_EXHAUSTED: durable generation reached u64::MAX",
            )
        })?;
        Ok(TaskQueueState {
            schema_version: TASK_QUEUE_STATE_SCHEMA_VERSION,
            mutation_generation,
            updated_unix_ms: now_unix_ms,
        })
    }

    fn stable_task_snapshot(db: &Db, now_unix_ms: u64) -> Result<StableTaskSnapshot, ErrorData> {
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            let before = Self::initialize_task_queue_state(db, now_unix_ms)?;
            let rows = Self::scan_task_rows_revisioned(db)?;
            let after = Self::read_task_queue_state_revisioned(db)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_QUEUE_STATE_MISSING: {TASK_QUEUE_STATE_KEY} disappeared \
                         during a physical task-row audit"
                    ),
                )
            })?;
            let Some(rows) = rows else {
                tracing::debug!(
                    code = "AGENT_TASK_QUEUE_SNAPSHOT_ROW_CONFLICT",
                    retry,
                    "task row changed during physical scan; rereading all SoTs"
                );
                continue;
            };
            if before.revision_sha256 != after.revision_sha256 {
                tracing::debug!(
                    code = "AGENT_TASK_QUEUE_SNAPSHOT_CONFLICT",
                    retry,
                    "task row or queue generation changed during physical scan; rereading all SoTs"
                );
                continue;
            }
            if let Some(row) = rows
                .iter()
                .find(|row| row.task.mutation_generation > after.state.mutation_generation)
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_QUEUE_GENERATION_DRIFT: task {:?} row_generation={} exceeds \
                         durable_queue_generation={}; task mutations are disabled until repair",
                        row.task.task_id,
                        row.task.mutation_generation,
                        after.state.mutation_generation
                    ),
                ));
            }
            return Ok(StableTaskSnapshot {
                queue_state: after,
                rows,
            });
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_QUEUE_SNAPSHOT_CONTENTION: queue changed during \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} consecutive physical audits"
            ),
        ))
    }

    fn decode_enqueue_seq_watermark(value: &[u8]) -> Result<u64, ErrorData> {
        let text = std::str::from_utf8(value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_SEQUENCE_CORRUPTED: {TASK_SEQUENCE_KEY} is not UTF-8: {error}; \
                     task creation is disabled until the physical watermark is repaired"
                ),
            )
        })?;
        let parsed = text.parse::<u64>().map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_SEQUENCE_CORRUPTED: {TASK_SEQUENCE_KEY} is not a canonical u64: \
                     {error}; task creation is disabled until the physical watermark is repaired"
                ),
            )
        })?;
        let canonical = parsed.to_string();
        if canonical != text {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_SEQUENCE_CORRUPTED: {TASK_SEQUENCE_KEY} uses non-canonical decimal \
                     bytes {text:?}; expected {canonical:?}; task creation is disabled until the \
                     physical watermark is repaired"
                ),
            ));
        }
        Ok(parsed)
    }

    fn read_enqueue_seq_watermark_revisioned(
        db: &Db,
    ) -> Result<Option<RevisionedTaskSequence>, ErrorData> {
        let revisioned = db
            .get_cf_revisioned(cf::CF_KV, TASK_SEQUENCE_KEY.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_task failed to read revisioned sequence watermark: {error}"),
                )
            })?;
        let Some(revisioned) = revisioned else {
            return Ok(None);
        };
        let value = revisioned.value.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_SEQUENCE_CORRUPTED: {TASK_SEQUENCE_KEY} has a physical expired \
                     envelope instead of a live watermark; task creation is disabled"
                ),
            )
        })?;
        Ok(Some(RevisionedTaskSequence {
            value: Self::decode_enqueue_seq_watermark(&value)?,
            revision_sha256: revisioned.revision_sha256,
        }))
    }

    fn initialize_enqueue_seq_watermark(db: &Db) -> Result<RevisionedTaskSequence, ErrorData> {
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            if let Some(sequence) = Self::read_enqueue_seq_watermark_revisioned(db)? {
                return Ok(sequence);
            }
            let max_seq = Self::scan_task_values(db)?
                .iter()
                .map(|row| row.task.enqueue_seq)
                .max()
                .unwrap_or(0);
            let encoded = max_seq.to_string().into_bytes();
            let outcome = db
                .mutate_batch_if_revisions_pressure_bypass(
                    cf::CF_KV,
                    [RevisionGuard::new(TASK_SEQUENCE_KEY.as_bytes(), None)],
                    std::iter::empty::<Vec<u8>>(),
                    [(TASK_SEQUENCE_KEY.as_bytes().to_vec(), encoded.clone())],
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "agent_task failed to initialize the guarded sequence watermark: \
                             {error}; exact key={TASK_SEQUENCE_KEY}"
                        ),
                    )
                })?;
            if !outcome.applied {
                tracing::warn!(
                    code = "AGENT_TASK_SEQUENCE_INIT_CONFLICT",
                    retry,
                    conflict_guard_index = outcome
                        .conflict
                        .as_ref()
                        .map(|conflict| conflict.guard_index),
                    "guarded task sequence initialization conflicted; rereading physical state"
                );
                continue;
            }
            let initialized = Self::read_enqueue_seq_watermark_revisioned(db)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_SEQUENCE_READBACK_MISSING: guarded initialization committed_seq={} \
                         but {TASK_SEQUENCE_KEY} is physically absent",
                        outcome.committed_seq
                    ),
                )
            })?;
            if initialized.value != max_seq {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_SEQUENCE_READBACK_DRIFT: guarded initialization committed_seq={} \
                         expected_watermark={max_seq} actual_watermark={}; task creation is disabled",
                        outcome.committed_seq, initialized.value
                    ),
                ));
            }
            return Ok(initialized);
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_SEQUENCE_CONTENTION: could not initialize {TASK_SEQUENCE_KEY} after \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} revision conflicts"
            ),
        ))
    }

    fn terminal_task_rows_to_prune(now: u64, rows: &[AgentTaskRow]) -> Vec<AgentTaskRow> {
        let mut terminal = rows
            .iter()
            .filter(|row| row.task.state.is_terminal())
            .map(|row| (row.task.updated_unix_ms, row))
            .collect::<Vec<_>>();
        terminal.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.key.cmp(&b.1.key)));

        let mut delete_keys = terminal
            .iter()
            .filter(|(updated_at, _row)| {
                updated_at.saturating_add(TERMINAL_TASK_RETENTION_MS) <= now
            })
            .map(|(_updated_at, row)| row.key.clone())
            .collect::<BTreeSet<_>>();

        if terminal.len() > TERMINAL_TASK_RETAIN_ROWS {
            let over_cap = terminal.len() - TERMINAL_TASK_RETAIN_ROWS;
            delete_keys.extend(
                terminal
                    .iter()
                    .take(over_cap)
                    .map(|(_updated_at, row)| row.key.clone()),
            );
        }
        rows.iter()
            .filter(|row| delete_keys.contains(&row.key))
            .cloned()
            .collect()
    }

    fn prune_terminal_tasks(db: &Db, now: u64) -> Result<usize, ErrorData> {
        let _mutation_guard = Self::acquire_task_queue_mutation_lock("task_retention_prune")?;
        let mut deleted_total = 0_usize;
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            let snapshot = Self::stable_task_snapshot(db, now)?;
            let sequence = Self::initialize_enqueue_seq_watermark(db)?;
            let max_row_sequence = snapshot
                .rows
                .iter()
                .map(|row| row.task.enqueue_seq)
                .max()
                .unwrap_or(0);
            if max_row_sequence > sequence.value {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_SEQUENCE_DRIFT: physical task max_enqueue_seq={max_row_sequence} \
                         exceeds durable watermark={}; task mutations are disabled until the \
                         watermark is explicitly repaired",
                        sequence.value
                    ),
                ));
            }
            let delete = Self::terminal_task_rows_to_prune(now, &snapshot.rows);
            if delete.is_empty() {
                return Ok(deleted_total);
            }
            let terminal_rows = snapshot
                .rows
                .iter()
                .filter(|row| row.task.state.is_terminal())
                .count();
            let mut queue = snapshot.queue_state;
            let mut restart = false;
            for chunk in delete.chunks(DELETE_BATCH_ROWS) {
                let next_queue = Self::next_task_queue_state(&queue.state, now)?;
                let encoded_queue = Self::encode_task_queue_state(&next_queue)?;
                let mut guards = Vec::with_capacity(chunk.len() + 1);
                guards.push(RevisionGuard::new(
                    TASK_QUEUE_STATE_KEY.as_bytes(),
                    Some(queue.revision_sha256),
                ));
                guards.extend(
                    chunk
                        .iter()
                        .map(|row| RevisionGuard::new(row.key.clone(), Some(row.revision_sha256))),
                );
                let keys = chunk.iter().map(|row| row.key.clone()).collect::<Vec<_>>();
                let outcome = db.mutate_batch_if_revisions_pressure_bypass(
                    cf::CF_KV,
                    guards,
                    keys.clone(),
                    [(TASK_QUEUE_STATE_KEY.as_bytes().to_vec(), encoded_queue)],
                );
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let all_absent = keys.iter().try_fold(true, |all_absent, key| {
                            db.get_cf(cf::CF_KV, key)
                                .map(|value| all_absent && value.is_none())
                                .map_err(|read_error| {
                                    mcp_error(
                                        read_error.code(),
                                        format!(
                                            "AGENT_TASK_RETENTION_COMMIT_AMBIGUOUS: prune failed \
                                             ({error}) and exact row readback failed for {}: \
                                             {read_error}",
                                            String::from_utf8_lossy(key)
                                        ),
                                    )
                                })
                        })?;
                        let queue_readback = Self::read_task_queue_state_revisioned(db)?;
                        if all_absent
                            && queue_readback.as_ref().is_some_and(|readback| {
                                readback.state.mutation_generation >= next_queue.mutation_generation
                            })
                        {
                            deleted_total = deleted_total.saturating_add(keys.len());
                            queue = queue_readback.ok_or_else(|| {
                                mcp_error(
                                    error_codes::STORAGE_CORRUPTED,
                                    "task retention queue readback disappeared after exact proof",
                                )
                            })?;
                            continue;
                        }
                        return Err(mcp_error(
                            error.code(),
                            format!(
                                "AGENT_TASK_RETENTION_COMMIT_AMBIGUOUS: prune failed ({error}); \
                                 exact row/queue readback did not prove commit, so no blind retry \
                                 was attempted"
                            ),
                        ));
                    }
                };
                if !outcome.applied {
                    tracing::warn!(
                        code = "AGENT_TASK_RETENTION_REVISION_CONFLICT",
                        retry,
                        "terminal task or queue generation changed; rebuilding prune set"
                    );
                    restart = true;
                    break;
                }
                for key in &keys {
                    if db
                        .get_cf(cf::CF_KV, key)
                        .map_err(|error| mcp_error(error.code(), error.to_string()))?
                        .is_some()
                    {
                        return Err(mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "AGENT_TASK_RETENTION_READBACK_PRESENT: committed_seq={} row={} \
                                 remains physically present",
                                outcome.committed_seq,
                                String::from_utf8_lossy(key)
                            ),
                        ));
                    }
                }
                queue = Self::read_task_queue_state_revisioned(db)?.ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        "task queue state missing after guarded terminal prune",
                    )
                })?;
                if queue.state.mutation_generation < next_queue.mutation_generation {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        "task queue generation did not advance after guarded terminal prune",
                    ));
                }
                deleted_total = deleted_total.saturating_add(keys.len());
            }
            if restart {
                continue;
            }
            tracing::info!(
                code = "AGENT_TASK_RETENTION_PRUNED",
                scanned_rows = snapshot.rows.len(),
                terminal_rows,
                deleted_rows = deleted_total,
                retain_terminal_rows = TERMINAL_TASK_RETAIN_ROWS,
                retention_ms = TERMINAL_TASK_RETENTION_MS,
                "readback=CF_KV terminal agent task rows pruned under queue generation"
            );
            return Ok(deleted_total);
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_RETENTION_CONTENTION: terminal prune conflicted \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} times"
            ),
        ))
    }

    fn acquire_task_queue_mutation_lock(
        context: &str,
    ) -> Result<std::sync::MutexGuard<'static, ()>, ErrorData> {
        task_queue_mutation_lock().lock().map_err(|poisoned| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "AGENT_TASK_QUEUE_MUTATION_LOCK_POISONED: context={context}: {poisoned}; \
                     remediation=restart the daemon and audit the queue generation plus exact task \
                     revisions before retrying"
                ),
            )
        })
    }

    fn verify_guarded_task_successor(
        db: &Db,
        task_id: &str,
        encoded_successor: &[u8],
        expected_generation: u64,
        context: &str,
    ) -> Result<TaskRowReadback, ErrorData> {
        let key = task_key(task_id);
        let stored = db
            .get_cf(cf::CF_KV, key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "AGENT_TASK_MUTATION_READBACK_FAILED: context={context} row={key} \
                         generation={expected_generation}: {error}"
                    ),
                )
            })?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_MUTATION_READBACK_MISSING: context={context} row={key} \
                         generation={expected_generation}"
                    ),
                )
            })?;
        if stored != encoded_successor {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_MUTATION_READBACK_DRIFT: context={context} row={key} \
                     generation={expected_generation} exact successor bytes do not match"
                ),
            ));
        }
        let decoded = decode_task(&key, &stored)?;
        if decoded.mutation_generation != expected_generation {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_MUTATION_GENERATION_DRIFT: context={context} row={key} \
                     expected_generation={expected_generation} row_generation={}",
                    decoded.mutation_generation
                ),
            ));
        }
        let queue = Self::read_task_queue_state_revisioned(db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_QUEUE_STATE_MISSING: context={context} expected_generation={expected_generation}"
                ),
            )
        })?;
        if queue.state.mutation_generation < expected_generation {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_QUEUE_GENERATION_ROLLBACK: context={context} \
                     expected_generation_at_least={expected_generation} actual_generation={}",
                    queue.state.mutation_generation
                ),
            ));
        }
        Ok(TaskRowReadback {
            cf_name: cf::CF_KV.to_owned(),
            row_key: key,
            value_len_bytes: stored.len() as u64,
        })
    }

    fn commit_guarded_task_successor(
        db: &Db,
        predecessor: &AgentTaskRow,
        queue: &RevisionedTaskQueueState,
        mut successor: AgentTask,
        now_unix_ms: u64,
        context: &str,
    ) -> Result<GuardedTaskMutationOutcome, ErrorData> {
        if successor.task_id != predecessor.task.task_id
            || successor.schema_version != TASK_SCHEMA_VERSION
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_MUTATION_IDENTITY_INVALID: context={context} predecessor={:?} \
                     successor={:?} successor_schema={}",
                    predecessor.task.task_id, successor.task_id, successor.schema_version
                ),
            ));
        }
        let next_queue = Self::next_task_queue_state(&queue.state, now_unix_ms)?;
        successor.mutation_generation = next_queue.mutation_generation;
        successor.updated_unix_ms = now_unix_ms;
        let key_text = String::from_utf8(predecessor.key.clone()).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("agent task physical key is not UTF-8: {error}"),
            )
        })?;
        let encoded_successor = encode_task(&successor)?;
        let _validated = decode_task(&key_text, &encoded_successor)?;
        let encoded_queue = Self::encode_task_queue_state(&next_queue)?;
        let outcome = db.mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [
                RevisionGuard::new(TASK_QUEUE_STATE_KEY.as_bytes(), Some(queue.revision_sha256)),
                RevisionGuard::new(predecessor.key.clone(), Some(predecessor.revision_sha256)),
            ],
            std::iter::empty::<Vec<u8>>(),
            [
                (TASK_QUEUE_STATE_KEY.as_bytes().to_vec(), encoded_queue),
                (predecessor.key.clone(), encoded_successor.clone()),
            ],
        );
        let committed_seq = match outcome {
            Ok(outcome) => {
                if !outcome.applied {
                    return Ok(GuardedTaskMutationOutcome::Conflict);
                }
                outcome.committed_seq
            }
            Err(error) => {
                match Self::verify_guarded_task_successor(
                    db,
                    &successor.task_id,
                    &encoded_successor,
                    next_queue.mutation_generation,
                    context,
                ) {
                    Ok(_readback) => {
                        tracing::warn!(
                            code = "AGENT_TASK_MUTATION_AMBIGUOUS_COMMIT_RECONCILED",
                            context,
                            task_id = %successor.task_id,
                            mutation_generation = next_queue.mutation_generation,
                            "separate exact task-row and queue-state readback proved commit"
                        );
                    }
                    Err(readback_error) => {
                        return Err(mcp_error(
                            error.code(),
                            format!(
                                "AGENT_TASK_MUTATION_COMMIT_AMBIGUOUS: context={context} \
                                 task_id={:?} commit_error={error}; exact successor readback did \
                                 not prove commit: {}; no stale retry was attempted",
                                successor.task_id, readback_error.message
                            ),
                        ));
                    }
                }
                0
            }
        };
        let written_row = Self::verify_guarded_task_successor(
            db,
            &successor.task_id,
            &encoded_successor,
            next_queue.mutation_generation,
            context,
        )?;
        tracing::debug!(
            code = "AGENT_TASK_MUTATION_COMMITTED",
            context,
            task_id = %successor.task_id,
            state = successor.state.as_str(),
            mutation_generation = successor.mutation_generation,
            committed_seq,
            "readback=CF_KV edge=guarded_task_mutation"
        );
        Ok(GuardedTaskMutationOutcome::Committed {
            task: Box::new(successor),
            written_row,
        })
    }

    fn mutate_task_guarded<F>(
        db: &Db,
        task_id: &str,
        context: &str,
        mut transition: F,
    ) -> Result<(AgentTask, TaskRowReadback), ErrorData>
    where
        F: FnMut(&AgentTask, u64) -> Result<AgentTask, ErrorData>,
    {
        let _mutation_guard = Self::acquire_task_queue_mutation_lock(context)?;
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            let queue = Self::initialize_task_queue_state(db, unix_time_ms_now())?;
            let predecessor = Self::read_task_row_revisioned(db, task_id)?
                .ok_or_else(|| task_not_found(task_id))?;
            let now = unix_time_ms_now();
            let successor = transition(&predecessor.task, now)?;
            match Self::commit_guarded_task_successor(
                db,
                &predecessor,
                &queue,
                successor,
                now,
                context,
            )? {
                GuardedTaskMutationOutcome::Conflict => {
                    tracing::warn!(
                        code = "AGENT_TASK_MUTATION_REVISION_CONFLICT",
                        context,
                        task_id,
                        retry,
                        "task or queue generation changed; rereading and recomputing transition"
                    );
                }
                GuardedTaskMutationOutcome::Committed { task, written_row } => {
                    return Ok((*task, written_row));
                }
            }
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_MUTATION_CONTENTION: context={context} task={task_id:?} conflicted \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} times"
            ),
        ))
    }

    fn reserve_next_dispatch(
        db: &Db,
        concurrency_cap: usize,
        wait_timeout_ms: u64,
        context: &str,
    ) -> Result<DispatchReservationOutcome, ErrorData> {
        if concurrency_cap == 0 {
            return Err(params_error(
                "agent_task concurrency_cap must be at least 1",
            ));
        }
        if wait_timeout_ms == 0 || wait_timeout_ms > 1_800_000 {
            return Err(params_error(
                "agent_task dispatch wait_timeout_ms must be 1..=1800000",
            ));
        }
        let _mutation_guard = Self::acquire_task_queue_mutation_lock(context)?;
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            let now = unix_time_ms_now();
            let snapshot = Self::stable_task_snapshot(db, now)?;
            let tasks = snapshot
                .rows
                .iter()
                .map(|row| row.task.clone())
                .collect::<Vec<_>>();
            let in_flight = tasks.iter().filter(|task| task.is_in_flight()).count();
            let task_id = match dispatch_decision(&tasks, concurrency_cap) {
                DispatchDecision::Empty => {
                    return Ok(DispatchReservationOutcome::Empty { in_flight });
                }
                DispatchDecision::AtCapacity { in_flight } => {
                    return Ok(DispatchReservationOutcome::AtCapacity { in_flight });
                }
                DispatchDecision::Dispatch { task_id } => task_id,
            };
            let predecessor = snapshot
                .rows
                .iter()
                .find(|row| row.task.task_id == task_id)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_TASK_DISPATCH_SELECTION_MISSING: selected task {task_id:?} \
                             was absent from its stable physical snapshot"
                        ),
                    )
                })?;
            if predecessor.task.state != TaskState::Todo
                || predecessor.task.dispatch_reservation.is_some()
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_DISPATCH_SELECTION_INVALID: selected task {task_id:?} \
                         state={} reservation_present={}",
                        predecessor.task.state.as_str(),
                        predecessor.task.dispatch_reservation.is_some()
                    ),
                ));
            }
            let next_generation = snapshot
                .queue_state
                .state
                .mutation_generation
                .checked_add(1)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        "AGENT_TASK_QUEUE_GENERATION_EXHAUSTED during dispatch reservation",
                    )
                })?;
            let reservation = build_task_dispatch_reservation(
                &predecessor.task,
                next_generation,
                now,
                wait_timeout_ms,
            )?;
            let mut successor = predecessor.task.clone();
            successor.attempts.push(TaskAttempt {
                attempt_id: next_attempt_id(&successor)?,
                session_id: reservation_session_id(&reservation.reservation_id),
                spawn_id: None,
                template_version: None,
                outcome: AttemptOutcome::Pending,
                started_unix_ms: now,
                ended_unix_ms: None,
                reason: None,
            });
            successor.state = TaskState::InProgress;
            successor.dispatch_reservation = Some(reservation.clone());
            match Self::commit_guarded_task_successor(
                db,
                predecessor,
                &snapshot.queue_state,
                successor,
                now,
                context,
            )? {
                GuardedTaskMutationOutcome::Conflict => {
                    tracing::warn!(
                        code = "AGENT_TASK_DISPATCH_RESERVATION_CONFLICT",
                        context,
                        retry,
                        "admission snapshot changed; recomputing cap and selection"
                    );
                }
                GuardedTaskMutationOutcome::Committed { task, .. } => {
                    tracing::info!(
                        code = "AGENT_TASK_DISPATCH_RESERVED",
                        context,
                        task_id = %task.task_id,
                        reservation_id = %reservation.reservation_id,
                        reservation_expires_at_unix_ms = reservation.expires_at_unix_ms,
                        in_flight_before = in_flight,
                        concurrency_cap,
                        task_generation = task.mutation_generation,
                        "readback=CF_KV edge=task_dispatch_reservation"
                    );
                    return Ok(DispatchReservationOutcome::Reserved {
                        task,
                        reservation_id: reservation.reservation_id,
                        in_flight_before: in_flight,
                    });
                }
            }
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "AGENT_TASK_DISPATCH_ADMISSION_CONTENTION: context={context} conflicted \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} times while recomputing the stable cap"
            ),
        ))
    }

    fn bind_dispatch_reservation(
        db: &Db,
        task_id: &str,
        reservation_id: &str,
        session_id: &str,
        spawn_id: String,
        template_version: Option<u32>,
        context: &str,
    ) -> Result<(AgentTask, TaskRowReadback), ErrorData> {
        if session_id.trim().is_empty() || !is_spawn_id_shape(&spawn_id) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "AGENT_TASK_DISPATCH_BIND_IDENTITY_INVALID: task={task_id:?} \
                     reservation={reservation_id} session_id={session_id:?} spawn_id={spawn_id:?}"
                ),
            ));
        }
        Self::mutate_task_guarded(db, task_id, context, |predecessor, _now| {
            let reservation = predecessor.dispatch_reservation.as_ref().ok_or_else(|| {
                mcp_error(
                    error_codes::AGENT_TASK_INVALID_TRANSITION,
                    format!(
                        "AGENT_TASK_DISPATCH_BIND_RESERVATION_MISSING: task={task_id:?} \
                         expected_reservation={reservation_id} state={}",
                        predecessor.state.as_str()
                    ),
                )
            })?;
            if reservation.reservation_id != reservation_id {
                return Err(mcp_error(
                    error_codes::AGENT_TASK_INVALID_TRANSITION,
                    format!(
                        "AGENT_TASK_DISPATCH_BIND_RESERVATION_CONFLICT: task={task_id:?} \
                         expected={reservation_id} actual={}",
                        reservation.reservation_id
                    ),
                ));
            }
            let expected_session = reservation_session_id(reservation_id);
            let mut task = predecessor.clone();
            let pending = task
                .attempts
                .iter_mut()
                .find(|attempt| attempt.outcome == AttemptOutcome::Pending)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_TASK_DISPATCH_BIND_ATTEMPT_MISSING: task={task_id:?} \
                             reservation={reservation_id}"
                        ),
                    )
                })?;
            if pending.session_id != expected_session
                || pending.spawn_id.is_some()
                || pending.template_version.is_some()
            {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_DISPATCH_BIND_ATTEMPT_DRIFT: task={task_id:?} \
                         reservation={reservation_id} pending={pending:?}"
                    ),
                ));
            }
            pending.session_id = session_id.to_owned();
            pending.spawn_id = Some(spawn_id.clone());
            pending.template_version = template_version;
            task.dispatch_reservation = None;
            Ok(task)
        })
    }

    fn fail_dispatch_reservation(
        db: &Db,
        task_id: &str,
        reservation_id: &str,
        reason: String,
        context: &str,
    ) -> Result<(AgentTask, TaskRowReadback), ErrorData> {
        Self::mutate_task_guarded(db, task_id, context, |predecessor, now| {
            let reservation = predecessor.dispatch_reservation.as_ref().ok_or_else(|| {
                mcp_error(
                    error_codes::AGENT_TASK_INVALID_TRANSITION,
                    format!(
                        "AGENT_TASK_DISPATCH_FAILURE_RESERVATION_MISSING: task={task_id:?} \
                         expected_reservation={reservation_id} state={}",
                        predecessor.state.as_str()
                    ),
                )
            })?;
            if reservation.reservation_id != reservation_id {
                return Err(mcp_error(
                    error_codes::AGENT_TASK_INVALID_TRANSITION,
                    format!(
                        "AGENT_TASK_DISPATCH_FAILURE_RESERVATION_CONFLICT: task={task_id:?} \
                         expected={reservation_id} actual={}",
                        reservation.reservation_id
                    ),
                ));
            }
            let expected_session = reservation_session_id(reservation_id);
            let mut task = predecessor.clone();
            let pending = task
                .attempts
                .iter_mut()
                .find(|attempt| attempt.outcome == AttemptOutcome::Pending)
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_TASK_DISPATCH_FAILURE_ATTEMPT_MISSING: task={task_id:?} \
                             reservation={reservation_id}"
                        ),
                    )
                })?;
            if pending.session_id != expected_session {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "AGENT_TASK_DISPATCH_FAILURE_ATTEMPT_DRIFT: task={task_id:?} \
                         reservation={reservation_id} pending_session={:?}",
                        pending.session_id
                    ),
                ));
            }
            pending.outcome = AttemptOutcome::Failed;
            pending.ended_unix_ms = Some(now);
            pending.reason = Some(reason.clone());
            task.dispatch_reservation = None;
            task.state = TaskState::Todo;
            task.review_reason = None;
            Ok(task)
        })
    }

    /// Live MCP session ids, for reconcile.
    fn live_session_ids(&self, now_unix_ms: u64) -> Result<BTreeSet<String>, ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while reconciling agent tasks",
            )
        })?;
        Ok(guard
            .reads(now_unix_ms)
            .into_iter()
            .filter(|entry| entry.lifecycle == "live")
            .map(|entry| entry.session_id)
            .collect())
    }

    fn reconcile_task_rows(
        db: &Db,
        live: &BTreeSet<String>,
        now: u64,
        spawn_log_root: Option<&Path>,
    ) -> Result<(usize, Vec<String>), ErrorData> {
        let snapshot = Self::stable_task_snapshot(db, now)?;
        let mut scanned = 0usize;
        let mut flagged = Vec::new();
        for row in snapshot.rows {
            let task = row.task;
            if task.state != TaskState::InProgress {
                continue;
            }
            scanned += 1;
            if let Some(reservation) = &task.dispatch_reservation {
                if reservation.expires_at_unix_ms > now {
                    continue;
                }
                let reservation_id = reservation.reservation_id.clone();
                let reason = format!(
                    "expired durable dispatch reservation {reservation_id} at {} ms before a \
                     spawned session was bound; external spawn outcome is unknown and requires review",
                    reservation.expires_at_unix_ms
                );
                let (reconciled, _readback) = Self::mutate_task_guarded(
                    db,
                    &task.task_id,
                    "task_reconcile_expired_reservation",
                    |predecessor, transition_now| {
                        let current =
                            predecessor.dispatch_reservation.as_ref().ok_or_else(|| {
                                mcp_error(
                                    error_codes::AGENT_TASK_INVALID_TRANSITION,
                                    format!(
                                        "AGENT_TASK_RECONCILE_RESERVATION_SUPERSEDED: task={:?} \
                                     expected_reservation={reservation_id} is no longer present",
                                        predecessor.task_id
                                    ),
                                )
                            })?;
                        if current.reservation_id != reservation_id
                            || current.expires_at_unix_ms > transition_now
                        {
                            return Err(mcp_error(
                                error_codes::AGENT_TASK_INVALID_TRANSITION,
                                format!(
                                    "AGENT_TASK_RECONCILE_RESERVATION_CONFLICT: task={:?} \
                                     expected={reservation_id} actual={} expires_at={} now={transition_now}",
                                    predecessor.task_id,
                                    current.reservation_id,
                                    current.expires_at_unix_ms
                                ),
                            ));
                        }
                        let mut successor = predecessor.clone();
                        for attempt in &mut successor.attempts {
                            if attempt.outcome == AttemptOutcome::Pending {
                                attempt.outcome = AttemptOutcome::Orphaned;
                                attempt.ended_unix_ms = Some(transition_now);
                                attempt.reason = Some(reason.clone());
                            }
                        }
                        successor.dispatch_reservation = None;
                        successor.state = TaskState::Review;
                        successor.review_reason = Some(reason.clone());
                        Ok(successor)
                    },
                )?;
                flagged.push(reconciled.task_id);
                continue;
            }
            let missing_live_session = match task.live_attempt() {
                // An in_progress task with no live attempt is itself orphaned.
                None => true,
                Some(attempt) => !live.contains(&attempt.session_id),
            };
            if !missing_live_session {
                continue;
            }
            let task_id = task.task_id.clone();
            let (reconciled, _readback) = Self::mutate_task_guarded(
                db,
                &task_id,
                "task_reconcile",
                |predecessor, transition_now| {
                    if predecessor.state != TaskState::InProgress
                        || predecessor.dispatch_reservation.is_some()
                    {
                        return Err(mcp_error(
                            error_codes::AGENT_TASK_INVALID_TRANSITION,
                            format!(
                                "AGENT_TASK_RECONCILE_SUPERSEDED: task={:?} state={} \
                                 reservation_present={}",
                                predecessor.task_id,
                                predecessor.state.as_str(),
                                predecessor.dispatch_reservation.is_some()
                            ),
                        ));
                    }
                    let missing = predecessor
                        .live_attempt()
                        .is_none_or(|attempt| !live.contains(&attempt.session_id));
                    if !missing {
                        return Err(mcp_error(
                            error_codes::AGENT_TASK_INVALID_TRANSITION,
                            format!(
                                "AGENT_TASK_RECONCILE_SESSION_RECOVERED: task={:?} live session \
                                 reappeared before guarded reconciliation",
                                predecessor.task_id
                            ),
                        ));
                    }
                    let terminal_completion = predecessor
                        .live_attempt()
                        .and_then(|attempt| attempt.spawn_id.as_deref())
                        .and_then(|spawn_id| {
                            read_spawn_terminal_completion(spawn_log_root, spawn_id)
                        });
                    let (outcome, attempt_reason, review_reason) = match terminal_completion {
                        Some(completion) if completion.is_success() => {
                            (AttemptOutcome::Succeeded, completion.reason(), None)
                        }
                        Some(completion) => {
                            let reason = completion.reason();
                            (AttemptOutcome::Failed, reason.clone(), Some(reason))
                        }
                        None => {
                            let reason = format!(
                                "orphaned: in_progress attempt session no longer live and no \
                                 terminal completion artifact was available (reconciled at \
                                 {transition_now} ms)"
                            );
                            (AttemptOutcome::Orphaned, reason.clone(), Some(reason))
                        }
                    };
                    let mut successor = predecessor.clone();
                    for attempt in &mut successor.attempts {
                        if attempt.outcome == AttemptOutcome::Pending {
                            attempt.outcome = outcome;
                            attempt.ended_unix_ms = Some(transition_now);
                            attempt.reason = Some(attempt_reason.clone());
                        }
                    }
                    successor.state = TaskState::Review;
                    successor.review_reason = review_reason;
                    Ok(successor)
                },
            )?;
            let flagged_orphan = reconciled
                .attempts
                .last()
                .is_some_and(|attempt| attempt.outcome == AttemptOutcome::Orphaned);
            if flagged_orphan {
                flagged.push(reconciled.task_id);
            }
        }
        Ok((scanned, flagged))
    }

    /// Settles every `in_progress` task whose live attempt's session is no
    /// longer live. Spawned attempts with terminal completion artifacts become
    /// succeeded/failed review rows; missing terminal evidence is flagged as an
    /// orphan. Never silently re-queues — a human (or operator tool) decides
    /// what next.
    fn reconcile_tasks(&self, db: &Db) -> Result<(usize, Vec<String>), ErrorData> {
        let now = unix_time_ms_now();
        let live = self.live_session_ids(now)?;
        let spawn_log_root = default_agent_spawn_log_root();
        Self::reconcile_task_rows(db, &live, now, spawn_log_root.as_deref())
    }

    /// Shared claim primitive: transitions a `todo` task to `in_progress` and
    /// appends a `Pending` attempt bound to `session_id`. Rejects a claim on a
    /// task that is not `todo` (duplicate-claim race protection — the daemon's
    /// storage path is serialized so the first claim wins and the second sees
    /// `in_progress`).
    fn claim_internal(
        db: &Db,
        task_id: &str,
        session_id: &str,
        spawn_id: Option<String>,
        template_version: Option<u32>,
    ) -> Result<(AgentTask, TaskRowReadback), ErrorData> {
        Self::mutate_task_guarded(db, task_id, "task_claim", |predecessor, now| {
            if predecessor.dispatch_reservation.is_some() {
                return Err(active_dispatch_reservation_error(predecessor, "claim"));
            }
            if predecessor.state != TaskState::Todo {
                return Err(mcp_error(
                    error_codes::AGENT_TASK_INVALID_TRANSITION,
                    format!(
                        "agent_task {task_id:?} cannot be claimed: it is {}, not todo \
                             (already claimed or finished)",
                        predecessor.state.as_str()
                    ),
                ));
            }
            let mut task = predecessor.clone();
            task.attempts.push(TaskAttempt {
                attempt_id: next_attempt_id(predecessor)?,
                session_id: session_id.to_owned(),
                spawn_id: spawn_id.clone(),
                template_version,
                outcome: AttemptOutcome::Pending,
                started_unix_ms: now,
                ended_unix_ms: None,
                reason: None,
            });
            task.state = TaskState::InProgress;
            Ok(task)
        })
    }

    fn task_create_impl(
        &self,
        params: TaskCreateParams,
    ) -> Result<TaskMutationResponse, ErrorData> {
        if !is_kebab_id(&params.task_id) || params.task_id.len() > MAX_TASK_ID_CHARS {
            return Err(params_error(format!(
                "agent_task task_id must be non-empty [a-z0-9._-] and <= {MAX_TASK_ID_CHARS} chars"
            )));
        }
        if params.title.trim().is_empty() {
            return Err(params_error("agent_task title must not be empty"));
        }
        validate_text("title", &params.title, MAX_TITLE_CHARS)?;
        if let Some(description) = &params.description {
            validate_text("description", description, MAX_TEXT_CHARS)?;
        }
        if let Some(acceptance) = &params.acceptance {
            validate_text("acceptance", acceptance, MAX_TEXT_CHARS)?;
        }
        validate_priority(params.priority)?;
        if !is_kebab_id(&params.template_id) {
            return Err(params_error(
                "agent_task template_id must be non-empty [a-z0-9._-]",
            ));
        }
        validate_template_params(&params.template_params)?;

        let db = self.agent_task_db()?;
        let now = unix_time_ms_now();
        Self::prune_terminal_tasks(&db, now)?;
        let _mutation_guard = Self::acquire_task_queue_mutation_lock("task_create")?;
        let row_key = task_key(&params.task_id);
        let (task, written_row) = 'create: {
            for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
                let queue = Self::initialize_task_queue_state(&db, now)?;
                let sequence = Self::initialize_enqueue_seq_watermark(&db)?;
                let existing = db
                    .get_cf_revisioned(cf::CF_KV, row_key.as_bytes())
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "agent_task failed to read revisioned create key {row_key}: {error}"
                            ),
                        )
                    })?;
                if let Some(existing) = existing {
                    let existing_value = existing.value.ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "AGENT_TASK_ROW_EXPIRED: {row_key} has a physical expired envelope; \
                                 task ids are permanent identities and cannot be reused"
                            ),
                        )
                    })?;
                    let existing_task = decode_task(&row_key, &existing_value)?;
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        format!(
                            "agent_task {:?} already exists with enqueue_seq={}; use task_update to modify it",
                            existing_task.task_id, existing_task.enqueue_seq
                        ),
                    ));
                }
                let next_seq = sequence.value.checked_add(1).ok_or_else(|| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_TASK_SEQUENCE_EXHAUSTED: {TASK_SEQUENCE_KEY} is u64::MAX; no \
                             unique FIFO sequence remains"
                        ),
                    )
                })?;
                let next_queue = Self::next_task_queue_state(&queue.state, now)?;
                let task = AgentTask {
                    schema_version: TASK_SCHEMA_VERSION,
                    task_id: params.task_id.clone(),
                    state: TaskState::Todo,
                    title: params.title.clone(),
                    description: params.description.clone(),
                    acceptance: params.acceptance.clone(),
                    priority: params.priority,
                    template_id: params.template_id.clone(),
                    template_params: params.template_params.clone(),
                    enqueue_seq: next_seq,
                    mutation_generation: next_queue.mutation_generation,
                    attempts: Vec::new(),
                    dispatch_reservation: None,
                    review_reason: None,
                    created_unix_ms: now,
                    updated_unix_ms: now,
                };
                let encoded_task = encode_task(&task)?;
                let encoded_sequence = next_seq.to_string().into_bytes();
                let encoded_queue = Self::encode_task_queue_state(&next_queue)?;
                let outcome = db.mutate_batch_if_revisions_pressure_bypass(
                    cf::CF_KV,
                    [
                        RevisionGuard::new(
                            TASK_QUEUE_STATE_KEY.as_bytes(),
                            Some(queue.revision_sha256),
                        ),
                        RevisionGuard::new(
                            TASK_SEQUENCE_KEY.as_bytes(),
                            Some(sequence.revision_sha256),
                        ),
                        RevisionGuard::new(row_key.as_bytes(), None),
                    ],
                    std::iter::empty::<Vec<u8>>(),
                    [
                        (TASK_QUEUE_STATE_KEY.as_bytes().to_vec(), encoded_queue),
                        (
                            TASK_SEQUENCE_KEY.as_bytes().to_vec(),
                            encoded_sequence.clone(),
                        ),
                        (row_key.as_bytes().to_vec(), encoded_task.clone()),
                    ],
                );
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let stored_task = db.get_cf(cf::CF_KV, row_key.as_bytes()).map_err(
                            |read_error| {
                                mcp_error(
                                    read_error.code(),
                                    format!(
                                        "AGENT_TASK_CREATE_COMMIT_AMBIGUOUS: guarded create for \
                                         {row_key} failed ({error}) and exact item readback also \
                                         failed ({read_error}); inspect {TASK_SEQUENCE_KEY} and \
                                         {row_key} before retrying"
                                    ),
                                )
                            },
                        )?;
                        let stored_sequence = Self::read_enqueue_seq_watermark_revisioned(&db)?;
                        let stored_queue = Self::read_task_queue_state_revisioned(&db)?;
                        if let (Some(stored_task), Some(stored_sequence), Some(stored_queue)) =
                            (stored_task, stored_sequence, stored_queue)
                        {
                            let decoded = decode_task(&row_key, &stored_task)?;
                            if stored_task == encoded_task
                                && decoded.task_id == task.task_id
                                && decoded.enqueue_seq == next_seq
                                && decoded.mutation_generation == next_queue.mutation_generation
                                && stored_sequence.value >= next_seq
                                && stored_queue.state.mutation_generation
                                    >= next_queue.mutation_generation
                            {
                                tracing::warn!(
                                    code = "AGENT_TASK_CREATE_AMBIGUOUS_COMMIT_RECONCILED",
                                    task_id = %task.task_id,
                                    enqueue_seq = next_seq,
                                    watermark_after = stored_sequence.value,
                                    mutation_generation = next_queue.mutation_generation,
                                    queue_generation_after = stored_queue.state.mutation_generation,
                                    exact_bytes_match = stored_task == encoded_task,
                                    "separate physical item/watermark readback proved the guarded create committed"
                                );
                                break 'create (
                                    task,
                                    TaskRowReadback {
                                        cf_name: cf::CF_KV.to_owned(),
                                        row_key,
                                        value_len_bytes: encoded_task.len() as u64,
                                    },
                                );
                            }
                        }
                        return Err(mcp_error(
                            error.code(),
                            format!(
                                "AGENT_TASK_CREATE_NOT_COMMITTED: guarded create for {row_key} \
                                 failed: {error}; separate item/watermark/queue readback did not prove \
                                 this create committed, so no stale sequence retry was attempted"
                            ),
                        ));
                    }
                };
                if !outcome.applied {
                    tracing::warn!(
                        code = "AGENT_TASK_CREATE_REVISION_CONFLICT",
                        task_id = %task.task_id,
                        retry,
                        conflict_guard_index = outcome
                            .conflict
                            .as_ref()
                            .map(|conflict| conflict.guard_index),
                        "guarded task create conflicted; rereading task and watermark before recomputing"
                    );
                    continue;
                }
                let stored_task = db
                    .get_cf(cf::CF_KV, row_key.as_bytes())
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "AGENT_TASK_CREATE_READBACK_FAILED: committed_seq={} exact task \
                                 readback for {row_key} failed: {error}",
                                outcome.committed_seq
                            ),
                        )
                    })?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "AGENT_TASK_CREATE_READBACK_MISSING: committed_seq={} but task row \
                                 {row_key} is physically absent",
                                outcome.committed_seq
                            ),
                        )
                    })?;
                let decoded = decode_task(&row_key, &stored_task)?;
                let stored_sequence = Self::read_enqueue_seq_watermark_revisioned(&db)?
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "AGENT_TASK_CREATE_WATERMARK_MISSING: committed_seq={} but \
                                 {TASK_SEQUENCE_KEY} is physically absent",
                                outcome.committed_seq
                            ),
                        )
                    })?;
                let stored_queue =
                    Self::read_task_queue_state_revisioned(&db)?.ok_or_else(|| {
                        mcp_error(
                            error_codes::STORAGE_CORRUPTED,
                            format!(
                                "AGENT_TASK_CREATE_QUEUE_STATE_MISSING: committed_seq={} but \
                             {TASK_QUEUE_STATE_KEY} is physically absent",
                                outcome.committed_seq
                            ),
                        )
                    })?;
                if stored_task != encoded_task
                    || decoded.task_id != task.task_id
                    || decoded.enqueue_seq != next_seq
                    || decoded.mutation_generation != next_queue.mutation_generation
                    || stored_sequence.value < next_seq
                    || stored_queue.state.mutation_generation < next_queue.mutation_generation
                {
                    return Err(mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!(
                            "AGENT_TASK_CREATE_READBACK_DRIFT: committed_seq={} expected_task={} \
                             expected_enqueue_seq={next_seq} actual_task={} actual_enqueue_seq={} \
                             expected_generation={} actual_row_generation={} watermark={} \
                             queue_generation={}; task creation is disabled",
                            outcome.committed_seq,
                            task.task_id,
                            decoded.task_id,
                            decoded.enqueue_seq,
                            next_queue.mutation_generation,
                            decoded.mutation_generation,
                            stored_sequence.value,
                            stored_queue.state.mutation_generation
                        ),
                    ));
                }
                break 'create (
                    task,
                    TaskRowReadback {
                        cf_name: cf::CF_KV.to_owned(),
                        row_key,
                        value_len_bytes: encoded_task.len() as u64,
                    },
                );
            }
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "AGENT_TASK_CREATE_CONTENTION: task {:?} could not linearize after \
                     {TASK_CREATE_MAX_CONFLICT_RETRIES} guarded conflicts",
                    params.task_id
                ),
            ));
        };
        tracing::info!(
            code = "AGENT_TASK_CREATE",
            task_id = %task.task_id,
            priority = task.priority,
            template_id = %task.template_id,
            enqueue_seq = task.enqueue_seq,
            "readback=agent_tasks edge=create"
        );
        Ok(TaskMutationResponse {
            ok: true,
            task,
            written_row,
        })
    }

    fn task_get_impl(&self, params: TaskIdParams) -> Result<TaskGetResponse, ErrorData> {
        let db = self.agent_task_db()?;
        let task = Self::read_task(&db, &params.task_id)?
            .ok_or_else(|| task_not_found(&params.task_id))?;
        Ok(TaskGetResponse { ok: true, task })
    }

    fn task_update_impl(
        &self,
        params: TaskUpdateParams,
    ) -> Result<TaskMutationResponse, ErrorData> {
        if let Some(priority) = params.priority {
            validate_priority(priority)?;
        }
        if let Some(title) = &params.title {
            if title.trim().is_empty() {
                return Err(params_error("agent_task title must not be empty"));
            }
            validate_text("title", title, MAX_TITLE_CHARS)?;
        }
        if let Some(description) = &params.description {
            validate_text("description", description, MAX_TEXT_CHARS)?;
        }
        if let Some(acceptance) = &params.acceptance {
            validate_text("acceptance", acceptance, MAX_TEXT_CHARS)?;
        }
        let db = self.agent_task_db()?;
        let task_id = params.task_id.clone();
        let (task, written_row) =
            Self::mutate_task_guarded(&db, &task_id, "task_update", |predecessor, now| {
                if predecessor.dispatch_reservation.is_some() {
                    return Err(active_dispatch_reservation_error(predecessor, "update"));
                }
                let mut task = predecessor.clone();
                if let Some(priority) = params.priority {
                    task.priority = priority;
                }
                if let Some(title) = &params.title {
                    task.title.clone_from(title);
                }
                if let Some(description) = &params.description {
                    task.description = Some(description.clone());
                }
                if let Some(acceptance) = &params.acceptance {
                    task.acceptance = Some(acceptance.clone());
                }
                if let Some(target) = params.state
                    && target != task.state
                {
                    if target == TaskState::InProgress {
                        return Err(mcp_error(
                            error_codes::AGENT_TASK_INVALID_TRANSITION,
                            format!(
                                "agent_task {:?} cannot enter in_progress through task_update; \
                                 use task_claim or task_dispatch_once so a durable attempt \
                                 identity is created atomically",
                                task.task_id
                            ),
                        ));
                    }
                    if !task.state.can_transition_to(target) {
                        return Err(mcp_error(
                            error_codes::AGENT_TASK_INVALID_TRANSITION,
                            format!(
                                "agent_task {:?} cannot move {} -> {}; valid targets: {:?}",
                                task.task_id,
                                task.state.as_str(),
                                target.as_str(),
                                task.state.allowed_targets()
                            ),
                        ));
                    }
                    let outcome = match target {
                        TaskState::Review | TaskState::Done => Some(AttemptOutcome::Succeeded),
                        TaskState::Cancelled | TaskState::Todo => Some(AttemptOutcome::Failed),
                        TaskState::InProgress => None,
                    };
                    if let Some(outcome) = outcome {
                        for attempt in &mut task.attempts {
                            if attempt.outcome == AttemptOutcome::Pending {
                                attempt.outcome = outcome;
                                attempt.ended_unix_ms = Some(now);
                                attempt.reason.clone_from(&params.reason);
                            }
                        }
                    }
                    task.review_reason = if target == TaskState::Review {
                        params.reason.clone()
                    } else {
                        None
                    };
                    task.state = target;
                }
                Ok(task)
            })?;
        tracing::info!(
            code = "AGENT_TASK_UPDATE",
            task_id = %task.task_id,
            state = task.state.as_str(),
            "readback=agent_tasks edge=update"
        );
        Ok(TaskMutationResponse {
            ok: true,
            task,
            written_row,
        })
    }

    fn task_claim_impl(&self, params: TaskClaimParams) -> Result<TaskMutationResponse, ErrorData> {
        if params.session_id.trim().is_empty() {
            return Err(params_error(
                "agent_task claim session_id must not be empty",
            ));
        }
        let db = self.agent_task_db()?;
        let (task, written_row) =
            Self::claim_internal(&db, &params.task_id, &params.session_id, None, None)?;
        tracing::info!(
            code = "AGENT_TASK_CLAIM",
            task_id = %task.task_id,
            session_id = %params.session_id,
            "readback=agent_tasks edge=claim"
        );
        Ok(TaskMutationResponse {
            ok: true,
            task,
            written_row,
        })
    }

    fn task_cancel_impl(
        &self,
        params: TaskCancelParams,
    ) -> Result<TaskMutationResponse, ErrorData> {
        let db = self.agent_task_db()?;
        // Terminal re-cancel is a read-only idempotent no-op. `Cancelled` is a
        // transition-matrix sink, so `Cancelled -> Cancelled` carries no state
        // delta; delegating to the guarded update path would still commit a write
        // (bumping the queue mutation_generation and moving updated_unix_ms on a
        // terminal row), leaving a misleading audit trail for a request that
        // changed nothing. Short-circuit with a read-only readback instead. A
        // `Cancelled` row can never leave that state, so this early observation
        // can never be stale in a way that hides a pending transition.
        //
        // Every non-cancelled state (including the other terminal state, `done`)
        // falls through to `task_update_impl`, where the transition matrix still
        // rejects `Done -> Cancelled` with AGENT_TASK_INVALID_TRANSITION and
        // performs the guarded write for live states.
        let existing = Self::read_task_row_revisioned(&db, &params.task_id)?
            .ok_or_else(|| task_not_found(&params.task_id))?;
        if existing.task.state == TaskState::Cancelled {
            let row_key = String::from_utf8(existing.key).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("agent task physical key is not UTF-8: {error}"),
                )
            })?;
            let value_len_bytes = encode_task(&existing.task)?.len() as u64;
            tracing::info!(
                code = "AGENT_TASK_CANCEL_NOOP",
                task_id = %existing.task.task_id,
                state = existing.task.state.as_str(),
                mutation_generation = existing.task.mutation_generation,
                "readback=agent_tasks edge=cancel_noop terminal re-cancel is a read-only idempotent success"
            );
            return Ok(TaskMutationResponse {
                ok: true,
                task: existing.task,
                written_row: TaskRowReadback {
                    cf_name: cf::CF_KV.to_owned(),
                    row_key,
                    value_len_bytes,
                },
            });
        }
        self.task_update_impl(TaskUpdateParams {
            task_id: params.task_id,
            state: Some(TaskState::Cancelled),
            reason: params.reason,
            priority: None,
            title: None,
            description: None,
            acceptance: None,
        })
    }

    fn task_list_impl(&self, params: TaskListParams) -> Result<TaskListResponse, ErrorData> {
        let db = self.agent_task_db()?;
        Self::prune_terminal_tasks(&db, unix_time_ms_now())?;
        // Lazy reconcile on read so orphaned in_progress tasks surface even
        // without an explicit reconcile or a daemon restart hook.
        let (_, flagged) = self.reconcile_tasks(&db)?;
        let mut tasks = Self::read_all_tasks(&db)?;
        if let Some(state) = params.state {
            tasks.retain(|task| task.state == state);
        }
        let mut ordered = order_for_list(tasks);
        ordered.truncate(params.max);
        Ok(TaskListResponse {
            ok: true,
            count: ordered.len(),
            tasks: ordered,
            reconciled_orphans: flagged,
        })
    }

    fn task_next_impl(&self, params: TaskNextParams) -> Result<TaskNextResponse, ErrorData> {
        let db = self.agent_task_db()?;
        Self::prune_terminal_tasks(&db, unix_time_ms_now())?;
        self.reconcile_tasks(&db)?;
        let tasks = Self::read_all_tasks(&db)?;
        let in_flight = tasks.iter().filter(|task| task.is_in_flight()).count();
        let decision = dispatch_decision(&tasks, params.concurrency_cap);
        let (decision_str, task) = match decision {
            DispatchDecision::Empty => ("empty".to_owned(), None),
            DispatchDecision::AtCapacity { in_flight } => {
                (format!("at_capacity:{in_flight}"), None)
            }
            DispatchDecision::Dispatch { task_id } => {
                let task = Self::read_task(&db, &task_id)?;
                ("dispatch".to_owned(), task)
            }
        };
        Ok(TaskNextResponse {
            ok: true,
            decision: decision_str,
            task,
            in_flight,
            concurrency_cap: params.concurrency_cap,
        })
    }

    fn task_reconcile_impl(&self) -> Result<TaskReconcileResponse, ErrorData> {
        let db = self.agent_task_db()?;
        Self::prune_terminal_tasks(&db, unix_time_ms_now())?;
        let (scanned, flagged) = self.reconcile_tasks(&db)?;
        tracing::info!(
            code = "AGENT_TASK_RECONCILE",
            scanned_in_progress = scanned,
            flagged = flagged.len(),
            "readback=agent_tasks edge=reconcile"
        );
        Ok(TaskReconcileResponse {
            ok: true,
            scanned_in_progress: scanned,
            flagged_orphans: flagged,
        })
    }

    pub(crate) fn task_repair_sequence_impl(
        &self,
        params: TaskSequenceRepairParams,
    ) -> Result<TaskSequenceRepairResponse, ErrorData> {
        if params.reason.trim().is_empty() {
            return Err(params_error(
                "task sequence repair requires a non-empty audit reason",
            ));
        }
        validate_text("repair reason", &params.reason, MAX_TEXT_CHARS)?;
        let db = self.agent_task_db()?;
        for retry in 0..TASK_CREATE_MAX_CONFLICT_RETRIES {
            let current = db
                .get_cf_revisioned(cf::CF_KV, TASK_SEQUENCE_KEY.as_bytes())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("read raw task sequence state for explicit repair: {error}"),
                    )
                })?;
            let expected_revision = current
                .as_ref()
                .map(|revisioned| revisioned.revision_sha256);
            let previous_decodable_sequence = current
                .as_ref()
                .and_then(|revisioned| revisioned.value.as_deref())
                .and_then(|value| Self::decode_enqueue_seq_watermark(value).ok());
            if current.is_some()
                && previous_decodable_sequence.is_none()
                && params.minimum_last_enqueue_seq == 0
            {
                return Err(params_error(
                    "task sequence repair found an undecodable physical watermark; \
                     minimum_last_enqueue_seq must be the non-zero last known-good physical \
                     sequence so repair cannot reuse an unknown allocation",
                ));
            }
            let raw_rows = Self::scan_raw_task_values(&db)?;
            let mut observed_max_task_sequence = 0_u64;
            let mut unobservable_task_sequences = 0_usize;
            for (key, encoded) in &raw_rows {
                let key_text = String::from_utf8_lossy(key);
                match decode_task(&key_text, encoded) {
                    Ok(task) => {
                        observed_max_task_sequence =
                            observed_max_task_sequence.max(task.enqueue_seq);
                    }
                    Err(error) => {
                        unobservable_task_sequences = unobservable_task_sequences.saturating_add(1);
                        tracing::error!(
                            code = "AGENT_TASK_SEQUENCE_REPAIR_ROW_UNOBSERVABLE",
                            row_key = %key_text,
                            error_code = %error_code_str(&error),
                            error = %error.message,
                            "task row sequence is unobservable during explicit watermark repair"
                        );
                    }
                }
            }
            if unobservable_task_sequences > 0 && params.minimum_last_enqueue_seq == 0 {
                return Err(params_error(format!(
                    "task sequence repair found {unobservable_task_sequences} task row(s) whose \
                     sequence is unobservable; minimum_last_enqueue_seq must be the non-zero last \
                     known-good physical allocation so repair cannot reuse an unknown sequence"
                )));
            }
            let repaired_sequence = params
                .minimum_last_enqueue_seq
                .max(observed_max_task_sequence)
                .max(previous_decodable_sequence.unwrap_or(0));
            let encoded = repaired_sequence.to_string().into_bytes();
            let outcome = db
                .mutate_batch_if_revisions_pressure_bypass(
                    cf::CF_KV,
                    [RevisionGuard::new(
                        TASK_SEQUENCE_KEY.as_bytes(),
                        expected_revision,
                    )],
                    std::iter::empty::<Vec<u8>>(),
                    [(TASK_SEQUENCE_KEY.as_bytes().to_vec(), encoded.clone())],
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "TASK_SEQUENCE_REPAIR_COMMIT_FAILED: explicit guarded repair failed: \
                             {error}; key={TASK_SEQUENCE_KEY} requested_minimum={} \
                             observed_max={observed_max_task_sequence}",
                            params.minimum_last_enqueue_seq
                        ),
                    )
                })?;
            if !outcome.applied {
                tracing::warn!(
                    code = "AGENT_TASK_SEQUENCE_REPAIR_CONFLICT",
                    retry,
                    "task sequence changed during explicit repair; rereading every repair SoT"
                );
                continue;
            }
            let readback = Self::read_enqueue_seq_watermark_revisioned(&db)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "TASK_SEQUENCE_REPAIR_READBACK_MISSING: committed_seq={} key={TASK_SEQUENCE_KEY}",
                        outcome.committed_seq
                    ),
                )
            })?;
            if readback.value != repaired_sequence {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "TASK_SEQUENCE_REPAIR_READBACK_DRIFT: committed_seq={} expected={} actual={}",
                        outcome.committed_seq, repaired_sequence, readback.value
                    ),
                ));
            }
            tracing::warn!(
                code = "AGENT_TASK_SEQUENCE_REPAIRED",
                reason = %params.reason,
                observed_task_rows = raw_rows.len(),
                unobservable_task_sequences,
                observed_max_task_sequence,
                previous_decodable_sequence,
                requested_minimum_sequence = params.minimum_last_enqueue_seq,
                repaired_sequence,
                committed_seq = outcome.committed_seq,
                "explicit guarded task sequence repair completed with separate physical readback"
            );
            return Ok(TaskSequenceRepairResponse {
                ok: true,
                observed_task_rows: raw_rows.len(),
                unobservable_task_sequences,
                observed_max_task_sequence,
                previous_decodable_sequence,
                requested_minimum_sequence: params.minimum_last_enqueue_seq,
                repaired_sequence,
                committed_seq: outcome.committed_seq,
                written_row: TaskRowReadback {
                    cf_name: cf::CF_KV.to_owned(),
                    row_key: TASK_SEQUENCE_KEY.to_owned(),
                    value_len_bytes: encoded.len() as u64,
                },
            });
        }
        Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "TASK_SEQUENCE_REPAIR_CONTENTION: guarded repair conflicted \
                 {TASK_CREATE_MAX_CONFLICT_RETRIES} times"
            ),
        ))
    }

    pub(crate) fn task_repair_queue_state_impl(
        &self,
        params: TaskQueueStateRepairParams,
    ) -> Result<TaskQueueStateRepairResponse, ErrorData> {
        if params.reason.trim().is_empty() {
            return Err(params_error(
                "task queue-state repair requires a non-empty audit reason",
            ));
        }
        validate_text("repair reason", &params.reason, MAX_TEXT_CHARS)?;
        let expected_revision = params
            .expected_revision_sha256
            .as_deref()
            .map(|revision| parse_revision_sha256(revision, "expected_revision_sha256"))
            .transpose()?;
        let db = self.agent_task_db()?;
        let _mutation_guard = Self::acquire_task_queue_mutation_lock("task_repair_queue_state")?;
        let current = db
            .get_cf_revisioned(cf::CF_KV, TASK_QUEUE_STATE_KEY.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("read raw task queue state for explicit repair: {error}"),
                )
            })?;
        let actual_revision = current
            .as_ref()
            .map(|revisioned| revisioned.revision_sha256);
        if actual_revision != expected_revision {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "TASK_QUEUE_STATE_REPAIR_REVISION_MISMATCH: key={TASK_QUEUE_STATE_KEY} \
                     expected_revision={} actual_revision={}; perform a new separate physical \
                     read before retrying",
                    expected_revision
                        .as_ref()
                        .map(revision_sha256_hex)
                        .unwrap_or_else(|| "<absent>".to_owned()),
                    actual_revision
                        .as_ref()
                        .map(revision_sha256_hex)
                        .unwrap_or_else(|| "<absent>".to_owned())
                ),
            ));
        }
        let previous_decodable_generation = current
            .as_ref()
            .and_then(|revisioned| revisioned.value.as_deref())
            .and_then(|value| synapse_storage::decode_json::<TaskQueueState>(value).ok())
            .filter(|state| state.schema_version == TASK_QUEUE_STATE_SCHEMA_VERSION)
            .map(|state| state.mutation_generation);
        let raw_rows = Self::scan_raw_task_values(&db)?;
        let mut observed_max_task_generation = 0_u64;
        let mut unobservable_task_generations = 0_usize;
        for (key, encoded) in &raw_rows {
            let key_text = String::from_utf8_lossy(key);
            match decode_task(&key_text, encoded) {
                Ok(task) => {
                    observed_max_task_generation =
                        observed_max_task_generation.max(task.mutation_generation);
                }
                Err(error) => {
                    unobservable_task_generations = unobservable_task_generations.saturating_add(1);
                    tracing::error!(
                        code = "AGENT_TASK_QUEUE_REPAIR_ROW_UNOBSERVABLE",
                        row_key = %key_text,
                        error_code = %error_code_str(&error),
                        error = %error.message,
                        "task row generation is unobservable during explicit queue-state repair"
                    );
                }
            }
        }
        let current_is_unobservable = current.is_some() && previous_decodable_generation.is_none();
        if (current_is_unobservable || unobservable_task_generations > 0)
            && params.minimum_mutation_generation == 0
        {
            return Err(params_error(format!(
                "task queue-state repair has unobservable durable history \
                 (queue_state_unobservable={current_is_unobservable}, \
                 unobservable_task_rows={unobservable_task_generations}); \
                 minimum_mutation_generation must be a non-zero last known-good physical floor"
            )));
        }
        let repaired_generation = params
            .minimum_mutation_generation
            .max(observed_max_task_generation)
            .max(previous_decodable_generation.unwrap_or(0));
        let repaired = TaskQueueState {
            schema_version: TASK_QUEUE_STATE_SCHEMA_VERSION,
            mutation_generation: repaired_generation,
            updated_unix_ms: unix_time_ms_now(),
        };
        let encoded = Self::encode_task_queue_state(&repaired)?;
        let outcome = db.mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [RevisionGuard::new(
                TASK_QUEUE_STATE_KEY.as_bytes(),
                expected_revision,
            )],
            std::iter::empty::<Vec<u8>>(),
            [(TASK_QUEUE_STATE_KEY.as_bytes().to_vec(), encoded.clone())],
        );
        let committed_seq = match outcome {
            Ok(outcome) if outcome.applied => Some(outcome.committed_seq),
            Ok(outcome) => {
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "TASK_QUEUE_STATE_REPAIR_CONFLICT: exact revision guard changed before \
                         commit; conflict_guard={:?}; perform a new separate physical read",
                        outcome
                            .conflict
                            .as_ref()
                            .map(|conflict| conflict.guard_index)
                    ),
                ));
            }
            Err(error) => {
                let readback = db
                    .get_cf(cf::CF_KV, TASK_QUEUE_STATE_KEY.as_bytes())
                    .map_err(|read_error| {
                        mcp_error(
                            read_error.code(),
                            format!(
                                "TASK_QUEUE_STATE_REPAIR_COMMIT_AMBIGUOUS: commit failed \
                                 ({error}) and exact readback failed ({read_error})"
                            ),
                        )
                    })?;
                if readback.as_deref() == Some(encoded.as_slice()) {
                    tracing::warn!(
                        code = "AGENT_TASK_QUEUE_REPAIR_AMBIGUOUS_COMMIT_RECONCILED",
                        repaired_generation,
                        "exact physical readback proved the queue-state repair committed"
                    );
                    None
                } else {
                    return Err(mcp_error(
                        error.code(),
                        format!(
                            "TASK_QUEUE_STATE_REPAIR_NOT_COMMITTED: guarded repair failed: \
                             {error}; exact physical readback did not prove the requested bytes"
                        ),
                    ));
                }
            }
        };
        let readback = Self::read_task_queue_state_revisioned(&db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                "TASK_QUEUE_STATE_REPAIR_READBACK_MISSING: repaired row is physically absent",
            )
        })?;
        if readback.state != repaired {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "TASK_QUEUE_STATE_REPAIR_READBACK_DRIFT: expected_generation={} \
                     actual_generation={}",
                    repaired_generation, readback.state.mutation_generation
                ),
            ));
        }
        tracing::warn!(
            code = "AGENT_TASK_QUEUE_STATE_REPAIRED",
            reason = %params.reason,
            observed_task_rows = raw_rows.len(),
            unobservable_task_generations,
            observed_max_task_generation,
            previous_decodable_generation,
            requested_minimum_generation = params.minimum_mutation_generation,
            repaired_generation,
            committed_seq,
            "explicit revision-bound task queue-state repair completed with physical readback"
        );
        Ok(TaskQueueStateRepairResponse {
            ok: true,
            observed_task_rows: raw_rows.len(),
            unobservable_task_generations,
            observed_max_task_generation,
            previous_decodable_generation,
            requested_minimum_generation: params.minimum_mutation_generation,
            repaired_generation,
            previous_revision_sha256: actual_revision.as_ref().map(revision_sha256_hex),
            committed_seq,
            written_row: TaskRowReadback {
                cf_name: cf::CF_KV.to_owned(),
                row_key: TASK_QUEUE_STATE_KEY.to_owned(),
                value_len_bytes: encoded.len() as u64,
            },
        })
    }

    pub(crate) fn task_repair_row_impl(
        &self,
        params: TaskRowRepairParams,
    ) -> Result<TaskRowRepairResponse, ErrorData> {
        if !is_kebab_id(&params.task_id) || params.task_id.len() > MAX_TASK_ID_CHARS {
            return Err(params_error(format!(
                "agent_task task_id must be non-empty [a-z0-9._-] and <= {MAX_TASK_ID_CHARS} chars"
            )));
        }
        if params.reason.trim().is_empty() {
            return Err(params_error(
                "task row repair requires a non-empty audit reason",
            ));
        }
        validate_text("repair reason", &params.reason, MAX_TEXT_CHARS)?;
        let expected_revision =
            parse_revision_sha256(&params.expected_revision_sha256, "expected_revision_sha256")?;
        if params.replacement.task_id != params.task_id {
            return Err(params_error(format!(
                "task row repair identity mismatch: task_id={:?} replacement.task_id={:?}",
                params.task_id, params.replacement.task_id
            )));
        }
        if params.replacement.mutation_generation != 0 {
            return Err(params_error(
                "task row repair replacement.mutation_generation must be 0; the repair assigns the next durable generation",
            ));
        }
        let db = self.agent_task_db()?;
        let _mutation_guard = Self::acquire_task_queue_mutation_lock("task_repair_row")?;
        let row_key = task_key(&params.task_id);
        let current = db
            .get_cf_revisioned(cf::CF_KV, row_key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("read raw task row {row_key} for explicit repair: {error}"),
                )
            })?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::AGENT_TASK_NOT_FOUND,
                    format!(
                        "TASK_ROW_REPAIR_ABSENT: {row_key} is physically absent; repair never creates a new task identity"
                    ),
                )
            })?;
        if current.revision_sha256 != expected_revision {
            return Err(mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "TASK_ROW_REPAIR_REVISION_MISMATCH: row={row_key} expected_revision={} \
                     actual_revision={}; perform a new separate physical read before retrying",
                    params.expected_revision_sha256,
                    revision_sha256_hex(&current.revision_sha256)
                ),
            ));
        }
        let previous_value_len_bytes = current.value.as_ref().map_or(0, |value| value.len() as u64);
        let queue = Self::read_task_queue_state_revisioned(&db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "TASK_ROW_REPAIR_QUEUE_STATE_MISSING: {TASK_QUEUE_STATE_KEY} must be explicitly repaired before a task row"
                ),
            )
        })?;
        let sequence = Self::read_enqueue_seq_watermark_revisioned(&db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "TASK_ROW_REPAIR_SEQUENCE_MISSING: {TASK_SEQUENCE_KEY} must be explicitly repaired before a task row"
                ),
            )
        })?;
        if params.replacement.enqueue_seq > sequence.value {
            return Err(params_error(format!(
                "task row repair replacement.enqueue_seq={} exceeds durable watermark={}; \
                 repair the watermark to an operator-observed monotonic floor first",
                params.replacement.enqueue_seq, sequence.value
            )));
        }
        let mut unobservable_other_task_rows = 0_usize;
        for (key, encoded) in Self::scan_raw_task_values(&db)? {
            if key == row_key.as_bytes() {
                continue;
            }
            let key_text = String::from_utf8_lossy(&key);
            match decode_task(&key_text, &encoded) {
                Ok(task) if task.enqueue_seq == params.replacement.enqueue_seq => {
                    return Err(params_error(format!(
                        "task row repair replacement.enqueue_seq={} is already owned by task {:?}; enqueue identities must remain unique",
                        params.replacement.enqueue_seq, task.task_id
                    )));
                }
                Ok(_) => {}
                Err(error) => {
                    unobservable_other_task_rows = unobservable_other_task_rows.saturating_add(1);
                    tracing::error!(
                        code = "AGENT_TASK_ROW_REPAIR_OTHER_ROW_UNOBSERVABLE",
                        row_key = %key_text,
                        error_code = %error_code_str(&error),
                        error = %error.message,
                        "another corrupt task row remains fail-closed during explicit row repair"
                    );
                }
            }
        }
        let now = unix_time_ms_now();
        let next_queue = Self::next_task_queue_state(&queue.state, now)?;
        let mut replacement = params.replacement;
        replacement.schema_version = TASK_SCHEMA_VERSION;
        replacement.mutation_generation = next_queue.mutation_generation;
        let encoded_task = encode_task(&replacement)?;
        decode_task(&row_key, &encoded_task)?;
        let encoded_queue = Self::encode_task_queue_state(&next_queue)?;
        let outcome = db.mutate_batch_if_revisions_pressure_bypass(
            cf::CF_KV,
            [
                RevisionGuard::new(TASK_QUEUE_STATE_KEY.as_bytes(), Some(queue.revision_sha256)),
                RevisionGuard::new(row_key.as_bytes(), Some(expected_revision)),
            ],
            std::iter::empty::<Vec<u8>>(),
            [
                (TASK_QUEUE_STATE_KEY.as_bytes().to_vec(), encoded_queue),
                (row_key.as_bytes().to_vec(), encoded_task.clone()),
            ],
        );
        let committed_seq = match outcome {
            Ok(outcome) if outcome.applied => Some(outcome.committed_seq),
            Ok(outcome) => {
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "TASK_ROW_REPAIR_CONFLICT: row={row_key} conflict_guard={:?}; queue or \
                         row changed after the physical observation, so no repair was applied",
                        outcome
                            .conflict
                            .as_ref()
                            .map(|conflict| conflict.guard_index)
                    ),
                ));
            }
            Err(error) => {
                let stored = db
                    .get_cf(cf::CF_KV, row_key.as_bytes())
                    .map_err(|read_error| {
                        mcp_error(
                            read_error.code(),
                            format!(
                                "TASK_ROW_REPAIR_COMMIT_AMBIGUOUS: row={row_key} commit failed \
                             ({error}) and exact readback failed ({read_error})"
                            ),
                        )
                    })?;
                let queue_readback = Self::read_task_queue_state_revisioned(&db)?;
                if stored.as_deref() == Some(encoded_task.as_slice())
                    && queue_readback.as_ref().is_some_and(|readback| {
                        readback.state.mutation_generation >= next_queue.mutation_generation
                    })
                {
                    tracing::warn!(
                        code = "AGENT_TASK_ROW_REPAIR_AMBIGUOUS_COMMIT_RECONCILED",
                        task_id = %replacement.task_id,
                        mutation_generation = next_queue.mutation_generation,
                        "exact task/queue physical readback proved the repair committed"
                    );
                    None
                } else {
                    return Err(mcp_error(
                        error.code(),
                        format!(
                            "TASK_ROW_REPAIR_NOT_COMMITTED: row={row_key} guarded repair failed: \
                             {error}; exact task/queue readback did not prove the requested state"
                        ),
                    ));
                }
            }
        };
        let row_readback =
            Self::read_task_row_revisioned(&db, &params.task_id)?.ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("TASK_ROW_REPAIR_READBACK_MISSING: {row_key} is physically absent"),
                )
            })?;
        if row_readback.task != replacement {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "TASK_ROW_REPAIR_READBACK_DRIFT: row={row_key} exact decoded replacement differs after commit"
                ),
            ));
        }
        let queue_readback = Self::read_task_queue_state_revisioned(&db)?.ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                "TASK_ROW_REPAIR_QUEUE_READBACK_MISSING: queue state disappeared after commit",
            )
        })?;
        if queue_readback.state.mutation_generation < next_queue.mutation_generation {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "TASK_ROW_REPAIR_QUEUE_READBACK_DRIFT: expected_generation_at_least={} actual_generation={}",
                    next_queue.mutation_generation, queue_readback.state.mutation_generation
                ),
            ));
        }
        tracing::warn!(
            code = "AGENT_TASK_ROW_REPAIRED",
            reason = %params.reason,
            task_id = %replacement.task_id,
            previous_revision_sha256 = %params.expected_revision_sha256,
            previous_value_len_bytes,
            unobservable_other_task_rows,
            mutation_generation = replacement.mutation_generation,
            committed_seq,
            "explicit revision-bound task row repair completed with physical task/queue readback"
        );
        Ok(TaskRowRepairResponse {
            ok: true,
            previous_revision_sha256: params.expected_revision_sha256,
            previous_value_len_bytes,
            unobservable_other_task_rows,
            committed_seq,
            task: replacement,
            written_row: TaskRowReadback {
                cf_name: cf::CF_KV.to_owned(),
                row_key,
                value_len_bytes: encoded_task.len() as u64,
            },
        })
    }

    async fn task_dispatch_once_impl(
        &self,
        params: TaskDispatchOnceParams,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<TaskDispatchOnceResponse, ErrorData> {
        let dispatch_activity =
            super::m4_tools::AgentSpawnInFlightGuard::enter("mcp_task_dispatch_outer")?;
        let db = self.agent_task_db()?;
        Self::prune_terminal_tasks(&db, unix_time_ms_now())?;
        self.reconcile_tasks(&db)?;
        let wait_timeout_ms = dashboard_task_dispatch_wait_timeout_ms(params.wait_timeout_ms);
        let (task, reservation_id, in_flight) = match Self::reserve_next_dispatch(
            &db,
            params.concurrency_cap,
            wait_timeout_ms,
            "mcp_task_dispatch_reserve",
        )? {
            DispatchReservationOutcome::Empty { in_flight } => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: "empty".to_owned(),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchReservationOutcome::AtCapacity { in_flight } => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: format!("at_capacity:{in_flight}"),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchReservationOutcome::Reserved {
                task,
                reservation_id,
                in_flight_before,
            } => (task, reservation_id, in_flight_before),
        };
        let task_id = task.task_id.clone();
        let request = ActSpawnAgentRequest {
            template_id: Some(task.template_id.clone()),
            template_version: None,
            template_params: task.template_params.clone(),
            cli: None,
            kind: None,
            model: None,
            model_ref: None,
            prompt: None,
            target: None,
            working_dir: None,
            mcp_url: params.mcp_url,
            wait_timeout_ms,
            hold_open_ms: default_agent_spawn_hold_open_ms(),
            require_approval_gate: crate::m4::default_require_approval_gate(),
        };

        tracing::info!(
            code = "AGENT_TASK_DISPATCH_SPAWN",
            task_id = %task_id,
            template_id = %task.template_id,
            "readback=agent_tasks edge=dispatch_spawn_begin"
        );

        let spawn_result = match dispatch_activity.ensure("mcp_task_dispatch_before_spawn") {
            Ok(()) => self.spawn_agent_journaled(request, request_context).await,
            Err(error) => Err(error),
        };
        let response = match spawn_result {
            Ok(response) => response,
            Err(spawn_error) => {
                let error_code = error_code_str(&spawn_error);
                let reason = format!(
                    "dispatch spawn failed [{error_code}]: {}",
                    spawn_error.message
                );
                if let Err(settle_error) = Self::fail_dispatch_reservation(
                    &db,
                    &task_id,
                    &reservation_id,
                    reason.clone(),
                    "mcp_task_dispatch_spawn_failed",
                ) {
                    return Err(mcp_error(
                        error_codes::STORAGE_WRITE_FAILED,
                        format!(
                            "AGENT_TASK_DISPATCH_FAILURE_SETTLE_FAILED: task={task_id:?} \
                             reservation={reservation_id} spawn_error={}; settle_error={}; \
                             durable reservation remains authoritative and requires reconciliation",
                            spawn_error.message, settle_error.message
                        ),
                    ));
                }
                tracing::error!(
                    code = "AGENT_TASK_DISPATCH_SPAWN_FAILED",
                    task_id = %task_id,
                    template_id = %task.template_id,
                    error_code = %error_code,
                    "readback=agent_tasks edge=dispatch_spawn_failed reason={reason}"
                );
                return Err(spawn_error);
            }
        };
        if let Err(error) = dispatch_activity.ensure("mcp_task_dispatch_after_spawn") {
            let cleanup = self
                .cleanup_spawn_response_after_operator_panic(
                    &response,
                    "mcp_task_dispatch_after_spawn",
                )
                .await;
            let reason = format!(
                "dispatch spawn superseded by operator panic [{}]: {}; cleanup={cleanup}",
                error_code_str(&error),
                error.message
            );
            Self::fail_dispatch_reservation(
                &db,
                &task_id,
                &reservation_id,
                reason,
                "mcp_task_dispatch_operator_panic",
            )?;
            return Err(error);
        }

        let claimed = match Self::bind_dispatch_reservation(
            &db,
            &task_id,
            &reservation_id,
            &response.session_id,
            response.spawn_id.clone(),
            response.template_version,
            "mcp_task_dispatch_bind",
        ) {
            Ok((task, _readback)) => task,
            Err(claim_error) => {
                let cleanup = self
                    .cleanup_spawn_response_after_operator_panic(
                        &response,
                        "mcp_task_dispatch_bind_failed",
                    )
                    .await;
                tracing::error!(
                    code = "AGENT_TASK_DISPATCH_BIND_FAILED",
                    task_id = %task_id,
                    spawn_id = %response.spawn_id,
                    session_id = %response.session_id,
                    cleanup = %cleanup,
                    "guarded dispatch bind failed; spawned agent cleanup was attempted"
                );
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "task_dispatch_once spawned agent session {:?} (spawn {:?}) for task \
                         {task_id:?} but could not bind durable reservation {reservation_id}; \
                         cleanup={cleanup}. The task reservation remains authoritative for \
                         reconcile. Underlying error: {}",
                        response.session_id, response.spawn_id, claim_error.message
                    ),
                ));
            }
        };

        tracing::info!(
            code = "AGENT_TASK_DISPATCH",
            task_id = %task_id,
            spawn_id = %response.spawn_id,
            session_id = %response.session_id,
            template_id = %task.template_id,
            template_version = response.template_version.unwrap_or(0),
            "readback=agent_tasks edge=dispatch"
        );

        Ok(TaskDispatchOnceResponse {
            ok: true,
            decision: "dispatched".to_owned(),
            task: Some(claimed),
            spawn: Some(DispatchSpawnReadback {
                spawn_id: response.spawn_id,
                session_id: response.session_id,
                template_id: response.template_id,
                template_version: response.template_version,
                agent_process_id: response.agent_process_id,
                launched_at_unix_ms: response.launched_at_unix_ms,
                task_started_at_unix_ms: response.task_started_at_unix_ms,
            }),
            in_flight: in_flight + 1,
            concurrency_cap: params.concurrency_cap,
        })
    }

    pub(crate) fn dashboard_task_snapshot(
        &self,
        max: usize,
    ) -> Result<TaskListResponse, ErrorData> {
        self.task_list_impl(TaskListParams { state: None, max })
    }

    pub(crate) fn dashboard_task_next(
        &self,
        concurrency_cap: usize,
    ) -> Result<TaskNextResponse, ErrorData> {
        self.task_next_impl(TaskNextParams { concurrency_cap })
    }

    pub(crate) fn dashboard_task_create(
        &self,
        params: TaskCreateParams,
    ) -> Result<TaskMutationResponse, ErrorData> {
        self.task_create_impl(params)
    }

    pub(crate) fn dashboard_task_update(
        &self,
        params: TaskUpdateParams,
    ) -> Result<TaskMutationResponse, ErrorData> {
        self.task_update_impl(params)
    }

    pub(crate) async fn dashboard_task_cancel(
        &self,
        params: TaskCancelParams,
    ) -> Result<DashboardTaskCancelResponse, ErrorData> {
        let db = self.agent_task_db()?;
        let before = Self::read_task(&db, &params.task_id)?
            .ok_or_else(|| task_not_found(&params.task_id))?;
        if before.dispatch_reservation.is_some() {
            return Err(active_dispatch_reservation_error(&before, "cancel"));
        }
        let interrupt_target = before.live_attempt().and_then(|attempt| {
            attempt
                .spawn_id
                .as_ref()
                .filter(|spawn_id| !spawn_id.trim().is_empty())
                .cloned()
                .or_else(|| {
                    (!attempt.session_id.trim().is_empty()).then(|| attempt.session_id.clone())
                })
        });
        let interrupt = if let Some(session_id) = interrupt_target {
            Some(
                self.dashboard_agent_kill_request(super::agent_control::AgentKillParams {
                    session_id,
                    grace_ms: 3_000,
                    interrupt_first: true,
                })
                .await?,
            )
        } else {
            None
        };
        let cancel = self.task_cancel_impl(params)?;
        Ok(DashboardTaskCancelResponse {
            ok: true,
            cancel,
            interrupt,
        })
    }

    pub(crate) async fn dashboard_task_dispatch_once(
        &self,
        params: TaskDispatchOnceParams,
    ) -> Result<TaskDispatchOnceResponse, ErrorData> {
        let dispatch_activity =
            super::m4_tools::AgentSpawnInFlightGuard::enter("dashboard_task_dispatch_outer")?;
        let db = self.agent_task_db()?;
        Self::prune_terminal_tasks(&db, unix_time_ms_now())?;
        self.reconcile_tasks(&db)?;
        let wait_timeout_ms = dashboard_task_dispatch_wait_timeout_ms(params.wait_timeout_ms);
        let (task, reservation_id, in_flight) = match Self::reserve_next_dispatch(
            &db,
            params.concurrency_cap,
            wait_timeout_ms,
            "dashboard_task_dispatch_reserve",
        )? {
            DispatchReservationOutcome::Empty { in_flight } => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: "empty".to_owned(),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchReservationOutcome::AtCapacity { in_flight } => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: format!("at_capacity:{in_flight}"),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchReservationOutcome::Reserved {
                task,
                reservation_id,
                in_flight_before,
            } => (task, reservation_id, in_flight_before),
        };
        let task_id = task.task_id.clone();
        let request = ActSpawnAgentRequest {
            template_id: Some(task.template_id.clone()),
            template_version: None,
            template_params: task.template_params.clone(),
            cli: None,
            kind: None,
            model: None,
            model_ref: None,
            prompt: None,
            target: None,
            working_dir: None,
            mcp_url: params.mcp_url,
            wait_timeout_ms,
            hold_open_ms: default_agent_spawn_hold_open_ms(),
            require_approval_gate: crate::m4::default_require_approval_gate(),
        };

        tracing::info!(
            code = "AGENT_TASK_DASHBOARD_DISPATCH_SPAWN",
            task_id = %task_id,
            template_id = %task.template_id,
            "readback=agent_tasks edge=dashboard_dispatch_spawn_begin"
        );

        let spawn_result = match dispatch_activity.ensure("dashboard_task_dispatch_before_spawn") {
            Ok(()) => self.dashboard_spawn_agent_request(request).await,
            Err(error) => Err(error),
        };
        let response = match spawn_result {
            Ok(response) => response,
            Err(spawn_error) => {
                let error_code = error_code_str(&spawn_error);
                let reason = format!(
                    "dashboard dispatch spawn failed [{error_code}]: {}",
                    spawn_error.message
                );
                if let Err(settle_error) = Self::fail_dispatch_reservation(
                    &db,
                    &task_id,
                    &reservation_id,
                    reason.clone(),
                    "dashboard_task_dispatch_spawn_failed",
                ) {
                    return Err(mcp_error(
                        error_codes::STORAGE_WRITE_FAILED,
                        format!(
                            "AGENT_TASK_DASHBOARD_DISPATCH_FAILURE_SETTLE_FAILED: \
                             task={task_id:?} reservation={reservation_id} spawn_error={}; \
                             settle_error={}; durable reservation remains authoritative and \
                             requires reconciliation",
                            spawn_error.message, settle_error.message
                        ),
                    ));
                }
                tracing::error!(
                    code = "AGENT_TASK_DASHBOARD_DISPATCH_SPAWN_FAILED",
                    task_id = %task_id,
                    template_id = %task.template_id,
                    error_code = %error_code,
                    "readback=agent_tasks edge=dashboard_dispatch_spawn_failed reason={reason}"
                );
                return Err(spawn_error);
            }
        };
        if let Err(error) = dispatch_activity.ensure("dashboard_task_dispatch_after_spawn") {
            let cleanup = self
                .cleanup_spawn_response_after_operator_panic(
                    &response,
                    "dashboard_task_dispatch_after_spawn",
                )
                .await;
            let reason = format!(
                "dashboard dispatch spawn superseded by operator panic [{}]: {}; cleanup={cleanup}",
                error_code_str(&error),
                error.message
            );
            Self::fail_dispatch_reservation(
                &db,
                &task_id,
                &reservation_id,
                reason,
                "dashboard_task_dispatch_operator_panic",
            )?;
            return Err(error);
        }

        let claimed = match Self::bind_dispatch_reservation(
            &db,
            &task_id,
            &reservation_id,
            &response.session_id,
            response.spawn_id.clone(),
            response.template_version,
            "dashboard_task_dispatch_bind",
        ) {
            Ok((task, _readback)) => task,
            Err(claim_error) => {
                let cleanup = self
                    .cleanup_spawn_response_after_operator_panic(
                        &response,
                        "dashboard_task_dispatch_bind_failed",
                    )
                    .await;
                tracing::error!(
                    code = "AGENT_TASK_DASHBOARD_DISPATCH_BIND_FAILED",
                    task_id = %task_id,
                    spawn_id = %response.spawn_id,
                    session_id = %response.session_id,
                    cleanup = %cleanup,
                    "guarded dashboard dispatch bind failed; spawned agent cleanup was attempted"
                );
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "dashboard task dispatch spawned agent session {:?} (spawn {:?}) for task \
                         {task_id:?} but could not bind durable reservation {reservation_id}; \
                         cleanup={cleanup}. The task reservation remains authoritative for \
                         reconcile. Underlying error: {}",
                        response.session_id, response.spawn_id, claim_error.message
                    ),
                ));
            }
        };

        tracing::info!(
            code = "AGENT_TASK_DASHBOARD_DISPATCH",
            task_id = %task_id,
            spawn_id = %response.spawn_id,
            session_id = %response.session_id,
            template_id = %task.template_id,
            template_version = response.template_version.unwrap_or(0),
            "readback=agent_tasks edge=dashboard_dispatch"
        );

        Ok(TaskDispatchOnceResponse {
            ok: true,
            decision: "dispatched".to_owned(),
            task: Some(claimed),
            spawn: Some(DispatchSpawnReadback {
                spawn_id: response.spawn_id,
                session_id: response.session_id,
                template_id: response.template_id,
                template_version: response.template_version,
                agent_process_id: response.agent_process_id,
                launched_at_unix_ms: response.launched_at_unix_ms,
                task_started_at_unix_ms: response.task_started_at_unix_ms,
            }),
            in_flight: in_flight + 1,
            concurrency_cap: params.concurrency_cap,
        })
    }
}

#[tool_router(router = agent_task_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Create a durable agent task (todo) on the fleet queue: title/description/acceptance, priority 1-5 (1=highest), and the template_id (+ template_params) a dispatcher spawns its agent from. Strict global FIFO enqueue order is assigned."
    )]
    pub async fn task_create(
        &self,
        params: Parameters<TaskCreateParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskMutationResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_create",
            task_id = %params.0.task_id,
            "tool.invocation kind=task_create"
        );
        self.task_create_impl(params.0).map(Json)
    }

    #[tool(
        description = "Read one agent task by id, including its full attempt history. Errors AGENT_TASK_NOT_FOUND if absent."
    )]
    pub async fn task_get(
        &self,
        params: Parameters<TaskIdParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskGetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_get",
            task_id = %params.0.task_id,
            "tool.invocation kind=task_get"
        );
        self.task_get_impl(params.0).map(Json)
    }

    #[tool(
        description = "Update an agent task: move it to a new state (validated against the todo->in_progress->review->done state machine; invalid transitions error AGENT_TASK_INVALID_TRANSITION), and/or edit priority/title/description/acceptance. Settles the live attempt when leaving in_progress."
    )]
    pub async fn task_update(
        &self,
        params: Parameters<TaskUpdateParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskMutationResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_update",
            task_id = %params.0.task_id,
            "tool.invocation kind=task_update"
        );
        self.task_update_impl(params.0).map(Json)
    }

    #[tool(
        description = "Claim a todo task for an agent session: transitions it to in_progress and appends a Pending attempt bound to session_id (so reconcile can detect if that agent vanishes). Errors AGENT_TASK_INVALID_TRANSITION if the task is not todo (duplicate-claim protection)."
    )]
    pub async fn task_claim(
        &self,
        params: Parameters<TaskClaimParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskMutationResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_claim",
            task_id = %params.0.task_id,
            "tool.invocation kind=task_claim"
        );
        self.task_claim_impl(params.0).map(Json)
    }

    #[tool(
        description = "Cancel an agent task (move it to the terminal cancelled state), settling any live attempt as failed. Re-cancelling an already-cancelled task is an idempotent read-only no-op (no generation bump, no updated_unix_ms change). Errors AGENT_TASK_INVALID_TRANSITION for any other terminal state (e.g. done)."
    )]
    pub async fn task_cancel(
        &self,
        params: Parameters<TaskCancelParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskMutationResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_cancel",
            task_id = %params.0.task_id,
            "tool.invocation kind=task_cancel"
        );
        self.task_cancel_impl(params.0).map(Json)
    }

    #[tool(
        description = "List agent tasks (optionally filtered by state), todo tasks in dispatch order (priority, per-template fairness, FIFO). Lazily reconciles orphaned in_progress tasks first; returns which were flagged."
    )]
    pub async fn task_list(
        &self,
        params: Parameters<TaskListParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_list",
            "tool.invocation kind=task_list"
        );
        self.task_list_impl(params.0).map(Json)
    }

    #[tool(
        description = "Preview the dispatcher's next pick without spawning: applies strict priority then per-template fairness (least in-flight) then FIFO, honoring the concurrency_cap. Returns the selected task or why none (empty / at_capacity). Reconciles orphans first."
    )]
    pub async fn task_next(
        &self,
        params: Parameters<TaskNextParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskNextResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_next",
            "tool.invocation kind=task_next"
        );
        self.task_next_impl(params.0).map(Json)
    }

    #[tool(
        description = "Reconcile the queue against live sessions: completed spawned attempts are settled from completion-status.json into review, while in_progress attempts whose session is gone without terminal evidence are flagged orphaned and moved to review (never silently re-queued). Crash-safe recovery; also runs lazily on task_list/task_next/task_dispatch_once."
    )]
    pub async fn task_reconcile(
        &self,
        _params: Parameters<EmptyParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskReconcileResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_reconcile",
            "tool.invocation kind=task_reconcile"
        );
        self.task_reconcile_impl().map(Json)
    }

    #[tool(
        description = "Atomically dispatch the next eligible task: reconcile orphans, apply the priority/fairness/FIFO selector under concurrency_cap, spawn a real agent from the task's template, and bind the task attempt to the spawned session_id + spawn_id + template_version. Returns empty / at_capacity:N with no spawn when nothing is eligible. A spawn failure leaves the task todo with a recorded failed attempt and returns the structured error."
    )]
    pub async fn task_dispatch_once(
        &self,
        params: Parameters<TaskDispatchOnceParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TaskDispatchOnceResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "task_dispatch_once",
            concurrency_cap = params.0.concurrency_cap,
            "tool.invocation kind=task_dispatch_once"
        );
        self.task_dispatch_once_impl(params.0, &request_context)
            .await
            .map(Json)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}
