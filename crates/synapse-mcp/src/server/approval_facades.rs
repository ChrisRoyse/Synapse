use super::{
    ApprovalDecideParams, ApprovalDecideResponse, ApprovalListParams, ApprovalListResponse,
    ApprovalRequestParams, ApprovalRequestResponse, ErrorData, Json, Parameters, SynapseService,
    escalation::{
        EscalationAckParams, EscalationAckResponse, EscalationConfigGetParams,
        EscalationConfigResponse, EscalationConfigSetParams, EscalationListParams,
        EscalationListResponse,
    },
    permission_gate::{AgentAskOperatorParams, ApprovalGateParams},
    tool, tool_router,
};

use rmcp::{
    RoleServer,
    model::{CallToolResult, ErrorCode},
    schemars::JsonSchema,
    service::RequestContext,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const APPROVAL_TOOL: &str = "approval";
const ESCALATION_TOOL: &str = "escalation";
const APPROVAL_SOURCE_OF_TRUTH: &str =
    "CF_KV approval/v1/item rows + approval/v1/audit rows + daemon-tool-events.jsonl";
const ESCALATION_SOURCE_OF_TRUTH: &str =
    "CF_KV escalation/v1/config + escalation/v1/item rows + escalation/v1/audit rows";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOperation {
    Request,
    List,
    Decide,
    Gate,
    AskOperator,
}

impl ApprovalOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::List => "list",
            Self::Decide => "decide",
            Self::Gate => "gate",
            Self::AskOperator => "ask_operator",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalParams {
    #[serde(default)]
    pub operation: Option<ApprovalOperation>,
    #[serde(default)]
    pub request: Option<ApprovalRequestParams>,
    #[serde(default)]
    pub list: Option<ApprovalListParams>,
    #[serde(default)]
    pub decide: Option<ApprovalDecideParams>,
    #[serde(default)]
    pub gate: Option<ApprovalGateParams>,
    #[serde(default)]
    pub ask_operator: Option<AgentAskOperatorParams>,
    /// Claude Code `--permission-prompt-tool` calls one MCP tool directly with
    /// the gate payload shape. These top-level fields are accepted only when
    /// `operation` is omitted and are normalized to `operation=gate`.
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub spawn_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalVerdictReadback {
    pub value: Value,
    pub content_text: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResponse {
    pub operation: ApprovalOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    /// Top-level Claude Code permission-prompt compatibility fields. Claude
    /// reads the MCP tool text as a JSON permission verdict; keeping these at
    /// the top level lets the public `approval` facade serve as the gate tool
    /// without re-exposing the hidden `approval_gate` implementation tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
    #[serde(
        default,
        rename = "updatedInput",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<ApprovalRequestResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<ApprovalListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide: Option<ApprovalDecideResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<ApprovalVerdictReadback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_operator: Option<ApprovalVerdictReadback>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationOperation {
    ConfigGet,
    ConfigSet,
    List,
    Ack,
}

impl EscalationOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigGet => "config_get",
            Self::ConfigSet => "config_set",
            Self::List => "list",
            Self::Ack => "ack",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationParams {
    pub operation: EscalationOperation,
    #[serde(default)]
    pub config_get: Option<EscalationConfigGetParams>,
    #[serde(default)]
    pub config_set: Option<EscalationConfigSetParams>,
    #[serde(default)]
    pub list: Option<EscalationListParams>,
    #[serde(default)]
    pub ack: Option<EscalationAckParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EscalationResponse {
    pub operation: EscalationOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_get: Option<EscalationConfigResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_set: Option<EscalationConfigResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<EscalationListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<EscalationAckResponse>,
}

#[tool_router(router = approval_facade_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Facade for durable approval queue operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Claude permission-prompt compatibility may omit operation only when sending the direct gate payload shape. Mutating operations delegate to the real approval_request/approval_decide/approval_gate/agent_ask_operator paths and return physical CF_KV readback metadata."
    )]
    pub async fn approval(
        &self,
        params: Parameters<ApprovalParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ApprovalResponse>, ErrorData> {
        validate_approval_facade_params(&params.0)?;
        let operation = approval_operation(&params.0)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = APPROVAL_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=approval"
        );
        match operation {
            ApprovalOperation::Request => {
                let spec = params
                    .0
                    .request
                    .ok_or_else(|| missing_approval_spec("request"))?;
                let source_id = spec
                    .dedupe_key
                    .clone()
                    .unwrap_or_else(|| spec.kind.as_str().to_owned());
                let response = self
                    .approval_request(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        approval_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix the approval request fields and inspect item_row/audit_row in CF_KV",
                        )
                    })?
                    .0;
                Ok(Json(approval_response(
                    operation,
                    format!(
                        "CF_KV approval item={} audit={} deduped={} status={}",
                        response.item_row.key,
                        response.audit_row.key,
                        response.deduped,
                        response.item.status.as_str()
                    ),
                    |out| out.request = Some(response),
                )))
            }
            ApprovalOperation::List => {
                let spec = params.0.list.ok_or_else(|| missing_approval_spec("list"))?;
                let response = self
                    .approval_list(Parameters(spec))
                    .await
                    .map_err(|error| {
                        approval_delegate_error(
                            operation,
                            "approval_queue",
                            error,
                            "inspect the approval CF_KV prefix scan filters, cursor, and materialized timeout rows",
                        )
                    })?
                    .0;
                Ok(Json(approval_response(
                    operation,
                    format!(
                        "CF_KV approval prefix scan items={} materialized_timeouts={} scanned_rows={}",
                        response.items.len(),
                        response.materialized_timeouts.len(),
                        response.scanned_rows
                    ),
                    |out| out.list = Some(response),
                )))
            }
            ApprovalOperation::Decide => {
                let spec = params
                    .0
                    .decide
                    .ok_or_else(|| missing_approval_spec("decide"))?;
                let source_id = spec.approval_id.clone();
                let response = self
                    .approval_decide(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        approval_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing non-terminal approval_id, explicit decision, and required note/response fields",
                        )
                    })?
                    .0;
                Ok(Json(approval_response(
                    operation,
                    format!(
                        "CF_KV approval decision item={} audit={} before={} after={}",
                        response.item_row.key,
                        response.audit_row.key,
                        response.before_status.as_str(),
                        response.after_status.as_str()
                    ),
                    |out| out.decide = Some(response),
                )))
            }
            ApprovalOperation::Gate => {
                let spec =
                    approval_gate_spec(&params.0).ok_or_else(|| missing_approval_spec("gate"))?;
                let source_id = spec
                    .tool_use_id
                    .clone()
                    .or_else(|| spec.tool_name.clone())
                    .unwrap_or_else(|| "approval_gate".to_owned());
                let result = self
                    .approval_gate(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        approval_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the approval queue row for risky calls or the gate verdict for auto-allowed calls",
                        )
                    })?;
                let readback = verdict_readback(&result);
                Ok(Json(approval_response(
                    operation,
                    format!(
                        "approval_gate verdict content_bytes={} value_type={}",
                        readback.content_text.len(),
                        value_kind(&readback.value)
                    ),
                    |out| {
                        apply_permission_prompt_verdict(out, &readback);
                        out.gate = Some(readback);
                    },
                )))
            }
            ApprovalOperation::AskOperator => {
                let spec = params
                    .0
                    .ask_operator
                    .ok_or_else(|| missing_approval_spec("ask_operator"))?;
                let source_id = spec
                    .spawn_id
                    .clone()
                    .unwrap_or_else(|| "agent_question".to_owned());
                let result = self
                    .agent_ask_operator(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        approval_delegate_error(
                            operation,
                            source_id,
                            error,
                            "inspect the agent_question approval row and operator response/timeout decision",
                        )
                    })?;
                let readback = verdict_readback(&result);
                Ok(Json(approval_response(
                    operation,
                    format!(
                        "agent_question verdict content_bytes={} value_type={}",
                        readback.content_text.len(),
                        value_kind(&readback.value)
                    ),
                    |out| out.ask_operator = Some(readback),
                )))
            }
        }
    }

    #[tool(
        description = "Facade for AFK escalation policy, list, and acknowledgement operations in the <=40 public MCP surface. operation is a strict enum; exactly one matching operation spec is accepted. Mutating operations delegate to the real escalation implementation and return physical CF_KV readback metadata."
    )]
    pub async fn escalation(
        &self,
        params: Parameters<EscalationParams>,
    ) -> Result<Json<EscalationResponse>, ErrorData> {
        validate_escalation_facade_params(&params.0)?;
        let operation = params.0.operation;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = ESCALATION_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=escalation"
        );
        match operation {
            EscalationOperation::ConfigGet => {
                let spec = params
                    .0
                    .config_get
                    .ok_or_else(|| missing_escalation_spec("config_get"))?;
                let response = self
                    .escalation_config_get(Parameters(spec))
                    .await
                    .map_err(|error| {
                        escalation_delegate_error(
                            operation,
                            "escalation/v1/config",
                            error,
                            "inspect the escalation policy row or absent-row default",
                        )
                    })?
                    .0;
                Ok(Json(escalation_response(
                    operation,
                    escalation_config_readback(&response),
                    |out| out.config_get = Some(response),
                )))
            }
            EscalationOperation::ConfigSet => {
                let spec = params
                    .0
                    .config_set
                    .ok_or_else(|| missing_escalation_spec("config_set"))?;
                let response = self
                    .escalation_config_set(Parameters(spec))
                    .await
                    .map_err(|error| {
                        escalation_delegate_error(
                            operation,
                            "escalation/v1/config",
                            error,
                            "fix escalation policy fields and inspect the persisted config row",
                        )
                    })?
                    .0;
                Ok(Json(escalation_response(
                    operation,
                    escalation_config_readback(&response),
                    |out| out.config_set = Some(response),
                )))
            }
            EscalationOperation::List => {
                let spec = params
                    .0
                    .list
                    .ok_or_else(|| missing_escalation_spec("list"))?;
                let response = self
                    .escalation_list(Parameters(spec))
                    .await
                    .map_err(|error| {
                        escalation_delegate_error(
                            operation,
                            "escalation_items",
                            error,
                            "inspect escalation status/anchor filters and CF_KV item rows",
                        )
                    })?
                    .0;
                Ok(Json(escalation_response(
                    operation,
                    format!(
                        "CF_KV escalation item prefix scan returned={} total_open={}",
                        response.returned, response.total_open
                    ),
                    |out| out.list = Some(response),
                )))
            }
            EscalationOperation::Ack => {
                let spec = params.0.ack.ok_or_else(|| missing_escalation_spec("ack"))?;
                let source_id = spec.escalation_id.clone();
                let response = self
                    .escalation_ack(Parameters(spec))
                    .await
                    .map_err(|error| {
                        escalation_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing escalation_id and inspect the item/audit rows after ack",
                        )
                    })?
                    .0;
                Ok(Json(escalation_response(
                    operation,
                    format!(
                        "CF_KV escalation ack id={} newly_acked={} status={:?}",
                        response.escalation.escalation_id,
                        response.newly_acked,
                        response.escalation.status
                    ),
                    |out| out.ack = Some(response),
                )))
            }
        }
    }
}

