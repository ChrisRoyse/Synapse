use std::{sync::Arc, time::Instant};

use rmcp::{ErrorData, model::ErrorCode};
use serde_json::{Map, Value, json};
use synapse_action::{ActionError, ActionHandle, RecordingBackend, cached_double_click_timing};
use synapse_core::{Action, Backend, ButtonAction, ElementId, MouseTarget, Point, error_codes};

use crate::m1::mcp_error;

#[cfg(windows)]
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, IsWindow},
};

mod element;
mod record;
mod schema;

pub use schema::{
    ActClickParams, ActClickPostcondition, ActClickResponse, ActClickTarget, ActClickTierAttempt,
};

const MAX_CLICK_HOLD_MS: u32 = 30_000;
const SUPPORTED_UIA_CLICK_PATTERNS: [&str; 5] = [
    "InvokePattern",
    "TogglePattern",
    "SelectionItemPattern",
    "ExpandCollapsePattern",
    "LegacyIAccessiblePattern.DoDefaultAction",
];
pub(crate) const CLICK_TIER_CDP: &str = "cdp";
pub(crate) const CLICK_TIER_UIA: &str = "uia";
pub(crate) const CLICK_TIER_POSTMESSAGE: &str = "postmessage";
pub(crate) const CLICK_TIER_FOREGROUND: &str = "foreground";
pub(crate) const CLICK_REASON_PATTERN_UNSUPPORTED: &str = "pattern_unsupported";
pub(crate) const CLICK_REASON_ELEMENT_STALE: &str = "element_stale";
pub(crate) const CLICK_REASON_BACKEND_UNAVAILABLE: &str = "backend_unavailable";
pub(crate) const CLICK_REASON_TARGET_INVALID: &str = "target_invalid";
pub(crate) const CLICK_REASON_PARAMS_INVALID: &str = "params_invalid";
pub(crate) const CLICK_REASON_NO_OBSERVED_DELTA: &str = "no_observed_delta";
pub(crate) const CLICK_REASON_SELECTION_ONLY: &str = "selection_only";
pub(crate) const CLICK_REASON_FOREGROUND_REFUSED: &str = "foreground_refused";
pub(crate) const CLICK_REASON_ERROR: &str = "error";

#[derive(Clone, Debug, Default)]
pub(crate) struct ForegroundClickPolicy {
    session_id: Option<String>,
    hidden_desktop_refusal: Option<HiddenDesktopForegroundRefusal>,
}

#[derive(Clone, Debug)]
struct HiddenDesktopForegroundRefusal {
    session_id: String,
    desktop_names: Vec<String>,
}

impl ForegroundClickPolicy {
    pub(crate) fn allowed(session_id: Option<String>) -> Self {
        Self {
            session_id,
            hidden_desktop_refusal: None,
        }
    }

