//! Armed routine execution state (#862).
//!
//! `routine_update action=arm|disarm` owns the durable arming row in `CF_KV`.
//! The server-side `armed_routine_tick` tool and periodic daemon job use these
//! helpers to select due runs, claim a trigger before execution, and record the
//! final outcome. Durable trigger keys are written before execution so daemon
//! restarts do not double-fire the same schedule window or intent evidence.

use std::sync::Arc;

use chrono::{Datelike, Local, TimeZone};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;
use synapse_core::types::{RoutineDowClass, RoutineLifecycle, RoutineRecord};
use synapse_storage::{Db, cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::episodes::{hex_encode, key_after, local_day_start, now_ts_ns};
use super::intent::{IntentCurrentParams, current_intents};
use super::permissions::{Permission, RequiredPermissions, required};
use super::profile_authoring::load_routine_automation_record;
use super::routines::{load_routine_record, load_state_row, validate_routine_id_param};

const ARMED_ROUTINE_PREFIX: &str = "armed_routine/v1/";
const ARMED_ROUTINE_RUN_PREFIX: &str = "armed_routine_run/v1/";
const ARMED_ROUTINE_RECORD_VERSION: u32 = 1;
const ARMED_ROUTINE_RUN_RECORD_VERSION: u32 = 1;
const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
const MAX_FAILURE_THRESHOLD: u32 = 20;
const MIN_SCHEDULE_WINDOW_MINUTES: u32 = 5;
const MAX_SCAN_ROWS: usize = 200_000;
const SCAN_CHUNK_ROWS: usize = 4_096;

pub const ARMED_ROUTINE_SOURCE_OF_TRUTH: &str =
    "CF_KV armed_routine/v1 and armed_routine_run/v1 rows plus plan_execution/v1 rows";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArmedRoutineTriggerKind {
    Schedule,
    Intent,
}

impl ArmedRoutineTriggerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Intent => "intent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArmedRoutineRunStatus {
    Started,
    Succeeded,
    Failed,
    DryRun,
}

