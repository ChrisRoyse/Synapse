use std::{fs, path::Path};

use anyhow::{Context, bail};
use serde_json::Value;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SHELL_JOB_STORE_LOCK_FILE: &str = "shell-job-store.lock";
const SHELL_JOB_STORE_PID_FILE: &str = "shell-job-store.pid";
const SYNAPSE_SHELL_JOB_ROOT: &str = "SYNAPSE_SHELL_JOB_ROOT";

#[tokio::test]
async fn parallel_stdio_clients_own_distinct_shell_job_stores() -> anyhow::Result<()> {
    let (mut first, mut second) = tokio::try_join!(
        StdioMcpClient::launch_and_init(),
        StdioMcpClient::launch_and_init()
    )?;

    let first_root = first.shell_job_root().to_path_buf();
    let second_root = second.shell_job_root().to_path_buf();
    assert_ne!(
        first_root, second_root,
        "parallel stdio clients must never share a shell-job root"
    );

    assert_owned_store(
        &first_root,
        first.child_id().context("first child pid missing")?,
    )?;
    assert_owned_store(
        &second_root,
        second.child_id().context("second child pid missing")?,
    )?;

    assert_tools_list_has_tools(&mut first, "first").await?;
    assert_tools_list_has_tools(&mut second, "second").await?;

    let first_status = first.shutdown().await?;
    let second_status = second.shutdown().await?;
    assert!(first_status.success());
    assert!(second_status.success());
    assert!(
        !first_root.exists(),
        "first owned temp shell-job root should be removed after shutdown: {}",
        first_root.display()
    );
    assert!(
        !second_root.exists(),
        "second owned temp shell-job root should be removed after shutdown: {}",
        second_root.display()
    );
    Ok(())
}

#[test]
fn caller_supplied_shell_job_root_env_is_rejected() -> anyhow::Result<()> {
    let root = TempDir::new()?;
    let root_text = root.path().to_string_lossy().into_owned();
    let Err(error) =
        StdioMcpClient::launch_with_env(None, &[(SYNAPSE_SHELL_JOB_ROOT, root_text.as_str())])
    else {
        bail!("caller-supplied {SYNAPSE_SHELL_JOB_ROOT} unexpectedly launched a stdio daemon");
    };
    assert!(
        error
            .to_string()
            .contains("StdioMcpClient owns SYNAPSE_SHELL_JOB_ROOT isolation"),
        "unexpected error: {error}"
    );
    Ok(())
}

async fn assert_tools_list_has_tools(
    client: &mut StdioMcpClient,
    client_label: &str,
) -> anyhow::Result<()> {
    let response = client.tools_list().await?;
    let tools = response
        .get("tools")
        .and_then(Value::as_array)
        .with_context(|| format!("{client_label} tools/list response missing tools array"))?;
    if tools.is_empty() {
        bail!("{client_label} tools/list returned an empty tools array");
    }
    Ok(())
}

fn assert_owned_store(root: &Path, expected_pid: u32) -> anyhow::Result<()> {
    if !root.is_dir() {
        bail!("shell-job root is not a directory: {}", root.display());
    }
    let lock_path = root.join(SHELL_JOB_STORE_LOCK_FILE);
    if !lock_path.exists() {
        bail!("shell-job lock file missing: {}", lock_path.display());
    }
    let pid_path = root.join(SHELL_JOB_STORE_PID_FILE);
    let actual_pid = read_pid_sidecar(&pid_path)?;
    assert_eq!(
        actual_pid, expected_pid,
        "shell-job PID sidecar must identify the stdio daemon that owns this root"
    );
    Ok(())
}

fn read_pid_sidecar(path: &Path) -> anyhow::Result<u32> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read shell-job PID sidecar {}", path.display()))?;
    text.trim()
        .parse()
        .with_context(|| format!("parse shell-job PID sidecar {}", path.display()))
}
