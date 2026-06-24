//! Persistent per-target CDP Network capture (#1080).
//!
//! Browser automation needs the same request/response visibility Playwright
//! exposes through `page.on("request")`, `page.on("response")`, and request
//! completion/failure events. CDP does not replay old Network events after
//! `Network.enable`, so this module mirrors `cdp_console`: one long-lived CDP
//! connection per armed target, a live event pump, and a bounded ring buffer
//! that can be read by cursor without consuming entries.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chromiumoxide::cdp::browser_protocol::emulation::SetUserAgentOverrideParams as EmulationSetUserAgentOverrideParams;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams as FetchContinueRequestParams, DisableParams as FetchDisableParams,
    EnableParams as FetchEnableParams, EventRequestPaused as FetchEventRequestPaused,
    FailRequestParams as FetchFailRequestParams, FulfillRequestParams as FetchFulfillRequestParams,
    HeaderEntry as FetchHeaderEntry, RequestId as FetchRequestId,
    RequestPattern as FetchRequestPattern, RequestStage as FetchRequestStage,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, ErrorReason as NetworkErrorReason, EventLoadingFailed,
    EventLoadingFinished, EventRequestWillBeSent, EventResponseReceived, EventWebSocketClosed,
    EventWebSocketCreated, EventWebSocketFrameError, EventWebSocketFrameReceived,
    EventWebSocketFrameSent, EventWebSocketHandshakeResponseReceived,
    EventWebSocketWillSendHandshakeRequest, GetRequestPostDataParams, GetResponseBodyParams,
    Headers, ResourceType as NetworkResourceType, Response,
    SetExtraHttpHeadersParams as NetworkSetExtraHttpHeadersParams, WebSocketFrame,
};
use chromiumoxide::{Browser, Page};
use futures_util::StreamExt as _;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::{A11yError, A11yResult};

/// Default network buffer capacity (request records) per captured target.
pub const DEFAULT_NETWORK_BUFFER_CAPACITY: usize = 1000;
/// Hard ceiling on requested network buffer capacity.
pub const MAX_NETWORK_BUFFER_CAPACITY: usize = 10_000;
/// Per-WebSocket frame retention cap so chatty sockets cannot grow unbounded.
pub const MAX_WEBSOCKET_FRAMES_PER_ENTRY: usize = 1000;

/// A response snapshot captured either as the current response or as a redirect
/// response attached to the next `Network.requestWillBeSent` event.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpNetworkResponseSnapshot {
    pub url: String,
    pub status: i64,
    pub status_text: String,
    pub headers: Value,
    pub request_headers: Option<Value>,
    pub mime_type: String,
    pub protocol: Option<String>,
    pub remote_ip_address: Option<String>,
    pub remote_port: Option<i64>,
    pub encoded_data_length: f64,
    pub timing: Option<Value>,
    pub response_time_ms: Option<f64>,
    pub from_disk_cache: Option<bool>,
    pub from_service_worker: Option<bool>,
    pub from_prefetch_cache: Option<bool>,
    pub from_early_hints: Option<bool>,
    pub timestamp_s: Option<f64>,
    pub resource_type: Option<String>,
}

/// One request record keyed by CDP `requestId`. `seq` is the last event cursor
/// for this request; `first_seq` is the cursor when the record was created.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpNetworkEntry {
    pub seq: u64,
    pub first_seq: u64,
    pub request_id: String,
    pub loader_id: Option<String>,
    pub frame_id: Option<String>,
    pub document_url: Option<String>,
    pub url: Option<String>,
    pub method: Option<String>,
    pub resource_type: Option<String>,
    pub request_headers: Option<Value>,
    pub request_has_post_data: Option<bool>,
    pub request_timestamp_s: Option<f64>,
    pub request_wall_time_ms: Option<f64>,
    pub initiator: Option<Value>,
    pub redirects: Vec<CdpNetworkResponseSnapshot>,
    pub response: Option<CdpNetworkResponseSnapshot>,
    pub response_timestamp_s: Option<f64>,
    pub response_received: bool,
    pub loading_finished: bool,
    pub loading_failed: bool,
    pub finished_timestamp_s: Option<f64>,
    pub failed_timestamp_s: Option<f64>,
    pub encoded_data_length: Option<f64>,
    pub failure_error_text: Option<String>,
    pub failure_canceled: Option<bool>,
    pub failure_blocked_reason: Option<String>,
    pub failure_cors_error_status: Option<Value>,
}

impl CdpNetworkEntry {
    fn new(seq: u64, request_id: String) -> Self {
        Self {
            seq,
            first_seq: seq,
            request_id,
            loader_id: None,
            frame_id: None,
            document_url: None,
            url: None,
            method: None,
            resource_type: None,
            request_headers: None,
            request_has_post_data: None,
            request_timestamp_s: None,
            request_wall_time_ms: None,
            initiator: None,
            redirects: Vec::new(),
            response: None,
            response_timestamp_s: None,
            response_received: false,
            loading_finished: false,
            loading_failed: false,
            finished_timestamp_s: None,
            failed_timestamp_s: None,
            encoded_data_length: None,
            failure_error_text: None,
            failure_canceled: None,
            failure_blocked_reason: None,
            failure_cors_error_status: None,
        }
    }
}

/// Result of arming network capture for a target.
#[derive(Clone, Debug, Serialize)]
pub struct CdpNetworkCaptureStatus {
    pub newly_armed: bool,
    pub armed_at_unix_ms: f64,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub capacity: usize,
}

/// Optional filters for [`network_capture_read`].
#[derive(Clone, Debug, Default)]
pub struct CdpNetworkReadFilter<'a> {
    /// Only entries whose latest event has `seq >= since_seq`.
    pub since_seq: Option<u64>,
    /// Exact CDP request id match.
    pub request_id: Option<&'a str>,
    /// Case-insensitive substring match against the current request URL.
    pub url_contains: Option<&'a str>,
    /// Exact resource type match, e.g. `Document`, `Script`, `XHR`.
    pub resource_type: Option<&'a str>,
    /// Exact HTTP response status match.
    pub status: Option<i64>,
    /// If set, returns only failed or non-failed records.
    pub failed: Option<bool>,
    /// Maximum entries to return (oldest-first by latest event cursor).
    pub max: usize,
}

/// A cursor-delimited view of the network capture buffer.
#[derive(Clone, Debug, Serialize)]
pub struct CdpNetworkReadResult {
    pub entries: Vec<CdpNetworkEntry>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub armed_at_unix_ms: f64,
    pub capacity: usize,
}

/// One captured WebSocket frame or frame error.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpWebSocketFrame {
    pub seq: u64,
    /// `sent`, `received`, or `error`.
    pub direction: String,
    pub timestamp_s: Option<f64>,
    pub opcode: Option<f64>,
    pub mask: Option<bool>,
    pub payload_data: Option<String>,
    pub payload_len_chars: usize,
    pub payload_base64_encoded: bool,
    pub close_code: Option<u16>,
    pub close_reason: Option<String>,
    pub error_message: Option<String>,
}

/// One WebSocket lifecycle record keyed by CDP request id.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpWebSocketEntry {
    pub seq: u64,
    pub first_seq: u64,
    pub request_id: String,
    pub url: Option<String>,
    pub created: bool,
    pub created_at_unix_ms: Option<f64>,
    pub initiator: Option<Value>,
    pub handshake_request_timestamp_s: Option<f64>,
    pub handshake_request_wall_time_ms: Option<f64>,
    pub handshake_request_headers: Option<Value>,
    pub handshake_response_timestamp_s: Option<f64>,
    pub status: Option<i64>,
    pub status_text: Option<String>,
    pub handshake_response_headers: Option<Value>,
    pub handshake_response_headers_text: Option<String>,
    pub handshake_response_request_headers: Option<Value>,
    pub handshake_response_request_headers_text: Option<String>,
    pub frames: Vec<CdpWebSocketFrame>,
    pub sent_frame_count: u64,
    pub received_frame_count: u64,
    pub frame_error_count: u64,
    pub dropped_frames: u64,
    pub closed: bool,
    pub closed_timestamp_s: Option<f64>,
    pub close_code: Option<u16>,
    pub close_reason: Option<String>,
}

impl CdpWebSocketEntry {
    fn new(seq: u64, request_id: String) -> Self {
        Self {
            seq,
            first_seq: seq,
            request_id,
            url: None,
            created: false,
            created_at_unix_ms: None,
            initiator: None,
            handshake_request_timestamp_s: None,
            handshake_request_wall_time_ms: None,
            handshake_request_headers: None,
            handshake_response_timestamp_s: None,
            status: None,
            status_text: None,
            handshake_response_headers: None,
            handshake_response_headers_text: None,
            handshake_response_request_headers: None,
            handshake_response_request_headers_text: None,
            frames: Vec::new(),
            sent_frame_count: 0,
            received_frame_count: 0,
            frame_error_count: 0,
            dropped_frames: 0,
            closed: false,
            closed_timestamp_s: None,
            close_code: None,
            close_reason: None,
        }
    }
}

/// Optional filters for [`network_web_socket_read`].
#[derive(Clone, Debug, Default)]
pub struct CdpWebSocketReadFilter<'a> {
    pub since_seq: Option<u64>,
    pub request_id: Option<&'a str>,
    pub url_contains: Option<&'a str>,
    pub max: usize,
}

/// A cursor-delimited view of captured WebSocket lifecycle/frame buffers.
#[derive(Clone, Debug, Serialize)]
pub struct CdpWebSocketReadResult {
    pub entries: Vec<CdpWebSocketEntry>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub armed_at_unix_ms: f64,
    pub capacity: usize,
}

/// Response body returned by `Network.getResponseBody` for a captured request.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpNetworkResponseBody {
    pub request_id: String,
    pub body: String,
    pub base64_encoded: bool,
}

/// Request body returned by `Network.getRequestPostData` for a captured request.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpNetworkRequestPostData {
    pub request_id: String,
    pub post_data: String,
}

/// Target-scoped Network/Emulation override state (#1087).
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CdpNetworkOverrideConfig {
    pub headers: Vec<(String, String)>,
    pub user_agent: Option<String>,
}

/// Readback after applying or clearing target-scoped network overrides.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpNetworkOverrideStatus {
    pub newly_armed: bool,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub armed_at_unix_ms: u64,
    pub applied_at_unix_ms: u64,
    pub header_count: usize,
    pub headers: Vec<(String, String)>,
    pub user_agent: Option<String>,
    pub original_user_agent: Option<String>,
}

/// Fetch interception stage for [`CdpFetchInterceptionPattern`].
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub enum CdpFetchInterceptionStage {
    #[default]
    Request,
    Response,
}

/// CDP Fetch interception pattern. An empty pattern list passed to
/// [`fetch_interception_ensure`] means "intercept all requests".
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CdpFetchInterceptionPattern {
    pub url_pattern: Option<String>,
    pub resource_type: Option<String>,
    pub request_stage: CdpFetchInterceptionStage,
}

/// Status/readback for the continue-by-default Fetch interception scaffold.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpFetchInterceptionStatus {
    pub newly_armed: bool,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub armed_at_unix_ms: u64,
    pub pattern_count: usize,
    pub route_count: usize,
    pub paused_count: u64,
    pub continued_count: u64,
    pub fulfilled_count: u64,
    pub failed_count: u64,
    pub continue_error_count: u64,
    pub last_request_id: Option<String>,
    pub last_url: Option<String>,
    pub last_route_id: Option<String>,
    pub last_error: Option<String>,
}

