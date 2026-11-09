use std::{
    collections::HashSet,
    ffi::c_void,
    mem,
    sync::Mutex,
    time::{Duration, Instant},
};

use synapse_core::{
    AccessibleNode, AccessibleSubtree, ElementId, Point, UiaPattern, element_id,
    win32_hwnd::{hwnd_from_wire, hwnd_to_wire, native_hwnds_equal},
};
use uiautomation::{
    UIAutomation, UIElement,
    core::{UICacheRequest, UICondition, UITreeWalker},
    types::{
        ElementMode, Handle, Point as UiaPoint, PropertyConditionFlags, TreeScope, UIProperty,
    },
    variants::Variant,
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GA_ROOT, GA_ROOTOWNER, GUITHREADINFO, GW_OWNER, GetAncestor, GetForegroundWindow,
        GetGUIThreadInfo, GetWindow, GetWindowThreadProcessId, IsWindow,
    },
};

use crate::{A11yError, A11yResult, ElementSearchScope};

use super::common::{
    TreeView, cached_hwnd, cached_patterns, cached_rect, cached_role,
    cached_runtime_id_hex_or_fallback, cached_value, create_cache_request, map_uia_error,
    non_empty, pattern_property, with_automation, with_automation_operation,
};

static SNAPSHOT_CACHE: Mutex<Option<SnapshotCache>> = Mutex::new(None);
const RAW_SUPPLEMENT_DEPTH: u32 = 2;
/// Hard ceiling on nodes collected in one snapshot. Cross-process trees (UWP,
/// browsers) can be large; this bounds worst-case work while preserving every
/// node collected up to the cap (the result is flagged `truncated`, never
/// silently collapsed).
const SNAPSHOT_NODE_BUDGET: usize = 4000;
/// Wall-clock budget for one snapshot walk. When exceeded the walk stops
/// descending further but KEEPS everything collected so far and flags
/// `truncated`. This replaces the previous behaviour, which discarded a
/// complete tree and re-ran at depth 1 whenever a walk took >25ms — fatal for
/// inherently slower cross-process UWP/ApplicationFrameHost trees.
const SNAPSHOT_DEADLINE: Duration = Duration::from_millis(400);
const SNAPSHOT_WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const RAW_SUPPLEMENT_NODE_BUDGET: usize = 60;
const CHROMIUM_RENDERER_CONTENT_TOP_INSET_PX: i32 = 96;
// Packaged Notepad exposes these top-level menu items through raw
// name+ExpandCollapse search even when RawView child walking omits them.
const RAW_MENU_SUPPLEMENT_NAMES: [&str; 3] = ["File", "Edit", "View"];

struct SnapshotCache {
    requested_depth: u32,
    root: ElementId,
    captured_at: Instant,
    tree: AccessibleSubtree,
}

struct SnapshotWalk<'a> {
    /// True condition so every child is enumerated (raw view equivalent).
    true_condition: &'a UICondition,
    cache: &'a UICacheRequest,
    raw_walker: &'a UITreeWalker,
    root_hwnd: i64,
    /// Stop collecting once this many nodes are collected.
    node_budget: usize,
    /// Stop descending once this instant is reached.
    deadline: Instant,
}

pub fn snapshot(root: &UIElement, depth: u32) -> A11yResult<AccessibleSubtree> {
    let _ = root;
    let _ = depth;
    Err(A11yError::internal(
        "direct UIElement snapshot is disabled; use snapshot_focused_window, snapshot_window_from_hwnd, or snapshot_element so UIA stays on the dedicated MTA worker",
    ))
}

pub fn snapshot_window_from_hwnd(hwnd: i64, depth: u32) -> A11yResult<AccessibleSubtree> {
    let hwnd = native_hwnd_value(hwnd)?;
    with_automation_operation(
        format!("snapshot_window_from_hwnd hwnd=0x{hwnd:x} depth={depth}"),
        SNAPSHOT_WORKER_REPLY_TIMEOUT,
        move |automation| {
            let root = automation
                .element_from_handle(Handle::from(hwnd))
                .map_err(map_uia_error)?;
            snapshot_from_root(automation, &root, depth)
        },
    )
}

