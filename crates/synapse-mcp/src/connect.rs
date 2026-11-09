//! `--mode connect`: native stdio<->HTTP bridge to the shared Synapse daemon.
//!
//! Lets a stdio-only MCP client (Claude Desktop, Codex) reach the single shared
//! HTTP daemon instead of spawning its own embedded server (which would contend
//! for the one RocksDB lock). The bridge is a transport-level pump: it forwards
//! raw JSON-RPC between the client's stdio transport and an rmcp
//! Streamable-HTTP client transport pointed at the daemon, so the initialize
//! handshake, `Mcp-Session-Id` sessions, and SSE server->client notifications
//! are all handled by rmcp's client worker. No message interpretation, no
//! external proxy dependency.

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::{
    borrow::Cow,
    collections::HashMap,
    future,
    path::Path,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        Transport,
        async_rw::AsyncRwTransport,
        streamable_http_client::{
            SseError, StreamableHttpClient, StreamableHttpClientTransport,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use sse_stream::Sse;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::stdio_eof::CancelOnEofRead;

/// How long to wait for a freshly spawned daemon to become healthy.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(15);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(200);
// rmcp bounds its internal session DELETE to five seconds. Keep our transport
// join bound outside that window so a healthy-but-slow DELETE can finish and be
// observed instead of having its join future detached at three seconds.
const DAEMON_CLOSE_TIMEOUT: Duration = Duration::from_secs(7);
const DAEMON_OWNER_QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(6);
const CLIENT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const EXPIRED_SESSION_READBACK_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_ABSENCE_BODY_LIMIT: usize = 16 * 1024;
#[cfg(windows)]
const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;

#[derive(Debug)]
enum ParentWatchdogEvent {
    ParentExited {
        parent_pid: u32,
        parent_creation_time_100ns: u64,
    },
    Failed {
        code: &'static str,
        parent_pid: Option<u32>,
        detail: String,
    },
}

#[derive(Debug)]
struct ConnectShutdownFailure {
    code: &'static str,
    phase: &'static str,
    detail: String,
}

#[derive(Clone, Copy, Debug)]
enum ConnectTransport {
    Daemon,
    Client,
}

impl ConnectTransport {
    fn label(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Client => "client",
        }
    }

    fn close_failed_code(self) -> &'static str {
        match self {
            Self::Daemon => "MCP_CONNECT_DAEMON_CLOSE_FAILED",
            Self::Client => "MCP_CONNECT_CLIENT_CLOSE_FAILED",
        }
    }

    fn close_timeout_code(self) -> &'static str {
        match self {
            Self::Daemon => "MCP_CONNECT_DAEMON_CLOSE_TIMEOUT",
            Self::Client => "MCP_CONNECT_CLIENT_CLOSE_TIMEOUT",
        }
    }
}

#[derive(Debug, Default)]
struct ConnectShutdownFailures {
    failures: Vec<ConnectShutdownFailure>,
}

impl ConnectShutdownFailures {
    fn push(&mut self, code: &'static str, phase: &'static str, detail: impl Into<String>) {
        self.failures.push(ConnectShutdownFailure {
            code,
            phase,
            detail: detail.into(),
        });
    }

    fn inspect_close<E>(
        &mut self,
        transport: ConnectTransport,
        timeout: Duration,
        result: Result<Result<(), E>, tokio::time::error::Elapsed>,
    ) where
        E: std::fmt::Display,
    {
        let transport_label = transport.label();
        match result {
            Ok(Ok(())) => tracing::info!(
                code = "MCP_CONNECT_TRANSPORT_CLOSE_OK",
                transport = transport_label,
                timeout_ms = timeout.as_millis(),
                "connect bridge transport closed"
            ),
            Ok(Err(error)) => self.push(
                transport.close_failed_code(),
                "transport_close",
                format!("transport={transport_label}: {error}"),
            ),
            Err(_elapsed) => self.push(
                transport.close_timeout_code(),
                "transport_close",
                format!(
                    "transport={transport_label} timeout_ms={}",
                    timeout.as_millis()
                ),
            ),
        }
    }

    fn extend(&mut self, mut other: Self) {
        self.failures.append(&mut other.failures);
    }

    fn into_result(self) -> anyhow::Result<()> {
        use std::io::Write as _;

        if self.failures.is_empty() {
            return Ok(());
        }
        let failure_details = self
            .failures
            .iter()
            .map(|failure| {
                format!(
                    "code={} phase={} detail={}",
                    failure.code, failure.phase, failure.detail
                )
            })
            .collect::<Vec<_>>();
        tracing::error!(
            code = "MCP_CONNECT_SHUTDOWN_FAILED",
            failure_count = self.failures.len(),
            failures = ?failure_details,
            "connect bridge completed every shutdown phase but one or more phases failed"
        );
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = writeln!(
            stderr,
            "synapse-mcp connect shutdown error: code=MCP_CONNECT_SHUTDOWN_FAILED failure_count={} failures={:?}",
            self.failures.len(),
            failure_details
        );
        anyhow::bail!(
            "connect bridge shutdown failed ({}): {:?}",
            self.failures.len(),
            failure_details
        )
    }
}

#[derive(Clone, Debug)]
enum ConnectSessionDeleteState {
    Active,
    DeleteStarted,
    Deleted,
    Absent,
    DeleteFailed(String),
}

#[derive(Clone, Debug)]
struct ConnectSessionCleanupRecord {
    observation_id: u64,
    session_id: String,
    state: ConnectSessionDeleteState,
}

#[derive(Clone, Debug)]
struct ConnectSessionDeleteAttempt {
    observation_id: u64,
    session_id: String,
    terminal_before_attempt: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectSessionDeleteSuccess {
    Deleted,
    Absent,
}

#[derive(Debug, Default)]
struct ConnectSessionCleanupLedger {
    // Append-only generations matter here: a daemon restart is allowed to
    // reuse an opaque session ID. A map keyed only by that string could let a
    // later successful DELETE erase the fact that an earlier generation was
    // never cleaned up.
    sessions: Vec<ConnectSessionCleanupRecord>,
    next_observation_id: u64,
    // Deletion failures are immutable evidence. A later session that happens
    // to reuse the same opaque ID must not erase an earlier failed attempt.
    delete_failures: Vec<String>,
    integrity_failures: Vec<String>,
    mutex_poisoned: bool,
}

/// Process-local observation of every HTTP session created by this bridge.
///
/// rmcp 1.7 logs DELETE failures/timeouts but returns the forwarding-loop
/// result from its worker. This ledger sits at the actual HTTP-client boundary
/// so bridge success can require a separately observed successful DELETE for
/// every session rather than trusting the swallowed worker verdict.
#[derive(Clone, Debug, Default)]
struct ConnectSessionCleanupTracker {
    ledger: Arc<Mutex<ConnectSessionCleanupLedger>>,
}

impl ConnectSessionCleanupTracker {
    fn with_ledger<R>(&self, update: impl FnOnce(&mut ConnectSessionCleanupLedger) -> R) -> R {
        match self.ledger.lock() {
            Ok(mut ledger) => update(&mut ledger),
            Err(poisoned) => {
                let mut ledger = poisoned.into_inner();
                ledger.mutex_poisoned = true;
                update(&mut ledger)
            }
        }
    }

    fn observe_session(&self, session_id: &str) {
        self.with_ledger(|ledger| {
            // Callers invoke this only when a response established a session
            // that differs from the request's attached generation. Therefore
            // every call is a distinct creation observation even when a daemon
            // restart reuses the exact same opaque string.
            let observation_id = ledger.next_observation_id;
            match ledger.next_observation_id.checked_add(1) {
                Some(next) => ledger.next_observation_id = next,
                None => ledger.integrity_failures.push(
                    "session observation identifier overflowed; generation identity is no longer trustworthy"
                        .to_owned(),
                ),
            }
            ledger.sessions.push(ConnectSessionCleanupRecord {
                observation_id,
                session_id: session_id.to_owned(),
                state: ConnectSessionDeleteState::Active,
            });
        });
    }

