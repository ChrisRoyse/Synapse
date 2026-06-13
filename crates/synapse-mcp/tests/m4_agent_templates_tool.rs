//! Full State Verification for agent spawn templates (#909).
//!
//! Every assertion is grounded in the **source of truth**: the daemon's RocksDB
//! `CF_KV` column family. We drive the real MCP daemon over stdio, then prove the
//! outcome two independent ways:
//!  1. read it back through a *different* tool (get/list — a fresh RocksDB scan),
//!     and
//!  2. shut the daemon down and open its RocksDB **directly** from the test,
//!     scanning the physical `agent-template/v1/...` rows and decoding their JSON.
//!
//! It also audits the boundary/edge cases the issue calls out: unknown agent
//! kind rejected loudly, version bump on edit, historical-version resolution,
//! delete makes the current pointer vanish while version snapshots survive, and
//! spawning from a missing/deleted template or with a bad parameter contract
//! fails with nothing launched.

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{Db, cf};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;

/// Pulls the `structuredContent` object out of a successful tools/call result.
fn structured(result: &Value) -> anyhow::Result<&Value> {
    result
        .get("structuredContent")
        .with_context(|| format!("missing structuredContent in {result}"))
}

fn db_path_under(dir: &Path) -> PathBuf {
    dir.join("db")
}

