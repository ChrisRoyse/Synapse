use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use chrono::Utc;
use synapse_core::{
    AccessibleNode, AudioContext, CaptureRuntimeReadback, CdpDiagnostics, ClipboardSummary,
    DetectedEntity, EventSummary, FocusedElement, ForegroundContext, FsEvent, HudReadings,
    InputBackendDiagnostics, Observation, ObservationCaptureConfig, ObservationDiagnostics,
    ObservationElementsPage, PerceptionMode, SensorStatus, WebPerceptionPath,
};

use crate::{PerceptionError, PerceptionResult};

const DEFAULT_MAX_ELEMENTS: usize = 60;
const DEFAULT_MAX_DEPTH: u32 = 2;
const DEFAULT_MAX_ENTITIES: usize = 60;
const SPARSE_A11Y_NODE_THRESHOLD: usize = 2;
const SPARSE_A11Y_DEPTH_THRESHOLD: u32 = 1;
const SENSOR_KEYS: [&str; 5] = ["a11y", "capture", "detection", "ocr", "audio"];

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ObserveInclude {
    pub focused: bool,
    pub elements: bool,
    pub entities: bool,
    pub hud: bool,
    pub audio: bool,
    pub events: bool,
    pub clipboard: bool,
    pub fs: bool,
    pub diagnostics: bool,
    /// When true, `elements` is filtered to interactable controls only (edits,
    /// buttons, links, form widgets) before pagination, and the structural
    /// depth cut is skipped — deep web form fields are exactly what this mode
    /// exists to surface (#882).
    pub interactable_only: bool,
    pub max_subtree_depth: u32,
    pub max_subtree_nodes: usize,
    pub element_offset: usize,
    pub max_entities: usize,
}

impl Default for ObserveInclude {
    fn default() -> Self {
        Self {
            focused: true,
            elements: true,
            entities: true,
            hud: true,
            audio: false,
            events: true,
            clipboard: false,
            fs: false,
            diagnostics: true,
            interactable_only: false,
            max_subtree_depth: DEFAULT_MAX_DEPTH,
            max_subtree_nodes: DEFAULT_MAX_ELEMENTS,
            element_offset: 0,
            max_entities: DEFAULT_MAX_ENTITIES,
        }
    }
}

impl ObserveInclude {
    /// True when any requested slot needs window/foreground perception — the
    /// a11y/screen gather bound to a specific `HWND`.
    ///
    /// The global slots (`fs`, `clipboard`, `audio`) read host-wide state and
    /// never need a window. An observe requesting only those must not be
    /// refused when the session's active target is a Chrome CDP tab (#1508),
    /// nor silently downgraded into reading the human foreground window: it has
    /// nothing to read from a window at all. Everything else — focused,
    /// elements, entities, interactable filtering, hud, events, diagnostics —
    /// is derived from (or scoped to) the target window and keeps the
    /// fail-closed CDP-target refusal.
    #[must_use]
    pub const fn requires_window_perception(&self) -> bool {
        self.focused
            || self.elements
            || self.entities
            || self.interactable_only
            || self.hud
            || self.events
            || self.diagnostics
    }

    #[must_use]
    pub const fn focused_only() -> Self {
        Self {
            focused: true,
            elements: false,
            entities: false,
            hud: false,
            audio: false,
            events: false,
            clipboard: false,
            fs: false,
            diagnostics: true,
            interactable_only: false,
            max_subtree_depth: DEFAULT_MAX_DEPTH,
            max_subtree_nodes: DEFAULT_MAX_ELEMENTS,
            element_offset: 0,
            max_entities: DEFAULT_MAX_ENTITIES,
        }
    }
}

/// Roles that mark a node as an interactable control in both element
/// vocabularies Synapse produces. Normalized comparison (lowercase, spaces and
/// underscores removed) bridges the three producers:
/// - CDP web AX roles: `button`, `link`, `textbox`, `searchbox`, `combobox`,
///   `listbox`, `option`, `checkbox`, `radio`, `switch`, `slider`,
///   `spinbutton`, `menuitem`, `tab`, `treeitem`, `gridcell` (the de facto
///   interactive-role set used by accessibility-snapshot agent tooling)
/// - UIA `ControlType` debug names: `Edit`, `Button`, `SplitButton`,
///   `Hyperlink`, `ComboBox`, `CheckBox`, `RadioButton`, `MenuItem`,
///   `TabItem`, `TreeItem`, `ListItem`, `Slider`, `Spinner`
/// - Chromium-UIA localized roles: `check box`, `combo box`, `radio button`,
///   `list item`, `menu item`, `tab item`, `hyperlink`, `edit`
const INTERACTABLE_ROLES: [&str; 28] = [
    "button",
    "checkbox",
    "combobox",
    "edit",
    "gridcell",
    "hyperlink",
    "link",
    "listbox",
    "listitem",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "radiobutton",
    "searchbox",
    "slider",
    "spinbutton",
    "spinner",
    "splitbutton",
    "switch",
    "tab",
    "tabitem",
    "textarea",
    "textbox",
    "textfield",
    "togglebutton",
    "treeitem",
];

