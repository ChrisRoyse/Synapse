use synapse_core::{Action, Backend, Key, KeyCode};
use tokio_util::sync::CancellationToken;

use super::{
    ForegroundRestoreCurrentDecision, M2State, OperatorPanicActionBoundary, RECORDING_BACKEND_ENV,
    arm_operator_panic_action_admission, foreground_restore_current_decision,
    recording_backend_enabled,
};

#[test]
fn operator_panic_pending_closes_raw_action_admission_until_exact_finalization() {
    let _serial = crate::test_support::lease_serial("m2_operator_panic_pending_admission");
    synapse_action::isolate_interrupt_epochs_for_test();
    synapse_action::lease::isolate_for_test();
    let mut token = synapse_action::request_operator_panic_interrupt();
    let generation = token.generation();
    let _prior = synapse_action::force_preempt_input_lease_for_operator_panic(
        "m2_action_admission_test",
        generation,
    );

    let error = arm_operator_panic_action_admission("act_type", "test_entry")
        .expect_err("pending physical panic must close raw action admission");
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&serde_json::json!(
            synapse_core::error_codes::SAFETY_OPERATOR_HOTKEY_FIRED
        ))
    );

    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut token
    ));
    let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
        synapse_action::complete_operator_panic_safety_generation(token)
            .unwrap_or_else(|detail| panic!("complete test panic: {detail}"))
    else {
        panic!("isolated test generation must own finalization");
    };
    let _cleared = synapse_action::force_clear_operator_panic_input_lease_generation(
        finalization.generation(),
        "m2_action_admission_test_cleanup",
    );
    assert!(synapse_action::finish_operator_panic_safety_finalization(
        finalization,
        true
    ));
    arm_operator_panic_action_admission("act_type", "test_after_cleanup")
        .unwrap_or_else(|error| panic!("exact finalization must reopen admission: {error:?}"));
}

#[test]
fn operator_panic_boundary_rejects_fully_finalized_intervening_wave() {
    let _serial = crate::test_support::lease_serial("m2_operator_panic_boundary_epoch");
    synapse_action::isolate_interrupt_epochs_for_test();
    synapse_action::lease::isolate_for_test();

    let boundary = OperatorPanicActionBoundary::arm("act_scroll", "test_preflight")
        .expect("initial operator-panic admission should be open");
    let epoch_at_arm = boundary.epoch_at_arm();

    let mut token = synapse_action::request_operator_panic_interrupt();
    let generation = token.generation();
    let _prior = synapse_action::force_preempt_input_lease_for_operator_panic(
        "m2_boundary_epoch_test",
        generation,
    );
    assert!(synapse_action::acknowledge_operator_panic_preemption(
        &mut token
    ));
    let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
        synapse_action::complete_operator_panic_safety_generation(token)
            .unwrap_or_else(|detail| panic!("complete test panic: {detail}"))
    else {
        panic!("isolated test generation must own finalization");
    };
    let _cleared = synapse_action::force_clear_operator_panic_input_lease_generation(
        finalization.generation(),
        "m2_boundary_epoch_test_cleanup",
    );
    assert!(synapse_action::finish_operator_panic_safety_finalization(
        finalization,
        true
    ));

    let readback = synapse_action::operator_panic_safety_readback();
    assert!(!readback.pending);
    assert_ne!(readback.epoch, epoch_at_arm);
    let error = boundary
        .ensure("test_delayed_physical_dispatch")
        .expect_err("a fully finalized intervening panic must supersede delayed dispatch");
    let data = error.data.expect("boundary rejection should be structured");
    assert_eq!(
        data["code"],
        serde_json::json!(synapse_core::error_codes::SAFETY_OPERATOR_HOTKEY_FIRED)
    );
    assert_eq!(data["operator_panic_epoch_at_arm"], epoch_at_arm);
    assert_eq!(data["operator_panic"]["pending"], false);
}

#[test]
fn from_env_reads_recording_backend_env() {
    let before = std::env::var(RECORDING_BACKEND_ENV).ok();
    let expected = recording_backend_enabled(before.as_deref());
    let state = M2State::from_env();
    let event_count = state
        .recording
        .as_ref()
        .map_or(0, |recording| recording.events().len());
    println!(
        "readback=m2_state scenario=from_env before_env={before:?} expected_recording={expected} after_recording_enabled={} emitter_retained={} emitter_available={} recording_event_count={event_count}",
        state.recording_enabled(),
        state.emitter_retained(),
        state.emitter_available()
    );
    assert_eq!(state.recording_enabled(), expected);
    assert!(state.emitter_available());
    assert_eq!(event_count, 0);
}

