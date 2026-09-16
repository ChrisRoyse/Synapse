use synapse_core::{ForegroundContext, Point};

use crate::{A11yResult, UIElement, platform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundActivationIntent {
    OperatorRequested { caller: &'static str },
    LeaseContextRestore { caller: &'static str },
}

impl ForegroundActivationIntent {
    #[must_use]
    pub const fn caller(self) -> &'static str {
        match self {
            Self::OperatorRequested { caller } | Self::LeaseContextRestore { caller } => caller,
        }
    }

    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::OperatorRequested { .. } => "operator_requested",
            Self::LeaseContextRestore { .. } => "lease_context_restore",
        }
    }
}

/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when Windows has no foreground HWND, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn focused_window() -> A11yResult<UIElement> {
    platform::focused_window()
}

/// Returns process/title metadata for the current foreground native window.
///
/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when Windows has no foreground HWND, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn current_foreground_context() -> A11yResult<ForegroundContext> {
    platform::current_foreground_context()
}

/// Background-safe window move/resize outcome (#1349).
#[cfg(windows)]
pub use platform::WindowBoundsOutcome;

/// Move and/or resize a top-level window WITHOUT activating it, reading the
/// resulting outer rect back so the caller gets requested-vs-actual bounds and
/// minimized state (#1349). See `platform::set_window_bounds`.
///
/// # Errors
///
/// Returns an error if the HWND is not a live window, a requested dimension is
/// not positive, or the underlying Win32 calls fail.
#[cfg(windows)]
pub fn set_window_bounds(
    hwnd: i64,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
) -> A11yResult<platform::WindowBoundsOutcome> {
    platform::set_window_bounds(hwnd, x, y, width, height)
}

/// Returns a top-level UIA window for a native HWND.
///
/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when the HWND is invalid, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn window_from_hwnd(hwnd: i64) -> A11yResult<UIElement> {
    platform::window_from_hwnd(hwnd)
}

/// Returns milliseconds since the interactive session last saw keyboard,
/// mouse, or other input (`GetLastInputInfo`).
///
/// Synthetic input injected with `SendInput` (agent actions) also resets the
/// counter, so this measures "the session is receiving input", not "the human
/// is present"; cadence capture (#838) separates the two.
///
/// # Errors
///
/// Returns a structured internal error when `GetLastInputInfo` fails, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn millis_since_last_input() -> A11yResult<u64> {
    platform::millis_since_last_input()
}

/// Requests foreground focus for a top-level native HWND.
///
/// # Errors
///
/// Always returns `FOREGROUND_ACTIVATION_REFUSED`; callers that truly intend to
/// activate the foreground must use [`focus_window_with_intent`] and pass the
/// already-validated intent explicitly.
pub fn focus_window(hwnd: i64) -> A11yResult<()> {
    tracing::warn!(
        code = synapse_core::error_codes::FOREGROUND_ACTIVATION_REFUSED,
        hwnd,
        "implicit foreground activation refused"
    );
    Err(crate::A11yError::foreground_activation_refused(format!(
        "implicit focus_window(hwnd=0x{hwnd:x}) is disabled; use focus_window_with_intent after validating foreground intent"
    )))
}

/// Requests foreground focus for a top-level native HWND with explicit intent.
///
/// # Errors
///
/// Returns `FOREGROUND_ACTIVATION_REFUSED` if the caller has not already proven
/// the intent at its boundary, a structured UIA error when Windows rejects the
/// foreground request, or `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn focus_window_with_intent(hwnd: i64, intent: ForegroundActivationIntent) -> A11yResult<()> {
    platform::focus_window_with_intent(hwnd, intent)
}

/// Returns whether a top-level native HWND is minimized/iconic.
///
/// # Errors
///
/// Returns a structured UIA error when the HWND is invalid, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn is_window_minimized(hwnd: i64) -> A11yResult<bool> {
    platform::is_window_minimized(hwnd)
}

/// Returns whether a native HWND is visible.
///
/// # Errors
///
/// Returns a structured UIA error when the HWND is invalid, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn is_window_visible(hwnd: i64) -> A11yResult<bool> {
    platform::is_window_visible(hwnd)
}

/// Returns whether a native HWND is its own top-level root window.
///
/// # Errors
///
/// Returns a structured UIA error when the HWND is invalid, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn is_top_level_window(hwnd: i64) -> A11yResult<bool> {
    platform::is_top_level_window(hwnd)
}

/// Returns the top-level root HWND for a native HWND.
///
/// # Errors
///
/// Returns a structured UIA error when the HWND is invalid, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn top_level_root_hwnd(hwnd: i64) -> A11yResult<i64> {
    platform::top_level_root_hwnd(hwnd)
}

/// Requests that a top-level native HWND close by posting `WM_CLOSE`.
///
/// # Errors
///
/// Returns a structured UIA error when Windows rejects the close message, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn close_window(hwnd: i64) -> A11yResult<()> {
    platform::close_window(hwnd)
}

/// Returns the top-level UIA window for a process id.
///
/// # Errors
///
/// Returns `A11Y_NO_FOREGROUND` when no visible window exists for the pid, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn window_for_process(pid: u32) -> A11yResult<UIElement> {
    platform::window_for_process(pid)
}

/// Resolves a top-level UIA window name to its native HWND without returning
/// COM elements.
///
/// # Errors
///
/// Returns a structured UIA error for OS failures, or `A11Y_NOT_AVAILABLE` on
/// non-Windows platforms.
pub fn top_level_window_hwnd_by_name(name: impl Into<String>) -> A11yResult<Option<i64>> {
    platform::top_level_window_hwnd_by_name(name.into())
}

/// Returns foreground-window process, title, bounds, and display metadata.
///
/// # Errors
///
/// Returns a structured UIA error when the HWND cannot be inspected, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn foreground_context(hwnd: i64) -> A11yResult<ForegroundContext> {
    platform::foreground_context(hwnd)
}

/// Returns context for visible top-level windows on the current desktop.
///
/// # Errors
///
/// Returns `A11Y_NOT_AVAILABLE` on non-Windows platforms. Individual windows
/// that disappear during enumeration are skipped.
pub fn visible_top_level_window_contexts() -> A11yResult<Vec<ForegroundContext>> {
    platform::visible_top_level_window_contexts()
}

/// Returns the currently focused UIA element with cached basic properties.
///
/// # Errors
///
/// Returns a structured UIA error when the focused element cannot be resolved,
/// or `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn focused_element() -> A11yResult<UIElement> {
    platform::focused_element()
}

/// Returns the UIA element at a screen-space point.
///
/// # Errors
///
/// Returns a structured UIA error when hit testing fails, or
/// `A11Y_NOT_AVAILABLE` on non-Windows platforms.
pub fn element_from_point(point: Point) -> A11yResult<UIElement> {
    platform::element_from_point(point)
}
