use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

const EXPECTED_TOOLS: [&str; 53] = [
    "act_click",
    "act_clipboard",
    "act_keymap",
    "act_pad",
    "act_press",
    "act_scroll",
    "act_stroke",
    "act_type",
    "armed_routine_tick",
    "audio_tail",
    "audio_transcribe",
    "audit_export_bundle",
    "audit_intelligence_query",
    "capture_screenshot",
    "demo_record_start",
    "demo_record_stop",
    "find",
    "health",
    "hidden_desktop_pip_frame",
    "hygiene_flags",
    "hygiene_scan_storage",
    "hygiene_scan_text",
    "observe",
    "profile_activate",
    "profile_authoring_decide",
    "profile_authoring_export",
    "profile_authoring_generate",
    "profile_authoring_inspect",
    "profile_authoring_list",
    "profile_list",
    "profile_quality_refresh",
    "profile_registry_disable",
    "profile_registry_export",
    "profile_registry_import",
    "profile_registry_install",
    "profile_registry_query",
    "profile_registry_rollback",
    "read_text",
    "reflex_cancel",
    "reflex_history",
    "reflex_list",
    "reflex_register",
    "release_all",
    "replay_record",
    "routine_automate",
    "set_capture_target",
    "set_perception_mode",
    "storage_gc_once",
    "storage_inspect",
    "storage_pressure_sample",
    "storage_put_probe_rows",
    "subscribe",
    "subscribe_cancel",
];

#[tokio::test]
async fn m3_tools_list_snapshot_defaults_and_closed_schemas() -> anyhow::Result<()> {
    // Debug tools enabled so the snapshot covers the full surface, including the
    // SYNAPSE_DEBUG_TOOLS-gated storage probes.
    let mut client =
        StdioMcpClient::launch_and_init_with_env(None, &[("SYNAPSE_DEBUG_TOOLS", "1")]).await?;
    let response = client.tools_list().await?;
    let tools = response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;

    let names = sorted_m3_tool_names(tools)?;
    let expected = EXPECTED_TOOLS
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
    assert_no_duplicate_names(&names)?;

    assert_schema_roots_closed(tools)?;
    assert_motion_semantics_are_advertised(tools)?;
    assert_act_press_foreground_policy_described(tools)?;
    let defaults = m3_default_readbacks(tools)?;

    let snapshot = json!({
        "count": names.len(),
        "tools": names,
        "m3_defaults": defaults,
    });
    insta::assert_json_snapshot!("m3_tools_list", snapshot);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn sorted_m3_tool_names(tools: &[Value]) -> anyhow::Result<Vec<String>> {
    let expected = EXPECTED_TOOLS.iter().copied().collect::<BTreeSet<_>>();
    let mut names = sorted_tool_names(tools)?
        .into_iter()
        .filter(|name| expected.contains(name.as_str()))
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

fn sorted_tool_names(tools: &[Value]) -> anyhow::Result<Vec<String>> {
    let mut names = tools
        .iter()
        .map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .context("tool name missing")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn assert_no_duplicate_names(names: &[String]) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        ensure!(seen.insert(name), "duplicate tool name: {name}");
    }
    Ok(())
}

fn assert_schema_roots_closed(tools: &[Value]) -> anyhow::Result<()> {
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .context("tool name missing")?;
        for schema_key in ["inputSchema", "outputSchema"] {
            let Some(schema) = tool.get(schema_key) else {
                continue;
            };
            ensure!(
                schema.get("type") == Some(&json!("object")),
                "{name}.{schema_key}.type must be object"
            );
            ensure!(
                schema.get("additionalProperties") == Some(&json!(false)),
                "{name}.{schema_key}.additionalProperties must be false"
            );
        }
    }
    Ok(())
}

