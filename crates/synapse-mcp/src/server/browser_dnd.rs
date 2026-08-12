//! Browser drag-and-drop tools (#1144/#1145) for Synapse-owned raw-CDP targets.

use std::time::{Duration, Instant};

use super::{ErrorData, Json, Parameters, SessionTarget, SynapseService, tool, tool_router};
use crate::m1::mcp_error;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{
    BrowserDefaultActionSemantics, InputDeliveryOrigin, InputProvenance, error_codes,
};

use super::input_provenance::{InputProvenanceContext, InputProvenanceSpec};

const BROWSER_DRAG_TOOL: &str = "browser_drag";
const BROWSER_DROP_TOOL: &str = "browser_drop";
const DEFAULT_MOUSE_STEPS: u32 = 12;
const DEFAULT_MOUSE_DURATION_MS: u64 = 350;
const MAX_SELECTOR_CHARS: usize = 4096;
const MAX_DRAG_DATA_CHARS: usize = 16_384;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDndMode {
    /// CDP Input.dispatchMouseEvent mouseMoved/mousePressed/dragMove/mouseReleased.
    Mouse,
    /// In-page DragEvent sequence with a synthetic (isTrusted=false) DataTransfer
    /// object. Drives JS DnD libraries that do not check trust (e.g. react-dnd's
    /// HTML5 backend); does NOT drive isTrusted-gating native drop zones.
    Html5,
    /// Chrome-generated (`isTrusted=true`) HTML5 drop via CDP
    /// `Input.dispatchDragEvent` (dragEnter/dragOver/drop) onto the target with a
    /// constructed DragData built from `data_mime_type`/`data_text` (or the
    /// source element's text). Drives native drop zones that gate on
    /// `event.isTrusted` and read `dataTransfer` (#1356).
    Html5Real,
}

