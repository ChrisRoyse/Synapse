//! Armed routine execution state (#862).
//!
//! `routine_update action=arm|disarm` owns the durable arming row in `CF_KV`.
//! The server-side `armed_routine_tick` tool and periodic daemon job use these
//! helpers to select due runs, claim a trigger before execution, and record the
//! final outcome. Durable trigger keys are written before execution so daemon
//! restarts do not double-fire the same schedule window or intent evidence.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{Datelike, Local, TimeZone};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::error_codes;
use synapse_core::intent::IntentCandidate;
use synapse_core::types::{RoutineDowClass, RoutineLifecycle, RoutineRecord};
use synapse_storage::{Db, cf, decode_json, encode_json, routines as routine_codec};

use crate::m1::mcp_error;

use super::episodes::{hex_encode, key_after, local_day_start, next_local_day_start, now_ts_ns};
use super::intent::{IntentCurrentParams, current_intents};
use super::permissions::{Permission, RequiredPermissions, required};
use super::profile_authoring::load_routine_automation_record;
use super::routines::{load_routine_record, load_state_row, validate_routine_id_param};

const ARMED_ROUTINE_PREFIX: &str = "armed_routine/v1/";
const ARMED_ROUTINE_RUN_PREFIX: &str = "armed_routine_run/v1/";
const ARMED_ROUTINE_SCHEDULE_DUE_PREFIX: &str = "armed_routine_due/v1/schedule/";
const ARMED_ROUTINE_SCHEDULE_DUE_BY_ID_PREFIX: &str = "armed_routine_due_by_id/v1/schedule/";
const ARMED_ROUTINE_RECORD_VERSION: u32 = 1;
const ARMED_ROUTINE_RUN_RECORD_VERSION: u32 = 1;
const ARMED_ROUTINE_SCHEDULE_DUE_INDEX_RECORD_VERSION: u32 = 1;
const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
const MAX_FAILURE_THRESHOLD: u32 = 20;
const MIN_SCHEDULE_WINDOW_MINUTES: u32 = 5;
const MAX_SCAN_ROWS: usize = 200_000;
const SCAN_CHUNK_ROWS: usize = 4_096;

pub const ARMED_ROUTINE_SOURCE_OF_TRUTH: &str = "CF_KV armed_routine/v1, armed_routine_due/v1/schedule, armed_routine_due_by_id/v1/schedule, and armed_routine_run/v1 rows plus CF_ROUTINES/CF_ROUTINE_STATE joins and plan_execution/v1 rows";

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmedRoutineScheduleDueIndexRecord {
    record_version: u32,
    row_kind: String,
    routine_id: String,
    due_ts_ns: u64,
    primary_key_hex: String,
    primary_value_sha256: String,
    routine_key_hex: String,
    routine_value_sha256: String,
}

#[derive(Clone, Debug)]
struct RoutineSourceRow {
    record: RoutineRecord,
    key: Vec<u8>,
    value: Vec<u8>,
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
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub browser_window_hwnd: Option<i64>,
    /// Timeout applied to launch-window/postcondition waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    let mut evaluated = 0_u32;
    let mut due = Vec::new();
    let mut skipped = Vec::new();
    let mut evaluated_routine_ids = BTreeSet::new();

    let intent_candidates = intent_candidates_for_tick(db, params, now, mode)?;

    if let Some(routine_id) = &params.routine_id {
        let Some(record) = load_armed_routine_record(db, routine_id)? else {
            return Ok((now, evaluated, due, skipped));
        };
        evaluated = evaluated.saturating_add(1);
        evaluated_routine_ids.insert(record.routine_id.clone());
        evaluate_armed_record_due(
            db,
            &record,
            now,
            mode,
            intent_candidates.as_deref(),
            &mut due,
            &mut skipped,
        )?;
        return Ok((now, evaluated, due, skipped));
    }

    if matches!(
        mode,
        ArmedRoutineTickTriggerMode::Schedule | ArmedRoutineTickTriggerMode::Both
    ) {
        for index in load_due_schedule_indexes(db, now)? {
            let (record, primary_value) = load_armed_primary_from_schedule_index(db, &index)?;
            evaluated = evaluated.saturating_add(1);
            evaluated_routine_ids.insert(record.routine_id.clone());
            let due_before = due.len();
            let skipped_before = skipped.len();
            evaluate_armed_record_due(
                db,
                &record,
                now,
                ArmedRoutineTickTriggerMode::Schedule,
                None,
                &mut due,
                &mut skipped,
            )?;
            if due.len() == due_before && skipped.len() > skipped_before {
                refresh_schedule_due_index(db, &record, &primary_value, now)?;
            }
        }
    }

