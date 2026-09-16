use super::{
    ActComboParams, ActComboResponse, ActLaunchParams, ActLaunchResponse,
    ActRunShellCancelResponse, ActRunShellJobIdParams, ActRunShellParams, ActRunShellResponse,
    ActRunShellStartParams, ActRunShellStartResponse, ActRunShellStatusParams,
    ActRunShellStatusResponse, ActSpawnAgentCli, ActSpawnAgentLogPaths, ActSpawnAgentParams,
    ActSpawnAgentRequest, ActSpawnAgentResponse, ActSpawnAgentTarget, AgentSpawnTaskStartedParams,
    AgentSpawnTaskStartedResponse, ErrorData, Json, LaunchWindowState,
    MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS, Parameters, RunShellAuthorization, SessionTarget,
    ShellExecutionContext, SynapseService, TargetWire, assign_owned_process_job,
    authorize_run_shell, authorize_run_shell_start, cancel_shell_job, execute_combo_with_boundary,
    launch_for_session_with_boundary, launch_process_history_row, launch_process_history_row_key,
    launch_request_details, mcp_error, prepare_run_shell_params_for_context,
    prepare_run_shell_start_params_for_context, required_combo_permissions,
    run_authorized_shell_with_boundary, run_shell_idempotency_completed_row,
    run_shell_idempotency_replay, run_shell_idempotency_reservation_row,
    run_shell_idempotency_row_key, run_shell_request_details, run_shell_start_request_details,
    shell_execution_context_for_session, shell_job_status,
    start_authorized_shell_job_with_boundary, tool, tool_router, validate_agent_spawn_params,
    validate_run_shell_execution_plan,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use serde::Serialize;
use serde_json::{Map, Value, json};
use synapse_core::{error_codes, new_reflex_id};
use synapse_storage::{cf, decode_json};

use crate::m3::local_models::{
    LocalModelApiShape, LocalModelProbeParams, LocalModelRegistryRow, ResolvedApiKey,
};
use crate::m4::ActRunShellExecutionMode;

use super::{
    m1_tools::validate_target_window,
    session_registry::{
        SessionRegistryRead, SpawnedAgentControlRead, SpawnedAgentRead, unix_time_ms_now,
    },
    terminal_capture::capture::{CaptureArtifacts, CaptureSpec, spawn_capture_to_asciicast},
};

const ACT_SPAWN_AGENT: &str = "act_spawn_agent";
/// Filename of the per-spawn manifest written into each spawn dir. The
/// transcript ingester (#900/#949) reads it to learn the spawn's model when the
/// CLI stream does not carry one (Codex).
pub(crate) const AGENT_SPAWN_MANIFEST_FILENAME: &str = "spawn-manifest.json";
/// Schema version stamped onto the spawn manifest.
pub(crate) const AGENT_SPAWN_MANIFEST_VERSION: u32 = 1;
const CODEX_APP_SERVER_RUNNER_SCRIPT: &str = include_str!("../codex_app_server_runner.ps1");
const SHELL_FACADE_SOURCE_OF_TRUTH: &str = "%LOCALAPPDATA%\\Synapse\\shell-jobs + %LOCALAPPDATA%\\Synapse\\shell-sessions + daemon-tool-events.jsonl";
const PROCESS_FACADE_SOURCE_OF_TRUTH: &str = "live OS process table + CF_PROCESS_HISTORY";
const PROCESS_LIST_DEFAULT_LIMIT: usize = 100;
const PROCESS_LIST_MAX_LIMIT: usize = 1000;
const PROCESS_HISTORY_DEFAULT_LIMIT: usize = 20;
const PROCESS_HISTORY_MAX_LIMIT: usize = 200;

mod agent_spawn;
mod facade;
#[cfg(test)]
mod tests;
mod types;

pub(crate) use self::agent_spawn::agent_spawn_root_dir;
pub(super) use self::agent_spawn::*;
#[allow(clippy::wildcard_imports)]
use self::facade::*;
pub(super) use self::types::shell_input_schema;
pub use self::types::*;
/// Builds the per-spawn manifest JSON. Records the CLI, resolved working
/// directory, and, when the operator pinned one, the model. This is the
/// authoritative run identity the transcript ingester and respawn path read.
fn build_spawn_manifest(
    spawn_id: &str,
    params: &ActSpawnAgentParams,
    working_dir: &Path,
) -> Result<Value, ErrorData> {
    let agent_kind = params.effective_cli()?;
    let effective_working_dir = working_dir.display().to_string();
    Ok(json!({
        "version": AGENT_SPAWN_MANIFEST_VERSION,
        "spawn_id": spawn_id,
        "cli": agent_kind.as_str(),
        "kind": agent_kind.as_str(),
        "model": params.model_for_spawn_manifest(agent_kind),
        "model_ref": params.local_model_ref(),
        "working_dir": effective_working_dir,
        "effective_working_dir": effective_working_dir,
        "requested_working_dir": params.working_dir.as_deref(),
        "require_approval_gate": params.require_approval_gate,
        "approval_gate_effective": params.require_approval_gate && agent_kind.uses_approval_gate(),
        "local_model_autonomous_tool_calls": agent_kind.is_local_model(),
        "local_model_approval_gate_used": false,
        "local_model_trusted_unattended_exact_contract": agent_kind.is_local_model(),
        "assigned_prompt_present": params
            .prompt
            .as_deref()
            .is_some_and(|prompt| !prompt.trim().is_empty()),
        // Spawn-template provenance (#909): the exact template version + config
        // hash this spawn was rendered from, or null for a direct spawn. The
        // manifest is the physical source of truth for run reproducibility.
        "template_id": params.template_id.as_deref(),
        "template_version": params.template_version,
        "template_config_hash": params.template_config_hash.as_deref(),
        "created_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
    }))
}
const AGENT_SPAWN_SHELL_ENV_VAR: &str = "SYNAPSE_AGENT_SPAWN_SHELL";
const AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT: usize = 80;
const AGENT_SPAWN_POLL_INTERVAL_MS: u64 = 250;
const AGENT_SPAWN_LOG_TAIL_BYTES: usize = 8 * 1024;
const AGENT_SPAWN_ORPHAN_RECOVERY_STALE_MS: u64 = 10 * 60 * 1000;
const LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS: u64 = 15 * 60 * 1000;
static AGENT_SPAWN_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static AGENT_SPAWN_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static AGENT_SPAWN_CLEANUP_INCIDENT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentSpawnActivityReadback {
    pub(crate) sequence: u64,
    pub(crate) in_flight: u64,
    pub(crate) operator_panic_epoch: u64,
    pub(crate) operator_panic_safety_pending: bool,
    pub(crate) cleanup_incident: bool,
}

pub(crate) fn agent_spawn_activity_readback() -> AgentSpawnActivityReadback {
    loop {
        let sequence_before = AGENT_SPAWN_SEQUENCE.load(Ordering::SeqCst);
        let in_flight = AGENT_SPAWN_IN_FLIGHT.load(Ordering::SeqCst);
        let sequence_after = AGENT_SPAWN_SEQUENCE.load(Ordering::SeqCst);
        if sequence_before == sequence_after {
            let operator_panic = synapse_action::operator_panic_safety_readback();
            return AgentSpawnActivityReadback {
                sequence: sequence_after,
                in_flight,
                operator_panic_epoch: operator_panic.epoch,
                operator_panic_safety_pending: operator_panic.pending,
                cleanup_incident: AGENT_SPAWN_CLEANUP_INCIDENT.load(Ordering::SeqCst),
            };
        }
    }
}

#[derive(Debug)]
pub(crate) struct AgentSpawnInFlightGuard {
    source: &'static str,
    spawn_id: Option<String>,
    cli: Option<ActSpawnAgentCli>,
    sequence: u64,
    in_flight_at_start: u64,
    operator_panic_epoch_at_entry: u64,
    started_at: Instant,
}

impl AgentSpawnInFlightGuard {
    pub(crate) fn enter(source: &'static str) -> Result<Self, ErrorData> {
        Self::enter_with_pre_guard_hook(source, || {})
    }

    fn enter_with_pre_guard_hook(
        source: &'static str,
        after_precheck: impl FnOnce(),
    ) -> Result<Self, ErrorData> {
        let operator_panic_epoch_at_entry = synapse_action::operator_panic_safety_readback().epoch;
        ensure_agent_spawn_operator_panic_not_observed(
            source,
            operator_panic_epoch_at_entry,
            "before_spawn_activity_guard",
        )?;
        after_precheck();
        // Publish in-flight first. A K2 snapshot can therefore never observe a
        // newly allocated sequence with zero owners if this thread is preempted
        // between the two atomics.
        let in_flight_at_start = AGENT_SPAWN_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
        let sequence = AGENT_SPAWN_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
        let guard = Self {
            source,
            spawn_id: None,
            cli: None,
            sequence,
            in_flight_at_start,
            operator_panic_epoch_at_entry,
            started_at: Instant::now(),
        };
        tracing::info!(
            code = "AGENT_SPAWN_IN_FLIGHT_ENTER",
            source,
            sequence,
            in_flight_at_start,
            operator_panic_epoch_at_entry,
            "act_spawn_agent entered provisioning"
        );
        guard.ensure("after_spawn_activity_guard")?;
        Ok(guard)
    }

    fn identify(&mut self, spawn_id: &str, cli: ActSpawnAgentCli) {
        self.spawn_id = Some(spawn_id.to_owned());
        self.cli = Some(cli);
    }

    pub(crate) fn ensure(&self, stage: &'static str) -> Result<(), ErrorData> {
        ensure_agent_spawn_operator_panic_not_observed(
            self.source,
            self.operator_panic_epoch_at_entry,
            stage,
        )
    }

    fn in_flight_now() -> u64 {
        agent_spawn_activity_readback().in_flight
    }
}

impl Drop for AgentSpawnInFlightGuard {
    fn drop(&mut self) {
        let before = AGENT_SPAWN_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        let in_flight_after = before.saturating_sub(1);
        tracing::info!(
            code = "AGENT_SPAWN_IN_FLIGHT_EXIT",
            source = self.source,
            spawn_id = ?self.spawn_id,
            cli = ?self.cli.map(ActSpawnAgentCli::as_str),
            sequence = self.sequence,
            in_flight_after,
            elapsed_ms = duration_ms_u64(self.started_at.elapsed()),
            "act_spawn_agent left provisioning"
        );
    }
}

fn ensure_agent_spawn_operator_panic_not_observed(
    source: &'static str,
    operator_panic_epoch_at_entry: u64,
    stage: &'static str,
) -> Result<(), ErrorData> {
    let operator_panic = synapse_action::operator_panic_safety_readback();
    if operator_panic.pending || operator_panic.epoch != operator_panic_epoch_at_entry {
        let activity = agent_spawn_activity_readback();
        return Err(agent_spawn_tool_error(
            error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
            "act_spawn_agent was superseded by the physical operator panic control",
            json!({
                "code": error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                "detail_code": "AGENT_SPAWN_OPERATOR_PANIC_PREARMED",
                "source": source,
                "stage": stage,
                "operator_panic_epoch_at_entry": operator_panic_epoch_at_entry,
                "activity": activity,
                "source_of_truth": "AGENT_SPAWN_SEQUENCE + AGENT_SPAWN_IN_FLIGHT + synapse_action operator-panic safety readback",
            }),
        ));
    }
    Ok(())
}

fn agent_spawn_operator_panic_cleanup_error(
    error: ErrorData,
    stage: &'static str,
    cleanup: Value,
) -> ErrorData {
    agent_spawn_tool_error(
        error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
        "act_spawn_agent was superseded by operator panic after process launch; exact cleanup was attempted",
        json!({
            "code": error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
            "detail_code": "AGENT_SPAWN_OPERATOR_PANIC_AFTER_LAUNCH",
            "stage": stage,
            "source_error_message": error.message,
            "source_error_data": error.data,
            "cleanup": cleanup,
            "activity": agent_spawn_activity_readback(),
        }),
    )
}

fn ensure_m4_physical_mutation_boundary(
    preflight: &super::action_preflight::ActionPreflightReadback,
    stage: &'static str,
) -> Result<(), ErrorData> {
    crate::server::operator_panic_boundary::ensure_mcp_mutation(stage)?;
    preflight.ensure_operator_panic_boundary(stage)
}

fn action_preflight_cleanup_error(
    error: ErrorData,
    stage: &'static str,
    cleanup: Value,
) -> ErrorData {
    let mut data = match error.data {
        Some(Value::Object(data)) => data,
        Some(original_data) => {
            let mut data = serde_json::Map::new();
            data.insert("original_data".to_owned(), original_data);
            data
        }
        None => serde_json::Map::new(),
    };
    data.insert("physical_mutation_boundary_stage".to_owned(), json!(stage));
    data.insert("physical_mutation_cleanup".to_owned(), cleanup);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(Value::Object(data)),
    )
}

fn shell_operator_panic_cancel_verified(response: &ActRunShellCancelResponse) -> bool {
    shell_operator_panic_cleanup_verified(
        &response.remaining_process_ids,
        response.status.running,
        response.remote_process_scope.remote_cleanup_required,
        response.remote_process_scope.remote_cleanup_verified,
    )
}

fn shell_operator_panic_cleanup_verified(
    remaining_process_ids: &[u32],
    running: bool,
    remote_cleanup_required: bool,
    remote_cleanup_verified: bool,
) -> bool {
    remaining_process_ids.is_empty()
        && !running
        && (!remote_cleanup_required || remote_cleanup_verified)
}

fn agent_spawn_operator_panic_cleanup_verified(
    remaining_process_ids: &[u32],
    session_teardown_error: Option<&str>,
    session_teardown_failure_count: Option<u32>,
) -> bool {
    remaining_process_ids.is_empty()
        && session_teardown_error.is_none()
        && session_teardown_failure_count.is_none_or(|failure_count| failure_count == 0)
}

async fn await_agent_spawn_phase_under_operator_panic_guard<T, E>(
    guard: &AgentSpawnInFlightGuard,
    stage: &'static str,
    future: impl Future<Output = Result<T, E>>,
    panic_error: impl Fn(ErrorData) -> E,
) -> Result<T, E> {
    guard.ensure(stage).map_err(&panic_error)?;
    let mut future = Box::pin(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                guard.ensure(stage).map_err(&panic_error)?;
            }
        }
    }
}

#[derive(Debug)]
struct AgentSpawnTiming {
    request_started: Instant,
    prelaunch_done: Option<Instant>,
    launch_started: Option<Instant>,
    launch_completed: Option<Instant>,
    session_wait_started: Option<Instant>,
    session_matched: Option<Instant>,
    task_wait_started: Option<Instant>,
    task_started: Option<Instant>,
}

impl AgentSpawnTiming {
    fn new() -> Self {
        Self {
            request_started: Instant::now(),
            prelaunch_done: None,
            launch_started: None,
            launch_completed: None,
            session_wait_started: None,
            session_matched: None,
            task_wait_started: None,
            task_started: None,
        }
    }

    fn mark_prelaunch_done(&mut self) {
        self.prelaunch_done = Some(Instant::now());
    }

    fn mark_launch_started(&mut self) {
        self.launch_started = Some(Instant::now());
    }

    fn mark_launch_completed(&mut self) {
        self.launch_completed = Some(Instant::now());
    }

    fn mark_session_wait_started(&mut self) {
        self.session_wait_started = Some(Instant::now());
    }

    fn mark_session_matched(&mut self) {
        self.session_matched = Some(Instant::now());
    }

    fn mark_task_wait_started(&mut self) {
        self.task_wait_started = Some(Instant::now());
    }

    fn mark_task_started(&mut self) {
        self.task_started = Some(Instant::now());
    }

