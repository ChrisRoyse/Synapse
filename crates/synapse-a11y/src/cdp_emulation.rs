//! Target-scoped raw-CDP browser emulation helpers (#1173/#1174/#1175/#1176/#1177/#1178).

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

use crate::{A11yError, A11yResult};

pub const CDP_DEVICE_METRICS_MAX_DIMENSION: u32 = 10_000_000;
pub const CDP_DEVICE_SCALE_FACTOR_MAX: f64 = 1000.0;
pub const CDP_DEVICE_MAX_TOUCH_POINTS: u32 = 16;
pub const CDP_DEVICE_MAX_USER_AGENT_CHARS: usize = 4096;
pub const CDP_GEOLOCATION_MAX_ACCURACY_METERS: f64 = 1_000_000_000.0;
pub const CDP_LOCALE_MAX_CHARS: usize = 128;
pub const CDP_TIMEZONE_MAX_CHARS: usize = 128;
pub const CDP_NETWORK_MAX_LATENCY_MS: f64 = 3_600_000.0;
pub const CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC: f64 = 10_000_000_000.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpViewportOverride {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpViewportReadback {
    pub inner_width: i64,
    pub inner_height: i64,
    pub device_pixel_ratio: f64,
    pub screen_width: i64,
    pub screen_height: i64,
    pub outer_width: i64,
    pub outer_height: i64,
    pub visual_viewport_width: Option<f64>,
    pub visual_viewport_height: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpViewportResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub requested: Option<CdpViewportOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub readback: CdpViewportReadback,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpDeviceDescriptor {
    pub user_agent: String,
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub is_mobile: bool,
    pub has_touch: bool,
    pub max_touch_points: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpDeviceReadback {
    pub viewport: CdpViewportReadback,
    pub user_agent: String,
    pub max_touch_points: i64,
    pub ontouchstart_available: bool,
    pub pointer_coarse: bool,
    pub any_pointer_coarse: bool,
    pub hover_none: bool,
    pub any_hover_none: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpDeviceResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub descriptor: Option<CdpDeviceDescriptor>,
    pub restored_user_agent: Option<String>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub readback: CdpDeviceReadback,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpGeolocationOverride {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    pub altitude: Option<f64>,
    pub altitude_accuracy: Option<f64>,
    pub heading: Option<f64>,
    pub speed: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpGeolocationCoordinatesReadback {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
    pub altitude: Option<f64>,
    pub altitude_accuracy: Option<f64>,
    pub heading: Option<f64>,
    pub speed: Option<f64>,
    pub timestamp: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpGeolocationErrorReadback {
    pub code: i64,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpGeolocationReadback {
    pub permission_state: String,
    pub position: Option<CdpGeolocationCoordinatesReadback>,
    pub error: Option<CdpGeolocationErrorReadback>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpGeolocationResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub origin: String,
    pub requested: Option<CdpGeolocationOverride>,
    pub permission_setting: String,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub readback: CdpGeolocationReadback,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpLocaleTimezoneOverride {
    pub locale: Option<String>,
    pub timezone_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpLocaleTimezoneReadback {
    pub locale: String,
    pub calendar: String,
    pub numbering_system: String,
    pub time_zone: String,
    pub sample_number: String,
    pub sample_date: String,
    pub date_string: String,
    pub timezone_offset_minutes: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpLocaleTimezoneResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub requested: Option<CdpLocaleTimezoneOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub readback: CdpLocaleTimezoneReadback,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpMediaOverride {
    pub media: Option<String>,
    pub color_scheme: Option<String>,
    pub reduced_motion: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpMediaReadback {
    pub media_screen: bool,
    pub media_print: bool,
    pub color_scheme_dark: bool,
    pub color_scheme_light: bool,
    pub color_scheme_no_preference: bool,
    pub reduced_motion_reduce: bool,
    pub reduced_motion_no_preference: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpMediaResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub requested: Option<CdpMediaOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub readback: CdpMediaReadback,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpNetworkConditionsOverride {
    pub offline: bool,
    pub latency_ms: f64,
    pub download_throughput_bytes_per_sec: f64,
    pub upload_throughput_bytes_per_sec: f64,
    pub connection_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpNetworkConditionsReadback {
    pub online: bool,
    pub connection_type: Option<String>,
    pub effective_type: Option<String>,
    pub downlink_mbps: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub save_data: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpNetworkConditionsResult {
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: String,
    pub requested: Option<CdpNetworkConditionsOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub readback: CdpNetworkConditionsReadback,
}

enum DeviceMetricsCommand {
    Set(CdpViewportOverride),
    Reset,
}

enum DeviceDescriptorCommand {
    Set(CdpDeviceDescriptor),
    Reset { original_user_agent: Option<String> },
}

enum GeolocationCommand {
    Set {
        geolocation: CdpGeolocationOverride,
        permission_setting: GeolocationPermissionSetting,
        origin: String,
    },
    Reset {
        origin: String,
    },
}

enum LocaleTimezoneCommand {
    Set(CdpLocaleTimezoneOverride),
    Reset,
}

enum MediaCommand {
    Set(CdpMediaOverride),
    Reset,
}

enum NetworkConditionsCommand {
    Set(CdpNetworkConditionsOverride),
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeolocationPermissionSetting {
    Granted,
    Denied,
    Prompt,
}

impl GeolocationPermissionSetting {
    fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Prompt => "prompt",
        }
    }
}

fn original_user_agents() -> &'static Mutex<HashMap<String, String>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn device_registry_key(endpoint: &str, target_id: &str) -> String {
    format!("{endpoint}\0{target_id}")
}

/// Applies `Emulation.setDeviceMetricsOverride` to one CDP page target, then
/// reads back page-visible viewport metrics from the same target.
pub async fn cdp_set_viewport_size(
    endpoint: &str,
    target_id: &str,
    width: u32,
    height: u32,
    device_scale_factor: f64,
) -> A11yResult<CdpViewportResult> {
    cdp_set_viewport_size_with_mobile(
        endpoint,
        target_id,
        width,
        height,
        device_scale_factor,
        false,
    )
    .await
}

/// Applies `Emulation.setDeviceMetricsOverride` to one CDP page target with an
/// explicit mobile-emulation flag, then reads back page-visible viewport
/// metrics from the same target.
pub async fn cdp_set_viewport_size_with_mobile(
    endpoint: &str,
    target_id: &str,
    width: u32,
    height: u32,
    device_scale_factor: f64,
    mobile: bool,
) -> A11yResult<CdpViewportResult> {
    validate_viewport_override(width, height, device_scale_factor)?;
    let requested = CdpViewportOverride {
        width,
        height,
        device_scale_factor,
        mobile,
    };
    run_device_metrics_command(
        endpoint,
        target_id,
        DeviceMetricsCommand::Set(requested.clone()),
    )
    .await?;
    let readback = viewport_readback(endpoint, target_id).await?;
    Ok(CdpViewportResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "set".to_owned(),
        requested: Some(requested),
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Clears `Emulation.setDeviceMetricsOverride` for one CDP page target, then
/// reads back the real page-visible viewport metrics from that target.
pub async fn cdp_reset_viewport_size(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpViewportResult> {
    run_device_metrics_command(endpoint, target_id, DeviceMetricsCommand::Reset).await?;
    let readback = viewport_readback(endpoint, target_id).await?;
    Ok(CdpViewportResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "reset".to_owned(),
        requested: None,
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Applies a Playwright-style device descriptor to one CDP page target: user
/// agent, viewport/DPR/mobile metrics, and touch capability in one operation.
pub async fn cdp_apply_device_descriptor(
    endpoint: &str,
    target_id: &str,
    descriptor: CdpDeviceDescriptor,
) -> A11yResult<CdpDeviceResult> {
    validate_device_descriptor(&descriptor)?;
    let key = device_registry_key(endpoint, target_id);
    let original_readback = device_readback(endpoint, target_id).await?;
    if let Ok(mut originals) = original_user_agents().lock() {
        originals
            .entry(key)
            .or_insert_with(|| original_readback.metrics.user_agent.clone());
    }
    run_device_descriptor_command(
        endpoint,
        target_id,
        DeviceDescriptorCommand::Set(descriptor.clone()),
    )
    .await?;
    let readback = device_readback(endpoint, target_id).await?;
    Ok(CdpDeviceResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "set".to_owned(),
        descriptor: Some(descriptor),
        restored_user_agent: None,
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Clears the active device descriptor for one CDP page target. Device metrics
/// and touch emulation are cleared through CDP. The user agent is restored to
/// the value observed before the first descriptor set in this process.
pub async fn cdp_reset_device_descriptor(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpDeviceResult> {
    let key = device_registry_key(endpoint, target_id);
    let original_user_agent = original_user_agents()
        .lock()
        .ok()
        .and_then(|mut originals| originals.remove(&key));
    run_device_descriptor_command(
        endpoint,
        target_id,
        DeviceDescriptorCommand::Reset {
            original_user_agent: original_user_agent.clone(),
        },
    )
    .await?;
    let readback = device_readback(endpoint, target_id).await?;
    Ok(CdpDeviceResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "reset".to_owned(),
        descriptor: None,
        restored_user_agent: original_user_agent,
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Applies `Emulation.setGeolocationOverride` to one CDP page target and sets
/// the current page origin's geolocation permission to granted or denied.
pub async fn cdp_set_geolocation_override(
    endpoint: &str,
    target_id: &str,
    geolocation: CdpGeolocationOverride,
    grant_permission: bool,
) -> A11yResult<CdpGeolocationResult> {
    validate_geolocation_override(&geolocation)?;
    let origin = geolocation_origin(endpoint, target_id).await?;
    let permission_setting = if grant_permission {
        GeolocationPermissionSetting::Granted
    } else {
        GeolocationPermissionSetting::Denied
    };
    run_geolocation_command(
        endpoint,
        target_id,
        GeolocationCommand::Set {
            geolocation: geolocation.clone(),
            permission_setting,
            origin: origin.clone(),
        },
    )
    .await?;
    let readback = geolocation_readback(endpoint, target_id).await?;
    Ok(CdpGeolocationResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "set".to_owned(),
        origin,
        requested: Some(geolocation),
        permission_setting: permission_setting.as_str().to_owned(),
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Clears the active geolocation override for one CDP page target and restores
/// the current page origin's geolocation permission to the default prompt state.
pub async fn cdp_reset_geolocation_override(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpGeolocationResult> {
    let origin = geolocation_origin(endpoint, target_id).await?;
    run_geolocation_command(
        endpoint,
        target_id,
        GeolocationCommand::Reset {
            origin: origin.clone(),
        },
    )
    .await?;
    let readback = geolocation_readback(endpoint, target_id).await?;
    Ok(CdpGeolocationResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "reset".to_owned(),
        origin,
        requested: None,
        permission_setting: GeolocationPermissionSetting::Prompt.as_str().to_owned(),
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Applies locale and/or timezone overrides to one CDP page target and reads
/// back page-visible Intl/Date state from the same target.
pub async fn cdp_set_locale_timezone_override(
    endpoint: &str,
    target_id: &str,
    requested: CdpLocaleTimezoneOverride,
) -> A11yResult<CdpLocaleTimezoneResult> {
    validate_locale_timezone_override(&requested)?;
    run_locale_timezone_command(
        endpoint,
        target_id,
        LocaleTimezoneCommand::Set(requested.clone()),
    )
    .await?;
    let readback = locale_timezone_readback(endpoint, target_id).await?;
    Ok(CdpLocaleTimezoneResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "set".to_owned(),
        requested: Some(requested),
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Clears locale and timezone overrides for one CDP page target and reads back
/// host-default Intl/Date state from that target.
pub async fn cdp_reset_locale_timezone_override(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpLocaleTimezoneResult> {
    run_locale_timezone_command(endpoint, target_id, LocaleTimezoneCommand::Reset).await?;
    let readback = locale_timezone_readback(endpoint, target_id).await?;
    Ok(CdpLocaleTimezoneResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "reset".to_owned(),
        requested: None,
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Applies media type and/or media feature overrides to one CDP page target and
/// reads back page-visible matchMedia state from the same target.
pub async fn cdp_set_media_override(
    endpoint: &str,
    target_id: &str,
    requested: CdpMediaOverride,
) -> A11yResult<CdpMediaResult> {
    validate_media_override(&requested)?;
    run_media_command(endpoint, target_id, MediaCommand::Set(requested.clone())).await?;
    let readback = media_readback(endpoint, target_id).await?;
    Ok(CdpMediaResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "set".to_owned(),
        requested: Some(requested),
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Clears media type and media feature overrides for one CDP page target.
pub async fn cdp_reset_media_override(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpMediaResult> {
    run_media_command(endpoint, target_id, MediaCommand::Reset).await?;
    let readback = media_readback(endpoint, target_id).await?;
    Ok(CdpMediaResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "reset".to_owned(),
        requested: None,
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Applies offline/throttling network conditions to one CDP page target, then
/// reads back page-visible navigator network state from the same target.
pub async fn cdp_set_network_conditions(
    endpoint: &str,
    target_id: &str,
    requested: CdpNetworkConditionsOverride,
) -> A11yResult<CdpNetworkConditionsResult> {
    validate_network_conditions_override(&requested)?;
    run_network_conditions_command(
        endpoint,
        target_id,
        NetworkConditionsCommand::Set(requested.clone()),
    )
    .await?;
    let readback = network_conditions_readback(endpoint, target_id).await?;
    Ok(CdpNetworkConditionsResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "set".to_owned(),
        requested: Some(requested),
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

/// Clears network-condition emulation for one CDP page target by restoring
/// Chromium's online/full-speed defaults.
pub async fn cdp_reset_network_conditions(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpNetworkConditionsResult> {
    run_network_conditions_command(endpoint, target_id, NetworkConditionsCommand::Reset).await?;
    let readback = network_conditions_readback(endpoint, target_id).await?;
    Ok(CdpNetworkConditionsResult {
        endpoint: endpoint.to_owned(),
        cdp_target_id: readback.target_id,
        operation: "reset".to_owned(),
        requested: None,
        page_url: readback.url,
        page_title: readback.title,
        ready_state: readback.ready_state,
        readback: readback.metrics,
    })
}

fn validate_viewport_override(width: u32, height: u32, device_scale_factor: f64) -> A11yResult<()> {
    if width == 0 || width > CDP_DEVICE_METRICS_MAX_DIMENSION {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "viewport width must be 1..={CDP_DEVICE_METRICS_MAX_DIMENSION}, got {width}"
            ),
        });
    }
    if height == 0 || height > CDP_DEVICE_METRICS_MAX_DIMENSION {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "viewport height must be 1..={CDP_DEVICE_METRICS_MAX_DIMENSION}, got {height}"
            ),
        });
    }
    if !device_scale_factor.is_finite()
        || device_scale_factor <= 0.0
        || device_scale_factor > CDP_DEVICE_SCALE_FACTOR_MAX
    {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "device_scale_factor must be finite and in 0..={CDP_DEVICE_SCALE_FACTOR_MAX}, got {device_scale_factor}"
            ),
        });
    }
    Ok(())
}

fn validate_device_descriptor(descriptor: &CdpDeviceDescriptor) -> A11yResult<()> {
    validate_user_agent(&descriptor.user_agent)?;
    validate_viewport_override(
        descriptor.width,
        descriptor.height,
        descriptor.device_scale_factor,
    )?;
    if descriptor.has_touch {
        if descriptor.max_touch_points == 0
            || descriptor.max_touch_points > CDP_DEVICE_MAX_TOUCH_POINTS
        {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "max_touch_points must be 1..={CDP_DEVICE_MAX_TOUCH_POINTS} when has_touch=true, got {}",
                    descriptor.max_touch_points
                ),
            });
        }
    } else if descriptor.max_touch_points != 0 {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "max_touch_points must be 0 when has_touch=false, got {}",
                descriptor.max_touch_points
            ),
        });
    }
    Ok(())
}

fn validate_user_agent(value: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "device descriptor user_agent must be non-empty without surrounding whitespace"
                .to_owned(),
        });
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "device descriptor user_agent must not contain line breaks or NUL".to_owned(),
        });
    }
    if value.chars().count() > CDP_DEVICE_MAX_USER_AGENT_CHARS {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "device descriptor user_agent must be at most {CDP_DEVICE_MAX_USER_AGENT_CHARS} Unicode scalar values"
            ),
        });
    }
    Ok(())
}

fn validate_geolocation_override(geolocation: &CdpGeolocationOverride) -> A11yResult<()> {
    validate_geolocation_range("latitude", geolocation.latitude, -90.0, 90.0)?;
    validate_geolocation_range("longitude", geolocation.longitude, -180.0, 180.0)?;
    validate_geolocation_range(
        "accuracy",
        geolocation.accuracy,
        0.0,
        CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;
    validate_geolocation_optional_finite("altitude", geolocation.altitude)?;
    validate_geolocation_optional_range(
        "altitude_accuracy",
        geolocation.altitude_accuracy,
        0.0,
        CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;
    validate_geolocation_optional_range("heading", geolocation.heading, 0.0, 360.0)?;
    validate_geolocation_optional_range(
        "speed",
        geolocation.speed,
        0.0,
        CDP_GEOLOCATION_MAX_ACCURACY_METERS,
    )?;
    Ok(())
}

fn validate_geolocation_range(field: &str, value: f64, min: f64, max: f64) -> A11yResult<()> {
    if !value.is_finite() || value < min || value > max {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("geolocation {field} must be finite and in {min}..={max}, got {value}"),
        });
    }
    Ok(())
}

fn validate_geolocation_optional_range(
    field: &str,
    value: Option<f64>,
    min: f64,
    max: f64,
) -> A11yResult<()> {
    if let Some(value) = value {
        validate_geolocation_range(field, value, min, max)?;
    }
    Ok(())
}

fn validate_geolocation_optional_finite(field: &str, value: Option<f64>) -> A11yResult<()> {
    if let Some(value) = value
        && !value.is_finite() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!("geolocation {field} must be finite, got {value}"),
            });
        }
    Ok(())
}

fn validate_locale_timezone_override(requested: &CdpLocaleTimezoneOverride) -> A11yResult<()> {
    if requested.locale.is_none() && requested.timezone_id.is_none() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "locale/timezone override requires locale and/or timezone_id".to_owned(),
        });
    }
    if let Some(locale) = requested.locale.as_deref() {
        validate_locale_override(locale)?;
    }
    if let Some(timezone_id) = requested.timezone_id.as_deref() {
        validate_timezone_override(timezone_id)?;
    }
    Ok(())
}

fn validate_locale_override(value: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "locale override must be non-empty without surrounding whitespace".to_owned(),
        });
    }
    if value.chars().count() > CDP_LOCALE_MAX_CHARS {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("locale override must be at most {CDP_LOCALE_MAX_CHARS} characters"),
        });
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "locale override must contain only ASCII letters, digits, '_' or '-'"
                .to_owned(),
        });
    }
    Ok(())
}

fn validate_timezone_override(value: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "timezone_id override must be non-empty without surrounding whitespace"
                .to_owned(),
        });
    }
    if value.chars().count() > CDP_TIMEZONE_MAX_CHARS {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "timezone_id override must be at most {CDP_TIMEZONE_MAX_CHARS} characters"
            ),
        });
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-' | '+'))
    {
        return Err(A11yError::CdpAxtreeFailed {
            detail:
                "timezone_id override must contain only ASCII letters, digits, '/', '_', '-' or '+'"
                    .to_owned(),
        });
    }
    Ok(())
}

