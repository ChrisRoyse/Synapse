pub(crate) mod auto_wait;
mod click;
mod clipboard;
mod config;
mod focus_window;
mod pad;
pub(crate) mod postcondition;
pub(crate) mod press;
mod release_all;
mod scroll;
mod set_field_text;
mod set_value;
mod stroke;
mod type_text;

use std::{
    fmt,
    sync::{Arc, Mutex, RwLock},
};

use rmcp::{ErrorData, model::ErrorCode};
use serde_json::{Value, json};
use synapse_action::{
    ActionBackend, ActionEmitter, ActionEmitterSnapshotHandle, ActionError, ActionHandle,
    ActionStateSnapshot, BackendRateLimitControl, BackendResolutionPolicy, LeaseOutcome,
    RELEASE_ALL_HANDLE, RecordingBackend, initialize_double_click_timing_cache, lease,
};
use synapse_core::{Point, error_codes};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const FOREGROUND_RESTORE_STABILITY_SAMPLES: usize = 6;
const FOREGROUND_RESTORE_STABILITY_INTERVAL_MS: u64 = 250;
const FOREGROUND_RESTORE_LEASE_MARGIN_MS: u64 = 1_000;
pub(crate) const FOREGROUND_CONTEXT_RESTORE_STABILITY_MS: u64 =
    FOREGROUND_RESTORE_STABILITY_INTERVAL_MS * FOREGROUND_RESTORE_STABILITY_SAMPLES as u64;

pub(crate) use click::ForegroundClickPolicy;
#[allow(unused_imports)]
pub use click::{
    ActClickParams, ActClickPostcondition, ActClickResponse, ActClickTarget, act_click_with_handle,
};
pub(crate) use click::{
    ActClickTierAttempt, CLICK_REASON_NO_OBSERVED_DELTA, CLICK_TIER_FOREGROUND,
    CLICK_TIER_POSTMESSAGE, act_click_postmessage_with_params, act_click_with_handle_and_lease,
    attach_click_tier_attempts, click_params_can_route_background_first,
    click_target_foreground_guard_hwnds, click_target_root_hwnd, click_tier_delivered,
    click_tier_failed,
};
#[cfg(test)]
pub use clipboard::{ActClipboardFormat, ActClipboardVerb};
pub use clipboard::{ActClipboardParams, ActClipboardResponse};
pub(crate) use clipboard::{
    SharedSessionClipboardBuffers, act_clipboard_session_buffer, new_session_clipboards,
};
pub use config::M2ServiceConfig;
pub use focus_window::{
    ActFocusWindowParams, ActFocusWindowResponse, act_focus_window,
    act_focus_window_request_details, act_focus_window_target_hwnd,
};
pub use pad::{ActPadParams, ActPadResponse, act_pad_with_handle};
pub use postcondition::default_verify_timeout_ms;
pub use press::action_from_press_params;
pub use press::{
    ActKeymapParams, ActKeymapResponse, ActPressParams, ActPressResponse, PressBackend,
    act_press_with_handle,
};
pub(crate) use press::{
    HwndKeyboardTargetState, ResolvedKeymapPress, act_keymap_response_from_press,
    act_press_cdp_target, act_press_normalized_labels, act_press_postmessage_target,
    delete_key_action, hwnd_keyboard_target_state, resolve_keymap_press, select_all_chord_action,
};
pub use release_all::{ReleaseAllParams, ReleaseAllResponse, release_all_with_handles};
pub use scroll::{ActScrollParams, ActScrollPoint, ActScrollResponse, act_scroll_with_handle};
#[cfg(windows)]
pub(crate) use set_field_text::act_set_field_text_web;
pub use set_field_text::{
    ActSetFieldTextLocator, ActSetFieldTextParams, ActSetFieldTextResponse,
    act_set_field_text_request_details,
};
pub(crate) use set_field_text::{
    METHOD_FOREGROUND_CLEAR, METHOD_FOREGROUND_REPLACE, SOURCE_UIA_PASSWORD_LENGTH,
    SOURCE_UIA_VALUE, SetFieldTextRoute, TIER_FOREGROUND_KEYS, act_set_field_text_native,
    finish_replace_response, params_with_resolved_element, required_element_id,
    set_field_text_route, validate_set_field_text_params,
};
pub use set_value::{
    ActSetValueParams, ActSetValueResponse, act_set_value, act_set_value_request_details,
};
pub use stroke::{
    ActStrokeParams, ActStrokeResponse, act_stroke_error_details, act_stroke_request_details,
    act_stroke_validation_failure_details, act_stroke_with_handle, validate_act_stroke_params,
};
pub(crate) use stroke::{ActStrokePlan, act_stroke_cdp_target};
pub use type_text::action_from_type_params;
pub(crate) use type_text::emitted_text;
pub use type_text::{ActTypeParams, ActTypeResponse, TypeBackend, act_type_with_handle};

pub(crate) use auto_wait::{default_auto_wait_timeout_ms, validate_auto_wait_timeout};
use config::RECORDING_BACKEND_ENV;

