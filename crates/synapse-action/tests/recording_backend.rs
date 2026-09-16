use std::collections::{BTreeMap, BTreeSet, HashMap};

use synapse_action::{
    ActionBackend, EmitState, RecordedInput, RecordingBackend, sample_typing_schedule,
};
use synapse_core::{
    Action, AimCurve, AimNaturalParams, AimStyle, AimTarget, Backend, ButtonAction, ComboInput,
    ComboStep, GamepadReport, Key, KeyCode, KeystrokeDynamics, KeystrokeNaturalParams, MouseButton,
    MouseTarget, PadButton, PathPoint, PathSpec, Point, Stick, StrokeMotionModel, StrokeTiming,
    Trigger, VelocityProfile,
};

#[test]
fn recording_backend_records_every_action_variant_deterministically() {
    let mut snapshot = BTreeMap::new();

    for case in action_cases() {
        let backend = RecordingBackend::new();
        let mut emit_state = EmitState::new();
        let before_events = backend.events();
        let before_held_keys = backend.held_keys();
        let before_held_buttons = backend.held_buttons();
        let before_pad_state = backend.pad_state();
        println!(
            "readback=recording_backend edge={} before_events={before_events:?} before_held_keys={before_held_keys:?} before_held_buttons={before_held_buttons:?} before_pad_state={before_pad_state:?}",
            case.edge
        );

        backend
            .execute(&case.action, &mut emit_state)
            .unwrap_or_else(|err| panic!("recording backend must accept {}: {err}", case.edge));

        let after_events = backend.events();
        let after_held_keys = backend.held_keys();
        let after_held_buttons = backend.held_buttons();
        let after_pad_state = backend.pad_state();
        println!(
            "readback=recording_backend edge={} after_state events={after_events:?} held_keys={after_held_keys:?} held_buttons={after_held_buttons:?} pad_state={after_pad_state:?} result_value=events:{}",
            case.edge,
            after_events.len()
        );

        assert_eq!(before_events, Vec::<RecordedInput>::new());
        assert_eq!(before_held_keys, BTreeSet::new());
        assert_eq!(before_held_buttons, BTreeSet::new());
        assert_eq!(before_pad_state, HashMap::new());
        assert_eq!(after_events, case.expected_events);
        assert_eq!(after_held_keys, case.expected_held_keys);
        assert_eq!(after_held_buttons, case.expected_held_buttons);
        assert_eq!(after_pad_state, case.expected_pad_state);
        assert_eq!(
            emit_state.snapshot().held_keys.len(),
            case.expected_emit_keys
        );
        assert_eq!(
            emit_state.snapshot().held_buttons.len(),
            case.expected_emit_buttons
        );
        assert_eq!(emit_state.snapshot().pad_state, case.expected_pad_state);

        snapshot.insert(case.edge, after_events);
    }

    insta::assert_json_snapshot!("recording_backend_action_variants", snapshot);
}