    if let Some(candidates) = intent_candidates.as_deref() {
        for candidate in candidates {
            if evaluated_routine_ids.contains(&candidate.routine_id) {
                continue;
            }
            let Some(record) = load_armed_routine_record(db, &candidate.routine_id)? else {
                continue;
            };
            evaluated = evaluated.saturating_add(1);
            evaluated_routine_ids.insert(record.routine_id.clone());
            evaluate_armed_record_due(
                db,
                &record,
                now,
                ArmedRoutineTickTriggerMode::Intent,
                Some(candidates),
                &mut due,
                &mut skipped,
            )?;
        }
    }
    Ok((now, evaluated, due, skipped))
}

fn intent_candidates_for_tick(
    db: &Arc<Db>,
    params: &ArmedRoutineTickParams,
    now: u64,
    mode: ArmedRoutineTickTriggerMode,
) -> Result<Option<Vec<IntentCandidate>>, ErrorData> {
    if !matches!(
        mode,
        ArmedRoutineTickTriggerMode::Intent | ArmedRoutineTickTriggerMode::Both
    ) {
        return Ok(None);
    }
    current_intents(
        db,
        &IntentCurrentParams {
            now_ts_ns: Some(now),
            lookback_hours: params.lookback_hours,
            min_confidence: Some(0.0),
            max_candidates: Some(50),
            include_agent_activity: false,
        },
    )
    .map(|response| Some(response.candidates))
}

