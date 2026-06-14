use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use serde_json::Value;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

const MATRIX_DOC: &str = include_str!("../../../docs/multi-agent-capability-matrix.md");

const EXPECTED_MATRIX_TOOLS: [&str; 73] = [
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
    "capture_screenshot",
    "cdp_close_tab",
    "cdp_navigate_tab",
    "cdp_open_tab",
    "clear_target",
    "control_lease_acquire",
    "control_lease_handoff",
    "control_lease_release",
    "control_lease_status",
    "find",
    "get_target",
    "hidden_desktop_pip_frame",
    "observe",
    "observe_delta",
    "read_text",
    "reality_audit",
    "reality_baseline",
    "reflex_cancel",
    "reflex_history",
    "reflex_list",
    "reflex_register",
    "release_all",
    "session_list",
    "session_status",
    "set_capture_target",
    "set_perception_mode",
    "set_target",
    "subscribe",
    "subscribe_cancel",
    "target_claim",
    "target_claim_adopt",
    "target_claim_status",
    "target_release",
    "workspace_get",
    "workspace_list",
    "workspace_put",
    "workspace_subscribe",
];

const ALLOWED_STATUS: [&str; 7] = [
    "background-pass",
    "conditional-pass",
    "foreground-lease",
    "gap-linked",
    "control",
    "diagnostic",
    "sessionless",
];

#[derive(Debug)]
struct MatrixRow {
    tool: String,
    class: String,
    target_source: String,
    background_path: String,
    lease_policy: String,
    status: String,
    follow_up: String,
    manual_source_of_truth: String,
}

#[tokio::test]
async fn multi_agent_capability_matrix_covers_action_perception_surface() -> anyhow::Result<()> {
    let rows = parse_matrix(MATRIX_DOC)?;
    let by_tool = rows_by_tool(&rows)?;
    let expected_tools = expected_tool_set();
    let matrix_tools = by_tool.keys().cloned().collect::<BTreeSet<_>>();
    ensure!(
        matrix_tools == expected_tools,
        "matrix tool set drift: missing={:?} extra={:?}",
        expected_tools.difference(&matrix_tools).collect::<Vec<_>>(),
        matrix_tools.difference(&expected_tools).collect::<Vec<_>>()
    );

    for row in &rows {
        assert_row_complete(row)?;
    }

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
    let listed_matrix_scope = listed_matrix_scope_tools(tools)?;
    ensure!(
        listed_matrix_scope == expected_tools,
        "tools/list matrix-scope drift: missing={:?} extra={:?}",
        expected_tools
            .difference(&listed_matrix_scope)
            .collect::<Vec<_>>(),
        listed_matrix_scope
            .difference(&expected_tools)
            .collect::<Vec<_>>()
    );

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn parse_matrix(markdown: &str) -> anyhow::Result<Vec<MatrixRow>> {
    let mut rows = Vec::new();
    let mut saw_header = false;

    for line in markdown.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
            continue;
        }

        let cells = trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect::<Vec<_>>();
        if cells.len() != 8 {
            continue;
        }
        if cells[0] == "Tool" {
            ensure!(
                cells
                    == [
                        "Tool",
                        "Class",
                        "Target source",
                        "Background path",
                        "Lease policy",
                        "Status",
                        "Follow-up",
                        "Manual source of truth"
                    ],
                "capability matrix header changed: {cells:?}"
            );
            saw_header = true;
            continue;
        }
        if cells.iter().all(|cell| cell.chars().all(|ch| ch == '-')) {
            continue;
        }

        rows.push(MatrixRow {
            tool: cells[0].to_owned(),
            class: cells[1].to_owned(),
            target_source: cells[2].to_owned(),
            background_path: cells[3].to_owned(),
            lease_policy: cells[4].to_owned(),
            status: cells[5].to_owned(),
            follow_up: cells[6].to_owned(),
            manual_source_of_truth: cells[7].to_owned(),
        });
    }

    ensure!(saw_header, "capability matrix table header missing");
    ensure!(!rows.is_empty(), "capability matrix has no rows");
    Ok(rows)
}

fn rows_by_tool(rows: &[MatrixRow]) -> anyhow::Result<BTreeMap<String, &MatrixRow>> {
    let mut by_tool = BTreeMap::new();
    for row in rows {
        ensure!(
            by_tool.insert(row.tool.clone(), row).is_none(),
            "duplicate capability matrix row for {}",
            row.tool
        );
    }
    Ok(by_tool)
}

fn assert_row_complete(row: &MatrixRow) -> anyhow::Result<()> {
    ensure!(!row.tool.is_empty(), "matrix row has empty tool");
    ensure!(!row.class.is_empty(), "{} class missing", row.tool);
    ensure!(
        !row.target_source.is_empty(),
        "{} target source missing",
        row.tool
    );
    ensure!(
        !row.background_path.is_empty(),
        "{} background path missing",
        row.tool
    );
    ensure!(
        !row.lease_policy.is_empty(),
        "{} lease policy missing",
        row.tool
    );
    ensure!(
        ALLOWED_STATUS.contains(&row.status.as_str()),
        "{} has unsupported status {}",
        row.tool,
        row.status
    );
    ensure!(!row.follow_up.is_empty(), "{} follow-up missing", row.tool);
    ensure!(
        !row.manual_source_of_truth.is_empty(),
        "{} manual source of truth missing",
        row.tool
    );
    if row.status == "gap-linked" {
        ensure!(
            row.follow_up.contains('#'),
            "{} gap-linked row must name a GitHub issue",
            row.tool
        );
    }
    if row.status == "foreground-lease" {
        ensure!(
            row.lease_policy.to_ascii_lowercase().contains("lease"),
            "{} foreground-lease row must explicitly name the lease",
            row.tool
        );
    }
    if matches!(row.status.as_str(), "background-pass" | "conditional-pass") {
        ensure!(
            !row.background_path.to_ascii_lowercase().starts_with("none"),
            "{} pass row must name a real background path",
            row.tool
        );
    }
    Ok(())
}

fn listed_matrix_scope_tools(tools: &[Value]) -> anyhow::Result<BTreeSet<String>> {
    let mut scoped = BTreeSet::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .context("tool name missing")?;
        if is_matrix_scope_tool(name) {
            scoped.insert(name.to_owned());
        }
    }
    Ok(scoped)
}

fn is_matrix_scope_tool(name: &str) -> bool {
    name.starts_with("act_")
        || name.starts_with("action_diagnostic_")
        || name.starts_with("agent_")
        || name.starts_with("audio_")
        || name.starts_with("cdp_")
        || name.starts_with("control_lease_")
        || name.starts_with("reflex_")
        || name.starts_with("target_")
        || name.starts_with("workspace_")
        || matches!(
            name,
            "capture_screenshot"
                | "clear_target"
                | "find"
                | "get_target"
                | "hidden_desktop_pip_frame"
                | "observe"
                | "observe_delta"
                | "read_text"
                | "reality_audit"
                | "reality_baseline"
                | "release_all"
                | "session_list"
                | "session_status"
                | "set_capture_target"
                | "set_perception_mode"
                | "set_target"
                | "subscribe"
                | "subscribe_cancel"
        )
}

fn expected_tool_set() -> BTreeSet<String> {
    EXPECTED_MATRIX_TOOLS
        .iter()
        .copied()
        .map(str::to_owned)
        .collect()
}
