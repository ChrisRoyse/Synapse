//! Full State Verification for the durable agent task queue (#910).
//!
//! Source of truth: the daemon's RocksDB `CF_KV` column family. We drive the
//! real MCP daemon over stdio and verify every outcome two ways — through a
//! read tool (`task_get`, which does NOT reconcile, so it reflects raw stored
//! state), and by shutting the daemon down and scanning the physical
//! `agent-task/v1/task/...` rows directly from RocksDB.
//!
//! Note on reconcile: the session registry is only populated in HTTP mode, so
//! under the stdio harness the live-session set is empty. That makes the
//! orphan-flagging path deterministic — any `in_progress` task is treated as
//! having a dead session, exactly the "agent died mid-task" crash-recovery case
//! the issue calls out. Happy-path lifecycle assertions therefore use
//! `task_get` and explicit `task_update` transitions (never the lazily-
//! reconciling `task_list`) until each task is settled.
//!
//! Synthetic ground truth (one board):
//!   deploy   template=reviewer priority=1  (highest)
//!   docs     template=writer   priority=3
//!   cleanup  template=reviewer priority=5  (lowest)

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
async fn agent_tasks_lifecycle_round_trips_against_physical_cf_rows() -> anyhow::Result<()> {
    let db_dir = tempfile::Builder::new()
        .prefix("synapse-agent-tasks-fsv")
        .tempdir()?;
    let db_path = db_path_under(db_dir.path());
    let db_path_str = db_path.to_string_lossy().into_owned();

    let mut client =
        StdioMcpClient::launch_and_init_with_env(None, &[("SYNAPSE_DB", db_path_str.as_str())])
            .await?;

    // ---- BEFORE: empty board -------------------------------------------
    let empty = client.tools_call("task_list", json!({})).await?;
    let empty = structured(&empty)?;
    println!("readback=task_list edge=before state={empty}");
    ensure!(empty["count"] == json!(0), "board must start empty, got {empty}");

    // ---- ACTION: create three tasks ------------------------------------
    for (id, template, priority, seq) in [
        ("deploy", "reviewer", 1, 1),
        ("docs", "writer", 3, 2),
        ("cleanup", "reviewer", 5, 3),
    ] {
        let created = client
            .tools_call(
                "task_create",
                json!({
                    "task_id": id,
                    "title": format!("Task {id}"),
                    "acceptance": "done when green",
                    "priority": priority,
                    "template_id": template,
                    "template_params": {"repo": "Synapse"}
                }),
            )
            .await?;
        let created = structured(&created)?;
        println!("readback=task_create edge=create state={created}");
        ensure!(
            created["task"]["state"] == json!("todo")
                && created["task"]["priority"] == json!(priority)
                && created["task"]["enqueue_seq"] == json!(seq)
                && created["written_row"]["cf_name"] == json!("CF_KV"),
            "task {id} create mismatch: {created}"
        );
    }

    // Duplicate id rejected loudly.
    let dup = client
        .tools_call_error(
            "task_create",
            json!({"task_id": "deploy", "title": "dup", "template_id": "reviewer"}),
        )
        .await?;
    ensure!(
        dup.to_string().contains("already exists"),
        "duplicate task id must be rejected, got {dup}"
    );

    // ---- VERIFY: dispatcher picks the highest priority (deploy, p1) -----
    let next = client
        .tools_call("task_next", json!({"concurrency_cap": 8}))
        .await?;
    let next = structured(&next)?;
    println!("readback=task_next edge=select state={next}");
    ensure!(
        next["decision"] == json!("dispatch")
            && next["task"]["task_id"] == json!("deploy")
            && next["in_flight"] == json!(0),
        "task_next must pick the p1 task, got {next}"
    );

    // ---- HAPPY PATH: claim -> review -> done (verified via task_get) ----
    let claimed = client
        .tools_call(
            "task_claim",
            json!({"task_id": "deploy", "session_id": "agent-deploy-1"}),
        )
        .await?;
    let claimed = structured(&claimed)?;
    println!("readback=task_claim edge=claim state={claimed}");
    ensure!(
        claimed["task"]["state"] == json!("in_progress")
            && claimed["task"]["attempts"][0]["session_id"] == json!("agent-deploy-1")
            && claimed["task"]["attempts"][0]["outcome"] == json!("pending")
            && claimed["task"]["attempts"][0]["attempt_id"] == json!(1),
        "claim must move deploy to in_progress with a pending attempt, got {claimed}"
    );

    // Duplicate claim race: deploy is no longer todo.
    let dup_claim = client
        .tools_call_error(
            "task_claim",
            json!({"task_id": "deploy", "session_id": "agent-deploy-2"}),
        )
        .await?;
    println!("readback=task_claim edge=duplicate err={dup_claim}");
    ensure!(
        dup_claim.to_string().contains("AGENT_TASK_INVALID_TRANSITION")
            && dup_claim.to_string().contains("not todo"),
        "double claim must be rejected, got {dup_claim}"
    );

    // Invalid transition: a todo task cannot jump straight to done.
    let bad_transition = client
        .tools_call_error("task_update", json!({"task_id": "docs", "state": "done"}))
        .await?;
    println!("readback=task_update edge=invalid_transition err={bad_transition}");
    ensure!(
        bad_transition
            .to_string()
            .contains("AGENT_TASK_INVALID_TRANSITION")
            && bad_transition.to_string().contains("todo -> done"),
        "todo->done must be rejected, got {bad_transition}"
    );

    // in_progress -> review settles the live attempt as succeeded.
    let reviewed = client
        .tools_call(
            "task_update",
            json!({"task_id": "deploy", "state": "review", "reason": "ready for human"}),
        )
        .await?;
    let reviewed = structured(&reviewed)?;
    ensure!(
        reviewed["task"]["state"] == json!("review")
            && reviewed["task"]["attempts"][0]["outcome"] == json!("succeeded"),
        "review must settle the attempt succeeded, got {reviewed}"
    );
    let done = client
        .tools_call("task_update", json!({"task_id": "deploy", "state": "done"}))
        .await?;
    ensure!(
        structured(&done)?["task"]["state"] == json!("done"),
        "review->done must succeed"
    );

    // ---- CANCEL: a todo task -> cancelled ------------------------------
    let cancelled = client
        .tools_call(
            "task_cancel",
            json!({"task_id": "cleanup", "reason": "obsolete"}),
        )
        .await?;
    ensure!(
        structured(&cancelled)?["task"]["state"] == json!("cancelled"),
        "cleanup must cancel"
    );

    // ---- EDGE: agent dies mid-task -> reconcile flags it orphaned -------
    // docs is the only remaining task; claim it (session never goes live in
    // stdio), then reconcile must flag it.
    let claim_docs = client
        .tools_call(
            "task_claim",
            json!({"task_id": "docs", "session_id": "ghost-session"}),
        )
        .await?;
    let before = structured(&claim_docs)?;
    println!(
        "readback=task_get edge=orphan_BEFORE state={} attempt_outcome={}",
        before["task"]["state"], before["task"]["attempts"][0]["outcome"]
    );
    ensure!(before["task"]["state"] == json!("in_progress"));

    let reconciled = client.tools_call("task_reconcile", json!({})).await?;
    let reconciled = structured(&reconciled)?;
    println!("readback=task_reconcile edge=orphan state={reconciled}");
    ensure!(
        reconciled["scanned_in_progress"] == json!(1)
            && reconciled["flagged_orphans"] == json!(["docs"]),
        "reconcile must flag the orphaned in_progress task, got {reconciled}"
    );

    let after = client
        .tools_call("task_get", json!({"task_id": "docs"}))
        .await?;
    let after = structured(&after)?;
    println!(
        "readback=task_get edge=orphan_AFTER state={} attempt_outcome={} reason={}",
        after["task"]["state"],
        after["task"]["attempts"][0]["outcome"],
        after["task"]["review_reason"]
    );
    ensure!(
        after["task"]["state"] == json!("review")
            && after["task"]["attempts"][0]["outcome"] == json!("orphaned")
            && after["task"]["review_reason"]
                .as_str()
                .is_some_and(|r| r.contains("orphaned")),
        "orphaned task must be flagged into review with a reason, got {after}"
    );

    // ---- VERIFY: queue now empty (all tasks settled) -------------------
    let next_empty = client.tools_call("task_next", json!({})).await?;
    ensure!(
        structured(&next_empty)?["decision"] == json!("empty"),
        "no todo tasks remain -> empty"
    );

    // ---- PHYSICAL SOURCE-OF-TRUTH VERIFICATION -------------------------
    let status = client.shutdown().await?;
    ensure!(status.success(), "daemon must exit cleanly");

    let db = Db::open(&db_path, SCHEMA_VERSION).context("open daemon RocksDB directly")?;
    let rows = db
        .scan_cf_prefix(cf::CF_KV, b"agent-task/v1/task/")
        .context("scan CF_KV for task rows")?;
    let mut by_id: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for (k, v) in &rows {
        let key = String::from_utf8_lossy(k).into_owned();
        let task: Value = serde_json::from_slice(v).context("decode task row")?;
        by_id.insert(
            task["task_id"].as_str().unwrap_or_default().to_owned(),
            task,
        );
        println!("readback=cf_kv edge=physical_row key={key} state={}", by_id.values().last().map(|t| t["state"].clone()).unwrap_or(Value::Null));
    }
    ensure!(by_id.len() == 3, "exactly 3 task rows on disk, got {}", by_id.len());
    ensure!(
        by_id["deploy"]["state"] == json!("done"),
        "deploy must be done on disk, got {}",
        by_id["deploy"]
    );
    ensure!(
        by_id["cleanup"]["state"] == json!("cancelled"),
        "cleanup must be cancelled on disk"
    );
    ensure!(
        by_id["docs"]["state"] == json!("review")
            && by_id["docs"]["attempts"][0]["outcome"] == json!("orphaned"),
        "docs must be a flagged orphan on disk, got {}",
        by_id["docs"]
    );
    println!(
        "readback=cf_kv edge=decoded_orphan row={}",
        by_id["docs"]
    );

    Ok(())
}