pub fn snapshot_element(id: &ElementId, depth: u32) -> A11yResult<AccessibleSubtree> {
    let id = id.clone();
    with_automation_operation(
        format!("snapshot_element element_id={id} depth={depth}"),
        SNAPSHOT_WORKER_REPLY_TIMEOUT,
        move |automation| {
            let root = super::resolve::re_resolve_on_worker(automation, &id)?;
            snapshot_from_root(automation, &root, depth)
        },
    )
}

pub(super) fn snapshot_from_root(
    automation: &UIAutomation,
    root: &UIElement,
    depth: u32,
) -> A11yResult<AccessibleSubtree> {
    let started = Instant::now();
    let mut tree = snapshot_at_depth(automation, root, depth)?;
    if depth >= RAW_SUPPLEMENT_DEPTH {
        tree.truncated |= supplement_raw_pattern_nodes(automation, root, &mut tree.nodes)?;
        tree.max_depth = tree.max_depth.max(RAW_SUPPLEMENT_DEPTH);
    }
    // Observability: `truncated` is also surfaced to callers (observe
    // diagnostics) so an incomplete tree is never mistaken for a complete
    // one. A truncated tree means the node budget/deadline was hit or a
    // subtree errored (the per-subtree error is logged at warn separately).
    tracing::debug!(
        code = "A11Y_SNAPSHOT_ASSEMBLED",
        requested_depth = depth,
        nodes = tree.nodes.len(),
        max_depth = tree.max_depth,
        truncated = tree.truncated,
        elapsed_ms = started.elapsed().as_millis(),
        "a11y snapshot assembled"
    );
    store_snapshot(depth, &tree);
    Ok(tree)
}

pub fn focused_element_node() -> A11yResult<AccessibleNode> {
    with_automation(|automation| {
        let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Control)?;
        let element = automation
            .get_focused_element_build_cache(&cache)
            .map_err(map_uia_error)?;
        let foreground_hwnd = unsafe { GetForegroundWindow() };
        let root_hwnd = cached_hwnd(&element)
            .filter(|value| *value != 0)
            .unwrap_or_else(|| hwnd_to_wire(foreground_hwnd.0 as isize));
        node_from_cached_element(&element, None, 0, root_hwnd, 0)
    })
}

pub fn focused_element_node_in_window(hwnd: i64) -> A11yResult<Option<AccessibleNode>> {
    let target = valid_hwnd(hwnd)?;
    let thread_id = unsafe { GetWindowThreadProcessId(target, None) };
    if thread_id == 0 {
        return Err(A11yError::NoForeground {
            detail: format!(
                "GetWindowThreadProcessId returned no GUI thread for hwnd 0x{:x}",
                target.0 as isize
            ),
        });
    }

    let mut info = GUITHREADINFO {
        cbSize: u32::try_from(mem::size_of::<GUITHREADINFO>())
            .map_err(|_err| A11yError::internal("GUITHREADINFO size does not fit cbSize"))?,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(thread_id, &raw mut info) }.map_err(|err| {
        A11yError::internal(format!(
            "GetGUIThreadInfo failed for hwnd 0x{:x} thread_id={thread_id}: {err}",
            target.0 as isize
        ))
    })?;

    let focus = info.hwndFocus;
    if focus.0.is_null() {
        return Ok(None);
    }
    if !unsafe { IsWindow(Some(focus)) }.as_bool() {
        tracing::debug!(
            code = "A11Y_TARGET_FOCUS_HWND_STALE",
            target_hwnd = target.0 as isize,
            focus_hwnd = focus.0 as isize,
            "target GUI thread reported a stale focused HWND"
        );
        return Ok(None);
    }
    if !focus_belongs_to_target(target, focus) {
        tracing::debug!(
            code = "A11Y_TARGET_FOCUS_OUTSIDE_TARGET",
            target_hwnd = target.0 as isize,
            focus_hwnd = focus.0 as isize,
            "target GUI thread focus was outside the requested target root/owner chain"
        );
        return Ok(None);
    }

    let focus_raw = focus.0 as isize;
    let target_raw = hwnd_to_wire(target.0 as isize);
    with_automation_operation(
        format!(
            "focused_element_node_in_window target=0x{:x} focus=0x{focus_raw:x}",
            target.0 as isize
        ),
        SNAPSHOT_WORKER_REPLY_TIMEOUT,
        move |automation| {
            let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Control)?;
            let element = automation
                .element_from_handle_build_cache(Handle::from(focus_raw), &cache)
                .map_err(map_uia_error)?;
            let mut node = node_from_cached_element(&element, None, 0, target_raw, 0)?;
            node.focused = true;
            Ok(Some(node))
        },
    )
}

