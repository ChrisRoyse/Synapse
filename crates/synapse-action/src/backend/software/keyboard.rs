use enigo::{Direction, Enigo, Key as EnigoKey, Keyboard};
use synapse_core::{Key, KeyCode};

use crate::{ActionError, EmitState, recovery};

use super::utils::{enigo, enigo_error, enigo_preserving_held_keys, sleep_ms};

#[tracing::instrument(skip_all, fields(action_kind = "software_key_press"))]
pub(super) fn press_key(key: &Key, hold_ms: u32, state: &mut EmitState) -> Result<(), ActionError> {
    validate_key(key)?;
    let mut enigo = enigo()?;
    recovery::record_held_key(key)?;
    state.hold_key(key);
    if let Err(error) = emit_key(&mut enigo, key, Direction::Press) {
        state.release_key(key);
        let _clear_result = recovery::clear_held_key(key);
        return Err(error);
    }
    let _interrupted = sleep_ms(hold_ms);
    emit_key(&mut enigo, key, Direction::Release)?;
    state.release_key(key);
    recovery::clear_held_key(key)?;
    Ok(())
}

#[tracing::instrument(skip_all, fields(action_kind = "software_key_state"))]
pub(super) fn key_down(key: &Key, state: &mut EmitState) -> Result<(), ActionError> {
    validate_key(key)?;
    let mut enigo = enigo_preserving_held_keys()?;
    recovery::record_held_key(key)?;
    state.hold_key(key);
    if let Err(error) = emit_key(&mut enigo, key, Direction::Press) {
        state.release_key(key);
        let _clear_result = recovery::clear_held_key(key);
        return Err(error);
    }
    Ok(())
}

#[tracing::instrument(skip_all, fields(action_kind = "software_key_state"))]
pub(super) fn key_up(key: &Key, state: &mut EmitState) -> Result<(), ActionError> {
    let mut enigo = enigo()?;
    emit_key(&mut enigo, key, Direction::Release)?;
    state.release_key(key);
    recovery::clear_held_key(key)?;
    Ok(())
}

