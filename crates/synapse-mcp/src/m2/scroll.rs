use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rmcp::ErrorData;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_action::{
    ActionBackend, ActionError, ActionHandle, EmitState, RecordedInput, RecordingBackend,
};
use synapse_core::{Action, Backend, ElementId, Point, error_codes};

use crate::m1::mcp_error;
use crate::m2::postcondition::{
    ActPostcondition, default_verify_timeout_ms, postcondition_not_requested,
};
#[cfg(windows)]
use crate::m2::postcondition::{hash_json, no_observed_delta_error, postcondition_observed_delta};
#[cfg(windows)]
use serde_json::json;

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT as WinPoint, RECT, WPARAM},
        UI::WindowsAndMessaging::{
            EnumChildWindows, GA_ROOT, GetAncestor, GetClassNameW, GetWindowRect, IsWindow,
            IsWindowVisible, PostMessageW, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WindowFromPoint,
        },
    },
    core::BOOL,
};

const SMOOTH_SCROLL_INTERVAL_MS: u32 = 30;
const MAX_SMOOTH_SCROLL_STEPS: u32 = 120;
const WHEEL_DELTA: i32 = 120;
#[cfg(windows)]
const MAX_TARGETED_WHEEL_MESSAGES: usize = 1024;
#[cfg(windows)]
const SOURCE_UIA_SCROLL_PATTERN: &str = "uia_scroll_pattern.scroll_state";
#[cfg(windows)]
const SOURCE_UIA_SCROLL_ITEM: &str = "uia_scroll_item_pattern.bounding_rect";

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActScrollParams {
    #[serde(default)]
    #[schemars(default)]
    pub dy: i32,
    #[serde(default)]
    #[schemars(default)]
    pub dx: i32,
    pub at: Option<ActScrollPoint>,
    pub target: Option<ActScrollElementTarget>,
    #[serde(default)]
    #[schemars(default)]
    pub smooth: bool,
    #[serde(default)]
    #[schemars(default)]
    pub verify_delta: bool,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
}

#[derive(Copy, Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActScrollPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActScrollElementTarget {
    pub element_id: ElementId,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActScrollResponse {
    pub ok: bool,
    pub dy: i32,
    pub dx: i32,
    pub smooth: bool,
    pub scrolled: bool,
    pub wheel_event_count: u32,
    pub smooth_interval_ms: u32,
    pub scheduled_smooth_total_ms: u32,
    pub backend_used: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub elapsed_ms: u32,
    pub postcondition: ActPostcondition,
}

pub(crate) async fn act_scroll_with_handle_and_boundary(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActScrollParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActScrollResponse, ErrorData> {
    validate_scroll_params(&params)?;
    let started = Instant::now();
    if params.dy == 0 && params.dx == 0 {
        if let Some(recording) = recording {
            execute_recording_noop(&recording, &params);
        }
        return Ok(response(&params, false, 0, "none", started));
    }

    if let Some(target) = &params.target {
        #[cfg(windows)]
        {
            if let Some(backend_node_id) =
                synapse_a11y::cdp_backend_from_element_id(&target.element_id)
            {
                return execute_cdp_scroll(
                    &params,
                    &target.element_id,
                    backend_node_id,
                    started,
                    boundary,
                )
                .await;
            }
            return execute_uia_scroll(&params, &target.element_id, started, boundary).await;
        }
        #[cfg(not(windows))]
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "act_scroll target.element_id {} is not a CDP web element id; UIA element-target scroll requires Windows ScrollPattern or ScrollItemPattern",
                    target.element_id
                ),
            ));
        }
    }

    let actions = scroll_actions(&params)?;
    let mut wheel_event_count = actions.len();
    let mut backend_used = "software";

    if let Some(recording) = recording {
        execute_recording(&recording, &actions, &params, boundary).await?;
    } else if let Some(point) = params.at.map(Into::into) {
        let dispatch = execute_targeted_scroll_actions(&params, point, boundary).await?;
        wheel_event_count = dispatch.wheel_event_count;
        backend_used = dispatch.backend_used;
    } else {
        execute_scroll_actions(&handle, actions, params.smooth, boundary).await?;
    }

    Ok(response(
        &params,
        true,
        wheel_event_count,
        backend_used,
        started,
    ))
}