pub type SharedM2State = Arc<Mutex<M2State>>;

#[derive(Debug)]
pub(crate) struct ForegroundInputLeaseGuard {
    tool: &'static str,
    session_id: String,
    release_on_drop: bool,
    context_restore: Option<ForegroundInputContextSnapshot>,
}

impl ForegroundInputLeaseGuard {
    fn renewed(tool: &'static str, session_id: String) -> Self {
        Self {
            tool,
            context_restore: Some(ForegroundInputContextSnapshot::capture(tool, &session_id)),
            session_id,
            release_on_drop: false,
        }
    }

    fn auto_acquired(tool: &'static str, session_id: String) -> Self {
        Self {
            tool,
            context_restore: Some(ForegroundInputContextSnapshot::capture(tool, &session_id)),
            session_id,
            release_on_drop: true,
        }
    }

    pub(crate) fn disable_context_restore(&mut self, reason_code: &'static str) {
        if self.context_restore.take().is_some() {
            tracing::info!(
                code = "INPUT_LEASE_CONTEXT_RESTORE_DISABLED",
                tool = self.tool,
                session_id = %self.session_id,
                reason_code,
                "readback=foreground_input_context outcome=restore_disabled"
            );
        }
    }
}

impl Drop for ForegroundInputLeaseGuard {
    fn drop(&mut self) {
        if let Some(context_restore) = &self.context_restore {
            context_restore.restore(self.tool, &self.session_id);
        }
        if !self.release_on_drop {
            return;
        }
        match lease::release(&self.session_id) {
            Ok(status) => {
                tracing::info!(
                    code = "INPUT_LEASE_AUTO_RELEASED",
                    tool = self.tool,
                    session_id = %self.session_id,
                    held = status.held,
                    "readback=input_lease outcome=auto_released"
                );
            }
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    tool = self.tool,
                    session_id = %self.session_id,
                    detail = %error,
                    "foreground input lease auto-release failed"
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ForegroundInputContextSnapshot {
    cursor: Option<Point>,
    foreground_hwnd: Option<i64>,
    foreground_pid: Option<u32>,
    foreground_process_started_at_100ns: Option<u64>,
    foreground_title: Option<String>,
    cursor_capture_error: Option<String>,
    foreground_capture_error: Option<String>,
}

impl ForegroundInputContextSnapshot {
    fn capture(tool: &'static str, session_id: &str) -> Self {
        let cursor_read = synapse_action::backend::software::cursor_position();
        let foreground_read = synapse_a11y::current_foreground_context();

        let (cursor, cursor_capture_error) = match cursor_read {
            Ok(point) => (Some(point), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let (
            foreground_hwnd,
            foreground_pid,
            foreground_process_started_at_100ns,
            foreground_title,
            foreground_capture_error,
        ) = match foreground_read {
            Ok(context) => (
                Some(context.hwnd),
                Some(context.pid),
                process_started_at_100ns(context.pid),
                Some(context.window_title),
                None,
            ),
            Err(error) => (None, None, None, None, Some(error.to_string())),
        };

        let snapshot = Self {
            cursor,
            foreground_hwnd,
            foreground_pid,
            foreground_process_started_at_100ns,
            foreground_title,
            cursor_capture_error,
            foreground_capture_error,
        };
        snapshot.log_capture(tool, session_id);
        snapshot
    }

    fn log_capture(&self, tool: &'static str, session_id: &str) {
        if self.cursor.is_some() && self.foreground_hwnd.is_some() {
            if let Some(cursor) = self.cursor {
                tracing::info!(
                    code = "INPUT_LEASE_CONTEXT_CAPTURED",
                    tool,
                    session_id,
                    cursor_x = cursor.x,
                    cursor_y = cursor.y,
                    foreground_hwnd = ?self.foreground_hwnd,
                    foreground_pid = ?self.foreground_pid,
                    foreground_process_started_at_100ns = ?self.foreground_process_started_at_100ns,
                    foreground_title = ?self.foreground_title,
                    "readback=foreground_input_context outcome=captured"
                );
            }
            return;
        }

        tracing::warn!(
            code = error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
            tool,
            session_id,
            reason_code = self.capture_reason_code(),
            cursor = ?self.cursor,
            foreground_hwnd = ?self.foreground_hwnd,
            foreground_pid = ?self.foreground_pid,
            foreground_process_started_at_100ns = ?self.foreground_process_started_at_100ns,
            cursor_error = ?self.cursor_capture_error,
            foreground_error = ?self.foreground_capture_error,
            "foreground input context capture incomplete"
        );
    }

    fn restore(&self, tool: &'static str, session_id: &str) {
        let cursor = self.restore_cursor(tool, session_id);
        let foreground = self.restore_foreground(tool, session_id);
        tracing::info!(
            code = "INPUT_LEASE_CONTEXT_RESTORE_SUMMARY",
            tool,
            session_id,
            cursor_outcome = cursor,
            foreground_outcome = foreground,
            "readback=foreground_input_context outcome=restore_summary"
        );
    }

    fn restore_cursor(&self, tool: &'static str, session_id: &str) -> &'static str {
        let Some(cursor) = self.cursor else {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                tool,
                session_id,
                reason_code = "cursor_capture_unavailable",
                capture_error = ?self.cursor_capture_error,
                "foreground input cursor restore skipped"
            );
            return "skipped";
        };

        match synapse_action::backend::software::set_cursor_position(cursor) {
            Ok(actual) => {
                tracing::info!(
                    code = "INPUT_LEASE_CONTEXT_CURSOR_RESTORED",
                    tool,
                    session_id,
                    requested_x = cursor.x,
                    requested_y = cursor.y,
                    actual_x = actual.x,
                    actual_y = actual.y,
                    "readback=cursor_position outcome=restored"
                );
                "restored"
            }
            Err(error) => {
                tracing::error!(
                    code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
                    tool,
                    session_id,
                    reason_code = "cursor_restore_failed",
                    requested_x = cursor.x,
                    requested_y = cursor.y,
                    detail = %error,
                    "foreground input cursor restore failed"
                );
                "failed"
            }
        }
    }

    fn restore_foreground(&self, tool: &'static str, session_id: &str) -> &'static str {
        let Some(hwnd) = self.foreground_hwnd else {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                tool,
                session_id,
                reason_code = "foreground_capture_unavailable",
                capture_error = ?self.foreground_capture_error,
                "foreground window restore skipped"
            );
            record_foreground_restore_context_event(
                tool,
                session_id,
                "skipped",
                error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                "foreground_capture_unavailable",
                None,
                None,
                json!({
                    "capture_error": self.foreground_capture_error,
                }),
            );
            return "skipped";
        };
        let Some(expected_pid) = self.foreground_pid else {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                tool,
                session_id,
                reason_code = "foreground_pid_capture_unavailable",
                foreground_hwnd = hwnd,
                foreground_title = ?self.foreground_title,
                "foreground window restore skipped because prior owner pid was not captured"
            );
            record_foreground_restore_context_event(
                tool,
                session_id,
                "skipped",
                error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                "foreground_pid_capture_unavailable",
                None,
                None,
                json!({
                    "prior_hwnd": hwnd,
                    "prior_title": self.foreground_title,
                }),
            );
            return "skipped";
        };
        let Some(expected_process_started_at_100ns) = self.foreground_process_started_at_100ns
        else {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                tool,
                session_id,
                reason_code = "foreground_process_identity_capture_unavailable",
                foreground_hwnd = hwnd,
                expected_pid,
                foreground_title = ?self.foreground_title,
                "foreground window restore skipped because prior owner process identity was not captured"
            );
            record_foreground_restore_context_event(
                tool,
                session_id,
                "skipped",
                error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                "foreground_process_identity_capture_unavailable",
                None,
                None,
                json!({
                    "prior_hwnd": hwnd,
                    "expected_pid": expected_pid,
                    "prior_title": self.foreground_title,
                }),
            );
            return "skipped";
        };

        let readiness = foreground_window_restore_readiness(
            hwnd,
            expected_pid,
            expected_process_started_at_100ns,
        );
        if !readiness.alive {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                tool,
                session_id,
                reason_code = readiness.reason_code,
                foreground_hwnd = hwnd,
                expected_pid,
                actual_pid = ?readiness.actual_pid,
                expected_process_started_at_100ns,
                actual_process_started_at_100ns = ?readiness.actual_process_started_at_100ns,
                exit_code = ?readiness.exit_code,
                wait_status = ?readiness.wait_status,
                foreground_title = ?self.foreground_title,
                "foreground window restore skipped because prior HWND/PID is not restorable"
            );
            record_foreground_restore_context_event(
                tool,
                session_id,
                "skipped",
                error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                readiness.reason_code,
                None,
                None,
                json!({
                    "prior_hwnd": hwnd,
                    "expected_pid": expected_pid,
                    "actual_pid": readiness.actual_pid,
                    "expected_process_started_at_100ns": expected_process_started_at_100ns,
                    "actual_process_started_at_100ns": readiness.actual_process_started_at_100ns,
                    "exit_code": readiness.exit_code,
                    "wait_status": readiness.wait_status,
                    "prior_title": self.foreground_title,
                }),
            );
            return "skipped";
        }

        let current = match synapse_a11y::current_foreground_context() {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(
                    code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                    tool,
                    session_id,
                    reason_code = "foreground_current_read_failed",
                    foreground_hwnd = hwnd,
                    expected_pid,
                    foreground_title = ?self.foreground_title,
                    detail = %error,
                    "foreground window restore skipped because current foreground readback failed"
                );
                record_foreground_restore_context_event(
                    tool,
                    session_id,
                    "skipped",
                    error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                    "foreground_current_read_failed",
                    None,
                    Some(json!({
                        "detail": error.to_string(),
                    })),
                    json!({
                        "prior_hwnd": hwnd,
                        "expected_pid": expected_pid,
                        "prior_title": self.foreground_title,
                    }),
                );
                return "skipped";
            }
        };

        match foreground_restore_current_decision(hwnd, expected_pid, current.hwnd, current.pid) {
            ForegroundRestoreCurrentDecision::AlreadyCurrent => {
                tracing::info!(
                    code = "INPUT_LEASE_CONTEXT_FOREGROUND_ALREADY_CURRENT",
                    tool,
                    session_id,
                    foreground_hwnd = hwnd,
                    foreground_pid = expected_pid,
                    foreground_title = ?self.foreground_title,
                    current_hwnd = current.hwnd,
                    current_pid = current.pid,
                    current_title = %current.window_title,
                    "readback=foreground_window outcome=already_current"
                );
                record_foreground_restore_context_event(
                    tool,
                    session_id,
                    "already_current",
                    "INPUT_LEASE_CONTEXT_FOREGROUND_ALREADY_CURRENT",
                    "foreground_already_current",
                    Some(json!({
                        "hwnd": current.hwnd,
                        "pid": current.pid,
                        "process_name": current.process_name,
                        "window_title": current.window_title,
                    })),
                    None,
                    json!({
                        "prior_hwnd": hwnd,
                        "expected_pid": expected_pid,
                        "expected_process_started_at_100ns": expected_process_started_at_100ns,
                        "prior_title": self.foreground_title,
                    }),
                );
                return "already_current";
            }
            ForegroundRestoreCurrentDecision::SkipHumanMoved => {
                tracing::warn!(
                    code = error_codes::FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED,
                    tool,
                    session_id,
                    reason_code = "foreground_restore_skipped_human_moved",
                    prior_hwnd = hwnd,
                    expected_pid,
                    prior_title = ?self.foreground_title,
                    current_hwnd = current.hwnd,
                    current_pid = current.pid,
                    current_title = %current.window_title,
                    "foreground window restore skipped because current foreground moved after lease capture"
                );
                record_foreground_restore_context_event(
                    tool,
                    session_id,
                    "skipped_human_moved",
                    error_codes::FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED,
                    "foreground_restore_skipped_human_moved",
                    Some(json!({
                        "hwnd": current.hwnd,
                        "pid": current.pid,
                        "process_name": current.process_name,
                        "window_title": current.window_title,
                    })),
                    None,
                    json!({
                        "prior_hwnd": hwnd,
                        "expected_pid": expected_pid,
                        "expected_process_started_at_100ns": expected_process_started_at_100ns,
                        "prior_title": self.foreground_title,
                    }),
                );
                return "skipped_human_moved";
            }
        }
    }

    fn capture_reason_code(&self) -> &'static str {
        match (self.cursor.is_some(), self.foreground_hwnd.is_some()) {
            (false, false) => "cursor_and_foreground_capture_failed",
            (false, true) => "cursor_capture_failed",
            (true, false) => "foreground_capture_failed",
            (true, true) => "captured",
        }
    }
}

