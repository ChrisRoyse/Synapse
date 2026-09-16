//! File upload MCP tool (#1101-#1105) backed by the normal Chrome bridge.
//!
//! The bridge path uses `DOM.setFileInputFiles` for direct input assignment and
//! `Page.setInterceptFileChooserDialog`/`Page.fileChooserOpened` for chooser
//! interception. It never opens an OS file picker and never activates Chrome.

use std::path::{Path, PathBuf};

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{
        browser_raw_cdp_required_error, cdp_target_id_audit_ref, chrome_debugger_default_endpoint,
        chrome_debugger_endpoint, require_target_session_id, validate_cdp_target_id,
    },
    tool, tool_router,
};
use crate::m1::mcp_error;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_core::error_codes;

const TOOL: &str = "browser_file_upload";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
const DEFAULT_CHOOSER_READ_LIMIT: usize = 20;
const MAX_CHOOSER_READ_LIMIT: usize = 100;
const MAX_FILE_UPLOAD_PATHS: usize = 256;
const MAX_SELECTOR_CHARS: usize = 4096;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFileUploadOperation {
    /// Set files directly on a resolved input[type=file].
    #[default]
    SetFiles,
    /// Clear files directly on a resolved input[type=file].
    Clear,
    /// Enable file chooser interception for the target tab.
    ArmChooser,
    /// Read captured file chooser events.
    ReadChooser,
    /// Resolve the currently pending chooser and set files on its backing node.
    SetChooser,
    /// Clear the currently pending chooser record without setting files.
    CancelChooser,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFileUploadParams {
    /// Operation to run. Defaults to `set_files`.
    #[serde(default)]
    pub operation: BrowserFileUploadOperation,
    /// Local files to assign. Required for `set_files` and `set_chooser`.
    #[serde(default)]
    pub files: Vec<String>,
    /// Strict CSS selector for direct `set_files`/`clear`.
    #[serde(default)]
    pub selector: Option<String>,
    /// Synapse bridge element id for direct `set_files`/`clear`.
    #[serde(default)]
    pub element_id: Option<String>,
    /// Target the tab's current `document.activeElement` for direct
    /// `set_files`/`clear`.
    #[serde(default)]
    pub active_element: bool,
    /// CDP TargetID to mutate. Defaults to the active session CDP target. Must
    /// be a normal Chrome bridge target (`chrome-tab:<tabId>`).
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Required only with an explicit target and
    /// no active session target.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Return only chooser records with `seq >= since_seq`.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum chooser history entries to return. Defaults to 20, max 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BrowserFileUploadFile {
    pub name: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub file_type: String,
    pub last_modified: f64,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BrowserFileUploadInput {
    pub resolved_by: String,
    pub match_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_node_id: Option<i64>,
    pub tag_name: String,
    pub type_attr: String,
    pub id: String,
    pub name_attr: String,
    pub accept: String,
    pub multiple: bool,
    pub webkitdirectory: bool,
    pub disabled: bool,
    pub file_count: usize,
    pub files: Vec<BrowserFileUploadFile>,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BrowserFileChooserEntry {
    pub seq: u64,
    pub frame_id: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_node_id: Option<i64>,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_file_count: Option<usize>,
    pub file_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<BrowserFileUploadInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFileUploadResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserFileUploadOperation,
    pub capture_newly_armed: bool,
    pub requested_file_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<BrowserFileUploadInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_chooser: Option<BrowserFileChooserEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled_chooser: Option<BrowserFileChooserEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_chooser: Option<BrowserFileChooserEntry>,
    pub entries: Vec<BrowserFileChooserEntry>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub opened_count: u64,
    pub handled_count: u64,
    pub canceled_count: u64,
    pub error_count: u64,
    pub readback_backend: String,
    pub chooser_readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedBrowserFileUploadParams {
    operation: BrowserFileUploadOperation,
    files: Vec<String>,
    selector: Option<String>,
    element_id: Option<String>,
    active_element: bool,
    since_seq: Option<u64>,
    limit: usize,
}

#[tool_router(router = browser_files_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Set or clear input[type=file] files and intercept file chooser openings in the calling session's owned normal Chrome bridge tab. Direct operations use DOM.setFileInputFiles by selector, bridge element_id, or active_element; chooser operations arm Page.setInterceptFileChooserDialog so clicking an input records Page.fileChooserOpened without opening the OS picker, then set_chooser assigns files to the pending backend node. Paths are validated locally before Chrome is called. Background-safe: never activates Chrome, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_file_upload(
        &self,
        params: Parameters<BrowserFileUploadParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserFileUploadResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_file_upload"
        );
        let session_id = require_target_session_id(&request_context)?;
        let upload = validate_browser_file_upload_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": upload.operation,
            "requested_file_count": upload.files.len(),
            "selector": upload.selector.as_deref(),
            "element_id_present": upload.element_id.is_some(),
            "active_element": upload.active_element,
            "since_seq": upload.since_seq,
            "limit": upload.limit,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": upload.operation,
            "requested_file_count": upload.files.len(),
            "selector": upload.selector.as_deref(),
            "element_id_present": upload.element_id.is_some(),
            "active_element": upload.active_element,
            "since_seq": upload.since_seq,
            "limit": upload.limit,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_file_upload_impl(&session_id, window_hwnd, &cdp_target_id, &upload)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_file_upload_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        upload: &NormalizedBrowserFileUploadParams,
    ) -> Result<BrowserFileUploadResponse, ErrorData> {
        if synapse_a11y::endpoint_for_window(window_hwnd).is_some() {
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        }
        if !cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{TOOL} requires a normal Chrome bridge tab target ({CHROME_TAB_PREFIX}<id>); got {cdp_target_id:?}"
                ),
            ));
        }
        let operation = browser_file_upload_operation_name(upload.operation);
        if upload.operation != BrowserFileUploadOperation::ReadChooser {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_file_upload_before_bridge_mutation",
            )?;
        }
        let result = crate::chrome_debugger_bridge::file_upload(
            crate::chrome_debugger_bridge::ChromeDebuggerFileUploadRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                operation,
                files: &upload.files,
                selector: upload.selector.as_deref(),
                element_id: upload.element_id.as_deref(),
                active_element: upload.active_element,
                since_seq: upload.since_seq,
                limit: upload.limit,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{TOOL} normal Chrome bridge DOM.setFileInputFiles/Page.fileChooserOpened failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        if upload.operation != BrowserFileUploadOperation::ReadChooser {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_file_upload_after_bridge_mutation",
            )?;
        }
        let endpoint = result
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        tracing::info!(
            code = "CHROME_BRIDGE_FILE_UPLOAD_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %result.target_id,
            operation = %result.operation,
            requested_file_count = result.requested_file_count,
            opened_count = result.opened_count,
            handled_count = result.handled_count,
            "readback=chrome.debugger.DOM.setFileInputFiles+Page.fileChooserOpened outcome=file_upload_status"
        );
        Ok(browser_file_upload_bridge_response(
            session_id,
            window_hwnd,
            endpoint,
            upload.operation,
            result,
        ))
    }

    #[cfg(not(windows))]
    async fn browser_file_upload_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _upload: &NormalizedBrowserFileUploadParams,
    ) -> Result<BrowserFileUploadResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_file_upload is only available on Windows in this build",
        ))
    }
}

