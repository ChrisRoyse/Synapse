use super::super::{ErrorData, Json, Parameters, SynapseService};
use super::common::{delegate_error, facade_params_error, validate_exact_operation_spec};

use crate::m3::{
    intent::{IntentCurrentParams, IntentCurrentResponse},
    intent_events::{IntentDetectOutcome, IntentDetectTickParams},
    suggestions::{
        SuggestionAcceptParams, SuggestionAcceptResponse, SuggestionDeclineParams,
        SuggestionDeclineResponse, SuggestionListParams, SuggestionListResponse,
        SuggestionTickParams, SuggestionTickResponse,
    },
};
use crate::server::mcp_usage::{
    McpUsageGuideParams, McpUsageGuidePolicyParams, McpUsageGuidePromoteParams,
    McpUsageGuideResponse, McpUsageGuideRollbackParams, McpUsagePolicyResponse,
    McpUsagePromotionResponse,
};
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};

pub(super) const ASSIST_TOOL: &str = "assist";
const ASSIST_SOURCE_OF_TRUTH: &str = "CF_KV suggestion/v1 + intent tracker/events + CF_ROUTINES/CF_ROUTINE_STATE + CF_KV mcp-usage/v1 + Calyx syn-mcp-usage-v1 + Anchors";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistOperation {
    Intent,
    Detect,
    SuggestionTick,
    SuggestionList,
    SuggestionAccept,
    SuggestionDecline,
    Guide,
    GuidePolicy,
    GuidePromote,
    GuideRollback,
}

