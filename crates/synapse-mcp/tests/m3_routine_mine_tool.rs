//! `routine_mine` tool integration regression (#848): real daemon, real
//! `RocksDB`, real MCP calls, real pipeline. Seeds five synthetic days of
//! timeline rows containing a planted morning routine (outlook → excel →
//! teams around 09:00) plus single-day noise, segments them with the real
//! `episode_segment` tool, mines with `routine_mine`, exercises dry-run,
//! idempotency, support gating, disk-pressure refusal, and validation
//! errors, then reopens the database after shutdown and decodes the
//! physical `CF_ROUTINES` rows — cross-checking the persisted evidence
//! links against physical `CF_EPISODES` rows.

use std::collections::BTreeSet;

use anyhow::Context;
use chrono::{Days, Local};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::{EpisodeRecord, RoutineRecord};
use synapse_storage::{Db, cf, decode_json, routines as routine_codec};
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
    let naive = date
        .and_hms_opt(hour, minute, 0)
        .context("time must exist")?;
    let instant = chrono::TimeZone::from_local_datetime(&Local, &naive)
        .earliest()
        .context("local time unresolvable")?;
    Ok(u64::try_from(
        instant.timestamp_nanos_opt().context("ts out of range")?,
    )?)
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
    seed_row(
        client,
        prefix,
        ts_ns,
        json!({"record_version": 1, "kind": "focus_change", "actor": {"actor": "human"},
               "app": app,
               "payload": {"title": title, "pid": 7, "hwnd": 11, "source": "event"}}),
    )
    .await
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

