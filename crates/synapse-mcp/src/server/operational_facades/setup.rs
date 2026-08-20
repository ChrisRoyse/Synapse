use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::model::ErrorCode;
use rmcp::{RoleServer, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::{
    chrome_debugger_bridge,
    server::{ErrorData, Json, Parameters, SynapseService, mcp_error},
};

use super::{
    SETUP_SOT, SETUP_TOOL,
    errors::{facade_delegate_error, missing_spec},
    host_transition, launchd_service,
    policy::require_maintenance_profile,
    response::setup_response,
    types::{FileReadback, SetupOperation, SetupParams, SetupResponse, SetupStatusResponse},
    validation::validate_setup_params,
};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<SetupParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<SetupResponse>, ErrorData> {
    validate_setup_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = SETUP_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=setup"
    );
    match operation {
        SetupOperation::Status | SetupOperation::Doctor => {
            let status = setup_status(service).map_err(|error| {
                    facade_delegate_error(
                        SETUP_TOOL,
                        operation.as_str(),
                        "setup_status",
                        SETUP_SOT,
                        error,
                        "repair the exact unreadable setup file/env prerequisite and retry setup status",
                    )
                })?;
            Ok(Json(setup_response(
                operation,
                "setup status physical files read".to_owned(),
                |out| {
                    if operation == SetupOperation::Status {
                        out.status = Some(status);
                    } else {
                        out.doctor = Some(status);
                    }
                },
            )))
        }
        SetupOperation::Repair => {
            let spec = params
                .0
                .repair
                .ok_or_else(|| missing_spec(SETUP_TOOL, "repair"))?;
            if spec.reason.trim().is_empty() {
                return Err(missing_spec(SETUP_TOOL, "repair.reason"));
            }
            require_maintenance_profile(
                service,
                &request_context,
                SETUP_TOOL,
                operation.as_str(),
                "synapse_setup_repair",
                SETUP_SOT,
            )?;
            let plan = setup_repair_plan()?;
            let chrome_bridge_preflight = match &plan {
                SetupRepairPlan::Full => preflight_setup_repair_chrome_bridge().await?,
                SetupRepairPlan::ResumeChromeBridge { .. } => {
                    "chrome_bridge_preflight=deferred_to_checkpointed_chrome_bridge_activation"
                        .to_owned()
                }
            };
            let launched =
                launch_setup_repair(service, &spec.reason, &chrome_bridge_preflight, &plan)?;
            let status = setup_status(service).map_err(|error| {
                facade_delegate_error(
                    SETUP_TOOL,
                    operation.as_str(),
                    "setup_status_after_repair_launch",
                    SETUP_SOT,
                    error,
                    "inspect the setup repair run manifest/logs and retry setup status after the external process exits",
                )
            })?;
            Ok(Json(setup_response(
                operation,
                launched.readback_source_of_truth(),
                |out| {
                    out.status = Some(status);
                },
            )))
        }
        SetupOperation::LaunchdService => {
            let spec = params
                .0
                .launchd_service
                .ok_or_else(|| missing_spec(SETUP_TOOL, "launchd_service"))?;
            if spec.action == super::types::SetupLaunchdServiceAction::Restart {
                require_maintenance_profile(
                    service,
                    &request_context,
                    SETUP_TOOL,
                    operation.as_str(),
                    "synapse_launchd_service_restart",
                    SETUP_SOT,
                )?;
            }
            let result = launchd_service::handle(spec).await?;
            Ok(Json(setup_response(
                operation,
                result.source_of_truth.clone(),
                |out| {
                    out.launchd_service = Some(result);
                },
            )))
        }
        SetupOperation::HostTransition => {
            let spec = params
                .0
                .host_transition
                .ok_or_else(|| missing_spec(SETUP_TOOL, "host_transition"))?;
            let result = host_transition::handle(spec)?;
            Ok(Json(setup_response(
                operation,
                result.source_of_truth.clone(),
                |out| {
                    out.host_transition = Some(result);
                },
            )))
        }
    }
}

#[derive(Debug)]
struct SetupRepairLaunchReadback {
    run_id: String,
    run_dir: PathBuf,
    manifest_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    setup_script_path: PathBuf,
    source_dir: PathBuf,
    launcher_path: PathBuf,
    repair_mode: &'static str,
    chrome_bridge_preflight: String,
    child_pid: u32,
}