#[derive(Debug)]
struct ForegroundWindowRestoreReadiness {
    alive: bool,
    reason_code: &'static str,
    actual_pid: Option<u32>,
    actual_process_started_at_100ns: Option<u64>,
    exit_code: Option<u32>,
    wait_status: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundRestoreCurrentDecision {
    AlreadyCurrent,
    SkipHumanMoved,
}

fn foreground_restore_current_decision(
    captured_hwnd: i64,
    expected_pid: u32,
    current_hwnd: i64,
    current_pid: u32,
) -> ForegroundRestoreCurrentDecision {
    if current_hwnd == captured_hwnd && current_pid == expected_pid {
        ForegroundRestoreCurrentDecision::AlreadyCurrent
    } else {
        ForegroundRestoreCurrentDecision::SkipHumanMoved
    }
}

fn record_foreground_restore_context_event(
    tool: &'static str,
    session_id: &str,
    status: &'static str,
    code: &'static str,
    reason_code: &'static str,
    foreground: Option<Value>,
    foreground_read_error: Option<Value>,
    detail: Value,
) {
    // The daemon lifecycle ledger is a process-global singleton. In this crate's
    // own unit-test binary, tests run in parallel and this production side-effect
    // would write into whichever ledger the `daemon_lifecycle` tests momentarily
    // have configured — corrupting their assertions and poisoning `last_error`
    // (which flips `health_payload().ok` false for any concurrent health-reading
    // test). The recording itself is covered directly by
    // `daemon_lifecycle::tests::records_context_event_to_physical_files`, and the
    // real daemon (integration tests, production) is built without `cfg(test)` and
    // records normally. So suppress only this unasserted side-effect under test.
    #[cfg(test)]
    {
        let _ = (
            tool,
            session_id,
            status,
            code,
            reason_code,
            foreground,
            foreground_read_error,
            detail,
        );
        return;
    }
    #[cfg(not(test))]
    {
        let detail = json!({
            "code": code,
            "reason_code": reason_code,
            "detail": detail,
        });
        match crate::daemon_lifecycle::record_context_event(crate::daemon_lifecycle::ContextEvent {
            event_kind: "foreground_context_restore",
            tool,
            status,
            mcp_session_id: Some(session_id.to_owned()),
            foreground,
            foreground_read_error,
            detail,
        }) {
            Ok(seq) => tracing::info!(
                code = "INPUT_LEASE_CONTEXT_RESTORE_EVENT_RECORDED",
                tool,
                session_id,
                status,
                seq,
                reason_code,
                "readback=daemon_lifecycle outcome=foreground_context_restore_recorded"
            ),
            Err(error) => tracing::error!(
                code = "INPUT_LEASE_CONTEXT_RESTORE_EVENT_WRITE_FAILED",
                tool,
                session_id,
                status,
                reason_code,
                detail = %error,
                "foreground input context restore event write failed"
            ),
        }
    }
}

#[cfg(windows)]
fn foreground_window_restore_readiness(
    hwnd: i64,
    expected_pid: u32,
    expected_process_started_at_100ns: u64,
) -> ForegroundWindowRestoreReadiness {
    use std::ffi::c_void;

    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
        UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow},
    };

    let hwnd = HWND(hwnd as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return ForegroundWindowRestoreReadiness {
            alive: false,
            reason_code: "prior_foreground_closed",
            actual_pid: None,
            actual_process_started_at_100ns: None,
            exit_code: None,
            wait_status: None,
        };
    }

    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process_id)) };
    if process_id == 0 {
        return ForegroundWindowRestoreReadiness {
            alive: false,
            reason_code: "prior_foreground_owner_unavailable",
            actual_pid: None,
            actual_process_started_at_100ns: None,
            exit_code: None,
            wait_status: None,
        };
    }
    if process_id != expected_pid {
        return ForegroundWindowRestoreReadiness {
            alive: false,
            reason_code: "prior_foreground_owner_changed",
            actual_pid: Some(process_id),
            actual_process_started_at_100ns: None,
            exit_code: None,
            wait_status: None,
        };
    }
    let Ok(handle) = (unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            process_id,
        )
    }) else {
        return ForegroundWindowRestoreReadiness {
            alive: false,
            reason_code: "prior_foreground_process_unavailable",
            actual_pid: Some(process_id),
            actual_process_started_at_100ns: None,
            exit_code: None,
            wait_status: None,
        };
    };
    let mut exit_code = 0_u32;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let actual_process_started_at_100ns = unsafe {
        GetProcessTimes(
            handle,
            std::ptr::addr_of_mut!(creation),
            std::ptr::addr_of_mut!(exit),
            std::ptr::addr_of_mut!(kernel),
            std::ptr::addr_of_mut!(user),
        )
    }
    .is_ok()
    .then_some(filetime_ticks(creation));
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    let exit_code = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) }
        .is_ok()
        .then_some(exit_code);
    let _ = unsafe { CloseHandle(handle) };

    let (alive, reason_code, wait_status) = if actual_process_started_at_100ns.is_none() {
        (
            false,
            "prior_foreground_process_identity_unavailable",
            Some("identity_read_failed"),
        )
    } else if actual_process_started_at_100ns != Some(expected_process_started_at_100ns) {
        (
            false,
            "prior_foreground_process_identity_changed",
            Some("identity_mismatch"),
        )
    } else if wait_result == WAIT_TIMEOUT {
        (true, "prior_foreground_alive", Some("timeout_nonsignaled"))
    } else if wait_result == WAIT_OBJECT_0 {
        (
            false,
            "prior_foreground_process_exited",
            Some("object_signaled"),
        )
    } else {
        (
            false,
            "prior_foreground_process_wait_failed",
            Some("unexpected_wait_result"),
        )
    };
    ForegroundWindowRestoreReadiness {
        alive,
        reason_code,
        actual_pid: Some(process_id),
        actual_process_started_at_100ns,
        exit_code,
        wait_status,
    }
}