fn evaluate_armed_record_due(
    db: &Arc<Db>,
    record: &ArmedRoutineRecord,
    now: u64,
    mode: ArmedRoutineTickTriggerMode,
    intent_candidates: Option<&[IntentCandidate]>,
    due: &mut Vec<ArmedRoutineDueRun>,
    skipped: &mut Vec<ArmedRoutineTickSkip>,
) -> Result<(), ErrorData> {
    if !record.enabled {
        skipped.push(skip(&record.routine_id, "armed_record_disabled"));
        return Ok(());
    }
    let Some(routine) = load_routine_record(db.as_ref(), &record.routine_id)? else {
        skipped.push(skip(&record.routine_id, "routine_not_mined"));
        return Ok(());
    };
    if let Some(state) = load_state_row(db.as_ref(), &record.routine_id)?
        && matches!(
            state.lifecycle,
            RoutineLifecycle::Disabled | RoutineLifecycle::Archived
        )
    {
        skipped.push(skip(&record.routine_id, "routine_lifecycle_disabled"));
        return Ok(());
    }
    let Some(automation) = load_routine_automation_record(db, &record.routine_id)? else {
        skipped.push(skip(&record.routine_id, "automation_not_installed"));
        return Ok(());
    };
    if automation.state != "installed" {
        skipped.push(skip(&record.routine_id, "automation_not_installed"));
        return Ok(());
    }

    if matches!(
        mode,
        ArmedRoutineTickTriggerMode::Schedule | ArmedRoutineTickTriggerMode::Both
    ) && record.schedule_enabled
        && let Some(schedule_due) = schedule_due_run(record, &routine, now)?
    {
        due.push(schedule_due);
        return Ok(());
    }

    if matches!(
        mode,
        ArmedRoutineTickTriggerMode::Intent | ArmedRoutineTickTriggerMode::Both
    ) && record.intent_enabled
        && let Some(candidates) = intent_candidates
        && let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.routine_id == record.routine_id)
    {
        let matched_prefix_len = u32::try_from(candidate.matched_prefix_len).unwrap_or(u32::MAX);
        let trigger_key = format!(
            "intent:{}:{}:{}",
            record.routine_id, matched_prefix_len, candidate.last_matched_end_ts_ns
        );
        if record.last_intent_fire_key.as_deref() == Some(trigger_key.as_str()) {
            skipped.push(skip(&record.routine_id, "intent_already_fired"));
            return Ok(());
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
    Ok(())
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

fn schedule_window_segments(mean_minute_of_day: u32, tolerance_minutes: u32) -> Vec<(u32, u32)> {
    let mean = mean_minute_of_day % 1440;
    let tolerance = tolerance_minutes.max(MIN_SCHEDULE_WINDOW_MINUTES).min(720);
    if tolerance >= 720 {
        return vec![(0, 1439)];
    }
    let start = i64::from(mean) - i64::from(tolerance);
    let end = i64::from(mean) + i64::from(tolerance);
    if start < 0 {
        vec![
            (0, u32::try_from(end).unwrap_or(1439)),
            (u32::try_from(1440 + start).unwrap_or(0), 1439),
        ]
    } else if end >= 1440 {
        vec![
            (0, u32::try_from(end - 1440).unwrap_or(0)),
            (u32::try_from(start).unwrap_or(0), 1439),
        ]
    } else {
        vec![(
            u32::try_from(start).unwrap_or(0),
            u32::try_from(end).unwrap_or(1439),
        )]
    }
}

fn next_schedule_due_ts(
    record: &ArmedRoutineRecord,
    routine: &RoutineRecord,
    anchor_ts_ns: u64,
) -> Result<Option<u64>, ErrorData> {
    if !record.enabled || !record.schedule_enabled {
        return Ok(None);
    }
    const NANOS_PER_MINUTE: u64 = 60_000_000_000;
    let anchor_day_start = local_day_start(anchor_ts_ns)?;
    let anchor_minute =
        u32::try_from(anchor_ts_ns.saturating_sub(anchor_day_start) / NANOS_PER_MINUTE)
            .unwrap_or(0)
            .min(1439);
    let mut day_start = anchor_day_start;
    for day_offset in 0..14 {
        let weekday = weekday_for_ts(day_start)?;
        let trigger_key = format!("schedule:{}:{day_start}", record.routine_id);
        if dow_matches(&routine.dow_class, weekday)
            && record.last_schedule_fire_key.as_deref() != Some(trigger_key.as_str())
        {
            let segments =
                schedule_window_segments(routine.mean_minute_of_day, routine.tolerance_minutes);
            for (segment_start, segment_end) in segments {
                if day_offset == 0 && anchor_minute > segment_end {
                    continue;
                }
                let due_minute = segment_start;
                let due_ts = day_start.saturating_add(u64::from(due_minute) * NANOS_PER_MINUTE);
                return Ok(Some(due_ts));
            }
        }
        day_start = next_local_day_start(day_start)?;
    }
    Ok(None)
}

fn load_exact_cf_value(
    db: &Db,
    cf_name: &str,
    key: &[u8],
    context: &'static str,
) -> Result<Option<Vec<u8>>, ErrorData> {
    let rows = db.scan_cf_prefix(cf_name, key).map_err(storage_error)?;
    let mut exact_values = rows
        .into_iter()
        .filter_map(|(row_key, value)| (row_key == key).then_some(value))
        .collect::<Vec<_>>();
    if exact_values.len() > 1 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_EXACT_KEY_DUPLICATE: {context} key {} appeared more than once in {cf_name}",
                String::from_utf8_lossy(key)
            ),
        ));
    }
    Ok(exact_values.pop())
}

fn load_routine_record_with_raw(
    db: &Db,
    routine_id: &str,
) -> Result<Option<RoutineSourceRow>, ErrorData> {
    let key = routine_codec::routine_key(routine_id).map_err(|error| invalid(error.to_string()))?;
    let Some(value) = load_exact_cf_value(db, cf::CF_ROUTINES, &key, "routine primary row")? else {
        return Ok(None);
    };
    let record: RoutineRecord = decode_json(&value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!("ROUTINE_ROW_DECODE_FAILED in CF_ROUTINES for {routine_id}: {error}"),
        )
    })?;
    if record.routine_id != routine_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ROUTINE_ID_MISMATCH in CF_ROUTINES: row key {routine_id} holds routine_id {}",
                record.routine_id
            ),
        ));
    }
    Ok(Some(RoutineSourceRow { record, key, value }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest[..])
}

fn routine_id_hex(routine_id: &str) -> String {
    hex_encode(routine_id.as_bytes())
}

fn schedule_due_index_key(routine_id: &str, due_ts_ns: u64) -> Vec<u8> {
    format!(
        "{ARMED_ROUTINE_SCHEDULE_DUE_PREFIX}{due_ts_ns:020}/{}",
        routine_id_hex(routine_id)
    )
    .into_bytes()
}

