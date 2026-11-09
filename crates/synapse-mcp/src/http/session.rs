use std::{sync::Arc, time::Duration};

use anyhow::{Context, bail};
use axum::{
    body::{Body, to_bytes},
    extract::State,
    http::{Method, Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, SessionConfig};

const SESSION_IDLE_TIMEOUT_ENV: &str = "SYNAPSE_HTTP_SESSION_IDLE_TIMEOUT_SECS";
const DEFAULT_SESSION_IDLE_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MAX_MCP_REQUEST_BYTES: usize = 1024 * 1024;
const SESSION_ID_HEADER: &str = "Mcp-Session-Id";

tokio::task_local! {
    static CURRENT_MCP_SESSION_ID: Option<String>;
}

#[derive(Clone)]
pub(super) struct SessionRequestState {
    session_registry: crate::server::session_registry::SharedSessionRegistry,
    terminated_sessions: crate::server::session_lifecycle::SharedTerminatedSessions,
}

#[derive(Clone)]
pub(super) struct SessionCleanupState {
    session_manager: Arc<LocalSessionManager>,
    lifecycle: crate::server::session_lifecycle::SessionLifecycleState,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SessionFailure {
    /// No `Mcp-Session-Id` header at all — the caller never initialized a
    /// session on this daemon.
    Missing,
    /// A session id was presented but this daemon's session manager does not
    /// know it: the daemon idle-expired it, or the daemon restarted and lost
    /// all in-memory sessions. The daemon is alive (it answered), so this is a
    /// session-level fact, not a daemon crash.
    UnknownOrExpired,
    /// The session was explicitly torn down by the session lifecycle layer
    /// (stale-eviction, `session_end`, or `agent_kill`). Distinct from
    /// `UnknownOrExpired` because the teardown is intentional and the bound
    /// target row was persisted, so re-binding (adopting) the same browser tab
    /// is the right recovery rather than re-creating it.
    Terminated,
}

impl SessionFailure {
    /// Stable, machine-matchable reason token for the returned diagnostic.
    fn reason(self) -> &'static str {
        match self {
            Self::Missing => "session_header_missing",
            Self::UnknownOrExpired => "session_unknown_or_expired",
            Self::Terminated => "session_terminated",
        }
    }

    /// Which of the three failure classes #1360 part 3 asks callers to
    /// distinguish this is. All HTTP-layer session failures are
    /// `session_level` (the daemon answered with a 404 diagnostic, so the
    /// daemon itself did not crash and the chrome bridge was never reached);
    /// target-invalidation and recoverable-bridge failures surface instead as
    /// tool-level error codes (e.g. `CAPTURE_TARGET_INVALID`) on a *valid*
    /// session.
    fn failure_class(self) -> &'static str {
        "session_level"
    }

    /// The recovery action a caller should take. `Missing` never had a session
    /// to rebind; the other two had a target binding (persisted across
    /// teardown), so the caller should re-create a session then re-bind the
    /// same target instead of re-creating the tab.
    fn recovery(self) -> &'static str {
        match self {
            Self::Missing => "initialize_session",
            Self::UnknownOrExpired | Self::Terminated => "recreate_session_then_rebind_target",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Missing => {
                "no Mcp-Session-Id header was presented; initialize an MCP session before calling tools"
            }
            Self::UnknownOrExpired => {
                "the daemon is alive but does not recognize this session id (idle-expired or daemon restarted); create a new session and re-bind your target"
            }
            Self::Terminated => {
                "the session lifecycle terminated this session (stale-eviction / session_end / agent_kill); create a new session and re-bind the same target (its binding was persisted)"
            }
        }
    }
}

impl SessionCleanupState {
    pub(super) fn request_state(
        session_registry: crate::server::session_registry::SharedSessionRegistry,
        terminated_sessions: crate::server::session_lifecycle::SharedTerminatedSessions,
    ) -> SessionRequestState {
        SessionRequestState {
            session_registry,
            terminated_sessions,
        }
    }

    pub(super) fn new(
        session_manager: Arc<LocalSessionManager>,
        lifecycle: crate::server::session_lifecycle::SessionLifecycleState,
    ) -> Self {
        Self {
            session_manager,
            lifecycle,
        }
    }
}

pub(crate) fn current_mcp_session_id() -> Option<String> {
    CURRENT_MCP_SESSION_ID.try_with(Clone::clone).ok().flatten()
}

pub(super) fn load_session_config() -> anyhow::Result<SessionConfig> {
    let mut config = SessionConfig::default();
    let idle_timeout_secs = session_idle_timeout_secs()?;
    config.keep_alive = Some(Duration::from_secs(idle_timeout_secs));
    tracing::info!(
        code = "MCP_HTTP_SESSION_CONFIGURED",
        idle_timeout_s = idle_timeout_secs,
        "HTTP MCP session lifecycle configured"
    );
    Ok(config)
}

