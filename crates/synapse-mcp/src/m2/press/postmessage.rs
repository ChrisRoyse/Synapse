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
    boundary: crate::m2::OperatorPanicActionBoundary,
) -> Result<HwndKeyboardTargetState, ErrorData> {
    post_key_sequence_impl(root_hwnd, keys, hold_ms, boundary).await
}

#[cfg(windows)]
async fn post_key_sequence_impl(
    root_hwnd: i64,
    keys: &[Key],
    hold_ms: u32,
    boundary: crate::m2::OperatorPanicActionBoundary,
) -> Result<HwndKeyboardTargetState, ErrorData> {
    let key_specs = keys
        .iter()
        .map(key_spec)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| action_error_to_mcp(&error))?;
    // #1332: a Shift+navigation chord extends the selection ONLY if the control
    // reads the OS Shift key-state, which PostMessage WM_KEYDOWN cannot set — the
    // caret would just move WITHOUT selecting (a silent false success). Fail loud
    // with the correct alternatives instead.
    if let Some(nav) = shift_navigation_chord(&key_specs) {
        return Err(action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage cannot extend a selection with Shift+{nav}: WM_KEYDOWN cannot set the OS Shift key-state the control reads, so the caret would move without selecting. Use target_act verb=set_selection for an exact background selection, or acquire the foreground input lease (backend=hardware) for real Shift+navigation."
            ),
        }));
    }
    let mut release_keys = false;
    let target_hwnd = {
        let target =
            best_keyboard_target(root_hwnd).map_err(|error| action_error_to_mcp(&error))?;
        if is_ctrl_a_shortcut(&key_specs) {
            boundary.ensure("immediately_before_postmessage_select_all")?;
            select_all_edit_target(&target).map_err(|error| action_error_to_mcp(&error))?;
            hwnd_to_i64(target.hwnd)
        } else if let Some(chord) = edit_control_chord(&key_specs) {
            // #1331: PostMessage WM_KEYDOWN cannot set OS key-state, so editing
            // chords (Ctrl+C/X/V/Z, Ctrl+Y/Ctrl+Shift+Z) never reach the control
            // as accelerators. Deliver the equivalent edit-control message directly
            // and verify the real effect (clipboard sequence number for copy/cut,
            // window-text change for paste/undo/redo) so we never falsely report
            // success on a chord the control ignored.
            boundary.ensure("immediately_before_postmessage_edit_chord")?;
            send_edit_control_chord(&target, &chord)
                .map_err(|error| action_error_to_mcp(&error))?;
            hwnd_to_i64(target.hwnd)
        } else if is_plain_printable_text(&key_specs) {
            boundary.ensure("immediately_before_postmessage_plain_text")?;
            post_char_messages(target.hwnd, &key_specs)
                .map_err(|error| action_error_to_mcp(&error))?;
            hwnd_to_i64(target.hwnd)
        } else {
            for spec in &key_specs {
                boundary.ensure("immediately_before_postmessage_key_down")?;
                post_key_message(
                    target.hwnd,
                    windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                    spec,
                )
                .map_err(|error| action_error_to_mcp(&error))?;
            }
            release_keys = true;
            boundary.ensure("immediately_before_postmessage_key_chars")?;
            post_char_messages(target.hwnd, &key_specs)
                .map_err(|error| action_error_to_mcp(&error))?;
            hwnd_to_i64(target.hwnd)
        }
    };
    if hold_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(hold_ms))).await;
    }
    let boundary_error = boundary
        .ensure("after_postmessage_key_hold_before_release")
        .err();
    let target = hwnd_from_i64(target_hwnd).map_err(|error| action_error_to_mcp(&error))?;
    let mut release_error = None;
    if release_keys {
        for spec in key_specs.iter().rev() {
            if let Err(error) = post_key_message(
                target,
                windows::Win32::UI::WindowsAndMessaging::WM_KEYUP,
                spec,
            ) && release_error.is_none()
            {
                release_error = Some(action_error_to_mcp(&error));
            }
        }
    }
    if let Some(error) = boundary_error {
        if let Some(release_error) = release_error {
            tracing::error!(
                code = synapse_core::error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                detail_code = "POSTMESSAGE_KEY_RELEASE_AFTER_OPERATOR_PANIC_FAILED",
                detail = %release_error,
                "operator panic superseded held PostMessage keys and best-effort key-up cleanup failed"
            );
        }
        return Err(error);
    }
    if let Some(error) = release_error {
        return Err(error);
    }
    let target = best_keyboard_target(root_hwnd).map_err(|error| action_error_to_mcp(&error))?;
    target_state(target).map_err(|error| action_error_to_mcp(&error))
}

