//! #2056: routing for native background ACTIONS whose target window lives on a
//! session-owned hidden Win32 desktop.
//!
//! Session-owned desktops already back observation (`observe`/`find`/
//! `screenshot`) through a per-desktop worker **process**: the daemon spawns
//! itself with `STARTUPINFOW.lpDesktop` set to the owned desktop, so every
//! thread in that child — including the UIA MTA worker thread that initializes
//! COM — is connected to that desktop before any UI work happens. Microsoft
//! documents this as the third rule of desktop connection ("If a desktop name
//! was specified in the lpDesktop member of the STARTUPINFO structure that was
//! used when the process was created, the thread connects to the specified
//! desktop"), and it is strictly safer than `SetThreadDesktop`, which fails on
//! any thread that already owns windows or hooks.
//!
//! Actions were still executed and validated from the daemon's own (visible)
//! desktop. Microsoft documents that a desktop "contains user interface objects
//! such as windows", that window enumeration is per-desktop
//! (`EnumDesktopWindows` enumerates "all top-level windows associated with the
//! specified desktop"), and that "window messages can be sent only between
//! processes that are on the same desktop". So neither the `IsWindow`/UIA
//! resolution nor the `WM_SETTEXT` delivery could reach a hidden-desktop HWND
//! from the daemon, and `act` failed with `A11Y_NO_FOREGROUND ... not a valid
//! window` even though the HWND was physically alive on the owned desktop.
//!
//! This module resolves *which* session-owned desktop physically owns the
//! target HWND and hands the caller a route through that desktop's worker.
//! Deliberately out of scope, and left failing closed:
//!
//! - Raw `SendInput` / cursor tiers. Only one desktop at a time is the *input
//!   desktop*, and `SetCursorPos`/`GetCursorPos` are documented to require that
//!   "the input desktop must be the current desktop when you call" them. Raw
//!   input therefore cannot be aimed at a hidden desktop at all; it is refused
//!   by `ForegroundClickPolicy::refuse_hidden_desktop`
//!   (`FOREGROUND_ACTIVATION_REFUSED`), never rerouted here.
//! - Activating or switching desktops (`SwitchDesktop`), and any fallback to
//!   the human's foreground.

use rmcp::ErrorData;
use synapse_core::{ElementId, error_codes};

use crate::m1::mcp_error;

/// A resolved background-action route: the session-owned desktop that
/// physically owns the target HWND, plus the "before" Source-of-Truth read
/// taken on that desktop by an independent worker process.
#[derive(Clone, Debug)]
pub(crate) struct HiddenDesktopValueRoute {
    pub(crate) desktop_name: String,
    pub(crate) before: synapse_a11y::ElementValueReadback,
}

impl HiddenDesktopValueRoute {
    /// Audit/ledger tier name for this route. Recorded verbatim in the tool
    /// response and therefore in the `CF_ACTION_LOG` row.
    pub(crate) fn backend_tier_used(&self, method: &str) -> String {
        if method == "uia_native_window_text_message" {
            "wm_settext_hidden_desktop_worker".to_owned()
        } else {
            "uia_hidden_desktop_worker".to_owned()
        }
    }

    pub(crate) fn route_label(&self) -> String {
        format!("hidden_desktop_worker:{}", self.desktop_name)
    }
}