impl ArmedRoutineRunStatus {
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineRecord {
    pub record_version: u32,
    pub row_kind: String,
    pub routine_id: String,
    pub enabled: bool,
    pub schedule_enabled: bool,
    pub intent_enabled: bool,
    pub failure_threshold: u32,
    pub consecutive_failures: u32,
    pub created_at_ns: u64,
    pub updated_at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disarmed_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disarmed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disarm_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_schedule_fire_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_intent_fire_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<ArmedRoutineRunStatus>,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineIntentEvidence {
    pub confidence: f64,
    pub matched_prefix_len: u32,
    pub total_steps: u32,
    pub last_matched_end_ts_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineDueRun {
    pub routine_id: String,
    pub trigger_kind: ArmedRoutineTriggerKind,
    pub trigger_key: String,
    pub due_ts_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<ArmedRoutineIntentEvidence>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineTickSkip {
    pub routine_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineRunRecord {
    pub record_version: u32,
    pub row_kind: String,
    pub run_id: String,
    pub routine_id: String,
    pub trigger_kind: ArmedRoutineTriggerKind,
    pub trigger_key: String,
    pub started_ts_ns: u64,
    pub completed_ts_ns: u64,
    pub dry_run: bool,
    pub status: ArmedRoutineRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    pub failure_count_after: u32,
    pub disarmed_after_failure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<ArmedRoutineIntentEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub evidence: Value,
    pub source_of_truth: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ArmedRoutineTickTriggerMode {
    Schedule,
    Intent,
    #[default]
    Both,
}


#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineTickParams {
    /// Evaluate as of this instant (replay/test). Defaults to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_ts_ns: Option<u64>,
    /// Limit the tick to one routine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routine_id: Option<String>,
    /// Which trigger family to evaluate. Defaults to both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_mode: Option<ArmedRoutineTickTriggerMode>,
    /// Compute due runs and per-step routing without mutating storage or
    /// launching/opening anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Recent-activity lookback handed to the intent matcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_hours: Option<u32>,
    /// Browser HWND used by `cdp_open_tab` steps when this tick is invoked from
    /// an MCP session. Periodic daemon ticks have no session, so browser steps
    /// refuse rather than using the human foreground implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_window_hwnd: Option<i64>,
    /// Timeout applied to launch-window/postcondition waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineTickRun {
    pub routine_id: String,
    pub trigger_kind: ArmedRoutineTriggerKind,
    pub trigger_key: String,
    pub status: ArmedRoutineRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    pub failure_count_after: u32,
    pub disarmed_after_failure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmedRoutineTickResponse {
    pub now_ts_ns: u64,
    pub dry_run: bool,
    pub evaluated: u32,
    pub due: u32,
    pub executed: u32,
    pub skipped: Vec<ArmedRoutineTickSkip>,
    pub runs: Vec<ArmedRoutineTickRun>,
    pub source_of_truth: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArmRoutineConfig {
    pub schedule_enabled: bool,
    pub intent_enabled: bool,
    pub failure_threshold: u32,
}

impl ArmRoutineConfig {
    #[must_use]
    pub fn from_optional(
        schedule_enabled: Option<bool>,
        intent_enabled: Option<bool>,
        failure_threshold: Option<u32>,
    ) -> Self {
        Self {
            schedule_enabled: schedule_enabled.unwrap_or(true),
            intent_enabled: intent_enabled.unwrap_or(true),
            failure_threshold: failure_threshold.unwrap_or(DEFAULT_FAILURE_THRESHOLD),
        }
    }
}

#[must_use]
pub const fn armed_routine_tick() -> super::M3ToolStub {
    super::M3ToolStub::new("armed_routine_tick")
}

#[must_use]
pub fn required_permissions_tick(_params: &ArmedRoutineTickParams) -> RequiredPermissions {
    required([Permission::ReadStorage, Permission::WriteStorage])
}

pub fn validate_arm_config(config: ArmRoutineConfig) -> Result<(), ErrorData> {
    if !config.schedule_enabled && !config.intent_enabled {
        return Err(invalid(
            "routine_update action=arm requires at least one trigger: arm_schedule or arm_intent",
        ));
    }
    if !(1..=MAX_FAILURE_THRESHOLD).contains(&config.failure_threshold) {
        return Err(invalid(format!(
            "routine_update failure_threshold must be between 1 and {MAX_FAILURE_THRESHOLD}; got {}",
            config.failure_threshold
        )));
    }
    Ok(())
}

pub fn arm_routine(
    db: &Arc<Db>,
    routine_id: &str,
    config: ArmRoutineConfig,
    by_session: &str,
    note: Option<String>,
) -> Result<ArmedRoutineRecord, ErrorData> {
    validate_routine_id_param("routine_update", routine_id)?;
    validate_arm_config(config)?;
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "routine_update action=arm refused under disk pressure: cf_name={} pressure_level={:?}",
                cf::CF_KV,
                db.pressure_level()
            ),
        ));
    }
    let Some(_routine) = load_routine_record(db.as_ref(), routine_id)? else {
        return Err(invalid(format!(
            "ROUTINE_NOT_MINED: routine_id {routine_id} is not in CF_ROUTINES; run routine_mine before arming"
        )));
    };
    let Some(automation) = load_routine_automation_record(db, routine_id)? else {
        return Err(invalid(format!(
            "ROUTINE_AUTOMATION_NOT_INSTALLED: routine_id {routine_id} has no routine_automation row; run routine_automate and accept the profile-authoring candidate before arming"
        )));
    };
    if automation.state != "installed" || automation.plan_ref.trim().is_empty() {
        return Err(invalid(format!(
            "ROUTINE_AUTOMATION_NOT_INSTALLED: routine_id {routine_id} automation state is {:?}, plan_ref={:?}; accept the profile-authoring candidate before arming",
            automation.state, automation.plan_ref
        )));
    }

    let now = now_ts_ns();
    let existing = load_armed_routine_record(db, routine_id)?;
    let mut record = existing.unwrap_or_else(|| ArmedRoutineRecord {
        record_version: ARMED_ROUTINE_RECORD_VERSION,
        row_kind: "armed_routine".to_owned(),
        routine_id: routine_id.to_owned(),
        enabled: false,
        schedule_enabled: false,
        intent_enabled: false,
        failure_threshold: config.failure_threshold,
        consecutive_failures: 0,
        created_at_ns: now,
        updated_at_ns: now,
        armed_at_ns: None,
        armed_by: None,
        arm_note: None,
        disarmed_at_ns: None,
        disarmed_by: None,
        disarm_reason: None,
        plan_ref: None,
        last_schedule_fire_key: None,
        last_intent_fire_key: None,
        last_run_id: None,
        last_run_status: None,
        source_of_truth: ARMED_ROUTINE_SOURCE_OF_TRUTH.to_owned(),
    });
    record.record_version = ARMED_ROUTINE_RECORD_VERSION;
    record.row_kind = "armed_routine".to_owned();
    record.enabled = true;
    record.schedule_enabled = config.schedule_enabled;
    record.intent_enabled = config.intent_enabled;
    record.failure_threshold = config.failure_threshold;
    record.consecutive_failures = 0;
    record.updated_at_ns = now;
    record.armed_at_ns = Some(now);
    record.armed_by = Some(by_session.to_owned());
    record.arm_note = note;
    record.disarmed_at_ns = None;
    record.disarmed_by = None;
    record.disarm_reason = None;
    record.plan_ref = Some(automation.plan_ref);
    record.last_run_status = None;
    write_armed_routine_record(db, &record)?;
    read_armed_required(db, routine_id)
}

pub fn disarm_routine(
    db: &Arc<Db>,
    routine_id: &str,
    by_session: &str,
    reason: Option<String>,
) -> Result<ArmedRoutineRecord, ErrorData> {
    validate_routine_id_param("routine_update", routine_id)?;
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "routine_update action=disarm refused under disk pressure: cf_name={} pressure_level={:?}",
                cf::CF_KV,
                db.pressure_level()
            ),
        ));
    }
    let Some(mut record) = load_armed_routine_record(db, routine_id)? else {
        return Err(invalid(format!(
            "ARMED_ROUTINE_NOT_FOUND: routine_id {routine_id} is not armed"
        )));
    };
    let now = now_ts_ns();
    record.enabled = false;
    record.updated_at_ns = now;
    record.disarmed_at_ns = Some(now);
    record.disarmed_by = Some(by_session.to_owned());
    record.disarm_reason = reason;
    write_armed_routine_record(db, &record)?;
    read_armed_required(db, routine_id)
}

