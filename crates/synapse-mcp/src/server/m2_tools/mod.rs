use super::{
    ActClickParams, ActClickResponse, ActClipboardParams, ActClipboardResponse,
    ActFocusWindowParams, ActFocusWindowResponse, ActKeymapParams, ActKeymapResponse, ActPadParams,
    ActPadResponse, ActPressParams, ActPressResponse, ActScrollParams, ActScrollResponse,
    ActSetValueParams, ActSetValueResponse, ActStrokeParams, ActStrokeResponse, ActTypeParams,
    ActTypeResponse, ErrorData, Json, Parameters, ReleaseAllParams, ReleaseAllResponse,
    SessionTarget, SynapseService, act_click_with_handle_and_lease, act_clipboard_session_buffer,
    act_focus_window_request_details, act_focus_window_target_hwnd, act_set_value_request_details,
    act_stroke_validation_failure_details,
    action_preflight::{ActionPreflightReadback, ForegroundProof},
    release_all_with_handles, tool, tool_router, validate_act_stroke_params,
};
use crate::m1::{FindParams, FindResult, FindResultKind, FindScope, mcp_error};
use crate::m2::postcondition::{
    ActPostcondition, hash_json as verify_hash_json,
    no_observed_delta_error as source_no_observed_delta_error, postcondition_failed_error,
    postcondition_not_requested, postcondition_observed_delta, postcondition_target_window_closed,
};
use crate::m2::{
    ActClickPostcondition, ActClickTierAttempt, ActStrokePlan, CLICK_REASON_NO_OBSERVED_DELTA,
    CLICK_TIER_FOREGROUND, CLICK_TIER_POSTMESSAGE, ForegroundClickPolicy, HwndKeyboardTargetState,
    OperatorPanicActionBoundary, PressBackend, ResolvedKeymapPress, TypeBackend,
    act_click_postmessage_with_params, act_focus_window_with_boundary,
    act_keymap_response_from_press, act_pad_with_handle_and_boundary, act_press_cdp_target,
    act_press_normalized_labels, act_press_postmessage_target, act_press_with_handle_and_boundary,
    act_scroll_with_handle_and_boundary, act_set_value_with_boundary, act_stroke_cdp_target,
    act_stroke_error_details, act_stroke_request_details, act_stroke_with_handle_and_boundary,
    act_type_with_handle_and_boundary, action_from_press_params, action_from_type_params,
    attach_click_tier_attempts, click_params_can_route_background_first,
    click_target_foreground_guard_hwnds, click_target_root_hwnd, click_tier_delivered,
    click_tier_failed, emitted_text, hwnd_keyboard_target_state, resolve_keymap_press,
};
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use synapse_action::{
    ACTION_QUEUE_CAPACITY, ActionEmitterSnapshotHandle, ActionError, ActionHandle,
    ActionStateSnapshot, RecordingBackend, ResolvedBackend, TokenBucketSnapshot,
};
use synapse_core::{
    AccessibleNode, Action, AimCurve, Backend, ButtonAction, ElementId, FocusedElement,
    ForegroundContext, KeystrokeDynamics, KeystrokeNaturalParams, MouseButton, MouseTarget,
    PathPoint, PathSpec, Point, Rect, StrokeMotionModel, StrokeTiming, UiaPattern, VelocityProfile,
    error_codes,
};
use synapse_perception::ObservationInput;
use tokio_util::sync::CancellationToken;

const ACT_STROKE_FOREGROUND_MONITOR_INTERVAL_MS: u64 = 10;
const ACTION_DIAGNOSTIC_RATE_LIMIT_CONFIRM: &str = "force-real-rate-limit-for-diagnostic";
const ACTION_DIAGNOSTIC_QUEUE_FULL_CONFIRM: &str = "saturate-real-action-queue-for-diagnostic";
const ACTION_DIAGNOSTIC_MAX_TTL_MS: u64 = 10_000;
const ACTION_DIAGNOSTIC_MIN_TTL_MS: u64 = 100;
const ACTION_DIAGNOSTIC_MAX_QUEUE_BLOCKER_MS: u32 = 10_000;
const ACTION_DIAGNOSTIC_MIN_QUEUE_BLOCKER_MS: u32 = 250;
const ACTION_DIAGNOSTIC_QUEUE_SETTLE_MS: u64 = 50;
const ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH: &str = "browser_target.url";
const ACT_TYPE_BROWSER_URL_TEXT_INTEGRITY: &str = "browser_target_url_readback";
const ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH: &str = "foreground_text_readback";
const ACT_TYPE_TEXT_INTEGRITY_PREFIX: &str = "verify_delta_text_readback";
const ACT_TYPE_TEXT_SOURCE_UIA_VALUE: &str = "uia_focused_value";
const ACT_TYPE_TEXT_SOURCE_UIA_EMPTY: &str = "uia_focused_empty_value_or_text";
const ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE: &str = "cdp_active_element_value";
const ACT_TYPE_TEXT_SOURCE_OCR_FOCUSED_RECT: &str = "ocr_focused_rect_text";
const ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_SOURCE_OF_TRUTH: &str =
    "chrome_bridge_active_element.value";
const ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_TEXT_INTEGRITY: &str =
    "chrome_bridge_active_element_value_readback";
const ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_TIER: &str = "chrome_bridge_active_element";
const ACT_TYPE_FOREGROUND_FALLBACK_CLICK_HOLD_MS: u32 = 120;
const ACT_TYPE_FOREGROUND_FALLBACK_CLICK_DURATION_MS: u32 = 50;
const ACT_TYPE_VERIFY_POLL_INTERVAL_MS: u64 = 50;
const BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH: &str = "os_foreground_window";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionDiagnosticRateLimitOverrideParams {
    #[serde(default)]
    pub confirm: String,
    #[serde(default = "default_diagnostic_ttl_ms")]
    #[schemars(default = "default_diagnostic_ttl_ms", range(min = 100, max = 10000))]
    pub ttl_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionDiagnosticRateLimitOverrideResponse {
    pub backend: String,
    pub ttl_ms: u64,
    pub before: TokenBucketReadback,
    pub after: TokenBucketReadback,
    pub reset_scheduled: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionDiagnosticQueueFullSetupParams {
    #[serde(default)]
    pub confirm: String,
    #[serde(default = "default_queue_blocker_duration_ms")]
    #[schemars(
        default = "default_queue_blocker_duration_ms",
        range(min = 250, max = 10000)
    )]
    pub blocker_duration_ms: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionDiagnosticQueueFullSetupResponse {
    pub backend: String,
    pub expected_queue_capacity: u32,
    pub blocker_duration_ms: u32,
    pub blocker_from: Point,
    pub blocker_to: Point,
    pub blocker_queued: bool,
    pub filler_attempts: u32,
    pub queued_fillers: u32,
    pub queue_full_observed: bool,
    pub next_act_stroke_expected_error: String,
}

#[derive(Copy, Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenBucketReadback {
    pub capacity: u32,
    pub tokens: u32,
    pub refill_rate_per_s: u32,
    pub last_refill_ns: u64,
}

const fn default_diagnostic_ttl_ms() -> u64 {
    5_000
}

const fn default_queue_blocker_duration_ms() -> u32 {
    5_000
}

