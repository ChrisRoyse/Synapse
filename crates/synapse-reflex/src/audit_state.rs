use serde_json::json;
use synapse_core::{ReflexState, ReflexStatus, SCHEMA_VERSION, StoredReflexAudit, error_codes};
use uuid::Uuid;

use crate::audit::write_registration_audit;
use crate::{
    REFLEX_CANCELLED_KIND, REFLEX_DISABLED_KIND, REFLEX_REGISTERED_KIND, ReflexError, ReflexResult,
    ReflexRuntime, write_audit,
};

impl ReflexRuntime {
    pub(crate) fn write_registration_audit(
        &self,
        status: &ReflexStatus,
        registered_at_ns: u64,
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
        write_registration_audit(&self.db, &audit).map_err(|error| {
            ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_REGISTRATION_DURABLE_COMMIT_FAILED: phase=durable_prepare scheduler=prepared_inactive audit=not_committed detail={error}; remediation=inspect the structured storage/Calyx error, repair the named guard or durability failure, and retry registration; the requested reflex was not activated"
                ),
            }
        })
    }

    pub(crate) fn write_cancellation_audit(&self, status: &ReflexStatus) -> ReflexResult<()> {
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
        write_audit(&self.db, &audit).map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("cancellation audit write failed: {error}"),
        })?;
        self.db.flush().map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("cancellation audit flush failed: {error}"),
        })
    }

    pub(crate) fn write_disabled_audits_with_reason(
        &self,
        statuses: &[ReflexStatus],
        reason: &'static str,
    ) -> ReflexResult<()> {
        if statuses.is_empty() {
            return Ok(());
        }
        for status in statuses {
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
            write_audit(&self.db, &audit).map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("disabled audit write failed: {error}"),
            })?;
        }
        self.db.flush().map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("disabled audit flush failed: {error}"),
        })
    }
}
