use rmcp::model::ErrorCode;
use serde_json::json;
use synapse_core::error_codes;

use crate::server::ErrorData;

use super::{
    SETUP_SOT, SETUP_TOOL,
    types::{SetupLaunchdServiceParams, SetupLaunchdServiceResponse},
};

pub(super) async fn handle(
    params: SetupLaunchdServiceParams,
) -> Result<SetupLaunchdServiceResponse, ErrorData> {
    platform::handle(params).await
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{
        ErrorCode, ErrorData, SETUP_SOT, SETUP_TOOL, SetupLaunchdServiceParams,
        SetupLaunchdServiceResponse, error_codes, json,
    };

    pub(super) async fn handle(
        params: SetupLaunchdServiceParams,
    ) -> Result<SetupLaunchdServiceResponse, ErrorData> {
        Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "setup.launchd_service action={} is available only on macOS",
                params.action.as_str()
            ),
            Some(json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "detail_code": "SETUP_LAUNCHD_UNSUPPORTED_PLATFORM",
                "tool": SETUP_TOOL,
                "operation": "launchd_service",
                "action": params.action.as_str(),
                "reason_present": params.reason.is_some(),
                "confirmation_present": params.confirmation.is_some(),
                "source_of_truth": SETUP_SOT,
                "platform": std::env::consts::OS,
                "remediation": "run this operation on the installed macOS Synapse LaunchAgent host; generic shell policy is intentionally not a fallback",
            })),
        ))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ErrorData, SetupLaunchdServiceParams, SetupLaunchdServiceResponse};

    pub(super) async fn handle(
        params: SetupLaunchdServiceParams,
    ) -> Result<SetupLaunchdServiceResponse, ErrorData> {
        super::implementation::handle(params).await
    }
}