impl SetupRepairLaunchReadback {
    fn readback_source_of_truth(&self) -> String {
        format!(
            "external setup repair launched; run_id={} child_pid={} repair_mode={} launcher={} source_dir={} setup_script={} run_dir={} manifest={} stdout_log={} stderr_log={} {}",
            self.run_id,
            self.child_pid,
            self.repair_mode,
            self.launcher_path.display(),
            self.source_dir.display(),
            self.setup_script_path.display(),
            self.run_dir.display(),
            self.manifest_path.display(),
            self.stdout_path.display(),
            self.stderr_path.display(),
            self.chrome_bridge_preflight
        )
    }
}

#[derive(Serialize)]
struct SetupRepairRunManifest<'a> {
    schema: &'static str,
    state: &'a str,
    run_id: &'a str,
    reason: &'a str,
    source_dir: String,
    setup_script_path: String,
    bind: String,
    launcher_path: String,
    stdout_log: String,
    stderr_log: String,
    child_pid: Option<u32>,
    started_at_unix_ms: u128,
    command_args: Vec<String>,
    active_issue: Option<String>,
    repair_mode: &'static str,
    chrome_bridge_preflight: String,
    remediation: &'static str,
}

#[derive(Debug)]
enum SetupRepairPlan {
    Full,
    ResumeChromeBridge {
        checkpoint_path: PathBuf,
        maintenance_lock_path: PathBuf,
    },
}

impl SetupRepairPlan {
    fn mode(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ResumeChromeBridge { .. } => "resume_chrome_bridge",
        }
    }
}

#[derive(Deserialize)]
struct SetupBridgeCheckpointEnvelope {
    schema: String,
    state: String,
    phase: String,
    maintenance_lock_path: String,
    #[serde(default)]
    checkpoint_generation_id: String,
    #[serde(default)]
    daemon_pid: u32,
    #[serde(default)]
    installed_binary_path: String,
    #[serde(default)]
    installed_binary_sha256: String,
    #[serde(default)]
    daemon_run_current_path: String,
    #[serde(default)]
    daemon_run_current_sha256: String,
    #[serde(default)]
    setup_script_path: String,
    #[serde(default)]
    setup_script_sha256: String,
    #[serde(default)]
    chrome_native_host_exe_path: String,
    #[serde(default)]
    chrome_native_host_exe_sha256: String,
}

fn launch_setup_repair(
    service: &SynapseService,
    reason: &str,
    chrome_bridge_preflight: &str,
    plan: &SetupRepairPlan,
) -> Result<SetupRepairLaunchReadback, ErrorData> {
    let bind = service.m3_bind_addr()?;
    let source_dir = setup_source_dir()?;
    let setup_script_path = setup_script_path(&source_dir)?;
    let launcher_path = powershell_launcher_path()?;
    let started_at_unix_ms = unix_now_ms()?;
    let run_id = format!("repair-{}-{started_at_unix_ms}", std::process::id());
    let run_dir = localappdata_path(["synapse", "setup-repair-runs", run_id.as_str()]);
    fs::create_dir_all(&run_dir).map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_RUN_DIR_CREATE_FAILED",
            "run_dir",
            format!(
                "setup repair could not create run_dir={} error={}",
                run_dir.display(),
                error
            ),
            "repair permissions on %LOCALAPPDATA%\\synapse and retry setup repair",
        )
    })?;

    let manifest_path = run_dir.join("repair-run.json");
    let stdout_path = run_dir.join("stdout.log");
    let stderr_path = run_dir.join("stderr.log");
    let args = setup_repair_command_args(&setup_script_path, &source_dir, &bind, plan);
    let active_issue = setup_repair_active_issue_from_reason(reason);

    write_setup_repair_manifest(
        &manifest_path,
        &SetupRepairRunManifest {
            schema: "synapse_setup_repair_run/v1",
            state: "launching",
            run_id: &run_id,
            reason,
            source_dir: source_dir.display().to_string(),
            setup_script_path: setup_script_path.display().to_string(),
            bind: bind.clone(),
            launcher_path: launcher_path.display().to_string(),
            stdout_log: stdout_path.display().to_string(),
            stderr_log: stderr_path.display().to_string(),
            child_pid: None,
            started_at_unix_ms,
            command_args: args.clone(),
            active_issue: active_issue.clone(),
            repair_mode: plan.mode(),
            chrome_bridge_preflight: chrome_bridge_preflight.to_owned(),
            remediation: "inspect stdout/stderr and daemon process/socket readback after the external setup process exits",
        },
    )?;

    let stdout = create_repair_log(&stdout_path)?;
    let stderr = create_repair_log(&stderr_path)?;
    let mut command = Command::new(&launcher_path);
    command
        .args(&args)
        .current_dir(&source_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .env("SYNAPSE_SETUP_REPAIR_REASON", reason)
        .env("SYNAPSE_SETUP_REPAIR_MANIFEST", &manifest_path)
        .env("SYNAPSE_SETUP_INVOCATION_ID", &run_id);
    if let Some(active_issue) = active_issue.as_deref() {
        command.env("SYNAPSE_ACTIVE_ISSUE", active_issue);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let child = command.spawn().map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_PROCESS_SPAWN_FAILED",
            "process_spawn",
            format!(
                "setup repair could not spawn launcher={} script={} error={}",
                launcher_path.display(),
                setup_script_path.display(),
                error
            ),
            "verify PowerShell, scripts\\synapse-setup.ps1, and source directory permissions, then retry setup repair",
        )
    })?;
    let child_pid = child.id();
    drop(child);

    write_setup_repair_manifest(
        &manifest_path,
        &SetupRepairRunManifest {
            schema: "synapse_setup_repair_run/v1",
            state: "started",
            run_id: &run_id,
            reason,
            source_dir: source_dir.display().to_string(),
            setup_script_path: setup_script_path.display().to_string(),
            bind,
            launcher_path: launcher_path.display().to_string(),
            stdout_log: stdout_path.display().to_string(),
            stderr_log: stderr_path.display().to_string(),
            child_pid: Some(child_pid),
            started_at_unix_ms,
            command_args: args,
            active_issue,
            repair_mode: plan.mode(),
            chrome_bridge_preflight: chrome_bridge_preflight.to_owned(),
            remediation: "inspect stdout/stderr and daemon process/socket readback after the external setup process exits",
        },
    )?;

    Ok(SetupRepairLaunchReadback {
        run_id,
        run_dir,
        manifest_path,
        stdout_path,
        stderr_path,
        setup_script_path,
        source_dir,
        launcher_path,
        repair_mode: plan.mode(),
        chrome_bridge_preflight: chrome_bridge_preflight.to_owned(),
        child_pid,
    })
}

