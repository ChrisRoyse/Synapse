use rmcp::ErrorData;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use synapse_action::ActionError;
use synapse_core::Key;

use super::{action_error_to_mcp, key_label};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HwndKeyboardTargetState {
    pub root_hwnd: i64,
    pub hwnd: i64,
    pub class_name: String,
    pub text_len: Option<usize>,
    pub text_sha256: Option<String>,
    pub selection_start: Option<u32>,
    pub selection_end: Option<u32>,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct WindowCandidate {
    root_hwnd: windows::Win32::Foundation::HWND,
    hwnd: windows::Win32::Foundation::HWND,
    class_name: String,
    score: u8,
}

#[cfg(windows)]
#[derive(Debug)]
struct ChildEnumContext {
    root_hwnd: windows::Win32::Foundation::HWND,
    candidates: Vec<WindowCandidate>,
}

pub(crate) fn hwnd_keyboard_target_state(
    root_hwnd: i64,
) -> Result<HwndKeyboardTargetState, ErrorData> {
    hwnd_keyboard_target_state_impl(root_hwnd).map_err(|error| action_error_to_mcp(&error))
}

pub(crate) async fn post_key_sequence(
    root_hwnd: i64,
    keys: &[Key],
    hold_ms: u32,
) -> Result<HwndKeyboardTargetState, ErrorData> {
    post_key_sequence_impl(root_hwnd, keys, hold_ms)
        .await
        .map_err(|error| action_error_to_mcp(&error))
}

#[cfg(windows)]
async fn post_key_sequence_impl(
    root_hwnd: i64,
    keys: &[Key],
    hold_ms: u32,
) -> Result<HwndKeyboardTargetState, ActionError> {
    let key_specs = keys.iter().map(key_spec).collect::<Result<Vec<_>, _>>()?;
    let chord_label = keys.iter().map(key_label).collect::<Vec<_>>().join("+");
    let has_command_modifier = key_specs
        .iter()
        .any(|spec| matches!(spec.label, "ctrl" | "alt" | "super"));
    let mut release_keys = false;
    let target_hwnd = {
        let target = best_keyboard_target(root_hwnd)?;
        if is_ctrl_a_shortcut(&key_specs) {
            select_all_edit_target(&target)?;
            hwnd_to_i64(target.hwnd)
        } else if has_command_modifier {
            // A Ctrl/Alt/Win + key chord cannot be delivered by posting
            // WM_KEYDOWN: PostMessage does not update the OS keyboard shift
            // state, so the target's GetKeyState/GetAsyncKeyState see no
            // modifier held and the accelerator never fires (documented in
            // "You can't simulate keyboard input with PostMessage", Raymond
            // Chen). Worse, the target's own TranslateMessage turns the posted
            // WM_KEYDOWN into a stray literal WM_CHAR that corrupts the window.
            // We deliver the well-known edit shortcuts as their semantic window
            // message instead, and fail loud for every other chord so the
            // caller escalates to the foreground SendInput tier rather than
            // silently corrupting the target.
            let Some((message, message_name)) = edit_semantic_message(&key_specs) else {
                return Err(ActionError::BackendUnavailable {
                    detail: format!(
                        "act_press PostMessage background tier cannot deliver modifier chord {chord_label:?}: posting WM_KEYDOWN cannot set the OS modifier key-state the target reads via GetKeyState/GetAsyncKeyState, so application accelerators never fire and a stray literal character would corrupt the target. Deliver this chord through the foreground SendInput tier (acquire the input lease and foreground the window) or a CDP target instead."
                    ),
                });
            };
            send_edit_semantic_message(&target, message, message_name)?;
            hwnd_to_i64(target.hwnd)
        } else if is_plain_printable_text(&key_specs) {
            post_char_messages(target.hwnd, &key_specs)?;
            hwnd_to_i64(target.hwnd)
        } else {
            for spec in &key_specs {
                post_key_message(
                    target.hwnd,
                    windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                    spec,
                )?;
            }
            release_keys = true;
            post_char_messages(target.hwnd, &key_specs)?;
            hwnd_to_i64(target.hwnd)
        }
    };
    if hold_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(hold_ms))).await;
    }
    let target = hwnd_from_i64(target_hwnd)?;
    if release_keys {
        for spec in key_specs.iter().rev() {
            post_key_message(
                target,
                windows::Win32::UI::WindowsAndMessaging::WM_KEYUP,
                spec,
            )?;
        }
    }
    target_state(best_keyboard_target(root_hwnd)?)
}