#[tokio::test]
async fn agent_templates_crud_round_trips_against_physical_cf_rows() -> anyhow::Result<()> {
    let db_dir = tempfile::Builder::new()
        .prefix("synapse-agent-templates-fsv")
        .tempdir()?;
    let db_path = db_path_under(db_dir.path());
    let db_path_str = db_path.to_string_lossy().into_owned();

    let mut client =
        StdioMcpClient::launch_and_init_with_env(None, &[("SYNAPSE_DB", db_path_str.as_str())])
            .await?;

    // ---- BEFORE state: the store is empty --------------------------------
    let empty = client
        .tools_call("agent_template_list", json!({}))
        .await?;
    let empty = structured(&empty)?;
    println!("readback=agent_template_list edge=before state={empty}");
    ensure!(
        empty["count"] == json!(0) && empty["templates"] == json!([]),
        "store must start empty, got {empty}"
    );

    // ---- ACTION 1: create a template (v1) --------------------------------
    // Synthetic ground truth: a Claude reviewer with two declared slots.
    let put_v1 = client
        .tools_call(
            "agent_template_put",
            json!({
                "template_id": "reviewer",
                "name": "Code reviewer",
                "agent_kind": "claude",
                "model": "claude-opus-4-8",
                "prompt_template": "Review ${repo} focusing on ${area}.",
                "required_params": ["repo", "area"],
                "working_dir": "C:\\code\\Synapse"
            }),
        )
        .await?;
    let put_v1 = structured(&put_v1)?;
    println!("readback=agent_template_put edge=create state={put_v1}");
    ensure!(put_v1["created"] == json!(true), "first put must be created");
    ensure!(
        put_v1["template"]["version"] == json!(1),
        "first version must be 1, got {put_v1}"
    );
    let hash_v1 = put_v1["template"]["config_hash"]
        .as_str()
        .context("config_hash missing")?
        .to_owned();
    ensure!(hash_v1.len() == 64, "config_hash must be 64 hex chars");
    // The put response reports exactly which physical rows it wrote.
    let written = put_v1["written_rows"]
        .as_array()
        .context("written_rows missing")?;
    ensure!(written.len() == 2, "put writes a snapshot + pointer row");
    ensure!(
        written
            .iter()
            .all(|row| row["cf_name"] == json!("CF_KV")),
        "rows must live in CF_KV"
    );

    // ---- VERIFY via a different tool (independent RocksDB read) -----------
    let got_v1 = client
        .tools_call("agent_template_get", json!({"template_id": "reviewer"}))
        .await?;
    let got_v1 = structured(&got_v1)?;
    println!("readback=agent_template_get edge=current_after_create state={got_v1}");
    ensure!(
        got_v1["template"]["version"] == json!(1)
            && got_v1["template"]["model"] == json!("claude-opus-4-8")
            && got_v1["row_key"] == json!("agent-template/v1/cur/reviewer"),
        "current get must return v1, got {got_v1}"
    );

    // ---- ACTION 2: edit the template (model change) bumps to v2 -----------
    let put_v2 = client
        .tools_call(
            "agent_template_put",
            json!({
                "template_id": "reviewer",
                "name": "Code reviewer",
                "agent_kind": "claude",
                "model": "claude-sonnet-4-6",
                "prompt_template": "Review ${repo} focusing on ${area}.",
                "required_params": ["repo", "area"],
                "working_dir": "C:\\code\\Synapse"
            }),
        )
        .await?;
    let put_v2 = structured(&put_v2)?;
    println!("readback=agent_template_put edge=edit state={put_v2}");
    ensure!(
        put_v2["created"] == json!(false) && put_v2["template"]["version"] == json!(2),
        "edit must bump to v2 and not be 'created', got {put_v2}"
    );
    ensure!(
        put_v2["template"]["config_hash"].as_str() != Some(hash_v1.as_str()),
        "changing the model must change config_hash"
    );
    ensure!(
        put_v2["template"]["created_unix_ms"] == put_v1["template"]["created_unix_ms"],
        "created_unix_ms must be preserved across an edit"
    );

    // ---- VERIFY: current is v2, but v1 snapshot is still resolvable -------
    let got_current = client
        .tools_call("agent_template_get", json!({"template_id": "reviewer"}))
        .await?;
    let got_current = structured(&got_current)?;
    ensure!(
        got_current["template"]["version"] == json!(2)
            && got_current["template"]["model"] == json!("claude-sonnet-4-6"),
        "current must now be v2, got {got_current}"
    );
    let got_historic = client
        .tools_call(
            "agent_template_get",
            json!({"template_id": "reviewer", "version": 1}),
        )
        .await?;
    let got_historic = structured(&got_historic)?;
    println!("readback=agent_template_get edge=historical_v1 state={got_historic}");
    ensure!(
        got_historic["template"]["version"] == json!(1)
            && got_historic["template"]["model"] == json!("claude-opus-4-8")
            && got_historic["template"]["config_hash"] == json!(hash_v1)
            && got_historic["row_key"] == json!("agent-template/v1/ver/reviewer/0000000001"),
        "v1 snapshot must remain exactly reproducible, got {got_historic}"
    );

    // ---- EDGE: a second template so list ordering is meaningful -----------
    client
        .tools_call(
            "agent_template_put",
            json!({
                "template_id": "idle-codex",
                "name": "Idle Codex",
                "agent_kind": "codex",
                "required_params": [],
                "prompt_template": "Stand by."
            }),
        )
        .await?;
    let listed = client.tools_call("agent_template_list", json!({})).await?;
    let listed = structured(&listed)?;
    println!("readback=agent_template_list edge=two_templates state={listed}");
    ensure!(listed["count"] == json!(2), "two current templates expected");
    let ids: Vec<&str> = listed["templates"]
        .as_array()
        .context("templates array")?
        .iter()
        .filter_map(|t| t["template_id"].as_str())
        .collect();
    ensure!(
        ids == vec!["idle-codex", "reviewer"],
        "list must be sorted by id, got {ids:?}"
    );

    // ---- EDGE: unknown agent_kind rejected loudly ------------------------
    let bad_kind = client
        .tools_call_error(
            "agent_template_put",
            json!({
                "template_id": "bad",
                "name": "Bad",
                "agent_kind": "gpt-5",
                "required_params": []
            }),
        )
        .await?;
    let bad_kind = bad_kind.to_string();
    println!("readback=agent_template_put edge=unknown_kind err={bad_kind}");
    ensure!(
        bad_kind.contains("TOOL_PARAMS_INVALID") && bad_kind.contains("not supported"),
        "unknown agent_kind must be rejected loudly, got {bad_kind}"
    );

    // ---- EDGE: prompt placeholders must match required_params ------------
    let bad_slots = client
        .tools_call_error(
            "agent_template_put",
            json!({
                "template_id": "mismatch",
                "name": "Mismatch",
                "agent_kind": "claude",
                "prompt_template": "Do ${x} and ${y}.",
                "required_params": ["x"]
            }),
        )
        .await?;
    let bad_slots = bad_slots.to_string();
    println!("readback=agent_template_put edge=slot_mismatch err={bad_slots}");
    ensure!(
        bad_slots.contains("placeholders not declared") && bad_slots.contains("y"),
        "placeholder/param mismatch must be rejected, got {bad_slots}"
    );

    // ---- ACTION 3: delete the reviewer's current pointer -----------------
    let deleted = client
        .tools_call("agent_template_delete", json!({"template_id": "reviewer"}))
        .await?;
    let deleted = structured(&deleted)?;
    println!("readback=agent_template_delete edge=delete state={deleted}");
    ensure!(
        deleted["deleted_version"] == json!(2)
            && deleted["retained_version_snapshots"] == json!(2),
        "delete reports the live version and retains both snapshots, got {deleted}"
    );

    // ---- VERIFY: current get now fails; v1 snapshot still resolves --------
    let gone = client
        .tools_call_error("agent_template_get", json!({"template_id": "reviewer"}))
        .await?;
    let gone = gone.to_string();
    println!("readback=agent_template_get edge=after_delete err={gone}");
    ensure!(
        gone.contains("AGENT_TEMPLATE_NOT_FOUND"),
        "deleted current pointer must be gone, got {gone}"
    );
    let snapshot_after_delete = client
        .tools_call(
            "agent_template_get",
            json!({"template_id": "reviewer", "version": 1}),
        )
        .await?;
    ensure!(
        structured(&snapshot_after_delete)?["template"]["version"] == json!(1),
        "version snapshots must survive a delete for run reproducibility"
    );

    // ---- EDGE: spawn from a deleted template errors, nothing launched ----
    let spawn_deleted = client
        .tools_call_error(
            "act_spawn_agent",
            json!({"template_id": "reviewer", "template_params": {"repo": "x", "area": "y"}}),
        )
        .await?;
    let spawn_deleted = spawn_deleted.to_string();
    println!("readback=act_spawn_agent edge=template_deleted err={spawn_deleted}");
    ensure!(
        spawn_deleted.contains("AGENT_TEMPLATE_NOT_FOUND"),
        "spawning from a deleted template must fail, got {spawn_deleted}"
    );

    // ---- EDGE: spawn param contract — missing required param -------------
    let spawn_missing = client
        .tools_call_error(
            "act_spawn_agent",
            json!({"template_id": "idle-codex", "template_params": {"oops": "1"}}),
        )
        .await?;
    let spawn_missing = spawn_missing.to_string();
    println!("readback=act_spawn_agent edge=unknown_param err={spawn_missing}");
    ensure!(
        spawn_missing.contains("unknown template_params"),
        "idle template takes no params; an unknown param must be rejected, got {spawn_missing}"
    );

    // ---- EDGE: spawn conflict — template_id alongside template-owned field
    let spawn_conflict = client
        .tools_call_error(
            "act_spawn_agent",
            json!({"template_id": "idle-codex", "cli": "claude"}),
        )
        .await?;
    let spawn_conflict = spawn_conflict.to_string();
    println!("readback=act_spawn_agent edge=field_conflict err={spawn_conflict}");
    ensure!(
        spawn_conflict.contains("cli") && spawn_conflict.contains("template"),
        "passing a template-owned field alongside template_id must be rejected, got {spawn_conflict}"
    );

    // ---- PHYSICAL SOURCE-OF-TRUTH VERIFICATION ---------------------------
    // Shut the daemon down (releasing the RocksDB lock), then open the very
    // same on-disk database directly and scan the physical CF_KV rows. This is
    // the strongest possible proof: bytes written by the daemon process, read
    // back by an independent reader straight from the database files.
    let status = client.shutdown().await?;
    ensure!(status.success(), "daemon must exit cleanly");

    let db = Db::open(&db_path, SCHEMA_VERSION).context("open daemon RocksDB directly")?;
    let rows = db
        .scan_cf_prefix(cf::CF_KV, b"agent-template/v1/")
        .context("scan CF_KV for template rows")?;
    let keys: Vec<String> = rows
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    println!("readback=cf_kv edge=physical_rows keys={keys:?}");

    // reviewer: pointer deleted, but v1 + v2 snapshots remain.
    ensure!(
        !keys.contains(&"agent-template/v1/cur/reviewer".to_owned()),
        "deleted reviewer pointer must be physically absent, keys={keys:?}"
    );
    for expected in [
        "agent-template/v1/ver/reviewer/0000000001",
        "agent-template/v1/ver/reviewer/0000000002",
        "agent-template/v1/cur/idle-codex",
        "agent-template/v1/ver/idle-codex/0000000001",
    ] {
        ensure!(
            keys.contains(&expected.to_owned()),
            "expected physical row {expected} missing; keys={keys:?}"
        );
    }

    // Decode the v1 snapshot row and prove its field-level contents on disk.
    let (_, v1_bytes) = rows
        .iter()
        .find(|(k, _)| k == b"agent-template/v1/ver/reviewer/0000000001")
        .context("v1 snapshot row missing on disk")?;
    let v1_row: Value = serde_json::from_slice(v1_bytes).context("decode v1 snapshot row")?;
    println!("readback=cf_kv edge=decoded_v1 row={v1_row}");
    ensure!(
        v1_row["template_id"] == json!("reviewer")
            && v1_row["version"] == json!(1)
            && v1_row["model"] == json!("claude-opus-4-8")
            && v1_row["config_hash"] == json!(hash_v1)
            && v1_row["required_params"] == json!(["repo", "area"]),
        "on-disk v1 snapshot must match what the tool reported, got {v1_row}"
    );

    Ok(())
}
