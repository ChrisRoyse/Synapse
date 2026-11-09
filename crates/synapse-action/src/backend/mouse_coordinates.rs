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