pub(super) async fn require_mcp_session(
    State(state): State<SessionRequestState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !is_mcp_endpoint(request.uri().path()) {
        return next.run(request).await;
    }
    let session_id = session_id_from_header(&request);
    let request = match enforce_session_header(request).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let request = match session_id.as_deref() {
        Some(session_id) => {
            if session_is_terminated(&state.terminated_sessions, session_id) {
                if request.method() == Method::DELETE {
                    tracing::info!(
                        code = "MCP_HTTP_SESSION_DELETE_ALREADY_TERMINATED",
                        session_id,
                        "HTTP MCP session DELETE allowed as an idempotent already-terminated cleanup trigger"
                    );
                    let scoped_session_id = Some(session_id.to_owned());
                    return CURRENT_MCP_SESSION_ID
                        .scope(scoped_session_id, next.run(request))
                        .await;
                }
                tracing::warn!(
                    code = synapse_core::error_codes::HTTP_SESSION_INVALID,
                    session_id,
                    "HTTP MCP session rejected because session lifecycle already terminated it"
                );
                return session_invalid_for(SessionFailure::Terminated, Some(session_id));
            }
            match record_session_request(&state.session_registry, session_id, request).await {
                Ok(request) => request,
                Err(response) => return response,
            }
        }
        None => request,
    };
    let diagnostic_session_id = session_id.clone();
    CURRENT_MCP_SESSION_ID
        .scope(session_id, async move {
            let response = next.run(request).await;
            if response.status() == StatusCode::NOT_FOUND {
                return session_invalid_for(
                    SessionFailure::UnknownOrExpired,
                    diagnostic_session_id.as_deref(),
                );
            }
            response
        })
        .await
}

pub(super) async fn release_held_inputs_on_delete(
    State(state): State<SessionCleanupState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let cleanup_session_id = (request.method() == Method::DELETE
        && is_mcp_endpoint(request.uri().path()))
    .then(|| session_id_from_header(&request))
    .flatten();
    if let Some(session_id) = cleanup_session_id.as_deref()
        && !session_is_active(&state.session_manager, session_id).await
    {
        if state.lifecycle.is_session_terminated(session_id) {
            tracing::info!(
                code = "MCP_HTTP_SESSION_DELETE_ALREADY_CLOSED_NOOP",
                session_id,
                "HTTP MCP session DELETE accepted as idempotent no-op for an already-closed terminated session"
            );
            return StatusCode::OK.into_response();
        }
        tracing::warn!(
            code = synapse_core::error_codes::HTTP_SESSION_INVALID,
            session_id,
            reason = ?SessionFailure::UnknownOrExpired,
            "HTTP MCP session delete rejected before held-input cleanup"
        );
        return session_invalid_for(SessionFailure::UnknownOrExpired, Some(session_id));
    }
    let response = next.run(request).await;
    let Some(session_id) = cleanup_session_id else {
        return response;
    };
    if !response.status().is_success() {
        return response;
    }

    match state
        .lifecycle
        .teardown_session(&session_id, "http_delete")
        .await
    {
        Ok(report) => {
            tracing::info!(
                code = "MCP_HTTP_SESSION_LIFECYCLE_CLEANUP",
                session_id,
                report = ?report,
                "readback=session_lifecycle edge=http_delete after_cleanup"
            );
            response
        }
        Err(error) => {
            tracing::error!(
                code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                session_id,
                detail = %error.message,
                data = ?error.data,
                "HTTP MCP session lifecycle cleanup failed"
            );
            lifecycle_cleanup_failed(error)
        }
    }
}

async fn session_is_active(session_manager: &LocalSessionManager, session_id: &str) -> bool {
    session_manager
        .sessions
        .read()
        .await
        .contains_key(session_id)
}

fn session_is_terminated(
    terminated_sessions: &crate::server::session_lifecycle::SharedTerminatedSessions,
    session_id: &str,
) -> bool {
    terminated_sessions
        .lock()
        .is_ok_and(|terminated| terminated.contains(session_id))
}

fn session_idle_timeout_secs() -> anyhow::Result<u64> {
    match std::env::var(SESSION_IDLE_TIMEOUT_ENV) {
        Ok(raw) => parse_idle_timeout(&raw)
            .with_context(|| format!("parse {SESSION_IDLE_TIMEOUT_ENV}={raw:?}")),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SESSION_IDLE_TIMEOUT_SECS),
        Err(error) => Err(error).with_context(|| format!("read {SESSION_IDLE_TIMEOUT_ENV}")),
    }
}

fn parse_idle_timeout(raw: &str) -> anyhow::Result<u64> {
    let value = raw.trim();
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("invalid integer {value:?}"))?;
    if seconds == 0 {
        bail!("idle timeout must be greater than zero seconds");
    }
    Ok(seconds)
}

