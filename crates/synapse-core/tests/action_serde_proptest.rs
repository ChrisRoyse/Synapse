use proptest::{collection::vec, prelude::*};
use synapse_core::Action;

#[path = "action_serde_proptest/helpers.rs"]
mod helpers;
#[path = "action_serde_proptest/strategies.rs"]
mod strategies;

use helpers::run_action_round_trip;
use strategies::{
    aim_curve_strategy, aim_style_strategy, aim_target_strategy, backend_strategy,
    button_action_strategy, combo_step_strategy, coord_strategy, dynamics_strategy,
    gamepad_report_strategy, humanize_params_strategy, key_strategy, mouse_button_strategy,
    mouse_target_strategy, normalized_axis_strategy, pad_button_strategy, path_spec_strategy,
    point_strategy, stick_strategy, stroke_motion_model_strategy, stroke_timing_strategy,
    text_strategy, trigger_strategy, trigger_value_strategy, velocity_profile_strategy,
};

#[test]
fn action_key_press_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "key_press",
        (key_strategy(), 0u32..=30_000, backend_strategy()).prop_map(|(key, hold_ms, backend)| {
            Action::KeyPress {
                key,
                hold_ms,
                backend,
            }
        }),
    )
}

#[test]
fn action_key_down_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "key_down",
        (key_strategy(), backend_strategy())
            .prop_map(|(key, backend)| Action::KeyDown { key, backend }),
    )
}

#[test]
fn action_key_up_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "key_up",
        (key_strategy(), backend_strategy())
            .prop_map(|(key, backend)| Action::KeyUp { key, backend }),
    )
}

#[test]
fn action_key_chord_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "key_chord",
        (
            vec(key_strategy(), 0..=4),
            0u32..=30_000,
            backend_strategy(),
        )
            .prop_map(|(keys, hold_ms, backend)| Action::KeyChord {
                keys,
                hold_ms,
                backend,
            }),
    )
}

#[test]
fn action_type_text_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "type_text",
        (text_strategy(), dynamics_strategy(), backend_strategy()).prop_map(
            |(text, dynamics, backend)| Action::TypeText {
                text,
                dynamics,
                backend,
            },
        ),
    )
}

#[test]
fn action_mouse_move_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "mouse_move",
        (
            mouse_target_strategy(),
            aim_curve_strategy(),
            0u32..=1_000,
            backend_strategy(),
        )
            .prop_map(|(to, curve, duration_ms, backend)| Action::MouseMove {
                to,
                curve,
                duration_ms,
                backend,
            }),
    )
}

#[test]
fn action_mouse_move_relative_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "mouse_move_relative",
        (coord_strategy(), coord_strategy(), backend_strategy())
            .prop_map(|(dx, dy, backend)| Action::MouseMoveRelative { dx, dy, backend }),
    )
}

#[test]
fn action_mouse_button_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "mouse_button",
        (
            mouse_button_strategy(),
            button_action_strategy(),
            0u32..=30_000,
            backend_strategy(),
        )
            .prop_map(|(button, action, hold_ms, backend)| Action::MouseButton {
                button,
                action,
                hold_ms,
                backend,
            }),
    )
}

#[test]
fn action_mouse_drag_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "mouse_drag",
        (
            point_strategy(),
            point_strategy(),
            mouse_button_strategy(),
            aim_curve_strategy(),
            0u32..=1_000,
            backend_strategy(),
        )
            .prop_map(|(from, to, button, curve, duration_ms, backend)| {
                Action::MouseDrag {
                    from,
                    to,
                    button,
                    curve,
                    duration_ms,
                    backend,
                }
            }),
    )
}

#[test]
fn action_mouse_stroke_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "mouse_stroke",
        (
            path_spec_strategy(),
            prop::option::of(mouse_button_strategy()),
            velocity_profile_strategy(),
            stroke_timing_strategy(),
            stroke_motion_model_strategy(),
            prop::option::of(humanize_params_strategy()),
            backend_strategy(),
        )
            .prop_map(
                |(path, button, profile, timing, motion_model, humanize, backend)| {
                    Action::MouseStroke {
                        path,
                        button,
                        profile,
                        timing,
                        motion_model,
                        humanize,
                        backend,
                    }
                },
            ),
    )
}

#[test]
fn action_mouse_scroll_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "mouse_scroll",
        (
            -10_000i32..=10_000,
            -10_000i32..=10_000,
            prop::option::of(point_strategy()),
            backend_strategy(),
        )
            .prop_map(|(dy, dx, at, backend)| Action::MouseScroll {
                dy,
                dx,
                at,
                backend,
            }),
    )
}

#[test]
fn action_pad_button_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "pad_button",
        (
            0u8..=3,
            pad_button_strategy(),
            button_action_strategy(),
            0u32..=30_000,
        )
            .prop_map(|(pad, button, action, hold_ms)| Action::PadButton {
                pad,
                button,
                action,
                hold_ms,
            }),
    )
}

#[test]
fn action_pad_stick_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "pad_stick",
        (
            0u8..=3,
            stick_strategy(),
            normalized_axis_strategy(),
            normalized_axis_strategy(),
        )
            .prop_map(|(pad, stick, x, y)| Action::PadStick { pad, stick, x, y }),
    )
}

#[test]
fn action_pad_trigger_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "pad_trigger",
        (0u8..=3, trigger_strategy(), trigger_value_strategy()).prop_map(
            |(pad, trigger, value)| Action::PadTrigger {
                pad,
                trigger,
                value,
            },
        ),
    )
}

#[test]
fn action_pad_report_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "pad_report",
        (0u8..=3, gamepad_report_strategy())
            .prop_map(|(pad, report)| Action::PadReport { pad, report }),
    )
}

#[test]
fn action_aim_at_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "aim_at",
        (
            aim_target_strategy(),
            aim_style_strategy(),
            0u32..=1_000,
            backend_strategy(),
        )
            .prop_map(|(target, style, deadline_ms, backend)| Action::AimAt {
                target,
                style,
                deadline_ms,
                backend,
            }),
    )
}

#[test]
fn action_combo_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip(
        "combo",
        (vec(combo_step_strategy(), 0..=8), backend_strategy())
            .prop_map(|(steps, backend)| Action::Combo { steps, backend }),
    )
}

#[test]
fn action_release_all_round_trips_1000_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_action_round_trip("release_all", Just(Action::ReleaseAll))
}