    fn begin_delete(&self, session_id: &str) -> ConnectSessionDeleteAttempt {
        self.with_ledger(|ledger| {
            if let Some(record) = ledger
                .sessions
                .iter_mut()
                .rev()
                .find(|record| record.session_id == session_id)
            {
                if matches!(&record.state, ConnectSessionDeleteState::Active) {
                    record.state = ConnectSessionDeleteState::DeleteStarted;
                    return ConnectSessionDeleteAttempt {
                        observation_id: record.observation_id,
                        session_id: session_id.to_owned(),
                        terminal_before_attempt: false,
                    };
                }
                if matches!(
                    &record.state,
                    ConnectSessionDeleteState::Deleted | ConnectSessionDeleteState::Absent
                ) {
                    // rmcp retains the old cleanup slot until transparent
                    // reinitialization succeeds. If reinitialization fails,
                    // its finalizer legitimately issues a second idempotent
                    // DELETE after our explicit absence readback. The prior
                    // terminal evidence remains authoritative regardless of
                    // this redundant attempt's transport outcome.
                    return ConnectSessionDeleteAttempt {
                        observation_id: record.observation_id,
                        session_id: session_id.to_owned(),
                        terminal_before_attempt: true,
                    };
                }
            }

            // A DELETE for a session this bridge never observed is not proof
            // that every created session was cleaned. Preserve the anomalous
            // attempt as its own generation and make the integrity failure
            // sticky even if the HTTP request later succeeds.
            let observation_id = ledger.next_observation_id;
            ledger.next_observation_id = ledger.next_observation_id.saturating_add(1);
            ledger.integrity_failures.push(format!(
                "session_id={session_id} DELETE began without an active observed generation"
            ));
            ledger.sessions.push(ConnectSessionCleanupRecord {
                observation_id,
                session_id: session_id.to_owned(),
                state: ConnectSessionDeleteState::DeleteStarted,
            });
            ConnectSessionDeleteAttempt {
                observation_id,
                session_id: session_id.to_owned(),
                terminal_before_attempt: false,
            }
        })
    }

    fn finish_delete(
        &self,
        attempt: &ConnectSessionDeleteAttempt,
        result: &Result<ConnectSessionDeleteSuccess, StreamableHttpError<reqwest::Error>>,
    ) {
        self.with_ledger(|ledger| {
            let Some(record_index) = ledger.sessions.iter().position(|record| {
                record.observation_id == attempt.observation_id
                    && record.session_id == attempt.session_id
            }) else {
                ledger.integrity_failures.push(format!(
                    "observation_id={} session_id={} DELETE completed without its begin record",
                    attempt.observation_id, attempt.session_id
                ));
                return;
            };
            if attempt.terminal_before_attempt {
                if !matches!(
                    &ledger.sessions[record_index].state,
                    ConnectSessionDeleteState::Deleted | ConnectSessionDeleteState::Absent
                ) {
                    ledger.integrity_failures.push(format!(
                        "observation_id={} session_id={} lost terminal state during an idempotent DELETE",
                        attempt.observation_id, attempt.session_id
                    ));
                }
                return;
            }
            if !matches!(
                &ledger.sessions[record_index].state,
                ConnectSessionDeleteState::DeleteStarted
            ) {
                ledger.integrity_failures.push(format!(
                    "observation_id={} session_id={} DELETE completed from non-started state",
                    attempt.observation_id, attempt.session_id
                ));
            }
            ledger.sessions[record_index].state = match result {
                Ok(ConnectSessionDeleteSuccess::Deleted) => ConnectSessionDeleteState::Deleted,
                Ok(ConnectSessionDeleteSuccess::Absent) => ConnectSessionDeleteState::Absent,
                Err(error) => {
                    let detail = format!(
                        "observation_id={} session_id={} error={error}",
                        attempt.observation_id, attempt.session_id
                    );
                    ledger.delete_failures.push(detail.clone());
                    ConnectSessionDeleteState::DeleteFailed(detail)
                }
            };
        });
    }

