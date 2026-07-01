use super::{
    ActComboParams, ActComboResponse, ActLaunchParams, ActLaunchResponse,
    ActRunShellCancelResponse, ActRunShellJobIdParams, ActRunShellParams, ActRunShellResponse,
    ActRunShellStartParams, ActRunShellStartResponse, ActRunShellStatusParams,
    ActRunShellStatusResponse, ActSpawnAgentCli, ActSpawnAgentLogPaths, ActSpawnAgentParams,
    ActSpawnAgentRequest, ActSpawnAgentResponse, ActSpawnAgentTarget, AgentSpawnTaskStartedParams,
    AgentSpawnTaskStartedResponse, ErrorData, Json, LaunchWindowState,
    MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS, Parameters, RunShellAuthorization, SessionTarget,
    ShellExecutionContext, SynapseService, TargetWire, assign_owned_process_job,
    authorize_run_shell, authorize_run_shell_start, cancel_shell_job, execute_combo,
    launch_for_session, launch_process_history_row, launch_process_history_row_key,
    launch_request_details, mcp_error, prepare_run_shell_params_for_context,
    prepare_run_shell_start_params_for_context, required_combo_permissions, run_authorized_shell,
    run_shell_idempotency_completed_row, run_shell_idempotency_replay,
    run_shell_idempotency_reservation_row, run_shell_idempotency_row_key,
    run_shell_request_details, run_shell_start_request_details,
    shell_execution_context_for_session, shell_job_status, start_authorized_shell_job, tool,
    tool_router, validate_agent_spawn_params, validate_run_shell_execution_plan,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

use rmcp::{RoleServer, model::ErrorCode, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
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
const CODEX_APP_SERVER_RUNNER_SCRIPT: &str = include_str!("codex_app_server_runner.ps1");
const SHELL_FACADE_SOURCE_OF_TRUTH: &str = "%LOCALAPPDATA%\\Synapse\\shell-jobs + %LOCALAPPDATA%\\Synapse\\shell-sessions + daemon-tool-events.jsonl";
const PROCESS_FACADE_SOURCE_OF_TRUTH: &str = "live OS process table + CF_PROCESS_HISTORY";
const PROCESS_LIST_DEFAULT_LIMIT: usize = 100;
const PROCESS_LIST_MAX_LIMIT: usize = 1000;
const PROCESS_HISTORY_DEFAULT_LIMIT: usize = 20;
const PROCESS_HISTORY_MAX_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellOperation {
    #[default]
    Run,
    Start,
    Status,
    Cancel,
}

impl ShellOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Start => "start",
            Self::Status => "status",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellParams {
    #[serde(default)]
    #[schemars(description = "Shell facade operation. Defaults to run.")]
    pub operation: ShellOperation,
    #[serde(default)]
    #[schemars(description = "Executable path/name only for run/start operations.")]
    pub command: Option<String>,
    #[serde(default)]
    #[schemars(default, description = "Literal executable arguments for run/start.")]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub execution_mode: Option<ActRunShellExecutionMode>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub durable_timeout_ms: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 128))]
    pub job_id: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 0, max = 1048576))]
    pub tail_bytes: Option<u64>,
}

