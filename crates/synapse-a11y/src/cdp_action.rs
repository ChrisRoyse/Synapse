//! CDP-routed actions on web DOM nodes (#686).
//!
//! When an action targets a web node (an element id carrying the
//! [`crate::CDP_RUNTIME_PREFIX`] sentinel), the action layer routes it here
//! instead of UIA/`SendInput`. We attach CDP, locate the page that owns the
//! node, scroll it into view, resolve its live box model, and dispatch via
//! `Input.dispatchMouseEvent` / `Input.dispatchTouchEvent` / `Input.insertText` in **viewport CSS
//! coordinates** — which sidesteps the DPI / scroll / window-content-origin
//! mapping that screen-coordinate clicking would need, and works regardless of
//! the node's initial scroll position.
//!
//! Everything here is `cfg(windows)` because it depends on `chromiumoxide`.

#![cfg(windows)]

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, GetBoxModelParams, ResolveNodeParams, ScrollIntoViewIfNeededParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    DispatchTouchEventParams, DispatchTouchEventType, InsertTextParams, MouseButton, TouchPoint,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
    EventRequestWillBeSent, RequestId,
};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CaptureScreenshotFormat,
    EnableParams as PageEnableParams, EventDomContentEventFired, EventFrameNavigated,
    EventLifecycleEvent, EventLoadEventFired, EventNavigatedWithinDocument, GetLayoutMetricsParams,
    GetNavigationHistoryParams, NavigateParams, NavigateToHistoryEntryParams, ReloadParams,
    RemoveScriptToEvaluateOnNewDocumentParams, ScriptIdentifier, SetDocumentContentParams,
    SetLifecycleEventsEnabledParams, Viewport,
};
use chromiumoxide::cdp::browser_protocol::target::TargetId;
use chromiumoxide::cdp::js_protocol::runtime::{CallArgument, CallFunctionOnParams};
use chromiumoxide::page::ScreenshotParams;
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::{A11yError, A11yResult, cdp_dom::rect_from_quad};

const CDP_INPUT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Where a CDP action landed, in viewport CSS coordinates (diagnostics).
#[derive(Copy, Clone, Debug, PartialEq, Serialize)]
pub struct CdpActionPoint {
    pub x: f64,
    pub y: f64,
}

/// One point in a CDP mouse stroke, in viewport CSS coordinates.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CdpMouseStrokePoint {
    pub x: f64,
    pub y: f64,
    pub elapsed_ms: f64,
}

/// Dispatch summary for a CDP mouse stroke.
#[derive(Clone, Debug, PartialEq)]
pub struct CdpMouseStrokeResult {
    pub target_id: String,
    pub point_count: usize,
    pub start: CdpActionPoint,
    pub end: CdpActionPoint,
    pub duration_ms: f64,
}

/// Dispatch summary for a CDP touch tap.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpTouchTapResult {
    pub target_id: String,
    pub point: CdpActionPoint,
    pub dispatched_events: Vec<String>,
    pub max_touch_points: i64,
    pub ontouchstart_available: bool,
    pub touch_emulation_detected: bool,
    pub non_touch_fallback: String,
}

/// One CDP wheel event in viewport CSS pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CdpWheelDelta {
    pub delta_x: f64,
    pub delta_y: f64,
}

/// One key descriptor for `Input.dispatchKeyEvent`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdpKeyStroke {
    pub key: String,
    pub code: String,
    pub windows_virtual_key_code: i64,
    pub native_virtual_key_code: i64,
    pub key_identifier: Option<String>,
    pub text: Option<String>,
    pub unmodified_text: Option<String>,
    pub modifier_bit: i64,
    pub location: Option<i64>,
}

/// Scroll source-of-truth read from the target node's DOM context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpScrollState {
    pub is_connected: bool,
    pub window_scroll_x: f64,
    pub window_scroll_y: f64,
    pub target_scroll_left: f64,
    pub target_scroll_top: f64,
    pub target_scroll_width: f64,
    pub target_scroll_height: f64,
    pub target_client_width: f64,
    pub target_client_height: f64,
    pub target_tag: String,
    pub target_id: String,
    pub node_rect_left: f64,
    pub node_rect_top: f64,
    pub node_rect_width: f64,
    pub node_rect_height: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpScrollIntoViewRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpScrollIntoViewContainer {
    pub is_root: bool,
    pub tag_name: String,
    pub id: String,
    pub scroll_left: f64,
    pub scroll_top: f64,
    pub scroll_width: f64,
    pub scroll_height: f64,
    pub client_width: f64,
    pub client_height: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpScrollIntoViewSnapshot {
    pub is_connected: bool,
    pub viewport_width: f64,
    pub viewport_height: f64,
    pub node_rect: CdpScrollIntoViewRect,
    pub node_fully_in_viewport: bool,
    pub window_scroll_x: f64,
    pub window_scroll_y: f64,
    pub container: CdpScrollIntoViewContainer,
    pub box_model_content: Option<CdpScrollIntoViewRect>,
    pub box_model_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpScrollIntoViewResult {
    pub target_id: String,
    pub backend_node_id: i64,
    pub before: CdpScrollIntoViewSnapshot,
    pub after: CdpScrollIntoViewSnapshot,
    pub window_scroll_changed: bool,
    pub container_scroll_changed: bool,
    pub node_fully_in_viewport_after: bool,
}

/// Active-element Source-of-Truth read from a CDP page target.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActiveElementState {
    pub target_id: String,
    pub has_active_element: bool,
    pub is_editable: bool,
    pub tag_name: String,
    pub id: String,
    pub name: String,
    pub value: String,
    pub selection_start: Option<u32>,
    pub selection_end: Option<u32>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CdpPageNavigationAction {
    Navigate,
    Reload,
    Back,
    Forward,
}

impl CdpPageNavigationAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Reload => "reload",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpPageState {
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub history_current_index: i64,
    pub history_entry_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum CdpLoadState {
    DomContentLoaded,
    Load,
    NetworkIdle,
}

impl CdpLoadState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DomContentLoaded => "domcontentloaded",
            Self::Load => "load",
            Self::NetworkIdle => "networkidle",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpLoadStateWaitResult {
    pub target_id: String,
    pub requested_state: String,
    pub observed_state: String,
    pub elapsed_ms: u64,
    pub event_count: u64,
    pub network_event_count: u64,
    pub max_in_flight_requests: usize,
    pub in_flight_requests: usize,
    pub network_idle_quiet_ms: u64,
    pub lifecycle_network_idle_seen: bool,
    pub url: String,
    pub title: String,
    pub ready_state: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum CdpUrlMatchKind {
    Exact,
    Glob,
    Regex,
}

impl CdpUrlMatchKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Glob => "glob",
            Self::Regex => "regex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpUrlWaitResult {
    pub target_id: String,
    pub pattern: String,
    pub match_kind: String,
    pub matched_url: String,
    pub elapsed_ms: u64,
    pub poll_count: u64,
    pub navigation_event_count: u64,
    pub url: String,
    pub title: String,
    pub ready_state: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpPageTextState {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub text: String,
    pub text_len: usize,
    pub text_truncated: bool,
    pub max_chars: usize,
}

/// Result of evaluating a JavaScript expression in a CDP page target via
/// `Runtime.evaluate` (#1065/#1067). Background-safe: this never activates the
/// tab or uses OS foreground input — it attaches CDP to the owned target only.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpEvaluateResult {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    /// `Runtime.RemoteObject.type` (e.g. "object", "string", "number",
    /// "boolean", "undefined", "function", "symbol", "bigint").
    pub result_type: String,
    /// `Runtime.RemoteObject.subtype` when present (e.g. "array", "null",
    /// "node", "error", "promise", "date", "regexp").
    pub result_subtype: Option<String>,
    /// True when the value was serialized by value (JSON round-trippable).
    pub returned_by_value: bool,
    /// The serialized JSON value when `returnByValue` produced one; JSON `null`
    /// otherwise (inspect `result_type`/`description` for non-serializable
    /// handles such as DOM nodes or functions).
    pub value: Value,
    /// `Runtime.RemoteObject.description` (the engine's string rendering),
    /// useful for non-by-value handles where `value` is `null`.
    pub description: Option<String>,
    /// `Runtime.RemoteObject.unserializableValue` (e.g. "Infinity", "NaN",
    /// "-0", bigint literals) when the value cannot be represented as JSON.
    pub unserializable_value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpInitScriptResult {
    pub target_id: String,
    pub identifier: String,
    pub state: CdpPageState,
}

/// Selector resolution engine for [`cdp_locate`] (#1110–#1119), giving Synapse
/// the full Playwright locator surface (CSS / XPath / text / role / label /
/// placeholder / altText / title / testid / layout).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CdpLocateEngine {
    /// `DOM.querySelectorAll` semantics, shadow-piercing (`getBy`-free CSS).
    #[default]
    Css,
    /// `document.evaluate` XPath (Playwright `xpath=`).
    Xpath,
    /// Visible text (`getByText`): normalized whitespace, substring/exact/regex.
    Text,
    /// ARIA role + accessible name + state (`getByRole`), via the live AX tree.
    Role,
    /// `getByLabel`: `aria-labelledby` / `aria-label` / wrapping/`for=` `<label>`.
    Label,
    /// `getByPlaceholder`: the `placeholder` attribute.
    Placeholder,
    /// `getByAltText`: the `alt` attribute.
    AltText,
    /// `getByTitle`: the `title` attribute.
    Title,
    /// `getByTestId`: a configurable attribute (default `data-testid`).
    TestId,
    /// Layout/relational (`:near` / `:right-of` / … ) ranked by box geometry.
    Layout,
}

impl CdpLocateEngine {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Css => "css",
            Self::Xpath => "xpath",
            Self::Text => "text",
            Self::Role => "role",
            Self::Label => "label",
            Self::Placeholder => "placeholder",
            Self::AltText => "alttext",
            Self::Title => "title",
            Self::TestId => "testid",
            Self::Layout => "layout",
        }
    }

    /// Engines resolved by the injected JavaScript selector engine (everything
    /// except `role`, which uses the native `Accessibility.queryAXTree`).
    const fn uses_injected_js(self) -> bool {
        !matches!(self, Self::Role)
    }
}

/// Direction for the `layout` engine (Playwright proximity pseudo-classes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CdpLayoutRelation {
    /// Within `max_distance` (default 50 CSS px) in any direction.
    #[default]
    Near,
    RightOf,
    LeftOf,
    Above,
    Below,
}

impl CdpLayoutRelation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Near => "near",
            Self::RightOf => "right-of",
            Self::LeftOf => "left-of",
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

/// A fully-specified selector resolution request (#1110). Built by the MCP layer
/// and consumed by [`cdp_locate`]. Field semantics mirror Playwright locators.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CdpLocateRequest {
    pub engine: CdpLocateEngine,
    /// Primary query: CSS/XPath text, visible-text, role token, label text,
    /// placeholder/alt/title text, test-id value, or (layout) the base CSS.
    pub query: String,
    /// Exact match (whitespace-normalized) vs the default case-insensitive
    /// substring, for text/label/placeholder/altText/title/testid.
    pub exact: bool,
    /// Interpret `query` as a JS regular expression body.
    pub regex: bool,
    /// `getByRole` accessible-name filter (role token stays in `query`).
    pub name: Option<String>,
    /// Exact accessible-name match for `role`.
    pub name_exact: bool,
    /// Interpret `name` as a regular expression for `role`.
    pub name_regex: bool,
    /// `getByTestId` attribute name (default `data-testid`).
    pub testid_attribute: Option<String>,
    /// ARIA state filters for `role` (`None` = unconstrained).
    pub checked: Option<bool>,
    pub pressed: Option<bool>,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    pub disabled: Option<bool>,
    /// `aria-level` (headings) exact match.
    pub level: Option<i64>,
    /// Include nodes ignored for accessibility (`getByRole` `includeHidden`).
    pub include_hidden: bool,
    /// Layout direction (required for `layout`).
    pub relation: Option<CdpLayoutRelation>,
    /// Layout anchor CSS selector (required for `layout`).
    pub anchor: Option<String>,
    /// Layout maximum CSS-pixel distance (default 50 for `near`).
    pub max_distance: Option<f64>,
    /// `.filter({ hasText })`: keep only matches whose normalized text contains
    /// this (case-insensitive). Applies to every JS-resolved engine.
    pub has_text: Option<String>,
    /// Positional pick (`.nth`/`.first`/`.last`): 0-based, negative counts from
    /// the end (-1 == last). Applied after `has_text`, before `limit`.
    pub nth: Option<i64>,
    /// Strict mode: error when more than one element matches (Playwright strict).
    pub strict: bool,
    /// Resolve only within this element (`backendNodeId`); chaining/scoping.
    pub root_backend_node_id: Option<i64>,
    /// Resolve in this Page.FrameId's execution context.
    pub frame_id: Option<String>,
    /// Maximum element ids to return. `match_count` always reports the true total.
    pub limit: usize,
}

