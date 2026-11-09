use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_action::{
    ActionBackend, ActionError, ActionHandle, ArcLengthPath, EmitState, PathError, RecordedInput,
    RecordingBackend, StrokeError, StrokePlan, plan_timed_stroke, screen_point_from_path_point,
};
use synapse_core::{
    Action, Backend, ElementId, HumanizeParams, MouseButton, PathPoint, PathSpec, Point, Rect,
    StrokeMotionModel, StrokeTiming, VelocityProfile, error_codes,
};

use crate::m1::mcp_error;
use crate::m2::postcondition::{
    ActPostcondition, default_verify_timeout_ms, postcondition_not_requested,
};

pub const MAX_STROKE_PATH_POINTS: usize = 4096;
pub const MAX_STROKE_SAMPLES: usize = 60_001;
const MAX_STROKE_DURATION_MS: f64 = 60_000.0;
const MODIFIER_RELEASE_SETTLE_MS: u64 = 200;
const STROKE_DETAIL_COORD_NONFINITE: &str = "STROKE_COORD_NONFINITE";
const STROKE_DETAIL_COORD_OUT_OF_I32_RANGE: &str = "STROKE_COORD_OUT_OF_I32_RANGE";
const STROKE_DETAIL_POINT_OUT_OF_VIRTUAL_SCREEN: &str = "STROKE_POINT_OUT_OF_VIRTUAL_SCREEN";
const STROKE_DETAIL_PATH_DEGENERATE: &str = "STROKE_PATH_DEGENERATE";
const STROKE_DETAIL_PATH_POINT_CAP_EXCEEDED: &str = "STROKE_PATH_POINT_CAP_EXCEEDED";
const STROKE_DETAIL_SAMPLE_CAP_EXCEEDED: &str = "STROKE_SAMPLE_CAP_EXCEEDED";
const STROKE_DETAIL_DURATION_INVALID: &str = "STROKE_DURATION_INVALID";
const STROKE_DETAIL_DURATION_CAP_EXCEEDED: &str = "STROKE_DURATION_CAP_EXCEEDED";
const STROKE_DETAIL_SPEED_INVALID: &str = "STROKE_SPEED_INVALID";
const STROKE_DETAIL_PATH_PARAMETER_INVALID: &str = "STROKE_PATH_PARAMETER_INVALID";
const STROKE_DETAIL_VELOCITY_INVALID: &str = "STROKE_VELOCITY_INVALID";
const STROKE_DETAIL_HUMANIZE_INVALID: &str = "STROKE_HUMANIZE_INVALID";
const STROKE_DETAIL_MOTION_MODEL_INVALID: &str = "STROKE_MOTION_MODEL_INVALID";
const STROKE_DETAIL_TARGET_MISSING: &str = "STROKE_TARGET_MISSING";
const STROKE_DETAIL_TARGET_CONFLICT: &str = "STROKE_TARGET_CONFLICT";
const STROKE_DETAIL_TARGET_UNRESOLVED: &str = "STROKE_TARGET_UNRESOLVED";
#[cfg(windows)]
const CDP_STROKE_MIN_DISPATCH_INTERVAL_MS: f64 = 8.0;
#[cfg(windows)]
const CDP_STROKE_MAX_DISPATCH_POINTS: usize = 256;
#[cfg(windows)]
const CDP_STROKE_ROUTE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActStrokeParams {
    #[serde(default)]
    #[schemars(default)]
    pub path: Option<PathSpec>,
    #[serde(default)]
    #[schemars(default)]
    pub target: Option<ActStrokeTarget>,
    #[serde(default)]
    #[schemars(default)]
    pub from: Option<ActStrokeTarget>,
    #[serde(default)]
    #[schemars(default)]
    pub to: Option<ActStrokeTarget>,
    #[serde(default)]
    #[schemars(default)]
    pub button: Option<MouseButton>,
    #[serde(default = "default_stroke_velocity_profile")]
    #[schemars(default = "default_stroke_velocity_profile")]
    pub velocity_profile: VelocityProfile,
    pub duration_or_speed: StrokeTiming,
    #[serde(default = "default_stroke_motion_model")]
    #[schemars(default = "default_stroke_motion_model")]
    pub motion_model: StrokeMotionModel,
    #[serde(default)]
    #[schemars(default)]
    pub humanize: Option<HumanizeParams>,
    #[serde(default = "default_stroke_backend")]
    #[schemars(default = "default_stroke_backend")]
    pub backend: StrokeBackend,
    #[serde(default)]
    #[schemars(default)]
    pub modifiers: Vec<StrokeModifier>,
    #[serde(default)]
    #[schemars(default)]
    pub verify_delta: bool,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(untagged)]
