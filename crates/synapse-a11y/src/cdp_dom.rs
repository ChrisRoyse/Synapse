//! CDP DOM/accessibility tree → queryable `AccessibleNode` mapping (#685).
//!
//! When CDP is attached to a Chromium-family foreground, this module pulls the
//! page's `Accessibility.getFullAXTree` (+ `DOM.getBoxModel` for bounds) and maps
//! each node into the same [`AccessibleNode`] model the UIA path uses, so an
//! agent can `find(role="button", name_substring="Apply")` on a web page exactly
//! as it does on native UI.
//!
//! The pure mapping ([`build_accessible_nodes`]) is unit-tested with real CDP
//! response shapes; the async fetch ([`fetch_dom_snapshot`]) is the I/O wrapper
//! verified manually against a live Chrome (see `examples/cdp_axtree_probe.rs`).
//!
//! ## Element id scheme
//!
//! Web nodes get an [`ElementId`] of
//! `<hwnd_hex>:cdcd<targetId-hex><backendNodeId-hex>`. The `cdcd` sentinel lets
//! the action layer (#686) recognise a CDP-resolved node and route it back
//! through CDP (`DOM.scrollIntoViewIfNeeded` + box model + `Input.dispatch*`)
//! instead of UIA re-resolution. The target id and backendNodeId round-trip out
//! of the id with no side registry. Legacy backend-only ids still parse, but
//! new observations carry the target id so duplicate tab titles cannot steer
//! follow-up `actions/read_text` to the wrong document.
//!
//! ## bbox semantics
//!
//! Web-node `bbox` is the element's CSS-pixel rectangle in page-layout
//! coordinates (from `DOM.getBoxModel`), NOT screen pixels. Actions do not rely
//! on it — they re-resolve the live box model at click time after scrolling the
//! node into view — but it lets an agent reason about on-page layout.

use synapse_core::{AccessibleNode, ElementId, Rect, element_id};

/// Sentinel prefix in the runtime-id portion of a web node's [`ElementId`].
/// Hex-only so the id still parses; `cdcd` is vanishingly unlikely to collide
/// with a real UIA runtime id.
pub const CDP_RUNTIME_PREFIX: &str = "cdcd";
const CDP_BACKEND_NODE_HEX_LEN: usize = 12;

/// One node distilled from a CDP `AXNode` (+ its resolved box model). This is the
/// crate-internal, browser-free representation the pure mapper consumes so it can
/// be unit-tested without a live Chrome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdpDomNode {
    /// `DOM.BackendNodeId` of the associated DOM node (stable within a document).
    pub backend_node_id: i64,
    /// Backend id of this node's nearest mapped ancestor, if any.
    pub parent_backend_node_id: Option<i64>,
    /// Computed ARIA/AX role (e.g. `button`, `link`, `textbox`, `heading`).
    pub role: String,
    /// Accessible name.
    pub name: String,
    /// Accessible value (form fields, sliders), if any.
    pub value: Option<String>,
    /// CDP Page.FrameId owning this node, when known.
    pub frame_id: Option<String>,
    /// Element rectangle in CSS px / page-layout coords, if a box model resolved.
    pub bbox: Option<Rect>,
    /// Number of mapped child nodes.
    pub child_count: u32,
    /// Whether the node is enabled (defaults true unless AX reports disabled).
    pub enabled: bool,
    /// Whether the node is focused.
    pub focused: bool,
}

/// Builds an [`ElementId`] for a web node from the browser window `hwnd` and the
/// DOM `backend_node_id`.
#[must_use]
pub fn cdp_element_id(hwnd: i64, backend_node_id: i64) -> ElementId {
    // Backend ids are non-negative and small; mask to a stable unsigned hex.
    let unsigned = u64::from_ne_bytes(backend_node_id.to_ne_bytes());
    element_id(
        hwnd,
        &format!("{CDP_RUNTIME_PREFIX}{unsigned:0CDP_BACKEND_NODE_HEX_LEN$x}"),
    )
}

/// Builds a CDP web-node id that carries both the selected target id and the
/// backend node id. If Chromium ever returns a non-hex target id, fall back to
/// the legacy backend-only shape rather than emitting an invalid [`ElementId`].
#[must_use]
pub fn cdp_element_id_for_target(hwnd: i64, target_id: &str, backend_node_id: i64) -> ElementId {
    let target_id = target_id.trim();
    if target_id.is_empty() || !target_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return cdp_element_id(hwnd, backend_node_id);
    }
    let unsigned = u64::from_ne_bytes(backend_node_id.to_ne_bytes());
    element_id(
        hwnd,
        &format!(
            "{CDP_RUNTIME_PREFIX}{}{unsigned:0CDP_BACKEND_NODE_HEX_LEN$x}",
            target_id.to_ascii_lowercase()
        ),
    )
}

/// If `id` is a CDP web-node id (`…:cdcd<hex>`), returns its `backend_node_id`.
/// Returns `None` for ordinary UIA element ids so the action layer can tell them
/// apart.
#[must_use]
pub fn cdp_backend_from_element_id(id: &ElementId) -> Option<i64> {
    let parts = id.parts().ok()?;
    let hex = parts.runtime_id_hex.strip_prefix(CDP_RUNTIME_PREFIX)?;
    let backend_hex = hex
        .get(hex.len().saturating_sub(CDP_BACKEND_NODE_HEX_LEN)..)
        .filter(|value| value.len() == CDP_BACKEND_NODE_HEX_LEN)?;
    let unsigned = u64::from_str_radix(backend_hex, 16).ok()?;
    Some(i64::from_ne_bytes(unsigned.to_ne_bytes()))
}

/// If `id` is a target-aware CDP web-node id, returns the CDP `TargetID` encoded
/// in it. Legacy backend-only ids return `None`.
#[must_use]
pub fn cdp_target_from_element_id(id: &ElementId) -> Option<String> {
    let parts = id.parts().ok()?;
    let hex = parts.runtime_id_hex.strip_prefix(CDP_RUNTIME_PREFIX)?;
    let target_hex = hex.get(..hex.len().checked_sub(CDP_BACKEND_NODE_HEX_LEN)?)?;
    if target_hex.is_empty() {
        return None;
    }
    Some(target_hex.to_ascii_uppercase())
}