fn validate_media_override(requested: &CdpMediaOverride) -> A11yResult<()> {
    if requested.media.is_none()
        && requested.color_scheme.is_none()
        && requested.reduced_motion.is_none()
    {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "media override requires media, color_scheme and/or reduced_motion".to_owned(),
        });
    }
    if let Some(media) = requested.media.as_deref() {
        validate_media_type(media)?;
    }
    if let Some(color_scheme) = requested.color_scheme.as_deref() {
        validate_color_scheme(color_scheme)?;
    }
    if let Some(reduced_motion) = requested.reduced_motion.as_deref() {
        validate_reduced_motion(reduced_motion)?;
    }
    Ok(())
}

fn validate_media_type(value: &str) -> A11yResult<()> {
    if matches!(value, "screen" | "print") {
        Ok(())
    } else {
        Err(A11yError::CdpAxtreeFailed {
            detail: format!("media must be 'screen' or 'print', got {value:?}"),
        })
    }
}

fn validate_color_scheme(value: &str) -> A11yResult<()> {
    if matches!(value, "light" | "dark" | "no-preference") {
        Ok(())
    } else {
        Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "color_scheme must be 'light', 'dark' or 'no-preference', got {value:?}"
            ),
        })
    }
}

fn validate_reduced_motion(value: &str) -> A11yResult<()> {
    if matches!(value, "reduce" | "no-preference") {
        Ok(())
    } else {
        Err(A11yError::CdpAxtreeFailed {
            detail: format!("reduced_motion must be 'reduce' or 'no-preference', got {value:?}"),
        })
    }
}