/// Result of resolving a selector to live DOM nodes in a CDP page target
/// (#1110). `backend_node_ids` are the matched nodes (capped at the caller's
/// limit); `match_count` is the total number of matches before the cap.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpLocateResult {
    pub target_id: String,
    pub url: String,
    pub title: String,
    pub engine: String,
    pub query: String,
    pub frame_id: Option<String>,
    pub match_count: usize,
    pub backend_node_ids: Vec<i64>,
    pub returned_count: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpPageNavigationResult {
    pub target_id: String,
    pub action: String,
    pub requested_url: Option<String>,
    pub before: CdpPageState,
    pub after: CdpPageState,
    pub navigation_error_text: Option<String>,
    pub is_download: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CdpSetDocumentContentResult {
    pub target_id: String,
    pub frame_id: String,
    pub html_len: usize,
    pub before: CdpPageState,
    pub after: CdpPageState,
}

#[derive(Clone, Debug)]
enum CdpPageReadbackExpectation {
    Stable,
    UrlChanged { previous_url: String },
    HistoryEntry { current_index: i64, url: String },
}

impl CdpPageReadbackExpectation {
    fn matches(&self, state: &CdpPageState) -> bool {
        match self {
            Self::Stable => true,
            Self::UrlChanged { previous_url } => state.url != *previous_url,
            Self::HistoryEntry { current_index, url } => {
                state.history_current_index == *current_index && state.url == *url
            }
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Stable => "stable loaded page".to_owned(),
            Self::UrlChanged { previous_url } => {
                format!("url to change from {previous_url:?}")
            }
            Self::HistoryEntry { current_index, url } => {
                format!("historyIndex={current_index} and url={url:?}")
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct CdpRuntimeScrollDispatch {
    is_connected: bool,
    default_prevented: bool,
    target_scroll_left_before: f64,
    target_scroll_top_before: f64,
    target_scroll_left_after: f64,
    target_scroll_top_after: f64,
    target_tag: String,
    target_id: String,
}

/// Which pointer button a CDP click uses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CdpMouseButton {
    Left,
    Right,
    Middle,
}

impl CdpMouseButton {
    const fn to_cdp(self) -> MouseButton {
        match self {
            Self::Left => MouseButton::Left,
            Self::Right => MouseButton::Right,
            Self::Middle => MouseButton::Middle,
        }
    }
}

/// Clicks a web node `click_count` times with `button`, after scrolling it into
/// view. Returns the viewport point clicked.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/node cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if box-model resolution or dispatch fails.
pub async fn cdp_click_node(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    button: CdpMouseButton,
    click_count: i64,
    modifiers: i64,
) -> A11yResult<CdpActionPoint> {
    with_node_center(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page, center| async move {
            page.execute(mouse_event_with_modifiers(
                DispatchMouseEventType::MouseMoved,
                center,
                button.to_cdp(),
                0,
                modifiers,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            page.execute(mouse_event_with_modifiers(
                DispatchMouseEventType::MousePressed,
                center,
                button.to_cdp(),
                click_count.max(1),
                modifiers,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            page.execute(mouse_event_with_modifiers(
                DispatchMouseEventType::MouseReleased,
                center,
                button.to_cdp(),
                click_count.max(1),
                modifiers,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            Ok(center)
        },
    )
    .await
}

/// Touch-taps a web node with `Input.dispatchTouchEvent` (`touchStart` then
/// `touchEnd`) after scrolling it into view. Returns the viewport point tapped.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/node cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if box-model resolution or dispatch fails.
pub async fn cdp_touch_tap_node(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<CdpTouchTapResult> {
    with_node_center(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page, center| async move { dispatch_touch_tap_on_page(&page, center).await },
    )
    .await
}

/// Focuses a web input node and inserts `text` (as if typed).
///
/// # Errors
///
/// As [`cdp_click_node`].
pub async fn cdp_type_node(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    text: &str,
) -> A11yResult<()> {
    use chromiumoxide::cdp::browser_protocol::dom::FocusParams;

    let text = text.to_owned();
    with_node_center(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page, center| async move {
            // Click to place the caret, then focus and insert text.
            page.execute(mouse_event(
                DispatchMouseEventType::MousePressed,
                center,
                MouseButton::Left,
                1,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            page.execute(mouse_event(
                DispatchMouseEventType::MouseReleased,
                center,
                MouseButton::Left,
                1,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            // The click above already places focus/caret in the field. DOM.focus is
            // a best-effort reinforcement — some nodes (e.g. the AX node maps to a
            // non-focusable wrapper) report "not focusable", which must not abort the
            // insert when the click already focused the input.
            let focus = FocusParams::builder()
                .backend_node_id(BackendNodeId::new(backend_node_id))
                .build();
            let _ = page.execute(focus).await;
            page.execute(InsertTextParams::new(text))
                .await
                .map_err(|err| dispatch_err(&err))?;
            Ok(center)
        },
    )
    .await
    .map(|_point| ())
}

/// Readback for [`cdp_set_node_text`]: which selection strategy applied before
/// the replace, and whether an empty replacement was delivered as a Delete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdpSetNodeTextReadback {
    /// `selected_value` (input/textarea `select()`) or
    /// `selected_contenteditable` (DOM range over the editable host).
    pub selection_mode: String,
    /// True when `text` was empty and the selection was removed with a
    /// synthesized Delete key instead of `Input.insertText`.
    pub cleared_with_delete: bool,
}

/// DOM primitive result for Playwright-style `clear`, `focus`, `blur`, and
/// `selectText` actions on an observed CDP backend node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpDomPrimitiveResult {
    pub action: String,
    pub before_element: Value,
    pub after_element: Value,
    pub before_active_element: Value,
    pub after_active_element: Value,
    pub events_dispatched: Vec<String>,
    pub action_readback: Value,
}

const CDP_DOM_PRIMITIVE_FUNCTION: &str = r#"(el, requestedAction) => {
    const action = normalizeAction(requestedAction);
    if (!el || !el.isConnected) {
        throw new Error("resolved backend node is detached");
    }
    const events = [];
    const beforeElement = elementSummary(el);
    const beforeActiveElement = activeElementSummary(el.ownerDocument, el);
    let actionReadback = {};
    if (action === "clear") {
        actionReadback = performClear(el, events);
    } else if (action === "focus") {
        actionReadback = performFocus(el, events);
    } else if (action === "blur") {
        actionReadback = performBlur(el, events);
    } else if (action === "select_text") {
        actionReadback = performSelectText(el, events);
    } else {
        throw new Error(`unsupported DOM primitive ${JSON.stringify(requestedAction)}`);
    }
    return {
        action,
        before_element: beforeElement,
        after_element: el.isConnected ? elementSummary(el) : null,
        before_active_element: beforeActiveElement,
        after_active_element: activeElementSummary(el.ownerDocument, el),
        events_dispatched: events,
        action_readback: actionReadback
    };

    function normalizeAction(value) {
        const normalized = String(value || "").trim().toLowerCase();
        if (normalized === "selecttext" || normalized === "select-text") {
            return "select_text";
        }
        if (["clear", "focus", "blur", "select_text"].includes(normalized)) {
            return normalized;
        }
        throw new Error(`unsupported DOM primitive ${JSON.stringify(value)}`);
    }

    function performClear(element, events) {
        const editable = editableKind(element);
        if (!editable) {
            throw new Error(`clear supports editable input/textarea/contenteditable targets only; resolved ${tag(element)}`);
        }
        if (element.disabled || element.readOnly) {
            throw new Error("clear target is disabled or readonly");
        }
        try {
            if (typeof element.focus === "function") {
                element.focus({ preventScroll: true });
                events.push("focus");
            }
        } catch (_) {
            // Value mutation and readback are the source of truth.
        }
        const beforeValue = elementTextValue(element);
        const beforeInput = dispatchInputLikeEvent(element, "beforeinput", null, "deleteContentBackward", true);
        events.push("beforeinput");
        if (!beforeInput) {
            throw new Error("clear was cancelled by a beforeinput listener");
        }
        if (editable === "value") {
            setNativeValue(element, "");
            try {
                if (typeof element.setSelectionRange === "function") {
                    element.setSelectionRange(0, 0);
                }
            } catch (_) {
                // Some input types expose value but reject text selection.
            }
        } else {
            element.textContent = "";
        }
        dispatchInputLikeEvent(element, "input", null, "deleteContentBackward", false);
        events.push("input");
        element.dispatchEvent(new Event("change", { bubbles: true }));
        events.push("change");
        const afterValue = elementTextValue(element);
        if (afterValue !== "") {
            throw new Error(`clear postcondition failed: after value/text length ${afterValue.length} was not zero`);
        }
        return {
            before_value_len: beforeValue.length,
            after_value_len: afterValue.length,
            input_fired: true,
            change_fired: true
        };
    }

    function performFocus(element, events) {
        if (typeof element.focus !== "function") {
            throw new Error(`resolved ${tag(element)} element has no focus() method`);
        }
        try {
            element.focus({ preventScroll: true });
        } catch (_) {
            element.focus();
        }
        events.push("focus");
        const after = activeElementSummary(element.ownerDocument, element);
        if (!after.is_target) {
            throw new Error(`focus postcondition failed: activeElement is ${after.tag_name || "none"}#${after.id || ""}`);
        }
        return {
            active_element_is_target: true
        };
    }

    function performBlur(element, events) {
        if (typeof element.blur !== "function") {
            throw new Error(`resolved ${tag(element)} element has no blur() method`);
        }
        element.blur();
        events.push("blur");
        const after = activeElementSummary(element.ownerDocument, element);
        if (after.is_target) {
            throw new Error("blur postcondition failed: target remained document.activeElement");
        }
        return {
            active_element_is_target: false
        };
    }

    function performSelectText(element, events) {
        const beforeSelection = selectionSummary(element);
        let readback;
        if (isSelectableValueControl(element)) {
            try {
                if (typeof element.focus === "function") {
                    element.focus({ preventScroll: true });
                    events.push("focus");
                }
            } catch (_) {
                // select() may still work for some controls.
            }
            element.select();
            element.dispatchEvent(new Event("select", { bubbles: true }));
            events.push("select");
            element.ownerDocument.dispatchEvent(new Event("selectionchange", { bubbles: true }));
            events.push("selectionchange");
            const value = elementTextValue(element);
            const start = typeof element.selectionStart === "number" ? element.selectionStart : null;
            const end = typeof element.selectionEnd === "number" ? element.selectionEnd : null;
            const selected = start === null || end === null ? "" : value.slice(start, end);
            if (selected !== value) {
                throw new Error(`selectText postcondition failed: selected ${selected.length} of ${value.length} chars`);
            }
            readback = {
                selection_mode: "value_control_select",
                text_len: value.length,
                selection_start: start,
                selection_end: end,
                selected_text: selected
            };
        } else {
            const doc = element.ownerDocument;
            const selection = doc.defaultView && doc.defaultView.getSelection ? doc.defaultView.getSelection() : null;
            if (!selection) {
                throw new Error("selectText requires document.getSelection()");
            }
            const range = doc.createRange();
            range.selectNodeContents(element);
            selection.removeAllRanges();
            selection.addRange(range);
            doc.dispatchEvent(new Event("selectionchange", { bubbles: true }));
            events.push("selectionchange");
            const expected = String(element.textContent || "");
            const selected = String(selection.toString() || "");
            if (selected !== expected) {
                throw new Error(`selectText postcondition failed: selected ${selected.length} of ${expected.length} chars`);
            }
            readback = {
                selection_mode: "dom_range_select_node_contents",
                text_len: expected.length,
                selection_start: null,
                selection_end: null,
                selected_text: selected
            };
        }
        return {
            before_selection: beforeSelection,
            after_selection: selectionSummary(element),
            ...readback
        };
    }

    function editableKind(element) {
        const lower = tag(element);
        if (lower === "textarea") {
            return "value";
        }
        if (lower === "input") {
            const type = String(element.getAttribute("type") || "text").toLowerCase();
            const nonText = ["button", "submit", "reset", "checkbox", "radio", "range", "color", "file", "image", "hidden"];
            return nonText.includes(type) ? null : "value";
        }
        if (
            element.isContentEditable ||
            String(element.getAttribute("contenteditable") || "").toLowerCase() === "true" ||
            String(element.getAttribute("role") || "").toLowerCase() === "textbox"
        ) {
            return "contenteditable";
        }
        return null;
    }

    function isSelectableValueControl(element) {
        const lower = tag(element);
        if (!["input", "textarea"].includes(lower) || typeof element.select !== "function" || !("value" in element)) {
            return false;
        }
        if (lower === "textarea") {
            return true;
        }
        const type = String(element.getAttribute("type") || "text").toLowerCase();
        const nonSelectable = ["button", "submit", "reset", "checkbox", "radio", "range", "color", "file", "image", "hidden"];
        return !nonSelectable.includes(type);
    }

    function elementTextValue(element) {
        if ("value" in element) {
            return String(element.value ?? "");
        }
        return String(element.textContent || "");
    }

    function setNativeValue(element, value) {
        const lower = tag(element);
        const win = element.ownerDocument.defaultView || window;
        const proto =
            lower === "input"
                ? win.HTMLInputElement && win.HTMLInputElement.prototype
                : lower === "textarea"
                    ? win.HTMLTextAreaElement && win.HTMLTextAreaElement.prototype
                    : null;
        const descriptor = proto ? Object.getOwnPropertyDescriptor(proto, "value") : null;
        if (descriptor && typeof descriptor.set === "function") {
            descriptor.set.call(element, value);
        } else {
            element.value = value;
        }
    }

    function dispatchInputLikeEvent(element, type, data, inputType, cancelable) {
        const win = element.ownerDocument.defaultView || window;
        let event;
        try {
            event = new win.InputEvent(type, {
                bubbles: true,
                cancelable,
                data,
                inputType
            });
        } catch (_) {
            event = new win.Event(type, { bubbles: true, cancelable });
        }
        return element.dispatchEvent(event);
    }

    function activeElementSummary(doc, target) {
        const active = doc && doc.activeElement ? doc.activeElement : null;
        if (!active) {
            return {
                has_active_element: false,
                tag_name: "",
                id: "",
                name_attr: "",
                is_target: false
            };
        }
        return {
            has_active_element: true,
            tag_name: tag(active),
            id: String(active.id || ""),
            name_attr: String(active.getAttribute("name") || ""),
            is_target: active === target
        };
    }

    function selectionSummary(element) {
        const value = elementTextValue(element);
        if (typeof element.selectionStart === "number" && typeof element.selectionEnd === "number") {
            return {
                selection_mode: "value_control",
                selection_start: element.selectionStart,
                selection_end: element.selectionEnd,
                selected_text: value.slice(element.selectionStart, element.selectionEnd)
            };
        }
        const selection = element.ownerDocument.defaultView && element.ownerDocument.defaultView.getSelection
            ? element.ownerDocument.defaultView.getSelection()
            : null;
        return {
            selection_mode: "dom_selection",
            selection_start: null,
            selection_end: null,
            selected_text: selection ? String(selection.toString() || "") : ""
        };
    }

    function elementSummary(element) {
        const value = "value" in element ? String(element.value ?? "") : String(element.textContent || "");
        return {
            tag_name: tag(element),
            id: String(element.id || ""),
            name_attr: String(element.getAttribute("name") || ""),
            type_attr: String(element.getAttribute("type") || ""),
            value_len: value.length,
            text_len: String(element.textContent || "").length,
            disabled: Boolean(element.disabled),
            readonly: Boolean(element.readOnly)
        };
    }

    function tag(element) {
        return String(element && element.tagName || "").toLowerCase();
    }
}"#;

/// Performs a Playwright-style DOM primitive against an observed CDP backend
/// node without activating the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the node cannot be resolved, the primitive fails
/// its DOM postcondition, or the structured readback cannot be decoded.
pub async fn cdp_dom_primitive_node(
    endpoint: &str,
    target_id: &str,
    backend_node_id: i64,
    action: &str,
) -> A11yResult<CdpDomPrimitiveResult> {
    let normalized = match action.trim().to_ascii_lowercase().as_str() {
        "selecttext" | "select-text" => "select_text".to_owned(),
        "clear" | "focus" | "blur" | "select_text" => action.trim().to_ascii_lowercase(),
        other => {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!("unsupported CDP DOM primitive {other:?}"),
            });
        }
    };
    let result = cdp_evaluate_on_element(
        endpoint,
        target_id,
        backend_node_id,
        CDP_DOM_PRIMITIVE_FUNCTION,
        &[json!(normalized)],
        false,
        true,
    )
    .await?;
    serde_json::from_value::<CdpDomPrimitiveResult>(result.value).map_err(|err| {
        A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn DOM primitive decode: {err}"),
        }
    })
}

/// JS run on the resolved node to select its full content before the replace.
/// Mirrors the Playwright `fill` strategy: value controls use `select()`, a
/// contenteditable host gets a DOM range over its contents. Returns a wire
/// string so a non-editable target fails loud instead of appending.
const CDP_SELECT_ALL_FUNCTION: &str = r#"function() {
    if (this === null || this === undefined || !this.isConnected) { return 'detached'; }
    if (typeof this.select === 'function' && ('value' in this) && !this.disabled && !this.readOnly) {
        this.focus();
        this.select();
        return 'selected_value';
    }
    if (this.isContentEditable) {
        this.focus();
        const range = this.ownerDocument.createRange();
        range.selectNodeContents(this);
        const selection = this.ownerDocument.defaultView.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        return 'selected_contenteditable';
    }
    if (('value' in this) && (this.disabled || this.readOnly)) { return 'not_editable_disabled_or_readonly'; }
    return 'not_editable';
}"#;

/// Replaces a web editable node's full text content (#882): real click to
/// place focus/caret, best-effort `DOM.focus`, select-all on the exact
/// resolved node, then `Input.insertText` — which replaces the active
/// selection, the same strategy Playwright `fill()` uses. Empty `text`
/// removes the selection with a synthesized Delete key.
///
/// Fail-loud: a target that is neither a value control nor contenteditable
/// returns `A11Y_CDP_AXTREE_FAILED` naming the select-all readback — there is
/// no append fallback. Callers must verify with a separate
/// [`cdp_node_value`] readback.
///
/// # Errors
///
/// As [`cdp_click_node`], plus `A11Y_CDP_AXTREE_FAILED` for non-editable
/// targets.
pub async fn cdp_set_node_text(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    text: &str,
) -> A11yResult<CdpSetNodeTextReadback> {
    use chromiumoxide::cdp::browser_protocol::dom::FocusParams;

    let text = text.to_owned();
    with_node_center(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page, center| async move {
            page.execute(mouse_event(
                DispatchMouseEventType::MousePressed,
                center,
                MouseButton::Left,
                1,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            page.execute(mouse_event(
                DispatchMouseEventType::MouseReleased,
                center,
                MouseButton::Left,
                1,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            // The click already places focus; DOM.focus is best-effort
            // reinforcement (some AX nodes map to non-focusable wrappers).
            let focus = FocusParams::builder()
                .backend_node_id(BackendNodeId::new(backend_node_id))
                .build();
            let _ = page.execute(focus).await;

            let resolve = ResolveNodeParams::builder()
                .backend_node_id(BackendNodeId::new(backend_node_id))
                .object_group("synapse_set_field_text")
                .build();
            let resolved =
                page.execute(resolve)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
                    })?;
            let object_id =
                resolved
                    .object
                    .object_id
                    .clone()
                    .ok_or_else(|| A11yError::CdpAxtreeFailed {
                        detail: format!(
                            "resolveNode for backendNodeId {backend_node_id} returned no objectId"
                        ),
                    })?;
            let call = CallFunctionOnParams::builder()
                .function_declaration(CDP_SELECT_ALL_FUNCTION)
                .object_id(object_id)
                .return_by_value(true)
                .silent(true)
                .build()
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("build Runtime.callFunctionOn select-all params: {err}"),
                })?;
            let selection_mode: String = call_function_on_value(&page, call, "select-all").await?;
            if !matches!(
                selection_mode.as_str(),
                "selected_value" | "selected_contenteditable"
            ) {
                return Err(A11yError::CdpAxtreeFailed {
                    detail: format!(
                        "cdp_set_node_text refused backendNodeId {backend_node_id}: target is not an editable web node (select-all readback: {selection_mode})"
                    ),
                });
            }

            let cleared_with_delete = text.is_empty();
            if cleared_with_delete {
                let delete = CdpKeyStroke {
                    key: "Delete".to_owned(),
                    code: "Delete".to_owned(),
                    windows_virtual_key_code: 46,
                    native_virtual_key_code: 46,
                    key_identifier: None,
                    text: None,
                    unmodified_text: None,
                    modifier_bit: 0,
                    location: None,
                };
                page.execute(cdp_key_event(DispatchKeyEventType::KeyDown, &delete, 0)?)
                    .await
                    .map_err(|err| dispatch_err(&err))?;
                page.execute(cdp_key_event(DispatchKeyEventType::KeyUp, &delete, 0)?)
                    .await
                    .map_err(|err| dispatch_err(&err))?;
            } else {
                page.execute(InsertTextParams::new(text))
                    .await
                    .map_err(|err| dispatch_err(&err))?;
            }
            Ok(CdpSetNodeTextReadback {
                selection_mode,
                cleared_with_delete,
            })
        },
    )
    .await
}

/// Dispatches a key sequence to a specific CDP page target without activating
/// the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if `Input.dispatchKeyEvent` fails.
pub async fn cdp_press_key_sequence(
    endpoint: &str,
    target_id: &str,
    keys: Vec<CdpKeyStroke>,
    hold_ms: u32,
) -> A11yResult<()> {
    if keys.is_empty() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "cdp_press_key_sequence requires at least one key".to_owned(),
        });
    }
    with_target_page(endpoint, target_id, |page| async move {
        let mut modifiers = 0_i64;
        for key in &keys {
            let key_down_type = if key.text.is_some() {
                DispatchKeyEventType::KeyDown
            } else {
                DispatchKeyEventType::RawKeyDown
            };
            let event_modifiers = modifiers | key.modifier_bit;
            page.execute(cdp_key_event(key_down_type, key, event_modifiers)?)
                .await
                .map_err(|err| dispatch_err(&err))?;
            modifiers = event_modifiers;
        }
        if hold_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(hold_ms))).await;
        }
        for key in keys.iter().rev() {
            if key.modifier_bit != 0 {
                modifiers &= !key.modifier_bit;
            }
            page.execute(cdp_key_event(DispatchKeyEventType::KeyUp, key, modifiers)?)
                .await
                .map_err(|err| dispatch_err(&err))?;
        }
        Ok(())
    })
    .await
}

/// Dispatches a viewport-CSS mouse move/drag path to one CDP page target
/// without moving the OS cursor or activating the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the stroke is invalid or CDP dispatch fails.
pub async fn cdp_mouse_stroke_target(
    endpoint: &str,
    target_id: &str,
    points: Vec<CdpMouseStrokePoint>,
    button: Option<CdpMouseButton>,
) -> A11yResult<CdpMouseStrokeResult> {
    validate_cdp_mouse_stroke_points(&points)?;
    let start = cdp_stroke_action_point(points[0]);
    let end = points
        .last()
        .map_or(start, |point| cdp_stroke_action_point(*point));
    let duration_ms = points.last().map_or(0.0, |point| point.elapsed_ms.max(0.0));
    let button = button
        .map(CdpMouseButton::to_cdp)
        .unwrap_or(MouseButton::None);
    let dispatched_target_id = with_target_page(endpoint, target_id, |page| async move {
        let dispatched_target_id = page.target_id().inner().clone();
        Ok(dispatched_target_id)
    })
    .await?;
    dispatch_cdp_mouse_stroke_raw(endpoint, &dispatched_target_id, &points, button).await?;
    Ok(CdpMouseStrokeResult {
        target_id: dispatched_target_id,
        point_count: points.len(),
        start,
        end,
        duration_ms,
    })
}

/// Touch-taps viewport CSS coordinates in a specific CDP page target without
/// moving the OS cursor or activating the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the point is invalid or CDP dispatch fails.
pub async fn cdp_touch_tap_target(
    endpoint: &str,
    target_id: &str,
    point: CdpActionPoint,
) -> A11yResult<CdpTouchTapResult> {
    validate_cdp_action_point(point, "touch tap")?;
    with_target_page(endpoint, target_id, |page| async move {
        dispatch_touch_tap_on_page(&page, point).await
    })
    .await
}

/// Reads the target page's active DOM element, value, and selection without
/// activating the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the DOM readback fails.
pub async fn cdp_active_element_state(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpActiveElementState> {
    let target_id = target_id.to_owned();
    let target_id_for_lookup = target_id.clone();
    with_target_page(endpoint, &target_id_for_lookup, |page| async move {
        let expression = format!(
            r#"(() => {{
                const el = document.activeElement;
                if (!el) {{
                    return {{
                        target_id: {target_id_json},
                        has_active_element: false,
                        is_editable: false,
                        tag_name: "",
                        id: "",
                        name: "",
                        value: "",
                        selection_start: null,
                        selection_end: null
                    }};
                }}
                const tagName = String(el.tagName || "");
                const tag = tagName.toUpperCase();
                const inputType = String(el.getAttribute("type") || "text").toLowerCase();
                const textInputTypes = new Set([
                    "text", "search", "url", "tel", "email", "password", "number",
                    "date", "datetime-local", "month", "time", "week", "color"
                ]);
                const ariaDisabled = String(el.getAttribute("aria-disabled") || "").toLowerCase() === "true";
                const isDisabled = Boolean(el.disabled) || ariaDisabled;
                const isReadOnly = Boolean(el.readOnly);
                const isEditable =
                    (tag === "TEXTAREA" && !isDisabled && !isReadOnly) ||
                    (tag === "INPUT" && textInputTypes.has(inputType) && !isDisabled && !isReadOnly) ||
                    (Boolean(el.isContentEditable) && !isDisabled) ||
                    (String(el.getAttribute("role") || "").toLowerCase() === "textbox" && !isDisabled);
                const value = ("value" in el)
                    ? String(el.value ?? "")
                    : String(el.textContent ?? "");
                const selectionStart = (typeof el.selectionStart === "number")
                    ? el.selectionStart
                    : null;
                const selectionEnd = (typeof el.selectionEnd === "number")
                    ? el.selectionEnd
                    : null;
                return {{
                    target_id: {target_id_json},
                    has_active_element: true,
                    is_editable: isEditable,
                    tag_name: tagName,
                    id: String(el.id || ""),
                    name: String(el.getAttribute("name") || ""),
                    value,
                    selection_start: selectionStart,
                    selection_end: selectionEnd
                }};
            }})()"#,
            target_id_json =
                serde_json::to_string(&target_id).unwrap_or_else(|_| "\"\"".to_owned())
        );
        page.evaluate_expression(expression)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.evaluate active-element readback: {err}"),
            })?
            .into_value::<CdpActiveElementState>()
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.evaluate active-element decode: {err}"),
            })
    })
    .await
}

