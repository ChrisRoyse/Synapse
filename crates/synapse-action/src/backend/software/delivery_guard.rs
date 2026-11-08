//! Delivery-boundary invariant for OS-level input (OS-input safety).
//!
//! Windows delivers `SendInput`, Enigo key events and `SetPhysicalCursorPos`
//! to whatever window is foreground at the instant of delivery, not to the
//! window an agent had in mind. Each batch is therefore checked immediately
//! before delivery:
//!
//! * keyboard/text press: the intended target (root HWND + pid) must be known
//!   and `GetAncestor(GetForegroundWindow(), GA_ROOT)` + its pid must equal it;
//!   otherwise `ACTION_INPUT_TARGET_REQUIRED` / `ACTION_FOREGROUND_LOST` /
//!   `ACTION_HUMAN_FULLSCREEN_FOREGROUND`;
//! * pointer move/press with a target: the foreground root must be the target
//!   and, for button-down/wheel, the root window under the cursor must belong
//!   to the target process;
//! * pointer with no target: refused while the human is in a fullscreen/busy/
//!   presentation app (`SHQueryUserNotificationState`);
//! * key-up / button-up batches are releases and always allowed (safety).
//!
//! The intended target is installed for the duration of one backend `execute`
//! call on the blocking dispatch thread (`enter_scope`). Cost per batch: three
//! constant-time USER32 reads; the shell notification-state query runs only on
//! a mismatch or for untargeted pointer input, memoized per scope by the
//! foreground root it was read under (PC-06 hoist).

use std::cell::RefCell;

use synapse_core::error_codes;
use windows::Win32::{
    Foundation::POINT,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        },
        WindowsAndMessaging::GetCursorPos,
    },
};

use crate::{ActionError, InputDeliveryTarget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PointerDelivery {
    Move,
    Press,
}

#[derive(Debug, Default)]
struct DeliveryScope {
    target: Option<InputDeliveryTarget>,
    /// Foreground root under which the notification state was last read as
    /// non-protecting; reused while the foreground root is unchanged.
    unprotected_foreground_root: Option<i64>,
}

thread_local! {
    static SCOPE: RefCell<Option<DeliveryScope>> = const { RefCell::new(None) };
}

/// Restores the previous scope when the backend `execute` call ends.
pub(super) struct DeliveryScopeGuard {
    previous: Option<DeliveryScope>,
}

impl Drop for DeliveryScopeGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SCOPE.with(|scope| *scope.borrow_mut() = previous);
    }
}

/// Installs the intended delivery target for the current blocking dispatch.
pub(super) fn enter_scope(target: Option<InputDeliveryTarget>) -> DeliveryScopeGuard {
    let previous = SCOPE.with(|scope| {
        scope.borrow_mut().replace(DeliveryScope {
            target,
            unprotected_foreground_root: None,
        })
    });
    DeliveryScopeGuard { previous }
}

fn current_target() -> Option<InputDeliveryTarget> {
    SCOPE.with(|scope| {
        scope
            .borrow()
            .as_ref()
            .and_then(|scope| scope.target.clone())
    })
}

/// Verifies a keyboard/text press batch may be delivered right now.
pub(super) fn ensure_keyboard_press(detail: &str) -> Result<(), ActionError> {
    let Some(target) = current_target() else {
        let foreground = describe_foreground();
        tracing::warn!(
            code = error_codes::ACTION_INPUT_TARGET_REQUIRED,
            detail,
            foreground = %foreground,
            "refused keyboard input: no intended target window"
        );
        return Err(ActionError::InputTargetRequired {
            detail: format!(
                "code={} {detail}: keyboard/text input has no intended target window (root HWND + pid), so Synapse refuses to type into whatever is foreground ({foreground}). Pass window_hwnd, set a session target, or focus an explicit window first; prefer a background route (browser_dom/CDP/PostMessage).",
                error_codes::ACTION_INPUT_TARGET_REQUIRED
            ),
        });
    };
    ensure_foreground_is_target(&target, detail)
}

/// Verifies a pointer batch may be delivered right now.
pub(super) fn ensure_pointer(kind: PointerDelivery, detail: &str) -> Result<(), ActionError> {
    match current_target() {
        Some(target) => {
            ensure_foreground_is_target(&target, detail)?;
            if kind == PointerDelivery::Press {
                ensure_cursor_over_target(&target, detail)?;
            }
            Ok(())
        }
        None => ensure_human_not_in_protected_foreground(detail),
    }
}