fn validate_network_conditions_override(
    requested: &CdpNetworkConditionsOverride,
) -> A11yResult<()> {
    validate_network_latency(requested.latency_ms)?;
    validate_network_throughput(
        "download_throughput_bytes_per_sec",
        requested.download_throughput_bytes_per_sec,
    )?;
    validate_network_throughput(
        "upload_throughput_bytes_per_sec",
        requested.upload_throughput_bytes_per_sec,
    )?;
    if let Some(connection_type) = requested.connection_type.as_deref() {
        validate_network_connection_type(connection_type)?;
    }
    if !requested.offline
        && requested.latency_ms == 0.0
        && requested.download_throughput_bytes_per_sec == -1.0
        && requested.upload_throughput_bytes_per_sec == -1.0
        && requested.connection_type.is_none()
    {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "network conditions set requires offline=true, latency, throughput and/or connection_type; use reset to restore defaults".to_owned(),
        });
    }
    Ok(())
}

fn validate_network_latency(value: f64) -> A11yResult<()> {
    if value.is_finite() && (0.0..=CDP_NETWORK_MAX_LATENCY_MS).contains(&value) {
        Ok(())
    } else {
        Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "latency_ms must be finite and in 0..={CDP_NETWORK_MAX_LATENCY_MS}, got {value}"
            ),
        })
    }
}

