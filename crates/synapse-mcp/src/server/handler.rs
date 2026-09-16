use super::{
    ErrorData, Implementation, ServerCapabilities, ServerHandler, ServerInfo, SessionTarget,
    SynapseService, mcp_error, tool_handler,
};
use futures_util::FutureExt as _;
use rmcp::model::{CallToolResult, ErrorCode};
use serde_json::{Value, json};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};
use synapse_core::error_codes;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SynapseService {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let tool_name = request.name.to_string();
        let mcp_session_id = super::context::mcp_session_id_from_request_context(&context)?;
        let operation = tool_operation_from_arguments(&tool_name, request.arguments.as_ref());
        let lifecycle_guard = self.begin_daemon_lifecycle_tool_call(
            &tool_name,
            operation,
            mcp_session_id.as_deref(),
        )?;
        if let Some(session_id) = mcp_session_id.as_deref()
            && let Err(error) = self.reject_terminated_session_tool_call(&tool_name, session_id)
        {
            lifecycle_guard
                .finish_error(error_snapshot(&error))
                .map_err(lifecycle_mcp_error)?;
            return Err(error);
        }
        if let Err(error) = self.admit_tool_call_for_profile(&tool_name, mcp_session_id.as_deref())
        {
            // Terminalize lifecycle ownership before the best-effort peer
            // notification. The peer await is transport-owned and may be
            // cancelled if the caller disconnects.
            lifecycle_guard
                .finish_error(error_snapshot(&error))
                .map_err(lifecycle_mcp_error)?;
            if profile_policy_denied(&error) {
                match context.peer.notify_tool_list_changed().await {
                    Ok(()) => {
                        tracing::info!(
                            code = "MCP_TOOL_LIST_CHANGED_NOTIFIED",
                            tool = %tool_name,
                            mcp_session_id = ?mcp_session_id,
                            "profile policy denied a hidden/stale tool call and pushed notifications/tools/list_changed"
                        );
                    }
                    Err(notify_err) => {
                        tracing::error!(
                            code = "MCP_TOOL_LIST_CHANGED_NOTIFY_FAILED",
                            tool = %tool_name,
                            mcp_session_id = ?mcp_session_id,
                            error = %notify_err,
                            "profile policy denied a hidden/stale tool call but failed to notify tools/list_changed"
                        );
                    }
                }
            }
            return Err(error);
        }
        let shutdown_cancel = match self.shutdown_cancel_token() {
            Ok(shutdown_cancel) => shutdown_cancel,
            Err(error) => {
                lifecycle_guard
                    .finish_error(error_snapshot(&error))
                    .map_err(lifecycle_mcp_error)?;
                return Err(error);
            }
        };
        let drain_state = self.drain_state_handle();
        let drain_cancel = drain_state.token();
        let drain_snapshot = drain_state.snapshot();
        if drain_snapshot.draining || drain_cancel.is_cancelled() || shutdown_cancel.is_cancelled()
        {
            let snapshot = if drain_snapshot.draining {
                drain_snapshot
            } else {
                drain_state.mark_draining("shutdown_token")
            };
            let error =
                daemon_restarting_mcp_error(&tool_name, mcp_session_id.as_deref(), snapshot);
            lifecycle_guard
                .finish_error(error_snapshot(&error))
                .map_err(lifecycle_mcp_error)?;
            return Err(error);
        }
        let operator_panic_boundary =
            super::operator_panic_boundary::McpOperatorPanicBoundary::capture(
                &tool_name,
                mcp_session_id.as_deref(),
            );
        // The caller owns only a result receiver. The exact routed-call child
        // is retained by the authority supervisor, so HTTP/RMCP cancellation
        // cannot drop a mutation reservation while remote work is still live.
        let lifecycle_owner = Arc::new(Mutex::new(Some(lifecycle_guard)));
        let child_lifecycle_owner = Arc::clone(&lifecycle_owner);
        let child_service = self.clone();
        let child_tool_name = tool_name.clone();
        let child_mcp_session_id = mcp_session_id.clone();
        let (result_sender, mut result_receiver) = oneshot::channel();
        let authority_completion = match self.spawn_cooperative_authority_transaction(
            move |supervisor_cancellation| async move {
                let lifecycle_guard = child_lifecycle_owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                let Some(lifecycle_guard) = lifecycle_guard else {
                    synapse_action::record_operator_panic_safety_incident();
                    child_service
                        .drain_state_handle()
                        .mark_draining("routed_tool_lifecycle_owner_missing");
                    let error = routed_lifecycle_owner_missing_mcp_error(
                        &child_tool_name,
                        child_mcp_session_id.as_deref(),
                        "supervised_child_take",
                    );
                    if result_sender.send(Err(error)).is_err() {
                        tracing::error!(
                            code = error_codes::TOOL_INTERNAL_ERROR,
                            detail_code = "MCP_ROUTED_CALL_LIFECYCLE_OWNER_MISSING",
                            tool = %child_tool_name,
                            mcp_session_id = ?child_mcp_session_id,
                            "routed tool lifecycle owner was missing after its caller disconnected"
                        );
                    }
                    return;
                };
                super::operator_panic_boundary::MCP_OPERATOR_PANIC_BOUNDARY
                    .scope(operator_panic_boundary, {
                        let routed_cancellation = supervisor_cancellation.clone();
                        super::operator_panic_boundary::MCP_REQUEST_CANCELLATION.scope(
                            routed_cancellation,
                            async move {
                                let context =
                                    rmcp::handler::server::tool::ToolCallContext::new(
                                        &child_service,
                                        request,
                                        context,
                                    );
                                let mut result_sender = result_sender;
                                let execution = await_routed_tool_call(
                                    child_service.tool_router.call(context),
                                    &mut result_sender,
                                    supervisor_cancellation,
                                )
                                .await;
                                let result = finish_routed_tool_call(
                                    lifecycle_guard,
                                    &child_tool_name,
                                    child_mcp_session_id.as_deref(),
                                    execution,
                                );
                                if result_sender.send(result).is_err() {
                                    tracing::debug!(
                                        code = "MCP_ROUTED_CALL_CALLER_GONE",
                                        tool = %child_tool_name,
                                        mcp_session_id = ?child_mcp_session_id,
                                        "routed tool reached terminal ownership after its caller disconnected"
                                    );
                                }
                            },
                        )
                    })
                    .await;
            },
        ) {
            Ok(completion) => completion,
            Err(error) => {
                let lifecycle_guard = lifecycle_owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                let Some(lifecycle_guard) = lifecycle_guard else {
                    synapse_action::record_operator_panic_safety_incident();
                    self.drain_state_handle()
                        .mark_draining("routed_tool_lifecycle_owner_missing");
                    return Err(routed_lifecycle_owner_missing_mcp_error(
                        &tool_name,
                        mcp_session_id.as_deref(),
                        "failed_supervisor_admission_take",
                    ));
                };
                lifecycle_guard
                    .finish_error(error_snapshot(&error))
                    .map_err(lifecycle_mcp_error)?;
                return Err(error);
            }
        };
        let result = tokio::select! {
            _ = drain_cancel.cancelled() => {
                let snapshot = drain_state.mark_draining("drain_token");
                let error = daemon_restarting_mcp_error(&tool_name, mcp_session_id.as_deref(), snapshot);
                // Dropping the result receiver is the cancellation signal. The
                // supervised child cancels only if mutation ownership is still
                // unarmed; otherwise it continues through cleanup/audit.
                return Err(error);
            }
            _ = shutdown_cancel.cancelled() => {
                let snapshot = drain_state.mark_draining("shutdown_token");
                let error = daemon_restarting_mcp_error(&tool_name, mcp_session_id.as_deref(), snapshot);
                return Err(error);
            }
            result = &mut result_receiver => result,
        };
        let owner_result = authority_completion.await;
        if let Err(error) = owner_result {
            return Err(authority_join_mcp_error(
                &tool_name,
                mcp_session_id.as_deref(),
                &error,
            ));
        }
        result.map_err(|error| routed_result_channel_mcp_error(&tool_name, error))?
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, ErrorData> {
        let mcp_session_id = super::context::mcp_session_id_from_request_context(&context)?;
        // Normalize schemas before they reach the client, then apply the
        // session's durable tool profile. The policy gate in `call_tool` uses
        // the same profile row so hand-written calls cannot bypass discovery.
        let tools = self.tools_for_session_profile(mcp_session_id.as_deref())?;
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new(
            "synapse-mcp",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(self.instructions())
    }
}

enum RoutedToolExecution<T> {
    Completed(T),
    CancelledBeforeMutation(&'static str),
    Panicked(String),
}

async fn await_routed_tool_call<F, T, R>(
    future: F,
    result_sender: &mut oneshot::Sender<R>,
    supervisor_cancellation: CancellationToken,
) -> RoutedToolExecution<T>
where
    F: Future<Output = T>,
{
    let mut future = Box::pin(AssertUnwindSafe(future).catch_unwind());
    let (poll_outcome, cancellation_reason) = tokio::select! {
        biased;
        outcome = future.as_mut() => (Some(outcome), None),
        () = result_sender.closed() => {
            const REASON: &str = "caller_result_receiver_closed";
            match super::operator_panic_boundary::cancel_mcp_request_before_mutation(
                REASON,
            ) {
                Ok(true) => (None, Some(REASON)),
                Ok(false) => (Some(future.as_mut().await), None),
                Err(error) => {
                    tracing::error!(
                        code = error_codes::TOOL_INTERNAL_ERROR,
                        detail_code = "MCP_ROUTED_CALL_CANCELLATION_STATE_UNKNOWN",
                        error = %error.message,
                        "caller disconnected but routed mutation ownership was unreadable; retaining the exact child to terminal"
                    );
                    (Some(future.as_mut().await), None)
                }
            }
        },
        () = supervisor_cancellation.cancelled() => {
            const REASON: &str = "authority_supervisor_shutdown";
            match super::operator_panic_boundary::cancel_mcp_request_before_mutation(
                REASON,
            ) {
                Ok(true) => (None, Some(REASON)),
                Ok(false) => (Some(future.as_mut().await), None),
                Err(error) => {
                    tracing::error!(
                        code = error_codes::TOOL_INTERNAL_ERROR,
                        detail_code = "MCP_ROUTED_CALL_CANCELLATION_STATE_UNKNOWN",
                        error = %error.message,
                        "daemon shutdown could not prove routed mutation ownership unarmed; retaining the exact child to terminal"
                    );
                    (Some(future.as_mut().await), None)
                }
            }
        },
    };
    // `catch_unwind` contains panics while polling, not panics from a future's
    // destructor. Publish terminal mutation ownership only after that exact
    // destructor has also completed (or its panic has been contained).
    let future_drop_panic = super::drop_authority_owned(future).err();
    match poll_outcome {
        None => match (future_drop_panic, cancellation_reason) {
            (Some(drop_panic), _) => RoutedToolExecution::Panicked(format!(
                "routed tool future destructor panicked during pre-mutation cancellation: {drop_panic}"
            )),
            (None, Some(reason)) => RoutedToolExecution::CancelledBeforeMutation(reason),
            (None, None) => RoutedToolExecution::Panicked(
                "routed tool cancellation completed without a retained causal reason".to_owned(),
            ),
        },
        Some(Ok(output)) => match future_drop_panic {
            Some(drop_panic) => {
                let output_drop_panic = super::drop_authority_owned(output).err();
                let detail = output_drop_panic.map_or_else(
                    || format!("routed tool future destructor panicked after completion: {drop_panic}"),
                    |output_drop_panic| {
                        format!(
                            "routed tool future destructor panicked after completion: {drop_panic}; routed output destructor also panicked: {output_drop_panic}"
                        )
                    },
                );
                RoutedToolExecution::Panicked(detail)
            }
            None => RoutedToolExecution::Completed(output),
        },
        Some(Err(payload)) => {
            let poll_panic = crate::daemon_lifecycle::consume_panic_payload(payload);
            let detail = future_drop_panic.map_or(poll_panic.clone(), |drop_panic| {
                format!("{poll_panic}; routed tool future destructor also panicked: {drop_panic}")
            });
            RoutedToolExecution::Panicked(detail)
        }
    }
}

fn finish_routed_tool_call(
    lifecycle_guard: crate::daemon_lifecycle::ToolCallGuard,
    tool_name: &str,
    mcp_session_id: Option<&str>,
    execution: RoutedToolExecution<Result<CallToolResult, ErrorData>>,
) -> Result<CallToolResult, ErrorData> {
    match execution {
        RoutedToolExecution::Completed(Ok(result)) => {
            let effective_target = effective_target_from_tool_result(&result);
            lifecycle_guard
                .finish_ok_with_effective_target(effective_target)
                .map_err(lifecycle_mcp_error)?;
            Ok(result)
        }
        RoutedToolExecution::Completed(Err(error)) => {
            let error = normalize_tool_error(tool_name, error);
            let error_snapshot = error_snapshot(&error);
            let effective_target = effective_target_from_error_snapshot(&error_snapshot);
            lifecycle_guard
                .finish_error_with_effective_target(error_snapshot, effective_target)
                .map_err(lifecycle_mcp_error)?;
            Err(error)
        }
        RoutedToolExecution::CancelledBeforeMutation(reason) => {
            let error = routed_call_cancelled_mcp_error(tool_name, mcp_session_id, reason);
            lifecycle_guard
                .finish_error(error_snapshot(&error))
                .map_err(lifecycle_mcp_error)?;
            Err(error)
        }
        RoutedToolExecution::Panicked(panic_message) => {
            let panic = json!({
                "payload": panic_message,
                "tool": tool_name,
                "mcp_session_id": mcp_session_id,
            });
            lifecycle_guard
                .finish_panic(panic)
                .map_err(lifecycle_mcp_error)?;
            Err(tool_panic_mcp_error(tool_name, mcp_session_id))
        }
    }
}

impl SynapseService {
    pub(super) fn reject_terminated_session_tool_call(
        &self,
        tool_name: &str,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        let terminated = self.terminated_sessions.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "terminated-session registry lock poisoned while admitting MCP tool call",
            )
        })?;
        if !terminated.contains(session_id) {
            return Ok(());
        }
        tracing::warn!(
            code = error_codes::HTTP_SESSION_INVALID,
            session_id,
            tool = tool_name,
            "MCP tool call rejected because session lifecycle already terminated it"
        );
        Err(ErrorData::new(
            ErrorCode(-32099),
            format!("MCP session {session_id:?} was terminated and cannot call {tool_name}"),
            Some(json!({
                "code": error_codes::HTTP_SESSION_INVALID,
                "session_id": session_id,
                "tool": tool_name,
                "reason": "session_terminated",
                "source_of_truth": "terminated-session registry",
            })),
        ))
    }

    fn begin_daemon_lifecycle_tool_call(
        &self,
        tool_name: &str,
        operation: Option<String>,
        mcp_session_id: Option<&str>,
    ) -> Result<crate::daemon_lifecycle::ToolCallGuard, ErrorData> {
        let (audit_context, audit_context_read_error) = match self.current_action_audit_context() {
            Ok(context) => (
                Some(serde_json::to_value(context).map_err(|error| {
                    lifecycle_mcp_error(anyhow::anyhow!(
                        "serialize daemon lifecycle audit context: {error}"
                    ))
                })?),
                None,
            ),
            Err(error) => (None, Some(error_snapshot(&error))),
        };
        let (foreground, foreground_read_error) = match self.current_audit_foreground() {
            Ok(foreground) => (
                Some(serde_json::to_value(foreground).map_err(|error| {
                    lifecycle_mcp_error(anyhow::anyhow!(
                        "serialize daemon lifecycle foreground: {error}"
                    ))
                })?),
                None,
            ),
            Err(error) => (None, Some(error_snapshot(&error))),
        };
        let (session_target, session_target_read_error) = match self.session_target(mcp_session_id)
        {
            Ok(target) => (target.as_ref().map(session_target_value), None),
            Err(error) => (None, Some(error_snapshot(&error))),
        };
        let (profile, tool_surface_sha256, tool_profile_read_error) =
            match self.tool_profile_snapshot(mcp_session_id) {
                Ok(snapshot) => (
                    Some(snapshot.profile.as_str().to_owned()),
                    Some(snapshot.visible_tool_sha256),
                    None,
                ),
                Err(error) => (None, None, Some(error_snapshot(&error))),
            };
        let route_id = operation
            .as_deref()
            .map(|operation| format!("{tool_name}.{operation}"))
            .or_else(|| Some(tool_name.to_owned()));
        crate::daemon_lifecycle::begin_tool_call(crate::daemon_lifecycle::ToolCallStart {
            tool: tool_name.to_owned(),
            operation,
            route_id,
            profile,
            tool_surface_sha256,
            tool_profile_read_error,
            mcp_session_id: mcp_session_id.map(ToOwned::to_owned),
            audit_context,
            audit_context_read_error,
            foreground,
            foreground_read_error,
            session_target,
            session_target_read_error,
        })
        .map_err(lifecycle_mcp_error)
    }
}