pub enum ActStrokeTarget {
    Point(ActStrokePointTarget),
    Element(ActStrokeElementTarget),
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActStrokePointTarget {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActStrokeElementTarget {
    pub element_id: ElementId,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StrokeBackend {
    Software,
    Hardware,
    Auto,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StrokeModifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActStrokeResponse {
    pub ok: bool,
    pub path_kind: String,
    pub control_point_count: u32,
    pub button_used: Option<MouseButton>,
    pub velocity_profile_used: VelocityProfile,
    pub duration_or_speed_used: StrokeTiming,
    pub motion_model_used: StrokeMotionModel,
    pub humanized: bool,
    pub point_stream_count: u32,
    pub path_length_px: f64,
    pub duration_ms: f64,
    pub modifiers_used: Vec<StrokeModifier>,
    pub backend_used: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub elapsed_ms: u32,
    pub postcondition: ActPostcondition,
}

#[derive(Clone, Debug)]
pub struct ActStrokePlan {
    input_kind: ActStrokeInputKind,
    path: Option<PathSpec>,
    plan: Option<StrokePlan>,
    cdp_aim: Option<CdpAimTarget>,
}

impl ActStrokePlan {
    pub(crate) const fn requires_input_lease(&self) -> bool {
        !matches!(self.input_kind, ActStrokeInputKind::CdpElementAim)
    }

    pub(crate) const fn can_try_cdp_target_stroke(&self) -> bool {
        matches!(self.input_kind, ActStrokeInputKind::Path)
            && self.path.is_some()
            && self.plan.is_some()
    }

    pub(crate) const fn is_cdp_element_aim(&self) -> bool {
        matches!(self.input_kind, ActStrokeInputKind::CdpElementAim)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ActStrokeInputKind {
    Path,
    TargetLine,
    CdpElementAim,
}

#[derive(Clone, Debug)]
struct CdpAimTarget {
    element_id: ElementId,
    backend_node_id: i64,
}

pub(crate) async fn act_stroke_with_handle_and_boundary(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActStrokeParams,
    plan: ActStrokePlan,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActStrokeResponse, ErrorData> {
    let started = Instant::now();
    if let Some(cdp_aim) = &plan.cdp_aim {
        return execute_cdp_aim(&params, cdp_aim, started, boundary).await;
    }
    let path = plan.path.clone().ok_or_else(|| {
        params_invalid_detail(
            STROKE_DETAIL_TARGET_UNRESOLVED,
            "act_stroke internal error: validated stroke had no executable path",
        )
    })?;
    let stroke_plan = plan.plan.clone().ok_or_else(|| {
        params_invalid_detail(
            STROKE_DETAIL_TARGET_UNRESOLVED,
            "act_stroke internal error: validated stroke had no sample plan",
        )
    })?;
    let backend = params.backend.to_backend();
    let action = Action::MouseStroke {
        path: path.clone(),
        button: params.button,
        profile: params.velocity_profile,
        timing: params.duration_or_speed.clone(),
        motion_model: params.motion_model,
        humanize: params.humanize,
        backend,
    };
    let modifier_keys: Vec<_> = params
        .modifiers
        .iter()
        .map(|modifier| modifier.to_key())
        .collect();

    if let Some(recording) = recording {
        execute_recording(&recording, &modifier_keys, &action, backend, boundary)?;
    } else {
        execute_with_modifiers(&handle, &modifier_keys, action, backend, boundary).await?;
    }

    Ok(response(&params, &path, &stroke_plan, started, backend))
}

pub(crate) async fn act_stroke_cdp_target(
    endpoint: &str,
    cdp_target_id: &str,
    params: ActStrokeParams,
    plan: ActStrokePlan,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActStrokeResponse, ErrorData> {
    #[cfg(windows)]
    {
        let started = Instant::now();
        if params.requests_hardware_backend() {
            return Err(params_invalid_detail(
                STROKE_DETAIL_TARGET_UNRESOLVED,
                "act_stroke CDP background route is unavailable when backend=hardware requests the real cursor backend",
            ));
        }
        if !params.modifiers.is_empty() {
            return Err(params_invalid_detail(
                STROKE_DETAIL_TARGET_UNRESOLVED,
                "act_stroke CDP background route does not support stroke modifiers; refusing to ignore them",
            ));
        }
        if params.verify_delta {
            return Err(params_invalid_detail(
                STROKE_DETAIL_TARGET_UNRESOLVED,
                "act_stroke verify_delta is not available for CDP mouse strokes because the expected DOM/canvas delta is app-specific; read the target DOM SoT separately after the tool call",
            ));
        }
        let path = plan.path.clone().ok_or_else(|| {
            params_invalid_detail(
                STROKE_DETAIL_TARGET_UNRESOLVED,
                "act_stroke CDP background route requires an explicit path",
            )
        })?;
        let stroke_plan = plan.plan.clone().ok_or_else(|| {
            params_invalid_detail(
                STROKE_DETAIL_TARGET_UNRESOLVED,
                "act_stroke CDP background route requires a sampled path plan",
            )
        })?;
        let points = cdp_dispatch_points_from_stroke_plan(&stroke_plan);
        let button = cdp_mouse_button(params.button)?;
        boundary.ensure("immediately_before_cdp_mouse_stroke_target")?;
        let dispatch = tokio::time::timeout(
            CDP_STROKE_ROUTE_TIMEOUT,
            synapse_a11y::cdp_mouse_stroke_target(endpoint, cdp_target_id, points, button),
        )
        .await
        .map_err(|_| {
            mcp_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!(
                    "act_stroke CDP Input.dispatchMouseEvent route timed out after {} ms for target {cdp_target_id:?}",
                    CDP_STROKE_ROUTE_TIMEOUT.as_millis()
                ),
            )
        })?
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "act_stroke CDP Input.dispatchMouseEvent failed for target {cdp_target_id:?}: {error}"
                ),
            )
        })?;
        boundary.ensure("after_cdp_mouse_stroke_target")?;
        tracing::info!(
            code = "M2_ACT_STROKE_CDP_TARGET_DISPATCHED",
            cdp_target_id = %dispatch.target_id,
            point_stream_count = dispatch.point_count,
            start_x = dispatch.start.x,
            start_y = dispatch.start.y,
            end_x = dispatch.end.x,
            end_y = dispatch.end.y,
            duration_ms = dispatch.duration_ms,
            button = ?params.button,
            planned_point_stream_count = stroke_plan.samples.len(),
            "readback=cdp_dispatch tool=act_stroke method=Input.dispatchMouseEvent"
        );
        Ok(cdp_target_response(
            &params,
            &path,
            &stroke_plan,
            dispatch.point_count,
            started,
        ))
    }

    #[cfg(not(windows))]
    {
        let _ = (endpoint, cdp_target_id, params, plan);
        Err(action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: "act_stroke CDP background route requires Windows CDP action support"
                .to_owned(),
        }))
    }
}

pub fn validate_act_stroke_params(params: &ActStrokeParams) -> Result<ActStrokePlan, ErrorData> {
    validate_and_plan(params)
}

pub fn act_stroke_request_details(params: &ActStrokeParams, plan: &ActStrokePlan) -> Value {
    let resolved_path = plan.path.as_ref();
    let planned = plan.plan.as_ref();
    json!({
        "path_id": act_stroke_path_id(params, plan),
        "input_kind": plan.input_kind.as_str(),
        "path_kind": resolved_path.map_or("cdp_element", path_kind),
        "control_point_count": resolved_path.map_or(1, control_point_count),
        "target": &params.target,
        "from": &params.from,
        "to": &params.to,
        "button": params.button,
        "velocity_profile": params.velocity_profile,
        "duration_or_speed": &params.duration_or_speed,
        "motion_model": params.motion_model,
        "humanized": params.humanize.is_some(),
        "humanize": params.humanize,
        "backend_requested": params.backend,
        "backend_resolved": backend_used_name(params.backend.to_backend()),
        "modifiers": &params.modifiers,
        "plan": {
            "point_stream_count": planned.map_or(1, |plan| plan.samples.len()),
            "path_length_px": planned.map_or(0.0, |plan| plan.path_length_px),
            "duration_ms": planned.map_or(0.0, |plan| plan.duration_ms),
            "first_sample": planned.and_then(|plan| plan.samples.first().map(stroke_sample_details)),
            "last_sample": planned.and_then(|plan| plan.samples.last().map(stroke_sample_details)),
        },
        "fallback_path_executed": false,
    })
}

pub fn act_stroke_validation_failure_details(params: &ActStrokeParams, error: &ErrorData) -> Value {
    json!({
        "stroke": {
            "validation_stage": "params",
            "validated": false,
            "input_kind": act_stroke_input_summary(params),
            "path_kind": params.path.as_ref().map(path_kind),
            "control_point_count": params.path.as_ref().map(control_point_count),
            "target_present": params.target.is_some(),
            "from_present": params.from.is_some(),
            "to_present": params.to.is_some(),
            "button": params.button,
            "velocity_profile": params.velocity_profile,
            "duration_or_speed": &params.duration_or_speed,
            "motion_model": params.motion_model,
            "humanized": params.humanize.is_some(),
            "backend_requested": params.backend,
            "backend_resolved": backend_used_name(params.backend.to_backend()),
            "modifiers": &params.modifiers,
            "fallback_path_executed": false,
        },
        "preflight": Value::Null,
        "failure": act_stroke_error_details(error),
    })
}

pub fn act_stroke_error_details(error: &ErrorData) -> Value {
    let data = error.data.as_ref();
    json!({
        "code": data
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str),
        "message": error.message.to_string(),
        "data": error.data.clone(),
        "point_index": data
            .and_then(|data| data.get("point_index"))
            .cloned()
            .unwrap_or(Value::Null),
        "queue_rate_state": data
            .and_then(|data| data.get("queue_rate_state"))
            .cloned()
            .unwrap_or_else(|| json!({ "kind": "not_rate_or_queue" })),
        "fallback_path_executed": false,
    })
}