fn validate_network_throughput(field: &str, value: f64) -> A11yResult<()> {
    if value.is_finite()
        && (value == -1.0 || (0.0..=CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC).contains(&value))
    {
        Ok(())
    } else {
        Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "{field} must be -1 to disable throttling or finite in 0..={CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC}, got {value}"
            ),
        })
    }
}

fn validate_network_connection_type(value: &str) -> A11yResult<()> {
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
        Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "connection_type must be one of none, cellular2g, cellular3g, cellular4g, bluetooth, ethernet, wifi, wimax or other, got {value:?}"
            ),
        })
    }
}

async fn run_device_metrics_command(
    endpoint: &str,
    target_id: &str,
    command: DeviceMetricsCommand,
) -> A11yResult<()> {
    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::emulation::{
        ClearDeviceMetricsOverrideParams, SetDeviceMetricsOverrideParams,
    };
    use futures_util::StreamExt as _;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        match command {
            DeviceMetricsCommand::Set(override_metrics) => {
                let params = SetDeviceMetricsOverrideParams::builder()
                    .width(i64::from(override_metrics.width))
                    .height(i64::from(override_metrics.height))
                    .device_scale_factor(override_metrics.device_scale_factor)
                    .mobile(override_metrics.mobile)
                    .screen_width(i64::from(override_metrics.width))
                    .screen_height(i64::from(override_metrics.height))
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setDeviceMetricsOverride params: {err}"),
                    })?;
                page.execute(params)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setDeviceMetricsOverride: {err}"),
                    })?;
            }
            DeviceMetricsCommand::Reset => {
                page.execute(ClearDeviceMetricsOverrideParams::default())
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.clearDeviceMetricsOverride: {err}"),
                    })?;
                let params = SetDeviceMetricsOverrideParams::builder()
                    .width(0)
                    .height(0)
                    .device_scale_factor(0.0)
                    .mobile(false)
                    .screen_width(0)
                    .screen_height(0)
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setDeviceMetricsOverride reset params: {err}"),
                    })?;
                page.execute(params)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setDeviceMetricsOverride reset: {err}"),
                    })?;
            }
        }
        Ok(())
    }
    .await;

    handler_task.abort();
    result
}

async fn run_device_descriptor_command(
    endpoint: &str,
    target_id: &str,
    command: DeviceDescriptorCommand,
) -> A11yResult<()> {
    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::emulation::{
        ClearDeviceMetricsOverrideParams, SetDeviceMetricsOverrideParams,
        SetEmitTouchEventsForMouseConfiguration, SetEmitTouchEventsForMouseParams,
        SetTouchEmulationEnabledParams, SetUserAgentOverrideParams,
    };
    use futures_util::StreamExt as _;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        match command {
            DeviceDescriptorCommand::Set(descriptor) => {
                let user_agent = SetUserAgentOverrideParams::builder()
                    .user_agent(descriptor.user_agent.clone())
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setUserAgentOverride params: {err}"),
                    })?;
                page.execute(user_agent)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setUserAgentOverride: {err}"),
                    })?;

                let metrics = SetDeviceMetricsOverrideParams::builder()
                    .width(i64::from(descriptor.width))
                    .height(i64::from(descriptor.height))
                    .device_scale_factor(descriptor.device_scale_factor)
                    .mobile(descriptor.is_mobile)
                    .screen_width(i64::from(descriptor.width))
                    .screen_height(i64::from(descriptor.height))
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setDeviceMetricsOverride params: {err}"),
                    })?;
                page.execute(metrics)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setDeviceMetricsOverride: {err}"),
                    })?;

                let mut touch =
                    SetTouchEmulationEnabledParams::builder().enabled(descriptor.has_touch);
                if descriptor.has_touch {
                    touch = touch.max_touch_points(i64::from(descriptor.max_touch_points));
                }
                page.execute(touch.build().map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Emulation.setTouchEmulationEnabled params: {err}"),
                })?)
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Emulation.setTouchEmulationEnabled: {err}"),
                })?;

                let touch_config = if descriptor.is_mobile {
                    SetEmitTouchEventsForMouseConfiguration::Mobile
                } else {
                    SetEmitTouchEventsForMouseConfiguration::Desktop
                };
                let emit = SetEmitTouchEventsForMouseParams::builder()
                    .enabled(descriptor.has_touch)
                    .configuration(touch_config)
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setEmitTouchEventsForMouse params: {err}"),
                    })?;
                page.execute(emit)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setEmitTouchEventsForMouse: {err}"),
                    })?;
            }
            DeviceDescriptorCommand::Reset {
                original_user_agent,
            } => {
                page.execute(ClearDeviceMetricsOverrideParams::default())
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.clearDeviceMetricsOverride: {err}"),
                    })?;
                page.execute(SetTouchEmulationEnabledParams::new(false))
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setTouchEmulationEnabled(false): {err}"),
                    })?;
                let emit = SetEmitTouchEventsForMouseParams::builder()
                    .enabled(false)
                    .configuration(SetEmitTouchEventsForMouseConfiguration::Desktop)
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setEmitTouchEventsForMouse params: {err}"),
                    })?;
                page.execute(emit)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setEmitTouchEventsForMouse(false): {err}"),
                    })?;
                if let Some(original_user_agent) = original_user_agent {
                    let user_agent = SetUserAgentOverrideParams::builder()
                        .user_agent(original_user_agent)
                        .build()
                        .map_err(|err| A11yError::CdpAxtreeFailed {
                            detail: format!("Emulation.setUserAgentOverride restore params: {err}"),
                        })?;
                    page.execute(user_agent)
                        .await
                        .map_err(|err| A11yError::CdpAxtreeFailed {
                            detail: format!("Emulation.setUserAgentOverride restore: {err}"),
                        })?;
                }
            }
        }
        Ok(())
    }
    .await;

    handler_task.abort();
    result
}

