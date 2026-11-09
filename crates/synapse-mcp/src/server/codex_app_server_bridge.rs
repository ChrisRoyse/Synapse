//! Bridges Codex app-server client requests into Synapse's durable approval queue.
//!
//! The PowerShell app-server runner is the JSON-RPC client, so it is the only
//! process that can answer app-server server-to-client requests. This module
//! gives that runner a bearer-protected local HTTP endpoint: create the
//! `CF_KV` approval row, wait for the operator decision or timeout, then return
//! the app-server response payload the runner should send back.

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use synapse_storage::Db;

use crate::m3::approvals::{
    self, ApprovalAllow, ApprovalDecision, ApprovalItemRecord, ApprovalKind, ApprovalRequestParams,
    ApprovalRowEvidence, ApprovalStatus, ApprovalTimeoutDecision,
};

use super::permission_policy::{self, GateDecision};

const MAX_SPAWN_ID_CHARS: usize = 128;
pub(crate) const MAX_CODEX_APP_SERVER_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const DEFAULT_CODEX_REQUEST_TIMEOUT_MS: u64 = 25 * 60 * 1_000;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexAppServerRequestEnvelope {
    pub spawn_id: String,
    pub method: String,
    pub id: Value,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexAppServerBridgeResponse {
    pub ok: bool,
    pub spawn_id: String,
    pub method: String,
    pub request_id: Value,
    pub approval_id: String,
    pub approval_kind: ApprovalKind,
    pub final_status: ApprovalStatus,
    pub app_server_response: Value,
    pub item_row: ApprovalRowEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_row: Option<ApprovalRowEvidence>,
    #[serde(default)]
    pub timed_out_by_bridge: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexAppServerBridgeError {
    pub http_status: u16,
    pub code: &'static str,
    pub detail: String,
}

impl CodexAppServerBridgeError {
    fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            http_status: 400,
            code: "CODEX_APP_SERVER_REQUEST_INVALID",
            detail: detail.into(),
        }
    }

    fn unknown_spawn(detail: impl Into<String>) -> Self {
        Self {
            http_status: 404,
            code: "CODEX_APP_SERVER_UNKNOWN_SPAWN",
            detail: detail.into(),
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            http_status: 422,
            code: "CODEX_APP_SERVER_REQUEST_UNSUPPORTED",
            detail: detail.into(),
        }
    }

    fn storage(detail: impl Into<String>) -> Self {
        Self {
            http_status: 500,
            code: "CODEX_APP_SERVER_APPROVAL_BRIDGE_FAILED",
            detail: detail.into(),
        }
    }

    pub(crate) fn response_body(&self) -> Value {
        json!({
            "ok": false,
            "code": self.code,
            "detail": self.detail,
        })
    }
}