/// URL match kind for Fetch route rules.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub enum CdpFetchRouteMatchKind {
    #[default]
    Glob,
    Regex,
}

/// Synthetic response used by a Fetch route fulfill rule.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CdpFetchRouteFulfill {
    pub status: i64,
    pub response_phrase: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Base64-encoded response body, as expected by CDP Fetch.fulfillRequest.
    pub body_base64: Option<String>,
}

/// Network error used by a Fetch route abort rule.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpFetchRouteAbort {
    /// CDP Network.ErrorReason, e.g. `BlockedByClient` or `Aborted`.
    pub error_reason: String,
}

/// Request overrides used by a Fetch route continue rule.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CdpFetchRouteContinue {
    pub url: Option<String>,
    pub method: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Base64-encoded request body, as expected by CDP Fetch.continueRequest.
    pub post_data_base64: Option<String>,
}

/// Action for a Fetch route rule.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum CdpFetchRouteAction {
    Fulfill(CdpFetchRouteFulfill),
    Abort(CdpFetchRouteAbort),
    Continue(CdpFetchRouteContinue),
}

/// Per-target Fetch route rule. Rules are evaluated in insertion order; the
/// first match handles the paused request.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpFetchRouteRule {
    pub id: String,
    pub url: String,
    pub match_kind: CdpFetchRouteMatchKind,
    pub method: Option<String>,
    pub resource_type: Option<String>,
    pub action: CdpFetchRouteAction,
}

struct RingBuffer {
    entries: VecDeque<CdpNetworkEntry>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            capacity: capacity.max(1),
            next_seq: 0,
            dropped: 0,
        }
    }

    fn cursor(&self) -> u64 {
        self.next_seq
    }

    fn reserve_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    fn entry_for_event(&mut self, request_id: &str) -> &mut CdpNetworkEntry {
        let seq = self.reserve_seq();
        let index = if let Some(index) =
            self.entries.iter().position(|e| e.request_id == request_id)
        {
            index
        } else {
            while self.entries.len() >= self.capacity {
                self.entries.pop_front();
                self.dropped += 1;
            }
            self.entries
                .push_back(CdpNetworkEntry::new(seq, request_id.to_owned()));
            self.entries.len() - 1
        };
        let entry = &mut self.entries[index];
        entry.seq = seq;
        entry
    }

    fn apply_request_will_be_sent(&mut self, event: &EventRequestWillBeSent) {
        let entry = self.entry_for_event(event.request_id.inner());

        if let Some(redirect) = &event.redirect_response {
            let resource_type = event.r#type.as_ref().map(enum_str);
            entry
                .redirects
                .push(response_snapshot(redirect, None, resource_type.as_deref()));
            entry.response = None;
            entry.response_timestamp_s = None;
            entry.response_received = false;
            entry.loading_finished = false;
            entry.loading_failed = false;
            entry.finished_timestamp_s = None;
            entry.failed_timestamp_s = None;
            entry.encoded_data_length = None;
            entry.failure_error_text = None;
            entry.failure_canceled = None;
            entry.failure_blocked_reason = None;
            entry.failure_cors_error_status = None;
        }

        entry.loader_id = Some(event.loader_id.inner().clone());
        entry.frame_id = event.frame_id.as_ref().map(|id| id.inner().clone());
        entry.document_url = Some(event.document_url.clone());
        entry.url = Some(event.request.url.clone());
        entry.method = Some(event.request.method.clone());
        entry.resource_type = event.r#type.as_ref().map(enum_str);
        entry.request_headers = Some(headers_value(&event.request.headers));
        entry.request_has_post_data = event.request.has_post_data;
        entry.request_timestamp_s = Some(timestamp_s(&event.timestamp));
        entry.request_wall_time_ms = Some(timestamp_s(&event.wall_time) * 1000.0);
        entry.initiator = serde_json::to_value(&event.initiator).ok();
    }

    fn apply_response_received(&mut self, event: &EventResponseReceived) {
        let entry = self.entry_for_event(event.request_id.inner());
        entry.loader_id = Some(event.loader_id.inner().clone());
        entry.frame_id = event.frame_id.as_ref().map(|id| id.inner().clone());
        entry.resource_type = Some(enum_str(&event.r#type));
        entry.response = Some(response_snapshot(
            &event.response,
            Some(timestamp_s(&event.timestamp)),
            Some(&enum_str(&event.r#type)),
        ));
        entry.response_timestamp_s = Some(timestamp_s(&event.timestamp));
        entry.response_received = true;
        entry.loading_failed = false;
        entry.failure_error_text = None;
        entry.failure_canceled = None;
        entry.failure_blocked_reason = None;
        entry.failure_cors_error_status = None;
    }

    fn apply_loading_finished(&mut self, event: &EventLoadingFinished) {
        let entry = self.entry_for_event(event.request_id.inner());
        entry.loading_finished = true;
        entry.loading_failed = false;
        entry.finished_timestamp_s = Some(timestamp_s(&event.timestamp));
        entry.encoded_data_length = Some(event.encoded_data_length);
    }

    fn apply_loading_failed(&mut self, event: &EventLoadingFailed) {
        let entry = self.entry_for_event(event.request_id.inner());
        entry.resource_type = Some(enum_str(&event.r#type));
        entry.loading_finished = false;
        entry.loading_failed = true;
        entry.failed_timestamp_s = Some(timestamp_s(&event.timestamp));
        entry.failure_error_text = Some(event.error_text.clone());
        entry.failure_canceled = event.canceled;
        entry.failure_blocked_reason = event.blocked_reason.as_ref().map(enum_str);
        entry.failure_cors_error_status = event
            .cors_error_status
            .as_ref()
            .and_then(|status| serde_json::to_value(status).ok());
    }
}

struct WebSocketRingBuffer {
    entries: VecDeque<CdpWebSocketEntry>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
}

impl WebSocketRingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            capacity: capacity.max(1),
            next_seq: 0,
            dropped: 0,
        }
    }

    fn cursor(&self) -> u64 {
        self.next_seq
    }

    fn reserve_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    fn entry_for_seq(&mut self, request_id: &str, seq: u64) -> &mut CdpWebSocketEntry {
        let index = if let Some(index) =
            self.entries.iter().position(|e| e.request_id == request_id)
        {
            index
        } else {
            while self.entries.len() >= self.capacity {
                self.entries.pop_front();
                self.dropped = self.dropped.saturating_add(1);
            }
            self.entries
                .push_back(CdpWebSocketEntry::new(seq, request_id.to_owned()));
            self.entries.len() - 1
        };
        let entry = &mut self.entries[index];
        entry.seq = seq;
        entry
    }

    fn apply_created(&mut self, event: &EventWebSocketCreated) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.created = true;
        entry.created_at_unix_ms = Some(now_unix_ms());
        entry.url = Some(event.url.clone());
        entry.initiator = event
            .initiator
            .as_ref()
            .and_then(|initiator| serde_json::to_value(initiator).ok());
        entry.closed = false;
        entry.closed_timestamp_s = None;
    }

    fn apply_handshake_request(&mut self, event: &EventWebSocketWillSendHandshakeRequest) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.handshake_request_timestamp_s = Some(timestamp_s(&event.timestamp));
        entry.handshake_request_wall_time_ms = Some(timestamp_s(&event.wall_time) * 1000.0);
        entry.handshake_request_headers = Some(headers_value(&event.request.headers));
    }

    fn apply_handshake_response(&mut self, event: &EventWebSocketHandshakeResponseReceived) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.handshake_response_timestamp_s = Some(timestamp_s(&event.timestamp));
        entry.status = Some(event.response.status);
        entry.status_text = Some(event.response.status_text.clone());
        entry.handshake_response_headers = Some(headers_value(&event.response.headers));
        entry.handshake_response_headers_text = event.response.headers_text.clone();
        entry.handshake_response_request_headers =
            event.response.request_headers.as_ref().map(headers_value);
        entry.handshake_response_request_headers_text = event.response.request_headers_text.clone();
    }

    fn apply_frame_sent(&mut self, event: &EventWebSocketFrameSent) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.sent_frame_count = entry.sent_frame_count.saturating_add(1);
        push_websocket_frame(
            entry,
            websocket_frame_snapshot(
                seq,
                "sent",
                Some(timestamp_s(&event.timestamp)),
                &event.response,
            ),
        );
    }

    fn apply_frame_received(&mut self, event: &EventWebSocketFrameReceived) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.received_frame_count = entry.received_frame_count.saturating_add(1);
        push_websocket_frame(
            entry,
            websocket_frame_snapshot(
                seq,
                "received",
                Some(timestamp_s(&event.timestamp)),
                &event.response,
            ),
        );
    }

    fn apply_frame_error(&mut self, event: &EventWebSocketFrameError) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.frame_error_count = entry.frame_error_count.saturating_add(1);
        push_websocket_frame(
            entry,
            CdpWebSocketFrame {
                seq,
                direction: "error".to_owned(),
                timestamp_s: Some(timestamp_s(&event.timestamp)),
                opcode: None,
                mask: None,
                payload_data: None,
                payload_len_chars: 0,
                payload_base64_encoded: false,
                close_code: None,
                close_reason: None,
                error_message: Some(event.error_message.clone()),
            },
        );
    }

    fn apply_closed(&mut self, event: &EventWebSocketClosed) {
        let seq = self.reserve_seq();
        let entry = self.entry_for_seq(event.request_id.inner(), seq);
        entry.closed = true;
        entry.closed_timestamp_s = Some(timestamp_s(&event.timestamp));
    }
}

struct NetworkCaptureSlot {
    buffer: Arc<Mutex<RingBuffer>>,
    web_sockets: Arc<Mutex<WebSocketRingBuffer>>,
    endpoint: String,
    armed_at_unix_ms: f64,
    capacity: usize,
    page: Page,
    _browser: Browser,
    handler_task: JoinHandle<()>,
    listener_task: JoinHandle<()>,
}

impl Drop for NetworkCaptureSlot {
    fn drop(&mut self) {
        self.handler_task.abort();
        self.listener_task.abort();
    }
}

#[derive(Default)]
struct NetworkCaptureRegistry {
    slots: Mutex<HashMap<String, Arc<NetworkCaptureSlot>>>,
}

#[derive(Default)]
struct FetchInterceptionCounters {
    paused_count: u64,
    continued_count: u64,
    fulfilled_count: u64,
    failed_count: u64,
    continue_error_count: u64,
    last_request_id: Option<String>,
    last_url: Option<String>,
    last_route_id: Option<String>,
    last_error: Option<String>,
}

struct FetchInterceptionSlot {
    endpoint: String,
    target_id: String,
    armed_at_unix_ms: u64,
    patterns: Vec<CdpFetchInterceptionPattern>,
    rules: Arc<Mutex<Vec<CdpFetchRouteRule>>>,
    counters: Arc<Mutex<FetchInterceptionCounters>>,
    page: Page,
    _browser: Browser,
    handler_task: JoinHandle<()>,
    listener_task: JoinHandle<()>,
}