#[cfg(not(windows))]
async fn post_key_sequence_impl(
    _root_hwnd: i64,
    _keys: &[Key],
    _hold_ms: u32,
) -> Result<HwndKeyboardTargetState, ActionError> {
    Err(ActionError::BackendUnavailable {
        detail: "act_press PostMessage keyboard tier is only available on Windows".to_owned(),
    })
}

#[cfg(windows)]
fn hwnd_keyboard_target_state_impl(root_hwnd: i64) -> Result<HwndKeyboardTargetState, ActionError> {
    target_state(best_keyboard_target(root_hwnd)?)
}

#[cfg(not(windows))]
fn hwnd_keyboard_target_state_impl(
    _root_hwnd: i64,
) -> Result<HwndKeyboardTargetState, ActionError> {
    Err(ActionError::BackendUnavailable {
        detail: "act_press PostMessage keyboard tier is only available on Windows".to_owned(),
    })
}

#[cfg(windows)]
fn best_keyboard_target(root_hwnd: i64) -> Result<WindowCandidate, ActionError> {
    use std::ffi::c_void;
    use windows::Win32::{
        Foundation::LPARAM,
        UI::{
            Input::KeyboardAndMouse::IsWindowEnabled,
            WindowsAndMessaging::{EnumChildWindows, IsWindow, IsWindowVisible},
        },
    };

    let root = hwnd_from_i64(root_hwnd)?;
    if !unsafe { IsWindow(Some(root)) }.as_bool() {
        return Err(ActionError::TargetInvalid {
            detail: format!("act_press PostMessage root hwnd 0x{root_hwnd:x} is not a live window"),
        });
    }
    let mut context = ChildEnumContext {
        root_hwnd: root,
        candidates: Vec::new(),
    };
    let context_ptr = (&raw mut context).cast::<c_void>();
    let _ = unsafe {
        EnumChildWindows(
            Some(root),
            Some(enum_keyboard_child),
            LPARAM(context_ptr as isize),
        )
    };
    let root_class_name = window_class_name(root);
    let root_candidate = WindowCandidate {
        root_hwnd: root,
        hwnd: root,
        score: class_keyboard_score(&root_class_name),
        class_name: root_class_name,
    };
    context
        .candidates
        .into_iter()
        .chain(std::iter::once(root_candidate))
        .filter(|candidate| unsafe { IsWindowVisible(candidate.hwnd) }.as_bool())
        .filter(|candidate| unsafe { IsWindowEnabled(candidate.hwnd) }.as_bool())
        .max_by_key(|candidate| candidate.score)
        .ok_or_else(|| ActionError::ElementNotResolved {
            detail: format!(
                "act_press PostMessage could not resolve an enabled keyboard target under hwnd 0x{root_hwnd:x}"
            ),
        })
}

