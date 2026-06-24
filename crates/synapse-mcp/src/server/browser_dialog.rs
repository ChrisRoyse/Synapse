//! JavaScript dialog MCP tool (#1099) backed by target-scoped CDP Page listeners.

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

const TOOL: &str = "browser_handle_dialog";
const DEFAULT_DIALOG_READ_LIMIT: usize = 20;
const MAX_DIALOG_READ_LIMIT: usize = 100;
const MAX_DIALOG_PROMPT_TEXT_CHARS: usize = 8192;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserHandleDialogOperation {
    /// Arm/read dialog capture and return pending/history state.
    #[default]
    Status,
    /// Accept the currently pending dialog.
    Accept,
    /// Dismiss the currently pending dialog.
    Dismiss,
    /// Set the default auto-policy for future dialogs.
    SetPolicy,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDialogDefaultPolicy {
    Accept,
    #[default]
    Dismiss,
    Manual,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserHandleDialogParams {
    /// CDP TargetID to inspect/handle. Defaults to the active session CDP target.
    /// Must be owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Operation to run. Defaults to status/readback.
    #[serde(default)]
    pub operation: BrowserHandleDialogOperation,
    /// Default policy for `status` arming or `set_policy`.
    #[serde(default)]
    pub default_policy: Option<BrowserDialogDefaultPolicy>,
    /// Prompt text for `accept`. Ignored by CDP for non-prompt dialogs.
    #[serde(default)]
    pub prompt_text: Option<String>,
    /// Return only dialog records with `seq >= since_seq`.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum dialog history entries to return. Defaults to 20, max 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrowserDialogEntry {
    pub seq: u64,
    pub url: String,
    pub frame_id: String,
    pub dialog_type: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<String>,
    pub has_browser_handler: bool,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
    pub default_policy: BrowserDialogDefaultPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_handled_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_handle_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_prompt_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_handled_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_handle_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_result: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_input: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserHandleDialogResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserHandleDialogOperation,
    pub default_policy: BrowserDialogDefaultPolicy,
    pub capture_newly_armed: bool,
    pub handled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_dialog: Option<BrowserDialogEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_dialog: Option<BrowserDialogEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dialog: Option<BrowserDialogEntry>,
    pub entries: Vec<BrowserDialogEntry>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub opened_count: u64,
    pub closed_count: u64,
    pub auto_handled_count: u64,
    pub error_count: u64,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug)]
struct NormalizedBrowserHandleDialogParams {
    operation: BrowserHandleDialogOperation,
    default_policy: Option<BrowserDialogDefaultPolicy>,
    prompt_text: Option<String>,
    since_seq: Option<u64>,
    limit: usize,
}

#[tool_router(router = browser_dialog_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Read and handle JavaScript dialogs for the calling session's owned browser tab. Arms a target-scoped Page.javascriptDialogOpening/Page.javascriptDialogClosed listener over raw CDP or the normal Chrome bridge's narrow chrome.debugger lane, returns pending dialog message/history, accepts or dismisses the pending dialog with optional prompt text, and sets the default policy (accept/dismiss/manual) for future dialogs. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_handle_dialog(
        &self,
        params: Parameters<BrowserHandleDialogParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserHandleDialogResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_handle_dialog"
        );
        let session_id = require_target_session_id(&request_context)?;
        let dialog = validate_browser_handle_dialog_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": dialog.operation,
            "default_policy": dialog.default_policy,
            "prompt_text_len": dialog.prompt_text.as_deref().map(str::len),
            "since_seq": dialog.since_seq,
            "limit": dialog.limit,
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
            "operation": dialog.operation,
            "default_policy": dialog.default_policy,
            "prompt_text_len": dialog.prompt_text.as_deref().map(str::len),
            "since_seq": dialog.since_seq,
            "limit": dialog.limit,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_handle_dialog_impl(&session_id, window_hwnd, &cdp_target_id, &dialog)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_handle_dialog_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        dialog: &NormalizedBrowserHandleDialogParams,
    ) -> Result<BrowserHandleDialogResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let operation = browser_dialog_operation_name(dialog.operation);
                let default_policy = dialog
                    .default_policy
                    .map(browser_dialog_default_policy_name);
                let result = crate::chrome_debugger_bridge::handle_dialog(
                    window_hwnd,
                    cdp_target_id,
                    operation,
                    default_policy,
                    dialog.prompt_text.as_deref(),
                    dialog.since_seq,
                    dialog.limit,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_handle_dialog normal Chrome bridge Page.javascriptDialogOpening/Page.handleJavaScriptDialog failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                let endpoint = result
                    .extension_id
                    .as_deref()
                    .map(chrome_debugger_endpoint)
                    .unwrap_or_else(chrome_debugger_default_endpoint);
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_DIALOG_READBACK",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    endpoint = %endpoint,
                    cdp_target_id = %result.target_id,
                    operation = ?dialog.operation,
                    handled = result.handled,
                    returned = result.returned,
                    total_buffered = result.total_buffered,
                    "readback=chrome.debugger.Page.javascriptDialogOpening+Page.handleJavaScriptDialog outcome=dialog_status"
                );
                return Ok(browser_handle_dialog_bridge_response(
                    session_id,
                    window_hwnd,
                    endpoint,
                    dialog.operation,
                    result,
                ));
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let read_filter = synapse_a11y::CdpDialogReadFilter {
            since_seq: dialog.since_seq,
            max: dialog.limit,
        };
        let (status, handled) = match dialog.operation {
            BrowserHandleDialogOperation::Status => {
                let policy = dialog.default_policy.unwrap_or_default();
                let status = synapse_a11y::dialog_capture_ensure(
                    &endpoint,
                    cdp_target_id,
                    policy.into(),
                    synapse_a11y::DEFAULT_DIALOG_BUFFER_CAPACITY,
                )
                .await
                .map_err(|error| dialog_mcp_error("arm/read", error))?;
                (Some(status), None)
            }
            BrowserHandleDialogOperation::SetPolicy => {
                let policy = dialog.default_policy.ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_handle_dialog set_policy requires default_policy but it was None"
                            .to_string(),
                    )
                })?;
                let status = synapse_a11y::dialog_capture_ensure(
                    &endpoint,
                    cdp_target_id,
                    policy.into(),
                    synapse_a11y::DEFAULT_DIALOG_BUFFER_CAPACITY,
                )
                .await
                .map_err(|error| dialog_mcp_error("set policy", error))?;
                (Some(status), None)
            }
            BrowserHandleDialogOperation::Accept => {
                let handled = synapse_a11y::dialog_handle_pending(
                    cdp_target_id,
                    synapse_a11y::CdpDialogHandleAction::Accept,
                    dialog.prompt_text.clone(),
                )
                .await
                .map_err(|error| dialog_mcp_error("accept", error))?;
                (
                    synapse_a11y::dialog_capture_status(cdp_target_id),
                    Some(handled),
                )
            }
            BrowserHandleDialogOperation::Dismiss => {
                let handled = synapse_a11y::dialog_handle_pending(
                    cdp_target_id,
                    synapse_a11y::CdpDialogHandleAction::Dismiss,
                    None,
                )
                .await
                .map_err(|error| dialog_mcp_error("dismiss", error))?;
                (
                    synapse_a11y::dialog_capture_status(cdp_target_id),
                    Some(handled),
                )
            }
        };
        let read = synapse_a11y::dialog_capture_read(cdp_target_id, &read_filter);
        tracing::info!(
            code = "CDP_BACKGROUND_DIALOG_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?dialog.operation,
            handled = handled.is_some(),
            "readback=Page.javascriptDialogOpening+Page.handleJavaScriptDialog outcome=dialog_status"
        );
        Ok(browser_handle_dialog_response(
            session_id,
            window_hwnd,
            endpoint,
            cdp_target_id,
            dialog.operation,
            status,
            read,
            handled,
        ))
    }

    #[cfg(not(windows))]
    async fn browser_handle_dialog_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _dialog: &NormalizedBrowserHandleDialogParams,
    ) -> Result<BrowserHandleDialogResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_handle_dialog is only available on Windows in this build",
        ))
    }
}

