//! `episode_segment` tool integration regression (#846): real daemon, real
//! `RocksDB`, real MCP calls. Seeds a synthetic day of timeline rows whose
//! expected episodes are known, segments it, re-segments it (idempotency),
//! exercises dry-run, disk-pressure refusal, and validation errors, then
//! reopens the database after shutdown and decodes the physical `CF_EPISODES`
//! rows.

use anyhow::Context;
use chrono::{Local, TimeZone};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::EpisodeRecord;
use synapse_storage::{Db, cf, decode_json, episodes as episode_codec};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SEC: u64 = 1_000_000_000;

/// 01:00 of the current local day: every seeded row lands inside one local
/// day, so the segmentation unit under test is exactly one replacement day.
fn base_ts_ns() -> anyhow::Result<u64> {
    let midnight = Local::now()
        .date_naive()
        .and_hms_opt(1, 0, 0)
        .context("01:00 must exist")?;
    let instant = Local
        .from_local_datetime(&midnight)
        .earliest()
        .context("local 01:00 unresolvable")?;
    let nanos = instant
        .timestamp_nanos_opt()
        .context("timestamp out of range")?;
    Ok(u64::try_from(nanos)?)
}

fn structured(result: &Value) -> anyhow::Result<Value> {
    result
        .get("structuredContent")
        .cloned()
        .with_context(|| format!("missing structuredContent in {result}"))
}

async fn seed_row(
    client: &mut StdioMcpClient,
    prefix: &str,
    ts_ns: u64,
    value_json: Value,
) -> anyhow::Result<()> {
    let put = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_TIMELINE,
                    "key_prefix": prefix,
                    "rows": 1,
                    "value_bytes": 0,
                    "value_json": value_json,
                    "ts_ns_start": ts_ns,
                    "key_mode": "timeline_ts",
                }),
            )
            .await?,
    )?;
    anyhow::ensure!(put["rows_added"] == 1, "seed {prefix} failed: {put}");
    Ok(())
}

fn episode_cf_count(inspect: &Value) -> u64 {
    inspect["cf_row_counts"][cf::CF_EPISODES]
        .as_u64()
        .unwrap_or(0)
}

fn episode_samples(inspect: &Value) -> Value {
    inspect["cf_row_samples"][cf::CF_EPISODES].clone()
}