    pub(crate) fn refuse_hidden_desktop(session_id: String, desktop_names: Vec<String>) -> Self {
        Self {
            session_id: Some(session_id.clone()),
            hidden_desktop_refusal: Some(HiddenDesktopForegroundRefusal {
                session_id,
                desktop_names,
            }),
        }
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn foreground_refusal_error(&self, tool: &'static str) -> Option<ErrorData> {
        let refusal = self.hidden_desktop_refusal.as_ref()?;
        let detail = format!(
            "{tool} cannot use the visible foreground input tier because MCP session {:?} owns hidden desktop(s) {:?}; hidden Win32 desktops are not the active input desktop, so raw SendInput/cursor delivery is refused. Use a background CDP/UIA/PostMessage target route or a separate Windows session/RDP path for raw-input-required apps.",
            refusal.session_id, refusal.desktop_names
        );
        Some(ErrorData::new(
            ErrorCode(-32099),
            detail,
            Some(json!({
                "code": error_codes::FOREGROUND_ACTIVATION_REFUSED,
                "reason": "hidden_desktop_foreground_tier_refused",
                "tool": tool,
                "session_id": refusal.session_id,
                "desktop_names": refusal.desktop_names,
                "foreground_tier_allowed": false,
            })),
        ))
    }
}

pub(crate) async fn act_click_with_handle_and_lease(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActClickParams,
    foreground_click_policy: ForegroundClickPolicy,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActClickResponse, ErrorData> {
    validate_click_params(&params)?;
    if params.deprecated_curve_alias_used {
        tracing::warn!(
            code = "M2_ACT_CLICK_DEPRECATED_CURVE_ALIAS",
            kind = "act_click",
            replacement = "velocity_profile",
            "act_click deprecated curve alias accepted; use velocity_profile for coordinate-move timing"
        );
    }
    let started = Instant::now();
    let double_click_timing = cached_double_click_timing();
    // #686: a web element id (cdcd sentinel) routes through CDP instead of UIA.
    #[cfg(windows)]
    if let ActClickTarget::Element(element) = &params.target
        && let Some(backend) = synapse_a11y::cdp_backend_from_element_id(&element.element_id)
    {
        ensure_element_transport_backend_allowed(&params, "CDP")?;
        return execute_cdp_click(
            &params,
            element,
            backend,
            double_click_timing,
            started,
            boundary,
        )
        .await;
    }
    if let ActClickTarget::Element(element) = &params.target {
        reject_click_modifiers_for_non_cdp(&params, "native/UIA element targets")?;
        ensure_element_transport_backend_allowed(&params, "UIA")?;
        return element::execute_element_click(
            handle,
            &params,
            element,
            recording.as_deref(),
            double_click_timing,
            started,
            foreground_click_policy,
            boundary,
        )
        .await;
    }

    reject_click_modifiers_for_non_cdp(&params, "coordinate point targets")?;
    let target = point_mouse_target(&params.target)?;
    let mut actions = Vec::with_capacity(usize::from(params.clicks) + 1);
    actions.push(Action::MouseMove {
        to: target,
        curve: params.velocity_profile.to_aim_curve(),
        duration_ms: params.duration_ms,
        backend: params.backend,
    });
    for _ in 0..params.clicks {
        actions.push(Action::MouseButton {
            button: params.button,
            action: ButtonAction::Press,
            hold_ms: params.hold_ms,
            backend: params.backend,
        });
    }

    let tier_attempts = if let Some(recording) = recording {
        boundary.ensure("immediately_before_coordinate_recording_dispatch")?;
        if let Err(error) = record::execute_recording(
            &recording,
            &actions,
            params.clicks,
            double_click_timing,
            boundary,
        )
        .await
        {
            let error_code = click_error_code(&error);
            let reason_code = click_reason_for_error_code(&error_code);
            let detail = error.message.to_string();
            return Err(attach_click_tier_attempts(
                error,
                vec![click_tier_failed(
                    CLICK_TIER_FOREGROUND,
                    reason_code,
                    error_code,
                    true,
                    detail,
                )],
            ));
        }
        vec![click_tier_delivered(
            CLICK_TIER_FOREGROUND,
            true,
            "screen-coordinate click recorded through the foreground input tier",
        )]
    } else {
        let mut tier_attempts = Vec::new();
        let _lease_guard = acquire_click_foreground_lease(
            &foreground_click_policy,
            params.hold_ms,
            &mut tier_attempts,
        )?;
        boundary.ensure("immediately_before_coordinate_action_dispatch")?;
        match record::execute_actor_actions(handle, actions, double_click_timing, boundary).await {
            Ok(()) => {
                tier_attempts.push(click_tier_delivered(
                    CLICK_TIER_FOREGROUND,
                    true,
                    "screen-coordinate click delivered through the foreground input tier",
                ));
                tier_attempts
            }
            Err(error) => {
                let error_code = click_error_code(&error);
                let reason_code = click_reason_for_error_code(&error_code);
                let detail = error.message.to_string();
                return Err(attach_click_tier_attempts(
                    error,
                    vec![click_tier_failed(
                        CLICK_TIER_FOREGROUND,
                        reason_code,
                        error_code,
                        true,
                        detail,
                    )],
                ));
            }
        }
    };
    let backend_tier_used = click_backend_tier_used(&tier_attempts);
    let required_foreground = click_required_foreground(&tier_attempts);

    Ok(ActClickResponse {
        ok: true,
        used_invoke_pattern: false,
        backend_used: backend_used_name(params.backend).to_owned(),
        backend_tier_used,
        required_foreground,
        desktop_route: None,
        tier_attempts,
        postcondition: schema::postcondition_not_requested(),
        press_hold_ms: params.hold_ms,
        double_click_window_ms: double_click_timing.window_ms,
        inter_click_delay_ms: double_click_timing.inter_click_delay_ms,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// Ledger tier name for the #2063 hidden-desktop click route. Distinct from the
/// plain `uia` tier precisely so a `CF_ACTION_LOG` reader can tell a
/// hidden-desktop click from a visible-desktop one.
pub(crate) const CLICK_TIER_UIA_HIDDEN_DESKTOP_WORKER: &str = "uia_invoke_hidden_desktop_worker";
const HIDDEN_DESKTOP_CLICK_SOURCE_OF_TRUTH: &str =
    "session_owned_desktop_worker_uia_subtree_readback";
/// Enough depth to see the state change a click produces (pressed/checked
/// state, selection, the dialog it opened) without snapshotting a whole app.
const HIDDEN_DESKTOP_CLICK_SNAPSHOT_DEPTH: u32 = 4;

/// #2063 finding 1: performs the semantic UIA click on the session-owned hidden
/// desktop that physically owns the element's window.
///
/// The #2056 FSV showed a daemon-desktop `InvokePattern` call reaching a
/// hidden-desktop button, so the previous behaviour was a real delivery — but an
/// unattributable one: no `desktop_route`, and its verification readback ran on
/// the daemon's desktop, where `IsWindow` for that HWND is false. That readback
/// can therefore never witness the click it is supposed to verify. Rather than
/// annotate the daemon-desktop call, the whole click is rerouted through the
/// owning desktop's worker, for three reasons:
///
/// 1. Microsoft documents client-side UIA *proxy* providers as communicating
///    "across the process boundary by sending and receiving Windows messages",
///    which cannot cross desktops. Cross-desktop UIA reach is therefore a
///    per-provider accident, not a capability; relying on it would make the tier
///    silently work for some controls and silently fail for others.
/// 2. Only a worker on the owning desktop can take an honest before/after
///    Source-of-Truth readback, and it does so in processes distinct from the
///    one that performed the click — the mutation is never its own witness.
/// 3. It keeps exactly one canonical hidden-desktop action path (#2056), so the
///    route label, the audit row, and the refusal vocabulary are uniform across
///    `set_field`, `click`, and `key`.
pub(crate) async fn act_click_hidden_desktop_worker(
    params: &ActClickParams,
    element_id: &ElementId,
    route: super::hidden_desktop::HiddenDesktopWindowRoute,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActClickResponse, ErrorData> {
    validate_click_params(params)?;
    let started = Instant::now();
    let double_click_timing = cached_double_click_timing();
    let route_label = route.route_label();

    // Worker process #1: the "before" Source of Truth, taken on the owning
    // desktop. Doubles as the desktop-membership re-check immediately before
    // delivery.
    let before = crate::desktop_worker::hidden_desktop_window_snapshot(
        &route.desktop_name,
        route.hwnd,
        HIDDEN_DESKTOP_CLICK_SNAPSHOT_DEPTH,
    )
    .map_err(|error| hidden_desktop_click_stage_error(element_id, &route, "before_read", error))?;
    let before_signature = super::postcondition::hash_json(&before.tree)?;

    // Worker process #2..N: one worker per requested click, each attached to the
    // owning desktop. No daemon-desktop UIA call, no foreground activation, no
    // raw input, no desktop switch.
    let mut outcomes = Vec::with_capacity(usize::from(params.clicks));
    for click_index in 0..params.clicks {
        boundary.ensure("immediately_before_hidden_desktop_uia_element_click")?;
        let action =
            crate::desktop_worker::hidden_desktop_invoke_element(&route.desktop_name, element_id)
                .map_err(|error| {
                hidden_desktop_click_stage_error(element_id, &route, "invoke", error)
            })?;
        outcomes.push(action);
        if click_index + 1 < params.clicks {
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(
                double_click_timing.inter_click_delay_ms,
            )))
            .await;
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(u64::from(
        params.verify_timeout_ms,
    )))
    .await;

    // Worker process #N+1: a fresh desktop connection, fresh COM/UIA client and
    // fresh element resolution read the "after" state.
    let after = crate::desktop_worker::hidden_desktop_window_snapshot(
        &route.desktop_name,
        route.hwnd,
        HIDDEN_DESKTOP_CLICK_SNAPSHOT_DEPTH,
    )
    .map_err(|error| hidden_desktop_click_stage_error(element_id, &route, "after_read", error))?;
    let after_signature = super::postcondition::hash_json(&after.tree)?;
    let observed_delta = before_signature != after_signature;

    let outcome_labels = outcomes
        .iter()
        .map(|outcome| {
            serde_json::to_value(outcome)
                .ok()
                .and_then(|value| match value {
                    Value::String(name) => Some(name),
                    Value::Object(map) => map.keys().next().cloned(),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown".to_owned())
        })
        .collect::<Vec<_>>();

    tracing::info!(
        code = "M2_ACT_CLICK_HIDDEN_DESKTOP_READBACK",
        tool = "act_click",
        element_id = %element_id,
        hwnd = route.hwnd,
        desktop_route = route_label.as_str(),
        backend_tier_used = CLICK_TIER_UIA_HIDDEN_DESKTOP_WORKER,
        required_foreground = false,
        clicks = params.clicks,
        outcomes = ?outcome_labels,
        before_signature = before_signature.as_str(),
        after_signature = after_signature.as_str(),
        observed_delta,
        source_of_truth = HIDDEN_DESKTOP_CLICK_SOURCE_OF_TRUTH,
        "readback=act_click route={route_label} outcomes={outcome_labels:?} observed_delta={observed_delta}"
    );

    if params.verify_delta && !observed_delta {
        return Err(attach_click_tier_attempts(
            super::postcondition::no_observed_delta_error(
                "act_click",
                HIDDEN_DESKTOP_CLICK_SOURCE_OF_TRUTH,
                params.verify_timeout_ms,
                before_signature,
                after_signature,
                json!({
                    "desktop_route": route_label,
                    "hwnd": route.hwnd,
                    "element_id": element_id.to_string(),
                    "uia_outcomes": outcome_labels,
                }),
            ),
            vec![click_tier_delivered(
                CLICK_TIER_UIA_HIDDEN_DESKTOP_WORKER,
                false,
                format!(
                    "UI Automation semantic click delivered on {route_label}; outcomes={}",
                    outcome_labels.join(",")
                ),
            )],
        ));
    }

    let postcondition = if observed_delta {
        super::postcondition::postcondition_observed_delta(
            "act_click",
            HIDDEN_DESKTOP_CLICK_SOURCE_OF_TRUTH,
            before_signature,
            after_signature,
            format!(
                "separate worker process on session-owned desktop {route_label} observed a UIA subtree change after the click"
            ),
        )
    } else {
        schema::postcondition_not_requested()
    };

    Ok(ActClickResponse {
        ok: true,
        used_invoke_pattern: true,
        backend_used: "uia".to_owned(),
        backend_tier_used: CLICK_TIER_UIA_HIDDEN_DESKTOP_WORKER.to_owned(),
        required_foreground: false,
        desktop_route: Some(route_label.clone()),
        tier_attempts: vec![click_tier_delivered(
            CLICK_TIER_UIA_HIDDEN_DESKTOP_WORKER,
            false,
            format!(
                "UI Automation semantic click delivered by a worker process on session-owned desktop {route_label}; outcomes={}",
                outcome_labels.join(",")
            ),
        )],
        postcondition,
        press_hold_ms: params.hold_ms,
        double_click_window_ms: double_click_timing.window_ms,
        inter_click_delay_ms: double_click_timing.inter_click_delay_ms,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// Re-frames a hidden-desktop worker failure with the exact route, stage, and
/// element so a refusal is never mistaken for a daemon-desktop failure (#2063).
fn hidden_desktop_click_stage_error(
    element_id: &ElementId,
    route: &super::hidden_desktop::HiddenDesktopWindowRoute,
    stage: &'static str,
    error: ErrorData,
) -> ErrorData {
    let code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let route_label = route.route_label();
    tracing::error!(
        code = code.as_str(),
        tool = "act_click",
        element_id = %element_id,
        hwnd = route.hwnd,
        desktop_route = route_label.as_str(),
        stage,
        required_foreground = false,
        detail = %error.message,
        "act_click hidden-desktop worker stage failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_click hidden-desktop {stage} failed for element {element_id} on session-owned desktop {}: {}",
            route.desktop_name, error.message
        ),
        Some(json!({
            "code": code,
            "tool": "act_click",
            "operation": stage,
            "desktop_route": route_label,
            "desktop_name": route.desktop_name,
            "element_id": element_id.to_string(),
            "hwnd": route.hwnd,
            "required_foreground": false,
            "backend_tier_used": CLICK_TIER_UIA_HIDDEN_DESKTOP_WORKER,
            "source_of_truth": HIDDEN_DESKTOP_CLICK_SOURCE_OF_TRUTH,
            "worker_error": error.data,
        })),
    )
}

/// #2063 finding 4: the top-level window the OS would actually hit-test at
/// `point`, read on the daemon's desktop — which is the input desktop, and the
/// only desktop a physical cursor click can ever land on.
///
/// `None` means no window is under the point at all. Callers must treat that as
/// evidence, never as permission.
#[cfg(windows)]
pub(crate) fn window_root_at_screen_point(point: Point) -> Option<i64> {
    use windows::Win32::{Foundation::POINT as WinPoint, UI::WindowsAndMessaging::WindowFromPoint};

    let hwnd = unsafe {
        WindowFromPoint(WinPoint {
            x: point.x,
            y: point.y,
        })
    };
    if hwnd.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let resolved = if root.0.is_null() { hwnd } else { root };
    Some(synapse_core::win32_hwnd::hwnd_to_wire(resolved.0 as isize))
}

#[cfg(not(windows))]
pub(crate) const fn window_root_at_screen_point(_point: Point) -> Option<i64> {
    None
}

pub(crate) async fn act_click_postmessage_with_params(
    params: &ActClickParams,
    mut prior_attempts: Vec<ActClickTierAttempt>,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActClickResponse, ErrorData> {
    validate_click_params(params)?;
    let started = Instant::now();
    let double_click_timing = cached_double_click_timing();
    match &params.target {
        ActClickTarget::Element(element) => {
            ensure_element_transport_backend_allowed(params, "PostMessage")?;
            element::execute_element_postmessage_click(
                params,
                element,
                prior_attempts,
                double_click_timing,
                started,
                boundary,
            )
            .await
        }
        ActClickTarget::Point(point) => {
            let detail = format!(
                "act_click PostMessage tier requires an element target resolved to an HWND, got point ({}, {})",
                point.x, point.y
            );
            prior_attempts.push(click_tier_failed(
                CLICK_TIER_POSTMESSAGE,
                CLICK_REASON_TARGET_INVALID,
                error_codes::ACTION_TARGET_INVALID,
                false,
                detail.clone(),
            ));
            Err(attach_click_tier_attempts(
                mcp_error(error_codes::ACTION_TARGET_INVALID, detail),
                prior_attempts,
            ))
        }
    }
}

/// Routes a click on a CDP web element id through CDP (#686): resolve the
/// browser's debug endpoint from the element's window, scroll the node into
/// view, and dispatch the click in viewport coordinates. Fail-loud if the
/// endpoint is gone or the node cannot be resolved.
#[cfg(windows)]
async fn execute_cdp_click(
    params: &ActClickParams,
    element: &schema::ActClickElementTarget,
    backend_node_id: i64,
    double_click_timing: synapse_action::DoubleClickTiming,
    started: Instant,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActClickResponse, ErrorData> {
    use synapse_core::MouseButton;

    let hwnd = element
        .element_id
        .parts()
        .map_err(|err| {
            let detail = format!("web element id is malformed: {err}");
            attach_click_tier_attempts(
                mcp_error(error_codes::ACTION_ELEMENT_NOT_RESOLVED, detail.clone()),
                vec![click_tier_failed(
                    CLICK_TIER_CDP,
                    CLICK_REASON_TARGET_INVALID,
                    error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                    false,
                    detail,
                )],
            )
        })?
        .hwnd;
    // Foreground window title disambiguates which tab owns the per-document node.
    let title_hint = synapse_a11y::foreground_context(hwnd)
        .map(|context| context.window_title)
        .unwrap_or_default();
    let target_id_hint = synapse_a11y::cdp_target_from_element_id(&element.element_id);
    let button = match params.button {
        MouseButton::Left => synapse_a11y::CdpMouseButton::Left,
        MouseButton::Right => synapse_a11y::CdpMouseButton::Right,
        MouseButton::Middle => synapse_a11y::CdpMouseButton::Middle,
        other => {
            let detail =
                format!("act_click button {other:?} is not supported for web (CDP) elements");
            return Err(attach_click_tier_attempts(
                mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.clone()),
                vec![click_tier_failed(
                    CLICK_TIER_CDP,
                    CLICK_REASON_PARAMS_INVALID,
                    error_codes::TOOL_PARAMS_INVALID,
                    false,
                    detail,
                )],
            ));
        }
    };
    let modifiers = cdp_click_modifier_bits(&params.modifiers);

    if let Some(endpoint) = synapse_a11y::endpoint_for_window(hwnd) {
        boundary.ensure("immediately_before_cdp_click_node")?;
        synapse_a11y::cdp_click_node(
            &endpoint,
            &title_hint,
            target_id_hint.as_deref(),
            backend_node_id,
            button,
            i64::from(params.clicks),
            modifiers,
        )
        .await
        .map_err(|err| {
            let error = action_error_to_mcp(&a11y_to_action_error(&err));
            attach_click_tier_attempts(
                error,
                vec![click_tier_failed(
                    CLICK_TIER_CDP,
                    click_reason_for_error_code(err.code()),
                    err.code(),
                    false,
                    err.to_string(),
                )],
            )
        })?;
    } else {
        let bridge_button = match button {
            synapse_a11y::CdpMouseButton::Left => {
                crate::chrome_debugger_bridge::ChromeDebuggerMouseButton::Left
            }
            synapse_a11y::CdpMouseButton::Right => {
                crate::chrome_debugger_bridge::ChromeDebuggerMouseButton::Right
            }
            synapse_a11y::CdpMouseButton::Middle => {
                crate::chrome_debugger_bridge::ChromeDebuggerMouseButton::Middle
            }
        };
        boundary.ensure("immediately_before_chrome_bridge_click_node")?;
        crate::chrome_debugger_bridge::click_node(
            hwnd,
            &title_hint,
            target_id_hint.as_deref(),
            backend_node_id,
            bridge_button,
            i64::from(params.clicks),
        )
        .await
        .map_err(|err| {
            let detail = format!("Chrome debugger extension click failed: {}", err.detail());
            attach_click_tier_attempts(
                mcp_error(err.code(), detail.clone()),
                vec![click_tier_failed(
                    CLICK_TIER_CDP,
                    click_reason_for_error_code(err.code()),
                    err.code(),
                    false,
                    detail,
                )],
            )
        })?;
    }

    Ok(ActClickResponse {
        ok: true,
        used_invoke_pattern: false,
        backend_used: "cdp".to_owned(),
        backend_tier_used: CLICK_TIER_CDP.to_owned(),
        required_foreground: false,
        desktop_route: None,
        tier_attempts: vec![click_tier_delivered(
            CLICK_TIER_CDP,
            false,
            "web element click delivered through Chrome DevTools Protocol",
        )],
        postcondition: schema::postcondition_not_requested(),
        press_hold_ms: params.hold_ms,
        double_click_window_ms: double_click_timing.window_ms,
        inter_click_delay_ms: double_click_timing.inter_click_delay_ms,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// Maps an a11y CDP error to an action error so it surfaces with the same shape
/// as other action failures.
#[cfg(windows)]
fn a11y_to_action_error(err: &synapse_a11y::A11yError) -> ActionError {
    ActionError::TargetInvalid {
        detail: format!("{} ({})", err, err.code()),
    }
}

pub(crate) fn click_tier_delivered(
    tier: impl Into<String>,
    required_foreground: bool,
    detail: impl Into<String>,
) -> ActClickTierAttempt {
    let attempt = ActClickTierAttempt {
        tier: tier.into(),
        status: "delivered".to_owned(),
        reason_code: None,
        error_code: None,
        detail: Some(detail.into()),
        required_foreground,
    };
    log_click_tier_attempt(&attempt);
    attempt
}

pub(crate) fn click_tier_failed(
    tier: impl Into<String>,
    reason_code: impl Into<String>,
    error_code: impl Into<String>,
    required_foreground: bool,
    detail: impl Into<String>,
) -> ActClickTierAttempt {
    let attempt = ActClickTierAttempt {
        tier: tier.into(),
        status: "failed".to_owned(),
        reason_code: Some(reason_code.into()),
        error_code: Some(error_code.into()),
        detail: Some(detail.into()),
        required_foreground,
    };
    log_click_tier_attempt(&attempt);
    attempt
}

pub(crate) fn attach_click_tier_attempts(
    mut error: ErrorData,
    tier_attempts: Vec<ActClickTierAttempt>,
) -> ErrorData {
    let attempts = serde_json::to_value(&tier_attempts).unwrap_or_else(|err| {
        json!([{
            "tier": "telemetry",
            "status": "failed",
            "reason_code": "attempt_chain_encode_failed",
            "error_code": error_codes::TOOL_INTERNAL_ERROR,
            "detail": err.to_string(),
            "required_foreground": false,
        }])
    });
    let mut data = match error.data.take() {
        Some(Value::Object(map)) => map,
        Some(other) => {
            let mut map = Map::new();
            map.insert("original_data".to_owned(), other);
            map
        }
        None => Map::new(),
    };
    data.insert("tier_attempts".to_owned(), attempts);
    data.insert("silent_fallback_allowed".to_owned(), Value::Bool(false));
    error.data = Some(Value::Object(data));
    error
}

pub(crate) fn click_backend_tier_used(tier_attempts: &[ActClickTierAttempt]) -> String {
    tier_attempts
        .iter()
        .rev()
        .find(|attempt| attempt.status == "delivered")
        .map(|attempt| attempt.tier.clone())
        .unwrap_or_else(|| "none".to_owned())
}

pub(crate) fn click_required_foreground(tier_attempts: &[ActClickTierAttempt]) -> bool {
    tier_attempts
        .iter()
        .rev()
        .find(|attempt| attempt.status == "delivered")
        .is_some_and(|attempt| attempt.required_foreground)
}

pub(crate) fn click_params_can_route_background_first(params: &ActClickParams) -> bool {
    if !matches!(params.backend, Backend::Auto | Backend::Software) {
        return false;
    }
    match &params.target {
        ActClickTarget::Element(element) => {
            #[cfg(windows)]
            if synapse_a11y::cdp_backend_from_element_id(&element.element_id).is_some() {
                return false;
            }
            !params.use_invoke_pattern || params.coordinate_fallback_on_unsupported
        }
        ActClickTarget::Point(_) => false,
    }
}

pub(crate) fn click_target_root_hwnd(params: &ActClickParams) -> Result<Option<i64>, ErrorData> {
    let verify_target_window_hwnd = params
        .verify_target_window_hwnd
        .map(|hwnd| crate::m1::validate_hwnd_shape("act_click", "verify_target_window_hwnd", hwnd))
        .transpose()?;
    match &params.target {
        ActClickTarget::Element(element) => {
            let parsed_hwnd = element
                .element_id
                .parts()
                .map_err(|error| {
                    mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "act_click element id {} could not be parsed for target-window verification: {error}",
                            element.element_id
                        ),
                    )
                })?
                .hwnd;
            let hwnd = verified_top_level_hwnd(parsed_hwnd).map_err(|detail| {
                mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "act_click element id {} could not be normalized for target-window verification: {detail}",
                        element.element_id
                    ),
                )
            })?;
            if let Some(expected) = verify_target_window_hwnd {
                let expected_root = verified_top_level_hwnd(expected).map_err(|detail| {
                    mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "act_click verify_target_window_hwnd 0x{expected:x} could not be normalized for element-target verification: {detail}"
                        ),
                    )
                })?;
                if expected_root != hwnd {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "act_click verify_target_window_hwnd 0x{expected_root:x} does not match element target root HWND 0x{hwnd:x}"
                        ),
                    ));
                }
            }
            Ok(Some(hwnd))
        }
        ActClickTarget::Point(_) => verify_target_window_hwnd
            .map(|hwnd| {
                verified_top_level_hwnd(hwnd).map_err(|detail| {
                    mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "act_click verify_target_window_hwnd 0x{hwnd:x} could not be normalized for point-target verification: {detail}"
                        ),
                    )
                })
            })
            .transpose(),
    }
}

pub(crate) fn click_target_foreground_guard_hwnds(
    params: &ActClickParams,
) -> Result<Option<(i64, i64)>, ErrorData> {
    match &params.target {
        ActClickTarget::Element(element) => {
            let element_hwnd = element
                .element_id
                .parts()
                .map_err(|error| {
                    mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "act_click element id {} could not be parsed for foreground guard: {error}",
                            element.element_id
                        ),
                    )
                })?
                .hwnd;
            let root_hwnd = verified_top_level_hwnd(element_hwnd).map_err(|detail| {
                mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "act_click element id {} HWND 0x{element_hwnd:x} could not be normalized for foreground guard: {detail}",
                        element.element_id
                    ),
                )
            })?;
            Ok(Some((element_hwnd, root_hwnd)))
        }
        ActClickTarget::Point(_) => Ok(None),
    }
}

