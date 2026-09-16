//! Supporting integration evidence for `intent_detect_tick` and the intent-event
//! bus (#855): real daemon, `RocksDB`, event bus, reflex runtime, and MCP calls.
//! Manual FSV remains separate.
//!
//! Plants a mineable morning routine (outlook → excel → teams) over seven days,
//! mines and confirms it, registers a planted `on_event` reflex keyed on the
//! `intent-detected` event kind, then drives the push-based detector through its
//! full state machine against synthetic "as of" instants and verifies every
//! transition both in the tool response AND in physical state:
//!
//! * **detected** — replaying the routine's first two steps at its usual time
//!   publishes exactly one `intent-detected`; the planted reflex FIRES, leaving
//!   a `CF_REFLEX_AUDIT` row whose `trigger_kind` is `intent-detected` (the
//!   physical source of truth that the event reached the bus and matched).
//! * **abandoned** — querying the same partial routine long after the freshness
//!   window publishes exactly one `intent-abandoned` (operator diverged).
//! * **detected + confirmed** — once the third step is observed too, a fresh
//!   tick publishes `intent-detected` then `intent-confirmed` (every step done).
//! * **confirmed leaves silently** — a confirmed routine that drops out of the
//!   live set publishes nothing (it completed, it was not abandoned).
//!
//! Edge cases: an empty store and a no-activity instant both yield zero
//! transitions (honest empty, never a forced event). Finishes with a
//! post-shutdown physical readback of `CF_REFLEX_AUDIT`, `CF_ROUTINES`, and
//! `CF_ROUTINE_STATE`.