fn validate_browser_file_upload_params(
    params: &BrowserFileUploadParams,
) -> Result<NormalizedBrowserFileUploadParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let limit = params
        .limit
        .unwrap_or(DEFAULT_CHOOSER_READ_LIMIT)
        .clamp(1, MAX_CHOOSER_READ_LIMIT);
    let selector = normalize_optional_text(params.selector.as_deref(), "selector")?;
    if let Some(selector) = selector.as_deref()
        && selector.chars().count() > MAX_SELECTOR_CHARS
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} selector must be at most {MAX_SELECTOR_CHARS} Unicode scalar values"),
        ));
    }
    let element_id = normalize_optional_text(params.element_id.as_deref(), "element_id")?;
    let locator_count = usize::from(selector.is_some())
        + usize::from(element_id.is_some())
        + usize::from(params.active_element);

    match params.operation {
        BrowserFileUploadOperation::SetFiles | BrowserFileUploadOperation::Clear => {
            if locator_count != 1 {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "{TOOL} operation={} requires exactly one of selector, element_id, or active_element=true",
                        browser_file_upload_operation_name(params.operation)
                    ),
                ));
            }
        }
        BrowserFileUploadOperation::ArmChooser
        | BrowserFileUploadOperation::ReadChooser
        | BrowserFileUploadOperation::SetChooser
        | BrowserFileUploadOperation::CancelChooser => {
            if locator_count != 0 {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "{TOOL} operation={} uses the pending file chooser and must not include selector, element_id, or active_element",
                        browser_file_upload_operation_name(params.operation)
                    ),
                ));
            }
        }
    }

    let files = match params.operation {
        BrowserFileUploadOperation::SetFiles | BrowserFileUploadOperation::SetChooser => {
            validate_upload_files(&params.files)?
        }
        BrowserFileUploadOperation::Clear
        | BrowserFileUploadOperation::ArmChooser
        | BrowserFileUploadOperation::ReadChooser
        | BrowserFileUploadOperation::CancelChooser => {
            if !params.files.is_empty() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "{TOOL} files is only valid for operation=set_files or operation=set_chooser"
                    ),
                ));
            }
            Vec::new()
        }
    };

    Ok(NormalizedBrowserFileUploadParams {
        operation: params.operation,
        files,
        selector,
        element_id,
        active_element: params.active_element,
        since_seq: params.since_seq,
        limit,
    })
}