#[cfg(windows)]
unsafe extern "system" fn enum_keyboard_child(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::UI::{
        Input::KeyboardAndMouse::IsWindowEnabled, WindowsAndMessaging::IsWindowVisible,
    };

    let context = unsafe { &mut *(lparam.0 as *mut ChildEnumContext) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool() && unsafe { IsWindowEnabled(hwnd) }.as_bool() {
        let class_name = window_class_name(hwnd);
        let score = class_keyboard_score(&class_name);
        if score > 0 {
            context.candidates.push(WindowCandidate {
                root_hwnd: context.root_hwnd,
                hwnd,
                class_name,
                score,
            });
        }
    }
    windows::core::BOOL(1)
}

#[cfg(windows)]
fn class_keyboard_score(class_name: &str) -> u8 {
    let lowered = class_name.to_ascii_lowercase();
    if lowered.contains("edit") || lowered.contains("richedit") {
        return 100;
    }
    if lowered.contains("text") || lowered.contains("scintilla") {
        return 80;
    }
    if lowered.contains("document") || lowered.contains("chrome_renderwidgethosthwnd") {
        return 40;
    }
    1
}

#[cfg(windows)]
fn is_ctrl_a_shortcut(key_specs: &[KeySpec]) -> bool {
    key_specs.len() == 2
        && key_specs[0].label == "ctrl"
        && key_specs[1].label == "letter"
        && key_specs[1].char_code == Some(u16::from(b'a'))
}

#[cfg(windows)]
fn is_plain_printable_text(key_specs: &[KeySpec]) -> bool {
    key_specs.len() == 1 && key_specs[0].char_code.is_some()
}

#[cfg(windows)]
fn select_all_edit_target(target: &WindowCandidate) -> Result<(), ActionError> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW},
    };

    let class_name = target.class_name.to_ascii_lowercase();
    if !(class_name.contains("edit") || class_name.contains("richedit")) {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage Ctrl+A background delivery requires an edit/rich-edit target; resolved hwnd 0x{:x} class {:?}",
                hwnd_to_i64(target.hwnd),
                target.class_name
            ),
        });
    }

    const EM_SETSEL: u32 = 0x00B1;
    const TIMEOUT_MS: u32 = 250;
    let result = unsafe {
        SendMessageTimeoutW(
            target.hwnd,
            EM_SETSEL,
            WPARAM(0),
            LPARAM(-1),
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            None,
        )
    };
    if result.0 == 0 {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage Ctrl+A EM_SETSEL timed out or failed for hwnd 0x{:x}",
                hwnd_to_i64(target.hwnd)
            ),
        });
    }
    Ok(())
}

/// Map an exact `Ctrl+<letter>` clipboard/undo chord to the semantic window
/// message that standard EDIT and RICHEDIT controls implement without any
/// dependency on OS modifier key-state. Returns `None` for everything else,
/// including `Ctrl+C` (copy) and `Ctrl+Y`/`Ctrl+Shift+Z` (redo): their effect
/// lives in the clipboard, which the HWND-text Source-of-Truth verify path
/// cannot prove, so they fail loud and escalate rather than report an
/// unverifiable success.
#[cfg(windows)]
fn edit_semantic_message(key_specs: &[KeySpec]) -> Option<(u32, &'static str)> {
    const WM_CUT: u32 = 0x0300;
    const WM_PASTE: u32 = 0x0302;
    const EM_UNDO: u32 = 0x00C7;
    if key_specs.len() != 2 || key_specs[0].label != "ctrl" || key_specs[1].label != "letter" {
        return None;
    }
    match key_specs[1].char_code {
        Some(code) if code == u16::from(b'v') => Some((WM_PASTE, "WM_PASTE")),
        Some(code) if code == u16::from(b'x') => Some((WM_CUT, "WM_CUT")),
        Some(code) if code == u16::from(b'z') => Some((EM_UNDO, "EM_UNDO")),
        _ => None,
    }
}

