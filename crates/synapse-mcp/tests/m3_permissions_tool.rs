use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::error_codes;
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn m3_permissions_refuse_ungranted_tool_calls_and_replay_path_escape() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_DEBUG_TOOLS", "1"),
            ("SYNAPSE_MCP_ALLOWED_PERMISSIONS", "none"),
        ],
    )
    .await?;

    assert_missing_permission(
        &client.tools_call_error("subscribe", json!({})).await?,
        "READ_EVENTS",
    );
    assert_missing_permission(
        &client.tools_call_error("reflex_list", json!({})).await?,
        "READ_REFLEX",
    );
    assert_missing_permission(
        &client.tools_call_error("profile_list", json!({})).await?,
        "READ_PROFILE",
    );
    assert_missing_permission(
        &client
            .tools_call_error("replay_record", json!({"duration_ms": 0}))
            .await?,
        "WRITE_REPLAY",
    );
    assert_missing_permission(
        &client
            .tools_call_error("audio_tail", json!({"seconds": 0}))
            .await?,
        "READ_AUDIO",
    );
    assert_missing_permission(
        &client
            .tools_call_error("storage_inspect", json!({}))
            .await?,
        "READ_STORAGE",
    );
    assert_missing_permission(
        &client
            .tools_call_error(
                "storage_put_probe_rows",
                json!({"cf_name": "events", "key_prefix": "deny", "rows": 1, "value_bytes": 1}),
            )
            .await?,
        "WRITE_STORAGE",
    );

    let status = client.shutdown().await?;
    assert!(status.success());

    let logs = TempDir::new()?;
    let local_app_data = TempDir::new()?;
    let outside_dir = TempDir::new()?;
    let outside = outside_dir.path().join("outside.jsonl");
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_MCP_ALLOWED_PERMISSIONS", "WRITE_REPLAY"),
            (
                "LOCALAPPDATA",
                local_app_data
                    .path()
                    .to_str()
                    .context("LOCALAPPDATA path utf8")?,
            ),
        ],
    )
    .await?;

    let error = client
        .tools_call_error("replay_record", json!({"duration_ms": 0, "path": outside}))
        .await?;
    assert_eq!(error["data"]["code"], error_codes::SAFETY_PERMISSION_DENIED);
    assert_eq!(error["data"]["permission"], "WRITE_REPLAY");
    assert_eq!(error["data"]["reason"], "path_outside_allow_root");
    assert!(error["data"]["allow_root"].as_str().is_some_and(|path| {
        path.ends_with(r"synapse\replays") || path.ends_with("synapse/replays")
    }));

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

fn assert_missing_permission(error: &Value, permission: &str) {
    assert_eq!(error["data"]["code"], error_codes::SAFETY_PERMISSION_DENIED);
    assert_eq!(error["data"]["missing_permission"], permission);
}
