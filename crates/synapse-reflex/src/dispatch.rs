use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use synapse_action::ActionHandle;
use synapse_core::{
    Action, ReflexId, ReflexState, SCHEMA_VERSION, StoredAuditContext, StoredReflexAudit,
    StoredReflexStep, error_codes,
};
use synapse_storage::Db;
use uuid::Uuid;

use crate::{ReflexError, ReflexResult, write_audit};

pub const REFLEX_ACTION_PERMISSION_DENIED_KIND: &str = "reflex_action_permission_denied";
pub const REFLEX_ACTION_DENIED_STEP_STATUS: &str = "action_denied";

pub type ReflexActionGateHandle = Arc<dyn ReflexActionGate>;

pub trait ReflexActionGate: Send + Sync {
    /// Checks whether a reflex action may dispatch at the current point in time.
    ///
    /// # Errors
    ///
    /// Returns [`ReflexActionPermissionDenied`] when the active policy refuses
    /// the action.
    fn ensure_action_allowed(
        &self,
        reflex_id: &ReflexId,
        action: &Action,
    ) -> Result<(), ReflexActionPermissionDenied>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflexActionPermissionDenied {
    pub policy_code: Option<String>,
    pub policy_reason: Option<String>,
    pub profile_id: Option<String>,
    pub use_scope: Option<String>,
    pub detail: String,
}

impl ReflexActionPermissionDenied {
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            policy_code: None,
            policy_reason: None,
            profile_id: None,
            use_scope: None,
            detail: detail.into(),
        }
    }
}

#[derive(Clone)]
pub struct ReflexActionDispatchContext {
    action_handle: ActionHandle,
    action_gate: Option<ReflexActionGateHandle>,
    audit_db: Option<Arc<Db>>,
    audit_context: Option<StoredAuditContext>,
    tick_index: u64,
}

impl ReflexActionDispatchContext {
    #[must_use]
    pub fn new(
        action_handle: ActionHandle,
        action_gate: Option<ReflexActionGateHandle>,
        audit_db: Option<Arc<Db>>,
        audit_context: Option<StoredAuditContext>,
        tick_index: u64,
    ) -> Self {
        Self {
            action_handle,
            action_gate,
            audit_db,
            audit_context,
            tick_index,
        }
    }

    pub fn dispatch_action(&self, reflex_id: &ReflexId, action: &Action) -> ReflexResult<()> {
        self.ensure_action_allowed(reflex_id, action)?;

        self.action_handle
            .try_execute(action.clone())
            .map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("scheduler action dispatch failed: {error}"),
            })
    }

    pub fn ensure_action_allowed(&self, reflex_id: &ReflexId, action: &Action) -> ReflexResult<()> {
        if let Some(gate) = &self.action_gate
            && let Err(denial) = gate.ensure_action_allowed(reflex_id, action)
        {
            self.write_action_denied_audit(reflex_id, action, &denial);
            return Err(ReflexError::ActionPermissionDenied {
                reflex_id: reflex_id.clone(),
                detail: denial.detail,
            });
        }
        Ok(())
    }

    fn write_action_denied_audit(
        &self,
        reflex_id: &ReflexId,
        action: &Action,
        denial: &ReflexActionPermissionDenied,
    ) {
        let Some(db) = self.audit_db.as_deref() else {
            return;
        };
        let audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: reflex_id.clone(),
            ts_ns: now_ts_ns(),
            status: ReflexState::ActionDenied,
            event_id: None,
            audit_context: self.audit_context.clone(),
            steps: vec![StoredReflexStep {
                index: 0,
                action: action.clone(),
                status: REFLEX_ACTION_DENIED_STEP_STATUS.to_owned(),
                error_code: Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED.to_owned()),
            }],
            error_code: Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED.to_owned()),
            details: json!({
                "kind": REFLEX_ACTION_PERMISSION_DENIED_KIND,
                "reason": error_codes::REFLEX_ACTION_PERMISSION_DENIED,
                "policy_code": denial.policy_code,
                "policy_reason": denial.policy_reason,
                "profile_id": denial.profile_id,
                "use_scope": denial.use_scope,
                "detail": denial.detail,
                "action_kind": action_kind(action),
                "tick_index": self.tick_index,
            }),
            redacted: false,
            redactions: Vec::new(),
        };

        if let Err(error) = write_audit(db, &audit).and_then(|()| db.flush()) {
            tracing::warn!(
                component = "reflex_dispatch",
                reflex_id = %audit.reflex_id,
                audit_id = %audit.audit_id,
                detail = %error,
                "reflex action-denied audit write failed"
            );
        }
    }
}

const fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::KeyPress { .. } => "key_press",
        Action::KeyDown { .. } => "key_down",
        Action::KeyUp { .. } => "key_up",
        Action::KeyChord { .. } => "key_chord",
        Action::TypeText { .. } => "type_text",
        Action::MouseMove { .. } => "mouse_move",
        Action::MouseMoveRelative { .. } => "mouse_move_relative",
        Action::MouseButton { .. } => "mouse_button",
        Action::MouseDrag { .. } => "mouse_drag",
        Action::MouseStroke { .. } => "mouse_stroke",
        Action::MouseScroll { .. } => "mouse_scroll",
        Action::PadButton { .. } => "pad_button",
        Action::PadStick { .. } => "pad_stick",
        Action::PadTrigger { .. } => "pad_trigger",
        Action::PadReport { .. } => "pad_report",
        Action::AimAt { .. } => "aim_at",
        Action::Combo { .. } => "combo",
        Action::ReleaseAll => "release_all",
    }
}

fn now_ts_ns() -> u64 {
    Utc::now()
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}