#[tool_router(router = m2_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Click a screen coordinate or UI Automation element. Default element delivery uses background UIA control patterns (Invoke, Toggle, SelectionItem, ExpandCollapse, LegacyIAccessible.DoDefaultAction). For CDP web element targets, auto_wait=true scrolls into view and polls actionability before dispatch; timeout returns BROWSER_WAIT_TIMEOUT with precise unmet predicates. When element coordinate delivery is needed, Synapse tries a background HWND PostMessage click to the resolved child window before escalating to the leased foreground coordinate tier; enabled keyboard-focusable edit/document/text or Value/Text targets bypass PostMessage and use the leased foreground coordinate tier so the real caret/focus state is placed. verify_delta reads the target window SoT for element clicks. coordinate_fallback_on_unsupported=true allows bbox-center coordinate delivery only for enabled keyboard-focusable edit/document/text targets or elements exposing Value/Text patterns; set false to fail closed with ACTION_ELEMENT_PATTERN_UNSUPPORTED. This mouse click tool does not synthesize WM_CHAR/dead-key keyboard text; use act_type/act_set_value for text. velocity_profile controls coordinate-move timing only, while explicit spatial paths belong to act_stroke. If a previously observed transient element expired before dispatch, returns TRANSIENT_ELEMENT_EXPIRED with re-observe/find guidance."
    )]
    pub async fn act_click(
        &self,
        params: Parameters<ActClickParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActClickResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_click",
            "tool.invocation kind=act_click"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_click") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_click", &error, &request_context);
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_click",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_click",
            &action_preflight_details(&preflight),
            &request_context,
        )?;
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_click",
            click_claim_target(&params),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_click", error, &request_context);
        }
        if let Err(error) = maybe_auto_wait_for_actionability(
            self,
            "act_click",
            &request_context,
            params.auto_wait,
            click_auto_wait_element_id(&params),
            params.auto_wait_timeout_ms,
            None,
            None,
            ActionabilityAutoWaitRequirement::Action,
            boundary,
        )
        .await
        {
            let result: Result<ActClickResponse, ErrorData> = Err(error);
            self.audit_action_result_for_request("act_click", &result, &request_context)?;
            return result.map(Json);
        }
        let target_window_hwnd = if params.verify_delta {
            match click_target_root_hwnd(&params) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let result: Result<ActClickResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_click", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let before_delta_signature = if params.verify_delta {
            match self
                .capture_click_delta_signature(160, target_window_hwnd)
                .await
            {
                Ok(signature) => Some(signature),
                Err(error) => {
                    let result: Result<ActClickResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_click", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let verify_timeout_ms = params.verify_timeout_ms;
        let foreground_click_policy = self.foreground_click_policy_for_request(&request_context)?;
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        let foreground_guard = match act_click_target_foreground_guard(&params) {
            Ok(guard) => guard,
            Err(error) => {
                let result: Result<ActClickResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_click", &result, &request_context)?;
                return result.map(Json);
            }
        };
        let foreground_before = if foreground_guard.is_some() {
            match self.current_audit_foreground() {
                Ok(foreground) => Some(foreground),
                Err(error) => {
                    let result: Result<ActClickResponse, ErrorData> =
                        Err(act_click_foreground_read_error("before", "unknown", &error));
                    self.audit_action_result_for_request("act_click", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let started = Instant::now();
        let result = if let Some(before) = before_delta_signature {
            self.act_click_with_verified_router(
                handle,
                recording,
                params,
                before,
                verify_timeout_ms,
                target_window_hwnd,
                foreground_click_policy,
                started,
                boundary,
            )
            .await
        } else {
            act_click_with_handle_and_lease(
                handle,
                recording,
                params,
                foreground_click_policy,
                boundary,
            )
            .await
        };
        let result = match result {
            Ok(response) if response.required_foreground => Ok(response),
            other => match (foreground_guard, foreground_before) {
                (Some(guard), Some(before)) => {
                    let action_source_of_truth = background_result_source_of_truth(
                        &other,
                        |response| {
                            response
                                .postcondition
                                .source_of_truth
                                .as_deref()
                                .unwrap_or("act_click.background_target")
                        },
                        "act_click.background_target",
                    );
                    match self.current_audit_foreground() {
                        Ok(after) => background_result_with_foreground_guard(
                            "act_click",
                            &action_source_of_truth,
                            guard,
                            &before,
                            &after,
                            other,
                        ),
                        Err(error) => background_result_with_foreground_read_error(
                            other,
                            act_click_foreground_read_error(
                                "after",
                                &action_source_of_truth,
                                &error,
                            ),
                        ),
                    }
                }
                _ => other,
            },
        };
        self.audit_action_result_for_request("act_click", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Type text. With into_element, routes through background CDP insertText for web nodes, foreground-safe native HWND text messages for UIA-resolved edit controls, UIA ValuePattern.SetValue with value readback for native elements without a native edit HWND, or a leased foreground click/type fallback for verified Chromium UIA editable targets when CDP is unavailable and the target window is already foreground. For CDP into_element targets, auto_wait=true scrolls into view and waits for editable actionability before typing; timeout returns BROWSER_WAIT_TIMEOUT with precise unmet predicates. Without into_element, types through the leased foreground keyboard backend."
    )]
    pub async fn act_type(
        &self,
        params: Parameters<ActTypeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActTypeResponse>, ErrorData> {
        let mut params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_type",
            "tool.invocation kind=act_type"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_type") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_type", &error, &request_context);
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_type",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_type",
            &action_preflight_details(&preflight),
            &request_context,
        )?;
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_type",
            params.into_element.as_ref().and_then(element_claim_target),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_type", error, &request_context);
        }
        if let Err(error) = maybe_auto_wait_for_actionability(
            self,
            "act_type",
            &request_context,
            params.auto_wait,
            params.into_element.as_ref(),
            params.auto_wait_timeout_ms,
            None,
            None,
            ActionabilityAutoWaitRequirement::Editable,
            boundary,
        )
        .await
        {
            let result: Result<ActTypeResponse, ErrorData> = Err(error);
            self.audit_action_result_for_request("act_type", &result, &request_context)?;
            return result.map(Json);
        }
        let browser_url_policy = match act_type_browser_url_policy(&params) {
            Ok(policy) => policy,
            Err(error) => {
                let result: Result<ActTypeResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_type", &result, &request_context)?;
                return result.map(Json);
            }
        };
        let visual_delta_target_window_hwnd =
            match act_type_visual_delta_target_window(&params, browser_url_policy.as_ref()) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let result: Result<ActTypeResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
            };
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        let verify_timeout_ms = params.verify_timeout_ms;
        let emitted = emitted_text(&params);
        if let Some(session_id_for_target) = session_id.as_deref() {
            match self
                .act_type_chrome_bridge_session_target(
                    session_id_for_target,
                    &params,
                    &emitted,
                    boundary,
                )
                .await
            {
                Ok(Some(response)) => {
                    let result: Result<ActTypeResponse, ErrorData> = Ok(response);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
                Ok(None) => {}
                Err(error) => {
                    let result: Result<ActTypeResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        }
        let foreground_fallback =
            match act_type_chromium_foreground_fallback_target(params.into_element.as_ref()) {
                Ok(target) => target,
                Err(error) => {
                    let result: Result<ActTypeResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
            };
        let requires_foreground_route =
            act_type_requires_foreground_route(&params, foreground_fallback.as_ref());
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        if requires_foreground_route
            && let Err(error) = self.ensure_act_type_foreground(&preflight, recording.as_ref())
        {
            let result: Result<ActTypeResponse, ErrorData> = Err(error);
            self.audit_action_result_for_request("act_type", &result, &request_context)?;
            return result.map(Json);
        }
        let _lease_guard = if requires_foreground_route {
            match acquire_tool_foreground_input_lease(self, "act_type", &request_context) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    let result: Result<ActTypeResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let before_text_signature = if let Some(target) = foreground_fallback.as_ref() {
            let mut foreground_params = params.clone();
            foreground_params.into_element = None;
            if let Err(error) = action_from_type_params(&foreground_params) {
                let result: Result<ActTypeResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_type", &result, &request_context)?;
                return result.map(Json);
            }
            if let Err(error) = self.ensure_act_type_foreground_fallback_target(
                &preflight,
                target,
                recording.as_ref(),
            ) {
                let result: Result<ActTypeResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_type", &result, &request_context)?;
                return result.map(Json);
            }
            if let Err(error) = self
                .click_act_type_foreground_fallback_target(handle.clone(), target, boundary)
                .await
            {
                let result: Result<ActTypeResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_type", &result, &request_context)?;
                return result.map(Json);
            }
            let focus_readback = match self
                .capture_act_type_text_signature(160, true, false, None)
                .await
            {
                Ok(signature) => signature,
                Err(error) => {
                    let result: Result<ActTypeResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
            };
            if let Err(error) =
                act_type_foreground_fallback_focus_matches_target(target, &focus_readback.signature)
            {
                let result: Result<ActTypeResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_type", &result, &request_context)?;
                return result.map(Json);
            }
            tracing::info!(
                code = "M2_ACT_TYPE_CHROMIUM_FOREGROUND_FALLBACK_READY",
                element_id = %target.element_id,
                root_hwnd = target.root_hwnd,
                role = %target.role,
                "readback=foreground_text tool=act_type into_element_fallback=chromium_uia_value_pattern_refused"
            );
            params = foreground_params;
            params.verify_delta.then_some(focus_readback)
        } else if act_type_should_capture_text_signature(&params) {
            match self
                .capture_act_type_text_signature(
                    160,
                    browser_url_policy.is_none() && visual_delta_target_window_hwnd.is_none(),
                    browser_url_policy.is_some(),
                    session_id.as_deref(),
                )
                .await
            {
                Ok(signature) => Some(signature),
                Err(error) => {
                    let result: Result<ActTypeResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_type", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let before_visual_signature =
            if act_type_should_capture_visual_signature(&params, visual_delta_target_window_hwnd) {
                match self
                    .capture_action_delta_signature(
                        160,
                        None,
                        false,
                        visual_delta_target_window_hwnd,
                    )
                    .await
                {
                    Ok(signature) => Some(signature),
                    Err(error) => {
                        let result: Result<ActTypeResponse, ErrorData> = Err(error);
                        self.audit_action_result_for_request(
                            "act_type",
                            &result,
                            &request_context,
                        )?;
                        return result.map(Json);
                    }
                }
            } else {
                None
            };
        let result = act_type_with_handle_and_boundary(handle, recording, params, boundary).await;
        let result = match (result, before_text_signature) {
            (Ok(response), Some(before)) => match self
                .verify_act_type_response(
                    response.clone(),
                    before,
                    verify_timeout_ms,
                    &emitted,
                    browser_url_policy.as_ref(),
                    session_id.as_deref(),
                )
                .await
            {
                Ok(response) => Ok(response),
                Err(error) if act_type_error_allows_visual_delta(&error) => {
                    match before_visual_signature {
                        Some(before_visual) => {
                            self.verify_act_type_visual_delta_response(
                                response,
                                before_visual,
                                verify_timeout_ms,
                                visual_delta_target_window_hwnd,
                                &error,
                            )
                            .await
                        }
                        None => Err(error),
                    }
                }
                Err(error) => Err(error),
            },
            (other, _) => other,
        };
        self.audit_action_result_for_request("act_type", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Set a UI Automation element's text/value without foreground. For known native edit HWNDs, routes through Win32 WM_SETTEXT; otherwise routes through UIA ValuePattern.SetValue. Both tiers require a separate target readback from the same Source of Truth and fail closed with probed tier details; there is no keyboard/foreground fallback."
    )]
    pub async fn act_set_value(
        &self,
        params: Parameters<ActSetValueParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActSetValueResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_set_value",
            "tool.invocation kind=act_set_value"
        );
        let request_details = act_set_value_request_details(&params);
        let preflight = match self.ensure_supported_use_allows_action("act_set_value") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    "act_set_value",
                    &error,
                    &request_details,
                    &request_context,
                );
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_set_value",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_set_value",
            &json!({
                "request": request_details,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_set_value",
            element_claim_target(&params.element_id),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_set_value", error, &request_context);
        }
        let foreground_guard = match act_set_value_target_foreground_guard(&params.element_id) {
            Ok(guard) => guard,
            Err(error) => {
                let result: Result<ActSetValueResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_set_value", &result, &request_context)?;
                return result.map(Json);
            }
        };
        let foreground_before = match self.current_audit_foreground() {
            Ok(foreground) => foreground,
            Err(error) => {
                let result: Result<ActSetValueResponse, ErrorData> = Err(
                    act_set_value_foreground_read_error("before", "unknown", &error),
                );
                self.audit_action_result_for_request("act_set_value", &result, &request_context)?;
                return result.map(Json);
            }
        };
        let result = match act_set_value_with_boundary(params, boundary).await {
            Ok(response) if response.required_foreground => Ok(response),
            other => {
                let action_source_of_truth = background_result_source_of_truth(
                    &other,
                    |response| response.source_of_truth.as_str(),
                    "act_set_value.background_tier",
                );
                match self.current_audit_foreground() {
                    Ok(foreground_after) => background_result_with_foreground_guard(
                        "act_set_value",
                        &action_source_of_truth,
                        foreground_guard,
                        &foreground_before,
                        &foreground_after,
                        other,
                    ),
                    Err(error) => background_result_with_foreground_read_error(
                        other,
                        act_set_value_foreground_read_error(
                            "after",
                            &action_source_of_truth,
                            &error,
                        ),
                    ),
                }
            }
        };
        self.audit_action_result_for_request("act_set_value", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Set a field's text by REPLACING its full content — clear + type + verify in one call (the form-filling primitive; #882/#1299). Call with element_id, or with locator {window_hwnd?, role?, name?, name_substring?, automation_id?} so Synapse resolves a fresh UIA element at action time; when a locator-backed action hits A11Y_ELEMENT_STALE, Synapse re-resolves and retries once. CDP web element ids use a background select-all + insertText replace with a separate node-value readback; auto_wait=true scrolls CDP web nodes into view and waits for editable actionability before replacement. Chromium UIA editable targets (when CDP is unavailable) use the leased foreground tier — the target window must already be foreground (act_focus_window first), then click, Ctrl+A, type, and a separate UIA value readback; native elements route through the act_set_value background tiers. Empty text clears the field. Every tier fails closed with its own reason code — there is no cross-tier fallback and no append behavior."
    )]
    pub async fn act_set_field_text(
        &self,
        params: Parameters<crate::m2::ActSetFieldTextParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<crate::m2::ActSetFieldTextResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_set_field_text",
            "tool.invocation kind=act_set_field_text"
        );
        let request_details = crate::m2::act_set_field_text_request_details(&params);
        let preflight = match self.ensure_supported_use_allows_action("act_set_field_text") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    "act_set_field_text",
                    &error,
                    &request_details,
                    &request_context,
                );
                return Err(error);
            }
        };
        self.audit_action_started_with_details_for_request(
            "act_set_field_text",
            &json!({
                "request": request_details,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        if let Err(error) = crate::m2::validate_set_field_text_params(&params) {
            let result: Result<crate::m2::ActSetFieldTextResponse, ErrorData> = Err(error);
            self.audit_action_result_for_request("act_set_field_text", &result, &request_context)?;
            return result.map(Json);
        }
        let params =
            match self.act_set_field_text_resolve_params(&params, &request_context, "initial") {
                Ok(params) => params,
                Err(error) => {
                    let result: Result<crate::m2::ActSetFieldTextResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request(
                        "act_set_field_text",
                        &result,
                        &request_context,
                    )?;
                    return result.map(Json);
                }
            };
        let element_id = match crate::m2::required_element_id(&params) {
            Ok(element_id) => element_id.clone(),
            Err(error) => {
                let result: Result<crate::m2::ActSetFieldTextResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request(
                    "act_set_field_text",
                    &result,
                    &request_context,
                )?;
                return result.map(Json);
            }
        };
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_set_field_text",
            element_claim_target(&element_id),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_set_field_text", error, &request_context);
        }
        let result = self
            .act_set_field_text_execute_resolved(&params, &preflight, &request_context, "initial")
            .await;
        let result = if set_field_text_error_is_stale(&result) && params.locator.is_some() {
            tracing::warn!(
                code = "M2_ACT_SET_FIELD_TEXT_STALE_RETRY",
                element_id = %element_id,
                "act_set_field_text target went stale; re-resolving locator and retrying once"
            );
            match self.act_set_field_text_resolve_params(&params, &request_context, "stale_retry") {
                Ok(retry_params) => {
                    let retry_element_id = crate::m2::required_element_id(&retry_params)?.clone();
                    if let Err(error) = self.ensure_target_claim_allows_action(
                        "act_set_field_text",
                        element_claim_target(&retry_element_id),
                        &request_context,
                    ) {
                        Err(error)
                    } else {
                        self.act_set_field_text_execute_resolved(
                            &retry_params,
                            &preflight,
                            &request_context,
                            "stale_retry",
                        )
                        .await
                    }
                }
                Err(error) => Err(error),
            }
        } else {
            result
        };
        self.audit_action_result_for_request("act_set_field_text", &result, &request_context)?;
        result.map(Json)
    }

    async fn act_set_field_text_execute_resolved(
        &self,
        params: &crate::m2::ActSetFieldTextParams,
        preflight: &ActionPreflightReadback,
        request_context: &RequestContext<RoleServer>,
        resolution_phase: &'static str,
    ) -> Result<crate::m2::ActSetFieldTextResponse, ErrorData> {
        let element_id = crate::m2::required_element_id(params)?;
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_set_field_text",
            preflight.operator_panic_epoch_at_arm,
        );
        maybe_auto_wait_for_actionability(
            self,
            "act_set_field_text",
            request_context,
            params.auto_wait,
            Some(element_id),
            params.auto_wait_timeout_ms,
            None,
            None,
            ActionabilityAutoWaitRequirement::Editable,
            boundary,
        )
        .await?;
        let route = match crate::m2::set_field_text_route(element_id) {
            Ok(route) => route,
            Err(error) => {
                return Err(error);
            }
        };
        tracing::debug!(
            code = "M2_ACT_SET_FIELD_TEXT_ROUTE_RESOLVED",
            element_id = %element_id,
            resolution_phase,
            "readback=act_set_field_text route resolved"
        );
        match route {
            #[cfg(windows)]
            crate::m2::SetFieldTextRoute::Web { backend_node_id } => {
                self.act_set_field_text_background_guarded(params, |params| {
                    Box::pin(crate::m2::act_set_field_text_web(
                        params,
                        backend_node_id,
                        boundary,
                    ))
                })
                .await
            }
            #[cfg(not(windows))]
            crate::m2::SetFieldTextRoute::Web { .. } => Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "act_set_field_text web tier is only available on Windows",
            )),
            crate::m2::SetFieldTextRoute::ChromiumForeground {
                root_hwnd,
                process_name,
                metadata,
            } => {
                self.act_set_field_text_foreground_tier(
                    params,
                    root_hwnd,
                    &process_name,
                    metadata,
                    preflight,
                    boundary,
                    request_context,
                )
                .await
            }
            crate::m2::SetFieldTextRoute::NativeBackground => {
                self.act_set_field_text_background_guarded(params, |params| {
                    Box::pin(crate::m2::act_set_field_text_native(params, boundary))
                })
                .await
            }
        }
    }

    fn act_set_field_text_resolve_params(
        &self,
        params: &crate::m2::ActSetFieldTextParams,
        request_context: &RequestContext<RoleServer>,
        resolution_phase: &'static str,
    ) -> Result<crate::m2::ActSetFieldTextParams, ErrorData> {
        let Some(locator) = params.locator.as_ref() else {
            return if params.element_id.is_some() {
                Ok(params.clone())
            } else {
                Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "act_set_field_text requires element_id or locator",
                ))
            };
        };
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
        let target = if let Some(session_id) = session_id.as_deref() {
            self.memory_session_target(session_id)?
                .or(self.persisted_session_target_read_model(session_id)?)
        } else {
            None
        };
        let window_hwnd = set_field_text_locator_window_hwnd(params, locator, target.as_ref())?;
        let element_id =
            self.act_set_field_text_resolve_locator(locator, window_hwnd, resolution_phase)?;
        Ok(crate::m2::params_with_resolved_element(params, element_id))
    }

    fn act_set_field_text_resolve_locator(
        &self,
        locator: &crate::m2::ActSetFieldTextLocator,
        window_hwnd: i64,
        resolution_phase: &'static str,
    ) -> Result<ElementId, ErrorData> {
        let find_params = set_field_text_locator_find_params(locator, window_hwnd);
        let input = {
            let mut state = self.m1_state()?;
            crate::m1::build_find_input(&mut state, &find_params, Some(window_hwnd))?
        };
        let response = crate::m1::match_find_input(&input, &find_params);
        let results = response
            .results
            .into_iter()
            .filter(set_field_text_find_result_is_element)
            .filter(|result| set_field_text_locator_exact_name_matches(locator, result))
            .collect::<Vec<_>>();
        match results.as_slice() {
            [result] => {
                let element_id = result.element_id.clone().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "act_set_field_text locator matched an element result without element_id",
                    )
                })?;
                tracing::info!(
                    code = "M2_ACT_SET_FIELD_TEXT_LOCATOR_RESOLVED",
                    element_id = %element_id,
                    window_hwnd,
                    resolution_phase,
                    role = result.role.as_deref().unwrap_or(""),
                    automation_id_present = result.automation_id.is_some(),
                    "readback=locator tool=act_set_field_text outcome=resolved"
                );
                Ok(element_id)
            }
            [] => Err(set_field_text_locator_resolution_error(
                locator,
                window_hwnd,
                resolution_phase,
                "not_found",
                results.as_slice(),
            )),
            many => Err(set_field_text_locator_resolution_error(
                locator,
                window_hwnd,
                resolution_phase,
                "ambiguous",
                many,
            )),
        }
    }

    #[tool(
        description = "Operator-intent foreground action: lease-gated focus/activation of one visible top-level native window by exact hwnd, unique title_regex, or unique pid. Do not use as an action/perception precondition. It fails closed on missing, ambiguous, or contended targets and verifies success with a separate GetForegroundWindow readback."
    )]
    pub async fn act_focus_window(
        &self,
        params: Parameters<ActFocusWindowParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActFocusWindowResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_focus_window",
            "tool.invocation kind=act_focus_window"
        );
        let request_details = act_focus_window_request_details(&params);
        let preflight = match self.ensure_supported_use_allows_action("act_focus_window") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    "act_focus_window",
                    &error,
                    &request_details,
                    &request_context,
                );
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_focus_window",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_focus_window",
            &json!({
                "request": request_details,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        let focus_claim_target = match act_focus_window_target_hwnd(&params) {
            Ok(hwnd) => super::target_claims::window_session_target(hwnd),
            Err(error) => {
                return audit_target_claim_denial(
                    self,
                    "act_focus_window",
                    error,
                    &request_context,
                );
            }
        };
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_focus_window",
            Some(focus_claim_target),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_focus_window", error, &request_context);
        }
        let mut lease_guard =
            match acquire_tool_foreground_input_lease(self, "act_focus_window", &request_context) {
                Ok(guard) => guard,
                Err(error) => {
                    self.audit_action_error_with_details_for_request(
                        "act_focus_window",
                        &error,
                        &request_details,
                        &request_context,
                    )?;
                    return Err(error);
                }
            };
        lease_guard.disable_context_restore("act_focus_window_intentional_foreground_change");
        let result = act_focus_window_with_boundary(params, boundary).await;
        self.audit_action_result_for_request("act_focus_window", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Press a keyboard key or ordered chord. With an active session target and backend=auto/software, Synapse first uses background delivery: CDP Input.dispatchKeyEvent for CDP targets or HWND PostMessage keyboard messages for window targets. auto_wait=true with auto_wait_element_id scrolls that CDP web node into view and waits for actionability before pressing. PostMessage delivery is accepted only after a separate target text/selection readback changes; ignored posted keys fail with ACTION_NO_OBSERVED_DELTA. backend=hardware, recording, no active target, or declared foreground-transition verification uses the leased foreground keyboard path."
    )]
    pub async fn act_press(
        &self,
        params: Parameters<ActPressParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActPressResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_press",
            "tool.invocation kind=act_press"
        );
        super::context::maybe_force_panic_during_act("act_press");
        let preflight = match self.ensure_supported_use_allows_action("act_press") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_press", &error, &request_context);
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_press",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_press",
            &action_preflight_details(&preflight),
            &request_context,
        )?;
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_press",
            params
                .auto_wait_element_id
                .as_ref()
                .and_then(element_claim_target),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_press", error, &request_context);
        }
        let foreground_change_policy = match act_press_foreground_change_policy(&params) {
            Ok(policy) => policy,
            Err(error) => {
                let result: Result<ActPressResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_press", &result, &request_context)?;
                return result.map(Json);
            }
        };
        if let Err(error) = action_from_press_params(&params) {
            let result: Result<ActPressResponse, ErrorData> = Err(error);
            self.audit_action_result_for_request("act_press", &result, &request_context)?;
            return result.map(Json);
        }
        if let Err(error) = maybe_auto_wait_for_actionability(
            self,
            "act_press",
            &request_context,
            params.auto_wait,
            params.auto_wait_element_id.as_ref(),
            params.auto_wait_timeout_ms,
            params.window_hwnd,
            params.cdp_target_id.as_deref(),
            ActionabilityAutoWaitRequirement::Action,
            boundary,
        )
        .await
        {
            let result: Result<ActPressResponse, ErrorData> = Err(error);
            self.audit_action_result_for_request("act_press", &result, &request_context)?;
            return result.map(Json);
        }
        let (handle, recording, connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        match self
            .try_act_press_background_target(
                params.clone(),
                recording.is_some(),
                &request_context,
                boundary,
            )
            .await
        {
            Ok(Some(response)) => {
                let result = Ok(response);
                self.audit_action_result_for_request("act_press", &result, &request_context)?;
                return result.map(Json);
            }
            Ok(None) => {}
            Err(error) => {
                let result: Result<ActPressResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_press", &result, &request_context)?;
                return result.map(Json);
            }
        }
        let before_delta_signature = if params.verify_delta {
            match self
                .capture_action_delta_signature(160, None, false, None)
                .await
            {
                Ok(signature) => Some(signature),
                Err(error) => {
                    let result: Result<ActPressResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_press", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let verify_timeout_ms = params.verify_timeout_ms;
        let _lease_guard = match acquire_tool_foreground_input_lease_with_ttl(
            self,
            "act_press",
            &request_context,
            lease_ttl_for_hold_ms(params.hold_ms),
        ) {
            Ok(guard) => guard,
            Err(error) => {
                let result: Result<ActPressResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_press", &result, &request_context)?;
                return result.map(Json);
            }
        };
        let result = act_press_with_handle_and_boundary(
            handle,
            recording,
            connection_closed_cancel,
            params,
            boundary,
        )
        .await;
        let result = match (result, before_delta_signature) {
            (Ok(response), Some(before)) => {
                self.verify_act_press_response(
                    response,
                    before,
                    verify_timeout_ms,
                    foreground_change_policy,
                )
                .await
            }
            (other, _) => other,
        };
        self.audit_action_result_for_request("act_press", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Press a keyboard alias from the active profile keymap. With an active session target and backend=auto/software, resolves the alias before any lease and routes through the same background CDP/PostMessage keyboard tiers as act_press; hardware, recording, or no active target uses the leased foreground keyboard path."
    )]
    pub async fn act_keymap(
        &self,
        params: Parameters<ActKeymapParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActKeymapResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_keymap",
            alias = %params.alias,
            "tool.invocation kind=act_keymap"
        );
        let request_details = json!({
            "alias": params.alias.trim(),
            "hold_ms": params.hold_ms,
            "backend": params.backend,
        });
        let preflight = match self.ensure_supported_use_allows_action("act_keymap") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    "act_keymap",
                    &error,
                    &request_details,
                    &request_context,
                );
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_keymap",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_keymap",
            &json!({
                "request": request_details,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        if let Err(error) =
            self.ensure_target_claim_allows_action("act_keymap", None, &request_context)
        {
            return audit_target_claim_denial(self, "act_keymap", error, &request_context);
        }
        let profile = {
            let runtime = self.profile_runtime()?;
            let active_profile_id = runtime
                .active_profile_id()
                .map_err(|error| mcp_error(error.code(), error.to_string()))?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::PROFILE_NOT_FOUND,
                        "act_keymap requires an active profile",
                    )
                })?;
            runtime
                .profile(&active_profile_id)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::PROFILE_NOT_FOUND,
                        format!("active profile {active_profile_id} was not found"),
                    )
                })?
        };
        let resolved = match resolve_keymap_press(&profile, &params) {
            Ok(resolved) => resolved,
            Err(error) => {
                let result: Result<ActKeymapResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_keymap", &result, &request_context)?;
                return result.map(Json);
            }
        };
        let (handle, recording, connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        match self
            .try_act_keymap_background_target(
                &resolved,
                recording.is_some(),
                &request_context,
                boundary,
            )
            .await
        {
            Ok(Some(response)) => {
                let result = Ok(response);
                self.audit_action_result_for_request("act_keymap", &result, &request_context)?;
                return result.map(Json);
            }
            Ok(None) => {}
            Err(error) => {
                let result: Result<ActKeymapResponse, ErrorData> = Err(error);
                self.audit_action_result_for_request("act_keymap", &result, &request_context)?;
                return result.map(Json);
            }
        }
        let _lease_guard = match acquire_tool_foreground_input_lease_with_ttl(
            self,
            "act_keymap",
            &request_context,
            lease_ttl_for_hold_ms(params.hold_ms),
        ) {
            Ok(guard) => guard,
            Err(error) => {
                self.audit_action_error_with_details_for_request(
                    "act_keymap",
                    &error,
                    &request_details,
                    &request_context,
                )?;
                return Err(error);
            }
        };
        let result = act_press_with_handle_and_boundary(
            handle,
            recording,
            connection_closed_cancel,
            resolved.press.clone(),
            boundary,
        )
        .await
        .map(|response| act_keymap_response_from_press(&resolved, response));
        self.audit_action_result_for_request("act_keymap", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Move, aim, or drag to a point/element target or along an explicit spatial path using timed continuous mouse samples; button unset moves/aims and button set drags; motion_model defaults to path and can use wind_mouse for point-to-point line strokes"
    )]
    pub async fn act_stroke(
        &self,
        params: Parameters<ActStrokeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActStrokeResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_stroke",
            "tool.invocation kind=act_stroke"
        );
        let plan = match validate_act_stroke_params(&params) {
            Ok(plan) => plan,
            Err(error) => {
                let failure_details = act_stroke_validation_failure_details(&params, &error);
                log_act_stroke_failure(&failure_details, &error);
                self.audit_action_error_with_details_for_request(
                    "act_stroke",
                    &error,
                    &failure_details,
                    &request_context,
                )?;
                return Err(error);
            }
        };
        let stroke_details = act_stroke_request_details(&params, &plan);
        let preflight = match self.ensure_supported_use_allows_action("act_stroke") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    "act_stroke",
                    &error,
                    &json!({
                        "stroke": stroke_details,
                        "failure": act_stroke_error_details(&error),
                    }),
                    &request_context,
                );
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_stroke",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_stroke",
            &act_stroke_audit_details(&stroke_details, &preflight),
            &request_context,
        )?;
        if let Err(error) =
            self.ensure_target_claim_allows_action("act_stroke", None, &request_context)
        {
            return audit_target_claim_denial(self, "act_stroke", error, &request_context);
        }
        let verify_timeout_ms = params.verify_timeout_ms;
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        match self
            .try_act_stroke_cdp_background_target(
                &params,
                &plan,
                recording.is_some(),
                &request_context,
                boundary,
            )
            .await
        {
            Ok(Some(response)) => {
                self.audit_action_ok_with_details_for_request(
                    "act_stroke",
                    &json!({
                        "response": response,
                        "stroke": stroke_details,
                        "preflight": preflight,
                    }),
                    &request_context,
                )?;
                return Ok(Json(response));
            }
            Ok(None) => {}
            Err(error) => {
                let failure_details =
                    act_stroke_failure_audit_details(&stroke_details, &preflight, &error);
                log_act_stroke_failure(&failure_details, &error);
                self.audit_action_error_with_details_for_request(
                    "act_stroke",
                    &error,
                    &failure_details,
                    &request_context,
                )?;
                return Err(error);
            }
        }
        let before_delta_signature = if params.verify_delta {
            match self
                .capture_action_delta_signature(160, None, true, None)
                .await
            {
                Ok(signature) => Some(signature),
                Err(error) => {
                    self.audit_action_error_with_details_for_request(
                        "act_stroke",
                        &error,
                        &act_stroke_failure_audit_details(&stroke_details, &preflight, &error),
                        &request_context,
                    )?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let _lease_guard = if should_acquire_act_stroke_input_lease(
            recording.is_some(),
            plan.requires_input_lease(),
        ) {
            match acquire_tool_foreground_input_lease(self, "act_stroke", &request_context) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    let failure_details =
                        act_stroke_failure_audit_details(&stroke_details, &preflight, &error);
                    log_act_stroke_failure(&failure_details, &error);
                    self.audit_action_error_with_details_for_request(
                        "act_stroke",
                        &error,
                        &failure_details,
                        &request_context,
                    )?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let foreground_monitor =
            should_monitor_act_stroke_foreground(recording.is_some(), plan.requires_input_lease())
                .then(|| self.start_act_stroke_foreground_monitor(&preflight));
        let result = act_stroke_with_handle_and_boundary(
            handle,
            recording,
            params.clone(),
            plan.clone(),
            boundary,
        )
        .await;
        let foreground_error = await_act_stroke_foreground_monitor(foreground_monitor).await;
        let result = match foreground_error {
            Some(error) => Err(error),
            None => result,
        };
        let result = match (result, before_delta_signature) {
            (Ok(response), Some(before)) => {
                self.verify_act_stroke_response(response, before, verify_timeout_ms)
                    .await
            }
            (other, _) => other,
        };
        match &result {
            Ok(response) => {
                self.audit_action_ok_with_details_for_request(
                    "act_stroke",
                    &json!({
                        "response": response,
                        "stroke": stroke_details,
                        "preflight": preflight,
                    }),
                    &request_context,
                )?;
            }
            Err(error) => {
                let failure_details =
                    act_stroke_failure_audit_details(&stroke_details, &preflight, error);
                log_act_stroke_failure(&failure_details, error);
                self.audit_action_error_with_details_for_request(
                    "act_stroke",
                    error,
                    &failure_details,
                    &request_context,
                )?;
            }
        }
        result.map(Json)
    }

    #[cfg(test)]
    pub(crate) async fn act_clipboard_for_session_test_entrypoint(
        &self,
        params: Parameters<ActClipboardParams>,
        session_id: &str,
    ) -> Result<Json<ActClipboardResponse>, ErrorData> {
        let params = params.0;
        let request_details = clipboard_request_audit_details(&params);
        self.audit_action_started_with_details_for_session(
            "act_clipboard",
            &request_details,
            session_id,
        )?;
        let result = self.act_clipboard_for_session(params, session_id, "session_clipboard_buffer");
        match &result {
            Ok(response) => {
                self.audit_action_ok_with_details_for_session(
                    "act_clipboard",
                    &clipboard_response_audit_details(response),
                    session_id,
                )?;
            }
            Err(error) => {
                self.audit_action_error_with_details_for_session(
                    "act_clipboard",
                    error,
                    &request_details,
                    session_id,
                )?;
            }
        }
        result.map(Json)
    }

    #[tool(
        description = "Scroll vertically or horizontally at the current pointer or screen point"
    )]
    pub async fn act_scroll(
        &self,
        params: Parameters<ActScrollParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActScrollResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_scroll",
            "tool.invocation kind=act_scroll"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_scroll") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_scroll", &error, &request_context);
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_scroll",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_scroll",
            &action_preflight_details(&preflight),
            &request_context,
        )?;
        if let Err(error) = self.ensure_target_claim_allows_action(
            "act_scroll",
            scroll_claim_target(&params),
            &request_context,
        ) {
            return audit_target_claim_denial(self, "act_scroll", error, &request_context);
        }
        let point_region = params.verify_delta_point_region();
        let before_delta_signature = if params.verify_delta && !params.uses_element_target() {
            match self
                .capture_action_delta_signature(160, point_region, false, None)
                .await
            {
                Ok(signature) => Some(signature),
                Err(error) => {
                    let result: Result<ActScrollResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_scroll", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let verify_timeout_ms = params.verify_timeout_ms;
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        let foreground_guard = if params.uses_element_target() {
            match act_scroll_target_foreground_guard(&params) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    let result: Result<ActScrollResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_scroll", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let foreground_before = if foreground_guard.is_some() {
            match self.current_audit_foreground() {
                Ok(foreground) => Some(foreground),
                Err(error) => {
                    let result: Result<ActScrollResponse, ErrorData> = Err(
                        act_scroll_foreground_read_error("before", "unknown", &error),
                    );
                    self.audit_action_result_for_request("act_scroll", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let _lease_guard = if params.requires_input_lease() {
            match acquire_tool_foreground_input_lease(self, "act_scroll", &request_context) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    let result: Result<ActScrollResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_scroll", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let result = act_scroll_with_handle_and_boundary(handle, recording, params, boundary).await;
        let result = match (result, before_delta_signature) {
            (Ok(response), Some(before)) => {
                self.verify_act_scroll_response(response, before, verify_timeout_ms, point_region)
                    .await
            }
            (other, _) => other,
        };
        let result = match (result, foreground_guard, foreground_before) {
            (Ok(response), Some(guard), Some(before)) if !response.required_foreground => {
                let source_of_truth = response
                    .postcondition
                    .source_of_truth
                    .as_deref()
                    .unwrap_or("act_scroll.background_target");
                match self.current_audit_foreground() {
                    Ok(after) => {
                        match verify_background_target_not_activated(
                            "act_scroll",
                            source_of_truth,
                            guard,
                            &before,
                            &after,
                        ) {
                            Ok(()) => Ok(response),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(act_scroll_foreground_read_error(
                        "after",
                        source_of_truth,
                        &error,
                    )),
                }
            }
            (other, _, _) => other,
        };
        self.audit_action_result_for_request("act_scroll", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(description = "Apply a virtual gamepad report and optionally return it to neutral")]
    pub async fn act_pad(
        &self,
        params: Parameters<ActPadParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActPadResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_pad",
            "tool.invocation kind=act_pad"
        );
        let preflight = match self.ensure_supported_use_allows_action("act_pad") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_for_request("act_pad", &error, &request_context);
                return Err(error);
            }
        };
        let boundary = OperatorPanicActionBoundary::from_armed(
            "act_pad",
            preflight.operator_panic_epoch_at_arm,
        );
        self.audit_action_started_with_details_for_request(
            "act_pad",
            &action_preflight_details(&preflight),
            &request_context,
        )?;
        if let Err(error) =
            self.ensure_target_claim_allows_action("act_pad", None, &request_context)
        {
            return audit_target_claim_denial(self, "act_pad", error, &request_context);
        }
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_request(&request_context)?;
        let snapshot_handle = if params.verify_delta {
            Some(self.m2_snapshot_handle()?)
        } else {
            None
        };
        let before_snapshot = if let Some(snapshot_handle) = &snapshot_handle {
            match snapshot_handle.snapshot().await {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    let error = mcp_error(error.code(), error.to_string());
                    let result: Result<ActPadResponse, ErrorData> = Err(error);
                    self.audit_action_result_for_request("act_pad", &result, &request_context)?;
                    return result.map(Json);
                }
            }
        } else {
            None
        };
        let verify_timeout_ms = params.verify_timeout_ms;
        let result = act_pad_with_handle_and_boundary(handle, recording, params, boundary).await;
        let result = match (result, before_snapshot, snapshot_handle) {
            (Ok(response), Some(before), Some(snapshot_handle)) => {
                self.verify_act_pad_response(response, before, snapshot_handle, verify_timeout_ms)
                    .await
            }
            (other, _, _) => other,
        };
        self.audit_action_result_for_request("act_pad", &result, &request_context)?;
        result.map(Json)
    }

    #[tool(
        description = "Read, write, or clear this MCP session's virtual clipboard buffer. The default path is background-safe: it does not touch the real OS clipboard and does not acquire the foreground/input lease."
    )]
    pub async fn act_clipboard(
        &self,
        params: Parameters<ActClipboardParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActClipboardResponse>, ErrorData> {
        let params = params.0;
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::HTTP_SESSION_INVALID,
                    "act_clipboard requires an MCP session id for session-scoped virtual clipboard state",
                )
            })?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "act_clipboard",
            "tool.invocation kind=act_clipboard"
        );
        let request_details = clipboard_request_audit_details(&params);
        self.audit_action_started_with_details_for_session(
            "act_clipboard",
            &request_details,
            &session_id,
        )?;
        let result =
            self.act_clipboard_for_session(params, &session_id, "session_clipboard_buffer");
        match &result {
            Ok(response) => {
                self.audit_action_ok_with_details_for_session(
                    "act_clipboard",
                    &clipboard_response_audit_details(response),
                    &session_id,
                )?;
            }
            Err(error) => {
                self.audit_action_error_with_details_for_session(
                    "act_clipboard",
                    error,
                    &request_details,
                    &session_id,
                )?;
            }
        }
        result.map(Json)
    }

    #[tool(
        description = "Supporting diagnostic only: temporarily force the real software action rate limiter empty so the next real act_stroke exercises ACTION_RATE_LIMITED through the normal MCP action path. Its response is not an acceptance verdict; manual state verification remains separate."
    )]
    pub async fn action_diagnostic_rate_limit_override(
        &self,
        params: Parameters<ActionDiagnosticRateLimitOverrideParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActionDiagnosticRateLimitOverrideResponse>, ErrorData> {
        let params = params.0;
        const TOOL: &str = "action_diagnostic_rate_limit_override";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=action_diagnostic_rate_limit_override"
        );
        let request_details = json!({
            "backend": ResolvedBackend::Software.as_str(),
            "ttl_ms": params.ttl_ms,
        });
        if let Err(error) =
            require_diagnostic_confirm(&params.confirm, ACTION_DIAGNOSTIC_RATE_LIMIT_CONFIRM, TOOL)
        {
            self.audit_action_denied_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            );
            return Err(error);
        }
        if let Err(error) = validate_diagnostic_ttl_ms(params.ttl_ms) {
            self.audit_action_denied_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            );
            return Err(error);
        }
        let preflight = match self.ensure_supported_use_allows_action("act_stroke") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    TOOL,
                    &error,
                    &request_details,
                    &request_context,
                );
                return Err(error);
            }
        };
        self.audit_action_started_with_details_for_request(
            TOOL,
            &json!({
                "request": request_details,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        let boundary =
            OperatorPanicActionBoundary::from_armed(TOOL, preflight.operator_panic_epoch_at_arm);
        let control = self.m2_rate_limit_control()?;
        boundary.ensure("immediately_before_diagnostic_rate_limit_override")?;
        let override_readback = control
            .override_backend(ResolvedBackend::Software, 0, 0)
            .map_err(diagnostic_action_error_to_mcp)?;
        let response = ActionDiagnosticRateLimitOverrideResponse {
            backend: override_readback.backend.as_str().to_owned(),
            ttl_ms: params.ttl_ms,
            before: token_bucket_readback(override_readback.before),
            after: token_bucket_readback(override_readback.after),
            reset_scheduled: true,
        };
        schedule_rate_limit_reset(control, ResolvedBackend::Software, params.ttl_ms);
        self.audit_action_ok_with_details_for_request(
            TOOL,
            &json!({
                "response": response,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        Ok(Json(response))
    }

    #[tool(
        description = "Supporting diagnostic only: saturate the real bounded action queue behind a long software blocker so the next real act_stroke exercises ACTION_QUEUE_FULL through the normal MCP action path. Its response is not an acceptance verdict; manual state verification remains separate."
    )]
    pub async fn action_diagnostic_queue_full_setup(
        &self,
        params: Parameters<ActionDiagnosticQueueFullSetupParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ActionDiagnosticQueueFullSetupResponse>, ErrorData> {
        let params = params.0;
        const TOOL: &str = "action_diagnostic_queue_full_setup";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=action_diagnostic_queue_full_setup"
        );
        let request_details = json!({
            "backend": ResolvedBackend::Software.as_str(),
            "expected_queue_capacity": ACTION_QUEUE_CAPACITY,
            "blocker_duration_ms": params.blocker_duration_ms,
        });
        if let Err(error) =
            require_diagnostic_confirm(&params.confirm, ACTION_DIAGNOSTIC_QUEUE_FULL_CONFIRM, TOOL)
        {
            self.audit_action_denied_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            );
            return Err(error);
        }
        if let Err(error) = validate_queue_blocker_duration_ms(params.blocker_duration_ms) {
            self.audit_action_denied_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            );
            return Err(error);
        }
        let preflight = match self.ensure_supported_use_allows_action("act_stroke") {
            Ok(preflight) => preflight,
            Err(error) => {
                self.audit_action_denied_with_details_for_request(
                    TOOL,
                    &error,
                    &request_details,
                    &request_context,
                );
                return Err(error);
            }
        };
        self.audit_action_started_with_details_for_request(
            TOOL,
            &json!({
                "request": request_details,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        let boundary =
            OperatorPanicActionBoundary::from_armed(TOOL, preflight.operator_panic_epoch_at_arm);
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_session_id(None)?;
        if recording.is_some() {
            let error = mcp_error(
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                "action_diagnostic_queue_full_setup requires the real action emitter, not the recording backend",
            );
            self.audit_action_error_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            )?;
            return Err(error);
        }
        let from = synapse_action::backend::software::cursor_position()
            .map_err(diagnostic_action_error_to_mcp)?;
        let to = diagnostic_adjacent_point(from);
        boundary.ensure("immediately_before_diagnostic_queue_blocker")?;
        handle
            .try_execute(diagnostic_queue_blocker_action(
                from,
                to,
                params.blocker_duration_ms,
            ))
            .map_err(diagnostic_action_error_to_mcp)?;
        tokio::time::sleep(Duration::from_millis(ACTION_DIAGNOSTIC_QUEUE_SETTLE_MS)).await;
        let (filler_attempts, queued_fillers, queue_full_observed) =
            saturate_action_queue(&handle, boundary)?;
        if !queue_full_observed {
            let error = mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "action_diagnostic_queue_full_setup failed to observe ACTION_QUEUE_FULL after {filler_attempts} attempts and {queued_fillers} queued fillers"
                ),
            );
            self.audit_action_error_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            )?;
            return Err(error);
        }
        let response = ActionDiagnosticQueueFullSetupResponse {
            backend: ResolvedBackend::Software.as_str().to_owned(),
            expected_queue_capacity: u32::try_from(ACTION_QUEUE_CAPACITY).unwrap_or(u32::MAX),
            blocker_duration_ms: params.blocker_duration_ms,
            blocker_from: from,
            blocker_to: to,
            blocker_queued: true,
            filler_attempts,
            queued_fillers,
            queue_full_observed,
            next_act_stroke_expected_error: error_codes::ACTION_QUEUE_FULL.to_owned(),
        };
        self.audit_action_ok_with_details_for_request(
            TOOL,
            &json!({
                "response": response,
                "preflight": preflight,
            }),
            &request_context,
        )?;
        Ok(Json(response))
    }

    #[tool(description = "Release all held keyboard, mouse, and gamepad input state")]
    pub async fn release_all(
        &self,
        params: Parameters<ReleaseAllParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ReleaseAllResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "release_all",
            "tool.invocation kind=release_all"
        );
        let (handle, snapshot_handle, reflex_runtime) = self.m2_release_all_context()?;
        let result =
            release_all_with_handles(handle, snapshot_handle, reflex_runtime, params.0).await;
        self.audit_action_result_for_request_best_effort("release_all", &result, &request_context);
        result.map(Json)
    }
}

fn foreground_lease_session_id(
    request_context: &RequestContext<RoleServer>,
) -> Result<Option<String>, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)
}