fn normalize_optional_text(value: Option<&str>, field: &str) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} {field} must not contain NUL"),
        ));
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_owned()))
}

fn validate_upload_files(files: &[String]) -> Result<Vec<String>, ErrorData> {
    if files.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} operation=set_files/set_chooser requires one or more files"),
        ));
    }
    if files.len() > MAX_FILE_UPLOAD_PATHS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} supports at most {MAX_FILE_UPLOAD_PATHS} files per call, got {}",
                files.len()
            ),
        ));
    }
    files
        .iter()
        .enumerate()
        .map(|(index, path)| validate_upload_file(index, path))
        .collect()
}

fn validate_upload_file(index: usize, path: &str) -> Result<String, ErrorData> {
    if path.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} files[{index}] must not contain NUL"),
        ));
    }
    if path.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} files[{index}] must not be empty"),
        ));
    }
    let input = Path::new(path);
    let canonical = input.canonicalize().map_err(|error| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{TOOL} files[{index}] path does not exist or cannot be resolved: {} ({error})",
                input.display()
            ),
        )
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{TOOL} files[{index}] path cannot be read: {} ({error})",
                canonical.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{TOOL} files[{index}] must be a regular file: {}",
                canonical.display()
            ),
        ));
    }
    Ok(path_to_chrome_string(canonical))
}

fn path_to_chrome_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

fn browser_file_upload_operation_name(operation: BrowserFileUploadOperation) -> &'static str {
    match operation {
        BrowserFileUploadOperation::SetFiles => "set_files",
        BrowserFileUploadOperation::Clear => "clear",
        BrowserFileUploadOperation::ArmChooser => "arm_chooser",
        BrowserFileUploadOperation::ReadChooser => "read_chooser",
        BrowserFileUploadOperation::SetChooser => "set_chooser",
        BrowserFileUploadOperation::CancelChooser => "cancel_chooser",
    }
}

fn browser_file_upload_operation_from_wire(
    value: &str,
    fallback: BrowserFileUploadOperation,
) -> BrowserFileUploadOperation {
    match value {
        "set_files" => BrowserFileUploadOperation::SetFiles,
        "clear" => BrowserFileUploadOperation::Clear,
        "arm_chooser" => BrowserFileUploadOperation::ArmChooser,
        "read_chooser" => BrowserFileUploadOperation::ReadChooser,
        "set_chooser" => BrowserFileUploadOperation::SetChooser,
        "cancel_chooser" => BrowserFileUploadOperation::CancelChooser,
        _ => fallback,
    }
}

fn browser_file_upload_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    endpoint: String,
    requested_operation: BrowserFileUploadOperation,
    result: crate::chrome_debugger_bridge::ChromeDebuggerFileUploadResult,
) -> BrowserFileUploadResponse {
    BrowserFileUploadResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint,
        cdp_target_id: result.target_id,
        operation: browser_file_upload_operation_from_wire(&result.operation, requested_operation),
        capture_newly_armed: result.capture_newly_armed,
        requested_file_count: result.requested_file_count,
        input: result.input.map(file_upload_input_from_bridge),
        handled_chooser: result.handled_chooser.map(file_chooser_entry_from_bridge),
        canceled_chooser: result.canceled_chooser.map(file_chooser_entry_from_bridge),
        pending_chooser: result.pending_chooser.map(file_chooser_entry_from_bridge),
        entries: result
            .entries
            .into_iter()
            .map(file_chooser_entry_from_bridge)
            .collect(),
        next_cursor: result.next_cursor,
        returned: result.returned,
        total_buffered: result.total_buffered,
        dropped: result.dropped,
        opened_count: result.opened_count,
        handled_count: result.handled_count,
        canceled_count: result.canceled_count,
        error_count: result.error_count,
        readback_backend: if result.readback_backend.trim().is_empty() {
            "chrome.debugger.DOM.setFileInputFiles+Runtime.callFunctionOn".to_owned()
        } else {
            result.readback_backend
        },
        chooser_readback_backend: if result.chooser_readback_backend.trim().is_empty() {
            "chrome.debugger.Page.setInterceptFileChooserDialog+Page.fileChooserOpened".to_owned()
        } else {
            result.chooser_readback_backend
        },
        backend_tier_used: if result.backend_tier_used.trim().is_empty() {
            "chrome_tabs_extension".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
    }
}

