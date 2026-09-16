use std::mem;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, MOUSE_EVENT_FLAGS,
    MOUSEINPUT, SendInput, VIRTUAL_KEY,
};

use crate::ActionError;

pub(super) const fn keyboard_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub(super) const fn virtual_keyboard_input(vkey: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vkey,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub(super) const fn mouse_input(
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: MOUSE_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub(super) fn send_input_batch(inputs: &[INPUT], detail: &'static str) -> Result<(), ActionError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let cb_size =
        i32::try_from(mem::size_of::<INPUT>()).map_err(|_err| ActionError::BackendUnavailable {
            detail: "INPUT struct size does not fit SendInput cbSize".to_owned(),
        })?;
    // SAFETY: `inputs` points to initialized Windows `INPUT` values for the
    // duration of the call, and `cb_size` is exactly `size_of::<INPUT>()`.
    let sent = unsafe { SendInput(inputs, cb_size) };
    let expected = u32::try_from(inputs.len()).map_err(|_err| ActionError::BackendUnavailable {
        detail: "SendInput input count does not fit u32".to_owned(),
    })?;
    if sent == expected {
        Ok(())
    } else {
        Err(ActionError::BackendUnavailable {
            detail: format!(
                "SendInput inserted {sent}/{} events for {detail}",
                inputs.len()
            ),
        })
    }
}
