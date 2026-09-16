use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use crate::{ActionBackend, RecordedInput, RecordingBackend, ResolvedBackend, TokenBucket};
use synapse_core::{Action, Backend, GamepadReport, Key, KeyCode, PadButton, error_codes};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::writer::MakeWriter;

use super::keyboard::key_log_label;
use super::rate_limits::BackendRateLimits;
use super::*;

mod auto_release;
mod backend_resolution;
mod held_state;
mod rate_limit;

fn one_token_limits() -> BackendRateLimits {
    BackendRateLimits::with_buckets(
        TokenBucket::new(1, 5_000),
        TokenBucket::new(1, 1_000),
        TokenBucket::new(1, 5_000),
    )
}

fn empty_limits() -> BackendRateLimits {
    BackendRateLimits::with_buckets(
        TokenBucket::new(0, 0),
        TokenBucket::new(0, 0),
        TokenBucket::new(0, 0),
    )
}

fn generous_limits() -> BackendRateLimits {
    BackendRateLimits::with_buckets(
        TokenBucket::new(10, 5_000),
        TokenBucket::new(10, 1_000),
        TokenBucket::new(10, 5_000),
    )
}

fn read_pending_auto_release(emitter: &mut ActionEmitter) -> HeldKeyAutoRelease {
    match emitter.auto_release_rx.try_recv() {
        Ok(auto_release) => auto_release,
        Err(error) => panic!("expected fired auto-release timer message, got {error:?}"),
    }
}

fn assert_no_pending_auto_release(emitter: &mut ActionEmitter) {
    match emitter.auto_release_rx.try_recv() {
        Err(mpsc::error::TryRecvError::Empty) => {}
        other => panic!("expected no auto-release timer message, got {other:?}"),
    }
}

fn current_timer_id(emitter: &ActionEmitter, key: &Key) -> u64 {
    current_timer_id_for_backend(emitter, key, ResolvedBackend::Software)
}

fn current_timer_id_for_backend(
    emitter: &ActionEmitter,
    key: &Key,
    backend: ResolvedBackend,
) -> u64 {
    let timer_key = HeldKeyTimerKey::new(key.clone(), backend);
    emitter.held_key_timer_ids.get(&timer_key).map_or_else(
        || panic!("expected held key timer id for {timer_key:?}"),
        |timer_id| *timer_id,
    )
}

fn assert_auto_key_up(action: Option<&Action>, expected_key: &Key) {
    assert_auto_key_up_for_backend(action, expected_key, Backend::Software);
}

fn assert_auto_key_up_for_backend(action: Option<&Action>, expected_key: &Key, expected: Backend) {
    match action {
        Some(Action::KeyUp { key, backend }) => {
            assert_eq!(key, expected_key);
            assert_eq!(*backend, expected);
        }
        other => panic!("expected emitted auto KeyUp for {expected_key:?}, got {other:?}"),
    }
}

fn held_key_labels(snapshot: &ActionStateSnapshot) -> Vec<String> {
    snapshot.held_keys.iter().map(key_log_label).collect()
}

fn find_log_line(log_output: &str, needle: &str) -> String {
    log_output
        .lines()
        .find(|line| line.contains(needle))
        .map_or_else(
            || panic!("expected log output to contain {needle}, got {log_output:?}"),
            ToOwned::to_owned,
        )
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

async fn snapshot_until_empty(
    snapshot_handle: &ActionEmitterSnapshotHandle,
) -> ActionStateSnapshot {
    let mut last_snapshot = snapshot_or_panic(snapshot_handle).await;
    for _attempt in 0..8 {
        if last_snapshot.held_keys.is_empty() && last_snapshot.held_key_timer_count == 0 {
            return last_snapshot;
        }
        tokio::task::yield_now().await;
        last_snapshot = snapshot_or_panic(snapshot_handle).await;
    }
    panic!("expected actor auto-release to drain held key state, last={last_snapshot:?}");
}

async fn snapshot_or_panic(snapshot_handle: &ActionEmitterSnapshotHandle) -> ActionStateSnapshot {
    match snapshot_handle.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("snapshot should succeed: {error:?}"),
    }
}

async fn join_actor_or_panic(join: JoinHandle<ActionStateSnapshot>) -> ActionStateSnapshot {
    match join.await {
        Ok(snapshot) => snapshot,
        Err(error) => panic!("actor join should succeed: {error:?}"),
    }
}

fn key_named(name: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: name.to_owned(),
        },
        use_scancode: false,
    }
}

fn gamepad_report(button: PadButton) -> GamepadReport {
    GamepadReport {
        buttons: vec![button],
        thumb_l: (0.0, 0.0),
        thumb_r: (0.0, 0.0),
        lt: 0.0,
        rt: 0.0,
        ..GamepadReport::default()
    }
}
