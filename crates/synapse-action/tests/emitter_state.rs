use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{Config, TestCaseError, TestRunner},
};
use synapse_action::{
    ACTION_QUEUE_CAPACITY, ActionBackend, ActionEmitter, ActionStateSnapshot, RecordingBackend,
};
use synapse_core::{
    Action, Backend, ButtonAction, GamepadReport, Key, KeyCode, MouseButton, PadButton, Stick,
    Trigger,
};
use tokio_util::sync::CancellationToken;

/// Cross-platform substitute for the production `SoftwareBackend`: it mutates
/// the same `EmitState` the actor exposes via snapshot, so the actor's
/// held-state, release-all, and shutdown behavior can be exercised on any host.
fn substitute_backend() -> Arc<dyn ActionBackend> {
    Arc::new(RecordingBackend::new())
}

#[tokio::test]
async fn emitter_tracks_held_state_and_release_all_drains() -> Result<(), Box<dyn Error>> {
    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), substitute_backend());
    let key = key_named("shift");
    let report = pad_report(vec![PadButton::A], (0.5, -0.5), (0.0, 0.0), 0.25, 0.0);

    let before = snapshot_handle.snapshot().await?;
    println!("readback=action_emitter_state edge=happy before={before:?}");
    assert_empty(&before);

    handle
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await?;
    handle
        .execute(Action::MouseButton {
            button: MouseButton::Left,
            action: ButtonAction::Down,
            hold_ms: 0,
            backend: Backend::Software,
        })
        .await?;
    handle
        .execute(Action::PadReport {
            pad: 1,
            report: report.clone(),
        })
        .await?;

    let held = snapshot_handle.snapshot().await?;
    assert_eq!(held.held_keys, vec![key]);
    assert_eq!(held.held_buttons, vec![MouseButton::Left]);
    assert_eq!(held.pad_state.get(&1), Some(&report));
    println!("readback=action_emitter_state edge=happy held={held:?}");

    handle.execute(Action::ReleaseAll).await?;
    let after = snapshot_handle.snapshot().await?;
    assert_empty(&after);
    println!("readback=action_emitter_state edge=happy after={after:?} result_value=empty");

    cancel.cancel();
    let final_snapshot = join.await?;
    assert_empty(&final_snapshot);
    Ok(())
}

#[tokio::test]
async fn release_all_on_empty_state_stays_empty() -> Result<(), Box<dyn Error>> {
    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), substitute_backend());

    let before = snapshot_handle.snapshot().await?;
    println!("readback=action_emitter_state edge=empty_release before={before:?}");
    assert_empty(&before);

    handle.execute(Action::ReleaseAll).await?;
    let after = snapshot_handle.snapshot().await?;
    assert_empty(&after);
    println!("readback=action_emitter_state edge=empty_release after={after:?} result_value=empty");

    cancel.cancel();
    let final_snapshot = join.await?;
    assert_empty(&final_snapshot);
    Ok(())
}

#[test]
fn emitter_channel_preserves_bounded_queue_capacity() {
    let (handle, _snapshot_handle, emitter) = ActionEmitter::channel();
    println!(
        "readback=action_emitter_queue edge=capacity before=queued:{}",
        emitter.pending_len()
    );
    assert_eq!(emitter.pending_len(), 0);

    for _index in 0..ACTION_QUEUE_CAPACITY {
        handle
            .try_execute(Action::ReleaseAll)
            .unwrap_or_else(|err| {
                panic!("bounded queue should accept capacity-sized burst: {err}")
            });
    }

    assert_eq!(emitter.pending_len(), ACTION_QUEUE_CAPACITY);
    let error = match handle.try_execute(Action::ReleaseAll) {
        Ok(()) => panic!("257th action should return ACTION_QUEUE_FULL"),
        Err(error) => error,
    };
    assert_eq!(error.code(), synapse_core::error_codes::ACTION_QUEUE_FULL);
    println!(
        "readback=action_emitter_queue edge=capacity after=queued:{} result_value={}",
        emitter.pending_len(),
        error.code()
    );
}