#[cfg(windows)]
fn verified_top_level_hwnd(hwnd: i64) -> Result<i64, String> {
    let native = synapse_core::win32_hwnd::hwnd_from_wire(hwnd).ok_or_else(|| {
        format!(
            "HWND wire value {hwnd} is outside the canonical Win32 USER-handle range 1..=4294967295"
        )
    })?;
    let seed = HWND(native as *mut std::ffi::c_void);
    if seed.0.is_null() || !unsafe { IsWindow(Some(seed)) }.as_bool() {
        return Err(format!("element HWND 0x{hwnd:x} is not a live window"));
    }
    let root = unsafe { GetAncestor(seed, GA_ROOT) };
    let root = if root.0.is_null() { seed } else { root };
    if !unsafe { IsWindow(Some(root)) }.as_bool() {
        return Err(format!(
            "top-level root HWND 0x{:x} for element HWND 0x{hwnd:x} is not live",
            root.0 as usize
        ));
    }
    Ok(synapse_core::win32_hwnd::hwnd_to_wire(root.0 as isize))
}

#[cfg(not(windows))]
fn verified_top_level_hwnd(hwnd: i64) -> Result<i64, String> {
    synapse_core::win32_hwnd::hwnd_from_wire(hwnd)
        .map(|_native| hwnd)
        .ok_or_else(|| {
            format!(
                "HWND wire value {hwnd} is outside the canonical Win32 USER-handle range 1..=4294967295"
            )
        })
}

