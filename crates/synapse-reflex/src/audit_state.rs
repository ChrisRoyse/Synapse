use serde_json::json;
use synapse_core::{ReflexState, ReflexStatus, SCHEMA_VERSION, StoredReflexAudit, error_codes};
use uuid::Uuid;

use crate::audit::{
    TerminalLifecycleTransition, write_registration_audit, write_terminal_lifecycle_audit,
    write_terminal_lifecycle_batch,
};
use crate::{
    REFLEX_CANCELLED_KIND, REFLEX_DISABLED_KIND, REFLEX_REGISTERED_KIND, ReflexError, ReflexResult,
    ReflexRuntime,
};

impl ReflexRuntime {
    pub(crate) fn write_registration_audit(
        &self,
        status: &ReflexStatus,
        registered_at_ns: u64,
        durable_record: &crate::durable_state::DurableReflexRecord,
    ) -> ReflexResult<()> {
        let audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: status.id.clone(),
            ts_ns: registered_at_ns,
            status: ReflexState::Active,
            event_id: None,
            audit_context: self.audit_context.clone(),
            steps: Vec::new(),
            error_code: None,
            details: json!({
                "kind": REFLEX_REGISTERED_KIND,
                "kind_summary": status.kind_summary,
                "priority": status.priority,
                "lifetime": status.lifetime,
                "exclusive": status.exclusive,
            }),
            redacted: false,
            redactions: Vec::new(),
        };
        write_registration_audit(&self.db, &audit, durable_record).map_err(|error| {
            ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_REGISTRATION_DURABLE_COMMIT_FAILED: phase=durable_prepare scheduler=prepared_inactive audit=not_committed detail={error}; remediation=inspect the structured storage/Calyx error, repair the named guard or durability failure, and retry registration; the requested reflex was not activated"
                ),
            }
        })
    }

    pub(crate) fn write_cancellation_audit(
        &self,
        status: &ReflexStatus,
        prior_record: &crate::durable_state::DurableReflexRecord,
        next_record: &crate::durable_state::DurableReflexRecord,
    ) -> ReflexResult<()> {
        let audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: status.id.clone(),
            ts_ns: crate::audit_timestamp::now_unix_ns(REFLEX_CANCELLED_KIND)?,
            status: ReflexState::Cancelled,
            event_id: None,
            audit_context: self.audit_context.clone(),
            steps: Vec::new(),
            error_code: None,
            details: json!({
                "kind": REFLEX_CANCELLED_KIND,
                "kind_summary": status.kind_summary,
                "priority": status.priority,
                "lifetime": status.lifetime,
                "exclusive": status.exclusive,
            }),
            redacted: false,
            redactions: Vec::new(),
        };
        write_terminal_lifecycle_audit(&self.db, &audit, prior_record, next_record).map_err(
            |error| ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_CANCELLATION_DURABLE_COMMIT_FAILED: phase=durable_commit scheduler_prepared=true scheduler_activated=false desired_state_committed=false detail={error}; remediation=repair the named Calyx/revision failure and retry; the prior scheduler and desired state remain authoritative"
                ),
            },
        )
    }

    pub(crate) fn write_disabled_audits_with_reason(
        &self,
        statuses: &[ReflexStatus],
        reason: &'static str,
    ) -> ReflexResult<Vec<crate::durable_state::DurableReflexRecord>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let mut transitions = Vec::with_capacity(statuses.len());
        let mut next_records = Vec::with_capacity(statuses.len());
        for status in statuses {
            let prior_record = self.durable_records.get(&status.id).cloned().ok_or_else(|| {
                ReflexError::ParamsInvalid {
                    detail: format!(
                        "REFLEX_DURABLE_DEFINITION_MISSING: reflex_id={} phase=disable_prepare scheduler=unchanged; remediation=run explicit orphan reconciliation before disabling this reflex",
                        status.id
                    ),
                }
            })?;
            let next_record = prior_record.with_status(status.clone())?;
            let audit = StoredReflexAudit {
                schema_version: SCHEMA_VERSION,
                audit_id: Uuid::now_v7().to_string(),
                reflex_id: status.id.clone(),
                ts_ns: crate::audit_timestamp::now_unix_ns(REFLEX_DISABLED_KIND)?,
                status: ReflexState::Disabled,
                event_id: None,
                audit_context: self.audit_context.clone(),
                steps: Vec::new(),
                error_code: Some(error_codes::REFLEX_DISABLED_BY_OPERATOR.to_owned()),
                details: json!({
                    "kind": REFLEX_DISABLED_KIND,
                    "kind_summary": status.kind_summary,
                    "priority": status.priority,
                    "lifetime": status.lifetime,
                    "exclusive": status.exclusive,
                    "reason": reason,
                }),
                redacted: false,
                redactions: Vec::new(),
            };
            transitions.push(TerminalLifecycleTransition {
                audit,
                prior_record,
                next_record: next_record.clone(),
            });
            next_records.push(next_record);
        }
        write_terminal_lifecycle_batch(&self.db, transitions).map_err(|error| {
            ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_DISABLE_BATCH_DURABLE_COMMIT_FAILED: phase=durable_commit scheduler_prepared=true scheduler_activated=false desired_state_committed=false detail={error}; remediation=repair the named Calyx/revision failure and retry; every prior scheduler control and desired-state row remains authoritative"
                ),
            }
        })?;
        Ok(next_records)
    }
}
