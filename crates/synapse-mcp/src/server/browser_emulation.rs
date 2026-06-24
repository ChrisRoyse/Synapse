//! Browser emulation tools (#1173/#1174/#1175/#1176/#1177).

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{
        browser_raw_cdp_required_error, cdp_target_id_audit_ref, chrome_debugger_default_endpoint,
        require_target_session_id, validate_cdp_target_id,
    },
    tool, tool_router,
};
use crate::m1::mcp_error;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_core::error_codes;

const RESIZE_TOOL: &str = "browser_resize";
const DEVICE_TOOL: &str = "browser_device";
const GEOLOCATION_TOOL: &str = "browser_geolocation";
const LOCALE_TOOL: &str = "browser_locale";
const MEDIA_TOOL: &str = "browser_media";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
const DEFAULT_BRIDGE_VIEWPORT_WAIT_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserResizeOperation {
    #[default]
    Set,
    Reset,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserDeviceOperation {
    #[default]
    Set,
    Reset,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserGeolocationOperation {
    #[default]
    Set,
    Reset,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserLocaleOperation {
    #[default]
    Set,
    Reset,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserMediaOperation {
    #[default]
    Set,
    Reset,
}






#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserResizeParams {
    /// CDP TargetID to resize. Raw-CDP target IDs and normal Chrome bridge
    /// `chrome-tab:<id>` targets are supported. Defaults to the active session
    /// CDP target. Must be owned by this session; the human foreground tab is
    /// never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Rendered viewport width in CSS pixels for operation=set.
    #[serde(default)]
    pub width: Option<u32>,
    /// Rendered viewport height in CSS pixels for operation=set.
    #[serde(default)]
    pub height: Option<u32>,
    /// Device pixel ratio override for operation=set. Defaults to 1.0.
    #[serde(default)]
    pub device_scale_factor: Option<f64>,
    /// Whether Chromium should apply mobile viewport semantics. Defaults false.
    #[serde(default)]
    pub is_mobile: Option<bool>,
    /// `set` applies a viewport/DPR override; `reset` clears it.
    #[serde(default)]
    pub operation: BrowserResizeOperation,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDeviceParams {
    /// CDP TargetID to emulate. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// `set` applies the descriptor; `reset` clears metrics/touch and restores
    /// the user agent observed before the first set in this process.
    #[serde(default)]
    pub operation: BrowserDeviceOperation,
    /// User agent string for operation=set.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Rendered viewport width in CSS pixels for operation=set.
    #[serde(default)]
    pub width: Option<u32>,
    /// Rendered viewport height in CSS pixels for operation=set.
    #[serde(default)]
    pub height: Option<u32>,
    /// Device pixel ratio override for operation=set. Defaults to 1.0.
    #[serde(default)]
    pub device_scale_factor: Option<f64>,
    /// Whether Chromium should apply mobile viewport semantics. Defaults false.
    #[serde(default)]
    pub is_mobile: Option<bool>,
    /// Whether to enable touch emulation. Defaults false.
    #[serde(default)]
    pub has_touch: Option<bool>,
    /// Maximum emulated touch points. Defaults to 5 when has_touch=true and 0
    /// when has_touch=false.
    #[serde(default)]
    pub max_touch_points: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserGeolocationParams {
    /// CDP TargetID to emulate. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// `set` applies coordinates and an origin-scoped geolocation permission;
    /// `reset` clears the override and restores the permission to prompt.
    #[serde(default)]
    pub operation: BrowserGeolocationOperation,
    /// Latitude in degrees for operation=set.
    #[serde(default)]
    pub latitude: Option<f64>,
    /// Longitude in degrees for operation=set.
    #[serde(default)]
    pub longitude: Option<f64>,
    /// Accuracy in meters for operation=set. Defaults to 0.
    #[serde(default)]
    pub accuracy: Option<f64>,
    /// Optional altitude in meters for operation=set.
    #[serde(default)]
    pub altitude: Option<f64>,
    /// Optional altitude accuracy in meters for operation=set.
    #[serde(default)]
    pub altitude_accuracy: Option<f64>,
    /// Optional heading in degrees for operation=set.
    #[serde(default)]
    pub heading: Option<f64>,
    /// Optional speed in meters per second for operation=set.
    #[serde(default)]
    pub speed: Option<f64>,
    /// When true, grant geolocation to the page origin. When false, deny it so
    /// getCurrentPosition rejects even with coordinates set. Defaults true.
    #[serde(default)]
    pub grant_permission: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocaleParams {
    /// CDP TargetID to emulate. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// `set` applies locale/timezone overrides; `reset` restores host defaults.
    #[serde(default)]
    pub operation: BrowserLocaleOperation,
    /// ICU/BCP-style locale for operation=set, such as `fr_FR` or `fr-FR`.
    #[serde(default)]
    pub locale: Option<String>,
    /// IANA timezone id for operation=set, such as `Europe/Paris`.
    #[serde(default)]
    pub timezone_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserMediaParams {
    /// CDP TargetID to emulate. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// `set` applies media/media-feature overrides; `reset` restores defaults.
    #[serde(default)]
    pub operation: BrowserMediaOperation,
    /// CSS media type for operation=set: `screen` or `print`.
    #[serde(default)]
    pub media: Option<String>,
    /// prefers-color-scheme for operation=set: `light`, `dark`, or
    /// `no-preference`.
    #[serde(default)]
    pub color_scheme: Option<String>,
    /// prefers-reduced-motion for operation=set: `reduce` or `no-preference`.
    #[serde(default)]
    pub reduced_motion: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserViewportOverride {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDeviceDescriptor {
    pub user_agent: String,
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub is_mobile: bool,
    pub has_touch: bool,
    pub max_touch_points: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserGeolocationOverride {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_accuracy: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocaleOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserMediaOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduced_motion: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserViewportReadback {
    pub inner_width: i64,
    pub inner_height: i64,
    pub device_pixel_ratio: f64,
    pub screen_width: i64,
    pub screen_height: i64,
    pub outer_width: i64,
    pub outer_height: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_viewport_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_viewport_height: Option<f64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserResizeResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserResizeOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<BrowserViewportOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub viewport: BrowserViewportReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDeviceReadback {
    pub viewport: BrowserViewportReadback,
    pub user_agent: String,
    pub max_touch_points: i64,
    pub ontouchstart_available: bool,
    pub pointer_coarse: bool,
    pub any_pointer_coarse: bool,
    pub hover_none: bool,
    pub any_hover_none: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserGeolocationCoordinatesReadback {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_accuracy: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    pub timestamp: f64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserGeolocationErrorReadback {
    pub code: i64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserGeolocationReadback {
    pub permission_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<BrowserGeolocationCoordinatesReadback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BrowserGeolocationErrorReadback>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocaleReadback {
    pub locale: String,
    pub calendar: String,
    pub numbering_system: String,
    pub time_zone: String,
    pub sample_number: String,
    pub sample_date: String,
    pub date_string: String,
    pub timezone_offset_minutes: i64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserMediaReadback {
    pub media_screen: bool,
    pub media_print: bool,
    pub color_scheme_dark: bool,
    pub color_scheme_light: bool,
    pub color_scheme_no_preference: bool,
    pub reduced_motion_reduce: bool,
    pub reduced_motion_no_preference: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDeviceResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserDeviceOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<BrowserDeviceDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_user_agent: Option<String>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub device: BrowserDeviceReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserGeolocationResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserGeolocationOperation,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<BrowserGeolocationOverride>,
    pub permission_setting: String,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub geolocation: BrowserGeolocationReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocaleResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserLocaleOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<BrowserLocaleOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub locale: BrowserLocaleReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserMediaResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserMediaOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<BrowserMediaOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub media: BrowserMediaReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, PartialEq)]
struct NormalizedBrowserResizeParams {
    operation: BrowserResizeOperation,
    width: Option<u32>,
    height: Option<u32>,
    device_scale_factor: Option<f64>,
    is_mobile: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
struct NormalizedBrowserDeviceParams {
    operation: BrowserDeviceOperation,
    descriptor: Option<BrowserDeviceDescriptor>,
}

#[derive(Clone, Debug, PartialEq)]
struct NormalizedBrowserGeolocationParams {
    operation: BrowserGeolocationOperation,
    geolocation: Option<BrowserGeolocationOverride>,
    grant_permission: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct NormalizedBrowserLocaleParams {
    operation: BrowserLocaleOperation,
    requested: Option<BrowserLocaleOverride>,
}

#[derive(Clone, Debug, PartialEq)]
struct NormalizedBrowserMediaParams {
    operation: BrowserMediaOperation,
    requested: Option<BrowserMediaOverride>,
}

#[tool_router(router = browser_emulation_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Set or reset the rendered viewport size and device pixel ratio for the calling session's owned browser tab. Raw-CDP targets use Emulation.setDeviceMetricsOverride / Emulation.clearDeviceMetricsOverride plus Runtime.evaluate readback. Normal Chrome bridge chrome-tab:* targets in the already-open authenticated profile use the bundled viewportEmulation bridge lane and page-visible chrome.scripting readback. operation=set requires width, height, and optional device_scale_factor and is_mobile; operation=reset clears the override and reads back restored metrics. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_resize(
        &self,
        params: Parameters<BrowserResizeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserResizeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = RESIZE_TOOL,
            "tool.invocation kind=browser_resize"
        );
        let session_id = require_target_session_id(&request_context)?;
        let resize = validate_browser_resize_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": resize.operation,
            "width": resize.width,
            "height": resize.height,
            "device_scale_factor": resize.device_scale_factor,
            "is_mobile": resize.is_mobile,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            RESIZE_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            RESIZE_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": resize.operation,
            "width": resize.width,
            "height": resize.height,
            "device_scale_factor": resize.device_scale_factor,
            "is_mobile": resize.is_mobile,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            RESIZE_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_resize_impl(&session_id, window_hwnd, &cdp_target_id, &resize)
            .await;
        self.audit_action_result_for_session(RESIZE_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Apply or reset a Playwright-style device descriptor for the calling session's owned browser tab. Raw-CDP targets use Emulation.setUserAgentOverride, Emulation.setDeviceMetricsOverride, Emulation.setTouchEmulationEnabled, and Emulation.setEmitTouchEventsForMouse plus Runtime.evaluate readback. Normal Chrome bridge chrome-tab:* targets in the already-open authenticated profile use the bundled deviceEmulation bridge lane and page-visible chrome.scripting readback. operation=set applies user_agent, width, height, device_scale_factor, is_mobile, has_touch, and max_touch_points; operation=reset clears metrics/touch and restores the user agent observed before the first set on that tab. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_device(
        &self,
        params: Parameters<BrowserDeviceParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserDeviceResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = DEVICE_TOOL,
            "tool.invocation kind=browser_device"
        );
        let session_id = require_target_session_id(&request_context)?;
        let device = validate_browser_device_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": device.operation,
            "descriptor": &device.descriptor,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            DEVICE_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            DEVICE_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": device.operation,
            "descriptor": &device.descriptor,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            DEVICE_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_device_impl(&session_id, window_hwnd, &cdp_target_id, &device)
            .await;
        self.audit_action_result_for_session(DEVICE_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Set or reset geolocation emulation for the calling session's owned browser tab. Raw-CDP targets use Emulation.setGeolocationOverride and Browser.setPermission: grant_permission=true grants geolocation to the current page origin, grant_permission=false denies it so getCurrentPosition rejects. Normal Chrome bridge chrome-tab:* targets in the already-open authenticated profile use the bundled geolocationEmulation bridge lane with target-scoped debugger geolocation plus a MAIN-world navigator.permissions/geolocation shim and page-visible readback. operation=reset clears the override and restores the origin/page behavior to the profile default. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_geolocation(
        &self,
        params: Parameters<BrowserGeolocationParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserGeolocationResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = GEOLOCATION_TOOL,
            "tool.invocation kind=browser_geolocation"
        );
        let session_id = require_target_session_id(&request_context)?;
        let geolocation = validate_browser_geolocation_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": geolocation.operation,
            "geolocation": &geolocation.geolocation,
            "grant_permission": geolocation.grant_permission,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            GEOLOCATION_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            GEOLOCATION_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": geolocation.operation,
            "geolocation": &geolocation.geolocation,
            "grant_permission": geolocation.grant_permission,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            GEOLOCATION_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_geolocation_impl(&session_id, window_hwnd, &cdp_target_id, &geolocation)
            .await;
        self.audit_action_result_for_session(GEOLOCATION_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Set or reset locale and timezone emulation for the calling session's owned browser tab. Raw-CDP targets use Emulation.setLocaleOverride and/or Emulation.setTimezoneOverride plus Runtime.evaluate readback. Normal Chrome bridge chrome-tab:* targets in the already-open authenticated profile use the bundled localeEmulation bridge lane with target-scoped debugger locale/timezone override and MAIN-world Intl/Date readback. operation=set applies locale and/or timezone_id; operation=reset clears both overrides to host defaults. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_locale(
        &self,
        params: Parameters<BrowserLocaleParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserLocaleResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = LOCALE_TOOL,
            "tool.invocation kind=browser_locale"
        );
        let session_id = require_target_session_id(&request_context)?;
        let locale = validate_browser_locale_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": locale.operation,
            "requested": &locale.requested,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            LOCALE_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            LOCALE_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": locale.operation,
            "requested": &locale.requested,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            LOCALE_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_locale_impl(&session_id, window_hwnd, &cdp_target_id, &locale)
            .await;
        self.audit_action_result_for_session(LOCALE_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Set or reset CSS media emulation for the calling session's owned browser tab. Raw CDP targets use Emulation.setEmulatedMedia; normal Chrome bridge chrome-tab:* targets use the mediaEmulation bridge lane in the already-open authenticated profile with MAIN-world matchMedia readback. operation=set applies media (`screen` or `print`) and/or prefers-color-scheme / prefers-reduced-motion, then reads back matchMedia state for screen, print, color-scheme, and reduced-motion. Unspecified fields are cleared on each set so stale media features do not persist. operation=reset clears media and media-feature overrides. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Use browser_evaluate or cdp_target_info page text as an independent FSV readback for matchMedia('(prefers-color-scheme: dark)') and print media behavior."
    )]
    pub async fn browser_media(
        &self,
        params: Parameters<BrowserMediaParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserMediaResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = MEDIA_TOOL,
            "tool.invocation kind=browser_media"
        );
        let session_id = require_target_session_id(&request_context)?;
        let media = validate_browser_media_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": media.operation,
            "requested": &media.requested,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            MEDIA_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            MEDIA_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": media.operation,
            "requested": &media.requested,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            MEDIA_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_media_impl(&session_id, window_hwnd, &cdp_target_id, &media)
            .await;
        self.audit_action_result_for_session(MEDIA_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_resize_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserResizeParams,
    ) -> Result<BrowserResizeResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_resize_bridge_impl(session_id, window_hwnd, cdp_target_id, params)
                    .await;
            }
            return Err(browser_raw_cdp_required_error(RESIZE_TOOL, window_hwnd));
        };
        let result = match params.operation {
            BrowserResizeOperation::Set => {
                let (Some(width), Some(height), Some(device_scale_factor), Some(is_mobile)) = (
                    params.width,
                    params.height,
                    params.device_scale_factor,
                    params.is_mobile,
                ) else {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{RESIZE_TOOL} set operation reached dispatch without validated viewport fields"
                        ),
                    ));
                };
                synapse_a11y::cdp_set_viewport_size_with_mobile(
                    &endpoint,
                    cdp_target_id,
                    width,
                    height,
                    device_scale_factor,
                    is_mobile,
                )
                .await
            }
            BrowserResizeOperation::Reset => {
                synapse_a11y::cdp_reset_viewport_size(&endpoint, cdp_target_id).await
            }
        }
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{RESIZE_TOOL} raw CDP viewport emulation failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_VIEWPORT_RESIZE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            inner_width = result.readback.inner_width,
            inner_height = result.readback.inner_height,
            device_pixel_ratio = result.readback.device_pixel_ratio,
            "readback=Emulation.setDeviceMetricsOverride+Runtime.evaluate outcome=viewport_metrics"
        );
        Ok(browser_resize_response(session_id, window_hwnd, result))
    }

    #[cfg(windows)]
    async fn browser_resize_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserResizeParams,
    ) -> Result<BrowserResizeResponse, ErrorData> {
        let operation = match params.operation {
            BrowserResizeOperation::Set => "set",
            BrowserResizeOperation::Reset => "reset",
        };
        let result = crate::chrome_debugger_bridge::viewport_emulation(
            crate::chrome_debugger_bridge::ChromeDebuggerViewportEmulationRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                operation,
                width: params.width,
                height: params.height,
                device_scale_factor: params.device_scale_factor,
                is_mobile: params.is_mobile,
                wait_timeout_ms: DEFAULT_BRIDGE_VIEWPORT_WAIT_TIMEOUT_MS,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{RESIZE_TOOL} normal Chrome bridge viewport emulation failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        tracing::info!(
            code = "CHROME_BRIDGE_VIEWPORT_RESIZE",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?params.operation,
            inner_width = result.viewport.inner_width,
            inner_height = result.viewport.inner_height,
            device_pixel_ratio = result.viewport.device_pixel_ratio,
            "readback=chrome.debugger.Emulation+chrome.scripting.executeScript outcome=viewport_metrics"
        );
        Ok(browser_resize_bridge_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(windows)]
    async fn browser_device_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserDeviceParams,
    ) -> Result<BrowserDeviceResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_device_bridge_impl(session_id, window_hwnd, cdp_target_id, params)
                    .await;
            }
            return Err(browser_raw_cdp_required_error(DEVICE_TOOL, window_hwnd));
        };
        let result = match params.operation {
            BrowserDeviceOperation::Set => {
                let descriptor = params.descriptor.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{DEVICE_TOOL} set operation reached dispatch without validated device descriptor"
                        ),
                    )
                })?;
                synapse_a11y::cdp_apply_device_descriptor(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpDeviceDescriptor {
                        user_agent: descriptor.user_agent.clone(),
                        width: descriptor.width,
                        height: descriptor.height,
                        device_scale_factor: descriptor.device_scale_factor,
                        is_mobile: descriptor.is_mobile,
                        has_touch: descriptor.has_touch,
                        max_touch_points: descriptor.max_touch_points,
                    },
                )
                .await
            }
            BrowserDeviceOperation::Reset => {
                synapse_a11y::cdp_reset_device_descriptor(&endpoint, cdp_target_id).await
            }
        }
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{DEVICE_TOOL} raw CDP device emulation failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_DEVICE_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            inner_width = result.readback.viewport.inner_width,
            inner_height = result.readback.viewport.inner_height,
            device_pixel_ratio = result.readback.viewport.device_pixel_ratio,
            max_touch_points = result.readback.max_touch_points,
            pointer_coarse = result.readback.pointer_coarse,
            "readback=Emulation.device_descriptor+Runtime.evaluate outcome=device_metrics"
        );
        Ok(browser_device_response(session_id, window_hwnd, result))
    }

    #[cfg(windows)]
    async fn browser_device_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserDeviceParams,
    ) -> Result<BrowserDeviceResponse, ErrorData> {
        let operation = match params.operation {
            BrowserDeviceOperation::Set => "set",
            BrowserDeviceOperation::Reset => "reset",
        };
        let descriptor = params.descriptor.as_ref();
        let result = crate::chrome_debugger_bridge::device_emulation(
            crate::chrome_debugger_bridge::ChromeDebuggerDeviceEmulationRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                operation,
                user_agent: descriptor.map(|value| value.user_agent.as_str()),
                width: descriptor.map(|value| value.width),
                height: descriptor.map(|value| value.height),
                device_scale_factor: descriptor.map(|value| value.device_scale_factor),
                is_mobile: descriptor.map(|value| value.is_mobile),
                has_touch: descriptor.map(|value| value.has_touch),
                max_touch_points: descriptor.map(|value| value.max_touch_points),
                wait_timeout_ms: DEFAULT_BRIDGE_VIEWPORT_WAIT_TIMEOUT_MS,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{DEVICE_TOOL} normal Chrome bridge device emulation failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        tracing::info!(
            code = "CHROME_BRIDGE_DEVICE_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?params.operation,
            inner_width = result.device.viewport.inner_width,
            inner_height = result.device.viewport.inner_height,
            device_pixel_ratio = result.device.viewport.device_pixel_ratio,
            max_touch_points = result.device.max_touch_points,
            pointer_coarse = result.device.pointer_coarse,
            "readback=chrome.debugger.Emulation+chrome.scripting.executeScript outcome=device_metrics"
        );
        Ok(browser_device_bridge_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(windows)]
    async fn browser_geolocation_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserGeolocationParams,
    ) -> Result<BrowserGeolocationResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_geolocation_bridge_impl(session_id, window_hwnd, cdp_target_id, params)
                    .await;
            }
            return Err(browser_raw_cdp_required_error(
                GEOLOCATION_TOOL,
                window_hwnd,
            ));
        };
        let result = match params.operation {
            BrowserGeolocationOperation::Set => {
                let geolocation = params.geolocation.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{GEOLOCATION_TOOL} set operation reached dispatch without validated geolocation override"
                        ),
                    )
                })?;
                synapse_a11y::cdp_set_geolocation_override(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpGeolocationOverride {
                        latitude: geolocation.latitude,
                        longitude: geolocation.longitude,
                        accuracy: geolocation.accuracy,
                        altitude: geolocation.altitude,
                        altitude_accuracy: geolocation.altitude_accuracy,
                        heading: geolocation.heading,
                        speed: geolocation.speed,
                    },
                    params.grant_permission,
                )
                .await
            }
            BrowserGeolocationOperation::Reset => {
                synapse_a11y::cdp_reset_geolocation_override(&endpoint, cdp_target_id).await
            }
        }
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{GEOLOCATION_TOOL} raw CDP geolocation emulation failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_GEOLOCATION_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            origin = %result.origin,
            permission_state = %result.readback.permission_state,
            position_returned = result.readback.position.is_some(),
            error_code = ?result.readback.error.as_ref().map(|error| error.code),
            "readback=Emulation.geolocation+Browser.setPermission+Runtime.evaluate outcome=geolocation_state"
        );
        Ok(browser_geolocation_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(windows)]
    async fn browser_geolocation_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserGeolocationParams,
    ) -> Result<BrowserGeolocationResponse, ErrorData> {
        let operation = match params.operation {
            BrowserGeolocationOperation::Set => "set",
            BrowserGeolocationOperation::Reset => "reset",
        };
        let geolocation = params.geolocation.as_ref();
        let result = crate::chrome_debugger_bridge::geolocation_emulation(
            crate::chrome_debugger_bridge::ChromeDebuggerGeolocationEmulationRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id.to_owned(),
                operation: operation.to_owned(),
                latitude: geolocation.map(|value| value.latitude),
                longitude: geolocation.map(|value| value.longitude),
                accuracy: geolocation.map(|value| value.accuracy),
                altitude: geolocation.and_then(|value| value.altitude),
                altitude_accuracy: geolocation.and_then(|value| value.altitude_accuracy),
                heading: geolocation.and_then(|value| value.heading),
                speed: geolocation.and_then(|value| value.speed),
                grant_permission: match params.operation {
                    BrowserGeolocationOperation::Set => Some(params.grant_permission),
                    BrowserGeolocationOperation::Reset => None,
                },
                wait_timeout_ms: DEFAULT_BRIDGE_VIEWPORT_WAIT_TIMEOUT_MS,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{GEOLOCATION_TOOL} normal Chrome bridge geolocation emulation failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        tracing::info!(
            code = "CHROME_BRIDGE_GEOLOCATION_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?params.operation,
            origin = %result.origin,
            permission_state = %result.geolocation.permission_state,
            position_returned = result.geolocation.position.is_some(),
            error_code = ?result.geolocation.error.as_ref().map(|error| error.code),
            "readback=chrome.debugger.Emulation+chrome.scripting.executeScript outcome=geolocation_state"
        );
        Ok(browser_geolocation_bridge_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(windows)]
    async fn browser_locale_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserLocaleParams,
    ) -> Result<BrowserLocaleResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_locale_bridge_impl(session_id, window_hwnd, cdp_target_id, params)
                    .await;
            }
            return Err(browser_raw_cdp_required_error(LOCALE_TOOL, window_hwnd));
        };
        let result = match params.operation {
            BrowserLocaleOperation::Set => {
                let requested = params.requested.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{LOCALE_TOOL} set operation reached dispatch without validated locale override"
                        ),
                    )
                })?;
                synapse_a11y::cdp_set_locale_timezone_override(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpLocaleTimezoneOverride {
                        locale: requested.locale.clone(),
                        timezone_id: requested.timezone_id.clone(),
                    },
                )
                .await
            }
            BrowserLocaleOperation::Reset => {
                synapse_a11y::cdp_reset_locale_timezone_override(&endpoint, cdp_target_id).await
            }
        }
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{LOCALE_TOOL} raw CDP locale/timezone emulation failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_LOCALE_TIMEZONE_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            locale = %result.readback.locale,
            time_zone = %result.readback.time_zone,
            sample_number = %result.readback.sample_number,
            "readback=Emulation.locale_timezone+Runtime.evaluate outcome=intl_state"
        );
        Ok(browser_locale_response(session_id, window_hwnd, result))
    }

    #[cfg(windows)]
    async fn browser_locale_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserLocaleParams,
    ) -> Result<BrowserLocaleResponse, ErrorData> {
        let operation = match params.operation {
            BrowserLocaleOperation::Set => "set",
            BrowserLocaleOperation::Reset => "reset",
        };
        let requested = params.requested.as_ref();
        let result = crate::chrome_debugger_bridge::locale_emulation(
            crate::chrome_debugger_bridge::ChromeDebuggerLocaleEmulationRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                operation,
                locale: requested.and_then(|value| value.locale.as_deref()),
                timezone_id: requested.and_then(|value| value.timezone_id.as_deref()),
                wait_timeout_ms: DEFAULT_BRIDGE_VIEWPORT_WAIT_TIMEOUT_MS,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{LOCALE_TOOL} normal Chrome bridge locale/timezone emulation failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        tracing::info!(
            code = "CHROME_BRIDGE_LOCALE_TIMEZONE_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?params.operation,
            locale = %result.locale.locale,
            time_zone = %result.locale.time_zone,
            sample_number = %result.locale.sample_number,
            "readback=chrome.debugger.Emulation+chrome.scripting.executeScript outcome=intl_state"
        );
        Ok(browser_locale_bridge_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(windows)]
    async fn browser_media_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserMediaParams,
    ) -> Result<BrowserMediaResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_media_bridge_impl(session_id, window_hwnd, cdp_target_id, params)
                    .await;
            }
            return Err(browser_raw_cdp_required_error(MEDIA_TOOL, window_hwnd));
        };
        let result = match params.operation {
            BrowserMediaOperation::Set => {
                let requested = params.requested.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{MEDIA_TOOL} set operation reached dispatch without validated media override"
                        ),
                    )
                })?;
                synapse_a11y::cdp_set_media_override(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpMediaOverride {
                        media: requested.media.clone(),
                        color_scheme: requested.color_scheme.clone(),
                        reduced_motion: requested.reduced_motion.clone(),
                    },
                )
                .await
            }
            BrowserMediaOperation::Reset => {
                synapse_a11y::cdp_reset_media_override(&endpoint, cdp_target_id).await
            }
        }
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{MEDIA_TOOL} raw CDP media emulation failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_MEDIA_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            media_print = result.readback.media_print,
            color_scheme_dark = result.readback.color_scheme_dark,
            reduced_motion_reduce = result.readback.reduced_motion_reduce,
            "readback=Emulation.setEmulatedMedia+Runtime.evaluate outcome=media_state"
        );
        Ok(browser_media_response(session_id, window_hwnd, result))
    }

    #[cfg(windows)]
    async fn browser_media_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserMediaParams,
    ) -> Result<BrowserMediaResponse, ErrorData> {
        let operation = match params.operation {
            BrowserMediaOperation::Set => "set",
            BrowserMediaOperation::Reset => "reset",
        };
        let requested = params.requested.as_ref();
        let result = crate::chrome_debugger_bridge::media_emulation(
            crate::chrome_debugger_bridge::ChromeDebuggerMediaEmulationRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                operation,
                media: requested.and_then(|value| value.media.as_deref()),
                color_scheme: requested.and_then(|value| value.color_scheme.as_deref()),
                reduced_motion: requested.and_then(|value| value.reduced_motion.as_deref()),
                wait_timeout_ms: DEFAULT_BRIDGE_VIEWPORT_WAIT_TIMEOUT_MS,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{MEDIA_TOOL} normal Chrome bridge media emulation failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        tracing::info!(
            code = "CHROME_BRIDGE_MEDIA_EMULATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?params.operation,
            media_print = result.media.media_print,
            color_scheme_dark = result.media.color_scheme_dark,
            reduced_motion_reduce = result.media.reduced_motion_reduce,
            "readback=chrome.debugger.Emulation+chrome.scripting.executeScript outcome=media_state"
        );
        Ok(browser_media_bridge_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(not(windows))]
    async fn browser_resize_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserResizeParams,
    ) -> Result<BrowserResizeResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_resize is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_device_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserDeviceParams,
    ) -> Result<BrowserDeviceResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_device is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_geolocation_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserGeolocationParams,
    ) -> Result<BrowserGeolocationResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_geolocation is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_locale_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserLocaleParams,
    ) -> Result<BrowserLocaleResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_locale is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_media_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserMediaParams,
    ) -> Result<BrowserMediaResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_media is only available on Windows in this build",
        ))
    }
}

