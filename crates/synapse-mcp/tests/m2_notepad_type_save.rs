#[cfg(windows)]
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant},
};

#[cfg(windows)]
use anyhow::Context;
#[cfg(windows)]
use serde::de::DeserializeOwned;
#[cfg(windows)]
use serde_json::{Value, json};
#[cfg(windows)]
use synapse_core::{AccessibleNode, ElementId, Observation, Point, Rect, UiaPattern};
#[cfg(windows)]
use synapse_test_utils::{fixtures::launch_notepad, stdio_mcp_client::StdioMcpClient};
#[cfg(windows)]
use tempfile::TempDir;

#[cfg(windows)]
static WINDOWS_NOTEPAD_LIVE_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(windows)]
const DEMO_FILE_NAME: &str = "synapse-m2-demo.txt";
#[cfg(windows)]
const DEMO_TEXT_TO_TYPE: &str = "Hello world.\nThis is Synapse.";
#[cfg(windows)]
const EXPECTED_DISK_TEXT: &str = "Hello world.\r\nThis is Synapse.";
#[cfg(windows)]
const INVALID_SAVE_PATH: &str = r"Z:\nope\synapse-m2-invalid-dir.txt";
#[cfg(windows)]
const INVALID_EDGE_CLEANUP_FILE_NAME: &str = "synapse-m2-invalid-edge-cleanup.txt";
#[cfg(windows)]
const DOUBLE_CLICK_TEXT: &str = "abcdef";
#[cfg(windows)]
const DOUBLE_CLICK_CLEANUP_FILE_NAME: &str = "synapse-m2-double-click-cleanup.txt";

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires an interactive Windows desktop with Notepad and UIA"]
// #206 keeps the full disk-save live regression path linear so before/action/after evidence
// prints in execution order instead of being hidden behind helper indirection.
#[allow(clippy::too_many_lines)]
async fn notepad_type_save_writes_byte_correct_file_live() -> anyhow::Result<()> {
    let _notepad_lock = WINDOWS_NOTEPAD_LIVE_LOCK.lock().await;
    let target_path = std::env::temp_dir().join(DEMO_FILE_NAME);
    let cleanup = FileCleanup(target_path.clone());
    let stale_before_exists = target_path.exists();
    let stale_before_len = if stale_before_exists {
        Some(std::fs::metadata(&target_path)?.len())
    } else {
        None
    };
    println!(
        "readback=disk edge=stale_preexisting before_path={} before_exists={} before_len={:?}",
        target_path.display(),
        stale_before_exists,
        stale_before_len
    );
    if target_path.exists() {
        std::fs::remove_file(&target_path)
            .with_context(|| format!("remove stale demo file {}", target_path.display()))?;
    }
    println!(
        "readback=disk edge=happy before_path={} before_exists={}",
        target_path.display(),
        target_path.exists()
    );

    let log_dir = TempDir::new()?;
    let handle = launch_notepad()?;
    let hwnd = handle.hwnd();
    let pid = handle.pid();
    let pid_preexisting = handle.pid_preexisting();
    println!(
        "readback=NotepadHandle edge=ownership after_hwnd=0x{hwnd:x} after_pid={pid} pid_preexisting={pid_preexisting}"
    );
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(log_dir.path())).await?;
    activate_notepad_profile(&mut client).await?;

    let editor_node = editor_node_from_uia_snapshot(hwnd)?;
    let editor_id = editor_node.element_id.clone();
    match synapse_a11y::focus_window_with_intent(
        hwnd,
        synapse_a11y::ForegroundActivationIntent::OperatorRequested {
            caller: "m2_notepad_type_save_test",
        },
    ) {
        Ok(()) => {
            println!(
                "readback=synapse_a11y::focus_window_with_intent edge=window after=ok hwnd=0x{hwnd:x}"
            );
        }
        Err(error) => {
            println!(
                "readback=synapse_a11y::focus_window_with_intent edge=window after_error={error}"
            );
        }
    }
    synapse_a11y::focus_element(&editor_id)
        .with_context(|| format!("set UIA focus on Notepad editor element {editor_id}"))?;
    println!(
        "readback=synapse_a11y::UIElement::set_focus edge=editor after=ok element_id={editor_id}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("readback=editor_element edge=click before_element_id={editor_id}");
    let click = client
        .tools_call(
            "act_click",
            json!({"target": {"element_id": editor_id.to_string()}, "use_invoke_pattern": true}),
        )
        .await?;
    let click_response: ActClickWireResponse = structured(&click)?;
    println!(
        "readback=mcp_act_click edge=editor_focus after=ok:{} used_invoke_pattern:{} backend_used:{} elapsed_ms:{}",
        click_response.ok,
        click_response.used_invoke_pattern,
        click_response.backend_used,
        click_response.elapsed_ms
    );
    assert!(click_response.ok);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let observe_before = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=before_type hwnd=0x{hwnd:x} pid={pid} title={:?} focused_role={:?} element_count={}",
        observe_before.foreground.window_title,
        observe_before
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str()),
        observe_before.elements.len()
    );
    assert_eq!(observe_before.foreground.hwnd, hwnd);
    assert_eq!(observe_before.foreground.pid, pid);

    let typed = client
        .tools_call(
            "act_type",
            json!({
                "text": DEMO_TEXT_TO_TYPE,
                "dynamics": "linear",
                "linear_ms_per_char": 30
            }),
        )
        .await?;
    let typed_response: ActTypeWireResponse = structured(&typed)?;
    println!(
        "readback=mcp_act_type edge=demo_text after=ok:{} chars_typed:{} elapsed_ms:{} expected_chars:{}",
        typed_response.ok,
        typed_response.chars_typed,
        typed_response.elapsed_ms,
        DEMO_TEXT_TO_TYPE.chars().count()
    );
    assert!(typed_response.ok);
    assert_eq!(
        typed_response.chars_typed as usize,
        DEMO_TEXT_TO_TYPE.chars().count()
    );

    let observe_after_type = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=after_type focused_role={:?} focused_value_len={:?}",
        observe_after_type
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str()),
        observe_after_type
            .focused
            .as_ref()
            .and_then(|focused| focused.value.as_ref())
            .map(String::len)
    );

    println!("readback=mcp_act_press edge=save_chord before=keys:[ctrl,s]");
    let save = client
        .tools_call("act_press", json!({"keys": ["ctrl", "s"], "hold_ms": 33}))
        .await?;
    let save_response: ActPressWireResponse = structured(&save)?;
    println!(
        "readback=mcp_act_press edge=save_chord after=ok:{} keys_pressed:{} backend_used:{} elapsed_ms:{}",
        save_response.ok,
        save_response.keys_pressed,
        save_response.backend_used,
        save_response.elapsed_ms
    );
    assert!(save_response.ok);
    assert_eq!(save_response.keys_pressed, 2);

    tokio::time::sleep(Duration::from_millis(500)).await;
    let observe_save_dialog = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=save_dialog after_title={:?} process={:?} focused_role={:?}",
        observe_save_dialog.foreground.window_title,
        observe_save_dialog.foreground.process_name,
        observe_save_dialog
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str())
    );

    let save_path_text = target_path.to_string_lossy().into_owned();
    println!(
        "readback=mcp_act_type edge=filename before_path={} before_len={}",
        save_path_text,
        save_path_text.chars().count()
    );
    let filename = client
        .tools_call(
            "act_type",
            json!({
                "text": save_path_text,
                "dynamics": "linear",
                "linear_ms_per_char": 20
            }),
        )
        .await?;
    let filename_response: ActTypeWireResponse = structured(&filename)?;
    println!(
        "readback=mcp_act_type edge=filename after=ok:{} chars_typed:{} elapsed_ms:{}",
        filename_response.ok, filename_response.chars_typed, filename_response.elapsed_ms
    );
    assert!(filename_response.ok);

    println!("readback=mcp_act_press edge=confirm_save before=keys:[enter]");
    let confirm = client
        .tools_call("act_press", json!({"keys": ["enter"], "hold_ms": 33}))
        .await?;
    let confirm_response: ActPressWireResponse = structured(&confirm)?;
    println!(
        "readback=mcp_act_press edge=confirm_save after=ok:{} keys_pressed:{} backend_used:{} elapsed_ms:{}",
        confirm_response.ok,
        confirm_response.keys_pressed,
        confirm_response.backend_used,
        confirm_response.elapsed_ms
    );
    assert!(confirm_response.ok);

    let after_text = wait_for_file_text(&target_path, Duration::from_secs(5))?;
    let first_50 = after_text.chars().take(50).collect::<String>();
    println!(
        "readback=disk edge=happy after_bytes={}:{}",
        after_text.len(),
        first_50.escape_debug()
    );
    assert_eq!(after_text, EXPECTED_DISK_TEXT);

    assert!(client.shutdown().await?.success());
    let logs = read_logs(log_dir.path())?;
    let contains_act_type = logs.contains("tool.invocation kind=act_type");
    let contains_act_press = logs.contains("tool.invocation kind=act_press");
    println!(
        "readback=daemon_log edge=happy after_bytes={} contains_act_type={} contains_act_press={}",
        logs.len(),
        contains_act_type,
        contains_act_press
    );
    assert!(contains_act_type);
    assert!(contains_act_press);

    handle.close()?;
    println!("readback=NotepadHandle::close edge=happy after=closed pid={pid}");
    std::fs::remove_file(&target_path)
        .with_context(|| format!("cleanup demo file {}", target_path.display()))?;
    println!(
        "readback=disk edge=cleanup after_exists={}",
        target_path.exists()
    );
    assert!(!target_path.exists());
    std::mem::forget(cleanup);
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires an interactive Windows desktop with Notepad and UIA"]
// #207 keeps the invalid-save live regression path linear so the disk/UIA/cleanup evidence
// remains ordered exactly like the real Windows flow.
#[allow(clippy::too_many_lines)]
async fn notepad_save_invalid_dir_shows_dialog_and_writes_no_file_live() -> anyhow::Result<()> {
    let _notepad_lock = WINDOWS_NOTEPAD_LIVE_LOCK.lock().await;
    let invalid_path = PathBuf::from(INVALID_SAVE_PATH);
    let cleanup_path = std::env::temp_dir().join(INVALID_EDGE_CLEANUP_FILE_NAME);
    let cleanup = FileCleanup(cleanup_path.clone());
    if cleanup_path.exists() {
        std::fs::remove_file(&cleanup_path)
            .with_context(|| format!("remove stale cleanup file {}", cleanup_path.display()))?;
    }
    println!(
        "readback=disk edge=invalid_dir before_path={} before_exists={} cleanup_path={} cleanup_before_exists={}",
        invalid_path.display(),
        invalid_path.exists(),
        cleanup_path.display(),
        cleanup_path.exists()
    );
    assert!(!invalid_path.exists());

    let log_dir = TempDir::new()?;
    let handle = launch_notepad()?;
    let hwnd = handle.hwnd();
    let pid = handle.pid();
    println!(
        "readback=NotepadHandle edge=invalid_dir ownership after_hwnd=0x{hwnd:x} after_pid={pid} pid_preexisting={}",
        handle.pid_preexisting()
    );
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(log_dir.path())).await?;
    activate_notepad_profile(&mut client).await?;

    let editor_node = editor_node_from_uia_snapshot(hwnd)?;
    let editor_id = editor_node.element_id.clone();
    focus_editor(hwnd, &editor_id)?;
    println!("readback=editor_element edge=invalid_dir before_element_id={editor_id}");
    let click = client
        .tools_call(
            "act_click",
            json!({"target": {"element_id": editor_id.to_string()}, "use_invoke_pattern": true}),
        )
        .await?;
    let click_response: ActClickWireResponse = structured(&click)?;
    println!(
        "readback=mcp_act_click edge=invalid_dir after=ok:{} used_invoke_pattern:{} backend_used:{} elapsed_ms:{}",
        click_response.ok,
        click_response.used_invoke_pattern,
        click_response.backend_used,
        click_response.elapsed_ms
    );
    assert!(click_response.ok);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let observe_before = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=invalid_dir before_type hwnd=0x{hwnd:x} pid={pid} title={:?}",
        observe_before.foreground.window_title
    );
    assert_eq!(observe_before.foreground.hwnd, hwnd);
    assert_eq!(observe_before.foreground.pid, pid);

    let typed = client
        .tools_call(
            "act_type",
            json!({
                "text": DEMO_TEXT_TO_TYPE,
                "dynamics": "linear",
                "linear_ms_per_char": 30
            }),
        )
        .await?;
    let typed_response: ActTypeWireResponse = structured(&typed)?;
    println!(
        "readback=mcp_act_type edge=invalid_dir_body after=ok:{} chars_typed:{} elapsed_ms:{}",
        typed_response.ok, typed_response.chars_typed, typed_response.elapsed_ms
    );
    assert!(typed_response.ok);

    press_keys(&mut client, "invalid_dir_save_chord", json!(["ctrl", "s"])).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let save_dialog = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=invalid_dir_save_dialog after_title={:?} process={:?} focused_role={:?}",
        save_dialog.foreground.window_title,
        save_dialog.foreground.process_name,
        save_dialog
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str())
    );

    println!(
        "readback=mcp_act_type edge=invalid_dir_filename before_path={} before_len={}",
        INVALID_SAVE_PATH,
        INVALID_SAVE_PATH.chars().count()
    );
    let filename = client
        .tools_call(
            "act_type",
            json!({
                "text": INVALID_SAVE_PATH,
                "dynamics": "linear",
                "linear_ms_per_char": 20
            }),
        )
        .await?;
    let filename_response: ActTypeWireResponse = structured(&filename)?;
    println!(
        "readback=mcp_act_type edge=invalid_dir_filename after=ok:{} chars_typed:{} elapsed_ms:{}",
        filename_response.ok, filename_response.chars_typed, filename_response.elapsed_ms
    );
    assert!(filename_response.ok);

    press_keys(&mut client, "invalid_dir_confirm_save", json!(["enter"])).await?;
    let invalid_dialog = wait_for_invalid_save_dialog(&mut client, Duration::from_secs(5)).await?;
    let dialog_text = summarize_nodes(&invalid_dialog.elements);
    let dialog_title = invalid_dialog.foreground.window_title.clone();
    println!(
        "readback=disk edge=invalid_dir after_exists={}; readback=uia edge=invalid_dir after_dialog_title={:?} focused_role={:?} matched_text={}",
        invalid_path.exists(),
        dialog_title,
        invalid_dialog
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str()),
        dialog_text
    );
    assert!(!invalid_path.exists());
    assert!(dialog_mentions_invalid_path(&invalid_dialog));

    press_keys(&mut client, "invalid_dir_dismiss_error", json!(["escape"])).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cleanup_dialog = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=invalid_dir_cleanup_dialog before_select hwnd=0x{:x} pid={} title={:?} process={:?} focused_role={:?}",
        cleanup_dialog.foreground.hwnd,
        cleanup_dialog.foreground.pid,
        cleanup_dialog.foreground.window_title,
        cleanup_dialog.foreground.process_name,
        cleanup_dialog
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str())
    );
    assert!(
        cleanup_dialog
            .foreground
            .window_title
            .eq_ignore_ascii_case("Save as"),
        "expected Save as foreground after dismissing invalid path dialog, got {:?}",
        cleanup_dialog.foreground.window_title
    );
    press_keys(
        &mut client,
        "invalid_dir_select_filename",
        json!(["ctrl", "a"]),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cleanup_dialog_after_select = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=invalid_dir_cleanup_dialog after_select hwnd=0x{:x} pid={} title={:?} process={:?} focused_role={:?}",
        cleanup_dialog_after_select.foreground.hwnd,
        cleanup_dialog_after_select.foreground.pid,
        cleanup_dialog_after_select.foreground.window_title,
        cleanup_dialog_after_select.foreground.process_name,
        cleanup_dialog_after_select
            .focused
            .as_ref()
            .map(|focused| focused.role.as_str())
    );
    assert!(
        cleanup_dialog_after_select
            .foreground
            .window_title
            .eq_ignore_ascii_case("Save as"),
        "expected Save as foreground after selecting cleanup filename, got {:?}",
        cleanup_dialog_after_select.foreground.window_title
    );

    let cleanup_path_text = cleanup_path.to_string_lossy().into_owned();
    println!(
        "readback=mcp_act_type edge=invalid_dir_cleanup_filename before_path={} before_len={}",
        cleanup_path_text,
        cleanup_path_text.chars().count()
    );
    let cleanup_filename = client
        .tools_call(
            "act_type",
            json!({
                "text": cleanup_path_text,
                "dynamics": "linear",
                "linear_ms_per_char": 20
            }),
        )
        .await?;
    let cleanup_filename_response: ActTypeWireResponse = structured(&cleanup_filename)?;
    println!(
        "readback=mcp_act_type edge=invalid_dir_cleanup_filename after=ok:{} chars_typed:{} elapsed_ms:{}",
        cleanup_filename_response.ok,
        cleanup_filename_response.chars_typed,
        cleanup_filename_response.elapsed_ms
    );
    assert!(cleanup_filename_response.ok);
    press_keys(&mut client, "invalid_dir_cleanup_confirm", json!(["enter"])).await?;
    let cleanup_text = wait_for_file_text(&cleanup_path, Duration::from_secs(5))?;
    println!(
        "readback=disk edge=invalid_dir_cleanup_save after_exists={} after_bytes={}",
        cleanup_path.exists(),
        cleanup_text.len()
    );
    assert_eq!(cleanup_text, EXPECTED_DISK_TEXT);

    assert!(client.shutdown().await?.success());
    let logs = read_logs(log_dir.path())?;
    let contains_act_type = logs.contains("tool.invocation kind=act_type");
    let contains_act_press = logs.contains("tool.invocation kind=act_press");
    println!(
        "readback=daemon_log edge=invalid_dir after_bytes={} contains_act_type={} contains_act_press={}",
        logs.len(),
        contains_act_type,
        contains_act_press
    );
    assert!(contains_act_type);
    assert!(contains_act_press);

    handle.close()?;
    println!("readback=NotepadHandle::close edge=invalid_dir after=closed pid={pid}");
    std::fs::remove_file(&cleanup_path)
        .with_context(|| format!("cleanup valid save file {}", cleanup_path.display()))?;
    println!(
        "readback=disk edge=invalid_dir cleanup_after_exists={} invalid_after_exists={}",
        cleanup_path.exists(),
        invalid_path.exists()
    );
    assert!(!cleanup_path.exists());
    assert!(!invalid_path.exists());
    std::mem::forget(cleanup);
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires an interactive Windows desktop with Notepad and UIA"]
async fn notepad_act_type_foreground_lost_returns_error_without_recording_events_live()
-> anyhow::Result<()> {
    let _notepad_lock = WINDOWS_NOTEPAD_LIVE_LOCK.lock().await;
    let log_dir = TempDir::new()?;
    println!("readback=foreground edge=lost before=target_absent");
    let target = launch_notepad()?;
    let target_hwnd = target.hwnd();
    let target_pid = target.pid();
    let target_editor_id = editor_from_uia_snapshot(target_hwnd)?;
    focus_editor(target_hwnd, &target_editor_id)?;

    let mut client = StdioMcpClient::launch_and_init_with_env(
        Some(log_dir.path()),
        &[("SYNAPSE_MCP_RECORDING_BACKEND", "1")],
    )
    .await?;
    activate_notepad_profile(&mut client).await?;
    let observed_target = observe(&mut client).await?;
    println!(
        "readback=foreground edge=lost before_hwnd=0x{:x} before_pid={} before_title={:?}",
        observed_target.foreground.hwnd,
        observed_target.foreground.pid,
        observed_target.foreground.window_title
    );
    assert_eq!(observed_target.foreground.hwnd, target_hwnd);
    assert_eq!(observed_target.foreground.pid, target_pid);

    let distractor = launch_notepad()?;
    let distractor_hwnd = distractor.hwnd();
    let distractor_pid = distractor.pid();
    let distractor_editor_id = editor_from_uia_snapshot(distractor_hwnd)?;
    focus_editor(distractor_hwnd, &distractor_editor_id)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let actual_foreground = synapse_a11y::current_foreground_context()
        .context("read current foreground after focusing distractor Notepad")?;
    println!(
        "readback=foreground edge=lost after_hwnd=0x{:x} after_pid={} after_title={:?} distractor_hwnd=0x{:x}",
        actual_foreground.hwnd,
        actual_foreground.pid,
        actual_foreground.window_title,
        distractor_hwnd
    );
    assert_eq!(actual_foreground.hwnd, distractor_hwnd);
    assert_eq!(actual_foreground.pid, distractor_pid);
    assert_ne!(actual_foreground.hwnd, observed_target.foreground.hwnd);

    let error = client
        .tools_call_error(
            "act_type",
            json!({
                "text": "X",
                "dynamics": "linear",
                "linear_ms_per_char": 30
            }),
        )
        .await?;
    let code = error_code(&error);
    println!(
        "readback=foreground edge=lost before_hwnd=0x{:x} after_hwnd=0x{:x} code={:?} raw_error={error}",
        observed_target.foreground.hwnd, actual_foreground.hwnd, code
    );
    assert_eq!(code, Some("ACTION_FOREGROUND_LOST"));

    assert!(client.shutdown().await?.success());
    let logs = read_logs(log_dir.path())?;
    let contains_guard = logs.contains("readback=foreground edge=lost")
        && logs.contains("recording_events_before=0")
        && logs.contains("recording_events_after=0");
    println!(
        "readback=recording_backend edge=foreground_lost after_log_bytes={} contains_zero_event_guard={}",
        logs.len(),
        contains_guard
    );
    assert!(contains_guard);

    distractor.close()?;
    println!(
        "readback=NotepadHandle::close edge=foreground_lost distractor_after=closed pid={distractor_pid}"
    );
    target.close()?;
    println!(
        "readback=NotepadHandle::close edge=foreground_lost target_after=closed pid={target_pid}"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires an interactive Windows desktop with Notepad and UIA"]
// #189 keeps the click/copy/readback sequence linear so the clipboard SoT is
// visibly tied to the exact double-click action that produced it.
#[allow(clippy::too_many_lines)]
async fn notepad_double_click_selects_word_and_clipboard_reads_selection_live() -> anyhow::Result<()>
{
    let _notepad_lock = WINDOWS_NOTEPAD_LIVE_LOCK.lock().await;
    let cleanup_path = std::env::temp_dir().join(DOUBLE_CLICK_CLEANUP_FILE_NAME);
    let cleanup = FileCleanup(cleanup_path.clone());
    let stale_cleanup_before_exists = cleanup_path.exists();
    let stale_cleanup_before_len = if stale_cleanup_before_exists {
        Some(std::fs::metadata(&cleanup_path)?.len())
    } else {
        None
    };
    println!(
        "readback=disk edge=double_click_stale_cleanup before_path={} before_exists={} before_len={:?}",
        cleanup_path.display(),
        stale_cleanup_before_exists,
        stale_cleanup_before_len
    );
    if cleanup_path.exists() {
        std::fs::remove_file(&cleanup_path)
            .with_context(|| format!("remove stale cleanup file {}", cleanup_path.display()))?;
    }
    println!(
        "readback=disk edge=double_click_select cleanup_before_path={} cleanup_before_exists={}",
        cleanup_path.display(),
        cleanup_path.exists()
    );

    let log_dir = TempDir::new()?;
    let handle = launch_notepad()?;
    let hwnd = handle.hwnd();
    let pid = handle.pid();
    println!(
        "readback=NotepadHandle edge=double_click_select ownership after_hwnd=0x{hwnd:x} after_pid={pid} pid_preexisting={}",
        handle.pid_preexisting()
    );
    let mut client = StdioMcpClient::launch_and_init_with_log_dir(Some(log_dir.path())).await?;
    activate_notepad_profile(&mut client).await?;

    let original_clipboard = client
        .tools_call("act_clipboard", json!({"verb": "read"}))
        .await
        .ok()
        .and_then(|response| structured::<ActClipboardWireResponse>(&response).ok())
        .and_then(|response| response.text);
    println!(
        "readback=clipboard edge=double_click_select before_text_len={:?}",
        original_clipboard.as_ref().map(|text| text.chars().count())
    );
    let clear = client
        .tools_call("act_clipboard", json!({"verb": "clear"}))
        .await?;
    let clear_response: ActClipboardWireResponse = structured(&clear)?;
    println!(
        "readback=clipboard edge=double_click_select clear_before after_ok:{} cleared:{}",
        clear_response.ok, clear_response.cleared
    );
    assert!(clear_response.ok);
    assert!(clear_response.cleared);

    let editor_node = editor_node_from_uia_snapshot(hwnd)?;
    let editor_id = editor_node.element_id.clone();
    focus_editor(hwnd, &editor_id)?;
    let word_point = Point {
        x: editor_node.bbox.x.saturating_add(editor_node.bbox.w / 2),
        y: editor_node.bbox.y.saturating_add(editor_node.bbox.h / 2),
    };
    println!(
        "readback=editor_element edge=double_click_select before_element_id={editor_id} word_point={word_point:?}"
    );
    let click_focus = client
        .tools_call(
            "act_click",
            json!({"target": {"x": word_point.x, "y": word_point.y}}),
        )
        .await?;
    let click_focus_response: ActClickWireResponse = structured(&click_focus)?;
    println!(
        "readback=mcp_act_click edge=double_click_caret after=ok:{} used_invoke_pattern:{} backend_used:{} elapsed_ms:{}",
        click_focus_response.ok,
        click_focus_response.used_invoke_pattern,
        click_focus_response.backend_used,
        click_focus_response.elapsed_ms
    );
    assert!(click_focus_response.ok);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let observe_before = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=double_click_select before_type hwnd=0x{hwnd:x} pid={pid} title={:?}",
        observe_before.foreground.window_title
    );
    assert_eq!(observe_before.foreground.hwnd, hwnd);
    assert_eq!(observe_before.foreground.pid, pid);

    let typed = client
        .tools_call(
            "act_type",
            json!({
                "text": DOUBLE_CLICK_TEXT,
                "dynamics": "linear",
                "linear_ms_per_char": 30
            }),
        )
        .await?;
    let typed_response: ActTypeWireResponse = structured(&typed)?;
    println!(
        "readback=mcp_act_type edge=double_click_select after=ok:{} chars_typed:{} elapsed_ms:{}",
        typed_response.ok, typed_response.chars_typed, typed_response.elapsed_ms
    );
    assert!(typed_response.ok);
    assert_eq!(typed_response.chars_typed as usize, DOUBLE_CLICK_TEXT.len());

    println!(
        "readback=windows_cursor edge=double_click_select before_point={word_point:?} expected_text={DOUBLE_CLICK_TEXT}"
    );
    let double_click = client
        .tools_call(
            "act_click",
            json!({"target": {"x": word_point.x, "y": word_point.y}, "clicks": 2}),
        )
        .await?;
    let double_click_response: ActClickWireResponse = structured(&double_click)?;
    println!(
        "readback=mcp_act_click edge=double_click_select after=ok:{} clicks:2 used_invoke_pattern:{} backend_used:{} elapsed_ms:{}",
        double_click_response.ok,
        double_click_response.used_invoke_pattern,
        double_click_response.backend_used,
        double_click_response.elapsed_ms
    );
    assert!(double_click_response.ok);

    press_keys(&mut client, "double_click_copy", json!(["ctrl", "c"])).await?;
    let read = client
        .tools_call("act_clipboard", json!({"verb": "read"}))
        .await?;
    let read_response: ActClipboardWireResponse = structured(&read)?;
    let selected_text = read_response.text.unwrap_or_default();
    println!("readback=clipboard edge=double_click_select after_text={selected_text}");
    let selection_matches = selected_text == DOUBLE_CLICK_TEXT;

    if let Some(original_clipboard) = original_clipboard {
        let restore = client
            .tools_call(
                "act_clipboard",
                json!({"verb": "write", "text": original_clipboard}),
            )
            .await?;
        let restore_response: ActClipboardWireResponse = structured(&restore)?;
        println!(
            "readback=clipboard edge=double_click_select restore_after_ok:{} written:{} text_len:{:?}",
            restore_response.ok, restore_response.written, restore_response.text_len
        );
        assert!(restore_response.ok);
    } else {
        let clear = client
            .tools_call("act_clipboard", json!({"verb": "clear"}))
            .await?;
        let clear_response: ActClipboardWireResponse = structured(&clear)?;
        println!(
            "readback=clipboard edge=double_click_select restore_clear_after_ok:{} cleared:{}",
            clear_response.ok, clear_response.cleared
        );
        assert!(clear_response.ok);
    }

    press_keys(
        &mut client,
        "double_click_cleanup_save_chord",
        json!(["ctrl", "s"]),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cleanup_save_dialog = observe(&mut client).await?;
    println!(
        "readback=mcp_observe edge=double_click_cleanup_save_dialog after_title={:?} process={:?}",
        cleanup_save_dialog.foreground.window_title, cleanup_save_dialog.foreground.process_name
    );
    let cleanup_path_text = cleanup_path.to_string_lossy().into_owned();
    println!(
        "readback=mcp_act_type edge=double_click_cleanup_filename before_path={} before_len={}",
        cleanup_path_text,
        cleanup_path_text.chars().count()
    );
    let cleanup_filename = client
        .tools_call(
            "act_type",
            json!({
                "text": cleanup_path_text,
                "dynamics": "linear",
                "linear_ms_per_char": 20
            }),
        )
        .await?;
    let cleanup_filename_response: ActTypeWireResponse = structured(&cleanup_filename)?;
    println!(
        "readback=mcp_act_type edge=double_click_cleanup_filename after=ok:{} chars_typed:{} elapsed_ms:{}",
        cleanup_filename_response.ok,
        cleanup_filename_response.chars_typed,
        cleanup_filename_response.elapsed_ms
    );
    assert!(cleanup_filename_response.ok);
    press_keys(
        &mut client,
        "double_click_cleanup_confirm",
        json!(["enter"]),
    )
    .await?;
    let cleanup_text = wait_for_file_text(&cleanup_path, Duration::from_secs(5))?;
    println!(
        "readback=disk edge=double_click_cleanup_save after_exists={} after_text={cleanup_text:?}",
        cleanup_path.exists()
    );
    assert_eq!(cleanup_text, DOUBLE_CLICK_TEXT);

    assert!(client.shutdown().await?.success());
    let logs = read_logs(log_dir.path())?;
    let contains_act_click = logs.contains("tool.invocation kind=act_click");
    let contains_act_clipboard = logs.contains("tool.invocation kind=act_clipboard");
    println!(
        "readback=daemon_log edge=double_click_select after_bytes={} contains_act_click={} contains_act_clipboard={}",
        logs.len(),
        contains_act_click,
        contains_act_clipboard
    );
    assert!(contains_act_click);
    assert!(contains_act_clipboard);

    handle.close()?;
    println!("readback=NotepadHandle::close edge=double_click_select after=closed pid={pid}");
    std::fs::remove_file(&cleanup_path)
        .with_context(|| format!("cleanup double-click save file {}", cleanup_path.display()))?;
    println!(
        "readback=disk edge=double_click_select cleanup_after_exists={}",
        cleanup_path.exists()
    );
    assert!(!cleanup_path.exists());
    std::mem::forget(cleanup);
    assert!(
        selection_matches,
        "double-click clipboard text mismatch: expected {DOUBLE_CLICK_TEXT:?}, got {selected_text:?}"
    );
    Ok(())
}

#[cfg(windows)]
struct FileCleanup(PathBuf);

#[cfg(windows)]
impl Drop for FileCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
async fn observe(client: &mut StdioMcpClient) -> anyhow::Result<Observation> {
    let response = client.tools_call("observe", json!({})).await?;
    structured(&response)
}

#[cfg(windows)]
fn focus_editor(hwnd: i64, editor_id: &ElementId) -> anyhow::Result<()> {
    match synapse_a11y::focus_window_with_intent(
        hwnd,
        synapse_a11y::ForegroundActivationIntent::OperatorRequested {
            caller: "m2_notepad_type_save_focus_editor",
        },
    ) {
        Ok(()) => {
            println!(
                "readback=synapse_a11y::focus_window_with_intent edge=window after=ok hwnd=0x{hwnd:x}"
            );
        }
        Err(error) => {
            println!(
                "readback=synapse_a11y::focus_window_with_intent edge=window after_error={error}"
            );
        }
    }
    synapse_a11y::focus_element(editor_id)
        .with_context(|| format!("set UIA focus on Notepad editor element {editor_id}"))?;
    println!(
        "readback=synapse_a11y::UIElement::set_focus edge=editor after=ok element_id={editor_id}"
    );
    Ok(())
}

#[cfg(windows)]
async fn activate_notepad_profile(client: &mut StdioMcpClient) -> anyhow::Result<()> {
    let response = client
        .tools_call("profile_activate", json!({"profile_id": "notepad"}))
        .await?;
    let active: Value = structured(&response)?;
    assert_eq!(active["active_profile_id"], "notepad");
    Ok(())
}

#[cfg(windows)]
async fn press_keys(client: &mut StdioMcpClient, edge: &str, keys: Value) -> anyhow::Result<()> {
    println!("readback=mcp_act_press edge={edge} before=keys:{keys}");
    let response = client
        .tools_call("act_press", json!({"keys": keys, "hold_ms": 33}))
        .await?;
    let response: ActPressWireResponse = structured(&response)?;
    println!(
        "readback=mcp_act_press edge={edge} after=ok:{} keys_pressed:{} backend_used:{} elapsed_ms:{}",
        response.ok, response.keys_pressed, response.backend_used, response.elapsed_ms
    );
    assert!(response.ok);
    Ok(())
}

#[cfg(windows)]
async fn wait_for_invalid_save_dialog(
    client: &mut StdioMcpClient,
    timeout: Duration,
) -> anyhow::Result<Observation> {
    let start = Instant::now();
    let mut last_observation = None;
    while start.elapsed() <= timeout {
        let observation = observe(client).await?;
        if dialog_mentions_invalid_path(&observation) {
            return Ok(observation);
        }
        last_observation = Some(observation);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let summary = last_observation.as_ref().map_or_else(
        || "no observation captured".to_owned(),
        |observation| {
            format!(
                "title={:?}; focused={:?}; nodes={}",
                observation.foreground.window_title,
                observation
                    .focused
                    .as_ref()
                    .map(|focused| (&focused.role, &focused.name)),
                summarize_nodes(&observation.elements)
            )
        },
    );
    Err(anyhow::anyhow!(
        "invalid save dialog did not appear before timeout; last={summary}"
    ))
}

#[cfg(windows)]
fn dialog_mentions_invalid_path(observation: &Observation) -> bool {
    let haystack = format!(
        "{} {} {}",
        observation.foreground.window_title,
        observation
            .focused
            .as_ref()
            .map_or("", |focused| focused.name.as_str()),
        observation
            .elements
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_ascii_lowercase();
    haystack.contains("z:\\")
        || haystack.contains("doesn't exist")
        || haystack.contains("does not exist")
        || haystack.contains("not found")
        || haystack.contains("path")
}

#[cfg(windows)]
fn editor_from_uia_snapshot(hwnd: i64) -> anyhow::Result<ElementId> {
    Ok(editor_node_from_uia_snapshot(hwnd)?.element_id)
}

#[cfg(windows)]
fn editor_node_from_uia_snapshot(hwnd: i64) -> anyhow::Result<AccessibleNode> {
    let subtree = synapse_a11y::snapshot_window_from_hwnd(hwnd, 4)
        .with_context(|| format!("snapshot Notepad hwnd 0x{hwnd:x}"))?;
    let target = subtree
        .nodes
        .iter()
        .filter(|node| node.enabled && node.bbox.w > 4 && node.bbox.h > 4)
        .max_by_key(|node| editor_score(node))
        .cloned()
        .with_context(|| {
            format!(
                "Notepad snapshot did not contain an editor-like element; nodes={}",
                summarize_nodes(&subtree.nodes)
            )
        })?;
    println!(
        "readback=synapse_a11y::snapshot edge=editor_fallback hwnd=0x{hwnd:x} node_count={} target={} role={:?} name={:?} bbox={:?} patterns={:?}",
        subtree.nodes.len(),
        target.element_id,
        target.role,
        target.name,
        target.bbox,
        target.patterns
    );
    Ok(target)
}

#[cfg(windows)]
fn is_editor_like(role: &str, patterns: &[UiaPattern]) -> bool {
    let role = role.to_ascii_lowercase();
    patterns.contains(&UiaPattern::Value)
        || patterns.contains(&UiaPattern::Text)
        || role.contains("edit")
        || role.contains("document")
        || role.contains("text")
}

#[cfg(windows)]
fn editor_score(node: &AccessibleNode) -> (bool, bool, bool, u32, i64) {
    let area = rect_area(node.bbox);
    (
        node.patterns.contains(&UiaPattern::Value),
        node.patterns.contains(&UiaPattern::Text),
        is_editor_like(&node.role, &node.patterns),
        node.depth,
        area,
    )
}

#[cfg(windows)]
fn rect_area(rect: Rect) -> i64 {
    i64::from(rect.w).saturating_mul(i64::from(rect.h))
}

#[cfg(windows)]
fn wait_for_file_text(path: &Path, timeout: Duration) -> anyhow::Result<String> {
    let start = Instant::now();
    let mut last_error = None;
    while start.elapsed() <= timeout {
        match std::fs::read_to_string(path) {
            Ok(text) => return Ok(text),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last_error.map_or_else(
        || anyhow::anyhow!("{} did not appear before timeout", path.display()),
        |error| anyhow::anyhow!("{} not readable before timeout: {error}", path.display()),
    ))
}

#[cfg(windows)]
fn summarize_nodes(nodes: &[AccessibleNode]) -> String {
    nodes
        .iter()
        .map(|node| {
            format!(
                "{{id:{},role:{:?},name:{:?},enabled:{},bbox:{:?},patterns:{:?}}}",
                node.element_id, node.role, node.name, node.enabled, node.bbox, node.patterns
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(windows)]
fn read_logs(path: &Path) -> anyhow::Result<String> {
    let mut logs = String::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.metadata()?.is_file() {
            logs.push_str(&std::fs::read_to_string(entry.path())?);
        }
    }
    Ok(logs)
}

#[cfg(windows)]
fn structured<T: DeserializeOwned>(resp: &Value) -> anyhow::Result<T> {
    serde_json::from_value(resp["structuredContent"].clone()).context("decode structuredContent")
}

#[cfg(windows)]
fn error_code(error: &Value) -> Option<&str> {
    error
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

#[cfg(windows)]
#[derive(serde::Deserialize)]
struct ActClickWireResponse {
    ok: bool,
    used_invoke_pattern: bool,
    backend_used: String,
    elapsed_ms: u32,
}

#[cfg(windows)]
#[derive(serde::Deserialize)]
struct ActTypeWireResponse {
    ok: bool,
    chars_typed: u32,
    elapsed_ms: u32,
}

#[cfg(windows)]
#[derive(serde::Deserialize)]
struct ActPressWireResponse {
    ok: bool,
    keys_pressed: u32,
    elapsed_ms: u32,
    backend_used: String,
}

#[cfg(windows)]
#[derive(serde::Deserialize)]
struct ActClipboardWireResponse {
    ok: bool,
    written: bool,
    cleared: bool,
    text: Option<String>,
    text_len: Option<usize>,
}
