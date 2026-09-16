//! Browser clock control and page lifecycle/worker event tools (#1201).

use super::{
    ErrorData, Json, Parameters, SynapseService, TargetWire,
    m1_tools::{
        browser_raw_cdp_required_error, cdp_target_id_audit_ref, require_target_session_id,
        validate_cdp_target_id,
    },
    tool, tool_router,
};
use crate::m1::mcp_error;
use crate::server::url_redaction::{
    redact_url_for_public_readback, redact_url_opt_for_public_readback,
};
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_core::error_codes;

const CLOCK_TOOL: &str = "browser_clock";
const EVENTS_TOOL: &str = "browser_page_events";
const MAX_CLOCK_MS: u64 = 8_640_000_000_000_000;
const DEFAULT_PAGE_EVENT_LIMIT: usize = 100;
const MAX_PAGE_EVENT_LIMIT: usize = 1000;
const MAX_PAGE_EVENT_TOKEN_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserClockOperation {
    /// Read the installed clock state from the current page.
    #[default]
    Status,
    /// Install the fake clock shim into the current and future page documents.
    Install,
    /// Set Date/time to `time_unix_ms` without firing timers.
    SetFixedTime,
    /// Advance fake time by `delta_ms`, firing due timers/intervals/RAF callbacks.
    FastForward,
    /// Advance to `time_unix_ms`, firing due timers, then keep time frozen there.
    PauseAt,
}

impl BrowserClockOperation {
    fn wire(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Install => "install",
            Self::SetFixedTime => "setFixedTime",
            Self::FastForward => "fastForward",
            Self::PauseAt => "pauseAt",
        }
    }
}

