use chrono::Utc;
use serde_json::json;
use synapse_core::{ReflexState, ReflexStatus, SCHEMA_VERSION, StoredReflexAudit, error_codes};
use uuid::Uuid;

use crate::{
    REFLEX_CANCELLED_KIND, REFLEX_DISABLED_KIND, REFLEX_REGISTERED_KIND, ReflexError, ReflexResult,
    ReflexRuntime, write_audit,
};

impl ReflexRuntime {
    pub(crate) fn write_registration_audit(&self, status: &ReflexStatus) -> ReflexResult<()> {
        let audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: status.id.clone(),
            ts_ns: now_ts_ns(),
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
        write_audit(&self.db, &audit).map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("registration audit write failed: {error}"),
        })?;
        self.db.flush().map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("registration audit flush failed: {error}"),
        })
    }

    pub(crate) fn write_cancellation_audit(&self, status: &ReflexStatus) -> ReflexResult<()> {
        let audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: status.id.clone(),
            ts_ns: now_ts_ns(),
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
                ts_ns: now_ts_ns(),
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

fn now_ts_ns() -> u64 {
    Utc::now()
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}
