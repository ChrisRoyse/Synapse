//! Supporting integration evidence for the #843 recorder controls:
//! `timeline_pause`, `timeline_resume`, `timeline_exclusions`, and
//! `timeline_purge`, driven through the real MCP stdio server against a real
//! `RocksDB`. Manual FSV remains separate.
//!
//! The stdio server hosts no live recorder (that is the HTTP daemon's job),
//! so these tests verify the durable layer the tools own end to end: control
//! state persisted in `CF_KV` across server restarts, purge physically
//! deleting exactly the matching rows, the counts-only audit row, and the
//! structured validation errors. Live-recorder enforcement (zero rows while
//! paused, exclusion suppression) is covered by the in-crate recorder tests
//! and the manual daemon FSV on the issue.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf, timeline};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const STEP_NS: u64 = 1_000_000_000;
const CONTROL_KEY: &[u8] = b"timeline/control/v1";

fn base_ts_ns() -> anyhow::Result<u64> {
    let now_ns = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
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

fn control_row(db_path: &std::path::Path) -> anyhow::Result<Value> {
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    let rows = db.scan_cf_prefix(cf::CF_KV, CONTROL_KEY)?;
    let (_key, value) = rows
        .into_iter()
        .find(|(key, _value)| key.as_slice() == CONTROL_KEY)
        .context("timeline control row missing from CF_KV")?;
    Ok(serde_json::from_slice(&value)?)
}

#[tokio::test]
async fn pause_resume_and_exclusions_persist_across_server_restarts() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db_dir = TempDir::new()?;
    let db_path = db_dir.path().join("db");
    let db_path_string = db_path.to_string_lossy().into_owned();
    let env: &[(&str, &str)] = &[
        ("SYNAPSE_DEBUG_TOOLS", "1"),
        ("SYNAPSE_DB", db_path_string.as_str()),
        ("SYNAPSE_TIMELINE_EXCLUDE", "KeePass.exe"),
    ];

    let mut client = StdioMcpClient::launch_and_init_with_env(Some(logs.path()), env).await?;

    // Edge: resume before any pause — honest no-op, persisted state readable.
    let resume_noop = structured(&client.tools_call("timeline_resume", json!({})).await?)?;
    println!("readback=timeline_resume edge=never_paused after={resume_noop}");
    assert_eq!(resume_noop["paused"], false);
    assert_eq!(resume_noop["was_paused"], false);
    assert_eq!(resume_noop["recorder_live"], false);
    assert_eq!(resume_noop["boundary_row_written"], false);

    // Pause with an auto-resume deadline.
    let pause = structured(
        &client
            .tools_call("timeline_pause", json!({"duration_ms": 3_600_000}))
            .await?,
    )?;
    println!("readback=timeline_pause edge=happy_path after={pause}");
    assert_eq!(pause["paused"], true);
    assert_eq!(pause["was_paused"], false);
    assert_eq!(pause["persisted"], true);
    assert!(
        pause["paused_until_ns"].as_u64().is_some(),
        "duration_ms must arm an auto-resume deadline: {pause}"
    );

    // Exclusions: env baseline visible, runtime add/remove roundtrip.
    let exclusions = structured(
        &client
            .tools_call(
                "timeline_exclusions",
                json!({"add": ["Signal.EXE", "banking-app.exe"]}),
            )
            .await?,
    )?;
    println!("readback=timeline_exclusions edge=add after={exclusions}");
    assert_eq!(exclusions["env_exclusions"], json!(["keepass.exe"]));
    assert_eq!(
        exclusions["runtime_exclusions"],
        json!(["banking-app.exe", "signal.exe"])
    );
    assert_eq!(
        exclusions["effective_exclusions"],
        json!(["banking-app.exe", "keepass.exe", "signal.exe"])
    );
    assert_eq!(
        exclusions["added"],
        json!(["signal.exe", "banking-app.exe"]),
        "added preserves request order"
    );
    assert_eq!(exclusions["persisted"], true);

    // Edge: env-baseline entries are immutable at runtime.
    let env_remove = client
        .tools_call_error("timeline_exclusions", json!({"remove": ["keepass.exe"]}))
        .await?
        .to_string();
    println!("readback=timeline_exclusions edge=env_remove error={env_remove}");
    assert!(
        env_remove.contains("TOOL_PARAMS_INVALID")
            && env_remove.contains("SYNAPSE_TIMELINE_EXCLUDE"),
        "env removal must be a structured refusal: {env_remove}"
    );

    // Edge: path-like and empty entries are structured errors.
    for bad in [
        json!({"add": ["C:\\tools\\app.exe"]}),
        json!({"add": [" "]}),
    ] {
        let error = client
            .tools_call_error("timeline_exclusions", bad.clone())
            .await?
            .to_string();
        assert!(
            error.contains("TOOL_PARAMS_INVALID"),
            "expected TOOL_PARAMS_INVALID for {bad}, got {error}"
        );
    }

    // Edge: pause duration_ms = 0 is refused.
    let zero = client
        .tools_call_error("timeline_pause", json!({"duration_ms": 0}))
        .await?
        .to_string();
    assert!(
        zero.contains("TOOL_PARAMS_INVALID"),
        "duration_ms=0 must be refused: {zero}"
    );

    // Restart: durable truth must survive into a fresh server process.
    let status = client.shutdown().await?;
    assert!(status.success());
    let persisted = control_row(&db_path)?;
    println!("readback=cf_kv edge=post_shutdown control_row={persisted}");
    assert_eq!(persisted["paused"], true);
    assert_eq!(
        persisted["runtime_exclusions"],
        json!(["banking-app.exe", "signal.exe"])
    );

    let mut client = StdioMcpClient::launch_and_init_with_env(Some(logs.path()), env).await?;
    let after_restart = structured(&client.tools_call("timeline_exclusions", json!({})).await?)?;
    println!("readback=timeline_exclusions edge=post_restart after={after_restart}");
    assert_eq!(
        after_restart["effective_exclusions"],
        json!(["banking-app.exe", "keepass.exe", "signal.exe"])
    );
    assert_eq!(after_restart["persisted"], false, "read-only call");

    // Resume in the new process: the pre-restart pause must still be in force.
    let resume = structured(&client.tools_call("timeline_resume", json!({})).await?)?;
    println!("readback=timeline_resume edge=post_restart after={resume}");
    assert_eq!(resume["was_paused"], true, "pause must survive restart");
    assert_eq!(resume["paused"], false);

    let status = client.shutdown().await?;
    assert!(status.success());
    let final_row = control_row(&db_path)?;
    println!("readback=cf_kv edge=final control_row={final_row}");
    assert_eq!(final_row["paused"], false);
    Ok(())
}

