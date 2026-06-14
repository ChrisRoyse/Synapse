//! Full State Verification for the #957 auto-spawn dispatcher
//! (`task_dispatch_once`).
//!
//! Source of truth: the daemon's RocksDB `CF_KV` column family
//! (`agent-task/v1/task/...`). We drive the real MCP daemon over stdio, verify
//! through the non-reconciling `task_get` read tool, then shut the daemon down
//! and scan the physical task rows straight from RocksDB.
//!
//! What the stdio harness can and cannot exercise:
//!  - **No-op decisions** (empty queue) and the **spawn-failure path** are fully
//!    deterministic here. A spawn whose template does not exist fails at
//!    template resolution *before* any process is launched, so `task_dispatch_once`
//!    records a settled `failed` attempt on the still-`todo` task and returns the
//!    structured error — exactly the robust-logging-no-silent-drop contract.
//!  - The **successful real-agent dispatch** path needs HTTP mode + a live CLI
//!    (a session must register back), which no stdio test can provide. That happy
//!    path is verified by live FSV against the deployed daemon (see the issue),
//!    and by the unit tests over the pure `dispatch_decision` selector.

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

fn structured(result: &Value) -> anyhow::Result<&Value> {
    result
        .get("structuredContent")
        .with_context(|| format!("missing structuredContent in {result}"))
}

fn db_path_under(dir: &Path) -> PathBuf {
    dir.join("db")
}

