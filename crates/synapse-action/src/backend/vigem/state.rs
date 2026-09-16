#![allow(clippy::redundant_pub_crate)]

#[cfg(any(windows, test))]
use synapse_core::{
    ButtonAction, GamepadController, GamepadReport, PadButton, PadId, Stick, Trigger,
};

#[cfg(any(windows, test))]
use crate::EmitState;

#[cfg(windows)]
pub(super) fn report_for_pad(state: &EmitState, pad: PadId) -> GamepadReport {
    state
        .pad_state
        .get(&pad)
        .cloned()
        .unwrap_or_else(neutral_x360_gamepad_report)
}

#[cfg(any(windows, test))]
pub(crate) fn apply_pad_button(
    state: &mut EmitState,
    pad: PadId,
    button: PadButton,
    action: ButtonAction,
) {
    let should_remove = {
        let report = state
            .pad_state
            .entry(pad)
            .or_insert_with(neutral_x360_gamepad_report);
        match action {
            ButtonAction::Down => push_unique(&mut report.buttons, button),
            ButtonAction::Up | ButtonAction::Press => {
                report.buttons.retain(|held| *held != button);
            }
        }
        is_neutral_report(report)
    };

    if should_remove {
        state.pad_state.remove(&pad);
    }
}

#[cfg(any(windows, test))]
pub(crate) fn apply_pad_stick(state: &mut EmitState, pad: PadId, stick: Stick, x: f32, y: f32) {
    let should_remove = {
        let report = state
            .pad_state
            .entry(pad)
            .or_insert_with(neutral_x360_gamepad_report);
        match stick {
            Stick::Left => report.thumb_l = (x, y),
            Stick::Right => report.thumb_r = (x, y),
        }
        is_neutral_report(report)
    };

    if should_remove {
        state.pad_state.remove(&pad);
    }
}

#[cfg(any(windows, test))]
pub(crate) fn apply_pad_trigger(state: &mut EmitState, pad: PadId, trigger: Trigger, value: f32) {
    let should_remove = {
        let report = state
            .pad_state
            .entry(pad)
            .or_insert_with(neutral_x360_gamepad_report);
        match trigger {
            Trigger::Left => report.lt = value,
            Trigger::Right => report.rt = value,
        }
        is_neutral_report(report)
    };

    if should_remove {
        state.pad_state.remove(&pad);
    }
}

#[cfg(any(windows, test))]
pub(crate) fn apply_pad_report(state: &mut EmitState, pad: PadId, report: GamepadReport) {
    if is_neutral_report(&report) {
        state.pad_state.remove(&pad);
    } else {
        state.pad_state.insert(pad, report);
    }
}

#[cfg(any(windows, test))]
pub(super) const fn neutral_gamepad_report(controller: GamepadController) -> GamepadReport {
    GamepadReport::neutral(controller)
}

#[cfg(any(windows, test))]
const fn neutral_x360_gamepad_report() -> GamepadReport {
    neutral_gamepad_report(GamepadController::X360)
}

#[cfg(any(windows, test))]
pub(super) fn is_neutral_report(report: &GamepadReport) -> bool {
    report.buttons.is_empty()
        && report.thumb_l == (0.0, 0.0)
        && report.thumb_r == (0.0, 0.0)
        && report.lt == 0.0
        && report.rt == 0.0
}

#[cfg(any(windows, test))]
pub(super) fn push_unique(buttons: &mut Vec<PadButton>, button: PadButton) {
    if !buttons.contains(&button) {
        buttons.push(button);
    }
}
