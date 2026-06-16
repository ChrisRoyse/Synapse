use super::{
    ErrorData, Implementation, ServerCapabilities, ServerHandler, ServerInfo, SessionTarget,
    SynapseService, mcp_error, tool_handler,
};
use futures_util::FutureExt as _;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;
use synapse_core::error_codes;

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SynapseService {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        self.dispatch_tool_call_with_profile_policy(request, context)
            .await
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
        if let Some(session_id) = mcp_session_id.as_deref() {
            self.record_tools_list_surface_readback(session_id, &tools)?;
        }
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

impl SynapseService {
    pub(crate) async fn dispatch_tool_call_with_profile_policy(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, ErrorData> {
        let tool_name = request.name.to_string();
        let mcp_session_id = super::context::mcp_session_id_from_request_context(&context)?;
        let lifecycle_guard =
            self.begin_daemon_lifecycle_tool_call(&tool_name, mcp_session_id.as_deref())?;
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
            lifecycle_guard
                .finish_error(error_snapshot(&error))
                .map_err(lifecycle_mcp_error)?;
            return Err(error);
        }
        let shutdown_cancel = self.shutdown_cancel_token()?;
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
        let context = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let tool_call = AssertUnwindSafe(self.tool_router.call(context)).catch_unwind();
        tokio::pin!(tool_call);
        let result = tokio::select! {
            _ = drain_cancel.cancelled() => {
                let snapshot = drain_state.mark_draining("drain_token");
                let error = daemon_restarting_mcp_error(&tool_name, mcp_session_id.as_deref(), snapshot);
                lifecycle_guard
                    .finish_error(error_snapshot(&error))
                    .map_err(lifecycle_mcp_error)?;
                return Err(error);
            }
            _ = shutdown_cancel.cancelled() => {
                let snapshot = drain_state.mark_draining("shutdown_token");
                let error = daemon_restarting_mcp_error(&tool_name, mcp_session_id.as_deref(), snapshot);
                lifecycle_guard
                    .finish_error(error_snapshot(&error))
                    .map_err(lifecycle_mcp_error)?;
                return Err(error);
            }
            result = &mut tool_call => result,
        };
        match result {
            Ok(Ok(result)) => {
                lifecycle_guard.finish_ok().map_err(lifecycle_mcp_error)?;
                Ok(result)
            }
            Ok(Err(error)) => {
                let error = normalize_tool_error(&tool_name, error);
                lifecycle_guard
                    .finish_error(error_snapshot(&error))
                    .map_err(lifecycle_mcp_error)?;
                Err(error)
            }
            Err(payload) => {
                let panic_message =
                    crate::daemon_lifecycle::panic_payload_message(payload.as_ref());
                std::mem::forget(payload);
                let panic = json!({
                    "payload": panic_message,
                    "tool": tool_name,
                    "mcp_session_id": mcp_session_id.clone(),
                });
                lifecycle_guard
                    .finish_panic(panic)
                    .map_err(lifecycle_mcp_error)?;
                Err(tool_panic_mcp_error(&tool_name, mcp_session_id.as_deref()))
            }
        }
    }
    fn reject_terminated_session_tool_call(
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
        crate::daemon_lifecycle::begin_tool_call(crate::daemon_lifecycle::ToolCallStart {
            tool: tool_name.to_owned(),
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

fn normalize_tool_error(tool_name: &str, error: ErrorData) -> ErrorData {
    if error.data.is_none() && error.message == "tool not found" {
        return mcp_error(
            synapse_core::error_codes::TOOL_NOT_FOUND,
            format!("tool not found: {tool_name}"),
        );
    }
    if error.data.is_none() && error.code == rmcp::model::ErrorCode::INVALID_PARAMS {
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
