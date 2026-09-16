use std::{fs, path::Path};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "integration test keeps profile list/activate/report assertions in one flow"
)]
async fn profile_tools_list_activate_and_report_health() -> anyhow::Result<()> {
    let profiles = TempDir::new()?;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path = db_path.to_string_lossy().to_string();
    write_profile(
        &profiles.path().join("alpha.toml"),
        "alpha",
        "Alpha",
        "alpha.exe",
        None,
    )?;
    write_profile(
        &profiles.path().join("beta.toml"),
        "beta",
        "Beta",
        "beta.exe",
        Some("hardware"),
    )?;

    let profile_dir = profiles.path().to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_PROFILE_DIR", profile_dir.as_str()),
            ("SYNAPSE_DB", db_path.as_str()),
        ],
    )
    .await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    assert!(tools.iter().any(|tool| tool["name"] == "profile_list"));
    assert!(tools.iter().any(|tool| tool["name"] == "profile_activate"));
    assert_profile_tools_schema(tools);

    let response = client.tools_call("profile_list", json!({})).await?;
    let list = structured(&response)?;
    assert_initial_profile_list(&list)?;

    let response = client.tools_call("health", json!({})).await?;
    let health = structured(&response)?;
    assert_backend_resolution(
        &health,
        BackendResolutionExpectation {
            source: "global_default",
            default_backend: None,
            keyboard_auto: "software",
            mouse_auto: None,
            pad_auto: "vigem",
        },
    );

    activate_beta_profile(&mut client).await?;

    let response = client.tools_call("health", json!({})).await?;
    let health = structured(&response)?;
    assert_eq!(
        health["subsystems"]["profiles"]["active_profile_id"],
        "beta"
    );
    assert_eq!(
        health["subsystems"]["perception"]["perception_mode"],
        "hybrid"
    );
    assert_eq!(
        health["subsystems"]["perception"]["capture_config"]["target"]["kind"],
        "foreground_window"
    );
    assert_eq!(
        health["subsystems"]["perception"]["capture_config"]["source"],
        "profile:beta"
    );
    assert_backend_resolution(
        &health,
        BackendResolutionExpectation {
            source: "profile:beta",
            default_backend: Some("hardware"),
            keyboard_auto: "hardware",
            mouse_auto: Some("hardware"),
            pad_auto: "hardware",
        },
    );

    let error = client
        .tools_call_error("profile_activate", json!({"profile_id": "missing"}))
        .await?;
    assert_eq!(error["data"]["code"], "PROFILE_NOT_FOUND");

    let response = client.tools_call("health", json!({})).await?;
    let health = structured(&response)?;
    assert_eq!(
        health["subsystems"]["profiles"]["active_profile_id"],
        "beta"
    );
    assert_backend_resolution(
        &health,
        BackendResolutionExpectation {
            source: "profile:beta",
            default_backend: Some("hardware"),
            keyboard_auto: "hardware",
            mouse_auto: Some("hardware"),
            pad_auto: "hardware",
        },
    );

    let status = client.shutdown().await?;
    assert!(status.success());
    let logs_text = read_logs(logs.path())?;
    assert!(logs_text.contains("PROFILE_ACTIVATED"));
    assert!(logs_text.contains("beta"));
    observe_reports_matching_profile_without_manual_activation().await?;
    Ok(())
}

async fn observe_reports_matching_profile_without_manual_activation() -> anyhow::Result<()> {
    let profiles = TempDir::new()?;
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path = db_path.to_string_lossy().to_string();
    write_profile_with_runtime(
        &profiles.path().join("synthetic-notepad.toml"),
        "synthetic-notepad",
        "Synthetic Notepad",
        "notepad.exe",
        "pixel_only",
        33,
        false,
        None,
    )?;

    let profile_dir = profiles.path().to_string_lossy().to_string();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_PROFILE_DIR", profile_dir.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
            ("SYNAPSE_DB", db_path.as_str()),
        ],
    )
    .await?;

    let response = client.tools_call("observe", json!({})).await?;
    let observation = structured(&response)?;
    assert_eq!(observation["foreground"]["profile_id"], "synthetic-notepad");
    assert_eq!(observation["foreground"]["process_name"], "notepad.exe");
    assert_eq!(observation["mode"], "pixel_only");
    assert_eq!(
        observation["diagnostics"]["capture_config"]["target"]["kind"],
        "foreground_window"
    );
    assert_eq!(
        observation["diagnostics"]["capture_config"]["min_update_interval_ms"],
        33
    );
    assert_eq!(
        observation["diagnostics"]["capture_config"]["cursor_visible"],
        false
    );
    assert_eq!(
        observation["diagnostics"]["capture_config"]["source"],
        "profile:synthetic-notepad"
    );

    let response = client.tools_call("health", json!({})).await?;
    let health = structured(&response)?;
    assert_eq!(
        health["subsystems"]["profiles"]["active_profile_id"],
        "synthetic-notepad"
    );
    assert_eq!(health["subsystems"]["profiles"]["profile_count"], 1);

    let status = client.shutdown().await?;
    assert!(status.success());
    let logs_text = read_logs(logs.path())?;
    assert!(logs_text.contains("PROFILE_FOREGROUND_MATCHED"));
    assert!(logs_text.contains("synthetic-notepad"));
    Ok(())
}