#[tokio::test]
async fn task_dispatch_once_noop_and_spawn_failure_round_trip_against_physical_rows()
-> anyhow::Result<()> {
    let db_dir = tempfile::Builder::new()
        .prefix("synapse-task-dispatch-fsv")
        .tempdir()?;
    let db_path = db_path_under(db_dir.path());
    let db_path_str = db_path.to_string_lossy().into_owned();

    let mut client =
        StdioMcpClient::launch_and_init_with_env(None, &[("SYNAPSE_DB", db_path_str.as_str())])
            .await?;

    // ---- EDGE 1: empty board -> dispatch is a no-op (nothing spawned) ----
    let empty = client
        .tools_call("task_dispatch_once", json!({"concurrency_cap": 4}))
        .await?;
    let empty = structured(&empty)?;
    println!("readback=task_dispatch_once edge=empty state={empty}");
    ensure!(
        empty["decision"] == json!("empty")
            && empty["task"].is_null()
            && empty["spawn"].is_null()
            && empty["in_flight"] == json!(0)
            && empty["concurrency_cap"] == json!(4),
        "empty board must dispatch nothing, got {empty}"
    );

    // ---- ACTION: enqueue a task whose template does not exist ------------
    // Its dispatch will be SELECTED (it is the only todo, p1) but the spawn
    // fails at template resolution before any agent process is launched.
    let created = client
        .tools_call(
            "task_create",
            json!({
                "task_id": "ghost-task",
                "title": "references a missing template",
                "priority": 1,
                "template_id": "ghost-template",
                "template_params": {"repo": "Synapse"}
            }),
        )
        .await?;
    ensure!(
        structured(&created)?["task"]["state"] == json!("todo"),
        "task must be created todo"
    );

    // ---- EDGE 2: dispatch selects ghost-task, spawn fails loudly ---------
    // BEFORE: task is todo with zero attempts.
    let before = client
        .tools_call("task_get", json!({"task_id": "ghost-task"}))
        .await?;
    let before = structured(&before)?;
    println!(
        "readback=task_get edge=dispatch_BEFORE state={} attempts={}",
        before["task"]["state"], before["task"]["attempts"]
    );
    ensure!(
        before["task"]["state"] == json!("todo")
            && before["task"]["attempts"].as_array().map(Vec::len) == Some(0),
        "ghost-task must start todo with no attempts, got {before}"
    );

    let dispatch_err = client
        .tools_call_error("task_dispatch_once", json!({"concurrency_cap": 4}))
        .await?;
    let dispatch_err = dispatch_err.to_string();
    println!("readback=task_dispatch_once edge=spawn_failure err={dispatch_err}");
    ensure!(
        dispatch_err.contains("AGENT_TEMPLATE_NOT_FOUND"),
        "dispatch of a missing-template task must surface the structured template error, got {dispatch_err}"
    );

    // AFTER: task stayed todo, but a settled `failed` attempt was recorded with
    // the failure reason — the no-silent-drop contract. It is still dispatchable.
    let after = client
        .tools_call("task_get", json!({"task_id": "ghost-task"}))
        .await?;
    let after = structured(&after)?;
    println!(
        "readback=task_get edge=dispatch_AFTER state={} attempts={}",
        after["task"]["state"], after["task"]["attempts"]
    );
    ensure!(
        after["task"]["state"] == json!("todo"),
        "a failed spawn must leave the task todo (dispatchable), got {after}"
    );
    let attempts = after["task"]["attempts"]
        .as_array()
        .context("attempts must be an array")?;
    ensure!(attempts.len() == 1, "exactly one failed attempt, got {after}");
    let attempt = &attempts[0];
    ensure!(
        attempt["attempt_id"] == json!(1)
            && attempt["outcome"] == json!("failed")
            && attempt["session_id"] == json!("")
            && attempt["spawn_id"].is_null()
            && attempt["reason"]
                .as_str()
                .is_some_and(|r| r.contains("dispatch spawn failed")
                    && r.contains("AGENT_TEMPLATE_NOT_FOUND")),
        "the recorded attempt must be a settled failure carrying the reason, got {attempt}"
    );

    // ---- EDGE 3: re-dispatch — task is still eligible, fails again -------
    // Proves the task was never silently consumed: a second tick selects it and
    // appends a second failed attempt (attempt_id 2), still todo.
    let retry_err = client
        .tools_call_error("task_dispatch_once", json!({"concurrency_cap": 4}))
        .await?;
    ensure!(
        retry_err.to_string().contains("AGENT_TEMPLATE_NOT_FOUND"),
        "second dispatch must select the same task and fail again, got {retry_err}"
    );
    let after2 = client
        .tools_call("task_get", json!({"task_id": "ghost-task"}))
        .await?;
    let after2 = structured(&after2)?;
    let attempts2 = after2["task"]["attempts"]
        .as_array()
        .context("attempts must be an array")?;
    println!(
        "readback=task_get edge=redispatch state={} attempt_count={}",
        after2["task"]["state"],
        attempts2.len()
    );
    ensure!(
        after2["task"]["state"] == json!("todo")
            && attempts2.len() == 2
            && attempts2[1]["attempt_id"] == json!(2)
            && attempts2[1]["outcome"] == json!("failed"),
        "re-dispatch must append a second failed attempt, still todo, got {after2}"
    );

    // ---- PHYSICAL SOURCE-OF-TRUTH VERIFICATION ---------------------------
    let status = client.shutdown().await?;
    ensure!(status.success(), "daemon must exit cleanly");

    let db = Db::open(&db_path, SCHEMA_VERSION).context("open daemon RocksDB directly")?;
    let rows = db
        .scan_cf_prefix(cf::CF_KV, b"agent-task/v1/task/")
        .context("scan CF_KV for task rows")?;
    ensure!(rows.len() == 1, "exactly one task row on disk, got {}", rows.len());
    let (key, value) = &rows[0];
    let key = String::from_utf8_lossy(key).into_owned();
    let task: Value = serde_json::from_slice(value).context("decode task row")?;
    println!("readback=cf_kv edge=physical_row key={key} row={task}");
    ensure!(
        task["task_id"] == json!("ghost-task")
            && task["state"] == json!("todo")
            && task["attempts"].as_array().map(Vec::len) == Some(2)
            && task["attempts"][0]["outcome"] == json!("failed")
            && task["attempts"][1]["outcome"] == json!("failed")
            && task["attempts"][1]["reason"]
                .as_str()
                .is_some_and(|r| r.contains("AGENT_TEMPLATE_NOT_FOUND")),
        "on-disk row must be a still-todo task with two recorded failed attempts, got {task}"
    );

    Ok(())
}