fn validate_browser_resize_params(
    params: &BrowserResizeParams,
) -> Result<NormalizedBrowserResizeParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if params.operation == BrowserResizeOperation::Reset {
        reject_resize_field(params.width, "width", "reset")?;
        reject_resize_field(params.height, "height", "reset")?;
        reject_resize_field(params.device_scale_factor, "device_scale_factor", "reset")?;
        reject_resize_field(params.is_mobile, "is_mobile", "reset")?;
        return Ok(NormalizedBrowserResizeParams {
            operation: BrowserResizeOperation::Reset,
            width: None,
            height: None,
            device_scale_factor: None,
            is_mobile: None,
        });
    }

    let width = params.width.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{RESIZE_TOOL} operation=set requires width"),
        )
    })?;
    let height = params.height.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{RESIZE_TOOL} operation=set requires height"),
        )
    })?;
    validate_dimension("width", width)?;
    validate_dimension("height", height)?;
    let device_scale_factor = params.device_scale_factor.unwrap_or(1.0);
    if !device_scale_factor.is_finite()
        || device_scale_factor <= 0.0
        || device_scale_factor > synapse_a11y::CDP_DEVICE_SCALE_FACTOR_MAX
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{RESIZE_TOOL} device_scale_factor must be finite and in 0..={}",
                synapse_a11y::CDP_DEVICE_SCALE_FACTOR_MAX
            ),
        ));
    }
    Ok(NormalizedBrowserResizeParams {
        operation: BrowserResizeOperation::Set,
        width: Some(width),
        height: Some(height),
        device_scale_factor: Some(device_scale_factor),
        is_mobile: Some(params.is_mobile.unwrap_or(false)),
    })
}

