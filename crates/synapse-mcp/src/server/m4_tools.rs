use super::{
    ActComboParams, ActComboResponse, ActLaunchParams, ActLaunchResponse,
    ActRunShellCancelResponse, ActRunShellJobIdParams, ActRunShellParams, ActRunShellResponse,
    ActRunShellStartParams, ActRunShellStartResponse, ActRunShellStatusParams,
    ActRunShellStatusResponse, ActSpawnAgentCli, ActSpawnAgentLogPaths, ActSpawnAgentParams,
    ActSpawnAgentRequest, ActSpawnAgentResponse, ActSpawnAgentTarget, ErrorData, Json,
    LaunchWindowState, MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS, Parameters, RunShellAuthorization,
    ShellExecutionContext, SynapseService, assign_owned_process_job, authorize_run_shell,
    authorize_run_shell_start, cancel_shell_job, execute_combo, launch, launch_for_session,
    launch_process_history_row, launch_process_history_row_key, launch_request_details, mcp_error,
    prepare_run_shell_params_for_context, prepare_run_shell_start_params_for_context,
    required_combo_permissions, run_authorized_shell, run_shell_idempotency_completed_row,
    run_shell_idempotency_replay, run_shell_idempotency_reservation_row,
    run_shell_idempotency_row_key, run_shell_request_details, run_shell_start_request_details,
    shell_execution_context_for_session, shell_job_status, start_authorized_shell_job, tool,
    tool_router, validate_agent_spawn_params, validate_run_shell_execution_plan,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use serde_json::{Map, Value, json};
use synapse_core::{error_codes, new_reflex_id};

use crate::m3::local_models::{LocalModelApiShape, LocalModelRegistryRow};

use super::{
    m1_tools::validate_target_window,
    session_registry::{SpawnedAgentControlRead, SpawnedAgentRead, unix_time_ms_now},
};

const ACT_SPAWN_AGENT: &str = "act_spawn_agent";
/// Filename of the per-spawn manifest written into each spawn dir. The
/// transcript ingester (#900/#949) reads it to learn the spawn's model when the
/// CLI stream does not carry one (Codex).
pub(crate) const AGENT_SPAWN_MANIFEST_FILENAME: &str = "spawn-manifest.json";
/// Schema version stamped onto the spawn manifest.
pub(crate) const AGENT_SPAWN_MANIFEST_VERSION: u32 = 1;
const CODEX_APP_SERVER_RUNNER_SCRIPT: &str = include_str!("codex_app_server_runner.ps1");

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
        description = "Run an allowlisted executable child process. command is an executable path/name only; pass flags and shell snippets in args, using an explicit shell executable when shell syntax is required. execution_mode controls routing: auto preserves compatibility and backgrounds only when timeout_ms exceeds the inline await limit, inline never backgrounds and honors timeout_ms directly, durable returns a job handle immediately. durable_timeout_ms is an explicit durable job lifetime cap only when a durable/background job is created; omit for an unbounded durable job. Poll act_run_shell_status and cancel with act_run_shell_cancel."
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
        description = "Start an allowlisted executable as a durable background shell job. Returns immediately with a job id plus status/stdout/stderr file paths. Omitting timeout_ms leaves the durable job unbounded until normal exit, explicit act_run_shell_cancel, or session cleanup; providing timeout_ms is an explicit lifetime cap for that job only."
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
        result.map(Json)
    }

    #[tool(
        description = "Spawn a fully capable primary Codex, Claude, or local_model agent as a hidden background process, wire it to the configured Synapse HTTP MCP daemon, require real MCP session registration, optionally bind a per-session target, and return only after session_list readback plus a validated task-start readiness artifact prove the spawned prompt began executing. Pass cli/kind for a direct spawn, or template_id (+ template_params) to render the spawn from a durable agent_template; a template-rendered spawn records the exact (template_id, version, config_hash) used and rejects passing cli/kind/model/model_ref/prompt/working_dir/target alongside the template."
    )]
    pub async fn act_spawn_agent(
        &self,
        params: Parameters<ActSpawnAgentRequest>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActSpawnAgentResponse>, ErrorData> {
        if let Err(error) = self.ensure_supported_use_allows_action("act_launch") {
            self.audit_action_denied_for_request(ACT_SPAWN_AGENT, &error, &request_context);
            return Err(error);
        }
        // Resolve the request — direct spawn or template-rendered — into the
        // concrete spawn params before any side effect, so a bad template id or
        // param contract fails loudly with nothing launched (#909).
        let params = match self.resolve_spawn_request(params.0) {
            Ok(params) => params,
            Err(error) => {
                self.audit_action_denied_for_request(ACT_SPAWN_AGENT, &error, &request_context);
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
            super::context::mcp_session_id_from_request_context(&request_context)?;
        let actor_session_id_for_audit = started_by_session_id.clone();
        self.audit_action_started_with_details_for_request(
            ACT_SPAWN_AGENT,
            &agent_spawn_request_details(&params, started_by_session_id.as_deref()),
            &request_context,
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
                        &request_context,
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
        self.audit_action_result_for_request(ACT_SPAWN_AGENT, &result, &request_context)?;
        result.map(Json)
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

    /// Resolves a caller's spawn request into concrete spawn params. A direct
    /// spawn (no `template_id`) passes its fields through; a template spawn
    /// renders the params atomically from the durable template and stamps the
    /// `(id, version, config_hash)` provenance. The two modes are mutually
    /// exclusive and conflicts are rejected loudly — never silently merged.
    fn resolve_spawn_request(
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
                    template_id: None,
                    template_version: None,
                    template_config_hash: None,
                };
                params.effective_cli()?;
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
            Some(self.require_spawn_local_model_row(&params)?)
        } else {
            None
        };

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
        let wait_deadline = agent_spawn_wait_deadline(params.wait_timeout_ms)?;
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
        if let Some((env_var, value)) = resolve_spawn_local_model_api_key(local_model_row.as_ref())?
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

        let launch_response = match launch(&self.m4_config, launch_params.clone()).await {
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

        let matched = match self
            .wait_for_spawned_agent_session(
                &params,
                agent_kind,
                &before_session_ids,
                launched_at_unix_ms,
                launch_response.pid,
                &files,
                wait_deadline,
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
                        "wait_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };

        let task_started = match self
            .wait_for_spawned_agent_task_started(
                &params,
                agent_kind,
                &spawn_id,
                &matched,
                launch_response.pid,
                &files,
                wait_deadline,
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
                        "task_start_error": error,
                        "cleanup": cleanup,
                        "completion_artifacts": completion_artifacts,
                    }),
                ));
            }
        };
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
            target: params.target,
            template_id: params.template_id,
            template_version: params.template_version,
            template_config_hash: params.template_config_hash,
            log_paths: files.to_response(),
        })
    }

    fn current_session_ids(&self) -> Result<BTreeSet<String>, ErrorData> {
        Ok(self
            .session_list_impl(true)?
            .sessions
            .into_iter()
            .map(|summary| summary.registry.session_id)
            .collect())
    }

    async fn wait_for_spawned_agent_session(
        &self,
        params: &ActSpawnAgentParams,
        agent_kind: ActSpawnAgentCli,
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
            let list = self.session_list_impl(true).map_err(|error| {
                json!({
                    "reason": "session_list_read_failed",
                    "error": error.message,
                    "data": error.data,
                })
            })?;
            let mut matched_session = None;
            let session_count = list.sessions.len();
            let mut sessions_json = Vec::new();
            let mut readiness_reason_counts: BTreeMap<String, u64> = BTreeMap::new();
            let mut candidate_readiness = Vec::new();
            for summary in &list.sessions {
                let readiness = spawn_session_candidate_readiness(
                    summary,
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
                if matched_session.is_none()
                    && readiness.get("ready").and_then(Value::as_bool) == Some(true)
                {
                    matched_session = Some(MatchedSpawnSession {
                        session_id: summary.registry.session_id.clone(),
                        registered_at_unix_ms: unix_time_ms_now(),
                        agent_process_id: discover_agent_process_id(launcher_pid, agent_kind),
                    });
                }
                if reason != "session_existed_before_spawn"
                    && candidate_readiness.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT
                {
                    candidate_readiness.push(json!({
                        "session_id": summary.registry.session_id,
                        "started_at_unix_ms": summary.registry.started_at_unix_ms,
                        "last_action": summary.registry.last_action,
                        "active_target": summary.active_target,
                        "readiness": readiness.clone(),
                    }));
                }
                if sessions_json.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT {
                    sessions_json.push(spawn_session_observation(summary, readiness));
                }
            }
            last_observed = json!({
                "reason": "candidate_not_ready",
                "session_count": session_count,
                "sessions_recorded": sessions_json.len(),
                "readiness_reason_counts": readiness_reason_counts,
                "candidate_readiness_recorded": candidate_readiness.len(),
                "candidate_readiness": candidate_readiness,
                "sessions": sessions_json,
                "readiness_files": agent_spawn_readiness_file_readback(files),
            });

            if let Some(matched) = matched_session {
                return Ok(matched);
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
        matched: &MatchedSpawnSession,
        launcher_pid: u32,
        files: &AgentSpawnFiles,
        deadline: Instant,
    ) -> Result<AgentSpawnTaskStartRead, serde_json::Value> {
        let mut last_observed = json!({
            "reason": "task_start_artifact_not_observed",
            "task_started_path": files.task_started_path.display().to_string(),
        });
        while !agent_spawn_deadline_remaining(deadline).is_zero() {
            match read_agent_spawn_task_start_artifact(
                files, params, agent_kind, spawn_id, matched,
            )? {
                Some(read) => return Ok(read),
                None => {
                    last_observed = json!({
                        "reason": "task_start_artifact_not_observed",
                        "task_started_path": files.task_started_path.display().to_string(),
                        "completion_status": read_json_file_lossy(&files.completion_status_path),
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

    fn require_spawn_local_model_row(
        &self,
        params: &ActSpawnAgentParams,
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
        let row = rows
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
        let Some(probe) = row.last_probe.as_ref() else {
            return Err(local_model_spawn_refusal(
                error_codes::MODEL_REGISTRY_UNPROBED,
                "local_model_registry_row_unprobed",
                "act_spawn_agent local_model refused because the registry row has no probe evidence",
                json!({
                    "model_ref": model_ref,
                    "row_key": row.row_key.clone(),
                    "last_probe": null,
                    "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                }),
            ));
        };
        if !probe.healthy {
            let code = match probe.error_code.as_deref() {
                Some(error_codes::MODEL_TOOLS_UNSUPPORTED) => error_codes::MODEL_TOOLS_UNSUPPORTED,
                Some(error_codes::MODEL_ENDPOINT_UNREACHABLE) | None => {
                    error_codes::MODEL_ENDPOINT_UNREACHABLE
                }
                Some(_) => error_codes::MODEL_ENDPOINT_UNREACHABLE,
            };
            return Err(local_model_spawn_refusal(
                code,
                "local_model_registry_row_unhealthy",
                "act_spawn_agent local_model refused because the registry row's last probe is unhealthy",
                json!({
                    "model_ref": model_ref,
                    "row_key": row.row_key.clone(),
                    "last_probe": row.last_probe.clone(),
                    "source_of_truth": "CF_KV prefix local_model_registry/v1/model/name_hex/",
                }),
            ));
        }
        Ok(row)
    }
}

#[derive(Debug)]
struct MatchedSpawnSession {
    session_id: String,
    registered_at_unix_ms: u64,
    agent_process_id: Option<u32>,
}

#[derive(Debug)]
struct AgentSpawnTaskStartRead {
    started_at_unix_ms: u64,
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
        ActSpawnAgentLogPaths {
            log_dir: self.log_dir.display().to_string(),
            prompt_path: self.prompt_path.display().to_string(),
            stdout_path: self.stdout_path.display().to_string(),
            stderr_path: self.stderr_path.display().to_string(),
            final_message_path: self.final_message_path.display().to_string(),
            completion_status_path: self.completion_status_path.display().to_string(),
            task_started_path: self.task_started_path.display().to_string(),
            task_started_script_path: self.task_started_script_path.display().to_string(),
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
    local_model_row: Option<&LocalModelRegistryRow>,
) -> Result<Option<(String, String)>, ErrorData> {
    let Some(row) = local_model_row else {
        return Ok(None);
    };
    let Some(env_var) = row.api_key_env_var.as_deref() else {
        return Ok(None);
    };
    match std::env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => Ok(Some((env_var.to_owned(), value))),
        Ok(_) | Err(_) => Err(local_model_spawn_refusal(
            error_codes::MODEL_API_KEY_MISSING,
            "local_model_api_key_missing",
            "act_spawn_agent local_model refused because the registry row requires an API key env var the daemon does not have set to a non-empty value",
            json!({
                "model_ref": row.name,
                "api_key_env_var": env_var,
                "remediation": format!(
                    "start the Synapse daemon with {env_var} present in its environment (e.g. inject it from your secret manager) so spawned agents inherit the credential"
                ),
                "source_of_truth": "daemon process environment + CF_KV prefix local_model_registry/v1/model/name_hex/",
            }),
        )),
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

    let task_started_script =
        build_agent_spawn_task_start_script(spawn_id, params, &task_started_path);
    fs::write(&task_started_script_path, task_started_script).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "act_spawn_agent failed to write task-start script {}: {error}",
                task_started_script_path.display()
            ),
        )
    })?;

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
                        "Authorization": "Bearer ${SYNAPSE_BEARER_TOKEN}"
                    }
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
        let settings = build_claude_hook_settings(spawn_id, &params.mcp_url)?;
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
fn build_claude_hook_settings(spawn_id: &str, mcp_url: &str) -> Result<Value, ErrorData> {
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
    Ok(json!({
        "hooks": hooks,
        "allowedHttpHookUrls": [format!("{}*", ingress_url)],
        "httpHookAllowedEnvVars": ["SYNAPSE_BEARER_TOKEN"],
    }))
}

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
    task_started_script_path: &Path,
) -> Result<String, ErrorData> {
    let agent_kind = params.effective_cli()?;
    if agent_kind.is_local_model() {
        let assigned_prompt = params.prompt.as_deref().unwrap_or("").trim();
        if assigned_prompt.is_empty() {
            return Ok(
                "Use the Synapse health tool once, then respond with a compact health_ok summary."
                    .to_owned(),
            );
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
                "3. Call the Synapse MCP set_target tool with exactly this JSON as the target value: {target_json}\n4. Call the Synapse MCP get_target tool and verify its current target exactly matches that JSON."
            )
        }
        None => {
            "3. Call the Synapse MCP get_target tool and report the returned session_id/current target.".to_owned()
        }
    };
    let assigned_prompt = params.prompt.as_deref().unwrap_or("").trim();
    let assigned_block = if assigned_prompt.is_empty() {
        "No additional task was provided; perform only the provisioning checks above.".to_owned()
    } else {
        format!(
            "After the provisioning checks pass, perform this assigned task:\n{assigned_prompt}"
        )
    };
    let hold_instruction = if params.hold_open_ms == 0 {
        "Do not add an artificial hold-open sleep after completing the provisioning checks and assigned task.".to_owned()
    } else {
        format!(
            "After the provisioning checks and assigned task, keep this primary process alive for at least {} ms using Start-Sleep -Milliseconds {}, then finish.",
            params.hold_open_ms, params.hold_open_ms
        )
    };
    let task_started_path_display = task_started_path.display().to_string();
    let task_started_script_path_display = task_started_script_path.display().to_string();
    let task_started_script_path_ps = ps_single_quoted_path(task_started_script_path);
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
2. Use the real Synapse MCP session_list tool through your normal configured MCP client.\n\
{target_instruction}\n\
5. If any Synapse MCP tool is missing or fails, stop and report the exact tool/error.\n\
6. Before performing the assigned task or hold-open sleep, write the required task-start readiness artifact to: {task_started_path}\n\
   Helper path: {task_started_script_path}\n\
   Run the daemon-generated PowerShell helper exactly once after replacing <your_session_id> with this spawned MCP session id:\n\
   & {task_started_script_path_ps} -SessionId '<your_session_id>'\n\
   Do not rewrite the helper inline. The helper writes the JSON atomically, reads {task_started_path} back, and fails closed with an exact mismatch error if any field is wrong.\n\
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
        task_started_script_path_ps = task_started_script_path_ps,
        task_started_script_path = task_started_script_path_display,
        assigned_block = assigned_block,
        hold_instruction = hold_instruction,
    ))
}

fn build_agent_spawn_task_start_script(
    spawn_id: &str,
    params: &ActSpawnAgentParams,
    task_started_path: &Path,
) -> String {
    let agent_kind = params.effective_cli().ok();
    let agent_kind = agent_kind
        .map(ActSpawnAgentCli::as_str)
        .unwrap_or("invalid");
    let spawn_id = ps_single_quote(spawn_id);
    let cli = ps_single_quote(agent_kind);
    let task_started_path = ps_single_quoted_path(task_started_path);
    let assigned_prompt_present = if params
        .prompt
        .as_deref()
        .is_some_and(|prompt| !prompt.trim().is_empty())
    {
        "$true"
    } else {
        "$false"
    };
    format!(
        "param(\n\
    [Parameter(Mandatory = $true)]\n\
    [ValidateNotNullOrEmpty()]\n\
    [string]$SessionId\n\
)\n\
$ErrorActionPreference = 'Stop'\n\
Set-StrictMode -Version Latest\n\
$taskStartedPath = {task_started_path}\n\
$taskStartedTempPath = \"$taskStartedPath.tmp.$PID\"\n\
$taskStarted = [ordered]@{{\n\
    schema_version = 1\n\
    spawn_id = {spawn_id}\n\
    cli = {cli}\n\
    session_id = $SessionId\n\
    status = 'started'\n\
    health_ok = $true\n\
    target_ok = $true\n\
    assigned_prompt_present = {assigned_prompt_present}\n\
    task_started_path = $taskStartedPath\n\
    started_at_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()\n\
}}\n\
[System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($taskStartedPath)) | Out-Null\n\
$taskStarted | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $taskStartedTempPath -Encoding UTF8\n\
Move-Item -LiteralPath $taskStartedTempPath -Destination $taskStartedPath -Force\n\
\n\
$readBack = Get-Content -LiteralPath $taskStartedPath -Raw -Encoding UTF8 | ConvertFrom-Json\n\
$expected = [ordered]@{{\n\
    schema_version = 1\n\
    spawn_id = {spawn_id}\n\
    cli = {cli}\n\
    session_id = $SessionId\n\
    status = 'started'\n\
    health_ok = $true\n\
    target_ok = $true\n\
    assigned_prompt_present = {assigned_prompt_present}\n\
    task_started_path = $taskStartedPath\n\
}}\n\
foreach ($key in $expected.Keys) {{\n\
    $property = $readBack.PSObject.Properties.Item($key)\n\
    if ($null -eq $property) {{ throw (\"task-started missing field {{0}}\" -f $key) }}\n\
    $actual = $property.Value\n\
    $expectedValue = $expected[$key]\n\
    if ($actual -ne $expectedValue) {{\n\
        throw (\"task-started mismatch for {{0}}: expected '{{1}}' actual '{{2}}'\" -f $key, $expectedValue, $actual)\n\
    }}\n\
}}\n\
if ($null -eq $readBack.started_at_unix_ms -or [int64]$readBack.started_at_unix_ms -le 0) {{\n\
    throw 'task-started missing valid started_at_unix_ms'\n\
}}\n\
$readBack | ConvertTo-Json -Depth 8\n",
        task_started_path = task_started_path,
        spawn_id = spawn_id,
        cli = cli,
        assigned_prompt_present = assigned_prompt_present,
    )
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
                .map(|model| {
                    format!(
                        "$codexRunnerArgs += @('-Model',{})\n",
                        ps_single_quote(model)
                    )
                })
                .unwrap_or_default();
            format!(
                "$codexRunnerArgs = @('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',{runner_path},'-SpawnId',$spawnId,'-PromptPath',$spawnPromptPath,'-StdoutPath',$spawnStdoutPath,'-StderrPath',$spawnStderrPath,'-FinalMessagePath',$spawnFinalMessagePath,'-ControlPath',{control_path},'-EventsPath',{events_path},'-AppServerStdoutPath',{app_stdout_path},'-AppServerStderrPath',{app_stderr_path},'-WorkingDir',{working_dir},'-McpUrl',{mcp_url},'-NotifyScriptPath',{notify_script_path})\n\
{model_arg}\
& powershell.exe @codexRunnerArgs\n\
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
            format!(
                "$claudeArgs = @('-p'{model_arg},'--verbose','--output-format','stream-json','--input-format','text','--permission-mode','bypassPermissions','--mcp-config',{mcp_config_path},'--strict-mcp-config','--settings',{hook_settings_path},'--add-dir',{working_dir},'--debug-file',{debug_path})\n\
$prompt | & claude @claudeArgs 1> {stdout_path} 2> {stderr_path}\n\
",
                model_arg = model_arg,
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
                "@('--mode','local-agent','--local-agent-model',{model_ref},'--local-agent-task-file',{prompt_path},'--local-agent-mcp-url',{mcp_url},'--local-agent-spawn-id',$spawnId,'--local-agent-log-dir',{log_dir},'--local-agent-timeout-ms','{timeout_ms}')",
                model_ref = ps_single_quote(model_ref),
                prompt_path = prompt_path,
                mcp_url = ps_single_quote(&params.mcp_url),
                log_dir = ps_single_quoted_path(&files.log_dir),
                timeout_ms = timeout_ms,
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
                }} elseif ($event.type -eq 'agent_message' -and $null -ne $event.text) {{\n\
                    $finalText = [string]$event.text\n\
                }} elseif ($event.type -eq 'message' -and $event.role -eq 'assistant' -and $null -ne $event.content) {{\n\
                    if ($event.content -is [string]) {{\n\
                        $finalText = [string]$event.content\n\
                    }} else {{\n\
                        $parts = @()\n\
                        foreach ($part in $event.content) {{\n\
                            if ($null -ne $part.text) {{ $parts += [string]$part.text }}\n\
                        }}\n\
                        if ($parts.Count -gt 0) {{ $finalText = [string]::Join(\"`n\", $parts) }}\n\
                    }}\n\
                }}\n\
            }} catch {{}}\n\
        }}\n\
    }}\n\
    return $finalText\n\
}}\n\
\n\
function Write-SpawnRecoveredFinalMessage([string]$Text) {{\n\
    Set-Content -LiteralPath $spawnFinalMessagePath -Value $Text -Encoding UTF8\n\
    $script:spawnFinalMessageSource = 'stdout_jsonl_agent_message'\n\
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

fn agent_spawn_wait_deadline(wait_timeout_ms: u64) -> Result<Instant, ErrorData> {
    if wait_timeout_ms > MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_spawn_agent wait_timeout_ms must be <= {MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS}"),
        ));
    }
    Instant::now()
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
    let value = serde_json::from_slice::<Value>(&bytes).map_err(|error| {
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

    if !validation_errors.is_empty() {
        return Err(json!({
            "reason": "task_start_artifact_invalid",
            "task_started_path": expected_path,
            "validation_errors": validation_errors,
            "artifact": value,
        }));
    }

    Ok(Some(AgentSpawnTaskStartRead { started_at_unix_ms }))
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
    })
}

fn spawn_session_observation(
    summary: &super::session_tools::SessionSummary,
    readiness: Value,
) -> Value {
    json!({
        "session_id": summary.registry.session_id,
        "agent_kind": summary.registry.agent_kind,
        "client_name": summary.registry.client_name,
        "client_version": summary.registry.client_version,
        "lifecycle": summary.registry.lifecycle,
        "started_at_unix_ms": summary.registry.started_at_unix_ms,
        "last_seen_unix_ms": summary.registry.last_seen_unix_ms,
        "last_seen_ms_ago": summary.registry.last_seen_ms_ago,
        "last_action": summary.registry.last_action,
        "active_target": summary.active_target,
        "readiness": readiness,
    })
}

fn spawn_session_candidate_readiness(
    summary: &super::session_tools::SessionSummary,
    agent_kind: ActSpawnAgentCli,
    target: Option<&ActSpawnAgentTarget>,
    before_session_ids: &BTreeSet<String>,
    launched_at_unix_ms: u64,
) -> Value {
    if before_session_ids.contains(&summary.registry.session_id) {
        return json!({
            "ready": false,
            "reason": "session_existed_before_spawn",
            "expected": "new distinct MCP session id",
        });
    }
    if summary.registry.started_at_unix_ms + 2_000 < launched_at_unix_ms {
        return json!({
            "ready": false,
            "reason": "session_started_before_spawn_window",
            "started_at_unix_ms": summary.registry.started_at_unix_ms,
            "launched_at_unix_ms": launched_at_unix_ms,
            "allowed_clock_skew_ms": 2000,
        });
    }
    if !summary_matches_cli(summary, agent_kind) {
        return json!({
            "ready": false,
            "reason": "session_cli_mismatch",
            "expected_cli": agent_kind.as_str(),
            "agent_kind": summary.registry.agent_kind,
            "client_name": summary.registry.client_name,
        });
    }
    if let Some(expected) = target {
        if matches_target_wire(summary.active_target.as_ref(), expected) {
            json!({
                "ready": true,
                "reason": "target_bound",
            })
        } else {
            json!({
                "ready": false,
                "reason": "target_mismatch",
                "expected_target": expected,
                "active_target": summary.active_target,
            })
        }
    } else if summary
        .registry
        .last_action
        .as_deref()
        .is_some_and(|action| action.starts_with("tools/call:"))
    {
        json!({
            "ready": true,
            "reason": "tool_call_observed",
            "last_action": summary.registry.last_action,
        })
    } else {
        json!({
            "ready": false,
            "reason": "tool_call_not_observed",
            "last_action": summary.registry.last_action,
            "expected": "last_action beginning with tools/call:",
        })
    }
}

fn summary_matches_cli(
    summary: &super::session_tools::SessionSummary,
    cli: ActSpawnAgentCli,
) -> bool {
    let cli = cli.as_str();
    if summary.registry.agent_kind == cli {
        return true;
    }
    summary
        .registry
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
            template_id: None,
            template_version: None,
            template_config_hash: None,
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
        }
    }

    #[test]
    fn resolve_spawn_api_key_none_when_row_absent_or_keyless() {
        // Non-local-model spawn: nothing to resolve.
        assert!(
            resolve_spawn_local_model_api_key(None)
                .expect("no row resolves cleanly")
                .is_none()
        );
        // Loopback model with no declared key (e.g. Ollama): nothing to forward.
        let row = test_local_model_row(None);
        assert!(
            resolve_spawn_local_model_api_key(Some(&row))
                .expect("keyless row resolves cleanly")
                .is_none()
        );
    }

    #[test]
    fn resolve_spawn_api_key_forwards_value_when_present() {
        // Unique env var name so parallel tests never collide on process env.
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_PRESENT";
        // SAFETY: single-threaded within this test; unique key avoids races.
        unsafe { std::env::set_var(env_var, "sk-secret-value") };
        let row = test_local_model_row(Some(env_var));
        let resolved = resolve_spawn_local_model_api_key(Some(&row))
            .expect("present key resolves")
            .expect("present key yields a value");
        assert_eq!(resolved.0, env_var);
        assert_eq!(resolved.1, "sk-secret-value");
        unsafe { std::env::remove_var(env_var) };
    }

    #[test]
    fn resolve_spawn_api_key_refuses_loudly_when_missing() {
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_MISSING";
        unsafe { std::env::remove_var(env_var) };
        let row = test_local_model_row(Some(env_var));
        let err = resolve_spawn_local_model_api_key(Some(&row))
            .expect_err("missing key must refuse the spawn loudly");
        let data = err.data.expect("refusal carries structured detail");
        assert_eq!(data["code"], error_codes::MODEL_API_KEY_MISSING);
        assert_eq!(data["detail"]["api_key_env_var"], env_var);
        assert!(
            data["detail"]["remediation"]
                .as_str()
                .unwrap_or_default()
                .contains(env_var),
            "remediation names the missing env var"
        );
    }

    #[test]
    fn resolve_spawn_api_key_refuses_when_value_blank() {
        let env_var = "SYNAPSE_TEST_DEEPSEEK_KEY_BLANK";
        unsafe { std::env::set_var(env_var, "   ") };
        let row = test_local_model_row(Some(env_var));
        let err = resolve_spawn_local_model_api_key(Some(&row))
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
        assert!(prompt.contains("write-task-started.ps1"));
        assert!(prompt.contains("Do not rewrite the helper inline"));

        let script = build_agent_spawn_task_start_script(
            "agent-spawn-test",
            &test_spawn_params(),
            &task_started_path,
        );
        assert!(script.contains("-f $key, $expectedValue, $actual"));
        assert!(!script.contains("$key:"));
        assert!(script.contains("assigned_prompt_present = $true"));
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
    fn task_start_artifact_validation_accepts_matching_artifact() {
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
                && interrupt_script.contains("Append-LineNoBom"),
            "Codex app-server interrupt control/events files must be written without a UTF-8 BOM"
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
        assert!(script.contains("Get-Content -Raw -LiteralPath $spawnPromptPath -Encoding UTF8"));
        assert!(
            script.contains("codex-app-server-runner.ps1"),
            "codex spawn must run through the app-server runner: {script}"
        );
        assert!(script.contains("-ControlPath"));
        assert!(script.contains("codex-control.json"));
        assert!(script.contains("-NotifyScriptPath"));
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
            codex_script.contains("$codexRunnerArgs += @('-Model','gpt-5-codex')"),
            "codex runner args must inject the pinned model: {codex_script}"
        );

        // Codex without a model: no runner model override appears.
        let codex_no_model =
            agent_spawn_powershell_script(&test_spawn_params(), &codex_files, dir.path())
                .expect("codex script");
        assert!(
            codex_no_model.contains("codex-app-server-runner.ps1"),
            "codex still runs through app-server without a pinned model: {codex_no_model}"
        );
        assert!(!codex_no_model.contains("'-Model'"));

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
        assert!(!script.contains("& codex"));
        assert!(!script.contains("& claude"));

        let manifest =
            build_spawn_manifest("agent-spawn-manifest-local", &params).expect("manifest");
        assert_eq!(manifest["cli"], "local-model");
        assert_eq!(manifest["kind"], "local-model");
        assert_eq!(manifest["model"], "ollama-gemma4-e4b");
        assert_eq!(manifest["model_ref"], "ollama-gemma4-e4b");
    }

    #[test]
    fn claude_hook_settings_subscribe_every_ingress_event_with_bearer() {
        let settings = build_claude_hook_settings("agent-spawn-test", "http://127.0.0.1:7700/mcp")
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