impl BrowserDndMode {
    const fn delegated_method(self) -> &'static str {
        match self {
            Self::Mouse => "raw_cdp.Input.dispatchMouseEvent.drag",
            Self::Html5 => "raw_cdp.Runtime.evaluate.DragEvent",
            Self::Html5Real => "raw_cdp.Input.dispatchDragEvent",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDndParams {
    /// Strict CSS selector for the drag source. Exactly one visible/actionable
    /// top-frame element must match.
    pub source_selector: String,
    /// Strict CSS selector for the drop target. Exactly one visible/actionable
    /// top-frame element must match.
    pub target_selector: String,
    /// Drag mode. `browser_drag` defaults to mouse; `browser_drop` defaults to html5.
    #[serde(default)]
    pub mode: Option<BrowserDndMode>,
    /// Intermediate mouse move steps for mode=mouse. Defaults to 12, max 100.
    #[serde(default)]
    pub steps: Option<u32>,
    /// Total drag duration in milliseconds for mode=mouse. Defaults to 350.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// DataTransfer MIME type for mode=html5. Defaults to text/plain.
    #[serde(default)]
    pub data_mime_type: Option<String>,
    /// DataTransfer text payload for mode=html5. Defaults to source text in the page.
    #[serde(default)]
    pub data_text: Option<String>,
    /// Raw CDP target id. Defaults to this session's active CDP target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Required only with explicit cdp_target_id
    /// and no active session target.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Page/action readback wait budget in milliseconds. Defaults to 5000.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    /// Wait for source/target actionability before dispatch. Defaults true.
    #[serde(default)]
    pub auto_wait: Option<bool>,
    /// Per-element actionability wait budget. Defaults to 2000.
    #[serde(default)]
    pub auto_wait_timeout_ms: Option<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDndResponse {
    pub ok: bool,
    pub required_foreground: bool,
    pub transport: String,
    pub window_hwnd: i64,
    pub cdp_target_id: String,
    pub mode: BrowserDndMode,
    pub source_selector: String,
    pub target_selector: String,
    pub steps: u32,
    pub duration_ms: u64,
    pub delegated_tool: String,
    pub status: String,
    pub result: Value,
    pub input_provenance: InputProvenance,
}

#[derive(Clone, Debug)]
struct NormalizedBrowserDndParams {
    mode: BrowserDndMode,
    source_selector: String,
    target_selector: String,
    steps: u32,
    duration_ms: u64,
    data_mime_type: Option<String>,
    data_text: Option<String>,
    wait_timeout_ms: u64,
    auto_wait: bool,
    auto_wait_timeout_ms: u32,
}

#[tool_router(router = browser_dnd_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Drag one element to another in the calling session's owned raw-CDP tab on Synapse's dedicated non-default automation profile (#1144). Defaults to a target-scoped Input.dispatchMouseEvent sequence: mouseMoved to source, mousePressed, configurable dragMove steps, mouseReleased on target. Source and target are strict CSS selectors resolved and actionability-checked on the exact target. The debugger-free normal authenticated Chrome bridge fails closed before any Chrome command. Background-safe: no OS foreground input and no human foreground fallback."
    )]
    pub async fn browser_drag(
        &self,
        params: Parameters<BrowserDndParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserDndResponse>, ErrorData> {
        self.browser_dnd_tool(
            BROWSER_DRAG_TOOL,
            BrowserDndMode::Mouse,
            params.0,
            request_context,
        )
        .await
    }

    #[tool(
        description = "Dispatch an HTML5 drag-and-drop in the calling session's owned raw-CDP tab on Synapse's dedicated non-default automation profile (#1145). Defaults to an in-page DragEvent sequence with DataTransfer and exact dispatch readback; mode=html5_real uses Chrome-generated Input.dispatchDragEvent, and mode=mouse uses the same trusted pointer sequence as browser_drag. The debugger-free normal authenticated Chrome bridge fails closed before any Chrome command. Background-safe and target-scoped."
    )]
    pub async fn browser_drop(
        &self,
        params: Parameters<BrowserDndParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserDndResponse>, ErrorData> {
        self.browser_dnd_tool(
            BROWSER_DROP_TOOL,
            BrowserDndMode::Html5,
            params.0,
            request_context,
        )
        .await
    }

    async fn browser_dnd_tool(
        &self,
        tool: &'static str,
        default_mode: BrowserDndMode,
        params: BrowserDndParams,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserDndResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = tool,
            "tool.invocation kind={tool}"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{tool} requires an MCP session id (run the daemon in HTTP mode)"),
                )
            })?;
        let dnd = validate_browser_dnd_params(&params, default_mode)?;
        let (window_hwnd, cdp_target_id) = self.resolve_browser_dnd_target(&session_id, &params)?;
        if cdp_target_id.starts_with("chrome-tab:") {
            return Err(mcp_error(
                error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
                format!(
                    "{tool} refused normal authenticated Chrome target {cdp_target_id:?} before queueing any Chrome command; the normal profile permanently forbids debugger permission. Launch a session-owned raw-CDP browser on Synapse's dedicated non-default automation profile"
                ),
            ));
        }
        if synapse_a11y::endpoint_for_window(window_hwnd).is_none() {
            return Err(mcp_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "{tool} requires a reachable raw-CDP endpoint for window {window_hwnd:#x}; launch the browser with act_launch so Synapse owns a dedicated non-default profile"
                ),
            ));
        }

        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "mode": dnd.mode,
            "source_selector": &dnd.source_selector,
            "target_selector": &dnd.target_selector,
            "steps": dnd.steps,
            "duration_ms": dnd.duration_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(tool, &request_details, &session_id)?;
        let result = self
            .browser_dnd_run(tool, &session_id, window_hwnd, &cdp_target_id, &dnd)
            .await;
        self.audit_action_result_for_session(tool, &result, &session_id)?;
        result.map(Json)
    }

    fn resolve_browser_dnd_target(
        &self,
        session_id: &str,
        params: &BrowserDndParams,
    ) -> Result<(i64, String), ErrorData> {
        let target = self.action_session_target_override(
            params.window_hwnd,
            params.cdp_target_id.as_deref(),
            Some(session_id),
        )?;
        match target {
            Some(SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id,
            }) => Ok((window_hwnd, cdp_target_id)),
            Some(SessionTarget::Window { .. }) => Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                "browser_drag/browser_drop require a browser CDP tab target; bind a chrome-tab target with browser_tabs/select or set_target",
            )),
            None => Err(mcp_error(
                error_codes::TARGET_NOT_SET,
                "browser_drag/browser_drop require an active session browser tab target or explicit window_hwnd + cdp_target_id",
            )),
        }
    }

    async fn browser_dnd_run(
        &self,
        tool: &'static str,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        dnd: &NormalizedBrowserDndParams,
    ) -> Result<BrowserDndResponse, ErrorData> {
        let provenance_context =
            InputProvenanceContext::browser_tab(session_id, window_hwnd, cdp_target_id)?;
        let endpoint = synapse_a11y::endpoint_for_window(window_hwnd).ok_or_else(|| {
            mcp_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!("{tool} lost raw-CDP endpoint for window {window_hwnd:#x}"),
            )
        })?;
        let geometry = resolve_raw_dnd_geometry(&endpoint, cdp_target_id, dnd).await?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "browser_drag_drop_before_raw_cdp_input",
        )?;
        let result = match dnd.mode {
            BrowserDndMode::Mouse => {
                let point_count = usize::try_from(dnd.steps).unwrap_or(100).saturating_add(1);
                let denominator = f64::from(dnd.steps.max(1));
                let points = (0..point_count)
                    .map(|index| {
                        let ratio =
                            f64::from(u32::try_from(index).unwrap_or(dnd.steps)) / denominator;
                        synapse_a11y::CdpMouseStrokePoint {
                            x: (geometry.target_x - geometry.source_x)
                                .mul_add(ratio, geometry.source_x),
                            y: (geometry.target_y - geometry.source_y)
                                .mul_add(ratio, geometry.source_y),
                            elapsed_ms: Duration::from_millis(dnd.duration_ms).as_secs_f64()
                                * 1000.0
                                * ratio,
                        }
                    })
                    .collect();
                let stroke = synapse_a11y::cdp_mouse_stroke_target(
                    &endpoint,
                    cdp_target_id,
                    points,
                    Some(synapse_a11y::CdpMouseButton::Left),
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{tool} raw CDP mouse drag failed: {error}"),
                    )
                })?;
                json!({
                    "target_id": stroke.target_id,
                    "point_count": stroke.point_count,
                    "source": stroke.start,
                    "target": stroke.end,
                    "duration_ms": stroke.duration_ms,
                    "geometry": geometry,
                })
            }
            BrowserDndMode::Html5 => {
                dispatch_synthetic_html5_drag(&endpoint, cdp_target_id, dnd, &geometry).await?
            }
            BrowserDndMode::Html5Real => {
                let mime = dnd.data_mime_type.as_deref().unwrap_or("text/plain");
                let data = dnd.data_text.as_deref().unwrap_or(&geometry.source_text);
                let dispatched = synapse_a11y::cdp_html5_drag_target(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpActionPoint {
                        x: geometry.source_x,
                        y: geometry.source_y,
                    },
                    synapse_a11y::CdpActionPoint {
                        x: geometry.target_x,
                        y: geometry.target_y,
                    },
                    mime,
                    data,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{tool} raw CDP HTML5 drag failed: {error}"),
                    )
                })?;
                serde_json::to_value(dispatched).map_err(|error| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("{tool} could not serialize raw CDP drag readback: {error}"),
                    )
                })?
            }
        };
        super::operator_panic_boundary::ensure_mcp_mutation(
            "browser_drag_drop_after_raw_cdp_input",
        )?;
        super::input_provenance::reject_legacy_input_provenance_fragments(
            &result,
            "browser_drag_drop_legacy_input_provenance_fragment",
            Some(provenance_context.target()),
        )?;
        let (delivery_origin, expected_trust, default_actions, transport, method) = match dnd.mode {
            BrowserDndMode::Mouse => (
                InputDeliveryOrigin::CdpProtocol,
                Some(true),
                BrowserDefaultActionSemantics::UserAgentInput,
                "raw_cdp",
                "Input.dispatchMouseEvent(mouseMoved,mousePressed,dragMove,mouseReleased)",
            ),
            BrowserDndMode::Html5 => (
                InputDeliveryOrigin::DomDispatch,
                Some(false),
                BrowserDefaultActionSemantics::SyntheticDispatchNoUserAgentInputDefaults,
                "raw_cdp",
                "dispatchEvent(DragEvent sequence)",
            ),
            BrowserDndMode::Html5Real => (
                InputDeliveryOrigin::CdpProtocol,
                Some(true),
                BrowserDefaultActionSemantics::UserAgentInput,
                "raw_cdp",
                "Input.dispatchDragEvent(dragEnter,dragOver,drop)",
            ),
        };
        let input_provenance = provenance_context.finish(InputProvenanceSpec {
            delivery_origin,
            expected_dom_event_is_trusted: expected_trust,
            browser_default_actions: default_actions,
            backend: dnd.mode.delegated_method(),
            transport,
            protocol_method: Some(method),
            required_foreground: false,
            per_emission_fence_verified: false,
        })?;

        Ok(BrowserDndResponse {
            ok: true,
            required_foreground: false,
            transport: "raw_cdp".to_owned(),
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
            mode: dnd.mode,
            source_selector: dnd.source_selector.clone(),
            target_selector: dnd.target_selector.clone(),
            steps: dnd.steps,
            duration_ms: dnd.duration_ms,
            delegated_tool: dnd.mode.delegated_method().to_owned(),
            status: "dispatched_with_raw_cdp_readback".to_owned(),
            result,
            input_provenance,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RawDndGeometry {
    ok: bool,
    reason: String,
    source_count: usize,
    target_count: usize,
    source_x: f64,
    source_y: f64,
    target_x: f64,
    target_y: f64,
    source_text: String,
}

async fn resolve_raw_dnd_geometry(
    endpoint: &str,
    cdp_target_id: &str,
    dnd: &NormalizedBrowserDndParams,
) -> Result<RawDndGeometry, ErrorData> {
    let source = serde_json::to_string(&dnd.source_selector).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser drag source selector serialization failed: {error}"),
        )
    })?;
    let target = serde_json::to_string(&dnd.target_selector).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser drag target selector serialization failed: {error}"),
        )
    })?;
    let expression = format!(
        r#"(() => {{
          let sources, targets;
          try {{
            sources = Array.from(document.querySelectorAll({source}));
            targets = Array.from(document.querySelectorAll({target}));
          }} catch (error) {{
            return {{ok:false,reason:`invalid_selector:${{String(error && error.message || error)}}`,source_count:0,target_count:0,source_x:0,source_y:0,target_x:0,target_y:0,source_text:""}};
          }}
          if (sources.length !== 1 || targets.length !== 1) {{
            return {{ok:false,reason:"strict_match_count",source_count:sources.length,target_count:targets.length,source_x:0,source_y:0,target_x:0,target_y:0,source_text:""}};
          }}
          const inspect = (element) => {{
            const rect = element.getBoundingClientRect();
            const style = getComputedStyle(element);
            const actionable = element.isConnected && rect.width > 0 && rect.height > 0 &&
              style.display !== "none" && style.visibility !== "hidden" &&
              style.pointerEvents !== "none" && !element.disabled;
            return {{actionable,x:rect.left + rect.width / 2,y:rect.top + rect.height / 2}};
          }};
          const sourceState = inspect(sources[0]);
          const targetState = inspect(targets[0]);
          return {{
            ok: sourceState.actionable && targetState.actionable,
            reason: sourceState.actionable ? (targetState.actionable ? "ready" : "target_not_actionable") : "source_not_actionable",
            source_count: sources.length,
            target_count: targets.length,
            source_x: sourceState.x,
            source_y: sourceState.y,
            target_x: targetState.x,
            target_y: targetState.y,
            source_text: String(sources[0].innerText || sources[0].textContent || "")
          }};
        }})()"#
    );
    let started = Instant::now();
    let wait_budget = Duration::from_millis(u64::from(dnd.auto_wait_timeout_ms))
        .min(Duration::from_millis(dnd.wait_timeout_ms));
    loop {
        let evaluated = synapse_a11y::cdp_evaluate_expression_with_timeout(
            endpoint,
            cdp_target_id,
            &expression,
            false,
            true,
            dnd.wait_timeout_ms,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser drag/drop actionability readback failed: {error}"),
            )
        })?;
        let geometry: RawDndGeometry =
            serde_json::from_value(evaluated.value).map_err(|error| {
                mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!("browser drag/drop geometry readback was malformed: {error}"),
                )
            })?;
        if geometry.ok {
            return Ok(geometry);
        }
        if geometry.reason.starts_with("invalid_selector")
            || !dnd.auto_wait
            || started.elapsed() >= wait_budget
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser drag/drop source/target failed strict actionability: reason={} source_selector={:?} source_count={} target_selector={:?} target_count={} waited_ms={}",
                    geometry.reason,
                    dnd.source_selector,
                    geometry.source_count,
                    dnd.target_selector,
                    geometry.target_count,
                    started.elapsed().as_millis()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn dispatch_synthetic_html5_drag(
    endpoint: &str,
    cdp_target_id: &str,
    dnd: &NormalizedBrowserDndParams,
    geometry: &RawDndGeometry,
) -> Result<Value, ErrorData> {
    let source = serde_json::to_string(&dnd.source_selector)
        .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, error.to_string()))?;
    let target = serde_json::to_string(&dnd.target_selector)
        .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, error.to_string()))?;
    let mime = serde_json::to_string(dnd.data_mime_type.as_deref().unwrap_or("text/plain"))
        .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, error.to_string()))?;
    let data = serde_json::to_string(dnd.data_text.as_deref().unwrap_or(&geometry.source_text))
        .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, error.to_string()))?;
    let expression = format!(
        r#"(() => {{
          const sources = Array.from(document.querySelectorAll({source}));
          const targets = Array.from(document.querySelectorAll({target}));
          if (sources.length !== 1 || targets.length !== 1) {{
            throw new Error(`drag target drift: source_count=${{sources.length}} target_count=${{targets.length}}`);
          }}
          const source = sources[0];
          const target = targets[0];
          const transfer = new DataTransfer();
          transfer.setData({mime}, {data});
          const plan = [
            [source,"dragstart"], [target,"dragenter"], [target,"dragover"],
            [target,"drop"], [source,"dragend"]
          ];
          const events = plan.map(([node,type]) => {{
            const event = new DragEvent(type, {{bubbles:true,cancelable:true,dataTransfer:transfer}});
            const defaultAllowed = node.dispatchEvent(event);
            return {{type,default_allowed:defaultAllowed,default_prevented:event.defaultPrevented,is_trusted:event.isTrusted}};
          }});
          return {{
            target_id:{target_id},
            events,
            mime_type:{mime},
            data_length:{data_length},
            source_connected:source.isConnected,
            target_connected:target.isConnected,
            target_text:String(target.innerText || target.textContent || ""),
            target_class:String(target.className || "")
          }};
        }})()"#,
        target_id = serde_json::to_string(cdp_target_id)
            .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, error.to_string()))?,
        data_length = dnd
            .data_text
            .as_deref()
            .unwrap_or(&geometry.source_text)
            .len(),
    );
    let evaluated = synapse_a11y::cdp_evaluate_expression_with_timeout(
        endpoint,
        cdp_target_id,
        &expression,
        false,
        true,
        dnd.wait_timeout_ms,
    )
    .await
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!("browser_drop synthetic raw-CDP DragEvent dispatch failed: {error}"),
        )
    })?;
    Ok(evaluated.value)
}

