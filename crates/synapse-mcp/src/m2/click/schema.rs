use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use synapse_core::{AimCurve, AimNaturalParams, Backend, ElementId, MouseButton};

use crate::m2::default_auto_wait_timeout_ms;
pub(in crate::m2::click) use crate::m2::postcondition::default_verify_timeout_ms;
use crate::m2::postcondition::{
    ActPostcondition, postcondition_not_requested as base_postcondition_not_requested,
};

const DEFAULT_CLICK_HOLD_MS: u32 = 120;

#[derive(Clone, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClickParams {
    pub target: ActClickTarget,
    #[serde(default = "default_click_button")]
    #[schemars(default = "default_click_button")]
    pub button: MouseButton,
    #[serde(default = "default_click_count")]
    #[schemars(default = "default_click_count", range(min = 1, max = 3))]
    pub clicks: u8,
    #[serde(default)]
    pub modifiers: Vec<ClickModifier>,
    #[serde(default = "default_click_velocity_profile")]
    #[schemars(default = "default_click_velocity_profile")]
    pub velocity_profile: ClickVelocityProfile,
    #[serde(default = "default_click_duration_ms")]
    #[schemars(default = "default_click_duration_ms")]
    pub duration_ms: u32,
    #[serde(default = "default_click_hold_ms")]
    #[schemars(default = "default_click_hold_ms", range(min = 1, max = 30000))]
    pub hold_ms: u32,
    #[serde(default = "default_click_backend")]
    #[schemars(default = "default_click_backend")]
    pub backend: Backend,
    #[serde(default = "default_use_invoke_pattern")]
    #[schemars(default = "default_use_invoke_pattern")]
    pub use_invoke_pattern: bool,
    #[serde(default = "default_coordinate_fallback_on_unsupported")]
    #[schemars(default = "default_coordinate_fallback_on_unsupported")]
    pub coordinate_fallback_on_unsupported: bool,
    #[serde(default)]
    #[schemars(default)]
    pub verify_delta: bool,
    #[serde(default)]
    #[schemars(
        default,
        range(min = 1, max = 4_294_967_295_u64),
        description = "Optional target top-level HWND that verify_delta must use as the physical Source of Truth for point clicks. Intended for target-scoped routers that already validated and bound the native window; stale or conflicting HWNDs fail closed before input."
    )]
    pub verify_target_window_hwnd: Option<i64>,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
    #[serde(default)]
    #[schemars(
        default,
        description = "Opt in to pre-action CDP actionability polling for element targets. When true, Synapse scrolls the web node into view and waits until attached, visible, stable, enabled, and receiving events before dispatch. Default false preserves existing click semantics."
    )]
    pub auto_wait: bool,
    #[serde(default = "default_auto_wait_timeout_ms")]
    #[schemars(default = "default_auto_wait_timeout_ms", range(min = 50, max = 30000))]
    pub auto_wait_timeout_ms: u32,
    #[serde(skip)]
    #[schemars(skip)]
    pub deprecated_curve_alias_used: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(untagged)]