pub fn element_node_from_point(point: Point) -> A11yResult<AccessibleNode> {
    with_automation(move |automation| {
        let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Control)?;
        let element = automation
            .element_from_point_build_cache(UiaPoint::new(point.x, point.y), &cache)
            .map_err(map_uia_error)?;
        let root_hwnd = cached_hwnd(&element).unwrap_or(0);
        node_from_cached_element(&element, None, 0, root_hwnd, 0)
    })
}

pub fn find_by_name_and_pattern(
    root: &UIElement,
    name: &str,
    pattern: UiaPattern,
    scope: ElementSearchScope,
) -> A11yResult<Option<AccessibleNode>> {
    if name.is_empty() {
        return Ok(None);
    }
    let _ = root;
    let _ = pattern;
    let _ = scope;

    Err(A11yError::internal(
        "direct UIElement search is disabled; use a data-returning worker API so UIA stays on the dedicated MTA worker",
    ))
}

pub fn find_by_name_and_pattern_in_window(
    hwnd: i64,
    name: String,
    pattern: UiaPattern,
    scope: ElementSearchScope,
) -> A11yResult<Option<AccessibleNode>> {
    let hwnd = native_hwnd_value(hwnd)?;
    with_automation_operation(
        format!(
            "find_by_name_and_pattern_in_window hwnd=0x{hwnd:x} scope={scope:?} pattern={pattern:?}"
        ),
        SNAPSHOT_WORKER_REPLY_TIMEOUT,
        move |automation| {
            let root = automation
                .element_from_handle(Handle::from(hwnd))
                .map_err(map_uia_error)?;
            find_by_name_and_pattern_from_root(automation, &root, &name, pattern, scope)
        },
    )
}

pub fn chromium_renderer_accessibility_nodes_from_window(
    hwnd: i64,
    depth: u32,
    max_nodes: usize,
) -> A11yResult<Vec<AccessibleNode>> {
    if max_nodes == 0 {
        return Ok(Vec::new());
    }
    let hwnd = native_hwnd_value(hwnd)?;
    with_automation_operation(
        format!(
            "chromium_renderer_accessibility_nodes_from_window hwnd=0x{hwnd:x} depth={depth} max_nodes={max_nodes}"
        ),
        SNAPSHOT_WORKER_REPLY_TIMEOUT,
        move |automation| {
            let root = automation
                .element_from_handle(Handle::from(hwnd))
                .map_err(map_uia_error)?;
            chromium_renderer_accessibility_nodes_from_root(automation, &root, depth, max_nodes)
        },
    )
}

fn chromium_renderer_accessibility_nodes_from_root(
    automation: &UIAutomation,
    root: &UIElement,
    depth: u32,
    max_nodes: usize,
) -> A11yResult<Vec<AccessibleNode>> {
    let class_name = root.get_classname().unwrap_or_default();
    if !is_chromium_widget_window_class(&class_name) {
        return Ok(Vec::new());
    }

    let root_cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Raw)?;
    let cached_root = root
        .build_updated_cache(&root_cache)
        .map_err(map_uia_error)?;
    let root_hwnd = cached_hwnd(&cached_root).unwrap_or(0);
    let root_id = element_id_from_cached_element(&cached_root, root_hwnd)?;
    let root_rect = cached_rect(&cached_root);

    let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Raw)?;
    let content_condition = automation
        .create_property_condition(UIProperty::IsContentElement, Variant::from(true), None)
        .map_err(map_uia_error)?;
    let elements = root
        .find_all_build_cache(TreeScope::Subtree, &content_condition, &cache)
        .map_err(map_uia_error)?;

    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    let node_depth = depth.max(1);
    for element in elements {
        if nodes.len() >= max_nodes {
            break;
        }
        let node = match node_from_cached_element(
            &element,
            Some(root_id.clone()),
            node_depth,
            root_hwnd,
            0,
        ) {
            Ok(node) => node,
            Err(error) => {
                tracing::warn!(
                    code = "A11Y_CHROMIUM_RENDERER_SUPPLEMENT_NODE_FAILED",
                    error = %error,
                    element_name = %element.get_cached_name().unwrap_or_default(),
                    element_class = %element.get_cached_classname().unwrap_or_default(),
                    control_type = ?element.get_cached_control_type().ok(),
                    automation_id = %element.get_cached_automation_id().unwrap_or_default(),
                    process_id = element.get_cached_process_id().unwrap_or(-1),
                    "Chromium renderer UIA supplement node read failed; node omitted"
                );
                continue;
            }
        };
        if is_chromium_renderer_content_node(&node, root_rect)
            && seen.insert(node.element_id.clone())
        {
            nodes.push(node);
        }
    }
    Ok(nodes)
}