#[test]
fn recording_backend_manual_edges() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();

    print_truth("empty_text", "before", &backend);
    backend
        .execute(
            &Action::TypeText {
                text: String::new(),
                dynamics: KeystrokeDynamics::Natural {
                    params: KeystrokeNaturalParams::FAST,
                },
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("empty text should record as no input events: {err}"));
    print_truth("empty_text", "after_state", &backend);
    assert_eq!(backend.events(), Vec::<RecordedInput>::new());
    assert_eq!(backend.held_keys(), BTreeSet::new());

    print_truth("unmatched_key_up", "before", &backend);
    backend
        .execute(
            &Action::KeyUp {
                key: key("ghost"),
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("unmatched key up should still be recorded: {err}"));
    print_truth("unmatched_key_up", "after_state", &backend);
    assert_eq!(
        backend.events(),
        vec![RecordedInput::KeyUp { key: key("ghost") }]
    );
    assert_eq!(backend.held_keys(), BTreeSet::new());

    print_truth("duplicate_key_down", "before", &backend);
    let key_shift = key("shift");
    backend
        .execute(
            &Action::KeyDown {
                key: key_shift.clone(),
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("first key down should record: {err}"));
    backend
        .execute(
            &Action::KeyDown {
                key: key_shift,
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("duplicate key down should record: {err}"));
    print_truth("duplicate_key_down", "after_state", &backend);
    assert_eq!(
        backend.held_keys(),
        BTreeSet::from([KeyCode::Named {
            value: "shift".to_owned()
        }])
    );

    print_truth("mixed_release_all", "before", &backend);
    backend
        .execute(
            &Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Down,
                hold_ms: 0,
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("mouse down should record: {err}"));
    backend
        .execute(
            &Action::PadStick {
                pad: 2,
                stick: Stick::Right,
                x: 0.5,
                y: -0.5,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("pad stick should record: {err}"));
    backend
        .execute(&Action::ReleaseAll, &mut emit_state)
        .unwrap_or_else(|err| panic!("release all should record: {err}"));
    print_truth("mixed_release_all", "after_state", &backend);
    assert_eq!(backend.held_keys(), BTreeSet::new());
    assert_eq!(backend.held_buttons(), BTreeSet::new());
    assert_eq!(backend.pad_state(), HashMap::new());
    assert_eq!(emit_state.snapshot().held_keys, Vec::<Key>::new());
    assert_eq!(
        emit_state.snapshot().held_buttons,
        Vec::<MouseButton>::new()
    );
    assert_eq!(emit_state.snapshot().pad_state, HashMap::new());
    println!("readback=recording_backend edge=mixed_release_all result_value=empty");
}

#[test]
fn recording_backend_delta_readback_edges() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();

    let empty_mark = backend.event_count();
    println!("readback=recording_backend_delta edge=empty before_count={empty_mark}");
    let empty_delta = backend.events_since(empty_mark);
    println!("readback=recording_backend_delta edge=empty after_delta={empty_delta:?}");
    assert_eq!(empty_mark, 0);
    assert_eq!(empty_delta, Vec::<RecordedInput>::new());

    backend
        .execute(
            &Action::KeyDown {
                key: key("shift"),
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("synthetic key down should record: {err}"));
    let after_setup_count = backend.event_count();
    println!("readback=recording_backend_delta edge=single before_count={after_setup_count}");
    backend
        .execute(
            &Action::KeyUp {
                key: key("shift"),
                backend: Backend::Software,
            },
            &mut emit_state,
        )
        .unwrap_or_else(|err| panic!("synthetic key up should record: {err}"));
    let single_delta = backend.events_since(after_setup_count);
    println!("readback=recording_backend_delta edge=single after_delta={single_delta:?}");
    assert_eq!(
        single_delta,
        vec![RecordedInput::KeyUp { key: key("shift") }]
    );

    let beyond_end = backend.event_count() + 10;
    let beyond_delta = backend.events_since(beyond_end);
    println!(
        "readback=recording_backend_delta edge=beyond_end before_count={beyond_end} after_delta={beyond_delta:?}"
    );
    assert_eq!(beyond_delta, Vec::<RecordedInput>::new());
}

#[derive(Debug)]
struct RecordingCase {
    edge: &'static str,
    action: Action,
    expected_events: Vec<RecordedInput>,
    expected_held_keys: BTreeSet<KeyCode>,
    expected_held_buttons: BTreeSet<MouseButton>,
    expected_pad_state: HashMap<u8, GamepadReport>,
    expected_emit_keys: usize,
    expected_emit_buttons: usize,
}

fn action_cases() -> Vec<RecordingCase> {
    vec![
        key_press_case(),
        key_down_case(),
        key_up_case(),
        key_chord_case(),
        type_text_case(),
        mouse_move_case(),
        mouse_move_relative_case(),
        mouse_button_case(),
        mouse_drag_case(),
        mouse_stroke_case(),
        mouse_scroll_case(),
        pad_button_case(),
        pad_stick_case(),
        pad_trigger_case(),
        pad_report_case(),
        aim_at_case(),
        combo_case(),
        release_all_case(),
    ]
}

fn key_press_case() -> RecordingCase {
    let key_a = key("a");
    RecordingCase {
        edge: "key_press",
        action: Action::KeyPress {
            key: key_a.clone(),
            hold_ms: 33,
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::KeyDown { key: key_a.clone() },
            RecordedInput::DelayMs { ms: 33 },
            RecordedInput::KeyUp { key: key_a },
        ],
        ..empty_expectations()
    }
}

fn key_down_case() -> RecordingCase {
    let key_shift = key("shift");
    RecordingCase {
        edge: "key_down",
        action: Action::KeyDown {
            key: key_shift.clone(),
            backend: Backend::Software,
        },
        expected_events: vec![RecordedInput::KeyDown { key: key_shift }],
        expected_held_keys: BTreeSet::from([named_code("shift")]),
        expected_emit_keys: 1,
        ..empty_expectations()
    }
}

fn key_up_case() -> RecordingCase {
    let key_shift = key("shift");
    RecordingCase {
        edge: "key_up",
        action: Action::KeyUp {
            key: key_shift.clone(),
            backend: Backend::Software,
        },
        expected_events: vec![RecordedInput::KeyUp { key: key_shift }],
        ..empty_expectations()
    }
}

fn key_chord_case() -> RecordingCase {
    let key_ctrl = key("ctrl");
    let key_s = key("s");
    RecordingCase {
        edge: "key_chord",
        action: Action::KeyChord {
            keys: vec![key_ctrl.clone(), key_s.clone()],
            hold_ms: 33,
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::KeyDown {
                key: key_ctrl.clone(),
            },
            RecordedInput::KeyDown { key: key_s.clone() },
            RecordedInput::DelayMs { ms: 33 },
            RecordedInput::KeyUp { key: key_s },
            RecordedInput::KeyUp { key: key_ctrl },
        ],
        ..empty_expectations()
    }
}

fn type_text_case() -> RecordingCase {
    let dynamics = KeystrokeDynamics::Natural {
        params: KeystrokeNaturalParams::FAST,
    };
    let ikis = non_zero_ikis("Az", &dynamics);
    let delay = ikis
        .first()
        .copied()
        .unwrap_or_else(|| panic!("two-character natural typing must have one non-zero IKI"));
    RecordingCase {
        edge: "type_text",
        action: Action::TypeText {
            text: "Az".to_owned(),
            dynamics,
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::KeyDown { key: key("shift") },
            RecordedInput::KeyDown { key: key("a") },
            RecordedInput::KeyUp { key: key("a") },
            RecordedInput::KeyUp { key: key("shift") },
            RecordedInput::DelayMs { ms: delay },
            RecordedInput::KeyDown { key: key("z") },
            RecordedInput::KeyUp { key: key("z") },
        ],
        ..empty_expectations()
    }
}

fn non_zero_ikis(text: &str, dynamics: &KeystrokeDynamics) -> Vec<u32> {
    sample_typing_schedule(text, dynamics, None)
        .iter()
        .filter_map(|event| (event.iki_ms_before > 0).then_some(event.iki_ms_before))
        .collect()
}

fn mouse_move_case() -> RecordingCase {
    let target = MouseTarget::Screen {
        point: Point { x: 10, y: 20 },
    };
    let curve = AimCurve::Natural {
        params: AimNaturalParams::FAST,
    };
    RecordingCase {
        edge: "mouse_move",
        action: Action::MouseMove {
            to: target.clone(),
            curve: curve.clone(),
            duration_ms: 50,
            backend: Backend::Software,
        },
        expected_events: vec![RecordedInput::MouseMove {
            to: target,
            curve,
            duration_ms: 50,
        }],
        ..empty_expectations()
    }
}

fn mouse_move_relative_case() -> RecordingCase {
    RecordingCase {
        edge: "mouse_move_relative",
        action: Action::MouseMoveRelative {
            dx: 3.5,
            dy: -2.25,
            backend: Backend::Software,
        },
        expected_events: vec![RecordedInput::MouseMoveRelative { dx: 3.5, dy: -2.25 }],
        ..empty_expectations()
    }
}

fn mouse_button_case() -> RecordingCase {
    RecordingCase {
        edge: "mouse_button",
        action: Action::MouseButton {
            button: MouseButton::Left,
            action: ButtonAction::Press,
            hold_ms: 16,
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::MouseButtonDown {
                button: MouseButton::Left,
            },
            RecordedInput::DelayMs { ms: 16 },
            RecordedInput::MouseButtonUp {
                button: MouseButton::Left,
            },
        ],
        ..empty_expectations()
    }
}

fn mouse_drag_case() -> RecordingCase {
    let curve = AimCurve::Natural {
        params: AimNaturalParams::FAST,
    };
    RecordingCase {
        edge: "mouse_drag",
        action: Action::MouseDrag {
            from: Point { x: 1, y: 2 },
            to: Point { x: 11, y: 22 },
            button: MouseButton::Left,
            curve: curve.clone(),
            duration_ms: 200,
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::MouseButtonDown {
                button: MouseButton::Left,
            },
            RecordedInput::MouseMove {
                to: MouseTarget::Screen {
                    point: Point { x: 11, y: 22 },
                },
                curve,
                duration_ms: 200,
            },
            RecordedInput::MouseButtonUp {
                button: MouseButton::Left,
            },
        ],
        ..empty_expectations()
    }
}

fn mouse_stroke_case() -> RecordingCase {
    RecordingCase {
        edge: "mouse_stroke",
        action: Action::MouseStroke {
            path: PathSpec::Line {
                from: PathPoint::new(0.0, 0.0),
                to: PathPoint::new(4.0, 0.0),
            },
            button: Some(MouseButton::Left),
            profile: VelocityProfile::Constant,
            timing: StrokeTiming::DurationMs { duration_ms: 4 },
            motion_model: StrokeMotionModel::Path,
            humanize: None,
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::MouseButtonDown {
                button: MouseButton::Left,
            },
            RecordedInput::MouseStrokePoint {
                elapsed_ms: 0.0,
                point: Point { x: 0, y: 0 },
            },
            RecordedInput::MouseStrokePoint {
                elapsed_ms: 1.0,
                point: Point { x: 1, y: 0 },
            },
            RecordedInput::MouseStrokePoint {
                elapsed_ms: 2.0,
                point: Point { x: 2, y: 0 },
            },
            RecordedInput::MouseStrokePoint {
                elapsed_ms: 3.0,
                point: Point { x: 3, y: 0 },
            },
            RecordedInput::MouseStrokePoint {
                elapsed_ms: 4.0,
                point: Point { x: 4, y: 0 },
            },
            RecordedInput::MouseButtonUp {
                button: MouseButton::Left,
            },
        ],
        ..empty_expectations()
    }
}

fn mouse_scroll_case() -> RecordingCase {
    RecordingCase {
        edge: "mouse_scroll",
        action: Action::MouseScroll {
            dy: -3,
            dx: 1,
            at: Some(Point { x: 5, y: 6 }),
            backend: Backend::Software,
        },
        expected_events: vec![RecordedInput::MouseScroll {
            dy: -3,
            dx: 1,
            at: Some(Point { x: 5, y: 6 }),
        }],
        ..empty_expectations()
    }
}

fn pad_button_case() -> RecordingCase {
    RecordingCase {
        edge: "pad_button",
        action: Action::PadButton {
            pad: 0,
            button: PadButton::A,
            action: ButtonAction::Press,
            hold_ms: 33,
        },
        expected_events: vec![
            RecordedInput::PadButtonDown {
                pad: 0,
                button: PadButton::A,
            },
            RecordedInput::DelayMs { ms: 33 },
            RecordedInput::PadButtonUp {
                pad: 0,
                button: PadButton::A,
            },
        ],
        ..empty_expectations()
    }
}

fn pad_stick_case() -> RecordingCase {
    let report = GamepadReport {
        thumb_l: (0.25, -0.5),
        ..neutral_report()
    };
    RecordingCase {
        edge: "pad_stick",
        action: Action::PadStick {
            pad: 1,
            stick: Stick::Left,
            x: 0.25,
            y: -0.5,
        },
        expected_events: vec![RecordedInput::PadStick {
            pad: 1,
            stick: Stick::Left,
            x: 0.25,
            y: -0.5,
        }],
        expected_pad_state: HashMap::from([(1, report)]),
        ..empty_expectations()
    }
}

fn pad_trigger_case() -> RecordingCase {
    let report = GamepadReport {
        rt: 0.75,
        ..neutral_report()
    };
    RecordingCase {
        edge: "pad_trigger",
        action: Action::PadTrigger {
            pad: 1,
            trigger: Trigger::Right,
            value: 0.75,
        },
        expected_events: vec![RecordedInput::PadTrigger {
            pad: 1,
            trigger: Trigger::Right,
            value: 0.75,
        }],
        expected_pad_state: HashMap::from([(1, report)]),
        ..empty_expectations()
    }
}

fn pad_report_case() -> RecordingCase {
    let report = GamepadReport {
        buttons: vec![PadButton::A, PadButton::Start],
        thumb_l: (0.0, 0.0),
        thumb_r: (0.0, 0.5),
        lt: 0.25,
        rt: 0.0,
        ..GamepadReport::default()
    };
    RecordingCase {
        edge: "pad_report",
        action: Action::PadReport {
            pad: 1,
            report: report.clone(),
        },
        expected_events: vec![RecordedInput::PadReport {
            pad: 1,
            report: report.clone(),
        }],
        expected_pad_state: HashMap::from([(1, report)]),
        ..empty_expectations()
    }
}

fn aim_at_case() -> RecordingCase {
    let target = AimTarget::Screen {
        point: Point { x: 30, y: 40 },
    };
    RecordingCase {
        edge: "aim_at",
        action: Action::AimAt {
            target: target.clone(),
            style: AimStyle::Snap,
            deadline_ms: 60,
            backend: Backend::Software,
        },
        expected_events: vec![RecordedInput::AimAt {
            target,
            style: AimStyle::Snap,
            deadline_ms: 60,
        }],
        ..empty_expectations()
    }
}

fn combo_case() -> RecordingCase {
    let key_a = key("a");
    RecordingCase {
        edge: "combo",
        action: Action::Combo {
            steps: vec![
                ComboStep {
                    at_ms: 0,
                    input: ComboInput::KeyDown { key: key_a.clone() },
                },
                ComboStep {
                    at_ms: 33,
                    input: ComboInput::KeyUp { key: key_a.clone() },
                },
            ],
            backend: Backend::Software,
        },
        expected_events: vec![
            RecordedInput::ComboAt { at_ms: 0 },
            RecordedInput::KeyDown { key: key_a.clone() },
            RecordedInput::ComboAt { at_ms: 33 },
            RecordedInput::KeyUp { key: key_a },
        ],
        ..empty_expectations()
    }
}

fn release_all_case() -> RecordingCase {
    RecordingCase {
        edge: "release_all",
        action: Action::ReleaseAll,
        expected_events: vec![RecordedInput::ReleaseAll {
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            pads: Vec::new(),
        }],
        ..empty_expectations()
    }
}

fn empty_expectations() -> RecordingCase {
    RecordingCase {
        edge: "placeholder",
        action: Action::ReleaseAll,
        expected_events: Vec::new(),
        expected_held_keys: BTreeSet::new(),
        expected_held_buttons: BTreeSet::new(),
        expected_pad_state: HashMap::new(),
        expected_emit_keys: 0,
        expected_emit_buttons: 0,
    }
}

fn print_truth(edge: &str, phase: &str, backend: &RecordingBackend) {
    println!(
        "readback=recording_backend edge={edge} {phase} events={:?} held_keys={:?} held_buttons={:?} pad_state={:?}",
        backend.events(),
        backend.held_keys(),
        backend.held_buttons(),
        backend.pad_state()
    );
}

fn key(value: &str) -> Key {
    Key {
        code: named_code(value),
        use_scancode: false,
    }
}

fn named_code(value: &str) -> KeyCode {
    KeyCode::Named {
        value: value.to_owned(),
    }
}

const fn neutral_report() -> GamepadReport {
    GamepadReport::neutral(synapse_core::GamepadController::X360)
}