pub fn load_armed_routine_record(
    db: &Arc<Db>,
    routine_id: &str,
) -> Result<Option<ArmedRoutineRecord>, ErrorData> {
    validate_routine_id_param("routine_inspect", routine_id)?;
    let key = armed_routine_key(routine_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, key.as_bytes())
        .map_err(storage_error)?;
    match rows
        .into_iter()
        .find(|(row_key, _value)| row_key == key.as_bytes())
    {
        Some((_key, value)) => {
            decode_json::<ArmedRoutineRecord>(&value)
                .map(Some)
                .map_err(|error| {
                    mcp_error(
                        error_codes::STORAGE_CORRUPTED,
                        format!("ARMED_ROUTINE_ROW_DECODE_FAILED for {routine_id}: {error}"),
                    )
                })
        }
        None => Ok(None),
    }
}

pub fn due_armed_runs(
    db: &Arc<Db>,
    params: &ArmedRoutineTickParams,
) -> Result<(u64, u32, Vec<ArmedRoutineDueRun>, Vec<ArmedRoutineTickSkip>), ErrorData> {
    validate_tick_params(params)?;
    let now = params.now_ts_ns.unwrap_or_else(now_ts_ns);
    let mode = params.trigger_mode.unwrap_or_default();
    let all = load_all_armed_routines(db)?;
    let mut evaluated = 0_u32;
    let mut due = Vec::new();
    let mut skipped = Vec::new();

    let intent_candidates = if matches!(
        mode,
        ArmedRoutineTickTriggerMode::Intent | ArmedRoutineTickTriggerMode::Both
    ) {
        Some(
            current_intents(
                db,
                &IntentCurrentParams {
                    now_ts_ns: Some(now),
                    lookback_hours: params.lookback_hours,
                    min_confidence: Some(0.0),
                    max_candidates: Some(50),
                    include_agent_activity: false,
                },
            )?
            .candidates,
        )
    } else {
        None
    };

    for record in all {
        if params
            .routine_id
            .as_ref()
            .is_some_and(|routine_id| routine_id != &record.routine_id)
        {
            continue;
        }
        evaluated = evaluated.saturating_add(1);
        if !record.enabled {
            skipped.push(skip(&record.routine_id, "armed_record_disabled"));
            continue;
        }
        let Some(routine) = load_routine_record(db.as_ref(), &record.routine_id)? else {
            skipped.push(skip(&record.routine_id, "routine_not_mined"));
            continue;
        };
        if let Some(state) = load_state_row(db.as_ref(), &record.routine_id)?
            && matches!(
                state.lifecycle,
                RoutineLifecycle::Disabled | RoutineLifecycle::Archived
            )
        {
            skipped.push(skip(&record.routine_id, "routine_lifecycle_disabled"));
            continue;
        }
        let Some(automation) = load_routine_automation_record(db, &record.routine_id)? else {
            skipped.push(skip(&record.routine_id, "automation_not_installed"));
            continue;
        };
        if automation.state != "installed" {
            skipped.push(skip(&record.routine_id, "automation_not_installed"));
            continue;
        }

        if matches!(
            mode,
            ArmedRoutineTickTriggerMode::Schedule | ArmedRoutineTickTriggerMode::Both
        ) && record.schedule_enabled
            && let Some(schedule_due) = schedule_due_run(&record, &routine, now)?
        {
            due.push(schedule_due);
            continue;
        }

        if matches!(
            mode,
            ArmedRoutineTickTriggerMode::Intent | ArmedRoutineTickTriggerMode::Both
        ) && record.intent_enabled
            && let Some(candidates) = &intent_candidates
            && let Some(candidate) = candidates
                .iter()
                .find(|candidate| candidate.routine_id == record.routine_id)
        {
            let matched_prefix_len =
                u32::try_from(candidate.matched_prefix_len).unwrap_or(u32::MAX);
            let trigger_key = format!(
                "intent:{}:{}:{}",
                record.routine_id, matched_prefix_len, candidate.last_matched_end_ts_ns
            );
            if record.last_intent_fire_key.as_deref() == Some(trigger_key.as_str()) {
                skipped.push(skip(&record.routine_id, "intent_already_fired"));
                continue;
            }
            due.push(ArmedRoutineDueRun {
                routine_id: record.routine_id.clone(),
                trigger_kind: ArmedRoutineTriggerKind::Intent,
                trigger_key,
                due_ts_ns: now,
                plan_ref: Some(automation.plan_ref),
                intent: Some(ArmedRoutineIntentEvidence {
                    confidence: candidate.confidence,
                    matched_prefix_len,
                    total_steps: u32::try_from(candidate.total_steps).unwrap_or(u32::MAX),
                    last_matched_end_ts_ns: candidate.last_matched_end_ts_ns,
                }),
            });
        }
    }
    Ok((now, evaluated, due, skipped))
}

