//! Human-foreground protection primitives (OS-input safety).
//!
//! The human operator may be using a fullscreen application (a game, a
//! presentation) in the OS foreground while agents drive other windows in the
//! background. Every focus change and every OS-level input batch must first
//! prove it cannot land in, or steal focus from, a window the agent did not
//! target. These functions read the live Win32 state at the call boundary;
//! nothing here is cached across calls.

use std::ffi::c_void;

use synapse_core::win32_hwnd::{hwnd_from_wire, hwnd_to_wire};
use windows::Win32::{
    Foundation::{HWND, POINT},
    UI::{
        Shell::{
            QUNS_ACCEPTS_NOTIFICATIONS, QUNS_APP, QUNS_BUSY, QUNS_NOT_PRESENT,
            QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN,
            SHQueryUserNotificationState,
        },
        WindowsAndMessaging::{
            GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, IsWindow,
            WindowFromPoint,
        },
    },
};

use crate::{A11yError, A11yResult, HumanNotificationState, WindowRootIdentity};

/// Resolves a window to its top-level root HWND and owning process id.
///
/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when the HWND is not a live window or its owner
/// process cannot be read.
pub fn window_root_identity(hwnd: i64) -> A11yResult<WindowRootIdentity> {
    let native = hwnd_from_wire(hwnd).ok_or_else(|| A11yError::NoForeground {
        detail: format!("HWND wire value {hwnd} is outside the canonical Win32 USER-handle range"),
    })?;
    let hwnd_native = HWND(native as *mut c_void);
    // SAFETY: IsWindow accepts any handle value and only reads USER state.
    if !unsafe { IsWindow(Some(hwnd_native)) }.as_bool() {
        return Err(A11yError::NoForeground {
            detail: format!("HWND {hwnd:#x} is not a live window"),
        });
    }
    root_identity_for_native(hwnd_native, "target")
}

/// Reads the live OS foreground window as a top-level root HWND + pid.
///
/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when Windows reports no foreground window.
pub fn foreground_root_identity() -> A11yResult<WindowRootIdentity> {
    // SAFETY: GetForegroundWindow has no preconditions.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return Err(A11yError::NoForeground {
            detail: "GetForegroundWindow returned null".to_owned(),
        });
    }
    root_identity_for_native(foreground, "foreground")
}

/// Reads the top-level root window (and pid) under a physical screen point.
///
/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when no window is under the point.
pub fn window_root_identity_at_point(x: i32, y: i32) -> A11yResult<WindowRootIdentity> {
    // SAFETY: WindowFromPoint only reads USER window state.
    let hwnd = unsafe { WindowFromPoint(POINT { x, y }) };
    if hwnd.0.is_null() {
        return Err(A11yError::NoForeground {
            detail: format!("WindowFromPoint({x},{y}) returned null"),
        });
    }
    root_identity_for_native(hwnd, "window_under_point")
}

/// Reads the shell's user-notification state (`SHQueryUserNotificationState`).
///
/// # Errors
///
/// Returns an internal error when the shell query fails; callers fail closed.
pub fn human_notification_state() -> A11yResult<HumanNotificationState> {
    // SAFETY: SHQueryUserNotificationState writes one enum value.
    let state = unsafe { SHQueryUserNotificationState() }.map_err(|error| {
        A11yError::internal(format!("SHQueryUserNotificationState failed: {error}"))
    })?;
    Ok(match state {
        QUNS_NOT_PRESENT => HumanNotificationState::NotPresent,
        QUNS_BUSY => HumanNotificationState::Busy,
        QUNS_RUNNING_D3D_FULL_SCREEN => HumanNotificationState::RunningD3dFullScreen,
        QUNS_PRESENTATION_MODE => HumanNotificationState::PresentationMode,
        QUNS_ACCEPTS_NOTIFICATIONS => HumanNotificationState::AcceptsNotifications,
        QUNS_QUIET_TIME => HumanNotificationState::QuietTime,
        QUNS_APP => HumanNotificationState::App,
        other => HumanNotificationState::Unknown(other.0),
    })
}

/// Refuses a focus change toward `target` when the human is in a protected
/// (fullscreen D3D / busy / presentation) foreground app that is not the
/// target. Returns the foreground identity read at the boundary on success.
///
/// # Errors
///
/// Returns `ACTION_HUMAN_FULLSCREEN_FOREGROUND` when the human foreground is
/// protected and not the target, or an internal/no-foreground error when the
/// live state cannot be read (fail closed).
pub fn ensure_human_foreground_allows_focus_change(
    target: WindowRootIdentity,
    caller: &'static str,
) -> A11yResult<WindowRootIdentity> {
    let foreground = foreground_root_identity()?;
    if foreground.root_hwnd == target.root_hwnd && foreground.pid == target.pid {
        return Ok(foreground);
    }
    let state = human_notification_state()?;
    if state.protects_foreground() {
        tracing::warn!(
            code = synapse_core::error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND,
            caller,
            notification_state = state.as_str(),
            foreground_root_hwnd = foreground.root_hwnd,
            foreground_pid = foreground.pid,
            target_root_hwnd = target.root_hwnd,
            target_pid = target.pid,
            "refused focus change: human is in a protected fullscreen foreground app"
        );
        return Err(A11yError::HumanFullscreenForeground {
            detail: format!(
                "code={} caller={caller} notification_state={} human_foreground_root_hwnd={:#x} human_foreground_pid={} target_root_hwnd={:#x} target_pid={}; Synapse never steals focus from a fullscreen/busy/presentation app. Use a background-capable route (browser_dom / CDP / PostMessage) or wait until the human leaves the fullscreen app.",
                synapse_core::error_codes::ACTION_HUMAN_FULLSCREEN_FOREGROUND,
                state.as_str(),
                foreground.root_hwnd,
                foreground.pid,
                target.root_hwnd,
                target.pid,
            ),
        });
    }
    Ok(foreground)
}

fn root_identity_for_native(hwnd: HWND, role: &'static str) -> A11yResult<WindowRootIdentity> {
    // SAFETY: GetAncestor reads USER state for a handle value.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let root = if root.0.is_null() { hwnd } else { root };
    let mut pid = 0_u32;
    // SAFETY: `pid` is a valid out pointer for the duration of the call.
    let thread = unsafe { GetWindowThreadProcessId(root, Some(&raw mut pid)) };
    if thread == 0 || pid == 0 {
        return Err(A11yError::NoForeground {
            detail: format!(
                "{role} root HWND {:#x} owner process could not be read",
                root.0 as isize
            ),
        });
    }
    Ok(WindowRootIdentity {
        root_hwnd: hwnd_to_wire(root.0 as isize),
        pid,
    })
}