async fn preflight_setup_repair_chrome_bridge() -> Result<String, ErrorData> {
    match chrome_debugger_bridge::wait_for_active_bridge_host(250).await {
        Ok(host)
            if !host.extension_stale
                && host.extension_service_worker_sha256_status.as_deref() == Some("ok")
                && host
                    .extension_service_worker_sha256
                    .as_deref()
                    .is_some_and(|actual| {
                        !actual.is_empty()
                            && host.expected_service_worker_sha256.as_deref() == Some(actual)
                    })
                // The authenticated normal-profile bridge is deliberately
                // debugger-free (#1249). Deep CDP operations belong to the
                // daemon-owned isolated browser lane. Requiring `true` here
                // made setup reload an already exact popup-free host, which
                // could leave an unpacked MV3 worker dormant after
                // chrome.runtime.reload(). Require an explicit negative
                // readback instead: `None` is unknown and `true` violates the
                // normal-bridge security boundary, so both remain fail-closed.
                && host.extension_debugger_api_available == Some(false)
                && host
                    .extension_capabilities
                    .iter()
                    .any(|capability| capability == "maintenancePauseReconnect") =>
        {
            return Ok(format!(
                "chrome_bridge_preflight=current_debugger_free_host_verified host_id={} service_worker_sha256={} service_worker_sha256_status={} debugger_api_available=false maintenance_pause_capability=true",
                host.host_id,
                host.extension_service_worker_sha256
                    .as_deref()
                    .unwrap_or("<missing>"),
                host.extension_service_worker_sha256_status
                    .as_deref()
                    .unwrap_or("<missing>")
            ));
        }
        Ok(_stale_or_incomplete_host) => {}
        Err(error)
            if error.code() == error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE
                && error.detail().contains("no_active_chrome_bridge_host") =>
        {
            return Ok(
                "chrome_bridge_preflight=reload_bridge_skipped reason=no_active_chrome_bridge_host"
                    .to_owned(),
            );
        }
        Err(error) => {
            return Err(setup_repair_error(
                "SYNAPSE_SETUP_REPAIR_CHROME_BRIDGE_READBACK_FAILED",
                "chrome_bridge_host_readback",
                format!(
                    "setup repair could not read the active Chrome bridge identity before external maintenance handoff; code={} detail={}",
                    error.code(),
                    error.detail()
                ),
                "repair the exact bridge host readback failure and retry setup repair; setup did not launch or alter daemon restart authority",
            ));
        }
    }

    match chrome_debugger_bridge::reload_bridge(30_000).await {
        Ok(result) => Ok(format!(
            "chrome_bridge_preflight=background_runtime_reload_ok before_host={} after_host={} reconnected={} waited_ms={} control_surface={} active_profile={}",
            result
                .before
                .as_ref()
                .map_or("<none>", |before| before.host_id.as_str()),
            result.after.host_id,
            result.reconnected,
            result.waited_ms,
            result.command_ack.control_surface,
            result.command_ack.active_profile
        )),
        Err(error)
            if error.code() == error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE
                && error.detail().contains("no_active_chrome_bridge_host") =>
        {
            Ok(
                "chrome_bridge_preflight=reload_bridge_skipped reason=no_active_chrome_bridge_host"
                    .to_owned(),
            )
        }
        Err(error) => Err(setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_CHROME_BRIDGE_BACKGROUND_RELOAD_FAILED",
            "chrome_bridge_background_reload",
            format!(
                "setup repair could not reconcile the Chrome bridge through its background runtime lifecycle before external maintenance handoff; code={} detail={}",
                error.code(),
                error.detail()
            ),
            "repair the exact connected-host, PowerShell deployment, Chrome profile-row, service-worker SHA, or replacement-host condition in detail and retry setup repair; setup never touches a human Chrome window",
        )),
    }
}