fn act_stroke_input_summary(params: &ActStrokeParams) -> &'static str {
    if params.path.is_some() {
        "path"
    } else if params.target.is_some() || params.to.is_some() {
        "target_line"
    } else {
        "missing"
    }
}

impl StrokeBackend {
    const fn to_backend(self) -> Backend {
        match self {
            Self::Software => Backend::Software,
            Self::Hardware => Backend::Hardware,
            Self::Auto => Backend::Auto,
        }
    }
}

impl ActStrokeParams {
    pub(crate) const fn requests_hardware_backend(&self) -> bool {
        matches!(self.backend, StrokeBackend::Hardware)
    }
}

impl StrokeModifier {
    fn to_key(self) -> synapse_core::Key {
        let value = match self {
            Self::Ctrl => "ctrl",
            Self::Shift => "shift",
            Self::Alt => "alt",
            Self::Super => "super",
        };
        synapse_core::Key {
            code: synapse_core::KeyCode::Named {
                value: value.to_owned(),
            },
            use_scancode: false,
        }
    }
}

impl ActStrokeInputKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::TargetLine => "target_line",
            Self::CdpElementAim => "cdp_element_aim",
        }
    }
}

fn validate_and_plan(params: &ActStrokeParams) -> Result<ActStrokePlan, ErrorData> {
    validate_and_plan_with_screen_bounds(params, current_virtual_screen_bounds()?)
}

fn validate_and_plan_with_screen_bounds(
    params: &ActStrokeParams,
    screen_bounds: Option<StrokeScreenBounds>,
) -> Result<ActStrokePlan, ErrorData> {
    let resolved = resolve_stroke_execution(params)?;
    let (path, input_kind) = match resolved {
        ResolvedStrokeExecution::Path { path, input_kind } => (path, input_kind),
        ResolvedStrokeExecution::CdpAim(cdp_aim) => {
            return Ok(ActStrokePlan {
                input_kind: ActStrokeInputKind::CdpElementAim,
                path: None,
                plan: None,
                cdp_aim: Some(cdp_aim),
            });
        }
    };
    validate_control_point_cap(&path)?;
    validate_path_points(&path, screen_bounds)?;
    validate_duration_cap(&path, &params.duration_or_speed)?;
    let plan = plan_timed_stroke(
        &path,
        params.velocity_profile,
        &params.duration_or_speed,
        params.motion_model,
        params.humanize,
    )
    .map_err(|error| stroke_error_to_mcp(&error))?;
    if plan.samples.len() > MAX_STROKE_SAMPLES {
        return Err(params_invalid_detail(
            STROKE_DETAIL_SAMPLE_CAP_EXCEEDED,
            format!(
                "act_stroke planned point stream count {} exceeds max {MAX_STROKE_SAMPLES}",
                plan.samples.len()
            ),
        ));
    }
    validate_plan_points(&plan, screen_bounds)?;
    Ok(ActStrokePlan {
        input_kind,
        path: Some(path),
        plan: Some(plan),
        cdp_aim: None,
    })
}

enum ResolvedStrokeExecution {
    Path {
        path: PathSpec,
        input_kind: ActStrokeInputKind,
    },
    CdpAim(CdpAimTarget),
}

fn resolve_stroke_execution(
    params: &ActStrokeParams,
) -> Result<ResolvedStrokeExecution, ErrorData> {
    if let Some(path) = &params.path {
        if params.from.is_some() || params.to.is_some() || params.target.is_some() {
            return Err(params_invalid_detail(
                STROKE_DETAIL_TARGET_CONFLICT,
                "act_stroke path requests must not also set from, to, or target",
            ));
        }
        return Ok(ResolvedStrokeExecution::Path {
            path: path.clone(),
            input_kind: ActStrokeInputKind::Path,
        });
    }

    let to = match (&params.to, &params.target) {
        (Some(_), Some(_)) => {
            return Err(params_invalid_detail(
                STROKE_DETAIL_TARGET_CONFLICT,
                "act_stroke accepts either to or target, not both",
            ));
        }
        (Some(to), None) | (None, Some(to)) => to,
        (None, None) => {
            return Err(params_invalid_detail(
                STROKE_DETAIL_TARGET_MISSING,
                "act_stroke requires path, to, or target",
            ));
        }
    };

    if params.button.is_none()
        && params.from.is_none()
        && let ActStrokeTarget::Element(element) = to
        && let Some(backend_node_id) =
            synapse_a11y::cdp_backend_from_element_id(&element.element_id)
    {
        return Ok(ResolvedStrokeExecution::CdpAim(CdpAimTarget {
            element_id: element.element_id.clone(),
            backend_node_id,
        }));
    }

    let from = match &params.from {
        Some(from) => target_to_path_point(from, "from")?,
        None => current_cursor_path_point()?,
    };
    let to = target_to_path_point(to, "to")?;
    Ok(ResolvedStrokeExecution::Path {
        path: PathSpec::Line { from, to },
        input_kind: ActStrokeInputKind::TargetLine,
    })
}

