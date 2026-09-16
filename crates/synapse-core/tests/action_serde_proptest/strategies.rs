use proptest::{collection::vec, prelude::*};
use synapse_core::{
    AimCurve, AimNaturalParams, AimStyle, AimTarget, Backend, ButtonAction, ComboInput, ComboStep,
    ElementId, GamepadReport, HumanizeParams, Key, KeyCode, KeystrokeDynamics,
    KeystrokeNaturalParams, MouseButton, MouseTarget, PadButton, PathPoint, PathSpec, Point, Stick,
    StrokeMotionModel, StrokeTiming, Trigger, VelocityProfile,
};

pub fn backend_strategy() -> impl Strategy<Value = Backend> {
    prop_oneof![
        Just(Backend::Software),
        Just(Backend::Vigem),
        Just(Backend::Hardware),
        Just(Backend::Auto),
    ]
}

pub fn key_strategy() -> impl Strategy<Value = Key> {
    (key_code_strategy(), any::<bool>()).prop_map(|(code, use_scancode)| Key { code, use_scancode })
}

pub fn key_code_strategy() -> impl Strategy<Value = KeyCode> {
    prop_oneof![
        short_ascii_string_strategy().prop_map(|value| KeyCode::Named { value }),
        prop_oneof![Just('@'), Just(' '), Just('\n')].prop_map(|value| KeyCode::Symbol { value }),
        any::<u8>().prop_map(|value| KeyCode::HidCode { value }),
    ]
}

pub fn short_ascii_string_strategy() -> impl Strategy<Value = String> {
    vec(0u8..=25, 1..=16).prop_map(|bytes| {
        bytes
            .into_iter()
            .map(|byte| char::from(b'a' + byte))
            .collect()
    })
}