fn validate_browser_device_params(
    params: &BrowserDeviceParams,
) -> Result<NormalizedBrowserDeviceParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if params.operation == BrowserDeviceOperation::Reset {
        reject_device_field(params.user_agent.as_ref(), "user_agent", "reset")?;
        reject_device_field(params.width, "width", "reset")?;
        reject_device_field(params.height, "height", "reset")?;
        reject_device_field(params.device_scale_factor, "device_scale_factor", "reset")?;
        reject_device_field(params.is_mobile, "is_mobile", "reset")?;
        reject_device_field(params.has_touch, "has_touch", "reset")?;
        reject_device_field(params.max_touch_points, "max_touch_points", "reset")?;
        return Ok(NormalizedBrowserDeviceParams {
            operation: BrowserDeviceOperation::Reset,
            descriptor: None,
        });
    }

    let user_agent = validate_device_user_agent(params.user_agent.as_deref())?;
    let width = params.width.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} operation=set requires width"),
        )
    })?;
    let height = params.height.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} operation=set requires height"),
        )
    })?;
    validate_device_dimension("width", width)?;
    validate_device_dimension("height", height)?;
    let device_scale_factor = params.device_scale_factor.unwrap_or(1.0);
    if !device_scale_factor.is_finite()
        || device_scale_factor <= 0.0
        || device_scale_factor > synapse_a11y::CDP_DEVICE_SCALE_FACTOR_MAX
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{DEVICE_TOOL} device_scale_factor must be finite and in 0..={}",
                synapse_a11y::CDP_DEVICE_SCALE_FACTOR_MAX
            ),
        ));
    }
    let is_mobile = params.is_mobile.unwrap_or(false);
    let has_touch = params.has_touch.unwrap_or(false);
    let max_touch_points = params
        .max_touch_points
        .unwrap_or(if has_touch { 5 } else { 0 });
    if has_touch {
        if max_touch_points == 0 || max_touch_points > synapse_a11y::CDP_DEVICE_MAX_TOUCH_POINTS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{DEVICE_TOOL} max_touch_points must be 1..={} when has_touch=true",
                    synapse_a11y::CDP_DEVICE_MAX_TOUCH_POINTS
                ),
            ));
        }
    } else if max_touch_points != 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} max_touch_points must be 0 when has_touch=false"),
        ));
    }

    Ok(NormalizedBrowserDeviceParams {
        operation: BrowserDeviceOperation::Set,
        descriptor: Some(BrowserDeviceDescriptor {
            user_agent,
            width,
            height,
            device_scale_factor,
            is_mobile,
            has_touch,
            max_touch_points,
        }),
    })
}

