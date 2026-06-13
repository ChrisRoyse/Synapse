use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

const EXPECTED_TOOLS: [&str; 149] = [
    "act_click",
    "act_clipboard",
    "act_combo",
    "act_focus_window",
    "act_keymap",
    "act_launch",
    "act_pad",
    "act_press",
    "act_run_shell",
    "act_run_shell_cancel",
    "act_run_shell_start",
    "act_run_shell_status",
    "act_scroll",
    "act_set_field_text",
    "act_set_value",
    "act_spawn_agent",
    "act_stroke",
    "act_type",
    "action_diagnostic_queue_full_setup",
    "action_diagnostic_rate_limit_override",
    "agent_cost",
    "agent_cost_price_delete",
    "agent_cost_price_list",
    "agent_cost_price_put",
    "agent_inbox",
    "agent_query",
    "agent_receipts",
    "agent_send",
    "agent_send_broadcast",
    "agent_template_delete",
    "agent_template_get",
    "agent_template_list",
    "agent_template_put",
    "agent_wait",
    "audio_tail",
    "audio_transcribe",
    "audit_export_bundle",
    "audit_intelligence_query",
    "capture_screenshot",
    "cdp_close_tab",
    "cdp_navigate_tab",
    "cdp_open_tab",
    "clear_target",
    "control_lease_acquire",
    "control_lease_handoff",
    "control_lease_release",
    "control_lease_status",
    "episode_get",
    "episode_list",
    "episode_segment",
    "escalation_ack",
    "escalation_config_get",
    "escalation_config_set",
    "escalation_list",
    "everquest_action_prior_record",
    "everquest_action_prior_scorecard",
    "everquest_autocombat",
    "everquest_chat_input_state",
    "everquest_contextgraph_ingest",
    "everquest_contextgraph_search",
    "everquest_current_state",
    "everquest_domain_normalize",
    "everquest_episode_export",
    "everquest_loc_probe",
    "everquest_map_sensor",
    "everquest_memory_consult",
    "everquest_memory_record",
    "everquest_outcome_ingest",
    "everquest_planner_guard",
    "everquest_predictive_model_fit",
    "everquest_predictive_model_predict",
    "everquest_route_plan",
    "everquest_safe_command",
    "everquest_surprise_detect",
    "everquest_survival_readiness",
    "everquest_trajectory_record",
    "everquest_world_model_inspect",
    "everquest_world_model_record",
    "everquest_world_summary",
    "find",
    "get_target",
    "health",
    "hidden_desktop_pip_frame",
    "hygiene_flags",
    "hygiene_scan_storage",
    "hygiene_scan_text",
    "notify_human",
    "observe",
    "observe_delta",
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
    "reality_audit",
    "reality_baseline",
    "reflex_cancel",
    "reflex_history",
    "reflex_list",
    "reflex_register",
    "release_all",
    "replay_record",
    "routine_inspect",
    "routine_list",
    "routine_mine",
    "routine_update",
    "session_end",
    "session_list",
    "session_status",
    "set_capture_target",
    "set_perception_mode",
    "set_target",
    "storage_gc_once",
    "storage_inspect",
    "storage_pressure_sample",
    "storage_put_probe_rows",
    "subscribe",
    "subscribe_cancel",
    "target_claim",
    "target_claim_adopt",
    "target_claim_status",
    "target_release",
    "task_cancel",
    "task_claim",
    "task_create",
    "task_get",
    "task_list",
    "task_next",
    "task_reconcile",
    "task_update",
    "timeline_exclusions",
    "timeline_pause",
    "timeline_purge",
    "timeline_resume",
    "timeline_search",
    "workspace_get",
    "workspace_list",
    "workspace_put",
    "workspace_subscribe",
];

