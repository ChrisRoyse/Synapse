//! Live end-to-end regression coverage for the `agent_stats` MCP tool (#903).
//!
//! Seeds a real `CF_AGENT_EVENTS` journal with a synthetic agent whose every
//! derived metric is hand-computable, launches the actual `synapse-mcp` binary
//! pointed at that DB, calls `agent_stats` over a real MCP stdio session, and
//! reconciles the returned numbers against a direct physical-row scan of the
//! same column family. This is supporting evidence only; manual Full State
//! Verification is performed separately against a live daemon and physical
//! Sources of Truth. No mocks: real binary, real `RocksDB`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{Value, json};
use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord, SCHEMA_VERSION};
use synapse_storage::{Db, agent_events::agent_event_key, cf};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

const SPAWN: &str = "agent-spawn-stats-e2e";

/// Builds the 12-row synthetic agent. Offsets are in seconds from a recent
/// base (60 s ago, so retention never evicts a row), making the derived
/// durations independent of when the test runs.
fn synthetic_rows() -> anyhow::Result<Vec<AgentEventRecord>> {
    let now_ns = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("clock after epoch")?
            .as_nanos(),
    )
    .context("now overflows u64")?;
    let base = now_ns - 60_000_000_000;
    let secs = |n: u64| base + n * 1_000_000_000;

    let spawn_evt = |ts: u64, kind: AgentEventKind| {
        let mut r = AgentEventRecord::new(ts, kind);
        r.spawn_id = Some(SPAWN.to_owned());
        r
    };
    let finished = |ts: u64, duration_ms: u64, error: Option<&str>| {
        let mut r = spawn_evt(ts, AgentEventKind::ToolCallFinished);
        r.payload = json!({ "duration_ms": duration_ms });
        r.attributes.error_type = error.map(ToOwned::to_owned);
        r
    };
    let machine = |ts: u64, from: &str, to: &str| {
        let mut r = spawn_evt(ts, AgentEventKind::StateChanged);
        r.state_from = Some(from.to_owned());
        r.state_to = Some(to.to_owned());
        r.payload = json!({ "origin": "agent_state_machine" });
        r
    };

    Ok(vec![
        spawn_evt(secs(0), AgentEventKind::SpawnRequested),
        machine(secs(1), "spawning", "working"),
        spawn_evt(secs(2), AgentEventKind::ToolCallStarted),
        finished(secs(5), 3000, None),
        spawn_evt(secs(6), AgentEventKind::ToolCallStarted),
        finished(secs(7), 1000, Some("tool_failure")),
        spawn_evt(secs(8), AgentEventKind::ToolCallStarted),
        finished(secs(9), 5000, None),
        spawn_evt(secs(10), AgentEventKind::LeaseAcquired),
        spawn_evt(secs(11), AgentEventKind::LeaseReleased),
        machine(secs(12), "working", "needs_input"),
        {
            let mut exited = spawn_evt(secs(20), AgentEventKind::Exited);
            exited.end_state = Some(AgentEndState::Success);
            exited
        },
    ])
}

/// Writes the rows directly into the journal column family, then drops the
/// handle so the daemon can take the single-process lock.
fn seed_db(db_path: &std::path::Path, rows: &[AgentEventRecord]) -> anyhow::Result<()> {
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    let mut batch = Vec::with_capacity(rows.len());
    for (i, record) in rows.iter().enumerate() {
        record.validate().map_err(|e| anyhow::anyhow!(e))?;
        let value = serde_json::to_vec(record)?;
        // Distinct ts per row, so seq can be a simple ordinal tie-breaker.
        let seq = u32::try_from(i).context("row index fits u32")?;
        batch.push((agent_event_key(record.ts_ns, seq), value));
    }
    db.put_batch(cf::CF_AGENT_EVENTS, batch)?;
    db.flush()?;
    Ok(())
}