async fn run_geolocation_command(
    endpoint: &str,
    target_id: &str,
    command: GeolocationCommand,
) -> A11yResult<()> {
    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::emulation::{
        ClearGeolocationOverrideParams, SetGeolocationOverrideParams,
    };
    use futures_util::StreamExt as _;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        match command {
            GeolocationCommand::Set {
                geolocation,
                permission_setting,
                origin,
            } => {
                let permission = geolocation_permission_params(&origin, permission_setting)?;
                browser
                    .execute(permission)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!(
                            "Browser.setPermission geolocation={}: {err}",
                            permission_setting.as_str()
                        ),
                    })?;

                let mut params = SetGeolocationOverrideParams::builder()
                    .latitude(geolocation.latitude)
                    .longitude(geolocation.longitude)
                    .accuracy(geolocation.accuracy);
                if let Some(value) = geolocation.altitude {
                    params = params.altitude(value);
                }
                if let Some(value) = geolocation.altitude_accuracy {
                    params = params.altitude_accuracy(value);
                }
                if let Some(value) = geolocation.heading {
                    params = params.heading(value);
                }
                if let Some(value) = geolocation.speed {
                    params = params.speed(value);
                }
                page.execute(params.build())
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setGeolocationOverride: {err}"),
                    })?;
            }
            GeolocationCommand::Reset { origin } => {
                page.execute(ClearGeolocationOverrideParams::default())
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.clearGeolocationOverride: {err}"),
                    })?;
                let permission =
                    geolocation_permission_params(&origin, GeolocationPermissionSetting::Prompt)?;
                browser
                    .execute(permission)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Browser.setPermission geolocation=prompt: {err}"),
                    })?;
            }
        }
        Ok(())
    }
    .await;

    handler_task.abort();
    result
}

fn geolocation_permission_params(
    origin: &str,
    permission_setting: GeolocationPermissionSetting,
) -> A11yResult<chromiumoxide::cdp::browser_protocol::browser::SetPermissionParams> {
    use chromiumoxide::cdp::browser_protocol::browser::{
        PermissionDescriptor, PermissionSetting, SetPermissionParams,
    };

    let setting = match permission_setting {
        GeolocationPermissionSetting::Granted => PermissionSetting::Granted,
        GeolocationPermissionSetting::Denied => PermissionSetting::Denied,
        GeolocationPermissionSetting::Prompt => PermissionSetting::Prompt,
    };
    SetPermissionParams::builder()
        .permission(PermissionDescriptor::new("geolocation"))
        .setting(setting)
        .origin(origin.to_owned())
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Browser.setPermission geolocation params: {err}"),
        })
}

async fn run_locale_timezone_command(
    endpoint: &str,
    target_id: &str,
    command: LocaleTimezoneCommand,
) -> A11yResult<()> {
    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::emulation::{
        SetLocaleOverrideParams, SetTimezoneOverrideParams,
    };
    use futures_util::StreamExt as _;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        match command {
            LocaleTimezoneCommand::Set(requested) => {
                if let Some(locale) = requested.locale {
                    page.execute(SetLocaleOverrideParams::builder().locale(locale).build())
                        .await
                        .map_err(|err| A11yError::CdpAxtreeFailed {
                            detail: format!("Emulation.setLocaleOverride: {err}"),
                        })?;
                }
                if let Some(timezone_id) = requested.timezone_id {
                    let params = SetTimezoneOverrideParams::builder()
                        .timezone_id(timezone_id)
                        .build()
                        .map_err(|err| A11yError::CdpAxtreeFailed {
                            detail: format!("Emulation.setTimezoneOverride params: {err}"),
                        })?;
                    page.execute(params)
                        .await
                        .map_err(|err| A11yError::CdpAxtreeFailed {
                            detail: format!("Emulation.setTimezoneOverride: {err}"),
                        })?;
                }
            }
            LocaleTimezoneCommand::Reset => {
                page.execute(SetLocaleOverrideParams::default())
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setLocaleOverride reset: {err}"),
                    })?;
                page.execute(SetTimezoneOverrideParams::new(""))
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Emulation.setTimezoneOverride reset: {err}"),
                    })?;
            }
        }
        Ok(())
    }
    .await;

    handler_task.abort();
    result
}

async fn run_media_command(
    endpoint: &str,
    target_id: &str,
    command: MediaCommand,
) -> A11yResult<()> {
    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::emulation::{MediaFeature, SetEmulatedMediaParams};
    use futures_util::StreamExt as _;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        let params = match command {
            MediaCommand::Set(requested) => {
                let mut features = Vec::new();
                if let Some(color_scheme) = requested.color_scheme {
                    features.push(MediaFeature::new("prefers-color-scheme", color_scheme));
                }
                if let Some(reduced_motion) = requested.reduced_motion {
                    features.push(MediaFeature::new("prefers-reduced-motion", reduced_motion));
                }
                SetEmulatedMediaParams::builder()
                    .media(requested.media.unwrap_or_default())
                    .features(features)
                    .build()
            }
            MediaCommand::Reset => SetEmulatedMediaParams::builder()
                .media("")
                .features(Vec::<MediaFeature>::new())
                .build(),
        };
        page.execute(params)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Emulation.setEmulatedMedia: {err}"),
            })?;
        Ok(())
    }
    .await;

    handler_task.abort();
    result
}

