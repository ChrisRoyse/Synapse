//! Routine lifecycle integration regression (#849): real daemon, real
//! `RocksDB`, real MCP calls. Plants the #848 morning routine, mines it,
//! then drives the full lifecycle surface: miner-created candidate state
//! rows, `routine_list` filters, `routine_inspect` evidence links,
//! `routine_update` transitions (confirm/rename/disable/archive/enable)
//! with audit trails, miner non-re-promotion of disabled routines, unmined
//! flagging when the mine no longer derives the routine, disk-pressure
//! refusal, and structured validation errors — finishing with a physical
//! post-shutdown `CF_ROUTINE_STATE` readback as the source of truth.

use anyhow::Context;
use chrono::{Days, Local};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_core::types::{RoutineLifecycle, RoutineStateAction, RoutineStateRecord};
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

fn cf_count(inspect: &Value, cf_name: &str) -> u64 {
    inspect["cf_row_counts"][cf_name].as_u64().unwrap_or(0)
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered happy-path + edge-case narrative against a single live daemon"
)]
async fn routine_lifecycle_survives_mining_and_audits_transitions() -> anyhow::Result<()> {
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

    // Edge: empty stores list as empty, never as an error.
    let empty_list = structured(&client.tools_call("routine_list", json!({})).await?)?;
    println!("readback=routine_list edge=empty {empty_list}");
    assert_eq!(empty_list["total_mined"], 0);
    assert_eq!(empty_list["total_state_rows"], 0);
    assert_eq!(empty_list["entries"].as_array().map_or(1, Vec::len), 0);

    // Edge: unknown-but-well-formed id is a structured not-found.
    let missing = client
        .tools_call_error(
            "routine_inspect",
            json!({"routine_id": "rt1-00000000000000aa"}),
        )
        .await?
        .to_string();
    println!("readback=routine_inspect edge=not_found {missing}");
    assert!(missing.contains("ROUTINE_NOT_FOUND"), "{missing}");

    // Edge: malformed ids are parameter errors naming the codec rule.
    let malformed = client
        .tools_call_error("routine_inspect", json!({"routine_id": "rt1-NOPE"}))
        .await?
        .to_string();
    println!("readback=routine_inspect edge=malformed {malformed}");
    assert!(malformed.contains("ROUTINE_KEY_INVALID"), "{malformed}");

    // Planted ground truth (same shape as the #848 regression): five days
    // of outlook → excel → teams around 09:00.
    let jitter_min: [i64; 5] = [0, 5, -5, 10, -10];
    for (index, jitter) in jitter_min.iter().enumerate() {
        let days_ago = u64::try_from(5 - index)?;
        let base =
            u64::try_from(i64::try_from(local_ts_ns(days_ago, 9, 0)?)? + jitter * 60_000_000_000)?;
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
        seed_idle(&mut client, &format!("{tag}-idle"), base + 9 * MIN).await?;
    }
    let segmented = structured(&client.tools_call("episode_segment", json!({})).await?)?;
    assert_eq!(segmented["episodes_written"], 15);

    // Mine: the routine materializes WITH its candidate state row.
    let mined = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    println!(
        "readback=routine_mine edge=first written={} state_created={} state_updated={}",
        mined["routines_written"], mined["state_rows_created"], mined["state_rows_updated"]
    );
    assert_eq!(mined["routines_written"], 1);
    assert_eq!(mined["state_rows_created"], 1);
    assert_eq!(mined["state_rows_updated"], 0);
    assert_eq!(mined["state_rows_marked_unmined"], 0);
    let routine_id = mined["routines"][0]["routine_id"]
        .as_str()
        .context("routine_id")?
        .to_owned();
    let mined_confidence = mined["routines"][0]["confidence"]
        .as_f64()
        .context("confidence")?;

    let inspect_cf = structured(&client.tools_call("storage_inspect", json!({})).await?)?;
    println!(
        "readback=storage_inspect edge=after_mine routines={} state={}",
        cf_count(&inspect_cf, cf::CF_ROUTINES),
        cf_count(&inspect_cf, cf::CF_ROUTINE_STATE)
    );
    assert_eq!(cf_count(&inspect_cf, cf::CF_ROUTINES), 1);
    assert_eq!(cf_count(&inspect_cf, cf::CF_ROUTINE_STATE), 1);

    // Inspect: full record + miner-created candidate state with audit trail.
    let inspected = structured(
        &client
            .tools_call("routine_inspect", json!({"routine_id": routine_id.clone()}))
            .await?,
    )?;
    println!("readback=routine_inspect edge=candidate {inspected}");
    assert_eq!(inspected["mined"], true);
    assert_eq!(inspected["state_row_exists"], true);
    assert_eq!(inspected["state"]["lifecycle"], "candidate");
    assert_eq!(inspected["state"]["present_in_last_mine"], true);
    assert_eq!(inspected["state"]["transitions"][0]["action"], "discovered");
    assert_eq!(inspected["state"]["transitions"][0]["by"], "miner");
    assert_eq!(
        inspected["state"]["confidence_history"]
            .as_array()
            .map_or(0, Vec::len),
        1
    );
    assert_eq!(
        inspected["record"]["routine_id"],
        json!(routine_id.clone()),
        "inspect must return the full mined record"
    );

    // Evidence links must resolve through the real episode_get tool.
    let first_episode_id = inspected["record"]["evidence"][0]["episode_ids"][0]
        .as_str()
        .context("evidence episode id")?
        .to_owned();
    let episode = structured(
        &client
            .tools_call(
                "episode_get",
                json!({"episode_id": first_episode_id.clone()}),
            )
            .await?,
    )?;
    println!(
        "readback=episode_get edge=evidence_link id={} app={}",
        first_episode_id, episode["episode"]["app"]
    );
    assert_eq!(episode["episode"]["episode_id"], json!(first_episode_id));

    // Confirm: candidate → confirmed, read back from physical storage.
    let confirmed = structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id.clone(), "action": "confirm",
                       "note": "review: confirmed for regression"}),
            )
            .await?,
    )?;
    println!("readback=routine_update edge=confirm {confirmed}");
    assert_eq!(confirmed["lifecycle_before"], "candidate");
    assert_eq!(confirmed["lifecycle_after"], "confirmed");
    assert_eq!(confirmed["state_row_created"], false);
    assert_eq!(confirmed["state"]["lifecycle"], "confirmed");

    // Edge: confirming twice is an illegal transition, loudly.
    let double_confirm = client
        .tools_call_error(
            "routine_update",
            json!({"routine_id": routine_id.clone(), "action": "confirm"}),
        )
        .await?
        .to_string();
    println!("readback=routine_update edge=double_confirm {double_confirm}");
    assert!(
        double_confirm.contains("ROUTINE_TRANSITION_INVALID"),
        "{double_confirm}"
    );

    // Edge: label is rename-only; rename requires a label.
    let label_on_confirm = client
        .tools_call_error(
            "routine_update",
            json!({"routine_id": routine_id.clone(), "action": "disable", "label": "x"}),
        )
        .await?
        .to_string();
    assert!(
        label_on_confirm.contains("only valid for action=rename"),
        "{label_on_confirm}"
    );
    let rename_no_label = client
        .tools_call_error(
            "routine_update",
            json!({"routine_id": routine_id.clone(), "action": "rename"}),
        )
        .await?
        .to_string();
    assert!(
        rename_no_label.contains("requires a label"),
        "{rename_no_label}"
    );

    // Rename: label set, lifecycle unchanged, audit entry records both.
    let renamed = structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id.clone(), "action": "rename",
                       "label": "Morning report"}),
            )
            .await?,
    )?;
    println!("readback=routine_update edge=rename {renamed}");
    assert_eq!(renamed["lifecycle_after"], "confirmed");
    assert_eq!(renamed["label_after"], "Morning report");
    assert_eq!(renamed["state"]["label"], "Morning report");

    // Disable, then re-mine: the miner must refresh bookkeeping WITHOUT
    // re-promoting the lifecycle or touching the label.
    let disabled = structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id.clone(), "action": "disable",
                       "note": "review: disabling before re-mine"}),
            )
            .await?,
    )?;
    assert_eq!(disabled["lifecycle_after"], "disabled");
    let remined = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    println!(
        "readback=routine_mine edge=re_mine_disabled created={} updated={} unmined={}",
        remined["state_rows_created"],
        remined["state_rows_updated"],
        remined["state_rows_marked_unmined"]
    );
    assert_eq!(remined["state_rows_created"], 0);
    assert_eq!(remined["state_rows_updated"], 1);
    let after_remine = structured(
        &client
            .tools_call("routine_inspect", json!({"routine_id": routine_id.clone()}))
            .await?,
    )?;
    println!("readback=routine_inspect edge=no_re_promotion {after_remine}");
    assert_eq!(
        after_remine["state"]["lifecycle"], "disabled",
        "mining must never re-promote a disabled routine"
    );
    assert_eq!(after_remine["state"]["label"], "Morning report");
    assert_eq!(
        after_remine["state"]["confidence_history"]
            .as_array()
            .map_or(0, Vec::len),
        1,
        "identical confidence is a heartbeat, not a change-point"
    );

    // List filters: default includes disabled; lifecycle/app/min_confidence
    // filters behave; archived is excluded by default.
    let default_list = structured(&client.tools_call("routine_list", json!({})).await?)?;
    println!("readback=routine_list edge=default {default_list}");
    assert_eq!(default_list["matched"], 1);
    assert_eq!(default_list["entries"][0]["lifecycle"], "disabled");
    assert_eq!(default_list["entries"][0]["label"], "Morning report");
    let confirmed_only = structured(
        &client
            .tools_call("routine_list", json!({"lifecycle": ["confirmed"]}))
            .await?,
    )?;
    assert_eq!(confirmed_only["matched"], 0);
    let by_app = structured(
        &client
            .tools_call("routine_list", json!({"app": "excel.exe"}))
            .await?,
    )?;
    assert_eq!(by_app["matched"], 1);
    let by_other_app = structured(
        &client
            .tools_call("routine_list", json!({"app": "notepad.exe"}))
            .await?,
    )?;
    assert_eq!(by_other_app["matched"], 0);
    let by_confidence = structured(
        &client
            .tools_call("routine_list", json!({"min_confidence": 0.99}))
            .await?,
    )?;
    assert_eq!(by_confidence["matched"], 0);

    // Archive hides from the default listing; explicit filter finds it.
    let archived = structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id.clone(), "action": "archive"}),
            )
            .await?,
    )?;
    assert_eq!(archived["lifecycle_after"], "archived");
    let after_archive = structured(&client.tools_call("routine_list", json!({})).await?)?;
    println!("readback=routine_list edge=archived_hidden {after_archive}");
    assert_eq!(after_archive["matched"], 0);
    let archived_list = structured(
        &client
            .tools_call("routine_list", json!({"lifecycle": ["archived"]}))
            .await?,
    )?;
    assert_eq!(archived_list["matched"], 1);

    // Enable returns it to candidate (it re-earns confirmation).
    let enabled = structured(
        &client
            .tools_call(
                "routine_update",
                json!({"routine_id": routine_id.clone(), "action": "enable"}),
            )
            .await?,
    )?;
    assert_eq!(enabled["lifecycle_before"], "archived");
    assert_eq!(enabled["lifecycle_after"], "candidate");

    // A mine that no longer derives the routine flags the state row as
    // unmined but keeps every operator decision.
    let emptied = structured(
        &client
            .tools_call("routine_mine", json!({"min_support_days": 6}))
            .await?,
    )?;
    println!(
        "readback=routine_mine edge=emptied written={} unmined={}",
        emptied["routines_written"], emptied["state_rows_marked_unmined"]
    );
    assert_eq!(emptied["routines_written"], 0);
    assert_eq!(emptied["state_rows_marked_unmined"], 1);
    let hidden = structured(&client.tools_call("routine_list", json!({})).await?)?;
    assert_eq!(hidden["total_mined"], 0);
    assert_eq!(hidden["matched"], 0, "unmined entries need include_unmined");
    let unmined_list = structured(
        &client
            .tools_call("routine_list", json!({"include_unmined": true}))
            .await?,
    )?;
    println!("readback=routine_list edge=include_unmined {unmined_list}");
    assert_eq!(unmined_list["matched"], 1);
    assert_eq!(unmined_list["entries"][0]["mined"], false);
    assert_eq!(
        unmined_list["entries"][0]["confidence"].as_f64(),
        Some(mined_confidence),
        "unmined entries carry the last recorded confidence"
    );
    let unmined_inspect = structured(
        &client
            .tools_call("routine_inspect", json!({"routine_id": routine_id.clone()}))
            .await?,
    )?;
    assert_eq!(unmined_inspect["mined"], false);
    assert_eq!(unmined_inspect["state"]["present_in_last_mine"], false);
    assert!(unmined_inspect["record"].is_null());

    // Re-mining re-derives the same stable id and re-links the state row.
    let restored = structured(&client.tools_call("routine_mine", json!({})).await?)?;
    assert_eq!(
        restored["routines"][0]["routine_id"],
        json!(routine_id.clone())
    );
    assert_eq!(restored["state_rows_created"], 0);
    assert_eq!(restored["state_rows_updated"], 1);

    // Edge: disk pressure refuses lifecycle writes before any mutation.
    let _pressed = structured(
        &client
            .tools_call("storage_pressure_sample", json!({"free_bytes": 0}))
            .await?,
    )?;
    let refused = client
        .tools_call_error(
            "routine_update",
            json!({"routine_id": routine_id.clone(), "action": "confirm"}),
        )
        .await?
        .to_string();
    println!("readback=routine_update edge=pressure_refusal {refused}");
    assert!(refused.contains("disk pressure"), "{refused}");
    let _released = structured(
        &client
            .tools_call(
                "storage_pressure_sample",
                json!({"free_bytes": 1_000_000_000_000_u64}),
            )
            .await?,
    )?;

    // Edge: structured list validation errors.
    for bad in [
        json!({"min_confidence": 1.5}),
        json!({"limit": 0}),
        json!({"lifecycle": []}),
        json!({"app": "  "}),
    ] {
        let invalid = client.tools_call_error("routine_list", bad.clone()).await?;
        let invalid_text = invalid.to_string();
        println!("readback=routine_list edge=invalid params={bad} error={invalid_text}");
        assert!(
            invalid_text.contains("TOOL_PARAMS_INVALID"),
            "expected TOOL_PARAMS_INVALID, got {invalid_text}"
        );
    }

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after daemon shutdown: the state row holds
    // the whole story — survival across mining is survival on disk.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_ROUTINE_STATE)?;
    println!(
        "readback=cf_routine_state edge=physical_sot rows={}",
        rows.len()
    );
    assert_eq!(rows.len(), 1);
    let (key, value) = &rows[0];
    let key_id = routine_codec::decode_routine_state_key(key)?;
    let state: RoutineStateRecord = decode_json(value)?;
    let actions: Vec<RoutineStateAction> = state
        .transitions
        .iter()
        .map(|transition| transition.action)
        .collect();
    println!(
        "readback=cf_routine_state key_id={key_id} id={} lifecycle={:?} label={:?} \
         present={} transitions={actions:?} confidence_points={}",
        state.routine_id,
        state.lifecycle,
        state.label,
        state.present_in_last_mine,
        state.confidence_history.len()
    );
    assert_eq!(key_id, state.routine_id, "key must mirror the record");
    assert_eq!(state.routine_id, routine_id);
    assert_eq!(state.lifecycle, RoutineLifecycle::Candidate);
    assert_eq!(state.label.as_deref(), Some("Morning report"));
    assert!(state.present_in_last_mine);
    assert_eq!(
        actions,
        [
            RoutineStateAction::Discovered,
            RoutineStateAction::Confirm,
            RoutineStateAction::Rename,
            RoutineStateAction::Disable,
            RoutineStateAction::Archive,
            RoutineStateAction::Enable,
        ],
        "the audit trail must record the exact transition history"
    );
    assert_eq!(state.transitions[0].by, "miner");
    assert_eq!(
        state.transitions[3].note.as_deref(),
        Some("review: disabling before re-mine"),
        "operator notes must persist in the audit trail"
    );
    assert!(
        state.transitions[1..]
            .iter()
            .all(|transition| transition.by == "stdio"),
        "operator transitions over stdio must be attributed to the stdio session"
    );
    Ok(())
}
