use std::collections::BTreeMap;

use synapse_core::{
    Action, AimCurve, AimNaturalParams, AimStyle, AimTarget, Backend, ButtonAction, ComboInput,
    ComboStep, ElementId, GamepadReport, HumanizeParams, Key, KeyCode, KeystrokeDynamics,
    KeystrokeNaturalParams, MouseButton, MouseTarget, PadButton, PathPoint, PathSpec, Point, Stick,
    StrokeMotionModel, StrokeTiming, Trigger, VelocityProfile,
};

#[test]
fn action_variants() {
    insta::assert_json_snapshot!("action_variants", canonical_actions());
}

fn canonical_actions() -> BTreeMap<&'static str, Action> {
    let mut actions = BTreeMap::new();
    actions.extend(keyboard_actions());
    actions.extend(mouse_actions());
    actions.extend(pad_actions());
    actions.extend(compound_actions());
    actions
}

fn named_key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

fn keyboard_actions() -> BTreeMap<&'static str, Action> {
    let key_a = Key {
        code: KeyCode::Named {
            value: "a".to_owned(),
        },
        use_scancode: false,
    };
    let key_ctrl = Key {
        code: KeyCode::Named {
            value: "ctrl".to_owned(),
        },
        use_scancode: false,
    };

    BTreeMap::from([
        (
            "key_press",
            Action::KeyPress {
                key: key_a.clone(),
                hold_ms: 33,
                backend: Backend::Software,
            },
        ),
        (
            "key_down",
            Action::KeyDown {
                key: key_a.clone(),
                backend: Backend::Software,
            },
        ),
        (
            "key_up",
            Action::KeyUp {
                key: key_a.clone(),
                backend: Backend::Software,
            },
        ),
        (
            "key_chord",
            Action::KeyChord {
                keys: vec![key_ctrl, key_a],
                hold_ms: 33,
                backend: Backend::Software,
            },
        ),
        (
            "type_text",
            Action::TypeText {
                text: "Hello world.".to_owned(),
                dynamics: KeystrokeDynamics::Natural {
                    params: KeystrokeNaturalParams::FAST,
                },
                backend: Backend::Software,
            },
        ),
    ])
}

fn mouse_actions() -> BTreeMap<&'static str, Action> {
    BTreeMap::from([
        (
            "mouse_move",
            Action::MouseMove {
                to: MouseTarget::Screen {
                    point: Point { x: 100, y: 200 },
                },
                curve: AimCurve::Natural {
                    params: AimNaturalParams::FAST,
                },
                duration_ms: 50,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_move_relative",
            Action::MouseMoveRelative {
                dx: 10.5,
                dy: -4.25,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_button",
            Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Press,
                hold_ms: 16,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_drag",
            Action::MouseDrag {
                from: Point { x: 10, y: 20 },
                to: Point { x: 110, y: 220 },
                button: MouseButton::Left,
                curve: AimCurve::EaseInOut,
                duration_ms: 200,
                backend: Backend::Software,
            },
        ),
        (
            "mouse_stroke",
            Action::MouseStroke {
                path: PathSpec::Circle {
                    center: PathPoint::new(200.0, 150.0),
                    radius: 32.0,
                },
                button: Some(MouseButton::Left),
                profile: VelocityProfile::MinimumJerk,
                timing: StrokeTiming::DurationMs { duration_ms: 750 },
                motion_model: StrokeMotionModel::Path,
                humanize: Some(HumanizeParams {
                    tremor_base_stddev_px: 0.2,
                    tremor_velocity_scale: 0.5,
                    overshoot_prob: 0.1,
                    overshoot_factor_range: (1.02, 1.05),
                    micro_pause_prob: 0.05,
                    micro_pause_ms_range: (2, 8),
                    seed: Some(42),
                }),
                backend: Backend::Software,
            },
        ),
        (
            "mouse_scroll",
            Action::MouseScroll {
                dy: -120,
                dx: 0,
                at: Some(Point { x: 50, y: 60 }),
                backend: Backend::Software,
            },
        ),
    ])
}

fn pad_actions() -> BTreeMap<&'static str, Action> {
    BTreeMap::from([
        (
            "pad_button",
            Action::PadButton {
                pad: 0,
                button: PadButton::A,
                action: ButtonAction::Press,
                hold_ms: 33,
            },
        ),
        (
            "pad_stick",
            Action::PadStick {
                pad: 0,
                stick: Stick::Left,
                x: 0.5,
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
                    buttons: vec![PadButton::A, PadButton::Start],
                    thumb_l: (0.25, -0.25),
                    thumb_r: (0.0, 0.0),
                    lt: 0.0,
                    rt: 1.0,
                    ..GamepadReport::default()
                },
            },
        ),
    ])
}

fn compound_actions() -> BTreeMap<&'static str, Action> {
    let key_a = named_key("a");
    let element_id = ElementId::parse("0x12ab:0a1b2c3d")
        .unwrap_or_else(|err| panic!("literal element id should parse: {err}"));

    BTreeMap::from([
        (
            "aim_at",
            Action::AimAt {
                target: AimTarget::Element { element_id },
                style: AimStyle::Snap,
                deadline_ms: 60,
                backend: Backend::Software,
            },
        ),
        (
            "combo",
            Action::Combo {
                steps: vec![
                    ComboStep {
                        at_ms: 0,
                        input: ComboInput::KeyDown { key: key_a.clone() },
                    },
                    ComboStep {
                        at_ms: 33,
                        input: ComboInput::KeyUp { key: key_a },
                    },
                ],
                backend: Backend::Software,
            },
        ),
        ("release_all", Action::ReleaseAll),
    ])
}