fn assert_motion_semantics_are_advertised(tools: &[Value]) -> anyhow::Result<()> {
    ensure!(
        tool_by_name(tools, "act_aim").is_err() && tool_by_name(tools, "act_drag").is_err(),
        "act_aim and act_drag must be removed from tools/list after act_stroke unification"
    );

    let click = tool_by_name(tools, "act_click")?;
    let click_description = click
        .get("description")
        .and_then(Value::as_str)
        .context("act_click description missing")?;
    ensure!(
        click_description.contains("velocity_profile")
            && click_description.contains("coordinate-move timing")
            && click_description.contains("act_stroke"),
        "act_click description must separate timing from spatial paths: {click_description}"
    );
    value_at(click, "inputSchema.properties.velocity_profile")
        .context("act_click schema must advertise velocity_profile")?;
    ensure!(
        value_at(click, "inputSchema.properties.curve").is_err(),
        "act_click schema must not advertise deprecated curve"
    );

    let stroke_description = tool_by_name(tools, "act_stroke")?
        .get("description")
        .and_then(Value::as_str)
        .context("act_stroke description missing")?;
    ensure!(
        stroke_description.contains("explicit spatial path"),
        "act_stroke description must advertise explicit spatial path ownership: {stroke_description}"
    );
    ensure!(
        stroke_description.contains("point/element target"),
        "act_stroke description must advertise point/element target ownership: {stroke_description}"
    );
    ensure!(
        stroke_description.contains("button set drags"),
        "act_stroke description must advertise optional-button drag semantics: {stroke_description}"
    );
    ensure!(
        stroke_description.contains("motion_model"),
        "act_stroke description must advertise motion_model ownership: {stroke_description}"
    );
    ensure!(
        stroke_description.contains("wind_mouse"),
        "act_stroke description must advertise wind_mouse availability: {stroke_description}"
    );
    Ok(())
}

fn assert_act_press_foreground_policy_described(tools: &[Value]) -> anyhow::Result<()> {
    let tool = tool_by_name(tools, "act_press")?;

    let allow_description = value_at(
        tool,
        "inputSchema.properties.allow_foreground_change.description",
    )?
    .as_str()
    .context("act_press allow_foreground_change description missing")?;
    ensure!(
        allow_description.contains("accept a foreground-window identity change")
            && allow_description.contains("unexpected focus loss still fails closed"),
        "act_press allow_foreground_change schema must describe fail-closed foreground transition semantics: {allow_description}"
    );

    let process_description = value_at(
        tool,
        "inputSchema.properties.expected_foreground_process_regex.description",
    )?
    .as_str()
    .context("act_press expected_foreground_process_regex description missing")?;
    ensure!(
        process_description.contains("after-read foreground process name")
            && process_description.contains("Invalid regexes fail before key input is sent"),
        "act_press expected_foreground_process_regex schema must describe pre-input regex validation: {process_description}"
    );

    let title_description = value_at(
        tool,
        "inputSchema.properties.expected_foreground_title_regex.description",
    )?
    .as_str()
    .context("act_press expected_foreground_title_regex description missing")?;
    ensure!(
        title_description.contains("after-read foreground window title")
            && title_description.contains("Invalid regexes fail before key input is sent"),
        "act_press expected_foreground_title_regex schema must describe pre-input regex validation: {title_description}"
    );

    Ok(())
}

fn m3_default_readbacks(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let mut readbacks = Vec::new();
    read_schema_defaults(&mut readbacks, tools)?;
    read_required_fields(&mut readbacks, tools)?;
    Ok(readbacks)
}