    fn readback(&self, guard: &AgentSpawnInFlightGuard, wait_timeout_ms: u64) -> Value {
        let now = Instant::now();
        json!({
            "source_of_truth": "act_spawn_agent in-process timing/in-flight counters plus per-spawn artifacts",
            "spawn_sequence": guard.sequence,
            "in_flight_at_start": guard.in_flight_at_start,
            "in_flight_now": AgentSpawnInFlightGuard::in_flight_now(),
            "wait_timeout_ms": wait_timeout_ms,
            "queue_wait_ms": 0,
            "queue_wait_source": "no_explicit_spawn_queue; readiness deadline starts after this spawn launches",
            "request_elapsed_ms": elapsed_ms_between(self.request_started, now),
            "prelaunch_elapsed_ms": self.prelaunch_done.map(|end| elapsed_ms_between(self.request_started, end)),
            "launch_elapsed_ms": match (self.launch_started, self.launch_completed) {
                (Some(start), Some(end)) => Some(elapsed_ms_between(start, end)),
                (Some(start), None) => Some(elapsed_ms_between(start, now)),
                _ => None,
            },
            "session_wait_elapsed_ms": match (self.session_wait_started, self.session_matched) {
                (Some(start), Some(end)) => Some(elapsed_ms_between(start, end)),
                (Some(start), None) => Some(elapsed_ms_between(start, now)),
                _ => None,
            },
            "task_start_wait_elapsed_ms": match (self.task_wait_started, self.task_started) {
                (Some(start), Some(end)) => Some(elapsed_ms_between(start, end)),
                (Some(start), None) => Some(elapsed_ms_between(start, now)),
                _ => None,
            },
            "deadline_policy": "per_spawn_phase_deadlines_started_after_launch_and_after_session_match",
        })
    }
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn elapsed_ms_between(start: Instant, end: Instant) -> u64 {
    duration_ms_u64(end.saturating_duration_since(start))
}

#[cfg(windows)]
const AGENT_SPAWN_WINDOWS_SHELL_CANDIDATES: &[(&str, &str)] = &[
    ("path:pwsh.exe", "pwsh.exe"),
    (
        "known_path:powershell7_x64",
        r"C:\Program Files\PowerShell\7\pwsh.exe",
    ),
    (
        "known_path:powershell7_x86",
        r"C:\Program Files (x86)\PowerShell\7\pwsh.exe",
    ),
    ("path:powershell.exe", "powershell.exe"),
    (
        "known_path:windows_powershell",
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
    ),
];

#[tool_router(router = m4_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Execute a timed one-shot sequence of act_press key steps only; use act_stroke for continuous mouse motion/path execution"
    )]
    pub async fn act_combo(
        &self,
        params: Parameters<ActComboParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActComboResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_combo",
            step_count = params.0.steps.len(),
            "tool.invocation kind=act_combo"
        );
        let required = required_combo_permissions(&params.0)?;
        self.require_m3_permissions("act_combo", &required)?;
        let preflight = match self.ensure_supported_use_allows_action("act_combo") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_combo", &error, &request_context);
                return Err(error);
            }
        };
        self.refresh_reflex_audit_context()?;
        self.audit_action_started_for_request("act_combo", &request_context)?;
        let runtime = self.reflex_runtime()?;
        let runtime_for_cleanup = runtime.clone();
        let boundary = |stage| ensure_m4_physical_mutation_boundary(&preflight, stage);
        let mut result = execute_combo_with_boundary(runtime, params.0, &boundary).await;
        if let Ok(response) = &result
            && let Err(error) = boundary("act_combo_immediately_after_reflex_schedule")
        {
            let (cleanup, cleanup_verified) = match runtime_for_cleanup.lock() {
                Ok(mut runtime) => match runtime.cancel(&response.combo_id) {
                    Ok(outcome) => (json!({ "cancel_outcome": format!("{outcome:?}") }), true),
                    Err(cancel_error) => (
                        json!({
                            "cancel_error_code": cancel_error.code(),
                            "cancel_error": cancel_error.to_string(),
                        }),
                        false,
                    ),
                },
                Err(_error) => (
                    json!({
                        "cancel_error_code": error_codes::TOOL_INTERNAL_ERROR,
                        "cancel_error": "reflex runtime lock poisoned while cancelling operator-panic-superseded act_combo",
                    }),
                    false,
                ),
            };
            let drain = if cleanup_verified {
                None
            } else {
                synapse_action::record_operator_panic_safety_incident();
                Some(
                    self.drain_state_handle()
                        .mark_draining("operator_panic_act_combo_cleanup_unverified"),
                )
            };
            result = Err(action_preflight_cleanup_error(
                error,
                "act_combo_immediately_after_reflex_schedule",
                json!({
                    "cleanup": cleanup,
                    "cleanup_verified": cleanup_verified,
                    "drain": drain,
                }),
            ));
        }
        self.audit_action_result_for_request("act_combo", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Facade for shell execution. operation=run executes an allowlisted child process; start creates a durable shell job with artifact paths and uses timeout_ms as the durable lifetime cap; status reads persisted job/output state; cancel terminates the exact durable job and returns before/after readback. durable_timeout_ms, execution_mode, and idempotency_key are run-only fields and are invalid for start/status/cancel.",
        input_schema = shell_input_schema()
    )]
    pub async fn shell(
        &self,
        params: Parameters<ShellParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ShellFacadeResponse>, ErrorData> {
        let params = params.0;
        let operation = params.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "shell",
            operation = operation.as_str(),
            "tool.invocation kind=shell"
        );
        match operation {
            ShellOperation::Run => {
                let low_params = shell_run_params(params)?;
                let source_id = low_params.command.clone();
                let response = self
                    .act_run_shell(Parameters(low_params), request_context)
                    .await
                    .map_err(|error| {
                        shell_facade_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the shell command policy, executable path, and job artifacts before retrying",
                        )
                    })?
                    .0;
                Ok(Json(ShellFacadeResponse {
                    operation,
                    source_of_truth: SHELL_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    run: Some(response),
                    start: None,
                    status: None,
                    cancel: None,
                }))
            }
            ShellOperation::Start => {
                let low_params = shell_start_params(params)?;
                let source_id = low_params.command.clone();
                let response = self
                    .act_run_shell_start(Parameters(low_params), request_context)
                    .await
                    .map_err(|error| {
                        shell_facade_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the durable shell command policy and job artifact root before retrying",
                        )
                    })?
                    .0;
                Ok(Json(ShellFacadeResponse {
                    operation,
                    source_of_truth: SHELL_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    run: None,
                    start: Some(response),
                    status: None,
                    cancel: None,
                }))
            }
            ShellOperation::Status => {
                let low_params = shell_status_params(params)?;
                let source_id = low_params.job_id.clone();
                let response = self
                    .act_run_shell_status(Parameters(low_params), request_context)
                    .await
                    .map_err(|error| {
                        shell_facade_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an exact job_id owned by this MCP session and inspect the job status file",
                        )
                    })?
                    .0;
                Ok(Json(ShellFacadeResponse {
                    operation,
                    source_of_truth: SHELL_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    run: None,
                    start: None,
                    status: Some(response),
                    cancel: None,
                }))
            }
            ShellOperation::Cancel => {
                let low_params = shell_cancel_params(params)?;
                let source_id = low_params.job_id.clone();
                let response = self
                    .act_run_shell_cancel(Parameters(low_params), request_context)
                    .await
                    .map_err(|error| {
                        shell_facade_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an exact live job_id owned by this MCP session and inspect before/after status",
                        )
                    })?
                    .0;
                Ok(Json(ShellFacadeResponse {
                    operation,
                    source_of_truth: SHELL_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    run: None,
                    start: None,
                    status: None,
                    cancel: Some(response),
                }))
            }
        }
    }

    #[tool(
        description = "Facade for process capability. operation=list reads the live OS process table; launch delegates to the audited process launcher and records CF_PROCESS_HISTORY; history reads decoded CF_PROCESS_HISTORY rows for launch readback."
    )]
    pub async fn process(
        &self,
        params: Parameters<ProcessParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ProcessFacadeResponse>, ErrorData> {
        let params = params.0;
        let operation = params.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "process",
            operation = operation.as_str(),
            "tool.invocation kind=process"
        );
        match operation {
            ProcessOperation::List => {
                let response = process_list_response(&params)?;
                Ok(Json(ProcessFacadeResponse {
                    operation,
                    source_of_truth: PROCESS_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    launch: None,
                    processes: Some(response),
                    history: None,
                }))
            }
            ProcessOperation::Launch => {
                let low_params = process_launch_params(params)?;
                let source_id = low_params.target.clone();
                let response = self
                    .act_launch(Parameters(low_params), request_context)
                    .await
                    .map_err(|error| {
                        process_facade_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix the target executable/path or launch policy, then verify CF_PROCESS_HISTORY and the process table",
                        )
                    })?
                    .0;
                Ok(Json(ProcessFacadeResponse {
                    operation,
                    source_of_truth: PROCESS_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    launch: Some(response),
                    processes: None,
                    history: None,
                }))
            }
            ProcessOperation::History => {
                let response = process_history_response(self, &params)?;
                Ok(Json(ProcessFacadeResponse {
                    operation,
                    source_of_truth: PROCESS_FACADE_SOURCE_OF_TRUTH.to_owned(),
                    launch: None,
                    processes: None,
                    history: Some(response),
                }))
            }
        }
    }

    #[tool(
        description = "Run an allowlisted executable child process. command is an executable path/name only; pass flags and shell snippets in args, using an explicit shell executable when shell syntax is required. Do not use this to launch headed Playwright/Chromium automation: Synapse refuses direct or shell-wrapped Chrome/Playwright remote-debugging launches that can surface Chrome debugger/automation banners and shift browser layout. Use the existing authenticated Chrome through cdp_* / target_act / browser_* tools, or act_launch with Synapse-injected isolated CDP flags. execution_mode controls routing: auto preserves compatibility and backgrounds when timeout_ms exceeds the inline await limit, inline waits only while timeout_ms fits inside the MCP client-call budget and otherwise returns a durable job handle, durable returns a job handle immediately. durable_timeout_ms is an explicit durable job lifetime cap only when a durable/background job is created; it is ignored when execution completes inline. Omit it for an unbounded durable job. Poll act_run_shell_status and cancel with act_run_shell_cancel."
    )]
    pub async fn act_run_shell(
        &self,
        params: Parameters<ActRunShellParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActRunShellResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_run_shell",
            command = %params.0.command,
            "tool.invocation kind=act_run_shell"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_run_shell") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_run_shell", &error, &request_context);
                return Err(error);
            }
        };
        let raw_params = params.0;
        let session_id = require_shell_session_id(&request_context)?;
        let shell_context = shell_execution_context_for_session(&session_id)?;
        let params = prepare_run_shell_params_for_context(raw_params, &shell_context)?;
        let command_payload =
            run_shell_request_details(&params, self.m4_config.run_shell_inline_await_limit_ms());
        let command_before = json!({
            "source_of_truth": "durable shell registry/log files or inline child process",
            "session_id": &session_id,
            "execution_mode": params.execution_mode.as_str(),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "act_run_shell",
            "shell_run",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        self.audit_action_started_with_details_for_session(
            "act_run_shell",
            &command_payload,
            &session_id,
        )?;
        let mut result = match authorize_run_shell(&self.m4_config, &params) {
            Ok(authorization) => {
                run_shell_with_idempotency(
                    self,
                    params,
                    authorization,
                    self.m4_config.run_shell_inline_await_limit_ms(),
                    Some(&shell_context),
                    &preflight,
                )
                .await
            }
            Err(error) => Err(error),
        };
        if let Ok(response) = &result
            && let Err(error) =
                ensure_m4_physical_mutation_boundary(&preflight, "act_run_shell_after_execution")
        {
            let cleanup = cleanup_shell_job_after_operator_panic(
                self,
                response.job_id.as_deref(),
                &session_id,
                "act_run_shell_after_execution",
            )
            .await;
            result = Err(action_preflight_cleanup_error(
                error,
                "act_run_shell_after_execution",
                cleanup,
            ));
        }
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_run_shell",
                    "shell_run",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "durable shell registry/log files or inline child process",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_run_shell",
                    "shell_run",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "durable shell registry/log files or inline child process",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        self.audit_action_result_for_session("act_run_shell", &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Start an allowlisted executable as a durable background shell job. Do not use this to launch headed Playwright/Chromium automation: Synapse refuses direct or shell-wrapped Chrome/Playwright remote-debugging launches that can surface Chrome debugger/automation banners and shift browser layout. Use existing authenticated Chrome via cdp_* / target_act / browser_* tools, or act_launch with Synapse-injected isolated CDP flags. Returns immediately with a job id plus status/stdout/stderr file paths. Omitting timeout_ms leaves the durable job unbounded until normal exit, explicit act_run_shell_cancel, or session cleanup; providing timeout_ms is an explicit lifetime cap for that job only."
    )]
    pub async fn act_run_shell_start(
        &self,
        params: Parameters<ActRunShellStartParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActRunShellStartResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_run_shell_start",
            command = %params.0.command,
            "tool.invocation kind=act_run_shell_start"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_run_shell") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request(
                    "act_run_shell_start",
                    &error,
                    &request_context,
                );
                return Err(error);
            }
        };
        let raw_params = params.0;
        let session_id = require_shell_session_id(&request_context)?;
        let shell_context = shell_execution_context_for_session(&session_id)?;
        let params = prepare_run_shell_start_params_for_context(raw_params, &shell_context)?;
        let command_payload = run_shell_start_request_details(&params);
        let command_before = json!({
            "source_of_truth": "durable shell registry/log files/process table",
            "session_id": &session_id,
            "job_id": &params.job_id,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "act_run_shell_start",
            "shell_spawn",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        self.audit_action_started_with_details_for_session(
            "act_run_shell_start",
            &command_payload,
            &session_id,
        )?;
        let boundary = |stage| ensure_m4_physical_mutation_boundary(&preflight, stage);
        let mut result = match authorize_run_shell_start(&self.m4_config, &params) {
            Ok(authorization) => start_authorized_shell_job_with_boundary(
                params,
                &authorization,
                Some(&shell_context),
                &boundary,
            ),
            Err(error) => Err(error),
        };
        if let Ok(response) = &result
            && let Err(error) = boundary("act_run_shell_start_after_process_launch")
        {
            let cleanup = cleanup_shell_job_after_operator_panic(
                self,
                Some(&response.job.job_id),
                &session_id,
                "act_run_shell_start_after_process_launch",
            )
            .await;
            result = Err(action_preflight_cleanup_error(
                error,
                "act_run_shell_start_after_process_launch",
                cleanup,
            ));
        }
        match &result {
            Ok(response) => {
                self.command_audit_final(super::command_audit::CommandAuditInput::mcp(
                    "act_run_shell_start",
                    "shell_spawn",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "durable shell registry/log files/process table",
                        "response": response,
                    }),
                    "ok",
                ))?
            }
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_run_shell_start",
                    "shell_spawn",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "durable shell registry/log files/process table",
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(error),
                ),
            )?,
        };
        self.audit_action_result_for_session("act_run_shell_start", &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Read durable background shell job status, no-output/stall diagnostics, process-tree summary, SSH/SCP/SFTP transfer hints, and tails of persisted stdout/stderr logs by job id. This is a separate source-of-truth readback and does not wait for the process to finish."
    )]
    pub async fn act_run_shell_status(
        &self,
        params: Parameters<ActRunShellStatusParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActRunShellStatusResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_run_shell_status",
            job_id = %params.0.job_id,
            "tool.invocation kind=act_run_shell_status"
        );
        if let Err(error) = self.ensure_supported_use_allows_shell_observe_or_cancel() {
            self.audit_action_denied_for_request("act_run_shell_status", &error, &request_context);
            return Err(error);
        }
        let params = params.0;
        let session_id = require_shell_session_id(&request_context)?;
        self.audit_action_started_with_details_for_session(
            "act_run_shell_status",
            &json!({
                "job_id": &params.job_id,
                "tail_bytes": params.tail_bytes,
                "session_id": &session_id,
            }),
            &session_id,
        )?;
        let result =
            shell_job_status_blocking(params, session_id.clone(), "act_run_shell_status").await;
        self.audit_action_result_for_session("act_run_shell_status", &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Cancel a durable background shell job by exact job id, terminating the recorded local job-owned process tree and returning status/log/process readback. Direct SSH jobs with a tracked remote pid/process-group attempt bounded remote cleanup and report whether that cleanup was verified; untracked SSH modes fail closed with the remote cleanup handle/status marked unverified."
    )]
    pub async fn act_run_shell_cancel(
        &self,
        params: Parameters<ActRunShellJobIdParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActRunShellCancelResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_run_shell_cancel",
            job_id = %params.0.job_id,
            "tool.invocation kind=act_run_shell_cancel"
        );
        if let Err(error) = self.ensure_supported_use_allows_shell_observe_or_cancel() {
            self.audit_action_denied_for_request("act_run_shell_cancel", &error, &request_context);
            return Err(error);
        }
        let params = params.0;
        let session_id = require_shell_session_id(&request_context)?;
        let before_status = shell_job_status_blocking(
            ActRunShellStatusParams {
                job_id: params.job_id.clone(),
                tail_bytes: 1024,
            },
            session_id.clone(),
            "act_run_shell_cancel.before_status",
        )
        .await;
        let command_payload = json!({
            "job_id": &params.job_id,
            "session_id": &session_id,
        });
        let command_before = json!({
            "source_of_truth": "durable shell registry/log files/process table",
            "status_readback": before_status.as_ref().ok(),
            "status_error": before_status.as_ref().err().map(|error| json!({
                "code": error.data.as_ref().and_then(|data| data.get("code")).and_then(Value::as_str),
                "message": error.message.to_string(),
            })),
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "act_run_shell_cancel",
            "kill",
            Some(session_id.clone()),
            Some(session_id.clone()),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        self.audit_action_started_with_details_for_session(
            "act_run_shell_cancel",
            &command_payload,
            &session_id,
        )?;
        let result = cancel_shell_job_blocking(params.clone(), session_id.clone()).await;
        let after_status = shell_job_status_blocking(
            ActRunShellStatusParams {
                job_id: params.job_id.clone(),
                tail_bytes: 1024,
            },
            session_id.clone(),
            "act_run_shell_cancel.after_status",
        )
        .await;
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_run_shell_cancel",
                    "kill",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "durable shell registry/log files/process table",
                        "response": response,
                        "status_readback": after_status.as_ref().ok(),
                        "status_error": after_status.as_ref().err().map(|error| json!({
                            "code": error.data.as_ref().and_then(|data| data.get("code")).and_then(Value::as_str),
                            "message": error.message.to_string(),
                        })),
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_run_shell_cancel",
                    "kill",
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "durable shell registry/log files/process table",
                        "status_readback": after_status.as_ref().ok(),
                        "status_error": after_status.as_ref().err().map(|error| json!({
                            "code": error.data.as_ref().and_then(|data| data.get("code")).and_then(Value::as_str),
                            "message": error.message.to_string(),
                        })),
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        self.audit_action_result_for_session("act_run_shell_cancel", &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Launch an allowlisted local process, optionally on a session-owned hidden desktop, and optionally wait for a visible-desktop window when no desktop override is used"
    )]
    pub async fn act_launch(
        &self,
        params: Parameters<ActLaunchParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActLaunchResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_launch",
            target = %params.0.target,
            "tool.invocation kind=act_launch"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_launch") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_launch", &error, &request_context);
                return Err(error);
            }
        };
        let params = params.0;
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.act_launch_for_session_id(params, session_id, &preflight)
            .await
            .map(Json)
    }

    #[tool(
        description = "Spawn a fully capable primary Codex, Claude, or local_model agent as a hidden background process, wire it to the configured Synapse HTTP MCP daemon, require real MCP session registration, optionally bind a per-session target, and return only after public session facade readback plus a validated task-start readiness artifact prove the spawned prompt began executing. Pass cli/kind plus a non-empty prompt for a direct spawn, or template_id (+ template_params) to render the spawn from a durable agent_template; a template-rendered spawn records the exact (template_id, version, config_hash) used and rejects passing cli/kind/model/model_ref/prompt/working_dir/target alongside the template."
    )]
    pub async fn act_spawn_agent(
        &self,
        params: Parameters<ActSpawnAgentRequest>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActSpawnAgentResponse>, ErrorData> {
        self.spawn_agent_journaled(params.0, &request_context)
            .await
            .map(Json)
    }

    #[tool(
        description = "Spawned-agent cooperative readiness: after the spawned agent has called health/session facade and verified its target, call this tool with the daemon-issued spawn_id. The daemon uses the real MCP request session id to atomically write task-started.json, avoiding permission-gated local shell readiness helpers."
    )]
    pub async fn agent_spawn_task_started(
        &self,
        params: Parameters<AgentSpawnTaskStartedParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<AgentSpawnTaskStartedResponse>, ErrorData> {
        self.agent_spawn_task_started_impl(params.0, &request_context)
            .map(Json)
    }
}