fn validate_browser_handle_dialog_params(
    params: &BrowserHandleDialogParams,
) -> Result<NormalizedBrowserHandleDialogParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let limit = params
        .limit
        .unwrap_or(DEFAULT_DIALOG_READ_LIMIT)
        .clamp(1, MAX_DIALOG_READ_LIMIT);
    if let Some(prompt_text) = params.prompt_text.as_deref() {
        validate_prompt_text(prompt_text)?;
    }
    match params.operation {
        BrowserHandleDialogOperation::SetPolicy if params.default_policy.is_none() => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{TOOL} operation=set_policy requires default_policy"),
            ));
        }
        BrowserHandleDialogOperation::Accept => {}
        BrowserHandleDialogOperation::Dismiss => {
            if params.prompt_text.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{TOOL} prompt_text is only valid for operation=accept"),
                ));
            }
        }
        BrowserHandleDialogOperation::Status | BrowserHandleDialogOperation::SetPolicy => {
            if params.prompt_text.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{TOOL} prompt_text is only valid for operation=accept"),
                ));
            }
        }
    }
    if matches!(
        params.operation,
        BrowserHandleDialogOperation::Accept | BrowserHandleDialogOperation::Dismiss
    ) && params.default_policy.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} default_policy is only valid for operation=status or set_policy"),
        ));
    }
    Ok(NormalizedBrowserHandleDialogParams {
        operation: params.operation,
        default_policy: params.default_policy,
        prompt_text: params.prompt_text.clone(),
        since_seq: params.since_seq,
        limit,
    })
}