fn target_to_path_point(
    target: &ActStrokeTarget,
    role: &'static str,
) -> Result<PathPoint, ErrorData> {
    match target {
        ActStrokeTarget::Point(point) => Ok(PathPoint::new(point.x, point.y)),
        ActStrokeTarget::Element(element) => {
            if synapse_a11y::cdp_backend_from_element_id(&element.element_id).is_some() {
                return Err(params_invalid_detail(
                    STROKE_DETAIL_TARGET_UNRESOLVED,
                    format!(
                        "act_stroke {role} CDP element {} can only be used as a pointer aim target with no button and no from point",
                        element.element_id
                    ),
                ));
            }
            let center = element_center(&element.element_id, role)?;
            Ok(PathPoint::new(f64::from(center.x), f64::from(center.y)))
        }
    }
}

fn current_cursor_path_point() -> Result<PathPoint, ErrorData> {
    let point = synapse_action::backend::software::cursor_position()
        .map_err(|error| action_error_to_mcp(&error))?;
    Ok(PathPoint::new(f64::from(point.x), f64::from(point.y)))
}

#[cfg(windows)]
fn element_center(element_id: &ElementId, role: &'static str) -> Result<Point, ErrorData> {
    let rect = if let Some(rect) = browser_ocr_rect_or_error(element_id, role)? {
        rect
    } else {
        synapse_a11y::element_bounding_rect(element_id).map_err(|err| {
            action_error_to_mcp(&ActionError::ElementNotResolved {
                detail: format!(
                    "act_stroke {role} element {element_id} could not be resolved: {err}"
                ),
            })
        })?
    };
    center_from_rect(rect).map_err(|error| action_error_to_mcp(&error))
}

#[cfg(windows)]
fn browser_ocr_rect_or_error(
    element_id: &ElementId,
    role: &'static str,
) -> Result<Option<Rect>, ErrorData> {
    match crate::m1::browser_ocr_rect_from_element_id(element_id) {
        Some(rect) => Ok(Some(rect)),
        None if crate::m1::is_browser_ocr_element_id(element_id) => {
            Err(action_error_to_mcp(&ActionError::TargetInvalid {
                detail: format!(
                    "act_stroke {role} browser OCR element {element_id} does not contain a valid non-empty bbox"
                ),
            }))
        }
        None => Ok(None),
    }
}

#[cfg(not(windows))]
fn element_center(element_id: &ElementId, role: &'static str) -> Result<Point, ErrorData> {
    Err(action_error_to_mcp(&ActionError::BackendUnavailable {
        detail: format!(
            "act_stroke {role} element target {element_id} requires Windows UI Automation bbox resolution"
        ),
    }))
}

fn center_from_rect(rect: Rect) -> Result<Point, ActionError> {
    if rect.w <= 0 || rect.h <= 0 {
        return Err(ActionError::TargetInvalid {
            detail: format!("act_stroke element bbox is empty or inverted: {rect:?}"),
        });
    }

    let x = i64::from(rect.x) + i64::from(rect.w) / 2;
    let y = i64::from(rect.y) + i64::from(rect.h) / 2;

    Ok(Point {
        x: i32::try_from(x).map_err(|err| ActionError::TargetInvalid {
            detail: format!("act_stroke element bbox center x overflowed i32: {err}"),
        })?,
        y: i32::try_from(y).map_err(|err| ActionError::TargetInvalid {
            detail: format!("act_stroke element bbox center y overflowed i32: {err}"),
        })?,
    })
}

fn validate_control_point_cap(path: &PathSpec) -> Result<(), ErrorData> {
    let count = control_point_count(path);
    if count > MAX_STROKE_PATH_POINTS {
        return Err(params_invalid_detail(
            STROKE_DETAIL_PATH_POINT_CAP_EXCEEDED,
            format!(
                "act_stroke path control point count {count} exceeds max {MAX_STROKE_PATH_POINTS}"
            ),
        ));
    }
    Ok(())
}

fn validate_duration_cap(path: &PathSpec, timing: &StrokeTiming) -> Result<(), ErrorData> {
    let path_length_px = ArcLengthPath::new(path)
        .map_err(|error| path_error_to_mcp(&error))?
        .length();
    let duration_ms = match timing {
        StrokeTiming::DurationMs { duration_ms } => f64::from(*duration_ms),
        StrokeTiming::SpeedPxPerSec { px_per_sec } => {
            if !px_per_sec.is_finite() || *px_per_sec <= 0.0 {
                return Err(params_invalid_detail(
                    STROKE_DETAIL_SPEED_INVALID,
                    format!(
                        "act_stroke speed px_per_sec must be finite and greater than zero, got {px_per_sec}"
                    ),
                ));
            }
            path_length_px / px_per_sec * 1000.0
        }
    };
    if !duration_ms.is_finite() || duration_ms <= 0.0 {
        return Err(params_invalid_detail(
            STROKE_DETAIL_DURATION_INVALID,
            format!(
                "act_stroke duration_ms must be finite and greater than zero, got {duration_ms}"
            ),
        ));
    }
    if duration_ms > MAX_STROKE_DURATION_MS {
        return Err(params_invalid_detail(
            STROKE_DETAIL_DURATION_CAP_EXCEEDED,
            format!(
                "act_stroke planned duration_ms {duration_ms:.3} exceeds max {MAX_STROKE_DURATION_MS:.0}"
            ),
        ));
    }
    Ok(())
}

