use synapse_action::{
    ActionBackend, ActionError, EmitState, HardwareUnavailableBackend, ResolvedBackend,
    resolve_backend,
};
use synapse_core::{
    Action, AimCurve, AimStyle, AimTarget, Backend, ButtonAction, ComboInput, ComboStep,
    GamepadReport, Key, KeyCode, KeystrokeDynamics, KeystrokeNaturalParams, MouseButton,
    MouseTarget, PadButton, PathPoint, PathSpec, Point, Stick, StrokeMotionModel, StrokeTiming,
    Trigger, VelocityProfile, error_codes,
};

#[test]
fn hardware_backend_resolves_to_fail_closed_stub() {
    let action = Action::KeyPress {
        key: key("a"),
        hold_ms: 33,
        backend: Backend::Hardware,
    };
    let before = Backend::Hardware;
    let after = resolve_backend(before, &action)
        .unwrap_or_else(|err| panic!("hardware should resolve to unavailable backend: {err}"));

    assert_eq!(after, ResolvedBackend::Hardware);
    println!(
        "readback=hardware_resolution edge=act_press before_backend={before:?} after_backend={} result_value={after:?}",
        after.as_str()
    );
}

#[test]
fn hardware_unavailable_backend_rejects_every_action_variant() {
    let backend = HardwareUnavailableBackend::new();

    for (edge, action) in action_cases() {
        let mut state = EmitState::new();
        let before = state.snapshot();
        let error = match backend.execute(&action, &mut state) {
            Ok(()) => panic!("hardware backend must fail closed for {edge}"),
            Err(error) => error,
        };
        let after = state.snapshot();

        assert!(matches!(error, ActionError::BackendUnavailable { .. }));
        assert_eq!(error.code(), error_codes::ACTION_BACKEND_UNAVAILABLE);
        assert_eq!(before, after);
        assert!(error.detail().contains("backend=hardware"));
        assert!(error.detail().contains("backend removed"));
        assert!(error.detail().contains("backend=software"));
        assert!(error.detail().contains("backend=vigem"));
        assert!(error.detail().contains(action_kind(&action)));
        println!(
            "readback=hardware_m2 edge={edge} before={before:?} after={after:?} after_code={} detail={}",
            error.code(),
            error.detail()
        );
    }
}

fn action_cases() -> Vec<(&'static str, Action)> {
    let mut cases = Vec::new();
    cases.extend(keyboard_actions());
    cases.extend(mouse_actions());
    cases.extend(pad_actions());
    cases.extend(other_actions());
    cases
}

fn keyboard_actions() -> Vec<(&'static str, Action)> {
    vec![
        (
            "key_press",
            Action::KeyPress {
                key: key("a"),
                hold_ms: 33,
                backend: Backend::Hardware,
            },
        ),
        (
            "key_down",
            Action::KeyDown {
                key: key("shift"),
                backend: Backend::Hardware,
            },
        ),
        (
            "key_up",
            Action::KeyUp {
                key: key("shift"),
                backend: Backend::Hardware,
            },
        ),
        (
            "key_chord",
            Action::KeyChord {
                keys: vec![key("control"), key("s")],
                hold_ms: 33,
                backend: Backend::Hardware,
            },
        ),
        (
            "type_text",
            Action::TypeText {
                text: "synthetic".to_owned(),
                dynamics: KeystrokeDynamics::Natural {
                    params: KeystrokeNaturalParams::FAST,
                },
                backend: Backend::Hardware,
            },
        ),
    ]
}