fn validate_browser_geolocation_params(
    params: &BrowserGeolocationParams,
) -> Result<NormalizedBrowserGeolocationParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if params.operation == BrowserGeolocationOperation::Reset {
        reject_geolocation_field(params.latitude, "latitude", "reset")?;
        reject_geolocation_field(params.longitude, "longitude", "reset")?;
        reject_geolocation_field(params.accuracy, "accuracy", "reset")?;
        reject_geolocation_field(params.altitude, "altitude", "reset")?;
        reject_geolocation_field(params.altitude_accuracy, "altitude_accuracy", "reset")?;
        reject_geolocation_field(params.heading, "heading", "reset")?;
        reject_geolocation_field(params.speed, "speed", "reset")?;
        reject_geolocation_field(params.grant_permission, "grant_permission", "reset")?;
        return Ok(NormalizedBrowserGeolocationParams {
            operation: BrowserGeolocationOperation::Reset,
            geolocation: None,
            grant_permission: false,
        });
    }

    let latitude = params.latitude.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{GEOLOCATION_TOOL} operation=set requires latitude"),
        )
    })?;
    let longitude = params.longitude.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{GEOLOCATION_TOOL} operation=set requires longitude"),
        )
    })?;
    let accuracy = params.accuracy.unwrap_or(0.0);
    validate_geolocation_range("latitude", latitude, -90.0, 90.0)?;
    validate_geolocation_range("longitude", longitude, -180.0, 180.0)?;
    validate_geolocation_range(
        "accuracy",
        accuracy,
        0.0,
        synapse_a11y::CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;
    validate_geolocation_optional_finite("altitude", params.altitude)?;
    validate_geolocation_optional_range(
        "altitude_accuracy",
        params.altitude_accuracy,
        0.0,
        synapse_a11y::CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;
    validate_geolocation_optional_range("heading", params.heading, 0.0, 360.0)?;
    validate_geolocation_optional_range(
        "speed",
        params.speed,
        0.0,
        synapse_a11y::CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;

    Ok(NormalizedBrowserGeolocationParams {
        operation: BrowserGeolocationOperation::Set,
        geolocation: Some(BrowserGeolocationOverride {
            latitude,
            longitude,
            accuracy,
            altitude: params.altitude,
            altitude_accuracy: params.altitude_accuracy,
            heading: params.heading,
            speed: params.speed,
        }),
        grant_permission: params.grant_permission.unwrap_or(true),
    })
}

fn validate_browser_locale_params(
    params: &BrowserLocaleParams,
) -> Result<NormalizedBrowserLocaleParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if params.operation == BrowserLocaleOperation::Reset {
        reject_locale_field(params.locale.as_ref(), "locale", "reset")?;
        reject_locale_field(params.timezone_id.as_ref(), "timezone_id", "reset")?;
        return Ok(NormalizedBrowserLocaleParams {
            operation: BrowserLocaleOperation::Reset,
            requested: None,
        });
    }

    if params.locale.is_none() && params.timezone_id.is_none() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{LOCALE_TOOL} operation=set requires locale and/or timezone_id"),
        ));
    }
    if let Some(locale) = params.locale.as_deref() {
        validate_locale_value(locale)?;
    }
    if let Some(timezone_id) = params.timezone_id.as_deref() {
        validate_timezone_value(timezone_id)?;
    }

    Ok(NormalizedBrowserLocaleParams {
        operation: BrowserLocaleOperation::Set,
        requested: Some(BrowserLocaleOverride {
            locale: params.locale.clone(),
            timezone_id: params.timezone_id.clone(),
        }),
    })
}

fn validate_browser_media_params(
    params: &BrowserMediaParams,
) -> Result<NormalizedBrowserMediaParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if params.operation == BrowserMediaOperation::Reset {
        reject_media_field(params.media.as_ref(), "media", "reset")?;
        reject_media_field(params.color_scheme.as_ref(), "color_scheme", "reset")?;
        reject_media_field(params.reduced_motion.as_ref(), "reduced_motion", "reset")?;
        return Ok(NormalizedBrowserMediaParams {
            operation: BrowserMediaOperation::Reset,
            requested: None,
        });
    }

    if params.media.is_none() && params.color_scheme.is_none() && params.reduced_motion.is_none() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{MEDIA_TOOL} operation=set requires media, color_scheme and/or reduced_motion"
            ),
        ));
    }
    if let Some(media) = params.media.as_deref() {
        validate_media_type_value(media)?;
    }
    if let Some(color_scheme) = params.color_scheme.as_deref() {
        validate_color_scheme_value(color_scheme)?;
    }
    if let Some(reduced_motion) = params.reduced_motion.as_deref() {
        validate_reduced_motion_value(reduced_motion)?;
    }

    Ok(NormalizedBrowserMediaParams {
        operation: BrowserMediaOperation::Set,
        requested: Some(BrowserMediaOverride {
            media: params.media.clone(),
            color_scheme: params.color_scheme.clone(),
            reduced_motion: params.reduced_motion.clone(),
        }),
    })
}

fn validate_dimension(field: &str, value: u32) -> Result<(), ErrorData> {
    if value == 0 || value > synapse_a11y::CDP_DEVICE_METRICS_MAX_DIMENSION {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{RESIZE_TOOL} {field} must be 1..={}",
                synapse_a11y::CDP_DEVICE_METRICS_MAX_DIMENSION
            ),
        ));
    }
    Ok(())
}

fn validate_device_dimension(field: &str, value: u32) -> Result<(), ErrorData> {
    if value == 0 || value > synapse_a11y::CDP_DEVICE_METRICS_MAX_DIMENSION {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{DEVICE_TOOL} {field} must be 1..={}",
                synapse_a11y::CDP_DEVICE_METRICS_MAX_DIMENSION
            ),
        ));
    }
    Ok(())
}

fn validate_geolocation_range(
    field: &str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<(), ErrorData> {
    if !value.is_finite() || value < min || value > max {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{GEOLOCATION_TOOL} {field} must be finite and in {min}..={max}"),
        ));
    }
    Ok(())
}

fn validate_geolocation_optional_range(
    field: &str,
    value: Option<f64>,
    min: f64,
    max: f64,
) -> Result<(), ErrorData> {
    if let Some(value) = value {
        validate_geolocation_range(field, value, min, max)?;
    }
    Ok(())
}

fn validate_geolocation_optional_finite(field: &str, value: Option<f64>) -> Result<(), ErrorData> {
    if let Some(value) = value {
        if !value.is_finite() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{GEOLOCATION_TOOL} {field} must be finite"),
            ));
        }
    }
    Ok(())
}

fn validate_locale_value(value: &str) -> Result<(), ErrorData> {
    if value.trim() != value || value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{LOCALE_TOOL} locale must be non-empty without surrounding whitespace"),
        ));
    }
    if value.chars().count() > synapse_a11y::CDP_LOCALE_MAX_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{LOCALE_TOOL} locale must be at most {} characters",
                synapse_a11y::CDP_LOCALE_MAX_CHARS
            ),
        ));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{LOCALE_TOOL} locale must contain only ASCII letters, digits, '_' or '-'"),
        ));
    }
    Ok(())
}

fn validate_timezone_value(value: &str) -> Result<(), ErrorData> {
    if value.trim() != value || value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{LOCALE_TOOL} timezone_id must be non-empty without surrounding whitespace"),
        ));
    }
    if value.chars().count() > synapse_a11y::CDP_TIMEZONE_MAX_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{LOCALE_TOOL} timezone_id must be at most {} characters",
                synapse_a11y::CDP_TIMEZONE_MAX_CHARS
            ),
        ));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-' | '+'))
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{LOCALE_TOOL} timezone_id must contain only ASCII letters, digits, '/', '_', '-' or '+'"
            ),
        ));
    }
    Ok(())
}

fn validate_media_type_value(value: &str) -> Result<(), ErrorData> {
    if matches!(value, "screen" | "print") {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{MEDIA_TOOL} media must be 'screen' or 'print'"),
        ))
    }
}

fn validate_color_scheme_value(value: &str) -> Result<(), ErrorData> {
    if matches!(value, "light" | "dark" | "no-preference") {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{MEDIA_TOOL} color_scheme must be 'light', 'dark' or 'no-preference'"),
        ))
    }
}

fn validate_reduced_motion_value(value: &str) -> Result<(), ErrorData> {
    if matches!(value, "reduce" | "no-preference") {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{MEDIA_TOOL} reduced_motion must be 'reduce' or 'no-preference'"),
        ))
    }
}

fn validate_device_user_agent(value: Option<&str>) -> Result<String, ErrorData> {
    let Some(value) = value else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} operation=set requires user_agent"),
        ));
    };
    if value.trim() != value || value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} user_agent must be non-empty without surrounding whitespace"),
        ));
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} user_agent must not contain line breaks or NUL"),
        ));
    }
    if value.chars().count() > synapse_a11y::CDP_DEVICE_MAX_USER_AGENT_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{DEVICE_TOOL} user_agent must be at most {} Unicode scalar values",
                synapse_a11y::CDP_DEVICE_MAX_USER_AGENT_CHARS
            ),
        ));
    }
    Ok(value.to_owned())
}

