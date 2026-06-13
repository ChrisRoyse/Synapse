use super::{
    ActComboParams, ActComboResponse, ActLaunchParams, ActLaunchResponse,
    ActRunShellCancelResponse, ActRunShellJobIdParams, ActRunShellParams, ActRunShellResponse,
    ActRunShellStartParams, ActRunShellStartResponse, ActRunShellStatusParams,
    ActRunShellStatusResponse, ActSpawnAgentCli, ActSpawnAgentLogPaths, ActSpawnAgentParams,
    ActSpawnAgentResponse, ActSpawnAgentTarget, ErrorData, Json, LaunchWindowState,
    MAX_AGENT_SPAWN_WAIT_TIMEOUT_MS, Parameters, RunShellAuthorization, ShellExecutionContext,
    SynapseService, assign_owned_process_job, authorize_run_shell, authorize_run_shell_start,
    cancel_shell_job, execute_combo, launch, launch_for_session, launch_process_history_row,
    launch_process_history_row_key, launch_request_details, mcp_error,
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

use super::{
    m1_tools::validate_target_window,
    session_registry::{SpawnedAgentRead, unix_time_ms_now},
};

const ACT_SPAWN_AGENT: &str = "act_spawn_agent";
/// Filename of the per-spawn manifest written into each spawn dir. The
/// transcript ingester (#900/#949) reads it to learn the spawn's model when the
/// CLI stream does not carry one (Codex).
pub(crate) const AGENT_SPAWN_MANIFEST_FILENAME: &str = "spawn-manifest.json";
/// Schema version stamped onto the spawn manifest.
pub(crate) const AGENT_SPAWN_MANIFEST_VERSION: u32 = 1;

/// Builds the per-spawn manifest JSON. Records the CLI and, when the operator
/// pinned one, the model — the authoritative model source the transcript
/// ingester reads (indispensable for Codex, whose stream omits it, #949).
fn build_spawn_manifest(spawn_id: &str, params: &ActSpawnAgentParams) -> Value {
    json!({
        "version": AGENT_SPAWN_MANIFEST_VERSION,
        "spawn_id": spawn_id,
        "cli": params.cli.as_str(),
        "model": params.model,
        "created_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
    })
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
        self.audit_action_started_with_details_for_session(
            "act_run_shell",
            &run_shell_request_details(&params, self.m4_config.run_shell_inline_await_limit_ms()),
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
        self.audit_action_started_with_details_for_session(
            "act_run_shell_start",
            &run_shell_start_request_details(&params),
            &session_id,
        )?;
        let result = match authorize_run_shell_start(&self.m4_config, &params) {
            Ok(authorization) => {
                start_authorized_shell_job(params, &authorization, Some(&shell_context))
            }
            Err(error) => Err(error),
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
        description = "Cancel a durable background shell job by exact job id, terminating only the recorded local job-owned process tree and returning status/log/process readback. SSH jobs report remote cleanup as unverified unless the job status contains remote pid/process-group metadata."
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
        self.audit_action_started_with_details_for_session(
            "act_run_shell_cancel",
            &json!({
                "job_id": &params.job_id,
                "session_id": &session_id,
            }),
            &session_id,
        )?;
        let result = cancel_shell_job(&params, Some(&session_id));
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
        if let Some(session_id) = session_id.as_deref() {
            self.audit_action_started_with_details_for_session(
                "act_launch",
                &launch_request_details(&params),
                session_id,
            )?;
        } else {
            self.audit_action_started_with_details("act_launch", &launch_request_details(&params))?;
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
        if let Some(session_id) = session_id.as_deref() {
            self.audit_action_result_for_session("act_launch", &result, session_id)?;
        } else {
            self.audit_action_result("act_launch", &result)?;
        }
        result.map(Json)
    }

    #[tool(
        description = "Spawn a fully capable primary Codex or Claude agent as a hidden background process, wire it to the configured Synapse HTTP MCP daemon, require real MCP session registration, optionally bind a per-session target, and return only after session_list readback plus a validated task-start readiness artifact prove the spawned prompt began executing."
    )]
    pub async fn act_spawn_agent(
        &self,
        params: Parameters<ActSpawnAgentParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActSpawnAgentResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = ACT_SPAWN_AGENT,
            cli = params.0.cli.as_str(),
            "tool.invocation kind=act_spawn_agent"
        );
        if let Err(error) = self.ensure_supported_use_allows_action("act_launch") {
            self.audit_action_denied_for_request(ACT_SPAWN_AGENT, &error, &request_context);
            return Err(error);
        }
        let started_by_session_id =
            super::context::mcp_session_id_from_request_context(&request_context)?;
        let params = params.0;
        self.audit_action_started_with_details_for_request(
            ACT_SPAWN_AGENT,
            &agent_spawn_request_details(&params, started_by_session_id.as_deref()),
            &request_context,
        )?;
        // The spawn id is allocated before any side effect so every journal
        // event of this lifecycle (#897) shares one attribution anchor; a
        // spawn that cannot be journaled is refused before launching.
        let spawn_id = format!("agent-spawn-{}", new_reflex_id());
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
                    return Err(journal_error);
                }
            }
            Err(error) => {
                // The spawn error must win the response; the journal failure
                // (if any) is already logged as AGENT_EVENT_WRITE_FAILED.
                self.journal_spawn_failed(&spawn_id, error);
            }
        }
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
    async fn act_spawn_agent_impl(
        &self,
        params: ActSpawnAgentParams,
        started_by_session_id: Option<String>,
        spawn_id: String,
    ) -> Result<ActSpawnAgentResponse, ErrorData> {
        validate_agent_spawn_params(&params)?;
        validate_spawn_target(&params.target)?;

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
            params.cli.as_str().to_owned(),
        );
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
                        "cli": params.cli.as_str(),
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
                    "cli": params.cli.as_str(),
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
                        "cli": params.cli.as_str(),
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
                        "cli": params.cli.as_str(),
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

        let metadata = SpawnedAgentRead {
            spawn_id: spawn_id.clone(),
            cli: params.cli.as_str().to_owned(),
            launcher_process_id: launch_response.pid,
            agent_process_id: matched.agent_process_id,
            started_by_session_id,
            launched_at_unix_ms,
            launch_target: launch_params.target.clone(),
            log_dir: files.log_dir.display().to_string(),
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
                    "cli": params.cli.as_str(),
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
            .with_agent_cli(params.cli.as_str()),
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
                    "cli": params.cli.as_str(),
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
            cli: params.cli,
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
                    params,
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
                        agent_process_id: discover_agent_process_id(launcher_pid, params.cli),
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
            match read_agent_spawn_task_start_artifact(files, params, spawn_id, matched)? {
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
        let db = self.m3_storage()?;
        let mut record = synapse_core::AgentEventRecord::new(
            super::agent_events::unix_time_ns_now(),
            synapse_core::AgentEventKind::SpawnRequested,
        );
        record.spawn_id = Some(spawn_id.to_owned());
        record.attributes.operation_name = Some(synapse_core::GenAiOperationName::CreateAgent);
        record.attributes.agent_name = Some(params.cli.as_str().to_owned());
        record.attributes.provider_name =
            super::agent_events::provider_for_agent_kind(params.cli.as_str());
        // Prompt CONTENT stays out of the journal (OTel GenAI opt-in rule);
        // its length is enough for the dashboard to size the request.
        record.payload = json!({
            "started_by_session_id": started_by_session_id,
            "prompt_chars": params.prompt.as_deref().map(|prompt| prompt.chars().count()),
            "working_dir": params.working_dir,
            "mcp_url": params.mcp_url,
            "wait_timeout_ms": params.wait_timeout_ms,
            "hold_open_ms": params.hold_open_ms,
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
        }
    }
}

fn agent_spawn_request_details(
    params: &ActSpawnAgentParams,
    started_by_session_id: Option<&str>,
) -> serde_json::Value {
    json!({
        "cli": params.cli.as_str(),
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
    data.entry("code".to_owned())
        .or_insert_with(|| json!(error_codes::ACTION_AGENT_SPAWN_FAILED));
    data.entry("reason".to_owned())
        .or_insert_with(|| json!(failure_stage));
    data.insert("agent_spawn_failure_stage".to_owned(), json!(failure_stage));
    data.insert("spawn_id".to_owned(), json!(spawn_id));
    data.insert("cli".to_owned(), json!(params.cli.as_str()));
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
    let completed_at_unix_ms = unix_time_ms_now();
    let stdout_len = file_len(&files.stdout_path);
    let stderr_len = file_len(&files.stderr_path);
    let final_message = json!({
        "schema_version": 1,
        "spawn_id": spawn_id,
        "cli": params.cli.as_str(),
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
        "cli": params.cli.as_str(),
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
        (params.cli == ActSpawnAgentCli::Claude).then(|| log_dir.join("claude-debug.log"));
    let mcp_config_path =
        (params.cli == ActSpawnAgentCli::Claude).then(|| log_dir.join("claude-mcp-config.json"));
    let hook_settings_path =
        (params.cli == ActSpawnAgentCli::Claude).then(|| log_dir.join("claude-hook-settings.json"));
    let notify_script_path =
        (params.cli == ActSpawnAgentCli::Codex).then(|| log_dir.join("codex-notify.ps1"));

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

    // Spawn manifest: the authoritative record of which CLI and (when the
    // operator pinned one) which model this spawn was launched with. The
    // transcript ingester reads it to attribute cost — indispensable for Codex,
    // whose `exec --json` stream carries no model id (#949).
    let manifest_path = log_dir.join(AGENT_SPAWN_MANIFEST_FILENAME);
    let manifest = build_spawn_manifest(spawn_id, params);
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
        cli = params.cli.as_str(),
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
    let spawn_id = ps_single_quote(spawn_id);
    let cli = ps_single_quote(params.cli.as_str());
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
    let prompt_path = ps_single_quoted_path(&files.prompt_path);
    let stdout_path = ps_single_quoted_path(&files.stdout_path);
    let stderr_path = ps_single_quoted_path(&files.stderr_path);
    let final_message_path = ps_single_quoted_path(&files.final_message_path);
    let completion_status_path = ps_single_quoted_path(&files.completion_status_path);
    let task_started_path = ps_single_quoted_path(&files.task_started_path);
    let working_dir = ps_single_quoted_path(working_dir);
    let command_body = match params.cli {
        ActSpawnAgentCli::Codex => {
            let Some(notify_script_path) = files.notify_script_path.as_ref() else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "act_spawn_agent internal error: missing Codex notify script path",
                ));
            };
            let mcp_url_config = format!(
                "mcp_servers.synapse.url={}",
                toml_string_literal(&params.mcp_url)
            );
            let notify_config = format!(
                "notify=[\"powershell\",\"-NoLogo\",\"-NoProfile\",\"-NonInteractive\",\"-ExecutionPolicy\",\"Bypass\",\"-File\",{}]",
                toml_string_literal(&notify_script_path.display().to_string())
            );
            // The Codex `exec --json` stream carries no model id, so passing it
            // here (and recording it in the spawn manifest) is the only way the
            // transcript ingester can attribute the spawn's cost (#949).
            let model_arg = params
                .model
                .as_deref()
                .map(|model| format!(",'-m',{}", ps_single_quote(model)))
                .unwrap_or_default();
            format!(
                "$codexArgs = @('exec'{model_arg},'-C',{working_dir},'-s','danger-full-access','--json','-o',{final_message_path},'-c',{mcp_url_config},'-c','mcp_servers.synapse.bearer_token_env_var=\"SYNAPSE_BEARER_TOKEN\"','-c',{notify_config},'-')\n\
$prompt | & codex @codexArgs 1> {stdout_path} 2> {stderr_path}\n\
",
                model_arg = model_arg,
                working_dir = working_dir,
                final_message_path = final_message_path,
                mcp_url_config = ps_single_quote(&mcp_url_config),
                notify_config = ps_single_quote(&notify_config),
                stdout_path = stdout_path,
                stderr_path = stderr_path,
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
    let cli = ps_single_quote(params.cli.as_str());
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

fn toml_string_literal(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
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
    if object.get("cli").and_then(Value::as_str) != Some(params.cli.as_str()) {
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
    params: &ActSpawnAgentParams,
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
    if !summary_matches_cli(summary, params.cli) {
        return json!({
            "ready": false,
            "reason": "session_cli_mismatch",
            "expected_cli": params.cli.as_str(),
            "agent_kind": summary.registry.agent_kind,
            "client_name": summary.registry.client_name,
        });
    }
    if let Some(expected) = &params.target {
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
            cli: ActSpawnAgentCli::Codex,
            model: None,
            prompt: Some("write a report".to_owned()),
            target: None,
            working_dir: None,
            mcp_url: "http://127.0.0.1:7700/mcp".to_owned(),
            wait_timeout_ms: 30_000,
            hold_open_ms: 1234,
        }
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
            "agent-spawn-test",
            &matched,
        )
        .expect("read task start")
        .expect("task start present");

        assert_eq!(read.started_at_unix_ms, 1234);
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
            script.contains("'-c','notify=["),
            "codex args must inject the notify program: {script}"
        );
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
        };
        let mut params = test_spawn_params();
        params.cli = ActSpawnAgentCli::Claude;
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
        // Codex: `-m <model>` injected right after `exec`.
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
        };
        let mut codex_params = test_spawn_params();
        codex_params.model = Some("gpt-5-codex".to_owned());
        let codex_script = agent_spawn_powershell_script(&codex_params, &codex_files, dir.path())
            .expect("codex script");
        assert!(
            codex_script.contains("@('exec','-m','gpt-5-codex','-C'"),
            "codex args must inject -m <model> after exec: {codex_script}"
        );

        // Codex without a model: no `-m` arg appears.
        let codex_no_model = agent_spawn_powershell_script(&test_spawn_params(), &codex_files, dir.path())
            .expect("codex script");
        assert!(
            codex_no_model.contains("@('exec','-C'"),
            "codex args must omit -m when no model is pinned: {codex_no_model}"
        );
        assert!(!codex_no_model.contains("'-m'"));

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
        };
        let mut claude_params = test_spawn_params();
        claude_params.cli = ActSpawnAgentCli::Claude;
        claude_params.model = Some("claude-fable-5".to_owned());
        let claude_script = agent_spawn_powershell_script(&claude_params, &claude_files, dir.path())
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
        let manifest = build_spawn_manifest("agent-spawn-manifest-fsv", &params);
        assert_eq!(manifest["version"], AGENT_SPAWN_MANIFEST_VERSION);
        assert_eq!(manifest["spawn_id"], "agent-spawn-manifest-fsv");
        assert_eq!(manifest["cli"], "codex");
        assert_eq!(manifest["model"], "gpt-5-codex");
        assert!(manifest["created_unix_ms"].as_u64().is_some());

        // No pinned model -> manifest carries an explicit null, never a guess.
        params.model = None;
        let manifest = build_spawn_manifest("agent-spawn-manifest-fsv", &params);
        assert!(manifest["model"].is_null());
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
