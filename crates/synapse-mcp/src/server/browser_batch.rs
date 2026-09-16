//! `browser_batch` MCP tool (#1337).
//!
//! Anthropic's computer-use surface ships an experimental `browser_batch` that
//! runs an ordered list of browser sub-actions in one round-trip to cut
//! per-action latency/token overhead on long flows (navigate → wait → fill →
//! click → wait). Synapse had `act_combo` for desktop composite input but no
//! browser equivalent.
//!
//! This is PURE ORCHESTRATION: every step routes to the exact same code path as
//! its standalone tool (`cdp_navigate_tab`, `browser_wait_for_*`,
//! `browser_set_value`, `browser_fill_form`, `browser_scroll_into_view`,
//! `browser_file_upload`, `browser_evaluate`, `browser_screenshot`, and
//! `target_act verb=click`), so there are no new action semantics,
//! target-resolution rules, or guards — each sub-action inherits its tool's
//! session-target requirement and audit.
//!
//! Failure model (Anthropic guidance: interdependent steps compound errors):
//! stop-on-first-error by default. Each step yields a `{index, action, status,
//! ok, result|error}` row; on failure the remaining steps are reported
//! `skipped`, and the failing step carries the full structured error.
//!
//! Lives in its own tool router (merged in `server.rs`) so the orchestration
//! surface stays decoupled from the individual browser tool modules.

use rmcp::{RoleServer, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::background_router::TargetActParams;
use super::browser_field::{BrowserFillFormParams, BrowserSetValueParams};
use super::browser_files::BrowserFileUploadParams;
use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};
use crate::m1::{
    BrowserEvaluateParams, BrowserScreenshotParams, BrowserScrollIntoViewParams,
    BrowserWaitForLoadStateParams, BrowserWaitForSelectorParams, BrowserWaitForUrlParams,
    CdpNavigateTabParams, mcp_error,
};