/// Reads bounded visible DOM text from a specific CDP page target without
/// activating the tab or using OS foreground input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if `Runtime.evaluate` cannot read/decode the page.
pub async fn cdp_page_text_target(
    endpoint: &str,
    target_id: &str,
    max_chars: usize,
) -> A11yResult<CdpPageTextState> {
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let target_id_json =
            serde_json::to_string(&target_id).unwrap_or_else(|_| "\"\"".to_owned());
        let max_chars = max_chars.min(65_536);
        let expression = format!(
            r#"(() => {{
                const maxChars = {max_chars};
                const source =
                    (document.body && typeof document.body.innerText === "string")
                        ? document.body.innerText
                        : ((document.documentElement && typeof document.documentElement.innerText === "string")
                            ? document.documentElement.innerText
                            : ((document.body && typeof document.body.textContent === "string")
                                ? document.body.textContent
                                : ((document.documentElement && typeof document.documentElement.textContent === "string")
                                    ? document.documentElement.textContent
                                    : "")));
                let text = "";
                let textLen = 0;
                for (const ch of String(source || "")) {{
                    if (textLen < maxChars) {{
                        text += ch;
                    }}
                    textLen += 1;
                }}
                return {{
                    target_id: {target_id_json},
                    url: String(location.href || ""),
                    title: String(document.title || ""),
                    ready_state: String(document.readyState || ""),
                    text,
                    text_len: textLen,
                    text_truncated: textLen > maxChars,
                    max_chars: maxChars
                }};
            }})()"#,
        );
        page.evaluate_expression(expression)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.evaluate page text readback: {err}"),
            })?
            .into_value::<CdpPageTextState>()
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.evaluate page text decode: {err}"),
            })
    })
    .await
}

/// Evaluates a JavaScript `expression` in a specific CDP page target and returns
/// a structured, separately-read result. JS exceptions are surfaced loudly as
/// `A11Y_CDP_AXTREE_FAILED` carrying the thrown message, class, location and
/// stack — they are never swallowed or coerced to a success value.
///
/// `await_promise` awaits a returned thenable before resolving;
/// `return_by_value` serializes the result as JSON (set false to receive only
/// the type/description handle for non-serializable values like DOM nodes).
///
/// This is the keystone for the Playwright-parity browser surface (#1063): page
/// content, element introspection, state queries and assertions are all built on
/// it. It is background-safe — no tab activation, no OS foreground input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the evaluate command fails at the protocol level,
/// the page throws an exception, or the result cannot be decoded.
pub async fn cdp_evaluate_expression(
    endpoint: &str,
    target_id: &str,
    expression: &str,
    await_promise: bool,
    return_by_value: bool,
) -> A11yResult<CdpEvaluateResult> {
    use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
    let expression = expression.to_owned();
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        // Read URL/title/readyState separately so the result carries the page
        // context it was evaluated against (FSV source-of-truth correlation).
        let state = read_page_state(&page).await?;
        let params = EvaluateParams::builder()
            .expression(expression)
            .return_by_value(return_by_value)
            .await_promise(await_promise)
            .build()
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.evaluate params build: {err}"),
            })?;
        let returns = page
            .execute(params)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.evaluate: {err}"),
            })?
            .result;
        if let Some(exception) = returns.exception_details.as_ref() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Runtime.evaluate threw: {}",
                    format_evaluate_exception(exception)
                ),
            });
        }
        Ok(evaluate_result_from_remote(
            target_id,
            &state,
            return_by_value,
            returns.result,
        ))
    })
    .await
}

/// Calls a JavaScript function declaration against a specific DOM element
/// (resolved from its `backend_node_id`) in a CDP page target, passing `args` as
/// `Runtime.callFunctionOn` arguments and returning the same structured result
/// shape as [`cdp_evaluate_expression`] (#1066/#1067). `this` inside the function
/// is the element. Background-safe: no tab activation, no OS foreground input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the node cannot be resolved in this target, the
/// call fails at the protocol level, the function throws, or the result cannot
/// be decoded.
pub async fn cdp_evaluate_on_element(
    endpoint: &str,
    target_id: &str,
    backend_node_id: i64,
    function_declaration: &str,
    args: &[Value],
    await_promise: bool,
    return_by_value: bool,
) -> A11yResult<CdpEvaluateResult> {
    let function_declaration = function_declaration.to_owned();
    let args = args.to_vec();
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let state = read_page_state(&page).await?;
        let resolve = ResolveNodeParams::builder()
            .backend_node_id(BackendNodeId::new(backend_node_id))
            .object_group("synapse_browser_evaluate")
            .build();
        let resolved =
            page.execute(resolve)
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
                })?;
        let object_id =
            resolved
                .object
                .object_id
                .clone()
                .ok_or_else(|| A11yError::CdpAxtreeFailed {
                    detail: format!(
                        "resolveNode for backendNodeId {backend_node_id} returned no objectId (element not present in this target's DOM?)"
                    ),
                })?;
        // Playwright-parity calling convention: the element is passed as the
        // FIRST argument (e.g. `el => el.value`), followed by the caller's args.
        // CDP `callFunctionOn` binds the element to `this`, so wrap the user
        // function and forward `this` + the call arguments to it. This works for
        // arrow functions (which cannot bind `this`) and regular functions
        // alike.
        let wrapped = format!(
            "function() {{ return ({function_declaration}).apply(null, [this].concat(Array.prototype.slice.call(arguments))); }}"
        );
        let mut call = CallFunctionOnParams::builder()
            .function_declaration(wrapped)
            .object_id(object_id)
            .return_by_value(return_by_value)
            .await_promise(await_promise);
        for arg in &args {
            call = call.argument(CallArgument::builder().value(arg.clone()).build());
        }
        let call = call.build().map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Runtime.callFunctionOn params: {err}"),
        })?;
        let returns = page
            .execute(call)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.callFunctionOn: {err}"),
            })?
            .result;
        if let Some(exception) = returns.exception_details.as_ref() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Runtime.callFunctionOn threw: {}",
                    format_evaluate_exception(exception)
                ),
            });
        }
        Ok(evaluate_result_from_remote(
            target_id,
            &state,
            return_by_value,
            returns.result,
        ))
    })
    .await
}

/// Adds a script that evaluates before page scripts on every new document for a
/// CDP page target. Background-safe: this never activates the tab or uses OS
/// foreground input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the Page command or post-command readback fails.
pub async fn cdp_add_init_script_target(
    endpoint: &str,
    target_id: &str,
    source: &str,
    world_name: Option<&str>,
    include_command_line_api: Option<bool>,
    run_immediately: Option<bool>,
) -> A11yResult<CdpInitScriptResult> {
    let source = source.to_owned();
    let world_name = world_name.map(ToOwned::to_owned);
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let mut builder = AddScriptToEvaluateOnNewDocumentParams::builder().source(source);
        if let Some(world_name) = world_name {
            builder = builder.world_name(world_name);
        }
        if let Some(include_command_line_api) = include_command_line_api {
            builder = builder.include_command_line_api(include_command_line_api);
        }
        if let Some(run_immediately) = run_immediately {
            builder = builder.run_immediately(run_immediately);
        }
        let params = builder.build().map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Page.addScriptToEvaluateOnNewDocument params: {err}"),
        })?;
        let added = page
            .execute(params)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.addScriptToEvaluateOnNewDocument: {err}"),
            })?
            .result;
        let state = read_page_state(&page).await?;
        Ok(CdpInitScriptResult {
            target_id,
            identifier: added.identifier.inner().clone(),
            state,
        })
    })
    .await
}

/// Removes a script previously installed with
/// [`cdp_add_init_script_target`]. Background-safe: this never activates the tab
/// or uses OS foreground input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the Page command or post-command readback fails.
pub async fn cdp_remove_init_script_target(
    endpoint: &str,
    target_id: &str,
    identifier: &str,
) -> A11yResult<CdpInitScriptResult> {
    let identifier = identifier.to_owned();
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        page.execute(RemoveScriptToEvaluateOnNewDocumentParams::new(
            ScriptIdentifier::new(identifier.clone()),
        ))
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Page.removeScriptToEvaluateOnNewDocument({identifier:?}): {err}"),
        })?;
        let state = read_page_state(&page).await?;
        Ok(CdpInitScriptResult {
            target_id,
            identifier,
            state,
        })
    })
    .await
}

/// The injected JavaScript selector engine (#1110). One self-contained function
/// `(scope, spec) => { count, elements }` that resolves css / xpath / text /
/// label / placeholder / altText / title / testid / layout against `scope`
/// (the document or a root element), applies the `hasText` filter and `nth`
/// pick, and returns the matched elements (shadow-piercing, DOM order, or — for
/// `layout` — sorted by ascending geometric distance). The algorithms mirror
/// Playwright's injected engine (`selectorUtils.ts` / `layoutSelectorUtils.ts`):
/// whitespace normalization, deepest-element text matching, the label
/// resolution order, and the proximity box scorers. `role` is resolved natively
/// by [`locate_role`] instead because accessible role/name are not exposed to
/// page JavaScript.
///
/// Kept placeholder-free so it can be concatenated (never `format!`-ed, which
/// would choke on the JS braces) into a `Runtime.evaluate` / `callFunctionOn`
/// expression.
const SYNAPSE_LOCATE_JS: &str = r"function(scope, spec) {
  function norm(s){ return (s==null?'':String(s)).replace(/[\u200b\u00ad]/g,'').replace(/\s+/g,' ').trim(); }
  function skipText(el){
    if(!el||!el.nodeName) return true;
    var d = el.ownerDocument;
    return el.nodeName==='SCRIPT'||el.nodeName==='NOSCRIPT'||el.nodeName==='STYLE'||(!!d&&!!d.head&&d.head.contains(el));
  }
  var textCache = new Map();
  function elementText(root){
    var v = textCache.get(root);
    if(v!==undefined) return v;
    v = {full:'', normalized:'', immediate:[]};
    if(!skipText(root)){
      if((root instanceof HTMLInputElement) && (root.type==='submit'||root.type==='button')){
        v = {full:root.value, normalized:norm(root.value), immediate:[root.value]};
      } else {
        var cur='';
        for(var c=root.firstChild;c;c=c.nextSibling){
          if(c.nodeType===3){ v.full+=c.nodeValue||''; cur+=c.nodeValue||''; }
          else if(c.nodeType===8){ continue; }
          else { if(cur) v.immediate.push(cur); cur=''; if(c.nodeType===1) v.full+=elementText(c).full; }
        }
        if(cur) v.immediate.push(cur);
        if(root.shadowRoot) v.full+=elementText(root.shadowRoot).full;
        if(v.full) v.normalized=norm(v.full);
      }
    }
    textCache.set(root,v);
    return v;
  }
  // Playwright 'self': element matches AND no descendant element also matches,
  // i.e. the deepest element bearing the text (avoids returning <body>/<div>).
  function matchesTextSelf(el, matcher){
    if(skipText(el)) return false;
    if(!matcher(elementText(el))) return false;
    for(var c=el.firstChild;c;c=c.nextSibling){
      if(c.nodeType===1 && matcher(elementText(c))) return false;
    }
    if(el.shadowRoot && matcher(elementText(el.shadowRoot))) return false;
    return true;
  }
  function ariaLabelledBy(el){
    if(!el.getAttribute) return null;
    var ref = el.getAttribute('aria-labelledby');
    if(ref===null) return null;
    var root = el.getRootNode();
    var out=[]; var ids = ref.split(/\s+/);
    for(var i=0;i<ids.length;i++){
      var id=ids[i]; if(!id) continue;
      var found = (root && root.getElementById)? root.getElementById(id) : el.ownerDocument.getElementById(id);
      if(found) out.push(found);
    }
    return out;
  }
  function elementLabels(el){
    var lbe = ariaLabelledBy(el);
    if(lbe && lbe.length) return lbe.map(function(l){return elementText(l);});
    var al = el.getAttribute? el.getAttribute('aria-label'):null;
    if(al!==null && al.trim()) return [{full:al, normalized:norm(al), immediate:[al]}];
    var isInput = el.nodeName==='INPUT' && el.type!=='hidden';
    if(['BUTTON','METER','OUTPUT','PROGRESS','SELECT','TEXTAREA'].indexOf(el.nodeName)>=0 || isInput){
      var labels = el.labels;
      if(labels){ var arr=[]; for(var i=0;i<labels.length;i++) arr.push(elementText(labels[i])); return arr; }
    }
    return [];
  }
  function stringMatcher(query, exact, isRegex){
    if(isRegex){ var re=new RegExp(query); return function(s){ return re.test(s==null?'':String(s)); }; }
    if(exact){ var q=norm(query); return function(s){ return norm(s)===q; }; }
    var q2=norm(query).toLowerCase(); return function(s){ return norm(s).toLowerCase().indexOf(q2)>=0; };
  }
  function allElements(root){
    var out=[];
    function walk(r){
      var els=r.querySelectorAll('*');
      for(var i=0;i<els.length;i++){ out.push(els[i]); if(els[i].shadowRoot) walk(els[i].shadowRoot); }
    }
    walk(root);
    return out;
  }
  function boxRightOf(b1,b2,md){ var d=b1.left-b2.right; if(d<0||(md!==undefined&&d>md)) return undefined; return d+Math.max(b2.bottom-b1.bottom,0)+Math.max(b1.top-b2.top,0); }
  function boxLeftOf(b1,b2,md){ var d=b2.left-b1.right; if(d<0||(md!==undefined&&d>md)) return undefined; return d+Math.max(b2.bottom-b1.bottom,0)+Math.max(b1.top-b2.top,0); }
  function boxAbove(b1,b2,md){ var d=b2.top-b1.bottom; if(d<0||(md!==undefined&&d>md)) return undefined; return d+Math.max(b1.left-b2.left,0)+Math.max(b2.right-b1.right,0); }
  function boxBelow(b1,b2,md){ var d=b1.top-b2.bottom; if(d<0||(md!==undefined&&d>md)) return undefined; return d+Math.max(b1.left-b2.left,0)+Math.max(b2.right-b1.right,0); }
  function boxNear(b1,b2,md){ var k=(md===undefined)?50:md; var s=0; if(b1.left-b2.right>=0)s+=b1.left-b2.right; if(b2.left-b1.right>=0)s+=b2.left-b1.right; if(b2.top-b1.bottom>=0)s+=b2.top-b1.bottom; if(b1.top-b2.bottom>=0)s+=b1.top-b2.bottom; return s>k?undefined:s; }

  var qroot = scope;
  var engine = spec.engine;
  var results = [];
  if(engine==='css'){
    results = Array.prototype.slice.call(qroot.querySelectorAll(spec.query));
  } else if(engine==='xpath'){
    var doc = scope.ownerDocument || scope;
    var snap = doc.evaluate(spec.query, scope, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
    for(var i=0;i<snap.snapshotLength;i++){ var n=snap.snapshotItem(i); if(n&&n.nodeType===1) results.push(n); }
  } else if(engine==='text'){
    var m;
    if(spec.regex){ var re=new RegExp(spec.query); m=function(t){ return re.test(t.normalized); }; }
    else if(spec.exact){ var q=norm(spec.query); m=function(t){ return t.normalized===q; }; }
    else { var q2=norm(spec.query).toLowerCase(); m=function(t){ return t.normalized.toLowerCase().indexOf(q2)>=0; }; }
    var all=allElements(qroot);
    for(var i=0;i<all.length;i++){ if(matchesTextSelf(all[i],m)) results.push(all[i]); }
  } else if(engine==='testid'){
    var attr = spec.testidAttribute || 'data-testid';
    var exact = (spec.exact===undefined)? true : spec.exact;
    var mt = stringMatcher(spec.query, exact, spec.regex);
    var all=allElements(qroot);
    for(var i=0;i<all.length;i++){ var el=all[i]; if(el.hasAttribute(attr) && mt(el.getAttribute(attr))) results.push(el); }
  } else if(engine==='placeholder'||engine==='alttext'||engine==='title'){
    var attr2 = engine==='placeholder'?'placeholder':(engine==='alttext'?'alt':'title');
    var ma = stringMatcher(spec.query, spec.exact, spec.regex);
    var all=allElements(qroot);
    for(var i=0;i<all.length;i++){ var el=all[i]; var val=el.getAttribute(attr2); if(val!==null && ma(val)) results.push(el); }
  } else if(engine==='label'){
    var ml = stringMatcher(spec.query, spec.exact, spec.regex);
    var all=allElements(qroot);
    for(var i=0;i<all.length;i++){ var el=all[i]; var labels=elementLabels(el); if(labels.length){ var hit=false; for(var j=0;j<labels.length;j++){ if(ml(labels[j].normalized)){hit=true;break;} } if(hit) results.push(el); } }
  } else if(engine==='layout'){
    var base = Array.prototype.slice.call(qroot.querySelectorAll(spec.query));
    var anchors = spec.anchor ? Array.prototype.slice.call(qroot.querySelectorAll(spec.anchor)) : [];
    var rel = spec.relation;
    var md = (spec.maxDistance===undefined||spec.maxDistance===null)?undefined:spec.maxDistance;
    var scorer = rel==='left-of'?boxLeftOf : rel==='right-of'?boxRightOf : rel==='above'?boxAbove : rel==='below'?boxBelow : boxNear;
    var scored=[];
    for(var i=0;i<base.length;i++){
      var b=base[i]; var bb=b.getBoundingClientRect(); var best=undefined;
      for(var j=0;j<anchors.length;j++){ var a=anchors[j]; if(a===b) continue; var sc=scorer(bb,a.getBoundingClientRect(),md); if(sc===undefined) continue; if(best===undefined||sc<best) best=sc; }
      if(best!==undefined) scored.push([b,best]);
    }
    scored.sort(function(x,y){ return x[1]-y[1]; });
    results = scored.map(function(x){ return x[0]; });
  } else {
    throw new Error('synapse_locate: unsupported engine '+engine);
  }
  if(spec.hasText!==undefined && spec.hasText!==null && spec.hasText!==''){
    var ht=norm(spec.hasText).toLowerCase();
    results = results.filter(function(el){ return elementText(el).normalized.toLowerCase().indexOf(ht)>=0; });
  }
  var count = results.length;
  if(spec.nth!==undefined && spec.nth!==null){
    var idx = spec.nth; if(idx<0) idx = count + idx;
    results = (idx>=0 && idx<count)? [results[idx]] : [];
  }
  var limit = spec.limit || 50;
  var elements = results.slice(0, limit);
  return { count: count, returned: elements.length, elements: elements };
}";

const SYNAPSE_LOCATE_OBJECT_GROUP: &str = "synapse_locate";

