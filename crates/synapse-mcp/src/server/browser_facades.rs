//! Public browser facades for the <=40 MCP tool surface.

use super::{
    BrowserAddInitScriptParams, BrowserAddInitScriptResponse, BrowserAddScriptTagParams,
    BrowserAddStyleTagParams, BrowserAddTagResponse, BrowserConsoleMessagesParams,
    BrowserConsoleMessagesResponse, BrowserDownloadsParams, BrowserDownloadsResponse,
    BrowserEvaluateParams, BrowserEvaluateResponse, BrowserExposeBindingParams,
    BrowserExposeBindingResponse, BrowserPdfParams, BrowserPdfResponse, BrowserScreenshotParams,
    BrowserScreenshotResponse, CdpBridgeReloadParams, CdpBridgeReloadResponse, ErrorData, Json,
    Parameters, SynapseService,
    browser_dialog::{BrowserHandleDialogParams, BrowserHandleDialogResponse},
    browser_dnd::{BrowserDndParams, BrowserDndResponse},
    browser_emulate::{BrowserEmulateParams, BrowserEmulateResponse},
    browser_files::{BrowserFileUploadParams, BrowserFileUploadResponse},
    browser_network::{
        BrowserNetworkHarParams, BrowserNetworkHarResponse, BrowserNetworkOverridesParams,
        BrowserNetworkOverridesResponse, BrowserNetworkParams, BrowserNetworkResponse,
        BrowserRouteParams, BrowserRouteResponse,
    },
    tool,
    tool_profiles::ToolProfileKind,
    tool_router,
};
use rmcp::{RoleServer, model::ErrorCode, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_core::error_codes;

const BROWSER_CAPTURE_TOOL: &str = "browser_capture";
const BROWSER_DEBUGGER_TOOL: &str = "browser_debugger";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserCaptureOperation {
    Screenshot,
    Downloads,
}

impl BrowserCaptureOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Downloads => "downloads",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserCaptureParams {
    pub operation: BrowserCaptureOperation,
    #[serde(default)]
    pub screenshot: Option<BrowserScreenshotParams>,
    #[serde(default)]
    pub downloads: Option<BrowserDownloadsParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserCaptureResponse {
    pub operation: BrowserCaptureOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<BrowserScreenshotResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<BrowserDownloadsResponse>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDebuggerOperation {
    Evaluate,
    ConsoleMessages,
    ReloadBridge,
    Pdf,
    FileUpload,
    Dialog,
    AddInitScript,
    AddScriptTag,
    AddStyleTag,
    Network,
    NetworkHar,
    NetworkOverrides,
    Route,
    Emulate,
    ExposeBinding,
    Drag,
    Drop,
}

impl BrowserDebuggerOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Evaluate => "evaluate",
            Self::ConsoleMessages => "console_messages",
            Self::ReloadBridge => "reload_bridge",
            Self::Pdf => "pdf",
            Self::FileUpload => "file_upload",
            Self::Dialog => "dialog",
            Self::AddInitScript => "add_init_script",
            Self::AddScriptTag => "add_script_tag",
            Self::AddStyleTag => "add_style_tag",
            Self::Network => "network",
            Self::NetworkHar => "network_har",
            Self::NetworkOverrides => "network_overrides",
            Self::Route => "route",
            Self::Emulate => "emulate",
            Self::ExposeBinding => "expose_binding",
            Self::Drag => "drag",
            Self::Drop => "drop",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDebuggerParams {
    pub operation: BrowserDebuggerOperation,
    /// Envelope-level target alias (#1551/#1593). When set, it is folded into the
    /// selected operation spec's `cdp_target_id` before dispatch, so top-level
    /// addressing resolves the SAME target as the nested-spec form. Fails closed
    /// on a conflicting nested value. `reload_bridge` takes no target and rejects
    /// this alias.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Envelope-level target alias (#1551/#1593). Browser HWND that owns the
    /// target; folded into the selected operation spec's `window_hwnd`.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub evaluate: Option<BrowserEvaluateParams>,
    #[serde(default)]
    pub console_messages: Option<BrowserConsoleMessagesParams>,
    #[serde(default)]
    pub reload_bridge: Option<CdpBridgeReloadParams>,
    #[serde(default)]
    pub pdf: Option<BrowserPdfParams>,
    #[serde(default)]
    pub file_upload: Option<BrowserFileUploadParams>,
    #[serde(default)]
    pub dialog: Option<BrowserHandleDialogParams>,
    #[serde(default)]
    pub add_init_script: Option<BrowserAddInitScriptParams>,
    #[serde(default)]
    pub add_script_tag: Option<BrowserAddScriptTagParams>,
    #[serde(default)]
    pub add_style_tag: Option<BrowserAddStyleTagParams>,
    #[serde(default)]
    pub network: Option<BrowserNetworkParams>,
    #[serde(default)]
    pub network_har: Option<BrowserNetworkHarParams>,
    #[serde(default)]
    pub network_overrides: Option<BrowserNetworkOverridesParams>,
    #[serde(default)]
    pub route: Option<BrowserRouteParams>,
    #[serde(default)]
    pub emulate: Option<BrowserEmulateParams>,
    #[serde(default)]
    pub expose_binding: Option<BrowserExposeBindingParams>,
    #[serde(default)]
    pub drag: Option<BrowserDndParams>,
    #[serde(default)]
    pub drop: Option<BrowserDndParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserDebuggerResponse {
    pub operation: BrowserDebuggerOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluate: Option<BrowserEvaluateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_messages: Option<BrowserConsoleMessagesResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_bridge: Option<CdpBridgeReloadResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<BrowserPdfResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_upload: Option<BrowserFileUploadResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<BrowserHandleDialogResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_init_script: Option<BrowserAddInitScriptResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_script_tag: Option<BrowserAddTagResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_style_tag: Option<BrowserAddTagResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<BrowserNetworkResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_har: Option<BrowserNetworkHarResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_overrides: Option<BrowserNetworkOverridesResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<BrowserRouteResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulate: Option<BrowserEmulateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_binding: Option<BrowserExposeBindingResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drag: Option<BrowserDndResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop: Option<BrowserDndResponse>,
}

#[tool_router(router = browser_facade_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for browser capture and download readbacks in the <=40 public MCP surface. operation=screenshot requires screenshot params and writes an image artifact with byte/hash readback. operation=downloads requires downloads params and reads/waits/saves/moves Chrome download rows/events from the already-open Chrome profile. Exactly one operation-specific spec is accepted."
    )]
    pub async fn browser_capture(
        &self,
        params: Parameters<BrowserCaptureParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserCaptureResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = BROWSER_CAPTURE_TOOL,
            operation = params.0.operation.as_str(),
            "tool.invocation kind=browser_capture"
        );
        validate_browser_capture_params(&params.0)?;
        match params.0.operation {
            BrowserCaptureOperation::Screenshot => {
                let screenshot_params = params.0.screenshot.ok_or_else(|| {
                    browser_facade_error(
                        BROWSER_CAPTURE_TOOL,
                        error_codes::TOOL_PARAMS_INVALID,
                        BrowserCaptureOperation::Screenshot.as_str(),
                        "browser_capture operation=screenshot requires a screenshot spec",
                        "pass screenshot={...} and no other operation spec",
                    )
                })?;
                let screenshot = self
                    .browser_screenshot(Parameters(screenshot_params), request_context)
                    .await?
                    .0;
                let readback_source_of_truth = format!(
                    "browser_screenshot file={} bytes={} sha256={}",
                    screenshot.path, screenshot.bytes_written, screenshot.bitmap_sha256
                );
                Ok(Json(BrowserCaptureResponse {
                    operation: BrowserCaptureOperation::Screenshot,
                    source_of_truth: screenshot.source_of_truth.clone(),
                    readback_source_of_truth,
                    screenshot: Some(screenshot),
                    downloads: None,
                }))
            }
            BrowserCaptureOperation::Downloads => {
                let downloads_params = params.0.downloads.ok_or_else(|| {
                    browser_facade_error(
                        BROWSER_CAPTURE_TOOL,
                        error_codes::TOOL_PARAMS_INVALID,
                        BrowserCaptureOperation::Downloads.as_str(),
                        "browser_capture operation=downloads requires a downloads spec",
                        "pass downloads={...} and no other operation spec",
                    )
                })?;
                let downloads = self
                    .browser_downloads(Parameters(downloads_params), request_context)
                    .await?
                    .0;
                let readback_source_of_truth = browser_downloads_readback_source(&downloads);
                Ok(Json(BrowserCaptureResponse {
                    operation: BrowserCaptureOperation::Downloads,
                    source_of_truth: downloads.source_of_truth.clone(),
                    readback_source_of_truth,
                    screenshot: None,
                    downloads: Some(downloads),
                }))
            }
        }
    }

    #[tool(
        description = "Stable browser debugger facade. The tool name is visible in the default <=40 surface so clients with static tool namespaces can keep one callable route; each operation still fails closed with TOOL_PROFILE_POLICY_DENIED until this MCP session is explicitly set to browser_debugger (or a broader admin profile). Routes typed operations to target-scoped chrome.debugger/CDP browser tools: evaluate, console_messages, reload_bridge, pdf, file_upload, dialog, script/style injection, network/HAR/overrides/route, emulation, binding, drag, and drop. Exactly one operation-specific spec is accepted."
    )]
    pub async fn browser_debugger(
        &self,
        mut params: Parameters<BrowserDebuggerParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserDebuggerResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = BROWSER_DEBUGGER_TOOL,
            operation = params.0.operation.as_str(),
            "tool.invocation kind=browser_debugger"
        );
        validate_browser_debugger_params(&params.0)?;
        self.require_browser_debugger_facade_profile(&request_context, params.0.operation)?;
        // #1551/#1593: fold the envelope-level cdp_target_id/window_hwnd aliases
        // into the selected operation spec so top-level addressing resolves the
        // same target as the nested-spec form (fail-closed on conflict).
        merge_browser_debugger_top_level_target(&mut params.0)?;
        match params.0.operation {
            BrowserDebuggerOperation::Evaluate => {
                let delegate = params
                    .0
                    .evaluate
                    .ok_or_else(|| missing_debugger_spec("evaluate"))?;
                let response = self
                    .browser_evaluate(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Evaluate,
                    format!(
                        "Runtime.evaluate target={} url={} ready_state={}",
                        response.cdp_target_id, response.url, response.ready_state
                    ),
                    |out| out.evaluate = Some(response),
                )))
            }
            BrowserDebuggerOperation::ConsoleMessages => {
                let delegate = params
                    .0
                    .console_messages
                    .ok_or_else(|| missing_debugger_spec("console_messages"))?;
                let response = self
                    .browser_console_messages(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::ConsoleMessages,
                    format!(
                        "console buffer target={} returned={} next_cursor={}",
                        response.cdp_target_id, response.returned, response.next_cursor
                    ),
                    |out| out.console_messages = Some(response),
                )))
            }
            BrowserDebuggerOperation::ReloadBridge => {
                let delegate = params
                    .0
                    .reload_bridge
                    .ok_or_else(|| missing_debugger_spec("reload_bridge"))?;
                let response = self
                    .cdp_bridge_reload(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::ReloadBridge,
                    format!(
                        "bridge reload before_host={} after_host={} reconnected={} waited_ms={}",
                        response.before.host_id,
                        response.after.host_id,
                        response.reconnected,
                        response.waited_ms
                    ),
                    |out| out.reload_bridge = Some(response),
                )))
            }
            BrowserDebuggerOperation::Pdf => {
                let delegate = params.0.pdf.ok_or_else(|| missing_debugger_spec("pdf"))?;
                let response = self
                    .browser_pdf(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Pdf,
                    format!(
                        "pdf file={} bytes={} sha256={}",
                        response.path, response.bytes_written, response.pdf_sha256
                    ),
                    |out| out.pdf = Some(response),
                )))
            }
            BrowserDebuggerOperation::FileUpload => {
                let delegate = params
                    .0
                    .file_upload
                    .ok_or_else(|| missing_debugger_spec("file_upload"))?;
                let response = self
                    .browser_file_upload(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::FileUpload,
                    format!(
                        "file upload target={} readback_backend={} chooser_readback_backend={}",
                        response.cdp_target_id,
                        response.readback_backend,
                        response.chooser_readback_backend
                    ),
                    |out| out.file_upload = Some(response),
                )))
            }
            BrowserDebuggerOperation::Dialog => {
                let delegate = params
                    .0
                    .dialog
                    .ok_or_else(|| missing_debugger_spec("dialog"))?;
                let response = self
                    .browser_handle_dialog(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Dialog,
                    format!(
                        "dialog target={} returned={} next_cursor={}",
                        response.cdp_target_id, response.returned, response.next_cursor
                    ),
                    |out| out.dialog = Some(response),
                )))
            }
            BrowserDebuggerOperation::AddInitScript => {
                let delegate = params
                    .0
                    .add_init_script
                    .ok_or_else(|| missing_debugger_spec("add_init_script"))?;
                let response = self
                    .browser_add_init_script(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::AddInitScript,
                    format!(
                        "init script target={} identifier={}",
                        response.cdp_target_id, response.identifier
                    ),
                    |out| out.add_init_script = Some(response),
                )))
            }
            BrowserDebuggerOperation::AddScriptTag => {
                let delegate = params
                    .0
                    .add_script_tag
                    .ok_or_else(|| missing_debugger_spec("add_script_tag"))?;
                let response = self
                    .browser_add_script_tag(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::AddScriptTag,
                    format!(
                        "script tag target={} source_kind={}",
                        response.cdp_target_id, response.source_kind
                    ),
                    |out| out.add_script_tag = Some(response),
                )))
            }
            BrowserDebuggerOperation::AddStyleTag => {
                let delegate = params
                    .0
                    .add_style_tag
                    .ok_or_else(|| missing_debugger_spec("add_style_tag"))?;
                let response = self
                    .browser_add_style_tag(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::AddStyleTag,
                    format!(
                        "style tag target={} source_kind={}",
                        response.cdp_target_id, response.source_kind
                    ),
                    |out| out.add_style_tag = Some(response),
                )))
            }
            BrowserDebuggerOperation::Network => {
                let delegate = params
                    .0
                    .network
                    .ok_or_else(|| missing_debugger_spec("network"))?;
                let response = self
                    .browser_network(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Network,
                    format!("network mode={:?}", response.mode),
                    |out| out.network = Some(response),
                )))
            }
            BrowserDebuggerOperation::NetworkHar => {
                let delegate = params
                    .0
                    .network_har
                    .ok_or_else(|| missing_debugger_spec("network_har"))?;
                let response = self
                    .browser_network_har(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::NetworkHar,
                    format!(
                        "HAR target={} path={:?} har_bytes={} route_count={}",
                        response.cdp_target_id,
                        response.path,
                        response.har_bytes,
                        response.route_count
                    ),
                    |out| out.network_har = Some(response),
                )))
            }
            BrowserDebuggerOperation::NetworkOverrides => {
                let delegate = params
                    .0
                    .network_overrides
                    .ok_or_else(|| missing_debugger_spec("network_overrides"))?;
                let response = self
                    .browser_network_overrides(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::NetworkOverrides,
                    format!(
                        "network overrides target={} active={} cleared={}",
                        response.cdp_target_id, response.override_active, response.cleared
                    ),
                    |out| out.network_overrides = Some(response),
                )))
            }
            BrowserDebuggerOperation::Route => {
                let delegate = params
                    .0
                    .route
                    .ok_or_else(|| missing_debugger_spec("route"))?;
                let response = self
                    .browser_route(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Route,
                    format!(
                        "route target={} route_count={} removed={} cleared={}",
                        response.cdp_target_id,
                        response.route_count,
                        response.route_removed,
                        response.cleared_count
                    ),
                    |out| out.route = Some(response),
                )))
            }
            BrowserDebuggerOperation::Emulate => {
                let delegate = params
                    .0
                    .emulate
                    .ok_or_else(|| missing_debugger_spec("emulate"))?;
                let response = self
                    .browser_emulate(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Emulate,
                    format!(
                        "emulate target={} domains={}",
                        response.cdp_target_id,
                        response.domains.len()
                    ),
                    |out| out.emulate = Some(response),
                )))
            }
            BrowserDebuggerOperation::ExposeBinding => {
                let delegate = params
                    .0
                    .expose_binding
                    .ok_or_else(|| missing_debugger_spec("expose_binding"))?;
                let response = self
                    .browser_expose_binding(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::ExposeBinding,
                    format!(
                        "binding target={} name={} active={} next_cursor={}",
                        response.cdp_target_id,
                        response.name,
                        response.binding_active,
                        response.next_cursor
                    ),
                    |out| out.expose_binding = Some(response),
                )))
            }
            BrowserDebuggerOperation::Drag => {
                let delegate = params.0.drag.ok_or_else(|| missing_debugger_spec("drag"))?;
                let response = self
                    .browser_drag(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Drag,
                    format!(
                        "drag target={} status={} delegated_tool={}",
                        response.cdp_target_id, response.status, response.delegated_tool
                    ),
                    |out| out.drag = Some(response),
                )))
            }
            BrowserDebuggerOperation::Drop => {
                let delegate = params.0.drop.ok_or_else(|| missing_debugger_spec("drop"))?;
                let response = self
                    .browser_drop(Parameters(delegate), request_context)
                    .await?
                    .0;
                Ok(Json(browser_debugger_response(
                    BrowserDebuggerOperation::Drop,
                    format!(
                        "drop target={} status={} delegated_tool={}",
                        response.cdp_target_id, response.status, response.delegated_tool
                    ),
                    |out| out.drop = Some(response),
                )))
            }
        }
    }
}