fn find_by_name_and_pattern_from_root(
    automation: &UIAutomation,
    root: &UIElement,
    name: &str,
    pattern: UiaPattern,
    scope: ElementSearchScope,
) -> A11yResult<Option<AccessibleNode>> {
    if name.is_empty() {
        return Ok(None);
    }

    let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Raw)?;
    let name_condition = automation
        .create_property_condition(
            UIProperty::Name,
            Variant::from(name),
            Some(PropertyConditionFlags::IgnoreCase),
        )
        .map_err(map_uia_error)?;
    let pattern_condition = automation
        .create_property_condition(pattern_property(pattern), Variant::from(true), None)
        .map_err(map_uia_error)?;
    let condition = automation
        .create_and_condition(name_condition, pattern_condition)
        .map_err(map_uia_error)?;
    let elements = root
        .find_all_build_cache(scope.into(), &condition, &cache)
        .map_err(map_uia_error)?;
    let root_hwnd = root
        .build_updated_cache(&cache)
        .ok()
        .and_then(|cached_root| cached_hwnd(&cached_root))
        .unwrap_or(0);

    elements
        .into_iter()
        .filter(|element| element.is_cached_enabled().unwrap_or(true))
        .map(|element| node_from_cached_element(&element, None, 0, root_hwnd, 0))
        .next()
        .transpose()
}

fn valid_hwnd(hwnd: i64) -> A11yResult<HWND> {
    let native = native_hwnd_value(hwnd)?;
    let hwnd = HWND(native as *mut c_void);
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        Ok(hwnd)
    } else {
        Err(A11yError::NoForeground {
            detail: format!("HWND 0x{:x} is not a valid window", hwnd.0 as isize),
        })
    }
}

fn native_hwnd_value(hwnd: i64) -> A11yResult<isize> {
    hwnd_from_wire(hwnd).ok_or_else(|| A11yError::NoForeground {
        detail: format!(
            "HWND wire value {hwnd} is outside the canonical Win32 USER-handle range 1..=4294967295"
        ),
    })
}

fn focus_belongs_to_target(target: HWND, focus: HWND) -> bool {
    if same_native_hwnd(target, focus) {
        return true;
    }
    let target_root = ancestor_or_self(target, GA_ROOT);
    let focus_root = ancestor_or_self(focus, GA_ROOT);
    if same_native_hwnd(focus_root, target_root) {
        return true;
    }
    let target_owner = ancestor_or_self(target, GA_ROOTOWNER);
    let focus_owner = ancestor_or_self(focus, GA_ROOTOWNER);
    same_native_hwnd(focus_owner, target_root)
        || same_native_hwnd(focus_owner, target_owner)
        || owner_chain_contains(focus_root, target_root, target_owner)
}

fn same_native_hwnd(left: HWND, right: HWND) -> bool {
    native_hwnds_equal(left.0 as isize, right.0 as isize)
}

fn ancestor_or_self(
    hwnd: HWND,
    flags: windows::Win32::UI::WindowsAndMessaging::GET_ANCESTOR_FLAGS,
) -> HWND {
    let ancestor = unsafe { GetAncestor(hwnd, flags) };
    if ancestor.0.is_null() { hwnd } else { ancestor }
}