#[cfg(windows)]
fn process_started_at_100ns(pid: u32) -> Option<u64> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let started_at_100ns = unsafe {
        GetProcessTimes(
            handle,
            std::ptr::addr_of_mut!(creation),
            std::ptr::addr_of_mut!(exit),
            std::ptr::addr_of_mut!(kernel),
            std::ptr::addr_of_mut!(user),
        )
    }
    .is_ok()
    .then_some(filetime_ticks(creation));
    let _ = unsafe { CloseHandle(handle) };
    started_at_100ns
}

#[cfg(windows)]
fn filetime_ticks(value: windows::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(not(windows))]
fn foreground_window_restore_readiness(
    _hwnd: i64,
    _expected_pid: u32,
    _expected_process_started_at_100ns: u64,
) -> ForegroundWindowRestoreReadiness {
    ForegroundWindowRestoreReadiness {
        alive: false,
        reason_code: "foreground_restore_unsupported_platform",
        actual_pid: None,
        actual_process_started_at_100ns: None,
        exit_code: None,
        wait_status: None,
    }
}

#[cfg(not(windows))]
fn process_started_at_100ns(_pid: u32) -> Option<u64> {
    None
}

pub(crate) fn foreground_input_lease_ttl_for_hold_ms(hold_ms: u32) -> u64 {
    synapse_action::DEFAULT_LEASE_TTL_MS
        .max(
            u64::from(hold_ms)
                .saturating_add(FOREGROUND_CONTEXT_RESTORE_STABILITY_MS)
                .saturating_add(FOREGROUND_RESTORE_LEASE_MARGIN_MS),
        )
        .min(synapse_action::MAX_LEASE_TTL_MS)
}