fn setup_repair_command_args(
    setup_script_path: &Path,
    source_dir: &Path,
    bind: &str,
    plan: &SetupRepairPlan,
) -> Vec<String> {
    let mut args = vec![
        "-NoProfile".to_owned(),
        "-ExecutionPolicy".to_owned(),
        "Bypass".to_owned(),
        "-File".to_owned(),
        setup_script_path.display().to_string(),
        "-SourceDir".to_owned(),
        source_dir.display().to_string(),
    ];
    match plan {
        SetupRepairPlan::Full => {
            args.extend([
                "-Bind".to_owned(),
                bind.to_owned(),
                "-ForceRestart".to_owned(),
            ]);
        }
        SetupRepairPlan::ResumeChromeBridge {
            checkpoint_path,
            maintenance_lock_path,
        } => {
            args.extend([
                "-ResumeChromeBridgePending".to_owned(),
                "-ChromeBridgePendingPath".to_owned(),
                checkpoint_path.display().to_string(),
                "-MaintenanceLockPath".to_owned(),
                maintenance_lock_path.display().to_string(),
            ]);
        }
    }
    args
}

fn setup_repair_plan() -> Result<SetupRepairPlan, ErrorData> {
    let checkpoint_path = localappdata_path(["synapse", "setup-chrome-bridge-pending.json"]);
    let bytes = match fs::read(&checkpoint_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SetupRepairPlan::Full);
        }
        Err(error) => {
            return Err(setup_repair_error(
                "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_READ_FAILED",
                "chrome_bridge_checkpoint",
                format!(
                    "setup repair could not read Chrome bridge checkpoint path={} error={}",
                    checkpoint_path.display(),
                    error
                ),
                "repair checkpoint file permissions, inspect its contents, and retry setup repair",
            ));
        }
    };
    let checkpoint: SetupBridgeCheckpointEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| {
            setup_repair_error(
                "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_JSON_INVALID",
                "chrome_bridge_checkpoint",
                format!(
                    "setup repair found an unreadable Chrome bridge checkpoint path={} error={}",
                    checkpoint_path.display(),
                    error
                ),
                "inspect and repair the checkpoint JSON; setup refuses to replace a possibly pending phase with an unrelated full repair",
            )
        })?;
    if matches!(
        checkpoint.schema.as_str(),
        "synapse_setup_bridge_pending/v2" | "synapse_setup_bridge_pending/v3"
    ) {
        // v2 predates deployment-generation binding; v3 does not bind the
        // installed native-host bytes. Both are historical evidence, never
        // resumable authority for a current package generation.
        return Ok(SetupRepairPlan::Full);
    }
    if checkpoint.schema != "synapse_setup_bridge_pending/v4" {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_SCHEMA_INVALID",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair found unsupported Chrome bridge checkpoint schema={} path={}",
                checkpoint.schema,
                checkpoint_path.display()
            ),
            "inspect the checkpoint; setup refuses to delete or reinterpret an unknown transaction schema",
        ));
    }
    if checkpoint.phase != "chrome_bridge_activation" {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_PHASE_INVALID",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair found unsupported Chrome bridge checkpoint phase={} path={}",
                checkpoint.phase,
                checkpoint_path.display()
            ),
            "inspect the checkpoint phase; setup refuses to infer or replay an unknown continuation",
        ));
    }
    if matches!(checkpoint.state.as_str(), "completed" | "superseded") {
        return Ok(SetupRepairPlan::Full);
    }
    if checkpoint.state != "pending" {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_STATE_INVALID",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair found unsupported Chrome bridge checkpoint state={} path={}",
                checkpoint.state,
                checkpoint_path.display()
            ),
            "inspect the checkpoint state; only pending can resume and terminal completed/superseded records permit a new full repair",
        ));
    }
    if checkpoint.checkpoint_generation_id.trim().is_empty() {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_GENERATION_MISSING",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair found a pending Chrome bridge checkpoint without checkpoint_generation_id path={}",
                checkpoint_path.display()
            ),
            "inspect the checkpoint; a pending phase without an owning setup generation is never resumable",
        ));
    }
    if checkpoint.maintenance_lock_path.trim().is_empty() {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_MAINTENANCE_LOCK_MISSING",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair found a Chrome bridge checkpoint without maintenance_lock_path path={}",
                checkpoint_path.display()
            ),
            "inspect the checkpoint; a phase resume must reacquire the exact setup maintenance lock that protected the committed daemon handoff",
        ));
    }
    if checkpoint.daemon_pid != std::process::id() {
        return Ok(SetupRepairPlan::Full);
    }
    for (kind, path, expected_sha256) in [
        (
            "installed_binary",
            checkpoint.installed_binary_path.as_str(),
            checkpoint.installed_binary_sha256.as_str(),
        ),
        (
            "daemon_run_current",
            checkpoint.daemon_run_current_path.as_str(),
            checkpoint.daemon_run_current_sha256.as_str(),
        ),
        (
            "setup_script",
            checkpoint.setup_script_path.as_str(),
            checkpoint.setup_script_sha256.as_str(),
        ),
        (
            "chrome_native_host",
            checkpoint.chrome_native_host_exe_path.as_str(),
            checkpoint.chrome_native_host_exe_sha256.as_str(),
        ),
    ] {
        if path.trim().is_empty() || expected_sha256.trim().is_empty() {
            return Err(setup_repair_error(
                "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_IDENTITY_MISSING",
                "chrome_bridge_checkpoint",
                format!(
                    "setup repair found a current-PID pending checkpoint with incomplete identity kind={} path={}",
                    kind,
                    checkpoint_path.display()
                ),
                "inspect the checkpoint; a pending phase resumes only when every generation-bound file identity is present",
            ));
        }
        let actual = fs::read(path).map_err(|error| {
            setup_repair_error(
                "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_IDENTITY_READ_FAILED",
                "chrome_bridge_checkpoint",
                format!(
                    "setup repair could not read current-PID checkpoint identity kind={} file={} checkpoint={} error={}",
                    kind,
                    path,
                    checkpoint_path.display(),
                    error
                ),
                "repair the exact unreadable checkpointed file or perform a new full setup after preserving the stale record",
            )
        })?;
        let actual_sha256 = sha256_hex(&actual);
        if !actual_sha256.eq_ignore_ascii_case(expected_sha256.trim_start_matches("sha256:")) {
            return Err(setup_repair_error(
                "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_IDENTITY_DRIFT",
                "chrome_bridge_checkpoint",
                format!(
                    "setup repair found a current-generation pending checkpoint with changed bytes kind={} file={} expected_sha256={} actual_sha256={} checkpoint={}",
                    kind,
                    path,
                    expected_sha256,
                    actual_sha256,
                    checkpoint_path.display()
                ),
                "preserve the checkpoint and inspect the named identity drift; a current-generation pending transaction is never silently replaced",
            ));
        }
    }
    let current_executable = std::env::current_exe().map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CURRENT_EXE_READ_FAILED",
            "chrome_bridge_checkpoint",
            format!("setup repair could not resolve the live daemon executable: {error}"),
            "repair process executable-path access and retry setup status",
        )
    })?;
    let checkpoint_executable = fs::canonicalize(&checkpoint.installed_binary_path).map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_EXE_CANONICALIZE_FAILED",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair could not canonicalize checkpoint executable path={} error={}",
                checkpoint.installed_binary_path, error
            ),
            "repair the checkpointed executable path or perform a new full setup after preserving the stale record",
        )
    })?;
    let current_executable = fs::canonicalize(&current_executable).map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CURRENT_EXE_CANONICALIZE_FAILED",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair could not canonicalize live daemon executable path={} error={}",
                current_executable.display(),
                error
            ),
            "repair process executable-path access and retry setup status",
        )
    })?;
    #[cfg(windows)]
    let executable_matches = current_executable
        .to_string_lossy()
        .eq_ignore_ascii_case(&checkpoint_executable.to_string_lossy());
    #[cfg(not(windows))]
    let executable_matches = current_executable == checkpoint_executable;
    if !executable_matches {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_BRIDGE_CHECKPOINT_EXE_DRIFT",
            "chrome_bridge_checkpoint",
            format!(
                "setup repair found a current-generation pending checkpoint executable mismatch expected={} actual={} checkpoint={}",
                checkpoint_executable.display(),
                current_executable.display(),
                checkpoint_path.display()
            ),
            "preserve the checkpoint and inspect executable identity drift; a current-generation pending transaction is never silently replaced",
        ));
    }
    Ok(SetupRepairPlan::ResumeChromeBridge {
        checkpoint_path,
        maintenance_lock_path: PathBuf::from(checkpoint.maintenance_lock_path),
    })
}