pub(crate) fn click_error_code(error: &ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned()
}

pub(crate) fn error_has_click_tier_attempts(error: &ErrorData) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("tier_attempts"))
        .and_then(Value::as_array)
        .is_some_and(|attempts| !attempts.is_empty())
}

pub(crate) fn click_reason_for_error_code(error_code: &str) -> &'static str {
    match error_code {
        error_codes::FOREGROUND_ACTIVATION_REFUSED => CLICK_REASON_FOREGROUND_REFUSED,
        error_codes::ACTION_FOREGROUND_LEASE_BUSY => "foreground_lease_busy",
        error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED => CLICK_REASON_PATTERN_UNSUPPORTED,
        error_codes::TRANSIENT_ELEMENT_EXPIRED | error_codes::A11Y_ELEMENT_STALE => {
            CLICK_REASON_ELEMENT_STALE
        }
        error_codes::ACTION_BACKEND_UNAVAILABLE
        | error_codes::ACTION_QUEUE_FULL
        | error_codes::ACTION_RATE_LIMITED
        | error_codes::A11Y_CDP_UNREACHABLE
        | error_codes::A11Y_CDP_ATTACH_FAILED
        | error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        | error_codes::A11Y_CDP_AXTREE_FAILED => CLICK_REASON_BACKEND_UNAVAILABLE,
        error_codes::ACTION_TARGET_INVALID | error_codes::ACTION_ELEMENT_NOT_RESOLVED => {
            CLICK_REASON_TARGET_INVALID
        }
        error_codes::TOOL_PARAMS_INVALID => CLICK_REASON_PARAMS_INVALID,
        error_codes::ACTION_NO_OBSERVED_DELTA => CLICK_REASON_NO_OBSERVED_DELTA,
        _ => CLICK_REASON_ERROR,
    }
}