/// Pure mapping: CDP nodes → `AccessibleNode`s.
///
/// The mapper assigns stable ids, computes depth from the parent-backend chain,
/// and carries bounds through. Nodes are returned in input order, capped at
/// `max_nodes`. The mapping never invents data — a node with no box model gets a
/// zero `bbox` (callers may filter on it).
#[must_use]
pub fn build_accessible_nodes(
    hwnd: i64,
    nodes: &[CdpDomNode],
    max_nodes: usize,
) -> Vec<AccessibleNode> {
    build_accessible_nodes_for_target(hwnd, None, nodes, max_nodes)
}

/// Pure mapping variant that embeds the selected page target in every web node
/// id so later CDP `actions/read_text` resolve the same tab the observation read.
#[must_use]
pub fn build_accessible_nodes_for_target(
    hwnd: i64,
    target_id: Option<&str>,
    nodes: &[CdpDomNode],
    max_nodes: usize,
) -> Vec<AccessibleNode> {
    use std::collections::HashMap;

    // Depth = chain length from a root (a node whose parent is absent or not in
    // the set). Memoised so a deep tree stays linear.
    let by_backend: HashMap<i64, &CdpDomNode> = nodes
        .iter()
        .map(|node| (node.backend_node_id, node))
        .collect();
    let mut depth_cache: HashMap<i64, u32> = HashMap::new();

    nodes
        .iter()
        .take(max_nodes)
        .map(|node| {
            let depth = depth_of(node.backend_node_id, &by_backend, &mut depth_cache, 256);
            let element_id = target_id.map_or_else(
                || cdp_element_id(hwnd, node.backend_node_id),
                |target_id| cdp_element_id_for_target(hwnd, target_id, node.backend_node_id),
            );
            AccessibleNode {
                element_id,
                parent: node
                    .parent_backend_node_id
                    .filter(|parent| by_backend.contains_key(parent))
                    .map(|parent| {
                        target_id.map_or_else(
                            || cdp_element_id(hwnd, parent),
                            |target_id| cdp_element_id_for_target(hwnd, target_id, parent),
                        )
                    }),
                name: node.name.clone(),
                role: node.role.clone(),
                automation_id: Some(cdp_automation_id(target_id, node)),
                value: node.value.clone(),
                bbox: node.bbox.unwrap_or(Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                }),
                enabled: node.enabled,
                focused: node.focused,
                patterns: Vec::new(),
                children_count: node.child_count,
                depth,
            }
        })
        .collect()
}

fn cdp_automation_id(target_id: Option<&str>, node: &CdpDomNode) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(target_id) = target_id.filter(|value| !value.trim().is_empty()) {
        parts.push(format!("targetId={}", target_id.trim()));
    }
    if let Some(frame_id) = node
        .frame_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!("frameId={}", frame_id.trim()));
    }
    parts.push(format!("backendNodeId={}", node.backend_node_id));
    format!("cdp:{}", parts.join(";"))
}

/// Depth of `backend` = chain length to a root, memoised in `cache`. `guard`
/// bounds recursion against a malformed (cyclic) parent chain.
fn depth_of(
    backend: i64,
    by_backend: &std::collections::HashMap<i64, &CdpDomNode>,
    cache: &mut std::collections::HashMap<i64, u32>,
    guard: u32,
) -> u32 {
    if let Some(found) = cache.get(&backend) {
        return *found;
    }
    if guard == 0 {
        return 0;
    }
    let depth = by_backend
        .get(&backend)
        .and_then(|node| node.parent_backend_node_id)
        .filter(|parent| by_backend.contains_key(parent))
        .map_or(0, |parent| {
            depth_of(parent, by_backend, cache, guard - 1) + 1
        });
    cache.insert(backend, depth);
    depth
}

/// Axis-aligned bounding rect of a CDP box-model content quad
/// (`[x1,y1,x2,y2,x3,y3,x4,y4]`). Returns `None` for a malformed quad.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "page coordinates are rounded then cast into the i32 Rect space"
)]
pub fn rect_from_quad(quad: &[f64]) -> Option<Rect> {
    if quad.len() < 8 {
        return None;
    }
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let min_y = ys.iter().copied().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let max_y = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !(min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite()) {
        return None;
    }
    Some(Rect {
        x: min_x.round() as i32,
        y: min_y.round() as i32,
        w: (max_x - min_x).round().max(0.0) as i32,
        h: (max_y - min_y).round().max(0.0) as i32,
    })
}

/// A fully-resolved CDP DOM snapshot ready to fold into observation elements.
#[derive(Clone, Debug)]
pub struct CdpDomSnapshot {
    /// Mapped, queryable web nodes.
    pub nodes: Vec<AccessibleNode>,
    /// Total non-ignored AX nodes the page exposed (before `max_nodes` capping).
    pub total_ax_nodes: u32,
    /// Page URL the snapshot came from (diagnostics).
    pub page_url: String,
    /// CDP `TargetID` selected for this snapshot.
    pub target_id: String,
    /// CDP flat-session id attached to the selected target.
    pub session_id: String,
    /// Number of live page targets considered when selecting the tab.
    pub target_candidate_count: u32,
    /// Machine-readable reason this target was selected.
    pub target_selection_reason: String,
    /// Number of frame documents enumerated from the selected page target.
    pub frame_tree_frame_count: u32,
    /// Number of related iframe targets reached through flat-session attachment.
    pub attached_frame_target_count: u32,
    /// Related iframe targets discovered but not surfaced, with explicit reasons.
    pub blocked_frame_targets: Vec<String>,
    /// Non-fatal per-frame snapshot errors. Root-page attach/tree failures still fail loud.
    pub frame_snapshot_errors: Vec<String>,
}

