#![cfg(not(windows))]
#![cfg_attr(all(unix, not(target_os = "macos")), allow(dead_code, unused_imports))]

use synapse_action::{
    ActionBackend, ActionError, EmitState, RecordingBackend,
    backend::software::{SoftwareBackend, cursor_position},
};
use synapse_core::{
    Action, AimCurve, AimStyle, AimTarget, Backend, ButtonAction, ComboInput, ComboStep,
    GamepadReport, Key, KeyCode, KeystrokeDynamics, KeystrokeNaturalParams, MouseButton,
    MouseTarget, PadButton, Point, Stick, Trigger, error_codes,
};

#[test]
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn software_backend_returns_unavailable_for_every_action_variant_on_non_windows() {
    let backend = SoftwareBackend::new();

    for (edge, action) in action_cases() {
        let mut state = EmitState::new();
        let before = state.snapshot();
        let result = backend.execute(&action, &mut state);
        let after = state.snapshot();

        // Empty `ReleaseAll` is the documented exception: it performs no I/O
        // on any platform (the held-state loops emit zero SendInput calls on
        // Windows). The non-Windows stub mirrors that semantic so safety
        // paths — cancel/shutdown, panic hook, M2 `release_all` tool — can
        // dispatch `ReleaseAll` on Linux/macOS dev hosts without falsely
        // surfacing ACTION_BACKEND_UNAVAILABLE for a no-op.
        if matches!(action, Action::ReleaseAll) {
            assert!(
                result.is_ok(),
                "empty ReleaseAll on non-Windows must succeed as a no-op: {result:?}"
            );
            println!(
                "readback=software_linux edge={edge} before={before:?} after={after:?} after_code=OK (empty-state no-op)"
            );
            continue;
        }

        let error = match result {
            Ok(()) => panic!("non-Windows software backend must fail closed for {edge}"),
            Err(error) => error,
        };

        assert!(matches!(error, ActionError::BackendUnavailable { .. }));
        assert_eq!(error.code(), error_codes::ACTION_BACKEND_UNAVAILABLE);
        assert_eq!(before, after);
        assert!(error.detail().contains("requires Windows"));
        println!(
            "readback=software_linux edge={edge} before={before:?} after={after:?} after_code={}",
            error.code()
        );
    }
}

#[test]
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn non_windows_software_backend_release_all_on_non_empty_state_fails_closed() {
    let mut state = EmitState::new();
    // Drive the recording backend to seed `state.held_keys` without leaning
    // on EmitState's pub(crate) helpers from inside an integration test.
    let recording = RecordingBackend::new();
    recording
        .execute(
            &Action::KeyDown {
                key: key("synthetic-stuck"),
                backend: Backend::Software,
            },
            &mut state,
        )
        .unwrap_or_else(|error| panic!("recording KeyDown must seed state: {error}"));
    let before = state.snapshot();
    assert_eq!(before.held_keys.len(), 1);
    println!("readback=software_linux edge=release_all_non_empty before={before:?}");

    let backend = SoftwareBackend::new();
    let result = backend.execute(&Action::ReleaseAll, &mut state);
    let after = state.snapshot();
    let error = match result {
        Ok(()) => panic!(
            "non-Windows software backend must fail-closed for ReleaseAll on non-empty state"
        ),
        Err(error) => error,
    };
    assert_eq!(error.code(), error_codes::ACTION_BACKEND_UNAVAILABLE);
    assert_eq!(before, after);
    println!(
        "readback=software_linux edge=release_all_non_empty after={after:?} after_code={}",
        error.code()
    );
}

#[test]
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn cursor_position_fails_closed_on_non_windows() {
    println!("readback=software_cursor_linux edge=read before=platform:not_windows");
    let error = match cursor_position() {
        Ok(point) => panic!("non-Windows cursor_position must fail closed, got {point:?}"),
        Err(error) => error,
    };
    println!(
        "readback=software_cursor_linux edge=read after_code={} after_detail={:?}",
        error.code(),
        error.detail()
    );
    assert_eq!(error.code(), error_codes::ACTION_BACKEND_UNAVAILABLE);
    assert!(error.detail().contains("requires Windows"));
}

#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_software_backend_zero_io_edges_succeed_without_emitting_input() {
    let backend = SoftwareBackend::new();
    for (edge, action) in [
        (
            "type_text_empty",
            Action::TypeText {
                text: String::new(),
                dynamics: KeystrokeDynamics::Burst,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_move_relative_zero",
            Action::MouseMoveRelative {
                dx: 0.0,
                dy: 0.0,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_scroll_zero",
            Action::MouseScroll {
                dy: 0,
                dx: 0,
                at: None,
                backend: Backend::Software,
            },
        ),
        ("release_all_empty", Action::ReleaseAll),
    ] {
        let mut state = EmitState::new();
        let before = state.snapshot();
        backend
            .execute(&action, &mut state)
            .unwrap_or_else(|error| panic!("{edge} should succeed without I/O: {error}"));
        let after = state.snapshot();
        assert_eq!(before, after);
        println!("readback=software_linux_x11 edge={edge} before={before:?} after={after:?}");
    }
}

#[test]
#[cfg(all(unix, not(target_os = "macos")))]
fn linux_software_backend_keeps_gamepad_fail_closed() {
    let backend = SoftwareBackend::new();
    for (edge, action) in pad_actions() {
        let mut state = EmitState::new();
        let before = state.snapshot();
        let error = match backend.execute(&action, &mut state) {
            Ok(()) => panic!("software backend must not emit gamepad action {edge}"),
            Err(error) => error,
        };
        let after = state.snapshot();
        assert_eq!(error.code(), error_codes::ACTION_BACKEND_UNAVAILABLE);
        assert_eq!(before, after);
        println!(
            "readback=software_linux_x11 edge={edge} before={before:?} after={after:?} after_code={}",
            error.code()
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
                hold_ms: 1,
                backend: Backend::Software,
            },
        ),
        (
            "key_down",
            Action::KeyDown {
                key: key("shift"),
                backend: Backend::Software,
            },
        ),
        (
            "key_up",
            Action::KeyUp {
                key: key("shift"),
                backend: Backend::Software,
            },
        ),
        (
            "key_chord",
            Action::KeyChord {
                keys: vec![key("control"), key("s")],
                hold_ms: 1,
                backend: Backend::Software,
            },
        ),
        (
            "type_text",
            Action::TypeText {
                text: "synthetic".to_owned(),
                dynamics: KeystrokeDynamics::Natural {
                    params: KeystrokeNaturalParams::FAST,
                },
                backend: Backend::Software,
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
                backend: Backend::Software,
            },
        ),
        (
            "mouse_move_relative",
            Action::MouseMoveRelative {
                dx: 3.0,
                dy: -2.0,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_button",
            Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Press,
                hold_ms: 1,
                backend: Backend::Software,
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
                backend: Backend::Software,
            },
        ),
        (
            "mouse_scroll",
            Action::MouseScroll {
                dy: 1,
                dx: 0,
                at: Some(Point { x: 5, y: 6 }),
                backend: Backend::Software,
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
                backend: Backend::Software,
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
                backend: Backend::Software,
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