impl ActScrollParams {
    pub(crate) const fn requires_input_lease(&self) -> bool {
        self.target.is_none() && self.at.is_none() && (self.dy != 0 || self.dx != 0)
    }

    pub(crate) fn verify_delta_point_region(&self) -> Option<Point> {
        if self.target.is_some() {
            None
        } else {
            self.at.map(Into::into)
        }
    }

    pub(crate) fn uses_element_target(&self) -> bool {
        self.target.is_some()
    }
}

impl From<ActScrollPoint> for Point {
    fn from(value: ActScrollPoint) -> Self {
        Self {
            x: value.x,
            y: value.y,
        }
    }
}

fn validate_scroll_params(params: &ActScrollParams) -> Result<(), ErrorData> {
    if params.at.is_some() && params.target.is_some() {
        return Err(mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "act_scroll accepts either at={x,y} or target={element_id}, not both",
        ));
    }
    if params.smooth {
        let step_count = smooth_step_count(params.dy, params.dx);
        if step_count > MAX_SMOOTH_SCROLL_STEPS {
            return Err(mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "act_scroll smooth=true step count {step_count} exceeds max {MAX_SMOOTH_SCROLL_STEPS}"
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn execute_cdp_scroll(
    params: &ActScrollParams,
    element_id: &ElementId,
    backend_node_id: i64,
    started: Instant,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActScrollResponse, ErrorData> {
    let hwnd = element_id
        .parts()
        .map_err(|err| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("web element id is malformed: {err}"),
            )
        })?
        .hwnd;
    let endpoint = synapse_a11y::endpoint_for_window(hwnd).ok_or_else(|| {
        mcp_error(
            error_codes::A11Y_CDP_UNREACHABLE,
            format!(
                "no reachable CDP endpoint for web element {element_id} (browser closed or debug port gone)"
            ),
        )
    })?;
    let title_hint = synapse_a11y::foreground_context(hwnd)
        .map(|context| context.window_title)
        .unwrap_or_default();
    let target_id_hint = synapse_a11y::cdp_target_from_element_id(element_id);
    let before = if params.verify_delta {
        Some(
            synapse_a11y::cdp_node_scroll_state(
                &endpoint,
                &title_hint,
                target_id_hint.as_deref(),
                backend_node_id,
            )
            .await
            .map_err(|err| mcp_error(err.code(), err.to_string()))?,
        )
    } else {
        None
    };
    let deltas = cdp_wheel_deltas(params)?;
    let wheel_event_count = deltas.len();
    boundary.ensure("immediately_before_cdp_scroll_node")?;
    let point = synapse_a11y::cdp_scroll_node(
        &endpoint,
        &title_hint,
        target_id_hint.as_deref(),
        backend_node_id,
        deltas,
        if params.smooth {
            SMOOTH_SCROLL_INTERVAL_MS
        } else {
            0
        },
    )
    .await
    .map_err(|err| mcp_error(err.code(), err.to_string()))?;
    let postcondition = if let Some(before) = before {
        tokio::time::sleep(Duration::from_millis(u64::from(params.verify_timeout_ms))).await;
        let after = synapse_a11y::cdp_node_scroll_state(
            &endpoint,
            &title_hint,
            target_id_hint.as_deref(),
            backend_node_id,
        )
        .await
        .map_err(|err| mcp_error(err.code(), err.to_string()))?;
        let postcondition = verify_cdp_scroll_delta(params.verify_timeout_ms, &before, &after)?;
        tracing::info!(
            code = "M2_ACT_SCROLL_CDP_WHEEL",
            kind = "act_scroll",
            element_id = %element_id,
            backend_node_id,
            viewport_x = point.x,
            viewport_y = point.y,
            wheel_event_count,
            dy = params.dy,
            dx = params.dx,
            smooth = params.smooth,
            before = ?before,
            after = ?after,
            "readback=cdp_node.scroll_state tool=act_scroll cdp_scroll_after"
        );
        postcondition
    } else {
        tracing::info!(
            code = "M2_ACT_SCROLL_CDP_WHEEL",
            kind = "act_scroll",
            element_id = %element_id,
            backend_node_id,
            viewport_x = point.x,
            viewport_y = point.y,
            wheel_event_count,
            dy = params.dy,
            dx = params.dx,
            smooth = params.smooth,
            "readback=cdp_dispatch tool=act_scroll cdp_scroll_after"
        );
        postcondition_not_requested("act_scroll", "cdp_node.scroll_state")
    };
    let mut response = response(params, true, wheel_event_count, "cdp", started);
    response.postcondition = postcondition;
    Ok(response)
}

#[cfg(windows)]
async fn execute_uia_scroll(
    params: &ActScrollParams,
    element_id: &ElementId,
    started: Instant,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActScrollResponse, ErrorData> {
    boundary.ensure("immediately_before_uia_scroll_element")?;
    let mut readback =
        synapse_a11y::scroll_element(element_id, params.dy, params.dx).map_err(|error| {
            mcp_error(
                error.code(),
                format!("act_scroll UIA target scroll failed for element {element_id}: {error}"),
            )
        })?;
    if params.verify_delta {
        tokio::time::sleep(Duration::from_millis(u64::from(params.verify_timeout_ms))).await;
        readback.after = synapse_a11y::element_scroll_state(element_id).map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "act_scroll UIA Source-of-Truth readback failed for element {element_id}: {error}"
                ),
            )
        })?;
    }

    let postcondition = uia_scroll_postcondition(params, &readback)?;
    let backend_used = uia_scroll_backend_used(&readback);
    tracing::info!(
        code = "M2_ACT_SCROLL_UIA_PATTERN",
        kind = "act_scroll",
        element_id = %element_id,
        method = %readback.method,
        backend_used,
        dy = params.dy,
        dx = params.dx,
        scroll_call_count = readback.scroll_call_count,
        verify_delta = params.verify_delta,
        before = ?readback.before,
        after = ?readback.after,
        "readback=uia_scroll_state tool=act_scroll uia_scroll_after"
    );

    let mut response = response(
        params,
        true,
        usize::try_from(readback.scroll_call_count).unwrap_or(usize::MAX),
        backend_used,
        started,
    );
    response.postcondition = postcondition;
    Ok(response)
}