/// One frame document from `browser_frames` / raw-CDP frame enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdpFrameTreeEntry {
    /// CDP Page.FrameId for this document.
    pub frame_id: String,
    /// Parent Page.FrameId, when this is not the main frame.
    pub parent_frame_id: Option<String>,
    /// CDP target that owns this frame document.
    pub cdp_target_id: String,
    /// CDP target type for `cdp_target_id` (`page` or `iframe`).
    pub target_type: String,
    /// Whether Target.getTargets reported this target as attached.
    pub target_attached: Option<bool>,
    /// Document URL reported by Page.getFrameTree.
    pub url: String,
    /// Frame name from the frame element/name attribute, when present.
    pub name: Option<String>,
    /// Best available document origin.
    pub origin: String,
    /// Chromium securityOrigin, when available.
    pub security_origin: Option<String>,
    /// Loader id for the frame document.
    pub loader_id: Option<String>,
    /// Depth in the composed frame tree, with the main frame at 0.
    pub depth: u32,
    /// Zero-based sibling index under the parent frame.
    pub sibling_index: u32,
    /// Number of direct child frames known from Page.getFrameTree.
    pub child_count: u32,
    /// True when this frame is represented by a separate iframe target.
    pub is_out_of_process: bool,
    /// Synapse element id for the owning iframe/frame element, when CDP exposes it.
    pub frame_element_id: Option<String>,
    /// `BackendNodeId` for the owning iframe/frame element, when CDP exposes it.
    pub frame_element_backend_node_id: Option<i64>,
    /// CDP target that owns the iframe/frame element id.
    pub frame_element_cdp_target_id: Option<String>,
    /// How the owning frame element id was resolved.
    pub frame_element_source: String,
}

/// Structured result for background-safe raw-CDP frame enumeration.
#[derive(Clone, Debug)]
pub struct CdpFrameListResult {
    pub endpoint: String,
    pub target_id: String,
    pub session_id: String,
    pub page_url: String,
    pub page_title: String,
    pub frame_count: usize,
    pub oopif_target_count: u32,
    pub attached_frame_target_count: u32,
    pub blocked_frame_targets: Vec<String>,
    pub frame_snapshot_errors: Vec<String>,
    pub frames: Vec<CdpFrameTreeEntry>,
}

/// Attaches CDP at `endpoint` and maps the selected tab into web nodes.
///
/// The selected tab is an existing live target from `Target.getTargets`. Synapse
/// prefers an exact foreground URL hint, then a unique foreground-title match,
/// and only falls back to the first discovered page when there is no stronger
/// signal. The snapshot pulls `Accessibility.getFullAXTree` and per-node box
/// models, then maps everything into queryable [`AccessibleNode`]s owned by
/// `hwnd`.
///
/// Fail-loud: any attach/tree failure returns an `A11yError` with a specific
/// code (`A11Y_CDP_ATTACH_FAILED` / `A11Y_CDP_AXTREE_FAILED`) — never a silent
/// empty tree.
///
/// # Errors
///
/// Returns [`crate::A11yError::CdpAttachFailed`] when the client cannot connect
/// or no page target exists, and [`crate::A11yError::CdpAxtreeFailed`] when the
/// accessibility tree cannot be retrieved.
#[cfg(windows)]
#[allow(
    clippy::future_not_send,
    clippy::too_many_lines,
    reason = "single CDP attach/read transaction keeps browser handler lifetime explicit"
)]
pub async fn fetch_dom_snapshot(
    endpoint: &str,
    hwnd: i64,
    foreground_title: &str,
    foreground_url_hint: Option<&str>,
    target_id_hint: Option<&str>,
    max_nodes: usize,
) -> crate::A11yResult<CdpDomSnapshot> {
    use std::collections::HashSet;

    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
    use futures_util::StreamExt as _;

    use crate::A11yError;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let pages = wait_for_pages(&browser).await?;
        let target_infos = browser
            .execute(GetTargetsParams::default())
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("Target.getTargets: {err}"),
            })?
            .result
            .target_infos;

        let selection = select_existing_page(
            pages,
            &target_infos,
            foreground_title,
            foreground_url_hint,
            target_id_hint,
        )
        .await?;
        let page = selection.page;
        let page_url = page
            .url()
            .await
            .ok()
            .flatten()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| selection.target_url.clone());

        let mut frame_snapshot_errors = Vec::new();
        if let Err(detail) = enable_flat_iframe_auto_attach(&page).await {
            frame_snapshot_errors.push(detail);
        } else {
            // Give the handler one tick to receive Target.attachedToTarget events
            // for already-present child frame targets before we read getTargets.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let target_infos = match browser.execute(GetTargetsParams::default()).await {
            Ok(response) => response.result.target_infos,
            Err(error) => {
                frame_snapshot_errors.push(format!(
                    "Target.getTargets after iframe auto-attach failed: {error}; using initial target table"
                ));
                target_infos
            }
        };

        let main_read =
            fetch_target_dom_read(&page, &selection.target_id, max_nodes).await?;
        frame_snapshot_errors.extend(main_read.frame_snapshot_errors.clone());

        let mut nodes = build_accessible_nodes_for_target(
            hwnd,
            Some(&selection.target_id),
            &main_read.dom_nodes,
            max_nodes,
        );
        let mut total_ax_nodes = main_read.total_ax_nodes;
        let mut frame_tree_frame_count = main_read.frame_tree_frame_count;
        let mut attached_frame_target_count = 0_u32;
        let mut blocked_frame_targets = Vec::new();
        let mut known_frame_ids = main_read.frame_ids.clone();
        let mut seen_oopif_targets = HashSet::from([selection.target_id.clone()]);

        loop {
            let mut related_targets = related_iframe_targets(&target_infos, &known_frame_ids)
                .into_iter()
                .filter(|target| seen_oopif_targets.insert(target.target_id.inner().clone()))
                .cloned()
                .collect::<Vec<_>>();
            related_targets.sort_by(|a, b| a.target_id.inner().cmp(b.target_id.inner()));
            if related_targets.is_empty() {
                break;
            }

            for target in related_targets {
                if nodes.len() >= max_nodes {
                    blocked_frame_targets.push(format!(
                        "iframe target {} ({}) skipped because max_nodes={} was already reached",
                        target.target_id.inner(),
                        target.url,
                        max_nodes
                    ));
                    continue;
                }
                let iframe_page = match wait_for_page_target(&browser, target.target_id.clone()).await
                {
                    Ok(page) => page,
                    Err(error) => {
                        blocked_frame_targets.push(format!(
                            "iframe target {} parentFrameId={} url={} was discovered in Target.getTargets but did not expose a callable flat session through chromiumoxide: {error}",
                            target.target_id.inner(),
                            target
                                .parent_frame_id
                                .as_ref()
                                .map_or("", |frame_id| frame_id.inner().as_str()),
                            target.url
                        ));
                        continue;
                    }
                };
                let remaining = max_nodes.saturating_sub(nodes.len());
                match fetch_target_dom_read(&iframe_page, target.target_id.inner(), remaining).await
                {
                    Ok(read) => {
                        attached_frame_target_count = attached_frame_target_count.saturating_add(1);
                        total_ax_nodes = total_ax_nodes.saturating_add(read.total_ax_nodes);
                        frame_tree_frame_count =
                            frame_tree_frame_count.saturating_add(read.frame_tree_frame_count);
                        frame_snapshot_errors.extend(read.frame_snapshot_errors.clone());
                        for frame_id in &read.frame_ids {
                            if !known_frame_ids.contains(frame_id) {
                                known_frame_ids.push(frame_id.clone());
                            }
                        }
                        nodes.extend(build_accessible_nodes_for_target(
                            hwnd,
                            Some(target.target_id.inner()),
                            &read.dom_nodes,
                            remaining,
                        ));
                    }
                    Err(error) => {
                        blocked_frame_targets.push(format!(
                            "iframe target {} parentFrameId={} url={} attached but DOM/AX snapshot failed: {error}",
                            target.target_id.inner(),
                            target
                                .parent_frame_id
                                .as_ref()
                                .map_or("", |frame_id| frame_id.inner().as_str()),
                            target.url
                        ));
                    }
                }
            }
        }
        Ok(CdpDomSnapshot {
            nodes,
            total_ax_nodes,
            page_url,
            target_id: selection.target_id,
            session_id: selection.session_id,
            target_candidate_count: selection.target_candidate_count,
            target_selection_reason: selection.selection_reason,
            frame_tree_frame_count,
            attached_frame_target_count,
            blocked_frame_targets,
            frame_snapshot_errors,
        })
    }
    .await;

    handler_task.abort();
    result
}