pub(crate) async fn handle_codex_app_server_request(
    db: &Arc<Db>,
    envelope: CodexAppServerRequestEnvelope,
) -> Result<CodexAppServerBridgeResponse, CodexAppServerBridgeError> {
    validate_spawn(&envelope.spawn_id)?;
    if !is_supported_request_id(&envelope.id) {
        return Err(CodexAppServerBridgeError::bad_request(
            "app-server request id must be a string or integer",
        ));
    }
    let spec = request_spec(&envelope)?;
    let created = approvals::request_approval(db, &spec.request, &spec.by_session)
        .map_err(|error| CodexAppServerBridgeError::storage(error.message.to_string()))?;
    let approval_id = created.item.approval_id.clone();
    if let Some(auto_accept_note) = spec.auto_accept_note.as_ref() {
        let accept = approvals::ApprovalDecideParams {
            approval_id: approval_id.clone(),
            decision: ApprovalDecision::Accept,
            note: Some(auto_accept_note.clone()),
            snooze_ms: None,
            edited_args: None,
            response: None,
        };
        let decision = approvals::decide_approval(db, &accept, "codex_app_server_auto_policy")
            .map_err(|error| CodexAppServerBridgeError::storage(error.message.to_string()))?;
        let app_server_response = app_server_response_for(&spec, &decision.item);
        return Ok(CodexAppServerBridgeResponse {
            ok: true,
            spawn_id: envelope.spawn_id,
            method: envelope.method,
            request_id: envelope.id,
            approval_id,
            approval_kind: spec.kind,
            final_status: decision.item.status,
            app_server_response,
            item_row: created.item_row,
            audit_row: Some(decision.audit_row),
            timed_out_by_bridge: false,
        });
    }
    let timeout = codex_request_timeout();
    let started = tokio::time::Instant::now();
    let (item, audit_row, timed_out_by_bridge) = loop {
        let item = approvals::get_approval(db, &approval_id)
            .map_err(|error| CodexAppServerBridgeError::storage(error.message.to_string()))?
            .map(|queued| queued.item)
            .ok_or_else(|| {
                CodexAppServerBridgeError::storage(format!(
                    "approval row {approval_id} vanished while Codex app-server request was blocked"
                ))
            })?;
        match item.status {
            ApprovalStatus::Accepted | ApprovalStatus::Declined | ApprovalStatus::Ignored => {
                break (item, None, false);
            }
            ApprovalStatus::Pending | ApprovalStatus::Snoozed => {}
        }
        if started.elapsed() >= timeout {
            let note = format!(
                "Codex app-server request {} timed out after {}s; bridge denied it.",
                envelope.method,
                timeout.as_secs()
            );
            let decline = approvals::ApprovalDecideParams {
                approval_id: approval_id.clone(),
                decision: ApprovalDecision::Decline,
                note: Some(note),
                snooze_ms: None,
                edited_args: None,
                response: None,
            };
            let decision =
                approvals::decide_approval(db, &decline, "codex_app_server_request_timeout")
                    .map_err(|error| {
                        CodexAppServerBridgeError::storage(error.message.to_string())
                    })?;
            break (decision.item, Some(decision.audit_row), true);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    let app_server_response = app_server_response_for(&spec, &item);
    Ok(CodexAppServerBridgeResponse {
        ok: true,
        spawn_id: envelope.spawn_id,
        method: envelope.method,
        request_id: envelope.id,
        approval_id,
        approval_kind: spec.kind,
        final_status: item.status,
        app_server_response,
        item_row: created.item_row,
        audit_row,
        timed_out_by_bridge,
    })
}

#[derive(Clone, Debug)]
struct RequestSpec {
    kind: ApprovalKind,
    request: ApprovalRequestParams,
    by_session: String,
    response_kind: ResponseKind,
    params: Value,
    auto_accept_note: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseKind {
    CommandExecutionApproval,
    FileChangeApproval,
    PermissionProfileApproval,
    ToolRequestUserInput,
    McpServerElicitation,
}

fn request_spec(
    envelope: &CodexAppServerRequestEnvelope,
) -> Result<RequestSpec, CodexAppServerBridgeError> {
    let params_obj = envelope.params.as_object().ok_or_else(|| {
        CodexAppServerBridgeError::bad_request("app-server request params must be a JSON object")
    })?;
    let by_session = format!("codex_app_server:{}", envelope.spawn_id);
    let timeout_ms = Some(codex_request_timeout().as_millis() as u64 + 60_000);
    let payload_json = payload_json(envelope)?;
    let dedupe_key = dedupe_key(envelope, params_obj);
    let allow_permission = Some(ApprovalAllow {
        accept: true,
        edit: false,
        respond: false,
        ignore: true,
    });
    let allow_question = Some(ApprovalAllow {
        accept: false,
        edit: false,
        respond: true,
        ignore: true,
    });

    let (kind, title, body, destructive, response_kind, allow, auto_accept_note) =
        match envelope.method.as_str() {
            "item/commandExecution/requestApproval" => (
                ApprovalKind::AgentPermission,
                "Codex approval needed: command execution".to_owned(),
                command_body(params_obj),
                true,
                ResponseKind::CommandExecutionApproval,
                allow_permission,
                None,
            ),
            "item/fileChange/requestApproval" => (
                ApprovalKind::AgentPermission,
                "Codex approval needed: file change".to_owned(),
                file_change_body(params_obj),
                true,
                ResponseKind::FileChangeApproval,
                allow_permission,
                None,
            ),
            "item/permissions/requestApproval" => (
                ApprovalKind::AgentPermission,
                "Codex approval needed: permissions".to_owned(),
                permissions_body(params_obj),
                true,
                ResponseKind::PermissionProfileApproval,
                allow_permission,
                None,
            ),
            "item/tool/requestUserInput" => (
                ApprovalKind::AgentQuestion,
                "Codex needs input".to_owned(),
                request_user_input_body(params_obj),
                false,
                ResponseKind::ToolRequestUserInput,
                allow_question,
                None,
            ),
            "mcpServer/elicitation/request" => {
                if is_mcp_tool_call_elicitation(params_obj) {
                    let auto_accept_note = codex_mcp_tool_auto_accept_note(params_obj);
                    (
                        ApprovalKind::AgentPermission,
                        "Codex approval needed: MCP tool".to_owned(),
                        mcp_elicitation_body(params_obj),
                        false,
                        ResponseKind::McpServerElicitation,
                        allow_permission,
                        auto_accept_note,
                    )
                } else {
                    (
                        ApprovalKind::AgentQuestion,
                        "Codex MCP elicitation".to_owned(),
                        mcp_elicitation_body(params_obj),
                        false,
                        ResponseKind::McpServerElicitation,
                        allow_question,
                        None,
                    )
                }
            }
            other => {
                return Err(CodexAppServerBridgeError::unsupported(format!(
                    "Codex app-server request method {other:?} is not bridged"
                )));
            }
        };

    Ok(RequestSpec {
        kind,
        request: ApprovalRequestParams {
            kind,
            title,
            body,
            payload_json: Some(payload_json),
            dedupe_key: Some(dedupe_key),
            timeout_ms,
            timeout_decision: Some(ApprovalTimeoutDecision::Declined),
            destructive,
            notify: true,
            suppress_popup: false,
            allow,
        },
        by_session,
        response_kind,
        params: envelope.params.clone(),
        auto_accept_note,
    })
}

fn app_server_response_for(spec: &RequestSpec, item: &ApprovalItemRecord) -> Value {
    let accepted = item.status == ApprovalStatus::Accepted;
    match spec.response_kind {
        ResponseKind::CommandExecutionApproval => json!({
            "decision": if accepted { "accept" } else { "decline" }
        }),
        ResponseKind::FileChangeApproval => json!({
            "decision": if accepted { "accept" } else { "decline" }
        }),
        ResponseKind::PermissionProfileApproval => {
            if accepted {
                let requested = spec
                    .params
                    .get("permissions")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let mut granted = Map::new();
                if let Some(network) = requested.get("network") {
                    if !network.is_null() {
                        granted.insert("network".to_owned(), network.clone());
                    }
                }
                if let Some(file_system) = requested.get("fileSystem") {
                    if !file_system.is_null() {
                        granted.insert("fileSystem".to_owned(), file_system.clone());
                    }
                }
                json!({ "permissions": Value::Object(granted), "scope": "turn" })
            } else {
                json!({ "permissions": {}, "scope": "turn" })
            }
        }
        ResponseKind::ToolRequestUserInput => {
            if !accepted {
                return json!({ "answers": {} });
            }
            let response = item.operator_response.as_deref().unwrap_or_default();
            json!({ "answers": user_input_answers(&spec.params, response) })
        }
        ResponseKind::McpServerElicitation => {
            if accepted {
                json!({
                    "action": "accept",
                    "content": elicitation_content(item.operator_response.as_deref().unwrap_or_default()),
                    "_meta": null
                })
            } else {
                json!({ "action": "decline", "content": null, "_meta": null })
            }
        }
    }
}

fn user_input_answers(params: &Value, response: &str) -> Value {
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(response) {
        return value;
    }
    let Some(question_id) = params
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .and_then(|question| question.get("id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return json!({});
    };
    if response.trim().is_empty() {
        json!({})
    } else {
        json!({ question_id: { "answers": [response] } })
    }
}

fn elicitation_content(response: &str) -> Value {
    if response.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(response).unwrap_or_else(|_| Value::String(response.to_owned()))
}

fn payload_json(
    envelope: &CodexAppServerRequestEnvelope,
) -> Result<String, CodexAppServerBridgeError> {
    let full = json!({
        "source": "codex_app_server",
        "spawn_id": envelope.spawn_id,
        "method": envelope.method,
        "request_id": envelope.id,
        "params": envelope.params,
    });
    let encoded = serde_json::to_string(&full).map_err(|error| {
        CodexAppServerBridgeError::bad_request(format!("request payload encode failed: {error}"))
    })?;
    if encoded.len() <= 60 * 1024 {
        return Ok(encoded);
    }
    let truncated = json!({
        "source": "codex_app_server",
        "spawn_id": envelope.spawn_id,
        "method": envelope.method,
        "request_id": envelope.id,
        "params_truncated": true,
        "params_bytes": encoded.len(),
        "params_preview": encoded.chars().take(2_000).collect::<String>(),
    });
    serde_json::to_string(&truncated).map_err(|error| {
        CodexAppServerBridgeError::bad_request(format!("request payload encode failed: {error}"))
    })
}

fn dedupe_key(envelope: &CodexAppServerRequestEnvelope, params: &Map<String, Value>) -> String {
    let item_id = params
        .get("itemId")
        .and_then(Value::as_str)
        .or_else(|| params.get("item_id").and_then(Value::as_str))
        .or_else(|| params.get("elicitationId").and_then(Value::as_str))
        .map(bounded_key_part)
        .unwrap_or_else(|| request_id_key_part(&envelope.id));
    let callback_id = params
        .get("approvalId")
        .and_then(Value::as_str)
        .map(bounded_key_part)
        .unwrap_or_else(|| "default".to_owned());
    format!(
        "codex:{}:{}:{}:{}",
        envelope.spawn_id, envelope.method, item_id, callback_id
    )
}

fn request_id_key_part(id: &Value) -> String {
    let value = match id {
        Value::String(value) => format!("request-{value}"),
        Value::Number(value) => format!("request-{value}"),
        _ => "request-unknown".to_owned(),
    };
    bounded_key_part(&value)
}

fn bounded_key_part(value: &str) -> String {
    value.chars().take(64).collect()
}

fn command_body(params: &Map<String, Value>) -> String {
    let command = params
        .get("command")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "(command unavailable)".to_owned());
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "(cwd unavailable)".to_owned());
    bounded_body(format!("Codex wants to run command in {cwd}: {command}"))
}

fn file_change_body(params: &Map<String, Value>) -> String {
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "file change requires approval".to_owned());
    let grant_root = params
        .get("grantRoot")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "(no grant root)".to_owned());
    bounded_body(format!(
        "Codex wants to change files. reason={reason}; grant_root={grant_root}"
    ))
}

