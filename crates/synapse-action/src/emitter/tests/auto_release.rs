use super::*;
#[tokio::test(start_paused = true)]
async fn key_down_timer_auto_releases_held_key_and_clears_hashmap() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(generous_limits());
    let key = key_named("auto-release-happy");
    let before = emitter.snapshot();

    let key_down_result = emitter
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        key_down_result.is_ok(),
        "KeyDown should consume available token: {key_down_result:?}"
    );
    let after_key_down = emitter.snapshot();

    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS)).await;
    tokio::task::yield_now().await;
    let auto_release = read_pending_auto_release(&mut emitter);
    let emitted_action = emitter.auto_release_held_key(&auto_release);
    let after_auto_release_decision = emitter.snapshot();
    emitter
        .execute(
            emitted_action
                .clone()
                .unwrap_or_else(|| panic!("auto-release should emit KeyUp action")),
        )
        .await
        .unwrap_or_else(|error| panic!("auto-release KeyUp should execute: {error}"));
    let after_auto_release = emitter.snapshot();

    assert!(before.held_keys.is_empty());
    assert_eq!(after_key_down.held_keys, vec![key.clone()]);
    assert_eq!(after_key_down.held_key_timer_keys, vec![key.clone()]);
    assert_eq!(after_key_down.held_key_timer_count, 1);
    assert_auto_key_up(emitted_action.as_ref(), &key);
    assert_eq!(after_auto_release_decision.held_keys, vec![key.clone()]);
    assert!(after_auto_release_decision.held_key_timer_keys.is_empty());
    assert_eq!(after_auto_release_decision.held_key_timer_count, 0);
    assert!(after_auto_release.held_keys.is_empty());
    assert!(after_auto_release.held_key_timer_keys.is_empty());
    assert_eq!(after_auto_release.held_key_timer_count, 0);
    println!(
        "readback=held_keys_bitset_and_timer_hashmap edge=happy_auto_release before={before:?} after_key_down={after_key_down:?} after_auto_release_decision={after_auto_release_decision:?} after_auto_release={after_auto_release:?} data.code={}",
        error_codes::STUCK_KEY_AUTO_RELEASED
    );
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn stuck_key_auto_release_tracing_event_and_recording_keyup_are_observable() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();

    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(generous_limits());
    let recording_backend = RecordingBackend::new();
    let mut recording_state = EmitState::new();
    let key = key_named("a");
    let key_down = Action::KeyDown {
        key: key.clone(),
        backend: Backend::Software,
    };
    recording_backend
        .execute(&key_down, &mut recording_state)
        .unwrap_or_else(|error| panic!("recording keydown should succeed: {error}"));
    let before_empty = emitter.snapshot();

    let key_down_result = emitter.execute(key_down).await;
    assert!(
        key_down_result.is_ok(),
        "KeyDown should set held state for trace test: {key_down_result:?}"
    );
    let before_auto_release = emitter.snapshot();

    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS)).await;
    tokio::task::yield_now().await;
    let auto_release = read_pending_auto_release(&mut emitter);
    let emitted_action = tracing::subscriber::with_default(subscriber, || {
        emitter
            .auto_release_held_key(&auto_release)
            .unwrap_or_else(|| panic!("auto-release should emit KeyUp action"))
    });
    recording_backend
        .execute(&emitted_action, &mut recording_state)
        .unwrap_or_else(|error| panic!("recording auto KeyUp should succeed: {error}"));
    let after_auto_release = emitter.snapshot();

    let log_output = trace_buffer.text();
    let log_line = find_log_line(&log_output, error_codes::STUCK_KEY_AUTO_RELEASED);
    let recording_events = recording_backend.events();
    let expected_events = vec![
        RecordedInput::KeyDown { key: key.clone() },
        RecordedInput::KeyUp { key: key.clone() },
    ];

    assert!(before_empty.held_keys.is_empty());
    assert_eq!(before_auto_release.held_keys, vec![key.clone()]);
    assert_eq!(before_auto_release.held_key_timer_count, 1);
    assert_eq!(after_auto_release.held_keys, vec![key.clone()]);
    assert_eq!(after_auto_release.held_key_timer_count, 0);
    assert_eq!(recording_events, expected_events);
    assert_auto_key_up(Some(&emitted_action), &key);
    assert!(log_line.contains("code=STUCK_KEY_AUTO_RELEASED"));
    assert!(log_line.contains("held_ms=30000"));
    assert!(log_line.contains("key=a"));
    println!(
        "readback=stuck_key edge=auto_release before=held:{:?} after_decision_held:{:?} log_line={} recording_events={recording_events:?}",
        held_key_labels(&before_auto_release),
        held_key_labels(&after_auto_release),
        log_line
    );
}