#[allow(clippy::too_many_lines)]
fn read_schema_defaults(readbacks: &mut Vec<Value>, tools: &[Value]) -> anyhow::Result<()> {
    read_default(
        readbacks,
        tools,
        "act_keymap",
        "inputSchema.properties.hold_ms.default",
        &json!(33),
    )?;
    read_default(
        readbacks,
        tools,
        "act_keymap",
        "inputSchema.properties.backend.default",
        &json!("auto"),
    )?;
    read_default(
        readbacks,
        tools,
        "act_click",
        "inputSchema.properties.velocity_profile.default",
        &json!("natural"),
    )?;
    read_default(
        readbacks,
        tools,
        "act_click",
        "inputSchema.properties.hold_ms.default",
        &json!(120),
    )?;
    read_default(
        readbacks,
        tools,
        "act_stroke",
        "inputSchema.properties.velocity_profile.default",
        &json!("constant"),
    )?;
    read_default(
        readbacks,
        tools,
        "act_stroke",
        "inputSchema.properties.motion_model.default",
        &json!({"kind": "path"}),
    )?;
    read_default(
        readbacks,
        tools,
        "act_stroke",
        "inputSchema.properties.backend.default",
        &json!("auto"),
    )?;
    read_default(
        readbacks,
        tools,
        "subscribe",
        "inputSchema.properties.kinds.default",
        &json!([]),
    )?;
    read_default(
        readbacks,
        tools,
        "subscribe",
        "inputSchema.properties.snapshot_first.default",
        &json!(false),
    )?;
    read_default(
        readbacks,
        tools,
        "subscribe",
        "inputSchema.properties.buffer_size.default",
        &json!(4096),
    )?;
    read_default(
        readbacks,
        tools,
        "reflex_register",
        "inputSchema.properties.priority.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "reflex_register",
        "inputSchema.properties.lifetime.default",
        &json!({"kind": "until_cancelled"}),
    )?;
    read_default(
        readbacks,
        tools,
        "reflex_register",
        "inputSchema.properties.backend.default",
        &json!("auto"),
    )?;
    read_default(
        readbacks,
        tools,
        "reflex_list",
        "inputSchema.properties.include_expired.default",
        &json!(false),
    )?;
    read_default(
        readbacks,
        tools,
        "reflex_history",
        "inputSchema.properties.limit.default",
        &json!(50),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_list",
        "inputSchema.properties.include_inactive.default",
        &json!(true),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_authoring_generate",
        "inputSchema.properties.max_audit_rows.default",
        &json!(500),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_authoring_generate",
        "inputSchema.properties.max_replay_rows.default",
        &json!(500),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_authoring_list",
        "inputSchema.properties.limit.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_quality_refresh",
        "inputSchema.properties.max_audit_rows.default",
        &json!(5000),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_quality_refresh",
        "inputSchema.properties.stale_after_ns.default",
        &json!(86_400_000_000_000_u64),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_query",
        "inputSchema.properties.include_disabled.default",
        &json!(false),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_query",
        "inputSchema.properties.limit.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_install",
        "inputSchema.properties.source_id.default",
        &json!("registry.local"),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_install",
        "inputSchema.properties.trust_policy.default",
        &json!("local_first"),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_query",
        "inputSchema.properties.limit.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_query",
        "inputSchema.properties.max_audit_rows.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_disable",
        "inputSchema.properties.state.default",
        &json!("disabled"),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_export",
        "inputSchema.properties.include_disabled.default",
        &json!(false),
    )?;
    read_default(
        readbacks,
        tools,
        "profile_registry_export",
        "inputSchema.properties.limit.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "audit_intelligence_query",
        "inputSchema.properties.max_rows.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "audit_export_bundle",
        "inputSchema.$defs.AuditExportConsentParams.properties.redaction_policy.default",
        &json!("strict"),
    )?;
    read_default(
        readbacks,
        tools,
        "audit_export_bundle",
        "inputSchema.properties.max_rows.default",
        &json!(100),
    )?;
    read_default(
        readbacks,
        tools,
        "audit_export_bundle",
        "inputSchema.properties.max_row_bytes.default",
        &json!(65_536),
    )?;
    read_default(
        readbacks,
        tools,
        "replay_record",
        "inputSchema.properties.format.default",
        &json!("jsonl"),
    )?;
    read_default(
        readbacks,
        tools,
        "replay_record",
        "inputSchema.properties.target.default",
        &json!("observations"),
    )?;
    read_default(
        readbacks,
        tools,
        "audio_tail",
        "inputSchema.properties.seconds.default",
        &json!(5.0),
    )?;
    read_default(
        readbacks,
        tools,
        "audio_transcribe",
        "inputSchema.properties.seconds.default",
        &json!(5.0),
    )?;
    read_default(
        readbacks,
        tools,
        "audio_transcribe",
        "inputSchema.properties.language.default",
        &json!("en"),
    )?;
    Ok(())
}

