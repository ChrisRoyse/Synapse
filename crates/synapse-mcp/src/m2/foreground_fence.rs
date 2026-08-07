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
//! The fence closes that hole structurally. [`arm`] records an exact target
//! identity, either from a verified activation or from an agent-owned session
//! target that must already be foreground before raw input is allowed. Every
//! foreground-tier dispatch then calls [`ensure`], which re-reads
//! `GetForegroundWindow()` and refuses with `ACTION_FOREGROUND_LOST` when the
//! destination is not the armed window — naming the window that *did* hold the
//! foreground, so a near-miss is attributable after the fact instead of
//! silent.
//!
//! Fail-closed direction matters: unarmed, lease-lost, and unreadable states
//! all refuse before delivery. Raw global input without an exact destination
//! is not a capability; it is input to whichever human window wins the race.

use std::sync::{Mutex, OnceLock};

use rmcp::{ErrorData, model::ErrorCode};
use serde_json::{Value, json};
use synapse_core::error_codes;

const SOURCE_OF_TRUTH: &str = "GetForegroundWindow() read immediately before input dispatch";
const UNARMED_DETAIL_CODE: &str = "M2_FOREGROUND_FENCE_UNARMED";

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

/// Arms the fence with a fully resolved top-level window identity.
///
/// Callers either provide a verified `GetForegroundWindow()` readback or use
/// [`arm_expected_target`] to resolve an agent-owned target without activating
/// it. In both cases the stored identity comes from live Win32 state.
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

