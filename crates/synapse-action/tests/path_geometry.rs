use synapse_action::{PathError, SpatialPath, path_point_at, sample_path};
use synapse_core::{PathPoint, PathSpec};

const EPSILON: f64 = 1.0e-6;

#[test]
fn path_primitives_return_hand_computed_points() -> Result<(), Box<dyn std::error::Error>> {
    let line = PathSpec::Line {
        from: point(0.0, 0.0),
        to: point(10.0, 20.0),
    };
    let line_after = path_point_at(&line, 0.5)?;
    println!(
        "readback=path_geometry edge=line_midpoint before=from:(0,0),to:(10,20),t:0.5 after={line_after:?} expected=(5,10)"
    );
    assert_point(line_after, 5.0, 10.0);

    let arc = PathSpec::Arc {
        center: point(0.0, 0.0),
        radius: 100.0,
        start_angle_rad: 0.0,
        sweep_angle_rad: std::f64::consts::FRAC_PI_2,
    };
    let arc_after = path_point_at(&arc, 1.0)?;
    println!(
        "readback=path_geometry edge=arc_quarter_turn before=center:(0,0),radius:100,start:0,sweep:pi/2,t:1 after={arc_after:?} expected=(0,100)"
    );
    assert_point(arc_after, 0.0, 100.0);

    let circle = PathSpec::Circle {
        center: point(5.0, -5.0),
        radius: 20.0,
    };
    let circle_path = SpatialPath::new(&circle)?;
    let circle_samples = circle_path.sample(5)?;
    println!(
        "readback=path_geometry edge=circle_closed before=center:(5,-5),radius:20,samples:5 after={circle_samples:?} result_value=is_closed:{}",
        circle_path.is_closed()
    );
    assert!(circle_path.is_closed());
    assert_same_point(circle_samples[0], circle_samples[4]);

    let cubic = PathSpec::CubicBezier {
        p0: point(0.0, 0.0),
        p1: point(0.0, 100.0),
        p2: point(100.0, 100.0),
        p3: point(100.0, 0.0),
    };
    let cubic_after = path_point_at(&cubic, 0.5)?;
    println!(
        "readback=path_geometry edge=cubic_bezier_midpoint before=p0:(0,0),p1:(0,100),p2:(100,100),p3:(100,0),t:0.5 after={cubic_after:?} expected=(50,75)"
    );
    assert_point(cubic_after, 50.0, 75.0);

    Ok(())
}

#[test]
fn polyline_and_closed_path_flags_return_expected_points() -> Result<(), Box<dyn std::error::Error>>
{
    let polyline = PathSpec::Polyline {
        points: vec![point(0.0, 0.0), point(10.0, 0.0), point(10.0, 10.0)],
        closed: false,
    };
    let first_segment_after = path_point_at(&polyline, 0.25)?;
    let second_segment_after = path_point_at(&polyline, 0.75)?;
    println!(
        "readback=path_geometry edge=polyline_segments before=points:[(0,0),(10,0),(10,10)] after_t25={first_segment_after:?} after_t75={second_segment_after:?} expected_t25=(5,0) expected_t75=(10,5)"
    );
    assert_point(first_segment_after, 5.0, 0.0);
    assert_point(second_segment_after, 10.0, 5.0);

    let closed_polyline = PathSpec::Polyline {
        points: vec![point(0.0, 0.0), point(10.0, 0.0), point(10.0, 10.0)],
        closed: true,
    };
    let closed_path = SpatialPath::new(&closed_polyline)?;
    let closed_samples = sample_path(&closed_polyline, 4)?;
    println!(
        "readback=path_geometry edge=closed_polyline before=closed:true,points:[(0,0),(10,0),(10,10)] after={closed_samples:?} result_value=is_closed:{}",
        closed_path.is_closed()
    );
    assert!(closed_path.is_closed());
    assert_same_point(closed_samples[0], closed_samples[3]);

    Ok(())
}

#[test]
fn catmull_rom_passes_through_waypoints_and_closes() -> Result<(), Box<dyn std::error::Error>> {
    let open = PathSpec::CatmullRom {
        waypoints: vec![
            point(0.0, 0.0),
            point(10.0, 0.0),
            point(10.0, 10.0),
            point(20.0, 10.0),
        ],
        alpha: 0.5,
        tension: 0.0,
        closed: false,
    };
    let path = SpatialPath::new(&open)?;
    let boundaries = [
        path.point_at(0.0)?,
        path.point_at(1.0 / 3.0)?,
        path.point_at(2.0 / 3.0)?,
        path.point_at(1.0)?,
    ];
    println!(
        "readback=path_geometry edge=catmull_open_boundaries before=waypoints:[(0,0),(10,0),(10,10),(20,10)] after={boundaries:?} expected=waypoints"
    );
    assert_point(boundaries[0], 0.0, 0.0);
    assert_point(boundaries[1], 10.0, 0.0);
    assert_point(boundaries[2], 10.0, 10.0);
    assert_point(boundaries[3], 20.0, 10.0);

    let closed = PathSpec::CatmullRom {
        waypoints: vec![
            point(0.0, 0.0),
            point(10.0, 0.0),
            point(10.0, 10.0),
            point(0.0, 10.0),
        ],
        alpha: 0.5,
        tension: 0.25,
        closed: true,
    };
    let closed_path = SpatialPath::new(&closed)?;
    let closed_boundaries = [
        closed_path.point_at(0.0)?,
        closed_path.point_at(0.25)?,
        closed_path.point_at(0.5)?,
        closed_path.point_at(0.75)?,
        closed_path.point_at(1.0)?,
    ];
    println!(
        "readback=path_geometry edge=catmull_closed_boundaries before=closed:true,tension:0.25 after={closed_boundaries:?} result_value=is_closed:{}",
        closed_path.is_closed()
    );
    assert!(closed_path.is_closed());
    assert_point(closed_boundaries[0], 0.0, 0.0);
    assert_point(closed_boundaries[1], 10.0, 0.0);
    assert_point(closed_boundaries[2], 10.0, 10.0);
    assert_point(closed_boundaries[3], 0.0, 10.0);
    assert_same_point(closed_boundaries[0], closed_boundaries[4]);

    Ok(())
}

