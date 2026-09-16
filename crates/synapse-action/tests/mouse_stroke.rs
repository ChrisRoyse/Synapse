use synapse_action::{
    ActionBackend, EmitState, RecordedInput, RecordingBackend, StrokeError, plan_timed_stroke,
};
use synapse_core::{
    Action, Backend, HumanizeParams, MouseButton, PathPoint, PathSpec, StrokeMotionModel,
    StrokeTiming, VelocityProfile, error_codes,
};

#[test]
fn stroke_planner_duration_and_speed_keep_tick_count_and_monotonic_time() {
    let path = line_path(0.0, 0.0, 4.0, 0.0);
    let duration_plan = plan_timed_stroke(
        &path,
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 4 },
        StrokeMotionModel::Path,
        None,
    )
    .expect("duration stroke should plan");
    let speed_plan = plan_timed_stroke(
        &path,
        VelocityProfile::Constant,
        &StrokeTiming::SpeedPxPerSec { px_per_sec: 1000.0 },
        StrokeMotionModel::Path,
        None,
    )
    .expect("speed stroke should plan");

    println!(
        "readback=mouse_stroke_plan edge=duration_speed before=path:{path:?} after_duration={:?} after_speed={:?}",
        duration_plan.samples, speed_plan.samples
    );
    assert_eq!(duration_plan.samples, speed_plan.samples);
    assert_eq!(duration_plan.samples.len(), 5);
    assert!((duration_plan.duration_ms - 4.0).abs() < f64::EPSILON);
    assert!(monotonic_elapsed(&duration_plan.samples));
    assert_eq!(duration_plan.samples[0].point, PathPoint::new(0.0, 0.0));
    assert_eq!(duration_plan.samples[4].point, PathPoint::new(4.0, 0.0));
}

#[test]
fn stroke_planner_rejects_invalid_speed_and_humanize() {
    let path = line_path(0.0, 0.0, 4.0, 0.0);
    let speed_error = plan_timed_stroke(
        &path,
        VelocityProfile::Linear,
        &StrokeTiming::SpeedPxPerSec { px_per_sec: 0.0 },
        StrokeMotionModel::Path,
        None,
    )
    .expect_err("zero speed must fail closed");
    let humanize_error = plan_timed_stroke(
        &path,
        VelocityProfile::Linear,
        &StrokeTiming::DurationMs { duration_ms: 4 },
        StrokeMotionModel::Path,
        Some(HumanizeParams {
            tremor_base_stddev_px: 0.0,
            tremor_velocity_scale: 0.0,
            overshoot_prob: 2.0,
            overshoot_factor_range: (1.03, 1.12),
            micro_pause_prob: 0.0,
            micro_pause_ms_range: (15, 40),
            seed: None,
        }),
    )
    .expect_err("invalid humanize probability must fail closed");

    println!(
        "readback=mouse_stroke_plan edge=invalid before=speed:0 overshoot_prob:2 after_speed={speed_error:?} after_humanize={humanize_error:?}"
    );
    assert!(matches!(speed_error, StrokeError::InvalidSpeed { .. }));
    assert!(matches!(humanize_error, StrokeError::Humanize(_)));
}

