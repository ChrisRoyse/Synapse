use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::{ReflexState, SCHEMA_VERSION, StoredReflexAudit};
use synapse_storage::{Db, cf, decode_json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn reflex_list_schema_default_and_cancelled_merge() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        // Synthetic notepad foreground so the reflex_register supported-use gate
        // does not depend on a real focused window (A11Y_NO_FOREGROUND otherwise).
        &[
            ("SYNAPSE_DB", db_path_string.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
        ],
    )
    .await?;
    activate_notepad_profile(&mut client).await?;

    let tools = client.tools_list().await?;
    let tools = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools array missing")?;
    let reflex_list_tool = tools
        .iter()
        .find(|tool| tool["name"] == "reflex_list")
        .context("reflex_list tool missing")?;
    assert_reflex_list_schema(reflex_list_tool);

    let empty = client.tools_call("reflex_list", json!({})).await?;
    assert!(
        structured(&empty)?["reflexes"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    let first = register(&mut client, "support-list-a").await?;
    let second = register(&mut client, "support-list-b").await?;
    let third = register(&mut client, "support-list-c").await?;

    let list = client.tools_call("reflex_list", json!({})).await?;
    let list = structured(&list)?;
    assert_reflex_ids(&list, &[&first, &second, &third])?;

    let cancelled = client
        .tools_call("reflex_cancel", json!({"reflex_id": first}))
        .await?;
    assert_eq!(structured(&cancelled)?["cancelled"], true);

    let default_after_cancel = client.tools_call("reflex_list", json!({})).await?;
    let default_after_cancel = structured(&default_after_cancel)?;
    assert_reflex_ids(&default_after_cancel, &[&second, &third])?;

    let include_expired = client
        .tools_call("reflex_list", json!({"include_expired": true}))
        .await?;
    let include_expired = structured(&include_expired)?;
    assert_reflex_ids(&include_expired, &[&first, &second, &third])?;
    assert_eq!(state_for(&include_expired, &first)?, "cancelled");

    let status = client.shutdown().await?;
    assert!(status.success());

    let audits = read_audits(&db_path)?;
    assert_eq!(
        audits
            .iter()
            .filter(|audit| audit.details["kind"] == "reflex_registered")
            .count(),
        3
    );
    assert!(audits.iter().any(|audit| {
        audit.reflex_id == first
            && audit.status == ReflexState::Cancelled
            && audit.details["lifetime"]["kind"] == "until_cancelled"
    }));

    let mut restarted = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        // Synthetic notepad foreground so the reflex_register supported-use gate
        // does not depend on a real focused window (A11Y_NO_FOREGROUND otherwise).
        &[
            ("SYNAPSE_DB", db_path_string.as_str()),
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
        ],
    )
    .await?;
    let restored = restarted
        .tools_call("reflex_list", json!({"include_expired": true}))
        .await?;
    let restored = structured(&restored)?;
    assert_reflex_ids(&restored, &[&first])?;
    assert_eq!(state_for(&restored, &first)?, "cancelled");

    let status = restarted.shutdown().await?;
    assert!(status.success());
    Ok(())
}

async fn register(client: &mut StdioMcpClient, kind: &str) -> anyhow::Result<String> {
    let response = client
        .tools_call("reflex_register", valid_register_args(kind))
        .await?;
    let payload = structured(&response)?;
    payload["reflex_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .context("reflex_id missing")
}

async fn activate_notepad_profile(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let response = client
        .tools_call("profile_activate", json!({"profile_id": "notepad"}))
        .await?;
    assert_eq!(structured(&response)?["active_profile_id"], "notepad");
    Ok(())
}

fn valid_register_args(kind: &str) -> Value {
    json!({
        "kind": "on_event",
        "when": {"op": "kind", "kind": kind},
        "then": {"kind": "action", "action": {"kind": "release_all"}}
    })
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

fn assert_reflex_ids(payload: &Value, expected: &[&str]) -> anyhow::Result<()> {
    let mut actual = payload["reflexes"]
        .as_array()
        .context("reflexes should be an array")?
        .iter()
        .map(|reflex| reflex["id"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = expected
        .iter()
        .map(|id| (*id).to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
    Ok(())
}

fn state_for<'a>(payload: &'a Value, reflex_id: &str) -> anyhow::Result<&'a str> {
    payload["reflexes"]
        .as_array()
        .and_then(|reflexes| reflexes.iter().find(|reflex| reflex["id"] == reflex_id))
        .and_then(|reflex| reflex["state"].as_str())
        .context("reflex state missing")
}

fn read_audits(db_path: &std::path::Path) -> anyhow::Result<Vec<StoredReflexAudit>> {
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    db.scan_cf(cf::CF_REFLEX_AUDIT)?
        .into_iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(&value).map_err(Into::into))
        .collect()
}

fn assert_reflex_list_schema(tool: &Value) {
    let shape = json!({
        "name": tool.get("name").cloned().unwrap_or(Value::Null),
        "inputSchema": tool.get("inputSchema").cloned().unwrap_or(Value::Null),
        "outputSchema": tool.get("outputSchema").cloned().unwrap_or(Value::Null),
    });
    assert_eq!(shape["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        shape["inputSchema"]["properties"]["include_expired"]["default"],
        false
    );
    assert_eq!(shape["outputSchema"]["required"][0], "reflexes");
    insta::assert_json_snapshot!("m3_reflex_list_tool", shape);
}