#[test]
fn degenerate_segments_return_errors() {
    let same_line = PathSpec::Line {
        from: point(1.0, 1.0),
        to: point(1.0, 1.0),
    };
    let same_line_after = SpatialPath::new(&same_line);
    println!(
        "readback=path_geometry edge=degenerate_line before=from:(1,1),to:(1,1) after={same_line_after:?} expected=degenerate_segment"
    );
    assert!(matches!(
        same_line_after,
        Err(PathError::DegenerateSegment {
            kind: "line",
            index: 0
        })
    ));

    let too_short_polyline = PathSpec::Polyline {
        points: vec![point(0.0, 0.0)],
        closed: false,
    };
    let too_short_after = SpatialPath::new(&too_short_polyline);
    println!(
        "readback=path_geometry edge=too_short_polyline before=points:[(0,0)] after={too_short_after:?} expected=not_enough_points"
    );
    assert!(matches!(
        too_short_after,
        Err(PathError::NotEnoughPoints {
            kind: "polyline",
            min: 2,
            actual: 1
        })
    ));

    let duplicate_polyline = PathSpec::Polyline {
        points: vec![point(0.0, 0.0), point(0.0, 0.0), point(1.0, 1.0)],
        closed: false,
    };
    let duplicate_after = SpatialPath::new(&duplicate_polyline);
    println!(
        "readback=path_geometry edge=duplicate_polyline_segment before=points:[(0,0),(0,0),(1,1)] after={duplicate_after:?} expected=degenerate_segment"
    );
    assert!(matches!(
        duplicate_after,
        Err(PathError::DegenerateSegment {
            kind: "polyline",
            index: 0
        })
    ));
}

#[test]
fn invalid_parameters_return_errors() {
    let bad_arc = PathSpec::Arc {
        center: point(0.0, 0.0),
        radius: 0.0,
        start_angle_rad: 0.0,
        sweep_angle_rad: 1.0,
    };
    let bad_arc_after = SpatialPath::new(&bad_arc);
    println!(
        "readback=path_geometry edge=non_positive_radius before=radius:0 after={bad_arc_after:?} expected=non_positive_parameter"
    );
    assert!(matches!(
        bad_arc_after,
        Err(PathError::NonPositiveParameter {
            kind: "arc",
            field: "radius",
            value: 0.0
        })
    ));

    let non_finite = PathSpec::Line {
        from: point(f64::NAN, 0.0),
        to: point(1.0, 1.0),
    };
    let non_finite_after = SpatialPath::new(&non_finite);
    println!(
        "readback=path_geometry edge=non_finite_point before=from:(NaN,0),to:(1,1) after={non_finite_after:?} expected=non_finite_point"
    );
    assert!(matches!(
        non_finite_after,
        Err(PathError::NonFinitePoint {
            kind: "line",
            index: 0
        })
    ));

    let bad_catmull = PathSpec::CatmullRom {
        waypoints: vec![
            point(0.0, 0.0),
            point(1.0, 0.0),
            point(1.0, 1.0),
            point(2.0, 1.0),
        ],
        alpha: 1.25,
        tension: 0.0,
        closed: false,
    };
    let bad_catmull_after = SpatialPath::new(&bad_catmull);
    println!(
        "readback=path_geometry edge=bad_catmull_alpha before=alpha:1.25 after={bad_catmull_after:?} expected=invalid_alpha"
    );
    assert!(matches!(
        bad_catmull_after,
        Err(PathError::InvalidCatmullRomAlpha { alpha: 1.25 })
    ));

    let valid = PathSpec::Line {
        from: point(0.0, 0.0),
        to: point(1.0, 1.0),
    };
    let invalid_t_after = path_point_at(&valid, -0.1);
    println!(
        "readback=path_geometry edge=invalid_t before=t:-0.1 after={invalid_t_after:?} expected=invalid_t"
    );
    assert!(matches!(
        invalid_t_after,
        Err(PathError::InvalidT { t }) if (t + 0.1).abs() < EPSILON
    ));
}

const fn point(x: f64, y: f64) -> PathPoint {
    PathPoint { x, y }
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