fn validate_approval_facade_params(params: &ApprovalParams) -> Result<(), ErrorData> {
    let operation = approval_operation(params)?;
    if params.operation.is_some() && approval_direct_gate_present(params) {
        return Err(facade_params_error(
            APPROVAL_TOOL,
            operation.as_str(),
            "approval direct Claude permission-prompt fields require operation to be omitted",
            "omit operation and pass only tool_name/input/tool_use_id/spawn_id, or use operation=gate with gate={...}",
        ));
    }
    if params.gate.is_some() && approval_direct_gate_present(params) {
        return Err(facade_params_error(
            APPROVAL_TOOL,
            operation.as_str(),
            "approval received both nested gate spec and direct Claude permission-prompt fields",
            "pass exactly one gate shape: either gate={...} with operation=gate, or direct fields with operation omitted",
        ));
    }
    validate_exact_operation_spec(
        APPROVAL_TOOL,
        operation.as_str(),
        &[
            ("request", params.request.is_some()),
            ("list", params.list.is_some()),
            ("decide", params.decide.is_some()),
            (
                "gate",
                params.gate.is_some() || approval_direct_gate_present(params),
            ),
            ("ask_operator", params.ask_operator.is_some()),
        ],
    )
}

fn approval_operation(params: &ApprovalParams) -> Result<ApprovalOperation, ErrorData> {
    if let Some(operation) = params.operation {
        return Ok(operation);
    }
    if approval_direct_gate_present(params) {
        return Ok(ApprovalOperation::Gate);
    }
    Err(facade_params_error(
        APPROVAL_TOOL,
        "unknown",
        "approval requires operation unless called as a Claude permission-prompt gate payload",
        "pass operation=<request|list|decide|gate|ask_operator> with the matching spec, or pass direct Claude permission-prompt fields tool_name/input/tool_use_id/spawn_id",
    ))
}