    fn verification_error(&self) -> Option<String> {
        self.with_ledger(|ledger| {
            let unresolved = ledger
                .sessions
                .iter()
                .filter_map(|record| match &record.state {
                    ConnectSessionDeleteState::Deleted | ConnectSessionDeleteState::Absent => None,
                    ConnectSessionDeleteState::Active => Some(format!(
                        "observation_id={} session_id={} state=active",
                        record.observation_id, record.session_id
                    )),
                    ConnectSessionDeleteState::DeleteStarted => Some(format!(
                        "observation_id={} session_id={} state=delete_started",
                        record.observation_id, record.session_id
                    )),
                    ConnectSessionDeleteState::DeleteFailed(detail) => Some(format!(
                        "observation_id={} session_id={} state=delete_failed detail={detail}",
                        record.observation_id, record.session_id
                    )),
                })
                .collect::<Vec<_>>();
            if !ledger.mutex_poisoned
                && unresolved.is_empty()
                && ledger.delete_failures.is_empty()
                && ledger.integrity_failures.is_empty()
            {
                return None;
            }
            Some(format!(
                "mutex_poisoned={} unresolved=[{}] immutable_delete_failures=[{}] integrity_failures=[{}]",
                ledger.mutex_poisoned,
                unresolved.join("; "),
                ledger.delete_failures.join("; "),
                ledger.integrity_failures.join("; ")
            ))
        })
    }
}

fn verify_explicit_session_absence(
    session_id: &str,
    content_type: Option<&str>,
    recovery_header: Option<&str>,
    body: &[u8],
) -> Result<(), String> {
    if !content_type.is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"))
    {
        return Err(format!("unexpected content_type={content_type:?}"));
    }
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|error| format!("invalid JSON body: {error}"))?;
    let code = payload.get("code").and_then(serde_json::Value::as_str);
    let echoed_session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str);
    let daemon_alive = payload
        .get("daemon_alive")
        .and_then(serde_json::Value::as_bool);
    let source_of_truth = payload
        .get("source_of_truth")
        .and_then(serde_json::Value::as_str);
    let recovery = payload.get("recovery").and_then(serde_json::Value::as_str);
    let expected_code = synapse_core::error_codes::HTTP_SESSION_INVALID;
    if code != Some(expected_code)
        || echoed_session_id != Some(session_id)
        || daemon_alive != Some(true)
        || source_of_truth != Some("http_session_middleware")
        || recovery_header.is_none()
        || recovery != recovery_header
    {
        return Err(format!(
            "expected code={expected_code} session_id={session_id} daemon_alive=true source_of_truth=http_session_middleware and matching recovery; got code={code:?} session_id={echoed_session_id:?} daemon_alive={daemon_alive:?} source_of_truth={source_of_truth:?} recovery_header={recovery_header:?} recovery={recovery:?}"
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ConnectHttpOwnerState {
    live_clients: AtomicUsize,
    accounting_failed: AtomicBool,
    quiescent: tokio::sync::Notify,
}

#[derive(Clone, Debug)]
struct ConnectHttpOwner {
    state: Arc<ConnectHttpOwnerState>,
}

impl ConnectHttpOwner {
    fn new() -> (Self, Arc<ConnectHttpOwnerState>) {
        let state = Arc::new(ConnectHttpOwnerState {
            live_clients: AtomicUsize::new(1),
            accounting_failed: AtomicBool::new(false),
            quiescent: tokio::sync::Notify::new(),
        });
        (
            Self {
                state: Arc::clone(&state),
            },
            state,
        )
    }

    fn live_clients(&self) -> usize {
        self.state.live_clients.load(Ordering::Acquire)
    }

    fn accounting_failed(&self) -> bool {
        self.state.accounting_failed.load(Ordering::Acquire)
    }

    async fn wait_for_quiescence(&self) -> Result<(), tokio::time::error::Elapsed> {
        tokio::time::timeout(DAEMON_OWNER_QUIESCENCE_TIMEOUT, async {
            loop {
                if self.live_clients() == 0 {
                    break;
                }
                let notified = self.state.quiescent.notified();
                if self.live_clients() == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await
    }
}

#[derive(Debug)]
struct TrackedConnectHttpClient {
    inner: reqwest::Client,
    cleanup: ConnectSessionCleanupTracker,
    owner: Arc<ConnectHttpOwnerState>,
}

impl Clone for TrackedConnectHttpClient {
    fn clone(&self) -> Self {
        self.owner.live_clients.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
            cleanup: self.cleanup.clone(),
            owner: Arc::clone(&self.owner),
        }
    }
}

impl Drop for TrackedConnectHttpClient {
    fn drop(&mut self) {
        let previous = self.owner.live_clients.fetch_sub(1, Ordering::AcqRel);
        if previous == 1 {
            self.owner.quiescent.notify_waiters();
        } else if previous == 0 {
            // This cannot happen without an internal ownership accounting bug.
            // Restore zero rather than wrapping and preserve a visible log.
            self.owner.live_clients.store(0, Ordering::Release);
            self.owner.accounting_failed.store(true, Ordering::Release);
            tracing::error!(
                code = "MCP_CONNECT_DAEMON_OWNER_UNDERFLOW",
                "tracked HTTP client ownership underflowed"
            );
        }
    }
}

impl StreamableHttpClient for TrackedConnectHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        // rmcp 1.7 transparently reinitializes after SessionExpired, replacing
        // its cleanup slot with the new session. Preserve the old generation's
        // terminal state first; otherwise it remains Active forever and, more
        // importantly, the bridge never independently verifies its absence.
        let expired_session_cleanup = session_id.as_ref().map(|session_id| {
            (
                Arc::clone(&uri),
                Arc::clone(session_id),
                auth_header.clone(),
                custom_headers.clone(),
            )
        });
        let request_session_id = session_id.clone();
        let response = <reqwest::Client as StreamableHttpClient>::post_message(
            &self.inner,
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
        )
        .await;
        if matches!(&response, Err(StreamableHttpError::SessionExpired))
            && let Some((uri, session_id, auth_header, custom_headers)) = expired_session_cleanup
        {
            tracing::info!(
                code = "MCP_CONNECT_EXPIRED_SESSION_READBACK",
                session_id = session_id.as_ref(),
                "attached POST reported an expired session; issuing an independently tracked DELETE/absence readback before rmcp reinitializes"
            );
            match tokio::time::timeout(
                EXPIRED_SESSION_READBACK_TIMEOUT,
                self.delete_session(uri, session_id, auth_header, custom_headers),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    // The original SessionExpired remains the rmcp control
                    // signal, while the sticky cleanup ledger makes this
                    // failed readback prevent a clean bridge verdict.
                    tracing::error!(
                        code = "MCP_CONNECT_EXPIRED_SESSION_READBACK_FAILED",
                        error = %error,
                        "could not obtain terminal cleanup evidence for the expired session"
                    );
                }
                Err(_elapsed) => tracing::error!(
                    code = "MCP_CONNECT_EXPIRED_SESSION_READBACK_TIMEOUT",
                    timeout_ms = EXPIRED_SESSION_READBACK_TIMEOUT.as_millis(),
                    "timed out obtaining terminal cleanup evidence for the expired session"
                ),
            }
        }
        let response = response?;
        let observed_session = match &response {
            StreamableHttpPostResponse::Json(_, Some(session_id))
            | StreamableHttpPostResponse::Sse(_, Some(session_id)) => Some(session_id.as_str()),
            _ => None,
        };
        if let Some(session_id) = observed_session
            && request_session_id.as_deref() != Some(session_id)
        {
            self.cleanup.observe_session(session_id);
        }
        Ok(response)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let session_id_text = session_id.to_string();
        let delete_attempt = self.cleanup.begin_delete(&session_id_text);
        if delete_attempt.terminal_before_attempt {
            tracing::info!(
                code = "MCP_CONNECT_SESSION_DELETE_IDEMPOTENT_NOOP",
                observation_id = delete_attempt.observation_id,
                session_id = %session_id_text,
                "skipping redundant rmcp DELETE because this exact observed generation already has terminal cleanup evidence"
            );
            return Ok(());
        }
        let result = async {
            let mut request = self.inner.delete(uri.as_ref());
            if let Some(auth_header) = auth_header {
                request = request.bearer_auth(auth_header);
            }
            request = request.header("mcp-session-id", session_id.as_ref());
            for (name, value) in custom_headers {
                let reserved = ["accept", "mcp-session-id", "last-event-id"];
                if reserved
                    .iter()
                    .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
                {
                    return Err(StreamableHttpError::ReservedHeaderConflict(
                        name.to_string(),
                    ));
                }
                request = request.header(name, value);
            }
            let mut response = request.send().await.map_err(StreamableHttpError::Client)?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let recovery_header = response
                    .headers()
                    .get("mcp-session-recovery")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(StreamableHttpError::Client)?
                {
                    let next_len = body.len().saturating_add(chunk.len());
                    if next_len > SESSION_ABSENCE_BODY_LIMIT {
                        return Err(StreamableHttpError::UnexpectedServerResponse(
                            Cow::Owned(format!(
                                "HTTP 404 body exceeded {SESSION_ABSENCE_BODY_LIMIT} bytes while verifying session absence"
                            )),
                        ));
                    }
                    body.extend_from_slice(&chunk);
                }
                verify_explicit_session_absence(
                    &session_id_text,
                    content_type.as_deref(),
                    recovery_header.as_deref(),
                    &body,
                )
                .map_err(|detail| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!(
                        "HTTP 404 was not explicit Synapse session-absence evidence: {detail}"
                    )))
                })?;
                // Only Synapse's structured session middleware response is a
                // physical absence readback. A generic route/proxy 404 remains
                // a sticky cleanup failure instead of becoming false success.
                tracing::info!(
                    code = "MCP_CONNECT_SESSION_ALREADY_ABSENT",
                    session_id = %session_id_text,
                    "daemon independently reported that the bridge session no longer exists"
                );
                return Ok(ConnectSessionDeleteSuccess::Absent);
            }
            if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
                return Err(StreamableHttpError::ServerDoesNotSupportDeleteSession);
            }
            response
                .error_for_status()
                .map_err(StreamableHttpError::Client)?;
            Ok(ConnectSessionDeleteSuccess::Deleted)
        }
        .await;
        self.cleanup.finish_delete(&delete_attempt, &result);
        result.map(|_success| ())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        <reqwest::Client as StreamableHttpClient>::get_stream(
            &self.inner,
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
        )
        .await
    }
}

type ConnectDaemonTransport = StreamableHttpClientTransport<TrackedConnectHttpClient>;

struct OwnedConnectDaemonTransport {
    transport: Option<ConnectDaemonTransport>,
    owner: ConnectHttpOwner,
}

impl OwnedConnectDaemonTransport {
    fn transport_mut(&mut self) -> anyhow::Result<&mut ConnectDaemonTransport> {
        self.transport
            .as_mut()
            .context("MCP_CONNECT_DAEMON_TRANSPORT_ALREADY_CLOSED")
    }

    async fn send(&mut self, message: ClientJsonRpcMessage) -> anyhow::Result<()> {
        self.transport_mut()?
            .send(message)
            .await
            .context("send message to daemon transport")
    }

    async fn receive(&mut self) -> anyhow::Result<Option<ServerJsonRpcMessage>> {
        Ok(self.transport_mut()?.receive().await)
    }
}

