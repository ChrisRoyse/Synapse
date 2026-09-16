use anyhow::Context;
use serde_json::Value;
use synapse_core::Health;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn m0_demo_gate_health_full_flow() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(dir.path())).await?;

    let list = client.tools_list().await?;
    let tools = list
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name") == Some(&Value::String("health".to_owned())))
    );

    let resp = client.tools_call("health", serde_json::json!({})).await?;
    let payload: Health = if let Some(value) = resp.get("structuredContent") {
        serde_json::from_value(value.clone())?
    } else {
        let text = resp
            .get("content")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(|content| content.get("text"))
            .and_then(Value::as_str)
            .context("health text content missing")?;
        serde_json::from_str(text)?
    };
    assert!(payload.ok);
    assert_eq!(payload.version, env!("CARGO_PKG_VERSION"));

    let status = client.shutdown().await?;
    assert!(status.success());
    let logs = read_logs(dir.path())?;
    assert!(logs.contains("tool.invocation kind=health"));
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
