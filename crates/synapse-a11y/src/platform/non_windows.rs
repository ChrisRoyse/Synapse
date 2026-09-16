use synapse_core::{
    AccessibleNode, AccessibleSubtree, ElementId, ForegroundContext, Point, UiaPattern,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    A11yError, A11yResult, AccessibleEvent, ElementClickAction, ElementMetadataReadback,
    ElementScrollReadback, ElementSearchScope, ElementTextInsertReadback,
    ElementTextSelectionReadback, ElementValueReadback, ElementValueSetReadback, ExpandState,
    ForegroundActivationIntent, UIElement, UiaWorkerReadback, WinEventHookReadback,
};

pub struct WinEventSubscription {
    readback: WinEventHookReadback,
}

impl WinEventSubscription {
    pub const fn readback(&self) -> &WinEventHookReadback {
        &self.readback
    }
}

pub fn focused_window() -> A11yResult<UIElement> {
    Err(A11yError::not_available(
        "UIA foreground window lookup requires Windows",
    ))
}

pub fn current_foreground_context() -> A11yResult<ForegroundContext> {
    Err(A11yError::not_available(
        "foreground context lookup requires Windows",
    ))
}

pub fn window_from_hwnd(_hwnd: i64) -> A11yResult<UIElement> {
    Err(A11yError::not_available(
        "UIA HWND window lookup requires Windows",
    ))
}

pub fn focus_window_with_intent(_hwnd: i64, _intent: ForegroundActivationIntent) -> A11yResult<()> {
    Err(A11yError::not_available(
        "foreground window focus requires Windows",
    ))
}

pub fn is_window_minimized(_hwnd: i64) -> A11yResult<bool> {
    Err(A11yError::not_available(
        "minimized window readback requires Windows",
    ))
}

pub fn is_window_visible(_hwnd: i64) -> A11yResult<bool> {
    Err(A11yError::not_available(
        "visible window readback requires Windows",
    ))
}

pub fn millis_since_last_input() -> A11yResult<u64> {
    Err(A11yError::not_available(
        "last-input idle readback requires Windows",
    ))
}

pub fn is_top_level_window(_hwnd: i64) -> A11yResult<bool> {
    Err(A11yError::not_available(
        "top-level window readback requires Windows",
    ))
}

pub fn top_level_root_hwnd(_hwnd: i64) -> A11yResult<i64> {
    Err(A11yError::not_available(
        "top-level root HWND readback requires Windows",
    ))
}

pub fn close_window(_hwnd: i64) -> A11yResult<()> {
    Err(A11yError::not_available("window close requires Windows"))
}

pub fn window_for_process(_pid: u32) -> A11yResult<UIElement> {
    Err(A11yError::not_available(
        "UIA process window lookup requires Windows",
    ))
}

pub fn snapshot_focused_window(_depth: u32) -> A11yResult<AccessibleSubtree> {
    Err(A11yError::not_available(
        "UIA foreground snapshots require Windows",
    ))
}

pub fn snapshot_window_from_hwnd(_hwnd: i64, _depth: u32) -> A11yResult<AccessibleSubtree> {
    Err(A11yError::not_available(
        "UIA HWND snapshots require Windows",
    ))
}

pub fn snapshot_window_for_process(_pid: u32, _depth: u32) -> A11yResult<AccessibleSubtree> {
    Err(A11yError::not_available(
        "UIA process-window snapshots require Windows",
    ))
}

pub fn top_level_window_hwnd_by_name(_name: String) -> A11yResult<Option<i64>> {
    Err(A11yError::not_available(
        "UIA top-level window name lookup requires Windows",
    ))
}

pub fn foreground_context(_hwnd: i64) -> A11yResult<ForegroundContext> {
    Err(A11yError::not_available(
        "foreground context lookup requires Windows",
    ))
}

pub fn visible_top_level_window_contexts() -> A11yResult<Vec<ForegroundContext>> {
    Err(A11yError::not_available(
        "visible top-level window enumeration requires Windows",
    ))
}

pub fn focused_element() -> A11yResult<UIElement> {
    Err(A11yError::not_available(
        "UIA focused element lookup requires Windows",
    ))
}

pub fn element_from_point(_point: Point) -> A11yResult<UIElement> {
    Err(A11yError::not_available(
        "UIA element hit testing requires Windows",
    ))
}

pub fn focused_element_node() -> A11yResult<AccessibleNode> {
    Err(A11yError::not_available(
        "UIA focused element lookup requires Windows",
    ))
}

pub fn focused_element_node_in_window(_hwnd: i64) -> A11yResult<Option<AccessibleNode>> {
    Err(A11yError::not_available(
        "target-window UIA focused element lookup requires Windows",
    ))
}

pub fn element_node_from_point(_point: Point) -> A11yResult<AccessibleNode> {
    Err(A11yError::not_available(
        "UIA element hit testing requires Windows",
    ))
}