/// Arm a watchdog that notifies the bridge if its parent (the MCP client) dies.
///
/// stdin EOF is the normal shutdown path, but on Windows an abrupt parent death
/// does not always deliver EOF to an inherited stdin (the original orphan
/// failure mode). This watchdog owns only the blocking process-handle wait; it
/// reports the exact result to `run_connect`, which owns transport shutdown and
/// ordinary Rust unwinding. The shared daemon is intentionally NOT subject to
/// this — it must survive client churn.
fn install_parent_watchdog() -> anyhow::Result<Option<oneshot::Receiver<ParentWatchdogEvent>>> {
    #[cfg(windows)]
    {
        let mut parent = match parent_process_info() {
            Ok(Some(parent)) => parent,
            Ok(None) => {
                tracing::error!(
                    code = "MCP_CONNECT_PARENT_UNKNOWN",
                    "could not determine parent process; refusing bridge without lifecycle owner"
                );
                anyhow::bail!(
                    "MCP_CONNECT_PARENT_UNKNOWN: could not determine parent process; refusing bridge without lifecycle owner"
                );
            }
            Err(error) => {
                tracing::error!(
                    code = "MCP_CONNECT_PARENT_IDENTITY_CAPTURE_FAILED",
                    error = %error,
                    "could not capture stable parent-process identity; refusing bridge"
                );
                anyhow::bail!("MCP_CONNECT_PARENT_IDENTITY_CAPTURE_FAILED: {error:#}");
            }
        };
        if parent.is_unsupported_wsl_interop_host() {
            tracing::error!(
                code = "MCP_CONNECT_UNSUPPORTED_PARENT",
                parent_pid = parent.pid,
                parent_name = %parent.name,
                parent_command_line_len = parent.command_line.len(),
                parent_command_line_mentions_wsl = parent.command_line.to_ascii_lowercase().contains("\\wsl.exe"),
                remediation = "configure WSL clients for HTTP MCP transport or launch through a supported wrapper; direct WSL interop cannot prove client lifetime",
                "refusing direct WSL interop bridge parent"
            );
            anyhow::bail!(
                "MCP_CONNECT_UNSUPPORTED_PARENT: parent_pid={} parent_name={} direct WSL interop cannot prove client lifetime; configure WSL clients for HTTP MCP transport or a supported launcher",
                parent.pid,
                parent.name
            );
        }
        let parent_pid = parent.pid;
        let Some(parent_creation_time_100ns) = parent.creation_time_100ns else {
            tracing::error!(
                code = "MCP_CONNECT_PARENT_IDENTITY_CAPTURE_FAILED",
                parent_pid,
                "supported parent process was missing its exact creation-time identity"
            );
            anyhow::bail!(
                "MCP_CONNECT_PARENT_IDENTITY_CAPTURE_FAILED: supported parent pid {parent_pid} was missing its exact creation-time identity"
            );
        };
        let Some(parent_identity) = parent.identity.take() else {
            anyhow::bail!(
                "MCP_CONNECT_PARENT_IDENTITY_CAPTURE_FAILED: supported parent pid {parent_pid} was missing its exact process-handle owner"
            );
        };
        if parent_identity.pid != parent_pid
            || parent_identity.creation_time_100ns != parent_creation_time_100ns
        {
            let actual_pid = parent_identity.pid;
            let actual_creation_time_100ns = parent_identity.creation_time_100ns;
            let close_result = parent_identity.close_checked();
            anyhow::bail!(
                "MCP_CONNECT_PARENT_IDENTITY_MISMATCH: snapshot_pid={parent_pid} handle_pid={actual_pid} snapshot_creation_time_100ns={parent_creation_time_100ns} handle_creation_time_100ns={actual_creation_time_100ns} exact_handle_close={close_result:?}"
            );
        }
        let parent_start_time_unix_secs = parent.start_time_unix_secs;
        let (sender, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name("synapse-connect-parent-watchdog".to_owned())
            .spawn(move || {
                let event = wait_for_parent_exit(parent_identity, parent_start_time_unix_secs);
                let _receiver_was_dropped = sender.send(event);
            })
            .with_context(|| format!("spawn parent watchdog for pid {parent_pid}"))?;
        tracing::info!(
            code = "MCP_CONNECT_PARENT_WATCHDOG",
            parent_pid,
            parent_creation_time_100ns,
            parent_start_time_unix_secs,
            "parent-death watchdog armed"
        );
        return Ok(Some(receiver));
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

#[cfg(windows)]
fn wait_for_parent_exit(
    parent_identity: ParentProcessIdentity,
    expected_start_time_unix_secs: u64,
) -> ParentWatchdogEvent {
    use windows::Win32::Foundation::{GetLastError, WAIT_FAILED, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    let parent_pid = parent_identity.pid;
    let expected_creation_time_100ns = parent_identity.creation_time_100ns;
    let handle = match parent_identity.handle() {
        Ok(handle) => handle,
        Err(error) => {
            return parent_watchdog_failure_with_close(
                parent_identity,
                "MCP_CONNECT_PARENT_HANDLE_MISSING",
                error.to_string(),
            );
        }
    };
    let actual_creation_time_100ns = match process_creation_time_100ns(handle) {
        Ok(creation_time) => creation_time,
        Err(error) => {
            return parent_watchdog_failure_with_close(
                parent_identity,
                "MCP_CONNECT_PARENT_IDENTITY_READ_FAILED",
                format!(
                    "phase=before_wait expected_creation_time_100ns={expected_creation_time_100ns} expected_start_time_unix_secs={expected_start_time_unix_secs} GetProcessTimes failed: {error}"
                ),
            );
        }
    };
    if !exact_parent_creation_matches(expected_creation_time_100ns, actual_creation_time_100ns) {
        return parent_watchdog_failure_with_close(
            parent_identity,
            "MCP_CONNECT_PARENT_IDENTITY_MISMATCH",
            format!(
                "phase=before_wait expected_creation_time_100ns={expected_creation_time_100ns} actual_creation_time_100ns={actual_creation_time_100ns} expected_start_time_unix_secs={expected_start_time_unix_secs} actual_start_time_unix_secs={:?}",
                creation_time_unix_secs(actual_creation_time_100ns)
            ),
        );
    }
    // SAFETY: handle is a live process handle with synchronization rights.
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    let wait_error = (wait == WAIT_FAILED).then(|| unsafe { GetLastError() });
    if wait != WAIT_OBJECT_0 {
        let wait_detail = wait_error.map_or_else(
            || format!("unexpected wait result {wait:?}"),
            |error| format!("WaitForSingleObject failed: win32_error={}", error.0),
        );
        return parent_watchdog_failure_with_close(
            parent_identity,
            "MCP_CONNECT_PARENT_WAIT_FAILED",
            wait_detail,
        );
    }

    // The wait result is only a trigger. Re-read the exact kernel creation
    // FILETIME through the retained process-object handle immediately before
    // publishing the destructive parent-exit event. PID reuse cannot redirect
    // this read because no close/reopen boundary exists.
    let terminal_creation_time_100ns = match process_creation_time_100ns(handle) {
        Ok(creation_time) => creation_time,
        Err(error) => {
            return parent_watchdog_failure_with_close(
                parent_identity,
                "MCP_CONNECT_PARENT_IDENTITY_READ_FAILED",
                format!(
                    "phase=after_terminal_wait expected_creation_time_100ns={expected_creation_time_100ns} GetProcessTimes failed: {error}"
                ),
            );
        }
    };
    if !exact_parent_creation_matches(expected_creation_time_100ns, terminal_creation_time_100ns) {
        return parent_watchdog_failure_with_close(
            parent_identity,
            "MCP_CONNECT_PARENT_IDENTITY_MISMATCH",
            format!(
                "phase=after_terminal_wait expected_creation_time_100ns={expected_creation_time_100ns} actual_creation_time_100ns={terminal_creation_time_100ns}"
            ),
        );
    }
    if let Err(error) = parent_identity.close_checked() {
        return ParentWatchdogEvent::Failed {
            code: "MCP_CONNECT_PARENT_HANDLE_CLOSE_FAILED",
            parent_pid: Some(parent_pid),
            detail: format!("parent exited but CloseHandle failed: {error}"),
        };
    }
    ParentWatchdogEvent::ParentExited {
        parent_pid,
        parent_creation_time_100ns: terminal_creation_time_100ns,
    }
}

#[cfg(windows)]
fn parent_watchdog_failure_with_close(
    parent_identity: ParentProcessIdentity,
    code: &'static str,
    detail: String,
) -> ParentWatchdogEvent {
    let parent_pid = parent_identity.pid;
    let close_detail = parent_identity
        .close_checked()
        .err()
        .map(|error| format!("; exact parent handle close also failed: {error}"))
        .unwrap_or_default();
    ParentWatchdogEvent::Failed {
        code,
        parent_pid: Some(parent_pid),
        detail: format!("{detail}{close_detail}"),
    }
}

async fn wait_for_parent_watchdog(
    receiver: Option<oneshot::Receiver<ParentWatchdogEvent>>,
) -> ParentWatchdogEvent {
    match receiver {
        Some(receiver) => receiver.await.unwrap_or(ParentWatchdogEvent::Failed {
            code: "MCP_CONNECT_PARENT_WATCHDOG_CHANNEL_CLOSED",
            parent_pid: None,
            detail: "parent watchdog thread ended without publishing its wait result".to_owned(),
        }),
        None => future::pending().await,
    }
}

fn client_receive_end_reason(eof_observed: bool) -> anyhow::Result<&'static str> {
    if eof_observed {
        Ok("stdin_eof")
    } else {
        anyhow::bail!(
            "MCP_CONNECT_CLIENT_TRANSPORT_CLOSED_WITHOUT_EOF: stdio receive ended without the stdin EOF guard observing a zero-byte read"
        )
    }
}

#[cfg(windows)]
fn process_creation_time_100ns(
    handle: windows::Win32::Foundation::HANDLE,
) -> windows::core::Result<u64> {
    use windows::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: handle is a live process handle with query rights, and every
    // FILETIME out-pointer remains valid for the duration of the call.
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

#[cfg(windows)]
fn creation_time_unix_secs(creation_time_100ns: u64) -> Option<u64> {
    creation_time_100ns
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
        .map(|ticks| ticks / 10_000_000)
}

#[cfg(windows)]
const fn exact_parent_creation_matches(expected_100ns: u64, actual_100ns: u64) -> bool {
    expected_100ns == actual_100ns
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
struct KernelParentProcessSnapshot {
    pid: u32,
    creation_time_100ns: u64,
}

#[cfg(windows)]
fn kernel_parent_process_snapshot() -> anyhow::Result<Option<KernelParentProcessSnapshot>> {
    use windows::{
        Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation},
        Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION,
    };

    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
    const INITIAL_BUFFER_BYTES: usize = 64 * 1024;
    // In the documented SYSTEM_PROCESS_INFORMATION layout, `Reserved1`
    // contains WorkingSetPrivateSize, HardFaultCount,
    // NumberOfThreadsHighWatermark, CycleTime, CreateTime, UserTime, and
    // KernelTime. CreateTime begins 24 bytes into this exact 48-byte field.
    const CREATION_TIME_OFFSET_IN_RESERVED1: usize = 24;

    let word_size = std::mem::size_of::<usize>();
    let mut buffer = vec![0_usize; INITIAL_BUFFER_BYTES.div_ceil(word_size)];
    let used_bytes = loop {
        let buffer_bytes = buffer
            .len()
            .checked_mul(word_size)
            .ok_or_else(|| anyhow::anyhow!("native process snapshot buffer size overflow"))?;
        let buffer_bytes_u32 = u32::try_from(buffer_bytes)
            .context("native process snapshot buffer exceeds the Windows u32 length contract")?;
        let mut returned_bytes = 0_u32;
        // SAFETY: `buffer` is pointer-aligned storage with exactly the supplied
        // writable byte length. NtQuerySystemInformation reports the required
        // or written length through `returned_bytes`.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buffer.as_mut_ptr().cast(),
                buffer_bytes_u32,
                &mut returned_bytes,
            )
        };
        if status.0 == STATUS_INFO_LENGTH_MISMATCH {
            let required = usize::try_from(returned_bytes)
                .context("native process snapshot required length exceeds usize")?;
            let grown = required
                .max(buffer_bytes.saturating_mul(2))
                .checked_add(INITIAL_BUFFER_BYTES)
                .ok_or_else(|| anyhow::anyhow!("native process snapshot growth overflow"))?;
            buffer.resize(grown.div_ceil(word_size), 0);
            continue;
        }
        if status.0 < 0 {
            anyhow::bail!(
                "NtQuerySystemInformation(SystemProcessInformation) failed: ntstatus=0x{:08X}",
                status.0 as u32
            );
        }
        let returned_bytes = usize::try_from(returned_bytes)
            .context("native process snapshot returned length exceeds usize")?;
        break if returned_bytes == 0 {
            buffer_bytes
        } else {
            returned_bytes.min(buffer_bytes)
        };
    };

    let current_pid = std::process::id();
    let mut current_parent_pid = None;
    let mut creation_times = HashMap::<u32, u64>::new();
    let mut offset = 0_usize;
    loop {
        anyhow::ensure!(
            offset
                .checked_add(std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>())
                .is_some_and(|end| end <= used_bytes),
            "native process snapshot entry at offset {offset} exceeds returned length {used_bytes}"
        );
        // SAFETY: bounds were checked above. Copy the entry bytes into an
        // aligned local value because the Windows byte stream's alignment is
        // an external ABI fact, not a Rust type-system guarantee carried by
        // the `u8` offset pointer.
        let process = unsafe {
            let process_bytes = buffer.as_ptr().cast::<u8>().add(offset);
            let mut process = std::mem::MaybeUninit::<SYSTEM_PROCESS_INFORMATION>::uninit();
            std::ptr::copy_nonoverlapping(
                process_bytes,
                process.as_mut_ptr().cast::<u8>(),
                std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>(),
            );
            process.assume_init()
        };
        let pid_value = process.UniqueProcessId.0 as usize;
        if pid_value != 0 {
            let pid = u32::try_from(pid_value)
                .with_context(|| format!("native process snapshot PID {pid_value} exceeds u32"))?;
            let creation_bytes: [u8; 8] = process.Reserved1
                [CREATION_TIME_OFFSET_IN_RESERVED1..CREATION_TIME_OFFSET_IN_RESERVED1 + 8]
                .try_into()
                .context("native process snapshot creation-time slice was not 8 bytes")?;
            let creation_time = i64::from_ne_bytes(creation_bytes);
            let creation_time_100ns = u64::try_from(creation_time).with_context(|| {
                format!(
                    "native process snapshot reported negative creation time {creation_time} for pid {pid}"
                )
            })?;
            creation_times.insert(pid, creation_time_100ns);
            if pid == current_pid {
                let inherited_pid_value = process.Reserved2 as usize;
                current_parent_pid =
                    Some(u32::try_from(inherited_pid_value).with_context(|| {
                        format!(
                            "native process snapshot parent PID {inherited_pid_value} exceeds u32"
                        )
                    })?);
            }
        }

        let next = usize::try_from(process.NextEntryOffset)
            .context("native process snapshot next-entry offset exceeds usize")?;
        if next == 0 {
            break;
        }
        anyhow::ensure!(
            next >= std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>(),
            "native process snapshot returned invalid next-entry offset {next}"
        );
        offset = offset
            .checked_add(next)
            .ok_or_else(|| anyhow::anyhow!("native process snapshot offset overflow"))?;
    }

    let Some(parent_pid) = current_parent_pid else {
        anyhow::bail!(
            "native process snapshot did not contain current pid {current_pid}; exact parent identity is unavailable"
        );
    };
    if parent_pid == 0 {
        return Ok(None);
    }
    let creation_time_100ns = creation_times.get(&parent_pid).copied().ok_or_else(|| {
        anyhow::anyhow!(
            "native process snapshot named parent pid {parent_pid} for current pid {current_pid}, but no live exact parent entry existed"
        )
    })?;
    Ok(Some(KernelParentProcessSnapshot {
        pid: parent_pid,
        creation_time_100ns,
    }))
}

#[cfg(windows)]
#[derive(Debug)]
struct ParentProcessIdentity {
    pid: u32,
    creation_time_100ns: u64,
    handle: Option<OwnedHandle>,
}

#[cfg(windows)]
impl ParentProcessIdentity {
    fn handle(&self) -> anyhow::Result<windows::Win32::Foundation::HANDLE> {
        let handle = self.handle.as_ref().ok_or_else(|| {
            anyhow::anyhow!("live parent identity no longer owns its exact process handle")
        })?;
        Ok(windows::Win32::Foundation::HANDLE(handle.as_raw_handle()))
    }

    fn close_checked(mut self) -> windows::core::Result<()> {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};

        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        // SAFETY: `into_raw_handle` transfers the sole owned handle out of the
        // RAII wrapper; this call is therefore the exact, one-time close.
        unsafe { CloseHandle(HANDLE(handle.into_raw_handle())) }
    }
}

#[cfg(windows)]
fn capture_process_identity(parent_pid: u32) -> anyhow::Result<ParentProcessIdentity> {
    use windows::Win32::{
        Foundation::HANDLE,
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE},
    };

    // SAFETY: the returned kernel handle is immediately transferred into one
    // `OwnedHandle`. Keeping this exact process object open across the watchdog
    // handoff removes the close/reopen PID-reuse window.
    let raw_handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            parent_pid,
        )
    }
    .with_context(|| format!("open parent pid {parent_pid} for exact identity capture"))?;
    let handle = unsafe { OwnedHandle::from_raw_handle(raw_handle.0) };
    let creation_time_100ns = process_creation_time_100ns(HANDLE(handle.as_raw_handle()))
        .with_context(|| format!("read exact creation time for parent pid {parent_pid}"))?;
    Ok(ParentProcessIdentity {
        pid: parent_pid,
        creation_time_100ns,
        handle: Some(handle),
    })
}

