//! Unified browser emulation tool (#1179).

use std::collections::BTreeSet;

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
use serde_json::{Value, json};
use synapse_core::error_codes;

const TOOL: &str = "browser_emulate";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
const DEFAULT_BRIDGE_WAIT_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEmulateOperation {
    #[default]
    Set,
    Reset,
}

#[derive(
    Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEmulateDomain {
    Viewport,
    Device,
    Geolocation,
    Locale,
    Media,
    Network,
}

impl BrowserEmulateDomain {
    fn all() -> Vec<Self> {
        vec![
            Self::Viewport,
            Self::Device,
            Self::Geolocation,
            Self::Locale,
            Self::Media,
            Self::Network,
        ]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Viewport => "viewport",
            Self::Device => "device",
            Self::Geolocation => "geolocation",
            Self::Locale => "locale",
            Self::Media => "media",
            Self::Network => "network",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateParams {
    /// CDP TargetID to emulate. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// `set` applies one or more nested overrides; `reset` clears selected
    /// domains. For reset, an empty `domains` list means all domains.
    #[serde(default)]
    pub operation: BrowserEmulateOperation,
    /// Domains to reset for operation=reset. Empty means all domains.
    #[serde(default)]
    pub domains: Vec<BrowserEmulateDomain>,
    /// Viewport/DPR override. Cannot be combined with `device`.
    #[serde(default)]
    pub viewport: Option<BrowserEmulateViewport>,
    /// Device descriptor override. Includes viewport metrics and User-Agent.
    #[serde(default)]
    pub device: Option<BrowserEmulateDevice>,
    /// Geolocation override and origin-scoped permission.
    #[serde(default)]
    pub geolocation: Option<BrowserEmulateGeolocation>,
    /// Locale and/or timezone override.
    #[serde(default)]
    pub locale: Option<BrowserEmulateLocale>,
    /// CSS media type and media-feature override.
    #[serde(default)]
    pub media: Option<BrowserEmulateMedia>,
    /// Offline/throttled network conditions override.
    #[serde(default)]
    pub network: Option<BrowserEmulateNetwork>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateViewport {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub device_scale_factor: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateDevice {
    pub user_agent: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub device_scale_factor: Option<f64>,
    #[serde(default)]
    pub is_mobile: Option<bool>,
    #[serde(default)]
    pub has_touch: Option<bool>,
    #[serde(default)]
    pub max_touch_points: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateGeolocation {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: Option<f64>,
    #[serde(default)]
    pub altitude: Option<f64>,
    #[serde(default)]
    pub altitude_accuracy: Option<f64>,
    #[serde(default)]
    pub heading: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub grant_permission: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateLocale {
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub timezone_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateMedia {
    #[serde(default)]
    pub media: Option<String>,
    #[serde(default)]
    pub color_scheme: Option<String>,
    #[serde(default)]
    pub reduced_motion: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateNetwork {
    #[serde(default)]
    pub offline: Option<bool>,
    #[serde(default)]
    pub latency_ms: Option<f64>,
    #[serde(default)]
    pub download_throughput_bytes_per_sec: Option<f64>,
    #[serde(default)]
    pub upload_throughput_bytes_per_sec: Option<f64>,
    #[serde(default)]
    pub connection_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateDomainResult {
    pub domain: BrowserEmulateDomain,
    pub operation: BrowserEmulateOperation,
    pub result: Value,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEmulateResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserEmulateOperation,
    pub domains: Vec<BrowserEmulateDomain>,
    pub results: Vec<BrowserEmulateDomainResult>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Debug)]
struct NormalizedBrowserEmulateParams {
    operation: BrowserEmulateOperation,
    domains: Vec<BrowserEmulateDomain>,
    viewport: Option<BrowserEmulateViewport>,
    device: Option<BrowserEmulateDevice>,
    geolocation: Option<BrowserEmulateGeolocation>,
    locale: Option<BrowserEmulateLocale>,
    media: Option<BrowserEmulateMedia>,
    network: Option<BrowserEmulateNetwork>,
}

#[tool_router(router = browser_emulate_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Set or reset multiple target-scoped browser emulation overrides in one call for the calling session's owned browser tab. Raw CDP targets use the CDP emulation domains directly; normal Chrome bridge chrome-tab:* targets use the already-open authenticated Chrome profile's viewportEmulation, deviceEmulation, geolocationEmulation, localeEmulation, mediaEmulation, and networkConditions lanes with same-target MAIN-world readback. operation=set accepts nested viewport, device, geolocation, locale, media, and network sections and returns each domain's same-target readback; viewport and device are mutually exclusive because device includes viewport metrics. operation=reset clears the listed domains, or all domains when domains is empty. Target-scoped and foreground-capable without activating the human OS foreground tab; never falls back to the human foreground tab. Use page text/evaluate/network readback as independent manual FSV evidence."
    )]
    pub async fn browser_emulate(
        &self,
        params: Parameters<BrowserEmulateParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserEmulateResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_emulate"
        );
        let session_id = require_target_session_id(&request_context)?;
        let emulate = validate_browser_emulate_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": emulate.operation,
            "domains": &emulate.domains,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": emulate.operation,
            "domains": &emulate.domains,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_emulate_impl(&session_id, window_hwnd, &cdp_target_id, &emulate)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_emulate_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserEmulateParams,
    ) -> Result<BrowserEmulateResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_emulate_bridge_impl(session_id, window_hwnd, cdp_target_id, params)
                    .await;
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        super::operator_panic_boundary::ensure_mcp_mutation(
            "browser_emulate_before_raw_domain_command",
        )?;
        let mut results = Vec::new();
        match params.operation {
            BrowserEmulateOperation::Set => {
                if let Some(viewport) = params.viewport.as_ref() {
                    let result = synapse_a11y::cdp_set_viewport_size(
                        &endpoint,
                        cdp_target_id,
                        viewport.width,
                        viewport.height,
                        viewport.device_scale_factor.unwrap_or(1.0),
                    )
                    .await
                    .map_err(|error| emulate_error(BrowserEmulateDomain::Viewport, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_viewport_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Viewport,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(device) = params.device.as_ref() {
                    let has_touch = device.has_touch.unwrap_or(false);
                    let result = synapse_a11y::cdp_apply_device_descriptor(
                        &endpoint,
                        cdp_target_id,
                        synapse_a11y::CdpDeviceDescriptor {
                            user_agent: device.user_agent.clone(),
                            width: device.width,
                            height: device.height,
                            device_scale_factor: device.device_scale_factor.unwrap_or(1.0),
                            is_mobile: device.is_mobile.unwrap_or(false),
                            has_touch,
                            max_touch_points: device.max_touch_points.unwrap_or(if has_touch {
                                5
                            } else {
                                0
                            }),
                        },
                    )
                    .await
                    .map_err(|error| emulate_error(BrowserEmulateDomain::Device, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_device_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Device,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(geolocation) = params.geolocation.as_ref() {
                    let result = synapse_a11y::cdp_set_geolocation_override(
                        &endpoint,
                        cdp_target_id,
                        synapse_a11y::CdpGeolocationOverride {
                            latitude: geolocation.latitude,
                            longitude: geolocation.longitude,
                            accuracy: geolocation.accuracy.unwrap_or(0.0),
                            altitude: geolocation.altitude,
                            altitude_accuracy: geolocation.altitude_accuracy,
                            heading: geolocation.heading,
                            speed: geolocation.speed,
                        },
                        geolocation.grant_permission.unwrap_or(true),
                    )
                    .await
                    .map_err(|error| emulate_error(BrowserEmulateDomain::Geolocation, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_geolocation_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Geolocation,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(locale) = params.locale.as_ref() {
                    let result = synapse_a11y::cdp_set_locale_timezone_override(
                        &endpoint,
                        cdp_target_id,
                        synapse_a11y::CdpLocaleTimezoneOverride {
                            locale: locale.locale.clone(),
                            timezone_id: locale.timezone_id.clone(),
                        },
                    )
                    .await
                    .map_err(|error| emulate_error(BrowserEmulateDomain::Locale, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_locale_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Locale,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(media) = params.media.as_ref() {
                    let result = synapse_a11y::cdp_set_media_override(
                        &endpoint,
                        cdp_target_id,
                        synapse_a11y::CdpMediaOverride {
                            media: media.media.clone(),
                            color_scheme: media.color_scheme.clone(),
                            reduced_motion: media.reduced_motion.clone(),
                        },
                    )
                    .await
                    .map_err(|error| emulate_error(BrowserEmulateDomain::Media, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_media_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Media,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(network) = params.network.as_ref() {
                    let result = synapse_a11y::cdp_set_network_conditions(
                        &endpoint,
                        cdp_target_id,
                        synapse_a11y::CdpNetworkConditionsOverride {
                            offline: network.offline.unwrap_or(false),
                            latency_ms: network.latency_ms.unwrap_or(0.0),
                            download_throughput_bytes_per_sec: network
                                .download_throughput_bytes_per_sec
                                .unwrap_or(-1.0),
                            upload_throughput_bytes_per_sec: network
                                .upload_throughput_bytes_per_sec
                                .unwrap_or(-1.0),
                            connection_type: network.connection_type.clone(),
                        },
                    )
                    .await
                    .map_err(|error| emulate_error(BrowserEmulateDomain::Network, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_network_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Network,
                        params.operation,
                        result,
                    )?);
                }
            }
            BrowserEmulateOperation::Reset => {
                for domain in &params.domains {
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_before_raw_reset_domain",
                    )?;
                    match domain {
                        BrowserEmulateDomain::Viewport => {
                            let result =
                                synapse_a11y::cdp_reset_viewport_size(&endpoint, cdp_target_id)
                                    .await
                                    .map_err(|error| emulate_error(*domain, error))?;
                            results.push(domain_result(*domain, params.operation, result)?);
                        }
                        BrowserEmulateDomain::Device => {
                            let result =
                                synapse_a11y::cdp_reset_device_descriptor(&endpoint, cdp_target_id)
                                    .await
                                    .map_err(|error| emulate_error(*domain, error))?;
                            results.push(domain_result(*domain, params.operation, result)?);
                        }
                        BrowserEmulateDomain::Geolocation => {
                            let result = synapse_a11y::cdp_reset_geolocation_override(
                                &endpoint,
                                cdp_target_id,
                            )
                            .await
                            .map_err(|error| emulate_error(*domain, error))?;
                            results.push(domain_result(*domain, params.operation, result)?);
                        }
                        BrowserEmulateDomain::Locale => {
                            let result = synapse_a11y::cdp_reset_locale_timezone_override(
                                &endpoint,
                                cdp_target_id,
                            )
                            .await
                            .map_err(|error| emulate_error(*domain, error))?;
                            results.push(domain_result(*domain, params.operation, result)?);
                        }
                        BrowserEmulateDomain::Media => {
                            let result =
                                synapse_a11y::cdp_reset_media_override(&endpoint, cdp_target_id)
                                    .await
                                    .map_err(|error| emulate_error(*domain, error))?;
                            results.push(domain_result(*domain, params.operation, result)?);
                        }
                        BrowserEmulateDomain::Network => {
                            let result = synapse_a11y::cdp_reset_network_conditions(
                                &endpoint,
                                cdp_target_id,
                            )
                            .await
                            .map_err(|error| emulate_error(*domain, error))?;
                            results.push(domain_result(*domain, params.operation, result)?);
                        }
                    }
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_raw_reset_domain",
                    )?;
                }
            }
        }

        tracing::info!(
            code = "CDP_BACKGROUND_BROWSER_EMULATE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            domain_count = params.domains.len(),
            "readback=browser_emulate per-domain same-target readbacks outcome=emulation_returned"
        );
        Ok(BrowserEmulateResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            operation: params.operation,
            domains: params.domains.clone(),
            results,
            readback_backend: "raw CDP domain command + same-target Runtime.evaluate readback"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_emulate_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserEmulateParams,
    ) -> Result<BrowserEmulateResponse, ErrorData> {
        super::operator_panic_boundary::ensure_mcp_mutation(
            "browser_emulate_before_bridge_domain_command",
        )?;
        let mut results = Vec::new();
        match params.operation {
            BrowserEmulateOperation::Set => {
                if let Some(viewport) = params.viewport.as_ref() {
                    let result = crate::chrome_debugger_bridge::viewport_emulation(
                        crate::chrome_debugger_bridge::ChromeDebuggerViewportEmulationRequest {
                            hwnd: window_hwnd,
                            target_id: cdp_target_id,
                            operation: "set",
                            width: Some(viewport.width),
                            height: Some(viewport.height),
                            device_scale_factor: viewport.device_scale_factor,
                            is_mobile: None,
                            wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                        },
                    )
                    .await
                    .map_err(|error| emulate_bridge_error(BrowserEmulateDomain::Viewport, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_viewport_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Viewport,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(device) = params.device.as_ref() {
                    let has_touch = device.has_touch.unwrap_or(false);
                    let result = crate::chrome_debugger_bridge::device_emulation(
                        crate::chrome_debugger_bridge::ChromeDebuggerDeviceEmulationRequest {
                            hwnd: window_hwnd,
                            target_id: cdp_target_id,
                            operation: "set",
                            user_agent: Some(device.user_agent.as_str()),
                            width: Some(device.width),
                            height: Some(device.height),
                            device_scale_factor: Some(device.device_scale_factor.unwrap_or(1.0)),
                            is_mobile: Some(device.is_mobile.unwrap_or(false)),
                            has_touch: Some(has_touch),
                            max_touch_points: Some(
                                device
                                    .max_touch_points
                                    .unwrap_or(if has_touch { 5 } else { 0 }),
                            ),
                            wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                        },
                    )
                    .await
                    .map_err(|error| emulate_bridge_error(BrowserEmulateDomain::Device, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_device_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Device,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(geolocation) = params.geolocation.as_ref() {
                    let result = crate::chrome_debugger_bridge::geolocation_emulation(
                        crate::chrome_debugger_bridge::ChromeDebuggerGeolocationEmulationRequest {
                            hwnd: window_hwnd,
                            target_id: cdp_target_id.to_owned(),
                            operation: "set".to_owned(),
                            latitude: Some(geolocation.latitude),
                            longitude: Some(geolocation.longitude),
                            accuracy: Some(geolocation.accuracy.unwrap_or(0.0)),
                            altitude: geolocation.altitude,
                            altitude_accuracy: geolocation.altitude_accuracy,
                            heading: geolocation.heading,
                            speed: geolocation.speed,
                            grant_permission: Some(geolocation.grant_permission.unwrap_or(true)),
                            wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                        },
                    )
                    .await
                    .map_err(|error| {
                        emulate_bridge_error(BrowserEmulateDomain::Geolocation, error)
                    })?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_geolocation_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Geolocation,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(locale) = params.locale.as_ref() {
                    let result = crate::chrome_debugger_bridge::locale_emulation(
                        crate::chrome_debugger_bridge::ChromeDebuggerLocaleEmulationRequest {
                            hwnd: window_hwnd,
                            target_id: cdp_target_id,
                            operation: "set",
                            locale: locale.locale.as_deref(),
                            timezone_id: locale.timezone_id.as_deref(),
                            wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                        },
                    )
                    .await
                    .map_err(|error| emulate_bridge_error(BrowserEmulateDomain::Locale, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_locale_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Locale,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(media) = params.media.as_ref() {
                    let result = crate::chrome_debugger_bridge::media_emulation(
                        crate::chrome_debugger_bridge::ChromeDebuggerMediaEmulationRequest {
                            hwnd: window_hwnd,
                            target_id: cdp_target_id,
                            operation: "set",
                            media: media.media.as_deref(),
                            color_scheme: media.color_scheme.as_deref(),
                            reduced_motion: media.reduced_motion.as_deref(),
                            wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                        },
                    )
                    .await
                    .map_err(|error| emulate_bridge_error(BrowserEmulateDomain::Media, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_media_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Media,
                        params.operation,
                        result,
                    )?);
                }
                if let Some(network) = params.network.as_ref() {
                    let result = crate::chrome_debugger_bridge::network_conditions(
                        crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsRequest {
                            hwnd: window_hwnd,
                            target_id: cdp_target_id,
                            operation: "set",
                            offline: Some(network.offline.unwrap_or(false)),
                            latency_ms: Some(network.latency_ms.unwrap_or(0.0)),
                            download_throughput_bytes_per_sec: Some(
                                network.download_throughput_bytes_per_sec.unwrap_or(-1.0),
                            ),
                            upload_throughput_bytes_per_sec: Some(
                                network.upload_throughput_bytes_per_sec.unwrap_or(-1.0),
                            ),
                            connection_type: network.connection_type.as_deref(),
                            wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                        },
                    )
                    .await
                    .map_err(|error| emulate_bridge_error(BrowserEmulateDomain::Network, error))?;
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_network_command",
                    )?;
                    results.push(domain_result(
                        BrowserEmulateDomain::Network,
                        params.operation,
                        result,
                    )?);
                }
            }
            BrowserEmulateOperation::Reset => {
                for domain in &params.domains {
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_before_bridge_reset_domain",
                    )?;
                    let result = match domain {
                        BrowserEmulateDomain::Viewport => domain_result(
                            *domain,
                            params.operation,
                            crate::chrome_debugger_bridge::viewport_emulation(
                                crate::chrome_debugger_bridge::ChromeDebuggerViewportEmulationRequest {
                                    hwnd: window_hwnd,
                                    target_id: cdp_target_id,
                                    operation: "reset",
                                    width: None,
                                    height: None,
                                    device_scale_factor: None,
                                    is_mobile: None,
                                    wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                                },
                            )
                            .await
                            .map_err(|error| emulate_bridge_error(*domain, error))?,
                        )?,
                        BrowserEmulateDomain::Device => domain_result(
                            *domain,
                            params.operation,
                            crate::chrome_debugger_bridge::device_emulation(
                                crate::chrome_debugger_bridge::ChromeDebuggerDeviceEmulationRequest {
                                    hwnd: window_hwnd,
                                    target_id: cdp_target_id,
                                    operation: "reset",
                                    user_agent: None,
                                    width: None,
                                    height: None,
                                    device_scale_factor: None,
                                    is_mobile: None,
                                    has_touch: None,
                                    max_touch_points: None,
                                    wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                                },
                            )
                            .await
                            .map_err(|error| emulate_bridge_error(*domain, error))?,
                        )?,
                        BrowserEmulateDomain::Geolocation => domain_result(
                            *domain,
                            params.operation,
                            crate::chrome_debugger_bridge::geolocation_emulation(
                                crate::chrome_debugger_bridge::ChromeDebuggerGeolocationEmulationRequest {
                                    hwnd: window_hwnd,
                                    target_id: cdp_target_id.to_owned(),
                                    operation: "reset".to_owned(),
                                    latitude: None,
                                    longitude: None,
                                    accuracy: None,
                                    altitude: None,
                                    altitude_accuracy: None,
                                    heading: None,
                                    speed: None,
                                    grant_permission: None,
                                    wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                                },
                            )
                            .await
                            .map_err(|error| emulate_bridge_error(*domain, error))?,
                        )?,
                        BrowserEmulateDomain::Locale => domain_result(
                            *domain,
                            params.operation,
                            crate::chrome_debugger_bridge::locale_emulation(
                                crate::chrome_debugger_bridge::ChromeDebuggerLocaleEmulationRequest {
                                    hwnd: window_hwnd,
                                    target_id: cdp_target_id,
                                    operation: "reset",
                                    locale: None,
                                    timezone_id: None,
                                    wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                                },
                            )
                            .await
                            .map_err(|error| emulate_bridge_error(*domain, error))?,
                        )?,
                        BrowserEmulateDomain::Media => domain_result(
                            *domain,
                            params.operation,
                            crate::chrome_debugger_bridge::media_emulation(
                                crate::chrome_debugger_bridge::ChromeDebuggerMediaEmulationRequest {
                                    hwnd: window_hwnd,
                                    target_id: cdp_target_id,
                                    operation: "reset",
                                    media: None,
                                    color_scheme: None,
                                    reduced_motion: None,
                                    wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                                },
                            )
                            .await
                            .map_err(|error| emulate_bridge_error(*domain, error))?,
                        )?,
                        BrowserEmulateDomain::Network => domain_result(
                            *domain,
                            params.operation,
                            crate::chrome_debugger_bridge::network_conditions(
                                crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsRequest {
                                    hwnd: window_hwnd,
                                    target_id: cdp_target_id,
                                    operation: "reset",
                                    offline: None,
                                    latency_ms: None,
                                    download_throughput_bytes_per_sec: None,
                                    upload_throughput_bytes_per_sec: None,
                                    connection_type: None,
                                    wait_timeout_ms: DEFAULT_BRIDGE_WAIT_TIMEOUT_MS,
                                },
                            )
                            .await
                            .map_err(|error| emulate_bridge_error(*domain, error))?,
                        )?,
                    };
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_emulate_after_bridge_reset_domain",
                    )?;
                    results.push(result);
                }
            }
        }

        tracing::info!(
            code = "CHROME_BRIDGE_BROWSER_EMULATE",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?params.operation,
            domain_count = params.domains.len(),
            "readback=normal_bridge_per_domain_same_target_readbacks outcome=emulation_returned"
        );
        Ok(BrowserEmulateResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "chrome_tabs_extension".to_owned(),
            endpoint: chrome_debugger_default_endpoint(),
            cdp_target_id: cdp_target_id.to_owned(),
            operation: params.operation,
            domains: params.domains.clone(),
            results,
            readback_backend:
                "normal Chrome bridge per-domain debugger lanes + MAIN-world readback".to_owned(),
            backend_tier_used: "chrome_tabs_extension_debugger".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_emulate_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserEmulateParams,
    ) -> Result<BrowserEmulateResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_emulate is only available on Windows in this build",
        ))
    }
}

fn validate_browser_emulate_params(
    params: &BrowserEmulateParams,
) -> Result<NormalizedBrowserEmulateParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    match params.operation {
        BrowserEmulateOperation::Set => validate_browser_emulate_set_params(params),
        BrowserEmulateOperation::Reset => validate_browser_emulate_reset_params(params),
    }
}

fn validate_browser_emulate_set_params(
    params: &BrowserEmulateParams,
) -> Result<NormalizedBrowserEmulateParams, ErrorData> {
    if !params.domains.is_empty() {
        return Err(invalid("domains is only valid for operation=reset"));
    }
    if params.viewport.is_some() && params.device.is_some() {
        return Err(invalid(
            "viewport and device cannot be set together because device includes viewport metrics",
        ));
    }
    let mut domains = Vec::new();
    if let Some(viewport) = params.viewport.as_ref() {
        validate_viewport(viewport)?;
        domains.push(BrowserEmulateDomain::Viewport);
    }
    if let Some(device) = params.device.as_ref() {
        validate_device(device)?;
        domains.push(BrowserEmulateDomain::Device);
    }
    if let Some(geolocation) = params.geolocation.as_ref() {
        validate_geolocation(geolocation)?;
        domains.push(BrowserEmulateDomain::Geolocation);
    }
    if let Some(locale) = params.locale.as_ref() {
        validate_locale(locale)?;
        domains.push(BrowserEmulateDomain::Locale);
    }
    if let Some(media) = params.media.as_ref() {
        validate_media(media)?;
        domains.push(BrowserEmulateDomain::Media);
    }
    if let Some(network) = params.network.as_ref() {
        validate_network(network)?;
        domains.push(BrowserEmulateDomain::Network);
    }
    if domains.is_empty() {
        return Err(invalid(
            "operation=set requires at least one override section",
        ));
    }
    Ok(NormalizedBrowserEmulateParams {
        operation: BrowserEmulateOperation::Set,
        domains,
        viewport: params.viewport.clone(),
        device: params.device.clone(),
        geolocation: params.geolocation.clone(),
        locale: params.locale.clone(),
        media: params.media.clone(),
        network: params.network.clone(),
    })
}

fn validate_browser_emulate_reset_params(
    params: &BrowserEmulateParams,
) -> Result<NormalizedBrowserEmulateParams, ErrorData> {
    if params.viewport.is_some()
        || params.device.is_some()
        || params.geolocation.is_some()
        || params.locale.is_some()
        || params.media.is_some()
        || params.network.is_some()
    {
        return Err(invalid(
            "override sections are only valid for operation=set",
        ));
    }
    let domains = if params.domains.is_empty() {
        BrowserEmulateDomain::all()
    } else {
        let mut seen = BTreeSet::new();
        for domain in &params.domains {
            if !seen.insert(*domain) {
                return Err(invalid(format!(
                    "duplicate reset domain {}",
                    domain.as_str()
                )));
            }
        }
        params.domains.clone()
    };
    Ok(NormalizedBrowserEmulateParams {
        operation: BrowserEmulateOperation::Reset,
        domains,
        viewport: None,
        device: None,
        geolocation: None,
        locale: None,
        media: None,
        network: None,
    })
}

fn validate_viewport(viewport: &BrowserEmulateViewport) -> Result<(), ErrorData> {
    validate_dimension("viewport.width", viewport.width)?;
    validate_dimension("viewport.height", viewport.height)?;
    validate_scale_factor(viewport.device_scale_factor.unwrap_or(1.0))?;
    Ok(())
}

fn validate_device(device: &BrowserEmulateDevice) -> Result<(), ErrorData> {
    validate_user_agent(&device.user_agent)?;
    validate_dimension("device.width", device.width)?;
    validate_dimension("device.height", device.height)?;
    validate_scale_factor(device.device_scale_factor.unwrap_or(1.0))?;
    let has_touch = device.has_touch.unwrap_or(false);
    let max_touch_points = device
        .max_touch_points
        .unwrap_or(if has_touch { 5 } else { 0 });
    if has_touch {
        if max_touch_points == 0 || max_touch_points > synapse_a11y::CDP_DEVICE_MAX_TOUCH_POINTS {
            return Err(invalid(format!(
                "device.max_touch_points must be 1..={} when has_touch=true",
                synapse_a11y::CDP_DEVICE_MAX_TOUCH_POINTS
            )));
        }
    } else if max_touch_points != 0 {
        return Err(invalid(
            "device.max_touch_points must be 0 when has_touch=false",
        ));
    }
    Ok(())
}

fn validate_geolocation(geolocation: &BrowserEmulateGeolocation) -> Result<(), ErrorData> {
    validate_finite_range("geolocation.latitude", geolocation.latitude, -90.0, 90.0)?;
    validate_finite_range(
        "geolocation.longitude",
        geolocation.longitude,
        -180.0,
        180.0,
    )?;
    validate_finite_range(
        "geolocation.accuracy",
        geolocation.accuracy.unwrap_or(0.0),
        0.0,
        synapse_a11y::CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;
    validate_optional_finite("geolocation.altitude", geolocation.altitude)?;
    validate_optional_finite(
        "geolocation.altitude_accuracy",
        geolocation.altitude_accuracy,
    )?;
    validate_optional_finite("geolocation.heading", geolocation.heading)?;
    validate_optional_finite("geolocation.speed", geolocation.speed)?;
    Ok(())
}

fn validate_locale(locale: &BrowserEmulateLocale) -> Result<(), ErrorData> {
    if locale.locale.is_none() && locale.timezone_id.is_none() {
        return Err(invalid("locale requires locale and/or timezone_id"));
    }
    if let Some(value) = locale.locale.as_deref() {
        validate_locale_token("locale.locale", value, false)?;
    }
    if let Some(value) = locale.timezone_id.as_deref() {
        validate_locale_token("locale.timezone_id", value, true)?;
    }
    Ok(())
}

fn validate_media(media: &BrowserEmulateMedia) -> Result<(), ErrorData> {
    if media.media.is_none() && media.color_scheme.is_none() && media.reduced_motion.is_none() {
        return Err(invalid(
            "media requires media, color_scheme and/or reduced_motion",
        ));
    }
    if let Some(value) = media.media.as_deref()
        && !matches!(value, "screen" | "print")
    {
        return Err(invalid("media.media must be 'screen' or 'print'"));
    }
    if let Some(value) = media.color_scheme.as_deref()
        && !matches!(value, "light" | "dark" | "no-preference")
    {
        return Err(invalid(
            "media.color_scheme must be 'light', 'dark' or 'no-preference'",
        ));
    }
    if let Some(value) = media.reduced_motion.as_deref()
        && !matches!(value, "reduce" | "no-preference")
    {
        return Err(invalid(
            "media.reduced_motion must be 'reduce' or 'no-preference'",
        ));
    }
    Ok(())
}

fn validate_network(network: &BrowserEmulateNetwork) -> Result<(), ErrorData> {
    let offline = network.offline.unwrap_or(false);
    let latency_ms = network.latency_ms.unwrap_or(0.0);
    let download_throughput_bytes_per_sec =
        network.download_throughput_bytes_per_sec.unwrap_or(-1.0);
    let upload_throughput_bytes_per_sec = network.upload_throughput_bytes_per_sec.unwrap_or(-1.0);
    validate_network_latency(latency_ms)?;
    validate_network_throughput(
        "network.download_throughput_bytes_per_sec",
        download_throughput_bytes_per_sec,
    )?;
    validate_network_throughput(
        "network.upload_throughput_bytes_per_sec",
        upload_throughput_bytes_per_sec,
    )?;
    if let Some(value) = network.connection_type.as_deref() {
        validate_network_connection_type(value)?;
    }
    if !offline
        && latency_ms == 0.0
        && download_throughput_bytes_per_sec == -1.0
        && upload_throughput_bytes_per_sec == -1.0
        && network.connection_type.is_none()
    {
        return Err(invalid(
            "network requires offline=true, latency_ms, throughput and/or connection_type",
        ));
    }
    Ok(())
}

fn validate_dimension(field: &str, value: u32) -> Result<(), ErrorData> {
    if value == 0 || value > synapse_a11y::CDP_DEVICE_METRICS_MAX_DIMENSION {
        return Err(invalid(format!(
            "{field} must be 1..={}",
            synapse_a11y::CDP_DEVICE_METRICS_MAX_DIMENSION
        )));
    }
    Ok(())
}

fn validate_scale_factor(value: f64) -> Result<(), ErrorData> {
    if !value.is_finite() || value <= 0.0 || value > synapse_a11y::CDP_DEVICE_SCALE_FACTOR_MAX {
        return Err(invalid(format!(
            "device_scale_factor must be finite and in 0..={}",
            synapse_a11y::CDP_DEVICE_SCALE_FACTOR_MAX
        )));
    }
    Ok(())
}

fn validate_user_agent(value: &str) -> Result<(), ErrorData> {
    if value.trim() != value || value.is_empty() {
        return Err(invalid(
            "device.user_agent must be non-empty without surrounding whitespace",
        ));
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(invalid(
            "device.user_agent must not contain line breaks or NUL",
        ));
    }
    if value.chars().count() > synapse_a11y::CDP_DEVICE_MAX_USER_AGENT_CHARS {
        return Err(invalid(format!(
            "device.user_agent must be at most {} Unicode scalar values",
            synapse_a11y::CDP_DEVICE_MAX_USER_AGENT_CHARS
        )));
    }
    Ok(())
}

fn validate_finite_range(field: &str, value: f64, min: f64, max: f64) -> Result<(), ErrorData> {
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "{field} must be finite and in {min}..={max}"
        )))
    }
}

fn validate_optional_finite(field: &str, value: Option<f64>) -> Result<(), ErrorData> {
    if let Some(value) = value
        && !value.is_finite()
    {
        return Err(invalid(format!("{field} must be finite")));
    }
    Ok(())
}

fn validate_locale_token(
    field: &str,
    value: &str,
    allow_slash_plus: bool,
) -> Result<(), ErrorData> {
    if value.trim() != value || value.is_empty() {
        return Err(invalid(format!(
            "{field} must be non-empty without surrounding whitespace"
        )));
    }
    let max = if allow_slash_plus {
        synapse_a11y::CDP_TIMEZONE_MAX_CHARS
    } else {
        synapse_a11y::CDP_LOCALE_MAX_CHARS
    };
    if value.chars().count() > max {
        return Err(invalid(format!("{field} must be at most {max} characters")));
    }
    let valid = value.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || ch == '_'
            || ch == '-'
            || (allow_slash_plus && matches!(ch, '/' | '+'))
    });
    if !valid {
        return Err(invalid(format!(
            "{field} contains a character that is not allowed"
        )));
    }
    Ok(())
}

fn validate_network_latency(value: f64) -> Result<(), ErrorData> {
    if value.is_finite() && (0.0..=synapse_a11y::CDP_NETWORK_MAX_LATENCY_MS).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "network.latency_ms must be finite and in 0..={}",
            synapse_a11y::CDP_NETWORK_MAX_LATENCY_MS
        )))
    }
}

fn validate_network_throughput(field: &str, value: f64) -> Result<(), ErrorData> {
    if value.is_finite()
        && (value == -1.0
            || (0.0..=synapse_a11y::CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC).contains(&value))
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "{field} must be -1 to disable throttling or finite in 0..={}",
            synapse_a11y::CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC
        )))
    }
}

fn validate_network_connection_type(value: &str) -> Result<(), ErrorData> {
    if matches!(
        value,
        "none"
            | "cellular2g"
            | "cellular3g"
            | "cellular4g"
            | "bluetooth"
            | "ethernet"
            | "wifi"
            | "wimax"
            | "other"
    ) {
        Ok(())
    } else {
        Err(invalid(
            "network.connection_type must be one of none, cellular2g, cellular3g, cellular4g, bluetooth, ethernet, wifi, wimax or other",
        ))
    }
}

fn domain_result(
    domain: BrowserEmulateDomain,
    operation: BrowserEmulateOperation,
    result: impl Serialize,
) -> Result<BrowserEmulateDomainResult, ErrorData> {
    Ok(BrowserEmulateDomainResult {
        domain,
        operation,
        result: serde_json::to_value(result).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "{TOOL} failed to serialize {} result: {error}",
                    domain.as_str()
                ),
            )
        })?,
    })
}

fn emulate_error(domain: BrowserEmulateDomain, error: synapse_a11y::A11yError) -> ErrorData {
    mcp_error(
        error.code(),
        format!(
            "{TOOL} {} raw CDP emulation failed: {error}",
            domain.as_str()
        ),
    )
}

fn emulate_bridge_error(
    domain: BrowserEmulateDomain,
    error: crate::chrome_debugger_bridge::ChromeDebuggerBridgeError,
) -> ErrorData {
    mcp_error(
        error.code(),
        format!(
            "{TOOL} {} normal Chrome bridge emulation failed: {}",
            domain.as_str(),
            error.detail()
        ),
    )
}

fn invalid(message: impl Into<String>) -> ErrorData {
    mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("{TOOL} {}", message.into()),
    )
}