/// Arms an exact agent-owned target without activating it.
///
/// Background UIA/CDP/PostMessage tiers do not consult the fence and remain
/// fully concurrent. If routing reaches global input, [`ensure`] requires this
/// exact root HWND to already be the real foreground; it never activates the
/// target or falls back to the human foreground.
pub(crate) fn arm_expected_target(
    hwnd: i64,
    armed_by: &'static str,
) -> Result<ArmedForeground, ErrorData> {
    #[cfg(windows)]
    {
        let root_hwnd = synapse_a11y::top_level_root_hwnd(hwnd).map_err(|error| {
            ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "foreground target binding refused: target hwnd 0x{hwnd:x} could not be normalized to a live top-level window: {error}"
                ),
                Some(json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "detail_code": "M2_FOREGROUND_TARGET_ROOT_INVALID",
                    "refused_before_delivery": true,
                    "target_hwnd": hwnd,
                    "source_of_truth": "IsWindow + GetAncestor(GA_ROOT) before foreground dispatch",
                    "readback_error_code": error.code(),
                    "readback_error": error.to_string(),
                    "remediation": "bind a live agent-owned window target and retry; never route raw input to an invalid or stale HWND",
                })),
            )
        })?;
        let context = synapse_a11y::foreground_context(root_hwnd).map_err(|error| {
            ErrorData::new(
                ErrorCode(-32099),
                format!(
                    "foreground target binding refused: exact target identity for root hwnd 0x{root_hwnd:x} could not be read: {error}"
                ),
                Some(json!({
                    "code": error_codes::ACTION_TARGET_INVALID,
                    "detail_code": "M2_FOREGROUND_TARGET_IDENTITY_UNREADABLE",
                    "refused_before_delivery": true,
                    "requested_hwnd": hwnd,
                    "target_root_hwnd": root_hwnd,
                    "source_of_truth": "GetWindowThreadProcessId + process/window identity before foreground dispatch",
                    "readback_error_code": error.code(),
                    "readback_error": error.to_string(),
                    "remediation": "bind a live agent-owned window target and retry; never route raw input to an unverified window identity",
                })),
            )
        })?;
        let armed = ArmedForeground {
            hwnd: context.hwnd,
            pid: context.pid,
            process_name: context.process_name,
            window_title: context.window_title,
            armed_by,
        };
        arm(armed.clone());
        Ok(armed)
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, armed_by);
        Err(ErrorData::new(
            ErrorCode(-32099),
            "foreground target binding requires Windows GetForegroundWindow identity",
            Some(json!({
                "code": error_codes::ACTION_TARGET_INVALID,
                "detail_code": "M2_FOREGROUND_TARGET_BINDING_UNAVAILABLE",
                "refused_before_delivery": true,
                "source_of_truth": "Windows foreground target identity",
                "remediation": "use a supported background target route on this platform",
            })),
        ))
    }
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
        #[cfg(windows)]
        let actual = synapse_a11y::current_foreground_context()
            .ok()
            .map(|context| foreground_context_json(&context))
            .unwrap_or(Value::Null);
        #[cfg(not(windows))]
        let actual = Value::Null;
        tracing::error!(
            code = error_codes::ACTION_FOREGROUND_LOST,
            detail_code = UNARMED_DETAIL_CODE,
            stage,
            "refused global input because no exact destination was armed"
        );
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "foreground input refused at {stage}: no exact target window is armed, so global input has no verified destination"
            ),
            Some(json!({
                "code": error_codes::ACTION_FOREGROUND_LOST,
                "detail_code": UNARMED_DETAIL_CODE,
                "refused_before_delivery": true,
                "stage": stage,
                "expected": Value::Null,
                "actual": actual,
                "source_of_truth": SOURCE_OF_TRUTH,
                "resolution": "bind an agent-owned session target and use act operation=foreground, or explicitly focus_window under the input lease before calling a raw foreground primitive",
            })),
        ));
    };
    // The fence's validity is exactly the input lease's validity: a released
    // or expired lease ends the foreground claim, and foreground dispatch is
    // independently gated on holding one. Auto-disarming here keeps a stale
    // expectation from refusing a later session's legitimate activation
    // without needing every release path to remember to call `disarm`.
    if !synapse_action::lease::status().held {
        disarm("foreground_input_lease_not_held");
        tracing::error!(
            code = error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
            stage,
            expected_hwnd = expected.hwnd,
            "refused global input because the foreground input lease is no longer held"
        );
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "foreground input refused at {stage}: the input lease is not held, so target hwnd 0x{:x} has no delivery authority",
                expected.hwnd
            ),
            Some(json!({
                "code": error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
                "detail_code": "M2_FOREGROUND_FENCE_LEASE_NOT_HELD",
                "refused_before_delivery": true,
                "stage": stage,
                "expected": fence_window_json(&expected),
                "actual_lease": synapse_action::lease::status(),
                "source_of_truth": "synapse_action::lease status read immediately before input dispatch",
                "resolution": "acquire the foreground input lease for this session, re-bind or re-focus the exact target, then retry",
            })),
        ));
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
                    // This code is also raised by post-action readbacks, where
                    // input *was* delivered and the caller must verify. The
                    // fence refuses strictly before dispatch, so there is
                    // provably nothing to verify; the marker keeps the facade
                    // from reporting `delivered_unverified` for a call that
                    // delivered nothing (#1830).
                    "refused_before_delivery": true,
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
                    // This code is also raised by post-action readbacks, where
                    // input *was* delivered and the caller must verify. The
                    // fence refuses strictly before dispatch, so there is
                    // provably nothing to verify; the marker keeps the facade
                    // from reporting `delivered_unverified` for a call that
                    // delivered nothing (#1830).
                    "refused_before_delivery": true,
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

fn fence_window_json(armed: &ArmedForeground) -> Value {
    json!({
        "hwnd": armed.hwnd,
        "pid": armed.pid,
        "process_name": armed.process_name,
        "window_title": armed.window_title,
        "armed_by": armed.armed_by,
    })
}

#[cfg(windows)]
fn foreground_context_json(context: &synapse_core::ForegroundContext) -> Value {
    json!({
        "hwnd": context.hwnd,
        "pid": context.pid,
        "process_name": context.process_name,
        "window_title": context.window_title,
    })
}