impl From<BrowserClockOperation> for synapse_a11y::CdpClockOperation {
    fn from(value: BrowserClockOperation) -> Self {
        match value {
            BrowserClockOperation::Status => Self::Status,
            BrowserClockOperation::Install => Self::Install,
            BrowserClockOperation::SetFixedTime => Self::SetFixedTime,
            BrowserClockOperation::FastForward => Self::FastForward,
            BrowserClockOperation::PauseAt => Self::PauseAt,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserClockParams {
    /// CDP TargetID to control. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Clock operation. Defaults to status/readback.
    #[serde(default)]
    pub operation: BrowserClockOperation,
    /// Unix epoch milliseconds for install, set_fixed_time, and pause_at.
    #[serde(default)]
    pub time_unix_ms: Option<u64>,
    /// Virtual milliseconds to advance for fast_forward.
    #[serde(default)]
    pub delta_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserClockReadback {
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_timer_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired_timer_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_timer_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_timer_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserClockResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserClockOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_script_identifier: Option<String>,
    pub init_script_newly_added: bool,
    pub installed_at_unix_ms: u64,
    pub clock: BrowserClockReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug)]
struct NormalizedBrowserClockParams {
    operation: BrowserClockOperation,
    time_unix_ms: Option<u64>,
    delta_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPageEventsParams {
    /// CDP TargetID to read. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Return only records with `seq >= since_seq`.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum records to return. Defaults to 100, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional exact event kind filter.
    #[serde(default)]
    pub event_kind: Option<String>,
    /// Optional exact worker type filter: worker, service_worker, or shared_worker.
    #[serde(default)]
    pub worker_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPageEventsFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPageEventEntry {
    pub seq: u64,
    pub event_kind: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoptable_target: Option<TargetWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_access_opener: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_s: Option<f64>,
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPageTargetSnapshot {
    pub cdp_target_id: String,
    pub target: TargetWire,
    pub target_type: String,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_frame_id: Option<String>,
    pub can_access_opener: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    pub attached: bool,
    pub destroyed: bool,
    pub adoptable: bool,
    pub first_seen_seq: u64,
    pub last_seen_seq: u64,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserWorkerSnapshot {
    pub worker_id: String,
    pub worker_type: String,
    pub url: String,
    pub title: String,
    pub attached: bool,
    pub destroyed: bool,
    pub first_seen_seq: u64,
    pub last_seen_seq: u64,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserPageEventsResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub capture_newly_armed: bool,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub filters: BrowserPageEventsFilters,
    pub entries: Vec<BrowserPageEventEntry>,
    pub pages: Vec<BrowserPageTargetSnapshot>,
    pub workers: Vec<BrowserWorkerSnapshot>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug)]
struct NormalizedBrowserPageEventsParams {
    since_seq: Option<u64>,
    limit: usize,
    event_kind: Option<String>,
    worker_type: Option<String>,
}

#[tool_router(router = browser_clock_events_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Control a fake Playwright-style page clock in the calling session's owned browser tab. Raw CDP injects a target-scoped init-script shim for current/future documents; the normal Chrome bridge uses a typed MAIN-world chrome.scripting shim for chrome-tab:* current-document targets. set_fixed_time changes Date without firing timers; fast_forward advances virtual time and fires due timers; pause_at advances to an epoch-ms timestamp and freezes there; status returns current shim state. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_clock(
        &self,
        params: Parameters<BrowserClockParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserClockResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = CLOCK_TOOL,
            "tool.invocation kind=browser_clock"
        );
        let session_id = require_target_session_id(&request_context)?;
        let clock = validate_browser_clock_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": clock.operation,
            "time_unix_ms": clock.time_unix_ms,
            "delta_ms": clock.delta_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            CLOCK_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            CLOCK_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": clock.operation,
            "time_unix_ms": clock.time_unix_ms,
            "delta_ms": clock.delta_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            CLOCK_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_clock_impl(&session_id, window_hwnd, &cdp_target_id, &clock)
            .await;
        self.audit_action_result_for_session(CLOCK_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Arm and read target-scoped page lifecycle, popup/new-page, and worker events for the calling session's owned browser tab. Raw CDP returns Page.domContentEventFired, Page.loadEventFired, Page.lifecycleEvent, Page.frameNavigated, Page.navigatedWithinDocument / SPA route changes, frame loading events, Target page created/attached/destroyed snapshots, and Target worker/service_worker/shared_worker snapshots. The normal Chrome bridge supports chrome-tab:* current-profile targets through a per-tab chrome.webNavigation ring buffer plus a typed MAIN-world worker shim for current-document worker creation/termination readback. Captured live pages include ready-to-pass set_target payloads and are scoped by opener metadata to the armed page target. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Call before navigation, popup creation, or worker creation for gap-free capture, then poll with since_seq=next_cursor."
    )]
    pub async fn browser_page_events(
        &self,
        params: Parameters<BrowserPageEventsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserPageEventsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = EVENTS_TOOL,
            "tool.invocation kind=browser_page_events"
        );
        let session_id = require_target_session_id(&request_context)?;
        let filters = validate_browser_page_events_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "since_seq": filters.since_seq,
            "limit": filters.limit,
            "event_kind": filters.event_kind,
            "worker_type": filters.worker_type,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            EVENTS_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            EVENTS_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "since_seq": filters.since_seq,
            "limit": filters.limit,
            "event_kind": filters.event_kind,
            "worker_type": filters.worker_type,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            EVENTS_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_page_events_impl(&session_id, window_hwnd, &cdp_target_id, &filters)
            .await;
        self.audit_action_result_for_session(EVENTS_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_clock_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserClockParams,
    ) -> Result<BrowserClockResponse, ErrorData> {
        if params.operation != BrowserClockOperation::Status {
            super::operator_panic_boundary::ensure_mcp_mutation("browser_clock_before_mutation")?;
        }
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let result = crate::chrome_debugger_bridge::clock(
                    window_hwnd,
                    cdp_target_id,
                    params.operation.wire(),
                    params.time_unix_ms,
                    params.delta_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "{CLOCK_TOOL} normal bridge clock operation failed: {}",
                            error.detail()
                        ),
                    )
                })?;
                if params.operation != BrowserClockOperation::Status {
                    super::operator_panic_boundary::ensure_mcp_mutation(
                        "browser_clock_after_bridge_mutation",
                    )?;
                }
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_CLOCK_READBACK",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %result.target_id,
                    operation = ?params.operation,
                    installed = result.readback.installed,
                    now_ms = ?result.readback.now_ms,
                    "readback=chrome.scripting.executeScript(MAIN synapse clock shim) outcome=clock_state"
                );
                return Ok(BrowserClockResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs"
                        .to_owned(),
                    cdp_target_id: result.target_id,
                    operation: params.operation,
                    time_unix_ms: params.time_unix_ms,
                    delta_ms: params.delta_ms,
                    init_script_identifier: result.init_script_identifier,
                    init_script_newly_added: result.init_script_newly_added,
                    installed_at_unix_ms: result.installed_at_unix_ms,
                    clock: browser_bridge_clock_readback(result.readback),
                    readback_backend: result.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(CLOCK_TOOL, window_hwnd));
        };
        let result = synapse_a11y::cdp_clock(
            &endpoint,
            cdp_target_id,
            params.operation.into(),
            params.time_unix_ms,
            params.delta_ms,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{CLOCK_TOOL} raw CDP clock operation failed: {error}"),
            )
        })?;
        if params.operation != BrowserClockOperation::Status {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_clock_after_raw_mutation",
            )?;
        }
        tracing::info!(
            code = "CDP_BACKGROUND_CLOCK_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?params.operation,
            installed = result.readback.installed,
            now_ms = ?result.readback.now_ms,
            "readback=Page.addScriptToEvaluateOnNewDocument+Runtime.evaluate outcome=clock_state"
        );
        Ok(BrowserClockResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint: result.endpoint,
            cdp_target_id: result.cdp_target_id,
            operation: params.operation,
            time_unix_ms: params.time_unix_ms,
            delta_ms: params.delta_ms,
            init_script_identifier: result.init_script_identifier,
            init_script_newly_added: result.init_script_newly_added,
            installed_at_unix_ms: result.installed_at_unix_ms,
            clock: browser_clock_readback(result.readback),
            readback_backend: "Page.addScriptToEvaluateOnNewDocument + Runtime.evaluate".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_clock_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserClockParams,
    ) -> Result<BrowserClockResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_clock is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_page_events_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserPageEventsParams,
    ) -> Result<BrowserPageEventsResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let result = crate::chrome_debugger_bridge::page_events(
                    window_hwnd,
                    cdp_target_id,
                    params.since_seq,
                    params.limit,
                    params.event_kind.as_deref(),
                    params.worker_type.as_deref(),
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "{EVENTS_TOOL} normal bridge pageEvents operation failed: {}",
                            error.detail()
                        ),
                    )
                })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_PAGE_EVENTS_READBACK",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %result.target_id,
                    returned = result.returned,
                    page_count = result.pages.len(),
                    worker_count = result.workers.len(),
                    "readback=chrome.webNavigation+chrome.scripting.executeScript(MAIN worker shim) outcome=list_returned"
                );
                return Ok(BrowserPageEventsResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs"
                        .to_owned(),
                    cdp_target_id: result.target_id,
                    capture_newly_armed: result.capture_newly_armed,
                    next_cursor: result.next_cursor,
                    returned: result.returned,
                    total_buffered: result.total_buffered,
                    dropped: result.dropped,
                    filters: BrowserPageEventsFilters {
                        since_seq: params.since_seq,
                        limit: params.limit,
                        event_kind: params.event_kind.clone(),
                        worker_type: params.worker_type.clone(),
                    },
                    entries: result
                        .entries
                        .into_iter()
                        .map(|entry| browser_bridge_page_event_entry(window_hwnd, entry))
                        .collect(),
                    pages: result
                        .pages
                        .into_iter()
                        .map(|page| browser_bridge_page_target_snapshot(window_hwnd, page))
                        .collect(),
                    workers: result
                        .workers
                        .into_iter()
                        .map(browser_bridge_worker_snapshot)
                        .collect(),
                    readback_backend: result.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(EVENTS_TOOL, window_hwnd));
        };
        let status = synapse_a11y::lifecycle_capture_ensure(
            &endpoint,
            cdp_target_id,
            synapse_a11y::DEFAULT_LIFECYCLE_BUFFER_CAPACITY,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{EVENTS_TOOL} raw CDP event capture failed: {error}"),
            )
        })?;
        let filter = synapse_a11y::CdpPageEventsReadFilter {
            since_seq: params.since_seq,
            event_kind: params.event_kind.as_deref(),
            worker_type: params.worker_type.as_deref(),
            max: params.limit,
        };
        let read =
            synapse_a11y::lifecycle_capture_read(cdp_target_id, &filter).ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!("{EVENTS_TOOL} capture was armed but no target buffer was readable"),
                )
            })?;
        tracing::info!(
            code = "CDP_BACKGROUND_PAGE_EVENTS_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            returned = read.returned,
            page_count = read.pages.len(),
            worker_count = read.workers.len(),
            "readback=Page.lifecycleEvent+Page.frameNavigated+Target.page_and_worker_events outcome=list_returned"
        );
        Ok(BrowserPageEventsResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint: status.endpoint,
            cdp_target_id: status.cdp_target_id,
            capture_newly_armed: status.newly_armed,
            next_cursor: read.next_cursor,
            returned: read.returned,
            total_buffered: read.total_buffered,
            dropped: read.dropped,
            filters: BrowserPageEventsFilters {
                since_seq: params.since_seq,
                limit: params.limit,
                event_kind: params.event_kind.clone(),
                worker_type: params.worker_type.clone(),
            },
            entries: read
                .entries
                .into_iter()
                .map(|entry| browser_page_event_entry(window_hwnd, entry))
                .collect(),
            pages: read
                .pages
                .into_iter()
                .map(|page| browser_page_target_snapshot(window_hwnd, page))
                .collect(),
            workers: read
                .workers
                .into_iter()
                .map(browser_worker_snapshot)
                .collect(),
            readback_backend: "Page lifecycle + Target page/worker event buffer".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_page_events_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserPageEventsParams,
    ) -> Result<BrowserPageEventsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_page_events is only available on Windows in this build",
        ))
    }
}