fn routine_cf_count(inspect: &Value) -> u64 {
    inspect["cf_row_counts"][cf::CF_ROUTINES]
        .as_u64()
        .unwrap_or(0)
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered happy-path + edge-case narrative against a single live daemon"
)]
async fn routine_mine_mines_planted_routine_and_persists() -> anyhow::Result<()> {
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

    // Edge: empty episode store is a structured no-op, not an error.
    let empty = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    println!(
        "readback=routine_mine edge=empty written={} deleted={} routines={}",
        empty["routines_written"],
        empty["routines_deleted"],
        empty["routines"].as_array().map_or(0, Vec::len)
    );
    assert_eq!(empty["routines_written"], 0);
    assert_eq!(empty["routines_deleted"], 0);

    // Planted ground truth: five consecutive past days with a morning
    // routine (outlook 2 min → excel 5 min → teams 2 min, then idle) whose
    // start jitters ±10 min around 09:00, plus one single-day noise app.
    let jitter_min: [i64; 5] = [0, 5, -5, 10, -10];
    for (index, jitter) in jitter_min.iter().enumerate() {
        let days_ago = u64::try_from(5 - index)?; // oldest first
        let base = u64::try_from(
            i64::try_from(local_ts_ns(days_ago, 9, 0)?)? + jitter * 60 * 1_000_000_000,
        )?;
        let tag = format!("d{days_ago}");
        seed_focus(
            &mut client,
            &format!("{tag}-outlook"),
            base,
            "outlook.exe",
            "Inbox - Outlook",
        )
        .await?;
        seed_focus(
            &mut client,
            &format!("{tag}-excel"),
            base + 2 * MIN,
            "excel.exe",
            "report.xlsx - Excel",
        )
        .await?;
        seed_focus(
            &mut client,
            &format!("{tag}-teams"),
            base + 7 * MIN,
            "teams.exe",
            "Chat - Teams",
        )
        .await?;
        seed_row(
            &mut client,
            &format!("{tag}-idle"),
            base + 9 * MIN,
            json!({"record_version": 1, "kind": "idle_start", "actor": {"actor": "human"},
                   "payload": {"idle_ms_at_detection": 180_000, "idle_timeout_ms": 180_000}}),
        )
        .await?;
        if index == 0 {
            // Single-day noise: must never be promoted.
            let noise = local_ts_ns(days_ago, 14, 0)?;
            seed_focus(
                &mut client,
                &format!("{tag}-noise"),
                noise,
                "spotify.exe",
                "Spotify",
            )
            .await?;
            seed_row(
                &mut client,
                &format!("{tag}-noise-idle"),
                noise + 5 * MIN,
                json!({"record_version": 1, "kind": "idle_start", "actor": {"actor": "human"},
                       "payload": {"idle_ms_at_detection": 180_000, "idle_timeout_ms": 180_000}}),
            )
            .await?;
        }
    }

    // Real pipeline: derive episodes with the real segmentation tool.
    let segmented = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!(
        "readback=episode_segment edge=pipeline written={} days={}",
        segmented["episodes_written"], segmented["days_processed"]
    );
    assert_eq!(segmented["episodes_written"], 16); // 3 per day + noise on day 1
    assert_eq!(segmented["stopped_because"], "range_complete");

    // Edge: dry_run computes but never mutates CF_ROUTINES.
    let dry = structured(
        &client
            .tools_call("routine_mine", json!({"dry_run": true}))
            .await?,
    )?;
    println!(
        "readback=routine_mine edge=dry_run routines={} written={}",
        dry["routines"].as_array().map_or(0, Vec::len),
        dry["routines_written"]
    );
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["routines_written"], 0);
    assert_eq!(dry["routines"].as_array().map_or(0, Vec::len), 1);
    let after_dry = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(routine_cf_count(&after_dry), 0, "dry_run must not mutate");

    // Real mine: exactly one routine survives — the maximal three-step
    // document-level template; every sub-pattern and the single-day noise
    // are rejected, visibly, in the counters.
    let first = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    println!("readback=routine_mine edge=first {first}");
    assert_eq!(first["routines_written"], 1);
    assert_eq!(first["routines_deleted"], 0);
    assert_eq!(first["active_days"], 5);
    assert!(
        first["candidates_rejected_as_subpattern"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(first["clusters_rejected_low_support"].as_u64().unwrap_or(0) > 0);
    let mined = first["routines"][0].clone();
    let routine_id = mined["routine_id"]
        .as_str()
        .context("routine_id")?
        .to_owned();
    println!(
        "readback=routine_mine edge=first_routine id={routine_id} label={} support={} confidence={}",
        mined["schedule_label"], mined["support_days"], mined["confidence"]
    );
    assert_eq!(mined["support_days"], 5);
    assert_eq!(mined["opportunity_days"], 5);
    assert_eq!(mined["granularity"], "app_document");
    let apps: Vec<&str> = mined["steps"]
        .as_array()
        .context("steps")?
        .iter()
        .map(|step| step["app"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(apps, ["outlook.exe", "excel.exe", "teams.exe"]);
    let mean = mined["mean_minute_of_day"].as_u64().context("mean")?;
    assert!(
        (535..=545).contains(&mean),
        "mean {mean} must be near 09:00"
    );
    let confidence = mined["confidence"].as_f64().context("confidence")?;
    assert!(
        confidence > 0.5,
        "5/5 support must score high: {confidence}"
    );

    let after_first = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    println!(
        "readback=storage_inspect edge=after_first routines_rows={}",
        routine_cf_count(&after_first)
    );
    assert_eq!(routine_cf_count(&after_first), 1);

    // Idempotency: re-mining replaces the store and reproduces the same id.
    let second = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    println!(
        "readback=routine_mine edge=re_mine written={} deleted={} id={}",
        second["routines_written"], second["routines_deleted"], second["routines"][0]["routine_id"]
    );
    assert_eq!(second["routines_written"], 1);
    assert_eq!(second["routines_deleted"], 1);
    assert_eq!(
        second["routines"][0]["routine_id"],
        json!(routine_id.clone())
    );

    // Edge: a support floor above the data honestly empties the store —
    // derived state always reflects exactly the latest mine.
    let high_support = structured(
        &client
            .tools_call("routine_mine", json!({"min_support_days": 6}))
            .await?,
    )?;
    println!("readback=routine_mine edge=high_support {high_support}");
    assert_eq!(high_support["routines_written"], 0);
    assert_eq!(high_support["routines_deleted"], 1);
    assert!(
        high_support["clusters_rejected_low_support"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    let after_high = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    assert_eq!(routine_cf_count(&after_high), 0);

    // Restore the routine for the physical post-shutdown readback.
    let restored = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    assert_eq!(restored["routines_written"], 1);

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
    let refused = client.tools_call_error("routine_mine", json!({})).await?;
    let refused_text = refused.to_string();
    println!("readback=routine_mine edge=pressure_refusal {refused_text}");
    assert!(
        refused_text.contains("disk pressure"),
        "expected pressure refusal, got {refused_text}"
    );
    let dry_under_pressure = structured(
        &client
            .tools_call("routine_mine", json!({"dry_run": true}))
            .await?,
    )?;
    assert_eq!(
        dry_under_pressure["routines"]
            .as_array()
            .map_or(0, Vec::len),
        1
    );
    let _released = structured(
        &client
            .tools_call(
                "storage_pressure_sample",
                json!({"free_bytes": 1_000_000_000_000_u64}),
            )
            .await?,
    )?;

    // Edge: structured validation errors.
    for bad in [
        json!({"start_ts_ns": 10, "end_ts_ns": 5}),
        json!({"min_support_days": 0}),
        json!({"max_pattern_len": 99}),
    ] {
        let invalid = client.tools_call_error("routine_mine", bad.clone()).await?;
        let invalid_text = invalid.to_string();
        println!("readback=routine_mine edge=invalid params={bad} error={invalid_text}");
        assert!(
            invalid_text.contains("TOOL_PARAMS_INVALID"),
            "expected TOOL_PARAMS_INVALID, got {invalid_text}"
        );
    }

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let episode_rows = reopened.scan_cf(cf::CF_EPISODES)?;
    let known_episode_ids: BTreeSet<String> = episode_rows
        .iter()
        .map(|(_key, value)| decode_json::<EpisodeRecord>(value).map(|record| record.episode_id))
        .collect::<Result<_, _>>()?;
    let rows = reopened.scan_cf(cf::CF_ROUTINES)?;
    println!(
        "readback=routine_mine edge=physical_sot routine_rows={} episode_rows={}",
        rows.len(),
        episode_rows.len()
    );
    assert_eq!(rows.len(), 1);
    let (key, value) = &rows[0];
    let key_id = routine_codec::decode_routine_key(key)?;
    let record: RoutineRecord = decode_json(value)?;
    println!(
        "readback=cf_routines key_id={key_id} id={} label={} steps={:?} support={} \
         occurrences={} opportunities={} confidence={:.3} evidence={}",
        record.routine_id,
        record.schedule_label,
        record
            .steps
            .iter()
            .map(|step| step.app.as_str())
            .collect::<Vec<_>>(),
        record.support_days,
        record.occurrence_count,
        record.opportunity_days,
        record.confidence,
        record.evidence.len()
    );
    assert_eq!(key_id, record.routine_id, "key must mirror the record");
    assert_eq!(record.routine_id, routine_id, "re-mined id must be stable");
    assert_eq!(record.support_days, 5);
    assert_eq!(record.evidence.len(), 5);
    assert_eq!(
        record.steps[1].document.as_deref(),
        Some("report.xlsx - excel"),
        "document identity must survive into the persisted template"
    );
    // Evidence links must resolve to physical CF_EPISODES rows.
    for evidence in &record.evidence {
        assert_eq!(evidence.episode_ids.len(), 3);
        for episode_id in &evidence.episode_ids {
            assert!(
                known_episode_ids.contains(episode_id),
                "evidence episode {episode_id} missing from physical CF_EPISODES"
            );
        }
    }
    Ok(())
}
