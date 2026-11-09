//! Suggestion-engine MCP tools (#858) — own router, merged in `server.rs`.
//!
//! `suggestion_tick` runs one decision pass (expire/abandon + gated creation);
//! `suggestion_list` reads the persisted suggestion rows. Thin wrappers around
//! [`crate::m3::suggestions`], which owns the CF_KV truth and the anti-Clippy
//! gates.

use rmcp::{RoleServer, service::RequestContext};
use serde_json::{Value, json};
use synapse_core::{error_codes, types::RoutineFeedbackOutcome};

use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};

use crate::m1::CdpOpenTabParams;
use crate::m3::approvals::{
    ApprovalKind, ApprovalRequestParams, ApprovalTimeoutDecision, request_approval,
};
use crate::m3::armed_routines::{
    ARMED_ROUTINE_SOURCE_OF_TRUTH, ArmedRoutineRunRecord, ArmedRoutineRunStatus,
    ArmedRoutineTickParams, ArmedRoutineTickResponse, ArmedRoutineTickRun, claim_armed_run,
    complete_armed_run, dry_run_tick_run, due_armed_runs,
    required_permissions_tick as required_permissions_armed_tick, tick_run_from_record,
};
use crate::m3::episodes::now_ts_ns;
use crate::m3::plan::{
    PlanBackend, PlanDocument, PlanStep, Postcondition, RoutineCompilePlanParams,
    compile_routine_plan, load_plan,
};
use crate::m3::plan_execution::{
    PlanExecutionRecord, PlanExecutionStatus, PlanStepExecutionReport, PlanStepExecutionStatus,
    build_plan_execution_record, plan_execution_id, write_plan_execution,
};
use crate::m3::suggestions::{
    SuggestionAcceptParams, SuggestionAcceptResponse, SuggestionListParams, SuggestionListResponse,
    SuggestionRecord, SuggestionSource, SuggestionTickParams, SuggestionTickResponse,
    accept_suggestion_for_execution, assist_plan_for_suggestion, list_suggestions,
    load_suggestion_by_id, record_suggestion_execution_feedback, required_permissions_accept,
    required_permissions_list, required_permissions_tick, suggestion_tick,
    validate_stored_assist_target_hwnd,
};
use crate::m4::{ActLaunchParams, LaunchWindowState};

const PLAN_REF_PREFIX: &str = "plan/v1/";
const DEFAULT_EXECUTION_LAUNCH_TIMEOUT_MS: u64 = 10_000;
pub const ARMED_ROUTINE_INTERVAL_ENV: &str = "SYNAPSE_ARMED_ROUTINE_INTERVAL_SECS";
pub const ARMED_ROUTINE_STARTUP_DELAY_ENV: &str = "SYNAPSE_ARMED_ROUTINE_STARTUP_DELAY_SECS";
pub const DEFAULT_ARMED_ROUTINE_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_ARMED_ROUTINE_STARTUP_DELAY_SECS: u64 = 60;

#[derive(Clone, Debug)]
struct PlanExecutionOptions {
    dry_run: bool,
    browser_window_hwnd: Option<i64>,
    launch_timeout_ms: Option<u64>,
    idempotency_prefix: String,
    caller: &'static str,
}

impl PlanExecutionOptions {
    fn suggestion_accept(params: &SuggestionAcceptParams) -> Self {
        Self {
            dry_run: params.dry_run,
            browser_window_hwnd: params.browser_window_hwnd,
            launch_timeout_ms: params.launch_timeout_ms,
            idempotency_prefix: "suggestion_accept".to_owned(),
            caller: "suggestion_accept",
        }
    }

    fn armed_run(run_id: &str, params: &ArmedRoutineTickParams) -> Self {
        Self {
            dry_run: params.dry_run,
            browser_window_hwnd: params.browser_window_hwnd,
            launch_timeout_ms: params.launch_timeout_ms,
            idempotency_prefix: format!("armed_routine_tick:{run_id}"),
            caller: "armed_routine_tick",
        }
    }

    fn launch_timeout_ms(&self) -> u64 {
        match self.launch_timeout_ms {
            Some(timeout_ms) => timeout_ms,
            None => DEFAULT_EXECUTION_LAUNCH_TIMEOUT_MS,
        }
    }
}