#[cfg(not(windows))]
async fn post_key_sequence_impl(
    _root_hwnd: i64,
    _keys: &[Key],
    _hold_ms: u32,
    _boundary: crate::m2::OperatorPanicActionBoundary,
) -> Result<HwndKeyboardTargetState, ErrorData> {
    Err(action_error_to_mcp(&ActionError::BackendUnavailable {
        detail: "act_press PostMessage keyboard tier is only available on Windows".to_owned(),
    }))
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

#[cfg(windows)]
#[derive(Copy, Clone, Debug)]
struct EditChord {
    message: u32,
    name: &'static str,
    /// Redo has no standard message on a plain Edit control (EM_REDO is rich-edit
    /// only), so fail loud rather than silently no-op there.
    richedit_only: bool,
}

/// Maps a Ctrl-modified editing chord to the edit-control message that performs
/// it without OS key-state (#1331). Returns `None` for any non-editing chord so
/// the caller falls through to the generic key path.
#[cfg(windows)]
fn edit_control_chord(key_specs: &[KeySpec]) -> Option<EditChord> {
    const WM_CUT: u32 = 0x0300;
    const WM_COPY: u32 = 0x0301;
    const WM_PASTE: u32 = 0x0302;
    const WM_UNDO: u32 = 0x0304;
    const EM_REDO: u32 = 0x0454;

    let has_ctrl = key_specs.iter().any(|key| key.label == "ctrl");
    if !has_ctrl {
        return None;
    }
    if key_specs
        .iter()
        .any(|key| matches!(key.label, "alt" | "super"))
    {
        return None;
    }
    let has_shift = key_specs.iter().any(|key| key.label == "shift");
    let letters: Vec<u16> = key_specs
        .iter()
        .filter(|key| key.label == "letter")
        .filter_map(|key| key.char_code)
        .collect();
    if letters.len() != 1 {
        return None;
    }
    let letter = u8::try_from(letters[0]).ok()?.to_ascii_lowercase();
    match (letter, has_shift) {
        (b'c', false) => Some(EditChord {
            message: WM_COPY,
            name: "WM_COPY",
            richedit_only: false,
        }),
        (b'x', false) => Some(EditChord {
            message: WM_CUT,
            name: "WM_CUT",
            richedit_only: false,
        }),
        (b'v', false) => Some(EditChord {
            message: WM_PASTE,
            name: "WM_PASTE",
            richedit_only: false,
        }),
        (b'z', false) => Some(EditChord {
            message: WM_UNDO,
            name: "WM_UNDO",
            richedit_only: false,
        }),
        (b'z', true) | (b'y', false) => Some(EditChord {
            message: EM_REDO,
            name: "EM_REDO",
            richedit_only: true,
        }),
        _ => None,
    }
}

/// Windows clipboard sequence number, exposed so the act_press verify layer can
/// observe clipboard-mutating chords (#1331). Returns 0 off-Windows.
#[cfg(windows)]
pub(crate) fn clipboard_sequence_number() -> u32 {
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
pub(crate) fn clipboard_sequence_number() -> u32 {
    0
}

/// Detects a Shift + single-navigation-key chord (selection extension) that the
/// PostMessage tier cannot perform without OS key-state (#1332). Shift+letter
/// (uppercase typing) and Ctrl+Shift+Z (redo) are NOT navigation chords and pass
/// through. Returns the navigation key label when the chord must be refused.
#[cfg(windows)]
fn shift_navigation_chord(key_specs: &[KeySpec]) -> Option<&'static str> {
    if !key_specs.iter().any(|key| key.label == "shift") {
        return None;
    }
    let nav: Vec<&'static str> = key_specs
        .iter()
        .filter_map(|key| match key.label {
            label @ ("left" | "right" | "up" | "down" | "home" | "end" | "pageup" | "pagedown") => {
                Some(label)
            }
            _ => None,
        })
        .collect();
    (nav.len() == 1).then(|| nav[0])
}

/// Sends an edit-control message to the resolved keyboard target (#1331). The
/// real effect is verified by the act_press delta layer (clipboard sequence for
/// copy/cut, target text change for paste/undo/redo) so a chord the control
/// ignored fails loud as ACTION_NO_OBSERVED_DELTA rather than a false success.
/// Redo is refused here on non-rich-edit controls (no standard message).
#[cfg(windows)]
fn send_edit_control_chord(target: &WindowCandidate, chord: &EditChord) -> Result<(), ActionError> {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW},
    };

    let class_name = target.class_name.to_ascii_lowercase();
    if chord.richedit_only && !class_name.contains("richedit") {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage {} (redo) requires a rich-edit target; resolved hwnd 0x{:x} class {:?} has no standard redo message — use the foreground input lease for redo on this control",
                chord.name,
                hwnd_to_i64(target.hwnd),
                target.class_name
            ),
        });
    }

    const TIMEOUT_MS: u32 = 250;
    let mut message_result = 0_usize;
    let send_result = unsafe {
        SendMessageTimeoutW(
            target.hwnd,
            chord.message,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            Some(&raw mut message_result),
        )
    };
    if send_result.0 == 0 {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "act_press PostMessage {} (SendMessageTimeout) timed out or failed for hwnd 0x{:x}",
                chord.name,
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

    let native = synapse_core::win32_hwnd::hwnd_from_wire(hwnd).ok_or_else(|| {
        ActionError::TargetInvalid {
            detail: format!(
                "act_press PostMessage target hwnd {hwnd} is outside the canonical Win32 USER-handle range 1..=4294967295"
            ),
        }
    })?;
    Ok(HWND(native as *mut c_void))
}

