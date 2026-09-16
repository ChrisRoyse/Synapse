use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use synapse_action::{
    ActionBackend, ActionEmitter, ActionEmitterSnapshotHandle, ActionHandle, ActionStateSnapshot,
    RecordingBackend,
};
use synapse_core::{
    Action, Backend, ButtonAction, GamepadReport, Key, KeyCode, MouseButton, PadButton,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::writer::MakeWriter;

#[tokio::test]
async fn emitter_release_all_logs_reasons_and_drained_contents() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let _trace_guard = tracing::subscriber::set_default(subscriber);

    let tool_before = run_tool_invocation_case().await;
    let shutdown_before = run_shutdown_case().await;
    let connection_before = run_connection_closed_case().await;

    let log_output = trace_buffer.text();
    println!("readback=release_all_log edge=raw after={log_output:?}");
    assert_reason_line_with_contents(
        &log_output,
        "tool_invocation",
        "tool-held",
        tool_before.held_key_bits.as_slice(),
        7,
    );
    assert_reason_line_with_contents(
        &log_output,
        "shutdown",
        "shutdown-held",
        shutdown_before.held_key_bits.as_slice(),
        8,
    );
    assert_reason_line_with_contents(
        &log_output,
        "connection_closed",
        "connection-held",
        connection_before.held_key_bits.as_slice(),
        9,
    );
}

async fn run_tool_invocation_case() -> ActionStateSnapshot {
    let cancel = CancellationToken::new();
    let (handle, snapshot, join) = spawn_emitter(cancel.clone(), "shutdown", None);
    let before = hold_synthetic_state(&handle, &snapshot, "tool-held", 7).await;
    println!("readback=action_snapshot edge=tool_invocation before={before:?}");

    handle
        .execute(Action::ReleaseAll)
        .await
        .unwrap_or_else(|error| panic!("tool release_all should execute: {error}"));
    let after = snapshot
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after tool release_all should succeed: {error}"));
    println!("readback=action_snapshot edge=tool_invocation after={after:?}");
    assert_empty(&after);

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("tool emitter join should complete: {error}"));
    assert_empty(&final_snapshot);
    before
}

async fn run_shutdown_case() -> ActionStateSnapshot {
    let cancel = CancellationToken::new();
    let (handle, snapshot, join) = spawn_emitter(cancel.clone(), "shutdown", None);
    let before = hold_synthetic_state(&handle, &snapshot, "shutdown-held", 8).await;
    println!("readback=action_snapshot edge=shutdown before={before:?}");

    cancel.cancel();
    let after = join
        .await
        .unwrap_or_else(|error| panic!("shutdown emitter join should complete: {error}"));
    println!("readback=action_snapshot edge=shutdown after={after:?}");
    assert_empty(&after);
    before
}

async fn run_connection_closed_case() -> ActionStateSnapshot {
    let shutdown = CancellationToken::new();
    let connection_closed = CancellationToken::new();
    let (handle, snapshot, join) =
        spawn_emitter(shutdown, "shutdown", Some(connection_closed.clone()));
    let before = hold_synthetic_state(&handle, &snapshot, "connection-held", 9).await;
    println!("readback=action_snapshot edge=connection_closed before={before:?}");

    connection_closed.cancel();
    let after = join
        .await
        .unwrap_or_else(|error| panic!("connection-closed emitter join should complete: {error}"));
    println!("readback=action_snapshot edge=connection_closed after={after:?}");
    assert_empty(&after);
    before
}

fn spawn_emitter(
    shutdown: CancellationToken,
    shutdown_reason: &'static str,
    connection_closed: Option<CancellationToken>,
) -> (
    ActionHandle,
    ActionEmitterSnapshotHandle,
    JoinHandle<ActionStateSnapshot>,
) {
    let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, snapshot, emitter) = ActionEmitter::channel_with_backend(backend);
    let join = tokio::spawn(emitter.run_with_shutdown_reason(
        shutdown,
        shutdown_reason,
        connection_closed,
    ));
    (handle, snapshot, join)
}