fn validate_path_points(
    path: &PathSpec,
    screen_bounds: Option<StrokeScreenBounds>,
) -> Result<(), ErrorData> {
    match path {
        PathSpec::Line { from, to } => {
            validate_path_point(*from, "path.line.from", screen_bounds)?;
            validate_path_point(*to, "path.line.to", screen_bounds)?;
        }
        PathSpec::Arc { center, .. } => {
            validate_path_point(*center, "path.arc.center", screen_bounds)?;
        }
        PathSpec::Circle { center, .. } => {
            validate_path_point(*center, "path.circle.center", screen_bounds)?;
        }
        PathSpec::CubicBezier { p0, p1, p2, p3 } => {
            validate_path_point(*p0, "path.cubic_bezier.p0", screen_bounds)?;
            validate_path_point(*p1, "path.cubic_bezier.p1", screen_bounds)?;
            validate_path_point(*p2, "path.cubic_bezier.p2", screen_bounds)?;
            validate_path_point(*p3, "path.cubic_bezier.p3", screen_bounds)?;
        }
        PathSpec::Polyline { points, .. } => {
            for (index, point) in points.iter().enumerate() {
                validate_path_point(
                    *point,
                    &format!("path.polyline.points[{index}]"),
                    screen_bounds,
                )?;
            }
        }
        PathSpec::CatmullRom { waypoints, .. } => {
            for (index, point) in waypoints.iter().enumerate() {
                validate_path_point(
                    *point,
                    &format!("path.catmull_rom.waypoints[{index}]"),
                    screen_bounds,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_plan_points(
    plan: &StrokePlan,
    screen_bounds: Option<StrokeScreenBounds>,
) -> Result<(), ErrorData> {
    for (index, sample) in plan.samples.iter().enumerate() {
        validate_path_point(
            sample.point,
            &format!("planned_samples[{index}]"),
            screen_bounds,
        )?;
    }
    Ok(())
}

fn validate_path_point(
    point: PathPoint,
    label: &str,
    screen_bounds: Option<StrokeScreenBounds>,
) -> Result<Point, ErrorData> {
    if !point.is_finite() {
        return Err(params_invalid_detail(
            STROKE_DETAIL_COORD_NONFINITE,
            format!(
                "act_stroke {label} must have finite coordinates, got x={} y={}",
                point.x, point.y
            ),
        ));
    }
    let screen_point = screen_point_from_path_point(point, 0).map_err(|error| match error {
        StrokeError::ScreenPointOutOfRange { x, y, .. } => params_invalid_detail(
            STROKE_DETAIL_COORD_OUT_OF_I32_RANGE,
            format!("act_stroke {label} is outside i32 screen coordinate range: x={x} y={y}"),
        ),
        other => stroke_error_to_mcp(&other),
    })?;
    if let Some(bounds) = screen_bounds
        && !bounds.rect.contains(screen_point)
    {
        let right = bounds.rect.x.saturating_add(bounds.rect.w);
        let bottom = bounds.rect.y.saturating_add(bounds.rect.h);
        return Err(params_invalid_detail(
            STROKE_DETAIL_POINT_OUT_OF_VIRTUAL_SCREEN,
            format!(
                "act_stroke {label} is outside the virtual screen bounds: point=({}, {}) bounds=left:{} top:{} right_exclusive:{} bottom_exclusive:{} source:{}",
                screen_point.x,
                screen_point.y,
                bounds.rect.x,
                bounds.rect.y,
                right,
                bottom,
                bounds.source
            ),
        ));
    }
    Ok(screen_point)
}

#[derive(Copy, Clone, Debug)]
struct StrokeScreenBounds {
    rect: Rect,
    source: &'static str,
}

fn current_virtual_screen_bounds() -> Result<Option<StrokeScreenBounds>, ErrorData> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };

        // SAFETY: GetSystemMetrics is read-only for these process desktop metrics.
        let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        // SAFETY: GetSystemMetrics is read-only for these process desktop metrics.
        let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        // SAFETY: GetSystemMetrics is read-only for these process desktop metrics.
        let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        // SAFETY: GetSystemMetrics is read-only for these process desktop metrics.
        let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        if width <= 0 || height <= 0 {
            return Err(mcp_error(
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                format!(
                    "act_stroke could not read a valid virtual screen before validation: left={left} top={top} width={width} height={height}"
                ),
            ));
        }
        Ok(Some(StrokeScreenBounds {
            rect: Rect {
                x: left,
                y: top,
                w: width,
                h: height,
            },
            source: "GetSystemMetrics(SM_*VIRTUALSCREEN)",
        }))
    }

    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

async fn execute_with_modifiers(
    handle: &ActionHandle,
    modifier_keys: &[synapse_core::Key],
    stroke_action: Action,
    backend: Backend,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let mut pressed = Vec::with_capacity(modifier_keys.len());
    for key in modifier_keys {
        if let Err(error) = boundary.ensure("immediately_before_stroke_modifier_key_down") {
            let _release_result =
                release_pressed_modifiers(handle, &pressed, backend, "operator_panic_cleanup")
                    .await;
            return Err(error);
        }
        if let Err(error) = handle
            .execute(Action::KeyDown {
                key: key.clone(),
                backend,
            })
            .await
        {
            let _release_result =
                release_pressed_modifiers(handle, &pressed, backend, "modifier_press_cleanup")
                    .await;
            return Err(action_error_to_mcp(&error));
        }
        pressed.push(key.clone());
    }

    if let Err(error) = boundary.ensure("immediately_before_foreground_stroke_dispatch") {
        let _release_result =
            release_pressed_modifiers(handle, &pressed, backend, "operator_panic_cleanup").await;
        return Err(error);
    }
    let stroke_result = handle.execute(stroke_action).await;
    let boundary_error = boundary
        .ensure("after_foreground_stroke_before_modifier_release")
        .err();
    if stroke_result.is_ok() && !pressed.is_empty() {
        tokio::time::sleep(Duration::from_millis(MODIFIER_RELEASE_SETTLE_MS)).await;
    }
    let release_stage = if stroke_result.is_err() {
        "stroke_error_cleanup"
    } else {
        "post_stroke_release"
    };
    let release_result = release_pressed_modifiers(handle, &pressed, backend, release_stage).await;

    if let Err(error) = stroke_result {
        return Err(action_error_to_mcp(&error));
    }
    if let Some(error) = boundary_error {
        if let Err(release_error) = &release_result {
            tracing::error!(
                code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                detail_code = "STROKE_MODIFIER_RELEASE_AFTER_OPERATOR_PANIC_FAILED",
                detail = %release_error,
                "operator panic superseded a stroke and best-effort modifier cleanup failed"
            );
        }
        return Err(error);
    }
    if let Err(error) = release_result {
        return Err(action_error_to_mcp(&error));
    }
    Ok(())
}

async fn execute_cdp_aim(
    params: &ActStrokeParams,
    cdp_aim: &CdpAimTarget,
    started: Instant,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActStrokeResponse, ErrorData> {
    #[cfg(windows)]
    {
        let hwnd = cdp_aim
            .element_id
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
                    "no reachable CDP endpoint for web element {} (browser closed or debug port gone)",
                    cdp_aim.element_id
                ),
            )
        })?;
        let title_hint = synapse_a11y::foreground_context(hwnd)
            .map(|context| context.window_title)
            .unwrap_or_default();
        let target_id_hint = synapse_a11y::cdp_target_from_element_id(&cdp_aim.element_id);
        boundary.ensure("immediately_before_cdp_aim_node")?;
        let landed = synapse_a11y::cdp_aim_node(
            &endpoint,
            &title_hint,
            target_id_hint.as_deref(),
            cdp_aim.backend_node_id,
        )
        .await
        .map_err(|err| mcp_error(err.code(), err.to_string()))?;
        boundary.ensure("after_cdp_aim_node")?;
        tracing::info!(
            code = "M2_ACT_STROKE_CDP_AIM_MOVED",
            element_id = %cdp_aim.element_id,
            x = landed.x,
            y = landed.y,
            "readback=act_stroke element method=cdp_mouse_moved"
        );
        return Ok(cdp_aim_response(params, started));
    }

    #[cfg(not(windows))]
    {
        let _ = (params, cdp_aim, started);
        Err(action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: "act_stroke CDP element aim requires Windows CDP action support".to_owned(),
        }))
    }
}