struct NetworkOverrideSlot {
    state: Arc<Mutex<CdpNetworkOverrideStatus>>,
    page: Page,
    _browser: Browser,
    handler_task: JoinHandle<()>,
}

enum FetchRouteApplied {
    Fulfilled,
    Failed,
    Continued,
}

impl Drop for FetchInterceptionSlot {
    fn drop(&mut self) {
        self.handler_task.abort();
        self.listener_task.abort();
    }
}

impl Drop for NetworkOverrideSlot {
    fn drop(&mut self) {
        self.handler_task.abort();
    }
}

#[derive(Default)]
struct FetchInterceptionRegistry {
    slots: Mutex<HashMap<String, Arc<FetchInterceptionSlot>>>,
}

#[derive(Default)]
struct NetworkOverrideRegistry {
    slots: Mutex<HashMap<String, Arc<NetworkOverrideSlot>>>,
}

fn registry() -> &'static NetworkCaptureRegistry {
    static REGISTRY: OnceLock<NetworkCaptureRegistry> = OnceLock::new();
    REGISTRY.get_or_init(NetworkCaptureRegistry::default)
}

fn fetch_registry() -> &'static FetchInterceptionRegistry {
    static REGISTRY: OnceLock<FetchInterceptionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(FetchInterceptionRegistry::default)
}

fn override_registry() -> &'static NetworkOverrideRegistry {
    static REGISTRY: OnceLock<NetworkOverrideRegistry> = OnceLock::new();
    REGISTRY.get_or_init(NetworkOverrideRegistry::default)
}

/// Arms (or re-arms) persistent CDP Network capture for `target_id`.
///
/// Idempotent: a live capture is reused. Capture starts at the arm time because
/// Chrome only sends Network events live after `Network.enable`.
pub async fn network_capture_ensure(
    endpoint: &str,
    target_id: &str,
    capacity: usize,
) -> A11yResult<CdpNetworkCaptureStatus> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "network capture target id must not be empty".to_owned(),
        });
    }

    if let Some(slot) = lookup_live(target_id) {
        return Ok(CdpNetworkCaptureStatus {
            newly_armed: false,
            armed_at_unix_ms: slot.armed_at_unix_ms,
            endpoint: slot.endpoint.clone(),
            cdp_target_id: target_id.to_owned(),
            capacity: slot.capacity,
        });
    }

    let capacity = capacity.clamp(1, MAX_NETWORK_BUFFER_CAPACITY);
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("network capture connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let armed = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(NetworkEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Network.enable for network capture: {err}"),
            })?;
        let request_started = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.requestWillBeSent: {err}"),
            })?;
        let response_received = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.responseReceived: {err}"),
            })?;
        let loading_finished = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.loadingFinished: {err}"),
            })?;
        let loading_failed = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.loadingFailed: {err}"),
            })?;
        let websocket_created = page
            .event_listener::<EventWebSocketCreated>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.webSocketCreated: {err}"),
            })?;
        let websocket_handshake_request = page
            .event_listener::<EventWebSocketWillSendHandshakeRequest>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.webSocketWillSendHandshakeRequest: {err}"),
            })?;
        let websocket_handshake_response = page
            .event_listener::<EventWebSocketHandshakeResponseReceived>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.webSocketHandshakeResponseReceived: {err}"),
            })?;
        let websocket_frame_sent = page
            .event_listener::<EventWebSocketFrameSent>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.webSocketFrameSent: {err}"),
            })?;
        let websocket_frame_received = page
            .event_listener::<EventWebSocketFrameReceived>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("subscribe Network.webSocketFrameReceived: {err}"),
        })?;
        let websocket_frame_error = page
            .event_listener::<EventWebSocketFrameError>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.webSocketFrameError: {err}"),
            })?;
        let websocket_closed = page
            .event_listener::<EventWebSocketClosed>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Network.webSocketClosed: {err}"),
            })?;
        Ok::<_, A11yError>((
            page,
            request_started,
            response_received,
            loading_finished,
            loading_failed,
            websocket_created,
            websocket_handshake_request,
            websocket_handshake_response,
            websocket_frame_sent,
            websocket_frame_received,
            websocket_frame_error,
            websocket_closed,
        ))
    }
    .await;

    let (
        page,
        mut request_started,
        mut response_received,
        mut loading_finished,
        mut loading_failed,
        mut websocket_created,
        mut websocket_handshake_request,
        mut websocket_handshake_response,
        mut websocket_frame_sent,
        mut websocket_frame_received,
        mut websocket_frame_error,
        mut websocket_closed,
    ) = match armed {
        Ok(streams) => streams,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };

    let buffer = Arc::new(Mutex::new(RingBuffer::new(capacity)));
    let pump_buffer = Arc::clone(&buffer);
    let web_sockets = Arc::new(Mutex::new(WebSocketRingBuffer::new(capacity)));
    let pump_web_sockets = Arc::clone(&web_sockets);
    let slot_page = page.clone();
    let listener_task = tokio::spawn(async move {
        let _page = page;
        loop {
            tokio::select! {
                Some(event) = request_started.next() => {
                    apply_request(&pump_buffer, event.as_ref());
                }
                Some(event) = response_received.next() => {
                    apply_response(&pump_buffer, event.as_ref());
                }
                Some(event) = loading_finished.next() => {
                    apply_finished(&pump_buffer, event.as_ref());
                }
                Some(event) = loading_failed.next() => {
                    apply_failed(&pump_buffer, event.as_ref());
                }
                Some(event) = websocket_created.next() => {
                    apply_websocket_created(&pump_web_sockets, event.as_ref());
                }
                Some(event) = websocket_handshake_request.next() => {
                    apply_websocket_handshake_request(&pump_web_sockets, event.as_ref());
                }
                Some(event) = websocket_handshake_response.next() => {
                    apply_websocket_handshake_response(&pump_web_sockets, event.as_ref());
                }
                Some(event) = websocket_frame_sent.next() => {
                    apply_websocket_frame_sent(&pump_web_sockets, event.as_ref());
                }
                Some(event) = websocket_frame_received.next() => {
                    apply_websocket_frame_received(&pump_web_sockets, event.as_ref());
                }
                Some(event) = websocket_frame_error.next() => {
                    apply_websocket_frame_error(&pump_web_sockets, event.as_ref());
                }
                Some(event) = websocket_closed.next() => {
                    apply_websocket_closed(&pump_web_sockets, event.as_ref());
                }
                else => break,
            }
        }
    });

    let armed_at_unix_ms = now_unix_ms();
    let slot = Arc::new(NetworkCaptureSlot {
        buffer,
        web_sockets,
        endpoint: endpoint.to_owned(),
        armed_at_unix_ms,
        capacity,
        page: slot_page,
        _browser: browser,
        handler_task,
        listener_task,
    });
    if let Ok(mut slots) = registry().slots.lock() {
        if let Some(existing) = slots.get(target_id)
            && !existing.listener_task.is_finished()
        {
            return Ok(CdpNetworkCaptureStatus {
                newly_armed: false,
                armed_at_unix_ms: existing.armed_at_unix_ms,
                endpoint: existing.endpoint.clone(),
                cdp_target_id: target_id.to_owned(),
                capacity: existing.capacity,
            });
        }
        slots.insert(target_id.to_owned(), slot);
    }

    Ok(CdpNetworkCaptureStatus {
        newly_armed: true,
        armed_at_unix_ms,
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
        capacity,
    })
}

/// Compatibility alias for callers that use the issue's CDP-prefixed naming.
pub async fn cdp_network_capture_start(
    endpoint: &str,
    target_id: &str,
    capacity: usize,
) -> A11yResult<CdpNetworkCaptureStatus> {
    network_capture_ensure(endpoint, target_id, capacity).await
}

/// Reads a filtered, cursor-delimited slice of a target's network buffer.
pub fn network_capture_read(
    target_id: &str,
    filter: &CdpNetworkReadFilter<'_>,
) -> Option<CdpNetworkReadResult> {
    let slot = {
        let slots = registry().slots.lock().ok()?;
        Arc::clone(slots.get(target_id.trim())?)
    };
    let buffer = slot.buffer.lock().ok()?;
    let total_buffered = buffer.entries.len();
    let next_cursor = buffer.cursor();
    let dropped = buffer.dropped;
    let max = if filter.max == 0 {
        usize::MAX
    } else {
        filter.max
    };
    let mut entries: Vec<CdpNetworkEntry> = buffer
        .entries
        .iter()
        .filter(|entry| filter.since_seq.is_none_or(|since| entry.seq >= since))
        .filter(|entry| filter.request_id.is_none_or(|id| entry.request_id == id))
        .filter(|entry| {
            filter.url_contains.is_none_or(|needle| {
                entry
                    .url
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&needle.to_lowercase())
            })
        })
        .filter(|entry| {
            filter.resource_type.is_none_or(|resource_type| {
                entry
                    .resource_type
                    .as_deref()
                    .is_some_and(|entry_type| entry_type.eq_ignore_ascii_case(resource_type))
            })
        })
        .filter(|entry| {
            filter
                .status
                .is_none_or(|status| entry.response.as_ref().is_some_and(|r| r.status == status))
        })
        .filter(|entry| {
            filter
                .failed
                .is_none_or(|failed| entry.loading_failed == failed)
        })
        .cloned()
        .collect();
    entries.sort_by_key(|entry| entry.seq);
    entries.truncate(max);

    Some(CdpNetworkReadResult {
        returned: entries.len(),
        entries,
        next_cursor,
        total_buffered,
        dropped,
        armed_at_unix_ms: slot.armed_at_unix_ms,
        capacity: slot.capacity,
    })
}

/// Reads a filtered, cursor-delimited slice of captured WebSocket entries.
pub fn network_web_socket_read(
    target_id: &str,
    filter: &CdpWebSocketReadFilter<'_>,
) -> Option<CdpWebSocketReadResult> {
    let slot = {
        let slots = registry().slots.lock().ok()?;
        Arc::clone(slots.get(target_id.trim())?)
    };
    let buffer = slot.web_sockets.lock().ok()?;
    let total_buffered = buffer.entries.len();
    let next_cursor = buffer.cursor();
    let dropped = buffer.dropped;
    let max = if filter.max == 0 {
        usize::MAX
    } else {
        filter.max
    };
    let mut entries: Vec<CdpWebSocketEntry> = buffer
        .entries
        .iter()
        .filter(|entry| filter.since_seq.is_none_or(|since| entry.seq >= since))
        .filter(|entry| filter.request_id.is_none_or(|id| entry.request_id == id))
        .filter(|entry| {
            filter.url_contains.is_none_or(|needle| {
                entry
                    .url
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&needle.to_lowercase())
            })
        })
        .cloned()
        .collect();
    entries.sort_by_key(|entry| entry.seq);
    entries.truncate(max);

    Some(CdpWebSocketReadResult {
        returned: entries.len(),
        entries,
        next_cursor,
        total_buffered,
        dropped,
        armed_at_unix_ms: slot.armed_at_unix_ms,
        capacity: slot.capacity,
    })
}

