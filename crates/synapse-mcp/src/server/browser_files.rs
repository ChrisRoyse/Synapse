//! File upload MCP tool (#1101-#1105) backed by dedicated-profile raw CDP.
//!
//! The raw-CDP path uses `DOM.setFileInputFiles` for direct input assignment and
//! `Page.setInterceptFileChooserDialog`/`Page.fileChooserOpened` for chooser
//! interception. It never opens an OS file picker and never activates Chrome.

use std::path::{Path, PathBuf};

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{cdp_target_id_audit_ref, require_target_session_id, validate_cdp_target_id},
    tool, tool_router,
};
use crate::m1::mcp_error;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_core::error_codes;

const TOOL: &str = "browser_file_upload";
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
    /// Raw-CDP element id for direct `set_files`/`clear`.
    #[serde(default)]
    pub element_id: Option<String>,
    /// Target the tab's current `document.activeElement` for direct
    /// `set_files`/`clear`.
    #[serde(default)]
    pub active_element: bool,
    /// Raw CDP TargetID to mutate. Defaults to the active session CDP target.
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
        description = "Set or clear input[type=file] files and intercept file chooser openings in the calling session's owned raw-CDP tab on Synapse's dedicated non-default automation profile. Direct operations use DOM.setFileInputFiles by strict selector, raw-CDP element_id, or active_element; chooser operations arm Page.setInterceptFileChooserDialog so clicking an input records Page.fileChooserOpened without opening the OS picker, then set_chooser assigns files to the pending backend node. Paths are validated locally before Chrome is called. The debugger-free normal authenticated Chrome bridge fails closed before any Chrome command. Background-safe: never activates Chrome, never uses OS foreground input, and never falls back to the human foreground tab."
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
        if cdp_target_id.starts_with("chrome-tab:") {
            return Err(mcp_error(
                error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED,
                format!(
                    "{TOOL} refused normal authenticated Chrome target {cdp_target_id:?} before queueing any Chrome command; the normal profile permanently forbids debugger permission. Launch a session-owned raw-CDP browser on Synapse's dedicated non-default automation profile"
                ),
            ));
        }
        let endpoint = synapse_a11y::endpoint_for_window(window_hwnd).ok_or_else(|| {
            mcp_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "{TOOL} requires a reachable raw-CDP endpoint for window {window_hwnd:#x}; launch the browser with act_launch"
                ),
            )
        })?;
        if upload.operation != BrowserFileUploadOperation::ReadChooser {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_file_upload_before_raw_cdp_mutation",
            )?;
        }
        let mut input = None;
        let mut handled_chooser = None;
        let mut canceled_chooser = None;
        let status = match upload.operation {
            BrowserFileUploadOperation::SetFiles | BrowserFileUploadOperation::Clear => {
                let (backend_node_id, resolved_by, match_count) =
                    resolve_raw_file_input(&endpoint, cdp_target_id, upload).await?;
                let files = if upload.operation == BrowserFileUploadOperation::Clear {
                    &[]
                } else {
                    upload.files.as_slice()
                };
                input = Some(
                    synapse_a11y::cdp_set_file_input_files_target(
                        &endpoint,
                        cdp_target_id,
                        backend_node_id,
                        files,
                        resolved_by,
                        match_count,
                    )
                    .await
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!("{TOOL} raw CDP DOM.setFileInputFiles failed: {error}"),
                        )
                    })?,
                );
                empty_raw_file_chooser_status(cdp_target_id)
            }
            BrowserFileUploadOperation::ArmChooser => synapse_a11y::cdp_file_chooser_ensure(
                &endpoint,
                cdp_target_id,
                synapse_a11y::DEFAULT_FILE_CHOOSER_CAPACITY,
            )
            .await
            .map_err(|error| {
                mcp_error(error.code(), format!("{TOOL} arm chooser failed: {error}"))
            })?,
            BrowserFileUploadOperation::ReadChooser => synapse_a11y::cdp_file_chooser_read(
                &endpoint,
                cdp_target_id,
                upload.since_seq,
                upload.limit,
            )
            .map_err(|error| {
                mcp_error(error.code(), format!("{TOOL} read chooser failed: {error}"))
            })?,
            BrowserFileUploadOperation::SetChooser => {
                handled_chooser = Some(
                    synapse_a11y::cdp_file_chooser_set_pending(
                        &endpoint,
                        cdp_target_id,
                        &upload.files,
                    )
                    .await
                    .map_err(|error| {
                        mcp_error(error.code(), format!("{TOOL} set chooser failed: {error}"))
                    })?,
                );
                synapse_a11y::cdp_file_chooser_read(
                    &endpoint,
                    cdp_target_id,
                    upload.since_seq,
                    upload.limit,
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{TOOL} post-set chooser read failed: {error}"),
                    )
                })?
            }
            BrowserFileUploadOperation::CancelChooser => {
                canceled_chooser = Some(
                    synapse_a11y::cdp_file_chooser_cancel_pending(&endpoint, cdp_target_id)
                        .await
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{TOOL} cancel chooser failed: {error}"),
                            )
                        })?,
                );
                synapse_a11y::cdp_file_chooser_read(
                    &endpoint,
                    cdp_target_id,
                    upload.since_seq,
                    upload.limit,
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{TOOL} post-cancel chooser read failed: {error}"),
                    )
                })?
            }
        };
        if upload.operation != BrowserFileUploadOperation::ReadChooser {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_file_upload_after_raw_cdp_mutation",
            )?;
        }
        tracing::info!(
            code = "RAW_CDP_FILE_UPLOAD_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = %browser_file_upload_operation_name(upload.operation),
            requested_file_count = upload.files.len(),
            opened_count = status.opened_count,
            handled_count = status.handled_count,
            "readback=raw CDP DOM.setFileInputFiles+Page.fileChooserOpened outcome=file_upload_status"
        );
        Ok(raw_file_upload_response(
            session_id,
            window_hwnd,
            endpoint,
            upload.operation,
            upload.files.len(),
            input,
            handled_chooser,
            canceled_chooser,
            status,
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

#[cfg(windows)]
async fn resolve_raw_file_input<'a>(
    endpoint: &str,
    cdp_target_id: &str,
    upload: &'a NormalizedBrowserFileUploadParams,
) -> Result<(i64, &'a str, u32), ErrorData> {
    if let Some(selector) = upload.selector.as_deref() {
        let located = synapse_a11y::cdp_locate(
            endpoint,
            cdp_target_id,
            synapse_a11y::CdpLocateRequest {
                engine: synapse_a11y::CdpLocateEngine::Css,
                query: selector.to_owned(),
                strict: true,
                limit: 2,
                ..synapse_a11y::CdpLocateRequest::default()
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{TOOL} selector resolution failed: {error}"),
            )
        })?;
        let backend = located.backend_node_ids.first().copied().ok_or_else(|| {
            mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("{TOOL} selector {selector:?} matched no elements"),
            )
        })?;
        let count = u32::try_from(located.match_count).map_err(|_| {
            mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!("{TOOL} selector match count exceeded u32"),
            )
        })?;
        return Ok((backend, "selector", count));
    }
    if let Some(element_id) = upload.element_id.as_deref() {
        let parsed = synapse_core::ElementId::parse(element_id).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{TOOL} element_id {element_id:?} is invalid: {error}"),
            )
        })?;
        let backend = synapse_a11y::cdp_backend_from_element_id(&parsed).ok_or_else(|| {
            mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("{TOOL} element_id {element_id:?} is not a raw-CDP element"),
            )
        })?;
        let target = synapse_a11y::cdp_target_from_element_id(&parsed).ok_or_else(|| {
            mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!("{TOOL} element_id {element_id:?} has no CDP target"),
            )
        })?;
        if !target.eq_ignore_ascii_case(cdp_target_id) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{TOOL} element_id target {target:?} does not match requested target {cdp_target_id:?}"
                ),
            ));
        }
        return Ok((backend, "element_id", 1));
    }
    if upload.active_element {
        let backend = synapse_a11y::cdp_active_element_backend_node(endpoint, cdp_target_id)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("{TOOL} active-element resolution failed: {error}"),
                )
            })?;
        return Ok((backend, "active_element", 1));
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("{TOOL} direct operation has no locator"),
    ))
}

