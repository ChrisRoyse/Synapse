use std::{fs, path::PathBuf};

use rmcp::{RoleServer, service::RequestContext};
use sha2::{Digest as _, Sha256};

use crate::server::{
    ErrorData, Json, Parameters, SynapseService, mcp_error, tool_profiles::ToolProfileKind,
};

use super::{
    SETUP_SOT, SETUP_TOOL,
    errors::{facade_delegate_error, facade_policy_error, missing_spec},
    policy::require_maintenance_profile,
    response::setup_response,
    types::{FileReadback, SetupOperation, SetupParams, SetupResponse, SetupStatusResponse},
    validation::validate_setup_params,
};
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
            Err(facade_policy_error(
                SETUP_TOOL,
                operation.as_str(),
                "synapse_setup_repair",
                ToolProfileKind::BreakGlass,
                SETUP_SOT,
                "run scripts\\synapse-setup.ps1 from an external maintenance process so daemon replacement has a separate process/socket SoT; in-process self-repair is refused",
            ))
        }
    }
}

pub(super) fn setup_status(service: &SynapseService) -> Result<SetupStatusResponse, ErrorData> {
    let bind = service.m3_bind_addr()?;
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
}
