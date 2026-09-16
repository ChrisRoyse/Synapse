use std::cell::Cell;

use proptest::{
    prelude::*,
    test_runner::{Config, TestRunner},
};
use synapse_action::sample_curve;
use synapse_core::{AimCurve, AimNaturalParams, Point};

#[test]
fn sample_curve_known_shapes_and_edges() {
    let start = Point { x: 0, y: 0 };
    let end = Point { x: 8, y: 4 };

    let instant_before = "curve=instant start=(0,0) end=(8,4)";
    let instant = sample_curve(&AimCurve::Instant, start, end, 32, None);
    println!(
        "readback=curve_samples edge=instant before={instant_before:?} after={instant:?} result_value=len:{} first:{:?} last:{:?}",
        instant.len(),
        instant.first(),
        instant.last()
    );
    assert_eq!(instant, vec![start, end]);

    let linear = sample_curve(&AimCurve::Linear, start, end, 32, None);
    println!(
        "readback=curve_samples edge=linear_happy before=count_pending after={linear:?} result_value=len:{} first:{:?} last:{:?}",
        linear.len(),
        linear.first(),
        linear.last()
    );
    assert_eq!(linear.len(), 8);
    assert_eq!(linear.first().copied(), Some(start));
    assert_eq!(linear.last().copied(), Some(end));

    let zero_duration = sample_curve(&AimCurve::EaseInOut, start, end, 0, None);
    println!(
        "readback=curve_samples edge=zero_duration before=duration_ms:0 after={zero_duration:?} result_value=len:{} first:{:?} last:{:?}",
        zero_duration.len(),
        zero_duration.first(),
        zero_duration.last()
    );
    assert_eq!(zero_duration.len(), 8);
    assert_eq!(zero_duration.first().copied(), Some(start));
    assert_eq!(zero_duration.last().copied(), Some(end));

    let same_point = Point { x: -3, y: 7 };
    let stationary = sample_curve(&AimCurve::Linear, same_point, same_point, 40, None);
    println!(
        "readback=curve_samples edge=same_start_end before=point:{same_point:?} after={stationary:?} result_value=all_same:{}",
        stationary.iter().all(|point| *point == same_point)
    );
    assert!(stationary.iter().all(|point| *point == same_point));

    let bezier = sample_curve(
        &AimCurve::Bezier {
            p1: (0.0, 1.0),
            p2: (1.0, 0.0),
        },
        Point { x: -10, y: 10 },
        Point { x: 10, y: -10 },
        36,
        None,
    );
    println!(
        "readback=curve_samples edge=bezier_negative_coords before=start=(-10,10) end=(10,-10) after={bezier:?} result_value=len:{} first:{:?} last:{:?}",
        bezier.len(),
        bezier.first(),
        bezier.last()
    );
    assert_eq!(bezier.first().copied(), Some(Point { x: -10, y: 10 }));
    assert_eq!(bezier.last().copied(), Some(Point { x: 10, y: -10 }));

    let max_duration = sample_curve(
        &AimCurve::EaseInOut,
        Point { x: -250, y: 125 },
        Point { x: 250, y: -125 },
        2000,
        None,
    );
    println!(
        "readback=curve_samples edge=max_declared_duration before=duration_ms:2000 after_len={} after_first={:?} after_last={:?} result_value=capacity_boundary_pass",
        max_duration.len(),
        max_duration.first(),
        max_duration.last()
    );
    assert_eq!(max_duration.len(), 500);
    assert_eq!(
        max_duration.first().copied(),
        Some(Point { x: -250, y: 125 })
    );
    assert_eq!(
        max_duration.last().copied(),
        Some(Point { x: 250, y: -125 })
    );
}

#[test]
fn natural_curve_seeded_overshoot_and_determinism() {
    let start = Point { x: 0, y: 0 };
    let end = Point { x: 100, y: 0 };
    let params = AimNaturalParams {
        control_point_jitter: 0.0,
        tremor_stddev_px: 0.0,
        overshoot_prob: 1.0,
        overshoot_factor_range: (1.10, 1.30),
        micro_correct_steps: 2,
        timing_stddev_ms: 0.0,
        seed: Some(42),
    };
    let curve = AimCurve::Natural { params };

    let first = sample_curve(&curve, start, end, 50, None);
    let second = sample_curve(&curve, start, end, 50, None);
    let third = sample_curve(&curve, start, end, 50, Some(43));
    let overshoot_count = first.iter().filter(|point| point.x > end.x).count();

    println!(
        "readback=curve_samples edge=natural_forced_overshoot before=params:{params:?} after={first:?} result_value=len:{} final_sample:{:?} overshoot_count:{overshoot_count} deterministic:{}",
        first.len(),
        first.last(),
        first == second
    );

    assert_eq!(first, second);
    assert_ne!(first, third);
    assert!(overshoot_count > 0);
    assert_eq!(first.first().copied(), Some(start));
    assert_eq!(first.last().copied(), Some(end));
}

#[test]
fn curve_endpoints_proptest_all_variants() {
    run_endpoint_cases("instant", |_| AimCurve::Instant);
    run_endpoint_cases("linear", |_| AimCurve::Linear);
    run_endpoint_cases("ease_in_out", |_| AimCurve::EaseInOut);
    run_endpoint_cases("bezier", |_| AimCurve::Bezier {
        p1: (0.15, 0.85),
        p2: (0.85, 0.15),
    });
    run_endpoint_cases("natural", |seed| AimCurve::Natural {
        params: AimNaturalParams {
            seed: Some(u64::from(seed) + 7),
            ..AimNaturalParams::FAST
        },
    });
}

fn run_endpoint_cases(name: &str, curve_for_seed: impl Fn(u32) -> AimCurve) {
    let mut runner = TestRunner::new(Config {
        cases: 1000,
        failure_persistence: None,
        ..Config::default()
    });
    let strategy = (
        point_strategy(),
        point_strategy(),
        1_u32..=2000_u32,
        0_u32..=10_u32,
    );
    let final_samples = Cell::new(0);
    let final_first = Cell::new(None);
    let final_last = Cell::new(None);

    let result = runner.run(&strategy, |(start, end, duration_ms, seed)| {
        let curve = curve_for_seed(seed);
        let samples = sample_curve(&curve, start, end, duration_ms, Some(u64::from(seed)));

        prop_assert!(
            !samples.is_empty(),
            "empty samples for {name}: start={start:?} end={end:?} duration_ms={duration_ms}"
        );
        prop_assert_eq!(
            samples.first().copied(),
            Some(start),
            "bad first sample for {}: start={:?} end={:?} duration_ms={} samples={:?}",
            name,
            start,
            end,
            duration_ms,
            samples
        );
        prop_assert_eq!(
            samples.last().copied(),
            Some(end),
            "bad last sample for {}: start={:?} end={:?} duration_ms={} samples={:?}",
            name,
            start,
            end,
            duration_ms,
            samples
        );

        final_samples.set(samples.len());
        final_first.set(samples.first().copied());
        final_last.set(samples.last().copied());
        Ok(())
    });

    if let Err(error) = result {
        panic!("endpoint proptest failed for {name}: {error}");
    }

    println!(
        "readback=curve_endpoints edge={name} final_samples={} final_first={:?} final_last={:?} cases=1000 result_value=pass",
        final_samples.get(),
        final_first.get(),
        final_last.get()
    );
}

fn point_strategy() -> impl Strategy<Value = Point> {
    (-10_000_i32..=10_000, -10_000_i32..=10_000).prop_map(|(x, y)| Point { x, y })
}