fn validate_browser_clock_params(
    params: &BrowserClockParams,
) -> Result<NormalizedBrowserClockParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if let Some(time) = params.time_unix_ms
        && time > MAX_CLOCK_MS
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{CLOCK_TOOL} time_unix_ms exceeds JavaScript Date range"),
        ));
    }
    if let Some(delta) = params.delta_ms
        && delta > MAX_CLOCK_MS
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{CLOCK_TOOL} delta_ms exceeds JavaScript Date range"),
        ));
    }
    match params.operation {
        BrowserClockOperation::Status => {
            reject_field(params.time_unix_ms, "time_unix_ms", "status")?;
            reject_field(params.delta_ms, "delta_ms", "status")?;
        }
        BrowserClockOperation::Install => {
            reject_field(params.delta_ms, "delta_ms", "install")?;
        }
        BrowserClockOperation::SetFixedTime => {
            require_field(params.time_unix_ms, "time_unix_ms", "set_fixed_time")?;
            reject_field(params.delta_ms, "delta_ms", "set_fixed_time")?;
        }
        BrowserClockOperation::FastForward => {
            require_field(params.delta_ms, "delta_ms", "fast_forward")?;
            reject_field(params.time_unix_ms, "time_unix_ms", "fast_forward")?;
        }
        BrowserClockOperation::PauseAt => {
            require_field(params.time_unix_ms, "time_unix_ms", "pause_at")?;
            reject_field(params.delta_ms, "delta_ms", "pause_at")?;
        }
    }
    Ok(NormalizedBrowserClockParams {
        operation: params.operation,
        time_unix_ms: params.time_unix_ms,
        delta_ms: params.delta_ms,
    })
}

