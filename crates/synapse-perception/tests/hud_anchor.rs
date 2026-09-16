use std::{
    error::Error,
    io::{self, Write},
};

use synapse_core::{HudRegion, Rect, WindowEdge, error_codes};
use synapse_perception::{
    HudAnchor, HudAnchorRegion, resolve_anchor_region, resolve_hud_region, resolve_hud_region_rect,
};

type TestResult = Result<(), Box<dyn Error>>;

fn readback_log(args: std::fmt::Arguments<'_>) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_fmt(args)?;
    stdout.write_all(b"\n")
}

#[test]
fn hud_anchor_resolves_bottom_left_acceptance_tuple() -> TestResult {
    let window = Rect {
        x: 0,
        y: 0,
        w: 1_920,
        h: 1_080,
    };
    let region = HudAnchorRegion {
        anchor: HudAnchor::BottomLeft,
        x_offset: 8,
        y_offset: -32,
        w: 180,
        h: 16,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=bottom_left_acceptance before=window:{window:?} region:{region:?}"
    ))?;

    let resolved = resolve_anchor_region(region, window)?;
    let rect = resolved.rect();
    readback_log(format_args!(
        "readback=hud_anchor edge=bottom_left_acceptance after_ltrb={:?} after_rect={rect:?}",
        resolved.as_ltrb()
    ))?;

    assert_eq!(resolved.as_ltrb(), (8, 1_048, 188, 1_064));
    assert_eq!(
        rect,
        Rect {
            x: 8,
            y: 1_048,
            w: 180,
            h: 16
        }
    );
    Ok(())
}

#[test]
fn hud_anchor_recomputes_when_window_resizes() -> TestResult {
    let before = Rect {
        x: 0,
        y: 0,
        w: 1_920,
        h: 1_080,
    };
    let after = Rect {
        x: 0,
        y: 0,
        w: 1_280,
        h: 720,
    };
    let region = HudAnchorRegion {
        anchor: HudAnchor::BottomLeft,
        x_offset: 8,
        y_offset: -32,
        w: 180,
        h: 16,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=resize before_window:{before:?} after_window:{after:?} region:{region:?}"
    ))?;

    let before_resolved = resolve_anchor_region(region, before)?;
    let after_resolved = resolve_anchor_region(region, after)?;
    readback_log(format_args!(
        "readback=hud_anchor edge=resize before_ltrb={:?} after_ltrb={:?}",
        before_resolved.as_ltrb(),
        after_resolved.as_ltrb()
    ))?;

    assert_eq!(before_resolved.as_ltrb(), (8, 1_048, 188, 1_064));
    assert_eq!(after_resolved.as_ltrb(), (8, 688, 188, 704));
    Ok(())
}

#[test]
fn hud_anchor_none_uses_absolute_screen_coordinates() -> TestResult {
    let ignored_window = Rect {
        x: 900,
        y: 500,
        w: 0,
        h: 0,
    };
    let region = HudAnchorRegion {
        anchor: HudAnchor::None,
        x_offset: 320,
        y_offset: 240,
        w: 100,
        h: 25,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=none_absolute before=window:{ignored_window:?} region:{region:?}"
    ))?;

    let resolved = resolve_anchor_region(region, ignored_window)?;
    readback_log(format_args!(
        "readback=hud_anchor edge=none_absolute after_ltrb={:?} after_rect={:?}",
        resolved.as_ltrb(),
        resolved.rect()
    ))?;

    assert_eq!(resolved.as_ltrb(), (320, 240, 420, 265));
    Ok(())
}

#[test]
fn hud_profile_regions_resolve_top_right_center_and_fractional() -> TestResult {
    let window = Rect {
        x: 10,
        y: 20,
        w: 200,
        h: 100,
    };
    let top_right = HudRegion::AnchoredToEdge {
        edge: WindowEdge::TopRight,
        x_offset: -30,
        y_offset: 5,
        w: 20,
        h: 10,
    };
    let bottom_center = HudRegion::AnchoredToEdge {
        edge: WindowEdge::BottomCenter,
        x_offset: -32,
        y_offset: -56,
        w: 64,
        h: 16,
    };
    let center = HudRegion::AnchoredToEdge {
        edge: WindowEdge::Center,
        x_offset: -5,
        y_offset: -10,
        w: 20,
        h: 10,
    };
    let fractional = HudRegion::FractionOfWindow {
        x: 0.5,
        y: 0.25,
        w: 0.25,
        h: 0.5,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=profile_regions before=window:{window:?} top_right:{top_right:?} bottom_center:{bottom_center:?} center:{center:?} fractional:{fractional:?}"
    ))?;

    let top_right_resolved = resolve_hud_region(&top_right, window)?;
    let bottom_center_resolved = resolve_hud_region(&bottom_center, window)?;
    let center_resolved = resolve_hud_region(&center, window)?;
    let fractional_rect = resolve_hud_region_rect(&fractional, window)?;
    readback_log(format_args!(
        "readback=hud_anchor edge=profile_regions after_top_right={:?} after_bottom_center={:?} after_center={:?} after_fractional_rect={fractional_rect:?}",
        top_right_resolved.as_ltrb(),
        bottom_center_resolved.as_ltrb(),
        center_resolved.as_ltrb()
    ))?;

    assert_eq!(top_right_resolved.as_ltrb(), (180, 25, 200, 35));
    assert_eq!(bottom_center_resolved.as_ltrb(), (78, 64, 142, 80));
    assert_eq!(center_resolved.as_ltrb(), (105, 60, 125, 70));
    assert_eq!(
        fractional_rect,
        Rect {
            x: 110,
            y: 45,
            w: 50,
            h: 50
        }
    );
    Ok(())
}

#[test]
fn hud_anchor_rejects_invalid_geometry() -> TestResult {
    let window = Rect {
        x: 0,
        y: 0,
        w: 1_920,
        h: 1_080,
    };

    let zero_width = HudAnchorRegion {
        anchor: HudAnchor::BottomLeft,
        x_offset: 8,
        y_offset: -32,
        w: 0,
        h: 16,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=zero_width before=window:{window:?} region:{zero_width:?}"
    ))?;
    let zero_width_error = resolve_anchor_region(zero_width, window).err();
    readback_log(format_args!(
        "readback=hud_anchor edge=zero_width after={zero_width_error:?}"
    ))?;
    assert_eq!(
        zero_width_error.map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );

    let invalid_window = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 1_080,
    };
    let anchored = HudAnchorRegion {
        anchor: HudAnchor::TopLeft,
        x_offset: 0,
        y_offset: 0,
        w: 10,
        h: 10,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=invalid_window before=window:{invalid_window:?} region:{anchored:?}"
    ))?;
    let invalid_window_error = resolve_anchor_region(anchored, invalid_window).err();
    readback_log(format_args!(
        "readback=hud_anchor edge=invalid_window after={invalid_window_error:?}"
    ))?;
    assert_eq!(
        invalid_window_error.map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );

    let outside_fraction = HudRegion::FractionOfWindow {
        x: 0.75,
        y: 0.0,
        w: 0.5,
        h: 0.1,
    };
    readback_log(format_args!(
        "readback=hud_anchor edge=outside_fraction before=window:{window:?} region:{outside_fraction:?}"
    ))?;
    let outside_fraction_error = resolve_hud_region(&outside_fraction, window).err();
    readback_log(format_args!(
        "readback=hud_anchor edge=outside_fraction after={outside_fraction_error:?}"
    ))?;
    assert_eq!(
        outside_fraction_error.map(|error| error.code()),
        Some(error_codes::HUD_EXTRACTION_FAILED)
    );

    Ok(())
}