/// Reads the response body for a captured request from the persistent capture
/// CDP session. Chrome may evict bodies; that surfaces as a CDP error.
pub async fn network_response_body(
    target_id: &str,
    request_id: &str,
) -> A11yResult<CdpNetworkResponseBody> {
    let target_id = target_id.trim();
    let request_id = normalize_request_id(request_id)?;
    let slot = lookup_live(target_id).ok_or_else(|| A11yError::CdpAttachFailed {
        detail: format!("network capture for target {target_id} is not armed"),
    })?;
    let body = slot
        .page
        .execute(GetResponseBodyParams::new(request_id.to_owned()))
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Network.getResponseBody request_id={request_id}: {err}"),
        })?;
    Ok(CdpNetworkResponseBody {
        request_id: request_id.to_owned(),
        body: body.body.clone(),
        base64_encoded: body.base64_encoded,
    })
}

/// Reads request post data for a captured request from the persistent capture
/// CDP session. Chrome returns an error when no post data exists.
pub async fn network_request_post_data(
    target_id: &str,
    request_id: &str,
) -> A11yResult<CdpNetworkRequestPostData> {
    let target_id = target_id.trim();
    let request_id = normalize_request_id(request_id)?;
    let slot = lookup_live(target_id).ok_or_else(|| A11yError::CdpAttachFailed {
        detail: format!("network capture for target {target_id} is not armed"),
    })?;
    let post_data = slot
        .page
        .execute(GetRequestPostDataParams::new(request_id.to_owned()))
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Network.getRequestPostData request_id={request_id}: {err}"),
        })?;
    Ok(CdpNetworkRequestPostData {
        request_id: request_id.to_owned(),
        post_data: post_data.post_data.clone(),
    })
}

/// Applies target-scoped extra HTTP headers and optional User-Agent override.
/// The live CDP slot is kept so readback and clearing have a stable source.
pub async fn network_overrides_apply(
    endpoint: &str,
    target_id: &str,
    config: CdpNetworkOverrideConfig,
) -> A11yResult<CdpNetworkOverrideStatus> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "network override target id must not be empty".to_owned(),
        });
    }
    validate_network_override_config(&config)?;
    let (slot, newly_armed) = network_override_ensure(endpoint, target_id).await?;

    let headers_params = network_override_headers_params(&config.headers)?;
    slot.page
        .execute(headers_params)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Network.setExtraHTTPHeaders for target {target_id}: {err}"),
        })?;

    let (original_user_agent, had_user_agent_override) = {
        let state = slot.state.lock().ok();
        (
            state.as_ref().and_then(|s| s.original_user_agent.clone()),
            state.as_ref().is_some_and(|s| s.user_agent.is_some()),
        )
    };
    if let Some(user_agent) = config.user_agent.as_deref() {
        slot.page
            .execute(network_override_user_agent_params(user_agent)?)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Emulation.setUserAgentOverride for target {target_id}: {err}"),
            })?;
    } else if had_user_agent_override && let Some(original) = original_user_agent.as_deref() {
        slot.page
            .execute(network_override_user_agent_params(original)?)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Emulation.setUserAgentOverride restore for target {target_id}: {err}"
                ),
            })?;
    }

    let mut status = slot.state.lock().map_err(|_| A11yError::CdpAttachFailed {
        detail: "network override state lock is poisoned".to_owned(),
    })?;
    status.newly_armed = newly_armed;
    status.applied_at_unix_ms = now_unix_ms() as u64;
    status.header_count = config.headers.len();
    status.headers = config.headers;
    status.user_agent = config.user_agent;
    Ok(status.clone())
}

/// Returns the current tracked Network/Emulation override state for a target.
#[must_use]
pub fn network_overrides_status(target_id: &str) -> Option<CdpNetworkOverrideStatus> {
    let slot = lookup_override_live(target_id.trim())?;
    slot.state.lock().ok().map(|state| state.clone())
}

/// Clears target-scoped extra headers and restores the captured original UA.
/// Returns `None` if no override slot is active.
pub async fn network_overrides_clear(
    target_id: &str,
) -> A11yResult<Option<CdpNetworkOverrideStatus>> {
    let target_id = target_id.trim();
    let slot = override_registry()
        .slots
        .lock()
        .ok()
        .and_then(|mut slots| slots.remove(target_id));
    let Some(slot) = slot else {
        return Ok(None);
    };
    slot.page
        .execute(network_override_headers_params(&[])?)
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Network.setExtraHTTPHeaders clear for target {target_id}: {err}"),
        })?;
    let original_user_agent = {
        let state = slot.state.lock().ok();
        state.as_ref().and_then(|s| s.original_user_agent.clone())
    };
    if let Some(original) = original_user_agent.as_deref() {
        slot.page
            .execute(network_override_user_agent_params(original)?)
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Emulation.setUserAgentOverride restore for target {target_id}: {err}"
                ),
            })?;
    }
    let mut status = slot.state.lock().map_err(|_| A11yError::CdpAttachFailed {
        detail: "network override state lock is poisoned".to_owned(),
    })?;
    status.newly_armed = false;
    status.applied_at_unix_ms = now_unix_ms() as u64;
    status.header_count = 0;
    status.headers.clear();
    status.user_agent = None;
    Ok(Some(status.clone()))
}