pub fn claim_armed_run(
    db: &Arc<Db>,
    due: &ArmedRoutineDueRun,
    now: u64,
) -> Result<ArmedRoutineRunRecord, ErrorData> {
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "armed_routine_tick refused under disk pressure: cf_name={} pressure_level={:?}",
                cf::CF_KV,
                db.pressure_level()
            ),
        ));
    }
    let Some(mut armed) = load_armed_routine_record(db, &due.routine_id)? else {
        return Err(invalid(format!(
            "ARMED_ROUTINE_NOT_FOUND: routine_id {} is no longer armed",
            due.routine_id
        )));
    };
    if !armed.enabled {
        return Err(invalid(format!(
            "ARMED_ROUTINE_DISABLED: routine_id {} is no longer enabled",
            due.routine_id
        )));
    }
    match due.trigger_kind {
        ArmedRoutineTriggerKind::Schedule => {
            if armed.last_schedule_fire_key.as_deref() == Some(due.trigger_key.as_str()) {
                return Err(invalid(format!(
                    "ARMED_ROUTINE_DUPLICATE_TRIGGER: {} already claimed",
                    due.trigger_key
                )));
            }
            armed.last_schedule_fire_key = Some(due.trigger_key.clone());
        }
        ArmedRoutineTriggerKind::Intent => {
            if armed.last_intent_fire_key.as_deref() == Some(due.trigger_key.as_str()) {
                return Err(invalid(format!(
                    "ARMED_ROUTINE_DUPLICATE_TRIGGER: {} already claimed",
                    due.trigger_key
                )));
            }
            armed.last_intent_fire_key = Some(due.trigger_key.clone());
        }
    }
    let run_id = armed_run_id(&due.routine_id, due.trigger_kind, now);
    armed.updated_at_ns = now;
    armed.last_run_id = Some(run_id.clone());
    armed.last_run_status = Some(ArmedRoutineRunStatus::Started);
    let run = ArmedRoutineRunRecord {
        record_version: ARMED_ROUTINE_RUN_RECORD_VERSION,
        row_kind: "armed_routine_run".to_owned(),
        run_id,
        routine_id: due.routine_id.clone(),
        trigger_kind: due.trigger_kind,
        trigger_key: due.trigger_key.clone(),
        started_ts_ns: now,
        completed_ts_ns: now,
        dry_run: false,
        status: ArmedRoutineRunStatus::Started,
        plan_ref: due.plan_ref.clone(),
        execution_id: None,
        approval_id: None,
        failure_count_after: armed.consecutive_failures,
        disarmed_after_failure: false,
        intent: due.intent.clone(),
        error_code: None,
        error: None,
        evidence: json!({ "claimed": true }),
        source_of_truth: ARMED_ROUTINE_SOURCE_OF_TRUTH.to_owned(),
    };
    write_armed_and_run_records(db, &armed, &run)?;
    Ok(run)
}