pub(super) fn acquire_click_foreground_lease(
    foreground_click_policy: &ForegroundClickPolicy,
    hold_ms: u32,
    tier_attempts: &mut Vec<ActClickTierAttempt>,
) -> Result<crate::m2::ForegroundInputLeaseGuard, ErrorData> {
    if let Some(error) = foreground_click_policy.foreground_refusal_error("act_click") {
        tier_attempts.push(click_tier_failed(
            CLICK_TIER_FOREGROUND,
            CLICK_REASON_FOREGROUND_REFUSED,
            error_codes::FOREGROUND_ACTIVATION_REFUSED,
            true,
            error.message.to_string(),
        ));
        return Err(attach_click_tier_attempts(error, tier_attempts.clone()));
    }
    // #2071 non-reducing rule, applied at this out-of-funnel acquisition site
    // too: an action must never renew a live same-owner lease DOWN below its
    // remaining TTL. The hold-derived TTL stays the floor; a caller-held longer
    // lease survives; the ceiling stays hard.
    let lease_ttl_ms = crate::m2::foreground_input_lease_ttl_for_hold_ms(hold_ms)
        .max(
            foreground_click_policy
                .session_id()
                .map_or(0, synapse_action::lease::owner_remaining_ttl_ms),
        )
        .min(synapse_action::MAX_LEASE_TTL_MS);
    match crate::m2::acquire_foreground_input_lease_with_ttl(
        "act_click",
        foreground_click_policy.session_id(),
        lease_ttl_ms,
    ) {
        Ok(guard) => Ok(guard),
        Err(error) => {
            let error_code = click_error_data_code(&error)
                .unwrap_or(error_codes::ACTION_FOREGROUND_LEASE_BUSY)
                .to_owned();
            tier_attempts.push(click_tier_failed(
                CLICK_TIER_FOREGROUND,
                click_reason_for_error_code(&error_code),
                error_code,
                true,
                error.message.to_string(),
            ));
            Err(attach_click_tier_attempts(error, tier_attempts.clone()))
        }
    }
}