impl SynapseService {
    fn foreground_click_policy_for_request(
        &self,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<ForegroundClickPolicy, ErrorData> {
        let session_id = foreground_lease_session_id(request_context)?;
        let Some(session_id_ref) = session_id.as_deref() else {
            return Ok(ForegroundClickPolicy::allowed(None));
        };
        let Some(hidden_desktop) = self.session_hidden_desktop_readback(session_id_ref)? else {
            return Ok(ForegroundClickPolicy::allowed(session_id));
        };
        Ok(ForegroundClickPolicy::refuse_hidden_desktop(
            session_id_ref.to_owned(),
            hidden_desktop.desktop_names,
        ))
    }
}

fn hidden_desktop_foreground_refusal(
    tool: &'static str,
    hidden_desktop: &super::session_lifecycle::SessionHiddenDesktopReadback,
) -> ErrorData {
    let detail = format!(
        "{tool} cannot use the visible foreground input tier because MCP session {:?} owns hidden desktop(s) {:?}; hidden Win32 desktops are not the active input desktop, so raw SendInput/cursor/foreground activation is refused. Use a background CDP/UIA/PostMessage target route or a separate Windows session/RDP path for raw-input-required apps.",
        hidden_desktop.session_id, hidden_desktop.desktop_names
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail,
        Some(json!({
            "code": error_codes::FOREGROUND_ACTIVATION_REFUSED,
            "reason": "hidden_desktop_foreground_tier_refused",
            "tool": tool,
            "session_id": hidden_desktop.session_id,
            "desktop_names": hidden_desktop.desktop_names,
            "launch_pids": hidden_desktop.launch_pids,
            "resource_count": hidden_desktop.resource_count,
            "foreground_tier_allowed": false,
        })),
    )
}

fn audit_target_claim_denial<T: Serialize>(
    service: &SynapseService,
    tool: &'static str,
    error: ErrorData,
    request_context: &RequestContext<RoleServer>,
) -> Result<Json<T>, ErrorData> {
    let result: Result<T, ErrorData> = Err(error);
    service.audit_action_result_for_request(tool, &result, request_context)?;
    result.map(Json)
}

fn click_claim_target(params: &ActClickParams) -> Option<SessionTarget> {
    click_target_root_hwnd(params)
        .ok()
        .flatten()
        .map(super::target_claims::window_session_target)
}

fn element_claim_target(element_id: &ElementId) -> Option<SessionTarget> {
    element_id
        .parts()
        .ok()
        .map(|parts| super::target_claims::window_session_target(parts.hwnd))
}

fn click_auto_wait_element_id(params: &ActClickParams) -> Option<&ElementId> {
    match &params.target {
        crate::m2::ActClickTarget::Element(element) => Some(&element.element_id),
        crate::m2::ActClickTarget::Point(_) => None,
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ActionabilityAutoWaitRequirement {
    Action,
    Editable,
}

impl ActionabilityAutoWaitRequirement {
    const fn label(self) -> &'static str {
        match self {
            Self::Action => "action_ready",
            Self::Editable => "editable_action_ready",
        }
    }

    const fn includes_editable(self) -> bool {
        matches!(self, Self::Editable)
    }
}

async fn maybe_auto_wait_for_actionability(
    service: &SynapseService,
    tool: &'static str,
    request_context: &RequestContext<RoleServer>,
    enabled: bool,
    element_id: Option<&ElementId>,
    timeout_ms: u32,
    explicit_window_hwnd: Option<i64>,
    explicit_cdp_target_id: Option<&str>,
    requirement: ActionabilityAutoWaitRequirement,
    boundary: OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    if !enabled {
        return Ok(());
    }
    crate::m2::validate_auto_wait_timeout(tool, enabled, timeout_ms)?;
    let element_id = element_id.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} auto_wait=true requires a CDP web element_id target"),
        )
    })?;
    #[cfg(windows)]
    {
        auto_wait_for_actionability(
            service,
            tool,
            request_context,
            element_id,
            timeout_ms,
            explicit_window_hwnd,
            explicit_cdp_target_id,
            requirement,
            boundary,
        )
        .await
    }
    #[cfg(not(windows))]
    {
        let _ = (
            service,
            request_context,
            element_id,
            explicit_window_hwnd,
            explicit_cdp_target_id,
            requirement,
            boundary,
        );
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            format!("{tool} auto_wait actionability is only available on Windows in this build"),
        ))
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn auto_wait_for_actionability(
    service: &SynapseService,
    tool: &'static str,
    request_context: &RequestContext<RoleServer>,
    element_id: &ElementId,
    timeout_ms: u32,
    explicit_window_hwnd: Option<i64>,
    explicit_cdp_target_id: Option<&str>,
    requirement: ActionabilityAutoWaitRequirement,
    boundary: OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let backend_node_id =
        synapse_a11y::cdp_backend_from_element_id(element_id).ok_or_else(|| {
            mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("{tool} auto_wait requires a CDP web element_id, got {element_id}"),
            )
        })?;
    let element_window_hwnd = element_id
        .parts()
        .map_err(|error| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("{tool} auto_wait element id is malformed: {error}"),
            )
        })?
        .hwnd;
    if let Some(window_hwnd) = explicit_window_hwnd
        && window_hwnd != element_window_hwnd
    {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{tool} auto_wait element window {element_window_hwnd:#x} does not match explicit action window {window_hwnd:#x}"
            ),
        ));
    }
    if explicit_cdp_target_id.is_some() && explicit_window_hwnd.is_none() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} auto_wait with explicit cdp_target_id requires window_hwnd"),
        ));
    }
    let element_target_id = synapse_a11y::cdp_target_from_element_id(element_id);
    if let (Some(explicit), Some(encoded)) = (explicit_cdp_target_id, element_target_id.as_deref())
        && !explicit.eq_ignore_ascii_case(encoded)
    {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{tool} auto_wait element target {encoded:?} does not match explicit action target {explicit:?}"
            ),
        ));
    }
    let Some(session_id) = super::context::mcp_session_id_from_request_context(request_context)?
    else {
        return Err(mcp_error(
            error_codes::TARGET_NOT_SET,
            format!(
                "{tool} auto_wait requires an MCP session target; refusing to use the human foreground tab"
            ),
        ));
    };
    let target_id_param = explicit_cdp_target_id.or(element_target_id.as_deref());
    let (window_hwnd, cdp_target_id) = service.resolve_cdp_tab_mutation_target(
        tool,
        &session_id,
        explicit_window_hwnd.or(Some(element_window_hwnd)),
        target_id_param,
    )?;
    let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
        return Err(mcp_error(
            error_codes::A11Y_CDP_UNREACHABLE,
            format!(
                "{tool} auto_wait requires a reachable raw CDP endpoint for window_hwnd {window_hwnd:#x}"
            ),
        ));
    };
    boundary.ensure("immediately_before_auto_wait_scroll_into_view")?;
    let scroll =
        synapse_a11y::cdp_scroll_into_view_node(&endpoint, &cdp_target_id, backend_node_id)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("{tool} auto_wait scrollIntoViewIfNeeded failed: {error}"),
                )
            })?;
    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_millis(u64::from(timeout_ms)))
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{tool} auto_wait_timeout_ms {timeout_ms} overflowed this host clock"),
            )
        })?;
    let mut poll_count = 0_u32;
    loop {
        let actionability =
            synapse_a11y::cdp_actionability(&endpoint, &cdp_target_id, backend_node_id)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{tool} auto_wait actionability readback failed: {error}"),
                    )
                })?;
        poll_count = poll_count.saturating_add(1);
        if actionability_satisfies_requirement(&actionability, requirement) {
            tracing::info!(
                code = "M2_ACTIONABILITY_AUTO_WAIT_READY",
                tool,
                element_id = %element_id,
                hwnd = window_hwnd,
                cdp_target_id = %actionability.target_id,
                backend_node_id,
                requirement = requirement.label(),
                poll_count,
                elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
                window_scroll_changed = scroll.window_scroll_changed,
                container_scroll_changed = scroll.container_scroll_changed,
                "readback=DOM.scrollIntoViewIfNeeded+CDP.actionability outcome=auto_wait_ready"
            );
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(actionability_auto_wait_timeout_error(
                tool,
                element_id,
                backend_node_id,
                &cdp_target_id,
                timeout_ms,
                requirement,
                poll_count,
                Some(&actionability),
                &scroll,
            ));
        }
        let delay = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(50));
        tokio::time::sleep(delay).await;
    }
}

#[cfg(windows)]
fn actionability_satisfies_requirement(
    actionability: &synapse_a11y::CdpActionabilityResult,
    requirement: ActionabilityAutoWaitRequirement,
) -> bool {
    match requirement {
        ActionabilityAutoWaitRequirement::Action => actionability.action_ready,
        ActionabilityAutoWaitRequirement::Editable => actionability.editable_action_ready,
    }
}

fn actionability_failure_is_relevant(
    predicate: &str,
    requirement: ActionabilityAutoWaitRequirement,
) -> bool {
    matches!(
        predicate,
        "attached" | "visible" | "stable" | "enabled" | "receives_events"
    ) || (requirement.includes_editable() && predicate == "editable")
}

#[cfg(windows)]
fn actionability_relevant_failures(
    actionability: &synapse_a11y::CdpActionabilityResult,
    requirement: ActionabilityAutoWaitRequirement,
) -> Vec<Value> {
    actionability
        .failure_reasons
        .iter()
        .filter(|failure| actionability_failure_is_relevant(&failure.predicate, requirement))
        .map(|failure| {
            json!({
                "predicate": failure.predicate,
                "reason": failure.reason,
                "detail": failure.detail,
            })
        })
        .collect()
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn actionability_auto_wait_timeout_error(
    tool: &'static str,
    element_id: &ElementId,
    backend_node_id: i64,
    cdp_target_id: &str,
    timeout_ms: u32,
    requirement: ActionabilityAutoWaitRequirement,
    poll_count: u32,
    last_actionability: Option<&synapse_a11y::CdpActionabilityResult>,
    scroll: &synapse_a11y::CdpScrollIntoViewResult,
) -> ErrorData {
    let unmet_predicates = last_actionability
        .map(|actionability| actionability_relevant_failures(actionability, requirement))
        .unwrap_or_default();
    let predicate_labels = unmet_predicates
        .iter()
        .filter_map(|failure| failure.get("predicate").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let predicate_detail = if predicate_labels.is_empty() {
        "unknown".to_owned()
    } else {
        predicate_labels.join(",")
    };
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} auto_wait timed out after {timeout_ms} ms waiting for {}; unmet predicates: {predicate_detail}",
            requirement.label()
        ),
        Some(json!({
            "code": error_codes::BROWSER_WAIT_TIMEOUT,
            "tool": tool,
            "element_id": element_id.to_string(),
            "backend_node_id": backend_node_id,
            "cdp_target_id": cdp_target_id,
            "timeout_ms": timeout_ms,
            "requirement": requirement.label(),
            "poll_count": poll_count,
            "unmet_predicates": unmet_predicates,
            "last_actionability": last_actionability.and_then(|value| serde_json::to_value(value).ok()),
            "scroll": serde_json::to_value(scroll).ok(),
            "source_of_truth": "DOM.scrollIntoViewIfNeeded + DOM.getBoxModel + DOM.getNodeForLocation + elementFromPoint",
        })),
    )
}

fn scroll_claim_target(params: &ActScrollParams) -> Option<SessionTarget> {
    params
        .target
        .as_ref()
        .and_then(|target| element_claim_target(&target.element_id))
}

const fn click_delta_source_of_truth(target_window_hwnd: Option<i64>) -> &'static str {
    if target_window_hwnd.is_some() {
        "target_window_ui_or_pixels"
    } else {
        "foreground_focused_ui_or_pixels"
    }
}

fn visual_delta_text_integrity(source_of_truth: &str) -> String {
    format!("verify_delta_visual_readback:{source_of_truth}")
}

fn acquire_tool_foreground_input_lease(
    service: &SynapseService,
    tool: &'static str,
    request_context: &RequestContext<RoleServer>,
) -> Result<crate::m2::ForegroundInputLeaseGuard, ErrorData> {
    acquire_tool_foreground_input_lease_with_ttl(
        service,
        tool,
        request_context,
        synapse_action::DEFAULT_LEASE_TTL_MS,
    )
}

fn acquire_tool_foreground_input_lease_with_ttl(
    service: &SynapseService,
    tool: &'static str,
    request_context: &RequestContext<RoleServer>,
    ttl_ms: u64,
) -> Result<crate::m2::ForegroundInputLeaseGuard, ErrorData> {
    // `None` here means the stdio transport by construction — an HTTP request
    // without Mcp-Session-Id fails hard upstream. stdio is single-client, so
    // the stable "stdio" owner (the same idiom the m3 layer uses) gives the
    // lease registry a real owner instead of refusing every foreground tier
    // over stdio.
    let session_id =
        foreground_lease_session_id(request_context)?.unwrap_or_else(|| "stdio".to_owned());
    if let Some(hidden_desktop) = service.session_hidden_desktop_readback(&session_id)? {
        return Err(hidden_desktop_foreground_refusal(tool, &hidden_desktop));
    }
    crate::m2::acquire_foreground_input_lease_with_ttl(tool, Some(&session_id), ttl_ms)
}