fn permissions_body(params: &Map<String, Value>) -> String {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "(cwd unavailable)".to_owned());
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "permission request".to_owned());
    bounded_body(format!(
        "Codex wants additional permissions in {cwd}. reason={reason}"
    ))
}

fn request_user_input_body(params: &Map<String, Value>) -> String {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|question| question.get("question").and_then(Value::as_str))
                .map(single_line)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Codex requested operator input.".to_owned());
    bounded_body(questions)
}

fn mcp_elicitation_body(params: &Map<String, Value>) -> String {
    let server = params
        .get("serverName")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "(unknown MCP server)".to_owned());
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .map(single_line)
        .unwrap_or_else(|| "MCP server requested input.".to_owned());
    bounded_body(format!("{server}: {message}"))
}

fn is_mcp_tool_call_elicitation(params: &Map<String, Value>) -> bool {
    params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("codex_approval_kind"))
        .and_then(Value::as_str)
        == Some("mcp_tool_call")
}

fn codex_mcp_tool_auto_accept_note(params: &Map<String, Value>) -> Option<String> {
    let server = params.get("serverName")?.as_str()?;
    if server != "synapse" {
        return None;
    }
    let tool = codex_mcp_tool_name(params)?;
    let tool_input = params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("tool_params"))
        .unwrap_or(&Value::Null);
    let policy_name = format!("mcp__{server}__{tool}");
    match permission_policy::classify(&policy_name, tool_input) {
        GateDecision::AutoAllow => Some(format!(
            "codex_app_server_auto_allow_safe_mcp_tool: {policy_name}"
        )),
        GateDecision::Gate { .. } => None,
    }
}

