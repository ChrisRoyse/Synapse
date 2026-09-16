use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::model::ErrorCode;
use rmcp::{RoleServer, service::RequestContext};
use serde::Serialize;
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
            let chrome_bridge_preflight = preflight_setup_repair_chrome_bridge().await?;
            let launched = launch_setup_repair(service, &spec.reason, &chrome_bridge_preflight)?;
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
    chrome_bridge_preflight: String,
    child_pid: u32,
}

impl SetupRepairLaunchReadback {
    fn readback_source_of_truth(&self) -> String {
        format!(
            "external setup repair launched; run_id={} child_pid={} launcher={} source_dir={} setup_script={} run_dir={} manifest={} stdout_log={} stderr_log={} {}",
            self.run_id,
            self.child_pid,
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
    chrome_bridge_preflight: String,
    remediation: &'static str,
}

fn launch_setup_repair(
    service: &SynapseService,
    reason: &str,
    chrome_bridge_preflight: &str,
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
    let args = setup_repair_command_args(&setup_script_path, &source_dir, &bind);
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
        .env("SYNAPSE_SETUP_REPAIR_MANIFEST", &manifest_path);
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
        chrome_bridge_preflight: chrome_bridge_preflight.to_owned(),
        child_pid,
    })
}

async fn preflight_setup_repair_chrome_bridge() -> Result<String, ErrorData> {
    match chrome_debugger_bridge::reload_bridge(30_000).await {
        Ok(result) => Ok(format!(
            "chrome_bridge_preflight=reload_bridge_ok before_host={} after_host={} reconnected={} waited_ms={}",
            result.before.host_id, result.after.host_id, result.reconnected, result.waited_ms
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
            "SYNAPSE_SETUP_REPAIR_CHROME_BRIDGE_RELOAD_FAILED",
            "chrome_bridge_reload",
            format!(
                "setup repair could not reload the active Chrome bridge before external maintenance handoff; code={} detail={}",
                error.code(),
                error.detail()
            ),
            "reload the already-open Synapse Chrome Bridge through browser_debugger.reload_bridge or repair the bridge host before retrying setup repair",
        )),
    }
}

fn setup_repair_command_args(
    setup_script_path: &Path,
    source_dir: &Path,
    bind: &str,
) -> Vec<String> {
    vec![
        "-NoProfile".to_owned(),
        "-ExecutionPolicy".to_owned(),
        "Bypass".to_owned(),
        "-File".to_owned(),
        setup_script_path.display().to_string(),
        "-SourceDir".to_owned(),
        source_dir.display().to_string(),
        "-Bind".to_owned(),
        bind.to_owned(),
        "-ForceRestart".to_owned(),
    ]
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
    let setup_repair_command_args = setup_repair_command_args(&setup_script, &source_dir, &bind);
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
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_daemon_run_file_reads_configured_lifecycle_path() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        crate::daemon_lifecycle::reset_for_test();
        let temp = tempfile::tempdir().unwrap();
        let paths =
            crate::daemon_lifecycle::configure(crate::daemon_lifecycle::DaemonLifecycleConfig {
                mode: "http",
                bind_addr: Some("127.0.0.1:7813".to_owned()),
                db_path: temp.path().to_path_buf(),
            })
            .unwrap();

        let active = active_daemon_run_file().unwrap();
        let shared = file_readback(shared_daemon_run_file_path());

        assert_eq!(active.path, paths.run_current_path);
        assert_ne!(active.path, shared.path);
        assert!(active.exists);
        assert_eq!(
            active.len_bytes,
            Some(fs::metadata(&paths.run_current_path).unwrap().len())
        );

        crate::daemon_lifecycle::reset_for_test();
    }

    #[test]
    fn active_daemon_run_file_fails_closed_without_lifecycle_state() {
        let _serial = crate::test_support::daemon_lifecycle_serial();
        crate::daemon_lifecycle::reset_for_test();

        let error = match active_daemon_run_file() {
            Ok(readback) => panic!(
                "expected missing lifecycle state to fail closed, got {}",
                readback.path
            ),
            Err(error) => error,
        };

        assert!(
            error
                .message
                .contains("daemon lifecycle ledger is not configured")
        );
    }

    #[test]
    fn setup_repair_source_dir_points_at_real_repo_script() {
        let source_dir = setup_source_dir().expect("repo source dir");
        let script = setup_script_path(&source_dir).expect("setup script");

        assert!(source_dir.join("Cargo.toml").is_file());
        assert_eq!(
            script.file_name().and_then(|name| name.to_str()),
            Some("synapse-setup.ps1")
        );
        assert!(script.is_file());
    }

    #[test]
    fn setup_repair_command_args_use_external_force_restart_runner() {
        let source_dir = setup_source_dir().expect("repo source dir");
        let setup_script = source_dir.join("scripts").join("synapse-setup.ps1");
        let args = setup_repair_command_args(&setup_script, &source_dir, "127.0.0.1:7700");

        assert!(has_arg_pair(
            &args,
            "-File",
            &setup_script.display().to_string()
        ));
        assert!(has_arg_pair(
            &args,
            "-SourceDir",
            &source_dir.display().to_string()
        ));
        assert!(has_arg_pair(&args, "-Bind", "127.0.0.1:7700"));
        assert!(args.iter().any(|arg| arg == "-ForceRestart"));
    }

    #[test]
    fn setup_repair_active_issue_is_extracted_from_reason() {
        assert_eq!(
            setup_repair_active_issue_from_reason("issue1410 happy path"),
            Some("#1410".to_owned())
        );
        assert_eq!(
            setup_repair_active_issue_from_reason("#1405 bind-drain retry"),
            Some("#1405".to_owned())
        );
        assert_eq!(
            setup_repair_active_issue_from_reason("no issue marker"),
            None
        );
    }

    fn has_arg_pair(args: &[String], name: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == name && pair[1] == value)
    }
}