#[tokio::test]
async fn episode_segment_segments_idempotently_and_persists() -> anyhow::Result<()> {
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

    // Edge: empty timeline is a structured no-op, not an error.
    let empty = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!(
        "readback=episode_segment edge=empty stopped={} written={}",
        empty["stopped_because"], empty["episodes_written"]
    );
    assert_eq!(empty["stopped_because"], "empty_timeline");
    assert_eq!(empty["episodes_written"], 0);

    // Synthetic ground truth, one local day:
    //   01:00:00 focus code.exe          ┐ episode 1: code.exe, 120 s,
    //   01:00:30 cadence 100 keys/5 clk  ┘   100 keystrokes, 5 clicks
    //   01:02:00 focus chrome.exe        ┐ episode 2: chrome.exe,
    //   01:02:05 nav github.com          ┘   document github.com, ends idle
    //   01:06:40 idle_start (closes episode 2 at its ts)
    seed_row(
        &mut client,
        "ep-focus-code",
        base,
        json!({"record_version": 1, "kind": "focus_change", "actor": {"actor": "human"},
               "app": "code.exe",
               "payload": {"title": "main.rs - project", "pid": 7, "hwnd": 11, "source": "event"}}),
    )
    .await?;
    seed_row(
        &mut client,
        "ep-cadence-code",
        base + 30 * SEC,
        json!({"record_version": 1, "kind": "interaction_summary", "actor": {"actor": "human"},
               "app": "code.exe",
               "payload": {"keystroke_count": 100, "click_count": 5}}),
    )
    .await?;
    seed_row(
        &mut client,
        "ep-focus-chrome",
        base + 120 * SEC,
        json!({"record_version": 1, "kind": "focus_change", "actor": {"actor": "human"},
               "app": "chrome.exe",
               "payload": {"title": "GitHub", "pid": 8, "hwnd": 12, "source": "event"}}),
    )
    .await?;
    seed_row(
        &mut client,
        "ep-nav-chrome",
        base + 125 * SEC,
        json!({"record_version": 1, "kind": "browser_nav", "actor": {"actor": "human"},
               "app": "chrome.exe",
               "payload": {"url": "https://github.com/org/repo", "title": "repo"}}),
    )
    .await?;
    seed_row(
        &mut client,
        "ep-idle",
        base + 400 * SEC,
        json!({"record_version": 1, "kind": "idle_start", "actor": {"actor": "human"},
               "payload": {"idle_ms_at_detection": 180_000, "idle_timeout_ms": 180_000}}),
    )
    .await?;
    // Edge: undecodable rows must surface as invalid_rows, not vanish.
    let garbage = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_TIMELINE,
                    "key_prefix": "ep-garbage",
                    "rows": 3,
                    "value_bytes": 16,
                }),
            )
            .await?,
    )?;
    assert_eq!(garbage["rows_added"], 3);

    let before = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    println!(
        "readback=storage_inspect edge=before episodes_rows={}",
        episode_cf_count(&before)
    );
    assert_eq!(episode_cf_count(&before), 0);

    // First segmentation.
    let first = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!("readback=episode_segment edge=first {first}");
    assert_eq!(first["episodes_written"], 2);
    assert_eq!(first["episodes_deleted"], 0);
    assert_eq!(first["invalid_rows"], 3);
    assert_eq!(first["stopped_because"], "range_complete");

    let after_first = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    let samples_first = episode_samples(&after_first);
    println!(
        "readback=storage_inspect edge=after_first episodes_rows={} samples={samples_first}",
        episode_cf_count(&after_first)
    );
    assert_eq!(episode_cf_count(&after_first), 2);

    // Idempotency: same timeline, same config -> same physical bytes.
    let second = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!("readback=episode_segment edge=resegment {second}");
    assert_eq!(second["episodes_written"], 2);
    assert_eq!(second["episodes_deleted"], 2);
    let after_second = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(episode_cf_count(&after_second), 2);
    assert_eq!(
        episode_samples(&after_second),
        samples_first,
        "re-segmentation must reproduce byte-identical episode rows"
    );

    // Edge: dry_run computes but never mutates.
    let dry = structured(
        &client
            .tools_call("episode_segment", json!({"dry_run": true}))
            .await?,
    )?;
    println!("readback=episode_segment edge=dry_run {dry}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["episodes_written"], 2);
    let after_dry = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(
        episode_samples(&after_dry),
        samples_first,
        "dry_run must not mutate"
    );

    // Edge: disk-pressure refusal happens before any mutation; dry_run is
    // still allowed because it writes nothing.
    let pressed = structured(
        &client
            .tools_call("storage_pressure_sample", json!({"free_bytes": 0}))
            .await?,
    )?;
    println!(
        "readback=storage_pressure_sample edge=press level={}",
        pressed["report"]["current_level"]["name"]
    );
    let refused = client
        .tools_call_error("episode_segment", json!({}))
        .await?;
    let refused_text = refused.to_string();
    println!("readback=episode_segment edge=pressure_refusal {refused_text}");
    assert!(
        refused_text.contains("disk pressure"),
        "expected pressure refusal, got {refused_text}"
    );
    let dry_under_pressure = structured(
        &client
            .tools_call("episode_segment", json!({"dry_run": true}))
            .await?,
    )?;
    assert_eq!(dry_under_pressure["episodes_written"], 2);
    let _released = structured(
        &client
            .tools_call(
                "storage_pressure_sample",
                json!({"free_bytes": 1_000_000_000_000_u64}),
            )
            .await?,
    )?;

    // Edge: structured validation error.
    let invalid = client
        .tools_call_error(
            "episode_segment",
            json!({"start_ts_ns": 10, "end_ts_ns": 5}),
        )
        .await?;
    let invalid_text = invalid.to_string();
    assert!(
        invalid_text.contains("TOOL_PARAMS_INVALID"),
        "expected TOOL_PARAMS_INVALID, got {invalid_text}"
    );

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_EPISODES)?;
    println!(
        "readback=episode_segment edge=physical_sot rows={}",
        rows.len()
    );
    assert_eq!(rows.len(), 2);
    let mut episodes = Vec::new();
    for (key, value) in &rows {
        let (start_ts_ns, ordinal) = episode_codec::decode_episode_key(key)?;
        let record: EpisodeRecord = decode_json(value)?;
        println!(
            "readback=cf_episodes key_ts={start_ts_ns} ordinal={ordinal} id={} app={:?} doc={:?} \
             start={} end={} started={:?} ended={:?} keys={} clicks={}",
            record.episode_id,
            record.app,
            record.document,
            record.start_ts_ns,
            record.end_ts_ns,
            record.started_because,
            record.ended_because,
            record.keystroke_count,
            record.click_count
        );
        assert_eq!(
            start_ts_ns, record.start_ts_ns,
            "key must mirror the record"
        );
        assert_eq!(record.ts_ns, record.start_ts_ns, "TTL contract field");
        episodes.push(record);
    }
    episodes.sort_by_key(|episode| episode.start_ts_ns);

    let code = &episodes[0];
    assert_eq!(code.app.as_deref(), Some("code.exe"));
    assert_eq!(code.start_ts_ns, base);
    assert_eq!(code.end_ts_ns, base + 120 * SEC);
    assert_eq!(code.keystroke_count, 100);
    assert_eq!(code.click_count, 5);

    let chrome = &episodes[1];
    assert_eq!(chrome.app.as_deref(), Some("chrome.exe"));
    assert_eq!(chrome.document.as_deref(), Some("github.com"));
    assert_eq!(chrome.url.as_deref(), Some("https://github.com/org/repo"));
    assert_eq!(chrome.start_ts_ns, base + 120 * SEC);
    assert_eq!(chrome.end_ts_ns, base + 400 * SEC);
    Ok(())
}
