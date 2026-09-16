use std::sync::Arc;

use synapse_action::{ActionBackend, ActionEmitter, ActionError, ActionHandle, RecordingBackend};
use synapse_core::{Action, Backend, GamepadController, GamepadReport, Key, KeyCode, PadButton};
use tokio::sync::Mutex;
use tokio::sync::mpsc::error::TryRecvError;
use tokio_util::sync::CancellationToken;

// The input lease (`synapse_action::lease`) is process-global. The two tests
// below acquire it directly and then read it back, so if they run concurrently
// one observes the other's holder — the #883 lease race that made
// `failed_session_input_cleanup_keeps_lease_for_retry` flaky. Serialize every
// lease-touching test on a module-local async mutex held for the whole test,
// resetting the lease on entry so neither observes a stale holder. This mirrors
// the `static SERIAL` guard the `lease` module's own unit tests already use; an
// async (tokio) mutex is required here because these are `#[tokio::test]` bodies
// that hold the guard across `.await` points (a `std::sync::Mutex` guard there
// would trip the denied `clippy::await_holding_lock` lint).
static LEASE_SERIAL: Mutex<()> = Mutex::const_new(());

async fn lease_serial() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = LEASE_SERIAL.lock().await;
    let _prior = synapse_action::lease::force_clear("session_input_lease_test_reset");
    guard
}

#[tokio::test]
async fn session_release_keeps_other_session_inputs_held() {
    let cancel = CancellationToken::new();
    let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), backend);
    let session_a = handle.with_session_id(Some("session-a".to_owned()));
    let session_b = handle.with_session_id(Some("session-b".to_owned()));

    session_a
        .execute(Action::KeyDown {
            key: key("ctrl"),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("session A keydown should succeed: {error}"));
    session_b
        .execute(Action::KeyDown {
            key: key("shift"),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("session B keydown should succeed: {error}"));
    session_b
        .execute(Action::PadReport {
            pad: 2,
            report: held_pad_report(),
        })
        .await
        .unwrap_or_else(|error| panic!("session B pad report should succeed: {error}"));

    let before = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot before session release should succeed: {error}"));
    let before_ownership = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership before session release should read: {error}"));
    println!(
        "readback=session_inputs edge=distinct_sessions before_state={before:?} before_ownership={before_ownership:?}"
    );
    assert_eq!(before.held_keys.len(), 2);
    assert_eq!(before.pad_state.len(), 1);

    let summary_a = handle
        .release_session_inputs("session-a")
        .await
        .unwrap_or_else(|error| panic!("session A release should succeed: {error}"));
    let after_a = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after session A release should succeed: {error}"));
    let after_a_ownership = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership after session A release should read: {error}"));
    println!(
        "readback=session_inputs edge=distinct_sessions after_a_state={after_a:?} after_a_ownership={after_a_ownership:?} summary={summary_a:?}"
    );
    assert_eq!(summary_a.released_keys, 1);
    assert_eq!(summary_a.neutralized_pads, 0);
    assert_eq!(after_a.held_keys, vec![key("shift")]);
    assert_eq!(after_a.pad_state.len(), 1);

    let summary_b = handle
        .release_session_inputs("session-b")
        .await
        .unwrap_or_else(|error| panic!("session B release should succeed: {error}"));
    let after_b = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after session B release should succeed: {error}"));
    println!(
        "readback=session_inputs edge=distinct_sessions after_b_state={after_b:?} summary={summary_b:?}"
    );
    assert_eq!(summary_b.released_keys, 1);
    assert_eq!(summary_b.neutralized_pads, 1);
    assert!(after_b.held_keys.is_empty());
    assert!(after_b.pad_state.is_empty());

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
    assert!(final_snapshot.held_keys.is_empty());
    assert!(final_snapshot.pad_state.is_empty());
}

#[tokio::test]
async fn session_release_retains_shared_input_until_last_owner() {
    let cancel = CancellationToken::new();
    let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), backend);
    let session_a = handle.with_session_id(Some("session-a".to_owned()));
    let session_b = handle.with_session_id(Some("session-b".to_owned()));

    for session in [&session_a, &session_b] {
        session
            .execute(Action::KeyDown {
                key: key("alt"),
                backend: Backend::Software,
            })
            .await
            .unwrap_or_else(|error| panic!("shared keydown should succeed: {error}"));
    }
    let before = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot before shared release should succeed: {error}"));
    println!("readback=session_inputs edge=shared_key before_state={before:?}");
    assert_eq!(before.held_keys, vec![key("alt")]);

    let summary_a = handle
        .release_session_inputs("session-a")
        .await
        .unwrap_or_else(|error| panic!("session A shared release should succeed: {error}"));
    let after_a = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after shared release should succeed: {error}"));
    println!(
        "readback=session_inputs edge=shared_key after_a_state={after_a:?} summary={summary_a:?}"
    );
    assert_eq!(summary_a.released_keys, 0);
    assert_eq!(summary_a.retained_shared_inputs, 1);
    assert_eq!(after_a.held_keys, vec![key("alt")]);

    let summary_b = handle
        .release_session_inputs("session-b")
        .await
        .unwrap_or_else(|error| panic!("session B shared release should succeed: {error}"));
    let after_b = snapshot_handle.snapshot().await.unwrap_or_else(|error| {
        panic!("snapshot after final shared release should succeed: {error}")
    });
    println!(
        "readback=session_inputs edge=shared_key after_b_state={after_b:?} summary={summary_b:?}"
    );
    assert_eq!(summary_b.released_keys, 1);
    assert!(after_b.held_keys.is_empty());

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
    assert!(final_snapshot.held_keys.is_empty());
}

