use super::*;

fn recording_backends(
    hardware_release_enabled: bool,
) -> (
    Arc<RecordingBackend>,
    Arc<RecordingBackend>,
    Arc<RecordingBackend>,
    Backends,
) {
    let software = Arc::new(RecordingBackend::new());
    let vigem = Arc::new(RecordingBackend::new());
    let hardware = Arc::new(RecordingBackend::new());
    let software_backend: Arc<dyn ActionBackend> = software.clone();
    let vigem_backend: Arc<dyn ActionBackend> = vigem.clone();
    let hardware_backend: Arc<dyn ActionBackend> = hardware.clone();
    let backends = Backends::from_parts(
        software_backend,
        vigem_backend,
        hardware_backend,
        hardware_release_enabled,
    );
    (software, vigem, hardware, backends)
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
async fn release_all_drains_software_and_hardware_keys_on_own_backends() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let (software, _vigem, hardware, backends) = recording_backends(true);
    let (_handle, _snapshot_handle, mut emitter) = ActionEmitter::channel_with_backends(backends);
    let software_key = key_named("software-held");
    let hardware_key = key_named("hardware-held");

    emitter
        .execute(Action::KeyDown {
            key: software_key.clone(),
            backend: Backend::Software,
        })
        .await
        .unwrap_or_else(|error| panic!("software keydown should hold state: {error}"));
    emitter
        .execute(Action::KeyDown {
            key: hardware_key.clone(),
            backend: Backend::Hardware,
        })
        .await
        .unwrap_or_else(|error| panic!("hardware keydown should hold state: {error}"));
    let before_state = emitter.snapshot();
    let before_software_events = software.events();
    let before_hardware_events = hardware.events();

    let guard = tracing::subscriber::set_default(subscriber);
    let release_result = emitter.execute(Action::ReleaseAll).await;
    drop(guard);

    assert!(
        release_result.is_ok(),
        "release_all should drain both backends: {release_result:?}"
    );
    let after_state = emitter.snapshot();
    let software_events = software.events();
    let hardware_events = hardware.events();
    let log_output = trace_buffer.text();
    let log_line = find_log_line(&log_output, error_codes::SAFETY_RELEASE_ALL_FIRED);
    let expected_software = vec![software_key.code.clone()];
    let expected_hardware = vec![hardware_key.code.clone()];

    assert_eq!(
        before_state
            .held_keys_by_backend
            .get(&ResolvedBackend::Software),
        Some(&vec![software_key.clone()])
    );
    assert_eq!(
        before_state
            .held_keys_by_backend
            .get(&ResolvedBackend::Hardware),
        Some(&vec![hardware_key.clone()])
    );
    assert!(after_state.held_keys.is_empty());
    assert!(after_state.held_keys_by_backend.is_empty());
    assert!(
        software_events.iter().any(|event| matches!(
            event,
            RecordedInput::ReleaseAll {
                held_keys,
                held_buttons,
                pads,
            } if held_keys == &expected_software && held_buttons.is_empty() && pads.is_empty()
        )),
        "software_events={software_events:?}"
    );
    assert!(
        hardware_events.iter().any(|event| matches!(
            event,
            RecordedInput::ReleaseAll {
                held_keys,
                held_buttons,
                pads,
            } if held_keys == &expected_hardware && held_buttons.is_empty() && pads.is_empty()
        )),
        "hardware_events={hardware_events:?}"
    );
    assert!(
        !software_events.iter().any(|event| matches!(
            event,
            RecordedInput::ReleaseAll { held_keys, .. } if held_keys.contains(&hardware_key.code)
        )),
        "software release_all must not claim the hardware-held key: {software_events:?}"
    );
    assert!(
        !hardware_events.iter().any(|event| matches!(
            event,
            RecordedInput::ReleaseAll { held_keys, .. } if held_keys.contains(&software_key.code)
        )),
        "hardware release_all must not claim the software-held key: {hardware_events:?}"
    );
    assert!(
        log_line.contains("backend=\"hardware\""),
        "log_line={log_line}"
    );
    assert!(
        log_line.contains("release_backends=[\"software\", \"hardware\"]"),
        "log_line={log_line}"
    );
    println!(
        "readback=held_state_by_backend edge=release_all_mixed before_state={before_state:?} before_software_events={before_software_events:?} before_hardware_events={before_hardware_events:?} after_state={after_state:?} software_events={software_events:?} hardware_events={hardware_events:?} log_line={log_line}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn release_all_drains_vigem_key_without_pad_state() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let (_software, vigem, _hardware, backends) = recording_backends(false);
    let (_handle, _snapshot_handle, mut emitter) = ActionEmitter::channel_with_backends(backends);
    let vigem_key = key_named("vigem-held");

    emitter
        .execute(Action::KeyDown {
            key: vigem_key.clone(),
            backend: Backend::Vigem,
        })
        .await
        .unwrap_or_else(|error| panic!("vigem keydown should hold state: {error}"));
    let before_state = emitter.snapshot();
    let before_vigem_events = vigem.events();

    let guard = tracing::subscriber::set_default(subscriber);
    let release_result = emitter.execute(Action::ReleaseAll).await;
    drop(guard);

    assert!(
        release_result.is_ok(),
        "release_all should drain vigem-held keys without pad state: {release_result:?}"
    );
    let after_state = emitter.snapshot();
    let vigem_events = vigem.events();
    let log_output = trace_buffer.text();
    let log_line = find_log_line(&log_output, error_codes::SAFETY_RELEASE_ALL_FIRED);
    let expected_vigem = vec![vigem_key.code.clone()];

    assert_eq!(
        before_state
            .held_keys_by_backend
            .get(&ResolvedBackend::Vigem),
        Some(&vec![vigem_key.clone()])
    );
    assert!(before_state.pad_state.is_empty());
    assert!(after_state.held_keys.is_empty());
    assert!(after_state.held_keys_by_backend.is_empty());
    assert!(after_state.pad_state.is_empty());
    assert!(
        vigem_events.iter().any(|event| matches!(
            event,
            RecordedInput::ReleaseAll {
                held_keys,
                held_buttons,
                pads,
            } if held_keys == &expected_vigem && held_buttons.is_empty() && pads.is_empty()
        )),
        "vigem_events={vigem_events:?}"
    );
    assert!(
        log_line.contains("release_backends=[\"software\", \"vigem\"]"),
        "log_line={log_line}"
    );
    println!(
        "readback=held_state_by_backend edge=release_all_vigem_key_no_pad before_state={before_state:?} before_vigem_events={before_vigem_events:?} after_state={after_state:?} vigem_events={vigem_events:?} log_line={log_line}"
    );
}

#[tokio::test(start_paused = true, flavor = "current_thread")]
async fn hardware_stuck_key_auto_release_uses_hardware_backend_and_log() {
    let trace_buffer = SharedTraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(trace_buffer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_level(false)
        .finish();
    let (software, _vigem, hardware, backends) = recording_backends(true);
    let (_handle, _snapshot_handle, mut emitter) = ActionEmitter::channel_with_backends(backends);
    let key = key_named("hardware-stuck");
    let before = emitter.snapshot();

    emitter
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Hardware,
        })
        .await
        .unwrap_or_else(|error| panic!("hardware keydown should hold state: {error}"));
    let after_key_down = emitter.snapshot();

    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(HELD_KEY_MAX_DURATION_MS)).await;
    tokio::task::yield_now().await;
    let auto_release = read_pending_auto_release(&mut emitter);
    let emitted_action = tracing::subscriber::with_default(subscriber, || {
        emitter
            .auto_release_held_key(&auto_release)
            .unwrap_or_else(|| panic!("hardware auto-release should emit a KeyUp"))
    });
    let after_auto_release_decision = emitter.snapshot();
    emitter
        .execute(emitted_action.clone())
        .await
        .unwrap_or_else(|error| panic!("hardware auto-release KeyUp should execute: {error}"));
    let after_key_up = emitter.snapshot();
    let log_output = trace_buffer.text();
    let log_line = find_log_line(&log_output, error_codes::STUCK_KEY_AUTO_RELEASED);
    let software_events = software.events();
    let hardware_events = hardware.events();

    assert!(before.held_keys.is_empty());
    assert_eq!(after_key_down.held_keys, vec![key.clone()]);
    assert_eq!(
        after_key_down
            .held_keys_by_backend
            .get(&ResolvedBackend::Hardware),
        Some(&vec![key.clone()])
    );
    assert_eq!(after_key_down.held_key_timer_count, 1);
    assert_auto_key_up_for_backend(Some(&emitted_action), &key, Backend::Hardware);
    assert_eq!(
        after_auto_release_decision.held_keys,
        vec![key.clone()],
        "auto-release decision should not pre-clear hardware state before backend KeyUp"
    );
    assert!(after_key_up.held_keys.is_empty());
    assert!(
        software_events.is_empty(),
        "software_events={software_events:?}"
    );
    assert_eq!(
        hardware_events,
        vec![
            RecordedInput::KeyDown { key: key.clone() },
            RecordedInput::KeyUp { key: key.clone() },
        ]
    );
    assert!(
        log_line.contains("code=STUCK_KEY_AUTO_RELEASED"),
        "log_line={log_line}"
    );
    assert!(
        log_line.contains("backend=\"hardware\""),
        "log_line={log_line}"
    );
    println!(
        "readback=held_state_by_backend edge=hardware_auto_release before={before:?} after_key_down={after_key_down:?} after_auto_release_decision={after_auto_release_decision:?} after_key_up={after_key_up:?} software_events={software_events:?} hardware_events={hardware_events:?} log_line={log_line}"
    );
}
