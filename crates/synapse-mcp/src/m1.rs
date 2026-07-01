mod detection;
mod ocr;
mod search;
mod sources;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rmcp::{ErrorData, handler::server::common, model::JsonObject, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use synapse_capture::{
    CAPTURE_CHANNEL_CAPACITY, CaptureBackend, CaptureConfig, CaptureController, CaptureTarget,
    CaptureThreadPriority, resolve_capture_target,
};
use synapse_core::{
    AccessibleNode, CaptureRuntimeReadback, ElementId, FocusedElement, ForegroundContext,
    ObservationCaptureConfig, ObservationCaptureTarget, OcrBackend, PerceptionMode, Profile,
    ProfileCapture, ProfileCaptureTarget, ProfileDetection, Rect, error_codes,
};
use synapse_perception::{ObservationInput, ObserveInclude, parse_perception_mode};

pub use detection::populate_detection_from_state;
use detection::{DetectionRuntime, DetectionRuntimeConfig, default_detection_config};
#[cfg(windows)]
pub use ocr::ocr_result_from_web_bitmap;
pub use ocr::{
    ReadTextCaptureSource, ResolvedReadTextRequest, effective_ocr_backend,
    read_text_request_uncached, resolve_read_text_request,
};
#[cfg(windows)]
pub use ocr::{read_text_request_for_captured_bitmap, read_text_request_from_bgra};
use search::{element_match, entity_match};
pub use sources::{
    ClipboardTimelineSample, FsRecentTracker, FsTimelineEvent,
    hidden_desktop_input_from_worker_snapshot, populate_clipboard_summary, populate_fs_recent,
    timeline_clipboard_enabled, timeline_file_activity_enabled,
};
use sources::{
    element_input_from_id, platform_input, synthetic_notepad_input, window_input_from_hwnd,
};

pub type SharedM1State = Arc<Mutex<M1State>>;
const MIN_CAPTURE_UPDATE_INTERVAL_MS: u64 = 16;
const MIN_CAPTURE_UPDATE_INTERVAL_MS_U32: u32 = 16;

#[derive(Debug)]
pub struct M1State {
    pub capture_config: CaptureConfig,
    pub capture_controller: CaptureController,
    pub capture_generation: u64,
    pub active_capture_config: ObservationCaptureConfig,
    pub perception_mode: PerceptionMode,
    pub manual_perception_mode: Option<PerceptionMode>,
    pub detection_config: DetectionRuntimeConfig,
    pub detection_runtime: DetectionRuntime,
    pub synthetic: Option<ObservationInput>,
    pub force_no_perception: bool,
    pub force_observe_internal: bool,
    /// Reproduce the real `GetForegroundWindow returned null` condition (locked
    /// screen / desktop focus / unattended session) deterministically so the
    /// no-foreground action-gate behavior (#1061) is testable without depending
    /// on ambient host focus at run time.
    pub force_no_foreground: bool,
    pub last_observed_foreground: Option<ForegroundContext>,
    pub everquest_log_cursor: Option<EverQuestLogCursorState>,
    pub everquest_event_seq: u64,
    pub fs_recent_tracker: FsRecentTracker,
}

impl M1State {
    #[must_use]
    pub fn from_env() -> Self {
        let synthetic = match std::env::var("SYNAPSE_MCP_SYNTHETIC_FIXTURE") {
            Ok(value) if value.eq_ignore_ascii_case("notepad") => Some(synthetic_notepad_input()),
            _ => None,
        };
        let force_no_perception = std::env::var("SYNAPSE_MCP_FORCE_NO_PERCEPTION")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        let force_observe_internal = std::env::var("SYNAPSE_MCP_FORCE_OBSERVE_INTERNAL")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        let force_no_foreground = std::env::var("SYNAPSE_MCP_FORCE_NO_FOREGROUND")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        Self {
            capture_config: CaptureConfig::default().with_env_backend(),
            capture_controller: CaptureController::new(),
            capture_generation: 0,
            active_capture_config: default_observation_capture_config(),
            perception_mode: PerceptionMode::Auto,
            manual_perception_mode: None,
            detection_config: default_detection_config(),
            detection_runtime: DetectionRuntime::default(),
            synthetic,
            force_no_perception,
            force_observe_internal,
            force_no_foreground,
            last_observed_foreground: None,
            everquest_log_cursor: None,
            everquest_event_seq: 0,
            fs_recent_tracker: FsRecentTracker::from_env(),
        }
    }