#[cfg(windows)]
#[derive(Debug)]
struct ParentProcessInfo {
    pid: u32,
    name: String,
    command_line: String,
    creation_time_100ns: Option<u64>,
    identity: Option<ParentProcessIdentity>,
    start_time_unix_secs: u64,
}

#[cfg(windows)]
impl ParentProcessInfo {
    fn is_unsupported_wsl_interop_host(&self) -> bool {
        self.name.eq_ignore_ascii_case("wsl.exe")
            || self.name.eq_ignore_ascii_case("wslhost.exe")
            || self.command_line.to_ascii_lowercase().contains("\\wsl.exe")
    }
}

#[cfg(windows)]
fn parent_process_info() -> anyhow::Result<Option<ParentProcessInfo>> {
    use sysinfo::{ProcessesToUpdate, System, get_current_pid};
    let Some(kernel_parent) = kernel_parent_process_snapshot()? else {
        return Ok(None);
    };
    let Ok(current) = get_current_pid() else {
        return Ok(None);
    };
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let Some(sysinfo_parent_pid) = system.process(current).and_then(|process| process.parent())
    else {
        anyhow::bail!(
            "sysinfo did not expose a parent for current pid {}, while the exact kernel snapshot named pid {}",
            current.as_u32(),
            kernel_parent.pid
        );
    };
    if sysinfo_parent_pid.as_u32() != kernel_parent.pid {
        anyhow::bail!(
            "parent PID changed between exact kernel and metadata snapshots: kernel_parent_pid={} sysinfo_parent_pid={}",
            kernel_parent.pid,
            sysinfo_parent_pid.as_u32()
        );
    }
    let Some(parent) = system.process(sysinfo_parent_pid) else {
        anyhow::bail!(
            "sysinfo parent metadata disappeared for exact kernel parent pid {}",
            kernel_parent.pid
        );
    };
    let parent_pid = kernel_parent.pid;
    let start_time_unix_secs = parent.start_time();
    let kernel_start_time_unix_secs = creation_time_unix_secs(kernel_parent.creation_time_100ns)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "parent pid {parent_pid} kernel creation FILETIME {} predates the Unix epoch",
                kernel_parent.creation_time_100ns
            )
        })?;
    if kernel_start_time_unix_secs != start_time_unix_secs {
        anyhow::bail!(
            "parent metadata identity mismatch: pid={parent_pid} kernel_creation_time_100ns={} kernel_start_time_unix_secs={kernel_start_time_unix_secs} sysinfo_start_time_unix_secs={start_time_unix_secs}",
            kernel_parent.creation_time_100ns
        );
    }
    let mut info = ParentProcessInfo {
        pid: parent_pid,
        name: parent.name().to_string_lossy().into_owned(),
        command_line: parent
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
        creation_time_100ns: Some(kernel_parent.creation_time_100ns),
        identity: None,
        start_time_unix_secs,
    };
    // Preserve the explicit unsupported-WSL diagnosis/remediation even on a
    // host where querying that interop process handle is prohibited.
    if info.is_unsupported_wsl_interop_host() {
        return Ok(Some(info));
    }
    let identity = capture_process_identity(parent_pid)?;
    let creation_time_100ns = identity.creation_time_100ns;
    if !exact_parent_creation_matches(kernel_parent.creation_time_100ns, creation_time_100ns) {
        let close_result = identity.close_checked();
        anyhow::bail!(
            "parent identity changed during exact handle capture: pid={parent_pid} kernel_creation_time_100ns={} handle_creation_time_100ns={creation_time_100ns} exact_handle_close={close_result:?}",
            kernel_parent.creation_time_100ns
        );
    }
    info.creation_time_100ns = Some(creation_time_100ns);
    info.identity = Some(identity);
    Ok(Some(info))
}

