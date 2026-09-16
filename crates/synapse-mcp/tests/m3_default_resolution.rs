use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn m3_default_resolution_matches_impplan_table_and_natural_motion() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(logs.path())).await?;

    let response = client.tools_list().await?;
    let tools = response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;

    let defaults = assert_impplan_defaults(tools)?;
    let required = assert_required_fields(tools)?;
    let motion_defaults = assert_no_instant_or_burst_motion_defaults(tools)?;

    let snapshot = json!({
        "impplan_defaults": defaults,
        "required": required,
        "motion_defaults": motion_defaults,
    });
    insta::assert_json_snapshot!("m3_default_resolution", snapshot);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn assert_impplan_defaults(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let rows = [
        (
            "subscribe",
            "inputSchema.properties.kinds.default",
            json!([]),
        ),
        (
            "subscribe",
            "inputSchema.properties.snapshot_first.default",
            json!(false),
        ),
        (
            "subscribe",
            "inputSchema.properties.buffer_size.default",
            json!(4096),
        ),
        (
            "reflex_register",
            "inputSchema.properties.priority.default",
            json!(100),
        ),
        (
            "reflex_register",
            "inputSchema.properties.lifetime.default",
            json!({"kind": "until_cancelled"}),
        ),
        (
            "reflex_register",
            "inputSchema.properties.backend.default",
            json!("auto"),
        ),
        (
            "reflex_list",
            "inputSchema.properties.include_expired.default",
            json!(false),
        ),
        (
            "reflex_history",
            "inputSchema.properties.limit.default",
            json!(50),
        ),
        (
            "profile_list",
            "inputSchema.properties.include_inactive.default",
            json!(true),
        ),
        (
            "replay_record",
            "inputSchema.properties.format.default",
            json!("jsonl"),
        ),
        (
            "replay_record",
            "inputSchema.properties.target.default",
            json!("observations"),
        ),
        (
            "audio_tail",
            "inputSchema.properties.seconds.default",
            json!(5.0),
        ),
        (
            "audio_transcribe",
            "inputSchema.properties.seconds.default",
            json!(5.0),
        ),
        (
            "audio_transcribe",
            "inputSchema.properties.language.default",
            json!("en"),
        ),
    ];

    let mut observed = Vec::new();
    for (tool, path, expected) in rows {
        let actual = value_at(tool_by_name(tools, tool)?, path)?.clone();
        assert_eq!(actual, expected, "{tool}.{path}");
        observed.push(json!({
            "tool": tool,
            "path": path,
            "actual": actual,
        }));
    }
    Ok(observed)
}

fn assert_required_fields(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let rows = [
        ("subscribe_cancel", "subscription_id"),
        ("reflex_cancel", "reflex_id"),
        ("profile_activate", "profile_id"),
    ];

    let mut observed = Vec::new();
    for (tool, field) in rows {
        let required = value_at(tool_by_name(tools, tool)?, "inputSchema.required")?
            .as_array()
            .with_context(|| format!("{tool}.inputSchema.required must be an array"))?;
        ensure!(
            required.iter().any(|value| value.as_str() == Some(field)),
            "{tool}.inputSchema.required must contain {field}"
        );
        observed.push(json!({
            "tool": tool,
            "path": "inputSchema.required",
            "actual_contains": field,
        }));
    }
    Ok(observed)
}

fn assert_no_instant_or_burst_motion_defaults(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let mut observed = Vec::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .context("tool name missing")?;
        collect_motion_defaults(name, tool, name, &mut observed)?;
    }
    observed.sort_by_key(|row| {
        format!(
            "{}:{}",
            row["tool"].as_str().unwrap_or_default(),
            row["path"].as_str().unwrap_or_default()
        )
    });
    ensure!(
        observed.iter().any(|row| row["tool"] == "act_type"
            && row["path"] == "act_type.inputSchema.properties.dynamics.default"
            && row["actual"] == "natural"),
        "act_type dynamics default must be natural"
    );
    ensure!(
        observed.iter().any(|row| row["tool"] == "act_click"
            && row["path"] == "act_click.inputSchema.properties.velocity_profile.default"
            && row["actual"] == "natural"),
        "act_click velocity_profile default must be natural"
    );
    Ok(observed)
}

fn collect_motion_defaults(
    tool: &str,
    value: &Value,
    path: &str,
    observed: &mut Vec<Value>,
) -> anyhow::Result<()> {
    match value {
        Value::Object(map) => {
            if (path.ends_with(".properties.velocity_profile")
                || path.ends_with(".properties.dynamics"))
                && let Some(default) = map.get("default")
            {
                let forbidden = default
                    .as_str()
                    .is_some_and(|text| matches!(text, "instant" | "burst"));
                ensure!(
                    !forbidden,
                    "{tool}.{path}.default must not be instant/burst: {default}"
                );
                observed.push(json!({
                    "tool": tool,
                    "path": format!("{path}.default"),
                    "actual": default,
                }));
            }
            for (key, child) in map {
                collect_motion_defaults(tool, child, &format!("{path}.{key}"), observed)?;
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_motion_defaults(tool, child, &format!("{path}[{index}]"), observed)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

fn tool_by_name<'a>(tools: &'a [Value], name: &str) -> anyhow::Result<&'a Value> {
    tools
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