/// Resolves a Playwright-style selector (`engine` + `query` + options) to live
/// DOM nodes in a CDP page target, returning their `backendNodeId`s (capped at
/// `request.limit`) plus the total match count before the cap (#1110–#1119).
/// Background-safe: read-only, no tab activation, no OS foreground input.
///
/// `css` / `xpath` / `text` / `label` / `placeholder` / `altText` / `title` /
/// `testid` / `layout` are resolved by the injected [`SYNAPSE_LOCATE_JS`]
/// engine; `role` is resolved by the native `Accessibility.queryAXTree`. Strict
/// mode, `nth`, and `limit` are enforced uniformly.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if any document/eval/AX command fails (an invalid
/// selector or regex surfaces the engine's error verbatim) or strict mode is
/// violated.
pub async fn cdp_locate(
    endpoint: &str,
    target_id: &str,
    request: CdpLocateRequest,
) -> A11yResult<CdpLocateResult> {
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let state = read_page_state(&page).await?;
        let (backend_node_ids, match_count) = if request.engine.uses_injected_js() {
            locate_via_injected_js(&page, &request).await?
        } else {
            locate_role(&page, &request).await?
        };
        // Strict mode mirrors Playwright: an explicit nth/first/last already
        // disambiguates, so it bypasses the strictness check.
        if request.strict && request.nth.is_none() && match_count > 1 {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "strict mode: {} selector {:?} resolved to {match_count} elements; refine the query, set nth, or disable strict",
                    request.engine.as_str(),
                    request.query
                ),
            });
        }
        let returned_count = backend_node_ids.len();
        // With an explicit nth we deliberately picked a single element, so
        // `match_count > returned_count` is expected and not a truncation.
        let truncated = request.nth.is_none() && match_count > returned_count;
        Ok(CdpLocateResult {
            target_id,
            url: state.url,
            title: state.title,
            engine: request.engine.as_str().to_owned(),
            query: request.query.clone(),
            frame_id: request.frame_id.clone(),
            match_count,
            backend_node_ids,
            returned_count,
            truncated,
        })
    })
    .await
}

/// Serializes a [`CdpLocateRequest`] into the `spec` object the injected engine
/// consumes (camelCase keys; `None`s become JSON `null`, which the engine
/// treats as "unset").
fn locate_spec_json(request: &CdpLocateRequest) -> Value {
    serde_json::json!({
        "engine": request.engine.as_str(),
        "query": request.query,
        "exact": request.exact,
        "regex": request.regex,
        "testidAttribute": request.testid_attribute,
        "relation": request.relation.map(CdpLayoutRelation::as_str),
        "anchor": request.anchor,
        "maxDistance": request.max_distance,
        "hasText": request.has_text,
        "nth": request.nth,
        "limit": request.limit,
    })
}

/// Resolves the JS-engine families. Evaluates [`SYNAPSE_LOCATE_JS`] against the
/// document (or a root element, when `root_backend_node_id` is set) and maps the
/// returned element handles to `backendNodeId`s.
#[allow(
    clippy::future_not_send,
    reason = "single CDP eval/getProperties transaction; matches the rest of this module"
)]
async fn locate_via_injected_js(
    page: &chromiumoxide::Page,
    request: &CdpLocateRequest,
) -> A11yResult<(Vec<i64>, usize)> {
    use chromiumoxide::cdp::browser_protocol::dom::{
        BackendNodeId, DescribeNodeParams, ResolveNodeParams,
    };
    use chromiumoxide::cdp::js_protocol::runtime::{
        CallArgument, CallFunctionOnParams, EvaluateParams, GetPropertiesParams,
        ReleaseObjectGroupParams,
    };

    let spec = locate_spec_json(request);
    let frame_context = locate_frame_context(page, request.frame_id.as_deref()).await?;

    // Evaluate the engine, yielding the `{count, elements}` result object id.
    let (result_object_id, _) = if let Some(root_backend) = request.root_backend_node_id {
        let mut resolve = ResolveNodeParams::builder()
            .backend_node_id(BackendNodeId::new(root_backend))
            .object_group(SYNAPSE_LOCATE_OBJECT_GROUP);
        if let Some(context) = frame_context {
            resolve = resolve.execution_context_id(context);
        }
        let resolve = resolve.build();
        let resolved = page
            .execute(resolve)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate root resolveNode backendNodeId {root_backend}: {err}"),
            })?;
        let root_object_id = resolved.object.object_id.clone().ok_or_else(|| {
            A11yError::CdpAxtreeFailed {
                detail: format!(
                    "locate root backendNodeId {root_backend} returned no objectId (not in this target's DOM?)"
                ),
            }
        })?;
        let declaration =
            String::from("function(spec){ return (") + SYNAPSE_LOCATE_JS + ")(this, spec); }";
        let call = CallFunctionOnParams::builder()
            .function_declaration(declaration)
            .object_id(root_object_id)
            .object_group(SYNAPSE_LOCATE_OBJECT_GROUP)
            .argument(CallArgument::builder().value(spec).build())
            .return_by_value(false)
            .build()
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate callFunctionOn params build: {err}"),
            })?;
        let returns = page
            .execute(call)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate Runtime.callFunctionOn: {err}"),
            })?
            .result;
        if let Some(exception) = returns.exception_details.as_ref() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "locate engine threw: {}",
                    format_evaluate_exception(exception)
                ),
            });
        }
        (returns.result.object_id, returns.result.subtype)
    } else {
        let spec_json = serde_json::to_string(&spec).map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("locate spec serialize: {err}"),
        })?;
        let expression = String::from("(") + SYNAPSE_LOCATE_JS + ")(document, " + &spec_json + ")";
        let mut params = EvaluateParams::builder()
            .expression(expression)
            .object_group(SYNAPSE_LOCATE_OBJECT_GROUP)
            .return_by_value(false)
            .await_promise(false);
        if let Some(context) = frame_context {
            params = params.context_id(context);
        }
        let params = params.build().map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("locate Runtime.evaluate params build: {err}"),
        })?;
        let returns = page
            .execute(params)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate Runtime.evaluate: {err}"),
            })?
            .result;
        if let Some(exception) = returns.exception_details.as_ref() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "locate engine threw: {}",
                    format_evaluate_exception(exception)
                ),
            });
        }
        (returns.result.object_id, returns.result.subtype)
    };

    let Some(result_object_id) = result_object_id else {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "locate engine returned no result object (expected {count, elements})"
                .to_owned(),
        });
    };

    // Read `count` (true total) and the `elements` array handle off the result.
    let result_props = page
        .execute(
            GetPropertiesParams::builder()
                .object_id(result_object_id)
                .own_properties(true)
                .build()
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("locate getProperties(result) build: {err}"),
                })?,
        )
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("locate Runtime.getProperties(result): {err}"),
        })?
        .result;
    let mut match_count = 0usize;
    let mut elements_object_id = None;
    for prop in result_props.result {
        match prop.name.as_str() {
            "count" => {
                match_count = prop
                    .value
                    .as_ref()
                    .and_then(|value| value.value.as_ref())
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(0);
            }
            "elements" => {
                elements_object_id = prop
                    .value
                    .as_ref()
                    .and_then(|value| value.object_id.clone());
            }
            _ => {}
        }
    }

    let mut backend_node_ids = Vec::new();
    if let Some(elements_object_id) = elements_object_id {
        let element_props = page
            .execute(
                GetPropertiesParams::builder()
                    .object_id(elements_object_id)
                    .own_properties(true)
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("locate getProperties(elements) build: {err}"),
                    })?,
            )
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate Runtime.getProperties(elements): {err}"),
            })?
            .result;
        // Indexed properties ("0","1",…) carry the element handles; sort numeric.
        let mut indexed: Vec<(usize, _)> = Vec::new();
        for prop in element_props.result {
            let Ok(index) = prop.name.parse::<usize>() else {
                continue;
            };
            if let Some(object_id) = prop
                .value
                .as_ref()
                .and_then(|value| value.object_id.clone())
            {
                indexed.push((index, object_id));
            }
        }
        indexed.sort_by_key(|(index, _)| *index);
        for (index, object_id) in indexed {
            let described = page
                .execute(DescribeNodeParams::builder().object_id(object_id).build())
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("locate DOM.describeNode(match {index}): {err}"),
                })?;
            backend_node_ids.push(*described.result.node.backend_node_id.inner());
        }
    }

    // Best-effort release of the element handles; failure is non-fatal.
    let _ = page
        .execute(ReleaseObjectGroupParams::new(SYNAPSE_LOCATE_OBJECT_GROUP))
        .await;

    Ok((backend_node_ids, match_count))
}

/// Resolves the `role` engine (`getByRole`) via the native
/// `Accessibility.queryAXTree`, which computes ARIA role + accessible name for
/// every node in the subtree (the same computation Playwright reimplements in
/// JS). Filters by accessible name (exact/substring/regex), ARIA states
/// (checked/pressed/expanded/selected/disabled/level), and hidden-node
/// inclusion, then applies `nth`/`limit`.
#[allow(
    clippy::future_not_send,
    reason = "single CDP queryAXTree transaction; matches the rest of this module"
)]
async fn locate_role(
    page: &chromiumoxide::Page,
    request: &CdpLocateRequest,
) -> A11yResult<(Vec<i64>, usize)> {
    use chromiumoxide::cdp::browser_protocol::accessibility::QueryAxTreeParams;
    use chromiumoxide::cdp::browser_protocol::dom::{BackendNodeId, GetDocumentParams};
    use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;

    let mut builder = QueryAxTreeParams::builder().role(request.query.clone());
    builder = if let Some(root_backend) = request.root_backend_node_id {
        builder.backend_node_id(BackendNodeId::new(root_backend))
    } else if let Some(context) = locate_frame_context(page, request.frame_id.as_deref()).await? {
        let document = page
            .execute(
                EvaluateParams::builder()
                    .expression("document")
                    .context_id(context)
                    .object_group(SYNAPSE_LOCATE_OBJECT_GROUP)
                    .return_by_value(false)
                    .build()
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("locate role frame document evaluate params build: {err}"),
                    })?,
            )
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate role frame document evaluate: {err}"),
            })?
            .result;
        if let Some(exception) = document.exception_details.as_ref() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "locate role frame document evaluate threw: {}",
                    format_evaluate_exception(exception)
                ),
            });
        }
        let Some(object_id) = document.result.object_id else {
            return Err(A11yError::CdpAxtreeFailed {
                detail: "locate role frame document evaluate returned no objectId".to_owned(),
            });
        };
        builder.object_id(object_id)
    } else {
        let document = page
            .execute(GetDocumentParams::builder().depth(0).build())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate role DOM.getDocument: {err}"),
            })?;
        builder.node_id(document.result.root.node_id)
    };
    let nodes = page
        .execute(builder.build())
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Accessibility.queryAXTree(role={:?}): {err}", request.query),
        })?
        .result
        .nodes;

    let name_matcher = request
        .name
        .as_deref()
        .map(|name| NameMatcher::new(name, request.name_exact, request.name_regex))
        .transpose()?;

    let mut all: Vec<i64> = Vec::new();
    for node in nodes {
        if node.ignored && !request.include_hidden {
            continue;
        }
        let Some(backend) = node.backend_dom_node_id.as_ref().map(|id| *id.inner()) else {
            continue;
        };
        if let Some(matcher) = name_matcher.as_ref() {
            let actual = ax_value_to_string(node.name.as_ref());
            if !matcher.matches(&actual) {
                continue;
            }
        }
        if !ax_states_match(&node, request) {
            continue;
        }
        all.push(backend);
    }

    let match_count = all.len();
    let selected = apply_nth_and_limit(all, request.nth, request.limit);
    Ok((selected, match_count))
}

async fn locate_frame_context(
    page: &chromiumoxide::Page,
    frame_id: Option<&str>,
) -> A11yResult<Option<chromiumoxide::cdp::js_protocol::runtime::ExecutionContextId>> {
    use chromiumoxide::cdp::browser_protocol::page::{CreateIsolatedWorldParams, FrameId};

    let Some(frame_id) = frame_id
        .map(str::trim)
        .filter(|frame_id| !frame_id.is_empty())
    else {
        return Ok(None);
    };
    let context = page
        .execute(
            CreateIsolatedWorldParams::builder()
                .frame_id(FrameId::new(frame_id.to_owned()))
                .world_name(SYNAPSE_LOCATE_OBJECT_GROUP)
                .build()
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("locate frame {frame_id} isolated world params build: {err}"),
                })?,
        )
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("locate frame {frame_id} Page.createIsolatedWorld: {err}"),
        })?;
    Ok(Some(context.result.execution_context_id))
}

/// Accessible-name / attribute-text matcher mirroring the injected engine's
/// `stringMatcher`: regex (ECMA-ish), exact (whitespace-normalized,
/// case-sensitive), or default case-insensitive normalized substring.
#[derive(Debug)]
enum NameMatcher {
    Regex(regex::Regex),
    Exact(String),
    Substring(String),
}

impl NameMatcher {
    fn new(query: &str, exact: bool, is_regex: bool) -> A11yResult<Self> {
        if is_regex {
            let re = regex::Regex::new(query).map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("locate name regex {query:?} is invalid: {err}"),
            })?;
            Ok(Self::Regex(re))
        } else if exact {
            Ok(Self::Exact(normalize_ws(query)))
        } else {
            Ok(Self::Substring(normalize_ws(query).to_lowercase()))
        }
    }

    fn matches(&self, actual: &str) -> bool {
        match self {
            Self::Regex(re) => re.is_match(&normalize_ws(actual)),
            Self::Exact(want) => normalize_ws(actual) == *want,
            Self::Substring(want) => normalize_ws(actual).to_lowercase().contains(want),
        }
    }
}

/// Whitespace normalization identical to the injected engine's `norm`: drop
/// zero-width spaces / soft hyphens, collapse runs of whitespace to one space,
/// trim ends.
fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch == '\u{200b}' || ch == '\u{00ad}' {
            continue;
        }
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Reads an `AxValue`'s string payload (role/name), empty when absent.
fn ax_value_to_string(
    value: Option<&chromiumoxide::cdp::browser_protocol::accessibility::AxValue>,
) -> String {
    value
        .and_then(|value| value.value.as_ref())
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Reads a boolean-ish AX property (`true`/`false`/`mixed` tristate), `None`
/// when the property is absent. `mixed` is treated as not-`true`.
fn ax_bool_property(
    node: &chromiumoxide::cdp::browser_protocol::accessibility::AxNode,
    name: chromiumoxide::cdp::browser_protocol::accessibility::AxPropertyName,
) -> Option<bool> {
    let value = node
        .properties
        .as_ref()?
        .iter()
        .find(|prop| prop.name == name)?
        .value
        .value
        .as_ref()?;
    value
        .as_bool()
        .or_else(|| value.as_str().map(|raw| raw.eq_ignore_ascii_case("true")))
}

/// Reads the integer `aria-level` AX property, `None` when absent.
fn ax_level_property(
    node: &chromiumoxide::cdp::browser_protocol::accessibility::AxNode,
) -> Option<i64> {
    use chromiumoxide::cdp::browser_protocol::accessibility::AxPropertyName;
    let value = node
        .properties
        .as_ref()?
        .iter()
        .find(|prop| prop.name == AxPropertyName::Level)?
        .value
        .value
        .as_ref()?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
}

/// True when every requested ARIA state filter on `request` matches `node`.
/// An unrequested filter (`None`) is unconstrained; a missing boolean property
/// reads as `false`.
fn ax_states_match(
    node: &chromiumoxide::cdp::browser_protocol::accessibility::AxNode,
    request: &CdpLocateRequest,
) -> bool {
    use chromiumoxide::cdp::browser_protocol::accessibility::AxPropertyName;
    let bool_ok = |want: Option<bool>, name: AxPropertyName| -> bool {
        want.is_none_or(|want| ax_bool_property(node, name).unwrap_or(false) == want)
    };
    if !bool_ok(request.checked, AxPropertyName::Checked) {
        return false;
    }
    if !bool_ok(request.pressed, AxPropertyName::Pressed) {
        return false;
    }
    if !bool_ok(request.expanded, AxPropertyName::Expanded) {
        return false;
    }
    if !bool_ok(request.selected, AxPropertyName::Selected) {
        return false;
    }
    if !bool_ok(request.disabled, AxPropertyName::Disabled) {
        return false;
    }
    if let Some(level) = request.level
        && ax_level_property(node) != Some(level)
    {
        return false;
    }
    true
}

/// Applies an optional `nth` pick (0-based, negative counts from the end) and
/// then the `limit` cap to an ordered backend-id list.
fn apply_nth_and_limit(mut ids: Vec<i64>, nth: Option<i64>, limit: usize) -> Vec<i64> {
    if let Some(nth) = nth {
        let len = i64::try_from(ids.len()).unwrap_or(i64::MAX);
        let index = if nth < 0 { len + nth } else { nth };
        return match usize::try_from(index) {
            Ok(index) if index < ids.len() => vec![ids[index]],
            _ => Vec::new(),
        };
    }
    ids.truncate(limit);
    ids
}

/// Maps a `Runtime.RemoteObject` (from evaluate or callFunctionOn) plus the
/// page-state context into the structured [`CdpEvaluateResult`].
fn evaluate_result_from_remote(
    target_id: String,
    state: &CdpPageState,
    return_by_value: bool,
    remote: chromiumoxide::cdp::js_protocol::runtime::RemoteObject,
) -> CdpEvaluateResult {
    let result_type = remote_object_type_str(&remote.r#type);
    let result_subtype = remote
        .subtype
        .as_ref()
        .and_then(|subtype| serde_json::to_value(subtype).ok())
        .and_then(|value| value.as_str().map(ToOwned::to_owned));
    CdpEvaluateResult {
        target_id,
        url: state.url.clone(),
        title: state.title.clone(),
        ready_state: state.ready_state.clone(),
        result_type,
        result_subtype,
        returned_by_value: return_by_value,
        value: remote.value.unwrap_or(Value::Null),
        description: remote.description,
        unserializable_value: remote
            .unserializable_value
            .as_ref()
            .map(|raw| raw.inner().clone()),
    }
}

/// Renders a `Runtime.RemoteObject.type` enum to its protocol string.
fn remote_object_type_str(
    object_type: &chromiumoxide::cdp::js_protocol::runtime::RemoteObjectType,
) -> String {
    serde_json::to_value(object_type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Formats a `Runtime.ExceptionDetails` into a single, fully-detailed line so
/// the failure is actionable: the thrown class+message (from the RemoteObject
/// description when present, which includes the stack), and the source location.
fn format_evaluate_exception(
    exception: &chromiumoxide::cdp::js_protocol::runtime::ExceptionDetails,
) -> String {
    let mut detail = exception.text.clone();
    if let Some(thrown) = exception.exception.as_ref() {
        if let Some(description) = thrown.description.as_ref() {
            // For thrown Errors the description is "Name: message\n    at ...".
            detail = format!("{detail}: {description}");
        } else if let Some(value) = thrown.value.as_ref() {
            detail = format!("{detail}: {value}");
        }
    }
    format!(
        "{detail} (line {}, column {})",
        exception.line_number, exception.column_number
    )
}

/// Waits for a page lifecycle state on a specific CDP page target without
/// activating the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if lifecycle/network subscription or page-state
/// readback fails; `BROWSER_WAIT_TIMEOUT` if the requested state is not
/// observed within `wait_timeout_ms`.
pub async fn cdp_wait_for_load_state(
    endpoint: &str,
    target_id: &str,
    state: CdpLoadState,
    wait_timeout_ms: u64,
) -> A11yResult<CdpLoadStateWaitResult> {
    const NETWORK_IDLE_QUIET_MS: u64 = 500;

    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "CDP target id must not be empty".to_owned(),
        });
    }
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(PageEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.enable before waitForLoadState: {err}"),
            })?;
        page.execute(NetworkEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Network.enable before waitForLoadState: {err}"),
            })?;
        page.execute(SetLifecycleEventsEnabledParams::new(true))
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.setLifecycleEventsEnabled before waitForLoadState: {err}"),
            })?;

        let mut dom_content_loaded = page
            .event_listener::<EventDomContentEventFired>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.domContentEventFired: {err}"),
            })?;
        let mut load = page
            .event_listener::<EventLoadEventFired>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.loadEventFired: {err}"),
            })?;
        let mut lifecycle = page
            .event_listener::<EventLifecycleEvent>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.lifecycleEvent: {err}"),
            })?;
        let mut request_started = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.requestWillBeSent: {err}"),
            })?;
        let mut request_finished = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.loadingFinished: {err}"),
            })?;
        let mut request_failed = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.loadingFailed: {err}"),
            })?;

        let started = Instant::now();
        let deadline = std::time::Duration::from_millis(wait_timeout_ms);
        let quiet_budget = std::time::Duration::from_millis(NETWORK_IDLE_QUIET_MS);
        let mut observed_dom_content_loaded = false;
        let mut observed_load = false;
        let mut lifecycle_network_idle_seen = false;
        let mut event_count = 0u64;
        let mut network_event_count = 0u64;
        let mut in_flight = HashSet::<RequestId>::new();
        let mut max_in_flight_requests = 0usize;
        let mut last_network_activity = Instant::now();

        loop {
            let page_state = read_page_state(&page).await?;
            if cdp_ready_state_satisfies_load_state(state, &page_state.ready_state) {
                match state {
                    CdpLoadState::DomContentLoaded => observed_dom_content_loaded = true,
                    CdpLoadState::Load | CdpLoadState::NetworkIdle => observed_load = true,
                }
            }
            let quiet_for = last_network_activity.elapsed();
            if cdp_load_state_satisfied(
                state,
                &page_state.ready_state,
                observed_dom_content_loaded,
                observed_load,
                in_flight.len(),
                quiet_for,
            ) {
                return Ok(cdp_load_state_wait_result(
                    page.target_id().inner().clone(),
                    state,
                    &page_state,
                    started.elapsed(),
                    event_count,
                    network_event_count,
                    max_in_flight_requests,
                    in_flight.len(),
                    quiet_for,
                    lifecycle_network_idle_seen,
                ));
            }
            if started.elapsed() >= deadline {
                return Err(A11yError::BrowserWaitTimeout {
                    detail: format!(
                        "waitForLoadState({}) timed out after {wait_timeout_ms} ms; last url={:?} title={:?} readyState={:?}; event_count={event_count} network_event_count={network_event_count} in_flight_requests={} max_in_flight_requests={} network_idle_quiet_ms={} lifecycle_network_idle_seen={lifecycle_network_idle_seen}",
                        state.as_str(),
                        page_state.url,
                        page_state.title,
                        page_state.ready_state,
                        in_flight.len(),
                        max_in_flight_requests,
                        duration_millis_u64(quiet_for),
                    ),
                });
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            let next_quiet_check = if in_flight.is_empty() && quiet_for < quiet_budget {
                quiet_budget.saturating_sub(quiet_for)
            } else {
                std::time::Duration::from_millis(100)
            };
            let sleep_for = remaining
                .min(next_quiet_check)
                .min(std::time::Duration::from_millis(100));

            tokio::select! {
                Some(_) = dom_content_loaded.next() => {
                    observed_dom_content_loaded = true;
                    event_count = event_count.saturating_add(1);
                }
                Some(_) = load.next() => {
                    observed_load = true;
                    event_count = event_count.saturating_add(1);
                }
                Some(event) = lifecycle.next() => {
                    event_count = event_count.saturating_add(1);
                    match event.name.as_str() {
                        "DOMContentLoaded" => observed_dom_content_loaded = true,
                        "load" => observed_load = true,
                        "networkIdle" => lifecycle_network_idle_seen = true,
                        _ => {}
                    }
                }
                Some(event) = request_started.next() => {
                    network_event_count = network_event_count.saturating_add(1);
                    last_network_activity = Instant::now();
                    if event.redirect_response.is_some() {
                        in_flight.remove(&event.request_id);
                    }
                    in_flight.insert(event.request_id.clone());
                    max_in_flight_requests = max_in_flight_requests.max(in_flight.len());
                }
                Some(event) = request_finished.next() => {
                    network_event_count = network_event_count.saturating_add(1);
                    last_network_activity = Instant::now();
                    in_flight.remove(&event.request_id);
                }
                Some(event) = request_failed.next() => {
                    network_event_count = network_event_count.saturating_add(1);
                    last_network_activity = Instant::now();
                    in_flight.remove(&event.request_id);
                }
                _ = tokio::time::sleep(sleep_for) => {}
            }
        }
    }
    .await;

    handler_task.abort();
    result
}