fn shell_input_schema() -> Arc<Map<String, Value>> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "operation": {
                "type": "string",
                "enum": ["run", "start", "status", "cancel"],
                "default": "run",
                "description": "Shell facade operation. Omit only for the default run operation."
            },
            "command": {
                "type": ["string", "null"],
                "description": "Executable path/name only. Accepted by run/start."
            },
            "args": {
                "type": ["array", "null"],
                "items": { "type": "string" },
                "description": "Literal executable arguments. Accepted by run/start."
            },
            "working_dir": {
                "type": ["string", "null"],
                "description": "Working directory for run/start."
            },
            "env": {
                "type": ["object", "null"],
                "additionalProperties": { "type": "string" },
                "description": "Extra environment variables for run/start."
            },
            "timeout_ms": {
                "type": ["integer", "null"],
                "minimum": 1,
                "description": "run: caller inline wait budget. start: durable job lifetime cap. Omit start timeout for an unbounded durable job."
            },
            "execution_mode": {
                "type": ["string", "null"],
                "enum": ["auto", "inline", "durable", null],
                "description": "run only. Controls inline vs durable/background routing."
            },
            "durable_timeout_ms": {
                "type": ["integer", "null"],
                "minimum": 1,
                "description": "run only. Applies if run creates a durable/background job; start uses timeout_ms for its durable lifetime cap."
            },
            "idempotency_key": {
                "type": ["string", "null"],
                "description": "run only. Deduplicates/replays matching run requests."
            },
            "job_id": {
                "type": ["string", "null"],
                "minLength": 1,
                "maxLength": 128,
                "description": "start/status/cancel durable job id. Optional for start, required for status/cancel."
            },
            "tail_bytes": {
                "type": ["integer", "null"],
                "minimum": 0,
                "maximum": 1048576,
                "description": "status only. Number of stdout/stderr tail bytes to read."
            }
        },
        "oneOf": [
            {
                "title": "shell operation=run",
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "run",
                        "default": "run",
                        "description": "Default shell operation."
                    },
                    "command": { "$ref": "#/properties/command" },
                    "args": { "$ref": "#/properties/args" },
                    "working_dir": { "$ref": "#/properties/working_dir" },
                    "env": { "$ref": "#/properties/env" },
                    "timeout_ms": { "$ref": "#/properties/timeout_ms" },
                    "execution_mode": { "$ref": "#/properties/execution_mode" },
                    "durable_timeout_ms": { "$ref": "#/properties/durable_timeout_ms" },
                    "idempotency_key": { "$ref": "#/properties/idempotency_key" }
                }
            },
            {
                "title": "shell operation=start",
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "command"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "start",
                        "description": "Create a durable shell job immediately."
                    },
                    "command": { "$ref": "#/properties/command" },
                    "args": { "$ref": "#/properties/args" },
                    "working_dir": { "$ref": "#/properties/working_dir" },
                    "env": { "$ref": "#/properties/env" },
                    "timeout_ms": { "$ref": "#/properties/timeout_ms" },
                    "job_id": { "$ref": "#/properties/job_id" }
                }
            },
            {
                "title": "shell operation=status",
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "job_id"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "status",
                        "description": "Read persisted durable shell job state."
                    },
                    "job_id": { "$ref": "#/properties/job_id" },
                    "tail_bytes": { "$ref": "#/properties/tail_bytes" }
                }
            },
            {
                "title": "shell operation=cancel",
                "type": "object",
                "additionalProperties": false,
                "required": ["operation", "job_id"],
                "properties": {
                    "operation": {
                        "type": "string",
                        "const": "cancel",
                        "description": "Terminate an exact durable shell job."
                    },
                    "job_id": { "$ref": "#/properties/job_id" }
                }
            }
        ]
    });
    match schema {
        Value::Object(object) => Arc::new(object),
        _ => Arc::new(Map::new()),
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellFacadeResponse {
    pub operation: ShellOperation,
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ActRunShellResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<ActRunShellStartResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ActRunShellStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<ActRunShellCancelResponse>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOperation {
    #[default]
    List,
    Launch,
    History,
}

impl ProcessOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Launch => "launch",
            Self::History => "history",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessParams {
    #[serde(default)]
    #[schemars(description = "Process facade operation. Defaults to list.")]
    pub operation: ProcessOperation,
    #[serde(default)]
    #[schemars(description = "Executable path/name for launch operations.")]
    pub target: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub wait_for_window_title_regex: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub cdp_debug: Option<bool>,
    #[serde(default)]
    pub force_renderer_accessibility: Option<bool>,
    #[serde(default)]
    pub windows_console_window_state: Option<LaunchWindowState>,
    #[serde(default)]
    pub desktop: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub process_name_contains: Option<String>,
    #[serde(default)]
    pub command_line_contains: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 1000))]
    pub limit: Option<usize>,
    #[serde(default)]
    #[schemars(default)]
    pub include_command_line: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessFacadeResponse {
    pub operation: ProcessOperation,
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<ActLaunchResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<ProcessListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<ProcessHistoryResponse>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessListResponse {
    pub source_of_truth: String,
    pub returned_count: usize,
    pub limit: usize,
    pub filters: ProcessFilters,
    pub rows: Vec<ProcessRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line_contains: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessRow {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pid: Option<u32>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub status: String,
    pub start_time_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessHistoryResponse {
    pub source_of_truth: String,
    pub cf_name: String,
    pub returned_count: usize,
    pub scanned_tail_rows: usize,
    pub limit: usize,
    pub filters: ProcessFilters,
    pub rows: Vec<ProcessHistoryRow>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessHistoryRow {
    pub key: String,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub row_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
}

/// Builds the per-spawn manifest JSON. Records the CLI and, when the operator
/// pinned one, the model — the authoritative model source the transcript
/// ingester reads (indispensable for Codex, whose stream omits it, #949).
fn build_spawn_manifest(spawn_id: &str, params: &ActSpawnAgentParams) -> Result<Value, ErrorData> {
    let agent_kind = params.effective_cli()?;
    Ok(json!({
        "version": AGENT_SPAWN_MANIFEST_VERSION,
        "spawn_id": spawn_id,
        "cli": agent_kind.as_str(),
        "kind": agent_kind.as_str(),
        "model": params.model_for_spawn_manifest(agent_kind),
        "model_ref": params.local_model_ref(),
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
static AGENT_SPAWN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct AgentSpawnInFlightGuard {
    spawn_id: String,
    sequence: u64,
    in_flight_at_start: u64,
    started_at: Instant,
}

impl AgentSpawnInFlightGuard {
    fn enter(spawn_id: &str, cli: ActSpawnAgentCli) -> Self {
        let sequence = AGENT_SPAWN_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let in_flight_at_start = AGENT_SPAWN_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::info!(
            code = "AGENT_SPAWN_IN_FLIGHT_ENTER",
            spawn_id,
            cli = cli.as_str(),
            sequence,
            in_flight_at_start,
            "act_spawn_agent entered provisioning"
        );
        Self {
            spawn_id: spawn_id.to_owned(),
            sequence,
            in_flight_at_start,
            started_at: Instant::now(),
        }
    }

    fn in_flight_now() -> u64 {
        AGENT_SPAWN_IN_FLIGHT.load(Ordering::SeqCst)
    }
}

impl Drop for AgentSpawnInFlightGuard {
    fn drop(&mut self) {
        let before = AGENT_SPAWN_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        let in_flight_after = before.saturating_sub(1);
        tracing::info!(
            code = "AGENT_SPAWN_IN_FLIGHT_EXIT",
            spawn_id = %self.spawn_id,
            sequence = self.sequence,
            in_flight_after,
            elapsed_ms = duration_ms_u64(self.started_at.elapsed()),
            "act_spawn_agent left provisioning"
        );
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

fn shell_facade_error(
    operation: ShellOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message.clone(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": SHELL_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "remediation": remediation,
        })),
    )
}

fn process_facade_error(
    operation: ProcessOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message.clone(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": PROCESS_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "remediation": remediation,
        })),
    )
}

fn shell_facade_delegate_error(
    operation: ShellOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let source_id = source_id.into();
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": SHELL_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id,
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

fn process_facade_delegate_error(
    operation: ProcessOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let source_id = source_id.into();
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": PROCESS_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id,
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

fn require_shell_text(
    operation: ShellOperation,
    value: Option<String>,
    field: &'static str,
    source_id: &str,
) -> Result<String, ErrorData> {
    let Some(value) = value else {
        return Err(shell_facade_error(
            operation,
            source_id,
            format!("shell operation={} requires {field}", operation.as_str()),
            "provide the required field for this shell operation",
        ));
    };
    if value.trim().is_empty() {
        return Err(shell_facade_error(
            operation,
            source_id,
            format!(
                "shell operation={} requires non-empty {field}",
                operation.as_str()
            ),
            "provide a non-empty executable or job id",
        ));
    }
    Ok(value)
}

fn require_process_text(
    operation: ProcessOperation,
    value: Option<String>,
    field: &'static str,
    source_id: &str,
) -> Result<String, ErrorData> {
    let Some(value) = value else {
        return Err(process_facade_error(
            operation,
            source_id,
            format!("process operation={} requires {field}", operation.as_str()),
            "provide the required field for this process operation",
        ));
    };
    if value.trim().is_empty() {
        return Err(process_facade_error(
            operation,
            source_id,
            format!(
                "process operation={} requires non-empty {field}",
                operation.as_str()
            ),
            "provide a non-empty executable path/name or source id",
        ));
    }
    Ok(value)
}

fn shell_unexpected_fields(
    operation: ShellOperation,
    params: &ShellParams,
    fields: &[&'static str],
) -> Result<(), ErrorData> {
    if fields.is_empty() {
        return Ok(());
    }
    let source_id = params
        .job_id
        .as_deref()
        .or(params.command.as_deref())
        .unwrap_or_else(|| operation.as_str());
    Err(shell_facade_error(
        operation,
        source_id,
        format!(
            "shell operation={} does not accept field(s): {}",
            operation.as_str(),
            fields.join(", ")
        ),
        "remove fields that belong to a different shell operation",
    ))
}

fn process_unexpected_fields(
    operation: ProcessOperation,
    params: &ProcessParams,
    fields: &[&'static str],
) -> Result<(), ErrorData> {
    if fields.is_empty() {
        return Ok(());
    }
    let source_id = params
        .target
        .as_deref()
        .or_else(|| params.process_name_contains.as_deref())
        .or_else(|| params.command_line_contains.as_deref())
        .map(str::to_owned)
        .or_else(|| params.pid.map(|pid| pid.to_string()))
        .unwrap_or_else(|| operation.as_str().to_owned());
    Err(process_facade_error(
        operation,
        source_id,
        format!(
            "process operation={} does not accept field(s): {}",
            operation.as_str(),
            fields.join(", ")
        ),
        "remove fields that belong to a different process operation",
    ))
}

fn shell_run_params(params: ShellParams) -> Result<ActRunShellParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.job_id.is_some() {
        unexpected.push("job_id");
    }
    if params.tail_bytes.is_some() {
        unexpected.push("tail_bytes");
    }
    shell_unexpected_fields(ShellOperation::Run, &params, &unexpected)?;
    let command = require_shell_text(
        ShellOperation::Run,
        params.command,
        "command",
        ShellOperation::Run.as_str(),
    )?;
    Ok(ActRunShellParams {
        command,
        args: params.args.unwrap_or_default(),
        working_dir: params.working_dir,
        env: params.env.unwrap_or_default(),
        timeout_ms: params
            .timeout_ms
            .unwrap_or(crate::m4::DEFAULT_SHELL_TIMEOUT_MS),
        execution_mode: params
            .execution_mode
            .unwrap_or(ActRunShellExecutionMode::Auto),
        durable_timeout_ms: params.durable_timeout_ms,
        idempotency_key: params.idempotency_key,
    })
}

fn shell_start_params(params: ShellParams) -> Result<ActRunShellStartParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.execution_mode.is_some() {
        unexpected.push("execution_mode");
    }
    if params.durable_timeout_ms.is_some() {
        unexpected.push("durable_timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    if params.tail_bytes.is_some() {
        unexpected.push("tail_bytes");
    }
    shell_unexpected_fields(ShellOperation::Start, &params, &unexpected)?;
    let command = require_shell_text(
        ShellOperation::Start,
        params.command,
        "command",
        ShellOperation::Start.as_str(),
    )?;
    Ok(ActRunShellStartParams {
        command,
        args: params.args.unwrap_or_default(),
        working_dir: params.working_dir,
        env: params.env.unwrap_or_default(),
        timeout_ms: params.timeout_ms,
        job_id: params.job_id,
    })
}

fn shell_status_params(params: ShellParams) -> Result<ActRunShellStatusParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.command.is_some() {
        unexpected.push("command");
    }
    if params.args.is_some() {
        unexpected.push("args");
    }
    if params.working_dir.is_some() {
        unexpected.push("working_dir");
    }
    if params.env.is_some() {
        unexpected.push("env");
    }
    if params.timeout_ms.is_some() {
        unexpected.push("timeout_ms");
    }
    if params.execution_mode.is_some() {
        unexpected.push("execution_mode");
    }
    if params.durable_timeout_ms.is_some() {
        unexpected.push("durable_timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    shell_unexpected_fields(ShellOperation::Status, &params, &unexpected)?;
    let job_id = require_shell_text(
        ShellOperation::Status,
        params.job_id,
        "job_id",
        ShellOperation::Status.as_str(),
    )?;
    Ok(ActRunShellStatusParams {
        job_id,
        tail_bytes: params
            .tail_bytes
            .unwrap_or(crate::m4::SHELL_JOB_TAIL_DEFAULT_BYTES),
    })
}

fn shell_cancel_params(params: ShellParams) -> Result<ActRunShellJobIdParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.command.is_some() {
        unexpected.push("command");
    }
    if params.args.is_some() {
        unexpected.push("args");
    }
    if params.working_dir.is_some() {
        unexpected.push("working_dir");
    }
    if params.env.is_some() {
        unexpected.push("env");
    }
    if params.timeout_ms.is_some() {
        unexpected.push("timeout_ms");
    }
    if params.execution_mode.is_some() {
        unexpected.push("execution_mode");
    }
    if params.durable_timeout_ms.is_some() {
        unexpected.push("durable_timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    if params.tail_bytes.is_some() {
        unexpected.push("tail_bytes");
    }
    shell_unexpected_fields(ShellOperation::Cancel, &params, &unexpected)?;
    let job_id = require_shell_text(
        ShellOperation::Cancel,
        params.job_id,
        "job_id",
        ShellOperation::Cancel.as_str(),
    )?;
    Ok(ActRunShellJobIdParams { job_id })
}

fn process_launch_params(params: ProcessParams) -> Result<ActLaunchParams, ErrorData> {
    let mut unexpected = Vec::new();
    if params.pid.is_some() {
        unexpected.push("pid");
    }
    if params.process_name_contains.is_some() {
        unexpected.push("process_name_contains");
    }
    if params.command_line_contains.is_some() {
        unexpected.push("command_line_contains");
    }
    if params.limit.is_some() {
        unexpected.push("limit");
    }
    if params.include_command_line.is_some() {
        unexpected.push("include_command_line");
    }
    process_unexpected_fields(ProcessOperation::Launch, &params, &unexpected)?;
    let target = require_process_text(
        ProcessOperation::Launch,
        params.target,
        "target",
        ProcessOperation::Launch.as_str(),
    )?;
    Ok(ActLaunchParams {
        target,
        args: params.args.unwrap_or_default(),
        working_dir: params.working_dir,
        env: params.env.unwrap_or_default(),
        wait_for_window_title_regex: params.wait_for_window_title_regex,
        timeout_ms: params
            .timeout_ms
            .unwrap_or(crate::m4::DEFAULT_LAUNCH_TIMEOUT_MS),
        idempotency_key: params.idempotency_key,
        cdp_debug: params.cdp_debug,
        force_renderer_accessibility: params.force_renderer_accessibility,
        windows_console_window_state: params.windows_console_window_state,
        desktop: params.desktop,
    })
}

fn validate_process_query_params(
    operation: ProcessOperation,
    params: &ProcessParams,
) -> Result<usize, ErrorData> {
    let mut unexpected = Vec::new();
    if params.target.is_some() {
        unexpected.push("target");
    }
    if params.args.is_some() {
        unexpected.push("args");
    }
    if params.working_dir.is_some() {
        unexpected.push("working_dir");
    }
    if params.env.is_some() {
        unexpected.push("env");
    }
    if params.wait_for_window_title_regex.is_some() {
        unexpected.push("wait_for_window_title_regex");
    }
    if params.timeout_ms.is_some() {
        unexpected.push("timeout_ms");
    }
    if params.idempotency_key.is_some() {
        unexpected.push("idempotency_key");
    }
    if params.cdp_debug.is_some() {
        unexpected.push("cdp_debug");
    }
    if params.force_renderer_accessibility.is_some() {
        unexpected.push("force_renderer_accessibility");
    }
    if params.windows_console_window_state.is_some() {
        unexpected.push("windows_console_window_state");
    }
    if params.desktop.is_some() {
        unexpected.push("desktop");
    }
    process_unexpected_fields(operation, params, &unexpected)?;

    for (field, value) in [
        (
            "process_name_contains",
            params.process_name_contains.as_deref(),
        ),
        (
            "command_line_contains",
            params.command_line_contains.as_deref(),
        ),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(process_facade_error(
                operation,
                field,
                format!(
                    "process operation={} requires non-empty {field}",
                    operation.as_str()
                ),
                "remove the empty filter or provide a non-empty filter value",
            ));
        }
    }

    let limit = params.limit.unwrap_or(match operation {
        ProcessOperation::List => PROCESS_LIST_DEFAULT_LIMIT,
        ProcessOperation::History => PROCESS_HISTORY_DEFAULT_LIMIT,
        ProcessOperation::Launch => unreachable!("launch is not a query operation"),
    });
    let max_limit = match operation {
        ProcessOperation::List => PROCESS_LIST_MAX_LIMIT,
        ProcessOperation::History => PROCESS_HISTORY_MAX_LIMIT,
        ProcessOperation::Launch => unreachable!("launch is not a query operation"),
    };
    if limit == 0 || limit > max_limit {
        return Err(process_facade_error(
            operation,
            limit.to_string(),
            format!(
                "process operation={} limit must be between 1 and {max_limit}",
                operation.as_str()
            ),
            "use a bounded positive limit for the requested readback",
        ));
    }
    Ok(limit)
}

fn process_filters(params: &ProcessParams) -> ProcessFilters {
    ProcessFilters {
        pid: params.pid,
        process_name_contains: params.process_name_contains.clone(),
        command_line_contains: params.command_line_contains.clone(),
    }
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn process_row_matches(filters: &ProcessFilters, row: &ProcessRow) -> bool {
    if filters.pid.is_some_and(|pid| row.pid != pid) {
        return false;
    }
    if let Some(filter) = filters.process_name_contains.as_deref()
        && !contains_case_insensitive(&row.name, filter)
    {
        return false;
    }
    if let Some(filter) = filters.command_line_contains.as_deref() {
        let Some(command_line) = row.command_line.as_deref() else {
            return false;
        };
        if !contains_case_insensitive(command_line, filter) {
            return false;
        }
    }
    true
}

fn process_history_row_matches(filters: &ProcessFilters, row: &ProcessHistoryRow) -> bool {
    if filters.pid.is_some_and(|pid| row.pid != Some(pid)) {
        return false;
    }
    if let Some(filter) = filters.process_name_contains.as_deref() {
        let Some(target) = row.target.as_deref() else {
            return false;
        };
        if !contains_case_insensitive(target, filter) {
            return false;
        }
    }
    if let Some(filter) = filters.command_line_contains.as_deref() {
        let Some(command_line) = row.command_line.as_deref() else {
            return false;
        };
        if !contains_case_insensitive(command_line, filter) {
            return false;
        }
    }
    true
}

fn process_list_response(params: &ProcessParams) -> Result<ProcessListResponse, ErrorData> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let limit = validate_process_query_params(ProcessOperation::List, params)?;
    let filters = process_filters(params);
    let include_command_line =
        params.include_command_line.unwrap_or(false) || filters.command_line_contains.is_some();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );

    let mut rows = Vec::new();
    for (pid, process) in system.processes() {
        let command_line = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let row = ProcessRow {
            pid: pid.as_u32(),
            parent_pid: process.parent().map(|parent| parent.as_u32()),
            name: process.name().to_string_lossy().into_owned(),
            exe: process.exe().map(|path| path.display().to_string()),
            cwd: process.cwd().map(|path| path.display().to_string()),
            status: format!("{:?}", process.status()),
            start_time_unix_ms: process.start_time().saturating_mul(1000),
            command_line: include_command_line.then_some(command_line),
        };
        if !process_row_matches(&filters, &row) {
            continue;
        }
        rows.push(row);
        if rows.len() >= limit {
            break;
        }
    }
    Ok(ProcessListResponse {
        source_of_truth: "live OS process table via sysinfo refresh_processes_specifics".to_owned(),
        returned_count: rows.len(),
        limit,
        filters,
        rows,
    })
}

fn process_history_response(
    service: &SynapseService,
    params: &ProcessParams,
) -> Result<ProcessHistoryResponse, ErrorData> {
    let limit = validate_process_query_params(ProcessOperation::History, params)?;
    let filters = process_filters(params);
    let rows = {
        let runtime = service.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            process_facade_error(
                ProcessOperation::History,
                cf::CF_PROCESS_HISTORY,
                "reflex runtime lock poisoned while reading process history",
                "retry after the daemon lock recovers; inspect daemon logs if this repeats",
            )
        })?;
        runtime
            .storage_cf_tail_rows(cf::CF_PROCESS_HISTORY, limit)
            .map_err(|error| {
                process_facade_error(
                    ProcessOperation::History,
                    cf::CF_PROCESS_HISTORY,
                    format!("CF_PROCESS_HISTORY tail read failed: {error}"),
                    "inspect the RocksDB column family and daemon storage logs",
                )
            })?
    };
    let scanned_tail_rows = rows.len();
    let mut decoded_rows = Vec::new();
    for (key, value) in rows {
        let decoded = decode_json::<Value>(&value).map_err(|error| {
            process_facade_error(
                ProcessOperation::History,
                hex_lower(&key),
                format!("CF_PROCESS_HISTORY row decode failed: {error}"),
                "inspect the exact process history row bytes and fix the writer",
            )
        })?;
        let row_json = serde_json::to_string(&decoded).map_err(|error| {
            process_facade_error(
                ProcessOperation::History,
                hex_lower(&key),
                format!("CF_PROCESS_HISTORY row JSON render failed: {error}"),
                "inspect the decoded process history row",
            )
        })?;
        let row = ProcessHistoryRow {
            key: String::from_utf8_lossy(&key).into_owned(),
            key_hex: hex_lower(&key),
            value_len_bytes: u64::try_from(value.len()).unwrap_or(u64::MAX),
            row_json,
            pid: json_u32_field(&decoded, "pid"),
            target: json_string_field(&decoded, "target"),
            tool: json_string_field(&decoded, "tool"),
            status: json_string_field(&decoded, "status"),
            launched_at: json_string_field(&decoded, "launched_at"),
            command_line: json_string_field(&decoded, "command_line"),
        };
        if process_history_row_matches(&filters, &row) {
            decoded_rows.push(row);
        }
    }
    Ok(ProcessHistoryResponse {
        source_of_truth: PROCESS_FACADE_SOURCE_OF_TRUTH.to_owned(),
        cf_name: cf::CF_PROCESS_HISTORY.to_owned(),
        returned_count: decoded_rows.len(),
        scanned_tail_rows,
        limit,
        filters,
        rows: decoded_rows,
    })
}

fn json_u32_field(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|raw| u32::try_from(raw).ok())
}

fn json_string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
        if let Err(error) = self.ensure_supported_use_allows_action("act_combo") {
            self.audit_action_denied_for_request("act_combo", &error, &request_context);
            return Err(error);
        }
        self.refresh_reflex_audit_context()?;
        self.audit_action_started_for_request("act_combo", &request_context)?;
        let runtime = self.reflex_runtime()?;
        let result = execute_combo(runtime, params.0).await;
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
        if let Err(error) = self.ensure_supported_use_allows_action("act_run_shell") {
            self.audit_action_denied_for_request("act_run_shell", &error, &request_context);
            return Err(error);
        }
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
        let result = match authorize_run_shell(&self.m4_config, &params) {
            Ok(authorization) => {
                run_shell_with_idempotency(
                    self,
                    params,
                    authorization,
                    self.m4_config.run_shell_inline_await_limit_ms(),
                    Some(&shell_context),
                )
                .await
            }
            Err(error) => Err(error),
        };
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
        if let Err(error) = self.ensure_supported_use_allows_action("act_run_shell") {
            self.audit_action_denied_for_request("act_run_shell_start", &error, &request_context);
            return Err(error);
        }
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
        let result = match authorize_run_shell_start(&self.m4_config, &params) {
            Ok(authorization) => {
                start_authorized_shell_job(params, &authorization, Some(&shell_context))
            }
            Err(error) => Err(error),
        };
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
        if let Err(error) = self.ensure_supported_use_allows_action("act_run_shell") {
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
        let result = shell_job_status(&params, Some(&session_id));
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
        if let Err(error) = self.ensure_supported_use_allows_action("act_run_shell") {
            self.audit_action_denied_for_request("act_run_shell_cancel", &error, &request_context);
            return Err(error);
        }
        let params = params.0;
        let session_id = require_shell_session_id(&request_context)?;
        let before_status = shell_job_status(
            &ActRunShellStatusParams {
                job_id: params.job_id.clone(),
                tail_bytes: 1024,
            },
            Some(&session_id),
        );
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
        let result = cancel_shell_job(&params, Some(&session_id));
        let after_status = shell_job_status(
            &ActRunShellStatusParams {
                job_id: params.job_id.clone(),
                tail_bytes: 1024,
            },
            Some(&session_id),
        );
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
        if let Err(error) = self.ensure_supported_use_allows_action("act_launch") {
            self.audit_action_denied_for_request("act_launch", &error, &request_context);
            return Err(error);
        }
        let params = params.0;
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.act_launch_for_session_id(params, session_id)
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
            .act_spawn_agent_impl(params, started_by_session_id, spawn_id.clone())
            .await;
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
        let result = match launch_for_session(
            &self.m4_config,
            params.clone(),
            session_id.as_deref(),
        )
        .await
        {
            Ok(mut outcome) => {
                let response = outcome.response.clone();
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

impl SynapseService {
    pub(crate) async fn dashboard_spawn_local_model_agent(
        &self,
        params: ActSpawnAgentParams,
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
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
            .act_spawn_agent_impl(params, None, spawn_id.clone())
            .await;
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
            .act_spawn_agent_impl(params, None, spawn_id.clone())
            .await;
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
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
        validate_agent_spawn_params(&params)?;
        validate_spawn_target(&params.target)?;
        let agent_kind = params.effective_cli()?;
        let local_model_row = if agent_kind.is_local_model() {
            Some(
                self.require_spawn_local_model_row(
                    &params,
                    started_by_session_id
                        .as_deref()
                        .unwrap_or("dashboard_spawn_agent"),
                )
                .await?,
            )
        } else {
            None
        };
        let mut timing = AgentSpawnTiming::new();
        let in_flight = AgentSpawnInFlightGuard::enter(&spawn_id, agent_kind);

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
                        "source_error_data": error.data.clone(),
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
                        "source_error_data": error.data.clone(),
                        "spawn_timing": timing.readback(&in_flight, params.wait_timeout_ms),
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
        let mut matched = match self
            .wait_for_spawned_agent_session(
                &params,
                agent_kind,
                &spawn_id,
                &before_session_ids,
                launched_at_unix_ms,
                launch_response.pid,
                &files,
                session_wait_deadline,
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
                        "spawn_timing": timing.readback(&in_flight, params.wait_timeout_ms),
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
                        "spawn_timing": timing.readback(&in_flight, params.wait_timeout_ms),
                        "wait_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };
        timing.mark_session_matched();

        timing.mark_task_wait_started();
        let task_wait_deadline =
            agent_spawn_wait_deadline_from(Instant::now(), params.wait_timeout_ms)?;
        let task_started = match self
            .wait_for_spawned_agent_task_started(
                &params,
                agent_kind,
                &spawn_id,
                &mut matched,
                &before_session_ids,
                launched_at_unix_ms,
                launch_response.pid,
                &files,
                task_wait_deadline,
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
                        "spawn_timing": timing.readback(&in_flight, params.wait_timeout_ms),
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
                        "spawn_timing": timing.readback(&in_flight, params.wait_timeout_ms),
                        "task_start_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };
        timing.mark_task_started();
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

        Ok(ActSpawnAgentResponse {
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

fn validate_spawn_local_model_static_requirements(
    model_ref: &str,
    row: &LocalModelRegistryRow,
) -> Result<(), ErrorData> {
    if !row.enabled {
        return Err(local_model_spawn_refusal(
            error_codes::MODEL_REGISTRY_DISABLED,
            "local_model_registry_row_disabled",
            "act_spawn_agent local_model refused because the registry row is disabled",
            json!({
                "model_ref": model_ref,
                "row_key": row.row_key.clone(),
                "enabled": row.enabled,
                "last_probe": row.last_probe.clone(),
                "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
            }),
        ));
    }
    if row.api_shape != LocalModelApiShape::OpenAiChatCompletions {
        return Err(local_model_spawn_refusal(
            error_codes::MODEL_TOOLS_UNSUPPORTED,
            "local_model_api_shape_unsupported",
            "act_spawn_agent local_model refused because the registry row API shape is unsupported",
            json!({
                "model_ref": model_ref,
                "row_key": row.row_key.clone(),
                "api_shape": row.api_shape,
                "supported_api_shape": "open_ai_chat_completions",
            }),
        ));
    }
    Ok(())
}

fn local_model_unhealthy_refusal(model_ref: &str, row: &LocalModelRegistryRow) -> ErrorData {
    let code = match row
        .last_probe
        .as_ref()
        .and_then(|probe| probe.error_code.as_deref())
    {
        Some(error_codes::MODEL_TOOLS_UNSUPPORTED) => error_codes::MODEL_TOOLS_UNSUPPORTED,
        Some(error_codes::MODEL_ENDPOINT_UNREACHABLE) | None => {
            error_codes::MODEL_ENDPOINT_UNREACHABLE
        }
        Some(_) => error_codes::MODEL_ENDPOINT_UNREACHABLE,
    };
    local_model_spawn_refusal(
        code,
        "local_model_registry_row_unhealthy",
        "act_spawn_agent local_model refused because the registry row's last probe is unhealthy",
        json!({
            "model_ref": model_ref,
            "row_key": row.row_key.clone(),
            "last_probe": row.last_probe.clone(),
            "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
        }),
    )
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

fn agent_spawn_terminal_capture_artifacts(log_dir: &Path) -> CaptureArtifacts {
    CaptureArtifacts {
        asciicast_path: log_dir.join("terminal.cast"),
        status_path: log_dir.join("terminal-capture-status.json"),
        final_screen_path: log_dir.join("terminal-final-screen.txt"),
        input_audit_path: log_dir.join("terminal-input-audit.ndjson"),
    }
}

fn agent_spawn_request_details(
    params: &ActSpawnAgentParams,
    started_by_session_id: Option<&str>,
) -> serde_json::Value {
    let agent_kind = params
        .effective_cli()
        .map(ActSpawnAgentCli::as_str)
        .unwrap_or("invalid");
    json!({
        "cli": agent_kind,
        "kind": agent_kind,
        "model_ref": params.local_model_ref(),
        "target": params.target,
        "working_dir": params.working_dir,
        "mcp_url": params.mcp_url,
        "wait_timeout_ms": params.wait_timeout_ms,
        "hold_open_ms": params.hold_open_ms,
        "prompt_present": params.prompt.as_ref().is_some_and(|prompt| !prompt.is_empty()),
        "prompt_bytes": params.prompt.as_ref().map_or(0, String::len),
        "started_by_session_id": started_by_session_id,
        "required_foreground": false,
        "launch_target_resolution": "runtime_powershell_host_preflight",
        "launch_target_env_var": AGENT_SPAWN_SHELL_ENV_VAR,
        "windows_console_window_state": "hidden",
    })
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

fn resolve_agent_spawn_powershell_host() -> Result<AgentSpawnLaunchHost, ErrorData> {
    if let Some(configured) = std::env::var_os(AGENT_SPAWN_SHELL_ENV_VAR) {
        let configured = configured.into_string().map_err(|_| {
            agent_spawn_shell_error(
                "agent_spawn_shell_env_not_unicode",
                "act_spawn_agent launcher shell preflight failed because SYNAPSE_AGENT_SPAWN_SHELL is not valid Unicode",
                json!({
                    "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
                    "supported_shells": ["pwsh.exe", "powershell.exe"],
                }),
            )
        })?;
        let candidate = trim_configured_agent_spawn_shell(&configured);
        if candidate.is_empty() {
            return Err(agent_spawn_shell_error(
                "agent_spawn_shell_env_empty",
                "act_spawn_agent launcher shell preflight failed because SYNAPSE_AGENT_SPAWN_SHELL is empty",
                json!({
                    "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
                    "configured_value": configured,
                    "supported_shells": ["pwsh.exe", "powershell.exe"],
                }),
            ));
        }
        ensure_supported_agent_spawn_shell(candidate)?;
        let mut attempted = Vec::new();
        if let Some(target) = resolve_agent_spawn_shell_candidate(candidate, &mut attempted) {
            return Ok(AgentSpawnLaunchHost {
                target,
                source: format!("env:{AGENT_SPAWN_SHELL_ENV_VAR}"),
                attempted,
            });
        }
        return Err(agent_spawn_shell_error(
            "agent_spawn_shell_env_target_missing",
            "act_spawn_agent launcher shell preflight failed because SYNAPSE_AGENT_SPAWN_SHELL did not resolve to an executable file",
            json!({
                "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
                "configured_value": configured,
                "normalized_candidate": candidate,
                "attempted": attempted,
                "supported_shells": ["pwsh.exe", "powershell.exe"],
            }),
        ));
    }

    resolve_default_agent_spawn_powershell_host()
}

fn trim_configured_agent_spawn_shell(value: &str) -> &str {
    let trimmed = value.trim();
    if let Some(stripped) = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        return stripped.trim();
    }
    if let Some(stripped) = trimmed
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    {
        return stripped.trim();
    }
    trimmed
}

#[cfg(windows)]
fn resolve_default_agent_spawn_powershell_host() -> Result<AgentSpawnLaunchHost, ErrorData> {
    let mut attempted = Vec::new();
    for (source, candidate) in AGENT_SPAWN_WINDOWS_SHELL_CANDIDATES {
        ensure_supported_agent_spawn_shell(candidate)?;
        if let Some(target) = resolve_agent_spawn_shell_candidate(candidate, &mut attempted) {
            return Ok(AgentSpawnLaunchHost {
                target,
                source: (*source).to_owned(),
                attempted,
            });
        }
    }

    Err(agent_spawn_shell_error(
        "agent_spawn_shell_not_found",
        "act_spawn_agent launcher shell preflight failed because no supported PowerShell host was found",
        json!({
            "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
            "attempted": attempted,
            "supported_shells": ["pwsh.exe", "powershell.exe"],
            "setup_action": "Install PowerShell 7 or ensure Windows PowerShell exists at C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe, then restart the synapse-mcp daemon so the process environment is current.",
        }),
    ))
}

#[cfg(not(windows))]
fn resolve_default_agent_spawn_powershell_host() -> Result<AgentSpawnLaunchHost, ErrorData> {
    let mut attempted = Vec::new();
    let candidate = "pwsh";
    if let Some(target) = resolve_agent_spawn_shell_candidate(candidate, &mut attempted) {
        return Ok(AgentSpawnLaunchHost {
            target,
            source: "path:pwsh".to_owned(),
            attempted,
        });
    }

    Err(agent_spawn_shell_error(
        "agent_spawn_shell_not_found",
        "act_spawn_agent launcher shell preflight failed because no supported PowerShell host was found",
        json!({
            "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
            "attempted": attempted,
            "supported_shells": ["pwsh"],
        }),
    ))
}

fn ensure_supported_agent_spawn_shell(candidate: &str) -> Result<(), ErrorData> {
    let name = Path::new(candidate)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(candidate)
        .to_ascii_lowercase();
    let supported = if cfg!(windows) {
        matches!(
            name.as_str(),
            "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe"
        )
    } else {
        matches!(name.as_str(), "pwsh")
    };
    if supported {
        return Ok(());
    }

    Err(agent_spawn_shell_error(
        "agent_spawn_shell_unsupported",
        "act_spawn_agent launcher shell preflight failed because the configured shell is not a supported PowerShell host",
        json!({
            "env_var": AGENT_SPAWN_SHELL_ENV_VAR,
            "candidate": candidate,
            "observed_file_name": name,
            "supported_shells": if cfg!(windows) {
                json!(["pwsh.exe", "powershell.exe"])
            } else {
                json!(["pwsh"])
            },
        }),
    ))
}

fn resolve_agent_spawn_shell_candidate(
    candidate: &str,
    attempted: &mut Vec<String>,
) -> Option<String> {
    let candidate_path = Path::new(candidate);
    if is_path_like_agent_spawn_shell(candidate) {
        record_agent_spawn_shell_attempt(attempted, candidate_path);
        return candidate_path
            .is_file()
            .then(|| display_agent_spawn_shell_path(candidate_path));
    }

    let names = agent_spawn_executable_names(candidate);
    if let Some(path_value) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_value) {
            for name in &names {
                let path = directory.join(name);
                record_agent_spawn_shell_attempt(attempted, &path);
                if path.is_file() {
                    return Some(display_agent_spawn_shell_path(&path));
                }
            }
        }
    }
    None
}

fn is_path_like_agent_spawn_shell(candidate: &str) -> bool {
    let path = Path::new(candidate);
    path.is_absolute()
        || candidate.contains('\\')
        || candidate.contains('/')
        || candidate
            .as_bytes()
            .get(1)
            .is_some_and(|second| *second == b':')
}

fn agent_spawn_executable_names(candidate: &str) -> Vec<String> {
    if Path::new(candidate).extension().is_some() {
        return vec![candidate.to_owned()];
    }

    let mut names = vec![candidate.to_owned()];
    for extension in agent_spawn_path_extensions() {
        names.push(format!("{candidate}{extension}"));
    }
    names
}

#[cfg(windows)]
fn agent_spawn_path_extensions() -> Vec<String> {
    let mut extensions = std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .map(|value| {
            value
                .split(';')
                .filter_map(|extension| {
                    let extension = extension.trim();
                    (!extension.is_empty()).then(|| extension.to_owned())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            vec![
                ".COM".to_owned(),
                ".EXE".to_owned(),
                ".BAT".to_owned(),
                ".CMD".to_owned(),
            ]
        });
    if !extensions
        .iter()
        .any(|extension| extension.eq_ignore_ascii_case(".exe"))
    {
        extensions.push(".EXE".to_owned());
    }
    extensions
}

#[cfg(not(windows))]
fn agent_spawn_path_extensions() -> Vec<String> {
    Vec::new()
}

fn record_agent_spawn_shell_attempt(attempted: &mut Vec<String>, path: &Path) {
    if attempted.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT {
        attempted.push(path.display().to_string());
    }
}

fn display_agent_spawn_shell_path(path: &Path) -> String {
    if path.is_absolute() {
        return path.display().to_string();
    }
    std::env::current_dir()
        .map(|current_dir| current_dir.join(path).display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn agent_spawn_shell_error(
    reason: &'static str,
    message: &'static str,
    detail: Value,
) -> ErrorData {
    let mut data = Map::new();
    data.insert("code".to_owned(), json!(error_codes::ACTION_TARGET_INVALID));
    data.insert("reason".to_owned(), json!(reason));
    data.insert("tool".to_owned(), json!(ACT_SPAWN_AGENT));
    data.insert("detail".to_owned(), detail);
    agent_spawn_tool_error(
        error_codes::ACTION_TARGET_INVALID,
        message,
        Value::Object(data),
    )
}

fn augment_agent_spawn_error_with_artifacts(
    mut error: ErrorData,
    files: &AgentSpawnFiles,
    params: &ActSpawnAgentParams,
    spawn_id: &str,
    failure_stage: &'static str,
    launch_target: Option<&str>,
    completion_artifacts: Value,
) -> ErrorData {
    let mut data = match error.data.take() {
        Some(Value::Object(data)) => data,
        Some(source_data) => {
            let mut data = Map::new();
            data.insert("source_error_data".to_owned(), source_data);
            data
        }
        None => Map::new(),
    };
    let agent_kind = params.effective_cli().ok();
    data.entry("code".to_owned())
        .or_insert_with(|| json!(error_codes::ACTION_AGENT_SPAWN_FAILED));
    data.entry("reason".to_owned())
        .or_insert_with(|| json!(failure_stage));
    data.insert("agent_spawn_failure_stage".to_owned(), json!(failure_stage));
    data.insert("spawn_id".to_owned(), json!(spawn_id));
    data.insert(
        "cli".to_owned(),
        json!(
            agent_kind
                .map(ActSpawnAgentCli::as_str)
                .unwrap_or("invalid")
        ),
    );
    data.insert("mcp_url".to_owned(), json!(params.mcp_url));
    data.insert(
        "log_dir".to_owned(),
        json!(files.log_dir.display().to_string()),
    );
    if let Some(launch_target) = launch_target {
        data.insert("launch_target".to_owned(), json!(launch_target));
    }
    data.insert("completion_artifacts".to_owned(), completion_artifacts);
    error.data = Some(Value::Object(data));
    error
}

fn write_agent_spawn_daemon_terminal_artifacts(
    files: &AgentSpawnFiles,
    params: &ActSpawnAgentParams,
    spawn_id: &str,
    status: &str,
    error_message: &str,
    details: serde_json::Value,
) -> serde_json::Value {
    let agent_kind = params.effective_cli().ok();
    let agent_kind = agent_kind
        .map(ActSpawnAgentCli::as_str)
        .unwrap_or("invalid");
    let completed_at_unix_ms = unix_time_ms_now();
    let stdout_len = file_len(&files.stdout_path);
    let stderr_len = file_len(&files.stderr_path);
    let terminal = agent_spawn_terminal_capture_artifacts(&files.log_dir);
    let final_message = json!({
        "schema_version": 1,
        "spawn_id": spawn_id,
        "cli": agent_kind,
        "kind": agent_kind,
        "status": status,
        "exit_code": null,
        "error_message": error_message,
        "message": "Synapse act_spawn_agent wrote this terminal artifact because the daemon ended the spawn before a final assistant response was available.",
        "stdout_path": files.stdout_path.display().to_string(),
        "stderr_path": files.stderr_path.display().to_string(),
        "completion_status_path": files.completion_status_path.display().to_string(),
        "task_started_path": files.task_started_path.display().to_string(),
        "terminal_asciicast_path": terminal.asciicast_path.display().to_string(),
        "terminal_capture_status_path": terminal.status_path.display().to_string(),
        "terminal_final_screen_path": terminal.final_screen_path.display().to_string(),
        "terminal_input_audit_path": terminal.input_audit_path.display().to_string(),
        "details": details,
    });
    let final_write = serde_json::to_vec_pretty(&final_message)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            fs::write(&files.final_message_path, bytes).map_err(|error| error.to_string())
        });
    let final_len = file_len(&files.final_message_path);
    let completion_status = json!({
        "schema_version": 1,
        "spawn_id": spawn_id,
        "cli": agent_kind,
        "kind": agent_kind,
        "status": status,
        "exit_code": null,
        "error_message": error_message,
        "wrapper_started_at_unix_ms": null,
        "completed_at_unix_ms": completed_at_unix_ms,
        "elapsed_ms": null,
        "requested_hold_open_ms": params.hold_open_ms,
        "hold_open_elapsed_ms_met": false,
        "final_message_path": files.final_message_path.display().to_string(),
        "final_message_bytes": final_len,
        "final_message_present": final_len > 0,
        "final_message_source": "daemon_terminal_artifact_json",
        "recovered_final_message_written": false,
        "fallback_final_message_written": true,
        "stdout_path": files.stdout_path.display().to_string(),
        "stdout_line_count": null,
        "last_stdout_event_type": null,
        "stdout_bytes": stdout_len,
        "stderr_path": files.stderr_path.display().to_string(),
        "stderr_bytes": stderr_len,
        "terminal_asciicast_path": terminal.asciicast_path.display().to_string(),
        "terminal_asciicast_bytes": file_len(&terminal.asciicast_path),
        "terminal_capture_status_path": terminal.status_path.display().to_string(),
        "terminal_capture_status_bytes": file_len(&terminal.status_path),
        "terminal_final_screen_path": terminal.final_screen_path.display().to_string(),
        "terminal_final_screen_bytes": file_len(&terminal.final_screen_path),
        "terminal_input_audit_path": terminal.input_audit_path.display().to_string(),
        "terminal_input_audit_bytes": file_len(&terminal.input_audit_path),
        "daemon_terminal_artifact": true,
    });
    let status_write = serde_json::to_vec_pretty(&completion_status)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            fs::write(&files.completion_status_path, bytes).map_err(|error| error.to_string())
        });
    let status_len = file_len(&files.completion_status_path);
    json!({
        "final_message_path": files.final_message_path.display().to_string(),
        "final_message_write_ok": final_write.is_ok(),
        "final_message_write_error": final_write.err(),
        "final_message_bytes_after": final_len,
        "completion_status_path": files.completion_status_path.display().to_string(),
        "completion_status_write_ok": status_write.is_ok(),
        "completion_status_write_error": status_write.err(),
        "completion_status_bytes_after": status_len,
        "task_started_path": files.task_started_path.display().to_string(),
        "task_started_bytes": file_len(&files.task_started_path),
    })
}

/// Resolve the API-key credential that a spawned local-model agent must carry
/// into its child process environment.
///
/// `act_launch` clears the child environment, so the daemon is the only place
/// the credential lives at spawn time. Returns:
/// - `Ok(None)` when the row needs no key (loopback model, or non-local-model
///   spawn),
/// - `Ok(Some((env_var, value)))` when the daemon has the declared key set to a
///   non-empty value, ready to inject into the child env,
/// - `Err(..)` (MODEL_API_KEY_MISSING) when the row declares an
///   `api_key_env_var` the daemon does not have, so the spawn is refused loudly
///   instead of launching an agent that will 401 on its first model call.
fn resolve_spawn_local_model_api_key(
    db: &std::sync::Arc<synapse_storage::Db>,
    local_model_row: Option<&LocalModelRegistryRow>,
) -> Result<Option<(String, String)>, ErrorData> {
    let Some(row) = local_model_row else {
        return Ok(None);
    };
    // Single resolution point shared with probing: encrypted DPAPI secret store
    // first, then the daemon process environment, else a loud refusal. Only the
    // resolved value is forwarded into the child env (never persisted).
    match crate::m3::local_models::resolve_local_model_api_key(db, row) {
        Ok(ResolvedApiKey::NotRequired) => Ok(None),
        Ok(ResolvedApiKey::Resolved {
            env_var,
            value,
            source,
        }) => {
            // Audit which credential source authenticated the spawn (never the
            // value). "dpapi_secret_store" means the encrypted at-rest key was
            // used; "process_env" means the daemon environment fallback.
            tracing::info!(
                model = %row.name,
                env_var = %env_var,
                source,
                "resolved local-model API key for spawn"
            );
            Ok(Some((env_var, value)))
        }
        Err(error) => {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            let (refusal_code, reason): (&'static str, &'static str) = match code {
                Some(error_codes::MODEL_API_KEY_DECRYPT_FAILED) => (
                    error_codes::MODEL_API_KEY_DECRYPT_FAILED,
                    "local_model_api_key_decrypt_failed",
                ),
                _ => (
                    error_codes::MODEL_API_KEY_MISSING,
                    "local_model_api_key_missing",
                ),
            };
            Err(local_model_spawn_refusal(
                refusal_code,
                reason,
                "act_spawn_agent local_model refused because no API key could be resolved for the model from the encrypted secret store or the daemon environment",
                json!({
                    "model_ref": row.name,
                    "api_key_env_var": row.api_key_env_var,
                    "resolver_error": error.message,
                    "resolver_data": error.data,
                    "remediation": "store the key on this Windows account via the dashboard Add/Edit API Model form or local_model_update { name, api_key }, or set the environment variable before launching the daemon",
                    "source_of_truth": "DPAPI secret store (CF_KV local_model_secret/v1) + daemon process environment",
                }),
            ))
        }
    }
}

fn local_model_spawn_refusal(
    code: &'static str,
    reason: &'static str,
    message: &'static str,
    detail: Value,
) -> ErrorData {
    agent_spawn_tool_error(
        code,
        message,
        json!({
            "code": code,
            "reason": reason,
            "tool": ACT_SPAWN_AGENT,
            "detail": detail,
        }),
    )
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

fn recover_orphaned_agent_spawn_terminal_artifacts()
-> Result<AgentSpawnOrphanRecoveryReport, ErrorData> {
    let root = agent_spawn_root_dir()?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentSpawnOrphanRecoveryReport::default());
        }
        Err(error) => {
            return Err(mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "act_spawn_agent failed to read agent spawn root {} during orphan recovery: {error}",
                    root.display()
                ),
            ));
        }
    };
    let now = unix_time_ms_now();
    let mut report = AgentSpawnOrphanRecoveryReport::default();
    for entry in entries {
        let entry = entry.map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "act_spawn_agent failed to read an agent spawn root entry at {} during orphan recovery: {error}",
                    root.display()
                ),
            )
        })?;
        let file_type = entry.file_type().map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "act_spawn_agent failed to read file type for {} during orphan recovery: {error}",
                    entry.path().display()
                ),
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let spawn_id = entry.file_name().to_string_lossy().into_owned();
        if !spawn_id.starts_with("agent-spawn-") {
            continue;
        }
        report.scanned_count += 1;
        let log_dir = entry.path();
        let decision = agent_spawn_orphan_recovery_decision(&spawn_id, &log_dir, now)?;
        match decision {
            AgentSpawnOrphanRecoveryDecision::SkipTerminal => {
                report.skipped_terminal_count += 1;
            }
            AgentSpawnOrphanRecoveryDecision::SkipLive => {
                report.skipped_live_count += 1;
            }
            AgentSpawnOrphanRecoveryDecision::SkipFresh => {
                report.skipped_fresh_count += 1;
            }
            AgentSpawnOrphanRecoveryDecision::Recover(recovery) => {
                write_agent_spawn_orphan_terminal_artifacts(&spawn_id, &log_dir, &recovery)?;
                report.recovered_count += 1;
                report.recovered_spawn_ids.push(spawn_id);
            }
        }
    }
    Ok(report)
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

fn agent_spawn_orphan_recovery_decision(
    spawn_id: &str,
    log_dir: &Path,
    now: u64,
) -> Result<AgentSpawnOrphanRecoveryDecision, ErrorData> {
    let completion_status_path = log_dir.join("completion-status.json");
    let status_age_ms =
        file_age_ms(&completion_status_path, now).or_else(|| file_age_ms(log_dir, now));
    let status_bytes = match fs::read(&completion_status_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if stale_enough_for_orphan_recovery(status_age_ms) {
                return Ok(AgentSpawnOrphanRecoveryDecision::Recover(
                    AgentSpawnOrphanRecovery {
                        status: "orphaned_status_missing_recovered",
                        reason: "missing_completion_status_stale",
                        cli: "unknown".to_owned(),
                        wrapper_process_id: None,
                        source_completion_status: None,
                        source_completion_status_error: Some(format!(
                            "completion-status.json was missing for stale spawn directory {spawn_id}"
                        )),
                        status_age_ms,
                    },
                ));
            }
            return Ok(AgentSpawnOrphanRecoveryDecision::SkipFresh);
        }
        Err(error) => {
            return Err(mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "act_spawn_agent failed to read agent spawn completion status {} during orphan recovery: {error}",
                    completion_status_path.display()
                ),
            ));
        }
    };
    let status_json = match serde_json::from_slice::<Value>(&status_bytes) {
        Ok(status_json) => status_json,
        Err(error) => {
            if stale_enough_for_orphan_recovery(status_age_ms) {
                return Ok(AgentSpawnOrphanRecoveryDecision::Recover(
                    AgentSpawnOrphanRecovery {
                        status: "orphaned_status_invalid_recovered",
                        reason: "invalid_completion_status_stale",
                        cli: "unknown".to_owned(),
                        wrapper_process_id: None,
                        source_completion_status: None,
                        source_completion_status_error: Some(error.to_string()),
                        status_age_ms,
                    },
                ));
            }
            return Ok(AgentSpawnOrphanRecoveryDecision::SkipFresh);
        }
    };
    let Some(status) = status_json.get("status").and_then(Value::as_str) else {
        if stale_enough_for_orphan_recovery(status_age_ms) {
            return Ok(AgentSpawnOrphanRecoveryDecision::Recover(
                AgentSpawnOrphanRecovery {
                    status: "orphaned_status_invalid_recovered",
                    reason: "missing_status_field_stale",
                    cli: status_json
                        .get("cli")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    wrapper_process_id: wrapper_process_id_from_status(&status_json),
                    source_completion_status: Some(status_json),
                    source_completion_status_error: None,
                    status_age_ms,
                },
            ));
        }
        return Ok(AgentSpawnOrphanRecoveryDecision::SkipFresh);
    };
    if status != "running" {
        return Ok(AgentSpawnOrphanRecoveryDecision::SkipTerminal);
    }
    let wrapper_process_id = wrapper_process_id_from_status(&status_json);
    if let Some(pid) = wrapper_process_id {
        if wrapper_process_is_live_for_status(pid, &status_json) {
            return Ok(AgentSpawnOrphanRecoveryDecision::SkipLive);
        }
        return Ok(AgentSpawnOrphanRecoveryDecision::Recover(
            AgentSpawnOrphanRecovery {
                status: "orphaned_running_recovered",
                reason: "running_status_wrapper_process_gone",
                cli: status_json
                    .get("cli")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                wrapper_process_id,
                source_completion_status: Some(status_json),
                source_completion_status_error: None,
                status_age_ms,
            },
        ));
    }
    if stale_enough_for_orphan_recovery(status_age_ms) {
        return Ok(AgentSpawnOrphanRecoveryDecision::Recover(
            AgentSpawnOrphanRecovery {
                status: "orphaned_running_recovered",
                reason: "running_status_without_wrapper_pid_stale",
                cli: status_json
                    .get("cli")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                wrapper_process_id: None,
                source_completion_status: Some(status_json),
                source_completion_status_error: None,
                status_age_ms,
            },
        ));
    }
    Ok(AgentSpawnOrphanRecoveryDecision::SkipFresh)
}