fn validate_browser_dnd_params(
    params: &BrowserDndParams,
    default_mode: BrowserDndMode,
) -> Result<NormalizedBrowserDndParams, ErrorData> {
    let source_selector = validate_selector("source_selector", &params.source_selector)?;
    let target_selector = validate_selector("target_selector", &params.target_selector)?;
    let mode = params.mode.unwrap_or(default_mode);
    let steps = params.steps.unwrap_or(DEFAULT_MOUSE_STEPS);
    if !(1..=100).contains(&steps) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_drag/browser_drop steps must be in 1..=100, got {steps}"),
        ));
    }
    let duration_ms = params.duration_ms.unwrap_or(DEFAULT_MOUSE_DURATION_MS);
    if duration_ms > 10_000 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_drag/browser_drop duration_ms must be in 0..=10000, got {duration_ms}"
            ),
        ));
    }
    let wait_timeout_ms = params.wait_timeout_ms.unwrap_or(5000);
    if !(50..=30_000).contains(&wait_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_drag/browser_drop wait_timeout_ms must be in 50..=30000, got {wait_timeout_ms}"
            ),
        ));
    }
    let auto_wait_timeout_ms = params.auto_wait_timeout_ms.unwrap_or(2000);
    if !(50..=30_000).contains(&auto_wait_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_drag/browser_drop auto_wait_timeout_ms must be in 50..=30000, got {auto_wait_timeout_ms}"
            ),
        ));
    }
    let data_mime_type = params
        .data_mime_type
        .as_ref()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(mime_type) = data_mime_type.as_ref()
        && mime_type.len() > 255
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_drag/browser_drop data_mime_type must be at most 255 characters",
        ));
    }
    let data_text = params.data_text.clone();
    if let Some(text) = data_text.as_ref()
        && text.chars().count() > MAX_DRAG_DATA_CHARS
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_drag/browser_drop data_text must be at most {MAX_DRAG_DATA_CHARS} characters"
            ),
        ));
    }

    Ok(NormalizedBrowserDndParams {
        mode,
        source_selector,
        target_selector,
        steps,
        duration_ms,
        data_mime_type,
        data_text,
        wait_timeout_ms,
        auto_wait: params.auto_wait.unwrap_or(true),
        auto_wait_timeout_ms,
    })
}

fn validate_selector(name: &str, value: &str) -> Result<String, ErrorData> {
    let value = value.trim();
    if value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_drag/browser_drop {name} must be non-empty"),
        ));
    }
    if value.chars().count() > MAX_SELECTOR_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_drag/browser_drop {name} must be at most {MAX_SELECTOR_CHARS} characters"
            ),
        ));
    }
    Ok(value.to_owned())
}