fn validate_browser_page_events_params(
    params: &BrowserPageEventsParams,
) -> Result<NormalizedBrowserPageEventsParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PAGE_EVENT_LIMIT)
        .clamp(1, MAX_PAGE_EVENT_LIMIT);
    let event_kind =
        validate_optional_token(EVENTS_TOOL, "event_kind", params.event_kind.as_ref())?;
    if let Some(kind) = event_kind.as_deref()
        && !matches!(
            kind,
            "domcontentloaded"
                | "load"
                | "lifecycle"
                | "framenavigated"
                | "framestartednavigating"
                | "framestoppedloading"
                | "page_created"
                | "page_attached"
                | "page_destroyed"
                | "page_info_changed"
                | "worker_created"
                | "worker_attached"
                | "worker_destroyed"
                | "worker_info_changed"
        )
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{EVENTS_TOOL} event_kind {kind:?} is not supported"),
        ));
    }
    let worker_type =
        validate_optional_token(EVENTS_TOOL, "worker_type", params.worker_type.as_ref())?;
    if let Some(worker_type) = worker_type.as_deref()
        && !matches!(worker_type, "worker" | "service_worker" | "shared_worker")
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{EVENTS_TOOL} worker_type must be worker, service_worker, or shared_worker"),
        ));
    }
    Ok(NormalizedBrowserPageEventsParams {
        since_seq: params.since_seq,
        limit,
        event_kind,
        worker_type,
    })
}

