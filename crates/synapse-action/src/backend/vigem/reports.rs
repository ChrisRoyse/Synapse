#[cfg(windows)]
use synapse_core::{GamepadReport, PadButton};

#[cfg(windows)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct X360ReportSnapshot {
    pub(super) buttons_raw: u16,
    pub(super) left_trigger: u8,
    pub(super) right_trigger: u8,
    pub(super) thumb_lx: i16,
    pub(super) thumb_ly: i16,
    pub(super) thumb_rx: i16,
    pub(super) thumb_ry: i16,
}

#[cfg(windows)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct Ds4ReportSnapshot {
    pub(super) buttons: u16,
    pub(super) special: u8,
    pub(super) trigger_l: u8,
    pub(super) trigger_r: u8,
    pub(super) thumb_lx: u8,
    pub(super) thumb_ly: u8,
    pub(super) thumb_rx: u8,
    pub(super) thumb_ry: u8,
}

#[cfg(windows)]
pub(super) fn x360_report_snapshot(report: &GamepadReport) -> X360ReportSnapshot {
    X360ReportSnapshot {
        buttons_raw: x360_buttons_raw(&report.buttons),
        left_trigger: normalized_trigger_to_u8(report.lt),
        right_trigger: normalized_trigger_to_u8(report.rt),
        thumb_lx: normalized_axis_to_i16(report.thumb_l.0),
        thumb_ly: normalized_axis_to_i16(report.thumb_l.1),
        thumb_rx: normalized_axis_to_i16(report.thumb_r.0),
        thumb_ry: normalized_axis_to_i16(report.thumb_r.1),
    }
}

#[cfg(windows)]
pub(super) fn ds4_report_snapshot(report: &GamepadReport) -> Ds4ReportSnapshot {
    let trigger_l = normalized_trigger_to_u8(report.lt);
    let trigger_r = normalized_trigger_to_u8(report.rt);
    Ds4ReportSnapshot {
        buttons: ds4_buttons_raw(&report.buttons, trigger_l, trigger_r),
        special: ds4_special_raw(&report.buttons),
        trigger_l,
        trigger_r,
        thumb_lx: normalized_axis_to_ds4_x(report.thumb_l.0),
        thumb_ly: normalized_axis_to_ds4_y(report.thumb_l.1),
        thumb_rx: normalized_axis_to_ds4_x(report.thumb_r.0),
        thumb_ry: normalized_axis_to_ds4_y(report.thumb_r.1),
    }
}

#[cfg(windows)]
impl Ds4ReportSnapshot {
    pub(super) const fn into_vigem_report(self) -> vigem_client::DS4Report {
        vigem_client::DS4Report {
            thumb_lx: self.thumb_lx,
            thumb_ly: self.thumb_ly,
            thumb_rx: self.thumb_rx,
            thumb_ry: self.thumb_ry,
            buttons: self.buttons,
            special: self.special,
            trigger_l: self.trigger_l,
            trigger_r: self.trigger_r,
        }
    }
}

#[cfg(windows)]
pub(super) const fn xgamepad_from_snapshot(snapshot: X360ReportSnapshot) -> vigem_client::XGamepad {
    vigem_client::XGamepad {
        buttons: vigem_client::XButtons {
            raw: snapshot.buttons_raw,
        },
        left_trigger: snapshot.left_trigger,
        right_trigger: snapshot.right_trigger,
        thumb_lx: snapshot.thumb_lx,
        thumb_ly: snapshot.thumb_ly,
        thumb_rx: snapshot.thumb_rx,
        thumb_ry: snapshot.thumb_ry,
    }
}

#[cfg(windows)]
fn x360_buttons_raw(buttons: &[PadButton]) -> u16 {
    buttons.iter().fold(0, |raw, button| {
        raw | match button {
            PadButton::Up => 0x0001,
            PadButton::Down => 0x0002,
            PadButton::Left => 0x0004,
            PadButton::Right => 0x0008,
            PadButton::Start => 0x0010,
            PadButton::Back => 0x0020,
            PadButton::Ls => 0x0040,
            PadButton::Rs => 0x0080,
            PadButton::Lb => 0x0100,
            PadButton::Rb => 0x0200,
            PadButton::Guide => 0x0400,
            PadButton::A => 0x1000,
            PadButton::B => 0x2000,
            PadButton::X => 0x4000,
            PadButton::Y => 0x8000,
        }
    })
}

#[cfg(windows)]
fn ds4_buttons_raw(buttons: &[PadButton], trigger_l: u8, trigger_r: u8) -> u16 {
    let mut raw = ds4_dpad_raw(buttons);
    for button in buttons {
        raw |= match button {
            PadButton::A => 1 << 5,
            PadButton::B => 1 << 6,
            PadButton::X => 1 << 4,
            PadButton::Y => 1 << 7,
            PadButton::Lb => 1 << 8,
            PadButton::Rb => 1 << 9,
            PadButton::Ls => 1 << 14,
            PadButton::Rs => 1 << 15,
            PadButton::Back => 1 << 12,
            PadButton::Start => 1 << 13,
            PadButton::Up
            | PadButton::Down
            | PadButton::Left
            | PadButton::Right
            | PadButton::Guide => 0,
        };
    }
    if trigger_l > 0 {
        raw |= 1 << 10;
    }
    if trigger_r > 0 {
        raw |= 1 << 11;
    }
    raw
}

#[cfg(windows)]
fn ds4_dpad_raw(buttons: &[PadButton]) -> u16 {
    let up = buttons.contains(&PadButton::Up);
    let right = buttons.contains(&PadButton::Right);
    let down = buttons.contains(&PadButton::Down);
    let left = buttons.contains(&PadButton::Left);
    match (up, right, down, left) {
        (true, true, false, false) => 0x1,
        (false, true, true, false) => 0x3,
        (false, false, true, true) => 0x5,
        (true, false, false, true) => 0x7,
        (true, false, false, false) => 0x0,
        (false, true, false, false) => 0x2,
        (false, false, true, false) => 0x4,
        (false, false, false, true) => 0x6,
        _ => 0x8,
    }
}

#[cfg(windows)]
fn ds4_special_raw(buttons: &[PadButton]) -> u8 {
    u8::from(buttons.contains(&PadButton::Guide))
}

#[cfg(windows)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn normalized_trigger_to_u8(value: f32) -> u8 {
    if !value.is_finite() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * f32::from(u8::MAX)).round() as u8
}

#[cfg(windows)]
#[allow(clippy::cast_possible_truncation)]
fn normalized_axis_to_i16(value: f32) -> i16 {
    if !value.is_finite() {
        return 0;
    }
    let clamped = value.clamp(-1.0, 1.0);
    if clamped.is_sign_negative() {
        (clamped * 32_768.0).round() as i16
    } else {
        (clamped * f32::from(i16::MAX)).round() as i16
    }
}

#[cfg(windows)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn normalized_axis_to_ds4_x(value: f32) -> u8 {
    if !value.is_finite() {
        return 0x80;
    }
    ((value.clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8
}

#[cfg(windows)]
fn normalized_axis_to_ds4_y(value: f32) -> u8 {
    normalized_axis_to_ds4_x(-value)
}