fn lease_ttl_for_hold_ms(hold_ms: u32) -> u64 {
    crate::m2::foreground_input_lease_ttl_for_hold_ms(hold_ms)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClickDeltaSignature {
    foreground_hwnd: i64,
    foreground_pid: u32,
    foreground_process: String,
    foreground_title: String,
    foreground_title_sha256: Option<String>,
    focused_element_id: Option<String>,
    focused_role: Option<String>,
    focused_name_sha256: Option<String>,
    focused_value_sha256: Option<String>,
    focused_bbox: Option<Rect>,
    element_count: usize,
    elements_sha256: String,
    cdp_status: Option<String>,
    cdp_endpoint_present: bool,
    web_path: Option<String>,
    cursor_position: Option<Point>,
    pixel: ClickPixelSignature,
    point_pixel: Option<ClickPixelSignature>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CdpKeyboardDeltaSignature {
    target_id: String,
    has_active_element: bool,
    tag_name: String,
    id_sha256: Option<String>,
    name_sha256: Option<String>,
    value_len: usize,
    value_sha256: String,
    selection_start: Option<u32>,
    selection_end: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HwndKeyboardDeltaSignature {
    target: HwndKeyboardTargetState,
    /// Windows clipboard sequence number at capture time. Lets the PostMessage
    /// keyboard verify observe clipboard-mutating chords (Ctrl+C/Ctrl+X) whose
    /// effect is NOT a target text/selection change (#1331).
    clipboard_sequence: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HwndKeyboardExpectedEffect {
    AnyDelta,
    PrintableText {
        text: String,
    },
    SelectAll,
    /// Ctrl+C / Ctrl+X: success is observed as a clipboard sequence-number change.
    Clipboard,
}

#[derive(Clone, Debug)]
struct ForegroundChangePolicy {
    allow: bool,
    expected_process_regex: Option<regex::Regex>,
    expected_process_pattern: Option<String>,
    expected_title_regex: Option<regex::Regex>,
    expected_title_pattern: Option<String>,
}

impl ForegroundChangePolicy {
    fn reject() -> Self {
        Self {
            allow: false,
            expected_process_regex: None,
            expected_process_pattern: None,
            expected_title_regex: None,
            expected_title_pattern: None,
        }
    }

    fn has_expectations(&self) -> bool {
        self.expected_process_regex.is_some() || self.expected_title_regex.is_some()
    }
}

#[derive(Clone, Debug)]
struct ActTypeTextReadback {
    signature: ActTypeTextSignature,
    value: Option<String>,
    browser_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ActTypeForegroundFallbackTarget {
    element_id: String,
    root_hwnd: i64,
    process_name: String,
    role: String,
    automation_id_present: bool,
    bbox: Rect,
    enabled: bool,
    keyboard_focusable: bool,
    patterns: Vec<UiaPattern>,
    name_len: usize,
    value_len: Option<usize>,
}

#[derive(Clone, Debug)]
struct ActTypeBrowserUrlPolicy {
    expected_url_regex: regex::Regex,
    expected_url_pattern: String,
}

#[derive(Clone, Debug)]
struct CdpTargetUrlReadback {
    url: Option<String>,
    target_id: Option<String>,
    source: Option<String>,
}

#[derive(Clone, Debug)]
struct BrowserTargetReadback {
    url: CdpTargetUrlReadback,
    title: Option<String>,
    ready_state: Option<String>,
    active: Option<bool>,
    active_text: Option<CdpActiveTextReadback>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ActTypeTextSignature {
    foreground_hwnd: i64,
    foreground_pid: u32,
    foreground_process: String,
    foreground_title_sha256: Option<String>,
    focused_element_id: Option<String>,
    focused_role: Option<String>,
    focused_name_sha256: Option<String>,
    focused_value_len: Option<usize>,
    focused_value_sha256: Option<String>,
    focused_selected_text_sha256: Option<String>,
    focused_bbox: Option<Rect>,
    readback_source: Option<String>,
    has_text_readback: bool,
    text_readback_attempts: Vec<String>,
    cdp_status: Option<String>,
    cdp_endpoint_present: bool,
    cdp_selected_target_id: Option<String>,
    cdp_active_has_element: Option<bool>,
    cdp_active_is_editable: Option<bool>,
    cdp_active_tag_name: Option<String>,
    cdp_active_id_sha256: Option<String>,
    cdp_active_name_sha256: Option<String>,
    cdp_active_value_len: Option<usize>,
    cdp_active_value_sha256: Option<String>,
    cdp_active_error_code: Option<String>,
    cdp_active_error_detail_sha256: Option<String>,
    ocr_word_count: usize,
    ocr_text_len: Option<usize>,
    ocr_text_sha256: Option<String>,
    web_path: Option<String>,
    browser_url_len: Option<usize>,
    browser_url_sha256: Option<String>,
    browser_cdp_target_id: Option<String>,
    browser_url_readback_source: Option<String>,
    browser_title_sha256: Option<String>,
    browser_ready_state: Option<String>,
    browser_tab_active: Option<bool>,
}

#[derive(Clone, Debug)]
struct CdpActiveTextReadback {
    value: Option<String>,
    target_id: Option<String>,
    has_active_element: Option<bool>,
    is_editable: Option<bool>,
    tag_name: Option<String>,
    id_sha256: Option<String>,
    name_sha256: Option<String>,
    value_len: Option<usize>,
    value_sha256: Option<String>,
    error_code: Option<String>,
    error_detail_sha256: Option<String>,
    attempt: String,
}

#[derive(Clone, Debug)]
struct OcrTextReadback {
    value: Option<String>,
    word_count: usize,
    value_len: Option<usize>,
    value_sha256: Option<String>,
    attempt: String,
}

#[derive(Clone, Debug)]
struct ActTypeFocusedTextCandidate {
    element_id: String,
    role: String,
    name: String,
    selected_text: Option<String>,
    bbox: Rect,
    value: Option<String>,
    patterns: Vec<UiaPattern>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClickPixelSignature {
    status: String,
    region: Rect,
    bitmap_sha256: Option<String>,
    detail: Option<String>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ClickElementFingerprint {
    element_id: String,
    role: String,
    automation_id: Option<String>,
    name_sha256: Option<String>,
    value_sha256: Option<String>,
    bbox: Rect,
    enabled: bool,
    focused: bool,
}

impl SynapseService {
    pub(crate) fn act_clipboard_for_session(
        &self,
        params: ActClipboardParams,
        session_id: &str,
        source_of_truth: &'static str,
    ) -> Result<ActClipboardResponse, ErrorData> {
        let result =
            act_clipboard_session_buffer(params, session_id, self.session_clipboards_ref());
        if let Err(error) = &result {
            tracing::error!(
                code = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("code"))
                    .and_then(|value| value.as_str())
                    .unwrap_or(error_codes::TOOL_INTERNAL_ERROR),
                session_id,
                source_of_truth,
                detail = %error.message,
                data = ?error.data,
                "act_clipboard session buffer operation failed"
            );
        }
        result
    }

    async fn act_click_with_verified_router(
        &self,
        handle: ActionHandle,
        recording: Option<Arc<RecordingBackend>>,
        params: ActClickParams,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        target_window_hwnd: Option<i64>,
        foreground_click_policy: ForegroundClickPolicy,
        started: Instant,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<ActClickResponse, ErrorData> {
        match act_click_with_handle_and_lease(
            handle.clone(),
            recording.clone(),
            params.clone(),
            foreground_click_policy.clone(),
            boundary,
        )
        .await
        {
            Ok(response) => {
                match self
                    .verify_click_response(
                        response,
                        before.clone(),
                        verify_timeout_ms,
                        target_window_hwnd,
                    )
                    .await
                {
                    Ok(response) => Ok(response),
                    Err(error)
                        if can_route_click_element_background_first(
                            &params,
                            recording.as_ref(),
                        ) && should_try_next_click_tier(&error) =>
                    {
                        let tier_attempts = click_tier_attempts_from_error(&error);
                        if should_try_click_postmessage_tier(&tier_attempts) {
                            self.act_click_try_postmessage_then_foreground(
                                handle,
                                recording,
                                params,
                                before,
                                verify_timeout_ms,
                                target_window_hwnd,
                                tier_attempts,
                                foreground_click_policy,
                                boundary,
                            )
                            .await
                        } else if should_try_click_foreground_tier(&tier_attempts) {
                            self.act_click_try_foreground(
                                handle,
                                recording,
                                params,
                                before,
                                verify_timeout_ms,
                                target_window_hwnd,
                                tier_attempts,
                                foreground_click_policy,
                                boundary,
                            )
                            .await
                        } else {
                            Err(error)
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) if click_postdispatch_readback_failed(&error) => {
                match self
                    .reconcile_click_postdispatch_error(
                        &params,
                        &error,
                        before.clone(),
                        verify_timeout_ms,
                        target_window_hwnd,
                        started,
                    )
                    .await
                {
                    Ok(response) => return Ok(response),
                    Err(verify_error) => {
                        tracing::warn!(
                            code = click_error_data_code(&verify_error)
                                .unwrap_or(error_codes::ACTION_POSTCONDITION_FAILED),
                            kind = "act_click",
                            original_error_code = click_error_data_code(&error)
                                .unwrap_or(error_codes::ACTION_TARGET_INVALID),
                            detail = %verify_error.message,
                            "act_click post-dispatch UIA readback error was not reconciled by target-window SoT; considering next eligible delivery tier"
                        );
                    }
                }
                if !can_route_click_element_background_first(&params, recording.as_ref())
                    || !should_try_next_click_tier(&error)
                {
                    return Err(error);
                }
                let tier_attempts = click_tier_attempts_from_error(&error);
                if should_try_click_postmessage_tier(&tier_attempts) {
                    self.act_click_try_postmessage_then_foreground(
                        handle,
                        recording,
                        params,
                        before,
                        verify_timeout_ms,
                        target_window_hwnd,
                        tier_attempts,
                        foreground_click_policy,
                        boundary,
                    )
                    .await
                } else if should_try_click_foreground_tier(&tier_attempts) {
                    self.act_click_try_foreground(
                        handle,
                        recording,
                        params,
                        before,
                        verify_timeout_ms,
                        target_window_hwnd,
                        tier_attempts,
                        foreground_click_policy,
                        boundary,
                    )
                    .await
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn act_click_try_postmessage_then_foreground(
        &self,
        handle: ActionHandle,
        recording: Option<Arc<RecordingBackend>>,
        params: ActClickParams,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        target_window_hwnd: Option<i64>,
        tier_attempts: Vec<ActClickTierAttempt>,
        foreground_click_policy: ForegroundClickPolicy,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<ActClickResponse, ErrorData> {
        match act_click_postmessage_with_params(&params, tier_attempts, boundary).await {
            Ok(response) => {
                match self
                    .verify_click_response(
                        response,
                        before.clone(),
                        verify_timeout_ms,
                        target_window_hwnd,
                    )
                    .await
                {
                    Ok(response) => Ok(response),
                    Err(error) if should_try_next_click_tier(&error) => {
                        let tier_attempts = click_tier_attempts_from_error(&error);
                        if should_try_click_foreground_tier(&tier_attempts) {
                            self.act_click_try_foreground(
                                handle,
                                recording,
                                params,
                                before,
                                verify_timeout_ms,
                                target_window_hwnd,
                                tier_attempts,
                                foreground_click_policy,
                                boundary,
                            )
                            .await
                        } else {
                            Err(error)
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) if should_try_next_click_tier(&error) => {
                let tier_attempts = click_tier_attempts_from_error(&error);
                if should_try_click_foreground_tier(&tier_attempts) {
                    self.act_click_try_foreground(
                        handle,
                        recording,
                        params,
                        before,
                        verify_timeout_ms,
                        target_window_hwnd,
                        tier_attempts,
                        foreground_click_policy,
                        boundary,
                    )
                    .await
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn act_click_try_foreground(
        &self,
        handle: ActionHandle,
        recording: Option<Arc<RecordingBackend>>,
        mut params: ActClickParams,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        target_window_hwnd: Option<i64>,
        prior_attempts: Vec<ActClickTierAttempt>,
        foreground_click_policy: ForegroundClickPolicy,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<ActClickResponse, ErrorData> {
        params.use_invoke_pattern = false;
        match act_click_with_handle_and_lease(
            handle,
            recording,
            params,
            foreground_click_policy,
            boundary,
        )
        .await
        {
            Ok(mut response) => {
                let current_attempts = std::mem::take(&mut response.tier_attempts);
                response.tier_attempts =
                    merge_click_tier_attempts(prior_attempts, current_attempts);
                self.verify_click_response(response, before, verify_timeout_ms, target_window_hwnd)
                    .await
            }
            Err(error) => {
                let mut tier_attempts = prior_attempts;
                tier_attempts.extend(click_tier_attempts_from_error(&error));
                Err(attach_click_tier_attempts(error, tier_attempts))
            }
        }
    }

    async fn verify_click_response(
        &self,
        mut response: ActClickResponse,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        target_window_hwnd: Option<i64>,
    ) -> Result<ActClickResponse, ErrorData> {
        match self
            .verify_click_delta(before, verify_timeout_ms, target_window_hwnd)
            .await
        {
            Ok(postcondition) => {
                response.postcondition = postcondition;
                Ok(response)
            }
            Err(error) => {
                let error_code = click_error_data_code(&error)
                    .unwrap_or(error_codes::ACTION_NO_OBSERVED_DELTA)
                    .to_owned();
                // #1360: a DELIVERED click that closed its own target window
                // (a dialog Open/OK/Cancel button) makes the post-delivery SoT
                // readback fail with TARGET_WINDOW_NOT_FOUND. We are in the
                // Ok(response) branch, so delivery already succeeded and the
                // element + window were live at invoke time — the window
                // vanishing is the click's INTENDED effect, not a delivery
                // failure. Treat target-window-gone-after-delivery as success
                // (verified via window disappearance) instead of a
                // false-negative refusal. Any other verify failure still
                // propagates.
                if error_code == error_codes::TARGET_WINDOW_NOT_FOUND {
                    tracing::info!(
                        code = "M2_ACT_CLICK_TARGET_WINDOW_CLOSED_AFTER_DELIVERY",
                        kind = "act_click",
                        detail = %error.message,
                        "act_click delivered; target window closed afterward (dialog dismissed) — verified via window disappearance"
                    );
                    response.postcondition =
                        postcondition_target_window_closed("act_click", error.message.to_string());
                    return Ok(response);
                }
                let mut tier_attempts = response.tier_attempts.clone();
                tier_attempts.push(click_tier_failed(
                    response.backend_tier_used.clone(),
                    CLICK_REASON_NO_OBSERVED_DELTA,
                    &error_code,
                    response.required_foreground,
                    error.message.to_string(),
                ));
                Err(attach_click_tier_attempts(error, tier_attempts))
            }
        }
    }

    async fn reconcile_click_postdispatch_error(
        &self,
        params: &ActClickParams,
        error: &ErrorData,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        target_window_hwnd: Option<i64>,
        started: Instant,
    ) -> Result<ActClickResponse, ErrorData> {
        let postcondition = self
            .verify_click_delta(before, verify_timeout_ms, target_window_hwnd)
            .await?;
        let mut tier_attempts = click_tier_attempts_from_error(error);
        let tier = tier_attempts
            .first()
            .map(|attempt| attempt.tier.clone())
            .unwrap_or_else(|| "uia".to_owned());
        tier_attempts.push(click_tier_delivered(
            tier.clone(),
            tier_attempts
                .iter()
                .any(|attempt| attempt.required_foreground),
            format!(
                "delivery reported a post-dispatch UIA readback failure, but verify_delta separately observed target-window SoT mutation; original_error={}",
                error.message
            ),
        ));
        let timing = synapse_action::cached_double_click_timing();
        tracing::info!(
            code = "M2_ACT_CLICK_POSTDISPATCH_READBACK_RECONCILED",
            kind = "act_click",
            backend_tier_used = %tier,
            error_message = %error.message,
            postcondition_status = %postcondition.status,
            "readback=target_window_delta tool=act_click outcome=reconciled_postdispatch_error"
        );
        Ok(ActClickResponse {
            ok: true,
            used_invoke_pattern: tier == "uia",
            backend_used: tier.clone(),
            backend_tier_used: tier,
            required_foreground: tier_attempts
                .iter()
                .any(|attempt| attempt.required_foreground),
            tier_attempts,
            postcondition,
            press_hold_ms: params.hold_ms,
            double_click_window_ms: timing.window_ms,
            inter_click_delay_ms: timing.inter_click_delay_ms,
            elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        })
    }

    async fn capture_click_delta_signature(
        &self,
        max_elements: usize,
        target_window_hwnd: Option<i64>,
    ) -> Result<ClickDeltaSignature, ErrorData> {
        self.capture_action_delta_signature(max_elements, None, false, target_window_hwnd)
            .await
    }

    async fn capture_act_type_text_signature(
        &self,
        max_elements: usize,
        require_focused_text_value: bool,
        require_browser_url: bool,
        session_id: Option<&str>,
    ) -> Result<ActTypeTextReadback, ErrorData> {
        let mut input = {
            let state = self.m1_state()?;
            crate::m1::current_input(&state, 6)?
        };
        crate::m1::enrich_input_with_cdp(&mut input, 6, max_elements).await;
        crate::m1::enrich_input_with_browser_ocr(&mut input, max_elements);
        let bridge_target = self
            .chrome_bridge_session_target_readback(
                session_id,
                require_browser_url || require_focused_text_value,
            )
            .await?;
        let browser_url = match bridge_target.as_ref() {
            Some(readback) => readback.url.clone(),
            None => Self::cdp_selected_target_url(&input, require_browser_url).await?,
        };

        let focused = focused_text_candidate(input.focused.as_ref(), &input.elements);
        let (uia_value, uia_readback_source) = focused_text_readback(focused.as_ref());
        let cdp_readback = match bridge_target
            .as_ref()
            .and_then(|readback| readback.active_text.clone())
        {
            Some(readback) => readback,
            None => cdp_active_text_readback(&input).await,
        };
        let ocr_readback = ocr_focused_rect_text_readback(focused.as_ref(), &input.elements);
        let mut text_readback_attempts = Vec::new();
        text_readback_attempts.push(match uia_readback_source {
            Some(source) => format!("{source}:available"),
            None => "uia_focused_value:unavailable".to_owned(),
        });
        text_readback_attempts.push(cdp_readback.attempt.clone());
        text_readback_attempts.push(ocr_readback.attempt.clone());
        let (value, readback_source) = choose_act_type_text_readback(
            focused.as_ref(),
            uia_value,
            uia_readback_source,
            &cdp_readback,
            &ocr_readback,
        );
        let signature = ActTypeTextSignature {
            foreground_hwnd: input.foreground.hwnd,
            foreground_pid: input.foreground.pid,
            foreground_process: input.foreground.process_name,
            foreground_title_sha256: non_empty_sha256(&input.foreground.window_title),
            focused_element_id: focused.as_ref().map(|item| item.element_id.clone()),
            focused_role: focused.as_ref().map(|item| item.role.clone()),
            focused_name_sha256: focused
                .as_ref()
                .and_then(|item| non_empty_sha256(&item.name)),
            focused_value_len: value.as_ref().map(|value| value.chars().count()),
            focused_value_sha256: value.as_deref().and_then(non_empty_sha256),
            focused_selected_text_sha256: focused
                .as_ref()
                .and_then(|item| item.selected_text.as_deref())
                .and_then(non_empty_sha256),
            focused_bbox: focused.as_ref().map(|item| item.bbox),
            readback_source,
            has_text_readback: value.is_some(),
            text_readback_attempts,
            cdp_status: input.cdp.as_ref().map(|cdp| cdp.status.as_str().to_owned()),
            cdp_endpoint_present: input.cdp.as_ref().is_some_and(|cdp| cdp.endpoint.is_some()),
            cdp_selected_target_id: cdp_readback.target_id.clone().or_else(|| {
                input
                    .cdp
                    .as_ref()
                    .and_then(|cdp| cdp.selected_target_id.clone())
            }),
            cdp_active_has_element: cdp_readback.has_active_element,
            cdp_active_is_editable: cdp_readback.is_editable,
            cdp_active_tag_name: cdp_readback.tag_name,
            cdp_active_id_sha256: cdp_readback.id_sha256,
            cdp_active_name_sha256: cdp_readback.name_sha256,
            cdp_active_value_len: cdp_readback.value_len,
            cdp_active_value_sha256: cdp_readback.value_sha256,
            cdp_active_error_code: cdp_readback.error_code,
            cdp_active_error_detail_sha256: cdp_readback.error_detail_sha256,
            ocr_word_count: ocr_readback.word_count,
            ocr_text_len: ocr_readback.value_len,
            ocr_text_sha256: ocr_readback.value_sha256,
            web_path: input.web_path.map(|path| path.as_str().to_owned()),
            browser_url_len: browser_url.url.as_ref().map(|url| url.chars().count()),
            browser_url_sha256: browser_url.url.as_deref().and_then(non_empty_sha256),
            browser_cdp_target_id: browser_url.target_id.clone(),
            browser_url_readback_source: browser_url.source,
            browser_title_sha256: bridge_target
                .as_ref()
                .and_then(|readback| readback.title.as_deref())
                .and_then(non_empty_sha256),
            browser_ready_state: bridge_target
                .as_ref()
                .and_then(|readback| readback.ready_state.clone()),
            browser_tab_active: bridge_target.as_ref().and_then(|readback| readback.active),
        };
        if require_focused_text_value && value.is_none() {
            let signature_hash = verify_hash_json(&signature)?;
            return Err(act_type_verify_surface_unavailable_error(
                "no UIA Value/Text, CDP active-element, or focused-rectangle OCR text readback was available for act_type verify_delta",
                signature_hash,
                signature,
            ));
        }

        Ok(ActTypeTextReadback {
            signature,
            value,
            browser_url: browser_url.url,
        })
    }

    fn ensure_act_type_foreground_fallback_target(
        &self,
        preflight: &ActionPreflightReadback,
        target: &ActTypeForegroundFallbackTarget,
        recording: Option<&Arc<RecordingBackend>>,
    ) -> Result<(), ErrorData> {
        if recording.is_some() {
            return Err(act_type_foreground_fallback_recording_error(target));
        }
        let expected = preflight.after.as_ref().unwrap_or(&preflight.before);
        if expected.hwnd != target.root_hwnd {
            return Err(act_type_foreground_fallback_target_not_foreground_error(
                expected, target,
            ));
        }
        Ok(())
    }

    async fn click_act_type_foreground_fallback_target(
        &self,
        handle: ActionHandle,
        target: &ActTypeForegroundFallbackTarget,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<(), ErrorData> {
        let point = act_type_target_center_point(target)?;
        let actions = [
            Action::MouseMove {
                to: MouseTarget::Screen { point },
                curve: AimCurve::Instant,
                duration_ms: ACT_TYPE_FOREGROUND_FALLBACK_CLICK_DURATION_MS,
                backend: Backend::Auto,
            },
            Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Press,
                hold_ms: ACT_TYPE_FOREGROUND_FALLBACK_CLICK_HOLD_MS,
                backend: Backend::Auto,
            },
        ];
        for action in actions {
            boundary.ensure("immediately_before_act_type_foreground_fallback_click")?;
            handle
                .execute(action)
                .await
                .map_err(|error| act_type_foreground_fallback_click_error(target, point, &error))?;
        }
        tracing::info!(
            code = "M2_ACT_TYPE_CHROMIUM_FOREGROUND_FALLBACK_CLICKED",
            element_id = %target.element_id,
            root_hwnd = target.root_hwnd,
            screen_x = point.x,
            screen_y = point.y,
            role = %target.role,
            "readback=foreground_click tool=act_type into_element_fallback=chromium_uia_value_pattern_refused"
        );
        Ok(())
    }

    /// Runs one background `act_set_field_text` tier inside the same
    /// no-foreground-steal guard `act_set_value` uses: the OS foreground is
    /// read before and after, and an activation of the target window fails
    /// the call (epic #771).
    async fn act_set_field_text_background_guarded<'params, Run>(
        &self,
        params: &'params crate::m2::ActSetFieldTextParams,
        run: Run,
    ) -> Result<crate::m2::ActSetFieldTextResponse, ErrorData>
    where
        Run: FnOnce(
            &'params crate::m2::ActSetFieldTextParams,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::m2::ActSetFieldTextResponse, ErrorData>,
                    > + Send
                    + 'params,
            >,
        >,
    {
        let element_id = crate::m2::required_element_id(params)?;
        let foreground_guard = act_set_value_target_foreground_guard(element_id)?;
        let foreground_before = self
            .current_audit_foreground()
            .map_err(|error| act_set_value_foreground_read_error("before", "unknown", &error))?;
        let result = run(params).await;
        let action_source_of_truth = background_result_source_of_truth(
            &result,
            |response| response.source_of_truth.as_str(),
            "act_set_field_text.background_tier",
        );
        match self.current_audit_foreground() {
            Ok(foreground_after) => background_result_with_foreground_guard(
                "act_set_field_text",
                &action_source_of_truth,
                foreground_guard,
                &foreground_before,
                &foreground_after,
                result,
            ),
            Err(error) => background_result_with_foreground_read_error(
                result,
                act_set_value_foreground_read_error("after", &action_source_of_truth, &error),
            ),
        }
    }

    /// Leased foreground tier for Chromium UIA editable targets: click the
    /// element, prove focus landed on it, Ctrl+A, type (or Delete for empty
    /// text), then a separate UIA value readback must equal the requested
    /// text. The target window must already be foreground — Synapse never
    /// activates it implicitly (epic #771).
    async fn act_set_field_text_foreground_tier(
        &self,
        params: &crate::m2::ActSetFieldTextParams,
        root_hwnd: i64,
        process_name: &str,
        metadata: synapse_a11y::ElementMetadataReadback,
        preflight: &ActionPreflightReadback,
        boundary: OperatorPanicActionBoundary,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<crate::m2::ActSetFieldTextResponse, ErrorData> {
        let started = Instant::now();
        let element_id = crate::m2::required_element_id(params)?.clone();
        let target = ActTypeForegroundFallbackTarget {
            element_id: element_id.to_string(),
            root_hwnd,
            process_name: process_name.to_owned(),
            role: metadata.role.clone(),
            automation_id_present: metadata.automation_id.is_some(),
            bbox: metadata.bbox,
            enabled: metadata.enabled,
            keyboard_focusable: metadata.keyboard_focusable,
            patterns: metadata.patterns.clone(),
            name_len: metadata.name.chars().count(),
            value_len: metadata.value.as_ref().map(|value| value.chars().count()),
        };
        let (handle, recording, _connection_closed_cancel) =
            self.m2_action_context_for_request(request_context)?;
        if recording.is_some() {
            return Err(set_field_text_foreground_error(
                &target,
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                "foreground_tier_recording_backend_unsupported",
                "act_set_field_text Chromium foreground tier requires the live foreground input tier and cannot run against the recording backend",
            ));
        }
        let expected = preflight.after.as_ref().unwrap_or(&preflight.before);
        if expected.hwnd != target.root_hwnd {
            return Err(set_field_text_foreground_error(
                &target,
                error_codes::ACTION_FOREGROUND_LOST,
                "target_window_not_foreground",
                format!(
                    "act_set_field_text Chromium foreground tier requires target hwnd 0x{:x} to be the current foreground hwnd, but preflight foreground was 0x{:x}; call act_focus_window first — Synapse never activates a window implicitly",
                    target.root_hwnd, expected.hwnd
                ),
            ));
        }

        let before = synapse_a11y::element_value(&element_id).map_err(|error| {
            set_field_text_foreground_error(
                &target,
                error.code(),
                "before_value_read_failed",
                format!(
                    "act_set_field_text before-value UIA readback failed for element {element_id}: {error}"
                ),
            )
        })?;

        let _lease_guard =
            acquire_tool_foreground_input_lease(self, "act_set_field_text", request_context)?;

        // Actionability before the coordinate click (the Playwright `fill`
        // discipline): scroll an off-viewport target into view, re-read its
        // live bbox, and require the click point to land inside the target
        // window — a stale below-the-fold bbox must never steer a foreground
        // click into another window.
        let mut target = target;
        if metadata.patterns.contains(&UiaPattern::ScrollItem) {
            boundary.ensure("immediately_before_set_field_text_scroll_into_view")?;
            synapse_a11y::scroll_element_into_view(&element_id).map_err(|error| {
                set_field_text_foreground_error(
                    &target,
                    error.code(),
                    "scroll_into_view_failed",
                    format!(
                        "act_set_field_text ScrollItemPattern scroll-into-view failed for element {element_id}: {error}"
                    ),
                )
            })?;
            target.bbox = synapse_a11y::element_bounding_rect(&element_id).map_err(
                |error| {
                    set_field_text_foreground_error(
                        &target,
                        error.code(),
                        "post_scroll_bbox_read_failed",
                        format!(
                            "act_set_field_text post-scroll bounding-rect readback failed for element {element_id}: {error}"
                        ),
                    )
                },
            )?;
            tracing::info!(
                code = "M2_ACT_SET_FIELD_TEXT_SCROLLED_INTO_VIEW",
                element_id = %element_id,
                bbox_x = target.bbox.x,
                bbox_y = target.bbox.y,
                bbox_w = target.bbox.w,
                bbox_h = target.bbox.h,
                "readback=scroll_into_view tool=act_set_field_text"
            );
        }
        let point = act_type_target_center_point(&target)?;
        let window_bounds = synapse_a11y::foreground_context(root_hwnd)
            .map_err(|error| {
                set_field_text_foreground_error(
                    &target,
                    error.code(),
                    "window_bounds_read_failed",
                    format!(
                        "act_set_field_text target window bounds readback failed for hwnd 0x{root_hwnd:x}: {error}"
                    ),
                )
            })?
            .window_bounds;
        if !rect_contains_point(window_bounds, point) {
            return Err(set_field_text_foreground_error(
                &target,
                error_codes::ACTION_TARGET_INVALID,
                "target_click_point_outside_window",
                format!(
                    "act_set_field_text click point ({}, {}) is outside the target window bounds ({}, {}, {}x{}); the element is not visible in the viewport and could not be scrolled into it",
                    point.x,
                    point.y,
                    window_bounds.x,
                    window_bounds.y,
                    window_bounds.w,
                    window_bounds.h
                ),
            ));
        }
        let click_actions = [
            Action::MouseMove {
                to: MouseTarget::Screen { point },
                curve: AimCurve::Instant,
                duration_ms: ACT_TYPE_FOREGROUND_FALLBACK_CLICK_DURATION_MS,
                backend: Backend::Auto,
            },
            Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Press,
                hold_ms: ACT_TYPE_FOREGROUND_FALLBACK_CLICK_HOLD_MS,
                backend: Backend::Auto,
            },
        ];
        for action in click_actions {
            boundary.ensure("immediately_before_set_field_text_foreground_click")?;
            handle.execute(action).await.map_err(|error| {
                set_field_text_foreground_error(
                    &target,
                    error.code(),
                    "target_click_failed",
                    format!(
                        "act_set_field_text foreground click at ({}, {}) failed: {error}",
                        point.x, point.y
                    ),
                )
            })?;
        }
        tracing::info!(
            code = "M2_ACT_SET_FIELD_TEXT_FOREGROUND_CLICKED",
            element_id = %target.element_id,
            root_hwnd = target.root_hwnd,
            screen_x = point.x,
            screen_y = point.y,
            role = %target.role,
            "readback=foreground_click tool=act_set_field_text tier=foreground_keys"
        );

        let focus_readback = self
            .capture_act_type_text_signature(160, false, false, None)
            .await?;
        act_type_foreground_fallback_focus_matches_target(&target, &focus_readback.signature)?;

        let select_all = crate::m2::select_all_chord_action(60, Backend::Auto)?;
        boundary.ensure("immediately_before_set_field_text_select_all")?;
        handle.execute(select_all).await.map_err(|error| {
            set_field_text_foreground_error(
                &target,
                error.code(),
                "select_all_failed",
                format!("act_set_field_text Ctrl+A select-all failed: {error}"),
            )
        })?;

        let (method, replace_action) = if params.text.is_empty() {
            (
                crate::m2::METHOD_FOREGROUND_CLEAR,
                crate::m2::delete_key_action(40, Backend::Auto)?,
            )
        } else {
            (
                crate::m2::METHOD_FOREGROUND_REPLACE,
                Action::TypeText {
                    text: params.text.clone(),
                    dynamics: KeystrokeDynamics::Natural {
                        params: KeystrokeNaturalParams::FAST,
                    },
                    backend: Backend::Auto,
                },
            )
        };
        boundary.ensure("immediately_before_set_field_text_replacement_input")?;
        handle.execute(replace_action).await.map_err(|error| {
            set_field_text_foreground_error(
                &target,
                error.code(),
                "replacement_input_failed",
                format!("act_set_field_text foreground replacement input failed: {error}"),
            )
        })?;

        tokio::time::sleep(Duration::from_millis(u64::from(params.verify_timeout_ms))).await;
        let after = synapse_a11y::element_value(&element_id).map_err(|error| {
            set_field_text_foreground_error(
                &target,
                error.code(),
                "after_value_read_failed",
                format!(
                    "act_set_field_text after-value UIA readback failed for element {element_id}: {error}"
                ),
            )
        })?;

        if before.is_password || after.is_password {
            return set_field_text_password_response(params, started, method, &before, &after);
        }
        crate::m2::finish_replace_response(
            params,
            started,
            method,
            crate::m2::TIER_FOREGROUND_KEYS,
            true,
            crate::m2::SOURCE_UIA_VALUE,
            &before.value,
            &after.value,
            json!({
                "element_id": element_id.to_string(),
                "root_hwnd": target.root_hwnd,
                "process_name": target.process_name,
                "role": target.role,
                "click_point": { "x": point.x, "y": point.y },
                "focused_element_id": focus_readback.signature.focused_element_id,
            }),
        )
    }

    async fn capture_action_delta_signature(
        &self,
        max_elements: usize,
        point_region: Option<Point>,
        include_cursor: bool,
        target_window_hwnd: Option<i64>,
    ) -> Result<ClickDeltaSignature, ErrorData> {
        let mut input = {
            let state = self.m1_state()?;
            if let Some(hwnd) = target_window_hwnd {
                crate::m1::observe_input(
                    &state,
                    &crate::m1::ObserveParams {
                        depth: Some(6),
                        max_elements: Some(max_elements),
                        window_hwnd: Some(hwnd),
                        ..crate::m1::ObserveParams::default()
                    },
                    None,
                )?
            } else {
                crate::m1::current_input(&state, 6)?
            }
        };
        crate::m1::enrich_input_with_cdp(&mut input, 6, max_elements).await;
        crate::m1::enrich_input_with_browser_ocr(&mut input, max_elements);

        let focused = input.focused.clone();
        let elements_sha256 = elements_fingerprint_hash(&input.elements)?;
        let foreground_title_sha256 = non_empty_sha256(&input.foreground.window_title);
        let pixel = capture_pixel_signature(input.foreground.window_bounds);
        Ok(ClickDeltaSignature {
            foreground_hwnd: input.foreground.hwnd,
            foreground_pid: input.foreground.pid,
            foreground_process: input.foreground.process_name,
            foreground_title: input.foreground.window_title,
            foreground_title_sha256,
            focused_element_id: focused.as_ref().map(|item| item.element_id.to_string()),
            focused_role: focused.as_ref().map(|item| item.role.clone()),
            focused_name_sha256: focused
                .as_ref()
                .and_then(|item| non_empty_sha256(&item.name)),
            focused_value_sha256: focused
                .as_ref()
                .and_then(|item| item.value.as_deref())
                .and_then(non_empty_sha256),
            focused_bbox: focused.as_ref().map(|item| item.bbox),
            element_count: input.elements.len(),
            elements_sha256,
            cdp_status: input.cdp.as_ref().map(|cdp| cdp.status.as_str().to_owned()),
            cdp_endpoint_present: input.cdp.as_ref().is_some_and(|cdp| cdp.endpoint.is_some()),
            web_path: input.web_path.map(|path| path.as_str().to_owned()),
            cursor_position: include_cursor
                .then(|| synapse_action::backend::software::cursor_position().ok())
                .flatten(),
            pixel,
            point_pixel: point_region.map(|point| capture_pixel_signature(point_delta_rect(point))),
        })
    }

    async fn chrome_bridge_session_target_readback(
        &self,
        session_id: Option<&str>,
        require_readback: bool,
    ) -> Result<Option<BrowserTargetReadback>, ErrorData> {
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let Some(SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        }) = self.session_target(Some(session_id))?
        else {
            return Ok(None);
        };
        if synapse_a11y::endpoint_for_window(window_hwnd).is_some() {
            return Ok(None);
        }
        if !is_chrome_bridge_target_id(&cdp_target_id) {
            if require_readback {
                return Err(act_type_browser_url_readback_error(
                    error_codes::A11Y_CDP_ATTACH_FAILED,
                    format!(
                        "act_type readback requires session target {cdp_target_id:?}, but no raw CDP endpoint or normal Chrome bridge target id is available"
                    ),
                    Some("session_target_without_cdp_endpoint"),
                    None,
                    None,
                ));
            }
            return Ok(None);
        }
        let expected_context = synapse_a11y::foreground_context(window_hwnd).ok();
        let info = match crate::chrome_debugger_bridge::target_info(
            window_hwnd,
            &cdp_target_id,
            None,
            expected_context
                .as_ref()
                .map(|context| context.window_bounds),
            expected_context
                .as_ref()
                .map(|context| context.window_title.as_str()),
        )
        .await
        {
            Ok(info) => info,
            Err(error) => {
                if require_readback {
                    return Err(act_type_browser_url_readback_error(
                        error.code(),
                        format!(
                            "Chrome bridge targetInfo readback failed for act_type session target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                        Some("chrome_tabs_extension"),
                        None,
                        Some(error.detail()),
                    ));
                }
                return Ok(None);
            }
        };
        let source = if info.readback_backend.trim().is_empty() {
            "chrome.tabs.get".to_owned()
        } else {
            info.readback_backend.clone()
        };
        Ok(Some(BrowserTargetReadback {
            url: CdpTargetUrlReadback {
                url: (!info.url.trim().is_empty()).then(|| info.url.clone()),
                target_id: Some(info.target_id.clone()),
                source: Some(source),
            },
            title: (!info.title.trim().is_empty()).then_some(info.title),
            ready_state: (!info.ready_state.trim().is_empty()).then_some(info.ready_state),
            active: Some(info.active),
            active_text: info
                .active_element
                .as_ref()
                .map(|active| chrome_bridge_active_text_readback(&info.target_id, active)),
        }))
    }

    async fn act_type_chrome_bridge_session_target(
        &self,
        session_id: &str,
        params: &ActTypeParams,
        emitted: &str,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<Option<ActTypeResponse>, ErrorData> {
        if params.into_element.is_some() {
            return Ok(None);
        }
        let Some((window_hwnd, cdp_target_id)) =
            self.chrome_bridge_active_session_target(session_id)?
        else {
            return Ok(None);
        };
        if params.expected_browser_url_regex.is_some() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_type expected_browser_url_regex is a navigation postcondition; use cdp_navigate_tab for session-owned Chrome bridge tabs instead of keyboard typing",
            ));
        }
        if params.backend == TypeBackend::Hardware {
            return Err(mcp_error(
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                "act_type backend=hardware cannot target an inactive Chrome bridge tab; use backend=auto/software for background DOM typing",
            ));
        }
        action_from_type_params(params)?;
        let chars_typed = u32::try_from(emitted.chars().count()).map_err(|_error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_type text has more than u32::MAX chars",
            )
        })?;
        let started = Instant::now();
        boundary.ensure("immediately_before_chrome_bridge_type_active_element")?;
        let readback =
            crate::chrome_debugger_bridge::type_active_element(window_hwnd, &cdp_target_id, emitted)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "act_type Chrome bridge active-element typing failed for session target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
        let postcondition =
            act_type_chrome_bridge_type_postcondition(params, emitted, chars_typed, &readback)?;
        tracing::info!(
            code = "M2_ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id = %readback.target_id,
            tab_id = readback.tab_id,
            chars_typed = readback.chars_typed,
            readback_backend = %readback.readback_backend,
            target_candidate_count = readback.target_candidate_count,
            target_selection_reason = %readback.target_selection_reason,
            "readback=chrome_bridge_active_element tool=act_type method=chrome.scripting.executeScript"
        );
        Ok(Some(ActTypeResponse {
            ok: true,
            chars_typed,
            elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
            backend_tier_used: ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_TIER.to_owned(),
            required_foreground: false,
            target_text_integrity: ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_TEXT_INTEGRITY.to_owned(),
            target_readback_required: !params.verify_delta,
            minimum_linear_ms_per_char: 20,
            postcondition,
        }))
    }

    fn chrome_bridge_active_session_target(
        &self,
        session_id: &str,
    ) -> Result<Option<(i64, String)>, ErrorData> {
        let target = self.session_target(Some(session_id))?;
        let Some((window_hwnd, cdp_target_id)) =
            chrome_bridge_session_target_parts(target.as_ref())
        else {
            return Ok(None);
        };
        Ok(Some((window_hwnd, cdp_target_id.to_owned())))
    }

    async fn cdp_selected_target_url(
        input: &ObservationInput,
        require_browser_url: bool,
    ) -> Result<CdpTargetUrlReadback, ErrorData> {
        let Some(cdp) = input.cdp.as_ref() else {
            if require_browser_url {
                return Err(act_type_browser_url_readback_error(
                    error_codes::A11Y_CDP_UNREACHABLE,
                    "act_type expected_browser_url_regex requires a Chromium CDP readback, but the foreground is not a CDP-observable browser",
                    None,
                    None,
                    None,
                ));
            }
            return Ok(CdpTargetUrlReadback {
                url: None,
                target_id: None,
                source: None,
            });
        };
        let Some(endpoint) = cdp.endpoint.as_deref() else {
            if require_browser_url {
                return Err(act_type_browser_url_readback_error(
                    cdp.reason_code
                        .as_deref()
                        .unwrap_or(error_codes::A11Y_CDP_UNREACHABLE),
                    "act_type expected_browser_url_regex requires a reachable Chromium CDP endpoint",
                    Some(cdp.status.as_str()),
                    None,
                    cdp.detail.as_deref(),
                ));
            }
            return Ok(CdpTargetUrlReadback {
                url: None,
                target_id: cdp.selected_target_id.clone(),
                source: Some(cdp.status.as_str().to_owned()),
            });
        };
        let Some(target_id) = cdp.selected_target_id.as_deref() else {
            if require_browser_url {
                return Err(act_type_browser_url_readback_error(
                    error_codes::A11Y_CDP_ATTACH_FAILED,
                    "act_type expected_browser_url_regex requires a selected CDP target id from the observation readback",
                    Some(cdp.status.as_str()),
                    Some(endpoint),
                    cdp.detail.as_deref(),
                ));
            }
            return Ok(CdpTargetUrlReadback {
                url: None,
                target_id: None,
                source: Some("cdp_without_selected_target".to_owned()),
            });
        };
        let targets = synapse_a11y::cdp_list_targets(endpoint)
            .await
            .map_err(|error| {
                act_type_browser_url_readback_error(
                    error.code(),
                    format!("Target.getTargets readback failed for act_type browser URL verification: {error}"),
                    Some(cdp.status.as_str()),
                    Some(endpoint),
                    cdp.detail.as_deref(),
                )
            })?;
        let Some(target) = targets
            .iter()
            .find(|target| target.target_id.eq_ignore_ascii_case(target_id))
        else {
            return Err(act_type_browser_url_readback_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "Target.getTargets readback did not contain selected target id {target_id:?} for act_type browser URL verification"
                ),
                Some(cdp.status.as_str()),
                Some(endpoint),
                cdp.detail.as_deref(),
            ));
        };
        if target.url.trim().is_empty() && require_browser_url {
            return Err(act_type_browser_url_readback_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "Target.getTargets readback for selected target id {target_id:?} contained an empty URL"
                ),
                Some(cdp.status.as_str()),
                Some(endpoint),
                cdp.detail.as_deref(),
            ));
        }
        Ok(CdpTargetUrlReadback {
            url: (!target.url.trim().is_empty()).then(|| target.url.clone()),
            target_id: Some(target.target_id.clone()),
            source: Some("Target.getTargets".to_owned()),
        })
    }

    async fn verify_click_delta(
        &self,
        before: ClickDeltaSignature,
        timeout_ms: u32,
        target_window_hwnd: Option<i64>,
    ) -> Result<ActClickPostcondition, ErrorData> {
        tokio::time::sleep(Duration::from_millis(u64::from(timeout_ms))).await;
        let after = self
            .capture_action_delta_signature(160, None, false, target_window_hwnd)
            .await?;
        let source_of_truth = click_delta_source_of_truth(target_window_hwnd);
        let before_hash = signature_hash(&before)?;
        let after_hash = signature_hash(&after)?;
        if foreground_identity_changed(&before, &after) {
            return Err(foreground_lost_delta_error(
                "act_click",
                source_of_truth,
                timeout_ms,
                &before_hash,
                &after_hash,
                &before,
                &after,
            ));
        }
        if before == after {
            return Err(source_no_observed_delta_error(
                "act_click",
                source_of_truth,
                timeout_ms,
                before_hash,
                after_hash,
                json!({
                    "before": before,
                    "after": after,
                }),
            ));
        }
        Ok(postcondition_observed_delta(
            "act_click",
            source_of_truth,
            before_hash,
            after_hash,
            "observed a changed focused/UI/pixel signature after delivery",
        ))
    }

    async fn verify_act_type_response(
        &self,
        response: ActTypeResponse,
        before: ActTypeTextReadback,
        verify_timeout_ms: u32,
        emitted: &str,
        browser_url_policy: Option<&ActTypeBrowserUrlPolicy>,
        session_id: Option<&str>,
    ) -> Result<ActTypeResponse, ErrorData> {
        let started = Instant::now();
        let timeout = Duration::from_millis(u64::from(verify_timeout_ms));
        let poll_interval = Duration::from_millis(ACT_TYPE_VERIFY_POLL_INTERVAL_MS);
        let mut last_error: Option<ErrorData> = None;

        loop {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                break;
            }
            let Some(remaining) = timeout.checked_sub(elapsed) else {
                break;
            };
            tokio::time::sleep(std::cmp::min(poll_interval, remaining)).await;

            let after = self
                .capture_act_type_text_signature(
                    160,
                    false,
                    browser_url_policy.is_some(),
                    session_id,
                )
                .await?;
            let before_hash = verify_hash_json(&before.signature)?;
            let after_hash = verify_hash_json(&after.signature)?;
            let result = if let Some(policy) = browser_url_policy {
                verify_act_type_browser_url_response(
                    response.clone(),
                    before.clone(),
                    after,
                    before_hash,
                    after_hash,
                    verify_timeout_ms,
                    policy,
                )
            } else {
                let terminal_failure = act_type_text_terminal_failure(&before, &after);
                let result = verify_act_type_text_response(
                    response.clone(),
                    before.clone(),
                    after,
                    before_hash,
                    after_hash,
                    verify_timeout_ms,
                    emitted,
                );
                if terminal_failure && result.is_err() {
                    return result;
                }
                result
            };

            match result {
                Ok(response) => return Ok(response),
                Err(error) => {
                    last_error = Some(error);
                    if started.elapsed() >= timeout {
                        break;
                    }
                }
            }
        }

        let after = match self
            .capture_act_type_text_signature(160, false, browser_url_policy.is_some(), session_id)
            .await
        {
            Ok(after) => after,
            Err(error) => return Err(last_error.unwrap_or(error)),
        };
        let before_hash = verify_hash_json(&before.signature)?;
        let after_hash = verify_hash_json(&after.signature)?;
        if let Some(policy) = browser_url_policy {
            return verify_act_type_browser_url_response(
                response,
                before,
                after,
                before_hash,
                after_hash,
                verify_timeout_ms,
                policy,
            );
        }
        verify_act_type_text_response(
            response,
            before,
            after,
            before_hash,
            after_hash,
            verify_timeout_ms,
            emitted,
        )
    }

    async fn verify_act_type_visual_delta_response(
        &self,
        mut response: ActTypeResponse,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        target_window_hwnd: Option<i64>,
        semantic_error: &ErrorData,
    ) -> Result<ActTypeResponse, ErrorData> {
        let after = self
            .capture_action_delta_signature(160, None, false, target_window_hwnd)
            .await?;
        let source_of_truth = click_delta_source_of_truth(target_window_hwnd);
        let postcondition = verify_captured_action_delta(
            "act_type",
            source_of_truth,
            verify_timeout_ms,
            before,
            after,
            None,
            ForegroundChangePolicy::reject(),
        )?;
        tracing::info!(
            code = "M2_ACT_TYPE_VISUAL_DELTA_VERIFIED",
            tool = "act_type",
            target_window_hwnd,
            source_of_truth,
            semantic_error_code = click_error_data_code(semantic_error)
                .unwrap_or(error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE),
            semantic_error_detail = %semantic_error.message,
            "act_type semantic text readback could not prove the delivered input; visual target-window SoT changed after delivery"
        );
        response.postcondition = postcondition;
        response.target_readback_required = false;
        response.target_text_integrity = visual_delta_text_integrity(source_of_truth);
        Ok(response)
    }

    async fn verify_act_press_response(
        &self,
        mut response: ActPressResponse,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        foreground_change_policy: ForegroundChangePolicy,
    ) -> Result<ActPressResponse, ErrorData> {
        response.postcondition = self
            .verify_action_delta(
                "act_press",
                "foreground_focused_ui_or_pixels",
                before,
                verify_timeout_ms,
                None,
                foreground_change_policy,
            )
            .await?;
        Ok(response)
    }

    async fn try_act_press_background_target(
        &self,
        params: ActPressParams,
        recording_active: bool,
        request_context: &RequestContext<RoleServer>,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<Option<ActPressResponse>, ErrorData> {
        if !press_background_target_candidate(&params, recording_active) {
            return Ok(None);
        }
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
        let Some(target) = self.action_session_target_override(
            params.window_hwnd,
            params.cdp_target_id.as_deref(),
            session_id.as_deref(),
        )?
        else {
            return Ok(None);
        };
        match target {
            SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id,
            } => self
                .act_press_cdp_background_target(window_hwnd, cdp_target_id, params, boundary)
                .await
                .map(Some),
            SessionTarget::Window { hwnd } => self
                .act_press_postmessage_background_target(hwnd, params, boundary)
                .await
                .map(Some),
        }
    }

    async fn try_act_keymap_background_target(
        &self,
        resolved: &ResolvedKeymapPress,
        recording_active: bool,
        request_context: &RequestContext<RoleServer>,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<Option<ActKeymapResponse>, ErrorData> {
        self.try_act_press_background_target(
            resolved.press.clone(),
            recording_active,
            request_context,
            boundary,
        )
        .await
        .map(|response| response.map(|response| act_keymap_response_from_press(resolved, response)))
    }

    async fn try_act_stroke_cdp_background_target(
        &self,
        params: &ActStrokeParams,
        plan: &ActStrokePlan,
        recording_active: bool,
        request_context: &RequestContext<RoleServer>,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<Option<ActStrokeResponse>, ErrorData> {
        if recording_active || params.requests_hardware_backend() {
            return Ok(None);
        }
        let Some(session_id) =
            super::context::mcp_session_id_from_request_context(request_context)?
        else {
            return Ok(None);
        };
        let Some(target) = self.session_target(Some(&session_id))? else {
            return Ok(None);
        };
        let SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } = target
        else {
            return Ok(None);
        };
        if plan.is_cdp_element_aim() {
            return Ok(None);
        }
        if !plan.can_try_cdp_target_stroke() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_stroke active CDP targets require an explicit path for background mouse strokes; refusing to fall back to the real cursor for a browser target",
            ));
        }
        let endpoint = synapse_a11y::endpoint_for_window(window_hwnd).ok_or_else(|| {
            mcp_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "act_stroke background CDP target requires a reachable raw CDP endpoint for window_hwnd {window_hwnd:#x}; the normal chrome.tabs bridge cannot dispatch Input.dispatchMouseEvent"
                ),
            )
        })?;
        act_stroke_cdp_target(
            &endpoint,
            &cdp_target_id,
            params.clone(),
            plan.clone(),
            boundary,
        )
        .await
        .map(Some)
    }

    async fn act_press_cdp_background_target(
        &self,
        window_hwnd: i64,
        cdp_target_id: String,
        params: ActPressParams,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<ActPressResponse, ErrorData> {
        let endpoint = synapse_a11y::endpoint_for_window(window_hwnd).ok_or_else(|| {
            mcp_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "act_press background CDP target requires a reachable CDP endpoint for window_hwnd {window_hwnd:#x}"
                ),
            )
        })?;
        let before = if params.verify_delta {
            Some(
                self.capture_cdp_keyboard_delta_signature(&endpoint, &cdp_target_id)
                    .await?,
            )
        } else {
            None
        };
        let verify_timeout_ms = params.verify_timeout_ms;
        let mut response =
            act_press_cdp_target(&endpoint, &cdp_target_id, params, boundary).await?;
        if let Some(before) = before {
            tokio::time::sleep(Duration::from_millis(u64::from(verify_timeout_ms))).await;
            let after = self
                .capture_cdp_keyboard_delta_signature(&endpoint, &cdp_target_id)
                .await?;
            response.postcondition = verify_keyboard_delta_signature(
                "act_press",
                "cdp_active_element_value_or_selection",
                verify_timeout_ms,
                before,
                after,
                "observed CDP target active-element value/selection change after Input.dispatchKeyEvent delivery",
            )?;
        }
        Ok(response)
    }

    async fn act_press_postmessage_background_target(
        &self,
        root_hwnd: i64,
        params: ActPressParams,
        boundary: OperatorPanicActionBoundary,
    ) -> Result<ActPressResponse, ErrorData> {
        let expected_effect = hwnd_keyboard_expected_effect(&params)?;
        let before = self.capture_hwnd_keyboard_delta_signature(root_hwnd)?;
        let before_visual = if params.verify_delta {
            Some(
                self.capture_click_delta_signature(160, Some(root_hwnd))
                    .await?,
            )
        } else {
            None
        };
        let verify_timeout_ms = params.verify_timeout_ms;
        let mut response = act_press_postmessage_target(root_hwnd, params, boundary).await?;
        tokio::time::sleep(Duration::from_millis(u64::from(verify_timeout_ms))).await;
        let after = self.capture_hwnd_keyboard_delta_signature(root_hwnd)?;
        response.postcondition = match verify_hwnd_keyboard_delta_signature(
            "act_press",
            "target_hwnd_text_or_selection",
            verify_timeout_ms,
            before,
            after,
            expected_effect,
            "observed target HWND text/selection change after PostMessage keyboard delivery",
        ) {
            Ok(postcondition) => postcondition,
            Err(error)
                if click_error_data_code(&error) == Some(error_codes::ACTION_NO_OBSERVED_DELTA) =>
            {
                let Some(before_visual) = before_visual else {
                    return Err(error);
                };
                tracing::info!(
                    code = "M2_ACT_PRESS_VISUAL_DELTA_AFTER_HWND_NO_TEXT_DELTA",
                    tool = "act_press",
                    root_hwnd,
                    semantic_error_code = error_codes::ACTION_NO_OBSERVED_DELTA,
                    semantic_error_detail = %error.message,
                    "act_press target HWND text/selection readback showed no delta; checking target-window UI/pixel SoT"
                );
                let after_visual = self
                    .capture_click_delta_signature(160, Some(root_hwnd))
                    .await?;
                verify_captured_action_delta(
                    "act_press",
                    "target_window_ui_or_pixels",
                    verify_timeout_ms,
                    before_visual,
                    after_visual,
                    None,
                    ForegroundChangePolicy::reject(),
                )?
            }
            Err(error) => return Err(error),
        };
        Ok(response)
    }

    async fn capture_cdp_keyboard_delta_signature(
        &self,
        endpoint: &str,
        cdp_target_id: &str,
    ) -> Result<CdpKeyboardDeltaSignature, ErrorData> {
        let state = synapse_a11y::cdp_active_element_state(endpoint, cdp_target_id)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "act_press CDP active-element Source-of-Truth readback failed for target {cdp_target_id:?}: {error}"
                    ),
                )
            })?;
        Ok(cdp_keyboard_delta_signature(state))
    }

    fn capture_hwnd_keyboard_delta_signature(
        &self,
        root_hwnd: i64,
    ) -> Result<HwndKeyboardDeltaSignature, ErrorData> {
        Ok(HwndKeyboardDeltaSignature {
            target: hwnd_keyboard_target_state(root_hwnd)?,
            clipboard_sequence: crate::m2::press::clipboard_sequence_number(),
        })
    }

    async fn verify_act_scroll_response(
        &self,
        mut response: ActScrollResponse,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
        point_region: Option<Point>,
    ) -> Result<ActScrollResponse, ErrorData> {
        response.postcondition = self
            .verify_action_delta(
                "act_scroll",
                if point_region.is_some() {
                    "target_point_pixels"
                } else {
                    "foreground_focused_ui_or_pixels"
                },
                before,
                verify_timeout_ms,
                point_region,
                ForegroundChangePolicy::reject(),
            )
            .await?;
        Ok(response)
    }

    async fn verify_act_stroke_response(
        &self,
        mut response: ActStrokeResponse,
        before: ClickDeltaSignature,
        verify_timeout_ms: u32,
    ) -> Result<ActStrokeResponse, ErrorData> {
        response.postcondition = self
            .verify_action_delta(
                "act_stroke",
                "cursor_foreground_ui_or_pixels",
                before,
                verify_timeout_ms,
                None,
                ForegroundChangePolicy::reject(),
            )
            .await?;
        Ok(response)
    }

    async fn verify_action_delta(
        &self,
        tool: &str,
        source_of_truth: &str,
        before: ClickDeltaSignature,
        timeout_ms: u32,
        point_region: Option<Point>,
        foreground_change_policy: ForegroundChangePolicy,
    ) -> Result<ActPostcondition, ErrorData> {
        tokio::time::sleep(Duration::from_millis(u64::from(timeout_ms))).await;
        let after = self
            .capture_action_delta_signature(
                160,
                point_region,
                source_of_truth.contains("cursor"),
                None,
            )
            .await?;
        verify_captured_action_delta(
            tool,
            source_of_truth,
            timeout_ms,
            before,
            after,
            point_region,
            foreground_change_policy,
        )
    }

    async fn verify_act_pad_response(
        &self,
        mut response: ActPadResponse,
        before: ActionStateSnapshot,
        snapshot_handle: ActionEmitterSnapshotHandle,
        verify_timeout_ms: u32,
    ) -> Result<ActPadResponse, ErrorData> {
        tokio::time::sleep(Duration::from_millis(u64::from(verify_timeout_ms))).await;
        let after = snapshot_handle
            .snapshot()
            .await
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let before_hash = verify_hash_json(&before.pad_state)?;
        let after_hash = verify_hash_json(&after.pad_state)?;
        if before.pad_state == after.pad_state {
            return Err(source_no_observed_delta_error(
                "act_pad",
                "action_emitter.pad_state",
                verify_timeout_ms,
                before_hash,
                after_hash,
                json!({
                    "before_pad_state": before.pad_state,
                    "after_pad_state": after.pad_state,
                }),
            ));
        }
        response.postcondition = postcondition_observed_delta(
            "act_pad",
            "action_emitter.pad_state",
            before_hash,
            after_hash,
            "observed action emitter pad_state change after delivery",
        );
        Ok(response)
    }
}

fn verify_captured_action_delta(
    tool: &str,
    source_of_truth: &str,
    timeout_ms: u32,
    before: ClickDeltaSignature,
    after: ClickDeltaSignature,
    point_region: Option<Point>,
    foreground_change_policy: ForegroundChangePolicy,
) -> Result<ActPostcondition, ErrorData> {
    if point_region.is_some() {
        let before_point = before.point_pixel;
        let after_point = after.point_pixel;
        let before_hash = verify_hash_json(&before_point)?;
        let after_hash = verify_hash_json(&after_point)?;
        if before_point == after_point {
            return Err(source_no_observed_delta_error(
                tool,
                source_of_truth,
                timeout_ms,
                before_hash,
                after_hash,
                json!({
                    "point": point_region,
                    "before_point_pixel": before_point,
                    "after_point_pixel": after_point,
                }),
            ));
        }
        return Ok(postcondition_observed_delta(
            tool,
            source_of_truth,
            before_hash,
            after_hash,
            "observed a target point pixel signature change after delivery",
        ));
    }

    let before_hash = signature_hash(&before)?;
    let after_hash = signature_hash(&after)?;
    if foreground_identity_changed(&before, &after) {
        return verify_foreground_transition(
            tool,
            source_of_truth,
            timeout_ms,
            before_hash,
            after_hash,
            &before,
            &after,
            &foreground_change_policy,
        );
    }
    if foreground_change_policy.has_expectations() {
        return Err(postcondition_failed_error(
            tool,
            source_of_truth,
            "expected foreground transition did not occur",
            before_hash,
            after_hash,
            json!({
                "before": before,
                "after": after,
                "foreground_change_policy": foreground_change_policy_readback(&foreground_change_policy),
            }),
        ));
    }
    if before == after {
        return Err(source_no_observed_delta_error(
            tool,
            source_of_truth,
            timeout_ms,
            before_hash,
            after_hash,
            json!({
                "before": before,
                "after": after,
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        tool,
        source_of_truth,
        before_hash,
        after_hash,
        "observed a Source-of-Truth signature change after delivery",
    ))
}

fn verify_keyboard_delta_signature<T>(
    tool: &str,
    source_of_truth: &str,
    timeout_ms: u32,
    before: T,
    after: T,
    success_detail: &str,
) -> Result<ActPostcondition, ErrorData>
where
    T: Serialize + PartialEq,
{
    let before_hash = hash_json(&before)?;
    let after_hash = hash_json(&after)?;
    if before == after {
        return Err(source_no_observed_delta_error(
            tool,
            source_of_truth,
            timeout_ms,
            before_hash,
            after_hash,
            json!({
                "before": before,
                "after": after,
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        tool,
        source_of_truth,
        before_hash,
        after_hash,
        success_detail,
    ))
}

fn verify_hwnd_keyboard_delta_signature(
    tool: &str,
    source_of_truth: &str,
    timeout_ms: u32,
    before: HwndKeyboardDeltaSignature,
    after: HwndKeyboardDeltaSignature,
    expected_effect: HwndKeyboardExpectedEffect,
    success_detail: &str,
) -> Result<ActPostcondition, ErrorData> {
    let before_hash = hash_json(&before)?;
    let after_hash = hash_json(&after)?;
    if before == after {
        return Err(source_no_observed_delta_error(
            tool,
            source_of_truth,
            timeout_ms,
            before_hash,
            after_hash,
            json!({
                "before": before,
                "after": after,
                "expected_effect": hwnd_keyboard_expected_effect_name(&expected_effect),
            }),
        ));
    }
    if let Some(reason) = hwnd_keyboard_effect_mismatch(&before, &after, &expected_effect) {
        return Err(postcondition_failed_error(
            tool,
            source_of_truth,
            reason,
            before_hash,
            after_hash,
            json!({
                "before": before,
                "after": after,
                "expected_effect": hwnd_keyboard_expected_effect_name(&expected_effect),
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        tool,
        source_of_truth,
        before_hash,
        after_hash,
        success_detail,
    ))
}

fn hwnd_keyboard_expected_effect(
    params: &ActPressParams,
) -> Result<HwndKeyboardExpectedEffect, ErrorData> {
    let labels = act_press_normalized_labels(params)?;
    if labels == ["ctrl", "a"] {
        return Ok(HwndKeyboardExpectedEffect::SelectAll);
    }
    // Ctrl+C / Ctrl+X copy/cut to the clipboard without changing target text
    // (copy) — their effect is a clipboard sequence-number change (#1331).
    if hwnd_is_clipboard_chord(&labels) {
        return Ok(HwndKeyboardExpectedEffect::Clipboard);
    }
    let has_command_modifier = labels
        .iter()
        .any(|label| matches!(label.as_str(), "ctrl" | "alt" | "super"));
    if !has_command_modifier && labels.len() == 1 {
        if let Some(text) = hwnd_printable_text_for_label(&labels[0]) {
            return Ok(HwndKeyboardExpectedEffect::PrintableText { text });
        }
    }
    Ok(HwndKeyboardExpectedEffect::AnyDelta)
}

/// True for a Ctrl+C / Ctrl+X chord (copy/cut): exactly ctrl + {c|x}, no other
/// command modifier. These mutate the clipboard, not the target text (#1331).
fn hwnd_is_clipboard_chord(labels: &[String]) -> bool {
    let has_ctrl = labels.iter().any(|label| label == "ctrl");
    if !has_ctrl {
        return false;
    }
    if labels
        .iter()
        .any(|label| matches!(label.as_str(), "alt" | "super" | "shift"))
    {
        return false;
    }
    let letters: Vec<&str> = labels
        .iter()
        .filter(|label| label.as_str() != "ctrl")
        .map(String::as_str)
        .collect();
    letters.len() == 1 && matches!(letters[0], "c" | "x")
}

fn hwnd_printable_text_for_label(label: &str) -> Option<String> {
    if label.len() == 1 && label.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(label.to_owned());
    }
    match label {
        "`" => Some("`".to_owned()),
        "space" => Some(" ".to_owned()),
        _ => None,
    }
}

fn hwnd_keyboard_effect_mismatch(
    before: &HwndKeyboardDeltaSignature,
    after: &HwndKeyboardDeltaSignature,
    expected_effect: &HwndKeyboardExpectedEffect,
) -> Option<&'static str> {
    match expected_effect {
        HwndKeyboardExpectedEffect::AnyDelta => None,
        HwndKeyboardExpectedEffect::Clipboard => {
            if before.clipboard_sequence == after.clipboard_sequence {
                return Some(
                    "clipboard chord (Ctrl+C/Ctrl+X) did not change the clipboard sequence number",
                );
            }
            None
        }
        HwndKeyboardExpectedEffect::SelectAll => {
            if !same_hwnd_keyboard_target(before, after) {
                return Some("target HWND changed while verifying Ctrl+A select-all delivery");
            }
            if before.target.text_len != after.target.text_len
                || before.target.text_sha256 != after.target.text_sha256
            {
                return Some("Ctrl+A select-all changed target text instead of preserving it");
            }
            if !selection_covers_text(&after.target) {
                return Some("Ctrl+A select-all did not select the full target text");
            }
            None
        }
        HwndKeyboardExpectedEffect::PrintableText { text } => {
            if !same_hwnd_keyboard_target(before, after) {
                return Some("target HWND changed while verifying printable key delivery");
            }
            if before.target.text_sha256 == after.target.text_sha256 {
                return Some("printable key did not change target text");
            }
            if selection_covers_text(&before.target) {
                let expected_len = text.chars().count();
                let Ok(expected_len_u32) = u32::try_from(expected_len) else {
                    return Some("printable key expected text length exceeded u32::MAX");
                };
                let expected_sha256 = text_sha256(text);
                if after.target.text_len != Some(expected_len)
                    || after.target.text_sha256.as_deref() != Some(expected_sha256.as_str())
                    || after.target.selection_start != Some(expected_len_u32)
                    || after.target.selection_end != Some(expected_len_u32)
                {
                    return Some(
                        "printable key after full selection did not replace target text with the emitted character",
                    );
                }
            }
            None
        }
    }
}

fn same_hwnd_keyboard_target(
    before: &HwndKeyboardDeltaSignature,
    after: &HwndKeyboardDeltaSignature,
) -> bool {
    before.target.root_hwnd == after.target.root_hwnd
        && before.target.hwnd == after.target.hwnd
        && before.target.class_name == after.target.class_name
}

fn selection_covers_text(target: &HwndKeyboardTargetState) -> bool {
    let Some(text_len) = target.text_len else {
        return false;
    };
    let Ok(text_len) = u32::try_from(text_len) else {
        return false;
    };
    target.selection_start == Some(0) && target.selection_end == Some(text_len)
}

fn hwnd_keyboard_expected_effect_name(
    expected_effect: &HwndKeyboardExpectedEffect,
) -> &'static str {
    match expected_effect {
        HwndKeyboardExpectedEffect::AnyDelta => "any_delta",
        HwndKeyboardExpectedEffect::PrintableText { .. } => "printable_text",
        HwndKeyboardExpectedEffect::SelectAll => "select_all",
        HwndKeyboardExpectedEffect::Clipboard => "clipboard",
    }
}

fn cdp_keyboard_delta_signature(
    state: synapse_a11y::CdpActiveElementState,
) -> CdpKeyboardDeltaSignature {
    CdpKeyboardDeltaSignature {
        target_id: state.target_id,
        has_active_element: state.has_active_element,
        tag_name: state.tag_name,
        id_sha256: non_empty_sha256(&state.id),
        name_sha256: non_empty_sha256(&state.name),
        value_len: state.value.chars().count(),
        value_sha256: text_sha256(&state.value),
        selection_start: state.selection_start,
        selection_end: state.selection_end,
    }
}

fn press_background_target_candidate(params: &ActPressParams, recording_active: bool) -> bool {
    if recording_active {
        return false;
    }
    if !matches!(params.backend, PressBackend::Auto | PressBackend::Software) {
        return false;
    }
    !params.allow_foreground_change
        && params.expected_foreground_process_regex.is_none()
        && params.expected_foreground_title_regex.is_none()
}

fn act_press_foreground_change_policy(
    params: &ActPressParams,
) -> Result<ForegroundChangePolicy, ErrorData> {
    let has_process_expectation = params.expected_foreground_process_regex.is_some();
    let has_title_expectation = params.expected_foreground_title_regex.is_some();
    let has_expectation = has_process_expectation || has_title_expectation;

    if !params.verify_delta && (params.allow_foreground_change || has_expectation) {
        return Err(act_press_policy_params_invalid(
            "verify_delta",
            "act_press foreground-change verification policy requires verify_delta=true",
            "verify_delta_required",
        ));
    }

    if !params.allow_foreground_change && has_expectation {
        return Err(act_press_policy_params_invalid(
            "allow_foreground_change",
            "expected foreground regex fields require allow_foreground_change=true",
            "allow_foreground_change_required",
        ));
    }

    if !params.allow_foreground_change {
        return Ok(ForegroundChangePolicy::reject());
    }

    let expected_process_pattern = params.expected_foreground_process_regex.clone();
    let expected_title_pattern = params.expected_foreground_title_regex.clone();
    let expected_process_regex = compile_act_press_policy_regex(
        "expected_foreground_process_regex",
        expected_process_pattern.as_deref(),
    )?;
    let expected_title_regex = compile_act_press_policy_regex(
        "expected_foreground_title_regex",
        expected_title_pattern.as_deref(),
    )?;

    Ok(ForegroundChangePolicy {
        allow: true,
        expected_process_regex,
        expected_process_pattern,
        expected_title_regex,
        expected_title_pattern,
    })
}

fn act_type_browser_url_policy(
    params: &ActTypeParams,
) -> Result<Option<ActTypeBrowserUrlPolicy>, ErrorData> {
    let Some(pattern) = params.expected_browser_url_regex.as_deref() else {
        return Ok(None);
    };
    if !params.verify_delta {
        return Err(act_type_url_policy_params_invalid(
            "verify_delta",
            "act_type expected_browser_url_regex requires verify_delta=true",
            "verify_delta_required",
        ));
    }
    if params.into_element.is_some() {
        return Err(act_type_url_policy_params_invalid(
            "into_element",
            "act_type expected_browser_url_regex applies only to foreground typing, not into_element routing",
            "foreground_typing_required",
        ));
    }
    if pattern.trim().is_empty() {
        return Err(act_type_url_policy_params_invalid(
            "expected_browser_url_regex",
            "expected_browser_url_regex must not be empty",
            "empty_expected_browser_url_regex",
        ));
    }
    let expected_url_regex = regex::Regex::new(pattern).map_err(|error| {
        act_type_url_policy_params_invalid(
            "expected_browser_url_regex",
            format!("expected_browser_url_regex is not a valid regex: {error}"),
            "invalid_expected_browser_url_regex",
        )
    })?;
    Ok(Some(ActTypeBrowserUrlPolicy {
        expected_url_regex,
        expected_url_pattern: pattern.to_owned(),
    }))
}

fn act_type_should_capture_text_signature(params: &ActTypeParams) -> bool {
    params.verify_delta && params.into_element.is_none()
}

fn act_type_should_capture_visual_signature(
    params: &ActTypeParams,
    target_window_hwnd: Option<i64>,
) -> bool {
    params.verify_delta && params.into_element.is_none() && target_window_hwnd.is_some()
}

fn act_type_visual_delta_target_window(
    params: &ActTypeParams,
    browser_url_policy: Option<&ActTypeBrowserUrlPolicy>,
) -> Result<Option<i64>, ErrorData> {
    let Some(hwnd) = params.verify_target_window_hwnd else {
        return Ok(None);
    };
    let hwnd = crate::m1::validate_hwnd_shape("act_type", "verify_target_window_hwnd", hwnd)?;
    if !params.verify_delta {
        return Err(act_type_visual_delta_params_invalid(
            "verify_delta",
            "verify_target_window_hwnd requires verify_delta=true",
            "verify_delta_required",
        ));
    }
    if params.into_element.is_some() {
        return Err(act_type_visual_delta_params_invalid(
            "into_element",
            "verify_target_window_hwnd applies only to foreground typing, not into_element routing",
            "foreground_typing_required",
        ));
    }
    if browser_url_policy.is_some() {
        return Err(act_type_visual_delta_params_invalid(
            "expected_browser_url_regex",
            "verify_target_window_hwnd cannot replace browser URL verification; remove one postcondition",
            "conflicting_postconditions",
        ));
    }
    Ok(Some(hwnd))
}

fn act_type_requires_foreground_route(
    params: &ActTypeParams,
    fallback_target: Option<&ActTypeForegroundFallbackTarget>,
) -> bool {
    params.into_element.is_none() || fallback_target.is_some()
}

#[cfg(windows)]
fn act_type_chromium_foreground_fallback_target(
    element_id: Option<&ElementId>,
) -> Result<Option<ActTypeForegroundFallbackTarget>, ErrorData> {
    let Some(element_id) = element_id else {
        return Ok(None);
    };
    if synapse_a11y::cdp_backend_from_element_id(element_id).is_some() {
        return Ok(None);
    }
    let root_hwnd = element_id
        .parts()
        .map_err(|err| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("act_type into_element id is malformed: {err}"),
            )
        })?
        .hwnd;
    let context = synapse_a11y::foreground_context(root_hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "act_type into_element foreground fallback target HWND readback failed: {error}"
            ),
        )
    })?;
    if !synapse_a11y::is_chromium_family(&context.process_name) {
        return Ok(None);
    }
    let metadata = synapse_a11y::element_metadata(element_id).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "act_type into_element foreground fallback target metadata readback failed: {error}"
            ),
        )
    })?;
    if !chromium_editable_value_pattern_requires_foreground_fallback(
        &context.process_name,
        &metadata,
    ) {
        return Ok(None);
    }
    Ok(Some(ActTypeForegroundFallbackTarget {
        element_id: element_id.to_string(),
        root_hwnd,
        process_name: context.process_name,
        role: metadata.role,
        automation_id_present: metadata.automation_id.is_some(),
        bbox: metadata.bbox,
        enabled: metadata.enabled,
        keyboard_focusable: metadata.keyboard_focusable,
        patterns: metadata.patterns,
        name_len: metadata.name.chars().count(),
        value_len: metadata.value.as_ref().map(|value| value.chars().count()),
    }))
}

#[cfg(not(windows))]
fn act_type_chromium_foreground_fallback_target(
    _element_id: Option<&ElementId>,
) -> Result<Option<ActTypeForegroundFallbackTarget>, ErrorData> {
    Ok(None)
}

fn chromium_editable_value_pattern_requires_foreground_fallback(
    process_name: &str,
    metadata: &synapse_a11y::ElementMetadataReadback,
) -> bool {
    if !synapse_a11y::is_chromium_family(process_name) || !metadata.enabled {
        return false;
    }
    if !metadata.patterns.contains(&UiaPattern::Value) {
        return false;
    }
    metadata.keyboard_focusable
        && (act_type_editable_role(&metadata.role) || metadata.patterns.contains(&UiaPattern::Text))
}

fn act_type_editable_role(role: &str) -> bool {
    let role = role.to_ascii_lowercase();
    role.contains("edit") || role.contains("document") || role.contains("text")
}

fn act_type_url_policy_params_invalid(
    field: &'static str,
    detail: impl Into<String>,
    reason: &'static str,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code = error_codes::TOOL_PARAMS_INVALID,
        tool = "act_type",
        field,
        reason,
        detail = %detail,
        "act_type browser URL policy parameters invalid"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": "act_type",
            "field": field,
            "reason": reason,
            "detail": detail,
        })),
    )
}