#[tokio::test]
async fn unknown_session_release_does_not_emit_global_release() {
    let cancel = CancellationToken::new();
    let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), backend);
    let session_a = handle.with_session_id(Some("session-a".to_owned()));

    session_a
        .execute(Action::KeyDown {
            key: key("ctrl"),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("session A keydown should succeed: {error}"));
    let before = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot before unknown release should succeed: {error}"));

    let summary = handle
        .release_session_inputs("missing-session")
        .await
        .unwrap_or_else(|error| panic!("unknown session release should succeed as noop: {error}"));
    let after = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after unknown release should succeed: {error}"));
    println!(
        "readback=session_inputs edge=unknown_session before_state={before:?} after_state={after:?} summary={summary:?}"
    );
    assert_eq!(summary.released_keys, 0);
    assert_eq!(summary.released_buttons, 0);
    assert_eq!(summary.neutralized_pads, 0);
    assert_eq!(before, after);

    handle
        .execute(Action::ReleaseAll)
        .await
        .unwrap_or_else(|error| panic!("release_all cleanup should succeed: {error}"));
    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
    assert!(final_snapshot.held_keys.is_empty());
}

#[tokio::test]
async fn failed_session_release_keeps_unreleased_input_owned() {
    let (handle, mut action_rx) = ActionHandle::channel();
    let session_a = handle.with_session_id(Some("session-a".to_owned()));
    let ctrl_down = Action::KeyDown {
        key: key("ctrl"),
        backend: Backend::Software,
    };
    let shift_down = Action::KeyDown {
        key: key("shift"),
        backend: Backend::Software,
    };

    execute_with_ack(session_a.clone(), &mut action_rx, ctrl_down).await;
    execute_with_ack(session_a, &mut action_rx, shift_down).await;
    let before = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership before failed release should read: {error}"));

    let release_handle = handle.clone();
    let release_task =
        tokio::spawn(async move { release_handle.release_session_inputs("session-a").await });
    ack_next_action(
        &mut action_rx,
        Action::KeyUp {
            key: key("ctrl"),
            backend: Backend::Software,
        },
        Ok(()),
    )
    .await;
    ack_next_action(
        &mut action_rx,
        Action::KeyUp {
            key: key("shift"),
            backend: Backend::Software,
        },
        Err(ActionError::BackendUnavailable {
            detail: "forced key-up failure".to_owned(),
        }),
    )
    .await;

    let error = release_task
        .await
        .unwrap_or_else(|error| panic!("release task should join: {error}"))
        .expect_err("forced key-up failure should fail session release");
    let after = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership after failed release should read: {error}"));
    println!(
        "readback=session_inputs edge=failed_release before_ownership={before:?} after_ownership={after:?} error={error:?}"
    );

    assert_eq!(
        error.code(),
        synapse_core::error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    let session = after
        .sessions
        .iter()
        .find(|session| session.session_id == "session-a")
        .unwrap_or_else(|| panic!("failed release should leave session ownership for retry"));
    assert_eq!(session.keys.len(), 1);
    assert_eq!(session.keys[0].key, key("shift"));
}

#[tokio::test]
async fn session_release_serializes_new_owner_until_release_action_ack() {
    let (handle, mut action_rx) = ActionHandle::channel();
    let ctrl_down = Action::KeyDown {
        key: key("ctrl"),
        backend: Backend::Software,
    };
    let session_a = handle.with_session_id(Some("session-a".to_owned()));
    execute_with_ack(session_a, &mut action_rx, ctrl_down.clone()).await;

    let release_handle = handle.clone();
    let release_task =
        tokio::spawn(async move { release_handle.release_session_inputs("session-a").await });
    let (release_action, release_ack, _operator_panic_epoch_at_enqueue) = action_rx
        .recv()
        .await
        .unwrap_or_else(|| panic!("expected queued session-a release action"));
    assert_eq!(
        release_action,
        Action::KeyUp {
            key: key("ctrl"),
            backend: Backend::Software,
        }
    );

    let session_b = handle.with_session_id(Some("session-b".to_owned()));
    let session_b_task = tokio::spawn(async move { session_b.execute(ctrl_down).await });
    tokio::task::yield_now().await;
    assert!(
        matches!(action_rx.try_recv(), Err(TryRecvError::Empty)),
        "session-b must not enqueue a new down until session-a cleanup ack is confirmed"
    );

    release_ack
        .send(Ok(()))
        .unwrap_or_else(|_result| panic!("release acknowledgement receiver should be open"));
    let summary = release_task
        .await
        .unwrap_or_else(|error| panic!("release task should join: {error}"))
        .unwrap_or_else(|error| panic!("session release should succeed: {error}"));
    assert_eq!(summary.released_keys, 1);

    ack_next_action(
        &mut action_rx,
        Action::KeyDown {
            key: key("ctrl"),
            backend: Backend::Software,
        },
        Ok(()),
    )
    .await;
    session_b_task
        .await
        .unwrap_or_else(|error| panic!("session-b task should join: {error}"))
        .unwrap_or_else(|error| panic!("session-b keydown should succeed: {error}"));

    let after = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership after serialized release should read: {error}"));
    println!("readback=session_inputs edge=release_enqueue_race after_ownership={after:?}");
    let session = after
        .sessions
        .iter()
        .find(|session| session.session_id == "session-b")
        .unwrap_or_else(|| panic!("session-b ownership should be recorded after cleanup"));
    assert_eq!(session.keys.len(), 1);
    assert_eq!(session.keys[0].key, key("ctrl"));
}

#[tokio::test]
async fn session_input_lease_cleanup_releases_lease_after_ledger_empty() {
    let _serial = lease_serial().await;
    let cancel = CancellationToken::new();
    let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), backend);
    let session_id = "session-lease-cleanup";
    let session = handle.with_session_id(Some(session_id.to_owned()));
    let _prior = synapse_action::lease::force_clear("session_input_lease_cleanup_test_reset");
    let _held =
        synapse_action::lease::try_acquire(session_id, synapse_action::lease::ttl_from_ms(30_000));

    session
        .execute(Action::KeyDown {
            key: key("ctrl"),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("session keydown should succeed: {error}"));
    let before_state = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot before cleanup should succeed: {error}"));
    let before_lease = synapse_action::lease::status();
    println!(
        "readback=session_input_lease_cleanup edge=happy before_state={before_state:?} before_lease={before_lease:?}"
    );

    let summary = handle
        .release_session_inputs_and_lease(session_id)
        .await
        .unwrap_or_else(|error| panic!("session input+lease cleanup should succeed: {error}"));
    let after_state = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after cleanup should succeed: {error}"));
    let after_ownership = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership after cleanup should read: {error}"));
    let after_lease = synapse_action::lease::status();
    println!(
        "readback=session_input_lease_cleanup edge=happy after_state={after_state:?} after_ownership={after_ownership:?} after_lease={after_lease:?} summary={summary:?}"
    );
    assert_eq!(summary.input_summary.released_keys, 1);
    assert!(summary.lease_released);
    assert!(after_state.held_keys.is_empty());
    assert!(after_ownership.sessions.is_empty());
    assert!(!after_lease.held);

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter task should join: {error}"));
    assert!(final_snapshot.held_keys.is_empty());
    let _prior = synapse_action::lease::force_clear("session_input_lease_cleanup_test_reset");
}