/// Fold `browser_debugger`'s envelope-level `cdp_target_id`/`window_hwnd`
/// aliases (#1551/#1593) into whichever operation spec is present, reusing the
/// same [`merge_top_level_target`] resolver every other browser facade uses.
/// `reload_bridge` reloads the whole CDP bridge rather than a single target, so
/// it fails closed when a top-level target is supplied.
fn merge_browser_debugger_top_level_target(
    params: &mut BrowserDebuggerParams,
) -> Result<(), ErrorData> {
    let top_cdp = params.cdp_target_id.clone();
    let top_hwnd = params.window_hwnd;
    if top_cdp.is_none() && top_hwnd.is_none() {
        return Ok(());
    }
    let operation = params.operation.as_str();
    if matches!(params.operation, BrowserDebuggerOperation::ReloadBridge) {
        return Err(browser_facade_error(
            BROWSER_DEBUGGER_TOOL,
            error_codes::TOOL_PARAMS_INVALID,
            operation,
            "browser_debugger operation=reload_bridge does not address a single target; reload_bridge reloads the whole CDP bridge",
            "drop the top-level cdp_target_id/window_hwnd for reload_bridge",
        ));
    }
    macro_rules! fold {
        ($spec:expr) => {
            if let Some(spec) = $spec.as_mut() {
                merge_top_level_target(
                    BROWSER_DEBUGGER_TOOL,
                    operation,
                    top_cdp.as_deref(),
                    top_hwnd,
                    &mut spec.cdp_target_id,
                    &mut spec.window_hwnd,
                )?;
            }
        };
    }
    fold!(params.evaluate);
    fold!(params.console_messages);
    fold!(params.pdf);
    fold!(params.file_upload);
    fold!(params.dialog);
    fold!(params.add_init_script);
    fold!(params.add_script_tag);
    fold!(params.add_style_tag);
    fold!(params.network);
    fold!(params.network_har);
    fold!(params.network_overrides);
    fold!(params.route);
    fold!(params.emulate);
    fold!(params.expose_binding);
    fold!(params.drag);
    fold!(params.drop);
    Ok(())
}