// Assembles the load-state wait result from ten independent scalar observations
// (state, timings, in-flight/network counters) captured at the call site; bundling
// them into a params struct would only relocate the same fields.
#[allow(clippy::too_many_arguments)]
fn cdp_load_state_wait_result(
    target_id: String,
    state: CdpLoadState,
    page_state: &CdpPageState,
    elapsed: Duration,
    event_count: u64,
    network_event_count: u64,
    max_in_flight_requests: usize,
    in_flight_requests: usize,
    quiet_for: Duration,
    lifecycle_network_idle_seen: bool,
) -> CdpLoadStateWaitResult {
    CdpLoadStateWaitResult {
        target_id,
        requested_state: state.as_str().to_owned(),
        observed_state: state.as_str().to_owned(),
        elapsed_ms: duration_millis_u64(elapsed),
        event_count,
        network_event_count,
        max_in_flight_requests,
        in_flight_requests,
        network_idle_quiet_ms: duration_millis_u64(quiet_for),
        lifecycle_network_idle_seen,
        url: page_state.url.clone(),
        title: page_state.title.clone(),
        ready_state: page_state.ready_state.clone(),
    }
}

fn cdp_load_state_satisfied(
    state: CdpLoadState,
    ready_state: &str,
    observed_dom_content_loaded: bool,
    observed_load: bool,
    in_flight_requests: usize,
    quiet_for: Duration,
) -> bool {
    match state {
        CdpLoadState::DomContentLoaded => {
            observed_dom_content_loaded
                || cdp_ready_state_satisfies_load_state(CdpLoadState::DomContentLoaded, ready_state)
        }
        CdpLoadState::Load => {
            observed_load || cdp_ready_state_satisfies_load_state(CdpLoadState::Load, ready_state)
        }
        CdpLoadState::NetworkIdle => {
            cdp_ready_state_satisfies_load_state(CdpLoadState::Load, ready_state)
                && in_flight_requests == 0
                && quiet_for >= std::time::Duration::from_millis(500)
        }
    }
}

fn cdp_ready_state_satisfies_load_state(state: CdpLoadState, ready_state: &str) -> bool {
    match state {
        CdpLoadState::DomContentLoaded => matches!(ready_state, "interactive" | "complete"),
        CdpLoadState::Load | CdpLoadState::NetworkIdle => ready_state == "complete",
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Waits for a page target's current URL to match an exact string, glob, or
/// regex without activating the browser window.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the URL matcher or page-state readback fails;
/// `BROWSER_WAIT_TIMEOUT` if the URL does not match within `wait_timeout_ms`.
pub async fn cdp_wait_for_url(
    endpoint: &str,
    target_id: &str,
    pattern: &str,
    match_kind: CdpUrlMatchKind,
    wait_timeout_ms: u64,
    polling_interval_ms: u64,
) -> A11yResult<CdpUrlWaitResult> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "CDP target id must not be empty".to_owned(),
        });
    }
    let matcher = CdpUrlMatcher::new(pattern, match_kind)?;
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(PageEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.enable before waitForURL: {err}"),
            })?;
        let mut frame_navigated = page
            .event_listener::<EventFrameNavigated>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.frameNavigated: {err}"),
            })?;
        let mut same_document_navigated = page
            .event_listener::<EventNavigatedWithinDocument>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.navigatedWithinDocument: {err}"),
            })?;

        let started = Instant::now();
        let deadline = Duration::from_millis(wait_timeout_ms);
        let poll_interval = Duration::from_millis(polling_interval_ms.max(1));
        let mut poll_count = 0u64;
        let mut navigation_event_count = 0u64;
        let mut last_state: Option<CdpPageState> = None;
        let mut last_error: Option<String>;

        loop {
            poll_count = poll_count.saturating_add(1);
            match read_page_state(&page).await {
                Ok(page_state) => {
                    if matcher.matches(&page_state.url) {
                        let target_id = page.target_id().inner().clone();
                        return Ok(CdpUrlWaitResult {
                            target_id,
                            pattern: pattern.to_owned(),
                            match_kind: match_kind.as_str().to_owned(),
                            matched_url: page_state.url.clone(),
                            elapsed_ms: duration_millis_u64(started.elapsed()),
                            poll_count,
                            navigation_event_count,
                            url: page_state.url,
                            title: page_state.title,
                            ready_state: page_state.ready_state,
                        });
                    }
                    last_state = Some(page_state);
                    last_error = None;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                }
            }

            if started.elapsed() >= deadline {
                let state_detail = last_state
                    .as_ref()
                    .map(|state| {
                        format!(
                            "last url={:?} title={:?} readyState={:?}",
                            state.url, state.title, state.ready_state
                        )
                    })
                    .unwrap_or_else(|| "no page-state readback".to_owned());
                let error_detail = last_error
                    .as_deref()
                    .map(|error| format!("; last readback error={error}"))
                    .unwrap_or_default();
                return Err(A11yError::BrowserWaitTimeout {
                    detail: format!(
                        "waitForURL({} {:?}) timed out after {wait_timeout_ms} ms; {state_detail}; poll_count={poll_count} navigation_event_count={navigation_event_count}{error_detail}",
                        match_kind.as_str(),
                        pattern,
                    ),
                });
            }

            let remaining = deadline.saturating_sub(started.elapsed());
            let sleep_for = remaining.min(poll_interval);
            tokio::select! {
                Some(_) = frame_navigated.next() => {
                    navigation_event_count = navigation_event_count.saturating_add(1);
                }
                Some(_) = same_document_navigated.next() => {
                    navigation_event_count = navigation_event_count.saturating_add(1);
                }
                _ = tokio::time::sleep(sleep_for) => {}
            }
        }
    }
    .await;

    handler_task.abort();
    result
}

#[derive(Debug)]
enum CdpUrlMatcher {
    Exact(String),
    Glob(regex::Regex),
    Regex(regex::Regex),
}

impl CdpUrlMatcher {
    fn new(pattern: &str, match_kind: CdpUrlMatchKind) -> A11yResult<Self> {
        if pattern.is_empty() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: "waitForURL pattern must not be empty".to_owned(),
            });
        }
        match match_kind {
            CdpUrlMatchKind::Exact => Ok(Self::Exact(pattern.to_owned())),
            CdpUrlMatchKind::Glob => {
                let regex = cdp_url_glob_regex(pattern);
                regex::Regex::new(&regex).map(Self::Glob).map_err(|err| {
                    A11yError::CdpAxtreeFailed {
                        detail: format!(
                            "waitForURL glob {pattern:?} compiled to invalid regex {regex:?}: {err}"
                        ),
                    }
                })
            }
            CdpUrlMatchKind::Regex => regex::Regex::new(pattern).map(Self::Regex).map_err(|err| {
                A11yError::CdpAxtreeFailed {
                    detail: format!("waitForURL regex {pattern:?} is invalid: {err}"),
                }
            }),
        }
    }

    fn matches(&self, url: &str) -> bool {
        match self {
            Self::Exact(pattern) => url == pattern,
            Self::Glob(regex) | Self::Regex(regex) => regex.is_match(url),
        }
    }
}

fn cdp_url_glob_regex(glob: &str) -> String {
    let mut regex = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');
    regex
}

/// Navigates/reloads/history-navigates a specific CDP page target and returns a
/// separate post-command DOM/history readback for the same target.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the Page command fails, reports a navigation
/// error, history has no requested entry, or the readback never reaches a
/// stable loaded state within `wait_timeout_ms`.
pub async fn cdp_navigate_page_target(
    endpoint: &str,
    target_id: &str,
    action: CdpPageNavigationAction,
    url: Option<&str>,
    wait_timeout_ms: u64,
    ignore_cache: bool,
) -> A11yResult<CdpPageNavigationResult> {
    let requested_url = url.map(ToOwned::to_owned);
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let before = read_page_state(&page).await?;
        let mut navigation_error_text = None;
        let mut is_download = None;
        let mut readback_expectation = CdpPageReadbackExpectation::Stable;
        match action {
            CdpPageNavigationAction::Navigate => {
                let Some(url) = requested_url.as_deref() else {
                    return Err(A11yError::CdpAxtreeFailed {
                        detail: "Page.navigate requires a URL".to_owned(),
                    });
                };
                let navigated = page
                    .execute(NavigateParams::new(url.to_owned()))
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Page.navigate({url:?}): {err}"),
                    })?;
                navigation_error_text = navigated.result.error_text.clone();
                is_download = navigated.result.is_download;
                if navigation_error_text.is_none() && url != before.url {
                    readback_expectation = CdpPageReadbackExpectation::UrlChanged {
                        previous_url: before.url.clone(),
                    };
                }
            }
            CdpPageNavigationAction::Reload => {
                let reload = ReloadParams::builder().ignore_cache(ignore_cache).build();
                page.execute(reload)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Page.reload(ignoreCache={ignore_cache}): {err}"),
                    })?;
            }
            CdpPageNavigationAction::Back | CdpPageNavigationAction::Forward => {
                let history = page
                    .execute(GetNavigationHistoryParams::default())
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Page.getNavigationHistory before history action: {err}"),
                    })?
                    .result;
                let delta = if action == CdpPageNavigationAction::Back {
                    -1
                } else {
                    1
                };
                let target_index = history.current_index + delta;
                let Some(entry) = history
                    .entries
                    .get(usize::try_from(target_index).unwrap_or(usize::MAX))
                else {
                    return Err(A11yError::CdpAxtreeFailed {
                        detail: format!(
                            "Page.{} refused: currentIndex={} entries={}",
                            action.as_str(),
                            history.current_index,
                            history.entries.len()
                        ),
                    });
                };
                page.execute(NavigateToHistoryEntryParams::new(entry.id))
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("Page.navigateToHistoryEntry(entryId={}): {err}", entry.id),
                    })?;
                readback_expectation = CdpPageReadbackExpectation::HistoryEntry {
                    current_index: target_index,
                    url: entry.url.clone(),
                };
            }
        }
        let after = wait_for_page_readback(&page, wait_timeout_ms, &readback_expectation).await?;
        if let Some(error_text) = navigation_error_text.as_deref() {
            let url = requested_url.as_deref().unwrap_or("");
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Page.navigate({url:?}) failed: {error_text}; observed after readback url={:?} title={:?} readyState={:?} historyIndex={} historyEntries={} isDownload={:?}",
                    after.url,
                    after.title,
                    after.ready_state,
                    after.history_current_index,
                    after.history_entry_count,
                    is_download
                ),
            });
        }
        Ok(CdpPageNavigationResult {
            target_id,
            action: action.as_str().to_owned(),
            requested_url,
            before,
            after,
            navigation_error_text,
            is_download,
        })
    })
    .await
}

/// Replaces a CDP page target's main-frame document HTML and returns a
/// post-command page-state readback. Background-safe: this never activates the
/// tab or uses OS foreground input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the main frame cannot be resolved,
/// `Page.setDocumentContent` fails, or the page does not settle within
/// `wait_timeout_ms`.
pub async fn cdp_set_document_content_target(
    endpoint: &str,
    target_id: &str,
    html: &str,
    wait_timeout_ms: u64,
) -> A11yResult<CdpSetDocumentContentResult> {
    let html = html.to_owned();
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let before = read_page_state(&page).await?;
        let frame_id = page
            .mainframe()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page main frame readback before setDocumentContent: {err}"),
            })?
            .ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: "Page.setDocumentContent requires a main frame, but none was reported"
                    .to_owned(),
            })?;
        let frame_id_text = frame_id.inner().clone();
        page.execute(SetDocumentContentParams::new(frame_id, html.clone()))
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.setDocumentContent: {err}"),
            })?;
        let after =
            wait_for_page_readback(&page, wait_timeout_ms, &CdpPageReadbackExpectation::Stable)
                .await?;
        Ok(CdpSetDocumentContentResult {
            target_id,
            frame_id: frame_id_text,
            html_len: html.len(),
            before,
            after,
        })
    })
    .await
}

/// Scrolls a resolved web node into view with `DOM.scrollIntoViewIfNeeded` and
/// returns before/after Source-of-Truth readback for the viewport and nearest
/// scroll container. Background-safe: no tab activation and no OS foreground
/// input.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the node cannot be resolved, the scroll command
/// fails, or the post-scroll readback cannot be decoded.
pub async fn cdp_scroll_into_view_node(
    endpoint: &str,
    target_id: &str,
    backend_node_id: i64,
) -> A11yResult<CdpScrollIntoViewResult> {
    with_target_page(endpoint, target_id, |page| async move {
        let target_id = page.target_id().inner().clone();
        let before = read_scroll_into_view_snapshot(&page, backend_node_id).await?;
        if !before.is_connected {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!("backendNodeId {backend_node_id} resolved to a detached DOM node"),
            });
        }
        let scroll = ScrollIntoViewIfNeededParams::builder()
            .backend_node_id(BackendNodeId::new(backend_node_id))
            .build();
        page.execute(scroll)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "DOM.scrollIntoViewIfNeeded for backendNodeId {backend_node_id}: {err}"
                ),
            })?;
        let after = read_scroll_into_view_snapshot(&page, backend_node_id).await?;
        if !after.is_connected {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "backendNodeId {backend_node_id} detached after DOM.scrollIntoViewIfNeeded"
                ),
            });
        }
        Ok(CdpScrollIntoViewResult {
            target_id,
            backend_node_id,
            window_scroll_changed: scroll_value_changed(
                before.window_scroll_x,
                after.window_scroll_x,
            ) || scroll_value_changed(
                before.window_scroll_y,
                after.window_scroll_y,
            ),
            container_scroll_changed: container_scroll_changed(&before, &after),
            node_fully_in_viewport_after: after.node_fully_in_viewport,
            before,
            after,
        })
    })
    .await
}

/// Dispatches wheel events over a web node after scrolling it into view.
///
/// # Errors
///
/// As [`cdp_click_node`].
pub async fn cdp_scroll_node(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    deltas: Vec<CdpWheelDelta>,
    interval_ms: u32,
) -> A11yResult<CdpActionPoint> {
    with_node_page(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page| async move {
            let rect = node_content_rect(&page, backend_node_id).await?;
            let center = CdpActionPoint {
                x: f64::from(rect.x) + f64::from(rect.w) / 2.0,
                y: f64::from(rect.y) + f64::from(rect.h) / 2.0,
            };
            let last_index = deltas.len().saturating_sub(1);
            for (index, delta) in deltas.into_iter().enumerate() {
                let dispatch = dispatch_dom_scroll(&page, backend_node_id, delta).await?;
                tracing::debug!(
                    code = "A11Y_CDP_DOM_SCROLL_DISPATCHED",
                    backend_node_id,
                    delta_x = delta.delta_x,
                    delta_y = delta.delta_y,
                    target_tag = %dispatch.target_tag,
                    target_id = %dispatch.target_id,
                    default_prevented = dispatch.default_prevented,
                    before_left = dispatch.target_scroll_left_before,
                    before_top = dispatch.target_scroll_top_before,
                    after_left = dispatch.target_scroll_left_after,
                    after_top = dispatch.target_scroll_top_after,
                    "CDP Runtime.callFunctionOn dispatched background DOM scroll"
                );
                if interval_ms > 0 && index < last_index {
                    tokio::time::sleep(std::time::Duration::from_millis(u64::from(interval_ms)))
                        .await;
                }
            }
            Ok(center)
        },
    )
    .await
}