fn codex_mcp_tool_name(params: &Map<String, Value>) -> Option<String> {
    params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| {
            meta.get("tool_name")
                .or_else(|| meta.get("tool"))
                .or_else(|| meta.get("name"))
        })
        .and_then(Value::as_str)
        .filter(|tool| is_mcp_tool_name_shape(tool))
        .map(str::to_owned)
        .or_else(|| {
            params
                .get("message")
                .and_then(Value::as_str)
                .and_then(extract_tool_name_from_message)
        })
}

fn extract_tool_name_from_message(message: &str) -> Option<String> {
    let (_, after_marker) = message.split_once("tool \"")?;
    let (tool, _) = after_marker.split_once('"')?;
    if is_mcp_tool_name_shape(tool) {
        Some(tool.to_owned())
    } else {
        None
    }
}

fn is_mcp_tool_name_shape(tool: &str) -> bool {
    !tool.is_empty()
        && tool.len() <= 128
        && tool
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn bounded_body(mut value: String) -> String {
    value.truncate(4_000);
    value
}

fn is_supported_request_id(id: &Value) -> bool {
    id.as_str().is_some() || id.as_i64().is_some() || id.as_u64().is_some()
}

fn validate_spawn(spawn_id: &str) -> Result<(), CodexAppServerBridgeError> {
    validate_spawn_id_shape(spawn_id).map_err(CodexAppServerBridgeError::bad_request)?;
    let spawn_dir = spawn_root()
        .map_err(CodexAppServerBridgeError::storage)?
        .join(spawn_id);
    if !spawn_dir.is_dir() {
        return Err(CodexAppServerBridgeError::unknown_spawn(format!(
            "spawn_id {spawn_id:?} was never issued by act_spawn_agent on this daemon (no spawn directory at {})",
            spawn_dir.display()
        )));
    }
    Ok(())
}

fn spawn_root() -> Result<PathBuf, String> {
    super::m4_tools::agent_spawn_root_dir().map_err(|error| error.message.to_string())
}

fn validate_spawn_id_shape(spawn_id: &str) -> Result<(), String> {
    if !spawn_id.starts_with("agent-spawn-") {
        return Err(format!(
            "spawn_id must start with \"agent-spawn-\", got {spawn_id:?}"
        ));
    }
    if spawn_id.len() > MAX_SPAWN_ID_CHARS {
        return Err(format!(
            "spawn_id exceeds {MAX_SPAWN_ID_CHARS} chars ({})",
            spawn_id.len()
        ));
    }
    if !spawn_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(
            "spawn_id must contain only ASCII alphanumerics and dashes (path-safety invariant)"
                .to_owned(),
        );
    }
    Ok(())
}

fn codex_request_timeout() -> Duration {
    let ms = std::env::var("SYNAPSE_CODEX_APP_SERVER_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms >= 1_000)
        .unwrap_or(DEFAULT_CODEX_REQUEST_TIMEOUT_MS);
    Duration::from_millis(ms)
}