use anyhow::Context;
use chrono::{Days, Local};
use serde_json::{Value, json};
use synapse_core::{SCHEMA_VERSION, StoredReflexAudit};
use synapse_storage::{Db, cf, decode_json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SEC: u64 = 1_000_000_000;
const MIN: u64 = 60 * SEC;

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

/// One `intent_detect_tick` at an explicit instant with a low floor (so the test
/// exercises the state machine, not the confidence threshold).
async fn tick(client: &mut StdioMcpClient, now_ts_ns: u64) -> anyhow::Result<Value> {
    structured(
        &client
            .tools_call(
                "intent_detect_tick",
                json!({"now_ts_ns": now_ts_ns, "min_confidence": 0.1}),
            )
            .await?,
    )
}

/// The transition kinds published by one tick, in order.
fn transition_kinds(outcome: &Value) -> Vec<String> {
    outcome["transitions"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row["kind"].as_str().unwrap_or("?").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Polls `reflex_history` for the planted reflex until a fired audit (an audit
/// whose `details.kind` is `reflex_fired`) appears, returning its trigger kind.
async fn wait_for_reflex_fired(
    client: &mut StdioMcpClient,
    reflex_id: &str,
) -> anyhow::Result<String> {
    for _ in 0..40 {
        let history = structured(
            &client
                .tools_call(
                    "reflex_history",
                    json!({"reflex_id": reflex_id, "limit": 50}),
                )
                .await?,
        )?;
        if let Some(fired) = history["events"].as_array().and_then(|events| {
            events
                .iter()
                .find(|event| event["details"]["kind"] == "reflex_fired")
        }) {
            return fired["details"]["trigger_kind"]
                .as_str()
                .map(str::to_owned)
                .context("fired audit missing trigger_kind");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    anyhow::bail!("reflex {reflex_id} never fired within the poll budget")
}

fn read_audits(db_path: &std::path::Path) -> anyhow::Result<Vec<StoredReflexAudit>> {
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    db.scan_cf(cf::CF_REFLEX_AUDIT)?
        .into_iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(&value).map_err(Into::into))
        .collect()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered happy-path + edge-case narrative against a single live daemon"
)]
async fn intent_detect_tick_drives_transitions_and_fires_on_event_reflex() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path_string = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[
            ("SYNAPSE_DEBUG_TOOLS", "1"),
            ("SYNAPSE_DB", db_path_string.as_str()),
            // Inject a deterministic synthetic notepad foreground (matches the
            // notepad profile activated below). reflex_register runs the
            // supported-use scope gate, which reads the current foreground via
            // current_audit_foreground(); without a synthetic fixture it calls the
            // real GetForegroundWindow() and fails with A11Y_NO_FOREGROUND whenever
            // the host has no focused window (locked screen, focus elsewhere,
            // unattended run) — an ambient-state dependency that has nothing to do
            // with what this test verifies. The fixture makes the foreground a
            // controlled input. See also the product follow-up issue on
            // registration-time foreground coupling.
            ("SYNAPSE_MCP_SYNTHETIC_FIXTURE", "notepad"),
        ],
    )
    .await?;

    // An on_event reflex's action runs through the action scope gate; activate a
    // known profile so firing has a scope, exactly like the reflex_history
    // regression coverage.
    structured(
        &client
            .tools_call("profile_activate", json!({"profile_id": "notepad"}))
            .await?,
    )?;

    // Edge: empty stores yield an honest-empty tick — zero candidates, zero
    // transitions, never an error or a forced event.
    let empty = tick(&mut client, local_ts_ns(1, 9, 6)?).await?;
    println!("readback=intent_detect_tick edge=empty {empty}");
    assert_eq!(empty["candidates"], 0);
    assert_eq!(empty["events_published"], 0);
    assert!(transition_kinds(&empty).is_empty());

    // Ground truth: seven consecutive days of outlook -> excel -> teams at 09:00.
    for index in 0..7_u64 {
        let days_ago = 8 - index;
        let base = local_ts_ns(days_ago, 9, 0)?;
        let tag = format!("d{days_ago}");
        seed_focus(
            &mut client,
            &format!("{tag}-o"),
            base,
            "outlook.exe",
            "Inbox - Outlook",
        )
        .await?;
        seed_focus(
            &mut client,
            &format!("{tag}-x"),
            base + 2 * MIN,
            "excel.exe",
            "report.xlsx - Excel",
        )
        .await?;
        seed_focus(
            &mut client,
            &format!("{tag}-t"),
            base + 7 * MIN,
            "teams.exe",
            "Chat - Teams",
        )
        .await?;
        seed_idle(&mut client, &format!("{tag}-i"), base + 9 * MIN).await?;
    }

    let now_day_base = local_ts_ns(1, 9, 0)?;
    let segmented = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    println!(
        "readback=episode_segment written={}",
        segmented["episodes_written"]
    );
    assert!(segmented["episodes_written"].as_u64().unwrap_or(0) >= 21);

    // Mine only the seven full days; end snapped UP to the now day's midnight.
    let mine_end = local_ts_ns(1, 0, 0)?;
    let mined = structured(
        &client
            .tools_call("routine_mine", json!({"end_ts_ns": mine_end}))
            .await?,
    )?;
    println!(
        "readback=routine_mine written={}",
        mined["routines_written"]
    );
    assert_eq!(mined["routines_written"], 1);
    let routine_id = mined["routines"][0]["routine_id"]
        .as_str()
        .context("routine_id")?
        .to_owned();
    structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id, "action": "confirm"}),
            )
            .await?,
    )?;

    // Plant a no-op on_event reflex keyed on the intent-detected event kind.
    let registered = structured(
        &client
            .tools_call(
                "reflex_register",
                json!({
                    "kind": "on_event",
                    "when": {"op": "kind", "kind": "intent-detected"},
                    "then": {"kind": "action", "action": {"kind": "release_all"}},
                }),
            )
            .await?,
    )?;
    let reflex_id = registered["reflex_id"]
        .as_str()
        .context("reflex_id")?
        .to_owned();
    println!("readback=reflex_register reflex_id={reflex_id}");

    // Edge: a no-activity instant on the now day (no now-day episodes seeded yet)
    // is still honest-empty even though a routine exists.
    let quiet = tick(&mut client, now_day_base + 6 * MIN).await?;
    println!("readback=intent_detect_tick edge=quiet {quiet}");
    assert_eq!(quiet["candidates"], 0);
    assert!(transition_kinds(&quiet).is_empty());

    // === DETECTED: replay the first two steps at the usual time. ===
    seed_focus(
        &mut client,
        "now-o",
        now_day_base,
        "outlook.exe",
        "Inbox - Outlook",
    )
    .await?;
    seed_focus(
        &mut client,
        "now-x",
        now_day_base + 2 * MIN,
        "excel.exe",
        "report.xlsx - Excel",
    )
    .await?;
    seed_idle(&mut client, "now-i", now_day_base + 4 * MIN).await?;
    structured(&client.tools_call("episode_segment", json!({})).await?)?;

    let detected = tick(&mut client, now_day_base + 6 * MIN).await?;
    println!("readback=intent_detect_tick scenario=detected {detected}");
    assert_eq!(detected["candidates"], 1);
    assert_eq!(transition_kinds(&detected), vec!["detected"]);
    assert_eq!(
        detected["transitions"][0]["routine_id"],
        routine_id.as_str()
    );
    assert_eq!(detected["transitions"][0]["matched_prefix_len"], 2);
    assert_eq!(detected["transitions"][0]["total_steps"], 3);
    assert_eq!(detected["transitions"][0]["reason"], "prefix_match");
    assert_eq!(detected["events_published"], 1);
    // The reflex scheduler holds a live bus subscription, so the published event
    // reached at least one subscriber.
    assert!(
        detected["events_matched_subscribers"].as_u64().unwrap_or(0) >= 1,
        "intent-detected must reach the reflex subscriber: {detected}"
    );

    // Supporting integration readback: the planted reflex actually fired on
    // the published event — the physical CF_REFLEX_AUDIT row names
    // intent-detected as its trigger. Manual FSV remains separate.
    let trigger_kind = wait_for_reflex_fired(&mut client, &reflex_id).await?;
    println!("readback=reflex_fired trigger_kind={trigger_kind}");
    assert_eq!(trigger_kind, "intent-detected");

    // === ABANDONED: the same partial routine, long past the freshness window. ===
    let abandoned = tick(&mut client, now_day_base + 40 * MIN).await?;
    println!("readback=intent_detect_tick scenario=abandoned {abandoned}");
    assert_eq!(
        abandoned["candidates"], 0,
        "stale activity is not a live intent"
    );
    assert_eq!(transition_kinds(&abandoned), vec!["abandoned"]);
    assert_eq!(
        abandoned["transitions"][0]["routine_id"],
        routine_id.as_str()
    );
    assert_eq!(abandoned["transitions"][0]["reason"], "diverged_or_stale");
    // The abandonment carries the last-known evidence, not zeroes.
    assert_eq!(abandoned["transitions"][0]["matched_prefix_len"], 2);
    assert_eq!(abandoned["tracked"], 0);

    // === DETECTED + CONFIRMED: observe the third step too, then a fresh tick. ===
    seed_focus(
        &mut client,
        "now-t",
        now_day_base + 7 * MIN,
        "teams.exe",
        "Chat - Teams",
    )
    .await?;
    seed_idle(&mut client, "now-ti", now_day_base + 9 * MIN).await?;
    structured(&client.tools_call("episode_segment", json!({})).await?)?;

    let confirmed = tick(&mut client, now_day_base + 12 * MIN).await?;
    println!("readback=intent_detect_tick scenario=confirmed {confirmed}");
    assert_eq!(confirmed["candidates"], 1);
    // Re-detected (the abandon removed it) and immediately confirmed: all steps.
    assert_eq!(transition_kinds(&confirmed), vec!["detected", "confirmed"]);
    let confirm_row = confirmed["transitions"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["kind"] == "confirmed"))
        .context("confirmed transition missing")?;
    assert_eq!(confirm_row["routine_id"], routine_id.as_str());
    assert_eq!(confirm_row["matched_prefix_len"], 3);
    assert_eq!(confirm_row["total_steps"], 3);
    assert_eq!(confirm_row["reason"], "all_steps_completed");

    // === CONFIRMED LEAVES SILENTLY: a completed routine going stale is not an
    // abandonment. ===
    let after = tick(&mut client, now_day_base + 60 * MIN).await?;
    println!("readback=intent_detect_tick scenario=confirmed_then_stale {after}");
    assert_eq!(after["candidates"], 0);
    assert!(
        transition_kinds(&after).is_empty(),
        "a completed routine that leaves the live set must publish nothing: {after}"
    );
    assert_eq!(after["tracked"], 0);

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown.
    let audits = read_audits(&db_path)?;
    let fired: Vec<&StoredReflexAudit> = audits
        .iter()
        .filter(|audit| audit.details["kind"] == "reflex_fired")
        .collect();
    println!(
        "readback=physical_sot reflex_audits={} fired={}",
        audits.len(),
        fired.len()
    );
    assert!(
        !fired.is_empty(),
        "at least one fired reflex audit must persist"
    );
    assert!(
        fired
            .iter()
            .all(|audit| audit.details["trigger_kind"] == "intent-detected"),
        "every fired audit must name intent-detected as its trigger: {fired:?}"
    );

    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let routines = reopened.scan_cf(cf::CF_ROUTINES)?;
    let routine_states = reopened.scan_cf(cf::CF_ROUTINE_STATE)?;
    println!(
        "readback=physical_sot routines={} routine_state={}",
        routines.len(),
        routine_states.len()
    );
    assert_eq!(routines.len(), 1, "one mined routine on disk");
    assert_eq!(routine_states.len(), 1, "one lifecycle row on disk");
    Ok(())
}
