//! Supporting real-platform integration evidence for `notify_human` (#866);
//! manual FSV remains separate.
//!
//! These tests run against the real Windows notification platform — no mocks.
//! The daemon raises actual toasts (popup-suppressed so test runs do not spam
//! banners), and the test verifies delivery through two independent sources
//! of truth: Action Center history read directly via `WinRT` from the test
//! process, and the AUMID registration read back from the registry via
//! reg.exe.
#![cfg(windows)]

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use synapse_test_utils::stdio_mcp_client::StdioMcpClient;
use windows::{
    UI::Notifications::ToastNotificationManager,
    Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
    core::HSTRING,
};

const AUMID: &str = "Synapse.Daemon";
const GROUP: &str = "synapse";

/// Initialize COM on this thread and intentionally never uninitialize:
/// tearing down the last MTA invalidates windows-rs's cached `WinRT` factories
/// and later calls crash. Leaking the init is correct for a test process.
fn ensure_com() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Source-of-truth readback: count toasts with `tag` in Action Center history
/// for the Synapse AUMID, straight from `WinRT` in the test process.
fn action_center_count(tag: &str) -> anyhow::Result<u32> {
    ensure_com();
    let history = ToastNotificationManager::History().context("History()")?;
    let toasts = history
        .GetHistoryWithId(&HSTRING::from(AUMID))
        .context("GetHistoryWithId")?;
    let mut count = 0;
    for index in 0..toasts.Size().context("history Size()")? {
        let toast = toasts.GetAt(index).context("history GetAt")?;
        let toast_tag = toast.Tag().map(|t| t.to_string_lossy()).unwrap_or_default();
        let toast_group = toast
            .Group()
            .map(|g| g.to_string_lossy())
            .unwrap_or_default();
        if toast_tag == tag && toast_group == GROUP {
            count += 1;
        }
    }
    Ok(count)
}

/// Remove a test toast from Action Center so test runs clean up after
/// themselves and dedupe state resets between runs.
fn remove_from_action_center(tag: &str) -> anyhow::Result<()> {
    ensure_com();
    let history = ToastNotificationManager::History().context("History()")?;
    history
        .RemoveGroupedTagWithId(
            &HSTRING::from(tag),
            &HSTRING::from(GROUP),
            &HSTRING::from(AUMID),
        )
        .context("RemoveGroupedTagWithId")?;
    Ok(())
}

fn structured(response: &Value) -> anyhow::Result<&Value> {
    response
        .get("structuredContent")
        .with_context(|| format!("structuredContent missing from response: {response}"))
}

fn unique_marker() -> anyhow::Result<String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_nanos();
    Ok(format!("notify-regression-{nanos}-{}", std::process::id()))
}

