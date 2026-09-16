use synapse_action::{
    ACTION_QUEUE_CAPACITY, ActionBackend, EmitState, MAX_DRAG_DISTANCE_PX, RecordedInput,
    RecordingBackend,
};
use synapse_core::{
    Action, AimCurve, AimNaturalParams, Backend, MouseButton, MouseTarget, Point, error_codes,
};

/// Trigger: caller executes `Action::MouseDrag` against the recording backend.
/// Process: validation accepts a within-limit drag, then recording emits button
/// down, one curved absolute move to the target, and button up.
/// Outcome: `RecordingBackend.events()` contains exactly the three drag phases
/// and the held button source of truth is empty after release.
/// Source of truth: direct `RecordingBackend.events()` and `held_buttons()`.
#[test]
fn mouse_drag_records_three_phase_sequence() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();
    let curve = AimCurve::Natural {
        params: AimNaturalParams::FAST,
    };
    let action = drag_action(
        Point { x: 100, y: 100 },
        Point { x: 300, y: 300 },
        curve.clone(),
    );
    let before_events = backend.events();
    let before_buttons = backend.held_buttons();
    println!(
        "readback=recording_backend edge=valid_drag before_events={before_events:?} before_held_buttons={before_buttons:?}"
    );

    backend
        .execute(&action, &mut emit_state)
        .unwrap_or_else(|error| panic!("valid drag should execute: {error}"));

    let after_events = backend.events();
    let after_buttons = backend.held_buttons();
    println!(
        "readback=recording_backend edge=valid_drag after_state events={after_events:?} held_buttons={after_buttons:?} result_value=events:{}",
        after_events.len()
    );

    assert_eq!(before_events, Vec::<RecordedInput>::new());
    assert!(before_buttons.is_empty());
    assert_eq!(
        after_events,
        vec![
            RecordedInput::MouseButtonDown {
                button: MouseButton::Left,
            },
            RecordedInput::MouseMove {
                to: MouseTarget::Screen {
                    point: Point { x: 300, y: 300 },
                },
                curve,
                duration_ms: 200,
            },
            RecordedInput::MouseButtonUp {
                button: MouseButton::Left,
            },
        ]
    );
    assert!(after_buttons.is_empty());
    assert!(emit_state.snapshot().held_buttons.is_empty());
}

/// Trigger: caller executes drags at exact and unusual accepted boundaries.
/// Process: validation computes Euclidean distance through `Point::distance_to`.
/// Outcome: exact 4096 px, stationary, and negative-coordinate drags all
/// produce three recording events instead of an error.
/// Source of truth: direct `RecordingBackend.events()` after each action.
#[test]
fn mouse_drag_accepts_boundary_and_unusual_valid_edges() {
    for (edge, from, to) in [
        (
            "max_distance_exact",
            Point { x: 0, y: 0 },
            Point { x: 4096, y: 0 },
        ),
        ("stationary", Point { x: -12, y: 9 }, Point { x: -12, y: 9 }),
        (
            "negative_reverse",
            Point { x: 10, y: 10 },
            Point { x: -5, y: -15 },
        ),
    ] {
        let backend = RecordingBackend::new();
        let mut emit_state = EmitState::new();
        let before = backend.events();
        println!(
            "readback=recording_backend edge={edge} before=from:{from:?} to:{to:?} distance:{} events={before:?}",
            from.distance_to(to)
        );

        backend
            .execute(&drag_action(from, to, AimCurve::Linear), &mut emit_state)
            .unwrap_or_else(|error| panic!("{edge} drag should execute: {error}"));

        let after = backend.events();
        println!(
            "readback=recording_backend edge={edge} after_state events={after:?} result_value=events:{}",
            after.len()
        );
        assert_eq!(after.len(), 3);
        assert!(matches!(after[0], RecordedInput::MouseButtonDown { .. }));
        assert!(matches!(after[1], RecordedInput::MouseMove { .. }));
        assert!(matches!(after[2], RecordedInput::MouseButtonUp { .. }));
        assert!(backend.held_buttons().is_empty());
    }
}

/// Trigger: caller submits an over-limit drag.
/// Process: shared action validation rejects distance before recording mutates
/// state.
/// Outcome: error code is `ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT` and the
/// recording source of truth remains empty.
/// Source of truth: returned `ActionError.code()` plus
/// `RecordingBackend.events()` read after the failed call.
#[test]
fn mouse_drag_rejects_over_limit_without_recording_events() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();
    let action = drag_action(
        Point { x: 0, y: 0 },
        Point { x: 5000, y: 5000 },
        AimCurve::Instant,
    );
    let before = backend.events();
    println!(
        "readback=recording_backend edge=over_limit before_events={before:?} distance:{} max:{MAX_DRAG_DISTANCE_PX}",
        Point { x: 0, y: 0 }.distance_to(Point { x: 5000, y: 5000 })
    );

    let error = match backend.execute(&action, &mut emit_state) {
        Ok(()) => panic!("over-limit drag must fail closed"),
        Err(error) => error,
    };
    let after = backend.events();
    let after_state = emit_state.snapshot();
    println!(
        "readback=recording_backend edge=over_limit after_state events={after:?} state={after_state:?} data.code={} result_value=events:{}",
        error.code(),
        after.len()
    );

    assert_eq!(
        error.code(),
        error_codes::ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT
    );
    assert!(after.is_empty());
    assert!(after_state.held_buttons.is_empty());
}

/// Trigger: caller attempts to enqueue an over-limit drag through
/// `ActionHandle::try_execute`.
/// Process: shared validation runs before `mpsc::Sender::try_send`.
/// Outcome: the returned code is `ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT` and the
/// receiver queue length remains zero.
/// Source of truth: `mpsc::Receiver::len()` after the failed enqueue attempt.
#[test]
fn mouse_drag_over_limit_is_rejected_before_enqueue() {
    let (handle, rx) = synapse_action::ActionHandle::channel();
    let action = drag_action(
        Point { x: 0, y: 0 },
        Point { x: 5000, y: 5000 },
        AimCurve::Instant,
    );
    let before_len = rx.len();
    println!(
        "readback=action_queue edge=over_limit before_len={before_len} capacity={ACTION_QUEUE_CAPACITY}"
    );

    let error = match handle.try_execute(action) {
        Ok(()) => panic!("over-limit drag must not enqueue"),
        Err(error) => error,
    };
    let after_len = rx.len();
    println!(
        "readback=action_queue edge=over_limit after_len={after_len} data.code={} result_value=queue_len:{after_len}",
        error.code()
    );

    assert_eq!(before_len, 0);
    assert_eq!(after_len, 0);
    assert_eq!(
        error.code(),
        error_codes::ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT
    );
}

const fn drag_action(from: Point, to: Point, curve: AimCurve) -> Action {
    Action::MouseDrag {
        from,
        to,
        button: MouseButton::Left,
        curve,
        duration_ms: 200,
        backend: Backend::Software,
    }
}