fn tool_operation_from_arguments(
    tool_name: &str,
    arguments: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    if let Some(operation) = arguments
        .and_then(|args| args.get("operation"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|operation| !operation.is_empty())
    {
        return Some(operation.to_ascii_lowercase());
    }
    match tool_name {
        "shell" => Some("run".to_owned()),
        "process" => Some("list".to_owned()),
        "browser_tabs" => Some("list".to_owned()),
        "target" => Some("get".to_owned()),
        "profile" => Some("status".to_owned()),
        "telemetry" => Some("status".to_owned()),
        "storage" => Some("summary".to_owned()),
        _ => None,
    }
}

fn normalize_tool_error(tool_name: &str, error: ErrorData) -> ErrorData {
    if error.data.is_none() && error.message == "tool not found" {
        return mcp_error(
            synapse_core::error_codes::TOOL_NOT_FOUND,
            format!("tool not found: {tool_name}"),
        );
    }
    if error.data.is_none() && error.code == rmcp::model::ErrorCode::INVALID_PARAMS {
        // #1593: turn opaque `deny_unknown_fields` deserialize dead-ends into
        // actionable "did you mean" errors that name the correct field/location.
        // Falls through to the plain code rewrite when the message is not an
        // enrichable unknown-field failure.
        if let Some(enriched) =
            super::param_hints::enrich_param_deserialize_error(tool_name, &error.message)
        {
            return enriched;
        }
        return mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            error.message.to_string(),
        );
    }
    error
}

