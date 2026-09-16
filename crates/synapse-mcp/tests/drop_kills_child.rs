#![cfg(unix)]

use std::{process::Stdio, time::Duration};

use anyhow::{Context, bail};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tokio::process::Command;

#[tokio::test]
async fn drop_kills_uninitialized_child() -> anyhow::Result<()> {
    let client = StdioMcpClient::launch(None)?;
    let pid = client.child_id().context("child pid missing")?;

    drop(client);

    for _ in 0..40 {
        if !process_exists(pid).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    bail!("child process {pid} still existed after StdioMcpClient drop");
}

async fn process_exists(pid: u32) -> anyhow::Result<bool> {
    let status = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .await
        .context("run kill -0")?;
    Ok(status.success())
}