/// Probes each session-owned desktop for physical ownership of `element_id`'s
/// HWND and returns the owning desktop's route.
///
/// - `Ok(Some(route))` — a session-owned desktop owns the HWND; the caller must
///   perform the action through that desktop's worker.
/// - `Ok(None)` — no session-owned desktop owns the HWND *and* the HWND is a
///   live window on the daemon's own desktop, so the ordinary background tiers
///   are the correct route.
/// - `Err(..)` — fail loud. Either the owning desktop rejected the element with
///   a precise reason (unsupported UIA pattern, stale element, disabled target),
///   or the HWND is alive on no desktop at all (stale hidden HWND).
///
/// There is no fallback: a desktop that claims the window decides the outcome.
pub(crate) fn resolve_hidden_desktop_value_route(
    tool: &'static str,
    element_id: &ElementId,
    desktop_names: &[String],
) -> Result<Option<HiddenDesktopValueRoute>, ErrorData> {
    if desktop_names.is_empty() {
        return Ok(None);
    }
    let hwnd = element_hwnd(tool, element_id)?;
    let mut misses = Vec::with_capacity(desktop_names.len());
    for desktop_name in desktop_names {
        match crate::desktop_worker::hidden_desktop_element_value(desktop_name, element_id) {
            Ok(before) => {
                tracing::info!(
                    code = "M2_HIDDEN_DESKTOP_ACTION_ROUTE_RESOLVED",
                    tool,
                    element_id = %element_id,
                    hwnd,
                    desktop_name = desktop_name.as_str(),
                    method = before.method.as_str(),
                    required_foreground = false,
                    source_of_truth =
                        "session-owned desktop worker IsWindow + UIA/Win32 value readback",
                    "readback=hidden_desktop_action_route desktop owns the target HWND"
                );
                return Ok(Some(HiddenDesktopValueRoute {
                    desktop_name: desktop_name.clone(),
                    before,
                }));
            }
            Err(error) if hidden_desktop_target_miss(&error) => {
                misses.push(format!("{desktop_name}: {}", error.message));
            }
            Err(error) => return Err(error),
        }
    }
    if window_live_on_daemon_desktop(hwnd) {
        tracing::debug!(
            code = "M2_HIDDEN_DESKTOP_ACTION_ROUTE_NOT_OWNED",
            tool,
            element_id = %element_id,
            hwnd,
            probed_desktops = desktop_names.len(),
            "target HWND is live on the daemon desktop; using the ordinary background tiers"
        );
        return Ok(None);
    }
    Err(stale_hidden_hwnd_error(
        tool,
        element_id,
        hwnd,
        desktop_names,
        &misses,
    ))
}

/// Only a `TARGET_WINDOW_NOT_FOUND` verdict means "this desktop does not own
/// the HWND". Every other worker error came from a desktop that *did* own it
/// and must be surfaced verbatim.
fn hidden_desktop_target_miss(error: &ErrorData) -> bool {
    matches!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str),
        Some(error_codes::TARGET_WINDOW_NOT_FOUND)
    )
}

#[cfg(windows)]
fn window_live_on_daemon_desktop(hwnd: i64) -> bool {
    synapse_capture::validate_hwnd(hwnd).is_ok()
}

#[cfg(not(windows))]
const fn window_live_on_daemon_desktop(_hwnd: i64) -> bool {
    false
}

fn element_hwnd(tool: &'static str, element_id: &ElementId) -> Result<i64, ErrorData> {
    Ok(element_id
        .parts()
        .map_err(|error| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("{tool} element id {element_id} is malformed: {error}"),
            )
        })?
        .hwnd)
}

fn stale_hidden_hwnd_error(
    tool: &'static str,
    element_id: &ElementId,
    hwnd: i64,
    desktop_names: &[String],
    misses: &[String],
) -> ErrorData {
    tracing::error!(
        code = error_codes::A11Y_ELEMENT_STALE,
        tool,
        element_id = %element_id,
        hwnd,
        desktop_names = ?desktop_names,
        misses = ?misses,
        source_of_truth = "per-desktop worker IsWindow readback + daemon-desktop IsWindow readback",
        "stale hidden-desktop HWND: no session-owned desktop and not the daemon desktop owns the target window"
    );
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!(
            "{tool} target HWND {hwnd:#x} (element {element_id}) is stale: it is not a live window on any session-owned hidden desktop {desktop_names:?}, and it is not a live window on the daemon's own desktop either. Re-observe the target (observe/find) to obtain a fresh element id; Synapse will not activate or switch desktops to look for it."
        ),
        Some(serde_json::json!({
            "code": error_codes::A11Y_ELEMENT_STALE,
            "tool": tool,
            "reason": "stale_hidden_desktop_hwnd",
            "element_id": element_id.to_string(),
            "hwnd": hwnd,
            "probed_desktops": desktop_names,
            "per_desktop_misses": misses,
            "required_foreground": false,
            "source_of_truth": "per-desktop worker IsWindow readback",
        })),
    )
}
