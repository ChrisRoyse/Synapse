use super::*;

pub(super) fn validate_spawn_local_model_static_requirements(
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

pub(super) fn local_model_unhealthy_refusal(model_ref: &str, row: &LocalModelRegistryRow) -> ErrorData {
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

pub(super) fn agent_spawn_terminal_capture_artifacts(log_dir: &Path) -> CaptureArtifacts {
    CaptureArtifacts {
        asciicast_path: log_dir.join("terminal.cast"),
        status_path: log_dir.join("terminal-capture-status.json"),
        final_screen_path: log_dir.join("terminal-final-screen.txt"),
        input_audit_path: log_dir.join("terminal-input-audit.ndjson"),
    }
}

pub(super) fn agent_spawn_request_details(
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

pub(super) fn resolve_agent_spawn_powershell_host() -> Result<AgentSpawnLaunchHost, ErrorData> {
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

pub(super) fn trim_configured_agent_spawn_shell(value: &str) -> &str {
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
pub(super) fn resolve_default_agent_spawn_powershell_host() -> Result<AgentSpawnLaunchHost, ErrorData> {
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
pub(super) fn resolve_default_agent_spawn_powershell_host() -> Result<AgentSpawnLaunchHost, ErrorData> {
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

pub(super) fn ensure_supported_agent_spawn_shell(candidate: &str) -> Result<(), ErrorData> {
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

pub(super) fn resolve_agent_spawn_shell_candidate(
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

pub(super) fn is_path_like_agent_spawn_shell(candidate: &str) -> bool {
    let path = Path::new(candidate);
    path.is_absolute()
        || candidate.contains('\\')
        || candidate.contains('/')
        || candidate
            .as_bytes()
            .get(1)
            .is_some_and(|second| *second == b':')
}

pub(super) fn agent_spawn_executable_names(candidate: &str) -> Vec<String> {
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
pub(super) fn agent_spawn_path_extensions() -> Vec<String> {
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
pub(super) fn agent_spawn_path_extensions() -> Vec<String> {
    Vec::new()
}

pub(super) fn record_agent_spawn_shell_attempt(attempted: &mut Vec<String>, path: &Path) {
    if attempted.len() < AGENT_SPAWN_RECORDED_ATTEMPT_LIMIT {
        attempted.push(path.display().to_string());
    }
}

pub(super) fn display_agent_spawn_shell_path(path: &Path) -> String {
    if path.is_absolute() {
        return path.display().to_string();
    }
    std::env::current_dir()
        .map(|current_dir| current_dir.join(path).display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub(super) fn agent_spawn_shell_error(
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

pub(super) fn augment_agent_spawn_error_with_artifacts(
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

pub(super) fn write_agent_spawn_daemon_terminal_artifacts(
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
pub(super) fn resolve_spawn_local_model_api_key(
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

pub(super) fn local_model_spawn_refusal(
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

pub(super) fn recover_orphaned_agent_spawn_terminal_artifacts()
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

pub(super) fn agent_spawn_orphan_recovery_decision(
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

pub(super) fn stale_enough_for_orphan_recovery(age_ms: Option<u64>) -> bool {
    age_ms.is_some_and(|age_ms| age_ms >= AGENT_SPAWN_ORPHAN_RECOVERY_STALE_MS)
}

pub(super) fn wrapper_process_id_from_status(status: &Value) -> Option<u32> {
    status
        .get("wrapper_process_id")
        .or_else(|| status.get("powershell_process_id"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

pub(super) fn wrapper_process_is_live_for_status(pid: u32, status: &Value) -> bool {
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

pub(super) fn wrapper_process_is_live_for_recovery(recovery: &AgentSpawnOrphanRecovery) -> bool {
    let Some(pid) = recovery.wrapper_process_id else {
        return false;
    };
    if let Some(status) = &recovery.source_completion_status {
        wrapper_process_is_live_for_status(pid, status)
    } else {
        process_exists(pid)
    }
}

pub(super) fn write_agent_spawn_orphan_terminal_artifacts(
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
            "details": details,
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

pub(super) fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

pub(super) fn file_age_ms(path: &Path, now: u64) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let modified = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    let modified = u64::try_from(modified).ok()?;
    Some(now.saturating_sub(modified))
}

pub(super) fn stdout_summary_lossy(path: &Path) -> (u64, Option<String>) {
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

pub(super) fn validate_spawn_target(target: &Option<ActSpawnAgentTarget>) -> Result<(), ErrorData> {
    match target {
        Some(
            ActSpawnAgentTarget::Window { window_hwnd }
            | ActSpawnAgentTarget::Cdp { window_hwnd, .. },
        ) => {
            validate_target_window(*window_hwnd)?;
        }
        None => {}
    }
    Ok(())
}

pub(super) fn resolve_agent_working_dir(working_dir: Option<&str>) -> Result<PathBuf, ErrorData> {
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
            format!("act_spawn_agent working_dir {path:?} could not be resolved: {error}"),
        )
    })?;
    if !canonical.is_dir() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_spawn_agent working_dir {canonical:?} is not a directory"),
        ));
    }
    Ok(canonical)
}

pub(super) fn read_synapse_bearer_token() -> Result<String, ErrorData> {
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

pub(super) fn prepare_agent_spawn_files(
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
    let manifest = build_spawn_manifest(spawn_id, params, working_dir)?;
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
pub(super) fn agent_event_ingress_url(
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
/// [`crate::server::agent_event_ingress::CLAUDE_HOOK_SUBSCRIBED_EVENTS`]. Uses the
/// CLI's native HTTP hooks (verified on Claude Code 2.1.176): no per-event
/// child process, bearer injected via `allowedEnvVars` interpolation, and
/// delivery failures are non-blocking for the agent.
pub(super) fn build_claude_hook_settings(
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
    for event in crate::server::agent_event_ingress::CLAUDE_HOOK_SUBSCRIBED_EVENTS {
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
/// [`crate::server::permission_policy`]; the delegated gate still backstops anything
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

pub(super) const CLAUDE_COORDINATION_FACADE_MCP_TOOLS: &[&str] = &["workspace"];

/// Codex `notify` program: receives the notification JSON as its final argv
/// argument and POSTs it verbatim to the ingress. Codex spawns it
/// fire-and-forget, so delivery failures are persisted to
/// `notify-errors.log` in the spawn directory — local evidence instead of a
/// silent drop.
pub(super) fn build_codex_notify_script(
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

pub(super) fn build_agent_spawn_prompt(
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

pub(super) fn write_agent_spawn_task_started_from_session(
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

pub(super) fn validate_agent_spawn_id_path_segment(spawn_id: &str) -> Result<(), ErrorData> {
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

pub(super) fn read_agent_spawn_manifest(log_dir: &Path, spawn_id: &str) -> Result<Value, ErrorData> {
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

pub(super) fn agent_kind_from_spawn_manifest(manifest: &Value) -> Result<ActSpawnAgentCli, ErrorData> {
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

pub(super) fn build_agent_spawn_task_started_artifact(
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

pub(super) fn read_task_started_value_strict(path: &Path) -> Result<Value, ErrorData> {
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

pub(super) fn ensure_existing_task_started_claim(
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

pub(super) fn write_task_started_artifact_atomically(path: &Path, artifact: &Value) -> Result<(), ErrorData> {
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

pub(super) fn response_from_task_started_artifact(
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

pub(super) fn agent_spawn_powershell_script(
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
                local_args = format!("$localArgs = {local_args}");
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

pub(super) fn agent_spawn_wrapper_powershell(
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

pub(super) fn ps_single_quoted_path(path: &Path) -> String {
    ps_single_quote(&path.display().to_string())
}

pub(super) fn ps_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
pub(super) fn agent_spawn_wait_deadline(wait_timeout_ms: u64) -> Result<Instant, ErrorData> {
    agent_spawn_wait_deadline_from(Instant::now(), wait_timeout_ms)
}

pub(super) fn agent_spawn_wait_deadline_from(
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

pub(super) fn agent_spawn_deadline_remaining(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
}

pub(super) async fn sleep_agent_spawn_poll(deadline: Instant) {
    let remaining = agent_spawn_deadline_remaining(deadline);
    if remaining.is_zero() {
        return;
    }
    let poll = Duration::from_millis(AGENT_SPAWN_POLL_INTERVAL_MS);
    tokio::time::sleep(if remaining < poll { remaining } else { poll }).await;
}

pub(super) fn read_agent_spawn_task_start_artifact(
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
pub(super) fn agent_spawn_observed_task_progress(
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

pub(super) fn read_spawned_agent_control_artifact(
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

pub(super) fn read_json_file_lossy(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn agent_spawn_readiness_file_readback(files: &AgentSpawnFiles) -> Value {
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

pub(super) fn spawn_target_wire_from_session_target(target: &SessionTarget) -> TargetWire {
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

pub(super) fn spawn_session_observation_from_read(
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
pub(super) fn spawn_session_candidate_readiness(
    summary: &crate::server::session_tools::SessionSummary,
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

pub(super) fn spawn_session_candidate_readiness_from_read(
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

pub(super) fn task_start_session_id_for_spawn(files: &AgentSpawnFiles, spawn_id: &str) -> Option<String> {
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
pub(super) fn spawn_session_identity_matches(
    summary: &crate::server::session_tools::SessionSummary,
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

pub(super) fn spawn_session_identity_matches_from_read(
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

pub(super) fn registry_matches_cli(registry: &SessionRegistryRead, cli: ActSpawnAgentCli) -> bool {
    let cli = cli.as_str();
    if registry.agent_kind == cli {
        return true;
    }
    registry
        .client_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains(cli))
}

pub(super) fn matches_target_wire(wire: Option<&crate::server::TargetWire>, expected: &ActSpawnAgentTarget) -> bool {
    match (wire, expected) {
        (
            Some(crate::server::TargetWire::Window {
                window_hwnd: actual,
            }),
            ActSpawnAgentTarget::Window {
                window_hwnd: expected,
            },
        ) => actual == expected,
        (
            Some(crate::server::TargetWire::Cdp {
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

pub(super) fn tail_file_lossy(path: &Path, limit_bytes: usize) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(limit_bytes);
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

pub(super) fn process_has_exited(pid: u32) -> bool {
    !process_exists(pid)
}

pub(super) fn process_exists(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
    system.process(pid).is_some()
}

pub(super) fn discover_agent_process_id(launcher_pid: u32, cli: ActSpawnAgentCli) -> Option<u32> {
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

pub(super) fn descendant_process_ids(system: &sysinfo::System, root_pid: u32) -> Vec<u32> {
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

pub(super) fn agent_spawn_tool_error(
    code: &'static str,
    message: &'static str,
    data: serde_json::Value,
) -> ErrorData {
    tracing::warn!(code, "M4 agent spawn tool error: {message}");
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

pub(super) fn launch_lifecycle_tool_error(message: &'static str, data: serde_json::Value) -> ErrorData {
    tracing::warn!(
        code = error_codes::TOOL_INTERNAL_ERROR,
        "M4 launch lifecycle tool error: {message}"
    );
    ErrorData::new(ErrorCode(-32099), message, Some(data))
}