/// Probe the daemon `/health` endpoint. Returns true only on a 2xx response.
async fn probe_health(bind: &str, token: &str) -> bool {
    let url = format!("http://{bind}/health");
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    else {
        return false;
    };
    match client.get(&url).bearer_auth(token).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Spawn the shared daemon detached (its own stdio = null so it never writes to
/// the bridge's MCP stdout, and it outlives the bridge). The T1 single-instance
/// guard ensures that if several bridges race to spawn, only one daemon wins.
#[cfg(not(windows))]
fn spawn_detached_daemon(bind: &str, db: Option<&Path>) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolve current executable path")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--mode", "http", "--bind", bind]);
    if let Some(db) = db {
        cmd.arg("--db").arg(db);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().context("spawn shared daemon process")?;
    Ok(())
}

/// Spawn the daemon on Windows with `bInheritHandles = FALSE` via
/// `CreateProcessW`. This is critical: `std::process::Command` spawns with
/// handle inheritance enabled, which would leak the stdio pipe handles
/// connecting an MCP client to this bridge into the long-lived daemon — keeping
/// those pipes open so the client could never detect the bridge exiting. With
/// inheritance disabled the detached daemon shares none of our handles.
#[cfg(windows)]
fn spawn_detached_daemon(bind: &str, db: Option<&Path>) -> anyhow::Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    use windows::core::{PCWSTR, PWSTR};

    let exe = std::env::current_exe().context("resolve current executable path")?;
    let mut command_line = String::new();
    command_line.push('"');
    command_line.push_str(&exe.to_string_lossy());
    command_line.push_str("\" --mode http --bind ");
    command_line.push_str(bind);
    if let Some(db) = db {
        command_line.push_str(" --db \"");
        command_line.push_str(&db.to_string_lossy());
        command_line.push('"');
    }
    let mut command_line_w: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let startup_info = STARTUPINFOW {
        cb: u32::try_from(core::mem::size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    // SAFETY: command_line_w is a writable, NUL-terminated UTF-16 buffer kept
    // alive across the call; all optional pointers are null; bInheritHandles is
    // false so the daemon inherits none of this process's handles.
    let result = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command_line_w.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup_info,
            &mut process_info,
        )
    };
    result.context("CreateProcessW for shared daemon")?;

    // SAFETY: handles came from this successful CreateProcessW call and each
    // is closed exactly once. Attempt both closes and preserve both failures.
    let process_close = unsafe { CloseHandle(process_info.hProcess) };
    let thread_close = unsafe { CloseHandle(process_info.hThread) };
    let mut close_failures = Vec::new();
    if let Err(error) = process_close {
        close_failures.push(format!("process_handle: {error}"));
    }
    if let Err(error) = thread_close {
        close_failures.push(format!("thread_handle: {error}"));
    }
    if !close_failures.is_empty() {
        tracing::error!(
            code = "MCP_CONNECT_DAEMON_SPAWN_HANDLE_CLOSE_FAILED",
            failures = ?close_failures,
            "spawned daemon but could not close every CreateProcessW handle"
        );
        anyhow::bail!(
            "MCP_CONNECT_DAEMON_SPAWN_HANDLE_CLOSE_FAILED: {}",
            close_failures.join("; ")
        );
    }
    Ok(())
}

/// Ensure a shared daemon is reachable at `bind`: probe, and if absent spawn one
/// (guarded) and wait until it is healthy. Errors (no fallback) if it never
/// comes up within [`DAEMON_READY_TIMEOUT`].
async fn ensure_daemon_running(bind: &str, db: Option<&Path>, token: &str) -> anyhow::Result<()> {
    if probe_health(bind, token).await {
        tracing::info!(
            code = "MCP_CONNECT_DAEMON_PRESENT",
            bind = %bind,
            "shared daemon already running"
        );
        return Ok(());
    }
    tracing::info!(
        code = "MCP_CONNECT_DAEMON_SPAWNING",
        bind = %bind,
        "no daemon detected; spawning shared daemon"
    );
    spawn_detached_daemon(bind, db).context("spawn shared daemon")?;

    let max_attempts = (DAEMON_READY_TIMEOUT.as_millis() / DAEMON_POLL_INTERVAL.as_millis()) as u32;
    for attempt in 1..=max_attempts {
        tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
        if probe_health(bind, token).await {
            tracing::info!(
                code = "MCP_CONNECT_DAEMON_READY",
                bind = %bind,
                attempts = attempt,
                "spawned daemon is healthy"
            );
            return Ok(());
        }
    }
    anyhow::bail!(
        "MCP_DAEMON_SPAWN_FAILED: shared daemon at {bind} did not become healthy within {}s after spawn",
        DAEMON_READY_TIMEOUT.as_secs()
    );
}

fn new_daemon_transport(
    uri: &str,
    token: &str,
    cleanup: &ConnectSessionCleanupTracker,
) -> anyhow::Result<OwnedConnectDaemonTransport> {
    let config =
        StreamableHttpClientTransportConfig::with_uri(uri.to_owned()).auth_header(token.to_owned());
    let inner = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .context("build connect bridge HTTP client")?;
    let (owner, owner_registration) = ConnectHttpOwner::new();
    let client = TrackedConnectHttpClient {
        inner,
        cleanup: cleanup.clone(),
        owner: owner_registration,
    };
    Ok(OwnedConnectDaemonTransport {
        transport: Some(StreamableHttpClientTransport::with_client(client, config)),
        owner,
    })
}