fn require_field<T>(value: Option<T>, field: &str, operation: &str) -> Result<(), ErrorData> {
    if value.is_some() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{CLOCK_TOOL} operation={operation} requires {field}"),
        ))
    }
}

fn reject_field<T>(value: Option<T>, field: &str, operation: &str) -> Result<(), ErrorData> {
    if value.is_none() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{CLOCK_TOOL} {field} is not valid for operation={operation}"),
        ))
    }
}

fn validate_optional_token(
    tool: &str,
    field: &str,
    value: Option<&String>,
) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not be empty"),
        ));
    }
    if trimmed.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not contain NUL"),
        ));
    }
    if trimmed.chars().count() > MAX_PAGE_EVENT_TOKEN_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must be at most {MAX_PAGE_EVENT_TOKEN_CHARS} characters"),
        ));
    }
    Ok(Some(trimmed.to_ascii_lowercase()))
}

fn browser_clock_readback(readback: synapse_a11y::CdpClockReadback) -> BrowserClockReadback {
    BrowserClockReadback {
        installed: readback.installed,
        version: readback.version,
        now_ms: readback.now_ms,
        pending_timer_count: readback.pending_timer_count,
        fired_timer_count: readback.fired_timer_count,
        last_timer_id: readback.last_timer_id,
        next_timer_ms: readback.next_timer_ms,
        error_count: readback.error_count,
        last_error: readback.last_error,
    }
}

fn browser_bridge_clock_readback(
    readback: crate::chrome_debugger_bridge::ChromeDebuggerClockReadback,
) -> BrowserClockReadback {
    BrowserClockReadback {
        installed: readback.installed,
        version: readback.version,
        now_ms: readback.now_ms,
        pending_timer_count: readback.pending_timer_count,
        fired_timer_count: readback.fired_timer_count,
        last_timer_id: readback.last_timer_id,
        next_timer_ms: readback.next_timer_ms,
        error_count: readback.error_count,
        last_error: readback.last_error,
    }
}