fn owner_chain_contains(mut hwnd: HWND, target_root: HWND, target_owner: HWND) -> bool {
    for _ in 0..32 {
        let owner = match unsafe { GetWindow(hwnd, GW_OWNER) } {
            Ok(owner) => owner,
            Err(_) => return false,
        };
        if owner.0.is_null() {
            return false;
        }
        if same_native_hwnd(owner, target_root) || same_native_hwnd(owner, target_owner) {
            return true;
        }
        let owner_root = ancestor_or_self(owner, GA_ROOT);
        if same_native_hwnd(owner_root, target_root) || same_native_hwnd(owner_root, target_owner) {
            return true;
        }
        hwnd = owner;
    }
    false
}
fn snapshot_at_depth(
    automation: &UIAutomation,
    root: &UIElement,
    depth: u32,
) -> A11yResult<AccessibleSubtree> {
    let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Raw)?;
    let true_condition = automation.create_true_condition().map_err(map_uia_error)?;
    let raw_walker = automation.get_raw_view_walker().map_err(map_uia_error)?;
    let cached_root = root.build_updated_cache(&cache).map_err(map_uia_error)?;
    let root_hwnd = cached_hwnd(&cached_root).unwrap_or(0);
    let root_id = element_id_from_cached_element(&cached_root, root_hwnd)?;
    if let Some(tree) = cached_snapshot(depth, &root_id) {
        return Ok(tree);
    }
    let mut nodes = Vec::new();
    let mut truncated = false;
    let walk = SnapshotWalk {
        true_condition: &true_condition,
        cache: &cache,
        raw_walker: &raw_walker,
        root_hwnd,
        node_budget: SNAPSHOT_NODE_BUDGET,
        deadline: Instant::now() + SNAPSHOT_DEADLINE,
    };
    collect_nodes(
        &walk,
        &cached_root,
        None,
        0,
        depth,
        &mut nodes,
        &mut truncated,
    )?;
    let root = nodes
        .first()
        .map(|node| node.element_id.clone())
        .ok_or_else(|| A11yError::ElementStale {
            detail: "snapshot root produced no UIA node".to_owned(),
        })?;
    Ok(AccessibleSubtree {
        root,
        nodes,
        max_depth: depth,
        truncated,
    })
}

fn supplement_raw_pattern_nodes(
    automation: &UIAutomation,
    root: &UIElement,
    nodes: &mut Vec<AccessibleNode>,
) -> A11yResult<bool> {
    let Some(root_id) = nodes.first().map(|node| node.element_id.clone()) else {
        return Ok(false);
    };
    if !should_supplement_raw_pattern_nodes(nodes.first().map(|node| node.name.as_str())) {
        return Ok(false);
    }
    let root_hwnd = root_id
        .parts()
        .map_err(|err| A11yError::InvalidElementId {
            detail: err.to_string(),
        })?
        .hwnd;
    let cache = create_cache_request(automation, 0, ElementMode::Full, TreeView::Raw)?;
    let mut seen: HashSet<ElementId> = nodes.iter().map(|node| node.element_id.clone()).collect();
    let mut truncated = false;
    for name in RAW_MENU_SUPPLEMENT_NAMES {
        let name_condition = automation
            .create_property_condition(
                UIProperty::Name,
                Variant::from(name),
                Some(PropertyConditionFlags::IgnoreCase),
            )
            .map_err(map_uia_error)?;
        let pattern_condition = automation
            .create_property_condition(
                UIProperty::IsExpandCollapsePatternAvailable,
                Variant::from(true),
                None,
            )
            .map_err(map_uia_error)?;
        let condition = automation
            .create_and_condition(name_condition, pattern_condition)
            .map_err(map_uia_error)?;
        let raw_elements = root
            .find_all_build_cache(TreeScope::Subtree, &condition, &cache)
            .map_err(map_uia_error)?;
        for element in raw_elements {
            if nodes.len() >= RAW_SUPPLEMENT_NODE_BUDGET {
                truncated = true;
                break;
            }
            let node = match node_from_cached_element(
                &element,
                Some(root_id.clone()),
                RAW_SUPPLEMENT_DEPTH,
                root_hwnd,
                0,
            ) {
                Ok(node) => node,
                Err(error) => {
                    truncated = true;
                    tracing::warn!(
                        code = "A11Y_RAW_SUPPLEMENT_NODE_FAILED",
                        error = %error,
                        element_name = %element.get_cached_name().unwrap_or_default(),
                        element_class = %element.get_cached_classname().unwrap_or_default(),
                        control_type = ?element.get_cached_control_type().ok(),
                        automation_id = %element.get_cached_automation_id().unwrap_or_default(),
                        process_id = element.get_cached_process_id().unwrap_or(-1),
                        "UIA raw supplement node read failed; node omitted and snapshot flagged truncated"
                    );
                    continue;
                }
            };
            if seen.insert(node.element_id.clone()) {
                nodes.push(node);
            }
        }
        if truncated {
            break;
        }
    }
    Ok(truncated)
}