#[tokio::test]
async fn release_all_safety_lane_preempts_saturated_normal_queue() -> Result<(), Box<dyn Error>> {
    let cancel = CancellationToken::new();
    let recording = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, emitter) = ActionEmitter::channel_with_backend(recording.clone());
    let queued_key = key_named("shift");

    for _index in 0..ACTION_QUEUE_CAPACITY {
        handle.try_execute(Action::KeyDown {
            key: queued_key.clone(),
            backend: Backend::Software,
        })?;
    }
    assert_eq!(emitter.pending_len(), ACTION_QUEUE_CAPACITY);
    println!(
        "readback=action_emitter_queue edge=release_preempt before=queued:{} events={:?}",
        emitter.pending_len(),
        recording.events()
    );

    let join = tokio::spawn(emitter.run(cancel.clone()));
    let release_result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        handle.execute(Action::ReleaseAll),
    )
    .await?;
    release_result?;
    let after = snapshot_handle.snapshot().await?;
    let events = recording.events();

    assert_empty(&after);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, synapse_action::RecordedInput::ReleaseAll { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, synapse_action::RecordedInput::KeyDown { .. })),
        "queued normal actions must be rejected after release_all preemption: {events:?}"
    );
    println!(
        "readback=action_emitter_queue edge=release_preempt after={after:?} events={events:?}"
    );

    cancel.cancel();
    let final_snapshot = join.await?;
    assert_empty(&final_snapshot);
    Ok(())
}

#[tokio::test]
async fn cancellation_drains_held_state_before_actor_returns() -> Result<(), Box<dyn Error>> {
    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), substitute_backend());
    let key = key_named("control");

    handle
        .execute(Action::KeyDown {
            key,
            backend: Backend::Software,
        })
        .await?;
    let before = snapshot_handle.snapshot().await?;
    assert_eq!(before.held_keys.len(), 1);
    println!("readback=action_emitter_state edge=cancel before={before:?}");

    cancel.cancel();
    let after = join.await?;
    assert_empty(&after);
    println!("readback=action_emitter_state edge=cancel after={after:?} result_value=empty");
    Ok(())
}

#[tokio::test]
async fn unmatched_key_up_does_not_create_held_state() -> Result<(), Box<dyn Error>> {
    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), substitute_backend());

    let before = snapshot_handle.snapshot().await?;
    println!("readback=action_emitter_state edge=unmatched_key_up before={before:?}");
    handle
        .execute(Action::KeyUp {
            key: key_named("never-held"),
            backend: Backend::Software,
        })
        .await?;

    let after = snapshot_handle.snapshot().await?;
    assert_empty(&after);
    println!(
        "readback=action_emitter_state edge=unmatched_key_up after={after:?} result_value=empty"
    );

    cancel.cancel();
    let final_snapshot = join.await?;
    assert_empty(&final_snapshot);
    Ok(())
}

#[test]
fn randomized_streams_ending_release_all_leave_empty_state() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let case_counter = AtomicUsize::new(0);
    let mut runner = TestRunner::new(Config {
        cases: 1000,
        failure_persistence: None,
        ..Config::default()
    });

    runner.run(&vec(tracked_action_strategy(), 0..=32), |actions| {
        let case_index = case_counter.fetch_add(1, Ordering::Relaxed);
        let (after_release, after_shutdown) = runtime
            .block_on(run_stream_and_release(actions))
            .map_err(|err| TestCaseError::fail(err.to_string()))?;
        assert_empty_for_proptest(&after_release)?;
        assert_empty_for_proptest(&after_shutdown)?;
        println!(
            "readback=action_emitter_state edge=proptest_case_{case_index} result_value=held_keys:{:?} held_buttons:{:?} pad_state:{:?}",
            after_release.held_key_bits,
            after_release.held_button_bits,
            after_release.pad_state
        );
        Ok(())
    })?;
    Ok(())
}

async fn run_stream_and_release(
    actions: Vec<Action>,
) -> Result<(ActionStateSnapshot, ActionStateSnapshot), Box<dyn Error>> {
    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), substitute_backend());

    for action in actions {
        handle.execute(action).await?;
    }
    handle.execute(Action::ReleaseAll).await?;
    let after_release = snapshot_handle.snapshot().await?;
    cancel.cancel();
    let after_shutdown = join.await?;
    Ok((after_release, after_shutdown))
}

