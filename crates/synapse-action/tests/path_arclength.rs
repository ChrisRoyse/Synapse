use synapse_action::{
    ArcLengthPath, PathError, path_length, path_point_at_arclen, sample_path_arclen,
};
use synapse_core::{PathPoint, PathSpec};

const EPSILON: f64 = 1.0e-5;

#[test]
fn unit_circle_length_approximates_two_pi() -> Result<(), Box<dyn std::error::Error>> {
    let circle = PathSpec::Circle {
        center: point(0.0, 0.0),
        radius: 1.0,
    };
    let arclen = ArcLengthPath::with_lut_segments(&circle, 4096)?;
    let length = arclen.length();
    println!(
        "readback=path_arclength edge=unit_circle_length before=radius:1,lut_segments:4096 after_length={length} expected={}",
        std::f64::consts::TAU
    );
    assert!((length - std::f64::consts::TAU).abs() < EPSILON);

    let convenience_length = path_length(&circle)?;
    println!(
        "readback=path_arclength edge=path_length_convenience before=radius:1 after_length={convenience_length}"
    );
    assert!((convenience_length - std::f64::consts::TAU).abs() < 1.0e-4);

    Ok(())
}

#[test]
fn equal_arclen_circle_samples_have_equal_chord_lengths() -> Result<(), Box<dyn std::error::Error>>
{
    let circle = PathSpec::Circle {
        center: point(12.0, -7.0),
        radius: 25.0,
    };
    let arclen = ArcLengthPath::with_lut_segments(&circle, 4096)?;
    let samples = arclen.sample_arclen(33)?;
    let chord_lengths: Vec<f64> = samples
        .windows(2)
        .map(|pair| pair[0].distance_to(pair[1]))
        .collect();
    let first = chord_lengths[0];
    let max_deviation = chord_lengths
        .iter()
        .map(|chord| (chord - first).abs())
        .fold(0.0_f64, f64::max);

    println!(
        "readback=path_arclength edge=circle_equal_chords before=radius:25,samples:33 after_chords={chord_lengths:?} result_value=max_deviation:{max_deviation}"
    );
    assert!(max_deviation < 0.01);
    assert_same_point(samples[0], samples[32]);

    let convenience_samples = sample_path_arclen(&circle, 9)?;
    println!(
        "readback=path_arclength edge=sample_path_arclen_convenience before=radius:25,samples:9 after={convenience_samples:?}"
    );
    assert_eq!(convenience_samples.len(), 9);

    Ok(())
}

#[test]
fn point_at_arclen_inverts_distance_on_line() -> Result<(), Box<dyn std::error::Error>> {
    let line = PathSpec::Line {
        from: point(0.0, 0.0),
        to: point(10.0, 0.0),
    };
    let arclen = ArcLengthPath::new(&line)?;
    let midpoint = arclen.point_at_arclen(4.0)?;
    let convenience = path_point_at_arclen(&line, 7.5)?;
    println!(
        "readback=path_arclength edge=line_distance_inversion before=line:(0,0)->(10,0),s:4,7.5 after_mid={midpoint:?} after_convenience={convenience:?}"
    );
    assert_point(midpoint, 4.0, 0.0);
    assert_point(convenience, 7.5, 0.0);
    assert!((arclen.length() - 10.0).abs() < EPSILON);

    Ok(())
}

#[test]
fn arclen_invalid_edges_return_explicit_errors() {
    let zero_line = PathSpec::Line {
        from: point(1.0, 1.0),
        to: point(1.0, 1.0),
    };
    let zero_line_after = ArcLengthPath::new(&zero_line);
    println!(
        "readback=path_arclength edge=zero_length_line before=from:(1,1),to:(1,1) after={zero_line_after:?} expected=degenerate_segment"
    );
    assert!(matches!(
        zero_line_after,
        Err(PathError::DegenerateSegment {
            kind: "line",
            index: 0
        })
    ));

    let single_point = PathSpec::Polyline {
        points: vec![point(0.0, 0.0)],
        closed: false,
    };
    let single_point_after = ArcLengthPath::new(&single_point);
    println!(
        "readback=path_arclength edge=single_point_polyline before=points:[(0,0)] after={single_point_after:?} expected=not_enough_points"
    );
    assert!(matches!(
        single_point_after,
        Err(PathError::NotEnoughPoints {
            kind: "polyline",
            min: 2,
            actual: 1
        })
    ));

    let circle = PathSpec::Circle {
        center: point(0.0, 0.0),
        radius: 1.0,
    };
    let bad_lut_after = ArcLengthPath::with_lut_segments(&circle, 0);
    println!(
        "readback=path_arclength edge=zero_lut_segments before=segments:0 after={bad_lut_after:?} expected=invalid_lut_segments"
    );
    assert!(matches!(
        bad_lut_after,
        Err(PathError::InvalidArcLengthSegments { segments: 0 })
    ));

    let arclen = match ArcLengthPath::new(&circle) {
        Ok(value) => value,
        Err(err) => panic!("valid circle should build arclen path: {err}"),
    };
    let bad_sample_after = arclen.sample_arclen(1);
    println!(
        "readback=path_arclength edge=one_sample before=samples:1 after={bad_sample_after:?} expected=invalid_sample_count"
    );
    assert!(matches!(
        bad_sample_after,
        Err(PathError::InvalidSampleCount { samples: 1 })
    ));

    let invalid_s_after = arclen.point_at_arclen(arclen.length() + 1.0);
    println!(
        "readback=path_arclength edge=out_of_range_s before=s:length+1 after={invalid_s_after:?} expected=invalid_arc_length"
    );
    assert!(matches!(
        invalid_s_after,
        Err(PathError::InvalidArcLength { .. })
    ));
}

const fn point(x: f64, y: f64) -> PathPoint {
    PathPoint::new(x, y)
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

fn assert_same_point(left: PathPoint, right: PathPoint) {
    assert_point(left, right.x, right.y);
}