/// Enables the Fetch domain for a target and continues every paused request by
/// default. This is the route/interception substrate; rules are layered later.
pub async fn fetch_interception_ensure(
    endpoint: &str,
    target_id: &str,
    patterns: Vec<CdpFetchInterceptionPattern>,
) -> A11yResult<CdpFetchInterceptionStatus> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "fetch interception target id must not be empty".to_owned(),
        });
    }

    if let Some(slot) = lookup_fetch_live(target_id) {
        if slot.patterns == patterns {
            return Ok(fetch_interception_status_from_slot(&slot, false));
        }
        let _ = fetch_interception_stop(target_id).await?;
    }

    let fetch_patterns = fetch_patterns_to_cdp(&patterns)?;
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("fetch interception connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let armed = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        let mut enable = FetchEnableParams::builder().handle_auth_requests(false);
        if !fetch_patterns.is_empty() {
            enable = enable.patterns(fetch_patterns);
        }
        page.execute(enable.build())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Fetch.enable for interception scaffold: {err}"),
            })?;
        let request_paused = page
            .event_listener::<FetchEventRequestPaused>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Fetch.requestPaused: {err}"),
            })?;
        Ok::<_, A11yError>((page, request_paused))
    }
    .await;

    let (page, mut request_paused) = match armed {
        Ok(armed) => armed,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };

    let counters = Arc::new(Mutex::new(FetchInterceptionCounters::default()));
    let rules = Arc::new(Mutex::new(Vec::new()));
    let pump_counters = Arc::clone(&counters);
    let pump_rules = Arc::clone(&rules);
    let slot_page = page.clone();
    let listener_page = page.clone();
    let listener_task = tokio::spawn(async move {
        let _page = page;
        while let Some(event) = request_paused.next().await {
            let event = event.as_ref();
            let request_id = fetch_request_id_string(&event.request_id);
            if let Ok(mut counters) = pump_counters.lock() {
                counters.paused_count = counters.paused_count.saturating_add(1);
                counters.last_request_id = Some(request_id.clone());
                counters.last_url = Some(event.request.url.clone());
            }
            let matched_rule = pump_rules
                .lock()
                .ok()
                .and_then(|rules| fetch_route_match(event, &rules));
            match matched_rule {
                Some(rule) => {
                    let route_id = rule.id.clone();
                    match fetch_apply_route(&listener_page, event, &rule).await {
                        Ok(applied) => {
                            if let Ok(mut counters) = pump_counters.lock() {
                                counters.last_route_id = Some(route_id);
                                match applied {
                                    FetchRouteApplied::Fulfilled => {
                                        counters.fulfilled_count =
                                            counters.fulfilled_count.saturating_add(1);
                                    }
                                    FetchRouteApplied::Failed => {
                                        counters.failed_count =
                                            counters.failed_count.saturating_add(1);
                                    }
                                    FetchRouteApplied::Continued => {
                                        counters.continued_count =
                                            counters.continued_count.saturating_add(1);
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            let mut last_error = format!(
                                "Fetch route {route_id} failed request_id={request_id}: {error}"
                            );
                            let continued = listener_page
                                .execute(FetchContinueRequestParams::new(event.request_id.clone()))
                                .await;
                            if let Ok(mut counters) = pump_counters.lock() {
                                counters.last_route_id = Some(route_id);
                                counters.continue_error_count =
                                    counters.continue_error_count.saturating_add(1);
                                match continued {
                                    Ok(_) => {
                                        counters.continued_count =
                                            counters.continued_count.saturating_add(1);
                                    }
                                    Err(continue_error) => {
                                        counters.continue_error_count =
                                            counters.continue_error_count.saturating_add(1);
                                        last_error.push_str(&format!(
                                            "; fallback Fetch.continueRequest failed: {continue_error}"
                                        ));
                                    }
                                }
                                counters.last_error = Some(last_error);
                            }
                        }
                    }
                }
                None => {
                    let continued = listener_page
                        .execute(FetchContinueRequestParams::new(event.request_id.clone()))
                        .await;
                    if let Ok(mut counters) = pump_counters.lock() {
                        match continued {
                            Ok(_) => {
                                counters.continued_count =
                                    counters.continued_count.saturating_add(1);
                            }
                            Err(error) => {
                                counters.continue_error_count =
                                    counters.continue_error_count.saturating_add(1);
                                counters.last_error = Some(format!(
                                    "Fetch.continueRequest request_id={request_id}: {error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    });

    let armed_at_unix_ms = now_unix_ms() as u64;
    let slot = Arc::new(FetchInterceptionSlot {
        endpoint: endpoint.to_owned(),
        target_id: target_id.to_owned(),
        armed_at_unix_ms,
        patterns,
        rules,
        counters,
        page: slot_page,
        _browser: browser,
        handler_task,
        listener_task,
    });
    if let Ok(mut slots) = fetch_registry().slots.lock() {
        if let Some(existing) = slots.get(target_id)
            && !existing.listener_task.is_finished()
        {
            return Ok(fetch_interception_status_from_slot(existing, false));
        }
        slots.insert(target_id.to_owned(), Arc::clone(&slot));
    }
    Ok(fetch_interception_status_from_slot(&slot, true))
}

/// Returns the current Fetch interception scaffold status for a target.
#[must_use]
pub fn fetch_interception_status(target_id: &str) -> Option<CdpFetchInterceptionStatus> {
    lookup_fetch_live(target_id.trim())
        .map(|slot| fetch_interception_status_from_slot(&slot, false))
}

/// Returns the configured Fetch route rules for a target.
#[must_use]
pub fn fetch_route_rules(target_id: &str) -> Option<Vec<CdpFetchRouteRule>> {
    let slot = lookup_fetch_live(target_id.trim())?;
    slot.rules.lock().ok().map(|rules| rules.clone())
}

/// Adds or replaces a Fetch route rule for an active interception target.
pub fn fetch_route_add(
    target_id: &str,
    rule: CdpFetchRouteRule,
) -> A11yResult<CdpFetchInterceptionStatus> {
    let target_id = target_id.trim();
    validate_fetch_route_rule(&rule)?;
    let slot = lookup_fetch_live(target_id).ok_or_else(|| A11yError::CdpAttachFailed {
        detail: format!("Fetch interception for target {target_id} is not armed"),
    })?;
    {
        let mut rules = slot.rules.lock().map_err(|_| A11yError::CdpAttachFailed {
            detail: "Fetch route registry lock is poisoned".to_owned(),
        })?;
        if let Some(existing) = rules.iter_mut().find(|existing| existing.id == rule.id) {
            *existing = rule;
        } else {
            rules.push(rule);
        }
    }
    Ok(fetch_interception_status_from_slot(&slot, false))
}

/// Removes one Fetch route rule for a target. Returns true when a rule existed.
pub fn fetch_route_remove(target_id: &str, route_id: &str) -> A11yResult<bool> {
    let target_id = target_id.trim();
    let route_id = normalize_route_id(route_id)?;
    let Some(slot) = lookup_fetch_live(target_id) else {
        return Ok(false);
    };
    let mut rules = slot.rules.lock().map_err(|_| A11yError::CdpAttachFailed {
        detail: "Fetch route registry lock is poisoned".to_owned(),
    })?;
    let before = rules.len();
    rules.retain(|rule| rule.id != route_id);
    Ok(rules.len() != before)
}

/// Clears Fetch route rules for a target. Returns the number of removed rules.
pub fn fetch_route_clear(target_id: &str) -> A11yResult<usize> {
    let target_id = target_id.trim();
    let Some(slot) = lookup_fetch_live(target_id) else {
        return Ok(0);
    };
    let mut rules = slot.rules.lock().map_err(|_| A11yError::CdpAttachFailed {
        detail: "Fetch route registry lock is poisoned".to_owned(),
    })?;
    let removed = rules.len();
    rules.clear();
    Ok(removed)
}

/// Disables Fetch interception for a target. Returns false when no slot exists.
pub async fn fetch_interception_stop(target_id: &str) -> A11yResult<bool> {
    let target_id = target_id.trim();
    let slot = fetch_registry()
        .slots
        .lock()
        .ok()
        .and_then(|mut slots| slots.remove(target_id));
    let Some(slot) = slot else {
        return Ok(false);
    };
    slot.page
        .execute(FetchDisableParams::default())
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Fetch.disable for target {target_id}: {err}"),
        })?;
    Ok(true)
}

/// Number of targets with an active Fetch interception scaffold.
#[must_use]
pub fn fetch_interception_active_count() -> usize {
    fetch_registry().slots.lock().map(|s| s.len()).unwrap_or(0)
}

/// Tears down network capture for a target. Idempotent.
pub fn network_capture_stop(target_id: &str) -> bool {
    registry()
        .slots
        .lock()
        .ok()
        .and_then(|mut slots| slots.remove(target_id.trim()))
        .is_some()
}

/// Clears buffered network records for a target while keeping capture armed.
pub fn network_capture_clear(target_id: &str) -> bool {
    let slot = {
        let slots = match registry().slots.lock() {
            Ok(slots) => slots,
            Err(_) => return false,
        };
        match slots.get(target_id.trim()) {
            Some(slot) => Arc::clone(slot),
            None => return false,
        }
    };
    match slot.buffer.lock() {
        Ok(mut buffer) => {
            *buffer = RingBuffer::new(slot.capacity);
            if let Ok(mut web_sockets) = slot.web_sockets.lock() {
                *web_sockets = WebSocketRingBuffer::new(slot.capacity);
            }
            true
        }
        Err(_) => false,
    }
}

/// Number of targets with a registered network capture slot.
#[must_use]
pub fn network_capture_active_count() -> usize {
    registry().slots.lock().map(|s| s.len()).unwrap_or(0)
}

fn lookup_live(target_id: &str) -> Option<Arc<NetworkCaptureSlot>> {
    let mut slots = registry().slots.lock().ok()?;
    match slots.get(target_id) {
        Some(slot) if !slot.listener_task.is_finished() => Some(Arc::clone(slot)),
        Some(_) => {
            slots.remove(target_id);
            None
        }
        None => None,
    }
}

fn lookup_fetch_live(target_id: &str) -> Option<Arc<FetchInterceptionSlot>> {
    let mut slots = fetch_registry().slots.lock().ok()?;
    match slots.get(target_id) {
        Some(slot) if !slot.listener_task.is_finished() => Some(Arc::clone(slot)),
        Some(_) => {
            slots.remove(target_id);
            None
        }
        None => None,
    }
}

fn lookup_override_live(target_id: &str) -> Option<Arc<NetworkOverrideSlot>> {
    let mut slots = override_registry().slots.lock().ok()?;
    match slots.get(target_id) {
        Some(slot) if !slot.handler_task.is_finished() => Some(Arc::clone(slot)),
        Some(_) => {
            slots.remove(target_id);
            None
        }
        None => None,
    }
}

async fn network_override_ensure(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<(Arc<NetworkOverrideSlot>, bool)> {
    if let Some(slot) = lookup_override_live(target_id) {
        return Ok((slot, false));
    }

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("network override connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = match crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await {
        Ok(page) => page,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };
    let original_user_agent = match read_page_user_agent(&page).await {
        Ok(user_agent) => Some(user_agent),
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };
    let armed_at_unix_ms = now_unix_ms() as u64;
    let status = CdpNetworkOverrideStatus {
        newly_armed: true,
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
        armed_at_unix_ms,
        applied_at_unix_ms: armed_at_unix_ms,
        header_count: 0,
        headers: Vec::new(),
        user_agent: None,
        original_user_agent,
    };
    let slot = Arc::new(NetworkOverrideSlot {
        state: Arc::new(Mutex::new(status)),
        page,
        _browser: browser,
        handler_task,
    });
    if let Ok(mut slots) = override_registry().slots.lock() {
        if let Some(existing) = slots.get(target_id)
            && !existing.handler_task.is_finished()
        {
            return Ok((Arc::clone(existing), false));
        }
        slots.insert(target_id.to_owned(), Arc::clone(&slot));
    }
    Ok((slot, true))
}

fn fetch_interception_status_from_slot(
    slot: &FetchInterceptionSlot,
    newly_armed: bool,
) -> CdpFetchInterceptionStatus {
    let counters = slot.counters.lock().ok();
    let route_count = slot.rules.lock().map_or(0, |rules| rules.len());
    CdpFetchInterceptionStatus {
        newly_armed,
        endpoint: slot.endpoint.clone(),
        cdp_target_id: slot.target_id.clone(),
        armed_at_unix_ms: slot.armed_at_unix_ms,
        pattern_count: slot.patterns.len(),
        route_count,
        paused_count: counters.as_ref().map_or(0, |c| c.paused_count),
        continued_count: counters.as_ref().map_or(0, |c| c.continued_count),
        fulfilled_count: counters.as_ref().map_or(0, |c| c.fulfilled_count),
        failed_count: counters.as_ref().map_or(0, |c| c.failed_count),
        continue_error_count: counters.as_ref().map_or(0, |c| c.continue_error_count),
        last_request_id: counters.as_ref().and_then(|c| c.last_request_id.clone()),
        last_url: counters.as_ref().and_then(|c| c.last_url.clone()),
        last_route_id: counters.as_ref().and_then(|c| c.last_route_id.clone()),
        last_error: counters.as_ref().and_then(|c| c.last_error.clone()),
    }
}

fn fetch_patterns_to_cdp(
    patterns: &[CdpFetchInterceptionPattern],
) -> A11yResult<Vec<FetchRequestPattern>> {
    patterns
        .iter()
        .map(fetch_pattern_to_cdp)
        .collect::<A11yResult<Vec<_>>>()
}

fn fetch_pattern_to_cdp(pattern: &CdpFetchInterceptionPattern) -> A11yResult<FetchRequestPattern> {
    let mut builder = FetchRequestPattern::builder().request_stage(match pattern.request_stage {
        CdpFetchInterceptionStage::Request => FetchRequestStage::Request,
        CdpFetchInterceptionStage::Response => FetchRequestStage::Response,
    });
    if let Some(url_pattern) = pattern.url_pattern.as_deref() {
        if url_pattern.is_empty() {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch request pattern url_pattern must not be empty".to_owned(),
            });
        }
        if url_pattern.contains('\0') {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch request pattern url_pattern must not contain NUL".to_owned(),
            });
        }
        builder = builder.url_pattern(url_pattern.to_owned());
    }
    if let Some(resource_type) = pattern.resource_type.as_deref() {
        if resource_type.trim() != resource_type || resource_type.is_empty() {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch request pattern resource_type must be non-empty without surrounding whitespace"
                    .to_owned(),
            });
        }
        let resource_type = resource_type
            .parse::<NetworkResourceType>()
            .map_err(|error| A11yError::CdpAttachFailed {
                detail: format!(
                    "Fetch request pattern resource_type {resource_type:?} is invalid: {error}"
                ),
            })?;
        builder = builder.resource_type(resource_type);
    }
    Ok(builder.build())
}

fn fetch_request_id_string(request_id: &FetchRequestId) -> String {
    <FetchRequestId as std::borrow::Borrow<str>>::borrow(request_id).to_owned()
}

async fn read_page_user_agent(page: &Page) -> A11yResult<String> {
    page.evaluate_expression("navigator.userAgent")
        .await
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.evaluate navigator.userAgent: {err}"),
        })?
        .into_value::<String>()
        .map_err(|err| A11yError::CdpAxtreeFailed {
            detail: format!("Runtime.evaluate navigator.userAgent decode: {err}"),
        })
}

fn network_override_headers_params(
    headers: &[(String, String)],
) -> A11yResult<NetworkSetExtraHttpHeadersParams> {
    for (name, value) in headers {
        validate_header_name(name)?;
        validate_header_value(value)?;
    }
    let headers_value = Value::Object(
        headers
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect(),
    );
    NetworkSetExtraHttpHeadersParams::builder()
        .headers(Headers::new(headers_value))
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Network.setExtraHTTPHeaders params: {detail}"),
        })
}

fn network_override_user_agent_params(
    user_agent: &str,
) -> A11yResult<EmulationSetUserAgentOverrideParams> {
    validate_user_agent(user_agent)?;
    EmulationSetUserAgentOverrideParams::builder()
        .user_agent(user_agent.to_owned())
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Emulation.setUserAgentOverride params: {detail}"),
        })
}

fn validate_network_override_config(config: &CdpNetworkOverrideConfig) -> A11yResult<()> {
    for (name, value) in &config.headers {
        validate_header_name(name)?;
        validate_header_value(value)?;
    }
    if let Some(user_agent) = config.user_agent.as_deref() {
        validate_user_agent(user_agent)?;
    }
    Ok(())
}

fn validate_user_agent(value: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "network override user_agent must be non-empty without surrounding whitespace"
                .to_owned(),
        });
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(A11yError::CdpAttachFailed {
            detail: "network override user_agent must not contain line breaks or NUL".to_owned(),
        });
    }
    Ok(())
}

fn fetch_route_match(
    event: &FetchEventRequestPaused,
    rules: &[CdpFetchRouteRule],
) -> Option<CdpFetchRouteRule> {
    rules
        .iter()
        .find(|rule| fetch_route_rule_matches(event, rule))
        .cloned()
}

fn fetch_route_rule_matches(event: &FetchEventRequestPaused, rule: &CdpFetchRouteRule) -> bool {
    if !fetch_route_url_matches(&event.request.url, rule) {
        return false;
    }
    if let Some(method) = rule.method.as_deref()
        && event.request.method != method
    {
        return false;
    }
    if let Some(resource_type) = rule.resource_type.as_deref()
        && !enum_str(&event.resource_type).eq_ignore_ascii_case(resource_type) {
            return false;
        }
    true
}