fn cached_snapshot(depth: u32, root: &ElementId) -> Option<AccessibleSubtree> {
    let guard = SNAPSHOT_CACHE.lock().ok()?;
    let cache = guard.as_ref()?;
    let is_fresh = cache.requested_depth == depth
        && cache.root == *root
        && cache.captured_at.elapsed() <= Duration::from_millis(50);
    let tree = is_fresh.then(|| cache.tree.clone());
    drop(guard);
    tree
}

fn store_snapshot(depth: u32, tree: &AccessibleSubtree) {
    if let Ok(mut guard) = SNAPSHOT_CACHE.lock() {
        *guard = Some(SnapshotCache {
            requested_depth: depth,
            root: tree.root.clone(),
            captured_at: Instant::now(),
            tree: tree.clone(),
        });
    }
}

pub(super) fn invalidate_snapshot_cache() {
    if let Ok(mut guard) = SNAPSHOT_CACHE.lock() {
        *guard = None;
    }
}

fn collect_nodes(
    walk: &SnapshotWalk<'_>,
    element: &UIElement,
    parent: Option<ElementId>,
    depth: u32,
    max_depth: u32,
    nodes: &mut Vec<AccessibleNode>,
    truncated: &mut bool,
) -> A11yResult<ElementId> {
    if let Some(reason) =
        collection_limit_reason(nodes.len(), walk.node_budget, Instant::now(), walk.deadline)
    {
        *truncated = true;
        return Err(A11yError::internal(format!(
            "UIA snapshot {reason} reached before collecting node"
        )));
    }
    let node = node_from_cached_element(element, parent, depth, walk.root_hwnd, 0)?;
    let node_id = node.element_id.clone();
    let node_index = nodes.len();
    nodes.push(node);
    if depth >= max_depth {
        return Ok(node_id);
    }
    if collection_limit_reason(nodes.len(), walk.node_budget, Instant::now(), walk.deadline)
        .is_some()
    {
        *truncated = true;
        return Ok(node_id);
    }
    collect_child_nodes(
        walk, element, &node_id, node_index, depth, max_depth, nodes, truncated,
    );
    Ok(node_id)
}

// UIA tree-walk recursion threads the full walk context (depth/max_depth, parent id
// and index, output sink, truncation flag); a wrapper struct would not improve clarity.
#[allow(clippy::too_many_arguments)]
fn collect_child_nodes(
    walk: &SnapshotWalk<'_>,
    element: &UIElement,
    node_id: &ElementId,
    node_index: usize,
    depth: u32,
    max_depth: u32,
    nodes: &mut Vec<AccessibleNode>,
    truncated: &mut bool,
) {
    if should_use_bulk_child_fallback(element) {
        collect_child_nodes_from_bulk(
            walk, element, node_id, node_index, depth, max_depth, nodes, truncated,
        );
        return;
    }

    let Ok(mut child) = walk
        .raw_walker
        .get_first_child_build_cache(element, walk.cache)
    else {
        return;
    };
    loop {
        if let Some(reason) =
            collection_limit_reason(nodes.len(), walk.node_budget, Instant::now(), walk.deadline)
        {
            *truncated = true;
            tracing::debug!(
                code = "A11Y_SNAPSHOT_WALK_TRUNCATED",
                reason,
                nodes = nodes.len(),
                node_budget = walk.node_budget,
                depth,
                "UIA snapshot collection stopped before remaining siblings"
            );
            break;
        }
        let child_name = child.get_cached_name().unwrap_or_default();
        let child_class = child.get_cached_classname().unwrap_or_default();
        let child_control_type = child.get_cached_control_type().ok();
        let child_automation_id = child.get_cached_automation_id().unwrap_or_default();
        let child_process_id = child.get_cached_process_id().unwrap_or(-1);
        if let Err(error) = collect_nodes(
            walk,
            &child,
            Some(node_id.clone()),
            depth + 1,
            max_depth,
            nodes,
            truncated,
        ) {
            *truncated = true;
            tracing::warn!(
                code = "A11Y_CHILD_NODE_FAILED",
                error = %error,
                depth = depth + 1,
                element_name = %child_name,
                element_class = %child_class,
                control_type = ?child_control_type,
                automation_id = %child_automation_id,
                process_id = child_process_id,
                "UIA child node read failed; node omitted and snapshot flagged truncated"
            );
        } else if let Some(parent) = nodes.get_mut(node_index) {
            parent.children_count = parent.children_count.saturating_add(1);
        }
        match walk
            .raw_walker
            .get_next_sibling_build_cache(&child, walk.cache)
        {
            Ok(next) => child = next,
            Err(_) => break,
        }
    }
}

