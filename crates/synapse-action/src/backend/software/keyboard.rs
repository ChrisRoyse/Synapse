use enigo::{Direction, Enigo, Key as EnigoKey, Keyboard};
use synapse_core::{Key, KeyCode};

use crate::foreground_fence::{self, EmissionSite};
use crate::synthetic_input::{self, HeldKeyStrand};
use crate::{ActionError, EmitState, recovery};

use super::utils::{enigo, enigo_error, enigo_preserving_held_keys, sleep_ms};

/// A scoped key press: `SendInput` key state is global to the OS input queue, so
/// the only structurally safe way to press a key inside a scope that can `?`,
/// return early, or unwind is to make the matching release a [`Drop`]
/// obligation (#2082).
///
/// The guard is armed *before* the press leaves — a duplicate key-up is a no-op
/// in the input queue, an unpaired key-down disables the human's keyboard — and
/// disarmed only once the OS has taken the release.
#[tracing::instrument(skip_all, fields(action_kind = "software_key_press"))]
pub(super) fn press_key(key: &Key, hold_ms: u32, state: &mut EmitState) -> Result<(), ActionError> {
    validate_key(key)?;
    let mut enigo = enigo()?;
    recovery::record_held_key(key)?;
    state.hold_key(key);
    let mut strand = HeldKeyStrand::arm(key, "software_key_press");
    if let Err(error) = emit_key(&mut enigo, key, Direction::Press) {
        strand.disarm();
        state.release_key(key);
        let _clear_result = recovery::clear_held_key(key);
        return Err(error);
    }
    let _interrupted = sleep_ms(hold_ms);
    // `?` here would leave the key down; the guard's `Drop` is what makes the
    // early return safe, and it stays armed until the release provably lands.
    emit_key(&mut enigo, key, Direction::Release)?;
    strand.disarm();
    state.release_key(key);
    recovery::clear_held_key(key)?;
    Ok(())
}

/// An *unscoped* key press: `act_key_down` deliberately outlives this call and
/// is released by a later `act_key_up`.
///
/// There is no scope to hang a [`Drop`] on, so the strand mirror itself is the
/// obligation: the panic sweep, the startup sweep, `release_all` and the
/// watchdog all read it.
#[tracing::instrument(skip_all, fields(action_kind = "software_key_state"))]
pub(super) fn key_down(key: &Key, state: &mut EmitState) -> Result<(), ActionError> {
    validate_key(key)?;
    let mut enigo = enigo_preserving_held_keys()?;
    recovery::record_held_key(key)?;
    state.hold_key(key);
    let mut strand = HeldKeyStrand::arm(key, "software_key_down");
    if let Err(error) = emit_key(&mut enigo, key, Direction::Press) {
        strand.disarm();
        state.release_key(key);
        let _clear_result = recovery::clear_held_key(key);
        return Err(error);
    }
    // The press succeeded and is meant to persist past this call: hand the
    // mirror entry over to `key_up`/`release_all`/the watchdog instead of
    // releasing it at scope exit.
    strand.disarm_without_clearing_mirror();
    Ok(())
}

#[tracing::instrument(skip_all, fields(action_kind = "software_key_state"))]
pub(super) fn key_up(key: &Key, state: &mut EmitState) -> Result<(), ActionError> {
    let mut enigo = enigo()?;
    emit_key(&mut enigo, key, Direction::Release)?;
    forget_key_strand(key);
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
    // One guard per key that actually went down. Anything that leaves this
    // function without an explicit `disarm` — including a panic between the
    // first press and the last release — releases the whole chord.
    let mut strands: Vec<HeldKeyStrand> = Vec::with_capacity(keys.len());
    for key in keys {
        recovery::record_held_key(key)?;
        state.hold_key(key);
        let mut strand = HeldKeyStrand::arm(key, "software_key_chord");
        if let Err(error) = emit_key(&mut enigo, key, Direction::Press) {
            strand.disarm();
            state.release_key(key);
            let _clear_result = recovery::clear_held_key(key);
            // Unwind the prefix before propagating: `release_keys_with` now
            // completes every key-up even if one of them errors, and only then
            // reports. Nothing below may return early with a key still down.
            let unwind_result = release_keys_with(&mut enigo, &pressed);
            for pressed_key in pressed.iter().rev() {
                state.release_key(pressed_key);
                let _clear_result = recovery::clear_held_key(pressed_key);
            }
            for prior in &mut strands {
                prior.disarm();
            }
            unwind_result?;
            return Err(error);
        }
        strands.push(strand);
        pressed.push(key.clone());
    }
    let _interrupted = sleep_ms(hold_ms);
    let mut first_error = None;
    for (index, key) in pressed.iter().enumerate().rev() {
        match emit_key(&mut enigo, key, Direction::Release) {
            Ok(()) => {
                if let Some(strand) = strands.get_mut(index) {
                    strand.disarm();
                }
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
    // Any key whose release errored above is still armed and is released by
    // `strands` dropping here, at ERROR, rather than staying down.
    drop(strands);
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

pub(super) fn release_keys_with(enigo: &mut Enigo, keys: &[Key]) -> Result<(), ActionError> {
    let mut first_error = None;
    for key in keys.iter().rev() {
        // Release is the one path that may never return early leaving state
        // held: a failure on one key must not skip the remaining key-ups.
        match emit_key(enigo, key, Direction::Release) {
            Ok(()) => forget_key_strand(key),
            Err(error) => {
                tracing::error!(
                    code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                    phase = "release_keys_with",
                    key_debug = ?key,
                    detail = %error,
                    "a key release failed mid-sweep; retrying it raw and continuing with the remaining keys"
                );
                if let Some(token) = synthetic_input::key_strand_token(key)
                    // Raw retry through the panic-safe emitter: no enigo, no
                    // fence, no allocation.
                    && !synthetic_input::force_release_key_token(token)
                {
                    tracing::error!(
                        code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                        phase = "release_keys_with_raw_retry",
                        key_debug = ?key,
                        "the raw key-up retry could not be inserted either; the key may still be down system-wide"
                    );
                }
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn forget_key_strand(key: &Key) {
    if let Some(token) = synthetic_input::key_strand_token(key) {
        synthetic_input::clear_key_strand(token);
    }
}

/// Emits one key transition through `enigo`, which reaches `SendInput`
/// internally — a second global-input emission boundary alongside
/// [`super::input::send_input_batch`], so it carries its own fence check.
///
/// A key *release* is classified as a release emission and is allowed through a
/// tripped fence: stranding a held `Ctrl`/`Alt`/`Shift` across the desktop is a
/// worse failure for the human than one stray key-up in the window that took
/// the foreground. A key *press* is refused on drift (#2057).
fn emit_key(enigo: &mut Enigo, key: &Key, direction: Direction) -> Result<(), ActionError> {
    foreground_fence::guard_emission(match direction {
        Direction::Release => EmissionSite::release("key_release"),
        Direction::Press => EmissionSite::delivery("key_press"),
        Direction::Click => EmissionSite::delivery("key_click"),
    })?;
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