fn validate_browser_capture_params(params: &BrowserCaptureParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        BROWSER_CAPTURE_TOOL,
        params.operation.as_str(),
        &[
            ("screenshot", params.screenshot.is_some()),
            ("downloads", params.downloads.is_some()),
        ],
    )
}

fn validate_browser_debugger_params(params: &BrowserDebuggerParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        BROWSER_DEBUGGER_TOOL,
        params.operation.as_str(),
        &[
            ("evaluate", params.evaluate.is_some()),
            ("console_messages", params.console_messages.is_some()),
            ("reload_bridge", params.reload_bridge.is_some()),
            ("pdf", params.pdf.is_some()),
            ("file_upload", params.file_upload.is_some()),
            ("dialog", params.dialog.is_some()),
            ("add_init_script", params.add_init_script.is_some()),
            ("add_script_tag", params.add_script_tag.is_some()),
            ("add_style_tag", params.add_style_tag.is_some()),
            ("network", params.network.is_some()),
            ("network_har", params.network_har.is_some()),
            ("network_overrides", params.network_overrides.is_some()),
            ("route", params.route.is_some()),
            ("emulate", params.emulate.is_some()),
            ("expose_binding", params.expose_binding.is_some()),
            ("drag", params.drag.is_some()),
            ("drop", params.drop.is_some()),
        ],
    )
}