#[tokio::test(start_paused = true)]
async fn actor_loop_processes_auto_release_timer_message() {
    let recording = Arc::new(RecordingBackend::new());
    let backend: Arc<dyn ActionBackend> = recording.clone();
    let (handle, snapshot_handle, emitter) = ActionEmitter::channel_with_backend(backend);
    let cancel = CancellationToken::new();
    let join = tokio::spawn(emitter.run(cancel.clone()));
    let key = key_named("actor-auto-release");
    let before = snapshot_or_panic(&snapshot_handle).await;

    let key_down_result = handle
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        key_down_result.is_ok(),
        "actor KeyDown should be accepted: {key_down_result:?}"
    );
    let after_key_down = snapshot_or_panic(&snapshot_handle).await;
    let after_key_down_events = recording.events();

    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS)).await;
    tokio::task::yield_now().await;
    let after_auto_release = snapshot_until_empty(&snapshot_handle).await;
    let after_auto_release_events = recording.events();

    cancel.cancel();
    let after_cancel = join_actor_or_panic(join).await;

    assert!(before.held_keys.is_empty());
    assert_eq!(after_key_down.held_keys, vec![key.clone()]);
    assert_eq!(after_key_down.held_key_timer_count, 1);
    assert!(after_auto_release.held_keys.is_empty());
    assert_eq!(after_auto_release.held_key_timer_count, 0);
    assert_eq!(
        after_key_down_events,
        vec![RecordedInput::KeyDown { key: key.clone() }],
        "readback=RecordingBackend::events after KeyDown"
    );
    assert_eq!(
        after_auto_release_events,
        vec![
            RecordedInput::KeyDown { key: key.clone() },
            RecordedInput::KeyUp { key: key.clone() },
        ],
        "readback=RecordingBackend::events after auto release"
    );
    assert!(after_cancel.held_keys.is_empty());
    assert_eq!(after_cancel.held_key_timer_count, 0);
    println!(
        "readback=actor_snapshot_and_recording_backend edge=actor_loop_auto_release before={before:?} after_key_down={after_key_down:?} after_key_down_events={after_key_down_events:?} after_auto_release={after_auto_release:?} after_auto_release_events={after_auto_release_events:?} after_cancel={after_cancel:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn key_up_cancels_timer_before_releasing_even_when_buckets_are_empty() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(empty_limits());
    let key = key_named("manual-key-up-cancel");
    emitter.state.hold_key(&key);
    emitter.schedule_held_key_auto_release(key.clone(), ResolvedBackend::Software);
    let before = emitter.snapshot();
    let before_limits = emitter.rate_limits.snapshot();

    let key_up_result = emitter
        .execute(Action::KeyUp {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        key_up_result.is_ok(),
        "KeyUp must bypass rate limits to cancel the safety timer: {key_up_result:?}"
    );
    let after = emitter.snapshot();
    let after_limits = emitter.rate_limits.snapshot();

    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS + 1)).await;
    tokio::task::yield_now().await;
    assert_no_pending_auto_release(&mut emitter);

    assert_eq!(before.held_keys, vec![key]);
    assert_eq!(before.held_key_timer_count, 1);
    assert!(after.held_keys.is_empty());
    assert_eq!(after.held_key_timer_count, 0);
    assert_eq!(before_limits.software.tokens, 0);
    assert_eq!(after_limits.software.tokens, 0);
    println!(
        "readback=held_keys_bitset_and_timer_hashmap edge=keyup_cancel_empty_bucket before={before:?} before_limits={before_limits:?} after={after:?} after_limits={after_limits:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn repeated_key_down_replaces_timer_without_old_timer_release() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(generous_limits());
    let key = key_named("repeat-reset");
    let before = emitter.snapshot();

    let first_result = emitter
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        first_result.is_ok(),
        "first KeyDown should be accepted: {first_result:?}"
    );
    let after_first = emitter.snapshot();
    let first_timer_id = current_timer_id(&emitter, &key);

    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS - 1_000)).await;
    let second_result = emitter
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        second_result.is_ok(),
        "second KeyDown should reset the timer: {second_result:?}"
    );
    let after_second = emitter.snapshot();
    let second_timer_id = current_timer_id(&emitter, &key);

    time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    let after_old_deadline = emitter.snapshot();
    assert_no_pending_auto_release(&mut emitter);

    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS - 2_000)).await;
    tokio::task::yield_now().await;
    let auto_release = read_pending_auto_release(&mut emitter);
    let emitted_action = emitter.auto_release_held_key(&auto_release);
    let after_new_deadline_decision = emitter.snapshot();
    emitter
        .execute(
            emitted_action
                .clone()
                .unwrap_or_else(|| panic!("auto-release should emit KeyUp action")),
        )
        .await
        .unwrap_or_else(|error| panic!("auto-release KeyUp should execute: {error}"));
    let after_new_deadline = emitter.snapshot();

    assert_ne!(first_timer_id, second_timer_id);
    assert_eq!(after_first.held_key_timer_count, 1);
    assert_eq!(after_second.held_key_timer_count, 1);
    assert_eq!(after_old_deadline.held_keys, vec![key.clone()]);
    assert_eq!(after_old_deadline.held_key_timer_count, 1);
    assert_auto_key_up(emitted_action.as_ref(), &key);
    assert_eq!(after_new_deadline_decision.held_keys, vec![key.clone()]);
    assert_eq!(after_new_deadline_decision.held_key_timer_count, 0);
    assert!(after_new_deadline.held_keys.is_empty());
    assert_eq!(after_new_deadline.held_key_timer_count, 0);
    println!(
        "readback=held_keys_bitset_and_timer_hashmap edge=repeated_keydown_reset before={before:?} after_first={after_first:?} first_timer_id={first_timer_id} after_second={after_second:?} second_timer_id={second_timer_id} after_old_deadline={after_old_deadline:?} after_new_deadline_decision={after_new_deadline_decision:?} after_new_deadline={after_new_deadline:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn release_all_aborts_held_key_timer_hashmap() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(generous_limits());
    let key = key_named("release-all-abort");
    let key_down_result = emitter
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        key_down_result.is_ok(),
        "KeyDown should set up the timer: {key_down_result:?}"
    );
    let before_release_all = emitter.snapshot();

    let release_result = emitter.execute(Action::ReleaseAll).await;
    assert!(
        release_result.is_ok(),
        "ReleaseAll must abort timers without rate limiting: {release_result:?}"
    );
    let after_release_all = emitter.snapshot();

    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS + 1)).await;
    tokio::task::yield_now().await;
    assert_no_pending_auto_release(&mut emitter);

    assert_eq!(before_release_all.held_keys, vec![key]);
    assert_eq!(before_release_all.held_key_timer_count, 1);
    assert!(after_release_all.held_keys.is_empty());
    assert_eq!(after_release_all.held_key_timer_count, 0);
    println!(
        "readback=held_keys_bitset_and_timer_hashmap edge=release_all_abort before={before_release_all:?} after={after_release_all:?}"
    );
}
