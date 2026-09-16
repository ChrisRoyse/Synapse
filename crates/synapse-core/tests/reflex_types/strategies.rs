use chrono::{DateTime, Utc};
use proptest::prelude::*;
use synapse_core::{
    Action, AimCurve, AimNaturalParams, AimTarget, Backend, ButtonAction, ComboInput, ComboStep,
    DataPredicate, EventFilter, EventSource, HumanizeParams, Key, KeyCode, MouseButton, PadButton,
    PathPoint, PathSpec, Point, ReflexAimAxis, ReflexButtonTarget, ReflexKind, ReflexLifetime,
    ReflexRegistration, ReflexState, ReflexStatus, ReflexThen, StrokeTiming, VelocityProfile,
};

use super::fixtures::fixed_time;

pub fn small_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,8}".prop_map(|value| value)
}

pub fn backend_strategy() -> impl Strategy<Value = Backend> {
    prop_oneof![
        Just(Backend::Software),
        Just(Backend::Vigem),
        Just(Backend::Hardware),
        Just(Backend::Auto),
    ]
}

pub fn key_strategy() -> impl Strategy<Value = Key> {
    prop_oneof![
        small_string().prop_map(|value| Key {
            code: KeyCode::Named { value },
            use_scancode: false,
        }),
        (0_u8..128, any::<bool>()).prop_map(|(value, use_scancode)| Key {
            code: KeyCode::HidCode { value },
            use_scancode,
        }),
    ]
}

pub fn point_strategy() -> impl Strategy<Value = Point> {
    (-10_000_i32..10_000, -10_000_i32..10_000).prop_map(|(x, y)| Point { x, y })
}

pub fn path_point_strategy() -> impl Strategy<Value = PathPoint> {
    (-10_000.0_f64..10_000.0, -10_000.0_f64..10_000.0).prop_map(|(x, y)| PathPoint { x, y })
}

pub fn path_spec_strategy() -> impl Strategy<Value = PathSpec> {
    prop_oneof![
        (path_point_strategy(), path_point_strategy())
            .prop_map(|(from, to)| PathSpec::Line { from, to }),
        (path_point_strategy(), 1.0_f64..500.0)
            .prop_map(|(center, radius)| { PathSpec::Circle { center, radius } }),
        prop::collection::vec(path_point_strategy(), 2..6).prop_map(|points| {
            PathSpec::Polyline {
                points,
                closed: false,
            }
        }),
    ]
}

pub fn aim_target_strategy() -> impl Strategy<Value = AimTarget> {
    prop_oneof![
        point_strategy().prop_map(|point| AimTarget::Screen { point }),
        (1_u64..10_000).prop_map(|track_id| AimTarget::Track { track_id }),
    ]
}

pub fn aim_curve_strategy() -> impl Strategy<Value = AimCurve> {
    prop_oneof![
        Just(AimCurve::Instant),
        Just(AimCurve::Linear),
        (0.0_f32..1.0, 0.0_f32..1.0, 0.0_f32..1.0, 0.0_f32..1.0).prop_map(
            |(p1x, p1y, p2x, p2y)| AimCurve::Bezier {
                p1: (p1x, p1y),
                p2: (p2x, p2y),
            },
        ),
        Just(AimCurve::Natural {
            params: AimNaturalParams::FAST,
        }),
    ]
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
        (1_u32..5_000).prop_map(|duration_ms| StrokeTiming::DurationMs { duration_ms }),
        (1.0_f64..10_000.0).prop_map(|px_per_sec| StrokeTiming::SpeedPxPerSec { px_per_sec }),
    ]
}

pub fn humanize_params_strategy() -> impl Strategy<Value = HumanizeParams> {
    (
        0.0_f32..3.0,
        0.0_f32..3.0,
        0.0_f32..1.0,
        1.0_f32..1.5,
        1.5_f32..2.0,
        0.0_f32..1.0,
        0_u32..20,
        20_u32..80,
    )
        .prop_map(
            |(
                tremor_base_stddev_px,
                tremor_velocity_scale,
                overshoot_prob,
                overshoot_min,
                overshoot_max,
                micro_pause_prob,
                pause_min,
                pause_max,
            )| HumanizeParams {
                tremor_base_stddev_px,
                tremor_velocity_scale,
                overshoot_prob,
                overshoot_factor_range: (overshoot_min, overshoot_max),
                micro_pause_prob,
                micro_pause_ms_range: (pause_min, pause_max),
                seed: Some(42),
            },
        )
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
    ]
}