fn validate_prompt_text(prompt_text: &str) -> Result<(), ErrorData> {
    if prompt_text.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} prompt_text must not contain NUL"),
        ));
    }
    if prompt_text.chars().count() > MAX_DIALOG_PROMPT_TEXT_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} prompt_text must be at most {MAX_DIALOG_PROMPT_TEXT_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(())
}

fn dialog_mcp_error(phase: &str, error: synapse_a11y::A11yError) -> ErrorData {
    mcp_error(
        error.code(),
        format!("{TOOL} raw CDP dialog {phase} failed: {error}"),
    )
}

fn browser_dialog_operation_name(operation: BrowserHandleDialogOperation) -> &'static str {
    match operation {
        BrowserHandleDialogOperation::Status => "status",
        BrowserHandleDialogOperation::Accept => "accept",
        BrowserHandleDialogOperation::Dismiss => "dismiss",
        BrowserHandleDialogOperation::SetPolicy => "set_policy",
    }
}

fn browser_dialog_default_policy_name(policy: BrowserDialogDefaultPolicy) -> &'static str {
    match policy {
        BrowserDialogDefaultPolicy::Accept => "accept",
        BrowserDialogDefaultPolicy::Dismiss => "dismiss",
        BrowserDialogDefaultPolicy::Manual => "manual",
    }
}

fn browser_dialog_operation_from_wire(
    value: &str,
    fallback: BrowserHandleDialogOperation,
) -> BrowserHandleDialogOperation {
    match value {
        "status" => BrowserHandleDialogOperation::Status,
        "accept" => BrowserHandleDialogOperation::Accept,
        "dismiss" => BrowserHandleDialogOperation::Dismiss,
        "set_policy" => BrowserHandleDialogOperation::SetPolicy,
        _ => fallback,
    }
}

fn browser_dialog_default_policy_from_wire(value: &str) -> BrowserDialogDefaultPolicy {
    match value {
        "accept" => BrowserDialogDefaultPolicy::Accept,
        "manual" => BrowserDialogDefaultPolicy::Manual,
        _ => BrowserDialogDefaultPolicy::Dismiss,
    }
}