impl SynapseService {
    fn agent_spawn_task_started_impl(
        &self,
        params: AgentSpawnTaskStartedParams,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<AgentSpawnTaskStartedResponse, ErrorData> {
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::HTTP_SESSION_INVALID,
                    "agent_spawn_task_started requires an MCP session id (the daemon writes task-started.json from the real request session, not a caller-supplied value)",
                )
            })?;
        write_agent_spawn_task_started_from_session(&params.spawn_id, &session_id)
    }

    /// Shared audited spawn path for `act_spawn_agent` and task auto-dispatch.
    pub(crate) async fn spawn_agent_journaled(
        &self,
        request: ActSpawnAgentRequest,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
        let mut spawn_activity = AgentSpawnInFlightGuard::enter("mcp_or_task_dispatch")?;
        if let Err(error) = self.ensure_supported_use_allows_action("act_launch") {
            self.audit_action_denied_for_request(ACT_SPAWN_AGENT, &error, request_context);
            return Err(error);
        }
        // Resolve the request — direct spawn or template-rendered — into the
        // concrete spawn params before any side effect, so a bad template id or
        // param contract fails loudly with nothing launched (#909).
        let params = match self.resolve_spawn_request(request) {
            Ok(params) => params,
            Err(error) => {
                self.audit_action_denied_for_request(ACT_SPAWN_AGENT, &error, request_context);
                return Err(error);
            }
        };
        let agent_kind = params.effective_cli()?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = ACT_SPAWN_AGENT,
            cli = agent_kind.as_str(),
            model_ref = params.local_model_ref().unwrap_or(""),
            template_id = params.template_id.as_deref().unwrap_or(""),
            template_version = params.template_version.unwrap_or(0),
            "tool.invocation kind=act_spawn_agent"
        );
        let started_by_session_id =
            super::context::mcp_session_id_from_request_context(request_context)?;
        let actor_session_id_for_audit = started_by_session_id.clone();
        self.audit_action_started_with_details_for_request(
            ACT_SPAWN_AGENT,
            &agent_spawn_request_details(&params, started_by_session_id.as_deref()),
            request_context,
        )?;
        // The spawn id is allocated before any side effect so every journal
        // event of this lifecycle (#897) shares one attribution anchor; a
        // spawn that cannot be journaled is refused before launching.
        let spawn_id = format!("agent-spawn-{}", new_reflex_id());
        spawn_activity.identify(&spawn_id, agent_kind);
        let command_payload =
            agent_spawn_request_details(&params, started_by_session_id.as_deref());
        let command_before = json!({
            "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
            "spawn_id": &spawn_id,
            "before_session_ids": self.current_session_ids()?,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            ACT_SPAWN_AGENT,
            "spawn",
            started_by_session_id.clone(),
            started_by_session_id.clone(),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        self.journal_spawn_requested(&spawn_id, &params, started_by_session_id.as_deref())?;
        let result = self
            .act_spawn_agent_impl(
                params,
                started_by_session_id,
                spawn_id.clone(),
                &spawn_activity,
            )
            .await;
        if let Err(error) = spawn_activity.ensure("mcp_after_spawn_impl") {
            if let Ok(response) = &result {
                self.cleanup_spawn_response_after_operator_panic(response, "mcp_after_spawn_impl")
                    .await;
            }
            return Err(error);
        }
        match &result {
            Ok(response) => {
                if let Err(journal_error) = self.journal_spawn_ready(response) {
                    self.audit_action_result_for_request::<ActSpawnAgentResponse>(
                        ACT_SPAWN_AGENT,
                        &Err(journal_error.clone()),
                        request_context,
                    )?;
                    self.command_audit_final(
                        super::command_audit::CommandAuditInput::mcp(
                            ACT_SPAWN_AGENT,
                            "spawn",
                            actor_session_id_for_audit.clone(),
                            Some(response.session_id.clone()),
                            command_payload.clone(),
                            command_before.clone(),
                            json!({
                                "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                                "spawn_id": &spawn_id,
                                "response": response,
                                "after_session_ids": self.current_session_ids().unwrap_or_default(),
                            }),
                            "error",
                        )
                        .with_error(super::command_audit::command_audit_error_from_error_data(
                            &journal_error,
                        )),
                    )?;
                    return Err(journal_error);
                }
            }
            Err(error) => {
                // The spawn error must win the response; the journal failure
                // (if any) is already logged as AGENT_EVENT_WRITE_FAILED.
                self.journal_spawn_failed(&spawn_id, error);
            }
        }
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    ACT_SPAWN_AGENT,
                    "spawn",
                    actor_session_id_for_audit.clone(),
                    Some(response.session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                        "spawn_id": &spawn_id,
                        "response": response,
                        "after_session_ids": self.current_session_ids().unwrap_or_default(),
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    ACT_SPAWN_AGENT,
                    "spawn",
                    actor_session_id_for_audit.clone(),
                    actor_session_id_for_audit.clone(),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                        "spawn_id": &spawn_id,
                        "after_session_ids": self.current_session_ids().unwrap_or_default(),
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        self.audit_action_result_for_request(ACT_SPAWN_AGENT, &result, request_context)?;
        result
    }
}

impl SynapseService {
    pub(super) async fn act_launch_for_session_id(
        &self,
        params: ActLaunchParams,
        session_id: Option<String>,
        preflight: &super::action_preflight::ActionPreflightReadback,
    ) -> Result<ActLaunchResponse, ErrorData> {
        let command_payload = launch_request_details(&params);
        let command_before = json!({
            "source_of_truth": "process table plus CF_PROCESS_HISTORY/session lifecycle resources",
            "session_id": &session_id,
        });
        self.command_audit_intent(super::command_audit::CommandAuditInput::mcp(
            "act_launch",
            "spawn",
            session_id.clone(),
            session_id.clone(),
            command_payload.clone(),
            command_before.clone(),
            Value::Null,
            "pending",
        ))?;
        if let Some(session_id) = session_id.as_deref() {
            self.audit_action_started_with_details_for_session(
                "act_launch",
                &command_payload,
                session_id,
            )?;
        } else {
            self.audit_action_started_with_details("act_launch", &command_payload)?;
        }
        let boundary = |stage| ensure_m4_physical_mutation_boundary(preflight, stage);
        let result = match launch_for_session_with_boundary(
            &self.m4_config,
            params.clone(),
            session_id.as_deref(),
            &boundary,
        )
        .await
        {
            Ok(mut outcome) => {
                let response = outcome.response.clone();
                if let Err(error) = boundary("act_launch_after_low_level_launch") {
                    let cleanup = crate::m4::terminate_owned_process_tree(response.pid);
                    let cleanup_verified = cleanup.remaining_process_ids.is_empty();
                    let drain = if cleanup_verified {
                        None
                    } else {
                        synapse_action::record_operator_panic_safety_incident();
                        Some(
                            self.drain_state_handle()
                                .mark_draining("operator_panic_act_launch_cleanup_unverified"),
                        )
                    };
                    return Err(action_preflight_cleanup_error(
                        error,
                        "act_launch_after_low_level_launch",
                        json!({
                            "source_of_truth": "exact launched process tree + separate process-table readback",
                            "pid": response.pid,
                            "termination": cleanup,
                            "cleanup_verified": cleanup_verified,
                            "drain": drain,
                        }),
                    ));
                }
                let process_job = if session_id.is_some() {
                    match assign_owned_process_job(response.pid, "act_launch", None) {
                        Ok(process_job) => Some(process_job),
                        Err(error) => {
                            let cleanup = crate::m4::terminate_owned_process_tree(response.pid);
                            return Err(launch_lifecycle_tool_error(
                                "act_launch spawned the process but failed to assign a session process job; exact spawned PID cleanup was attempted",
                                json!({
                                    "code": error_codes::TOOL_INTERNAL_ERROR,
                                    "reason": "process_job_assign_failed",
                                    "pid": response.pid,
                                    "source_error": error.message,
                                    "cleanup": cleanup,
                                }),
                            ));
                        }
                    }
                } else {
                    None
                };
                if let Err(error) = record_launch_process_history(self, &params, &response) {
                    let cleanup = crate::m4::terminate_owned_process_tree(response.pid);
                    return Err(launch_lifecycle_tool_error(
                        "act_launch spawned the process but failed to record process history; exact spawned PID cleanup was attempted",
                        json!({
                            "code": error_codes::TOOL_INTERNAL_ERROR,
                            "reason": "process_history_record_failed",
                            "pid": response.pid,
                            "source_error": error.message,
                            "cleanup": cleanup,
                        }),
                    ));
                }
                if let (Some(session_id), Some(process_job)) = (session_id.clone(), process_job) {
                    if let Err(error) = self.register_session_process_resource(
                        super::session_lifecycle::SessionProcessResource::new(
                            session_id,
                            "act_launch",
                            response.pid,
                            None,
                            params.target.clone(),
                            process_job,
                        )
                        .with_desktop_lease(outcome.desktop_lease.take()),
                    ) {
                        let cleanup = crate::m4::terminate_owned_process_tree(response.pid);
                        return Err(launch_lifecycle_tool_error(
                            "act_launch spawned the process but failed to register the session process resource; exact spawned PID cleanup was attempted",
                            json!({
                                "code": error_codes::TOOL_INTERNAL_ERROR,
                                "reason": "session_process_register_failed",
                                "pid": response.pid,
                                "source_error": error.message,
                                "cleanup": cleanup,
                            }),
                        ));
                    }
                }
                Ok(response)
            }
            Err(error) => Err(error),
        };
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_launch",
                    "spawn",
                    session_id.clone(),
                    session_id.clone(),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "process table plus CF_PROCESS_HISTORY/session lifecycle resources",
                        "response": response,
                    }),
                    "ok",
                ),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "act_launch",
                    "spawn",
                    session_id.clone(),
                    session_id.clone(),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "process table plus CF_PROCESS_HISTORY/session lifecycle resources",
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(error)),
            )?,
        };
        if let Some(session_id) = session_id.as_deref() {
            self.audit_action_result_for_session("act_launch", &result, session_id)?;
        } else {
            self.audit_action_result("act_launch", &result)?;
        }
        result
    }
}

fn record_launch_process_history(
    service: &SynapseService,
    params: &ActLaunchParams,
    response: &ActLaunchResponse,
) -> Result<(), ErrorData> {
    let row = launch_process_history_row(params, response)?;
    let row_key = launch_process_history_row_key(response);
    let runtime = service.reflex_runtime()?;
    let runtime = runtime.lock().map_err(|_error| {
        mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned while recording act_launch process history",
        )
    })?;
    runtime
        .storage_put_process_history_rows(vec![(row_key, row)])
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