fn validate_exact_operation_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
) -> Result<(), ErrorData> {
    let present = specs
        .iter()
        .filter_map(|(name, is_present)| is_present.then_some(*name))
        .collect::<Vec<_>>();
    if !present.contains(&operation) {
        return Err(browser_facade_error(
            tool,
            error_codes::TOOL_PARAMS_INVALID,
            operation,
            format!("{tool} operation={operation} requires a matching {operation} spec"),
            format!("pass {operation}={{...}} and no other operation spec"),
        ));
    }
    if present.len() != 1 {
        return Err(browser_facade_error(
            tool,
            error_codes::TOOL_PARAMS_INVALID,
            operation,
            format!("{tool} operation={operation} received invalid operation specs {present:?}"),
            format!("pass exactly one operation-specific spec matching {operation}"),
        ));
    }
    Ok(())
}

fn missing_debugger_spec(operation: &'static str) -> ErrorData {
    browser_facade_error(
        BROWSER_DEBUGGER_TOOL,
        error_codes::TOOL_PARAMS_INVALID,
        operation,
        format!("browser_debugger operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn browser_facade_error(
    tool: &'static str,
    code: &'static str,
    operation: &'static str,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": code,
            "tool": tool,
            "operation": operation,
            "source_of_truth": "typed browser facade params before delegated MCP tool call",
            "remediation": remediation.into(),
        })),
    )
}