fn session_target_value(target: &SessionTarget) -> Value {
    match target {
        SessionTarget::Window { hwnd } => json!({
            "kind": "window",
            "hwnd": hwnd,
        }),
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => json!({
            "kind": "cdp",
            "window_hwnd": window_hwnd,
            "cdp_target_id": cdp_target_id,
        }),
    }
}

fn error_snapshot(error: &ErrorData) -> Value {
    json!({
        "rmcp_code": error.code.0,
        "message": error.message.to_string(),
        "data": error.data.clone(),
        "synapse_code": error.data.as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str),
    })
}

fn effective_target_from_tool_result(result: &CallToolResult) -> Option<Value> {
    result
        .structured_content
        .as_ref()
        .and_then(|value| effective_target_from_value(value, "structured_content"))
}

fn effective_target_from_error_snapshot(error: &Value) -> Option<Value> {
    let data = error.get("data")?;
    effective_target_from_value(data, "error.data").or_else(|| {
        data.get("source_id")
            .and_then(Value::as_str)
            .and_then(|source_id| {
                effective_target_from_source_id(source_id, "error.data.source_id")
            })
    })
}

fn effective_target_from_value(value: &Value, source: &'static str) -> Option<Value> {
    let object = value.as_object()?;
    if let Some(target) = effective_target_from_fields(object, source) {
        return Some(target);
    }
    if let Some(target) = object
        .get("target")
        .and_then(|value| effective_target_from_value(value, "structured_content.target"))
    {
        return Some(target);
    }
    for (field, nested_source) in [
        ("content", "structured_content.content"),
        ("locate", "structured_content.locate"),
        ("inspect", "structured_content.inspect"),
        ("aria_snapshot", "structured_content.aria_snapshot"),
        ("capture", "structured_content.capture"),
        ("current", "structured_content.current"),
    ] {
        if let Some(target) = object
            .get(field)
            .and_then(|value| effective_target_from_value(value, nested_source))
        {
            return Some(target);
        }
    }
    None
}