fn tracked_action_strategy() -> BoxedStrategy<Action> {
    prop_oneof![
        (key_strategy(), backend_strategy())
            .prop_map(|(key, backend)| Action::KeyDown { key, backend }),
        (key_strategy(), backend_strategy())
            .prop_map(|(key, backend)| Action::KeyUp { key, backend }),
        (key_strategy(), 0u32..=100, backend_strategy()).prop_map(|(key, hold_ms, backend)| {
            Action::KeyPress {
                key,
                hold_ms,
                backend,
            }
        },),
        (
            mouse_button_strategy(),
            button_action_strategy(),
            0u32..=100,
            backend_strategy()
        )
            .prop_map(|(button, action, hold_ms, backend)| Action::MouseButton {
                button,
                action,
                hold_ms,
                backend
            }),
        (
            0u8..=2,
            pad_button_strategy(),
            button_action_strategy(),
            0u32..=100
        )
            .prop_map(|(pad, button, action, hold_ms)| Action::PadButton {
                pad,
                button,
                action,
                hold_ms
            }),
        (0u8..=2, stick_strategy(), -1.0f32..=1.0, -1.0f32..=1.0)
            .prop_map(|(pad, stick, x, y)| Action::PadStick { pad, stick, x, y }),
        (0u8..=2, trigger_strategy(), 0.0f32..=1.0).prop_map(|(pad, trigger, value)| {
            Action::PadTrigger {
                pad,
                trigger,
                value,
            }
        }),
        (0u8..=2, gamepad_report_strategy())
            .prop_map(|(pad, report)| Action::PadReport { pad, report }),
    ]
    .boxed()
}

fn key_strategy() -> BoxedStrategy<Key> {
    prop_oneof![
        Just(key_named("a")),
        Just(key_named("shift")),
        Just(Key {
            code: KeyCode::Symbol { value: 'Z' },
            use_scancode: false,
        }),
        Just(Key {
            code: KeyCode::HidCode { value: 4 },
            use_scancode: true,
        }),
    ]
    .boxed()
}

fn backend_strategy() -> BoxedStrategy<Backend> {
    prop_oneof![
        Just(Backend::Software),
        Just(Backend::Vigem),
        Just(Backend::Hardware),
        Just(Backend::Auto),
    ]
    .boxed()
}

fn mouse_button_strategy() -> BoxedStrategy<MouseButton> {
    prop_oneof![
        Just(MouseButton::Left),
        Just(MouseButton::Right),
        Just(MouseButton::Middle),
        Just(MouseButton::X1),
        Just(MouseButton::X2),
    ]
    .boxed()
}

fn button_action_strategy() -> BoxedStrategy<ButtonAction> {
    prop_oneof![
        Just(ButtonAction::Press),
        Just(ButtonAction::Down),
        Just(ButtonAction::Up),
    ]
    .boxed()
}

fn pad_button_strategy() -> BoxedStrategy<PadButton> {
    prop_oneof![
        Just(PadButton::A),
        Just(PadButton::B),
        Just(PadButton::Lb),
        Just(PadButton::Start),
    ]
    .boxed()
}

fn stick_strategy() -> BoxedStrategy<Stick> {
    prop_oneof![Just(Stick::Left), Just(Stick::Right)].boxed()
}

fn trigger_strategy() -> BoxedStrategy<Trigger> {
    prop_oneof![Just(Trigger::Left), Just(Trigger::Right)].boxed()
}

fn gamepad_report_strategy() -> BoxedStrategy<GamepadReport> {
    (
        vec(pad_button_strategy(), 0..=2),
        -1.0f32..=1.0,
        -1.0f32..=1.0,
        -1.0f32..=1.0,
        -1.0f32..=1.0,
        0.0f32..=1.0,
        0.0f32..=1.0,
    )
        .prop_map(|(buttons, lx, ly, rx, ry, lt, rt)| {
            pad_report(buttons, (lx, ly), (rx, ry), lt, rt)
        })
        .boxed()
}

fn key_named(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

const fn pad_report(
    buttons: Vec<PadButton>,
    thumb_l: (f32, f32),
    thumb_r: (f32, f32),
    lt: f32,
    rt: f32,
) -> GamepadReport {
    GamepadReport {
        controller: synapse_core::GamepadController::X360,
        buttons,
        thumb_l,
        thumb_r,
        lt,
        rt,
    }
}

fn assert_empty(snapshot: &ActionStateSnapshot) {
    assert!(snapshot.held_keys.is_empty());
    assert!(snapshot.held_key_bits.is_empty());
    assert!(snapshot.held_buttons.is_empty());
    assert!(snapshot.held_button_bits.is_empty());
    assert!(snapshot.pad_state.is_empty());
}

fn assert_empty_for_proptest(snapshot: &ActionStateSnapshot) -> Result<(), TestCaseError> {
    if snapshot.held_keys.is_empty()
        && snapshot.held_key_bits.is_empty()
        && snapshot.held_buttons.is_empty()
        && snapshot.held_button_bits.is_empty()
        && snapshot.pad_state.is_empty()
    {
        Ok(())
    } else {
        Err(TestCaseError::fail(format!(
            "expected empty held state, got {snapshot:?}"
        )))
    }
}