// Mirrors `collect_child_nodes`' walk context (see above); same justified arg count.
#[allow(clippy::too_many_arguments)]
fn collect_child_nodes_from_bulk(
    walk: &SnapshotWalk<'_>,
    element: &UIElement,
    node_id: &ElementId,
    node_index: usize,
    depth: u32,
    max_depth: u32,
    nodes: &mut Vec<AccessibleNode>,
    truncated: &mut bool,
) {
    // `find_all_build_cache(Children, true)` reliably crosses the
    // cross-process `Windows.UI.Core.CoreWindow` boundary for UWP apps. Keep it
    // only for those known app-frame nodes; normal high-fanout trees use the
    // streaming walker above so a single child enumeration cannot monopolize
    // the snapshot deadline.
    let children =
        match element.find_all_build_cache(TreeScope::Children, walk.true_condition, walk.cache) {
            Ok(children) => children,
            Err(err) => {
                *truncated = true;
                tracing::warn!(
                    code = "A11Y_CHILD_ENUM_FAILED",
                    error = %err,
                    depth,
                    element_name = %element.get_cached_name().unwrap_or_default(),
                    element_class = %element.get_cached_classname().unwrap_or_default(),
                    control_type = ?element.get_cached_control_type().ok(),
                    automation_id = %element.get_cached_automation_id().unwrap_or_default(),
                    process_id = element.get_cached_process_id().unwrap_or(-1),
                    "UIA child enumeration failed; subtree omitted and snapshot flagged truncated"
                );
                return;
            }
        };
    for child in children {
        if let Some(reason) =
            collection_limit_reason(nodes.len(), walk.node_budget, Instant::now(), walk.deadline)
        {
            *truncated = true;
            tracing::debug!(
                code = "A11Y_SNAPSHOT_WALK_TRUNCATED",
                reason,
                nodes = nodes.len(),
                node_budget = walk.node_budget,
                depth,
                "UIA snapshot collection stopped before remaining siblings"
            );
            break;
        }
        let child_name = child.get_cached_name().unwrap_or_default();
        let child_class = child.get_cached_classname().unwrap_or_default();
        let child_control_type = child.get_cached_control_type().ok();
        let child_automation_id = child.get_cached_automation_id().unwrap_or_default();
        let child_process_id = child.get_cached_process_id().unwrap_or(-1);
        if let Err(error) = collect_nodes(
            walk,
            &child,
            Some(node_id.clone()),
            depth + 1,
            max_depth,
            nodes,
            truncated,
        ) {
            *truncated = true;
            tracing::warn!(
                code = "A11Y_CHILD_NODE_FAILED",
                error = %error,
                depth = depth + 1,
                element_name = %child_name,
                element_class = %child_class,
                control_type = ?child_control_type,
                automation_id = %child_automation_id,
                process_id = child_process_id,
                "UIA child node read failed; node omitted and snapshot flagged truncated"
            );
        } else if let Some(parent) = nodes.get_mut(node_index) {
            parent.children_count = parent.children_count.saturating_add(1);
        }
    }
}

