use anyhow::Context;
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn subscribe_schema_defaults_and_edges() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(logs.path())).await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let subscribe_tool = tools
        .iter()
        .find(|tool| tool["name"] == "subscribe")
        .context("subscribe tool missing")?;
    let subscribe_cancel_tool = tools
        .iter()
        .find(|tool| tool["name"] == "subscribe_cancel")
        .context("subscribe_cancel tool missing")?;
    assert_subscribe_schema(subscribe_tool, subscribe_cancel_tool);

    let response = client.tools_call("subscribe", json!({})).await?;
    let first = structured(&response)?;
    let first_subscription_id = first["subscription_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("subscription_id missing")?
        .to_owned();
    assert!(first["started_at"].as_str().is_some());

    let cancel = client
        .tools_call(
            "subscribe_cancel",
            json!({"subscription_id": first_subscription_id}),
        )
        .await?;
    let cancel_payload = structured(&cancel)?;
    assert_eq!(cancel_payload["cancelled"], true);
    assert_eq!(cancel_payload["reason"], "ok");

    let second_cancel = client
        .tools_call_error(
            "subscribe_cancel",
            json!({"subscription_id": first_subscription_id}),
        )
        .await?;
    assert_eq!(second_cancel["data"]["code"], "SUBSCRIPTION_NOT_FOUND");

    let unknown_cancel = client
        .tools_call_error(
            "subscribe_cancel",
            json!({"subscription_id": "missing-sub"}),
        )
        .await?;
    assert_eq!(unknown_cancel["data"]["code"], "SUBSCRIPTION_NOT_FOUND");

    let empty_cancel = client
        .tools_call_error("subscribe_cancel", json!({"subscription_id": ""}))
        .await?;
    assert_eq!(empty_cancel["data"]["code"], "TOOL_PARAMS_INVALID");

    let bad_buffer = client
        .tools_call_error("subscribe", json!({"buffer_size": 4097}))
        .await?;
    assert_eq!(bad_buffer["data"]["code"], "TOOL_PARAMS_INVALID");

    let bad_filter = client
        .tools_call_error("subscribe", json!({"filter": {"op": "and", "args": []}}))
        .await?;
    assert_eq!(bad_filter["data"]["code"], "TOOL_PARAMS_INVALID");

    let bad_regex_filter = client
        .tools_call_error(
            "subscribe",
            json!({"filter": {"op": "data", "path": "/field", "predicate": {"op": "regex", "pattern": "["}}}),
        )
        .await?;
    assert_eq!(bad_regex_filter["data"]["code"], "TOOL_PARAMS_INVALID");

    let bad_path_filter = client
        .tools_call_error(
            "subscribe",
            json!({"filter": {"op": "data", "path": "field", "predicate": {"op": "exists"}}}),
        )
        .await?;
    assert_eq!(bad_path_filter["data"]["code"], "TOOL_PARAMS_INVALID");

    for _ in 0..64 {
        let response = client.tools_call("subscribe", json!({})).await?;
        let payload = structured(&response)?;
        assert!(
            payload["subscription_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
    }
    let capped = client.tools_call_error("subscribe", json!({})).await?;
    assert_eq!(capped["data"]["code"], "SUBSCRIPTION_CAP_REACHED");

    let status = client.shutdown().await?;
    assert!(status.success());

    subscribe_honors_configured_subscription_cap_and_cancel_retry().await?;
    Ok(())
}

async fn subscribe_honors_configured_subscription_cap_and_cancel_retry() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_MAX_SUBSCRIPTIONS", "2")],
    )
    .await?;

    let first = structured(&client.tools_call("subscribe", json!({})).await?)?;
    let first_subscription_id = first["subscription_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("first subscription_id missing")?
        .to_owned();
    let second = structured(&client.tools_call("subscribe", json!({})).await?)?;
    let second_subscription_id = second["subscription_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("second subscription_id missing")?
        .to_owned();
    assert_ne!(first_subscription_id, second_subscription_id);

    let capped = client.tools_call_error("subscribe", json!({})).await?;
    assert_eq!(capped["data"]["code"], "SUBSCRIPTION_CAP_REACHED");

    let cancel = structured(
        &client
            .tools_call(
                "subscribe_cancel",
                json!({"subscription_id": first_subscription_id}),
            )
            .await?,
    )?;
    assert_eq!(cancel["cancelled"], true);
    assert_eq!(cancel["reason"], "ok");

    let retry = structured(&client.tools_call("subscribe", json!({})).await?)?;
    let retry_subscription_id = retry["subscription_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("retry subscription_id missing")?;
    assert_ne!(retry_subscription_id, second_subscription_id);

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

fn assert_subscribe_schema(subscribe_tool: &Value, subscribe_cancel_tool: &Value) {
    let shape = json!({
        "subscribe": {
            "name": subscribe_tool.get("name").cloned().unwrap_or(Value::Null),
            "inputSchema": subscribe_tool.get("inputSchema").cloned().unwrap_or(Value::Null),
            "outputSchema": subscribe_tool.get("outputSchema").cloned().unwrap_or(Value::Null),
        },
        "subscribe_cancel": {
            "name": subscribe_cancel_tool.get("name").cloned().unwrap_or(Value::Null),
            "inputSchema": subscribe_cancel_tool.get("inputSchema").cloned().unwrap_or(Value::Null),
            "outputSchema": subscribe_cancel_tool.get("outputSchema").cloned().unwrap_or(Value::Null),
        },
    });
    assert_eq!(
        shape["subscribe"]["inputSchema"]["additionalProperties"],
        false
    );
    assert_eq!(
        shape["subscribe"]["inputSchema"]["properties"]["kinds"]["default"],
        json!([])
    );
    assert_eq!(
        shape["subscribe"]["inputSchema"]["properties"]["snapshot_first"]["default"],
        false
    );
    assert_eq!(
        shape["subscribe"]["inputSchema"]["properties"]["buffer_size"]["default"],
        4096
    );
    assert_eq!(
        shape["subscribe_cancel"]["inputSchema"]["additionalProperties"],
        false
    );
    assert_eq!(
        shape["subscribe_cancel"]["inputSchema"]["required"],
        json!(["subscription_id"])
    );
    insta::assert_json_snapshot!("m3_subscribe_tool", shape);
}