async fn release_pressed_modifiers(
    handle: &ActionHandle,
    pressed: &[synapse_core::Key],
    backend: Backend,
    stage: &'static str,
) -> Result<(), ActionError> {
    let mut release_error = None;
    for (release_index, key) in pressed.iter().rev().enumerate() {
        if let Err(error) = handle
            .execute(Action::KeyUp {
                key: key.clone(),
                backend,
            })
            .await
        {
            log_modifier_release_error(stage, release_index, key, backend, &error);
            if release_error.is_none() {
                release_error = Some(error);
            }
        }
    }
    release_error.map_or(Ok(()), Err)
}

fn log_modifier_release_error(
    stage: &'static str,
    release_index: usize,
    key: &synapse_core::Key,
    backend: Backend,
    error: &ActionError,
) {
    let key = format!("{key:?}");
    tracing::error!(
        code = "M2_ACT_STROKE_MODIFIER_RELEASE_FAILED",
        failure_stage = stage,
        release_index,
        modifier_key = %key,
        backend = backend_used_name(backend),
        error_code = error.code(),
        detail = error.detail(),
        retry_after_ms = error.retry_after_ms(),
        queue_rate_state = %queue_rate_state(error),
        fallback_path_executed = false,
        action_kind = "act_stroke",
        "act_stroke modifier release failed without fallback"
    );
}

fn execute_recording(
    recording: &RecordingBackend,
    modifier_keys: &[synapse_core::Key],
    stroke_action: &Action,
    backend: Backend,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let before_events = recording.events();
    let before_event_count = before_events.len();
    let mut emit_state = EmitState::new();
    for key in modifier_keys {
        boundary.ensure("immediately_before_recorded_stroke_modifier_key_down")?;
        recording
            .execute(
                &Action::KeyDown {
                    key: key.clone(),
                    backend,
                },
                &mut emit_state,
            )
            .map_err(|error| action_error_to_mcp(&error))?;
    }
    boundary.ensure("immediately_before_recorded_stroke_dispatch")?;
    recording
        .execute(stroke_action, &mut emit_state)
        .map_err(|error| action_error_to_mcp(&error))?;
    for key in modifier_keys.iter().rev() {
        recording
            .execute(
                &Action::KeyUp {
                    key: key.clone(),
                    backend,
                },
                &mut emit_state,
            )
            .map_err(|error| action_error_to_mcp(&error))?;
    }
    let after_events = recording.events();
    let new_events = &after_events[before_event_count..];
    let event_sequence = event_sequence(new_events);
    tracing::info!(
        code = "M2_ACT_STROKE_RECORDING_READBACK",
        kind = "act_stroke",
        before_event_count,
        after_event_count = after_events.len(),
        new_event_count = new_events.len(),
        event_sequence,
        ?new_events,
        "readback=recording_backend tool=act_stroke after_events_readback"
    );
    Ok(())
}

fn response(
    params: &ActStrokeParams,
    path: &PathSpec,
    stroke_plan: &StrokePlan,
    started: Instant,
    backend: Backend,
) -> ActStrokeResponse {
    ActStrokeResponse {
        ok: true,
        path_kind: path_kind(path).to_owned(),
        control_point_count: u32::try_from(control_point_count(path)).unwrap_or(u32::MAX),
        button_used: params.button,
        velocity_profile_used: params.velocity_profile,
        duration_or_speed_used: params.duration_or_speed.clone(),
        motion_model_used: params.motion_model,
        humanized: params.humanize.is_some(),
        point_stream_count: u32::try_from(stroke_plan.samples.len()).unwrap_or(u32::MAX),
        path_length_px: stroke_plan.path_length_px,
        duration_ms: stroke_plan.duration_ms,
        modifiers_used: params.modifiers.clone(),
        backend_used: backend_used_name(backend).to_owned(),
        backend_tier_used: "foreground".to_owned(),
        required_foreground: true,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        postcondition: postcondition_not_requested("act_stroke", "cursor_foreground_ui_or_pixels"),
    }
}

fn cdp_aim_response(params: &ActStrokeParams, started: Instant) -> ActStrokeResponse {
    ActStrokeResponse {
        ok: true,
        path_kind: "cdp_element".to_owned(),
        control_point_count: 1,
        button_used: None,
        velocity_profile_used: params.velocity_profile,
        duration_or_speed_used: params.duration_or_speed.clone(),
        motion_model_used: params.motion_model,
        humanized: params.humanize.is_some(),
        point_stream_count: 1,
        path_length_px: 0.0,
        duration_ms: 0.0,
        modifiers_used: params.modifiers.clone(),
        backend_used: "cdp".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        postcondition: postcondition_not_requested("act_stroke", "cdp_pointer_or_foreground_ui"),
    }
}