#[allow(deprecated)]
async fn run_network_conditions_command(
    endpoint: &str,
    target_id: &str,
    command: NetworkConditionsCommand,
) -> A11yResult<()> {
    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::network::{
        ConnectionType, EmulateNetworkConditionsParams,
    };
    use futures_util::StreamExt as _;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        let params = match command {
            NetworkConditionsCommand::Set(requested) => {
                let mut builder = EmulateNetworkConditionsParams::builder()
                    .offline(requested.offline)
                    .latency(requested.latency_ms)
                    .download_throughput(requested.download_throughput_bytes_per_sec)
                    .upload_throughput(requested.upload_throughput_bytes_per_sec);
                if let Some(connection_type) = requested.connection_type.as_deref() {
                    let parsed = connection_type.parse::<ConnectionType>().map_err(|err| {
                        A11yError::CdpAxtreeFailed {
                            detail: format!("connection_type {connection_type:?}: {err}"),
                        }
                    })?;
                    builder = builder.connection_type(parsed);
                }
                builder.build().map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Network.emulateNetworkConditions params: {err}"),
                })?
            }
            NetworkConditionsCommand::Reset => EmulateNetworkConditionsParams::builder()
                .offline(false)
                .latency(0.0)
                .download_throughput(-1.0)
                .upload_throughput(-1.0)
                .build()
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Network.emulateNetworkConditions reset params: {err}"),
                })?,
        };
        page.execute(params)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Network.emulateNetworkConditions: {err}"),
            })?;
        Ok(())
    }
    .await;

    handler_task.abort();
    result
}

struct ViewportReadback {
    target_id: String,
    url: String,
    title: String,
    ready_state: String,
    metrics: CdpViewportReadback,
}

struct DeviceReadback {
    target_id: String,
    url: String,
    title: String,
    ready_state: String,
    metrics: CdpDeviceReadback,
}

struct GeolocationReadback {
    target_id: String,
    url: String,
    title: String,
    ready_state: String,
    metrics: CdpGeolocationReadback,
}

struct LocaleTimezoneReadback {
    target_id: String,
    url: String,
    title: String,
    ready_state: String,
    metrics: CdpLocaleTimezoneReadback,
}

struct MediaReadback {
    target_id: String,
    url: String,
    title: String,
    ready_state: String,
    metrics: CdpMediaReadback,
}

struct NetworkConditionsReadback {
    target_id: String,
    url: String,
    title: String,
    ready_state: String,
    metrics: CdpNetworkConditionsReadback,
}

async fn viewport_readback(endpoint: &str, target_id: &str) -> A11yResult<ViewportReadback> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        VIEWPORT_READBACK_JS,
        false,
        true,
    )
    .await?;
    let metrics =
        serde_json::from_value::<CdpViewportReadback>(evaluated.value).map_err(|error| {
            A11yError::CdpAxtreeFailed {
                detail: format!("viewport metrics readback decode: {error}"),
            }
        })?;
    Ok(ViewportReadback {
        target_id: evaluated.target_id,
        url: evaluated.url,
        title: evaluated.title,
        ready_state: evaluated.ready_state,
        metrics,
    })
}

async fn device_readback(endpoint: &str, target_id: &str) -> A11yResult<DeviceReadback> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        DEVICE_READBACK_JS,
        false,
        true,
    )
    .await?;
    let metrics =
        serde_json::from_value::<CdpDeviceReadback>(evaluated.value).map_err(|error| {
            A11yError::CdpAxtreeFailed {
                detail: format!("device descriptor readback decode: {error}"),
            }
        })?;
    Ok(DeviceReadback {
        target_id: evaluated.target_id,
        url: evaluated.url,
        title: evaluated.title,
        ready_state: evaluated.ready_state,
        metrics,
    })
}

async fn geolocation_origin(endpoint: &str, target_id: &str) -> A11yResult<String> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        GEOLOCATION_ORIGIN_JS,
        false,
        true,
    )
    .await?;
    let origin = serde_json::from_value::<String>(evaluated.value).map_err(|error| {
        A11yError::CdpAxtreeFailed {
            detail: format!("geolocation origin readback decode: {error}"),
        }
    })?;
    if origin.is_empty() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "geolocation permission requires a page with a non-opaque origin, current URL: {}",
                evaluated.url
            ),
        });
    }
    Ok(origin)
}

async fn geolocation_readback(endpoint: &str, target_id: &str) -> A11yResult<GeolocationReadback> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        GEOLOCATION_READBACK_JS,
        true,
        true,
    )
    .await?;
    let metrics =
        serde_json::from_value::<CdpGeolocationReadback>(evaluated.value).map_err(|error| {
            A11yError::CdpAxtreeFailed {
                detail: format!("geolocation readback decode: {error}"),
            }
        })?;
    Ok(GeolocationReadback {
        target_id: evaluated.target_id,
        url: evaluated.url,
        title: evaluated.title,
        ready_state: evaluated.ready_state,
        metrics,
    })
}

async fn locale_timezone_readback(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<LocaleTimezoneReadback> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        LOCALE_TIMEZONE_READBACK_JS,
        false,
        true,
    )
    .await?;
    let metrics =
        serde_json::from_value::<CdpLocaleTimezoneReadback>(evaluated.value).map_err(|error| {
            A11yError::CdpAxtreeFailed {
                detail: format!("locale/timezone readback decode: {error}"),
            }
        })?;
    Ok(LocaleTimezoneReadback {
        target_id: evaluated.target_id,
        url: evaluated.url,
        title: evaluated.title,
        ready_state: evaluated.ready_state,
        metrics,
    })
}

async fn media_readback(endpoint: &str, target_id: &str) -> A11yResult<MediaReadback> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        MEDIA_READBACK_JS,
        false,
        true,
    )
    .await?;
    let metrics = serde_json::from_value::<CdpMediaReadback>(evaluated.value).map_err(|error| {
        A11yError::CdpAxtreeFailed {
            detail: format!("media readback decode: {error}"),
        }
    })?;
    Ok(MediaReadback {
        target_id: evaluated.target_id,
        url: evaluated.url,
        title: evaluated.title,
        ready_state: evaluated.ready_state,
        metrics,
    })
}

async fn network_conditions_readback(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<NetworkConditionsReadback> {
    let evaluated = crate::cdp_action::cdp_evaluate_expression(
        endpoint,
        target_id,
        NETWORK_CONDITIONS_READBACK_JS,
        false,
        true,
    )
    .await?;
    let metrics = serde_json::from_value::<CdpNetworkConditionsReadback>(evaluated.value).map_err(
        |error| A11yError::CdpAxtreeFailed {
            detail: format!("network conditions readback decode: {error}"),
        },
    )?;
    Ok(NetworkConditionsReadback {
        target_id: evaluated.target_id,
        url: evaluated.url,
        title: evaluated.title,
        ready_state: evaluated.ready_state,
        metrics,
    })
}

const VIEWPORT_READBACK_JS: &str = r#"(() => {
  const viewport = globalThis.visualViewport || null;
  return {
    inner_width: Math.round(globalThis.innerWidth || 0),
    inner_height: Math.round(globalThis.innerHeight || 0),
    device_pixel_ratio: Number(globalThis.devicePixelRatio || 0),
    screen_width: Math.round(globalThis.screen ? globalThis.screen.width || 0 : 0),
    screen_height: Math.round(globalThis.screen ? globalThis.screen.height || 0 : 0),
    outer_width: Math.round(globalThis.outerWidth || 0),
    outer_height: Math.round(globalThis.outerHeight || 0),
    visual_viewport_width: viewport ? Number(viewport.width) : null,
    visual_viewport_height: viewport ? Number(viewport.height) : null
  };
})()"#;