/// Deliver a clipboard/undo edit shortcut to an edit/rich-edit target via its
/// semantic window message. Fails loud when the resolved target is not an
/// edit-class control (the message would be a no-op there) so the caller
/// escalates to the foreground SendInput tier. The actual effect is proven by
/// the caller's Source-of-Truth text-delta verification, not by this return.
#[cfg(windows)]
fn send_edit_semantic_message(
    target: &WindowCandidate,
    message: u32,
    message_name: &'static str,
) -> Result<(), ActionError> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW},
    };

    let class_name = target.class_name.to_ascii_lowercase();
    if !(class_name.contains("edit") || class_name.contains("richedit")) {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage {message_name} background delivery requires an edit/rich-edit target; resolved hwnd 0x{:x} class {:?}. Deliver this chord through the foreground SendInput tier or a CDP target instead.",
                hwnd_to_i64(target.hwnd),
                target.class_name
            ),
        });
    }

    const TIMEOUT_MS: u32 = 250;
    let result = unsafe {
        SendMessageTimeoutW(
            target.hwnd,
            message,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            None,
        )
    };
    if result.0 == 0 {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage {message_name} (0x{message:04x}) timed out or was not handled by hwnd 0x{:x}",
                hwnd_to_i64(target.hwnd)
            ),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn target_state(target: WindowCandidate) -> Result<HwndKeyboardTargetState, ActionError> {
    let text = window_text(target.hwnd)?;
    let selection = edit_selection(target.hwnd);
    Ok(HwndKeyboardTargetState {
        root_hwnd: hwnd_to_i64(target.root_hwnd),
        hwnd: hwnd_to_i64(target.hwnd),
        class_name: target.class_name,
        text_len: Some(text.chars().count()),
        text_sha256: Some(hex_encode(&Sha256::digest(text.as_bytes()))),
        selection_start: selection.map(|(start, _end)| start),
        selection_end: selection.map(|(_start, end)| end),
    })
}

#[cfg(windows)]
#[derive(Copy, Clone, Debug)]
struct KeySpec {
    label: &'static str,
    vk: u16,
    char_code: Option<u16>,
    ctrl_char_code: Option<u16>,
}

#[cfg(windows)]
fn key_spec(key: &Key) -> Result<KeySpec, ActionError> {
    let label = key_label(key);
    let label_ref = label.as_str();
    let spec = match label_ref {
        "ctrl" => KeySpec {
            label: "ctrl",
            vk: 0x11,
            char_code: None,
            ctrl_char_code: None,
        },
        "shift" => KeySpec {
            label: "shift",
            vk: 0x10,
            char_code: None,
            ctrl_char_code: None,
        },
        "alt" => KeySpec {
            label: "alt",
            vk: 0x12,
            char_code: None,
            ctrl_char_code: None,
        },
        "super" => KeySpec {
            label: "super",
            vk: 0x5B,
            char_code: None,
            ctrl_char_code: None,
        },
        "backspace" => named_key("backspace", 0x08),
        "tab" => named_key("tab", 0x09),
        "enter" => named_key("enter", 0x0D),
        "esc" => named_key("esc", 0x1B),
        "space" => KeySpec {
            label: "space",
            vk: 0x20,
            char_code: Some(u16::from(b' ')),
            ctrl_char_code: None,
        },
        "pageup" => named_key("pageup", 0x21),
        "pagedown" => named_key("pagedown", 0x22),
        "end" => named_key("end", 0x23),
        "home" => named_key("home", 0x24),
        "left" => named_key("left", 0x25),
        "up" => named_key("up", 0x26),
        "right" => named_key("right", 0x27),
        "down" => named_key("down", 0x28),
        "insert" => named_key("insert", 0x2D),
        "delete" => named_key("delete", 0x2E),
        "`" => KeySpec {
            label: "`",
            vk: 0xC0,
            char_code: Some(u16::from(b'`')),
            ctrl_char_code: None,
        },
        label if label.len() == 1 && label.as_bytes()[0].is_ascii_alphabetic() => {
            let byte = label.as_bytes()[0].to_ascii_uppercase();
            KeySpec {
                label: "letter",
                vk: u16::from(byte),
                char_code: Some(u16::from(label.as_bytes()[0].to_ascii_lowercase())),
                ctrl_char_code: Some(u16::from(
                    label.as_bytes()[0].to_ascii_uppercase() - b'A' + 1,
                )),
            }
        }
        label if label.len() == 1 && label.as_bytes()[0].is_ascii_digit() => KeySpec {
            label: "digit",
            vk: u16::from(label.as_bytes()[0]),
            char_code: Some(u16::from(label.as_bytes()[0])),
            ctrl_char_code: None,
        },
        label if label.starts_with('f') => {
            let number =
                label[1..]
                    .parse::<u16>()
                    .map_err(|error| ActionError::UnsupportedKey {
                        detail: format!(
                            "act_press PostMessage unsupported function key {label:?}: {error}"
                        ),
                    })?;
            if !(1..=24).contains(&number) {
                return Err(ActionError::UnsupportedKey {
                    detail: format!("act_press PostMessage unsupported function key {label:?}"),
                });
            }
            KeySpec {
                label: "function",
                vk: 0x70 + number - 1,
                char_code: None,
                ctrl_char_code: None,
            }
        }
        _ => {
            return Err(ActionError::UnsupportedKey {
                detail: format!("act_press PostMessage unsupported key {label:?}"),
            });
        }
    };
    Ok(spec)
}