#[cfg(windows)]
fn empty_raw_file_chooser_status(target_id: &str) -> synapse_a11y::CdpFileChooserStatus {
    synapse_a11y::CdpFileChooserStatus {
        newly_armed: false,
        target_id: target_id.to_owned(),
        armed_at_unix_ms: 0,
        pending_chooser: None,
        entries: Vec::new(),
        next_cursor: 0,
        returned: 0,
        total_buffered: 0,
        dropped: 0,
        opened_count: 0,
        handled_count: 0,
        canceled_count: 0,
        error_count: 0,
    }
}

#[cfg(windows)]
#[expect(
    clippy::too_many_arguments,
    reason = "maps the independently-produced raw-CDP mutation and chooser readbacks into the public response"
)]
fn raw_file_upload_response(
    session_id: &str,
    window_hwnd: i64,
    endpoint: String,
    operation: BrowserFileUploadOperation,
    requested_file_count: usize,
    input: Option<synapse_a11y::CdpFileInputState>,
    handled_chooser: Option<synapse_a11y::CdpFileChooserRecord>,
    canceled_chooser: Option<synapse_a11y::CdpFileChooserRecord>,
    status: synapse_a11y::CdpFileChooserStatus,
) -> BrowserFileUploadResponse {
    BrowserFileUploadResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint,
        cdp_target_id: status.target_id,
        operation,
        capture_newly_armed: status.newly_armed,
        requested_file_count,
        input: input.map(file_upload_input_from_raw),
        handled_chooser: handled_chooser.map(file_chooser_entry_from_raw),
        canceled_chooser: canceled_chooser.map(file_chooser_entry_from_raw),
        pending_chooser: status.pending_chooser.map(file_chooser_entry_from_raw),
        entries: status
            .entries
            .into_iter()
            .map(file_chooser_entry_from_raw)
            .collect(),
        next_cursor: status.next_cursor,
        returned: status.returned,
        total_buffered: status.total_buffered,
        dropped: status.dropped,
        opened_count: status.opened_count,
        handled_count: status.handled_count,
        canceled_count: status.canceled_count,
        error_count: status.error_count,
        readback_backend:
            "raw CDP DOM.setFileInputFiles + Runtime.callFunctionOn HTMLInputElement.files"
                .to_owned(),
        chooser_readback_backend:
            "raw CDP Page.setInterceptFileChooserDialog + Page.fileChooserOpened".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
    }
}

#[cfg(windows)]
fn file_upload_input_from_raw(input: synapse_a11y::CdpFileInputState) -> BrowserFileUploadInput {
    BrowserFileUploadInput {
        resolved_by: input.resolved_by,
        match_count: input.match_count,
        frame_id: None,
        element_path: None,
        backend_node_id: Some(input.backend_node_id),
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
                file_type: file.file_type,
                last_modified: file.last_modified,
            })
            .collect(),
        value: input.value,
    }
}

#[cfg(windows)]
fn file_chooser_entry_from_raw(
    entry: synapse_a11y::CdpFileChooserRecord,
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
        input: entry.input.map(file_upload_input_from_raw),
        error: entry.error,
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