#[test]
fn wind_mouse_motion_model_is_seeded_curved_variable_and_converges() {
    let path = line_path(0.0, 0.0, 120.0, 0.0);
    let model = StrokeMotionModel::WindMouse {
        gravity: 9.0,
        wind: 3.0,
        max_step: 10.0,
        damped_distance: 12.0,
        seed: Some(42),
    };
    let first = plan_timed_stroke(
        &path,
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 120 },
        model,
        None,
    )
    .expect("seeded wind_mouse stroke should plan");
    let second = plan_timed_stroke(
        &path,
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 120 },
        model,
        None,
    )
    .expect("same seeded wind_mouse stroke should plan");
    let different_seed = plan_timed_stroke(
        &path,
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 120 },
        StrokeMotionModel::WindMouse {
            gravity: 9.0,
            wind: 3.0,
            max_step: 10.0,
            damped_distance: 12.0,
            seed: Some(43),
        },
        None,
    )
    .expect("different seeded wind_mouse stroke should plan");

    let step_lengths = segment_lengths(&first.samples);
    let min_step = step_lengths.iter().copied().fold(f64::INFINITY, f64::min);
    let max_step = step_lengths.iter().copied().fold(0.0_f64, f64::max);
    let max_abs_y = first
        .samples
        .iter()
        .map(|sample| sample.point.y.abs())
        .fold(0.0_f64, f64::max);
    println!(
        "readback=mouse_stroke_plan edge=wind_mouse before=line:(0,0)->(120,0),seed:42 after_points={} after_min_step={min_step:.3} after_max_step={max_step:.3} after_max_abs_y={max_abs_y:.3} result_value=final:{:?}",
        first.samples.len(),
        first.samples.last().map(|sample| sample.point)
    );

    assert_eq!(first.samples, second.samples);
    assert_ne!(first.samples, different_seed.samples);
    assert_eq!(
        first.samples.last().map(|sample| sample.point),
        Some(PathPoint::new(120.0, 0.0))
    );
    assert!(max_abs_y > 1.0, "wind_mouse path should not stay straight");
    assert!(
        max_step - min_step > 1.0,
        "wind_mouse consecutive step lengths should vary: {step_lengths:?}"
    );
    assert!(monotonic_elapsed(&first.samples));
}

#[test]
fn wind_mouse_screen_space_diagonal_stops_at_target_zone() {
    let path = line_path(2200.0, 320.0, 2320.0, 340.0);
    let model = StrokeMotionModel::WindMouse {
        gravity: 9.0,
        wind: 3.0,
        max_step: 12.0,
        damped_distance: 50.0,
        seed: Some(42),
    };

    let plan = plan_timed_stroke(
        &path,
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 120 },
        model,
        None,
    )
    .expect("screen-space wind_mouse stroke should converge without exhausting the point cap");

    let arclen = plan.samples.last().map_or(f64::NAN, |sample| sample.arclen);
    println!(
        "readback=mouse_stroke_plan edge=wind_mouse_screen_diagonal before=line:(2200,320)->(2320,340),seed:42,damped:50 after_points={} after_last_arclen={arclen:.3} result_value=final:{:?}",
        plan.samples.len(),
        plan.samples.last().map(|sample| sample.point)
    );

    assert!(
        plan.samples.len() < 256,
        "wind_mouse must not exhaust the point cap for bounded screen-space strokes"
    );
    assert_eq!(
        plan.samples.last().map(|sample| sample.point),
        Some(PathPoint::new(2320.0, 340.0))
    );
    assert!(monotonic_elapsed(&plan.samples));
}

#[test]
fn wind_mouse_motion_model_rejects_invalid_edges() {
    let line = line_path(0.0, 0.0, 120.0, 0.0);
    let bad_param = plan_timed_stroke(
        &line,
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 120 },
        StrokeMotionModel::WindMouse {
            gravity: 9.0,
            wind: 3.0,
            max_step: 0.0,
            damped_distance: 12.0,
            seed: Some(42),
        },
        None,
    )
    .expect_err("wind_mouse max_step=0 should fail closed");
    let bad_path = plan_timed_stroke(
        &PathSpec::Circle {
            center: PathPoint::new(0.0, 0.0),
            radius: 10.0,
        },
        VelocityProfile::Constant,
        &StrokeTiming::DurationMs { duration_ms: 120 },
        StrokeMotionModel::WindMouse {
            gravity: 9.0,
            wind: 3.0,
            max_step: 10.0,
            damped_distance: 12.0,
            seed: Some(42),
        },
        None,
    )
    .expect_err("wind_mouse on non-line path should fail closed");
    println!(
        "readback=mouse_stroke_plan edge=wind_mouse_invalid after_bad_param={bad_param:?} after_bad_path={bad_path:?}"
    );

    assert!(matches!(
        bad_param,
        StrokeError::InvalidWindMouseParameter {
            field: "max_step",
            ..
        }
    ));
    assert!(matches!(
        bad_path,
        StrokeError::WindMouseRequiresLine {
            path_kind: "circle"
        }
    ));
}

