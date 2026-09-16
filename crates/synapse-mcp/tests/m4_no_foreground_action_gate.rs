//! End-to-end regression coverage for #1061: the background daemon must serve
//! non-foreground action-gated tools even when no window is focused (locked
//! screen, desktop focus, unattended session), while foreground-driving tools
//! stay fail-closed.
//!
//! These tests drive the REAL daemon over the REAL stdio MCP transport with the
//! `SYNAPSE_MCP_FORCE_NO_FOREGROUND` fixture, which reproduces the actual
//! `GetForegroundWindow returned null` condition. The test reads the daemon's
//! real persisted reflex registry (`reflex_list`), not just the tool return
//! value. Complements the in-module gate unit tests with proof over the real
//! transport. Manual FSV is still required before issue closure.

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use tempfile::TempDir;

/// `reflex_register` (named explicitly in #1061) must succeed when no foreground
/// window exists: registration does not drive the foreground, so it degrades to
/// scope-against-active-profile instead of hard-failing `A11Y_NO_FOREGROUND`.
/// Source of truth: the reflex is physically present in the registry afterwards.
#[tokio::test]
async fn reflex_register_succeeds_with_no_foreground_window() -> anyhow::Result<()> {
    let db = TempDir::new()?;
    let db_path = db.path().join("db");
    let db_path = db_path.to_string_lossy().into_owned();

    let mut client = StdioMcpClient::launch_and_init_with_env(
        None,
        &[
            ("SYNAPSE_DB", db_path.as_str()),
            // Reproduce the real "no focused window" condition; this also
            // disables the harness's default synthetic notepad foreground.
            ("SYNAPSE_MCP_FORCE_NO_FOREGROUND", "1"),
        ],
    )
    .await?;

    // Pre-state: the registry is empty of our reflex.
    let before = client.tools_call("reflex_list", json!({})).await?;
    ensure!(
        !serde_json::to_string(&before)?.contains("no-foreground-reflex"),
        "pre-state: reflex must not exist before registration"
    );

    // The action gate runs here, reading the (absent) foreground. With the fix
    // this no longer errors A11Y_NO_FOREGROUND for a non-foreground tool.
    let response = client
        .tools_call(
            "reflex_register",
            json!({
                "kind": "on_event",
                "when": {"op": "kind", "kind": "no-foreground-reflex"},
                "then": {"kind": "action", "action": {"kind": "release_all"}}
            }),
        )
        .await
        .context("reflex_register must not be blocked by A11Y_NO_FOREGROUND")?;
    let structured = structured(&response)?;
    let reflex_id = structured["reflex_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("reflex_id missing from registration response")?
        .to_owned();

    // Separate state readback: the reflex is physically in the registry.
    let after = client.tools_call("reflex_list", json!({})).await?;
    let after_text = serde_json::to_string(&after)?;
    ensure!(
        after_text.contains(&reflex_id),
        "post-state: registered reflex {reflex_id} not found in reflex_list registry: {after}"
    );

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

/// A foreground-driving input tool (`act_type`) must still FAIL CLOSED with
/// `A11Y_NO_FOREGROUND` when no window is focused — there is nothing to drive.
#[tokio::test]
async fn act_type_fails_closed_with_no_foreground_window() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init_with_env(
        None,
        &[
            ("SYNAPSE_MCP_FORCE_NO_FOREGROUND", "1"),
            // Expose the break-glass input tools so the call reaches the gate.
            ("SYNAPSE_DEBUG_TOOLS", "1"),
        ],
    )
    .await?;

    let error = client
        .tools_call_error(
            "act_type",
            json!({
                "text": "this must never be typed without a foreground window",
            }),
        )
        .await?;

    let code = error
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str);
    ensure!(
        code == Some("A11Y_NO_FOREGROUND"),
        "act_type must fail closed with A11Y_NO_FOREGROUND, got code={code:?} error={error}"
    );

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
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