fn schedule_due_by_id_index_key(routine_id: &str) -> Vec<u8> {
    format!(
        "{ARMED_ROUTINE_SCHEDULE_DUE_BY_ID_PREFIX}{}",
        routine_id_hex(routine_id)
    )
    .into_bytes()
}

fn decode_schedule_due_index(
    index_key: &[u8],
    value: &[u8],
    context: &'static str,
) -> Result<ArmedRoutineScheduleDueIndexRecord, ErrorData> {
    let index: ArmedRoutineScheduleDueIndexRecord = decode_json(value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_DECODE_FAILED during {context} at {}: {error}",
                String::from_utf8_lossy(index_key)
            ),
        )
    })?;
    if index.record_version != ARMED_ROUTINE_SCHEDULE_DUE_INDEX_RECORD_VERSION {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_VERSION_UNSUPPORTED for {}: expected {}, got {}",
                index.routine_id,
                ARMED_ROUTINE_SCHEDULE_DUE_INDEX_RECORD_VERSION,
                index.record_version
            ),
        ));
    }
    if index.row_kind != "armed_routine_schedule_due_index" {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_ROW_KIND_INVALID for {}: {}",
                index.routine_id, index.row_kind
            ),
        ));
    }
    Ok(index)
}

fn load_schedule_due_by_id_index(
    db: &Arc<Db>,
    routine_id: &str,
) -> Result<Option<ArmedRoutineScheduleDueIndexRecord>, ErrorData> {
    let key = schedule_due_by_id_index_key(routine_id);
    let Some(value) = load_exact_cf_value(db, cf::CF_KV, &key, "armed schedule due by-id index")?
    else {
        return Ok(None);
    };
    let index = decode_schedule_due_index(&key, &value, "by-id load")?;
    if index.routine_id != routine_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_BY_ID_KEY_MISMATCH: key for {routine_id} contains {}",
                index.routine_id
            ),
        ));
    }
    Ok(Some(index))
}

fn validate_schedule_due_index_key(
    key: &[u8],
    index: &ArmedRoutineScheduleDueIndexRecord,
    context: &'static str,
) -> Result<(), ErrorData> {
    let expected = schedule_due_index_key(&index.routine_id, index.due_ts_ns);
    if key != expected {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_KEY_MISMATCH during {context}: key={}, expected={}",
                String::from_utf8_lossy(key),
                String::from_utf8_lossy(&expected)
            ),
        ));
    }
    Ok(())
}

fn validate_schedule_due_by_id_backlink(
    db: &Arc<Db>,
    index: &ArmedRoutineScheduleDueIndexRecord,
) -> Result<(), ErrorData> {
    let Some(by_id) = load_schedule_due_by_id_index(db, &index.routine_id)? else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_BY_ID_INDEX_MISSING: due index for {} has no by-id backlink",
                index.routine_id
            ),
        ));
    };
    if by_id != *index {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_BY_ID_INDEX_MISMATCH for {}: due row and by-id row disagree",
                index.routine_id
            ),
        ));
    }
    Ok(())
}