impl AssistOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Detect => "detect",
            Self::SuggestionTick => "suggestion_tick",
            Self::SuggestionList => "suggestion_list",
            Self::SuggestionAccept => "suggestion_accept",
            Self::SuggestionDecline => "suggestion_decline",
            Self::Guide => "guide",
            Self::GuidePolicy => "guide_policy",
            Self::GuidePromote => "guide_promote",
            Self::GuideRollback => "guide_rollback",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistParams {
    pub operation: AssistOperation,
    #[serde(default)]
    pub intent: Option<IntentCurrentParams>,
    #[serde(default)]
    pub detect: Option<IntentDetectTickParams>,
    #[serde(default)]
    pub suggestion_tick: Option<SuggestionTickParams>,
    #[serde(default)]
    pub suggestion_list: Option<SuggestionListParams>,
    #[serde(default)]
    pub suggestion_accept: Option<SuggestionAcceptParams>,
    #[serde(default)]
    pub suggestion_decline: Option<SuggestionDeclineParams>,
    #[serde(default)]
    pub guide: Option<McpUsageGuideParams>,
    #[serde(default)]
    pub guide_policy: Option<McpUsageGuidePolicyParams>,
    #[serde(default)]
    pub guide_promote: Option<McpUsageGuidePromoteParams>,
    #[serde(default)]
    pub guide_rollback: Option<McpUsageGuideRollbackParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistResponse {
    pub operation: AssistOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentCurrentResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<IntentDetectOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_tick: Option<SuggestionTickResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_list: Option<SuggestionListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_accept: Option<SuggestionAcceptResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_decline: Option<SuggestionDeclineResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<McpUsageGuideResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_policy: Option<McpUsagePolicyResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_promote: Option<McpUsagePromotionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_rollback: Option<McpUsagePromotionResponse>,
}

pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<AssistParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<AssistResponse>, ErrorData> {
    validate_assist_facade_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = ASSIST_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=assist"
    );
    match operation {
        AssistOperation::Intent => {
            let spec = params
                .0
                .intent
                .ok_or_else(|| missing_assist_spec("intent"))?;
            let source_id = assist_now_source_id(spec.now_ts_ns);
            let response = service
                .intent_current(Parameters(spec))
                .await
                .map_err(|error| {
                    assist_delegate_error(
                        operation,
                        source_id,
                        error,
                        "fix intent filters and inspect CF_EPISODES/CF_ROUTINES/CF_ROUTINE_STATE",
                    )
                })?
                .0;
            Ok(Json(assist_response(
                operation,
                format!(
                    "intent snapshot now_ts_ns={} candidates={}",
                    response.now.ts_ns,
                    response.candidates.len()
                ),
                |out| out.intent = Some(response),
            )))
        }
        AssistOperation::Detect => {
            let spec = params
                .0
                .detect
                .ok_or_else(|| missing_assist_spec("detect"))?;
            let source_id = assist_now_source_id(spec.now_ts_ns);
            let response = service
                    .intent_detect_tick(Parameters(spec))
                    .await
                    .map_err(|error| {
                        assist_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix detection filters and inspect intent tracker/event delivery plus routine stores",
                        )
                    })?
                    .0;
            Ok(Json(assist_response(
                operation,
                format!(
                    "intent detect candidates={} tracked={} events_published={} dropped={}",
                    response.candidates,
                    response.tracked,
                    response.events_published,
                    response.events_dropped
                ),
                |out| out.detect = Some(response),
            )))
        }
        AssistOperation::SuggestionTick => {
            let spec = params
                .0
                .suggestion_tick
                .ok_or_else(|| missing_assist_spec("suggestion_tick"))?;
            let source_id = assist_now_source_id(spec.now_ts_ns);
            let response = service
                .suggestion_tick(Parameters(spec))
                .await
                .map_err(|error| {
                    assist_delegate_error(
                        operation,
                        source_id,
                        error,
                        "fix suggestion tick bounds/config and inspect CF_KV suggestion/v1 rows",
                    )
                })?
                .0;
            Ok(Json(assist_response(
                operation,
                format!(
                    "CF_KV suggestion tick candidates={} created={} expired={} abandoned={} dry_run={}",
                    response.candidates_evaluated,
                    response.created.len(),
                    response.expired.len(),
                    response.abandoned.len(),
                    response.dry_run
                ),
                |out| out.suggestion_tick = Some(response),
            )))
        }
        AssistOperation::SuggestionList => {
            let spec = params
                .0
                .suggestion_list
                .ok_or_else(|| missing_assist_spec("suggestion_list"))?;
            let source_id = spec
                .routine_id
                .clone()
                .unwrap_or_else(|| "routine_id:all".to_owned());
            let response = service
                .suggestion_list(Parameters(spec))
                .await
                .map_err(|error| {
                    assist_delegate_error(
                        operation,
                        source_id,
                        error,
                        "narrow suggestion list filters and inspect CF_KV suggestion/v1 rows",
                    )
                })?
                .0;
            Ok(Json(assist_response(
                operation,
                format!(
                    "CF_KV suggestion list total_rows={} returned={}",
                    response.total_rows, response.returned
                ),
                |out| out.suggestion_list = Some(response),
            )))
        }
        AssistOperation::SuggestionAccept => {
            let spec = params
                .0
                .suggestion_accept
                .ok_or_else(|| missing_assist_spec("suggestion_accept"))?;
            let source_id = spec.suggestion_id.clone();
            let response = service
                    .suggestion_accept(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        assist_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing live suggestion_id and inspect suggestion/plan_execution rows",
                        )
                    })?
                    .0;
            Ok(Json(assist_response(
                operation,
                format!(
                    "CF_KV suggestion accept suggestion_id={} execution_id={} dry_run={}",
                    response.suggestion.suggestion_id,
                    response.execution.execution_id,
                    response.execution.dry_run
                ),
                |out| out.suggestion_accept = Some(response),
            )))
        }
        AssistOperation::SuggestionDecline => {
            let spec = params
                .0
                .suggestion_decline
                .ok_or_else(|| missing_assist_spec("suggestion_decline"))?;
            let source_id = spec.suggestion_id.clone();
            let response = service
                    .suggestion_decline(Parameters(spec))
                    .await
                    .map_err(|error| {
                        assist_delegate_error(
                            operation,
                            source_id,
                            error,
                            "pass the exact durable suggestion/v1 id (not a routine_id) and inspect the CF_KV suggestion row plus its Calyx Anchors rows",
                        )
                    })?
                    .0;
            Ok(Json(assist_response(
                operation,
                format!(
                    "CF_KV suggestion decline suggestion_id={} replay={} declined_ts_ns={} anchor_cx_id={} anchor_exact_matches={}",
                    response.suggestion.suggestion_id,
                    response.replay,
                    response.declined_ts_ns,
                    response.anchor.cx_id,
                    response.anchor.readback_exact_match_count
                ),
                |out| out.suggestion_decline = Some(response),
            )))
        }
        AssistOperation::Guide => {
            let spec = params.0.guide.ok_or_else(|| missing_assist_spec("guide"))?;
            let response = super::super::mcp_usage::guide(service, spec)?;
            Ok(Json(assist_response(
                operation,
                format!(
                    "MCP usage guide route_id={} evidence_rows={}",
                    response.route_id, response.evidence_rows
                ),
                |out| out.guide = Some(response),
            )))
        }
        AssistOperation::GuidePolicy => {
            let spec = params
                .0
                .guide_policy
                .ok_or_else(|| missing_assist_spec("guide_policy"))?;
            let response = super::super::mcp_usage::write_policy(service, spec)?;
            Ok(Json(assist_response(
                operation,
                format!(
                    "MCP usage policy row={} enabled={}",
                    response.policy.row_key, response.policy.enabled
                ),
                |out| out.guide_policy = Some(response),
            )))
        }
        AssistOperation::GuidePromote => {
            let spec = params
                .0
                .guide_promote
                .ok_or_else(|| missing_assist_spec("guide_promote"))?;
            let response = super::super::mcp_usage::promote(service, spec)?;
            Ok(Json(assist_response(
                operation,
                format!(
                    "MCP usage promotion id={} state={}",
                    response.promotion_id, response.state
                ),
                |out| out.guide_promote = Some(response),
            )))
        }
        AssistOperation::GuideRollback => {
            let spec = params
                .0
                .guide_rollback
                .ok_or_else(|| missing_assist_spec("guide_rollback"))?;
            let response = super::super::mcp_usage::rollback(service, spec)?;
            Ok(Json(assist_response(
                operation,
                format!(
                    "MCP usage promotion rollback id={} state={}",
                    response.promotion_id, response.state
                ),
                |out| out.guide_rollback = Some(response),
            )))
        }
    }
}