/// Classifies a raw `SendInput` batch and runs the matching check. Batches
/// made only of key-up / button-up / no-op records are releases and pass.
pub(super) fn ensure_send_input_batch(inputs: &[INPUT], detail: &str) -> Result<(), ActionError> {
    let mut keyboard_press = false;
    let mut pointer_press = false;
    let mut pointer_move = false;
    for input in inputs {
        if input.r#type == INPUT_KEYBOARD {
            // SAFETY: `r#type == INPUT_KEYBOARD` selects the `ki` union arm.
            let flags = unsafe { input.Anonymous.ki.dwFlags };
            if !flags.contains(KEYEVENTF_KEYUP) {
                keyboard_press = true;
            }
        } else if input.r#type == INPUT_MOUSE {
            // SAFETY: `r#type == INPUT_MOUSE` selects the `mi` union arm.
            let flags = unsafe { input.Anonymous.mi.dwFlags };
            if flags.contains(MOUSEEVENTF_LEFTDOWN)
                || flags.contains(MOUSEEVENTF_RIGHTDOWN)
                || flags.contains(MOUSEEVENTF_MIDDLEDOWN)
                || flags.contains(MOUSEEVENTF_XDOWN)
                || flags.contains(MOUSEEVENTF_WHEEL)
                || flags.contains(MOUSEEVENTF_HWHEEL)
            {
                pointer_press = true;
            }
            if flags.contains(MOUSEEVENTF_MOVE) || flags.contains(MOUSEEVENTF_ABSOLUTE) {
                pointer_move = true;
            }
        }
    }
    if keyboard_press {
        ensure_keyboard_press(detail)?;
    }
    if pointer_press {
        ensure_pointer(PointerDelivery::Press, detail)?;
    } else if pointer_move {
        ensure_pointer(PointerDelivery::Move, detail)?;
    }
    Ok(())
}

fn ensure_foreground_is_target(
    target: &InputDeliveryTarget,
    detail: &str,
) -> Result<(), ActionError> {
    let foreground = match synapse_a11y::foreground_root_identity() {
        Ok(foreground) => foreground,
        Err(error) => {
            return Err(ActionError::ForegroundLost {
                detail: format!(
                    "code={} {detail}: foreground could not be read immediately before delivery (target root_hwnd={:#x} pid={} source={}): {error}",
                    error_codes::ACTION_FOREGROUND_LOST,
                    target.root_hwnd,
                    target.pid,
                    target.source
                ),
            });
        }
    };
    if foreground.root_hwnd == target.root_hwnd && foreground.pid == target.pid {
        return Ok(());
    }
    Err(foreground_mismatch_error(
        target,
        foreground.root_hwnd,
        foreground.pid,
        detail,
    ))
}

fn ensure_cursor_over_target(
    target: &InputDeliveryTarget,
    detail: &str,
) -> Result<(), ActionError> {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `point` is a valid out pointer for the duration of the call.
    if let Err(error) = unsafe { GetCursorPos(&raw mut point) } {
        return Err(ActionError::ForegroundLost {
            detail: format!(
                "code={} {detail}: cursor position could not be read before pointer press: {error}",
                error_codes::ACTION_FOREGROUND_LOST
            ),
        });
    }
    let under = synapse_a11y::window_root_identity_at_point(point.x, point.y).map_err(|error| {
        ActionError::ForegroundLost {
            detail: format!(
                "code={} {detail}: window under cursor ({},{}) could not be read before pointer press: {error}",
                error_codes::ACTION_FOREGROUND_LOST,
                point.x,
                point.y
            ),
        }
    })?;
    // Same-process popups/menus are separate top-level roots; any other
    // process under the cursor would receive the press.
    if under.root_hwnd == target.root_hwnd || under.pid == target.pid {
        return Ok(());
    }
    tracing::warn!(
        code = error_codes::ACTION_FOREGROUND_LOST,
        detail,
        cursor_x = point.x,
        cursor_y = point.y,
        under_root_hwnd = under.root_hwnd,
        under_pid = under.pid,
        target_root_hwnd = target.root_hwnd,
        target_pid = target.pid,
        "refused pointer press: the window under the cursor is not the target"
    );
    Err(ActionError::ForegroundLost {
        detail: format!(
            "code={} {detail}: window under cursor ({},{}) is root_hwnd={:#x} pid={}, not the intended target root_hwnd={:#x} pid={} source={}",
            error_codes::ACTION_FOREGROUND_LOST,
            point.x,
            point.y,
            under.root_hwnd,
            under.pid,
            target.root_hwnd,
            target.pid,
            target.source
        ),
    })
}