fn act_type_visual_delta_params_invalid(
    field: &'static str,
    detail: impl Into<String>,
    reason: &'static str,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code = error_codes::TOOL_PARAMS_INVALID,
        tool = "act_type",
        field,
        reason,
        detail = %detail,
        "act_type visual delta parameters invalid"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("act_type verify_target_window_hwnd invalid: {detail}"),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": "act_type",
            "field": field,
            "reason": reason,
            "detail": detail,
        })),
    )
}

fn compile_act_press_policy_regex(
    field: &'static str,
    pattern: Option<&str>,
) -> Result<Option<regex::Regex>, ErrorData> {
    let Some(pattern) = pattern else {
        return Ok(None);
    };
    if pattern.trim().is_empty() {
        return Err(act_press_policy_params_invalid(
            field,
            format!("{field} must not be empty"),
            "empty_expected_foreground_regex",
        ));
    }
    regex::Regex::new(pattern)
        .map(Some)
        .map_err(|error| act_press_policy_invalid_regex(field, pattern, &error))
}

fn verify_foreground_transition(
    tool: &str,
    source_of_truth: &str,
    timeout_ms: u32,
    before_hash: String,
    after_hash: String,
    before: &ClickDeltaSignature,
    after: &ClickDeltaSignature,
    policy: &ForegroundChangePolicy,
) -> Result<ActPostcondition, ErrorData> {
    if !policy.allow {
        return Err(foreground_lost_delta_error(
            tool,
            source_of_truth,
            timeout_ms,
            &before_hash,
            &after_hash,
            before,
            after,
        ));
    }

    let process_matches = policy
        .expected_process_regex
        .as_ref()
        .is_none_or(|regex| regex.is_match(&after.foreground_process));
    let title_matches = policy
        .expected_title_regex
        .as_ref()
        .is_none_or(|regex| regex.is_match(&after.foreground_title));

    if !process_matches || !title_matches {
        return Err(foreground_change_policy_mismatch_error(
            tool,
            source_of_truth,
            timeout_ms,
            &before_hash,
            &after_hash,
            before,
            after,
            policy,
            process_matches,
            title_matches,
        ));
    }

    tracing::info!(
        code = "ACTION_FOREGROUND_CHANGE_ACCEPTED",
        tool,
        source_of_truth,
        timeout_ms,
        before_hwnd = before.foreground_hwnd,
        after_hwnd = after.foreground_hwnd,
        before_pid = before.foreground_pid,
        after_pid = after.foreground_pid,
        before_process = %before.foreground_process,
        after_process = %after.foreground_process,
        after_title_sha256 = ?after.foreground_title_sha256,
        expected_process_regex = ?policy.expected_process_pattern,
        expected_title_regex = ?policy.expected_title_pattern,
        before_signature = before_hash,
        after_signature = after_hash,
        "verify_delta accepted declared foreground target transition"
    );

    Ok(postcondition_observed_delta(
        tool,
        source_of_truth,
        before_hash,
        after_hash,
        format!(
            "observed expected foreground transition after delivery; before_hwnd=0x{:x}; after_hwnd=0x{:x}; after_process={}; after_title_sha256={}; expected_process_regex_present={}; expected_title_regex_present={}",
            before.foreground_hwnd,
            after.foreground_hwnd,
            after.foreground_process,
            after.foreground_title_sha256.as_deref().unwrap_or("none"),
            policy.expected_process_regex.is_some(),
            policy.expected_title_regex.is_some()
        ),
    ))
}