pub(crate) fn acquire_foreground_input_lease_with_ttl(
    tool: &'static str,
    session_id: Option<&str>,
    ttl_ms: u64,
) -> Result<ForegroundInputLeaseGuard, ErrorData> {
    validate_foreground_input_lease_ttl_ms(tool, ttl_ms)?;
    let session_id = session_id.ok_or_else(|| {
        crate::m1::mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} requires an MCP session id before using the foreground input tier"),
        )
    })?;
    match lease::try_acquire(session_id, lease::ttl_from_ms(ttl_ms)) {
        LeaseOutcome::Acquired(status) => {
            tracing::info!(
                code = "INPUT_LEASE_ACTION_ACQUIRED",
                tool,
                session_id,
                ttl_ms = status.ttl_ms,
                expires_in_ms = status.expires_in_ms,
                "readback=input_lease outcome=action_acquired"
            );
            Ok(ForegroundInputLeaseGuard::auto_acquired(
                tool,
                session_id.to_owned(),
            ))
        }
        LeaseOutcome::Renewed(status) => {
            tracing::info!(
                code = "INPUT_LEASE_ACTION_RENEWED",
                tool,
                session_id,
                ttl_ms = status.ttl_ms,
                expires_in_ms = status.expires_in_ms,
                "readback=input_lease outcome=action_renewed"
            );
            Ok(ForegroundInputLeaseGuard::renewed(
                tool,
                session_id.to_owned(),
            ))
        }
        LeaseOutcome::Busy {
            holder,
            retry_after_ms,
        } => {
            let detail = format!(
                "{tool} requires the foreground input lease, but it is held by session {:?}; retry_after_ms={retry_after_ms}",
                holder.owner_session_id
            );
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                tool,
                requesting_session_id = session_id,
                holder_session_id = ?holder.owner_session_id,
                retry_after_ms,
                "readback=input_lease outcome=action_busy"
            );
            Err(action_error_to_mcp(&ActionError::ForegroundLeaseBusy {
                detail,
                holder_session_id: holder.owner_session_id,
                requesting_session_id: session_id.to_owned(),
                retry_after_ms,
            }))
        }
        LeaseOutcome::CleanupPending {
            expired,
            retry_after_ms,
        } => {
            let detail = format!(
                "{tool} requires the foreground input lease, but expired session {:?} still has pending held-input cleanup; retry_after_ms={retry_after_ms}",
                expired.owner_session_id
            );
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                tool,
                requesting_session_id = session_id,
                holder_session_id = ?expired.owner_session_id,
                retry_after_ms,
                "readback=input_lease outcome=action_cleanup_pending"
            );
            Err(action_error_to_mcp(&ActionError::ForegroundLeaseBusy {
                detail,
                holder_session_id: expired.owner_session_id,
                requesting_session_id: session_id.to_owned(),
                retry_after_ms,
            }))
        }
    }
}

