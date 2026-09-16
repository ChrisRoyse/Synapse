use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

const M4_TOOLS: [&str; 3] = ["act_combo", "act_run_shell", "act_launch"];

#[tokio::test]
async fn m4_default_resolution_matches_prd_table() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init().await?;
    let response = client.tools_list().await?;
    let tools = response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;

    let defaults = assert_prd_defaults(tools)?;
    let non_schema_defaults = assert_prd_non_schema_defaults(tools)?;
    let observed_default_paths = assert_no_unexpected_m4_defaults(tools)?;

    let snapshot = json!({
        "defaults": defaults,
        "non_schema_defaults": non_schema_defaults,
        "observed_default_paths": observed_default_paths,
    });
    insta::assert_json_snapshot!("m4_default_resolution", snapshot);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn assert_prd_defaults(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let rows = [
        (
            "act_combo",
            "inputSchema.properties.backend.default",
            json!("auto"),
        ),
        (
            "act_run_shell",
            "inputSchema.properties.args.default",
            json!([]),
        ),
        (
            "act_run_shell",
            "inputSchema.properties.env.default",
            json!({}),
        ),
        (
            "act_run_shell",
            "inputSchema.properties.timeout_ms.default",
            json!(30000),
        ),
        (
            "act_launch",
            "inputSchema.properties.args.default",
            json!([]),
        ),
        (
            "act_launch",
            "inputSchema.properties.cdp_debug.default",
            Value::Null,
        ),
        (
            "act_launch",
            "inputSchema.properties.desktop.default",
            Value::Null,
        ),
        (
            "act_launch",
            "inputSchema.properties.env.default",
            json!({}),
        ),
        (
            "act_launch",
            "inputSchema.properties.force_renderer_accessibility.default",
            Value::Null,
        ),
        (
            "act_launch",
            "inputSchema.properties.timeout_ms.default",
            json!(10000),
        ),
        (
            "act_launch",
            "inputSchema.properties.windows_console_window_state.default",
            Value::Null,
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

fn assert_prd_non_schema_defaults(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let mut observed = Vec::new();
    assert_required(
        &mut observed,
        tools,
        "act_combo",
        "inputSchema.required",
        "steps",
    )?;
    assert_required(
        &mut observed,
        tools,
        "act_combo",
        "inputSchema.$defs.ActComboStep.required",
        "params",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_combo",
        "inputSchema.$defs.ActComboStep.properties.backend.default",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_combo",
        "inputSchema.properties.idempotency_key.default",
    )?;
    assert_required(
        &mut observed,
        tools,
        "act_run_shell",
        "inputSchema.required",
        "command",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_run_shell",
        "inputSchema.properties.working_dir.default",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_run_shell",
        "inputSchema.properties.idempotency_key.default",
    )?;
    assert_required(
        &mut observed,
        tools,
        "act_launch",
        "inputSchema.required",
        "target",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_launch",
        "inputSchema.properties.working_dir.default",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_launch",
        "inputSchema.properties.wait_for_window_title_regex.default",
    )?;
    assert_default_absent(
        &mut observed,
        tools,
        "act_launch",
        "inputSchema.properties.idempotency_key.default",
    )?;
    Ok(observed)
}

fn assert_required(
    observed: &mut Vec<Value>,
    tools: &[Value],
    tool: &str,
    path: &str,
    field: &str,
) -> anyhow::Result<()> {
    let required = value_at(tool_by_name(tools, tool)?, path)?
        .as_array()
        .with_context(|| format!("{tool}.{path} must be an array"))?;
    ensure!(
        required.iter().any(|value| value.as_str() == Some(field)),
        "{tool}.{path} must contain {field}"
    );
    observed.push(json!({
        "tool": tool,
        "path": path,
        "actual_contains": field,
    }));
    Ok(())
}

fn assert_default_absent(
    observed: &mut Vec<Value>,
    tools: &[Value],
    tool: &str,
    path: &str,
) -> anyhow::Result<()> {
    ensure!(
        value_at_optional(tool_by_name(tools, tool)?, path).is_none(),
        "{tool}.{path} must not expose a JSON-Schema default"
    );
    observed.push(json!({
        "tool": tool,
        "path": path,
        "actual": "absent",
    }));
    Ok(())
}

fn assert_no_unexpected_m4_defaults(tools: &[Value]) -> anyhow::Result<Vec<String>> {
    let expected = [
        "act_combo.inputSchema.properties.backend.default",
        "act_launch.inputSchema.properties.args.default",
        "act_launch.inputSchema.properties.cdp_debug.default",
        "act_launch.inputSchema.properties.desktop.default",
        "act_launch.inputSchema.properties.env.default",
        "act_launch.inputSchema.properties.force_renderer_accessibility.default",
        "act_launch.inputSchema.properties.timeout_ms.default",
        "act_launch.inputSchema.properties.windows_console_window_state.default",
        "act_run_shell.inputSchema.properties.args.default",
        "act_run_shell.inputSchema.properties.durable_timeout_ms.default",
        "act_run_shell.inputSchema.properties.env.default",
        "act_run_shell.inputSchema.properties.execution_mode.default",
        "act_run_shell.inputSchema.properties.timeout_ms.default",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();

    let mut actual = BTreeSet::new();
    for tool in M4_TOOLS {
        collect_default_paths(tool_by_name(tools, tool)?, tool, &mut actual);
    }
    ensure!(
        actual == expected,
        "unexpected M4 defaults: expected {expected:?}, actual {actual:?}"
    );
    Ok(actual.into_iter().collect())
}

fn collect_default_paths(value: &Value, path: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("default") {
                out.insert(format!("{path}.default"));
            }
            for (key, child) in map {
                collect_default_paths(child, &format!("{path}.{key}"), out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_default_paths(child, &format!("{path}[{index}]"), out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn tool_by_name<'a>(tools: &'a [Value], name: &str) -> anyhow::Result<&'a Value> {
    tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        .with_context(|| format!("tool missing from tools/list: {name}"))
}

fn value_at<'a>(value: &'a Value, path: &str) -> anyhow::Result<&'a Value> {
    value_at_optional(value, path).with_context(|| format!("missing path {path}"))
}

fn value_at_optional<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}