#[tokio::test]
async fn m4_tools_list_snapshot_defaults_and_closed_schemas() -> anyhow::Result<()> {
    // Run with debug tools enabled so the asserted surface + snapshot covers the
    // full tool set, including the SYNAPSE_DEBUG_TOOLS-gated storage probes.
    let mut client = StdioMcpClient::launch_and_init_with_env(
        None,
        &[
            ("SYNAPSE_DEBUG_TOOLS", "1"),
            ("SYNAPSE_ENABLE_EVERQUEST", "1"),
        ],
    )
    .await?;
    let response = client.tools_list().await?;
    let tools = response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;

    let names = sorted_tool_names(tools)?;
    let expected = EXPECTED_TOOLS
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
    assert_eq!(names.len(), EXPECTED_TOOLS.len());
    assert_no_duplicate_names(&names)?;

    assert_schema_roots_closed(tools)?;
    assert_act_run_shell_semantics_described(tools)?;
    assert_act_press_foreground_policy_described(tools)?;
    let defaults = m4_default_readbacks(tools)?;

    let snapshot = json!({
        "count": names.len(),
        "tools": names,
        "m4_defaults": defaults,
    });
    insta::assert_json_snapshot!("m4_tools_list", snapshot);

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
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

fn assert_act_run_shell_semantics_described(tools: &[Value]) -> anyhow::Result<()> {
    let tool = tool_by_name(tools, "act_run_shell")?;
    let tool_description = tool
        .get("description")
        .and_then(Value::as_str)
        .context("act_run_shell description missing")?;
    ensure!(
        tool_description.contains("executable path/name only")
            && tool_description.contains("explicit shell executable")
            && tool_description.contains("execution_mode controls routing")
            && tool_description.contains("inline never backgrounds")
            && tool_description
                .contains("durable_timeout_ms is an explicit durable job lifetime cap"),
        "act_run_shell description must explain executable-plus-args semantics: {tool_description}"
    );

    let command_description = value_at(tool, "inputSchema.properties.command.description")?
        .as_str()
        .context("act_run_shell command description missing")?;
    ensure!(
        command_description.contains("Executable path or program name only")
            && command_description.contains("Do not include arguments"),
        "act_run_shell command schema must reject shell-command-string ambiguity: {command_description}"
    );

    let args_description = value_at(tool, "inputSchema.properties.args.description")?
        .as_str()
        .context("act_run_shell args description missing")?;
    ensure!(
        args_description.contains("Arguments passed literally")
            && args_description.contains("not parsed by a shell"),
        "act_run_shell args schema must explain literal argument passing: {args_description}"
    );

    let timeout_description = value_at(tool, "inputSchema.properties.timeout_ms.description")?
        .as_str()
        .context("act_run_shell timeout_ms description missing")?;
    ensure!(
        timeout_description.contains("Caller-requested inline wait budget")
            && timeout_description.contains("execution_mode=inline")
            && timeout_description.contains("execution_mode=auto"),
        "act_run_shell timeout_ms schema must separate inline and durable timeout semantics: {timeout_description}"
    );

    let execution_mode_description =
        value_at(tool, "inputSchema.properties.execution_mode.description")?
            .as_str()
            .context("act_run_shell execution_mode description missing")?;
    ensure!(
        execution_mode_description.contains("auto preserves compatibility")
            && execution_mode_description.contains("inline never backgrounds")
            && execution_mode_description.contains("durable immediately returns"),
        "act_run_shell execution_mode schema must describe routing choices: {execution_mode_description}"
    );

    let durable_timeout_description = value_at(
        tool,
        "inputSchema.properties.durable_timeout_ms.description",
    )?
    .as_str()
    .context("act_run_shell durable_timeout_ms description missing")?;
    ensure!(
        durable_timeout_description.contains("Optional explicit durable job lifetime cap")
            && durable_timeout_description.contains("Valid only when execution_mode=durable")
            && durable_timeout_description.contains("omit for an unbounded durable job"),
        "act_run_shell durable_timeout_ms schema must describe durable lifetime semantics: {durable_timeout_description}"
    );

    let start_tool = tool_by_name(tools, "act_run_shell_start")?;
    let start_description = start_tool
        .get("description")
        .and_then(Value::as_str)
        .context("act_run_shell_start description missing")?;
    ensure!(
        start_description.contains("Omitting timeout_ms")
            && start_description.contains("unbounded")
            && start_description.contains("explicit lifetime cap"),
        "act_run_shell_start description must explain durable lifetime cap semantics: {start_description}"
    );

    let start_timeout_description =
        value_at(start_tool, "inputSchema.properties.timeout_ms.description")?
            .as_str()
            .context("act_run_shell_start timeout_ms description missing")?;
    ensure!(
        start_timeout_description.contains("Optional explicit durable job lifetime cap")
            && start_timeout_description.contains("Omit for an unbounded job"),
        "act_run_shell_start timeout_ms schema must explain unbounded default: {start_timeout_description}"
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

#[allow(
    clippy::too_many_lines,
    reason = "schema readback table is intentionally kept as one ordered assertion"
)]
fn m4_default_readbacks(tools: &[Value]) -> anyhow::Result<Vec<Value>> {
    let mut readbacks = Vec::new();
    read_default(
        &mut readbacks,
        tools,
        "act_keymap",
        "inputSchema.properties.hold_ms.default",
        &json!(33),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_keymap",
        "inputSchema.properties.backend.default",
        &json!("auto"),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_click",
        "inputSchema.properties.hold_ms.default",
        &json!(120),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_stroke",
        "inputSchema.properties.velocity_profile.default",
        &json!("constant"),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_stroke",
        "inputSchema.properties.motion_model.default",
        &json!({"kind": "path"}),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_stroke",
        "inputSchema.properties.backend.default",
        &json!("auto"),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_combo",
        "inputSchema.properties.backend.default",
        &json!("auto"),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_run_shell",
        "inputSchema.properties.args.default",
        &json!([]),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_run_shell",
        "inputSchema.properties.env.default",
        &json!({}),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_run_shell",
        "inputSchema.properties.timeout_ms.default",
        &json!(30000),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_run_shell_start",
        "inputSchema.properties.args.default",
        &json!([]),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_run_shell_start",
        "inputSchema.properties.env.default",
        &json!({}),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_run_shell_status",
        "inputSchema.properties.tail_bytes.default",
        &json!(65536),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_launch",
        "inputSchema.properties.args.default",
        &json!([]),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_launch",
        "inputSchema.properties.env.default",
        &json!({}),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "act_launch",
        "inputSchema.properties.timeout_ms.default",
        &json!(10000),
    )?;
    read_required(&mut readbacks, tools, "act_combo", "steps")?;
    read_required(&mut readbacks, tools, "act_keymap", "alias")?;
    read_property(&mut readbacks, tools, "act_stroke", "path")?;
    read_property(&mut readbacks, tools, "act_stroke", "target")?;
    read_property(&mut readbacks, tools, "act_stroke", "from")?;
    read_property(&mut readbacks, tools, "act_stroke", "to")?;
    read_required(&mut readbacks, tools, "act_stroke", "duration_or_speed")?;
    read_required(&mut readbacks, tools, "act_run_shell", "command")?;
    read_required(&mut readbacks, tools, "act_run_shell_start", "command")?;
    read_required(&mut readbacks, tools, "act_run_shell_status", "job_id")?;
    read_required(&mut readbacks, tools, "act_run_shell_cancel", "job_id")?;
    read_required(&mut readbacks, tools, "act_launch", "target")?;
    read_required(&mut readbacks, tools, "notify_human", "title")?;
    read_required(&mut readbacks, tools, "notify_human", "body")?;
    read_required(&mut readbacks, tools, "notify_human", "kind")?;
    read_default(
        &mut readbacks,
        tools,
        "notify_human",
        "inputSchema.properties.suppress_popup.default",
        &json!(false),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.profile_id.default",
        &json!("everquest.live"),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.state_row_key.default",
        &json!("everquest/current_state/v1/everquest.live"),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.max_exits.default",
        &json!(5),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.max_landmarks.default",
        &json!(5),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.max_transitions.default",
        &json!(5),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.max_hazards.default",
        &json!(5),
    )?;
    read_default(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "inputSchema.properties.stale_after_seconds.default",
        &json!(300),
    )?;
    read_required(
        &mut readbacks,
        tools,
        "everquest_world_summary",
        "summary_id",
    )?;
    Ok(readbacks)
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