fn validate_foreground_input_lease_ttl_ms(
    tool: &'static str,
    ttl_ms: u64,
) -> Result<(), ErrorData> {
    if (synapse_action::MIN_LEASE_TTL_MS..=synapse_action::MAX_LEASE_TTL_MS).contains(&ttl_ms) {
        return Ok(());
    }
    tracing::warn!(
        code = error_codes::TOOL_PARAMS_INVALID,
        detail_code = "LEASE_TTL_OUT_OF_RANGE",
        tool,
        ttl_ms,
        min_ttl_ms = synapse_action::MIN_LEASE_TTL_MS,
        max_ttl_ms = synapse_action::MAX_LEASE_TTL_MS,
        source_of_truth = "request ttl_ms before synapse_action::lease mutation",
        "foreground helper ttl_ms rejected before lease mutation"
    );
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} ttl_ms must be between {} and {}; got {ttl_ms}",
            synapse_action::MIN_LEASE_TTL_MS,
            synapse_action::MAX_LEASE_TTL_MS
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": "LEASE_TTL_OUT_OF_RANGE",
            "tool": tool,
            "source_id": "ttl_ms",
            "source_of_truth": "request ttl_ms before synapse_action::lease mutation",
            "min_ttl_ms": synapse_action::MIN_LEASE_TTL_MS,
            "max_ttl_ms": synapse_action::MAX_LEASE_TTL_MS,
            "ttl_ms": ttl_ms,
            "remediation": "pass ttl_ms in the advertised lease range or omit it for the default",
        })),
    ))
}