#[tokio::test]
async fn purge_deletes_exactly_matching_rows_and_writes_audit() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db_dir = TempDir::new()?;
    let db_path = db_dir.path().join("db");
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

    // Ground truth: 20 chrome rows with a secret token, 30 excel rows.
    seed(
        &mut client,
        "purge-chrome",
        20,
        base,
        json!({"record_version": 1, "kind": "browser_nav", "actor": {"actor": "human"},
               "app": "chrome.exe",
               "payload": {"url": "https://example.test/secret-token-xyz"}}),
    )
    .await?;
    seed(
        &mut client,
        "purge-excel",
        30,
        base + 100 * STEP_NS,
        json!({"record_version": 1, "kind": "focus_change", "actor": {"actor": "human"},
               "app": "EXCEL.EXE", "payload": {"path": "C:/docs/budget.xlsx"}}),
    )
    .await?;

    // Edge: no filters and no all=true is a structured refusal.
    let unfiltered = client
        .tools_call_error("timeline_purge", json!({}))
        .await?
        .to_string();
    assert!(
        unfiltered.contains("TOOL_PARAMS_INVALID") && unfiltered.contains("at least one filter"),
        "filterless purge must be refused: {unfiltered}"
    );
    // Edge: all=true plus filters is ambiguous and refused.
    let ambiguous = client
        .tools_call_error(
            "timeline_purge",
            json!({"all": true, "apps": ["chrome.exe"]}),
        )
        .await?
        .to_string();
    assert!(
        ambiguous.contains("TOOL_PARAMS_INVALID") && ambiguous.contains("mutually exclusive"),
        "all+filters must be refused: {ambiguous}"
    );

    // Dry run: counts without deletion.
    let dry = structured(
        &client
            .tools_call(
                "timeline_purge",
                json!({"text": "secret-token-xyz", "dry_run": true}),
            )
            .await?,
    )?;
    println!("readback=timeline_purge edge=dry_run after={dry}");
    assert_eq!(dry["matched_rows"], 20);
    assert_eq!(dry["deleted_rows"], 0);
    assert!(
        dry["audit_key_hex"].is_null(),
        "dry_run writes no audit row"
    );
    let before = structured(
        &client
            .tools_call("timeline_search", json!({"limit": 500}))
            .await?,
    )?;
    assert_eq!(
        before["matches"].as_array().map_or(0, Vec::len),
        50,
        "dry_run must not delete: {before}"
    );

    // Real purge by text.
    let purge = structured(
        &client
            .tools_call("timeline_purge", json!({"text": "secret-token-xyz"}))
            .await?,
    )?;
    println!("readback=timeline_purge edge=happy_path after={purge}");
    assert_eq!(purge["matched_rows"], 20);
    assert_eq!(purge["deleted_rows"], 20);
    assert_eq!(purge["compacted"], true);
    let audit_key_hex = purge["audit_key_hex"]
        .as_str()
        .context("audit_key_hex missing")?
        .to_owned();

    // Post-state through the tool surface: 30 excel rows + 1 purge audit row.
    let after = structured(
        &client
            .tools_call("timeline_search", json!({"limit": 500}))
            .await?,
    )?;
    let kinds: Vec<&str> = after["matches"]
        .as_array()
        .context("matches")?
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    println!(
        "readback=timeline_search edge=post_purge total={} purge_rows={}",
        kinds.len(),
        kinds.iter().filter(|kind| **kind == "purge").count()
    );
    assert_eq!(kinds.len(), 31);
    assert_eq!(kinds.iter().filter(|kind| **kind == "purge").count(), 1);
    assert!(
        !kinds.contains(&"browser_nav"),
        "all secret-token rows must be gone: {after}"
    );

    // The audit row carries counts and filters, never deleted content.
    let audit = after["matches"]
        .as_array()
        .context("matches")?
        .iter()
        .find(|entry| entry["kind"] == "purge")
        .context("purge audit row missing from search")?
        .clone();
    assert_eq!(audit["key_hex"].as_str(), Some(audit_key_hex.as_str()));
    assert_eq!(audit["payload"]["deleted_rows"], 20);
    assert_eq!(audit["payload"]["filters"]["text"], "secret-token-xyz");
    assert!(
        audit["payload"]["by_session"].is_string(),
        "audit row must attribute the requesting session: {audit}"
    );

    // Self-reference guard: re-running the identical purge must match the
    // audit row (its filters contain the text) but protect, not delete, it.
    let again = structured(
        &client
            .tools_call("timeline_purge", json!({"text": "secret-token-xyz"}))
            .await?,
    )?;
    println!("readback=timeline_purge edge=self_reference after={again}");
    assert_eq!(again["deleted_rows"], 0);
    assert_eq!(again["protected_audit_rows"], 1);

    // Edge: zero-match purge is honest and still audited.
    let none = structured(
        &client
            .tools_call("timeline_purge", json!({"apps": ["never-ran.exe"]}))
            .await?,
    )?;
    println!("readback=timeline_purge edge=zero_match after={none}");
    assert_eq!(none["matched_rows"], 0);
    assert_eq!(none["deleted_rows"], 0);
    assert_eq!(none["compacted"], false);
    assert!(none["audit_key_hex"].is_string());

    // Explicitly purging the audit trail requires naming the kind.
    let purge_audits = structured(
        &client
            .tools_call("timeline_purge", json!({"kinds": ["purge"]}))
            .await?,
    )?;
    println!("readback=timeline_purge edge=explicit_audit_purge after={purge_audits}");
    assert_eq!(purge_audits["deleted_rows"], 3);
    assert_eq!(purge_audits["protected_audit_rows"], 0);

    let status = client.shutdown().await?;
    assert!(status.success());

    // Physical source of truth after shutdown: exactly the 30 excel rows plus
    // the final explicit-audit-purge audit row remain.
    let reopened = Db::open(&db_path, SCHEMA_VERSION)?;
    let rows = reopened.scan_cf(cf::CF_TIMELINE)?;
    let mut kind_counts = std::collections::BTreeMap::new();
    for (key, value) in &rows {
        timeline::decode_timeline_key(key)
            .map_err(|error| anyhow::anyhow!("non-codec key after purge: {error}"))?;
        let record: Value = serde_json::from_slice(value)?;
        *kind_counts
            .entry(record["kind"].as_str().unwrap_or("?").to_owned())
            .or_insert(0_u64) += 1;
    }
    println!(
        "readback=cf_timeline edge=physical_sot total_rows={} kind_counts={kind_counts:?}",
        rows.len()
    );
    assert_eq!(rows.len(), 31);
    assert_eq!(kind_counts.get("focus_change"), Some(&30));
    assert_eq!(kind_counts.get("purge"), Some(&1));
    Ok(())
}