#[tokio::test]
async fn notify_human_delivers_dedupes_and_redelivers_after_dismissal() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init().await?;
    let marker = unique_marker()?;

    // Happy path: synthetic input with a known dedupe_key.
    let response = client
        .tools_call(
            "notify_human",
            json!({
                "title": format!("Synapse notification regression {marker}"),
                "body": "happy-path toast raised by notify_human_toast.rs",
                "kind": "info",
                "dedupe_key": marker,
                "suppress_popup": true,
            }),
        )
        .await?;
    let payload = structured(&response)?;
    println!("readback=notify_human first call payload={payload}");
    assert_eq!(payload["shown"], json!(true), "first notify must show");
    assert_eq!(payload["deduped"], json!(false));
    assert_eq!(payload["verified_in_history"], json!(true));
    assert_eq!(payload["aumid"], json!(AUMID));
    assert_eq!(payload["group"], json!(GROUP));
    let tag = payload["tag"]
        .as_str()
        .context("tag missing from notify_human response")?
        .to_owned();
    ensure!(
        tag.starts_with("dk-"),
        "dedupe-keyed tag must be dk-…: {tag}"
    );

    // Source of truth #1: the toast is physically in Action Center, observed
    // from this test process independently of the daemon.
    let in_history = action_center_count(&tag)?;
    println!("readback=action_center tag={tag} count={in_history}");
    ensure!(
        in_history >= 1,
        "toast with tag {tag} not found in Action Center history"
    );

    // Source of truth #2: AUMID registration physically present in registry.
    let reg = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Classes\AppUserModelId\Synapse.Daemon",
            "/v",
            "DisplayName",
        ])
        .output()
        .context("spawning reg.exe")?;
    let reg_stdout = String::from_utf8_lossy(&reg.stdout);
    println!(
        "readback=registry status={:?} stdout={reg_stdout}",
        reg.status
    );
    ensure!(reg.status.success(), "AUMID registry key missing");
    ensure!(
        reg_stdout.contains("Synapse"),
        "DisplayName readback missing expected value: {reg_stdout}"
    );

    // Dedupe: same dedupe_key while the toast is still in Action Center must
    // suppress, not stack.
    let response = client
        .tools_call(
            "notify_human",
            json!({
                "title": format!("Synapse notification duplicate {marker}"),
                "body": "this repeat must be suppressed",
                "kind": "info",
                "dedupe_key": marker,
                "suppress_popup": true,
            }),
        )
        .await?;
    let payload = structured(&response)?;
    println!("readback=notify_human dedupe call payload={payload}");
    assert_eq!(payload["shown"], json!(false), "repeat must be suppressed");
    assert_eq!(payload["deduped"], json!(true));
    assert_eq!(payload["tag"], json!(tag.clone()));
    let after_dedupe = action_center_count(&tag)?;
    println!("readback=action_center after dedupe count={after_dedupe}");
    assert_eq!(after_dedupe, in_history, "dedupe must not add toasts");

    // Operator dismisses the toast → suppression clears → next notify shows.
    remove_from_action_center(&tag)?;
    let after_removal = action_center_count(&tag)?;
    println!("readback=action_center after removal count={after_removal}");
    assert_eq!(after_removal, 0);
    let response = client
        .tools_call(
            "notify_human",
            json!({
                "title": format!("Synapse notification redelivery {marker}"),
                "body": "after dismissal the same dedupe_key must deliver again",
                "kind": "warning",
                "dedupe_key": marker,
                "suppress_popup": true,
            }),
        )
        .await?;
    let payload = structured(&response)?;
    println!("readback=notify_human redelivery payload={payload}");
    assert_eq!(payload["shown"], json!(true));
    assert_eq!(payload["deduped"], json!(false));
    ensure!(action_center_count(&tag)? >= 1, "redelivered toast missing");

    remove_from_action_center(&tag)?;
    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn notify_human_without_dedupe_key_uses_unique_tags() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init().await?;
    let marker = unique_marker()?;
    let mut tags = Vec::new();
    for index in 0..2 {
        let response = client
            .tools_call(
                "notify_human",
                json!({
                    "title": format!("Synapse notification unique {marker} #{index}"),
                    "body": "",
                    "kind": "success",
                    "suppress_popup": true,
                }),
            )
            .await?;
        let payload = structured(&response)?;
        println!("readback=notify_human unique #{index} payload={payload}");
        assert_eq!(payload["shown"], json!(true));
        assert_eq!(payload["deduped"], json!(false));
        let tag = payload["tag"].as_str().context("tag missing")?.to_owned();
        ensure!(tag.starts_with("id-"), "keyless tag must be id-…: {tag}");
        ensure!(
            action_center_count(&tag)? >= 1,
            "toast {tag} not in history"
        );
        tags.push(tag);
    }
    ensure!(tags[0] != tags[1], "keyless notifies must not share tags");
    for tag in &tags {
        remove_from_action_center(tag)?;
    }
    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn notify_human_rejects_invalid_params_with_precise_errors() -> anyhow::Result<()> {
    let mut client = StdioMcpClient::launch_and_init().await?;

    // Edge 1: empty/whitespace title.
    let error = client
        .tools_call_error(
            "notify_human",
            json!({"title": "   ", "body": "b", "kind": "info"}),
        )
        .await?;
    println!("readback=edge empty-title error={error}");
    ensure!(
        error.to_string().contains("title must not be empty"),
        "empty title must be rejected precisely: {error}"
    );

    // Edge 2: oversized body (max 2000 chars).
    let error = client
        .tools_call_error(
            "notify_human",
            json!({"title": "t", "body": "x".repeat(2001), "kind": "info"}),
        )
        .await?;
    println!("readback=edge oversized-body error={error}");
    ensure!(
        error.to_string().contains("max 2000"),
        "oversized body must be rejected precisely: {error}"
    );

    // Edge 3: control characters that the toast XML payload cannot carry.
    let error = client
        .tools_call_error(
            "notify_human",
            json!({"title": "bel\u{0007}l", "body": "b", "kind": "info"}),
        )
        .await?;
    println!("readback=edge control-char error={error}");
    ensure!(
        error.to_string().contains("U+0007"),
        "control characters must be rejected precisely: {error}"
    );

    // Edge 4: unknown kind is a schema violation.
    let error = client
        .tools_call_error(
            "notify_human",
            json!({"title": "t", "body": "b", "kind": "shout"}),
        )
        .await?;
    println!("readback=edge bad-kind error={error}");

    // Edge 5: empty dedupe_key.
    let error = client
        .tools_call_error(
            "notify_human",
            json!({"title": "t", "body": "b", "kind": "info", "dedupe_key": " "}),
        )
        .await?;
    println!("readback=edge empty-dedupe error={error}");
    ensure!(
        error.to_string().contains("dedupe_key must not be empty"),
        "empty dedupe_key must be rejected precisely: {error}"
    );

    let status = client.shutdown().await?;
    assert!(status.success());
    Ok(())
}