#[cfg(windows)]
fn hwnd_to_i64(hwnd: windows::Win32::Foundation::HWND) -> i64 {
    synapse_core::win32_hwnd::hwnd_to_wire(hwnd.0 as isize)
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
mod chord_tests {
    use super::{KeySpec, edit_control_chord, shift_navigation_chord};

    fn spec(label: &'static str, char_code: Option<u16>) -> KeySpec {
        KeySpec {
            label,
            vk: 0,
            char_code,
            ctrl_char_code: None,
        }
    }

    fn letter(byte: u8) -> KeySpec {
        spec("letter", Some(u16::from(byte)))
    }

    // #1331: editing chords map to the correct edit-control message + SoT.
    #[test]
    fn ctrl_clipboard_chords_map_to_edit_messages() {
        let copy = edit_control_chord(&[spec("ctrl", None), letter(b'c')]).expect("ctrl+c");
        assert_eq!(copy.name, "WM_COPY");
        assert!(!copy.richedit_only);

        let cut = edit_control_chord(&[spec("ctrl", None), letter(b'x')]).expect("ctrl+x");
        assert_eq!(cut.name, "WM_CUT");
        assert!(!cut.richedit_only);

        let paste = edit_control_chord(&[spec("ctrl", None), letter(b'v')]).expect("ctrl+v");
        assert_eq!(paste.name, "WM_PASTE");
        assert!(!paste.richedit_only);

        let undo = edit_control_chord(&[spec("ctrl", None), letter(b'z')]).expect("ctrl+z");
        assert_eq!(undo.name, "WM_UNDO");
    }

    #[test]
    fn redo_chords_map_to_em_redo_and_are_richedit_only() {
        let ctrl_y = edit_control_chord(&[spec("ctrl", None), letter(b'y')]).expect("ctrl+y");
        assert_eq!(ctrl_y.name, "EM_REDO");
        assert!(ctrl_y.richedit_only);

        let ctrl_shift_z =
            edit_control_chord(&[spec("ctrl", None), spec("shift", None), letter(b'z')])
                .expect("ctrl+shift+z");
        assert_eq!(ctrl_shift_z.name, "EM_REDO");
    }

    #[test]
    fn non_editing_chords_are_not_remapped() {
        // No ctrl -> not an edit chord.
        assert!(edit_control_chord(&[letter(b'c')]).is_none());
        // Ctrl+A is select-all, handled by the dedicated EM_SETSEL path, not here.
        assert!(edit_control_chord(&[spec("ctrl", None), letter(b'a')]).is_none());
        // Ctrl+Alt+letter is not a clipboard chord.
        assert!(
            edit_control_chord(&[spec("ctrl", None), spec("alt", None), letter(b'c')]).is_none()
        );
    }

    // #1332: Shift+navigation is refused; uppercase typing and redo pass through.
    #[test]
    fn shift_navigation_refused_typing_and_redo_pass() {
        assert_eq!(
            shift_navigation_chord(&[spec("shift", None), spec("right", None)]),
            Some("right")
        );
        assert_eq!(
            shift_navigation_chord(&[spec("shift", None), spec("home", None)]),
            Some("home")
        );
        // Shift+letter is uppercase typing, not navigation.
        assert!(shift_navigation_chord(&[spec("shift", None), letter(b'a')]).is_none());
        // Ctrl+Shift+Z is redo, not navigation.
        assert!(
            shift_navigation_chord(&[spec("ctrl", None), spec("shift", None), letter(b'z')])
                .is_none()
        );
        // Plain navigation (no shift) is fine via PostMessage.
        assert!(shift_navigation_chord(&[spec("right", None)]).is_none());
    }
}
