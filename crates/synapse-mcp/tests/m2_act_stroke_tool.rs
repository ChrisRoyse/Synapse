use std::path::Path;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use synapse_core::{SCHEMA_VERSION, error_codes};
use synapse_storage::{Db, cf, decode_json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
#[allow(
    clippy::float_cmp,
    clippy::too_many_lines,
    reason = "integration test asserts exact synthetic stroke values across one MCP flow"
)]
async fn act_stroke_tools_call_recording_backend_and_path_edges() -> anyhow::Result<()> {
    let log_dir = TempDir::new()?;
    let db_dir = TempDir::new()?;
    let db_path_string = db_dir.path().to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(log_dir.path()),
        &[
            ("SYNAPSE_DB", db_path_string.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
            ("SYNAPSE_MCP_RECORDING_BACKEND", "1"),
        ],
    )
    .await?;
    activate_notepad_profile(&mut client).await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    assert!(tools.iter().any(|tool| tool["name"] == "act_stroke"));
    assert!(!tools.iter().any(|tool| tool["name"] == "act_aim"));
    assert!(!tools.iter().any(|tool| tool["name"] == "act_drag"));

    let response = client
        .tools_call(
            "act_stroke",
            json!({
                "path": {
                    "kind": "line",
                    "from": {"x": 0.0, "y": 0.0},
                    "to": {"x": 4.0, "y": 0.0}
                },
                "button": "left",
                "velocity_profile": "constant",
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 4},
                "backend": "software"
            }),
        )
        .await?;
    let stroke: ActStrokeWireResponse = structured(&response)?;
    println!(
        "readback=mcp_act_stroke edge=line after=ok:{} path_kind:{} points:{} path_length:{} duration_ms:{} backend:{}",
        stroke.ok,
        stroke.path_kind,
        stroke.point_stream_count,
        stroke.path_length_px,
        stroke.duration_ms,
        stroke.backend_used
    );
    assert!(stroke.ok);
    assert_eq!(stroke.path_kind, "line");
    assert_eq!(stroke.point_stream_count, 5);
    assert_eq!(stroke.path_length_px, 4.0);
    assert_eq!(stroke.duration_ms, 4.0);
    assert_eq!(stroke.motion_model_used, json!({"kind": "path"}));
    assert_eq!(stroke.backend_used, "software");

    let target_line_response = client
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
        .await?;
    let target_line: ActStrokeWireResponse = structured(&target_line_response)?;
    println!(
        "readback=mcp_act_stroke edge=target_line after=ok:{} path_kind:{} button:{:?} points:{} duration_ms:{}",
        target_line.ok,
        target_line.path_kind,
        target_line.button_used,
        target_line.point_stream_count,
        target_line.duration_ms
    );
    assert!(target_line.ok);
    assert_eq!(target_line.path_kind, "line");
    assert_eq!(target_line.button_used.as_deref(), Some("left"));
    assert!(target_line.point_stream_count > 1);
    assert_eq!(target_line.duration_ms, 80.0);

    let wind_response = client
        .tools_call(
            "act_stroke",
            json!({
                "path": {
                    "kind": "line",
                    "from": {"x": 0.0, "y": 0.0},
                    "to": {"x": 120.0, "y": 0.0}
                },
                "velocity_profile": "constant",
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 120},
                "motion_model": {
                    "kind": "wind_mouse",
                    "gravity": 9.0,
                    "wind": 3.0,
                    "max_step": 10.0,
                    "damped_distance": 12.0,
                    "seed": 42
                },
                "backend": "software"
            }),
        )
        .await?;
    let wind_stroke: ActStrokeWireResponse = structured(&wind_response)?;
    println!(
        "readback=mcp_act_stroke edge=wind_mouse after=ok:{} path_kind:{} points:{} motion_model:{}",
        wind_stroke.ok,
        wind_stroke.path_kind,
        wind_stroke.point_stream_count,
        wind_stroke.motion_model_used
    );
    assert!(wind_stroke.ok);
    assert_eq!(wind_stroke.path_kind, "line");
    assert!(wind_stroke.point_stream_count > 2);
    assert_eq!(
        wind_stroke.motion_model_used,
        json!({
            "kind": "wind_mouse",
            "gravity": 9.0,
            "wind": 3.0,
            "max_step": 10.0,
            "damped_distance": 12.0,
            "seed": 42
        })
    );

    let wind_circle = client
        .tools_call_error(
            "act_stroke",
            json!({
                "path": {
                    "kind": "circle",
                    "center": {"x": 0.0, "y": 0.0},
                    "radius": 10.0
                },
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 120},
                "motion_model": {
                    "kind": "wind_mouse",
                    "gravity": 9.0,
                    "wind": 3.0,
                    "max_step": 10.0,
                    "damped_distance": 12.0
                }
            }),
        )
        .await?;
    println!("readback=mcp_act_stroke edge=wind_mouse_circle after_error={wind_circle}");
    assert_eq!(
        error_code(&wind_circle),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    let one_point = client
        .tools_call_error(
            "act_stroke",
            json!({
                "path": {
                    "kind": "polyline",
                    "points": [{"x": 1.0, "y": 1.0}]
                },
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 4}
            }),
        )
        .await?;
    println!("readback=mcp_act_stroke edge=one_point after_error={one_point}");
    assert_eq!(
        error_code(&one_point),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    let over_cap_points = (0_u32..10_000)
        .map(|index| json!({"x": f64::from(index), "y": 0.0}))
        .collect::<Vec<_>>();
    let over_cap = client
        .tools_call_error(
            "act_stroke",
            json!({
                "path": {
                    "kind": "polyline",
                    "points": over_cap_points
                },
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 4}
            }),
        )
        .await?;
    println!("readback=mcp_act_stroke edge=over_cap after_error={over_cap}");
    assert_eq!(
        error_code(&over_cap),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    assert!(client.shutdown().await?.success());
    let logs = read_logs(log_dir.path())?;
    let contains_recording_readback = logs.contains("M2_ACT_STROKE_RECORDING_READBACK")
        && logs.contains("readback=recording_backend tool=act_stroke");
    println!(
        "readback=daemon_log edge=act_stroke after_bytes={} contains_recording_readback={contains_recording_readback}",
        logs.len()
    );
    assert!(contains_recording_readback);

    Ok(())
}

