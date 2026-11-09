//! CDP element actionability predicates (#1122).
//!
//! This mirrors the Playwright-style preflight checks Synapse needs before DOM
//! actions: live attachment, visibility, box stability, enabled/editable state,
//! and whether the action point receives pointer events.

#![cfg(windows)]

use std::time::Duration;

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, GetBoxModelParams, GetDocumentParams, GetNodeForLocationParams,
    ResolveNodeParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, RemoteObjectId,
};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::{A11yError, A11yResult};

const BOX_STABILITY_EPSILON_CSS_PX: f64 = 0.25;

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "CDP integer CSS-pixel fields require rounding a prevalidated finite f64"
)]
fn rounded_finite_css_px_to_i64(field: &str, value: f64) -> A11yResult<i64> {
    if !value.is_finite() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("{field} CSS pixel value was not finite: {value}"),
        });
    }
    let rounded = value.round();
    if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("{field} CSS pixel value {value} is outside the i64 CDP range"),
        });
    }
    Ok(rounded as i64)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActionabilityPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActionabilityRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl CdpActionabilityRect {
    #[must_use]
    pub fn center(&self) -> CdpActionabilityPoint {
        CdpActionabilityPoint {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }

    #[must_use]
    pub fn has_area(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActionabilityBoxModel {
    pub content: CdpActionabilityRect,
    pub border: CdpActionabilityRect,
    pub width: i64,
    pub height: i64,
}

impl CdpActionabilityBoxModel {
    #[must_use]
    pub fn action_point(&self) -> CdpActionabilityPoint {
        self.content.center()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActionabilityDomState {
    pub attached: bool,
    pub tag_name: String,
    pub id: String,
    pub class_name: String,
    pub name_attr: String,
    pub type_attr: String,
    pub display: String,
    pub visibility: String,
    pub pointer_events: String,
    pub disabled: bool,
    pub aria_disabled: bool,
    pub readonly: bool,
    pub enabled: bool,
    pub editable: bool,
    pub viewport_rect: CdpActionabilityRect,
    pub node_description: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CdpActionabilityFailure {
    pub predicate: String,
    pub reason: String,
    pub detail: String,
}

impl CdpActionabilityFailure {
    fn new(predicate: &str, reason: &str, detail: impl Into<String>) -> Self {
        Self {
            predicate: predicate.to_owned(),
            reason: reason.to_owned(),
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActionabilityHitTest {
    pub point: Option<CdpActionabilityPoint>,
    pub receives_events: bool,
    pub top_backend_node_id: Option<i64>,
    pub top_frame_id: Option<String>,
    pub target_or_descendant_from_point: bool,
    pub top_node_description: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CdpActionabilityResult {
    pub target_id: String,
    pub backend_node_id: i64,
    pub attached: bool,
    pub visible: bool,
    pub stable: bool,
    pub enabled: bool,
    pub editable: bool,
    pub receives_events: bool,
    /// Click/tap readiness: attached + visible + stable + enabled + receives-events.
    pub action_ready: bool,
    /// Fill/type readiness: action-ready + editable.
    pub editable_action_ready: bool,
    pub failure_reasons: Vec<CdpActionabilityFailure>,
    pub box_model: Option<CdpActionabilityBoxModel>,
    pub second_box_model: Option<CdpActionabilityBoxModel>,
    pub box_model_error: Option<String>,
    pub second_box_model_error: Option<String>,
    pub dom_state: CdpActionabilityDomState,
    pub hit_test: CdpActionabilityHitTest,
}

#[derive(Clone, Debug, Deserialize)]
struct HitElementState {
    found: bool,
    target_or_descendant: bool,
    node_description: String,
}

/// Computes live Playwright-style actionability predicates for a DOM backend
/// node in an owned CDP page target.
///
/// # Errors
///
/// Returns `A11Y_CDP_ATTACH_FAILED` if `endpoint`/`target_id` cannot be reached,
/// and `A11Y_CDP_AXTREE_FAILED` when the node cannot be resolved or a required
/// protocol read fails unexpectedly. Non-actionable element states are returned
/// as structured predicate failures instead of errors.
pub async fn cdp_actionability(
    endpoint: &str,
    target_id: &str,
    backend_node_id: i64,
) -> A11yResult<CdpActionabilityResult> {
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
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        prime_target_page_for_actionability(&page, target_id).await?;
        cdp_actionability_on_page(&page, target_id, backend_node_id).await
    }
    .await;

    handler_task.abort();
    result
}

async fn cdp_actionability_on_page(
    page: &chromiumoxide::Page,
    target_id: &str,
    backend_node_id: i64,
) -> A11yResult<CdpActionabilityResult> {
    let object_id = resolve_node_object_id(page, backend_node_id).await?;
    let dom_state = read_dom_state(page, &object_id, backend_node_id).await?;

    let (box_model, box_model_error) = box_model_snapshot(page, backend_node_id).await;
    wait_for_two_animation_frames(page).await;
    let (second_box_model, second_box_model_error) =
        box_model_snapshot(page, backend_node_id).await;

    let visible = visible_from(&dom_state, box_model.as_ref());
    let stable = stable_from(box_model.as_ref(), second_box_model.as_ref());
    let action_point = box_model
        .as_ref()
        .map(CdpActionabilityBoxModel::action_point);
    let hit_test = hit_test_action_point(page, &object_id, backend_node_id, action_point).await?;

    let mut failure_reasons = actionability_failures(
        &dom_state,
        visible,
        stable,
        box_model.as_ref(),
        box_model_error.as_deref(),
        &hit_test,
    );
    if box_model.is_some() && second_box_model.is_none() {
        failure_reasons.push(CdpActionabilityFailure::new(
            "stable",
            "second_box_model_unavailable",
            second_box_model_error
                .as_deref()
                .unwrap_or("second DOM.getBoxModel returned no box model"),
        ));
    }

    let receives_events = hit_test.receives_events;
    let action_ready =
        dom_state.attached && visible && stable && dom_state.enabled && receives_events;
    let editable_action_ready = action_ready && dom_state.editable;

    Ok(CdpActionabilityResult {
        target_id: target_id.to_owned(),
        backend_node_id,
        attached: dom_state.attached,
        visible,
        stable,
        enabled: dom_state.enabled,
        editable: dom_state.editable,
        receives_events,
        action_ready,
        editable_action_ready,
        failure_reasons,
        box_model,
        second_box_model,
        box_model_error,
        second_box_model_error,
        dom_state,
        hit_test,
    })
}

async fn prime_target_page_for_actionability(
    page: &chromiumoxide::Page,
    target_id: &str,
) -> A11yResult<()> {
    page.execute(GetDocumentParams::builder().depth(0).pierce(true).build())
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("DOM.getDocument before actionability for target {target_id}: {err}"),
        })?;
    Ok(())
}

async fn resolve_node_object_id(
    page: &chromiumoxide::Page,
    backend_node_id: i64,
) -> A11yResult<RemoteObjectId> {
    let resolve = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .object_group("synapse_actionability")
        .build();
    let resolved = page
        .execute(resolve)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("resolveNode for backendNodeId {backend_node_id}: {err}"),
        })?;
    resolved
        .object
        .object_id
        .clone()
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: format!("resolveNode for backendNodeId {backend_node_id} returned no objectId"),
        })
}

async fn read_dom_state(
    page: &chromiumoxide::Page,
    object_id: &RemoteObjectId,
    backend_node_id: i64,
) -> A11yResult<CdpActionabilityDomState> {
    let call = CallFunctionOnParams::builder()
        .function_declaration(ACTIONABILITY_DOM_STATE_JS)
        .object_id(object_id.clone())
        .object_group("synapse_actionability")
        .return_by_value(true)
        .silent(true)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Runtime.callFunctionOn actionability state params: {err}"),
        })?;
    let returns = page
        .execute(call)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn actionability state: {err}"),
        })?
        .result;
    if let Some(exception) = returns.exception_details.as_ref() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn actionability state threw: {exception:?}"),
        });
    }
    let value = returns.result.value.ok_or_else(|| A11yError::CdpAxtreeFailed {
        detail: format!(
            "Runtime.callFunctionOn actionability state returned no by-value payload for backendNodeId {backend_node_id}"
        ),
    })?;
    serde_json::from_value(value).map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("Runtime.callFunctionOn actionability state decode: {err}"),
    })
}

