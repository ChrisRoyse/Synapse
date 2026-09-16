use synapse_core::Point;

pub(super) const ABSOLUTE_MOUSE_RANGE: i32 = 65_535;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct VirtualDesktop {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl VirtualDesktop {
    #[must_use]
    pub(super) const fn new(left: i32, top: i32, width: i32, height: i32) -> Option<Self> {
        if width > 0 && height > 0 {
            Some(Self {
                left,
                top,
                width,
                height,
            })
        } else {
            None
        }
    }

    #[must_use]
    pub(super) fn contains(self, point: Point) -> bool {
        let right_exclusive = i64::from(self.left) + i64::from(self.width);
        let bottom_exclusive = i64::from(self.top) + i64::from(self.height);
        i64::from(point.x) >= i64::from(self.left)
            && i64::from(point.x) < right_exclusive
            && i64::from(point.y) >= i64::from(self.top)
            && i64::from(point.y) < bottom_exclusive
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct AbsoluteMousePoint {
    pub(super) dx: i32,
    pub(super) dy: i32,
}

#[must_use]
pub(super) fn normalize_absolute_mouse_point(
    point: Point,
    desktop: VirtualDesktop,
) -> AbsoluteMousePoint {
    AbsoluteMousePoint {
        dx: normalize_axis(point.x, desktop.left, desktop.width),
        dy: normalize_axis(point.y, desktop.top, desktop.height),
    }
}

fn normalize_axis(coord: i32, origin: i32, length: i32) -> i32 {
    debug_assert!(length > 0);
    if length == 1 {
        return 0;
    }

    let span = i64::from(length - 1);
    let relative = (i64::from(coord) - i64::from(origin)).clamp(0, span);
    let normalized = (relative * i64::from(ABSOLUTE_MOUSE_RANGE) + span / 2) / span;
    match i32::try_from(normalized) {
        Ok(value) => value,
        Err(_err) => ABSOLUTE_MOUSE_RANGE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_single_monitor_extrema() {
        let desktop = VirtualDesktop {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };

        println!(
            "readback=mouse_coordinates edge=single_monitor before=desktop:{desktop:?} input=(0,0)"
        );
        let top_left = normalize_absolute_mouse_point(Point { x: 0, y: 0 }, desktop);
        println!(
            "readback=mouse_coordinates edge=single_monitor after={top_left:?} expected=(0,0)"
        );
        assert_eq!(top_left, AbsoluteMousePoint { dx: 0, dy: 0 });

        println!(
            "readback=mouse_coordinates edge=single_monitor before=desktop:{desktop:?} input=(1919,1079)"
        );
        let bottom_right = normalize_absolute_mouse_point(Point { x: 1919, y: 1079 }, desktop);
        println!(
            "readback=mouse_coordinates edge=single_monitor after={bottom_right:?} expected=(65535,65535)"
        );
        assert_eq!(
            bottom_right,
            AbsoluteMousePoint {
                dx: ABSOLUTE_MOUSE_RANGE,
                dy: ABSOLUTE_MOUSE_RANGE
            }
        );
    }

    #[test]
    fn normalizes_negative_origin_virtual_desktop() {
        let desktop = VirtualDesktop {
            left: -1920,
            top: -1080,
            width: 3840,
            height: 2160,
        };

        println!(
            "readback=mouse_coordinates edge=negative_origin before=desktop:{desktop:?} input=(-1920,-1080)"
        );
        let min = normalize_absolute_mouse_point(Point { x: -1920, y: -1080 }, desktop);
        println!(
            "readback=mouse_coordinates edge=negative_origin after_min={min:?} expected=(0,0)"
        );
        assert_eq!(min, AbsoluteMousePoint { dx: 0, dy: 0 });

        println!(
            "readback=mouse_coordinates edge=negative_origin before=desktop:{desktop:?} input=(1919,1079)"
        );
        let max = normalize_absolute_mouse_point(Point { x: 1919, y: 1079 }, desktop);
        println!(
            "readback=mouse_coordinates edge=negative_origin after_max={max:?} expected=(65535,65535)"
        );
        assert_eq!(
            max,
            AbsoluteMousePoint {
                dx: ABSOLUTE_MOUSE_RANGE,
                dy: ABSOLUTE_MOUSE_RANGE
            }
        );
    }

    #[test]
    fn clamps_out_of_bounds_points() {
        let desktop = VirtualDesktop {
            left: 100,
            top: 200,
            width: 300,
            height: 400,
        };

        println!(
            "readback=mouse_coordinates edge=clamp_low before=desktop:{desktop:?} input=(-10,10)"
        );
        let low = normalize_absolute_mouse_point(Point { x: -10, y: 10 }, desktop);
        println!("readback=mouse_coordinates edge=clamp_low after={low:?} expected=(0,0)");
        assert_eq!(low, AbsoluteMousePoint { dx: 0, dy: 0 });

        println!(
            "readback=mouse_coordinates edge=clamp_high before=desktop:{desktop:?} input=(1000,1000)"
        );
        let high = normalize_absolute_mouse_point(Point { x: 1000, y: 1000 }, desktop);
        println!(
            "readback=mouse_coordinates edge=clamp_high after={high:?} expected=(65535,65535)"
        );
        assert_eq!(
            high,
            AbsoluteMousePoint {
                dx: ABSOLUTE_MOUSE_RANGE,
                dy: ABSOLUTE_MOUSE_RANGE
            }
        );
    }

    #[test]
    fn contains_rejects_points_outside_virtual_desktop() {
        let desktop = VirtualDesktop {
            left: 100,
            top: 200,
            width: 300,
            height: 400,
        };

        assert!(desktop.contains(Point { x: 100, y: 200 }));
        assert!(desktop.contains(Point { x: 399, y: 599 }));
        assert!(!desktop.contains(Point { x: 99, y: 200 }));
        assert!(!desktop.contains(Point { x: 400, y: 599 }));
        assert!(!desktop.contains(Point { x: 100, y: 600 }));
    }

    #[test]
    fn rejects_invalid_virtual_desktop_metrics() {
        println!(
            "readback=mouse_coordinates edge=invalid_width before=left:0 top:0 width:0 height:1"
        );
        let zero_width = VirtualDesktop::new(0, 0, 0, 1);
        println!(
            "readback=mouse_coordinates edge=invalid_width after={zero_width:?} expected=None"
        );
        assert_eq!(zero_width, None);

        println!(
            "readback=mouse_coordinates edge=invalid_height before=left:0 top:0 width:1 height:0"
        );
        let zero_height = VirtualDesktop::new(0, 0, 1, 0);
        println!(
            "readback=mouse_coordinates edge=invalid_height after={zero_height:?} expected=None"
        );
        assert_eq!(zero_height, None);
    }

    #[test]
    fn one_pixel_axis_maps_to_zero() {
        let desktop = VirtualDesktop {
            left: 50,
            top: -7,
            width: 1,
            height: 1,
        };

        println!(
            "readback=mouse_coordinates edge=one_pixel before=desktop:{desktop:?} input=(50,-7)"
        );
        let normalized = normalize_absolute_mouse_point(Point { x: 50, y: -7 }, desktop);
        println!("readback=mouse_coordinates edge=one_pixel after={normalized:?} expected=(0,0)");
        assert_eq!(normalized, AbsoluteMousePoint { dx: 0, dy: 0 });
    }
}