/// True when `node` is an interactable control an agent can act on.
///
/// Disabled nodes are never interactable. A node qualifies by role (covers CDP
/// web nodes, whose `patterns` are always empty) or by exposing an actionable
/// UIA pattern (`Invoke`/`Toggle`/`Value`/`SelectionItem`/`ExpandCollapse`/
/// `RangeValue`) on a non-structural role. `document` is special-cased: it is
/// interactable only when it exposes a Value or Text pattern (Chromium maps
/// `contenteditable` composers to the `document` role).
#[must_use]
pub fn is_interactable_node(node: &AccessibleNode) -> bool {
    if !node.enabled {
        return false;
    }
    let role = normalized_role(&node.role);
    if INTERACTABLE_ROLES.contains(&role.as_str()) {
        return true;
    }
    if role == "document" {
        return node.patterns.iter().any(|pattern| {
            matches!(
                pattern,
                synapse_core::UiaPattern::Value | synapse_core::UiaPattern::Text
            )
        });
    }
    if matches!(
        role.as_str(),
        "scrollbar" | "progressbar" | "titlebar" | "window" | "pane" | "group" | "generic"
    ) {
        return false;
    }
    node.patterns.iter().any(|pattern| {
        matches!(
            pattern,
            synapse_core::UiaPattern::Invoke
                | synapse_core::UiaPattern::Toggle
                | synapse_core::UiaPattern::Value
                | synapse_core::UiaPattern::SelectionItem
                | synapse_core::UiaPattern::ExpandCollapse
                | synapse_core::UiaPattern::RangeValue
        )
    })
}

