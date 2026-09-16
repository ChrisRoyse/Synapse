use super::*;
#[tokio::test(start_paused = true)]
async fn rate_limited_error_carries_code_and_retry_after_ms_without_state_mutation() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(one_token_limits());
    let first_key = key_named("first");
    let second_key = key_named("second");
    let before_state = emitter.snapshot();
    let before_limits = emitter.rate_limits.snapshot();

    let first_result = emitter
        .execute(Action::KeyDown {
            key: first_key.clone(),
            backend: Backend::Software,
        })
        .await;
    assert!(
        first_result.is_ok(),
        "first token should be available: {first_result:?}"
    );
    let after_first_state = emitter.snapshot();
    let after_first_limits = emitter.rate_limits.snapshot();
    let after = emitter
        .execute(Action::KeyDown {
            key: second_key,
            backend: Backend::Software,
        })
        .await;
    let after_limited_state = emitter.snapshot();
    let after_limited_limits = emitter.rate_limits.snapshot();

    let Err(error) = after else {
        panic!("second software action should be rate limited");
    };
    assert_eq!(error.code(), error_codes::ACTION_RATE_LIMITED);
    assert_eq!(error.retry_after_ms(), Some(1));
    assert!(error.detail().contains("retry_after_ms=1"));
    assert_eq!(before_limits.hardware.tokens, 1);
    assert_eq!(after_first_state.held_keys, vec![first_key.clone()]);
    assert_eq!(after_limited_state.held_keys, vec![first_key]);
    assert_eq!(after_first_limits.software.tokens, 0);
    assert_eq!(after_limited_limits.software.tokens, 0);
    println!(
        "readback=action_emitter_rate_limit edge=software_over_cap before_state={before_state:?} before_limits={before_limits:?} after_first_state={after_first_state:?} after_first_limits={after_first_limits:?} after_limited_state={after_limited_state:?} after_limited_limits={after_limited_limits:?} data.code={} data.retry_after_ms={:?} detail={}",
        error.code(),
        error.retry_after_ms(),
        error.detail()
    );
}

#[tokio::test(start_paused = true)]
async fn software_rate_limit_does_not_consume_vigem_bucket() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(one_token_limits());
    let before = emitter.rate_limits.snapshot();

    let software_result = emitter
        .execute(Action::KeyPress {
            key: key_named("software"),
            hold_ms: 0,
            backend: Backend::Software,
        })
        .await;
    assert!(
        software_result.is_ok(),
        "software token should be available: {software_result:?}"
    );
    let after_software = emitter.rate_limits.snapshot();
    let report = gamepad_report(PadButton::A);
    let vigem_result = emitter
        .execute(Action::PadReport {
            pad: 1,
            report: report.clone(),
        })
        .await;
    assert!(
        vigem_result.is_ok(),
        "vigem token should be independent from software: {vigem_result:?}"
    );
    let after_vigem = emitter.rate_limits.snapshot();
    let after_vigem_state = emitter.snapshot();
    let after = emitter
        .execute(Action::PadReport {
            pad: 1,
            report: gamepad_report(PadButton::B),
        })
        .await;
    let after_limited_state = emitter.snapshot();

    let Err(error) = after else {
        panic!("second vigem action should be rate limited");
    };
    assert_eq!(error.code(), error_codes::ACTION_RATE_LIMITED);
    assert_eq!(error.retry_after_ms(), Some(1));
    assert_eq!(after_software.software.tokens, 0);
    assert_eq!(after_software.vigem.tokens, 1);
    assert_eq!(after_software.hardware.tokens, 1);
    assert_eq!(after_vigem.vigem.tokens, 0);
    assert_eq!(after_vigem_state.pad_state.get(&1), Some(&report));
    assert_eq!(after_limited_state.pad_state.get(&1), Some(&report));
    println!(
        "readback=action_emitter_rate_limit edge=backend_separation before={before:?} after_software={after_software:?} after_vigem={after_vigem:?} after_vigem_state={after_vigem_state:?} after_limited_state={after_limited_state:?} data.code={} data.retry_after_ms={:?}",
        error.code(),
        error.retry_after_ms()
    );
}