#[cfg(windows)]
const fn named_key(label: &'static str, vk: u16) -> KeySpec {
    KeySpec {
        label,
        vk,
        char_code: None,
        ctrl_char_code: None,
    }
}

#[cfg(windows)]
fn post_char_messages(
    hwnd: windows::Win32::Foundation::HWND,
    key_specs: &[KeySpec],
) -> Result<(), ActionError> {
    let ctrl_down = key_specs.iter().any(|key| key.label == "ctrl");
    let alt_or_super_down = key_specs
        .iter()
        .any(|key| matches!(key.label, "alt" | "super"));
    let Some(final_key) = key_specs.last() else {
        return Ok(());
    };
    let char_code = if ctrl_down {
        final_key.ctrl_char_code
    } else if alt_or_super_down {
        None
    } else {
        final_key.char_code
    };
    if let Some(char_code) = char_code {
        post_char_message(hwnd, char_code, final_key)?;
    }
    Ok(())
}

#[cfg(windows)]
fn post_key_message(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    key: &KeySpec,
) -> Result<(), ActionError> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{
            Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW},
            WindowsAndMessaging::{PostMessageW, WM_KEYUP},
        },
    };

    let scan = unsafe { MapVirtualKeyW(u32::from(key.vk), MAPVK_VK_TO_VSC) } & 0xff;
    let mut bits = 1_u32 | (scan << 16);
    if message == WM_KEYUP {
        bits |= 1 << 30;
        bits |= 1 << 31;
    }
    let lparam = LPARAM(isize::try_from(bits).unwrap_or(isize::MAX));
    unsafe { PostMessageW(Some(hwnd), message, WPARAM(usize::from(key.vk)), lparam) }.map_err(
        |error| ActionError::BackendUnavailable {
            detail: format!(
                "PostMessageW act_press keyboard message 0x{message:x} failed for hwnd 0x{:x}: {error}",
                hwnd_to_i64(hwnd)
            ),
        },
    )
}

#[cfg(windows)]
fn post_char_message(
    hwnd: windows::Win32::Foundation::HWND,
    char_code: u16,
    key: &KeySpec,
) -> Result<(), ActionError> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{
            Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW},
            WindowsAndMessaging::{PostMessageW, WM_CHAR},
        },
    };

    let scan = unsafe { MapVirtualKeyW(u32::from(key.vk), MAPVK_VK_TO_VSC) } & 0xff;
    let bits = 1_u32 | (scan << 16);
    let lparam = LPARAM(isize::try_from(bits).unwrap_or(isize::MAX));
    unsafe { PostMessageW(Some(hwnd), WM_CHAR, WPARAM(usize::from(char_code)), lparam) }.map_err(
        |error| ActionError::BackendUnavailable {
            detail: format!(
                "PostMessageW act_press WM_CHAR failed for hwnd 0x{:x}: {error}",
                hwnd_to_i64(hwnd)
            ),
        },
    )
}