#[allow(clippy::too_many_arguments)]
pub fn complete_armed_run(
    db: &Arc<Db>,
    mut run: ArmedRoutineRunRecord,
    status: ArmedRoutineRunStatus,
    plan_ref: Option<String>,
    execution_id: Option<String>,
    approval_id: Option<String>,
    error_code: Option<String>,
    error: Option<String>,
    evidence: Value,
) -> Result<ArmedRoutineRunRecord, ErrorData> {
    let Some(mut armed) = load_armed_routine_record(db, &run.routine_id)? else {
        return Err(invalid(format!(
            "ARMED_ROUTINE_NOT_FOUND: routine_id {} vanished before run completion",
            run.routine_id
        )));
    };
    let now = now_ts_ns();
    if status.is_failure() {
        armed.consecutive_failures = armed.consecutive_failures.saturating_add(1);
    } else if matches!(status, ArmedRoutineRunStatus::Succeeded) {
        armed.consecutive_failures = 0;
    }
    let disarmed_after_failure =
        status.is_failure() && armed.consecutive_failures >= armed.failure_threshold;
    if disarmed_after_failure {
        armed.enabled = false;
        armed.disarmed_at_ns = Some(now);
        armed.disarmed_by = Some("armed-routine-runner".to_owned());
        armed.disarm_reason = Some(format!(
            "self-disarmed after {} consecutive failures",
            armed.consecutive_failures
        ));
    }
    armed.updated_at_ns = now;
    armed.last_run_id = Some(run.run_id.clone());
    armed.last_run_status = Some(status);

    run.completed_ts_ns = now;
    run.status = status;
    run.plan_ref = plan_ref.or(run.plan_ref);
    run.execution_id = execution_id;
    run.approval_id = approval_id;
    run.failure_count_after = armed.consecutive_failures;
    run.disarmed_after_failure = disarmed_after_failure;
    run.error_code = error_code;
    run.error = error;
    run.evidence = evidence;
    write_armed_and_run_records(db, &armed, &run)?;
    Ok(run)
}

pub fn dry_run_tick_run(due: &ArmedRoutineDueRun) -> ArmedRoutineTickRun {
    ArmedRoutineTickRun {
        routine_id: due.routine_id.clone(),
        trigger_kind: due.trigger_kind,
        trigger_key: due.trigger_key.clone(),
        status: ArmedRoutineRunStatus::DryRun,
        run_id: None,
        execution_id: None,
        approval_id: None,
        failure_count_after: 0,
        disarmed_after_failure: false,
        error_code: None,
        error: None,
    }
}

pub fn tick_run_from_record(run: &ArmedRoutineRunRecord) -> ArmedRoutineTickRun {
    ArmedRoutineTickRun {
        routine_id: run.routine_id.clone(),
        trigger_kind: run.trigger_kind,
        trigger_key: run.trigger_key.clone(),
        status: run.status,
        run_id: Some(run.run_id.clone()),
        execution_id: run.execution_id.clone(),
        approval_id: run.approval_id.clone(),
        failure_count_after: run.failure_count_after,
        disarmed_after_failure: run.disarmed_after_failure,
        error_code: run.error_code.clone(),
        error: run.error.clone(),
    }
}

fn schedule_due_run(
    record: &ArmedRoutineRecord,
    routine: &RoutineRecord,
    now: u64,
) -> Result<Option<ArmedRoutineDueRun>, ErrorData> {
    let day_start = local_day_start(now)?;
    let minute_of_day =
        u32::try_from(now.saturating_sub(day_start) / 60_000_000_000).unwrap_or(0) % 1440;
    let weekday = weekday_for_ts(now)?;
    if !dow_matches(&routine.dow_class, weekday) {
        return Ok(None);
    }
    let tolerance = routine
        .tolerance_minutes
        .max(MIN_SCHEDULE_WINDOW_MINUTES)
        .min(720);
    if circular_minute_distance(minute_of_day, routine.mean_minute_of_day % 1440) > tolerance {
        return Ok(None);
    }
    let trigger_key = format!("schedule:{}:{day_start}", record.routine_id);
    if record.last_schedule_fire_key.as_deref() == Some(trigger_key.as_str()) {
        return Ok(None);
    }
    Ok(Some(ArmedRoutineDueRun {
        routine_id: record.routine_id.clone(),
        trigger_kind: ArmedRoutineTriggerKind::Schedule,
        trigger_key,
        due_ts_ns: now,
        plan_ref: record.plan_ref.clone(),
        intent: None,
    }))
}