async fn hold_synthetic_state(
    handle: &ActionHandle,
    snapshot: &ActionEmitterSnapshotHandle,
    key_name: &str,
    pad: u8,
) -> ActionStateSnapshot {
    handle
        .execute(Action::KeyDown {
            key: key_named(key_name),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("KeyDown should execute: {error}"));
    handle
        .execute(Action::MouseButton {
            button: MouseButton::Left,
            action: ButtonAction::Down,
            hold_ms: 1,
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("MouseButton down should execute: {error}"));
    handle
        .execute(Action::PadReport {
            pad,
            report: non_neutral_report(),
        })
        .await
        .unwrap_or_else(|error| panic!("PadReport should execute: {error}"));

    let before = snapshot
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after holding synthetic state: {error}"));
    assert_eq!(before.held_keys, vec![key_named(key_name)]);
    assert_eq!(before.held_buttons, vec![MouseButton::Left]);
    assert!(before.pad_state.contains_key(&pad));
    before
}

fn assert_reason_line_with_contents(
    log_output: &str,
    reason: &str,
    key_name: &str,
    held_key_bits: &[usize],
    pad: u8,
) {
    let line = find_reason_line(log_output, reason, key_name);
    println!("readback=release_all_log edge={reason} after_line={line}");
    assert!(line.contains("code=\"SAFETY_RELEASE_ALL_FIRED\""));
    assert!(line.contains(&format!("reason=\"{reason}\"")));
    assert!(line.contains(&format!("value: \"{key_name}\"")));
    assert!(line.contains(&format!("held_key_bits={held_key_bits:?}")));
    assert!(line.contains("held_buttons=[Left]"));
    assert!(line.contains("held_button_bits=[0]"));
    assert!(line.contains(&format!("held_pad_ids=[{pad}]")));
    assert!(line.contains("released_keys=1"));
    assert!(line.contains("released_buttons=1"));
    assert!(line.contains("released_pads=1"));
}

fn find_reason_line(log_output: &str, reason: &str, key_name: &str) -> String {
    let needle = format!("reason=\"{reason}\"");
    log_output
        .lines()
        .find(|line| line.contains(&needle) && line.contains(&format!("value: \"{key_name}\"")))
        .map_or_else(
            || {
                panic!(
                    "expected release_all log reason {reason} with key {key_name}, got {log_output:?}"
                )
            },
            ToOwned::to_owned,
        )
}

fn assert_empty(snapshot: &ActionStateSnapshot) {
    assert!(snapshot.held_keys.is_empty());
    assert!(snapshot.held_key_bits.is_empty());
    assert!(snapshot.held_buttons.is_empty());
    assert!(snapshot.held_button_bits.is_empty());
    assert_eq!(snapshot.held_key_timer_count, 0);
    assert!(snapshot.pad_state.is_empty());
}

fn key_named(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

fn non_neutral_report() -> GamepadReport {
    GamepadReport {
        buttons: vec![PadButton::A],
        thumb_l: (0.25, -0.25),
        thumb_r: (0.0, 0.0),
        lt: 0.0,
        rt: 0.75,
        ..GamepadReport::default()
    }
}

#[derive(Clone, Default)]
struct SharedTraceBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedTraceBuffer {
    fn text(&self) -> String {
        let bytes = match self.bytes.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl<'a> MakeWriter<'a> for SharedTraceBuffer {
    type Writer = SharedTraceBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedTraceBufferWriter {
            bytes: Arc::clone(&self.bytes),
        }
    }
}

struct SharedTraceBufferWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedTraceBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.bytes.lock() {
            Ok(mut guard) => guard.extend_from_slice(buf),
            Err(poisoned) => poisoned.into_inner().extend_from_slice(buf),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