pub fn snapshot(_root: &UIElement, _depth: u32) -> A11yResult<AccessibleSubtree> {
    Err(A11yError::not_available(
        "UIA tree snapshots require Windows",
    ))
}

pub fn find_by_name_and_pattern(
    _root: &UIElement,
    _name: &str,
    _pattern: UiaPattern,
    _scope: ElementSearchScope,
) -> A11yResult<Option<AccessibleNode>> {
    Err(A11yError::not_available(
        "UIA direct element search requires Windows",
    ))
}

pub fn find_by_name_and_pattern_in_window(
    _hwnd: i64,
    _name: String,
    _pattern: UiaPattern,
    _scope: ElementSearchScope,
) -> A11yResult<Option<AccessibleNode>> {
    Err(A11yError::not_available(
        "UIA direct element search requires Windows",
    ))
}

pub fn chromium_renderer_accessibility_nodes_from_window(
    _hwnd: i64,
    _depth: u32,
    _max_nodes: usize,
) -> A11yResult<Vec<AccessibleNode>> {
    Err(A11yError::not_available(
        "Chromium renderer UIA supplement requires Windows",
    ))
}

pub fn snapshot_element(_id: &ElementId, _depth: u32) -> A11yResult<AccessibleSubtree> {
    Err(A11yError::not_available(
        "UIA element snapshots require Windows",
    ))
}

pub fn re_resolve(_id: &ElementId) -> A11yResult<UIElement> {
    Err(A11yError::not_available(
        "UIA element re-resolution requires Windows",
    ))
}

pub fn element_bounding_rect(_id: &ElementId) -> A11yResult<synapse_core::Rect> {
    Err(A11yError::not_available(
        "UIA element bounding rectangle requires Windows",
    ))
}

pub fn click_element_action(_id: &ElementId) -> A11yResult<ElementClickAction> {
    Err(A11yError::not_available(
        "UIA element click action requires Windows",
    ))
}

pub fn focus_element(_id: &ElementId) -> A11yResult<()> {
    Err(A11yError::not_available(
        "UIA element focus requires Windows",
    ))
}

pub fn set_element_value(_id: &ElementId, _value: &str) -> A11yResult<ElementValueSetReadback> {
    Err(A11yError::not_available(
        "UIA/native element text entry requires Windows",
    ))
}

pub fn element_value(_id: &ElementId) -> A11yResult<ElementValueReadback> {
    Err(A11yError::not_available(
        "UIA/native element text readback requires Windows",
    ))
}

pub fn set_element_text_selection(
    _id: &ElementId,
    _start: u32,
    _end: u32,
) -> A11yResult<ElementTextSelectionReadback> {
    Err(A11yError::not_available(
        "UIA/native text selection requires Windows",
    ))
}

pub fn replace_element_text_selection(
    _id: &ElementId,
    _text: &str,
) -> A11yResult<ElementTextInsertReadback> {
    Err(A11yError::not_available(
        "UIA/native text insertion requires Windows",
    ))
}

pub fn append_element_text(_id: &ElementId, _text: &str) -> A11yResult<ElementTextInsertReadback> {
    Err(A11yError::not_available(
        "UIA/native text append requires Windows",
    ))
}

pub fn element_metadata(_id: &ElementId) -> A11yResult<ElementMetadataReadback> {
    Err(A11yError::not_available(
        "UIA element metadata readback requires Windows",
    ))
}

pub fn scroll_element(_id: &ElementId, _dy: i32, _dx: i32) -> A11yResult<ElementScrollReadback> {
    Err(A11yError::not_available(
        "UIA element scrolling requires Windows",
    ))
}

pub fn scroll_element_into_view(_id: &ElementId) -> A11yResult<()> {
    Err(A11yError::not_available(
        "UIA scroll-into-view requires Windows",
    ))
}

pub fn element_scroll_state(_id: &ElementId) -> A11yResult<crate::ElementScrollStateReadback> {
    Err(A11yError::not_available(
        "UIA element scroll state readback requires Windows",
    ))
}

pub fn expand_state_of(_element: &UIElement) -> A11yResult<ExpandState> {
    Err(A11yError::not_available(
        "ExpandCollapsePattern state requires Windows",
    ))
}

pub fn expand_state_of_id(_id: &ElementId) -> A11yResult<ExpandState> {
    Err(A11yError::not_available(
        "ExpandCollapsePattern state requires Windows",
    ))
}

pub fn subscribe_win_events(
    _sender: UnboundedSender<AccessibleEvent>,
) -> A11yResult<WinEventSubscription> {
    Err(A11yError::not_available("WinEvent hooks require Windows"))
}

pub fn uia_worker_readback() -> A11yResult<UiaWorkerReadback> {
    Err(A11yError::not_available(
        "UI Automation worker readback requires Windows",
    ))
}
