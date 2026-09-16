use super::super::{ErrorData, Json, Parameters, SynapseService};
use super::common::{delegate_error, facade_params_error, validate_exact_operation_spec};

use crate::m3::{
    armed_routines::{ArmedRoutineTickParams, ArmedRoutineTickResponse},
    profile_authoring::{RoutineAutomateParams, RoutineAutomateResponse},
    routines::{
        RoutineFeedbackParams, RoutineFeedbackResponse, RoutineInspectParams,
        RoutineInspectResponse, RoutineLabelExportParams, RoutineLabelExportResponse,
        RoutineListParams, RoutineListResponse, RoutineMineParams, RoutineMineResponse,
        RoutineUpdateParams, RoutineUpdateResponse,
    },
};
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};

pub(super) const ROUTINE_TOOL: &str = "routine";
const ROUTINE_SOURCE_OF_TRUTH: &str =
    "CF_ROUTINES + CF_ROUTINE_STATE + CF_KV routine automation/armed rows";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineOperation {
    Mine,
    List,
    Inspect,
    Update,
    Feedback,
    Label,
    Automate,
    ArmedTick,
}

impl RoutineOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Mine => "mine",
            Self::List => "list",
            Self::Inspect => "inspect",
            Self::Update => "update",
            Self::Feedback => "feedback",
            Self::Label => "label",
            Self::Automate => "automate",
            Self::ArmedTick => "armed_tick",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineParams {
    pub operation: RoutineOperation,
    #[serde(default)]
    pub mine: Option<RoutineMineParams>,
    #[serde(default)]
    pub list: Option<RoutineListParams>,
    #[serde(default)]
    pub inspect: Option<RoutineInspectParams>,
    #[serde(default)]
    pub update: Option<RoutineUpdateParams>,
    #[serde(default)]
    pub feedback: Option<RoutineFeedbackParams>,
    #[serde(default)]
    pub label: Option<RoutineLabelExportParams>,
    #[serde(default)]
    pub automate: Option<RoutineAutomateParams>,
    #[serde(default)]
    pub armed_tick: Option<ArmedRoutineTickParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutineResponse {
    pub operation: RoutineOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mine: Option<RoutineMineResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<RoutineListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect: Option<RoutineInspectResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<RoutineUpdateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<RoutineFeedbackResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<RoutineLabelExportResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automate: Option<RoutineAutomateResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_tick: Option<ArmedRoutineTickResponse>,
}

pub(super) async fn handle(
    service: &SynapseService,
    params: Parameters<RoutineParams>,
    request_context: RequestContext<RoleServer>,
) -> Result<Json<RoutineResponse>, ErrorData> {
    validate_routine_facade_params(&params.0)?;
    let operation = params.0.operation;
    tracing::info!(
        code = "MCP_TOOL_INVOCATION",
        kind = ROUTINE_TOOL,
        operation = operation.as_str(),
        "tool.invocation kind=routine"
    );
    match operation {
        RoutineOperation::Mine => {
            let spec = params.0.mine.ok_or_else(|| missing_routine_spec("mine"))?;
            let source_id = routine_range_source_id(spec.start_ts_ns, spec.end_ts_ns);
            let response = service
                    .routine_mine(Parameters(spec))
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix routine mining bounds and inspect CF_EPISODES, CF_ROUTINES, and CF_ROUTINE_STATE",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_ROUTINES mine routines_written={} routines_deleted={} state_created={} state_updated={} dry_run={}",
                    response.routines_written,
                    response.routines_deleted,
                    response.state_rows_created,
                    response.state_rows_updated,
                    response.dry_run
                ),
                |out| out.mine = Some(response),
            )))
        }
        RoutineOperation::List => {
            let spec = params.0.list.ok_or_else(|| missing_routine_spec("list"))?;
            let source_id = routine_list_source_id(&spec);
            let response = service
                    .routine_list(Parameters(spec))
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "narrow routine list filters and inspect CF_ROUTINES plus CF_ROUTINE_STATE rows",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_ROUTINES list total_mined={} total_state_rows={} matched={} returned={} truncated={}",
                    response.total_mined,
                    response.total_state_rows,
                    response.matched,
                    response.returned,
                    response.truncated
                ),
                |out| out.list = Some(response),
            )))
        }
        RoutineOperation::Inspect => {
            let spec = params
                .0
                .inspect
                .ok_or_else(|| missing_routine_spec("inspect"))?;
            let source_id = spec.routine_id.clone();
            let response = service
                    .routine_inspect(Parameters(spec))
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing routine_id and inspect CF_ROUTINES plus CF_ROUTINE_STATE",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_ROUTINES/CF_ROUTINE_STATE routine_id={} mined={} state_row_exists={} armed_present={}",
                    response.routine_id,
                    response.mined,
                    response.state_row_exists,
                    response.armed.is_some()
                ),
                |out| out.inspect = Some(response),
            )))
        }
        RoutineOperation::Update => {
            let spec = params
                .0
                .update
                .ok_or_else(|| missing_routine_spec("update"))?;
            let source_id = spec.routine_id.clone();
            let response = service
                    .routine_update(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix routine lifecycle/arming params and inspect CF_ROUTINE_STATE plus armed_routine/v1 rows",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_ROUTINE_STATE routine_id={} action={:?} lifecycle_before={:?} lifecycle_after={:?} state_row_created={}",
                    response.routine_id,
                    response.action,
                    response.lifecycle_before,
                    response.lifecycle_after,
                    response.state_row_created
                ),
                |out| out.update = Some(response),
            )))
        }
        RoutineOperation::Feedback => {
            let spec = params
                .0
                .feedback
                .ok_or_else(|| missing_routine_spec("feedback"))?;
            let source_id = spec.routine_id.clone();
            let response = service
                    .routine_feedback(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing routine_id/outcome and inspect the CF_ROUTINE_STATE feedback history",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_ROUTINE_STATE feedback routine_id={} outcome={:?} suppressed={}",
                    response.routine_id, response.outcome, response.suppressed
                ),
                |out| out.feedback = Some(response),
            )))
        }
        RoutineOperation::Label => {
            let spec = params
                .0
                .label
                .ok_or_else(|| missing_routine_spec("label"))?;
            let source_id = spec.routine_id.clone();
            let response = service
                    .routine_label_export(Parameters(spec))
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide an existing routine_id and inspect CF_ROUTINES/CF_ROUTINE_STATE label evidence",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_ROUTINES label_export routine_id={} samples={} prompt_bytes={}",
                    response.routine_id,
                    response.samples.len(),
                    response.prompt.len()
                ),
                |out| out.label = Some(response),
            )))
        }
        RoutineOperation::Automate => {
            let spec = params
                .0
                .automate
                .ok_or_else(|| missing_routine_spec("automate"))?;
            let source_id = spec.routine_id.clone();
            let response = service
                    .routine_automate(Parameters(spec))
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "provide a mined routine_id and inspect profile_authoring/v1 plus routine_automation/v1 rows",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_KV routine automation routine_id={} candidate_id={} row_key={} wrote_row={}",
                    response.automation.routine_id,
                    response.automation.candidate_id,
                    response.row_key,
                    response.wrote_row
                ),
                |out| out.automate = Some(response),
            )))
        }
        RoutineOperation::ArmedTick => {
            let spec = params
                .0
                .armed_tick
                .ok_or_else(|| missing_routine_spec("armed_tick"))?;
            let source_id = spec
                .routine_id
                .clone()
                .unwrap_or_else(|| "routine_id:all".to_owned());
            let response = service
                    .armed_routine_tick(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        routine_delegate_error(
                            operation,
                            source_id,
                            error,
                            "fix armed routine filters and inspect armed_routine/v1 plus armed_routine_run/v1 rows",
                        )
                    })?
                    .0;
            Ok(Json(routine_response(
                operation,
                format!(
                    "CF_KV armed routine tick evaluated={} due={} executed={} dry_run={} source_of_truth={}",
                    response.evaluated,
                    response.due,
                    response.executed,
                    response.dry_run,
                    response.source_of_truth
                ),
                |out| out.armed_tick = Some(response),
            )))
        }
    }
}

