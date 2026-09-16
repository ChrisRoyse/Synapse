use std::time::{Duration, Instant};

use synapse_core::{Action, ComboInput, Point};

use crate::{ActionBackend, ActionError, EmitState, recovery};

mod input;
mod keyboard;
mod mouse;
mod text;
mod utils;

#[derive(Debug, Default)]
pub struct SoftwareBackend;

impl SoftwareBackend {
    #[must_use]
    #[tracing::instrument(fields(backend = "software"))]
    pub fn new() -> Self {
        Self
    }
}

/// Reads the current software cursor position in Win32 screen coordinates.
///
/// # Errors
///
/// Returns `ActionError::BackendUnavailable` when the OS cursor location cannot
/// be read from the active input desktop.
pub fn cursor_position() -> Result<Point, ActionError> {
    mouse::cursor_position()
}

/// Moves the software cursor to a Win32 screen coordinate and returns the
/// separately-read final cursor position.
///
/// # Errors
///
/// Returns `ActionError::BackendUnavailable` when the OS rejects the cursor
/// move or final readback does not match the requested point.
pub fn set_cursor_position(point: Point) -> Result<Point, ActionError> {
    mouse::set_cursor_position(point)
}

impl ActionBackend for SoftwareBackend {
    #[tracing::instrument(skip_all, fields(backend = "software"))]
    fn execute(&self, action: &Action, state: &mut EmitState) -> Result<(), ActionError> {
        crate::validate_action(action)?;
        match action {
            Action::KeyPress { key, hold_ms, .. } => keyboard::press_key(key, *hold_ms, state),
            Action::KeyDown { key, .. } => keyboard::key_down(key, state),
            Action::KeyUp { key, .. } => keyboard::key_up(key, state),
            Action::KeyChord { keys, hold_ms, .. } => keyboard::key_chord(keys, *hold_ms, state),
            Action::TypeText { text, dynamics, .. } => text::type_text(text, dynamics),
            Action::MouseMove {
                to,
                curve,
                duration_ms,
                ..
            } => mouse::mouse_move(to, curve, *duration_ms),
            Action::MouseMoveRelative { dx, dy, .. } => mouse::mouse_move_relative(*dx, *dy),
            Action::MouseButton {
                button,
                action,
                hold_ms,
                ..
            } => mouse::mouse_button(*button, *action, *hold_ms, state),
            Action::MouseDrag {
                from,
                to,
                button,
                curve,
                duration_ms,
                ..
            } => mouse::mouse_drag(*from, *to, *button, curve, *duration_ms, state),
            Action::MouseStroke {
                path,
                button,
                profile,
                timing,
                motion_model,
                humanize,
                ..
            } => mouse::mouse_stroke(
                path,
                *button,
                *profile,
                timing,
                *motion_model,
                *humanize,
                state,
            ),
            Action::MouseScroll { dy, dx, at, .. } => mouse::mouse_scroll(*dy, *dx, *at),
            Action::AimAt { target, style, .. } => mouse::aim_at(target, *style),
            Action::Combo { steps, .. } => combo(steps, state),
            Action::ReleaseAll => release_all(state),
            Action::PadButton { .. }
            | Action::PadStick { .. }
            | Action::PadTrigger { .. }
            | Action::PadReport { .. } => Err(ActionError::BackendUnavailable {
                detail: "software backend cannot emit gamepad actions".to_owned(),
            }),
        }
    }
}

#[tracing::instrument(skip_all, fields(action_kind = "software_combo"))]
fn combo(steps: &[synapse_core::ComboStep], state: &mut EmitState) -> Result<(), ActionError> {
    let start = Instant::now();
    for step in steps {
        sleep_until_combo_step(start, step.at_ms)?;
        match &step.input {
            ComboInput::KeyDown { key } => keyboard::key_down(key, state)?,
            ComboInput::KeyUp { key } => keyboard::key_up(key, state)?,
            ComboInput::KeyPress { key, hold_ms } => {
                keyboard::press_key(key, u32::from(*hold_ms), state)?;
            }
            ComboInput::MouseButton { button, action } => {
                mouse::mouse_button(*button, *action, 0, state)?;
            }
            ComboInput::MouseMoveRel { dx, dy } => mouse::mouse_move_relative(*dx, *dy)?,
            ComboInput::PadButton { .. } | ComboInput::PadStick { .. } => {
                return Err(ActionError::BackendUnavailable {
                    detail: "software backend cannot emit gamepad combo steps".to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn sleep_until_combo_step(start: Instant, at_ms: u32) -> Result<(), ActionError> {
    let elapsed_ms = elapsed_millis_u32(start.elapsed());
    let delay_ms = combo_step_delay_ms(elapsed_ms, at_ms);
    if utils::sleep_ms(delay_ms) {
        return Err(ActionError::SafetyOperatorHotkeyFired {
            detail: format!("operator release requested before combo step at_ms={at_ms}"),
        });
    }
    Ok(())
}

const fn combo_step_delay_ms(elapsed_ms: u32, at_ms: u32) -> u32 {
    at_ms.saturating_sub(elapsed_ms)
}

fn elapsed_millis_u32(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

#[tracing::instrument(skip_all, fields(action_kind = "software_release_all"))]
fn release_all(state: &mut EmitState) -> Result<(), ActionError> {
    let snapshot = state.snapshot();
    let mut enigo = utils::enigo()?;
    keyboard::release_keys_with(&mut enigo, &snapshot.held_keys)?;
    mouse::release_buttons_with(&mut enigo, &snapshot.held_buttons)?;
    for key in &snapshot.held_keys {
        recovery::clear_held_key(key)?;
    }
    for button in &snapshot.held_buttons {
        recovery::clear_held_button(*button)?;
    }
    state.release_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combo_step_delay_honors_absolute_offsets() {
        assert_eq!(combo_step_delay_ms(0, 150), 150);
        assert_eq!(combo_step_delay_ms(40, 150), 110);
        assert_eq!(combo_step_delay_ms(150, 150), 0);
        assert_eq!(combo_step_delay_ms(200, 150), 0);
    }
}