fn assert_initial_profile_list(list: &Value) -> anyhow::Result<()> {
    let listed = list["profiles"]
        .as_array()
        .context("profile_list did not return profiles array")?;
    assert_eq!(listed.len(), 2);
    assert_eq!(list["active_profile_id"], Value::Null);
    let alpha = listed
        .iter()
        .find(|profile| profile["id"] == "alpha")
        .context("alpha profile missing")?;
    let beta = listed
        .iter()
        .find(|profile| profile["id"] == "beta")
        .context("beta profile missing")?;
    assert_eq!(alpha["use_scope"], "productivity");
    assert_eq!(alpha["detection_model_id"], "alpha_detector");
    assert_eq!(alpha["metadata"]["test_profile_id"], "alpha");
    assert_eq!(beta["use_scope"], "productivity");
    assert_eq!(beta["detection_model_id"], "beta_detector");
    assert_eq!(beta["metadata"]["test_profile_id"], "beta");
    assert!(
        listed
            .iter()
            .all(|profile| profile["active"].as_bool() == Some(false))
    );
    Ok(())
}

async fn activate_beta_profile(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let response = client
        .tools_call("profile_activate", json!({"profile_id": "beta"}))
        .await?;
    let activate = structured(&response)?;
    assert_eq!(activate["active_profile_id"], "beta");
    assert_eq!(activate["changed"], true);

    let response = client
        .tools_call("profile_list", json!({"include_inactive": false}))
        .await?;
    let active_only = structured(&response)?;
    let active_profiles = active_only["profiles"]
        .as_array()
        .context("active-only profile_list did not return profiles array")?;
    assert_eq!(active_profiles.len(), 1);
    assert_eq!(active_profiles[0]["id"], "beta");
    assert_eq!(active_profiles[0]["active"], true);

    let response = client
        .tools_call("profile_activate", json!({"profile_id": "beta"}))
        .await?;
    let noop = structured(&response)?;
    assert_eq!(noop["active_profile_id"], "beta");
    assert_eq!(noop["previous_active_profile_id"], "beta");
    assert_eq!(noop["changed"], false);
    Ok(())
}

#[derive(Copy, Clone)]
struct BackendResolutionExpectation<'a> {
    source: &'a str,
    default_backend: Option<&'a str>,
    keyboard_auto: &'a str,
    mouse_auto: Option<&'a str>,
    pad_auto: &'a str,
}

fn assert_backend_resolution(health: &Value, expected: BackendResolutionExpectation<'_>) {
    let resolution = &health["subsystems"]["action"]["backend_resolution"];
    assert_eq!(resolution["source"], expected.source);
    if let Some(default_backend) = expected.default_backend {
        assert_eq!(resolution["default_backend"], default_backend);
    }
    assert_eq!(resolution["keyboard_auto"], expected.keyboard_auto);
    if let Some(mouse_auto) = expected.mouse_auto {
        assert_eq!(resolution["mouse_auto"], mouse_auto);
    }
    assert_eq!(resolution["pad_auto"], expected.pad_auto);
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

fn assert_profile_tools_schema(tools: &[Value]) {
    let mut profile_tool_shapes = tools
        .iter()
        .filter(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("profile_"))
        })
        .map(|tool| {
            json!({
                "name": tool.get("name").cloned().unwrap_or(Value::Null),
                "inputSchema": tool.get("inputSchema").cloned().unwrap_or(Value::Null),
                "outputSchema": tool.get("outputSchema").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    profile_tool_shapes.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    insta::assert_json_snapshot!("m3_profile_tools", profile_tool_shapes);
}

fn write_profile(
    path: &Path,
    id: &str,
    label: &str,
    exe: &str,
    default_backend: Option<&str>,
) -> anyhow::Result<()> {
    write_profile_with_runtime(path, id, label, exe, "hybrid", 16, true, default_backend)
}

#[allow(clippy::too_many_arguments)]
fn write_profile_with_runtime(
    path: &Path,
    id: &str,
    label: &str,
    exe: &str,
    mode: &str,
    capture_interval_ms: u32,
    cursor_visible: bool,
    default_backend: Option<&str>,
) -> anyhow::Result<()> {
    let backends = default_backend.map_or(String::new(), |backend| {
        format!(
            r#"
[backends]
default_backend = "{backend}"
"#
        )
    });
    fs::write(
        path,
        format!(
            r#"
id = "{id}"
label = "{label}"
schema_version = 2
use_scope = "productivity"
mode = "{mode}"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "{exe}"

[capture]
target = "foreground_window"
min_update_interval_ms = {capture_interval_ms}
cursor_visible = {cursor_visible}

[detection]
model_id = "{id}_detector"
classes_of_interest = ["window", "text"]
confidence_threshold = 0.50
max_detections = 8

[metadata]
test_profile_id = "{id}"
{backends}
"#
        ),
    )?;
    Ok(())
}

fn read_logs(path: &Path) -> anyhow::Result<String> {
    let mut logs = String::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            logs.push_str(&std::fs::read_to_string(entry.path())?);
        }
    }
    Ok(logs)
}