fn foreground_change_policy_readback(policy: &ForegroundChangePolicy) -> Value {
    json!({
        "allow_foreground_change": policy.allow,
        "expected_foreground_process_regex": policy.expected_process_pattern,
        "expected_foreground_title_regex": policy.expected_title_pattern,
    })
}

fn act_press_policy_params_invalid(
    field: &'static str,
    detail: impl Into<String>,
    reason: &'static str,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code = error_codes::TOOL_PARAMS_INVALID,
        tool = "act_press",
        field,
        reason,
        detail = %detail,
        "act_press foreground-change policy parameters invalid"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": "act_press",
            "field": field,
            "reason": reason,
            "detail": detail,
        })),
    )
}

fn act_press_policy_invalid_regex(
    field: &'static str,
    pattern: &str,
    error: &regex::Error,
) -> ErrorData {
    let detail = format!("{field} is not a valid regex: {error}");
    tracing::error!(
        code = error_codes::TOOL_PARAMS_INVALID,
        tool = "act_press",
        field,
        reason = "invalid_expected_foreground_regex",
        pattern,
        regex_error = %error,
        "act_press foreground-change policy regex invalid"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": "act_press",
            "field": field,
            "reason": "invalid_expected_foreground_regex",
            "pattern": pattern,
            "detail": detail,
        })),
    )
}

fn can_route_click_element_background_first(
    params: &ActClickParams,
    recording: Option<&Arc<RecordingBackend>>,
) -> bool {
    recording.is_none() && click_params_can_route_background_first(params)
}

fn should_try_next_click_tier(error: &ErrorData) -> bool {
    match click_error_data_code(error) {
        Some(
            error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
            | error_codes::ACTION_NO_OBSERVED_DELTA
            | error_codes::ACTION_BACKEND_UNAVAILABLE,
        ) => true,
        Some(error_codes::ACTION_TARGET_INVALID) => click_postdispatch_readback_failed(error),
        _ => false,
    }
}

fn click_postdispatch_readback_failed(error: &ErrorData) -> bool {
    let detail = click_error_detail(error).to_ascii_lowercase();
    (detail.contains("togglepattern.toggle returned") && detail.contains("togglestate stayed"))
        || (detail.contains("selectionitempattern.select returned")
            && detail.contains("isselected stayed false"))
}

fn click_error_detail(error: &ErrorData) -> String {
    let mut detail = error.message.to_string();
    if let Some(data_detail) = error
        .data
        .as_ref()
        .and_then(|data| data.get("detail"))
        .and_then(Value::as_str)
    {
        detail.push(' ');
        detail.push_str(data_detail);
    }
    for attempt in click_tier_attempts_from_error(error) {
        if let Some(attempt_detail) = attempt.detail {
            detail.push(' ');
            detail.push_str(&attempt_detail);
        }
    }
    detail
}

fn should_try_click_postmessage_tier(tier_attempts: &[ActClickTierAttempt]) -> bool {
    !click_tier_attempted(tier_attempts, CLICK_TIER_POSTMESSAGE)
        && !click_tier_attempted(tier_attempts, CLICK_TIER_FOREGROUND)
}

fn should_try_click_foreground_tier(tier_attempts: &[ActClickTierAttempt]) -> bool {
    click_tier_attempted(tier_attempts, CLICK_TIER_POSTMESSAGE)
        && !click_tier_attempted(tier_attempts, CLICK_TIER_FOREGROUND)
}

fn click_tier_attempted(tier_attempts: &[ActClickTierAttempt], tier: &str) -> bool {
    tier_attempts.iter().any(|attempt| attempt.tier == tier)
}

fn click_error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

fn click_tier_attempts_from_error(error: &ErrorData) -> Vec<ActClickTierAttempt> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("tier_attempts"))
        .cloned()
        .and_then(|attempts| serde_json::from_value(attempts).ok())
        .unwrap_or_default()
}

fn merge_click_tier_attempts(
    mut prior_attempts: Vec<ActClickTierAttempt>,
    current_attempts: Vec<ActClickTierAttempt>,
) -> Vec<ActClickTierAttempt> {
    prior_attempts.extend(current_attempts);
    prior_attempts
}

fn elements_fingerprint_hash(elements: &[AccessibleNode]) -> Result<String, ErrorData> {
    let fingerprints: Vec<_> = elements
        .iter()
        .take(160)
        .map(|node| ClickElementFingerprint {
            element_id: node.element_id.to_string(),
            role: node.role.clone(),
            automation_id: node.automation_id.clone(),
            name_sha256: non_empty_sha256(&node.name),
            value_sha256: node.value.as_deref().and_then(non_empty_sha256),
            bbox: node.bbox,
            enabled: node.enabled,
            focused: node.focused,
        })
        .collect();
    hash_json(&fingerprints)
}

fn capture_pixel_signature(region: Rect) -> ClickPixelSignature {
    if region.w <= 0 || region.h <= 0 {
        return ClickPixelSignature {
            status: "unavailable".to_owned(),
            region,
            bitmap_sha256: None,
            detail: Some("foreground window bounds are empty".to_owned()),
        };
    }
    match synapse_capture::screen_region_to_bgra_bitmap(region) {
        Ok(bitmap) => {
            let mut hasher = Sha256::new();
            hasher.update(bitmap.width.to_le_bytes());
            hasher.update(bitmap.height.to_le_bytes());
            hasher.update(&bitmap.bytes);
            ClickPixelSignature {
                status: "ok".to_owned(),
                region: bitmap.region,
                bitmap_sha256: Some(hex_encode(&hasher.finalize())),
                detail: Some(format!(
                    "captured {}x{} BGRA bytes={}",
                    bitmap.width,
                    bitmap.height,
                    bitmap.bytes.len()
                )),
            }
        }
        Err(error) => ClickPixelSignature {
            status: "unavailable".to_owned(),
            region,
            bitmap_sha256: None,
            detail: Some(error.to_string()),
        },
    }
}

fn point_delta_rect(point: Point) -> Rect {
    // Wide list rows often leave blank padding under the pointer; include nearby content.
    const HALF_WIDTH: i32 = 512;
    const HALF_HEIGHT: i32 = 192;
    Rect {
        x: point.x.saturating_sub(HALF_WIDTH),
        y: point.y.saturating_sub(HALF_HEIGHT),
        w: HALF_WIDTH * 2,
        h: HALF_HEIGHT * 2,
    }
}

fn signature_hash(signature: &ClickDeltaSignature) -> Result<String, ErrorData> {
    hash_json(signature)
}

fn focused_text_candidate(
    focused: Option<&FocusedElement>,
    elements: &[AccessibleNode],
) -> Option<ActTypeFocusedTextCandidate> {
    if let Some(node) = elements
        .iter()
        .find(|node| node.focused && has_text_readback_pattern(&node.patterns))
    {
        return Some(ActTypeFocusedTextCandidate {
            element_id: node.element_id.to_string(),
            role: node.role.clone(),
            name: node.name.clone(),
            selected_text: None,
            bbox: node.bbox,
            value: node.value.clone(),
            patterns: node.patterns.clone(),
        });
    }
    focused.map(|focused| ActTypeFocusedTextCandidate {
        element_id: focused.element_id.to_string(),
        role: focused.role.clone(),
        name: focused.name.clone(),
        selected_text: focused.selected_text.clone(),
        bbox: focused.bbox,
        value: focused.value.clone(),
        patterns: focused.patterns.clone(),
    })
}

fn focused_text_readback(
    focused: Option<&ActTypeFocusedTextCandidate>,
) -> (Option<String>, Option<&'static str>) {
    let Some(focused) = focused else {
        return (None, None);
    };
    if let Some(value) = &focused.value {
        return (Some(value.clone()), Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE));
    }
    if has_text_readback_pattern(&focused.patterns) {
        return (Some(String::new()), Some(ACT_TYPE_TEXT_SOURCE_UIA_EMPTY));
    }
    (None, None)
}

fn choose_act_type_text_readback(
    focused: Option<&ActTypeFocusedTextCandidate>,
    uia_value: Option<String>,
    uia_readback_source: Option<&'static str>,
    cdp_readback: &CdpActiveTextReadback,
    ocr_readback: &OcrTextReadback,
) -> (Option<String>, Option<String>) {
    if should_prefer_cdp_active_text(
        focused,
        uia_value.as_deref(),
        uia_readback_source,
        cdp_readback,
    ) {
        if let Some(value) = cdp_readback.value.clone() {
            return (
                Some(value),
                Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE.to_owned()),
            );
        }
    }

    let skip_uia = is_browser_shell_uia_readback(focused, uia_value.as_deref());
    if !skip_uia {
        if let Some(value) = uia_value {
            return (Some(value), uia_readback_source.map(str::to_owned));
        }
    }

    if let Some(value) = cdp_readback.value.clone() {
        return (
            Some(value),
            Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE.to_owned()),
        );
    }
    if !skip_uia {
        if let Some(value) = ocr_readback.value.clone() {
            return (
                Some(value),
                Some(ACT_TYPE_TEXT_SOURCE_OCR_FOCUSED_RECT.to_owned()),
            );
        }
    }
    (None, None)
}

fn is_chrome_bridge_target_id(target_id: &str) -> bool {
    target_id.starts_with("chrome-tab:")
}

fn chrome_bridge_session_target_parts(target: Option<&SessionTarget>) -> Option<(i64, &str)> {
    let Some(SessionTarget::Cdp {
        window_hwnd,
        cdp_target_id,
    }) = target
    else {
        return None;
    };
    is_chrome_bridge_target_id(cdp_target_id).then_some((*window_hwnd, cdp_target_id.as_str()))
}

fn act_type_chrome_bridge_type_postcondition(
    params: &ActTypeParams,
    emitted: &str,
    chars_typed: u32,
    readback: &crate::chrome_debugger_bridge::ChromeDebuggerTypeActiveElementResult,
) -> Result<ActPostcondition, ErrorData> {
    if readback.chars_typed != chars_typed {
        return Err(postcondition_failed_error(
            "act_type",
            ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_SOURCE_OF_TRUTH,
            format!(
                "Chrome bridge reported chars_typed={} but emitted chars_typed={chars_typed}",
                readback.chars_typed
            ),
            active_element_value_signature(&readback.before_active_element),
            active_element_value_signature(&readback.after_active_element),
            json!({
                "target_id": &readback.target_id,
                "tab_id": readback.tab_id,
                "reported_chars_typed": readback.chars_typed,
                "emitted_chars_typed": chars_typed,
            }),
        ));
    }
    let before_value = readback
        .before_active_element
        .value
        .as_deref()
        .unwrap_or_default();
    let after_value = readback
        .after_active_element
        .value
        .as_deref()
        .unwrap_or_default();
    let expected_value = readback.expected_value.as_deref().unwrap_or(after_value);
    let before_hash = active_element_value_signature(&readback.before_active_element);
    let after_hash = active_element_value_signature(&readback.after_active_element);
    if after_value != expected_value {
        return Err(postcondition_failed_error(
            "act_type",
            ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_SOURCE_OF_TRUTH,
            "Chrome bridge after-read active-element value did not match expected DOM value",
            before_hash,
            after_hash,
            json!({
                "target_id": &readback.target_id,
                "tab_id": readback.tab_id,
                "expected_value_len": expected_value.chars().count(),
                "expected_value_sha256": non_empty_sha256(expected_value),
                "after_value_len": after_value.chars().count(),
                "after_value_sha256": non_empty_sha256(after_value),
                "events_dispatched": &readback.events_dispatched,
                "readback_backend": &readback.readback_backend,
            }),
        ));
    }
    if !params.verify_delta {
        return Ok(postcondition_not_requested(
            "act_type",
            ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_SOURCE_OF_TRUTH,
        ));
    }
    if before_value == after_value {
        return Err(source_no_observed_delta_error(
            "act_type",
            ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_SOURCE_OF_TRUTH,
            params.verify_timeout_ms,
            before_hash,
            after_hash,
            json!({
                "target_id": &readback.target_id,
                "tab_id": readback.tab_id,
                "text_len": emitted.chars().count(),
                "expected_value_len": expected_value.chars().count(),
                "events_dispatched": &readback.events_dispatched,
                "readback_backend": &readback.readback_backend,
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        "act_type",
        ACT_TYPE_CHROME_BRIDGE_ACTIVE_ELEMENT_SOURCE_OF_TRUTH,
        before_hash,
        after_hash,
        format!(
            "observed inactive Chrome bridge active-element value change after background DOM typing; chars_typed={} events={:?}",
            readback.chars_typed, readback.events_dispatched
        ),
    ))
}

fn active_element_value_signature(
    active: &crate::chrome_debugger_bridge::ChromeDebuggerActiveElement,
) -> String {
    let value = active.value.as_deref().unwrap_or_default();
    text_sha256(value)
}

fn chrome_bridge_active_text_readback(
    target_id: &str,
    active: &crate::chrome_debugger_bridge::ChromeDebuggerActiveElement,
) -> CdpActiveTextReadback {
    let value = (active.available).then(|| active.value.clone()).flatten();
    let attempt = if active.available {
        "chrome_bridge_active_element:available".to_owned()
    } else {
        format!(
            "chrome_bridge_active_element:{}",
            active.error_code.as_deref().unwrap_or("unavailable")
        )
    };
    CdpActiveTextReadback {
        value_len: value.as_ref().map(|value| value.chars().count()),
        value_sha256: value.as_deref().and_then(non_empty_sha256),
        value,
        target_id: Some(target_id.to_owned()),
        has_active_element: active.has_active_element,
        is_editable: active.is_editable,
        tag_name: active.tag_name.clone(),
        id_sha256: active.id.as_deref().and_then(non_empty_sha256),
        name_sha256: active.name.as_deref().and_then(non_empty_sha256),
        error_code: active.error_code.clone(),
        error_detail_sha256: active.error_detail.as_deref().and_then(non_empty_sha256),
        attempt,
    }
}

fn should_prefer_cdp_active_text(
    focused: Option<&ActTypeFocusedTextCandidate>,
    uia_value: Option<&str>,
    uia_readback_source: Option<&'static str>,
    cdp_readback: &CdpActiveTextReadback,
) -> bool {
    if cdp_readback.value.is_none()
        || cdp_readback.has_active_element != Some(true)
        || cdp_readback.is_editable != Some(true)
    {
        return false;
    }
    let tag = cdp_readback.tag_name.as_deref().unwrap_or("").trim();
    if tag.is_empty() || tag.eq_ignore_ascii_case("BODY") || tag.eq_ignore_ascii_case("HTML") {
        return false;
    }
    if uia_readback_source == Some(ACT_TYPE_TEXT_SOURCE_UIA_EMPTY) {
        return true;
    }
    is_browser_shell_uia_readback(focused, uia_value)
}

fn is_browser_shell_uia_readback(
    focused: Option<&ActTypeFocusedTextCandidate>,
    uia_value: Option<&str>,
) -> bool {
    let Some(focused) = focused else {
        return false;
    };
    if !is_shell_text_focus_role(&focused.role) {
        return false;
    }
    match uia_value.map(str::trim) {
        Some(value) if value.is_empty() => true,
        Some(value) => looks_like_browser_page_value(value),
        None => false,
    }
}

fn is_shell_text_focus_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "window" | "document" | "pane" | "region" | "rootwebarea" | "webarea" | "web view"
    )
}

fn looks_like_browser_page_value(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("data:")
        || value.starts_with("about:")
        || value.starts_with("chrome://")
        || value.starts_with("edge://")
        || value.starts_with("file://")
}

async fn cdp_active_text_readback(input: &ObservationInput) -> CdpActiveTextReadback {
    let Some(cdp) = input.cdp.as_ref() else {
        return CdpActiveTextReadback {
            value: None,
            target_id: None,
            has_active_element: None,
            is_editable: None,
            tag_name: None,
            id_sha256: None,
            name_sha256: None,
            value_len: None,
            value_sha256: None,
            error_code: None,
            error_detail_sha256: None,
            attempt: "cdp_active_element_value:unavailable:no_cdp_diagnostics".to_owned(),
        };
    };
    let Some(endpoint) = cdp.endpoint.as_deref() else {
        return CdpActiveTextReadback {
            value: None,
            target_id: cdp.selected_target_id.clone(),
            has_active_element: None,
            is_editable: None,
            tag_name: None,
            id_sha256: None,
            name_sha256: None,
            value_len: None,
            value_sha256: None,
            error_code: cdp.reason_code.clone(),
            error_detail_sha256: cdp.detail.as_deref().and_then(non_empty_sha256),
            attempt: format!(
                "cdp_active_element_value:unavailable:status={}",
                cdp.status.as_str()
            ),
        };
    };
    let Some(target_id) = cdp.selected_target_id.as_deref() else {
        return CdpActiveTextReadback {
            value: None,
            target_id: None,
            has_active_element: None,
            is_editable: None,
            tag_name: None,
            id_sha256: None,
            name_sha256: None,
            value_len: None,
            value_sha256: None,
            error_code: Some(error_codes::A11Y_CDP_ATTACH_FAILED.to_owned()),
            error_detail_sha256: None,
            attempt: "cdp_active_element_value:unavailable:no_selected_target".to_owned(),
        };
    };

    match synapse_a11y::cdp_active_element_state(endpoint, target_id).await {
        Ok(state) => {
            let value =
                (state.has_active_element && state.is_editable).then(|| state.value.clone());
            CdpActiveTextReadback {
                value,
                target_id: Some(state.target_id),
                has_active_element: Some(state.has_active_element),
                is_editable: Some(state.is_editable),
                tag_name: (!state.tag_name.trim().is_empty()).then_some(state.tag_name),
                id_sha256: non_empty_sha256(&state.id),
                name_sha256: non_empty_sha256(&state.name),
                value_len: Some(state.value.chars().count()),
                value_sha256: Some(text_sha256(&state.value)),
                error_code: None,
                error_detail_sha256: None,
                attempt: if state.has_active_element && state.is_editable {
                    "cdp_active_element_value:available".to_owned()
                } else if state.has_active_element {
                    "cdp_active_element_value:unavailable:active_element_not_editable".to_owned()
                } else {
                    "cdp_active_element_value:unavailable:no_active_element".to_owned()
                },
            }
        }
        Err(error) => CdpActiveTextReadback {
            value: None,
            target_id: Some(target_id.to_owned()),
            has_active_element: None,
            is_editable: None,
            tag_name: None,
            id_sha256: None,
            name_sha256: None,
            value_len: None,
            value_sha256: None,
            error_code: Some(error.code().to_owned()),
            error_detail_sha256: non_empty_sha256(&error.to_string()),
            attempt: format!("cdp_active_element_value:error:{}", error.code()),
        },
    }
}

fn ocr_focused_rect_text_readback(
    focused: Option<&ActTypeFocusedTextCandidate>,
    elements: &[AccessibleNode],
) -> OcrTextReadback {
    let Some(focused) = focused else {
        return OcrTextReadback {
            value: None,
            word_count: 0,
            value_len: None,
            value_sha256: None,
            attempt: "ocr_focused_rect_text:unavailable:no_focused_element".to_owned(),
        };
    };
    if focused.bbox.w <= 0 || focused.bbox.h <= 0 {
        return OcrTextReadback {
            value: None,
            word_count: 0,
            value_len: None,
            value_sha256: None,
            attempt: "ocr_focused_rect_text:unavailable:empty_focused_bbox".to_owned(),
        };
    }

    let mut words = elements
        .iter()
        .filter(|node| {
            node.automation_id
                .as_deref()
                .is_some_and(|automation_id| automation_id.starts_with("ocr:word:"))
                && rects_intersect(node.bbox, focused.bbox)
                && !node.name.trim().is_empty()
        })
        .collect::<Vec<_>>();
    words.sort_by_key(|node| (node.bbox.y, node.bbox.x));
    let text = words
        .iter()
        .map(|node| node.name.trim())
        .collect::<Vec<_>>()
        .join(" ");
    let word_count = words.len();
    if word_count == 0 {
        return OcrTextReadback {
            value: None,
            word_count,
            value_len: None,
            value_sha256: None,
            attempt: "ocr_focused_rect_text:unavailable:no_ocr_words_in_focused_bbox".to_owned(),
        };
    }
    OcrTextReadback {
        value: Some(text.clone()),
        word_count,
        value_len: Some(text.chars().count()),
        value_sha256: Some(text_sha256(&text)),
        attempt: "ocr_focused_rect_text:available".to_owned(),
    }
}

fn rect_contains_point(rect: Rect, point: Point) -> bool {
    if rect.w <= 0 || rect.h <= 0 {
        return false;
    }
    point.x >= rect.x
        && point.x < rect.x.saturating_add(rect.w)
        && point.y >= rect.y
        && point.y < rect.y.saturating_add(rect.h)
}

fn rects_intersect(a: Rect, b: Rect) -> bool {
    if a.w <= 0 || a.h <= 0 || b.w <= 0 || b.h <= 0 {
        return false;
    }
    let a_right = a.x.saturating_add(a.w);
    let a_bottom = a.y.saturating_add(a.h);
    let b_right = b.x.saturating_add(b.w);
    let b_bottom = b.y.saturating_add(b.h);
    a.x < b_right && a_right > b.x && a.y < b_bottom && a_bottom > b.y
}

fn act_type_foreground_fallback_focus_matches_target(
    target: &ActTypeForegroundFallbackTarget,
    after_click: &ActTypeTextSignature,
) -> Result<(), ErrorData> {
    if after_click.foreground_hwnd != target.root_hwnd {
        return Err(act_type_foreground_fallback_focus_error(
            target,
            after_click,
            "foreground_hwnd_changed_after_target_click",
        ));
    }
    let role = after_click.focused_role.as_deref().unwrap_or_default();
    if !act_type_editable_role(role) {
        return Err(act_type_foreground_fallback_focus_error(
            target,
            after_click,
            "focused_role_is_not_text_editable",
        ));
    }
    if after_click
        .focused_element_id
        .as_deref()
        .is_some_and(|focused_id| focused_id == target.element_id)
    {
        return Ok(());
    }
    if after_click
        .focused_bbox
        .is_some_and(|bbox| rects_intersect(bbox, target.bbox))
    {
        return Ok(());
    }
    Err(act_type_foreground_fallback_focus_error(
        target,
        after_click,
        "focused_element_did_not_match_target_or_bbox",
    ))
}

fn act_type_target_center_point(
    target: &ActTypeForegroundFallbackTarget,
) -> Result<Point, ErrorData> {
    if target.bbox.w <= 0 || target.bbox.h <= 0 {
        return Err(act_type_foreground_fallback_target_invalid_error(
            target,
            "target bbox is empty or inverted",
        ));
    }
    let x = i64::from(target.bbox.x) + i64::from(target.bbox.w) / 2;
    let y = i64::from(target.bbox.y) + i64::from(target.bbox.h) / 2;
    let x = i32::try_from(x).map_err(|err| {
        act_type_foreground_fallback_target_invalid_error(
            target,
            format!("target bbox center x overflowed i32: {err}"),
        )
    })?;
    let y = i32::try_from(y).map_err(|err| {
        act_type_foreground_fallback_target_invalid_error(
            target,
            format!("target bbox center y overflowed i32: {err}"),
        )
    })?;
    Ok(Point { x, y })
}

fn has_text_readback_pattern(patterns: &[UiaPattern]) -> bool {
    patterns
        .iter()
        .any(|pattern| matches!(pattern, UiaPattern::Value | UiaPattern::Text))
}

fn normalized_text_contains(value: &str, emitted: &str) -> bool {
    if emitted.is_empty() {
        return false;
    }
    normalize_newlines(value).contains(&normalize_newlines(emitted))
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn text_sha256(value: &str) -> String {
    hex_encode(&Sha256::digest(value.as_bytes()))
}

fn hash_json<T: Serialize>(value: &T) -> Result<String, ErrorData> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("failed to encode click delta signature: {error}"),
        )
    })?;
    Ok(hex_encode(&Sha256::digest(bytes)))
}