#[test]
fn recording_backend_expands_stroke_to_single_down_stream_up() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();
    let action = Action::MouseStroke {
        path: line_path(0.0, 0.0, 4.0, 0.0),
        button: Some(MouseButton::Left),
        profile: VelocityProfile::Constant,
        timing: StrokeTiming::DurationMs { duration_ms: 4 },
        motion_model: StrokeMotionModel::Path,
        humanize: None,
        backend: Backend::Software,
    };
    let before = backend.events();
    println!("readback=mouse_stroke_record edge=button before_events={before:?}");

    backend
        .execute(&action, &mut emit_state)
        .expect("recording backend should expand valid stroke");

    let after = backend.events();
    let stroke_points: Vec<_> = after
        .iter()
        .filter_map(|event| match event {
            RecordedInput::MouseStrokePoint { elapsed_ms, point } => Some((*elapsed_ms, *point)),
            _ => None,
        })
        .collect();
    println!(
        "readback=mouse_stroke_record edge=button after_events={after:?} result_value=points:{}",
        stroke_points.len()
    );

    assert!(matches!(
        after.first(),
        Some(RecordedInput::MouseButtonDown {
            button: MouseButton::Left
        })
    ));
    assert!(matches!(
        after.last(),
        Some(RecordedInput::MouseButtonUp {
            button: MouseButton::Left
        })
    ));
    assert_eq!(stroke_points.len(), 5);
    assert!(stroke_points.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    assert!(backend.held_buttons().is_empty());
    assert!(emit_state.snapshot().held_buttons.is_empty());
}

#[test]
fn recording_backend_hover_stroke_has_no_button_events() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();
    let action = Action::MouseStroke {
        path: line_path(0.0, 0.0, 2.0, 0.0),
        button: None,
        profile: VelocityProfile::Constant,
        timing: StrokeTiming::DurationMs { duration_ms: 2 },
        motion_model: StrokeMotionModel::Path,
        humanize: None,
        backend: Backend::Software,
    };

    backend
        .execute(&action, &mut emit_state)
        .expect("recording backend should expand valid hover stroke");
    let after = backend.events();
    println!("readback=mouse_stroke_record edge=hover after_events={after:?}");

    assert_eq!(after.len(), 3);
    assert!(
        after
            .iter()
            .all(|event| matches!(event, RecordedInput::MouseStrokePoint { .. }))
    );
    assert!(backend.held_buttons().is_empty());
}

#[test]
fn recording_backend_invalid_stroke_does_not_mutate_events() {
    let backend = RecordingBackend::new();
    let mut emit_state = EmitState::new();
    let action = Action::MouseStroke {
        path: line_path(5.0, 5.0, 5.0, 5.0),
        button: Some(MouseButton::Left),
        profile: VelocityProfile::Constant,
        timing: StrokeTiming::DurationMs { duration_ms: 4 },
        motion_model: StrokeMotionModel::Path,
        humanize: None,
        backend: Backend::Software,
    };
    let before = backend.events();
    println!("readback=mouse_stroke_record edge=invalid before_events={before:?}");

    let error = backend
        .execute(&action, &mut emit_state)
        .expect_err("degenerate stroke path must fail closed");
    let after = backend.events();
    println!(
        "readback=mouse_stroke_record edge=invalid after_events={after:?} data.code={} result_value=events:{}",
        error.code(),
        after.len()
    );

    assert_eq!(error.code(), error_codes::ACTION_TARGET_INVALID);
    assert!(after.is_empty());
    assert!(backend.held_buttons().is_empty());
    assert!(emit_state.snapshot().held_buttons.is_empty());
}

const fn line_path(x0: f64, y0: f64, x1: f64, y1: f64) -> PathSpec {
    PathSpec::Line {
        from: PathPoint::new(x0, y0),
        to: PathPoint::new(x1, y1),
    }
}

fn monotonic_elapsed(samples: &[synapse_action::TimedPathPoint]) -> bool {
    samples
        .windows(2)
        .all(|pair| pair[0].elapsed_ms <= pair[1].elapsed_ms)
}

fn segment_lengths(samples: &[synapse_action::TimedPathPoint]) -> Vec<f64> {
    samples
        .windows(2)
        .map(|pair| pair[0].point.distance_to(pair[1].point))
        .collect()
}
