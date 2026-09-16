use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt::Write as _,
    fs,
    io::{self, Read as _},
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
#[cfg(windows)]
use axum::serve::Listener;
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, HeaderValue, Request, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::{SinkExt, StreamExt, future::join_all, stream::SplitSink};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::SessionError,
    session::{SessionState, SessionStore, SessionStoreError, local::LocalSessionManager},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
#[cfg(test)]
use synapse_action::ActionHandle;
use synapse_core::{AgentEventKind, AgentEventRecord, EventFilter, EventSource, Health};
use synapse_storage::{Db, cf};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpListener,
    sync::broadcast,
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows::Win32::Networking::WinSock::{
    SD_BOTH, SOCKET, WSAGetLastError, shutdown as winsock_shutdown,
};

#[cfg(windows)]
use std::os::windows::io::AsRawSocket;

use crate::{
    emitter_shutdown::{
        M2EmitterOwner, ShutdownTaskOwner, drain_m2_emitter_owner, take_m2_emitter_owner,
        wait_for_m2_emitter_done,
    },
    http::auth::{self, HttpAuth},
    http::session,
    http::sse::{self, SseState},
    m2::M2ServiceConfig,
    m3::M3ServiceConfig,
    m4::M4ServiceConfig,
    server::{
        SynapseService,
        terminal_capture::capture::{
            LiveTerminalSession, TerminalCaptureEvent, TerminalCaptureEventKind,
            TerminalCaptureStatus, terminal_capture_session,
        },
    },
};

type McpHttpService = StreamableHttpService<SynapseService, LocalSessionManager>;
type HttpBackgroundTaskOwner = (&'static str, ShutdownTaskOwner<()>);
const STALE_SESSION_INPUT_CLEANUP_INTERVAL: Duration = Duration::from_millis(250);
const DRAIN_RESPONSE_GRACE_TIMEOUT: Duration = Duration::from_secs(2);
const MCP_SESSION_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const DAEMON_SESSION_INPUT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_BACKGROUND_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_BACKGROUND_TASK_ABORT_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_SERVER_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_SERVER_TASK_ABORT_TIMEOUT: Duration = Duration::from_secs(2);
const DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES: usize = 256 * 1024;
const DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES: usize = 64 * 1024;
const DASHBOARD_CONTEXT_BODY_LIMIT_BYTES: usize = 256 * 1024;
const DASHBOARD_SPAWN_FAN_OUT_MAX: u32 = 5;
const DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS: u64 = 3_000;
const DASHBOARD_ASCIICAST_DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;
const DASHBOARD_ASCIICAST_HARD_MAX_BYTES: u64 = 64 * 1024 * 1024;
static HTTP_ACCEPTED_SOCKETS_TOTAL: AtomicU64 = AtomicU64::new(0);
static HTTP_ACCEPTED_SOCKETS_CURRENT: AtomicU64 = AtomicU64::new(0);
static HTTP_MCP_REQUEST_STARTED_TOTAL: AtomicU64 = AtomicU64::new(0);
static HTTP_MCP_REQUEST_COMPLETED_TOTAL: AtomicU64 = AtomicU64::new(0);
static HTTP_MCP_REQUEST_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static HTTP_MCP_REQUEST_ERROR_STATUS_TOTAL: AtomicU64 = AtomicU64::new(0);
static HTTP_MCP_LAST_EVENT: Mutex<Option<HttpMcpTransportEvent>> = Mutex::new(None);

#[derive(Clone, Debug)]
struct HttpMcpTransportEvent {
    request_id: u64,
    phase: &'static str,
    method: String,
    path: String,
    status_code: Option<u16>,
    elapsed_ms: Option<u128>,
    unix_ms: u64,
}

#[derive(Clone, Debug)]
struct HttpTransportDiagnosticsSnapshot {
    accepted_sockets_total: u64,
    accepted_sockets_current: u64,
    mcp_request_started_total: u64,
    mcp_request_completed_total: u64,
    mcp_request_in_flight: u64,
    mcp_request_error_status_total: u64,
    last_event: Option<HttpMcpTransportEvent>,
}

pub(crate) fn http_transport_diagnostics_detail() -> String {
    http_transport_diagnostics_detail_from_snapshot(http_transport_diagnostics_snapshot())
}

fn http_transport_diagnostics_snapshot() -> HttpTransportDiagnosticsSnapshot {
    HttpTransportDiagnosticsSnapshot {
        accepted_sockets_total: HTTP_ACCEPTED_SOCKETS_TOTAL.load(Ordering::Relaxed),
        accepted_sockets_current: HTTP_ACCEPTED_SOCKETS_CURRENT.load(Ordering::Relaxed),
        mcp_request_started_total: HTTP_MCP_REQUEST_STARTED_TOTAL.load(Ordering::Relaxed),
        mcp_request_completed_total: HTTP_MCP_REQUEST_COMPLETED_TOTAL.load(Ordering::Relaxed),
        mcp_request_in_flight: HTTP_MCP_REQUEST_IN_FLIGHT.load(Ordering::Relaxed),
        mcp_request_error_status_total: HTTP_MCP_REQUEST_ERROR_STATUS_TOTAL.load(Ordering::Relaxed),
        last_event: HTTP_MCP_LAST_EVENT
            .lock()
            .ok()
            .and_then(|event| event.clone()),
    }
}

fn http_transport_diagnostics_detail_from_snapshot(
    snapshot: HttpTransportDiagnosticsSnapshot,
) -> String {
    let last_event = snapshot.last_event.map_or_else(
        || "none".to_owned(),
        |event| {
            format!(
                "request_id:{} phase:{} method:{} path:{} status:{} elapsed_ms:{} unix_ms:{}",
                event.request_id,
                event.phase,
                event.method,
                event.path,
                event
                    .status_code
                    .map_or_else(|| "none".to_owned(), |status| status.to_string()),
                event
                    .elapsed_ms
                    .map_or_else(|| "none".to_owned(), |elapsed| elapsed.to_string()),
                event.unix_ms
            )
        },
    );
    format!(
        "mcp_transport_diagnostics=request_started_total:{} request_completed_total:{} request_in_flight:{} request_error_status_total:{} accepted_sockets_total:{} accepted_sockets_current:{} last_event=\"{}\" classification_hint=\"if client send errors occur while process/socket SoT is listening and request_started_total does not increase, the request did not reach the daemon HTTP middleware; if started increases without completed, inspect request_in_flight and daemon_lifecycle in-flight tools; if completed status is 5xx/4xx, inspect status and daemon-tool-events\"",
        snapshot.mcp_request_started_total,
        snapshot.mcp_request_completed_total,
        snapshot.mcp_request_in_flight,
        snapshot.mcp_request_error_status_total,
        snapshot.accepted_sockets_total,
        snapshot.accepted_sockets_current,
        last_event.replace('"', "'")
    )
}

fn store_http_mcp_last_event(event: HttpMcpTransportEvent) {
    if let Ok(mut last_event) = HTTP_MCP_LAST_EVENT.lock() {
        *last_event = Some(event);
    }
}

fn saturating_decrement(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(1))
    });
}

fn record_http_mcp_request_started(method: &str, path: &str) -> u64 {
    let request_id = HTTP_MCP_REQUEST_STARTED_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    HTTP_MCP_REQUEST_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    store_http_mcp_last_event(HttpMcpTransportEvent {
        request_id,
        phase: "started",
        method: method.to_owned(),
        path: path.to_owned(),
        status_code: None,
        elapsed_ms: None,
        unix_ms: dashboard_unix_time_ms(),
    });
    request_id
}

fn record_http_mcp_request_completed(
    request_id: u64,
    method: &str,
    path: &str,
    status: StatusCode,
    elapsed: Duration,
) {
    HTTP_MCP_REQUEST_COMPLETED_TOTAL.fetch_add(1, Ordering::Relaxed);
    saturating_decrement(&HTTP_MCP_REQUEST_IN_FLIGHT);
    if status.is_client_error() || status.is_server_error() {
        HTTP_MCP_REQUEST_ERROR_STATUS_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
    synapse_telemetry::metrics::counter!(
        synapse_telemetry::metrics::HTTP_REQUESTS_TOTAL,
        "path" => normalized_http_metric_path(path),
        "status" => http_status_metric_label(status)
    )
    .increment(1);
    store_http_mcp_last_event(HttpMcpTransportEvent {
        request_id,
        phase: "completed",
        method: method.to_owned(),
        path: path.to_owned(),
        status_code: Some(status.as_u16()),
        elapsed_ms: Some(elapsed.as_millis()),
        unix_ms: dashboard_unix_time_ms(),
    });
}

fn emit_http_active_sessions(count: usize) {
    synapse_telemetry::metrics::gauge!(synapse_telemetry::metrics::HTTP_ACTIVE_SESSIONS)
        .set(usize_metric_value(count));
}

fn usize_metric_value(value: usize) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

fn normalized_http_metric_path(path: &str) -> String {
    match path {
        "/mcp" | "/health" | "/agent-events" | "/events" => path.to_owned(),
        "" => "/".to_owned(),
        _ => "__other__".to_owned(),
    }
}

fn http_status_metric_label(status: StatusCode) -> &'static str {
    if status.is_success() {
        "2xx"
    } else if status.is_redirection() {
        "3xx"
    } else if status.is_client_error() {
        "4xx"
    } else if status.is_server_error() {
        "5xx"
    } else {
        "other"
    }
}

#[derive(Clone)]
struct HttpState {
    bind_addr: SocketAddr,
    health_service: Arc<SynapseService>,
    session_manager: Arc<LocalSessionManager>,
    shutdown_cancel: CancellationToken,
    drain_state: crate::server::drain::DaemonDrainState,
    active_http_sockets: ActiveHttpSockets,
    sse_state: SseState,
    /// Journal handle for the push-telemetry ingress (#899); the same DB the
    /// MCP session store writes through.
    agent_events_db: Arc<Db>,
}

struct HttpRouterRuntime {
    app: Router,
    session_manager: Arc<LocalSessionManager>,
    session_lifecycle: crate::server::session_lifecycle::SessionLifecycleState,
    drain_state: crate::server::drain::DaemonDrainState,
    background_tasks: Vec<HttpBackgroundTaskOwner>,
}

#[derive(Debug, Serialize)]
struct DaemonShutdownInputCleanupReport {
    reason: &'static str,
    authority_finalizer_drain_readback: Option<crate::server::AuthorityFinalizerDrainReadback>,
    authority_finalizer_drain_error: Option<String>,
    active_sessions_before: usize,
    live_spawn_snapshot_read_before: bool,
    live_spawn_snapshot_error: Option<String>,
    live_spawn_sessions_before: usize,
    close_candidate_sessions_before: usize,
    input_owner_snapshot_read_before: bool,
    input_owner_session_ids_before: Vec<String>,
    shutdown_sessions_before: usize,
    shutdown_session_ids: Vec<String>,
    cleaned_sessions: usize,
    session_cleanup_timeout_ms: u64,
    session_cleanup_timeouts: Vec<String>,
    orphan_lease_owner_cleanup:
        Option<crate::server::session_lifecycle::SessionShutdownInputCleanupReport>,
    final_lease_held: bool,
    final_lease_owner_session_id: Option<String>,
    final_lease_is_operator: bool,
    lease_still_held_after_cleanup: bool,
    input_owner_snapshot_read_after: bool,
    input_owner_session_ids_after: Vec<String>,
    input_owner_snapshot_errors: Vec<String>,
    failure_count: usize,
    session_reports: Vec<crate::server::session_lifecycle::SessionShutdownInputCleanupReport>,
}

impl DaemonShutdownInputCleanupReport {
    fn authority_finalizer_owners_quiescent(&self) -> bool {
        self.authority_finalizer_drain_readback
            .as_ref()
            .is_some_and(crate::server::AuthorityFinalizerDrainReadback::safe_to_unlock)
    }

    fn all_input_owners_quiescent(&self) -> bool {
        self.authority_finalizer_owners_quiescent()
            && self.failure_count == 0
            && self.live_spawn_snapshot_read_before
            && self.live_spawn_snapshot_error.is_none()
            && self.input_owner_snapshot_read_before
            && self.input_owner_snapshot_errors.is_empty()
            && !self.lease_still_held_after_cleanup
            && (!self.final_lease_held || self.final_lease_is_operator)
            && self.input_owner_snapshot_read_after
            && self.input_owner_session_ids_after.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
struct M3StorageOwnerReadback {
    strong_owner_count: usize,
    expected_owner_count: usize,
    owners_quiescent: bool,
}

fn m3_storage_owner_readback<T>(owner: &Arc<T>) -> M3StorageOwnerReadback {
    let strong_owner_count = Arc::strong_count(owner);
    let expected_owner_count = 1;
    M3StorageOwnerReadback {
        strong_owner_count,
        expected_owner_count,
        owners_quiescent: strong_owner_count == expected_owner_count,
    }
}

#[derive(Debug, Serialize)]
struct McpSessionShutdownCloseReport {
    reason: &'static str,
    sessions_before: usize,
    close_attempted: usize,
    close_succeeded: usize,
    already_terminated: usize,
    failure_count: usize,
    session_ids: Vec<String>,
    failures: Vec<String>,
    sessions_after: usize,
    session_ids_after: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ActiveHttpSocketShutdownReport {
    reason: &'static str,
    tracked_before: usize,
    shutdown_attempted: usize,
    shutdown_succeeded: usize,
    failure_count: usize,
    tracked_after_shutdown_attempt: usize,
    sockets: Vec<ActiveHttpSocketShutdownRow>,
    failures: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ActiveHttpSocketShutdownOnDropReport {
    reason: &'static str,
    enabled_now: bool,
    was_enabled: bool,
    tracked_now: usize,
}

#[derive(Debug, Serialize)]
struct ActiveHttpSocketShutdownRow {
    raw_socket: usize,
    peer_addr: String,
    accepted_at_unix_ms: u128,
}

#[derive(Debug, Serialize)]
struct HttpShutdownFailure {
    phase: &'static str,
    detail: String,
}

#[derive(Debug, Default, Serialize)]
struct HttpShutdownFailures {
    failures: Vec<HttpShutdownFailure>,
}

#[derive(Debug, Serialize)]
struct HttpBackgroundTaskDrainReport {
    tasks_before: usize,
    graceful_joined: usize,
    abort_requests_sent: usize,
    joined_after_abort: usize,
    still_live_task_names: Vec<&'static str>,
    failures: Vec<String>,
}

#[derive(Debug)]
struct HttpActivityOwnerDrainReport {
    recorder_expected: bool,
    recorder_present: bool,
    recorder_owners_quiescent: bool,
    a11y_expected: bool,
    a11y_present: bool,
    a11y_owners_quiescent: bool,
    retained_a11y_bridge_task_owners: usize,
    win_event_shutdown_history: crate::WinEventShutdownHistoryReadback,
    recorder_retained_owner_readback:
        crate::m3::activity_recorder::ActivityRecorderRetainedOwnerReadback,
    retained_interaction_hook_owners: usize,
    failures: Vec<String>,
}

impl HttpActivityOwnerDrainReport {
    fn safe_to_unlock(&self) -> bool {
        (!self.recorder_expected || self.recorder_present)
            && self.recorder_owners_quiescent
            && (!self.a11y_expected || self.a11y_present)
            && self.a11y_owners_quiescent
            && self.retained_a11y_bridge_task_owners == 0
            && self.win_event_shutdown_history.owners_quiescent()
            && self.recorder_retained_owner_readback.safe_to_unlock()
            && self.retained_interaction_hook_owners == 0
    }

    fn verdict(&self) -> anyhow::Result<()> {
        if self.failures.is_empty() && self.safe_to_unlock() {
            Ok(())
        } else {
            anyhow::bail!(
                "activity owner shutdown failed: {}; readback={self:?}",
                self.failures.join("; ")
            )
        }
    }
}

async fn drain_http_activity_owners(
    m3_state: &crate::m3::SharedM3State,
    recorder_expected: bool,
    a11y_expected: bool,
) -> HttpActivityOwnerDrainReport {
    let mut failures = Vec::new();
    let (recorder, a11y_bridge) = match m3_state.lock() {
        Ok(mut state) => (
            state.activity_recorder.take(),
            state.a11y_event_bridge.take(),
        ),
        Err(error) => {
            failures.push(format!(
                "m3 state lock poisoned while taking recorder/a11y owners: {error}"
            ));
            (None, None)
        }
    };
    let recorder_present = recorder.is_some();
    let a11y_present = a11y_bridge.is_some();
    if recorder_expected && !recorder_present {
        failures.push("eagerly configured activity recorder was missing at shutdown".to_owned());
    }
    if a11y_expected && !a11y_present {
        failures.push("eagerly configured a11y bridge was missing at shutdown".to_owned());
    }

    // Stop the event source first so no new record can race the recorder's
    // session_end boundary. The bridge report includes its exact Tokio owner;
    // dropping the subscription joins the OS hook thread before that await.
    let a11y_owners_quiescent = if let Some(bridge) = a11y_bridge {
        let report = bridge.shutdown().await;
        let quiescent = report.owners_quiescent();
        if let Err(error) = report.verdict() {
            failures.push(format!("a11y bridge: {error:#}"));
        }
        quiescent
    } else {
        !a11y_expected
    };

    let recorder_owners_quiescent = if let Some(recorder) = recorder {
        let report = recorder.shutdown().await;
        let quiescent = report.owners_quiescent();
        if let Err(error) = report.verdict() {
            failures.push(format!("activity recorder: {error:#}"));
        }
        quiescent
    } else {
        !recorder_expected
    };
    let retained_a11y_bridge_task_owners = crate::m3::a11y_events::retained_live_owner_count();
    if retained_a11y_bridge_task_owners != 0 {
        failures.push(format!(
            "{retained_a11y_bridge_task_owners} a11y bridge task owner(s) remain retained after bounded cleanup"
        ));
    }
    let win_event_shutdown_history = crate::win_event_shutdown_history_readback();
    if let Err(error) = win_event_shutdown_history.verdict() {
        failures.push(format!("WinEvent shutdown history: {error:#}"));
    }
    let recorder_retained_owner_readback = crate::m3::activity_recorder::retained_owner_readback();
    if !recorder_retained_owner_readback.safe_to_unlock() {
        failures.push(format!(
            "recorder retained-owner state is not safe to unlock after bounded cleanup: {recorder_retained_owner_readback:?}"
        ));
    }
    let retained_interaction_hook_owners =
        crate::m3::interaction_cadence::retained_live_owner_count();
    if retained_interaction_hook_owners != 0 {
        failures.push(format!(
            "{retained_interaction_hook_owners} interaction-hook thread owner(s) remain live after bounded cleanup"
        ));
    }

    HttpActivityOwnerDrainReport {
        recorder_expected,
        recorder_present,
        recorder_owners_quiescent,
        a11y_expected,
        a11y_present,
        a11y_owners_quiescent,
        retained_a11y_bridge_task_owners,
        win_event_shutdown_history,
        recorder_retained_owner_readback,
        retained_interaction_hook_owners,
        failures,
    }
}

impl HttpBackgroundTaskDrainReport {
    fn owners_quiescent(&self) -> bool {
        self.still_live_task_names.is_empty()
    }

    fn verdict(&self) -> anyhow::Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "HTTP background-task shutdown failed: {}; readback={self:?}",
                self.failures.join("; ")
            )
        }
    }
}

impl HttpShutdownFailures {
    fn push(&mut self, phase: &'static str, detail: impl Into<String>) {
        self.failures.push(HttpShutdownFailure {
            phase,
            detail: detail.into(),
        });
    }

    fn inspect_session_close(&mut self, report: &McpSessionShutdownCloseReport) {
        if report.failure_count != 0 || report.sessions_after != 0 {
            self.push("session_close", format!("report={report:?}"));
        }
    }

    fn inspect_socket_shutdown(&mut self, report: &ActiveHttpSocketShutdownReport) {
        // A successful `shutdown(SD_BOTH)` wakes connection tasks but does not
        // itself drop their tracked stream wrappers. Registry emptiness is a
        // distinct postcondition read after the HTTP server has stopped.
        if report.failure_count != 0 {
            self.push("socket_shutdown", format!("report={report:?}"));
        }
    }

    fn inspect_final_socket_count(&mut self, result: Result<usize, String>) {
        match result {
            Ok(0) => {}
            Ok(tracked_count) => self.push(
                "socket_registry_readback",
                format!("tracked_count={tracked_count}"),
            ),
            Err(error) => self.push("socket_registry_readback", error),
        }
    }

    fn inspect_final_session_ids(&mut self, session_ids: &BTreeSet<String>) {
        if !session_ids.is_empty() {
            self.push(
                "session_manager_readback",
                format!(
                    "session_count={} session_ids={session_ids:?}",
                    session_ids.len()
                ),
            );
        }
    }

    fn inspect_input_cleanup(&mut self, report: &DaemonShutdownInputCleanupReport) {
        if report.failure_count != 0 || report.lease_still_held_after_cleanup {
            self.push("input_cleanup", format!("report={report:?}"));
        }
    }

    fn inspect_result<E>(&mut self, phase: &'static str, result: Result<(), E>)
    where
        E: std::fmt::Display,
    {
        if let Err(error) = result {
            self.push(phase, error.to_string());
        }
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    fn into_result(self) -> anyhow::Result<()> {
        use std::io::Write as _;

        if self.is_empty() {
            return Ok(());
        }
        tracing::error!(
            code = "MCP_HTTP_SHUTDOWN_POSTCONDITION_FAILED",
            failure_count = self.failures.len(),
            failures = ?self.failures,
            "HTTP shutdown completed every cleanup phase but one or more required postconditions failed"
        );
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = writeln!(
            stderr,
            "synapse-mcp shutdown error: code=MCP_HTTP_SHUTDOWN_POSTCONDITION_FAILED failure_count={} failures={:?}",
            self.failures.len(),
            self.failures
        );
        anyhow::bail!(
            "HTTP shutdown postconditions failed ({}): {:?}",
            self.failures.len(),
            self.failures
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct HttpLifetimeOwnerReadback {
    authority_finalizers_quiescent: bool,
    session_input_owners_quiescent: bool,
    session_manager_quiescent: bool,
    active_socket_owners_quiescent: bool,
    server_dispatch_quiescent: bool,
    background_tasks_quiescent: bool,
    m2_emitter_safe: bool,
    activity_owners_quiescent: bool,
    win_event_shutdown_history_quiescent: bool,
    storage_service_owners_quiescent: bool,
    operator_hotkey_quiescent: bool,
    operator_panic_k2_tasks_quiescent: bool,
}

#[derive(Debug)]
struct HttpOperatorOwnerDrain {
    hotkey_report: Option<synapse_action::OperatorHotkeyShutdownReport>,
    install_unwind_retained_live_owner: bool,
    k2_report: crate::safety::OperatorPanicK2TaskDrainReport,
}

impl HttpOperatorOwnerDrain {
    fn hotkey_owners_quiescent(&self) -> bool {
        !self.install_unwind_retained_live_owner
            && self
                .hotkey_report
                .as_ref()
                .is_none_or(synapse_action::OperatorHotkeyShutdownReport::owners_quiescent)
    }

    fn k2_tasks_quiescent(&self) -> bool {
        self.k2_report.owners_quiescent()
    }

    fn inspect_failures(&self, failures: &mut HttpShutdownFailures) {
        if self.install_unwind_retained_live_owner {
            failures.push(
                "operator_hotkey_install_unwind",
                "operator-hotkey installation unwind retained a live exact owner",
            );
        }
        if let Some(report) = self.hotkey_report.as_ref() {
            failures.inspect_result(
                "operator_hotkey_shutdown",
                report.verdict().map_err(|error| anyhow::anyhow!("{error}")),
            );
        }
        failures.inspect_result("operator_panic_k2_drain", self.k2_report.verdict());
    }
}

async fn drain_http_operator_owners(
    guard: &mut Option<synapse_action::OperatorHotkeyGuard>,
    reason: &'static str,
) -> HttpOperatorOwnerDrain {
    let k2_owners_before = crate::safety::operator_panic_k2_task_owner_readback();
    let hotkey_report = crate::safety::shutdown_operator_hotkey(guard, reason);
    let install_unwind_retained_live_owner =
        crate::safety::operator_hotkey_install_unwind_retained_live_owner();
    let hotkey_owners_quiescent = !install_unwind_retained_live_owner
        && hotkey_report
            .as_ref()
            .is_none_or(synapse_action::OperatorHotkeyShutdownReport::owners_quiescent);
    let k2_report =
        crate::safety::drain_operator_panic_k2_tasks(reason, hotkey_owners_quiescent).await;
    let report = HttpOperatorOwnerDrain {
        hotkey_report,
        install_unwind_retained_live_owner,
        k2_report,
    };
    tracing::info!(
        code = "MCP_HTTP_OPERATOR_OWNER_DRAIN_READBACK",
        reason,
        k2_owners_before = ?k2_owners_before,
        report = ?report,
        "readback=operator_hotkey_and_k2_owners edge=http_shutdown after_checked_drain"
    );
    report
}

impl HttpLifetimeOwnerReadback {
    const fn safe_to_unlock(self) -> bool {
        self.authority_finalizers_quiescent
            && self.session_input_owners_quiescent
            && self.session_manager_quiescent
            && self.active_socket_owners_quiescent
            && self.server_dispatch_quiescent
            && self.background_tasks_quiescent
            && self.m2_emitter_safe
            && self.activity_owners_quiescent
            && self.win_event_shutdown_history_quiescent
            && self.storage_service_owners_quiescent
            && self.operator_hotkey_quiescent
            && self.operator_panic_k2_tasks_quiescent
    }
}

const fn http_lifetime_locks_safe_to_close(
    owners: HttpLifetimeOwnerReadback,
    desktop_worker_active_owner_count: usize,
    retained_shutdown_task_owners_quiescent: bool,
    unresolved_shell_child_owners_quiescent: bool,
    activity_recorder_retained_owners_quiescent: bool,
) -> bool {
    owners.safe_to_unlock()
        && desktop_worker_active_owner_count == 0
        && retained_shutdown_task_owners_quiescent
        && unresolved_shell_child_owners_quiescent
        && activity_recorder_retained_owners_quiescent
}

fn close_or_retain_http_lifetime_locks(
    shell_job_store: crate::single_instance::ShellJobStoreLockGuard,
    single_instance: crate::single_instance::SingleInstanceGuard,
    owners: HttpLifetimeOwnerReadback,
    reason: &'static str,
) -> anyhow::Result<()> {
    // Re-read the process-global retained-owner ledger at the destructive
    // boundary. A worker may have outlived an earlier transport instance even
    // though it is not represented by this HTTP runtime's local task vectors.
    let desktop_worker_owner_report = crate::desktop_worker::desktop_worker_retained_owner_report();
    let desktop_worker_owners_quiescent = desktop_worker_owner_report.active_owner_count == 0;
    let retained_shutdown_task_owner_report =
        crate::emitter_shutdown::retained_shutdown_task_owner_report();
    let retained_shutdown_task_owners_quiescent =
        retained_shutdown_task_owner_report.safe_to_unlock();
    let unresolved_shell_child_owner_report = crate::m4::unresolved_shell_child_owner_report();
    let unresolved_shell_child_owners_quiescent =
        unresolved_shell_child_owner_report.safe_to_unlock();
    let activity_recorder_retained_owner_readback =
        crate::m3::activity_recorder::retained_owner_readback();
    let activity_recorder_retained_owners_quiescent =
        activity_recorder_retained_owner_readback.safe_to_unlock();
    if !http_lifetime_locks_safe_to_close(
        owners,
        desktop_worker_owner_report.active_owner_count,
        retained_shutdown_task_owners_quiescent,
        unresolved_shell_child_owners_quiescent,
        activity_recorder_retained_owners_quiescent,
    ) {
        tracing::error!(
            code = "MCP_HTTP_LIFETIME_LOCKS_RETAINED",
            reason,
            owners = ?owners,
            desktop_worker_owners_quiescent,
            desktop_worker_owner_report = ?desktop_worker_owner_report,
            retained_shutdown_task_owners_quiescent,
            retained_shutdown_task_owner_report = ?retained_shutdown_task_owner_report,
            unresolved_shell_child_owners_quiescent,
            unresolved_shell_child_owner_report = ?unresolved_shell_child_owner_report,
            activity_recorder_retained_owners_quiescent,
            activity_recorder_retained_owner_readback = ?activity_recorder_retained_owner_readback,
            "one or more daemon-owned task sets remained live; retaining both lifetime locks until process teardown"
        );
        use std::io::Write as _;
        if let Err(stderr_error) = writeln!(
            std::io::stderr().lock(),
            "synapse-mcp fatal shutdown error: reason={reason} owners={owners:?} desktop_worker_owners_quiescent={desktop_worker_owners_quiescent} desktop_worker_owner_report={desktop_worker_owner_report:?} retained_shutdown_task_owners_quiescent={retained_shutdown_task_owners_quiescent} retained_shutdown_task_owner_report={retained_shutdown_task_owner_report:?} unresolved_shell_child_owners_quiescent={unresolved_shell_child_owners_quiescent} unresolved_shell_child_owner_report={unresolved_shell_child_owner_report:?} activity_recorder_retained_owners_quiescent={activity_recorder_retained_owners_quiescent} activity_recorder_retained_owner_readback={activity_recorder_retained_owner_readback:?}; daemon lifetime locks retained until process teardown"
        ) {
            tracing::error!(
                code = "MCP_HTTP_LIFETIME_LOCK_RETAIN_STDERR_WRITE_FAILED",
                reason,
                error = %stderr_error,
                "failed to write retained HTTP lifetime-lock failure to stderr"
            );
        }
        // Deliberately retain the exact file handles. Dropping either guard
        // here would admit a successor while an old task can still mutate the
        // DB, shell-job store, input state, or audit ledger. The OS releases
        // both locks only when this process is torn down.
        std::mem::forget(shell_job_store);
        std::mem::forget(single_instance);
        anyhow::bail!(
            "refused to release HTTP daemon lifetime locks after {reason}: owners={owners:?} desktop_worker_owners_quiescent={desktop_worker_owners_quiescent} desktop_worker_owner_report={desktop_worker_owner_report:?} retained_shutdown_task_owners_quiescent={retained_shutdown_task_owners_quiescent} retained_shutdown_task_owner_report={retained_shutdown_task_owner_report:?} unresolved_shell_child_owners_quiescent={unresolved_shell_child_owners_quiescent} unresolved_shell_child_owner_report={unresolved_shell_child_owner_report:?} activity_recorder_retained_owners_quiescent={activity_recorder_retained_owners_quiescent} activity_recorder_retained_owner_readback={activity_recorder_retained_owner_readback:?}"
        );
    }
    crate::single_instance::close_daemon_lifetime_locks(shell_job_store, single_instance)
        .map(|_readback| ())
        .map_err(anyhow::Error::new)
        .context("close daemon lifetime locks after HTTP transport cleanup")
}

async fn drain_http_background_tasks(
    tasks: Vec<HttpBackgroundTaskOwner>,
) -> HttpBackgroundTaskDrainReport {
    let tasks_before = tasks.len();
    // Supervise every independent task concurrently. One non-cooperative task
    // gets its own graceful and post-abort deadlines without serially adding
    // those deadlines to every later task's shutdown latency.
    let outcomes = join_all(tasks.into_iter().map(|(name, mut task)| async move {
        match time::timeout(HTTP_BACKGROUND_TASK_STOP_TIMEOUT, &mut task).await {
            Ok(result) => {
                let outcome = match result {
                    Ok(()) => (name, true, false, true, None),
                    Err(error) => (
                        name,
                        true,
                        false,
                        true,
                        Some(format!("{name}: join failed: {error}")),
                    ),
                };
                task.acknowledge_terminal_outcome();
                outcome
            }
            Err(_elapsed) => {
                task.abort();
                match time::timeout(HTTP_BACKGROUND_TASK_ABORT_TIMEOUT, &mut task).await {
                    Ok(result) => {
                        let outcome = (
                            name,
                            false,
                            true,
                            true,
                            Some(format!(
                                "{name}: did not stop within {} ms after shutdown cancellation; abort_join={result:?}",
                                HTTP_BACKGROUND_TASK_STOP_TIMEOUT.as_millis()
                            )),
                        );
                        task.acknowledge_terminal_outcome();
                        outcome
                    }
                    Err(_elapsed) => (
                        name,
                        false,
                        true,
                        false,
                        Some(format!(
                            "{name}: did not stop within {} ms after shutdown cancellation and did not join within {} ms after abort; exact JoinHandle retained until process teardown",
                            HTTP_BACKGROUND_TASK_STOP_TIMEOUT.as_millis(),
                            HTTP_BACKGROUND_TASK_ABORT_TIMEOUT.as_millis()
                        )),
                    ),
                }
            }
        }
    }))
    .await;
    let mut graceful_joined = 0;
    let mut abort_requests_sent = 0;
    let mut joined_after_abort = 0;
    let mut still_live_task_names = Vec::new();
    let mut failures = Vec::new();
    for (name, joined_during_grace, abort_requested, terminal_readback, failure) in outcomes {
        graceful_joined += usize::from(joined_during_grace);
        abort_requests_sent += usize::from(abort_requested);
        joined_after_abort += usize::from(abort_requested && terminal_readback);
        if !terminal_readback {
            still_live_task_names.push(name);
        }
        if let Some(failure) = failure {
            failures.push(failure);
        }
    }
    HttpBackgroundTaskDrainReport {
        tasks_before,
        graceful_joined,
        abort_requests_sent,
        joined_after_abort,
        still_live_task_names,
        failures,
    }
}

struct HttpRuntimeStartup {
    background_tasks: Vec<HttpBackgroundTaskOwner>,
    operator_hotkey_guard: Option<synapse_action::OperatorHotkeyGuard>,
    runtime: HttpRouterRuntime,
}

struct HttpRuntimeStartupFailure {
    phase: &'static str,
    error: anyhow::Error,
    background_tasks: Vec<HttpBackgroundTaskOwner>,
    operator_hotkey_guard: Option<synapse_action::OperatorHotkeyGuard>,
}

impl HttpRuntimeStartupFailure {
    fn new(
        phase: &'static str,
        error: anyhow::Error,
        background_tasks: Vec<HttpBackgroundTaskOwner>,
        operator_hotkey_guard: Option<synapse_action::OperatorHotkeyGuard>,
    ) -> Self {
        Self {
            phase,
            error,
            background_tasks,
            operator_hotkey_guard,
        }
    }
}

fn own_http_background_task(name: &'static str, task: JoinHandle<()>) -> HttpBackgroundTaskOwner {
    (name, ShutdownTaskOwner::new(name, task))
}

fn start_http_runtime(
    service: &SynapseService,
    shutdown_cancel: &CancellationToken,
    local_addr: SocketAddr,
    sse_state: SseState,
    active_http_sockets: ActiveHttpSockets,
) -> Result<HttpRuntimeStartup, HttpRuntimeStartupFailure> {
    let mut background_tasks = Vec::new();
    let routine_miner = match crate::m3::routine_miner_job::spawn_periodic_miner(
        service.m3_state_handle(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic routine miner")
    {
        Ok(task) => task,
        Err(error) => {
            return Err(HttpRuntimeStartupFailure::new(
                "routine_miner",
                error,
                background_tasks,
                None,
            ));
        }
    };
    if let Some(task) = routine_miner {
        background_tasks.push(own_http_background_task("routine_miner", task));
    }

    let intent_detector = match crate::m3::intent_events::spawn_intent_detector(
        service.m3_state_handle(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic intent detector")
    {
        Ok(task) => task,
        Err(error) => {
            return Err(HttpRuntimeStartupFailure::new(
                "intent_detector",
                error,
                background_tasks,
                None,
            ));
        }
    };
    if let Some(task) = intent_detector {
        background_tasks.push(own_http_background_task("intent_detector", task));
    }

    let armed_routine_runner =
        match crate::server::suggestions::spawn_periodic_armed_routine_runner(
            service.clone(),
            shutdown_cancel.clone(),
        )
        .context("spawn periodic armed routine runner")
        {
            Ok(task) => task,
            Err(error) => {
                return Err(HttpRuntimeStartupFailure::new(
                    "armed_routine_runner",
                    error,
                    background_tasks,
                    None,
                ));
            }
        };
    if let Some(task) = armed_routine_runner {
        background_tasks.push(own_http_background_task("armed_routine_runner", task));
    }

    let transcript_ingest =
        match crate::server::agent_transcripts::spawn_periodic_transcript_ingest(
            service.m3_state_handle(),
            shutdown_cancel.clone(),
        )
        .context("spawn periodic transcript ingester")
        {
            Ok(task) => task,
            Err(error) => {
                return Err(HttpRuntimeStartupFailure::new(
                    "transcript_ingest",
                    error,
                    background_tasks,
                    None,
                ));
            }
        };
    if let Some(task) = transcript_ingest {
        background_tasks.push(own_http_background_task("transcript_ingest", task));
    }

    let ambient_ingest = match crate::server::ambient_agents::spawn_periodic_ambient_ingest(
        service.m3_state_handle(),
        shutdown_cancel.clone(),
    )
    .context("spawn periodic ambient agent discovery")
    {
        Ok(task) => task,
        Err(error) => {
            return Err(HttpRuntimeStartupFailure::new(
                "ambient_ingest",
                error,
                background_tasks,
                None,
            ));
        }
    };
    if let Some(task) = ambient_ingest {
        background_tasks.push(own_http_background_task("ambient_ingest", task));
    }

    let operator_hotkey_guard = match crate::safety::install_operator_hotkey(service.clone())
        .context("install operator panic hotkey")
    {
        Ok(guard) => guard,
        Err(error) => {
            return Err(HttpRuntimeStartupFailure::new(
                "operator_hotkey",
                error,
                background_tasks,
                None,
            ));
        }
    };
    let runtime = match router(
        shutdown_cancel,
        local_addr,
        sse_state,
        service.clone(),
        active_http_sockets,
    )
    .context("build HTTP MCP router")
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Err(HttpRuntimeStartupFailure::new(
                "http_router",
                error,
                background_tasks,
                operator_hotkey_guard,
            ));
        }
    };
    Ok(HttpRuntimeStartup {
        background_tasks,
        operator_hotkey_guard,
        runtime,
    })
}

async fn fail_http_startup_after_service(
    failure: HttpRuntimeStartupFailure,
    service: SynapseService,
    m3_state: crate::m3::SharedM3State,
    m2_emitter_owner: M2EmitterOwner,
    recorder_expected: bool,
    a11y_expected: bool,
    shutdown_cancel: CancellationToken,
    connection_closed_cancel: CancellationToken,
    shell_job_store_lock_guard: crate::single_instance::ShellJobStoreLockGuard,
    single_instance_guard: crate::single_instance::SingleInstanceGuard,
) -> anyhow::Result<ExitCode> {
    let HttpRuntimeStartupFailure {
        phase,
        error,
        background_tasks,
        mut operator_hotkey_guard,
    } = failure;
    let primary_detail = format!("{error:#}");
    tracing::error!(
        code = "MCP_HTTP_STARTUP_TRANSACTION_FAILED",
        phase,
        detail = %primary_detail,
        "HTTP startup failed after daemon task activation; cancelling and reading every exact owner before lifetime-lock disposition"
    );
    shutdown_cancel.cancel();
    connection_closed_cancel.cancel();

    let mut failures = HttpShutdownFailures::default();
    failures.push(phase, primary_detail.clone());

    let operator_panic_k2_owners_before = crate::safety::operator_panic_k2_task_owner_readback();
    let hotkey_report =
        crate::safety::shutdown_operator_hotkey(&mut operator_hotkey_guard, "http_startup_failure");
    let install_unwind_retained_live_owner =
        crate::safety::operator_hotkey_install_unwind_retained_live_owner();
    let operator_hotkey_quiescent = !install_unwind_retained_live_owner
        && hotkey_report
            .as_ref()
            .is_none_or(synapse_action::OperatorHotkeyShutdownReport::owners_quiescent);
    if install_unwind_retained_live_owner {
        failures.push(
            "operator_hotkey_install_unwind",
            "operator-hotkey installation unwind retained a live exact owner",
        );
    }
    if let Some(report) = hotkey_report.as_ref() {
        failures.inspect_result(
            "operator_hotkey_shutdown",
            report.verdict().map_err(anyhow::Error::msg),
        );
    }
    let operator_panic_k2_drain = crate::safety::drain_operator_panic_k2_tasks(
        "http_startup_failure",
        operator_hotkey_quiescent,
    )
    .await;
    let operator_panic_k2_tasks_quiescent = operator_panic_k2_drain.owners_quiescent();
    failures.inspect_result("operator_panic_k2_drain", operator_panic_k2_drain.verdict());

    let authority_result = service.drain_authority_finalizers().await;
    let authority_finalizers_quiescent = match &authority_result {
        Ok(readback) => readback.safe_to_unlock(),
        Err(error) => error.readback.safe_to_unlock(),
    };
    failures.inspect_result(
        "authority_finalizer_drain",
        authority_result
            .map(|_readback| ())
            .map_err(anyhow::Error::new),
    );

    let activity_drain =
        drain_http_activity_owners(&m3_state, recorder_expected, a11y_expected).await;
    let activity_owners_quiescent = activity_drain.safe_to_unlock();
    let win_event_shutdown_history_quiescent =
        activity_drain.win_event_shutdown_history.owners_quiescent();
    failures.inspect_result("activity_owner_drain", activity_drain.verdict());

    let background_task_drain = drain_http_background_tasks(background_tasks).await;
    let background_tasks_quiescent = background_task_drain.owners_quiescent();
    failures.inspect_result("background_task_drain", background_task_drain.verdict());

    let m2_emitter_drain =
        drain_m2_emitter_owner(Some(m2_emitter_owner), "http", "http_startup_failure").await;
    let m2_emitter_safe = m2_emitter_drain.safe_to_unlock();
    failures.inspect_result("m2_emitter_drain", m2_emitter_drain.verdict());

    let (session_registry_readback_ok, session_registry_rows) =
        match service.session_registry_handle().lock() {
            Ok(registry) => (
                true,
                registry.reads(crate::server::session_registry::unix_time_ms_now()),
            ),
            Err(error) => {
                failures.push(
                    "session_registry_readback",
                    format!("session registry lock poisoned during startup cleanup: {error}"),
                );
                (false, Vec::new())
            }
        };
    if !session_registry_rows.is_empty() {
        failures.push(
            "session_registry_readback",
            format!(
                "startup failure left {} session registry rows: {session_registry_rows:?}",
                session_registry_rows.len()
            ),
        );
    }
    let lease_after = synapse_action::lease::status();
    if lease_after.held {
        failures.push(
            "input_lease_readback",
            format!("input lease remained held after startup cleanup: {lease_after:?}"),
        );
    }
    let session_manager_quiescent =
        session_registry_readback_ok && session_registry_rows.is_empty();
    let session_input_owners_quiescent = authority_finalizers_quiescent && !lease_after.held;

    drop(service);
    drop(shutdown_cancel);
    drop(connection_closed_cancel);
    let storage_owner_readback = m3_storage_owner_readback(&m3_state);
    if !storage_owner_readback.owners_quiescent {
        failures.push(
            "storage_service_owner_readback",
            format!("readback={storage_owner_readback:?}"),
        );
    }
    drop(m3_state);
    let owners = HttpLifetimeOwnerReadback {
        authority_finalizers_quiescent,
        session_input_owners_quiescent,
        session_manager_quiescent,
        active_socket_owners_quiescent: true,
        server_dispatch_quiescent: true,
        background_tasks_quiescent,
        m2_emitter_safe,
        activity_owners_quiescent,
        win_event_shutdown_history_quiescent,
        storage_service_owners_quiescent: storage_owner_readback.owners_quiescent,
        operator_hotkey_quiescent,
        operator_panic_k2_tasks_quiescent,
    };
    if !operator_hotkey_quiescent {
        crate::safety::retain_operator_hotkey_guard_to_process_exit(
            &mut operator_hotkey_guard,
            "http_startup_failure",
        );
    }
    let lifetime_lock_close = close_or_retain_http_lifetime_locks(
        shell_job_store_lock_guard,
        single_instance_guard,
        owners,
        "http_startup_failure",
    );
    let lifetime_locks_closed = lifetime_lock_close.is_ok();
    failures.inspect_result("lifetime_lock_close", lifetime_lock_close);
    if lifetime_locks_closed {
        let lifecycle_result = crate::daemon_lifecycle::record_startup_exit(
            "startup_http_runtime_failed",
            serde_json::json!({
                "phase": phase,
                "detail": primary_detail,
                "authority_finalizers_quiescent": authority_finalizers_quiescent,
                "session_input_owners_quiescent": session_input_owners_quiescent,
                "session_manager_quiescent": session_manager_quiescent,
                "background_tasks_quiescent": background_tasks_quiescent,
                "storage_service_owners": storage_owner_readback,
                "m2_emitter": format!("{m2_emitter_drain:?}"),
                "activity_owners": format!("{activity_drain:?}"),
                "operator_hotkey": format!("{hotkey_report:?}"),
                "operator_hotkey_install_unwind_retained_live_owner": install_unwind_retained_live_owner,
                "operator_panic_k2_owners_before": operator_panic_k2_owners_before,
                "operator_panic_k2_drain": operator_panic_k2_drain,
                "session_registry_rows": session_registry_rows.len(),
                "lease_after": lease_after,
                "lifetime_locks_closed": true,
            }),
        )
        .context("record daemon lifecycle HTTP runtime startup failure after lock close");
        failures.inspect_result("startup_lifecycle_record", lifecycle_result);
    } else {
        tracing::error!(
            code = "MCP_HTTP_STARTUP_EXIT_RECORD_SUPPRESSED",
            phase,
            owners = ?owners,
            "startup-exit lifecycle record suppressed because daemon lifetime locks were retained or failed to close"
        );
    }
    failures.into_result()?;
    anyhow::bail!("HTTP startup transaction failed without a recorded failure")
}

#[derive(Clone, Default)]
struct ActiveHttpSockets {
    #[cfg(windows)]
    inner: Arc<Mutex<BTreeMap<usize, ActiveHttpSocketInfo>>>,
    #[cfg(windows)]
    shutdown_on_drop: Arc<AtomicBool>,
}

#[cfg(windows)]
#[derive(Clone)]
struct ActiveHttpSocketInfo {
    raw_socket: usize,
    peer_addr: String,
    accepted_at_unix_ms: u128,
}

impl ActiveHttpSockets {
    #[cfg(windows)]
    fn begin_shutdown_on_drop(&self, reason: &'static str) -> ActiveHttpSocketShutdownOnDropReport {
        let was_enabled = self.shutdown_on_drop.swap(true, Ordering::SeqCst);
        let tracked_now = self.inner.lock().map_or(0, |sockets| sockets.len());
        ActiveHttpSocketShutdownOnDropReport {
            reason,
            enabled_now: true,
            was_enabled,
            tracked_now,
        }
    }

    #[cfg(not(windows))]
    fn begin_shutdown_on_drop(&self, reason: &'static str) -> ActiveHttpSocketShutdownOnDropReport {
        ActiveHttpSocketShutdownOnDropReport {
            reason,
            enabled_now: false,
            was_enabled: false,
            tracked_now: 0,
        }
    }

    #[cfg(windows)]
    fn shutdown_on_drop_enabled(&self) -> bool {
        self.shutdown_on_drop.load(Ordering::SeqCst)
    }

    #[cfg(windows)]
    fn register(&self, raw_socket: usize, peer_addr: SocketAddr) {
        HTTP_ACCEPTED_SOCKETS_TOTAL.fetch_add(1, Ordering::Relaxed);
        HTTP_ACCEPTED_SOCKETS_CURRENT.fetch_add(1, Ordering::Relaxed);
        let info = ActiveHttpSocketInfo {
            raw_socket,
            peer_addr: peer_addr.to_string(),
            accepted_at_unix_ms: u128::from(crate::server::session_registry::unix_time_ms_now()),
        };
        match self.inner.lock() {
            Ok(mut sockets) => {
                sockets.insert(raw_socket, info);
                tracing::debug!(
                    code = "MCP_HTTP_ACCEPTED_SOCKET_TRACKED",
                    raw_socket,
                    peer_addr = %peer_addr,
                    tracked_count = sockets.len(),
                    "tracked accepted HTTP socket for shutdown"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = "MCP_HTTP_ACCEPTED_SOCKET_TRACK_FAILED",
                    raw_socket,
                    peer_addr = %peer_addr,
                    error = %error,
                    "accepted HTTP socket registry lock poisoned"
                );
            }
        }
    }

    #[cfg(windows)]
    fn unregister(&self, raw_socket: usize) {
        saturating_decrement(&HTTP_ACCEPTED_SOCKETS_CURRENT);
        match self.inner.lock() {
            Ok(mut sockets) => {
                let removed = sockets.remove(&raw_socket).is_some();
                tracing::debug!(
                    code = "MCP_HTTP_ACCEPTED_SOCKET_UNTRACKED",
                    raw_socket,
                    removed,
                    tracked_count = sockets.len(),
                    "untracked accepted HTTP socket"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = "MCP_HTTP_ACCEPTED_SOCKET_UNTRACK_FAILED",
                    raw_socket,
                    error = %error,
                    "accepted HTTP socket registry lock poisoned while untracking"
                );
            }
        }
    }

    #[cfg(windows)]
    fn shutdown_socket_on_drop(&self, raw_socket: usize, reason: &'static str) {
        if !self.shutdown_on_drop_enabled() {
            return;
        }
        let shutdown_result = unsafe { winsock_shutdown(SOCKET(raw_socket), SD_BOTH) };
        if shutdown_result == 0 {
            tracing::warn!(
                code = "MCP_HTTP_ACCEPTED_SOCKET_DROP_SHUTDOWN",
                raw_socket,
                reason,
                "accepted HTTP socket was shut down during drop after daemon restart drain began"
            );
        } else {
            let error = unsafe { WSAGetLastError() };
            tracing::warn!(
                code = "MCP_HTTP_ACCEPTED_SOCKET_DROP_SHUTDOWN_FAILED",
                raw_socket,
                reason,
                wsa_error = error.0,
                "accepted HTTP socket drop shutdown failed after daemon restart drain began"
            );
        }
    }

    fn shutdown_all(&self, reason: &'static str) -> ActiveHttpSocketShutdownReport {
        #[cfg(windows)]
        {
            let _ = self.begin_shutdown_on_drop(reason);
            let tracked = match self.inner.lock() {
                Ok(sockets) => sockets.values().cloned().collect::<Vec<_>>(),
                Err(error) => {
                    return ActiveHttpSocketShutdownReport {
                        reason,
                        tracked_before: 0,
                        shutdown_attempted: 0,
                        shutdown_succeeded: 0,
                        failure_count: 1,
                        tracked_after_shutdown_attempt: 0,
                        sockets: Vec::new(),
                        failures: vec![format!("registry_lock_poisoned:{error}")],
                    };
                }
            };
            let mut failures = Vec::new();
            let mut succeeded = 0;
            for socket in &tracked {
                let shutdown_result =
                    unsafe { winsock_shutdown(SOCKET(socket.raw_socket), SD_BOTH) };
                if shutdown_result == 0 {
                    succeeded += 1;
                } else {
                    let error = unsafe { WSAGetLastError() };
                    failures.push(format!(
                        "raw_socket={} peer_addr={} wsa_error={}",
                        socket.raw_socket, socket.peer_addr, error.0
                    ));
                }
            }
            let tracked_after_shutdown_attempt = match self.inner.lock() {
                Ok(sockets) => sockets.len(),
                Err(error) => {
                    failures.push(format!("registry_readback_lock_poisoned:{error}"));
                    0
                }
            };
            ActiveHttpSocketShutdownReport {
                reason,
                tracked_before: tracked.len(),
                shutdown_attempted: tracked.len(),
                shutdown_succeeded: succeeded,
                failure_count: failures.len(),
                tracked_after_shutdown_attempt,
                sockets: tracked
                    .into_iter()
                    .map(|socket| ActiveHttpSocketShutdownRow {
                        raw_socket: socket.raw_socket,
                        peer_addr: socket.peer_addr,
                        accepted_at_unix_ms: socket.accepted_at_unix_ms,
                    })
                    .collect(),
                failures,
            }
        }
        #[cfg(not(windows))]
        {
            ActiveHttpSocketShutdownReport {
                reason,
                tracked_before: 0,
                shutdown_attempted: 0,
                shutdown_succeeded: 0,
                failure_count: 0,
                tracked_after_shutdown_attempt: 0,
                sockets: Vec::new(),
                failures: Vec::new(),
            }
        }
    }

    /// Separately read the accepted-socket registry after the listener and all
    /// connection tasks have stopped. This is the registry Source of Truth;
    /// the earlier socket-shutdown syscall result is only a trigger verdict.
    #[cfg(windows)]
    fn final_tracked_count(&self) -> Result<usize, String> {
        self.inner
            .lock()
            .map(|sockets| sockets.len())
            .map_err(|error| format!("registry_lock_poisoned:{error}"))
    }

    #[cfg(not(windows))]
    fn final_tracked_count(&self) -> Result<usize, String> {
        Ok(0)
    }
}

#[cfg(windows)]
struct TrackedTcpListener {
    inner: TcpListener,
    sockets: ActiveHttpSockets,
}

#[cfg(windows)]
impl Listener for TrackedTcpListener {
    type Io = TrackedTcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    if let Err(error) = stream.set_zero_linger() {
                        tracing::error!(
                            code = "MCP_HTTP_ACCEPTED_SOCKET_ZERO_LINGER_FAILED",
                            error = %error,
                            peer_addr = %addr,
                            "failed to configure accepted HTTP socket for abortive close on daemon shutdown"
                        );
                    }
                    let raw_socket = stream.as_raw_socket() as usize;
                    self.sockets.register(raw_socket, addr);
                    return (
                        TrackedTcpStream {
                            inner: stream,
                            sockets: self.sockets.clone(),
                            raw_socket,
                        },
                        addr,
                    );
                }
                Err(error) => handle_http_accept_error(error).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[cfg(windows)]
struct TrackedTcpStream {
    inner: tokio::net::TcpStream,
    sockets: ActiveHttpSockets,
    raw_socket: usize,
}

#[cfg(windows)]
impl Drop for TrackedTcpStream {
    fn drop(&mut self) {
        self.sockets
            .shutdown_socket_on_drop(self.raw_socket, "tracked_tcp_stream_drop");
        self.sockets.unregister(self.raw_socket);
    }
}

#[cfg(windows)]
impl AsyncRead for TrackedTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl AsyncWrite for TrackedTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub(super) async fn serve(
    bind: &str,
    allow_non_loopback: bool,
    m2_config: &M2ServiceConfig,
    m3_config: M3ServiceConfig,
    m4_config: M4ServiceConfig,
) -> anyhow::Result<ExitCode> {
    synapse_action::install_panic_hook();

    // Validate the bind address first — a pure argument check with no side
    // effects. Doing this before acquiring the single-instance lock means a
    // misconfigured non-loopback bind always fails with HTTP_BIND_NON_LOOPBACK_
    // REFUSED (exit 2), even when another daemon already holds the DB lock
    // (which would otherwise short-circuit to exit 3 and mask the real problem).
    let addr = bind
        .parse::<SocketAddr>()
        .with_context(|| format!("parse HTTP bind address {bind}"))?;
    if !addr.ip().is_loopback() && !allow_non_loopback {
        tracing::error!(
            code = synapse_core::error_codes::HTTP_BIND_NON_LOOPBACK_REFUSED,
            bind = %addr,
            "refusing non-loopback HTTP bind without --allow-non-loopback"
        );
        return Ok(ExitCode::from(2));
    }

    // Single-instance guard: at most one daemon may own a given RocksDB path.
    // Acquired before binding the port or opening storage so a duplicate launch
    // fails fast with a clear, holder-naming error instead of a cryptic RocksDB
    // LOCK failure surfacing later inside a tool call.
    let db_path = m3_config
        .db_path
        .clone()
        .unwrap_or_else(crate::m3::default_db_path);
    let single_instance_guard = match crate::single_instance::SingleInstanceGuard::acquire(&db_path)
    {
        Ok(guard) => {
            tracing::info!(
                code = "MCP_DAEMON_SINGLE_INSTANCE_ACQUIRED",
                lock_path = %guard.lock_path().display(),
                db_path = %db_path.display(),
                pid = std::process::id(),
                "daemon single-instance lock acquired"
            );
            guard
        }
        Err(crate::single_instance::SingleInstanceError::AlreadyRunning {
            lock_path,
            holder_pid,
        }) => {
            tracing::error!(
                code = "MCP_DAEMON_ALREADY_RUNNING",
                lock_path = %lock_path.display(),
                holder_pid = holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                db_path = %db_path.display(),
                "refusing to start: another synapse-mcp daemon already owns this DB path"
            );
            return Ok(ExitCode::from(3));
        }
        Err(err @ crate::single_instance::SingleInstanceError::Io { .. }) => {
            return Err(anyhow::Error::new(err)).context("acquire daemon single-instance lock");
        }
    };

    let shell_job_root = match crate::m4::shell_job_root_dir() {
        Ok(root) => root,
        Err(error) => {
            let detail = error.message.to_string();
            let error_data = error.data.unwrap_or(serde_json::Value::Null);
            tracing::error!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_ROOT_RESOLUTION_FAILED",
                mode = "http",
                bind = %addr,
                db_path = %db_path.display(),
                detail = %detail,
                error_data = ?error_data,
                "refusing to start: durable shell-job store root could not be resolved"
            );
            anyhow::bail!("resolve durable shell-job store root: {detail}; data={error_data}");
        }
    };
    let shell_job_store_lock_guard = match crate::single_instance::ShellJobStoreLockGuard::acquire(
        &shell_job_root,
    ) {
        Ok(guard) => {
            tracing::info!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_LOCK_ACQUIRED",
                mode = "http",
                bind = %addr,
                store_root = %guard.store_root().display(),
                lock_path = %guard.lock_path().display(),
                pid = std::process::id(),
                "daemon acquired exclusive shell-job store ownership"
            );
            guard
        }
        Err(crate::single_instance::ShellJobStoreLockError::AlreadyOwned {
            store_root,
            lock_path,
            holder_pid,
        }) => {
            tracing::error!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_ALREADY_OWNED",
                mode = "http",
                bind = %addr,
                store_root = %store_root.display(),
                lock_path = %lock_path.display(),
                holder_pid = holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                db_path = %db_path.display(),
                "refusing to start: another daemon owns the durable shell-job store"
            );
            return Ok(ExitCode::from(3));
        }
        Err(error @ crate::single_instance::ShellJobStoreLockError::Io { .. }) => {
            tracing::error!(
                code = "MCP_DAEMON_SHELL_JOB_STORE_LOCK_FAILED",
                mode = "http",
                bind = %addr,
                detail = %error,
                db_path = %db_path.display(),
                "refusing to start: durable shell-job store ownership could not be acquired"
            );
            return Err(anyhow::Error::new(error))
                .context("acquire durable shell-job store lifetime lock");
        }
    };
    let canonical_shell_job_root = shell_job_store_lock_guard.store_root().to_path_buf();
    if let Err(error) = crate::m4::freeze_shell_job_root_for_daemon(&canonical_shell_job_root) {
        let detail = error.message.to_string();
        let error_data = error.data.unwrap_or(serde_json::Value::Null);
        tracing::error!(
            code = "MCP_DAEMON_SHELL_JOB_STORE_ROOT_FREEZE_FAILED",
            mode = "http",
            bind = %addr,
            db_path = %db_path.display(),
            shell_job_root = %canonical_shell_job_root.display(),
            detail = %detail,
            error_data = ?error_data,
            "refusing to start: guarded shell-job store root could not be frozen for daemon operations"
        );
        anyhow::bail!(
            "freeze guarded durable shell-job store root {}: {detail}; data={error_data}",
            canonical_shell_job_root.display()
        );
    }

    let lifecycle_paths =
        crate::daemon_lifecycle::configure(crate::daemon_lifecycle::DaemonLifecycleConfig {
            mode: "http",
            bind_addr: Some(addr.to_string()),
            db_path: db_path.clone(),
        })
        .context("configure daemon lifecycle ledger")?;
    crate::daemon_lifecycle::install_panic_hook();
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_READY",
        run_current_path = %lifecycle_paths.run_current_path,
        tool_last_path = %lifecycle_paths.tool_last_path,
        tool_events_path = %lifecycle_paths.tool_events_path,
        exit_events_path = %lifecycle_paths.exit_events_path,
        "daemon lifecycle ledger ready"
    );

    // #1568: corrupt durable shell-job evidence is a startup safety gate. Run
    // it after the independent shell-job lifetime lock proves this process owns
    // that store and before binding a listener or constructing request-serving
    // state. Ordinary TTL retention inside this pass remains best-effort.
    if let Err(error) = crate::m4::reap_stale_shell_jobs_on_startup() {
        let detail = error.message.to_string();
        let error_data = error.data.unwrap_or(serde_json::Value::Null);
        tracing::error!(
            code = "MCP_DAEMON_STARTUP_SHELL_JOB_RECOVERY_FAILED",
            mode = "http",
            bind = %addr,
            db_path = %db_path.display(),
            shell_job_root = %canonical_shell_job_root.display(),
            detail = %detail,
            error_data = ?error_data,
            "refusing to start: corrupt durable shell-job recovery did not reach a verified terminal disposition"
        );
        crate::daemon_lifecycle::record_startup_exit(
            "startup_corrupt_shell_job_recovery_failed",
            serde_json::json!({
                "mode": "http",
                "bind": addr.to_string(),
                "db_path": db_path.display().to_string(),
                "shell_job_root": canonical_shell_job_root.display().to_string(),
                "detail": detail,
                "error_data": error_data,
            }),
        )
        .context("record daemon lifecycle startup corrupt-shell-job recovery failure")?;
        return Ok(ExitCode::from(4));
    }

    if !addr.ip().is_loopback() {
        tracing::warn!(
            code = "MCP_HTTP_NON_LOOPBACK_BIND_ALLOWED",
            bind = %addr,
            "non-loopback HTTP bind allowed by explicit operator flag"
        );
    }
    let listener = bind_http_listener(addr).await?;
    let local_addr = listener
        .local_addr()
        .context("read HTTP listener address")?;
    let shutdown_cancel = CancellationToken::new();
    let connection_closed_cancel = CancellationToken::new();
    let sse_state = SseState::with_max_subscriptions(m3_config.max_subscriptions);
    let service = http_service(
        shutdown_cancel.clone(),
        connection_closed_cancel.clone(),
        sse_state.clone(),
        m2_config,
        m3_config,
        m4_config,
    )
    .context("initialize shared HTTP service state")?;
    let m3_state_for_recorder = service.m3_state_handle();
    let m2_emitter_owner = take_m2_emitter_owner(&service);

    // Eager storage open + maintenance startup: validate RocksDB and retain the
    // periodic GC/pressure task handles before serving any MCP request, so a
    // lock/schema/task/probe fault fails fast instead of reporting healthy
    // storage while retention is inert. The handle is cached and reused by the
    // reflex runtime, so there is no open-then-reopen race.
    {
        let open_or_maintenance_result = match m3_state_for_recorder.lock() {
            Ok(mut state) => Some(
                state
                    .ensure_storage()
                    .and_then(|_| state.ensure_storage_maintenance_tasks()),
            ),
            Err(poisoned) => {
                drop(poisoned);
                None
            }
        };
        let Some(open_or_maintenance_result) = open_or_maintenance_result else {
            drop(listener);
            return fail_http_startup_after_service(
                HttpRuntimeStartupFailure::new(
                    "storage_state_lock",
                    anyhow::anyhow!(
                        "m3 service state lock poisoned during startup storage open/maintenance"
                    ),
                    Vec::new(),
                    None,
                ),
                service,
                Arc::clone(&m3_state_for_recorder),
                m2_emitter_owner,
                false,
                false,
                shutdown_cancel,
                connection_closed_cancel,
                shell_job_store_lock_guard,
                single_instance_guard,
            )
            .await;
        };
        if let Err(error) = open_or_maintenance_result {
            let detail = error.to_string();
            if detail.to_lowercase().contains("lock") {
                tracing::error!(
                    code = "STORAGE_LOCK_CONTENDED",
                    db_path = %db_path.display(),
                    detail = %detail,
                    "refusing to start: RocksDB storage lock is held by another process; run `synapse-mcp doctor` to find and stop the holder, or point this daemon at a different --db path"
                );
            } else {
                tracing::error!(
                    code = "STORAGE_OPEN_OR_MAINTENANCE_START_FAILED",
                    db_path = %db_path.display(),
                    detail = %detail,
                    "refusing to start: storage open/maintenance startup failed at daemon startup"
                );
            }
            drop(listener);
            return fail_http_startup_after_service(
                HttpRuntimeStartupFailure::new(
                    "storage_open_or_maintenance_start",
                    anyhow::anyhow!(detail),
                    Vec::new(),
                    None,
                ),
                service,
                m3_state_for_recorder,
                m2_emitter_owner,
                false,
                false,
                shutdown_cancel,
                connection_closed_cancel,
                shell_job_store_lock_guard,
                single_instance_guard,
            )
            .await;
        }
        tracing::info!(
            code = "MCP_DAEMON_STORAGE_OPENED_AND_MAINTENANCE_STARTED",
            db_path = %db_path.display(),
            "daemon storage opened eagerly and storage maintenance tasks started at startup"
        );
    }

    // Always-on activity recorder (#837): started eagerly so the operator
    // timeline records whenever the daemon runs, before any tool call can
    // lazily start a recorder-less WinEvent bridge. A recorder that cannot
    // record is a startup failure, not a degraded mode.
    {
        let recorder_result = match m3_state_for_recorder.lock() {
            Ok(mut state) => Some(state.ensure_activity_recorder(sse_state.event_bus())),
            Err(poisoned) => {
                drop(poisoned);
                None
            }
        };
        let Some(recorder_result) = recorder_result else {
            drop(listener);
            return fail_http_startup_after_service(
                HttpRuntimeStartupFailure::new(
                    "activity_recorder_state_lock",
                    anyhow::anyhow!(
                        "m3 service state lock poisoned during activity recorder startup"
                    ),
                    Vec::new(),
                    None,
                ),
                service,
                Arc::clone(&m3_state_for_recorder),
                m2_emitter_owner,
                false,
                false,
                shutdown_cancel,
                connection_closed_cancel,
                shell_job_store_lock_guard,
                single_instance_guard,
            )
            .await;
        };
        if let Err(error) = recorder_result {
            let detail = format!("{error:#}");
            tracing::error!(
                code = "TIMELINE_RECORDER_START_FAILED",
                db_path = %db_path.display(),
                detail = %detail,
                "refusing to start: activity recorder failed at daemon startup"
            );
            drop(listener);
            return fail_http_startup_after_service(
                HttpRuntimeStartupFailure::new(
                    "activity_recorder_start",
                    anyhow::anyhow!(detail),
                    Vec::new(),
                    None,
                ),
                service,
                m3_state_for_recorder,
                m2_emitter_owner,
                false,
                false,
                shutdown_cancel,
                connection_closed_cancel,
                shell_job_store_lock_guard,
                single_instance_guard,
            )
            .await;
        }
        tracing::info!(
            code = "MCP_DAEMON_ACTIVITY_RECORDER_STARTED",
            "activity recorder started eagerly at startup"
        );
    }

    let active_http_sockets = ActiveHttpSockets::default();
    let HttpRuntimeStartup {
        mut background_tasks,
        mut operator_hotkey_guard,
        mut runtime,
    } = match start_http_runtime(
        &service,
        &shutdown_cancel,
        local_addr,
        sse_state,
        active_http_sockets.clone(),
    ) {
        Ok(startup) => startup,
        Err(failure) => {
            drop(listener);
            return fail_http_startup_after_service(
                failure,
                service,
                m3_state_for_recorder,
                m2_emitter_owner,
                true,
                true,
                shutdown_cancel,
                connection_closed_cancel,
                shell_job_store_lock_guard,
                single_instance_guard,
            )
            .await;
        }
    };
    let m2_emitter_done = m2_emitter_owner.done_receiver();

    tracing::info!(
        code = "MCP_HTTP_STARTED",
        bind = %local_addr,
        "starting streamable HTTP MCP transport"
    );

    let shutdown_cancel_for_http_endpoint = shutdown_cancel.clone();
    let mut server_task = ShutdownTaskOwner::new(
        "http_server_dispatch",
        spawn_server(
            listener,
            runtime.app.clone(),
            shutdown_cancel.clone(),
            active_http_sockets.clone(),
        ),
    );
    let m2_done_after_server_stop = m2_emitter_done.clone();
    let m2_done_after_signal = m2_emitter_done.clone();
    let m2_done_after_http_endpoint = m2_emitter_done;
    let (
        code,
        shutdown_source,
        mut shutdown_failures,
        authority_finalizers_quiescent,
        session_input_owners_quiescent,
        operator_owner_drain,
    ) = tokio::select! {
        result = &mut server_task => {
            let mut failures = HttpShutdownFailures::default();
            let server_result = result
                .context("join HTTP MCP transport")
                .and_then(|result| result.context("serve HTTP MCP transport"));
            let shutdown_was_requested = shutdown_cancel.is_cancelled();
            let source = if shutdown_was_requested {
                "http_endpoint"
            } else {
                "server_task_unexpected_stop"
            };
            if shutdown_was_requested {
                tracing::info!(
                    code = "MCP_HTTP_SERVER_STOPPED",
                    source,
                    pid = std::process::id(),
                    "HTTP listener task stopped after shutdown endpoint cancellation"
                );
            } else {
                tracing::error!(
                    code = "MCP_HTTP_SERVER_STOPPED_UNEXPECTEDLY",
                    source,
                    pid = std::process::id(),
                    "HTTP listener task stopped without a shutdown request; draining daemon state before failing"
                );
                shutdown_cancel.cancel();
            }
            connection_closed_cancel.cancel();
            let operator_owner_drain =
                drain_http_operator_owners(&mut operator_hotkey_guard, source).await;
            let session_close =
                close_active_mcp_sessions_for_shutdown(&runtime.session_manager, source).await;
            tracing::warn!(
                code = "MCP_HTTP_SHUTDOWN_SESSIONS_CLOSED",
                source,
                session_close = ?session_close,
                "active MCP sessions received close attempts and the session-manager Source of Truth was reread"
            );
            failures.inspect_session_close(&session_close);
            let socket_shutdown = active_http_sockets.shutdown_all(source);
            tracing::warn!(
                code = "MCP_HTTP_ACTIVE_SOCKETS_SHUTDOWN",
                source,
                socket_shutdown = ?socket_shutdown,
                "accepted HTTP sockets explicitly shut down during daemon shutdown"
            );
            failures.inspect_socket_shutdown(&socket_shutdown);
            let cleanup = cleanup_active_session_inputs_for_shutdown(
                &runtime.session_lifecycle,
                &runtime.session_manager,
                &session_close.session_ids,
                source,
            ).await;
            tracing::info!(
                code = "MCP_HTTP_SHUTDOWN_INPUT_CLEANUP",
                source,
                cleanup = ?cleanup,
                "readback=session_input_ownership edge=http_server_task_stop after_cleanup"
            );
            let authority_finalizers_quiescent =
                cleanup.authority_finalizer_owners_quiescent();
            let session_input_owners_quiescent = cleanup.all_input_owners_quiescent();
            failures.inspect_input_cleanup(&cleanup);
            let emitter_drain = wait_for_m2_emitter_done(
                m2_done_after_server_stop,
                "http",
                source,
            )
            .await;
            failures.inspect_result(
                "m2_emitter_drain",
                emitter_drain.context("drain M2 emitter after HTTP server task completed"),
            );
            failures.inspect_result("server_task", server_result);
            if !shutdown_was_requested {
                failures.push(
                    "shutdown_request",
                    "HTTP MCP transport stopped without a shutdown request",
                );
            }
            (
                ExitCode::SUCCESS,
                source,
                failures,
                authority_finalizers_quiescent,
                session_input_owners_quiescent,
                operator_owner_drain,
            )
        }
        signal = wait_for_shutdown_signal("http") => {
            let mut failures = HttpShutdownFailures::default();
            if let Err(error) = &signal {
                tracing::error!(
                    code = "MCP_HTTP_SHUTDOWN_SIGNAL_WAIT_FAILED",
                    source = "signal",
                    error = %error,
                    "HTTP shutdown-signal listener failed; draining daemon state before returning the error"
                );
            }
            let drain = runtime.drain_state.mark_draining("signal");
            let shutdown_on_drop = active_http_sockets.begin_shutdown_on_drop("signal");
            tracing::warn!(
                code = "MCP_HTTP_SOCKET_SHUTDOWN_ON_DROP_ENABLED",
                source = "signal",
                shutdown_on_drop = ?shutdown_on_drop,
                "accepted HTTP socket drop now performs socket shutdown during daemon restart drain"
            );
            if signal.is_ok() {
                tracing::info!(code = "MCP_SHUTDOWN_GRACEFUL", "HTTP shutdown signal received");
            }
            let operator_owner_drain =
                drain_http_operator_owners(&mut operator_hotkey_guard, "signal").await;
            let session_close =
                close_active_mcp_sessions_for_shutdown(&runtime.session_manager, "signal").await;
            tracing::warn!(
                code = "MCP_HTTP_SHUTDOWN_SESSIONS_CLOSED",
                source = "signal",
                session_close = ?session_close,
                "active MCP sessions received close attempts and the session-manager Source of Truth was reread"
            );
            failures.inspect_session_close(&session_close);
            time::sleep(DRAIN_RESPONSE_GRACE_TIMEOUT).await;
            shutdown_cancel.cancel();
            connection_closed_cancel.cancel();
            let socket_shutdown = active_http_sockets.shutdown_all("signal");
            tracing::warn!(
                code = "MCP_HTTP_ACTIVE_SOCKETS_SHUTDOWN",
                source = "signal",
                socket_shutdown = ?socket_shutdown,
                "accepted HTTP sockets explicitly shut down during daemon shutdown"
            );
            failures.inspect_socket_shutdown(&socket_shutdown);
            let server_stop = wait_for_server_stop(&mut server_task, "signal").await;
            let cleanup = cleanup_active_session_inputs_for_shutdown(
                &runtime.session_lifecycle,
                &runtime.session_manager,
                &session_close.session_ids,
                "signal",
            ).await;
            tracing::info!(
                code = "MCP_HTTP_SHUTDOWN_INPUT_CLEANUP",
                drain = ?drain,
                cleanup = ?cleanup,
                "readback=session_input_ownership edge=signal_shutdown after_cleanup"
            );
            let authority_finalizers_quiescent =
                cleanup.authority_finalizer_owners_quiescent();
            let session_input_owners_quiescent = cleanup.all_input_owners_quiescent();
            failures.inspect_input_cleanup(&cleanup);
            let emitter_drain =
                wait_for_m2_emitter_done(m2_done_after_signal, "http", "signal").await;
            failures.inspect_result("server_stop", server_stop);
            failures.inspect_result(
                "shutdown_signal",
                signal.context("wait for HTTP shutdown signal"),
            );
            failures.inspect_result(
                "m2_emitter_drain",
                emitter_drain.context("drain M2 emitter after HTTP signal shutdown"),
            );
            (
                ExitCode::SUCCESS,
                "signal",
                failures,
                authority_finalizers_quiescent,
                session_input_owners_quiescent,
                operator_owner_drain,
            )
        }
        _ = shutdown_cancel_for_http_endpoint.cancelled() => {
            let mut failures = HttpShutdownFailures::default();
            // The `/shutdown` handler marks drain state, returns its ACCEPTED
            // response, and only cancels this token after
            // DRAIN_RESPONSE_GRACE_TIMEOUT. Reaching this branch therefore
            // means the bounded response-flush grace has already elapsed.
            tracing::info!(
                code = "MCP_HTTP_SHUTDOWN_TOKEN_CANCELLED",
                source = "http_endpoint",
                pid = std::process::id(),
                response_grace_ms = DRAIN_RESPONSE_GRACE_TIMEOUT.as_millis(),
                "HTTP shutdown endpoint cancellation observed by daemon supervisor"
            );
            let shutdown_on_drop = active_http_sockets.begin_shutdown_on_drop("http_endpoint");
            tracing::warn!(
                code = "MCP_HTTP_SOCKET_SHUTDOWN_ON_DROP_ENABLED",
                source = "http_endpoint",
                shutdown_on_drop = ?shutdown_on_drop,
                "accepted HTTP socket drop now performs socket shutdown during daemon restart drain"
            );
            let operator_owner_drain =
                drain_http_operator_owners(&mut operator_hotkey_guard, "http_endpoint").await;
            let session_close =
                close_active_mcp_sessions_for_shutdown(&runtime.session_manager, "http_endpoint").await;
            tracing::warn!(
                code = "MCP_HTTP_SHUTDOWN_SESSIONS_CLOSED",
                source = "http_endpoint",
                session_close = ?session_close,
                "active MCP sessions received close attempts and the session-manager Source of Truth was reread"
            );
            failures.inspect_session_close(&session_close);
            connection_closed_cancel.cancel();
            let socket_shutdown = active_http_sockets.shutdown_all("http_endpoint");
            tracing::warn!(
                code = "MCP_HTTP_ACTIVE_SOCKETS_SHUTDOWN",
                source = "http_endpoint",
                socket_shutdown = ?socket_shutdown,
                "accepted HTTP sockets explicitly shut down during daemon shutdown"
            );
            failures.inspect_socket_shutdown(&socket_shutdown);
            tracing::info!(
                code = "MCP_HTTP_CONNECTIONS_CANCELLED",
                source = "http_endpoint",
                pid = std::process::id(),
                "connection-scoped work cancelled before waiting for HTTP listener stop"
            );
            let server_stop = wait_for_server_stop(&mut server_task, "http_endpoint").await;
            let cleanup = cleanup_active_session_inputs_for_shutdown(
                &runtime.session_lifecycle,
                &runtime.session_manager,
                &session_close.session_ids,
                "http_endpoint",
            ).await;
            tracing::info!(
                code = "MCP_HTTP_SHUTDOWN_INPUT_CLEANUP",
                cleanup = ?cleanup,
                "readback=session_input_ownership edge=http_endpoint_shutdown after_cleanup"
            );
            let authority_finalizers_quiescent =
                cleanup.authority_finalizer_owners_quiescent();
            let session_input_owners_quiescent = cleanup.all_input_owners_quiescent();
            failures.inspect_input_cleanup(&cleanup);
            let emitter_drain =
                wait_for_m2_emitter_done(m2_done_after_http_endpoint, "http", "http_endpoint")
                    .await;
            failures.inspect_result("server_stop", server_stop);
            failures.inspect_result(
                "m2_emitter_drain",
                emitter_drain.context("drain M2 emitter after HTTP endpoint shutdown"),
            );
            (
                ExitCode::SUCCESS,
                "http_endpoint",
                failures,
                authority_finalizers_quiescent,
                session_input_owners_quiescent,
                operator_owner_drain,
            )
        }
    };
    let operator_hotkey_quiescent = operator_owner_drain.hotkey_owners_quiescent();
    let operator_panic_k2_tasks_quiescent = operator_owner_drain.k2_tasks_quiescent();
    operator_owner_drain.inspect_failures(&mut shutdown_failures);
    let server_dispatch_quiescent = server_task.terminal_join_observed();
    tracing::info!(
        code = "MCP_HTTP_SERVER_TASK_FINAL_READBACK",
        source = shutdown_source,
        server_dispatch_quiescent,
        "readback=http_server_join_handle edge=shutdown_supervisor after_stop_attempt"
    );
    if !server_dispatch_quiescent {
        shutdown_failures.push(
            "server_dispatch_readback",
            "HTTP server dispatch JoinHandle remained live after stop/abort attempts",
        );
    }
    let final_tracked_count = active_http_sockets.final_tracked_count();
    let active_socket_owners_quiescent = final_tracked_count
        .as_ref()
        .is_ok_and(|tracked_count| *tracked_count == 0);
    tracing::info!(
        code = "MCP_HTTP_ACTIVE_SOCKET_REGISTRY_FINAL_READBACK",
        source = shutdown_source,
        final_tracked_count = ?final_tracked_count,
        "readback=accepted_http_socket_registry edge=http_server_stopped after_cleanup"
    );
    shutdown_failures.inspect_final_socket_count(final_tracked_count);
    let final_session_ids = active_http_session_ids(&runtime.session_manager).await;
    let session_manager_quiescent = final_session_ids.is_empty();
    tracing::info!(
        code = "MCP_HTTP_SESSION_MANAGER_FINAL_READBACK",
        source = shutdown_source,
        final_session_count = final_session_ids.len(),
        final_session_ids = ?final_session_ids,
        "readback=local_session_manager edge=http_server_stopped after_cleanup"
    );
    shutdown_failures.inspect_final_session_ids(&final_session_ids);
    // Stop the WinEvent source before the recorder so session_end is the final
    // timeline row, then require terminal readback from all Tokio/OS owners.
    let activity_drain = drain_http_activity_owners(&m3_state_for_recorder, true, true).await;
    let activity_owners_quiescent = activity_drain.safe_to_unlock();
    let win_event_shutdown_history_quiescent =
        activity_drain.win_event_shutdown_history.owners_quiescent();
    shutdown_failures.inspect_result("activity_owner_drain", activity_drain.verdict());

    let m2_emitter_drain =
        drain_m2_emitter_owner(Some(m2_emitter_owner), "http", shutdown_source).await;
    let m2_emitter_safe = m2_emitter_drain.safe_to_unlock();
    shutdown_failures.inspect_result("m2_emitter_owner_join", m2_emitter_drain.verdict());
    background_tasks.append(&mut runtime.background_tasks);
    let background_task_drain = drain_http_background_tasks(background_tasks).await;
    let background_tasks_quiescent = background_task_drain.owners_quiescent();
    tracing::info!(
        code = "MCP_HTTP_BACKGROUND_TASKS_FINAL_READBACK",
        source = shutdown_source,
        background_tasks_quiescent,
        report = ?background_task_drain,
        "readback=http_background_join_handles edge=shutdown_supervisor after_drain"
    );
    shutdown_failures.inspect_result(
        "background_task_drain",
        background_task_drain
            .verdict()
            .context("drain HTTP daemon background tasks before releasing lifetime locks"),
    );

    // The custom daemon lock must outlive every RocksDB/service owner. Dropping
    // these Arcs and callbacks before unlock prevents a successor from winning
    // daemon.lock only to collide with this process's still-live RocksDB LOCK.
    drop(runtime);
    drop(service);
    drop(shutdown_cancel);
    drop(connection_closed_cancel);
    let storage_owner_readback = m3_storage_owner_readback(&m3_state_for_recorder);
    tracing::info!(
        code = "MCP_HTTP_STORAGE_SERVICE_OWNER_FINAL_READBACK",
        source = shutdown_source,
        readback = ?storage_owner_readback,
        "readback=m3_storage_arc_owners edge=shutdown_supervisor after_expected_owner_drop"
    );
    if !storage_owner_readback.owners_quiescent {
        shutdown_failures.push(
            "storage_service_owner_readback",
            format!("readback={storage_owner_readback:?}"),
        );
    }
    drop(m3_state_for_recorder);
    let owners = HttpLifetimeOwnerReadback {
        authority_finalizers_quiescent,
        session_input_owners_quiescent,
        session_manager_quiescent,
        active_socket_owners_quiescent,
        server_dispatch_quiescent,
        background_tasks_quiescent,
        m2_emitter_safe,
        activity_owners_quiescent,
        win_event_shutdown_history_quiescent,
        storage_service_owners_quiescent: storage_owner_readback.owners_quiescent,
        operator_hotkey_quiescent,
        operator_panic_k2_tasks_quiescent,
    };
    if !operator_hotkey_quiescent {
        crate::safety::retain_operator_hotkey_guard_to_process_exit(
            &mut operator_hotkey_guard,
            shutdown_source,
        );
    }
    let unresolved_shell_child_owner_report_before_lifecycle =
        crate::m4::unresolved_shell_child_owner_report();
    let unresolved_shell_child_owners_quiescent_before_lifecycle =
        unresolved_shell_child_owner_report_before_lifecycle.safe_to_unlock();
    if !unresolved_shell_child_owners_quiescent_before_lifecycle {
        shutdown_failures.push(
            "unresolved_shell_child_owner_readback",
            format!("readback={unresolved_shell_child_owner_report_before_lifecycle:?}"),
        );
    }
    let lifecycle_finalization = if owners.safe_to_unlock()
        && unresolved_shell_child_owners_quiescent_before_lifecycle
    {
        tracing::info!(
            code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_START",
            source = "http_service_completed",
            pid = std::process::id(),
            "locking daemon lifecycle finalization before releasing HTTP lifetime locks"
        );
        match crate::daemon_lifecycle::begin_graceful_exit_finalization() {
            Ok(finalization) => Some(finalization),
            Err(error) => {
                shutdown_failures.push(
                    "lifecycle_finalization_begin",
                    format!(
                        "lock graceful lifecycle transaction before HTTP lifetime-lock close: {error:#}"
                    ),
                );
                None
            }
        }
    } else {
        None
    };
    // Keep the exact listener output unacknowledged across every awaited
    // cleanup phase. Cancellation before this final synchronous lock gate must
    // remain a sticky incident rather than losing the transport verdict.
    if server_task.terminal_join_observed() {
        server_task.acknowledge_terminal_outcome();
    }
    drop(server_task);
    let lifetime_lock_close = close_or_retain_http_lifetime_locks(
        shell_job_store_lock_guard,
        single_instance_guard,
        owners,
        shutdown_source,
    );
    shutdown_failures.inspect_result("lifetime_lock_close", lifetime_lock_close);
    shutdown_failures.into_result()?;
    let lifecycle_finalization = lifecycle_finalization.ok_or_else(|| {
        anyhow::anyhow!(
            "HTTP shutdown postconditions passed without an owned lifecycle finalization guard"
        )
    })?;
    crate::daemon_lifecycle::record_graceful_exit_after_lifetime_lock_close(
        lifecycle_finalization,
        "http_service_completed",
    )
    .map_err(|error| {
        tracing::error!(
            code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_FAILED",
            source = "http_service_completed",
            pid = std::process::id(),
            error = %error,
            "failed to write daemon lifecycle graceful HTTP service completion"
        );
        error
    })
    .context("record daemon lifecycle graceful HTTP service completion")?;
    tracing::info!(
        code = "MCP_DAEMON_LIFECYCLE_EXIT_WRITE_OK",
        source = "http_service_completed",
        pid = std::process::id(),
        "daemon lifecycle graceful HTTP service completion written"
    );
    tracing::info!(
        code = "MCP_HTTP_PROCESS_EXIT_DECISION",
        source = "http_service_completed",
        pid = std::process::id(),
        exit_code = 0,
        "HTTP daemon process returning success after graceful shutdown"
    );
    Ok(code)
}

async fn bind_http_listener(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP MCP transport to {addr}"))?;
    tracing::info!(
        code = "MCP_HTTP_BIND_NORMAL",
        bind = %addr,
        "HTTP listener bound with normal bind path"
    );
    Ok(listener)
}

fn router(
    shutdown_cancel: &CancellationToken,
    bind_addr: SocketAddr,
    sse_state: SseState,
    service: SynapseService,
    active_http_sockets: ActiveHttpSockets,
) -> anyhow::Result<HttpRouterRuntime> {
    let auth = Arc::new(HttpAuth::load(bind_addr).context("load HTTP bearer token")?);
    tracing::info!(
        code = "MCP_HTTP_AUTH_CONFIGURED",
        source = auth.source_label(),
        "HTTP bearer token configured"
    );
    let health_service = Arc::new(service.clone());
    let drain_state = service.drain_state_handle();
    let session_registry = service.session_registry_handle();
    let terminated_sessions = service.terminated_sessions_handle();
    let session_lifecycle = service
        .session_lifecycle_state()
        .map_err(|error| anyhow::anyhow!("initialize session lifecycle state: {error:?}"))?;
    let (mcp_service, session_manager) = streamable_service(shutdown_cancel, service)
        .context("initialize HTTP MCP session state")?;
    let agent_events_db =
        session_store_db(&health_service).context("open storage for agent-event ingress")?;
    // #898: install the live-event sink, rebuild agent states from the
    // journal, and start the heartbeat/process-probe liveness sweep.
    let liveness_config = crate::server::agent_state::load_liveness_config()
        .map_err(|detail| anyhow::anyhow!("agent liveness configuration invalid: {detail}"))?;
    crate::server::agent_state::rebuild_from_journal(&agent_events_db)
        .context("rebuild agent state tracker from CF_AGENT_EVENTS")?;
    // Install process-global projections only after every fallible router
    // preflight has passed. Both sinks are non-owning/one-shot callbacks and no
    // fallible return remains after this point.
    crate::server::agent_events::install_session_registry_activity_sink(Arc::clone(
        &session_registry,
    ));
    crate::server::agent_state::install_event_bus(sse_state.event_bus());
    let agent_liveness_task = spawn_agent_liveness_sweep(
        Arc::clone(&agent_events_db),
        liveness_config,
        shutdown_cancel.child_token(),
    );
    // #948: start the AFK escalation delivery worker (Tier 0 on-PC toast +
    // operator-supplied Tier 1 webhook ladder). Installs the wake signal that
    // `agent_state::emit_transitions` pulses on each live attention transition.
    let escalation_worker = crate::server::escalation::spawn_worker(
        Arc::clone(&agent_events_db),
        shutdown_cancel.child_token(),
    );
    let session_request = session::SessionCleanupState::request_state(
        Arc::clone(&session_registry),
        terminated_sessions,
    );
    let session_cleanup =
        session::SessionCleanupState::new(Arc::clone(&session_manager), session_lifecycle.clone());
    let stale_cleanup_task = spawn_stale_session_input_cleanup(
        Arc::clone(&session_manager),
        session_lifecycle.clone(),
        shutdown_cancel.child_token(),
    );
    let state = HttpState {
        bind_addr,
        health_service,
        session_manager: Arc::clone(&session_manager),
        shutdown_cancel: shutdown_cancel.clone(),
        drain_state: drain_state.clone(),
        active_http_sockets,
        sse_state,
        agent_events_db,
    };
    let protected_routes = Router::new()
        .route("/health", get(health))
        .route("/shutdown", post(shutdown))
        .route("/events", get(events).post(publish_event))
        .route("/events/stats", get(event_stats))
        .route(
            "/agent-events",
            post(agent_events_ingest).layer(DefaultBodyLimit::max(
                crate::server::agent_event_ingress::MAX_AGENT_EVENT_INGRESS_BODY_BYTES,
            )),
        )
        .route("/agent-events/stats", get(agent_events_ingress_stats))
        .route(
            "/codex-app-server/request",
            post(codex_app_server_request).layer(DefaultBodyLimit::max(
                crate::server::codex_app_server_bridge::MAX_CODEX_APP_SERVER_REQUEST_BODY_BYTES,
            )),
        )
        .route(
            "/agent-transcripts/stats",
            get(agent_transcripts_ingest_stats),
        )
        .route(
            "/chrome-debugger/native/register",
            post(crate::chrome_debugger_bridge::http_register),
        )
        .route(
            "/chrome-debugger/native/message",
            post(crate::chrome_debugger_bridge::http_message),
        )
        .route(
            "/chrome-debugger/native/next",
            get(crate::chrome_debugger_bridge::http_next),
        )
        .route(
            "/chrome-debugger/native/ws",
            get(crate::chrome_debugger_bridge::http_ws),
        )
        .route(
            "/chrome-debugger/native/maintenance-pause",
            post(crate::chrome_debugger_bridge::http_maintenance_pause),
        )
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            refuse_mcp_while_draining,
        ))
        .layer(middleware::from_fn_with_state(
            session_request,
            session::require_mcp_session,
        ))
        .layer(middleware::from_fn_with_state(
            session_cleanup,
            session::release_held_inputs_on_delete,
        ))
        .layer(middleware::from_fn_with_state(
            auth,
            auth::require_http_security,
        ))
        .layer(middleware::from_fn(record_mcp_transport_diagnostics));
    let dashboard_routes = Router::new()
        .route("/dashboard", get(dashboard_index))
        .route("/dashboard/assets/{asset}", get(dashboard_asset))
        .route("/dashboard/state.json", get(dashboard_state))
        .route("/dashboard/tray-state.json", get(dashboard_tray_state))
        .route(
            "/dashboard/events/subscribe",
            post(dashboard_events_subscribe)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route("/dashboard/events", get(dashboard_events))
        .route(
            "/dashboard/agent-terminal/{spawn_id}/ws",
            get(dashboard_agent_terminal_ws),
        )
        .route(
            "/dashboard/agent-recordings/{spawn_id}",
            get(dashboard_agent_recording),
        )
        .route("/dashboard/audit/query", get(dashboard_audit_query))
        .route(
            "/dashboard/agent-events/query",
            get(dashboard_agent_events_query),
        )
        .route(
            "/dashboard/saved-views",
            get(dashboard_saved_views)
                .post(dashboard_saved_view_upsert)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/saved-views/{view_id}",
            delete(dashboard_saved_view_delete),
        )
        .route(
            "/dashboard/local-model-spawn",
            post(dashboard_local_model_spawn).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/spawn-agent",
            post(dashboard_spawn_agent).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/agent-kill",
            post(dashboard_agent_kill)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/agent-broadcast",
            post(dashboard_agent_broadcast)
                .layer(DefaultBodyLimit::max(DASHBOARD_CONTEXT_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/fleet-stop",
            post(dashboard_fleet_stop)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/agent-interrupt",
            post(dashboard_agent_interrupt)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/agent-pause",
            post(dashboard_agent_pause)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/agent-resume",
            post(dashboard_agent_resume)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/agent-respawn",
            post(dashboard_agent_respawn).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/tasks/create",
            post(dashboard_task_create).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/tasks/update",
            post(dashboard_task_update).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/tasks/cancel",
            post(dashboard_task_cancel)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/tasks/dispatch-once",
            post(dashboard_task_dispatch_once)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/pause",
            post(dashboard_timeline_pause)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/resume",
            post(dashboard_timeline_resume)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/get",
            post(dashboard_timeline_get)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/search",
            post(dashboard_timeline_search)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/timeline/digest",
            post(dashboard_timeline_digest)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/episodes/list",
            post(dashboard_episode_list)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/episodes/get",
            post(dashboard_episode_get)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/routines/list",
            post(dashboard_routine_list)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/routines/inspect",
            post(dashboard_routine_inspect)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/routines/update",
            post(dashboard_routine_update)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/storage/timeline-purge",
            post(dashboard_storage_timeline_purge)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/storage/gc",
            post(dashboard_storage_gc)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/control-lease/force-release",
            post(dashboard_control_lease_force_release)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/control-lease/handoff",
            post(dashboard_control_lease_handoff)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/target-claims/prune",
            post(dashboard_target_claims_prune)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/templates",
            get(dashboard_template_list)
                .post(dashboard_template_upsert)
                .layer(DefaultBodyLimit::max(
                    DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
                )),
        )
        .route(
            "/dashboard/templates/{template_id}",
            delete(dashboard_template_delete),
        )
        .route("/dashboard/models", get(dashboard_model_list))
        .route(
            "/dashboard/models/probe",
            post(dashboard_model_probe).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/api-model/register",
            post(dashboard_api_model_register).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/api-model/update",
            post(dashboard_api_model_update).layer(DefaultBodyLimit::max(
                DASHBOARD_LOCAL_MODEL_SPAWN_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            "/dashboard/api-model/remove",
            post(dashboard_api_model_remove),
        )
        .route(
            "/dashboard/approval/decide",
            post(dashboard_approval_decide)
                .layer(DefaultBodyLimit::max(DASHBOARD_SAVED_VIEW_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/context/inject",
            post(dashboard_context_inject)
                .layer(DefaultBodyLimit::max(DASHBOARD_CONTEXT_BODY_LIMIT_BYTES)),
        )
        .route(
            "/dashboard/context/plan",
            post(dashboard_context_plan)
                .layer(DefaultBodyLimit::max(DASHBOARD_CONTEXT_BODY_LIMIT_BYTES)),
        )
        .route("/approval/activate", get(approval_activate));
    let app = Router::new()
        .merge(dashboard_routes)
        .merge(protected_routes)
        .with_state(state)
        .layer(middleware::map_response(force_connection_close));
    Ok(HttpRouterRuntime {
        app,
        session_manager,
        session_lifecycle,
        drain_state,
        background_tasks: vec![
            own_http_background_task("agent_liveness", agent_liveness_task),
            own_http_background_task("escalation_worker", escalation_worker),
            own_http_background_task("stale_session_cleanup", stale_cleanup_task),
        ],
    })
}

fn streamable_service(
    shutdown_cancel: &CancellationToken,
    service: SynapseService,
) -> anyhow::Result<(McpHttpService, Arc<LocalSessionManager>)> {
    let session_config = session::load_session_config().context("load HTTP session config")?;
    {
        let session_registry = service.session_registry_handle();
        let mut registry = session_registry.lock().map_err(|_poisoned| {
            anyhow::anyhow!("session registry lock poisoned during HTTP session setup")
        })?;
        registry.set_stale_after(session_config.keep_alive);
    }
    let session_store = Arc::new(SynapseMcpSessionStore::new(
        session_store_db(&service)?,
        session_config.keep_alive,
        service.session_registry_handle(),
    ));
    let mut config = StreamableHttpServerConfig::default()
        .with_cancellation_token(shutdown_cancel.child_token());
    config.session_store = Some(session_store);
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config = session_config;
    let session_manager = Arc::new(session_manager);
    let service = StreamableHttpService::new(
        move || Ok(service.clone()),
        Arc::clone(&session_manager),
        config,
    );
    Ok((service, session_manager))
}

fn spawn_stale_session_input_cleanup(
    session_manager: Arc<LocalSessionManager>,
    session_lifecycle: crate::server::session_lifecycle::SessionLifecycleState,
    shutdown_cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(STALE_SESSION_INPUT_CLEANUP_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown_cancel.cancelled() => {
                    tracing::debug!(
                        code = "MCP_HTTP_SESSION_INPUT_STALE_CLEANUP_STOPPED",
                        "stopping stale HTTP session held-input cleanup"
                    );
                    break;
                }
                _ = interval.tick() => {
                    cleanup_stale_session_resources_once(
                        &session_lifecycle,
                        &session_manager,
                    ).await;
                }
            }
        }
    })
}

/// #898 liveness sweep: periodically cross-checks heartbeat silence with the
/// process table so stuck and dead agents surface within one sweep interval.
fn spawn_agent_liveness_sweep(
    agent_events_db: Arc<Db>,
    config: crate::server::agent_state::LivenessConfig,
    shutdown_cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(config.sweep_interval_ms));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        tracing::info!(
            code = "AGENT_LIVENESS_SWEEP_STARTED",
            sweep_interval_ms = config.sweep_interval_ms,
            stuck_after_ms = config.stuck_after_ms,
            runaway_identical_calls = config.runaway_identical_calls,
            "agent liveness sweep running"
        );
        loop {
            tokio::select! {
                _ = shutdown_cancel.cancelled() => {
                    tracing::debug!(
                        code = "AGENT_LIVENESS_SWEEP_STOPPED",
                        "stopping agent liveness sweep"
                    );
                    break;
                }
                _ = interval.tick() => {
                    let db = Arc::clone(&agent_events_db);
                    // `block_in_place` keeps blocking Win32 probes off this
                    // reactor worker without creating a nested JoinHandle that
                    // an aborted outer sweep could detach. Tokio cannot cancel
                    // the in-place closure; the exact outer owner remains
                    // nonterminal until the probe returns and is retained by
                    // the shutdown supervisor on deadline.
                    let transition_count = tokio::task::block_in_place(move || {
                        crate::server::agent_state::liveness_sweep_once(
                            &db,
                            crate::server::session_registry::unix_time_ms_now(),
                        )
                    });
                    if transition_count > 0 {
                        tracing::info!(
                            code = "AGENT_LIVENESS_SWEEP_TRANSITIONS",
                            transition_count,
                            "liveness sweep emitted state transitions"
                        );
                    }
                }
            }
        }
    })
}

async fn cleanup_stale_session_resources_once(
    session_lifecycle: &crate::server::session_lifecycle::SessionLifecycleState,
    session_manager: &LocalSessionManager,
) {
    let active_sessions = active_http_session_ids(session_manager).await;
    session_lifecycle.cleanup_expired_lease_inputs_once().await;
    let stale_sessions = session_lifecycle.stale_session_candidates(&active_sessions);
    for (session_id, reason) in stale_sessions {
        match session_lifecycle
            .teardown_session(&session_id, reason)
            .await
        {
            Ok(report) => {
                tracing::info!(
                    code = "MCP_HTTP_SESSION_STALE_LIFECYCLE_CLEANUP",
                    session_id = %session_id,
                    reason,
                    active_session_count = active_sessions.len(),
                    report = ?report,
                    "readback=session_lifecycle edge=http_session_gone after_cleanup"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    session_id = %session_id,
                    reason,
                    active_session_count = active_sessions.len(),
                    detail = %error.message,
                    data = ?error.data,
                    "HTTP MCP stale-session lifecycle cleanup failed"
                );
            }
        }
    }
}

#[cfg(test)]
async fn cleanup_stale_session_inputs_once(
    action_handle: &ActionHandle,
    session_manager: &LocalSessionManager,
    cdp_target_owners: &crate::server::SharedCdpTargetOwners,
) {
    let active_sessions = active_http_session_ids(session_manager).await;
    cleanup_expired_lease_inputs_once(action_handle).await;
    cleanup_stale_session_cdp_targets_once(cdp_target_owners, &active_sessions).await;

    let snapshot = match action_handle.session_inputs_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(
                code = error.code(),
                detail = %error.detail(),
                "HTTP MCP stale-session cleanup could not read held-input ownership"
            );
            return;
        }
    };
    for session in snapshot.sessions {
        if active_sessions.contains(&session.session_id) {
            continue;
        }
        release_stale_session_inputs_and_lease(action_handle, &session.session_id).await;
    }

    cleanup_stale_session_lease_once(action_handle, &active_sessions).await;
}

#[cfg(test)]
async fn cleanup_stale_session_cdp_targets_once(
    cdp_target_owners: &crate::server::SharedCdpTargetOwners,
    active_sessions: &BTreeSet<String>,
) {
    let stale_sessions = match cdp_target_owners.lock() {
        Ok(owners) => owners
            .values()
            .filter_map(|owner| {
                (!active_sessions.contains(&owner.session_id)).then(|| owner.session_id.clone())
            })
            .collect::<BTreeSet<_>>(),
        Err(_error) => {
            tracing::error!(
                code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "HTTP MCP stale-session cleanup could not lock CDP target ownership registry"
            );
            return;
        }
    };
    for session_id in stale_sessions {
        let (owned_before, target_ids) = match cdp_target_owners.lock() {
            Ok(mut owners) => {
                let stale_owner_keys = owners
                    .iter()
                    .filter_map(|(owner_key, owner)| {
                        (owner.session_id == session_id).then(|| owner_key.clone())
                    })
                    .collect::<Vec<_>>();
                let target_ids = stale_owner_keys
                    .iter()
                    .filter_map(|owner_key| {
                        owners
                            .get(owner_key)
                            .map(|owner| owner.cdp_target_id.clone())
                    })
                    .collect::<Vec<_>>();
                for owner_key in &stale_owner_keys {
                    owners.remove(owner_key);
                }
                (target_ids.len(), target_ids)
            }
            Err(_error) => {
                tracing::error!(
                    code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                    "HTTP MCP test stale-session cleanup could not lock CDP target ownership registry"
                );
                continue;
            }
        };
        tracing::info!(
            code = "MCP_HTTP_SESSION_CDP_TARGET_STALE_CLEANUP",
            session_id = %session_id,
            active_session_count = active_sessions.len(),
            cdp_cleanup_reason = "http_stale",
            cdp_owned_before = owned_before,
            cdp_closed = 0,
            cdp_failed = 0,
            cdp_target_ids = ?target_ids,
            "readback=cdp_target_ownership edge=http_session_gone after_cleanup"
        );
    }
}

async fn active_http_session_ids(session_manager: &LocalSessionManager) -> BTreeSet<String> {
    let sessions: BTreeSet<String> = session_manager
        .sessions
        .read()
        .await
        .keys()
        .map(|session_id| session_id.as_ref().to_owned())
        .collect();
    emit_http_active_sessions(sessions.len());
    sessions
}

async fn close_active_mcp_sessions_for_shutdown(
    session_manager: &LocalSessionManager,
    reason: &'static str,
) -> McpSessionShutdownCloseReport {
    let (sessions_before, sessions) = {
        let mut guard = session_manager.sessions.write().await;
        let sessions_before = guard.len();
        let sessions = guard
            .drain()
            .map(|(session_id, handle)| (session_id.as_ref().to_owned(), handle))
            .collect::<Vec<_>>();
        (sessions_before, sessions)
    };
    emit_http_active_sessions(0);
    let close_attempted = sessions.len();
    let session_ids = sessions
        .iter()
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();
    // A single wedged session must not suppress close attempts for every other
    // session. Start every independent close concurrently and give each one its
    // own deadline. rmcp's close only queues `SessionEvent::Close`; it does not
    // join the private session worker/service task. Consequently this report is
    // only a trigger/readback for the manager. The final accepted-socket,
    // server-task, and M3 Arc strong-count readbacks independently prove that no
    // per-session transport, service clone, or storage owner survived.
    let close_results = join_all(sessions.into_iter().map(|(session_id, handle)| async move {
        (
            session_id,
            time::timeout(MCP_SESSION_CLOSE_TIMEOUT, handle.close()).await,
        )
    }))
    .await;
    let mut close_succeeded = 0;
    let mut already_terminated = 0;
    let mut failures = Vec::new();
    for (session_id, result) in close_results {
        match result {
            Ok(Ok(())) => close_succeeded += 1,
            Ok(Err(SessionError::SessionServiceTerminated)) => already_terminated += 1,
            Ok(Err(error)) => failures.push(format!("{session_id}: close_failed: {error}")),
            Err(_elapsed) => failures.push(format!(
                "{session_id}: close_timeout_ms={}",
                MCP_SESSION_CLOSE_TIMEOUT.as_millis()
            )),
        }
    }
    // Read the session-manager Source of Truth independently after every
    // close attempt. `handle.close()` is a trigger result, not proof that no
    // live session remains registered.
    let session_ids_after = active_http_session_ids(session_manager)
        .await
        .into_iter()
        .collect::<Vec<_>>();
    McpSessionShutdownCloseReport {
        reason,
        sessions_before,
        close_attempted,
        close_succeeded,
        already_terminated,
        failure_count: failures.len(),
        session_ids,
        failures,
        sessions_after: session_ids_after.len(),
        session_ids_after,
    }
}

async fn cleanup_active_session_inputs_for_shutdown(
    session_lifecycle: &crate::server::session_lifecycle::SessionLifecycleState,
    session_manager: &LocalSessionManager,
    close_candidate_session_ids: &[String],
    reason: &'static str,
) -> DaemonShutdownInputCleanupReport {
    // Cancellation/unwind finalizers own durable profile/lease/audit cleanup.
    // Drain them before lifecycle teardown reads or deletes the same session
    // Sources of Truth.
    let (authority_finalizer_drain_readback, authority_finalizer_drain_error) =
        match session_lifecycle.drain_authority_finalizers().await {
            Ok(readback) => (Some(readback), None),
            Err(error) => (Some(error.readback.clone()), Some(error.to_string())),
        };
    let active_sessions = active_http_session_ids(session_manager).await;
    let (live_spawn_sessions, live_spawn_snapshot_error) =
        match session_lifecycle.live_spawned_session_ids_for_shutdown() {
            Ok(session_ids) => (session_ids, None),
            Err(error) => (BTreeSet::new(), Some(error)),
        };
    let live_spawn_snapshot_read_before = live_spawn_snapshot_error.is_none();
    let close_candidate_sessions = close_candidate_session_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut input_owner_snapshot_errors = Vec::new();
    let input_owner_sessions_before = match session_lifecycle.input_owner_session_ids_for_shutdown()
    {
        Ok(session_ids) => session_ids
            .into_iter()
            .filter(|session_id| session_id != synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID)
            .collect::<BTreeSet<_>>(),
        Err(error) => {
            input_owner_snapshot_errors.push(format!("before_cleanup: {error}"));
            BTreeSet::new()
        }
    };
    let input_owner_snapshot_read_before = input_owner_snapshot_errors.is_empty();
    let shutdown_sessions = shutdown_cleanup_session_ids(
        &active_sessions,
        &live_spawn_sessions,
        &close_candidate_sessions,
        &input_owner_sessions_before,
    );
    let shutdown_session_ids = shutdown_sessions.iter().cloned().collect::<Vec<_>>();
    // The bounded authority-finalizer drain above can deliberately return with
    // a retained cooperative owner. Never follow that bounded verdict with an
    // unbounded reacquisition of the same session gate or an unbounded emitter
    // acknowledgement. Start every independent cleanup so one retained owner
    // cannot suppress the remaining attempts, and give each exact operation its
    // own terminal deadline.
    let cleanup_results = join_all(shutdown_sessions.iter().map(|session_id| {
        await_daemon_session_input_cleanup(
            session_id,
            reason,
            session_lifecycle.release_session_inputs_for_daemon_shutdown(session_id, reason),
        )
    }))
    .await;
    let mut session_cleanup_timeouts = Vec::new();
    let session_reports = cleanup_results
        .into_iter()
        .map(|(report, timed_out)| {
            if timed_out {
                session_cleanup_timeouts.push(report.session_id.clone());
            }
            report
        })
        .collect::<Vec<_>>();
    let mut orphan_lease_owner_cleanup = None;
    let mut final_lease = synapse_action::lease::status();
    if let Some(owner_session_id) = final_lease.owner_session_id.clone()
        && owner_session_id != synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID
        && !shutdown_sessions.contains(&owner_session_id)
    {
        let (report, timed_out) = await_daemon_session_input_cleanup(
            &owner_session_id,
            reason,
            session_lifecycle.release_session_inputs_for_daemon_shutdown(&owner_session_id, reason),
        )
        .await;
        if timed_out {
            session_cleanup_timeouts.push(owner_session_id);
        }
        orphan_lease_owner_cleanup = Some(report);
        final_lease = synapse_action::lease::status();
    }
    let final_lease_is_operator = final_lease.owner_session_id.as_deref()
        == Some(synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID);
    let lease_still_held_after_cleanup = final_lease.held && !final_lease_is_operator;
    let input_owner_sessions_after = match session_lifecycle.input_owner_session_ids_for_shutdown()
    {
        Ok(session_ids) => session_ids
            .into_iter()
            .filter(|session_id| session_id != synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID)
            .collect::<BTreeSet<_>>(),
        Err(error) => {
            input_owner_snapshot_errors.push(format!("after_cleanup: {error}"));
            BTreeSet::new()
        }
    };
    let input_owner_snapshot_read_after = !input_owner_snapshot_errors
        .iter()
        .any(|error| error.starts_with("after_cleanup:"));
    let mut failure_count = session_reports
        .iter()
        .filter(|report| report.failed)
        .count();
    if orphan_lease_owner_cleanup
        .as_ref()
        .is_some_and(|report| report.failed)
    {
        failure_count += 1;
    }
    if lease_still_held_after_cleanup {
        failure_count += 1;
    }
    failure_count += input_owner_snapshot_errors.len();
    failure_count += input_owner_sessions_after.len();
    failure_count += usize::from(authority_finalizer_drain_error.is_some());
    failure_count += usize::from(live_spawn_snapshot_error.is_some());
    DaemonShutdownInputCleanupReport {
        reason,
        authority_finalizer_drain_readback,
        authority_finalizer_drain_error,
        active_sessions_before: active_sessions.len(),
        live_spawn_snapshot_read_before,
        live_spawn_snapshot_error,
        live_spawn_sessions_before: live_spawn_sessions.len(),
        close_candidate_sessions_before: close_candidate_sessions.len(),
        input_owner_snapshot_read_before,
        input_owner_session_ids_before: input_owner_sessions_before.into_iter().collect(),
        shutdown_sessions_before: shutdown_sessions.len(),
        shutdown_session_ids,
        cleaned_sessions: session_reports.len(),
        session_cleanup_timeout_ms: u64::try_from(DAEMON_SESSION_INPUT_CLEANUP_TIMEOUT.as_millis())
            .unwrap_or(u64::MAX),
        session_cleanup_timeouts,
        orphan_lease_owner_cleanup,
        final_lease_held: final_lease.held,
        final_lease_owner_session_id: final_lease.owner_session_id,
        final_lease_is_operator,
        lease_still_held_after_cleanup,
        input_owner_snapshot_read_after,
        input_owner_session_ids_after: input_owner_sessions_after.into_iter().collect(),
        input_owner_snapshot_errors,
        failure_count,
        session_reports,
    }
}

async fn await_daemon_session_input_cleanup<F>(
    session_id: &str,
    reason: &str,
    cleanup: F,
) -> (
    crate::server::session_lifecycle::SessionShutdownInputCleanupReport,
    bool,
)
where
    F: std::future::Future<
            Output = crate::server::session_lifecycle::SessionShutdownInputCleanupReport,
        >,
{
    match time::timeout(DAEMON_SESSION_INPUT_CLEANUP_TIMEOUT, cleanup).await {
        Ok(report) => (report, false),
        Err(_elapsed) => (
            crate::server::session_lifecycle::SessionShutdownInputCleanupReport {
                session_id: session_id.to_owned(),
                reason: reason.to_owned(),
                failed: true,
                error_message: Some(format!(
                    "daemon-shutdown session authority/input cleanup exceeded its {} ms terminal deadline; authority or emitter completion remains unproven",
                    DAEMON_SESSION_INPUT_CLEANUP_TIMEOUT.as_millis()
                )),
                ..Default::default()
            },
            true,
        ),
    }
}

fn shutdown_cleanup_session_ids(
    active_sessions: &BTreeSet<String>,
    live_spawn_sessions: &BTreeSet<String>,
    close_candidate_sessions: &BTreeSet<String>,
    input_owner_sessions: &BTreeSet<String>,
) -> BTreeSet<String> {
    active_sessions
        .iter()
        .cloned()
        .chain(live_spawn_sessions.iter().cloned())
        .chain(close_candidate_sessions.iter().cloned())
        .chain(input_owner_sessions.iter().cloned())
        .collect()
}

#[cfg(test)]
async fn cleanup_expired_lease_inputs_once(action_handle: &ActionHandle) {
    let _lease_status_readback = synapse_action::lease::status();
    let pending = synapse_action::lease::expired_cleanup_snapshot();
    for expired in pending {
        let Some(session_id) = expired.owner_session_id.clone() else {
            continue;
        };
        release_expired_session_inputs_and_lease(action_handle, &session_id, &expired).await;
    }
}

#[cfg(test)]
async fn cleanup_stale_session_lease_once(
    action_handle: &ActionHandle,
    active_sessions: &BTreeSet<String>,
) {
    let status = synapse_action::lease::status();
    let Some(owner_session_id) = status.owner_session_id.clone() else {
        return;
    };
    if owner_session_id == synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID {
        return;
    }
    if active_sessions.contains(&owner_session_id) {
        return;
    }
    let before_ownership = action_handle.session_inputs_snapshot();
    let result = action_handle
        .release_session_inputs_and_lease(&owner_session_id)
        .await;
    let after_ownership = action_handle.session_inputs_snapshot();
    let after_lease = synapse_action::lease::status();
    match result {
        Ok(summary) => {
            tracing::info!(
                code = "MCP_HTTP_SESSION_LEASE_STALE_CLEANUP",
                session_id = %owner_session_id,
                input_lease_released = summary.lease_released,
                expired_lease_cleanup_completed = summary.expired_lease_cleanup_completed,
                before_lease = ?status,
                after_lease = ?after_lease,
                before_ownership = ?before_ownership,
                after_ownership = ?after_ownership,
                active_session_count = active_sessions.len(),
                "readback=input_lease edge=http_session_gone after_cleanup"
            );
        }
        Err(error) => {
            tracing::error!(
                code = error.code(),
                session_id = %owner_session_id,
                detail = %error.detail(),
                before_lease = ?status,
                after_lease = ?after_lease,
                before_ownership = ?before_ownership,
                after_ownership = ?after_ownership,
                active_session_count = active_sessions.len(),
                "HTTP MCP stale-session lease cleanup failed"
            );
        }
    }
}

#[cfg(test)]
async fn release_stale_session_inputs_and_lease(action_handle: &ActionHandle, session_id: &str) {
    let before = action_handle.session_inputs_snapshot();
    let before_lease = synapse_action::lease::status();
    let result = action_handle
        .release_session_inputs_and_lease(session_id)
        .await;
    let after = action_handle.session_inputs_snapshot();
    let after_lease = synapse_action::lease::status();
    match result {
        Ok(summary) => {
            tracing::info!(
                code = "MCP_HTTP_SESSION_INPUT_STALE_CLEANUP",
                session_id,
                released_keys = summary.input_summary.released_keys,
                released_buttons = summary.input_summary.released_buttons,
                neutralized_pads = summary.input_summary.neutralized_pads,
                retained_shared_inputs = summary.input_summary.retained_shared_inputs,
                input_lease_released = summary.lease_released,
                expired_lease_cleanup_completed = summary.expired_lease_cleanup_completed,
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "readback=session_input_ownership edge=http_session_gone after_cleanup"
            );
        }
        Err(error) => {
            tracing::error!(
                code = error.code(),
                session_id,
                detail = %error.detail(),
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "HTTP MCP stale-session cleanup failed while releasing owned inputs"
            );
        }
    }
}

#[cfg(test)]
async fn release_expired_session_inputs_and_lease(
    action_handle: &ActionHandle,
    session_id: &str,
    expired: &synapse_action::LeaseStatus,
) {
    let before = action_handle.session_inputs_snapshot();
    let before_lease = synapse_action::lease::status();
    let result = action_handle
        .release_session_inputs_and_lease(session_id)
        .await;
    let after = action_handle.session_inputs_snapshot();
    let after_lease = synapse_action::lease::status();
    match result {
        Ok(summary) => {
            tracing::warn!(
                code = "MCP_HTTP_SESSION_LEASE_EXPIRED_INPUT_CLEANUP",
                session_id,
                released_keys = summary.input_summary.released_keys,
                released_buttons = summary.input_summary.released_buttons,
                neutralized_pads = summary.input_summary.neutralized_pads,
                retained_shared_inputs = summary.input_summary.retained_shared_inputs,
                input_lease_released = summary.lease_released,
                expired_lease_cleanup_completed = summary.expired_lease_cleanup_completed,
                expired = ?expired,
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "readback=session_input_ownership edge=input_lease_expired after_cleanup"
            );
        }
        Err(error) => {
            tracing::error!(
                code = error.code(),
                session_id,
                detail = %error.detail(),
                expired = ?expired,
                before = ?before,
                after = ?after,
                before_lease = ?before_lease,
                after_lease = ?after_lease,
                "HTTP MCP expired-lease cleanup failed while releasing owned inputs"
            );
        }
    }
}

fn session_store_db(service: &SynapseService) -> anyhow::Result<Arc<Db>> {
    let m3_handle = service.m3_state_handle();
    let mut state = m3_handle.lock().map_err(|_poisoned| {
        anyhow::anyhow!("m3 service state lock poisoned during session-store setup")
    })?;
    state
        .ensure_storage()
        .context("open storage for HTTP MCP session store")
}

#[derive(Clone)]
struct SynapseMcpSessionStore {
    db: Arc<Db>,
    ttl: Option<Duration>,
    session_registry: crate::server::session_registry::SharedSessionRegistry,
}

impl SynapseMcpSessionStore {
    fn new(
        db: Arc<Db>,
        ttl: Option<Duration>,
        session_registry: crate::server::session_registry::SharedSessionRegistry,
    ) -> Self {
        Self {
            db,
            ttl,
            session_registry,
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PersistedMcpSessionState {
    stored_at_unix_ms: u64,
    state: SessionState,
}

#[async_trait::async_trait]
impl SessionStore for SynapseMcpSessionStore {
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        let key = mcp_session_store_key(session_id);
        let rows = self
            .db
            .scan_cf_prefix(cf::CF_KV, &key)
            .map_err(session_store_error)?;
        let Some((_key, value)) = rows.into_iter().find(|(row_key, _value)| row_key == &key) else {
            return Ok(None);
        };
        let now_ms = unix_time_ms()?;
        let persisted = match synapse_storage::decode_json::<PersistedMcpSessionState>(&value) {
            Ok(persisted) => persisted,
            Err(wrapper_error) => {
                if synapse_storage::decode_json::<SessionState>(&value).is_ok() {
                    self.db
                        .delete_batch(cf::CF_KV, [key])
                        .map_err(session_store_error)?;
                    delete_session_continuity_rows(&self.db, session_id)?;
                    tracing::warn!(
                        code = "MCP_HTTP_SESSION_STORE_LEGACY_STALE_DELETE",
                        session_id,
                        detail = %wrapper_error,
                        "deleted legacy MCP HTTP session state without persistent TTL metadata"
                    );
                    return Ok(None);
                }
                tracing::error!(
                    code = "MCP_HTTP_SESSION_STORE_DECODE_FAILED",
                    session_id,
                    detail = %wrapper_error,
                    "failed to decode persisted MCP HTTP session state"
                );
                return Err(session_store_error(wrapper_error));
            }
        };
        if session_store_expired(persisted.stored_at_unix_ms, now_ms, self.ttl) {
            self.db
                .delete_batch(cf::CF_KV, [key])
                .map_err(session_store_error)?;
            delete_session_continuity_rows(&self.db, session_id)?;
            tracing::warn!(
                code = "MCP_HTTP_SESSION_STORE_EXPIRED",
                session_id,
                stored_at_unix_ms = persisted.stored_at_unix_ms,
                now_unix_ms = now_ms,
                ttl_ms = self.ttl.map(duration_millis_u64),
                "deleted expired MCP HTTP session state from CF_KV"
            );
            return Ok(None);
        }
        tracing::info!(
            code = "MCP_HTTP_SESSION_STORE_LOAD",
            session_id,
            stored_at_unix_ms = persisted.stored_at_unix_ms,
            "loaded MCP HTTP session state from CF_KV"
        );
        let newly_visible = record_registry_initialized(
            &self.session_registry,
            session_id,
            &persisted.state,
            persisted.stored_at_unix_ms,
        )
        .map_err(session_store_error)?;
        if newly_visible {
            journal_session_live_event(&self.db, session_id, &persisted.state, "session_restored")
                .map_err(session_store_error)?;
        }
        Ok(Some(persisted.state))
    }

    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        let key = mcp_session_store_key(session_id);
        let stored_at_unix_ms = unix_time_ms()?;
        let persisted = PersistedMcpSessionState {
            stored_at_unix_ms,
            state: state.clone(),
        };
        let encoded = synapse_storage::encode_json(&persisted).map_err(session_store_error)?;
        self.db
            .put_batch_pressure_bypass(cf::CF_KV, [(key, encoded)])
            .map_err(session_store_error)?;
        tracing::info!(
            code = "MCP_HTTP_SESSION_STORE_WRITE",
            session_id,
            stored_at_unix_ms,
            ttl_ms = self.ttl.map(duration_millis_u64),
            "persisted MCP HTTP session state to CF_KV"
        );
        let newly_visible = record_registry_initialized(
            &self.session_registry,
            session_id,
            state,
            stored_at_unix_ms,
        )
        .map_err(session_store_error)?;
        if newly_visible {
            journal_session_live_event(&self.db, session_id, state, "session_initialized")
                .map_err(session_store_error)?;
        }
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let key = mcp_session_store_key(session_id);
        self.db
            .delete_batch(cf::CF_KV, [key])
            .map_err(session_store_error)?;
        delete_session_continuity_rows(&self.db, session_id)?;
        tracing::info!(
            code = "MCP_HTTP_SESSION_STORE_DELETE",
            session_id,
            "deleted MCP HTTP session state from CF_KV"
        );
        let transitioned = record_registry_closed(&self.session_registry, session_id)
            .map_err(session_store_error)?;
        if transitioned {
            journal_session_exited_event(&self.db, session_id, "http_session_store_deleted")
                .map_err(session_store_error)?;
        }
        Ok(())
    }
}

fn record_registry_initialized(
    session_registry: &crate::server::session_registry::SharedSessionRegistry,
    session_id: &str,
    state: &SessionState,
    now_unix_ms: u64,
) -> Result<bool, synapse_storage::StorageError> {
    let mut registry =
        session_registry
            .lock()
            .map_err(|_error| synapse_storage::StorageError::WriteFailed {
                cf_name: cf::CF_KV.to_owned(),
                detail: "session registry lock poisoned during session store".to_owned(),
            })?;
    Ok(registry.record_initialized(session_id, state, "http", now_unix_ms))
}

fn record_registry_closed(
    session_registry: &crate::server::session_registry::SharedSessionRegistry,
    session_id: &str,
) -> Result<bool, synapse_storage::StorageError> {
    let mut registry =
        session_registry
            .lock()
            .map_err(|_error| synapse_storage::StorageError::WriteFailed {
                cf_name: cf::CF_KV.to_owned(),
                detail: "session registry lock poisoned during session delete".to_owned(),
            })?;
    Ok(registry.record_closed(
        session_id,
        crate::server::session_registry::unix_time_ms_now(),
    ))
}

/// Journals a `state_changed → live` agent event for a session that just
/// became visible through the HTTP session store (#897).
fn journal_session_live_event(
    db: &Db,
    session_id: &str,
    state: &SessionState,
    reason_code: &str,
) -> Result<(), synapse_storage::StorageError> {
    use crate::server::{agent_events, session_registry};
    let client_name = state.initialize_params.client_info.name.clone();
    let agent_kind = session_registry::infer_agent_kind(&client_name);
    let mut record = synapse_core::AgentEventRecord::new(
        agent_events::unix_time_ns_now(),
        synapse_core::AgentEventKind::StateChanged,
    );
    record.session_id = Some(session_id.to_owned());
    record.reason_code = Some(reason_code.to_owned());
    record.state_to = Some("live".to_owned());
    record.attributes.operation_name = Some(synapse_core::GenAiOperationName::InvokeAgent);
    record.attributes.conversation_id = Some(session_id.to_owned());
    record.attributes.agent_name = Some(client_name);
    record.attributes.provider_name = agent_events::provider_for_agent_kind(&agent_kind);
    agent_events::record_agent_event(db, &record).map(|_readback| ())
}

/// Journals a terminal `exited` agent event (durable flush) for a session
/// the HTTP session store just deleted (#897). The outcome of the agent's
/// work is unknown at this layer, so the end state is `indeterminate`.
fn journal_session_exited_event(
    db: &Db,
    session_id: &str,
    reason_code: &str,
) -> Result<(), synapse_storage::StorageError> {
    use crate::server::agent_events;
    let mut record = synapse_core::AgentEventRecord::new(
        agent_events::unix_time_ns_now(),
        synapse_core::AgentEventKind::Exited,
    );
    record.session_id = Some(session_id.to_owned());
    record.reason_code = Some(reason_code.to_owned());
    record.end_state = Some(synapse_core::AgentEndState::Indeterminate);
    record.attributes.conversation_id = Some(session_id.to_owned());
    agent_events::record_agent_event_durable(db, &record).map(|_readback| ())
}

fn mcp_session_store_key(session_id: &str) -> Vec<u8> {
    crate::server::session_lifecycle::mcp_session_store_key(session_id)
}

fn delete_session_continuity_rows(db: &Db, session_id: &str) -> Result<(), SessionStoreError> {
    let readback =
        crate::server::session_continuity::delete_persisted_session_continuity_rows_from_db(
            db, session_id,
        )
        .map_err(|error| -> SessionStoreError { Box::new(io::Error::other(error)) })?;
    tracing::info!(
        code = "MCP_HTTP_SESSION_CONTINUITY_DELETE",
        session_id,
        readback = ?readback,
        "readback=CF_SESSIONS after=http_session_store_deleted_continuity"
    );
    Ok(())
}

fn session_store_error(error: synapse_storage::StorageError) -> SessionStoreError {
    Box::new(error)
}

fn unix_time_ms() -> Result<u64, SessionStoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| -> SessionStoreError { Box::new(error) })?;
    Ok(duration_millis_u64(elapsed))
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn session_store_expired(stored_at_unix_ms: u64, now_unix_ms: u64, ttl: Option<Duration>) -> bool {
    let Some(ttl) = ttl else {
        return false;
    };
    now_unix_ms.saturating_sub(stored_at_unix_ms) > duration_millis_u64(ttl)
}

fn http_service(
    shutdown_cancel: CancellationToken,
    connection_closed_cancel: CancellationToken,
    sse_state: SseState,
    m2_config: &M2ServiceConfig,
    m3_config: M3ServiceConfig,
    m4_config: M4ServiceConfig,
) -> io::Result<SynapseService> {
    SynapseService::try_with_m2_shutdown_reason_and_sse_state_and_m3_config(
        shutdown_cancel,
        "http",
        connection_closed_cancel,
        sse_state,
        m2_config,
        m3_config,
        m4_config,
    )
    .map_err(|error| io::Error::other(format!("{error:#}")))
}

#[derive(Serialize)]
struct DashboardStateResponse {
    schema_version: u32,
    generated_at_unix_ms: u64,
    bind_addr: String,
    token_policy: &'static str,
    timings: DashboardStateTimings,
    dashboard_assets: DashboardPanel,
    auth: DashboardPanel,
    daemon: DashboardPanel,
    sessions: DashboardPanel,
    lease: DashboardPanel,
    storage: DashboardPanel,
    target_claims: DashboardPanel,
    timeline: DashboardPanel,
    demo_recording: DashboardPanel,
    events: DashboardPanel,
    hidden_desktops: DashboardPanel,
    cdp_attachments: DashboardPanel,
    shell_jobs: DashboardPanel,
    command_audit: DashboardPanel,
    tasks: DashboardPanel,
    approvals: DashboardPanel,
    suggestions: DashboardPanel,
    armed_runs: DashboardPanel,
    agent_transcripts: DashboardPanel,
    agent_cost: DashboardPanel,
    agent_stats: DashboardPanel,
    context: DashboardPanel,
    hygiene: DashboardPanel,
    local_models: DashboardPanel,
}

#[derive(Serialize)]
struct DashboardTrayStateResponse {
    schema_version: u32,
    generated_at_unix_ms: u64,
    bind_addr: String,
    token_policy: &'static str,
    source_of_truth: &'static str,
    timings: DashboardStateTimings,
    sessions: DashboardPanel,
    lease: DashboardPanel,
    timeline: DashboardPanel,
    approvals: DashboardPanel,
    demo_recording: DashboardPanel,
}

#[derive(Serialize)]
struct DashboardStateTimings {
    source_of_truth: &'static str,
    total_elapsed_ms: u64,
    segments: Vec<DashboardStateTiming>,
}

#[derive(Serialize)]
struct DashboardStateTiming {
    segment: &'static str,
    elapsed_ms: u64,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DashboardEventScope {
    Fleet,
    Agent,
    Tasks,
    System,
    Audit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardEventSubscribeRequest {
    scope: DashboardEventScope,
    #[serde(default)]
    snapshot_first: bool,
}

#[derive(Serialize)]
struct DashboardEventSubscribeResponse {
    ok: bool,
    source_of_truth: &'static str,
    scope: DashboardEventScope,
    subscription_id: String,
    event_url: String,
    replay_contract: &'static str,
}

#[derive(Serialize)]
struct DashboardPanel {
    status: &'static str,
    source: &'static str,
    data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DashboardAssetSurface {
    schema_version: u32,
    source_of_truth: &'static str,
    js_file: &'static str,
    css_file: &'static str,
}

impl DashboardPanel {
    fn ok(source: &'static str, data: impl Serialize) -> Self {
        Self {
            status: "ok",
            source,
            data: serde_json::to_value(data).unwrap_or_else(|error| {
                serde_json::json!({
                    "serialization_error": error.to_string(),
                })
            }),
            error: None,
        }
    }

    fn unavailable(source: &'static str, data: impl Serialize) -> Self {
        Self {
            status: "unavailable",
            source,
            data: serde_json::to_value(data).unwrap_or_else(|error| {
                serde_json::json!({
                    "serialization_error": error.to_string(),
                })
            }),
            error: None,
        }
    }

    fn error(source: &'static str, error: impl ToString) -> Self {
        Self {
            status: "error",
            source,
            data: serde_json::json!({}),
            error: Some(error.to_string()),
        }
    }
}

fn dashboard_event_subscription(
    scope: DashboardEventScope,
) -> (EventFilter, Vec<String>, &'static str) {
    let approval_kinds = || {
        vec![
            crate::server::APPROVAL_REQUEST_EVENT_KIND.to_owned(),
            crate::server::APPROVAL_DECISION_EVENT_KIND.to_owned(),
            crate::server::APPROVAL_TIMEOUT_EVENT_KIND.to_owned(),
        ]
    };
    let fleet_attention_kinds = || {
        let mut kinds = vec![
            crate::server::agent_state::AGENT_STATE_EVENT_KIND.to_owned(),
            "workspace.put".to_owned(),
        ];
        kinds.extend(approval_kinds());
        kinds
    };
    match scope {
        DashboardEventScope::Fleet => (
            EventFilter::All,
            fleet_attention_kinds(),
            "agent state + workspace + approval events",
        ),
        DashboardEventScope::Agent => (
            EventFilter::All,
            vec![
                crate::server::agent_state::AGENT_STATE_EVENT_KIND.to_owned(),
                "profile-changed".to_owned(),
                "scope-transitioned".to_owned(),
                "workspace.put".to_owned(),
            ],
            "agent state + profile/workspace events",
        ),
        DashboardEventScope::Tasks => (
            EventFilter::All,
            fleet_attention_kinds(),
            "task/attention/approval state events",
        ),
        DashboardEventScope::System => (
            EventFilter::Or {
                args: vec![
                    EventFilter::Source {
                        source: EventSource::System,
                    },
                    EventFilter::Source {
                        source: EventSource::Filesystem,
                    },
                    EventFilter::Source {
                        source: EventSource::Process,
                    },
                    EventFilter::Source {
                        source: EventSource::Clipboard,
                    },
                    EventFilter::Source {
                        source: EventSource::PerceptionAudio,
                    },
                ],
            },
            Vec::new(),
            "system/process/filesystem/audio events",
        ),
        DashboardEventScope::Audit => (
            EventFilter::Or {
                args: vec![
                    EventFilter::Source {
                        source: EventSource::ActionEmitter,
                    },
                    EventFilter::Source {
                        source: EventSource::Reflex,
                    },
                    EventFilter::Source {
                        source: EventSource::System,
                    },
                ],
            },
            Vec::new(),
            "command/audit-relevant system events",
        ),
    }
}

fn dashboard_event_url(subscription_id: &str) -> String {
    format!("/dashboard/events?subscription_id={subscription_id}")
}

#[derive(Serialize)]
struct DashboardStorageSummary {
    schema_version: u32,
    pressure_level: crate::m3::storage::StoragePressureLevel,
    pressure_transition_codes: Vec<String>,
    metrics_mode: String,
    cf_sizes: BTreeMap<String, u64>,
    cf_row_counts: BTreeMap<String, u64>,
    audit_retention_policy_count: usize,
    missing_cf_size_estimates: Vec<String>,
    missing_cf_row_count_estimates: Vec<String>,
}

#[derive(Serialize)]
struct DashboardEventSurface {
    source_of_truth: &'static str,
    active_subscription_count: usize,
    owner_session_ids: Vec<String>,
    owner_read_error: Option<String>,
    agent_event_ingress: serde_json::Value,
    agent_transcript_ingest: serde_json::Value,
}

#[derive(Serialize)]
struct DashboardHiddenDesktopSurface {
    source_of_truth: &'static str,
    row_count: usize,
    rows: Vec<crate::server::session_lifecycle::SessionHiddenDesktopReadback>,
}

#[derive(Serialize)]
struct DashboardCdpAttachmentSurface {
    source_of_truth: &'static str,
    row_count: usize,
    rows: Vec<crate::server::session_lifecycle::SessionCdpTargetOwnerReadback>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTimelinePauseRequest {
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTimelineQueryRequest {
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    end_ts_ns: Option<String>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardEpisodeListRequest {
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    end_ts_ns: Option<String>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    min_duration_ms: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardEpisodeGetRequest {
    episode_id: String,
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    refs_limit: Option<u32>,
    #[serde(default)]
    refs_cursor: Option<String>,
}

/// Dashboard Approvals-inbox decision (#927). Resolves one pending approval —
/// including the `agent_permission` rows a blocked `approval_gate` call is
/// waiting on, so deciding here resumes the agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardApprovalDecideRequest {
    approval_id: String,
    /// "approve"/"accept", "deny"/"decline"/"reject", or "snooze".
    decision: String,
    #[serde(default)]
    note: Option<String>,
    /// Approve-with-edits (#1030): full-replacement tool input, JSON object as a
    /// string. Honored only with an approve decision on an `allow.edit` item.
    #[serde(default)]
    edited_args: Option<String>,
    /// Respond (#1030): the operator's answer to a needs-input / agent_question
    /// item. Honored only with an approve decision on an `allow.respond` item.
    #[serde(default)]
    response: Option<String>,
}

#[derive(Serialize)]
struct DashboardApprovalDecideResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    decision: crate::m3::approvals::ApprovalDecideResponse,
}

#[derive(Serialize)]
struct DashboardTimelineControlResponse<T>
where
    T: Serialize,
{
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    readback: T,
}

/// Dashboard storage-manager request to purge operator timeline recordings by
/// time period / kind / app / actor. Maps to the [`timeline_purge`] tool params
/// (`flag_ids`/`text` are intentionally not exposed here — flag-id deletes flow
/// through the hygiene surface, free-text purges through `timeline_search`).
/// `dry_run` previews the matched count without deleting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTimelinePurgeRequest {
    /// Epoch-nanosecond bound as a decimal string — ns values exceed JS
    /// `Number.MAX_SAFE_INTEGER`, so the client serializes them as strings to
    /// avoid silent precision loss.
    #[serde(default)]
    start_ts_ns: Option<String>,
    #[serde(default)]
    end_ts_ns: Option<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    apps: Option<Vec<String>>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    cursor: Option<String>,
}

/// Dashboard storage-manager request to trim a column family to its newest rows
/// (`soft_cap_rows` = keep-newest-N) or run the `AUDIT_RETENTION` age sweep.
/// Maps directly to the [`storage_gc_once`] tool params.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardStorageGcRequest {
    cf_name: String,
    soft_cap_rows: u64,
    hard_cap_rows: u64,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    now_ns: Option<u64>,
    #[serde(default)]
    max_age_ns: Option<u64>,
    #[serde(default)]
    dedupe_window_ns: Option<u64>,
    #[serde(default)]
    profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardControlLeaseForceReleaseRequest {
    owner_session_id: String,
    confirmed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardControlLeaseHandoffRequest {
    from_session_id: String,
    to_session_id: String,
    #[serde(default)]
    ttl_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardControlResponse<T>
where
    T: Serialize,
{
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    readback: T,
}

#[derive(Serialize)]
struct DashboardDeferredSurface {
    tool: &'static str,
    available: bool,
    rows: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct DashboardApprovalSurface {
    tool: &'static str,
    available: bool,
    rows: Vec<crate::m3::approvals::ApprovalQueueItem>,
}

#[derive(Serialize)]
struct DashboardLocalModelSurface {
    tool: &'static str,
    available: bool,
    enabled_count: usize,
    unhealthy_count: usize,
    rows: Vec<crate::m3::local_models::LocalModelRegistryRow>,
}

#[derive(Serialize)]
struct DashboardTaskSurface {
    tool: &'static str,
    available: bool,
    source_of_truth: &'static str,
    row_count: usize,
    tasks: Vec<crate::server::agent_tasks::AgentTask>,
    reconciled_orphans: Vec<String>,
    next: crate::server::agent_tasks::TaskNextResponse,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardLocalModelSpawnRequest {
    model_ref: String,
    prompt: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    wait_timeout_ms: Option<u64>,
    #[serde(default)]
    hold_open_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardSpawnAgentRequest {
    #[serde(default)]
    fan_out: Option<u32>,
    #[serde(default)]
    template_id: Option<String>,
    #[serde(default)]
    template_version: Option<u32>,
    #[serde(default)]
    template_params: BTreeMap<String, String>,
    #[serde(default)]
    cli: Option<crate::m4::ActSpawnAgentCli>,
    #[serde(default)]
    kind: Option<crate::m4::ActSpawnAgentCli>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_ref: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    target: Option<crate::m4::ActSpawnAgentTarget>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    wait_timeout_ms: Option<u64>,
    #[serde(default)]
    hold_open_ms: Option<u64>,
    /// Route the spawned agent's risky tool calls through the Approvals inbox
    /// (#927). Defaults true; the dashboard may send false for trusted spawns.
    #[serde(default)]
    require_approval_gate: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardTaskDispatchOnceRequest {
    #[serde(default)]
    concurrency_cap: Option<usize>,
    #[serde(default)]
    wait_timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardLocalModelSpawnResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    spawn: crate::m4::ActSpawnAgentResponse,
}

#[derive(Serialize)]
struct DashboardSpawnAgentResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    requested_count: u32,
    succeeded_count: usize,
    failed_count: usize,
    attempts: Vec<DashboardSpawnAgentAttempt>,
}

#[derive(Serialize)]
struct DashboardSpawnAgentAttempt {
    index: u32,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    spawn: Option<crate::m4::ActSpawnAgentResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardAgentKillRequest {
    session_id: String,
    #[serde(default)]
    grace_ms: Option<u64>,
    #[serde(default)]
    interrupt_first: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardAgentBroadcastRequest {
    selector: String,
    #[serde(default)]
    agent_kinds: Vec<String>,
    #[serde(default)]
    sessions: Vec<String>,
    kind: String,
    payload: serde_json::Value,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    request_receipt: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardFleetStopRequest {
    mode: String,
    confirm: String,
    #[serde(default)]
    agent_kinds: Vec<String>,
    #[serde(default)]
    grace_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardAgentLookupRequest {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardAgentRespawnRequest {
    session_id: String,
    prompt: String,
    #[serde(default)]
    carry_context: Option<bool>,
    #[serde(default)]
    grace_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardAgentKillResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    kill: crate::server::agent_control::AgentKillResponse,
}

#[derive(Serialize)]
struct DashboardAgentBroadcastResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    broadcast: serde_json::Value,
}

#[derive(Serialize)]
struct DashboardFleetStopResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    fleet_stop: crate::server::agent_control::FleetStopResponse,
}

#[derive(Serialize)]
struct DashboardTemplateListResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    list: crate::server::agent_templates::AgentTemplateListResponse,
}

#[derive(Serialize)]
struct DashboardTemplateUpsertResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    put: crate::server::agent_templates::AgentTemplatePutResponse,
}

#[derive(Serialize)]
struct DashboardTemplateDeleteResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    delete: crate::server::agent_templates::AgentTemplateDeleteResponse,
}

/// Browser-facing request to register an OpenAI-compatible cloud API model
/// (DeepSeek first) into the local-model registry. `api_shape` is fixed to
/// `open_ai_chat_completions` and `allow_non_loopback` to `true` server-side —
/// these are the only valid settings for a remote https provider, so the UI
/// never has to (and cannot) get them wrong. The secret is never sent here:
/// only the `api_key_env_var` *name* the daemon already has in its environment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardApiModelRegisterRequest {
    name: String,
    base_url: String,
    model_id: String,
    #[serde(default)]
    runtime_preset: crate::m3::local_models::LocalModelRuntimePreset,
    api_key_env_var: String,
    /// Optional plaintext API key entered by the operator. DPAPI-encrypted at
    /// rest by the daemon; never stored or returned in plaintext.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    max_tools: Option<u32>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    probe_timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardApiModelRegisterResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    register: crate::m3::local_models::LocalModelRegisterResponse,
}

#[derive(Serialize)]
struct DashboardModelListResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    list: crate::m3::local_models::LocalModelListResponse,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardModelProbeRequest {
    name: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct DashboardModelProbeResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    probe: crate::m3::local_models::LocalModelProbeResponse,
}

/// Browser-facing request to remove a model-registry row (and its stored key).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardModelRemoveRequest {
    name: String,
}

#[derive(Serialize)]
struct DashboardModelRemoveResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    remove: crate::m3::local_models::LocalModelRemoveResponse,
}

#[derive(Serialize)]
struct DashboardModelUpdateResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    update: crate::m3::local_models::LocalModelUpdateResponse,
}

#[derive(Serialize)]
struct DashboardTranscriptSurface {
    source_of_truth: &'static str,
    row_count: usize,
    rows: Vec<crate::server::AgentTranscriptSnapshotRow>,
}

#[derive(Serialize)]
struct DashboardContextSurface {
    source_of_truth: &'static str,
    workspace: DashboardContextWorkspaceSurface,
    inboxes: Vec<DashboardContextInboxSurface>,
}

#[derive(Serialize)]
struct DashboardContextWorkspaceSurface {
    tool: &'static str,
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    list: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DashboardContextInboxSurface {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    spawn_id: Option<String>,
    agent_kind: String,
    lifecycle: String,
    source_of_truth: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    inbox: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardContextInjectRequest {
    session_id: String,
    channel: String,
    packet: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    workspace_key: Option<String>,
    #[serde(default)]
    request_receipt: bool,
}

#[derive(Serialize)]
struct DashboardContextInjectResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    channel: String,
    payload_sha256: String,
    readback: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardContextPlanRequest {
    session_id: String,
    plan: serde_json::Value,
    #[serde(default)]
    expected_version: Option<u64>,
    #[serde(default)]
    notify_agent: Option<bool>,
}

#[derive(Serialize)]
struct DashboardContextPlanResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    key: String,
    payload_sha256: String,
    workspace_put: serde_json::Value,
    notification: DashboardContextPlanNotification,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum DashboardContextPlanNotification {
    Skipped,
    Delivered {
        readback: serde_json::Value,
    },
    Failed {
        error_code: String,
        message: String,
        data: Option<serde_json::Value>,
    },
}

#[derive(Serialize)]
struct DashboardHygieneSurface {
    tool: &'static str,
    available: bool,
    source_of_truth: &'static str,
    report: crate::m3::hygiene::HygieneReportResponse,
}

const DASHBOARD_SAVED_VIEW_SCHEMA_VERSION: u32 = 1;
const DASHBOARD_SAVED_VIEW_PREFIX: &str = "dashboard-saved-view/v1/view/";
const DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH: &str = "CF_KV dashboard-saved-view/v1";
const DASHBOARD_SAVED_VIEW_MAX_NAME_CHARS: usize = 80;
const DASHBOARD_SAVED_VIEW_MAX_ID_CHARS: usize = 96;
const DASHBOARD_SAVED_VIEW_MAX_ROUTE_CHARS: usize = 32;
const DASHBOARD_SAVED_VIEW_MAX_FILTER_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardSavedViewRow {
    schema_version: u32,
    view_id: String,
    row_key: String,
    name: String,
    route: String,
    filters: serde_json::Value,
    created_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardSavedViewUpsertRequest {
    #[serde(default)]
    view_id: Option<String>,
    name: String,
    route: String,
    filters: serde_json::Value,
}

#[derive(Serialize)]
struct DashboardSavedViewsResponse {
    ok: bool,
    source_of_truth: &'static str,
    views: Vec<DashboardSavedViewRow>,
    corrupt_row_count: usize,
}

#[derive(Serialize)]
struct DashboardSavedViewUpsertResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    row_key: String,
    view: DashboardSavedViewRow,
}

#[derive(Serialize)]
struct DashboardSavedViewDeleteResponse {
    ok: bool,
    trigger: &'static str,
    source_of_truth: &'static str,
    deleted_row_key: String,
}

#[derive(Debug, Deserialize)]
struct DashboardAuditQueryRequest {
    limit: Option<usize>,
    scan_limit: Option<usize>,
    cursor: Option<String>,
    start_key_hex: Option<String>,
    start_ts_ns: Option<u64>,
    end_ts_ns: Option<u64>,
    session_id: Option<String>,
    tool: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
    row_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DashboardAgentEventsQueryRequest {
    limit: Option<usize>,
    scan_limit: Option<usize>,
    start_ts_ns: Option<u64>,
    end_ts_ns: Option<u64>,
    spawn_id: Option<String>,
    session_id: Option<String>,
    kind: Option<String>,
}

#[derive(Debug, Serialize)]
struct DashboardAgentEventRow {
    key_hex: String,
    ts_ns: u64,
    seq: u32,
    kind: String,
    spawn_id: Option<String>,
    session_id: Option<String>,
    record: AgentEventRecord,
}

#[derive(Debug, Serialize)]
struct DashboardAgentEventsQueryResponse {
    ok: bool,
    source_of_truth: &'static str,
    filters: DashboardAgentEventsQueryFilters,
    limit: usize,
    scan_limit: usize,
    scanned_rows: usize,
    matched_rows: usize,
    returned_count: usize,
    corrupt_row_count: usize,
    partial: bool,
    exhausted: bool,
    rows: Vec<DashboardAgentEventRow>,
}

#[derive(Debug, Serialize)]
struct DashboardAgentEventsQueryFilters {
    start_ts_ns: Option<u64>,
    end_ts_ns: Option<u64>,
    spawn_id: Option<String>,
    session_id: Option<String>,
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DashboardAgentRecordingQuery {
    event_limit: Option<usize>,
    event_scan_limit: Option<usize>,
    max_cast_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct DashboardAgentRecordingResponse {
    ok: bool,
    source_of_truth: &'static str,
    spawn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    agent_kind: String,
    lifecycle: String,
    metadata: DashboardAgentRecordingMetadata,
    asciicast: DashboardAsciicastReadback,
    journal: DashboardAgentEventsQueryResponse,
}

#[derive(Debug, Serialize)]
struct DashboardAgentRecordingMetadata {
    schema_version: u32,
    source: String,
    log_dir: String,
    asciicast_path: String,
    status_path: String,
    final_screen_path: String,
    input_audit_path: String,
    asciicast_bytes: u64,
    status_bytes: u64,
    final_screen_bytes: u64,
    input_audit_bytes: u64,
    status: Option<serde_json::Value>,
    capture_status: String,
    exit_code: Option<i64>,
    bytes_captured: Option<u64>,
    output_events: Option<u64>,
    duration_secs: f64,
    recording_truncated: bool,
    response_truncated: bool,
    crash_declared: bool,
    missing_artifact_count: usize,
}

#[derive(Debug, Serialize)]
struct DashboardAsciicastReadback {
    header: serde_json::Value,
    events: Vec<DashboardAsciicastEvent>,
    returned_event_count: usize,
    parsed_event_count: usize,
    corrupt_event_count: usize,
    output_event_count: usize,
    marker_event_count: usize,
    resize_event_count: usize,
    input_event_count: usize,
    exit_code: Option<i64>,
    duration_secs: f64,
    response_truncated: bool,
    recording_truncated: bool,
}

#[derive(Debug, Serialize)]
struct DashboardAsciicastEvent {
    time_secs: f64,
    interval_secs: f64,
    code: String,
    data: serde_json::Value,
}

#[derive(Debug)]
struct DashboardAgentRecordingSeed {
    spawn_id: String,
    session_id: Option<String>,
    agent_kind: String,
    lifecycle: String,
    log_dir: PathBuf,
    source: String,
}

async fn dashboard_index(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    with_dashboard_security_headers(Html(DASHBOARD_HTML).into_response())
}

async fn dashboard_asset(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(asset): Path<String>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match asset.as_str() {
        DASHBOARD_CSS_FILE => dashboard_asset_response("text/css; charset=utf-8", DASHBOARD_CSS),
        DASHBOARD_JS_FILE => {
            dashboard_asset_response("application/javascript; charset=utf-8", DASHBOARD_JS)
        }
        _ => with_dashboard_security_headers(
            (StatusCode::NOT_FOUND, "DASHBOARD_ASSET_NOT_FOUND").into_response(),
        ),
    }
}

async fn dashboard_saved_views(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match dashboard_saved_view_rows(&state.agent_events_db) {
        Ok((views, corrupt_row_count)) => with_dashboard_security_headers(
            Json(DashboardSavedViewsResponse {
                ok: true,
                source_of_truth: DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
                views,
                corrupt_row_count,
            })
            .into_response(),
        ),
        Err(response) => with_dashboard_security_headers(response),
    }
}

async fn dashboard_saved_view_upsert(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardSavedViewUpsertRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match dashboard_save_view_row(&state.agent_events_db, request) {
        Ok(view) => with_dashboard_security_headers(
            Json(DashboardSavedViewUpsertResponse {
                ok: true,
                trigger: "dashboard.saved_view_upsert",
                source_of_truth: DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
                row_key: view.row_key.clone(),
                view,
            })
            .into_response(),
        ),
        Err(response) => with_dashboard_security_headers(response),
    }
}

async fn dashboard_saved_view_delete(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(view_id): Path<String>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let view_id = match dashboard_validate_saved_view_id(&view_id) {
        Ok(view_id) => view_id,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let row_key = dashboard_saved_view_row_key(&view_id);
    match state
        .agent_events_db
        .delete_batch(cf::CF_KV, [row_key.as_bytes().to_vec()])
    {
        Ok(()) => with_dashboard_security_headers(
            Json(DashboardSavedViewDeleteResponse {
                ok: true,
                trigger: "dashboard.saved_view_delete",
                source_of_truth: DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
                deleted_row_key: row_key,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_storage_error_response(
            "dashboard saved view delete failed",
            error,
        )),
    }
}

#[derive(Debug, Deserialize)]
struct DashboardTerminalWsQuery {
    mode: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashboardTerminalMode {
    Observer,
    Controller,
}

impl DashboardTerminalMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Controller => "controller",
        }
    }
}

const TERMINAL_WS_COMMAND_INPUT: u8 = b'0';
const TERMINAL_WS_COMMAND_RESIZE: u8 = b'1';
const TERMINAL_WS_COMMAND_PAUSE: u8 = b'2';
const TERMINAL_WS_COMMAND_RESUME: u8 = b'3';
const TERMINAL_WS_COMMAND_AUTH_INIT: u8 = b'{';
const TERMINAL_WS_SERVER_OUTPUT: u8 = b'0';
const TERMINAL_WS_SERVER_TITLE: u8 = b'1';
const TERMINAL_WS_SERVER_PREFS: u8 = b'2';
const TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX: usize = 64 * 1024 * 1024;

async fn dashboard_agent_terminal_ws(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(spawn_id): Path<String>,
    Query(query): Query<DashboardTerminalWsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    if !dashboard_valid_agent_spawn_id(&spawn_id) {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "TERMINAL_SPAWN_ID_INVALID",
            "terminal WebSocket requires a valid agent-spawn id",
            Some(serde_json::json!({ "spawn_id": spawn_id })),
        ));
    }
    let mode = match dashboard_terminal_mode(query.mode.as_deref()) {
        Ok(mode) => mode,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let Some(session) = terminal_capture_session(&spawn_id) else {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::CONFLICT,
            "TERMINAL_SESSION_NOT_LIVE",
            "terminal WebSocket attach requires a currently running owned-PTY agent spawn",
            Some(serde_json::json!({
                "spawn_id": spawn_id,
                "structured_error": "dead_or_missing_terminal_session",
            })),
        ));
    };
    let snapshot = match session.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "TERMINAL_SNAPSHOT_FAILED",
                "terminal WebSocket failed to read the current shadow screen",
                Some(serde_json::json!({
                    "spawn_id": spawn_id,
                    "source_error": error.to_string(),
                })),
            ));
        }
    };
    if !matches!(snapshot.status, TerminalCaptureStatus::Running) {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::CONFLICT,
            "TERMINAL_SESSION_FINISHED",
            "terminal WebSocket attach requires a running PTY session",
            Some(serde_json::json!({
                "spawn_id": spawn_id,
                "status": format!("{:?}", snapshot.status),
            })),
        ));
    }

    let connection_id = uuid::Uuid::new_v4().to_string();
    tracing::info!(
        code = "DASHBOARD_TERMINAL_WS_ATTACH",
        spawn_id,
        connection_id,
        mode = mode.as_str(),
        process_id = snapshot.process_id,
        "dashboard terminal WebSocket attach accepted"
    );
    ws.on_upgrade(move |socket| {
        dashboard_agent_terminal_ws_loop(socket, session, spawn_id, connection_id, mode)
    })
}

async fn dashboard_agent_terminal_ws_loop(
    socket: WebSocket,
    session: Arc<LiveTerminalSession>,
    spawn_id: String,
    connection_id: String,
    mode: DashboardTerminalMode,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = session.subscribe();
    let snapshot = match session.snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = terminal_ws_send_prefs(
                &mut sender,
                serde_json::json!({
                    "event": "error",
                    "code": "TERMINAL_SNAPSHOT_FAILED",
                    "source_error": error.to_string(),
                }),
            )
            .await;
            return;
        }
    };
    let snapshot_seq = snapshot.seq;
    let mut paused = false;
    let mut paused_frames: VecDeque<Vec<u8>> = VecDeque::new();
    let mut paused_bytes = 0usize;

    if terminal_ws_send_prefs(
        &mut sender,
        serde_json::json!({
            "event": "attach",
            "protocol": "ttyd-compatible-1-byte-command",
            "mode": mode.as_str(),
            "spawn_id": spawn_id,
            "connection_id": connection_id,
            "process_id": snapshot.process_id,
            "cols": snapshot.cols,
            "rows": snapshot.rows,
            "snapshot_seq": snapshot_seq,
        }),
    )
    .await
    .is_err()
    {
        return;
    }
    if !snapshot.title.is_empty()
        && terminal_ws_send_frame(
            &mut sender,
            TERMINAL_WS_SERVER_TITLE,
            snapshot.title.as_bytes(),
        )
        .await
        .is_err()
    {
        return;
    }
    if terminal_ws_send_frame(
        &mut sender,
        TERMINAL_WS_SERVER_OUTPUT,
        &terminal_snapshot_dump(&snapshot.screen_text),
    )
    .await
    .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(message)) => {
                        match terminal_ws_client_payload(message) {
                            Some(payload) => {
                                if terminal_ws_handle_client_payload(
                                    &session,
                                    &mut sender,
                                    &connection_id,
                                    mode,
                                    &mut paused,
                                    &mut paused_frames,
                                    &mut paused_bytes,
                                    payload,
                                ).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(
                            code = "DASHBOARD_TERMINAL_WS_RECEIVE_FAILED",
                            spawn_id,
                            connection_id,
                            error = %error,
                            "dashboard terminal WebSocket receive failed"
                        );
                        break;
                    }
                    None => break,
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if event.seq <= snapshot_seq {
                            continue;
                        }
                        if terminal_ws_deliver_event(
                            &mut sender,
                            event,
                            paused,
                            &mut paused_frames,
                            &mut paused_bytes,
                        ).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        let _ = terminal_ws_send_prefs(
                            &mut sender,
                            serde_json::json!({
                                "event": "stream_lagged",
                                "dropped_events": dropped,
                            }),
                        ).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = terminal_ws_send_prefs(
                            &mut sender,
                            serde_json::json!({ "event": "closed" }),
                        ).await;
                        break;
                    }
                }
            }
        }
    }
    tracing::info!(
        code = "DASHBOARD_TERMINAL_WS_DETACHED",
        spawn_id,
        connection_id,
        mode = mode.as_str(),
        "dashboard terminal WebSocket detached"
    );
}

async fn terminal_ws_handle_client_payload(
    session: &LiveTerminalSession,
    sender: &mut SplitSink<WebSocket, Message>,
    connection_id: &str,
    mode: DashboardTerminalMode,
    paused: &mut bool,
    paused_frames: &mut VecDeque<Vec<u8>>,
    paused_bytes: &mut usize,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let Some((&command, body)) = payload.split_first() else {
        return Ok(());
    };
    match command {
        TERMINAL_WS_COMMAND_INPUT => {
            if mode != DashboardTerminalMode::Controller {
                let _ = session.audit_rejected_input(connection_id, body, "observer_mode");
                terminal_ws_send_prefs(
                    sender,
                    serde_json::json!({
                        "event": "input_rejected",
                        "reason": "observer_mode",
                    }),
                )
                .await
                .map_err(|_| ())?;
                return Ok(());
            }
            if let Err(error) = session.write_controller_input(connection_id, body) {
                terminal_ws_send_prefs(
                    sender,
                    serde_json::json!({
                        "event": "input_error",
                        "reason": error.to_string(),
                    }),
                )
                .await
                .map_err(|_| ())?;
            }
        }
        TERMINAL_WS_COMMAND_RESIZE => {
            if mode != DashboardTerminalMode::Controller {
                terminal_ws_send_prefs(
                    sender,
                    serde_json::json!({
                        "event": "resize_rejected",
                        "reason": "observer_mode",
                    }),
                )
                .await
                .map_err(|_| ())?;
                return Ok(());
            }
            match terminal_ws_parse_resize(body) {
                Ok((cols, rows)) => {
                    if let Err(error) = session.resize(connection_id, cols, rows) {
                        terminal_ws_send_prefs(
                            sender,
                            serde_json::json!({
                                "event": "resize_error",
                                "reason": error.to_string(),
                            }),
                        )
                        .await
                        .map_err(|_| ())?;
                    }
                }
                Err(error) => {
                    terminal_ws_send_prefs(
                        sender,
                        serde_json::json!({
                            "event": "resize_error",
                            "reason": error,
                        }),
                    )
                    .await
                    .map_err(|_| ())?;
                }
            }
        }
        TERMINAL_WS_COMMAND_PAUSE => {
            *paused = true;
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "paused",
                    "buffered_bytes": paused_bytes,
                }),
            )
            .await
            .map_err(|_| ())?;
        }
        TERMINAL_WS_COMMAND_RESUME => {
            *paused = false;
            while let Some(frame) = paused_frames.pop_front() {
                *paused_bytes = paused_bytes.saturating_sub(frame.len());
                sender
                    .send(Message::Binary(frame.into()))
                    .await
                    .map_err(|_| ())?;
            }
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "resumed",
                    "buffered_bytes": paused_bytes,
                }),
            )
            .await
            .map_err(|_| ())?;
        }
        TERMINAL_WS_COMMAND_AUTH_INIT => {
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "auth",
                    "status": "local_only_loopback",
                    "mode": mode.as_str(),
                }),
            )
            .await
            .map_err(|_| ())?;
        }
        _ => {
            terminal_ws_send_prefs(
                sender,
                serde_json::json!({
                    "event": "unknown_command",
                    "command": command,
                }),
            )
            .await
            .map_err(|_| ())?;
        }
    }
    Ok(())
}

async fn terminal_ws_deliver_event(
    sender: &mut SplitSink<WebSocket, Message>,
    event: TerminalCaptureEvent,
    paused: bool,
    paused_frames: &mut VecDeque<Vec<u8>>,
    paused_bytes: &mut usize,
) -> Result<(), ()> {
    let frame = match event.kind {
        TerminalCaptureEventKind::Output(bytes) => {
            terminal_ws_frame(TERMINAL_WS_SERVER_OUTPUT, &bytes)
        }
        TerminalCaptureEventKind::Title(title) => {
            terminal_ws_frame(TERMINAL_WS_SERVER_TITLE, title.as_bytes())
        }
        TerminalCaptureEventKind::Prefs(value) => {
            let bytes = serde_json::to_vec(&value).map_err(|_| ())?;
            terminal_ws_frame(TERMINAL_WS_SERVER_PREFS, &bytes)
        }
        TerminalCaptureEventKind::Exit(exit_code) => {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "event": "exit",
                "exit_code": exit_code,
            }))
            .map_err(|_| ())?;
            terminal_ws_frame(TERMINAL_WS_SERVER_PREFS, &bytes)
        }
    };
    if paused {
        terminal_ws_buffer_paused_frame(paused_frames, paused_bytes, frame)
    } else {
        sender
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|_| ())
    }
}

fn terminal_ws_buffer_paused_frame(
    paused_frames: &mut VecDeque<Vec<u8>>,
    paused_bytes: &mut usize,
    frame: Vec<u8>,
) -> Result<(), ()> {
    let new_total = paused_bytes.saturating_add(frame.len());
    if new_total > TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX {
        return Err(());
    }
    *paused_bytes = new_total;
    paused_frames.push_back(frame);
    Ok(())
}

fn terminal_ws_client_payload(message: Message) -> Option<Vec<u8>> {
    match message {
        Message::Binary(bytes) => Some(bytes.to_vec()),
        Message::Text(text) => Some(text.as_bytes().to_vec()),
        Message::Ping(_) | Message::Pong(_) => Some(Vec::new()),
        Message::Close(_) => None,
    }
}

async fn terminal_ws_send_prefs(
    sender: &mut SplitSink<WebSocket, Message>,
    value: serde_json::Value,
) -> Result<(), axum::Error> {
    let bytes =
        serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"event\":\"encode_error\"}".to_vec());
    terminal_ws_send_frame(sender, TERMINAL_WS_SERVER_PREFS, &bytes).await
}

async fn terminal_ws_send_frame(
    sender: &mut SplitSink<WebSocket, Message>,
    code: u8,
    payload: &[u8],
) -> Result<(), axum::Error> {
    sender
        .send(Message::Binary(terminal_ws_frame(code, payload).into()))
        .await
}

fn terminal_ws_frame(code: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(code);
    frame.extend_from_slice(payload);
    frame
}

fn terminal_snapshot_dump(screen_text: &str) -> Vec<u8> {
    let mut dump = Vec::from(&b"\x1b[H\x1b[2J"[..]);
    if !screen_text.is_empty() {
        dump.extend_from_slice(screen_text.replace('\n', "\r\n").as_bytes());
    }
    dump
}

fn terminal_ws_parse_resize(payload: &[u8]) -> Result<(u16, u16), String> {
    let text = std::str::from_utf8(payload)
        .map_err(|error| format!("resize payload must be UTF-8: {error}"))?
        .trim();
    if text.is_empty() {
        return Err("resize payload is empty".to_owned());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let cols = value
            .get("cols")
            .or_else(|| value.get("columns"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "resize JSON requires cols/columns".to_owned())?;
        let rows = value
            .get("rows")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "resize JSON requires rows".to_owned())?;
        return terminal_ws_validate_size(cols, rows);
    }
    let parts: Vec<&str> = text
        .split(['x', 'X', ',', ' '])
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 2 {
        return Err("resize payload must be COLSxROWS or JSON".to_owned());
    }
    let cols = parts[0]
        .parse::<u64>()
        .map_err(|error| format!("resize cols invalid: {error}"))?;
    let rows = parts[1]
        .parse::<u64>()
        .map_err(|error| format!("resize rows invalid: {error}"))?;
    terminal_ws_validate_size(cols, rows)
}

fn terminal_ws_validate_size(cols: u64, rows: u64) -> Result<(u16, u16), String> {
    if !(1..=500).contains(&cols) || !(1..=500).contains(&rows) {
        return Err("resize dimensions must be in 1..=500".to_owned());
    }
    Ok((cols as u16, rows as u16))
}

fn dashboard_terminal_mode(value: Option<&str>) -> Result<DashboardTerminalMode, Response> {
    match value
        .unwrap_or("observer")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "observer" | "observe" | "read" => Ok(DashboardTerminalMode::Observer),
        "controller" | "control" | "write" => Ok(DashboardTerminalMode::Controller),
        other => Err(with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "TERMINAL_MODE_INVALID",
            "terminal WebSocket mode must be observer or controller",
            Some(serde_json::json!({ "mode": other })),
        ))),
    }
}

fn dashboard_valid_agent_spawn_id(spawn_id: &str) -> bool {
    spawn_id.starts_with("agent-spawn-")
        && spawn_id.len() <= 128
        && spawn_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

async fn dashboard_tray_state(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let state_started = Instant::now();
    let mut timing_segments = Vec::new();
    let sessions = dashboard_timed_state_segment(&mut timing_segments, "sessions", || match state
        .health_service
        .session_list_impl(false)
    {
        Ok(sessions) => DashboardPanel::ok(
            "session_list dashboard primary agent feed + acknowledged escalation suppression",
            dashboard_primary_session_list_data(
                &sessions,
                state.health_service.acked_open_attention_anchors_snapshot(),
            ),
        ),
        Err(error) => DashboardPanel::error("session_list", format!("{error:?}")),
    });
    let lease = dashboard_timed_state_segment(&mut timing_segments, "lease", || {
        DashboardPanel::ok("control_lease_status", synapse_action::lease::status())
    });
    let timeline = dashboard_timed_state_segment(&mut timing_segments, "timeline", || match state
        .health_service
        .timeline_stats_snapshot()
    {
        Ok(snapshot) => DashboardPanel::ok("timeline_stats", snapshot),
        Err(error) => DashboardPanel::error("timeline_stats", format!("{error:?}")),
    });
    let approvals =
        dashboard_timed_state_segment(&mut timing_segments, "approvals", || {
            match state.health_service.approval_queue_snapshot(None) {
                Ok(rows) => DashboardPanel::ok(
                    "approval_list",
                    DashboardApprovalSurface {
                        tool: "approval_list",
                        available: true,
                        rows,
                    },
                ),
                Err(error) => DashboardPanel::error("approval_list", format!("{error:?}")),
            }
        });
    let demo_recording =
        dashboard_timed_state_segment(&mut timing_segments, "demo_recording", || {
            match state.health_service.demo_record_status_snapshot() {
                Ok(snapshot) => DashboardPanel::ok("demo_record_status", snapshot),
                Err(error) => DashboardPanel::error("demo_record_status", format!("{error:?}")),
            }
        });
    let timings = DashboardStateTimings {
        source_of_truth: "daemon Instant wall-clock around dashboard_tray_state segments",
        total_elapsed_ms: dashboard_elapsed_ms(state_started.elapsed()),
        segments: timing_segments,
    };
    let response = DashboardTrayStateResponse {
        schema_version: 1,
        generated_at_unix_ms: dashboard_unix_time_ms(),
        bind_addr: state.bind_addr.to_string(),
        token_policy: "dashboard responses never include bearer tokens",
        source_of_truth: "same dashboard/MCP snapshot methods as /dashboard/state.json, limited to tray companion panels",
        timings,
        sessions,
        lease,
        timeline,
        approvals,
        demo_recording,
    };
    with_dashboard_security_headers(Json(response).into_response())
}

async fn dashboard_state(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let state_started = Instant::now();
    let mut timing_segments = Vec::new();
    let active_sessions_started = Instant::now();
    let active_sessions = state.session_manager.sessions.read().await.len();
    emit_http_active_sessions(active_sessions);
    dashboard_push_state_timing(
        &mut timing_segments,
        "active_sessions",
        active_sessions_started,
    );
    let health = dashboard_timed_state_segment(&mut timing_segments, "health", || {
        state
            .health_service
            .health_payload_with_http_sessions(Some(active_sessions))
    });
    let sessions = dashboard_timed_state_segment(&mut timing_segments, "sessions", || match state
        .health_service
        .session_list_impl(false)
    {
        Ok(sessions) => DashboardPanel::ok(
            "session_list dashboard primary agent feed + acknowledged escalation suppression",
            dashboard_primary_session_list_data(
                &sessions,
                state.health_service.acked_open_attention_anchors_snapshot(),
            ),
        ),
        Err(error) => DashboardPanel::error("session_list", format!("{error:?}")),
    });
    let lease = dashboard_timed_state_segment(&mut timing_segments, "lease", || {
        DashboardPanel::ok("control_lease_status", synapse_action::lease::status())
    });
    let storage = dashboard_timed_state_segment(&mut timing_segments, "storage", || {
        match state.health_service.storage_summary_snapshot() {
            Ok(snapshot) => DashboardPanel::ok(
                "storage_summary",
                DashboardStorageSummary {
                    schema_version: snapshot.schema_version,
                    pressure_level: snapshot.pressure_level,
                    pressure_transition_codes: snapshot.pressure_transition_codes,
                    metrics_mode: snapshot.metrics_mode,
                    cf_sizes: snapshot.cf_sizes,
                    cf_row_counts: snapshot.cf_row_counts,
                    audit_retention_policy_count: snapshot.audit_retention_policy_count,
                    missing_cf_size_estimates: snapshot.missing_cf_size_estimates,
                    missing_cf_row_count_estimates: snapshot.missing_cf_row_count_estimates,
                },
            ),
            Err(error) => DashboardPanel::error("storage_summary", format!("{error:?}")),
        }
    });
    let target_claims =
        dashboard_timed_state_segment(&mut timing_segments, "target_claims", || {
            match state.health_service.target_claim_status_snapshot() {
                Ok(snapshot) => DashboardPanel::ok("target_claim_status", snapshot),
                Err(error) => DashboardPanel::error("target_claim_status", format!("{error:?}")),
            }
        });
    let timeline = dashboard_timed_state_segment(&mut timing_segments, "timeline", || match state
        .health_service
        .timeline_stats_snapshot()
    {
        Ok(snapshot) => DashboardPanel::ok("timeline_stats", snapshot),
        Err(error) => DashboardPanel::error("timeline_stats", format!("{error:?}")),
    });
    let demo_recording =
        dashboard_timed_state_segment(&mut timing_segments, "demo_recording", || {
            match state.health_service.demo_record_status_snapshot() {
                Ok(snapshot) => DashboardPanel::ok("demo_record_status", snapshot),
                Err(error) => DashboardPanel::error("demo_record_status", format!("{error:?}")),
            }
        });
    let events = dashboard_timed_state_segment(&mut timing_segments, "events", || {
        dashboard_events_panel(&state)
    });
    let hidden_desktops =
        dashboard_timed_state_segment(&mut timing_segments, "hidden_desktops", || {
            dashboard_hidden_desktops_panel(&state)
        });
    let cdp_attachments =
        dashboard_timed_state_segment(&mut timing_segments, "cdp_attachments", || {
            dashboard_cdp_attachments_panel(&state)
        });
    let shell_jobs = dashboard_timed_state_segment(&mut timing_segments, "shell_jobs", || {
        dashboard_shell_jobs_panel()
    });
    let tool_names = dashboard_timed_state_segment(&mut timing_segments, "tool_names", || {
        health
            .tool_names
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
    });
    let dashboard_assets =
        dashboard_timed_state_segment(&mut timing_segments, "dashboard_assets", || {
            DashboardPanel::ok(
                "embedded dashboard dist assets",
                DashboardAssetSurface {
                    schema_version: 1,
                    source_of_truth: "synapse-mcp embedded dashboard dist asset constants",
                    js_file: DASHBOARD_JS_FILE,
                    css_file: DASHBOARD_CSS_FILE,
                },
            )
        });
    let auth = dashboard_timed_state_segment(&mut timing_segments, "auth", || {
        DashboardPanel::ok(
            "local-only trust model (loopback bind + Host guard; no app-layer auth)",
            serde_json::json!({
                "mode": "local_only",
                "authenticated": true,
                "source": "loopback bind + Host header guard",
            }),
        )
    });
    let daemon = dashboard_timed_state_segment(&mut timing_segments, "daemon", || {
        DashboardPanel::ok("health", &health)
    });
    let command_audit =
        dashboard_timed_state_segment(&mut timing_segments, "command_audit", || {
            command_audit_panel(&state)
        });
    let tasks = dashboard_timed_state_segment(&mut timing_segments, "tasks", || {
        tasks_panel(&state, &tool_names)
    });
    let approvals = dashboard_timed_state_segment(&mut timing_segments, "approvals", || {
        approval_panel(&state, &tool_names, None)
    });
    let suggestions = dashboard_timed_state_segment(&mut timing_segments, "suggestions", || {
        approval_panel(
            &state,
            &tool_names,
            Some(crate::m3::approvals::ApprovalKind::Suggestion),
        )
    });
    let armed_runs = dashboard_timed_state_segment(&mut timing_segments, "armed_runs", || {
        approval_panel(
            &state,
            &tool_names,
            Some(crate::m3::approvals::ApprovalKind::ArmedRunReview),
        )
    });
    let agent_transcripts =
        dashboard_timed_state_segment(&mut timing_segments, "agent_transcripts", || {
            agent_transcript_panel(&state)
        });
    let agent_cost = dashboard_timed_state_segment(&mut timing_segments, "agent_cost", || {
        match state.health_service.dashboard_agent_cost_snapshot() {
            Ok(snapshot) => DashboardPanel::ok(
                "agent_cost transcript-authoritative fleet/model/template/task rollup",
                snapshot,
            ),
            Err(error) => DashboardPanel::error("agent_cost", format!("{error:?}")),
        }
    });
    let agent_stats = dashboard_timed_state_segment(&mut timing_segments, "agent_stats", || {
        match state.health_service.dashboard_agent_stats_snapshot() {
            Ok(snapshot) => DashboardPanel::ok(
                "agent_stats CF_AGENT_EVENTS fleet/per-agent rollup",
                snapshot,
            ),
            Err(error) => DashboardPanel::error("agent_stats", format!("{error:?}")),
        }
    });
    let context = dashboard_timed_state_segment(&mut timing_segments, "context", || {
        context_panel(&state, &tool_names, &sessions)
    });
    let hygiene = dashboard_timed_state_segment(&mut timing_segments, "hygiene", || {
        hygiene_panel(&state, &tool_names)
    });
    let local_models = dashboard_timed_state_segment(&mut timing_segments, "local_models", || {
        local_model_panel(&state, &tool_names)
    });
    let timings = DashboardStateTimings {
        source_of_truth: "daemon Instant wall-clock around dashboard_state segments",
        total_elapsed_ms: dashboard_elapsed_ms(state_started.elapsed()),
        segments: timing_segments,
    };
    let response = DashboardStateResponse {
        schema_version: 1,
        generated_at_unix_ms: dashboard_unix_time_ms(),
        bind_addr: state.bind_addr.to_string(),
        token_policy: "dashboard responses never include bearer tokens",
        timings,
        dashboard_assets,
        auth,
        daemon,
        sessions,
        lease,
        storage,
        target_claims,
        timeline,
        demo_recording,
        events,
        hidden_desktops,
        cdp_attachments,
        shell_jobs,
        command_audit,
        tasks,
        approvals,
        suggestions,
        armed_runs,
        agent_transcripts,
        agent_cost,
        agent_stats,
        context,
        hygiene,
        local_models,
    };
    with_dashboard_security_headers(Json(response).into_response())
}

fn dashboard_timed_state_segment<T>(
    timings: &mut Vec<DashboardStateTiming>,
    segment: &'static str,
    action: impl FnOnce() -> T,
) -> T {
    let started = Instant::now();
    let result = action();
    dashboard_push_state_timing(timings, segment, started);
    result
}

fn dashboard_push_state_timing(
    timings: &mut Vec<DashboardStateTiming>,
    segment: &'static str,
    started: Instant,
) {
    timings.push(DashboardStateTiming {
        segment,
        elapsed_ms: dashboard_elapsed_ms(started.elapsed()),
    });
}

fn dashboard_elapsed_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn dashboard_events_subscribe(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardEventSubscribeRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (filter, kinds, source_of_truth) = dashboard_event_subscription(request.scope);
    let subscription_id =
        match state
            .sse_state
            .subscribe(filter, kinds, request.snapshot_first, None)
        {
            Ok(subscription_id) => subscription_id,
            Err(error) => {
                return with_dashboard_security_headers(dashboard_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error.code(),
                    &error.message(),
                    Some(serde_json::json!({ "scope": request.scope })),
                ));
            }
        };
    with_dashboard_security_headers(
        Json(DashboardEventSubscribeResponse {
            ok: true,
            source_of_truth,
            scope: request.scope,
            event_url: dashboard_event_url(&subscription_id),
            subscription_id,
            replay_contract: "browser EventSource reconnect sends Last-Event-ID to /events with the stable subscription_id query parameter",
        })
        .into_response(),
    )
}

async fn dashboard_events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<sse::EventsQuery>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    with_dashboard_security_headers(state.sse_state.open(&headers, query))
}

async fn dashboard_audit_query(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(request): Query<DashboardAuditQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::server::command_audit::CommandAuditQueryParams {
        limit: request.limit,
        scan_limit: request.scan_limit,
        start_key_hex: request.cursor.or(request.start_key_hex),
        start_ts_ns: request.start_ts_ns,
        end_ts_ns: request.end_ts_ns,
        session_id: request.session_id,
        tool: request.tool,
        status: request.status,
        error_code: request.error_code,
        row_kind: request.row_kind,
    };
    match state.health_service.command_audit_query(params) {
        Ok(query) => with_dashboard_security_headers(
            Json(serde_json::json!({
                "ok": true,
                "source_of_truth": "CF_ACTION_LOG bounded scan",
                "query": query,
            }))
            .into_response(),
        ),
        Err(error) => {
            let code = dashboard_error_code(&error);
            let status = if code == synapse_core::error_codes::TOOL_PARAMS_INVALID {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            with_dashboard_security_headers(dashboard_error_response(
                status,
                &code,
                &error.message,
                error.data.clone(),
            ))
        }
    }
}

async fn dashboard_agent_events_query(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(request): Query<DashboardAgentEventsQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match dashboard_agent_event_rows(&state.agent_events_db, request) {
        Ok(response) => with_dashboard_security_headers(Json(response).into_response()),
        Err(response) => with_dashboard_security_headers(response),
    }
}

async fn dashboard_agent_recording(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(spawn_id): Path<String>,
    Query(request): Query<DashboardAgentRecordingQuery>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    if !dashboard_valid_agent_spawn_id(&spawn_id) {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "AGENT_RECORDING_SPAWN_ID_INVALID",
            "agent recording requires a valid agent-spawn id",
            Some(serde_json::json!({ "spawn_id": spawn_id })),
        ));
    }
    match dashboard_agent_recording_readback(&state, spawn_id, request) {
        Ok(response) => with_dashboard_security_headers(Json(response).into_response()),
        Err(response) => with_dashboard_security_headers(response),
    }
}

fn dashboard_agent_recording_readback(
    state: &HttpState,
    spawn_id: String,
    request: DashboardAgentRecordingQuery,
) -> Result<DashboardAgentRecordingResponse, Response> {
    let seed = dashboard_agent_recording_seed(state, &spawn_id)?;
    let cast_path = seed.log_dir.join("terminal.cast");
    let status_path = seed.log_dir.join("terminal-capture-status.json");
    let final_screen_path = seed.log_dir.join("terminal-final-screen.txt");
    let input_audit_path = seed.log_dir.join("terminal-input-audit.ndjson");
    let max_cast_bytes = request
        .max_cast_bytes
        .unwrap_or(DASHBOARD_ASCIICAST_DEFAULT_MAX_BYTES)
        .clamp(1, DASHBOARD_ASCIICAST_HARD_MAX_BYTES);
    let asciicast = dashboard_read_asciicast(&cast_path, max_cast_bytes)?;
    let status = dashboard_read_json_file_lossy(&status_path);
    let capture_status = status
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| {
            if cast_path.exists() {
                "unknown"
            } else {
                "missing"
            }
        })
        .to_owned();
    let exit_code = status
        .as_ref()
        .and_then(|value| value.get("exit_code"))
        .and_then(serde_json::Value::as_i64)
        .or(asciicast.exit_code);
    let bytes_captured = status
        .as_ref()
        .and_then(|value| value.get("bytes_captured"))
        .and_then(serde_json::Value::as_u64);
    let output_events = status
        .as_ref()
        .and_then(|value| value.get("output_events"))
        .and_then(serde_json::Value::as_u64)
        .or_else(|| u64::try_from(asciicast.output_event_count).ok());
    let recording_truncated = status
        .as_ref()
        .map(dashboard_capture_status_declares_truncation)
        .unwrap_or(false)
        || asciicast.recording_truncated;
    let crash_declared = dashboard_capture_status_declares_crash(status.as_ref(), exit_code);
    let missing_artifact_count = [
        cast_path.exists(),
        status_path.exists(),
        final_screen_path.exists(),
        input_audit_path.exists(),
    ]
    .into_iter()
    .filter(|exists| !exists)
    .count();
    let journal = dashboard_agent_event_rows(
        &state.agent_events_db,
        DashboardAgentEventsQueryRequest {
            limit: Some(request.event_limit.unwrap_or(500).clamp(1, 2_000)),
            scan_limit: Some(request.event_scan_limit.unwrap_or(50_000).clamp(1, 50_000)),
            start_ts_ns: None,
            end_ts_ns: None,
            spawn_id: Some(spawn_id),
            session_id: None,
            kind: None,
        },
    )?;

    Ok(DashboardAgentRecordingResponse {
        ok: true,
        source_of_truth: "session_list registry log_dir + terminal.cast/terminal-capture-status.json + CF_AGENT_EVENTS bounded scan",
        spawn_id: seed.spawn_id,
        session_id: seed.session_id,
        agent_kind: seed.agent_kind,
        lifecycle: seed.lifecycle,
        metadata: DashboardAgentRecordingMetadata {
            schema_version: 1,
            source: seed.source,
            log_dir: seed.log_dir.display().to_string(),
            asciicast_path: cast_path.display().to_string(),
            status_path: status_path.display().to_string(),
            final_screen_path: final_screen_path.display().to_string(),
            input_audit_path: input_audit_path.display().to_string(),
            asciicast_bytes: dashboard_file_len(&cast_path),
            status_bytes: dashboard_file_len(&status_path),
            final_screen_bytes: dashboard_file_len(&final_screen_path),
            input_audit_bytes: dashboard_file_len(&input_audit_path),
            status,
            capture_status,
            exit_code,
            bytes_captured,
            output_events,
            duration_secs: asciicast.duration_secs,
            recording_truncated,
            response_truncated: asciicast.response_truncated,
            crash_declared,
            missing_artifact_count,
        },
        asciicast,
        journal,
    })
}

fn dashboard_agent_recording_seed(
    state: &HttpState,
    spawn_id: &str,
) -> Result<DashboardAgentRecordingSeed, Response> {
    let sessions = state
        .health_service
        .session_list_impl(true)
        .map_err(|error| {
            let code = dashboard_error_code(&error);
            dashboard_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &code,
                "agent recording failed to read session registry",
                error.data,
            )
        })?;
    for summary in &sessions.sessions {
        if let Some(spawned) = summary.registry.spawned_agent.as_ref()
            && spawned.spawn_id == spawn_id
        {
            return Ok(DashboardAgentRecordingSeed {
                spawn_id: spawned.spawn_id.clone(),
                session_id: Some(summary.registry.session_id.clone()),
                agent_kind: spawned.cli.clone(),
                lifecycle: summary.registry.lifecycle.clone(),
                log_dir: PathBuf::from(&spawned.log_dir),
                source: "session_list.sessions[].spawned_agent.log_dir".to_owned(),
            });
        }
    }
    for row in sessions
        .unbound_agent_states
        .iter()
        .chain(sessions.terminal_unbound_agent_states.iter())
    {
        let row_spawn_id = row.spawn_id.as_deref().unwrap_or(row.anchor.as_str());
        if row_spawn_id == spawn_id
            && let Some(log_dir) = row.log_dir.as_ref().filter(|value| !value.is_empty())
        {
            return Ok(DashboardAgentRecordingSeed {
                spawn_id: spawn_id.to_owned(),
                session_id: row.session_id.clone(),
                agent_kind: row.agent_kind.clone().unwrap_or_else(|| "agent".to_owned()),
                lifecycle: row.state.as_str().to_owned(),
                log_dir: PathBuf::from(log_dir),
                source: "session_list.unbound_agent_states[].log_dir".to_owned(),
            });
        }
    }
    for row in &sessions.attached_agent_registry.rows {
        if row.spawn_id.as_deref() == Some(spawn_id)
            && let Some(spawn_dir) = row.spawn_dir.as_ref().filter(|value| !value.is_empty())
        {
            return Ok(DashboardAgentRecordingSeed {
                spawn_id: spawn_id.to_owned(),
                session_id: row.session_id.clone(),
                agent_kind: row.kind.clone(),
                lifecycle: row.lifecycle.clone(),
                log_dir: PathBuf::from(spawn_dir),
                source: "session_list.attached_agent_registry.rows[].spawn_dir".to_owned(),
            });
        }
    }
    for root in dashboard_agent_spawn_root_candidates() {
        let log_dir = root.join(spawn_id);
        if log_dir.join("terminal.cast").is_file() {
            return Ok(DashboardAgentRecordingSeed {
                spawn_id: spawn_id.to_owned(),
                session_id: None,
                agent_kind: "agent".to_owned(),
                lifecycle: "physical_recording".to_owned(),
                log_dir,
                source: "validated %LOCALAPPDATA%/Synapse/agent-spawns physical recording fallback"
                    .to_owned(),
            });
        }
    }
    Err(dashboard_error_response(
        StatusCode::NOT_FOUND,
        "AGENT_RECORDING_SPAWN_NOT_FOUND",
        "agent recording requires a known Synapse-spawned agent or a physical terminal.cast under the Synapse agent-spawns root",
        Some(serde_json::json!({
            "spawn_id": spawn_id,
            "source_of_truth": "session_list include_closed=true sessions/unbound/attached registry rows + validated %LOCALAPPDATA%/Synapse/agent-spawns fallback",
        })),
    ))
}

fn dashboard_agent_spawn_root_candidates() -> Vec<PathBuf> {
    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let local_app_data = PathBuf::from(local_app_data);
    let mut roots = vec![
        local_app_data.join("Synapse").join("agent-spawns"),
        local_app_data.join("synapse").join("agent-spawns"),
    ];
    roots.sort();
    roots.dedup();
    roots
}

fn dashboard_read_asciicast(
    path: &PathBuf,
    max_bytes: u64,
) -> Result<DashboardAsciicastReadback, Response> {
    let file_len = dashboard_file_len(path);
    if file_len == 0 {
        return Err(dashboard_error_response(
            StatusCode::NOT_FOUND,
            "AGENT_RECORDING_ASCIICAST_MISSING",
            "agent recording asciicast file is missing or empty",
            Some(serde_json::json!({
                "asciicast_path": path.display().to_string(),
            })),
        ));
    }
    let response_truncated = file_len > max_bytes;
    let mut file = fs::File::open(path).map_err(|error| {
        dashboard_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "AGENT_RECORDING_ASCIICAST_OPEN_FAILED",
            "agent recording asciicast file could not be opened",
            Some(serde_json::json!({
                "asciicast_path": path.display().to_string(),
                "source_error": error.to_string(),
            })),
        )
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            dashboard_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "AGENT_RECORDING_ASCIICAST_READ_FAILED",
                "agent recording asciicast file could not be read",
                Some(serde_json::json!({
                    "asciicast_path": path.display().to_string(),
                    "source_error": error.to_string(),
                })),
            )
        })?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if response_truncated && let Some(last_newline) = text.rfind('\n') {
        text.truncate(last_newline);
    }
    dashboard_parse_asciicast_text(&text, response_truncated)
}

fn dashboard_parse_asciicast_text(
    text: &str,
    response_truncated: bool,
) -> Result<DashboardAsciicastReadback, Response> {
    let mut lines = text.lines();
    let Some(header_line) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
        return Err(dashboard_error_response(
            StatusCode::BAD_GATEWAY,
            "AGENT_RECORDING_ASCIICAST_HEADER_MISSING",
            "agent recording asciicast file has no header line",
            None,
        ));
    };
    let header: serde_json::Value = serde_json::from_str(header_line).map_err(|error| {
        dashboard_error_response(
            StatusCode::BAD_GATEWAY,
            "AGENT_RECORDING_ASCIICAST_HEADER_INVALID",
            "agent recording asciicast header is not valid JSON",
            Some(serde_json::json!({
                "source_error": error.to_string(),
            })),
        )
    })?;
    if header.get("version").and_then(serde_json::Value::as_u64) != Some(3) {
        return Err(dashboard_error_response(
            StatusCode::BAD_GATEWAY,
            "AGENT_RECORDING_ASCIICAST_VERSION_INVALID",
            "agent recording asciicast must be version 3",
            Some(serde_json::json!({ "header": header })),
        ));
    }

    let mut events = Vec::new();
    let mut parsed_event_count = 0_usize;
    let mut corrupt_event_count = 0_usize;
    let mut output_event_count = 0_usize;
    let mut marker_event_count = 0_usize;
    let mut resize_event_count = 0_usize;
    let mut input_event_count = 0_usize;
    let mut exit_code = None;
    let mut duration_secs = 0.0_f64;
    for line in lines.map(str::trim).filter(|line| !line.is_empty()) {
        let value = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => value,
            Err(_error) => {
                corrupt_event_count += 1;
                continue;
            }
        };
        let Some(array) = value.as_array() else {
            corrupt_event_count += 1;
            continue;
        };
        if array.len() < 3 {
            corrupt_event_count += 1;
            continue;
        }
        let Some(interval_secs) = array[0].as_f64() else {
            corrupt_event_count += 1;
            continue;
        };
        let Some(code) = array[1].as_str() else {
            corrupt_event_count += 1;
            continue;
        };
        let data = array[2].clone();
        let interval_secs = if interval_secs.is_finite() {
            interval_secs.max(0.0)
        } else {
            0.0
        };
        duration_secs += interval_secs;
        parsed_event_count += 1;
        match code {
            "o" => output_event_count += 1,
            "m" => marker_event_count += 1,
            "r" => resize_event_count += 1,
            "i" => input_event_count += 1,
            "x" => {
                exit_code = data
                    .as_str()
                    .and_then(|value| value.parse::<i64>().ok())
                    .or_else(|| data.as_i64());
            }
            _ => {}
        }
        events.push(DashboardAsciicastEvent {
            time_secs: duration_secs,
            interval_secs,
            code: code.to_owned(),
            data,
        });
    }
    let recording_truncated = response_truncated || exit_code.is_none();
    Ok(DashboardAsciicastReadback {
        header,
        returned_event_count: events.len(),
        events,
        parsed_event_count,
        corrupt_event_count,
        output_event_count,
        marker_event_count,
        resize_event_count,
        input_event_count,
        exit_code,
        duration_secs,
        response_truncated,
        recording_truncated,
    })
}

fn dashboard_read_json_file_lossy(path: &PathBuf) -> Option<serde_json::Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn dashboard_file_len(path: &PathBuf) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn dashboard_capture_status_declares_truncation(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            let key_declares = key.to_ascii_lowercase().contains("trunc");
            (key_declares && dashboard_json_value_truthy(value))
                || dashboard_capture_status_declares_truncation(value)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(dashboard_capture_status_declares_truncation),
        serde_json::Value::String(value) => value.to_ascii_lowercase().contains("truncat"),
        _ => false,
    }
}

fn dashboard_capture_status_declares_crash(
    status: Option<&serde_json::Value>,
    exit_code: Option<i64>,
) -> bool {
    if let Some(code) = exit_code
        && code != 0
    {
        return true;
    }
    status.is_some_and(|value| dashboard_json_value_mentions_crash(value))
}

fn dashboard_json_value_mentions_crash(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            key.to_ascii_lowercase().contains("crash") || dashboard_json_value_mentions_crash(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(dashboard_json_value_mentions_crash),
        serde_json::Value::String(value) => {
            let value = value.to_ascii_lowercase();
            value.contains("crash") || value.contains("panic") || value.contains("fault")
        }
        _ => false,
    }
}

fn dashboard_json_value_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_i64().unwrap_or(0) != 0,
        serde_json::Value::String(value) => {
            let value = value.trim();
            !value.is_empty() && !matches!(value, "false" | "0" | "none" | "no")
        }
        serde_json::Value::Array(values) => !values.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Null => false,
    }
}

fn dashboard_agent_event_rows(
    db: &Db,
    request: DashboardAgentEventsQueryRequest,
) -> Result<DashboardAgentEventsQueryResponse, Response> {
    let limit = request.limit.unwrap_or(100).clamp(1, 2_000);
    let scan_limit = request.scan_limit.unwrap_or(10_000).clamp(limit, 50_000);
    if let (Some(start), Some(end)) = (request.start_ts_ns, request.end_ts_ns)
        && start >= end
    {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "DASHBOARD_AGENT_EVENTS_RANGE_INVALID",
            "start_ts_ns must be less than end_ts_ns",
            Some(serde_json::json!({
                "start_ts_ns": start,
                "end_ts_ns": end,
            })),
        ));
    }
    let spawn_id = request
        .spawn_id
        .and_then(|value| trim_optional_non_empty(&value));
    let session_id = request
        .session_id
        .and_then(|value| trim_optional_non_empty(&value));
    let kind = request
        .kind
        .and_then(|value| trim_optional_non_empty(&value));
    let filters = DashboardAgentEventsQueryFilters {
        start_ts_ns: request.start_ts_ns,
        end_ts_ns: request.end_ts_ns,
        spawn_id,
        session_id,
        kind,
    };

    let mut scanned_rows = 0_usize;
    let mut matched_rows = 0_usize;
    let mut corrupt_row_count = 0_usize;
    let mut rows = Vec::with_capacity(limit.min(128));
    let mut start =
        synapse_storage::agent_events::agent_event_scan_start(filters.start_ts_ns.unwrap_or(0));
    let mut exhausted = true;

    'paging: loop {
        if scanned_rows >= scan_limit {
            exhausted = false;
            break;
        }
        let (batch, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, 512)
            .map_err(|error| dashboard_storage_error_response("agent events read failed", error))?;
        if batch.is_empty() {
            break;
        }
        for (key, value) in &batch {
            let (ts_ns, seq) = match synapse_storage::agent_events::decode_agent_event_key(key) {
                Ok(decoded) => decoded,
                Err(error) => {
                    corrupt_row_count += 1;
                    tracing::warn!(
                        code = synapse_core::error_codes::STORAGE_CORRUPTED,
                        detail = %error,
                        "dashboard agent event key decode failed"
                    );
                    continue;
                }
            };
            if let Some(end) = filters.end_ts_ns
                && ts_ns >= end
            {
                break 'paging;
            }
            scanned_rows += 1;
            if scanned_rows > scan_limit {
                exhausted = false;
                break 'paging;
            }
            let record: AgentEventRecord = match synapse_storage::decode_json(value) {
                Ok(record) => record,
                Err(error) => {
                    corrupt_row_count += 1;
                    tracing::warn!(
                        code = synapse_core::error_codes::STORAGE_CORRUPTED,
                        ts_ns,
                        seq,
                        detail = %error,
                        "dashboard agent event row decode failed"
                    );
                    continue;
                }
            };
            if let Some(want) = filters.spawn_id.as_deref()
                && record.spawn_id.as_deref() != Some(want)
            {
                continue;
            }
            if let Some(want) = filters.session_id.as_deref()
                && record.session_id.as_deref() != Some(want)
            {
                continue;
            }
            let kind_label = dashboard_agent_event_kind(record.kind);
            if let Some(want) = filters.kind.as_deref()
                && kind_label != want
            {
                continue;
            }
            matched_rows += 1;
            if rows.len() < limit {
                rows.push(DashboardAgentEventRow {
                    key_hex: dashboard_hex_encode(key),
                    ts_ns,
                    seq,
                    kind: kind_label.to_owned(),
                    spawn_id: record.spawn_id.clone(),
                    session_id: record.session_id.clone(),
                    record,
                });
            }
        }
        if !more {
            break;
        }
        let Some((last_key, _value)) = batch.last() else {
            break;
        };
        start = dashboard_key_after(last_key);
    }
    let partial = !exhausted || matched_rows > rows.len();
    Ok(DashboardAgentEventsQueryResponse {
        ok: true,
        source_of_truth: "CF_AGENT_EVENTS bounded physical row scan",
        filters,
        limit,
        scan_limit,
        scanned_rows,
        matched_rows,
        returned_count: rows.len(),
        corrupt_row_count,
        partial,
        exhausted,
        rows,
    })
}

fn dashboard_saved_view_rows(db: &Db) -> Result<(Vec<DashboardSavedViewRow>, usize), Response> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, DASHBOARD_SAVED_VIEW_PREFIX.as_bytes())
        .map_err(|error| {
            dashboard_storage_error_response("dashboard saved views read failed", error)
        })?;
    let mut views = Vec::new();
    let mut corrupt_row_count = 0;
    for (key, value) in rows {
        match serde_json::from_slice::<DashboardSavedViewRow>(&value) {
            Ok(row) if key == row.row_key.as_bytes() => views.push(row),
            Ok(_row) => corrupt_row_count += 1,
            Err(error) => {
                corrupt_row_count += 1;
                tracing::warn!(
                    code = synapse_core::error_codes::STORAGE_CORRUPTED,
                    row_key = %String::from_utf8_lossy(&key),
                    detail = %error,
                    "dashboard saved view row decode failed"
                );
            }
        }
    }
    views.sort_by(|left, right| {
        right
            .updated_unix_ms
            .cmp(&left.updated_unix_ms)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok((views, corrupt_row_count))
}

fn dashboard_save_view_row(
    db: &Db,
    request: DashboardSavedViewUpsertRequest,
) -> Result<DashboardSavedViewRow, Response> {
    let now = dashboard_unix_time_ms();
    let name = dashboard_validate_saved_view_name(&request.name)?;
    let route = dashboard_validate_saved_view_route(&request.route)?;
    let filters = dashboard_validate_saved_view_filters(request.filters)?;
    let view_id = match request.view_id {
        Some(value) => dashboard_validate_saved_view_id(&value)?,
        None => dashboard_saved_view_id_from_name(&name, now),
    };
    let row_key = dashboard_saved_view_row_key(&view_id);
    let created_unix_ms = dashboard_read_saved_view_by_key(db, &row_key)?
        .map(|row| row.created_unix_ms)
        .unwrap_or(now);
    let row = DashboardSavedViewRow {
        schema_version: DASHBOARD_SAVED_VIEW_SCHEMA_VERSION,
        view_id,
        row_key: row_key.clone(),
        name,
        route,
        filters,
        created_unix_ms,
        updated_unix_ms: now,
    };
    let encoded = serde_json::to_vec(&row).map_err(|error| {
        tracing::error!(
            code = synapse_core::error_codes::STORAGE_WRITE_FAILED,
            row_key,
            detail = %error,
            "dashboard saved view row encode failed"
        );
        dashboard_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            synapse_core::error_codes::STORAGE_WRITE_FAILED,
            "dashboard saved view row encode failed",
            None,
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
        .map_err(|error| {
            dashboard_storage_error_response("dashboard saved view write failed", error)
        })?;
    tracing::info!(
        code = "DASHBOARD_SAVED_VIEW_WRITTEN",
        row_key,
        source_of_truth = DASHBOARD_SAVED_VIEW_SOURCE_OF_TRUTH,
        "dashboard saved view row written"
    );
    Ok(row)
}

fn dashboard_read_saved_view_by_key(
    db: &Db,
    row_key: &str,
) -> Result<Option<DashboardSavedViewRow>, Response> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| {
            dashboard_storage_error_response("dashboard saved view read failed", error)
        })?;
    let Some((_key, value)) = rows
        .into_iter()
        .find(|(key, _value)| key == row_key.as_bytes())
    else {
        return Ok(None);
    };
    let row = serde_json::from_slice::<DashboardSavedViewRow>(&value).map_err(|error| {
        tracing::error!(
            code = synapse_core::error_codes::STORAGE_CORRUPTED,
            row_key,
            detail = %error,
            "dashboard saved view row corrupt"
        );
        dashboard_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            synapse_core::error_codes::STORAGE_CORRUPTED,
            "dashboard saved view row corrupt",
            None,
        )
    })?;
    Ok(Some(row))
}

fn dashboard_validate_saved_view_name(value: &str) -> Result<String, Response> {
    let trimmed = value.trim();
    let char_count = trimmed.chars().count();
    if trimmed.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view requires name",
            None,
        ));
    }
    if char_count > DASHBOARD_SAVED_VIEW_MAX_NAME_CHARS {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view name is too long",
            None,
        ));
    }
    Ok(trimmed.to_owned())
}

fn dashboard_validate_saved_view_id(value: &str) -> Result<String, Response> {
    let trimmed = value.trim();
    let char_count = trimmed.chars().count();
    if trimmed.is_empty()
        || char_count > DASHBOARD_SAVED_VIEW_MAX_ID_CHARS
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view id must be ascii alphanumeric plus ._-",
            None,
        ));
    }
    Ok(trimmed.to_owned())
}

fn dashboard_validate_saved_view_route(value: &str) -> Result<String, Response> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > DASHBOARD_SAVED_VIEW_MAX_ROUTE_CHARS {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view route is invalid",
            None,
        ));
    }
    match trimmed {
        "fleet" | "agent" | "tasks" | "approvals" | "analytics" | "timeline" | "system"
        | "audit" => Ok(trimmed.to_owned()),
        _ => Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view route is not supported",
            None,
        )),
    }
}

fn dashboard_validate_saved_view_filters(
    value: serde_json::Value,
) -> Result<serde_json::Value, Response> {
    if !value.is_object() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view filters must be an object",
            None,
        ));
    }
    let encoded = serde_json::to_vec(&value).map_err(|error| {
        tracing::error!(
            code = synapse_core::error_codes::TOOL_PARAMS_INVALID,
            detail = %error,
            "dashboard saved view filters encode failed"
        );
        dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view filters are invalid",
            None,
        )
    })?;
    if encoded.len() > DASHBOARD_SAVED_VIEW_MAX_FILTER_BYTES {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard saved view filters are too large",
            None,
        ));
    }
    Ok(value)
}

fn dashboard_saved_view_id_from_name(name: &str, now_unix_ms: u64) -> String {
    let mut slug = String::with_capacity(name.len().min(40));
    let mut previous_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("view");
    }
    format!("{slug}-{now_unix_ms}")
}

fn dashboard_saved_view_row_key(view_id: &str) -> String {
    format!("{DASHBOARD_SAVED_VIEW_PREFIX}{view_id}")
}

async fn dashboard_local_model_spawn(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardLocalModelSpawnRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let mut params = match dashboard_local_model_spawn_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    // Anchor the spawned agent's MCP endpoint to THIS daemon's bind address so
    // it phones home to the daemon that launched it (not the hardcoded default).
    params.mcp_url = crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr);
    match state
        .health_service
        .dashboard_spawn_local_model_agent(params)
        .await
    {
        Ok(spawn) => with_dashboard_security_headers(
            Json(DashboardLocalModelSpawnResponse {
                ok: true,
                trigger: "dashboard.local_model_spawn",
                source_of_truth:
                    "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
                spawn,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_spawn_agent(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardSpawnAgentRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (fan_out, mut spawn_request) = match dashboard_spawn_agent_request_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    // Anchor browser-triggered spawns to this daemon, just like the legacy
    // local-model-only endpoint. The browser cannot override the MCP endpoint.
    spawn_request.mcp_url = crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr);
    let mut attempts = Vec::with_capacity(fan_out as usize);
    for index in 1..=fan_out {
        let attempt = match state
            .health_service
            .dashboard_spawn_agent_request(spawn_request.clone())
            .await
        {
            Ok(spawn) => DashboardSpawnAgentAttempt {
                index,
                status: "ok",
                spawn: Some(spawn),
                error_code: None,
                message: None,
                data: None,
            },
            Err(error) => DashboardSpawnAgentAttempt {
                index,
                status: "error",
                spawn: None,
                error_code: Some(dashboard_error_code(&error)),
                message: Some(error.message.to_string()),
                data: error.data.clone(),
            },
        };
        attempts.push(attempt);
    }
    let succeeded_count = attempts
        .iter()
        .filter(|attempt| attempt.status == "ok")
        .count();
    let failed_count = attempts.len().saturating_sub(succeeded_count);
    with_dashboard_security_headers(
        Json(DashboardSpawnAgentResponse {
            ok: true,
            trigger: "dashboard.spawn_agent",
            source_of_truth:
                "CF_AGENT_EVENTS, CF_PROCESS_HISTORY, session registry, agent spawn artifacts",
            requested_count: fan_out,
            succeeded_count,
            failed_count,
            attempts,
        })
        .into_response(),
    )
}

async fn dashboard_approval_decide(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardApprovalDecideRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let approval_id = request.approval_id.trim();
    if approval_id.is_empty() {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "APPROVAL_DECIDE_ID_EMPTY",
            "approval_id must be a non-empty approval id",
            None,
        ));
    }
    let decision = match request.decision.trim().to_ascii_lowercase().as_str() {
        "approve" | "accept" | "allow" => crate::m3::approvals::ApprovalDecision::Accept,
        "deny" | "decline" | "reject" => crate::m3::approvals::ApprovalDecision::Decline,
        "snooze" => crate::m3::approvals::ApprovalDecision::Snooze,
        other => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                "APPROVAL_DECIDE_DECISION_INVALID",
                &format!(
                    "decision {other:?} is not one of approve|accept|allow|deny|decline|reject|snooze"
                ),
                None,
            ));
        }
    };
    let note = request
        .note
        .as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty());
    // edited_args is JSON object text — must NOT be trimmed of internal content;
    // only drop an entirely-empty field. The approvals layer validates the JSON.
    let edited_args = request
        .edited_args
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let response = request
        .response
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match state.health_service.approval_decide_from_dashboard(
        approval_id,
        decision,
        note,
        edited_args,
        response,
        "dashboard_inbox",
    ) {
        Ok(decision) => with_dashboard_security_headers(
            Json(DashboardApprovalDecideResponse {
                ok: true,
                trigger: "dashboard.approval_decide",
                source_of_truth:
                    "CF_KV approval queue rows + approval audit rows + command audit; blocked approval_gate resumed",
                decision,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_context_inject(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardContextInjectRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let session_id = match dashboard_context_resolve_live_session_id(&state, &request.session_id) {
        Ok(session_id) => session_id,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let packet = request.packet.trim();
    if packet.is_empty() {
        return with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "CONTEXT_PACKET_EMPTY",
            "packet must be non-empty",
            None,
        ));
    }
    let channel = request.channel.trim().to_ascii_lowercase();
    let now_unix_ms = dashboard_unix_time_ms();
    let payload = serde_json::json!({
        "schema_version": 1,
        "source": "dashboard_context",
        "target_session_id": session_id,
        "requested_session_id": request.session_id.trim(),
        "channel": channel,
        "packet": packet,
        "created_at_unix_ms": now_unix_ms,
    });
    let payload_sha256 = dashboard_payload_sha256(&payload);
    let result = match channel.as_str() {
        "steer" => state.health_service.dashboard_agent_steer(
            session_id,
            packet.to_owned(),
            request.request_receipt,
        ),
        "mailbox" => {
            let kind = request
                .kind
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("context_packet")
                .to_owned();
            state.health_service.dashboard_agent_send(
                session_id,
                kind,
                payload,
                request.request_receipt,
            )
        }
        "workspace" => {
            let key = request
                .workspace_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("context/{session_id}/{now_unix_ms}"));
            state
                .health_service
                .dashboard_workspace_put(key, None, payload)
        }
        other => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                "CONTEXT_CHANNEL_INVALID",
                &format!("channel {other:?} is not one of steer|mailbox|workspace"),
                None,
            ));
        }
    };

    match result {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardContextInjectResponse {
                ok: true,
                trigger: "dashboard.context_inject",
                source_of_truth:
                    "agent_steer/agent_send/workspace_put command audit rows + tool-specific durable readback",
                channel,
                payload_sha256,
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_context_plan(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardContextPlanRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let session_id = match dashboard_context_resolve_live_session_id(&state, &request.session_id) {
        Ok(session_id) => session_id,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let key = format!("plan/{session_id}");
    let payload = serde_json::json!({
        "schema_version": 1,
        "source": "dashboard_context",
        "target_session_id": session_id,
        "requested_session_id": request.session_id.trim(),
        "plan": request.plan,
        "updated_at_unix_ms": dashboard_unix_time_ms(),
    });
    let payload_sha256 = dashboard_payload_sha256(&payload);
    let workspace_put = match state.health_service.dashboard_workspace_put(
        key.clone(),
        request.expected_version,
        payload,
    ) {
        Ok(readback) => readback,
        Err(error) => {
            return with_dashboard_security_headers(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                &dashboard_error_code(&error),
                &error.message,
                error.data,
            ));
        }
    };
    let notification = if request.notify_agent.unwrap_or(true) {
        let instruction = format!(
            "Plan updated in workspace key {key}. Re-read that plan artifact before continuing and acknowledge the changed step."
        );
        match state
            .health_service
            .dashboard_agent_steer(session_id, instruction, true)
        {
            Ok(readback) => DashboardContextPlanNotification::Delivered { readback },
            Err(error) => DashboardContextPlanNotification::Failed {
                error_code: dashboard_error_code(&error),
                message: error.message.to_string(),
                data: error.data,
            },
        }
    } else {
        DashboardContextPlanNotification::Skipped
    };
    with_dashboard_security_headers(
        Json(DashboardContextPlanResponse {
            ok: true,
            trigger: "dashboard.context_plan",
            source_of_truth:
                "workspace_put CF_KV plan artifact + optional agent_steer notification + CF_ACTION_LOG command audit",
            key,
            payload_sha256,
            workspace_put,
            notification,
        })
        .into_response(),
    )
}

async fn dashboard_agent_kill(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentKillRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_agent_kill_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state
        .health_service
        .dashboard_agent_kill_request(params)
        .await
    {
        Ok(kill) => with_dashboard_security_headers(
            Json(DashboardAgentKillResponse {
                ok: true,
                trigger: "dashboard.agent_kill",
                source_of_truth:
                    "OS process table, session registry, CF_AGENT_EVENTS, command audit rows, agent spawn artifacts",
                kill,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_agent_broadcast(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentBroadcastRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_agent_broadcast(
        request.selector,
        request.agent_kinds,
        request.sessions,
        request.kind,
        request.payload,
        request.ttl_ms,
        request.request_receipt,
    ) {
        Ok(broadcast) => with_dashboard_security_headers(
            Json(DashboardAgentBroadcastResponse {
                ok: true,
                trigger: "dashboard.agent_broadcast",
                source_of_truth:
                    "CF_KV agent mailbox rows + CF_ACTION_LOG command audit + dashboard readback",
                broadcast,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_fleet_stop(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardFleetStopRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::server::agent_control::FleetStopParams {
        mode: request.mode,
        confirm: request.confirm,
        agent_kinds: request.agent_kinds,
        grace_ms: request
            .grace_ms
            .unwrap_or(DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS),
    };
    match state.health_service.dashboard_fleet_stop_request(params).await {
        Ok(fleet_stop) => with_dashboard_security_headers(
            Json(DashboardFleetStopResponse {
                ok: true,
                trigger: "dashboard.fleet_stop",
                source_of_truth:
                    "OS process table, session registry, CF_AGENT_EVENTS, CF_ACTION_LOG command audit rows",
                fleet_stop,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_agent_interrupt(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentLookupRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_agent_interrupt_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state.health_service.dashboard_agent_interrupt_request(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.agent_interrupt",
                source_of_truth:
                    "ranked agent_interrupt delivery channels, CF_AGENT_EVENTS, command audit rows, process table readback",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_agent_pause(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentLookupRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_agent_pause_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state.health_service.dashboard_agent_pause_request(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.agent_pause",
                source_of_truth:
                    "OS process/thread table suspend readback, CF_AGENT_EVENTS, command audit rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_agent_resume(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentLookupRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_agent_pause_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state.health_service.dashboard_agent_resume_request(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.agent_resume",
                source_of_truth:
                    "OS process/thread table resume readback, CF_AGENT_EVENTS, command audit rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_agent_respawn(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardAgentRespawnRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_agent_respawn_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let mcp_url = crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr);
    match state
        .health_service
        .dashboard_agent_respawn_request(params, mcp_url)
        .await
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.agent_respawn",
                source_of_truth:
                    "spawn manifest, OS process table, CF_AGENT_EVENTS, session registry, command audit rows, agent spawn artifacts",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_create(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_tasks::TaskCreateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_task_create(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_create",
                source_of_truth: "CF_KV agent-task/v1 rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_update(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_tasks::TaskUpdateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_task_update(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_update",
                source_of_truth: "CF_KV agent-task/v1 rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_cancel(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_tasks::TaskCancelParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_task_cancel(params).await {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_cancel",
                source_of_truth:
                    "CF_KV agent-task/v1 rows plus OS process table/session registry for live-attempt interrupt",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_task_dispatch_once(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTaskDispatchOnceRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::server::agent_tasks::TaskDispatchOnceParams {
        concurrency_cap: request
            .concurrency_cap
            .unwrap_or_else(crate::server::agent_tasks::default_cap),
        mcp_url: crate::m4::agent_spawn_mcp_url_for_bind(state.bind_addr),
        wait_timeout_ms: request
            .wait_timeout_ms
            .unwrap_or_else(crate::m4::default_agent_spawn_wait_timeout_ms),
    };
    match state.health_service.dashboard_task_dispatch_once(params).await {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.task_dispatch_once",
                source_of_truth:
                    "CF_KV agent-task/v1 rows, CF_AGENT_EVENTS, session registry, and agent spawn artifacts",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_pause(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelinePauseRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::timeline_control::TimelinePauseParams {
        duration_ms: request.duration_ms,
    };
    match state.health_service.dashboard_timeline_pause(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_pause",
                source_of_truth: "CF_KV timeline recorder control row + live recorder gate",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_resume(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state
        .health_service
        .dashboard_timeline_resume(crate::m3::timeline_control::TimelineResumeParams::default())
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_resume",
                source_of_truth: "CF_KV timeline recorder control row + live recorder gate",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_get(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelineQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::timeline::TimelineGetParams {
        start_ts_ns,
        end_ts_ns,
        kinds: request.kinds,
        actor: request.actor,
        limit: request.limit,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_timeline_get(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_get",
                source_of_truth: "timeline_get bounded read over CF_TIMELINE",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_search(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelineQueryRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::timeline::TimelineSearchParams {
        start_ts_ns,
        end_ts_ns,
        apps: request.apps,
        text: request.text,
        kinds: request.kinds,
        actor: request.actor,
        limit: request.limit,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_timeline_search(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_search",
                source_of_truth: "timeline_search scan over CF_TIMELINE",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_timeline_digest(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::timeline_digest::TimelineDigestParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_timeline_digest(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_digest",
                source_of_truth: "timeline_digest derived from CF_EPISODES plus CF_ROUTINES",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_episode_list(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardEpisodeListRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::episodes::EpisodeListParams {
        start_ts_ns,
        end_ts_ns,
        apps: request.apps,
        actor: request.actor,
        min_duration_ms: request.min_duration_ms,
        limit: request.limit,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_episode_list(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.episode_list",
                source_of_truth: "episode_list read over CF_EPISODES",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_episode_get(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardEpisodeGetRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::episodes::EpisodeGetParams {
        episode_id: request.episode_id,
        start_ts_ns,
        refs_limit: request.refs_limit,
        refs_cursor: request.refs_cursor,
    };
    match state.health_service.dashboard_episode_get(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.episode_get",
                source_of_truth: "episode_get read over CF_EPISODES plus CF_TIMELINE refs",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_routine_list(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::routines::RoutineListParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_routine_list(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.routine_list",
                source_of_truth: "routine_list read over CF_ROUTINES joined to CF_ROUTINE_STATE",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_routine_inspect(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::routines::RoutineInspectParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_routine_inspect(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.routine_inspect",
                source_of_truth: "routine_inspect read over CF_ROUTINES, CF_ROUTINE_STATE, and armed rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_routine_update(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::routines::RoutineUpdateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_routine_update(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.routine_update",
                source_of_truth: "routine_update write/readback over CF_ROUTINE_STATE and armed_routine/v1 rows",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

/// Parses an optional decimal epoch-nanosecond string into `Option<u64>`,
/// returning a `TOOL_PARAMS_INVALID` response on a malformed value. ns values
/// arrive as strings because they exceed JS `Number.MAX_SAFE_INTEGER`.
fn parse_optional_ns(value: Option<&str>, field: &str) -> Result<Option<u64>, Response> {
    let Some(raw) = value else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<u64>().map(Some).map_err(|error| {
        dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            &format!(
                "dashboard storage purge {field} must be a decimal nanosecond integer: {error}"
            ),
            None,
        )
    })
}

async fn dashboard_storage_timeline_purge(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardTimelinePurgeRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let start_ts_ns = match parse_optional_ns(request.start_ts_ns.as_deref(), "start_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let end_ts_ns = match parse_optional_ns(request.end_ts_ns.as_deref(), "end_ts_ns") {
        Ok(value) => value,
        Err(response) => return with_dashboard_security_headers(response),
    };
    let params = crate::m3::timeline::TimelinePurgeParams {
        start_ts_ns,
        end_ts_ns,
        apps: request.apps,
        text: None,
        kinds: request.kinds,
        actor: request.actor,
        flag_ids: None,
        all: request.all,
        dry_run: request.dry_run,
        cursor: request.cursor,
    };
    match state.health_service.dashboard_timeline_purge(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.timeline_purge",
                source_of_truth: "CF_TIMELINE hard-delete + range compaction + counts-only purge audit row",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_storage_gc(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardStorageGcRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::storage::StorageGcOnceParams {
        cf_name: request.cf_name,
        soft_cap_rows: request.soft_cap_rows,
        hard_cap_rows: request.hard_cap_rows,
        run_id: request.run_id,
        now_ns: request.now_ns,
        max_age_ns: request.max_age_ns,
        dedupe_window_ns: request.dedupe_window_ns,
        profile_id: request.profile_id,
    };
    match state.health_service.dashboard_storage_gc(params) {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardTimelineControlResponse {
                ok: true,
                trigger: "dashboard.storage_gc_once",
                source_of_truth: "storage_gc_once oldest-row eviction over the column family",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_control_lease_force_release(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardControlLeaseForceReleaseRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (owner_session_id, confirmed) = match dashboard_control_lease_force_release_params(request)
    {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state
        .health_service
        .dashboard_control_lease_force_release(owner_session_id, confirmed)
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.control_lease_force_release",
                source_of_truth:
                    "synapse_action::lease + CF_KV MCP session lease rows + CF_AGENT_EVENTS + CF_ACTION_LOG",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_control_lease_handoff(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardControlLeaseHandoffRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let (from_session_id, to_session_id, ttl_ms) =
        match dashboard_control_lease_handoff_params(request) {
            Ok(params) => params,
            Err(response) => return with_dashboard_security_headers(response),
        };
    match state
        .health_service
        .dashboard_control_lease_handoff(from_session_id, to_session_id, ttl_ms)
    {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.control_lease_handoff",
                source_of_truth:
                    "synapse_action::lease + CF_KV MCP session lease rows + CF_AGENT_EVENTS + CF_ACTION_LOG",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_target_claims_prune(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_target_claim_prune() {
        Ok(readback) => with_dashboard_security_headers(
            Json(DashboardControlResponse {
                ok: true,
                trigger: "dashboard.target_claims_prune",
                source_of_truth: "daemon target claim registry + CF_ACTION_LOG",
                readback,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_template_list(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_list_agent_templates() {
        Ok(list) => with_dashboard_security_headers(
            Json(DashboardTemplateListResponse {
                ok: true,
                trigger: "dashboard.template_list",
                source_of_truth: "CF_KV agent-template/v2/cur/",
                list,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_template_upsert(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::server::agent_templates::AgentTemplatePutParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_put_agent_template(params) {
        Ok(put) => with_dashboard_security_headers(
            Json(DashboardTemplateUpsertResponse {
                ok: true,
                trigger: "dashboard.template_upsert",
                source_of_truth: "CF_KV agent-template/v2/cur/",
                put,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_template_delete(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(template_id): Path<String>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state
        .health_service
        .dashboard_delete_agent_template(&template_id)
    {
        Ok(delete) => with_dashboard_security_headers(
            Json(DashboardTemplateDeleteResponse {
                ok: true,
                trigger: "dashboard.template_delete",
                source_of_truth: "CF_KV agent-template/v2/cur/",
                delete,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

fn dashboard_local_model_spawn_params(
    request: DashboardLocalModelSpawnRequest,
) -> Result<crate::m4::ActSpawnAgentParams, Response> {
    let model_ref = request.model_ref.trim();
    if model_ref.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard local-model spawn requires model_ref",
            None,
        ));
    }
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard local-model spawn requires prompt",
            None,
        ));
    }
    Ok(crate::m4::ActSpawnAgentParams {
        cli: None,
        kind: Some(crate::m4::ActSpawnAgentCli::LocalModel),
        model: None,
        model_ref: Some(model_ref.to_owned()),
        prompt: Some(prompt.to_owned()),
        target: None,
        working_dir: request
            .working_dir
            .and_then(|value| trim_optional_non_empty(&value)),
        mcp_url: crate::m4::default_agent_spawn_mcp_url(),
        wait_timeout_ms: request
            .wait_timeout_ms
            .unwrap_or_else(crate::m4::default_agent_spawn_wait_timeout_ms),
        hold_open_ms: request
            .hold_open_ms
            .unwrap_or_else(crate::m4::default_agent_spawn_hold_open_ms),
        // Local-model spawns have no permission-prompt-tool hook; the gate flag
        // is inert for them but kept at the default for struct completeness.
        require_approval_gate: crate::m4::default_require_approval_gate(),
        template_id: None,
        template_version: None,
        template_config_hash: None,
    })
}

fn dashboard_spawn_agent_request_params(
    request: DashboardSpawnAgentRequest,
) -> Result<(u32, crate::m4::ActSpawnAgentRequest), Response> {
    let fan_out = request.fan_out.unwrap_or(1);
    if fan_out == 0 || fan_out > DASHBOARD_SPAWN_FAN_OUT_MAX {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            &format!("dashboard spawn fan_out must be 1..={DASHBOARD_SPAWN_FAN_OUT_MAX}"),
            None,
        ));
    }
    Ok((
        fan_out,
        crate::m4::ActSpawnAgentRequest {
            template_id: request
                .template_id
                .and_then(|value| trim_optional_non_empty(&value)),
            template_version: request.template_version,
            template_params: request.template_params,
            cli: request.cli,
            kind: request.kind,
            model: request
                .model
                .and_then(|value| trim_optional_non_empty(&value)),
            model_ref: request
                .model_ref
                .and_then(|value| trim_optional_non_empty(&value)),
            prompt: request
                .prompt
                .and_then(|value| trim_optional_non_empty(&value)),
            target: request.target,
            working_dir: request
                .working_dir
                .and_then(|value| trim_optional_non_empty(&value)),
            mcp_url: crate::m4::default_agent_spawn_mcp_url(),
            wait_timeout_ms: request
                .wait_timeout_ms
                .unwrap_or_else(crate::m4::default_agent_spawn_wait_timeout_ms),
            hold_open_ms: request
                .hold_open_ms
                .unwrap_or_else(crate::m4::default_agent_spawn_hold_open_ms),
            require_approval_gate: request
                .require_approval_gate
                .unwrap_or_else(crate::m4::default_require_approval_gate),
        },
    ))
}

fn dashboard_agent_kill_params(
    request: DashboardAgentKillRequest,
) -> Result<crate::server::agent_control::AgentKillParams, Response> {
    let session_id = dashboard_agent_control_id(&request.session_id, "kill")?;
    Ok(crate::server::agent_control::AgentKillParams {
        session_id,
        grace_ms: request
            .grace_ms
            .unwrap_or(DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS),
        interrupt_first: request.interrupt_first.unwrap_or(true),
    })
}

fn dashboard_agent_interrupt_params(
    request: DashboardAgentLookupRequest,
) -> Result<crate::server::agent_control::AgentInterruptParams, Response> {
    Ok(crate::server::agent_control::AgentInterruptParams {
        session_id: dashboard_agent_control_id(&request.session_id, "interrupt")?,
    })
}

fn dashboard_agent_pause_params(
    request: DashboardAgentLookupRequest,
) -> Result<crate::server::agent_control::AgentPauseParams, Response> {
    Ok(crate::server::agent_control::AgentPauseParams {
        session_id: dashboard_agent_control_id(&request.session_id, "pause/resume")?,
    })
}

fn dashboard_agent_respawn_params(
    request: DashboardAgentRespawnRequest,
) -> Result<crate::server::agent_control::AgentRespawnParams, Response> {
    let session_id = dashboard_agent_control_id(&request.session_id, "respawn")?;
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard agent respawn requires prompt",
            None,
        ));
    }
    Ok(crate::server::agent_control::AgentRespawnParams {
        session_id,
        prompt: prompt.to_owned(),
        carry_context: request.carry_context.unwrap_or(true),
        grace_ms: request
            .grace_ms
            .unwrap_or(DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS),
    })
}

fn dashboard_agent_control_id(value: &str, verb: &str) -> Result<String, Response> {
    let session_id = value.trim();
    if session_id.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            &format!("dashboard agent {verb} requires session_id or spawn_id"),
            None,
        ));
    }
    Ok(session_id.to_owned())
}

fn dashboard_control_lease_force_release_params(
    request: DashboardControlLeaseForceReleaseRequest,
) -> Result<(String, bool), Response> {
    let owner_session_id = request.owner_session_id.trim();
    if owner_session_id.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease force-release requires owner_session_id",
            None,
        ));
    }
    if !request.confirmed {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease force-release requires confirmation",
            None,
        ));
    }
    Ok((owner_session_id.to_owned(), request.confirmed))
}

fn dashboard_control_lease_handoff_params(
    request: DashboardControlLeaseHandoffRequest,
) -> Result<(String, String, u64), Response> {
    let from_session_id = request.from_session_id.trim();
    let to_session_id = request.to_session_id.trim();
    if from_session_id.is_empty() || to_session_id.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease handoff requires from_session_id and to_session_id",
            None,
        ));
    }
    if from_session_id == to_session_id {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease handoff requires distinct sessions",
            None,
        ));
    }
    let ttl_ms = request
        .ttl_ms
        .unwrap_or(synapse_action::DEFAULT_LEASE_TTL_MS);
    if !(synapse_action::MIN_LEASE_TTL_MS..=synapse_action::MAX_LEASE_TTL_MS).contains(&ttl_ms) {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "dashboard lease handoff ttl_ms is outside the allowed lease range",
            None,
        ));
    }
    Ok((from_session_id.to_owned(), to_session_id.to_owned(), ttl_ms))
}

async fn dashboard_api_model_register(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardApiModelRegisterRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = match dashboard_api_model_register_params(request) {
        Ok(params) => params,
        Err(response) => return with_dashboard_security_headers(response),
    };
    match state
        .health_service
        .dashboard_register_api_model(params)
        .await
    {
        Ok(register) => with_dashboard_security_headers(
            Json(DashboardApiModelRegisterResponse {
                ok: true,
                trigger: "dashboard.api_model_register",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                register,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_api_model_update(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(params): Json<crate::m3::local_models::LocalModelUpdateParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state
        .health_service
        .dashboard_update_local_model(params)
        .await
    {
        Ok(update) => with_dashboard_security_headers(
            Json(DashboardModelUpdateResponse {
                ok: true,
                trigger: "dashboard.api_model_update",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                update,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_api_model_remove(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardModelRemoveRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::local_models::LocalModelRemoveParams { name: request.name };
    match state.health_service.dashboard_remove_local_model(params) {
        Ok(remove) => with_dashboard_security_headers(
            Json(DashboardModelRemoveResponse {
                ok: true,
                trigger: "dashboard.api_model_remove",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                remove,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_model_list(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    match state.health_service.dashboard_list_local_models() {
        Ok(list) => with_dashboard_security_headers(
            Json(DashboardModelListResponse {
                ok: true,
                trigger: "dashboard.model_list",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                list,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

async fn dashboard_model_probe(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<DashboardModelProbeRequest>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    let params = crate::m3::local_models::LocalModelProbeParams {
        name: request.name,
        timeout_ms: request.timeout_ms,
    };
    match state
        .health_service
        .dashboard_probe_local_model(params)
        .await
    {
        Ok(probe) => with_dashboard_security_headers(
            Json(DashboardModelProbeResponse {
                ok: true,
                trigger: "dashboard.model_probe",
                source_of_truth: "CF_KV local_model_registry/v1/model/name_hex/",
                probe,
            })
            .into_response(),
        ),
        Err(error) => with_dashboard_security_headers(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            &dashboard_error_code(&error),
            &error.message,
            error.data,
        )),
    }
}

fn dashboard_api_model_register_params(
    request: DashboardApiModelRegisterRequest,
) -> Result<crate::m3::local_models::LocalModelRegisterParams, Response> {
    fn require_non_empty(value: &str, field: &str) -> Result<String, Response> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(dashboard_error_response(
                StatusCode::BAD_REQUEST,
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                &format!("dashboard api-model register requires {field}"),
                None,
            ));
        }
        Ok(trimmed.to_owned())
    }
    let name = require_non_empty(&request.name, "name")?;
    let base_url = require_non_empty(&request.base_url, "base_url")?;
    let model_id = require_non_empty(&request.model_id, "model_id")?;
    let api_key_env_var = require_non_empty(&request.api_key_env_var, "api_key_env_var")?;
    Ok(crate::m3::local_models::LocalModelRegisterParams {
        name,
        base_url,
        model_id,
        // A remote cloud provider is OpenAI chat-completions over https; these
        // two settings are not the operator's to get wrong, so we fix them here.
        api_shape: crate::m3::local_models::LocalModelApiShape::OpenAiChatCompletions,
        runtime_preset: request.runtime_preset,
        context_length: request.context_length,
        max_tools: request.max_tools,
        notes: request
            .notes
            .and_then(|value| trim_optional_non_empty(&value)),
        enabled: true,
        allow_non_loopback: true,
        api_key_env_var: Some(api_key_env_var),
        // Optional plaintext key entered in the dashboard form. When present it
        // is DPAPI-encrypted at rest by register_local_model; never persisted in
        // plaintext and never echoed back.
        api_key: request
            .api_key
            .and_then(|value| trim_optional_non_empty(&value)),
        probe_timeout_ms: request.probe_timeout_ms,
    })
}

fn trim_optional_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn dashboard_error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    data: Option<serde_json::Value>,
) -> Response {
    (
        status,
        Json(serde_json::json!({
            "ok": false,
            "code": code,
            "message": message,
            "data": data,
        })),
    )
        .into_response()
}

fn dashboard_error_code(error: &rmcp::ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{:?}", error.code))
}

fn dashboard_agent_event_kind(kind: AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::SpawnRequested => "spawn_requested",
        AgentEventKind::SpawnReady => "spawn_ready",
        AgentEventKind::StateChanged => "state_changed",
        AgentEventKind::ToolCallStarted => "tool_call_started",
        AgentEventKind::ToolCallFinished => "tool_call_finished",
        AgentEventKind::TurnStarted => "turn_started",
        AgentEventKind::TurnFinished => "turn_finished",
        AgentEventKind::MessageSent => "message_sent",
        AgentEventKind::MessageReceived => "message_received",
        AgentEventKind::LeaseAcquired => "lease_acquired",
        AgentEventKind::LeaseReleased => "lease_released",
        AgentEventKind::Interrupted => "interrupted",
        AgentEventKind::Killed => "killed",
        AgentEventKind::Exited => "exited",
    }
}

fn dashboard_key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

fn dashboard_hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn dashboard_context_resolve_live_session_id(
    state: &HttpState,
    requested_session_id: &str,
) -> Result<String, Response> {
    let requested = requested_session_id.trim();
    if requested.is_empty() {
        return Err(dashboard_error_response(
            StatusCode::BAD_REQUEST,
            "CONTEXT_TARGET_EMPTY",
            "session_id must be a non-empty live MCP session id or spawn id",
            None,
        ));
    }
    let sessions = state
        .health_service
        .session_list_impl(false)
        .map_err(|error| {
            dashboard_error_response(
                StatusCode::BAD_REQUEST,
                &dashboard_error_code(&error),
                &error.message,
                error.data,
            )
        })?;
    for row in sessions.sessions {
        if row.registry.session_id == requested {
            return Ok(row.registry.session_id);
        }
        if row
            .registry
            .spawned_agent
            .as_ref()
            .is_some_and(|spawn| spawn.spawn_id == requested)
        {
            return Ok(row.registry.session_id);
        }
    }
    Err(dashboard_error_response(
        StatusCode::BAD_REQUEST,
        "CONTEXT_TARGET_NOT_LIVE",
        &format!("context target {requested:?} is not a live session/spawn in session_list"),
        Some(serde_json::json!({
            "source_of_truth": "session_list live sessions",
            "requested_session_id": requested,
        })),
    ))
}

fn dashboard_payload_sha256(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| value.to_string().into_bytes());
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn dashboard_storage_error_response(
    message: &str,
    error: synapse_storage::StorageError,
) -> Response {
    tracing::error!(
        code = synapse_core::error_codes::STORAGE_READ_FAILED,
        detail = %error,
        "dashboard storage operation failed"
    );
    dashboard_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        synapse_core::error_codes::STORAGE_READ_FAILED,
        message,
        None,
    )
}

async fn approval_activate(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<crate::m3::approvals::ApprovalActivationParams>,
) -> Response {
    if let Err(response) = dashboard_local_only(&state, &headers) {
        return with_dashboard_security_headers(response);
    }
    if params.bind != state.bind_addr.to_string() {
        return with_dashboard_security_headers(
            (StatusCode::BAD_REQUEST, "APPROVAL_ACTIVATION_BIND_MISMATCH").into_response(),
        );
    }
    match state
        .health_service
        .approval_decide_from_activation(&params, "approval_protocol")
    {
        Ok(response) => with_dashboard_security_headers(
            Html(approval_activation_html(&response)).into_response(),
        ),
        Err(error) => with_dashboard_security_headers(
            (
                StatusCode::BAD_REQUEST,
                format!("APPROVAL_ACTIVATION_FAILED: {}", error.message),
            )
                .into_response(),
        ),
    }
}

fn approval_activation_html(
    response: &crate::m3::approvals::ApprovalActivationDecisionResponse,
) -> String {
    let status = response.decision.after_status.as_str();
    format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<title>Synapse Approval</title>",
            "<link rel=\"stylesheet\" href=\"/dashboard/assets/{css_file}\">",
            "</head><body><h1>Synapse Approval</h1>",
            "<p>Approval <strong>{approval_id}</strong> is now <strong>{status}</strong>.</p>",
            "<p>Activation <code>{activation_id}</code> consumed.</p>",
            "</body></html>"
        ),
        css_file = DASHBOARD_CSS_FILE,
        approval_id = escape_html(&response.decision.approval_id),
        status = escape_html(status),
        activation_id = escape_html(&response.activation_id),
    )
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn dashboard_asset_response(content_type: &'static str, body: &'static str) -> Response {
    with_dashboard_security_headers(([(header::CONTENT_TYPE, content_type)], body).into_response())
}

fn with_dashboard_security_headers(mut response: Response) -> Response {
    const DASHBOARD_CSP: &str = concat!(
        "default-src 'none'; ",
        "base-uri 'none'; ",
        "object-src 'none'; ",
        "frame-ancestors 'none'; ",
        "form-action 'none'; ",
        "script-src 'self'; ",
        "style-src 'self'; ",
        "connect-src 'self'; ",
        "img-src 'self' data:; ",
        "font-src 'self'"
    );
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DASHBOARD_CSP),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(
            "camera=(), microphone=(), geolocation=(), payment=(), usb=(), serial=(), clipboard-read=(), clipboard-write=()",
        ),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(header::EXPIRES, HeaderValue::from_static("0"));
    response
}

fn dashboard_primary_session_list_data(
    sessions: impl Serialize,
    acked_attention_anchors: Result<BTreeSet<String>, rmcp::ErrorData>,
) -> serde_json::Value {
    let mut data = serde_json::to_value(sessions).unwrap_or_else(|error| {
        serde_json::json!({
            "serialization_error": error.to_string(),
        })
    });
    let Some(object) = data.as_object_mut() else {
        return data;
    };
    let Some(unbound_value) = object.remove("unbound_agent_states") else {
        return data;
    };
    let Some(unbound_rows) = unbound_value.as_array() else {
        object.insert("unbound_agent_states".to_owned(), unbound_value);
        return data;
    };
    let existing_terminal_rows = object
        .remove("terminal_unbound_agent_states")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();

    let mut primary_rows = Vec::new();
    let mut acknowledged_rows = Vec::new();
    let mut terminal_rows = existing_terminal_rows;
    let (acked_anchors, acked_anchor_error) = match acked_attention_anchors {
        Ok(anchors) => (anchors, None),
        Err(error) => (BTreeSet::new(), Some(error.message.to_string())),
    };
    for row in unbound_rows.iter().cloned() {
        if dashboard_agent_row_is_terminal(&row) {
            terminal_rows.push(row);
        } else if dashboard_agent_row_is_acknowledged_attention(&row, &acked_anchors) {
            acknowledged_rows.push(dashboard_mark_acknowledged_attention(row));
        } else {
            primary_rows.push(row);
        }
    }
    let primary_count = primary_rows.len();
    let acknowledged_count = acknowledged_rows.len();
    let terminal_count = terminal_rows.len();

    object.insert(
        "unbound_agent_states".to_owned(),
        serde_json::Value::Array(primary_rows),
    );
    object.insert(
        "acknowledged_unbound_agent_states".to_owned(),
        serde_json::Value::Array(acknowledged_rows),
    );
    object.insert(
        "terminal_unbound_agent_states".to_owned(),
        serde_json::Value::Array(terminal_rows),
    );
    object.insert(
        "dashboard_unbound_agent_filter".to_owned(),
        serde_json::json!({
            "source_of_truth": "session_list unbound_agent_states/terminal_unbound_agent_states + CF_KV escalation/v1/item acknowledged-open anchors split for dashboard attention feed",
            "primary_unbound_agent_count": primary_count,
            "acknowledged_unbound_agent_count": acknowledged_count,
            "terminal_unbound_agent_count": terminal_count,
            "terminal_states": ["dead", "done", "exited", "closed"],
            "terminal_attention_classes": ["terminal_setup_failure", "terminal_runtime_failure"],
            "acknowledged_attention_statuses": ["acked"],
            "acknowledged_attention_anchor_count": acked_anchors.len(),
            "acknowledged_attention_read_error": acked_anchor_error,
            "reason": "terminal unbound history is diagnostic history, not actionable attention",
        }),
    );
    data
}

fn dashboard_agent_row_is_terminal(row: &serde_json::Value) -> bool {
    if let Some(attention_class) = row
        .get("attention_class")
        .and_then(serde_json::Value::as_str)
    {
        return matches!(
            attention_class,
            "terminal_setup_failure" | "terminal_runtime_failure"
        );
    }
    row.get("state")
        .and_then(serde_json::Value::as_str)
        .map(|state| matches!(state, "dead" | "done" | "exited" | "closed"))
        .unwrap_or(false)
}

fn dashboard_agent_row_is_acknowledged_attention(
    row: &serde_json::Value,
    acked_anchors: &BTreeSet<String>,
) -> bool {
    let Some(state) = row.get("state").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if !matches!(
        state,
        "stuck" | "needs_input" | "awaiting_approval" | "ready_for_review"
    ) {
        return false;
    }
    dashboard_agent_row_anchor(row).is_some_and(|anchor| acked_anchors.contains(anchor))
}

fn dashboard_agent_row_anchor(row: &serde_json::Value) -> Option<&str> {
    row.get("anchor")
        .and_then(serde_json::Value::as_str)
        .or_else(|| row.get("spawn_id").and_then(serde_json::Value::as_str))
        .or_else(|| row.get("session_id").and_then(serde_json::Value::as_str))
}

fn dashboard_mark_acknowledged_attention(mut row: serde_json::Value) -> serde_json::Value {
    if let Some(object) = row.as_object_mut() {
        object.insert(
            "dashboard_attention_suppressed".to_owned(),
            serde_json::json!({
                "reason": "acknowledged_escalation",
                "source_of_truth": "CF_KV escalation/v1/item status=acked",
            }),
        );
    }
    row
}

fn dashboard_events_panel(state: &HttpState) -> DashboardPanel {
    let (owner_session_ids, owner_read_error) =
        match state.sse_state.subscription_owner_session_ids() {
            Ok(owner_session_ids) => (owner_session_ids, None),
            Err(error) => (Vec::new(), Some(format!("{error:?}"))),
        };
    DashboardPanel::ok(
        "SseState subscriptions + process-lifetime ingress counters",
        DashboardEventSurface {
            source_of_truth: "SseState subscriptions + process-lifetime ingress counters",
            active_subscription_count: state.sse_state.active_subscription_count(),
            owner_session_ids,
            owner_read_error,
            agent_event_ingress: crate::server::agent_event_ingress::ingress_stats(),
            agent_transcript_ingest: crate::server::agent_transcripts::ingest_stats(),
        },
    )
}

fn dashboard_hidden_desktops_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.hidden_desktop_readbacks() {
        Ok(rows) => DashboardPanel::ok(
            "session process resource ledger / hidden desktop leases",
            DashboardHiddenDesktopSurface {
                source_of_truth: "session process resource ledger / hidden desktop leases",
                row_count: rows.len(),
                rows,
            },
        ),
        Err(error) => DashboardPanel::error(
            "session process resource ledger / hidden desktop leases",
            format!("{error:?}"),
        ),
    }
}

fn dashboard_cdp_attachments_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.cdp_target_owner_readbacks() {
        Ok(rows) => DashboardPanel::ok(
            "CDP target ownership registry",
            DashboardCdpAttachmentSurface {
                source_of_truth: "CDP target ownership registry",
                row_count: rows.len(),
                rows,
            },
        ),
        Err(error) => DashboardPanel::error("CDP target ownership registry", format!("{error:?}")),
    }
}

fn dashboard_shell_jobs_panel() -> DashboardPanel {
    match crate::m4::shell_jobs_dashboard_snapshot(50) {
        Ok(snapshot) => DashboardPanel::ok(
            "act_run_shell_status + durable shell status files",
            snapshot,
        ),
        Err(error) => DashboardPanel::error(
            "act_run_shell_status + durable shell status files",
            format!("{error:?}"),
        ),
    }
}

fn context_panel(
    state: &HttpState,
    tool_names: &BTreeSet<&str>,
    sessions_panel: &DashboardPanel,
) -> DashboardPanel {
    let workspace = if tool_names.contains("workspace_list") {
        match state
            .health_service
            .dashboard_workspace_list_snapshot(None, 200, true)
        {
            Ok(list) => DashboardContextWorkspaceSurface {
                tool: "workspace_list",
                available: true,
                list: Some(list),
                error: None,
            },
            Err(error) => DashboardContextWorkspaceSurface {
                tool: "workspace_list",
                available: true,
                list: None,
                error: Some(format!("{error:?}")),
            },
        }
    } else {
        DashboardContextWorkspaceSurface {
            tool: "workspace_list",
            available: false,
            list: None,
            error: Some("workspace_list is not visible in the active tool profile".to_owned()),
        }
    };

    let mut inboxes = Vec::new();
    if tool_names.contains("agent_inbox") {
        match dashboard_context_inbox_seeds(sessions_panel) {
            Ok(seeds) => {
                for seed in seeds.iter().take(50) {
                    match state.health_service.dashboard_agent_inbox_snapshot(
                        &seed.session_id,
                        25,
                        Vec::new(),
                    ) {
                        Ok(inbox) => inboxes.push(DashboardContextInboxSurface {
                            session_id: seed.session_id.clone(),
                            spawn_id: seed.spawn_id.clone(),
                            agent_kind: seed.agent_kind.clone(),
                            lifecycle: seed.lifecycle.clone(),
                            source_of_truth: "CF_KV agent-mailbox/v1 peek via agent_inbox drain=false; sessions reused from dashboard state",
                            inbox: Some(inbox),
                            error: None,
                        }),
                        Err(error) => inboxes.push(DashboardContextInboxSurface {
                            session_id: seed.session_id.clone(),
                            spawn_id: seed.spawn_id.clone(),
                            agent_kind: seed.agent_kind.clone(),
                            lifecycle: seed.lifecycle.clone(),
                            source_of_truth: "CF_KV agent-mailbox/v1 peek via agent_inbox drain=false; sessions reused from dashboard state",
                            inbox: None,
                            error: Some(format!("{error:?}")),
                        }),
                    }
                }
            }
            Err(error) => {
                inboxes.push(DashboardContextInboxSurface {
                    session_id: "dashboard_sessions".to_owned(),
                    spawn_id: None,
                    agent_kind: "dashboard".to_owned(),
                    lifecycle: "error".to_owned(),
                    source_of_truth: "dashboard sessions panel + CF_KV agent-mailbox/v1",
                    inbox: None,
                    error: Some(error),
                });
            }
        }
    }

    DashboardPanel::ok(
        "workspace_list + agent_inbox drain=false + session_list",
        DashboardContextSurface {
            source_of_truth: "workspace_list CF_KV workspace-blackboard/v1 + agent_inbox CF_KV agent-mailbox/v1 + session_list target/session rows",
            workspace,
            inboxes,
        },
    )
}

#[derive(Clone, Debug)]
struct DashboardContextInboxSeed {
    session_id: String,
    spawn_id: Option<String>,
    agent_kind: String,
    lifecycle: String,
}

fn dashboard_context_inbox_seeds(
    sessions_panel: &DashboardPanel,
) -> Result<Vec<DashboardContextInboxSeed>, String> {
    if sessions_panel.status != "ok" {
        return Err(format!(
            "sessions panel is {}, not ok: {}",
            sessions_panel.status,
            sessions_panel.error.as_deref().unwrap_or("no error detail")
        ));
    }
    let full_rows = sessions_panel
        .data
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !full_rows.is_empty() {
        return Ok(full_rows
            .into_iter()
            .filter_map(|row| dashboard_context_inbox_seed_from_full_row(&row))
            .collect());
    }
    let compact_rows = sessions_panel
        .data
        .get("compact_sessions")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(compact_rows
        .into_iter()
        .filter_map(|row| dashboard_context_inbox_seed_from_compact_row(&row))
        .collect())
}

fn dashboard_context_inbox_seed_from_full_row(
    row: &serde_json::Value,
) -> Option<DashboardContextInboxSeed> {
    let session_id = dashboard_json_string(row, "session_id")?;
    let agent_state = row.get("agent_state");
    let spawned_agent = row.get("spawned_agent");
    Some(DashboardContextInboxSeed {
        session_id,
        spawn_id: dashboard_json_string(row, "spawn_id")
            .or_else(|| agent_state.and_then(|value| dashboard_json_string(value, "spawn_id")))
            .or_else(|| spawned_agent.and_then(|value| dashboard_json_string(value, "spawn_id"))),
        agent_kind: dashboard_json_string(row, "agent_kind")
            .or_else(|| dashboard_json_string(row, "client_name"))
            .unwrap_or_else(|| "agent".to_owned()),
        lifecycle: dashboard_json_string(row, "lifecycle").unwrap_or_else(|| "unknown".to_owned()),
    })
}

fn dashboard_context_inbox_seed_from_compact_row(
    row: &serde_json::Value,
) -> Option<DashboardContextInboxSeed> {
    Some(DashboardContextInboxSeed {
        session_id: dashboard_json_string(row, "session_id")?,
        spawn_id: dashboard_json_string(row, "spawned_agent_id"),
        agent_kind: dashboard_json_string(row, "agent_kind").unwrap_or_else(|| "agent".to_owned()),
        lifecycle: dashboard_json_string(row, "lifecycle").unwrap_or_else(|| "unknown".to_owned()),
    })
}

fn dashboard_json_string(row: &serde_json::Value, key: &str) -> Option<String> {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn approval_panel(
    state: &HttpState,
    tool_names: &BTreeSet<&str>,
    kind: Option<crate::m3::approvals::ApprovalKind>,
) -> DashboardPanel {
    if !tool_names.contains("approval_list") {
        return deferred_panel("approval_list", tool_names);
    }
    match state.health_service.approval_queue_snapshot(kind) {
        Ok(rows) => DashboardPanel::ok(
            "approval_list",
            DashboardApprovalSurface {
                tool: "approval_list",
                available: true,
                rows,
            },
        ),
        Err(error) => DashboardPanel::error("approval_list", format!("{error:?}")),
    }
}

fn tasks_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    if !tool_names.contains("task_list") {
        return deferred_panel("task_list", tool_names);
    }
    let list = match state.health_service.dashboard_task_snapshot(1000) {
        Ok(list) => list,
        Err(error) => return DashboardPanel::error("task_list", format!("{error:?}")),
    };
    let next = match state
        .health_service
        .dashboard_task_next(crate::server::agent_tasks::default_cap())
    {
        Ok(next) => next,
        Err(error) => return DashboardPanel::error("task_next", format!("{error:?}")),
    };
    DashboardPanel::ok(
        "CF_KV agent-task/v1 via task_list",
        DashboardTaskSurface {
            tool: "task_list",
            available: true,
            source_of_truth: "CF_KV agent-task/v1",
            row_count: list.count,
            tasks: list.tasks,
            reconciled_orphans: list.reconciled_orphans,
            next,
        },
    )
}

fn local_model_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    if !tool_names.contains("local_model_list") {
        return deferred_panel("local_model_list", tool_names);
    }
    match state.health_service.local_model_registry_snapshot() {
        Ok(rows) => {
            let enabled_count = rows.iter().filter(|row| row.enabled).count();
            let unhealthy_count = rows
                .iter()
                .filter(|row| row.last_probe.as_ref().is_some_and(|probe| !probe.healthy))
                .count();
            DashboardPanel::ok(
                "local_model_list",
                DashboardLocalModelSurface {
                    tool: "local_model_list",
                    available: true,
                    enabled_count,
                    unhealthy_count,
                    rows,
                },
            )
        }
        Err(error) => DashboardPanel::error("local_model_list", format!("{error:?}")),
    }
}

fn agent_transcript_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.agent_transcript_snapshot(50) {
        Ok(rows) => DashboardPanel::ok(
            "CF_AGENT_TRANSCRIPTS",
            DashboardTranscriptSurface {
                source_of_truth: "CF_AGENT_TRANSCRIPTS",
                row_count: rows.len(),
                rows,
            },
        ),
        Err(error) => DashboardPanel::error("CF_AGENT_TRANSCRIPTS", format!("{error:?}")),
    }
}

fn command_audit_panel(state: &HttpState) -> DashboardPanel {
    match state.health_service.command_audit_snapshot() {
        Ok(snapshot) => DashboardPanel::ok("CF_ACTION_LOG command_audit", snapshot),
        Err(error) => DashboardPanel::error("CF_ACTION_LOG command_audit", format!("{error:?}")),
    }
}

fn hygiene_panel(state: &HttpState, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    if !tool_names.contains("hygiene_report") {
        return deferred_panel("hygiene_report", tool_names);
    }
    match state.health_service.hygiene_report_snapshot(100) {
        Ok(response) => DashboardPanel::ok(
            "hygiene_report",
            DashboardHygieneSurface {
                tool: "hygiene_report",
                available: true,
                source_of_truth: "CF_KV hygiene/flag/v1 plus CF_EPISODES/CF_ROUTINES/CF_PROFILES joins",
                report: response,
            },
        ),
        Err(error) => DashboardPanel::error("hygiene_report", format!("{error:?}")),
    }
}

fn deferred_panel(tool: &'static str, tool_names: &BTreeSet<&str>) -> DashboardPanel {
    DashboardPanel::unavailable(
        tool,
        DashboardDeferredSurface {
            tool,
            available: tool_names.contains(tool),
            rows: Vec::new(),
        },
    )
}

/// The dashboard's ONLY access control, by deliberate policy.
///
/// POLICY (binding — see STATE/DECISION_LOG.md, issues #892/#913): the local
/// dashboard must NEVER be locked behind an access token, login, cookie session,
/// or CSRF gate. Synapse is single-user on the operator's own machine; the OS
/// login is the trust boundary and an app-layer credential is pure friction that
/// has been removed twice. This guard is transport hardening, NOT authentication:
/// it requires a loopback bind and a loopback `Host` header (blocking DNS-rebind /
/// cross-origin reach) and nothing else. Do not add `Authorization`/cookie/token
/// checks to any `/dashboard/*` route.
fn dashboard_local_only(state: &HttpState, headers: &HeaderMap) -> Result<(), Response> {
    if !state.bind_addr.ip().is_loopback() {
        return Err((StatusCode::FORBIDDEN, "DASHBOARD_LOOPBACK_BIND_REQUIRED").into_response());
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return Err((StatusCode::FORBIDDEN, "DASHBOARD_HOST_REQUIRED").into_response());
    };
    if dashboard_host_allowed(host) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "DASHBOARD_HOST_REFUSED").into_response())
    }
}

fn dashboard_host_allowed(raw: &str) -> bool {
    let raw = raw.trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        raw.split(':').next().unwrap_or(raw)
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

fn dashboard_unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn record_mcp_transport_diagnostics(
    request: Request<Body>,
    next: middleware::Next,
) -> Response {
    let path = request.uri().path().to_owned();
    if !path.starts_with("/mcp") {
        return next.run(request).await;
    }

    let method = request.method().to_string();
    let request_id = record_http_mcp_request_started(&method, &path);
    let started = Instant::now();
    let response = next.run(request).await;
    record_http_mcp_request_completed(
        request_id,
        &method,
        &path,
        response.status(),
        started.elapsed(),
    );
    response
}

const DASHBOARD_CSS_FILE: &str = "dashboard-CicCCuUG.css";
const DASHBOARD_JS_FILE: &str = "dashboard-D_jF422B.js";
const DASHBOARD_HTML: &str = include_str!("../../../../dashboard/dist/index.html");
const DASHBOARD_CSS: &str =
    include_str!("../../../../dashboard/dist/assets/dashboard-CicCCuUG.css");
const DASHBOARD_JS: &str = include_str!("../../../../dashboard/dist/assets/dashboard-D_jF422B.js");
#[cfg(test)]
const DASHBOARD_APP_SOURCE: &str = include_str!("../../../../dashboard/src/app.tsx");
#[cfg(test)]
const DASHBOARD_STATE_SOURCE: &str =
    include_str!("../../../../dashboard/src/lib/dashboard-state.ts");
#[cfg(test)]
const DASHBOARD_UTILS_SOURCE: &str = include_str!("../../../../dashboard/src/lib/utils.ts");
#[cfg(test)]
const DASHBOARD_PRIMITIVES_SOURCE: &str =
    include_str!("../../../../dashboard/src/primitives/index.tsx");
#[cfg(test)]
const DASHBOARD_CHARTER_CHECK_SOURCE: &str =
    include_str!("../../../../dashboard/scripts/check-dashboard-charter.ts");
async fn health(State(state): State<HttpState>) -> Json<Health> {
    tracing::info!(
        code = "MCP_HTTP_HEALTH",
        "tool.invocation kind=health transport=http"
    );
    let active_sessions = state.session_manager.sessions.read().await.len();
    emit_http_active_sessions(active_sessions);
    Json(
        state
            .health_service
            .health_payload_with_http_sessions(Some(active_sessions)),
    )
}

async fn shutdown(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    let active_sessions = state.session_manager.sessions.read().await.len();
    emit_http_active_sessions(active_sessions);
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>");
    let drain = state.drain_state.mark_draining("http_shutdown");
    let shutdown_on_drop = state
        .active_http_sockets
        .begin_shutdown_on_drop("http_shutdown");
    tracing::warn!(
        code = "MCP_HTTP_SHUTDOWN_DRAIN_STARTED",
        pid = std::process::id(),
        active_sessions,
        user_agent,
        drain = ?drain,
        shutdown_on_drop = ?shutdown_on_drop,
        delay_ms = DRAIN_RESPONSE_GRACE_TIMEOUT.as_millis(),
        "HTTP shutdown request accepted and daemon drain state marked before cancellation"
    );
    let shutdown_cancel = state.shutdown_cancel.clone();
    tokio::spawn(async move {
        time::sleep(DRAIN_RESPONSE_GRACE_TIMEOUT).await;
        shutdown_cancel.cancel();
    });
    tracing::warn!(
        code = "MCP_HTTP_SHUTDOWN_REQUESTED",
        pid = std::process::id(),
        active_sessions,
        user_agent,
        "authenticated HTTP daemon shutdown requested"
    );
    tracing::info!(
        code = "MCP_SHUTDOWN_GRACEFUL",
        source = "http_shutdown",
        pid = std::process::id(),
        delay_ms = DRAIN_RESPONSE_GRACE_TIMEOUT.as_millis(),
        "HTTP shutdown endpoint scheduled daemon shutdown after drain grace"
    );
    (
        StatusCode::ACCEPTED,
        [(header::CONNECTION, HeaderValue::from_static("close"))],
        Json(serde_json::json!({
            "ok": true,
            "pid": std::process::id(),
            "shutdown": "requested",
            "drain": drain,
            "active_sessions_before_shutdown": active_sessions,
            "session_close": "scheduled_by_daemon_supervisor",
        })),
    )
        .into_response()
}

async fn force_connection_close(mut response: Response) -> Response {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

async fn refuse_mcp_while_draining(
    State(state): State<HttpState>,
    request: Request<Body>,
    next: middleware::Next,
) -> Response {
    let path = request.uri().path().to_owned();
    if !path.starts_with("/mcp") {
        return next.run(request).await;
    }
    let drain_snapshot = state.drain_state.snapshot();
    if !drain_snapshot.draining && !state.shutdown_cancel.is_cancelled() {
        return next.run(request).await;
    }
    let snapshot = if drain_snapshot.draining {
        drain_snapshot
    } else {
        state.drain_state.mark_draining("shutdown_token")
    };
    tracing::warn!(
        code = synapse_core::error_codes::DAEMON_RESTARTING,
        path = %path,
        method = %request.method(),
        drain = ?snapshot,
        "HTTP MCP request refused because daemon is draining for restart"
    );
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "code": synapse_core::error_codes::DAEMON_RESTARTING,
            "retryable": true,
            "path": path,
            "drain": snapshot,
            "message": "daemon is restarting; initialize a new MCP session after the replacement daemon is healthy"
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

async fn events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<sse::EventsQuery>,
) -> Response {
    state.sse_state.open(&headers, query)
}

async fn publish_event(
    State(state): State<HttpState>,
    Json(request): Json<sse::PublishRequest>,
) -> Response {
    state.sse_state.publish(request)
}

async fn event_stats(
    State(state): State<HttpState>,
    Query(query): Query<sse::StatsQuery>,
) -> Response {
    state.sse_state.stats(&query)
}

/// Push-telemetry ingress (#899): spawned agents POST their native hook /
/// notify payloads here; the daemon normalizes them into `CF_AGENT_EVENTS`
/// rows. Authentication is enforced by the surrounding bearer middleware.
async fn agent_events_ingest(
    State(state): State<HttpState>,
    Query(query_pairs): Query<Vec<(String, String)>>,
    request: Request<Body>,
) -> Response {
    use crate::server::agent_event_ingress as ingress;

    let identity = match ingress::validate_ingress_identity(&query_pairs) {
        Ok(identity) => identity,
        Err(refusal) => return agent_events_refusal_response(&refusal),
    };
    let body = match axum::body::to_bytes(
        request.into_body(),
        ingress::MAX_AGENT_EVENT_INGRESS_BODY_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            return agent_events_refusal_response(&ingress::refuse_oversized_or_unreadable_body(
                &identity.spawn_id,
                &error.to_string(),
            ));
        }
    };
    match ingress::ingest_agent_event(&state.agent_events_db, &identity, &body) {
        Ok((readback, record)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "accepted": true,
                "kind": record.kind,
                "ts_ns": readback.ts_ns,
                "seq": readback.seq,
            })),
        )
            .into_response(),
        Err(refusal) => agent_events_refusal_response(&refusal),
    }
}

fn agent_events_refusal_response(
    refusal: &crate::server::agent_event_ingress::AgentEventIngressRefusal,
) -> Response {
    let status =
        StatusCode::from_u16(refusal.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(refusal.response_body())).into_response()
}

/// Codex app-server bridge: the PowerShell runner POSTs server-to-client
/// app-server requests here, this route blocks on the durable Synapse approval
/// row, then returns the app-server response payload the runner should send.
async fn codex_app_server_request(
    State(state): State<HttpState>,
    Json(request): Json<crate::server::codex_app_server_bridge::CodexAppServerRequestEnvelope>,
) -> Response {
    match crate::server::codex_app_server_bridge::handle_codex_app_server_request(
        &state.agent_events_db,
        request,
    )
    .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => {
            let status = StatusCode::from_u16(error.http_status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(error.response_body())).into_response()
        }
    }
}

/// Process-lifetime acceptance/rejection counters for the ingress, proving
/// "no silent drops" (#899 acceptance).
async fn agent_events_ingress_stats() -> Response {
    Json(crate::server::agent_event_ingress::ingress_stats()).into_response()
}

async fn agent_transcripts_ingest_stats() -> Response {
    Json(serde_json::json!({
        "spawn_dir": crate::server::agent_transcripts::ingest_stats(),
        "ambient": crate::server::ambient_agents::ingest_stats(),
    }))
    .into_response()
}

fn spawn_server(
    listener: TcpListener,
    app: Router,
    shutdown_cancel: CancellationToken,
    active_http_sockets: ActiveHttpSockets,
) -> JoinHandle<io::Result<()>> {
    tokio::spawn(async move {
        #[cfg(windows)]
        let listener = TrackedTcpListener {
            inner: listener,
            sockets: active_http_sockets,
        };
        #[cfg(not(windows))]
        let _ = active_http_sockets;
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_cancel.cancelled_owned().await })
            .await
    })
}

async fn handle_http_accept_error(error: io::Error) {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) {
        return;
    }
    tracing::error!(code = "MCP_HTTP_ACCEPT_ERROR", error = %error, "HTTP accept error");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

async fn wait_for_server_stop(
    server_task: &mut ShutdownTaskOwner<io::Result<()>>,
    source: &'static str,
) -> anyhow::Result<()> {
    wait_for_server_stop_with_timeouts(
        server_task,
        source,
        HTTP_SERVER_TASK_STOP_TIMEOUT,
        HTTP_SERVER_TASK_ABORT_TIMEOUT,
    )
    .await
}

async fn wait_for_server_stop_with_timeouts(
    server_task: &mut ShutdownTaskOwner<io::Result<()>>,
    source: &'static str,
    stop_timeout: Duration,
    abort_timeout: Duration,
) -> anyhow::Result<()> {
    let started = Instant::now();
    tracing::info!(
        code = "MCP_HTTP_SERVER_STOP_WAIT",
        source,
        timeout_ms = stop_timeout.as_millis(),
        "waiting for HTTP listener task to stop"
    );
    match tokio::time::timeout(stop_timeout, &mut *server_task).await {
        Ok(result) => {
            let stopped = result
                .context("join stopped HTTP MCP transport")
                .and_then(|result| result.context("stop HTTP MCP transport"));
            stopped?;
            tracing::info!(
                code = "MCP_HTTP_SERVER_STOPPED",
                source,
                elapsed_ms = started.elapsed().as_millis(),
                "HTTP listener task stopped"
            );
        }
        Err(_elapsed) => {
            server_task.abort();
            tracing::warn!(
                code = "MCP_HTTP_SHUTDOWN_TIMEOUT",
                source,
                timeout_ms = stop_timeout.as_millis(),
                elapsed_ms = started.elapsed().as_millis(),
                "HTTP transport did not stop within shutdown timeout"
            );
            let terminal_after_abort = match tokio::time::timeout(abort_timeout, &mut *server_task)
                .await
            {
                Ok(result) => match result {
                    Ok(Ok(())) => {
                        tracing::info!(
                            code = "MCP_HTTP_SERVER_STOPPED_AFTER_ABORT",
                            source,
                            elapsed_ms = started.elapsed().as_millis(),
                            "HTTP listener task stopped after abort request"
                        );
                        "listener returned success only after its graceful deadline and abort request"
                    }
                    Ok(Err(error)) => {
                        return Err(error).context(format!(
                            "stop HTTP MCP transport after missing the {}ms graceful deadline and requesting abort",
                            stop_timeout.as_millis()
                        ));
                    }
                    Err(join_error) if join_error.is_cancelled() => {
                        tracing::warn!(
                            code = "MCP_HTTP_SERVER_ABORTED",
                            source,
                            elapsed_ms = started.elapsed().as_millis(),
                            "HTTP listener task aborted after shutdown timeout"
                        );
                        "listener reached a cancelled terminal join only after its graceful deadline and abort request"
                    }
                    Err(join_error) => {
                        return Err(join_error).context(format!(
                            "join HTTP MCP transport after missing the {}ms graceful deadline and requesting abort",
                            stop_timeout.as_millis()
                        ));
                    }
                },
                Err(_elapsed) => {
                    anyhow::bail!(
                        "HTTP listener task did not stop after abort request within {}ms",
                        abort_timeout.as_millis()
                    );
                }
            };
            anyhow::bail!(
                "HTTP listener task missed its {}ms graceful shutdown deadline; {terminal_after_abort}",
                stop_timeout.as_millis()
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn wait_for_shutdown_signal(phase: &'static str) -> anyhow::Result<()> {
    let mut ctrl_break = tokio::signal::windows::ctrl_break()
        .with_context(|| format!("register ctrl-break handler {phase}"))?;
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.with_context(|| format!("wait for ctrl-c {phase}"))?;
        }
        received = ctrl_break.recv() => {
            if received.is_none() {
                anyhow::bail!("ctrl-break stream ended while waiting for shutdown signal {phase}");
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
async fn wait_for_shutdown_signal(phase: &'static str) -> anyhow::Result<()> {
    tokio::signal::ctrl_c()
        .await
        .with_context(|| format!("wait for ctrl-c {phase}"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use crate::test_support;
    use anyhow::Context as _;
    use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
    use rmcp::transport::streamable_http_server::session::SessionManager as _;
    use synapse_action::{ActionBackend, ActionEmitter, RecordingBackend};
    use synapse_core::{Action, Backend, Key, KeyCode, SCHEMA_VERSION};

    use super::*;

    const TEST_RESET_REASON: &str = "http_transport_lease_test_reset";

    #[test]
    fn lifetime_unlock_requires_every_physical_http_owner_readback() {
        let all_quiescent = HttpLifetimeOwnerReadback {
            authority_finalizers_quiescent: true,
            session_input_owners_quiescent: true,
            session_manager_quiescent: true,
            active_socket_owners_quiescent: true,
            server_dispatch_quiescent: true,
            background_tasks_quiescent: true,
            m2_emitter_safe: true,
            activity_owners_quiescent: true,
            win_event_shutdown_history_quiescent: true,
            storage_service_owners_quiescent: true,
            operator_hotkey_quiescent: true,
            operator_panic_k2_tasks_quiescent: true,
        };
        assert!(all_quiescent.safe_to_unlock());
        assert!(http_lifetime_locks_safe_to_close(
            all_quiescent,
            0,
            true,
            true,
            true
        ));
        assert!(
            !http_lifetime_locks_safe_to_close(all_quiescent, 1, true, true, true),
            "any retained desktop-worker exact owner must gate HTTP lifetime-lock release"
        );
        assert!(
            !http_lifetime_locks_safe_to_close(all_quiescent, 0, false, true, true),
            "any retained shutdown-task incident, including a reaped owner with erased output, must gate HTTP lifetime-lock release"
        );
        assert!(
            !http_lifetime_locks_safe_to_close(all_quiescent, 0, true, false, true),
            "any unresolved exact shell child/job owner must gate HTTP lifetime-lock release"
        );
        assert!(
            !http_lifetime_locks_safe_to_close(all_quiescent, 0, true, true, false),
            "any retained recorder task or unresolved Drop producer must gate HTTP lifetime-lock release"
        );

        macro_rules! rejects_false_field {
            ($field:ident) => {{
                let mut readback = all_quiescent;
                readback.$field = false;
                assert!(
                    !readback.safe_to_unlock(),
                    "{} must gate lifetime-lock release",
                    stringify!($field)
                );
            }};
        }
        rejects_false_field!(authority_finalizers_quiescent);
        rejects_false_field!(session_input_owners_quiescent);
        rejects_false_field!(session_manager_quiescent);
        rejects_false_field!(active_socket_owners_quiescent);
        rejects_false_field!(server_dispatch_quiescent);
        rejects_false_field!(background_tasks_quiescent);
        rejects_false_field!(m2_emitter_safe);
        rejects_false_field!(activity_owners_quiescent);
        rejects_false_field!(win_event_shutdown_history_quiescent);
        rejects_false_field!(storage_service_owners_quiescent);
        rejects_false_field!(operator_hotkey_quiescent);
        rejects_false_field!(operator_panic_k2_tasks_quiescent);
    }

    #[test]
    fn storage_owner_sentinel_detects_hidden_service_clones() {
        let owner = Arc::new(());
        assert!(m3_storage_owner_readback(&owner).owners_quiescent);

        let hidden_owner = Arc::clone(&owner);
        let live = m3_storage_owner_readback(&owner);
        assert!(!live.owners_quiescent);
        assert_eq!(live.strong_owner_count, 2);

        drop(hidden_owner);
        assert!(m3_storage_owner_readback(&owner).owners_quiescent);
    }

    #[tokio::test(start_paused = true)]
    async fn background_task_abort_join_has_a_separate_bounded_verdict() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocking = tokio::task::spawn_blocking(move || {
            started_tx.send(()).expect("publish real task start");
            release_rx.recv().expect("receive real task release");
        });
        started_rx.recv().expect("real blocking task started");
        let later_task_ran = Arc::new(AtomicBool::new(false));
        let later_task_state = Arc::clone(&later_task_ran);
        let later = tokio::spawn(async move {
            later_task_state.store(true, Ordering::Release);
        });
        let drain = tokio::spawn(drain_http_background_tasks(vec![
            own_http_background_task("non_cooperative_real_task", blocking),
            own_http_background_task("later_real_task", later),
        ]));

        tokio::task::yield_now().await;
        time::advance(HTTP_BACKGROUND_TASK_STOP_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        time::advance(HTTP_BACKGROUND_TASK_ABORT_TIMEOUT + Duration::from_millis(1)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let bounded_drain_finished = drain.is_finished();
        release_tx.send(()).expect("release real blocking task");
        let report = drain.await.expect("join bounded drain supervisor");

        assert!(
            bounded_drain_finished,
            "drain supervisor must finish before the non-cooperative task is externally released"
        );
        assert!(!report.owners_quiescent(), "{report:?}");
        assert_eq!(
            report.still_live_task_names,
            vec!["non_cooperative_real_task"]
        );
        let error = report
            .verdict()
            .expect_err("non-cooperative task must reject graceful drain");
        let detail = error.to_string();
        assert!(detail.contains("non_cooperative_real_task"), "{detail}");
        assert!(detail.contains("did not join"), "{detail}");
        assert!(
            later_task_ran.load(Ordering::Acquire),
            "a failed earlier join must not suppress later task observation"
        );
    }

    #[tokio::test]
    async fn listener_abort_join_is_terminal_but_never_restores_a_graceful_verdict() {
        let task = tokio::spawn(std::future::pending::<io::Result<()>>());
        let mut owner = ShutdownTaskOwner::new("test_http_listener_deadline", task);

        let error = wait_for_server_stop_with_timeouts(
            &mut owner,
            "test_deadline",
            Duration::ZERO,
            Duration::from_secs(5),
        )
        .await
        .expect_err("missing the graceful deadline must remain non-graceful");

        assert!(owner.terminal_join_observed());
        assert!(
            error
                .to_string()
                .contains("missed its 0ms graceful shutdown deadline"),
            "{error:#}"
        );
        // The test has incorporated the terminal error into its verdict. The
        // exact cancelled join is safe to acknowledge even though the process
        // result remains non-graceful.
        owner.acknowledge_terminal_outcome();
    }

    #[tokio::test]
    async fn shutdown_closes_every_real_local_session_and_rereads_empty_manager() {
        let manager = LocalSessionManager::default();
        let mut transports = Vec::new();
        let mut expected_ids = BTreeSet::new();
        for _ in 0..3 {
            let (session_id, transport) = manager
                .create_session()
                .await
                .map_err(|error| anyhow::anyhow!("create real local session: {error}"))
                .unwrap();
            expected_ids.insert(session_id.as_ref().to_owned());
            transports.push(transport);
        }
        assert_eq!(active_http_session_ids(&manager).await, expected_ids);

        let report = close_active_mcp_sessions_for_shutdown(&manager, "test_real_sessions").await;
        let manager_after = active_http_session_ids(&manager).await;
        drop(transports);

        assert_eq!(report.sessions_before, 3);
        assert_eq!(report.close_attempted, 3);
        assert_eq!(report.failure_count, 0, "{report:?}");
        assert_eq!(report.close_succeeded + report.already_terminated, 3);
        assert!(report.sessions_after == 0 && report.session_ids_after.is_empty());
        assert!(manager_after.is_empty());
    }

    #[test]
    fn http_transport_diagnostics_detail_names_request_counters_and_hint() {
        let detail =
            http_transport_diagnostics_detail_from_snapshot(HttpTransportDiagnosticsSnapshot {
                accepted_sockets_total: 7,
                accepted_sockets_current: 2,
                mcp_request_started_total: 11,
                mcp_request_completed_total: 10,
                mcp_request_in_flight: 1,
                mcp_request_error_status_total: 3,
                last_event: Some(HttpMcpTransportEvent {
                    request_id: 11,
                    phase: "completed",
                    method: "POST".to_owned(),
                    path: "/mcp".to_owned(),
                    status_code: Some(500),
                    elapsed_ms: Some(42),
                    unix_ms: 1234,
                }),
            });

        assert!(detail.contains("request_started_total:11"));
        assert!(detail.contains("request_completed_total:10"));
        assert!(detail.contains("request_in_flight:1"));
        assert!(detail.contains("request_error_status_total:3"));
        assert!(detail.contains("accepted_sockets_current:2"));
        assert!(detail.contains("request_id:11 phase:completed"));
        assert!(detail.contains("client send errors"));
        assert!(detail.contains("daemon HTTP middleware"));
    }

    #[test]
    fn shutdown_socket_syscall_success_waits_for_final_registry_readback() {
        let report = ActiveHttpSocketShutdownReport {
            reason: "test",
            tracked_before: 1,
            shutdown_attempted: 1,
            shutdown_succeeded: 1,
            failure_count: 0,
            tracked_after_shutdown_attempt: 1,
            sockets: Vec::new(),
            failures: Vec::new(),
        };
        let mut failures = HttpShutdownFailures::default();

        failures.inspect_socket_shutdown(&report);
        assert!(
            failures.is_empty(),
            "a stream remains registered until its connection task drops"
        );

        failures.inspect_final_socket_count(Ok(1));
        assert_eq!(failures.failures.len(), 1);
        assert_eq!(failures.failures[0].phase, "socket_registry_readback");
    }

    #[test]
    fn shutdown_input_cleanup_allows_operator_owned_global_lease() {
        let report = DaemonShutdownInputCleanupReport {
            reason: "test",
            authority_finalizer_drain_readback: None,
            authority_finalizer_drain_error: None,
            active_sessions_before: 0,
            live_spawn_snapshot_read_before: true,
            live_spawn_snapshot_error: None,
            live_spawn_sessions_before: 0,
            close_candidate_sessions_before: 0,
            input_owner_snapshot_read_before: true,
            input_owner_session_ids_before: Vec::new(),
            shutdown_sessions_before: 0,
            shutdown_session_ids: Vec::new(),
            cleaned_sessions: 0,
            session_cleanup_timeout_ms: u64::try_from(
                DAEMON_SESSION_INPUT_CLEANUP_TIMEOUT.as_millis(),
            )
            .unwrap_or(u64::MAX),
            session_cleanup_timeouts: Vec::new(),
            orphan_lease_owner_cleanup: None,
            final_lease_held: true,
            final_lease_owner_session_id: Some(
                synapse_action::OPERATOR_LEASE_OWNER_SESSION_ID.to_owned(),
            ),
            final_lease_is_operator: true,
            lease_still_held_after_cleanup: false,
            input_owner_snapshot_read_after: true,
            input_owner_session_ids_after: Vec::new(),
            input_owner_snapshot_errors: Vec::new(),
            failure_count: 0,
            session_reports: Vec::new(),
        };
        let mut failures = HttpShutdownFailures::default();

        failures.inspect_input_cleanup(&report);

        assert!(failures.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_session_input_cleanup_timeout_is_sticky_and_does_not_block_next_attempt() {
        let (timed_out, did_time_out) =
            await_daemon_session_input_cleanup("wedged-session", "test", std::future::pending())
                .await;
        assert!(did_time_out, "{timed_out:?}");
        assert!(timed_out.failed, "{timed_out:?}");
        assert_eq!(timed_out.session_id, "wedged-session");
        assert!(
            timed_out
                .error_message
                .as_deref()
                .is_some_and(|detail| detail.contains("completion remains unproven")),
            "{timed_out:?}"
        );

        let completed = crate::server::session_lifecycle::SessionShutdownInputCleanupReport {
            session_id: "next-session".to_owned(),
            reason: "test".to_owned(),
            ..Default::default()
        };
        let (next, next_timed_out) = await_daemon_session_input_cleanup(
            "next-session",
            "test",
            std::future::ready(completed),
        )
        .await;
        assert!(!next_timed_out, "{next:?}");
        assert!(!next.failed, "{next:?}");
        assert_eq!(next.session_id, "next-session");
    }

    #[test]
    fn shutdown_failure_aggregation_retains_every_failed_phase() {
        let session_report = McpSessionShutdownCloseReport {
            reason: "test",
            sessions_before: 2,
            close_attempted: 2,
            close_succeeded: 1,
            already_terminated: 0,
            failure_count: 1,
            session_ids: vec!["session-a".to_owned(), "session-b".to_owned()],
            failures: vec!["session-b: close failed".to_owned()],
            sessions_after: 1,
            session_ids_after: vec!["session-b".to_owned()],
        };
        let socket_report = ActiveHttpSocketShutdownReport {
            reason: "test",
            tracked_before: 1,
            shutdown_attempted: 1,
            shutdown_succeeded: 0,
            failure_count: 1,
            tracked_after_shutdown_attempt: 1,
            sockets: Vec::new(),
            failures: vec!["raw_socket=7 wsa_error=10038".to_owned()],
        };
        let input_report = DaemonShutdownInputCleanupReport {
            reason: "test",
            authority_finalizer_drain_readback: None,
            authority_finalizer_drain_error: Some(
                "authority transaction admission lock poisoned".to_owned(),
            ),
            active_sessions_before: 1,
            live_spawn_snapshot_read_before: false,
            live_spawn_snapshot_error: Some("session registry lock poisoned".to_owned()),
            live_spawn_sessions_before: 0,
            close_candidate_sessions_before: 1,
            input_owner_snapshot_read_before: true,
            input_owner_session_ids_before: vec!["session-b".to_owned()],
            shutdown_sessions_before: 1,
            shutdown_session_ids: vec!["session-b".to_owned()],
            cleaned_sessions: 1,
            session_cleanup_timeout_ms: u64::try_from(
                DAEMON_SESSION_INPUT_CLEANUP_TIMEOUT.as_millis(),
            )
            .unwrap_or(u64::MAX),
            session_cleanup_timeouts: Vec::new(),
            orphan_lease_owner_cleanup: None,
            final_lease_held: true,
            final_lease_owner_session_id: Some("session-b".to_owned()),
            final_lease_is_operator: false,
            lease_still_held_after_cleanup: true,
            input_owner_snapshot_read_after: true,
            input_owner_session_ids_after: vec!["session-b".to_owned()],
            input_owner_snapshot_errors: Vec::new(),
            failure_count: 1,
            session_reports: Vec::new(),
        };
        let mut failures = HttpShutdownFailures::default();

        failures.inspect_session_close(&session_report);
        failures.inspect_socket_shutdown(&socket_report);
        failures.inspect_input_cleanup(&input_report);
        failures.inspect_final_socket_count(Err("registry lock poisoned".to_owned()));
        failures.inspect_final_session_ids(&BTreeSet::from(["late-session".to_owned()]));
        failures.inspect_result("m2_emitter_drain", Err(anyhow::anyhow!("M2 timeout")));

        let phases = failures
            .failures
            .iter()
            .map(|failure| failure.phase)
            .collect::<Vec<_>>();
        assert_eq!(
            phases,
            vec![
                "session_close",
                "socket_shutdown",
                "input_cleanup",
                "socket_registry_readback",
                "session_manager_readback",
                "m2_emitter_drain",
            ]
        );
        let error = failures
            .into_result()
            .expect_err("any failed shutdown phase must prevent a success verdict");
        let detail = error.to_string();
        for phase in phases {
            assert!(detail.contains(phase), "aggregate omitted phase {phase}");
        }
    }

    #[test]
    fn dashboard_host_gate_accepts_loopback_only() {
        assert!(dashboard_host_allowed("127.0.0.1:7700"));
        assert!(dashboard_host_allowed("localhost:7700"));
        assert!(dashboard_host_allowed("[::1]:7700"));
        assert!(!dashboard_host_allowed("192.168.1.20:7700"));
        assert!(!dashboard_host_allowed("evil.example"));
    }

    #[test]
    fn dashboard_html_does_not_embed_bearer_material() {
        assert!(DASHBOARD_HTML.contains("Synapse Command Center"));
        assert!(!DASHBOARD_HTML.contains("Authorization"));
        assert!(!DASHBOARD_HTML.contains("Bearer"));
        assert!(!DASHBOARD_HTML.contains("SYNAPSE_BEARER_TOKEN"));
        assert!(!DASHBOARD_CSS.contains("Authorization"));
        assert!(!DASHBOARD_CSS.contains("Bearer"));
        assert!(!DASHBOARD_CSS.contains("SYNAPSE_BEARER_TOKEN"));
        assert!(!DASHBOARD_JS.contains("Authorization"));
        assert!(!DASHBOARD_JS.contains("Bearer"));
        assert!(!DASHBOARD_JS.contains("SYNAPSE_BEARER_TOKEN"));
    }

    #[test]
    fn dashboard_html_uses_external_assets_without_inline_blocks() {
        assert!(DASHBOARD_HTML.contains(&format!("/dashboard/assets/{DASHBOARD_CSS_FILE}")));
        assert!(DASHBOARD_HTML.contains(&format!("/dashboard/assets/{DASHBOARD_JS_FILE}")));
        assert!(DASHBOARD_HTML.contains("id=\"root\""));
        assert!(DASHBOARD_HTML.contains("<script type=\"module\""));
        assert!(!DASHBOARD_HTML.contains("<style"));
        assert!(!DASHBOARD_HTML.contains("src=\"http://"));
        assert!(!DASHBOARD_HTML.contains("src=\"https://"));
        assert!(!DASHBOARD_HTML.contains("href=\"http://"));
        assert!(!DASHBOARD_HTML.contains("href=\"https://"));
        assert!(!DASHBOARD_HTML.contains("<script>"));
    }

    #[test]
    fn dashboard_bundle_contains_asset_reload_contract() {
        assert!(DASHBOARD_STATE_SOURCE.contains("dashboardAssetReloadDecision"));
        assert!(DASHBOARD_STATE_SOURCE.contains("invalid_server_asset_id"));
        assert!(DASHBOARD_STATE_SOURCE.contains("_synapse_dashboard_asset"));
        assert!(DASHBOARD_APP_SOURCE.contains("claimDashboardAssetReload"));
        assert!(DASHBOARD_JS.contains("_synapse_dashboard_asset"));
        assert!(DASHBOARD_JS.contains("synapse.dashboard.asset-reload"));
        assert!(DASHBOARD_JS.contains("invalid_server_asset_id"));
    }

    #[test]
    fn dashboard_event_scope_filters_are_panel_scoped() {
        let agent_state = dashboard_scope_test_event(
            EventSource::System,
            crate::server::agent_state::AGENT_STATE_EVENT_KIND,
        );
        let profile_changed = dashboard_scope_test_event(EventSource::System, "profile-changed");
        let audit = dashboard_scope_test_event(EventSource::ActionEmitter, "command_finished");
        let filesystem = dashboard_scope_test_event(EventSource::Filesystem, "file_changed");
        let approval_request = dashboard_scope_test_event(
            EventSource::System,
            crate::server::APPROVAL_REQUEST_EVENT_KIND,
        );
        let approval_decision = dashboard_scope_test_event(
            EventSource::System,
            crate::server::APPROVAL_DECISION_EVENT_KIND,
        );
        let approval_timeout = dashboard_scope_test_event(
            EventSource::System,
            crate::server::APPROVAL_TIMEOUT_EVENT_KIND,
        );

        assert!(dashboard_scope_matches(
            DashboardEventScope::Fleet,
            &agent_state
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Agent,
            &profile_changed
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Tasks,
            &agent_state
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Fleet,
            &approval_request
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Fleet,
            &approval_decision
        ));
        assert!(dashboard_scope_matches(
            DashboardEventScope::Tasks,
            &approval_timeout
        ));
        assert!(dashboard_scope_matches(DashboardEventScope::Audit, &audit));
        assert!(dashboard_scope_matches(
            DashboardEventScope::System,
            &filesystem
        ));

        assert!(!dashboard_scope_matches(DashboardEventScope::Fleet, &audit));
        assert!(!dashboard_scope_matches(
            DashboardEventScope::Tasks,
            &profile_changed
        ));
        assert!(!dashboard_scope_matches(
            DashboardEventScope::Agent,
            &approval_request
        ));
        assert!(!dashboard_scope_matches(
            DashboardEventScope::Audit,
            &filesystem
        ));
    }

    #[test]
    fn dashboard_event_url_keeps_subscription_id_for_last_event_id_replay() {
        assert_eq!(
            dashboard_event_url("sub-01234567-89ab-cdef"),
            "/dashboard/events?subscription_id=sub-01234567-89ab-cdef"
        );
    }

    #[test]
    fn dashboard_bundle_contains_terminal_ws_contract() {
        assert!(DASHBOARD_APP_SOURCE.contains("/dashboard/agent-terminal/"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_CLIENT_PAUSE"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_CLIENT_RESUME"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_CLIENT_INPUT"));
        assert!(DASHBOARD_APP_SOURCE.contains("TERMINAL_SERVER_OUTPUT"));
        assert!(DASHBOARD_JS.contains("/dashboard/agent-terminal/"));
    }

    #[test]
    fn dashboard_asciicast_parser_returns_cumulative_v3_events() {
        let text = concat!(
            "{\"version\":3,\"term\":{\"cols\":80,\"rows\":24},\"timestamp\":1700000000}\n",
            "[0.25,\"o\",\"first\"]\n",
            "[0.75,\"m\",\"tool_call_started\"]\n",
            "[0.5,\"x\",\"7\"]\n"
        );
        let replay = dashboard_parse_asciicast_text(text, false).expect("valid asciicast");

        assert_eq!(replay.header["version"], 3);
        assert_eq!(replay.returned_event_count, 3);
        assert_eq!(replay.output_event_count, 1);
        assert_eq!(replay.marker_event_count, 1);
        assert_eq!(replay.exit_code, Some(7));
        assert!((replay.duration_secs - 1.5).abs() < f64::EPSILON);
        assert_eq!(replay.events[0].time_secs, 0.25);
        assert_eq!(replay.events[1].time_secs, 1.0);
        assert!(!replay.recording_truncated);
    }

    #[test]
    fn dashboard_asciicast_parser_declares_bounded_response_truncation() {
        let text = concat!(
            "{\"version\":3,\"term\":{\"cols\":80,\"rows\":24},\"timestamp\":1700000000}\n",
            "[0.25,\"o\",\"first\"]\n"
        );
        let replay = dashboard_parse_asciicast_text(text, true).expect("valid partial asciicast");

        assert_eq!(replay.returned_event_count, 1);
        assert!(replay.response_truncated);
        assert!(replay.recording_truncated);
        assert_eq!(replay.exit_code, None);
    }

    #[test]
    fn dashboard_recording_status_flags_crash_and_truncation() {
        let crashed = serde_json::json!({
            "status": "crashed",
            "truncated": true,
            "reason": "panic in worker",
        });
        let clean = serde_json::json!({
            "status": "finished",
            "truncated": false,
        });

        assert!(dashboard_capture_status_declares_truncation(&crashed));
        assert!(dashboard_capture_status_declares_crash(
            Some(&crashed),
            Some(0)
        ));
        assert!(dashboard_capture_status_declares_crash(None, Some(2)));
        assert!(!dashboard_capture_status_declares_truncation(&clean));
        assert!(!dashboard_capture_status_declares_crash(
            Some(&clean),
            Some(0)
        ));
    }

    #[test]
    fn dashboard_bundle_contains_session_replay_contract() {
        assert!(DASHBOARD_STATE_SOURCE.contains("/dashboard/agent-recordings/"));
        assert!(DASHBOARD_APP_SOURCE.contains("Session Replay"));
        assert!(DASHBOARD_APP_SOURCE.contains("fetchAgentRecording"));
        assert!(DASHBOARD_APP_SOURCE.contains("activeReplayEvent"));
        assert!(DASHBOARD_APP_SOURCE.contains("Recording ended without a complete exit event"));
        assert!(DASHBOARD_APP_SOURCE.contains("Exit/crash state is declared"));
    }

    fn dashboard_scope_matches(scope: DashboardEventScope, event: &synapse_core::Event) -> bool {
        let (filter, kinds, _) = dashboard_event_subscription(scope);
        (kinds.is_empty() || kinds.iter().any(|kind| kind == &event.kind)) && filter.matches(event)
    }

    fn dashboard_scope_test_event(source: EventSource, kind: &str) -> synapse_core::Event {
        synapse_core::Event {
            seq: 1,
            at: chrono::Utc::now(),
            source,
            kind: kind.to_owned(),
            data: serde_json::json!({}),
            correlations: Vec::new(),
        }
    }

    #[test]
    fn terminal_ws_resize_payload_accepts_text_and_json() {
        assert_eq!(
            terminal_ws_parse_resize(b"120x40").expect("text resize"),
            (120, 40)
        );
        assert_eq!(
            terminal_ws_parse_resize(br#"{"cols":100,"rows":32}"#).expect("json resize"),
            (100, 32)
        );
        assert!(terminal_ws_parse_resize(b"0x40").is_err());
        assert!(terminal_ws_parse_resize(b"120").is_err());
    }

    #[test]
    fn terminal_ws_paused_buffer_preserves_order_and_caps_floods() {
        let mut frames = VecDeque::new();
        let mut bytes = 0usize;
        let first = terminal_ws_frame(TERMINAL_WS_SERVER_OUTPUT, b"first");
        let second = terminal_ws_frame(TERMINAL_WS_SERVER_OUTPUT, b"second");

        terminal_ws_buffer_paused_frame(&mut frames, &mut bytes, first.clone())
            .expect("first paused frame should buffer");
        terminal_ws_buffer_paused_frame(&mut frames, &mut bytes, second.clone())
            .expect("second paused frame should buffer");

        assert_eq!(bytes, first.len() + second.len());
        assert_eq!(frames.pop_front(), Some(first.clone()));
        assert_eq!(frames.pop_front(), Some(second.clone()));

        let mut near_limit = TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX;
        let mut full = VecDeque::new();
        full.push_back(vec![TERMINAL_WS_SERVER_OUTPUT; near_limit]);
        assert!(terminal_ws_buffer_paused_frame(&mut full, &mut near_limit, vec![b'x']).is_err());
        assert_eq!(near_limit, TERMINAL_WS_PAUSED_BUFFER_BYTES_MAX);
        assert_eq!(full.len(), 1);
    }

    #[test]
    fn dashboard_session_feed_splits_terminal_unbound_history() {
        let source = serde_json::json!({
            "now_unix_ms": 10,
            "stale_after_ms": 300_000,
            "registry_entry_count": 1,
            "target_session_count": 0,
            "returned_count": 1,
            "input_lease_held": false,
            "sessions": [
                {
                    "session_id": "live-session",
                    "lifecycle": "live",
                    "agent_state": { "state": "idle" }
                }
            ],
            "unbound_agent_states": [
                {
                    "anchor": "agent-spawn-dead",
                    "spawn_id": "agent-spawn-dead",
                    "state": "dead",
                    "reason_code": "local_model_registry_row_missing",
                    "attention_class": "terminal_setup_failure"
                },
                {
                    "anchor": "agent-spawn-cleanup",
                    "spawn_id": "agent-spawn-cleanup",
                    "state": "dead",
                    "reason_code": "process_gone_without_exit_event",
                    "attention_class": "cleanup_required"
                },
                {
                    "anchor": "agent-spawn-stuck",
                    "spawn_id": "agent-spawn-stuck",
                    "state": "stuck",
                    "reason_code": "silent_timeout"
                },
                {
                    "anchor": "agent-spawn-acked-stuck",
                    "spawn_id": "agent-spawn-acked-stuck",
                    "state": "stuck",
                    "reason_code": "silent_timeout_unprobeable"
                },
                {
                    "anchor": "agent-spawn-needs-input",
                    "spawn_id": "agent-spawn-needs-input",
                    "state": "needs_input",
                    "reason_code": "permission_prompt"
                },
                "malformed-row"
            ]
        });

        let data = dashboard_primary_session_list_data(
            &source,
            Ok(BTreeSet::from(["agent-spawn-acked-stuck".to_owned()])),
        );
        let primary = data["unbound_agent_states"]
            .as_array()
            .expect("primary rows");
        let acknowledged = data["acknowledged_unbound_agent_states"]
            .as_array()
            .expect("acknowledged rows");
        let terminal = data["terminal_unbound_agent_states"]
            .as_array()
            .expect("terminal rows");

        assert_eq!(primary.len(), 4);
        assert_eq!(primary[0]["anchor"], "agent-spawn-cleanup");
        assert_eq!(primary[0]["attention_class"], "cleanup_required");
        assert_eq!(primary[1]["state"], "stuck");
        assert_eq!(primary[2]["state"], "needs_input");
        assert_eq!(primary[3], "malformed-row");
        assert_eq!(acknowledged.len(), 1);
        assert_eq!(acknowledged[0]["anchor"], "agent-spawn-acked-stuck");
        assert_eq!(
            acknowledged[0]["dashboard_attention_suppressed"]["reason"],
            "acknowledged_escalation"
        );
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0]["anchor"], "agent-spawn-dead");
        assert_eq!(terminal[0]["attention_class"], "terminal_setup_failure");
        assert_eq!(
            data["dashboard_unbound_agent_filter"]["acknowledged_unbound_agent_count"],
            1
        );
        assert_eq!(
            data["dashboard_unbound_agent_filter"]["terminal_unbound_agent_count"],
            1
        );
    }

    #[test]
    fn dashboard_security_headers_disallow_inline_script_and_eval() {
        let response = with_dashboard_security_headers(Html("").into_response());
        let csp = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .expect("CSP header present");
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains("style-src 'self'"));
        assert!(!csp.contains("'unsafe-inline'"));
        assert!(!csp.contains("'unsafe-eval'"));
        assert_eq!(
            response
                .headers()
                .get(HeaderName::from_static("x-content-type-options"))
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store, max-age=0")
        );
    }

    #[test]
    fn dashboard_source_uses_charter_guardrails() {
        let source = [
            DASHBOARD_APP_SOURCE,
            DASHBOARD_STATE_SOURCE,
            DASHBOARD_UTILS_SOURCE,
            DASHBOARD_PRIMITIVES_SOURCE,
        ]
        .join("\n");
        assert!(source.contains("stripTerminalSequences"));
        assert!(source.contains("ReactMarkdown"));
        assert!(source.contains("rehypeSanitize"));
        assert!(source.contains("RawValue"));
        assert!(source.contains("Section"));
        assert!(source.contains("/dashboard/state.json"));
        assert!(source.contains("cache: \"no-store\""));
        assert!(DASHBOARD_CHARTER_CHECK_SOURCE.contains("dangerouslySetInnerHTML"));
        assert!(DASHBOARD_CHARTER_CHECK_SOURCE.contains("insertAdjacentHTML"));
        assert!(DASHBOARD_CHARTER_CHECK_SOURCE.contains("every Section must declare questions"));
        assert!(
            DASHBOARD_CHARTER_CHECK_SOURCE.contains("RawValue disclosure must not default open")
        );
        assert!(!source.contains("dangerouslySetInnerHTML"));
        assert!(!source.contains(".innerHTML"));
        assert!(!source.contains("insertAdjacentHTML"));
        assert!(!source.contains("new Function"));
        assert!(!source.contains("eval("));
    }

    #[test]
    fn dashboard_local_model_spawn_params_force_local_model_kind() {
        let params = dashboard_local_model_spawn_params(DashboardLocalModelSpawnRequest {
            model_ref: " ollama-gemma4-e4b ".to_owned(),
            prompt: " write known result ".to_owned(),
            working_dir: Some(" C:\\code\\Synapse ".to_owned()),
            wait_timeout_ms: Some(300_000),
            hold_open_ms: Some(0),
        })
        .expect("valid dashboard local model spawn params");

        assert_eq!(params.cli, None);
        assert_eq!(params.kind, Some(crate::m4::ActSpawnAgentCli::LocalModel));
        assert_eq!(params.model_ref.as_deref(), Some("ollama-gemma4-e4b"));
        assert_eq!(params.prompt.as_deref(), Some("write known result"));
        assert_eq!(params.working_dir.as_deref(), Some("C:\\code\\Synapse"));
        assert_eq!(params.wait_timeout_ms, 300_000);
        assert_eq!(params.hold_open_ms, 0);
    }

    fn empty_dashboard_spawn_agent_request() -> DashboardSpawnAgentRequest {
        DashboardSpawnAgentRequest {
            fan_out: None,
            template_id: None,
            template_version: None,
            template_params: BTreeMap::new(),
            cli: None,
            kind: None,
            model: None,
            model_ref: None,
            prompt: None,
            target: None,
            working_dir: None,
            wait_timeout_ms: None,
            hold_open_ms: None,
            require_approval_gate: None,
        }
    }

    #[test]
    fn dashboard_spawn_agent_request_params_preserves_template_fanout() {
        let mut request = empty_dashboard_spawn_agent_request();
        request.fan_out = Some(5);
        request.template_id = Some(" issue923-template ".to_owned());
        request.template_version = Some(7);
        request
            .template_params
            .insert("task".to_owned(), "write-known-row".to_owned());

        let (fan_out, spawn) =
            dashboard_spawn_agent_request_params(request).expect("valid template spawn params");

        assert_eq!(fan_out, 5);
        assert_eq!(spawn.template_id.as_deref(), Some("issue923-template"));
        assert_eq!(spawn.template_version, Some(7));
        assert_eq!(
            spawn.template_params.get("task").map(String::as_str),
            Some("write-known-row")
        );
        assert_eq!(spawn.kind, None);
        assert_eq!(spawn.model_ref, None);
        assert_eq!(
            spawn.wait_timeout_ms,
            crate::m4::default_agent_spawn_wait_timeout_ms()
        );
        assert_eq!(
            spawn.hold_open_ms,
            crate::m4::default_agent_spawn_hold_open_ms()
        );
    }

    #[test]
    fn dashboard_spawn_agent_request_params_trims_direct_spawn_and_target() {
        let mut request = empty_dashboard_spawn_agent_request();
        request.kind = Some(crate::m4::ActSpawnAgentCli::Codex);
        request.model = Some(" gpt-5-codex ".to_owned());
        request.prompt = Some(" write known row ".to_owned());
        request.working_dir = Some(" C:\\code\\Synapse ".to_owned());
        request.target = Some(crate::m4::ActSpawnAgentTarget::Window {
            window_hwnd: 1116654,
        });
        request.wait_timeout_ms = Some(42_000);
        request.hold_open_ms = Some(0);

        let (fan_out, spawn) =
            dashboard_spawn_agent_request_params(request).expect("valid direct spawn params");

        assert_eq!(fan_out, 1);
        assert_eq!(spawn.kind, Some(crate::m4::ActSpawnAgentCli::Codex));
        assert_eq!(spawn.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(spawn.prompt.as_deref(), Some("write known row"));
        assert_eq!(spawn.working_dir.as_deref(), Some("C:\\code\\Synapse"));
        assert_eq!(
            spawn.target,
            Some(crate::m4::ActSpawnAgentTarget::Window {
                window_hwnd: 1116654
            })
        );
        assert_eq!(spawn.wait_timeout_ms, 42_000);
        assert_eq!(spawn.hold_open_ms, 0);
    }

    #[test]
    fn dashboard_spawn_agent_request_params_rejects_invalid_fanout() {
        for fan_out in [0, DASHBOARD_SPAWN_FAN_OUT_MAX + 1] {
            let mut request = empty_dashboard_spawn_agent_request();
            request.fan_out = Some(fan_out);
            let response = dashboard_spawn_agent_request_params(request)
                .expect_err("invalid dashboard fan-out should fail closed");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn dashboard_agent_kill_params_trim_id_and_keep_options() {
        let params = dashboard_agent_kill_params(DashboardAgentKillRequest {
            session_id: " agent-spawn-issue923 ".to_owned(),
            grace_ms: Some(0),
            interrupt_first: Some(false),
        })
        .expect("valid dashboard kill params");

        assert_eq!(params.session_id, "agent-spawn-issue923");
        assert_eq!(params.grace_ms, 0);
        assert!(!params.interrupt_first);
    }

    #[test]
    fn dashboard_agent_kill_params_reject_empty_id() {
        let response = dashboard_agent_kill_params(DashboardAgentKillRequest {
            session_id: "   ".to_owned(),
            grace_ms: None,
            interrupt_first: None,
        })
        .expect_err("empty dashboard kill id should fail closed");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn dashboard_agent_control_params_trim_selected_agent_ids() {
        let interrupt = dashboard_agent_interrupt_params(DashboardAgentLookupRequest {
            session_id: " agent-spawn-issue917 ".to_owned(),
        })
        .expect("valid dashboard interrupt params");
        let pause = dashboard_agent_pause_params(DashboardAgentLookupRequest {
            session_id: " session-issue917 ".to_owned(),
        })
        .expect("valid dashboard pause params");

        assert_eq!(interrupt.session_id, "agent-spawn-issue917");
        assert_eq!(pause.session_id, "session-issue917");
    }

    #[test]
    fn dashboard_agent_respawn_params_require_prompt() {
        let response = dashboard_agent_respawn_params(DashboardAgentRespawnRequest {
            session_id: "agent-spawn-issue917".to_owned(),
            prompt: "   ".to_owned(),
            carry_context: None,
            grace_ms: None,
        })
        .expect_err("empty dashboard respawn prompt should fail closed");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn dashboard_agent_respawn_params_trim_and_default_options() {
        let params = dashboard_agent_respawn_params(DashboardAgentRespawnRequest {
            session_id: " agent-spawn-issue917 ".to_owned(),
            prompt: " continue this work ".to_owned(),
            carry_context: None,
            grace_ms: None,
        })
        .expect("valid dashboard respawn params");

        assert_eq!(params.session_id, "agent-spawn-issue917");
        assert_eq!(params.prompt, "continue this work");
        assert!(params.carry_context);
        assert_eq!(params.grace_ms, DASHBOARD_AGENT_KILL_DEFAULT_GRACE_MS);
    }

    #[test]
    fn dashboard_control_lease_force_release_params_trim_owner() {
        let (owner_session_id, confirmed) = dashboard_control_lease_force_release_params(
            DashboardControlLeaseForceReleaseRequest {
                owner_session_id: " lease-owner-session ".to_owned(),
                confirmed: true,
            },
        )
        .expect("valid dashboard force-release params");

        assert_eq!(owner_session_id, "lease-owner-session");
        assert!(confirmed);
    }

    #[test]
    fn dashboard_control_lease_force_release_params_require_confirmation() {
        let response = dashboard_control_lease_force_release_params(
            DashboardControlLeaseForceReleaseRequest {
                owner_session_id: "lease-owner-session".to_owned(),
                confirmed: false,
            },
        )
        .expect_err("unconfirmed dashboard force-release should fail closed");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn dashboard_control_lease_handoff_params_trim_sessions_and_default_ttl() {
        let (from_session_id, to_session_id, ttl_ms) =
            dashboard_control_lease_handoff_params(DashboardControlLeaseHandoffRequest {
                from_session_id: " from-session ".to_owned(),
                to_session_id: " to-session ".to_owned(),
                ttl_ms: None,
            })
            .expect("valid dashboard handoff params");

        assert_eq!(from_session_id, "from-session");
        assert_eq!(to_session_id, "to-session");
        assert_eq!(ttl_ms, synapse_action::DEFAULT_LEASE_TTL_MS);
    }

    #[test]
    fn dashboard_control_lease_handoff_params_reject_bad_sessions_and_ttl() {
        let same_session =
            dashboard_control_lease_handoff_params(DashboardControlLeaseHandoffRequest {
                from_session_id: "same".to_owned(),
                to_session_id: " same ".to_owned(),
                ttl_ms: Some(synapse_action::DEFAULT_LEASE_TTL_MS),
            })
            .expect_err("same session handoff should fail closed");
        assert_eq!(same_session.status(), StatusCode::BAD_REQUEST);

        let bad_ttl = dashboard_control_lease_handoff_params(DashboardControlLeaseHandoffRequest {
            from_session_id: "from".to_owned(),
            to_session_id: "to".to_owned(),
            ttl_ms: Some(synapse_action::MIN_LEASE_TTL_MS - 1),
        })
        .expect_err("out-of-range dashboard handoff ttl should fail closed");
        assert_eq!(bad_ttl.status(), StatusCode::BAD_REQUEST);
    }

    fn test_session_state(name: &str) -> SessionState {
        SessionState::new(InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new(name, "0.0.0-test"),
        ))
    }

    fn test_store_error(error: SessionStoreError) -> anyhow::Error {
        anyhow::anyhow!("{error}")
    }

    fn empty_cdp_target_owners() -> crate::server::SharedCdpTargetOwners {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn empty_session_registry() -> crate::server::session_registry::SharedSessionRegistry {
        Arc::new(Mutex::new(
            crate::server::session_registry::SessionRegistry::default(),
        ))
    }

    #[test]
    fn shutdown_cleanup_session_ids_union_every_pre_cleanup_source() {
        let active = BTreeSet::from(["active".to_owned(), "both".to_owned()]);
        let live_spawns = BTreeSet::from(["both".to_owned(), "idle-spawn".to_owned()]);
        let close_candidates = BTreeSet::from(["close-failed".to_owned()]);
        let input_owners = BTreeSet::from(["input-owner".to_owned()]);

        let cleanup =
            shutdown_cleanup_session_ids(&active, &live_spawns, &close_candidates, &input_owners);

        assert_eq!(
            cleanup,
            BTreeSet::from([
                "active".to_owned(),
                "both".to_owned(),
                "close-failed".to_owned(),
                "idle-spawn".to_owned(),
                "input-owner".to_owned(),
            ])
        );
    }

    #[tokio::test]
    async fn synapse_mcp_session_store_round_trips_exact_keys_and_deletes() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
        let store = SynapseMcpSessionStore::new(
            Arc::clone(&db),
            Some(Duration::from_mins(5)),
            empty_session_registry(),
        );

        assert!(
            store
                .load("codex-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "unknown session should not load"
        );

        let state = test_session_state("codex-test");
        let neighboring_state = test_session_state("codex-test-neighbor");
        store
            .store("codex-session", &state)
            .await
            .map_err(test_store_error)?;
        let stored_rows = db.scan_cf_prefix(cf::CF_KV, &mcp_session_store_key("codex-session"))?;
        let stored_row = stored_rows
            .iter()
            .find(|(key, _value)| key == &mcp_session_store_key("codex-session"))
            .context("stored row should exist in CF_KV")?;
        let persisted = synapse_storage::decode_json::<PersistedMcpSessionState>(&stored_row.1)?;
        assert_eq!(persisted.state.initialize_params, state.initialize_params);

        store
            .store("codex-session-extra", &neighboring_state)
            .await
            .map_err(test_store_error)?;

        let loaded = store
            .load("codex-session")
            .await
            .map_err(test_store_error)?
            .context("stored session should load")?;
        assert_eq!(loaded.initialize_params, state.initialize_params);

        store
            .delete("codex-session")
            .await
            .map_err(test_store_error)?;
        assert!(
            store
                .load("codex-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "deleted session should not load"
        );
        assert!(
            store
                .load("codex-session-extra")
                .await
                .map_err(test_store_error)?
                .is_some(),
            "deleting one session should not delete a prefix-sharing neighbor"
        );

        Ok(())
    }

    #[tokio::test]
    async fn synapse_mcp_session_store_deletes_expired_rows() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
        let store = SynapseMcpSessionStore::new(
            Arc::clone(&db),
            Some(Duration::from_millis(1)),
            empty_session_registry(),
        );
        let key = mcp_session_store_key("expired-session");

        store
            .store("expired-session", &test_session_state("expired-test"))
            .await
            .map_err(test_store_error)?;
        assert!(
            db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "stored row should physically exist before expiry"
        );

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(
            store
                .load("expired-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "expired session should not load"
        );
        assert!(
            !db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "expired session row should be deleted from CF_KV"
        );

        Ok(())
    }

    #[tokio::test]
    async fn synapse_mcp_session_store_deletes_legacy_rows_without_ttl() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
        let store = SynapseMcpSessionStore::new(
            Arc::clone(&db),
            Some(Duration::from_mins(5)),
            empty_session_registry(),
        );
        let key = mcp_session_store_key("legacy-session");
        let legacy_state = test_session_state("legacy-test");
        let legacy_encoded = synapse_storage::encode_json(&legacy_state)?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(key.clone(), legacy_encoded)])?;

        assert!(
            db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "legacy row should physically exist before load"
        );

        assert!(
            store
                .load("legacy-session")
                .await
                .map_err(test_store_error)?
                .is_none(),
            "legacy row without persistent TTL metadata should not load"
        );
        assert!(
            !db.scan_cf_prefix(cf::CF_KV, &key)?
                .into_iter()
                .any(|(row_key, _value)| row_key == key),
            "legacy session row should be deleted from CF_KV"
        );

        Ok(())
    }

    #[tokio::test]
    async fn stale_session_cleanup_releases_absent_inputs_only() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let session_manager = Arc::new(LocalSessionManager::default());
        let (active_session_id, _active_transport) = session_manager
            .create_session()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let active_session_text = active_session_id.as_ref().to_owned();
        let stale_session_id = "stale-session".to_owned();
        let _prior = synapse_action::lease::force_clear("http_stale_inputs_test_reset");
        let _held = synapse_action::lease::try_acquire(&stale_session_id, Duration::from_secs(30));

        handle
            .with_session_id(Some(stale_session_id.clone()))
            .execute(Action::KeyDown {
                key: test_key("ctrl"),
                backend: Backend::Software,
            })
            .await?;
        handle
            .with_session_id(Some(active_session_text.clone()))
            .execute(Action::KeyDown {
                key: test_key("shift"),
                backend: Backend::Software,
            })
            .await?;

        let before_state = snapshot_handle.snapshot().await?;
        let before_ownership = handle.session_inputs_snapshot()?;
        let before_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_owner before_state={before_state:?} before_ownership={before_ownership:?} before_lease={before_lease:?} active_session_id={active_session_text}"
        );
        assert_eq!(
            before_lease.owner_session_id.as_deref(),
            Some(stale_session_id.as_str())
        );

        let cdp_target_owners = empty_cdp_target_owners();
        cleanup_stale_session_inputs_once(&handle, &session_manager, &cdp_target_owners).await;

        let after_state = snapshot_handle.snapshot().await?;
        let after_ownership = handle.session_inputs_snapshot()?;
        let after_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_owner after_state={after_state:?} after_ownership={after_ownership:?} after_lease={after_lease:?}"
        );

        assert_eq!(after_state.held_keys, vec![test_key("shift")]);
        assert!(!after_lease.held);
        assert!(
            after_ownership
                .sessions
                .iter()
                .any(|session| session.session_id == active_session_text),
            "active session ownership should be retained"
        );
        assert!(
            !after_ownership
                .sessions
                .iter()
                .any(|session| session.session_id == stale_session_id),
            "stale session ownership should be removed"
        );

        session_manager
            .close_session(&active_session_id)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        handle.execute(Action::ReleaseAll).await?;
        cancel.cancel();
        let final_snapshot = join.await?;
        assert!(final_snapshot.held_keys.is_empty());
        let _prior = synapse_action::lease::force_clear("http_stale_inputs_test_reset");

        Ok(())
    }

    #[tokio::test]
    async fn stale_session_cleanup_releases_absent_lease_without_inputs() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let session_manager = LocalSessionManager::default();
        let stale_session_id = "stale-lease-session";
        let _prior = synapse_action::lease::force_clear("http_stale_lease_test_reset");
        let _held = synapse_action::lease::try_acquire(stale_session_id, Duration::from_secs(30));

        let before_state = snapshot_handle.snapshot().await?;
        let before_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_lease before_state={before_state:?} before_lease={before_lease:?}"
        );

        let cdp_target_owners = empty_cdp_target_owners();
        cleanup_stale_session_inputs_once(&handle, &session_manager, &cdp_target_owners).await;

        let after_state = snapshot_handle.snapshot().await?;
        let after_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=stale_lease after_state={after_state:?} after_lease={after_lease:?}"
        );
        assert!(!after_lease.held);
        assert_eq!(after_lease.owner_session_id, None);

        cancel.cancel();
        let final_snapshot = join.await?;
        assert!(final_snapshot.held_keys.is_empty());
        let _prior = synapse_action::lease::force_clear("http_stale_lease_test_reset");

        Ok(())
    }

    #[tokio::test]
    async fn expired_lease_cleanup_releases_held_input_before_reacquire() -> anyhow::Result<()> {
        let _serial = test_support::lease_serial(TEST_RESET_REASON);
        let cancel = CancellationToken::new();
        let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), backend);
        let session_manager = LocalSessionManager::default();
        let expired_session_id = "expired-lease-session";
        let contender_session_id = "expired-lease-contender";
        let _prior = synapse_action::lease::force_clear("http_expired_lease_test_reset");

        handle
            .with_session_id(Some(expired_session_id.to_owned()))
            .execute(Action::KeyDown {
                key: test_key("ctrl"),
                backend: Backend::Software,
            })
            .await?;
        let _held = synapse_action::lease::try_acquire(
            expired_session_id,
            Duration::from_millis(synapse_action::MIN_LEASE_TTL_MS),
        );
        tokio::time::sleep(Duration::from_millis(synapse_action::MIN_LEASE_TTL_MS + 50)).await;

        let before_state = snapshot_handle.snapshot().await?;
        let before_ownership = handle.session_inputs_snapshot()?;
        let before_lease = synapse_action::lease::status();
        let before_pending = synapse_action::lease::expired_cleanup_snapshot();
        println!(
            "readback=http_session_cleanup edge=expired_lease before_state={before_state:?} before_ownership={before_ownership:?} before_lease={before_lease:?} before_pending={before_pending:?}"
        );
        assert_eq!(before_state.held_keys, vec![test_key("ctrl")]);
        assert!(!before_lease.held);
        assert_eq!(before_pending.len(), 1);
        match synapse_action::lease::try_acquire(contender_session_id, Duration::from_secs(30)) {
            synapse_action::LeaseOutcome::CleanupPending { expired, .. } => {
                assert_eq!(
                    expired.owner_session_id.as_deref(),
                    Some(expired_session_id)
                );
            }
            other => anyhow::bail!("contender should be refused pending cleanup, got {other:?}"),
        }

        let cdp_target_owners = empty_cdp_target_owners();
        cleanup_stale_session_inputs_once(&handle, &session_manager, &cdp_target_owners).await;

        let after_state = snapshot_handle.snapshot().await?;
        let after_ownership = handle.session_inputs_snapshot()?;
        let after_pending = synapse_action::lease::expired_cleanup_snapshot();
        let acquire_after_cleanup =
            synapse_action::lease::try_acquire(contender_session_id, Duration::from_secs(30));
        let after_lease = synapse_action::lease::status();
        println!(
            "readback=http_session_cleanup edge=expired_lease after_state={after_state:?} after_ownership={after_ownership:?} after_pending={after_pending:?} acquire_after_cleanup={acquire_after_cleanup:?} after_lease={after_lease:?}"
        );
        assert!(after_state.held_keys.is_empty());
        assert!(after_ownership.sessions.is_empty());
        assert!(after_pending.is_empty());
        assert!(matches!(
            acquire_after_cleanup,
            synapse_action::LeaseOutcome::Acquired(_)
        ));
        assert_eq!(
            after_lease.owner_session_id.as_deref(),
            Some(contender_session_id)
        );

        let _released = synapse_action::lease::release_if_owner(contender_session_id);
        cancel.cancel();
        let final_snapshot = join.await?;
        assert!(final_snapshot.held_keys.is_empty());
        let _prior = synapse_action::lease::force_clear("http_expired_lease_test_reset");

        Ok(())
    }

    fn test_key(value: &str) -> Key {
        Key {
            code: KeyCode::Named {
                value: value.to_owned(),
            },
            use_scancode: false,
        }
    }
}