fn reject_resize_field<T>(value: Option<T>, field: &str, operation: &str) -> Result<(), ErrorData> {
    if value.is_none() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{RESIZE_TOOL} {field} is not valid for operation={operation}"),
        ))
    }
}

fn reject_device_field<T>(value: Option<T>, field: &str, operation: &str) -> Result<(), ErrorData> {
    if value.is_none() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{DEVICE_TOOL} {field} is not valid for operation={operation}"),
        ))
    }
}

fn reject_geolocation_field<T>(
    value: Option<T>,
    field: &str,
    operation: &str,
) -> Result<(), ErrorData> {
    if value.is_none() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{GEOLOCATION_TOOL} {field} is not valid for operation={operation}"),
        ))
    }
}

fn reject_locale_field<T>(value: Option<T>, field: &str, operation: &str) -> Result<(), ErrorData> {
    if value.is_none() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{LOCALE_TOOL} {field} is not valid for operation={operation}"),
        ))
    }
}

fn reject_media_field<T>(value: Option<T>, field: &str, operation: &str) -> Result<(), ErrorData> {
    if value.is_none() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{MEDIA_TOOL} {field} is not valid for operation={operation}"),
        ))
    }
}

fn browser_resize_response(
    session_id: &str,
    window_hwnd: i64,
    result: synapse_a11y::CdpViewportResult,
) -> BrowserResizeResponse {
    BrowserResizeResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint: result.endpoint,
        cdp_target_id: result.cdp_target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserResizeOperation::Reset,
            _ => BrowserResizeOperation::Set,
        },
        requested: result.requested.map(|requested| BrowserViewportOverride {
            width: requested.width,
            height: requested.height,
            device_scale_factor: requested.device_scale_factor,
            mobile: requested.mobile,
        }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        viewport: BrowserViewportReadback {
            inner_width: result.readback.inner_width,
            inner_height: result.readback.inner_height,
            device_pixel_ratio: result.readback.device_pixel_ratio,
            screen_width: result.readback.screen_width,
            screen_height: result.readback.screen_height,
            outer_width: result.readback.outer_width,
            outer_height: result.readback.outer_height,
            visual_viewport_width: result.readback.visual_viewport_width,
            visual_viewport_height: result.readback.visual_viewport_height,
        },
        readback_backend: "Emulation.setDeviceMetricsOverride / Emulation.clearDeviceMetricsOverride + Runtime.evaluate".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        source_of_truth: "raw CDP Runtime.evaluate window.innerWidth/window.innerHeight/devicePixelRatio".to_owned(),
    }
}

fn browser_resize_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    result: crate::chrome_debugger_bridge::ChromeDebuggerViewportEmulationResult,
) -> BrowserResizeResponse {
    BrowserResizeResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: chrome_debugger_default_endpoint(),
        cdp_target_id: result.target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserResizeOperation::Reset,
            _ => BrowserResizeOperation::Set,
        },
        requested: result.requested.map(|requested| BrowserViewportOverride {
            width: requested.width,
            height: requested.height,
            device_scale_factor: requested.device_scale_factor,
            mobile: requested.mobile,
        }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        viewport: BrowserViewportReadback {
            inner_width: result.viewport.inner_width,
            inner_height: result.viewport.inner_height,
            device_pixel_ratio: result.viewport.device_pixel_ratio,
            screen_width: result.viewport.screen_width,
            screen_height: result.viewport.screen_height,
            outer_width: result.viewport.outer_width,
            outer_height: result.viewport.outer_height,
            visual_viewport_width: result.viewport.visual_viewport_width,
            visual_viewport_height: result.viewport.visual_viewport_height,
        },
        readback_backend: if result.readback_backend.is_empty() {
            "chrome.debugger.Emulation + chrome.scripting.executeScript".to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.is_empty() {
            "chrome_tabs_extension_debugger".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
        source_of_truth: if result.source_of_truth.is_empty() {
            "normal Chrome bridge page script window.innerWidth/window.innerHeight/devicePixelRatio"
                .to_owned()
        } else {
            result.source_of_truth
        },
    }
}

fn browser_device_response(
    session_id: &str,
    window_hwnd: i64,
    result: synapse_a11y::CdpDeviceResult,
) -> BrowserDeviceResponse {
    BrowserDeviceResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint: result.endpoint,
        cdp_target_id: result.cdp_target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserDeviceOperation::Reset,
            _ => BrowserDeviceOperation::Set,
        },
        descriptor: result.descriptor.map(|descriptor| BrowserDeviceDescriptor {
            user_agent: descriptor.user_agent,
            width: descriptor.width,
            height: descriptor.height,
            device_scale_factor: descriptor.device_scale_factor,
            is_mobile: descriptor.is_mobile,
            has_touch: descriptor.has_touch,
            max_touch_points: descriptor.max_touch_points,
        }),
        restored_user_agent: result.restored_user_agent,
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        device: BrowserDeviceReadback {
            viewport: BrowserViewportReadback {
                inner_width: result.readback.viewport.inner_width,
                inner_height: result.readback.viewport.inner_height,
                device_pixel_ratio: result.readback.viewport.device_pixel_ratio,
                screen_width: result.readback.viewport.screen_width,
                screen_height: result.readback.viewport.screen_height,
                outer_width: result.readback.viewport.outer_width,
                outer_height: result.readback.viewport.outer_height,
                visual_viewport_width: result.readback.viewport.visual_viewport_width,
                visual_viewport_height: result.readback.viewport.visual_viewport_height,
            },
            user_agent: result.readback.user_agent,
            max_touch_points: result.readback.max_touch_points,
            ontouchstart_available: result.readback.ontouchstart_available,
            pointer_coarse: result.readback.pointer_coarse,
            any_pointer_coarse: result.readback.any_pointer_coarse,
            hover_none: result.readback.hover_none,
            any_hover_none: result.readback.any_hover_none,
        },
        readback_backend: "Emulation.setUserAgentOverride + Emulation.setDeviceMetricsOverride + Emulation.setTouchEmulationEnabled + Runtime.evaluate".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        source_of_truth:
            "raw CDP Runtime.evaluate navigator/userAgent/viewport/touch media queries".to_owned(),
    }
}

fn browser_device_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    result: crate::chrome_debugger_bridge::ChromeDebuggerDeviceEmulationResult,
) -> BrowserDeviceResponse {
    BrowserDeviceResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: chrome_debugger_default_endpoint(),
        cdp_target_id: result.target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserDeviceOperation::Reset,
            _ => BrowserDeviceOperation::Set,
        },
        descriptor: result.descriptor.map(|descriptor| BrowserDeviceDescriptor {
            user_agent: descriptor.user_agent,
            width: descriptor.width,
            height: descriptor.height,
            device_scale_factor: descriptor.device_scale_factor,
            is_mobile: descriptor.is_mobile,
            has_touch: descriptor.has_touch,
            max_touch_points: descriptor.max_touch_points,
        }),
        restored_user_agent: result.restored_user_agent,
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        device: BrowserDeviceReadback {
            viewport: BrowserViewportReadback {
                inner_width: result.device.viewport.inner_width,
                inner_height: result.device.viewport.inner_height,
                device_pixel_ratio: result.device.viewport.device_pixel_ratio,
                screen_width: result.device.viewport.screen_width,
                screen_height: result.device.viewport.screen_height,
                outer_width: result.device.viewport.outer_width,
                outer_height: result.device.viewport.outer_height,
                visual_viewport_width: result.device.viewport.visual_viewport_width,
                visual_viewport_height: result.device.viewport.visual_viewport_height,
            },
            user_agent: result.device.user_agent,
            max_touch_points: result.device.max_touch_points,
            ontouchstart_available: result.device.ontouchstart_available,
            pointer_coarse: result.device.pointer_coarse,
            any_pointer_coarse: result.device.any_pointer_coarse,
            hover_none: result.device.hover_none,
            any_hover_none: result.device.any_hover_none,
        },
        readback_backend: if result.readback_backend.is_empty() {
            "chrome.debugger.Emulation + chrome.scripting.executeScript".to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.is_empty() {
            "chrome_tabs_extension_debugger".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
        source_of_truth: if result.source_of_truth.is_empty() {
            "normal Chrome bridge page script navigator/userAgent/viewport/touch media queries"
                .to_owned()
        } else {
            result.source_of_truth
        },
    }
}

fn browser_geolocation_response(
    session_id: &str,
    window_hwnd: i64,
    result: synapse_a11y::CdpGeolocationResult,
) -> BrowserGeolocationResponse {
    BrowserGeolocationResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint: result.endpoint,
        cdp_target_id: result.cdp_target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserGeolocationOperation::Reset,
            _ => BrowserGeolocationOperation::Set,
        },
        origin: result.origin,
        requested: result.requested.map(|requested| BrowserGeolocationOverride {
            latitude: requested.latitude,
            longitude: requested.longitude,
            accuracy: requested.accuracy,
            altitude: requested.altitude,
            altitude_accuracy: requested.altitude_accuracy,
            heading: requested.heading,
            speed: requested.speed,
        }),
        permission_setting: result.permission_setting,
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        geolocation: BrowserGeolocationReadback {
            permission_state: result.readback.permission_state,
            position: result
                .readback
                .position
                .map(|position| BrowserGeolocationCoordinatesReadback {
                    latitude: position.latitude,
                    longitude: position.longitude,
                    accuracy: position.accuracy,
                    altitude: position.altitude,
                    altitude_accuracy: position.altitude_accuracy,
                    heading: position.heading,
                    speed: position.speed,
                    timestamp: position.timestamp,
                }),
            error: result
                .readback
                .error
                .map(|error| BrowserGeolocationErrorReadback {
                    code: error.code,
                    message: error.message,
                }),
        },
        readback_backend: "Emulation.setGeolocationOverride / Emulation.clearGeolocationOverride + Browser.setPermission + Runtime.evaluate".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        source_of_truth:
            "raw CDP Runtime.evaluate navigator.permissions + navigator.geolocation".to_owned(),
    }
}

fn browser_geolocation_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    result: crate::chrome_debugger_bridge::ChromeDebuggerGeolocationEmulationResult,
) -> BrowserGeolocationResponse {
    BrowserGeolocationResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: chrome_debugger_default_endpoint(),
        cdp_target_id: result.target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserGeolocationOperation::Reset,
            _ => BrowserGeolocationOperation::Set,
        },
        origin: result.origin,
        requested: result
            .requested
            .map(|requested| BrowserGeolocationOverride {
                latitude: requested.latitude,
                longitude: requested.longitude,
                accuracy: requested.accuracy,
                altitude: requested.altitude,
                altitude_accuracy: requested.altitude_accuracy,
                heading: requested.heading,
                speed: requested.speed,
            }),
        permission_setting: result.permission_setting,
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        geolocation: BrowserGeolocationReadback {
            permission_state: result.geolocation.permission_state,
            position: result.geolocation.position.map(|position| {
                BrowserGeolocationCoordinatesReadback {
                    latitude: position.latitude,
                    longitude: position.longitude,
                    accuracy: position.accuracy,
                    altitude: position.altitude,
                    altitude_accuracy: position.altitude_accuracy,
                    heading: position.heading,
                    speed: position.speed,
                    timestamp: position.timestamp,
                }
            }),
            error: result
                .geolocation
                .error
                .map(|error| BrowserGeolocationErrorReadback {
                    code: error.code,
                    message: error.message,
                }),
        },
        readback_backend: if result.readback_backend.is_empty() {
            "chrome.debugger.Emulation + chrome.scripting.executeScript".to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.is_empty() {
            "chrome_tabs_extension_debugger".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
        source_of_truth: if result.source_of_truth.is_empty() {
            "normal Chrome bridge page script navigator.permissions + navigator.geolocation"
                .to_owned()
        } else {
            result.source_of_truth
        },
    }
}