pub(crate) fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    match error {
        ActionError::ForegroundLeaseBusy {
            detail,
            holder_session_id,
            requesting_session_id,
            retry_after_ms,
        } => ErrorData::new(
            ErrorCode(-32099),
            error.to_string(),
            Some(json!({
                "code": error.code(),
                "detail": detail,
                "holder_session_id": holder_session_id,
                "requesting_session_id": requesting_session_id,
                "retry_after_ms": retry_after_ms,
            })),
        ),
        _ => crate::m1::mcp_error(error.code(), error.to_string()),
    }
}

pub struct M2State {
    pub emitter_handle: ActionHandle,
    pub snapshot_handle: ActionEmitterSnapshotHandle,
    pub rate_limit_control: BackendRateLimitControl,
    pub recording: Option<Arc<RecordingBackend>>,
    pub connection_closed_cancel: Option<CancellationToken>,
    backend_resolution: Arc<RwLock<BackendResolutionPolicy>>,
    backend_resolution_source: String,
    retained_emitter: Option<ActionEmitter>,
    emitter_cancel: Option<CancellationToken>,
    emitter_task: Option<JoinHandle<ActionStateSnapshot>>,
    emitter_done: Option<watch::Receiver<Option<ActionStateSnapshot>>>,
}

impl M2State {
    #[must_use]
    pub fn from_env() -> Self {
        let recording_backend = std::env::var(RECORDING_BACKEND_ENV).ok();
        Self::from_recording_backend_env(recording_backend.as_deref())
    }

    pub fn try_from_env() -> anyhow::Result<Self> {
        Self::try_from_config(&M2ServiceConfig::from_env())
    }

