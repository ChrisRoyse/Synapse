use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

#[tokio::test]
async fn storage_tools_are_default_granted_and_persist_probe_rows() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_DEBUG_TOOLS", "1"),
            ("SYNAPSE_DB", db_path_string.as_str()),
        ],
    )
    .await?;

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    let before_events = before["cf_row_counts"][cf::CF_EVENTS].as_u64().unwrap_or(0);
    println!("readback=storage_tool edge=before before_events={before_events}");

    let key_prefix = "regression-default-storage";
    let put = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_EVENTS,
                    "key_prefix": key_prefix,
                    "rows": 2,
                    "value_bytes": 16
                }),
            )
            .await?,
    )?;
    assert_eq!(put["rows_added"], 2);
    assert_eq!(put["before_rows"], before_events);
    assert_eq!(put["after_rows"], before_events + 2);

    let after = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    let after_events = after["cf_row_counts"][cf::CF_EVENTS]
        .as_u64()
        .context("events row count missing")?;
    println!(
        "readback=storage_tool edge=after_mcp before_events={before_events} after_events={after_events} put={put}"
    );
    assert_eq!(after_events, before_events + 2);

    let status = client.shutdown().await?;
    assert!(status.success());

    let durable_rows = direct_probe_row_count(&db_path, cf::CF_EVENTS, key_prefix)?;
    println!(
        "readback=storage_tool edge=after_shutdown_source_of_truth key_prefix={key_prefix} durable_rows={durable_rows}"
    );
    assert_eq!(durable_rows, 2);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "integration test keeps storage pressure and column-family assertions in one flow"
)]
async fn storage_probe_rows_cover_pressure_gated_column_families() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_DEBUG_TOOLS", "1"),
            ("SYNAPSE_DB", db_path_string.as_str()),
        ],
    )
    .await?;

    let l3 = structured(
        &client
            .tools_call(
                "storage_pressure_sample",
                json!({"free_bytes": 499_999_999u64}),
            )
            .await?,
    )?;
    assert_eq!(l3["report"]["current_level"]["name"], "Level3");
    assert_eq!(
        l3["report"]["compacted_cfs"].as_array().map(Vec::len),
        Some(cf::ALL_COLUMN_FAMILIES.len())
    );

    let l3_blocked = client
        .tools_call_error(
            "storage_put_probe_rows",
            json!({
                "cf_name": cf::CF_OBSERVATIONS,
                "key_prefix": "pressure-l3-observations",
                "rows": 1,
                "value_bytes": 8
            }),
        )
        .await?;
    assert_eq!(
        l3_blocked["data"]["code"],
        synapse_core::error_codes::STORAGE_WRITE_FAILED
    );

    let l3_allowed = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_EVENTS,
                    "key_prefix": "pressure-l3-events",
                    "rows": 1,
                    "value_bytes": 8
                }),
            )
            .await?,
    )?;
    assert_eq!(l3_allowed["rows_added"], 1);

    let after_l3 = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    println!(
        "readback=storage_pressure edge=l3 observations={} events={} pressure={}",
        after_l3["cf_row_counts"][cf::CF_OBSERVATIONS],
        after_l3["cf_row_counts"][cf::CF_EVENTS],
        after_l3["pressure_level"]["name"]
    );
    assert_eq!(after_l3["cf_row_counts"][cf::CF_OBSERVATIONS], 0);
    assert_eq!(after_l3["cf_row_counts"][cf::CF_EVENTS], 1);

    let l4 = structured(
        &client
            .tools_call(
                "storage_pressure_sample",
                json!({"free_bytes": 199_999_999u64}),
            )
            .await?,
    )?;
    assert_eq!(l4["report"]["current_level"]["name"], "Level4");

    let l4_blocked = client
        .tools_call_error(
            "storage_put_probe_rows",
            json!({
                "cf_name": cf::CF_EVENTS,
                "key_prefix": "pressure-l4-events",
                "rows": 1,
                "value_bytes": 8
            }),
        )
        .await?;
    assert_eq!(
        l4_blocked["data"]["code"],
        synapse_core::error_codes::STORAGE_WRITE_FAILED
    );

    let l4_audit = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_REFLEX_AUDIT,
                    "key_prefix": "pressure-l4-reflex-audit",
                    "rows": 1,
                    "value_bytes": 8
                }),
            )
            .await?,
    )?;
    assert_eq!(l4_audit["rows_added"], 1);

    let l4_sessions = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_SESSIONS,
                    "key_prefix": "pressure-l4-sessions",
                    "rows": 1,
                    "value_bytes": 8
                }),
            )
            .await?,
    )?;
    assert_eq!(l4_sessions["rows_added"], 1);

    let after_l4 = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    println!(
        "readback=storage_pressure edge=l4 events={} reflex_audit={} sessions={} pressure={}",
        after_l4["cf_row_counts"][cf::CF_EVENTS],
        after_l4["cf_row_counts"][cf::CF_REFLEX_AUDIT],
        after_l4["cf_row_counts"][cf::CF_SESSIONS],
        after_l4["pressure_level"]["name"]
    );
    assert_eq!(after_l4["cf_row_counts"][cf::CF_EVENTS], 1);
    assert_eq!(after_l4["cf_row_counts"][cf::CF_REFLEX_AUDIT], 1);
    assert_eq!(after_l4["cf_row_counts"][cf::CF_SESSIONS], 1);

    let status = client.shutdown().await?;
    assert!(status.success());

    assert_eq!(
        direct_probe_row_count(&db_path, cf::CF_OBSERVATIONS, "pressure-l3-observations")?,
        0
    );
    assert_eq!(
        direct_probe_row_count(&db_path, cf::CF_EVENTS, "pressure-l3-events")?,
        1
    );
    assert_eq!(
        direct_probe_row_count(&db_path, cf::CF_EVENTS, "pressure-l4-events")?,
        0
    );
    assert_eq!(
        direct_probe_row_count(&db_path, cf::CF_REFLEX_AUDIT, "pressure-l4-reflex-audit")?,
        1
    );
    assert_eq!(
        direct_probe_row_count(&db_path, cf::CF_SESSIONS, "pressure-l4-sessions")?,
        1
    );
    Ok(())
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

fn direct_probe_row_count(
    db_path: &std::path::Path,
    cf_name: &str,
    key_prefix: &str,
) -> anyhow::Result<usize> {
    let prefix = key_prefix.as_bytes();
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    Ok(db
        .scan_cf(cf_name)?
        .into_iter()
        .filter(|(key, _value)| key.starts_with(prefix))
        .count())
}
