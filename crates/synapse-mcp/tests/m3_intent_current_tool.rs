//! `intent_current` integration FSV (#854): real daemon, real `RocksDB`, real
//! MCP calls. Plants a mineable morning routine (outlook → excel → teams) over
//! seven consecutive days, mines it, confirms it, then drives the live
//! intent-matcher surface against synthetic "as of" instants:
//!
//! * Happy path — replays the first two steps of the routine on a later day at
//!   its usual time and asserts the routine is the top-1 candidate with the
//!   matched steps (whose episode ids resolve via `episode_get`, the physical
//!   source of truth), the remaining-step preview, and the schedule context.
//! * Honest empty — unrelated activity yields zero candidates (no forced top-1).
//! * Stale — activity older than the freshness window is not a live intent.
//! * Disabled — a disabled routine never matches.
//!
//! Finishes with a post-shutdown physical `CF_ROUTINES`/`CF_ROUTINE_STATE`
//! readback as the source of truth.

use anyhow::Context;
use chrono::{Datelike, Days, Local, TimeZone, Timelike};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SEC: u64 = 1_000_000_000;
const MIN: u64 = 60 * SEC;

/// Local `hour:minute` of the day `days_ago` days before today, as ns.
fn local_ts_ns(days_ago: u64, hour: u32, minute: u32) -> anyhow::Result<u64> {
    let date = Local::now()
        .date_naive()
        .checked_sub_days(Days::new(days_ago))
        .context("date arithmetic")?;
    let naive = date.and_hms_opt(hour, minute, 0).context("time must exist")?;
    let instant = chrono::TimeZone::from_local_datetime(&Local, &naive)
        .earliest()
        .context("local time unresolvable")?;
    Ok(u64::try_from(
        instant.timestamp_nanos_opt().context("ts out of range")?,
    )?)
}

/// Local weekday (0=Mon..6=Sun) and minute-of-day for a ns instant.
fn local_dow_minute(ts_ns: u64) -> anyhow::Result<(u8, u32)> {
    let dt = Local.timestamp_nanos(i64::try_from(ts_ns)?);
    let weekday = u8::try_from(dt.weekday().num_days_from_monday())?;
    let minute = dt.hour() * 60 + dt.minute();
    Ok((weekday, minute))
}

fn structured(result: &Value) -> anyhow::Result<Value> {
    result
        .get("structuredContent")
        .cloned()
        .with_context(|| format!("missing structuredContent in {result}"))
}

async fn seed_focus(
    client: &mut StdioMcpClient,
    prefix: &str,
    ts_ns: u64,
    app: &str,
    title: &str,
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
                    "value_json": {"record_version": 1, "kind": "focus_change",
                                   "actor": {"actor": "human"}, "app": app,
                                   "payload": {"title": title, "pid": 7, "hwnd": 11,
                                               "source": "event"}},
                    "ts_ns_start": ts_ns,
                    "key_mode": "timeline_ts",
                }),
            )
            .await?,
    )?;
    anyhow::ensure!(put["rows_added"] == 1, "seed {prefix} failed: {put}");
    Ok(())
}