pub(crate) fn spawn_periodic_armed_routine_runner(
    service: SynapseService,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let interval_secs = parse_secs_env(
        ARMED_ROUTINE_INTERVAL_ENV,
        DEFAULT_ARMED_ROUTINE_INTERVAL_SECS,
    )?;
    let startup_delay_secs = parse_secs_env(
        ARMED_ROUTINE_STARTUP_DELAY_ENV,
        DEFAULT_ARMED_ROUTINE_STARTUP_DELAY_SECS,
    )?;
    if interval_secs == 0 {
        tracing::info!(
            code = "ARMED_ROUTINE_PERIODIC_DISABLED",
            "periodic armed routine runner disabled via {ARMED_ROUTINE_INTERVAL_ENV}=0"
        );
        return Ok(None);
    }
    tracing::info!(
        code = "ARMED_ROUTINE_PERIODIC_SCHEDULED",
        interval_secs,
        startup_delay_secs,
        "periodic armed routine runner scheduled"
    );
    let handle = tokio::spawn(async move {
        let mut delay = std::time::Duration::from_secs(startup_delay_secs);
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!(
                        code = "ARMED_ROUTINE_PERIODIC_STOPPED",
                        "periodic armed routine runner stopped by daemon shutdown"
                    );
                    return;
                }
                () = tokio::time::sleep(delay) => {}
            }
            run_periodic_armed_routine_once(&service).await;
            delay = std::time::Duration::from_secs(interval_secs);
        }
    });
    Ok(Some(handle))
}

async fn run_periodic_armed_routine_once(service: &SynapseService) {
    match service
        .armed_routine_tick_impl(ArmedRoutineTickParams::default(), None)
        .await
    {
        Ok(response) => {
            tracing::info!(
                code = "ARMED_ROUTINE_PERIODIC_OK",
                evaluated = response.evaluated,
                due = response.due,
                executed = response.executed,
                skipped = response.skipped.len(),
                "periodic armed routine runner completed"
            );
        }
        Err(error) => {
            tracing::error!(
                code = "ARMED_ROUTINE_PERIODIC_FAILED",
                error_code = %error.code.0,
                detail = %error.message,
                "periodic armed routine runner failed; next run keeps the schedule"
            );
        }
    }
}

fn parse_secs_env(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => anyhow::bail!("{name} is not valid unicode: {error}"),
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(default)
            } else {
                trimmed.parse::<u64>().map_err(|error| {
                    anyhow::anyhow!(
                        "{name} must be an unsigned integer of seconds; got {value:?}: {error}"
                    )
                })
            }
        }
    }
}

#[tool_router(router = suggestions_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Run ONE suggestion-engine pass (#858): expire timed-out live suggestions (→ ignored_timeout feedback), abandon ones whose routine left the live intent set (→ abandoned feedback), then create suggestions for the routines the operator appears to be executing now that pass EVERY anti-Clippy gate — confidence threshold, #856 decline cooldown, quiet hours, dedup (one live per routine), per-routine frequency cap, and global frequency cap. Disabled/archived routines never surface. Truth is persisted in CF_KV (suggestion/v1/), so caps/dedup survive a daemon restart. Returns every candidate's gate decision (created or the precise suppression reason), plus the created/expired/abandoned ids and the active config. Pass now_ts_ns to evaluate a past instant (replay), or dry_run to compute decisions without persisting."
    )]
    pub async fn suggestion_tick(
        &self,
        params: Parameters<SuggestionTickParams>,
    ) -> Result<Json<SuggestionTickResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "suggestion_tick",
            now_ts_ns = params.0.now_ts_ns,
            dry_run = params.0.dry_run,
            "tool.invocation kind=suggestion_tick"
        );
        self.require_m3_permissions("suggestion_tick", &required_permissions_tick(&params.0))?;
        let db = self.m3_storage()?;
        suggestion_tick(&db, &params.0).map(Json)
    }

    #[tool(
        description = "List surfaced suggestions (#858) from CF_KV, newest first, optionally filtered by status (live/accepted/declined/expired/abandoned) and/or routine_id. Read-only — the operator-facing view of what the suggestion engine has produced and how each resolved."
    )]
    pub async fn suggestion_list(
        &self,
        params: Parameters<SuggestionListParams>,
    ) -> Result<Json<SuggestionListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "suggestion_list",
            status = ?params.0.status,
            routine_id = params.0.routine_id.as_deref(),
            "tool.invocation kind=suggestion_list"
        );
        self.require_m3_permissions("suggestion_list", &required_permissions_list(&params.0))?;
        let db = self.m3_storage()?;
        list_suggestions(&db, &params.0).map(Json)
    }

    #[tool(
        description = "Accept one live suggestion and execute its compiled setup plan (#860). Loads or compiles the routine plan, marks the durable suggestion/v1 row accepted, runs supported steps through background-first routes (act_launch for apps, cdp_open_tab for browser hosts), refuses unsupported/ambiguous steps loudly, verifies each mutating step's postcondition, persists a plan_execution/v1 report, and records routine feedback with the execution outcome. Assist report-only mitigations are recorded as skipped unless a scoped correction is actually attempted and verified. dry_run returns the same routing report without mutating storage or launching/opening anything."
    )]
    pub async fn suggestion_accept(
        &self,
        params: Parameters<SuggestionAcceptParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<SuggestionAcceptResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "suggestion_accept",
            suggestion_id = %params.0.suggestion_id,
            dry_run = params.0.dry_run,
            "tool.invocation kind=suggestion_accept"
        );
        self.require_m3_permissions("suggestion_accept", &required_permissions_accept(&params.0))?;
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.suggestion_accept_impl(params.0, session_id)
            .await
            .map(Json)
    }

    #[tool(
        description = "Run ONE armed-routine pass (#862). Evaluates enabled armed_routine/v1 rows for due schedule windows and/or live intent matches, claims each trigger before execution so restarts do not double-fire, runs the installed automation plan through the same background-first executor as suggestion_accept, persists plan_execution/v1 plus armed_routine_run/v1 audit rows, queues an armed_run_review approval for human outcome review, and self-disarms after the configured consecutive failure threshold. dry_run computes due runs and routing reports without mutating storage or launching/opening anything."
    )]
    pub async fn armed_routine_tick(
        &self,
        params: Parameters<ArmedRoutineTickParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ArmedRoutineTickResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "armed_routine_tick",
            routine_id = params.0.routine_id.as_deref(),
            trigger_mode = ?params.0.trigger_mode,
            dry_run = params.0.dry_run,
            "tool.invocation kind=armed_routine_tick"
        );
        self.require_m3_permissions(
            "armed_routine_tick",
            &required_permissions_armed_tick(&params.0),
        )?;
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.armed_routine_tick_impl(params.0, session_id)
            .await
            .map(Json)
    }
}

