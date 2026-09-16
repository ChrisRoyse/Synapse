use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf, timeline};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const STEP_NS: u64 = 1_000_000_000;

fn base_ts_ns() -> anyhow::Result<u64> {
    let now_ns = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
    // Recent timestamps so the 90-day TTL compaction filter cannot expire
    // rows mid-test; offset leaves room for 1000 forward steps.
    Ok(now_ns - 2_000 * STEP_NS)
}

fn structured(result: &Value) -> anyhow::Result<Value> {
    result
        .get("structuredContent")
        .cloned()
        .with_context(|| format!("missing structuredContent in {result}"))
}

async fn seed(
    client: &mut StdioMcpClient,
    prefix: &str,
    rows: u32,
    ts_start: u64,
    value_json: Value,
) -> anyhow::Result<()> {
    let put = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_TIMELINE,
                    "key_prefix": prefix,
                    "rows": rows,
                    "value_bytes": 0,
                    "value_json": value_json,
                    "ts_ns_start": ts_start,
                    "ts_ns_step": STEP_NS,
                    "key_mode": "timeline_ts",
                }),
            )
            .await?,
    )?;
    anyhow::ensure!(
        put["rows_added"] == rows,
        "seed {prefix} expected {rows} rows added, got {put}"
    );
    Ok(())
}

#[tokio::test]
async fn timeline_search_filters_pages_and_persists() -> anyhow::Result<()> {
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
    let base = base_ts_ns()?;

    // Synthetic ground truth: 30 chrome browser_nav rows (10 contain a
    // unique URL token), 20 excel focus_change rows offset by 100s, and 5
    // non-JSON byte rows that must surface as invalid_rows, not vanish.
    seed(
        &mut client,
        "tl-chrome-report",
        10,
        base,
        json!({"record_version": 1, "kind": "browser_nav", "actor": {"actor": "human"},
               "app": "chrome.exe",
               "payload": {"url": "https://example.test/quarterly-report", "title": "Quarterly Report"}}),
    )
    .await?;
    seed(
        &mut client,
        "tl-chrome-other",
        20,
        base + 10 * STEP_NS,
        json!({"record_version": 1, "kind": "browser_nav", "actor": {"actor": "human"},
               "app": "chrome.exe",
               "payload": {"url": "https://example.test/dashboard", "title": "Dashboard"}}),
    )
    .await?;
    seed(
        &mut client,
        "tl-excel",
        20,
        base + 100 * STEP_NS,
        json!({"record_version": 1, "kind": "focus_change", "actor": {"actor": "agent", "session_id": "worker-1"},
               "app": "EXCEL.EXE", "payload": {"path": "C:/docs/report.xlsx"}}),
    )
    .await?;
    let garbage = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_TIMELINE,
                    "key_prefix": "tl-garbage",
                    "rows": 5,
                    "value_bytes": 32,
                }),
            )
            .await?,
    )?;
    assert_eq!(garbage["rows_added"], 5);

    // Full scan: 50 decodable matches, 5 invalid rows surfaced.
    let all = structured(
        &client
            .tools_call("timeline_search", json!({"limit": 500}))
            .await?,
    )?;
    println!(
        "readback=timeline_search edge=full matches={} scanned={} invalid={} stopped={}",
        all["matches"].as_array().map_or(0, Vec::len),
        all["scanned_rows"],
        all["invalid_rows"],
        all["stopped_because"]
    );
    assert_eq!(all["matches"].as_array().map_or(0, Vec::len), 50);
    assert_eq!(all["invalid_rows"], 5);
    assert_eq!(all["stopped_because"], "end_of_timeline");
    assert!(all["next_cursor"].is_null());

    // Time window: exactly the 10 report rows.
    let window = structured(
        &client
            .tools_call(
                "timeline_search",
                json!({"start_ts_ns": base, "end_ts_ns": base + 9 * STEP_NS, "limit": 500}),
            )
            .await?,
    )?;
    assert_eq!(window["matches"].as_array().map_or(0, Vec::len), 10);
    assert_eq!(window["stopped_because"], "end_ts_reached");

    // App filter (case-insensitive).
    let excel = structured(
        &client
            .tools_call(
                "timeline_search",
                json!({"apps": ["excel.exe"], "limit": 500}),
            )
            .await?,
    )?;
    assert_eq!(excel["matches"].as_array().map_or(0, Vec::len), 20);

    // Text over nested payload values.
    let text = structured(
        &client
            .tools_call(
                "timeline_search",
                json!({"text": "Quarterly-Report", "limit": 500}),
            )
            .await?,
    )?;
    assert_eq!(text["matches"].as_array().map_or(0, Vec::len), 10);

    // Kind + actor filters.
    let agent_focus = structured(
        &client
            .tools_call(
                "timeline_search",
                json!({"kinds": ["focus_change"], "actor": "agent", "limit": 500}),
            )
            .await?,
    )?;
    assert_eq!(agent_focus["matches"].as_array().map_or(0, Vec::len), 20);
    let first_actor = agent_focus["matches"][0]["actor"]
        .as_str()
        .context("actor missing")?;
    assert_eq!(first_actor, "agent:worker-1");

    // Paging: limit 7 -> cursor loop must reassemble all 50 with no overlap.
    let mut cursor: Option<String> = None;
    let mut paged_keys = Vec::new();
    let started = Instant::now();
    loop {
        let mut args = json!({"limit": 7});
        if let Some(cursor_value) = &cursor {
            args["cursor"] = Value::String(cursor_value.clone());
        }
        let page = structured(&client.tools_call("timeline_search", args).await?)?;
        for entry in page["matches"].as_array().context("matches array")? {
            paged_keys.push(entry["key_hex"].as_str().context("key_hex")?.to_owned());
        }
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    println!(
        "readback=timeline_search edge=paging pages_total_keys={} elapsed_ms={}",
        paged_keys.len(),
        started.elapsed().as_millis()
    );
    assert_eq!(paged_keys.len(), 50);
    let mut deduped = paged_keys.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), 50, "paging must not duplicate rows");

    // Honest empty result.
    let empty = structured(
        &client
            .tools_call("timeline_search", json!({"text": "zzz-no-such-token"}))
            .await?,
    )?;
    assert_eq!(empty["matches"].as_array().map_or(1, Vec::len), 0);

    // Validation failures are structured errors.
    for (args, fragment) in [
        (json!({"start_ts_ns": 10, "end_ts_ns": 1}), "must be <="),
        (json!({"limit": 0}), "limit"),
        (json!({"kinds": ["nope"]}), "not a known timeline kind"),
        (json!({"actor": "alien"}), "actor"),
        (json!({"cursor": "zz"}), "cursor"),
    ] {
        let error = client.tools_call_error("timeline_search", args).await?;
        let text = error.to_string();
        assert!(
            text.contains("TOOL_PARAMS_INVALID") && text.contains(fragment),
            "expected TOOL_PARAMS_INVALID with {fragment:?}, got {text}"
        );
    }

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown: 55 rows persisted, codec keys
    // decode to the seeded timestamps.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_TIMELINE)?;
    let codec_count = rows
        .iter()
        .filter(|(key, _value)| timeline::decode_timeline_key(key).is_ok())
        .count();
    println!(
        "readback=timeline_search edge=physical_sot total_rows={} codec_rows={}",
        rows.len(),
        codec_count
    );
    assert_eq!(rows.len(), 55);
    assert_eq!(codec_count, 50);
    Ok(())
}