    pub fn try_from_config(config: &M2ServiceConfig) -> anyhow::Result<Self> {
        Self::try_from_config_with_shutdown_tokens(
            config,
            CancellationToken::new(),
            "shutdown",
            None,
        )
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "keeps the fallible constructor contract aligned with try_from_env/try_from_config"
    )]
    pub fn try_from_config_with_shutdown_tokens(
        config: &M2ServiceConfig,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_recording_backend_env_with_configured_backends(
            config.recording_backend.as_deref(),
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            None,
            None,
        ))
    }

    #[must_use]
    pub fn from_recording_backend_env(recording_backend: Option<&str>) -> Self {
        Self::from_recording_backend_env_with_cancel(recording_backend, CancellationToken::new())
    }

    #[must_use]
    pub fn from_recording_backend_env_with_cancel(
        recording_backend: Option<&str>,
        emitter_cancel: CancellationToken,
    ) -> Self {
        Self::from_recording_backend_env_with_shutdown_tokens(
            recording_backend,
            emitter_cancel,
            "shutdown",
            None,
        )
    }

    #[must_use]
    pub fn from_recording_backend_env_with_shutdown_tokens(
        recording_backend: Option<&str>,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
    ) -> Self {
        Self::from_recording_backend_env_with_actor_backend(
            recording_backend,
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            None,
        )
    }

    /// Lower-level constructor that lets callers (notably cross-platform
    /// tests) substitute the actor's `ActionBackend` for one that does not
    /// require the production OS — e.g. `RecordingBackend`. Production code
    /// passes `actor_backend = None` and gets the platform-native backends.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn from_recording_backend_env_with_actor_backend(
        recording_backend: Option<&str>,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
        actor_backend: Option<Arc<dyn ActionBackend>>,
    ) -> Self {
        Self::from_recording_backend_env_with_configured_backends(
            recording_backend,
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            actor_backend,
            None,
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    fn from_recording_backend_env_with_configured_backends(
        recording_backend: Option<&str>,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
        actor_backend: Option<Arc<dyn ActionBackend>>,
        action_backends: Option<synapse_action::Backends>,
    ) -> Self {
        let double_click_timing = initialize_double_click_timing_cache();
        tracing::info!(
            code = "M2_DOUBLE_CLICK_TIMING_CACHED",
            window_ms = double_click_timing.window_ms,
            inter_click_delay_ms = double_click_timing.inter_click_delay_ms,
            source = double_click_timing.source,
            "readback=double_click_timing after_cache_readback"
        );
        let recording =
            recording_backend_enabled(recording_backend).then(|| Arc::new(RecordingBackend::new()));
        let actor_backend = actor_backend.or_else(|| {
            recording
                .as_ref()
                .map(|recording| Arc::clone(recording) as Arc<dyn ActionBackend>)
        });
        let backend_resolution = Arc::new(RwLock::new(BackendResolutionPolicy::default()));
        let tool_connection_closed_cancel = connection_closed_cancel.clone();
        let (emitter_handle, snapshot_handle, emitter) = actor_backend.map_or_else(
            || {
                action_backends.map_or_else(
                    || {
                        ActionEmitter::channel_with_backends_and_policy(
                            synapse_action::Backends::production(),
                            Arc::clone(&backend_resolution),
                        )
                    },
                    |backends| {
                        ActionEmitter::channel_with_backends_and_policy(
                            backends,
                            Arc::clone(&backend_resolution),
                        )
                    },
                )
            },
            |backend| {
                ActionEmitter::channel_with_backends_and_policy(
                    synapse_action::Backends::all_routed_to(backend),
                    Arc::clone(&backend_resolution),
                )
            },
        );
        let rate_limit_control = emitter.rate_limit_control();
        if tokio::runtime::Handle::try_current().is_ok() {
            let _release_handle_result = RELEASE_ALL_HANDLE.set(emitter_handle.clone());
            let (done_tx, done_rx) = watch::channel(None);
            let emitter_task = tokio::spawn(async move {
                let snapshot = emitter
                    .run_with_shutdown_reason(
                        shutdown_cancel,
                        shutdown_reason,
                        connection_closed_cancel,
                    )
                    .await;
                let _send_result = done_tx.send(Some(snapshot.clone()));
                snapshot
            });
            return Self {
                emitter_handle,
                snapshot_handle,
                rate_limit_control,
                recording,
                connection_closed_cancel: tool_connection_closed_cancel,
                backend_resolution,
                backend_resolution_source: "global_default".to_owned(),
                retained_emitter: None,
                emitter_cancel: None,
                emitter_task: Some(emitter_task),
                emitter_done: Some(done_rx),
            };
        }

        Self {
            emitter_handle,
            snapshot_handle,
            rate_limit_control,
            recording,
            connection_closed_cancel: tool_connection_closed_cancel,
            backend_resolution,
            backend_resolution_source: "global_default".to_owned(),
            retained_emitter: Some(emitter),
            emitter_cancel: None,
            emitter_task: None,
            emitter_done: None,
        }
    }

    #[must_use]
    pub const fn recording_enabled(&self) -> bool {
        self.recording.is_some()
    }

    #[must_use]
    pub const fn emitter_retained(&self) -> bool {
        self.retained_emitter.is_some()
    }

    #[must_use]
    pub fn emitter_running(&self) -> bool {
        self.emitter_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    #[must_use]
    pub fn emitter_available(&self) -> bool {
        self.emitter_retained() || self.emitter_running()
    }

    #[must_use]
    pub fn emitter_done_receiver(&self) -> Option<watch::Receiver<Option<ActionStateSnapshot>>> {
        self.emitter_done.clone()
    }

    #[must_use]
    pub fn backend_resolution_source(&self) -> &str {
        &self.backend_resolution_source
    }

    pub fn backend_resolution_readback(&self) -> Result<(String, BackendResolutionPolicy), String> {
        self.backend_resolution
            .read()
            .map(|policy| (self.backend_resolution_source.clone(), *policy))
            .map_err(|_err| "backend resolution policy lock poisoned".to_owned())
    }

    pub fn set_backend_resolution(
        &mut self,
        source: String,
        policy: BackendResolutionPolicy,
    ) -> Result<(), String> {
        let mut guard = self
            .backend_resolution
            .write()
            .map_err(|_err| "backend resolution policy lock poisoned".to_owned())?;
        *guard = policy;
        drop(guard);
        self.backend_resolution_source = source;
        Ok(())
    }
}

impl Default for M2State {
    fn default() -> Self {
        Self::from_env()
    }
}

impl fmt::Debug for M2State {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend_resolution = self.backend_resolution_readback().ok();
        formatter
            .debug_struct("M2State")
            .field("emitter_handle", &self.emitter_handle)
            .field("snapshot_handle", &self.snapshot_handle)
            .field(
                "rate_limit_control",
                &self.rate_limit_control.try_snapshot().ok(),
            )
            .field("recording", &self.recording_enabled())
            .field(
                "connection_closed_cancel",
                &self.connection_closed_cancel.is_some(),
            )
            .field("backend_resolution", &backend_resolution)
            .field(
                "backend_resolution_source",
                &self.backend_resolution_source(),
            )
            .field("retained_emitter", &self.emitter_retained())
            .field("emitter_cancel", &self.emitter_cancel.is_some())
            .field("emitter_task", &self.emitter_running())
            .field("emitter_done", &self.emitter_done.is_some())
            .field("emitter_available", &self.emitter_available())
            .finish()
    }
}

pub fn shared_m2_state_from_env() -> anyhow::Result<SharedM2State> {
    Ok(Arc::new(Mutex::new(M2State::try_from_env()?)))
}

pub fn shared_m2_state_from_config_with_shutdown_reason(
    config: &M2ServiceConfig,
    shutdown_cancel: CancellationToken,
    shutdown_reason: &'static str,
    connection_closed_cancel: Option<CancellationToken>,
) -> anyhow::Result<SharedM2State> {
    Ok(Arc::new(Mutex::new(
        M2State::try_from_config_with_shutdown_tokens(
            config,
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
        )?,
    )))
}
#[must_use]
pub fn recording_backend_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests;