fn load_due_schedule_indexes(
    db: &Arc<Db>,
    now: u64,
) -> Result<Vec<ArmedRoutineScheduleDueIndexRecord>, ErrorData> {
    let mut out = Vec::new();
    let mut scanned = 0_usize;
    let prefix = ARMED_ROUTINE_SCHEDULE_DUE_PREFIX.as_bytes();
    let mut start = prefix.to_vec();
    loop {
        if scanned >= MAX_SCAN_ROWS {
            return Err(internal(format!(
                "ARMED_ROUTINE_DUE_INDEX_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS} CF_KV rows"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
            .map_err(storage_error)?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            if !key.starts_with(prefix) {
                return Ok(out);
            }
            scanned = scanned.saturating_add(1);
            let index = decode_schedule_due_index(key, value, "due scan")?;
            validate_schedule_due_index_key(key, &index, "due scan")?;
            if index.due_ts_ns > now {
                return Ok(out);
            }
            validate_schedule_due_by_id_backlink(db, &index)?;
            out.push(index);
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

fn load_armed_primary_from_schedule_index(
    db: &Arc<Db>,
    index: &ArmedRoutineScheduleDueIndexRecord,
) -> Result<(ArmedRoutineRecord, Vec<u8>), ErrorData> {
    let primary_key = armed_routine_key(&index.routine_id).into_bytes();
    let expected_primary_key_hex = hex_encode(&primary_key);
    if index.primary_key_hex != expected_primary_key_hex {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_PRIMARY_KEY_MISMATCH for {}: index_key={}, expected_key={}",
                index.routine_id, index.primary_key_hex, expected_primary_key_hex
            ),
        ));
    }
    let Some(primary_value) = load_exact_cf_value(
        db,
        cf::CF_KV,
        &primary_key,
        "armed routine primary from due index",
    )?
    else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_DANGLING: routine_id {} points at missing primary key {}",
                index.routine_id,
                String::from_utf8_lossy(&primary_key)
            ),
        ));
    };
    let actual_hash = sha256_hex(&primary_value);
    if index.primary_value_sha256 != actual_hash {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_PRIMARY_HASH_MISMATCH for {}: index_hash={}, actual_hash={}",
                index.routine_id, index.primary_value_sha256, actual_hash
            ),
        ));
    }
    let record: ArmedRoutineRecord = decode_json(&primary_value).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_ROW_DECODE_FAILED for {} from due index: {error}",
                index.routine_id
            ),
        )
    })?;
    if record.routine_id != index.routine_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_RECORD_MISMATCH: index for {} points at primary row for {}",
                index.routine_id, record.routine_id
            ),
        ));
    }
    let Some(routine_source) = load_routine_record_with_raw(db, &index.routine_id)? else {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_ROUTINE_DANGLING: routine_id {} points at missing CF_ROUTINES row",
                index.routine_id
            ),
        ));
    };
    let routine_key_hex = hex_encode(&routine_source.key);
    let routine_value_sha256 = sha256_hex(&routine_source.value);
    if index.routine_key_hex != routine_key_hex
        || index.routine_value_sha256 != routine_value_sha256
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "ARMED_ROUTINE_DUE_INDEX_ROUTINE_HASH_MISMATCH for {}: index_key={}, actual_key={}, index_hash={}, actual_hash={}",
                index.routine_id,
                index.routine_key_hex,
                routine_key_hex,
                index.routine_value_sha256,
                routine_value_sha256
            ),
        ));
    }
    Ok((record, primary_value))
}

struct ScheduleDueIndexMutation {
    deletes: Vec<Vec<u8>>,
    puts: Vec<(Vec<u8>, Vec<u8>)>,
    expected: Option<ArmedRoutineScheduleDueIndexRecord>,
}

fn build_schedule_due_index_record(
    db: &Arc<Db>,
    record: &ArmedRoutineRecord,
    primary_key: &[u8],
    primary_value: &[u8],
    anchor_ts_ns: u64,
) -> Result<Option<ArmedRoutineScheduleDueIndexRecord>, ErrorData> {
    if !record.enabled || !record.schedule_enabled {
        return Ok(None);
    }
    let Some(routine_source) = load_routine_record_with_raw(db, &record.routine_id)? else {
        return Ok(None);
    };
    if let Some(state) = load_state_row(db.as_ref(), &record.routine_id)?
        && matches!(
            state.lifecycle,
            RoutineLifecycle::Disabled | RoutineLifecycle::Archived
        )
    {
        return Ok(None);
    }
    let Some(automation) = load_routine_automation_record(db, &record.routine_id)? else {
        return Ok(None);
    };
    if automation.state != "installed" || automation.plan_ref.trim().is_empty() {
        return Ok(None);
    }
    let Some(due_ts_ns) = next_schedule_due_ts(record, &routine_source.record, anchor_ts_ns)?
    else {
        return Ok(None);
    };
    Ok(Some(ArmedRoutineScheduleDueIndexRecord {
        record_version: ARMED_ROUTINE_SCHEDULE_DUE_INDEX_RECORD_VERSION,
        row_kind: "armed_routine_schedule_due_index".to_owned(),
        routine_id: record.routine_id.clone(),
        due_ts_ns,
        primary_key_hex: hex_encode(primary_key),
        primary_value_sha256: sha256_hex(primary_value),
        routine_key_hex: hex_encode(&routine_source.key),
        routine_value_sha256: sha256_hex(&routine_source.value),
    }))
}