async fn box_model_snapshot(
    page: &chromiumoxide::Page,
    backend_node_id: i64,
) -> (Option<CdpActionabilityBoxModel>, Option<String>) {
    let params = GetBoxModelParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build();
    match page.execute(params).await {
        Ok(model) => {
            let result = model.result.model;
            let Some(content) = rect_from_quad(result.content.inner()) else {
                return (
                    None,
                    Some("DOM.getBoxModel returned malformed content quad".to_owned()),
                );
            };
            let Some(border) = rect_from_quad(result.border.inner()) else {
                return (
                    None,
                    Some("DOM.getBoxModel returned malformed border quad".to_owned()),
                );
            };
            (
                Some(CdpActionabilityBoxModel {
                    content,
                    border,
                    width: result.width,
                    height: result.height,
                }),
                None,
            )
        }
        Err(err) => (None, Some(format!("DOM.getBoxModel: {err}"))),
    }
}

fn rect_from_quad(quad: &[f64]) -> Option<CdpActionabilityRect> {
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
    Some(CdpActionabilityRect {
        x: min_x,
        y: min_y,
        width: (max_x - min_x).max(0.0),
        height: (max_y - min_y).max(0.0),
    })
}

async fn wait_for_two_animation_frames(page: &chromiumoxide::Page) {
    let Ok(params) = EvaluateParams::builder()
        .expression(
            r"new Promise(resolve => {
                const raf = window.requestAnimationFrame || (cb => setTimeout(cb, 16));
                raf(() => raf(() => resolve(true)));
            })",
        )
        .await_promise(true)
        .return_by_value(true)
        .build()
    else {
        tokio::time::sleep(Duration::from_millis(50)).await;
        return;
    };
    if tokio::time::timeout(Duration::from_millis(250), page.execute(params))
        .await
        .is_err()
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn hit_test_action_point(
    page: &chromiumoxide::Page,
    object_id: &RemoteObjectId,
    backend_node_id: i64,
    point: Option<CdpActionabilityPoint>,
) -> A11yResult<CdpActionabilityHitTest> {
    let Some(point) = point else {
        return Ok(CdpActionabilityHitTest {
            point: None,
            receives_events: false,
            top_backend_node_id: None,
            top_frame_id: None,
            target_or_descendant_from_point: false,
            top_node_description: None,
            error: Some(
                "no action point because DOM.getBoxModel returned no content box".to_owned(),
            ),
        });
    };
    if !(point.x.is_finite() && point.y.is_finite()) {
        return Ok(CdpActionabilityHitTest {
            point: Some(point),
            receives_events: false,
            top_backend_node_id: None,
            top_frame_id: None,
            target_or_descendant_from_point: false,
            top_node_description: None,
            error: Some("action point was not finite".to_owned()),
        });
    }

    let location = GetNodeForLocationParams::builder()
        .x(rounded_finite_css_px_to_i64("action point x", point.x)?)
        .y(rounded_finite_css_px_to_i64("action point y", point.y)?)
        .include_user_agent_shadow_dom(true)
        .ignore_pointer_events_none(false)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build DOM.getNodeForLocation params: {err}"),
        })?;
    let location_result = page.execute(location).await;
    let hit_state = read_element_from_point_state(page, object_id, &point).await?;

    match location_result {
        Ok(found) => {
            let top_backend_node_id = *found.result.backend_node_id.inner();
            let receives_events =
                top_backend_node_id == backend_node_id || hit_state.target_or_descendant;
            Ok(CdpActionabilityHitTest {
                point: Some(point),
                receives_events,
                top_backend_node_id: Some(top_backend_node_id),
                top_frame_id: Some(found.result.frame_id.inner().clone()),
                target_or_descendant_from_point: hit_state.target_or_descendant,
                top_node_description: hit_state.found.then_some(hit_state.node_description),
                error: None,
            })
        }
        Err(err) => Ok(CdpActionabilityHitTest {
            point: Some(point),
            receives_events: false,
            top_backend_node_id: None,
            top_frame_id: None,
            target_or_descendant_from_point: hit_state.target_or_descendant,
            top_node_description: hit_state.found.then_some(hit_state.node_description),
            error: Some(format!("DOM.getNodeForLocation: {err}")),
        }),
    }
}