fn read_required_fields(readbacks: &mut Vec<Value>, tools: &[Value]) -> anyhow::Result<()> {
    read_required(readbacks, tools, "act_keymap", "alias")?;
    read_property(readbacks, tools, "act_stroke", "path")?;
    read_property(readbacks, tools, "act_stroke", "target")?;
    read_property(readbacks, tools, "act_stroke", "from")?;
    read_property(readbacks, tools, "act_stroke", "to")?;
    read_required(readbacks, tools, "act_stroke", "duration_or_speed")?;
    read_required(readbacks, tools, "subscribe_cancel", "subscription_id")?;
    read_required(readbacks, tools, "reflex_cancel", "reflex_id")?;
    read_required(readbacks, tools, "profile_activate", "profile_id")?;
    read_required(readbacks, tools, "profile_authoring_generate", "profile_id")?;
    read_required(
        readbacks,
        tools,
        "profile_authoring_inspect",
        "candidate_id",
    )?;
    read_required(readbacks, tools, "profile_authoring_decide", "candidate_id")?;
    read_required(readbacks, tools, "profile_authoring_decide", "decision")?;
    read_required(readbacks, tools, "profile_authoring_export", "candidate_id")?;
    read_required(readbacks, tools, "profile_authoring_export", "output_path")?;
    read_required(readbacks, tools, "routine_automate", "routine_id")?;
    read_required(readbacks, tools, "profile_quality_refresh", "profile_id")?;
    read_required(
        readbacks,
        tools,
        "profile_registry_install",
        "manifest_path",
    )?;
    read_required(readbacks, tools, "profile_registry_query", "view")?;
    read_required(readbacks, tools, "profile_registry_disable", "profile_id")?;
    read_required(readbacks, tools, "profile_registry_export", "output_path")?;
    read_required(readbacks, tools, "profile_registry_import", "bundle_path")?;
    read_required(readbacks, tools, "profile_registry_rollback", "profile_id")?;
    read_required(readbacks, tools, "audit_intelligence_query", "profile_id")?;
    read_required(readbacks, tools, "audit_export_bundle", "profile_id")?;
    read_required(readbacks, tools, "audit_export_bundle", "output_path")?;
    read_required(readbacks, tools, "storage_put_probe_rows", "cf_name")?;
    read_required(readbacks, tools, "storage_put_probe_rows", "key_prefix")?;
    read_required(readbacks, tools, "storage_put_probe_rows", "rows")?;
    read_required(readbacks, tools, "storage_put_probe_rows", "value_bytes")?;
    read_required(readbacks, tools, "storage_gc_once", "cf_name")?;
    read_required(readbacks, tools, "storage_gc_once", "soft_cap_rows")?;
    read_required(readbacks, tools, "storage_gc_once", "hard_cap_rows")?;
    read_required(readbacks, tools, "storage_pressure_sample", "free_bytes")?;
    Ok(())
}

fn read_default(
    readbacks: &mut Vec<Value>,
    tools: &[Value],
    tool_name: &str,
    path: &str,
    expected: &Value,
) -> anyhow::Result<()> {
    let actual = value_at(tool_by_name(tools, tool_name)?, path)?.clone();
    assert_eq!(&actual, expected, "{tool_name}.{path}");
    readbacks.push(json!({
        "tool": tool_name,
        "path": path,
        "actual": actual,
    }));
    Ok(())
}

fn read_required(
    readbacks: &mut Vec<Value>,
    tools: &[Value],
    tool_name: &str,
    field: &str,
) -> anyhow::Result<()> {
    let required = value_at(tool_by_name(tools, tool_name)?, "inputSchema.required")?;
    let required = required
        .as_array()
        .with_context(|| format!("{tool_name}.inputSchema.required must be an array"))?;
    ensure!(
        required.iter().any(|value| value.as_str() == Some(field)),
        "{tool_name}.inputSchema.required must contain {field}"
    );
    readbacks.push(json!({
        "tool": tool_name,
        "path": "inputSchema.required",
        "actual_contains": field,
    }));
    Ok(())
}

fn read_property(
    readbacks: &mut Vec<Value>,
    tools: &[Value],
    tool_name: &str,
    field: &str,
) -> anyhow::Result<()> {
    value_at(
        tool_by_name(tools, tool_name)?,
        &format!("inputSchema.properties.{field}"),
    )?;
    readbacks.push(json!({
        "tool": tool_name,
        "path": "inputSchema.properties",
        "actual_contains": field,
    }));
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
