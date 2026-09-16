//! `timeline_digest` tool integration regression (#850): real daemon, real
//! `RocksDB`, real MCP calls. Seeds a synthetic day of timeline rows whose
//! expected episodes are known, segments it into real `CF_EPISODES` rows, then
//! asserts the digest's totals/by-app/by-document/per-day breakdown reconcile
//! exactly with those rows — happy path, day vs week windows, validation
//! edges, the empty store, and a physical `CF_EPISODES` readback after
//! shutdown proving `active_ms` equals the sum of the physical episode
//! durations.

use anyhow::Context;
use chrono::{Local, TimeZone};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::EpisodeRecord;
use synapse_storage::{Db, cf, decode_json, episodes as episode_codec};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SEC: u64 = 1_000_000_000;

/// 01:00 of the current local day, matching the #846/#847 regressions so every
/// seeded row lands inside one local segmentation day.
fn base_ts_ns() -> anyhow::Result<u64> {
    let one_am = Local::now()
        .date_naive()
        .and_hms_opt(1, 0, 0)
        .context("01:00 must exist")?;
    let instant = Local
        .from_local_datetime(&one_am)
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

fn u64_at(value: &Value, key: &str) -> anyhow::Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("missing u64 field {key} in {value}"))
}

/// Finds the `by_app`/`top_documents` group row whose `key` matches.
fn group<'a>(value: &'a Value, array_key: &str, group_key: &str) -> anyhow::Result<&'a Value> {
    value
        .get(array_key)
        .and_then(Value::as_array)
        .with_context(|| format!("missing array {array_key}"))?
        .iter()
        .find(|row| row.get("key").and_then(Value::as_str) == Some(group_key))
        .with_context(|| format!("no {array_key} row with key {group_key}"))
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