async fn open_daemon_transport(
    bind: &str,
    uri: &str,
    db: Option<&Path>,
    token: &str,
    cleanup: &ConnectSessionCleanupTracker,
) -> anyhow::Result<OwnedConnectDaemonTransport> {
    ensure_daemon_running(bind, db, token)
        .await
        .context("ensure shared daemon is running")?;
    new_daemon_transport(uri, token, cleanup)
}

async fn close_daemon_transport_verified(
    daemon: &mut OwnedConnectDaemonTransport,
    cleanup: &ConnectSessionCleanupTracker,
) -> ConnectShutdownFailures {
    let mut failures = ConnectShutdownFailures::default();
    match daemon.transport.take() {
        Some(mut transport) => {
            let close = tokio::time::timeout(DAEMON_CLOSE_TIMEOUT, transport.close()).await;
            failures.inspect_close(ConnectTransport::Daemon, DAEMON_CLOSE_TIMEOUT, close);
            drop(transport);
        }
        None => failures.push(
            "MCP_CONNECT_DAEMON_TRANSPORT_ALREADY_CLOSED",
            "transport_close",
            "verified close was invoked after the owned daemon transport had already been taken",
        ),
    }

    match daemon.owner.wait_for_quiescence().await {
        Ok(()) => tracing::info!(
            code = "MCP_CONNECT_DAEMON_OWNER_QUIESCENT",
            "all exact rmcp HTTP-client owners reached terminal Drop"
        ),
        Err(_elapsed) => failures.push(
            "MCP_CONNECT_DAEMON_OWNER_QUIESCENCE_TIMEOUT",
            "transport_owner",
            format!(
                "timeout_ms={} live_clients={}",
                DAEMON_OWNER_QUIESCENCE_TIMEOUT.as_millis(),
                daemon.owner.live_clients()
            ),
        ),
    }
    if daemon.owner.accounting_failed() {
        failures.push(
            "MCP_CONNECT_DAEMON_OWNER_ACCOUNTING_FAILED",
            "transport_owner",
            "tracked HTTP client ownership counter underflowed",
        );
    }
    if let Some(detail) = cleanup.verification_error() {
        failures.push(
            "MCP_CONNECT_SESSION_DELETE_UNVERIFIED",
            "session_delete_readback",
            detail,
        );
    } else {
        tracing::info!(
            code = "MCP_CONNECT_SESSION_DELETE_VERIFIED",
            "every bridge-created HTTP session has a successful DELETE readback"
        );
    }
    failures
}

async fn replace_daemon_transport(
    daemon: &mut OwnedConnectDaemonTransport,
    bind: &str,
    uri: &str,
    db: Option<&Path>,
    token: &str,
    cleanup: &ConnectSessionCleanupTracker,
) -> anyhow::Result<()> {
    // A transport failure often means the daemon itself disappeared. Bring the
    // shared endpoint back first so the old generation's close can obtain an
    // actual DELETE or structured-absence readback. Closing against a dead
    // socket before running ensure_daemon_running would make the reconnect path
    // permanently fail at cleanup and never reach the code that restarts it.
    ensure_daemon_running(bind, db, token)
        .await
        .context("ensure daemon is reachable before closing prior transport")?;
    close_daemon_transport_verified(daemon, cleanup)
        .await
        .into_result()
        .context("close prior daemon transport before reconnect")?;
    *daemon = new_daemon_transport(uri, token, cleanup)
        .context("open replacement daemon transport after daemon health readback")?;
    Ok(())
}

async fn reconnect_daemon_transport_in_place(
    daemon: &mut OwnedConnectDaemonTransport,
    bind: &str,
    uri: &str,
    db: Option<&Path>,
    token: &str,
    cleanup: &ConnectSessionCleanupTracker,
    saved_initialize: Option<&ClientJsonRpcMessage>,
    saved_initialized: Option<&ClientJsonRpcMessage>,
) -> anyhow::Result<()> {
    let Some(initialize_message) = saved_initialize.cloned() else {
        anyhow::bail!("MCP_CONNECT_RECONNECT_NO_INITIALIZE: cannot replay bridge handshake");
    };
    let Some(initialized_message) = saved_initialized.cloned() else {
        anyhow::bail!("MCP_CONNECT_RECONNECT_NO_INITIALIZED: cannot replay bridge handshake");
    };

    // Replace the outer owned slot before replaying any message. If parent
    // death cancels this future, `run_connect` still owns the exact replacement
    // transport and can close it; no reconnect-local worker can detach.
    replace_daemon_transport(daemon, bind, uri, db, token, cleanup).await?;
    daemon
        .send(initialize_message)
        .await
        .context("replay initialize to daemon after reconnect")?;
    let Some(_initialize_response) = tokio::time::timeout(DAEMON_READY_TIMEOUT, daemon.receive())
        .await
        .context("wait for replayed initialize response after daemon reconnect")?
        .context("receive replayed initialize response after daemon reconnect")?
    else {
        anyhow::bail!(
            "MCP_CONNECT_RECONNECT_INIT_EOF: daemon closed before replayed initialize response"
        );
    };
    daemon
        .send(initialized_message)
        .await
        .context("replay initialized notification to daemon after reconnect")?;
    tracing::info!(
        code = "MCP_CONNECT_DAEMON_RECONNECTED",
        bind = %bind,
        "reconnected daemon transport and replayed MCP handshake"
    );
    Ok(())
}