const SCROLL_INTO_VIEW_STATE_JS: &str = r#"function() {
    const node = this;
    const doc = (node && node.ownerDocument) || document;
    const win = doc.defaultView || window;
    const root = doc.scrollingElement || doc.documentElement || doc.body;
    function isElement(value) {
        return Boolean(value && value.nodeType === 1);
    }
    function isScrollable(element) {
        if (!isElement(element)) { return false; }
        const style = win.getComputedStyle(element);
        const overflowY = style.overflowY || '';
        const overflowX = style.overflowX || '';
        const canY = /(auto|scroll|overlay)/.test(overflowY)
            && element.scrollHeight > element.clientHeight;
        const canX = /(auto|scroll|overlay)/.test(overflowX)
            && element.scrollWidth > element.clientWidth;
        return canY || canX;
    }
    let container = isElement(node) ? node.parentElement : null;
    while (container && container !== root && !isScrollable(container)) {
        container = container.parentElement;
    }
    const target = (container && isScrollable(container)) ? container : root;
    const rect = node && node.getBoundingClientRect
        ? node.getBoundingClientRect()
        : { left: 0, top: 0, width: 0, height: 0, right: 0, bottom: 0 };
    const viewportWidth = Number(win.innerWidth || doc.documentElement.clientWidth || 0);
    const viewportHeight = Number(win.innerHeight || doc.documentElement.clientHeight || 0);
    const fullyInViewport = Boolean(
        node && node.isConnected &&
        rect.width > 0 && rect.height > 0 &&
        rect.left >= 0 && rect.top >= 0 &&
        rect.right <= viewportWidth &&
        rect.bottom <= viewportHeight
    );
    const rootScrollLeft = Number(win.scrollX || root.scrollLeft || 0);
    const rootScrollTop = Number(win.scrollY || root.scrollTop || 0);
    return {
        is_connected: Boolean(node && node.isConnected),
        viewport_width: viewportWidth,
        viewport_height: viewportHeight,
        node_rect: {
            x: Number(rect.left || 0),
            y: Number(rect.top || 0),
            width: Number(rect.width || 0),
            height: Number(rect.height || 0)
        },
        node_fully_in_viewport: fullyInViewport,
        window_scroll_x: rootScrollLeft,
        window_scroll_y: rootScrollTop,
        container: {
            is_root: target === root,
            tag_name: String(target && target.tagName || 'DOCUMENT'),
            id: String(target && target.id || ''),
            scroll_left: target === root ? rootScrollLeft : Number(target.scrollLeft || 0),
            scroll_top: target === root ? rootScrollTop : Number(target.scrollTop || 0),
            scroll_width: Number(target && target.scrollWidth || 0),
            scroll_height: Number(target && target.scrollHeight || 0),
            client_width: Number(target && target.clientWidth || viewportWidth),
            client_height: Number(target && target.clientHeight || viewportHeight)
        },
        box_model_content: null,
        box_model_error: null
    };
}"#;

async fn read_scroll_into_view_snapshot(
    page: &chromiumoxide::Page,
    backend_node_id: i64,
) -> A11yResult<CdpScrollIntoViewSnapshot> {
    let resolve = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .object_group("synapse_scroll_into_view")
        .build();
    let resolved = page
        .execute(resolve)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
        })?;
    let object_id =
        resolved
            .object
            .object_id
            .clone()
            .ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "resolveNode for backendNodeId {backend_node_id} returned no objectId"
                ),
            })?;
    let call = CallFunctionOnParams::builder()
        .function_declaration(SCROLL_INTO_VIEW_STATE_JS)
        .object_id(object_id)
        .object_group("synapse_scroll_into_view")
        .return_by_value(true)
        .silent(true)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Runtime.callFunctionOn scroll-into-view params: {err}"),
        })?;
    let mut snapshot: CdpScrollIntoViewSnapshot =
        call_function_on_value(page, call, "scroll-into-view").await?;
    match node_content_rect(page, backend_node_id).await {
        Ok(rect) => {
            snapshot.box_model_content = Some(CdpScrollIntoViewRect {
                x: f64::from(rect.x),
                y: f64::from(rect.y),
                width: f64::from(rect.w),
                height: f64::from(rect.h),
            })
        }
        Err(error) => snapshot.box_model_error = Some(error.to_string()),
    }
    Ok(snapshot)
}

fn container_scroll_changed(
    before: &CdpScrollIntoViewSnapshot,
    after: &CdpScrollIntoViewSnapshot,
) -> bool {
    scroll_value_changed(before.container.scroll_left, after.container.scroll_left)
        || scroll_value_changed(before.container.scroll_top, after.container.scroll_top)
}

fn scroll_value_changed(before: f64, after: f64) -> bool {
    (before - after).abs() > 0.25
}

async fn dispatch_dom_scroll(
    page: &chromiumoxide::Page,
    backend_node_id: i64,
    delta: CdpWheelDelta,
) -> A11yResult<CdpRuntimeScrollDispatch> {
    let resolve = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .object_group("synapse_scroll_dispatch")
        .build();
    let resolved = page
        .execute(resolve)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
        })?;
    let object_id =
        resolved
            .object
            .object_id
            .clone()
            .ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "resolveNode for backendNodeId {backend_node_id} returned no objectId"
                ),
            })?;
    let call = CallFunctionOnParams::builder()
        .function_declaration(
            r#"function(deltaX, deltaY) {
                const node = this;
                const doc = (node && node.ownerDocument) || document;
                const win = doc.defaultView || window;
                const root = doc.scrollingElement || doc.documentElement || doc.body;
                function isElement(value) {
                    return Boolean(value && value.nodeType === 1);
                }
                function isScrollable(element) {
                    if (!isElement(element)) { return false; }
                    const style = win.getComputedStyle(element);
                    const overflowY = style.overflowY || "";
                    const overflowX = style.overflowX || "";
                    const canY = /(auto|scroll|overlay)/.test(overflowY)
                        && element.scrollHeight > element.clientHeight;
                    const canX = /(auto|scroll|overlay)/.test(overflowX)
                        && element.scrollWidth > element.clientWidth;
                    return canY || canX;
                }
                let container = isElement(node) ? node : node.parentElement;
                while (container && container !== root && !isScrollable(container)) {
                    container = container.parentElement;
                }
                const target = (container && isScrollable(container)) ? container : root;
                const beforeLeft = Number(target.scrollLeft || win.scrollX || 0);
                const beforeTop = Number(target.scrollTop || win.scrollY || 0);
                const eventTarget = isElement(node) ? node : target;
                const event = new win.WheelEvent("wheel", {
                    deltaX,
                    deltaY,
                    bubbles: true,
                    cancelable: true,
                    view: win
                });
                const defaultAllowed = eventTarget.dispatchEvent(event);
                if (defaultAllowed) {
                    if (target === root) {
                        win.scrollBy(deltaX, deltaY);
                    } else if (typeof target.scrollBy === "function") {
                        target.scrollBy({ left: deltaX, top: deltaY, behavior: "auto" });
                    } else {
                        target.scrollLeft += deltaX;
                        target.scrollTop += deltaY;
                    }
                }
                return {
                    is_connected: Boolean(node && node.isConnected),
                    default_prevented: !defaultAllowed,
                    target_scroll_left_before: beforeLeft,
                    target_scroll_top_before: beforeTop,
                    target_scroll_left_after: Number(target.scrollLeft || win.scrollX || 0),
                    target_scroll_top_after: Number(target.scrollTop || win.scrollY || 0),
                    target_tag: String(target.tagName || "DOCUMENT"),
                    target_id: String(target.id || "")
                };
            }"#,
        )
        .object_id(object_id)
        .argument(CallArgument::builder().value(json!(delta.delta_x)).build())
        .argument(CallArgument::builder().value(json!(delta.delta_y)).build())
        .return_by_value(true)
        .silent(true)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Runtime.callFunctionOn scroll-dispatch params: {err}"),
        })?;
    let dispatch: CdpRuntimeScrollDispatch =
        call_function_on_value(page, call, "scroll-dispatch").await?;
    if !dispatch.is_connected {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("backendNodeId {backend_node_id} resolved to a detached DOM node"),
        });
    }
    Ok(dispatch)
}

/// Reads a web node's semantic value/text via CDP after resolving the exact
/// backend node into a JavaScript object.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the node cannot be resolved or evaluated.
pub async fn cdp_node_value(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<String> {
    with_node_page(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page| async move {
            let resolve = ResolveNodeParams::builder()
                .backend_node_id(BackendNodeId::new(backend_node_id))
                .object_group("synapse_verify_delta")
                .build();
            let resolved =
                page.execute(resolve)
                    .await
                    .map_err(|err| A11yError::CdpAxtreeFailed {
                        detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
                    })?;
            let object_id =
                resolved
                    .object
                    .object_id
                    .clone()
                    .ok_or_else(|| A11yError::CdpAxtreeFailed {
                        detail: format!(
                            "resolveNode for backendNodeId {backend_node_id} returned no objectId"
                        ),
                    })?;
            let call = CallFunctionOnParams::builder()
                .function_declaration(
                    "function() {
                    if (this === null || this === undefined) { return ''; }
                    if ('value' in this) { return String(this.value ?? ''); }
                    if ('checked' in this) { return String(Boolean(this.checked)); }
                    if (this.isContentEditable && this.innerText !== null && this.innerText !== undefined) {
                        return String(this.innerText).replace(/\\n$/, '');
                    }
                    if (this.textContent !== null && this.textContent !== undefined) {
                        return String(this.textContent);
                    }
                    return '';
                }",
                )
                .object_id(object_id)
                .return_by_value(true)
                .silent(true)
                .build()
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("build Runtime.callFunctionOn params: {err}"),
                })?;
            call_function_on_value(&page, call, "value").await
        },
    )
    .await
}

/// Reads the DOM scroll state attached to a web node's nearest scroll container.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if the node cannot be resolved or is detached.
pub async fn cdp_node_scroll_state(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<CdpScrollState> {
    with_node_page(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page| async move {
            let state = read_node_scroll_state(&page, backend_node_id).await?;
            if !state.is_connected {
                return Err(A11yError::CdpAxtreeFailed {
                    detail: format!(
                        "backendNodeId {backend_node_id} resolved to a detached DOM node"
                    ),
                });
            }
            Ok(state)
        },
    )
    .await
}

/// Resolves the viewport-CSS centre of a web node (for `act_stroke` target
/// aiming), scrolling it into view first.
///
/// # Errors
///
/// As [`cdp_click_node`].
pub async fn cdp_node_viewport_center(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<CdpActionPoint> {
    with_node_center(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |_page, center| async move { Ok(center) },
    )
    .await
}

/// Moves the in-page CDP pointer over a web node after scrolling it into view.
///
/// # Errors
///
/// As [`cdp_click_node`].
pub async fn cdp_aim_node(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<CdpActionPoint> {
    with_node_center(
        endpoint,
        page_title_hint,
        target_id_hint,
        backend_node_id,
        |page, center| async move {
            page.execute(mouse_event(
                DispatchMouseEventType::MouseMoved,
                center,
                MouseButton::None,
                0,
            ))
            .await
            .map_err(|err| dispatch_err(&err))?;
            Ok(center)
        },
    )
    .await
}

fn validate_cdp_mouse_stroke_points(points: &[CdpMouseStrokePoint]) -> A11yResult<()> {
    if points.is_empty() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: "cdp_mouse_stroke_target requires at least one point".to_owned(),
        });
    }
    let mut previous_elapsed_ms = 0.0;
    for (index, point) in points.iter().enumerate() {
        if !point.x.is_finite() || !point.y.is_finite() || !point.elapsed_ms.is_finite() {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "cdp mouse stroke point {index} must contain finite x/y/elapsed_ms values"
                ),
            });
        }
        if point.elapsed_ms < 0.0 {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "cdp mouse stroke point {index} elapsed_ms must be >= 0, got {}",
                    point.elapsed_ms
                ),
            });
        }
        if index > 0 && point.elapsed_ms < previous_elapsed_ms {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "cdp mouse stroke point {index} elapsed_ms {} is before prior sample {}",
                    point.elapsed_ms, previous_elapsed_ms
                ),
            });
        }
        previous_elapsed_ms = point.elapsed_ms;
    }
    Ok(())
}

async fn dispatch_cdp_mouse_stroke_raw(
    endpoint: &str,
    target_id: &str,
    points: &[CdpMouseStrokePoint],
    button: MouseButton,
) -> A11yResult<()> {
    let ws_url = cdp_page_websocket_url(endpoint, target_id)?;
    let connect = tokio_tungstenite::connect_async(ws_url.as_str());
    let (mut socket, _response) = tokio::time::timeout(CDP_INPUT_COMMAND_TIMEOUT, connect)
        .await
        .map_err(|_| A11yError::CdpAttachFailed {
            detail: format!(
                "CDP page WebSocket connect timed out after {} ms for target {target_id}",
                CDP_INPUT_COMMAND_TIMEOUT.as_millis()
            ),
        })?
        .map_err(|err| A11yError::CdpAttachFailed {
            detail: format!("connect CDP page WebSocket {ws_url}: {err}"),
        })?;
    let mut command_id = 1_u64;
    let first = points[0];
    let first_point = cdp_stroke_action_point(first);
    if button == MouseButton::None {
        let mut previous_elapsed_ms = first.elapsed_ms;
        send_raw_mouse_event(
            &mut socket,
            &mut command_id,
            DispatchMouseEventType::MouseMoved,
            first_point,
            MouseButton::None,
            0,
            0,
            "move",
            0,
        )
        .await?;
        for (index, point) in points.iter().enumerate().skip(1) {
            sleep_until_sample(previous_elapsed_ms, point.elapsed_ms).await;
            previous_elapsed_ms = point.elapsed_ms;
            send_raw_mouse_event(
                &mut socket,
                &mut command_id,
                DispatchMouseEventType::MouseMoved,
                cdp_stroke_action_point(*point),
                MouseButton::None,
                0,
                0,
                "move",
                index,
            )
            .await?;
        }
        settle_and_close_raw_input_socket(&mut socket).await;
        return Ok(());
    }

    send_raw_mouse_event(
        &mut socket,
        &mut command_id,
        DispatchMouseEventType::MouseMoved,
        first_point,
        button.clone(),
        0,
        0,
        "pre_press_move",
        0,
    )
    .await?;
    dispatch_raw_mouse_event(
        &mut socket,
        &mut command_id,
        DispatchMouseEventType::MousePressed,
        first_point,
        button.clone(),
        Some(mouse_button_bit(&button)),
        1,
        "press",
        0,
    )
    .await?;

    let held_buttons = mouse_button_bit(&button);
    let mut previous_elapsed_ms = first.elapsed_ms;
    for (index, point) in points.iter().enumerate().skip(1) {
        sleep_until_sample(previous_elapsed_ms, point.elapsed_ms).await;
        previous_elapsed_ms = point.elapsed_ms;
        dispatch_raw_mouse_event(
            &mut socket,
            &mut command_id,
            DispatchMouseEventType::MouseMoved,
            cdp_stroke_action_point(*point),
            button.clone(),
            Some(held_buttons),
            0,
            "drag_move",
            index,
        )
        .await?;
    }
    dispatch_raw_mouse_event(
        &mut socket,
        &mut command_id,
        DispatchMouseEventType::MouseReleased,
        points
            .last()
            .map_or(first_point, |point| cdp_stroke_action_point(*point)),
        button,
        Some(0),
        1,
        "release",
        points.len().saturating_sub(1),
    )
    .await?;
    settle_and_close_raw_input_socket(&mut socket).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_raw_mouse_event(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    command_id: &mut u64,
    event_type: DispatchMouseEventType,
    point: CdpActionPoint,
    button: MouseButton,
    buttons: Option<i64>,
    click_count: i64,
    stage: &'static str,
    sample_index: usize,
) -> A11yResult<()> {
    let button_bits = buttons.unwrap_or_else(|| mouse_button_bit(&button));
    let payload = raw_mouse_event_message(
        *command_id,
        event_type,
        point,
        button,
        button_bits,
        click_count,
    );
    *command_id = command_id.saturating_add(1);
    send_raw_mouse_event_payload(socket, payload, stage, sample_index).await
}

#[allow(clippy::too_many_arguments)]
async fn send_raw_mouse_event(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    command_id: &mut u64,
    event_type: DispatchMouseEventType,
    point: CdpActionPoint,
    button: MouseButton,
    buttons: i64,
    click_count: i64,
    stage: &'static str,
    sample_index: usize,
) -> A11yResult<()> {
    dispatch_raw_mouse_event(
        socket,
        command_id,
        event_type,
        point,
        button,
        Some(buttons),
        click_count,
        stage,
        sample_index,
    )
    .await
}

async fn send_raw_mouse_event_payload(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    payload: Value,
    stage: &'static str,
    sample_index: usize,
) -> A11yResult<()> {
    let text = serde_json::to_string(&payload).map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!(
            "serialize raw CDP Input.dispatchMouseEvent at stage {stage} sample_index={sample_index}: {err}"
        ),
    })?;
    let send = socket.send(Message::Text(text.into()));
    match tokio::time::timeout(CDP_INPUT_COMMAND_TIMEOUT, send).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "CDP raw input send failed at stage {stage} sample_index={sample_index}: {err}"
            ),
        }),
        Err(_) => Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "CDP raw input send timed out after {} ms at stage {stage} sample_index={sample_index}",
                CDP_INPUT_COMMAND_TIMEOUT.as_millis()
            ),
        }),
    }
}

async fn settle_and_close_raw_input_socket(
    socket: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), socket.close(None)).await;
}

fn cdp_page_websocket_url(endpoint: &str, target_id: &str) -> A11yResult<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let target_id = target_id.trim();
    if endpoint.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "CDP endpoint must not be empty".to_owned(),
        });
    }
    if target_id.is_empty() || target_id.contains('/') || target_id.contains('\\') {
        return Err(A11yError::CdpAttachFailed {
            detail: format!("CDP target id is not safe for a page WebSocket URL: {target_id:?}"),
        });
    }
    let base = if let Some(rest) = endpoint.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = endpoint.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        endpoint.to_owned()
    } else {
        return Err(A11yError::CdpAttachFailed {
            detail: format!(
                "CDP endpoint {endpoint:?} must start with http://, https://, ws://, or wss://"
            ),
        });
    };
    Ok(format!("{base}/devtools/page/{target_id}"))
}

fn raw_mouse_event_message(
    command_id: u64,
    event_type: DispatchMouseEventType,
    point: CdpActionPoint,
    button: MouseButton,
    buttons: i64,
    click_count: i64,
) -> Value {
    json!({
        "id": command_id,
        "method": "Input.dispatchMouseEvent",
        "params": {
            "type": event_type.as_ref(),
            "x": point.x,
            "y": point.y,
            "button": mouse_button_wire(&button),
            "buttons": buttons,
            "clickCount": click_count,
        },
    })
}

fn mouse_button_wire(button: &MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
        _ => "none",
    }
}

async fn sleep_until_sample(previous_elapsed_ms: f64, next_elapsed_ms: f64) {
    let delta_ms = (next_elapsed_ms - previous_elapsed_ms).max(0.0);
    if delta_ms > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(delta_ms / 1000.0)).await;
    }
}

fn cdp_stroke_action_point(point: CdpMouseStrokePoint) -> CdpActionPoint {
    CdpActionPoint {
        x: point.x,
        y: point.y,
    }
}

/// A decoded, top-down BGRA8 bitmap captured from a web node via CDP (#703).
///
/// `bgra` is 4 bytes per pixel with no row padding, sized `width * height * 4`,
/// ready for the `WinRT` OCR `read_text_from_bgra_bitmap` path.
#[derive(Clone, Debug)]
pub struct CdpNodeBitmap {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Captures just a web node's rendered pixels and returns them as a BGRA8 bitmap
/// for OCR (#703).
///
/// Mirrors how clicks resolve a node — attach, find the owning page, scroll the
/// node into view, resolve its live box model — then converts the viewport-CSS
/// box to document coordinates (using `Page.getLayoutMetrics` scroll offset) and
/// captures exactly that element via `Page.captureScreenshot { clip,
/// captureBeyondViewport:true }`. This is DPI-/scroll-/occlusion-robust and
/// needs no CSS→screen mapping (which the click path also deliberately avoids).
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/node cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if box-model resolution, layout metrics, capture, or
/// PNG decode fails.
pub async fn cdp_capture_node_bgra(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<CdpNodeBitmap> {
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page =
            resolve_owning_page(&browser, page_title_hint, target_id_hint, backend_node_id).await?;
        let rect = node_content_rect(&page, backend_node_id).await?;
        // getBoxModel is viewport-relative (the click path dispatches at its
        // centre as viewport coords and lands correctly); captureScreenshot with
        // captureBeyondViewport=true expects document coords, so add the scroll
        // offset. Chrome's own "Capture node screenshot" uses the same shape.
        let metrics = page
            .execute(GetLayoutMetricsParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("getLayoutMetrics: {err}"),
            })?;
        let scroll_x = i32::try_from(metrics.result.css_layout_viewport.page_x).unwrap_or(0);
        let scroll_y = i32::try_from(metrics.result.css_layout_viewport.page_y).unwrap_or(0);
        let clip = Viewport {
            x: f64::from(rect.x) + f64::from(scroll_x),
            y: f64::from(rect.y) + f64::from(scroll_y),
            width: f64::from(rect.w),
            height: f64::from(rect.h),
            scale: 1.0,
        };
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .clip(clip)
            .from_surface(true)
            .capture_beyond_viewport(true)
            .build();
        let png_bytes =
            page.screenshot(params)
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("captureScreenshot: {err}"),
                })?;
        decode_png_to_bgra(&png_bytes)
    }
    .await;

    handler_task.abort();
    result
}