pub(super) fn validate_assist_facade_params(params: &AssistParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        ASSIST_TOOL,
        params.operation.as_str(),
        &[
            ("intent", params.intent.is_some()),
            ("detect", params.detect.is_some()),
            ("suggestion_tick", params.suggestion_tick.is_some()),
            ("suggestion_list", params.suggestion_list.is_some()),
            ("suggestion_accept", params.suggestion_accept.is_some()),
            ("suggestion_decline", params.suggestion_decline.is_some()),
            ("guide", params.guide.is_some()),
            ("guide_policy", params.guide_policy.is_some()),
            ("guide_promote", params.guide_promote.is_some()),
            ("guide_rollback", params.guide_rollback.is_some()),
        ],
    )
}

fn missing_assist_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        ASSIST_TOOL,
        operation,
        format!("assist operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn assist_delegate_error(
    operation: AssistOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        ASSIST_TOOL,
        operation.as_str(),
        ASSIST_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn assist_response(
    operation: AssistOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut AssistResponse),
) -> AssistResponse {
    let mut response = AssistResponse {
        operation,
        source_of_truth: format!(
            "{ASSIST_SOURCE_OF_TRUTH} + delegated assist operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        intent: None,
        detect: None,
        suggestion_tick: None,
        suggestion_list: None,
        suggestion_accept: None,
        suggestion_decline: None,
        guide: None,
        guide_policy: None,
        guide_promote: None,
        guide_rollback: None,
    };
    populate(&mut response);
    response
}

fn assist_now_source_id(now_ts_ns: Option<u64>) -> String {
    format!(
        "now_ts_ns:{}",
        now_ts_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "now".to_owned())
    )
}