fn cdp_target_response(
    params: &ActStrokeParams,
    path: &PathSpec,
    stroke_plan: &StrokePlan,
    dispatched_point_count: usize,
    started: Instant,
) -> ActStrokeResponse {
    ActStrokeResponse {
        ok: true,
        path_kind: path_kind(path).to_owned(),
        control_point_count: u32::try_from(control_point_count(path)).unwrap_or(u32::MAX),
        button_used: params.button,
        velocity_profile_used: params.velocity_profile,
        duration_or_speed_used: params.duration_or_speed.clone(),
        motion_model_used: params.motion_model,
        humanized: params.humanize.is_some(),
        point_stream_count: u32::try_from(dispatched_point_count).unwrap_or(u32::MAX),
        path_length_px: stroke_plan.path_length_px,
        duration_ms: stroke_plan.duration_ms,
        modifiers_used: params.modifiers.clone(),
        backend_used: "cdp".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        postcondition: postcondition_not_requested("act_stroke", "cdp_target_mouse_events"),
    }
}

#[cfg(windows)]
fn cdp_dispatch_points_from_stroke_plan(
    stroke_plan: &StrokePlan,
) -> Vec<synapse_a11y::CdpMouseStrokePoint> {
    let samples = &stroke_plan.samples;
    if samples.is_empty() {
        return Vec::new();
    }
    if samples.len() <= 2 {
        return samples
            .iter()
            .map(cdp_point_from_sample)
            .collect::<Vec<_>>();
    }

    let duration_ms = samples
        .last()
        .map_or(0.0, |sample| sample.elapsed_ms.max(0.0));
    let cap_interval_ms = duration_ms / (CDP_STROKE_MAX_DISPATCH_POINTS.saturating_sub(1) as f64);
    let min_interval_ms = CDP_STROKE_MIN_DISPATCH_INTERVAL_MS.max(cap_interval_ms);
    let mut points = Vec::with_capacity(samples.len().min(CDP_STROKE_MAX_DISPATCH_POINTS));
    let first = cdp_point_from_sample(&samples[0]);
    points.push(first);
    let mut last_kept_elapsed_ms = first.elapsed_ms;

    for sample in samples.iter().skip(1).take(samples.len().saturating_sub(2)) {
        if sample.elapsed_ms - last_kept_elapsed_ms >= min_interval_ms {
            points.push(cdp_point_from_sample(sample));
            last_kept_elapsed_ms = sample.elapsed_ms;
        }
    }

    if let Some(last) = samples.last() {
        points.push(cdp_point_from_sample(last));
    }
    points
}

#[cfg(windows)]
fn cdp_point_from_sample(
    sample: &synapse_action::TimedPathPoint,
) -> synapse_a11y::CdpMouseStrokePoint {
    synapse_a11y::CdpMouseStrokePoint {
        x: sample.point.x,
        y: sample.point.y,
        elapsed_ms: sample.elapsed_ms,
    }
}

#[cfg(windows)]
fn cdp_mouse_button(
    button: Option<MouseButton>,
) -> Result<Option<synapse_a11y::CdpMouseButton>, ErrorData> {
    match button {
        None => Ok(None),
        Some(MouseButton::Left) => Ok(Some(synapse_a11y::CdpMouseButton::Left)),
        Some(MouseButton::Right) => Ok(Some(synapse_a11y::CdpMouseButton::Right)),
        Some(MouseButton::Middle) => Ok(Some(synapse_a11y::CdpMouseButton::Middle)),
        Some(other) => Err(params_invalid_detail(
            STROKE_DETAIL_TARGET_UNRESOLVED,
            format!("act_stroke CDP mouse stroke does not support button {other:?}"),
        )),
    }
}

fn event_sequence(events: &[RecordedInput]) -> String {
    events.iter().map(event_label).collect::<Vec<_>>().join(">")
}

fn event_label(event: &RecordedInput) -> String {
    match event {
        RecordedInput::KeyDown { key } => format!("key_down:{}", key_label(key)),
        RecordedInput::KeyUp { key } => format!("key_up:{}", key_label(key)),
        RecordedInput::MouseButtonDown { button } => format!("down:{}", button_label(*button)),
        RecordedInput::MouseButtonUp { button } => format!("up:{}", button_label(*button)),
        RecordedInput::MouseStrokePoint { elapsed_ms, point } => {
            format!(
                "stroke_point:{elapsed_ms:.3}:screen({},{})",
                point.x, point.y
            )
        }
        other => format!("{other:?}"),
    }
}

fn key_label(key: &synapse_core::Key) -> String {
    match &key.code {
        synapse_core::KeyCode::Named { value } => value.clone(),
        synapse_core::KeyCode::Symbol { value } => value.to_string(),
        synapse_core::KeyCode::HidCode { value } => format!("hid:{value}"),
    }
}

const fn button_label(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
        MouseButton::X1 => "x1",
        MouseButton::X2 => "x2",
    }
}

fn control_point_count(path: &PathSpec) -> usize {
    match path {
        PathSpec::Line { .. } => 2,
        PathSpec::Arc { .. } | PathSpec::Circle { .. } => 1,
        PathSpec::CubicBezier { .. } => 4,
        PathSpec::Polyline { points, .. } => points.len(),
        PathSpec::CatmullRom { waypoints, .. } => waypoints.len(),
    }
}

fn path_kind(path: &PathSpec) -> &'static str {
    match path {
        PathSpec::Line { .. } => "line",
        PathSpec::Arc { .. } => "arc",
        PathSpec::Circle { .. } => "circle",
        PathSpec::CubicBezier { .. } => "cubic_bezier",
        PathSpec::Polyline { .. } => "polyline",
        PathSpec::CatmullRom { .. } => "catmull_rom",
    }
}