async fn seed_idle(client: &mut StdioMcpClient, prefix: &str, ts_ns: u64) -> anyhow::Result<()> {
    let put = structured(
        &client
            .tools_call(
                "storage_put_probe_rows",
                json!({
                    "cf_name": cf::CF_TIMELINE,
                    "key_prefix": prefix,
                    "rows": 1,
                    "value_bytes": 0,
                    "value_json": {"record_version": 1, "kind": "idle_start",
                                   "actor": {"actor": "human"},
                                   "payload": {"idle_ms_at_detection": 180_000,
                                               "idle_timeout_ms": 180_000}},
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
#[allow(
    clippy::too_many_lines,
    reason = "one ordered happy-path + edge-case narrative against a single live daemon"
)]
async fn intent_current_matches_live_routine_prefix_and_is_honest() -> anyhow::Result<()> {
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

    // Edge: empty stores yield an honest empty snapshot, never an error.
    let empty = structured(&client.tools_call("intent_current", json!({})).await?)?;
    println!("readback=intent_current edge=empty {empty}");
    assert_eq!(empty["candidates"].as_array().map_or(1, Vec::len), 0);
    assert_eq!(empty["evaluated_routines"], 0);

    // Ground truth: seven consecutive days of outlook -> excel -> teams at
    // 09:00. Seven distinct weekdays makes the mined day-of-week class `daily`,
    // so the later "now" day always falls inside it regardless of when this
    // test runs.
    for index in 0..7_u64 {
        let days_ago = 8 - index;
        let base = local_ts_ns(days_ago, 9, 0)?;
        let tag = format!("d{days_ago}");
        seed_focus(&mut client, &format!("{tag}-o"), base, "outlook.exe", "Inbox - Outlook").await?;
        seed_focus(&mut client, &format!("{tag}-x"), base + 2 * MIN, "excel.exe", "report.xlsx - Excel").await?;
        seed_focus(&mut client, &format!("{tag}-t"), base + 7 * MIN, "teams.exe", "Chat - Teams").await?;
        seed_idle(&mut client, &format!("{tag}-i"), base + 9 * MIN).await?;
    }

    // The "now" day (yesterday): only the first two steps, closed by idle, plus
    // an unrelated notepad burst later in the day for the honest-empty case.
    let now_day_base = local_ts_ns(1, 9, 0)?;
    seed_focus(&mut client, "now-o", now_day_base, "outlook.exe", "Inbox - Outlook").await?;
    seed_focus(&mut client, "now-x", now_day_base + 2 * MIN, "excel.exe", "report.xlsx - Excel").await?;
    seed_idle(&mut client, "now-i", now_day_base + 4 * MIN).await?;
    let notepad_ts = local_ts_ns(1, 11, 0)?;
    seed_focus(&mut client, "now-n", notepad_ts, "notepad.exe", "untitled - Notepad").await?;
    seed_idle(&mut client, "now-ni", notepad_ts + 3 * MIN).await?;

    let segmented = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!("readback=episode_segment written={}", segmented["episodes_written"]);
    // 7 mining days x 3 episodes + now-day (outlook, excel, notepad).
    assert_eq!(segmented["episodes_written"], 24);

    // Mine only the seven full days (exclude the partial now day) so exactly
    // one routine — the 3-step morning sequence — is derived. The mine end is
    // snapped UP to the next local midnight, so the exclusive bound must be the
    // now day's midnight (not its 09:00 activity) to keep the now day out.
    let mine_end = local_ts_ns(1, 0, 0)?;
    let mined = structured(
        &client
            .tools_call("routine_mine", json!({"end_ts_ns": mine_end}))
            .await?,
    )?;
    println!(
        "readback=routine_mine written={} steps={}",
        mined["routines_written"], mined["routines"][0]["steps"]
    );
    assert_eq!(mined["routines_written"], 1);
    let routine_id = mined["routines"][0]["routine_id"].as_str().context("routine_id")?.to_owned();
    let steps = mined["routines"][0]["steps"].as_array().context("steps")?;
    assert_eq!(steps.len(), 3, "expected the 3-step morning routine: {steps:?}");
    assert_eq!(steps[0]["app"], "outlook.exe");
    assert_eq!(steps[1]["app"], "excel.exe");
    assert_eq!(steps[2]["app"], "teams.exe");

    // Confirm it (the matcher accepts candidates too, but confirm exercises the
    // confirmed lifecycle path and the operator-label echo).
    structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id, "action": "confirm"}),
            )
            .await?,
    )?;

    // === Happy path: first two steps replayed at the usual time. ===
    // now = 09:06 on the now day: 2 min after excel's step closed (idle 09:04).
    let now_happy = now_day_base + 6 * MIN;
    let (now_weekday, now_minute) = local_dow_minute(now_happy)?;
    let live = structured(
        &client
            .tools_call("intent_current", json!({"now_ts_ns": now_happy}))
            .await?,
    )?;
    println!("readback=intent_current scenario=live {live}");
    assert_eq!(live["evaluated_routines"], 1);
    assert_eq!(live["considered_episodes"], 2, "only the now-day morning prefix is recent");
    assert_eq!(live["now"]["weekday"], i64::from(now_weekday));
    assert_eq!(live["now"]["minute_of_day"], i64::from(now_minute));
    let candidates = live["candidates"].as_array().context("candidates")?;
    assert_eq!(candidates.len(), 1, "exactly the morning routine matches");
    let top = &candidates[0];
    assert_eq!(top["routine_id"], routine_id.as_str());
    assert_eq!(top["lifecycle"], "confirmed");
    assert_eq!(top["matched_prefix_len"], 2);
    assert_eq!(top["total_steps"], 3);
    assert_eq!(top["matched_steps"][0]["app"], "outlook.exe");
    assert_eq!(top["matched_steps"][1]["app"], "excel.exe");
    assert_eq!(top["remaining_steps"].as_array().map(Vec::len), Some(1));
    assert_eq!(top["remaining_steps"][0]["app"], "teams.exe");
    assert_eq!(top["schedule"]["dow_match"], true);
    assert_eq!(top["schedule"]["within_tolerance"], true);
    assert!(top["confidence"].as_f64().context("confidence")? > 0.0);

    // FSV: the matched-step episode ids are physical CF_EPISODES rows — resolve
    // the first one through episode_get and confirm app + identity.
    let matched_episode_id = top["matched_steps"][0]["episode_id"].as_str().context("episode_id")?;
    let got = structured(
        &client
            .tools_call("episode_get", json!({"episode_id": matched_episode_id}))
            .await?,
    )?;
    println!("readback=episode_get matched_step_episode {}", got["episode"]);
    assert_eq!(got["episode"]["episode_id"], matched_episode_id);
    assert_eq!(got["episode"]["app"], "outlook.exe");

    // === Honest empty: unrelated activity (notepad) at 11:07. ===
    let now_notepad = notepad_ts + 7 * MIN;
    let unrelated = structured(
        &client
            .tools_call("intent_current", json!({"now_ts_ns": now_notepad}))
            .await?,
    )?;
    println!("readback=intent_current scenario=unrelated {unrelated}");
    assert_eq!(
        unrelated["candidates"].as_array().map_or(1, Vec::len),
        0,
        "random activity must not force a top-1"
    );

    // === Stale: the morning prefix queried hours later is not a live intent. ===
    let now_stale = now_day_base + 6 * 60 * MIN; // 15:00, well past the 30-min freshness window
    let stale = structured(
        &client
            .tools_call("intent_current", json!({"now_ts_ns": now_stale}))
            .await?,
    )?;
    println!("readback=intent_current scenario=stale {stale}");
    assert_eq!(stale["candidates"].as_array().map_or(1, Vec::len), 0, "stale activity is not live");

    // === Disabled routines never match. ===
    structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id, "action": "disable"}),
            )
            .await?,
    )?;
    let after_disable = structured(
        &client
            .tools_call("intent_current", json!({"now_ts_ns": now_happy}))
            .await?,
    )?;
    println!("readback=intent_current scenario=disabled {after_disable}");
    assert_eq!(after_disable["evaluated_routines"], 1, "the routine is still evaluated");
    assert_eq!(
        after_disable["candidates"].as_array().map_or(1, Vec::len),
        0,
        "a disabled routine must never match"
    );

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown: exactly one mined routine row.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let routine_rows = reopened.scan_cf(cf::CF_ROUTINES)?;
    let state_rows = reopened.scan_cf(cf::CF_ROUTINE_STATE)?;
    println!(
        "readback=physical_sot routines={} state={}",
        routine_rows.len(),
        state_rows.len()
    );
    assert_eq!(routine_rows.len(), 1, "one routine on disk");
    assert_eq!(state_rows.len(), 1, "one lifecycle row on disk");
    Ok(())
}
