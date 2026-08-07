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
//!
//! # Where the armed identity actually lives (#2057)
//!
//! One high-level action is many OS emissions over real time (`type_text` calls
//! `SendInput` once per UTF-16 unit, strokes emit per timed sample), so a check
//! that only runs here leaves a time-of-check/time-of-use gap: the human can
//! take the foreground mid-sequence and receive the suffix. The armed identity
//! is therefore owned by [`synapse_action::foreground_fence`], one crate below,
//! where it is re-verified immediately before every `SendInput`, physical
//! cursor mutation, and ViGEm HID report.
//!
//! This module is the MCP-facing face of that single store — not a parallel
//! copy. It keeps the two policies that belong at the tool boundary and have no
//! meaning at an emission site: *unarmed global input is refused outright*, and
//! refusals are rendered as `ErrorData` with `refused_before_delivery`.

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

/// Arms the fence with a fully resolved top-level window identity.
///
/// Callers either provide a verified `GetForegroundWindow()` readback or use
/// [`arm_expected_target`] to resolve an agent-owned target without activating
/// it. In both cases the stored identity comes from live Win32 state.
pub(crate) fn arm(armed: ArmedForeground) {
    // The window class is read here, once, and carried into the emission-layer
    // store: USER handles are recycled, so `HWND` equality alone cannot prove
    // that the window the fence checks per emission is still the window that
    // was bound (#2057).
    let class_name = synapse_action::foreground_fence::window_identity(armed.hwnd)
        .map(|identity| identity.class_name)
        .unwrap_or_default();
    tracing::info!(
        code = "M2_FOREGROUND_FENCE_ARMED",
        hwnd = armed.hwnd,
        pid = armed.pid,
        class_name = %class_name,
        process_name = %armed.process_name,
        window_title = %armed.window_title,
        armed_by = armed.armed_by,
        "foreground delivery fence armed to the verified foreground window"
    );
    synapse_action::foreground_fence::arm(synapse_action::ForegroundTarget {
        hwnd: armed.hwnd,
        pid: armed.pid,
        class_name,
        process_name: armed.process_name,
        window_title: armed.window_title,
        armed_by: armed.armed_by,
    });
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
    if let Some(previous) = synapse_action::foreground_fence::disarm(reason) {
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
    synapse_action::foreground_fence::armed().map(|target| ArmedForeground {
        hwnd: target.hwnd,
        pid: target.pid,
        process_name: target.process_name,
        window_title: target.window_title,
        armed_by: target.armed_by,
    })
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
    // The identity comparison itself is the emission layer's, so the tool
    // boundary and every `SendInput` boundary agree on what "the same window"
    // means — HWND *and* pid *and* window class, never the handle value alone
    // (#2057).
    synapse_action::foreground_fence::check(stage).map_err(|drift| {
        let mut data = drift.to_json();
        if let Value::Object(map) = &mut data {
            map.insert(
                "code".to_owned(),
                Value::String(error_codes::ACTION_FOREGROUND_LOST.to_owned()),
            );
            map.insert(
                "resolution".to_owned(),
                Value::String(drift.reason.remediation().to_owned()),
            );
        }
        tracing::error!(
            code = error_codes::ACTION_FOREGROUND_LOST,
            detail_code = drift.reason.detail_code(),
            stage,
            expected_hwnd = drift.expected.hwnd,
            expected_process = %drift.expected.process_name,
            actual_hwnd = drift.actual.as_ref().map_or(0, |actual| actual.hwnd),
            actual_process = %drift.actual.as_ref().map_or("", |actual| actual.process_name.as_str()),
            actual_title = %drift.actual.as_ref().map_or("", |actual| actual.window_title.as_str()),
            "refused foreground input: the foreground is not the armed window at the dispatch boundary"
        );
        ErrorData::new(ErrorCode(-32099), drift.message(), Some(data))
    })
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