fn stale_enough_for_orphan_recovery(age_ms: Option<u64>) -> bool {
    age_ms.is_some_and(|age_ms| age_ms >= AGENT_SPAWN_ORPHAN_RECOVERY_STALE_MS)
}

fn wrapper_process_id_from_status(status: &Value) -> Option<u32> {
    status
        .get("wrapper_process_id")
        .or_else(|| status.get("powershell_process_id"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

fn wrapper_process_is_live_for_status(pid: u32, status: &Value) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    let sys_pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[sys_pid]), false);
    let Some(process) = system.process(sys_pid) else {
        return false;
    };
    let Some(wrapper_started_at_unix_ms) = status
        .get("wrapper_started_at_unix_ms")
        .and_then(Value::as_u64)
    else {
        return true;
    };
    let process_started_at_unix_ms = process.start_time().saturating_mul(1000);
    process_started_at_unix_ms.abs_diff(wrapper_started_at_unix_ms) <= 30_000
}

fn wrapper_process_is_live_for_recovery(recovery: &AgentSpawnOrphanRecovery) -> bool {
    let Some(pid) = recovery.wrapper_process_id else {
        return false;
    };
    if let Some(status) = &recovery.source_completion_status {
        wrapper_process_is_live_for_status(pid, status)
    } else {
        process_exists(pid)
    }
}

fn write_agent_spawn_orphan_terminal_artifacts(
    spawn_id: &str,
    log_dir: &Path,
    recovery: &AgentSpawnOrphanRecovery,
) -> Result<(), ErrorData> {
    let stdout_path = log_dir.join("stdout.jsonl");
    let stderr_path = log_dir.join("stderr.log");
    let final_message_path = log_dir.join("final-message.txt");
    let completion_status_path = log_dir.join("completion-status.json");
    let stdout_len = file_len(&stdout_path);
    let stderr_len = file_len(&stderr_path);
    let final_message_len_before = file_len(&final_message_path);
    let (stdout_line_count, last_stdout_event_type) = stdout_summary_lossy(&stdout_path);
    let stdout_tail = tail_file_lossy(&stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES);
    let stderr_tail = tail_file_lossy(&stderr_path, AGENT_SPAWN_LOG_TAIL_BYTES);
    let completed_at_unix_ms = unix_time_ms_now();
    let details = json!({
        "reason": recovery.reason,
        "spawn_id": spawn_id,
        "log_dir": log_dir.display().to_string(),
        "wrapper_process_id": recovery.wrapper_process_id,
        "wrapper_process_live": wrapper_process_is_live_for_recovery(recovery),
        "status_age_ms": recovery.status_age_ms,
        "stale_threshold_ms": AGENT_SPAWN_ORPHAN_RECOVERY_STALE_MS,
        "source_completion_status": &recovery.source_completion_status,
        "source_completion_status_error": &recovery.source_completion_status_error,
        "stdout_tail": stdout_tail,
        "stderr_tail": stderr_tail,
    });
    if final_message_len_before == 0 {
        let final_message = json!({
            "schema_version": 1,
            "spawn_id": spawn_id,
            "cli": &recovery.cli,
            "status": recovery.status,
            "exit_code": null,
            "error_message": "agent spawn wrapper exited, disappeared, or daemon state was lost before a terminal completion artifact was written",
            "message": "Synapse act_spawn_agent orphan recovery wrote this terminal artifact because a stale spawn directory did not contain a terminal final-message/completion-status pair.",
            "stdout_path": stdout_path.display().to_string(),
            "stderr_path": stderr_path.display().to_string(),
            "completion_status_path": completion_status_path.display().to_string(),
            "details": details.clone(),
        });
        let bytes = serde_json::to_vec_pretty(&final_message).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_spawn_agent failed to encode orphan recovery final-message artifact: {error}"),
            )
        })?;
        fs::write(&final_message_path, bytes).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "act_spawn_agent failed to write orphan recovery final-message artifact {}: {error}",
                    final_message_path.display()
                ),
            )
        })?;
    }
    let final_message_len_after = file_len(&final_message_path);
    let completion_status = json!({
        "schema_version": 1,
        "spawn_id": spawn_id,
        "cli": &recovery.cli,
        "status": recovery.status,
        "exit_code": null,
        "error_message": "agent spawn wrapper exited, disappeared, or daemon state was lost before a terminal completion artifact was written",
        "wrapper_process_id": recovery.wrapper_process_id,
        "wrapper_process_live": wrapper_process_is_live_for_recovery(recovery),
        "wrapper_started_at_unix_ms": recovery
            .source_completion_status
            .as_ref()
            .and_then(|status| status.get("wrapper_started_at_unix_ms"))
            .and_then(Value::as_u64),
        "completed_at_unix_ms": completed_at_unix_ms,
        "elapsed_ms": null,
        "requested_hold_open_ms": recovery
            .source_completion_status
            .as_ref()
            .and_then(|status| status.get("requested_hold_open_ms"))
            .cloned(),
        "hold_open_elapsed_ms_met": false,
        "final_message_path": final_message_path.display().to_string(),
        "final_message_bytes": final_message_len_after,
        "final_message_present": final_message_len_after > 0,
        "final_message_source": if final_message_len_before > 0 {
            "preexisting_final_message_without_terminal_status"
        } else {
            "orphan_recovery_artifact_json"
        },
        "recovered_final_message_written": false,
        "fallback_final_message_written": final_message_len_before == 0,
        "stdout_path": stdout_path.display().to_string(),
        "stdout_line_count": stdout_line_count,
        "last_stdout_event_type": last_stdout_event_type,
        "stdout_bytes": stdout_len,
        "stderr_path": stderr_path.display().to_string(),
        "stderr_bytes": stderr_len,
        "daemon_terminal_artifact": true,
        "orphan_recovery_artifact": true,
        "completion_status_source": "act_spawn_agent_orphan_recovery",
        "source_completion_status_error": &recovery.source_completion_status_error,
        "log_dir": log_dir.display().to_string(),
        "details": details,
    });
    let bytes = serde_json::to_vec_pretty(&completion_status).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_spawn_agent failed to encode orphan recovery completion-status artifact: {error}"),
        )
    })?;
    fs::write(&completion_status_path, bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_spawn_agent failed to write orphan recovery completion-status artifact {}: {error}",
                completion_status_path.display()
            ),
        )
    })?;
    Ok(())
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn file_age_ms(path: &Path, now: u64) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let modified = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    let modified = u64::try_from(modified).ok()?;
    Some(now.saturating_sub(modified))
}

fn stdout_summary_lossy(path: &Path) -> (u64, Option<String>) {
    let Some(stdout) = tail_file_lossy(path, usize::MAX) else {
        return (0, None);
    };
    let mut line_count = 0;
    let mut last_event_type = None;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        line_count += 1;
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(event_type) = value.get("type").and_then(Value::as_str) {
                last_event_type = Some(event_type.to_owned());
            } else if let Some(event_type) = value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
            {
                last_event_type = Some(event_type.to_owned());
            }
        }
    }
    (line_count, last_event_type)
}

fn validate_spawn_target(target: &Option<ActSpawnAgentTarget>) -> Result<(), ErrorData> {
    match target {
        Some(ActSpawnAgentTarget::Window { window_hwnd })
        | Some(ActSpawnAgentTarget::Cdp { window_hwnd, .. }) => {
            validate_target_window(*window_hwnd)?;
        }
        None => {}
    }
    Ok(())
}

fn resolve_agent_working_dir(working_dir: Option<&str>) -> Result<PathBuf, ErrorData> {
    let path = match working_dir {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_spawn_agent failed to read daemon current directory: {error}"),
            )
        })?,
    };
    let canonical = fs::canonicalize(&path).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_spawn_agent working_dir {:?} could not be resolved: {error}",
                path
            ),
        )
    })?;
    if !canonical.is_dir() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_spawn_agent working_dir {:?} is not a directory",
                canonical
            ),
        ));
    }
    Ok(canonical)
}

fn read_synapse_bearer_token() -> Result<String, ErrorData> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            "act_spawn_agent requires APPDATA to locate the Synapse bearer token file",
        )
    })?;
    let token_path = PathBuf::from(appdata).join("synapse").join("token.txt");
    let token = fs::read_to_string(&token_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "act_spawn_agent failed to read Synapse bearer token at {}: {error}",
                token_path.display()
            ),
        )
    })?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(mcp_error(
            error_codes::HTTP_TOKEN_INVALID,
            format!(
                "act_spawn_agent read an empty Synapse bearer token at {}",
                token_path.display()
            ),
        ));
    }
    Ok(token)
}

fn prepare_agent_spawn_files(
    spawn_id: &str,
    params: &ActSpawnAgentParams,
    working_dir: &Path,
) -> Result<AgentSpawnFiles, ErrorData> {
    let agent_kind = params.effective_cli()?;
    let root = agent_spawn_root_dir()?;
    let log_dir = root.join(spawn_id);
    fs::create_dir_all(&log_dir).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_spawn_agent failed to create log directory {}: {error}",
                log_dir.display()
            ),
        )
    })?;
    let prompt_path = log_dir.join("prompt.txt");
    let stdout_path = log_dir.join("stdout.jsonl");
    let stderr_path = log_dir.join("stderr.log");
    let final_message_path = log_dir.join("final-message.txt");
    let completion_status_path = log_dir.join("completion-status.json");
    let task_started_path = log_dir.join("task-started.json");
    let task_started_script_path = log_dir.join("write-task-started.ps1");
    let debug_path =
        (agent_kind == ActSpawnAgentCli::Claude).then(|| log_dir.join("claude-debug.log"));
    let mcp_config_path =
        (agent_kind == ActSpawnAgentCli::Claude).then(|| log_dir.join("claude-mcp-config.json"));
    let hook_settings_path =
        (agent_kind == ActSpawnAgentCli::Claude).then(|| log_dir.join("claude-hook-settings.json"));
    let notify_script_path =
        (agent_kind == ActSpawnAgentCli::Codex).then(|| log_dir.join("codex-notify.ps1"));
    let codex_app_server_runner_path = (agent_kind == ActSpawnAgentCli::Codex)
        .then(|| log_dir.join("codex-app-server-runner.ps1"));
    let codex_app_server_control_path =
        (agent_kind == ActSpawnAgentCli::Codex).then(|| log_dir.join("codex-control.json"));
    let codex_app_server_events_path = (agent_kind == ActSpawnAgentCli::Codex)
        .then(|| log_dir.join("codex-app-server-events.jsonl"));
    let codex_app_server_stdout_path = (agent_kind == ActSpawnAgentCli::Codex)
        .then(|| log_dir.join("codex-app-server.stdout.log"));
    let codex_app_server_stderr_path = (agent_kind == ActSpawnAgentCli::Codex)
        .then(|| log_dir.join("codex-app-server.stderr.log"));
    let local_model_runner_path = agent_kind
        .is_local_model()
        .then(|| log_dir.join("local-model-runner.json"));

    let prompt = build_agent_spawn_prompt(
        spawn_id,
        params,
        working_dir,
        &task_started_path,
        &task_started_script_path,
    )?;
    fs::write(&prompt_path, prompt).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_spawn_agent failed to write prompt file {}: {error}",
                prompt_path.display()
            ),
        )
    })?;
    if let Some(config_path) = &mcp_config_path {
        let config = json!({
            "mcpServers": {
                "synapse": {
                    "type": "http",
                    "url": params.mcp_url,
                    "headers": {
                        "Authorization": "Bearer ${SYNAPSE_BEARER_TOKEN}",
                        // Lets the approval facade's delegated gate attribute a permission
                        // request to THIS spawn (#927); the bearer is shared
                        // across spawns and can't distinguish them. Read by
                        // server::permission_gate::SPAWN_ID_HEADER (case-insensitive).
                        "X-Synapse-Spawn-Id": spawn_id
                    },
                    // Per-server tool-call wall-clock (ms). The approval facade
                    // call blocks while a human decides; Claude's default
                    // MCP_TOOL_TIMEOUT (~28h) is plenty, but we pin 30 min here
                    // so a forgotten approval can't pin the agent forever — the
                    // gate itself denies at ~25 min, just inside this wall.
                    "timeout": 1_800_000
                }
            }
        });
        let encoded = serde_json::to_vec_pretty(&config).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_spawn_agent failed to encode Claude MCP config: {error}"),
            )
        })?;
        fs::write(config_path, encoded).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "act_spawn_agent failed to write Claude MCP config {}: {error}",
                    config_path.display()
                ),
            )
        })?;
    }

    if let Some(settings_path) = &hook_settings_path {
        let settings =
            build_claude_hook_settings(spawn_id, &params.mcp_url, params.require_approval_gate)?;
        let encoded = serde_json::to_vec_pretty(&settings).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("act_spawn_agent failed to encode Claude hook settings: {error}"),
            )
        })?;
        fs::write(settings_path, encoded).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "act_spawn_agent failed to write Claude hook settings {}: {error}",
                    settings_path.display()
                ),
            )
        })?;
    }
    if let Some(script_path) = &notify_script_path {
        let script = build_codex_notify_script(spawn_id, &params.mcp_url, &log_dir)?;
        fs::write(script_path, script).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "act_spawn_agent failed to write Codex notify script {}: {error}",
                    script_path.display()
                ),
            )
        })?;
    }
    if let Some(runner_path) = &codex_app_server_runner_path {
        fs::write(runner_path, CODEX_APP_SERVER_RUNNER_SCRIPT).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "act_spawn_agent failed to write Codex app-server runner {}: {error}",
                    runner_path.display()
                ),
            )
        })?;
    }

    // Spawn manifest: the authoritative record of which CLI and (when the
    // operator pinned one) which model this spawn was launched with. The
    // transcript ingester reads it to attribute cost — indispensable for Codex,
    // whose `exec --json` stream carries no model id (#949).
    let manifest_path = log_dir.join(AGENT_SPAWN_MANIFEST_FILENAME);
    let manifest = build_spawn_manifest(spawn_id, params)?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_spawn_agent failed to encode spawn manifest: {error}"),
        )
    })?;
    fs::write(&manifest_path, manifest_bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_spawn_agent failed to write spawn manifest {}: {error}",
                manifest_path.display()
            ),
        )
    })?;

    Ok(AgentSpawnFiles {
        log_dir,
        prompt_path,
        stdout_path,
        stderr_path,
        final_message_path,
        completion_status_path,
        task_started_path,
        task_started_script_path,
        debug_path,
        mcp_config_path,
        hook_settings_path,
        notify_script_path,
        codex_app_server_runner_path,
        codex_app_server_control_path,
        codex_app_server_events_path,
        codex_app_server_stdout_path,
        codex_app_server_stderr_path,
        local_model_runner_path,
    })
}

/// Derives the push-telemetry ingress endpoint from the MCP URL the spawned
/// agent is wired to. The daemon serves both from one origin, so anything
/// other than a `/mcp`-suffixed URL is a caller error, not a guessing game.
fn agent_event_ingress_url(
    spawn_id: &str,
    mcp_url: &str,
    source: &str,
) -> Result<String, ErrorData> {
    let Some(base) = mcp_url.strip_suffix("/mcp") else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_spawn_agent cannot derive the /agent-events ingress URL: mcp_url {mcp_url:?} does not end with \"/mcp\""
            ),
        ));
    };
    if base.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_spawn_agent mcp_url {mcp_url:?} has no scheme/authority before \"/mcp\""),
        ));
    }
    Ok(format!(
        "{base}/agent-events?spawn_id={spawn_id}&source={source}"
    ))
}