fn browser_handle_dialog_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    endpoint: String,
    requested_operation: BrowserHandleDialogOperation,
    result: crate::chrome_debugger_bridge::ChromeDebuggerHandleDialogResult,
) -> BrowserHandleDialogResponse {
    BrowserHandleDialogResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint,
        cdp_target_id: result.target_id,
        operation: browser_dialog_operation_from_wire(&result.operation, requested_operation),
        default_policy: browser_dialog_default_policy_from_wire(&result.default_policy),
        capture_newly_armed: result.capture_newly_armed,
        handled: result.handled,
        handle_action: result.handle_action,
        prompt_text: result.prompt_text,
        pending_dialog: result.pending_dialog.map(browser_dialog_entry_from_bridge),
        handled_dialog: result.handled_dialog.map(browser_dialog_entry_from_bridge),
        last_dialog: result.last_dialog.map(browser_dialog_entry_from_bridge),
        entries: result
            .entries
            .into_iter()
            .map(browser_dialog_entry_from_bridge)
            .collect(),
        next_cursor: result.next_cursor,
        returned: result.returned,
        total_buffered: result.total_buffered,
        dropped: result.dropped,
        opened_count: result.opened_count,
        closed_count: result.closed_count,
        auto_handled_count: result.auto_handled_count,
        error_count: result.error_count,
        readback_backend: if result.readback_backend.trim().is_empty() {
            "chrome.debugger.Page.javascriptDialogOpening+Page.handleJavaScriptDialog".to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.trim().is_empty() {
            "chrome_tabs_extension".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
    }
}

fn browser_dialog_entry_from_bridge(
    entry: crate::chrome_debugger_bridge::ChromeDebuggerDialogEntry,
) -> BrowserDialogEntry {
    BrowserDialogEntry {
        seq: entry.seq,
        url: entry.url,
        frame_id: entry.frame_id,
        dialog_type: entry.dialog_type,
        message: entry.message,
        default_prompt: entry.default_prompt,
        has_browser_handler: entry.has_browser_handler,
        opened_at_unix_ms: entry.opened_at_unix_ms,
        pending: entry.pending,
        default_policy: browser_dialog_default_policy_from_wire(&entry.default_policy),
        auto_action: entry.auto_action,
        auto_handled_at_unix_ms: entry.auto_handled_at_unix_ms,
        auto_handle_error: entry.auto_handle_error,
        manual_action: entry.manual_action,
        manual_prompt_text: entry.manual_prompt_text,
        manual_handled_at_unix_ms: entry.manual_handled_at_unix_ms,
        manual_handle_error: entry.manual_handle_error,
        closed_at_unix_ms: entry.closed_at_unix_ms,
        close_result: entry.close_result,
        user_input: entry.user_input,
    }
}

fn browser_handle_dialog_response(
    session_id: &str,
    window_hwnd: i64,
    endpoint: String,
    cdp_target_id: &str,
    operation: BrowserHandleDialogOperation,
    status: Option<synapse_a11y::CdpDialogCaptureStatus>,
    read: Option<synapse_a11y::CdpDialogReadResult>,
    handled: Option<synapse_a11y::CdpDialogHandleResult>,
) -> BrowserHandleDialogResponse {
    let default_policy = status
        .as_ref()
        .map(|status| BrowserDialogDefaultPolicy::from(status.default_policy))
        .or_else(|| {
            read.as_ref()
                .map(|read| BrowserDialogDefaultPolicy::from(read.default_policy))
        })
        .unwrap_or_default();
    let entries: Vec<BrowserDialogEntry> = read
        .as_ref()
        .map(|read| read.entries.iter().map(browser_dialog_entry).collect())
        .unwrap_or_default();
    let handled_dialog = handled
        .as_ref()
        .map(|handled| browser_dialog_entry(&handled.dialog));
    BrowserHandleDialogResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint,
        cdp_target_id: cdp_target_id.to_owned(),
        operation,
        default_policy,
        capture_newly_armed: status.as_ref().is_some_and(|status| status.newly_armed),
        handled: handled.is_some(),
        handle_action: handled
            .as_ref()
            .map(|handled| dialog_handle_action_wire(handled.action)),
        prompt_text: handled.and_then(|handled| handled.prompt_text),
        pending_dialog: read
            .as_ref()
            .and_then(|read| read.pending_dialog.as_ref())
            .map(browser_dialog_entry)
            .or_else(|| {
                status
                    .as_ref()
                    .and_then(|status| status.pending_dialog.as_ref())
                    .map(browser_dialog_entry)
            }),
        handled_dialog,
        last_dialog: status
            .as_ref()
            .and_then(|status| status.last_dialog.as_ref())
            .map(browser_dialog_entry),
        entries,
        next_cursor: read.as_ref().map_or(0, |read| read.next_cursor),
        returned: read.as_ref().map_or(0, |read| read.returned),
        total_buffered: read.as_ref().map_or(0, |read| read.total_buffered),
        dropped: read.as_ref().map_or(0, |read| read.dropped),
        opened_count: status.as_ref().map_or(0, |status| status.opened_count),
        closed_count: status.as_ref().map_or(0, |status| status.closed_count),
        auto_handled_count: status
            .as_ref()
            .map_or(0, |status| status.auto_handled_count),
        error_count: status.as_ref().map_or(0, |status| status.error_count),
        readback_backend: "Page.javascriptDialogOpening + Page.handleJavaScriptDialog".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
    }
}