async fn enforce_session_header(request: Request<Body>) -> Result<Request<Body>, Response> {
    if has_session_header(&request) {
        return Ok(request);
    }
    if request.method() == Method::POST {
        allow_initialize_without_session(request).await
    } else if request.method() == Method::GET || request.method() == Method::DELETE {
        Err(session_invalid(SessionFailure::Missing))
    } else {
        Ok(request)
    }
}

fn has_session_header(request: &Request<Body>) -> bool {
    session_id_from_header(request).is_some()
}

fn session_id_from_header(request: &Request<Body>) -> Option<String> {
    request
        .headers()
        .get(SESSION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

async fn allow_initialize_without_session(
    request: Request<Body>,
) -> Result<Request<Body>, Response> {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_MCP_REQUEST_BYTES)
        .await
        .map_err(|_| payload_too_large())?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes);
    let is_initialize = parsed.as_ref().is_ok_and(jsonrpc_method_is_initialize);
    let request = Request::from_parts(parts, Body::from(bytes));
    if parsed.is_err() || is_initialize {
        Ok(request)
    } else {
        Err(session_invalid(SessionFailure::Missing))
    }
}

async fn record_session_request(
    session_registry: &crate::server::session_registry::SharedSessionRegistry,
    session_id: &str,
    request: Request<Body>,
) -> Result<Request<Body>, Response> {
    if request.method() != Method::POST {
        record_session_heartbeat(session_registry, session_id, None)?;
        return Ok(request);
    }

    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_MCP_REQUEST_BYTES)
        .await
        .map_err(|_| payload_too_large())?;
    let action = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| jsonrpc_action_label(&value));
    record_session_heartbeat(session_registry, session_id, action)?;
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

fn record_session_heartbeat(
    session_registry: &crate::server::session_registry::SharedSessionRegistry,
    session_id: &str,
    action: Option<String>,
) -> Result<(), Response> {
    let mut registry = session_registry.lock().map_err(|_error| {
        tracing::error!(
            code = synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            session_id,
            "HTTP MCP session request could not lock cross-session registry"
        );
        session_registry_failed()
    })?;
    registry.record_seen(
        session_id,
        action,
        crate::server::session_registry::unix_time_ms_now(),
    );
    Ok(())
}

fn jsonrpc_action_label(value: &serde_json::Value) -> Option<String> {
    if value.is_array() {
        return Some("jsonrpc_batch".to_owned());
    }
    let method = value.get("method")?.as_str()?;
    if method == "tools/call"
        && let Some(name) = value
            .get("params")
            .and_then(|params| params.get("name"))
            .and_then(serde_json::Value::as_str)
    {
        return Some(format!("tools/call:{name}"));
    }
    Some(method.to_owned())
}

fn jsonrpc_method_is_initialize(value: &serde_json::Value) -> bool {
    value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|method| method == "initialize")
}

fn is_mcp_endpoint(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/")
}

const SESSION_RECOVERY_HEADER: &str = "mcp-session-recovery";

fn session_invalid(failure: SessionFailure) -> Response {
    session_invalid_for(failure, None)
}

/// Build the 404 session-invalid response with an actionable, machine-matchable
/// diagnostic (#1360 part 3). The body is JSON carrying the failure class and
/// the recovery the caller should take, and the `code` field is the literal
/// `HTTP_SESSION_INVALID` string so existing plaintext sniffers still match.
/// Status stays 404 so rmcp transports keep treating it as session-expired.
fn session_invalid_for(failure: SessionFailure, session_id: Option<&str>) -> Response {
    tracing::warn!(
        code = synapse_core::error_codes::HTTP_SESSION_INVALID,
        reason = failure.reason(),
        failure_class = failure.failure_class(),
        recovery = failure.recovery(),
        session_id = session_id.unwrap_or(""),
        "HTTP MCP session rejected"
    );
    let body = serde_json::json!({
        "code": synapse_core::error_codes::HTTP_SESSION_INVALID,
        "reason": failure.reason(),
        // #1360 part 3: a 404 here means the daemon answered, so it is not a
        // daemon crash and the chrome bridge was never reached. This is a
        // session-level failure; target-invalidation / recoverable-bridge
        // failures surface as tool-level codes on a *valid* session.
        "failure_class": failure.failure_class(),
        "daemon_alive": true,
        "recovery": failure.recovery(),
        "session_id": session_id,
        "detail": failure.detail(),
        "source_of_truth": "http_session_middleware",
    });
    let body = serde_json::to_string(&body)
        .unwrap_or_else(|_| synapse_core::error_codes::HTTP_SESSION_INVALID.to_owned());
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (
                header::HeaderName::from_static(SESSION_RECOVERY_HEADER),
                failure.recovery(),
            ),
        ],
        body,
    )
        .into_response()
}

fn payload_too_large() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
}

fn lifecycle_cleanup_failed(error: rmcp::ErrorData) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!(
            "{}: {}",
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            error.message
        ),
    )
        .into_response()
}

fn session_registry_failed() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "SESSION_REGISTRY_UNAVAILABLE",
    )
        .into_response()
}