#[cfg(windows)]
fn uia_scroll_postcondition(
    params: &ActScrollParams,
    readback: &synapse_a11y::ElementScrollReadback,
) -> Result<ActPostcondition, ErrorData> {
    let source_of_truth = uia_scroll_source_of_truth(readback);
    if !params.verify_delta {
        return Ok(postcondition_not_requested("act_scroll", source_of_truth));
    }
    let before_signature = hash_json(&readback.before)?;
    let after_signature = hash_json(&readback.after)?;
    if readback.before == readback.after {
        return Err(no_observed_delta_error(
            "act_scroll",
            source_of_truth,
            params.verify_timeout_ms,
            before_signature,
            after_signature,
            json!({
                "method": readback.method,
                "requested_dy": readback.requested_dy,
                "requested_dx": readback.requested_dx,
                "scroll_call_count": readback.scroll_call_count,
                "before": &readback.before,
                "after": &readback.after,
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        "act_scroll",
        source_of_truth,
        before_signature,
        after_signature,
        "observed UIA target scroll state change after control-pattern dispatch",
    ))
}

#[cfg(windows)]
fn uia_scroll_backend_used(readback: &synapse_a11y::ElementScrollReadback) -> &'static str {
    if readback.method == "uia_scroll_item_pattern" {
        "uia_scroll_item_pattern"
    } else {
        "uia_scroll_pattern"
    }
}

#[cfg(windows)]
fn uia_scroll_source_of_truth(readback: &synapse_a11y::ElementScrollReadback) -> &'static str {
    if readback.method == "uia_scroll_item_pattern" {
        SOURCE_UIA_SCROLL_ITEM
    } else {
        SOURCE_UIA_SCROLL_PATTERN
    }
}

#[cfg(windows)]
fn cdp_wheel_deltas(
    params: &ActScrollParams,
) -> Result<Vec<synapse_a11y::CdpWheelDelta>, ErrorData> {
    let mut ticks = if params.smooth {
        smooth_scroll_ticks(params)?
    } else {
        vec![(params.dy, params.dx)]
    };
    ticks.retain(|(dy, dx)| *dy != 0 || *dx != 0);
    Ok(ticks
        .into_iter()
        .map(|(dy, dx)| synapse_a11y::CdpWheelDelta {
            delta_x: scroll_ticks_to_cdp_delta(dx),
            delta_y: scroll_ticks_to_cdp_delta(dy),
        })
        .collect())
}

#[cfg(windows)]
fn smooth_scroll_ticks(params: &ActScrollParams) -> Result<Vec<(i32, i32)>, ErrorData> {
    let step_count = smooth_step_count(params.dy, params.dx);
    let capacity = usize::try_from(step_count).map_err(|_err| {
        mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "act_scroll smooth=true step count cannot fit in memory",
        )
    })?;
    let mut ticks = Vec::with_capacity(capacity);
    let mut vertical_ticks_remaining = params.dy;
    let mut horizontal_ticks_remaining = params.dx;
    for _ in 0..step_count {
        ticks.push((
            take_tick(&mut vertical_ticks_remaining),
            take_tick(&mut horizontal_ticks_remaining),
        ));
    }
    Ok(ticks)
}

