use synapse_action::{
    VelocityError, fitts_law_duration_ms, normalized_velocity_at_time, position_at_time,
    sample_timed_path, time_at_position,
};
use synapse_core::{PathPoint, PathSpec, VelocityProfile};

const EPSILON: f64 = 1.0e-6;

#[test]
fn same_path_profiles_keep_points_but_change_timestamps() -> Result<(), Box<dyn std::error::Error>>
{
    let path = PathSpec::Line {
        from: PathPoint::new(0.0, 0.0),
        to: PathPoint::new(10.0, 0.0),
    };
    let linear = sample_timed_path(&path, VelocityProfile::Linear, 6, 1000.0)?;
    let minimum_jerk = sample_timed_path(&path, VelocityProfile::MinimumJerk, 6, 1000.0)?;
    let linear_points: Vec<PathPoint> = linear.iter().map(|sample| sample.point).collect();
    let min_jerk_points: Vec<PathPoint> = minimum_jerk.iter().map(|sample| sample.point).collect();
    let linear_times: Vec<f64> = linear.iter().map(|sample| sample.elapsed_ms).collect();
    let min_jerk_times: Vec<f64> = minimum_jerk
        .iter()
        .map(|sample| sample.elapsed_ms)
        .collect();

    println!(
        "readback=velocity_profile edge=same_path_different_profiles before=line:(0,0)->(10,0),samples:6,duration:1000 after_linear_points={linear_points:?} after_minjerk_points={min_jerk_points:?} after_linear_times={linear_times:?} after_minjerk_times={min_jerk_times:?}"
    );
    assert_eq!(linear_points, min_jerk_points);
    assert!((linear_times[1] - min_jerk_times[1]).abs() > EPSILON);
    assert!((linear_times[4] - min_jerk_times[4]).abs() > EPSILON);
    assert_point(linear_points[2], 4.0, 0.0);

    Ok(())
}

#[test]
fn minimum_jerk_velocity_starts_and_ends_near_zero_and_peaks_midway()
-> Result<(), Box<dyn std::error::Error>> {
    let start = normalized_velocity_at_time(VelocityProfile::MinimumJerk, 0.0)?;
    let quarter = normalized_velocity_at_time(VelocityProfile::MinimumJerk, 0.25)?;
    let mid = normalized_velocity_at_time(VelocityProfile::MinimumJerk, 0.5)?;
    let three_quarter = normalized_velocity_at_time(VelocityProfile::MinimumJerk, 0.75)?;
    let end = normalized_velocity_at_time(VelocityProfile::MinimumJerk, 1.0)?;

    println!(
        "readback=velocity_profile edge=minimum_jerk_velocity before=t:[0,.25,.5,.75,1] after=[{start},{quarter},{mid},{three_quarter},{end}] expected=start_end_zero_mid_peak"
    );
    assert!(start.abs() < EPSILON);
    assert!(end.abs() < EPSILON);
    assert!(mid > quarter);
    assert!(mid > three_quarter);
    assert!((quarter - three_quarter).abs() < EPSILON);
    assert!((mid - 1.875).abs() < EPSILON);

    Ok(())
}

#[test]
fn profile_position_and_inverse_are_consistent() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = [
        VelocityProfile::Constant,
        VelocityProfile::Linear,
        VelocityProfile::EaseInOut,
        VelocityProfile::MinimumJerk,
    ];

    for profile in profiles {
        let position = position_at_time(profile, 0.35)?;
        let inverted_time = time_at_position(profile, position)?;
        println!(
            "readback=velocity_profile edge=inverse_profile profile={profile:?} before_time=0.35 after_position={position} after_inverted_time={inverted_time}"
        );
        assert!((inverted_time - 0.35).abs() < EPSILON);
    }

    Ok(())
}

#[test]
fn fitts_law_duration_and_invalid_edges_are_explicit() -> Result<(), Box<dyn std::error::Error>> {
    let duration = fitts_law_duration_ms(300.0, 20.0, 50.0, 100.0)?;
    println!(
        "readback=velocity_profile edge=fitts_law before=distance:300,width:20,a:50,b:100 after_duration={duration} expected=450"
    );
    assert!((duration - 450.0).abs() < EPSILON);

    let invalid_width = fitts_law_duration_ms(300.0, 0.0, 50.0, 100.0);
    println!(
        "readback=velocity_profile edge=fitts_bad_width before=width:0 after={invalid_width:?} expected=invalid_fitts"
    );
    assert!(matches!(
        invalid_width,
        Err(VelocityError::InvalidFittsLawParameter {
            field: "target_width_px",
            value: 0.0
        })
    ));

    let invalid_time = position_at_time(VelocityProfile::EaseInOut, f64::NAN);
    println!(
        "readback=velocity_profile edge=invalid_time before=t:NaN after={invalid_time:?} expected=invalid_time"
    );
    assert!(matches!(
        invalid_time,
        Err(VelocityError::InvalidTimeFraction { .. })
    ));

    let invalid_duration = sample_timed_path(
        &PathSpec::Line {
            from: PathPoint::new(0.0, 0.0),
            to: PathPoint::new(1.0, 0.0),
        },
        VelocityProfile::Linear,
        2,
        0.0,
    );
    println!(
        "readback=velocity_profile edge=invalid_duration before=duration:0 after={invalid_duration:?} expected=invalid_duration"
    );
    assert!(matches!(
        invalid_duration,
        Err(VelocityError::InvalidDuration { duration_ms: 0.0 })
    ));

    Ok(())
}

fn assert_point(actual: PathPoint, expected_x: f64, expected_y: f64) {
    assert!(
        (actual.x - expected_x).abs() < EPSILON,
        "x mismatch: actual={actual:?} expected_x={expected_x}"
    );
    assert!(
        (actual.y - expected_y).abs() < EPSILON,
        "y mismatch: actual={actual:?} expected_y={expected_y}"
    );
}