#[tokio::test(start_paused = true)]
async fn release_all_bypasses_empty_buckets_and_drains_state() {
    let (_handle, _snapshot_handle, mut emitter) =
        ActionEmitter::channel_with_rate_limits(empty_limits());
    let key = key_named("stuck");
    emitter.state.hold_key(&key);
    let before_state = emitter.snapshot();
    let before_limits = emitter.rate_limits.snapshot();

    let release_result = emitter.execute(Action::ReleaseAll).await;
    assert!(
        release_result.is_ok(),
        "ReleaseAll must not be rate limited: {release_result:?}"
    );
    let after_state = emitter.snapshot();
    let after_limits = emitter.rate_limits.snapshot();

    assert_eq!(before_state.held_keys, vec![key]);
    assert!(after_state.held_keys.is_empty());
    assert_eq!(before_limits.software.tokens, 0);
    assert_eq!(after_limits.software.tokens, 0);
    println!(
        "readback=action_emitter_rate_limit edge=release_all_bypass before_state={before_state:?} before_limits={before_limits:?} after_state={after_state:?} after_limits={after_limits:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn release_all_dispatches_configured_hardware_backend_and_logs_backend() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let software = Arc::new(RecordingBackend::new());
    let vigem = Arc::new(RecordingBackend::new());
    let hardware = Arc::new(RecordingBackend::new());
    let software_backend: Arc<dyn ActionBackend> = software.clone();
    let vigem_backend: Arc<dyn ActionBackend> = vigem.clone();
    let hardware_backend: Arc<dyn ActionBackend> = hardware.clone();
    let backends = Backends::from_parts(software_backend, vigem_backend, hardware_backend, true);
    let (_handle, _snapshot_handle, mut emitter) = ActionEmitter::channel_with_backends(backends);
    let w = key_named("w");
    let ctrl = key_named("ctrl");

    emitter
        .execute(Action::KeyDown {
            key: w.clone(),
            backend: Backend::Hardware,
        })
        .await
        .unwrap_or_else(|error| panic!("hardware w keydown should hold state: {error}"));
    emitter
        .execute(Action::KeyDown {
            key: ctrl.clone(),
            backend: Backend::Hardware,
        })
        .await
        .unwrap_or_else(|error| panic!("hardware ctrl keydown should hold state: {error}"));
    let before_state = emitter.snapshot();
    let before_hardware_events = hardware.events();

    let guard = tracing::subscriber::set_default(subscriber);
    let release_result = emitter.execute(Action::ReleaseAll).await;
    drop(guard);

    assert!(
        release_result.is_ok(),
        "release_all should release configured hardware backend: {release_result:?}"
    );
    let after_state = emitter.snapshot();
    let software_events = software.events();
    let hardware_events = hardware.events();
    let log_output = trace_buffer.text();
    let log_line = find_log_line(&log_output, error_codes::SAFETY_RELEASE_ALL_FIRED);
    let expected_release_keys = vec![
        KeyCode::Named {
            value: "ctrl".to_owned(),
        },
        KeyCode::Named {
            value: "w".to_owned(),
        },
    ];

    assert_eq!(before_state.held_keys, vec![w.clone(), ctrl.clone()]);
    assert_eq!(
        before_hardware_events,
        vec![
            RecordedInput::KeyDown { key: w },
            RecordedInput::KeyDown { key: ctrl },
        ]
    );
    assert!(after_state.held_keys.is_empty());
    assert!(after_state.held_buttons.is_empty());
    assert!(after_state.pad_state.is_empty());
    assert!(software_events.iter().any(|event| matches!(
        event,
        RecordedInput::ReleaseAll {
            held_keys,
            held_buttons,
            pads,
        } if held_keys.is_empty() && held_buttons.is_empty() && pads.is_empty()
    )));
    assert!(hardware_events.iter().any(|event| matches!(
        event,
        RecordedInput::ReleaseAll {
            held_keys,
            held_buttons,
            pads,
        } if held_keys == &expected_release_keys && held_buttons.is_empty() && pads.is_empty()
    )));
    assert!(
        log_line.contains("code=\"SAFETY_RELEASE_ALL_FIRED\""),
        "log_line={log_line}"
    );
    assert!(
        log_line.contains("backend=\"hardware\""),
        "log_line={log_line}"
    );
    assert!(
        log_line.contains("primary_backend=\"software\""),
        "log_line={log_line}"
    );
    assert!(
        log_line.contains("hardware_release_ok=true"),
        "log_line={log_line}"
    );
    println!(
        "readback=hardware_release_all edge=emitter before_state={before_state:?} before_hardware_events={before_hardware_events:?} after_state={after_state:?} software_events={software_events:?} hardware_events={hardware_events:?} log_line={log_line}"
    );
}
