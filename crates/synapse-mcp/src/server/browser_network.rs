//! Network capture listing tools (#1081) backed by the a11y CDP Network buffer.

use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{
        browser_raw_cdp_required_error, cdp_target_id_audit_ref, chrome_debugger_default_endpoint,
        require_target_session_id, validate_cdp_target_id,
    },
    tool, tool_router,
};
use crate::m1::{BrowserNetworkWaitEntry, mcp_error};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::{DateTime, Utc};
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const REQUESTS_TOOL: &str = "browser_network_requests";
const REQUEST_TOOL: &str = "browser_network_request";
const WEBSOCKETS_TOOL: &str = "browser_network_websockets";
const HAR_TOOL: &str = "browser_network_har";
const OVERRIDES_TOOL: &str = "browser_network_overrides";
const CONDITIONS_TOOL: &str = "browser_network_conditions";
const ROUTE_TOOL: &str = "browser_route";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
const DEFAULT_NETWORK_REQUEST_LIMIT: usize = 100;
const MAX_NETWORK_REQUEST_LIMIT: usize = 1000;
const MAX_NETWORK_FILTER_CHARS: usize = 8192;
const MAX_NETWORK_RESOURCE_TYPE_CHARS: usize = 128;
const MAX_NETWORK_REQUEST_ID_CHARS: usize = 2048;
const MAX_ROUTE_ID_CHARS: usize = 256;
const MAX_ROUTE_URL_CHARS: usize = 8192;
const MAX_ROUTE_RESPONSE_PHRASE_CHARS: usize = 256;
const MAX_ROUTE_HEADER_COUNT: usize = 128;
const MAX_ROUTE_HEADER_NAME_CHARS: usize = 256;
const MAX_ROUTE_HEADER_VALUE_CHARS: usize = 8192;
const MAX_ROUTE_BODY_CHARS: usize = 1_048_576;
const MAX_NETWORK_USER_AGENT_CHARS: usize = 4096;
const MAX_HAR_PATH_CHARS: usize = 4096;
const MAX_HAR_REPLAY_ENTRIES: usize = 1000;
const MAX_HAR_FILE_BYTES: u64 = 64 * 1024 * 1024;
const HAR_REPLAY_ROUTE_PREFIX: &str = "har-replay-";
const HAR_REPLAY_MISS_ROUTE_ID: &str = "har-replay-missing";