pub(super) fn validate_routine_facade_params(params: &RoutineParams) -> Result<(), ErrorData> {
    validate_exact_operation_spec(
        ROUTINE_TOOL,
        params.operation.as_str(),
        &[
            ("mine", params.mine.is_some()),
            ("list", params.list.is_some()),
            ("inspect", params.inspect.is_some()),
            ("update", params.update.is_some()),
            ("feedback", params.feedback.is_some()),
            ("label", params.label.is_some()),
            ("automate", params.automate.is_some()),
            ("armed_tick", params.armed_tick.is_some()),
        ],
    )
}

fn missing_routine_spec(operation: &'static str) -> ErrorData {
    facade_params_error(
        ROUTINE_TOOL,
        operation,
        format!("routine operation={operation} requires a {operation} spec"),
        format!("pass {operation}={{...}} and no other operation spec"),
    )
}

fn routine_delegate_error(
    operation: RoutineOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    delegate_error(
        ROUTINE_TOOL,
        operation.as_str(),
        ROUTINE_SOURCE_OF_TRUTH,
        source_id,
        error,
        remediation,
    )
}

fn routine_response(
    operation: RoutineOperation,
    readback_source_of_truth: String,
    populate: impl FnOnce(&mut RoutineResponse),
) -> RoutineResponse {
    let mut response = RoutineResponse {
        operation,
        source_of_truth: format!(
            "{ROUTINE_SOURCE_OF_TRUTH} + delegated routine operation={}",
            operation.as_str()
        ),
        readback_source_of_truth,
        mine: None,
        list: None,
        inspect: None,
        update: None,
        feedback: None,
        label: None,
        automate: None,
        armed_tick: None,
    };
    populate(&mut response);
    response
}

fn routine_range_source_id(start_ts_ns: Option<u64>, end_ts_ns: Option<u64>) -> String {
    format!(
        "range:{}..{}",
        start_ts_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unbounded".to_owned()),
        end_ts_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unbounded".to_owned())
    )
}

fn routine_list_source_id(params: &RoutineListParams) -> String {
    format!(
        "routine_list:lifecycle={} app={} limit={}",
        params
            .lifecycle
            .as_ref()
            .map_or("default".to_owned(), |items| items.len().to_string()),
        params.app.as_deref().unwrap_or("<any>"),
        params
            .limit
            .map_or_else(|| "default".to_owned(), |value| value.to_string())
    )
}