fn schedule_due_index_mutation(
    db: &Arc<Db>,
    record: &ArmedRoutineRecord,
    primary_key: &[u8],
    primary_value: &[u8],
    anchor_ts_ns: u64,
) -> Result<ScheduleDueIndexMutation, ErrorData> {
    let mut deletes = Vec::new();
    let by_id_key = schedule_due_by_id_index_key(&record.routine_id);
    if let Some(existing) = load_schedule_due_by_id_index(db, &record.routine_id)? {
        deletes.push(by_id_key.clone());
        deletes.push(schedule_due_index_key(
            &existing.routine_id,
            existing.due_ts_ns,
        ));
    }

    let expected =
        build_schedule_due_index_record(db, record, primary_key, primary_value, anchor_ts_ns)?;
    let mut puts = Vec::new();
    if let Some(index) = &expected {
        let due_key = schedule_due_index_key(&index.routine_id, index.due_ts_ns);
        let index_value = encode_json(index).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to encode armed routine due index for {}: {error}",
                    index.routine_id
                ),
            )
        })?;
        puts.push((due_key, index_value.clone()));
        puts.push((by_id_key, index_value));
    }

    Ok(ScheduleDueIndexMutation {
        deletes,
        puts,
        expected,
    })
}

fn validate_schedule_due_index_readback(
    db: &Arc<Db>,
    routine_id: &str,
    mutation: &ScheduleDueIndexMutation,
) -> Result<(), ErrorData> {
    if let Some(index) = &mutation.expected {
        let due_key = schedule_due_index_key(&index.routine_id, index.due_ts_ns);
        let Some(due_value) = load_exact_cf_value(db, cf::CF_KV, &due_key, "due index readback")?
        else {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_DUE_INDEX_READBACK_MISSING for {} at {}",
                    index.routine_id,
                    String::from_utf8_lossy(&due_key)
                ),
            ));
        };
        let due_readback = decode_schedule_due_index(&due_key, &due_value, "due readback")?;
        if due_readback != *index {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_DUE_INDEX_READBACK_MISMATCH for {}",
                    index.routine_id
                ),
            ));
        }

        let by_id_key = schedule_due_by_id_index_key(&index.routine_id);
        let Some(by_id_value) =
            load_exact_cf_value(db, cf::CF_KV, &by_id_key, "due by-id readback")?
        else {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_DUE_BY_ID_READBACK_MISSING for {}",
                    index.routine_id
                ),
            ));
        };
        let by_id_readback = decode_schedule_due_index(&by_id_key, &by_id_value, "by-id readback")?;
        if by_id_readback != *index {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_DUE_BY_ID_READBACK_MISMATCH for {}",
                    index.routine_id
                ),
            ));
        }
    } else {
        let by_id_key = schedule_due_by_id_index_key(routine_id);
        if load_exact_cf_value(db, cf::CF_KV, &by_id_key, "due by-id absence readback")?.is_some() {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("ARMED_ROUTINE_DUE_BY_ID_DELETE_READBACK_PRESENT for {routine_id}"),
            ));
        }
    }

    for deleted_key in &mutation.deletes {
        if let Some(index) = &mutation.expected {
            if deleted_key == &schedule_due_index_key(&index.routine_id, index.due_ts_ns)
                || deleted_key == &schedule_due_by_id_index_key(&index.routine_id)
            {
                continue;
            }
        }
        if load_exact_cf_value(db, cf::CF_KV, deleted_key, "deleted due index readback")?.is_some()
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_DUE_INDEX_DELETE_READBACK_PRESENT at {}",
                    String::from_utf8_lossy(deleted_key)
                ),
            ));
        }
    }
    Ok(())
}

fn refresh_schedule_due_index(
    db: &Arc<Db>,
    record: &ArmedRoutineRecord,
    primary_value: &[u8],
    anchor_ts_ns: u64,
) -> Result<(), ErrorData> {
    let primary_key = armed_routine_key(&record.routine_id).into_bytes();
    let mutation =
        schedule_due_index_mutation(db, record, &primary_key, primary_value, anchor_ts_ns)?;
    db.mutate_batch_pressure_bypass(cf::CF_KV, mutation.deletes.clone(), mutation.puts.clone())
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to refresh armed routine due index for {}: {error}",
                    record.routine_id
                ),
            )
        })?;
    validate_schedule_due_index_readback(db, &record.routine_id, &mutation)
}

