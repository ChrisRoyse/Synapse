//! Durable agent task queue (#910).
//!
//! A crash-safe work queue agents are dispatched from — the board the fleet
//! kanban renders. Tasks live in `CF_KV` (same durable handle as templates #909
//! and the mailbox #908); each `task_*` mutation flushes so a row is on disk and
//! visible to the RocksDB read path before the tool returns (config/operational
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
};

use rmcp::{RoleServer, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use synapse_core::error_codes;
use synapse_storage::{Db, cf};

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
    pub attempts: Vec<TaskAttempt>,
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

// ---- dispatch selection (pure, unit-tested) -------------------------------

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
    serde_json::from_slice(bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("agent_task row {row_key} is corrupt and could not be decoded: {error}"),
        )
    })
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
    task: AgentTask,
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
        let key = task_key(task_id);
        let rows = db
            .scan_cf_prefix(cf::CF_KV, key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_task failed to read {key}: {error}"),
                )
            })?;
        for (raw_key, raw_value) in rows {
            if raw_key == key.as_bytes() {
                return Ok(Some(decode_task(&key, &raw_value)?));
            }
        }
        Ok(None)
    }

    pub(crate) fn read_all_tasks(db: &Db) -> Result<Vec<AgentTask>, ErrorData> {
        Self::scan_task_rows(db).map(|rows| rows.into_iter().map(|row| row.task).collect())
    }

    fn scan_task_rows(db: &Db) -> Result<Vec<AgentTaskRow>, ErrorData> {
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
                let key = String::from_utf8_lossy(&raw_key).into_owned();
                out.push(AgentTaskRow {
                    key: raw_key.clone(),
                    task: decode_task(&key, &raw_value)?,
                });
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

    fn read_enqueue_seq_watermark(db: &Db) -> Result<Option<u64>, ErrorData> {
        let rows = db
            .scan_cf_prefix(cf::CF_KV, TASK_SEQUENCE_KEY.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_task failed to read sequence watermark: {error}"),
                )
            })?;
        for (key, value) in rows {
            if key == TASK_SEQUENCE_KEY.as_bytes() {
                let text = std::str::from_utf8(&value).map_err(|error| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!("agent_task sequence watermark is not UTF-8: {error}"),
                    )
                })?;
                let seq = text.parse::<u64>().map_err(|error| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!("agent_task sequence watermark is not a u64: {error}"),
                    )
                })?;
                return Ok(Some(seq));
            }
        }
        Ok(None)
    }

    fn write_enqueue_seq_watermark(db: &Db, seq: u64) -> Result<(), ErrorData> {
        db.put_batch(
            cf::CF_KV,
            [(
                TASK_SEQUENCE_KEY.as_bytes().to_vec(),
                seq.to_string().into_bytes(),
            )],
        )
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("agent_task failed to persist sequence watermark: {error}"),
            )
        })?;
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!("agent_task sequence watermark persisted but failed to flush: {error}"),
            )
        })
    }

    fn ensure_enqueue_seq_watermark(db: &Db) -> Result<u64, ErrorData> {
        if let Some(seq) = Self::read_enqueue_seq_watermark(db)? {
            return Ok(seq);
        }
        let max_seq = Self::scan_task_rows(db)?
            .iter()
            .map(|row| row.task.enqueue_seq)
            .max()
            .unwrap_or(0);
        Self::write_enqueue_seq_watermark(db, max_seq)?;
        Ok(max_seq)
    }

    fn next_enqueue_seq(db: &Db) -> Result<u64, ErrorData> {
        let current = Self::ensure_enqueue_seq_watermark(db)?;
        let next = current.checked_add(1).ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "agent_task enqueue sequence overflowed u64",
            )
        })?;
        Self::write_enqueue_seq_watermark(db, next)?;
        Ok(next)
    }

    fn delete_task_keys(db: &Db, mut keys: Vec<Vec<u8>>) -> Result<usize, ErrorData> {
        keys.sort();
        keys.dedup();
        let deleted = keys.len();
        for chunk in keys.chunks(DELETE_BATCH_ROWS) {
            db.delete_batch(cf::CF_KV, chunk.iter().cloned())
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("agent_task failed to prune terminal rows: {error}"),
                    )
                })?;
        }
        Ok(deleted)
    }

    fn prune_terminal_task_rows(
        db: &Db,
        now: u64,
        rows: &[AgentTaskRow],
    ) -> Result<usize, ErrorData> {
        let mut terminal = rows
            .iter()
            .filter(|row| row.task.state.is_terminal())
            .map(|row| (row.task.updated_unix_ms, row.key.clone()))
            .collect::<Vec<_>>();
        terminal.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut delete = terminal
            .iter()
            .filter(|(updated_at, _key)| {
                updated_at.saturating_add(TERMINAL_TASK_RETENTION_MS) <= now
            })
            .map(|(_updated_at, key)| key.clone())
            .collect::<Vec<_>>();

        if terminal.len() > TERMINAL_TASK_RETAIN_ROWS {
            let over_cap = terminal.len() - TERMINAL_TASK_RETAIN_ROWS;
            delete.extend(
                terminal
                    .iter()
                    .take(over_cap)
                    .map(|(_updated_at, key)| key.clone()),
            );
        }

        let deleted = Self::delete_task_keys(db, delete)?;
        if deleted > 0 {
            tracing::info!(
                code = "AGENT_TASK_RETENTION_PRUNED",
                scanned_rows = rows.len(),
                terminal_rows = terminal.len(),
                deleted_rows = deleted,
                retain_terminal_rows = TERMINAL_TASK_RETAIN_ROWS,
                retention_ms = TERMINAL_TASK_RETENTION_MS,
                "readback=CF_KV terminal agent task rows pruned"
            );
        }
        Ok(deleted)
    }

    fn prune_terminal_tasks(db: &Db, now: u64) -> Result<usize, ErrorData> {
        Self::ensure_enqueue_seq_watermark(db)?;
        let rows = Self::scan_task_rows(db)?;
        Self::prune_terminal_task_rows(db, now, &rows)
    }

    /// Persists a task row and flushes so it is durable + immediately visible on
    /// the read path before the caller returns.
    fn write_task(db: &Db, task: &AgentTask) -> Result<TaskRowReadback, ErrorData> {
        let key = task_key(&task.task_id);
        let encoded = encode_task(task)?;
        db.put_batch(cf::CF_KV, [(key.clone().into_bytes(), encoded.clone())])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_task failed to persist {key}: {error}"),
                )
            })?;
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!("agent_task persisted {key} but failed to flush to disk: {error}"),
            )
        })?;
        Ok(TaskRowReadback {
            cf_name: cf::CF_KV.to_owned(),
            row_key: key,
            value_len_bytes: encoded.len() as u64,
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
        let tasks = Self::read_all_tasks(db)?;
        let mut scanned = 0usize;
        let mut flagged = Vec::new();
        for mut task in tasks {
            if task.state != TaskState::InProgress {
                continue;
            }
            scanned += 1;
            let missing_live_session = match task.live_attempt() {
                // An in_progress task with no live attempt is itself orphaned.
                None => true,
                Some(attempt) => !live.contains(&attempt.session_id),
            };
            if !missing_live_session {
                continue;
            }

            let terminal_completion = task
                .live_attempt()
                .and_then(|attempt| attempt.spawn_id.as_deref())
                .and_then(|spawn_id| read_spawn_terminal_completion(spawn_log_root, spawn_id));
            let (outcome, attempt_reason, review_reason, flagged_orphan) = match terminal_completion
            {
                Some(completion) if completion.is_success() => {
                    (AttemptOutcome::Succeeded, completion.reason(), None, false)
                }
                Some(completion) => {
                    let reason = completion.reason();
                    (AttemptOutcome::Failed, reason.clone(), Some(reason), false)
                }
                None => {
                    let reason = format!(
                        "orphaned: in_progress attempt session no longer live and no terminal completion artifact was available (reconciled at {now} ms)"
                    );
                    (AttemptOutcome::Orphaned, reason.clone(), Some(reason), true)
                }
            };
            for attempt in &mut task.attempts {
                if attempt.outcome == AttemptOutcome::Pending {
                    attempt.outcome = outcome;
                    attempt.ended_unix_ms = Some(now);
                    attempt.reason = Some(attempt_reason.clone());
                }
            }
            task.state = TaskState::Review;
            task.review_reason = review_reason;
            task.updated_unix_ms = now;
            Self::write_task(db, &task)?;
            if flagged_orphan {
                flagged.push(task.task_id.clone());
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
    ) -> Result<AgentTask, ErrorData> {
        let mut task = Self::read_task(db, task_id)?.ok_or_else(|| task_not_found(task_id))?;
        if task.state != TaskState::Todo {
            return Err(mcp_error(
                error_codes::AGENT_TASK_INVALID_TRANSITION,
                format!(
                    "agent_task {task_id:?} cannot be claimed: it is {}, not todo (already claimed or finished)",
                    task.state.as_str()
                ),
            ));
        }
        let now = unix_time_ms_now();
        let attempt_id = u32::try_from(task.attempts.len()).unwrap_or(u32::MAX) + 1;
        task.attempts.push(TaskAttempt {
            attempt_id,
            session_id: session_id.to_owned(),
            spawn_id,
            template_version,
            outcome: AttemptOutcome::Pending,
            started_unix_ms: now,
            ended_unix_ms: None,
            reason: None,
        });
        task.state = TaskState::InProgress;
        task.updated_unix_ms = now;
        Self::write_task(db, &task)?;
        Ok(task)
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
        if Self::read_task(&db, &params.task_id)?.is_some() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "agent_task {:?} already exists; use task_update to modify it",
                    params.task_id
                ),
            ));
        }
        // Strict global FIFO survives terminal-row pruning via a durable
        // namespace watermark instead of deriving from retained rows.
        let next_seq = Self::next_enqueue_seq(&db)?;
        let task = AgentTask {
            schema_version: TASK_SCHEMA_VERSION,
            task_id: params.task_id,
            state: TaskState::Todo,
            title: params.title,
            description: params.description,
            acceptance: params.acceptance,
            priority: params.priority,
            template_id: params.template_id,
            template_params: params.template_params,
            enqueue_seq: next_seq,
            attempts: Vec::new(),
            review_reason: None,
            created_unix_ms: now,
            updated_unix_ms: now,
        };
        let written_row = Self::write_task(&db, &task)?;
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
        let db = self.agent_task_db()?;
        let mut task = Self::read_task(&db, &params.task_id)?
            .ok_or_else(|| task_not_found(&params.task_id))?;
        let now = unix_time_ms_now();

        if let Some(priority) = params.priority {
            validate_priority(priority)?;
            task.priority = priority;
        }
        if let Some(title) = params.title {
            if title.trim().is_empty() {
                return Err(params_error("agent_task title must not be empty"));
            }
            validate_text("title", &title, MAX_TITLE_CHARS)?;
            task.title = title;
        }
        if let Some(description) = params.description {
            validate_text("description", &description, MAX_TEXT_CHARS)?;
            task.description = Some(description);
        }
        if let Some(acceptance) = params.acceptance {
            validate_text("acceptance", &acceptance, MAX_TEXT_CHARS)?;
            task.acceptance = Some(acceptance);
        }

        if let Some(target) = params.state {
            if target != task.state {
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
                // Settle the live attempt when leaving in_progress.
                let outcome = match target {
                    TaskState::Review | TaskState::Done => Some(AttemptOutcome::Succeeded),
                    TaskState::Cancelled => Some(AttemptOutcome::Failed),
                    _ => None,
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
        }
        task.updated_unix_ms = now;
        let written_row = Self::write_task(&db, &task)?;
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
        let task = Self::claim_internal(&db, &params.task_id, &params.session_id, None, None)?;
        let written_row = TaskRowReadback {
            cf_name: cf::CF_KV.to_owned(),
            row_key: task_key(&task.task_id),
            value_len_bytes: encode_task(&task)?.len() as u64,
        };
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

    fn record_failed_attempt_internal(
        db: &Db,
        task_id: &str,
        reason: String,
    ) -> Result<AgentTask, ErrorData> {
        let mut task = Self::read_task(db, task_id)?.ok_or_else(|| task_not_found(task_id))?;
        if task.state != TaskState::Todo {
            return Err(mcp_error(
                error_codes::AGENT_TASK_INVALID_TRANSITION,
                format!(
                    "agent_task {task_id:?} cannot record a dispatch-failure attempt: it is {}, not todo (raced by another dispatcher?)",
                    task.state.as_str()
                ),
            ));
        }
        let now = unix_time_ms_now();
        let attempt_id = u32::try_from(task.attempts.len()).unwrap_or(u32::MAX) + 1;
        task.attempts.push(TaskAttempt {
            attempt_id,
            session_id: String::new(),
            spawn_id: None,
            template_version: None,
            outcome: AttemptOutcome::Failed,
            started_unix_ms: now,
            ended_unix_ms: Some(now),
            reason: Some(reason),
        });
        task.updated_unix_ms = now;
        Self::write_task(db, &task)?;
        Ok(task)
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
        let tasks = Self::read_all_tasks(&db)?;
        let in_flight = tasks.iter().filter(|task| task.is_in_flight()).count();
        let task_id = match dispatch_decision(&tasks, params.concurrency_cap) {
            DispatchDecision::Empty => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: "empty".to_owned(),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchDecision::AtCapacity { in_flight } => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: format!("at_capacity:{in_flight}"),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchDecision::Dispatch { task_id } => task_id,
        };

        let task = Self::read_task(&db, &task_id)?.ok_or_else(|| task_not_found(&task_id))?;
        let wait_timeout_ms = dashboard_task_dispatch_wait_timeout_ms(params.wait_timeout_ms);
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
                Self::record_failed_attempt_internal(&db, &task_id, reason.clone())?;
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
            Self::record_failed_attempt_internal(&db, &task_id, reason)?;
            return Err(error);
        }

        let claimed = match Self::claim_internal(
            &db,
            &task_id,
            &response.session_id,
            Some(response.spawn_id.clone()),
            response.template_version,
        ) {
            Ok(task) => task,
            Err(claim_error) => {
                tracing::error!(
                    code = "AGENT_TASK_DISPATCH_ORPHAN",
                    task_id = %task_id,
                    spawn_id = %response.spawn_id,
                    session_id = %response.session_id,
                    "readback=agent_tasks edge=dispatch_claim_failed: live agent is unbound"
                );
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "task_dispatch_once spawned agent session {:?} (spawn {:?}) for task {task_id:?} but could not bind the attempt: {}. The agent is live and unbound; use agent_kill on the spawn. Underlying error: {}",
                        response.session_id,
                        response.spawn_id,
                        claim_error.message,
                        claim_error.message
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
        let tasks = Self::read_all_tasks(&db)?;
        let in_flight = tasks.iter().filter(|task| task.is_in_flight()).count();
        let task_id = match dispatch_decision(&tasks, params.concurrency_cap) {
            DispatchDecision::Empty => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: "empty".to_owned(),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchDecision::AtCapacity { in_flight } => {
                return Ok(TaskDispatchOnceResponse {
                    ok: true,
                    decision: format!("at_capacity:{in_flight}"),
                    task: None,
                    spawn: None,
                    in_flight,
                    concurrency_cap: params.concurrency_cap,
                });
            }
            DispatchDecision::Dispatch { task_id } => task_id,
        };

        let task = Self::read_task(&db, &task_id)?.ok_or_else(|| task_not_found(&task_id))?;
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
            wait_timeout_ms: params.wait_timeout_ms,
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
                Self::record_failed_attempt_internal(&db, &task_id, reason.clone())?;
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
            Self::record_failed_attempt_internal(&db, &task_id, reason)?;
            return Err(error);
        }

        let claimed = match Self::claim_internal(
            &db,
            &task_id,
            &response.session_id,
            Some(response.spawn_id.clone()),
            response.template_version,
        ) {
            Ok(task) => task,
            Err(claim_error) => {
                tracing::error!(
                    code = "AGENT_TASK_DASHBOARD_DISPATCH_ORPHAN",
                    task_id = %task_id,
                    spawn_id = %response.spawn_id,
                    session_id = %response.session_id,
                    "readback=agent_tasks edge=dashboard_dispatch_claim_failed: live agent is unbound"
                );
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "dashboard task dispatch spawned agent session {:?} (spawn {:?}) for task {task_id:?} but could not bind the attempt: {}. The agent is live and unbound; use agent_kill on the spawn. Underlying error: {}",
                        response.session_id,
                        response.spawn_id,
                        claim_error.message,
                        claim_error.message
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
        description = "Cancel an agent task (move it to the terminal cancelled state), settling any live attempt as failed. Errors AGENT_TASK_INVALID_TRANSITION if already terminal."
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

#[cfg(test)]
impl SynapseService {
    /// Test-only: create a task through the production `task_create_impl` path
    /// (real CF_KV row + flush). Lets cross-module tests (e.g. #951 cost
    /// rollups) seed real tasks without `task_create_impl` being public.
    pub(crate) fn task_create_for_test(
        &self,
        task_id: &str,
        template_id: &str,
    ) -> Result<(), ErrorData> {
        self.task_create_impl(TaskCreateParams {
            task_id: task_id.to_owned(),
            title: task_id.to_owned(),
            description: None,
            acceptance: None,
            priority: 3,
            template_id: template_id.to_owned(),
            template_params: BTreeMap::new(),
        })
        .map(|_response| ())
    }

    /// Test-only: claim a task while binding a `spawn_id` + `template_version`
    /// to the attempt — exactly what the #957 auto-dispatcher does via
    /// `claim_internal`. Lets cost-rollup tests (#951) build real
    /// `spawn -> task -> template` rows through the production claim path
    /// instead of hand-writing CF_KV rows, without the live-spawn harness.
    pub(crate) fn task_claim_with_spawn_for_test(
        &self,
        task_id: &str,
        session_id: &str,
        spawn_id: &str,
        template_version: u32,
    ) -> Result<AgentTask, ErrorData> {
        let db = self.agent_task_db()?;
        Self::claim_internal(
            &db,
            task_id,
            session_id,
            Some(spawn_id.to_owned()),
            Some(template_version),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use serde_json::json;
    use synapse_core::SCHEMA_VERSION;

    fn task(id: &str, state: TaskState, priority: u8, template: &str, seq: u64) -> AgentTask {
        AgentTask {
            schema_version: TASK_SCHEMA_VERSION,
            task_id: id.to_owned(),
            state,
            title: id.to_owned(),
            description: None,
            acceptance: None,
            priority,
            template_id: template.to_owned(),
            template_params: BTreeMap::new(),
            enqueue_seq: seq,
            attempts: Vec::new(),
            review_reason: None,
            created_unix_ms: 1_000,
            updated_unix_ms: 1_000,
        }
    }

    fn task_result<T>(result: std::result::Result<T, ErrorData>) -> Result<T> {
        result.map_err(|error| anyhow::anyhow!("{}", error.message))
    }

    fn task_with_pending_spawn(task_id: &str, session_id: &str, spawn_id: &str) -> AgentTask {
        let mut task = task(task_id, TaskState::InProgress, 1, "reviewer", 1);
        task.attempts.push(TaskAttempt {
            attempt_id: 1,
            session_id: session_id.to_owned(),
            spawn_id: Some(spawn_id.to_owned()),
            template_version: Some(7),
            outcome: AttemptOutcome::Pending,
            started_unix_ms: 1_500,
            ended_unix_ms: None,
            reason: None,
        });
        task
    }

    fn write_completion_artifact(
        root: &Path,
        spawn_id: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<()> {
        let dir = root.join(spawn_id);
        std::fs::create_dir_all(&dir)?;
        let value = json!({
            "schema_version": 1,
            "spawn_id": spawn_id,
            "status": status,
            "error_message": error_message,
        });
        std::fs::write(
            dir.join("completion-status.json"),
            serde_json::to_vec(&value)?,
        )?;
        Ok(())
    }

    #[test]
    fn terminal_task_retention_preserves_enqueue_watermark() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Db::open(&temp.path().join("db"), SCHEMA_VERSION)?;
        let now = TERMINAL_TASK_RETENTION_MS + 10_000;
        let mut old_terminal = task("old-terminal", TaskState::Done, 3, "reviewer", 99);
        old_terminal.updated_unix_ms = 1;
        old_terminal.created_unix_ms = 1;
        let mut open_old = task("open-old", TaskState::Todo, 3, "reviewer", 5);
        open_old.updated_unix_ms = 1;
        open_old.created_unix_ms = 1;
        task_result(SynapseService::write_task(&db, &old_terminal))?;
        task_result(SynapseService::write_task(&db, &open_old))?;

        let deleted = task_result(SynapseService::prune_terminal_tasks(&db, now))?;

        assert_eq!(deleted, 1);
        assert!(task_result(SynapseService::read_task(&db, "old-terminal"))?.is_none());
        assert!(task_result(SynapseService::read_task(&db, "open-old"))?.is_some());
        assert_eq!(
            task_result(SynapseService::read_enqueue_seq_watermark(&db))?,
            Some(99)
        );
        assert_eq!(task_result(SynapseService::next_enqueue_seq(&db))?, 100);
        Ok(())
    }

    #[test]
    fn terminal_task_retention_caps_recent_rows() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Db::open(&temp.path().join("db"), SCHEMA_VERSION)?;
        let now = TERMINAL_TASK_RETENTION_MS + 100_000;
        let rows = (0..(TERMINAL_TASK_RETAIN_ROWS + 2))
            .map(|idx| {
                let mut row = task(
                    &format!("cap-{idx:05}"),
                    TaskState::Done,
                    3,
                    "reviewer",
                    u64::try_from(idx + 1).expect("idx fits u64"),
                );
                row.updated_unix_ms = now - 1_000 + u64::try_from(idx).expect("idx fits u64");
                row.created_unix_ms = row.updated_unix_ms;
                Ok((
                    task_key(&row.task_id).into_bytes(),
                    task_result(encode_task(&row))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        db.put_batch_pressure_bypass(cf::CF_KV, rows)?;

        let deleted = task_result(SynapseService::prune_terminal_tasks(&db, now))?;

        assert_eq!(deleted, 2);
        assert!(task_result(SynapseService::read_task(&db, "cap-00000"))?.is_none());
        assert!(task_result(SynapseService::read_task(&db, "cap-00001"))?.is_none());
        assert!(task_result(SynapseService::read_task(&db, "cap-00002"))?.is_some());
        assert!(
            task_result(SynapseService::read_all_tasks(&db))?.len() <= TERMINAL_TASK_RETAIN_ROWS
        );
        Ok(())
    }

    #[test]
    fn dispatch_prefers_highest_priority_tier() {
        let tasks = vec![
            task("low", TaskState::Todo, 5, "t1", 1),
            task("high", TaskState::Todo, 1, "t1", 2),
            task("mid", TaskState::Todo, 3, "t1", 3),
        ];
        assert_eq!(
            dispatch_decision(&tasks, 8),
            DispatchDecision::Dispatch {
                task_id: "high".to_owned()
            }
        );
    }

    #[test]
    fn dispatch_is_fifo_within_priority_and_template() {
        let tasks = vec![
            task("second", TaskState::Todo, 2, "t1", 10),
            task("first", TaskState::Todo, 2, "t1", 5),
        ];
        assert_eq!(
            dispatch_decision(&tasks, 8),
            DispatchDecision::Dispatch {
                task_id: "first".to_owned()
            }
        );
    }

    #[test]
    fn dispatch_fairness_avoids_starving_other_templates() {
        // t1 already has 2 in-flight; t2 has none. Same priority + t2 enqueued
        // later, but fairness must pick t2 so the greedy t1 cannot starve it.
        let tasks = vec![
            task("t1-a", TaskState::InProgress, 2, "t1", 1),
            task("t1-b", TaskState::InProgress, 2, "t1", 2),
            task("t1-c", TaskState::Todo, 2, "t1", 3),
            task("t2-a", TaskState::Todo, 2, "t2", 99),
        ];
        assert_eq!(
            dispatch_decision(&tasks, 8),
            DispatchDecision::Dispatch {
                task_id: "t2-a".to_owned()
            }
        );
    }

    #[test]
    fn dispatch_reports_at_capacity() {
        let tasks = vec![
            task("a", TaskState::InProgress, 1, "t1", 1),
            task("b", TaskState::InProgress, 1, "t1", 2),
            task("c", TaskState::Todo, 1, "t1", 3),
        ];
        assert_eq!(
            dispatch_decision(&tasks, 2),
            DispatchDecision::AtCapacity { in_flight: 2 }
        );
    }

    #[test]
    fn dashboard_dispatch_wait_timeout_floors_human_approval_budget() {
        assert_eq!(
            dashboard_task_dispatch_wait_timeout_ms(default_agent_spawn_wait_timeout_ms()),
            DASHBOARD_TASK_DISPATCH_WAIT_TIMEOUT_MS
        );
        assert_eq!(
            dashboard_task_dispatch_wait_timeout_ms(DASHBOARD_TASK_DISPATCH_WAIT_TIMEOUT_MS + 1),
            DASHBOARD_TASK_DISPATCH_WAIT_TIMEOUT_MS + 1
        );
    }

    #[test]
    fn dispatch_empty_when_no_todo() {
        let tasks = vec![task("done", TaskState::Done, 1, "t1", 1)];
        assert_eq!(dispatch_decision(&tasks, 8), DispatchDecision::Empty);
    }

    #[test]
    fn state_machine_allows_happy_path_and_rejects_skips() {
        assert!(TaskState::Todo.can_transition_to(TaskState::InProgress));
        assert!(TaskState::InProgress.can_transition_to(TaskState::Review));
        assert!(TaskState::Review.can_transition_to(TaskState::Done));
        // illegal skips / terminal exits
        assert!(!TaskState::Todo.can_transition_to(TaskState::Review));
        assert!(!TaskState::Todo.can_transition_to(TaskState::Done));
        assert!(!TaskState::Done.can_transition_to(TaskState::Todo));
        assert!(!TaskState::Cancelled.can_transition_to(TaskState::InProgress));
        assert!(TaskState::Done.is_terminal());
        assert!(TaskState::Cancelled.is_terminal());
    }

    #[test]
    fn allowed_targets_match_matrix() {
        assert_eq!(
            TaskState::Todo.allowed_targets(),
            vec!["in_progress", "cancelled"]
        );
        assert!(TaskState::Done.allowed_targets().is_empty());
    }

    #[test]
    fn reconcile_settles_terminal_ok_spawn_as_review_succeeded() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Db::open(&temp.path().join("db"), SCHEMA_VERSION)?;
        let spawn_root = temp.path().join("agent-spawns");
        let task =
            task_with_pending_spawn("terminal-ok", "closed-session", "agent-spawn-terminal-ok");
        task_result(SynapseService::write_task(&db, &task))?;
        write_completion_artifact(&spawn_root, "agent-spawn-terminal-ok", "ok", None)?;

        let (scanned, flagged) = task_result(SynapseService::reconcile_task_rows(
            &db,
            &BTreeSet::new(),
            2_000,
            Some(&spawn_root),
        ))?;

        assert_eq!(scanned, 1);
        assert!(flagged.is_empty());
        let read = task_result(SynapseService::read_task(&db, "terminal-ok"))?
            .context("terminal-ok task row")?;
        assert_eq!(read.state, TaskState::Review);
        assert_eq!(read.review_reason, None);
        assert_eq!(read.attempts[0].outcome, AttemptOutcome::Succeeded);
        assert_eq!(read.attempts[0].ended_unix_ms, Some(2_000));
        assert!(
            read.attempts[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("status=ok"))
        );
        Ok(())
    }

    #[test]
    fn reconcile_settles_terminal_non_ok_spawn_as_review_failed() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Db::open(&temp.path().join("db"), SCHEMA_VERSION)?;
        let spawn_root = temp.path().join("agent-spawns");
        let task = task_with_pending_spawn(
            "terminal-failed",
            "closed-session",
            "agent-spawn-terminal-failed",
        );
        task_result(SynapseService::write_task(&db, &task))?;
        write_completion_artifact(
            &spawn_root,
            "agent-spawn-terminal-failed",
            "failed",
            Some("synthetic exit 7"),
        )?;

        let (scanned, flagged) = task_result(SynapseService::reconcile_task_rows(
            &db,
            &BTreeSet::new(),
            2_000,
            Some(&spawn_root),
        ))?;

        assert_eq!(scanned, 1);
        assert!(flagged.is_empty());
        let read = task_result(SynapseService::read_task(&db, "terminal-failed"))?
            .context("terminal-failed task row")?;
        assert_eq!(read.state, TaskState::Review);
        assert_eq!(read.attempts[0].outcome, AttemptOutcome::Failed);
        assert!(read.review_reason.as_deref().is_some_and(
            |reason| reason.contains("status=failed") && reason.contains("synthetic exit 7")
        ));
        Ok(())
    }
}