/// Unify top-level `cdp_target_id`/`window_hwnd` envelope aliases (#1551) into an
/// operation spec's own target fields so every browser facade accepts target
/// addressing at the envelope top level and routes it to the exact same nested
/// resolver the spec form already triggers. An agent passing top-level
/// `cdp_target_id` therefore resolves the SAME target as the equivalent
/// nested-spec form.
///
/// Precedence (fail-closed): when BOTH the top-level alias and the same nested
/// spec field carry a value and they CONFLICT, this returns a typed
/// [`error_codes::TOOL_PARAMS_INVALID`] error naming both locations; when only one
/// side is set the spec field is populated from it; when the nested spec already
/// carries the value and the top-level alias is absent the spec is left unchanged.
pub(super) fn merge_top_level_target(
    tool: &'static str,
    operation: &str,
    top_cdp_target_id: Option<&str>,
    top_window_hwnd: Option<i64>,
    spec_cdp_target_id: &mut Option<String>,
    spec_window_hwnd: &mut Option<i64>,
) -> Result<(), ErrorData> {
    if let Some(top) = top_cdp_target_id {
        match spec_cdp_target_id.as_deref() {
            Some(nested) if nested != top => {
                return Err(target_alias_conflict_error(
                    tool,
                    operation,
                    "cdp_target_id",
                    top.to_owned(),
                    nested.to_owned(),
                ));
            }
            _ => *spec_cdp_target_id = Some(top.to_owned()),
        }
    }
    if let Some(top) = top_window_hwnd {
        match *spec_window_hwnd {
            Some(nested) if nested != top => {
                return Err(target_alias_conflict_error(
                    tool,
                    operation,
                    "window_hwnd",
                    format!("{top:#x}"),
                    format!("{nested:#x}"),
                ));
            }
            _ => *spec_window_hwnd = Some(top),
        }
    }
    Ok(())
}