fn launch_agent_spawn_with_terminal_capture(
    config: &crate::m4::M4ServiceConfig,
    params: &ActLaunchParams,
    files: &AgentSpawnFiles,
    spawn_id: &str,
    launched_at_unix_ms: u64,
) -> Result<ActLaunchResponse, ErrorData> {
    crate::m4::validate_launch_authorized(config, params)?;
    let env = crate::m4::launch_child_environment(params, ACT_SPAWN_AGENT)?;
    let artifacts = agent_spawn_terminal_capture_artifacts(&files.log_dir);
    let spec = CaptureSpec {
        live_key: Some(spawn_id.to_owned()),
        program: params.target.clone(),
        args: params.args.clone(),
        cwd: params.working_dir.as_ref().map(PathBuf::from),
        env,
        cols: 120,
        rows: 40,
        started_unix_secs: launched_at_unix_ms / 1000,
        title: Some(format!("Synapse {spawn_id}")),
    };
    let spawned = spawn_capture_to_asciicast(spec, artifacts.clone()).map_err(|error| {
        agent_spawn_tool_error(
            error_codes::ACTION_AGENT_SPAWN_FAILED,
            "act_spawn_agent failed to start the spawned agent in an owned PTY terminal",
            json!({
                "code": error_codes::ACTION_AGENT_SPAWN_FAILED,
                "reason": "owned_pty_spawn_failed",
                "spawn_id": spawn_id,
                "launch_target": params.target,
                "log_dir": files.log_dir.display().to_string(),
                "terminal_asciicast_path": artifacts.asciicast_path.display().to_string(),
                "terminal_capture_status_path": artifacts.status_path.display().to_string(),
                "terminal_final_screen_path": artifacts.final_screen_path.display().to_string(),
                "terminal_input_audit_path": artifacts.input_audit_path.display().to_string(),
                "source_error": error.to_string(),
            }),
        )
    })?;
    tracing::info!(
        code = "AGENT_SPAWN_OWNED_PTY_STARTED",
        spawn_id,
        pid = spawned.process_id,
        asciicast_path = %spawned.artifacts.asciicast_path.display(),
        status_path = %spawned.artifacts.status_path.display(),
        "act_spawn_agent launched wrapper in an owned PTY"
    );
    Ok(ActLaunchResponse {
        pid: spawned.process_id,
        hwnd: None,
        window_owner_pid: None,
        reused_existing_window: false,
        matched_title: None,
        launched_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("owned_conpty_terminal_capture".to_owned()),
        cdp_debug_port: None,
        cdp_endpoint: None,
        cdp_user_data_dir: None,
        cdp_verified_url: None,
        cdp_verified_title: None,
        desktop: None,
    })
}

fn require_shell_session_id(
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)?.ok_or_else(|| {
        mcp_error(
            error_codes::HTTP_SESSION_INVALID,
            "act_run_shell tools require an MCP session id (run the daemon in HTTP mode so each agent has its own Mcp-Session-Id)",
        )
    })
}

async fn shell_job_status_blocking(
    params: ActRunShellStatusParams,
    session_id: String,
    tool: &'static str,
) -> Result<ActRunShellStatusResponse, ErrorData> {
    tokio::task::spawn_blocking(move || shell_job_status(&params, Some(&session_id)))
        .await
        .map_err(|error| blocking_worker_join_error(tool, error))?
}

async fn cancel_shell_job_blocking(
    params: ActRunShellJobIdParams,
    session_id: String,
) -> Result<ActRunShellCancelResponse, ErrorData> {
    tokio::task::spawn_blocking(move || cancel_shell_job(&params, Some(&session_id)))
        .await
        .map_err(|error| blocking_worker_join_error("act_run_shell_cancel", error))?
}

async fn cleanup_shell_job_after_operator_panic(
    service: &SynapseService,
    job_id: Option<&str>,
    session_id: &str,
    stage: &'static str,
) -> Value {
    let Some(job_id) = job_id else {
        return json!({
            "source_of_truth": "inline shell response (child already exited and was reaped)",
            "stage": stage,
            "cleanup_verified": true,
            "cancel_attempted": false,
        });
    };
    let cancel = cancel_shell_job_blocking(
        ActRunShellJobIdParams {
            job_id: job_id.to_owned(),
        },
        session_id.to_owned(),
    )
    .await;
    let cleanup_verified = cancel
        .as_ref()
        .is_ok_and(shell_operator_panic_cancel_verified);
    let drain = if cleanup_verified {
        None
    } else {
        synapse_action::record_operator_panic_safety_incident();
        Some(
            service
                .drain_state_handle()
                .mark_draining("operator_panic_shell_cleanup_unverified"),
        )
    };
    json!({
        "source_of_truth": "durable shell status + identity-bound local process tree + remote cleanup readback",
        "stage": stage,
        "job_id": job_id,
        "cancel": cancel,
        "cleanup_verified": cleanup_verified,
        "drain": drain,
    })
}

fn blocking_worker_join_error(tool: &str, error: tokio::task::JoinError) -> ErrorData {
    mcp_error(
        error_codes::TOOL_INTERNAL_ERROR,
        format!("{tool}: blocking worker join failed: {error}"),
    )
}

impl SynapseService {
    pub(crate) async fn dashboard_spawn_local_model_agent(
        &self,
        params: ActSpawnAgentParams,
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
        let mut spawn_activity = AgentSpawnInFlightGuard::enter("dashboard_local_model")?;
        tracing::info!(
            code = "DASHBOARD_LOCAL_MODEL_SPAWN_REQUESTED",
            kind = ACT_SPAWN_AGENT,
            "dashboard.invocation kind=act_spawn_agent"
        );
        if let Err(error) = self.ensure_supported_use_allows_action("act_launch") {
            self.audit_action_denied_with_details(
                ACT_SPAWN_AGENT,
                &error,
                &json!({
                    "channel": "dashboard",
                    "source": "dashboard_local_model_spawn",
                }),
            );
            return Err(error);
        }
        let agent_kind = params.effective_cli()?;
        if !agent_kind.is_local_model() {
            let error = mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "dashboard local-model spawn requires kind=local_model",
            );
            self.audit_action_denied_with_details(
                ACT_SPAWN_AGENT,
                &error,
                &json!({
                    "channel": "dashboard",
                    "source": "dashboard_local_model_spawn",
                    "requested_kind": agent_kind.as_str(),
                }),
            );
            return Err(error);
        }