/// Counts physical `CF_AGENT_EVENTS` rows for `SPAWN` by re-opening the DB —
/// the independent source of truth the tool's output must reconcile with.
fn direct_spawn_row_count(db_path: &std::path::Path) -> anyhow::Result<usize> {
    let db = Db::open(db_path, SCHEMA_VERSION)?;
    let rows = db.scan_cf(cf::CF_AGENT_EVENTS)?;
    let mut count = 0;
    for (_key, value) in &rows {
        let record: AgentEventRecord = serde_json::from_slice(value)?;
        if record.spawn_id.as_deref() == Some(SPAWN) {
            count += 1;
        }
    }
    Ok(count)
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

#[tokio::test]
async fn agent_stats_reconciles_with_physical_journal_rows() -> anyhow::Result<()> {
    let logs = TempDir::new()?;
    let db_dir = TempDir::new()?;
    let db_path = db_dir.path().join("db");

    // Seed before the daemon takes the DB lock.
    let rows = synthetic_rows()?;
    seed_db(&db_path, &rows)?;

    let db_path_string = db_path.to_string_lossy().into_owned();
    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(logs.path()),
        &[("SYNAPSE_DB", db_path_string.as_str())],
    )
    .await?;

    // Registration: the tool is exposed over MCP.
    let tools = client.tools_list().await?;
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .context("tools array")?
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"agent_stats"),
        "agent_stats must be registered: {names:?}"
    );

    // Call the tool scoped to our seeded spawn.
    let response = client
        .tools_call(
            "agent_stats",
            json!({ "spawn_id": SPAWN, "group_by": "agent" }),
        )
        .await?;
    let stats = structured(&response)?;
    println!("readback=agent_stats edge=mcp stats={stats}");

    // scanned_rows reconciles exactly with the physical CF row count.
    assert_eq!(stats["ok"], json!(true));
    assert_eq!(stats["scanned_rows"], json!(12));
    assert_eq!(stats["agents_total"], json!(1));

    let fleet = &stats["fleet"];
    assert_eq!(fleet["events_total"], json!(12));
    assert_eq!(fleet["events_by_kind"]["tool_call_started"], json!(3));
    assert_eq!(fleet["events_by_kind"]["tool_call_finished"], json!(3));

    // Latency over [1000, 3000, 5000].
    let lat = &fleet["tool_latency_ms"];
    assert_eq!(lat["count"], json!(3));
    assert_eq!(lat["p50_ms"], json!(3000));
    assert_eq!(lat["p95_ms"], json!(5000));
    assert_eq!(lat["p99_ms"], json!(5000));
    assert_eq!(lat["min_ms"], json!(1000));
    assert_eq!(lat["max_ms"], json!(5000));

    // Errors: 1 of 3 finished calls failed.
    assert_eq!(fleet["errors"]["errored_tool_calls"], json!(1));
    let error_rate = fleet["errors"]["error_rate"]
        .as_f64()
        .context("error_rate")?;
    assert!(
        (error_rate - 1.0 / 3.0).abs() < 1e-9,
        "error_rate={error_rate}"
    );
    assert_eq!(fleet["errors"]["by_type"]["tool_failure"], json!(1));

    // Leases + end states.
    assert_eq!(fleet["leases"]["acquired"], json!(1));
    assert_eq!(fleet["leases"]["released"], json!(1));
    assert_eq!(fleet["end_states"]["success"], json!(1));

    // Time-in-state: working 11 s, needs_input 8 s.
    assert_eq!(fleet["time_in_state_ms"]["working"], json!(11_000));
    assert_eq!(fleet["time_in_state_ms"]["needs_input"], json!(8_000));

    // group_by=agent surfaces the single anchor.
    let per_agent = stats["per_agent"].as_array().context("per_agent array")?;
    assert_eq!(per_agent.len(), 1);
    assert_eq!(per_agent[0]["anchor"], json!(SPAWN));

    let exit_status = client.shutdown().await?;
    assert!(exit_status.success(), "daemon must exit cleanly");

    // Independent cross-check: the physical journal holds exactly the 12 rows
    // the tool reported scanning for this spawn.
    let physical = direct_spawn_row_count(&db_path)?;
    assert_eq!(
        physical, 12,
        "direct CF scan must match agent_stats scanned_rows"
    );

    Ok(())
}