pub fn reflex_aim_axis_strategy() -> impl Strategy<Value = ReflexAimAxis> {
    prop_oneof![
        Just(ReflexAimAxis::Xy),
        Just(ReflexAimAxis::XOnly),
        Just(ReflexAimAxis::YOnly),
    ]
}

pub fn reflex_button_target_strategy() -> impl Strategy<Value = ReflexButtonTarget> {
    prop_oneof![
        mouse_button_strategy().prop_map(|button| ReflexButtonTarget::Mouse { button }),
        (0_u8..4, pad_button_strategy())
            .prop_map(|(pad, button)| ReflexButtonTarget::Pad { pad, button }),
    ]
}

pub fn combo_input_strategy() -> impl Strategy<Value = ComboInput> {
    prop_oneof![
        key_strategy().prop_map(|key| ComboInput::KeyDown { key }),
        key_strategy().prop_map(|key| ComboInput::KeyUp { key }),
        (key_strategy(), 0_u16..250)
            .prop_map(|(key, hold_ms)| ComboInput::KeyPress { key, hold_ms }),
        (mouse_button_strategy(), button_action_strategy())
            .prop_map(|(button, action)| { ComboInput::MouseButton { button, action } }),
        (-100.0_f32..100.0, -100.0_f32..100.0)
            .prop_map(|(dx, dy)| ComboInput::MouseMoveRel { dx, dy }),
    ]
}

pub fn combo_step_strategy() -> impl Strategy<Value = ComboStep> {
    (0_u32..5_000, combo_input_strategy()).prop_map(|(at_ms, input)| ComboStep { at_ms, input })
}

pub fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        (key_strategy(), 0_u32..250, backend_strategy()).prop_map(|(key, hold_ms, backend)| {
            Action::KeyPress {
                key,
                hold_ms,
                backend,
            }
        }),
        (key_strategy(), backend_strategy())
            .prop_map(|(key, backend)| Action::KeyDown { key, backend }),
        (
            mouse_button_strategy(),
            button_action_strategy(),
            0_u32..250,
            backend_strategy()
        )
            .prop_map(|(button, action, hold_ms, backend)| Action::MouseButton {
                button,
                action,
                hold_ms,
                backend,
            }),
        (-500.0_f32..500.0, -500.0_f32..500.0, backend_strategy())
            .prop_map(|(dx, dy, backend)| Action::MouseMoveRelative { dx, dy, backend },),
    ]
}

pub fn event_filter_strategy() -> impl Strategy<Value = EventFilter> {
    prop_oneof![
        Just(EventFilter::All),
        Just(EventFilter::None),
        small_string().prop_map(|kind| EventFilter::Kind { kind }),
        Just(EventFilter::Source {
            source: EventSource::Reflex,
        }),
        small_string().prop_map(|kind| EventFilter::Not {
            arg: Box::new(EventFilter::Kind { kind }),
        }),
        (small_string(), small_string()).prop_map(|(path, value)| EventFilter::Data {
            path: format!("/{path}"),
            predicate: DataPredicate::Eq {
                value: serde_json::json!(value),
            },
        }),
    ]
}

pub fn reflex_then_strategy() -> impl Strategy<Value = ReflexThen> {
    prop_oneof![
        action_strategy().prop_map(|action| ReflexThen::Action { action }),
        prop::collection::vec(action_strategy(), 0..4)
            .prop_map(|actions| ReflexThen::Actions { actions }),
        (
            prop::collection::vec(combo_step_strategy(), 0..4),
            backend_strategy(),
        )
            .prop_map(|(steps, backend)| ReflexThen::Combo { steps, backend }),
    ]
}