fn fetch_route_url_matches(url: &str, rule: &CdpFetchRouteRule) -> bool {
    match rule.match_kind {
        CdpFetchRouteMatchKind::Glob => glob_matches(&rule.url, url),
        CdpFetchRouteMatchKind::Regex => Regex::new(&rule.url)
            .map(|regex| regex.is_match(url))
            .unwrap_or(false),
    }
}

async fn fetch_apply_route(
    page: &Page,
    event: &FetchEventRequestPaused,
    rule: &CdpFetchRouteRule,
) -> A11yResult<FetchRouteApplied> {
    match &rule.action {
        CdpFetchRouteAction::Fulfill(fulfill) => {
            let params = fetch_fulfill_params(event.request_id.clone(), fulfill)?;
            page.execute(params)
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Fetch.fulfillRequest route_id={}: {err}", rule.id),
                })?;
            Ok(FetchRouteApplied::Fulfilled)
        }
        CdpFetchRouteAction::Abort(abort) => {
            let params = fetch_fail_params(event.request_id.clone(), abort)?;
            page.execute(params)
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Fetch.failRequest route_id={}: {err}", rule.id),
                })?;
            Ok(FetchRouteApplied::Failed)
        }
        CdpFetchRouteAction::Continue(continue_rule) => {
            let params = fetch_continue_params(event.request_id.clone(), continue_rule)?;
            page.execute(params)
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("Fetch.continueRequest route_id={}: {err}", rule.id),
                })?;
            Ok(FetchRouteApplied::Continued)
        }
    }
}

fn fetch_fulfill_params(
    request_id: FetchRequestId,
    fulfill: &CdpFetchRouteFulfill,
) -> A11yResult<FetchFulfillRequestParams> {
    validate_fetch_route_fulfill(fulfill)?;
    let headers = fulfill
        .headers
        .iter()
        .map(|(name, value)| FetchHeaderEntry::new(name.clone(), value.clone()));
    let mut builder = FetchFulfillRequestParams::builder()
        .request_id(request_id)
        .response_code(fulfill.status)
        .response_headers(headers);
    if let Some(body_base64) = fulfill.body_base64.as_deref() {
        builder = builder.body(body_base64.to_owned());
    }
    if let Some(response_phrase) = fulfill.response_phrase.as_deref() {
        builder = builder.response_phrase(response_phrase.to_owned());
    }
    builder
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Fetch.fulfillRequest route params: {detail}"),
        })
}

fn fetch_continue_params(
    request_id: FetchRequestId,
    continue_rule: &CdpFetchRouteContinue,
) -> A11yResult<FetchContinueRequestParams> {
    validate_fetch_route_continue(continue_rule)?;
    let mut builder = FetchContinueRequestParams::builder().request_id(request_id);
    if !continue_rule.headers.is_empty() {
        let headers = continue_rule
            .headers
            .iter()
            .map(|(name, value)| FetchHeaderEntry::new(name.clone(), value.clone()));
        builder = builder.headers(headers);
    }
    if let Some(url) = continue_rule.url.as_deref() {
        builder = builder.url(url.to_owned());
    }
    if let Some(method) = continue_rule.method.as_deref() {
        builder = builder.method(method.to_owned());
    }
    if let Some(post_data_base64) = continue_rule.post_data_base64.as_deref() {
        builder = builder.post_data(post_data_base64.to_owned());
    }
    builder
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Fetch.continueRequest route params: {detail}"),
        })
}

fn fetch_fail_params(
    request_id: FetchRequestId,
    abort: &CdpFetchRouteAbort,
) -> A11yResult<FetchFailRequestParams> {
    validate_fetch_route_abort(abort)?;
    let error_reason = abort
        .error_reason
        .parse::<NetworkErrorReason>()
        .map_err(|error| A11yError::CdpAttachFailed {
            detail: format!(
                "Fetch route abort error_reason {:?} is invalid: {error}",
                abort.error_reason
            ),
        })?;
    FetchFailRequestParams::builder()
        .request_id(request_id)
        .error_reason(error_reason)
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Fetch.failRequest route params: {detail}"),
        })
}

fn validate_fetch_route_rule(rule: &CdpFetchRouteRule) -> A11yResult<()> {
    normalize_route_id(&rule.id)?;
    if rule.url.trim() != rule.url || rule.url.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route url must be non-empty without surrounding whitespace".to_owned(),
        });
    }
    if rule.url.contains('\0') {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route url must not contain NUL".to_owned(),
        });
    }
    if matches!(rule.match_kind, CdpFetchRouteMatchKind::Regex) {
        Regex::new(&rule.url).map_err(|error| A11yError::CdpAttachFailed {
            detail: format!("Fetch route regex url is invalid: {error}"),
        })?;
    }
    if let Some(method) = rule.method.as_deref() {
        validate_http_method(method)?;
    }
    if let Some(resource_type) = rule.resource_type.as_deref() {
        validate_fetch_resource_type(resource_type, "Fetch route resource_type")?;
    }
    match &rule.action {
        CdpFetchRouteAction::Fulfill(fulfill) => validate_fetch_route_fulfill(fulfill)?,
        CdpFetchRouteAction::Abort(abort) => validate_fetch_route_abort(abort)?,
        CdpFetchRouteAction::Continue(continue_rule) => {
            validate_fetch_route_continue(continue_rule)?;
        }
    }
    Ok(())
}

fn validate_fetch_route_fulfill(fulfill: &CdpFetchRouteFulfill) -> A11yResult<()> {
    if !(100..=599).contains(&fulfill.status) {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route fulfill status must be between 100 and 599".to_owned(),
        });
    }
    if let Some(response_phrase) = fulfill.response_phrase.as_deref()
        && response_phrase.contains(['\r', '\n', '\0']) {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch route response_phrase must not contain control line breaks or NUL"
                    .to_owned(),
            });
        }
    for (name, value) in &fulfill.headers {
        validate_header_name(name)?;
        validate_header_value(value)?;
    }
    if let Some(body_base64) = fulfill.body_base64.as_deref()
        && body_base64.contains('\0') {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch route body_base64 must not contain NUL".to_owned(),
            });
        }
    Ok(())
}

fn validate_fetch_route_continue(continue_rule: &CdpFetchRouteContinue) -> A11yResult<()> {
    if continue_rule.url.is_none()
        && continue_rule.method.is_none()
        && continue_rule.headers.is_empty()
        && continue_rule.post_data_base64.is_none()
    {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route continue requires at least one override".to_owned(),
        });
    }
    if let Some(url) = continue_rule.url.as_deref()
        && (url.trim() != url || url.is_empty() || url.contains('\0')) {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch route continue url must be non-empty without surrounding whitespace or NUL"
                    .to_owned(),
            });
        }
    if let Some(method) = continue_rule.method.as_deref() {
        validate_http_method(method)?;
    }
    for (name, value) in &continue_rule.headers {
        validate_header_name(name)?;
        validate_header_value(value)?;
    }
    if let Some(post_data_base64) = continue_rule.post_data_base64.as_deref()
        && post_data_base64.contains('\0') {
            return Err(A11yError::CdpAttachFailed {
                detail: "Fetch route continue post_data_base64 must not contain NUL".to_owned(),
            });
        }
    Ok(())
}

fn validate_fetch_route_abort(abort: &CdpFetchRouteAbort) -> A11yResult<()> {
    if abort.error_reason.trim() != abort.error_reason || abort.error_reason.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail:
                "Fetch route abort error_reason must be non-empty without surrounding whitespace"
                    .to_owned(),
        });
    }
    abort
        .error_reason
        .parse::<NetworkErrorReason>()
        .map(|_| ())
        .map_err(|error| A11yError::CdpAttachFailed {
            detail: format!(
                "Fetch route abort error_reason {:?} is invalid: {error}",
                abort.error_reason
            ),
        })
}

fn validate_http_method(value: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route continue method must be non-empty without surrounding whitespace"
                .to_owned(),
        });
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
        return Err(A11yError::CdpAttachFailed {
            detail: format!("Fetch route continue method {value:?} contains an invalid byte"),
        });
    }
    Ok(())
}

fn normalize_route_id(route_id: &str) -> A11yResult<&str> {
    let route_id = route_id.trim();
    if route_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route id must not be empty".to_owned(),
        });
    }
    if route_id.contains(char::is_whitespace) || route_id.contains('\0') {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route id must not contain whitespace or NUL".to_owned(),
        });
    }
    Ok(route_id)
}

fn validate_fetch_resource_type(value: &str, field: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: format!("{field} must be non-empty without surrounding whitespace"),
        });
    }
    value
        .parse::<NetworkResourceType>()
        .map(|_| ())
        .map_err(|error| A11yError::CdpAttachFailed {
            detail: format!("{field} {value:?} is invalid: {error}"),
        })
}

fn validate_header_name(value: &str) -> A11yResult<()> {
    if value.trim() != value || value.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route header name must be non-empty without surrounding whitespace"
                .to_owned(),
        });
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
        return Err(A11yError::CdpAttachFailed {
            detail: format!("Fetch route header name {value:?} contains an invalid byte"),
        });
    }
    Ok(())
}

fn validate_header_value(value: &str) -> A11yResult<()> {
    if value.contains(['\r', '\n', '\0']) {
        return Err(A11yError::CdpAttachFailed {
            detail: "Fetch route header value must not contain line breaks or NUL".to_owned(),
        });
    }
    Ok(())
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let (mut p, mut c) = (0usize, 0usize);
    let mut star = None;
    let mut retry = 0usize;
    while c < candidate.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == candidate[c]) {
            p += 1;
            c += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            retry = c;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            retry += 1;
            c = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn normalize_request_id(request_id: &str) -> A11yResult<&str> {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "network request id must not be empty".to_owned(),
        });
    }
    Ok(request_id)
}

fn apply_request(buffer: &Arc<Mutex<RingBuffer>>, event: &EventRequestWillBeSent) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_request_will_be_sent(event);
    }
}

fn apply_response(buffer: &Arc<Mutex<RingBuffer>>, event: &EventResponseReceived) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_response_received(event);
    }
}

fn apply_finished(buffer: &Arc<Mutex<RingBuffer>>, event: &EventLoadingFinished) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_loading_finished(event);
    }
}

fn apply_failed(buffer: &Arc<Mutex<RingBuffer>>, event: &EventLoadingFailed) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_loading_failed(event);
    }
}

fn apply_websocket_created(
    buffer: &Arc<Mutex<WebSocketRingBuffer>>,
    event: &EventWebSocketCreated,
) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_created(event);
    }
}

fn apply_websocket_handshake_request(
    buffer: &Arc<Mutex<WebSocketRingBuffer>>,
    event: &EventWebSocketWillSendHandshakeRequest,
) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_handshake_request(event);
    }
}

fn apply_websocket_handshake_response(
    buffer: &Arc<Mutex<WebSocketRingBuffer>>,
    event: &EventWebSocketHandshakeResponseReceived,
) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_handshake_response(event);
    }
}

fn apply_websocket_frame_sent(
    buffer: &Arc<Mutex<WebSocketRingBuffer>>,
    event: &EventWebSocketFrameSent,
) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_frame_sent(event);
    }
}

