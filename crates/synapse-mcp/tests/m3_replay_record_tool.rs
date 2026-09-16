use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::Observation;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn replay_record_schema_defaults_file_output_and_edges() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let local_app_data = TempDir::new()?;
    let replays = local_app_data.path().join("synapse").join("replays");
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
            (
                "LOCALAPPDATA",
                local_app_data
                    .path()
                    .to_str()
                    .context("LOCALAPPDATA path utf8")?,
            ),
        ],
    )
    .await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let replay_record_tool = tools
        .iter()
        .find(|tool| tool["name"] == "replay_record")
        .context("replay_record tool missing")?;
    assert_replay_record_schema(replay_record_tool);

    let observation_path = replays.join("observations.jsonl");
    let response = client
        .tools_call(
            "replay_record",
            json!({"duration_ms": 25, "path": observation_path}),
        )
        .await?;
    let payload = structured(&response)?;
    assert!(payload["records_written"].as_u64().unwrap_or_default() >= 1);
    assert_eq!(payload["observations_skipped"], 0);
    assert!(payload["bytes"].as_u64().unwrap_or_default() > 0);

    let replay_text = std::fs::read_to_string(&observation_path)?;
    let observations = replay_text
        .lines()
        .map(serde_json::from_str::<Observation>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(!observations.is_empty());
    assert!(
        observations
            .iter()
            .all(|observation| observation.foreground.process_name == "notepad.exe")
    );

    let empty_path = replays.join("empty.jsonl");
    let empty = client
        .tools_call(
            "replay_record",
            json!({"duration_ms": 0, "path": empty_path}),
        )
        .await?;
    let empty_payload = structured(&empty)?;
    assert_eq!(empty_payload["records_written"], 0);
    assert_eq!(empty_payload["observations_skipped"], 0);
    assert_eq!(empty_payload["bytes"], 0);
    assert_eq!(std::fs::read_to_string(&empty_path)?, "");

    let bad_target = client
        .tools_call_error(
            "replay_record",
            json!({"target": "bogus", "duration_ms": 1, "path": replays.join("bad-target.jsonl")}),
        )
        .await?;
    assert_eq!(bad_target["data"]["code"], "REPLAY_TARGET_INVALID");

    let bad_format = client
        .tools_call_error(
            "replay_record",
            json!({"format": "csv", "duration_ms": 1, "path": replays.join("bad-format.jsonl")}),
        )
        .await?;
    assert_eq!(bad_format["data"]["code"], "REPLAY_FORMAT_INVALID");

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn structured(response: &Value) -> anyhow::Result<Value> {
    if let Some(value) = response.get("structuredContent") {
        return Ok(value.clone());
    }

    let text = response
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .context("structured content missing")?;
    serde_json::from_str(text).context("parse text content")
}

fn assert_replay_record_schema(tool: &Value) {
    let shape = json!({
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "inputSchema": tool.get("inputSchema").cloned().unwrap_or(Value::Null),
        "outputSchema": tool.get("outputSchema").cloned().unwrap_or(Value::Null),
    });
    assert_eq!(shape["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        shape["inputSchema"]["properties"]["target"]["default"],
        "observations"
    );
    assert_eq!(
        shape["inputSchema"]["properties"]["format"]["default"],
        "jsonl"
    );
    assert_eq!(
        shape["inputSchema"]["properties"]["target"]["enum"],
        json!(["observations", "events", "both"])
    );
    assert_eq!(
        shape["inputSchema"]["properties"]["format"]["enum"],
        json!(["jsonl"])
    );
    assert_eq!(shape["inputSchema"]["required"], json!(["duration_ms"]));
    assert_eq!(
        shape["outputSchema"]["required"],
        json!(["path", "records_written", "observations_skipped", "bytes"])
    );
    insta::assert_json_snapshot!("m3_replay_record_tool", shape);
}
