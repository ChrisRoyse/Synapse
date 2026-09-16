use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use serde_json::Value;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

const MATRIX_DOC: &str = include_str!("../../../docs/multi-agent-capability-matrix.md");
const TOOL_PROFILES_SOURCE: &str = include_str!("../src/server/tool_profiles.rs");

// The set of action/perception tools the matrix must cover is DERIVED from the
// real built tool surface at run time (the same surface `m4_tools_list`
// snapshots), filtered to matrix scope by `is_matrix_scope_tool`. There is
// deliberately NO hardcoded `EXPECTED_MATRIX_TOOLS` array/count literal: a
// hand-maintained third copy of the surface was the single highest-friction
// recurring merge-conflict source (#1062) and a silent count-drift class
// (#953/#960/#963). Adding or removing a matrix-scope tool now updates exactly
// two places — the doc tables in this file's sibling markdown (whose row
// completeness this test enforces against the live surface) and the
// `m4_tools_list` insta snapshot — never a magic `[&str; N]` array. See #1062.

const ALLOWED_STATUS: [&str; 8] = [
    "background-pass",
    "conditional-pass",
    "foreground-capable-targeted",
    "foreground-lease",
    "gap-linked",
    "control",
    "diagnostic",
    "sessionless",
];

const ALLOWED_DEFAULT_EXPOSURE: [&str; 5] = [
    "normal_agent",
    "facade_only",
    "browser_debugger",
    "break_glass",
    "debug_only",
];
const YES_NO: [&str; 2] = ["yes", "no"];

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

#[derive(Debug)]
struct ExposureRow {
    tool: String,
    default_exposure: String,
    break_glass_only: String,
    hidden_internal: String,
    deprecated_alias: String,
    foreground_prone_wording: String,
    safe_replacement_tool: String,
}