fn browser_page_event_entry(
    window_hwnd: i64,
    entry: synapse_a11y::CdpPageEventEntry,
) -> BrowserPageEventEntry {
    let adoptable_target = entry.page_target_id.as_ref().and_then(|target_id| {
        (entry.event_kind != "page_destroyed").then(|| TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: target_id.clone(),
        })
    });
    BrowserPageEventEntry {
        seq: entry.seq,
        event_kind: entry.event_kind,
        target_id: entry.target_id,
        target_type: entry.target_type,
        target_attached: entry.target_attached,
        page_target_id: entry.page_target_id,
        adoptable_target,
        opener_id: entry.opener_id,
        opener_frame_id: entry.opener_frame_id,
        can_access_opener: entry.can_access_opener,
        browser_context_id: entry.browser_context_id,
        subtype: entry.subtype,
        worker_id: entry.worker_id,
        worker_type: entry.worker_type,
        worker_url: redact_url_opt_for_public_readback(entry.worker_url),
        frame_id: entry.frame_id,
        parent_frame_id: entry.parent_frame_id,
        loader_id: entry.loader_id,
        name: entry.name,
        url: redact_url_opt_for_public_readback(entry.url),
        title: entry.title,
        navigation_type: entry.navigation_type,
        timestamp_s: entry.timestamp_s,
        observed_at_unix_ms: entry.observed_at_unix_ms,
    }
}

fn browser_bridge_page_event_entry(
    window_hwnd: i64,
    entry: crate::chrome_debugger_bridge::ChromeDebuggerPageEventEntry,
) -> BrowserPageEventEntry {
    let adoptable_target = entry.page_target_id.as_ref().and_then(|target_id| {
        (entry.event_kind != "page_destroyed").then(|| TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: target_id.clone(),
        })
    });
    BrowserPageEventEntry {
        seq: entry.seq,
        event_kind: entry.event_kind,
        target_id: entry.target_id,
        target_type: entry.target_type,
        target_attached: entry.target_attached,
        page_target_id: entry.page_target_id,
        adoptable_target,
        opener_id: entry.opener_id,
        opener_frame_id: entry.opener_frame_id,
        can_access_opener: entry.can_access_opener,
        browser_context_id: entry.browser_context_id,
        subtype: entry.subtype,
        worker_id: entry.worker_id,
        worker_type: entry.worker_type,
        worker_url: redact_url_opt_for_public_readback(entry.worker_url),
        frame_id: entry.frame_id,
        parent_frame_id: entry.parent_frame_id,
        loader_id: entry.loader_id,
        name: entry.name,
        url: redact_url_opt_for_public_readback(entry.url),
        title: entry.title,
        navigation_type: entry.navigation_type,
        timestamp_s: entry.timestamp_s,
        observed_at_unix_ms: entry.observed_at_unix_ms,
    }
}

fn browser_page_target_snapshot(
    window_hwnd: i64,
    page: synapse_a11y::CdpPageTargetSnapshot,
) -> BrowserPageTargetSnapshot {
    BrowserPageTargetSnapshot {
        cdp_target_id: page.target_id.clone(),
        target: TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: page.target_id,
        },
        target_type: page.target_type,
        url: redact_url_for_public_readback(&page.url),
        title: page.title,
        opener_id: page.opener_id,
        opener_frame_id: page.opener_frame_id,
        can_access_opener: page.can_access_opener,
        browser_context_id: page.browser_context_id,
        subtype: page.subtype,
        attached: page.attached,
        adoptable: !page.destroyed,
        destroyed: page.destroyed,
        first_seen_seq: page.first_seen_seq,
        last_seen_seq: page.last_seen_seq,
        first_seen_unix_ms: page.first_seen_unix_ms,
        last_seen_unix_ms: page.last_seen_unix_ms,
    }
}

fn browser_bridge_page_target_snapshot(
    window_hwnd: i64,
    page: crate::chrome_debugger_bridge::ChromeDebuggerPageTargetSnapshot,
) -> BrowserPageTargetSnapshot {
    BrowserPageTargetSnapshot {
        cdp_target_id: page.target_id.clone(),
        target: TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: page.target_id,
        },
        target_type: page.target_type,
        url: redact_url_for_public_readback(&page.url),
        title: page.title,
        opener_id: page.opener_id,
        opener_frame_id: page.opener_frame_id,
        can_access_opener: page.can_access_opener,
        browser_context_id: page.browser_context_id,
        subtype: page.subtype,
        attached: page.attached,
        adoptable: !page.destroyed,
        destroyed: page.destroyed,
        first_seen_seq: page.first_seen_seq,
        last_seen_seq: page.last_seen_seq,
        first_seen_unix_ms: page.first_seen_unix_ms,
        last_seen_unix_ms: page.last_seen_unix_ms,
    }
}

