use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_core::error_codes;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn act_stroke_is_the_unified_public_motion_tool() -> anyhow::Result<()> {
    let log_dir = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(log_dir.path()),
        &[
            ("SYNAPSE_DB", db_path_string.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
            ("SYNAPSE_MCP_RECORDING_BACKEND", "1"),
        ],
    )
    .await?;
    client
        .tools_call("profile_activate", json!({"profile_id": "notepad"}))
        .await?;

    let tools_response = client.tools_list().await?;
    ensure!(
        tool_by_name(&tools_response, "act_aim").is_err()
            && tool_by_name(&tools_response, "act_drag").is_err(),
        "act_aim and act_drag must not be advertised after act_stroke unification"
    );
    let stroke = tool_by_name(&tools_response, "act_stroke")?;
    let description = stroke
        .get("description")
        .and_then(Value::as_str)
        .context("act_stroke description missing")?;
    ensure!(
        description.contains("point/element target")
            && description.contains("button set drags")
            && description.contains("explicit spatial path"),
        "act_stroke description must describe unified target/path/button semantics: {description}"
    );
    for field in ["path", "target", "from", "to", "button"] {
        value_at(stroke, &format!("inputSchema.properties.{field}"))
            .with_context(|| format!("act_stroke schema must advertise {field}"))?;
    }
    ensure!(
        value_at(stroke, "inputSchema.properties.curve").is_err(),
        "act_stroke schema must not advertise the old act_drag curve alias"
    );

    let drag = structured(
        &client
            .tools_call(
                "act_stroke",
                json!({
                    "from": {"x": 10.0, "y": 20.0},
                    "to": {"x": 70.0, "y": 80.0},
                    "button": "left",
                    "velocity_profile": "linear",
                    "duration_or_speed": {"kind": "duration_ms", "duration_ms": 80},
                    "backend": "software"
                }),
            )
            .await?,
    )?;
    println!("readback=mcp_act_stroke edge=unified_drag after={drag}");
    assert_eq!(drag["path_kind"], "line");
    assert_eq!(drag["button_used"], "left");
    assert_eq!(drag["velocity_profile_used"], "linear");
    assert_eq!(drag["duration_ms"], 80.0);
    assert!(
        drag["point_stream_count"]
            .as_u64()
            .is_some_and(|count| count > 1)
    );

    let legacy_curve = client
        .tools_call_error(
            "act_stroke",
            json!({
                "from": {"x": 10.0, "y": 20.0},
                "to": {"x": 70.0, "y": 80.0},
                "curve": "bezier",
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 80},
                "backend": "software"
            }),
        )
        .await?;
    println!("readback=mcp_act_stroke edge=legacy_curve_rejected after_error={legacy_curve}");
    assert_eq!(
        error_code(&legacy_curve),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    assert!(client.shutdown().await?.success());
    Ok(())
}

fn structured(resp: &Value) -> anyhow::Result<Value> {
    resp.get("structuredContent")
        .cloned()
        .context("structuredContent missing")
}

fn tool_by_name<'a>(tools_response: &'a Value, name: &str) -> anyhow::Result<&'a Value> {
    tools_response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        .with_context(|| format!("tool missing from tools/list: {name}"))
}

fn value_at<'a>(value: &'a Value, path: &str) -> anyhow::Result<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current
            .get(segment)
            .with_context(|| format!("missing path {path}"))?;
    }
    Ok(current)
}

fn error_code(error: &Value) -> Option<&str> {
    error
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}