#[tokio::test]
async fn multi_agent_capability_matrix_covers_action_perception_surface() -> anyhow::Result<()> {
    // Source of truth: the real built tool surface. Launch the wired client with
    // the full surface enabled, then derive the matrix-scope set from it instead
    // of from a hand-maintained literal (#1062).
    let mut client =
        StdioMcpClient::launch_and_init_with_env(None, &[("SYNAPSE_DEBUG_TOOLS", "1")]).await?;
    let response = client.tools_list().await?;
    let tools = response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let expected_tools = listed_matrix_scope_tools(tools)?;
    ensure!(
        !expected_tools.is_empty(),
        "live tool surface produced no matrix-scope tools — scope filter or surface is broken"
    );

    let rows = parse_matrix(MATRIX_DOC)?;
    let by_tool = rows_by_tool(&rows)?;
    let exposure_rows = parse_exposure_overlay(MATRIX_DOC)?;
    let exposure_by_tool = exposure_rows_by_tool(&exposure_rows)?;
    let matrix_tools = by_tool.keys().cloned().collect::<BTreeSet<_>>();
    ensure!(
        matrix_tools == expected_tools,
        "capability matrix doc drift vs live surface: missing rows for={:?} stale rows for={:?}",
        expected_tools.difference(&matrix_tools).collect::<Vec<_>>(),
        matrix_tools.difference(&expected_tools).collect::<Vec<_>>()
    );

    for row in &rows {
        assert_row_complete(row)?;
    }
    for row in &exposure_rows {
        assert_exposure_row_complete(row)?;
    }
    assert_exposure_overlay_matches_policy(&exposure_by_tool, &expected_tools)?;

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn parse_exposure_overlay(markdown: &str) -> anyhow::Result<Vec<ExposureRow>> {
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
        if cells.len() != 7 {
            continue;
        }
        if cells[0] == "Tool" && cells[1] == "Default exposure" {
            ensure!(
                cells
                    == [
                        "Tool",
                        "Default exposure",
                        "Break-glass only",
                        "Hidden/internal",
                        "Deprecated alias",
                        "Foreground-prone wording",
                        "Safe replacement tool"
                    ],
                "exposure overlay header changed: {cells:?}"
            );
            saw_header = true;
            continue;
        }
        if cells.iter().all(|cell| cell.chars().all(|ch| ch == '-')) {
            continue;
        }

        rows.push(ExposureRow {
            tool: cells[0].to_owned(),
            default_exposure: cells[1].to_owned(),
            break_glass_only: cells[2].to_owned(),
            hidden_internal: cells[3].to_owned(),
            deprecated_alias: cells[4].to_owned(),
            foreground_prone_wording: cells[5].to_owned(),
            safe_replacement_tool: cells[6].to_owned(),
        });
    }

    ensure!(
        saw_header,
        "model-selection exposure overlay header missing"
    );
    ensure!(
        !rows.is_empty(),
        "model-selection exposure overlay has no rows"
    );
    Ok(rows)
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

fn exposure_rows_by_tool(rows: &[ExposureRow]) -> anyhow::Result<BTreeMap<String, &ExposureRow>> {
    let mut by_tool = BTreeMap::new();
    for row in rows {
        ensure!(
            by_tool.insert(row.tool.clone(), row).is_none(),
            "duplicate exposure overlay row for {}",
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
    if matches!(
        row.status.as_str(),
        "background-pass" | "conditional-pass" | "foreground-capable-targeted"
    ) {
        ensure!(
            !row.background_path.to_ascii_lowercase().starts_with("none"),
            "{} pass row must name a real background path",
            row.tool
        );
    }
    Ok(())
}

fn assert_exposure_row_complete(row: &ExposureRow) -> anyhow::Result<()> {
    ensure!(!row.tool.is_empty(), "exposure row has empty tool");
    ensure!(
        ALLOWED_DEFAULT_EXPOSURE.contains(&row.default_exposure.as_str()),
        "{} has unsupported default exposure {}",
        row.tool,
        row.default_exposure
    );
    ensure!(
        YES_NO.contains(&row.break_glass_only.as_str()),
        "{} has unsupported break-glass-only value {}",
        row.tool,
        row.break_glass_only
    );
    ensure!(
        YES_NO.contains(&row.hidden_internal.as_str()),
        "{} has unsupported hidden/internal value {}",
        row.tool,
        row.hidden_internal
    );
    ensure!(
        YES_NO.contains(&row.deprecated_alias.as_str()),
        "{} has unsupported deprecated-alias value {}",
        row.tool,
        row.deprecated_alias
    );
    ensure!(
        YES_NO.contains(&row.foreground_prone_wording.as_str()),
        "{} has unsupported foreground-prone value {}",
        row.tool,
        row.foreground_prone_wording
    );
    ensure!(
        !row.safe_replacement_tool.is_empty(),
        "{} safe replacement missing",
        row.tool
    );
    if row.break_glass_only == "yes" {
        ensure!(
            row.default_exposure != "normal_agent",
            "{} cannot be both default normal_agent and break-glass-only",
            row.tool
        );
        ensure!(
            row.default_exposure != "facade_only",
            "{} cannot be both facade_only and break-glass-only",
            row.tool
        );
    }
    if row.default_exposure == "facade_only" {
        ensure!(
            row.hidden_internal == "yes",
            "{} facade_only rows must mark the raw implementation name hidden/internal",
            row.tool
        );
        ensure!(
            !row.safe_replacement_tool.starts_with("none"),
            "{} facade_only rows must name the public facade route",
            row.tool
        );
    }
    if row.foreground_prone_wording == "yes" {
        ensure!(
            !row.safe_replacement_tool.starts_with("none"),
            "{} foreground-prone row must name a safe replacement tool or issue",
            row.tool
        );
    }
    Ok(())
}

fn assert_exposure_overlay_matches_policy(
    exposure_by_tool: &BTreeMap<String, &ExposureRow>,
    expected_tools: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let exposure_tools = exposure_by_tool.keys().cloned().collect::<BTreeSet<_>>();
    ensure!(
        exposure_tools == *expected_tools,
        "exposure overlay tool set drift: missing={:?} extra={:?}",
        expected_tools
            .difference(&exposure_tools)
            .collect::<Vec<_>>(),
        exposure_tools
            .difference(expected_tools)
            .collect::<Vec<_>>()
    );

    let public_tools = parse_string_array_const(TOOL_PROFILES_SOURCE, "PUBLIC_TOOL_NAMES")?;
    let normal_exact = parse_string_array_const(TOOL_PROFILES_SOURCE, "NORMAL_ALLOWED_EXACT")?;
    let normal_prefixes =
        parse_string_array_const_allow_empty(TOOL_PROFILES_SOURCE, "NORMAL_ALLOWED_PREFIXES")?;
    let browser_debugger_only =
        parse_string_array_const(TOOL_PROFILES_SOURCE, "BROWSER_DEBUGGER_ONLY_EXACT")?;
    let break_glass_hazards =
        parse_string_array_const(TOOL_PROFILES_SOURCE, "BREAK_GLASS_HAZARDOUS_TOOLS")?;

    for tool in expected_tools {
        let row = exposure_by_tool
            .get(tool)
            .with_context(|| format!("exposure row missing for {tool}"))?;
        let visible_in_normal = normal_exact.contains(tool)
            || normal_prefixes
                .iter()
                .any(|prefix| tool.starts_with(prefix.as_str()));
        if visible_in_normal {
            ensure!(
                row.default_exposure == "normal_agent",
                "{} is visible in normal_agent policy but matrix says {}",
                tool,
                row.default_exposure
            );
            ensure!(
                row.break_glass_only == "no",
                "{tool} visible in normal_agent cannot be break-glass-only"
            );
        } else if tool.starts_with("action_diagnostic_") {
            ensure!(
                row.default_exposure == "debug_only" && row.hidden_internal == "yes",
                "{tool} diagnostic tool must be debug_only hidden/internal"
            );
        } else if browser_debugger_only.contains(tool) {
            ensure!(
                row.default_exposure == "browser_debugger",
                "{} hidden browser debugger tool must be classified browser_debugger, got {}",
                tool,
                row.default_exposure
            );
            ensure!(
                row.break_glass_only == "no",
                "{tool} browser_debugger tool must not be classified break-glass-only"
            );
        } else if public_tools.contains(tool) {
            ensure!(
                row.default_exposure == "browser_debugger",
                "{} public profile-gated tool must be classified browser_debugger, got {}",
                tool,
                row.default_exposure
            );
            ensure!(
                row.break_glass_only == "no",
                "{tool} public profile-gated tool must not be classified break-glass-only"
            );
        } else if break_glass_hazards.contains(tool) {
            ensure!(
                row.default_exposure == "break_glass",
                "{} break-glass hazard must be classified break_glass, got {}",
                tool,
                row.default_exposure
            );
            ensure!(
                row.break_glass_only == "yes",
                "{tool} break-glass hazard must be break-glass-only"
            );
        } else {
            ensure!(
                matches!(row.default_exposure.as_str(), "facade_only" | "break_glass"),
                "{} hidden implementation tool must be classified facade_only, browser_debugger, break_glass, or debug_only; got {}",
                tool,
                row.default_exposure
            );
            if row.default_exposure == "facade_only" {
                ensure!(
                    row.break_glass_only == "no",
                    "{tool} facade-routed implementation tool must not be break-glass-only"
                );
                ensure!(
                    row.hidden_internal == "yes",
                    "{tool} facade-routed implementation tool must mark the raw name hidden/internal"
                );
                ensure!(
                    !row.safe_replacement_tool.starts_with("none"),
                    "{tool} facade-routed implementation tool must name the public facade route"
                );
            } else {
                ensure!(
                    row.break_glass_only == "yes",
                    "{tool} break-glass hidden implementation tool must be break-glass-only"
                );
            }
        }

        if break_glass_hazards.contains(tool) && !tool.starts_with("action_diagnostic_") {
            ensure!(
                row.default_exposure == "break_glass",
                "{} is in BREAK_GLASS_HAZARDOUS_TOOLS but matrix says {}",
                tool,
                row.default_exposure
            );
        }
    }

    assert_representative_exposure(exposure_by_tool, "act_type", "break_glass", "yes", "yes")?;
    assert_representative_exposure(exposure_by_tool, "act_click", "break_glass", "yes", "yes")?;
    assert_representative_exposure(
        exposure_by_tool,
        "act_focus_window",
        "break_glass",
        "yes",
        "yes",
    )?;
    assert_representative_exposure(exposure_by_tool, "release_all", "break_glass", "yes", "yes")?;
    assert_representative_exposure(
        exposure_by_tool,
        "browser_console_messages",
        "browser_debugger",
        "no",
        "yes",
    )?;
    assert_representative_exposure(
        exposure_by_tool,
        "browser_evaluate",
        "browser_debugger",
        "no",
        "yes",
    )?;
    assert_representative_exposure(
        exposure_by_tool,
        "cdp_navigate_tab",
        "facade_only",
        "no",
        "no",
    )?;
    assert_representative_exposure(
        exposure_by_tool,
        "cdp_target_info",
        "facade_only",
        "no",
        "no",
    )?;
    assert_representative_exposure(exposure_by_tool, "set_target", "facade_only", "no", "no")?;
    assert_representative_exposure(
        exposure_by_tool,
        "tool_profile_status",
        "facade_only",
        "no",
        "no",
    )?;
    assert_representative_exposure(
        exposure_by_tool,
        "action_diagnostic_queue_full_setup",
        "debug_only",
        "yes",
        "no",
    )?;
    Ok(())
}

fn assert_representative_exposure(
    exposure_by_tool: &BTreeMap<String, &ExposureRow>,
    tool: &str,
    default_exposure: &str,
    break_glass_only: &str,
    foreground_prone_wording: &str,
) -> anyhow::Result<()> {
    let row = exposure_by_tool
        .get(tool)
        .with_context(|| format!("representative exposure row missing for {tool}"))?;
    ensure!(
        row.default_exposure == default_exposure,
        "{} default exposure expected {}, got {}",
        tool,
        default_exposure,
        row.default_exposure
    );
    ensure!(
        row.break_glass_only == break_glass_only,
        "{} break-glass-only expected {}, got {}",
        tool,
        break_glass_only,
        row.break_glass_only
    );
    ensure!(
        row.foreground_prone_wording == foreground_prone_wording,
        "{} foreground-prone expected {}, got {}",
        tool,
        foreground_prone_wording,
        row.foreground_prone_wording
    );
    Ok(())
}

fn parse_string_array_const(source: &str, const_name: &str) -> anyhow::Result<BTreeSet<String>> {
    let values = parse_string_array_const_allow_empty(source, const_name)?;
    ensure!(!values.is_empty(), "{const_name} parsed as empty");
    Ok(values)
}

fn parse_string_array_const_allow_empty(
    source: &str,
    const_name: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let marker = format!("const {const_name}: &[&str] = &[");
    let Some(start) = source.find(&marker) else {
        // Alias form `const X: &[&str] = OTHER_CONST;` — resolve the alias to the
        // const it points at. tool_profiles.rs aliases several policy consts to
        // PUBLIC_TOOL_NAMES; the inline-array-only parser silently broke when
        // that landed under [skip ci], disabling this whole gate. Following the
        // alias keeps the gate live regardless of alias vs inline authoring.
        let alias_marker = format!("const {const_name}: &[&str] = ");
        let alias_start = source
            .find(&alias_marker)
            .with_context(|| format!("{const_name} const missing"))?;
        let after_alias = &source[alias_start + alias_marker.len()..];
        let end = after_alias
            .find(';')
            .with_context(|| format!("{const_name} alias terminator missing"))?;
        let target = after_alias[..end].trim();
        ensure!(
            !target.is_empty()
                && target
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'),
            "{const_name} alias RHS is not a const identifier: {target:?}"
        );
        return parse_string_array_const_allow_empty(source, target);
    };
    let after_start = &source[start + marker.len()..];
    let end = after_start
        .find("];")
        .with_context(|| format!("{const_name} const terminator missing"))?;
    let body = &after_start[..end];
    let mut values = BTreeSet::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('"') {
            continue;
        }
        let value = trimmed
            .trim_start_matches('"')
            .split('"')
            .next()
            .with_context(|| format!("{const_name} malformed value: {trimmed}"))?;
        values.insert(value.to_owned());
    }
    Ok(values)
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
        || name.starts_with("browser_")
        || name.starts_with("cdp_")
        || name.starts_with("control_lease_")
        || name.starts_with("demo_record_")
        || name.starts_with("fleet_")
        || name.starts_with("reflex_")
        || name.starts_with("target_")
        || name.starts_with("tool_profile_")
        || name.starts_with("verification_")
        || name.starts_with("workspace_")
        || matches!(
            name,
            "capture_gif"
                | "capture_screenshot"
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
                | "window_list"
        )
}