/// Enumerates the composed frame tree for an existing CDP target without
/// activating the tab or reading the human foreground window.
///
/// # Errors
///
/// Returns [`crate::A11yError::CdpAttachFailed`] when the client cannot connect
/// or attach to `target_id`, and [`crate::A11yError::CdpAxtreeFailed`] when the
/// root frame tree cannot be read.
#[cfg(windows)]
#[allow(
    clippy::future_not_send,
    clippy::too_many_lines,
    reason = "single CDP attach/read transaction keeps browser handler lifetime explicit"
)]
pub async fn cdp_list_frames(
    endpoint: &str,
    hwnd: i64,
    target_id: &str,
) -> crate::A11yResult<CdpFrameListResult> {
    use std::collections::{HashMap, HashSet};

    use chromiumoxide::Browser;
    use chromiumoxide::cdp::browser_protocol::target::{GetTargetsParams, TargetId, TargetInfo};
    use futures_util::StreamExt as _;

    use crate::A11yError;

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = async {
        let page = wait_for_page_target(&browser, TargetId::new(target_id.to_owned()))
            .await
            .map_err(|error| A11yError::CdpAttachFailed {
                detail: format!("target {target_id} did not expose a callable page: {error}"),
            })?;
        let page_url = page.url().await.ok().flatten().unwrap_or_default();
        let page_title = page.get_title().await.ok().flatten().unwrap_or_default();
        let session_id = page.session_id().inner().clone();

        let mut frame_snapshot_errors = Vec::new();
        if let Err(detail) = enable_flat_iframe_auto_attach(&page).await {
            frame_snapshot_errors.push(detail);
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let target_infos = browser
            .execute(GetTargetsParams::default())
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("Target.getTargets: {err}"),
            })?
            .result
            .target_infos;
        let target_info_by_id: HashMap<String, TargetInfo> = target_infos
            .iter()
            .map(|info| (info.target_id.inner().clone(), info.clone()))
            .collect();
        let root_target_info = target_info_by_id.get(target_id);
        let root_attached = root_target_info.map(|info| info.attached);
        let root_target_type = root_target_info.map_or("page", |info| info.r#type.as_str());

        let mut owner_elements = HashMap::new();
        let mut frames = frame_entries_for_page(
            &page,
            hwnd,
            target_id,
            root_target_type,
            root_attached,
            false,
            None,
            0,
            &mut owner_elements,
            &mut frame_snapshot_errors,
        )
        .await?;

        let mut seen_oopif_targets = HashSet::from([target_id.to_owned()]);
        let mut blocked_frame_targets = Vec::new();
        let mut attached_frame_target_count = 0_u32;

        loop {
            let known_frame_ids = frames
                .iter()
                .map(|frame| frame.frame_id.clone())
                .collect::<Vec<_>>();
            let mut related_targets = related_iframe_targets(&target_infos, &known_frame_ids)
                .into_iter()
                .filter(|target| seen_oopif_targets.insert(target.target_id.inner().clone()))
                .cloned()
                .collect::<Vec<_>>();
            related_targets.sort_by(|a, b| a.target_id.inner().cmp(b.target_id.inner()));
            if related_targets.is_empty() {
                break;
            }

            for target in related_targets {
                let target_id = target.target_id.inner().clone();
                let parent_frame_id = target.parent_frame_id.as_ref().map(|id| id.inner().clone());
                let parent_depth = parent_frame_id
                    .as_deref()
                    .and_then(|id| frames.iter().find(|frame| frame.frame_id == id))
                    .map_or(0, |frame| frame.depth.saturating_add(1));
                let iframe_page = match wait_for_page_target(&browser, target.target_id.clone()).await
                {
                    Ok(page) => page,
                    Err(error) => {
                        blocked_frame_targets.push(format!(
                            "iframe target {} parentFrameId={} url={} was discovered in Target.getTargets but did not expose a callable flat session through chromiumoxide: {error}",
                            target_id,
                            parent_frame_id.as_deref().unwrap_or(""),
                            target.url
                        ));
                        continue;
                    }
                };

                attached_frame_target_count = attached_frame_target_count.saturating_add(1);
                let mut target_entries = match frame_entries_for_page(
                    &iframe_page,
                    hwnd,
                    &target_id,
                    target.r#type.as_str(),
                    Some(target.attached),
                    true,
                    parent_frame_id.as_deref(),
                    parent_depth,
                    &mut owner_elements,
                    &mut frame_snapshot_errors,
                )
                .await
                {
                    Ok(entries) => entries,
                    Err(error) => {
                        blocked_frame_targets.push(format!(
                            "iframe target {} parentFrameId={} url={} attached but Page.getFrameTree failed: {error}",
                            target_id,
                            parent_frame_id.as_deref().unwrap_or(""),
                            target.url
                        ));
                        continue;
                    }
                };
                for entry in &mut target_entries {
                    apply_frame_owner(entry, &owner_elements);
                }
                merge_frame_entries(&mut frames, target_entries);
            }
        }

        for frame in &mut frames {
            apply_frame_owner(frame, &owner_elements);
        }
        frames.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| a.parent_frame_id.cmp(&b.parent_frame_id))
                .then_with(|| a.sibling_index.cmp(&b.sibling_index))
                .then_with(|| a.frame_id.cmp(&b.frame_id))
        });

        Ok(CdpFrameListResult {
            endpoint: endpoint.to_owned(),
            target_id: target_id.to_owned(),
            session_id,
            page_url,
            page_title,
            frame_count: frames.len(),
            oopif_target_count: u32::try_from(
                frames.iter().filter(|frame| frame.is_out_of_process).count(),
            )
            .unwrap_or(u32::MAX),
            attached_frame_target_count,
            blocked_frame_targets,
            frame_snapshot_errors,
            frames,
        })
    }
    .await;

    handler_task.abort();
    result
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct CdpTargetDomRead {
    dom_nodes: Vec<CdpDomNode>,
    total_ax_nodes: u32,
    frame_tree_frame_count: u32,
    frame_ids: Vec<String>,
    frame_snapshot_errors: Vec<String>,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct CdpFrameDescriptor {
    frame_id: Option<chromiumoxide::cdp::browser_protocol::page::FrameId>,
    frame_id_wire: Option<String>,
}

/// Turns on flat (sessionId-routed) auto-attach for child `iframe` targets of
/// `page`. Sharing this with the action layer (`cdp_action`) lets a write/click
/// resolve an out-of-process iframe child target the same way `observe`
/// enumerated it, instead of failing once the element id names a child target.
#[cfg(windows)]
pub async fn enable_flat_iframe_auto_attach(page: &chromiumoxide::Page) -> Result<(), String> {
    use chromiumoxide::cdp::browser_protocol::target::{
        FilterEntry, SetAutoAttachParams, TargetFilter,
    };

    let filter = TargetFilter::new(vec![
        FilterEntry::builder()
            .r#type("iframe")
            .exclude(false)
            .build(),
    ]);
    let params = SetAutoAttachParams::builder()
        .auto_attach(true)
        .wait_for_debugger_on_start(false)
        .flatten(true)
        .filter(filter)
        .build()
        .map_err(|error| format!("Target.setAutoAttach iframe params: {error}"))?;
    page.execute(params)
        .await
        .map_err(|error| format!("Target.setAutoAttach iframe flatten=true failed: {error}"))?;
    Ok(())
}

#[cfg(windows)]
async fn fetch_target_dom_read(
    page: &chromiumoxide::Page,
    target_id: &str,
    max_nodes: usize,
) -> crate::A11yResult<CdpTargetDomRead> {
    use std::collections::{HashMap, HashSet};

    use chromiumoxide::cdp::browser_protocol::accessibility::{EnableParams, GetFullAxTreeParams};
    use chromiumoxide::cdp::browser_protocol::dom::{
        BackendNodeId, GetBoxModelParams, GetDocumentParams,
    };

    use crate::A11yError;

    page.execute(EnableParams::default())
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("target {target_id} Accessibility.enable: {err}"),
        })?;

    let mut frame_snapshot_errors = Vec::new();
    let prime = GetDocumentParams::builder().depth(-1).pierce(true).build();
    if let Err(error) = page.execute(prime).await {
        frame_snapshot_errors.push(format!(
            "target {target_id} DOM.getDocument depth=-1 pierce=true failed: {error}"
        ));
    }

    let frames = frame_descriptors_for_page(page, target_id, &mut frame_snapshot_errors).await;
    let mut dom_nodes = Vec::new();
    let mut total_ax_nodes = 0_u32;
    let mut first_frame_error = None;
    let mut seen_backends = HashSet::new();

    for frame in &frames {
        let params = frame
            .frame_id
            .clone()
            .map_or_else(GetFullAxTreeParams::default, |frame_id| {
                GetFullAxTreeParams::builder().frame_id(frame_id).build()
            });
        let tree = match page.execute(params).await {
            Ok(tree) => tree,
            Err(error) => {
                let detail = format!(
                    "target {target_id} frame {} Accessibility.getFullAXTree failed: {error}",
                    frame.frame_id_wire.as_deref().unwrap_or("<root>")
                );
                if first_frame_error.is_none() {
                    first_frame_error = Some(detail.clone());
                }
                frame_snapshot_errors.push(detail);
                continue;
            }
        };

        let ax_nodes = &tree.result.nodes;
        let by_ax_id: HashMap<&str, &_> = ax_nodes
            .iter()
            .map(|node| (node.node_id.inner().as_str(), node))
            .collect();

        for node in ax_nodes {
            if node.ignored {
                continue;
            }
            total_ax_nodes = total_ax_nodes.saturating_add(1);
            let Some(backend) = node.backend_dom_node_id.as_ref().map(|id| *id.inner()) else {
                continue;
            };
            if !seen_backends.insert(backend) {
                continue;
            }
            let role = ax_value_string(node.role.as_ref());
            if role.is_empty() {
                continue;
            }
            let name = ax_value_string(node.name.as_ref());
            let value = {
                let value = ax_value_string(node.value.as_ref());
                (!value.is_empty()).then_some(value)
            };
            let parent_backend = nearest_backend_ancestor(node, &by_ax_id);
            let child_count = node
                .child_ids
                .as_ref()
                .map_or(0, |ids| u32::try_from(ids.len()).unwrap_or(u32::MAX));

            let bbox = if dom_nodes.len() < max_nodes {
                let params = GetBoxModelParams::builder()
                    .backend_node_id(BackendNodeId::new(backend))
                    .build();
                page.execute(params)
                    .await
                    .ok()
                    .and_then(|response| rect_from_quad(response.result.model.content.inner()))
            } else {
                None
            };
            let frame_id = frame.frame_id_wire.clone();

            dom_nodes.push(CdpDomNode {
                backend_node_id: backend,
                parent_backend_node_id: parent_backend,
                role,
                name,
                value,
                frame_id,
                bbox,
                child_count,
                enabled: true,
                focused: false,
            });
        }
    }

    if dom_nodes.is_empty()
        && let Some(detail) = first_frame_error
    {
        return Err(A11yError::CdpAxtreeFailed { detail });
    }

    Ok(CdpTargetDomRead {
        dom_nodes,
        total_ax_nodes,
        frame_tree_frame_count: u32::try_from(frames.len()).unwrap_or(u32::MAX),
        frame_ids: frames
            .iter()
            .filter_map(|frame| frame.frame_id_wire.clone())
            .collect(),
        frame_snapshot_errors,
    })
}

