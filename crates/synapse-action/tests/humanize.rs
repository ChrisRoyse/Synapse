use synapse_action::{HumanizeError, TimedPathPoint, humanize_timed_path};
use synapse_core::{HumanizeParams, PathPoint};

#[test]
fn disabled_humanization_preserves_exact_timed_path() -> Result<(), Box<dyn std::error::Error>> {
    let base = base_timed_path();
    let no_params = humanize_timed_path(&base, None)?;
    let zero_params = humanize_timed_path(
        &base,
        Some(HumanizeParams {
            tremor_base_stddev_px: 0.0,
            tremor_velocity_scale: 10.0,
            overshoot_prob: 0.0,
            overshoot_factor_range: (1.03, 1.12),
            micro_pause_prob: 0.0,
            micro_pause_ms_range: (15, 40),
            seed: Some(42),
        }),
    )?;

    println!(
        "readback=humanize edge=disabled before={base:?} after_none={no_params:?} after_zero={zero_params:?}"
    );
    assert_eq!(no_params, base);
    assert_eq!(zero_params, base);
    Ok(())
}

#[test]
fn seeded_humanization_is_byte_stable_and_adds_correction() -> Result<(), Box<dyn std::error::Error>>
{
    let base = base_timed_path();
    let params = HumanizeParams {
        tremor_base_stddev_px: 0.25,
        tremor_velocity_scale: 2.0,
        overshoot_prob: 1.0,
        overshoot_factor_range: (1.10, 1.10),
        micro_pause_prob: 1.0,
        micro_pause_ms_range: (15, 15),
        seed: Some(42),
    };

    let first = humanize_timed_path(&base, Some(params))?;
    let second = humanize_timed_path(&base, Some(params))?;
    let rendered_first = format!("{first:?}");
    let rendered_second = format!("{second:?}");

    println!(
        "readback=humanize edge=seeded before=params:{params:?} after={rendered_first} result_value=len:{} deterministic:{}",
        first.len(),
        rendered_first == rendered_second
    );
    assert_eq!(rendered_first.as_bytes(), rendered_second.as_bytes());
    assert_eq!(
        first.last().map(|sample| sample.point),
        base.last().map(|sample| sample.point)
    );
    assert!(first.len() > base.len());
    assert!(timestamps_are_monotonic(&first));

    let max_x = first
        .iter()
        .map(|sample| sample.point.x)
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(max_x > base.last().expect("base has endpoint").point.x);
    Ok(())
}

#[test]
fn tremor_amplitude_is_larger_at_lower_velocity() -> Result<(), Box<dyn std::error::Error>> {
    let base = vec![
        timed(0.0, 0.0, 0.0, 0.0),
        timed(100.0, 1.0, 1.0, 0.0),
        timed(101.0, 101.0, 101.0, 0.0),
        timed(102.0, 201.0, 201.0, 0.0),
    ];
    let params = HumanizeParams {
        tremor_base_stddev_px: 1.0,
        tremor_velocity_scale: 25.0,
        overshoot_prob: 0.0,
        overshoot_factor_range: (1.03, 1.12),
        micro_pause_prob: 0.0,
        micro_pause_ms_range: (15, 40),
        seed: Some(11),
    };

    let humanized = humanize_timed_path(&base, Some(params))?;
    let low_velocity_delta = distance(base[1].point, humanized[1].point);
    let high_velocity_delta = distance(base[2].point, humanized[2].point);

    println!(
        "readback=humanize edge=tremor_velocity before={base:?} after={humanized:?} result_value=low_delta:{low_velocity_delta:.6} high_delta:{high_velocity_delta:.6}"
    );
    assert!(low_velocity_delta > high_velocity_delta);
    Ok(())
}

#[test]
fn invalid_humanize_params_fail_closed() {
    let base = base_timed_path();
    let error = humanize_timed_path(
        &base,
        Some(HumanizeParams {
            tremor_base_stddev_px: f32::NAN,
            tremor_velocity_scale: 0.0,
            overshoot_prob: 0.0,
            overshoot_factor_range: (1.03, 1.12),
            micro_pause_prob: 0.0,
            micro_pause_ms_range: (15, 40),
            seed: None,
        }),
    )
    .expect_err("NaN tremor must be rejected");

    println!(
        "readback=humanize edge=invalid_param before=tremor_base_stddev_px:NaN after={error:?}"
    );
    assert!(matches!(
        error,
        HumanizeError::InvalidNonNegative {
            field: "tremor_base_stddev_px",
            ..
        }
    ));
}

fn base_timed_path() -> Vec<TimedPathPoint> {
    vec![
        timed(0.0, 0.0, 0.0, 0.0),
        timed(25.0, 25.0, 25.0, 0.0),
        timed(50.0, 50.0, 50.0, 0.0),
        timed(75.0, 75.0, 75.0, 0.0),
        timed(100.0, 100.0, 100.0, 0.0),
    ]
}

const fn timed(elapsed_ms: f64, arclen: f64, x: f64, y: f64) -> TimedPathPoint {
    TimedPathPoint {
        elapsed_ms,
        arclen,
        point: PathPoint { x, y },
    }
}

fn distance(left: PathPoint, right: PathPoint) -> f64 {
    left.distance_to(right)
}

fn timestamps_are_monotonic(samples: &[TimedPathPoint]) -> bool {
    samples
        .windows(2)
        .all(|window| window[0].elapsed_ms <= window[1].elapsed_ms)
}