fn collection_limit_reason(
    nodes_len: usize,
    node_budget: usize,
    now: Instant,
    deadline: Instant,
) -> Option<&'static str> {
    if nodes_len >= node_budget {
        Some("node_budget")
    } else if now >= deadline {
        Some("deadline")
    } else {
        None
    }
}

fn should_use_bulk_child_fallback(element: &UIElement) -> bool {
    let class_name = element.get_cached_classname().unwrap_or_default();
    let class_name = class_name.as_str();
    class_name == "ApplicationFrameWindow"
        || class_name == "Windows.UI.Core.CoreWindow"
        || class_name == "ApplicationFrameInputSinkWindow"
}

fn should_supplement_raw_pattern_nodes(root_name: Option<&str>) -> bool {
    let Some(root_name) = root_name else {
        return false;
    };
    root_name == "Notepad" || root_name.ends_with(" - Notepad")
}

fn is_chromium_widget_window_class(class_name: &str) -> bool {
    class_name.starts_with("Chrome_WidgetWin_")
}

fn is_chromium_renderer_content_node(node: &AccessibleNode, root_rect: synapse_core::Rect) -> bool {
    if node.bbox.w <= 0 || node.bbox.h <= 0 {
        return false;
    }
    if node.bbox.y < chromium_renderer_content_top(root_rect) {
        return false;
    }
    if node
        .automation_id
        .as_deref()
        .is_some_and(|automation_id| automation_id.starts_with("view_"))
    {
        return false;
    }

    let role = node.role.to_ascii_lowercase();
    let content_role = matches!(
        role.as_str(),
        "button"
            | "check box"
            | "combo box"
            | "document"
            | "edit"
            | "group"
            | "heading"
            | "hyperlink"
            | "image"
            | "link"
            | "list"
            | "list item"
            | "pane"
            | "radio button"
            | "table"
            | "text"
    );
    if !content_role {
        return false;
    }
    role == "document"
        || !node.name.trim().is_empty()
        || node
            .value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || !node.patterns.is_empty()
}

fn chromium_renderer_content_top(root_rect: synapse_core::Rect) -> i32 {
    if root_rect.h <= 240 {
        return root_rect.y;
    }
    root_rect
        .y
        .saturating_add(CHROMIUM_RENDERER_CONTENT_TOP_INSET_PX.min(root_rect.h / 3))
}

fn node_from_cached_element(
    element: &UIElement,
    parent: Option<ElementId>,
    depth: u32,
    root_hwnd: i64,
    children_count: usize,
) -> A11yResult<AccessibleNode> {
    let hwnd = cached_hwnd(element)
        .filter(|value| *value != 0)
        .unwrap_or(root_hwnd);
    Ok(AccessibleNode {
        element_id: element_id_from_cached_element(element, hwnd)?,
        parent,
        name: element.get_cached_name().unwrap_or_default(),
        role: cached_role(element),
        automation_id: non_empty(element.get_cached_automation_id().unwrap_or_default()),
        value: cached_value(element),
        bbox: cached_rect(element),
        enabled: element.is_cached_enabled().unwrap_or(false),
        focused: element.has_cached_keyboard_focus().unwrap_or(false),
        patterns: cached_patterns(element),
        children_count: u32::try_from(children_count).unwrap_or(u32::MAX),
        depth,
    })
}

fn element_id_from_cached_element(element: &UIElement, hwnd: i64) -> A11yResult<ElementId> {
    let readback = cached_runtime_id_hex_or_fallback(element, hwnd)?;
    if readback.used_fallback {
        tracing::warn!(
            code = "A11Y_RUNTIME_ID_UNAVAILABLE",
            hwnd,
            fallback_runtime_id_hex = %readback.hex,
            element_name = %element.get_cached_name().unwrap_or_default(),
            element_class = %element.get_cached_classname().unwrap_or_default(),
            control_type = ?element.get_cached_control_type().ok(),
            automation_id = %element.get_cached_automation_id().unwrap_or_default(),
            process_id = element.get_cached_process_id().unwrap_or(-1),
            "UIA RuntimeId was unavailable; generated a process-local fallback element id"
        );
    }
    Ok(element_id(hwnd, &readback.hex))
}
