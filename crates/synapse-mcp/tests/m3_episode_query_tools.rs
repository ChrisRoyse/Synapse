//! `episode_list` / `episode_get` tool integration regression (#847): real
//! daemon, real `RocksDB`, real MCP calls. Seeds a synthetic day of timeline
//! rows whose expected episodes are known, segments it, then exercises the
//! query surface: overlap-window listing, app/actor/min-duration filters,
//! cursor paging, id lookup with timeline evidence refs and ref paging, and
//! the loud-failure edges (empty store, unknown id, invalid params, corrupt
//! derived rows). Ends with a physical `CF_EPISODES` readback after shutdown.

use anyhow::Context;
use chrono::{Local, TimeZone};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::EpisodeRecord;
use synapse_storage::{Db, cf, decode_json, episodes as episode_codec};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SEC: u64 = 1_000_000_000;

/// 01:00 of the current local day, matching the #846 regression so every
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

#[expect(
    clippy::too_many_lines,
    reason = "one ordered end-to-end regression: seed -> segment -> list/get matrix -> corruption -> physical readback"
)]
#[tokio::test]
async fn episode_query_tools_list_filter_page_and_fail_loudly() -> anyhow::Result<()> {
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

    // Edge: empty store lists nothing and a lookup is a structured error.
    let empty = structured(&client.tools_call("episode_list", json!({})).await?)?;
    println!(
        "readback=episode_list edge=empty_store episodes={} stopped={}",
        empty["episodes"].as_array().map_or(0, Vec::len),
        empty["stopped_because"]
    );
    assert_eq!(empty["episodes"], json!([]));
    assert_eq!(empty["stopped_because"], "end_of_episodes");
    let missing = client
        .tools_call_error("episode_get", json!({"episode_id": "ep1-deadbeefdeadbeef"}))
        .await?;
    let missing_text = missing.to_string();
    println!("readback=episode_get edge=empty_store {missing_text}");
    assert!(
        missing_text.contains("EPISODE_NOT_FOUND"),
        "expected EPISODE_NOT_FOUND, got {missing_text}"
    );

    // Synthetic ground truth (one local day), the #846 day plus one agent row:
    //   01:00:00 focus code.exe          ┐ episode 1: code.exe, 120 s,
    //   01:00:30 cadence 100 keys/5 clk  ┘   100 keystrokes, 5 clicks
    //   01:02:00 focus chrome.exe        ┐ episode 2: chrome.exe,
    //   01:02:05 nav github.com          │   document github.com, ends idle
    //   01:03:20 agent clipboard row     │   (excluded from segmentation,
    //   01:06:40 idle_start              ┘    visible in evidence refs)
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

    // Happy path: the whole day lists both episodes in order with identity,
    // duration, and interaction summaries.
    let listed = structured(&client.tools_call("episode_list", json!({})).await?)?;
    println!("readback=episode_list scenario=whole_day {listed}");
    let episodes = listed["episodes"]
        .as_array()
        .context("episodes array")?
        .clone();
    assert_eq!(episodes.len(), 2);
    assert_eq!(listed["stopped_because"], "end_of_episodes");
    assert!(listed.get("next_cursor").is_none());
    let code = &episodes[0];
    let chrome = &episodes[1];
    assert_eq!(code["app"], "code.exe");
    assert_eq!(code["start_ts_ns"], json!(base));
    assert_eq!(code["end_ts_ns"], json!(base + 120 * SEC));
    assert_eq!(code["duration_ms"], json!(120_000));
    assert_eq!(code["keystroke_count"], 100);
    assert_eq!(code["click_count"], 5);
    assert_eq!(code["actor"], "human");
    assert_eq!(chrome["app"], "chrome.exe");
    assert_eq!(chrome["document"], "github.com");
    assert_eq!(chrome["url"], "https://github.com/org/repo");
    assert_eq!(chrome["duration_ms"], json!(280_000));
    let code_id = code["episode_id"].as_str().context("code id")?.to_owned();
    let chrome_id = chrome["episode_id"]
        .as_str()
        .context("chrome id")?
        .to_owned();
    assert!(code_id.starts_with("ep1-"), "stable id shape: {code_id}");

    // Cursor paging: limit=1 walks the same two episodes one at a time.
    let page1 = structured(
        &client
            .tools_call("episode_list", json!({"limit": 1}))
            .await?,
    )?;
    println!("readback=episode_list scenario=page1 {page1}");
    assert_eq!(page1["episodes"][0]["episode_id"], json!(code_id.clone()));
    assert_eq!(page1["stopped_because"], "limit_reached");
    let cursor = page1["next_cursor"].as_str().context("cursor")?.to_owned();
    let page2 = structured(
        &client
            .tools_call("episode_list", json!({"limit": 1, "cursor": cursor}))
            .await?,
    )?;
    println!("readback=episode_list scenario=page2 {page2}");
    assert_eq!(page2["episodes"][0]["episode_id"], json!(chrome_id.clone()));
    let cursor2 = page2["next_cursor"].as_str().context("cursor2")?.to_owned();
    let page3 = structured(
        &client
            .tools_call("episode_list", json!({"limit": 1, "cursor": cursor2}))
            .await?,
    )?;
    println!("readback=episode_list scenario=page3 {page3}");
    assert_eq!(page3["episodes"], json!([]));
    assert!(page3.get("next_cursor").is_none());

    // Filters: app, min_duration, actor, and inclusive overlap windows.
    let by_app = structured(
        &client
            .tools_call("episode_list", json!({"apps": ["CHROME.exe"]}))
            .await?,
    )?;
    println!(
        "readback=episode_list scenario=app_filter count={}",
        by_app["episodes"].as_array().map_or(0, Vec::len)
    );
    assert_eq!(by_app["episodes"].as_array().map_or(0, Vec::len), 1);
    assert_eq!(
        by_app["episodes"][0]["episode_id"],
        json!(chrome_id.clone())
    );
    let by_duration = structured(
        &client
            .tools_call("episode_list", json!({"min_duration_ms": 200_000}))
            .await?,
    )?;
    assert_eq!(by_duration["episodes"].as_array().map_or(0, Vec::len), 1);
    let by_actor = structured(
        &client
            .tools_call("episode_list", json!({"actor": "agent"}))
            .await?,
    )?;
    assert_eq!(by_actor["episodes"], json!([]));
    // Window that only the chrome episode overlaps.
    let mid_window = structured(
        &client
            .tools_call(
                "episode_list",
                json!({"start_ts_ns": base + 130 * SEC, "end_ts_ns": base + 200 * SEC}),
            )
            .await?,
    )?;
    println!("readback=episode_list scenario=mid_window {mid_window}");
    assert_eq!(mid_window["episodes"].as_array().map_or(0, Vec::len), 1);
    assert_eq!(
        mid_window["episodes"][0]["episode_id"],
        json!(chrome_id.clone())
    );
    // Instant where one episode ends exactly as the other starts: inclusive
    // overlap returns both.
    let touch_point = structured(
        &client
            .tools_call(
                "episode_list",
                json!({"start_ts_ns": base + 120 * SEC, "end_ts_ns": base + 120 * SEC}),
            )
            .await?,
    )?;
    println!("readback=episode_list scenario=touch_point {touch_point}");
    assert_eq!(touch_point["episodes"].as_array().map_or(0, Vec::len), 2);

    // Edge: structured validation errors.
    for (params, fragment) in [
        (json!({"limit": 0}), "limit"),
        (json!({"start_ts_ns": 10, "end_ts_ns": 5}), "must be <="),
        (json!({"cursor": "zz-not-hex"}), "cursor"),
        (json!({"actor": "alien"}), "actor"),
    ] {
        let error = client.tools_call_error("episode_list", params).await?;
        let text = error.to_string();
        assert!(
            text.contains("TOOL_PARAMS_INVALID") && text.contains(fragment),
            "expected TOOL_PARAMS_INVALID with {fragment:?}, got {text}"
        );
    }

    // Happy path: id lookup returns the full episode and its evidence rows —
    // including the agent row segmentation excluded and the closing idle row.
    let got = structured(
        &client
            .tools_call("episode_get", json!({"episode_id": chrome_id.clone()}))
            .await?,
    )?;
    println!("readback=episode_get scenario=chrome {got}");
    assert_eq!(got["episode"]["episode_id"], json!(chrome_id.clone()));
    assert_eq!(got["episode"]["document"], "github.com");
    let refs = got["timeline_refs"].as_array().context("refs")?.clone();
    let ref_kinds = refs
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    println!("readback=episode_get scenario=chrome ref_kinds={ref_kinds:?}");
    assert_eq!(
        ref_kinds,
        ["focus_change", "browser_nav", "clipboard", "idle_start"]
    );
    assert_eq!(refs[2]["actor"], "agent:sess-test");
    assert_eq!(got["refs_stopped_because"], "range_complete");
    assert!(got.get("next_refs_cursor").is_none());

    // Ref paging: refs_limit=1 walks the same evidence rows one at a time.
    let mut paged_kinds = Vec::new();
    let mut refs_cursor: Option<String> = None;
    loop {
        let mut params = json!({"episode_id": chrome_id.clone(), "refs_limit": 1});
        if let Some(cursor) = refs_cursor.as_deref() {
            params["refs_cursor"] = json!(cursor);
        }
        let page = structured(&client.tools_call("episode_get", params).await?)?;
        for entry in page["timeline_refs"].as_array().context("paged refs")? {
            paged_kinds.push(entry["kind"].as_str().unwrap_or_default().to_owned());
        }
        match page["next_refs_cursor"].as_str() {
            Some(cursor) => refs_cursor = Some(cursor.to_owned()),
            None => break,
        }
        anyhow::ensure!(paged_kinds.len() <= 8, "ref paging must terminate");
    }
    println!("readback=episode_get scenario=ref_paging kinds={paged_kinds:?}");
    assert_eq!(
        paged_kinds,
        ["focus_change", "browser_nav", "clipboard", "idle_start"]
    );

    // Edge: a seek hint past the episode start fails loudly instead of
    // silently returning nothing.
    let hint_missed = client
        .tools_call_error(
            "episode_get",
            json!({"episode_id": code_id.clone(), "start_ts_ns": base + 130 * SEC}),
        )
        .await?;
    let hint_text = hint_missed.to_string();
    println!("readback=episode_get edge=hint_past_start {hint_text}");
    assert!(
        hint_text.contains("EPISODE_NOT_FOUND") && hint_text.contains("hint"),
        "expected EPISODE_NOT_FOUND mentioning the hint, got {hint_text}"
    );

    // Edge: corrupt derived state is a loud structured error, never a skip.
    let garbage = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_EPISODES,
                    "key_prefix": "ep-corrupt",
                    "rows": 1,
                    "value_bytes": 8,
                }),
            )
            .await?,
    )?;
    assert_eq!(garbage["rows_added"], 1);
    let corrupted = client.tools_call_error("episode_list", json!({})).await?;
    let corrupted_text = corrupted.to_string();
    println!("readback=episode_list edge=corrupt_derived_row {corrupted_text}");
    assert!(
        corrupted_text.contains("EPISODE_KEY_INVALID"),
        "expected EPISODE_KEY_INVALID, got {corrupted_text}"
    );

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown: the listed episodes are the
    // physical CF_EPISODES rows (plus the planted corruption).
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_EPISODES)?;
    println!(
        "readback=episode_query edge=physical_sot rows={}",
        rows.len()
    );
    assert_eq!(rows.len(), 3, "2 episodes + 1 planted corrupt row");
    let mut physical_ids = Vec::new();
    for (key, value) in &rows {
        if episode_codec::decode_episode_key(key).is_ok() {
            let record: EpisodeRecord = decode_json(value)?;
            physical_ids.push(record.episode_id);
        }
    }
    assert_eq!(physical_ids, [code_id, chrome_id]);
    Ok(())
}
