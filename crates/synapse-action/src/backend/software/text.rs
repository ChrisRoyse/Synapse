use std::thread;

use synapse_core::KeystrokeDynamics;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY,
};

use super::{
    input::{keyboard_input, send_input_batch, virtual_keyboard_input},
    utils::sleep_ms_since,
};
use crate::ActionError;
use crate::backend::text_dispatch::{TextDispatchInput, text_dispatch_plan};
use crate::foreground_fence::EmissionSite;

#[tracing::instrument(skip_all, fields(action_kind = "software_type_text"))]
pub(super) fn type_text(text: &str, dynamics: &KeystrokeDynamics) -> Result<(), ActionError> {
    type_text_with_sender(text, dynamics, send_text_input)
}

/// Types a planned string, one OS emission per UTF-16 unit.
///
/// The plan sleeps a sampled inter-keystroke interval between steps, so this
/// loop spans real time — seconds for a long string. Each `sender` call reaches
/// `send_input_batch`, which re-reads the live foreground immediately before
/// its `SendInput`. The emission position (`unit_index`/`unit_total`) is
/// threaded down so a refusal names the exact character the sequence stopped
/// at, and the caller can re-issue only the undelivered remainder (#2057).
///
/// Because that timeline routinely outruns any default lease TTL, every unit
/// that has *already* cleared the fence heartbeats the holder's own input lease
/// (#2065). The heartbeat is bounded by the budget armed at the MCP layer and
/// cannot revive a lapsed or preempted lease, so a legitimately long string runs
/// to completion while a genuinely lost lease still refuses at the very next
/// emission boundary.
fn type_text_with_sender(
    text: &str,
    dynamics: &KeystrokeDynamics,
    mut sender: impl FnMut(TextDispatchInput, EmissionSite) -> Result<(), ActionError>,
) -> Result<(), ActionError> {
    let release_epoch = crate::hotkey::operator_release_epoch();
    let plan = text_dispatch_plan(text, dynamics);
    let unit_total = plan.iter().map(|step| step.inputs.len()).sum::<usize>();
    let mut unit_index = 0_usize;
    for (step_index, step) in plan.into_iter().enumerate() {
        if sleep_ms_since(step.iki_ms_before, release_epoch) {
            return Err(operator_release_error(
                "delay",
                step_index,
                None,
                step.iki_ms_before,
            ));
        }
        for (input_index, input) in step.inputs.into_iter().enumerate() {
            ensure_operator_release_not_requested(
                release_epoch,
                "before_input",
                step_index,
                Some(input_index),
                step.iki_ms_before,
            )?;
            sender(
                input,
                EmissionSite::delivery("type_text_unit")
                    .at(unit_index)
                    .of(unit_total),
            )?;
            unit_index += 1;
            // This unit passed the per-emission fence, so the lease was held and
            // the bound window still owned the foreground at the OS call. Re-arm
            // that same lease inside its armed ceiling so the *next* unit is not
            // refused merely because the string is long (#2065).
            crate::lease::heartbeat_emission_budget();
            thread::yield_now();
            ensure_operator_release_not_requested(
                release_epoch,
                "after_input",
                step_index,
                Some(input_index),
                step.iki_ms_before,
            )?;
        }
        thread::yield_now();
    }
    Ok(())
}

fn ensure_operator_release_not_requested(
    release_epoch: u64,
    stage: &'static str,
    step_index: usize,
    input_index: Option<usize>,
    delay_ms: u32,
) -> Result<(), ActionError> {
    if crate::hotkey::operator_release_requested_since(release_epoch) {
        return Err(operator_release_error(
            stage,
            step_index,
            input_index,
            delay_ms,
        ));
    }
    Ok(())
}

fn operator_release_error(
    stage: &'static str,
    step_index: usize,
    input_index: Option<usize>,
    delay_ms: u32,
) -> ActionError {
    let input_detail = input_index
        .map(|index| format!(" input_index={index}"))
        .unwrap_or_default();
    ActionError::SafetyOperatorHotkeyFired {
        detail: format!(
            "operator release requested during type_text stage={stage} step_index={step_index}{input_detail} delay_ms={delay_ms}"
        ),
    }
}

fn send_text_input(input: TextDispatchInput, site: EmissionSite) -> Result<(), ActionError> {
    match input {
        TextDispatchInput::UnicodeUnit(unit) => send_unicode_unit(unit, site),
        TextDispatchInput::VirtualKey(vkey) => send_virtual_key(VIRTUAL_KEY(vkey), site),
    }
}

/// One UTF-16 unit is a self-contained down+up pair in a single `SendInput`
/// batch, so the pair is one indivisible delivery: it can never be split by
/// the fence into a stranded key-down.
fn send_unicode_unit(unit: u16, site: EmissionSite) -> Result<(), ActionError> {
    let inputs = [
        keyboard_input(unit, KEYEVENTF_UNICODE),
        keyboard_input(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
    ];
    send_input_batch(&inputs, site)
}

fn send_virtual_key(vkey: VIRTUAL_KEY, site: EmissionSite) -> Result<(), ActionError> {
    let inputs = [
        virtual_keyboard_input(vkey, KEYBD_EVENT_FLAGS(0)),
        virtual_keyboard_input(vkey, KEYEVENTF_KEYUP),
    ];
    send_input_batch(&inputs, site)
}