#[tokio::test]
async fn timeline_digest_reconciles_with_the_episode_store() -> anyhow::Result<()> {
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

    // Edge: empty episode store digests to all zeros with a single day row, and
    // reports zero rows scanned in both stores.
    let empty = structured(
        &client
            .tools_call(
                "timeline_digest",
                json!({"period": "day", "anchor_ts_ns": base}),
            )
            .await?,
    )?;
    println!(
        "readback=timeline_digest edge=empty_store episodes={} active_ms={} per_day_len={} routines_scanned={}",
        empty["episode_count"],
        empty["active_ms"],
        empty["per_day"].as_array().map_or(0, Vec::len),
        empty["routines_scanned_rows"]
    );
    assert_eq!(u64_at(&empty, "episode_count")?, 0);
    assert_eq!(u64_at(&empty, "active_ms")?, 0);
    assert_eq!(u64_at(&empty, "idle_ms")?, 0);
    assert_eq!(empty["per_day"].as_array().map_or(0, Vec::len), 1);
    assert_eq!(empty["by_app"], json!([]));
    assert_eq!(u64_at(&empty, "routines_scanned_rows")?, 0);
    assert_eq!(empty["routines_touched"], json!([]));

    // Synthetic ground truth (one local day), identical to the #847 regression:
    //   01:00:00 focus code.exe          ┐ episode 1: code.exe, 120 s,
    //   01:00:30 cadence 100 keys/5 clk  ┘   100 keystrokes, 5 clicks
    //   01:02:00 focus chrome.exe        ┐ episode 2: chrome.exe,
    //   01:02:05 nav github.com          │   document github.com, 280 s,
    //   01:03:20 agent clipboard row     │   (agent row excluded from
    //   01:06:40 idle_start              ┘    segmentation, no human time)
    // Human active_ms = 120_000 + 280_000 = 400_000; the two episodes are
    // contiguous (chrome starts exactly when code ends) so idle_ms = 0.
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
        "ep-agent-clip",
        base + 200 * SEC,
        json!({"record_version": 1, "kind": "clipboard",
               "actor": {"actor": "agent", "session_id": "sess-test"},
               "app": "chrome.exe", "payload": {"summary": "copied"}}),
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

    let segmented = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!("readback=episode_segment scenario=seeded_day {segmented}");
    assert_eq!(segmented["episodes_written"], 2);

    // Happy path: the day digest reconciles with the two episodes exactly.
    let digest = structured(
        &client
            .tools_call(
                "timeline_digest",
                json!({"period": "day", "anchor_ts_ns": base}),
            )
            .await?,
    )?;
    println!("readback=timeline_digest scenario=day {digest}");
    assert_eq!(u64_at(&digest, "episode_count")?, 2);
    assert_eq!(
        u64_at(&digest, "active_ms")?,
        400_000,
        "120s + 280s human active"
    );
    assert_eq!(
        u64_at(&digest, "idle_ms")?,
        0,
        "contiguous episodes ⇒ no idle"
    );
    assert_eq!(u64_at(&digest, "total_keystrokes")?, 100);
    assert_eq!(u64_at(&digest, "total_clicks")?, 5);
    assert_eq!(digest["actor_filter"], "human");
    assert_eq!(u64_at(&digest, "days_covered")?, 1);

    // by_app reconciles per app and is ordered by active time (chrome first).
    let by_app = digest["by_app"].as_array().context("by_app array")?;
    assert_eq!(by_app.len(), 2, "code.exe + chrome.exe");
    assert_eq!(by_app[0]["key"], "chrome.exe", "biggest active app first");
    let code = group(&digest, "by_app", "code.exe")?;
    assert_eq!(u64_at(code, "active_ms")?, 120_000);
    assert_eq!(u64_at(code, "episode_count")?, 1);
    assert_eq!(u64_at(code, "keystroke_count")?, 100);
    assert_eq!(u64_at(code, "click_count")?, 5);
    let chrome = group(&digest, "by_app", "chrome.exe")?;
    assert_eq!(u64_at(chrome, "active_ms")?, 280_000);

    // top_documents carries the browser host with its representative url.
    let github = group(&digest, "top_documents", "github.com")?;
    assert_eq!(u64_at(github, "active_ms")?, 280_000);
    assert_eq!(github["url"], "https://github.com/org/repo");

    // Reconciliation invariants: active == Σ by_app(+residual) == Σ per_day.
    let app_sum: u64 = by_app
        .iter()
        .filter_map(|g| g["active_ms"].as_u64())
        .sum::<u64>()
        + u64_at(&digest["by_app_other"], "active_ms")?;
    assert_eq!(app_sum, 400_000, "Σ by_app + residual == active_ms");
    let day_sum: u64 = digest["per_day"]
        .as_array()
        .context("per_day")?
        .iter()
        .filter_map(|d| d["active_ms"].as_u64())
        .sum();
    assert_eq!(day_sum, 400_000, "Σ per_day == active_ms");
    // No routines are mined from a single day, so the routine surface is empty
    // but the real CF_ROUTINES scan still ran.
    assert_eq!(digest["routines_touched"], json!([]));
    assert!(u64_at(&digest, "episodes_scanned_rows")? >= 2);

    // Week window over the same data: the day's totals roll up unchanged, but
    // the breakdown now spans seven local-day rows with one populated.
    let week = structured(
        &client
            .tools_call(
                "timeline_digest",
                json!({"period": "week", "anchor_ts_ns": base}),
            )
            .await?,
    )?;
    println!(
        "readback=timeline_digest scenario=week days_covered={} active_ms={}",
        week["days_covered"], week["active_ms"]
    );
    assert_eq!(u64_at(&week, "days_covered")?, 7);
    assert_eq!(week["per_day"].as_array().map_or(0, Vec::len), 7);
    assert_eq!(
        u64_at(&week, "active_ms")?,
        400_000,
        "all activity is in one day"
    );
    let populated = week["per_day"]
        .as_array()
        .context("week per_day")?
        .iter()
        .filter(|d| d["active_ms"].as_u64().unwrap_or(0) > 0)
        .count();
    assert_eq!(populated, 1, "exactly one day in the week has activity");

    // Agent-inclusive digest: episode_segment wrote human episodes only, so the
    // agent-inclusive view still sees the same human episodes (the agent
    // clipboard row never became its own episode).
    let with_agent = structured(
        &client
            .tools_call(
                "timeline_digest",
                json!({"period": "day", "anchor_ts_ns": base, "include_agent_activity": true}),
            )
            .await?,
    )?;
    assert_eq!(with_agent["actor_filter"], "human+agent");
    assert_eq!(u64_at(&with_agent, "active_ms")?, 400_000);

    // Edge: structured validation errors, never a guessed digest.
    for (params, fragment) in [
        (
            json!({"period": "month", "anchor_ts_ns": base}),
            "period must be",
        ),
        (
            json!({"period": "day", "anchor_ts_ns": base, "top_n": 0}),
            "top_n",
        ),
        (
            json!({"period": "day", "date": "2026-06-13", "anchor_ts_ns": base}),
            "at most one",
        ),
        (json!({"period": "day", "date": "not-a-date"}), "YYYY-MM-DD"),
    ] {
        let error = client
            .tools_call_error("timeline_digest", params.clone())
            .await?;
        let text = error.to_string();
        println!("readback=timeline_digest edge=invalid params={params} err={text}");
        assert!(
            text.contains(fragment),
            "expected {fragment:?} in error for {params}, got {text}"
        );
    }

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown: the digest's active_ms equals
    // the sum of the physical CF_EPISODES row durations.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_EPISODES)?;
    let mut physical_active_ms = 0_u64;
    let mut physical_episodes = 0_u64;
    for (key, value) in &rows {
        if episode_codec::decode_episode_key(key).is_ok() {
            let record: EpisodeRecord = decode_json(value)?;
            physical_active_ms += record.duration_ms();
            physical_episodes += 1;
        }
    }
    println!(
        "readback=timeline_digest edge=physical_sot physical_episodes={physical_episodes} physical_active_ms={physical_active_ms}"
    );
    assert_eq!(physical_episodes, 2, "two physical episode rows");
    assert_eq!(
        physical_active_ms, 400_000,
        "digest active_ms reconciles with physical CF_EPISODES durations"
    );
    Ok(())
}