fn effective_target_from_fields(
    object: &serde_json::Map<String, Value>,
    source: &'static str,
) -> Option<Value> {
    let cdp_target_id = object
        .get("cdp_target_id")
        .or_else(|| object.get("cdpTargetId"))
        .and_then(Value::as_str)
        .filter(|target_id| !target_id.trim().is_empty());
    let window_hwnd = object
        .get("window_hwnd")
        .or_else(|| object.get("windowHwnd"))
        .and_then(Value::as_i64);
    match (window_hwnd, cdp_target_id) {
        (Some(window_hwnd), Some(cdp_target_id)) => Some(json!({
            "kind": "cdp",
            "window_hwnd": window_hwnd,
            "cdp_target_id": cdp_target_id,
            "source": source,
        })),
        (None, Some(cdp_target_id)) => Some(json!({
            "kind": "cdp",
            "cdp_target_id": cdp_target_id,
            "source": source,
        })),
        (Some(hwnd), None) => Some(json!({
            "kind": "window",
            "hwnd": hwnd,
            "source": source,
        })),
        (None, None) => None,
    }
}

fn effective_target_from_source_id(source_id: &str, source: &'static str) -> Option<Value> {
    let mut window_hwnd = None;
    let mut cdp_target_id = None;
    for part in source_id.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "window_hwnd" => window_hwnd = parse_hwnd_literal(value.trim()),
            "cdp_target_id" => {
                let target = value.trim();
                if !target.is_empty() {
                    cdp_target_id = Some(target.to_owned());
                }
            }
            _ => {}
        }
    }
    match (window_hwnd, cdp_target_id) {
        (Some(window_hwnd), Some(cdp_target_id)) => Some(json!({
            "kind": "cdp",
            "window_hwnd": window_hwnd,
            "cdp_target_id": cdp_target_id,
            "source": source,
        })),
        (None, Some(cdp_target_id)) => Some(json!({
            "kind": "cdp",
            "cdp_target_id": cdp_target_id,
            "source": source,
        })),
        (Some(hwnd), None) => Some(json!({
            "kind": "window",
            "hwnd": hwnd,
            "source": source,
        })),
        (None, None) => None,
    }
}