const DEVICE_READBACK_JS: &str = r#"(() => {
  const viewport = globalThis.visualViewport || null;
  const media = query => {
    try { return Boolean(globalThis.matchMedia && globalThis.matchMedia(query).matches); }
    catch (_error) { return false; }
  };
  return {
    viewport: {
      inner_width: Math.round(globalThis.innerWidth || 0),
      inner_height: Math.round(globalThis.innerHeight || 0),
      device_pixel_ratio: Number(globalThis.devicePixelRatio || 0),
      screen_width: Math.round(globalThis.screen ? globalThis.screen.width || 0 : 0),
      screen_height: Math.round(globalThis.screen ? globalThis.screen.height || 0 : 0),
      outer_width: Math.round(globalThis.outerWidth || 0),
      outer_height: Math.round(globalThis.outerHeight || 0),
      visual_viewport_width: viewport ? Number(viewport.width) : null,
      visual_viewport_height: viewport ? Number(viewport.height) : null
    },
    user_agent: String(globalThis.navigator ? globalThis.navigator.userAgent || "" : ""),
    max_touch_points: Number(globalThis.navigator ? globalThis.navigator.maxTouchPoints || 0 : 0),
    ontouchstart_available: Boolean("ontouchstart" in globalThis),
    pointer_coarse: media("(pointer: coarse)"),
    any_pointer_coarse: media("(any-pointer: coarse)"),
    hover_none: media("(hover: none)"),
    any_hover_none: media("(any-hover: none)")
  };
})()"#;

const GEOLOCATION_ORIGIN_JS: &str = r#"(() => {
  const location = globalThis.location || null;
  const origin = location ? String(location.origin || "") : "";
  return origin && origin !== "null" ? origin : "";
})()"#;

const GEOLOCATION_READBACK_JS: &str = r#"(async () => {
  const permissionState = await (async () => {
    try {
      if (!globalThis.navigator || !navigator.permissions || !navigator.permissions.query) {
        return "unsupported";
      }
      const status = await navigator.permissions.query({ name: "geolocation" });
      return String(status.state || "unknown");
    } catch (error) {
      const name = error && error.name ? error.name : error;
      return `error:${String(name)}`;
    }
  })();

  const result = await new Promise(resolve => {
    if (!globalThis.navigator || !navigator.geolocation) {
      resolve({
        position: null,
        error: { code: -1, message: "navigator.geolocation unavailable" }
      });
      return;
    }
    let settled = false;
    const finish = value => {
      if (!settled) {
        settled = true;
        resolve(value);
      }
    };
    const timer = globalThis.setTimeout(() => finish({
      position: null,
      error: { code: 3, message: "timeout waiting for geolocation callback" }
    }), 1500);
    navigator.geolocation.getCurrentPosition(
      position => {
        globalThis.clearTimeout(timer);
        const coords = position.coords || {};
        finish({
          position: {
            latitude: Number(coords.latitude),
            longitude: Number(coords.longitude),
            accuracy: Number(coords.accuracy),
            altitude: coords.altitude == null ? null : Number(coords.altitude),
            altitude_accuracy: coords.altitudeAccuracy == null ? null : Number(coords.altitudeAccuracy),
            heading: coords.heading == null || Number.isNaN(Number(coords.heading)) ? null : Number(coords.heading),
            speed: coords.speed == null || Number.isNaN(Number(coords.speed)) ? null : Number(coords.speed),
            timestamp: Number(position.timestamp || 0)
          },
          error: null
        });
      },
      error => {
        globalThis.clearTimeout(timer);
        finish({
          position: null,
          error: {
            code: Number(error && error.code || 0),
            message: String(error && error.message || "")
          }
        });
      },
      { enableHighAccuracy: false, maximumAge: 0, timeout: 1000 }
    );
  });

  return {
    permission_state: permissionState,
    position: result.position,
    error: result.error
  };
})()"#;

const LOCALE_TIMEZONE_READBACK_JS: &str = r#"(() => {
  const sampleDate = new Date(Date.UTC(2020, 0, 2, 3, 4, 5));
  const options = Intl.DateTimeFormat().resolvedOptions();
  const dateFormatter = new Intl.DateTimeFormat(undefined, {
    dateStyle: "full",
    timeStyle: "long"
  });
  return {
    locale: String(options.locale || ""),
    calendar: String(options.calendar || ""),
    numbering_system: String(options.numberingSystem || ""),
    time_zone: String(options.timeZone || ""),
    sample_number: new Intl.NumberFormat().format(1234567.89),
    sample_date: dateFormatter.format(sampleDate),
    date_string: sampleDate.toString(),
    timezone_offset_minutes: Number(sampleDate.getTimezoneOffset())
  };
})()"#;

const MEDIA_READBACK_JS: &str = r#"(() => {
  const media = query => {
    try { return Boolean(globalThis.matchMedia && globalThis.matchMedia(query).matches); }
    catch (_error) { return false; }
  };
  return {
    media_screen: media("screen"),
    media_print: media("print"),
    color_scheme_dark: media("(prefers-color-scheme: dark)"),
    color_scheme_light: media("(prefers-color-scheme: light)"),
    color_scheme_no_preference: media("(prefers-color-scheme: no-preference)"),
    reduced_motion_reduce: media("(prefers-reduced-motion: reduce)"),
    reduced_motion_no_preference: media("(prefers-reduced-motion: no-preference)")
  };
})()"#;