async fn read_element_from_point_state(
    page: &chromiumoxide::Page,
    object_id: &RemoteObjectId,
    point: &CdpActionabilityPoint,
) -> A11yResult<HitElementState> {
    let call = CallFunctionOnParams::builder()
        .function_declaration(ACTIONABILITY_ELEMENT_FROM_POINT_JS)
        .object_id(object_id.clone())
        .argument(
            chromiumoxide::cdp::js_protocol::runtime::CallArgument::builder()
                .value(serde_json::json!(point.x))
                .build(),
        )
        .argument(
            chromiumoxide::cdp::js_protocol::runtime::CallArgument::builder()
                .value(serde_json::json!(point.y))
                .build(),
        )
        .object_group("synapse_actionability")
        .return_by_value(true)
        .silent(true)
        .build()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("build Runtime.callFunctionOn elementFromPoint params: {err}"),
        })?;
    let returns = page
        .execute(call)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn elementFromPoint: {err}"),
        })?
        .result;
    if let Some(exception) = returns.exception_details.as_ref() {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.callFunctionOn elementFromPoint threw: {exception:?}"),
        });
    }
    let value = returns
        .result
        .value
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "Runtime.callFunctionOn elementFromPoint returned no by-value payload"
                .to_owned(),
        })?;
    serde_json::from_value(value).map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("Runtime.callFunctionOn elementFromPoint decode: {err}"),
    })
}