/// Run the stdio<->HTTP bridge against the daemon listening at `bind`
/// (`host:port`). Exits 0 when the client closes stdin; daemon stream loss is
/// repaired by reopening the HTTP transport and replaying the MCP handshake.
pub async fn run_connect(bind: &str, db: Option<&Path>) -> anyhow::Result<ExitCode> {
    let uri = format!("http://{bind}/mcp");
    tracing::info!(
        code = "MCP_CONNECT_STARTING",
        daemon_uri = %uri,
        "starting stdio<->http bridge to shared daemon"
    );

    // Arm the parent-death watchdog before anything else so the bridge can
    // never outlive the client that launched it.
    let parent_watchdog = install_parent_watchdog()?;
    let parent_watchdog = wait_for_parent_watchdog(parent_watchdog);
    tokio::pin!(parent_watchdog);

    let token = crate::http::load_token_value().context("load daemon bearer token for bridge")?;
    let session_cleanup = ConnectSessionCleanupTracker::default();
    let mut shutdown_failures = ConnectShutdownFailures::default();

    let client_closed_token = CancellationToken::new();
    let (stdin, stdout) = rmcp::transport::stdio();
    let stdin = CancelOnEofRead::new(
        stdin,
        client_closed_token.clone(),
        client_closed_token.clone(),
        "MCP_CONNECT_EOF_CONNECTION_CLOSED",
        "connect",
    );
    let mut client = AsyncRwTransport::new_server(stdin, stdout);
    let first_client_message = {
        let first_client_receive = client.receive();
        tokio::pin!(first_client_receive);
        tokio::select! {
            biased;
            event = &mut parent_watchdog => {
                match event {
                    ParentWatchdogEvent::ParentExited {
                        parent_pid,
                        parent_creation_time_100ns,
                    } => {
                        tracing::warn!(
                            code = "MCP_CONNECT_PARENT_EXITED",
                            parent_pid,
                            parent_creation_time_100ns,
                            phase = "client_startup",
                            "parent client process exited before first client message; connect bridge returning through ordinary unwind"
                        );
                        shutdown_failures.push(
                            "MCP_CONNECT_PARENT_EXITED",
                            "parent_watchdog",
                            format!(
                                "parent_pid={parent_pid} parent_creation_time_100ns={parent_creation_time_100ns} phase=client_startup"
                            ),
                        );
                    }
                    ParentWatchdogEvent::Failed { code, parent_pid, detail } => {
                        shutdown_failures.push(
                            code,
                            "parent_watchdog",
                            format!("parent_pid={parent_pid:?}: {detail}"),
                        );
                    }
                }
                shutdown_failures.into_result()?;
                return Ok(ExitCode::SUCCESS);
            }
            from_client = &mut first_client_receive => from_client,
        }
    };
    let Some(first_client_message) = first_client_message else {
        let reason = client_receive_end_reason(client_closed_token.is_cancelled())?;
        tracing::info!(
            code = "MCP_CONNECT_STDIN_EOF",
            "client closed stdin before daemon transport open; shutting down bridge"
        );
        tracing::info!(
            code = "MCP_CONNECT_BRIDGE_STOPPED",
            reason,
            "connect bridge stopped before daemon transport open; closing client transport"
        );
        let client_close = tokio::time::timeout(CLIENT_CLOSE_TIMEOUT, client.close()).await;
        shutdown_failures.inspect_close(
            ConnectTransport::Client,
            CLIENT_CLOSE_TIMEOUT,
            client_close,
        );
        shutdown_failures.into_result()?;
        return Ok(ExitCode::SUCCESS);
    };

    // Ensure exactly one shared daemon is up (spawn it if needed) only after a
    // real client message exists. Empty stdin must not create a daemon HTTP
    // transport whose upstream close future can time out without any MCP
    // session having existed.
    let daemon_open = open_daemon_transport(bind, &uri, db, &token, &session_cleanup);
    tokio::pin!(daemon_open);
    let mut daemon = tokio::select! {
        biased;
        event = &mut parent_watchdog => {
            match event {
                ParentWatchdogEvent::ParentExited {
                    parent_pid,
                    parent_creation_time_100ns,
                } => {
                    tracing::warn!(
                        code = "MCP_CONNECT_PARENT_EXITED",
                        parent_pid,
                        parent_creation_time_100ns,
                        phase = "daemon_startup",
                        "parent client process exited; connect bridge returning through ordinary unwind"
                    );
                    shutdown_failures.push(
                        "MCP_CONNECT_PARENT_EXITED",
                        "parent_watchdog",
                        format!(
                            "parent_pid={parent_pid} parent_creation_time_100ns={parent_creation_time_100ns} phase=daemon_startup"
                        ),
                    );
                }
                ParentWatchdogEvent::Failed { code, parent_pid, detail } => {
                    shutdown_failures.push(
                        code,
                        "parent_watchdog",
                        format!("parent_pid={parent_pid:?}: {detail}"),
                    );
                }
            }
            shutdown_failures.into_result()?;
            return Ok(ExitCode::SUCCESS);
        }
        result = &mut daemon_open => result?,
    };
    let mut saved_initialize = Some(first_client_message.clone());
    let mut saved_initialized: Option<ClientJsonRpcMessage> = None;
    let mut client_message_count = 1usize;
    if let Err(error) = daemon.send(first_client_message.clone()).await {
        tracing::warn!(
            code = "MCP_CONNECT_CLIENT_SEND_FAILED",
            error = %error,
            "initial client->daemon send failed; attempting daemon reconnect"
        );
        replace_daemon_transport(&mut daemon, bind, &uri, db, &token, &session_cleanup)
            .await
            .context("replace daemon after initial send failure")?;
        daemon
            .send(first_client_message)
            .await
            .context("forward initial client->daemon message after reconnect")?;
    }

    let bridge_result: anyhow::Result<&'static str> = {
        // Parent death and stdin EOF sit outside the forwarding future. They
        // therefore cancel even an in-flight send/reconnect await instead of
        // waiting for the selected forwarding branch to return first.
        let forwarding = async {
            loop {
                tokio::select! {
                from_client = client.receive() => {
                    match from_client {
                        Some(message) => {
                            let message_index = client_message_count;
                            client_message_count = client_message_count.saturating_add(1);
                            match message_index {
                                0 => saved_initialize = Some(message.clone()),
                                1 => saved_initialized = Some(message.clone()),
                                _ => {}
                            }

                            if let Err(error) = daemon.send(message.clone()).await {
                                tracing::warn!(
                                    code = "MCP_CONNECT_CLIENT_SEND_FAILED",
                                    error = %error,
                                    "client->daemon send failed; attempting daemon reconnect"
                                );
                                if message_index == 0 {
                                    replace_daemon_transport(
                                        &mut daemon,
                                        bind,
                                        &uri,
                                        db,
                                        &token,
                                        &session_cleanup,
                                    )
                                    .await
                                    .context("replace daemon after initial send failure")?;
                                    daemon
                                        .send(message)
                                        .await
                                        .context("forward initial client->daemon message after reconnect")?;
                                } else {
                                    reconnect_daemon_transport_in_place(
                                        &mut daemon,
                                        bind,
                                        &uri,
                                        db,
                                        &token,
                                        &session_cleanup,
                                        saved_initialize.as_ref(),
                                        saved_initialized.as_ref(),
                                    )
                                    .await
                                    .context("reconnect daemon after client->daemon send failure")?;
                                    if message_index != 1 {
                                        daemon
                                            .send(message)
                                            .await
                                            .context("forward client->daemon message after reconnect")?;
                                    }
                                }
                            }
                        }
                        None => {
                            let reason = client_receive_end_reason(
                                client_closed_token.is_cancelled(),
                            )?;
                            tracing::info!(
                                code = "MCP_CONNECT_STDIN_EOF",
                                "client closed stdin; shutting down bridge"
                            );
                            break Ok(reason);
                        }
                    }
                }
                from_daemon = daemon.receive() => {
                    match from_daemon? {
                        Some(message) => client
                            .send(message)
                            .await
                            .context("forward daemon->client message")?,
                        None => {
                            tracing::warn!(
                                code = "MCP_CONNECT_DAEMON_CLOSED",
                                "daemon stream closed; attempting reconnect"
                            );
                            reconnect_daemon_transport_in_place(
                                &mut daemon,
                                bind,
                                &uri,
                                db,
                                &token,
                                &session_cleanup,
                                saved_initialize.as_ref(),
                                saved_initialized.as_ref(),
                            )
                            .await
                            .context("reconnect daemon after stream close")?;
                        }
                    }
                }
                }
            }
        };
        tokio::pin!(forwarding);
        tokio::select! {
            biased;
            event = &mut parent_watchdog => {
                match event {
                    ParentWatchdogEvent::ParentExited {
                        parent_pid,
                        parent_creation_time_100ns,
                    } => {
                        tracing::warn!(
                            code = "MCP_CONNECT_PARENT_EXITED",
                            parent_pid,
                            parent_creation_time_100ns,
                            phase = "bridge_active",
                            "parent client process exited; cancelling in-flight forwarding and closing bridge transports before ordinary unwind"
                        );
                        shutdown_failures.push(
                            "MCP_CONNECT_PARENT_EXITED",
                            "parent_watchdog",
                            format!(
                                "parent_pid={parent_pid} parent_creation_time_100ns={parent_creation_time_100ns} phase=bridge_active"
                            ),
                        );
                        Ok("parent_exit")
                    }
                    ParentWatchdogEvent::Failed { code, parent_pid, detail } => {
                        shutdown_failures.push(
                            code,
                            "parent_watchdog",
                            format!("parent_pid={parent_pid:?}: {detail}"),
                        );
                        Ok("parent_watchdog_failure")
                    }
                }
            }
            _ = client_closed_token.cancelled() => {
                tracing::info!(
                    code = "MCP_CONNECT_STDIN_EOF",
                    "client stdin EOF guard cancelled in-flight forwarding; shutting down bridge"
                );
                Ok("stdin_eof_guard")
            }
            result = &mut forwarding => result,
        }
    };

    match bridge_result {
        Ok(reason) => tracing::info!(
            code = "MCP_CONNECT_BRIDGE_STOPPED",
            reason,
            "connect bridge forwarding loop stopped; closing both transports"
        ),
        Err(error) => shutdown_failures.push(
            "MCP_CONNECT_FORWARDING_FAILED",
            "bridge_forwarding",
            format!("{error:#}"),
        ),
    }

    // Both closes are independent required phases. Attempt them concurrently,
    // retain timeout/transport errors from each, and return only after the
    // complete aggregate has been reported.
    let (daemon_failures, client_close) = tokio::join!(
        close_daemon_transport_verified(&mut daemon, &session_cleanup),
        tokio::time::timeout(CLIENT_CLOSE_TIMEOUT, client.close()),
    );
    shutdown_failures.extend(daemon_failures);
    shutdown_failures.inspect_close(ConnectTransport::Client, CLIENT_CLOSE_TIMEOUT, client_close);
    shutdown_failures.into_result()?;
    Ok(ExitCode::SUCCESS)
}