#[tokio::test]
async fn failed_session_input_cleanup_keeps_lease_for_retry() {
    let _serial = lease_serial().await;
    let (handle, mut action_rx) = ActionHandle::channel();
    let session_id = "session-lease-retry";
    let session = handle.with_session_id(Some(session_id.to_owned()));
    let _prior = synapse_action::lease::force_clear("session_input_lease_cleanup_test_reset");
    let _held =
        synapse_action::lease::try_acquire(session_id, synapse_action::lease::ttl_from_ms(30_000));

    execute_with_ack(
        session,
        &mut action_rx,
        Action::KeyDown {
            key: key("ctrl"),
            backend: Backend::Software,
        },
    )
    .await;

    let release_handle = handle.clone();
    let release_task = tokio::spawn(async move {
        release_handle
            .release_session_inputs_and_lease(session_id)
            .await
    });
    ack_next_action(
        &mut action_rx,
        Action::KeyUp {
            key: key("ctrl"),
            backend: Backend::Software,
        },
        Err(ActionError::BackendUnavailable {
            detail: "forced key-up failure".to_owned(),
        }),
    )
    .await;

    let error = release_task
        .await
        .unwrap_or_else(|error| panic!("release task should join: {error}"))
        .expect_err("forced key-up failure should fail session input+lease cleanup");
    let after_ownership = handle
        .session_inputs_snapshot()
        .unwrap_or_else(|error| panic!("ownership after failed cleanup should read: {error}"));
    let after_lease = synapse_action::lease::status();
    println!(
        "readback=session_input_lease_cleanup edge=failed_release after_ownership={after_ownership:?} after_lease={after_lease:?} error={error:?}"
    );
    assert_eq!(
        error.code(),
        synapse_core::error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    assert!(
        after_ownership
            .sessions
            .iter()
            .any(|session| session.session_id == session_id)
    );
    assert_eq!(after_lease.owner_session_id.as_deref(), Some(session_id));
    let _prior = synapse_action::lease::force_clear("session_input_lease_cleanup_test_reset");
}

fn key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

fn held_pad_report() -> GamepadReport {
    GamepadReport {
        controller: GamepadController::X360,
        buttons: vec![PadButton::A],
        thumb_l: (0.0, 0.0),
        thumb_r: (0.0, 0.0),
        lt: 0.0,
        rt: 0.0,
    }
}

async fn execute_with_ack(
    handle: ActionHandle,
    action_rx: &mut tokio::sync::mpsc::Receiver<synapse_action::ActionMessage>,
    action: Action,
) {
    let expected = action.clone();
    let task = tokio::spawn(async move { handle.execute(action).await });
    ack_next_action(action_rx, expected, Ok(())).await;
    task.await
        .unwrap_or_else(|error| panic!("action task should join: {error}"))
        .unwrap_or_else(|error| panic!("action should succeed: {error}"));
}

async fn ack_next_action(
    action_rx: &mut tokio::sync::mpsc::Receiver<synapse_action::ActionMessage>,
    expected: Action,
    result: Result<(), ActionError>,
) {
    let (actual, ack, _operator_panic_epoch_at_enqueue) = action_rx
        .recv()
        .await
        .unwrap_or_else(|| panic!("expected queued action {expected:?}"));
    assert_eq!(actual, expected);
    ack.send(result)
        .unwrap_or_else(|_result| panic!("action acknowledgement receiver should be open"));
}