fn setup_repair_active_issue_from_reason(reason: &str) -> Option<String> {
    reason
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '#'))
        .find_map(|token| {
            if let Some(number) = token.strip_prefix('#') {
                if !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit()) {
                    return Some(format!("#{number}"));
                }
            }
            let lower = token.to_ascii_lowercase();
            if let Some(number) = lower.strip_prefix("issue") {
                if !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit()) {
                    return Some(format!("#{number}"));
                }
            }
            None
        })
}

fn setup_source_dir() -> Result<PathBuf, ErrorData> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(source_dir) = manifest_dir.parent().and_then(Path::parent) else {
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_SOURCE_DIR_UNRESOLVED",
            "source_dir",
            format!(
                "setup repair could not resolve source checkout from CARGO_MANIFEST_DIR={}",
                manifest_dir.display()
            ),
            "rebuild synapse-mcp from a real Synapse source checkout and retry setup repair",
        ));
    };
    let source_dir = source_dir.to_path_buf();
    let _ = setup_script_path(&source_dir)?;
    Ok(source_dir)
}

fn setup_script_path(source_dir: &Path) -> Result<PathBuf, ErrorData> {
    let path = source_dir.join("scripts").join("synapse-setup.ps1");
    if path.is_file() {
        return Ok(path);
    }
    Err(setup_repair_error(
        "SYNAPSE_SETUP_REPAIR_SCRIPT_MISSING",
        "setup_script",
        format!(
            "setup repair requires scripts\\synapse-setup.ps1 at path={}",
            path.display()
        ),
        "run setup repair from a repo-built daemon whose source checkout still contains scripts\\synapse-setup.ps1",
    ))
}