#[tokio::test]
async fn m2_state_spawns_running_emitter_inside_runtime() {
    let before = Some("true");
    let state = M2State::from_recording_backend_env(before);
    let snapshot = match state.snapshot_handle.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(err) => panic!("snapshot failed: {err}"),
    };
    println!(
        "readback=m2_state scenario=runtime_actor before_env={before:?} after_recording_enabled={} emitter_retained={} emitter_running={} held_keys={:?} held_key_timer_count={} held_buttons={:?} pad_state_len={}",
        state.recording_enabled(),
        state.emitter_retained(),
        state.emitter_running(),
        snapshot.held_keys,
        snapshot.held_key_timer_count,
        snapshot.held_buttons,
        snapshot.pad_state.len()
    );
    assert!(state.recording_enabled());
    assert!(!state.emitter_retained());
    assert!(state.emitter_running());
    assert!(snapshot.held_keys.is_empty());
    assert_eq!(snapshot.held_key_timer_count, 0);
    assert!(snapshot.held_buttons.is_empty());
    assert!(snapshot.pad_state.is_empty());
}

#[tokio::test]
async fn m2_state_reports_transferred_emitter_owner_until_done_readback() {
    let before = Some("false");
    let cancel = CancellationToken::new();
    let substitute: std::sync::Arc<dyn synapse_action::ActionBackend> =
        std::sync::Arc::new(synapse_action::RecordingBackend::new());
    let mut state = M2State::from_recording_backend_env_with_actor_backend(
        before,
        cancel.clone(),
        "shutdown",
        None,
        Some(substitute),
    );
    let mut done = state
        .emitter_done_receiver()
        .unwrap_or_else(|| panic!("runtime M2 state should expose emitter done receiver"));
    let join = state
        .take_emitter_task()
        .unwrap_or_else(|| panic!("runtime M2 state should transfer emitter join handle"));

    println!(
        "readback=m2_state scenario=transferred_owner before_done={:?} externally_owned={} emitter_running={} emitter_available={}",
        done.borrow().is_some(),
        state.emitter_task_externally_owned,
        state.emitter_running(),
        state.emitter_available()
    );
    assert!(state.emitter_task_externally_owned);
    assert!(state.emitter_running());
    assert!(state.emitter_available());

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter join should complete after cancel: {error}"));
    done.changed()
        .await
        .unwrap_or_else(|error| panic!("emitter done watch should update after cancel: {error}"));
    let done_snapshot = done
        .borrow()
        .clone()
        .unwrap_or_else(|| panic!("emitter done receiver should contain final snapshot"));

    println!(
        "readback=m2_state scenario=transferred_owner after_cancelled={} after_done={:?} emitter_running={} emitter_available={} final_held_keys={:?}",
        cancel.is_cancelled(),
        done.borrow().is_some(),
        state.emitter_running(),
        state.emitter_available(),
        final_snapshot.held_keys
    );
    assert_eq!(done_snapshot, final_snapshot);
    assert!(!state.emitter_running());
    assert!(!state.emitter_available());
}

#[tokio::test]
async fn recording_env_routes_actor_actions_to_recording_backend() {
    let before = Some("true");
    let state = M2State::from_recording_backend_env(before);
    let recording = state
        .recording
        .clone()
        .unwrap_or_else(|| panic!("recording env should install recording backend"));
    let key = key_named("m2-recording-actor");
    let before_events = recording.events();

    state
        .emitter_handle
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("KeyDown should route to recording backend: {error}"));
    let after_key_down = state
        .snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after keydown should succeed: {error}"));
    state
        .emitter_handle
        .execute(Action::ReleaseAll)
        .await
        .unwrap_or_else(|error| panic!("ReleaseAll should route to recording backend: {error}"));
    let after_release = state
        .snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot after release should succeed: {error}"));
    let after_events = recording.events();

    println!(
        "readback=m2_state scenario=recording_actor before_events:{} after_key_down_held_keys:{:?} after_release_held_keys:{:?} after_events:{}",
        before_events.len(),
        after_key_down.held_keys,
        after_release.held_keys,
        after_events.len()
    );
    assert_eq!(before_events.len(), 0);
    assert_eq!(after_key_down.held_keys, vec![key]);
    assert!(after_release.held_keys.is_empty());
    assert_eq!(after_events.len(), 2);
}