#[cfg(windows)]
fn window_text(hwnd: windows::Win32::Foundation::HWND) -> Result<String, ActionError> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{
            SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_GETTEXT, WM_GETTEXTLENGTH,
        },
    };

    const TIMEOUT_MS: u32 = 250;
    let mut length_result = 0_usize;
    let length_lresult = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXTLENGTH,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            Some(&raw mut length_result),
        )
    };
    if length_lresult.0 == 0 && length_result == 0 {
        return Ok(String::new());
    }
    let capacity = length_result.saturating_add(1).min(1_048_576);
    let mut buffer = vec![0_u16; capacity];
    let mut copied = 0_usize;
    let copied_lresult = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buffer.len()),
            LPARAM(buffer.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            Some(&raw mut copied),
        )
    };
    if copied_lresult.0 == 0 && copied == 0 {
        return Ok(String::new());
    }
    let text_len = copied.min(buffer.len().saturating_sub(1));
    Ok(String::from_utf16_lossy(&buffer[..text_len]))
}

#[cfg(windows)]
fn edit_selection(hwnd: windows::Win32::Foundation::HWND) -> Option<(u32, u32)> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW},
    };

    const EM_GETSEL: u32 = 0x00B0;
    const TIMEOUT_MS: u32 = 250;
    let mut start = 0_u32;
    let mut end = 0_u32;
    let result = unsafe {
        SendMessageTimeoutW(
            hwnd,
            EM_GETSEL,
            WPARAM((&raw mut start) as usize),
            LPARAM((&raw mut end) as isize),
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            None,
        )
    };
    (result.0 != 0).then_some((start, end))
}

#[cfg(windows)]
fn window_class_name(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;

    let mut buffer = vec![0_u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)])
}

#[cfg(windows)]
fn hwnd_from_i64(hwnd: i64) -> Result<windows::Win32::Foundation::HWND, ActionError> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;

    if hwnd == 0 {
        return Err(ActionError::TargetInvalid {
            detail: "act_press PostMessage target hwnd is null".to_owned(),
        });
    }
    Ok(HWND(hwnd as isize as *mut c_void))
}

#[cfg(windows)]
fn hwnd_to_i64(hwnd: windows::Win32::Foundation::HWND) -> i64 {
    hwnd.0 as isize as i64
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn ctrl() -> KeySpec {
        KeySpec {
            label: "ctrl",
            vk: 0x11,
            char_code: None,
            ctrl_char_code: None,
        }
    }

    fn letter(byte: u8) -> KeySpec {
        KeySpec {
            label: "letter",
            vk: u16::from(byte.to_ascii_uppercase()),
            char_code: Some(u16::from(byte.to_ascii_lowercase())),
            ctrl_char_code: Some(u16::from(byte.to_ascii_uppercase() - b'A' + 1)),
        }
    }

    // Only the clipboard/undo edit shortcuts that EDIT/RICHEDIT controls
    // implement as window messages map to a semantic message; everything else
    // returns None so the caller fails loud instead of corrupting the target.
    #[test]
    fn edit_semantic_message_maps_only_paste_cut_undo() {
        assert_eq!(
            edit_semantic_message(&[ctrl(), letter(b'v')]),
            Some((0x0302, "WM_PASTE"))
        );
        assert_eq!(
            edit_semantic_message(&[ctrl(), letter(b'x')]),
            Some((0x0300, "WM_CUT"))
        );
        assert_eq!(
            edit_semantic_message(&[ctrl(), letter(b'z')]),
            Some((0x00C7, "EM_UNDO"))
        );
    }

    #[test]
    fn edit_semantic_message_refuses_copy_and_app_accelerators() {
        // Copy lives in the clipboard (unverifiable via the HWND-text SoT) and
        // app accelerators have no edit message: both must fail loud.
        assert_eq!(edit_semantic_message(&[ctrl(), letter(b'c')]), None);
        assert_eq!(edit_semantic_message(&[ctrl(), letter(b'n')]), None);
        assert_eq!(edit_semantic_message(&[ctrl(), letter(b's')]), None);
        // A bare letter with no Ctrl is not an edit shortcut.
        assert_eq!(edit_semantic_message(&[letter(b'v')]), None);
        // Three-key chords (e.g. Ctrl+Shift+Z redo) are out of scope here.
        assert_eq!(
            edit_semantic_message(&[ctrl(), ctrl(), letter(b'z')]),
            None
        );
    }
}