fn normalized_role(role: &str) -> String {
    role.chars()
        .filter(|character| !character.is_whitespace() && *character != '_' && *character != '-')
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct A11yTreeSummary {
    pub node_count: usize,
    pub max_depth: u32,
}

impl A11yTreeSummary {
    #[must_use]
    pub fn from_nodes(nodes: &[AccessibleNode]) -> Self {
        let max_depth = nodes
            .iter()
            .map(|node| node.depth)
            .max()
            .unwrap_or_default();
        Self {
            node_count: nodes.len(),
            max_depth,
        }
    }

    #[must_use]
    pub const fn is_sparse(&self) -> bool {
        self.node_count < SPARSE_A11Y_NODE_THRESHOLD || self.max_depth < SPARSE_A11Y_DEPTH_THRESHOLD
    }
}

#[derive(Clone, Debug)]
pub struct ObservationInput {
    pub foreground: ForegroundContext,
    pub is_minimized: bool,
    pub focused: Option<FocusedElement>,
    pub elements: Vec<AccessibleNode>,
    pub entities: Vec<DetectedEntity>,
    pub hud: HudReadings,
    pub audio: AudioContext,
    pub recent_events: Vec<EventSummary>,
    pub clipboard_summary: Option<ClipboardSummary>,
    pub fs_recent: Vec<FsEvent>,
    pub sensor_latency_ms: BTreeMap<String, f32>,
    pub a11y_status: SensorStatus,
    pub capture_status: SensorStatus,
    pub detection_status: SensorStatus,
    pub audio_status: SensorStatus,
    pub mode_override: Option<PerceptionMode>,
    pub capture_config: Option<ObservationCaptureConfig>,
    pub capture_runtime: Option<CaptureRuntimeReadback>,
    pub input_backends: Option<InputBackendDiagnostics>,
    /// CDP probe/attach outcome for the foreground (Chromium-family only).
    /// Threaded into [`ObservationDiagnostics::cdp`] by [`assemble`].
    pub cdp: Option<CdpDiagnostics>,
    /// Which perception path produced web content (Chromium-family only).
    /// Threaded into [`ObservationDiagnostics::web_path`] by [`assemble`].
    pub web_path: Option<WebPerceptionPath>,
}

impl ObservationInput {
    #[must_use]
    pub fn new(foreground: ForegroundContext) -> Self {
        Self {
            foreground,
            is_minimized: false,
            focused: None,
            elements: Vec::new(),
            entities: Vec::new(),
            hud: HudReadings::default(),
            audio: AudioContext::default(),
            recent_events: Vec::new(),
            clipboard_summary: None,
            fs_recent: Vec::new(),
            sensor_latency_ms: BTreeMap::new(),
            a11y_status: SensorStatus::Unavailable,
            capture_status: SensorStatus::Unavailable,
            detection_status: SensorStatus::Disabled,
            audio_status: SensorStatus::Disabled,
            mode_override: None,
            capture_config: None,
            capture_runtime: None,
            input_backends: None,
            cdp: None,
            web_path: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ObservationAssembler {
    next_seq: AtomicU64,
}

impl ObservationAssembler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(1),
        }
    }

    /// Fuses current perception producer state into one `Observation`.
    ///
    /// # Errors
    ///
    /// Returns `OBSERVE_NO_PERCEPTION_AVAILABLE` when all sensor inputs are
    /// unavailable or disabled, and `OBSERVE_INTERNAL` when serialization fails.
    pub fn assemble(
        &self,
        include: ObserveInclude,
        input: ObservationInput,
    ) -> PerceptionResult<Observation> {
        let started = Instant::now();
        ensure_any_sensor_available(include, &input)?;
        let summary = A11yTreeSummary::from_nodes(&input.elements);
        let mode = input
            .mode_override
            .unwrap_or_else(|| auto_mode_with_a11y(&input.foreground, &summary));
        let cdp = input.cdp.clone();
        let web_path = input.web_path;
        let (elements, elements_truncated, elements_page) =
            filter_elements(input.elements, include);
        let (entities, entities_truncated) = filter_entities(input.entities, include);
        // #882: diagnostic payloads are opt-in. When the caller's include set
        // does not request diagnostics, the heavy repeated blocks
        // (input_backends, cdp probe evidence, capture config/runtime) are
        // dropped from the wire shape. web_path stays — it is one enum and is
        // the fidelity signal agents must always see (#682).
        let (capture_config, capture_runtime, input_backends, cdp) = if include.diagnostics {
            (
                input.capture_config,
                input.capture_runtime,
                input.input_backends,
                cdp,
            )
        } else {
            (None, None, None, None)
        };
        let mut observation = Observation {
            seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
            at: Utc::now(),
            mode,
            foreground: input.foreground,
            perceived_text_notice: None,
            suspected_injection: Vec::new(),
            focused: include.focused.then_some(input.focused).flatten(),
            elements,
            entities,
            hud: if include.hud {
                input.hud
            } else {
                HudReadings::default()
            },
            audio: if include.audio {
                input.audio
            } else {
                AudioContext::default()
            },
            recent_events: if include.events {
                input.recent_events
            } else {
                Vec::new()
            },
            clipboard_summary: include
                .clipboard
                .then_some(input.clipboard_summary)
                .flatten(),
            fs_recent: if include.fs {
                input.fs_recent
            } else {
                Vec::new()
            },
            diagnostics: ObservationDiagnostics {
                assembled_in_ms: started.elapsed().as_secs_f32() * 1000.0,
                sensor_latency_ms: bounded_sensor_latency(input.sensor_latency_ms),
                a11y_enabled: include.focused || include.elements || include.events,
                pixel_enabled: include.entities || include.hud,
                audio_enabled: include.audio,
                a11y_status: input.a11y_status,
                capture_status: input.capture_status,
                detection_status: input.detection_status,
                audio_status: input.audio_status,
                is_minimized: input.is_minimized,
                capture_config,
                capture_runtime,
                input_backends,
                cdp,
                web_path,
                elements_truncated,
                elements_page,
                entities_truncated,
                size_bytes: 0,
                size_estimate_tokens: 0,
            },
        };
        update_size_fields(&mut observation)?;
        update_size_fields(&mut observation)?;
        Ok(observation)
    }
}

/// Assembles one observation with a fresh sequence counter.
///
/// # Errors
///
/// Returns the same errors as [`ObservationAssembler::assemble`].
pub fn assemble(include: ObserveInclude, input: ObservationInput) -> PerceptionResult<Observation> {
    ObservationAssembler::new().assemble(include, input)
}

/// Assembles one observation using default include filters.
///
/// # Errors
///
/// Returns the same errors as [`ObservationAssembler::assemble`].
pub fn assemble_from_input(input: ObservationInput) -> PerceptionResult<Observation> {
    assemble(ObserveInclude::default(), input)
}

#[must_use]
pub fn auto_mode(foreground: &ForegroundContext) -> PerceptionMode {
    if is_known_game_process(&foreground.process_name) {
        PerceptionMode::Hybrid
    } else {
        PerceptionMode::A11yOnly
    }
}

#[must_use]
pub fn auto_mode_with_a11y(
    foreground: &ForegroundContext,
    summary: &A11yTreeSummary,
) -> PerceptionMode {
    if is_known_game_process(&foreground.process_name) || summary.is_sparse() {
        PerceptionMode::Hybrid
    } else {
        PerceptionMode::A11yOnly
    }
}

/// Parses a manual perception-mode override.
///
/// # Errors
///
/// Returns `PERCEPTION_MODE_INVALID` for unknown strings.
pub fn parse_perception_mode(value: &str) -> PerceptionResult<PerceptionMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "a11y_only" => Ok(PerceptionMode::A11yOnly),
        "pixel_only" => Ok(PerceptionMode::PixelOnly),
        "hybrid" => Ok(PerceptionMode::Hybrid),
        "auto" => Ok(PerceptionMode::Auto),
        _ => Err(PerceptionError::PerceptionModeInvalid {
            value: value.to_owned(),
        }),
    }
}