fn approval_direct_gate_present(params: &ApprovalParams) -> bool {
    params.tool_name.is_some()
        || params.input.is_some()
        || params.tool_use_id.is_some()
        || params.spawn_id.is_some()
}

fn approval_gate_spec(params: &ApprovalParams) -> Option<ApprovalGateParams> {
    params.gate.clone().or_else(|| {
        approval_direct_gate_present(params).then(|| ApprovalGateParams {
            tool_name: params.tool_name.clone(),
            input: params.input.clone(),
            tool_use_id: params.tool_use_id.clone(),
            spawn_id: params.spawn_id.clone(),
        })
    })
}

fn validate_escalation_facade_params(params: &EscalationParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        ESCALATION_TOOL,
        params.operation.as_str(),
        &[
            ("config_get", params.config_get.is_some()),
            ("config_set", params.config_set.is_some()),
            ("list", params.list.is_some()),
            ("ack", params.ack.is_some()),
        ],
    )
}

fn validate_exact_operation_spec(
    tool: &'static str,
    operation: &'static str,
    specs: &[(&'static str, bool)],
) -> Result<(), ErrorData> {
    let present = specs
        .iter()
        .filter_map(|(name, is_present)| is_present.then_some(*name))
        .collect::<Vec<_>>();
    if !present.contains(&operation) {
        return Err(facade_params_error(
            tool,
            operation,
            format!("{tool} operation={operation} requires a matching {operation} spec"),
            format!("pass {operation}={{...}} and no other operation spec"),
        ));
    }
    if present.len() != 1 {
        return Err(facade_params_error(
            tool,
            operation,
            format!("{tool} operation={operation} received invalid operation specs {present:?}"),
            format!("pass exactly one operation-specific spec matching {operation}"),
        ));
    }
    Ok(())
}

fn missing_approval_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        APPROVAL_TOOL,
        operation,
        format!("approval operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn missing_escalation_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        ESCALATION_TOOL,
        operation,
        format!("escalation operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn facade_params_error(
    tool: &'static str,
    operation: &'static str,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": tool,
            "operation": operation,
            "source_of_truth": "typed facade params before delegated operation",
            "remediation": remediation.into(),
        })),
    )
}