impl SynapseService {
    async fn suggestion_accept_impl(
        &self,
        params: SuggestionAcceptParams,
        session_id: Option<String>,
    ) -> Result<SuggestionAcceptResponse, ErrorData> {
        validate_suggestion_accept_params(&params)?;
        let db = self.m3_storage()?;
        let Some(existing) = load_suggestion_by_id(&db, &params.suggestion_id)? else {
            return Err(crate::m1::mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "SUGGESTION_NOT_FOUND: suggestion_id {} is not in CF_KV",
                    params.suggestion_id
                ),
            ));
        };
        if existing.source == SuggestionSource::AssistOpportunity {
            return self
                .assist_suggestion_accept_impl(db, existing, params, session_id)
                .await;
        }
        let plan = load_or_compile_plan(&db, &existing.routine_id, !params.dry_run)?;
        let accepted_ts_ns = now_ts_ns();
        let started_ts_ns = accepted_ts_ns;
        let execution_id = plan_execution_id(&existing.suggestion_id, started_ts_ns);
        let plan_ref = format!("{PLAN_REF_PREFIX}{}", plan.routine_id);
        let accepted = accept_suggestion_for_execution(
            &db,
            &existing.suggestion_id,
            accepted_ts_ns,
            &plan_ref,
            &execution_id,
            params.dry_run,
        )?;

        let options = PlanExecutionOptions::suggestion_accept(&params);
        let steps = self
            .execute_plan_steps(&plan, &options, session_id.as_deref())
            .await;
        let completed_ts_ns = now_ts_ns();
        let execution = build_plan_execution_record(
            execution_id,
            accepted.suggestion_id.clone(),
            session_id,
            accepted_ts_ns,
            started_ts_ns,
            completed_ts_ns,
            params.dry_run,
            plan.clone(),
            steps,
        );
        if !params.dry_run {
            write_plan_execution(&db, &execution)?;
            let feedback_note = format!(
                "suggestion_accept execution_status={} execution_id={} succeeded_steps={} failed_or_refused_steps={}",
                execution.status.as_str(),
                execution.execution_id,
                execution
                    .steps
                    .iter()
                    .filter(|step| step.status == PlanStepExecutionStatus::Succeeded)
                    .count(),
                execution
                    .steps
                    .iter()
                    .filter(|step| step.status.is_terminal_failure())
                    .count()
            );
            record_suggestion_execution_feedback(
                &db,
                &accepted.routine_id,
                RoutineFeedbackOutcome::Accepted,
                completed_ts_ns,
                &feedback_note,
            )?;
        }
        Ok(SuggestionAcceptResponse {
            suggestion: accepted,
            plan,
            execution,
        })
    }

    async fn assist_suggestion_accept_impl(
        &self,
        db: std::sync::Arc<synapse_storage::Db>,
        existing: SuggestionRecord,
        params: SuggestionAcceptParams,
        session_id: Option<String>,
    ) -> Result<SuggestionAcceptResponse, ErrorData> {
        let accepted_ts_ns = now_ts_ns();
        let started_ts_ns = accepted_ts_ns;
        let plan = assist_plan_for_suggestion(&existing, accepted_ts_ns)?;
        let execution_id = plan_execution_id(&existing.suggestion_id, started_ts_ns);
        let plan_ref = format!(
            "assist-opportunity/v1/{}",
            existing
                .source_event_id
                .as_deref()
                .unwrap_or(&existing.routine_id)
        );
        let accepted = accept_suggestion_for_execution(
            &db,
            &existing.suggestion_id,
            accepted_ts_ns,
            &plan_ref,
            &execution_id,
            params.dry_run,
        )?;
        let options = PlanExecutionOptions::suggestion_accept(&params);
        let steps = if params.dry_run {
            plan.steps
                .iter()
                .map(|step| dry_run_step_report(step, &options))
                .collect()
        } else {
            self.execute_assist_plan_steps(&accepted, &plan, &options)
                .await
        };
        let completed_ts_ns = now_ts_ns();
        let execution = build_plan_execution_record(
            execution_id,
            accepted.suggestion_id.clone(),
            session_id,
            accepted_ts_ns,
            started_ts_ns,
            completed_ts_ns,
            params.dry_run,
            plan.clone(),
            steps,
        );
        if !params.dry_run {
            write_plan_execution(&db, &execution)?;
            tracing::info!(
                code = "ASSIST_SUGGESTION_ACCEPTED",
                suggestion_id = %accepted.suggestion_id,
                source_event_id = accepted.source_event_id.as_deref().unwrap_or(""),
                execution_id = %execution.execution_id,
                execution_status = execution.status.as_str(),
                "assist suggestion accepted and execution report persisted"
            );
        }
        Ok(SuggestionAcceptResponse {
            suggestion: accepted,
            plan,
            execution,
        })
    }

    async fn armed_routine_tick_impl(
        &self,
        params: ArmedRoutineTickParams,
        session_id: Option<String>,
    ) -> Result<ArmedRoutineTickResponse, ErrorData> {
        let db = self.m3_storage()?;
        let (now, evaluated, due, skipped) = due_armed_runs(&db, &params)?;
        let due_count = u32::try_from(due.len()).unwrap_or(u32::MAX);
        let mut runs: Vec<ArmedRoutineTickRun> = Vec::new();

        for due_run in due {
            if params.dry_run {
                runs.push(dry_run_tick_run(&due_run));
                continue;
            }
            let claimed = claim_armed_run(&db, &due_run, now)?;
            let completed = self
                .execute_claimed_armed_run(&db, &params, claimed, session_id.clone())
                .await?;
            runs.push(tick_run_from_record(&completed));
        }

        let executed = runs
            .iter()
            .filter(|run| run.status != ArmedRoutineRunStatus::DryRun)
            .count();
        Ok(ArmedRoutineTickResponse {
            now_ts_ns: now,
            dry_run: params.dry_run,
            evaluated,
            due: due_count,
            executed: u32::try_from(executed).unwrap_or(u32::MAX),
            skipped,
            runs,
            source_of_truth: ARMED_ROUTINE_SOURCE_OF_TRUTH.to_owned(),
        })
    }

    async fn execute_claimed_armed_run(
        &self,
        db: &std::sync::Arc<synapse_storage::Db>,
        params: &ArmedRoutineTickParams,
        run: ArmedRoutineRunRecord,
        session_id: Option<String>,
    ) -> Result<ArmedRoutineRunRecord, ErrorData> {
        let plan = match load_or_compile_plan(db, &run.routine_id, true) {
            Ok(plan) => plan,
            Err(error) => {
                return complete_armed_run(
                    db,
                    run,
                    ArmedRoutineRunStatus::Failed,
                    None,
                    None,
                    None,
                    error_data_code(&error),
                    Some(error.message.to_string()),
                    json!({ "error_data": error.data }),
                );
            }
        };
        let started_ts_ns = now_ts_ns();
        let execution_id = plan_execution_id(&run.run_id, started_ts_ns);
        let options = PlanExecutionOptions::armed_run(&run.run_id, params);
        let steps = self
            .execute_plan_steps(&plan, &options, session_id.as_deref())
            .await;
        let completed_ts_ns = now_ts_ns();
        let execution = build_plan_execution_record(
            execution_id,
            run.run_id.clone(),
            session_id.clone(),
            run.started_ts_ns,
            started_ts_ns,
            completed_ts_ns,
            false,
            plan.clone(),
            steps,
        );
        write_plan_execution(db, &execution)?;

        let approval_id = match queue_armed_run_review(
            db,
            &run,
            &execution,
            session_id.as_deref().unwrap_or("armed-routine-runner"),
        ) {
            Ok(approval_id) => Some(approval_id),
            Err(error) => {
                return complete_armed_run(
                    db,
                    run,
                    ArmedRoutineRunStatus::Failed,
                    Some(format!("{PLAN_REF_PREFIX}{}", plan.routine_id)),
                    Some(execution.execution_id),
                    None,
                    error_data_code(&error),
                    Some(error.message.to_string()),
                    json!({
                        "plan_execution_status": execution.status.as_str(),
                        "approval_request_error": error.data,
                    }),
                );
            }
        };

        let status = match execution.status {
            PlanExecutionStatus::Succeeded => ArmedRoutineRunStatus::Succeeded,
            PlanExecutionStatus::Failed
            | PlanExecutionStatus::Skipped
            | PlanExecutionStatus::DryRun => ArmedRoutineRunStatus::Failed,
        };
        let (error_code, error) = if status == ArmedRoutineRunStatus::Failed {
            (
                Some("ARMED_ROUTINE_PLAN_EXECUTION_FAILED".to_owned()),
                Some(format!(
                    "plan execution {} ended with status {}",
                    execution.execution_id,
                    execution.status.as_str()
                )),
            )
        } else {
            (None, None)
        };
        complete_armed_run(
            db,
            run,
            status,
            Some(format!("{PLAN_REF_PREFIX}{}", plan.routine_id)),
            Some(execution.execution_id),
            approval_id,
            error_code,
            error,
            json!({
                "plan_execution_status": execution.status.as_str(),
                "succeeded_steps": execution
                    .steps
                    .iter()
                    .filter(|step| step.status == PlanStepExecutionStatus::Succeeded)
                    .count(),
                "failed_or_refused_steps": execution
                    .steps
                    .iter()
                    .filter(|step| step.status.is_terminal_failure())
                    .count(),
            }),
        )
    }

    async fn execute_plan_steps(
        &self,
        plan: &PlanDocument,
        options: &PlanExecutionOptions,
        session_id: Option<&str>,
    ) -> Vec<PlanStepExecutionReport> {
        let mut reports = Vec::with_capacity(plan.steps.len());
        let mut aborted = false;
        for step in &plan.steps {
            let report = if aborted {
                skipped_step_report(
                    step,
                    "previous step failed or was refused; execution aborted",
                )
            } else if options.dry_run {
                dry_run_step_report(step, options)
            } else {
                self.execute_plan_step(step, options, session_id).await
            };
            if report.status.is_terminal_failure() {
                aborted = true;
            }
            reports.push(report);
        }
        reports
    }

    async fn execute_assist_plan_steps(
        &self,
        suggestion: &SuggestionRecord,
        plan: &PlanDocument,
        options: &PlanExecutionOptions,
    ) -> Vec<PlanStepExecutionReport> {
        plan.steps
            .iter()
            .map(|step| self.execute_assist_mitigation_step(suggestion, step, options))
            .collect()
    }

    fn execute_assist_mitigation_step(
        &self,
        suggestion: &SuggestionRecord,
        step: &PlanStep,
        options: &PlanExecutionOptions,
    ) -> PlanStepExecutionReport {
        let started = now_ts_ns();
        let Some(mitigation) = &suggestion.mitigation else {
            return step_report(
                started,
                step,
                PlanStepExecutionStatus::Failed,
                json!({
                    "suggestion_id": &suggestion.suggestion_id,
                    "caller": options.caller,
                }),
                Some("ASSIST_MITIGATION_MISSING"),
                Some("assist suggestion has no mitigation payload".to_owned()),
            );
        };
        let Some(hwnd) = mitigation.target_window_hwnd else {
            return step_report(
                started,
                step,
                PlanStepExecutionStatus::Refused,
                json!({
                    "suggestion_id": &suggestion.suggestion_id,
                    "source_event_id": &mitigation.source_event_id,
                    "detector": &mitigation.detector,
                    "caller": options.caller,
                    "reason": "assist event did not identify a target HWND",
                }),
                Some("ASSIST_TARGET_UNIDENTIFIED"),
                Some(
                    "assist correction requires a target HWND so the in-session report can read back the physical app state"
                        .to_owned(),
                ),
            );
        };
        let hwnd = match validate_stored_assist_target_hwnd(
            &format!("CF_KV suggestion row {}", suggestion.suggestion_id),
            hwnd,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => return error_step_report(started, step, &error),
        };
        match synapse_a11y::foreground_context(hwnd) {
            Ok(context) => {
                let process_matches = mitigation
                    .process_name
                    .as_ref()
                    .is_none_or(|expected| expected.eq_ignore_ascii_case(&context.process_name));
                let pid_matches = mitigation.target_pid.is_none_or(|pid| pid == context.pid);
                let evidence = json!({
                    "suggestion_id": &suggestion.suggestion_id,
                    "source_event_id": &mitigation.source_event_id,
                    "detector": &mitigation.detector,
                    "strategy": mitigation.strategy,
                    "caller": options.caller,
                    "target_readback": {
                        "hwnd": context.hwnd,
                        "pid": context.pid,
                        "process_name": &context.process_name,
                        "window_title_present": !context.window_title.is_empty(),
                        "is_fullscreen": context.is_fullscreen,
                        "is_dwm_composed": context.is_dwm_composed,
                    },
                    "expected_target": {
                        "hwnd": hwnd,
                        "pid": mitigation.target_pid,
                        "process_name": &mitigation.process_name,
                    },
                    "process_matches": process_matches,
                    "pid_matches": pid_matches,
                    "correction_attempt": {
                        "mode": "in_session_report",
                        "mutation": "not_attempted_without_verifiable_desired_state",
                        "status": "report_only",
                        "honesty": "privacy-safe detector evidence does not include raw user content; the assist acceptance produced a scoped report and fresh target readback without claiming a correction was applied"
                    },
                    "postcondition": mitigation.postcondition,
                });
                if process_matches && pid_matches {
                    step_report(
                        started,
                        step,
                        PlanStepExecutionStatus::Skipped,
                        evidence,
                        Some("ASSIST_CORRECTION_REPORT_ONLY"),
                        Some(
                            "assist produced a scoped target readback report only; no correction was attempted because no verifiable desired state was available"
                                .to_owned(),
                        ),
                    )
                } else {
                    step_report(
                        started,
                        step,
                        PlanStepExecutionStatus::Failed,
                        evidence,
                        Some("ASSIST_TARGET_READBACK_MISMATCH"),
                        Some(
                            "assist target HWND resolved, but process/pid no longer match the detector evidence"
                                .to_owned(),
                        ),
                    )
                }
            }
            Err(error) => step_report(
                started,
                step,
                PlanStepExecutionStatus::Failed,
                json!({
                    "suggestion_id": &suggestion.suggestion_id,
                    "source_event_id": &mitigation.source_event_id,
                    "detector": &mitigation.detector,
                    "target_hwnd": hwnd,
                    "caller": options.caller,
                    "a11y_error": error.to_string(),
                }),
                Some("ASSIST_TARGET_READBACK_FAILED"),
                Some(format!(
                    "assist correction target hwnd 0x{hwnd:x} could not be read back: {error}"
                )),
            ),
        }
    }

    async fn execute_plan_step(
        &self,
        step: &PlanStep,
        options: &PlanExecutionOptions,
        session_id: Option<&str>,
    ) -> PlanStepExecutionReport {
        match step.backend {
            PlanBackend::ActLaunch => self.execute_launch_step(step, options, session_id).await,
            PlanBackend::CdpOpenTab => {
                self.execute_cdp_open_tab_step(step, options, session_id)
                    .await
            }
            PlanBackend::ShellOpen => refused_step_report(
                step,
                "PLAN_EXECUTOR_BACKEND_UNSUPPORTED",
                "shell_open execution is not implemented yet; refusing instead of silently opening an unverified document",
                json!({
                    "source_app": &step.source_app,
                    "source_document": &step.source_document,
                }),
            ),
            PlanBackend::AgentTask => refused_step_report(
                step,
                "PLAN_EXECUTOR_AGENT_TASK_REQUIRED",
                step.agent_task_reason.as_deref().unwrap_or(
                    "plan step requires agent judgment; no agent was spawned by this executor",
                ),
                json!({
                    "caller": options.caller,
                    "agent_task_reason": &step.agent_task_reason,
                    "source_app": &step.source_app,
                    "source_document": &step.source_document,
                }),
            ),
        }
    }

    async fn execute_launch_step(
        &self,
        step: &PlanStep,
        options: &PlanExecutionOptions,
        session_id: Option<&str>,
    ) -> PlanStepExecutionReport {
        let started = now_ts_ns();
        let preflight = match self.ensure_supported_use_allows_action("act_launch") {
            Ok(preflight) => preflight,
            Err(error) => return error_step_report(started, step, &error),
        };
        let launch = ActLaunchParams {
            target: step.source_app.clone(),
            args: Vec::new(),
            working_dir: None,
            env: Default::default(),
            wait_for_window_title_regex: Some(".*".to_owned()),
            timeout_ms: options.launch_timeout_ms(),
            idempotency_key: Some(format!(
                "{}:{}:step:{}",
                options.idempotency_prefix, step.source_app, step.index
            )),
            cdp_debug: Some(false),
            force_renderer_accessibility: None,
            windows_console_window_state: Some(LaunchWindowState::Hidden),
            desktop: session_id.map(|_| "agent:session".to_owned()),
        };
        let result = self
            .act_launch_for_session_id(launch, session_id.map(ToOwned::to_owned), &preflight)
            .await;
        match result {
            Ok(response) => {
                let evidence = json!({ "act_launch": response });
                match &step.postcondition {
                    Postcondition::WindowForProcessExists { .. } if response.hwnd.is_some() => {
                        step_report(
                            started,
                            step,
                            PlanStepExecutionStatus::Succeeded,
                            evidence,
                            None,
                            None,
                        )
                    }
                    Postcondition::WindowForProcessExists { process } => step_report(
                        started,
                        step,
                        PlanStepExecutionStatus::Failed,
                        evidence,
                        Some(error_codes::ACTION_POSTCONDITION_FAILED),
                        Some(format!(
                            "act_launch returned without a window for expected process {process}"
                        )),
                    ),
                    other => step_report(
                        started,
                        step,
                        PlanStepExecutionStatus::Failed,
                        evidence,
                        Some(error_codes::ACTION_POSTCONDITION_FAILED),
                        Some(format!(
                            "act_launch step produced a launch response but cannot verify postcondition {other:?}"
                        )),
                    ),
                }
            }
            Err(error) => error_step_report(started, step, &error),
        }
    }

    async fn execute_cdp_open_tab_step(
        &self,
        step: &PlanStep,
        options: &PlanExecutionOptions,
        session_id: Option<&str>,
    ) -> PlanStepExecutionReport {
        let started = now_ts_ns();
        let Some(session_id) = session_id else {
            return step_report(
                started,
                step,
                PlanStepExecutionStatus::Refused,
                json!({
                    "browser_window_hwnd": options.browser_window_hwnd,
                    "source_document": &step.source_document,
                }),
                Some(error_codes::HTTP_SESSION_INVALID),
                Some("cdp_open_tab plan steps require an MCP session id; refusing to use the human foreground browser implicitly".to_owned()),
            );
        };
        let Some(host) = browser_host_for_step(step) else {
            return refused_step_report(
                step,
                "PLAN_EXECUTOR_BROWSER_HOST_MISSING",
                "cdp_open_tab step did not carry a BrowserTabAtHost postcondition or source document host",
                json!({
                    "postcondition": &step.postcondition,
                    "source_document": &step.source_document,
                }),
            );
        };
        let requested_url = format!("https://{host}");
        let result = self
            .cdp_open_tab_for_session(
                CdpOpenTabParams {
                    window_hwnd: options.browser_window_hwnd,
                    url: requested_url.clone(),
                },
                session_id,
            )
            .await;
        match result {
            Ok(response) => {
                let host_matches = url_host_matches(&response.target_url, &host);
                let evidence = json!({
                    "requested_host": host,
                    "requested_url": requested_url,
                    "cdp_open_tab": response,
                    "host_matches": host_matches,
                });
                if host_matches {
                    step_report(
                        started,
                        step,
                        PlanStepExecutionStatus::Succeeded,
                        evidence,
                        None,
                        None,
                    )
                } else {
                    step_report(
                        started,
                        step,
                        PlanStepExecutionStatus::Failed,
                        evidence,
                        Some(error_codes::ACTION_POSTCONDITION_FAILED),
                        Some(
                            "cdp_open_tab target_url host did not match the plan postcondition"
                                .to_owned(),
                        ),
                    )
                }
            }
            Err(error) => error_step_report(started, step, &error),
        }
    }
}