fn target_alias_conflict_error(
    tool: &'static str,
    operation: &str,
    field: &'static str,
    top_level: String,
    nested: String,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{tool} operation={operation} received conflicting {field}: top-level {field}={top_level} disagrees with nested {operation}.{field}={nested}"
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "field": field,
            "top_level_value": top_level,
            "nested_spec_value": nested,
            "source_of_truth": "unified browser facade target addressing (#1551): top-level cdp_target_id/window_hwnd alias the nested operation spec's target fields",
            "remediation": format!(
                "pass {field} at the envelope top level OR inside the operation spec, not both with different values"
            ),
        })),
    )
}

fn browser_downloads_readback_source(response: &BrowserDownloadsResponse) -> String {
    match (
        &response.saved_path,
        response.saved_bytes,
        &response.saved_sha256,
    ) {
        (Some(path), Some(bytes), Some(sha)) => {
            format!("browser_downloads file={path} bytes={bytes} sha256={sha}")
        }
        _ => format!(
            "browser_downloads source={} returned={} events={} next_event_cursor={}",
            response.source_of_truth,
            response.returned,
            response.event_count,
            response.next_event_cursor
        ),
    }
}

fn browser_debugger_response(
    operation: BrowserDebuggerOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut BrowserDebuggerResponse),
) -> BrowserDebuggerResponse {
    let mut response = BrowserDebuggerResponse {
        operation,
        source_of_truth: format!(
            "CF_SESSIONS mcp/tool-profile/v1/<session_id> profile=browser_debugger + delegated browser operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        evaluate: None,
        console_messages: None,
        reload_bridge: None,
        pdf: None,
        file_upload: None,
        dialog: None,
        add_init_script: None,
        add_script_tag: None,
        add_style_tag: None,
        network: None,
        network_har: None,
        network_overrides: None,
        route: None,
        emulate: None,
        expose_binding: None,
        drag: None,
        drop: None,
    };
    populate(&mut response);
    response
}

impl SynapseService {
    fn require_browser_debugger_facade_profile(
        &self,
        request_context: &RequestContext<RoleServer>,
        operation: BrowserDebuggerOperation,
    ) -> Result<(), ErrorData> {
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?
            .ok_or_else(|| {
                browser_facade_error(
                    BROWSER_DEBUGGER_TOOL,
                    error_codes::HTTP_SESSION_INVALID,
                    operation.as_str(),
                    "browser_debugger requires an MCP session id so the profile policy row can be read",
                    "initialize a session, set profile=browser_debugger with a reason, then retry",
                )
            })?;
        let snapshot = self.tool_profile_snapshot(Some(&session_id))?;
        if matches!(
            snapshot.profile,
            ToolProfileKind::BrowserDebugger
                | ToolProfileKind::BreakGlass
                | ToolProfileKind::FullCapability
        ) {
            return Ok(());
        }
        let error = ErrorData::new(
            ErrorCode(-32099),
            format!(
                "browser_debugger operation={} requires profile=browser_debugger for session {session_id}; current profile={}",
                operation.as_str(),
                snapshot.profile.as_str()
            ),
            Some(json!({
                "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
                "tool": BROWSER_DEBUGGER_TOOL,
                "operation": operation.as_str(),
                "session_id": &session_id,
                "profile": snapshot.profile.as_str(),
                "profile_label": snapshot.profile_label,
                "source_of_truth": "CF_SESSIONS mcp/tool-profile/v1/<session_id> profile row",
                "policy_row": &snapshot.policy_row,
                "visible_tool_count": snapshot.visible_tool_count,
                "resolution": "call profile operation=set profile=browser_debugger confirm_break_glass=true with a non-empty reason, then retry browser_debugger operation",
            })),
        );
        tracing::warn!(
            code = error_codes::TOOL_PROFILE_POLICY_DENIED,
            tool = BROWSER_DEBUGGER_TOOL,
            operation = operation.as_str(),
            session_id = %session_id,
            profile = snapshot.profile.as_str(),
            "browser_debugger facade denied operation because session profile is not browser_debugger"
        );
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                BROWSER_DEBUGGER_TOOL,
                operation.as_str(),
                Some(session_id.clone()),
                Some(session_id),
                json!({
                    "operation": operation.as_str(),
                    "requested_tool": BROWSER_DEBUGGER_TOOL,
                    "required_profile": ToolProfileKind::BrowserDebugger.as_str(),
                }),
                json!({
                    "source_of_truth": "CF_SESSIONS mcp/tool-profile/v1/<session_id> profile row",
                    "policy_row": &snapshot.policy_row,
                    "visible_tool_count": snapshot.visible_tool_count,
                }),
                json!({
                    "source_of_truth": "CF_ACTION_LOG command_audit row",
                    "denied_tool": BROWSER_DEBUGGER_TOOL,
                    "denied_operation": operation.as_str(),
                    "profile": snapshot.profile.as_str(),
                }),
                "error",
            )
            .with_error(super::command_audit::command_audit_error_from_error_data(
                &error,
            )),
        )?;
        Err(error)
    }
}