#[must_use]
pub fn visible_from(
    dom_state: &CdpActionabilityDomState,
    box_model: Option<&CdpActionabilityBoxModel>,
) -> bool {
    dom_state.attached
        && css_visibility_allows_action(dom_state)
        && box_model.is_some_and(|model| model.content.has_area())
}

#[must_use]
pub fn stable_from(
    first: Option<&CdpActionabilityBoxModel>,
    second: Option<&CdpActionabilityBoxModel>,
) -> bool {
    first
        .zip(second)
        .is_some_and(|(first, second)| box_models_equal(first, second))
}

#[must_use]
pub fn box_models_equal(
    first: &CdpActionabilityBoxModel,
    second: &CdpActionabilityBoxModel,
) -> bool {
    rects_equal(&first.content, &second.content)
        && rects_equal(&first.border, &second.border)
        && first.width == second.width
        && first.height == second.height
}

fn rects_equal(first: &CdpActionabilityRect, second: &CdpActionabilityRect) -> bool {
    (first.x - second.x).abs() <= BOX_STABILITY_EPSILON_CSS_PX
        && (first.y - second.y).abs() <= BOX_STABILITY_EPSILON_CSS_PX
        && (first.width - second.width).abs() <= BOX_STABILITY_EPSILON_CSS_PX
        && (first.height - second.height).abs() <= BOX_STABILITY_EPSILON_CSS_PX
}

fn css_visibility_allows_action(dom_state: &CdpActionabilityDomState) -> bool {
    dom_state.display != "none"
        && dom_state.visibility != "hidden"
        && dom_state.visibility != "collapse"
}

fn actionability_failures(
    dom_state: &CdpActionabilityDomState,
    visible: bool,
    stable: bool,
    box_model: Option<&CdpActionabilityBoxModel>,
    box_model_error: Option<&str>,
    hit_test: &CdpActionabilityHitTest,
) -> Vec<CdpActionabilityFailure> {
    let mut failures = Vec::new();
    if !dom_state.attached {
        failures.push(CdpActionabilityFailure::new(
            "attached",
            "detached",
            "resolved DOM node is not connected to its document",
        ));
    }
    if !visible {
        if !dom_state.attached {
            failures.push(CdpActionabilityFailure::new(
                "visible",
                "detached",
                "detached nodes are not visible",
            ));
        } else if dom_state.display == "none" {
            failures.push(CdpActionabilityFailure::new(
                "visible",
                "display_none",
                "computed style display is none",
            ));
        } else if matches!(dom_state.visibility.as_str(), "hidden" | "collapse") {
            failures.push(CdpActionabilityFailure::new(
                "visible",
                "visibility_hidden",
                format!("computed style visibility is {}", dom_state.visibility),
            ));
        } else if let Some(error) = box_model_error {
            failures.push(CdpActionabilityFailure::new(
                "visible",
                "no_box_model",
                error,
            ));
        } else if box_model.is_none_or(|model| !model.content.has_area()) {
            failures.push(CdpActionabilityFailure::new(
                "visible",
                "zero_box",
                "DOM.getBoxModel content box has zero width or height",
            ));
        }
    }
    if !stable {
        failures.push(CdpActionabilityFailure::new(
            "stable",
            "box_changed_or_unavailable",
            "DOM.getBoxModel content/border boxes changed across the stability sample or were unavailable",
        ));
    }
    if !dom_state.enabled {
        failures.push(CdpActionabilityFailure::new(
            "enabled",
            "disabled",
            format!(
                "disabled={} aria_disabled={} readonly={}",
                dom_state.disabled, dom_state.aria_disabled, dom_state.readonly
            ),
        ));
    }
    if !dom_state.editable {
        failures.push(CdpActionabilityFailure::new(
            "editable",
            "not_editable",
            format!(
                "{} is not a writable input, textarea, contenteditable, or role=textbox",
                dom_state.node_description
            ),
        ));
    }
    if !hit_test.receives_events {
        failures.push(CdpActionabilityFailure::new(
            "receives_events",
            "obscured_or_not_hit",
            hit_test.top_node_description.as_ref().map_or_else(
                || {
                    hit_test
                        .error
                        .as_deref()
                        .unwrap_or("no hit-test node")
                        .to_owned()
                },
                |description| format!("top hit-test node is {description}"),
            ),
        ));
    }
    failures
}