#[cfg(windows)]
async fn frame_descriptors_for_page(
    page: &chromiumoxide::Page,
    target_id: &str,
    frame_snapshot_errors: &mut Vec<String>,
) -> Vec<CdpFrameDescriptor> {
    use chromiumoxide::cdp::browser_protocol::page::GetFrameTreeParams;

    match page.execute(GetFrameTreeParams::default()).await {
        Ok(response) => {
            let mut frames = Vec::new();
            collect_frame_descriptors(&response.result.frame_tree, &mut frames);
            if frames.is_empty() {
                frame_snapshot_errors.push(format!(
                    "target {target_id} Page.getFrameTree returned zero frames; falling back to root AX tree"
                ));
                vec![CdpFrameDescriptor {
                    frame_id: None,
                    frame_id_wire: None,
                }]
            } else {
                frames
            }
        }
        Err(error) => {
            frame_snapshot_errors.push(format!(
                "target {target_id} Page.getFrameTree failed: {error}; falling back to root AX tree"
            ));
            vec![CdpFrameDescriptor {
                frame_id: None,
                frame_id_wire: None,
            }]
        }
    }
}

#[cfg(windows)]
fn collect_frame_descriptors(
    frame_tree: &chromiumoxide::cdp::browser_protocol::page::FrameTree,
    out: &mut Vec<CdpFrameDescriptor>,
) {
    out.push(CdpFrameDescriptor {
        frame_id: Some(frame_tree.frame.id.clone()),
        frame_id_wire: Some(frame_tree.frame.id.inner().clone()),
    });
    if let Some(children) = frame_tree.child_frames.as_ref() {
        for child in children {
            collect_frame_descriptors(child, out);
        }
    }
}