fn dow_matches(dow: &RoutineDowClass, weekday: u8) -> bool {
    match dow {
        RoutineDowClass::Daily => true,
        RoutineDowClass::Weekdays => weekday <= 4,
        RoutineDowClass::Weekend => weekday >= 5,
        RoutineDowClass::Days { days } => days.contains(&weekday),
    }
}

fn weekday_for_ts(ts_ns: u64) -> Result<u8, ErrorData> {
    let ts = i64::try_from(ts_ns)
        .map_err(|_e| invalid(format!("now_ts_ns {ts_ns} exceeds the representable range")))?;
    let weekday = Local.timestamp_nanos(ts).weekday().num_days_from_monday();
    u8::try_from(weekday).map_err(|_e| internal("weekday outside 0..=6"))
}

fn circular_minute_distance(a: u32, b: u32) -> u32 {
    let raw = a.abs_diff(b);
    raw.min(1440 - raw)
}

fn load_all_armed_routines(db: &Arc<Db>) -> Result<Vec<ArmedRoutineRecord>, ErrorData> {
    let mut out = Vec::new();
    let mut scanned = 0_usize;
    let mut start = ARMED_ROUTINE_PREFIX.as_bytes().to_vec();
    loop {
        if scanned >= MAX_SCAN_ROWS {
            return Err(internal(format!(
                "ARMED_ROUTINE_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS} CF_KV rows"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
            .map_err(storage_error)?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            if !key.starts_with(ARMED_ROUTINE_PREFIX.as_bytes()) {
                return Ok(out);
            }
            scanned = scanned.saturating_add(1);
            let record = decode_json::<ArmedRoutineRecord>(value).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "ARMED_ROUTINE_ROW_DECODE_FAILED at {}: {error}",
                        hex_encode(key)
                    ),
                )
            })?;
            out.push(record);
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(out)
}

fn validate_tick_params(params: &ArmedRoutineTickParams) -> Result<(), ErrorData> {
    if let Some(routine_id) = &params.routine_id {
        validate_routine_id_param("armed_routine_tick", routine_id)?;
    }
    if let Some(timeout_ms) = params.launch_timeout_ms
        && timeout_ms == 0
    {
        return Err(invalid("armed_routine_tick launch_timeout_ms must be >= 1"));
    }
    Ok(())
}

fn read_armed_required(db: &Arc<Db>, routine_id: &str) -> Result<ArmedRoutineRecord, ErrorData> {
    load_armed_routine_record(db, routine_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("ARMED_ROUTINE_READBACK_MISSING for {routine_id}"),
        )
    })
}

fn write_armed_routine_record(db: &Arc<Db>, record: &ArmedRoutineRecord) -> Result<(), ErrorData> {
    let key = armed_routine_key(&record.routine_id);
    let value = encode_json(record).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode armed routine row for {}: {error}",
                record.routine_id
            ),
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(key.into_bytes(), value)])
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to persist armed routine row for {}: {error}",
                    record.routine_id
                ),
            )
        })?;
    let readback = read_armed_required(db, &record.routine_id)?;
    if readback != *record {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_READBACK_MISMATCH for {}: persisted row != value just written",
                record.routine_id
            ),
        ));
    }
    Ok(())
}

fn write_armed_and_run_records(
    db: &Arc<Db>,
    armed: &ArmedRoutineRecord,
    run: &ArmedRoutineRunRecord,
) -> Result<(), ErrorData> {
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "armed routine write refused under disk pressure: pressure_level={:?}",
                db.pressure_level()
            ),
        ));
    }
    let armed_key = armed_routine_key(&armed.routine_id).into_bytes();
    let run_key = armed_run_key(&run.run_id).into_bytes();
    let armed_value = encode_json(armed).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("failed to encode armed routine row: {error}"),
        )
    })?;
    let run_value = encode_json(run).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("failed to encode armed routine run row: {error}"),
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(armed_key, armed_value), (run_key, run_value)])
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to persist armed routine run {}: {error}",
                    run.run_id
                ),
            )
        })?;
    let armed_readback = read_armed_required(db, &armed.routine_id)?;
    if armed_readback != *armed {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("ARMED_ROUTINE_READBACK_MISMATCH for {}", armed.routine_id),
        ));
    }
    let run_readback = load_armed_run(db, &run.run_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("ARMED_ROUTINE_RUN_READBACK_MISSING for {}", run.run_id),
        )
    })?;
    if run_readback != *run {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("ARMED_ROUTINE_RUN_READBACK_MISMATCH for {}", run.run_id),
        ));
    }
    Ok(())
}