#[cfg(windows)]
fn scroll_ticks_to_cdp_delta(ticks: i32) -> f64 {
    -f64::from(ticks.saturating_mul(WHEEL_DELTA))
}

#[cfg(windows)]
fn verify_cdp_scroll_delta(
    verify_timeout_ms: u32,
    before: &synapse_a11y::CdpScrollState,
    after: &synapse_a11y::CdpScrollState,
) -> Result<ActPostcondition, ErrorData> {
    let before_signature = hash_json(before)?;
    let after_signature = hash_json(after)?;
    if before == after {
        return Err(no_observed_delta_error(
            "act_scroll",
            "cdp_node.scroll_state",
            verify_timeout_ms,
            before_signature,
            after_signature,
            json!({
                "before": before,
                "after": after,
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        "act_scroll",
        "cdp_node.scroll_state",
        before_signature,
        after_signature,
        "observed target DOM scroll state change after CDP wheel dispatch",
    ))
}

fn scroll_actions(params: &ActScrollParams) -> Result<Vec<Action>, ErrorData> {
    if !params.smooth {
        return Ok(vec![scroll_action(
            params.dy,
            params.dx,
            params.at.map(Into::into),
        )]);
    }
    let step_count = smooth_step_count(params.dy, params.dx);
    let capacity = usize::try_from(step_count).map_err(|_err| {
        mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "act_scroll smooth=true step count cannot fit in memory",
        )
    })?;
    let mut actions = Vec::with_capacity(capacity);
    let mut vertical_ticks_remaining = params.dy;
    let mut horizontal_ticks_remaining = params.dx;
    for step_index in 0..step_count {
        let vertical_tick = take_tick(&mut vertical_ticks_remaining);
        let horizontal_tick = take_tick(&mut horizontal_ticks_remaining);
        actions.push(scroll_action(
            vertical_tick,
            horizontal_tick,
            if step_index == 0 {
                params.at.map(Into::into)
            } else {
                None
            },
        ));
    }
    Ok(actions)
}

const fn scroll_action(dy: i32, dx: i32, at: Option<Point>) -> Action {
    Action::MouseScroll {
        dy,
        dx,
        at,
        backend: Backend::Auto,
    }
}

fn smooth_step_count(dy: i32, dx: i32) -> u32 {
    dy.unsigned_abs().max(dx.unsigned_abs())
}

fn take_tick(value: &mut i32) -> i32 {
    match (*value).cmp(&0) {
        std::cmp::Ordering::Less => {
            *value += 1;
            -1
        }
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => {
            *value -= 1;
            1
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ScrollDispatchResult {
    backend_used: &'static str,
    wheel_event_count: usize,
}

async fn execute_targeted_scroll_actions(
    params: &ActScrollParams,
    point: Point,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ScrollDispatchResult, ErrorData> {
    execute_targeted_scroll_actions_platform(params, point, boundary).await
}

#[cfg(windows)]
async fn execute_targeted_scroll_actions_platform(
    params: &ActScrollParams,
    point: Point,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ScrollDispatchResult, ErrorData> {
    let readback =
        windows_hwnd_message_scroll_readback(point).map_err(|error| action_error_to_mcp(&error))?;
    let mut wheel_event_count = 0_usize;
    for delta in wheel_delta_chunks(params.dy).map_err(|error| action_error_to_mcp(&error))? {
        boundary.ensure("immediately_before_postmessage_vertical_wheel")?;
        post_wheel_message(readback.hwnd, WM_MOUSEWHEEL, delta, point)
            .map_err(|error| action_error_to_mcp(&error))?;
        wheel_event_count = wheel_event_count.saturating_add(1);
        tracing::info!(
            code = "M2_ACT_SCROLL_HWND_MESSAGE",
            kind = "act_scroll",
            target_hwnd = readback.hwnd,
            target_class = %readback.class_name,
            screen_x = point.x,
            screen_y = point.y,
            delta = i32::from(delta),
            axis = "vertical",
            "readback=window_message tool=act_scroll targeted_scroll_after"
        );
    }
    for delta in wheel_delta_chunks(params.dx).map_err(|error| action_error_to_mcp(&error))? {
        boundary.ensure("immediately_before_postmessage_horizontal_wheel")?;
        post_wheel_message(readback.hwnd, WM_MOUSEHWHEEL, delta, point)
            .map_err(|error| action_error_to_mcp(&error))?;
        wheel_event_count = wheel_event_count.saturating_add(1);
        tracing::info!(
            code = "M2_ACT_SCROLL_HWND_MESSAGE",
            kind = "act_scroll",
            target_hwnd = readback.hwnd,
            target_class = %readback.class_name,
            screen_x = point.x,
            screen_y = point.y,
            delta = i32::from(delta),
            axis = "horizontal",
            "readback=window_message tool=act_scroll targeted_scroll_after"
        );
    }
    Ok(ScrollDispatchResult {
        backend_used: "software_window_message",
        wheel_event_count,
    })
}

#[cfg(not(windows))]
async fn execute_targeted_scroll_actions_platform(
    _params: &ActScrollParams,
    point: Point,
    _boundary: super::OperatorPanicActionBoundary,
) -> Result<ScrollDispatchResult, ErrorData> {
    Err(action_error_to_mcp(&ActionError::BackendUnavailable {
        detail: format!("act_scroll at={point:?} targeted window-message path requires Windows"),
    }))
}

async fn execute_scroll_actions(
    handle: &ActionHandle,
    actions: Vec<Action>,
    smooth: bool,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let last_index = actions.len().saturating_sub(1);
    for (index, action) in actions.into_iter().enumerate() {
        boundary.ensure("immediately_before_foreground_scroll_dispatch")?;
        handle
            .execute(action)
            .await
            .map_err(|error| action_error_to_mcp(&error))?;
        if smooth && index < last_index {
            tokio::time::sleep(Duration::from_millis(u64::from(SMOOTH_SCROLL_INTERVAL_MS))).await;
        }
    }
    Ok(())
}

fn response(
    params: &ActScrollParams,
    scrolled: bool,
    wheel_event_count: usize,
    backend_used: &'static str,
    started: Instant,
) -> ActScrollResponse {
    let wheel_event_count = u32::try_from(wheel_event_count).unwrap_or(u32::MAX);
    let backend_tier_used = scroll_backend_tier_used(params, scrolled, backend_used);
    ActScrollResponse {
        ok: true,
        dy: params.dy,
        dx: params.dx,
        smooth: params.smooth,
        scrolled,
        wheel_event_count,
        smooth_interval_ms: if params.smooth {
            SMOOTH_SCROLL_INTERVAL_MS
        } else {
            0
        },
        scheduled_smooth_total_ms: scheduled_smooth_total_ms(params.smooth, wheel_event_count),
        backend_used: backend_used.to_owned(),
        backend_tier_used: backend_tier_used.to_owned(),
        required_foreground: scroll_required_foreground(backend_tier_used),
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        postcondition: postcondition_not_requested(
            "act_scroll",
            response_source_of_truth(params, scrolled),
        ),
    }
}

fn response_source_of_truth(params: &ActScrollParams, scrolled: bool) -> &'static str {
    if !scrolled {
        return "not_applicable.no_scroll";
    }
    if params.target.is_some() {
        "cdp_node.scroll_state"
    } else {
        "target_point_pixels_or_foreground_ui"
    }
}

fn scroll_backend_tier_used(
    params: &ActScrollParams,
    scrolled: bool,
    backend_used: &'static str,
) -> &'static str {
    if !scrolled {
        return "none";
    }
    if backend_used == "uia_scroll_pattern" || backend_used == "uia_scroll_item_pattern" {
        return "uia";
    }
    if params.target.is_some() || backend_used == "cdp" {
        return "cdp";
    }
    if params.at.is_some() || backend_used == "software_window_message" {
        return "postmessage";
    }
    "foreground"
}

fn scroll_required_foreground(backend_tier_used: &str) -> bool {
    backend_tier_used == "foreground"
}

const fn scheduled_smooth_total_ms(smooth: bool, wheel_event_count: u32) -> u32 {
    if !smooth || wheel_event_count == 0 {
        return 0;
    }
    wheel_event_count
        .saturating_sub(1)
        .saturating_mul(SMOOTH_SCROLL_INTERVAL_MS)
}

async fn execute_recording(
    recording: &RecordingBackend,
    actions: &[Action],
    params: &ActScrollParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let before_events = recording.events();
    let before_event_count = before_events.len();
    let mut emit_state = EmitState::new();
    let last_index = actions.len().saturating_sub(1);
    for (index, action) in actions.iter().enumerate() {
        boundary.ensure("immediately_before_recorded_scroll_dispatch")?;
        recording
            .execute(action, &mut emit_state)
            .map_err(|error| action_error_to_mcp(&error))?;
        if params.smooth && index < last_index {
            tokio::time::sleep(Duration::from_millis(u64::from(SMOOTH_SCROLL_INTERVAL_MS))).await;
        }
    }
    let after_events = recording.events();
    let new_events = &after_events[before_event_count..];
    log_recording_readback(before_event_count, &after_events, new_events, params);
    Ok(())
}

fn execute_recording_noop(recording: &RecordingBackend, params: &ActScrollParams) {
    let before_events = recording.events();
    let before_event_count = before_events.len();
    let after_events = recording.events();
    let new_events = &after_events[before_event_count..];
    log_recording_readback(before_event_count, &after_events, new_events, params);
}

fn log_recording_readback(
    before_event_count: usize,
    after_events: &[RecordedInput],
    new_events: &[RecordedInput],
    params: &ActScrollParams,
) {
    let event_sequence = event_sequence(new_events);
    let smooth_step_count = if params.smooth {
        smooth_step_count(params.dy, params.dx)
    } else {
        0
    };
    tracing::info!(
        code = "M2_ACT_SCROLL_RECORDING_READBACK",
        kind = "act_scroll",
        before_event_count,
        after_event_count = after_events.len(),
        new_event_count = new_events.len(),
        dy = params.dy,
        dx = params.dx,
        smooth = params.smooth,
        smooth_step_count,
        smooth_interval_ms = if params.smooth {
            SMOOTH_SCROLL_INTERVAL_MS
        } else {
            0
        },
        scheduled_smooth_total_ms = scheduled_smooth_total_ms(params.smooth, smooth_step_count),
        event_sequence,
        ?new_events,
        "readback=recording_backend tool=act_scroll after_events_readback"
    );
}

fn event_sequence(events: &[RecordedInput]) -> String {
    events.iter().map(event_label).collect::<Vec<_>>().join(">")
}

fn event_label(event: &RecordedInput) -> String {
    match event {
        RecordedInput::MouseScroll { dy, dx, at } => {
            format!("mouse_scroll:dy={dy}:dx={dx}:at={}", at_label(*at))
        }
        other => format!("{other:?}"),
    }
}

fn at_label(at: Option<Point>) -> String {
    at.map_or_else(
        || "none".to_owned(),
        |point| format!("screen({},{})", point.x, point.y),
    )
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    crate::m2::action_error_to_mcp(error)
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct HwndMessageScrollReadback {
    hwnd: i64,
    class_name: String,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct WindowCandidate {
    hwnd: HWND,
    rect: RECT,
    class_name: String,
}

#[cfg(windows)]
struct ChildEnumContext {
    point: Point,
    candidates: Vec<WindowCandidate>,
}

#[cfg(windows)]
fn windows_hwnd_message_scroll_readback(
    point: Point,
) -> Result<HwndMessageScrollReadback, ActionError> {
    let seed = unsafe {
        WindowFromPoint(WinPoint {
            x: point.x,
            y: point.y,
        })
    };
    if seed.0.is_null() {
        return Err(ActionError::TargetInvalid {
            detail: format!("act_scroll at point {point:?} is not over a live window"),
        });
    }
    let root = unsafe { GetAncestor(seed, GA_ROOT) };
    let root = if root.0.is_null() { seed } else { root };
    if !unsafe { IsWindow(Some(root)) }.as_bool() {
        return Err(ActionError::TargetInvalid {
            detail: format!(
                "act_scroll root hwnd 0x{:x} for point {point:?} is not a live window",
                hwnd_to_i64(root)
            ),
        });
    }

    let target = hit_test_hwnd_for_screen_point(seed, root, point)?;
    let _ = screen_lparam(point)?;
    Ok(HwndMessageScrollReadback {
        hwnd: hwnd_to_i64(target.hwnd),
        class_name: target.class_name,
    })
}

#[cfg(windows)]
fn hit_test_hwnd_for_screen_point(
    seed: HWND,
    root: HWND,
    point: Point,
) -> Result<WindowCandidate, ActionError> {
    let root_rect = window_rect(root)?;
    if !rect_contains_point(&root_rect, point) {
        return Err(ActionError::TargetInvalid {
            detail: format!(
                "act_scroll point {point:?} is outside root hwnd 0x{:x} rect {:?}",
                hwnd_to_i64(root),
                rect_tuple(&root_rect)
            ),
        });
    }

    if let Ok(seed_rect) = window_rect(seed)
        && unsafe { IsWindowVisible(seed) }.as_bool()
        && rect_contains_point(&seed_rect, point)
        && rect_area(&seed_rect) > 0
    {
        return Ok(WindowCandidate {
            hwnd: seed,
            rect: seed_rect,
            class_name: window_class_name(seed),
        });
    }

    best_child_hwnd_for_screen_point(root, root_rect, point)
}

#[cfg(windows)]
fn best_child_hwnd_for_screen_point(
    root: HWND,
    root_rect: RECT,
    point: Point,
) -> Result<WindowCandidate, ActionError> {
    let mut context = ChildEnumContext {
        point,
        candidates: Vec::new(),
    };
    let context_ptr = (&raw mut context).cast::<c_void>();
    let _ = unsafe {
        EnumChildWindows(
            Some(root),
            Some(enum_child_containing_point),
            LPARAM(context_ptr as isize),
        )
    };

    Ok(context
        .candidates
        .into_iter()
        .min_by_key(|candidate| rect_area(&candidate.rect))
        .unwrap_or_else(|| WindowCandidate {
            hwnd: root,
            rect: root_rect,
            class_name: window_class_name(root),
        }))
}

#[cfg(windows)]
unsafe extern "system" fn enum_child_containing_point(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = unsafe { &mut *(lparam.0 as *mut ChildEnumContext) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool()
        && let Ok(rect) = window_rect(hwnd)
        && rect_contains_point(&rect, context.point)
        && rect_area(&rect) > 0
    {
        context.candidates.push(WindowCandidate {
            hwnd,
            rect,
            class_name: window_class_name(hwnd),
        });
    }
    BOOL(1)
}

#[cfg(windows)]
fn post_wheel_message(
    hwnd: i64,
    message: u32,
    delta: i16,
    screen_point: Point,
) -> Result<(), ActionError> {
    let hwnd = hwnd_from_i64(hwnd)?;
    let wparam = wheel_wparam(delta)?;
    let lparam = screen_lparam(screen_point)?;
    unsafe { PostMessageW(Some(hwnd), message, wparam, lparam) }.map_err(|error| {
        ActionError::BackendUnavailable {
            detail: format!(
                "PostMessageW act_scroll wheel message 0x{message:x} failed for hwnd 0x{:x} screen_point={screen_point:?} delta={delta}: {error}",
                hwnd_to_i64(hwnd)
            ),
        }
    })
}

#[cfg(windows)]
fn wheel_delta_chunks(ticks: i32) -> Result<Vec<i16>, ActionError> {
    if ticks == 0 {
        return Ok(Vec::new());
    }
    let max_ticks_per_message = i32::from(i16::MAX) / WHEEL_DELTA;
    let mut remaining = ticks;
    let mut chunks = Vec::new();
    while remaining != 0 {
        if chunks.len() >= MAX_TARGETED_WHEEL_MESSAGES {
            return Err(ActionError::TargetInvalid {
                detail: format!(
                    "act_scroll targeted wheel message count exceeds {MAX_TARGETED_WHEEL_MESSAGES} for ticks={ticks}"
                ),
            });
        }
        let step_ticks = remaining.clamp(-max_ticks_per_message, max_ticks_per_message);
        let delta = step_ticks.saturating_mul(WHEEL_DELTA);
        chunks.push(
            i16::try_from(delta).map_err(|error| ActionError::TargetInvalid {
                detail: format!(
                    "act_scroll wheel delta {delta} cannot fit WM_MOUSE*WHEEL i16: {error}"
                ),
            })?,
        );
        remaining = remaining.saturating_sub(step_ticks);
    }
    Ok(chunks)
}

#[cfg(windows)]
fn wheel_wparam(delta: i16) -> Result<WPARAM, ActionError> {
    let high_word = u32::from(u16::from_ne_bytes(delta.to_ne_bytes())) << 16;
    Ok(WPARAM(usize::try_from(high_word).map_err(|error| {
        ActionError::TargetInvalid {
            detail: format!("act_scroll wheel wParam overflowed usize: {error}"),
        }
    })?))
}

#[cfg(windows)]
fn screen_lparam(point: Point) -> Result<LPARAM, ActionError> {
    let x = i16::try_from(point.x).map_err(|error| ActionError::TargetInvalid {
        detail: format!(
            "act_scroll screen x {} cannot fit a WM_MOUSE*WHEEL lParam i16: {error}",
            point.x
        ),
    })?;
    let y = i16::try_from(point.y).map_err(|error| ActionError::TargetInvalid {
        detail: format!(
            "act_scroll screen y {} cannot fit a WM_MOUSE*WHEEL lParam i16: {error}",
            point.y
        ),
    })?;
    let packed = (u32::from(u16::from_ne_bytes(y.to_ne_bytes())) << 16)
        | u32::from(u16::from_ne_bytes(x.to_ne_bytes()));
    Ok(LPARAM(isize::try_from(packed).unwrap_or(isize::MAX)))
}

#[cfg(windows)]
fn window_rect(hwnd: HWND) -> Result<RECT, ActionError> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &raw mut rect) }.map_err(|error| {
        ActionError::ElementNotResolved {
            detail: format!(
                "GetWindowRect failed for act_scroll hwnd 0x{:x}: {error}",
                hwnd_to_i64(hwnd)
            ),
        }
    })?;
    Ok(rect)
}

#[cfg(windows)]
fn rect_contains_point(rect: &RECT, point: Point) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

#[cfg(windows)]
fn rect_area(rect: &RECT) -> i64 {
    let width = i64::from(rect.right.saturating_sub(rect.left).max(0));
    let height = i64::from(rect.bottom.saturating_sub(rect.top).max(0));
    width.saturating_mul(height)
}

#[cfg(windows)]
fn rect_tuple(rect: &RECT) -> (i32, i32, i32, i32) {
    (rect.left, rect.top, rect.right, rect.bottom)
}

#[cfg(windows)]
fn window_class_name(hwnd: HWND) -> String {
    let mut buffer = vec![0_u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)])
}

#[cfg(windows)]
fn hwnd_from_i64(hwnd: i64) -> Result<HWND, ActionError> {
    let native = synapse_core::win32_hwnd::hwnd_from_wire(hwnd).ok_or_else(|| {
        ActionError::TargetInvalid {
            detail: format!(
                "act_scroll target hwnd {hwnd} is outside the canonical Win32 USER-handle range 1..=4294967295"
            ),
        }
    })?;
    Ok(HWND(native as *mut c_void))
}

#[cfg(windows)]
fn hwnd_to_i64(hwnd: HWND) -> i64 {
    synapse_core::win32_hwnd::hwnd_to_wire(hwnd.0 as isize)
}
