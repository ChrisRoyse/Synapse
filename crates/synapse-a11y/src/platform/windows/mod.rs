mod common;
mod events;
mod resolve;
mod snapshot;
mod window;

pub fn uia_worker_readback() -> crate::A11yResult<crate::UiaWorkerReadback> {
    common::worker_readback()
}

pub use events::{WinEventSubscription, retained_live_owner_count, subscribe_win_events};
pub use resolve::{
    append_element_text, click_element_action, element_bounding_rect, element_metadata,
    element_scroll_state, element_value, expand_state_of, expand_state_of_id, focus_element,
    re_resolve, replace_element_text_selection, scroll_element, scroll_element_into_view,
    set_element_text_selection, set_element_value,
};
pub use snapshot::{
    chromium_renderer_accessibility_nodes_from_window, element_node_from_point,
    find_by_name_and_pattern, find_by_name_and_pattern_in_window, focused_element_node,
    focused_element_node_in_window, snapshot, snapshot_element, snapshot_window_from_hwnd,
};
pub use window::{
    WindowBoundsOutcome, close_window, current_foreground_context, element_from_point,
    focus_window_with_intent, focused_element, focused_window, foreground_context,
    is_top_level_window, is_window_minimized, is_window_visible, millis_since_last_input,
    set_window_bounds, snapshot_focused_window, snapshot_window_for_process, top_level_root_hwnd,
    top_level_window_hwnd_by_name, visible_top_level_window_contexts, window_for_process,
    window_from_hwnd,
};