/// Claude Code `--settings` payload subscribing the daemon's ingress to the
/// hook events listed in
/// [`super::agent_event_ingress::CLAUDE_HOOK_SUBSCRIBED_EVENTS`]. Uses the
/// CLI's native HTTP hooks (verified on Claude Code 2.1.176): no per-event
/// child process, bearer injected via `allowedEnvVars` interpolation, and
/// delivery failures are non-blocking for the agent.
fn build_claude_hook_settings(
    spawn_id: &str,
    mcp_url: &str,
    require_approval_gate: bool,
) -> Result<Value, ErrorData> {
    let ingress_url = agent_event_ingress_url(spawn_id, mcp_url, "claude_code_hooks")?;
    let hook_entry = json!({
        "type": "http",
        "url": ingress_url,
        "headers": { "Authorization": "Bearer $SYNAPSE_BEARER_TOKEN" },
        "allowedEnvVars": ["SYNAPSE_BEARER_TOKEN"],
        // Localhost POST; anything slower than this means the daemon is
        // gone and the agent should not crawl through its turn.
        "timeout": 5,
    });
    let mut hooks = Map::new();
    for event in super::agent_event_ingress::CLAUDE_HOOK_SUBSCRIBED_EVENTS {
        hooks.insert(
            (*event).to_owned(),
            json!([{ "hooks": [hook_entry.clone()] }]),
        );
    }
    let mut settings = json!({
        "hooks": hooks,
        "allowedHttpHookUrls": [format!("{}*", ingress_url)],
        "httpHookAllowedEnvVars": ["SYNAPSE_BEARER_TOKEN"],
    });
    if require_approval_gate {
        // Static allow rules are consulted BEFORE the permission-prompt-tool.
        // Only public facade tool names are valid here: spawned Claude runs on
        // the same <=40 normal-agent tool surface as production clients.
        // Anything unmatched falls through to mcp__synapse__approval, which
        // delegates to the hidden approval_gate implementation internally.
        let mut allow_rules: Vec<String> = CLAUDE_AUTO_ALLOW_RULES
            .iter()
            .map(|rule| (*rule).to_owned())
            .collect();
        allow_rules.extend(
            CLAUDE_COORDINATION_FACADE_MCP_TOOLS
                .iter()
                .map(|tool| format!("mcp__synapse__{tool}")),
        );
        settings["permissions"] = json!({ "allow": allow_rules });
    }
    Ok(settings)
}

/// Tools pre-approved in a gated Claude spawn's `--settings` so they skip the
/// approval facade round-trip. Mirrors the auto-allow side of
/// [`super::permission_policy`]; the delegated gate still backstops anything
/// unmatched.
const CLAUDE_AUTO_ALLOW_RULES: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "NotebookRead",
    "NotebookEdit",
    "TodoWrite",
    "ExitPlanMode",
    "BashOutput",
    "Task",
    "mcp__synapse__health",
    "mcp__synapse__session",
    "mcp__synapse__target",
    "mcp__synapse__agent",
    "mcp__synapse__approval",
    "Edit",
    "Write",
    "MultiEdit",
    "Bash(ls:*)",
    "Bash(cat:*)",
    "Bash(pwd)",
    "Bash(echo:*)",
    "Bash(head:*)",
    "Bash(tail:*)",
    "Bash(wc:*)",
    "Bash(grep:*)",
    "Bash(rg:*)",
    "Bash(find:*)",
    "Bash(which:*)",
    "Bash(git status:*)",
    "Bash(git diff:*)",
    "Bash(git log:*)",
    "Bash(git show:*)",
    "Bash(git branch:*)",
    "Bash(cargo build:*)",
    "Bash(cargo check:*)",
    "Bash(cargo test:*)",
    "Bash(cargo clippy:*)",
    "Bash(cargo fmt:*)",
];

const CLAUDE_COORDINATION_FACADE_MCP_TOOLS: &[&str] = &["workspace"];

/// Codex `notify` program: receives the notification JSON as its final argv
/// argument and POSTs it verbatim to the ingress. Codex spawns it
/// fire-and-forget, so delivery failures are persisted to
/// `notify-errors.log` in the spawn directory — local evidence instead of a
/// silent drop.
fn build_codex_notify_script(
    spawn_id: &str,
    mcp_url: &str,
    log_dir: &Path,
) -> Result<String, ErrorData> {
    let ingress_url = agent_event_ingress_url(spawn_id, mcp_url, "codex_notify")?;
    let error_log = ps_single_quoted_path(&log_dir.join("notify-errors.log"));
    Ok(format!(
        "$ErrorActionPreference = 'Stop'\n\
try {{\n\
    $payload = $args[-1]\n\
    if ([string]::IsNullOrWhiteSpace($payload)) {{ throw 'codex notify payload argument is missing' }}\n\
    $token = $env:SYNAPSE_BEARER_TOKEN\n\
    if ([string]::IsNullOrWhiteSpace($token)) {{ throw 'SYNAPSE_BEARER_TOKEN is not set in the codex notify environment' }}\n\
    Invoke-RestMethod -Method Post -Uri {ingress_url} -Headers @{{ Authorization = \"Bearer $token\" }} -ContentType 'application/json' -Body $payload -TimeoutSec 5 | Out-Null\n\
}} catch {{\n\
    Add-Content -Path {error_log} -Value (\"{{0}}|codex-notify-post-failed|{{1}}\" -f (Get-Date -Format o), $_.Exception.Message)\n\
    exit 1\n\
}}\n",
        ingress_url = ps_single_quote(&ingress_url),
        error_log = error_log,
    ))
}

pub(crate) fn agent_spawn_root_dir() -> Result<PathBuf, ErrorData> {
    let local_appdata = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            "act_spawn_agent requires LOCALAPPDATA to create per-agent log directories",
        )
    })?;
    Ok(PathBuf::from(local_appdata)
        .join("Synapse")
        .join("agent-spawns"))
}

fn build_agent_spawn_prompt(
    spawn_id: &str,
    params: &ActSpawnAgentParams,
    working_dir: &Path,
    task_started_path: &Path,
    _task_started_script_path: &Path,
) -> Result<String, ErrorData> {
    let agent_kind = params.effective_cli()?;
    if agent_kind.is_local_model() {
        let assigned_prompt = params.prompt.as_deref().unwrap_or("").trim();
        if assigned_prompt.is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_spawn_agent local_model prompt must not be empty",
            ));
        }
        return Ok(assigned_prompt.to_owned());
    }
    let target_instruction = match &params.target {
        Some(target) => {
            let target_json = serde_json::to_string(target).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("act_spawn_agent failed to encode target prompt JSON: {error}"),
                )
            })?;
            format!(
                "3. Call the real Synapse MCP target facade with exactly this JSON: {{\"operation\":\"set\",\"target\":{target_json}}}\n4. Call the real Synapse MCP target facade with exactly this JSON: {{\"operation\":\"get\"}} and verify its current target exactly matches that JSON."
            )
        }
        None => {
            "3. Call the real Synapse MCP target facade with exactly this JSON: {\"operation\":\"get\"} and report the returned session_id/current target.".to_owned()
        }
    };
    let assigned_prompt = params.prompt.as_deref().unwrap_or("").trim();
    if assigned_prompt.is_empty() {
        let mode = if params.template_id.is_some() {
            "template"
        } else {
            "direct spawn"
        };
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_spawn_agent {mode} prompt must not be empty"),
        ));
    }
    let assigned_block = format!(
        "After the provisioning checks pass, perform this assigned task:\n{assigned_prompt}"
    );
    let hold_instruction = if params.hold_open_ms == 0 {
        "Do not add an artificial hold-open sleep after completing the provisioning checks and assigned task.".to_owned()
    } else {
        format!(
            "After the provisioning checks and assigned task, keep this primary process alive for at least {} ms using Start-Sleep -Milliseconds {}, then finish.",
            params.hold_open_ms, params.hold_open_ms
        )
    };
    let task_started_path_display = task_started_path.display().to_string();
    Ok(format!(
        "You are a primary {cli} agent spawned by Synapse act_spawn_agent.\n\
Spawn ID: {spawn_id}\n\
Working directory: {working_dir}\n\
Windows shell contract:\n\
- Your local shell commands run under PowerShell on Windows, not Bash.\n\
- Do not use Bash heredocs such as `python - <<'PY'`. For multi-line Python, pipe a PowerShell here-string into Python:\n\
  @'\n\
  print(\"ok\")\n\
  '@ | python -\n\
- Use `Start-Sleep -Milliseconds N` for sleeps.\n\
- Write evidence/report files as UTF-8.\n\
Mandatory provisioning checks:\n\
1. Use the real Synapse MCP health tool through your normal configured MCP client. Do not use curl, direct HTTP, helper scripts, or local storage writes as a substitute.\n\
2. Use the real Synapse MCP session facade with exactly this JSON through your normal configured MCP client: {{\"operation\":\"list\"}}\n\
{target_instruction}\n\
5. If any Synapse MCP tool is missing or fails, stop and report the exact tool/error.\n\
6. Before performing the assigned task or hold-open sleep, write the required task-start readiness artifact to: {task_started_path}\n\
   Call the real Synapse MCP agent facade with exactly this JSON: {{\"operation\":\"task_started\",\"task_started\":{{\"spawn_id\":\"{spawn_id}\"}}}}\n\
   Verify the tool response has ok=true, session_id equal to this spawned MCP session id, and task_started_path equal to {task_started_path}.\n\
   If the facade call is missing or fails, stop and report the exact tool/error; do not use a helper script or direct file write.\n\
7. In your final response, include one compact JSON object containing spawn_id, health_ok, session_id, target_ok, task_started_path, and any error.\n\
\n\
{assigned_block}\n\
\n\
{hold_instruction}\n",
        cli = agent_kind.as_str(),
        spawn_id = spawn_id,
        working_dir = working_dir.display(),
        target_instruction = target_instruction,
        task_started_path = task_started_path_display,
        assigned_block = assigned_block,
        hold_instruction = hold_instruction,
    ))
}

fn write_agent_spawn_task_started_from_session(
    spawn_id: &str,
    session_id: &str,
) -> Result<AgentSpawnTaskStartedResponse, ErrorData> {
    validate_agent_spawn_id_path_segment(spawn_id)?;
    let root = agent_spawn_root_dir()?;
    let log_dir = root.join(spawn_id);
    if !log_dir.is_dir() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "agent_spawn_task_started refused unknown spawn_id {spawn_id:?}: no spawn directory at {}",
                log_dir.display()
            ),
        ));
    }

    let manifest = read_agent_spawn_manifest(&log_dir, spawn_id)?;
    let agent_kind = agent_kind_from_spawn_manifest(&manifest)?;
    let assigned_prompt_present = manifest
        .get("assigned_prompt_present")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let task_started_path = log_dir.join("task-started.json");

    if task_started_path.is_file() {
        let existing = read_task_started_value_strict(&task_started_path)?;
        ensure_existing_task_started_claim(
            &existing,
            spawn_id,
            agent_kind,
            session_id,
            assigned_prompt_present,
            &task_started_path,
        )?;
        return response_from_task_started_artifact(
            spawn_id,
            agent_kind,
            session_id,
            &task_started_path,
            existing,
        );
    }

    let artifact = build_agent_spawn_task_started_artifact(
        spawn_id,
        agent_kind,
        session_id,
        assigned_prompt_present,
        &task_started_path,
    );
    write_task_started_artifact_atomically(&task_started_path, &artifact)?;
    let readback = read_task_started_value_strict(&task_started_path)?;
    if readback != artifact {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "agent_spawn_task_started wrote {} but readback did not match the intended artifact",
                task_started_path.display()
            ),
        ));
    }
    response_from_task_started_artifact(
        spawn_id,
        agent_kind,
        session_id,
        &task_started_path,
        readback,
    )
}

fn validate_agent_spawn_id_path_segment(spawn_id: &str) -> Result<(), ErrorData> {
    if !spawn_id.starts_with("agent-spawn-") {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("spawn_id must start with \"agent-spawn-\", got {spawn_id:?}"),
        ));
    }
    if spawn_id.len() > 128 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("spawn_id exceeds 128 chars ({})", spawn_id.len()),
        ));
    }
    if !spawn_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "spawn_id must contain only ASCII alphanumerics and dashes",
        ));
    }
    Ok(())
}

fn read_agent_spawn_manifest(log_dir: &Path, spawn_id: &str) -> Result<Value, ErrorData> {
    let manifest_path = log_dir.join(AGENT_SPAWN_MANIFEST_FILENAME);
    let bytes = fs::read(&manifest_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "agent_spawn_task_started failed to read spawn manifest {}: {error}",
                manifest_path.display()
            ),
        )
    })?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "agent_spawn_task_started failed to parse spawn manifest {}: {error}",
                manifest_path.display()
            ),
        )
    })?;
    if manifest.get("spawn_id").and_then(Value::as_str) != Some(spawn_id) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "agent_spawn_task_started refused spawn_id {spawn_id:?}: manifest spawn_id mismatch in {}",
                manifest_path.display()
            ),
        ));
    }
    Ok(manifest)
}

fn agent_kind_from_spawn_manifest(manifest: &Value) -> Result<ActSpawnAgentCli, ErrorData> {
    let cli = manifest
        .get("cli")
        .or_else(|| manifest.get("kind"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "agent_spawn_task_started refused spawn manifest without cli/kind",
            )
        })?;
    match cli {
        "codex" => Ok(ActSpawnAgentCli::Codex),
        "claude" => Ok(ActSpawnAgentCli::Claude),
        "local_model" => Ok(ActSpawnAgentCli::LocalModel),
        other => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("agent_spawn_task_started refused unsupported spawn cli {other:?}"),
        )),
    }
}

fn build_agent_spawn_task_started_artifact(
    spawn_id: &str,
    agent_kind: ActSpawnAgentCli,
    session_id: &str,
    assigned_prompt_present: bool,
    task_started_path: &Path,
) -> Value {
    json!({
        "schema_version": 1,
        "spawn_id": spawn_id,
        "cli": agent_kind.as_str(),
        "session_id": session_id,
        "status": "started",
        "health_ok": true,
        "target_ok": true,
        "assigned_prompt_present": assigned_prompt_present,
        "task_started_path": task_started_path.display().to_string(),
        "started_at_unix_ms": unix_time_ms_now(),
        "readiness_source": "agent_spawn_task_started_tool",
    })
}

fn read_task_started_value_strict(path: &Path) -> Result<Value, ErrorData> {
    let bytes = fs::read(path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "agent_spawn_task_started failed to read {}: {error}",
                path.display()
            ),
        )
    })?;
    if bytes.is_empty() {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!("agent_spawn_task_started found empty {}", path.display()),
        ));
    }
    let json_bytes = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(bytes.as_slice());
    serde_json::from_slice(json_bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "agent_spawn_task_started failed to parse {}: {error}",
                path.display()
            ),
        )
    })
}

fn ensure_existing_task_started_claim(
    existing: &Value,
    spawn_id: &str,
    agent_kind: ActSpawnAgentCli,
    session_id: &str,
    assigned_prompt_present: bool,
    task_started_path: &Path,
) -> Result<(), ErrorData> {
    let expected_path = task_started_path.display().to_string();
    let mut errors = Vec::new();
    if existing.get("schema_version").and_then(Value::as_u64) != Some(1) {
        errors.push("schema_version mismatch");
    }
    if existing.get("spawn_id").and_then(Value::as_str) != Some(spawn_id) {
        errors.push("spawn_id mismatch");
    }
    if existing.get("cli").and_then(Value::as_str) != Some(agent_kind.as_str()) {
        errors.push("cli mismatch");
    }
    if existing.get("session_id").and_then(Value::as_str) != Some(session_id) {
        errors.push("session_id mismatch");
    }
    if existing.get("status").and_then(Value::as_str) != Some("started") {
        errors.push("status mismatch");
    }
    if existing.get("health_ok").and_then(Value::as_bool) != Some(true) {
        errors.push("health_ok mismatch");
    }
    if existing.get("target_ok").and_then(Value::as_bool) != Some(true) {
        errors.push("target_ok mismatch");
    }
    if existing
        .get("assigned_prompt_present")
        .and_then(Value::as_bool)
        != Some(assigned_prompt_present)
    {
        errors.push("assigned_prompt_present mismatch");
    }
    if existing.get("task_started_path").and_then(Value::as_str) != Some(expected_path.as_str()) {
        errors.push("task_started_path mismatch");
    }
    if existing
        .get("started_at_unix_ms")
        .and_then(Value::as_u64)
        .is_none_or(|value| value == 0)
    {
        errors.push("started_at_unix_ms missing");
    }
    if !errors.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "agent_spawn_task_started refused to overwrite existing readiness artifact at {}: {}",
                task_started_path.display(),
                errors.join(", ")
            ),
        ));
    }
    Ok(())
}

fn write_task_started_artifact_atomically(path: &Path, artifact: &Value) -> Result<(), ErrorData> {
    let bytes = serde_json::to_vec_pretty(artifact).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("agent_spawn_task_started failed to encode artifact: {error}"),
        )
    })?;
    let temp_path = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("task-started.json"),
        std::process::id(),
        unix_time_ms_now()
    ));
    fs::write(&temp_path, bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "agent_spawn_task_started failed to write temp artifact {}: {error}",
                temp_path.display()
            ),
        )
    })?;
    fs::rename(&temp_path, path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "agent_spawn_task_started failed to move temp artifact {} to {}: {error}",
                temp_path.display(),
                path.display()
            ),
        )
    })
}

fn response_from_task_started_artifact(
    spawn_id: &str,
    agent_kind: ActSpawnAgentCli,
    session_id: &str,
    task_started_path: &Path,
    artifact: Value,
) -> Result<AgentSpawnTaskStartedResponse, ErrorData> {
    let started_at_unix_ms = artifact
        .get("started_at_unix_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "agent_spawn_task_started read {} without started_at_unix_ms",
                    task_started_path.display()
                ),
            )
        })?;
    Ok(AgentSpawnTaskStartedResponse {
        ok: true,
        spawn_id: spawn_id.to_owned(),
        session_id: session_id.to_owned(),
        cli: agent_kind,
        task_started_path: task_started_path.display().to_string(),
        started_at_unix_ms,
        readiness_source: "agent_spawn_task_started_tool".to_owned(),
        artifact,
    })
}

fn agent_spawn_powershell_script(
    params: &ActSpawnAgentParams,
    files: &AgentSpawnFiles,
    working_dir: &Path,
) -> Result<String, ErrorData> {
    let agent_kind = params.effective_cli()?;
    let prompt_path = ps_single_quoted_path(&files.prompt_path);
    let stdout_path = ps_single_quoted_path(&files.stdout_path);
    let stderr_path = ps_single_quoted_path(&files.stderr_path);
    let final_message_path = ps_single_quoted_path(&files.final_message_path);
    let completion_status_path = ps_single_quoted_path(&files.completion_status_path);
    let task_started_path = ps_single_quoted_path(&files.task_started_path);
    let working_dir = ps_single_quoted_path(working_dir);
    let command_body = match agent_kind {
        ActSpawnAgentCli::Codex => {
            let Some(notify_script_path) = files.notify_script_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex notify script path",
                ));
            };
            let Some(runner_path) = files.codex_app_server_runner_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex app-server runner path",
                ));
            };
            let Some(control_path) = files.codex_app_server_control_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex app-server control path",
                ));
            };
            let Some(events_path) = files.codex_app_server_events_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex app-server events path",
                ));
            };
            let Some(app_stdout_path) = files.codex_app_server_stdout_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex app-server stdout path",
                ));
            };
            let Some(app_stderr_path) = files.codex_app_server_stderr_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex app-server stderr path",
                ));
            };
            let model_arg = params
                .model
                .as_deref()
                .map(|model| format!("$codexRunnerArgs['Model'] = {}\n", ps_single_quote(model)))
                .unwrap_or_default();
            let approval_gate_arg = if params.require_approval_gate {
                "$codexRunnerArgs['RequireApprovalGate'] = $true\n"
            } else {
                ""
            };
            format!(
                "$codexRunnerArgs = @{{\n\
    SpawnId = $spawnId\n\
    PromptPath = $spawnPromptPath\n\
    StdoutPath = $spawnStdoutPath\n\
    StderrPath = $spawnStderrPath\n\
    FinalMessagePath = $spawnFinalMessagePath\n\
    ControlPath = {control_path}\n\
    EventsPath = {events_path}\n\
    AppServerStdoutPath = {app_stdout_path}\n\
    AppServerStderrPath = {app_stderr_path}\n\
    WorkingDir = {working_dir}\n\
    McpUrl = {mcp_url}\n\
    NotifyScriptPath = {notify_script_path}\n\
}}\n\
{model_arg}\
{approval_gate_arg}\
& {runner_path} @codexRunnerArgs\n\
",
                runner_path = ps_single_quoted_path(runner_path),
                control_path = ps_single_quoted_path(control_path),
                events_path = ps_single_quoted_path(events_path),
                app_stdout_path = ps_single_quoted_path(app_stdout_path),
                app_stderr_path = ps_single_quoted_path(app_stderr_path),
                working_dir = working_dir,
                mcp_url = ps_single_quote(&params.mcp_url),
                notify_script_path = ps_single_quoted_path(notify_script_path),
                model_arg = model_arg,
                approval_gate_arg = approval_gate_arg,
            )
        }
        ActSpawnAgentCli::Claude => {
            let Some(debug_path) = files.debug_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Claude debug path",
                ));
            };
            let Some(mcp_config_path) = files.mcp_config_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Claude MCP config path",
                ));
            };
            let Some(hook_settings_path) = files.hook_settings_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Claude hook settings path",
                ));
            };
            let debug_path = ps_single_quoted_path(debug_path);
            let mcp_config_path = ps_single_quoted_path(mcp_config_path);
            let hook_settings_path = ps_single_quoted_path(hook_settings_path);
            let model_arg = params
                .model
                .as_deref()
                .map(|model| format!(",'--model',{}", ps_single_quote(model)))
                .unwrap_or_default();
            // #927: by default route Claude risky tool calls through the human
            // Approvals inbox. `--permission-mode default` consults the static
            // `permissions.allow` rules in the --settings file first (so safe
            // tools never pause), then delegates anything unmatched to the
            // public approval facade, which delegates to the gate and blocks
            // until a human decides.
            // The auto-approve-everything behavior is opt-in via
            // require_approval_gate=false (trusted unattended automation).
            let permission_args = if params.require_approval_gate {
                "'--permission-mode','default','--permission-prompt-tool','mcp__synapse__approval'"
            } else {
                "'--permission-mode','bypassPermissions'"
            };
            format!(
                "$claudeArgs = @('-p'{model_arg},'--verbose','--output-format','stream-json','--input-format','text',{permission_args},'--mcp-config',{mcp_config_path},'--strict-mcp-config','--settings',{hook_settings_path},'--add-dir',{working_dir},'--debug-file',{debug_path})\n\
$prompt | & claude @claudeArgs 1> {stdout_path} 2> {stderr_path}\n\
",
                model_arg = model_arg,
                permission_args = permission_args,
                working_dir = working_dir,
                mcp_config_path = mcp_config_path,
                hook_settings_path = hook_settings_path,
                debug_path = debug_path,
                stdout_path = stdout_path,
                stderr_path = stderr_path,
            )
        }
        ActSpawnAgentCli::LocalModel => {
            let exe = std::env::current_exe().map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "act_spawn_agent failed to resolve current synapse-mcp executable: {error}"
                    ),
                )
            })?;
            let model_ref = params.local_model_ref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: local_model command requested without model_ref",
                )
            })?;
            let timeout_ms = params
                .wait_timeout_ms
                .saturating_add(params.hold_open_ms)
                .max(120_000)
                .min(MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS);
            let mut local_args = format!(
                "@('--mode','local-agent','--local-agent-model',{model_ref},'--local-agent-task-file',{prompt_path},'--local-agent-mcp-url',{mcp_url},'--local-agent-spawn-id',$spawnId,'--local-agent-log-dir',{log_dir},'--local-agent-timeout-ms','{timeout_ms}','--local-agent-hold-open-ms','{hold_open_ms}')",
                model_ref = ps_single_quote(model_ref),
                prompt_path = prompt_path,
                mcp_url = ps_single_quote(&params.mcp_url),
                log_dir = ps_single_quoted_path(&files.log_dir),
                timeout_ms = timeout_ms,
                hold_open_ms = params.hold_open_ms,
            );
            if let Some(target) = &params.target {
                let target_json = serde_json::to_string(target).map_err(|error| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "act_spawn_agent failed to encode local-model target JSON: {error}"
                        ),
                    )
                })?;
                local_args = format!(
                    "$localArgs = {local_args}\n$localArgs += @('--local-agent-target-json',{target_json})",
                    local_args = local_args,
                    target_json = ps_single_quote(&target_json),
                );
            } else {
                local_args = format!("$localArgs = {local_args}", local_args = local_args);
            }
            local_args
                .push_str("\n$localArgs += @('--local-agent-trusted-unattended-exact-contract')");
            format!(
                "{local_args}\n& {exe} @localArgs 1> {stdout_path} 2> {stderr_path}\n",
                local_args = local_args,
                exe = ps_single_quoted_path(&exe),
                stdout_path = stdout_path,
                stderr_path = stderr_path,
            )
        }
    };
    Ok(agent_spawn_wrapper_powershell(
        params,
        &prompt_path,
        &stdout_path,
        &stderr_path,
        &final_message_path,
        &completion_status_path,
        &task_started_path,
        &working_dir,
        &command_body,
    ))
}

