use anyhow::Context;
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn reflex_cancel_schema_and_idempotent_edges() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        // Synthetic notepad foreground so the reflex_register supported-use gate
        // does not depend on a real focused window (A11Y_NO_FOREGROUND otherwise).
        &[
            ("SYNAPSE_DB", db_path.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
        ],
    )
    .await?;
    activate_notepad_profile(&mut client).await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let reflex_cancel_tool = tools
        .iter()
        .find(|tool| tool["name"] == "reflex_cancel")
        .context("reflex_cancel tool missing")?;
    assert_reflex_cancel_schema(reflex_cancel_tool);

    let missing = client
        .tools_call("reflex_cancel", json!({"reflex_id": "support-missing"}))
        .await?;
    let missing = structured(&missing)?;
    assert_eq!(missing["cancelled"], false);
    assert_eq!(missing["reason"], "not_found");

    let empty = client
        .tools_call_error("reflex_cancel", json!({"reflex_id": "   "}))
        .await?;
    assert_eq!(empty["data"]["code"], "TOOL_PARAMS_INVALID");

    let response = client
        .tools_call("reflex_register", valid_register_args("support-cancel"))
        .await?;
    let registered = structured(&response)?;
    let reflex_id = registered["reflex_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("reflex_id missing")?;

    let cancelled = client
        .tools_call("reflex_cancel", json!({"reflex_id": reflex_id}))
        .await?;
    let cancelled = structured(&cancelled)?;
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["reason"], "ok");

    let cancelled_again = client
        .tools_call("reflex_cancel", json!({"reflex_id": reflex_id}))
        .await?;
    let cancelled_again = structured(&cancelled_again)?;
    assert_eq!(cancelled_again["cancelled"], true);
    assert_eq!(cancelled_again["reason"], "ok");

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn valid_register_args(kind: &str) -> Value {
    json!({
        "kind": "on_event",
        "when": {"op": "kind", "kind": kind},
        "then": {"kind": "action", "action": {"kind": "release_all"}}
    })
}

async fn activate_notepad_profile(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let response = client
        .tools_call("profile_activate", json!({"profile_id": "notepad"}))
        .await?;
    assert_eq!(structured(&response)?["active_profile_id"], "notepad");
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

fn assert_reflex_cancel_schema(tool: &Value) {
    let shape = json!({
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "inputSchema": tool.get("inputSchema").cloned().unwrap_or(Value::Null),
        "outputSchema": tool.get("outputSchema").cloned().unwrap_or(Value::Null),
    });
    assert_eq!(shape["inputSchema"]["additionalProperties"], false);
    assert_eq!(shape["inputSchema"]["required"][0], "reflex_id");
    assert_eq!(
        shape["outputSchema"]["$defs"]["ReflexCancelReason"]["enum"][2],
        "already_expired"
    );
    insta::assert_json_snapshot!("m3_reflex_cancel_tool", shape);
}