fn non_empty_sha256(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| hex_encode(&Sha256::digest(trimmed.as_bytes())))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn foreground_identity_changed(before: &ClickDeltaSignature, after: &ClickDeltaSignature) -> bool {
    before.foreground_hwnd != after.foreground_hwnd
        || before.foreground_pid != after.foreground_pid
        || before.foreground_process != after.foreground_process
}

fn act_type_foreground_identity_changed(
    before: &ActTypeTextSignature,
    after: &ActTypeTextSignature,
) -> bool {
    before.foreground_hwnd != after.foreground_hwnd
        || before.foreground_pid != after.foreground_pid
        || before.foreground_process != after.foreground_process
}

fn act_type_text_terminal_failure(
    before: &ActTypeTextReadback,
    after: &ActTypeTextReadback,
) -> bool {
    act_type_foreground_identity_changed(&before.signature, &after.signature)
        || act_type_text_target_changed(&before.signature, &after.signature)
}

fn verify_act_type_browser_url_response(
    mut response: ActTypeResponse,
    before: ActTypeTextReadback,
    after: ActTypeTextReadback,
    before_hash: String,
    after_hash: String,
    verify_timeout_ms: u32,
    policy: &ActTypeBrowserUrlPolicy,
) -> Result<ActTypeResponse, ErrorData> {
    let before_signature_readback = before.signature.clone();
    let after_signature_readback = after.signature.clone();
    let after_url = after.browser_url.as_deref().ok_or_else(|| {
        postcondition_failed_error(
            "act_type",
            ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH,
            "expected_browser_url_regex was set but after-read browser target URL was absent",
            before_hash.clone(),
            after_hash.clone(),
            json!({
                "expected_browser_url_regex": &policy.expected_url_pattern,
                "before": before_signature_readback,
                "after": after_signature_readback,
            }),
        )
    })?;
    if !policy.expected_url_regex.is_match(after_url) {
        return Err(postcondition_failed_error(
            "act_type",
            ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH,
            "after-read browser target URL did not match expected_browser_url_regex",
            before_hash,
            after_hash,
            json!({
                "expected_browser_url_regex": &policy.expected_url_pattern,
                "after_url_len": after_url.chars().count(),
                "after_url_sha256": non_empty_sha256(after_url),
                "before": before.signature,
                "after": after.signature,
            }),
        ));
    }
    let before_url = before.browser_url.as_deref();
    response.postcondition = if before_url == Some(after_url) {
        ActPostcondition {
            status: "verified_state".to_owned(),
            observed_delta: Some(false),
            source_of_truth: Some(ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH.to_owned()),
            before_signature: Some(before_hash),
            after_signature: Some(after_hash),
            detail: Some(format!(
                "act_type verify_delta verified after-read browser target URL matched expected_browser_url_regex; no URL delta was observed within {verify_timeout_ms} ms"
            )),
        }
    } else {
        postcondition_observed_delta(
            "act_type",
            ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH,
            before_hash,
            after_hash,
            "observed after-read browser target URL matching expected_browser_url_regex after delivery",
        )
    };
    response.target_readback_required = false;
    response.target_text_integrity = ACT_TYPE_BROWSER_URL_TEXT_INTEGRITY.to_owned();
    Ok(response)
}

fn verify_act_type_text_response(
    mut response: ActTypeResponse,
    before: ActTypeTextReadback,
    after: ActTypeTextReadback,
    before_hash: String,
    after_hash: String,
    verify_timeout_ms: u32,
    emitted: &str,
) -> Result<ActTypeResponse, ErrorData> {
    let source_of_truth = act_type_text_source_of_truth(&before.signature, &after.signature);
    if act_type_foreground_identity_changed(&before.signature, &after.signature) {
        return Err(act_type_text_foreground_lost_error(
            verify_timeout_ms,
            &before_hash,
            &after_hash,
            &before.signature,
            &after.signature,
        ));
    }
    if act_type_text_target_changed(&before.signature, &after.signature) {
        return Err(postcondition_failed_error(
            "act_type",
            &source_of_truth,
            "focused text target changed before postcondition readback",
            before_hash,
            after_hash,
            json!({
                "before": before.signature,
                "after": after.signature,
            }),
        ));
    }
    if before.value == after.value {
        return Err(source_no_observed_delta_error(
            "act_type",
            &source_of_truth,
            verify_timeout_ms,
            before_hash,
            after_hash,
            json!({
                "before": before.signature,
                "after": after.signature,
            }),
        ));
    }
    let Some(after_value) = after.value.as_deref() else {
        return Err(act_type_verify_surface_unavailable_error(
            "after-read had no UIA, CDP, or OCR text Source-of-Truth surface for act_type verify_delta",
            after_hash,
            after.signature,
        ));
    };
    if !normalized_text_contains(after_value, emitted) {
        return Err(postcondition_failed_error(
            "act_type",
            &source_of_truth,
            "text Source-of-Truth changed but did not contain the emitted text",
            before_hash,
            after_hash,
            json!({
                "expected_emitted_len": emitted.chars().count(),
                "expected_emitted_sha256": text_sha256(emitted),
                "before": before.signature,
                "after": after.signature,
            }),
        ));
    }
    response.postcondition = postcondition_observed_delta(
        "act_type",
        &source_of_truth,
        before_hash,
        after_hash,
        "observed selected text Source-of-Truth changed and containing emitted text after delivery",
    );
    response.target_readback_required = false;
    response.target_text_integrity = format!(
        "{}:{}",
        ACT_TYPE_TEXT_INTEGRITY_PREFIX,
        after
            .signature
            .readback_source
            .as_deref()
            .unwrap_or("unknown")
    );
    Ok(response)
}

fn act_type_error_allows_visual_delta(error: &ErrorData) -> bool {
    match click_error_data_code(error) {
        Some(error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE) => true,
        Some(error_codes::ACTION_NO_OBSERVED_DELTA) => {
            act_type_no_observed_delta_has_no_text_surface(error)
        }
        _ => false,
    }
}

fn act_type_no_observed_delta_has_no_text_surface(error: &ErrorData) -> bool {
    let readback = error.data.as_ref().and_then(|data| {
        data.pointer("/verify_delta/readback")
            .or_else(|| data.get("readback"))
    });
    let Some(readback) = readback else {
        return false;
    };
    let before_has_text = readback
        .pointer("/before/has_text_readback")
        .and_then(Value::as_bool);
    let after_has_text = readback
        .pointer("/after/has_text_readback")
        .and_then(Value::as_bool);
    before_has_text == Some(false) && after_has_text == Some(false)
}

fn act_type_text_source_of_truth(
    before: &ActTypeTextSignature,
    after: &ActTypeTextSignature,
) -> String {
    after
        .readback_source
        .as_ref()
        .or(before.readback_source.as_ref())
        .map(|source| format!("{ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH}:{source}"))
        .unwrap_or_else(|| ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH.to_owned())
}

fn act_type_text_target_changed(
    before: &ActTypeTextSignature,
    after: &ActTypeTextSignature,
) -> bool {
    if before.focused_element_id != after.focused_element_id {
        return true;
    }
    if before.readback_source.as_deref() == Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE)
        || after.readback_source.as_deref() == Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE)
    {
        return before.cdp_selected_target_id != after.cdp_selected_target_id
            || before.cdp_active_tag_name != after.cdp_active_tag_name
            || before.cdp_active_id_sha256 != after.cdp_active_id_sha256
            || before.cdp_active_name_sha256 != after.cdp_active_name_sha256;
    }
    if before.readback_source.as_deref() == Some(ACT_TYPE_TEXT_SOURCE_OCR_FOCUSED_RECT)
        || after.readback_source.as_deref() == Some(ACT_TYPE_TEXT_SOURCE_OCR_FOCUSED_RECT)
    {
        return before.focused_bbox != after.focused_bbox;
    }
    false
}

/// Structured failure for the `act_set_field_text` foreground tier: precise
/// reason code, full target evidence, and the action error detail.
fn set_field_text_foreground_error(
    target: &ActTypeForegroundFallbackTarget,
    code: &'static str,
    reason: &'static str,
    detail: impl Into<String>,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code,
        tool = "act_set_field_text",
        element_id = %target.element_id,
        root_hwnd = target.root_hwnd,
        reason,
        detail = %detail,
        "act_set_field_text foreground tier failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": code,
            "tool": "act_set_field_text",
            "reason": reason,
            "detail": detail,
            "target": target,
            "backend_tier_used": crate::m2::TIER_FOREGROUND_KEYS,
            "required_foreground": true,
        })),
    )
}

/// Password targets never expose value content; verification compares the
/// UTF-16 password length Source of Truth instead, mirroring `act_set_value`.
fn set_field_text_password_response(
    params: &crate::m2::ActSetFieldTextParams,
    started: Instant,
    method: &str,
    before: &synapse_a11y::ElementValueReadback,
    after: &synapse_a11y::ElementValueReadback,
) -> Result<crate::m2::ActSetFieldTextResponse, ErrorData> {
    let element_id = crate::m2::required_element_id(params)?;
    let expected_len = params.text.encode_utf16().count();
    let before_len = before.password_len.unwrap_or(0);
    let after_len = after.password_len.unwrap_or(0);
    let signature = |len: usize| format!("password_len:{len}");
    let requested_len = u32::try_from(params.text.chars().count()).unwrap_or(u32::MAX);
    if after.password_len != Some(expected_len) {
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            tool = "act_set_field_text",
            element_id = %element_id,
            method,
            before_len,
            after_len,
            expected_len,
            "act_set_field_text password-length readback did not equal requested length"
        );
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "act_set_field_text Source-of-Truth postcondition failed: password target length readback does not equal requested text length",
            Some(json!({
                "code": error_codes::ACTION_POSTCONDITION_FAILED,
                "tool": "act_set_field_text",
                "method": method,
                "source_of_truth": crate::m2::SOURCE_UIA_PASSWORD_LENGTH,
                "before_len": before_len,
                "after_len": after_len,
                "expected_len": expected_len,
                "is_password": true,
            })),
        ));
    }
    let changed = before.password_len != after.password_len;
    Ok(crate::m2::ActSetFieldTextResponse {
        ok: true,
        method: method.to_owned(),
        backend_tier_used: crate::m2::TIER_FOREGROUND_KEYS.to_owned(),
        required_foreground: true,
        source_of_truth: crate::m2::SOURCE_UIA_PASSWORD_LENGTH.to_owned(),
        requested_len,
        before_len: u32::try_from(before_len).unwrap_or(u32::MAX),
        after_len: u32::try_from(after_len).unwrap_or(u32::MAX),
        requested_sha256: signature(expected_len),
        before_sha256: signature(before_len),
        after_sha256: signature(after_len),
        changed,
        postcondition: ActPostcondition {
            status: "verified_state".to_owned(),
            observed_delta: Some(changed),
            source_of_truth: Some(crate::m2::SOURCE_UIA_PASSWORD_LENGTH.to_owned()),
            before_signature: Some(signature(before_len)),
            after_signature: Some(signature(after_len)),
            detail: Some(
                "act_set_field_text password target length equals requested length; value content intentionally not read or compared"
                    .to_owned(),
            ),
        },
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

fn act_type_foreground_fallback_recording_error(
    target: &ActTypeForegroundFallbackTarget,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        "act_type into_element Chromium foreground fallback requires the live foreground input tier and cannot run against the recording backend",
        Some(json!({
            "code": error_codes::ACTION_BACKEND_UNAVAILABLE,
            "tool": "act_type",
            "reason": "chromium_foreground_fallback_recording_backend_unsupported",
            "target": target,
            "required_foreground": true,
            "target_readback_required": true,
        })),
    )
}

fn act_type_foreground_fallback_target_not_foreground_error(
    expected: &ForegroundProof,
    target: &ActTypeForegroundFallbackTarget,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_type into_element Chromium foreground fallback requires target hwnd 0x{:x} to be the current foreground hwnd, but preflight foreground was 0x{:x}",
            target.root_hwnd, expected.hwnd
        ),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LOST,
            "tool": "act_type",
            "reason": "into_element_foreground_fallback_target_not_foreground",
            "foreground_expected": expected,
            "target": target,
            "required_foreground": true,
            "target_readback_required": true,
        })),
    )
}

fn act_type_foreground_fallback_target_invalid_error(
    target: &ActTypeForegroundFallbackTarget,
    detail: impl Into<String>,
) -> ErrorData {
    let detail = detail.into();
    ErrorData::new(
        ErrorCode(-32099),
        format!("act_type into_element Chromium foreground fallback target invalid: {detail}"),
        Some(json!({
            "code": error_codes::ACTION_TARGET_INVALID,
            "tool": "act_type",
            "reason": "chromium_foreground_fallback_target_invalid",
            "detail": detail,
            "target": target,
            "required_foreground": true,
            "target_readback_required": true,
        })),
    )
}

fn act_type_foreground_fallback_click_error(
    target: &ActTypeForegroundFallbackTarget,
    point: Point,
    error: &ActionError,
) -> ErrorData {
    let mapped = crate::m2::action_error_to_mcp(error);
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_type into_element Chromium foreground fallback click failed at ({},{}): {}",
            point.x, point.y, error
        ),
        Some(json!({
            "code": mapped
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .unwrap_or(error_codes::ACTION_BACKEND_UNAVAILABLE),
            "tool": "act_type",
            "reason": "chromium_foreground_fallback_click_failed",
            "point": point,
            "target": target,
            "cause": mapped.data,
            "required_foreground": true,
            "target_readback_required": true,
        })),
    )
}

fn act_type_foreground_fallback_focus_error(
    target: &ActTypeForegroundFallbackTarget,
    after_click: &ActTypeTextSignature,
    reason: &'static str,
) -> ErrorData {
    let after_hash =
        verify_hash_json(after_click).unwrap_or_else(|_| "hash_unavailable".to_owned());
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_type into_element Chromium foreground fallback click did not focus the requested editable target: {reason}"
        ),
        Some(json!({
            "code": error_codes::ACTION_TARGET_INVALID,
            "tool": "act_type",
            "reason": reason,
            "source_of_truth": ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH,
            "after_click_signature": after_hash,
            "after_click": after_click,
            "target": target,
            "required_foreground": true,
            "target_readback_required": true,
        })),
    )
}

fn act_type_verify_surface_unavailable_error(
    detail: impl Into<String>,
    signature_hash: String,
    readback: ActTypeTextSignature,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code = error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE,
        tool = "act_type",
        source_of_truth = ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH,
        signature = %signature_hash,
        readback_source = ?readback.readback_source,
        attempts = ?readback.text_readback_attempts,
        "act_type verify_delta text Source-of-Truth surface unavailable"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("act_type verify_delta Source-of-Truth surface unavailable: {detail}"),
        Some(json!({
            "code": error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE,
            "tool": "act_type",
            "source_of_truth": ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH,
            "detail": detail,
            "signature": signature_hash,
            "readback": readback,
        })),
    )
}

fn act_type_browser_url_readback_error(
    code: &str,
    detail: impl Into<String>,
    cdp_status: Option<&str>,
    endpoint: Option<&str>,
    cdp_detail: Option<&str>,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code,
        tool = "act_type",
        source_of_truth = ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH,
        cdp_status,
        endpoint,
        detail = %detail,
        cdp_detail,
        "act_type browser URL Source-of-Truth readback failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": code,
            "tool": "act_type",
            "source_of_truth": ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH,
            "cdp_status": cdp_status,
            "endpoint": endpoint,
            "cdp_detail": cdp_detail,
            "detail": detail,
        })),
    )
}

fn act_type_text_foreground_lost_error(
    timeout_ms: u32,
    before_hash: &str,
    after_hash: &str,
    before: &ActTypeTextSignature,
    after: &ActTypeTextSignature,
) -> ErrorData {
    tracing::warn!(
        code = error_codes::ACTION_FOREGROUND_LOST,
        tool = "act_type",
        source_of_truth = ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH,
        timeout_ms,
        before_hwnd = before.foreground_hwnd,
        after_hwnd = after.foreground_hwnd,
        before_pid = before.foreground_pid,
        after_pid = after.foreground_pid,
        before_process = %before.foreground_process,
        after_process = %after.foreground_process,
        before_signature = before_hash,
        after_signature = after_hash,
        "act_type text readback foreground target identity changed before postcondition readback"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_type text readback cannot accept observed delta because foreground target changed within {timeout_ms} ms"
        ),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LOST,
            "tool": "act_type",
            "source_of_truth": ACT_TYPE_FOREGROUND_TEXT_SOURCE_OF_TRUTH,
            "verify_delta": {
                "timeout_ms": timeout_ms,
                "before_signature": before_hash,
                "after_signature": after_hash,
                "before": before,
                "after": after,
            }
        })),
    )
}

fn foreground_lost_delta_error(
    tool: &str,
    source_of_truth: &str,
    timeout_ms: u32,
    before_hash: &str,
    after_hash: &str,
    before: &ClickDeltaSignature,
    after: &ClickDeltaSignature,
) -> ErrorData {
    tracing::warn!(
        code = error_codes::ACTION_FOREGROUND_LOST,
        tool,
        source_of_truth,
        timeout_ms,
        before_hwnd = before.foreground_hwnd,
        after_hwnd = after.foreground_hwnd,
        before_pid = before.foreground_pid,
        after_pid = after.foreground_pid,
        before_process = %before.foreground_process,
        after_process = %after.foreground_process,
        before_title_sha256 = ?before.foreground_title_sha256,
        after_title_sha256 = ?after.foreground_title_sha256,
        before_signature = before_hash,
        after_signature = after_hash,
        "verify_delta foreground target identity changed before postcondition readback"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} verify_delta cannot accept observed delta because foreground target changed within {timeout_ms} ms"
        ),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LOST,
            "reason": "unexpected_foreground_change",
            "tool": tool,
            "source_of_truth": source_of_truth,
            "verify_delta": {
                "timeout_ms": timeout_ms,
                "before_signature": before_hash,
                "after_signature": after_hash,
                "before": before,
                "after": after,
            }
        })),
    )
}

fn foreground_change_policy_mismatch_error(
    tool: &str,
    source_of_truth: &str,
    timeout_ms: u32,
    before_hash: &str,
    after_hash: &str,
    before: &ClickDeltaSignature,
    after: &ClickDeltaSignature,
    policy: &ForegroundChangePolicy,
    process_matches: bool,
    title_matches: bool,
) -> ErrorData {
    tracing::error!(
        code = error_codes::ACTION_FOREGROUND_LOST,
        reason = "foreground_change_policy_mismatch",
        tool,
        source_of_truth,
        timeout_ms,
        before_hwnd = before.foreground_hwnd,
        after_hwnd = after.foreground_hwnd,
        before_pid = before.foreground_pid,
        after_pid = after.foreground_pid,
        before_process = %before.foreground_process,
        after_process = %after.foreground_process,
        before_title_sha256 = ?before.foreground_title_sha256,
        after_title_sha256 = ?after.foreground_title_sha256,
        expected_process_regex = ?policy.expected_process_pattern,
        expected_title_regex = ?policy.expected_title_pattern,
        process_matches,
        title_matches,
        before_signature = before_hash,
        after_signature = after_hash,
        "verify_delta foreground target changed but did not match declared policy"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} verify_delta foreground target changed but did not match expected foreground policy within {timeout_ms} ms"
        ),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LOST,
            "reason": "foreground_change_policy_mismatch",
            "tool": tool,
            "source_of_truth": source_of_truth,
            "foreground_change_policy": foreground_change_policy_readback(policy),
            "matches": {
                "process": process_matches,
                "title": title_matches,
            },
            "verify_delta": {
                "timeout_ms": timeout_ms,
                "before_signature": before_hash,
                "after_signature": after_hash,
                "before": before,
                "after": after,
            }
        })),
    )
}

fn action_preflight_details(preflight: &ActionPreflightReadback) -> Value {
    json!({
        "preflight": preflight,
    })
}

fn act_stroke_audit_details(stroke_details: &Value, preflight: &ActionPreflightReadback) -> Value {
    json!({
        "stroke": stroke_details,
        "preflight": preflight,
    })
}

fn act_stroke_failure_audit_details(
    stroke_details: &Value,
    preflight: &ActionPreflightReadback,
    error: &ErrorData,
) -> Value {
    json!({
        "stroke": stroke_details,
        "preflight": preflight,
        "failure": act_stroke_error_details(error),
    })
}

fn log_act_stroke_failure(details: &Value, error: &ErrorData) {
    let stroke = details.get("stroke").unwrap_or(&Value::Null);
    let failure = details.get("failure").unwrap_or(&Value::Null);
    let preflight = details.get("preflight").unwrap_or(&Value::Null);
    let error_code = failure
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    let path_id = stroke
        .get("path_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let path_kind = stroke
        .get("path_kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let backend_requested = stroke
        .get("backend_requested")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let backend_resolved = stroke
        .get("backend_resolved")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let point_index = failure.get("point_index").cloned().unwrap_or(Value::Null);
    let queue_rate_state = failure
        .get("queue_rate_state")
        .cloned()
        .unwrap_or_else(|| json!({ "kind": "not_rate_or_queue" }));
    let foreground_proof = preflight
        .get("after")
        .or_else(|| preflight.get("before"))
        .cloned()
        .unwrap_or(Value::Null);

    tracing::error!(
        code = error_code,
        detail = %error.message,
        path_id,
        path_kind,
        backend_requested,
        backend_resolved,
        point_index = ?point_index,
        queue_rate_state = ?queue_rate_state,
        foreground_proof = ?foreground_proof,
        fallback_path_executed = false,
        action_kind = "act_stroke",
        "act_stroke failed without fallback"
    );
}

fn require_diagnostic_confirm(
    actual: &str,
    expected: &'static str,
    tool: &'static str,
) -> Result<(), ErrorData> {
    if actual == expected {
        return Ok(());
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!("{tool} requires confirm=\"{expected}\""),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": "ACTION_DIAGNOSTIC_CONFIRM_MISMATCH",
            "expected_confirm": expected,
            "actual_confirm_present": !actual.is_empty(),
        })),
    ))
}

fn validate_diagnostic_ttl_ms(ttl_ms: u64) -> Result<(), ErrorData> {
    if (ACTION_DIAGNOSTIC_MIN_TTL_MS..=ACTION_DIAGNOSTIC_MAX_TTL_MS).contains(&ttl_ms) {
        return Ok(());
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!(
            "diagnostic ttl_ms must be between {ACTION_DIAGNOSTIC_MIN_TTL_MS} and {ACTION_DIAGNOSTIC_MAX_TTL_MS}; got {ttl_ms}"
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": "ACTION_DIAGNOSTIC_TTL_OUT_OF_RANGE",
            "min_ttl_ms": ACTION_DIAGNOSTIC_MIN_TTL_MS,
            "max_ttl_ms": ACTION_DIAGNOSTIC_MAX_TTL_MS,
            "ttl_ms": ttl_ms,
        })),
    ))
}

fn validate_queue_blocker_duration_ms(duration_ms: u32) -> Result<(), ErrorData> {
    if (ACTION_DIAGNOSTIC_MIN_QUEUE_BLOCKER_MS..=ACTION_DIAGNOSTIC_MAX_QUEUE_BLOCKER_MS)
        .contains(&duration_ms)
    {
        return Ok(());
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!(
            "diagnostic blocker_duration_ms must be between {ACTION_DIAGNOSTIC_MIN_QUEUE_BLOCKER_MS} and {ACTION_DIAGNOSTIC_MAX_QUEUE_BLOCKER_MS}; got {duration_ms}"
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": "ACTION_DIAGNOSTIC_QUEUE_BLOCKER_DURATION_OUT_OF_RANGE",
            "min_blocker_duration_ms": ACTION_DIAGNOSTIC_MIN_QUEUE_BLOCKER_MS,
            "max_blocker_duration_ms": ACTION_DIAGNOSTIC_MAX_QUEUE_BLOCKER_MS,
            "blocker_duration_ms": duration_ms,
        })),
    ))
}

fn token_bucket_readback(snapshot: TokenBucketSnapshot) -> TokenBucketReadback {
    TokenBucketReadback {
        capacity: snapshot.capacity,
        tokens: snapshot.tokens,
        refill_rate_per_s: snapshot.refill_rate_per_s,
        last_refill_ns: snapshot.last_refill_ns,
    }
}

fn schedule_rate_limit_reset(
    control: synapse_action::BackendRateLimitControl,
    backend: ResolvedBackend,
    ttl_ms: u64,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ttl_ms)).await;
        match control.reset_backend(backend) {
            Ok(readback) => {
                tracing::info!(
                    code = "ACTION_DIAGNOSTIC_RATE_LIMIT_RESET",
                    backend = readback.backend.as_str(),
                    before_capacity = readback.before.capacity,
                    before_tokens = readback.before.tokens,
                    before_refill_rate_per_s = readback.before.refill_rate_per_s,
                    after_capacity = readback.after.capacity,
                    after_tokens = readback.after.tokens,
                    after_refill_rate_per_s = readback.after.refill_rate_per_s,
                    "action diagnostic rate limit reset completed"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = error.code(),
                    backend = backend.as_str(),
                    detail = error.detail(),
                    "action diagnostic rate limit reset failed"
                );
            }
        }
    });
}

fn diagnostic_adjacent_point(from: Point) -> Point {
    let x = if from.x == i32::MAX {
        from.x - 1
    } else {
        from.x + 1
    };
    Point { x, y: from.y }
}

fn diagnostic_queue_blocker_action(from: Point, to: Point, duration_ms: u32) -> Action {
    Action::MouseStroke {
        path: PathSpec::Line {
            from: PathPoint::from(from),
            to: PathPoint::from(to),
        },
        button: None,
        profile: VelocityProfile::Constant,
        timing: StrokeTiming::DurationMs { duration_ms },
        motion_model: StrokeMotionModel::Path,
        humanize: None,
        backend: Backend::Software,
    }
}

fn saturate_action_queue(
    handle: &synapse_action::ActionHandle,
    boundary: OperatorPanicActionBoundary,
) -> Result<(u32, u32, bool), ErrorData> {
    let mut filler_attempts = 0_u32;
    let mut queued_fillers = 0_u32;
    for _ in 0..=ACTION_QUEUE_CAPACITY {
        filler_attempts = filler_attempts.saturating_add(1);
        let action = Action::MouseMoveRelative {
            dx: 0.0,
            dy: 0.0,
            backend: Backend::Software,
        };
        boundary.ensure("immediately_before_diagnostic_queue_filler")?;
        match handle.try_execute(action) {
            Ok(()) => {
                queued_fillers = queued_fillers.saturating_add(1);
            }
            Err(ActionError::QueueFull { .. }) => {
                return Ok((filler_attempts, queued_fillers, true));
            }
            Err(error) => {
                return Err(diagnostic_action_error_to_mcp(error));
            }
        }
    }
    Ok((filler_attempts, queued_fillers, false))
}

