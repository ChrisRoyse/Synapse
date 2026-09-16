use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_core::{Backend, ElementId};

use crate::m2::{
    default_auto_wait_timeout_ms,
    postcondition::{ActPostcondition, default_verify_timeout_ms, postcondition_not_requested},
};

const DEFAULT_HOLD_MS: u32 = 33;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActPressParams {
    pub keys: Vec<String>,
    #[serde(default = "default_hold_ms")]
    #[schemars(default = "default_hold_ms", range(min = 1, max = 30000))]
    pub hold_ms: u32,
    #[serde(default = "default_press_backend")]
    #[schemars(default = "default_press_backend")]
    pub backend: PressBackend,
    #[serde(default)]
    #[schemars(default)]
    pub verify_delta: bool,
    #[serde(default)]
    #[schemars(
        default,
        description = "When verify_delta=true, accept a foreground-window identity change as the observed postcondition. Defaults false so unexpected focus loss still fails closed."
    )]
    pub allow_foreground_change: bool,
    #[serde(default)]
    #[schemars(
        default,
        description = "Optional regex that the after-read foreground process name must match when allow_foreground_change=true. Invalid regexes fail before key input is sent."
    )]
    pub expected_foreground_process_regex: Option<String>,
    #[serde(default)]
    #[schemars(
        default,
        description = "Optional regex that the after-read foreground window title must match when allow_foreground_change=true. Invalid regexes fail before key input is sent."
    )]
    pub expected_foreground_title_regex: Option<String>,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
    #[serde(default)]
    #[schemars(
        default,
        description = "Explicit per-call routing override (HWND). Routes keyboard input to this window's background CDP/PostMessage tier instead of the session's bound target, making multi-window / multi-agent automation deterministic (mirrors observe.window_hwnd). With cdp_target_id, the browser window that owns the CDP endpoint."
    )]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    #[schemars(
        default,
        description = "Explicit per-call CDP target id to route keyboard input to a specific browser tab. Requires window_hwnd. A CDP targetId is a stable routing handle that survives window-handle recycling across multi-agent use."
    )]
    pub cdp_target_id: Option<String>,
    #[serde(default)]
    #[schemars(
        default,
        description = "Opt in to pre-action CDP actionability polling before pressing keys. Requires auto_wait_element_id and waits until the web node is attached, visible, stable, enabled, and receiving events before dispatch. Default false preserves existing key semantics."
    )]
    pub auto_wait: bool,
    #[serde(default = "default_auto_wait_timeout_ms")]
    #[schemars(default = "default_auto_wait_timeout_ms", range(min = 50, max = 30000))]
    pub auto_wait_timeout_ms: u32,
    #[serde(default)]
    #[schemars(
        default,
        description = "CDP web element whose actionability is polled when auto_wait=true for act_press."
    )]
    pub auto_wait_element_id: Option<ElementId>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActKeymapParams {
    pub alias: String,
    #[serde(default = "default_hold_ms")]
    #[schemars(default = "default_hold_ms", range(min = 1, max = 30000))]
    pub hold_ms: u32,
    #[serde(default = "default_press_backend")]
    #[schemars(default = "default_press_backend")]
    pub backend: PressBackend,
    #[serde(default)]
    #[schemars(
        default,
        description = "Explicit per-call routing override (HWND); see act_press.window_hwnd."
    )]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    #[schemars(
        default,
        description = "Explicit per-call CDP target id; see act_press.cdp_target_id. Requires window_hwnd."
    )]
    pub cdp_target_id: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PressBackend {
    Software,
    Hardware,
    Auto,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActPressResponse {
    pub ok: bool,
    pub keys_pressed: u32,
    pub elapsed_ms: u32,
    pub backend_used: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub postcondition: ActPostcondition,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActKeymapResponse {
    pub ok: bool,
    pub alias: String,
    pub resolved_binding: String,
    pub resolved_keys: Vec<String>,
    pub hold_ms: u32,
    pub keys_pressed: u32,
    pub elapsed_ms: u32,
    pub backend_used: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

impl PressBackend {
    pub(in crate::m2::press) const fn to_backend(self) -> Backend {
        match self {
            Self::Software => Backend::Software,
            Self::Hardware => Backend::Hardware,
            Self::Auto => Backend::Auto,
        }
    }
}

pub(in crate::m2::press) const fn default_hold_ms() -> u32 {
    DEFAULT_HOLD_MS
}

pub(in crate::m2::press) const fn default_press_backend() -> PressBackend {
    PressBackend::Auto
}

pub(in crate::m2::press) fn press_postcondition_not_requested() -> ActPostcondition {
    postcondition_not_requested("act_press", "foreground_focused_ui_or_pixels")
}