const STATUS_OK: &str = "ok";
const STATUS_ERROR: &str = "error";
const STATUS_SKIPPED: &str = "skipped";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserBatchStep {
    /// Sub-action to run. One of: `navigate`, `wait_for_selector`,
    /// `wait_for_url`, `wait_for_load_state`, `click`, `set_value`, `fill_form`,
    /// `file_upload`, `scroll_into_view`, `evaluate`, `screenshot`.
    pub action: String,
    /// Parameters object for the sub-action's standalone tool, shaped exactly
    /// like that tool's params (e.g. `navigate` → `cdp_navigate_tab` params,
    /// `set_value` → `browser_set_value` params, `click` → `target_act` params
    /// minus `verb`). Defaults to an empty object.
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserBatchParams {
    /// Ordered list of browser sub-actions executed in sequence against the
    /// calling session's bound browser target. Must be non-empty.
    pub steps: Vec<BrowserBatchStep>,
    /// Stop the batch at the first failing step (default true). When false, every
    /// step runs and failures are reported per-step without skipping the rest.
    #[serde(default)]
    pub stop_on_error: Option<bool>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserBatchStepResult {
    pub index: usize,
    pub action: String,
    /// `ok` (ran and succeeded), `error` (ran and failed), or `skipped`
    /// (a prior step failed with stop_on_error).
    pub status: String,
    pub ok: bool,
    /// The sub-action tool's response, present when `status == "ok"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The full structured error, present when `status == "error"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserBatchResponse {
    pub steps_total: usize,
    pub steps_run: usize,
    pub steps_succeeded: usize,
    /// Index of the step that failed and stopped the batch, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at_index: Option<usize>,
    pub ok: bool,
    pub results: Vec<BrowserBatchStepResult>,
}

#[tool_router(router = browser_batch_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Execute an ordered list of browser sub-actions in one MCP call against the calling session's bound browser target (Claude browser_batch parity, #1337). Pure orchestration over vetted primitives — each step routes to the SAME code path as its standalone tool, so there are no new action semantics: action=navigate→cdp_navigate_tab, wait_for_selector/wait_for_url/wait_for_load_state→the matching browser_wait_for condition lane, set_value→browser_set_value, fill_form→browser_fill_form, file_upload→browser_file_upload, scroll_into_view→browser_scroll_into_view, evaluate→browser_evaluate, screenshot→browser_screenshot, click→target_act verb=click. Each step's `params` object is shaped exactly like that tool's parameters. Stop-on-first-error by default (interdependent steps compound errors): returns a per-step result array [{index, action, status (ok|error|skipped), ok, result|error}]; the failing step carries its full structured error and later steps are reported skipped. Set stop_on_error=false to run every step regardless. Bind a target first with set_target; an empty steps list is a loud error."
    )]
    pub async fn browser_batch(
        &self,
        params: Parameters<BrowserBatchParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserBatchResponse>, ErrorData> {
        let params = params.0;
        let stop_on_error = params.stop_on_error.unwrap_or(true);
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "browser_batch",
            steps = params.steps.len(),
            stop_on_error,
            "tool.invocation kind=browser_batch"
        );
        if params.steps.is_empty() {
            return Err(mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                "browser_batch requires a non-empty steps list",
            ));
        }

        let mut results: Vec<BrowserBatchStepResult> = Vec::with_capacity(params.steps.len());
        let mut steps_run = 0usize;
        let mut steps_succeeded = 0usize;
        let mut stopped_at_index: Option<usize> = None;
        let mut halted = false;

        for (index, step) in params.steps.into_iter().enumerate() {
            if halted {
                results.push(BrowserBatchStepResult {
                    index,
                    action: step.action,
                    status: STATUS_SKIPPED.to_owned(),
                    ok: false,
                    result: None,
                    error: None,
                });
                continue;
            }

            steps_run += 1;
            let mutates = browser_batch_action_mutates(&step.action);
            if mutates {
                super::operator_panic_boundary::ensure_mcp_mutation(
                    "browser_batch_before_mutating_step",
                )?;
            }
            let outcome = self
                .browser_batch_dispatch(
                    &step.action,
                    step.params,
                    session_id.as_deref(),
                    &request_context,
                )
                .await;
            let outcome = match outcome {
                Ok(result) if mutates => super::operator_panic_boundary::ensure_mcp_mutation(
                    "browser_batch_after_mutating_step",
                )
                .map(|()| result),
                other => other,
            };
            match outcome {
                Ok(result) => {
                    steps_succeeded += 1;
                    results.push(BrowserBatchStepResult {
                        index,
                        action: step.action,
                        status: STATUS_OK.to_owned(),
                        ok: true,
                        result: Some(result),
                        error: None,
                    });
                }
                Err(error) => {
                    results.push(BrowserBatchStepResult {
                        index,
                        action: step.action,
                        status: STATUS_ERROR.to_owned(),
                        ok: false,
                        result: None,
                        error: Some(error_data_to_value(&error)),
                    });
                    if stop_on_error {
                        stopped_at_index = Some(index);
                        halted = true;
                    }
                }
            }
        }

        let ok = stopped_at_index.is_none() && results.iter().all(|step| step.ok);
        Ok(Json(BrowserBatchResponse {
            steps_total: results.len(),
            steps_run,
            steps_succeeded,
            stopped_at_index,
            ok,
            results,
        }))
    }

    /// Route one batch step to its standalone tool's code path. Returns the
    /// tool's response as JSON, or the structured error (including a `target_act`
    /// step whose response is ok=false).
    async fn browser_batch_dispatch(
        &self,
        action: &str,
        params: Value,
        session_id: Option<&str>,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<Value, ErrorData> {
        // Enforce the caller's tool profile per sub-action: browser_batch must NOT
        // let a session reach a tool its profile hides (e.g. a normal_agent batch
        // invoking the browser_debugger-gated evaluate). The delegated-tool name is
        // the real tool the step routes to.
        let delegated_tool = match action {
            "navigate" => "cdp_navigate_tab",
            "wait_for_selector" => "browser_wait_for_selector",
            "wait_for_url" => "browser_wait_for_url",
            "wait_for_load_state" => "browser_wait_for_load_state",
            "set_value" => "browser_set_value",
            "fill_form" => "browser_fill_form",
            "file_upload" => "browser_file_upload",
            "scroll_into_view" => "browser_scroll_into_view",
            "evaluate" => "browser_evaluate",
            "screenshot" => "browser_screenshot",
            "click" => "target_act",
            other => {
                return Err(mcp_error(
                    synapse_core::error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "browser_batch unknown step action {other:?}; supported: navigate, wait_for_selector, wait_for_url, wait_for_load_state, click, set_value, fill_form, file_upload, scroll_into_view, evaluate, screenshot"
                    ),
                ));
            }
        };
        self.admit_tool_call_for_profile(delegated_tool, session_id)?;
        match action {
            "navigate" => {
                let parsed: CdpNavigateTabParams = browser_batch_parse(action, params)?;
                let response = self
                    .cdp_navigate_tab(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "wait_for_selector" => {
                let parsed: BrowserWaitForSelectorParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_wait_for_selector_inner(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "wait_for_url" => {
                let parsed: BrowserWaitForUrlParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_wait_for_url_inner(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "wait_for_load_state" => {
                let parsed: BrowserWaitForLoadStateParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_wait_for_load_state_inner(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "set_value" => {
                let parsed: BrowserSetValueParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_set_value(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "fill_form" => {
                let parsed: BrowserFillFormParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_fill_form(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "file_upload" => {
                let parsed: BrowserFileUploadParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_file_upload(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "scroll_into_view" => {
                let parsed: BrowserScrollIntoViewParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_scroll_into_view(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "evaluate" => {
                let parsed: BrowserEvaluateParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_evaluate(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "screenshot" => {
                let parsed: BrowserScreenshotParams = browser_batch_parse(action, params)?;
                let response = self
                    .browser_screenshot(Parameters(parsed), request_context.clone())
                    .await?;
                response_to_value(&response.0)
            }
            "click" => {
                // click has no standalone tool — it is target_act verb=click. Force
                // the verb so callers pass only the click selector/element/coords.
                let mut object = match params {
                    Value::Object(map) => map,
                    Value::Null => serde_json::Map::new(),
                    other => {
                        return Err(mcp_error(
                            synapse_core::error_codes::TOOL_PARAMS_INVALID,
                            format!(
                                "browser_batch step action=click params must be an object, got {other}"
                            ),
                        ));
                    }
                };
                object.insert("verb".to_owned(), json!("click"));
                let parsed: TargetActParams = browser_batch_parse(action, Value::Object(object))?;
                let response = self
                    .target_act(Parameters(parsed), request_context.clone())
                    .await?;
                if !response.0.ok {
                    return Err(mcp_error(
                        synapse_core::error_codes::ACTION_POSTCONDITION_FAILED,
                        format!(
                            "browser_batch step action=click failed: {}",
                            serde_json::to_string(&response.0).unwrap_or_default()
                        ),
                    ));
                }
                response_to_value(&response.0)
            }
            other => Err(mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_batch unknown step action {other:?}; supported: navigate, wait_for_selector, wait_for_url, wait_for_load_state, click, set_value, fill_form, file_upload, scroll_into_view, evaluate, screenshot"
                ),
            )),
        }
    }
}

fn browser_batch_action_mutates(action: &str) -> bool {
    matches!(
        action,
        "navigate"
            | "click"
            | "set_value"
            | "fill_form"
            | "file_upload"
            | "scroll_into_view"
            | "evaluate"
            | "screenshot"
    )
}

fn browser_batch_parse<T: for<'de> Deserialize<'de>>(
    action: &str,
    params: Value,
) -> Result<T, ErrorData> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|error| {
        mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            format!("browser_batch step action={action} has invalid params: {error}"),
        )
    })
}

fn response_to_value<T: Serialize>(response: &T) -> Result<Value, ErrorData> {
    serde_json::to_value(response).map_err(|error| {
        mcp_error(
            synapse_core::error_codes::TOOL_INTERNAL_ERROR,
            format!("browser_batch could not serialize a step response: {error}"),
        )
    })
}

fn error_data_to_value(error: &ErrorData) -> Value {
    json!({
        "code": error.code.0,
        "message": error.message,
        "data": error.data,
    })
}

#[cfg(test)]
mod operator_panic_tests {
    use super::browser_batch_action_mutates;

    #[test]
    fn classifies_every_supported_batch_action_for_operator_panic_checks() {
        for action in [
            "navigate",
            "click",
            "set_value",
            "fill_form",
            "file_upload",
            "scroll_into_view",
            "evaluate",
            "screenshot",
        ] {
            assert!(browser_batch_action_mutates(action), "{action}");
        }
        for action in ["wait_for_selector", "wait_for_url", "wait_for_load_state"] {
            assert!(!browser_batch_action_mutates(action), "{action}");
        }
    }
}