#[cfg(windows)]
fn related_iframe_targets<'a>(
    target_infos: &'a [chromiumoxide::cdp::browser_protocol::target::TargetInfo],
    selected_frame_ids: &[String],
) -> Vec<&'a chromiumoxide::cdp::browser_protocol::target::TargetInfo> {
    target_infos
        .iter()
        .filter(|target| target.r#type == "iframe")
        .filter(|target| {
            target
                .parent_frame_id
                .as_ref()
                .is_some_and(|frame_id| selected_frame_ids.iter().any(|id| id == frame_id.inner()))
        })
        .collect()
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct CdpFrameOwnerElement {
    cdp_target_id: String,
    backend_node_id: i64,
    element_id: String,
}

#[cfg(windows)]
#[allow(
    clippy::too_many_arguments,
    reason = "CDP frame metadata is shaped for one wire entry"
)]
async fn frame_entries_for_page(
    page: &chromiumoxide::Page,
    hwnd: i64,
    target_id: &str,
    target_type: &str,
    target_attached: Option<bool>,
    is_oopif: bool,
    parent_frame_override: Option<&str>,
    depth_offset: u32,
    owner_elements: &mut std::collections::HashMap<String, CdpFrameOwnerElement>,
    frame_snapshot_errors: &mut Vec<String>,
) -> crate::A11yResult<Vec<CdpFrameTreeEntry>> {
    use chromiumoxide::cdp::browser_protocol::page::GetFrameTreeParams;

    let page_owner_elements =
        frame_owner_elements_for_page(page, hwnd, target_id, frame_snapshot_errors).await;
    owner_elements.extend(page_owner_elements.clone());

    let tree = page
        .execute(GetFrameTreeParams::default())
        .await
        .map_err(|error| crate::A11yError::CdpAxtreeFailed {
            detail: format!("target {target_id} Page.getFrameTree failed: {error}"),
        })?;
    let mut frames = Vec::new();
    collect_frame_list_entries(
        &tree.result.frame_tree,
        target_id,
        target_type,
        target_attached,
        is_oopif,
        parent_frame_override,
        depth_offset,
        0,
        0,
        &page_owner_elements,
        &mut frames,
    );
    if frames.is_empty() {
        return Err(crate::A11yError::CdpAxtreeFailed {
            detail: format!("target {target_id} Page.getFrameTree returned zero frames"),
        });
    }
    Ok(frames)
}

#[cfg(windows)]
async fn frame_owner_elements_for_page(
    page: &chromiumoxide::Page,
    hwnd: i64,
    target_id: &str,
    frame_snapshot_errors: &mut Vec<String>,
) -> std::collections::HashMap<String, CdpFrameOwnerElement> {
    use chromiumoxide::cdp::browser_protocol::dom::GetDocumentParams;

    let mut owner_elements = std::collections::HashMap::new();
    let params = GetDocumentParams::builder().depth(-1).pierce(true).build();
    match page.execute(params).await {
        Ok(document) => collect_frame_owner_elements(
            &document.result.root,
            hwnd,
            target_id,
            &mut owner_elements,
        ),
        Err(error) => frame_snapshot_errors.push(format!(
            "target {target_id} DOM.getDocument depth=-1 pierce=true failed while resolving frame owner elements: {error}"
        )),
    }
    owner_elements
}