fn powershell_launcher_path() -> Result<PathBuf, ErrorData> {
    #[cfg(windows)]
    {
        let system_root = std::env::var("SystemRoot").map_err(|error| {
            setup_repair_error(
                "SYNAPSE_SETUP_REPAIR_SYSTEMROOT_MISSING",
                "SystemRoot",
                format!("setup repair cannot resolve SystemRoot: {error}"),
                "repair the Windows process environment so SystemRoot points at the Windows directory",
            )
        })?;
        let path = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if path.is_file() {
            return Ok(path);
        }
        return Err(setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_POWERSHELL_MISSING",
            "powershell",
            format!(
                "setup repair requires Windows PowerShell at path={}",
                path.display()
            ),
            "repair the Windows PowerShell installation or run setup from a host with powershell.exe",
        ));
    }
    #[cfg(not(windows))]
    {
        Err(setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_UNSUPPORTED_PLATFORM",
            "platform",
            "setup repair currently requires Windows PowerShell and the Windows daemon host"
                .to_owned(),
            "run setup repair on the configured Windows Synapse host",
        ))
    }
}

fn create_repair_log(path: &Path) -> Result<File, ErrorData> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            setup_repair_error(
                "SYNAPSE_SETUP_REPAIR_LOG_OPEN_FAILED",
                "repair_log",
                format!(
                    "setup repair could not open log path={} error={}",
                    path.display(),
                    error
                ),
                "repair permissions on the setup repair run directory and retry",
            )
        })
}

fn write_setup_repair_manifest(
    path: &Path,
    manifest: &SetupRepairRunManifest<'_>,
) -> Result<(), ErrorData> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_MANIFEST_SERIALIZE_FAILED",
            "repair_manifest",
            format!("setup repair could not serialize manifest error={error}"),
            "fix manifest serialization fields and retry setup repair",
        )
    })?;
    fs::write(path, bytes).map_err(|error| {
        setup_repair_error(
            "SYNAPSE_SETUP_REPAIR_MANIFEST_WRITE_FAILED",
            "repair_manifest",
            format!(
                "setup repair could not write manifest path={} error={}",
                path.display(),
                error
            ),
            "repair permissions on the setup repair run directory and retry",
        )
    })
}

fn unix_now_ms() -> Result<u128, ErrorData> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| {
            setup_repair_error(
                "SYNAPSE_SETUP_REPAIR_CLOCK_BEFORE_EPOCH",
                "system_clock",
                format!(
                    "setup repair cannot create run id because system clock is invalid: {error}"
                ),
                "repair the host system clock and retry setup repair",
            )
        })
}