        let command_payload = agent_spawn_request_details(&params, None);
        self.audit_action_started_with_details(ACT_SPAWN_AGENT, &command_payload)?;
        let spawn_id = format!("agent-spawn-{}", new_reflex_id());
        spawn_activity.identify(&spawn_id, agent_kind);
        let command_before = json!({
            "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
            "spawn_id": &spawn_id,
            "before_session_ids": self.current_session_ids()?,
            "dashboard_channel": true,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                ACT_SPAWN_AGENT,
                "spawn",
                None,
                None,
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        self.journal_spawn_requested(&spawn_id, &params, None)?;
        let result = self
            .act_spawn_agent_impl(params, None, spawn_id.clone(), &spawn_activity)
            .await;
        if let Err(error) = spawn_activity.ensure("dashboard_local_model_after_spawn_impl") {
            if let Ok(response) = &result {
                self.cleanup_spawn_response_after_operator_panic(
                    response,
                    "dashboard_local_model_after_spawn_impl",
                )
                .await;
            }
            return Err(error);
        }
        match &result {
            Ok(response) => {
                if let Err(journal_error) = self.journal_spawn_ready(response) {
                    self.command_audit_final(
                        super::command_audit::CommandAuditInput::mcp(
                            ACT_SPAWN_AGENT,
                            "spawn",
                            None,
                            Some(response.session_id.clone()),
                            command_payload.clone(),
                            command_before.clone(),
                            json!({
                                "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                                "spawn_id": &spawn_id,
                                "response": response,
                                "after_session_ids": self.current_session_ids().unwrap_or_default(),
                            }),
                            "error",
                        )
                        .with_channel("dashboard")
                        .with_error(super::command_audit::command_audit_error_from_error_data(
                            &journal_error,
                        )),
                    )?;
                    self.audit_action_result::<ActSpawnAgentResponse>(
                        ACT_SPAWN_AGENT,
                        &Err(journal_error.clone()),
                    )?;
                    return Err(journal_error);
                }
            }
            Err(error) => {
                self.journal_spawn_failed(&spawn_id, error);
            }
        }
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    ACT_SPAWN_AGENT,
                    "spawn",
                    None,
                    Some(response.session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                        "spawn_id": &spawn_id,
                        "response": response,
                        "after_session_ids": self.current_session_ids().unwrap_or_default(),
                    }),
                    "ok",
                )
                .with_channel("dashboard"),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    ACT_SPAWN_AGENT,
                    "spawn",
                    None,
                    None,
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                        "spawn_id": &spawn_id,
                        "after_session_ids": self.current_session_ids().unwrap_or_default(),
                    }),
                    "error",
                )
                .with_channel("dashboard")
                .with_error(super::command_audit::command_audit_error_from_error_data(
                    error,
                )),
            )?,
        };
        self.audit_action_result(ACT_SPAWN_AGENT, &result)?;
        result
    }

    pub(crate) async fn dashboard_spawn_agent_request(
        &self,
        request: ActSpawnAgentRequest,
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
        let mut spawn_activity = AgentSpawnInFlightGuard::enter("dashboard_or_task_dispatch")?;
        tracing::info!(
            code = "DASHBOARD_AGENT_SPAWN_REQUESTED",
            kind = ACT_SPAWN_AGENT,
            template_id = request.template_id.as_deref().unwrap_or(""),
            "dashboard.invocation kind=act_spawn_agent"
        );
        if let Err(error) = self.ensure_supported_use_allows_action("act_launch") {
            self.audit_action_denied_with_details(
                ACT_SPAWN_AGENT,
                &error,
                &json!({
                    "channel": "dashboard",
                    "source": "dashboard_spawn_agent",
                }),
            );
            return Err(error);
        }
        let params = match self.resolve_spawn_request(request) {
            Ok(params) => params,
            Err(error) => {
                self.audit_action_denied_with_details(
                    ACT_SPAWN_AGENT,
                    &error,
                    &json!({
                        "channel": "dashboard",
                        "source": "dashboard_spawn_agent",
                    }),
                );
                return Err(error);
            }
        };
        let agent_kind = params.effective_cli()?;
        tracing::info!(
            code = "DASHBOARD_AGENT_SPAWN_RESOLVED",
            cli = agent_kind.as_str(),
            model_ref = params.local_model_ref().unwrap_or(""),
            template_id = params.template_id.as_deref().unwrap_or(""),
            template_version = params.template_version.unwrap_or(0),
            "dashboard.invocation kind=act_spawn_agent resolved"
        );

        let command_payload = agent_spawn_request_details(&params, None);
        self.audit_action_started_with_details(ACT_SPAWN_AGENT, &command_payload)?;
        let spawn_id = format!("agent-spawn-{}", new_reflex_id());
        spawn_activity.identify(&spawn_id, agent_kind);
        let command_before = json!({
            "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
            "spawn_id": &spawn_id,
            "before_session_ids": self.current_session_ids()?,
            "dashboard_channel": true,
        });
        self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                ACT_SPAWN_AGENT,
                "spawn",
                None,
                None,
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            )
            .with_channel("dashboard"),
        )?;
        self.journal_spawn_requested(&spawn_id, &params, None)?;
        let result = self
            .act_spawn_agent_impl(params, None, spawn_id.clone(), &spawn_activity)
            .await;
        if let Err(error) = spawn_activity.ensure("dashboard_after_spawn_impl") {
            if let Ok(response) = &result {
                self.cleanup_spawn_response_after_operator_panic(
                    response,
                    "dashboard_after_spawn_impl",
                )
                .await;
            }
            return Err(error);
        }
        match &result {
            Ok(response) => {
                if let Err(journal_error) = self.journal_spawn_ready(response) {
                    self.command_audit_final(
                        super::command_audit::CommandAuditInput::mcp(
                            ACT_SPAWN_AGENT,
                            "spawn",
                            None,
                            Some(response.session_id.clone()),
                            command_payload.clone(),
                            command_before.clone(),
                            json!({
                                "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                                "spawn_id": &spawn_id,
                                "response": response,
                                "after_session_ids": self.current_session_ids().unwrap_or_default(),
                            }),
                            "error",
                        )
                        .with_channel("dashboard")
                        .with_error(super::command_audit::command_audit_error_from_error_data(
                            &journal_error,
                        )),
                    )?;
                    self.audit_action_result::<ActSpawnAgentResponse>(
                        ACT_SPAWN_AGENT,
                        &Err(journal_error.clone()),
                    )?;
                    return Err(journal_error);
                }
            }
            Err(error) => {
                self.journal_spawn_failed(&spawn_id, error);
            }
        }
        match &result {
            Ok(response) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    ACT_SPAWN_AGENT,
                    "spawn",
                    None,
                    Some(response.session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                        "spawn_id": &spawn_id,
                        "response": response,
                        "after_session_ids": self.current_session_ids().unwrap_or_default(),
                    }),
                    "ok",
                )
                .with_channel("dashboard"),
            )?,
            Err(error) => self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    ACT_SPAWN_AGENT,
                    "spawn",
                    None,
                    None,
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                        "spawn_id": &spawn_id,
                        "after_session_ids": self.current_session_ids().unwrap_or_default(),
                    }),
                    "error",
                )
                .with_channel("dashboard")
                .with_error(super::command_audit::command_audit_error_from_error_data(
                    error,
                )),
            )?,
        };
        self.audit_action_result(ACT_SPAWN_AGENT, &result)?;
        result
    }

    /// Registers an OpenAI-compatible API model (e.g. DeepSeek) from the
    /// dashboard channel. This is the same registry the MCP `local_model_register`
    /// tool writes to (CF_KV `local_model_registry/v1/...`); registration runs
    /// the forced structured tool-call probe against the live endpoint before
    /// persisting, so a model that cannot tool-call (or whose key is missing) is
    /// rejected loudly here rather than failing on first spawn. The credential
    /// itself is never stored — only the `api_key_env_var` name is — and the
    /// daemon must already have that env var set (the spawn path forwards it into
    /// the agent child via [`resolve_spawn_local_model_api_key`]).
    pub(crate) async fn dashboard_register_api_model(
        &self,
        params: crate::m3::local_models::LocalModelRegisterParams,
    ) -> Result<crate::m3::local_models::LocalModelRegisterResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_API_MODEL_REGISTER_REQUESTED",
            name = %params.name,
            model_id = %params.model_id,
            allow_non_loopback = params.allow_non_loopback,
            has_api_key_env_var = params.api_key_env_var.is_some(),
            "dashboard.invocation kind=local_model_register"
        );
        let db = self.m3_storage()?;
        crate::m3::local_models::register_local_model(&db, params, "dashboard").await
    }

    /// Lists the local/API model registry rows for the dashboard spawn-console
    /// model picker. Reads the same CF_KV source of truth as `local_model_list`.
    pub(crate) fn dashboard_list_local_models(
        &self,
    ) -> Result<crate::m3::local_models::LocalModelListResponse, ErrorData> {
        let db = self.m3_storage()?;
        crate::m3::local_models::list_local_models(
            &db,
            &crate::m3::local_models::LocalModelListParams {
                name: None,
                include_disabled: true,
                limit: 1000,
            },
        )
    }

    /// Re-probes a single model-registry row from the dashboard. This persists
    /// the same probe evidence as the MCP `local_model_probe` tool, keeping the
    /// model table's manual action tied to the CF_KV registry source of truth.
    pub(crate) async fn dashboard_probe_local_model(
        &self,
        params: crate::m3::local_models::LocalModelProbeParams,
    ) -> Result<crate::m3::local_models::LocalModelProbeResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_MODEL_PROBE_REQUESTED",
            name = %params.name,
            timeout_ms = ?params.timeout_ms,
            "dashboard.invocation kind=local_model_probe"
        );
        let db = self.m3_storage()?;
        crate::m3::local_models::probe_local_model(&db, &params, "dashboard").await
    }

    /// Removes a model-registry row (and any stored encrypted key) for the
    /// dashboard model manager. Same CF_KV source of truth as `local_model_remove`.
    pub(crate) fn dashboard_remove_local_model(
        &self,
        params: crate::m3::local_models::LocalModelRemoveParams,
    ) -> Result<crate::m3::local_models::LocalModelRemoveResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_MODEL_REMOVE_REQUESTED",
            name = %params.name,
            "dashboard.invocation kind=local_model_remove"
        );
        let db = self.m3_storage()?;
        crate::m3::local_models::remove_local_model(&db, &params)
    }

    /// Updates a model-registry row (rename, edit fields, (re)store/clear the
    /// API key, enable/disable) for the dashboard model manager, re-probing as
    /// `update_local_model` requires. Same CF_KV source of truth.
    pub(crate) async fn dashboard_update_local_model(
        &self,
        params: crate::m3::local_models::LocalModelUpdateParams,
    ) -> Result<crate::m3::local_models::LocalModelUpdateResponse, ErrorData> {
        tracing::info!(
            code = "DASHBOARD_MODEL_UPDATE_REQUESTED",
            name = %params.name,
            new_name = params.new_name.as_deref().unwrap_or(""),
            has_api_key = params.api_key.is_some(),
            clear_api_key = params.clear_api_key,
            enabled = ?params.enabled,
            "dashboard.invocation kind=local_model_update"
        );
        let db = self.m3_storage()?;
        crate::m3::local_models::update_local_model(&db, params, "dashboard").await
    }

    /// Resolves a caller's spawn request into concrete spawn params. A direct
    /// spawn (no `template_id`) passes its fields through; a template spawn
    /// renders the params atomically from the durable template and stamps the
    /// `(id, version, config_hash)` provenance. The two modes are mutually
    /// exclusive and conflicts are rejected loudly — never silently merged.
    pub(crate) fn resolve_spawn_request(
        &self,
        request: ActSpawnAgentRequest,
    ) -> Result<ActSpawnAgentParams, ErrorData> {
        match request.template_id {
            Some(template_id) => {
                let mut conflicts = Vec::new();
                if request.cli.is_some() {
                    conflicts.push("cli");
                }
                if request.kind.is_some() {
                    conflicts.push("kind");
                }
                if request.model.is_some() {
                    conflicts.push("model");
                }
                if request.model_ref.is_some() {
                    conflicts.push("model_ref");
                }
                if request.prompt.is_some() {
                    conflicts.push("prompt");
                }
                if request.working_dir.is_some() {
                    conflicts.push("working_dir");
                }
                if request.target.is_some() {
                    conflicts.push("target");
                }
                if !conflicts.is_empty() {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        format!(
                            "act_spawn_agent renders {conflicts:?} from template {template_id:?}; edit the template instead of passing these fields alongside template_id"
                        ),
                    ));
                }
                let rendered = self.resolve_spawn_template(
                    &template_id,
                    request.template_version,
                    &request.template_params,
                )?;
                Ok(ActSpawnAgentParams {
                    cli: Some(rendered.cli),
                    kind: Some(rendered.cli),
                    model: rendered.model,
                    model_ref: rendered.model_ref,
                    prompt: rendered.prompt,
                    target: rendered.target,
                    working_dir: rendered.working_dir,
                    mcp_url: request.mcp_url,
                    wait_timeout_ms: request.wait_timeout_ms,
                    hold_open_ms: request.hold_open_ms,
                    require_approval_gate: request.require_approval_gate,
                    template_id: Some(rendered.provenance.template_id),
                    template_version: Some(rendered.provenance.version),
                    template_config_hash: Some(rendered.provenance.config_hash),
                })
            }
            None => {
                if request.template_version.is_some() || !request.template_params.is_empty() {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "act_spawn_agent template_version/template_params require template_id to be set",
                    ));
                }
                let params = ActSpawnAgentParams {
                    cli: request.cli,
                    kind: request.kind,
                    model: request.model,
                    model_ref: request.model_ref,
                    prompt: request.prompt,
                    target: request.target,
                    working_dir: request.working_dir,
                    mcp_url: request.mcp_url,
                    wait_timeout_ms: request.wait_timeout_ms,
                    hold_open_ms: request.hold_open_ms,
                    require_approval_gate: request.require_approval_gate,
                    template_id: None,
                    template_version: None,
                    template_config_hash: None,
                };
                validate_agent_spawn_params(&params)?;
                Ok(params)
            }
        }
    }

    async fn act_spawn_agent_impl(
        &self,
        params: ActSpawnAgentParams,
        started_by_session_id: Option<String>,
        spawn_id: String,
        in_flight: &AgentSpawnInFlightGuard,
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
        validate_agent_spawn_params(&params)?;
        validate_spawn_target(&params.target)?;
        let agent_kind = params.effective_cli()?;
        let local_model_row = if agent_kind.is_local_model() {
            Some(
                await_agent_spawn_phase_under_operator_panic_guard(
                    in_flight,
                    "while_resolving_local_model_spawn_prerequisite",
                    self.require_spawn_local_model_row(
                        &params,
                        started_by_session_id
                            .as_deref()
                            .unwrap_or("dashboard_spawn_agent"),
                    ),
                    |error| error,
                )
                .await?,
            )
        } else {
            None
        };
        in_flight.ensure("impl_after_async_prerequisites")?;
        let mut timing = AgentSpawnTiming::new();

        let orphan_recovery = recover_orphaned_agent_spawn_terminal_artifacts()?;
        if orphan_recovery.recovered_count > 0 {
            tracing::warn!(
                code = "AGENT_SPAWN_ORPHAN_RECOVERY",
                ?orphan_recovery,
                "act_spawn_agent recovered stale non-terminal agent spawn artifacts before launching a new agent"
            );
        }

        let working_dir = resolve_agent_working_dir(params.working_dir.as_deref())?;
        let token = read_synapse_bearer_token()?;
        let before_session_ids = self.current_session_ids()?;
        let launched_at_unix_ms = unix_time_ms_now();
        let files = prepare_agent_spawn_files(&spawn_id, &params, &working_dir)?;
        let script = agent_spawn_powershell_script(&params, &files, &working_dir)?;
        let launch_host = match resolve_agent_spawn_powershell_host() {
            Ok(launch_host) => launch_host,
            Err(error) => {
                let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                    &files,
                    &params,
                    &spawn_id,
                    "failed",
                    "agent spawn PowerShell host preflight failed before launching a child process",
                    json!({
                        "reason": "agent_spawn_shell_preflight_failed",
                        "source_error_message": error.message.clone(),
                        "source_error_data": error.data,
                    }),
                );
                return Err(augment_agent_spawn_error_with_artifacts(
                    error,
                    &files,
                    &params,
                    &spawn_id,
                    "agent_spawn_shell_preflight_failed",
                    None,
                    completion_artifacts,
                ));
            }
        };

        let mut env = BTreeMap::new();
        env.insert("SYNAPSE_BEARER_TOKEN".to_owned(), token);
        env.insert("SYNAPSE_AGENT_SPAWN_ID".to_owned(), spawn_id.clone());
        env.insert(
            "SYNAPSE_AGENT_KIND".to_owned(),
            agent_kind.as_str().to_owned(),
        );
        if let Some(model_ref) = params.local_model_ref() {
            env.insert("SYNAPSE_AGENT_MODEL_REF".to_owned(), model_ref.to_owned());
        }
        env.insert(
            "SYNAPSE_AGENT_SPAWN_LAUNCH_TARGET".to_owned(),
            launch_host.target.clone(),
        );
        env.insert(
            "SYNAPSE_AGENT_SPAWN_LAUNCH_SOURCE".to_owned(),
            launch_host.source.clone(),
        );
        env.insert("SYNAPSE_MCP_URL".to_owned(), params.mcp_url.clone());
        env.insert("PYTHONUTF8".to_owned(), "1".to_owned());
        env.insert("PYTHONIOENCODING".to_owned(), "utf-8".to_owned());
        env.insert("PYTHONUNBUFFERED".to_owned(), "1".to_owned());

        // Forward the resolved local-model API key into the spawned agent's
        // environment. act_launch clears the child environment (env_clear) and
        // only re-applies a curated allow-list plus this map, so a remote
        // API-backed model (e.g. DeepSeek over https) would otherwise reach its
        // endpoint with no credentials and fail with HTTP 401 mid-run. Resolve
        // it from the daemon environment now and refuse the spawn loudly if the
        // row declares a key the daemon does not have, so the failure surfaces
        // at the spawn boundary with a remediation hint rather than after a
        // launched agent has already started burning a turn. Only the value is
        // forwarded into the child process env (never persisted): the process
        // history row records env keys only (m4::launch_process_history_row).
        if let Some((env_var, value)) =
            resolve_spawn_local_model_api_key(&self.m3_storage()?, local_model_row.as_ref())?
        {
            env.insert(env_var, value);
        }

        let launch_params = ActLaunchParams {
            target: launch_host.target.clone(),
            args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-Command".to_owned(),
                script,
            ],
            working_dir: Some(working_dir.display().to_string()),
            env,
            wait_for_window_title_regex: None,
            timeout_ms: 10_000,
            idempotency_key: None,
            cdp_debug: Some(false),
            force_renderer_accessibility: None,
            windows_console_window_state: Some(LaunchWindowState::Hidden),
            desktop: None,
        };

        timing.mark_prelaunch_done();
        in_flight.ensure("immediately_before_physical_agent_launch")?;
        timing.mark_launch_started();
        let launch_response = match launch_agent_spawn_with_terminal_capture(
            &self.m4_config,
            &launch_params,
            &files,
            &spawn_id,
            launched_at_unix_ms,
        ) {
            Ok(response) => response,
            Err(error) => {
                let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                    &files,
                    &params,
                    &spawn_id,
                    "failed",
                    "agent spawn PowerShell host launch failed before MCP session registration",
                    json!({
                        "reason": "agent_spawn_shell_launch_failed",
                        "launch_host": launch_host.to_json(),
                        "source_error_message": error.message.clone(),
                        "source_error_data": error.data,
                        "spawn_timing": timing.readback(in_flight, params.wait_timeout_ms),
                    }),
                );
                return Err(augment_agent_spawn_error_with_artifacts(
                    error,
                    &files,
                    &params,
                    &spawn_id,
                    "agent_spawn_shell_launch_failed",
                    Some(&launch_host.target),
                    completion_artifacts,
                ));
            }
        };
        timing.mark_launch_completed();
        if let Err(error) = in_flight.ensure("immediately_after_physical_agent_launch") {
            return Err(self
                .cleanup_launched_agent_after_operator_panic(
                    launch_response.pid,
                    None,
                    &files,
                    &params,
                    &spawn_id,
                    "immediately_after_physical_agent_launch",
                    error,
                )
                .await);
        }
        let process_job = match assign_owned_process_job(
            launch_response.pid,
            ACT_SPAWN_AGENT,
            Some(&spawn_id),
        ) {
            Ok(process_job) => process_job,
            Err(error) => {
                let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
                let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                    &files,
                    &params,
                    &spawn_id,
                    "failed",
                    "process job assignment failed before spawned agent completion",
                    json!({
                        "reason": "process_job_assign_failed",
                        "source_error": error.message,
                        "cleanup": cleanup,
                    }),
                );
                return Err(agent_spawn_tool_error(
                    error_codes::ACTION_AGENT_SPAWN_FAILED,
                    "act_spawn_agent spawned the wrapper but failed to assign a session process job; exact spawned PID cleanup was attempted",
                    json!({
                        "code": error_codes::ACTION_AGENT_SPAWN_FAILED,
                        "reason": "process_job_assign_failed",
                        "spawn_id": spawn_id,
                        "cli": agent_kind.as_str(),
                        "launcher_process_id": launch_response.pid,
                        "log_dir": files.log_dir.display().to_string(),
                        "source_error": error.message,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };
        if let Err(error) = record_launch_process_history(self, &launch_params, &launch_response) {
            let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
            let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                &files,
                &params,
                &spawn_id,
                "failed",
                "process history recording failed before spawned agent completion",
                json!({
                    "reason": "process_history_record_failed",
                    "source_error": error.message,
                    "cleanup": cleanup,
                }),
            );
            return Err(agent_spawn_tool_error(
                error_codes::ACTION_AGENT_SPAWN_FAILED,
                "act_spawn_agent spawned the wrapper but failed to record process history; exact spawned PID cleanup was attempted",
                json!({
                    "code": error_codes::ACTION_AGENT_SPAWN_FAILED,
                    "reason": "process_history_record_failed",
                    "spawn_id": spawn_id,
                    "cli": agent_kind.as_str(),
                    "launcher_process_id": launch_response.pid,
                    "log_dir": files.log_dir.display().to_string(),
                    "source_error": error.message,
                    "cleanup": cleanup,
                    "completion_artifacts": completion_artifacts,
                }),
            ));
        }

        timing.mark_session_wait_started();
        let session_wait_deadline =
            agent_spawn_wait_deadline_from(Instant::now(), params.wait_timeout_ms)?;
        let mut matched = match await_agent_spawn_phase_under_operator_panic_guard(
            in_flight,
            "while_waiting_for_spawned_agent_session",
            self.wait_for_spawned_agent_session(
                &params,
                agent_kind,
                &spawn_id,
                &before_session_ids,
                launched_at_unix_ms,
                launch_response.pid,
                &files,
                session_wait_deadline,
            ),
            |error| json!({ "operator_panic_error": error }),
        )
        .await
        {
            Ok(matched) => matched,
            Err(error) => {
                let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
                let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                    &files,
                    &params,
                    &spawn_id,
                    "session_timeout",
                    "spawned agent did not register a ready MCP session before wait_timeout_ms",
                    json!({
                        "reason": "session_registry_readback_timeout",
                        "wait_timeout_ms": params.wait_timeout_ms,
                        "wait_error": error,
                        "spawn_timing": timing.readback(in_flight, params.wait_timeout_ms),
                        "cleanup": cleanup,
                    }),
                );
                return Err(agent_spawn_tool_error(
                    error_codes::ACTION_AGENT_SPAWN_SESSION_TIMEOUT,
                    "act_spawn_agent did not observe a fully provisioned MCP session before timeout; exact spawned PID cleanup was attempted",
                    json!({
                        "code": error_codes::ACTION_AGENT_SPAWN_SESSION_TIMEOUT,
                        "reason": "session_registry_readback_timeout",
                        "spawn_id": spawn_id,
                        "cli": agent_kind.as_str(),
                        "launcher_process_id": launch_response.pid,
                        "mcp_url": params.mcp_url,
                        "wait_timeout_ms": params.wait_timeout_ms,
                        "target": params.target,
                        "log_dir": files.log_dir.display().to_string(),
                        "readiness_files": agent_spawn_readiness_file_readback(&files),
                        "stdout_tail": tail_file_lossy(&files.stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                        "stderr_tail": tail_file_lossy(&files.stderr_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                        "final_message_tail": tail_file_lossy(&files.final_message_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                        "spawn_timing": timing.readback(in_flight, params.wait_timeout_ms),
                        "wait_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };
        timing.mark_session_matched();
        if let Err(error) = in_flight.ensure("after_spawned_session_wait") {
            return Err(self
                .cleanup_launched_agent_after_operator_panic(
                    launch_response.pid,
                    Some(&matched.session_id),
                    &files,
                    &params,
                    &spawn_id,
                    "after_spawned_session_wait",
                    error,
                )
                .await);
        }

        timing.mark_task_wait_started();
        let task_wait_deadline =
            agent_spawn_wait_deadline_from(Instant::now(), params.wait_timeout_ms)?;
        let task_started = match await_agent_spawn_phase_under_operator_panic_guard(
            in_flight,
            "while_waiting_for_spawned_agent_task_start",
            self.wait_for_spawned_agent_task_started(
                &params,
                agent_kind,
                &spawn_id,
                &mut matched,
                &before_session_ids,
                launched_at_unix_ms,
                launch_response.pid,
                &files,
                task_wait_deadline,
            ),
            |error| json!({ "operator_panic_error": error }),
        )
        .await
        {
            Ok(task_started) => task_started,
            Err(error) => {
                let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
                let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                    &files,
                    &params,
                    &spawn_id,
                    "task_not_started",
                    "spawned agent registered an MCP session but did not write a valid task-start readiness artifact before wait_timeout_ms",
                    json!({
                        "reason": "task_start_readiness_readback_failed",
                        "wait_timeout_ms": params.wait_timeout_ms,
                        "task_start_error": error,
                        "session_id": matched.session_id,
                        "spawn_timing": timing.readback(in_flight, params.wait_timeout_ms),
                        "cleanup": cleanup,
                    }),
                );
                return Err(agent_spawn_tool_error(
                    error_codes::ACTION_AGENT_SPAWN_TASK_NOT_STARTED,
                    "act_spawn_agent observed the spawned session but did not observe task-start readiness before timeout; exact spawned PID cleanup was attempted",
                    json!({
                        "code": error_codes::ACTION_AGENT_SPAWN_TASK_NOT_STARTED,
                        "reason": "task_start_readiness_readback_failed",
                        "spawn_id": spawn_id,
                        "cli": agent_kind.as_str(),
                        "launcher_process_id": launch_response.pid,
                        "agent_process_id": matched.agent_process_id,
                        "session_id": matched.session_id,
                        "mcp_url": params.mcp_url,
                        "wait_timeout_ms": params.wait_timeout_ms,
                        "target": params.target,
                        "log_dir": files.log_dir.display().to_string(),
                        "task_started_path": files.task_started_path.display().to_string(),
                        "readiness_files": agent_spawn_readiness_file_readback(&files),
                        "stdout_tail": tail_file_lossy(&files.stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                        "stderr_tail": tail_file_lossy(&files.stderr_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                        "final_message_tail": tail_file_lossy(&files.final_message_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                        "spawn_timing": timing.readback(in_flight, params.wait_timeout_ms),
                        "task_start_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };
        timing.mark_task_started();
        if let Err(error) = in_flight.ensure("after_spawned_task_start_wait") {
            return Err(self
                .cleanup_launched_agent_after_operator_panic(
                    launch_response.pid,
                    Some(&matched.session_id),
                    &files,
                    &params,
                    &spawn_id,
                    "after_spawned_task_start_wait",
                    error,
                )
                .await);
        }
        if let Err(error) =
            self.require_spawned_agent_session_live(&matched.session_id, &files, agent_kind)
        {
            let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
            let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                &files,
                &params,
                &spawn_id,
                "task_not_started",
                "spawned agent session ended before durable spawn readiness could be returned",
                json!({
                    "reason": "spawned_session_ended_before_ready",
                    "wait_timeout_ms": params.wait_timeout_ms,
                    "session_id": matched.session_id,
                    "session_liveness_error": error,
                    "cleanup": cleanup,
                }),
            );
            return Err(agent_spawn_tool_error(
                error_codes::ACTION_AGENT_SPAWN_TASK_NOT_STARTED,
                "act_spawn_agent observed task-start readiness but the spawned MCP session was no longer live before ready could be returned; exact spawned PID cleanup was attempted",
                json!({
                    "code": error_codes::ACTION_AGENT_SPAWN_TASK_NOT_STARTED,
                    "reason": "spawned_session_ended_before_ready",
                    "spawn_id": spawn_id,
                    "cli": agent_kind.as_str(),
                    "launcher_process_id": launch_response.pid,
                    "agent_process_id": matched.agent_process_id,
                    "session_id": matched.session_id,
                    "mcp_url": params.mcp_url,
                    "wait_timeout_ms": params.wait_timeout_ms,
                    "target": params.target,
                    "log_dir": files.log_dir.display().to_string(),
                    "task_started_path": files.task_started_path.display().to_string(),
                    "readiness_files": agent_spawn_readiness_file_readback(&files),
                    "session_liveness_error": error,
                    "cleanup": cleanup,
                    "completion_artifacts": completion_artifacts,
                }),
            ));
        }
        let control = match read_spawned_agent_control_artifact(&files, agent_kind) {
            Ok(control) => control,
            Err(error) => {
                let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
                let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                    &files,
                    &params,
                    &spawn_id,
                    "failed",
                    "spawned agent wrote task-start readiness but the interrupt-control artifact was missing or invalid",
                    json!({
                        "reason": "spawned_agent_control_readback_failed",
                        "control_error": error,
                        "session_id": matched.session_id,
                        "cleanup": cleanup,
                    }),
                );
                return Err(agent_spawn_tool_error(
                    error_codes::ACTION_AGENT_SPAWN_FAILED,
                    "act_spawn_agent observed task-start readiness but failed to read Codex interrupt-control metadata; exact spawned PID cleanup was attempted",
                    json!({
                        "code": error_codes::ACTION_AGENT_SPAWN_FAILED,
                        "reason": "spawned_agent_control_readback_failed",
                        "spawn_id": spawn_id,
                        "cli": agent_kind.as_str(),
                        "launcher_process_id": launch_response.pid,
                        "agent_process_id": matched.agent_process_id,
                        "session_id": matched.session_id,
                        "log_dir": files.log_dir.display().to_string(),
                        "control_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };

        let metadata = SpawnedAgentRead {
            spawn_id: spawn_id.clone(),
            cli: agent_kind.as_str().to_owned(),
            launcher_process_id: launch_response.pid,
            agent_process_id: matched.agent_process_id,
            started_by_session_id,
            launched_at_unix_ms,
            launch_target: launch_params.target.clone(),
            log_dir: files.log_dir.display().to_string(),
            template_id: params.template_id.clone(),
            template_version: params.template_version,
            control,
        };
        if let Err(error) = self.record_spawned_agent_metadata(&matched.session_id, metadata) {
            let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
            let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                &files,
                &params,
                &spawn_id,
                "failed",
                "spawned agent metadata recording failed before spawned agent completion",
                json!({
                    "reason": "spawned_agent_metadata_record_failed",
                    "source_error": error.message,
                    "session_id": matched.session_id,
                    "cleanup": cleanup,
                }),
            );
            return Err(agent_spawn_tool_error(
                error_codes::ACTION_AGENT_SPAWN_FAILED,
                "act_spawn_agent observed the spawned session but failed to record session metadata; exact spawned PID cleanup was attempted",
                json!({
                    "code": error_codes::ACTION_AGENT_SPAWN_FAILED,
                    "reason": "spawned_agent_metadata_record_failed",
                    "spawn_id": spawn_id,
                    "cli": agent_kind.as_str(),
                    "launcher_process_id": launch_response.pid,
                    "agent_process_id": matched.agent_process_id,
                    "session_id": matched.session_id,
                    "log_dir": files.log_dir.display().to_string(),
                    "source_error": error.message,
                    "cleanup": cleanup,
                    "completion_artifacts": completion_artifacts,
                }),
            ));
        }
        if let Err(error) = self.register_session_process_resource(
            super::session_lifecycle::SessionProcessResource::new(
                matched.session_id.clone(),
                ACT_SPAWN_AGENT,
                launch_response.pid,
                Some(spawn_id.clone()),
                launch_params.target.clone(),
                process_job,
            )
            .with_agent_cli(agent_kind.as_str()),
        ) {
            let cleanup = crate::m4::terminate_owned_process_tree(launch_response.pid);
            let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
                &files,
                &params,
                &spawn_id,
                "failed",
                "session process resource registration failed before spawned agent completion",
                json!({
                    "reason": "session_process_register_failed",
                    "source_error": error.message,
                    "session_id": matched.session_id,
                    "cleanup": cleanup,
                }),
            );
            return Err(agent_spawn_tool_error(
                error_codes::ACTION_AGENT_SPAWN_FAILED,
                "act_spawn_agent observed the spawned session but failed to register the session process resource; exact spawned PID cleanup was attempted",
                json!({
                    "code": error_codes::ACTION_AGENT_SPAWN_FAILED,
                    "reason": "session_process_register_failed",
                    "spawn_id": spawn_id,
                    "cli": agent_kind.as_str(),
                    "launcher_process_id": launch_response.pid,
                    "agent_process_id": matched.agent_process_id,
                    "session_id": matched.session_id,
                    "log_dir": files.log_dir.display().to_string(),
                    "source_error": error.message,
                    "cleanup": cleanup,
                    "completion_artifacts": completion_artifacts,
                }),
            ));
        }

        let response = ActSpawnAgentResponse {
            spawn_id,
            cli: agent_kind,
            kind: agent_kind,
            model_ref: local_model_row
                .as_ref()
                .map(|row| row.name.clone())
                .or_else(|| params.local_model_ref().map(ToOwned::to_owned)),
            launcher_process_id: launch_response.pid,
            agent_process_id: matched.agent_process_id,
            session_id: matched.session_id,
            mcp_url: params.mcp_url,
            working_dir: working_dir.display().to_string(),
            launch_target: launch_params.target,
            launch_target_source: launch_host.source,
            launched_at_unix_ms,
            registered_at_unix_ms: matched.registered_at_unix_ms,
            task_started_at_unix_ms: task_started.started_at_unix_ms,
            task_readiness_source: task_started.readiness_source.to_owned(),
            target: params.target,
            template_id: params.template_id,
            template_version: params.template_version,
            template_config_hash: params.template_config_hash,
            log_paths: files.to_response(),
        };
        if let Err(error) = in_flight.ensure("before_spawn_ready_response") {
            let cleanup = self
                .cleanup_spawn_response_after_operator_panic(
                    &response,
                    "before_spawn_ready_response",
                )
                .await;
            return Err(agent_spawn_operator_panic_cleanup_error(
                error,
                "before_spawn_ready_response",
                cleanup,
            ));
        }
        Ok(response)
    }

    async fn cleanup_launched_agent_after_operator_panic(
        &self,
        launcher_process_id: u32,
        session_id: Option<&str>,
        files: &AgentSpawnFiles,
        params: &ActSpawnAgentParams,
        spawn_id: &str,
        stage: &'static str,
        error: ErrorData,
    ) -> ErrorData {
        let cleanup = self
            .cleanup_agent_spawn_process_and_session_after_operator_panic(
                launcher_process_id,
                session_id,
                spawn_id,
                stage,
            )
            .await;
        let completion_artifacts = write_agent_spawn_daemon_terminal_artifacts(
            files,
            params,
            spawn_id,
            "operator_panic_cancelled",
            "physical operator panic superseded the spawn after its process launch; exact owned cleanup was attempted",
            json!({
                "reason": "operator_panic_after_agent_launch",
                "stage": stage,
                "cleanup": &cleanup,
            }),
        );
        agent_spawn_operator_panic_cleanup_error(
            error,
            stage,
            json!({
                "cleanup": cleanup,
                "completion_artifacts": completion_artifacts,
            }),
        )
    }

    pub(crate) async fn cleanup_spawn_response_after_operator_panic(
        &self,
        response: &ActSpawnAgentResponse,
        stage: &'static str,
    ) -> Value {
        self.cleanup_agent_spawn_process_and_session_after_operator_panic(
            response.launcher_process_id,
            Some(&response.session_id),
            &response.spawn_id,
            stage,
        )
        .await
    }

    async fn cleanup_agent_spawn_process_and_session_after_operator_panic(
        &self,
        launcher_process_id: u32,
        session_id: Option<&str>,
        spawn_id: &str,
        stage: &'static str,
    ) -> Value {
        let process_cleanup = crate::m4::terminate_owned_process_tree(launcher_process_id);
        let (session_teardown, session_teardown_error) = match session_id {
            Some(session_id) => match self.session_lifecycle_state() {
                Ok(lifecycle) => match lifecycle
                    .teardown_session_with_options_report(
                        session_id,
                        "operator_panic_agent_spawn_cleanup",
                        super::session_lifecycle::SessionTeardownOptions::explicit_kill(),
                    )
                    .await
                {
                    Ok(report) => (Some(report), None),
                    Err(error) => (None, Some(error.message.to_string())),
                },
                Err(error) => (None, Some(error.message.to_string())),
            },
            None => (None, None),
        };
        let cleanup_verified = agent_spawn_operator_panic_cleanup_verified(
            &process_cleanup.remaining_process_ids,
            session_teardown_error.as_deref(),
            session_teardown.as_ref().map(|report| report.failure_count),
        );
        if cleanup_verified {
            tracing::warn!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                detail_code = "AGENT_SPAWN_OPERATOR_PANIC_CLEANUP_VERIFIED",
                spawn_id,
                launcher_process_id,
                session_id,
                stage,
                "operator panic raced a spawned agent launch; exact process/session cleanup was verified"
            );
        } else {
            AGENT_SPAWN_CLEANUP_INCIDENT.store(true, Ordering::SeqCst);
            let drain = self
                .drain_state_handle()
                .mark_draining("operator_panic_agent_spawn_cleanup_unverified");
            tracing::error!(
                code = error_codes::ACTION_POSTCONDITION_FAILED,
                detail_code = "AGENT_SPAWN_OPERATOR_PANIC_CLEANUP_UNVERIFIED",
                spawn_id,
                launcher_process_id,
                session_id,
                stage,
                process_cleanup = ?process_cleanup,
                session_teardown = ?session_teardown,
                session_teardown_error = ?session_teardown_error,
                drain = ?drain,
                "operator panic raced a spawned agent launch and exact cleanup did not reach a terminal postcondition"
            );
        }
        json!({
            "source_of_truth": "owned launcher process tree + session lifecycle physical cleanup readback",
            "spawn_id": spawn_id,
            "launcher_process_id": launcher_process_id,
            "session_id": session_id,
            "stage": stage,
            "process_cleanup": process_cleanup,
            "session_teardown": session_teardown,
            "session_teardown_error": session_teardown_error,
            "cleanup_verified": cleanup_verified,
        })
    }

    fn current_session_ids(&self) -> Result<BTreeSet<String>, ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while reading pre-spawn session ids",
            )
        })?;
        Ok(guard
            .reads(unix_time_ms_now())
            .into_iter()
            .map(|read| read.session_id)
            .collect())
    }

    fn spawn_session_candidates_for_readiness(
        &self,
        include_targets: bool,
    ) -> Result<Vec<SpawnSessionCandidateRead>, ErrorData> {
        let registry_reads = {
            let guard = self.session_registry_ref().lock().map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "session registry lock poisoned while reading spawn readiness candidates",
                )
            })?;
            guard.reads(unix_time_ms_now())
        };
        let mut candidates = Vec::with_capacity(registry_reads.len());
        for registry in registry_reads {
            let active_target = if include_targets {
                self.agent_logical_foreground_read_model(&registry.session_id)?
                    .map(|target| spawn_target_wire_from_session_target(&target))
            } else {
                None
            };
            candidates.push(SpawnSessionCandidateRead {
                registry,
                active_target,
            });
        }
        Ok(candidates)
    }

    async fn wait_for_spawned_agent_session(
        &self,
        params: &ActSpawnAgentParams,
        agent_kind: ActSpawnAgentCli,
        spawn_id: &str,
        before_session_ids: &BTreeSet<String>,
        launched_at_unix_ms: u64,
        launcher_pid: u32,
        files: &AgentSpawnFiles,
        deadline: Instant,
    ) -> Result<MatchedSpawnSession, serde_json::Value> {
        let mut last_observed = json!({
            "reason": "no_matching_session_observed",
            "sessions": [],
        });
        while !agent_spawn_deadline_remaining(deadline).is_zero() {
            let candidates = self
                .spawn_session_candidates_for_readiness(params.target.is_some())
                .map_err(|error| {
                    json!({
                        "reason": "spawn_readiness_candidate_read_failed",
                        "error": error.message,
                        "data": error.data,
                    })
                })?;
            let mut matched_session = None;
            let session_count = candidates.len();
            let mut sessions_json = Vec::new();
            let mut readiness_reason_counts: BTreeMap<String, u64> = BTreeMap::new();
            let mut candidate_readiness = Vec::new();
            let explicit_task_session_id = task_start_session_id_for_spawn(files, spawn_id);
            let mut explicit_task_session_match = None;
            let mut ready_candidates = Vec::new();
            // Sessions that are a new + in-window + CLI match for this spawn but
            // have not (yet) issued a daemon MCP tool call. They are identified
            // as ours; we bind one only with independent proof of task progress.
            let mut lenient_candidates: Vec<String> = Vec::new();
            for candidate in &candidates {
                let readiness = spawn_session_candidate_readiness_from_read(
                    &candidate.registry,
                    candidate.active_target.as_ref(),
                    agent_kind,
                    params.target.as_ref(),
                    before_session_ids,
                    launched_at_unix_ms,
                );
                let reason = readiness
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                *readiness_reason_counts.entry(reason.clone()).or_default() += 1;
                if reason == "tool_call_not_observed" {
                    lenient_candidates.push(candidate.registry.session_id.clone());
                }
                if explicit_task_session_id.as_deref()
                    == Some(candidate.registry.session_id.as_str())
                    && spawn_session_identity_matches_from_read(
                        &candidate.registry,
                        agent_kind,
                        before_session_ids,
                        launched_at_unix_ms,
                    )
                {
                    explicit_task_session_match = Some(MatchedSpawnSession {
                        session_id: candidate.registry.session_id.clone(),
                        registered_at_unix_ms: unix_time_ms_now(),
                        agent_process_id: discover_agent_process_id(launcher_pid, agent_kind),
                    });
                }
                if readiness.get("ready").and_then(Value::as_bool) == Some(true) {
                    ready_candidates.push(MatchedSpawnSession {
                        session_id: candidate.registry.session_id.clone(),
                        registered_at_unix_ms: unix_time_ms_now(),
                        agent_process_id: discover_agent_process_id(launcher_pid, agent_kind),
                    });
                }
                if reason != "session_existed_before_spawn"
                    && candidate_readiness.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT
                {
                    candidate_readiness.push(json!({
                        "session_id": candidate.registry.session_id,
                        "started_at_unix_ms": candidate.registry.started_at_unix_ms,
                        "last_action": candidate.registry.last_action,
                        "active_target": candidate.active_target,
                        "readiness": readiness.clone(),
                    }));
                }
                if sessions_json.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT {
                    sessions_json.push(spawn_session_observation_from_read(candidate, readiness));
                }
            }
            last_observed = json!({
                "reason": "candidate_not_ready",
                "session_count": session_count,
                "sessions_recorded": sessions_json.len(),
                "readiness_reason_counts": readiness_reason_counts,
                "candidate_readiness_recorded": candidate_readiness.len(),
                "candidate_readiness": candidate_readiness,
                "explicit_task_session_id": explicit_task_session_id.clone(),
                "ready_candidate_count": ready_candidates.len(),
                "sessions": sessions_json,
                "readiness_files": agent_spawn_readiness_file_readback(files),
                "read_model": "session_registry + per-session logical foreground target only; skips full session_list attached process/window scan",
            });

            if let Some(matched) = explicit_task_session_match {
                return Ok(matched);
            }
            if explicit_task_session_id.is_none() && ready_candidates.len() == 1 {
                matched_session = ready_candidates.pop();
            }
            if let Some(matched) = matched_session {
                return Ok(matched);
            }

            // Robust fallback for the agent-cooperative readiness protocol: a
            // session that registered, matches this spawn's CLI, and started in
            // the launch window is ours even if it never issued a daemon MCP
            // tool call (codex drives its own app-server tools; any agent may
            // skip the injected ceremony). Bind it only when the daemon
            // INDEPENDENTLY observes the task is underway, and only when the
            // candidate is unambiguous — fan_out disambiguates via the agent's
            // self-named session in the task-start artifact.
            if !lenient_candidates.is_empty()
                && agent_spawn_observed_task_progress(files, agent_kind).is_some()
            {
                let bind = if lenient_candidates.len() == 1 {
                    Some(lenient_candidates[0].clone())
                } else {
                    read_json_file_lossy(&files.task_started_path)
                        .and_then(|value| {
                            value
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .filter(|session_id| lenient_candidates.contains(session_id))
                };
                if let Some(session_id) = bind {
                    return Ok(MatchedSpawnSession {
                        session_id,
                        registered_at_unix_ms: unix_time_ms_now(),
                        agent_process_id: discover_agent_process_id(launcher_pid, agent_kind),
                    });
                }
            }

            if process_has_exited(launcher_pid) {
                return Err(json!({
                    "reason": "launcher_process_exited_before_registry_match",
                    "launcher_process_id": launcher_pid,
                    "readiness_files": agent_spawn_readiness_file_readback(files),
                    "stdout_tail": tail_file_lossy(&files.stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                    "stderr_tail": tail_file_lossy(&files.stderr_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                    "final_message_tail": tail_file_lossy(&files.final_message_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                    "last_observed": last_observed,
                }));
            }
            sleep_agent_spawn_poll(deadline).await;
        }
        Err(last_observed)
    }

    async fn wait_for_spawned_agent_task_started(
        &self,
        params: &ActSpawnAgentParams,
        agent_kind: ActSpawnAgentCli,
        spawn_id: &str,
        matched: &mut MatchedSpawnSession,
        before_session_ids: &BTreeSet<String>,
        launched_at_unix_ms: u64,
        launcher_pid: u32,
        files: &AgentSpawnFiles,
        deadline: Instant,
    ) -> Result<AgentSpawnTaskStartRead, serde_json::Value> {
        let mut last_observed = json!({
            "reason": "task_start_artifact_not_observed",
            "task_started_path": files.task_started_path.display().to_string(),
        });
        while !agent_spawn_deadline_remaining(deadline).is_zero() {
            if let Err(liveness_error) =
                self.require_spawned_agent_session_live(&matched.session_id, files, agent_kind)
            {
                match self.rebind_spawned_agent_session_for_task_start(
                    params,
                    agent_kind,
                    spawn_id,
                    before_session_ids,
                    launched_at_unix_ms,
                    launcher_pid,
                    files,
                    &liveness_error,
                )? {
                    Some(rebound) => {
                        *matched = rebound;
                    }
                    None => {
                        last_observed = json!({
                            "reason": "matched_session_not_live_waiting_for_replacement",
                            "matched_session_id": matched.session_id,
                            "task_started_path": files.task_started_path.display().to_string(),
                            "session_liveness_error": liveness_error,
                            "readiness_files": agent_spawn_readiness_file_readback(files),
                            "observed_task_progress": agent_spawn_observed_task_progress(files, agent_kind),
                        });
                        if process_has_exited(launcher_pid) {
                            return Err(json!({
                                "reason": "launcher_process_exited_after_matched_session_closed",
                                "launcher_process_id": launcher_pid,
                                "task_started_path": files.task_started_path.display().to_string(),
                                "completion_status": read_json_file_lossy(&files.completion_status_path),
                                "stdout_tail": tail_file_lossy(&files.stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                                "stderr_tail": tail_file_lossy(&files.stderr_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                                "final_message_tail": tail_file_lossy(&files.final_message_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                                "last_observed": last_observed,
                            }));
                        }
                        sleep_agent_spawn_poll(deadline).await;
                        continue;
                    }
                }
            }
            match read_agent_spawn_task_start_artifact(
                files, params, agent_kind, spawn_id, matched,
            )? {
                Some(read) => return Ok(read),
                None => {
                    // Observed work is diagnostic evidence, not the readiness
                    // verdict. #1225 requires durable session liveness plus the
                    // task-start artifact so a torn-down session cannot later be
                    // journaled as spawn_ready.
                    let observed_task_progress =
                        agent_spawn_observed_task_progress(files, agent_kind);
                    last_observed = json!({
                        "reason": "task_start_artifact_not_observed",
                        "task_started_path": files.task_started_path.display().to_string(),
                        "completion_status": read_json_file_lossy(&files.completion_status_path),
                        "observed_task_progress": observed_task_progress,
                        "stdout_bytes": file_len(&files.stdout_path),
                        "stderr_bytes": file_len(&files.stderr_path),
                        "final_message_bytes": file_len(&files.final_message_path),
                    });
                }
            }

            if process_has_exited(launcher_pid) {
                return Err(json!({
                    "reason": "launcher_process_exited_before_task_start",
                    "launcher_process_id": launcher_pid,
                    "task_started_path": files.task_started_path.display().to_string(),
                    "completion_status": read_json_file_lossy(&files.completion_status_path),
                    "stdout_tail": tail_file_lossy(&files.stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                    "stderr_tail": tail_file_lossy(&files.stderr_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                    "final_message_tail": tail_file_lossy(&files.final_message_path, AGENT_SPAWN_LOG_TAIL_BYTES),
                    "last_observed": last_observed,
                }));
            }
            sleep_agent_spawn_poll(deadline).await;
        }
        Err(json!({
            "reason": "task_start_artifact_timeout",
            "task_started_path": files.task_started_path.display().to_string(),
            "last_observed": last_observed,
        }))
    }

    fn rebind_spawned_agent_session_for_task_start(
        &self,
        params: &ActSpawnAgentParams,
        agent_kind: ActSpawnAgentCli,
        spawn_id: &str,
        before_session_ids: &BTreeSet<String>,
        launched_at_unix_ms: u64,
        launcher_pid: u32,
        files: &AgentSpawnFiles,
        liveness_error: &serde_json::Value,
    ) -> Result<Option<MatchedSpawnSession>, serde_json::Value> {
        let candidates = self
            .spawn_session_candidates_for_readiness(params.target.is_some())
            .map_err(|error| {
                json!({
                    "reason": "spawn_rebind_candidate_read_failed",
                    "error": error.message,
                    "data": error.data,
                    "session_liveness_error": liveness_error,
                })
            })?;
        let explicit_task_session_id = task_start_session_id_for_spawn(files, spawn_id);
        let observed_task_progress = agent_spawn_observed_task_progress(files, agent_kind);
        let mut ready_candidates = Vec::new();
        let mut lenient_candidates = Vec::new();
        let mut candidate_readiness = Vec::new();
        for candidate in &candidates {
            let readiness = spawn_session_candidate_readiness_from_read(
                &candidate.registry,
                candidate.active_target.as_ref(),
                agent_kind,
                params.target.as_ref(),
                before_session_ids,
                launched_at_unix_ms,
            );
            let reason = readiness
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if candidate_readiness.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT
                && reason != "session_existed_before_spawn"
            {
                candidate_readiness.push(json!({
                    "session_id": candidate.registry.session_id,
                    "started_at_unix_ms": candidate.registry.started_at_unix_ms,
                    "last_action": candidate.registry.last_action,
                    "active_target": candidate.active_target,
                    "readiness": readiness.clone(),
                }));
            }
            if explicit_task_session_id.as_deref() == Some(candidate.registry.session_id.as_str())
                && spawn_session_identity_matches_from_read(
                    &candidate.registry,
                    agent_kind,
                    before_session_ids,
                    launched_at_unix_ms,
                )
            {
                return Ok(Some(MatchedSpawnSession {
                    session_id: candidate.registry.session_id.clone(),
                    registered_at_unix_ms: unix_time_ms_now(),
                    agent_process_id: discover_agent_process_id(launcher_pid, agent_kind),
                }));
            }
            if readiness.get("ready").and_then(Value::as_bool) == Some(true) {
                ready_candidates.push(candidate.registry.session_id.clone());
            } else if reason == "tool_call_not_observed" && observed_task_progress.is_some() {
                lenient_candidates.push(candidate.registry.session_id.clone());
            }
        }

        let bind = if ready_candidates.len() == 1 {
            Some(ready_candidates[0].clone())
        } else if ready_candidates.is_empty() && lenient_candidates.len() == 1 {
            Some(lenient_candidates[0].clone())
        } else if ready_candidates.len() + lenient_candidates.len() > 1 {
            return Err(json!({
                "reason": "spawn_rebind_ambiguous",
                "explicit_task_session_id": explicit_task_session_id,
                "ready_candidate_count": ready_candidates.len(),
                "lenient_candidate_count": lenient_candidates.len(),
                "ready_candidates": ready_candidates,
                "lenient_candidates": lenient_candidates,
                "candidate_readiness": candidate_readiness,
                "session_liveness_error": liveness_error,
                "readiness_files": agent_spawn_readiness_file_readback(files),
                "observed_task_progress": observed_task_progress,
            }));
        } else {
            None
        };

        Ok(bind.map(|session_id| MatchedSpawnSession {
            session_id,
            registered_at_unix_ms: unix_time_ms_now(),
            agent_process_id: discover_agent_process_id(launcher_pid, agent_kind),
        }))
    }

    fn require_spawned_agent_session_live(
        &self,
        session_id: &str,
        files: &AgentSpawnFiles,
        agent_kind: ActSpawnAgentCli,
    ) -> Result<(), serde_json::Value> {
        let liveness = self.spawned_agent_session_liveness_readback(session_id)?;
        if liveness.get("lifecycle").and_then(Value::as_str) == Some("live") {
            return Ok(());
        }
        Err(json!({
            "reason": "spawned_session_not_live",
            "session_id": session_id,
            "session_liveness": liveness,
            "readiness_files": agent_spawn_readiness_file_readback(files),
            "observed_task_progress": agent_spawn_observed_task_progress(files, agent_kind),
        }))
    }

    fn spawned_agent_session_liveness_readback(
        &self,
        session_id: &str,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let reads = {
            let guard = self.session_registry_ref().lock().map_err(|_error| {
                json!({
                    "reason": "session_registry_lock_poisoned",
                    "session_id": session_id,
                })
            })?;
            guard.reads(unix_time_ms_now())
        };
        let Some(summary) = reads.iter().find(|read| read.session_id == session_id) else {
            return Ok(json!({
                "reason": "spawned_session_missing_from_registry",
                "session_id": session_id,
                "session_count": reads.len(),
            }));
        };
        Ok(json!({
            "session_id": summary.session_id,
            "lifecycle": summary.lifecycle,
            "closed_at_unix_ms": summary.closed_at_unix_ms,
            "last_seen_unix_ms": summary.last_seen_unix_ms,
            "last_seen_ms_ago": summary.last_seen_ms_ago,
            "last_action": summary.last_action,
            "last_reason_code": summary.last_reason_code,
        }))
    }

    fn record_spawned_agent_metadata(
        &self,
        session_id: &str,
        metadata: SpawnedAgentRead,
    ) -> Result<(), ErrorData> {
        let mut registry = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while recording spawned agent metadata",
            )
        })?;
        registry.record_spawned_agent(session_id, metadata, unix_time_ms_now());
        Ok(())
    }

    /// Journals `spawn_requested` (#897) before any spawn side effect; a
    /// spawn whose lifecycle cannot be journaled is refused up front.
    fn journal_spawn_requested(
        &self,
        spawn_id: &str,
        params: &ActSpawnAgentParams,
        started_by_session_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        let agent_kind = params.effective_cli()?;
        let log_dir = agent_spawn_root_dir()?.join(spawn_id);
        let db = self.m3_storage()?;
        let mut record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::SpawnRequested,
        );
        record.spawn_id = Some(spawn_id.to_owned());
        record.attributes.operation_name = Some(synapse_core::GenAiOperationName::CreateAgent);
        record.attributes.agent_name = Some(agent_kind.as_str().to_owned());
        record.attributes.provider_name =
            super::agent_events::provider_for_agent_kind(agent_kind.as_str());
        // Prompt CONTENT stays out of the journal (OTel GenAI opt-in rule);
        // its length is enough for the dashboard to size the request.
        record.payload = json!({
            "started_by_session_id": started_by_session_id,
            "prompt_chars": params.prompt.as_deref().map(|prompt| prompt.chars().count()),
            "working_dir": params.working_dir,
            "mcp_url": params.mcp_url,
            "wait_timeout_ms": params.wait_timeout_ms,
            "hold_open_ms": params.hold_open_ms,
            "kind": agent_kind.as_str(),
            "model_ref": params.local_model_ref(),
            "log_dir": log_dir.display().to_string(),
            // Spawn-template provenance (#909) so the journal records which
            // template version drove the run, not just the rendered params.
            "template_id": params.template_id.as_deref(),
            "template_version": params.template_version,
            "template_config_hash": params.template_config_hash.as_deref(),
        });
        super::agent_events::record_agent_event(&db, &record)
            .map(|_readback| ())
            .map_err(|error| {
                super::agent_events::agent_event_tool_error(ACT_SPAWN_AGENT, &error, false)
            })
    }

    /// Journals `spawn_ready` (#897) once the spawned agent has a registered
    /// MCP session and a validated task-start artifact.
    fn journal_spawn_ready(&self, response: &ActSpawnAgentResponse) -> Result<(), ErrorData> {
        let db = self.m3_storage()?;
        let mut record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::SpawnReady,
        );
        record.spawn_id = Some(response.spawn_id.clone());
        record.session_id = Some(response.session_id.clone());
        record.state_to = Some("live".to_owned());
        record.attributes.operation_name = Some(synapse_core::GenAiOperationName::InvokeAgent);
        record.attributes.agent_name = Some(response.cli.as_str().to_owned());
        record.attributes.provider_name =
            super::agent_events::provider_for_agent_kind(response.cli.as_str());
        record.attributes.conversation_id = Some(response.session_id.clone());
        record.payload = json!({
            "launcher_process_id": response.launcher_process_id,
            "agent_process_id": response.agent_process_id,
            "launched_at_unix_ms": response.launched_at_unix_ms,
            "registered_at_unix_ms": response.registered_at_unix_ms,
            "task_started_at_unix_ms": response.task_started_at_unix_ms,
            "launch_target": response.launch_target,
            "log_dir": response.log_paths.log_dir,
        });
        super::agent_events::record_agent_event(&db, &record)
            .map(|_readback| ())
            .map_err(|error| {
                super::agent_events::agent_event_tool_error(ACT_SPAWN_AGENT, &error, true)
            })
    }

    /// Journals a terminal `exited` event (durable flush) for a failed spawn
    /// (#897). The original spawn error always wins the tool response; a
    /// journal failure here is logged (AGENT_EVENT_WRITE_FAILED) not raised.
    fn journal_spawn_failed(&self, spawn_id: &str, error: &ErrorData) {
        let db = match self.m3_storage() {
            Ok(db) => db,
            Err(storage_error) => {
                tracing::error!(
                    code = "AGENT_EVENT_WRITE_FAILED",
                    spawn_id,
                    detail = %storage_error.message,
                    "spawn-failure agent event skipped: storage unavailable"
                );
                return;
            }
        };
        let reason = error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("agent_spawn_failed");
        let error_code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str);
        let mut record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::Exited,
        );
        record.spawn_id = Some(spawn_id.to_owned());
        record.reason_code = Some(reason.to_owned());
        record.end_state = Some(synapse_core::AgentEndState::Error);
        record.attributes.error_type = error_code.map(ToOwned::to_owned);
        record.payload = json!({
            "error_message": error.message.chars().take(512).collect::<String>(),
        });
        if let Err(journal_error) = super::agent_events::record_agent_event_durable(&db, &record) {
            tracing::error!(
                code = "AGENT_EVENT_WRITE_FAILED",
                spawn_id,
                reason,
                detail = %journal_error,
                "spawn-failure agent event could not be journaled"
            );
        }
    }

    async fn require_spawn_local_model_row(
        &self,
        params: &ActSpawnAgentParams,
        probe_by_session: &str,
    ) -> Result<LocalModelRegistryRow, ErrorData> {
        let model_ref = params.local_model_ref().ok_or_else(|| {
            local_model_spawn_refusal(
                error_codes::TOOL_PARAMS_INVALID,
                "local_model_model_ref_missing",
                "act_spawn_agent local_model requires model_ref",
                json!({
                    "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                }),
            )
        })?;
        let rows = self.local_model_registry_snapshot()?;
        let mut row = rows
            .into_iter()
            .find(|row| row.name == model_ref)
            .ok_or_else(|| {
                local_model_spawn_refusal(
                    error_codes::MODEL_REGISTRY_NOT_FOUND,
                    "local_model_registry_row_missing",
                    "act_spawn_agent local_model refused because the requested model_ref is not registered",
                    json!({
                        "model_ref": model_ref,
                        "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                    }),
                )
            })?;
        validate_spawn_local_model_static_requirements(model_ref, &row)?;
        let probe = row.last_probe.as_ref().ok_or_else(|| {
            local_model_spawn_refusal(
                error_codes::MODEL_REGISTRY_UNPROBED,
                "local_model_registry_row_unprobed",
                "act_spawn_agent local_model refused because the registry row has no probe evidence",
                json!({
                    "model_ref": model_ref,
                    "row_key": row.row_key.clone(),
                    "last_probe": null,
                    "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                }),
            )
        })?;
        if !probe.healthy {
            return Err(local_model_unhealthy_refusal(model_ref, &row));
        }
        let probe_age_ms = unix_time_ms_now().saturating_sub(probe.observed_at_unix_ms);
        if probe_age_ms > LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS {
            tracing::warn!(
                code = "LOCAL_MODEL_SPAWN_STALE_PROBE_REFRESH",
                model_ref,
                row_key = %row.row_key,
                probe_age_ms,
                max_probe_age_ms = LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS,
                probe_by_session,
                "act_spawn_agent refreshing stale local-model probe before launch"
            );
            let db = self.m3_storage()?;
            let refresh = crate::m3::local_models::probe_local_model(
                &db,
                &LocalModelProbeParams {
                    name: model_ref.to_owned(),
                    timeout_ms: None,
                },
                probe_by_session,
            )
            .await?;
            row = refresh.row;
            validate_spawn_local_model_static_requirements(model_ref, &row)?;
            let Some(refreshed_probe) = row.last_probe.as_ref() else {
                return Err(local_model_spawn_refusal(
                    error_codes::MODEL_REGISTRY_UNPROBED,
                    "local_model_registry_row_unprobed_after_refresh",
                    "act_spawn_agent local_model probe refresh did not persist probe evidence",
                    json!({
                        "model_ref": model_ref,
                        "row_key": row.row_key.clone(),
                        "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                    }),
                ));
            };
            if !refreshed_probe.healthy {
                return Err(local_model_unhealthy_refusal(model_ref, &row));
            }
            let refreshed_age_ms =
                unix_time_ms_now().saturating_sub(refreshed_probe.observed_at_unix_ms);
            if refreshed_age_ms > LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS {
                return Err(local_model_spawn_refusal(
                    error_codes::MODEL_REGISTRY_PROBE_STALE,
                    "local_model_registry_probe_stale_after_refresh",
                    "act_spawn_agent local_model refused because probe refresh did not produce fresh evidence",
                    json!({
                        "model_ref": model_ref,
                        "row_key": row.row_key.clone(),
                        "last_probe": row.last_probe.clone(),
                        "probe_age_ms": refreshed_age_ms,
                        "max_probe_age_ms": LOCAL_MODEL_SPAWN_MAX_PROBE_AGE_MS,
                        "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                    }),
                ));
            }
            tracing::info!(
                code = "LOCAL_MODEL_SPAWN_STALE_PROBE_REFRESHED",
                model_ref,
                row_key = %row.row_key,
                refreshed_age_ms,
                probe_by_session,
                "act_spawn_agent refreshed stale local-model probe and will launch with fresh evidence"
            );
        }
        Ok(row)
    }
}

#[derive(Clone, Debug)]
struct MatchedSpawnSession {
    session_id: String,
    registered_at_unix_ms: u64,
    agent_process_id: Option<u32>,
}

#[derive(Debug)]
struct AgentSpawnTaskStartRead {
    started_at_unix_ms: u64,
    /// How task-start readiness was proven. #1225 keeps this fail-closed:
    /// successful readiness requires the cooperative task-start artifact plus a
    /// live spawned MCP session at readback time.
    readiness_source: &'static str,
}

#[derive(Debug)]
struct AgentSpawnFiles {
    log_dir: PathBuf,
    prompt_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    final_message_path: PathBuf,
    completion_status_path: PathBuf,
    task_started_path: PathBuf,
    task_started_script_path: PathBuf,
    debug_path: Option<PathBuf>,
    mcp_config_path: Option<PathBuf>,
    /// Claude only: generated `--settings` file wiring the CLI's HTTP hooks
    /// to the daemon's `/agent-events` ingress (#899).
    hook_settings_path: Option<PathBuf>,
    /// Codex only: generated `notify` program POSTing turn-complete events
    /// to the same ingress (#899).
    notify_script_path: Option<PathBuf>,
    /// Codex only: generated app-server runner script for interruptible turns.
    codex_app_server_runner_path: Option<PathBuf>,
    /// Codex only: control artifact with endpoint/thread/turn ids.
    codex_app_server_control_path: Option<PathBuf>,
    /// Codex only: raw app-server JSON-RPC event stream.
    codex_app_server_events_path: Option<PathBuf>,
    codex_app_server_stdout_path: Option<PathBuf>,
    codex_app_server_stderr_path: Option<PathBuf>,
    /// Local-model only: marker file written by the #931 runner.
    local_model_runner_path: Option<PathBuf>,
}

impl AgentSpawnFiles {
    fn to_response(&self) -> ActSpawnAgentLogPaths {
        let terminal = agent_spawn_terminal_capture_artifacts(&self.log_dir);
        ActSpawnAgentLogPaths {
            log_dir: self.log_dir.display().to_string(),
            prompt_path: self.prompt_path.display().to_string(),
            stdout_path: self.stdout_path.display().to_string(),
            stderr_path: self.stderr_path.display().to_string(),
            final_message_path: self.final_message_path.display().to_string(),
            completion_status_path: self.completion_status_path.display().to_string(),
            task_started_path: self.task_started_path.display().to_string(),
            task_started_script_path: self.task_started_script_path.display().to_string(),
            terminal_asciicast_path: terminal.asciicast_path.display().to_string(),
            terminal_capture_status_path: terminal.status_path.display().to_string(),
            terminal_final_screen_path: terminal.final_screen_path.display().to_string(),
            terminal_input_audit_path: terminal.input_audit_path.display().to_string(),
            debug_path: self
                .debug_path
                .as_ref()
                .map(|path| path.display().to_string()),
            mcp_config_path: self
                .mcp_config_path
                .as_ref()
                .map(|path| path.display().to_string()),
            hook_settings_path: self
                .hook_settings_path
                .as_ref()
                .map(|path| path.display().to_string()),
            notify_script_path: self
                .notify_script_path
                .as_ref()
                .map(|path| path.display().to_string()),
            codex_app_server_runner_path: self
                .codex_app_server_runner_path
                .as_ref()
                .map(|path| path.display().to_string()),
            codex_app_server_control_path: self
                .codex_app_server_control_path
                .as_ref()
                .map(|path| path.display().to_string()),
            codex_app_server_events_path: self
                .codex_app_server_events_path
                .as_ref()
                .map(|path| path.display().to_string()),
            codex_app_server_stdout_path: self
                .codex_app_server_stdout_path
                .as_ref()
                .map(|path| path.display().to_string()),
            codex_app_server_stderr_path: self
                .codex_app_server_stderr_path
                .as_ref()
                .map(|path| path.display().to_string()),
            local_model_runner_path: self
                .local_model_runner_path
                .as_ref()
                .map(|path| path.display().to_string()),
        }
    }
}

#[derive(Clone, Debug)]
struct AgentSpawnLaunchHost {
    target: String,
    source: String,
    attempted: Vec<String>,
}

impl AgentSpawnLaunchHost {
    fn to_json(&self) -> Value {
        json!({
        "target": self.target,
        "source": self.source,
        "attempted": self.attempted,
        "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
        })
    }
}

#[derive(Debug, Default)]
struct AgentSpawnOrphanRecoveryReport {
    scanned_count: usize,
    recovered_count: usize,
    skipped_terminal_count: usize,
    skipped_live_count: usize,
    skipped_fresh_count: usize,
    recovered_spawn_ids: Vec<String>,
}

#[derive(Debug)]
enum AgentSpawnOrphanRecoveryDecision {
    SkipTerminal,
    SkipLive,
    SkipFresh,
    Recover(AgentSpawnOrphanRecovery),
}

#[derive(Debug)]
struct AgentSpawnOrphanRecovery {
    status: &'static str,
    reason: &'static str,
    cli: String,
    wrapper_process_id: Option<u32>,
    source_completion_status: Option<Value>,
    source_completion_status_error: Option<String>,
    status_age_ms: Option<u64>,
}

#[derive(Clone, Debug)]
struct SpawnSessionCandidateRead {
    registry: SessionRegistryRead,
    active_target: Option<TargetWire>,
}

async fn run_shell_with_idempotency(
    service: &SynapseService,
    params: ActRunShellParams,
    authorization: RunShellAuthorization,
    inline_await_limit_ms: u64,
    context: Option<&ShellExecutionContext>,
    preflight: &super::action_preflight::ActionPreflightReadback,
) -> Result<ActRunShellResponse, ErrorData> {
    validate_run_shell_execution_plan(&params, inline_await_limit_ms)?;
    let session_id = context.map(ShellExecutionContext::session_id);
    let boundary = |stage| ensure_m4_physical_mutation_boundary(preflight, stage);
    let Some(row_key) = run_shell_idempotency_row_key(&params, session_id)? else {
        return run_authorized_shell_with_boundary(
            params,
            &authorization,
            inline_await_limit_ms,
            context,
            &boundary,
        )
        .await;
    };

    let runtime = service.reflex_runtime()?;
    {
        let runtime = runtime.lock().map_err(|_error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned while checking act_run_shell idempotency",
            )
        })?;
        if let Some(existing) = runtime
            .storage_kv_row(&row_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
        {
            drop(runtime);
            return run_shell_idempotency_replay(&params, &existing, session_id);
        }
        let reservation =
            run_shell_idempotency_reservation_row(&params, &authorization, session_id)?;
        runtime
            .storage_put_kv_rows(vec![(row_key.clone(), reservation)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    }

    if let Err(error) = boundary("act_run_shell_before_idempotent_process_launch") {
        return Err(clear_failed_run_shell_idempotency_reservation(
            service,
            &runtime,
            &row_key,
            "act_run_shell_before_idempotent_process_launch",
            error,
        ));
    }
    let response = match run_authorized_shell_with_boundary(
        params.clone(),
        &authorization,
        inline_await_limit_ms,
        context,
        &boundary,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return Err(clear_failed_run_shell_idempotency_reservation(
                service,
                &runtime,
                &row_key,
                "act_run_shell_authorized_execution_failed",
                error,
            ));
        }
    };
    let completed =
        run_shell_idempotency_completed_row(&params, &authorization, &response, session_id)?;
    {
        let runtime = runtime.lock().map_err(|_error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned while recording act_run_shell idempotency",
            )
        })?;
        runtime
            .storage_put_kv_rows(vec![(row_key, completed)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    }
    Ok(response)
}

fn clear_failed_run_shell_idempotency_reservation(
    service: &SynapseService,
    runtime: &Arc<std::sync::Mutex<synapse_reflex::ReflexRuntime>>,
    row_key: &[u8],
    stage: &'static str,
    error: ErrorData,
) -> ErrorData {
    let mut delete_error = None;
    let mut readback_error = None;
    let mut row_exists_after = None;
    match runtime.lock() {
        Ok(runtime) => {
            if let Err(storage_error) =
                runtime.storage_delete_rows(cf::CF_KV, vec![row_key.to_vec()])
            {
                delete_error = Some(storage_error.to_string());
            }
            match runtime.storage_kv_row(row_key) {
                Ok(row) => row_exists_after = Some(row.is_some()),
                Err(storage_error) => readback_error = Some(storage_error.to_string()),
            }
        }
        Err(_error) => {
            delete_error = Some(
                "reflex runtime lock poisoned while deleting failed act_run_shell idempotency reservation"
                    .to_owned(),
            );
        }
    }
    let cleanup_verified =
        delete_error.is_none() && readback_error.is_none() && row_exists_after == Some(false);
    let drain = (!cleanup_verified).then(|| {
        service
            .drain_state_handle()
            .mark_draining("act_run_shell_idempotency_cleanup_unverified")
    });
    if !cleanup_verified {
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            detail_code = "ACT_RUN_SHELL_IDEMPOTENCY_RESERVATION_CLEANUP_UNVERIFIED",
            stage,
            row_key = %String::from_utf8_lossy(row_key),
            delete_error = ?delete_error,
            readback_error = ?readback_error,
            row_exists_after = ?row_exists_after,
            drain = ?drain,
            "failed act_run_shell left an idempotency reservation whose physical cleanup could not be verified"
        );
    }
    let mut data = match error.data {
        Some(Value::Object(data)) => data,
        Some(original_data) => {
            let mut data = Map::new();
            data.insert("original_data".to_owned(), original_data);
            data
        }
        None => Map::new(),
    };
    data.insert(
        "idempotency_reservation_cleanup".to_owned(),
        json!({
            "stage": stage,
            "row_key": String::from_utf8_lossy(row_key),
            "delete_error": delete_error,
            "readback_error": readback_error,
            "row_exists_after": row_exists_after,
            "cleanup_verified": cleanup_verified,
            "source_of_truth": "CF_KV exact idempotency row readback after synchronous delete",
            "drain": drain,
        }),
    );
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(Value::Object(data)),
    )
}