pub fn reflex_lifetime_strategy() -> impl Strategy<Value = ReflexLifetime> {
    prop_oneof![
        Just(ReflexLifetime::UntilCancelled),
        Just(ReflexLifetime::OneShot),
        (0_u32..60_000).prop_map(|ms| ReflexLifetime::Duration { ms }),
        event_filter_strategy().prop_map(|filter| ReflexLifetime::UntilEvent { filter }),
        (0_u32..3_600_000).prop_map(|ms| ReflexLifetime::UntilDeadline { ms }),
    ]
}

pub fn reflex_kind_strategy() -> impl Strategy<Value = ReflexKind> {
    prop_oneof![
        (
            aim_target_strategy(),
            reflex_aim_axis_strategy(),
            0.0_f32..2.0,
            0.0_f32..50.0,
            1.0_f32..5_000.0,
            aim_curve_strategy(),
            backend_strategy(),
        )
            .prop_map(
                |(
                    target,
                    axis,
                    gain,
                    deadzone_px,
                    max_speed_px_per_ms,
                    curve_per_step,
                    backend,
                )| ReflexKind::AimTrack {
                    target,
                    axis,
                    gain,
                    deadzone_px,
                    max_speed_px_per_ms,
                    curve_per_step,
                    backend,
                },
            ),
        (
            prop::collection::vec(key_strategy(), 0..4),
            backend_strategy(),
            any::<bool>(),
        )
            .prop_map(|(keys, backend, re_assert)| ReflexKind::HoldMove {
                keys,
                backend,
                re_assert,
            }),
        (reflex_button_target_strategy(), backend_strategy())
            .prop_map(|(button, backend)| { ReflexKind::HoldButton { button, backend } }),
        (
            prop::collection::vec(combo_step_strategy(), 0..4),
            backend_strategy(),
        )
            .prop_map(|(steps, backend)| ReflexKind::Combo { steps, backend }),
        (
            path_spec_strategy(),
            prop::option::of(mouse_button_strategy()),
            velocity_profile_strategy(),
            stroke_timing_strategy(),
            prop::option::of(humanize_params_strategy()),
            backend_strategy(),
        )
            .prop_map(|(path, button, profile, timing, humanize, backend)| {
                ReflexKind::PathFollow {
                    path,
                    button,
                    profile,
                    timing,
                    humanize,
                    backend,
                }
            },),
        (
            event_filter_strategy(),
            reflex_then_strategy(),
            0_u32..10_000
        )
            .prop_map(|(when, then, debounce_ms)| ReflexKind::OnEvent {
                when,
                then,
                debounce_ms,
            },),
    ]
}

pub fn reflex_registration_strategy() -> impl Strategy<Value = ReflexRegistration> {
    (
        small_string(),
        reflex_kind_strategy(),
        0_u32..1_001,
        reflex_lifetime_strategy(),
        any::<bool>(),
    )
        .prop_map(
            |(id, kind, priority, lifetime, exclusive)| ReflexRegistration {
                id,
                kind,
                priority,
                lifetime,
                exclusive,
            },
        )
}

pub fn reflex_state_strategy() -> impl Strategy<Value = ReflexState> {
    prop_oneof![
        Just(ReflexState::Active),
        Just(ReflexState::ActionDenied),
        Just(ReflexState::Paused),
        Just(ReflexState::Cancelled),
        Just(ReflexState::Expired),
        Just(ReflexState::Disabled),
        Just(ReflexState::Starved),
    ]
}

pub fn instant_strategy() -> impl Strategy<Value = DateTime<Utc>> {
    (0_i64..86_400).prop_map(fixed_time)
}

pub fn reflex_status_strategy() -> impl Strategy<Value = ReflexStatus> {
    (
        small_string(),
        small_string(),
        reflex_state_strategy(),
        instant_strategy(),
        prop::option::of(instant_strategy()),
        0_u64..10_000,
        0_u32..1_001,
        reflex_lifetime_strategy(),
        any::<bool>(),
        prop::option::of(small_string()),
    )
        .prop_map(
            |(
                id,
                kind_summary,
                state,
                registered_at,
                last_fired_at,
                fire_count,
                priority,
                lifetime,
                exclusive,
                last_error_code,
            )| ReflexStatus {
                id,
                kind_summary,
                state,
                registered_at,
                last_fired_at,
                fire_count,
                priority,
                lifetime,
                exclusive,
                last_error_code,
            },
        )
}