fn browser_dialog_entry(entry: &synapse_a11y::CdpDialogEntry) -> BrowserDialogEntry {
    BrowserDialogEntry {
        seq: entry.seq,
        url: entry.url.clone(),
        frame_id: entry.frame_id.clone(),
        dialog_type: entry.dialog_type.clone(),
        message: entry.message.clone(),
        default_prompt: entry.default_prompt.clone(),
        has_browser_handler: entry.has_browser_handler,
        opened_at_unix_ms: entry.opened_at_unix_ms,
        pending: entry.pending,
        default_policy: BrowserDialogDefaultPolicy::from(entry.default_policy),
        auto_action: entry.auto_action.map(dialog_auto_action_wire),
        auto_handled_at_unix_ms: entry.auto_handled_at_unix_ms,
        auto_handle_error: entry.auto_handle_error.clone(),
        manual_action: entry.manual_action.map(dialog_handle_action_wire),
        manual_prompt_text: entry.manual_prompt_text.clone(),
        manual_handled_at_unix_ms: entry.manual_handled_at_unix_ms,
        manual_handle_error: entry.manual_handle_error.clone(),
        closed_at_unix_ms: entry.closed_at_unix_ms,
        close_result: entry.close_result,
        user_input: entry.user_input.clone(),
    }
}

fn dialog_auto_action_wire(action: synapse_a11y::CdpDialogAutoAction) -> String {
    match action {
        synapse_a11y::CdpDialogAutoAction::Accepted => "accepted",
        synapse_a11y::CdpDialogAutoAction::Dismissed => "dismissed",
    }
    .to_owned()
}

fn dialog_handle_action_wire(action: synapse_a11y::CdpDialogHandleAction) -> String {
    match action {
        synapse_a11y::CdpDialogHandleAction::Accept => "accept",
        synapse_a11y::CdpDialogHandleAction::Dismiss => "dismiss",
    }
    .to_owned()
}

impl From<BrowserDialogDefaultPolicy> for synapse_a11y::CdpDialogDefaultPolicy {
    fn from(value: BrowserDialogDefaultPolicy) -> Self {
        match value {
            BrowserDialogDefaultPolicy::Accept => Self::Accept,
            BrowserDialogDefaultPolicy::Dismiss => Self::Dismiss,
            BrowserDialogDefaultPolicy::Manual => Self::Manual,
        }
    }
}