#[must_use]
pub fn bounded_sensor_latency(input: BTreeMap<String, f32>) -> BTreeMap<String, f32> {
    input
        .into_iter()
        .filter(|(key, value)| SENSOR_KEYS.contains(&key.as_str()) && value.is_finite())
        .collect()
}

#[must_use]
pub fn is_known_game_process(process_name: &str) -> bool {
    matches!(
        process_name.to_ascii_lowercase().as_str(),
        "eldenring.exe"
            | "fortniteclient-win64-shipping.exe"
            | "game.exe"
            | "minecraft.exe"
            | "overwatch.exe"
            | "starfield.exe"
            | "valorant.exe"
    )
}

fn filter_elements(
    mut elements: Vec<AccessibleNode>,
    include: ObserveInclude,
) -> (Vec<AccessibleNode>, bool, Option<ObservationElementsPage>) {
    let original_total = elements.len();
    if !include.elements {
        let truncated = !elements.is_empty();
        let page = truncated.then_some(ObservationElementsPage {
            total: original_total,
            offset: 0,
            limit: 0,
            next_offset: Some(0),
        });
        return (Vec::new(), truncated, page);
    }
    let depth_truncated = if include.interactable_only {
        // #882: the semantic filter replaces the structural depth cut — deep
        // web form fields (real DOM-chain depths) are the point of this mode.
        elements.retain(is_interactable_node);
        false
    } else {
        let before = elements.len();
        elements.retain(|node| node.depth <= include.max_subtree_depth);
        elements.len() != before
    };
    let total = elements.len();
    let offset = include.element_offset.min(total);
    let limit = include.max_subtree_nodes;
    let next_offset = offset.checked_add(limit).filter(|next| *next < total);
    let paged = next_offset.is_some();
    let page = Some(ObservationElementsPage {
        total,
        offset,
        limit,
        next_offset,
    });
    let elements = elements.into_iter().skip(offset).take(limit).collect();
    (elements, depth_truncated || paged, page)
}

fn filter_entities(
    mut entities: Vec<DetectedEntity>,
    include: ObserveInclude,
) -> (Vec<DetectedEntity>, bool) {
    if !include.entities {
        return (Vec::new(), !entities.is_empty());
    }
    let truncated = entities.len() > include.max_entities;
    if truncated {
        entities.truncate(include.max_entities);
    }
    (entities, truncated)
}

fn ensure_any_sensor_available(
    include: ObserveInclude,
    input: &ObservationInput,
) -> PerceptionResult<()> {
    // Global-only observes (fs/clipboard/audio) read host-wide state that is
    // structurally available; an empty result is valid data, not a perception
    // failure. Only window/foreground perception can be genuinely unavailable
    // (no reachable a11y/capture/detection sensor), so gate the failure on
    // requests that actually need a window (#1508).
    if !include.requires_window_perception() {
        return Ok(());
    }
    let statuses = [
        &input.a11y_status,
        &input.capture_status,
        &input.detection_status,
        &input.audio_status,
    ];
    if statuses.iter().any(|status| {
        matches!(
            status,
            SensorStatus::Healthy
                | SensorStatus::DegradedLatency { .. }
                | SensorStatus::DegradedSensorFailed { .. }
        )
    }) {
        return Ok(());
    }
    Err(PerceptionError::ObserveNoPerceptionAvailable {
        detail: "all perception producers unavailable or disabled".to_owned(),
    })
}

fn update_size_fields(observation: &mut Observation) -> PerceptionResult<()> {
    let size_bytes = u32::try_from(
        serde_json::to_vec(observation)
            .map_err(|err| PerceptionError::ObserveInternal {
                detail: err.to_string(),
            })?
            .len(),
    )
    .unwrap_or(u32::MAX);
    observation.diagnostics.size_bytes = size_bytes;
    observation.diagnostics.size_estimate_tokens = size_bytes.div_ceil(4);
    Ok(())
}
