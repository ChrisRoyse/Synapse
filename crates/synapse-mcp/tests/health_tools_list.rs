use anyhow::Context;
use serde_json::Value;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

#[tokio::test]
async fn health_and_action_tools_appear_in_tools_list_with_schema() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init().await?;
    assert!(
        client
            .raw_received()
            .iter()
            .any(|line| line.contains("\"tools\"") && line.contains("\"listChanged\":true")),
        "initialize response must advertise tools.listChanged: {:?}",
        client.raw_received()
    );
    let resp = client.tools_list().await?;
    let tools = resp
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let health_tool = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&Value::String("health".to_owned())))
        .context("health tool missing")?;

    assert!(
        health_tool["description"]
            .as_str()
            .is_some_and(|description| description.starts_with("Return server health")),
        "health description should start with the stable prefix: {:?}",
        health_tool["description"]
    );
    assert_eq!(health_tool["inputSchema"]["type"], "object");
    let set_value_tool = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&Value::String("act_set_value".to_owned())))
        .context("act_set_value tool missing")?;
    assert_eq!(set_value_tool["inputSchema"]["type"], "object");
    assert_eq!(
        set_value_tool["inputSchema"]["additionalProperties"],
        Value::Bool(false)
    );
    assert!(
        set_value_tool["inputSchema"]["properties"]["element_id"]
            .get("$ref")
            .and_then(Value::as_str)
            .is_some_and(|reference| reference.contains("ElementId"))
    );
    assert_eq!(
        set_value_tool["inputSchema"]["properties"]["text"]["type"],
        "string"
    );

    let focus_tool = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&Value::String("act_focus_window".to_owned())))
        .context("act_focus_window tool missing")?;
    assert_eq!(focus_tool["inputSchema"]["type"], "object");
    assert_eq!(
        focus_tool["inputSchema"]["additionalProperties"],
        Value::Bool(false)
    );
    assert_schema_accepts_type(&focus_tool["inputSchema"]["properties"]["hwnd"], "integer");
    assert_schema_accepts_type(
        &focus_tool["inputSchema"]["properties"]["title_regex"],
        "string",
    );
    assert_schema_accepts_type(&focus_tool["inputSchema"]["properties"]["pid"], "integer");
    assert!(
        focus_tool["description"]
            .as_str()
            .is_some_and(|description| description.contains("fails closed"))
    );

    let click_tool = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&Value::String("act_click".to_owned())))
        .context("act_click tool missing")?;
    assert_eq!(
        click_tool["inputSchema"]["properties"]["coordinate_fallback_on_unsupported"]["default"],
        Value::Bool(true)
    );
    assert!(
        click_tool["description"]
            .as_str()
            .is_some_and(|description| description.contains("coordinate_fallback_on_unsupported"))
    );
    assert!(
        click_tool["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("edit/document/text or Value/Text targets bypass PostMessage")
            })
    );

    let set_target_tool = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&Value::String("set_target".to_owned())))
        .context("set_target tool missing")?;
    assert_eq!(set_target_tool["inputSchema"]["type"], "object");
    assert_eq!(
        set_target_tool["inputSchema"]["additionalProperties"],
        Value::Bool(false)
    );
    assert_eq!(
        set_target_tool["inputSchema"]["required"],
        serde_json::json!(["target"])
    );
    let target_schema = &set_target_tool["inputSchema"]["properties"]["target"];
    let target_variants = target_schema["oneOf"]
        .as_array()
        .context("set_target target schema oneOf missing")?;
    assert_eq!(target_variants.len(), 2);
    assert!(
        target_variants
            .iter()
            .any(|variant| variant["properties"]["kind"]["const"] == "window"
                && variant["required"] == serde_json::json!(["kind", "window_hwnd"])
                && variant["properties"]["window_hwnd"]["type"] == "integer")
    );
    assert!(
        target_variants
            .iter()
            .any(|variant| variant["properties"]["kind"]["const"] == "cdp"
                && variant["required"]
                    == serde_json::json!(["kind", "window_hwnd", "cdp_target_id"])
                && variant["properties"]["cdp_target_id"]["type"] == "string")
    );
    let set_target_description = set_target_tool["description"]
        .as_str()
        .context("set_target description missing")?;
    assert!(set_target_description.contains("\"kind\":\"window\""));
    assert!(set_target_description.contains("\"kind\":\"cdp\""));
    assert!(set_target_description.contains("Legacy {\"hwnd\":...} is intentionally unsupported"));

    let type_tool = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&Value::String("act_type".to_owned())))
        .context("act_type tool missing")?;
    let type_description = type_tool["description"]
        .as_str()
        .context("act_type description missing")?;
    assert!(type_description.contains("foreground-safe native HWND text messages"));
    assert!(type_description.contains("UIA ValuePattern.SetValue"));
    assert!(type_description.contains("leased foreground click/type fallback"));
    assert!(type_description.contains("target window is already foreground"));
    assert!(type_description.contains("leased foreground keyboard backend"));

    assert!(
        client
            .raw_received()
            .iter()
            .any(|line| line.contains("\"tools\"") && line.contains("\"act_focus_window\""))
    );
    // #1554: health now defaults to compact (detail prose omitted). Request
    // detail=full to assert the verbose chrome_bridge detail blob below.
    let health_response = client
        .tools_call("health", serde_json::json!({ "detail": "full" }))
        .await?;
    let health = health_response
        .get("structuredContent")
        .context("health structuredContent missing")?;
    assert!(health["tool_count"].as_u64().is_some_and(|count| count > 0));
    assert!(
        health["tool_names"]
            .as_array()
            .is_some_and(|tools| tools.contains(&Value::String("act_run_shell_status".to_owned())))
    );
    // #1612: health's reported tool surface must be *identical* to the surface
    // actually served by tools/list for the same session. Both are derived from
    // the single `tools_for_session_profile` source of truth, so any future
    // divergence (e.g. a reintroduced hand-maintained parallel list) fails here.
    let served_names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    let health_names = health["tool_names"]
        .as_array()
        .context("health tool_names array missing")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        served_names,
        health_names,
        "health tool_names must equal the served tools/list surface; served-only={:?} health-only={:?}",
        served_names.difference(&health_names).collect::<Vec<_>>(),
        health_names.difference(&served_names).collect::<Vec<_>>(),
    );
    assert_eq!(
        health["tool_count"].as_u64(),
        Some(served_names.len() as u64),
        "health tool_count must equal the served tool surface size"
    );
    assert_eq!(
        health["subsystems"]["action"]["run_shell_inline_await_limit_ms"],
        Value::from(90_000)
    );
    assert_eq!(
        health["subsystems"]["action"]["run_shell_inline_client_call_budget_ms"],
        Value::from(110_000)
    );
    let action_health = health["subsystems"]["action"]
        .as_object()
        .context("action health object missing")?;
    assert_eq!(
        action_health.get("run_shell_durable_default_timeout_ms"),
        Some(&Value::Null)
    );
    assert_eq!(
        action_health.get("run_shell_durable_max_timeout_ms"),
        Some(&Value::Null)
    );
    let chrome_bridge = health["subsystems"]["chrome_bridge"]
        .as_object()
        .context("chrome_bridge health object missing")?;
    assert!(
        chrome_bridge
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(
                status,
                "ok" | "connecting" | "unavailable" | "unsafe_profile"
            )),
        "chrome_bridge status should be an explicit readiness state: {chrome_bridge:?}"
    );
    let chrome_bridge_detail = chrome_bridge
        .get("detail")
        .and_then(Value::as_str)
        .context("chrome_bridge detail missing")?;
    assert!(chrome_bridge_detail.contains("tab_control_available="));
    assert!(
        chrome_bridge_detail.contains("expected_extension_id=leoocgnkjnplbfdbklajepahofecgfbk")
    );
    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn assert_schema_accepts_type(schema: &Value, expected: &str) {
    if schema.get("type") == Some(&Value::String(expected.to_owned())) {
        return;
    }
    assert!(
        schema
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|types| types.contains(&Value::String(expected.to_owned()))),
        "schema {schema:?} did not accept type {expected}"
    );
}