    #[must_use]
    pub fn capture_runtime_readback(&self) -> CaptureRuntimeReadback {
        let Some(handle) = self.capture_controller.active() else {
            return CaptureRuntimeReadback {
                status: "inactive".to_owned(),
                target: None,
                backend: None,
                selected_backend: Some(
                    capture_backend_name(self.capture_config.selected_backend()).to_owned(),
                ),
                generation: self.capture_controller.generation(),
                min_update_interval_ms: Some(
                    u32::try_from(self.capture_config.min_update_interval_ms)
                        .unwrap_or(u32::MAX)
                        .max(MIN_CAPTURE_UPDATE_INTERVAL_MS_U32),
                ),
                cursor_visible: Some(self.capture_config.cursor_visible),
                dirty_region_only: Some(self.capture_config.dirty_region_only),
                frames_captured: 0,
                frames_dropped: 0,
                latest_frame_seq: None,
                latest_frame_width: None,
                latest_frame_height: None,
                channel_len: 0,
                channel_capacity: CAPTURE_CHANNEL_CAPACITY,
                thread_priority: None,
                stop_requested: false,
            };
        };

        let stats = handle.stats();
        let active_config = handle.config();
        let latest_frame = stats.latest_frame();
        CaptureRuntimeReadback {
            status: "running".to_owned(),
            target: Some(observation_target_from_capture_target(
                &handle.target().target,
            )),
            backend: stats
                .effective_backend()
                .map(|backend| capture_backend_name(backend).to_owned()),
            selected_backend: Some(capture_backend_name(handle.target().backend).to_owned()),
            generation: self.capture_controller.generation(),
            min_update_interval_ms: Some(
                u32::try_from(active_config.min_update_interval_ms)
                    .unwrap_or(u32::MAX)
                    .max(MIN_CAPTURE_UPDATE_INTERVAL_MS_U32),
            ),
            cursor_visible: Some(active_config.cursor_visible),
            dirty_region_only: Some(active_config.dirty_region_only),
            frames_captured: stats.frames_captured(),
            frames_dropped: stats.frames_dropped(),
            latest_frame_seq: latest_frame.map(|frame| frame.frame_seq),
            latest_frame_width: latest_frame.map(|frame| frame.width),
            latest_frame_height: latest_frame.map(|frame| frame.height),
            channel_len: handle.channel_len(),
            channel_capacity: handle.channel_capacity(),
            thread_priority: Some(capture_thread_priority_name(stats.thread_priority())),
            stop_requested: handle.is_stop_requested(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EverQuestLogCursorState {
    pub path: PathBuf,
    pub offset: u64,
}

impl Default for M1State {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveParams {
    #[serde(default)]
    pub include: Vec<ObserveSlot>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub max_elements: Option<usize>,
    #[serde(default)]
    pub element_offset: Option<usize>,
    #[serde(default)]
    pub subtree_root: Option<ElementId>,
    #[serde(default)]
    pub since_event_seq: Option<u64>,
    /// Explicit per-call window override (HWND). Takes precedence over the
    /// session's active target; when both are absent, observe falls back to the
    /// global foreground (back-compat).
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObserveSlot {
    Focused,
    Elements,
    Entities,
    Hud,
    Audio,
    Events,
    Clipboard,
    Fs,
    Diagnostics,
    /// Elements filtered to interactable controls only (edits, buttons, links,
    /// form widgets). Implies `elements`; skips the structural depth cut and,
    /// when `depth` is not set, raises the gather depth to the maximum so deep
    /// native form fields are reachable without paging (#882).
    Interactable,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindParams {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name_substring: Option<String>,
    #[serde(default)]
    pub automation_id: Option<String>,
    #[serde(default)]
    pub scope: Option<FindScope>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub in_window: Option<ElementId>,
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindScope {
    Elements,
    Entities,
    #[default]
    Both,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindResponse {
    pub results: Vec<FindResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perceived_text_notice: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspected_injection: Vec<synapse_core::SuspectedInjectionAnnotation>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindResult {
    pub kind: FindResultKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<ElementId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_label: Option<String>,
    pub bbox: Rect,
    pub score: f32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindResultKind {
    Element,
    Entity,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadTextParams {
    /// OCR region. With `window_hwnd` or this MCP session's active window
    /// target, this is window-client-relative. With no target, this is an
    /// absolute screen region. When a target window is present and both
    /// `region` and `element_id` are omitted, OCR reads the whole target window.
    #[serde(default)]
    pub region: Option<Rect>,
    #[serde(default)]
    pub element_id: Option<ElementId>,
    /// Explicit per-call window override (HWND). Takes precedence over the
    /// session's active target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub backend: OcrBackend,
    #[serde(default)]
    pub lang_hint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureScreenshotParams {
    pub path: String,
    #[serde(default)]
    pub region: Option<Rect>,
    /// Explicit per-call window override (HWND). Takes precedence over the
    /// session's active target. When set, `region` is client-relative.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub overwrite: bool,
    /// Optional vision pixel budget: downscale (aspect-preserving) so the written
    /// image's total pixels never exceed this. Mirrors Anthropic computer-use
    /// resolution guidance (e.g. 1_150_000 for the 4.6 family, 3_750_000 for
    /// Opus 4.7). No-op when the native capture already fits. Must be > 0.
    #[serde(default)]
    pub max_pixels: Option<u64>,
    /// Optional vision pixel budget: downscale (aspect-preserving) so the written
    /// image's longest edge never exceeds this many pixels (e.g. 1568 for the 4.6
    /// family, 2576 for Opus 4.7). Combined with `max_pixels`, the more
    /// restrictive constraint wins. No-op when the native capture already fits.
    /// Must be > 0.
    #[serde(default)]
    pub max_long_edge: Option<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureScreenshotResponse {
    pub path: String,
    pub format: CaptureScreenshotFormat,
    pub capture_backend: String,
    pub region: Rect,
    /// Written image width in pixels (after any `max_pixels`/`max_long_edge` downscale).
    pub width: u32,
    /// Written image height in pixels (after any `max_pixels`/`max_long_edge` downscale).
    pub height: u32,
    /// Source capture width in pixels before any downscale (equals `width` when not downscaled).
    pub native_width: u32,
    /// Source capture height in pixels before any downscale (equals `height` when not downscaled).
    pub native_height: u32,
    /// Downscale ratio applied = written_long_edge / native_long_edge. 1.0 when not downscaled.
    /// Multiply a coordinate read off the written image by `1.0 / scale` to map it back to native pixels.
    pub scale: f64,
    pub bytes_written: u64,
    pub bitmap_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<ForegroundContext>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureScreenshotFormat {
    Png,
    Jpeg,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotOperation {
    #[default]
    Capture,
    Gif,
}

/// Public perception artifact facade. `capture` writes a still image through
/// the same path as `capture_screenshot`; `gif` records a bounded window GIF.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotParams {
    /// Operation to perform. Defaults to a still screenshot capture.
    #[serde(default)]
    pub operation: ScreenshotOperation,
    /// Output `.png`, `.jpg`, `.jpeg`, or `.gif` file path. Must be absolute.
    pub path: String,
    #[serde(default)]
    pub region: Option<Rect>,
    /// Explicit per-call window override (HWND). Takes precedence over the
    /// session's active target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub overwrite: bool,
    /// Still-image pixel budget. Valid only with `operation=capture`.
    #[serde(default)]
    pub max_pixels: Option<u64>,
    /// Still-image long-edge budget, or GIF frame long-edge budget.
    #[serde(default)]
    pub max_long_edge: Option<u32>,
    /// GIF recording duration. Valid only with `operation=gif`.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// GIF frame interval. Valid only with `operation=gif`.
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotResponse {
    pub operation: ScreenshotOperation,
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureScreenshotResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gif: Option<CaptureGifResponse>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureGifParams {
    /// Output `.gif` file path. Must be absolute.
    pub path: String,
    /// Total recording window in milliseconds (default 3000, max 60000).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Delay between captured frames in milliseconds (default 500, min 100).
    #[serde(default)]
    pub interval_ms: Option<u64>,
    /// Window HWND to record. Defaults to this session's bound target window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Downscale (aspect-preserving) so each frame's longest edge never exceeds
    /// this. Default 800; set 0 to disable.
    #[serde(default)]
    pub max_long_edge: Option<u32>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureGifResponse {
    pub path: String,
    pub frames_captured: usize,
    pub frames_requested: usize,
    pub width: u32,
    pub height: u32,
    pub native_width: u32,
    pub native_height: u32,
    pub interval_ms: u64,
    pub duration_ms: u64,
    pub elapsed_ms: u64,
    pub bytes_written: u64,
    pub capture_backend: String,
    pub window_hwnd: i64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserScreenshotParams {
    /// Output PNG/JPEG file path. Must be absolute.
    pub path: String,
    /// CDP/normal-bridge target id. Defaults to this MCP session's active CDP
    /// target. Normal Chrome bridge targets are shaped like `chrome-tab:<id>`.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only when passing an
    /// explicit `cdp_target_id` without an active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub scope: BrowserScreenshotScope,
    /// Page CSS-coordinate clip. Required with `scope=clip`; rejected for other
    /// scopes. Uses page/document coordinates, not viewport or screen coords.
    #[serde(default)]
    pub clip: Option<Rect>,
    /// Normal bridge element id returned by `browser_locate` /
    /// `browser_aria_snapshot`. Required with `scope=element`.
    #[serde(default)]
    pub element_id: Option<String>,
    /// Elements to obscure before capture. Restored after capture.
    #[serde(default)]
    pub masks: Vec<BrowserScreenshotMask>,
    /// Image format. Defaults to the file extension.
    #[serde(default)]
    pub format: Option<CaptureScreenshotFormat>,
    /// JPEG quality 0..=100. Ignored for PNG.
    #[serde(default)]
    pub quality: Option<u8>,
    /// Request transparent page background for PNG. The normal bridge also
    /// restores any inline background changes after capture.
    #[serde(default)]
    pub omit_background: bool,
    #[serde(default)]
    pub overwrite: bool,
    /// Optional caller wait budget, capped by the bridge command timeout.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// Optional vision pixel budget: downscale (aspect-preserving) so the written
    /// image's total pixels never exceed this. See `CaptureScreenshotParams::max_pixels`.
    /// No-op when the captured page image already fits. Must be > 0.
    #[serde(default)]
    pub max_pixels: Option<u64>,
    /// Optional vision pixel budget: downscale (aspect-preserving) so the written
    /// image's longest edge never exceeds this many pixels. See
    /// `CaptureScreenshotParams::max_long_edge`. No-op when it already fits. Must be > 0.
    #[serde(default)]
    pub max_long_edge: Option<u32>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserScreenshotScope {
    #[default]
    Viewport,
    FullPage,
    Clip,
    Element,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserScreenshotMask {
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub element_id: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserScreenshotResponse {
    pub path: String,
    pub format: CaptureScreenshotFormat,
    pub capture_backend: String,
    pub scope: BrowserScreenshotScope,
    pub page_region: Rect,
    /// Written image width in pixels (after any `max_pixels`/`max_long_edge` downscale).
    pub width: u32,
    /// Written image height in pixels (after any `max_pixels`/`max_long_edge` downscale).
    pub height: u32,
    /// Source capture width in pixels before any downscale (equals `width` when not downscaled).
    pub native_width: u32,
    /// Source capture height in pixels before any downscale (equals `height` when not downscaled).
    pub native_height: u32,
    /// Downscale ratio applied = written_long_edge / native_long_edge. 1.0 when not downscaled.
    pub scale: f64,
    pub bytes_written: u64,
    pub bitmap_sha256: String,
    pub cdp_target_id: String,
    pub tab_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    pub url: String,
    pub title: String,
    pub device_pixel_ratio: f64,
    pub viewport_width_css: f64,
    pub viewport_height_css: f64,
    pub scroll_width_css: f64,
    pub scroll_height_css: f64,
    pub tile_count: usize,
    pub mask_count: usize,
    pub omit_background: bool,
    pub required_foreground: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_os_foreground_before_hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_os_foreground_capture_hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_os_foreground_after_restore_hwnd: Option<i64>,
    pub restored_human_os_foreground: bool,
    pub backend_tier_used: String,
    pub source_of_truth: String,
    /// Stable machine-readable code when the screenshot was degraded from the
    /// normal page-screenshot lane. Absent on a full-fidelity bridge capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation_code: Option<String>,
    /// Physical readback source used to preserve target metadata during a
    /// degraded capture. Absent on a full-fidelity bridge capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_metadata_source: Option<String>,
    /// #1341/#1343: set when the normal Chrome bridge `captureVisibleTab` lane
    /// disconnected mid-capture (the MV3 service worker drops the WebSocket on
    /// some GPU/WebGL-heavy pages) and the screenshot was instead produced by a
    /// passive WGC capture of the owning Chrome window. Carries the original
    /// bridge error so the caller knows the image is a whole-window fallback,
    /// not a viewport/clip/element capture. Absent on a normal bridge capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPdfParams {
    /// Output PDF file path. Must be absolute and end in .pdf.
    pub path: String,
    /// CDP/normal-bridge target id. Defaults to this MCP session's active CDP
    /// target. Normal Chrome bridge targets are shaped like `chrome-tab:<id>`.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only when passing an explicit
    /// `cdp_target_id` without an active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub landscape: bool,
    #[serde(default)]
    pub print_background: bool,
    #[serde(default)]
    pub display_header_footer: bool,
    #[serde(default)]
    pub header_template: Option<String>,
    #[serde(default)]
    pub footer_template: Option<String>,
    /// Print scale in Chrome Page.printToPDF units, 0.1 through 2.0.
    #[serde(default)]
    pub scale: Option<f64>,
    /// Paper width in inches. Chrome defaults to 8.5.
    #[serde(default)]
    pub paper_width: Option<f64>,
    /// Paper height in inches. Chrome defaults to 11.
    #[serde(default)]
    pub paper_height: Option<f64>,
    #[serde(default)]
    pub margin_top: Option<f64>,
    #[serde(default)]
    pub margin_bottom: Option<f64>,
    #[serde(default)]
    pub margin_left: Option<f64>,
    #[serde(default)]
    pub margin_right: Option<f64>,
    #[serde(default)]
    pub page_ranges: Option<String>,
    #[serde(default)]
    pub prefer_css_page_size: bool,
    #[serde(default)]
    pub overwrite: bool,
    /// Optional caller wait budget, capped by the bridge command timeout.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPdfResponse {
    pub path: String,
    pub bytes_written: u64,
    pub pdf_sha256: String,
    pub capture_backend: String,
    pub cdp_target_id: String,
    pub tab_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    pub url: String,
    pub title: String,
    pub landscape: bool,
    pub print_background: bool,
    pub display_header_footer: bool,
    pub scale: f64,
    pub paper_width: f64,
    pub paper_height: f64,
    pub margin_top: f64,
    pub margin_bottom: f64,
    pub margin_left: f64,
    pub margin_right: f64,
    pub page_ranges: String,
    pub prefer_css_page_size: bool,
    pub required_foreground: bool,
    pub backend_tier_used: String,
    pub source_of_truth: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDownloadsOperation {
    /// Return matching Chrome download rows immediately.
    #[default]
    List,
    /// Block until a matching download reaches the requested state.
    Wait,
    /// Copy a completed matching download to `path`.
    Save,
    /// Move a completed matching download to `path` after a verified copy.
    Move,
}

/// Parameters for `browser_downloads` (#1106-#1109): enumerate, wait for, save,
/// or move normal-profile Chrome downloads through the bundled extension.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDownloadsParams {
    /// Operation to perform. Defaults to `list`.
    #[serde(default)]
    pub operation: BrowserDownloadsOperation,
    /// Browser HWND used to reach the already-open normal Chrome bridge. If
    /// omitted, the active session target's browser window is used; if no
    /// session target is set, the current human foreground Chromium window is
    /// used only as an explicit bridge discovery source and reported back.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Chrome download id. Use this for exact save/move of a prior list/wait row.
    #[serde(default)]
    pub download_id: Option<i64>,
    #[serde(default)]
    pub url_contains: Option<String>,
    #[serde(default)]
    pub filename_contains: Option<String>,
    #[serde(default)]
    pub mime_contains: Option<String>,
    /// Chrome download state filter: `in_progress`, `interrupted`, or `complete`.
    #[serde(default)]
    pub state: Option<String>,
    /// Only include downloads whose Chrome startTime is at or after this Unix
    /// timestamp in milliseconds.
    #[serde(default)]
    pub since_unix_ms: Option<u64>,
    /// Only include extension event rows at or after this cursor.
    #[serde(default)]
    pub since_event_seq: Option<u64>,
    /// Maximum matching rows/events to return. Default 50, max 500.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Wait budget for `wait`, `save`, and `move`. Default 30000, max 300000.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// Absolute destination file path for `save` or `move`.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDownloadEntry {
    pub id: i64,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_url: String,
    pub filename: String,
    pub filename_basename: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub start_time: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_time: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub estimated_end_time: String,
    pub state: String,
    pub paused: bool,
    pub can_resume: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub danger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub bytes_received: u64,
    pub total_bytes: i64,
    pub file_size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    pub incognito: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub referrer: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDownloadEvent {
    pub seq: u64,
    pub event_kind: String,
    pub timestamp_unix_ms: u64,
    pub download_id: i64,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_url: String,
    pub filename: String,
    pub filename_basename: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub danger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub bytes_received: u64,
    pub total_bytes: i64,
    pub file_size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDownloadsResponse {
    pub session_id: String,
    pub operation: BrowserDownloadsOperation,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_state: Option<String>,
    pub used_human_os_foreground_window: bool,
    pub condition_met: bool,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub returned: u32,
    pub event_count: u32,
    pub next_event_cursor: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_item: Option<BrowserDownloadEntry>,
    pub items: Vec<BrowserDownloadEntry>,
    pub events: Vec<BrowserDownloadEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_sha256: Option<String>,
    pub moved_file: bool,
    pub required_foreground: bool,
    pub backend_tier_used: String,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HiddenDesktopPipFrameParams {
    /// MCP session id whose session-owned hidden desktop should be viewed. If
    /// omitted, the caller's current MCP session is viewed.
    #[serde(default)]
    pub watched_session_id: Option<String>,
    /// Hidden-desktop top-level window HWND to capture.
    pub window_hwnd: i64,
    /// Output PNG/JPEG frame path. This is the read-only viewer surface.
    pub path: String,
    /// Optional client-relative region within `window_hwnd`.
    #[serde(default)]
    pub region: Option<Rect>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HiddenDesktopPipStreamStatus {
    Frame,
    Ended,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HiddenDesktopPipFrameResponse {
    pub stream_status: HiddenDesktopPipStreamStatus,
    pub watched_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watched_session_lifecycle: Option<String>,
    pub watched_window_hwnd: i64,
    pub viewer_surface: String,
    pub read_only: bool,
    pub input_forwarding: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub desktop_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_pids: Vec<u32>,
    pub resource_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<CaptureScreenshotFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Rect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitmap_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<ForegroundContext>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCaptureTargetParams {
    pub target: CaptureTargetParam,
    #[serde(default)]
    pub min_update_interval_ms: Option<u64>,
    #[serde(default)]
    pub cursor_visible: Option<bool>,
    #[serde(default)]
    pub dirty_region_only: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureTargetParam {
    Primary,
    Monitor { monitor_index: u32 },
    Window { window_hwnd: i64 },
    ElementWindow { element_id: ElementId },
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCaptureTargetResponse {
    pub previous: CaptureTargetWire,
    pub current: CaptureTargetWire,
    pub generation: u64,
    pub backend: String,
    pub capture_runtime: CaptureRuntimeReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureTargetWire {
    Primary,
    Monitor { monitor_index: u32 },
    Window { window_hwnd: i64 },
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPerceptionModeParams {
    pub mode: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetPerceptionModeResponse {
    pub previous: PerceptionMode,
    pub mode: PerceptionMode,
    pub rationale: String,
}

/// `set_target` request: bind this MCP session's active perception target
/// (epic #720, issue #736). Window targeting binds a native HWND; CDP
/// targeting binds a specific browser tab target within a browser HWND.
#[derive(Clone, Debug, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SetTargetParam {
    Window {
        window_hwnd: i64,
    },
    Cdp {
        window_hwnd: i64,
        cdp_target_id: String,
    },
}

impl<'de> Deserialize<'de> for SetTargetParam {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_set_target_param(value).map_err(serde::de::Error::custom)
    }
}

const SET_TARGET_ACCEPTED_SHAPES: &str = "accepted set_target target shapes are {\"kind\":\"window\",\"window_hwnd\":<integer>} or {\"kind\":\"cdp\",\"window_hwnd\":<integer>,\"cdp_target_id\":\"<target id>\"}";

fn parse_set_target_param(value: Value) -> Result<SetTargetParam, String> {
    let object = value.as_object().ok_or_else(|| {
        format!("set_target target must be an object; {SET_TARGET_ACCEPTED_SHAPES}")
    })?;
    if object.contains_key("hwnd") && !object.contains_key("kind") {
        return Err(format!(
            "set_target target does not accept legacy field `hwnd`; use `window_hwnd` with `kind:\"window\"`. {SET_TARGET_ACCEPTED_SHAPES}"
        ));
    }
    let kind = object.get("kind").and_then(Value::as_str).ok_or_else(|| {
        format!("set_target target is missing string field `kind`; {SET_TARGET_ACCEPTED_SHAPES}")
    })?;
    match kind {
        "window" => parse_set_target_window(object),
        "cdp" => parse_set_target_cdp(object),
        other => Err(format!(
            "set_target target kind must be `window` or `cdp`, got {other:?}; {SET_TARGET_ACCEPTED_SHAPES}"
        )),
    }
}

fn parse_set_target_window(object: &Map<String, Value>) -> Result<SetTargetParam, String> {
    reject_unknown_set_target_fields(object, &["kind", "window_hwnd"])?;
    Ok(SetTargetParam::Window {
        window_hwnd: required_i64(object, "window_hwnd")?,
    })
}

fn parse_set_target_cdp(object: &Map<String, Value>) -> Result<SetTargetParam, String> {
    reject_unknown_set_target_fields(object, &["kind", "window_hwnd", "cdp_target_id"])?;
    let cdp_target_id = object
        .get("cdp_target_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("set_target cdp target requires string field `cdp_target_id`; {SET_TARGET_ACCEPTED_SHAPES}")
        })?
        .to_owned();
    Ok(SetTargetParam::Cdp {
        window_hwnd: required_i64(object, "window_hwnd")?,
        cdp_target_id,
    })
}

fn reject_unknown_set_target_fields(
    object: &Map<String, Value>,
    accepted: &[&str],
) -> Result<(), String> {
    if let Some(field) = object
        .keys()
        .find(|field| !accepted.contains(&field.as_str()))
    {
        return Err(format!(
            "set_target target field `{field}` is not accepted; {SET_TARGET_ACCEPTED_SHAPES}"
        ));
    }
    Ok(())
}

fn required_i64(object: &Map<String, Value>, field: &str) -> Result<i64, String> {
    object.get(field).and_then(Value::as_i64).ok_or_else(|| {
        format!("set_target target requires integer field `{field}`; {SET_TARGET_ACCEPTED_SHAPES}")
    })
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTargetParams {
    pub target: SetTargetParam,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetWire {
    Window {
        window_hwnd: i64,
    },
    Cdp {
        window_hwnd: i64,
        cdp_target_id: String,
    },
}

/// Response shared by `set_target`/`get_target`/`clear_target`. `current` is the
/// target after the call (`None` when cleared/unset); `window_title`/
/// `process_name` echo the validated window so the agent sees exactly which
/// window it bound (fail-loud confirmation).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetResponse {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpOpenTabParams {
    /// Browser HWND with a reachable CDP endpoint. If omitted, the caller must
    /// already have a session window/CDP target set.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Initial URL for the background tab. Empty string opens about:blank.
    pub url: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpOpenTabResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_window_hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_os_foreground_before_hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_os_foreground_after_hwnd: Option<i64>,
    pub target_active: bool,
    pub target_highlighted: bool,
    pub requested_url: String,
    pub cdp_target_id: String,
    pub target_type: String,
    pub target_title: String,
    pub target_url: String,
    pub target_attached: bool,
    pub target_count_before: u32,
    pub target_count_after: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<TargetWire>,
    pub current: TargetWire,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpCloseTabParams {
    pub cdp_target_id: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpCloseTabResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub closed: bool,
    pub target_count_before: u32,
    pub target_count_after: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<TargetWire>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpTargetInfoParams {
    /// Browser HWND whose target table contains the CDP/Chrome bridge target.
    /// If omitted, the active session CDP target is used.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// CDP TargetID / Chrome bridge target id to read. If omitted, the active
    /// session CDP target is used.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpTargetInfoResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    pub target_type: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub active: bool,
    pub highlighted: bool,
    pub pinned: bool,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_element: Option<CdpActiveElementInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_text: Option<CdpPageTextInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_vitals: Option<CdpPageVitalsInfo>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTabsOperation {
    /// Enumerate tabs without changing browser or session state.
    #[default]
    List,
    /// Bind an existing listed tab as this MCP session's active CDP target.
    Select,
    /// Open a new background tab in the already-open browser window.
    New,
    /// Close a tab owned by this MCP session.
    Close,
}

/// Parameters for `browser_tabs` (#1298/#1188): enumerate and manage tabs in an
/// already-open Chromium browser window through the normal Chrome bridge.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserTabsParams {
    /// Operation to perform. Defaults to `list`, preserving the original
    /// read-only tab enumeration behavior.
    #[serde(default)]
    pub operation: BrowserTabsOperation,
    /// Browser HWND whose tabs should be listed. If omitted, the active session
    /// target's window is used; if the session has no target, this explicit
    /// discovery behavior is available only for `list` and `select`.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Target id for `select` or `close`.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// URL for `new`. Empty string opens about:blank.
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserTabEntry {
    /// Ready to pass to `set_target` to bind this tab.
    pub target: TargetWire,
    pub window_hwnd: i64,
    pub cdp_target_id: String,
    pub tab_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    pub index: i32,
    pub target_type: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub active: bool,
    pub highlighted: bool,
    pub pinned: bool,
    pub target_attached: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserTabsMutation {
    pub operation: BrowserTabsOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_cdp_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_tab: Option<BrowserTabEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_cdp_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_cdp_target_id: Option<String>,
    pub closed: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserTabsResponse {
    pub session_id: String,
    pub operation: BrowserTabsOperation,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_focused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_state: Option<String>,
    pub chrome_window_selection_reason: String,
    pub chrome_window_candidate_count: u32,
    pub chrome_window_non_focused_count: u32,
    pub target_count: u32,
    pub active_tab_count: u32,
    pub used_human_os_foreground_window: bool,
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation: Option<BrowserTabsMutation>,
    pub tabs: Vec<BrowserTabEntry>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNavOperation {
    /// Navigate the owned tab to `url`.
    #[default]
    Navigate,
    /// Reload the owned tab.
    Reload,
    /// Move the owned tab back in its navigation history.
    Back,
    /// Move the owned tab forward in its navigation history.
    Forward,
}

impl BrowserNavOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Reload => "reload",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNavParams {
    /// Navigation operation. Defaults to `navigate`, which requires `url`.
    #[serde(default)]
    pub operation: BrowserNavOperation,
    /// Browser HWND whose target table contains the tab. If omitted, the active
    /// session CDP target or owned `cdp_target_id` is used.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Chrome bridge/CDP target id. If omitted, the active session CDP target is used.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Destination URL for `operation=navigate`.
    #[serde(default)]
    pub url: Option<String>,
    /// Optional caller load/readback budget. Defaults to 10000 ms and is capped at 30000 ms.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// Reload cache policy. Valid only with `operation=reload`.
    #[serde(default)]
    pub ignore_cache: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNavResponse {
    pub operation: BrowserNavOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    pub navigation: CdpNavigateTabResponse,
}

/// Parameters for `browser_adopt_active_tab` (#1298): explicitly bind the
/// active tab from an already-open Chromium window as this MCP session's CDP
/// target without creating, navigating, activating, or closing any tab.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAdoptActiveTabParams {
    /// Browser HWND whose active tab should be adopted. If omitted, the active
    /// session target's window is used; if the session has no target, this
    /// explicit adoption tool passively uses the current human OS foreground
    /// Chromium window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAdoptActiveTabResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub tab_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_window_id: Option<i64>,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub target_count: u32,
    pub active_tab_count: u32,
    pub chrome_window_selection_reason: String,
    pub used_human_os_foreground_window: bool,
    pub source_of_truth: String,
    /// False: adopted user tabs are bindable/drivable but not owned for
    /// `cdp_close_tab`; closing remains limited to tabs Synapse created.
    pub close_authority: bool,
    pub previous: Option<TargetWire>,
    pub current: TargetWire,
    pub tab: BrowserTabEntry,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpLargestContentfulPaintInfo {
    pub name: String,
    pub entry_type: String,
    pub start_time: f64,
    pub render_time: f64,
    pub load_time: f64,
    pub size: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_tag_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_class_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_text_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_text_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_current_src_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_url_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpPageVitalsInfo {
    pub available: bool,
    pub readback_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lcp_supported: Option<bool>,
    pub lcp_entry_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lcp: Option<CdpLargestContentfulPaintInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail_sha256: Option<String>,
}

/// Parameters for `window_list` (#1021). All fields optional; an empty object
/// returns every visible top-level window.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowListParams {
    /// Case-insensitive substring filter on the window title. None = no filter.
    #[serde(default)]
    pub title_contains: Option<String>,
    /// Case-insensitive substring filter on the process name (e.g. "chrome").
    /// None = no filter.
    #[serde(default)]
    pub process_name_contains: Option<String>,
    /// When true, omit minimized windows. They are still valid *background*
    /// targets, so the default (false) includes them.
    #[serde(default)]
    pub exclude_minimized: bool,
}

/// One enumerated top-level window. `target` round-trips directly into
/// `set_target { target }`. Snapshot-only: producing this row never activates,
/// foregrounds, or attaches a debugger to the window.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowListEntry {
    pub hwnd: i64,
    pub pid: u32,
    pub process_name: String,
    pub process_path: String,
    pub window_title: String,
    pub window_bounds: synapse_core::Rect,
    pub monitor_index: u32,
    pub dpi_scale: f32,
    pub is_minimized: bool,
    /// True only for the single window that is the live human OS foreground at
    /// snapshot time. This is `human_os_foreground`, NOT any agent target (#994).
    pub is_foreground: bool,
    pub is_fullscreen: bool,
    pub is_dwm_composed: bool,
    /// Heuristic (process-name family match): this window belongs to a Chromium
    /// browser. Per-tab CDP/bridge targetIds are not enumerated here — bind the
    /// window then use the Chrome bridge / `cdp_target_info` to read tabs.
    pub is_chromium: bool,
    /// Session id that currently holds a target claim on this window, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by_session_id: Option<String>,
    /// Full target-claim row (ttl/expiry/generation) when claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_claim: Option<crate::server::target_claims::TargetClaimRead>,
    /// Ready to paste into `set_target { target: <this> }`.
    pub target: TargetWire,
}

/// Response for `window_list`. `human_os_foreground_hwnd` is reported separately
/// from the per-entry rows so callers can explicitly avoid the human's window
/// (the core #994 invariant: agent_active_target is distinct from
/// human_os_foreground).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowListResponse {
    pub session_id: String,
    pub now_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_os_foreground_hwnd: Option<i64>,
    pub window_count: usize,
    pub windows: Vec<WindowListEntry>,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpBridgeReloadParams {
    /// Optional reconnect wait budget. Defaults to 10000 ms and is capped at
    /// 30000 ms. The tool returns only after a separate bridge host readback
    /// observes a new extension registration.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpBridgeHostReadback {
    pub host_id: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_protocol_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_build_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_build_sha256: Option<String>,
    pub extension_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_debugger_api_available: Option<bool>,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    pub registered_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_disconnect_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_detach_reason: Option<String>,
    pub extension_stale: bool,
    pub extension_stale_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpBridgeReloadAckReadback {
    pub ok: bool,
    pub extension_id: String,
    pub version: String,
    pub protocol_version: u32,
    pub build_id: String,
    pub build_sha256: String,
    pub debugger_api_available: bool,
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub reload_requested_at_unix_ms: u64,
    pub reload_delay_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpBridgeReloadResponse {
    pub session_id: String,
    pub required_foreground: bool,
    pub wait_timeout_ms: u64,
    pub before: CdpBridgeHostReadback,
    pub command_ack: CdpBridgeReloadAckReadback,
    pub after: CdpBridgeHostReadback,
    pub reconnected: bool,
    pub waited_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpPageTextInfo {
    pub available: bool,
    pub readback_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub text_len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_sha256: Option<String>,
    pub text_truncated: bool,
    pub max_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perceived_text_notice: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspected_injection: Vec<synapse_core::SuspectedInjectionAnnotation>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpActiveElementInfo {
    pub available: bool,
    pub readback_source: String,
    pub has_active_element: Option<bool>,
    pub is_editable: Option<bool>,
    pub tag_name: Option<String>,
    pub id_sha256: Option<String>,
    pub name_sha256: Option<String>,
    pub value_len: Option<usize>,
    pub value_sha256: Option<String>,
    pub selected_text_sha256: Option<String>,
    pub error_code: Option<String>,
    pub error_detail_sha256: Option<String>,
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CdpNavigateAction {
    Navigate,
    Reload,
    Back,
    Forward,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpNavigateTabParams {
    /// Browser HWND whose target table contains the CDP target. If omitted, the
    /// caller must already have an active CDP session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// CDP TargetID to navigate. If omitted, the active session CDP target is used.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Navigation operation. `navigate` requires `url`; raw CDP uses
    /// Page.getNavigationHistory for history actions, while the normal Chrome
    /// extension bridge uses chrome.tabs history methods without debugger attach.
    pub action: CdpNavigateAction,
    /// Destination URL for `action=navigate`.
    #[serde(default)]
    pub url: Option<String>,
    /// Optional caller load/readback budget. Defaults to the bridge/CDP command
    /// budget and is capped by the daemon command timeout.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// `Page.reload(ignoreCache=true)` for `action=reload`.
    #[serde(default)]
    pub ignore_cache: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpNavigateTabResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub action: CdpNavigateAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_url: Option<String>,
    pub before_url: String,
    pub before_title: String,
    pub after_url: String,
    pub after_title: String,
    pub ready_state: String,
    pub history_current_index: i64,
    pub history_entry_count: u32,
    pub history_readback_source: String,
    pub readback_backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation_error_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_download: Option<bool>,
    /// #1344: when the navigate started a Chrome download instead of changing the
    /// tab URL, the structured outcome — `download_started` or `download_completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_final_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_state: Option<String>,
    /// Why the navigate-triggered download was matched: `url_match` or
    /// `download_created_during_navigate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_match_reason: Option<String>,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpActivateTabParams {
    /// Browser HWND whose target table contains the CDP target. If omitted, the
    /// caller must already have an active CDP session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// CDP TargetID to activate. If omitted, the active session CDP target is used.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Optional readback budget for confirming the tab became active. Defaults to
    /// the bridge command budget and is capped by the daemon command timeout.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CdpActivateTabResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    /// Whether the tab was the active tab in its window before activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_active: Option<bool>,
    /// Tab is now the active tab in its window (read back after activation).
    pub active: bool,
    pub url: String,
    pub title: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    /// Always false: activation selects the tab within its window and never
    /// seizes the human OS foreground.
    pub required_foreground: bool,
    pub target_candidate_count: u32,
    pub target_selection_reason: String,
}

/// Parameters for `browser_evaluate` (#1065/#1067): evaluate a JavaScript
/// expression in the calling session's owned CDP page target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEvaluateParams {
    /// JavaScript expression to evaluate in the page's main world. To return an
    /// object literal, parenthesize it (e.g. `({a: 1})`). Use an async IIFE plus
    /// `await_promise` for asynchronous work (e.g. `(async () => await
    /// fetch('/x').then(r => r.status))()`).
    pub expression: String,
    /// CDP TargetID to evaluate in. If omitted, the active session CDP target is
    /// used. The target must be owned by this session; the human foreground tab
    /// is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only when passing an explicit
    /// `cdp_target_id` without an active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Optional element id (from `find`/`observe`) to scope evaluation to a DOM
    /// element. When set, `expression` MUST be a function and is called
    /// Playwright-style as `fn(element, ...args)` — the element is the FIRST
    /// argument (e.g. `el => el.value` or
    /// `(el, suffix) => el.id + suffix`), followed by any `args`. The element's
    /// CDP target must be owned by this session.
    #[serde(default)]
    pub element_id: Option<String>,
    /// Optional JSON arguments. When provided (or with `element_id`), `expression`
    /// is treated as a function declaration invoked with these args (Playwright
    /// `evaluate(fn, ...args)` semantics). Page-scope args are passed by
    /// JSON-injection into a `Runtime.evaluate` call; element-scope args are
    /// passed as `Runtime.callFunctionOn` arguments.
    #[serde(default)]
    pub args: Option<Vec<serde_json::Value>>,
    /// Await a returned promise/thenable before resolving. Defaults to true.
    #[serde(default)]
    pub await_promise: Option<bool>,
    /// Serialize the result by value as JSON. Defaults to true. Set false to
    /// receive only the type/description handle for non-serializable values
    /// (DOM nodes, functions).
    #[serde(default)]
    pub return_by_value: Option<bool>,
}

/// Response for `browser_evaluate`. The evaluated value plus the page context it
/// was read against and the `Runtime.RemoteObject` type metadata.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserEvaluateResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    /// Evaluation scope: "page" (Runtime.evaluate) or "element"
    /// (Runtime.callFunctionOn on a resolved DOM node).
    pub scope: String,
    /// Echo of the element id when scope is "element".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    /// `Runtime.RemoteObject.type` of the result.
    pub result_type: String,
    /// `Runtime.RemoteObject.subtype` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_subtype: Option<String>,
    pub returned_by_value: bool,
    /// Serialized JSON value when `returned_by_value`; JSON `null` otherwise.
    pub value: serde_json::Value,
    /// Engine string rendering for non-by-value handles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Present for values that cannot be JSON-represented ("Infinity", "NaN", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unserializable_value: Option<String>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    /// Always false: evaluation attaches to the owned background target only.
    pub required_foreground: bool,
}

/// Operation for `browser_expose_binding`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserExposeBindingOperation {
    /// Install Runtime.addBinding and arm the per-target binding event buffer.
    #[default]
    Add,
    /// Read the existing buffer without adding or removing a binding.
    Read,
    /// Stop receiving Runtime.bindingCalled notifications for this binding.
    Remove,
}

/// Parameters for `browser_expose_binding` (#1069): expose/read/remove a
/// page-callable Runtime binding on the calling session's owned raw CDP page
/// target or normal Chrome bridge `chrome-tab:*` target.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserExposeBindingParams {
    /// Add, read, or remove the binding. Defaults to add.
    #[serde(default)]
    pub operation: BrowserExposeBindingOperation,
    /// Binding function name exposed on `window`. Synapse accepts JavaScript
    /// identifier names so page code can call `window.name("payload")`.
    pub name: String,
    /// CDP TargetID to mutate/read. Defaults to the active session CDP target.
    /// Must be owned by this session; the human foreground tab is never an
    /// implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Optional execution context name for Runtime.addBinding, matching CDP
    /// `executionContextName` / init-script `worldName`.
    #[serde(default)]
    pub execution_context_name: Option<String>,
    /// Return only entries with `seq >= since_seq` (delta semantics). Pass the
    /// prior response's `next_cursor` to receive only newer entries.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum calls to return (default 200), oldest-first after the cursor.
    #[serde(default)]
    pub max_calls: Option<usize>,
}

/// One captured Runtime.bindingCalled payload.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserBindingCall {
    /// Monotonic per-target sequence number; the cursor for delta reads.
    pub seq: u64,
    pub name: String,
    /// String payload passed by page code to the binding function.
    pub payload: String,
    /// Full payload length in Unicode scalar values before truncation.
    pub payload_len: usize,
    pub payload_truncated: bool,
    /// JSON-decoded payload when the string is valid JSON and not truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<serde_json::Value>,
    pub execution_context_id: i64,
    pub timestamp_ms: f64,
}

/// Response for `browser_expose_binding`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserExposeBindingResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserExposeBindingOperation,
    pub name: String,
    /// `true` if this call established the long-lived target listener.
    pub newly_armed: bool,
    /// `true` when Runtime.addBinding was sent on this call.
    pub binding_newly_added: bool,
    /// `true` when Runtime.removeBinding was sent on this call.
    pub binding_removed: bool,
    /// When the persistent target listener was armed (Unix ms), or 0 when no
    /// listener exists for an idempotent remove on an unarmed target.
    pub armed_at_unix_ms: f64,
    /// Whether this binding remains active for future Runtime.bindingCalled
    /// delivery after this operation.
    pub binding_active: bool,
    pub active_binding_count: usize,
    pub active_binding_names: Vec<String>,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    /// Filtered, cursor-delimited binding calls.
    pub calls: Vec<BrowserBindingCall>,
    /// Highest assigned target buffer seq. Pass back as `since_seq` next call.
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Operation for `browser_add_init_script`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserInitScriptOperation {
    /// Add a script via Page.addScriptToEvaluateOnNewDocument.
    #[default]
    Add,
    /// Remove a previously added script by CDP script identifier.
    Remove,
}

/// Parameters for `browser_add_init_script` (#1068): install or remove a
/// Playwright-style init script on the calling session's owned CDP page target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAddInitScriptParams {
    /// Add or remove an init script. Defaults to `add`.
    #[serde(default)]
    pub operation: BrowserInitScriptOperation,
    /// CDP TargetID to mutate. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// JavaScript source for `operation=add`. It runs before page scripts on
    /// subsequent new documents/navigations for this page target.
    #[serde(default)]
    pub source: Option<String>,
    /// Script identifier returned by `operation=add`; required for
    /// `operation=remove`.
    #[serde(default)]
    pub identifier: Option<String>,
    /// Optional isolated-world name (`worldName`) for the init script.
    #[serde(default)]
    pub world_name: Option<String>,
    /// Expose command-line API helpers to the script. Defaults to CDP false.
    #[serde(default)]
    pub include_command_line_api: Option<bool>,
    /// Run immediately in existing execution contexts as well as future
    /// documents. Defaults to CDP false.
    #[serde(default)]
    pub run_immediately: Option<bool>,
}

/// Response for `browser_add_init_script`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAddInitScriptResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserInitScriptOperation,
    pub identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_command_line_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_immediately: Option<bool>,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_add_script_tag` (#1070): inject a `<script>` into the
/// current document from exactly one of `url`, `content`, or `path`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAddScriptTagParams {
    /// CDP TargetID to mutate. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Remote script URL to load into a `<script src=...>`.
    #[serde(default)]
    pub url: Option<String>,
    /// Inline script source.
    #[serde(default)]
    pub content: Option<String>,
    /// Local UTF-8 file path to read and inject as inline script source.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional script `type` attribute, e.g. `module`.
    #[serde(default)]
    pub script_type: Option<String>,
}

/// Parameters for `browser_add_style_tag` (#1070): inject a stylesheet into the
/// current document from exactly one of `url`, `content`, or `path`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAddStyleTagParams {
    /// CDP TargetID to mutate. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Remote stylesheet URL to load into a `<link rel=stylesheet href=...>`.
    #[serde(default)]
    pub url: Option<String>,
    /// Inline CSS source.
    #[serde(default)]
    pub content: Option<String>,
    /// Local UTF-8 file path to read and inject as inline CSS source.
    #[serde(default)]
    pub path: Option<String>,
}

/// Response for `browser_add_script_tag` / `browser_add_style_tag`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAddTagResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub tag_name: String,
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_type: Option<String>,
    pub content_len: usize,
    pub element_marker: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Desired condition for `browser_wait_for` (#1127).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWaitForState {
    /// Resolve when the target text appears in the page text.
    TextAppears,
    /// Resolve when the target text is absent from the page text.
    TextGone,
    /// Resolve after the timeout budget elapses without checking text.
    Timeout,
}

/// Parameters for `browser_wait_for` (#1127): wait for text to appear, text to
/// disappear, or for a plain timeout in the calling session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForParams {
    /// CDP TargetID to wait in. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Text to wait for. If supplied without `state`, the tool waits for it to
    /// appear. Omit text for a plain timeout wait.
    #[serde(default)]
    pub text: Option<String>,
    /// Wait condition. Defaults to `text_appears` when `text` is supplied and to
    /// `timeout` when `text` is omitted.
    #[serde(default)]
    pub state: Option<BrowserWaitForState>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds for text waits. Defaults to 100 ms.
    #[serde(default)]
    pub polling_interval_ms: Option<u64>,
}

/// Response for `browser_wait_for`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub state: BrowserWaitForState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub polling_interval_ms: u64,
    pub poll_count: u64,
    pub observed_text_len: usize,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Desired page lifecycle state for `browser_wait_for_load_state` (#1130).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserWaitForLoadStateState {
    /// DOMContentLoaded has fired, or `document.readyState` is interactive or complete.
    DomContentLoaded,
    /// The page load event has fired, or `document.readyState` is complete.
    #[default]
    Load,
    /// Load has completed and the target has no in-flight network requests for
    /// at least 500 ms.
    NetworkIdle,
}

/// Parameters for `browser_wait_for_load_state` (#1130): wait for a page
/// lifecycle state in the calling session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForLoadStateParams {
    /// Lifecycle state to wait for. Defaults to `load`.
    #[serde(default)]
    pub state: Option<BrowserWaitForLoadStateState>,
    /// CDP TargetID to wait in. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Response for `browser_wait_for_load_state`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForLoadStateResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub state: BrowserWaitForLoadStateState,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub event_count: u64,
    pub network_event_count: u64,
    pub max_in_flight_requests: usize,
    pub in_flight_requests: usize,
    pub network_idle_quiet_ms: u64,
    pub lifecycle_network_idle_seen: bool,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// URL matching mode for `browser_wait_for_url` (#1131).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserWaitForUrlMatchKind {
    /// Exact string match against the full current URL.
    #[default]
    Exact,
    /// Glob match where `*` matches any sequence and `?` matches one character.
    Glob,
    /// Rust regular expression match against the full current URL string.
    Regex,
}

/// Parameters for `browser_wait_for_url` (#1131): wait until the calling
/// session's owned CDP target URL matches a string, glob, or regex.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForUrlParams {
    /// URL pattern to wait for. Interpreted according to `match_kind`.
    pub url: String,
    /// Matching mode. Defaults to exact string matching.
    #[serde(default)]
    pub match_kind: Option<BrowserWaitForUrlMatchKind>,
    /// CDP TargetID to wait in. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds. Defaults to 100 ms.
    #[serde(default)]
    pub polling_interval_ms: Option<u64>,
}

/// Response for `browser_wait_for_url`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForUrlResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub url_pattern: String,
    pub match_kind: BrowserWaitForUrlMatchKind,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub polling_interval_ms: u64,
    pub poll_count: u64,
    pub navigation_event_count: u64,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Network request/response record returned by `browser_wait_for_request` and
/// `browser_wait_for_response` (#1132). Headers and timing stay as JSON because
/// CDP reports protocol-shaped maps whose keys vary by server.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkWaitEntry {
    pub seq: u64,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<Value>,
    pub response_received: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_timing: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_ip_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_data_length: Option<f64>,
    pub loading_finished: bool,
    pub loading_failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_error_text: Option<String>,
}

/// Parameters for `browser_wait_for_request` (#1132): wait for a captured
/// Network.requestWillBeSent entry matching optional URL/method/resource-type
/// predicates in the calling session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForRequestParams {
    /// Optional URL pattern to match. If omitted, the first request matching the
    /// remaining predicates is returned.
    #[serde(default)]
    pub url: Option<String>,
    /// Matching mode for `url`. Defaults to exact string matching when `url` is supplied.
    #[serde(default)]
    pub match_kind: Option<BrowserWaitForUrlMatchKind>,
    /// Optional HTTP method predicate, case-insensitive.
    #[serde(default)]
    pub method: Option<String>,
    /// Optional CDP Network resource type predicate, case-insensitive.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// CDP TargetID to wait in. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds. Defaults to 100 ms.
    #[serde(default)]
    pub polling_interval_ms: Option<u64>,
}

/// Response for `browser_wait_for_request`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForRequestResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_pattern: Option<String>,
    pub match_kind: BrowserWaitForUrlMatchKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub polling_interval_ms: u64,
    pub poll_count: u64,
    pub matched_entry: BrowserNetworkWaitEntry,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_wait_for_response` (#1132): wait for a captured
/// Network.responseReceived entry matching optional URL/method/status/resource
/// predicates in the calling session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForNetworkResponseParams {
    /// Optional URL pattern to match. If omitted, the first response matching
    /// the remaining predicates is returned.
    #[serde(default)]
    pub url: Option<String>,
    /// Matching mode for `url`. Defaults to exact string matching when `url` is supplied.
    #[serde(default)]
    pub match_kind: Option<BrowserWaitForUrlMatchKind>,
    /// Optional HTTP method predicate, case-insensitive.
    #[serde(default)]
    pub method: Option<String>,
    /// Optional HTTP status predicate.
    #[serde(default)]
    pub status: Option<i64>,
    /// Optional CDP Network resource type predicate, case-insensitive.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// CDP TargetID to wait in. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds. Defaults to 100 ms.
    #[serde(default)]
    pub polling_interval_ms: Option<u64>,
}

/// Response for `browser_wait_for_response`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForNetworkResponseResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_pattern: Option<String>,
    pub match_kind: BrowserWaitForUrlMatchKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub polling_interval_ms: u64,
    pub poll_count: u64,
    pub matched_entry: BrowserNetworkWaitEntry,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_wait_for_function` (#1129): poll a JavaScript
/// predicate/expression until it resolves truthy in the calling session's owned
/// CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForFunctionParams {
    /// JavaScript expression or function declaration. If it evaluates to a
    /// function, Synapse calls it as `fn(...args)` on every poll; otherwise the
    /// expression value itself is tested on every poll.
    pub expression: String,
    /// CDP TargetID to wait in. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Optional JSON arguments forwarded to function predicates.
    #[serde(default)]
    pub args: Option<Vec<serde_json::Value>>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds. Defaults to 100 ms.
    #[serde(default)]
    pub polling_interval_ms: Option<u64>,
}

/// Response for `browser_wait_for_function`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForFunctionResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub polling_interval_ms: u64,
    pub poll_count: u64,
    pub expression_len: usize,
    pub arg_count: usize,
    /// Final predicate value converted to a JSON-safe representation where
    /// possible.
    pub value: serde_json::Value,
    /// JavaScript `typeof` for the final predicate value.
    pub value_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unserializable_value: Option<String>,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Desired selector state for `browser_wait_for_selector` (#1128).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserWaitForSelectorState {
    /// Element exists in the DOM, regardless of rendered visibility.
    Attached,
    /// Element is attached and rendered with a non-empty visible box.
    #[default]
    Visible,
    /// Element is absent, or attached but not visibly rendered.
    Hidden,
    /// Element is absent from the DOM.
    Detached,
}

/// Parameters for `browser_wait_for_selector` (#1128): poll a Playwright-style
/// selector until it reaches attached / visible / hidden / detached state in
/// the calling session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForSelectorParams {
    /// The primary query, with the same semantics as `browser_locate`.
    pub query: String,
    /// Which selector engine interprets `query` (default `css`).
    #[serde(default)]
    pub engine: BrowserLocateEngine,
    /// Desired selector state. Defaults to `visible`.
    #[serde(default)]
    pub state: Option<BrowserWaitForSelectorState>,
    /// Exact match for text/attribute engines.
    #[serde(default)]
    pub exact: Option<bool>,
    /// Interpret `query` as a JS regular expression body.
    #[serde(default)]
    pub regex: Option<bool>,
    /// `role` only: accessible-name filter.
    #[serde(default)]
    pub name: Option<String>,
    /// `role` only: exact accessible-name match.
    #[serde(default)]
    pub name_exact: Option<bool>,
    /// `role` only: interpret `name` as a regular expression.
    #[serde(default)]
    pub name_regex: Option<bool>,
    /// `testid` only: attribute name to read (default `data-testid`).
    #[serde(default)]
    pub testid_attribute: Option<String>,
    /// `role` only: ARIA state filters.
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub pressed: Option<bool>,
    #[serde(default)]
    pub expanded: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub level: Option<i64>,
    #[serde(default)]
    pub include_hidden: Option<bool>,
    /// `layout` only: relation to the anchor.
    #[serde(default)]
    pub relation: Option<BrowserLayoutRelation>,
    /// `layout` only: the anchor CSS selector.
    #[serde(default)]
    pub anchor: Option<String>,
    /// `layout` only: maximum CSS-pixel distance.
    #[serde(default)]
    pub max_distance: Option<f64>,
    /// `.filter({ hasText })` equivalent for JS-resolved engines.
    #[serde(default)]
    pub has_text: Option<String>,
    /// Positional pick (`.nth`/`.first`/`.last`): 0-based; negative from end.
    #[serde(default)]
    pub nth: Option<i64>,
    /// Strict mode: error when more than one element matches (unless `nth` set).
    #[serde(default)]
    pub strict: Option<bool>,
    /// Resolve only within this element id (chaining / scoping).
    #[serde(default)]
    pub root_element_id: Option<String>,
    /// Resolve inside a specific frame selected by exactly one of frame_id,
    /// frame_element_id, name, url, or index.
    #[serde(default)]
    pub frame: Option<BrowserFrameLocator>,
    /// CDP TargetID to query. Defaults to the active session CDP target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum matches to inspect per poll (default 50, capped at 500).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Maximum wait budget in milliseconds. Defaults to 30 seconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds. Defaults to 100 ms.
    #[serde(default)]
    pub polling_interval_ms: Option<u64>,
}

/// Response for `browser_wait_for_selector`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitForSelectorResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub engine: String,
    pub query: String,
    pub state: BrowserWaitForSelectorState,
    pub condition_met: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub polling_interval_ms: u64,
    pub poll_count: u64,
    pub match_count: usize,
    pub returned_count: usize,
    pub visible_count: usize,
    pub truncated: bool,
    /// Present when the satisfied state has a concrete matched element. Hidden
    /// and detached can resolve with no element when the selector is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<BrowserLocatedFrame>,
    pub url: String,
    pub title: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Which wait predicate the unified `browser_wait_for` tool evaluates (#1348).
/// The matching nested spec under [`BrowserWaitParams`] must be supplied.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWaitConditionKind {
    /// Wait for page text to appear/disappear, or a plain timeout.
    #[default]
    Text,
    /// Wait for a page lifecycle state (domcontentloaded/load/networkidle).
    LoadState,
    /// Wait until the tab URL matches an exact string, glob, or regex.
    Url,
    /// Wait for a Playwright-style selector to reach attached/visible/hidden/detached.
    Selector,
    /// Poll a JavaScript predicate until it resolves truthy.
    Function,
    /// Wait for a captured network request matching predicates.
    Request,
    /// Wait for a captured network response matching predicates.
    Response,
}

/// Parameters for the unified `browser_wait_for` tool (#1348). One tool, one
/// `condition` discriminator, and exactly one matching nested spec carrying that
/// predicate's fields. Each spec is the former standalone `browser_wait_for_*`
/// tool's parameter object, reused verbatim, so behaviour is identical to the
/// seven tools this folds together — only the surface shrinks.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitParams {
    /// Which wait predicate to evaluate. Supply the matching nested spec.
    pub condition: BrowserWaitConditionKind,
    /// `condition=text`: page-text appear/disappear or plain timeout.
    #[serde(default)]
    pub text: Option<BrowserWaitForParams>,
    /// `condition=load_state`: page lifecycle state.
    #[serde(default)]
    pub load_state: Option<BrowserWaitForLoadStateParams>,
    /// `condition=url`: tab URL match.
    #[serde(default)]
    pub url: Option<BrowserWaitForUrlParams>,
    /// `condition=selector`: Playwright-style selector state.
    #[serde(default)]
    pub selector: Option<BrowserWaitForSelectorParams>,
    /// `condition=function`: JavaScript predicate poll.
    #[serde(default)]
    pub function: Option<BrowserWaitForFunctionParams>,
    /// `condition=request`: captured network request match.
    #[serde(default)]
    pub request: Option<BrowserWaitForRequestParams>,
    /// `condition=response`: captured network response match.
    #[serde(default)]
    pub response: Option<BrowserWaitForNetworkResponseParams>,
}

/// Response for the unified `browser_wait_for` tool (#1348): the populated field
/// matches `condition` and carries the former standalone tool's full response.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWaitResponse {
    /// Which wait predicate was evaluated.
    pub condition: BrowserWaitConditionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<BrowserWaitForResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_state: Option<BrowserWaitForLoadStateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<BrowserWaitForUrlResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<BrowserWaitForSelectorResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<BrowserWaitForFunctionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<BrowserWaitForRequestResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<BrowserWaitForNetworkResponseResponse>,
}

/// Parameters for `browser_content` (#1158): return the full serialized HTML of
/// the calling session's owned browser page target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserContentParams {
    /// CDP/Chrome bridge target id to read. Defaults to the active session
    /// target. Must be owned by this session; the human foreground tab is never
    /// an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum HTML bytes to return (UTF-16 length cap, default 2 MiB). The HTML
    /// is truncated in-page; `truncated`/`html_len` report the original size.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Response for `browser_content`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserContentResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    /// Serialized `document.documentElement.outerHTML` (possibly truncated).
    pub html: String,
    /// Full original length of the serialized HTML (before truncation).
    pub html_len: usize,
    pub truncated: bool,
    pub max_bytes: usize,
    pub readback_backend: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_set_content` (#1159): replace the main-frame HTML of
/// the calling session's owned browser page target. The normal Chrome bridge
/// may move an inaccessible blank/internal page to a daemon-local seed URL
/// before replacing the document.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserSetContentParams {
    /// CDP/Chrome bridge target id to mutate. Defaults to the active session
    /// target. Must be owned by this session; the human foreground tab is never
    /// an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Full replacement HTML for the target's main frame.
    pub html: String,
    /// Optional caller load/readback budget. Defaults to the bridge/CDP command
    /// budget and is capped by the daemon command timeout.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

/// Response for `browser_set_content`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserSetContentResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub frame_id: String,
    pub html_len: usize,
    pub before_url: String,
    pub before_title: String,
    pub after_url: String,
    pub after_title: String,
    pub ready_state: String,
    pub history_current_index: i64,
    pub history_entry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeded_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeded_from_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeded_reason: Option<String>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_console_messages` (#1091/#1092/#1093/#1094): list the
/// console output + page errors captured from the calling session's owned CDP
/// page target. The first call arms a persistent per-target capture; subsequent
/// calls read the bounded buffer with optional filters and delta cursoring.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserConsoleMessagesParams {
    /// CDP TargetID to read. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit
    /// fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Return only entries with `seq >= since_seq` (delta semantics). Pass the
    /// prior response's `next_cursor` to receive only entries added since.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Exact level filter (case-insensitive): `log`, `info`, `warning`, `error`,
    /// `debug`, `trace`, `verbose`, …
    #[serde(default)]
    pub level: Option<String>,
    /// Exact source-class filter: `console-api`, `page-error`,
    /// `unhandled-rejection`, or `browser-log`.
    #[serde(default)]
    pub source: Option<String>,
    /// Case-insensitive substring filter on the entry display text.
    #[serde(default)]
    pub text_contains: Option<String>,
    /// Maximum entries to return (default 200). Entries are returned oldest-first
    /// after the cursor.
    #[serde(default)]
    pub max_messages: Option<usize>,
}

/// One captured console / page-error / browser-log record.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsoleMessage {
    /// Monotonic per-target sequence number; the cursor for delta reads.
    pub seq: u64,
    /// Origin class: `console-api`, `page-error`, `unhandled-rejection`, or
    /// `browser-log`.
    pub source: String,
    /// Severity / call type string (`log`, `warning`, `error`, `verbose`, …).
    pub level: String,
    /// Rendered, space-joined display text (the args, or the exception message).
    pub text: String,
    /// Structured rendered arguments (primitives as JSON; objects/arrays from
    /// their CDP preview — never `[object Object]`). Empty for page errors.
    pub args: Vec<serde_json::Value>,
    /// Source URL of the call / error, when CDP reports a location.
    pub url: Option<String>,
    /// 1-based source line, when known.
    pub line: Option<u32>,
    /// 1-based source column, when known.
    pub column: Option<u32>,
    /// Formatted stack trace, when known.
    pub stack: Option<String>,
    /// `Log.entryAdded` sub-source (`network`, `security`, …); null otherwise.
    pub category: Option<String>,
    /// Event timestamp, milliseconds since the Unix epoch.
    pub timestamp_ms: f64,
}

/// Response for `browser_console_messages`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserConsoleMessagesResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    /// `true` if this call established the capture; `false` if it reused a live
    /// one. A freshly-armed target only captures messages emitted after arming.
    pub newly_armed: bool,
    /// When capture for this target was armed (Unix ms).
    pub armed_at_unix_ms: f64,
    /// Filtered, cursor-delimited entries.
    pub messages: Vec<ConsoleMessage>,
    /// Highest `seq` currently buffered — pass back as `since_seq` next call.
    pub next_cursor: u64,
    /// Entries returned after filtering.
    pub returned: usize,
    /// Entries currently held in the ring buffer (pre-filter).
    pub total_buffered: usize,
    /// Entries evicted over the target's lifetime because the buffer was full.
    pub dropped: u64,
    pub readback_backend: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_inspect` (#1160/#1161/#1162/#1163): typed
/// introspection of a single DOM element.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserInspectParams {
    /// Element id (from `find`/`observe`) of a CDP web element to introspect.
    pub element_id: String,
    /// CDP TargetID; defaults to the element's embedded target. When supplied it
    /// must match the element's target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only when the active session
    /// target is absent.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum bytes per HTML/text field (UTF-16 length cap, default 256 KiB).
    #[serde(default)]
    pub max_html_bytes: Option<usize>,
}

/// An element's page-relative bounding rectangle (#1163). `x`/`y` include scroll
/// offset (page coordinates, the Playwright `boundingBox` semantics);
/// `viewport_x`/`viewport_y` are the unscrolled viewport coordinates. All values
/// are CSS pixels; multiply by `device_pixel_ratio` for device pixels.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserBoundingBox {
    pub x: f64,
    pub y: f64,
    pub viewport_x: f64,
    pub viewport_y: f64,
    pub width: f64,
    pub height: f64,
}

/// The in-page introspection payload for one element (deserialized from the
/// element-scoped evaluation, re-serialized to the caller).
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementInspection {
    pub tag_name: String,
    pub outer_html: String,
    pub inner_html: String,
    pub inner_text: String,
    pub text_content: String,
    /// True when any of the html/text fields exceeded `max_html_bytes`.
    pub html_truncated: bool,
    pub max_html_bytes: usize,
    /// Live attribute name → value map (string values).
    pub attributes: serde_json::Map<String, serde_json::Value>,
    /// The element's `value` property when it has one (inputs/textarea/select);
    /// `null` otherwise.
    pub input_value: Option<String>,
    pub is_visible: bool,
    pub is_enabled: bool,
    pub is_checked: bool,
    pub is_editable: bool,
    pub bounding_box: BrowserBoundingBox,
    pub device_pixel_ratio: f64,
    /// Protocol-backed actionability predicates (#1122): attached, visible,
    /// stable, enabled, editable, and receives-events, with structured failure
    /// reasons. Omitted only by older/non-Windows implementations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actionability: Option<serde_json::Value>,
}

/// Response for `browser_inspect`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserInspectResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub element_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub element: ElementInspection,
    pub readback_backend: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_scroll_into_view` (#1123): scroll a resolved DOM
/// element into the viewport using raw CDP.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserScrollIntoViewParams {
    /// Element id (from `find`/`observe`/`browser_locate`) of a CDP web element.
    pub element_id: String,
    /// CDP TargetID; defaults to the element's embedded target. When supplied it
    /// must match the element's target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only when the active session
    /// target is absent.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

/// Response for `browser_scroll_into_view`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserScrollIntoViewResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub element_id: String,
    /// Structured `synapse_a11y::CdpScrollIntoViewResult` readback.
    pub scroll: serde_json::Value,
    pub readback_backend: String,
    pub required_foreground: bool,
}

/// Selector engine for `browser_locate` (#1110) - the full Playwright locator
/// surface. Each engine resolves to Synapse element ids consumable by every
/// action tool.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserLocateEngine {
    /// CSS selector (`DOM.querySelectorAll` semantics), shadow-piercing.
    #[default]
    Css,
    /// XPath query (`document.evaluate`).
    Xpath,
    /// Visible text (`getByText`): normalized whitespace; substring/exact/regex.
    Text,
    /// ARIA role + accessible name + state (`getByRole`), via the live AX tree.
    Role,
    /// `getByLabel`: aria-labelledby / aria-label / wrapping or `for=` `<label>`.
    Label,
    /// `getByPlaceholder`: the `placeholder` attribute.
    Placeholder,
    /// `getByAltText`: the `alt` attribute.
    #[serde(rename = "alttext")]
    AltText,
    /// `getByTitle`: the `title` attribute.
    Title,
    /// `getByTestId`: a configurable attribute (default `data-testid`).
    #[serde(rename = "testid")]
    TestId,
    /// Layout / relational (`:near`/`:right-of`/...), ranked by box geometry.
    Layout,
}

/// Direction for the `layout` engine (Playwright proximity pseudo-classes).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserLayoutRelation {
    /// Within `max_distance` (default 50 CSS px) in any direction.
    #[default]
    Near,
    /// To the right of the anchor.
    RightOf,
    /// To the left of the anchor.
    LeftOf,
    /// Above the anchor.
    Above,
    /// Below the anchor.
    Below,
}

/// Parameters for `browser_locate` (#1110-#1119): resolve any Playwright-style
/// selector to element ids in the calling session's owned CDP page target.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocateParams {
    /// The primary query: CSS/XPath text, visible text, ARIA role token, label /
    /// placeholder / alt / title text, test-id value, or (for `layout`) the base
    /// CSS selector whose matches are ranked against `anchor`.
    pub query: String,
    /// Which selector engine interprets `query` (default `css`).
    #[serde(default)]
    pub engine: BrowserLocateEngine,
    /// Exact match (whitespace-normalized) instead of the default
    /// case-insensitive substring, for text/label/placeholder/alttext/title.
    /// (`testid` defaults to exact; set `exact:false` for substring.)
    #[serde(default)]
    pub exact: Option<bool>,
    /// Interpret `query` as a JS regular expression body.
    #[serde(default)]
    pub regex: Option<bool>,
    /// `role` only: accessible-name filter (the role token stays in `query`).
    #[serde(default)]
    pub name: Option<String>,
    /// `role` only: exact accessible-name match.
    #[serde(default)]
    pub name_exact: Option<bool>,
    /// `role` only: interpret `name` as a regular expression.
    #[serde(default)]
    pub name_regex: Option<bool>,
    /// `testid` only: attribute name to read (default `data-testid`).
    #[serde(default)]
    pub testid_attribute: Option<String>,
    /// `role` only: require `aria-checked` to equal this.
    #[serde(default)]
    pub checked: Option<bool>,
    /// `role` only: require `aria-pressed` to equal this.
    #[serde(default)]
    pub pressed: Option<bool>,
    /// `role` only: require `aria-expanded` to equal this.
    #[serde(default)]
    pub expanded: Option<bool>,
    /// `role` only: require `aria-selected` to equal this.
    #[serde(default)]
    pub selected: Option<bool>,
    /// `role` only: require the disabled state to equal this.
    #[serde(default)]
    pub disabled: Option<bool>,
    /// `role` only: require `aria-level` (e.g. heading level) to equal this.
    #[serde(default)]
    pub level: Option<i64>,
    /// `role` only: include nodes ignored for accessibility (`includeHidden`).
    #[serde(default)]
    pub include_hidden: Option<bool>,
    /// `layout` only: relation to the anchor (required for `layout`).
    #[serde(default)]
    pub relation: Option<BrowserLayoutRelation>,
    /// `layout` only: the anchor CSS selector (required for `layout`).
    #[serde(default)]
    pub anchor: Option<String>,
    /// `layout` only: maximum CSS-pixel distance (default 50 for `near`).
    #[serde(default)]
    pub max_distance: Option<f64>,
    /// `.filter({ hasText })`: keep only matches whose normalized text contains
    /// this (case-insensitive). Applies to every engine except `role`.
    #[serde(default)]
    pub has_text: Option<String>,
    /// Positional pick (`.nth`/`.first`/`.last`): 0-based; negative counts from
    /// the end (-1 == last). Bypasses strict mode.
    #[serde(default)]
    pub nth: Option<i64>,
    /// Strict mode: error when more than one element matches (unless `nth` set).
    #[serde(default)]
    pub strict: Option<bool>,
    /// Resolve only within this element id (chaining / scoping). Its embedded CDP
    /// target must match the resolved target.
    #[serde(default)]
    pub root_element_id: Option<String>,
    /// Resolve inside a specific frame selected by exactly one of frame_id,
    /// frame_element_id, name, url, or index.
    #[serde(default)]
    pub frame: Option<BrowserFrameLocator>,
    /// CDP TargetID to query. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Maximum element ids to return (default 50, capped at 500). `match_count`
    /// always reports the full number of matches.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Frame selector for `browser_locate` / `browser_wait_for_selector`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrowserFrameLocator {
    /// Exact CDP Page.FrameId from `browser_frames`.
    #[serde(default)]
    pub frame_id: Option<String>,
    /// Owning iframe/frame element id from `browser_frames`.
    #[serde(default)]
    pub frame_element_id: Option<String>,
    /// Exact frame name.
    #[serde(default)]
    pub name: Option<String>,
    /// Exact frame URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Zero-based index into the `browser_frames` returned frame list.
    #[serde(default)]
    pub index: Option<usize>,
}

/// Readback for frame-scoped locator resolution.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocatedFrame {
    pub resolved: bool,
    pub matched_frame_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub is_out_of_process: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_element_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_element_cdp_target_id: Option<String>,
    pub frame_element_source: String,
}

/// Response for `browser_locate`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserLocateResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    /// The engine that resolved the query (echoed for audit).
    pub engine: String,
    /// The resolved query string (echoed for audit).
    pub query: String,
    /// Total matches before the `nth`/`limit` cap (Playwright `count()`).
    pub match_count: usize,
    /// Number of element ids returned (== `element_ids.len()`).
    pub returned_count: usize,
    /// True when `match_count` exceeded the returned cap (false when `nth` set).
    pub truncated: bool,
    /// Resolved element ids - feed directly into `browser_inspect`, `act_*`, etc.
    pub element_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<BrowserLocatedFrame>,
    pub url: String,
    pub title: String,
    pub readback_backend: String,
    pub required_foreground: bool,
}

pub fn empty_input_schema() -> Arc<JsonObject> {
    common::schema_for_type::<EmptyParams>()
}

pub fn set_target_input_schema() -> Arc<JsonObject> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["target"],
        "properties": {
            "target": {
                "description": "Accepted target union. Use {\"kind\":\"window\",\"window_hwnd\":<integer>} for a native HWND, or {\"kind\":\"cdp\",\"window_hwnd\":<integer>,\"cdp_target_id\":\"<target id>\"} for a browser tab target. Legacy {\"hwnd\":...} is intentionally unsupported.",
                "oneOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "window_hwnd"],
                        "properties": {
                            "kind": {
                                "type": "string",
                                "const": "window",
                                "description": "Bind a native top-level window HWND."
                            },
                            "window_hwnd": {
                                "type": "integer",
                                "description": "Native top-level window HWND to bind to this MCP session."
                            }
                        }
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "window_hwnd", "cdp_target_id"],
                        "properties": {
                            "kind": {
                                "type": "string",
                                "const": "cdp",
                                "description": "Bind a browser CDP target within a Chromium window."
                            },
                            "window_hwnd": {
                                "type": "integer",
                                "description": "Native browser window HWND whose CDP endpoint owns the target."
                            },
                            "cdp_target_id": {
                                "type": "string",
                                "description": "CDP Target.getTargets targetId for the tab/page to bind."
                            }
                        }
                    }
                ]
            }
        }
    });
    match schema {
        Value::Object(object) => object.into(),
        _ => Map::new().into(),
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[must_use]
pub fn observe_include(params: &ObserveParams) -> ObserveInclude {
    let mut include = if params.include.is_empty() {
        ObserveInclude::default()
    } else {
        ObserveInclude {
            focused: false,
            elements: false,
            entities: false,
            hud: false,
            audio: false,
            events: false,
            clipboard: false,
            fs: false,
            diagnostics: false,
            interactable_only: false,
            max_subtree_depth: 2,
            max_subtree_nodes: 60,
            element_offset: 0,
            max_entities: 60,
        }
    };
    for slot in &params.include {
        match slot {
            ObserveSlot::Focused => include.focused = true,
            ObserveSlot::Elements => include.elements = true,
            ObserveSlot::Entities => include.entities = true,
            ObserveSlot::Hud => include.hud = true,
            ObserveSlot::Audio => include.audio = true,
            ObserveSlot::Events => include.events = true,
            ObserveSlot::Clipboard => include.clipboard = true,
            ObserveSlot::Fs => include.fs = true,
            ObserveSlot::Diagnostics => include.diagnostics = true,
            ObserveSlot::Interactable => {
                include.elements = true;
                include.interactable_only = true;
            }
        }
    }
    include.max_subtree_depth = observe_gather_depth(params);
    include.max_subtree_nodes = params.max_elements.unwrap_or(60).clamp(1, 500);
    include.element_offset = params.element_offset.unwrap_or(0).min(100_000);
    include
}

/// A11y gather depth for one observe call. Interactable mode defaults to the
/// maximum walk depth (form fields live deep in real trees); everything else
/// keeps the historical default of 2. An explicit `depth` always wins.
#[must_use]
pub fn observe_gather_depth(params: &ObserveParams) -> u32 {
    let default_depth = if params.include.contains(&ObserveSlot::Interactable) {
        6
    } else {
        2
    };
    params.depth.unwrap_or(default_depth).min(6)
}

pub fn current_input(state: &M1State, depth: u32) -> Result<ObservationInput, ErrorData> {
    if state.force_observe_internal {
        return Err(mcp_error(
            error_codes::OBSERVE_INTERNAL,
            "forced observe internal error",
        ));
    }
    if state.force_no_perception {
        return Err(mcp_error(
            error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
            "no perception source is available",
        ));
    }
    if let Some(input) = &state.synthetic {
        let mut input = input_limited_to_depth(input.clone(), depth);
        if state.perception_mode != PerceptionMode::Auto {
            input.mode_override = Some(state.perception_mode);
        }
        input.capture_config = Some(state.active_capture_config.clone());
        input.capture_runtime = Some(state.capture_runtime_readback());
        return Ok(input);
    }
    let mut input = platform_input(depth, state.perception_mode)?;
    input.capture_config = Some(state.active_capture_config.clone());
    input.capture_runtime = Some(state.capture_runtime_readback());
    Ok(input)
}

pub fn observe_input(
    state: &M1State,
    params: &ObserveParams,
    target_hwnd: Option<i64>,
) -> Result<ObservationInput, ErrorData> {
    let depth = observe_gather_depth(params);
    if let Some(element_id) = &params.subtree_root {
        return element_input_from_id(element_id, depth, state.perception_mode);
    }
    // Precedence: explicit per-call window_hwnd > session active target >
    // foreground. The target path snapshots the window without foregrounding it.
    if let Some(hwnd) = params.window_hwnd.or(target_hwnd) {
        let mut input = window_input_from_hwnd(hwnd, depth, state.perception_mode)?;
        input.capture_config = Some(state.active_capture_config.clone());
        input.capture_runtime = Some(state.capture_runtime_readback());
        return Ok(input);
    }
    current_input(state, depth)
}

/// Attaches CDP (when reachable) and folds the page's DOM/accessibility tree
/// into `input.elements` as queryable web nodes (#685), upgrading `web_path` to
/// `cdp`. This is the async companion to the synchronous probe in
/// `sources::populate_cdp_diagnostics`: the probe reports *whether* a debug port
/// is reachable; this turns a reachable port into actual web content.
///
/// Fail-loud: an attach/tree failure flips `cdp.status` to `attach_failed` with
/// the specific reason code and detail, and leaves `web_path = uia_only` — never
/// a silent empty tree. Non-browser / no-port foregrounds are a no-op.
#[cfg(windows)]
pub async fn enrich_input_with_cdp(input: &mut ObservationInput, max_depth: u32, max_nodes: usize) {
    enrich_input_with_cdp_for_target(input, max_depth, max_nodes, None).await;
}

#[cfg(windows)]
pub async fn enrich_input_with_cdp_for_target(
    input: &mut ObservationInput,
    max_depth: u32,
    max_nodes: usize,
    target_id_hint: Option<&str>,
) {
    use synapse_core::{CdpStatus, WebPerceptionPath};

    let Some(cdp) = input.cdp.clone() else {
        return;
    };
    let hwnd = input.foreground.hwnd;
    let title = input.foreground.window_title.clone();
    let url_hint = foreground_web_url_hint(input);

    if cdp.status == CdpStatus::Ok {
        let Some(endpoint) = cdp.endpoint.clone() else {
            return;
        };
        match synapse_a11y::fetch_dom_snapshot(
            &endpoint,
            hwnd,
            &title,
            url_hint.as_deref(),
            target_id_hint,
            max_nodes,
        )
        .await
        {
            Ok(snapshot) => {
                let count = u32::try_from(snapshot.nodes.len()).unwrap_or(u32::MAX);
                for mut node in snapshot.nodes {
                    // Clamp web-node depth to the requested observe depth so deeply
                    // nested DOM elements still survive the element depth filter;
                    // parent links keep the true hierarchy.
                    node.depth = node.depth.min(max_depth);
                    input.elements.push(node);
                }
                input.web_path = Some(WebPerceptionPath::Cdp);
                if let Some(diagnostics) = input.cdp.as_mut() {
                    diagnostics.attached_node_count = Some(count);
                    diagnostics.selected_target_id = Some(snapshot.target_id.clone());
                    diagnostics.selected_session_id = Some(snapshot.session_id.clone());
                    diagnostics.target_selection_reason =
                        Some(snapshot.target_selection_reason.clone());
                    diagnostics.target_candidate_count = Some(snapshot.target_candidate_count);
                    diagnostics.frame_tree_frame_count = Some(snapshot.frame_tree_frame_count);
                    diagnostics.attached_frame_target_count =
                        Some(snapshot.attached_frame_target_count);
                    diagnostics.blocked_frame_targets = snapshot.blocked_frame_targets.clone();
                    diagnostics.frame_snapshot_errors = snapshot.frame_snapshot_errors.clone();
                }
                tracing::info!(
                    code = "A11Y_CDP_DOM_ATTACHED",
                    endpoint = %endpoint,
                    hwnd,
                    page_url = %snapshot.page_url,
                    target_id = %snapshot.target_id,
                    session_id = %snapshot.session_id,
                    requested_target_id = target_id_hint.unwrap_or_default(),
                    target_candidate_count = snapshot.target_candidate_count,
                    frame_tree_frame_count = snapshot.frame_tree_frame_count,
                    attached_frame_target_count = snapshot.attached_frame_target_count,
                    blocked_frame_target_count = snapshot.blocked_frame_targets.len(),
                    frame_snapshot_error_count = snapshot.frame_snapshot_errors.len(),
                    target_selection_reason = %snapshot.target_selection_reason,
                    node_count = count,
                    total_ax_nodes = snapshot.total_ax_nodes,
                    "attached CDP DOM tree into observation elements"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = error.code(),
                    endpoint = %endpoint,
                    hwnd,
                    requested_target_id = target_id_hint.unwrap_or_default(),
                    error = %error,
                    "CDP DOM snapshot failed; web content not exposed (web_path stays uia_only)"
                );
                if let Some(diagnostics) = input.cdp.as_mut() {
                    diagnostics.status = CdpStatus::AttachFailed;
                    diagnostics.reason_code = Some(error.code().to_owned());
                    diagnostics.detail = Some(error.to_string());
                }
            }
        }
        return;
    }

    if cdp.status != CdpStatus::Unreachable {
        return;
    }

    let detail = format!(
        "normal Chrome has no reachable raw CDP endpoint, and the old Chrome debugger-extension DOM snapshot path is disabled because it can trigger Chrome's layout-shifting debugger infobar; use a session-owned Chrome bridge target with target_act/browser_set_value/cdp_* typed routes, or a dedicated Synapse-launched automation profile for raw CDP attach work (requested_target_id={})",
        target_id_hint.unwrap_or_default()
    );
    tracing::warn!(
        code = error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
        hwnd,
        requested_target_id = target_id_hint.unwrap_or_default(),
        detail = %detail,
        "skipped deprecated Chrome debugger-extension DOM snapshot fallback"
    );
    if let Some(diagnostics) = input.cdp.as_mut() {
        diagnostics.status = CdpStatus::ExtensionUnavailable;
        diagnostics.reason_code = Some(error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE.to_owned());
        diagnostics.detail = Some(detail);
    }
}

#[cfg(windows)]
fn foreground_web_url_hint(input: &ObservationInput) -> Option<String> {
    input
        .elements
        .iter()
        .filter(|node| {
            node.role.eq_ignore_ascii_case("document")
                || node.automation_id.as_deref() == Some("RootWebArea")
        })
        .find_map(|node| {
            node.value
                .as_deref()
                .map(str::trim)
                .filter(|value| is_browser_url(value))
                .map(ToOwned::to_owned)
        })
}

#[cfg(windows)]
fn is_browser_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "http://",
        "https://",
        "file://",
        "chrome://",
        "edge://",
        "about:",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

const BROWSER_OCR_CHROME_TOP_PX: i32 = 96;
const BROWSER_OCR_TILE_HEIGHT_PX: i32 = 600;
const BROWSER_OCR_MAX_TILES: usize = 8;
const BROWSER_OVERLAY_OCR_MIN_ACTION_TOKENS: usize = 2;
const BROWSER_OVERLAY_OCR_MIN_NEW_TOKENS_WITH_ACTION: usize = 3;
const OCR_RUNTIME_RECT_PREFIX: &str = "0c0c01";

/// Recovers browser page text through screen OCR when CDP did not yield a DOM
/// tree. This is the degraded leg of the #687 ladder: it creates queryable text
/// nodes and upgrades `web_path` to `ocr` only when OCR actually returned text.
#[cfg(windows)]
pub fn enrich_input_with_browser_ocr(input: &mut ObservationInput, max_nodes: usize) {
    if max_nodes == 0 {
        return;
    }
    let should_full_ocr = should_attempt_browser_ocr(input);
    let should_overlay_ocr = should_attempt_browser_overlay_ocr(input);
    log_browser_ocr_guard(input, should_full_ocr, should_overlay_ocr);
    if should_full_ocr {
        enrich_input_with_full_browser_ocr(input, max_nodes);
    } else if should_overlay_ocr {
        enrich_input_with_overlay_browser_ocr(input, max_nodes);
    }
}

#[cfg(windows)]
fn log_browser_ocr_guard(
    input: &ObservationInput,
    should_full_ocr: bool,
    should_overlay_ocr: bool,
) {
    use synapse_core::WebPerceptionPath;

    if input.web_path != Some(WebPerceptionPath::UiaOnly)
        && !browser_ocr_cdp_failed(input)
        && !synapse_a11y::is_chromium_family(&input.foreground.process_name)
    {
        return;
    }
    let has_main_pane_uia = has_chromium_main_pane_uia_content(input);
    tracing::info!(
        code = "A11Y_BROWSER_OCR_GUARD_EVALUATED",
        hwnd = input.foreground.hwnd,
        process_name = %input.foreground.process_name,
        web_path = ?input.web_path,
        cdp_failed = browser_ocr_cdp_failed(input),
        has_main_pane_uia,
        should_full_ocr,
        should_overlay_ocr,
        element_count = input.elements.len(),
        "browser OCR guard evaluated"
    );
}

#[cfg(windows)]
fn enrich_input_with_full_browser_ocr(input: &mut ObservationInput, max_nodes: usize) {
    use synapse_core::{SensorStatus, WebPerceptionPath};

    if !matches!(
        input.capture_status,
        SensorStatus::Healthy | SensorStatus::DegradedLatency { .. }
    ) {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_SKIPPED_CAPTURE_UNAVAILABLE",
            hwnd = input.foreground.hwnd,
            capture_status = ?input.capture_status,
            "browser OCR fallback skipped because screen capture is not available"
        );
        return;
    }
    let Some(content_region) = browser_content_region(input.foreground.window_bounds) else {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_SKIPPED_EMPTY_REGION",
            hwnd = input.foreground.hwnd,
            window_bounds = ?input.foreground.window_bounds,
            "browser OCR fallback skipped because the browser content region is empty"
        );
        return;
    };

    let started = std::time::Instant::now();
    let mut words = Vec::new();
    let mut attempted_tiles = 0usize;
    let mut failed_tiles = 0usize;
    for tile in browser_ocr_tiles(content_region) {
        attempted_tiles += 1;
        match synapse_perception::read_text(tile) {
            Ok(mut tile_words) => words.append(&mut tile_words),
            Err(error) => {
                failed_tiles = failed_tiles.saturating_add(1);
                tracing::debug!(
                    code = error.code(),
                    hwnd = input.foreground.hwnd,
                    tile = ?tile,
                    error = %error,
                    "browser OCR tile produced no text"
                );
            }
        }
        if words.len() >= max_nodes {
            break;
        }
    }
    input
        .sensor_latency_ms
        .insert("ocr".to_owned(), started.elapsed().as_secs_f32() * 1000.0);

    let added = apply_browser_ocr_words(input, words, max_nodes);
    if added == 0 {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_NO_TEXT",
            hwnd = input.foreground.hwnd,
            content_region = ?content_region,
            attempted_tiles,
            failed_tiles,
            "browser OCR fallback found no readable page text; web_path remains uia_only"
        );
        return;
    }
    tracing::info!(
        code = "A11Y_BROWSER_OCR_ATTACHED",
        hwnd = input.foreground.hwnd,
        content_region = ?content_region,
        attempted_tiles,
        failed_tiles,
        node_count = added,
        web_path = %WebPerceptionPath::Ocr.as_str(),
        "browser OCR fallback added queryable text nodes"
    );
}

#[cfg(windows)]
fn enrich_input_with_overlay_browser_ocr(input: &mut ObservationInput, max_nodes: usize) {
    use synapse_core::{SensorStatus, WebPerceptionPath};

    if !matches!(
        input.capture_status,
        SensorStatus::Healthy | SensorStatus::DegradedLatency { .. }
    ) {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_OVERLAY_SKIPPED_CAPTURE_UNAVAILABLE",
            hwnd = input.foreground.hwnd,
            capture_status = ?input.capture_status,
            "browser overlay OCR probe skipped because screen capture is not available"
        );
        return;
    }
    let Some(content_region) = browser_content_region(input.foreground.window_bounds) else {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_OVERLAY_SKIPPED_EMPTY_REGION",
            hwnd = input.foreground.hwnd,
            window_bounds = ?input.foreground.window_bounds,
            "browser overlay OCR probe skipped because the browser content region is empty"
        );
        return;
    };
    let Some(probe_region) = browser_overlay_probe_region(content_region) else {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_OVERLAY_SKIPPED_EMPTY_PROBE",
            hwnd = input.foreground.hwnd,
            content_region = ?content_region,
            "browser overlay OCR probe skipped because the modal probe region is empty"
        );
        return;
    };

    let started = std::time::Instant::now();
    let words = match synapse_perception::read_text(probe_region) {
        Ok(words) => words,
        Err(error) => {
            input.sensor_latency_ms.insert(
                "ocr_overlay".to_owned(),
                started.elapsed().as_secs_f32() * 1000.0,
            );
            tracing::debug!(
                code = error.code(),
                hwnd = input.foreground.hwnd,
                probe_region = ?probe_region,
                error = %error,
                "browser overlay OCR probe produced no text"
            );
            return;
        }
    };
    input.sensor_latency_ms.insert(
        "ocr_overlay".to_owned(),
        started.elapsed().as_secs_f32() * 1000.0,
    );

    let BrowserOverlayOcrGap {
        words: unexposed_words,
        new_token_count,
        new_action_token_count,
        cluster_region,
    } = browser_overlay_ocr_gap(input, words, probe_region);
    if !browser_overlay_ocr_gap_is_actionable(
        new_token_count,
        new_action_token_count,
        cluster_region,
        probe_region,
    ) {
        tracing::debug!(
            code = "A11Y_BROWSER_OCR_OVERLAY_NO_UNEXPOSED_TEXT",
            hwnd = input.foreground.hwnd,
            probe_region = ?probe_region,
            cluster_region = ?cluster_region,
            new_token_count,
            new_action_token_count,
            "browser overlay OCR probe found no actionable visible text missing from UIA"
        );
        return;
    }

    let added = apply_browser_ocr_words(input, unexposed_words, max_nodes);
    if added == 0 {
        tracing::warn!(
            code = "A11Y_BROWSER_OCR_OVERLAY_NO_USABLE_NODES",
            hwnd = input.foreground.hwnd,
            probe_region = ?probe_region,
            new_token_count,
            new_action_token_count,
            "browser overlay OCR probe found missing text but produced no usable OCR nodes"
        );
        return;
    }
    tracing::info!(
        code = "A11Y_BROWSER_OCR_OVERLAY_ATTACHED",
        hwnd = input.foreground.hwnd,
        probe_region = ?probe_region,
        node_count = added,
        new_token_count,
        new_action_token_count,
        cluster_region = ?cluster_region,
        web_path = %WebPerceptionPath::Ocr.as_str(),
        "browser overlay OCR added visible top-layer text omitted by UIA"
    );
}

#[cfg(not(windows))]
pub fn enrich_input_with_browser_ocr(_input: &mut ObservationInput, _max_nodes: usize) {}

fn should_attempt_browser_ocr(input: &ObservationInput) -> bool {
    use synapse_core::WebPerceptionPath;

    if input.web_path != Some(WebPerceptionPath::UiaOnly) {
        return false;
    }
    if has_chromium_main_pane_uia_content(input) {
        return false;
    }
    browser_ocr_cdp_failed(input)
}

fn should_attempt_browser_overlay_ocr(input: &ObservationInput) -> bool {
    use synapse_core::WebPerceptionPath;

    if input.web_path != Some(WebPerceptionPath::UiaOnly) {
        return false;
    }
    if !has_chromium_main_pane_uia_content(input) {
        return false;
    }
    browser_ocr_cdp_failed(input)
}

fn browser_ocr_cdp_failed(input: &ObservationInput) -> bool {
    use synapse_core::CdpStatus;

    input.cdp.as_ref().is_some_and(|diagnostics| {
        matches!(
            diagnostics.status,
            CdpStatus::Unreachable | CdpStatus::AttachFailed | CdpStatus::ExtensionUnavailable
        )
    })
}

struct BrowserOverlayOcrGap {
    words: Vec<synapse_perception::TextRegion>,
    new_token_count: usize,
    new_action_token_count: usize,
    cluster_region: Option<Rect>,
}

fn browser_overlay_ocr_gap(
    input: &ObservationInput,
    words: Vec<synapse_perception::TextRegion>,
    probe_region: Rect,
) -> BrowserOverlayOcrGap {
    use std::collections::HashSet;

    let exposed_tokens = browser_uia_tokens_in_region(input, probe_region);
    let mut candidates = Vec::new();
    let mut action_region = None;

    for word in words {
        if word.bbox.w <= 0 || word.bbox.h <= 0 || !rects_overlap(word.bbox, probe_region) {
            continue;
        }
        let tokens: Vec<_> = meaningful_text_tokens(&word.text)
            .into_iter()
            .filter(|token| !exposed_tokens.contains(token))
            .collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens
            .iter()
            .any(|token| browser_overlay_action_token(token))
        {
            action_region = Some(match action_region {
                Some(current) => union_rect(current, word.bbox),
                None => word.bbox,
            });
        }
        candidates.push((word, tokens));
    }

    let Some(action_region) = action_region else {
        return BrowserOverlayOcrGap {
            words: Vec::new(),
            new_token_count: 0,
            new_action_token_count: 0,
            cluster_region: None,
        };
    };
    let candidate_region = browser_overlay_action_neighborhood(action_region, probe_region);
    let mut new_tokens = HashSet::new();
    let mut new_action_tokens = HashSet::new();
    let mut unexposed_words = Vec::new();
    let mut cluster_region = None;

    for (word, tokens) in candidates {
        if !rects_overlap(word.bbox, candidate_region) {
            continue;
        }
        for token in tokens {
            if browser_overlay_action_token(&token) {
                new_action_tokens.insert(token.clone());
            }
            new_tokens.insert(token);
        }
        cluster_region = Some(match cluster_region {
            Some(current) => union_rect(current, word.bbox),
            None => word.bbox,
        });
        unexposed_words.push(word);
    }

    BrowserOverlayOcrGap {
        words: unexposed_words,
        new_token_count: new_tokens.len(),
        new_action_token_count: new_action_tokens.len(),
        cluster_region,
    }
}

fn browser_overlay_ocr_gap_is_actionable(
    new_token_count: usize,
    new_action_token_count: usize,
    cluster_region: Option<Rect>,
    probe_region: Rect,
) -> bool {
    let Some(cluster_region) = cluster_region else {
        return false;
    };
    browser_overlay_ocr_cluster_is_compact(cluster_region, probe_region)
        && (new_action_token_count >= BROWSER_OVERLAY_OCR_MIN_ACTION_TOKENS
            || (new_token_count >= BROWSER_OVERLAY_OCR_MIN_NEW_TOKENS_WITH_ACTION
                && new_action_token_count > 0))
}

fn browser_overlay_ocr_cluster_is_compact(cluster_region: Rect, probe_region: Rect) -> bool {
    if cluster_region.w <= 0 || cluster_region.h <= 0 || probe_region.w <= 0 || probe_region.h <= 0
    {
        return false;
    }
    let max_width = ((probe_region.w / 5).saturating_mul(4)).max(1);
    let max_height = ((probe_region.h / 5).saturating_mul(3)).max(1);
    cluster_region.w <= max_width && cluster_region.h <= max_height
}

fn browser_overlay_action_neighborhood(action_region: Rect, probe_region: Rect) -> Rect {
    let pad_x = (probe_region.w / 10).clamp(80, 180);
    let pad_y = (probe_region.h / 10).clamp(72, 160);
    let left = action_region.x.saturating_sub(pad_x).max(probe_region.x);
    let top = action_region.y.saturating_sub(pad_y).max(probe_region.y);
    let right = action_region
        .x
        .saturating_add(action_region.w)
        .saturating_add(pad_x)
        .min(probe_region.x.saturating_add(probe_region.w));
    let bottom = action_region
        .y
        .saturating_add(action_region.h)
        .saturating_add(pad_y)
        .min(probe_region.y.saturating_add(probe_region.h));
    Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = a.x.saturating_add(a.w).max(b.x.saturating_add(b.w));
    let bottom = a.y.saturating_add(a.h).max(b.y.saturating_add(b.h));
    Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    }
}

fn browser_uia_tokens_in_region(
    input: &ObservationInput,
    region: Rect,
) -> std::collections::HashSet<String> {
    let mut tokens = std::collections::HashSet::new();
    for node in &input.elements {
        if node
            .automation_id
            .as_deref()
            .is_some_and(|automation_id| automation_id.starts_with("ocr:"))
        {
            continue;
        }
        if node.bbox.w <= 0 || node.bbox.h <= 0 || !rects_overlap(node.bbox, region) {
            continue;
        }
        tokens.extend(meaningful_text_tokens(&node.name));
        if let Some(value) = &node.value {
            tokens.extend(meaningful_text_tokens(value));
        }
    }
    tokens
}

fn meaningful_text_tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter_map(|part| {
            let token = part.trim().to_lowercase();
            if token.chars().count() < 3 || token.chars().all(|ch| ch.is_ascii_digit()) {
                None
            } else {
                Some(token)
            }
        })
        .collect()
}

fn browser_overlay_action_token(token: &str) -> bool {
    matches!(
        token,
        "allow"
            | "authorize"
            | "cancel"
            | "close"
            | "compose"
            | "confirm"
            | "continue"
            | "create"
            | "delete"
            | "discard"
            | "done"
            | "post"
            | "publish"
            | "remove"
            | "reply"
            | "save"
            | "send"
            | "share"
            | "submit"
            | "update"
    )
}

fn has_chromium_main_pane_uia_content(input: &ObservationInput) -> bool {
    if !synapse_a11y::is_chromium_family(&input.foreground.process_name) {
        return false;
    }
    let Some(main_region) = browser_main_pane_region(input.foreground.window_bounds) else {
        return false;
    };
    input.elements.iter().any(|node| {
        if node
            .automation_id
            .as_deref()
            .is_some_and(|automation_id| automation_id.starts_with("ocr:"))
        {
            return false;
        }
        if node.bbox.w <= 0 || node.bbox.h <= 0 || !rects_overlap(node.bbox, main_region) {
            return false;
        }
        let role = node.role.to_ascii_lowercase();
        matches!(
            role.as_str(),
            "button"
                | "cell"
                | "data_item"
                | "edit"
                | "heading"
                | "hyperlink"
                | "link"
                | "list_item"
                | "row"
                | "text"
        ) && (!node.name.trim().is_empty()
            || node
                .value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || !node.patterns.is_empty()
            || role == "document")
    })
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    let a_right = a.x.saturating_add(a.w);
    let a_bottom = a.y.saturating_add(a.h);
    let b_right = b.x.saturating_add(b.w);
    let b_bottom = b.y.saturating_add(b.h);
    a.x < b_right && a_right > b.x && a.y < b_bottom && a_bottom > b.y
}

fn apply_browser_ocr_words(
    input: &mut ObservationInput,
    words: Vec<synapse_perception::TextRegion>,
    max_nodes: usize,
) -> usize {
    let nodes = browser_ocr_nodes(input.foreground.hwnd, words, max_nodes);
    if nodes.is_empty() {
        return 0;
    }
    let added = nodes.len();
    input.elements.extend(nodes);
    input.web_path = Some(synapse_core::WebPerceptionPath::Ocr);
    added
}

fn browser_ocr_nodes(
    hwnd: i64,
    words: Vec<synapse_perception::TextRegion>,
    max_nodes: usize,
) -> Vec<AccessibleNode> {
    words
        .into_iter()
        .filter(|word| !word.text.trim().is_empty() && word.bbox.w > 0 && word.bbox.h > 0)
        .take(max_nodes)
        .enumerate()
        .map(|(index, word)| {
            let trimmed = word.text.trim().to_owned();
            let bbox = word.bbox;
            AccessibleNode {
                element_id: browser_ocr_element_id(hwnd, bbox, index),
                parent: None,
                name: trimmed,
                role: "text".to_owned(),
                automation_id: Some(format!("ocr:word:{index}")),
                value: None,
                bbox,
                enabled: true,
                focused: false,
                patterns: Vec::new(),
                children_count: 0,
                depth: 1,
            }
        })
        .collect()
}

fn browser_ocr_element_id(hwnd: i64, bbox: Rect, index: usize) -> ElementId {
    synapse_core::element_id(
        hwnd,
        &format!(
            "{OCR_RUNTIME_RECT_PREFIX}{:08x}{:08x}{:08x}{:08x}{index:08x}",
            bbox.x.cast_unsigned(),
            bbox.y.cast_unsigned(),
            bbox.w.cast_unsigned(),
            bbox.h.cast_unsigned()
        ),
    )
}

pub(crate) fn browser_ocr_rect_from_element_id(element_id: &ElementId) -> Option<Rect> {
    let parts = element_id.parts().ok()?;
    browser_ocr_rect_from_runtime_id(&parts.runtime_id_hex)
}

pub(crate) fn is_browser_ocr_element_id(element_id: &ElementId) -> bool {
    element_id
        .parts()
        .is_ok_and(|parts| parts.runtime_id_hex.starts_with(OCR_RUNTIME_RECT_PREFIX))
}

fn browser_ocr_rect_from_runtime_id(runtime_id_hex: &str) -> Option<Rect> {
    let encoded = runtime_id_hex.strip_prefix(OCR_RUNTIME_RECT_PREFIX)?;
    if encoded.len() < 32 {
        return None;
    }
    let x = i32_from_hex_u32(&encoded[0..8])?;
    let y = i32_from_hex_u32(&encoded[8..16])?;
    let w = i32_from_hex_u32(&encoded[16..24])?;
    let h = i32_from_hex_u32(&encoded[24..32])?;
    (w > 0 && h > 0).then_some(Rect { x, y, w, h })
}

fn i32_from_hex_u32(raw: &str) -> Option<i32> {
    u32::from_str_radix(raw, 16).ok().map(u32::cast_signed)
}

fn browser_content_region(window_bounds: Rect) -> Option<Rect> {
    if window_bounds.w <= 0 || window_bounds.h <= 0 {
        return None;
    }
    let top_inset = if window_bounds.h > 240 {
        BROWSER_OCR_CHROME_TOP_PX.min(window_bounds.h / 3)
    } else {
        0
    };
    let height = window_bounds.h.saturating_sub(top_inset);
    (height > 0).then_some(Rect {
        x: window_bounds.x,
        y: window_bounds.y.saturating_add(top_inset),
        w: window_bounds.w,
        h: height,
    })
}

fn browser_main_pane_region(window_bounds: Rect) -> Option<Rect> {
    let content = browser_content_region(window_bounds)?;
    if content.w <= 1 {
        return Some(content);
    }
    let left_inset = (content.w / 4).clamp(240, 720).min(content.w - 1);
    Some(Rect {
        x: content.x.saturating_add(left_inset),
        y: content.y,
        w: content.w.saturating_sub(left_inset),
        h: content.h,
    })
}

fn browser_overlay_probe_region(content_region: Rect) -> Option<Rect> {
    if content_region.w <= 0 || content_region.h <= 0 {
        return None;
    }
    let width = if content_region.w >= 640 {
        content_region.w / 2
    } else {
        content_region.w
    }
    .clamp(1, content_region.w);
    let height = if content_region.h >= 480 {
        content_region.h / 2
    } else {
        content_region.h
    }
    .clamp(1, content_region.h);

    Some(Rect {
        x: content_region
            .x
            .saturating_add((content_region.w - width) / 2),
        y: content_region
            .y
            .saturating_add((content_region.h - height) / 2),
        w: width,
        h: height,
    })
}

fn browser_ocr_tiles(content_region: Rect) -> Vec<Rect> {
    if content_region.w <= 0 || content_region.h <= 0 {
        return Vec::new();
    }
    let mut tiles = Vec::new();
    let tile_height = content_region.h.min(BROWSER_OCR_TILE_HEIGHT_PX).max(1);
    let bottom = content_region.y.saturating_add(content_region.h);
    let mut y = content_region.y;
    while y < bottom && tiles.len() < BROWSER_OCR_MAX_TILES {
        let height = bottom.saturating_sub(y).min(tile_height).max(1);
        tiles.push(Rect {
            x: content_region.x,
            y,
            w: content_region.w,
            h: height,
        });
        y = y.saturating_add(height);
    }
    tiles
}

#[cfg(not(windows))]
#[allow(clippy::unused_async)]
pub async fn enrich_input_with_cdp(
    _input: &mut ObservationInput,
    _max_depth: u32,
    _max_nodes: usize,
) {
}

#[cfg(not(windows))]
#[allow(clippy::unused_async)]
pub async fn enrich_input_with_cdp_for_target(
    _input: &mut ObservationInput,
    _max_depth: u32,
    _max_nodes: usize,
    _target_id_hint: Option<&str>,
) {
}

fn input_limited_to_depth(mut input: ObservationInput, depth: u32) -> ObservationInput {
    input.elements.retain(|node| node.depth <= depth);
    if let Some(focused) = &input.focused {
        let focused_present = input
            .elements
            .iter()
            .any(|node| node.element_id == focused.element_id);
        if focused_present {
            return input;
        }
    }
    input.focused = input.elements.first().map(focused_from_accessible_node);
    input
}

fn focused_from_accessible_node(node: &AccessibleNode) -> FocusedElement {
    FocusedElement {
        element_id: node.element_id.clone(),
        name: node.name.clone(),
        role: node.role.clone(),
        automation_id: node.automation_id.clone(),
        bbox: node.bbox,
        enabled: node.enabled,
        patterns: node.patterns.clone(),
        value: node.value.clone(),
        selected_text: None,
    }
}

/// Depth `find` walks the foreground tree. `observe`'s default is shallow (2),
/// but `find` must reach deeply-nested controls (e.g. a UWP app's display text
/// at depth ~5, or toolbar tool buttons), so it requests a deep snapshot. The
/// snapshot's node-budget/deadline bounds the cost.
const FIND_SNAPSHOT_DEPTH: u32 = 16;

/// Upper bound on CDP web nodes folded into a `find` snapshot. Web pages have
/// far more nodes than native windows, and `find` walks deeper than `observe`.
const FIND_CDP_MAX_NODES: usize = 300;

/// Builds the perception input a `find` query searches (foreground or a specific
/// window), including detection entities. Split from matching so the async `find`
/// handler can fold in CDP web nodes (#685) before matching.
pub fn build_find_input(
    state: &mut M1State,
    params: &FindParams,
    target_hwnd: Option<i64>,
) -> Result<ObservationInput, ErrorData> {
    // Precedence matches observe: explicit window_hwnd > session target > foreground.
    let mut input = if let Some(hwnd) = params.window_hwnd.or(target_hwnd) {
        let mut input = window_input_from_hwnd(hwnd, FIND_SNAPSHOT_DEPTH, state.perception_mode)?;
        input.capture_config = Some(state.active_capture_config.clone());
        input.capture_runtime = Some(state.capture_runtime_readback());
        input
    } else {
        current_input(state, FIND_SNAPSHOT_DEPTH)?
    };
    populate_detection_from_state(state, &mut input);
    Ok(input)
}

/// Maximum CDP web nodes a `find` query folds in. Exposed so the async handler
/// can size its enrichment to match `find`'s deep snapshot.
#[must_use]
pub const fn find_cdp_max_nodes() -> usize {
    FIND_CDP_MAX_NODES
}

/// `find`'s snapshot depth (deep, so nested controls are reachable).
#[must_use]
pub const fn find_snapshot_depth() -> u32 {
    FIND_SNAPSHOT_DEPTH
}

/// Matches a prepared input against the `find` query.
#[must_use]
pub fn match_find_input(input: &ObservationInput, params: &FindParams) -> FindResponse {
    let limit = params.limit.unwrap_or(5).clamp(1, 20);
    let mut results = Vec::new();
    if matches!(
        params.scope.unwrap_or_default(),
        FindScope::Elements | FindScope::Both
    ) {
        results.extend(
            input
                .elements
                .iter()
                .filter_map(|node| element_match(node, params)),
        );
    }
    if matches!(
        params.scope.unwrap_or_default(),
        FindScope::Entities | FindScope::Both
    ) {
        results.extend(
            input
                .entities
                .iter()
                .filter_map(|entity| entity_match(entity, params)),
        );
    }
    results.sort_by(|left, right| right.score.total_cmp(&left.score));
    results.truncate(limit);
    FindResponse {
        results,
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    }
}

pub fn set_capture_target_in_state(
    state: &mut M1State,
    params: SetCaptureTargetParams,
) -> Result<SetCaptureTargetResponse, ErrorData> {
    let previous = capture_target_wire(&state.capture_config.target);
    let mut config = state.capture_config.clone();
    config.target = capture_target_from_param(params.target)?;
    if let Some(interval) = params.min_update_interval_ms {
        config.min_update_interval_ms = clamp_capture_interval(interval);
    }
    if let Some(cursor_visible) = params.cursor_visible {
        config.cursor_visible = cursor_visible;
    }
    if let Some(dirty_region_only) = params.dirty_region_only {
        config.dirty_region_only = dirty_region_only;
    }
    let resolved =
        resolve_capture_target(&config).map_err(|err| mcp_error(err.code(), err.to_string()))?;
    let generation = state
        .capture_controller
        .switch_to(config.clone())
        .map_err(|err| mcp_error(err.code(), err.to_string()))?;
    state.capture_config = config;
    state.capture_generation = generation;
    state.active_capture_config = observation_capture_from_capture_config(
        &state.capture_config,
        state.capture_generation,
        "manual".to_owned(),
    );
    Ok(SetCaptureTargetResponse {
        previous,
        current: capture_target_wire(&resolved.target),
        generation: state.capture_generation,
        backend: capture_backend_name(resolved.backend).to_owned(),
        capture_runtime: state.capture_runtime_readback(),
    })
}

pub fn apply_profile_runtime_config_in_state(
    state: &mut M1State,
    profile: &Profile,
) -> Result<ObservationCaptureConfig, ErrorData> {
    if state.manual_perception_mode.is_none() {
        state.perception_mode = profile.mode;
    }
    state.detection_config = detection_config_from_profile(&profile.detection);

    let mut config = state.capture_config.clone();
    config.min_update_interval_ms = u64::from(
        profile
            .capture
            .min_update_interval_ms
            .max(MIN_CAPTURE_UPDATE_INTERVAL_MS_U32),
    );
    config.cursor_visible = profile.capture.cursor_visible;
    if let Some(target) = capture_target_from_profile_target(&profile.capture.target) {
        config.target = target;
        resolve_capture_target(&config).map_err(|err| mcp_error(err.code(), err.to_string()))?;
        state.capture_config.target = config.target.clone();
    }
    state.capture_config.min_update_interval_ms = config.min_update_interval_ms;
    state.capture_config.cursor_visible = config.cursor_visible;

    let mut active_capture = observation_capture_from_profile_capture(
        &profile.capture,
        state.capture_config.dirty_region_only,
        state.capture_generation,
        format!("profile:{}", profile.id),
    );
    if capture_config_without_generation_eq(&state.active_capture_config, &active_capture) {
        active_capture.generation = state.active_capture_config.generation;
    } else {
        state.capture_generation = state.capture_generation.saturating_add(1);
        active_capture.generation = state.capture_generation;
    }
    state.active_capture_config = active_capture.clone();
    Ok(active_capture)
}

pub fn set_perception_mode_in_state(
    state: &mut M1State,
    params: &SetPerceptionModeParams,
) -> Result<SetPerceptionModeResponse, ErrorData> {
    let previous = state.perception_mode;
    let mode = parse_perception_mode(&params.mode)
        .map_err(|err| mcp_error(err.code(), err.to_string()))?;
    state.perception_mode = mode;
    state.manual_perception_mode = (mode != PerceptionMode::Auto).then_some(mode);
    Ok(SetPerceptionModeResponse {
        previous,
        mode,
        rationale: mode_rationale(mode).to_owned(),
    })
}

fn detection_config_from_profile(profile: &ProfileDetection) -> DetectionRuntimeConfig {
    DetectionRuntimeConfig::from_profile(profile)
}

pub fn mcp_error(code: &'static str, message: impl Into<String>) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        message,
        Some(json!({ "code": code })),
    )
}

fn default_observation_capture_config() -> ObservationCaptureConfig {
    observation_capture_from_capture_config(&CaptureConfig::default(), 0, "default".to_owned())
}

fn observation_capture_from_capture_config(
    config: &CaptureConfig,
    generation: u64,
    source: String,
) -> ObservationCaptureConfig {
    ObservationCaptureConfig {
        target: observation_target_from_capture_target(&config.target),
        min_update_interval_ms: u32::try_from(config.min_update_interval_ms)
            .unwrap_or(u32::MAX)
            .max(MIN_CAPTURE_UPDATE_INTERVAL_MS_U32),
        cursor_visible: config.cursor_visible,
        dirty_region_only: config.dirty_region_only,
        generation,
        source,
    }
}

fn observation_capture_from_profile_capture(
    capture: &ProfileCapture,
    dirty_region_only: bool,
    generation: u64,
    source: String,
) -> ObservationCaptureConfig {
    ObservationCaptureConfig {
        target: observation_target_from_profile_target(&capture.target),
        min_update_interval_ms: capture
            .min_update_interval_ms
            .max(MIN_CAPTURE_UPDATE_INTERVAL_MS_U32),
        cursor_visible: capture.cursor_visible,
        dirty_region_only,
        generation,
        source,
    }
}

const fn observation_target_from_capture_target(
    target: &CaptureTarget,
) -> ObservationCaptureTarget {
    match target {
        CaptureTarget::Primary => ObservationCaptureTarget::PrimaryMonitor,
        CaptureTarget::Monitor { monitor_index } => ObservationCaptureTarget::MonitorIndex {
            index: *monitor_index,
        },
        CaptureTarget::Window { hwnd } => ObservationCaptureTarget::Window { window_hwnd: *hwnd },
    }
}

const fn observation_target_from_profile_target(
    target: &ProfileCaptureTarget,
) -> ObservationCaptureTarget {
    match target {
        ProfileCaptureTarget::ForegroundWindow => ObservationCaptureTarget::ForegroundWindow,
        ProfileCaptureTarget::PrimaryMonitor => ObservationCaptureTarget::PrimaryMonitor,
        ProfileCaptureTarget::MonitorIndex { index } => {
            ObservationCaptureTarget::MonitorIndex { index: *index }
        }
    }
}

const fn capture_target_from_profile_target(
    target: &ProfileCaptureTarget,
) -> Option<CaptureTarget> {
    match target {
        ProfileCaptureTarget::ForegroundWindow => None,
        ProfileCaptureTarget::PrimaryMonitor => Some(CaptureTarget::Primary),
        ProfileCaptureTarget::MonitorIndex { index } => Some(CaptureTarget::Monitor {
            monitor_index: *index,
        }),
    }
}

fn capture_config_without_generation_eq(
    left: &ObservationCaptureConfig,
    right: &ObservationCaptureConfig,
) -> bool {
    left.target == right.target
        && left.min_update_interval_ms == right.min_update_interval_ms
        && left.cursor_visible == right.cursor_visible
        && left.dirty_region_only == right.dirty_region_only
        && left.source == right.source
}

const fn clamp_capture_interval(interval_ms: u64) -> u64 {
    if interval_ms < MIN_CAPTURE_UPDATE_INTERVAL_MS {
        MIN_CAPTURE_UPDATE_INTERVAL_MS
    } else {
        interval_ms
    }
}

fn capture_target_from_param(param: CaptureTargetParam) -> Result<CaptureTarget, ErrorData> {
    match param {
        CaptureTargetParam::Primary => Ok(CaptureTarget::Primary),
        CaptureTargetParam::Monitor { monitor_index } => {
            Ok(CaptureTarget::Monitor { monitor_index })
        }
        CaptureTargetParam::Window { window_hwnd } => {
            Ok(CaptureTarget::Window { hwnd: window_hwnd })
        }
        CaptureTargetParam::ElementWindow { element_id } => {
            let rect = synapse_a11y::element_bounding_rect(&element_id).map_err(|err| {
                mcp_error(
                    error_codes::CAPTURE_TARGET_INVALID,
                    format!("element_window target could not be re-resolved: {err}"),
                )
            })?;
            validate_element_window_rect(&element_id, rect)?;
            element_id
                .parts()
                .map(|parts| CaptureTarget::Window { hwnd: parts.hwnd })
                .map_err(|err| mcp_error(error_codes::CAPTURE_TARGET_INVALID, err.to_string()))
        }
    }
}

fn validate_element_window_rect(element_id: &ElementId, rect: Rect) -> Result<(), ErrorData> {
    if rect.w <= 0 || rect.h <= 0 {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "element_window target is not displaying a non-empty UI rectangle: element_id={element_id} bbox=({}, {}, {}, {})",
                rect.x, rect.y, rect.w, rect.h
            ),
        ));
    }

    Ok(())
}

const fn capture_target_wire(target: &CaptureTarget) -> CaptureTargetWire {
    match target {
        CaptureTarget::Primary => CaptureTargetWire::Primary,
        CaptureTarget::Monitor { monitor_index } => CaptureTargetWire::Monitor {
            monitor_index: *monitor_index,
        },
        CaptureTarget::Window { hwnd } => CaptureTargetWire::Window { window_hwnd: *hwnd },
    }
}

const fn capture_backend_name(backend: CaptureBackend) -> &'static str {
    match backend {
        CaptureBackend::GraphicsCaptureApi => "graphics_capture_api",
        CaptureBackend::DxgiDuplication => "dxgi_duplication",
    }
}

fn capture_thread_priority_name(priority: CaptureThreadPriority) -> String {
    match priority {
        CaptureThreadPriority::TimeCritical => "time_critical".to_owned(),
        CaptureThreadPriority::Unsupported => "unsupported".to_owned(),
        CaptureThreadPriority::Unknown => "unknown".to_owned(),
        CaptureThreadPriority::Other(value) => format!("other:{value}"),
    }
}

const fn mode_rationale(mode: PerceptionMode) -> &'static str {
    match mode {
        PerceptionMode::Auto => "auto_select_by_foreground_and_a11y_density",
        PerceptionMode::A11yOnly => "manual_a11y_only",
        PerceptionMode::PixelOnly => "manual_pixel_only",
        PerceptionMode::Hybrid => "manual_hybrid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use synapse_core::{
        Backend, CdpDiagnostics, CdpStatus, ProfileBackends, ProfileDetection, ProfileMatch,
        ProfileOcr, ProfileUseScope, SensorStatus, WebPerceptionPath,
    };
    use synapse_perception::TextRegion;

    /// #882: `include:["interactable"]` implies elements, flips the semantic
    /// filter on, and raises the default gather depth to the maximum; an
    /// explicit `depth` always wins.
    #[test]
    fn interactable_slot_implies_elements_and_deep_gather() {
        let params: ObserveParams = serde_json::from_value(json!({
            "include": ["interactable"]
        }))
        .expect("interactable include slot must deserialize");
        let include = observe_include(&params);
        println!(
            "readback=observe_include edge=interactable elements={} interactable_only={} depth={}",
            include.elements, include.interactable_only, include.max_subtree_depth
        );
        assert!(include.elements);
        assert!(include.interactable_only);
        assert!(!include.diagnostics);
        assert_eq!(include.max_subtree_depth, 6);
        assert_eq!(observe_gather_depth(&params), 6);

        let explicit_depth: ObserveParams = serde_json::from_value(json!({
            "include": ["interactable"],
            "depth": 3
        }))
        .expect("explicit depth with interactable must deserialize");
        assert_eq!(observe_gather_depth(&explicit_depth), 3);

        let default_params = ObserveParams::default();
        let default_include = observe_include(&default_params);
        println!(
            "readback=observe_include edge=default interactable_only={} depth={}",
            default_include.interactable_only, default_include.max_subtree_depth
        );
        assert!(!default_include.interactable_only);
        assert!(default_include.diagnostics);
        assert_eq!(observe_gather_depth(&default_params), 2);
    }

    #[test]
    fn set_target_deserialize_names_canonical_shapes() {
        let canonical: SetTargetParams = serde_json::from_value(json!({
            "target": {
                "kind": "window",
                "window_hwnd": 1234
            }
        }))
        .expect("canonical window target shape must deserialize");
        match canonical.target {
            SetTargetParam::Window { window_hwnd } => assert_eq!(window_hwnd, 1234),
            SetTargetParam::Cdp { .. } => panic!("expected window target"),
        }

        let alias_error = serde_json::from_value::<SetTargetParams>(json!({
            "target": {
                "hwnd": 1234
            }
        }))
        .expect_err("legacy hwnd alias must fail loudly")
        .to_string();
        assert!(alias_error.contains("does not accept legacy field `hwnd`"));
        assert!(alias_error.contains("\"kind\":\"window\""));
        assert!(alias_error.contains("window_hwnd"));

        let missing_kind_error = serde_json::from_value::<SetTargetParams>(json!({
            "target": {
                "window_hwnd": 1234
            }
        }))
        .expect_err("missing kind must name accepted target shapes")
        .to_string();
        assert!(missing_kind_error.contains("missing string field `kind`"));
        assert!(missing_kind_error.contains("\"kind\":\"cdp\""));
    }

    /// Real-window Full-State-Verification for per-agent target perception
    /// (#736/#737): bind a BACKGROUND window as the target and prove `observe`
    /// returns that window's content WITHOUT stealing the foreground. Source of
    /// truth = `synapse_a11y::current_foreground_context()` (the real OS
    /// foreground) read separately before/after. Spawns real Notepad + mspaint,
    /// so it is `#[ignore]` by default; run on an interactive desktop with
    /// `cargo test -p synapse-mcp --bins observe_target_window -- --ignored --nocapture`.
    #[cfg(windows)]
    #[ignore = "spawns real Notepad + mspaint; run on an interactive desktop with --ignored"]
    #[test]
    fn observe_target_window_in_background_without_foreground_steal() -> anyhow::Result<()> {
        use std::{
            process::Command,
            thread::sleep,
            time::{Duration, Instant},
        };

        fn wait_for_foreground_process(name: &str) -> Option<synapse_core::ForegroundContext> {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if let Ok(foreground) = synapse_a11y::current_foreground_context()
                    && foreground.process_name.eq_ignore_ascii_case(name)
                {
                    return Some(foreground);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                sleep(Duration::from_millis(150));
            }
        }

        // 1) Launch Notepad and capture its HWND while it is the foreground.
        let mut notepad = Command::new("notepad.exe").spawn()?;
        let notepad_fg = wait_for_foreground_process("notepad.exe")
            .ok_or_else(|| anyhow::anyhow!("notepad did not reach the foreground"))?;
        let notepad_hwnd = notepad_fg.hwnd;
        println!(
            "readback=launch app=notepad hwnd=0x{:x} title={:?}",
            notepad_hwnd, notepad_fg.window_title
        );

        // 2) Launch mspaint to STEAL the foreground away from Notepad (stands in
        //    for the human / another agent changing focus).
        let mut paint = Command::new("mspaint.exe").spawn()?;
        let _paint_fg = wait_for_foreground_process("mspaint.exe")
            .ok_or_else(|| anyhow::anyhow!("mspaint did not reach the foreground"))?;
        let before_fg = synapse_a11y::current_foreground_context()?;
        assert_ne!(
            before_fg.hwnd, notepad_hwnd,
            "precondition: Notepad must NOT be the foreground window"
        );
        println!(
            "readback=foreground_before_observe hwnd=0x{:x} process={}",
            before_fg.hwnd, before_fg.process_name
        );

        // 3) Observe the BACKGROUND Notepad via the per-session target path.
        let state = M1State::default();
        let params = ObserveParams {
            window_hwnd: Some(notepad_hwnd),
            ..ObserveParams::default()
        };
        let observation = observe_input(&state, &params, None)?;
        println!(
            "readback=observation foreground_hwnd=0x{:x} process={} title={:?}",
            observation.foreground.hwnd,
            observation.foreground.process_name,
            observation.foreground.window_title
        );

        // Source of truth: the observation describes the TARGET (Notepad), not
        // the OS foreground (mspaint).
        assert_eq!(
            observation.foreground.hwnd, notepad_hwnd,
            "observation must describe the target window, not the foreground"
        );
        assert!(
            observation
                .foreground
                .process_name
                .eq_ignore_ascii_case("notepad.exe"),
            "observed process should be notepad.exe, got {}",
            observation.foreground.process_name
        );

        // 4) observe did NOT steal the foreground (no SetForegroundWindow on the
        //    perception path).
        let after_fg = synapse_a11y::current_foreground_context()?;
        println!(
            "readback=foreground_after_observe hwnd=0x{:x} process={}",
            after_fg.hwnd, after_fg.process_name
        );
        assert_eq!(
            after_fg.hwnd, before_fg.hwnd,
            "observe must NOT change the foreground window"
        );

        // 5) Edge case: close the target, then observing it must fail loud
        //    instead of silently reverting to the foreground.
        notepad.kill().ok();
        sleep(Duration::from_millis(750));
        let after_close = observe_input(&state, &params, None);
        println!(
            "readback=observe_after_target_closed is_err={}",
            after_close.is_err()
        );
        assert!(
            after_close.is_err(),
            "observing a closed target window must error, not silently fall back to foreground"
        );

        paint.kill().ok();
        Ok(())
    }

    #[test]
    fn capture_interval_floor_applies_to_manual_and_profile_metadata() {
        let config = CaptureConfig {
            min_update_interval_ms: 1,
            ..CaptureConfig::default()
        };
        let manual = observation_capture_from_capture_config(&config, 42, "manual-test".to_owned());
        assert_eq!(
            manual.min_update_interval_ms,
            MIN_CAPTURE_UPDATE_INTERVAL_MS_U32
        );

        let profile = ProfileCapture {
            target: ProfileCaptureTarget::PrimaryMonitor,
            min_update_interval_ms: 1,
            cursor_visible: true,
        };
        let from_profile =
            observation_capture_from_profile_capture(&profile, true, 43, "profile:test".to_owned());
        assert_eq!(
            from_profile.min_update_interval_ms,
            MIN_CAPTURE_UPDATE_INTERVAL_MS_U32
        );
    }

    #[test]
    fn inactive_capture_runtime_readback_reports_controller_state() {
        let mut state = M1State::default();
        state.capture_config.min_update_interval_ms = 1;

        let readback = state.capture_runtime_readback();

        assert_eq!(readback.status, "inactive");
        assert!(readback.target.is_none());
        assert!(readback.backend.is_none());
        assert_eq!(readback.generation, 0);
        assert_eq!(
            readback.min_update_interval_ms,
            Some(MIN_CAPTURE_UPDATE_INTERVAL_MS_U32)
        );
        assert_eq!(readback.frames_captured, 0);
        assert_eq!(readback.frames_dropped, 0);
        assert_eq!(readback.channel_len, 0);
        assert_eq!(readback.channel_capacity, CAPTURE_CHANNEL_CAPACITY);
        assert!(!readback.stop_requested);
    }

    #[test]
    fn element_window_rect_validation_requires_non_empty_bounds() {
        let element_id = ElementId::parse("0x1:00000001").expect("valid element id");
        let positive = Rect {
            x: 10,
            y: 20,
            w: 1,
            h: 1,
        };
        assert!(validate_element_window_rect(&element_id, positive).is_ok());

        for rect in [
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 10,
            },
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 0,
            },
            Rect {
                x: 0,
                y: 0,
                w: -1,
                h: 10,
            },
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: -1,
            },
        ] {
            let error = validate_element_window_rect(&element_id, rect)
                .expect_err("empty element_window bounds must fail closed");
            assert!(error.message.contains("non-empty UI rectangle"));
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::CAPTURE_TARGET_INVALID))
            );
        }
    }

    #[test]
    fn manual_perception_mode_survives_profile_runtime_apply() {
        let mut state = M1State::default();
        set_perception_mode_in_state(
            &mut state,
            &SetPerceptionModeParams {
                mode: "pixel_only".to_owned(),
            },
        )
        .expect("manual mode parses");

        apply_profile_runtime_config_in_state(
            &mut state,
            &profile_with_mode(PerceptionMode::Hybrid),
        )
        .expect("profile config applies");

        assert_eq!(state.perception_mode, PerceptionMode::PixelOnly);
        assert_eq!(
            state.manual_perception_mode,
            Some(PerceptionMode::PixelOnly)
        );
    }

    #[test]
    fn auto_perception_mode_releases_profile_runtime_apply() {
        let mut state = M1State::default();
        set_perception_mode_in_state(
            &mut state,
            &SetPerceptionModeParams {
                mode: "pixel_only".to_owned(),
            },
        )
        .expect("manual mode parses");
        set_perception_mode_in_state(
            &mut state,
            &SetPerceptionModeParams {
                mode: "auto".to_owned(),
            },
        )
        .expect("auto mode parses");

        apply_profile_runtime_config_in_state(
            &mut state,
            &profile_with_mode(PerceptionMode::Hybrid),
        )
        .expect("profile config applies");

        assert_eq!(state.perception_mode, PerceptionMode::Hybrid);
        assert_eq!(state.manual_perception_mode, None);
    }

    #[test]
    fn read_text_resolves_focused_region_when_target_is_omitted() {
        let state = M1State {
            synthetic: Some(synthetic_notepad_input()),
            ..Default::default()
        };
        let focused = state
            .synthetic
            .as_ref()
            .and_then(|input| input.focused.as_ref())
            .expect("synthetic fixture has focused element")
            .bbox;

        let request = resolve_read_text_request(
            &state,
            &ReadTextParams {
                backend: OcrBackend::Auto,
                lang_hint: Some(" en-US ".to_owned()),
                ..ReadTextParams::default()
            },
            None,
        )
        .expect("focused fallback should resolve");

        assert_eq!(request.region, focused);
        assert_eq!(request.requested_backend, OcrBackend::Auto);
        assert_eq!(request.effective_backend, OcrBackend::Winrt);
        assert_eq!(request.lang(), "en-US");
        assert!(request.synthetic);
    }

    #[test]
    fn read_text_crnn_backend_fails_closed_until_provider_is_wired() {
        let state = M1State {
            synthetic: Some(synthetic_notepad_input()),
            ..Default::default()
        };

        let error = resolve_read_text_request(
            &state,
            &ReadTextParams {
                region: Some(Rect {
                    x: 1,
                    y: 2,
                    w: 80,
                    h: 24,
                }),
                backend: OcrBackend::Crnn,
                ..ReadTextParams::default()
            },
            None,
        )
        .expect_err("unwired CRNN backend must not silently fall through");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::OCR_BACKEND_UNAVAILABLE))
        );
        assert!(error.message.contains("CRNN OCR backend"));
    }

    #[test]
    fn read_text_rejects_zero_sized_regions_before_ocr() {
        let state = M1State {
            synthetic: Some(synthetic_notepad_input()),
            ..Default::default()
        };

        for region in [
            Rect {
                x: 1,
                y: 2,
                w: 0,
                h: 24,
            },
            Rect {
                x: 1,
                y: 2,
                w: 80,
                h: 0,
            },
            Rect {
                x: 1,
                y: 2,
                w: -1,
                h: 24,
            },
            Rect {
                x: 1,
                y: 2,
                w: 80,
                h: -1,
            },
        ] {
            let error = resolve_read_text_request(
                &state,
                &ReadTextParams {
                    region: Some(region),
                    backend: OcrBackend::Winrt,
                    ..ReadTextParams::default()
                },
                None,
            )
            .expect_err("empty OCR regions must fail closed");
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::OCR_NO_TEXT))
            );
        }
    }

    #[test]
    fn browser_ocr_words_upgrade_uia_only_to_queryable_ocr_nodes() {
        let mut input = chromium_ocr_input();
        let before_path = input.web_path;
        let before_len = input.elements.len();

        let added = apply_browser_ocr_words(
            &mut input,
            vec![
                TextRegion {
                    text: " Checkout ".to_owned(),
                    bbox: Rect {
                        x: 120,
                        y: 180,
                        w: 92,
                        h: 24,
                    },
                    confidence: 0.95,
                },
                TextRegion {
                    text: "now".to_owned(),
                    bbox: Rect {
                        x: 218,
                        y: 180,
                        w: 44,
                        h: 24,
                    },
                    confidence: 0.93,
                },
            ],
            8,
        );

        println!(
            "readback=browser_ocr edge=happy before_path:{before_path:?} before_elements:{before_len} after_path:{:?} after_elements:{} added:{added}",
            input.web_path,
            input.elements.len()
        );
        assert_eq!(added, 2);
        assert_eq!(input.web_path, Some(WebPerceptionPath::Ocr));
        assert_eq!(input.elements[0].name, "Checkout");
        assert_eq!(input.elements[0].role, "text");
        assert_eq!(
            input.elements[0].automation_id.as_deref(),
            Some("ocr:word:0")
        );
        assert!(
            input.elements[0]
                .element_id
                .parts()
                .expect("OCR element id parses")
                .runtime_id_hex
                .starts_with("0c0c")
        );
        assert_eq!(
            browser_ocr_rect_from_element_id(&input.elements[0].element_id),
            Some(Rect {
                x: 120,
                y: 180,
                w: 92,
                h: 24,
            })
        );
    }

    #[test]
    fn browser_ocr_words_keep_uia_only_when_ocr_has_no_usable_text() {
        let mut input = chromium_ocr_input();

        let added = apply_browser_ocr_words(
            &mut input,
            vec![
                TextRegion {
                    text: "   ".to_owned(),
                    bbox: Rect {
                        x: 1,
                        y: 2,
                        w: 30,
                        h: 12,
                    },
                    confidence: 0.5,
                },
                TextRegion {
                    text: "Hidden".to_owned(),
                    bbox: Rect {
                        x: 1,
                        y: 2,
                        w: 0,
                        h: 12,
                    },
                    confidence: 0.5,
                },
            ],
            8,
        );

        println!(
            "readback=browser_ocr edge=empty after_path:{:?} after_elements:{} added:{added}",
            input.web_path,
            input.elements.len()
        );
        assert_eq!(added, 0);
        assert_eq!(input.web_path, Some(WebPerceptionPath::UiaOnly));
        assert!(input.elements.is_empty());
    }

    #[test]
    fn browser_ocr_guard_only_allows_cdp_failures_on_uia_only_path() {
        let mut input = chromium_ocr_input();
        assert!(should_attempt_browser_ocr(&input));

        input.cdp = Some(CdpDiagnostics {
            process_name: "chrome.exe".to_owned(),
            status: CdpStatus::Ok,
            endpoint: Some("http://127.0.0.1:9222".to_owned()),
            checked_ports: vec![9222],
            checked_endpoints: vec!["http://127.0.0.1:9222".to_owned()],
            reason_code: None,
            detail: None,
            capabilities: Vec::new(),
            attached_node_count: None,
            selected_target_id: None,
            selected_session_id: None,
            target_selection_reason: None,
            target_candidate_count: None,
            frame_tree_frame_count: None,
            attached_frame_target_count: None,
            blocked_frame_targets: Vec::new(),
            frame_snapshot_errors: Vec::new(),
        });
        assert!(!should_attempt_browser_ocr(&input));

        input.cdp = Some(CdpDiagnostics::unreachable(
            "chrome.exe",
            error_codes::A11Y_CDP_UNREACHABLE,
        ));
        input.web_path = Some(WebPerceptionPath::Cdp);
        assert!(!should_attempt_browser_ocr(&input));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cdp_enrichment_skips_deprecated_chrome_debugger_extension_snapshot() {
        let mut input = chromium_ocr_input();
        let before_element_count = input.elements.len();

        enrich_input_with_cdp_for_target(&mut input, 6, 160, Some("chrome-tab:test")).await;

        let diagnostics = input.cdp.as_ref().expect("cdp diagnostics");
        println!(
            "readback=cdp_enrich edge=normal_chrome_no_raw_cdp status:{} reason:{:?} detail:{:?} before_elements:{} after_elements:{}",
            diagnostics.status.as_str(),
            diagnostics.reason_code,
            diagnostics.detail,
            before_element_count,
            input.elements.len()
        );
        assert_eq!(diagnostics.status, CdpStatus::ExtensionUnavailable);
        assert_eq!(
            diagnostics.reason_code.as_deref(),
            Some(error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE)
        );
        assert!(
            diagnostics
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("disabled"))
        );
        assert!(diagnostics.detail.as_deref().is_none_or(|detail| {
            !detail.contains(error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED)
        }));
        assert_eq!(input.elements.len(), before_element_count);
    }

    #[test]
    fn browser_ocr_skips_when_main_pane_uia_content_is_present() {
        let mut input = chromium_ocr_input();
        input.elements.push(chromium_uia_node(
            "Force Renderer Complete Button",
            "button",
            Rect {
                x: 420,
                y: 180,
                w: 320,
                h: 34,
            },
            "0000002a00000042",
        ));

        println!(
            "readback=browser_ocr edge=main_pane_uia after_has_content:{} after_should_full_ocr:{} after_should_overlay_ocr:{}",
            has_chromium_main_pane_uia_content(&input),
            should_attempt_browser_ocr(&input),
            should_attempt_browser_overlay_ocr(&input)
        );
        assert!(has_chromium_main_pane_uia_content(&input));
        assert!(!should_attempt_browser_ocr(&input));
        assert!(should_attempt_browser_overlay_ocr(&input));
    }

    #[test]
    fn browser_overlay_ocr_accepts_new_visible_text_missing_from_uia() {
        let mut input = chromium_ocr_input();
        input.elements.push(chromium_uia_node(
            "Timeline item loaded",
            "text",
            Rect {
                x: 440,
                y: 280,
                w: 360,
                h: 36,
            },
            "0000002a00000044",
        ));
        let probe_region = browser_overlay_probe_region(
            browser_content_region(input.foreground.window_bounds)
                .expect("test browser content region exists"),
        )
        .expect("test overlay probe region exists");

        let gap = browser_overlay_ocr_gap(
            &input,
            vec![
                ocr_word("Compose", 520, 340),
                ocr_word("hidden", 520, 388),
                ocr_word("modal", 618, 388),
                ocr_word("draft", 702, 388),
                ocr_word("Post", 520, 454),
                ocr_word("Cancel", 610, 454),
            ],
            probe_region,
        );

        println!(
            "readback=browser_overlay_ocr edge=missing_modal before_path:{:?} probe:{probe_region:?} cluster:{:?} new_tokens:{} action_tokens:{} attach:{}",
            input.web_path,
            gap.cluster_region,
            gap.new_token_count,
            gap.new_action_token_count,
            browser_overlay_ocr_gap_is_actionable(
                gap.new_token_count,
                gap.new_action_token_count,
                gap.cluster_region,
                probe_region
            )
        );
        assert!(should_attempt_browser_overlay_ocr(&input));
        assert_eq!(gap.new_token_count, 6);
        assert_eq!(gap.new_action_token_count, 3);
        assert_eq!(
            gap.cluster_region,
            Some(Rect {
                x: 520,
                y: 340,
                w: 260,
                h: 140,
            })
        );
        assert!(browser_overlay_ocr_gap_is_actionable(
            gap.new_token_count,
            gap.new_action_token_count,
            gap.cluster_region,
            probe_region
        ));

        let added = apply_browser_ocr_words(&mut input, gap.words, 8);
        assert_eq!(added, 6);
        assert_eq!(input.web_path, Some(WebPerceptionPath::Ocr));
        assert!(input.elements.iter().any(|node| node.name == "Post"));
        assert!(input.elements.iter().any(|node| {
            node.automation_id
                .as_deref()
                .is_some_and(|automation_id| automation_id.starts_with("ocr:word:"))
        }));
    }

    #[test]
    fn browser_overlay_ocr_rejects_text_already_exposed_by_uia() {
        let input = {
            let mut input = chromium_ocr_input();
            input.elements.push(chromium_uia_node(
                "Compose hidden modal draft Post Cancel",
                "text",
                Rect {
                    x: 440,
                    y: 280,
                    w: 560,
                    h: 160,
                },
                "0000002a00000045",
            ));
            input
        };
        let probe_region = browser_overlay_probe_region(
            browser_content_region(input.foreground.window_bounds)
                .expect("test browser content region exists"),
        )
        .expect("test overlay probe region exists");

        let gap = browser_overlay_ocr_gap(
            &input,
            vec![
                ocr_word("Compose", 520, 340),
                ocr_word("hidden", 520, 388),
                ocr_word("modal", 618, 388),
                ocr_word("draft", 702, 388),
                ocr_word("Post", 520, 454),
                ocr_word("Cancel", 610, 454),
            ],
            probe_region,
        );

        println!(
            "readback=browser_overlay_ocr edge=already_exposed probe:{probe_region:?} cluster:{:?} new_tokens:{} action_tokens:{} attach:{}",
            gap.cluster_region,
            gap.new_token_count,
            gap.new_action_token_count,
            browser_overlay_ocr_gap_is_actionable(
                gap.new_token_count,
                gap.new_action_token_count,
                gap.cluster_region,
                probe_region
            )
        );
        assert!(should_attempt_browser_overlay_ocr(&input));
        assert_eq!(gap.new_token_count, 0);
        assert_eq!(gap.new_action_token_count, 0);
        assert_eq!(gap.cluster_region, None);
        assert!(gap.words.is_empty());
        assert!(!browser_overlay_ocr_gap_is_actionable(
            gap.new_token_count,
            gap.new_action_token_count,
            gap.cluster_region,
            probe_region
        ));
    }

    #[test]
    fn browser_overlay_ocr_rejects_scattered_page_noise_without_modal_actions() {
        let mut input = chromium_ocr_input();
        input.elements.push(chromium_uia_node(
            "Share Mode starts after workspace ready",
            "text",
            Rect {
                x: 420,
                y: 260,
                w: 520,
                h: 42,
            },
            "0000002a00000046",
        ));
        let probe_region = browser_overlay_probe_region(
            browser_content_region(input.foreground.window_bounds)
                .expect("test browser content region exists"),
        )
        .expect("test overlay probe region exists");

        let gap = browser_overlay_ocr_gap(
            &input,
            vec![
                ocr_word("Start", 910, 320),
                ocr_word("Turn", 880, 460),
                ocr_word("month", 930, 660),
                ocr_word("queue", 540, 650),
                ocr_word("private", 500, 330),
            ],
            probe_region,
        );

        println!(
            "readback=browser_overlay_ocr edge=scattered_noise probe:{probe_region:?} cluster:{:?} new_tokens:{} action_tokens:{} attach:{}",
            gap.cluster_region,
            gap.new_token_count,
            gap.new_action_token_count,
            browser_overlay_ocr_gap_is_actionable(
                gap.new_token_count,
                gap.new_action_token_count,
                gap.cluster_region,
                probe_region
            )
        );
        assert_eq!(gap.new_action_token_count, 0);
        assert!(!browser_overlay_ocr_gap_is_actionable(
            gap.new_token_count,
            gap.new_action_token_count,
            gap.cluster_region,
            probe_region
        ));
    }

    #[test]
    fn browser_overlay_ocr_anchors_on_action_cluster_when_probe_includes_page_noise() {
        let mut input = chromium_ocr_input();
        input.foreground.window_bounds = Rect {
            x: 1974,
            y: 29,
            w: 2976,
            h: 1936,
        };
        input.elements.extend([
            chromium_uia_node(
                "Main timeline control surface",
                "heading",
                Rect {
                    x: 2658,
                    y: 421,
                    w: 1294,
                    h: 59,
                },
                "0000002a00000047",
            ),
            chromium_uia_node(
                "Refresh timeline",
                "button",
                Rect {
                    x: 2658,
                    y: 609,
                    w: 261,
                    h: 66,
                },
                "0000002a00000048",
            ),
            chromium_uia_node(
                "Underlying state ready for OCR overlay verification.",
                "text",
                Rect {
                    x: 2680,
                    y: 718,
                    w: 684,
                    h: 33,
                },
                "0000002a00000049",
            ),
            chromium_uia_node(
                "Open settings",
                "link",
                Rect {
                    x: 2658,
                    y: 795,
                    w: 230,
                    h: 66,
                },
                "0000002a0000004a",
            ),
        ]);
        let probe_region = browser_overlay_probe_region(
            browser_content_region(input.foreground.window_bounds)
                .expect("observed browser content region exists"),
        )
        .expect("observed overlay probe region exists");

        let gap = browser_overlay_ocr_gap(
            &input,
            vec![
                ocr_sized_word("fresh", 2719, 631, 64, 22),
                ocr_sized_word("timeline", 2794, 631, 102, 22),
                ocr_sized_word("derlying", 2719, 724, 103, 28),
                ocr_sized_word("state", 2833, 725, 63, 21),
                ocr_sized_word("ready", 2907, 724, 73, 28),
                ocr_sized_word("for", 2989, 724, 35, 22),
                ocr_sized_word("OCR", 3033, 724, 65, 22),
                ocr_sized_word("overlay", 3108, 724, 95, 28),
                ocr_sized_word("verification.", 3212, 724, 149, 22),
                ocr_sized_word("en", 2718, 823, 34, 16),
                ocr_sized_word("settings", 2763, 817, 102, 28),
                ocr_sized_word("Compose", 3028, 856, 322, 67),
                ocr_sized_word("Visible", 3025, 980, 147, 37),
                ocr_sized_word("Modal", 3192, 980, 132, 37),
                ocr_sized_word("Alpha", 3341, 980, 129, 47),
                ocr_sized_word("Top", 3025, 1058, 80, 47),
                ocr_sized_word("layer", 3124, 1058, 108, 47),
                ocr_sized_word("pixels", 3249, 1058, 125, 47),
                ocr_sized_word("only", 3392, 1058, 92, 47),
                ocr_sized_word("Post", 3073, 1242, 107, 37),
                ocr_sized_word("Cancel", 3425, 1241, 163, 38),
            ],
            probe_region,
        );

        println!(
            "readback=browser_overlay_ocr edge=action_anchor_noise probe:{probe_region:?} cluster:{:?} new_tokens:{} action_tokens:{} attach:{}",
            gap.cluster_region,
            gap.new_token_count,
            gap.new_action_token_count,
            browser_overlay_ocr_gap_is_actionable(
                gap.new_token_count,
                gap.new_action_token_count,
                gap.cluster_region,
                probe_region
            )
        );
        assert_eq!(
            probe_region,
            Rect {
                x: 2718,
                y: 585,
                w: 1488,
                h: 920,
            }
        );
        assert_eq!(gap.new_action_token_count, 3);
        assert!(gap.words.iter().any(|word| word.text == "Compose"));
        assert!(gap.words.iter().any(|word| word.text == "Post"));
        assert!(gap.words.iter().any(|word| word.text == "Cancel"));
        assert!(!gap.words.iter().any(|word| word.text == "fresh"));
        assert!(!gap.words.iter().any(|word| word.text == "derlying"));
        assert!(browser_overlay_ocr_gap_is_actionable(
            gap.new_token_count,
            gap.new_action_token_count,
            gap.cluster_region,
            probe_region
        ));
    }

    #[test]
    fn browser_ocr_runs_when_only_sidebar_uia_content_is_present() {
        let mut input = chromium_ocr_input();
        input.elements.push(AccessibleNode {
            element_id: synapse_core::element_id(0x2200, "0000002a00000043"),
            parent: None,
            name: "Navigation Item".to_owned(),
            role: "link".to_owned(),
            automation_id: Some("sidebar-link".to_owned()),
            value: None,
            bbox: Rect {
                x: 40,
                y: 180,
                w: 220,
                h: 34,
            },
            enabled: true,
            focused: false,
            patterns: vec![synapse_core::UiaPattern::Invoke],
            children_count: 0,
            depth: 2,
        });

        println!(
            "readback=browser_ocr edge=sidebar_only after_has_content:{} after_should_ocr:{}",
            has_chromium_main_pane_uia_content(&input),
            should_attempt_browser_ocr(&input)
        );
        assert!(!has_chromium_main_pane_uia_content(&input));
        assert!(should_attempt_browser_ocr(&input));
    }

    #[test]
    fn browser_content_tiles_skip_chrome_band_and_bound_tile_count() {
        let content = browser_content_region(Rect {
            x: 10,
            y: 20,
            w: 1200,
            h: 1600,
        })
        .expect("large browser window has content region");
        let tiles = browser_ocr_tiles(content);

        println!(
            "readback=browser_ocr edge=tiles content:{content:?} tile_count:{} first:{:?} last:{:?}",
            tiles.len(),
            tiles.first(),
            tiles.last()
        );
        assert_eq!(content.y, 116);
        assert_eq!(content.h, 1504);
        assert_eq!(tiles.len(), 3);
        assert_eq!(tiles[0].h, BROWSER_OCR_TILE_HEIGHT_PX);
        assert_eq!(tiles[2].h, 304);
        assert!(
            browser_content_region(Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 0,
            })
            .is_none()
        );
    }

    fn chromium_ocr_input() -> ObservationInput {
        let mut input = ObservationInput::new(ForegroundContext {
            hwnd: 0x2200,
            pid: 7777,
            process_name: "chrome.exe".to_owned(),
            process_path: "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe".to_owned(),
            window_title: "Example - Google Chrome".to_owned(),
            window_bounds: Rect {
                x: 0,
                y: 0,
                w: 1280,
                h: 900,
            },
            monitor_index: 0,
            dpi_scale: 1.0,
            profile_id: Some("chrome".to_owned()),
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        });
        input.capture_status = SensorStatus::Healthy;
        input.cdp = Some(CdpDiagnostics::unreachable(
            "chrome.exe",
            error_codes::A11Y_CDP_UNREACHABLE,
        ));
        input.web_path = Some(WebPerceptionPath::UiaOnly);
        input
    }

    fn chromium_uia_node(name: &str, role: &str, bbox: Rect, runtime_id: &str) -> AccessibleNode {
        AccessibleNode {
            element_id: synapse_core::element_id(0x2200, runtime_id),
            parent: None,
            name: name.to_owned(),
            role: role.to_owned(),
            automation_id: Some(format!("test-{runtime_id}")),
            value: None,
            bbox,
            enabled: true,
            focused: false,
            patterns: vec![synapse_core::UiaPattern::Invoke],
            children_count: 0,
            depth: 2,
        }
    }

    fn ocr_word(text: &str, x: i32, y: i32) -> TextRegion {
        ocr_sized_word(text, x, y, 78, 26)
    }

    fn ocr_sized_word(text: &str, x: i32, y: i32, w: i32, h: i32) -> TextRegion {
        TextRegion {
            text: text.to_owned(),
            bbox: Rect { x, y, w, h },
            confidence: 0.95,
        }
    }

    fn profile_with_mode(mode: PerceptionMode) -> Profile {
        Profile {
            id: "test-profile".to_owned(),
            label: "Test Profile".to_owned(),
            version: "2".to_owned(),
            use_scope: ProfileUseScope::OperatorOwnedTest,
            matches: vec![ProfileMatch {
                exe: Some("test.exe".to_owned()),
                title_regex: None,
                steam_appid: None,
                window_class: None,
                process_args: Vec::new(),
            }],
            mode,
            capture: ProfileCapture {
                target: ProfileCaptureTarget::ForegroundWindow,
                min_update_interval_ms: 50,
                cursor_visible: true,
            },
            detection: ProfileDetection {
                model_id: None,
                classes_of_interest: Vec::new(),
                confidence_threshold: 0.5,
                max_detections: 32,
            },
            ocr: ProfileOcr {
                default_backend: OcrBackend::Auto,
                regions: Vec::new(),
                parser_config: BTreeMap::new(),
            },
            hud: Vec::new(),
            keymap: BTreeMap::new(),
            backends: ProfileBackends {
                default: Backend::Auto,
                keyboard_default: Backend::Auto,
                mouse_default: Backend::Auto,
                pad_default: Backend::Auto,
            },
            metadata: BTreeMap::new(),
            event_extensions: Vec::new(),
        }
    }
}