fn setup_repair_error(
    code: &'static str,
    source_id: &'static str,
    message: String,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": code,
            "tool": SETUP_TOOL,
            "operation": "repair",
            "source_id": source_id,
            "source_of_truth": SETUP_SOT,
            "remediation": remediation,
        })),
    )
}

pub(super) fn setup_status(service: &SynapseService) -> Result<SetupStatusResponse, ErrorData> {
    let bind = service.m3_bind_addr()?;
    let source_dir = setup_source_dir()?;
    let setup_script = setup_script_path(&source_dir)?;
    let plan = setup_repair_plan()?;
    let setup_repair_command_args =
        setup_repair_command_args(&setup_script, &source_dir, &bind, &plan);
    let token_file = file_readback(appdata_path(["synapse", "token.txt"]));
    let daemon_run_file = active_daemon_run_file()?;
    let shared_daemon_run_file = file_readback(shared_daemon_run_file_path());
    let codex_config_file = file_readback(userprofile_path([".codex", "config.toml"]));
    let codex_text = fs::read_to_string(codex_config_file.path.as_str()).unwrap_or_default();
    let token_env = std::env::var("SYNAPSE_BEARER_TOKEN").ok();
    Ok(SetupStatusResponse {
        source_of_truth: SETUP_SOT,
        pid: std::process::id(),
        bind,
        source_dir: source_dir.display().to_string(),
        setup_script_file: file_readback(setup_script),
        setup_repair_command_args,
        setup_repair_mcp_tool: "setup operation=repair repair.reason=<reason> profile=maintenance"
            .to_owned(),
        token_file,
        daemon_run_file,
        shared_daemon_run_file,
        codex_config_file,
        token_env_present: token_env.is_some(),
        token_env_len_bytes: token_env.as_ref().map(|value| value.len()),
        codex_mcp_config_mentions_synapse: codex_text.contains("[mcp_servers.synapse]")
            || codex_text.contains("synapse"),
        codex_mcp_config_mentions_bearer_env: codex_text.contains("SYNAPSE_BEARER_TOKEN"),
        autostart: autostart_readback(),
    })
}