fn diagnostic_action_error_to_mcp(error: ActionError) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        error.to_string(),
        Some(json!({
            "code": error.code(),
            "detail": error.detail(),
            "retry_after_ms": error.retry_after_ms(),
            "queue_rate_state": diagnostic_queue_rate_state(&error),
        })),
    )
}

fn diagnostic_queue_rate_state(error: &ActionError) -> Value {
    match error {
        ActionError::RateLimited {
            retry_after_ms,
            detail,
        } => json!({
            "kind": "rate_limited",
            "retry_after_ms": retry_after_ms,
            "detail": detail,
        }),
        ActionError::QueueFull { detail } => json!({
            "kind": "queue_full",
            "detail": detail,
        }),
        _ => json!({
            "kind": "not_rate_or_queue",
        }),
    }
}

impl SynapseService {
    fn start_act_stroke_foreground_monitor(
        &self,
        preflight: &ActionPreflightReadback,
    ) -> ActStrokeForegroundMonitor {
        let cancel = CancellationToken::new();
        let expected = preflight
            .after
            .clone()
            .unwrap_or_else(|| preflight.before.clone());
        let task = tokio::spawn(monitor_act_stroke_foreground(
            self.clone(),
            expected,
            cancel.clone(),
        ));
        ActStrokeForegroundMonitor { cancel, task }
    }
}

struct ActStrokeForegroundMonitor {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Option<ErrorData>>,
}

async fn await_act_stroke_foreground_monitor(
    monitor: Option<ActStrokeForegroundMonitor>,
) -> Option<ErrorData> {
    let Some(monitor) = monitor else {
        return None;
    };
    monitor.cancel.cancel();
    match monitor.task.await {
        Ok(error) => error,
        Err(error) => Some(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("act_stroke foreground monitor join failed: {error}"),
        )),
    }
}

const fn should_monitor_act_stroke_foreground(
    recording_active: bool,
    requires_input_lease: bool,
) -> bool {
    !recording_active && requires_input_lease
}

const fn should_acquire_act_stroke_input_lease(
    recording_active: bool,
    requires_input_lease: bool,
) -> bool {
    !recording_active && requires_input_lease
}

async fn monitor_act_stroke_foreground(
    service: SynapseService,
    expected: ForegroundProof,
    cancel: CancellationToken,
) -> Option<ErrorData> {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return None,
            () = tokio::time::sleep(std::time::Duration::from_millis(ACT_STROKE_FOREGROUND_MONITOR_INTERVAL_MS)) => {}
        }

        match service.current_audit_foreground() {
            Ok(actual) if actual.hwnd == expected.hwnd => {}
            Ok(actual) => {
                synapse_action::request_release_interrupt();
                tracing::error!(
                    code = error_codes::ACTION_FOREGROUND_LOST,
                    expected_hwnd = expected.hwnd,
                    actual_hwnd = actual.hwnd,
                    expected_pid = expected.pid,
                    actual_pid = actual.pid,
                    expected_process_name = %expected.process_name,
                    actual_process_name = %actual.process_name,
                    expected_window_title = %expected.window_title,
                    actual_window_title = %actual.window_title,
                    action_kind = "act_stroke",
                    "act_stroke foreground lost mid-stroke; release interrupt requested"
                );
                return Some(act_stroke_foreground_lost_error(
                    &expected,
                    Some(&actual),
                    None,
                ));
            }
            Err(error) => {
                synapse_action::request_release_interrupt();
                tracing::error!(
                    code = error_codes::ACTION_FOREGROUND_LOST,
                    expected_hwnd = expected.hwnd,
                    expected_pid = expected.pid,
                    expected_process_name = %expected.process_name,
                    expected_window_title = %expected.window_title,
                    read_error = %error.message,
                    action_kind = "act_stroke",
                    "act_stroke foreground read failed mid-stroke; release interrupt requested"
                );
                return Some(act_stroke_foreground_lost_error(
                    &expected,
                    None,
                    Some(&error),
                ));
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize)]
struct BackgroundTargetForegroundGuard {
    element_hwnd: i64,
    root_hwnd: i64,
}

impl BackgroundTargetForegroundGuard {
    fn contains(self, hwnd: i64) -> bool {
        hwnd == self.element_hwnd || hwnd == self.root_hwnd
    }
}

fn act_set_value_target_foreground_guard(
    element_id: &ElementId,
) -> Result<BackgroundTargetForegroundGuard, ErrorData> {
    let hwnd = element_id.parts().map_err(|error| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "act_set_value element id {element_id} could not be parsed for foreground guard: {error}"
            ),
        )
    })?.hwnd;
    let root_hwnd = synapse_a11y::top_level_root_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "act_set_value element id {element_id} HWND 0x{hwnd:x} could not be normalized to a live top-level target root for foreground guard: {error}"
            ),
        )
    })?;
    Ok(BackgroundTargetForegroundGuard {
        element_hwnd: hwnd,
        root_hwnd,
    })
}

fn set_field_text_locator_window_hwnd(
    params: &crate::m2::ActSetFieldTextParams,
    locator: &crate::m2::ActSetFieldTextLocator,
    target: Option<&SessionTarget>,
) -> Result<i64, ErrorData> {
    if let Some(window_hwnd) = locator.window_hwnd {
        return crate::m1::validate_window_hwnd_shape("act_set_field_text", window_hwnd);
    }
    if let Some(target) = target {
        let window_hwnd = match target {
            SessionTarget::Window { hwnd } => *hwnd,
            SessionTarget::Cdp { window_hwnd, .. } => *window_hwnd,
        };
        return crate::m1::validate_window_hwnd_shape("act_set_field_text", window_hwnd);
    }
    if let Some(element_id) = params.element_id.as_ref() {
        let hwnd = element_id
            .parts()
            .map_err(|error| {
                mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "act_set_field_text locator could not use element_id HWND hint from {element_id}: {error}"
                    ),
                )
            })?
            .hwnd;
        let window_hwnd = synapse_a11y::top_level_root_hwnd(hwnd).or(Ok(hwnd))?;
        return crate::m1::validate_window_hwnd_shape("act_set_field_text", window_hwnd);
    }
    Err(mcp_error(
        error_codes::TARGET_NOT_SET,
        "act_set_field_text locator requires locator.window_hwnd, an active session target, or an element_id HWND hint; refusing to use the human foreground implicitly",
    ))
}

fn set_field_text_locator_find_params(
    locator: &crate::m2::ActSetFieldTextLocator,
    window_hwnd: i64,
) -> FindParams {
    FindParams {
        query: None,
        role: trimmed_non_empty(locator.role.as_deref()),
        name_substring: trimmed_non_empty(
            locator
                .name_substring
                .as_deref()
                .or(locator.name.as_deref()),
        ),
        automation_id: trimmed_non_empty(locator.automation_id.as_deref()),
        scope: Some(FindScope::Elements),
        limit: Some(20),
        in_window: None,
        window_hwnd: Some(window_hwnd),
    }
}

fn set_field_text_find_result_is_element(result: &FindResult) -> bool {
    result.kind == FindResultKind::Element && result.element_id.is_some()
}

fn set_field_text_locator_exact_name_matches(
    locator: &crate::m2::ActSetFieldTextLocator,
    result: &FindResult,
) -> bool {
    let Some(expected) = locator.name.as_deref().map(str::trim) else {
        return true;
    };
    result
        .name
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
}

fn set_field_text_locator_resolution_error(
    locator: &crate::m2::ActSetFieldTextLocator,
    window_hwnd: i64,
    resolution_phase: &'static str,
    reason: &'static str,
    candidates: &[FindResult],
) -> ErrorData {
    let code = if reason == "ambiguous" {
        error_codes::ACTION_TARGET_INVALID
    } else {
        error_codes::ACTION_ELEMENT_NOT_RESOLVED
    };
    let detail = if reason == "ambiguous" {
        format!(
            "act_set_field_text locator matched {} elements in window 0x{window_hwnd:x}; refine role/name/automation_id so exactly one element resolves",
            candidates.len()
        )
    } else {
        format!("act_set_field_text locator did not match any element in window 0x{window_hwnd:x}")
    };
    ErrorData::new(
        ErrorCode(-32099),
        detail,
        Some(json!({
            "code": code,
            "tool": "act_set_field_text",
            "reason": reason,
            "resolution_phase": resolution_phase,
            "window_hwnd": window_hwnd,
            "locator": set_field_text_locator_error_details(locator),
            "candidate_count": candidates.len(),
            "candidates": candidates
                .iter()
                .take(5)
                .map(set_field_text_locator_candidate_details)
                .collect::<Vec<_>>(),
        })),
    )
}

fn set_field_text_locator_error_details(locator: &crate::m2::ActSetFieldTextLocator) -> Value {
    json!({
        "window_hwnd": locator.window_hwnd,
        "role": locator.role.as_deref(),
        "name_sha256": locator
            .name
            .as_deref()
            .map(crate::m2::postcondition::text_signature),
        "name_len": locator.name.as_ref().map(|name| name.chars().count()),
        "name_substring_sha256": locator
            .name_substring
            .as_deref()
            .map(crate::m2::postcondition::text_signature),
        "name_substring_len": locator
            .name_substring
            .as_ref()
            .map(|name| name.chars().count()),
        "automation_id": locator.automation_id.as_deref(),
    })
}

fn set_field_text_locator_candidate_details(result: &FindResult) -> Value {
    json!({
        "element_id": result.element_id.as_ref().map(ToString::to_string),
        "role": result.role.as_deref(),
        "name_sha256": result
            .name
            .as_deref()
            .map(crate::m2::postcondition::text_signature),
        "name_len": result.name.as_ref().map(|name| name.chars().count()),
        "automation_id": result.automation_id.as_deref(),
        "bbox": result.bbox,
        "score": result.score,
    })
}

fn set_field_text_error_is_stale(
    result: &Result<crate::m2::ActSetFieldTextResponse, ErrorData>,
) -> bool {
    matches!(
        result,
        Err(error) if mcp_error_data_code(error) == Some(error_codes::A11Y_ELEMENT_STALE)
    )
}

fn mcp_error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}

fn trimmed_non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn act_click_target_foreground_guard(
    params: &ActClickParams,
) -> Result<Option<BackgroundTargetForegroundGuard>, ErrorData> {
    Ok(
        click_target_foreground_guard_hwnds(params)?.map(|(element_hwnd, root_hwnd)| {
            BackgroundTargetForegroundGuard {
                element_hwnd,
                root_hwnd,
            }
        }),
    )
}

fn act_scroll_target_foreground_guard(
    params: &ActScrollParams,
) -> Result<BackgroundTargetForegroundGuard, ErrorData> {
    let Some(target) = params.target.as_ref() else {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            "act_scroll foreground guard requires target.element_id",
        ));
    };
    let hwnd = target
        .element_id
        .parts()
        .map_err(|error| {
            mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "act_scroll element id {} could not be parsed for foreground guard: {error}",
                    target.element_id
                ),
            )
        })?
        .hwnd;
    let root_hwnd = synapse_a11y::top_level_root_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "act_scroll element id {} HWND 0x{hwnd:x} could not be normalized to a live top-level target root for foreground guard: {error}",
                target.element_id
            ),
        )
    })?;
    Ok(BackgroundTargetForegroundGuard {
        element_hwnd: hwnd,
        root_hwnd,
    })
}

fn verify_background_target_not_activated(
    tool: &'static str,
    action_source_of_truth: &str,
    target: BackgroundTargetForegroundGuard,
    before: &ForegroundContext,
    after: &ForegroundContext,
) -> Result<(), ErrorData> {
    if before.hwnd == after.hwnd && before.pid == after.pid {
        return Ok(());
    }
    if !target.contains(before.hwnd) && target.contains(after.hwnd) {
        let foreground_restore = attempt_background_foreground_restore(
            tool,
            action_source_of_truth,
            target,
            before,
            after,
        );
        return Err(background_foreground_lost_error(
            tool,
            action_source_of_truth,
            target,
            before,
            after,
            foreground_restore,
        ));
    }
    tracing::warn!(
        code = "BACKGROUND_FOREGROUND_CHANGED_NON_TARGET",
        tool,
        source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
        action_source_of_truth,
        target_element_hwnd = target.element_hwnd,
        target_root_hwnd = target.root_hwnd,
        before_hwnd = before.hwnd,
        after_hwnd = after.hwnd,
        before_pid = before.pid,
        after_pid = after.pid,
        before_process_name = %before.process_name,
        after_process_name = %after.process_name,
        "background action completed while foreground changed to a non-target window"
    );
    Ok(())
}

fn background_result_source_of_truth<T>(
    result: &Result<T, ErrorData>,
    response_source_of_truth: impl FnOnce(&T) -> &str,
    fallback: &'static str,
) -> String {
    match result {
        Ok(response) => response_source_of_truth(response).to_owned(),
        Err(error) => {
            background_error_source_of_truth(error).unwrap_or_else(|| fallback.to_owned())
        }
    }
}

fn background_error_source_of_truth(error: &ErrorData) -> Option<String> {
    let source = error
        .data
        .as_ref()
        .and_then(|data| data.get("source_of_truth"))?;
    if let Some(source) = source.as_str() {
        return Some(source.to_owned());
    }
    if source.is_array() {
        return Some(source.to_string());
    }
    None
}

fn background_result_with_foreground_guard<T>(
    tool: &'static str,
    action_source_of_truth: &str,
    target: BackgroundTargetForegroundGuard,
    before: &ForegroundContext,
    after: &ForegroundContext,
    result: Result<T, ErrorData>,
) -> Result<T, ErrorData> {
    match verify_background_target_not_activated(
        tool,
        action_source_of_truth,
        target,
        before,
        after,
    ) {
        Ok(()) => result,
        Err(foreground_error) => match result {
            Ok(_) => Err(foreground_error),
            Err(error) => Err(attach_background_foreground_guard_error(
                error,
                foreground_error,
            )),
        },
    }
}

fn background_result_with_foreground_read_error<T>(
    result: Result<T, ErrorData>,
    foreground_error: ErrorData,
) -> Result<T, ErrorData> {
    match result {
        Ok(_) => Err(foreground_error),
        Err(error) => Err(attach_background_foreground_guard_error(
            error,
            foreground_error,
        )),
    }
}

fn attach_background_foreground_guard_error(
    original: ErrorData,
    foreground_error: ErrorData,
) -> ErrorData {
    let code = original.code;
    let message = original.message.to_string();
    let original_data = original.data;
    let mut data = match original_data {
        Some(Value::Object(map)) => map,
        Some(other) => {
            let mut map = serde_json::Map::new();
            map.insert("original_data".to_owned(), other);
            map
        }
        None => serde_json::Map::new(),
    };
    data.insert(
        "background_foreground_guard".to_owned(),
        json!({
            "code": foreground_error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .unwrap_or(error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED),
            "message": foreground_error.message.to_string(),
            "data": foreground_error.data,
        }),
    );
    ErrorData::new(code, message, Some(Value::Object(data)))
}

fn attempt_background_foreground_restore(
    tool: &'static str,
    action_source_of_truth: &str,
    target: BackgroundTargetForegroundGuard,
    before: &ForegroundContext,
    after: &ForegroundContext,
) -> Value {
    #[cfg(test)]
    {
        let _ = (tool, action_source_of_truth, target, before, after);
        return json!({
            "attempted": false,
            "status": "skipped",
            "reason": "unit_test",
        });
    }

    #[cfg(not(test))]
    {
        let prior = match synapse_a11y::foreground_context(before.hwnd) {
            Ok(prior) => prior,
            Err(error) => {
                tracing::warn!(
                    code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                    reason = "background_prior_foreground_read_failed",
                    tool,
                    source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
                    action_source_of_truth,
                    target_element_hwnd = target.element_hwnd,
                    target_root_hwnd = target.root_hwnd,
                    prior_hwnd = before.hwnd,
                    expected_pid = before.pid,
                    target_hwnd = after.hwnd,
                    target_pid = after.pid,
                    restore_error = %error,
                    "background action target activation restore skipped because the prior foreground HWND could not be reread"
                );
                return json!({
                    "attempted": false,
                    "status": "skipped",
                    "reason": "background_prior_foreground_read_failed",
                    "prior_hwnd": before.hwnd,
                    "expected_pid": before.pid,
                    "target_foreground": foreground_context_details(after),
                    "read_error": {
                        "code": error.code(),
                        "message": error.to_string(),
                    },
                });
            }
        };

        if prior.pid != before.pid {
            tracing::warn!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED,
                reason = "background_prior_foreground_pid_mismatch",
                tool,
                source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
                action_source_of_truth,
                target_element_hwnd = target.element_hwnd,
                target_root_hwnd = target.root_hwnd,
                prior_hwnd = before.hwnd,
                expected_pid = before.pid,
                actual_pid = prior.pid,
                actual_process_name = %prior.process_name,
                actual_window_title = %prior.window_title,
                target_hwnd = after.hwnd,
                target_pid = after.pid,
                "background action target activation restore skipped because the prior HWND now belongs to a different process"
            );
            return json!({
                "attempted": false,
                "status": "skipped",
                "reason": "background_prior_foreground_pid_mismatch",
                "prior_expected": foreground_context_details(before),
                "prior_actual": foreground_context_details(&prior),
                "target_foreground": foreground_context_details(after),
            });
        }

        let intent = synapse_a11y::ForegroundActivationIntent::LeaseContextRestore { caller: tool };
        if let Err(error) = synapse_a11y::focus_window_with_intent(before.hwnd, intent) {
            tracing::error!(
                code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
                reason = "background_prior_foreground_focus_failed",
                tool,
                source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
                action_source_of_truth,
                target_element_hwnd = target.element_hwnd,
                target_root_hwnd = target.root_hwnd,
                prior_hwnd = before.hwnd,
                prior_pid = before.pid,
                target_hwnd = after.hwnd,
                target_pid = after.pid,
                restore_error = %error,
                "background action target activation restore failed while returning foreground to the pre-action window"
            );
            return json!({
                "attempted": true,
                "status": "failed",
                "reason": "background_prior_foreground_focus_failed",
                "prior_expected": foreground_context_details(before),
                "prior_actual": foreground_context_details(&prior),
                "target_foreground": foreground_context_details(after),
                "restore_error": {
                    "code": error.code(),
                    "message": error.to_string(),
                },
            });
        }

        match synapse_a11y::current_foreground_context() {
            Ok(restored) if restored.hwnd == before.hwnd && restored.pid == before.pid => {
                tracing::info!(
                    code = "BACKGROUND_FOREGROUND_RESTORED_AFTER_TARGET_ACTIVATION",
                    reason = "background_prior_foreground_restored",
                    tool,
                    source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
                    action_source_of_truth,
                    target_element_hwnd = target.element_hwnd,
                    target_root_hwnd = target.root_hwnd,
                    prior_hwnd = before.hwnd,
                    prior_pid = before.pid,
                    target_hwnd = after.hwnd,
                    target_pid = after.pid,
                    restored_hwnd = restored.hwnd,
                    restored_pid = restored.pid,
                    restored_process_name = %restored.process_name,
                    restored_window_title = %restored.window_title,
                    "background action target activation was repaired by restoring the pre-action foreground window"
                );
                json!({
                    "attempted": true,
                    "status": "restored",
                    "reason": "background_prior_foreground_restored",
                    "prior_expected": foreground_context_details(before),
                    "prior_actual": foreground_context_details(&prior),
                    "target_foreground": foreground_context_details(after),
                    "foreground_restored": foreground_context_details(&restored),
                })
            }
            Ok(restored) => {
                tracing::error!(
                    code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
                    reason = "background_prior_foreground_post_restore_mismatch",
                    tool,
                    source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
                    action_source_of_truth,
                    target_element_hwnd = target.element_hwnd,
                    target_root_hwnd = target.root_hwnd,
                    prior_hwnd = before.hwnd,
                    prior_pid = before.pid,
                    target_hwnd = after.hwnd,
                    target_pid = after.pid,
                    restored_hwnd = restored.hwnd,
                    restored_pid = restored.pid,
                    restored_process_name = %restored.process_name,
                    restored_window_title = %restored.window_title,
                    "background action target activation restore did not return the expected foreground window"
                );
                json!({
                    "attempted": true,
                    "status": "failed",
                    "reason": "background_prior_foreground_post_restore_mismatch",
                    "prior_expected": foreground_context_details(before),
                    "prior_actual": foreground_context_details(&prior),
                    "target_foreground": foreground_context_details(after),
                    "foreground_after_restore": foreground_context_details(&restored),
                })
            }
            Err(error) => {
                tracing::error!(
                    code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
                    reason = "background_prior_foreground_post_restore_read_failed",
                    tool,
                    source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
                    action_source_of_truth,
                    target_element_hwnd = target.element_hwnd,
                    target_root_hwnd = target.root_hwnd,
                    prior_hwnd = before.hwnd,
                    prior_pid = before.pid,
                    target_hwnd = after.hwnd,
                    target_pid = after.pid,
                    restore_error = %error,
                    "background action target activation restore could not read foreground after the restore attempt"
                );
                json!({
                    "attempted": true,
                    "status": "failed",
                    "reason": "background_prior_foreground_post_restore_read_failed",
                    "prior_expected": foreground_context_details(before),
                    "prior_actual": foreground_context_details(&prior),
                    "target_foreground": foreground_context_details(after),
                    "read_error": {
                        "code": error.code(),
                        "message": error.to_string(),
                    },
                })
            }
        }
    }
}

fn background_foreground_lost_error(
    tool: &'static str,
    action_source_of_truth: &str,
    target: BackgroundTargetForegroundGuard,
    before: &ForegroundContext,
    after: &ForegroundContext,
    foreground_restore: Value,
) -> ErrorData {
    let detail = format!(
        "{tool} returned a background result but foreground changed from hwnd 0x{:x} ({}) to hwnd 0x{:x} ({})",
        before.hwnd, before.process_name, after.hwnd, after.process_name
    );
    tracing::error!(
        code = error_codes::ACTION_FOREGROUND_LOST,
        reason = "background_action_changed_foreground",
        tool,
        source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
        action_source_of_truth,
        target_element_hwnd = target.element_hwnd,
        target_root_hwnd = target.root_hwnd,
        before_hwnd = before.hwnd,
        after_hwnd = after.hwnd,
        before_pid = before.pid,
        after_pid = after.pid,
        before_process_name = %before.process_name,
        after_process_name = %after.process_name,
        before_window_title = %before.window_title,
        after_window_title = %after.window_title,
        "background action changed foreground after reporting required_foreground=false"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LOST,
            "reason": "background_action_changed_foreground",
            "tool": tool,
            "source_of_truth": BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
            "action_source_of_truth": action_source_of_truth,
            "required_foreground": false,
            "target_element_hwnd": target.element_hwnd,
            "target_root_hwnd": target.root_hwnd,
            "detail": detail,
            "foreground_before": foreground_context_details(before),
            "foreground_after": foreground_context_details(after),
            "foreground_restore": foreground_restore,
        })),
    )
}

fn act_click_foreground_read_error(
    stage: &'static str,
    action_source_of_truth: &str,
    error: &ErrorData,
) -> ErrorData {
    let detail = format!(
        "act_click could not read foreground {stage} background dispatch: {}",
        error.message
    );
    tracing::error!(
        code = error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
        reason = "background_foreground_read_failed",
        tool = "act_click",
        source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
        action_source_of_truth,
        stage,
        read_error = %error.message,
        "act_click background foreground guard could not read OS foreground Source of Truth"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
            "reason": "background_foreground_read_failed",
            "tool": "act_click",
            "source_of_truth": BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
            "action_source_of_truth": action_source_of_truth,
            "stage": stage,
            "detail": detail,
            "read_error": error.data.clone(),
        })),
    )
}

fn act_set_value_foreground_read_error(
    stage: &'static str,
    action_source_of_truth: &str,
    error: &ErrorData,
) -> ErrorData {
    let detail = format!(
        "act_set_value could not read foreground {stage} background dispatch: {}",
        error.message
    );
    tracing::error!(
        code = error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
        reason = "background_foreground_read_failed",
        tool = "act_set_value",
        source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
        action_source_of_truth,
        stage,
        read_error = %error.message,
        "act_set_value background foreground guard could not read OS foreground Source of Truth"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
            "reason": "background_foreground_read_failed",
            "tool": "act_set_value",
            "source_of_truth": BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
            "action_source_of_truth": action_source_of_truth,
            "required_foreground": false,
            "stage": stage,
            "detail": detail,
            "read_error": {
                "message": error.message.to_string(),
                "data": error.data.clone(),
            },
        })),
    )
}

fn act_scroll_foreground_read_error(
    stage: &'static str,
    action_source_of_truth: &str,
    error: &ErrorData,
) -> ErrorData {
    let detail = format!(
        "act_scroll could not read foreground {stage} background dispatch: {}",
        error.message
    );
    tracing::error!(
        code = error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
        tool = "act_scroll",
        stage,
        source_of_truth = BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
        action_source_of_truth,
        error = %error.message,
        "background scroll foreground read failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
            "reason": "background_foreground_read_failed",
            "tool": "act_scroll",
            "stage": stage,
            "source_of_truth": BACKGROUND_FOREGROUND_SOURCE_OF_TRUTH,
            "action_source_of_truth": action_source_of_truth,
            "required_foreground": false,
            "detail": detail,
        })),
    )
}

fn act_stroke_foreground_lost_error(
    expected: &ForegroundProof,
    actual: Option<&ForegroundContext>,
    read_error: Option<&ErrorData>,
) -> ErrorData {
    let detail = match actual {
        Some(actual) => format!(
            "act_stroke expected foreground hwnd 0x{:x} ({}) but current foreground is hwnd 0x{:x} ({})",
            expected.hwnd, expected.window_title, actual.hwnd, actual.window_title
        ),
        None => format!(
            "act_stroke could not read current foreground mid-stroke for expected hwnd 0x{:x} ({})",
            expected.hwnd, expected.window_title
        ),
    };
    ErrorData::new(
        ErrorCode(-32099),
        detail.clone(),
        Some(json!({
            "code": error_codes::ACTION_FOREGROUND_LOST,
            "reason": "act_stroke_foreground_lost_mid_stroke",
            "detail": detail,
            "point_index": Value::Null,
            "queue_rate_state": {
                "kind": "not_rate_or_queue",
            },
            "foreground_expected": expected,
            "foreground_actual": actual.map(foreground_context_details),
            "foreground_read_error": read_error.map(|error| json!({
                "message": error.message.to_string(),
                "data": error.data.clone(),
            })),
        })),
    )
}

fn foreground_context_details(foreground: &ForegroundContext) -> Value {
    json!({
        "hwnd": foreground.hwnd,
        "pid": foreground.pid,
        "process_name": &foreground.process_name,
        "process_path": &foreground.process_path,
        "window_title": &foreground.window_title,
        "monitor_index": foreground.monitor_index,
        "dpi_scale": foreground.dpi_scale,
        "profile_id": &foreground.profile_id,
        "steam_appid": foreground.steam_appid,
        "is_fullscreen": foreground.is_fullscreen,
        "is_dwm_composed": foreground.is_dwm_composed,
        "window_bounds": &foreground.window_bounds,
    })
}

fn clipboard_response_audit_details(response: &ActClipboardResponse) -> Value {
    json!({
        "response": {
            "ok": response.ok,
            "verb": response.verb,
            "format": response.format,
            "written": response.written,
            "cleared": response.cleared,
            "text_len": response.text_len,
            "text_present": response.text.is_some(),
            "backing": response.backing,
            "backend_tier_used": response.backend_tier_used,
            "source_of_truth": response.source_of_truth,
            "os_clipboard_touched": response.os_clipboard_touched,
            "required_foreground": response.required_foreground,
            "lease_required": response.lease_required,
            "elapsed_ms": response.elapsed_ms,
        },
    })
}

fn clipboard_request_audit_details(params: &ActClipboardParams) -> Value {
    json!({
        "verb": params.verb,
        "format": params.format,
        "text_len": params.text.as_ref().map(|text| text.chars().count()),
        "session_scoped": true,
        "source_of_truth": "session_clipboard_buffer",
        "os_clipboard_touched": false,
        "backend_tier_used": "session_buffer",
        "required_foreground": false,
        "lease_required": false,
    })
}

#[cfg(test)]
mod tests;