fn load_armed_run(db: &Arc<Db>, run_id: &str) -> Result<Option<ArmedRoutineRunRecord>, ErrorData> {
    let key = armed_run_key(run_id);
    let rows = db
        .scan_cf_prefix(cf::CF_KV, key.as_bytes())
        .map_err(storage_error)?;
    match rows
        .into_iter()
        .find(|(row_key, _value)| row_key == key.as_bytes())
    {
        Some((_key, value)) => decode_json::<ArmedRoutineRunRecord>(&value)
            .map(Some)
            .map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("ARMED_ROUTINE_RUN_ROW_DECODE_FAILED for {run_id}: {error}"),
                )
            }),
        None => Ok(None),
    }
}

fn armed_routine_key(routine_id: &str) -> String {
    format!("{ARMED_ROUTINE_PREFIX}{routine_id}")
}

fn armed_run_key(run_id: &str) -> String {
    format!("{ARMED_ROUTINE_RUN_PREFIX}{run_id}")
}

fn armed_run_id(routine_id: &str, trigger: ArmedRoutineTriggerKind, started_ts_ns: u64) -> String {
    format!("arr1-{routine_id}-{}-{started_ts_ns:020}", trigger.as_str())
}

fn skip(routine_id: &str, reason: &str) -> ArmedRoutineTickSkip {
    ArmedRoutineTickSkip {
        routine_id: routine_id.to_owned(),
        reason: reason.to_owned(),
    }
}

fn storage_error(error: impl std::fmt::Display) -> ErrorData {
    mcp_error(
        error_codes::STORAGE_READ_FAILED,
        format!("armed routine storage failure: {error}"),
    )
}

fn invalid(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail.into())
}