const ACTIONABILITY_DOM_STATE_JS: &str = r#"function() {
    const el = this;
    const str = value => String(value == null ? '' : value);
    const attached = !!(el && el.isConnected);
    const doc = el && el.ownerDocument ? el.ownerDocument : document;
    const win = doc.defaultView || window;
    const cs = attached && el.nodeType === 1 ? win.getComputedStyle(el) : null;
    const rect = attached && el.getBoundingClientRect
        ? el.getBoundingClientRect()
        : { left: 0, top: 0, width: 0, height: 0 };
    const tag = str(el && el.tagName).toLowerCase();
    const tagU = tag.toUpperCase();
    const getAttr = name => (el && el.getAttribute) ? str(el.getAttribute(name)) : '';
    const typeAttr = (getAttr('type') || 'text').toLowerCase();
    const textTypes = new Set(['text','search','url','tel','email','password','number','date','datetime-local','month','time','week','color']);
    const ariaDisabled = getAttr('aria-disabled').toLowerCase() === 'true';
    const disabled = !!(el && ('disabled' in el) && el.disabled) || ariaDisabled;
    const readonly = !!(el && ('readOnly' in el) && el.readOnly);
    const enabled = !disabled;
    const editable = enabled && (
        tagU === 'TEXTAREA' ||
        (tagU === 'INPUT' && textTypes.has(typeAttr) && !readonly) ||
        !!(el && el.isContentEditable) ||
        getAttr('role').toLowerCase() === 'textbox'
    );
    return {
        attached,
        tag_name: tag,
        id: str(el && el.id),
        class_name: str(el && el.className),
        name_attr: getAttr('name'),
        type_attr: typeAttr,
        display: cs ? str(cs.display) : '',
        visibility: cs ? str(cs.visibility) : '',
        pointer_events: cs ? str(cs.pointerEvents) : '',
        disabled,
        aria_disabled: ariaDisabled,
        readonly,
        enabled,
        editable,
        viewport_rect: {
            x: Number(rect.left || 0),
            y: Number(rect.top || 0),
            width: Number(rect.width || 0),
            height: Number(rect.height || 0)
        },
        node_description: describe(el)
    };

    function describe(node) {
        if (!node || node.nodeType !== 1) { return 'none'; }
        const id = node.id ? `#${node.id}` : '';
        const cls = typeof node.className === 'string' && node.className
            ? `.${node.className.trim().replace(/\s+/g, '.')}` : '';
        const name = node.getAttribute && node.getAttribute('name')
            ? `[name="${node.getAttribute('name')}"]` : '';
        return `${String(node.tagName || '').toLowerCase()}${id}${cls}${name}`;
    }
}"#;

const ACTIONABILITY_ELEMENT_FROM_POINT_JS: &str = r#"function(x, y) {
    const doc = this && this.ownerDocument ? this.ownerDocument : document;
    const hit = doc.elementFromPoint(Number(x), Number(y));
    return {
        found: !!hit,
        target_or_descendant: !!(hit && (hit === this || this.contains(hit))),
        node_description: describe(hit)
    };

    function describe(node) {
        if (!node || node.nodeType !== 1) { return 'none'; }
        const id = node.id ? `#${node.id}` : '';
        const cls = typeof node.className === 'string' && node.className
            ? `.${node.className.trim().replace(/\s+/g, '.')}` : '';
        const name = node.getAttribute && node.getAttribute('name')
            ? `[name="${node.getAttribute('name')}"]` : '';
        return `${String(node.tagName || '').toLowerCase()}${id}${cls}${name}`;
    }
}"#;