fn stroke_error_to_mcp(error: &StrokeError) -> ErrorData {
    match error {
        StrokeError::Path(error) => path_error_to_mcp(error),
        StrokeError::Velocity(error) => params_invalid_detail(
            STROKE_DETAIL_VELOCITY_INVALID,
            format!("act_stroke velocity profile invalid: {error}"),
        ),
        StrokeError::Humanize(error) => params_invalid_detail(
            STROKE_DETAIL_HUMANIZE_INVALID,
            format!("act_stroke humanize params invalid: {error}"),
        ),
        StrokeError::InvalidDuration { duration_ms } => params_invalid_detail(
            STROKE_DETAIL_DURATION_INVALID,
            format!(
                "act_stroke duration_ms must be finite and greater than zero, got {duration_ms}"
            ),
        ),
        StrokeError::InvalidSpeed { px_per_sec } => params_invalid_detail(
            STROKE_DETAIL_SPEED_INVALID,
            format!(
                "act_stroke speed px_per_sec must be finite and greater than zero, got {px_per_sec}"
            ),
        ),
        StrokeError::SampleCountOverflow { duration_ms } => params_invalid_detail(
            STROKE_DETAIL_SAMPLE_CAP_EXCEEDED,
            format!("act_stroke sample count overflow for duration_ms={duration_ms}"),
        ),
        StrokeError::ScreenPointOutOfRange { index, x, y } => params_invalid_detail(
            STROKE_DETAIL_COORD_OUT_OF_I32_RANGE,
            format!(
                "act_stroke planned point {index} is outside i32 screen coordinate range: x={x} y={y}"
            ),
        ),
        StrokeError::WindMouseRequiresLine { path_kind } => params_invalid_detail(
            STROKE_DETAIL_MOTION_MODEL_INVALID,
            format!("act_stroke motion_model wind_mouse requires path.kind=line, got {path_kind}"),
        ),
        StrokeError::InvalidWindMouseParameter { field, value } => params_invalid_detail(
            STROKE_DETAIL_MOTION_MODEL_INVALID,
            format!(
                "act_stroke motion_model wind_mouse parameter {field} must be finite and greater than zero, got {value}"
            ),
        ),
        StrokeError::WindMouseNonFinitePoint { index, x, y } => params_invalid_detail(
            STROKE_DETAIL_MOTION_MODEL_INVALID,
            format!(
                "act_stroke motion_model wind_mouse generated non-finite point {index}: x={x} y={y}"
            ),
        ),
        StrokeError::WindMouseDidNotConverge {
            max_points,
            remaining_distance_px,
        } => params_invalid_detail(
            STROKE_DETAIL_SAMPLE_CAP_EXCEEDED,
            format!(
                "act_stroke motion_model wind_mouse did not converge within {max_points} points; remaining distance {remaining_distance_px:.3}px"
            ),
        ),
    }
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    let mut data = json!({
        "code": error.code(),
        "detail": error.detail(),
        "retry_after_ms": error.retry_after_ms(),
        "point_index": extract_sample_index(error.detail()),
        "queue_rate_state": queue_rate_state(error),
    });
    if let ActionError::ForegroundLeaseBusy {
        holder_session_id,
        requesting_session_id,
        retry_after_ms,
        ..
    } = error
    {
        data["holder_session_id"] = json!(holder_session_id);
        data["requesting_session_id"] = json!(requesting_session_id);
        data["retry_after_ms"] = json!(retry_after_ms);
    }
    ErrorData::new(ErrorCode(-32099), error.to_string(), Some(data))
}

fn path_error_to_mcp(error: &PathError) -> ErrorData {
    let detail_code = match error {
        PathError::NotEnoughPoints { .. }
        | PathError::DegenerateSegment { .. }
        | PathError::DegenerateCurve { .. }
        | PathError::ZeroLengthPath
        | PathError::InvalidSampleCount { .. } => STROKE_DETAIL_PATH_DEGENERATE,
        PathError::NonFinitePoint { .. } | PathError::InvalidT { .. } => {
            STROKE_DETAIL_COORD_NONFINITE
        }
        PathError::NonFiniteParameter { .. }
        | PathError::NonPositiveParameter { .. }
        | PathError::InvalidCatmullRomAlpha { .. }
        | PathError::InvalidCatmullRomTension { .. }
        | PathError::InvalidArcLengthSegments { .. }
        | PathError::InvalidArcLength { .. } => STROKE_DETAIL_PATH_PARAMETER_INVALID,
    };
    params_invalid_detail(detail_code, format!("act_stroke path invalid: {error}"))
}

fn params_invalid_detail(detail_code: &'static str, message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": detail_code,
        })),
    )
}

const fn default_stroke_velocity_profile() -> VelocityProfile {
    VelocityProfile::Constant
}

const fn default_stroke_motion_model() -> StrokeMotionModel {
    StrokeMotionModel::Path
}

const fn default_stroke_backend() -> StrokeBackend {
    StrokeBackend::Auto
}

const fn backend_used_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Auto | Backend::Software => "software",
        Backend::Hardware => "hardware",
        Backend::Vigem => "vigem",
    }
}

fn act_stroke_path_id(params: &ActStrokeParams, plan: &ActStrokePlan) -> String {
    let payload = serde_json::to_vec(&json!({
        "input_kind": plan.input_kind.as_str(),
        "path": &plan.path,
        "target": &params.target,
        "from": &params.from,
        "to": &params.to,
        "velocity_profile": params.velocity_profile,
        "duration_or_speed": &params.duration_or_speed,
        "motion_model": params.motion_model,
        "humanize": params.humanize,
        "plan": {
            "point_stream_count": plan.plan.as_ref().map_or(1, |plan| plan.samples.len()),
            "duration_ms": plan.plan.as_ref().map_or(0.0, |plan| plan.duration_ms),
            "path_length_px": plan.plan.as_ref().map_or(0.0, |plan| plan.path_length_px),
        },
    }))
    .unwrap_or_else(|_error| {
        format!(
            "{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}:{:?}",
            plan.input_kind,
            plan.path,
            params.target,
            params.from,
            params.to,
            params.velocity_profile,
            params.duration_or_speed,
            params.motion_model
        )
        .into_bytes()
    });
    format!("stroke:{}", sha256_hex(payload))
}

fn sha256_hex(payload: Vec<u8>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(payload);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn stroke_sample_details(sample: &synapse_action::TimedPathPoint) -> Value {
    json!({
        "elapsed_ms": sample.elapsed_ms,
        "arclen": sample.arclen,
        "point": {
            "x": sample.point.x,
            "y": sample.point.y,
        },
    })
}

fn queue_rate_state(error: &ActionError) -> Value {
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
        ActionError::ForegroundLeaseBusy {
            holder_session_id,
            requesting_session_id,
            retry_after_ms,
            detail,
        } => json!({
            "kind": "foreground_lease_busy",
            "holder_session_id": holder_session_id,
            "requesting_session_id": requesting_session_id,
            "retry_after_ms": retry_after_ms,
            "detail": detail,
        }),
        _ => json!({
            "kind": "not_rate_or_queue",
        }),
    }
}

fn extract_sample_index(detail: &str) -> Option<usize> {
    let marker = "sample_index=";
    let start = detail.find(marker)? + marker.len();
    let digits = detail[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty())
        .then(|| digits.parse::<usize>().ok())
        .flatten()
}
