//! Browser drag-and-drop tools (#1144/#1145) for the normal authenticated
//! Chrome bridge.

use super::{ErrorData, Json, Parameters, SessionTarget, SynapseService, tool, tool_router};
use crate::m1::mcp_error;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const BROWSER_DRAG_TOOL: &str = "browser_drag";
const BROWSER_DROP_TOOL: &str = "browser_drop";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
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
    /// Real, trusted (isTrusted=true) HTML5 drop via CDP
    /// `Input.dispatchDragEvent` (dragEnter/dragOver/drop) onto the target with a
    /// constructed DragData built from `data_mime_type`/`data_text` (or the
    /// source element's text). Drives native drop zones that gate on
    /// `event.isTrusted` and read `dataTransfer` (#1356).
    Html5Real,
}

impl BrowserDndMode {
    const fn bridge_action(self) -> &'static str {
        match self {
            Self::Mouse => "drag",
            Self::Html5 => "html5_drag",
            Self::Html5Real => "html5_real_drag",
        }
    }

    const fn delegated_method(self) -> &'static str {
        match self {
            Self::Mouse => "chrome_debugger_bridge.cdpInput.drag",
            Self::Html5 => "chrome_debugger_bridge.cdpInput.html5_drag",
            Self::Html5Real => "chrome_debugger_bridge.cdpInput.html5_real_drag",
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
    /// Chrome bridge tab target id (`chrome-tab:<id>`). Defaults to this
    /// session's active CDP target.
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
        description = "Drag one element to another in the calling session's owned normal Chrome bridge tab (#1144). Defaults to a target-scoped chrome.debugger CDP Input mouse sequence: mouseMoved to source, mousePressed, configurable dragMove steps, mouseReleased on target. Source and target are strict CSS selectors resolved/actionability-checked in the already-open authenticated Chrome profile. Background-safe: no OS foreground input, no helper Chrome process, and no human foreground fallback."
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
        description = "Dispatch an HTML5 drag-and-drop from one element to another in the calling session's owned normal Chrome bridge tab (#1145). Defaults to in-page DragEvent dragstart/dragenter/dragover/drop/dragend with a real DataTransfer payload, reporting whether dragover was cancelled so drop-capable zones can be verified. Set mode=mouse to use the same CDP mouse sequence as browser_drag. Background-safe and scoped to the already-open authenticated Chrome profile."
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
        if synapse_a11y::endpoint_for_window(window_hwnd).is_some() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} targets the normal Chrome extension bridge, but window {window_hwnd} exposes a raw CDP debug endpoint; use raw-CDP primitives for a Synapse automation profile"
                ),
            ));
        }
        if !cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} requires a normal Chrome bridge tab target ({CHROME_TAB_PREFIX}<id>); got {cdp_target_id:?}"
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
            .browser_dnd_run(tool, window_hwnd, &cdp_target_id, &dnd)
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
        window_hwnd: i64,
        cdp_target_id: &str,
        dnd: &NormalizedBrowserDndParams,
    ) -> Result<BrowserDndResponse, ErrorData> {
        super::operator_panic_boundary::ensure_mcp_mutation(
            "browser_drag_drop_before_bridge_input",
        )?;
        let result = crate::chrome_debugger_bridge::cdp_input(
            crate::chrome_debugger_bridge::ChromeDebuggerCdpInputRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                action: dnd.mode.bridge_action(),
                selector: None,
                element_id: None,
                active_element: false,
                role: None,
                name: None,
                value: None,
                text: None,
                x: None,
                y: None,
                coordinate_space: None,
                source_selector: Some(dnd.source_selector.as_str()),
                target_selector: Some(dnd.target_selector.as_str()),
                drag_steps: Some(dnd.steps),
                drag_duration_ms: Some(dnd.duration_ms),
                drag_data_mime_type: dnd.data_mime_type.as_deref(),
                drag_data_text: dnd.data_text.as_deref(),
                button: None,
                modifiers: None,
                clicks: None,
                wait_timeout_ms: dnd.wait_timeout_ms,
                auto_wait: dnd.auto_wait,
                auto_wait_timeout_ms: dnd.auto_wait_timeout_ms,
                suppress_page_text: false,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{tool} bridge {} failed for target {cdp_target_id:?}: {}",
                    dnd.mode.bridge_action(),
                    error.detail()
                ),
            )
        })?;
        super::operator_panic_boundary::ensure_mcp_mutation(
            "browser_drag_drop_after_bridge_input",
        )?;

        Ok(BrowserDndResponse {
            ok: true,
            required_foreground: false,
            transport: "chrome_tabs_extension".to_owned(),
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
            mode: dnd.mode,
            source_selector: dnd.source_selector.clone(),
            target_selector: dnd.target_selector.clone(),
            steps: dnd.steps,
            duration_ms: dnd.duration_ms,
            delegated_tool: dnd.mode.delegated_method().to_owned(),
            status: "dispatched_with_bridge_readback".to_owned(),
            result,
        })
    }
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