#[cfg(test)]
mod merge_top_level_target_tests {
    use super::*;
    use serde_json::Value;

    /// #1551/#1593: a top-level `cdp_target_id`/`window_hwnd` on
    /// `browser_debugger` (the telemetry payload that used to fail closed as
    /// `unknown field`) now deserializes AND is folded into the selected
    /// operation spec, so envelope addressing resolves the SAME target as the
    /// nested-spec form. This exercises the real deserialize path end to end.
    #[test]
    fn browser_debugger_top_level_target_folds_into_operation_spec_1593() {
        let mut params: BrowserDebuggerParams = serde_json::from_value(json!({
            "operation": "evaluate",
            "cdp_target_id": "TARGET-1593-DBG",
            "window_hwnd": 0x1234,
            "evaluate": { "expression": "1 + 1" },
        }))
        .expect("top-level target now deserializes on browser_debugger");
        println!(
            "readback=before cdp_target_id={:?}",
            params
                .evaluate
                .as_ref()
                .and_then(|spec| spec.cdp_target_id.clone())
        );
        merge_browser_debugger_top_level_target(&mut params)
            .expect("top-level target folds into the evaluate spec");
        let spec = params.evaluate.expect("evaluate spec present");
        println!(
            "readback=after cdp_target_id={:?} window_hwnd={:?}",
            spec.cdp_target_id, spec.window_hwnd
        );
        assert_eq!(spec.cdp_target_id.as_deref(), Some("TARGET-1593-DBG"));
        assert_eq!(spec.window_hwnd, Some(0x1234));
    }