const NETWORK_CONDITIONS_READBACK_JS: &str = r#"(() => {
  const nav = globalThis.navigator || null;
  const connection = nav && (nav.connection || nav.mozConnection || nav.webkitConnection) || null;
  const finiteNumber = value => {
    return typeof value === "number" && Number.isFinite(value) ? value : null;
  };
  return {
    online: Boolean(nav ? nav.onLine : false),
    connection_type: connection && typeof connection.type === "string" ? connection.type : null,
    effective_type: connection && typeof connection.effectiveType === "string" ? connection.effectiveType : null,
    downlink_mbps: connection ? finiteNumber(connection.downlink) : null,
    rtt_ms: connection ? finiteNumber(connection.rtt) : null,
    save_data: connection && typeof connection.saveData === "boolean" ? connection.saveData : null
  };
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_override_validation_edges() {
        assert!(validate_viewport_override(1280, 720, 1.25).is_ok());
        assert!(validate_viewport_override(0, 720, 1.0).is_err());
        assert!(validate_viewport_override(1280, 0, 1.0).is_err());
        assert!(
            validate_viewport_override(CDP_DEVICE_METRICS_MAX_DIMENSION + 1, 720, 1.0).is_err()
        );
        assert!(validate_viewport_override(1280, 720, 0.0).is_err());
        assert!(validate_viewport_override(1280, 720, f64::NAN).is_err());
    }

    #[test]
    fn device_descriptor_validation_edges() {
        let mobile = CdpDeviceDescriptor {
            user_agent: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Mobile/15E148"
                .to_owned(),
            width: 390,
            height: 844,
            device_scale_factor: 3.0,
            is_mobile: true,
            has_touch: true,
            max_touch_points: 5,
        };
        assert!(validate_device_descriptor(&mobile).is_ok());

        let mut bad_ua = mobile.clone();
        bad_ua.user_agent = " bad ".to_owned();
        assert!(validate_device_descriptor(&bad_ua).is_err());

        let mut no_touch_points = mobile.clone();
        no_touch_points.max_touch_points = 0;
        assert!(validate_device_descriptor(&no_touch_points).is_err());

        let desktop = CdpDeviceDescriptor {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36".to_owned(),
            width: 1280,
            height: 720,
            device_scale_factor: 1.0,
            is_mobile: false,
            has_touch: false,
            max_touch_points: 0,
        };
        assert!(validate_device_descriptor(&desktop).is_ok());

        let mut desktop_with_touch_points = desktop;
        desktop_with_touch_points.max_touch_points = 1;
        assert!(validate_device_descriptor(&desktop_with_touch_points).is_err());
    }

    #[test]
    fn geolocation_override_validation_edges() {
        let valid = CdpGeolocationOverride {
            latitude: 37.7749,
            longitude: -122.4194,
            accuracy: 12.5,
            altitude: Some(25.0),
            altitude_accuracy: Some(4.0),
            heading: Some(180.0),
            speed: Some(1.5),
        };
        assert!(validate_geolocation_override(&valid).is_ok());

        let mut bad_latitude = valid.clone();
        bad_latitude.latitude = 91.0;
        assert!(validate_geolocation_override(&bad_latitude).is_err());

        let mut bad_longitude = valid.clone();
        bad_longitude.longitude = -181.0;
        assert!(validate_geolocation_override(&bad_longitude).is_err());

        let mut bad_accuracy = valid.clone();
        bad_accuracy.accuracy = -1.0;
        assert!(validate_geolocation_override(&bad_accuracy).is_err());

        let mut bad_heading = valid.clone();
        bad_heading.heading = Some(361.0);
        assert!(validate_geolocation_override(&bad_heading).is_err());

        let mut bad_speed = valid;
        bad_speed.speed = Some(f64::INFINITY);
        assert!(validate_geolocation_override(&bad_speed).is_err());
    }

    #[test]
    fn locale_timezone_override_validation_edges() {
        let valid = CdpLocaleTimezoneOverride {
            locale: Some("fr_FR".to_owned()),
            timezone_id: Some("Europe/Paris".to_owned()),
        };
        assert!(validate_locale_timezone_override(&valid).is_ok());

        let locale_only = CdpLocaleTimezoneOverride {
            locale: Some("en-US".to_owned()),
            timezone_id: None,
        };
        assert!(validate_locale_timezone_override(&locale_only).is_ok());

        let timezone_only = CdpLocaleTimezoneOverride {
            locale: None,
            timezone_id: Some("America/Los_Angeles".to_owned()),
        };
        assert!(validate_locale_timezone_override(&timezone_only).is_ok());

        let empty = CdpLocaleTimezoneOverride {
            locale: None,
            timezone_id: None,
        };
        assert!(validate_locale_timezone_override(&empty).is_err());

        let bad_locale = CdpLocaleTimezoneOverride {
            locale: Some(" fr_FR ".to_owned()),
            timezone_id: Some("Europe/Paris".to_owned()),
        };
        assert!(validate_locale_timezone_override(&bad_locale).is_err());

        let bad_timezone = CdpLocaleTimezoneOverride {
            locale: Some("fr_FR".to_owned()),
            timezone_id: Some("Europe Paris".to_owned()),
        };
        assert!(validate_locale_timezone_override(&bad_timezone).is_err());
    }

    #[test]
    fn media_override_validation_edges() {
        let valid = CdpMediaOverride {
            media: Some("print".to_owned()),
            color_scheme: Some("dark".to_owned()),
            reduced_motion: Some("reduce".to_owned()),
        };
        assert!(validate_media_override(&valid).is_ok());

        let color_only = CdpMediaOverride {
            media: None,
            color_scheme: Some("light".to_owned()),
            reduced_motion: None,
        };
        assert!(validate_media_override(&color_only).is_ok());

        let empty = CdpMediaOverride {
            media: None,
            color_scheme: None,
            reduced_motion: None,
        };
        assert!(validate_media_override(&empty).is_err());

        let bad_media = CdpMediaOverride {
            media: Some("tv".to_owned()),
            color_scheme: Some("dark".to_owned()),
            reduced_motion: None,
        };
        assert!(validate_media_override(&bad_media).is_err());

        let bad_color = CdpMediaOverride {
            media: Some("screen".to_owned()),
            color_scheme: Some("sepia".to_owned()),
            reduced_motion: None,
        };
        assert!(validate_media_override(&bad_color).is_err());

        let bad_motion = CdpMediaOverride {
            media: Some("screen".to_owned()),
            color_scheme: None,
            reduced_motion: Some("always".to_owned()),
        };
        assert!(validate_media_override(&bad_motion).is_err());
    }

    #[test]
    fn network_conditions_validation_edges() {
        let offline = CdpNetworkConditionsOverride {
            offline: true,
            latency_ms: 0.0,
            download_throughput_bytes_per_sec: -1.0,
            upload_throughput_bytes_per_sec: -1.0,
            connection_type: None,
        };
        assert!(validate_network_conditions_override(&offline).is_ok());

        let throttled = CdpNetworkConditionsOverride {
            offline: false,
            latency_ms: 400.0,
            download_throughput_bytes_per_sec: 50_000.0,
            upload_throughput_bytes_per_sec: 20_000.0,
            connection_type: Some("cellular3g".to_owned()),
        };
        assert!(validate_network_conditions_override(&throttled).is_ok());

        let no_op = CdpNetworkConditionsOverride {
            offline: false,
            latency_ms: 0.0,
            download_throughput_bytes_per_sec: -1.0,
            upload_throughput_bytes_per_sec: -1.0,
            connection_type: None,
        };
        assert!(validate_network_conditions_override(&no_op).is_err());

        let bad_latency = CdpNetworkConditionsOverride {
            latency_ms: -1.0,
            ..offline.clone()
        };
        assert!(validate_network_conditions_override(&bad_latency).is_err());

        let bad_throughput = CdpNetworkConditionsOverride {
            download_throughput_bytes_per_sec: -2.0,
            ..offline.clone()
        };
        assert!(validate_network_conditions_override(&bad_throughput).is_err());

        let bad_connection = CdpNetworkConditionsOverride {
            connection_type: Some("lte".to_owned()),
            ..offline
        };
        assert!(validate_network_conditions_override(&bad_connection).is_err());
    }
}
