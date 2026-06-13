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
//! live attempt against the session registry; an attempt whose session is gone
//! is flagged `orphaned` and the task is moved to `review` — never silently
//! re-queued.

use std::collections::{BTreeMap, BTreeSet};

use rmcp::{RoleServer, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_storage::{Db, cf};

use super::{
    ErrorData, Json, Parameters, SynapseService, mcp_error,
    session_registry::unix_time_ms_now, tool, tool_router,
};

/// CF_KV key namespace for task rows. Versioned prefix so a format change is a
/// clean re-key, never an in-place migration.
const TASK_NAMESPACE: &str = "agent-task/v1";
const TASK_SCHEMA_VERSION: u32 = 1;

const MAX_TASK_ID_CHARS: usize = 200;
const MAX_TITLE_CHARS: usize = 500;
const MAX_TEXT_CHARS: usize = 16 * 1024;
const MAX_PARAM_VALUE_BYTES: usize = 16 * 1024;
const MIN_PRIORITY: u8 = 1;
const MAX_PRIORITY: u8 = 5;
const MAX_LIST_TASKS: usize = 1000;
/// Default global cap on concurrently in-flight (`in_progress`) tasks the
/// dispatcher will allow. Operators override per call.
const DEFAULT_CONCURRENCY_CAP: usize = 8;

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

fn default_cap() -> usize {
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

// ---- key encoding & validation -------------------------------------------

fn task_key(task_id: &str) -> String {
    format!("{TASK_NAMESPACE}/task/{task_id}")
}

fn task_prefix() -> String {
    format!("{TASK_NAMESPACE}/task/")
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

/// Orders tasks for the queue view: todo tasks in dispatch order, then the rest
/// by enqueue sequence.
fn order_for_list(mut tasks: Vec<AgentTask>) -> Vec<AgentTask> {
    tasks.sort_by(|a, b| {
        // todo first, ordered by (priority asc, enqueue_seq asc); others after,
        // by enqueue_seq.
        let a_todo = a.state == TaskState::Todo;
        let b_todo = b.state == TaskState::Todo;
        b_todo
            .cmp(&a_todo)
            .then_with(|| {
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
        let rows = db.scan_cf_prefix(cf::CF_KV, key.as_bytes()).map_err(|error| {
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

    fn read_all_tasks(db: &Db) -> Result<Vec<AgentTask>, ErrorData> {
        let prefix = task_prefix();
        let rows = db
            .scan_cf_prefix(cf::CF_KV, prefix.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("agent_task failed to scan tasks: {error}"),
                )
            })?;
        rows.into_iter()
            .map(|(raw_key, raw_value)| {
                let key = String::from_utf8_lossy(&raw_key).into_owned();
                decode_task(&key, &raw_value)
            })
            .collect()
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

    /// Flags every `in_progress` task whose live attempt's session is no longer
    /// live as `orphaned`, moving it to `review`. Returns the flagged task ids.
    /// Never silently re-queues — a human (or operator tool) decides what next.
    fn reconcile_tasks(&self, db: &Db) -> Result<(usize, Vec<String>), ErrorData> {
        let now = unix_time_ms_now();
        let live = self.live_session_ids(now)?;
        let tasks = Self::read_all_tasks(db)?;
        let mut scanned = 0usize;
        let mut flagged = Vec::new();
        for mut task in tasks {
            if task.state != TaskState::InProgress {
                continue;
            }
            scanned += 1;
            let orphaned = match task.live_attempt() {
                // An in_progress task with no live attempt is itself orphaned.
                None => true,
                Some(attempt) => !live.contains(&attempt.session_id),
            };
            if !orphaned {
                continue;
            }
            let reason = format!(
                "orphaned: in_progress attempt session no longer live (reconciled at {now} ms)"
            );
            for attempt in &mut task.attempts {
                if attempt.outcome == AttemptOutcome::Pending {
                    attempt.outcome = AttemptOutcome::Orphaned;
                    attempt.ended_unix_ms = Some(now);
                    attempt.reason = Some(reason.clone());
                }
            }
            task.state = TaskState::Review;
            task.review_reason = Some(reason);
            task.updated_unix_ms = now;
            Self::write_task(db, &task)?;
            flagged.push(task.task_id.clone());
        }
        Ok((scanned, flagged))
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

    fn task_create_impl(&self, params: TaskCreateParams) -> Result<TaskMutationResponse, ErrorData> {
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
        if Self::read_task(&db, &params.task_id)?.is_some() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "agent_task {:?} already exists; use task_update to modify it",
                    params.task_id
                ),
            ));
        }
        // Strict global FIFO: enqueue_seq is max(existing) + 1, stable across
        // restarts (low task volume makes the scan negligible).
        let next_seq = Self::read_all_tasks(&db)?
            .iter()
            .map(|task| task.enqueue_seq)
            .max()
            .map_or(1, |max| max + 1);
        let now = unix_time_ms_now();
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

    fn task_update_impl(&self, params: TaskUpdateParams) -> Result<TaskMutationResponse, ErrorData> {
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
            return Err(params_error("agent_task claim session_id must not be empty"));
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

    fn task_cancel_impl(&self, params: TaskCancelParams) -> Result<TaskMutationResponse, ErrorData> {
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

    #[tool(description = "Read one agent task by id, including its full attempt history. Errors AGENT_TASK_NOT_FOUND if absent.")]
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

    #[tool(description = "Cancel an agent task (move it to the terminal cancelled state), settling any live attempt as failed. Errors AGENT_TASK_INVALID_TRANSITION if already terminal.")]
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
        description = "Reconcile the queue against live sessions: every in_progress task whose attempt session is no longer live is flagged orphaned and moved to review (never silently re-queued). Crash-safe recovery; also runs lazily on task_list/task_next."
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
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

#[cfg(test)]
mod tests {
    use super::*;

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
}