fn scan_index_keys(db: &Arc<Db>, prefix: &[u8]) -> Result<Vec<Vec<u8>>, ErrorData> {
    let mut out = Vec::new();
    let mut scanned = 0_usize;
    let mut start = prefix.to_vec();
    loop {
        if scanned >= MAX_SCAN_ROWS {
            return Err(internal(format!(
                "ARMED_ROUTINE_INDEX_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS} CF_KV rows"
            )));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
            .map_err(storage_error)?;
        if rows.is_empty() {
            break;
        }
        for (key, _value) in &rows {
            if !key.starts_with(prefix) {
                return Ok(out);
            }
            scanned = scanned.saturating_add(1);
            out.push(key.clone());
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

pub fn reindex_armed_routine_schedule_due_indexes(db: &Arc<Db>) -> Result<(), ErrorData> {
    let armed = load_all_armed_routines(db)?;
    let mut deletes = scan_index_keys(db, ARMED_ROUTINE_SCHEDULE_DUE_PREFIX.as_bytes())?;
    deletes.extend(scan_index_keys(
        db,
        ARMED_ROUTINE_SCHEDULE_DUE_BY_ID_PREFIX.as_bytes(),
    )?);
    let anchor = now_ts_ns();
    let mut expected = Vec::new();
    let mut puts = Vec::new();
    for record in &armed {
        let primary_key = armed_routine_key(&record.routine_id).into_bytes();
        let Some(primary_value) =
            load_exact_cf_value(db, cf::CF_KV, &primary_key, "armed primary during reindex")?
        else {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_REINDEX_PRIMARY_MISSING for {}",
                    record.routine_id
                ),
            ));
        };
        let decoded: ArmedRoutineRecord = decode_json(&primary_value).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_REINDEX_PRIMARY_DECODE_FAILED for {}: {error}",
                    record.routine_id
                ),
            )
        })?;
        if decoded != *record {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "ARMED_ROUTINE_REINDEX_PRIMARY_MISMATCH for {}",
                    record.routine_id
                ),
            ));
        }
        if let Some(index) =
            build_schedule_due_index_record(db, record, &primary_key, &primary_value, anchor)?
        {
            let index_value = encode_json(&index).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "failed to encode armed routine due index for {} during reindex: {error}",
                        index.routine_id
                    ),
                )
            })?;
            puts.push((
                schedule_due_index_key(&index.routine_id, index.due_ts_ns),
                index_value.clone(),
            ));
            puts.push((schedule_due_by_id_index_key(&index.routine_id), index_value));
            expected.push(index);
        }
    }

    db.mutate_batch_pressure_bypass(cf::CF_KV, deletes, puts)
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!("failed to rebuild armed routine due indexes atomically: {error}"),
            )
        })?;

    for index in expected {
        let routine_id = index.routine_id.clone();
        validate_schedule_due_index_readback(
            db,
            &routine_id,
            &ScheduleDueIndexMutation {
                deletes: Vec::new(),
                puts: Vec::new(),
                expected: Some(index),
            },
        )?;
    }
    Ok(())
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
    if let Some(browser_window_hwnd) = params.browser_window_hwnd {
        crate::m1::validate_hwnd_shape(
            "armed_routine_tick",
            "browser_window_hwnd",
            browser_window_hwnd,
        )?;
    }
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
    let key_bytes = key.into_bytes();
    let value = encode_json(record).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode armed routine row for {}: {error}",
                record.routine_id
            ),
        )
    })?;
    let index_mutation =
        schedule_due_index_mutation(db, record, &key_bytes, &value, record.updated_at_ns)?;
    let mut puts = index_mutation.puts.clone();
    puts.push((key_bytes, value));
    db.mutate_batch_pressure_bypass(cf::CF_KV, index_mutation.deletes.clone(), puts)
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to persist armed routine row and due indexes for {}: {error}",
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
    validate_schedule_due_index_readback(db, &record.routine_id, &index_mutation)?;
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
    let index_mutation =
        schedule_due_index_mutation(db, armed, &armed_key, &armed_value, armed.updated_at_ns)?;
    let mut puts = index_mutation.puts.clone();
    puts.push((armed_key, armed_value));
    puts.push((run_key, run_value));
    db.mutate_batch_pressure_bypass(cf::CF_KV, index_mutation.deletes.clone(), puts)
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to persist armed routine run {} and due indexes atomically: {error}",
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
    validate_schedule_due_index_readback(db, &armed.routine_id, &index_mutation)?;
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