fn parse_hwnd_literal(value: &str) -> Option<i64> {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .and_then(|hex| i64::from_str_radix(hex, 16).ok())
        .or_else(|| value.parse::<i64>().ok())
}

fn profile_policy_denied(error: &ErrorData) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        == Some(error_codes::TOOL_PROFILE_POLICY_DENIED)
}

fn daemon_restarting_mcp_error(
    tool_name: &str,
    mcp_session_id: Option<&str>,
    snapshot: super::drain::DaemonDrainSnapshot,
) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("daemon is restarting; tool {tool_name} was refused before transport shutdown"),
        Some(json!({
            "code": synapse_core::error_codes::DAEMON_RESTARTING,
            "retryable": true,
            "tool": tool_name,
            "mcp_session_id": mcp_session_id,
            "drain": snapshot,
        })),
    )
}

fn routed_call_cancelled_mcp_error(
    tool_name: &str,
    mcp_session_id: Option<&str>,
    reason: &'static str,
) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("tool {tool_name} was cancelled before physical mutation admission"),
        Some(json!({
            "code": synapse_core::error_codes::DAEMON_RESTARTING,
            "detail_code": "MCP_ROUTED_CALL_CANCELLED_BEFORE_MUTATION",
            "retryable": true,
            "tool": tool_name,
            "mcp_session_id": mcp_session_id,
            "reason": reason,
            "source_of_truth": "McpOperatorPanicBoundary::mutation_state",
        })),
    )
}

