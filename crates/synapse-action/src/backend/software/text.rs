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

#[tracing::instrument(skip_all, fields(action_kind = "software_type_text"))]
pub(super) fn type_text(text: &str, dynamics: &KeystrokeDynamics) -> Result<(), ActionError> {
    type_text_with_sender(text, dynamics, send_text_input)
}

fn type_text_with_sender(
    text: &str,
    dynamics: &KeystrokeDynamics,
    mut sender: impl FnMut(TextDispatchInput) -> Result<(), ActionError>,
) -> Result<(), ActionError> {
    let release_epoch = crate::hotkey::operator_release_epoch();
    for (step_index, step) in text_dispatch_plan(text, dynamics).into_iter().enumerate() {
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
            sender(input)?;
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

fn send_text_input(input: TextDispatchInput) -> Result<(), ActionError> {
    match input {
        TextDispatchInput::UnicodeUnit(unit) => send_unicode_unit(unit),
        TextDispatchInput::VirtualKey(vkey) => {
            send_virtual_key(VIRTUAL_KEY(vkey), "text virtual key")
        }
    }
}

fn send_unicode_unit(unit: u16) -> Result<(), ActionError> {
    let inputs = [
        keyboard_input(unit, KEYEVENTF_UNICODE),
        keyboard_input(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
    ];
    send_input_batch(&inputs, "unicode text character")
}

fn send_virtual_key(vkey: VIRTUAL_KEY, detail: &'static str) -> Result<(), ActionError> {
    let inputs = [
        virtual_keyboard_input(vkey, KEYBD_EVENT_FLAGS(0)),
        virtual_keyboard_input(vkey, KEYEVENTF_KEYUP),
    ];
    send_input_batch(&inputs, detail)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use synapse_core::KeystrokeDynamics;
    use synapse_core::error_codes;

    use super::*;

    static HOTKEY_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn type_text_abort_observes_release_between_characters() {
        let _guard = HOTKEY_TEST_LOCK.lock().expect("hotkey test lock poisoned");
        let mut sent = Vec::new();
        let error = type_text_with_sender("abc", &KeystrokeDynamics::Burst, |input| {
            sent.push(input);
            if sent.len() == 1 {
                crate::hotkey::request_release_interrupt();
            }
            Ok(())
        })
        .expect_err("operator release must abort remaining text");

        assert_eq!(error.code(), error_codes::SAFETY_OPERATOR_HOTKEY_FIRED);
        assert_eq!(sent, [TextDispatchInput::UnicodeUnit(u16::from(b'a'))]);
        assert!(error.detail().contains("stage=after_input"));
        assert!(error.detail().contains("step_index=0"));
        assert!(error.detail().contains("input_index=0"));
    }

    #[test]
    fn type_text_ignores_release_epoch_from_previous_action() {
        let _guard = HOTKEY_TEST_LOCK.lock().expect("hotkey test lock poisoned");
        crate::hotkey::request_release_interrupt();
        let mut sent = Vec::new();

        type_text_with_sender("ab", &KeystrokeDynamics::Burst, |input| {
            sent.push(input);
            Ok(())
        })
        .expect("stale release epoch from before type_text start must not abort");

        assert_eq!(
            sent,
            [
                TextDispatchInput::UnicodeUnit(u16::from(b'a')),
                TextDispatchInput::UnicodeUnit(u16::from(b'b')),
            ]
        );
    }
}