fn internal(detail: impl Into<String>) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_core::SCHEMA_VERSION;
    use synapse_core::types::{
        RoutineGranularity, RoutineRecord, RoutineStateAction, RoutineStateRecord, RoutineStep,
        RoutineTransition,
    };
    use synapse_storage::routines as routine_codec;

    use crate::m3::profile_authoring::RoutineAutomationRecord;

    fn temp_db() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(dir.path(), SCHEMA_VERSION).expect("open db"));
        (dir, db)
    }

    fn routine(now: u64) -> RoutineRecord {
        RoutineRecord {
            record_version: 1,
            ts_ns: now,
            routine_id: "rt1-0123456789abcdef".to_owned(),
            granularity: RoutineGranularity::App,
            steps: vec![RoutineStep {
                app: "notepad.exe".to_owned(),
                document: None,
            }],
            dow_class: RoutineDowClass::Daily,
            mean_minute_of_day: minute_of_day(now),
            tolerance_minutes: 0,
            schedule_label: "daily".to_owned(),
            support_days: 3,
            occurrence_count: 3,
            opportunity_days: 3,
            confidence: 0.8,
            window_start_ns: 0,
            window_end_ns: now,
            active_days_in_window: 3,
            first_seen_day_start_ns: local_day_start(now).expect("day"),
            last_seen_day_start_ns: local_day_start(now).expect("day"),
            evidence: Vec::new(),
        }
    }

    fn minute_of_day(ts_ns: u64) -> u32 {
        let day = local_day_start(ts_ns).expect("day");
        u32::try_from(ts_ns.saturating_sub(day) / 60_000_000_000).unwrap()
    }

    fn write_routine(db: &Arc<Db>, routine: &RoutineRecord) {
        let key = routine_codec::routine_key(&routine.routine_id).expect("key");
        let value = encode_json(routine).expect("encode routine");
        db.put_batch_pressure_bypass(cf::CF_ROUTINES, [(key, value)])
            .expect("write routine");
    }

    fn write_state(db: &Arc<Db>, routine_id: &str, lifecycle: RoutineLifecycle) {
        let now = now_ts_ns();
        let state = RoutineStateRecord {
            record_version: 2,
            routine_id: routine_id.to_owned(),
            lifecycle,
            label: None,
            created_ts_ns: now,
            updated_ts_ns: now,
            last_mined_ts_ns: Some(now),
            present_in_last_mine: true,
            transitions: vec![RoutineTransition {
                ts_ns: now,
                action: RoutineStateAction::Discovered,
                from: None,
                to: lifecycle,
                by: "test".to_owned(),
                label_before: None,
                label_after: None,
                note: None,
            }],
            transitions_truncated: 0,
            confidence_history: Vec::new(),
            confidence_history_truncated: 0,
            feedback_events: Vec::new(),
            feedback_events_truncated: 0,
            accept_count: 0,
            decline_count: 0,
            ignore_count: 0,
            abandon_count: 0,
            consecutive_declines: 0,
            cooldown_level: 0,
            cooldown_until_ts_ns: None,
        };
        let key = routine_codec::routine_state_key(routine_id).expect("state key");
        let value = encode_json(&state).expect("encode state");
        db.put_batch_pressure_bypass(cf::CF_ROUTINE_STATE, [(key, value)])
            .expect("write state");
    }

    fn write_automation(db: &Arc<Db>, routine_id: &str) {
        let record = RoutineAutomationRecord {
            schema_version: 1,
            row_kind: "routine_automation".to_owned(),
            routine_id: routine_id.to_owned(),
            profile_id: "profile.test".to_owned(),
            candidate_id: "routine-auto.test".to_owned(),
            candidate_row_key: "profile_authoring_candidate/v1/routine-auto.test".to_owned(),
            plan_ref: format!("plan/v1/{routine_id}"),
            state: "installed".to_owned(),
            generated_at_ns: 1,
            updated_at_ns: 2,
            installed_at_ns: Some(2),
            rejected_at_ns: None,
            plan_fully_deterministic: true,
            total_steps: 1,
            deterministic_steps: 1,
            agent_task_steps: 0,
        };
        db.put_batch_pressure_bypass(
            cf::CF_KV,
            [(
                format!("routine_automation/v1/{routine_id}").into_bytes(),
                encode_json(&record).expect("automation"),
            )],
        )
        .expect("write automation");
    }

    #[test]
    fn arm_requires_installed_automation_and_reads_back() {
        let (_dir, db) = temp_db();
        let now = now_ts_ns();
        let routine = routine(now);
        write_routine(&db, &routine);
        write_state(&db, &routine.routine_id, RoutineLifecycle::Confirmed);

        let missing = arm_routine(
            &db,
            &routine.routine_id,
            ArmRoutineConfig::from_optional(None, None, None),
            "test-session",
            None,
        )
        .expect_err("automation missing");
        assert!(missing.message.contains("ROUTINE_AUTOMATION_NOT_INSTALLED"));

        write_automation(&db, &routine.routine_id);
        let armed = arm_routine(
            &db,
            &routine.routine_id,
            ArmRoutineConfig::from_optional(Some(true), Some(false), Some(2)),
            "test-session",
            Some("operator armed".to_owned()),
        )
        .expect("arm");
        println!(
            "readback=armed_routine routine_id={} enabled={} threshold={}",
            armed.routine_id, armed.enabled, armed.failure_threshold
        );
        assert!(armed.enabled);
        assert!(armed.schedule_enabled);
        assert!(!armed.intent_enabled);
        assert_eq!(armed.failure_threshold, 2);
        assert_eq!(
            load_armed_routine_record(&db, &routine.routine_id)
                .expect("load")
                .expect("row"),
            armed
        );
    }

    #[test]
    fn schedule_due_claims_once_and_failure_threshold_disarms() {
        let (_dir, db) = temp_db();
        let now = now_ts_ns();
        let routine = routine(now);
        write_routine(&db, &routine);
        write_state(&db, &routine.routine_id, RoutineLifecycle::Confirmed);
        write_automation(&db, &routine.routine_id);
        let armed = arm_routine(
            &db,
            &routine.routine_id,
            ArmRoutineConfig::from_optional(Some(true), Some(false), Some(1)),
            "test-session",
            None,
        )
        .expect("arm");
        assert!(armed.enabled);

        let params = ArmedRoutineTickParams {
            now_ts_ns: Some(now),
            trigger_mode: Some(ArmedRoutineTickTriggerMode::Schedule),
            ..ArmedRoutineTickParams::default()
        };
        let (_now, evaluated, due, skipped) = due_armed_runs(&db, &params).expect("due");
        assert_eq!(evaluated, 1);
        assert!(skipped.is_empty());
        assert_eq!(due.len(), 1);
        let started = claim_armed_run(&db, &due[0], now).expect("claim");
        let duplicate = due_armed_runs(&db, &params).expect("due after claim").2;
        assert!(duplicate.is_empty());

        let completed = complete_armed_run(
            &db,
            started,
            ArmedRoutineRunStatus::Failed,
            None,
            Some("px1-test".to_owned()),
            None,
            Some("TEST_FAILURE".to_owned()),
            Some("failed".to_owned()),
            json!({ "test": true }),
        )
        .expect("complete");
        assert!(completed.disarmed_after_failure);
        let armed = load_armed_routine_record(&db, &routine.routine_id)
            .expect("load")
            .expect("armed");
        assert!(!armed.enabled);
        assert_eq!(armed.consecutive_failures, 1);
    }
}