/// Captures a CDP page target's viewport, or a viewport clip, as a BGRA8 bitmap.
///
/// This is target-specific `Page.captureScreenshot`: no OS foreground, no
/// window downgrade, and no screen-coordinate capture. `region`, when present,
/// is interpreted as CSS viewport coordinates for the page target.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if capture or PNG decode fails.
pub async fn cdp_capture_page_bgra(
    endpoint: &str,
    target_id: &str,
    region: Option<synapse_core::Rect>,
) -> A11yResult<CdpNodeBitmap> {
    if let Some(region) = region
        && (region.w <= 0 || region.h <= 0)
    {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "Page.captureScreenshot clip must be non-empty: bbox=({}, {}, {}, {})",
                region.x, region.y, region.w, region.h
            ),
        });
    }
    with_target_page(endpoint, target_id, |page| async move {
        let mut params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .from_surface(true);
        if let Some(region) = region {
            params = params.clip(Viewport {
                x: f64::from(region.x),
                y: f64::from(region.y),
                width: f64::from(region.w),
                height: f64::from(region.h),
                scale: 1.0,
            });
        }
        let png_bytes =
            page.screenshot(params.build())
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Page.captureScreenshot: {err}"),
                })?;
        decode_png_to_bgra(&png_bytes)
    })
    .await
}

fn mouse_event(
    kind: DispatchMouseEventType,
    point: CdpActionPoint,
    button: MouseButton,
    click_count: i64,
) -> DispatchMouseEventParams {
    mouse_event_with_modifiers(kind, point, button, click_count, 0)
}

fn mouse_event_with_modifiers(
    kind: DispatchMouseEventType,
    point: CdpActionPoint,
    button: MouseButton,
    click_count: i64,
    modifiers: i64,
) -> DispatchMouseEventParams {
    mouse_event_with_buttons(kind, point, button, click_count, None, modifiers)
}

fn mouse_event_with_buttons(
    kind: DispatchMouseEventType,
    point: CdpActionPoint,
    button: MouseButton,
    click_count: i64,
    buttons_override: Option<i64>,
    modifiers: i64,
) -> DispatchMouseEventParams {
    // `buttons` is the bitmask of buttons CURRENTLY held: the button's bit while
    // pressed, 0 once moved or released. Getting this wrong (e.g. leaving the
    // bit set on release) makes Chrome think the button is still down and it
    // never synthesises a `click` event.
    let is_pressed = matches!(kind, DispatchMouseEventType::MousePressed);
    let bit = mouse_button_bit(&button);
    let mut params = DispatchMouseEventParams::new(kind, point.x, point.y);
    params.click_count = Some(click_count);
    params.buttons = Some(buttons_override.unwrap_or(if is_pressed { bit } else { 0 }));
    params.button = Some(button);
    params.modifiers = Some(modifiers);
    params
}

fn touch_event(
    kind: DispatchTouchEventType,
    point: Option<CdpActionPoint>,
) -> A11yResult<DispatchTouchEventParams> {
    let touch_points = match point {
        Some(point) => vec![touch_point(point)?],
        None => Vec::new(),
    };
    Ok(DispatchTouchEventParams::new(kind, touch_points))
}

fn touch_point(point: CdpActionPoint) -> A11yResult<TouchPoint> {
    validate_cdp_action_point(point, "touch point")?;
    TouchPoint::builder()
        .x(point.x)
        .y(point.y)
        .id(1.0)
        .radius_x(1.0)
        .radius_y(1.0)
        .force(1.0)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Input.dispatchTouchEvent touch point: {err}"),
        })
}

fn validate_cdp_action_point(point: CdpActionPoint, label: &str) -> A11yResult<()> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "CDP {label} requires finite viewport coordinates, got x={} y={}",
                point.x, point.y
            ),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize)]
struct CdpTouchInputState {
    max_touch_points: i64,
    ontouchstart_available: bool,
}

async fn dispatch_touch_tap_on_page(
    page: &chromiumoxide::Page,
    point: CdpActionPoint,
) -> A11yResult<CdpTouchTapResult> {
    validate_cdp_action_point(point, "touch tap")?;
    let touch_state = read_touch_input_state(page).await.unwrap_or_default();
    page.execute(touch_event(
        DispatchTouchEventType::TouchStart,
        Some(point),
    )?)
    .await
    .map_err(|err| dispatch_err(&err))?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    page.execute(touch_event(DispatchTouchEventType::TouchEnd, None)?)
        .await
        .map_err(|err| dispatch_err(&err))?;
    Ok(CdpTouchTapResult {
        target_id: page.target_id().inner().clone(),
        point,
        dispatched_events: vec!["touchStart".to_owned(), "touchEnd".to_owned()],
        max_touch_points: touch_state.max_touch_points,
        ontouchstart_available: touch_state.ontouchstart_available,
        touch_emulation_detected: touch_state.max_touch_points > 0
            || touch_state.ontouchstart_available,
        non_touch_fallback:
            "none; use mouse click explicitly when touch semantics are not required".to_owned(),
    })
}

async fn read_touch_input_state(page: &chromiumoxide::Page) -> A11yResult<CdpTouchInputState> {
    page.evaluate_expression(
        r#"(() => ({
            max_touch_points: Number(navigator.maxTouchPoints || 0),
            ontouchstart_available: Boolean("ontouchstart" in window)
        }))()"#,
    )
    .await
    .map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("Runtime.evaluate touch input state: {err}"),
    })?
    .into_value::<CdpTouchInputState>()
    .map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("Runtime.evaluate touch input state decode: {err}"),
    })
}

fn mouse_button_bit(button: &MouseButton) -> i64 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        _ => 0,
    }
}

fn dispatch_err(err: &chromiumoxide::error::CdpError) -> A11yError {
    A11yError::CdpAxtreeFailed {
        detail: format!("CDP input dispatch failed: {err}"),
    }
}

fn cdp_key_event(
    event_type: DispatchKeyEventType,
    key: &CdpKeyStroke,
    modifiers: i64,
) -> A11yResult<DispatchKeyEventParams> {
    let mut builder = DispatchKeyEventParams::builder()
        .r#type(event_type)
        .modifiers(modifiers)
        .key(key.key.clone())
        .code(key.code.clone())
        .windows_virtual_key_code(key.windows_virtual_key_code)
        .native_virtual_key_code(key.native_virtual_key_code);
    if let Some(value) = &key.key_identifier {
        builder = builder.key_identifier(value.clone());
    }
    if let Some(value) = &key.text {
        builder = builder.text(value.clone());
    }
    if let Some(value) = &key.unmodified_text {
        builder = builder.unmodified_text(value.clone());
    }
    if let Some(value) = key.location {
        builder = builder.location(value);
    }
    builder.build().map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("build Input.dispatchKeyEvent params: {err}"),
    })
}

async fn wait_for_page_readback(
    page: &chromiumoxide::Page,
    wait_timeout_ms: u64,
    expectation: &CdpPageReadbackExpectation,
) -> A11yResult<CdpPageState> {
    let started = tokio::time::Instant::now();
    let budget = std::time::Duration::from_millis(wait_timeout_ms);
    let mut last_state: Option<CdpPageState> = None;
    let mut last_error: Option<String> = None;
    loop {
        match read_page_state(page).await {
            Ok(state) => {
                let loaded = state.ready_state == "complete" || state.ready_state == "interactive";
                if loaded && expectation.matches(&state) {
                    return Ok(state);
                }
                last_state = Some(state);
            }
            Err(err) => {
                last_error = Some(err.to_string());
            }
        }
        if started.elapsed() >= budget {
            if let Some(state) = last_state {
                return Err(A11yError::CdpAxtreeFailed {
                    detail: format!(
                        "page readback did not settle within {wait_timeout_ms} ms waiting for {}; last url={:?} title={:?} readyState={:?} historyIndex={} historyEntries={}",
                        expectation.detail(),
                        state.url,
                        state.title,
                        state.ready_state,
                        state.history_current_index,
                        state.history_entry_count
                    ),
                });
            }
            if let Some(error) = last_error {
                return Err(A11yError::CdpAxtreeFailed {
                    detail: format!(
                        "page readback did not settle within {wait_timeout_ms} ms; last readback error: {error}"
                    ),
                });
            }
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "page readback did not settle within {wait_timeout_ms} ms; no page state readback"
                ),
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn read_page_state(page: &chromiumoxide::Page) -> A11yResult<CdpPageState> {
    #[derive(Debug, Deserialize)]
    struct DomState {
        url: String,
        title: String,
        ready_state: String,
    }

    let dom = page
        .evaluate_expression(
            r#"(() => ({
                url: String(location.href || ""),
                title: String(document.title || ""),
                ready_state: String(document.readyState || "")
            }))()"#,
        )
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.evaluate page readback: {err}"),
        })?
        .into_value::<DomState>()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.evaluate page readback decode: {err}"),
        })?;
    let history = page
        .execute(GetNavigationHistoryParams::default())
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Page.getNavigationHistory readback: {err}"),
        })?
        .result;
    Ok(CdpPageState {
        url: dom.url,
        title: dom.title,
        ready_state: dom.ready_state,
        history_current_index: history.current_index,
        history_entry_count: u32::try_from(history.entries.len()).unwrap_or(u32::MAX),
    })
}

/// Polls `browser.pages()` until target discovery surfaces at least one page
/// (fresh connections discover targets asynchronously), up to ~3s.
async fn wait_for_pages(browser: &chromiumoxide::Browser) -> A11yResult<Vec<chromiumoxide::Page>> {
    for _ in 0..30 {
        match browser.pages().await {
            Ok(pages) if !pages.is_empty() => return Ok(pages),
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            Err(err) => {
                return Err(A11yError::CdpAttachFailed {
                    detail: format!("list pages: {err}"),
                });
            }
        }
    }
    Err(A11yError::CdpAttachFailed {
        detail: "no page targets became available within 3s".to_owned(),
    })
}

/// Attaches, finds the page owning `backend_node_id`, scrolls it into view,
/// resolves its box-model centre, runs `action(page, center)`, and tears down.
async fn with_node_center<A, Fut, T>(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    action: A,
) -> A11yResult<T>
where
    A: FnOnce(chromiumoxide::Page, CdpActionPoint) -> Fut,
    Fut: std::future::Future<Output = A11yResult<T>>,
{
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page =
            resolve_owning_page(&browser, page_title_hint, target_id_hint, backend_node_id).await?;
        let rect = node_content_rect(&page, backend_node_id).await?;
        let center = CdpActionPoint {
            x: f64::from(rect.x) + f64::from(rect.w) / 2.0,
            y: f64::from(rect.y) + f64::from(rect.h) / 2.0,
        };
        action(page, center).await
    }
    .await;

    handler_task.abort();
    result
}

async fn with_node_page<A, Fut, T>(
    endpoint: &str,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    action: A,
) -> A11yResult<T>
where
    A: FnOnce(chromiumoxide::Page) -> Fut,
    Fut: std::future::Future<Output = A11yResult<T>>,
{
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page =
            resolve_owning_page(&browser, page_title_hint, target_id_hint, backend_node_id).await?;
        action(page).await
    }
    .await;

    handler_task.abort();
    result
}

async fn with_target_page<A, Fut, T>(endpoint: &str, target_id: &str, action: A) -> A11yResult<T>
where
    A: FnOnce(chromiumoxide::Page) -> Fut,
    Fut: std::future::Future<Output = A11yResult<T>>,
{
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "CDP target id must not be empty".to_owned(),
        });
    }
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = get_target_page_with_discovery(&browser, target_id).await?;
        prime_target_page_for_input(&page, target_id).await?;
        action(page).await
    }
    .await;

    handler_task.abort();
    result
}

/// Resolves a CDP target id to its [`chromiumoxide::Page`], tolerating the
/// asynchronous target discovery that follows every fresh `Browser::connect`.
///
/// Root cause of #982: each action opens a fresh CDP connection, and a freshly
/// connected browser populates its target map from
/// `Target.targetCreated`/`attachedToTarget` events processed on the handler
/// task. The FIRST `get_page` after connect therefore routinely races ahead of
/// discovery and returns `NotFound`, surfacing as
/// "selected target … is no longer present for backendNodeId N" for EVERY CDP
/// web element — top-level or iframe — not just nested frames. `observe` never
/// hit this because it waits via [`wait_for_pages`] and retries `get_page`
/// (`wait_for_page_target`); the action path did a single unguarded `get_page`.
///
/// This mirrors observe: prime discovery with [`wait_for_pages`], then retry
/// `get_page` on a bounded schedule. If the target id names an out-of-process
/// iframe child target (its own `targetId`), a plain `get_page` never resolves
/// until flat auto-attach exposes a callable session, so after a few failed
/// attempts we enable flat iframe auto-attach on the discovered pages — exactly
/// what observe does before reading OOPIF DOM.
async fn get_page_with_discovery(
    browser: &chromiumoxide::Browser,
    target_id: &str,
    backend_node_id: i64,
) -> A11yResult<chromiumoxide::Page> {
    get_target_page_with_discovery(browser, target_id)
        .await
        .map_err(|error| match error {
            A11yError::CdpAxtreeFailed { detail } => A11yError::CdpAxtreeFailed {
                detail: format!("{detail} for backendNodeId {backend_node_id}"),
            },
            other => other,
        })
}

pub(crate) async fn get_target_page_with_discovery(
    browser: &chromiumoxide::Browser,
    target_id: &str,
) -> A11yResult<chromiumoxide::Page> {
    // Block until target discovery surfaces at least one page (or the endpoint
    // is unreachable) so the retry loop below is not racing an empty map.
    let pages = wait_for_pages(browser).await?;
    let mut last_error: Option<String> = None;
    let mut auto_attach_enabled = false;
    for attempt in 0..30u32 {
        match browser.get_page(TargetId::new(target_id.to_owned())).await {
            Ok(page) => return Ok(page),
            Err(error) => last_error = Some(error.to_string()),
        }
        // The common in-process case (target id == page target) resolves on an
        // early attempt once discovery lands. If it has not resolved after a few
        // tries the id likely names an OOPIF child target that needs flat
        // auto-attach to expose a session; enable it once, then keep retrying.
        if attempt == 4 && !auto_attach_enabled {
            for page in &pages {
                let _ = crate::cdp_dom::enable_flat_iframe_auto_attach(page).await;
            }
            auto_attach_enabled = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let discovered = pages
        .iter()
        .map(|page| page.target_id().inner().clone())
        .collect::<Vec<_>>()
        .join(",");
    Err(A11yError::CdpAxtreeFailed {
        detail: format!(
            "selected target {target_id} is no longer present after waiting for CDP target discovery (auto_attach_enabled={auto_attach_enabled}; {} page target(s) discovered: [{discovered}]); last get_page error: {}",
            pages.len(),
            last_error.unwrap_or_else(|| "none".to_owned())
        ),
    })
}

async fn prime_target_page_for_input(
    page: &chromiumoxide::Page,
    target_id: &str,
) -> A11yResult<()> {
    use chromiumoxide::cdp::browser_protocol::dom::GetDocumentParams;

    let document = GetDocumentParams::builder().depth(0).pierce(true).build();
    page.execute(document)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!(
                "DOM.getDocument before Input.dispatchMouseEvent for target {target_id}: {err}"
            ),
        })?;
    Ok(())
}

/// Finds the attached page that owns `backend_node_id`, priming each candidate's
/// DOM and confirming ownership by scrolling the node into view.
///
/// Backend node ids are per-DOCUMENT, so the same numeric id can exist in
/// several tabs. Candidate pages whose title matches the foreground window (the
/// tab `observe` read) are tried first. A fresh CDP connection discovers targets
/// asynchronously and has not been pushed each page's DOM, so we poll for pages
/// and prime with `DOM.getDocument` before resolving (required, not optional).
async fn resolve_owning_page(
    browser: &chromiumoxide::Browser,
    page_title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> A11yResult<chromiumoxide::Page> {
    if let Some(target_id_hint) = target_id_hint.filter(|hint| !hint.trim().is_empty()) {
        let target_id_hint = target_id_hint.trim();
        let page = get_page_with_discovery(browser, target_id_hint, backend_node_id).await?;
        return if page_owns_backend_node(&page, backend_node_id).await {
            Ok(page)
        } else {
            Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "selected target {target_id_hint} does not own backendNodeId {backend_node_id}"
                ),
            })
        };
    }

    let pages = wait_for_pages(browser).await?;
    let mut ordered = Vec::with_capacity(pages.len());
    let mut tail = Vec::new();
    for page in pages {
        let matches_hint = matches!(
            page.get_title().await,
            Ok(Some(title)) if !title.is_empty() && page_title_hint.contains(title.as_str())
        );
        if matches_hint {
            ordered.push(page);
        } else {
            tail.push(page);
        }
    }
    ordered.extend(tail);

    for page in ordered {
        if page_owns_backend_node(&page, backend_node_id).await {
            return Ok(page);
        }
    }
    Err(A11yError::CdpAxtreeFailed {
        detail: format!("no attached page owns backendNodeId {backend_node_id}"),
    })
}

async fn page_owns_backend_node(page: &chromiumoxide::Page, backend_node_id: i64) -> bool {
    use chromiumoxide::cdp::browser_protocol::dom::GetDocumentParams;

    let prime = GetDocumentParams::builder().depth(-1).pierce(true).build();
    let _ = page.execute(prime).await;
    let scroll = ScrollIntoViewIfNeededParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build();
    page.execute(scroll).await.is_ok()
}

/// Resolves a web node's live content-box rectangle in viewport-CSS pixels.
async fn node_content_rect(
    page: &chromiumoxide::Page,
    backend_node_id: i64,
) -> A11yResult<synapse_core::Rect> {
    let box_params = GetBoxModelParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build();
    let model = page
        .execute(box_params)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("getBoxModel: {err}"),
        })?;
    rect_from_quad(model.result.model.content.inner()).ok_or_else(|| A11yError::CdpAxtreeFailed {
        detail: "node has no resolvable box model (not rendered)".to_owned(),
    })
}

