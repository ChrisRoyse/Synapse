//! Durable setup-plan execution reports (#860).
//!
//! Plan compilation (#859) is intentionally pure. This module owns the durable
//! readback for execution attempts so an accepted suggestion never resolves as
//! a silent success: every step is stored as succeeded, failed, refused,
//! skipped, or dry-run with evidence/error detail.

use std::sync::Arc;

use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use synapse_core::error_codes;
use synapse_storage::{Db, cf, decode_json, encode_json};

use crate::m1::mcp_error;

use super::plan::{PlanBackend, PlanDocument, Postcondition};

const PLAN_EXECUTION_PREFIX: &str = "plan_execution/v1/";
const PLAN_EXECUTION_RECORD_VERSION: u32 = 1;

pub const PLAN_EXECUTION_SOURCE_OF_TRUTH: &str =
    "CF_KV plan_execution/v1 rows plus physical action/backend readbacks";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanExecutionStatus {
    Succeeded,
    Failed,
    Skipped,
    DryRun,
}

impl PlanExecutionStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::DryRun => "dry_run",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepExecutionStatus {
    Succeeded,
    Failed,
    Refused,
    Skipped,
    DryRun,
}

impl PlanStepExecutionStatus {
    #[must_use]
    pub const fn is_terminal_failure(self) -> bool {
        matches!(self, Self::Failed | Self::Refused)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStepExecutionReport {
    pub index: u32,
    pub backend: PlanBackend,
    pub action: String,
    pub postcondition: Postcondition,
    pub status: PlanStepExecutionStatus,
    pub started_ts_ns: u64,
    pub completed_ts_ns: u64,
    pub duration_ns: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub evidence: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanExecutionRecord {
    pub record_version: u32,
    pub execution_id: String,
    pub suggestion_id: String,
    pub routine_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub accepted_ts_ns: u64,
    pub started_ts_ns: u64,
    pub completed_ts_ns: u64,
    pub duration_ns: u64,
    pub dry_run: bool,
    pub status: PlanExecutionStatus,
    pub source_of_truth: String,
    pub plan: PlanDocument,
    pub steps: Vec<PlanStepExecutionReport>,
}

#[must_use]
pub fn plan_execution_id(suggestion_id: &str, started_ts_ns: u64) -> String {
    format!(
        "px1-{}-{started_ts_ns:020}",
        storage_token(suggestion_id).trim_matches('-')
    )
}

fn storage_token(raw: &str) -> String {
    let token: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if token.is_empty() {
        "unknown".to_owned()
    } else {
        token
    }
}

fn plan_execution_key(execution_id: &str) -> Vec<u8> {
    format!("{PLAN_EXECUTION_PREFIX}{execution_id}").into_bytes()
}

fn storage_error(error: impl std::fmt::Display) -> ErrorData {
    mcp_error(
        error_codes::STORAGE_READ_FAILED,
        format!("plan execution storage failure: {error}"),
    )
}

#[must_use]
pub fn build_plan_execution_record(
    execution_id: String,
    suggestion_id: String,
    session_id: Option<String>,
    accepted_ts_ns: u64,
    started_ts_ns: u64,
    completed_ts_ns: u64,
    dry_run: bool,
    plan: PlanDocument,
    steps: Vec<PlanStepExecutionReport>,
) -> PlanExecutionRecord {
    let failed = steps.iter().any(|step| step.status.is_terminal_failure());
    let all_succeeded = !steps.is_empty()
        && steps
            .iter()
            .all(|step| step.status == PlanStepExecutionStatus::Succeeded);
    let status = if dry_run {
        PlanExecutionStatus::DryRun
    } else if failed {
        PlanExecutionStatus::Failed
    } else if all_succeeded {
        PlanExecutionStatus::Succeeded
    } else {
        PlanExecutionStatus::Skipped
    };
    PlanExecutionRecord {
        record_version: PLAN_EXECUTION_RECORD_VERSION,
        execution_id,
        suggestion_id,
        routine_id: plan.routine_id.clone(),
        session_id,
        accepted_ts_ns,
        started_ts_ns,
        completed_ts_ns,
        duration_ns: completed_ts_ns.saturating_sub(started_ts_ns),
        dry_run,
        status,
        source_of_truth: PLAN_EXECUTION_SOURCE_OF_TRUTH.to_owned(),
        plan,
        steps,
    }
}

pub fn write_plan_execution(db: &Arc<Db>, record: &PlanExecutionRecord) -> Result<(), ErrorData> {
    if !db.pressure_permits_write(cf::CF_KV) {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "plan execution write refused under disk pressure: pressure_level={:?}",
                db.pressure_level()
            ),
        ));
    }
    let key = plan_execution_key(&record.execution_id);
    let value = encode_json(record).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "failed to encode plan execution {}: {error}",
                record.execution_id
            ),
        )
    })?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(key, value)])
        .map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "failed to persist plan execution {}: {error}",
                    record.execution_id
                ),
            )
        })?;
    let readback = load_plan_execution(db, &record.execution_id)?.ok_or_else(|| {
        mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "PLAN_EXECUTION_READBACK_MISSING: row for {} vanished immediately after write",
                record.execution_id
            ),
        )
    })?;
    if &readback != record {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "PLAN_EXECUTION_READBACK_MISMATCH for {}: persisted row != value just written",
                record.execution_id
            ),
        ));
    }
    Ok(())
}

pub fn load_plan_execution(
    db: &Arc<Db>,
    execution_id: &str,
) -> Result<Option<PlanExecutionRecord>, ErrorData> {
    let key = plan_execution_key(execution_id);
    let rows = db.scan_cf_prefix(cf::CF_KV, &key).map_err(storage_error)?;
    match rows.into_iter().find(|(k, _)| k == &key) {
        Some((_, value)) => {
            let record: PlanExecutionRecord = decode_json(&value).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("PLAN_EXECUTION_ROW_DECODE_FAILED for {execution_id}: {error}"),
                )
            })?;
            Ok(Some(record))
        }
        None => Ok(None),
    }
}