#[cfg(windows)]
fn collect_frame_owner_elements(
    node: &chromiumoxide::cdp::browser_protocol::dom::Node,
    hwnd: i64,
    target_id: &str,
    out: &mut std::collections::HashMap<String, CdpFrameOwnerElement>,
) {
    if let Some(frame_id) = node.frame_id.as_ref() {
        let backend_node_id = *node.backend_node_id.inner();
        out.insert(
            frame_id.inner().clone(),
            CdpFrameOwnerElement {
                cdp_target_id: target_id.to_owned(),
                backend_node_id,
                element_id: cdp_element_id_for_target(hwnd, target_id, backend_node_id).to_string(),
            },
        );
    }
    if let Some(children) = node.children.as_ref() {
        for child in children {
            collect_frame_owner_elements(child, hwnd, target_id, out);
        }
    }
    if let Some(content_document) = node.content_document.as_ref() {
        collect_frame_owner_elements(content_document, hwnd, target_id, out);
    }
    if let Some(shadow_roots) = node.shadow_roots.as_ref() {
        for shadow_root in shadow_roots {
            collect_frame_owner_elements(shadow_root, hwnd, target_id, out);
        }
    }
    if let Some(template_content) = node.template_content.as_ref() {
        collect_frame_owner_elements(template_content, hwnd, target_id, out);
    }
    if let Some(pseudo_elements) = node.pseudo_elements.as_ref() {
        for pseudo in pseudo_elements {
            collect_frame_owner_elements(pseudo, hwnd, target_id, out);
        }
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments, reason = "recursive CDP tree flattening")]
fn collect_frame_list_entries(
    frame_tree: &chromiumoxide::cdp::browser_protocol::page::FrameTree,
    target_id: &str,
    target_type: &str,
    target_attached: Option<bool>,
    is_oopif: bool,
    parent_frame_override: Option<&str>,
    depth_offset: u32,
    local_depth: u32,
    sibling_index: u32,
    owner_elements: &std::collections::HashMap<String, CdpFrameOwnerElement>,
    out: &mut Vec<CdpFrameTreeEntry>,
) {
    let frame = &frame_tree.frame;
    let frame_id = frame.id.inner().clone();
    let parent_frame_id = frame
        .parent_id
        .as_ref()
        .map(|id| id.inner().clone())
        .or_else(|| {
            (local_depth == 0)
                .then(|| parent_frame_override.map(str::to_owned))
                .flatten()
        });
    let children = frame_tree.child_frames.as_deref().unwrap_or(&[]);
    let owner = owner_elements.get(&frame_id);
    let url = frame_url(frame);
    let security_origin =
        (!frame.security_origin.is_empty()).then(|| frame.security_origin.clone());
    out.push(CdpFrameTreeEntry {
        frame_id: frame_id.clone(),
        parent_frame_id,
        cdp_target_id: target_id.to_owned(),
        target_type: target_type.to_owned(),
        target_attached,
        url: url.clone(),
        name: frame.name.as_ref().filter(|name| !name.is_empty()).cloned(),
        origin: security_origin
            .clone()
            .filter(|origin| !origin.is_empty() && origin != "://")
            .unwrap_or_else(|| origin_from_url(&url)),
        security_origin,
        loader_id: Some(frame.loader_id.inner().clone()),
        depth: depth_offset.saturating_add(local_depth),
        sibling_index,
        child_count: u32::try_from(children.len()).unwrap_or(u32::MAX),
        is_out_of_process: is_oopif,
        frame_element_id: owner.map(|owner| owner.element_id.clone()),
        frame_element_backend_node_id: owner.map(|owner| owner.backend_node_id),
        frame_element_cdp_target_id: owner.map(|owner| owner.cdp_target_id.clone()),
        frame_element_source: frame_element_source(local_depth, parent_frame_override, owner),
    });
    for (index, child) in children.iter().enumerate() {
        collect_frame_list_entries(
            child,
            target_id,
            target_type,
            target_attached,
            is_oopif,
            None,
            depth_offset,
            local_depth.saturating_add(1),
            u32::try_from(index).unwrap_or(u32::MAX),
            owner_elements,
            out,
        );
    }
}

#[cfg(windows)]
fn frame_url(frame: &chromiumoxide::cdp::browser_protocol::page::Frame) -> String {
    let mut url = frame.url.clone();
    if let Some(fragment) = frame.url_fragment.as_ref()
        && !fragment.is_empty()
        && !url.ends_with(fragment)
    {
        url.push_str(fragment);
    }
    url
}

#[cfg(windows)]
fn frame_element_source(
    local_depth: u32,
    parent_frame_override: Option<&str>,
    owner: Option<&CdpFrameOwnerElement>,
) -> String {
    if owner.is_some() {
        "DOM.Node.frameId".to_owned()
    } else if local_depth == 0 && parent_frame_override.is_none() {
        "main_frame".to_owned()
    } else {
        "unavailable".to_owned()
    }
}

#[cfg(windows)]
fn apply_frame_owner(
    frame: &mut CdpFrameTreeEntry,
    owner_elements: &std::collections::HashMap<String, CdpFrameOwnerElement>,
) {
    if frame.frame_element_id.is_some() {
        return;
    }
    if let Some(owner) = owner_elements.get(&frame.frame_id) {
        frame.frame_element_id = Some(owner.element_id.clone());
        frame.frame_element_backend_node_id = Some(owner.backend_node_id);
        frame.frame_element_cdp_target_id = Some(owner.cdp_target_id.clone());
        frame.frame_element_source = "DOM.Node.frameId".to_owned();
    }
}

#[cfg(windows)]
fn merge_frame_entries(frames: &mut Vec<CdpFrameTreeEntry>, incoming: Vec<CdpFrameTreeEntry>) {
    for entry in incoming {
        if let Some(existing) = frames
            .iter_mut()
            .find(|frame| frame.frame_id == entry.frame_id)
        {
            existing.cdp_target_id = entry.cdp_target_id;
            existing.target_type = entry.target_type;
            existing.target_attached = entry.target_attached.or(existing.target_attached);
            existing.is_out_of_process |= entry.is_out_of_process;
            if existing.parent_frame_id.is_none() {
                existing.parent_frame_id = entry.parent_frame_id;
            }
            if existing.frame_element_id.is_none() {
                existing.frame_element_id = entry.frame_element_id;
                existing.frame_element_backend_node_id = entry.frame_element_backend_node_id;
                existing.frame_element_cdp_target_id = entry.frame_element_cdp_target_id;
                existing.frame_element_source = entry.frame_element_source;
            }
            if existing.security_origin.is_none() {
                existing.security_origin = entry.security_origin;
            }
            if existing.origin.is_empty() {
                existing.origin = entry.origin;
            }
            if existing.loader_id.is_none() {
                existing.loader_id = entry.loader_id;
            }
        } else {
            frames.push(entry);
        }
    }
}

#[cfg(windows)]
fn origin_from_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some(scheme_end) = trimmed.find("://") else {
        return String::new();
    };
    let scheme = &trimmed[..scheme_end];
    if scheme.is_empty() {
        return String::new();
    }
    let rest = &trimmed[scheme_end + 3..];
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    if authority.is_empty() {
        return String::new();
    }
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