async fn read_node_scroll_state(
    page: &chromiumoxide::Page,
    backend_node_id: i64,
) -> A11yResult<CdpScrollState> {
    let resolve = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .object_group("synapse_scroll_verify")
        .build();
    let resolved = page
        .execute(resolve)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
        })?;
    let object_id =
        resolved
            .object
            .object_id
            .clone()
            .ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "resolveNode for backendNodeId {backend_node_id} returned no objectId"
                ),
            })?;
    let call = CallFunctionOnParams::builder()
        .function_declaration(
            r#"function() {
                const node = this;
                const doc = (node && node.ownerDocument) || document;
                const win = doc.defaultView || window;
                const root = doc.scrollingElement || doc.documentElement || doc.body;
                function isElement(value) {
                    return Boolean(value && value.nodeType === 1);
                }
                function isScrollable(element) {
                    if (!isElement(element)) { return false; }
                    const style = win.getComputedStyle(element);
                    const overflowY = style.overflowY || "";
                    const overflowX = style.overflowX || "";
                    const canY = /(auto|scroll|overlay)/.test(overflowY)
                        && element.scrollHeight > element.clientHeight;
                    const canX = /(auto|scroll|overlay)/.test(overflowX)
                        && element.scrollWidth > element.clientWidth;
                    return canY || canX;
                }
                let container = isElement(node) ? node : node.parentElement;
                while (container && container !== root && !isScrollable(container)) {
                    container = container.parentElement;
                }
                const target = (container && isScrollable(container)) ? container : root;
                const rect = node && node.getBoundingClientRect
                    ? node.getBoundingClientRect()
                    : { left: 0, top: 0, width: 0, height: 0 };
                return {
                    is_connected: Boolean(node && node.isConnected),
                    window_scroll_x: Number(win.scrollX || 0),
                    window_scroll_y: Number(win.scrollY || 0),
                    target_scroll_left: Number(target.scrollLeft || 0),
                    target_scroll_top: Number(target.scrollTop || 0),
                    target_scroll_width: Number(target.scrollWidth || 0),
                    target_scroll_height: Number(target.scrollHeight || 0),
                    target_client_width: Number(target.clientWidth || 0),
                    target_client_height: Number(target.clientHeight || 0),
                    target_tag: String(target.tagName || "DOCUMENT"),
                    target_id: String(target.id || ""),
                    node_rect_left: Number(rect.left || 0),
                    node_rect_top: Number(rect.top || 0),
                    node_rect_width: Number(rect.width || 0),
                    node_rect_height: Number(rect.height || 0)
                };
            }"#,
        )
        .object_id(object_id)
        .return_by_value(true)
        .silent(true)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Runtime.callFunctionOn scroll-state params: {err}"),
        })?;
    call_function_on_value(page, call, "scroll-state").await
}

async fn call_function_on_value<T>(
    page: &chromiumoxide::Page,
    call: CallFunctionOnParams,
    label: &str,
) -> A11yResult<T>
where
    T: DeserializeOwned,
{
    let returns = page
        .execute(call)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn {label} readback: {err}"),
        })?;
    if let Some(exception) = &returns.exception_details {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn {label} exception: {exception:?}"),
        });
    }
    let remote = returns.result.result.clone();
    let value = remote.value.ok_or_else(|| A11yError::CdpAxtreeFailed {
        detail: format!(
            "Runtime.callFunctionOn {label} returned no by-value payload (type={:?} subtype={:?} description={:?})",
            remote.r#type, remote.subtype, remote.description
        ),
    })?;
    serde_json::from_value::<T>(value).map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("Runtime.callFunctionOn {label} decode: {err}"),
    })
}

/// Decodes a Chrome PNG screenshot into a top-down BGRA8 bitmap for OCR (#703).
fn decode_png_to_bgra(png_bytes: &[u8]) -> A11yResult<CdpNodeBitmap> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes))
        .read_info()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("screenshot PNG header decode failed: {err}"),
        })?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "screenshot PNG output buffer size overflowed usize".to_owned(),
        })?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("screenshot PNG frame decode failed: {err}"),
        })?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "unexpected screenshot PNG bit depth {:?}; expected 8-bit",
                info.bit_depth
            ),
        });
    }
    let pixels = buf
        .get(..info.buffer_size())
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "screenshot PNG buffer shorter than reported frame size".to_owned(),
        })?;
    let bgra = match info.color_type {
        png::ColorType::Rgba => rgba8_to_bgra(pixels),
        png::ColorType::Rgb => rgb8_to_bgra(pixels),
        other => {
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!(
                    "unexpected screenshot PNG color type {other:?}; expected RGB/RGBA"
                ),
            });
        }
    };
    Ok(CdpNodeBitmap {
        width: info.width,
        height: info.height,
        bgra,
    })
}

/// Swaps RGBA8 → BGRA8 (Chrome screenshots are RGBA; `WinRT` OCR wants BGRA).
fn rgba8_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

/// Expands RGB8 → BGRA8 with an opaque alpha channel.
fn rgb8_to_bgra(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.chunks_exact(3) {
        out.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Locks the FSV-discovered bug: leaving the `buttons` bit set on release
    // makes Chrome think the button is still held and never fires a `click`
    // event. Pressed → button bit; moved/released → 0.
    #[test]
    fn mouse_event_buttons_bitmask_is_set_only_while_pressed() {
        let point = CdpActionPoint { x: 10.0, y: 20.0 };
        let pressed = mouse_event(
            DispatchMouseEventType::MousePressed,
            point,
            MouseButton::Left,
            1,
        );
        let released = mouse_event(
            DispatchMouseEventType::MouseReleased,
            point,
            MouseButton::Left,
            1,
        );
        let moved = mouse_event(
            DispatchMouseEventType::MouseMoved,
            point,
            MouseButton::Left,
            0,
        );
        let hover = mouse_event(
            DispatchMouseEventType::MouseMoved,
            point,
            MouseButton::None,
            0,
        );
        println!(
            "readback=mouse_event buttons pressed:{:?} released:{:?} moved:{:?} hover_button:{:?}",
            pressed.buttons, released.buttons, moved.buttons, hover.button
        );
        assert_eq!(pressed.buttons, Some(1), "left press must hold bit 1");
        assert_eq!(released.buttons, Some(0), "release must clear the bitmask");
        assert_eq!(moved.buttons, Some(0), "move must not hold any button");
        assert_eq!(hover.button, Some(MouseButton::None));
        assert_eq!(pressed.click_count, Some(1));

        let right = mouse_event(
            DispatchMouseEventType::MousePressed,
            point,
            MouseButton::Right,
            1,
        );
        assert_eq!(right.buttons, Some(2), "right press must hold bit 2");
    }

    #[test]
    fn mouse_event_preserves_modifier_bitmask() {
        let point = CdpActionPoint { x: 10.0, y: 20.0 };
        let pressed = mouse_event_with_modifiers(
            DispatchMouseEventType::MousePressed,
            point,
            MouseButton::Left,
            2,
            1 | 2 | 8,
        );

        assert_eq!(pressed.modifiers, Some(11));
        assert_eq!(pressed.click_count, Some(2));
        assert_eq!(pressed.buttons, Some(1));
    }

    #[test]
    fn cdp_mouse_button_maps_to_cdp_enum() {
        assert_eq!(CdpMouseButton::Left.to_cdp(), MouseButton::Left);
        assert_eq!(CdpMouseButton::Right.to_cdp(), MouseButton::Right);
        assert_eq!(CdpMouseButton::Middle.to_cdp(), MouseButton::Middle);
    }

    #[test]
    fn cdp_page_websocket_url_maps_browser_endpoint_to_page_socket() {
        let ws = cdp_page_websocket_url("http://127.0.0.1:64499/", "ABC123")
            .unwrap_or_else(|err| panic!("valid CDP endpoint rejected: {err}"));

        assert_eq!(ws, "ws://127.0.0.1:64499/devtools/page/ABC123");
    }

    #[test]
    fn raw_mouse_event_message_uses_cdp_wire_values() {
        let message = raw_mouse_event_message(
            7,
            DispatchMouseEventType::MousePressed,
            CdpActionPoint { x: 52.0, y: 191.0 },
            MouseButton::Left,
            1,
            1,
        );

        println!("readback=raw_cdp_mouse_event_message {message}");
        assert_eq!(message["id"], json!(7));
        assert_eq!(message["method"], json!("Input.dispatchMouseEvent"));
        assert_eq!(message["params"]["type"], json!("mousePressed"));
        assert_eq!(message["params"]["x"], json!(52.0));
        assert_eq!(message["params"]["y"], json!(191.0));
        assert_eq!(message["params"]["button"], json!("left"));
        assert_eq!(message["params"]["buttons"], json!(1));
        assert_eq!(message["params"]["clickCount"], json!(1));
    }

    #[test]
    fn touch_event_message_uses_cdp_wire_values() {
        let point = CdpActionPoint { x: 52.0, y: 191.0 };
        let start = serde_json::to_value(
            touch_event(DispatchTouchEventType::TouchStart, Some(point))
                .expect("touch start params"),
        )
        .expect("serialize touch start");
        let end = serde_json::to_value(
            touch_event(DispatchTouchEventType::TouchEnd, None).expect("touch end params"),
        )
        .expect("serialize touch end");

        println!("readback=touch_event start={start} end={end}");
        assert_eq!(start["type"], json!("touchStart"));
        assert_eq!(start["touchPoints"][0]["x"], json!(52.0));
        assert_eq!(start["touchPoints"][0]["y"], json!(191.0));
        assert_eq!(start["touchPoints"][0]["id"], json!(1.0));
        assert_eq!(start["touchPoints"][0]["force"], json!(1.0));
        assert_eq!(end["type"], json!("touchEnd"));
        assert_eq!(end.get("touchPoints"), None);
    }

    #[test]
    fn mouse_event_drag_move_can_hold_button_bit() {
        let point = CdpActionPoint { x: 10.0, y: 20.0 };
        let drag_move = mouse_event_with_buttons(
            DispatchMouseEventType::MouseMoved,
            point,
            MouseButton::Left,
            0,
            Some(mouse_button_bit(&MouseButton::Left)),
            0,
        );
        println!(
            "readback=mouse_event drag_move buttons:{:?} button:{:?}",
            drag_move.buttons, drag_move.button
        );
        assert_eq!(drag_move.buttons, Some(1));
        assert_eq!(drag_move.button, Some(MouseButton::Left));
    }

    // ---- selector engine: pure helpers (#1110–#1118) ----

    #[test]
    fn normalize_ws_collapses_trims_and_drops_zero_width() {
        let cases = [
            ("  Apply   now \n", "Apply now"),
            ("\tSubmit\u{200b} \u{00ad}order\t", "Submit order"),
            ("single", "single"),
            ("   ", ""),
        ];
        for (input, expected) in cases {
            let got = normalize_ws(input);
            println!("readback=normalize_ws before={input:?} after={got:?}");
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn name_matcher_substring_is_case_insensitive_and_normalized() {
        let matcher = NameMatcher::new("apply now", false, false).expect("valid substring");
        println!("readback=name_matcher edge=substring");
        assert!(matcher.matches("  Click  APPLY   NOW  please "));
        assert!(!matcher.matches("apply"));
    }

    #[test]
    fn name_matcher_exact_is_case_sensitive_after_normalization() {
        let matcher = NameMatcher::new("Apply Now", true, false).expect("valid exact");
        println!("readback=name_matcher edge=exact");
        assert!(matcher.matches("Apply   Now"));
        assert!(!matcher.matches("apply now"));
        assert!(!matcher.matches("Apply Now please"));
    }

    #[test]
    fn name_matcher_regex_runs_against_normalized_text() {
        let matcher = NameMatcher::new("^Item \\d+$", false, true).expect("valid regex");
        println!("readback=name_matcher edge=regex");
        assert!(matcher.matches("Item   42"));
        assert!(!matcher.matches("Item x"));
    }

    #[test]
    fn name_matcher_invalid_regex_errors_loud() {
        let err = NameMatcher::new("a(", false, true).expect_err("invalid regex must fail");
        let detail = err.to_string();
        println!("readback=name_matcher edge=invalid_regex err={detail:?}");
        assert!(detail.contains("invalid"), "error should explain the cause");
    }

    #[test]
    fn apply_nth_and_limit_picks_positions_and_caps() {
        let ids = vec![10_i64, 20, 30, 40];
        println!("readback=nth before={ids:?}");
        assert_eq!(apply_nth_and_limit(ids.clone(), Some(0), 50), vec![10]);
        assert_eq!(apply_nth_and_limit(ids.clone(), Some(2), 50), vec![30]);
        assert_eq!(apply_nth_and_limit(ids.clone(), Some(-1), 50), vec![40]);
        assert_eq!(apply_nth_and_limit(ids.clone(), Some(-2), 50), vec![30]);
        // Out-of-range nth resolves to empty rather than panicking.
        assert_eq!(
            apply_nth_and_limit(ids.clone(), Some(9), 50),
            Vec::<i64>::new()
        );
        assert_eq!(
            apply_nth_and_limit(ids.clone(), Some(-9), 50),
            Vec::<i64>::new()
        );
        // No nth: cap by limit, preserve order.
        assert_eq!(apply_nth_and_limit(ids.clone(), None, 2), vec![10, 20]);
        assert_eq!(apply_nth_and_limit(ids, None, 50).len(), 4);
    }

    #[test]
    fn engine_and_relation_wire_strings_match_playwright_tokens() {
        assert_eq!(CdpLocateEngine::Css.as_str(), "css");
        assert_eq!(CdpLocateEngine::AltText.as_str(), "alttext");
        assert_eq!(CdpLocateEngine::TestId.as_str(), "testid");
        assert!(CdpLocateEngine::Text.uses_injected_js());
        assert!(!CdpLocateEngine::Role.uses_injected_js());
        assert_eq!(CdpLayoutRelation::RightOf.as_str(), "right-of");
        assert_eq!(CdpLayoutRelation::Near.as_str(), "near");
    }

    #[test]
    fn locate_spec_json_carries_options_in_camel_case() {
        let request = CdpLocateRequest {
            engine: CdpLocateEngine::Layout,
            query: "button".to_owned(),
            relation: Some(CdpLayoutRelation::RightOf),
            anchor: Some("#label".to_owned()),
            max_distance: Some(120.0),
            has_text: Some("Save".to_owned()),
            nth: Some(-1),
            limit: 25,
            ..Default::default()
        };
        let spec = locate_spec_json(&request);
        println!("readback=locate_spec_json spec={spec}");
        assert_eq!(spec["engine"], "layout");
        assert_eq!(spec["query"], "button");
        assert_eq!(spec["relation"], "right-of");
        assert_eq!(spec["anchor"], "#label");
        assert_eq!(spec["maxDistance"], 120.0);
        assert_eq!(spec["hasText"], "Save");
        assert_eq!(spec["nth"], -1);
        assert_eq!(spec["limit"], 25);
        // Unset options serialize to JSON null (the engine treats null as unset).
        assert!(spec["testidAttribute"].is_null());
    }

    #[test]
    fn injected_engine_js_is_syntactically_self_contained() {
        // Guards against an accidental unbalanced brace/paren in the engine body
        // that would only surface as a runtime CDP exception against live Chrome.
        let opens = SYNAPSE_LOCATE_JS.matches('{').count();
        let closes = SYNAPSE_LOCATE_JS.matches('}').count();
        let popen = SYNAPSE_LOCATE_JS.matches('(').count();
        let pclose = SYNAPSE_LOCATE_JS.matches(')').count();
        println!(
            "readback=engine_js braces={opens}/{closes} parens={popen}/{pclose} len={}",
            SYNAPSE_LOCATE_JS.len()
        );
        assert_eq!(opens, closes, "unbalanced braces in injected engine");
        assert_eq!(popen, pclose, "unbalanced parens in injected engine");
        assert!(SYNAPSE_LOCATE_JS.starts_with("function(scope, spec)"));
        assert!(
            !SYNAPSE_LOCATE_JS.contains('"'),
            "engine must use single quotes"
        );
    }

    #[test]
    fn cdp_load_state_conditions_match_playwright_states() {
        assert!(cdp_ready_state_satisfies_load_state(
            CdpLoadState::DomContentLoaded,
            "interactive"
        ));
        assert!(cdp_ready_state_satisfies_load_state(
            CdpLoadState::DomContentLoaded,
            "complete"
        ));
        assert!(!cdp_ready_state_satisfies_load_state(
            CdpLoadState::DomContentLoaded,
            "loading"
        ));
        assert!(!cdp_ready_state_satisfies_load_state(
            CdpLoadState::Load,
            "interactive"
        ));
        assert!(cdp_ready_state_satisfies_load_state(
            CdpLoadState::Load,
            "complete"
        ));

        assert!(cdp_load_state_satisfied(
            CdpLoadState::DomContentLoaded,
            "loading",
            true,
            false,
            0,
            Duration::from_millis(0),
        ));
        assert!(cdp_load_state_satisfied(
            CdpLoadState::Load,
            "loading",
            false,
            true,
            0,
            Duration::from_millis(0),
        ));
        assert!(!cdp_load_state_satisfied(
            CdpLoadState::NetworkIdle,
            "interactive",
            true,
            false,
            0,
            Duration::from_millis(600),
        ));
        assert!(!cdp_load_state_satisfied(
            CdpLoadState::NetworkIdle,
            "complete",
            true,
            true,
            1,
            Duration::from_millis(600),
        ));
        assert!(!cdp_load_state_satisfied(
            CdpLoadState::NetworkIdle,
            "complete",
            true,
            true,
            0,
            Duration::from_millis(499),
        ));
        assert!(cdp_load_state_satisfied(
            CdpLoadState::NetworkIdle,
            "complete",
            true,
            true,
            0,
            Duration::from_millis(500),
        ));
    }

    #[test]
    fn cdp_url_matcher_supports_exact_glob_and_regex() {
        let exact = CdpUrlMatcher::new("https://example.test/path", CdpUrlMatchKind::Exact)
            .expect("exact matcher");
        assert!(exact.matches("https://example.test/path"));
        assert!(!exact.matches("https://example.test/path?x=1"));

        let glob = CdpUrlMatcher::new("https://example.test/*/done?x=?", CdpUrlMatchKind::Glob)
            .expect("glob matcher");
        assert!(glob.matches("https://example.test/a/b/done?x=1"));
        assert!(glob.matches("https://example.test/route/done?x=z"));
        assert!(!glob.matches("https://example.test/route/done?x=zz"));

        let regex =
            CdpUrlMatcher::new(r"^https://example\.test/items/\d+$", CdpUrlMatchKind::Regex)
                .expect("regex matcher");
        assert!(regex.matches("https://example.test/items/42"));
        assert!(!regex.matches("https://example.test/items/new"));

        let err =
            CdpUrlMatcher::new("(", CdpUrlMatchKind::Regex).expect_err("invalid regex must fail");
        println!("readback=wait_for_url invalid_regex err={err}");
        assert!(err.to_string().contains("waitForURL regex"));
    }

    #[test]
    fn scroll_into_view_change_detection_distinguishes_window_and_container() {
        fn snapshot(window_y: f64, container_top: f64, is_root: bool) -> CdpScrollIntoViewSnapshot {
            CdpScrollIntoViewSnapshot {
                is_connected: true,
                viewport_width: 800.0,
                viewport_height: 600.0,
                node_rect: CdpScrollIntoViewRect {
                    x: 10.0,
                    y: 700.0 - window_y - container_top,
                    width: 40.0,
                    height: 20.0,
                },
                node_fully_in_viewport: false,
                window_scroll_x: 0.0,
                window_scroll_y: window_y,
                container: CdpScrollIntoViewContainer {
                    is_root,
                    tag_name: "DIV".to_owned(),
                    id: "scrollbox".to_owned(),
                    scroll_left: 0.0,
                    scroll_top: container_top,
                    scroll_width: 800.0,
                    scroll_height: 1400.0,
                    client_width: 800.0,
                    client_height: 300.0,
                },
                box_model_content: None,
                box_model_error: None,
            }
        }

        let before = snapshot(0.0, 0.0, false);
        let container_after = snapshot(0.0, 320.0, false);
        let window_after = snapshot(320.0, 0.0, true);

        assert!(container_scroll_changed(&before, &container_after));
        assert!(!container_scroll_changed(&before, &window_after));
        assert!(scroll_value_changed(
            before.window_scroll_y,
            window_after.window_scroll_y
        ));
        assert!(!scroll_value_changed(10.0, 10.1));
    }
}