fn browser_locale_response(
    session_id: &str,
    window_hwnd: i64,
    result: synapse_a11y::CdpLocaleTimezoneResult,
) -> BrowserLocaleResponse {
    BrowserLocaleResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint: result.endpoint,
        cdp_target_id: result.cdp_target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserLocaleOperation::Reset,
            _ => BrowserLocaleOperation::Set,
        },
        requested: result.requested.map(|requested| BrowserLocaleOverride {
            locale: requested.locale,
            timezone_id: requested.timezone_id,
        }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        locale: BrowserLocaleReadback {
            locale: result.readback.locale,
            calendar: result.readback.calendar,
            numbering_system: result.readback.numbering_system,
            time_zone: result.readback.time_zone,
            sample_number: result.readback.sample_number,
            sample_date: result.readback.sample_date,
            date_string: result.readback.date_string,
            timezone_offset_minutes: result.readback.timezone_offset_minutes,
        },
        readback_backend:
            "Emulation.setLocaleOverride + Emulation.setTimezoneOverride + Runtime.evaluate"
                .to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        source_of_truth: "raw CDP Runtime.evaluate Intl.DateTimeFormat/NumberFormat and Date"
            .to_owned(),
    }
}

fn browser_locale_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    result: crate::chrome_debugger_bridge::ChromeDebuggerLocaleEmulationResult,
) -> BrowserLocaleResponse {
    BrowserLocaleResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: chrome_debugger_default_endpoint(),
        cdp_target_id: result.target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserLocaleOperation::Reset,
            _ => BrowserLocaleOperation::Set,
        },
        requested: result.requested.map(|requested| BrowserLocaleOverride {
            locale: requested.locale,
            timezone_id: requested.timezone_id,
        }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        locale: BrowserLocaleReadback {
            locale: result.locale.locale,
            calendar: result.locale.calendar,
            numbering_system: result.locale.numbering_system,
            time_zone: result.locale.time_zone,
            sample_number: result.locale.sample_number,
            sample_date: result.locale.sample_date,
            date_string: result.locale.date_string,
            timezone_offset_minutes: result.locale.timezone_offset_minutes,
        },
        readback_backend: if result.readback_backend.is_empty() {
            "chrome.debugger.Emulation locale/timezone + chrome.scripting.executeScript".to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.is_empty() {
            "chrome_tabs_extension_debugger".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
        source_of_truth: if result.source_of_truth.is_empty() {
            "normal Chrome bridge page script Intl.DateTimeFormat/NumberFormat and Date".to_owned()
        } else {
            result.source_of_truth
        },
    }
}

fn browser_media_response(
    session_id: &str,
    window_hwnd: i64,
    result: synapse_a11y::CdpMediaResult,
) -> BrowserMediaResponse {
    BrowserMediaResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint: result.endpoint,
        cdp_target_id: result.cdp_target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserMediaOperation::Reset,
            _ => BrowserMediaOperation::Set,
        },
        requested: result.requested.map(|requested| BrowserMediaOverride {
            media: requested.media,
            color_scheme: requested.color_scheme,
            reduced_motion: requested.reduced_motion,
        }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        media: BrowserMediaReadback {
            media_screen: result.readback.media_screen,
            media_print: result.readback.media_print,
            color_scheme_dark: result.readback.color_scheme_dark,
            color_scheme_light: result.readback.color_scheme_light,
            color_scheme_no_preference: result.readback.color_scheme_no_preference,
            reduced_motion_reduce: result.readback.reduced_motion_reduce,
            reduced_motion_no_preference: result.readback.reduced_motion_no_preference,
        },
        readback_backend: "Emulation.setEmulatedMedia + Runtime.evaluate".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
        source_of_truth: "raw CDP Runtime.evaluate matchMedia media queries".to_owned(),
    }
}

