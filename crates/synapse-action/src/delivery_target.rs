//! Intended OS-input delivery target (OS-input safety invariant).
//!
//! Every OS-level input batch (`SendInput`, Enigo key, `SetPhysicalCursorPos`)
//! is delivered by Windows to whatever window is foreground at that instant.
//! An action therefore carries the window it was *meant* for — the top-level
//! root HWND plus owning pid — and the software backend re-verifies the live
//! foreground against it immediately before each batch. Keyboard/text input
//! with no intended target is refused rather than typed into whatever the human
//! happens to be using.

use crate::{ActionError, ActionResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputDeliveryTarget {
    /// Top-level root HWND (`GetAncestor(.., GA_ROOT)`) in Synapse wire form.
    pub root_hwnd: i64,
    /// Process id owning `root_hwnd`.
    pub pid: u32,
    /// Which act path resolved the target (audit/diagnostics only).
    pub source: &'static str,
}

impl InputDeliveryTarget {
    /// Resolves an HWND (any descendant is accepted) to its live root HWND and
    /// owning pid.
    ///
    /// # Errors
    ///
    /// Returns `ACTION_TARGET_INVALID` when the HWND is not a live window, or
    /// `ACTION_BACKEND_UNAVAILABLE` on non-Windows platforms.
    pub fn resolve_window(hwnd: i64, source: &'static str) -> ActionResult<Self> {
        resolve_window_impl(hwnd, source)
    }
}

#[cfg(windows)]
fn resolve_window_impl(hwnd: i64, source: &'static str) -> ActionResult<InputDeliveryTarget> {
    let identity =
        synapse_a11y::window_root_identity(hwnd).map_err(|error| ActionError::TargetInvalid {
            detail: format!(
                "intended input target HWND {hwnd:#x} (source={source}) could not be resolved to a live root window: {error}"
            ),
        })?;
    Ok(InputDeliveryTarget {
        root_hwnd: identity.root_hwnd,
        pid: identity.pid,
        source,
    })
}

#[cfg(not(windows))]
fn resolve_window_impl(hwnd: i64, source: &'static str) -> ActionResult<InputDeliveryTarget> {
    Err(ActionError::BackendUnavailable {
        detail: format!(
            "intended input target HWND {hwnd:#x} (source={source}) requires Windows to resolve"
        ),
    })
}