fn apply_websocket_frame_received(
    buffer: &Arc<Mutex<WebSocketRingBuffer>>,
    event: &EventWebSocketFrameReceived,
) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_frame_received(event);
    }
}

fn apply_websocket_frame_error(
    buffer: &Arc<Mutex<WebSocketRingBuffer>>,
    event: &EventWebSocketFrameError,
) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_frame_error(event);
    }
}

fn apply_websocket_closed(buffer: &Arc<Mutex<WebSocketRingBuffer>>, event: &EventWebSocketClosed) {
    if let Ok(mut buf) = buffer.lock() {
        buf.apply_closed(event);
    }
}

fn push_websocket_frame(entry: &mut CdpWebSocketEntry, frame: CdpWebSocketFrame) {
    if let Some(close_code) = frame.close_code {
        entry.close_code = Some(close_code);
        entry.close_reason = frame.close_reason.clone();
    }
    while entry.frames.len() >= MAX_WEBSOCKET_FRAMES_PER_ENTRY {
        entry.frames.remove(0);
        entry.dropped_frames = entry.dropped_frames.saturating_add(1);
    }
    entry.frames.push(frame);
}

fn websocket_frame_snapshot(
    seq: u64,
    direction: &str,
    timestamp_s: Option<f64>,
    frame: &WebSocketFrame,
) -> CdpWebSocketFrame {
    let payload_base64_encoded = frame.opcode != 1.0;
    let (close_code, close_reason) = websocket_close_info(frame);
    CdpWebSocketFrame {
        seq,
        direction: direction.to_owned(),
        timestamp_s,
        opcode: Some(frame.opcode),
        mask: Some(frame.mask),
        payload_len_chars: frame.payload_data.chars().count(),
        payload_data: Some(frame.payload_data.clone()),
        payload_base64_encoded,
        close_code,
        close_reason,
        error_message: None,
    }
}

fn websocket_close_info(frame: &WebSocketFrame) -> (Option<u16>, Option<String>) {
    if frame.opcode != 8.0 {
        return (None, None);
    }
    let Ok(bytes) = BASE64_STANDARD.decode(&frame.payload_data) else {
        return (None, None);
    };
    if bytes.len() < 2 {
        return (None, None);
    }
    let code = u16::from_be_bytes([bytes[0], bytes[1]]);
    let reason = if bytes.len() > 2 {
        String::from_utf8(bytes[2..].to_vec()).ok()
    } else {
        None
    };
    (Some(code), reason)
}

fn response_snapshot(
    response: &Response,
    event_timestamp_s: Option<f64>,
    resource_type: Option<&str>,
) -> CdpNetworkResponseSnapshot {
    CdpNetworkResponseSnapshot {
        url: response.url.clone(),
        status: response.status,
        status_text: response.status_text.clone(),
        headers: headers_value(&response.headers),
        request_headers: response.request_headers.as_ref().map(headers_value),
        mime_type: response.mime_type.clone(),
        protocol: response.protocol.clone(),
        remote_ip_address: response.remote_ip_address.clone(),
        remote_port: response.remote_port,
        encoded_data_length: response.encoded_data_length,
        timing: response
            .timing
            .as_ref()
            .and_then(|timing| serde_json::to_value(timing).ok()),
        response_time_ms: response
            .response_time
            .as_ref()
            .map(|t| timestamp_s(t) * 1000.0),
        from_disk_cache: response.from_disk_cache,
        from_service_worker: response.from_service_worker,
        from_prefetch_cache: response.from_prefetch_cache,
        from_early_hints: response.from_early_hints,
        timestamp_s: event_timestamp_s,
        resource_type: resource_type.map(str::to_owned),
    }
}

fn headers_value(headers: &Headers) -> Value {
    headers.inner().clone()
}