fn file_upload_input_from_bridge(
    input: crate::chrome_debugger_bridge::ChromeDebuggerFileUploadInput,
) -> BrowserFileUploadInput {
    BrowserFileUploadInput {
        resolved_by: input.resolved_by,
        match_count: input.match_count,
        frame_id: input.frame_id,
        element_path: input.element_path,
        backend_node_id: input.backend_node_id,
        tag_name: input.tag_name,
        type_attr: input.type_attr,
        id: input.id,
        name_attr: input.name_attr,
        accept: input.accept,
        multiple: input.multiple,
        webkitdirectory: input.webkitdirectory,
        disabled: input.disabled,
        file_count: input.file_count,
        files: input
            .files
            .into_iter()
            .map(|file| BrowserFileUploadFile {
                name: file.name,
                size: file.size,
                file_type: file.r#type,
                last_modified: file.last_modified,
            })
            .collect(),
        value: input.value,
    }
}

fn file_chooser_entry_from_bridge(
    entry: crate::chrome_debugger_bridge::ChromeDebuggerFileChooserEntry,
) -> BrowserFileChooserEntry {
    BrowserFileChooserEntry {
        seq: entry.seq,
        frame_id: entry.frame_id,
        mode: entry.mode,
        backend_node_id: entry.backend_node_id,
        opened_at_unix_ms: entry.opened_at_unix_ms,
        pending: entry.pending,
        handled_at_unix_ms: entry.handled_at_unix_ms,
        canceled_at_unix_ms: entry.canceled_at_unix_ms,
        requested_file_count: entry.requested_file_count,
        file_names: entry.file_names,
        input: entry.input.map(file_upload_input_from_bridge),
        error: entry.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_file_upload_validation_edges() {
        let temp = tempfile::NamedTempFile::new().expect("temp upload file");
        let valid = validate_browser_file_upload_params(&BrowserFileUploadParams {
            files: vec![temp.path().display().to_string()],
            selector: Some("#file".to_owned()),
            ..Default::default()
        })
        .expect("valid direct upload");
        assert_eq!(valid.operation, BrowserFileUploadOperation::SetFiles);
        assert_eq!(valid.selector.as_deref(), Some("#file"));
        assert_eq!(valid.files.len(), 1);

        let clear = validate_browser_file_upload_params(&BrowserFileUploadParams {
            operation: BrowserFileUploadOperation::Clear,
            active_element: true,
            ..Default::default()
        })
        .expect("valid clear");
        assert!(clear.files.is_empty());

        let arm = validate_browser_file_upload_params(&BrowserFileUploadParams {
            operation: BrowserFileUploadOperation::ArmChooser,
            limit: Some(MAX_CHOOSER_READ_LIMIT + 10),
            ..Default::default()
        })
        .expect("valid arm chooser");
        assert_eq!(arm.limit, MAX_CHOOSER_READ_LIMIT);

        for error in [
            validate_browser_file_upload_params(&BrowserFileUploadParams {
                files: vec![temp.path().display().to_string()],
                ..Default::default()
            })
            .expect_err("direct upload requires locator"),
            validate_browser_file_upload_params(&BrowserFileUploadParams {
                selector: Some("#file".to_owned()),
                element_id: Some("chrome-bridge-element:v1:1:0:0".to_owned()),
                files: vec![temp.path().display().to_string()],
                ..Default::default()
            })
            .expect_err("direct upload rejects multiple locators"),
            validate_browser_file_upload_params(&BrowserFileUploadParams {
                operation: BrowserFileUploadOperation::SetChooser,
                files: vec![r"C:\definitely\missing\synapse-upload.txt".to_owned()],
                ..Default::default()
            })
            .expect_err("missing file rejected"),
            validate_browser_file_upload_params(&BrowserFileUploadParams {
                operation: BrowserFileUploadOperation::ReadChooser,
                files: vec![temp.path().display().to_string()],
                ..Default::default()
            })
            .expect_err("read chooser rejects files"),
            validate_browser_file_upload_params(&BrowserFileUploadParams {
                operation: BrowserFileUploadOperation::ArmChooser,
                selector: Some("#file".to_owned()),
                ..Default::default()
            })
            .expect_err("chooser ops reject locator"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            assert!(
                matches!(
                    code,
                    Some(error_codes::TOOL_PARAMS_INVALID | error_codes::ACTION_TARGET_INVALID)
                ),
                "unexpected error code: {code:?}"
            );
        }
    }
}