// The launchd implementation remains in every host's compile and Clippy
// surface. It is deliberately unreachable outside macOS; the allowance only
// suppresses that expected reachability fact, not diagnostics within the code.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod implementation {
    use std::{
        fs::{self, File, OpenOptions},
        io::Write as _,
        path::{Path, PathBuf},
        process::Stdio,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use serde::{Deserialize, Serialize};
    use sha2::{Digest as _, Sha256};

    use super::{
        ErrorCode, ErrorData, SETUP_SOT, SETUP_TOOL, SetupLaunchdServiceParams,
        SetupLaunchdServiceResponse, error_codes, json,
    };
    use crate::server::operational_facades::types::{
        SetupLaunchdCommandReadback, SetupLaunchdRestartReadback, SetupLaunchdRestartState,
        SetupLaunchdServiceAction, SetupLaunchdServiceProbe,
    };

    const LAUNCHCTL: &str = "/bin/launchctl";
    const LABEL: &str = "com.synapse.mcp";
    const RESTART_CONFIRMATION: &str = "RESTART_COM.SYNAPSE.MCP";
    const RESTART_SCHEMA: &str = "synapse_launchd_restart/v1";
    const RESTART_MANIFEST: &str = "launchd-restart-current.json";
    const COMMAND_OUTPUT_MAX_BYTES: usize = 512 * 1024;
    const MANIFEST_MAX_BYTES: u64 = 64 * 1024;
    const RESTART_PENDING_MAX_MS: u128 = 30_000;
    const REASON_MAX_BYTES: usize = 1_024;
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct RestartManifest {
        schema: String,
        request_id: String,
        state: SetupLaunchdRestartState,
        service_target: String,
        requesting_pid: u32,
        completed_pid: Option<u32>,
        requested_at_unix_ms: u128,
        completed_at_unix_ms: Option<u128>,
        reason_len_bytes: usize,
        reason_sha256: String,
        command_executable: String,
        command_args: Vec<String>,
        command_exit_code: Option<i32>,
        before_query_sha256: String,
        after_query_sha256: Option<String>,
        failure_code: Option<String>,
    }

    struct CommandOutcome {
        readback: SetupLaunchdCommandReadback,
        stdout: String,
        stderr: String,
    }

    pub(super) async fn handle(
        params: SetupLaunchdServiceParams,
    ) -> Result<SetupLaunchdServiceResponse, ErrorData> {
        match params.action {
            SetupLaunchdServiceAction::Status => status(params).await,
            SetupLaunchdServiceAction::Restart => restart(params).await,
        }
    }

    async fn status(
        params: SetupLaunchdServiceParams,
    ) -> Result<SetupLaunchdServiceResponse, ErrorData> {
        if params.reason.is_some() || params.confirmation.is_some() {
            return Err(invalid_params(
                "status accepts neither reason nor confirmation",
                "launchd_service",
            ));
        }
        let probe = probe_service().await?;
        let manifest_path = restart_manifest_path()?;
        let restart = reconcile_restart_manifest(&manifest_path, &probe)?
            .map(|manifest| restart_readback(&manifest_path, &manifest))
            .transpose()?;
        Ok(response(
            SetupLaunchdServiceAction::Status,
            probe,
            restart,
            &manifest_path,
        ))
    }

    async fn restart(
        params: SetupLaunchdServiceParams,
    ) -> Result<SetupLaunchdServiceResponse, ErrorData> {
        let reason = params.reason.as_deref().ok_or_else(|| {
            invalid_params(
                "restart requires a non-empty reason",
                "launchd_service.reason",
            )
        })?;
        if reason.trim().is_empty() || reason.len() > REASON_MAX_BYTES {
            return Err(invalid_params(
                format!("restart reason must contain 1..={REASON_MAX_BYTES} bytes after trimming"),
                "launchd_service.reason",
            ));
        }
        if params.confirmation.as_deref() != Some(RESTART_CONFIRMATION) {
            return Err(invalid_params(
                format!("restart confirmation must equal {RESTART_CONFIRMATION:?}"),
                "launchd_service.confirmation",
            ));
        }

        let before = probe_service().await?;
        let current_pid = std::process::id();
        if !before.registered
            || before.state.as_deref() != Some("running")
            || before.pid != Some(current_pid)
        {
            return Err(launchd_error(
                "SETUP_LAUNCHD_RESTART_IDENTITY_MISMATCH",
                "restart",
                format!(
                    "refusing launchd restart: target={} registered={} state={:?} launchd_pid={:?} current_pid={current_pid}",
                    before.service_target, before.registered, before.state, before.pid
                ),
                "inspect setup.launchd_service action=status from the launchd-owned daemon; repair the exact gui-domain registration before retrying",
            ));
        }

        let manifest_path = restart_manifest_path()?;
        if let Some(existing) = reconcile_restart_manifest(&manifest_path, &before)? {
            if existing.state == SetupLaunchdRestartState::Requested {
                return Err(launchd_error(
                    "SETUP_LAUNCHD_RESTART_ALREADY_PENDING",
                    "restart",
                    format!(
                        "restart request {} is still pending in {}",
                        existing.request_id,
                        manifest_path.display()
                    ),
                    "read setup.launchd_service action=status until the prior durable restart request is completed or failed before issuing another restart",
                ));
            }
        }

        let command_args = vec![
            "kickstart".to_owned(),
            "-kp".to_owned(),
            before.service_target.clone(),
        ];
        let now = now_unix_ms()?;
        let mut manifest = RestartManifest {
            schema: RESTART_SCHEMA.to_owned(),
            request_id: format!("launchd-{}-{}", now, uuid::Uuid::new_v4().simple()),
            state: SetupLaunchdRestartState::Requested,
            service_target: before.service_target.clone(),
            requesting_pid: current_pid,
            completed_pid: None,
            requested_at_unix_ms: now,
            completed_at_unix_ms: None,
            reason_len_bytes: reason.len(),
            reason_sha256: prefixed_sha256(reason.as_bytes()),
            command_executable: LAUNCHCTL.to_owned(),
            command_args: command_args.clone(),
            command_exit_code: None,
            before_query_sha256: before.query.stdout_sha256.clone(),
            after_query_sha256: None,
            failure_code: None,
        };
        write_manifest_atomic(&manifest_path, &manifest)?;
        tracing::warn!(
            code = "SETUP_LAUNCHD_RESTART_REQUESTED",
            request_id = %manifest.request_id,
            service_target = %manifest.service_target,
            requesting_pid = manifest.requesting_pid,
            manifest_path = %manifest_path.display(),
            command_executable = LAUNCHCTL,
            command_args = ?command_args,
            reason_len_bytes = manifest.reason_len_bytes,
            reason_sha256 = %manifest.reason_sha256,
            "persisted exact launchd restart intent before invoking the command that is expected to terminate this MCP transport"
        );

        // `-k` is expected to terminate this process. In that normal path this
        // call never returns to the client; the next daemon reconciles the
        // requested manifest against its own PID and `launchctl print`.
        let outcome = run_launchctl(&command_args, "restart").await?;
        manifest.command_exit_code = Some(outcome.readback.exit_code);
        if outcome.readback.exit_code != 0 {
            manifest.state = SetupLaunchdRestartState::Failed;
            manifest.completed_at_unix_ms = Some(now_unix_ms()?);
            manifest.failure_code = Some("SETUP_LAUNCHD_KICKSTART_FAILED".to_owned());
            write_manifest_atomic(&manifest_path, &manifest)?;
            return Err(launchd_error(
                "SETUP_LAUNCHD_KICKSTART_FAILED",
                "restart",
                format!(
                    "{} {:?} failed exit_code={} stderr={}",
                    LAUNCHCTL,
                    command_args,
                    outcome.readback.exit_code,
                    output_preview(&outcome.stderr)
                ),
                "inspect the exact launchd GUI-domain registration and the durable restart manifest, repair launchd ownership/registration, then retry",
            ));
        }

        let after = probe_service().await?;
        if after.registered && after.pid.is_some() && after.pid != before.pid {
            manifest.state = SetupLaunchdRestartState::Completed;
            manifest.completed_pid = after.pid;
            manifest.completed_at_unix_ms = Some(now_unix_ms()?);
            manifest.after_query_sha256 = Some(after.query.stdout_sha256.clone());
            write_manifest_atomic(&manifest_path, &manifest)?;
            return Ok(response(
                SetupLaunchdServiceAction::Restart,
                after,
                Some(restart_readback(&manifest_path, &manifest)?),
                &manifest_path,
            ));
        }

        manifest.state = SetupLaunchdRestartState::Failed;
        manifest.completed_at_unix_ms = Some(now_unix_ms()?);
        manifest.after_query_sha256 = Some(after.query.stdout_sha256.clone());
        manifest.failure_code = Some("SETUP_LAUNCHD_RESTART_NOT_OBSERVED".to_owned());
        write_manifest_atomic(&manifest_path, &manifest)?;
        Err(launchd_error(
            "SETUP_LAUNCHD_RESTART_NOT_OBSERVED",
            "restart",
            format!(
                "launchctl returned success but independent print readback did not observe a replacement: before_pid={:?} after_pid={:?} after_state={:?}",
                before.pid, after.pid, after.state
            ),
            "inspect the durable restart manifest and launchd print output; do not retry until the exact target's PID transition is understood",
        ))
    }

    fn response(
        action: SetupLaunchdServiceAction,
        service: SetupLaunchdServiceProbe,
        restart: Option<SetupLaunchdRestartReadback>,
        manifest_path: &Path,
    ) -> SetupLaunchdServiceResponse {
        SetupLaunchdServiceResponse {
            action,
            source_of_truth: format!(
                "{} print {} + {}",
                LAUNCHCTL,
                service.service_target,
                manifest_path.display()
            ),
            service,
            restart,
        }
    }

    async fn probe_service() -> Result<SetupLaunchdServiceProbe, ErrorData> {
        let effective_uid = effective_uid()?;
        let service_target = format!("gui/{effective_uid}/{LABEL}");
        let args = vec!["print".to_owned(), service_target.clone()];
        let outcome = run_launchctl(&args, "status").await?;
        if outcome.readback.exit_code != 0 {
            return Err(launchd_error(
                "SETUP_LAUNCHD_PRINT_FAILED",
                "status",
                format!(
                    "{} {:?} failed exit_code={} stdout_sha256={} stderr_sha256={} stderr={}",
                    LAUNCHCTL,
                    args,
                    outcome.readback.exit_code,
                    outcome.readback.stdout_sha256,
                    outcome.readback.stderr_sha256,
                    output_preview(&outcome.stderr)
                ),
                "inspect the exact launchd GUI domain and service registration; nonzero launchctl output is never reclassified as an absent service",
            ));
        }
        let state = parse_assignment(&outcome.stdout, "state")?.ok_or_else(|| {
            launchd_error(
                "SETUP_LAUNCHD_PRINT_STATE_MISSING",
                "status",
                format!(
                    "launchctl print succeeded for {service_target} but emitted no unique state assignment; stdout_sha256={}",
                    outcome.readback.stdout_sha256
                ),
                "inspect the exact launchctl print output on the macOS host; update the parser only after confirming the installed launchctl format",
            )
        })?;
        let pid = parse_assignment(&outcome.stdout, "pid")?
            .map(|value| {
                value.parse::<u32>().map_err(|error| {
                    launchd_error(
                        "SETUP_LAUNCHD_PRINT_PID_INVALID",
                        "status",
                        format!(
                            "launchctl print emitted non-u32 pid={value:?} for {service_target}: {error}"
                        ),
                        "inspect the exact launchctl print output and repair the service registration before retrying",
                    )
                })
            })
            .transpose()?;
        Ok(SetupLaunchdServiceProbe {
            label: LABEL.to_owned(),
            service_target,
            effective_uid,
            registered: true,
            state: Some(state),
            pid,
            query: outcome.readback,
        })
    }

    async fn run_launchctl(
        args: &[String],
        operation: &'static str,
    ) -> Result<CommandOutcome, ErrorData> {
        let mut command = tokio::process::Command::new(LAUNCHCTL);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                launchd_error(
                    "SETUP_LAUNCHD_COMMAND_TIMEOUT",
                    operation,
                    format!(
                        "{} {:?} exceeded the {} ms deadline; kill-on-drop was armed for its exact owned child",
                        LAUNCHCTL,
                        args,
                        COMMAND_TIMEOUT.as_millis()
                    ),
                    "inspect macOS launchd and unified logs for the stalled command before retrying",
                )
            })?
            .map_err(|error| {
                launchd_error(
                    "SETUP_LAUNCHD_EXEC_FAILED",
                    operation,
                    format!("failed to execute {LAUNCHCTL} {args:?}: {error}"),
                    "verify /bin/launchctl exists and is executable by the installed LaunchAgent account",
                )
            })?;
        if output.stdout.len() > COMMAND_OUTPUT_MAX_BYTES
            || output.stderr.len() > COMMAND_OUTPUT_MAX_BYTES
        {
            return Err(launchd_error(
                "SETUP_LAUNCHD_OUTPUT_TOO_LARGE",
                operation,
                format!(
                    "launchctl output exceeded the {COMMAND_OUTPUT_MAX_BYTES}-byte per-stream bound: stdout_bytes={} stderr_bytes={}",
                    output.stdout.len(),
                    output.stderr.len()
                ),
                "inspect launchctl directly for an anomalously large service record; reduce the record before retrying",
            ));
        }
        let exit_code = output.status.code().ok_or_else(|| {
            launchd_error(
                "SETUP_LAUNCHD_TERMINATED_WITHOUT_EXIT_CODE",
                operation,
                format!("{LAUNCHCTL} {args:?} terminated without an exit code"),
                "inspect macOS process and launchd logs for the signal that terminated launchctl",
            )
        })?;
        let stdout = String::from_utf8(output.stdout.clone()).map_err(|error| {
            launchd_error(
                "SETUP_LAUNCHD_STDOUT_NOT_UTF8",
                operation,
                format!(
                    "launchctl stdout was not UTF-8: valid_up_to={} stdout_sha256={}",
                    error.utf8_error().valid_up_to(),
                    prefixed_sha256(&output.stdout)
                ),
                "inspect the exact launchctl output bytes and host locale; no lossy parser fallback is used",
            )
        })?;
        let stderr = String::from_utf8(output.stderr.clone()).map_err(|error| {
            launchd_error(
                "SETUP_LAUNCHD_STDERR_NOT_UTF8",
                operation,
                format!(
                    "launchctl stderr was not UTF-8: valid_up_to={} stderr_sha256={}",
                    error.utf8_error().valid_up_to(),
                    prefixed_sha256(&output.stderr)
                ),
                "inspect the exact launchctl output bytes and host locale; no lossy parser fallback is used",
            )
        })?;
        Ok(CommandOutcome {
            readback: SetupLaunchdCommandReadback {
                executable: LAUNCHCTL.to_owned(),
                args: args.to_vec(),
                exit_code,
                stdout_len_bytes: output.stdout.len(),
                stdout_sha256: prefixed_sha256(&output.stdout),
                stderr_len_bytes: output.stderr.len(),
                stderr_sha256: prefixed_sha256(&output.stderr),
            },
            stdout,
            stderr,
        })
    }

    fn parse_assignment(stdout: &str, key: &str) -> Result<Option<String>, ErrorData> {
        let mut found: Option<String> = None;
        for line in stdout.lines() {
            let Some((candidate, value)) = line.trim().split_once('=') else {
                continue;
            };
            if candidate.trim() != key {
                continue;
            }
            let value = value.trim();
            if value.is_empty() {
                return Err(launchd_error(
                    "SETUP_LAUNCHD_PRINT_ASSIGNMENT_EMPTY",
                    "status",
                    format!("launchctl print emitted an empty {key} assignment"),
                    "inspect the exact launchctl print output and repair the service registration before retrying",
                ));
            }
            if found.as_deref().is_some_and(|prior| prior != value) {
                return Err(launchd_error(
                    "SETUP_LAUNCHD_PRINT_ASSIGNMENT_AMBIGUOUS",
                    "status",
                    format!(
                        "launchctl print emitted conflicting {key} assignments: prior={found:?} next={value:?}"
                    ),
                    "inspect the exact launchctl print output; the parser refuses to guess between conflicting service state",
                ));
            }
            found = Some(value.to_owned());
        }
        Ok(found)
    }

    fn reconcile_restart_manifest(
        path: &Path,
        probe: &SetupLaunchdServiceProbe,
    ) -> Result<Option<RestartManifest>, ErrorData> {
        let Some(mut manifest) = read_manifest(path)? else {
            return Ok(None);
        };
        if manifest.state != SetupLaunchdRestartState::Requested {
            return Ok(Some(manifest));
        }
        let current_pid = std::process::id();
        let now = now_unix_ms()?;
        if probe.registered
            && probe.state.as_deref() == Some("running")
            && probe.pid == Some(current_pid)
            && current_pid != manifest.requesting_pid
        {
            manifest.state = SetupLaunchdRestartState::Completed;
            manifest.completed_pid = Some(current_pid);
            manifest.completed_at_unix_ms = Some(now);
            manifest.after_query_sha256 = Some(probe.query.stdout_sha256.clone());
            write_manifest_atomic(path, &manifest)?;
            tracing::info!(
                code = "SETUP_LAUNCHD_RESTART_RECONCILED",
                request_id = %manifest.request_id,
                requesting_pid = manifest.requesting_pid,
                completed_pid = current_pid,
                service_target = %manifest.service_target,
                manifest_path = %path.display(),
                "new launchd-owned daemon independently reconciled the durable restart request"
            );
        } else if now.saturating_sub(manifest.requested_at_unix_ms) > RESTART_PENDING_MAX_MS {
            manifest.state = SetupLaunchdRestartState::Failed;
            manifest.completed_at_unix_ms = Some(now);
            manifest.after_query_sha256 = Some(probe.query.stdout_sha256.clone());
            manifest.failure_code = Some(if !probe.registered {
                "SETUP_LAUNCHD_SERVICE_ABSENT_AFTER_RESTART".to_owned()
            } else if probe.pid == Some(manifest.requesting_pid) {
                "SETUP_LAUNCHD_RESTART_DID_NOT_REPLACE_PROCESS".to_owned()
            } else {
                "SETUP_LAUNCHD_RESTART_IDENTITY_MISMATCH_AFTER_RESTART".to_owned()
            });
            write_manifest_atomic(path, &manifest)?;
            tracing::error!(
                code = manifest.failure_code.as_deref().unwrap_or("SETUP_LAUNCHD_RESTART_FAILED"),
                request_id = %manifest.request_id,
                requesting_pid = manifest.requesting_pid,
                current_pid,
                launchd_pid = ?probe.pid,
                launchd_state = ?probe.state,
                service_target = %manifest.service_target,
                manifest_path = %path.display(),
                "durable launchd restart request exceeded its reconciliation deadline without an exact replacement identity"
            );
        }
        Ok(Some(manifest))
    }

    fn read_manifest(path: &Path) -> Result<Option<RestartManifest>, ErrorData> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(launchd_error(
                    "SETUP_LAUNCHD_MANIFEST_READ_FAILED",
                    "status",
                    format!(
                        "failed to read restart manifest {}: {error}",
                        path.display()
                    ),
                    "repair the daemon lifecycle directory permissions and retry status",
                ));
            }
        };
        if bytes.len() as u64 > MANIFEST_MAX_BYTES {
            return Err(launchd_error(
                "SETUP_LAUNCHD_MANIFEST_TOO_LARGE",
                "status",
                format!(
                    "restart manifest {} is {} bytes, exceeding the {MANIFEST_MAX_BYTES}-byte bound",
                    path.display(),
                    bytes.len()
                ),
                "inspect and repair the corrupt restart manifest; do not delete it before preserving its bytes for diagnosis",
            ));
        }
        let manifest: RestartManifest = serde_json::from_slice(&bytes).map_err(|error| {
            launchd_error(
                "SETUP_LAUNCHD_MANIFEST_INVALID",
                "status",
                format!(
                    "restart manifest {} failed strict decode: {error}; bytes={} sha256={}",
                    path.display(),
                    bytes.len(),
                    prefixed_sha256(&bytes)
                ),
                "inspect and repair the corrupt restart manifest; the service status refuses to ignore durable ambiguity",
            )
        })?;
        let expected_target = format!("gui/{}/{LABEL}", effective_uid()?);
        if manifest.schema != RESTART_SCHEMA
            || manifest.service_target != expected_target
            || manifest.command_executable != LAUNCHCTL
            || manifest.command_args
                != ["kickstart", "-kp", expected_target.as_str()].map(str::to_owned)
        {
            return Err(launchd_error(
                "SETUP_LAUNCHD_MANIFEST_IDENTITY_MISMATCH",
                "status",
                format!(
                    "restart manifest {} does not bind the supported schema/target/command: schema={:?} target={:?} executable={:?} args={:?}",
                    path.display(),
                    manifest.schema,
                    manifest.service_target,
                    manifest.command_executable,
                    manifest.command_args
                ),
                "preserve the manifest for diagnosis and repair it only from the exact installed Synapse LaunchAgent identity",
            ));
        }
        Ok(Some(manifest))
    }

    fn write_manifest_atomic(path: &Path, manifest: &RestartManifest) -> Result<(), ErrorData> {
        let parent = path.parent().ok_or_else(|| {
            launchd_error(
                "SETUP_LAUNCHD_MANIFEST_PARENT_MISSING",
                "restart",
                format!("restart manifest has no parent: {}", path.display()),
                "repair the configured daemon lifecycle run-current path",
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            launchd_error(
                "SETUP_LAUNCHD_MANIFEST_DIRECTORY_FAILED",
                "restart",
                format!("failed to create {}: {error}", parent.display()),
                "repair the daemon lifecycle directory ownership and permissions",
            )
        })?;
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
            launchd_error(
                "SETUP_LAUNCHD_MANIFEST_SERIALIZE_FAILED",
                "restart",
                format!("failed to serialize restart manifest: {error}"),
                "fix the restart manifest schema implementation before retrying",
            )
        })?;
        if bytes.len() as u64 > MANIFEST_MAX_BYTES {
            return Err(launchd_error(
                "SETUP_LAUNCHD_MANIFEST_TOO_LARGE",
                "restart",
                format!(
                    "serialized restart manifest is {} bytes, exceeding the {MANIFEST_MAX_BYTES}-byte bound",
                    bytes.len()
                ),
                "reduce the fixed manifest schema before retrying",
            ));
        }
        let temp = parent.join(format!(
            ".{RESTART_MANIFEST}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let cleanup_error = fs::remove_file(&temp)
                .err()
                .filter(|cleanup| cleanup.kind() != std::io::ErrorKind::NotFound);
            return Err(launchd_error(
                "SETUP_LAUNCHD_MANIFEST_WRITE_FAILED",
                "restart",
                format!(
                    "failed atomic restart-manifest publication temp={} target={} error={} cleanup_error={:?}",
                    temp.display(),
                    path.display(),
                    error,
                    cleanup_error
                ),
                "repair the daemon lifecycle directory ownership/free space and retry after inspecting any named temporary file",
            ));
        }
        Ok(())
    }

    fn restart_readback(
        path: &Path,
        manifest: &RestartManifest,
    ) -> Result<SetupLaunchdRestartReadback, ErrorData> {
        let bytes = fs::read(path).map_err(|error| {
            launchd_error(
                "SETUP_LAUNCHD_MANIFEST_READBACK_FAILED",
                "status",
                format!(
                    "failed separate restart-manifest readback {}: {error}",
                    path.display()
                ),
                "repair the daemon lifecycle directory and retry status; the mutation is not accepted without physical readback",
            )
        })?;
        Ok(SetupLaunchdRestartReadback {
            schema: manifest.schema.clone(),
            request_id: manifest.request_id.clone(),
            state: manifest.state,
            service_target: manifest.service_target.clone(),
            requesting_pid: manifest.requesting_pid,
            completed_pid: manifest.completed_pid,
            requested_at_unix_ms: manifest.requested_at_unix_ms,
            completed_at_unix_ms: manifest.completed_at_unix_ms,
            reason_len_bytes: manifest.reason_len_bytes,
            reason_sha256: manifest.reason_sha256.clone(),
            command_executable: manifest.command_executable.clone(),
            command_args: manifest.command_args.clone(),
            command_exit_code: manifest.command_exit_code,
            before_query_sha256: manifest.before_query_sha256.clone(),
            after_query_sha256: manifest.after_query_sha256.clone(),
            failure_code: manifest.failure_code.clone(),
            manifest_path: path.display().to_string(),
            manifest_len_bytes: bytes.len() as u64,
            manifest_sha256: prefixed_sha256(&bytes),
        })
    }

    fn restart_manifest_path() -> Result<PathBuf, ErrorData> {
        let paths = crate::daemon_lifecycle::current_paths().ok_or_else(|| {
            launchd_error(
                "SETUP_LAUNCHD_LIFECYCLE_PATHS_UNCONFIGURED",
                "status",
                "daemon lifecycle paths are not configured".to_owned(),
                "start the installed launchd-owned Synapse daemon with its lifecycle ledger configured",
            )
        })?;
        let run_current = PathBuf::from(paths.run_current_path);
        let parent = run_current.parent().ok_or_else(|| {
            launchd_error(
                "SETUP_LAUNCHD_LIFECYCLE_PARENT_MISSING",
                "status",
                format!(
                    "daemon lifecycle run-current path has no parent: {}",
                    run_current.display()
                ),
                "repair the configured daemon lifecycle run-current path",
            )
        })?;
        Ok(parent.join(RESTART_MANIFEST))
    }

    #[cfg(target_os = "macos")]
    fn effective_uid() -> Result<u32, ErrorData> {
        // SAFETY: `geteuid` takes no arguments, cannot write through pointers,
        // and returns the process's effective user ID on macOS.
        Ok(unsafe { geteuid() })
    }

    #[cfg(not(target_os = "macos"))]
    fn effective_uid() -> Result<u32, ErrorData> {
        Err(launchd_error(
            "SETUP_LAUNCHD_UNSUPPORTED_PLATFORM",
            "status",
            format!(
                "launchd effective-user lookup is unavailable on {}",
                std::env::consts::OS
            ),
            "run this operation on the installed macOS Synapse LaunchAgent host",
        ))
    }

    fn now_unix_ms() -> Result<u128, ErrorData> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .map_err(|error| {
                launchd_error(
                    "SETUP_LAUNCHD_CLOCK_BEFORE_EPOCH",
                    "restart",
                    format!("macOS system clock is before Unix epoch: {error}"),
                    "repair the host system clock before issuing a restart",
                )
            })
    }

    fn prefixed_sha256(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut output = String::with_capacity(7 + digest.len() * 2);
        output.push_str("sha256:");
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
        }
        output
    }

    fn output_preview(output: &str) -> String {
        const MAX: usize = 2_048;
        let trimmed = output.trim();
        if trimmed.len() <= MAX {
            return trimmed.to_owned();
        }
        let mut boundary = MAX;
        while !trimmed.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}...<truncated>", &trimmed[..boundary])
    }

    fn invalid_params(message: impl Into<String>, source_id: &'static str) -> ErrorData {
        ErrorData::new(
            ErrorCode(-32602),
            message.into(),
            Some(json!({
                "code": error_codes::TOOL_PARAMS_INVALID,
                "tool": SETUP_TOOL,
                "operation": "launchd_service",
                "source_id": source_id,
                "source_of_truth": SETUP_SOT,
            })),
        )
    }

    fn launchd_error(
        detail_code: &'static str,
        action: &'static str,
        message: String,
        remediation: &'static str,
    ) -> ErrorData {
        tracing::error!(
            code = detail_code,
            tool = SETUP_TOOL,
            operation = "launchd_service",
            action,
            launchctl = LAUNCHCTL,
            label = LABEL,
            message = %message,
            remediation,
            "typed launchd service operation failed"
        );
        ErrorData::new(
            ErrorCode(-32099),
            message,
            Some(json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "detail_code": detail_code,
                "tool": SETUP_TOOL,
                "operation": "launchd_service",
                "action": action,
                "source_of_truth": SETUP_SOT,
                "launchctl": LAUNCHCTL,
                "label": LABEL,
                "remediation": remediation,
            })),
        )
    }
}