fn click_error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

fn log_click_tier_attempt(attempt: &ActClickTierAttempt) {
    let tier = attempt.tier.as_str();
    let status = attempt.status.as_str();
    let reason_code = attempt.reason_code.as_deref().unwrap_or("none");
    let error_code = attempt.error_code.as_deref().unwrap_or("none");
    let detail = attempt.detail.as_deref().unwrap_or("");
    if status == "failed" {
        tracing::warn!(
            code = "M2_ACT_CLICK_TIER_ATTEMPT",
            kind = "act_click",
            tier,
            status,
            reason_code,
            error_code,
            required_foreground = attempt.required_foreground,
            detail,
            "act_click backend tier attempt failed"
        );
    } else {
        tracing::info!(
            code = "M2_ACT_CLICK_TIER_ATTEMPT",
            kind = "act_click",
            tier,
            status,
            reason_code,
            error_code,
            required_foreground = attempt.required_foreground,
            detail,
            "act_click backend tier attempt delivered"
        );
    }
}

fn validate_click_params(params: &ActClickParams) -> Result<(), ErrorData> {
    if !(1..=3).contains(&params.clicks) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_click clicks must be in 1..=3, got {}", params.clicks),
        ));
    }
    if params.hold_ms == 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_click hold_ms must be at least 1",
        ));
    }
    if params.hold_ms > MAX_CLICK_HOLD_MS {
        return Err(action_error_to_mcp(&ActionError::HoldExceededMax {
            detail: format!(
                "act_click hold_ms {} exceeds max {MAX_CLICK_HOLD_MS}",
                params.hold_ms
            ),
        }));
    }
    if !(50..=5000).contains(&params.verify_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_click verify_timeout_ms must be in 50..=5000, got {}",
                params.verify_timeout_ms
            ),
        ));
    }
    Ok(())
}