fn agent_spawn_wrapper_powershell(
    params: &ActSpawnAgentParams,
    prompt_path: &str,
    stdout_path: &str,
    stderr_path: &str,
    final_message_path: &str,
    completion_status_path: &str,
    task_started_path: &str,
    working_dir: &str,
    command_body: &str,
) -> String {
    let agent_kind = params.effective_cli().ok();
    let cli = ps_single_quote(
        agent_kind
            .map(ActSpawnAgentCli::as_str)
            .unwrap_or("invalid"),
    );
    format!(
        "$ErrorActionPreference = 'Stop'\n\
$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)\n\
$OutputEncoding = $Utf8NoBom\n\
$PSDefaultParameterValues['*:Encoding'] = 'utf8'\n\
try {{ [Console]::OutputEncoding = $Utf8NoBom }} catch {{}}\n\
try {{ [Console]::InputEncoding = $Utf8NoBom }} catch {{}}\n\
$env:PYTHONUTF8 = '1'\n\
$env:PYTHONIOENCODING = 'utf-8'\n\
$env:PYTHONUNBUFFERED = '1'\n\
Remove-Item Env:PYTHONLEGACYWINDOWSSTDIO -ErrorAction SilentlyContinue\n\
$spawnId = {spawn_id_expr}\n\
$spawnCli = {cli}\n\
$spawnWrapperProcessId = $PID\n\
$requestedHoldOpenMs = [int64]{hold_open_ms}\n\
$spawnPromptPath = {prompt_path}\n\
$spawnStdoutPath = {stdout_path}\n\
$spawnStderrPath = {stderr_path}\n\
$spawnFinalMessagePath = {final_message_path}\n\
$spawnCompletionStatusPath = {completion_status_path}\n\
$spawnTaskStartedPath = {task_started_path}\n\
$spawnStartedAtUnixMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()\n\
$spawnExitCode = 1\n\
$spawnTerminalStatus = 'wrapper_error'\n\
$spawnErrorMessage = $null\n\
$spawnFinalMessageSource = $null\n\
$spawnRecoveredFinalMessageWritten = $false\n\
$spawnRecoveredFinalMessageSource = $null\n\
\n\
function Get-SpawnFileLength([string]$Path) {{\n\
    if (Test-Path -LiteralPath $Path) {{ return [int64](Get-Item -LiteralPath $Path).Length }}\n\
    return [int64]0\n\
}}\n\
\n\
function Get-SpawnStdoutSummary([string]$Path) {{\n\
    $lineCount = 0\n\
    $lastEventType = $null\n\
    if (Test-Path -LiteralPath $Path) {{\n\
        foreach ($line in Get-Content -LiteralPath $Path -Encoding UTF8) {{\n\
            if ([string]::IsNullOrWhiteSpace($line)) {{ continue }}\n\
            $lineCount++\n\
            try {{\n\
                $event = $line | ConvertFrom-Json -ErrorAction Stop\n\
                if ($null -ne $event.type) {{ $lastEventType = [string]$event.type }}\n\
                elseif ($null -ne $event.item -and $null -ne $event.item.type) {{ $lastEventType = [string]$event.item.type }}\n\
            }} catch {{}}\n\
        }}\n\
    }}\n\
    return [pscustomobject]@{{ line_count = $lineCount; last_event_type = $lastEventType }}\n\
}}\n\
\n\
function Get-SpawnFinalAssistantTextFromStdout([string]$Path) {{\n\
    $finalText = $null\n\
    if (Test-Path -LiteralPath $Path) {{\n\
        foreach ($line in Get-Content -LiteralPath $Path -Encoding UTF8) {{\n\
            if ([string]::IsNullOrWhiteSpace($line)) {{ continue }}\n\
            try {{\n\
                $event = $line | ConvertFrom-Json -ErrorAction Stop\n\
                if ($null -ne $event.item -and $event.item.type -eq 'agent_message' -and $null -ne $event.item.text) {{\n\
                    $finalText = [string]$event.item.text\n\
                    $script:spawnRecoveredFinalMessageSource = 'stdout_jsonl_agent_message'\n\
                }} elseif ($event.type -eq 'agent_message' -and $null -ne $event.text) {{\n\
                    $finalText = [string]$event.text\n\
                    $script:spawnRecoveredFinalMessageSource = 'stdout_jsonl_agent_message'\n\
                }} elseif ($event.type -eq 'message' -and $event.role -eq 'assistant' -and $null -ne $event.content) {{\n\
                    if ($event.content -is [string]) {{\n\
                        $finalText = [string]$event.content\n\
                        $script:spawnRecoveredFinalMessageSource = 'stdout_jsonl_message'\n\
                    }} else {{\n\
                        $parts = @()\n\
                        foreach ($part in $event.content) {{\n\
                            if ($null -ne $part.text) {{ $parts += [string]$part.text }}\n\
                        }}\n\
                        if ($parts.Count -gt 0) {{\n\
                            $finalText = [string]::Join(\"`n\", $parts)\n\
                            $script:spawnRecoveredFinalMessageSource = 'stdout_jsonl_message'\n\
                        }}\n\
                    }}\n\
                }} elseif ($event.type -eq 'assistant' -and $null -ne $event.message -and $event.message.role -eq 'assistant' -and $null -ne $event.message.content) {{\n\
                    $parts = @()\n\
                    foreach ($part in $event.message.content) {{\n\
                        if ($null -ne $part.text) {{ $parts += [string]$part.text }}\n\
                    }}\n\
                    if ($parts.Count -gt 0) {{\n\
                        $finalText = [string]::Join(\"`n\", $parts)\n\
                        $script:spawnRecoveredFinalMessageSource = 'stdout_jsonl_claude_assistant_message'\n\
                    }}\n\
                }} elseif ($event.type -eq 'result' -and $null -ne $event.result) {{\n\
                    $finalText = [string]$event.result\n\
                    $script:spawnRecoveredFinalMessageSource = 'stdout_jsonl_result'\n\
                }}\n\
            }} catch {{}}\n\
        }}\n\
    }}\n\
    return $finalText\n\
}}\n\
\n\
function Write-SpawnRecoveredFinalMessage([string]$Text) {{\n\
    Set-Content -LiteralPath $spawnFinalMessagePath -Value $Text -Encoding UTF8\n\
    if ([string]::IsNullOrWhiteSpace($script:spawnRecoveredFinalMessageSource)) {{\n\
        $script:spawnFinalMessageSource = 'stdout_jsonl_recovered_final_text'\n\
    }} else {{\n\
        $script:spawnFinalMessageSource = $script:spawnRecoveredFinalMessageSource\n\
    }}\n\
    $script:spawnRecoveredFinalMessageWritten = $true\n\
}}\n\
\n\
function Write-SpawnFallbackFinalMessage([string]$Status, [int]$ExitCode, [string]$ErrorMessage) {{\n\
    $stdoutSummary = Get-SpawnStdoutSummary -Path $spawnStdoutPath\n\
    $fallback = [ordered]@{{\n\
        schema_version = 1\n\
        spawn_id = $spawnId\n\
        cli = $spawnCli\n\
        status = $Status\n\
        exit_code = $ExitCode\n\
        error_message = $ErrorMessage\n\
        message = 'No final assistant response artifact was produced by the spawned agent CLI; this file was written by the Synapse act_spawn_agent wrapper.'\n\
        stdout_path = $spawnStdoutPath\n\
        stderr_path = $spawnStderrPath\n\
        completion_status_path = $spawnCompletionStatusPath\n\
        task_started_path = $spawnTaskStartedPath\n\
        stdout_line_count = $stdoutSummary.line_count\n\
        last_stdout_event_type = $stdoutSummary.last_event_type\n\
    }}\n\
    $fallback | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $spawnFinalMessagePath -Encoding UTF8\n\
    $script:spawnFinalMessageSource = 'wrapper_fallback_json'\n\
}}\n\
\n\
function Write-SpawnCompletionStatus([string]$Status, [int]$ExitCode, [string]$ErrorMessage, [bool]$FallbackFinalMessageWritten) {{\n\
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()\n\
    $elapsed = [int64]($now - $spawnStartedAtUnixMs)\n\
    $stdoutSummary = Get-SpawnStdoutSummary -Path $spawnStdoutPath\n\
    $finalBytes = Get-SpawnFileLength -Path $spawnFinalMessagePath\n\
    $statusObject = [ordered]@{{\n\
        schema_version = 1\n\
        spawn_id = $spawnId\n\
        cli = $spawnCli\n\
        status = $Status\n\
        exit_code = $ExitCode\n\
        error_message = $ErrorMessage\n\
        wrapper_process_id = $spawnWrapperProcessId\n\
        wrapper_started_at_unix_ms = $spawnStartedAtUnixMs\n\
        completed_at_unix_ms = $now\n\
        elapsed_ms = $elapsed\n\
        requested_hold_open_ms = $requestedHoldOpenMs\n\
        hold_open_elapsed_ms_met = ($elapsed -ge $requestedHoldOpenMs)\n\
        final_message_path = $spawnFinalMessagePath\n\
        final_message_bytes = $finalBytes\n\
        final_message_present = ($finalBytes -gt 0)\n\
        final_message_source = $spawnFinalMessageSource\n\
        recovered_final_message_written = $spawnRecoveredFinalMessageWritten\n\
        fallback_final_message_written = $FallbackFinalMessageWritten\n\
        task_started_path = $spawnTaskStartedPath\n\
        task_started_bytes = (Get-SpawnFileLength -Path $spawnTaskStartedPath)\n\
        task_started_present = ((Get-SpawnFileLength -Path $spawnTaskStartedPath) -gt 0)\n\
        stdout_path = $spawnStdoutPath\n\
        stdout_line_count = $stdoutSummary.line_count\n\
        last_stdout_event_type = $stdoutSummary.last_event_type\n\
        stderr_path = $spawnStderrPath\n\
        stderr_bytes = (Get-SpawnFileLength -Path $spawnStderrPath)\n\
    }}\n\
    $statusObject | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $spawnCompletionStatusPath -Encoding UTF8\n\
}}\n\
\n\
function Invoke-SpawnHoldOpen {{\n\
    if ($requestedHoldOpenMs -le 0) {{ return }}\n\
    while ($true) {{\n\
        $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()\n\
        $elapsed = [int64]($now - $spawnStartedAtUnixMs)\n\
        $remaining = [int64]($requestedHoldOpenMs - $elapsed)\n\
        if ($remaining -le 0) {{ return }}\n\
        $sleepMs = [int][Math]::Min($remaining, 60000)\n\
        Start-Sleep -Milliseconds $sleepMs\n\
    }}\n\
}}\n\
\n\
try {{\n\
    Write-SpawnCompletionStatus -Status 'running' -ExitCode -1 -ErrorMessage $null -FallbackFinalMessageWritten:$false\n\
    Set-Location -LiteralPath {working_dir}\n\
    $prompt = Get-Content -Raw -LiteralPath $spawnPromptPath -Encoding UTF8\n\
{command_body}\
    $spawnExitCode = if ($null -eq $LASTEXITCODE) {{ 0 }} else {{ [int]$LASTEXITCODE }}\n\
    $spawnTerminalStatus = if ($spawnExitCode -eq 0) {{ 'completed' }} else {{ 'failed' }}\n\
}} catch {{\n\
    $spawnErrorMessage = $_.Exception.Message\n\
    $spawnExitCode = 1\n\
    $spawnTerminalStatus = 'wrapper_error'\n\
    try {{ Add-Content -LiteralPath $spawnStderrPath -Value (\"SYNAPSE_AGENT_SPAWN_WRAPPER_ERROR: \" + $spawnErrorMessage) -Encoding UTF8 }} catch {{}}\n\
}} finally {{\n\
    $finalBytesBeforeFallback = Get-SpawnFileLength -Path $spawnFinalMessagePath\n\
    $fallbackWritten = $false\n\
    if ($spawnTerminalStatus -eq 'completed' -and $finalBytesBeforeFallback -gt 0) {{\n\
        $finalStatus = 'ok'\n\
        $spawnFinalMessageSource = 'cli_output_file'\n\
    }} elseif ($spawnTerminalStatus -eq 'completed') {{\n\
        $recoveredFinalText = Get-SpawnFinalAssistantTextFromStdout -Path $spawnStdoutPath\n\
        if (-not [string]::IsNullOrWhiteSpace($recoveredFinalText)) {{\n\
            Write-SpawnRecoveredFinalMessage -Text $recoveredFinalText\n\
            $finalStatus = 'ok'\n\
            $spawnErrorMessage = $null\n\
        }} else {{\n\
            $finalStatus = 'missing_final_response'\n\
            $spawnErrorMessage = 'spawned agent CLI exited 0 but did not write final-message.txt and no assistant message was recoverable from stdout.jsonl'\n\
            Write-SpawnFallbackFinalMessage -Status $finalStatus -ExitCode $spawnExitCode -ErrorMessage $spawnErrorMessage\n\
            $fallbackWritten = $true\n\
        }}\n\
    }} elseif ($spawnTerminalStatus -eq 'wrapper_error') {{\n\
        $finalStatus = 'wrapper_error'\n\
        Write-SpawnFallbackFinalMessage -Status $finalStatus -ExitCode $spawnExitCode -ErrorMessage $spawnErrorMessage\n\
        $fallbackWritten = $true\n\
    }} else {{\n\
        $finalStatus = 'failed'\n\
        if ($null -eq $spawnErrorMessage) {{ $spawnErrorMessage = \"spawned agent CLI exited with code $spawnExitCode\" }}\n\
        Write-SpawnFallbackFinalMessage -Status $finalStatus -ExitCode $spawnExitCode -ErrorMessage $spawnErrorMessage\n\
        $fallbackWritten = $true\n\
    }}\n\
    Invoke-SpawnHoldOpen\n\
    Write-SpawnCompletionStatus -Status $finalStatus -ExitCode $spawnExitCode -ErrorMessage $spawnErrorMessage -FallbackFinalMessageWritten:$fallbackWritten\n\
}}\n\
exit $spawnExitCode\n",
        spawn_id_expr = "$env:SYNAPSE_AGENT_SPAWN_ID",
        cli = cli,
        hold_open_ms = params.hold_open_ms,
        prompt_path = prompt_path,
        stdout_path = stdout_path,
        stderr_path = stderr_path,
        final_message_path = final_message_path,
        completion_status_path = completion_status_path,
        task_started_path = task_started_path,
        working_dir = working_dir,
        command_body = command_body,
    )
}

fn ps_single_quoted_path(path: &Path) -> String {
    ps_single_quote(&path.display().to_string())
}

fn ps_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
fn agent_spawn_wait_deadline(wait_timeout_ms: u64) -> Result<Instant, ErrorData> {
    agent_spawn_wait_deadline_from(Instant::now(), wait_timeout_ms)
}

fn agent_spawn_wait_deadline_from(
    start: Instant,
    wait_timeout_ms: u64,
) -> Result<Instant, ErrorData> {
    if wait_timeout_ms > MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_spawn_agent wait_timeout_ms must be <= {MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS}"),
        ));
    }
    start
        .checked_add(Duration::from_millis(wait_timeout_ms))
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "act_spawn_agent wait_timeout_ms {wait_timeout_ms} is too large for this host clock"
                ),
            )
        })
}

fn agent_spawn_deadline_remaining(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
}

async fn sleep_agent_spawn_poll(deadline: Instant) {
    let remaining = agent_spawn_deadline_remaining(deadline);
    if remaining.is_zero() {
        return;
    }
    let poll = Duration::from_millis(AGENT_SPAWN_POLL_INTERVAL_MS);
    tokio::time::sleep(if remaining < poll { remaining } else { poll }).await;
}

fn read_agent_spawn_task_start_artifact(
    files: &AgentSpawnFiles,
    params: &ActSpawnAgentParams,
    agent_kind: ActSpawnAgentCli,
    spawn_id: &str,
    matched: &MatchedSpawnSession,
) -> Result<Option<AgentSpawnTaskStartRead>, serde_json::Value> {
    let bytes = match fs::read(&files.task_started_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(json!({
                "reason": "task_start_artifact_read_failed",
                "task_started_path": files.task_started_path.display().to_string(),
                "error": error.to_string(),
            }));
        }
    };
    if bytes.is_empty() {
        return Err(json!({
            "reason": "task_start_artifact_empty",
            "task_started_path": files.task_started_path.display().to_string(),
        }));
    }
    let json_bytes = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(bytes.as_slice());
    let value = serde_json::from_slice::<Value>(json_bytes).map_err(|error| {
        json!({
            "reason": "task_start_artifact_json_invalid",
            "task_started_path": files.task_started_path.display().to_string(),
            "bytes": bytes.len(),
            "error": error.to_string(),
            "text": String::from_utf8_lossy(&bytes).into_owned(),
        })
    })?;
    let Some(object) = value.as_object() else {
        return Err(json!({
            "reason": "task_start_artifact_not_object",
            "task_started_path": files.task_started_path.display().to_string(),
            "artifact": value,
        }));
    };

    let assigned_prompt_present = params
        .prompt
        .as_deref()
        .is_some_and(|prompt| !prompt.trim().is_empty());
    let expected_path = files.task_started_path.display().to_string();
    let mut validation_errors = Vec::new();

    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        validation_errors.push("schema_version must be 1");
    }
    if object.get("spawn_id").and_then(Value::as_str) != Some(spawn_id) {
        validation_errors.push("spawn_id mismatch");
    }
    if object.get("cli").and_then(Value::as_str) != Some(agent_kind.as_str()) {
        validation_errors.push("cli mismatch");
    }
    if object.get("session_id").and_then(Value::as_str) != Some(matched.session_id.as_str()) {
        validation_errors.push("session_id mismatch");
    }
    if object.get("status").and_then(Value::as_str) != Some("started") {
        validation_errors.push("status must be started");
    }
    if object.get("health_ok").and_then(Value::as_bool) != Some(true) {
        validation_errors.push("health_ok must be true");
    }
    if object.get("target_ok").and_then(Value::as_bool) != Some(true) {
        validation_errors.push("target_ok must be true");
    }
    if object
        .get("assigned_prompt_present")
        .and_then(Value::as_bool)
        != Some(assigned_prompt_present)
    {
        validation_errors.push("assigned_prompt_present mismatch");
    }
    if object.get("task_started_path").and_then(Value::as_str) != Some(expected_path.as_str()) {
        validation_errors.push("task_started_path mismatch");
    }
    let started_at_unix_ms = match object.get("started_at_unix_ms").and_then(Value::as_u64) {
        Some(value) if value > 0 => value,
        _ => {
            validation_errors.push("started_at_unix_ms must be a positive integer");
            0
        }
    };
    let readiness_source = match object.get("readiness_source").and_then(Value::as_str) {
        None => "task_start_artifact",
        Some("task_start_artifact") => "task_start_artifact",
        Some("agent_spawn_task_started_tool") => "agent_spawn_task_started_tool",
        Some(_) => {
            validation_errors.push(
                "readiness_source must be task_start_artifact or agent_spawn_task_started_tool",
            );
            "task_start_artifact"
        }
    };
    if matches!(
        agent_kind,
        ActSpawnAgentCli::Claude | ActSpawnAgentCli::Codex
    ) && readiness_source != "agent_spawn_task_started_tool"
    {
        validation_errors
            .push("readiness_source must be agent_spawn_task_started_tool for claude/codex spawns");
    }

    if !validation_errors.is_empty() {
        return Err(json!({
            "reason": "task_start_artifact_invalid",
            "task_started_path": expected_path,
            "validation_errors": validation_errors,
            "artifact": value,
        }));
    }

    Ok(Some(AgentSpawnTaskStartRead {
        started_at_unix_ms,
        readiness_source,
    }))
}

/// Daemon-OBSERVABLE evidence that a spawned agent began executing. This is
/// diagnostic/session-attribution evidence only; #1225 forbids treating it as
/// final spawn readiness without a live session plus task-start artifact.
fn agent_spawn_observed_task_progress(
    files: &AgentSpawnFiles,
    agent_kind: ActSpawnAgentCli,
) -> Option<&'static str> {
    // The agent emitted a final message: it executed the task to completion.
    if file_len(&files.final_message_path) > 0 {
        return Some("final_message_present");
    }
    // Codex: the app-server control artifact (written by the daemon-controlled
    // runner, not the model) shows a thread established and a turn underway.
    if agent_kind == ActSpawnAgentCli::Codex {
        if let Some(path) = files.codex_app_server_control_path.as_ref() {
            if let Some(value) = read_json_file_lossy(path) {
                let thread_present = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty());
                let turn_status = value
                    .get("turn_status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // `starting`/`app_server_started` predate the agent's thread; any
                // later status means codex created its thread and is executing.
                let turn_underway = !matches!(turn_status, "" | "starting" | "app_server_started");
                if thread_present && turn_underway {
                    return Some("codex_control_thread_established");
                }
            }
        }
    }
    // The agent's stdout shows turn/assistant activity (local-model or codex):
    // it began reasoning or acting, which proves the task is underway.
    if let Some(tail) = tail_file_lossy(&files.stdout_path, AGENT_SPAWN_LOG_TAIL_BYTES) {
        if tail.contains("local.turn.started")
            || tail.contains("local.assistant.message")
            || tail.contains("\"turn.started\"")
            || tail.contains("\"item.completed\"")
        {
            return Some("stdout_turn_activity");
        }
    }
    None
}

fn read_spawned_agent_control_artifact(
    files: &AgentSpawnFiles,
    agent_kind: ActSpawnAgentCli,
) -> Result<Option<SpawnedAgentControlRead>, Value> {
    if agent_kind != ActSpawnAgentCli::Codex {
        return Ok(None);
    }
    let Some(path) = files.codex_app_server_control_path.as_ref() else {
        return Err(json!({ "reason": "codex_control_path_missing" }));
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(json!({
                "reason": "codex_control_artifact_missing",
                "control_path": path.display().to_string(),
            }));
        }
        Err(error) => {
            return Err(json!({
                "reason": "codex_control_artifact_read_failed",
                "control_path": path.display().to_string(),
                "error": error.to_string(),
            }));
        }
    };
    if bytes.is_empty() {
        return Err(json!({
            "reason": "codex_control_artifact_empty",
            "control_path": path.display().to_string(),
        }));
    }
    let json_bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let control =
        serde_json::from_slice::<SpawnedAgentControlRead>(json_bytes).map_err(|error| {
            json!({
                "reason": "codex_control_artifact_json_invalid",
                "control_path": path.display().to_string(),
                "bytes": bytes.len(),
                "error": error.to_string(),
                "text": String::from_utf8_lossy(&bytes).into_owned(),
            })
        })?;
    let mut validation_errors = Vec::new();
    if control.schema_version != 1 {
        validation_errors.push("schema_version mismatch");
    }
    if control.protocol != "codex_app_server_ws" {
        validation_errors.push("protocol mismatch");
    }
    if !control.endpoint.starts_with("ws://127.0.0.1:") {
        validation_errors.push("endpoint must be loopback ws://127.0.0.1:<port>");
    }
    if control.control_path != path.display().to_string() {
        validation_errors.push("control_path mismatch");
    }
    if control.thread_id.as_deref().is_none_or(str::is_empty) {
        validation_errors.push("thread_id missing");
    }
    if control.turn_id.as_deref().is_none_or(str::is_empty) {
        validation_errors.push("turn_id missing");
    }
    if control.app_server_process_id == 0 {
        validation_errors.push("app_server_process_id missing");
    }
    if !validation_errors.is_empty() {
        return Err(json!({
            "reason": "codex_control_artifact_invalid",
            "control_path": path.display().to_string(),
            "validation_errors": validation_errors,
            "control": control,
        }));
    }
    Ok(Some(control))
}