fn validate_suggestion_accept_params(params: &SuggestionAcceptParams) -> Result<(), ErrorData> {
    if let Some(browser_window_hwnd) = params.browser_window_hwnd {
        crate::m1::validate_hwnd_shape(
            "suggestion_accept",
            "browser_window_hwnd",
            browser_window_hwnd,
        )?;
    }
    if params.suggestion_id.trim().is_empty() {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "suggestion_accept suggestion_id must not be empty",
        ));
    }
    if let Some(timeout_ms) = params.launch_timeout_ms
        && timeout_ms == 0
    {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "suggestion_accept launch_timeout_ms must be >= 1",
        ));
    }
    Ok(())
}

fn queue_armed_run_review(
    db: &std::sync::Arc<synapse_storage::Db>,
    run: &ArmedRoutineRunRecord,
    execution: &PlanExecutionRecord,
    by_session: &str,
) -> Result<String, ErrorData> {
    let succeeded_steps = execution
        .steps
        .iter()
        .filter(|step| step.status == PlanStepExecutionStatus::Succeeded)
        .count();
    let failed_or_refused_steps = execution
        .steps
        .iter()
        .filter(|step| step.status.is_terminal_failure())
        .count();
    let payload = json!({
        "kind": "armed_routine_run_review",
        "source_of_truth": ARMED_ROUTINE_SOURCE_OF_TRUTH,
        "routine_id": run.routine_id,
        "run_id": run.run_id,
        "trigger_kind": run.trigger_kind,
        "trigger_key": run.trigger_key,
        "execution_id": execution.execution_id,
        "execution_status": execution.status.as_str(),
        "succeeded_steps": succeeded_steps,
        "failed_or_refused_steps": failed_or_refused_steps,
        "plan_execution_ref": format!("plan_execution/v1/{}", execution.execution_id),
        "armed_run_ref": format!("armed_routine_run/v1/{}", run.run_id),
    });
    let payload_json = serde_json::to_string(&payload).map_err(|error| {
        crate::m1::mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("armed run review payload encoding failed: {error}"),
        )
    })?;
    let status_label = execution.status.as_str();
    let request = ApprovalRequestParams {
        kind: ApprovalKind::ArmedRunReview,
        title: format!("Review armed routine {}", run.routine_id),
        body: format!(
            "Armed routine {} fired by {} and finished {status_label}: {} succeeded, {} failed/refused.",
            run.routine_id,
            run.trigger_kind.as_str(),
            succeeded_steps,
            failed_or_refused_steps
        ),
        payload_json: Some(payload_json),
        dedupe_key: Some(format!("armed_routine_run_review:{}", run.run_id)),
        timeout_ms: None,
        timeout_decision: Some(ApprovalTimeoutDecision::Ignored),
        destructive: false,
        notify: true,
        suppress_popup: false,
        allow: None,
    };
    let response = request_approval(db, &request, by_session)?;
    Ok(response.item.approval_id)
}