impl From<synapse_a11y::CdpDialogDefaultPolicy> for BrowserDialogDefaultPolicy {
    fn from(value: synapse_a11y::CdpDialogDefaultPolicy) -> Self {
        match value {
            synapse_a11y::CdpDialogDefaultPolicy::Accept => Self::Accept,
            synapse_a11y::CdpDialogDefaultPolicy::Dismiss => Self::Dismiss,
            synapse_a11y::CdpDialogDefaultPolicy::Manual => Self::Manual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_handle_dialog_validation_edges() {
        let normalized = validate_browser_handle_dialog_params(&BrowserHandleDialogParams {
            operation: BrowserHandleDialogOperation::Status,
            default_policy: Some(BrowserDialogDefaultPolicy::Manual),
            limit: Some(MAX_DIALOG_READ_LIMIT + 100),
            ..Default::default()
        })
        .expect("status params");
        assert_eq!(normalized.limit, MAX_DIALOG_READ_LIMIT);
        assert_eq!(
            normalized.default_policy,
            Some(BrowserDialogDefaultPolicy::Manual)
        );

        for error in [
            validate_browser_handle_dialog_params(&BrowserHandleDialogParams {
                operation: BrowserHandleDialogOperation::SetPolicy,
                ..Default::default()
            })
            .expect_err("set_policy requires default_policy"),
            validate_browser_handle_dialog_params(&BrowserHandleDialogParams {
                operation: BrowserHandleDialogOperation::Dismiss,
                prompt_text: Some("ignored".to_owned()),
                ..Default::default()
            })
            .expect_err("dismiss rejects prompt_text"),
            validate_browser_handle_dialog_params(&BrowserHandleDialogParams {
                operation: BrowserHandleDialogOperation::Accept,
                default_policy: Some(BrowserDialogDefaultPolicy::Accept),
                ..Default::default()
            })
            .expect_err("accept rejects default_policy"),
            validate_browser_handle_dialog_params(&BrowserHandleDialogParams {
                operation: BrowserHandleDialogOperation::Accept,
                prompt_text: Some("bad\0text".to_owned()),
                ..Default::default()
            })
            .expect_err("prompt text rejects nul"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
        }
    }

    #[test]
    fn browser_handle_dialog_response_maps_pending_and_handled_state() {
        let pending = synapse_a11y::CdpDialogEntry {
            seq: 7,
            url: "https://example.test".to_owned(),
            frame_id: "frame-1".to_owned(),
            dialog_type: "prompt".to_owned(),
            message: "Name?".to_owned(),
            default_prompt: Some("default".to_owned()),
            has_browser_handler: true,
            opened_at_unix_ms: 10,
            pending: true,
            default_policy: synapse_a11y::CdpDialogDefaultPolicy::Manual,
            auto_action: None,
            auto_handled_at_unix_ms: None,
            auto_handle_error: None,
            manual_action: None,
            manual_prompt_text: None,
            manual_handled_at_unix_ms: None,
            manual_handle_error: None,
            closed_at_unix_ms: None,
            close_result: None,
            user_input: None,
        };
        let mut handled_entry = pending.clone();
        handled_entry.pending = false;
        handled_entry.manual_action = Some(synapse_a11y::CdpDialogHandleAction::Accept);
        handled_entry.manual_prompt_text = Some("Ada".to_owned());
        handled_entry.manual_handled_at_unix_ms = Some(20);
        handled_entry.closed_at_unix_ms = Some(20);
        handled_entry.close_result = Some(true);
        handled_entry.user_input = Some("Ada".to_owned());

        let response = browser_handle_dialog_response(
            "session-1",
            100,
            "http://127.0.0.1:9222".to_owned(),
            "target-1",
            BrowserHandleDialogOperation::Accept,
            Some(synapse_a11y::CdpDialogCaptureStatus {
                newly_armed: false,
                endpoint: "http://127.0.0.1:9222".to_owned(),
                cdp_target_id: "target-1".to_owned(),
                armed_at_unix_ms: 1,
                capacity: 128,
                default_policy: synapse_a11y::CdpDialogDefaultPolicy::Manual,
                pending_dialog: None,
                last_dialog: Some(handled_entry.clone()),
                opened_count: 1,
                closed_count: 1,
                auto_handled_count: 0,
                error_count: 0,
            }),
            Some(synapse_a11y::CdpDialogReadResult {
                entries: vec![handled_entry.clone()],
                pending_dialog: None,
                next_cursor: 8,
                returned: 1,
                total_buffered: 1,
                dropped: 0,
                armed_at_unix_ms: 1,
                default_policy: synapse_a11y::CdpDialogDefaultPolicy::Manual,
            }),
            Some(synapse_a11y::CdpDialogHandleResult {
                cdp_target_id: "target-1".to_owned(),
                action: synapse_a11y::CdpDialogHandleAction::Accept,
                prompt_text: Some("Ada".to_owned()),
                handled_at_unix_ms: 20,
                dialog: handled_entry,
            }),
        );

        assert!(response.handled);
        assert_eq!(response.handle_action.as_deref(), Some("accept"));
        assert_eq!(response.prompt_text.as_deref(), Some("Ada"));
        assert_eq!(response.default_policy, BrowserDialogDefaultPolicy::Manual);
        assert_eq!(response.returned, 1);
        assert_eq!(response.next_cursor, 8);
        assert_eq!(response.entries[0].manual_action.as_deref(), Some("accept"));
        assert_eq!(response.entries[0].user_input.as_deref(), Some("Ada"));
    }
}