/// Reads the daemon autostart task and proves its launcher exists (#1862).
///
/// Task state is not evidence: a task whose action targets a deleted file still
/// reports `Ready`. The launcher used to live in the log directory, so emptying
/// logs deleted it and autostart died silently. This names the exact defect.
#[cfg(windows)]
fn autostart_readback() -> super::types::SetupAutostartReadback {
    use super::types::SetupAutostartReadback;

    const TASK_NAME: &str = "SynapseMcpDaemon";
    let log_dir = localappdata_path(["synapse", "logs"]);
    let mut problems = Vec::new();

    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$t = Get-ScheduledTask -TaskName '{TASK_NAME}' -ErrorAction SilentlyContinue; \
                 if (-not $t) {{ 'NOTREGISTERED' }} else {{ \
                 $a = @($t.Actions)[0]; \
                 \"$($t.State)`n$($a.Execute)`n$($a.Arguments)\" }}"
            ),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output();

    let stdout = match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        Ok(output) => {
            problems.push(format!(
                "SYNAPSE_AUTOSTART_QUERY_FAILED exit_code={:?} stderr={} remediation=inspect Task \
                 Scheduler access for this account",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
            String::new()
        }
        Err(error) => {
            problems.push(format!(
                "SYNAPSE_AUTOSTART_QUERY_FAILED error={error} remediation=inspect whether \
                 powershell.exe is available to this daemon process"
            ));
            String::new()
        }
    };

    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "NOTREGISTERED" {
        if trimmed == "NOTREGISTERED" {
            problems.push(format!(
                "SYNAPSE_AUTOSTART_TASK_MISSING task={TASK_NAME} remediation=the daemon will not \
                 start at logon; re-run scripts/synapse-setup.ps1 to register it"
            ));
        }
        return SetupAutostartReadback {
            task_name: TASK_NAME.to_owned(),
            task_registered: false,
            task_state: None,
            action_execute: None,
            action_arguments: None,
            launcher_path: None,
            launcher_file: None,
            can_start_daemon: false,
            launcher_in_log_dir: false,
            problems,
        };
    }

    let mut lines = trimmed.lines();
    let task_state = lines.next().unwrap_or_default().trim().to_owned();
    let action_execute = lines.next().unwrap_or_default().trim().to_owned();
    let action_arguments = lines.collect::<Vec<_>>().join("\n").trim().to_owned();

    let launcher_path = action_arguments
        .split('"')
        .find(|segment| segment.to_ascii_lowercase().ends_with(".vbs"))
        .map(str::to_owned);

    let (launcher_file, can_start_daemon, launcher_in_log_dir) = match &launcher_path {
        Some(path) => {
            let readback = file_readback(PathBuf::from(path));
            let exists = readback.exists;
            if !exists {
                problems.push(format!(
                    "SYNAPSE_AUTOSTART_LAUNCHER_MISSING task={TASK_NAME} task_state={task_state} \
                     launcher={path} remediation=the task is registered and reports \
                     State={task_state}, but its launcher file does not exist so it can never \
                     start the daemon; re-run scripts/synapse-setup.ps1"
                ));
            }
            let in_log_dir = Path::new(path).starts_with(&log_dir);
            if in_log_dir {
                problems.push(format!(
                    "SYNAPSE_AUTOSTART_LAUNCHER_IN_LOG_DIR task={TASK_NAME} launcher={path} \
                     log_dir={} remediation=the launcher lives in the log directory, so routine \
                     log cleanup will delete it and silently disable autostart; re-run \
                     scripts/synapse-setup.ps1 to move it into the runtime bin directory",
                    log_dir.display()
                ));
            }
            (Some(readback), exists, in_log_dir)
        }
        None => {
            problems.push(format!(
                "SYNAPSE_AUTOSTART_TASK_ACTION_UNPARSEABLE task={TASK_NAME} \
                 arguments={action_arguments} remediation=the registered action does not name a \
                 quoted .vbs launcher; re-run scripts/synapse-setup.ps1"
            ));
            (None, false, false)
        }
    };

    SetupAutostartReadback {
        task_name: TASK_NAME.to_owned(),
        task_registered: true,
        task_state: Some(task_state),
        action_execute: Some(action_execute),
        action_arguments: Some(action_arguments),
        launcher_path,
        launcher_file,
        can_start_daemon,
        launcher_in_log_dir,
        problems,
    }
}

#[cfg(not(windows))]
fn autostart_readback() -> super::types::SetupAutostartReadback {
    super::types::SetupAutostartReadback {
        task_name: String::new(),
        task_registered: false,
        task_state: None,
        action_execute: None,
        action_arguments: None,
        launcher_path: None,
        launcher_file: None,
        can_start_daemon: false,
        launcher_in_log_dir: false,
        problems: vec![
            "SYNAPSE_AUTOSTART_UNSUPPORTED_PLATFORM remediation=daemon autostart is registered \
             through Windows Task Scheduler; this platform has no equivalent readback"
                .to_owned(),
        ],
    }
}

fn active_daemon_run_file() -> Result<FileReadback, ErrorData> {
    let Some(paths) = crate::daemon_lifecycle::current_paths() else {
        return Err(mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "setup.status cannot identify the active daemon run file because the daemon lifecycle ledger is not configured",
        ));
    };
    Ok(file_readback(PathBuf::from(paths.run_current_path)))
}

fn shared_daemon_run_file_path() -> PathBuf {
    localappdata_path(["synapse", "db-daemon", "daemon-run-current.json"])
}

fn file_readback(path: PathBuf) -> FileReadback {
    match fs::read(&path) {
        Ok(bytes) => FileReadback {
            path: path.display().to_string(),
            exists: true,
            len_bytes: Some(bytes.len() as u64),
            sha256: Some(format!("sha256:{}", sha256_hex(&bytes))),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileReadback {
            path: path.display().to_string(),
            exists: false,
            len_bytes: None,
            sha256: None,
        },
        Err(error) => FileReadback {
            path: path.display().to_string(),
            exists: false,
            len_bytes: Some(error.raw_os_error().unwrap_or_default() as u64),
            sha256: None,
        },
    }
}

fn appdata_path<const N: usize>(parts: [&str; N]) -> PathBuf {
    env_path("APPDATA", "C:\\Users\\Default\\AppData\\Roaming", parts)
}

fn localappdata_path<const N: usize>(parts: [&str; N]) -> PathBuf {
    env_path("LOCALAPPDATA", "C:\\Users\\Default\\AppData\\Local", parts)
}

fn userprofile_path<const N: usize>(parts: [&str; N]) -> PathBuf {
    env_path("USERPROFILE", "C:\\Users\\Default", parts)
}

fn env_path<const N: usize>(name: &str, fallback: &str, parts: [&str; N]) -> PathBuf {
    let mut path = PathBuf::from(std::env::var(name).unwrap_or_else(|_| fallback.to_owned()));
    for part in parts {
        path.push(part);
    }
    path
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}