#[tracing::instrument(skip_all, fields(action_kind = "software_key_chord"))]
pub(super) fn key_chord(
    keys: &[Key],
    hold_ms: u32,
    state: &mut EmitState,
) -> Result<(), ActionError> {
    let mut enigo = enigo()?;
    for key in keys {
        validate_key(key)?;
    }
    let mut pressed = Vec::with_capacity(keys.len());
    for key in keys {
        recovery::record_held_key(key)?;
        state.hold_key(key);
        if let Err(error) = emit_key(&mut enigo, key, Direction::Press) {
            state.release_key(key);
            let _clear_result = recovery::clear_held_key(key);
            release_keys_with(&mut enigo, &pressed)?;
            for pressed_key in pressed.iter().rev() {
                state.release_key(pressed_key);
                let _clear_result = recovery::clear_held_key(pressed_key);
            }
            return Err(error);
        }
        pressed.push(key.clone());
    }
    let _interrupted = sleep_ms(hold_ms);
    let mut first_error = None;
    for key in pressed.iter().rev() {
        match emit_key(&mut enigo, key, Direction::Release) {
            Ok(()) => {
                state.release_key(key);
                if let Err(error) = recovery::clear_held_key(key)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            Err(error) if first_error.is_none() => {
                first_error = Some(error);
            }
            Err(_error) => {}
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

pub(super) fn release_keys_with(enigo: &mut Enigo, keys: &[Key]) -> Result<(), ActionError> {
    for key in keys.iter().rev() {
        emit_key(enigo, key, Direction::Release)?;
    }
    Ok(())
}

fn emit_key(enigo: &mut Enigo, key: &Key, direction: Direction) -> Result<(), ActionError> {
    if key.use_scancode {
        let KeyCode::HidCode { value } = &key.code else {
            return Err(unsupported_key(key));
        };
        enigo
            .raw(u16::from(*value), direction)
            .map_err(enigo_error("emit raw scancode"))
    } else {
        enigo
            .key(enigo_key(key)?, direction)
            .map_err(enigo_error("emit key"))
    }
}

fn validate_key(key: &Key) -> Result<(), ActionError> {
    if key.use_scancode {
        matches!(&key.code, KeyCode::HidCode { .. })
            .then_some(())
            .ok_or_else(|| unsupported_key(key))
    } else {
        enigo_key(key).map(|_key| ())
    }
}

fn enigo_key(key: &Key) -> Result<EnigoKey, ActionError> {
    match &key.code {
        KeyCode::Symbol { value } => Ok(EnigoKey::Unicode(*value)),
        KeyCode::HidCode { .. } => Err(unsupported_key(key)),
        KeyCode::Named { value } => named_key(value).ok_or_else(|| unsupported_key(key)),
    }
}

fn named_key(value: &str) -> Option<EnigoKey> {
    let lower = value.to_ascii_lowercase();
    if let Some(ch) = single_ascii(&lower) {
        return Some(EnigoKey::Unicode(ch));
    }
    match lower.as_str() {
        "alt" => Some(EnigoKey::Alt),
        "backspace" => Some(EnigoKey::Backspace),
        "ctrl" | "control" => Some(EnigoKey::Control),
        "delete" => Some(EnigoKey::Delete),
        "down" | "arrowdown" => Some(EnigoKey::DownArrow),
        "end" => Some(EnigoKey::End),
        "enter" | "return" => Some(EnigoKey::Return),
        "escape" | "esc" => Some(EnigoKey::Escape),
        "home" => Some(EnigoKey::Home),
        "insert" => Some(EnigoKey::Insert),
        "left" | "arrowleft" => Some(EnigoKey::LeftArrow),
        "meta" | "win" | "windows" | "super" => Some(EnigoKey::Meta),
        "pagedown" => Some(EnigoKey::PageDown),
        "pageup" => Some(EnigoKey::PageUp),
        "right" | "arrowright" => Some(EnigoKey::RightArrow),
        "shift" | "leftshift" | "lshift" | "rightshift" | "rshift" => Some(EnigoKey::Shift),
        "space" => Some(EnigoKey::Space),
        "tab" => Some(EnigoKey::Tab),
        "up" | "arrowup" => Some(EnigoKey::UpArrow),
        "f1" => Some(EnigoKey::F1),
        "f2" => Some(EnigoKey::F2),
        "f3" => Some(EnigoKey::F3),
        "f4" => Some(EnigoKey::F4),
        "f5" => Some(EnigoKey::F5),
        "f6" => Some(EnigoKey::F6),
        "f7" => Some(EnigoKey::F7),
        "f8" => Some(EnigoKey::F8),
        "f9" => Some(EnigoKey::F9),
        "f10" => Some(EnigoKey::F10),
        "f11" => Some(EnigoKey::F11),
        "f12" => Some(EnigoKey::F12),
        _ => None,
    }
}

fn unsupported_key(key: &Key) -> ActionError {
    ActionError::UnsupportedKey {
        detail: format!("software backend does not support key code {:?}", key.code),
    }
}

fn single_ascii(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(ch)
}

#[cfg(test)]
mod tests {
    use synapse_core::{Key, KeyCode, error_codes};

    use super::*;

    #[test]
    fn unsupported_key_down_does_not_mark_key_held() {
        let key = Key {
            code: KeyCode::Named {
                value: "f24".to_owned(),
            },
            use_scancode: false,
        };
        let mut state = EmitState::new();
        let before = state.snapshot();
        let error = match key_down(&key, &mut state) {
            Ok(()) => panic!("f24 should be unsupported by Enigo"),
            Err(error) => error,
        };
        let after = state.snapshot();

        assert_eq!(error.code(), error_codes::ACTION_UNSUPPORTED_KEY);
        assert!(before.held_keys.is_empty());
        assert!(after.held_keys.is_empty());
    }
}