#[cfg(windows)]
async fn wait_for_page_target(
    browser: &chromiumoxide::Browser,
    target_id: chromiumoxide::cdp::browser_protocol::target::TargetId,
) -> Result<chromiumoxide::Page, String> {
    let mut last_error = None;
    for _ in 0..10 {
        match browser.get_page(target_id.clone()).await {
            Ok(page) => return Ok(page),
            Err(error) => {
                last_error = Some(error.to_string());
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "target was not present".to_owned()))
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct ExistingPageCandidate {
    page: chromiumoxide::Page,
    target_id: String,
    session_id: String,
    title: String,
    url: String,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct ExistingPageSelection {
    page: chromiumoxide::Page,
    target_id: String,
    session_id: String,
    target_url: String,
    target_candidate_count: u32,
    selection_reason: String,
}

#[cfg(windows)]
async fn select_existing_page(
    pages: Vec<chromiumoxide::Page>,
    target_infos: &[chromiumoxide::cdp::browser_protocol::target::TargetInfo],
    foreground_title: &str,
    foreground_url_hint: Option<&str>,
    target_id_hint: Option<&str>,
) -> crate::A11yResult<ExistingPageSelection> {
    use std::collections::HashMap;

    use crate::A11yError;

    let page_targets: HashMap<&str, _> = target_infos
        .iter()
        .filter(|info| info.r#type == "page")
        .map(|info| (info.target_id.as_ref(), info))
        .collect();
    let mut candidates = Vec::new();
    for page in pages {
        let target_id = page.target_id().inner().clone();
        let info = page_targets.get(target_id.as_str()).copied();
        let title = if let Some(info) = info
            && !info.title.is_empty()
        {
            info.title.clone()
        } else {
            page.get_title().await.ok().flatten().unwrap_or_default()
        };
        let url = if let Some(info) = info
            && !info.url.is_empty()
        {
            info.url.clone()
        } else {
            page.url().await.ok().flatten().unwrap_or_default()
        };
        let session_id = page.session_id().inner().clone();
        candidates.push(ExistingPageCandidate {
            page,
            target_id,
            session_id,
            title,
            url,
        });
    }

    if candidates.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "Target.getTargets/page discovery found no existing page targets".to_owned(),
        });
    }

    let target_candidate_count = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
    if let Some(target_id_hint) = target_id_hint.filter(|hint| !hint.trim().is_empty()) {
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.target_id == target_id_hint)
        {
            return Ok(selection(
                candidate,
                target_candidate_count,
                "target_id_hint",
            ));
        }
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "explicit CDP target_id hint {target_id_hint:?} was not discovered among page targets: {}",
                candidates
                    .iter()
                    .map(|candidate| candidate.target_id.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        });
    }

    if let Some(url_hint) = foreground_url_hint.filter(|hint| !hint.trim().is_empty()) {
        let url_matches: Vec<_> = candidates
            .iter()
            .filter(|candidate| url_matches_hint(&candidate.url, url_hint))
            .collect();
        match url_matches.as_slice() {
            [candidate] => {
                return Ok(selection(candidate, target_candidate_count, "url_hint"));
            }
            [] => {}
            matches => {
                return Err(A11yError::CdpAxtreeFailed {
                    detail: format!(
                        "ambiguous CDP target selection for foreground URL hint {url_hint:?}: {} matching page targets",
                        matches.len()
                    ),
                });
            }
        }
    }

    let title_matches: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            !candidate.title.is_empty() && foreground_title.contains(candidate.title.as_str())
        })
        .collect();
    match title_matches.as_slice() {
        [candidate] => Ok(selection(
            candidate,
            target_candidate_count,
            "foreground_title",
        )),
        [] => Ok(selection(
            &candidates[0],
            target_candidate_count,
            "fallback_first_page",
        )),
        matches => Err(A11yError::CdpAxtreeFailed {
            detail: format!(
                "ambiguous CDP target selection for foreground title {foreground_title:?}: {} matching page targets",
                matches.len()
            ),
        }),
    }
}

#[cfg(windows)]
fn selection(
    candidate: &ExistingPageCandidate,
    target_candidate_count: u32,
    selection_reason: &str,
) -> ExistingPageSelection {
    ExistingPageSelection {
        page: candidate.page.clone(),
        target_id: candidate.target_id.clone(),
        session_id: candidate.session_id.clone(),
        target_url: candidate.url.clone(),
        target_candidate_count,
        selection_reason: selection_reason.to_owned(),
    }
}

#[cfg(windows)]
fn url_matches_hint(candidate_url: &str, hint: &str) -> bool {
    let candidate = candidate_url.trim();
    let hint = hint.trim();
    if candidate.eq_ignore_ascii_case(hint) {
        return true;
    }
    trim_trailing_url_slash(candidate).eq_ignore_ascii_case(trim_trailing_url_slash(hint))
}

#[cfg(windows)]
fn trim_trailing_url_slash(value: &str) -> &str {
    value.strip_suffix('/').unwrap_or(value)
}

#[cfg(windows)]
async fn wait_for_pages(
    browser: &chromiumoxide::Browser,
) -> crate::A11yResult<Vec<chromiumoxide::Page>> {
    use crate::A11yError;

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

/// Extracts the string value of a CDP `AxValue` (role/name/value), empty if none.
#[cfg(windows)]
fn ax_value_string(
    value: Option<&chromiumoxide::cdp::browser_protocol::accessibility::AxValue>,
) -> String {
    value
        .and_then(|value| value.value.as_ref())
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Walks an AX node's `parentId` chain to the first ancestor that has a backend
/// DOM node id, so the mapped tree stays connected even across generic AX nodes.
#[cfg(windows)]
fn nearest_backend_ancestor(
    node: &chromiumoxide::cdp::browser_protocol::accessibility::AxNode,
    by_ax_id: &std::collections::HashMap<
        &str,
        &chromiumoxide::cdp::browser_protocol::accessibility::AxNode,
    >,
) -> Option<i64> {
    let mut current = node.parent_id.as_ref()?.inner().as_str();
    for _ in 0..256 {
        let parent = by_ax_id.get(current)?;
        if let Some(backend) = parent.backend_dom_node_id.as_ref() {
            return Some(*backend.inner());
        }
        current = parent.parent_id.as_ref()?.inner().as_str();
    }
    None
}