#[tokio::test]
async fn m2_state_uses_injected_cancel_token_to_release_all_on_shutdown() {
    let before = Some("false");
    let cancel = CancellationToken::new();
    let substitute: std::sync::Arc<dyn synapse_action::ActionBackend> =
        std::sync::Arc::new(synapse_action::RecordingBackend::new());
    let mut state = M2State::from_recording_backend_env_with_actor_backend(
        before,
        cancel.clone(),
        "shutdown",
        None,
        Some(substitute),
    );
    let key = key_named("m2-cancel-token");
    println!(
        "readback=m2_state scenario=injected_cancel before_cancelled:{} emitter_running:{}",
        cancel.is_cancelled(),
        state.emitter_running()
    );

    state
        .emitter_handle
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("KeyDown should reach emitter before cancel: {error}"));
    let before_cancel = state
        .snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot before cancel should succeed: {error}"));
    assert_eq!(before_cancel.held_keys, vec![key.clone()]);
    let done = state
        .emitter_done_receiver()
        .unwrap_or_else(|| panic!("runtime M2 state should expose emitter done receiver"));

    cancel.cancel();
    let join = state
        .emitter_task
        .take()
        .unwrap_or_else(|| panic!("runtime M2 state should retain emitter join handle"));
    let after_cancel = join
        .await
        .unwrap_or_else(|error| panic!("emitter join should complete after cancel: {error}"));
    let done_snapshot = done
        .borrow()
        .clone()
        .unwrap_or_else(|| panic!("emitter done receiver should contain final snapshot"));

    println!(
        "readback=m2_state scenario=injected_cancel after_cancelled:{} before_held_keys:{:?} after_held_keys:{:?} done_held_keys:{:?} after_timer_count:{} after_buttons:{:?} after_pad_state_len:{}",
        cancel.is_cancelled(),
        before_cancel.held_keys,
        after_cancel.held_keys,
        done_snapshot.held_keys,
        after_cancel.held_key_timer_count,
        after_cancel.held_buttons,
        after_cancel.pad_state.len()
    );
    assert!(cancel.is_cancelled());
    assert!(after_cancel.held_keys.is_empty());
    assert_eq!(done_snapshot, after_cancel);
    assert_eq!(after_cancel.held_key_timer_count, 0);
    assert!(after_cancel.held_buttons.is_empty());
    assert!(after_cancel.pad_state.is_empty());
}

#[test]
fn recording_backend_env_parser_handles_happy_path_and_edges() {
    let cases = [
        ("happy_one", Some("1"), true),
        ("happy_true_uppercase", Some("TRUE"), true),
        ("edge_absent", None, false),
        ("edge_empty", Some(""), false),
        ("edge_false", Some("false"), false),
        ("edge_whitespace", Some(" true "), false),
        ("edge_invalid", Some("record"), false),
    ];
    for (name, before, expected) in cases {
        let state = M2State::from_recording_backend_env(before);
        let event_count = state
            .recording
            .as_ref()
            .map_or(0, |recording| recording.events().len());
        println!(
            "readback=m2_state scenario={name} before_env={before:?} expected_recording={expected} after_recording_enabled={} emitter_retained={} emitter_available={} recording_event_count={event_count}",
            state.recording_enabled(),
            state.emitter_retained(),
            state.emitter_available()
        );
        assert_eq!(state.recording_enabled(), expected);
        assert!(state.emitter_available());
        assert_eq!(event_count, 0);
    }
}

#[test]
fn foreground_restore_current_decision_skips_when_current_foreground_moved() {
    let captured_hwnd = 0x1001;
    let expected_pid = 42;
    let cases = [
        (
            "same_hwnd_and_pid",
            captured_hwnd,
            expected_pid,
            ForegroundRestoreCurrentDecision::AlreadyCurrent,
        ),
        (
            "different_hwnd",
            0x2002,
            expected_pid,
            ForegroundRestoreCurrentDecision::SkipHumanMoved,
        ),
        (
            "same_hwnd_different_pid",
            captured_hwnd,
            99,
            ForegroundRestoreCurrentDecision::SkipHumanMoved,
        ),
    ];

    for (name, current_hwnd, current_pid, expected) in cases {
        let actual = foreground_restore_current_decision(
            captured_hwnd,
            expected_pid,
            current_hwnd,
            current_pid,
        );
        println!(
            "readback=foreground_restore_decision scenario={name} captured_hwnd=0x{captured_hwnd:x} expected_pid={expected_pid} current_hwnd=0x{current_hwnd:x} current_pid={current_pid} expected={expected:?} actual={actual:?}"
        );
        assert_eq!(actual, expected);
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