fn browser_media_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    result: crate::chrome_debugger_bridge::ChromeDebuggerMediaEmulationResult,
) -> BrowserMediaResponse {
    BrowserMediaResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: chrome_debugger_default_endpoint(),
        cdp_target_id: result.target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserMediaOperation::Reset,
            _ => BrowserMediaOperation::Set,
        },
        requested: result.requested.map(|requested| BrowserMediaOverride {
            media: requested.media,
            color_scheme: requested.color_scheme,
            reduced_motion: requested.reduced_motion,
        }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        media: BrowserMediaReadback {
            media_screen: result.media.media_screen,
            media_print: result.media.media_print,
            color_scheme_dark: result.media.color_scheme_dark,
            color_scheme_light: result.media.color_scheme_light,
            color_scheme_no_preference: result.media.color_scheme_no_preference,
            reduced_motion_reduce: result.media.reduced_motion_reduce,
            reduced_motion_no_preference: result.media.reduced_motion_no_preference,
        },
        readback_backend: if result.readback_backend.is_empty() {
            "chrome.debugger.Emulation.setEmulatedMedia + chrome.scripting.executeScript".to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.is_empty() {
            "chrome_tabs_extension_debugger".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
        source_of_truth: if result.source_of_truth.is_empty() {
            "normal Chrome bridge page script matchMedia media queries".to_owned()
        } else {
            result.source_of_truth
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_resize_validation_edges() {
        let set = validate_browser_resize_params(&BrowserResizeParams {
            width: Some(1280),
            height: Some(720),
            device_scale_factor: None,
            ..BrowserResizeParams::default()
        })
        .expect("valid set params");
        assert_eq!(set.operation, BrowserResizeOperation::Set);
        assert_eq!(set.device_scale_factor, Some(1.0));
        assert_eq!(set.is_mobile, Some(false));

        let mobile = validate_browser_resize_params(&BrowserResizeParams {
            width: Some(390),
            height: Some(844),
            device_scale_factor: Some(3.0),
            is_mobile: Some(true),
            ..BrowserResizeParams::default()
        })
        .expect("valid mobile resize params");
        assert_eq!(mobile.is_mobile, Some(true));

        assert!(
            validate_browser_resize_params(&BrowserResizeParams {
                width: Some(1280),
                operation: BrowserResizeOperation::Reset,
                ..BrowserResizeParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_resize_params(&BrowserResizeParams {
                is_mobile: Some(true),
                operation: BrowserResizeOperation::Reset,
                ..BrowserResizeParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_resize_params(&BrowserResizeParams {
                width: Some(0),
                height: Some(720),
                ..BrowserResizeParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_resize_params(&BrowserResizeParams {
                width: Some(1280),
                height: Some(720),
                device_scale_factor: Some(f64::INFINITY),
                ..BrowserResizeParams::default()
            })
            .is_err()
        );

        let reset = validate_browser_resize_params(&BrowserResizeParams {
            operation: BrowserResizeOperation::Reset,
            ..BrowserResizeParams::default()
        })
        .expect("valid reset params");
        assert_eq!(reset.operation, BrowserResizeOperation::Reset);
        assert_eq!(reset.is_mobile, None);
    }

    #[test]
    fn browser_resize_response_maps_viewport_readback() {
        let response = browser_resize_response(
            "session-1",
            0x2200,
            synapse_a11y::CdpViewportResult {
                endpoint: "ws://127.0.0.1/devtools/browser/1".to_owned(),
                cdp_target_id: "target-1".to_owned(),
                operation: "set".to_owned(),
                requested: Some(synapse_a11y::CdpViewportOverride {
                    width: 390,
                    height: 844,
                    device_scale_factor: 3.0,
                    mobile: false,
                }),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                readback: synapse_a11y::CdpViewportReadback {
                    inner_width: 390,
                    inner_height: 844,
                    device_pixel_ratio: 3.0,
                    screen_width: 390,
                    screen_height: 844,
                    outer_width: 390,
                    outer_height: 844,
                    visual_viewport_width: Some(390.0),
                    visual_viewport_height: Some(844.0),
                },
            },
        );

        assert_eq!(response.operation, BrowserResizeOperation::Set);
        assert_eq!(response.viewport.inner_width, 390);
        assert_eq!(response.viewport.device_pixel_ratio, 3.0);
        assert_eq!(
            response.requested.as_ref().map(|requested| requested.width),
            Some(390)
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_resize_bridge_response_maps_viewport_readback() {
        let response = browser_resize_bridge_response(
            "session-1",
            0x2200,
            crate::chrome_debugger_bridge::ChromeDebuggerViewportEmulationResult {
                extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
                target_id: "chrome-tab:44".to_owned(),
                tab_id: 44,
                chrome_window_id: Some(7),
                operation: "set".to_owned(),
                requested: Some(crate::chrome_debugger_bridge::ChromeDebuggerViewportOverride {
                    width: 390,
                    height: 844,
                    device_scale_factor: 3.0,
                    mobile: false,
                }),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                viewport: crate::chrome_debugger_bridge::ChromeDebuggerViewportReadback {
                    inner_width: 390,
                    inner_height: 844,
                    device_pixel_ratio: 3.0,
                    screen_width: 390,
                    screen_height: 844,
                    outer_width: 1024,
                    outer_height: 768,
                    visual_viewport_width: Some(390.0),
                    visual_viewport_height: Some(844.0),
                },
                readback_backend: "chrome.debugger.Emulation + chrome.scripting.executeScript"
                    .to_owned(),
                backend_tier_used: "chrome_tabs_extension_debugger".to_owned(),
                required_foreground: false,
                source_of_truth:
                    "normal Chrome bridge page script window.innerWidth/window.innerHeight/devicePixelRatio"
                        .to_owned(),
                method: "Emulation.setDeviceMetricsOverride".to_owned(),
                debugger_protocol_version: Some("1.3".to_owned()),
                target_candidate_count: 1,
                target_selection_reason: "target_id_hint".to_owned(),
            },
        );

        assert_eq!(response.transport, "chrome_tabs_extension");
        assert_eq!(response.endpoint, chrome_debugger_default_endpoint());
        assert_eq!(response.cdp_target_id, "chrome-tab:44");
        assert_eq!(response.operation, BrowserResizeOperation::Set);
        assert_eq!(response.viewport.inner_width, 390);
        assert_eq!(response.viewport.device_pixel_ratio, 3.0);
        assert_eq!(
            response
                .requested
                .as_ref()
                .map(|requested| requested.height),
            Some(844)
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_device_validation_edges() {
        let mobile = validate_browser_device_params(&BrowserDeviceParams {
            user_agent: Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Mobile/15E148".to_owned(),
            ),
            width: Some(390),
            height: Some(844),
            device_scale_factor: Some(3.0),
            is_mobile: Some(true),
            has_touch: Some(true),
            max_touch_points: None,
            ..BrowserDeviceParams::default()
        })
        .expect("valid mobile descriptor");
        let descriptor = mobile.descriptor.expect("descriptor");
        assert_eq!(descriptor.max_touch_points, 5);
        assert!(descriptor.is_mobile);
        assert!(descriptor.has_touch);

        assert!(
            validate_browser_device_params(&BrowserDeviceParams {
                operation: BrowserDeviceOperation::Reset,
                width: Some(390),
                ..BrowserDeviceParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_device_params(&BrowserDeviceParams {
                user_agent: Some(" bad ".to_owned()),
                width: Some(390),
                height: Some(844),
                ..BrowserDeviceParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_device_params(&BrowserDeviceParams {
                user_agent: Some("Desktop UA".to_owned()),
                width: Some(1280),
                height: Some(720),
                has_touch: Some(false),
                max_touch_points: Some(1),
                ..BrowserDeviceParams::default()
            })
            .is_err()
        );

        let reset = validate_browser_device_params(&BrowserDeviceParams {
            operation: BrowserDeviceOperation::Reset,
            ..BrowserDeviceParams::default()
        })
        .expect("valid reset");
        assert_eq!(reset.operation, BrowserDeviceOperation::Reset);
        assert!(reset.descriptor.is_none());
    }

    #[test]
    fn browser_device_response_maps_readback() {
        let response = browser_device_response(
            "session-1",
            0x2200,
            synapse_a11y::CdpDeviceResult {
                endpoint: "ws://127.0.0.1/devtools/browser/1".to_owned(),
                cdp_target_id: "target-1".to_owned(),
                operation: "set".to_owned(),
                descriptor: Some(synapse_a11y::CdpDeviceDescriptor {
                    user_agent: "Mobile UA".to_owned(),
                    width: 390,
                    height: 844,
                    device_scale_factor: 3.0,
                    is_mobile: true,
                    has_touch: true,
                    max_touch_points: 5,
                }),
                restored_user_agent: None,
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                readback: synapse_a11y::CdpDeviceReadback {
                    viewport: synapse_a11y::CdpViewportReadback {
                        inner_width: 390,
                        inner_height: 844,
                        device_pixel_ratio: 3.0,
                        screen_width: 390,
                        screen_height: 844,
                        outer_width: 390,
                        outer_height: 844,
                        visual_viewport_width: Some(390.0),
                        visual_viewport_height: Some(844.0),
                    },
                    user_agent: "Mobile UA".to_owned(),
                    max_touch_points: 5,
                    ontouchstart_available: true,
                    pointer_coarse: true,
                    any_pointer_coarse: true,
                    hover_none: true,
                    any_hover_none: true,
                },
            },
        );

        assert_eq!(response.operation, BrowserDeviceOperation::Set);
        assert_eq!(response.device.viewport.inner_width, 390);
        assert_eq!(response.device.user_agent, "Mobile UA");
        assert!(response.device.pointer_coarse);
        assert_eq!(
            response
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.max_touch_points),
            Some(5)
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_device_bridge_response_maps_readback() {
        let response = browser_device_bridge_response(
            "session-1",
            0x2200,
            crate::chrome_debugger_bridge::ChromeDebuggerDeviceEmulationResult {
                extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
                target_id: "chrome-tab:44".to_owned(),
                tab_id: 44,
                chrome_window_id: Some(7),
                operation: "set".to_owned(),
                descriptor: Some(
                    crate::chrome_debugger_bridge::ChromeDebuggerDeviceDescriptor {
                        user_agent: "Mobile UA".to_owned(),
                        width: 390,
                        height: 844,
                        device_scale_factor: 3.0,
                        is_mobile: true,
                        has_touch: true,
                        max_touch_points: 5,
                    },
                ),
                restored_user_agent: None,
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                device: crate::chrome_debugger_bridge::ChromeDebuggerDeviceReadback {
                    viewport: crate::chrome_debugger_bridge::ChromeDebuggerViewportReadback {
                        inner_width: 390,
                        inner_height: 844,
                        device_pixel_ratio: 3.0,
                        screen_width: 390,
                        screen_height: 844,
                        outer_width: 390,
                        outer_height: 844,
                        visual_viewport_width: Some(390.0),
                        visual_viewport_height: Some(844.0),
                    },
                    user_agent: "Mobile UA".to_owned(),
                    max_touch_points: 5,
                    ontouchstart_available: true,
                    pointer_coarse: true,
                    any_pointer_coarse: true,
                    hover_none: true,
                    any_hover_none: true,
                },
                readback_backend: "chrome.debugger.Emulation + chrome.scripting.executeScript"
                    .to_owned(),
                backend_tier_used: "chrome_tabs_extension_debugger".to_owned(),
                required_foreground: false,
                source_of_truth: "normal Chrome bridge page script".to_owned(),
                method: "Emulation.setUserAgentOverride".to_owned(),
                debugger_protocol_version: Some("1.3".to_owned()),
                target_candidate_count: 1,
                target_selection_reason: "chrome_tab_id_hint".to_owned(),
            },
        );

        assert_eq!(response.transport, "chrome_tabs_extension");
        assert_eq!(response.cdp_target_id, "chrome-tab:44");
        assert_eq!(response.operation, BrowserDeviceOperation::Set);
        assert_eq!(response.device.viewport.inner_width, 390);
        assert_eq!(response.device.user_agent, "Mobile UA");
        assert!(response.device.pointer_coarse);
        assert_eq!(
            response
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.max_touch_points),
            Some(5)
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_geolocation_bridge_response_maps_readback() {
        let response = browser_geolocation_bridge_response(
            "session-1",
            0x2200,
            crate::chrome_debugger_bridge::ChromeDebuggerGeolocationEmulationResult {
                extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
                target_id: "chrome-tab:45".to_owned(),
                tab_id: 45,
                chrome_window_id: Some(7),
                operation: "set".to_owned(),
                origin: "http://127.0.0.1:8769".to_owned(),
                requested: Some(crate::chrome_debugger_bridge::ChromeDebuggerGeolocationOverride {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    accuracy: 12.0,
                    altitude: None,
                    altitude_accuracy: None,
                    heading: None,
                    speed: None,
                }),
                permission_setting: "granted".to_owned(),
                page_url: "http://127.0.0.1:8769/".to_owned(),
                page_title: "Geo".to_owned(),
                ready_state: "complete".to_owned(),
                geolocation: crate::chrome_debugger_bridge::ChromeDebuggerGeolocationReadback {
                    permission_state: "granted".to_owned(),
                    position: Some(
                        crate::chrome_debugger_bridge::ChromeDebuggerGeolocationCoordinatesReadback {
                            latitude: 37.7749,
                            longitude: -122.4194,
                            accuracy: 12.0,
                            altitude: None,
                            altitude_accuracy: None,
                            heading: None,
                            speed: None,
                            timestamp: 123.0,
                        },
                    ),
                    error: None,
                },
                readback_backend: "chrome.debugger.Emulation + chrome.scripting.executeScript"
                    .to_owned(),
                backend_tier_used: "chrome_tabs_extension_debugger".to_owned(),
                required_foreground: false,
                source_of_truth: "normal Chrome bridge page script".to_owned(),
                method: "Emulation.setGeolocationOverride".to_owned(),
                debugger_protocol_version: Some("1.3".to_owned()),
                target_candidate_count: 1,
                target_selection_reason: "chrome_tab_id_hint".to_owned(),
            },
        );

        assert_eq!(response.transport, "chrome_tabs_extension");
        assert_eq!(response.cdp_target_id, "chrome-tab:45");
        assert_eq!(response.operation, BrowserGeolocationOperation::Set);
        assert_eq!(response.permission_setting, "granted");
        assert_eq!(response.geolocation.permission_state, "granted");
        assert_eq!(
            response
                .geolocation
                .position
                .as_ref()
                .map(|position| position.latitude),
            Some(37.7749)
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_geolocation_validation_edges() {
        let granted = validate_browser_geolocation_params(&BrowserGeolocationParams {
            latitude: Some(37.7749),
            longitude: Some(-122.4194),
            accuracy: None,
            grant_permission: None,
            ..BrowserGeolocationParams::default()
        })
        .expect("valid geolocation override");
        let geolocation = granted.geolocation.expect("geolocation");
        assert_eq!(geolocation.accuracy, 0.0);
        assert!(granted.grant_permission);

        let denied = validate_browser_geolocation_params(&BrowserGeolocationParams {
            latitude: Some(37.7749),
            longitude: Some(-122.4194),
            grant_permission: Some(false),
            ..BrowserGeolocationParams::default()
        })
        .expect("valid denied geolocation override");
        assert!(!denied.grant_permission);

        assert!(
            validate_browser_geolocation_params(&BrowserGeolocationParams {
                operation: BrowserGeolocationOperation::Reset,
                latitude: Some(37.7749),
                ..BrowserGeolocationParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_geolocation_params(&BrowserGeolocationParams {
                latitude: Some(91.0),
                longitude: Some(-122.4194),
                ..BrowserGeolocationParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_geolocation_params(&BrowserGeolocationParams {
                latitude: Some(37.7749),
                longitude: Some(-181.0),
                ..BrowserGeolocationParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_geolocation_params(&BrowserGeolocationParams {
                latitude: Some(37.7749),
                longitude: Some(-122.4194),
                heading: Some(361.0),
                ..BrowserGeolocationParams::default()
            })
            .is_err()
        );

        let reset = validate_browser_geolocation_params(&BrowserGeolocationParams {
            operation: BrowserGeolocationOperation::Reset,
            ..BrowserGeolocationParams::default()
        })
        .expect("valid reset");
        assert_eq!(reset.operation, BrowserGeolocationOperation::Reset);
        assert!(reset.geolocation.is_none());
    }

    #[test]
    fn browser_geolocation_response_maps_readback() {
        let response = browser_geolocation_response(
            "session-1",
            0x2200,
            synapse_a11y::CdpGeolocationResult {
                endpoint: "ws://127.0.0.1/devtools/browser/1".to_owned(),
                cdp_target_id: "target-1".to_owned(),
                operation: "set".to_owned(),
                origin: "https://example.test".to_owned(),
                requested: Some(synapse_a11y::CdpGeolocationOverride {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    accuracy: 9.0,
                    altitude: None,
                    altitude_accuracy: None,
                    heading: Some(180.0),
                    speed: Some(1.25),
                }),
                permission_setting: "granted".to_owned(),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                readback: synapse_a11y::CdpGeolocationReadback {
                    permission_state: "granted".to_owned(),
                    position: Some(synapse_a11y::CdpGeolocationCoordinatesReadback {
                        latitude: 37.7749,
                        longitude: -122.4194,
                        accuracy: 9.0,
                        altitude: None,
                        altitude_accuracy: None,
                        heading: Some(180.0),
                        speed: Some(1.25),
                        timestamp: 123.0,
                    }),
                    error: None,
                },
            },
        );

        assert_eq!(response.operation, BrowserGeolocationOperation::Set);
        assert_eq!(response.origin, "https://example.test");
        assert_eq!(response.permission_setting, "granted");
        assert_eq!(response.geolocation.permission_state, "granted");
        assert_eq!(
            response
                .geolocation
                .position
                .as_ref()
                .map(|position| position.latitude),
            Some(37.7749)
        );
        assert!(response.geolocation.error.is_none());
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_locale_validation_edges() {
        let both = validate_browser_locale_params(&BrowserLocaleParams {
            locale: Some("fr_FR".to_owned()),
            timezone_id: Some("Europe/Paris".to_owned()),
            ..BrowserLocaleParams::default()
        })
        .expect("valid locale/timezone override");
        let requested = both.requested.expect("requested override");
        assert_eq!(requested.locale.as_deref(), Some("fr_FR"));
        assert_eq!(requested.timezone_id.as_deref(), Some("Europe/Paris"));

        assert!(
            validate_browser_locale_params(&BrowserLocaleParams {
                ..BrowserLocaleParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_locale_params(&BrowserLocaleParams {
                locale: Some(" fr_FR ".to_owned()),
                ..BrowserLocaleParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_locale_params(&BrowserLocaleParams {
                timezone_id: Some("Europe Paris".to_owned()),
                ..BrowserLocaleParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_locale_params(&BrowserLocaleParams {
                operation: BrowserLocaleOperation::Reset,
                timezone_id: Some("Europe/Paris".to_owned()),
                ..BrowserLocaleParams::default()
            })
            .is_err()
        );

        let reset = validate_browser_locale_params(&BrowserLocaleParams {
            operation: BrowserLocaleOperation::Reset,
            ..BrowserLocaleParams::default()
        })
        .expect("valid reset");
        assert_eq!(reset.operation, BrowserLocaleOperation::Reset);
        assert!(reset.requested.is_none());
    }

    #[test]
    fn browser_locale_response_maps_readback() {
        let response = browser_locale_response(
            "session-1",
            0x2200,
            synapse_a11y::CdpLocaleTimezoneResult {
                endpoint: "ws://127.0.0.1/devtools/browser/1".to_owned(),
                cdp_target_id: "target-1".to_owned(),
                operation: "set".to_owned(),
                requested: Some(synapse_a11y::CdpLocaleTimezoneOverride {
                    locale: Some("fr_FR".to_owned()),
                    timezone_id: Some("Europe/Paris".to_owned()),
                }),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                readback: synapse_a11y::CdpLocaleTimezoneReadback {
                    locale: "fr-FR".to_owned(),
                    calendar: "gregory".to_owned(),
                    numbering_system: "latn".to_owned(),
                    time_zone: "Europe/Paris".to_owned(),
                    sample_number: "1 234 567,89".to_owned(),
                    sample_date: "jeudi 2 janvier 2020 a 04:04:05 UTC+1".to_owned(),
                    date_string: "Thu Jan 02 2020 04:04:05 GMT+0100".to_owned(),
                    timezone_offset_minutes: -60,
                },
            },
        );

        assert_eq!(response.operation, BrowserLocaleOperation::Set);
        assert_eq!(response.locale.locale, "fr-FR");
        assert_eq!(response.locale.time_zone, "Europe/Paris");
        assert_eq!(
            response
                .requested
                .as_ref()
                .and_then(|requested| requested.timezone_id.as_deref()),
            Some("Europe/Paris")
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_locale_bridge_response_maps_readback() {
        let response = browser_locale_bridge_response(
            "session-1",
            0x2200,
            crate::chrome_debugger_bridge::ChromeDebuggerLocaleEmulationResult {
                extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
                target_id: "chrome-tab:100".to_owned(),
                tab_id: 100,
                chrome_window_id: Some(200),
                operation: "set".to_owned(),
                requested: Some(
                    crate::chrome_debugger_bridge::ChromeDebuggerLocaleTimezoneOverride {
                        locale: Some("fr_FR".to_owned()),
                        timezone_id: Some("Europe/Paris".to_owned()),
                    },
                ),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                locale: crate::chrome_debugger_bridge::ChromeDebuggerLocaleTimezoneReadback {
                    locale: "fr-FR".to_owned(),
                    calendar: "gregory".to_owned(),
                    numbering_system: "latn".to_owned(),
                    time_zone: "Europe/Paris".to_owned(),
                    sample_number: "1 234 567,89".to_owned(),
                    sample_date: "jeudi 2 janvier 2020 a 04:04:05 UTC+1".to_owned(),
                    date_string: "Thu Jan 02 2020 04:04:05 GMT+0100".to_owned(),
                    timezone_offset_minutes: -60,
                },
                readback_backend:
                    "chrome.debugger.Emulation locale/timezone + chrome.scripting.executeScript"
                        .to_owned(),
                backend_tier_used: "chrome_tabs_extension_debugger".to_owned(),
                required_foreground: false,
                source_of_truth: "normal Chrome bridge page script Intl/Date".to_owned(),
                method: "Emulation.setLocaleOverride+Emulation.setTimezoneOverride".to_owned(),
                debugger_protocol_version: Some("1.3".to_owned()),
                target_candidate_count: 1,
                target_selection_reason: "chrome_tab_id_hint".to_owned(),
            },
        );

        assert_eq!(response.transport, "chrome_tabs_extension");
        assert_eq!(response.cdp_target_id, "chrome-tab:100");
        assert_eq!(response.operation, BrowserLocaleOperation::Set);
        assert_eq!(response.locale.locale, "fr-FR");
        assert_eq!(response.locale.time_zone, "Europe/Paris");
        assert_eq!(response.backend_tier_used, "chrome_tabs_extension_debugger");
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_media_validation_edges() {
        let full = validate_browser_media_params(&BrowserMediaParams {
            media: Some("print".to_owned()),
            color_scheme: Some("dark".to_owned()),
            reduced_motion: Some("reduce".to_owned()),
            ..BrowserMediaParams::default()
        })
        .expect("valid media override");
        let requested = full.requested.expect("requested override");
        assert_eq!(requested.media.as_deref(), Some("print"));
        assert_eq!(requested.color_scheme.as_deref(), Some("dark"));
        assert_eq!(requested.reduced_motion.as_deref(), Some("reduce"));

        assert!(
            validate_browser_media_params(&BrowserMediaParams {
                ..BrowserMediaParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_media_params(&BrowserMediaParams {
                media: Some("tv".to_owned()),
                ..BrowserMediaParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_media_params(&BrowserMediaParams {
                color_scheme: Some("sepia".to_owned()),
                ..BrowserMediaParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_media_params(&BrowserMediaParams {
                reduced_motion: Some("always".to_owned()),
                ..BrowserMediaParams::default()
            })
            .is_err()
        );
        assert!(
            validate_browser_media_params(&BrowserMediaParams {
                operation: BrowserMediaOperation::Reset,
                media: Some("print".to_owned()),
                ..BrowserMediaParams::default()
            })
            .is_err()
        );

        let reset = validate_browser_media_params(&BrowserMediaParams {
            operation: BrowserMediaOperation::Reset,
            ..BrowserMediaParams::default()
        })
        .expect("valid reset");
        assert_eq!(reset.operation, BrowserMediaOperation::Reset);
        assert!(reset.requested.is_none());
    }

    #[test]
    fn browser_media_response_maps_readback() {
        let response = browser_media_response(
            "session-1",
            0x2200,
            synapse_a11y::CdpMediaResult {
                endpoint: "ws://127.0.0.1/devtools/browser/1".to_owned(),
                cdp_target_id: "target-1".to_owned(),
                operation: "set".to_owned(),
                requested: Some(synapse_a11y::CdpMediaOverride {
                    media: Some("print".to_owned()),
                    color_scheme: Some("dark".to_owned()),
                    reduced_motion: Some("reduce".to_owned()),
                }),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                readback: synapse_a11y::CdpMediaReadback {
                    media_screen: false,
                    media_print: true,
                    color_scheme_dark: true,
                    color_scheme_light: false,
                    color_scheme_no_preference: false,
                    reduced_motion_reduce: true,
                    reduced_motion_no_preference: false,
                },
            },
        );

        assert_eq!(response.operation, BrowserMediaOperation::Set);
        assert!(response.media.media_print);
        assert!(response.media.color_scheme_dark);
        assert!(response.media.reduced_motion_reduce);
        assert_eq!(
            response
                .requested
                .as_ref()
                .and_then(|requested| requested.media.as_deref()),
            Some("print")
        );
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_media_bridge_response_maps_readback() {
        let response = browser_media_bridge_response(
            "session-1",
            0x2200,
            crate::chrome_debugger_bridge::ChromeDebuggerMediaEmulationResult {
                extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
                target_id: "chrome-tab:7".to_owned(),
                tab_id: 7,
                chrome_window_id: Some(3),
                operation: "set".to_owned(),
                requested: Some(crate::chrome_debugger_bridge::ChromeDebuggerMediaOverride {
                    media: Some("print".to_owned()),
                    color_scheme: Some("dark".to_owned()),
                    reduced_motion: Some("reduce".to_owned()),
                }),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                media: crate::chrome_debugger_bridge::ChromeDebuggerMediaReadback {
                    media_screen: false,
                    media_print: true,
                    color_scheme_dark: true,
                    color_scheme_light: false,
                    color_scheme_no_preference: false,
                    reduced_motion_reduce: true,
                    reduced_motion_no_preference: false,
                },
                readback_backend: String::new(),
                backend_tier_used: String::new(),
                required_foreground: false,
                source_of_truth: String::new(),
                method: "Emulation.setEmulatedMedia(media,features)".to_owned(),
                debugger_protocol_version: Some("1.3".to_owned()),
                target_candidate_count: 1,
                target_selection_reason: "targetIdHint".to_owned(),
            },
        );

        assert_eq!(response.transport, "chrome_tabs_extension");
        assert_eq!(response.operation, BrowserMediaOperation::Set);
        assert!(response.media.media_print);
        assert!(response.media.color_scheme_dark);
        assert!(response.media.reduced_motion_reduce);
        assert_eq!(
            response
                .requested
                .as_ref()
                .and_then(|requested| requested.media.as_deref()),
            Some("print")
        );
        assert_eq!(response.backend_tier_used, "chrome_tabs_extension_debugger");
        assert!(!response.required_foreground);
    }
}