fn read_json_file_lossy(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn agent_spawn_readiness_file_readback(files: &AgentSpawnFiles) -> Value {
    let terminal = agent_spawn_terminal_capture_artifacts(&files.log_dir);
    json!({
        "task_started_path": files.task_started_path.display().to_string(),
        "task_started_bytes": file_len(&files.task_started_path),
        "task_started": read_json_file_lossy(&files.task_started_path),
        "completion_status_path": files.completion_status_path.display().to_string(),
        "completion_status_bytes": file_len(&files.completion_status_path),
        "completion_status": read_json_file_lossy(&files.completion_status_path),
        "stdout_path": files.stdout_path.display().to_string(),
        "stdout_bytes": file_len(&files.stdout_path),
        "stderr_path": files.stderr_path.display().to_string(),
        "stderr_bytes": file_len(&files.stderr_path),
        "final_message_path": files.final_message_path.display().to_string(),
        "final_message_bytes": file_len(&files.final_message_path),
        "codex_app_server_control_path": files
            .codex_app_server_control_path
            .as_ref()
            .map(|path| path.display().to_string()),
        "codex_app_server_control_bytes": files
            .codex_app_server_control_path
            .as_ref()
            .map_or(0, |path| file_len(path)),
        "codex_app_server_control": files
            .codex_app_server_control_path
            .as_ref()
            .and_then(|path| read_json_file_lossy(path)),
        "codex_app_server_events_path": files
            .codex_app_server_events_path
            .as_ref()
            .map(|path| path.display().to_string()),
        "codex_app_server_events_bytes": files
            .codex_app_server_events_path
            .as_ref()
            .map_or(0, |path| file_len(path)),
        "terminal_asciicast_path": terminal.asciicast_path.display().to_string(),
        "terminal_asciicast_bytes": file_len(&terminal.asciicast_path),
        "terminal_capture_status_path": terminal.status_path.display().to_string(),
        "terminal_capture_status_bytes": file_len(&terminal.status_path),
        "terminal_capture_status": read_json_file_lossy(&terminal.status_path),
        "terminal_final_screen_path": terminal.final_screen_path.display().to_string(),
        "terminal_final_screen_bytes": file_len(&terminal.final_screen_path),
        "terminal_input_audit_path": terminal.input_audit_path.display().to_string(),
        "terminal_input_audit_bytes": file_len(&terminal.input_audit_path),
    })
}

#[derive(Clone, Debug)]
struct SpawnSessionCandidateRead {
    registry: SessionRegistryRead,
    active_target: Option<TargetWire>,
}

fn spawn_target_wire_from_session_target(target: &SessionTarget) -> TargetWire {
    match target {
        SessionTarget::Window { hwnd } => TargetWire::Window { window_hwnd: *hwnd },
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => TargetWire::Cdp {
            window_hwnd: *window_hwnd,
            cdp_target_id: cdp_target_id.clone(),
        },
    }
}

fn spawn_session_observation_from_read(
    candidate: &SpawnSessionCandidateRead,
    readiness: Value,
) -> Value {
    json!({
        "session_id": candidate.registry.session_id,
        "agent_kind": candidate.registry.agent_kind,
        "client_name": candidate.registry.client_name,
        "client_version": candidate.registry.client_version,
        "lifecycle": candidate.registry.lifecycle,
        "started_at_unix_ms": candidate.registry.started_at_unix_ms,
        "last_seen_unix_ms": candidate.registry.last_seen_unix_ms,
        "last_seen_ms_ago": candidate.registry.last_seen_ms_ago,
        "last_action": candidate.registry.last_action,
        "active_target": candidate.active_target,
        "readiness": readiness,
    })
}

#[cfg(test)]
fn spawn_session_candidate_readiness(
    summary: &super::session_tools::SessionSummary,
    agent_kind: ActSpawnAgentCli,
    target: Option<&ActSpawnAgentTarget>,
    before_session_ids: &BTreeSet<String>,
    launched_at_unix_ms: u64,
) -> Value {
    spawn_session_candidate_readiness_from_read(
        &summary.registry,
        summary.active_target.as_ref(),
        agent_kind,
        target,
        before_session_ids,
        launched_at_unix_ms,
    )
}

fn spawn_session_candidate_readiness_from_read(
    registry: &SessionRegistryRead,
    active_target: Option<&TargetWire>,
    agent_kind: ActSpawnAgentCli,
    target: Option<&ActSpawnAgentTarget>,
    before_session_ids: &BTreeSet<String>,
    launched_at_unix_ms: u64,
) -> Value {
    if before_session_ids.contains(&registry.session_id) {
        return json!({
            "ready": false,
            "reason": "session_existed_before_spawn",
            "expected": "new distinct MCP session id",
        });
    }
    if registry.lifecycle != "live" {
        return json!({
            "ready": false,
            "reason": "session_not_live",
            "lifecycle": registry.lifecycle,
            "closed_at_unix_ms": registry.closed_at_unix_ms,
            "last_reason_code": registry.last_reason_code,
        });
    }
    if registry.started_at_unix_ms + 2_000 < launched_at_unix_ms {
        return json!({
            "ready": false,
            "reason": "session_started_before_spawn_window",
            "started_at_unix_ms": registry.started_at_unix_ms,
            "launched_at_unix_ms": launched_at_unix_ms,
            "allowed_clock_skew_ms": 2000,
        });
    }
    if !registry_matches_cli(registry, agent_kind) {
        return json!({
            "ready": false,
            "reason": "session_cli_mismatch",
            "expected_cli": agent_kind.as_str(),
            "agent_kind": registry.agent_kind,
            "client_name": registry.client_name,
        });
    }
    if let Some(expected) = target {
        if matches_target_wire(active_target, expected) {
            json!({
                "ready": true,
                "reason": "target_bound",
            })
        } else {
            json!({
                "ready": false,
                "reason": "target_mismatch",
                "expected_target": expected,
                "active_target": active_target,
            })
        }
    } else if registry
        .last_action
        .as_deref()
        .is_some_and(|action| action.starts_with("tools/call:"))
    {
        json!({
            "ready": true,
            "reason": "tool_call_observed",
            "last_action": registry.last_action,
        })
    } else {
        json!({
            "ready": false,
            "reason": "tool_call_not_observed",
            "last_action": registry.last_action,
            "expected": "last_action beginning with tools/call:",
        })
    }
}

fn task_start_session_id_for_spawn(files: &AgentSpawnFiles, spawn_id: &str) -> Option<String> {
    let value = read_json_file_lossy(&files.task_started_path)?;
    let object_spawn_id = value.get("spawn_id").and_then(Value::as_str)?;
    if object_spawn_id != spawn_id {
        return None;
    }
    value
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
fn spawn_session_identity_matches(
    summary: &super::session_tools::SessionSummary,
    agent_kind: ActSpawnAgentCli,
    before_session_ids: &BTreeSet<String>,
    launched_at_unix_ms: u64,
) -> bool {
    spawn_session_identity_matches_from_read(
        &summary.registry,
        agent_kind,
        before_session_ids,
        launched_at_unix_ms,
    )
}

fn spawn_session_identity_matches_from_read(
    registry: &SessionRegistryRead,
    agent_kind: ActSpawnAgentCli,
    before_session_ids: &BTreeSet<String>,
    launched_at_unix_ms: u64,
) -> bool {
    !before_session_ids.contains(&registry.session_id)
        && registry.lifecycle == "live"
        && registry.started_at_unix_ms + 2_000 >= launched_at_unix_ms
        && registry_matches_cli(registry, agent_kind)
}

fn registry_matches_cli(registry: &SessionRegistryRead, cli: ActSpawnAgentCli) -> bool {
    let cli = cli.as_str();
    if registry.agent_kind == cli {
        return true;
    }
    registry
        .client_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains(cli))
}

fn matches_target_wire(wire: Option<&super::TargetWire>, expected: &ActSpawnAgentTarget) -> bool {
    match (wire, expected) {
        (
            Some(super::TargetWire::Window {
                window_hwnd: actual,
            }),
            ActSpawnAgentTarget::Window {
                window_hwnd: expected,
            },
        ) => actual == expected,
        (
            Some(super::TargetWire::Cdp {
                window_hwnd: actual_hwnd,
                cdp_target_id: actual_target,
            }),
            ActSpawnAgentTarget::Cdp {
                window_hwnd: expected_hwnd,
                cdp_target_id: expected_target,
            },
        ) => actual_hwnd == expected_hwnd && actual_target == expected_target,
        _ => false,
    }
}

fn tail_file_lossy(path: &Path, limit_bytes: usize) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(limit_bytes);
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

fn process_has_exited(pid: u32) -> bool {
    !process_exists(pid)
}

fn process_exists(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
    system.process(pid).is_some()
}

fn discover_agent_process_id(launcher_pid: u32, cli: ActSpawnAgentCli) -> Option<u32> {
    use sysinfo::{ProcessesToUpdate, System};

    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let descendants = descendant_process_ids(&system, launcher_pid);
    let cli = cli.as_str();
    descendants
        .iter()
        .copied()
        .find(|pid| {
            let Some(process) = system.process(sysinfo::Pid::from_u32(*pid)) else {
                return false;
            };
            let name = process.name().to_string_lossy().to_ascii_lowercase();
            let cmd = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            name.contains(cli) || cmd.contains(cli)
        })
        .or_else(|| descendants.first().copied())
}

fn descendant_process_ids(system: &sysinfo::System, root_pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(parent) = stack.pop() {
        for (pid, process) in system.processes() {
            if process.parent().map(|value| value.as_u32()) == Some(parent) {
                let child = pid.as_u32();
                descendants.push(child);
                stack.push(child);
            }
        }
    }
    descendants
}

fn agent_spawn_tool_error(
    code: &'static str,
    message: &'static str,
    data: serde_json::Value,
) -> ErrorData {
    tracing::warn!(code, "M4 agent spawn tool error: {message}");
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

fn launch_lifecycle_tool_error(message: &'static str, data: serde_json::Value) -> ErrorData {
    tracing::warn!(
        code = error_codes::TOOL_INTERNAL_ERROR,
        "M4 launch lifecycle tool error: {message}"
    );
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spawn_params() -> ActSpawnAgentParams {
        ActSpawnAgentParams {
            cli: Some(ActSpawnAgentCli::Codex),
            kind: None,
            model: None,
            model_ref: None,
            prompt: Some("write a report".to_owned()),
            target: None,
            working_dir: None,
            mcp_url: "http://127.0.0.1:7700/mcp".to_owned(),
            wait_timeout_ms: 30_000,
            hold_open_ms: 1234,
            require_approval_gate: true,
            template_id: None,
            template_version: None,
            template_config_hash: None,
        }
    }

    fn test_spawn_session_summary(
        lifecycle: &str,
        last_action: Option<&str>,
    ) -> crate::server::session_tools::SessionSummary {
        crate::server::session_tools::SessionSummary {
            registry: crate::server::session_registry::SessionRegistryRead {
                session_id: "spawned-session".to_owned(),
                transport: "http".to_owned(),
                client_name: Some("codex-mcp-client".to_owned()),
                client_version: Some("test".to_owned()),
                protocol_version: Some("test".to_owned()),
                agent_kind: "codex".to_owned(),
                lifecycle: lifecycle.to_owned(),
                started_at_unix_ms: 1_000,
                last_seen_unix_ms: 1_500,
                last_seen_ms_ago: 50,
                stale_after_ms: 300_000,
                closed_at_unix_ms: (lifecycle != "live").then_some(1_550),
                last_action: last_action.map(ToOwned::to_owned),
                last_reason_code: (lifecycle != "live")
                    .then_some("http_session_store_deleted".to_owned()),
                spawned_agent: None,
            },
            active_target: None,
            agent_logical_foreground:
                crate::server::session_tools::AgentLogicalForegroundReadback {
                    source_of_truth: "test".to_owned(),
                    session_id: "spawned-session".to_owned(),
                    status: "missing".to_owned(),
                    target: None,
                    persisted_row_key: None,
                    no_human_os_foreground_fallback: true,
                    missing_reason: Some("test".to_owned()),
                },
            foreground_lane: crate::server::session_tools::ForegroundLaneReadback {
                source_of_truth: "test".to_owned(),
                session_id: "spawned-session".to_owned(),
                status: "missing".to_owned(),
                capacity_model: "test".to_owned(),
                capacity_exhausted: false,
                lane_kind: None,
                target_key: None,
                target: None,
                target_claim: None,
                owner_session_id: None,
                explicit_real_foreground_lease: false,
                no_human_os_foreground_fallback: true,
                missing_reason: Some("test".to_owned()),
            },
            target_claims: Vec::new(),
            persisted_cdp_target_owners: Vec::new(),
            lease: crate::server::session_tools::SessionLeaseReadback {
                held: false,
                owner_session_id: None,
                is_owner: false,
                acquired_at_ms_ago: None,
                renewed_at_ms_ago: None,
                ttl_ms: None,
                expires_in_ms: None,
            },
            agent_state: None,
            attention_class: crate::server::agent_state::AgentAttentionClass::None,
        }
    }

    fn test_local_model_row(api_key_env_var: Option<&str>) -> LocalModelRegistryRow {
        LocalModelRegistryRow {
            schema_version: 1,
            row_key: "local_model_registry/v1/model/deadbeef".to_owned(),
            name: "deepseek".to_owned(),
            base_url: "https://api.deepseek.com".to_owned(),
            model_id: "deepseek-v4-flash".to_owned(),
            api_shape: LocalModelApiShape::OpenAiChatCompletions,
            runtime_preset:
                crate::m3::local_models::LocalModelRuntimePreset::DeepSeekV4FlashNonThinking,
            context_length: Some(1_000_000),
            max_tools: Some(128),
            notes: None,
            enabled: true,
            allow_non_loopback: true,
            api_key_env_var: api_key_env_var.map(ToOwned::to_owned),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            created_by_session: "session-test".to_owned(),
            updated_by_session: "session-test".to_owned(),
            last_probe: None,
            has_api_key_secret: false,
        }
    }

    fn resolver_test_db() -> (tempfile::TempDir, std::sync::Arc<synapse_storage::Db>) {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let db = std::sync::Arc::new(
            synapse_storage::Db::open(dir.path(), synapse_core::SCHEMA_VERSION)
                .expect("open temp db"),
        );
        (dir, db)
    }

    fn shell_params(operation: ShellOperation) -> ShellParams {
        ShellParams {
            operation,
            command: None,
            args: None,
            working_dir: None,
            env: None,
            timeout_ms: None,
            execution_mode: None,
            durable_timeout_ms: None,
            idempotency_key: None,
            job_id: None,
            tail_bytes: None,
        }
    }

    fn process_params(operation: ProcessOperation) -> ProcessParams {
        ProcessParams {
            operation,
            target: None,
            args: None,
            working_dir: None,
            env: None,
            wait_for_window_title_regex: None,
            timeout_ms: None,
            idempotency_key: None,
            cdp_debug: None,
            force_renderer_accessibility: None,
            windows_console_window_state: None,
            desktop: None,
            pid: None,
            process_name_contains: None,
            command_line_contains: None,
            limit: None,
            include_command_line: None,
        }
    }

    fn tool_param_error_code(error: &ErrorData) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    fn sanitized_tool_input_schema(tool_name: &str) -> Value {
        let tools = crate::server::schema_sanitize::sanitize_tools(
            crate::server::SynapseService::tool_router().list_all(),
        );
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} tool missing"));
        Value::Object((*tool.input_schema).clone())
    }

    fn shell_schema_variant<'a>(schema: &'a Value, operation: &str) -> &'a Value {
        schema["oneOf"]
            .as_array()
            .unwrap_or_else(|| panic!("shell schema oneOf missing"))
            .iter()
            .find(|variant| variant["properties"]["operation"]["const"] == operation)
            .unwrap_or_else(|| panic!("shell schema operation={operation} variant missing"))
    }

    fn schema_property_names(schema: &Value) -> BTreeSet<String> {
        schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("schema properties missing"))
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn shell_facade_rejects_unknown_operation_enum() {
        let error = serde_json::from_value::<ShellParams>(json!({"operation": "not_real"}))
            .expect_err("unknown shell operation must fail closed");
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn shell_facade_public_schema_is_operation_specific() {
        let schema = sanitized_tool_input_schema("shell");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        assert_eq!(
            schema["properties"]["durable_timeout_ms"]["description"],
            "run only. Applies if run creates a durable/background job; start uses timeout_ms for its durable lifetime cap."
        );

        let variants = schema["oneOf"]
            .as_array()
            .expect("shell schema oneOf present");
        assert_eq!(variants.len(), 4);
        for variant in variants {
            assert_eq!(variant["type"], "object");
            assert_eq!(variant["additionalProperties"], Value::Bool(false));
        }

        let run_fields = schema_property_names(shell_schema_variant(&schema, "run"));
        assert!(run_fields.contains("durable_timeout_ms"));
        assert!(run_fields.contains("idempotency_key"));
        assert!(run_fields.contains("execution_mode"));
        assert!(!run_fields.contains("job_id"));
        assert!(!run_fields.contains("tail_bytes"));

        let start_fields = schema_property_names(shell_schema_variant(&schema, "start"));
        assert!(start_fields.contains("timeout_ms"));
        assert!(start_fields.contains("job_id"));
        assert!(!start_fields.contains("durable_timeout_ms"));
        assert!(!start_fields.contains("idempotency_key"));
        assert!(!start_fields.contains("execution_mode"));
        assert!(!start_fields.contains("tail_bytes"));

        let status_fields = schema_property_names(shell_schema_variant(&schema, "status"));
        assert_eq!(
            status_fields,
            BTreeSet::from([
                "job_id".to_owned(),
                "operation".to_owned(),
                "tail_bytes".to_owned()
            ])
        );

        let cancel_fields = schema_property_names(shell_schema_variant(&schema, "cancel"));
        assert_eq!(
            cancel_fields,
            BTreeSet::from(["job_id".to_owned(), "operation".to_owned()])
        );
    }

    #[test]
    fn shell_facade_validates_operation_specific_fields() {
        let empty_run = shell_run_params(ShellParams {
            command: Some(" ".to_owned()),
            ..shell_params(ShellOperation::Run)
        })
        .expect_err("run requires non-empty command");
        assert_eq!(
            tool_param_error_code(&empty_run),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );

        let wrong_run = shell_run_params(ShellParams {
            command: Some("powershell.exe".to_owned()),
            job_id: Some("job-from-status".to_owned()),
            ..shell_params(ShellOperation::Run)
        })
        .expect_err("run must reject status/cancel-only job_id");
        assert_eq!(
            wrong_run
                .data
                .as_ref()
                .and_then(|data| data.get("operation"))
                .and_then(Value::as_str),
            Some("run")
        );

        let missing_status = shell_status_params(shell_params(ShellOperation::Status))
            .expect_err("status requires job_id");
        assert_eq!(
            tool_param_error_code(&missing_status),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );

        let wrong_status = shell_status_params(ShellParams {
            command: Some("powershell.exe".to_owned()),
            job_id: Some("job-1".to_owned()),
            ..shell_params(ShellOperation::Status)
        })
        .expect_err("status rejects run-only command");
        assert_eq!(
            wrong_status
                .data
                .as_ref()
                .and_then(|data| data.get("operation"))
                .and_then(Value::as_str),
            Some("status")
        );
    }

    #[test]
    fn process_facade_rejects_unknown_operation_enum() {
        let error = serde_json::from_value::<ProcessParams>(json!({"operation": "not_real"}))
            .expect_err("unknown process operation must fail closed");
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn process_facade_validates_operation_specific_fields() {
        let missing_launch = process_launch_params(process_params(ProcessOperation::Launch))
            .expect_err("launch requires target");
        assert_eq!(
            tool_param_error_code(&missing_launch),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );

        let wrong_launch = process_launch_params(ProcessParams {
            target: Some("notepad.exe".to_owned()),
            pid: Some(1234),
            ..process_params(ProcessOperation::Launch)
        })
        .expect_err("launch rejects list/history-only pid");
        assert_eq!(
            wrong_launch
                .data
                .as_ref()
                .and_then(|data| data.get("operation"))
                .and_then(Value::as_str),
            Some("launch")
        );

        let wrong_history = validate_process_query_params(
            ProcessOperation::History,
            &ProcessParams {
                target: Some("notepad.exe".to_owned()),
                ..process_params(ProcessOperation::History)
            },
        )
        .expect_err("history rejects launch target field");
        assert_eq!(
            wrong_history
                .data
                .as_ref()
                .and_then(|data| data.get("operation"))
                .and_then(Value::as_str),
            Some("history")
        );

        let unbounded_history = validate_process_query_params(
            ProcessOperation::History,
            &ProcessParams {
                limit: Some(PROCESS_HISTORY_MAX_LIMIT + 1),
                ..process_params(ProcessOperation::History)
            },
        )
        .expect_err("history rejects limits above the explicit cap");
        assert_eq!(
            tool_param_error_code(&unbounded_history),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }

    #[test]
    fn process_facade_delegate_error_preserves_code_and_adds_context() {
        let low_level = ErrorData::new(
            ErrorCode(-32099),
            "target missing",
            Some(json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "target": "C:\\missing.exe",
            })),
        );
        let error = process_facade_delegate_error(
            ProcessOperation::Launch,
            "C:\\missing.exe",
            low_level,
            "fix target",
        );
        let data = error.data.as_ref().expect("facade data");
        assert_eq!(
            data.get("code").and_then(Value::as_str),
            Some(error_codes::ACTION_TARGET_INVALID)
        );
        assert_eq!(
            data.get("operation").and_then(Value::as_str),
            Some("launch")
        );
        assert_eq!(
            data.get("source_id").and_then(Value::as_str),
            Some("C:\\missing.exe")
        );
        assert_eq!(
            data.get("remediation").and_then(Value::as_str),
            Some("fix target")
        );
        assert!(data.get("cause").is_some());
    }

    #[test]
    fn resolve_spawn_api_key_none_when_row_absent_or_keyless() {
        let (_dir, db) = resolver_test_db();
        // Non-local-model spawn: nothing to resolve.
        assert!(
            resolve_spawn_local_model_api_key(&db, None)
                .expect("no row resolves cleanly")
                .is_none()
        );
        // Loopback model with no declared key (e.g. Ollama): nothing to forward.
        let row = test_local_model_row(None);
        assert!(
            resolve_spawn_local_model_api_key(&db, Some(&row))
                .expect("keyless row resolves cleanly")
                .is_none()
        );
    }

    #[test]
    fn resolve_spawn_api_key_forwards_value_when_present() {
        let (_dir, db) = resolver_test_db();
        // Unique env var name so parallel tests never collide on process env.
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_PRESENT";
        // SAFETY: single-threaded within this test; unique key avoids races.
        unsafe { std::env::set_var(env_var, "sk-secret-value") };
        let row = test_local_model_row(Some(env_var));
        let resolved = resolve_spawn_local_model_api_key(&db, Some(&row))
            .expect("present key resolves")
            .expect("present key yields a value");
        assert_eq!(resolved.0, env_var);
        assert_eq!(resolved.1, "sk-secret-value");
        unsafe { std::env::remove_var(env_var) };
    }

    #[cfg(windows)]
    #[test]
    fn resolve_spawn_api_key_prefers_encrypted_secret_store_over_env() {
        // FSV: a DPAPI-encrypted stored key takes priority over the process env
        // and round-trips through CryptProtectData/CryptUnprotectData.
        let (_dir, db) = resolver_test_db();
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_PRECEDENCE";
        unsafe { std::env::set_var(env_var, "env-fallback-value") };
        let row = test_local_model_row(Some(env_var));
        crate::m3::local_models::put_model_secret(&db, &row.name, "stored-secret-value", "test")
            .expect("store secret");
        let resolved = resolve_spawn_local_model_api_key(&db, Some(&row))
            .expect("secret resolves")
            .expect("secret yields a value");
        assert_eq!(resolved.0, env_var);
        assert_eq!(
            resolved.1, "stored-secret-value",
            "stored secret must win over the env var"
        );
        unsafe { std::env::remove_var(env_var) };
    }

    #[test]
    fn resolve_spawn_api_key_refuses_loudly_when_missing() {
        let (_dir, db) = resolver_test_db();
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_MISSING";
        unsafe { std::env::remove_var(env_var) };
        let row = test_local_model_row(Some(env_var));
        let err = resolve_spawn_local_model_api_key(&db, Some(&row))
            .expect_err("missing key must refuse the spawn loudly");
        let data = err.data.expect("refusal carries structured detail");
        assert_eq!(data["code"], error_codes::MODEL_API_KEY_MISSING);
        assert_eq!(data["reason"], "local_model_api_key_missing");
        assert_eq!(data["detail"]["api_key_env_var"], env_var);
        assert!(
            data["detail"]["resolver_data"]["api_key_env_var"]
                .as_str()
                .unwrap_or_default()
                .contains(env_var),
            "resolver detail names the missing env var"
        );
    }

    #[test]
    fn resolve_spawn_api_key_refuses_when_value_blank() {
        let (_dir, db) = resolver_test_db();
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_BLANK";
        unsafe { std::env::set_var(env_var, "   ") };
        let row = test_local_model_row(Some(env_var));
        let err = resolve_spawn_local_model_api_key(&db, Some(&row))
            .expect_err("blank key is treated as missing");
        let data = err.data.expect("refusal carries structured detail");
        assert_eq!(data["code"], error_codes::MODEL_API_KEY_MISSING);
        unsafe { std::env::remove_var(env_var) };
    }

    #[test]
    fn spawn_prompt_names_powershell_contract() {
        let dir = Path::new(r"C:\code\Synapse");
        let task_started_path = dir.join("task-started.json");
        let task_started_script_path = dir.join("write-task-started.ps1");
        let prompt = build_agent_spawn_prompt(
            "agent-spawn-test",
            &test_spawn_params(),
            dir,
            &task_started_path,
            &task_started_script_path,
        )
        .expect("build spawn prompt");

        assert!(prompt.contains("PowerShell on Windows, not Bash"));
        assert!(prompt.contains("Do not use Bash heredocs"));
        assert!(prompt.contains("@'"));
        assert!(prompt.contains("Start-Sleep -Milliseconds 1234"));
        assert!(prompt.contains("task-start readiness artifact"));
        assert!(prompt.contains("task-started.json"));
        assert!(prompt.contains("\"operation\":\"task_started\""));
        assert!(prompt.contains("do not use a helper script or direct file write"));
        assert!(!prompt.contains("agent_spawn_task_started"));
        assert!(!prompt.contains("write-task-started.ps1"));

        let prompt_json = serde_json::to_string(&prompt).expect("prompt serializes");
        assert!(!prompt_json.contains("write-task-started.ps1"));
        assert!(!prompt_json.contains("session_list"));
        assert!(!prompt_json.contains("get_target"));
        assert!(!prompt_json.contains("set_target"));
        assert!(prompt.contains("session facade"));
        assert!(prompt.contains("target facade"));
    }

    #[test]
    fn agent_spawn_files_do_not_materialize_task_started_helper() {
        struct EnvRestore {
            key: &'static str,
            value: Option<std::ffi::OsString>,
        }
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                unsafe {
                    match &self.value {
                        Some(value) => std::env::set_var(self.key, value),
                        None => std::env::remove_var(self.key),
                    }
                }
            }
        }

        let local_appdata = tempfile::TempDir::new().expect("create local appdata");
        let _restore = EnvRestore {
            key: "LOCALAPPDATA",
            value: std::env::var_os("LOCALAPPDATA"),
        };
        unsafe { std::env::set_var("LOCALAPPDATA", local_appdata.path()) };

        let working_dir = tempfile::TempDir::new().expect("create working dir");
        let codex_files = prepare_agent_spawn_files(
            "agent-spawn-codex",
            &test_spawn_params(),
            working_dir.path(),
        )
        .expect("prepare codex spawn files");

        assert!(
            !codex_files.task_started_script_path.exists(),
            "Codex readiness must go through agent operation=task_started, not a helper script"
        );
        assert!(
            codex_files
                .codex_app_server_runner_path
                .as_ref()
                .is_some_and(|path| path.exists()),
            "Codex still writes the app-server runner"
        );

        let mut claude_params = test_spawn_params();
        claude_params.cli = Some(ActSpawnAgentCli::Claude);
        let claude_files =
            prepare_agent_spawn_files("agent-spawn-claude", &claude_params, working_dir.path())
                .expect("prepare claude spawn files");

        assert!(
            !claude_files.task_started_script_path.exists(),
            "Claude readiness must go through agent operation=task_started, not a helper script"
        );
        let claude_settings_path = claude_files
            .hook_settings_path
            .as_ref()
            .expect("Claude hook settings path");
        let claude_settings =
            fs::read_to_string(claude_settings_path).expect("read Claude hook settings");
        assert!(
            !claude_settings.contains("write-task-started.ps1"),
            "Claude hook settings must not allowlist a task-start helper script"
        );
    }

    #[test]
    fn agent_spawn_wait_deadline_rejects_impossible_timeout() {
        let error = agent_spawn_wait_deadline(MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS + 1)
            .expect_err("over-limit deadline must fail closed");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::TOOL_PARAMS_INVALID))
        );
        assert!(error.message.contains("must be <="));
    }

    #[test]
    fn agent_spawn_wait_deadline_uses_supplied_phase_start() {
        let start = Instant::now();
        let deadline = agent_spawn_wait_deadline_from(start, 1_234).expect("valid phase deadline");

        assert_eq!(deadline.duration_since(start), Duration::from_millis(1_234));
    }

    #[test]
    fn spawn_session_readiness_rejects_closed_session_even_after_tool_call() {
        let summary = test_spawn_session_summary("closed", Some("tools/call:session_list"));
        let before_session_ids = BTreeSet::new();
        let readiness = spawn_session_candidate_readiness(
            &summary,
            ActSpawnAgentCli::Codex,
            None,
            &before_session_ids,
            1_000,
        );

        assert_eq!(readiness.get("ready").and_then(Value::as_bool), Some(false));
        assert_eq!(
            readiness.get("reason").and_then(Value::as_str),
            Some("session_not_live")
        );
        assert_eq!(
            readiness.get("last_reason_code").and_then(Value::as_str),
            Some("http_session_store_deleted")
        );
        assert!(
            !spawn_session_identity_matches(
                &summary,
                ActSpawnAgentCli::Codex,
                &before_session_ids,
                1_000,
            ),
            "closed sessions must not bind through the task-start or observed-progress paths"
        );
    }

    #[test]
    fn spawn_session_readiness_accepts_live_tool_call() {
        let summary = test_spawn_session_summary("live", Some("tools/call:session_list"));
        let before_session_ids = BTreeSet::new();
        let readiness = spawn_session_candidate_readiness(
            &summary,
            ActSpawnAgentCli::Codex,
            None,
            &before_session_ids,
            1_000,
        );

        assert_eq!(readiness.get("ready").and_then(Value::as_bool), Some(true));
        assert_eq!(
            readiness.get("reason").and_then(Value::as_str),
            Some("tool_call_observed")
        );
        assert!(spawn_session_identity_matches(
            &summary,
            ActSpawnAgentCli::Codex,
            &before_session_ids,
            1_000,
        ));
    }

    #[test]
    fn task_start_rebinds_from_closed_codex_bootstrap_session_to_live_reconnect() {
        let service = SynapseService::new();
        let dir = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(dir.path());
        let params = test_spawn_params();
        let launch_ms = unix_time_ms_now();
        let spawn_id = "agent-spawn-rebind-test";
        let old_session_id = "codex-bootstrap-closed";
        let new_session_id = "codex-live-reconnect";
        let mut before_session_ids = BTreeSet::new();
        before_session_ids.insert("operator-session".to_owned());

        let spawned_metadata = || SpawnedAgentRead {
            spawn_id: spawn_id.to_owned(),
            cli: "codex".to_owned(),
            launcher_process_id: 4242,
            agent_process_id: Some(5252),
            started_by_session_id: Some("operator-session".to_owned()),
            launched_at_unix_ms: launch_ms,
            launch_target: "none".to_owned(),
            log_dir: dir.path().display().to_string(),
            template_id: None,
            template_version: None,
            control: None,
        };

        {
            let mut registry = service
                .session_registry_ref()
                .lock()
                .expect("session registry");
            registry.record_spawned_agent(old_session_id, spawned_metadata(), launch_ms + 10);
            registry.record_seen(
                old_session_id,
                Some("tools/list".to_owned()),
                launch_ms + 20,
            );
            registry.record_closed_with_reason(
                old_session_id,
                launch_ms + 30,
                Some("http_session_store_deleted"),
            );
            registry.record_spawned_agent(new_session_id, spawned_metadata(), launch_ms + 40);
            registry.record_seen(
                new_session_id,
                Some("tools/list".to_owned()),
                launch_ms + 50,
            );
        }

        let liveness_error = json!({
            "reason": "spawned_session_not_live",
            "session_id": old_session_id,
        });
        let no_progress = service
            .rebind_spawned_agent_session_for_task_start(
                &params,
                ActSpawnAgentCli::Codex,
                spawn_id,
                &before_session_ids,
                launch_ms,
                4242,
                &files,
                &liveness_error,
            )
            .expect("rebind read succeeds");
        assert!(
            no_progress.is_none(),
            "a live replacement without daemon-observed task progress must not be rebound"
        );

        fs::write(
            files.codex_app_server_control_path.as_ref().unwrap(),
            serde_json::to_vec(&json!({
                "thread_id": "019ef79d-abb2-71e0-9e91-3d1f5a34dd9b",
                "turn_status": "inProgress"
            }))
            .expect("encode codex control"),
        )
        .expect("write codex control");
        let rebound = service
            .rebind_spawned_agent_session_for_task_start(
                &params,
                ActSpawnAgentCli::Codex,
                spawn_id,
                &before_session_ids,
                launch_ms,
                4242,
                &files,
                &liveness_error,
            )
            .expect("rebind read succeeds")
            .expect("progress-backed replacement session binds");

        assert_eq!(rebound.session_id, new_session_id);
    }

    #[test]
    fn task_start_artifact_validation_rejects_wrong_session() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        fs::write(
            &files.task_started_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "spawn_id": "agent-spawn-test",
                "cli": "codex",
                "session_id": "wrong-session",
                "status": "started",
                "health_ok": true,
                "target_ok": true,
                "assigned_prompt_present": true,
                "task_started_path": files.task_started_path.display().to_string(),
                "readiness_source": "agent_spawn_task_started_tool",
                "started_at_unix_ms": 1234
            }))
            .expect("encode task start"),
        )
        .expect("write task start");
        let matched = MatchedSpawnSession {
            session_id: "expected-session".to_owned(),
            registered_at_unix_ms: 1000,
            agent_process_id: Some(42),
        };
        let error = read_agent_spawn_task_start_artifact(
            &files,
            &test_spawn_params(),
            ActSpawnAgentCli::Codex,
            "agent-spawn-test",
            &matched,
        )
        .expect_err("wrong session must fail");

        assert_eq!(
            error.get("reason").and_then(Value::as_str),
            Some("task_start_artifact_invalid")
        );
        assert!(
            error
                .get("validation_errors")
                .and_then(Value::as_array)
                .expect("validation errors")
                .iter()
                .any(|entry| entry.as_str() == Some("session_id mismatch"))
        );
    }

    #[test]
    fn task_start_artifact_validation_rejects_legacy_direct_artifact_for_codex() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        fs::write(
            &files.task_started_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "spawn_id": "agent-spawn-test",
                "cli": "codex",
                "session_id": "expected-session",
                "status": "started",
                "health_ok": true,
                "target_ok": true,
                "assigned_prompt_present": true,
                "task_started_path": files.task_started_path.display().to_string(),
                "started_at_unix_ms": 1234
            }))
            .expect("encode task start"),
        )
        .expect("write task start");
        let matched = MatchedSpawnSession {
            session_id: "expected-session".to_owned(),
            registered_at_unix_ms: 1000,
            agent_process_id: Some(42),
        };
        let error = read_agent_spawn_task_start_artifact(
            &files,
            &test_spawn_params(),
            ActSpawnAgentCli::Codex,
            "agent-spawn-test",
            &matched,
        )
        .expect_err("Codex must not accept direct task-start artifacts");

        assert_eq!(
            error.get("reason").and_then(Value::as_str),
            Some("task_start_artifact_invalid")
        );
        assert!(
            error
                .get("validation_errors")
                .and_then(Value::as_array)
                .expect("validation errors")
                .iter()
                .any(|entry| entry.as_str()
                    == Some(
                        "readiness_source must be agent_spawn_task_started_tool for claude/codex spawns"
                    ))
        );
    }

    #[test]
    fn task_start_artifact_validation_accepts_legacy_local_model_artifact() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        fs::write(
            &files.task_started_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "spawn_id": "agent-spawn-test",
                "cli": "local-model",
                "session_id": "expected-session",
                "status": "started",
                "health_ok": true,
                "target_ok": true,
                "assigned_prompt_present": true,
                "task_started_path": files.task_started_path.display().to_string(),
                "started_at_unix_ms": 1234
            }))
            .expect("encode task start"),
        )
        .expect("write task start");
        let matched = MatchedSpawnSession {
            session_id: "expected-session".to_owned(),
            registered_at_unix_ms: 1000,
            agent_process_id: Some(42),
        };
        let mut params = test_spawn_params();
        params.cli = None;
        params.kind = Some(ActSpawnAgentCli::LocalModel);
        params.model_ref = Some("ollama-gemma4-e4b".to_owned());
        let read = read_agent_spawn_task_start_artifact(
            &files,
            &params,
            ActSpawnAgentCli::LocalModel,
            "agent-spawn-test",
            &matched,
        )
        .expect("read task start")
        .expect("task start present");

        assert_eq!(read.started_at_unix_ms, 1234);
        assert_eq!(read.readiness_source, "task_start_artifact");
    }

    #[test]
    fn task_start_artifact_validation_accepts_daemon_readiness_tool_source() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        let artifact = build_agent_spawn_task_started_artifact(
            "agent-spawn-test",
            ActSpawnAgentCli::Claude,
            "expected-session",
            true,
            &files.task_started_path,
        );
        fs::write(
            &files.task_started_path,
            serde_json::to_vec_pretty(&artifact).expect("encode task start"),
        )
        .expect("write task start");
        let matched = MatchedSpawnSession {
            session_id: "expected-session".to_owned(),
            registered_at_unix_ms: 1000,
            agent_process_id: Some(42),
        };
        let mut params = test_spawn_params();
        params.cli = Some(ActSpawnAgentCli::Claude);
        let read = read_agent_spawn_task_start_artifact(
            &files,
            &params,
            ActSpawnAgentCli::Claude,
            "agent-spawn-test",
            &matched,
        )
        .expect("read task start")
        .expect("task start present");

        assert!(read.started_at_unix_ms > 0);
        assert_eq!(read.readiness_source, "agent_spawn_task_started_tool");
    }

    fn observed_progress_test_files(dir: &Path) -> AgentSpawnFiles {
        AgentSpawnFiles {
            log_dir: dir.to_path_buf(),
            prompt_path: dir.join("prompt.txt"),
            stdout_path: dir.join("stdout.jsonl"),
            stderr_path: dir.join("stderr.log"),
            final_message_path: dir.join("final-message.txt"),
            completion_status_path: dir.join("completion-status.json"),
            task_started_path: dir.join("task-started.json"),
            task_started_script_path: dir.join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: Some(dir.join("codex-control.json")),
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        }
    }

    #[test]
    fn observed_task_progress_detects_real_liveness_without_artifact() {
        // No artifact + no activity => no false-positive (deadline still governs).
        let empty = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(empty.path());
        assert_eq!(
            agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Codex),
            None,
            "an idle spawn dir must not be reported as making progress"
        );

        // A produced final message proves the agent ran the task to completion,
        // even though it never wrote the cooperative task-start artifact.
        let finished = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(finished.path());
        fs::write(&files.final_message_path, b"PONG").expect("write final message");
        assert_eq!(
            agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Claude),
            Some("final_message_present")
        );

        // Codex: a control artifact with an established thread + underway turn is
        // daemon-trusted proof the agent connected and is executing.
        let codex = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(codex.path());
        fs::write(
            files.codex_app_server_control_path.as_ref().unwrap(),
            serde_json::to_vec(&json!({
                "thread_id": "019ec782-052f-7083-a7f9-79d97702b344",
                "turn_status": "completed"
            }))
            .unwrap(),
        )
        .expect("write codex control");
        assert_eq!(
            agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Codex),
            Some("codex_control_thread_established")
        );

        // A control artifact that only reached `starting` is NOT yet proof.
        let codex_early = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(codex_early.path());
        fs::write(
            files.codex_app_server_control_path.as_ref().unwrap(),
            serde_json::to_vec(&json!({ "thread_id": "", "turn_status": "starting" })).unwrap(),
        )
        .expect("write codex control");
        assert_eq!(
            agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::Codex),
            None
        );

        // Local-model stdout turn activity proves the task is underway.
        let local = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(local.path());
        fs::write(
            &files.stdout_path,
            b"{\"type\":\"local.turn.started\",\"turn_index\":1}\n",
        )
        .expect("write stdout");
        assert_eq!(
            agent_spawn_observed_task_progress(&files, ActSpawnAgentCli::LocalModel),
            Some("stdout_turn_activity")
        );
    }

    #[test]
    fn task_start_session_id_only_binds_matching_spawn() {
        let dir = tempfile::TempDir::new().expect("temp");
        let files = observed_progress_test_files(dir.path());
        fs::write(
            &files.task_started_path,
            serde_json::to_vec(&json!({
                "spawn_id": "agent-spawn-current",
                "session_id": "session-current"
            }))
            .expect("encode task-start marker"),
        )
        .expect("write task-start marker");

        assert_eq!(
            task_start_session_id_for_spawn(&files, "agent-spawn-current").as_deref(),
            Some("session-current")
        );
        assert_eq!(
            task_start_session_id_for_spawn(&files, "agent-spawn-other"),
            None
        );
    }

    #[test]
    fn task_start_artifact_validation_accepts_bom_prefixed_artifact() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "spawn_id": "agent-spawn-test",
                "cli": "codex",
                "session_id": "expected-session",
                "status": "started",
                "health_ok": true,
                "target_ok": true,
                "assigned_prompt_present": true,
                "task_started_path": files.task_started_path.display().to_string(),
                "readiness_source": "agent_spawn_task_started_tool",
                "started_at_unix_ms": 1234
            }))
            .expect("encode task start"),
        );
        fs::write(&files.task_started_path, bytes).expect("write task start");
        let matched = MatchedSpawnSession {
            session_id: "expected-session".to_owned(),
            registered_at_unix_ms: 1000,
            agent_process_id: Some(42),
        };
        let read = read_agent_spawn_task_start_artifact(
            &files,
            &test_spawn_params(),
            ActSpawnAgentCli::Codex,
            "agent-spawn-test",
            &matched,
        )
        .expect("read task start")
        .expect("task start present");

        assert_eq!(read.started_at_unix_ms, 1234);
    }

    #[test]
    fn codex_control_artifact_validation_accepts_matching_artifact() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let control_path = dir.path().join("codex-control.json");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: Some(dir.path().join("codex-notify.ps1")),
            codex_app_server_runner_path: Some(dir.path().join("codex-app-server-runner.ps1")),
            codex_app_server_control_path: Some(control_path.clone()),
            codex_app_server_events_path: Some(dir.path().join("codex-app-server-events.jsonl")),
            codex_app_server_stdout_path: Some(dir.path().join("codex-app-server.stdout.log")),
            codex_app_server_stderr_path: Some(dir.path().join("codex-app-server.stderr.log")),
            local_model_runner_path: None,
        };
        fs::write(
            &control_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "protocol": "codex_app_server_ws",
                "endpoint": "ws://127.0.0.1:38658",
                "control_path": control_path.display().to_string(),
                "events_path": files
                    .codex_app_server_events_path
                    .as_ref()
                    .expect("events path")
                    .display()
                    .to_string(),
                "app_server_process_id": 1234,
                "thread_id": "thread-1",
                "turn_id": "turn-1",
                "turn_status": "inProgress",
                "last_error": null,
                "approval_policy": "on-request",
                "sandbox_mode": "workspace-write",
                "app_server_request_bridge_url": "http://127.0.0.1:17700/codex-app-server/request",
                "last_app_server_request_status": "responded",
                "last_app_server_request_method": "mcpServer/elicitation/request",
                "last_app_server_request_id": "3",
                "last_app_server_request_approval_id": "apr1-test",
                "last_app_server_request_final_status": "accepted",
                "last_app_server_request_error": null,
                "last_app_server_request_at_unix_ms": 123790,
                "last_steer_status": "delivered",
                "last_steer_error": null,
                "last_steer_at_unix_ms": 123789,
                "last_steer_turn_id": "turn-1",
                "last_steer_instruction_chars": 42,
                "updated_at_unix_ms": 123456
            }))
            .expect("encode control"),
        )
        .expect("write control");

        let control = read_spawned_agent_control_artifact(&files, ActSpawnAgentCli::Codex)
            .expect("control read")
            .expect("codex control present");
        assert_eq!(control.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(control.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(control.last_steer_status.as_deref(), Some("delivered"));
        assert_eq!(control.last_steer_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(control.last_steer_instruction_chars, Some(42));
        assert_eq!(control.approval_policy.as_deref(), Some("on-request"));
        assert_eq!(control.sandbox_mode.as_deref(), Some("workspace-write"));
        assert_eq!(
            control.last_app_server_request_status.as_deref(),
            Some("responded")
        );
        assert_eq!(
            control.last_app_server_request_approval_id.as_deref(),
            Some("apr1-test")
        );

        let mut bom_prefixed = vec![0xEF, 0xBB, 0xBF];
        bom_prefixed.extend(
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "protocol": "codex_app_server_ws",
                "endpoint": "ws://127.0.0.1:38658",
                "control_path": control_path.display().to_string(),
                "events_path": files
                    .codex_app_server_events_path
                    .as_ref()
                    .expect("events path")
                    .display()
                    .to_string(),
                "app_server_process_id": 1234,
                "thread_id": "thread-bom",
                "turn_id": "turn-bom",
                "turn_status": "inProgress",
                "last_error": null,
                "updated_at_unix_ms": 123456
            }))
            .expect("encode bom control"),
        );
        fs::write(&control_path, bom_prefixed).expect("write bom control");
        let control = read_spawned_agent_control_artifact(&files, ActSpawnAgentCli::Codex)
            .expect("bom control read")
            .expect("codex control present");
        assert_eq!(control.thread_id.as_deref(), Some("thread-bom"));
        assert_eq!(control.turn_id.as_deref(), Some("turn-bom"));

        fs::write(
            &control_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "protocol": "codex_app_server_ws",
                "endpoint": "ws://127.0.0.1:38658",
                "control_path": control_path.display().to_string(),
                "events_path": "events.jsonl",
                "app_server_process_id": 1234,
                "thread_id": null,
                "turn_id": "turn-1",
                "turn_status": "inProgress",
                "last_error": null,
                "updated_at_unix_ms": 123456
            }))
            .expect("encode invalid control"),
        )
        .expect("write invalid control");
        let error = read_spawned_agent_control_artifact(&files, ActSpawnAgentCli::Codex)
            .expect_err("missing thread_id must fail");
        assert_eq!(
            error.get("reason").and_then(Value::as_str),
            Some("codex_control_artifact_invalid")
        );
    }

    #[test]
    fn codex_app_server_runner_prefers_powershell_shim_and_tree_cleanup() {
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Get-Command codex.ps1"),
            "Windows npm installs expose codex.ps1; launching it through powershell preserves -c array arguments"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Get-Command codex.cmd"),
            "codex.cmd remains a fallback when the PowerShell shim is absent"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Stop-OwnedProcessTree"),
            "app-server cleanup must target the exact spawned root PID and descendants"
        );
        assert!(
            !CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Start-Process -FilePath 'codex'"),
            "bare Start-Process codex resolves to a non-executable ps1 shim on this host"
        );
        assert!(
            !CODEX_APP_SERVER_RUNNER_SCRIPT.contains("notify=["),
            "app-server startup must not depend on the legacy Codex notify TOML array"
        );
        let existing_read = CODEX_APP_SERVER_RUNNER_SCRIPT
            .find("foreach ($property in $existing.PSObject.Properties)")
            .expect("runner reads existing control artifact");
        let live_thread_write = CODEX_APP_SERVER_RUNNER_SCRIPT
            .find("$current['thread_id'] = $script:ThreadId")
            .expect("runner writes live thread_id into control artifact");
        let live_turn_write = CODEX_APP_SERVER_RUNNER_SCRIPT
            .find("$current['turn_id'] = $script:TurnId")
            .expect("runner writes live turn_id into control artifact");
        assert!(
            existing_read < live_thread_write && existing_read < live_turn_write,
            "live runner control state must overwrite stale values from the previous artifact"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("phase = 'send_start'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("phase = 'send_ok'"),
            "runner must journal outbound JSON-RPC send boundaries for app-server stalls"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("SYNAPSE_CODEX_APP_SERVER_RPC_FAILED")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("expected_method")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("response_error_shape")
                && CODEX_APP_SERVER_RUNNER_SCRIPT
                    .contains("Receive-Response $socket 2 'thread/start'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT
                    .contains("Receive-Response $socket 3 'turn/start'"),
            "runner must fail closed with structured JSON-RPC diagnostics for Bad Request-style app-server failures"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("/codex-app-server/request")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Handle-AppServerRequest")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("app_server_response")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("last_app_server_request_status"),
            "runner must bridge Codex app-server approval/input requests into Synapse queue rows"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'workspace-write'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'on-request'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("New-CodexSandboxPolicy"),
            "gated Codex spawns must use Codex's own on-request approval policy and workspace-write sandbox"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("mcp_servers.synapse.tools.")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'health'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'session_list'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'get_target'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'agent'")
                && !CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'agent_spawn_task_started'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("approval_mode=")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("'approve'"),
            "startup-safe Synapse MCP tools must be pre-approved so Codex readiness cannot deadlock on its own health/task-start calls"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("$script:LastFinalAgentMessageText")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("$phase -eq 'final_answer'")
                && CODEX_APP_SERVER_RUNNER_SCRIPT
                    .contains("$finalText = $script:LastFinalAgentMessageText"),
            "runner must preserve final_answer item/completed notifications when turn/completed omits turn items"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT
                .contains("$prompt = [string](Get-Content -Raw -LiteralPath $PromptPath"),
            "Windows PowerShell 5.1 must cast Get-Content -Raw prompt bytes before ConvertTo-Json"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT.contains("ConvertTo-Json -Compress -Depth 20"),
            "JSON-RPC request encoding should use bounded schema depth, not an unbounded diagnostic depth"
        );
        assert!(
            CODEX_APP_SERVER_RUNNER_SCRIPT
                .contains("$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Write-TextNoBom -Path $tmp")
                && CODEX_APP_SERVER_RUNNER_SCRIPT.contains("Append-LineNoBom"),
            "Codex app-server control/events files must be written without a UTF-8 BOM"
        );
        let interrupt_script = include_str!("codex_app_server_interrupt.ps1");
        assert!(
            interrupt_script.contains("$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)")
                && interrupt_script.contains("Write-TextNoBom -Path $tmp")
                && interrupt_script.contains("Append-LineNoBom")
                && interrupt_script.contains("Invoke-WithFileRetry")
                && interrupt_script.contains("Move-ReplaceWithRetry"),
            "Codex app-server interrupt control/events files must be no-BOM and retry transient file contention"
        );
        assert!(
            interrupt_script.contains("SYNAPSE_CODEX_APP_SERVER_RPC_FAILED")
                && interrupt_script.contains("Receive-Response $socket 2 'turn/interrupt'")
                && interrupt_script.contains("response_error_shape"),
            "Codex app-server interrupt must preserve structured JSON-RPC failure diagnostics"
        );
        let steer_script = include_str!("codex_app_server_steer.ps1");
        assert!(
            steer_script.contains("method = 'turn/steer'")
                && steer_script.contains("expectedTurnId = $TurnId")
                && steer_script.contains("text_elements = @()")
                && steer_script.contains("responsesapiClientMetadata")
                && steer_script.contains("last_steer_status")
                && steer_script.contains("Move-ReplaceWithRetry"),
            "Codex app-server steer must use the generated turn/steer protocol with expectedTurnId and durable control readback"
        );
        assert!(
            steer_script.contains("SYNAPSE_CODEX_APP_SERVER_RPC_FAILED")
                && steer_script.contains("Receive-Response $socket 2 'turn/steer'")
                && steer_script.contains("response_error_shape"),
            "Codex app-server steer must preserve structured JSON-RPC failure diagnostics"
        );
    }

    #[test]
    fn spawn_wrapper_forces_utf8_and_records_wrapper_pid() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: Some(dir.path().join("codex-notify.ps1")),
            codex_app_server_runner_path: Some(dir.path().join("codex-app-server-runner.ps1")),
            codex_app_server_control_path: Some(dir.path().join("codex-control.json")),
            codex_app_server_events_path: Some(dir.path().join("codex-app-server-events.jsonl")),
            codex_app_server_stdout_path: Some(dir.path().join("codex-app-server.stdout.log")),
            codex_app_server_stderr_path: Some(dir.path().join("codex-app-server.stderr.log")),
            local_model_runner_path: None,
        };
        let script = agent_spawn_powershell_script(&test_spawn_params(), &files, dir.path())
            .expect("build wrapper script");

        assert!(script.contains("$env:PYTHONUTF8 = '1'"));
        assert!(script.contains("$env:PYTHONIOENCODING = 'utf-8'"));
        assert!(script.contains("Remove-Item Env:PYTHONLEGACYWINDOWSSTDIO"));
        assert!(script.contains("wrapper_process_id = $spawnWrapperProcessId"));
        assert!(script.contains("$spawnTaskStartedPath"));
        assert!(script.contains("task_started_present"));
        assert!(script.contains("function Invoke-SpawnHoldOpen"));
        assert!(script.contains("Start-Sleep -Milliseconds $sleepMs"));
        assert!(
            script.find("Invoke-SpawnHoldOpen")
                < script.find("Write-SpawnCompletionStatus -Status $finalStatus"),
            "wrapper must enforce hold_open before writing terminal status: {script}"
        );
        assert!(script.contains("Get-Content -Raw -LiteralPath $spawnPromptPath -Encoding UTF8"));
        assert!(
            script.contains("$event.type -eq 'assistant'")
                && script.contains("$event.message.role -eq 'assistant'")
                && script.contains("$event.type -eq 'result'")
                && script.contains("$event.result")
                && script.contains("'stdout_jsonl_result'"),
            "wrapper must recover Claude stream-json assistant/result final text: {script}"
        );
        assert!(
            script.contains("codex-app-server-runner.ps1"),
            "codex spawn must run through the app-server runner: {script}"
        );
        assert!(
            script.contains("& '")
                && script.contains("codex-app-server-runner.ps1' @codexRunnerArgs")
                && !script.contains("& powershell.exe @codexRunnerArgs"),
            "codex spawn must invoke the runner inside the already hidden wrapper instead of spawning a visible nested PowerShell: {script}"
        );
        assert!(
            script.contains("$codexRunnerArgs = @{")
                && script.contains("SpawnId = $spawnId")
                && script.contains("EventsPath = '")
                && !script.contains("$codexRunnerArgs = @('-SpawnId'"),
            "codex spawn must use named parameter splatting so direct script invocation cannot misbind runner args: {script}"
        );
        assert!(
            script.contains("RequireApprovalGate"),
            "default gated Codex spawns must tell the app-server runner to bridge approvals: {script}"
        );
        assert!(script.contains("ControlPath"));
        assert!(script.contains("codex-control.json"));
        assert!(script.contains("NotifyScriptPath"));
        assert!(script.contains("codex-notify.ps1"));
    }

    #[test]
    fn claude_spawn_script_injects_hook_settings() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: Some(dir.path().join("claude-debug.log")),
            mcp_config_path: Some(dir.path().join("claude-mcp-config.json")),
            hook_settings_path: Some(dir.path().join("claude-hook-settings.json")),
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        let mut params = test_spawn_params();
        params.cli = Some(ActSpawnAgentCli::Claude);
        let script = agent_spawn_powershell_script(&params, &files, dir.path())
            .expect("build wrapper script");
        assert!(
            script.contains("'--settings'"),
            "claude args must inject the hook settings file: {script}"
        );
        assert!(script.contains("claude-hook-settings.json"));
    }

    #[test]
    fn spawn_script_injects_model_arg_for_both_clis() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        // Codex: model is passed to the app-server runner, which starts the
        // actual turn through `thread/start`/`turn/start`.
        let codex_files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: Some(dir.path().join("codex-notify.ps1")),
            codex_app_server_runner_path: Some(dir.path().join("codex-app-server-runner.ps1")),
            codex_app_server_control_path: Some(dir.path().join("codex-control.json")),
            codex_app_server_events_path: Some(dir.path().join("codex-app-server-events.jsonl")),
            codex_app_server_stdout_path: Some(dir.path().join("codex-app-server.stdout.log")),
            codex_app_server_stderr_path: Some(dir.path().join("codex-app-server.stderr.log")),
            local_model_runner_path: None,
        };
        let mut codex_params = test_spawn_params();
        codex_params.model = Some("gpt-5-codex".to_owned());
        let codex_script = agent_spawn_powershell_script(&codex_params, &codex_files, dir.path())
            .expect("codex script");
        assert!(
            codex_script.contains("$codexRunnerArgs['Model'] = 'gpt-5-codex'"),
            "codex runner args must inject the pinned model: {codex_script}"
        );
        assert!(
            codex_script.contains("$codexRunnerArgs['RequireApprovalGate'] = $true"),
            "codex runner args must enable app-server approval bridging when the spawn is gated: {codex_script}"
        );

        // Codex without a model: no runner model override appears.
        let codex_no_model =
            agent_spawn_powershell_script(&test_spawn_params(), &codex_files, dir.path())
                .expect("codex script");
        assert!(
            codex_no_model.contains("codex-app-server-runner.ps1"),
            "codex still runs through app-server without a pinned model: {codex_no_model}"
        );
        assert!(!codex_no_model.contains("['Model']"));
        assert!(codex_no_model.contains("['RequireApprovalGate']"));

        // Claude: `--model <model>` injected right after `-p`.
        let claude_files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: Some(dir.path().join("claude-debug.log")),
            mcp_config_path: Some(dir.path().join("claude-mcp-config.json")),
            hook_settings_path: Some(dir.path().join("claude-hook-settings.json")),
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: None,
        };
        let mut claude_params = test_spawn_params();
        claude_params.cli = Some(ActSpawnAgentCli::Claude);
        claude_params.model = Some("claude-fable-5".to_owned());
        let claude_script =
            agent_spawn_powershell_script(&claude_params, &claude_files, dir.path())
                .expect("claude script");
        assert!(
            claude_script.contains("@('-p','--model','claude-fable-5','--verbose'"),
            "claude args must inject --model after -p: {claude_script}"
        );
    }

    #[test]
    fn spawn_manifest_records_cli_and_model() {
        // The manifest is the transcript ingester's authoritative model source.
        let mut params = test_spawn_params();
        params.model = Some("gpt-5-codex".to_owned());
        let manifest = build_spawn_manifest("agent-spawn-manifest-regression", &params)
            .expect("build spawn manifest");
        assert_eq!(manifest["version"], AGENT_SPAWN_MANIFEST_VERSION);
        assert_eq!(manifest["spawn_id"], "agent-spawn-manifest-regression");
        assert_eq!(manifest["cli"], "codex");
        assert_eq!(manifest["model"], "gpt-5-codex");
        assert_eq!(manifest["require_approval_gate"], true);
        assert_eq!(manifest["approval_gate_effective"], true);
        assert_eq!(manifest["assigned_prompt_present"], true);
        assert!(manifest["created_unix_ms"].as_u64().is_some());

        // No pinned model -> manifest carries an explicit null, never a guess.
        params.model = None;
        let manifest = build_spawn_manifest("agent-spawn-manifest-regression", &params)
            .expect("build spawn manifest");
        assert!(manifest["model"].is_null());
    }

    #[test]
    fn local_model_spawn_script_uses_repo_runner_and_model_ref() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let files = AgentSpawnFiles {
            log_dir: dir.path().to_path_buf(),
            prompt_path: dir.path().join("prompt.txt"),
            stdout_path: dir.path().join("stdout.jsonl"),
            stderr_path: dir.path().join("stderr.log"),
            final_message_path: dir.path().join("final-message.txt"),
            completion_status_path: dir.path().join("completion-status.json"),
            task_started_path: dir.path().join("task-started.json"),
            task_started_script_path: dir.path().join("write-task-started.ps1"),
            debug_path: None,
            mcp_config_path: None,
            hook_settings_path: None,
            notify_script_path: None,
            codex_app_server_runner_path: None,
            codex_app_server_control_path: None,
            codex_app_server_events_path: None,
            codex_app_server_stdout_path: None,
            codex_app_server_stderr_path: None,
            local_model_runner_path: Some(dir.path().join("local-model-runner.json")),
        };
        let mut params = test_spawn_params();
        params.cli = None;
        params.kind = Some(ActSpawnAgentCli::LocalModel);
        params.model_ref = Some("ollama-gemma4-e4b".to_owned());
        params.prompt = Some("call health once".to_owned());

        let prompt = build_agent_spawn_prompt(
            "agent-spawn-test",
            &params,
            dir.path(),
            &files.task_started_path,
            &files.task_started_script_path,
        )
        .expect("local prompt");
        assert_eq!(prompt, "call health once");

        let script =
            agent_spawn_powershell_script(&params, &files, dir.path()).expect("local script");
        assert!(script.contains("--mode"));
        assert!(script.contains("local-agent"));
        assert!(script.contains("--local-agent-model"));
        assert!(script.contains("ollama-gemma4-e4b"));
        assert!(script.contains("--local-agent-task-file"));
        assert!(script.contains("--local-agent-spawn-id"));
        assert!(script.contains("--local-agent-log-dir"));
        assert!(script.contains("--local-agent-hold-open-ms"));
        assert!(script.contains("'1234'"));
        assert!(script.contains("--local-agent-trusted-unattended-exact-contract"));
        assert!(!script.contains("& codex"));
        assert!(!script.contains("& claude"));

        let manifest =
            build_spawn_manifest("agent-spawn-manifest-local", &params).expect("manifest");
        assert_eq!(manifest["cli"], "local-model");
        assert_eq!(manifest["kind"], "local-model");
        assert_eq!(manifest["model"], "ollama-gemma4-e4b");
        assert_eq!(manifest["model_ref"], "ollama-gemma4-e4b");
        assert_eq!(manifest["require_approval_gate"], true);
        assert_eq!(manifest["approval_gate_effective"], false);
        assert_eq!(manifest["local_model_autonomous_tool_calls"], true);
        assert_eq!(manifest["local_model_approval_gate_used"], false);
        assert_eq!(
            manifest["local_model_trusted_unattended_exact_contract"],
            true
        );

        params.require_approval_gate = false;
        let trusted_script =
            agent_spawn_powershell_script(&params, &files, dir.path()).expect("trusted script");
        assert!(trusted_script.contains("--local-agent-trusted-unattended-exact-contract"));
        let trusted_manifest =
            build_spawn_manifest("agent-spawn-manifest-local", &params).expect("manifest");
        assert_eq!(trusted_manifest["require_approval_gate"], false);
        assert_eq!(trusted_manifest["approval_gate_effective"], false);
        assert_eq!(trusted_manifest["local_model_autonomous_tool_calls"], true);
        assert_eq!(trusted_manifest["local_model_approval_gate_used"], false);
        assert_eq!(
            trusted_manifest["local_model_trusted_unattended_exact_contract"],
            true
        );
    }

    #[test]
    fn local_model_spawn_prompt_builder_rejects_blank_prompt() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let task_started_path = dir.path().join("task-started.json");
        let task_started_script_path = dir.path().join("write-task-started.ps1");
        let mut params = test_spawn_params();
        params.cli = None;
        params.kind = Some(ActSpawnAgentCli::LocalModel);
        params.model_ref = Some("ollama-gemma4-e4b".to_owned());
        params.prompt = Some("  \r\n\t ".to_owned());

        let error = build_agent_spawn_prompt(
            "agent-spawn-test",
            &params,
            dir.path(),
            &task_started_path,
            &task_started_script_path,
        )
        .expect_err("blank local-model prompt must fail closed");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::TOOL_PARAMS_INVALID))
        );
        assert!(
            error
                .message
                .contains("local_model prompt must not be empty"),
            "{}",
            error.message
        );
    }

    #[test]
    fn direct_spawn_prompt_builder_rejects_blank_prompt() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let task_started_path = dir.path().join("task-started.json");
        let task_started_script_path = dir.path().join("write-task-started.ps1");

        for prompt in [None, Some(""), Some("  \r\n\t ")] {
            let mut params = test_spawn_params();
            params.prompt = prompt.map(str::to_owned);
            let error = build_agent_spawn_prompt(
                "agent-spawn-test",
                &params,
                dir.path(),
                &task_started_path,
                &task_started_script_path,
            )
            .expect_err("blank direct spawn prompt must fail closed");

            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::TOOL_PARAMS_INVALID))
            );
            assert!(
                error
                    .message
                    .contains("direct spawn prompt must not be empty"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn template_rendered_prompt_builder_accepts_template_prompt() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let task_started_path = dir.path().join("task-started.json");
        let task_started_script_path = dir.path().join("write-task-started.ps1");
        let mut params = test_spawn_params();
        params.template_id = Some("issue1245-template".to_owned());
        params.template_version = Some(1);
        params.template_config_hash = Some("sha256:test".to_owned());
        params.prompt = Some("template-provided task".to_owned());

        let prompt = build_agent_spawn_prompt(
            "agent-spawn-test",
            &params,
            dir.path(),
            &task_started_path,
            &task_started_script_path,
        )
        .expect("template prompt builds");

        assert!(prompt.contains("template-provided task"));
        assert!(prompt.contains("task-start readiness artifact"));
    }

    #[test]
    fn claude_hook_settings_subscribe_every_ingress_event_with_bearer() {
        let settings =
            build_claude_hook_settings("agent-spawn-test", "http://127.0.0.1:7700/mcp", true)
                .expect("settings build");
        let hooks = settings["hooks"].as_object().expect("hooks object");
        for event in super::super::agent_event_ingress::CLAUDE_HOOK_SUBSCRIBED_EVENTS {
            let entry = &hooks[*event][0]["hooks"][0];
            assert_eq!(entry["type"], "http", "{event} must use a native HTTP hook");
            assert_eq!(
                entry["url"],
                "http://127.0.0.1:7700/agent-events?spawn_id=agent-spawn-test&source=claude_code_hooks"
            );
            assert_eq!(
                entry["headers"]["Authorization"],
                "Bearer $SYNAPSE_BEARER_TOKEN"
            );
            assert_eq!(entry["allowedEnvVars"][0], "SYNAPSE_BEARER_TOKEN");
        }
        assert_eq!(
            settings["allowedHttpHookUrls"][0],
            "http://127.0.0.1:7700/agent-events?spawn_id=agent-spawn-test&source=claude_code_hooks*"
        );
        assert_eq!(
            settings["httpHookAllowedEnvVars"][0],
            "SYNAPSE_BEARER_TOKEN"
        );
        // Gated spawns pre-approve the safe tools so they skip the gate (#927).
        let allow = settings["permissions"]["allow"]
            .as_array()
            .expect("permissions.allow present when gating");
        assert!(allow.iter().any(|rule| rule == "Read"));
        assert!(allow.iter().any(|rule| rule == "Bash(git status:*)"));
        assert!(allow.iter().any(|rule| rule == "mcp__synapse__agent"));
        assert!(allow.iter().any(|rule| rule == "mcp__synapse__approval"));
        assert!(allow.iter().any(|rule| rule == "mcp__synapse__session"));
        assert!(allow.iter().any(|rule| rule == "mcp__synapse__target"));
        for tool in CLAUDE_COORDINATION_FACADE_MCP_TOOLS {
            let expected = format!("mcp__synapse__{tool}");
            assert!(
                allow.iter().any(|rule| rule == &expected),
                "Claude static allow list must include {expected}"
            );
        }
        let encoded = serde_json::to_string(&settings).expect("settings serialize");
        assert!(!encoded.contains("write-task-started.ps1"));
        assert!(!encoded.contains("mcp__synapse__approval_gate"));
        assert!(!encoded.contains("mcp__synapse__session_list"));
        assert!(!encoded.contains("mcp__synapse__tool_profile_status"));
        assert!(!encoded.contains("mcp__synapse__get_target"));
        assert!(!encoded.contains("mcp__synapse__set_target"));
        assert!(
            !allow.iter().any(|rule| rule
                .as_str()
                .is_some_and(|rule| rule.starts_with("PowerShell("))),
            "Claude gated allowlist must not preserve task-start helper PowerShell rules"
        );
    }

    #[test]
    fn claude_hook_settings_omit_permissions_when_gate_disabled() {
        let settings =
            build_claude_hook_settings("agent-spawn-test", "http://127.0.0.1:7700/mcp", false)
                .expect("settings build");
        assert!(
            settings.get("permissions").is_none(),
            "ungated spawn (bypassPermissions) must not inject allow rules"
        );
    }

    #[test]
    fn codex_notify_script_posts_to_ingress_and_logs_failures() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let script =
            build_codex_notify_script("agent-spawn-test", "http://127.0.0.1:7700/mcp", dir.path())
                .expect("notify script build");
        assert!(script.contains(
            "http://127.0.0.1:7700/agent-events?spawn_id=agent-spawn-test&source=codex_notify"
        ));
        assert!(script.contains("$args[-1]"), "{script}");
        assert!(script.contains("SYNAPSE_BEARER_TOKEN"));
        assert!(script.contains("notify-errors.log"));
        assert!(script.contains("TimeoutSec 5"));
    }

    #[test]
    fn ingress_url_refuses_mcp_url_without_mcp_suffix() {
        let error = agent_event_ingress_url("agent-spawn-test", "http://127.0.0.1:7700/", "x")
            .expect_err("non-/mcp URL must fail closed");
        assert!(
            error.message.contains("does not end with"),
            "{}",
            error.message
        );
        let error = agent_event_ingress_url("agent-spawn-test", "/mcp", "x")
            .expect_err("authority-less URL must fail closed");
        assert!(
            error.message.contains("no scheme/authority"),
            "{}",
            error.message
        );
    }

    #[test]
    fn orphan_recovery_writes_terminal_artifacts_for_dead_wrapper() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let log_dir = dir.path().join("agent-spawn-test");
        fs::create_dir_all(&log_dir).expect("create log dir");
        let completion_status_path = log_dir.join("completion-status.json");
        let stdout_path = log_dir.join("stdout.jsonl");
        fs::write(
            &completion_status_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "spawn_id": "agent-spawn-test",
                "cli": "codex",
                "status": "running",
                "wrapper_process_id": 99_999_999u32,
                "wrapper_started_at_unix_ms": unix_time_ms_now().saturating_sub(120_000),
                "requested_hold_open_ms": 60_000
            }))
            .expect("encode running status"),
        )
        .expect("write running status");
        fs::write(
            &stdout_path,
            b"{\"type\":\"agent_message\",\"text\":\"partial\"}\n",
        )
        .expect("write stdout");

        let decision =
            agent_spawn_orphan_recovery_decision("agent-spawn-test", &log_dir, unix_time_ms_now())
                .expect("orphan decision");
        let AgentSpawnOrphanRecoveryDecision::Recover(recovery) = decision else {
            panic!("dead wrapper should recover");
        };
        write_agent_spawn_orphan_terminal_artifacts("agent-spawn-test", &log_dir, &recovery)
            .expect("write orphan artifacts");

        let status: Value = serde_json::from_slice(
            &fs::read(&completion_status_path).expect("read recovered status"),
        )
        .expect("parse recovered status");
        assert_eq!(
            status.get("status").and_then(Value::as_str),
            Some("orphaned_running_recovered")
        );
        assert_eq!(
            status
                .get("orphan_recovery_artifact")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(log_dir.join("final-message.txt").exists());
    }
}

async fn run_shell_with_idempotency(
    service: &SynapseService,
    params: ActRunShellParams,
    authorization: RunShellAuthorization,
    inline_await_limit_ms: u64,
    context: Option<&ShellExecutionContext>,
) -> Result<ActRunShellResponse, ErrorData> {
    validate_run_shell_execution_plan(&params, inline_await_limit_ms)?;
    let session_id = context.map(ShellExecutionContext::session_id);
    let Some(row_key) = run_shell_idempotency_row_key(&params, session_id)? else {
        return run_authorized_shell(params, &authorization, inline_await_limit_ms, context).await;
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

    let response = run_authorized_shell(
        params.clone(),
        &authorization,
        inline_await_limit_ms,
        context,
    )
    .await?;
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