fn load_or_compile_plan(
    db: &std::sync::Arc<synapse_storage::Db>,
    routine_id: &str,
    store: bool,
) -> Result<PlanDocument, ErrorData> {
    if let Some(plan) = load_plan(db, routine_id)? {
        return Ok(plan);
    }
    Ok(compile_routine_plan(
        db,
        &RoutineCompilePlanParams {
            routine_id: routine_id.to_owned(),
            store,
        },
    )?
    .plan)
}

fn browser_host_for_step(step: &PlanStep) -> Option<String> {
    match &step.postcondition {
        Postcondition::BrowserTabAtHost { host } => Some(host.clone()),
        _ => step.source_document.clone(),
    }
}

fn dry_run_step_report(step: &PlanStep, options: &PlanExecutionOptions) -> PlanStepExecutionReport {
    let started = now_ts_ns();
    step_report(
        started,
        step,
        PlanStepExecutionStatus::DryRun,
        json!({
            "dry_run": true,
            "backend": step.backend,
            "caller": options.caller,
            "browser_window_hwnd": options.browser_window_hwnd,
            "launch_timeout_ms": options.launch_timeout_ms(),
        }),
        None,
        None,
    )
}

fn skipped_step_report(step: &PlanStep, reason: &str) -> PlanStepExecutionReport {
    let started = now_ts_ns();
    step_report(
        started,
        step,
        PlanStepExecutionStatus::Skipped,
        json!({ "reason": reason }),
        Some("PLAN_EXECUTOR_STEP_SKIPPED"),
        Some(reason.to_owned()),
    )
}