fn cdp_click_modifier_bits(modifiers: &[schema::ClickModifier]) -> i64 {
    modifiers.iter().fold(0_i64, |bits, modifier| {
        bits | match modifier {
            schema::ClickModifier::Alt => 1,
            schema::ClickModifier::Ctrl => 2,
            schema::ClickModifier::Super => 4,
            schema::ClickModifier::Shift => 8,
        }
    })
}

fn reject_click_modifiers_for_non_cdp(
    params: &ActClickParams,
    target_kind: &str,
) -> Result<(), ErrorData> {
    if params.modifiers.is_empty() {
        return Ok(());
    }
    Err(action_error_to_mcp(&ActionError::BackendUnavailable {
        detail: format!(
            "act_click modifiers are supported only for web (CDP) element targets, not {target_kind}"
        ),
    }))
}

fn ensure_element_transport_backend_allowed(
    params: &ActClickParams,
    transport: &str,
) -> Result<(), ErrorData> {
    if matches!(params.backend, Backend::Vigem | Backend::Hardware) {
        let tier = match transport {
            "CDP" => CLICK_TIER_CDP,
            "PostMessage" => CLICK_TIER_POSTMESSAGE,
            _ => CLICK_TIER_UIA,
        };
        let detail = format!(
            "act_click element target requested backend={} but {transport} element delivery is only valid for backend=auto or backend=software; no fallback delivery was attempted",
            backend_used_name(params.backend)
        );
        let error = action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: detail.clone(),
        });
        return Err(attach_click_tier_attempts(
            error,
            vec![click_tier_failed(
                tier,
                CLICK_REASON_BACKEND_UNAVAILABLE,
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                false,
                detail,
            )],
        ));
    }
    Ok(())
}