fn now_unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn enum_str<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn timestamp_s<T: Serialize>(value: &T) -> f64 {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::browser_protocol::network::{MonotonicTime, ResourceType};
    use serde_json::json;

    fn request_event(request_id: &str, url: &str) -> EventRequestWillBeSent {
        serde_json::from_value(json!({
            "requestId": request_id,
            "loaderId": "loader-1",
            "documentURL": "https://example.test/",
            "request": {
                "url": url,
                "method": "GET",
                "headers": {"Accept": "text/html"},
                "initialPriority": "VeryHigh",
                "referrerPolicy": "strict-origin-when-cross-origin"
            },
            "timestamp": 10.0,
            "wallTime": 1710000000.25,
            "initiator": {"type": "other"},
            "redirectHasExtraInfo": false,
            "type": "Document",
            "frameId": "frame-1"
        }))
        .expect("request event")
    }

    fn response_event(request_id: &str, status: i64) -> EventResponseReceived {
        serde_json::from_value(json!({
            "requestId": request_id,
            "loaderId": "loader-1",
            "timestamp": 10.5,
            "type": "Document",
            "response": {
                "url": "https://example.test/",
                "status": status,
                "statusText": "OK",
                "headers": {"content-type": "text/html"},
                "mimeType": "text/html",
                "charset": "",
                "connectionReused": false,
                "connectionId": 1,
                "remoteIPAddress": "127.0.0.1",
                "remotePort": 443,
                "encodedDataLength": 512,
                "protocol": "h2",
                "securityState": "secure"
            },
            "hasExtraInfo": false,
            "frameId": "frame-1"
        }))
        .expect("response event")
    }

    fn finished_event(request_id: &str) -> EventLoadingFinished {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 11.0,
            "encodedDataLength": 1024
        }))
        .expect("loading finished event")
    }

    fn failed_event(request_id: &str) -> EventLoadingFailed {
        EventLoadingFailed {
            request_id: request_id.to_owned().into(),
            timestamp: MonotonicTime::new(12.0),
            r#type: ResourceType::Image,
            error_text: "net::ERR_ABORTED".to_owned(),
            canceled: Some(true),
            blocked_reason: None,
            cors_error_status: None,
        }
    }

    fn websocket_created_event(request_id: &str, url: &str) -> EventWebSocketCreated {
        serde_json::from_value(json!({
            "requestId": request_id,
            "url": url,
            "initiator": {"type": "script"}
        }))
        .expect("websocket created event")
    }

    fn websocket_handshake_request_event(
        request_id: &str,
    ) -> EventWebSocketWillSendHandshakeRequest {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 20.0,
            "wallTime": 30.0,
            "request": {
                "headers": {
                    "Sec-WebSocket-Key": "abc"
                }
            }
        }))
        .expect("websocket handshake request event")
    }

    fn websocket_handshake_response_event(
        request_id: &str,
    ) -> EventWebSocketHandshakeResponseReceived {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 21.0,
            "response": {
                "status": 101,
                "statusText": "Switching Protocols",
                "headers": {
                    "Upgrade": "websocket"
                }
            }
        }))
        .expect("websocket handshake response event")
    }

    fn websocket_frame_sent_event(
        request_id: &str,
        opcode: f64,
        payload_data: &str,
    ) -> EventWebSocketFrameSent {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 22.0,
            "response": {
                "opcode": opcode,
                "mask": true,
                "payloadData": payload_data
            }
        }))
        .expect("websocket frame sent event")
    }

    fn websocket_frame_received_event(
        request_id: &str,
        opcode: f64,
        payload_data: &str,
    ) -> EventWebSocketFrameReceived {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 23.0,
            "response": {
                "opcode": opcode,
                "mask": false,
                "payloadData": payload_data
            }
        }))
        .expect("websocket frame received event")
    }

    fn websocket_closed_event(request_id: &str) -> EventWebSocketClosed {
        serde_json::from_value(json!({
            "requestId": request_id,
            "timestamp": 24.0
        }))
        .expect("websocket closed event")
    }

    fn paused_event(request_id: &str, url: &str, resource_type: &str) -> FetchEventRequestPaused {
        serde_json::from_value(json!({
            "requestId": request_id,
            "request": {
                "url": url,
                "method": "GET",
                "headers": {"Accept": "application/json"},
                "initialPriority": "High",
                "referrerPolicy": "strict-origin-when-cross-origin"
            },
            "frameId": "frame-1",
            "resourceType": resource_type
        }))
        .expect("Fetch.requestPaused event")
    }

    fn fulfill_rule(id: &str, url: &str, match_kind: CdpFetchRouteMatchKind) -> CdpFetchRouteRule {
        CdpFetchRouteRule {
            id: id.to_owned(),
            url: url.to_owned(),
            match_kind,
            method: None,
            resource_type: None,
            action: CdpFetchRouteAction::Fulfill(CdpFetchRouteFulfill {
                status: 201,
                response_phrase: Some("Created".to_owned()),
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body_base64: Some("eyJvayI6dHJ1ZX0=".to_owned()),
            }),
        }
    }

    #[test]
    fn ring_buffer_keeps_bounded_request_records_and_evicts_oldest() {
        let mut buffer = RingBuffer::new(2);
        buffer.apply_request_will_be_sent(&request_event("r1", "https://example.test/one"));
        buffer.apply_request_will_be_sent(&request_event("r2", "https://example.test/two"));
        buffer.apply_request_will_be_sent(&request_event("r3", "https://example.test/three"));

        let request_ids: Vec<&str> = buffer
            .entries
            .iter()
            .map(|entry| entry.request_id.as_str())
            .collect();
        assert_eq!(request_ids, vec!["r2", "r3"]);
        assert_eq!(buffer.dropped, 1);
        assert_eq!(buffer.cursor(), 3);
    }

    #[test]
    fn response_and_finished_events_update_existing_request_cursor() {
        let mut buffer = RingBuffer::new(10);
        buffer.apply_request_will_be_sent(&request_event("r1", "https://example.test/"));
        let after_request_cursor = buffer.cursor();
        buffer.apply_response_received(&response_event("r1", 200));
        buffer.apply_loading_finished(&finished_event("r1"));

        assert_eq!(buffer.entries.len(), 1);
        let entry = &buffer.entries[0];
        assert_eq!(entry.first_seq, 0);
        assert_eq!(entry.seq, 2);
        assert!(entry.seq >= after_request_cursor);
        assert_eq!(entry.method.as_deref(), Some("GET"));
        assert_eq!(entry.url.as_deref(), Some("https://example.test/"));
        assert_eq!(entry.resource_type.as_deref(), Some("Document"));
        assert_eq!(entry.response.as_ref().map(|r| r.status), Some(200));
        assert_eq!(entry.encoded_data_length, Some(1024.0));
        assert!(entry.response_received);
        assert!(entry.loading_finished);
        assert!(!entry.loading_failed);
    }

    #[test]
    fn websocket_buffer_tracks_frames_and_close_info() {
        let mut buffer = WebSocketRingBuffer::new(8);
        buffer.apply_created(&websocket_created_event(
            "ws-1",
            "wss://example.test/socket",
        ));
        buffer.apply_handshake_request(&websocket_handshake_request_event("ws-1"));
        buffer.apply_handshake_response(&websocket_handshake_response_event("ws-1"));
        buffer.apply_frame_sent(&websocket_frame_sent_event("ws-1", 1.0, "hello"));
        let close_payload = BASE64_STANDARD.encode([0x03, 0xe8, b'o', b'k']);
        buffer.apply_frame_received(&websocket_frame_received_event("ws-1", 8.0, &close_payload));
        buffer.apply_closed(&websocket_closed_event("ws-1"));

        assert_eq!(buffer.entries.len(), 1);
        let entry = &buffer.entries[0];
        assert_eq!(entry.request_id, "ws-1");
        assert_eq!(entry.url.as_deref(), Some("wss://example.test/socket"));
        assert!(entry.created);
        assert_eq!(entry.status, Some(101));
        assert_eq!(entry.sent_frame_count, 1);
        assert_eq!(entry.received_frame_count, 1);
        assert!(entry.closed);
        assert_eq!(entry.close_code, Some(1000));
        assert_eq!(entry.close_reason.as_deref(), Some("ok"));
        assert_eq!(entry.frames.len(), 2);
        assert_eq!(entry.frames[0].payload_data.as_deref(), Some("hello"));
        assert_eq!(entry.frames[1].close_code, Some(1000));
        println!(
            "readback=websocket_buffer request_id={} sent={} received={} close_code={:?} close_reason={:?}",
            entry.request_id,
            entry.sent_frame_count,
            entry.received_frame_count,
            entry.close_code,
            entry.close_reason
        );
    }

    #[test]
    fn loading_failed_creates_failure_entry_with_error_text() {
        let mut buffer = RingBuffer::new(10);
        buffer.apply_loading_failed(&failed_event("r-failed"));

        let entry = &buffer.entries[0];
        assert_eq!(entry.request_id, "r-failed");
        assert_eq!(entry.resource_type.as_deref(), Some("Image"));
        assert!(entry.loading_failed);
        assert!(!entry.loading_finished);
        assert_eq!(
            entry.failure_error_text.as_deref(),
            Some("net::ERR_ABORTED")
        );
        assert_eq!(entry.failure_canceled, Some(true));
    }

    #[test]
    fn read_filter_sorts_by_latest_event_cursor_for_delta_reads() {
        let mut buffer = RingBuffer::new(10);
        buffer.apply_request_will_be_sent(&request_event("r1", "https://example.test/one"));
        let since = buffer.cursor();
        buffer.apply_request_will_be_sent(&request_event("r2", "https://example.test/two"));
        buffer.apply_response_received(&response_event("r1", 201));

        let mut entries: Vec<CdpNetworkEntry> = buffer
            .entries
            .iter()
            .filter(|entry| entry.seq >= since)
            .cloned()
            .collect();
        entries.sort_by_key(|entry| entry.seq);

        let ids: Vec<&str> = entries
            .iter()
            .map(|entry| entry.request_id.as_str())
            .collect();
        assert_eq!(ids, vec!["r2", "r1"]);
        assert_eq!(entries[1].response.as_ref().map(|r| r.status), Some(201));
    }

    #[test]
    fn fetch_pattern_conversion_preserves_url_resource_type_and_stage() {
        let converted = fetch_pattern_to_cdp(&CdpFetchInterceptionPattern {
            url_pattern: Some("https://example.test/api/*".to_owned()),
            resource_type: Some("XHR".to_owned()),
            request_stage: CdpFetchInterceptionStage::Response,
        })
        .expect("pattern converts");
        let value = serde_json::to_value(converted).expect("pattern json");
        assert_eq!(value["urlPattern"], "https://example.test/api/*");
        assert_eq!(value["resourceType"], "XHR");
        assert_eq!(value["requestStage"], "Response");
    }

    #[test]
    fn fetch_pattern_conversion_rejects_invalid_values() {
        assert!(
            fetch_pattern_to_cdp(&CdpFetchInterceptionPattern {
                url_pattern: Some(String::new()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            fetch_pattern_to_cdp(&CdpFetchInterceptionPattern {
                resource_type: Some("NotAResource".to_owned()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn fetch_request_id_string_round_trips_generated_id() {
        let request_id = FetchRequestId::from("intercept-1".to_owned());
        assert_eq!(fetch_request_id_string(&request_id), "intercept-1");
    }

    #[test]
    fn network_override_headers_params_serializes_header_map() {
        let params =
            network_override_headers_params(&[("x-synapse-test".to_owned(), "enabled".to_owned())])
                .expect("headers params");
        let value = serde_json::to_value(params).expect("params json");

        assert_eq!(value["headers"]["x-synapse-test"], "enabled");
    }

    #[test]
    fn network_override_user_agent_params_serializes_ua() {
        let params =
            network_override_user_agent_params("SynapseTest/1.0").expect("user-agent params");
        let value = serde_json::to_value(params).expect("params json");

        assert_eq!(value["userAgent"], "SynapseTest/1.0");
    }

    #[test]
    fn network_override_config_rejects_invalid_values() {
        assert!(
            validate_network_override_config(&CdpNetworkOverrideConfig {
                headers: vec![("bad header".to_owned(), "value".to_owned())],
                user_agent: None,
            })
            .is_err()
        );
        assert!(
            validate_network_override_config(&CdpNetworkOverrideConfig {
                headers: Vec::new(),
                user_agent: Some("bad\nua".to_owned()),
            })
            .is_err()
        );
    }

    #[test]
    fn fetch_route_glob_matches_url_with_star_and_question_mark() {
        assert!(glob_matches(
            "https://example.test/api/*/user?.json",
            "https://example.test/api/v1/user1.json"
        ));
        assert!(!glob_matches(
            "https://example.test/api/*/user?.json",
            "https://example.test/assets/user1.json"
        ));
    }

    #[test]
    fn fetch_route_match_supports_regex_and_first_match_order() {
        let event = paused_event("fetch-1", "https://example.test/api/users/42", "XHR");
        let first = fulfill_rule(
            "first",
            r"^https://example\.test/api/users/\d+$",
            CdpFetchRouteMatchKind::Regex,
        );
        let second = fulfill_rule(
            "second",
            "https://example.test/api/*",
            CdpFetchRouteMatchKind::Glob,
        );

        let matched = fetch_route_match(&event, &[first, second]).expect("matched route");
        assert_eq!(matched.id, "first");
    }

    #[test]
    fn fetch_route_match_respects_resource_type() {
        let event = paused_event("fetch-1", "https://example.test/api/users", "XHR");
        let mut document_rule = fulfill_rule(
            "document",
            "https://example.test/api/*",
            CdpFetchRouteMatchKind::Glob,
        );
        document_rule.resource_type = Some("Document".to_owned());
        let mut xhr_rule = fulfill_rule(
            "xhr",
            "https://example.test/api/*",
            CdpFetchRouteMatchKind::Glob,
        );
        xhr_rule.resource_type = Some("XHR".to_owned());

        let matched =
            fetch_route_match(&event, &[document_rule, xhr_rule]).expect("matched xhr route");
        assert_eq!(matched.id, "xhr");
    }

    #[test]
    fn fetch_route_match_respects_method_when_present() {
        let event = paused_event("fetch-1", "https://example.test/api/users", "XHR");
        let mut post_rule = fulfill_rule(
            "post",
            "https://example.test/api/*",
            CdpFetchRouteMatchKind::Glob,
        );
        post_rule.method = Some("POST".to_owned());
        let mut get_rule = fulfill_rule(
            "get",
            "https://example.test/api/*",
            CdpFetchRouteMatchKind::Glob,
        );
        get_rule.method = Some("GET".to_owned());

        let matched = fetch_route_match(&event, &[post_rule, get_rule]).expect("matched get route");
        assert_eq!(matched.id, "get");
    }

    #[test]
    fn fetch_route_validation_rejects_bad_values() {
        let mut rule = fulfill_rule(
            "bad id",
            "https://example.test/*",
            CdpFetchRouteMatchKind::Glob,
        );
        assert!(validate_fetch_route_rule(&rule).is_err());

        rule.id = "ok".to_owned();
        rule.url = "[".to_owned();
        rule.match_kind = CdpFetchRouteMatchKind::Regex;
        assert!(validate_fetch_route_rule(&rule).is_err());

        rule.url = "https://example.test/*".to_owned();
        rule.match_kind = CdpFetchRouteMatchKind::Glob;
        let fulfill = match &mut rule.action {
            CdpFetchRouteAction::Fulfill(fulfill) => fulfill,
            CdpFetchRouteAction::Abort(_) => panic!("expected fulfill rule"),
            CdpFetchRouteAction::Continue(_) => panic!("expected fulfill rule"),
        };
        fulfill.status = 99;
        assert!(validate_fetch_route_rule(&rule).is_err());

        rule.action = CdpFetchRouteAction::Abort(CdpFetchRouteAbort {
            error_reason: "NotAReason".to_owned(),
        });
        assert!(validate_fetch_route_rule(&rule).is_err());

        rule.action = CdpFetchRouteAction::Continue(CdpFetchRouteContinue::default());
        assert!(validate_fetch_route_rule(&rule).is_err());
    }

    #[test]
    fn fetch_fulfill_params_serializes_status_headers_phrase_and_body() {
        let fulfill = match fulfill_rule(
            "route-1",
            "https://example.test/*",
            CdpFetchRouteMatchKind::Glob,
        )
        .action
        {
            CdpFetchRouteAction::Fulfill(fulfill) => fulfill,
            CdpFetchRouteAction::Abort(_) => panic!("expected fulfill rule"),
            CdpFetchRouteAction::Continue(_) => panic!("expected fulfill rule"),
        };
        let params = fetch_fulfill_params(FetchRequestId::from("intercept-1".to_owned()), &fulfill)
            .expect("fulfill params");
        let value = serde_json::to_value(params).expect("params json");

        assert_eq!(value["requestId"], "intercept-1");
        assert_eq!(value["responseCode"], 201);
        assert_eq!(value["responsePhrase"], "Created");
        assert_eq!(value["body"], "eyJvayI6dHJ1ZX0=");
        assert_eq!(value["responseHeaders"][0]["name"], "content-type");
        assert_eq!(value["responseHeaders"][0]["value"], "application/json");
    }

    #[test]
    fn fetch_continue_params_serializes_overrides() {
        let params = fetch_continue_params(
            FetchRequestId::from("intercept-1".to_owned()),
            &CdpFetchRouteContinue {
                url: Some("https://example.test/rewritten".to_owned()),
                method: Some("POST".to_owned()),
                headers: vec![("x-test".to_owned(), "yes".to_owned())],
                post_data_base64: Some("eyJwYXRjaGVkIjp0cnVlfQ==".to_owned()),
            },
        )
        .expect("continue params");
        let value = serde_json::to_value(params).expect("params json");

        assert_eq!(value["requestId"], "intercept-1");
        assert_eq!(value["url"], "https://example.test/rewritten");
        assert_eq!(value["method"], "POST");
        assert_eq!(value["postData"], "eyJwYXRjaGVkIjp0cnVlfQ==");
        assert_eq!(value["headers"][0]["name"], "x-test");
        assert_eq!(value["headers"][0]["value"], "yes");
    }

    #[test]
    fn fetch_fail_params_serializes_error_reason() {
        let params = fetch_fail_params(
            FetchRequestId::from("intercept-1".to_owned()),
            &CdpFetchRouteAbort {
                error_reason: "BlockedByClient".to_owned(),
            },
        )
        .expect("fail params");
        let value = serde_json::to_value(params).expect("params json");

        assert_eq!(value["requestId"], "intercept-1");
        assert_eq!(value["errorReason"], "BlockedByClient");
    }
}
