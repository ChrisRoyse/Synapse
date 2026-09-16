use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Backend, EventFilter, OcrBackend, PerceptionMode, ProfileId};

pub const PROFILE_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: ProfileId,
    pub label: String,
    pub version: String,
    pub use_scope: ProfileUseScope,
    #[serde(default)]
    pub matches: Vec<ProfileMatch>,
    pub mode: PerceptionMode,
    pub capture: ProfileCapture,
    pub detection: ProfileDetection,
    pub ocr: ProfileOcr,
    #[serde(default)]
    pub hud: Vec<HudFieldSpec>,
    #[serde(default)]
    pub keymap: BTreeMap<String, String>,
    pub backends: ProfileBackends,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub event_extensions: Vec<EventExtension>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_appid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_class: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_args: Vec<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProfileUseScope {
    Productivity,
    SinglePlayer,
    OperatorOwnedTest,
    SanctionedResearch,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileCapture {
    pub target: ProfileCaptureTarget,
    pub min_update_interval_ms: u32,
    pub cursor_visible: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileCaptureTarget {
    ForegroundWindow,
    PrimaryMonitor,
    MonitorIndex { index: u32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileDetection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default)]
    pub classes_of_interest: Vec<String>,
    pub confidence_threshold: f32,
    pub max_detections: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileOcr {
    pub default_backend: OcrBackend,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<HudRegion>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parser_config: BTreeMap<String, String>,
}

pub const DEFAULT_HUD_CONFIDENCE_THRESHOLD: f32 = 0.85;

#[must_use]
pub const fn default_hud_confidence_threshold() -> f32 {
    DEFAULT_HUD_CONFIDENCE_THRESHOLD
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HudFieldSpec {
    pub name: String,
    pub region: HudRegion,
    pub extractor: HudExtractor,
    pub parser: HudParser,
    #[serde(default = "default_hud_confidence_threshold")]
    pub confidence_threshold: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HudRegion {
    Absolute {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    FractionOfWindow {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    AnchoredToEdge {
        edge: WindowEdge,
        x_offset: i32,
        y_offset: i32,
        w: i32,
        h: i32,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowEdge {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
    Center,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HudExtractor {
    WinrtOcr,
    Crnn {
        model_id: String,
    },
    TemplateMatch {
        templates: Vec<String>,
    },
    ColorRatio {
        sample_points: Vec<(i32, i32)>,
        mapping: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HudParser {
    Number,
    BoundedInteger {
        min: u32,
        max: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_on_no_text: Option<u32>,
    },
    FractionNumerator,
    FractionDenominator,
    Regex {
        pattern: String,
        group: u32,
    },
    Enum {
        mapping: BTreeMap<String, String>,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileBackends {
    pub default: Backend,
    pub keyboard_default: Backend,
    pub mouse_default: Backend,
    pub pad_default: Backend,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventExtension {
    pub name: String,
    pub from_filter: EventFilter,
    pub emits_kind: String,
}