/// Parameters for `browser_network_requests` (#1081): return captured Network
/// request records for the calling session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestsParams {
    /// CDP TargetID to read. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Return only records whose latest update sequence is >= this cursor.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum records to return after filtering. Defaults to 100, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Case-insensitive substring filter against request URL.
    #[serde(default)]
    pub url_contains: Option<String>,
    /// Regular expression filter against request URL.
    #[serde(default)]
    pub url_regex: Option<String>,
    /// Case-insensitive CDP Network resource type filter.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// Minimum HTTP status, inclusive.
    #[serde(default)]
    pub status_min: Option<i64>,
    /// Maximum HTTP status, inclusive.
    #[serde(default)]
    pub status_max: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_max: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestsResponse {
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
    pub filters: BrowserNetworkRequestFilters,
    pub entries: Vec<BrowserNetworkWaitEntry>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_network_websockets` (#1089): return captured
/// WebSocket lifecycle and sent/received frame records for the calling
/// session's owned CDP target.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkWebSocketsParams {
    /// CDP TargetID to read. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Return only sockets whose latest update sequence is >= this cursor.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum sockets to return after filtering. Defaults to 100, max 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Exact CDP request id filter.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Case-insensitive substring filter against WebSocket URL.
    #[serde(default)]
    pub url_contains: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkWebSocketFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_contains: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkWebSocketsResponse {
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
    pub filters: BrowserNetworkWebSocketFilters,
    pub entries: Vec<BrowserNetworkWebSocketEntry>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Parameters for `browser_network_request` (#1082): inspect one captured
/// Network request by CDP request id, including response body by default.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestParams {
    /// CDP request id from `browser_network_requests`, `browser_wait_for_request`,
    /// or `browser_wait_for_response`.
    pub request_id: String,
    /// CDP TargetID to read. Defaults to the active session CDP target. Must be
    /// owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Include `Network.getResponseBody` readback. Defaults to true.
    #[serde(default = "default_true")]
    pub include_body: bool,
    /// Include `Network.getRequestPostData` when CDP reported post data.
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub include_post_data: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub capture_newly_armed: bool,
    pub request_id: String,
    pub include_body: bool,
    pub include_post_data: bool,
    pub entry: BrowserNetworkRequestDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_post_data: Option<BrowserNetworkRequestPostData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<BrowserNetworkResponseBody>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestDetail {
    pub seq: u64,
    pub first_seq: u64,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_has_post_data: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_wall_time_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Value>,
    pub redirects: Vec<BrowserNetworkResponseSnapshot>,
    pub response_received: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<BrowserNetworkResponseSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_timestamp_s: Option<f64>,
    pub loading_finished: bool,
    pub loading_failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_data_length: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_error_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_canceled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_cors_error_status: Option<Value>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkResponseSnapshot {
    pub url: String,
    pub status: i64,
    pub status_text: String,
    pub headers: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<Value>,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_ip_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<i64>,
    pub encoded_data_length: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_time_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_disk_cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_service_worker: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_prefetch_cache: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_early_hints: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkRequestPostData {
    pub request_id: String,
    pub post_data: String,
    pub post_data_len_chars: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkResponseBody {
    pub request_id: String,
    pub body: String,
    pub base64_encoded: bool,
    pub body_len_chars: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkWebSocketEntry {
    pub seq: u64,
    pub first_seq: u64,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub created: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_request_timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_request_wall_time_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_request_headers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_response_timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_response_headers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_response_headers_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_response_request_headers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake_response_request_headers_text: Option<String>,
    pub frames: Vec<BrowserNetworkWebSocketFrame>,
    pub sent_frame_count: u64,
    pub received_frame_count: u64,
    pub frame_error_count: u64,
    pub dropped_frames: u64,
    pub closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkWebSocketFrame {
    pub seq: u64,
    pub direction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opcode: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_data: Option<String>,
    pub payload_len_chars: usize,
    pub payload_base64_encoded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Operation for `browser_network_har` (#1088).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNetworkHarOperation {
    /// Serialize captured Network entries to a HAR 1.2 file.
    #[default]
    Record,
    /// Install Fetch fulfill rules from a HAR file.
    Replay,
    /// Remove route rules previously installed by HAR replay.
    ClearReplay,
}

/// Missing-entry policy for HAR replay.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNetworkHarMissingPolicy {
    /// Requests absent from the HAR continue to the network.
    #[default]
    Passthrough,
    /// Requests absent from the HAR fail with `BlockedByClient`.
    Abort,
}

/// Parameters for `browser_network_har` (#1088): record captured requests to a
/// HAR file or replay a HAR through target-scoped Fetch fulfill routes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkHarParams {
    /// CDP TargetID to record/replay. Defaults to the active session CDP target.
    /// Must be owned by this session; the human foreground tab is never an
    /// implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// HAR operation. Defaults to `record`.
    #[serde(default)]
    pub operation: BrowserNetworkHarOperation,
    /// HAR file path. Required for `record` and `replay`.
    #[serde(default)]
    pub path: Option<String>,
    /// Record only entries whose latest update sequence is >= this cursor.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum entries to record/replay. Defaults to 100 for record and all HAR
    /// entries for replay, capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Case-insensitive substring filter against request URL while recording.
    #[serde(default)]
    pub url_contains: Option<String>,
    /// Regular expression filter against request URL while recording.
    #[serde(default)]
    pub url_regex: Option<String>,
    /// Case-insensitive CDP Network resource type filter while recording.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// Minimum HTTP status, inclusive, while recording.
    #[serde(default)]
    pub status_min: Option<i64>,
    /// Maximum HTTP status, inclusive, while recording.
    #[serde(default)]
    pub status_max: Option<i64>,
    /// Include retained response bodies in recorded HAR content. Defaults true.
    #[serde(default)]
    pub include_bodies: Option<bool>,
    /// Replay behavior for requests not present in the HAR. Defaults
    /// `passthrough`.
    #[serde(default)]
    pub missing_policy: Option<BrowserNetworkHarMissingPolicy>,
    /// Remove existing HAR replay rules for this target before replaying.
    /// Defaults true.
    #[serde(default)]
    pub clear_existing_replay: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkHarFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<u64>,
    pub limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_max: Option<i64>,
    pub include_bodies: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkHarResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserNetworkHarOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<BrowserNetworkHarFilters>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_policy: Option<BrowserNetworkHarMissingPolicy>,
    pub capture_newly_armed: bool,
    pub recorded_entry_count: usize,
    pub skipped_entry_count: usize,
    pub replay_entry_count: usize,
    pub replay_route_count: usize,
    pub cleared_replay_route_count: usize,
    pub missing_abort_route_installed: bool,
    pub har_bytes: u64,
    pub route_count: usize,
    pub routes: Vec<BrowserRouteRuleResponse>,
    pub fetch_status: BrowserRouteFetchStatus,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Operation for `browser_network_overrides` (#1087).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNetworkOverridesOperation {
    /// Replace target-scoped headers and optional User-Agent override.
    #[default]
    Set,
    /// Read current tracked override state.
    Get,
    /// Clear headers and restore the captured original User-Agent.
    Clear,
}

/// Parameters for `browser_network_overrides` (#1087).
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkOverridesParams {
    /// CDP TargetID to configure. Defaults to the active session CDP target.
    /// Must be owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Operation. Defaults to `set`.
    #[serde(default)]
    pub operation: BrowserNetworkOverridesOperation,
    /// Replacement extra HTTP headers for `set`. An empty list clears headers.
    #[serde(default)]
    pub headers: Vec<BrowserRouteHeader>,
    /// Replacement User-Agent for `set`. Omit to clear a prior UA override.
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkOverridesResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserNetworkOverridesOperation,
    pub override_active: bool,
    pub newly_armed: bool,
    pub cleared: bool,
    pub armed_at_unix_ms: u64,
    pub applied_at_unix_ms: u64,
    pub header_count: usize,
    pub headers: Vec<BrowserRouteHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_user_agent: Option<String>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Operation for `browser_network_conditions` (#1178).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserNetworkConditionsOperation {
    /// Apply offline/throttling settings to the target.
    #[default]
    Set,
    /// Restore online/full-speed defaults.
    Reset,
}

/// Parameters for `browser_network_conditions` (#1178).
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkConditionsParams {
    /// CDP TargetID to configure. Defaults to the active session CDP target.
    /// Must be owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Operation. Defaults to `set`.
    #[serde(default)]
    pub operation: BrowserNetworkConditionsOperation,
    /// Emulate offline mode for operation=set. Omit to keep online.
    #[serde(default)]
    pub offline: Option<bool>,
    /// Minimum request latency in milliseconds for operation=set. Defaults to 0.
    #[serde(default)]
    pub latency_ms: Option<f64>,
    /// Max aggregate download throughput in bytes/sec for operation=set.
    /// Defaults to -1, which disables download throttling.
    #[serde(default)]
    pub download_throughput_bytes_per_sec: Option<f64>,
    /// Max aggregate upload throughput in bytes/sec for operation=set.
    /// Defaults to -1, which disables upload throttling.
    #[serde(default)]
    pub upload_throughput_bytes_per_sec: Option<f64>,
    /// CDP connection type for operation=set: none, cellular2g, cellular3g,
    /// cellular4g, bluetooth, ethernet, wifi, wimax, or other.
    #[serde(default)]
    pub connection_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkConditionsOverride {
    pub offline: bool,
    pub latency_ms: f64,
    pub download_throughput_bytes_per_sec: f64,
    pub upload_throughput_bytes_per_sec: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkConditionsReadback {
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downlink_mbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_data: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserNetworkConditionsResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserNetworkConditionsOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<BrowserNetworkConditionsOverride>,
    pub page_url: String,
    pub page_title: String,
    pub ready_state: String,
    pub network: BrowserNetworkConditionsReadback,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

/// Operation for `browser_route` (#1084).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRouteOperation {
    /// Add or replace a route that fulfills matching requests.
    #[default]
    AddFulfill,
    /// Add or replace a route that aborts matching requests.
    AddAbort,
    /// Add or replace a route that continues matching requests with overrides.
    AddContinue,
    /// Remove one route by id.
    Remove,
    /// Clear all routes for the target and disable Fetch interception.
    Clear,
    /// List active routes without arming interception.
    List,
}

/// URL match kind for `browser_route` (#1084).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRouteMatchKind {
    /// Simple glob with `*` and `?`.
    #[default]
    Glob,
    /// Rust regular expression.
    Regex,
}

/// CDP network error reason for `browser_route` add_abort (#1085).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRouteErrorReason {
    Failed,
    Aborted,
    TimedOut,
    AccessDenied,
    ConnectionClosed,
    ConnectionReset,
    ConnectionRefused,
    ConnectionAborted,
    ConnectionFailed,
    NameNotResolved,
    InternetDisconnected,
    AddressUnreachable,
    #[default]
    BlockedByClient,
    BlockedByResponse,
}

impl BrowserRouteErrorReason {
    fn as_cdp_str(self) -> &'static str {
        match self {
            BrowserRouteErrorReason::Failed => "Failed",
            BrowserRouteErrorReason::Aborted => "Aborted",
            BrowserRouteErrorReason::TimedOut => "TimedOut",
            BrowserRouteErrorReason::AccessDenied => "AccessDenied",
            BrowserRouteErrorReason::ConnectionClosed => "ConnectionClosed",
            BrowserRouteErrorReason::ConnectionReset => "ConnectionReset",
            BrowserRouteErrorReason::ConnectionRefused => "ConnectionRefused",
            BrowserRouteErrorReason::ConnectionAborted => "ConnectionAborted",
            BrowserRouteErrorReason::ConnectionFailed => "ConnectionFailed",
            BrowserRouteErrorReason::NameNotResolved => "NameNotResolved",
            BrowserRouteErrorReason::InternetDisconnected => "InternetDisconnected",
            BrowserRouteErrorReason::AddressUnreachable => "AddressUnreachable",
            BrowserRouteErrorReason::BlockedByClient => "BlockedByClient",
            BrowserRouteErrorReason::BlockedByResponse => "BlockedByResponse",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrowserRouteHeader {
    pub name: String,
    pub value: String,
}

/// Parameters for `browser_route` (#1084): add/list/remove/clear target-scoped
/// Fetch routes. `add_fulfill` fulfills matching requests; `add_abort` fails
/// matching requests; unmatched requests continue by default.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserRouteParams {
    /// CDP TargetID to route. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Route operation. Defaults to `add_fulfill`.
    #[serde(default)]
    pub operation: BrowserRouteOperation,
    /// Route id. Optional for `add_fulfill`; generated when omitted.
    #[serde(default)]
    pub route_id: Option<String>,
    /// URL glob or regex for `add_fulfill`.
    #[serde(default)]
    pub url: Option<String>,
    /// URL match kind for `add_fulfill`. Defaults to `glob`.
    #[serde(default)]
    pub match_kind: BrowserRouteMatchKind,
    /// Optional CDP resource type, e.g. `Document`, `XHR`, `Fetch`, `Script`.
    #[serde(default)]
    pub resource_type: Option<String>,
    /// HTTP status for fulfilled responses. Defaults to 200.
    #[serde(default)]
    pub status: Option<i64>,
    /// Optional reason phrase for the fulfilled response.
    #[serde(default)]
    pub response_phrase: Option<String>,
    /// UTF-8 response headers for the fulfilled response.
    #[serde(default)]
    pub headers: Vec<BrowserRouteHeader>,
    /// UTF-8 response body. Mutually exclusive with `body_base64`.
    #[serde(default)]
    pub body: Option<String>,
    /// Base64-encoded response body. Mutually exclusive with `body`.
    #[serde(default)]
    pub body_base64: Option<String>,
    /// CDP Network.ErrorReason for `add_abort`. Defaults to `blocked_by_client`.
    #[serde(default)]
    pub error_reason: Option<BrowserRouteErrorReason>,
    /// Replacement request URL for `add_continue`.
    #[serde(default)]
    pub continue_url: Option<String>,
    /// Replacement request method for `add_continue`.
    #[serde(default)]
    pub continue_method: Option<String>,
    /// Replacement request headers for `add_continue`.
    #[serde(default)]
    pub continue_headers: Vec<BrowserRouteHeader>,
    /// UTF-8 replacement request postData for `add_continue`.
    #[serde(default)]
    pub continue_post_data: Option<String>,
    /// Base64 replacement request postData for `add_continue`.
    #[serde(default)]
    pub continue_post_data_base64: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserRouteResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub operation: BrowserRouteOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    pub route_removed: bool,
    pub cleared_count: usize,
    pub route_count: usize,
    pub routes: Vec<BrowserRouteRuleResponse>,
    pub fetch_status: BrowserRouteFetchStatus,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserRouteRuleResponse {
    pub id: String,
    pub url: String,
    pub match_kind: BrowserRouteMatchKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_phrase: Option<String>,
    pub headers: Vec<BrowserRouteHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64_len_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_data_base64_len_chars: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserRouteFetchStatus {
    pub fetch_armed: bool,
    pub newly_armed: bool,
    pub armed_at_unix_ms: u64,
    pub pattern_count: usize,
    pub route_count: usize,
    pub paused_count: u64,
    pub continued_count: u64,
    pub fulfilled_count: u64,
    pub failed_count: u64,
    pub continue_error_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_route_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkRequestsParams {
    since_seq: Option<u64>,
    limit: usize,
    url_contains: Option<String>,
    url_regex_pattern: Option<String>,
    url_regex: Option<regex::Regex>,
    resource_type: Option<String>,
    status_min: Option<i64>,
    status_max: Option<i64>,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkWebSocketsParams {
    since_seq: Option<u64>,
    limit: usize,
    request_id: Option<String>,
    url_contains: Option<String>,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkHarParams {
    operation: BrowserNetworkHarOperation,
    path: Option<PathBuf>,
    path_display: Option<String>,
    filters: NormalizedBrowserNetworkRequestsParams,
    include_bodies: bool,
    missing_policy: BrowserNetworkHarMissingPolicy,
    clear_existing_replay: bool,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkRequestParams {
    request_id: String,
    include_body: bool,
    include_post_data: bool,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkOverridesParams {
    operation: BrowserNetworkOverridesOperation,
    headers: Vec<(String, String)>,
    user_agent: Option<String>,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkConditionsParams {
    operation: BrowserNetworkConditionsOperation,
    requested: Option<BrowserNetworkConditionsOverride>,
}

#[derive(Debug)]
struct NormalizedBrowserRouteParams {
    operation: BrowserRouteOperation,
    route_id: Option<String>,
    route: Option<synapse_a11y::CdpFetchRouteRule>,
}

#[tool_router(router = browser_network_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "List captured Network request records for the calling session's owned browser tab. Arms/reuses the target-scoped raw CDP Network buffer, returns cursor-delimited entries, and supports filters for url_contains, url_regex, resource_type, status_min/status_max, and since_seq. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Raw CDP only; the popup-safe normal Chrome extension bridge fails closed."
    )]
    pub async fn browser_network_requests(
        &self,
        params: Parameters<BrowserNetworkRequestsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNetworkRequestsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = REQUESTS_TOOL,
            "tool.invocation kind=browser_network_requests"
        );
        let session_id = require_target_session_id(&request_context)?;
        let filters = validate_browser_network_requests_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "since_seq": filters.since_seq,
            "limit": filters.limit,
            "url_contains_len": filters.url_contains.as_deref().map(str::len),
            "url_regex_len": filters.url_regex_pattern.as_deref().map(str::len),
            "resource_type": filters.resource_type.as_deref(),
            "status_min": filters.status_min,
            "status_max": filters.status_max,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            REQUESTS_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            REQUESTS_TOOL,
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
            "url_contains_len": filters.url_contains.as_deref().map(str::len),
            "url_regex_len": filters.url_regex_pattern.as_deref().map(str::len),
            "resource_type": filters.resource_type.as_deref(),
            "status_min": filters.status_min,
            "status_max": filters.status_max,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            REQUESTS_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_network_requests_impl(&session_id, window_hwnd, &cdp_target_id, &filters)
            .await;
        self.audit_action_result_for_session(REQUESTS_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "List captured WebSocket lifecycle and frame records for the calling session's owned browser tab. Arms/reuses the same target-scoped raw CDP Network buffer as browser_network_requests and returns cursor-delimited Network.webSocketCreated, handshake, sent/received frame, frame error, and closed events. Close code/reason are decoded from opcode 8 close frames when CDP provides the frame payload. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Raw CDP only; the popup-safe normal Chrome bridge fails closed."
    )]
    pub async fn browser_network_websockets(
        &self,
        params: Parameters<BrowserNetworkWebSocketsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNetworkWebSocketsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = WEBSOCKETS_TOOL,
            "tool.invocation kind=browser_network_websockets"
        );
        let session_id = require_target_session_id(&request_context)?;
        let filters = validate_browser_network_websockets_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "since_seq": filters.since_seq,
            "limit": filters.limit,
            "request_id": filters.request_id.as_deref(),
            "url_contains_len": filters.url_contains.as_deref().map(str::len),
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            WEBSOCKETS_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            WEBSOCKETS_TOOL,
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
            "request_id": filters.request_id.as_deref(),
            "url_contains_len": filters.url_contains.as_deref().map(str::len),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            WEBSOCKETS_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_network_websockets_impl(&session_id, window_hwnd, &cdp_target_id, &filters)
            .await;
        self.audit_action_result_for_session(WEBSOCKETS_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Record captured Network requests to a HAR 1.2 file, replay a HAR through exact target-scoped Fetch fulfill routes, or clear HAR replay routes for the calling session's owned browser tab. Record reuses the raw CDP Network buffer and can include retained Network.getResponseBody payloads. Replay reads a local HAR file, installs exact URL fulfill routes, and makes the missing-entry policy explicit as passthrough or abort. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Raw CDP only; the popup-safe normal Chrome bridge fails closed."
    )]
    pub async fn browser_network_har(
        &self,
        params: Parameters<BrowserNetworkHarParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNetworkHarResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = HAR_TOOL,
            "tool.invocation kind=browser_network_har"
        );
        let session_id = require_target_session_id(&request_context)?;
        let har = validate_browser_network_har_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": har.operation,
            "path": har.path_display.as_deref(),
            "since_seq": har.filters.since_seq,
            "limit": har.filters.limit,
            "url_contains_len": har.filters.url_contains.as_deref().map(str::len),
            "url_regex_len": har.filters.url_regex_pattern.as_deref().map(str::len),
            "resource_type": har.filters.resource_type.as_deref(),
            "status_min": har.filters.status_min,
            "status_max": har.filters.status_max,
            "include_bodies": har.include_bodies,
            "missing_policy": har.missing_policy,
            "clear_existing_replay": har.clear_existing_replay,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            HAR_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            HAR_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": har.operation,
            "path": har.path_display.as_deref(),
            "since_seq": har.filters.since_seq,
            "limit": har.filters.limit,
            "url_contains_len": har.filters.url_contains.as_deref().map(str::len),
            "url_regex_len": har.filters.url_regex_pattern.as_deref().map(str::len),
            "resource_type": har.filters.resource_type.as_deref(),
            "status_min": har.filters.status_min,
            "status_max": har.filters.status_max,
            "include_bodies": har.include_bodies,
            "missing_policy": har.missing_policy,
            "clear_existing_replay": har.clear_existing_replay,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            HAR_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_network_har_impl(&session_id, window_hwnd, &cdp_target_id, &har)
            .await;
        self.audit_action_result_for_session(HAR_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Inspect one captured Network request by CDP request_id for the calling session's owned browser tab. Reuses/arms the target-scoped raw CDP Network buffer, returns full request/response metadata, optional request post data, and a base64-aware Network.getResponseBody payload by default. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Raw CDP only; the popup-safe normal Chrome extension bridge fails closed."
    )]
    pub async fn browser_network_request(
        &self,
        params: Parameters<BrowserNetworkRequestParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNetworkRequestResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = REQUEST_TOOL,
            "tool.invocation kind=browser_network_request"
        );
        let session_id = require_target_session_id(&request_context)?;
        let request = validate_browser_network_request_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "request_id": &request.request_id,
            "include_body": request.include_body,
            "include_post_data": request.include_post_data,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            REQUEST_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            REQUEST_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "request_id": &request.request_id,
            "include_body": request.include_body,
            "include_post_data": request.include_post_data,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            REQUEST_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_network_request_impl(&session_id, window_hwnd, &cdp_target_id, &request)
            .await;
        self.audit_action_result_for_session(REQUEST_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Set/get/clear target-scoped extra HTTP headers and User-Agent override for the calling session's owned browser tab. Uses raw CDP Network.setExtraHTTPHeaders and Emulation.setUserAgentOverride, keeps a target-scoped override session alive for readback/clear, and never activates the tab, uses OS foreground input, or falls back to the human foreground tab. Raw CDP only; the popup-safe normal Chrome extension bridge fails closed."
    )]
    pub async fn browser_network_overrides(
        &self,
        params: Parameters<BrowserNetworkOverridesParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNetworkOverridesResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = OVERRIDES_TOOL,
            "tool.invocation kind=browser_network_overrides"
        );
        let session_id = require_target_session_id(&request_context)?;
        let overrides = validate_browser_network_overrides_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": overrides.operation,
            "header_count": overrides.headers.len(),
            "user_agent_len": overrides.user_agent.as_deref().map(str::len),
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            OVERRIDES_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            OVERRIDES_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": overrides.operation,
            "header_count": overrides.headers.len(),
            "user_agent_len": overrides.user_agent.as_deref().map(str::len),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            OVERRIDES_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_network_overrides_impl(&session_id, window_hwnd, &cdp_target_id, &overrides)
            .await;
        self.audit_action_result_for_session(OVERRIDES_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Set or reset target-scoped offline/throttled network conditions for the calling session's owned browser tab. Raw CDP targets use Network.emulateNetworkConditions; normal Chrome bridge chrome-tab:* targets use the networkConditions bridge lane in the already-open authenticated profile with MAIN-world navigator/fetch/networkInformation readback. operation=set applies offline, latency_ms, download_throughput_bytes_per_sec, upload_throughput_bytes_per_sec, and optional connection_type; omitted numeric fields default to online/full-speed values. operation=reset restores online/full-speed defaults. Returns same-target navigator.onLine and Network Information API readback; use page text/fetch timing or browser_network_requests on raw-CDP targets as independent FSV timing evidence. Target-scoped and foreground-capable without activating the human OS foreground tab; never falls back to the human foreground tab."
    )]
    pub async fn browser_network_conditions(
        &self,
        params: Parameters<BrowserNetworkConditionsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNetworkConditionsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = CONDITIONS_TOOL,
            "tool.invocation kind=browser_network_conditions"
        );
        let session_id = require_target_session_id(&request_context)?;
        let conditions = validate_browser_network_conditions_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": conditions.operation,
            "requested": &conditions.requested,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            CONDITIONS_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            CONDITIONS_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": conditions.operation,
            "requested": &conditions.requested,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            CONDITIONS_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_network_conditions_impl(&session_id, window_hwnd, &cdp_target_id, &conditions)
            .await;
        self.audit_action_result_for_session(CONDITIONS_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Add/list/remove/clear Fetch route rules for the calling session's owned browser tab. The default add_fulfill operation arms target-scoped raw CDP Fetch interception and fulfills matching URL glob/regex requests with status/headers/body; add_abort fails matching requests with Fetch.failRequest and a CDP Network.ErrorReason; add_continue continues matching requests with optional URL, method, headers, and postData overrides. Unmatched requests continue by default. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Raw CDP only; the popup-safe normal Chrome extension bridge fails closed."
    )]
    pub async fn browser_route(
        &self,
        params: Parameters<BrowserRouteParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserRouteResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = ROUTE_TOOL,
            "tool.invocation kind=browser_route"
        );
        let session_id = require_target_session_id(&request_context)?;
        let route = validate_browser_route_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": route.operation,
            "route_id": route.route_id.as_deref(),
            "url_len": params.0.url.as_deref().map(str::len),
            "match_kind": params.0.match_kind,
            "resource_type": params.0.resource_type.as_deref(),
            "status": params.0.status,
            "header_count": params.0.headers.len(),
            "body_len": params.0.body.as_deref().map(str::len),
            "body_base64_len": params.0.body_base64.as_deref().map(str::len),
            "error_reason": params.0.error_reason,
            "continue_url_len": params.0.continue_url.as_deref().map(str::len),
            "continue_method": params.0.continue_method.as_deref(),
            "continue_header_count": params.0.continue_headers.len(),
            "continue_post_data_len": params.0.continue_post_data.as_deref().map(str::len),
            "continue_post_data_base64_len": params.0.continue_post_data_base64.as_deref().map(str::len),
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            ROUTE_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            ROUTE_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": route.operation,
            "route_id": route.route_id.as_deref(),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            ROUTE_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_route_impl(&session_id, window_hwnd, &cdp_target_id, &route)
            .await;
        self.audit_action_result_for_session(ROUTE_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_network_requests_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        filters: &NormalizedBrowserNetworkRequestsParams,
    ) -> Result<BrowserNetworkRequestsResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(REQUESTS_TOOL, window_hwnd));
        };
        let capture = synapse_a11y::network_capture_ensure(
            &endpoint,
            cdp_target_id,
            synapse_a11y::DEFAULT_NETWORK_BUFFER_CAPACITY,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{REQUESTS_TOOL} raw CDP network capture failed: {error}"),
            )
        })?;
        let read = synapse_a11y::network_capture_read(
            cdp_target_id,
            &synapse_a11y::CdpNetworkReadFilter {
                since_seq: filters.since_seq,
                max: 0,
                ..Default::default()
            },
        )
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{REQUESTS_TOOL} network capture was not armed for target {cdp_target_id}"),
            )
        })?;
        let entries = filter_network_entries(read.entries.into_iter(), filters)
            .into_iter()
            .take(filters.limit)
            .map(|entry| browser_network_entry_to_wire(&entry))
            .collect::<Vec<_>>();
        tracing::info!(
            code = "CDP_BACKGROUND_NETWORK_REQUESTS",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            returned = entries.len(),
            total_buffered = read.total_buffered,
            next_cursor = read.next_cursor,
            "readback=Network.event_buffer(browser_network_requests) outcome=list_returned"
        );
        Ok(BrowserNetworkRequestsResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            capture_newly_armed: capture.newly_armed,
            next_cursor: read.next_cursor,
            returned: entries.len(),
            total_buffered: read.total_buffered,
            dropped: read.dropped,
            filters: filters.to_wire(),
            entries,
            readback_backend: "Network event buffer(browser_network_requests)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_network_websockets_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        filters: &NormalizedBrowserNetworkWebSocketsParams,
    ) -> Result<BrowserNetworkWebSocketsResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(WEBSOCKETS_TOOL, window_hwnd));
        };
        let capture = synapse_a11y::network_capture_ensure(
            &endpoint,
            cdp_target_id,
            synapse_a11y::DEFAULT_NETWORK_BUFFER_CAPACITY,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{WEBSOCKETS_TOOL} raw CDP network capture failed: {error}"),
            )
        })?;
        let read = synapse_a11y::network_web_socket_read(
            cdp_target_id,
            &synapse_a11y::CdpWebSocketReadFilter {
                since_seq: filters.since_seq,
                request_id: filters.request_id.as_deref(),
                url_contains: filters.url_contains.as_deref(),
                max: filters.limit,
            },
        )
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "{WEBSOCKETS_TOOL} network capture was not armed for target {cdp_target_id}"
                ),
            )
        })?;
        let entries = read
            .entries
            .iter()
            .map(browser_network_websocket_entry_to_wire)
            .collect::<Vec<_>>();
        tracing::info!(
            code = "CDP_BACKGROUND_NETWORK_WEBSOCKETS",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            returned = entries.len(),
            total_buffered = read.total_buffered,
            next_cursor = read.next_cursor,
            "readback=Network.webSocket* event_buffer(browser_network_websockets) outcome=list_returned"
        );
        Ok(BrowserNetworkWebSocketsResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            capture_newly_armed: capture.newly_armed,
            next_cursor: read.next_cursor,
            returned: entries.len(),
            total_buffered: read.total_buffered,
            dropped: read.dropped,
            filters: filters.to_wire(),
            entries,
            readback_backend: "Network.webSocket* event buffer(browser_network_websockets)"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_network_har_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        har: &NormalizedBrowserNetworkHarParams,
    ) -> Result<BrowserNetworkHarResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(HAR_TOOL, window_hwnd));
        };

        let mut capture_newly_armed = false;
        let mut recorded_entry_count = 0usize;
        let mut skipped_entry_count = 0usize;
        let mut replay_entry_count = 0usize;
        let mut replay_route_count = 0usize;
        let mut cleared_replay_route_count = 0usize;
        let mut missing_abort_route_installed = false;
        let mut har_bytes = 0u64;

        match har.operation {
            BrowserNetworkHarOperation::Record => {
                let capture = synapse_a11y::network_capture_ensure(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::DEFAULT_NETWORK_BUFFER_CAPACITY,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{HAR_TOOL} raw CDP network capture failed: {error}"),
                    )
                })?;
                capture_newly_armed = capture.newly_armed;
                let read = synapse_a11y::network_capture_read(
                    cdp_target_id,
                    &synapse_a11y::CdpNetworkReadFilter {
                        since_seq: har.filters.since_seq,
                        max: 0,
                        ..Default::default()
                    },
                )
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{HAR_TOOL} network capture was not armed for target {cdp_target_id}"
                        ),
                    )
                })?;
                let entries = filter_network_entries(read.entries.into_iter(), &har.filters)
                    .into_iter()
                    .take(har.filters.limit)
                    .collect::<Vec<_>>();
                let path = har.path.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("{HAR_TOOL} record path was not normalized"),
                    )
                })?;
                let record =
                    write_har_record(cdp_target_id, path, &entries, har.include_bodies).await?;
                recorded_entry_count = record.entry_count;
                skipped_entry_count = record.skipped_count;
                har_bytes = record.bytes_written;
            }
            BrowserNetworkHarOperation::Replay => {
                let path = har.path.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("{HAR_TOOL} replay path was not normalized"),
                    )
                })?;
                if har.clear_existing_replay {
                    cleared_replay_route_count = clear_har_replay_routes(cdp_target_id)?;
                }
                let replay = load_har_replay(path, har.filters.limit, har.missing_policy)?;
                har_bytes = replay.source_bytes;
                replay_entry_count = replay.entry_count;
                skipped_entry_count = replay.skipped_count;
                replay_route_count = replay.rules.len();
                missing_abort_route_installed = replay.missing_abort_route_installed;

                let ensure =
                    synapse_a11y::fetch_interception_ensure(&endpoint, cdp_target_id, Vec::new())
                        .await
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{HAR_TOOL} raw CDP Fetch interception failed: {error}"),
                            )
                        })?;
                for rule in replay.rules {
                    let mut status =
                        synapse_a11y::fetch_route_add(cdp_target_id, rule).map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{HAR_TOOL} raw CDP Fetch route add failed: {error}"),
                            )
                        })?;
                    status.newly_armed = ensure.newly_armed;
                }
            }
            BrowserNetworkHarOperation::ClearReplay => {
                cleared_replay_route_count = clear_har_replay_routes(cdp_target_id)?;
                stop_fetch_if_no_routes(cdp_target_id).await?;
            }
        }

        let route_rules = synapse_a11y::fetch_route_rules(cdp_target_id).unwrap_or_default();
        let fetch_status_raw = synapse_a11y::fetch_interception_status(cdp_target_id);
        let fetch_status = browser_route_fetch_status_from_a11y(
            fetch_status_raw,
            synapse_a11y::fetch_interception_status(cdp_target_id).is_some(),
        );
        let routes = route_rules
            .iter()
            .map(browser_route_rule_to_wire)
            .collect::<Vec<_>>();

        tracing::info!(
            code = "CDP_BACKGROUND_NETWORK_HAR",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?har.operation,
            recorded_entry_count,
            skipped_entry_count,
            replay_entry_count,
            replay_route_count,
            cleared_replay_route_count,
            missing_abort_route_installed,
            "readback=HAR record/replay(browser_network_har) outcome=har_operation_returned"
        );

        Ok(BrowserNetworkHarResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            operation: har.operation,
            path: har.path_display.clone(),
            filters: (har.operation == BrowserNetworkHarOperation::Record)
                .then(|| har.to_record_filters_wire()),
            missing_policy: (har.operation == BrowserNetworkHarOperation::Replay)
                .then_some(har.missing_policy),
            capture_newly_armed,
            recorded_entry_count,
            skipped_entry_count,
            replay_entry_count,
            replay_route_count,
            cleared_replay_route_count,
            missing_abort_route_installed,
            har_bytes,
            route_count: routes.len(),
            routes,
            fetch_status,
            readback_backend: match har.operation {
                BrowserNetworkHarOperation::Record => {
                    "Network event buffer + Network.getResponseBody(browser_network_har)"
                }
                BrowserNetworkHarOperation::Replay | BrowserNetworkHarOperation::ClearReplay => {
                    "HAR file + Fetch fulfill routes(browser_network_har)"
                }
            }
            .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_network_request_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        request: &NormalizedBrowserNetworkRequestParams,
    ) -> Result<BrowserNetworkRequestResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(REQUEST_TOOL, window_hwnd));
        };
        let capture = synapse_a11y::network_capture_ensure(
            &endpoint,
            cdp_target_id,
            synapse_a11y::DEFAULT_NETWORK_BUFFER_CAPACITY,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{REQUEST_TOOL} raw CDP network capture failed: {error}"),
            )
        })?;
        let read = synapse_a11y::network_capture_read(
            cdp_target_id,
            &synapse_a11y::CdpNetworkReadFilter {
                request_id: Some(request.request_id.as_str()),
                max: 1,
                ..Default::default()
            },
        )
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{REQUEST_TOOL} network capture was not armed for target {cdp_target_id}"),
            )
        })?;
        let Some(entry) = read.entries.into_iter().next() else {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{REQUEST_TOOL} request_id {:?} is not present in the target network buffer",
                    request.request_id
                ),
            ));
        };
        let request_post_data = if request.include_post_data
            && entry.request_has_post_data.unwrap_or(false)
        {
            Some(
                synapse_a11y::network_request_post_data(cdp_target_id, &entry.request_id)
                    .await
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "{REQUEST_TOOL} raw CDP request post data failed request_id={}: {error}",
                                entry.request_id
                            ),
                        )
                    })
                    .map(browser_network_post_data_to_wire)?,
            )
        } else {
            None
        };
        let response_body = if request.include_body {
            require_response_body_available(&entry)?;
            Some(
                synapse_a11y::network_response_body(cdp_target_id, &entry.request_id)
                    .await
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "{REQUEST_TOOL} raw CDP response body failed request_id={}: {error}",
                                entry.request_id
                            ),
                        )
                    })
                    .map(browser_network_response_body_to_wire)?,
            )
        } else {
            None
        };
        tracing::info!(
            code = "CDP_BACKGROUND_NETWORK_REQUEST",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            request_id = %entry.request_id,
            include_body = request.include_body,
            response_body_returned = response_body.is_some(),
            request_post_data_returned = request_post_data.is_some(),
            "readback=Network.getResponseBody(browser_network_request) outcome=request_returned"
        );
        Ok(BrowserNetworkRequestResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            capture_newly_armed: capture.newly_armed,
            request_id: entry.request_id.clone(),
            include_body: request.include_body,
            include_post_data: request.include_post_data,
            entry: browser_network_request_detail_to_wire(&entry),
            request_post_data,
            response_body,
            readback_backend:
                "Network event buffer + Network.getResponseBody(browser_network_request)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_network_overrides_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        overrides: &NormalizedBrowserNetworkOverridesParams,
    ) -> Result<BrowserNetworkOverridesResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(OVERRIDES_TOOL, window_hwnd));
        };
        let (status, cleared) = match overrides.operation {
            BrowserNetworkOverridesOperation::Set => {
                let status = synapse_a11y::network_overrides_apply(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpNetworkOverrideConfig {
                        headers: overrides.headers.clone(),
                        user_agent: overrides.user_agent.clone(),
                    },
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{OVERRIDES_TOOL} raw CDP override apply failed: {error}"),
                    )
                })?;
                (Some(status), false)
            }
            BrowserNetworkOverridesOperation::Get => {
                (synapse_a11y::network_overrides_status(cdp_target_id), false)
            }
            BrowserNetworkOverridesOperation::Clear => {
                let status = synapse_a11y::network_overrides_clear(cdp_target_id)
                    .await
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!("{OVERRIDES_TOOL} raw CDP override clear failed: {error}"),
                        )
                    })?;
                let cleared = status.is_some();
                (status, cleared)
            }
        };
        tracing::info!(
            code = "CDP_BACKGROUND_NETWORK_OVERRIDES",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?overrides.operation,
            override_active = status.is_some(),
            cleared,
            "readback=Network.setExtraHTTPHeaders+Emulation.setUserAgentOverride outcome=overrides_returned"
        );
        Ok(browser_network_overrides_response(
            session_id,
            window_hwnd,
            endpoint,
            cdp_target_id,
            overrides.operation,
            status,
            cleared,
        ))
    }

    #[cfg(not(windows))]
    async fn browser_network_overrides_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _overrides: &NormalizedBrowserNetworkOverridesParams,
    ) -> Result<BrowserNetworkOverridesResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_network_overrides is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_network_conditions_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        conditions: &NormalizedBrowserNetworkConditionsParams,
    ) -> Result<BrowserNetworkConditionsResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                return self
                    .browser_network_conditions_bridge_impl(
                        session_id,
                        window_hwnd,
                        cdp_target_id,
                        conditions,
                    )
                    .await;
            }
            return Err(browser_raw_cdp_required_error(CONDITIONS_TOOL, window_hwnd));
        };
        let result = match conditions.operation {
            BrowserNetworkConditionsOperation::Set => {
                let requested = conditions.requested.as_ref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "{CONDITIONS_TOOL} requested network conditions override missing for set operation"
                        ),
                    )
                })?;
                synapse_a11y::cdp_set_network_conditions(
                    &endpoint,
                    cdp_target_id,
                    synapse_a11y::CdpNetworkConditionsOverride {
                        offline: requested.offline,
                        latency_ms: requested.latency_ms,
                        download_throughput_bytes_per_sec: requested
                            .download_throughput_bytes_per_sec,
                        upload_throughput_bytes_per_sec: requested.upload_throughput_bytes_per_sec,
                        connection_type: requested.connection_type.clone(),
                    },
                )
                .await
            }
            BrowserNetworkConditionsOperation::Reset => {
                synapse_a11y::cdp_reset_network_conditions(&endpoint, cdp_target_id).await
            }
        }
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{CONDITIONS_TOOL} raw CDP network conditions failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_NETWORK_CONDITIONS",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?conditions.operation,
            online = result.readback.online,
            requested_offline = conditions.requested.as_ref().map(|requested| requested.offline),
            "readback=Network.emulateNetworkConditions+Runtime.evaluate outcome=network_state"
        );
        Ok(browser_network_conditions_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(windows)]
    async fn browser_network_conditions_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        conditions: &NormalizedBrowserNetworkConditionsParams,
    ) -> Result<BrowserNetworkConditionsResponse, ErrorData> {
        let operation = match conditions.operation {
            BrowserNetworkConditionsOperation::Set => "set",
            BrowserNetworkConditionsOperation::Reset => "reset",
        };
        let requested = conditions.requested.as_ref();
        let result = crate::chrome_debugger_bridge::network_conditions(
            crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                operation,
                offline: requested.map(|value| value.offline),
                latency_ms: requested.map(|value| value.latency_ms),
                download_throughput_bytes_per_sec: requested
                    .map(|value| value.download_throughput_bytes_per_sec),
                upload_throughput_bytes_per_sec: requested
                    .map(|value| value.upload_throughput_bytes_per_sec),
                connection_type: requested.and_then(|value| value.connection_type.as_deref()),
                wait_timeout_ms: 10_000,
            },
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{CONDITIONS_TOOL} normal Chrome bridge network conditions failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        tracing::info!(
            code = "CHROME_BRIDGE_NETWORK_CONDITIONS",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id,
            operation = ?conditions.operation,
            online = result.network.online,
            requested_offline = conditions.requested.as_ref().map(|requested| requested.offline),
            "readback=chrome.debugger.Network+chrome.scripting.executeScript outcome=network_state"
        );
        Ok(browser_network_conditions_bridge_response(
            session_id,
            window_hwnd,
            result,
        ))
    }

    #[cfg(not(windows))]
    async fn browser_network_conditions_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _conditions: &NormalizedBrowserNetworkConditionsParams,
    ) -> Result<BrowserNetworkConditionsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_network_conditions is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_route_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        route: &NormalizedBrowserRouteParams,
    ) -> Result<BrowserRouteResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(ROUTE_TOOL, window_hwnd));
        };
        let mut route_removed = false;
        let mut cleared_count = 0usize;
        let fetch_status = match route.operation {
            BrowserRouteOperation::AddFulfill
            | BrowserRouteOperation::AddAbort
            | BrowserRouteOperation::AddContinue => {
                let ensure =
                    synapse_a11y::fetch_interception_ensure(&endpoint, cdp_target_id, Vec::new())
                        .await
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{ROUTE_TOOL} raw CDP Fetch interception failed: {error}"),
                            )
                        })?;
                let normalized_route = route.route.clone().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("{ROUTE_TOOL} add_fulfill route was not normalized"),
                    )
                })?;
                let mut status = synapse_a11y::fetch_route_add(cdp_target_id, normalized_route)
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!("{ROUTE_TOOL} raw CDP Fetch route add failed: {error}"),
                        )
                    })?;
                status.newly_armed = ensure.newly_armed;
                browser_route_fetch_status_from_a11y(Some(status), true)
            }
            BrowserRouteOperation::Remove => {
                if let Some(route_id) = route.route_id.as_deref() {
                    route_removed = synapse_a11y::fetch_route_remove(cdp_target_id, route_id)
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{ROUTE_TOOL} raw CDP Fetch route remove failed: {error}"),
                            )
                        })?;
                }
                let routes = synapse_a11y::fetch_route_rules(cdp_target_id).unwrap_or_default();
                let status = synapse_a11y::fetch_interception_status(cdp_target_id);
                if routes.is_empty() && status.is_some() {
                    synapse_a11y::fetch_interception_stop(cdp_target_id)
                        .await
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{ROUTE_TOOL} raw CDP Fetch disable failed: {error}"),
                            )
                        })?;
                    browser_route_fetch_status_from_a11y(None, false)
                } else {
                    browser_route_fetch_status_from_a11y(status, !routes.is_empty())
                }
            }
            BrowserRouteOperation::Clear => {
                cleared_count =
                    synapse_a11y::fetch_route_clear(cdp_target_id).map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!("{ROUTE_TOOL} raw CDP Fetch route clear failed: {error}"),
                        )
                    })?;
                if synapse_a11y::fetch_interception_status(cdp_target_id).is_some() {
                    synapse_a11y::fetch_interception_stop(cdp_target_id)
                        .await
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!("{ROUTE_TOOL} raw CDP Fetch disable failed: {error}"),
                            )
                        })?;
                }
                browser_route_fetch_status_from_a11y(None, false)
            }
            BrowserRouteOperation::List => {
                let status = synapse_a11y::fetch_interception_status(cdp_target_id);
                let fetch_armed = status.is_some();
                browser_route_fetch_status_from_a11y(status, fetch_armed)
            }
        };
        let routes = synapse_a11y::fetch_route_rules(cdp_target_id)
            .unwrap_or_default()
            .iter()
            .map(browser_route_rule_to_wire)
            .collect::<Vec<_>>();
        tracing::info!(
            code = "CDP_BACKGROUND_BROWSER_ROUTE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            operation = ?route.operation,
            route_id = route.route_id.as_deref(),
            route_count = routes.len(),
            route_removed,
            cleared_count,
            "readback=Fetch.fulfillRequest(browser_route) outcome=route_operation_returned"
        );
        Ok(BrowserRouteResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            operation: route.operation,
            route_id: route.route_id.clone(),
            route_removed,
            cleared_count,
            route_count: routes.len(),
            routes,
            fetch_status,
            readback_backend: "Fetch interception routes(browser_route)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_route_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _route: &NormalizedBrowserRouteParams,
    ) -> Result<BrowserRouteResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_route is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_network_request_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _request: &NormalizedBrowserNetworkRequestParams,
    ) -> Result<BrowserNetworkRequestResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_network_request is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_network_requests_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _filters: &NormalizedBrowserNetworkRequestsParams,
    ) -> Result<BrowserNetworkRequestsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_network_requests is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_network_websockets_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _filters: &NormalizedBrowserNetworkWebSocketsParams,
    ) -> Result<BrowserNetworkWebSocketsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_network_websockets is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_network_har_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _har: &NormalizedBrowserNetworkHarParams,
    ) -> Result<BrowserNetworkHarResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_network_har is only available on Windows in this build",
        ))
    }
}

impl NormalizedBrowserNetworkRequestsParams {
    fn to_wire(&self) -> BrowserNetworkRequestFilters {
        BrowserNetworkRequestFilters {
            since_seq: self.since_seq,
            limit: self.limit,
            url_contains: self.url_contains.clone(),
            url_regex: self.url_regex_pattern.clone(),
            resource_type: self.resource_type.clone(),
            status_min: self.status_min,
            status_max: self.status_max,
        }
    }
}

impl NormalizedBrowserNetworkWebSocketsParams {
    fn to_wire(&self) -> BrowserNetworkWebSocketFilters {
        BrowserNetworkWebSocketFilters {
            since_seq: self.since_seq,
            limit: self.limit,
            request_id: self.request_id.clone(),
            url_contains: self.url_contains.clone(),
        }
    }
}

impl NormalizedBrowserNetworkHarParams {
    fn to_record_filters_wire(&self) -> BrowserNetworkHarFilters {
        BrowserNetworkHarFilters {
            since_seq: self.filters.since_seq,
            limit: self.filters.limit,
            url_contains: self.filters.url_contains.clone(),
            url_regex: self.filters.url_regex_pattern.clone(),
            resource_type: self.filters.resource_type.clone(),
            status_min: self.filters.status_min,
            status_max: self.filters.status_max,
            include_bodies: self.include_bodies,
        }
    }
}

fn default_true() -> bool {
    true
}

fn validate_browser_network_overrides_params(
    params: &BrowserNetworkOverridesParams,
) -> Result<NormalizedBrowserNetworkOverridesParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    match params.operation {
        BrowserNetworkOverridesOperation::Set => {
            let headers = validate_route_headers(&params.headers)?;
            let user_agent = validate_network_user_agent(params.user_agent.as_deref())?;
            Ok(NormalizedBrowserNetworkOverridesParams {
                operation: params.operation,
                headers,
                user_agent,
            })
        }
        BrowserNetworkOverridesOperation::Get | BrowserNetworkOverridesOperation::Clear => {
            if !params.headers.is_empty() || params.user_agent.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "{OVERRIDES_TOOL} headers and user_agent are only valid for operation=set"
                    ),
                ));
            }
            Ok(NormalizedBrowserNetworkOverridesParams {
                operation: params.operation,
                headers: Vec::new(),
                user_agent: None,
            })
        }
    }
}

fn validate_browser_network_conditions_params(
    params: &BrowserNetworkConditionsParams,
) -> Result<NormalizedBrowserNetworkConditionsParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if params.operation == BrowserNetworkConditionsOperation::Reset {
        if params.offline.is_some()
            || params.latency_ms.is_some()
            || params.download_throughput_bytes_per_sec.is_some()
            || params.upload_throughput_bytes_per_sec.is_some()
            || params.connection_type.is_some()
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{CONDITIONS_TOOL} offline, latency_ms, throughput and connection_type are only valid for operation=set"
                ),
            ));
        }
        return Ok(NormalizedBrowserNetworkConditionsParams {
            operation: BrowserNetworkConditionsOperation::Reset,
            requested: None,
        });
    }

    let offline = params.offline.unwrap_or(false);
    let latency_ms = params.latency_ms.unwrap_or(0.0);
    let download_throughput_bytes_per_sec =
        params.download_throughput_bytes_per_sec.unwrap_or(-1.0);
    let upload_throughput_bytes_per_sec = params.upload_throughput_bytes_per_sec.unwrap_or(-1.0);

    validate_network_latency(latency_ms)?;
    validate_network_throughput(
        "download_throughput_bytes_per_sec",
        download_throughput_bytes_per_sec,
    )?;
    validate_network_throughput(
        "upload_throughput_bytes_per_sec",
        upload_throughput_bytes_per_sec,
    )?;
    if let Some(connection_type) = params.connection_type.as_deref() {
        validate_network_connection_type(connection_type)?;
    }
    if !offline
        && latency_ms == 0.0
        && download_throughput_bytes_per_sec == -1.0
        && upload_throughput_bytes_per_sec == -1.0
        && params.connection_type.is_none()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{CONDITIONS_TOOL} operation=set requires offline=true, latency_ms, throughput and/or connection_type; use operation=reset to restore defaults"
            ),
        ));
    }

    Ok(NormalizedBrowserNetworkConditionsParams {
        operation: BrowserNetworkConditionsOperation::Set,
        requested: Some(BrowserNetworkConditionsOverride {
            offline,
            latency_ms,
            download_throughput_bytes_per_sec,
            upload_throughput_bytes_per_sec,
            connection_type: params.connection_type.clone(),
        }),
    })
}

fn validate_network_latency(value: f64) -> Result<(), ErrorData> {
    if value.is_finite() && (0.0..=synapse_a11y::CDP_NETWORK_MAX_LATENCY_MS).contains(&value) {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{CONDITIONS_TOOL} latency_ms must be finite and in 0..={}",
                synapse_a11y::CDP_NETWORK_MAX_LATENCY_MS
            ),
        ))
    }
}

fn validate_network_throughput(field: &str, value: f64) -> Result<(), ErrorData> {
    if value.is_finite()
        && (value == -1.0
            || (0.0..=synapse_a11y::CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC).contains(&value))
    {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{CONDITIONS_TOOL} {field} must be -1 to disable throttling or finite in 0..={}",
                synapse_a11y::CDP_NETWORK_MAX_THROUGHPUT_BYTES_PER_SEC
            ),
        ))
    }
}

fn validate_network_connection_type(value: &str) -> Result<(), ErrorData> {
    if matches!(
        value,
        "none"
            | "cellular2g"
            | "cellular3g"
            | "cellular4g"
            | "bluetooth"
            | "ethernet"
            | "wifi"
            | "wimax"
            | "other"
    ) {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{CONDITIONS_TOOL} connection_type must be one of none, cellular2g, cellular3g, cellular4g, bluetooth, ethernet, wifi, wimax or other"
            ),
        ))
    }
}

fn validate_network_user_agent(value: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{OVERRIDES_TOOL} user_agent must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{OVERRIDES_TOOL} user_agent must not contain leading or trailing whitespace"),
        ));
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{OVERRIDES_TOOL} user_agent must not contain line breaks or NUL"),
        ));
    }
    if value.chars().count() > MAX_NETWORK_USER_AGENT_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{OVERRIDES_TOOL} user_agent must be at most {MAX_NETWORK_USER_AGENT_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_browser_route_params(
    params: &BrowserRouteParams,
) -> Result<NormalizedBrowserRouteParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let route_id = match params.operation {
        BrowserRouteOperation::AddFulfill
        | BrowserRouteOperation::AddAbort
        | BrowserRouteOperation::AddContinue => params
            .route_id
            .as_deref()
            .map(validate_route_id)
            .transpose()?
            .unwrap_or_else(generate_route_id),
        BrowserRouteOperation::Remove => {
            validate_route_id(params.route_id.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{ROUTE_TOOL} route_id is required for remove"),
                )
            })?)?
        }
        BrowserRouteOperation::Clear | BrowserRouteOperation::List => params
            .route_id
            .as_deref()
            .map(validate_route_id)
            .transpose()?
            .unwrap_or_default(),
    };
    let route_id = if route_id.is_empty() {
        None
    } else {
        Some(route_id)
    };
    let route = match params.operation {
        BrowserRouteOperation::AddFulfill => Some(normalize_route_fulfill(
            params,
            route_id.clone().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{ROUTE_TOOL} add_fulfill missing generated route_id"),
                )
            })?,
        )?),
        BrowserRouteOperation::AddAbort => Some(normalize_route_abort(
            params,
            route_id.clone().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{ROUTE_TOOL} add_abort missing generated route_id"),
                )
            })?,
        )?),
        BrowserRouteOperation::AddContinue => Some(normalize_route_continue(
            params,
            route_id.clone().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{ROUTE_TOOL} add_continue missing generated route_id"),
                )
            })?,
        )?),
        BrowserRouteOperation::Remove
        | BrowserRouteOperation::Clear
        | BrowserRouteOperation::List => None,
    };
    Ok(NormalizedBrowserRouteParams {
        operation: params.operation,
        route_id,
        route,
    })
}

fn normalize_route_fulfill(
    params: &BrowserRouteParams,
    route_id: String,
) -> Result<synapse_a11y::CdpFetchRouteRule, ErrorData> {
    let (url, resource_type) = normalize_route_match(params, "add_fulfill")?;
    let status = params.status.unwrap_or(200);
    if !(100..=599).contains(&status) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} status must be 100..=599"),
        ));
    }
    if params.error_reason.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} error_reason is only valid for add_abort"),
        ));
    }
    let response_phrase = validate_route_response_phrase(params.response_phrase.as_deref())?;
    let headers = validate_route_headers(&params.headers)?;
    let body_base64 = normalize_route_body(params)?;
    Ok(synapse_a11y::CdpFetchRouteRule {
        id: route_id,
        url,
        match_kind: match params.match_kind {
            BrowserRouteMatchKind::Glob => synapse_a11y::CdpFetchRouteMatchKind::Glob,
            BrowserRouteMatchKind::Regex => synapse_a11y::CdpFetchRouteMatchKind::Regex,
        },
        method: None,
        resource_type,
        action: synapse_a11y::CdpFetchRouteAction::Fulfill(synapse_a11y::CdpFetchRouteFulfill {
            status,
            response_phrase,
            headers,
            body_base64,
        }),
    })
}

fn normalize_route_abort(
    params: &BrowserRouteParams,
    route_id: String,
) -> Result<synapse_a11y::CdpFetchRouteRule, ErrorData> {
    let (url, resource_type) = normalize_route_match(params, "add_abort")?;
    if params.status.is_some()
        || params.response_phrase.is_some()
        || !params.headers.is_empty()
        || params.body.is_some()
        || params.body_base64.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} status, response_phrase, headers, body, and body_base64 are only valid for add_fulfill"
            ),
        ));
    }
    let error_reason = params.error_reason.unwrap_or_default();
    Ok(synapse_a11y::CdpFetchRouteRule {
        id: route_id,
        url,
        match_kind: match params.match_kind {
            BrowserRouteMatchKind::Glob => synapse_a11y::CdpFetchRouteMatchKind::Glob,
            BrowserRouteMatchKind::Regex => synapse_a11y::CdpFetchRouteMatchKind::Regex,
        },
        method: None,
        resource_type,
        action: synapse_a11y::CdpFetchRouteAction::Abort(synapse_a11y::CdpFetchRouteAbort {
            error_reason: error_reason.as_cdp_str().to_owned(),
        }),
    })
}

fn normalize_route_continue(
    params: &BrowserRouteParams,
    route_id: String,
) -> Result<synapse_a11y::CdpFetchRouteRule, ErrorData> {
    let (url, resource_type) = normalize_route_match(params, "add_continue")?;
    if params.status.is_some()
        || params.response_phrase.is_some()
        || !params.headers.is_empty()
        || params.body.is_some()
        || params.body_base64.is_some()
        || params.error_reason.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} status, response_phrase, headers, body, body_base64, and error_reason are not valid for add_continue"
            ),
        ));
    }
    if params.continue_url.is_none()
        && params.continue_method.is_none()
        && params.continue_headers.is_empty()
        && params.continue_post_data.is_none()
        && params.continue_post_data_base64.is_none()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} add_continue requires at least one override"),
        ));
    }
    let continue_url = params
        .continue_url
        .as_deref()
        .map(validate_route_url)
        .transpose()?;
    let continue_method = params
        .continue_method
        .as_deref()
        .map(validate_route_method)
        .transpose()?;
    let headers = validate_route_headers(&params.continue_headers)?;
    let post_data_base64 = normalize_route_continue_post_data(params)?;
    Ok(synapse_a11y::CdpFetchRouteRule {
        id: route_id,
        url,
        match_kind: match params.match_kind {
            BrowserRouteMatchKind::Glob => synapse_a11y::CdpFetchRouteMatchKind::Glob,
            BrowserRouteMatchKind::Regex => synapse_a11y::CdpFetchRouteMatchKind::Regex,
        },
        method: None,
        resource_type,
        action: synapse_a11y::CdpFetchRouteAction::Continue(synapse_a11y::CdpFetchRouteContinue {
            url: continue_url,
            method: continue_method,
            headers,
            post_data_base64,
        }),
    })
}

fn normalize_route_match(
    params: &BrowserRouteParams,
    operation: &str,
) -> Result<(String, Option<String>), ErrorData> {
    let url = validate_route_url(params.url.as_deref().ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} url is required for {operation}"),
        )
    })?)?;
    if matches!(params.match_kind, BrowserRouteMatchKind::Regex) {
        regex::Regex::new(&url).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{ROUTE_TOOL} url regex is invalid: {error}"),
            )
        })?;
    }
    let resource_type =
        validate_resource_type_for_tool(ROUTE_TOOL, params.resource_type.as_deref())?;
    Ok((url, resource_type))
}

fn validate_browser_network_requests_params(
    params: &BrowserNetworkRequestsParams,
) -> Result<NormalizedBrowserNetworkRequestsParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let limit = params.limit.unwrap_or(DEFAULT_NETWORK_REQUEST_LIMIT);
    if !(1..=MAX_NETWORK_REQUEST_LIMIT).contains(&limit) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{REQUESTS_TOOL} limit must be 1..={MAX_NETWORK_REQUEST_LIMIT}"),
        ));
    }
    if params.url_contains.is_some() && params.url_regex.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{REQUESTS_TOOL} accepts url_contains or url_regex, not both"),
        ));
    }
    let url_contains = validate_text_filter("url_contains", params.url_contains.as_deref())?;
    let url_regex_pattern = validate_text_filter("url_regex", params.url_regex.as_deref())?;
    let url_regex = url_regex_pattern
        .as_deref()
        .map(|pattern| {
            regex::Regex::new(pattern).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{REQUESTS_TOOL} url_regex is invalid: {error}"),
                )
            })
        })
        .transpose()?;
    let resource_type =
        validate_resource_type_for_tool(REQUESTS_TOOL, params.resource_type.as_deref())?;
    validate_status_bound_for_tool(REQUESTS_TOOL, "status_min", params.status_min)?;
    validate_status_bound_for_tool(REQUESTS_TOOL, "status_max", params.status_max)?;
    if let (Some(min), Some(max)) = (params.status_min, params.status_max)
        && min > max
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{REQUESTS_TOOL} status_min must be <= status_max"),
        ));
    }
    Ok(NormalizedBrowserNetworkRequestsParams {
        since_seq: params.since_seq,
        limit,
        url_contains,
        url_regex_pattern,
        url_regex,
        resource_type,
        status_min: params.status_min,
        status_max: params.status_max,
    })
}

fn validate_browser_network_websockets_params(
    params: &BrowserNetworkWebSocketsParams,
) -> Result<NormalizedBrowserNetworkWebSocketsParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let limit = params.limit.unwrap_or(DEFAULT_NETWORK_REQUEST_LIMIT);
    if !(1..=MAX_NETWORK_REQUEST_LIMIT).contains(&limit) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{WEBSOCKETS_TOOL} limit must be 1..={MAX_NETWORK_REQUEST_LIMIT}"),
        ));
    }
    let request_id = params
        .request_id
        .as_deref()
        .map(|request_id| validate_request_id_for_tool(WEBSOCKETS_TOOL, request_id))
        .transpose()?;
    let url_contains = validate_text_filter_for_tool(
        WEBSOCKETS_TOOL,
        "url_contains",
        params.url_contains.as_deref(),
    )?;
    Ok(NormalizedBrowserNetworkWebSocketsParams {
        since_seq: params.since_seq,
        limit,
        request_id,
        url_contains,
    })
}

fn validate_browser_network_har_params(
    params: &BrowserNetworkHarParams,
) -> Result<NormalizedBrowserNetworkHarParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let path = match params.operation {
        BrowserNetworkHarOperation::Record | BrowserNetworkHarOperation::Replay => Some(
            validate_har_path(params.path.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{HAR_TOOL} path is required for {:?}", params.operation),
                )
            })?)?,
        ),
        BrowserNetworkHarOperation::ClearReplay => {
            if params.path.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{HAR_TOOL} path is only valid for record and replay"),
                ));
            }
            None
        }
    };
    if params.operation != BrowserNetworkHarOperation::Record {
        if params.since_seq.is_some()
            || params.url_contains.is_some()
            || params.url_regex.is_some()
            || params.resource_type.is_some()
            || params.status_min.is_some()
            || params.status_max.is_some()
            || params.include_bodies.is_some()
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{HAR_TOOL} capture filters and include_bodies are only valid for record"),
            ));
        }
    }
    if params.operation != BrowserNetworkHarOperation::Replay {
        if params.missing_policy.is_some() || params.clear_existing_replay.is_some() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{HAR_TOOL} missing_policy and clear_existing_replay are only valid for replay"
                ),
            ));
        }
        if params.operation == BrowserNetworkHarOperation::ClearReplay && params.limit.is_some() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{HAR_TOOL} limit is only valid for record and replay"),
            ));
        }
    }
    let limit = match params.operation {
        BrowserNetworkHarOperation::Record => params.limit.unwrap_or(DEFAULT_NETWORK_REQUEST_LIMIT),
        BrowserNetworkHarOperation::Replay => params.limit.unwrap_or(MAX_HAR_REPLAY_ENTRIES),
        BrowserNetworkHarOperation::ClearReplay => 1,
    };
    let max_limit = match params.operation {
        BrowserNetworkHarOperation::Record => MAX_NETWORK_REQUEST_LIMIT,
        BrowserNetworkHarOperation::Replay => MAX_HAR_REPLAY_ENTRIES,
        BrowserNetworkHarOperation::ClearReplay => 1,
    };
    if !(1..=max_limit).contains(&limit) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} limit must be 1..={max_limit}"),
        ));
    }
    if params.url_contains.is_some() && params.url_regex.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} accepts url_contains or url_regex, not both"),
        ));
    }
    let url_contains =
        validate_text_filter_for_tool(HAR_TOOL, "url_contains", params.url_contains.as_deref())?;
    let url_regex_pattern =
        validate_text_filter_for_tool(HAR_TOOL, "url_regex", params.url_regex.as_deref())?;
    let url_regex = url_regex_pattern
        .as_deref()
        .map(|pattern| {
            regex::Regex::new(pattern).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{HAR_TOOL} url_regex is invalid: {error}"),
                )
            })
        })
        .transpose()?;
    let resource_type = validate_resource_type_for_tool(HAR_TOOL, params.resource_type.as_deref())?;
    validate_status_bound_for_tool(HAR_TOOL, "status_min", params.status_min)?;
    validate_status_bound_for_tool(HAR_TOOL, "status_max", params.status_max)?;
    if let (Some(min), Some(max)) = (params.status_min, params.status_max)
        && min > max
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} status_min must be <= status_max"),
        ));
    }
    let (path, path_display) = match path {
        Some((path, display)) => (Some(path), Some(display)),
        None => (None, None),
    };
    Ok(NormalizedBrowserNetworkHarParams {
        operation: params.operation,
        path,
        path_display,
        filters: NormalizedBrowserNetworkRequestsParams {
            since_seq: params.since_seq,
            limit,
            url_contains,
            url_regex_pattern,
            url_regex,
            resource_type,
            status_min: params.status_min,
            status_max: params.status_max,
        },
        include_bodies: params.include_bodies.unwrap_or(true),
        missing_policy: params.missing_policy.unwrap_or_default(),
        clear_existing_replay: params.clear_existing_replay.unwrap_or(true),
    })
}

fn validate_har_path(path: &str) -> Result<(PathBuf, String), ErrorData> {
    if path.is_empty() || path.trim() != path {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} path must be non-empty without leading or trailing whitespace"),
        ));
    }
    if path.contains('\0') || path.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} path must not contain control characters"),
        ));
    }
    if path.chars().count() > MAX_HAR_PATH_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} path must be at most {MAX_HAR_PATH_CHARS} Unicode scalar values"),
        ));
    }
    Ok((PathBuf::from(path), path.to_owned()))
}

fn validate_browser_network_request_params(
    params: &BrowserNetworkRequestParams,
) -> Result<NormalizedBrowserNetworkRequestParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let request_id = validate_request_id(&params.request_id)?;
    Ok(NormalizedBrowserNetworkRequestParams {
        request_id,
        include_body: params.include_body,
        include_post_data: params.include_post_data,
    })
}

fn validate_request_id(request_id: &str) -> Result<String, ErrorData> {
    validate_request_id_for_tool(REQUEST_TOOL, request_id)
}

fn validate_request_id_for_tool(tool: &str, request_id: &str) -> Result<String, ErrorData> {
    if request_id.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} request_id must not be empty"),
        ));
    }
    if request_id.trim() != request_id {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} request_id must not contain leading or trailing whitespace"),
        ));
    }
    if request_id.contains('\0') || request_id.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} request_id must not contain control characters"),
        ));
    }
    if request_id.chars().count() > MAX_NETWORK_REQUEST_ID_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} request_id must be at most {MAX_NETWORK_REQUEST_ID_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(request_id.to_owned())
}

fn validate_text_filter(field: &str, value: Option<&str>) -> Result<Option<String>, ErrorData> {
    validate_text_filter_for_tool(REQUESTS_TOOL, field, value)
}

fn validate_text_filter_for_tool(
    tool: &str,
    field: &str,
    value: Option<&str>,
) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not be empty"),
        ));
    }
    if value.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not contain NUL"),
        ));
    }
    if value.chars().count() > MAX_NETWORK_FILTER_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} {field} must be at most {MAX_NETWORK_FILTER_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_resource_type_for_tool(
    tool_name: &str,
    value: Option<&str>,
) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool_name} resource_type must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool_name} resource_type must not contain leading or trailing whitespace"),
        ));
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool_name} resource_type must not contain control characters"),
        ));
    }
    if value.chars().count() > MAX_NETWORK_RESOURCE_TYPE_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool_name} resource_type must be at most {MAX_NETWORK_RESOURCE_TYPE_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_route_id(route_id: &str) -> Result<String, ErrorData> {
    if route_id.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} route_id must not be empty"),
        ));
    }
    if route_id.trim() != route_id {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} route_id must not contain leading or trailing whitespace"),
        ));
    }
    if route_id.contains('\0') || route_id.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} route_id must not contain control characters"),
        ));
    }
    if route_id.chars().any(char::is_whitespace) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} route_id must not contain whitespace"),
        ));
    }
    if route_id.chars().count() > MAX_ROUTE_ID_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} route_id must be at most {MAX_ROUTE_ID_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(route_id.to_owned())
}

fn validate_route_url(url: &str) -> Result<String, ErrorData> {
    if url.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} url must not be empty"),
        ));
    }
    if url.trim() != url {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} url must not contain leading or trailing whitespace"),
        ));
    }
    if url.contains('\0') || url.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} url must not contain control characters"),
        ));
    }
    if url.chars().count() > MAX_ROUTE_URL_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} url must be at most {MAX_ROUTE_URL_CHARS} Unicode scalar values"),
        ));
    }
    Ok(url.to_owned())
}

fn validate_route_method(method: &str) -> Result<String, ErrorData> {
    if method.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} continue_method must not be empty"),
        ));
    }
    if method.trim() != method {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} continue_method must not contain leading or trailing whitespace"),
        ));
    }
    if method.bytes().any(|byte| {
        byte <= 0x20
            || byte >= 0x7f
            || matches!(
                byte,
                b'(' | b')'
                    | b'<'
                    | b'>'
                    | b'@'
                    | b','
                    | b';'
                    | b':'
                    | b'\\'
                    | b'"'
                    | b'/'
                    | b'['
                    | b']'
                    | b'?'
                    | b'='
                    | b'{'
                    | b'}'
            )
    }) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} continue_method {method:?} contains an invalid byte"),
        ));
    }
    Ok(method.to_owned())
}

fn validate_route_response_phrase(value: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.contains(['\r', '\n', '\0']) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} response_phrase must not contain line breaks or NUL"),
        ));
    }
    if value.chars().count() > MAX_ROUTE_RESPONSE_PHRASE_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} response_phrase must be at most {MAX_ROUTE_RESPONSE_PHRASE_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_route_headers(
    headers: &[BrowserRouteHeader],
) -> Result<Vec<(String, String)>, ErrorData> {
    if headers.len() > MAX_ROUTE_HEADER_COUNT {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} headers must contain at most {MAX_ROUTE_HEADER_COUNT} entries"),
        ));
    }
    headers
        .iter()
        .map(|header| {
            validate_route_header_name(&header.name)?;
            validate_route_header_value(&header.value)?;
            Ok((header.name.clone(), header.value.clone()))
        })
        .collect()
}

fn validate_route_header_name(value: &str) -> Result<(), ErrorData> {
    if value.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} header name must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} header name must not contain leading or trailing whitespace"),
        ));
    }
    if value.chars().count() > MAX_ROUTE_HEADER_NAME_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} header name must be at most {MAX_ROUTE_HEADER_NAME_CHARS} Unicode scalar values"
            ),
        ));
    }
    if value.bytes().any(|byte| {
        byte <= 0x20
            || byte >= 0x7f
            || matches!(
                byte,
                b'(' | b')'
                    | b'<'
                    | b'>'
                    | b'@'
                    | b','
                    | b';'
                    | b':'
                    | b'\\'
                    | b'"'
                    | b'/'
                    | b'['
                    | b']'
                    | b'?'
                    | b'='
                    | b'{'
                    | b'}'
            )
    }) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} header name {value:?} contains an invalid byte"),
        ));
    }
    Ok(())
}

fn validate_route_header_value(value: &str) -> Result<(), ErrorData> {
    if value.contains(['\r', '\n', '\0']) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} header value must not contain line breaks or NUL"),
        ));
    }
    if value.chars().count() > MAX_ROUTE_HEADER_VALUE_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} header value must be at most {MAX_ROUTE_HEADER_VALUE_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(())
}

fn normalize_route_body(params: &BrowserRouteParams) -> Result<Option<String>, ErrorData> {
    if params.body.is_some() && params.body_base64.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ROUTE_TOOL} accepts body or body_base64, not both"),
        ));
    }
    if let Some(body) = params.body.as_deref() {
        if body.chars().count() > MAX_ROUTE_BODY_CHARS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{ROUTE_TOOL} body must be at most {MAX_ROUTE_BODY_CHARS} Unicode scalar values"
                ),
            ));
        }
        return Ok(Some(BASE64_STANDARD.encode(body.as_bytes())));
    }
    if let Some(body_base64) = params.body_base64.as_deref() {
        if body_base64.contains('\0') || body_base64.chars().any(char::is_control) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{ROUTE_TOOL} body_base64 must not contain control characters"),
            ));
        }
        if body_base64.chars().count() > MAX_ROUTE_BODY_CHARS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{ROUTE_TOOL} body_base64 must be at most {MAX_ROUTE_BODY_CHARS} Unicode scalar values"
                ),
            ));
        }
        BASE64_STANDARD.decode(body_base64).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{ROUTE_TOOL} body_base64 is invalid: {error}"),
            )
        })?;
        return Ok(Some(body_base64.to_owned()));
    }
    Ok(None)
}

fn normalize_route_continue_post_data(
    params: &BrowserRouteParams,
) -> Result<Option<String>, ErrorData> {
    if params.continue_post_data.is_some() && params.continue_post_data_base64.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ROUTE_TOOL} accepts continue_post_data or continue_post_data_base64, not both"
            ),
        ));
    }
    if let Some(post_data) = params.continue_post_data.as_deref() {
        if post_data.chars().count() > MAX_ROUTE_BODY_CHARS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{ROUTE_TOOL} continue_post_data must be at most {MAX_ROUTE_BODY_CHARS} Unicode scalar values"
                ),
            ));
        }
        return Ok(Some(BASE64_STANDARD.encode(post_data.as_bytes())));
    }
    if let Some(post_data_base64) = params.continue_post_data_base64.as_deref() {
        if post_data_base64.contains('\0') || post_data_base64.chars().any(char::is_control) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{ROUTE_TOOL} continue_post_data_base64 must not contain control characters"
                ),
            ));
        }
        if post_data_base64.chars().count() > MAX_ROUTE_BODY_CHARS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{ROUTE_TOOL} continue_post_data_base64 must be at most {MAX_ROUTE_BODY_CHARS} Unicode scalar values"
                ),
            ));
        }
        BASE64_STANDARD.decode(post_data_base64).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{ROUTE_TOOL} continue_post_data_base64 is invalid: {error}"),
            )
        })?;
        return Ok(Some(post_data_base64.to_owned()));
    }
    Ok(None)
}

fn generate_route_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("route-{millis}-{}", std::process::id())
}

fn validate_status_bound_for_tool(
    tool: &str,
    field: &str,
    value: Option<i64>,
) -> Result<(), ErrorData> {
    if let Some(value) = value
        && !(0..=999).contains(&value)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must be 0..=999"),
        ));
    }
    Ok(())
}

fn filter_network_entries(
    entries: impl Iterator<Item = synapse_a11y::CdpNetworkEntry>,
    filters: &NormalizedBrowserNetworkRequestsParams,
) -> Vec<synapse_a11y::CdpNetworkEntry> {
    entries
        .filter(|entry| network_entry_matches(entry, filters))
        .collect()
}

fn network_entry_matches(
    entry: &synapse_a11y::CdpNetworkEntry,
    filters: &NormalizedBrowserNetworkRequestsParams,
) -> bool {
    if let Some(resource_type) = filters.resource_type.as_deref()
        && !entry
            .resource_type
            .as_deref()
            .is_some_and(|entry_type| entry_type.eq_ignore_ascii_case(resource_type))
    {
        return false;
    }
    let status = entry.response.as_ref().map(|response| response.status);
    if let Some(min) = filters.status_min
        && !status.is_some_and(|status| status >= min)
    {
        return false;
    }
    if let Some(max) = filters.status_max
        && !status.is_some_and(|status| status <= max)
    {
        return false;
    }
    if let Some(needle) = filters.url_contains.as_deref()
        && !entry
            .url
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains(&needle.to_lowercase())
    {
        return false;
    }
    if let Some(regex) = filters.url_regex.as_ref()
        && !entry.url.as_deref().is_some_and(|url| regex.is_match(url))
    {
        return false;
    }
    true
}

#[derive(Debug)]
struct HarRecordWriteResult {
    entry_count: usize,
    skipped_count: usize,
    bytes_written: u64,
}

#[derive(Debug)]
struct HarReplayLoadResult {
    entry_count: usize,
    skipped_count: usize,
    source_bytes: u64,
    missing_abort_route_installed: bool,
    rules: Vec<synapse_a11y::CdpFetchRouteRule>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarFile {
    log: HarLog,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarLog {
    #[serde(default = "har_version")]
    version: String,
    #[serde(default)]
    creator: HarCreator,
    #[serde(default)]
    entries: Vec<HarEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HarCreator {
    name: String,
    version: String,
}

impl Default for HarCreator {
    fn default() -> Self {
        Self {
            name: "Synapse".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarEntry {
    #[serde(rename = "startedDateTime", default = "har_epoch_datetime")]
    started_date_time: String,
    #[serde(default = "har_unknown_time")]
    time: f64,
    #[serde(default)]
    request: HarRequest,
    #[serde(default)]
    response: HarResponse,
    #[serde(default)]
    cache: Value,
    #[serde(default)]
    timings: HarTimings,
    #[serde(
        rename = "_synapseRequestId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    synapse_request_id: Option<String>,
    #[serde(
        rename = "_synapseResourceType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    synapse_resource_type: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarRequest {
    #[serde(default)]
    method: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "httpVersion", default = "har_http_version")]
    http_version: String,
    #[serde(default)]
    cookies: Vec<Value>,
    #[serde(default)]
    headers: Vec<HarHeader>,
    #[serde(rename = "queryString", default)]
    query_string: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_data: Option<HarPostData>,
    #[serde(rename = "headersSize", default = "har_size_unknown")]
    headers_size: i64,
    #[serde(rename = "bodySize", default = "har_size_unknown")]
    body_size: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarPostData {
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    params: Vec<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarResponse {
    #[serde(default)]
    status: i64,
    #[serde(rename = "statusText", default)]
    status_text: String,
    #[serde(rename = "httpVersion", default = "har_http_version")]
    http_version: String,
    #[serde(default)]
    cookies: Vec<Value>,
    #[serde(default)]
    headers: Vec<HarHeader>,
    #[serde(default)]
    content: HarContent,
    #[serde(rename = "redirectURL", default)]
    redirect_url: String,
    #[serde(rename = "headersSize", default = "har_size_unknown")]
    headers_size: i64,
    #[serde(rename = "bodySize", default = "har_size_unknown")]
    body_size: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarHeader {
    name: String,
    value: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HarContent {
    #[serde(default)]
    size: i64,
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encoding: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HarTimings {
    send: f64,
    wait: f64,
    receive: f64,
}

impl Default for HarTimings {
    fn default() -> Self {
        Self {
            send: 0.0,
            wait: 0.0,
            receive: 0.0,
        }
    }
}

async fn write_har_record(
    cdp_target_id: &str,
    path: &PathBuf,
    entries: &[synapse_a11y::CdpNetworkEntry],
    include_bodies: bool,
) -> Result<HarRecordWriteResult, ErrorData> {
    let mut har_entries = Vec::new();
    let mut skipped_count = 0usize;
    for entry in entries {
        let response_body = if include_bodies && entry.response_received && entry.loading_finished {
            synapse_a11y::network_response_body(cdp_target_id, &entry.request_id)
                .await
                .ok()
        } else {
            None
        };
        let request_post_data = if include_bodies && entry.request_has_post_data == Some(true) {
            synapse_a11y::network_request_post_data(cdp_target_id, &entry.request_id)
                .await
                .ok()
        } else {
            None
        };
        if let Some(har_entry) = har_entry_from_network(entry, response_body, request_post_data) {
            har_entries.push(har_entry);
        } else {
            skipped_count = skipped_count.saturating_add(1);
        }
    }

    let har = HarFile {
        log: HarLog {
            version: har_version(),
            creator: HarCreator::default(),
            entries: har_entries,
        },
    };
    let bytes = serde_json::to_vec_pretty(&har).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{HAR_TOOL} failed to serialize HAR: {error}"),
        )
    })?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{HAR_TOOL} failed to create HAR parent directory: {error}"),
            )
        })?;
    }
    fs::write(path, &bytes).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{HAR_TOOL} failed to write HAR {}: {error}", path.display()),
        )
    })?;
    Ok(HarRecordWriteResult {
        entry_count: har.log.entries.len(),
        skipped_count,
        bytes_written: bytes.len() as u64,
    })
}

fn load_har_replay(
    path: &PathBuf,
    limit: usize,
    missing_policy: BrowserNetworkHarMissingPolicy,
) -> Result<HarReplayLoadResult, ErrorData> {
    let metadata = fs::metadata(path).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} failed to stat HAR {}: {error}", path.display()),
        )
    })?;
    if metadata.len() > MAX_HAR_FILE_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} HAR file must be at most {MAX_HAR_FILE_BYTES} bytes"),
        ));
    }
    let raw = fs::read_to_string(path).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} failed to read HAR {}: {error}", path.display()),
        )
    })?;
    let har: HarFile = serde_json::from_str(&raw).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{HAR_TOOL} failed to parse HAR JSON: {error}"),
        )
    })?;
    let mut rules = Vec::new();
    let mut entry_count = 0usize;
    let mut skipped_count = 0usize;
    for (index, entry) in har.log.entries.iter().take(limit).enumerate() {
        match har_entry_to_route_rule(index, entry)? {
            Some(rule) => {
                entry_count = entry_count.saturating_add(1);
                rules.push(rule);
            }
            None => skipped_count = skipped_count.saturating_add(1),
        }
    }
    let mut missing_abort_route_installed = false;
    if missing_policy == BrowserNetworkHarMissingPolicy::Abort {
        rules.push(har_missing_abort_rule());
        missing_abort_route_installed = true;
    }
    Ok(HarReplayLoadResult {
        entry_count,
        skipped_count,
        source_bytes: metadata.len(),
        missing_abort_route_installed,
        rules,
    })
}

fn har_entry_from_network(
    entry: &synapse_a11y::CdpNetworkEntry,
    response_body: Option<synapse_a11y::CdpNetworkResponseBody>,
    request_post_data: Option<synapse_a11y::CdpNetworkRequestPostData>,
) -> Option<HarEntry> {
    let url = entry.url.clone()?;
    let method = entry.method.clone().unwrap_or_else(|| "GET".to_owned());
    let response = entry.response.as_ref();
    let started_date_time = har_datetime(entry.request_wall_time_ms);
    let time = match (entry.request_timestamp_s, entry.finished_timestamp_s) {
        (Some(start), Some(finish)) if finish >= start => (finish - start) * 1000.0,
        _ => -1.0,
    };
    let post_data = request_post_data.map(|post_data| HarPostData {
        mime_type: header_lookup(entry.request_headers.as_ref(), "content-type")
            .unwrap_or_default(),
        text: post_data.post_data,
        params: Vec::new(),
    });
    let response_status = response.map_or(0, |response| response.status);
    let response_status_text = response
        .map(|response| response.status_text.clone())
        .unwrap_or_default();
    let response_protocol = response
        .and_then(|response| response.protocol.clone())
        .unwrap_or_else(har_http_version);
    let response_headers = response
        .map(|response| har_headers_from_value(Some(&response.headers)))
        .unwrap_or_default();
    let content = har_content_from_response(response, response_body);
    let body_size = content.size;
    Some(HarEntry {
        started_date_time,
        time,
        request: HarRequest {
            method,
            url,
            http_version: har_http_version(),
            cookies: Vec::new(),
            headers: har_headers_from_value(entry.request_headers.as_ref()),
            query_string: Vec::new(),
            post_data,
            headers_size: -1,
            body_size: -1,
        },
        response: HarResponse {
            status: response_status,
            status_text: response_status_text,
            http_version: response_protocol,
            cookies: Vec::new(),
            headers: response_headers,
            content,
            redirect_url: String::new(),
            headers_size: -1,
            body_size,
        },
        cache: json!({}),
        timings: HarTimings::default(),
        synapse_request_id: Some(entry.request_id.clone()),
        synapse_resource_type: entry.resource_type.clone(),
    })
}

fn har_entry_to_route_rule(
    index: usize,
    entry: &HarEntry,
) -> Result<Option<synapse_a11y::CdpFetchRouteRule>, ErrorData> {
    if entry.request.url.is_empty()
        || entry.request.method.is_empty()
        || !(100..=599).contains(&entry.response.status)
    {
        return Ok(None);
    }
    let body_base64 = har_content_body_base64(&entry.response.content)?;
    Ok(Some(synapse_a11y::CdpFetchRouteRule {
        id: har_route_id(index, &entry.request.method, &entry.request.url),
        url: format!("^{}$", regex::escape(&entry.request.url)),
        match_kind: synapse_a11y::CdpFetchRouteMatchKind::Regex,
        method: Some(entry.request.method.clone()),
        resource_type: None,
        action: synapse_a11y::CdpFetchRouteAction::Fulfill(synapse_a11y::CdpFetchRouteFulfill {
            status: entry.response.status,
            response_phrase: (!entry.response.status_text.is_empty())
                .then(|| entry.response.status_text.clone()),
            headers: entry
                .response
                .headers
                .iter()
                .filter(|header| !har_replay_unsafe_response_header(&header.name))
                .map(|header| (header.name.clone(), header.value.clone()))
                .collect(),
            body_base64,
        }),
    }))
}

fn har_replay_unsafe_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "content-encoding"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn har_missing_abort_rule() -> synapse_a11y::CdpFetchRouteRule {
    synapse_a11y::CdpFetchRouteRule {
        id: HAR_REPLAY_MISS_ROUTE_ID.to_owned(),
        url: "*".to_owned(),
        match_kind: synapse_a11y::CdpFetchRouteMatchKind::Glob,
        method: None,
        resource_type: None,
        action: synapse_a11y::CdpFetchRouteAction::Abort(synapse_a11y::CdpFetchRouteAbort {
            error_reason: BrowserRouteErrorReason::BlockedByClient
                .as_cdp_str()
                .to_owned(),
        }),
    }
}

fn clear_har_replay_routes(cdp_target_id: &str) -> Result<usize, ErrorData> {
    let routes = synapse_a11y::fetch_route_rules(cdp_target_id).unwrap_or_default();
    let route_ids = routes
        .into_iter()
        .filter(|route| route.id.starts_with(HAR_REPLAY_ROUTE_PREFIX))
        .map(|route| route.id)
        .collect::<Vec<_>>();
    let mut removed = 0usize;
    for route_id in route_ids {
        if synapse_a11y::fetch_route_remove(cdp_target_id, &route_id).map_err(|error| {
            mcp_error(
                error.code(),
                format!("{HAR_TOOL} raw CDP Fetch route remove failed: {error}"),
            )
        })? {
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

async fn stop_fetch_if_no_routes(cdp_target_id: &str) -> Result<(), ErrorData> {
    let routes = synapse_a11y::fetch_route_rules(cdp_target_id).unwrap_or_default();
    if routes.is_empty() && synapse_a11y::fetch_interception_status(cdp_target_id).is_some() {
        synapse_a11y::fetch_interception_stop(cdp_target_id)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("{HAR_TOOL} raw CDP Fetch disable failed: {error}"),
                )
            })?;
    }
    Ok(())
}

fn har_headers_from_value(value: Option<&Value>) -> Vec<HarHeader> {
    let Some(Value::Object(headers)) = value else {
        return Vec::new();
    };
    headers
        .iter()
        .map(|(name, value)| HarHeader {
            name: name.clone(),
            value: header_json_value_to_string(value),
        })
        .collect()
}

fn header_lookup(headers: Option<&Value>, name: &str) -> Option<String> {
    let Value::Object(headers) = headers? else {
        return None;
    };
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .map(|(_, value)| header_json_value_to_string(value))
}

fn header_json_value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn har_content_from_response(
    response: Option<&synapse_a11y::CdpNetworkResponseSnapshot>,
    body: Option<synapse_a11y::CdpNetworkResponseBody>,
) -> HarContent {
    let mime_type = response
        .map(|response| response.mime_type.clone())
        .unwrap_or_default();
    let mut content = HarContent {
        size: response
            .map(|response| response.encoded_data_length.max(0.0) as i64)
            .unwrap_or(0),
        mime_type,
        text: None,
        encoding: None,
    };
    if let Some(body) = body {
        content.size = if body.base64_encoded {
            BASE64_STANDARD
                .decode(&body.body)
                .map(|bytes| bytes.len() as i64)
                .unwrap_or_else(|_| body.body.len() as i64)
        } else {
            body.body.len() as i64
        };
        content.text = Some(body.body);
        content.encoding = body.base64_encoded.then(|| "base64".to_owned());
    }
    content
}

fn har_content_body_base64(content: &HarContent) -> Result<Option<String>, ErrorData> {
    let Some(text) = content.text.as_deref() else {
        return Ok(None);
    };
    if content
        .encoding
        .as_deref()
        .is_some_and(|encoding| encoding.eq_ignore_ascii_case("base64"))
    {
        BASE64_STANDARD.decode(text).map_err(|error| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{HAR_TOOL} HAR response content is not valid base64: {error}"),
            )
        })?;
        return Ok(Some(text.to_owned()));
    }
    Ok(Some(BASE64_STANDARD.encode(text.as_bytes())))
}

fn har_route_id(index: usize, method: &str, url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    method.hash(&mut hasher);
    url.hash(&mut hasher);
    format!(
        "{HAR_REPLAY_ROUTE_PREFIX}{index:04x}-{:016x}",
        hasher.finish()
    )
}

fn har_datetime(unix_ms: Option<f64>) -> String {
    let unix_ms = unix_ms.unwrap_or_else(current_unix_ms);
    let secs = (unix_ms / 1000.0).floor() as i64;
    let nanos = ((unix_ms - (secs as f64 * 1000.0)) * 1_000_000.0)
        .round()
        .clamp(0.0, 999_999_999.0) as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn current_unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn har_version() -> String {
    "1.2".to_owned()
}

fn har_http_version() -> String {
    "HTTP/2".to_owned()
}

fn har_epoch_datetime() -> String {
    "1970-01-01T00:00:00.000Z".to_owned()
}

fn har_unknown_time() -> f64 {
    -1.0
}

fn har_size_unknown() -> i64 {
    -1
}

fn browser_network_entry_to_wire(entry: &synapse_a11y::CdpNetworkEntry) -> BrowserNetworkWaitEntry {
    let response = entry.response.as_ref();
    BrowserNetworkWaitEntry {
        seq: entry.seq,
        request_id: entry.request_id.clone(),
        url: entry.url.clone(),
        method: entry.method.clone(),
        resource_type: entry.resource_type.clone(),
        request_headers: entry.request_headers.clone(),
        response_received: entry.response_received,
        response_url: response.map(|response| response.url.clone()),
        status: response.map(|response| response.status),
        status_text: response.map(|response| response.status_text.clone()),
        response_headers: response.map(|response| response.headers.clone()),
        response_timing: response.and_then(|response| response.timing.clone()),
        protocol: response.and_then(|response| response.protocol.clone()),
        remote_ip_address: response.and_then(|response| response.remote_ip_address.clone()),
        remote_port: response.and_then(|response| response.remote_port),
        encoded_data_length: entry
            .encoded_data_length
            .or_else(|| response.map(|response| response.encoded_data_length)),
        loading_finished: entry.loading_finished,
        loading_failed: entry.loading_failed,
        failure_error_text: entry.failure_error_text.clone(),
    }
}

fn browser_network_request_detail_to_wire(
    entry: &synapse_a11y::CdpNetworkEntry,
) -> BrowserNetworkRequestDetail {
    BrowserNetworkRequestDetail {
        seq: entry.seq,
        first_seq: entry.first_seq,
        request_id: entry.request_id.clone(),
        loader_id: entry.loader_id.clone(),
        frame_id: entry.frame_id.clone(),
        document_url: entry.document_url.clone(),
        url: entry.url.clone(),
        method: entry.method.clone(),
        resource_type: entry.resource_type.clone(),
        request_headers: entry.request_headers.clone(),
        request_has_post_data: entry.request_has_post_data,
        request_timestamp_s: entry.request_timestamp_s,
        request_wall_time_ms: entry.request_wall_time_ms,
        initiator: entry.initiator.clone(),
        redirects: entry
            .redirects
            .iter()
            .map(browser_network_response_snapshot_to_wire)
            .collect(),
        response_received: entry.response_received,
        response: entry
            .response
            .as_ref()
            .map(browser_network_response_snapshot_to_wire),
        response_timestamp_s: entry.response_timestamp_s,
        loading_finished: entry.loading_finished,
        loading_failed: entry.loading_failed,
        finished_timestamp_s: entry.finished_timestamp_s,
        failed_timestamp_s: entry.failed_timestamp_s,
        encoded_data_length: entry.encoded_data_length,
        failure_error_text: entry.failure_error_text.clone(),
        failure_canceled: entry.failure_canceled,
        failure_blocked_reason: entry.failure_blocked_reason.clone(),
        failure_cors_error_status: entry.failure_cors_error_status.clone(),
    }
}

fn browser_network_websocket_entry_to_wire(
    entry: &synapse_a11y::CdpWebSocketEntry,
) -> BrowserNetworkWebSocketEntry {
    BrowserNetworkWebSocketEntry {
        seq: entry.seq,
        first_seq: entry.first_seq,
        request_id: entry.request_id.clone(),
        url: entry.url.clone(),
        created: entry.created,
        created_at_unix_ms: entry.created_at_unix_ms,
        initiator: entry.initiator.clone(),
        handshake_request_timestamp_s: entry.handshake_request_timestamp_s,
        handshake_request_wall_time_ms: entry.handshake_request_wall_time_ms,
        handshake_request_headers: entry.handshake_request_headers.clone(),
        handshake_response_timestamp_s: entry.handshake_response_timestamp_s,
        status: entry.status,
        status_text: entry.status_text.clone(),
        handshake_response_headers: entry.handshake_response_headers.clone(),
        handshake_response_headers_text: entry.handshake_response_headers_text.clone(),
        handshake_response_request_headers: entry.handshake_response_request_headers.clone(),
        handshake_response_request_headers_text: entry
            .handshake_response_request_headers_text
            .clone(),
        frames: entry
            .frames
            .iter()
            .map(browser_network_websocket_frame_to_wire)
            .collect(),
        sent_frame_count: entry.sent_frame_count,
        received_frame_count: entry.received_frame_count,
        frame_error_count: entry.frame_error_count,
        dropped_frames: entry.dropped_frames,
        closed: entry.closed,
        closed_timestamp_s: entry.closed_timestamp_s,
        close_code: entry.close_code,
        close_reason: entry.close_reason.clone(),
    }
}

fn browser_network_websocket_frame_to_wire(
    frame: &synapse_a11y::CdpWebSocketFrame,
) -> BrowserNetworkWebSocketFrame {
    BrowserNetworkWebSocketFrame {
        seq: frame.seq,
        direction: frame.direction.clone(),
        timestamp_s: frame.timestamp_s,
        opcode: frame.opcode,
        mask: frame.mask,
        payload_data: frame.payload_data.clone(),
        payload_len_chars: frame.payload_len_chars,
        payload_base64_encoded: frame.payload_base64_encoded,
        close_code: frame.close_code,
        close_reason: frame.close_reason.clone(),
        error_message: frame.error_message.clone(),
    }
}

fn browser_network_response_snapshot_to_wire(
    response: &synapse_a11y::CdpNetworkResponseSnapshot,
) -> BrowserNetworkResponseSnapshot {
    BrowserNetworkResponseSnapshot {
        url: response.url.clone(),
        status: response.status,
        status_text: response.status_text.clone(),
        headers: response.headers.clone(),
        request_headers: response.request_headers.clone(),
        mime_type: response.mime_type.clone(),
        protocol: response.protocol.clone(),
        remote_ip_address: response.remote_ip_address.clone(),
        remote_port: response.remote_port,
        encoded_data_length: response.encoded_data_length,
        timing: response.timing.clone(),
        response_time_ms: response.response_time_ms,
        from_disk_cache: response.from_disk_cache,
        from_service_worker: response.from_service_worker,
        from_prefetch_cache: response.from_prefetch_cache,
        from_early_hints: response.from_early_hints,
        timestamp_s: response.timestamp_s,
        resource_type: response.resource_type.clone(),
    }
}

fn browser_network_response_body_to_wire(
    body: synapse_a11y::CdpNetworkResponseBody,
) -> BrowserNetworkResponseBody {
    let body_len_chars = body.body.chars().count();
    BrowserNetworkResponseBody {
        request_id: body.request_id,
        body: body.body,
        base64_encoded: body.base64_encoded,
        body_len_chars,
    }
}

fn browser_network_post_data_to_wire(
    post_data: synapse_a11y::CdpNetworkRequestPostData,
) -> BrowserNetworkRequestPostData {
    let post_data_len_chars = post_data.post_data.chars().count();
    BrowserNetworkRequestPostData {
        request_id: post_data.request_id,
        post_data: post_data.post_data,
        post_data_len_chars,
    }
}

fn browser_network_overrides_response(
    session_id: &str,
    window_hwnd: i64,
    endpoint: String,
    cdp_target_id: &str,
    operation: BrowserNetworkOverridesOperation,
    status: Option<synapse_a11y::CdpNetworkOverrideStatus>,
    cleared: bool,
) -> BrowserNetworkOverridesResponse {
    match status {
        Some(status) => BrowserNetworkOverridesResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            operation,
            override_active: !status.headers.is_empty() || status.user_agent.is_some(),
            newly_armed: status.newly_armed,
            cleared,
            armed_at_unix_ms: status.armed_at_unix_ms,
            applied_at_unix_ms: status.applied_at_unix_ms,
            header_count: status.header_count,
            headers: status
                .headers
                .into_iter()
                .map(|(name, value)| BrowserRouteHeader { name, value })
                .collect(),
            user_agent: status.user_agent,
            original_user_agent: status.original_user_agent,
            readback_backend: "Network.setExtraHTTPHeaders + Emulation.setUserAgentOverride"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        },
        None => BrowserNetworkOverridesResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            operation,
            override_active: false,
            newly_armed: false,
            cleared,
            armed_at_unix_ms: 0,
            applied_at_unix_ms: 0,
            header_count: 0,
            headers: Vec::new(),
            user_agent: None,
            original_user_agent: None,
            readback_backend: "Network.setExtraHTTPHeaders + Emulation.setUserAgentOverride"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        },
    }
}

fn browser_network_conditions_response(
    session_id: &str,
    window_hwnd: i64,
    result: synapse_a11y::CdpNetworkConditionsResult,
) -> BrowserNetworkConditionsResponse {
    BrowserNetworkConditionsResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "raw_cdp".to_owned(),
        endpoint: result.endpoint,
        cdp_target_id: result.cdp_target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserNetworkConditionsOperation::Reset,
            _ => BrowserNetworkConditionsOperation::Set,
        },
        requested: result
            .requested
            .map(|requested| BrowserNetworkConditionsOverride {
                offline: requested.offline,
                latency_ms: requested.latency_ms,
                download_throughput_bytes_per_sec: requested.download_throughput_bytes_per_sec,
                upload_throughput_bytes_per_sec: requested.upload_throughput_bytes_per_sec,
                connection_type: requested.connection_type,
            }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        network: BrowserNetworkConditionsReadback {
            online: result.readback.online,
            connection_type: result.readback.connection_type,
            effective_type: result.readback.effective_type,
            downlink_mbps: result.readback.downlink_mbps,
            rtt_ms: result.readback.rtt_ms,
            save_data: result.readback.save_data,
        },
        readback_backend: "Network.emulateNetworkConditions + Runtime.evaluate".to_owned(),
        backend_tier_used: "cdp".to_owned(),
        required_foreground: false,
    }
}

fn browser_network_conditions_bridge_response(
    session_id: &str,
    window_hwnd: i64,
    result: crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsResult,
) -> BrowserNetworkConditionsResponse {
    BrowserNetworkConditionsResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: chrome_debugger_default_endpoint(),
        cdp_target_id: result.target_id,
        operation: match result.operation.as_str() {
            "reset" => BrowserNetworkConditionsOperation::Reset,
            _ => BrowserNetworkConditionsOperation::Set,
        },
        requested: result
            .requested
            .map(|requested| BrowserNetworkConditionsOverride {
                offline: requested.offline,
                latency_ms: requested.latency_ms,
                download_throughput_bytes_per_sec: requested.download_throughput_bytes_per_sec,
                upload_throughput_bytes_per_sec: requested.upload_throughput_bytes_per_sec,
                connection_type: requested.connection_type,
            }),
        page_url: result.page_url,
        page_title: result.page_title,
        ready_state: result.ready_state,
        network: BrowserNetworkConditionsReadback {
            online: result.network.online,
            connection_type: result.network.connection_type,
            effective_type: result.network.effective_type,
            downlink_mbps: result.network.downlink_mbps,
            rtt_ms: result.network.rtt_ms,
            save_data: result.network.save_data,
        },
        readback_backend: if result.readback_backend.is_empty() {
            "chrome.debugger.Network.emulateNetworkConditions + chrome.scripting.executeScript"
                .to_owned()
        } else {
            result.readback_backend
        },
        backend_tier_used: if result.backend_tier_used.is_empty() {
            "chrome_tabs_extension_debugger".to_owned()
        } else {
            result.backend_tier_used
        },
        required_foreground: result.required_foreground,
    }
}

fn require_response_body_available(entry: &synapse_a11y::CdpNetworkEntry) -> Result<(), ErrorData> {
    if !entry.response_received {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "{REQUEST_TOOL} response body is unavailable for request_id={}: no responseReceived event captured",
                entry.request_id
            ),
        ));
    }
    if entry.loading_failed {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "{REQUEST_TOOL} response body is unavailable for request_id={}: loadingFailed {:?}",
                entry.request_id, entry.failure_error_text
            ),
        ));
    }
    if !entry.loading_finished {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "{REQUEST_TOOL} response body is unavailable for request_id={}: loadingFinished has not been captured yet",
                entry.request_id
            ),
        ));
    }
    Ok(())
}

fn browser_route_fetch_status_from_a11y(
    status: Option<synapse_a11y::CdpFetchInterceptionStatus>,
    fetch_armed: bool,
) -> BrowserRouteFetchStatus {
    match status {
        Some(status) => BrowserRouteFetchStatus {
            fetch_armed,
            newly_armed: status.newly_armed,
            armed_at_unix_ms: status.armed_at_unix_ms,
            pattern_count: status.pattern_count,
            route_count: status.route_count,
            paused_count: status.paused_count,
            continued_count: status.continued_count,
            fulfilled_count: status.fulfilled_count,
            failed_count: status.failed_count,
            continue_error_count: status.continue_error_count,
            last_request_id: status.last_request_id,
            last_url: status.last_url,
            last_route_id: status.last_route_id,
            last_error: status.last_error,
        },
        None => BrowserRouteFetchStatus {
            fetch_armed,
            newly_armed: false,
            armed_at_unix_ms: 0,
            pattern_count: 0,
            route_count: 0,
            paused_count: 0,
            continued_count: 0,
            fulfilled_count: 0,
            failed_count: 0,
            continue_error_count: 0,
            last_request_id: None,
            last_url: None,
            last_route_id: None,
            last_error: None,
        },
    }
}

fn browser_route_rule_to_wire(rule: &synapse_a11y::CdpFetchRouteRule) -> BrowserRouteRuleResponse {
    match &rule.action {
        synapse_a11y::CdpFetchRouteAction::Fulfill(fulfill) => BrowserRouteRuleResponse {
            id: rule.id.clone(),
            url: rule.url.clone(),
            match_kind: match rule.match_kind {
                synapse_a11y::CdpFetchRouteMatchKind::Glob => BrowserRouteMatchKind::Glob,
                synapse_a11y::CdpFetchRouteMatchKind::Regex => BrowserRouteMatchKind::Regex,
            },
            method: rule.method.clone(),
            resource_type: rule.resource_type.clone(),
            action: "fulfill".to_owned(),
            status: Some(fulfill.status),
            error_reason: None,
            continue_url: None,
            continue_method: None,
            response_phrase: fulfill.response_phrase.clone(),
            headers: fulfill
                .headers
                .iter()
                .map(|(name, value)| BrowserRouteHeader {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            body_base64_len_chars: fulfill
                .body_base64
                .as_ref()
                .map(|body| body.chars().count()),
            post_data_base64_len_chars: None,
        },
        synapse_a11y::CdpFetchRouteAction::Abort(abort) => BrowserRouteRuleResponse {
            id: rule.id.clone(),
            url: rule.url.clone(),
            match_kind: match rule.match_kind {
                synapse_a11y::CdpFetchRouteMatchKind::Glob => BrowserRouteMatchKind::Glob,
                synapse_a11y::CdpFetchRouteMatchKind::Regex => BrowserRouteMatchKind::Regex,
            },
            method: rule.method.clone(),
            resource_type: rule.resource_type.clone(),
            action: "abort".to_owned(),
            status: None,
            error_reason: Some(abort.error_reason.clone()),
            continue_url: None,
            continue_method: None,
            response_phrase: None,
            headers: Vec::new(),
            body_base64_len_chars: None,
            post_data_base64_len_chars: None,
        },
        synapse_a11y::CdpFetchRouteAction::Continue(continue_rule) => BrowserRouteRuleResponse {
            id: rule.id.clone(),
            url: rule.url.clone(),
            match_kind: match rule.match_kind {
                synapse_a11y::CdpFetchRouteMatchKind::Glob => BrowserRouteMatchKind::Glob,
                synapse_a11y::CdpFetchRouteMatchKind::Regex => BrowserRouteMatchKind::Regex,
            },
            method: rule.method.clone(),
            resource_type: rule.resource_type.clone(),
            action: "continue".to_owned(),
            status: None,
            error_reason: None,
            continue_url: continue_rule.url.clone(),
            continue_method: continue_rule.method.clone(),
            response_phrase: None,
            headers: continue_rule
                .headers
                .iter()
                .map(|(name, value)| BrowserRouteHeader {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            body_base64_len_chars: None,
            post_data_base64_len_chars: continue_rule
                .post_data_base64
                .as_ref()
                .map(|post_data| post_data.chars().count()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        seq: u64,
        request_id: &str,
        url: &str,
        resource_type: &str,
        status: Option<i64>,
    ) -> synapse_a11y::CdpNetworkEntry {
        let response = status.map(|status| synapse_a11y::CdpNetworkResponseSnapshot {
            url: url.to_owned(),
            status,
            status_text: "OK".to_owned(),
            headers: json!({"content-type": "application/json"}),
            request_headers: None,
            mime_type: "application/json".to_owned(),
            protocol: Some("h2".to_owned()),
            remote_ip_address: Some("127.0.0.1".to_owned()),
            remote_port: Some(443),
            encoded_data_length: 42.0,
            timing: Some(json!({"requestTime": 1.0})),
            response_time_ms: None,
            from_disk_cache: None,
            from_service_worker: None,
            from_prefetch_cache: None,
            from_early_hints: None,
            timestamp_s: Some(2.0),
            resource_type: Some(resource_type.to_owned()),
        });
        synapse_a11y::CdpNetworkEntry {
            seq,
            first_seq: seq,
            request_id: request_id.to_owned(),
            loader_id: Some("loader".to_owned()),
            frame_id: Some("frame".to_owned()),
            document_url: Some("https://example.test/".to_owned()),
            url: Some(url.to_owned()),
            method: Some("GET".to_owned()),
            resource_type: Some(resource_type.to_owned()),
            request_headers: Some(json!({"accept": "*/*"})),
            request_has_post_data: None,
            request_timestamp_s: Some(1.0),
            request_wall_time_ms: Some(1_710_000_000_000.0),
            initiator: None,
            redirects: Vec::new(),
            response_timestamp_s: response.as_ref().and_then(|r| r.timestamp_s),
            response_received: response.is_some(),
            response,
            loading_finished: true,
            loading_failed: false,
            finished_timestamp_s: Some(3.0),
            failed_timestamp_s: None,
            encoded_data_length: Some(84.0),
            failure_error_text: None,
            failure_canceled: None,
            failure_blocked_reason: None,
            failure_cors_error_status: None,
        }
    }

    fn websocket_entry() -> synapse_a11y::CdpWebSocketEntry {
        synapse_a11y::CdpWebSocketEntry {
            seq: 4,
            first_seq: 0,
            request_id: "ws-1".to_owned(),
            url: Some("wss://example.test/socket".to_owned()),
            created: true,
            created_at_unix_ms: Some(1_710_000_000_000.0),
            initiator: Some(json!({"type": "script"})),
            handshake_request_timestamp_s: Some(1.0),
            handshake_request_wall_time_ms: Some(1_710_000_000_100.0),
            handshake_request_headers: Some(json!({"sec-websocket-key": "abc"})),
            handshake_response_timestamp_s: Some(2.0),
            status: Some(101),
            status_text: Some("Switching Protocols".to_owned()),
            handshake_response_headers: Some(json!({"upgrade": "websocket"})),
            handshake_response_headers_text: None,
            handshake_response_request_headers: None,
            handshake_response_request_headers_text: None,
            frames: vec![
                synapse_a11y::CdpWebSocketFrame {
                    seq: 3,
                    direction: "sent".to_owned(),
                    timestamp_s: Some(3.0),
                    opcode: Some(1.0),
                    mask: Some(true),
                    payload_data: Some("ping".to_owned()),
                    payload_len_chars: 4,
                    payload_base64_encoded: false,
                    close_code: None,
                    close_reason: None,
                    error_message: None,
                },
                synapse_a11y::CdpWebSocketFrame {
                    seq: 4,
                    direction: "received".to_owned(),
                    timestamp_s: Some(4.0),
                    opcode: Some(8.0),
                    mask: Some(false),
                    payload_data: Some("A+hPSw==".to_owned()),
                    payload_len_chars: 8,
                    payload_base64_encoded: true,
                    close_code: Some(1000),
                    close_reason: Some("OK".to_owned()),
                    error_message: None,
                },
            ],
            sent_frame_count: 1,
            received_frame_count: 1,
            frame_error_count: 0,
            dropped_frames: 0,
            closed: true,
            closed_timestamp_s: Some(5.0),
            close_code: Some(1000),
            close_reason: Some("OK".to_owned()),
        }
    }

    #[test]
    fn browser_network_requests_validation_edges() {
        let ok = validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
            cdp_target_id: Some("target-123".to_owned()),
            since_seq: Some(7),
            limit: Some(MAX_NETWORK_REQUEST_LIMIT),
            url_regex: Some(r"^https://example\.test/api".to_owned()),
            resource_type: Some("XHR".to_owned()),
            status_min: Some(200),
            status_max: Some(299),
            ..Default::default()
        })
        .expect("valid params pass");
        assert_eq!(ok.since_seq, Some(7));
        assert_eq!(ok.limit, MAX_NETWORK_REQUEST_LIMIT);
        assert!(
            ok.url_regex
                .as_ref()
                .unwrap()
                .is_match("https://example.test/api")
        );
        assert_eq!(ok.resource_type.as_deref(), Some("XHR"));

        for error in [
            validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
                limit: Some(0),
                ..Default::default()
            })
            .expect_err("zero limit must be rejected"),
            validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
                url_contains: Some("api".to_owned()),
                url_regex: Some("api".to_owned()),
                ..Default::default()
            })
            .expect_err("ambiguous URL filters must be rejected"),
            validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
                url_regex: Some("(".to_owned()),
                ..Default::default()
            })
            .expect_err("invalid URL regex must be rejected"),
            validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
                resource_type: Some(" XHR".to_owned()),
                ..Default::default()
            })
            .expect_err("resource type whitespace must be rejected"),
            validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
                status_min: Some(500),
                status_max: Some(200),
                ..Default::default()
            })
            .expect_err("inverted status range must be rejected"),
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
    fn browser_network_websockets_validation_edges() {
        let ok = validate_browser_network_websockets_params(&BrowserNetworkWebSocketsParams {
            cdp_target_id: Some("target-123".to_owned()),
            since_seq: Some(7),
            limit: Some(MAX_NETWORK_REQUEST_LIMIT),
            request_id: Some("ws-1".to_owned()),
            url_contains: Some("socket".to_owned()),
            window_hwnd: None,
        })
        .expect("valid websocket params pass");
        assert_eq!(ok.since_seq, Some(7));
        assert_eq!(ok.limit, MAX_NETWORK_REQUEST_LIMIT);
        assert_eq!(ok.request_id.as_deref(), Some("ws-1"));
        assert_eq!(ok.url_contains.as_deref(), Some("socket"));

        for error in [
            validate_browser_network_websockets_params(&BrowserNetworkWebSocketsParams {
                limit: Some(0),
                ..Default::default()
            })
            .expect_err("zero limit must be rejected"),
            validate_browser_network_websockets_params(&BrowserNetworkWebSocketsParams {
                request_id: Some(" ws-1".to_owned()),
                ..Default::default()
            })
            .expect_err("request id whitespace must be rejected"),
            validate_browser_network_websockets_params(&BrowserNetworkWebSocketsParams {
                url_contains: Some(String::new()),
                ..Default::default()
            })
            .expect_err("empty url_contains must be rejected"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
        }
        println!("readback=browser_network_websockets validation edges rejected invalid params");
    }

    #[test]
    fn browser_network_har_validation_edges() {
        let record = validate_browser_network_har_params(&BrowserNetworkHarParams {
            cdp_target_id: Some("target-123".to_owned()),
            operation: BrowserNetworkHarOperation::Record,
            path: Some(r"C:\tmp\capture.har".to_owned()),
            since_seq: Some(3),
            limit: Some(50),
            url_contains: Some("api".to_owned()),
            include_bodies: Some(false),
            ..Default::default()
        })
        .expect("valid HAR record params pass");
        assert_eq!(record.operation, BrowserNetworkHarOperation::Record);
        assert_eq!(record.filters.since_seq, Some(3));
        assert_eq!(record.filters.limit, 50);
        assert!(!record.include_bodies);

        let replay = validate_browser_network_har_params(&BrowserNetworkHarParams {
            operation: BrowserNetworkHarOperation::Replay,
            path: Some(r"C:\tmp\capture.har".to_owned()),
            limit: Some(25),
            missing_policy: Some(BrowserNetworkHarMissingPolicy::Abort),
            clear_existing_replay: Some(false),
            ..Default::default()
        })
        .expect("valid HAR replay params pass");
        assert_eq!(replay.operation, BrowserNetworkHarOperation::Replay);
        assert_eq!(replay.filters.limit, 25);
        assert_eq!(replay.missing_policy, BrowserNetworkHarMissingPolicy::Abort);
        assert!(!replay.clear_existing_replay);

        let clear = validate_browser_network_har_params(&BrowserNetworkHarParams {
            operation: BrowserNetworkHarOperation::ClearReplay,
            ..Default::default()
        })
        .expect("valid HAR clear params pass");
        assert_eq!(clear.operation, BrowserNetworkHarOperation::ClearReplay);

        for error in [
            validate_browser_network_har_params(&BrowserNetworkHarParams {
                operation: BrowserNetworkHarOperation::Record,
                ..Default::default()
            })
            .expect_err("record requires path"),
            validate_browser_network_har_params(&BrowserNetworkHarParams {
                operation: BrowserNetworkHarOperation::Replay,
                path: Some("capture.har".to_owned()),
                url_contains: Some("api".to_owned()),
                ..Default::default()
            })
            .expect_err("replay rejects record filters"),
            validate_browser_network_har_params(&BrowserNetworkHarParams {
                operation: BrowserNetworkHarOperation::ClearReplay,
                path: Some("capture.har".to_owned()),
                ..Default::default()
            })
            .expect_err("clear rejects path"),
            validate_browser_network_har_params(&BrowserNetworkHarParams {
                operation: BrowserNetworkHarOperation::Record,
                path: Some("capture.har".to_owned()),
                url_contains: Some("api".to_owned()),
                url_regex: Some("api".to_owned()),
                ..Default::default()
            })
            .expect_err("record rejects ambiguous URL filters"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(serde_json::Value::as_str);
            assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
        }
        println!("readback=browser_network_har validation edges rejected invalid params");
    }

    #[tokio::test]
    async fn browser_network_har_record_writes_har_and_replay_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.har");
        let entries = vec![entry(
            1,
            "req-1",
            "https://example.test/api/data?x=1",
            "Fetch",
            Some(200),
        )];

        let written = write_har_record("target-1", &path, &entries, false)
            .await
            .expect("write HAR");
        assert_eq!(written.entry_count, 1);
        assert_eq!(written.skipped_count, 0);
        assert!(written.bytes_written > 0);
        let raw = fs::read_to_string(&path).expect("read HAR");
        assert!(raw.contains("\"version\": \"1.2\""));
        assert!(raw.contains("\"_synapseRequestId\": \"req-1\""));

        let replay = load_har_replay(&path, 10, BrowserNetworkHarMissingPolicy::Abort)
            .expect("load HAR replay");
        assert_eq!(replay.entry_count, 1);
        assert_eq!(replay.skipped_count, 0);
        assert!(replay.missing_abort_route_installed);
        assert_eq!(replay.rules.len(), 2);
        assert_eq!(replay.rules[0].method.as_deref(), Some("GET"));
        assert!(replay.rules[0].url.starts_with('^'));
        assert_eq!(replay.rules[1].id, HAR_REPLAY_MISS_ROUTE_ID);
        println!(
            "readback=browser_network_har record_entries={} replay_rules={}",
            written.entry_count,
            replay.rules.len()
        );
    }

    #[test]
    fn browser_network_har_replay_encodes_text_content() {
        let har_entry = HarEntry {
            request: HarRequest {
                method: "POST".to_owned(),
                url: "https://example.test/submit".to_owned(),
                ..Default::default()
            },
            response: HarResponse {
                status: 201,
                status_text: "Created".to_owned(),
                headers: vec![
                    HarHeader {
                        name: "content-type".to_owned(),
                        value: "text/plain".to_owned(),
                    },
                    HarHeader {
                        name: "content-length".to_owned(),
                        value: "999".to_owned(),
                    },
                ],
                content: HarContent {
                    size: 5,
                    mime_type: "text/plain".to_owned(),
                    text: Some("hello".to_owned()),
                    encoding: None,
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let rule = har_entry_to_route_rule(0, &har_entry)
            .expect("HAR route")
            .expect("route exists");
        assert_eq!(rule.method.as_deref(), Some("POST"));
        match rule.action {
            synapse_a11y::CdpFetchRouteAction::Fulfill(fulfill) => {
                assert_eq!(fulfill.status, 201);
                assert_eq!(fulfill.body_base64.as_deref(), Some("aGVsbG8="));
                assert_eq!(fulfill.headers.len(), 1);
                assert_eq!(fulfill.headers[0].0, "content-type");
            }
            _ => panic!("expected fulfill rule"),
        }
        println!("readback=browser_network_har replay text content encoded as base64");
    }

    #[test]
    fn browser_network_websocket_entry_maps_frames_and_close_info() {
        let wire = browser_network_websocket_entry_to_wire(&websocket_entry());
        assert_eq!(wire.request_id, "ws-1");
        assert_eq!(wire.status, Some(101));
        assert_eq!(wire.sent_frame_count, 1);
        assert_eq!(wire.received_frame_count, 1);
        assert!(wire.closed);
        assert_eq!(wire.close_code, Some(1000));
        assert_eq!(wire.close_reason.as_deref(), Some("OK"));
        assert_eq!(wire.frames.len(), 2);
        assert_eq!(wire.frames[0].direction, "sent");
        assert_eq!(wire.frames[0].payload_data.as_deref(), Some("ping"));
        assert_eq!(wire.frames[1].close_code, Some(1000));
        println!(
            "readback=browser_network_websocket wire request_id={} frames={} close_code={:?}",
            wire.request_id,
            wire.frames.len(),
            wire.close_code
        );
    }

    #[test]
    fn browser_network_requests_filters_entries_after_cursor_read() {
        let filters = validate_browser_network_requests_params(&BrowserNetworkRequestsParams {
            url_contains: Some("/api/".to_owned()),
            resource_type: Some("XHR".to_owned()),
            status_min: Some(200),
            status_max: Some(299),
            ..Default::default()
        })
        .expect("filters validate");
        let filtered = filter_network_entries(
            vec![
                entry(1, "doc", "https://example.test/", "Document", Some(200)),
                entry(
                    2,
                    "api-ok",
                    "https://example.test/api/users",
                    "XHR",
                    Some(204),
                ),
                entry(
                    3,
                    "api-err",
                    "https://example.test/api/fail",
                    "XHR",
                    Some(500),
                ),
            ]
            .into_iter(),
            &filters,
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].request_id, "api-ok");
        let wire = browser_network_entry_to_wire(&filtered[0]);
        assert_eq!(wire.status, Some(204));
        assert_eq!(
            wire.response_headers,
            Some(json!({"content-type": "application/json"}))
        );
        assert_eq!(wire.encoded_data_length, Some(84.0));
    }

    #[test]
    fn browser_network_request_validation_edges() {
        let ok = validate_browser_network_request_params(&BrowserNetworkRequestParams {
            request_id: "1234.56".to_owned(),
            cdp_target_id: Some("target-123".to_owned()),
            window_hwnd: Some(100),
            include_body: true,
            include_post_data: true,
        })
        .expect("valid request params pass");
        assert_eq!(ok.request_id, "1234.56");
        assert!(ok.include_body);
        assert!(ok.include_post_data);

        for error in [
            validate_browser_network_request_params(&BrowserNetworkRequestParams {
                request_id: String::new(),
                cdp_target_id: None,
                window_hwnd: None,
                include_body: true,
                include_post_data: true,
            })
            .expect_err("empty request id must be rejected"),
            validate_browser_network_request_params(&BrowserNetworkRequestParams {
                request_id: " request ".to_owned(),
                cdp_target_id: None,
                window_hwnd: None,
                include_body: true,
                include_post_data: true,
            })
            .expect_err("request id whitespace must be rejected"),
            validate_browser_network_request_params(&BrowserNetworkRequestParams {
                request_id: "bad\nid".to_owned(),
                cdp_target_id: None,
                window_hwnd: None,
                include_body: true,
                include_post_data: true,
            })
            .expect_err("request id control chars must be rejected"),
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
    fn browser_network_overrides_validation_edges() {
        let ok = validate_browser_network_overrides_params(&BrowserNetworkOverridesParams {
            cdp_target_id: Some("target-123".to_owned()),
            headers: vec![BrowserRouteHeader {
                name: "x-synapse-test".to_owned(),
                value: "enabled".to_owned(),
            }],
            user_agent: Some("SynapseTest/1.0".to_owned()),
            ..Default::default()
        })
        .expect("valid override params pass");
        assert_eq!(ok.operation, BrowserNetworkOverridesOperation::Set);
        assert_eq!(ok.headers[0].0, "x-synapse-test");
        assert_eq!(ok.user_agent.as_deref(), Some("SynapseTest/1.0"));

        for error in [
            validate_browser_network_overrides_params(&BrowserNetworkOverridesParams {
                headers: vec![BrowserRouteHeader {
                    name: "bad header".to_owned(),
                    value: "value".to_owned(),
                }],
                ..Default::default()
            })
            .expect_err("bad header name must be rejected"),
            validate_browser_network_overrides_params(&BrowserNetworkOverridesParams {
                user_agent: Some(" bad ".to_owned()),
                ..Default::default()
            })
            .expect_err("bad user-agent whitespace must be rejected"),
            validate_browser_network_overrides_params(&BrowserNetworkOverridesParams {
                operation: BrowserNetworkOverridesOperation::Get,
                user_agent: Some("SynapseTest/1.0".to_owned()),
                ..Default::default()
            })
            .expect_err("get rejects set-only fields"),
            validate_browser_network_overrides_params(&BrowserNetworkOverridesParams {
                operation: BrowserNetworkOverridesOperation::Clear,
                headers: vec![BrowserRouteHeader {
                    name: "x-test".to_owned(),
                    value: "yes".to_owned(),
                }],
                ..Default::default()
            })
            .expect_err("clear rejects set-only fields"),
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
    fn browser_network_overrides_response_maps_active_and_inactive_state() {
        let active = browser_network_overrides_response(
            "session-1",
            100,
            "http://127.0.0.1:9222".to_owned(),
            "target-123",
            BrowserNetworkOverridesOperation::Set,
            Some(synapse_a11y::CdpNetworkOverrideStatus {
                newly_armed: true,
                endpoint: "http://127.0.0.1:9222".to_owned(),
                cdp_target_id: "target-123".to_owned(),
                armed_at_unix_ms: 10,
                applied_at_unix_ms: 20,
                header_count: 1,
                headers: vec![("x-synapse-test".to_owned(), "enabled".to_owned())],
                user_agent: Some("SynapseTest/1.0".to_owned()),
                original_user_agent: Some("Chrome/Default".to_owned()),
            }),
            false,
        );
        assert!(active.override_active);
        assert!(active.newly_armed);
        assert_eq!(active.header_count, 1);
        assert_eq!(active.headers[0].name, "x-synapse-test");
        assert_eq!(active.user_agent.as_deref(), Some("SynapseTest/1.0"));
        assert_eq!(
            active.original_user_agent.as_deref(),
            Some("Chrome/Default")
        );

        let inactive = browser_network_overrides_response(
            "session-1",
            100,
            "http://127.0.0.1:9222".to_owned(),
            "target-123",
            BrowserNetworkOverridesOperation::Get,
            None,
            false,
        );
        assert!(!inactive.override_active);
        assert_eq!(inactive.header_count, 0);
        assert!(inactive.headers.is_empty());
    }

    #[test]
    fn browser_network_conditions_validation_edges() {
        let offline = validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
            cdp_target_id: Some("target-123".to_owned()),
            offline: Some(true),
            ..Default::default()
        })
        .expect("offline params pass");
        assert_eq!(offline.operation, BrowserNetworkConditionsOperation::Set);
        let requested = offline.requested.expect("requested conditions");
        assert!(requested.offline);
        assert_eq!(requested.latency_ms, 0.0);
        assert_eq!(requested.download_throughput_bytes_per_sec, -1.0);
        assert_eq!(requested.upload_throughput_bytes_per_sec, -1.0);

        let throttled =
            validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
                latency_ms: Some(400.0),
                download_throughput_bytes_per_sec: Some(50_000.0),
                upload_throughput_bytes_per_sec: Some(20_000.0),
                connection_type: Some("cellular3g".to_owned()),
                ..Default::default()
            })
            .expect("throttled params pass");
        let requested = throttled.requested.expect("requested conditions");
        assert!(!requested.offline);
        assert_eq!(requested.connection_type.as_deref(), Some("cellular3g"));

        for error in [
            validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
                ..Default::default()
            })
            .expect_err("no-op set must be rejected"),
            validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
                latency_ms: Some(-1.0),
                ..Default::default()
            })
            .expect_err("negative latency must be rejected"),
            validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
                download_throughput_bytes_per_sec: Some(-2.0),
                ..Default::default()
            })
            .expect_err("invalid download throughput must be rejected"),
            validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
                connection_type: Some("lte".to_owned()),
                ..Default::default()
            })
            .expect_err("invalid connection type must be rejected"),
            validate_browser_network_conditions_params(&BrowserNetworkConditionsParams {
                operation: BrowserNetworkConditionsOperation::Reset,
                offline: Some(false),
                ..Default::default()
            })
            .expect_err("reset rejects set-only fields"),
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
    fn browser_network_conditions_response_maps_readback() {
        let response = browser_network_conditions_response(
            "session-1",
            100,
            synapse_a11y::CdpNetworkConditionsResult {
                endpoint: "http://127.0.0.1:9222".to_owned(),
                cdp_target_id: "target-123".to_owned(),
                operation: "set".to_owned(),
                requested: Some(synapse_a11y::CdpNetworkConditionsOverride {
                    offline: true,
                    latency_ms: 100.0,
                    download_throughput_bytes_per_sec: 50_000.0,
                    upload_throughput_bytes_per_sec: 20_000.0,
                    connection_type: Some("cellular3g".to_owned()),
                }),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                readback: synapse_a11y::CdpNetworkConditionsReadback {
                    online: false,
                    connection_type: Some("cellular".to_owned()),
                    effective_type: Some("3g".to_owned()),
                    downlink_mbps: Some(0.4),
                    rtt_ms: Some(400.0),
                    save_data: Some(false),
                },
            },
        );
        assert_eq!(response.operation, BrowserNetworkConditionsOperation::Set);
        assert_eq!(
            response
                .requested
                .as_ref()
                .unwrap()
                .connection_type
                .as_deref(),
            Some("cellular3g")
        );
        assert!(!response.network.online);
        assert_eq!(response.network.effective_type.as_deref(), Some("3g"));
        assert_eq!(
            response.readback_backend,
            "Network.emulateNetworkConditions + Runtime.evaluate"
        );
    }

    #[test]
    fn browser_network_conditions_bridge_response_maps_readback() {
        let response = browser_network_conditions_bridge_response(
            "session-1",
            100,
            crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsResult {
                extension_id: Some("leoocgnkjnplbfdbklajepahofecgfbk".to_owned()),
                target_id: "chrome-tab:7".to_owned(),
                tab_id: 7,
                chrome_window_id: Some(3),
                operation: "set".to_owned(),
                requested: Some(
                    crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsOverride {
                        offline: true,
                        latency_ms: 100.0,
                        download_throughput_bytes_per_sec: 50_000.0,
                        upload_throughput_bytes_per_sec: 20_000.0,
                        connection_type: Some("cellular3g".to_owned()),
                    },
                ),
                page_url: "https://example.test/".to_owned(),
                page_title: "Example".to_owned(),
                ready_state: "complete".to_owned(),
                network: crate::chrome_debugger_bridge::ChromeDebuggerNetworkConditionsReadback {
                    online: false,
                    connection_type: Some("cellular3g".to_owned()),
                    effective_type: Some("3g".to_owned()),
                    downlink_mbps: Some(0.4),
                    rtt_ms: Some(100.0),
                    save_data: Some(false),
                },
                readback_backend: String::new(),
                backend_tier_used: String::new(),
                required_foreground: false,
                source_of_truth: String::new(),
                method: "Network.enable+Network.emulateNetworkConditions".to_owned(),
                debugger_protocol_version: Some("1.3".to_owned()),
                target_candidate_count: 1,
                target_selection_reason: "targetIdHint".to_owned(),
            },
        );
        assert_eq!(response.transport, "chrome_tabs_extension");
        assert_eq!(response.operation, BrowserNetworkConditionsOperation::Set);
        assert!(!response.network.online);
        assert_eq!(response.network.effective_type.as_deref(), Some("3g"));
        assert_eq!(
            response
                .requested
                .as_ref()
                .unwrap()
                .connection_type
                .as_deref(),
            Some("cellular3g")
        );
        assert_eq!(response.backend_tier_used, "chrome_tabs_extension_debugger");
        assert!(!response.required_foreground);
    }

    #[test]
    fn browser_route_add_fulfill_validation_defaults_and_encodes_body() {
        let normalized = validate_browser_route_params(&BrowserRouteParams {
            cdp_target_id: Some("target-123".to_owned()),
            route_id: Some("api-users".to_owned()),
            url: Some("https://example.test/api/*".to_owned()),
            resource_type: Some("XHR".to_owned()),
            headers: vec![BrowserRouteHeader {
                name: "content-type".to_owned(),
                value: "application/json".to_owned(),
            }],
            body: Some("{\"ok\":true}".to_owned()),
            ..Default::default()
        })
        .expect("valid route params pass");

        assert_eq!(normalized.operation, BrowserRouteOperation::AddFulfill);
        assert_eq!(normalized.route_id.as_deref(), Some("api-users"));
        let route = normalized.route.expect("route normalized");
        assert_eq!(route.id, "api-users");
        assert_eq!(route.url, "https://example.test/api/*");
        assert_eq!(route.match_kind, synapse_a11y::CdpFetchRouteMatchKind::Glob);
        assert_eq!(route.resource_type.as_deref(), Some("XHR"));
        let fulfill = match route.action {
            synapse_a11y::CdpFetchRouteAction::Fulfill(fulfill) => fulfill,
            synapse_a11y::CdpFetchRouteAction::Abort(_) => panic!("expected fulfill rule"),
            synapse_a11y::CdpFetchRouteAction::Continue(_) => panic!("expected fulfill rule"),
        };
        assert_eq!(fulfill.status, 200);
        assert_eq!(fulfill.headers[0].0, "content-type");
        assert_eq!(
            fulfill.body_base64.as_deref(),
            Some(BASE64_STANDARD.encode("{\"ok\":true}").as_str())
        );
    }

    #[test]
    fn browser_route_add_abort_validation_defaults_to_blocked_by_client() {
        let normalized = validate_browser_route_params(&BrowserRouteParams {
            operation: BrowserRouteOperation::AddAbort,
            route_id: Some("block-images".to_owned()),
            url: Some("https://example.test/assets/*".to_owned()),
            resource_type: Some("Image".to_owned()),
            ..Default::default()
        })
        .expect("valid abort params pass");

        assert_eq!(normalized.operation, BrowserRouteOperation::AddAbort);
        assert_eq!(normalized.route_id.as_deref(), Some("block-images"));
        let route = normalized.route.expect("route normalized");
        assert_eq!(route.id, "block-images");
        assert_eq!(route.resource_type.as_deref(), Some("Image"));
        let abort = match route.action {
            synapse_a11y::CdpFetchRouteAction::Abort(abort) => abort,
            synapse_a11y::CdpFetchRouteAction::Fulfill(_) => panic!("expected abort rule"),
            synapse_a11y::CdpFetchRouteAction::Continue(_) => panic!("expected abort rule"),
        };
        assert_eq!(abort.error_reason, "BlockedByClient");
    }

    #[test]
    fn browser_route_add_continue_validation_encodes_post_data() {
        let normalized = validate_browser_route_params(&BrowserRouteParams {
            operation: BrowserRouteOperation::AddContinue,
            route_id: Some("rewrite-api".to_owned()),
            url: Some("https://example.test/api/*".to_owned()),
            continue_url: Some("https://example.test/mock".to_owned()),
            continue_method: Some("POST".to_owned()),
            continue_headers: vec![BrowserRouteHeader {
                name: "x-test".to_owned(),
                value: "yes".to_owned(),
            }],
            continue_post_data: Some("{\"patched\":true}".to_owned()),
            ..Default::default()
        })
        .expect("valid continue params pass");

        assert_eq!(normalized.operation, BrowserRouteOperation::AddContinue);
        assert_eq!(normalized.route_id.as_deref(), Some("rewrite-api"));
        let route = normalized.route.expect("route normalized");
        let continue_rule = match route.action {
            synapse_a11y::CdpFetchRouteAction::Continue(continue_rule) => continue_rule,
            synapse_a11y::CdpFetchRouteAction::Fulfill(_) => panic!("expected continue rule"),
            synapse_a11y::CdpFetchRouteAction::Abort(_) => panic!("expected continue rule"),
        };
        assert_eq!(
            continue_rule.url.as_deref(),
            Some("https://example.test/mock")
        );
        assert_eq!(continue_rule.method.as_deref(), Some("POST"));
        assert_eq!(continue_rule.headers[0].0, "x-test");
        assert_eq!(
            continue_rule.post_data_base64.as_deref(),
            Some(BASE64_STANDARD.encode("{\"patched\":true}").as_str())
        );
    }

    #[test]
    fn browser_route_validation_edges() {
        for error in [
            validate_browser_route_params(&BrowserRouteParams {
                operation: BrowserRouteOperation::Remove,
                ..Default::default()
            })
            .expect_err("remove requires route_id"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("bad id".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                ..Default::default()
            })
            .expect_err("route id whitespace must be rejected"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("bad-status".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                status: Some(99),
                ..Default::default()
            })
            .expect_err("bad status must be rejected"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("bad-regex".to_owned()),
                url: Some("[".to_owned()),
                match_kind: BrowserRouteMatchKind::Regex,
                ..Default::default()
            })
            .expect_err("bad regex must be rejected"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("two-bodies".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                body: Some("plain".to_owned()),
                body_base64: Some("cGxhaW4=".to_owned()),
                ..Default::default()
            })
            .expect_err("body and body_base64 are mutually exclusive"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("bad-base64".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                body_base64: Some("not base64".to_owned()),
                ..Default::default()
            })
            .expect_err("invalid base64 must be rejected"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("bad-header".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                headers: vec![BrowserRouteHeader {
                    name: "bad header".to_owned(),
                    value: "value".to_owned(),
                }],
                ..Default::default()
            })
            .expect_err("bad header name must be rejected"),
            validate_browser_route_params(&BrowserRouteParams {
                operation: BrowserRouteOperation::AddAbort,
                route_id: Some("abort-with-body".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                body: Some("plain".to_owned()),
                ..Default::default()
            })
            .expect_err("abort rejects fulfill-only fields"),
            validate_browser_route_params(&BrowserRouteParams {
                route_id: Some("fulfill-with-reason".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                error_reason: Some(BrowserRouteErrorReason::Aborted),
                ..Default::default()
            })
            .expect_err("fulfill rejects error_reason"),
            validate_browser_route_params(&BrowserRouteParams {
                operation: BrowserRouteOperation::AddContinue,
                route_id: Some("continue-empty".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                ..Default::default()
            })
            .expect_err("continue requires at least one override"),
            validate_browser_route_params(&BrowserRouteParams {
                operation: BrowserRouteOperation::AddContinue,
                route_id: Some("continue-bad-method".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                continue_method: Some("BAD METHOD".to_owned()),
                ..Default::default()
            })
            .expect_err("continue method token must be valid"),
            validate_browser_route_params(&BrowserRouteParams {
                operation: BrowserRouteOperation::AddContinue,
                route_id: Some("continue-two-bodies".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                continue_post_data: Some("plain".to_owned()),
                continue_post_data_base64: Some("cGxhaW4=".to_owned()),
                ..Default::default()
            })
            .expect_err("continue postData inputs are mutually exclusive"),
            validate_browser_route_params(&BrowserRouteParams {
                operation: BrowserRouteOperation::AddContinue,
                route_id: Some("continue-fulfill-fields".to_owned()),
                url: Some("https://example.test/*".to_owned()),
                status: Some(204),
                continue_method: Some("GET".to_owned()),
                ..Default::default()
            })
            .expect_err("continue rejects fulfill-only fields"),
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
    fn browser_route_rule_wire_hides_body_content() {
        let rule = synapse_a11y::CdpFetchRouteRule {
            id: "api-users".to_owned(),
            url: "https://example.test/api/*".to_owned(),
            match_kind: synapse_a11y::CdpFetchRouteMatchKind::Regex,
            method: None,
            resource_type: Some("XHR".to_owned()),
            action: synapse_a11y::CdpFetchRouteAction::Fulfill(
                synapse_a11y::CdpFetchRouteFulfill {
                    status: 204,
                    response_phrase: Some("No Content".to_owned()),
                    headers: vec![("x-test".to_owned(), "yes".to_owned())],
                    body_base64: Some("c2VjcmV0".to_owned()),
                },
            ),
        };

        let wire = browser_route_rule_to_wire(&rule);
        assert_eq!(wire.id, "api-users");
        assert_eq!(wire.match_kind, BrowserRouteMatchKind::Regex);
        assert_eq!(wire.status, Some(204));
        assert_eq!(wire.error_reason, None);
        assert_eq!(wire.response_phrase.as_deref(), Some("No Content"));
        assert_eq!(wire.headers[0].name, "x-test");
        assert_eq!(wire.body_base64_len_chars, Some(8));
    }

    #[test]
    fn browser_route_rule_wire_maps_abort_reason() {
        let rule = synapse_a11y::CdpFetchRouteRule {
            id: "block-images".to_owned(),
            url: "https://example.test/assets/*".to_owned(),
            match_kind: synapse_a11y::CdpFetchRouteMatchKind::Glob,
            method: None,
            resource_type: Some("Image".to_owned()),
            action: synapse_a11y::CdpFetchRouteAction::Abort(synapse_a11y::CdpFetchRouteAbort {
                error_reason: "BlockedByClient".to_owned(),
            }),
        };

        let wire = browser_route_rule_to_wire(&rule);
        assert_eq!(wire.id, "block-images");
        assert_eq!(wire.action, "abort");
        assert_eq!(wire.status, None);
        assert_eq!(wire.error_reason.as_deref(), Some("BlockedByClient"));
        assert!(wire.headers.is_empty());
        assert_eq!(wire.body_base64_len_chars, None);
        assert_eq!(wire.post_data_base64_len_chars, None);
    }

    #[test]
    fn browser_route_rule_wire_maps_continue_overrides() {
        let rule = synapse_a11y::CdpFetchRouteRule {
            id: "rewrite-api".to_owned(),
            url: "https://example.test/api/*".to_owned(),
            match_kind: synapse_a11y::CdpFetchRouteMatchKind::Glob,
            method: None,
            resource_type: Some("Fetch".to_owned()),
            action: synapse_a11y::CdpFetchRouteAction::Continue(
                synapse_a11y::CdpFetchRouteContinue {
                    url: Some("https://example.test/mock".to_owned()),
                    method: Some("POST".to_owned()),
                    headers: vec![("x-test".to_owned(), "yes".to_owned())],
                    post_data_base64: Some("eyJwYXRjaGVkIjp0cnVlfQ==".to_owned()),
                },
            ),
        };

        let wire = browser_route_rule_to_wire(&rule);
        assert_eq!(wire.id, "rewrite-api");
        assert_eq!(wire.action, "continue");
        assert_eq!(wire.status, None);
        assert_eq!(
            wire.continue_url.as_deref(),
            Some("https://example.test/mock")
        );
        assert_eq!(wire.continue_method.as_deref(), Some("POST"));
        assert_eq!(wire.headers[0].name, "x-test");
        assert_eq!(wire.post_data_base64_len_chars, Some(24));
    }

    #[test]
    fn browser_route_fetch_status_maps_a11y_counters() {
        let wire = browser_route_fetch_status_from_a11y(
            Some(synapse_a11y::CdpFetchInterceptionStatus {
                newly_armed: true,
                endpoint: "http://127.0.0.1:9222".to_owned(),
                cdp_target_id: "target-123".to_owned(),
                armed_at_unix_ms: 42,
                pattern_count: 0,
                route_count: 2,
                paused_count: 3,
                continued_count: 1,
                fulfilled_count: 2,
                failed_count: 1,
                continue_error_count: 0,
                last_request_id: Some("fetch-1".to_owned()),
                last_url: Some("https://example.test/api".to_owned()),
                last_route_id: Some("api-users".to_owned()),
                last_error: None,
            }),
            true,
        );

        assert!(wire.fetch_armed);
        assert!(wire.newly_armed);
        assert_eq!(wire.route_count, 2);
        assert_eq!(wire.fulfilled_count, 2);
        assert_eq!(wire.failed_count, 1);
        assert_eq!(wire.last_route_id.as_deref(), Some("api-users"));
    }

    #[test]
    fn browser_network_request_detail_maps_full_entry_and_body_metadata() {
        let mut captured = entry(
            9,
            "api-ok",
            "https://example.test/api/users",
            "XHR",
            Some(200),
        );
        captured.first_seq = 7;
        captured.request_has_post_data = Some(true);
        captured.initiator = Some(json!({"type": "script"}));
        captured
            .redirects
            .push(synapse_a11y::CdpNetworkResponseSnapshot {
                url: "https://example.test/old".to_owned(),
                status: 302,
                status_text: "Found".to_owned(),
                headers: json!({"location": "/api/users"}),
                request_headers: None,
                mime_type: "text/html".to_owned(),
                protocol: Some("h2".to_owned()),
                remote_ip_address: Some("127.0.0.1".to_owned()),
                remote_port: Some(443),
                encoded_data_length: 10.0,
                timing: None,
                response_time_ms: Some(3.0),
                from_disk_cache: Some(false),
                from_service_worker: Some(false),
                from_prefetch_cache: Some(false),
                from_early_hints: Some(false),
                timestamp_s: Some(1.5),
                resource_type: Some("XHR".to_owned()),
            });

        let detail = browser_network_request_detail_to_wire(&captured);
        assert_eq!(detail.seq, 9);
        assert_eq!(detail.first_seq, 7);
        assert_eq!(detail.request_id, "api-ok");
        assert_eq!(detail.request_has_post_data, Some(true));
        assert_eq!(detail.initiator, Some(json!({"type": "script"})));
        assert_eq!(detail.redirects.len(), 1);
        assert_eq!(detail.response.as_ref().map(|r| r.status), Some(200));

        let body = browser_network_response_body_to_wire(synapse_a11y::CdpNetworkResponseBody {
            request_id: "api-ok".to_owned(),
            body: "{\"ok\":true}".to_owned(),
            base64_encoded: false,
        });
        assert_eq!(body.body_len_chars, 11);
        assert!(!body.base64_encoded);

        let post_data =
            browser_network_post_data_to_wire(synapse_a11y::CdpNetworkRequestPostData {
                request_id: "api-ok".to_owned(),
                post_data: "{\"name\":\"Ada\"}".to_owned(),
            });
        assert_eq!(post_data.post_data_len_chars, 14);
    }

    #[test]
    fn browser_network_request_body_requires_completed_response() {
        let mut pending = entry(1, "pending", "https://example.test/api", "XHR", Some(200));
        pending.loading_finished = false;
        let error = require_response_body_available(&pending)
            .expect_err("pending response body must be rejected");
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(code, Some(error_codes::A11Y_CDP_AXTREE_FAILED));
    }
}
