//! Foreground delivery fence for the trusted-input lane (issue #1830).
//!
//! `SendInput` has no destination argument: it delivers to whatever window
//! holds the foreground at the instant it runs. The foreground lane's contract
//! is that `ok` means "delivered to the bound target", but nothing in the
//! dispatch path used to read the foreground back, so a keystroke sequence
//! could report five successes while typing into an unrelated application.
//!
//! That is not hypothetical. On 2026-07-25 a `focus_window` → `key` → `key` →
//! `type` → `key` sequence returned `ok` five times and delivered nothing: the
//! activation genuinely succeeded, but each following call sat ~111 s behind
//! the exclusive-router stall (#1806/#1829), and by dispatch time the human had
//! long since taken the foreground back. The typed string and an Enter went to
//! a terminal at a shell prompt.
//!
//! The fence closes that hole structurally. [`arm`] records the window a
//! verified activation actually put in the foreground; every foreground-tier
//! dispatch then calls [`ensure`], which re-reads `GetForegroundWindow()` and
//! refuses with `ACTION_FOREGROUND_LOST` when the destination is not the armed
//! window — naming the window that *did* hold the foreground, so a near-miss is
//! attributable after the fact instead of silent.
//!
//! Fail-closed direction matters: an unarmed fence permits dispatch (no
//! activation was ever claimed, so there is no expectation to violate), but an
//! armed fence that cannot read the foreground refuses. Never the reverse.

use std::sync::{Mutex, OnceLock};

use rmcp::{ErrorData, model::ErrorCode};
use serde_json::{Value, json};
use synapse_core::error_codes;

const SOURCE_OF_TRUTH: &str = "GetForegroundWindow() read immediately before input dispatch";

/// The window a verified foreground activation left in the foreground.
#[derive(Debug, Clone)]
pub(crate) struct ArmedForeground {
    pub hwnd: i64,
    pub pid: u32,
    pub process_name: String,
    pub window_title: String,
    pub armed_by: &'static str,
}

fn cell() -> &'static Mutex<Option<ArmedForeground>> {
    static CELL: OnceLock<Mutex<Option<ArmedForeground>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn guard() -> std::sync::MutexGuard<'static, Option<ArmedForeground>> {
    cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Arms the fence with the window a foreground activation verifiably reached.
///
/// Called only after `act_focus_window`'s separate `GetForegroundWindow()`
/// readback has confirmed the move, so the armed value is observed reality,
/// never the requested target.
pub(crate) fn arm(armed: ArmedForeground) {
    tracing::info!(
        code = "M2_FOREGROUND_FENCE_ARMED",
        hwnd = armed.hwnd,
        pid = armed.pid,
        process_name = %armed.process_name,
        window_title = %armed.window_title,
        armed_by = armed.armed_by,
        "foreground delivery fence armed to the verified foreground window"
    );
    *guard() = Some(armed);
}

/// Clears the fence when the foreground claim is given up.
///
/// Releasing the input lease ends the claim that any particular window owns
/// trusted input, so leaving the fence armed would refuse later legitimate
/// dispatch from a fresh activation.
pub(crate) fn disarm(reason: &str) {
    let previous = guard().take();
    if let Some(previous) = previous {
        tracing::info!(
            code = "M2_FOREGROUND_FENCE_DISARMED",
            reason,
            hwnd = previous.hwnd,
            armed_by = previous.armed_by,
            "foreground delivery fence disarmed"
        );
    }
}

/// Currently armed expectation, for diagnostics and response readback.
pub(crate) fn armed() -> Option<ArmedForeground> {
    guard().clone()
}

/// Refuses dispatch unless the live foreground is still the armed window.
///
/// `stage` names the exact dispatch boundary so a refusal says which physical
/// step was stopped, matching the operator-panic boundary convention.
pub(crate) fn ensure(stage: &'static str) -> Result<(), ErrorData> {
    let Some(expected) = armed() else {
        return Ok(());
    };
    // The fence's validity is exactly the input lease's validity: a released
    // or expired lease ends the foreground claim, and foreground dispatch is
    // independently gated on holding one. Auto-disarming here keeps a stale
    // expectation from refusing a later session's legitimate activation
    // without needing every release path to remember to call `disarm`.
    if !synapse_action::lease::status().held {
        disarm("foreground_input_lease_not_held");
        return Ok(());
    }
    #[cfg(windows)]
    {
        let actual = synapse_a11y::current_foreground_context().map_err(|error| {
            // Armed but unreadable: refuse. An unverifiable destination is
            // exactly the case this fence exists to stop.
            tracing::error!(
                code = error_codes::ACTION_FOREGROUND_LOST,
                stage,
                expected_hwnd = expected.hwnd,
                error = %error,
                "foreground fence could not read the live foreground before dispatch"
            );
            ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "foreground input refused at {stage}: the live foreground could not be read back, so the destination of this input is unverified (expected hwnd 0x{:x} {})",
                    expected.hwnd, expected.process_name
                ),
                Some(json!({
                    "code": error_codes::ACTION_FOREGROUND_LOST,
                    "stage": stage,
                    "expected": fence_window_json(&expected),
                    "actual": Value::Null,
                    "source_of_truth": SOURCE_OF_TRUTH,
                    "readback_error": error.to_string(),
                    "resolution": "re-run act operation=invoke verb=focus_window and retry; never dispatch trusted input to an unverified window",
                })),
            )
        })?;
        if actual.hwnd != expected.hwnd {
            tracing::error!(
                code = error_codes::ACTION_FOREGROUND_LOST,
                stage,
                expected_hwnd = expected.hwnd,
                expected_process = %expected.process_name,
                actual_hwnd = actual.hwnd,
                actual_pid = actual.pid,
                actual_process = %actual.process_name,
                actual_title = %actual.window_title,
                "refused foreground input: the foreground moved away from the armed window before dispatch"
            );
            return Err(ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "foreground input refused at {stage}: the foreground is hwnd 0x{:x} ({} — {:?}), not the armed window hwnd 0x{:x} ({}). Dispatching would have delivered this input to an unrelated window.",
                    actual.hwnd,
                    actual.process_name,
                    actual.window_title,
                    expected.hwnd,
                    expected.process_name
                ),
                Some(json!({
                    "code": error_codes::ACTION_FOREGROUND_LOST,
                    "stage": stage,
                    "expected": fence_window_json(&expected),
                    "actual": {
                        "hwnd": actual.hwnd,
                        "pid": actual.pid,
                        "process_name": actual.process_name,
                        "window_title": actual.window_title,
                    },
                    "source_of_truth": SOURCE_OF_TRUTH,
                    "resolution": "re-run act operation=invoke verb=focus_window to re-acquire the foreground, then retry the input",
                })),
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = stage;
        Ok(())
    }
}

#[cfg(windows)]
fn fence_window_json(armed: &ArmedForeground) -> Value {
    json!({
        "hwnd": armed.hwnd,
        "pid": armed.pid,
        "process_name": armed.process_name,
        "window_title": armed.window_title,
        "armed_by": armed.armed_by,
    })
}