fn refused_step_report(
    step: &PlanStep,
    code: &'static str,
    reason: &str,
    evidence: Value,
) -> PlanStepExecutionReport {
    let started = now_ts_ns();
    step_report(
        started,
        step,
        PlanStepExecutionStatus::Refused,
        evidence,
        Some(code),
        Some(reason.to_owned()),
    )
}

fn error_step_report(started: u64, step: &PlanStep, error: &ErrorData) -> PlanStepExecutionReport {
    let code = error_data_code(error);
    let status = if is_refusal_code(code.as_deref()) {
        PlanStepExecutionStatus::Refused
    } else {
        PlanStepExecutionStatus::Failed
    };
    step_report(
        started,
        step,
        status,
        json!({
            "error_data": &error.data,
        }),
        code.as_deref(),
        Some(error.message.to_string()),
    )
}

fn step_report(
    started: u64,
    step: &PlanStep,
    status: PlanStepExecutionStatus,
    evidence: Value,
    error_code: Option<&str>,
    error: Option<String>,
) -> PlanStepExecutionReport {
    let completed = now_ts_ns();
    PlanStepExecutionReport {
        index: step.index,
        backend: step.backend,
        action: step.action.clone(),
        postcondition: step.postcondition.clone(),
        status,
        started_ts_ns: started,
        completed_ts_ns: completed,
        duration_ns: completed.saturating_sub(started),
        evidence,
        error_code: error_code.map(ToOwned::to_owned),
        error,
    }
}

fn error_data_code(error: &ErrorData) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn is_refusal_code(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            error_codes::SAFETY_PERMISSION_DENIED
                | error_codes::SAFETY_PROFILE_ACTION_DENIED
                | error_codes::SAFETY_LAUNCH_DENIED_BY_POLICY
                | error_codes::HTTP_SESSION_INVALID
                | error_codes::TOOL_PARAMS_INVALID
        )
    )
}

fn url_host_matches(url: &str, expected_host: &str) -> bool {
    let expected = expected_host
        .trim()
        .trim_end_matches('/')
        .to_ascii_lowercase();
    if expected.is_empty() {
        return false;
    }
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == expected)
}