pub fn text_strategy() -> impl Strategy<Value = String> {
    vec(
        prop_oneof![Just('a'), Just('Z'), Just('0'), Just(' '), Just('\n')],
        0..=64,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

pub fn dynamics_strategy() -> impl Strategy<Value = KeystrokeDynamics> {
    prop_oneof![
        Just(KeystrokeDynamics::Burst),
        (0u32..=1000).prop_map(|ms_per_char| KeystrokeDynamics::Linear { ms_per_char }),
        (0.0f32..128.0, 0.0f32..64.0, any::<bool>()).prop_map(
            |(mean_iki_ms, stddev_ms, bigram_bias)| KeystrokeDynamics::Natural {
                params: KeystrokeNaturalParams {
                    mean_iki_ms,
                    stddev_ms,
                    bigram_bias,
                },
            },
        ),
    ]
}

pub fn point_strategy() -> impl Strategy<Value = Point> {
    (-16_384i32..=16_384, -16_384i32..=16_384).prop_map(|(x, y)| Point { x, y })
}

pub fn path_point_strategy() -> impl Strategy<Value = PathPoint> {
    (-16_384i32..=16_384, -16_384i32..=16_384)
        .prop_map(|(x, y)| PathPoint::new(f64::from(x), f64::from(y)))
}

pub fn coord_strategy() -> impl Strategy<Value = f32> {
    -4096.0f32..4096.0
}

pub fn normalized_axis_strategy() -> impl Strategy<Value = f32> {
    -1.0f32..1.0
}

pub fn trigger_value_strategy() -> impl Strategy<Value = f32> {
    0.0f32..1.0
}

pub fn element_id_strategy() -> impl Strategy<Value = ElementId> {
    (0u16..=u16::MAX, 1u16..=u16::MAX).prop_map(|(hwnd, runtime)| {
        ElementId::parse(&format!("0x{hwnd:x}:{runtime:x}"))
            .unwrap_or_else(|err| panic!("generated element id should parse: {err}"))
    })
}

pub fn mouse_target_strategy() -> impl Strategy<Value = MouseTarget> {
    prop_oneof![
        point_strategy().prop_map(|point| MouseTarget::Screen { point }),
        element_id_strategy().prop_map(|element_id| MouseTarget::Element { element_id }),
    ]
}

pub fn aim_target_strategy() -> impl Strategy<Value = AimTarget> {
    prop_oneof![
        point_strategy().prop_map(|point| AimTarget::Screen { point }),
        element_id_strategy().prop_map(|element_id| AimTarget::Element { element_id }),
        any::<u64>().prop_map(|track_id| AimTarget::Track { track_id }),
    ]
}

pub fn aim_curve_strategy() -> impl Strategy<Value = AimCurve> {
    prop_oneof![
        Just(AimCurve::Instant),
        Just(AimCurve::Linear),
        Just(AimCurve::EaseInOut),
        ((0.0f32..1.0, 0.0f32..1.0), (0.0f32..1.0, 0.0f32..1.0))
            .prop_map(|(p1, p2)| AimCurve::Bezier { p1, p2 }),
        (
            0.0f32..0.5,
            0.0f32..1.0,
            0.0f32..1.0,
            (1.0f32..1.5, 1.5f32..2.0),
            0u8..=4,
            0.0f32..10.0,
            prop::option::of(any::<u64>()),
        )
            .prop_map(
                |(
                    control_point_jitter,
                    tremor_stddev_px,
                    overshoot_prob,
                    overshoot_factor_range,
                    micro_correct_steps,
                    timing_stddev_ms,
                    seed,
                )| AimCurve::Natural {
                    params: AimNaturalParams {
                        control_point_jitter,
                        tremor_stddev_px,
                        overshoot_prob,
                        overshoot_factor_range,
                        micro_correct_steps,
                        timing_stddev_ms,
                        seed,
                    },
                },
            ),
    ]
}

pub fn path_spec_strategy() -> impl Strategy<Value = PathSpec> {
    let line = (path_point_strategy(), path_point_strategy())
        .prop_map(|(from, to)| PathSpec::Line { from, to });
    let arc = (path_point_strategy(), 1u32..=2048, -6i32..=6, -6i32..=6).prop_map(
        |(center, radius, start_angle_rad, sweep_angle_rad)| PathSpec::Arc {
            center,
            radius: f64::from(radius),
            start_angle_rad: f64::from(start_angle_rad),
            sweep_angle_rad: f64::from(sweep_angle_rad),
        },
    );
    let circle =
        (path_point_strategy(), 1u32..=2048).prop_map(|(center, radius)| PathSpec::Circle {
            center,
            radius: f64::from(radius),
        });
    let cubic = (
        path_point_strategy(),
        path_point_strategy(),
        path_point_strategy(),
        path_point_strategy(),
    )
        .prop_map(|(p0, p1, p2, p3)| PathSpec::CubicBezier { p0, p1, p2, p3 });
    let polyline = (vec(path_point_strategy(), 2..=6), any::<bool>())
        .prop_map(|(points, closed)| PathSpec::Polyline { points, closed });
    let catmull = (
        vec(path_point_strategy(), 4..=7),
        prop_oneof![Just(0.0), Just(0.5), Just(1.0)],
        prop_oneof![Just(0.0), Just(0.5), Just(1.0)],
        any::<bool>(),
    )
        .prop_map(|(waypoints, alpha, tension, closed)| PathSpec::CatmullRom {
            waypoints,
            alpha,
            tension,
            closed,
        });

    prop_oneof![line, arc, circle, cubic, polyline, catmull]
}

pub fn velocity_profile_strategy() -> impl Strategy<Value = VelocityProfile> {
    prop_oneof![
        Just(VelocityProfile::Constant),
        Just(VelocityProfile::Linear),
        Just(VelocityProfile::EaseInOut),
        Just(VelocityProfile::MinimumJerk),
    ]
}

pub fn stroke_timing_strategy() -> impl Strategy<Value = StrokeTiming> {
    prop_oneof![
        (1u32..=30_000).prop_map(|duration_ms| StrokeTiming::DurationMs { duration_ms }),
        (1u32..=10_000).prop_map(|px_per_sec| StrokeTiming::SpeedPxPerSec {
            px_per_sec: f64::from(px_per_sec),
        }),
    ]
}

pub fn stroke_motion_model_strategy() -> impl Strategy<Value = StrokeMotionModel> {
    prop_oneof![
        Just(StrokeMotionModel::Path),
        (
            1u32..=20,
            1u32..=20,
            1u32..=30,
            1u32..=128,
            prop::option::of(any::<u64>()),
        )
            .prop_map(|(gravity, wind, max_step, damped_distance, seed)| {
                StrokeMotionModel::WindMouse {
                    gravity: f64::from(gravity),
                    wind: f64::from(wind),
                    max_step: f64::from(max_step),
                    damped_distance: f64::from(damped_distance),
                    seed,
                }
            }),
    ]
}

pub fn humanize_params_strategy() -> impl Strategy<Value = HumanizeParams> {
    (
        0.0f32..=4.0,
        0.0f32..=4.0,
        0.0f32..=1.0,
        (1.0f32..=1.1, 1.1f32..=1.3),
        0.0f32..=1.0,
        (0u32..=10, 11u32..=50),
        prop::option::of(any::<u64>()),
    )
        .prop_map(
            |(
                tremor_base_stddev_px,
                tremor_velocity_scale,
                overshoot_prob,
                overshoot_factor_range,
                micro_pause_prob,
                micro_pause_ms_range,
                seed,
            )| HumanizeParams {
                tremor_base_stddev_px,
                tremor_velocity_scale,
                overshoot_prob,
                overshoot_factor_range,
                micro_pause_prob,
                micro_pause_ms_range,
                seed,
            },
        )
}

pub fn mouse_button_strategy() -> impl Strategy<Value = MouseButton> {
    prop_oneof![
        Just(MouseButton::Left),
        Just(MouseButton::Right),
        Just(MouseButton::Middle),
        Just(MouseButton::X1),
        Just(MouseButton::X2),
    ]
}

pub fn button_action_strategy() -> impl Strategy<Value = ButtonAction> {
    prop_oneof![
        Just(ButtonAction::Press),
        Just(ButtonAction::Down),
        Just(ButtonAction::Up),
    ]
}

pub fn pad_button_strategy() -> impl Strategy<Value = PadButton> {
    prop_oneof![
        Just(PadButton::A),
        Just(PadButton::B),
        Just(PadButton::X),
        Just(PadButton::Y),
        Just(PadButton::Lb),
        Just(PadButton::Rb),
        Just(PadButton::Ls),
        Just(PadButton::Rs),
        Just(PadButton::Back),
        Just(PadButton::Start),
        Just(PadButton::Up),
        Just(PadButton::Down),
        Just(PadButton::Left),
        Just(PadButton::Right),
        Just(PadButton::Guide),
    ]
}

pub fn stick_strategy() -> impl Strategy<Value = Stick> {
    prop_oneof![Just(Stick::Left), Just(Stick::Right)]
}

pub fn trigger_strategy() -> impl Strategy<Value = Trigger> {
    prop_oneof![Just(Trigger::Left), Just(Trigger::Right)]
}

pub fn aim_style_strategy() -> impl Strategy<Value = AimStyle> {
    prop_oneof![
        Just(AimStyle::Snap),
        Just(AimStyle::Flick),
        Just(AimStyle::Natural),
        Just(AimStyle::Track),
    ]
}

pub fn gamepad_report_strategy() -> impl Strategy<Value = GamepadReport> {
    (
        vec(pad_button_strategy(), 0..=8),
        normalized_axis_strategy(),
        normalized_axis_strategy(),
        normalized_axis_strategy(),
        normalized_axis_strategy(),
        trigger_value_strategy(),
        trigger_value_strategy(),
    )
        .prop_map(
            |(buttons, thumb_l_x, thumb_l_y, thumb_r_x, thumb_r_y, lt, rt)| GamepadReport {
                buttons,
                thumb_l: (thumb_l_x, thumb_l_y),
                thumb_r: (thumb_r_x, thumb_r_y),
                lt,
                rt,
                ..GamepadReport::default()
            },
        )
}

pub fn combo_step_strategy() -> impl Strategy<Value = ComboStep> {
    (0u32..=1_000, combo_input_strategy()).prop_map(|(at_ms, input)| ComboStep { at_ms, input })
}

pub fn combo_input_strategy() -> impl Strategy<Value = ComboInput> {
    prop_oneof![
        key_strategy().prop_map(|key| ComboInput::KeyDown { key }),
        key_strategy().prop_map(|key| ComboInput::KeyUp { key }),
        (key_strategy(), 0u16..=30_000)
            .prop_map(|(key, hold_ms)| ComboInput::KeyPress { key, hold_ms }),
        (mouse_button_strategy(), button_action_strategy())
            .prop_map(|(button, action)| { ComboInput::MouseButton { button, action } }),
        (coord_strategy(), coord_strategy())
            .prop_map(|(dx, dy)| ComboInput::MouseMoveRel { dx, dy }),
        (0u8..=3, pad_button_strategy(), button_action_strategy()).prop_map(
            |(pad, button, action)| ComboInput::PadButton {
                pad,
                button,
                action,
            },
        ),
        (
            0u8..=3,
            stick_strategy(),
            normalized_axis_strategy(),
            normalized_axis_strategy(),
        )
            .prop_map(|(pad, stick, x, y)| ComboInput::PadStick { pad, stick, x, y }),
    ]
}