fn point_mouse_target(target: &ActClickTarget) -> Result<MouseTarget, ErrorData> {
    match target {
        ActClickTarget::Point(point) => Ok(MouseTarget::Screen {
            point: Point {
                x: point.x,
                y: point.y,
            },
        }),
        ActClickTarget::Element(element) => {
            let detail = format!(
                "act_click element target {} reached the point-target path unexpectedly",
                element.element_id
            );
            let error = action_error_to_mcp(&ActionError::TargetInvalid {
                detail: format!(
                    "act_click element target {} reached the point-target path unexpectedly",
                    element.element_id
                ),
            });
            Err(attach_click_tier_attempts(
                error,
                vec![click_tier_failed(
                    CLICK_TIER_FOREGROUND,
                    CLICK_REASON_TARGET_INVALID,
                    error_codes::ACTION_TARGET_INVALID,
                    true,
                    detail,
                )],
            ))
        }
    }
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    match error {
        ActionError::TransientElementExpired { element_id, detail } => {
            transient_element_expired_error(element_id, detail)
        }
        ActionError::ElementPatternUnsupported { element_id, detail } => {
            element_pattern_unsupported_error(element_id, detail)
        }
        _ => crate::m2::action_error_to_mcp(error),
    }
}

fn transient_element_expired_error(element_id: &ElementId, detail: &str) -> ErrorData {
    let root_hwnd = element_id.parts().ok().map(|parts| parts.hwnd);
    let recommended_pattern = "Call observe or find again immediately before acting on the transient UI, then pass the fresh element_id to act_click; do not reuse element_ids from expired toast/snackbar observations.";
    let tier_attempts = vec![click_tier_failed(
        CLICK_TIER_UIA,
        CLICK_REASON_ELEMENT_STALE,
        error_codes::TRANSIENT_ELEMENT_EXPIRED,
        false,
        detail.to_owned(),
    )];
    tracing::warn!(
        code = error_codes::TRANSIENT_ELEMENT_EXPIRED,
        element_id = %element_id,
        root_hwnd,
        detail,
        recommended_pattern,
        "act_click transient UI element expired before dispatch; no fallback click attempted"
    );
    attach_click_tier_attempts(
        ErrorData::new(
            ErrorCode(-32099),
            format!("transient UI element expired before act_click dispatch: {detail}"),
            Some(json!({
                "code": error_codes::TRANSIENT_ELEMENT_EXPIRED,
                "detail_code": "UIA_ELEMENT_STALE_AFTER_OBSERVE",
                "transient": true,
                "fallback_attempted": false,
                "element_id": element_id.to_string(),
                "root_hwnd": root_hwnd,
                "source_of_truth": "live UI Automation re-resolution under the element_id root HWND",
                "recommended_next_tools": ["observe", "find", "act_click"],
                "recommended_pattern": recommended_pattern,
                "detail": detail,
            })),
        ),
        tier_attempts,
    )
}

fn element_pattern_unsupported_error(element_id: &ElementId, detail: &str) -> ErrorData {
    let root_hwnd = element_id.parts().ok().map(|parts| parts.hwnd);
    let tier_attempts = vec![click_tier_failed(
        CLICK_TIER_UIA,
        CLICK_REASON_PATTERN_UNSUPPORTED,
        error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
        false,
        detail.to_owned(),
    )];
    tracing::warn!(
        code = error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
        element_id = %element_id,
        root_hwnd,
        attempted_patterns = ?SUPPORTED_UIA_CLICK_PATTERNS,
        detail,
        fallback_attempted = false,
        "act_click element target exposes no supported UIA click control pattern; no fallback delivery attempted"
    );
    attach_click_tier_attempts(
        ErrorData::new(
            ErrorCode(-32099),
            format!("element target exposes no supported UIA click control pattern: {detail}"),
            Some(json!({
                "code": error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
                "detail_code": "UIA_CONTROL_PATTERN_UNSUPPORTED",
                "transient": false,
                "fallback_attempted": false,
                "element_id": element_id.to_string(),
                "root_hwnd": root_hwnd,
                "attempted_patterns": SUPPORTED_UIA_CLICK_PATTERNS,
                "source_of_truth": "live UI Automation control-pattern availability on the re-resolved element",
                "router_escalation_required": true,
                "router_next_tier": "postmessage",
                "detail": detail,
            })),
        ),
        tier_attempts,
    )
}

const fn backend_used_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Auto | Backend::Software => "software",
        Backend::Vigem => "vigem",
        Backend::Hardware => "hardware",
    }
}