fn mouse_actions() -> Vec<(&'static str, Action)> {
    vec![
        (
            "mouse_move",
            Action::MouseMove {
                to: MouseTarget::Screen {
                    point: Point { x: 10, y: 20 },
                },
                curve: AimCurve::Instant,
                duration_ms: 1,
                backend: Backend::Hardware,
            },
        ),
        (
            "mouse_move_relative",
            Action::MouseMoveRelative {
                dx: 3.0,
                dy: -2.0,
                backend: Backend::Hardware,
            },
        ),
        (
            "mouse_button",
            Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Press,
                hold_ms: 1,
                backend: Backend::Hardware,
            },
        ),
        (
            "mouse_drag",
            Action::MouseDrag {
                from: Point { x: 1, y: 2 },
                to: Point { x: 3, y: 4 },
                button: MouseButton::Left,
                curve: AimCurve::Instant,
                duration_ms: 1,
                backend: Backend::Hardware,
            },
        ),
        (
            "mouse_stroke",
            Action::MouseStroke {
                path: PathSpec::Line {
                    from: PathPoint::new(1.0, 2.0),
                    to: PathPoint::new(3.0, 4.0),
                },
                button: Some(MouseButton::Left),
                profile: VelocityProfile::Linear,
                timing: StrokeTiming::DurationMs { duration_ms: 1 },
                motion_model: StrokeMotionModel::Path,
                humanize: None,
                backend: Backend::Hardware,
            },
        ),
        (
            "mouse_scroll",
            Action::MouseScroll {
                dy: 1,
                dx: 0,
                at: Some(Point { x: 5, y: 6 }),
                backend: Backend::Hardware,
            },
        ),
    ]
}

fn pad_actions() -> Vec<(&'static str, Action)> {
    vec![
        (
            "pad_button",
            Action::PadButton {
                pad: 0,
                button: PadButton::A,
                action: ButtonAction::Press,
                hold_ms: 1,
            },
        ),
        (
            "pad_stick",
            Action::PadStick {
                pad: 0,
                stick: Stick::Left,
                x: 0.25,
                y: -0.5,
            },
        ),
        (
            "pad_trigger",
            Action::PadTrigger {
                pad: 0,
                trigger: Trigger::Right,
                value: 0.75,
            },
        ),
        (
            "pad_report",
            Action::PadReport {
                pad: 0,
                report: GamepadReport {
                    buttons: vec![PadButton::A],
                    thumb_l: (0.0, 0.0),
                    thumb_r: (0.0, 0.0),
                    lt: 0.0,
                    rt: 1.0,
                    ..GamepadReport::default()
                },
            },
        ),
    ]
}

fn other_actions() -> Vec<(&'static str, Action)> {
    vec![
        (
            "aim_at",
            Action::AimAt {
                target: AimTarget::Screen {
                    point: Point { x: 8, y: 9 },
                },
                style: AimStyle::Snap,
                deadline_ms: 16,
                backend: Backend::Hardware,
            },
        ),
        (
            "combo",
            Action::Combo {
                steps: vec![ComboStep {
                    at_ms: 0,
                    input: ComboInput::KeyPress {
                        key: key("enter"),
                        hold_ms: 1,
                    },
                }],
                backend: Backend::Hardware,
            },
        ),
        ("release_all", Action::ReleaseAll),
    ]
}

fn key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

const fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::KeyPress { .. } => "key_press",
        Action::KeyDown { .. } => "key_down",
        Action::KeyUp { .. } => "key_up",
        Action::KeyChord { .. } => "key_chord",
        Action::TypeText { .. } => "type_text",
        Action::MouseMove { .. } => "mouse_move",
        Action::MouseMoveRelative { .. } => "mouse_move_relative",
        Action::MouseButton { .. } => "mouse_button",
        Action::MouseDrag { .. } => "mouse_drag",
        Action::MouseStroke { .. } => "mouse_stroke",
        Action::MouseScroll { .. } => "mouse_scroll",
        Action::PadButton { .. } => "pad_button",
        Action::PadStick { .. } => "pad_stick",
        Action::PadTrigger { .. } => "pad_trigger",
        Action::PadReport { .. } => "pad_report",
        Action::AimAt { .. } => "aim_at",
        Action::Combo { .. } => "combo",
        Action::ReleaseAll => "release_all",
    }
}