fn routed_lifecycle_owner_missing_mcp_error(
    tool_name: &str,
    mcp_session_id: Option<&str>,
    stage: &'static str,
) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("supervised routed tool {tool_name} lost its daemon lifecycle owner"),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "MCP_ROUTED_CALL_LIFECYCLE_OWNER_MISSING",
            "tool": tool_name,
            "mcp_session_id": mcp_session_id,
            "stage": stage,
            "source_of_truth": "routed-call lifecycle-owner transfer slot + daemon lifecycle Drop backstop",
        })),
    )
}

fn authority_join_mcp_error(
    tool_name: &str,
    mcp_session_id: Option<&str>,
    error: &super::AuthorityTransactionJoinError,
) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("supervised routed tool {tool_name} failed to reach a clean terminal join"),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "MCP_ROUTED_CALL_OWNER_JOIN_FAILED",
            "tool": tool_name,
            "mcp_session_id": mcp_session_id,
            "cancelled": error.is_cancelled(),
            "panicked": error.is_panic(),
            "detail": error.to_string(),
            "source_of_truth": "AuthorityFinalizerSupervisor transaction registry + TaskTracker",
        })),
    )
}

fn routed_result_channel_mcp_error(tool_name: &str, error: oneshot::error::RecvError) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("supervised routed tool {tool_name} closed without publishing its result"),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "MCP_ROUTED_CALL_RESULT_CHANNEL_CLOSED",
            "tool": tool_name,
            "detail": error.to_string(),
            "source_of_truth": "routed-call result oneshot + AuthorityFinalizerSupervisor",
        })),
    )
}

fn lifecycle_mcp_error(error: anyhow::Error) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("daemon lifecycle ledger failure: {error:#}"),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "MCP_DAEMON_LIFECYCLE_WRITE_FAILED",
            "daemon_lifecycle": crate::daemon_lifecycle::diagnostic_value(),
        })),
    )
}