    /// #1551/#1593: a top-level target that conflicts with the value already in
    /// the operation spec must fail closed rather than silently pick one.
    #[test]
    fn browser_debugger_top_level_target_conflict_fails_closed_1593() {
        let mut params: BrowserDebuggerParams = serde_json::from_value(json!({
            "operation": "evaluate",
            "cdp_target_id": "TARGET-1593-DBG",
            "evaluate": { "expression": "1 + 1", "cdp_target_id": "OTHER-TARGET" },
        }))
        .expect("both target locations deserialize");
        let err = merge_browser_debugger_top_level_target(&mut params)
            .expect_err("conflicting top-level and nested cdp_target_id must fail closed");
        let code = err
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        println!("readback=conflict code={code:?} message={}", err.message);
        assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
    }

    /// #1593: `reload_bridge` reloads the whole CDP bridge, not a single target,
    /// so a top-level target alias must fail closed.
    #[test]
    fn browser_debugger_reload_bridge_rejects_top_level_target_1593() {
        let mut params: BrowserDebuggerParams = serde_json::from_value(json!({
            "operation": "reload_bridge",
            "cdp_target_id": "TARGET-1593-DBG",
        }))
        .expect("reload_bridge with top-level target deserializes");
        let err = merge_browser_debugger_top_level_target(&mut params)
            .expect_err("reload_bridge must reject a top-level target");
        println!("readback=reload_bridge rejection message={}", err.message);
        assert!(
            err.message.contains("reload_bridge"),
            "message: {}",
            err.message
        );
    }

    /// #1593: no envelope target supplied is a no-op; the nested spec is
    /// untouched so the happy path stays identical.
    #[test]
    fn browser_debugger_without_top_level_target_is_noop_1593() {
        let mut params: BrowserDebuggerParams = serde_json::from_value(json!({
            "operation": "evaluate",
            "evaluate": { "expression": "1 + 1", "cdp_target_id": "NESTED-ONLY" },
        }))
        .expect("nested-only target deserializes");
        merge_browser_debugger_top_level_target(&mut params)
            .expect("no envelope target is a no-op");
        assert_eq!(
            params
                .evaluate
                .expect("evaluate spec present")
                .cdp_target_id
                .as_deref(),
            Some("NESTED-ONLY")
        );
    }
}
