#![cfg(unix)]

use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::error_codes;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn synthetic_sigint_results_in_exit_0_and_flushed_log() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let client = StdioMcpClient::launch_and_init_with_log_dir(Some(dir.path())).await?;
    let before_logs = read_logs(dir.path())?;
    println!(
        "readback=daemon_log edge=sigint before_shutdown_count={} before_safety_count={}",
        event_code_count(&before_logs, "MCP_SHUTDOWN_GRACEFUL"),
        safety_reason_count(&before_logs, "sigint")
    );

    let status = client.send_sigint_and_wait().await?;

    let logs = read_logs(dir.path())?;
    let shutdown_count = event_code_count(&logs, "MCP_SHUTDOWN_GRACEFUL");
    let safety_lines = safety_reason_lines(&logs, "sigint");
    let safety_count = safety_lines.len();
    println!(
        "readback=daemon_log edge=sigint after_shutdown_count={shutdown_count} after_safety_count={safety_count} after_line={:?} exit_code={:?}",
        safety_lines.first(),
        status.code()
    );
    assert_eq!(status.code(), Some(0));
    assert!(
        shutdown_count >= 1,
        "expected shutdown log, got logs: {logs}"
    );
    assert!(
        safety_count >= 1,
        "expected shutdown release_all safety log, got logs: {logs}"
    );
    Ok(())
}

#[tokio::test]
async fn stdio_connection_closed_emits_release_all_log() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(dir.path())).await?;
    let pad_error = client
        .tools_call_error(
            "act_pad",
            json!({
                "pad_id": 4,
                "report": {
                    "buttons": ["a"],
                    "rt": 0.5
                }
            }),
        )
        .await?;
    assert_eq!(
        pad_error["data"]["code"],
        error_codes::SAFETY_PROFILE_ACTION_DENIED
    );
    println!(
        "readback=daemon_log edge=connection_closed before=safety_count:0 expected_held_pad_ids:[] pad_error={pad_error}"
    );

    let status = client.shutdown().await?;
    assert!(status.success());

    let logs = read_logs(dir.path())?;
    let safety_lines = safety_reason_lines(&logs, "connection_closed");
    let safety_count = safety_lines.len();
    let safety_line = safety_lines
        .first()
        .context("connection_closed safety line missing")?;
    println!(
        "readback=daemon_log edge=connection_closed after_safety_count={safety_count} after_line={safety_line:?}"
    );
    assert!(
        safety_count >= 1,
        "expected connection_closed release_all safety log, got logs: {logs}"
    );
    assert!(safety_line.contains("\"held_pad_ids\":\"[]\""));
    assert!(safety_line.contains("\"released_pads\":0"));
    Ok(())
}

fn read_logs(path: &std::path::Path) -> anyhow::Result<String> {
    let mut logs = String::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            logs.push_str(&std::fs::read_to_string(entry.path())?);
        }
    }
    Ok(logs)
}

fn safety_reason_count(logs: &str, reason: &str) -> usize {
    safety_reason_lines(logs, reason).len()
}

fn safety_reason_lines(logs: &str, reason: &str) -> Vec<String> {
    logs.lines()
        .filter(|line| {
            parse_log_fields(line).is_some_and(|fields| {
                fields.get("code").and_then(Value::as_str)
                    == Some(error_codes::SAFETY_RELEASE_ALL_FIRED)
                    && fields.get("reason").and_then(Value::as_str) == Some(reason)
            })
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn event_code_count(logs: &str, code: &str) -> usize {
    logs.lines()
        .filter_map(parse_log_fields)
        .filter(|fields| fields.get("code").and_then(Value::as_str) == Some(code))
        .count()
}

fn parse_log_fields(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    Some(value.get("fields")?.clone())
}