#[tokio::test]
async fn act_stroke_validation_errors_are_durably_audited() -> anyhow::Result<()> {
    let log_dir = TempDir::new()?;
    let db_dir = TempDir::new()?;
    let db_path = db_dir.path().join("db");
    let db_path_string = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(log_dir.path()),
        &[
            ("SYNAPSE_DB", db_path_string.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
            ("SYNAPSE_MCP_RECORDING_BACKEND", "1"),
        ],
    )
    .await?;
    activate_notepad_profile(&mut client).await?;

    let before: Value = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    let before_action_rows = before["cf_row_counts"][cf::CF_ACTION_LOG]
        .as_u64()
        .context("before CF_ACTION_LOG count missing")?;
    println!(
        "readback=act_stroke_validation_audit edge=before before_action_rows={before_action_rows}"
    );

    let missing_target = client
        .tools_call_error(
            "act_stroke",
            json!({
                "duration_or_speed": {"kind": "duration_ms", "duration_ms": 100},
                "velocity_profile": "linear",
                "motion_model": {"kind": "path"},
                "backend": "software"
            }),
        )
        .await?;
    println!(
        "readback=act_stroke_validation_audit edge=missing_target after_error={missing_target}"
    );
    assert_eq!(
        error_code(&missing_target),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );

    let after: Value = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    let after_action_rows = after["cf_row_counts"][cf::CF_ACTION_LOG]
        .as_u64()
        .context("after CF_ACTION_LOG count missing")?;
    println!(
        "readback=act_stroke_validation_audit edge=after_mcp before_action_rows={before_action_rows} after_action_rows={after_action_rows}"
    );
    assert_eq!(after_action_rows, before_action_rows + 1);

    assert!(client.shutdown().await?.success());

    let row = latest_action_log_row(&db_path)?;
    println!(
        "readback=act_stroke_validation_audit edge=after_shutdown_source_of_truth tool={} status={} error_code={:?} detail_code={:?} validated={:?} fallback={:?}",
        row["tool"].as_str().unwrap_or("<missing>"),
        row["status"].as_str().unwrap_or("<missing>"),
        row["error_code"].as_str(),
        row.pointer("/details/request/failure/data/detail_code")
            .and_then(Value::as_str),
        row.pointer("/details/request/stroke/validated"),
        row.pointer("/details/request/stroke/fallback_path_executed")
    );
    assert_eq!(row["tool"], "act_stroke");
    assert_eq!(row["status"], "error");
    assert_eq!(row["error_code"], error_codes::TOOL_PARAMS_INVALID);
    assert_eq!(
        row.pointer("/details/request/failure/data/detail_code")
            .and_then(Value::as_str),
        Some("STROKE_TARGET_MISSING")
    );
    assert_eq!(
        row.pointer("/details/request/stroke/validation_stage")
            .and_then(Value::as_str),
        Some("params")
    );
    assert_eq!(
        row.pointer("/details/request/stroke/validated")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        row.pointer("/details/request/stroke/input_kind")
            .and_then(Value::as_str),
        Some("missing")
    );
    assert_eq!(
        row.pointer("/details/request/stroke/fallback_path_executed")
            .and_then(Value::as_bool),
        Some(false)
    );

    Ok(())
}

async fn activate_notepad_profile(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    client
        .tools_call("profile_activate", json!({"profile_id": "notepad"}))
        .await?;
    Ok(())
}

fn structured<T: DeserializeOwned>(resp: &Value) -> anyhow::Result<T> {
    serde_json::from_value(resp["structuredContent"].clone()).context("decode structuredContent")
}

fn error_code(error: &Value) -> Option<&str> {
    error
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
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

fn latest_action_log_row(db_path: &Path) -> anyhow::Result<Value> {
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    let mut rows = db.scan_cf(cf::CF_ACTION_LOG)?;
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let (_key, value) = rows.pop().context("CF_ACTION_LOG row missing")?;
    decode_json::<Value>(&value).map_err(Into::into)
}

#[derive(serde::Deserialize)]
struct ActStrokeWireResponse {
    ok: bool,
    path_kind: String,
    button_used: Option<String>,
    point_stream_count: u32,
    path_length_px: f64,
    duration_ms: f64,
    motion_model_used: Value,
    backend_used: String,
}