fn tool_panic_mcp_error(tool_name: &str, mcp_session_id: Option<&str>) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode(-32099),
        format!("tool {tool_name} panicked; daemon lifecycle ledger captured the panic"),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "MCP_TOOL_PANIC",
            "tool": tool_name,
            "mcp_session_id": mcp_session_id,
            "daemon_lifecycle": crate::daemon_lifecycle::diagnostic_value(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RoutedFutureDropProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for RoutedFutureDropProbe {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn routed_read_caller_drop_cancels_before_mutation_and_drops_exact_future() {
        let boundary = super::super::operator_panic_boundary::McpOperatorPanicBoundary::capture(
            "synthetic_routed_read",
            Some("session-test"),
        );
        let future_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let future_drop_probe = std::sync::Arc::clone(&future_dropped);
        let (mut result_sender, result_receiver) = oneshot::channel::<()>();
        drop(result_receiver);

        let (execution, late_mutation_error) =
            super::super::operator_panic_boundary::MCP_OPERATOR_PANIC_BOUNDARY
                .scope(boundary, async move {
                    let routed_read = async move {
                        let _drop_probe = RoutedFutureDropProbe(future_drop_probe);
                        std::future::pending::<()>().await;
                    };
                    let execution = await_routed_tool_call(
                        routed_read,
                        &mut result_sender,
                        CancellationToken::new(),
                    )
                    .await;
                    let late_mutation_error =
                        super::super::operator_panic_boundary::ensure_mcp_mutation(
                            "synthetic_late_mutation",
                        )
                        .expect_err("caller cancellation must close later mutation admission");
                    (execution, late_mutation_error)
                })
                .await;

        assert!(matches!(
            execution,
            RoutedToolExecution::CancelledBeforeMutation("caller_result_receiver_closed")
        ));
        assert!(
            future_dropped.load(std::sync::atomic::Ordering::Acquire),
            "pre-mutation caller cancellation must synchronously drop the exact routed future"
        );
        assert_eq!(
            late_mutation_error
                .data
                .as_ref()
                .and_then(|data| data.get("detail_code")),
            Some(&json!("MCP_ROUTED_CALL_CANCELLED_BEFORE_MUTATION"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn routed_mutation_survives_caller_drop_and_supervisor_drain_until_terminal() {
        synapse_action::isolate_interrupt_epochs_for_test();
        let baseline = super::super::operator_panic_boundary::mcp_mutation_activity_snapshot()
            .unwrap_or_else(|error| panic!("read baseline MCP mutation activity: {error}"));
        let service = SynapseService::new();
        let boundary = super::super::operator_panic_boundary::McpOperatorPanicBoundary::capture(
            "synthetic_routed_mutation",
            Some("session-test"),
        );
        let release = CancellationToken::new();
        let child_release = release.clone();
        let terminal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_terminal = std::sync::Arc::clone(&terminal);
        let (armed_sender, armed_receiver) = oneshot::channel();
        let (drain_observed_sender, drain_observed_receiver) = oneshot::channel();
        let (result_sender, result_receiver) = oneshot::channel::<()>();

        let caller = service
            .spawn_cooperative_authority_transaction(move |supervisor_cancellation| async move {
                let route_cancellation = supervisor_cancellation.clone();
                let routed_mutation = async move {
                    super::super::operator_panic_boundary::ensure_mcp_mutation(
                        "synthetic_physical_mutation",
                    )?;
                    let _armed = armed_sender.send(());
                    route_cancellation.cancelled().await;
                    let _drain_observed = drain_observed_sender.send(());
                    child_release.cancelled().await;
                    Ok::<(), ErrorData>(())
                };
                let mut result_sender = result_sender;
                let execution = super::super::operator_panic_boundary::MCP_OPERATOR_PANIC_BOUNDARY
                    .scope(boundary, async move {
                        await_routed_tool_call(
                            routed_mutation,
                            &mut result_sender,
                            supervisor_cancellation,
                        )
                        .await
                    })
                    .await;
                assert!(matches!(execution, RoutedToolExecution::Completed(Ok(()))));
                child_terminal.store(true, std::sync::atomic::Ordering::Release);
            })
            .unwrap_or_else(|error| panic!("spawn routed mutation owner: {error:?}"));

        armed_receiver
            .await
            .unwrap_or_else(|error| panic!("routed mutation did not reserve ownership: {error}"));
        let armed = super::super::operator_panic_boundary::mcp_mutation_activity_snapshot()
            .unwrap_or_else(|error| panic!("read armed MCP mutation activity: {error}"));
        assert_eq!(armed.in_flight, baseline.in_flight + 1);

        // Model both transport cancellation and the handler future being
        // dropped. Neither owns the routed mutation after reservation.
        drop(result_receiver);
        drop(caller);
        let drain_service = service.clone();
        let drain = tokio::spawn(async move { drain_service.drain_authority_finalizers().await });
        drain_observed_receiver
            .await
            .unwrap_or_else(|error| panic!("routed mutation did not observe drain: {error}"));
        tokio::task::yield_now().await;
        assert!(
            !terminal.load(std::sync::atomic::Ordering::Acquire),
            "armed routed mutation must not publish terminal state before its exact owner finishes"
        );
        assert!(
            !drain.is_finished(),
            "daemon drain must retain an armed routed mutation owner"
        );
        assert_eq!(
            super::super::operator_panic_boundary::mcp_mutation_activity_snapshot()
                .unwrap_or_else(|error| panic!("read retained MCP mutation activity: {error}"))
                .in_flight,
            baseline.in_flight + 1
        );

        release.cancel();
        let readback = drain
            .await
            .unwrap_or_else(|error| panic!("join authority drain: {error}"))
            .unwrap_or_else(|error| panic!("drain routed mutation owner: {error}"));
        assert!(terminal.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(readback.registered_tasks_before, 1);
        assert_eq!(readback.cancellation_signals_sent, 1);
        assert_eq!(readback.abort_requests_sent, 0);
        assert_eq!(readback.registered_tasks_after, 0);
        assert_eq!(readback.tracked_tasks_after, 0);
        assert!(readback.safe_to_unlock());
        assert_eq!(
            super::super::operator_panic_boundary::mcp_mutation_activity_snapshot()
                .unwrap_or_else(|error| panic!("read terminal MCP mutation activity: {error}"))
                .in_flight,
            baseline.in_flight
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervisor_drain_cancels_unarmed_routed_read_after_caller_drop() {
        let service = SynapseService::new();
        let boundary = super::super::operator_panic_boundary::McpOperatorPanicBoundary::capture(
            "synthetic_supervised_read",
            Some("session-test"),
        );
        let future_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let future_drop_probe = std::sync::Arc::clone(&future_dropped);
        let (started_sender, started_receiver) = oneshot::channel();
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        let (result_sender, result_receiver) = oneshot::channel::<()>();
        let caller = service
            .spawn_cooperative_authority_transaction(move |supervisor_cancellation| async move {
                let routed_read = async move {
                    let _drop_probe = RoutedFutureDropProbe(future_drop_probe);
                    let _started = started_sender.send(());
                    std::future::pending::<()>().await;
                };
                let mut result_sender = result_sender;
                let execution = super::super::operator_panic_boundary::MCP_OPERATOR_PANIC_BOUNDARY
                    .scope(boundary, async move {
                        await_routed_tool_call(
                            routed_read,
                            &mut result_sender,
                            supervisor_cancellation,
                        )
                        .await
                    })
                    .await;
                let reason = match execution {
                    RoutedToolExecution::CancelledBeforeMutation(reason) => reason,
                    RoutedToolExecution::Completed(()) => "unexpected_completion",
                    RoutedToolExecution::Panicked(_) => "unexpected_panic",
                };
                let _terminal = terminal_sender.send(reason);
            })
            .unwrap_or_else(|error| panic!("spawn supervised routed read: {error:?}"));
        started_receiver
            .await
            .unwrap_or_else(|error| panic!("supervised routed read did not start: {error}"));
        drop(caller);

        let readback = service
            .drain_authority_finalizers()
            .await
            .unwrap_or_else(|error| panic!("drain supervised routed read: {error}"));
        assert_eq!(
            terminal_receiver
                .await
                .unwrap_or_else(|error| panic!("read routed terminal cause: {error}")),
            "authority_supervisor_shutdown"
        );
        assert!(future_dropped.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(readback.registered_tasks_before, 1);
        assert_eq!(readback.cancellation_signals_sent, 1);
        assert!(readback.safe_to_unlock());
        drop(result_receiver);
    }

    #[test]
    fn extracts_effective_target_from_browser_dom_content_result() {
        let result = CallToolResult::structured(json!({
            "operation": "content",
            "content": {
                "window_hwnd": 47060598,
                "cdp_target_id": "chrome-tab:600757323",
                "html": "<html>large payload must not be copied into target metadata</html>"
            }
        }));

        let target = effective_target_from_tool_result(&result).expect("effective target");
        assert_eq!(target.get("kind").and_then(Value::as_str), Some("cdp"));
        assert_eq!(
            target.get("window_hwnd").and_then(Value::as_i64),
            Some(47060598)
        );
        assert_eq!(
            target.get("cdp_target_id").and_then(Value::as_str),
            Some("chrome-tab:600757323")
        );
        assert_eq!(
            target.get("source").and_then(Value::as_str),
            Some("structured_content.content")
        );
        assert!(target.get("html").is_none());
    }

    #[test]
    fn extracts_effective_target_from_error_source_id() {
        let error = json!({
            "data": {
                "source_id": "window_hwnd=0x2ce1676;cdp_target_id=chrome-tab:600757326;query_len=9"
            }
        });

        let target = effective_target_from_error_snapshot(&error).expect("effective target");
        assert_eq!(
            target.get("window_hwnd").and_then(Value::as_i64),
            Some(47060598)
        );
        assert_eq!(
            target.get("cdp_target_id").and_then(Value::as_str),
            Some("chrome-tab:600757326")
        );
        assert_eq!(
            target.get("source").and_then(Value::as_str),
            Some("error.data.source_id")
        );
    }

    #[test]
    fn ignores_tool_result_without_target_metadata() {
        let result = CallToolResult::structured(json!({
            "ok": true,
            "html": "<html>no target fields</html>"
        }));

        assert!(effective_target_from_tool_result(&result).is_none());
    }
}