fn foreground_mismatch_error(
    target: &InputDeliveryTarget,
    foreground_root_hwnd: i64,
    foreground_pid: u32,
    detail: &str,
) -> ActionError {
    let state = synapse_a11y::human_notification_state();
    let state_text = match &state {
        Ok(state) => state.as_str().to_owned(),
        Err(error) => format!("unreadable({error})"),
    };
    if matches!(&state, Ok(state) if state.protects_foreground()) {
        tracing::warn!(
            code = error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND,
            detail,
            notification_state = %state_text,
            foreground_root_hwnd,
            foreground_pid,
            target_root_hwnd = target.root_hwnd,
            target_pid = target.pid,
            "refused OS input: human is in a fullscreen foreground app that is not the target"
        );
        return ActionError::HumanFullscreenForeground {
            detail: format!(
                "code={} {detail}: human foreground root_hwnd={foreground_root_hwnd:#x} pid={foreground_pid} is a protected app (notification_state={state_text}) and not the intended target root_hwnd={:#x} pid={} source={}; no input was delivered",
                error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND,
                target.root_hwnd,
                target.pid,
                target.source
            ),
        };
    }
    tracing::warn!(
        code = error_codes::ACTION_FOREGROUND_LOST,
        detail,
        notification_state = %state_text,
        foreground_root_hwnd,
        foreground_pid,
        target_root_hwnd = target.root_hwnd,
        target_pid = target.pid,
        "refused OS input: foreground is not the intended target"
    );
    ActionError::ForegroundLost {
        detail: format!(
            "code={} {detail}: foreground root_hwnd={foreground_root_hwnd:#x} pid={foreground_pid} (notification_state={state_text}) is not the intended target root_hwnd={:#x} pid={} source={} immediately before delivery; no input was delivered",
            error_codes::ACTION_FOREGROUND_LOST,
            target.root_hwnd,
            target.pid,
            target.source
        ),
    }
}

fn ensure_human_not_in_protected_foreground(detail: &str) -> Result<(), ActionError> {
    let foreground_root = synapse_a11y::foreground_root_identity()
        .ok()
        .map(|identity| identity.root_hwnd);
    let memoized = SCOPE.with(|scope| {
        scope.borrow().as_ref().is_some_and(|scope| {
            foreground_root.is_some() && scope.unprotected_foreground_root == foreground_root
        })
    });
    if memoized {
        return Ok(());
    }
    let state = synapse_a11y::human_notification_state().map_err(|error| {
        ActionError::HumanFullscreenForeground {
            detail: format!(
                "code={} {detail}: SHQueryUserNotificationState could not be read before untargeted pointer input; refusing (fail closed): {error}",
                error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND
            ),
        }
    })?;
    if state.protects_foreground() {
        tracing::warn!(
            code = error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND,
            detail,
            notification_state = state.as_str(),
            foreground_root_hwnd = foreground_root.unwrap_or_default(),
            "refused untargeted pointer input: human is in a fullscreen foreground app"
        );
        return Err(ActionError::HumanFullscreenForeground {
            detail: format!(
                "code={} {detail}: pointer input has no intended target window and the human is in a protected foreground app (notification_state={}, foreground_root_hwnd={:#x}); no input was delivered",
                error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND,
                state.as_str(),
                foreground_root.unwrap_or_default()
            ),
        });
    }
    SCOPE.with(|scope| {
        if let Some(scope) = scope.borrow_mut().as_mut() {
            scope.unprotected_foreground_root = foreground_root;
        }
    });
    Ok(())
}

fn describe_foreground() -> String {
    match synapse_a11y::foreground_root_identity() {
        Ok(identity) => format!(
            "foreground root_hwnd={:#x} pid={}",
            identity.root_hwnd, identity.pid
        ),
        Err(error) => format!("foreground unreadable: {error}"),
    }
}