fn approval_delegate_error(
    operation: ApprovalOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        APPROVAL_TOOL,
        operation.as_str(),
        APPROVAL_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn escalation_delegate_error(
    operation: EscalationOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        ESCALATION_TOOL,
        operation.as_str(),
        ESCALATION_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn delegate_error(
    tool: &'static str,
    operation: &'static str,
    source_of_truth: &'static str,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "tool": tool,
            "operation": operation,
            "source_of_truth": source_of_truth,
            "source_id": source_id.into(),
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

fn approval_response(
    operation: ApprovalOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut ApprovalResponse),
) -> ApprovalResponse {
    let mut response = ApprovalResponse {
        operation,
        source_of_truth: format!(
            "{APPROVAL_SOURCE_OF_TRUTH} + delegated approval operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        behavior: None,
        updated_input: None,
        message: None,
        request: None,
        list: None,
        decide: None,
        gate: None,
        ask_operator: None,
    };
    populate(&mut response);
    response
}

fn apply_permission_prompt_verdict(
    response: &mut ApprovalResponse,
    readback: &ApprovalVerdictReadback,
) {
    let Some(verdict) = readback.value.as_object() else {
        return;
    };
    response.behavior = verdict
        .get("behavior")
        .and_then(Value::as_str)
        .map(str::to_owned);
    response.updated_input = verdict.get("updatedInput").cloned();
    response.message = verdict
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned);
}

fn escalation_response(
    operation: EscalationOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut EscalationResponse),
) -> EscalationResponse {
    let mut response = EscalationResponse {
        operation,
        source_of_truth: format!(
            "{ESCALATION_SOURCE_OF_TRUTH} + delegated escalation operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        config_get: None,
        config_set: None,
        list: None,
        ack: None,
    };
    populate(&mut response);
    response
}

fn verdict_readback(result: &CallToolResult) -> ApprovalVerdictReadback {
    let content_text = result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    let value = result.structured_content.clone().unwrap_or_else(|| {
        serde_json::from_str(&content_text).unwrap_or(Value::String(content_text.clone()))
    });
    ApprovalVerdictReadback {
        value,
        content_text,
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn escalation_config_readback(response: &EscalationConfigResponse) -> String {
    format!(
        "CF_KV escalation/v1/config updated_at={} webhooks={} tier0_only={}",
        response.updated_at_unix_ms,
        response.webhooks.len(),
        response.tier0_only
    )
}