pub enum ActClickTarget {
    Element(ActClickElementTarget),
    Point(ActClickPointTarget),
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClickElementTarget {
    pub element_id: ElementId,
}

#[derive(Copy, Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClickPointTarget {
    pub x: i32,
    pub y: i32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClickModifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClickVelocityProfile {
    Natural,
    Instant,
    Linear,
    EaseInOut,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClickResponse {
    pub ok: bool,
    pub used_invoke_pattern: bool,
    pub backend_used: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub tier_attempts: Vec<ActClickTierAttempt>,
    pub postcondition: ActClickPostcondition,
    pub press_hold_ms: u32,
    pub double_click_window_ms: u32,
    pub inter_click_delay_ms: u32,
    pub elapsed_ms: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActClickTierAttempt {
    pub tier: String,
    pub status: String,
    pub reason_code: Option<String>,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub required_foreground: bool,
}

pub type ActClickPostcondition = ActPostcondition;

impl<'de> Deserialize<'de> for ActClickParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawActClickParams {
            target: ActClickTarget,
            #[serde(default = "default_click_button")]
            button: MouseButton,
            #[serde(default = "default_click_count")]
            clicks: u8,
            #[serde(default)]
            modifiers: Vec<ClickModifier>,
            #[serde(default)]
            velocity_profile: Option<ClickVelocityProfile>,
            #[serde(default)]
            curve: Option<ClickVelocityProfile>,
            #[serde(default = "default_click_duration_ms")]
            duration_ms: u32,
            #[serde(default = "default_click_hold_ms")]
            hold_ms: u32,
            #[serde(default = "default_click_backend")]
            backend: Backend,
            #[serde(default = "default_use_invoke_pattern")]
            use_invoke_pattern: bool,
            #[serde(default = "default_coordinate_fallback_on_unsupported")]
            coordinate_fallback_on_unsupported: bool,
            #[serde(default)]
            verify_delta: bool,
            #[serde(default)]
            verify_target_window_hwnd: Option<i64>,
            #[serde(default = "default_verify_timeout_ms")]
            verify_timeout_ms: u32,
            #[serde(default)]
            auto_wait: bool,
            #[serde(default = "default_auto_wait_timeout_ms")]
            auto_wait_timeout_ms: u32,
        }

        let raw = RawActClickParams::deserialize(deserializer)?;
        let (velocity_profile, deprecated_curve_alias_used) =
            match (raw.velocity_profile, raw.curve) {
                (Some(_), Some(_)) => {
                    return Err(de::Error::custom(
                        "act_click accepts velocity_profile or deprecated curve, not both",
                    ));
                }
                (Some(profile), None) => (profile, false),
                (None, Some(profile)) => (profile, true),
                (None, None) => (default_click_velocity_profile(), false),
            };

        Ok(Self {
            target: raw.target,
            button: raw.button,
            clicks: raw.clicks,
            modifiers: raw.modifiers,
            velocity_profile,
            duration_ms: raw.duration_ms,
            hold_ms: raw.hold_ms,
            backend: raw.backend,
            use_invoke_pattern: raw.use_invoke_pattern,
            coordinate_fallback_on_unsupported: raw.coordinate_fallback_on_unsupported,
            verify_delta: raw.verify_delta,
            verify_target_window_hwnd: raw.verify_target_window_hwnd,
            verify_timeout_ms: raw.verify_timeout_ms,
            auto_wait: raw.auto_wait,
            auto_wait_timeout_ms: raw.auto_wait_timeout_ms,
            deprecated_curve_alias_used,
        })
    }
}

impl ClickVelocityProfile {
    pub(in crate::m2::click) const fn to_aim_curve(self) -> AimCurve {
        match self {
            Self::Natural => AimCurve::Natural {
                params: AimNaturalParams::FAST,
            },
            Self::Instant => AimCurve::Instant,
            Self::Linear => AimCurve::Linear,
            Self::EaseInOut => AimCurve::EaseInOut,
        }
    }
}

pub(in crate::m2::click) const fn default_click_button() -> MouseButton {
    MouseButton::Left
}

pub(in crate::m2::click) const fn default_click_count() -> u8 {
    1
}

pub(in crate::m2::click) const fn default_click_velocity_profile() -> ClickVelocityProfile {
    ClickVelocityProfile::Natural
}

pub(in crate::m2::click) const fn default_click_duration_ms() -> u32 {
    50
}

pub(in crate::m2::click) const fn default_click_hold_ms() -> u32 {
    DEFAULT_CLICK_HOLD_MS
}

pub(in crate::m2::click) const fn default_click_backend() -> Backend {
    Backend::Auto
}

pub(in crate::m2::click) const fn default_use_invoke_pattern() -> bool {
    true
}

pub(in crate::m2::click) const fn default_coordinate_fallback_on_unsupported() -> bool {
    true
}

pub(in crate::m2::click) fn postcondition_not_requested() -> ActClickPostcondition {
    base_postcondition_not_requested("act_click", "foreground_focused_ui_or_pixels")
}