fn browser_worker_snapshot(worker: synapse_a11y::CdpWorkerSnapshot) -> BrowserWorkerSnapshot {
    BrowserWorkerSnapshot {
        worker_id: worker.worker_id,
        worker_type: worker.worker_type,
        url: redact_url_for_public_readback(&worker.url),
        title: worker.title,
        attached: worker.attached,
        destroyed: worker.destroyed,
        first_seen_seq: worker.first_seen_seq,
        last_seen_seq: worker.last_seen_seq,
        first_seen_unix_ms: worker.first_seen_unix_ms,
        last_seen_unix_ms: worker.last_seen_unix_ms,
    }
}

fn browser_bridge_worker_snapshot(
    worker: crate::chrome_debugger_bridge::ChromeDebuggerWorkerSnapshot,
) -> BrowserWorkerSnapshot {
    BrowserWorkerSnapshot {
        worker_id: worker.worker_id,
        worker_type: worker.worker_type,
        url: redact_url_for_public_readback(&worker.url),
        title: worker.title,
        attached: worker.attached,
        destroyed: worker.destroyed,
        first_seen_seq: worker.first_seen_seq,
        last_seen_seq: worker.last_seen_seq,
        first_seen_unix_ms: worker.first_seen_unix_ms,
        last_seen_unix_ms: worker.last_seen_unix_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock_params(operation: BrowserClockOperation) -> BrowserClockParams {
        BrowserClockParams {
            operation,
            ..Default::default()
        }
    }

    #[test]
    fn browser_clock_validation_edges() {
        let install = validate_browser_clock_params(&BrowserClockParams {
            operation: BrowserClockOperation::Install,
            time_unix_ms: Some(1_700_000_000_000),
            ..Default::default()
        })
        .expect("install with time");
        assert_eq!(install.operation, BrowserClockOperation::Install);

        for error in [
            validate_browser_clock_params(&BrowserClockParams {
                operation: BrowserClockOperation::SetFixedTime,
                ..Default::default()
            })
            .expect_err("set_fixed_time requires time"),
            validate_browser_clock_params(&BrowserClockParams {
                operation: BrowserClockOperation::FastForward,
                time_unix_ms: Some(1),
                delta_ms: Some(10),
                ..Default::default()
            })
            .expect_err("fast_forward rejects time"),
            validate_browser_clock_params(&BrowserClockParams {
                operation: BrowserClockOperation::PauseAt,
                time_unix_ms: Some(MAX_CLOCK_MS + 1),
                ..Default::default()
            })
            .expect_err("oversized time rejected"),
            validate_browser_clock_params(&BrowserClockParams {
                operation: BrowserClockOperation::Status,
                delta_ms: Some(1),
                ..Default::default()
            })
            .expect_err("status rejects delta"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
        }

        assert!(
            validate_browser_clock_params(&clock_params(BrowserClockOperation::Status)).is_ok()
        );
    }

    #[test]
    fn browser_page_events_validation_edges() {
        let filters = validate_browser_page_events_params(&BrowserPageEventsParams {
            limit: Some(MAX_PAGE_EVENT_LIMIT + 100),
            event_kind: Some("Load".to_owned()),
            worker_type: Some("SERVICE_WORKER".to_owned()),
            ..Default::default()
        })
        .expect("filters");
        assert_eq!(filters.limit, MAX_PAGE_EVENT_LIMIT);
        assert_eq!(filters.event_kind.as_deref(), Some("load"));
        assert_eq!(filters.worker_type.as_deref(), Some("service_worker"));

        for error in [
            validate_browser_page_events_params(&BrowserPageEventsParams {
                event_kind: Some("navigation".to_owned()),
                ..Default::default()
            })
            .expect_err("unknown event rejected"),
            validate_browser_page_events_params(&BrowserPageEventsParams {
                event_kind: Some("page_created".to_owned()),
                worker_type: Some("page".to_owned()),
                ..Default::default()
            })
            .expect_err("page is not a worker type"),
            validate_browser_page_events_params(&BrowserPageEventsParams {
                worker_type: Some("page".to_owned()),
                ..Default::default()
            })
            .expect_err("unknown worker type rejected"),
            validate_browser_page_events_params(&BrowserPageEventsParams {
                event_kind: Some("bad\0kind".to_owned()),
                ..Default::default()
            })
            .expect_err("nul rejected"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
        }

        let page_filter = validate_browser_page_events_params(&BrowserPageEventsParams {
            event_kind: Some("PAGE_CREATED".to_owned()),
            ..Default::default()
        })
        .expect("page events are supported");
        assert_eq!(page_filter.event_kind.as_deref(), Some("page_created"));
    }

    #[test]
    fn browser_page_events_maps_page_targets_as_adoptable() {
        let page_event = browser_page_event_entry(
            0x1234,
            synapse_a11y::CdpPageEventEntry {
                seq: 7,
                event_kind: "page_created".to_owned(),
                target_id: "root-page".to_owned(),
                target_type: Some("page".to_owned()),
                target_attached: Some(false),
                page_target_id: Some("popup-page".to_owned()),
                opener_id: Some("root-page".to_owned()),
                opener_frame_id: None,
                can_access_opener: Some(false),
                browser_context_id: Some("context-1".to_owned()),
                subtype: None,
                worker_id: None,
                worker_type: None,
                worker_url: None,
                frame_id: None,
                parent_frame_id: None,
                loader_id: None,
                name: None,
                url: Some("https://example.test/popup".to_owned()),
                title: Some("Popup".to_owned()),
                navigation_type: None,
                timestamp_s: None,
                observed_at_unix_ms: 10,
            },
        );
        match page_event.adoptable_target {
            Some(TargetWire::Cdp {
                window_hwnd,
                cdp_target_id,
            }) => {
                assert_eq!(window_hwnd, 0x1234);
                assert_eq!(cdp_target_id, "popup-page");
            }
            other => panic!("unexpected target: {other:?}"),
        }

        let destroyed = browser_page_event_entry(
            0x1234,
            synapse_a11y::CdpPageEventEntry {
                seq: 8,
                event_kind: "page_destroyed".to_owned(),
                target_id: "root-page".to_owned(),
                target_type: Some("page".to_owned()),
                target_attached: None,
                page_target_id: Some("popup-page".to_owned()),
                opener_id: None,
                opener_frame_id: None,
                can_access_opener: None,
                browser_context_id: None,
                subtype: None,
                worker_id: None,
                worker_type: None,
                worker_url: None,
                frame_id: None,
                parent_frame_id: None,
                loader_id: None,
                name: None,
                url: None,
                title: None,
                navigation_type: None,
                timestamp_s: None,
                observed_at_unix_ms: 11,
            },
        );
        assert!(destroyed.adoptable_target.is_none());

        let snapshot = browser_page_target_snapshot(
            0x1234,
            synapse_a11y::CdpPageTargetSnapshot {
                target_id: "popup-page".to_owned(),
                target_type: "page".to_owned(),
                url: "https://example.test/popup".to_owned(),
                title: "Popup".to_owned(),
                opener_id: Some("root-page".to_owned()),
                opener_frame_id: None,
                can_access_opener: false,
                browser_context_id: Some("context-1".to_owned()),
                subtype: None,
                attached: true,
                destroyed: false,
                first_seen_seq: 7,
                last_seen_seq: 7,
                first_seen_unix_ms: 10,
                last_seen_unix_ms: 10,
            },
        );
        assert!(snapshot.adoptable);
        assert_eq!(snapshot.cdp_target_id, "popup-page");
    }
}
