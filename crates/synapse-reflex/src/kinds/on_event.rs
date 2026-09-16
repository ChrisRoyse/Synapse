use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::json;
use synapse_core::{
    Action, Event, EventRef, EventSource, ReflexId, ReflexState, SCHEMA_VERSION,
    StoredAuditContext, StoredReflexAudit, StoredReflexStep, error_codes,
};
use synapse_storage::Db;
use uuid::Uuid;

use crate::{EventBus, write_audit};

pub const MAX_ON_EVENT_FIRINGS_PER_TICK: usize = 4;
pub const REFLEX_DEBOUNCED_KIND: &str = "reflex_debounced";
pub const REFLEX_FIRED_KIND: &str = "reflex_fired";
pub const REFLEX_RECURSION_LIMIT_KIND: &str = "reflex_recursion_limit";
const REFLEX_RECURSION_CLAMPS_METRIC: &str = "reflex_recursion_clamps_total";

#[derive(Clone, Debug, Default)]
pub(crate) struct OnEventState {
    last_fire: Option<Instant>,
}

impl OnEventState {
    #[must_use]
    pub(crate) fn allows_fire(&self, now: Instant, debounce: Duration) -> bool {
        self.last_fire
            .is_none_or(|last_fire| now.duration_since(last_fire) >= debounce)
    }

    pub(crate) const fn mark_fired(&mut self, now: Instant) {
        self.last_fire = Some(now);
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OnEventTickGuard {
    fired_count: usize,
    limit_reported: bool,
}

impl OnEventTickGuard {
    #[must_use]
    pub(crate) const fn can_fire(&self) -> bool {
        self.fired_count < MAX_ON_EVENT_FIRINGS_PER_TICK
    }

    pub(crate) const fn record_fire(&mut self) {
        self.fired_count = self.fired_count.saturating_add(1);
    }

    pub(crate) fn report_limit_once(
        &mut self,
        event_bus: &EventBus,
        audit_db: Option<&Db>,
        reflex_id: &ReflexId,
        tick_index: u64,
        trigger_event: &Event,
        audit_context: Option<&StoredAuditContext>,
    ) {
        if self.limit_reported {
            return;
        }
        self.limit_reported = true;
        metrics::counter!(REFLEX_RECURSION_CLAMPS_METRIC).increment(1);
        publish_limit_event(event_bus, reflex_id, tick_index, trigger_event);
        let audit = recursion_limit_audit(reflex_id, tick_index, trigger_event, audit_context);
        write_audit_if_configured(audit_db, &audit);
    }
}

pub(crate) fn publish_fired(
    event_bus: &EventBus,
    audit_db: Option<&Db>,
    reflex_id: &ReflexId,
    tick_index: u64,
    trigger_event: &Event,
    actions: &[Action],
    audit_context: Option<&StoredAuditContext>,
) {
    let event = Event {
        seq: tick_index,
        at: Utc::now(),
        source: EventSource::Reflex,
        kind: REFLEX_FIRED_KIND.to_owned(),
        data: json!({
            "reflex_id": reflex_id,
            "trigger_seq": trigger_event.seq,
            "trigger_kind": trigger_event.kind.as_str(),
            "action_count": actions.len(),
        }),
        correlations: trigger_correlation(trigger_event),
    };
    let _report = event_bus.publish(event);
    let audit = fired_audit(reflex_id, tick_index, trigger_event, actions, audit_context);
    write_audit_if_configured(audit_db, &audit);
    tracing::info!(
        code = "REFLEX_FIRED",
        reflex_id = %reflex_id,
        trigger_seq = trigger_event.seq,
        trigger_kind = %trigger_event.kind,
        action_count = actions.len(),
        tick_index,
        "reflex fired"
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_debounced(
    event_bus: &EventBus,
    audit_db: Option<&Db>,
    reflex_id: &ReflexId,
    tick_index: u64,
    trigger_event: &Event,
    debounce: Duration,
    suppressed_count: usize,
    reason: &str,
    audit_context: Option<&StoredAuditContext>,
) {
    let debounce_ms = u64::try_from(debounce.as_millis()).unwrap_or(u64::MAX);
    let suppressed_count = u64::try_from(suppressed_count).unwrap_or(u64::MAX);
    let event = Event {
        seq: tick_index,
        at: Utc::now(),
        source: EventSource::Reflex,
        kind: REFLEX_DEBOUNCED_KIND.to_owned(),
        data: json!({
            "code": error_codes::REFLEX_DEBOUNCED,
            "reflex_id": reflex_id,
            "tick_index": tick_index,
            "trigger_seq": trigger_event.seq,
            "trigger_kind": trigger_event.kind.as_str(),
            "debounce_ms": debounce_ms,
            "suppressed_count": suppressed_count,
            "reason": reason,
        }),
        correlations: trigger_correlation(trigger_event),
    };
    let _report = event_bus.publish(event);
    let audit = debounced_audit(
        reflex_id,
        tick_index,
        trigger_event,
        debounce_ms,
        suppressed_count,
        reason,
        audit_context,
    );
    write_audit_if_configured(audit_db, &audit);
    tracing::info!(
        code = error_codes::REFLEX_DEBOUNCED,
        reflex_id = %reflex_id,
        trigger_seq = trigger_event.seq,
        trigger_kind = %trigger_event.kind,
        debounce_ms,
        suppressed_count,
        reason,
        tick_index,
        "reflex trigger suppressed by debounce"
    );
}

fn publish_limit_event(
    event_bus: &EventBus,
    reflex_id: &ReflexId,
    tick_index: u64,
    trigger_event: &Event,
) {
    let event = Event {
        seq: tick_index,
        at: Utc::now(),
        source: EventSource::Reflex,
        kind: REFLEX_RECURSION_LIMIT_KIND.to_owned(),
        data: json!({
            "code": error_codes::REFLEX_RECURSION_LIMIT,
            "reflex_id": reflex_id,
            "limit": MAX_ON_EVENT_FIRINGS_PER_TICK,
            "tick_index": tick_index,
            "trigger_seq": trigger_event.seq,
            "trigger_kind": trigger_event.kind.as_str(),
        }),
        correlations: trigger_correlation(trigger_event),
    };
    let _report = event_bus.publish(event);
}

fn debounced_audit(
    reflex_id: &ReflexId,
    tick_index: u64,
    trigger_event: &Event,
    debounce_ms: u64,
    suppressed_count: u64,
    reason: &str,
    audit_context: Option<&StoredAuditContext>,
) -> StoredReflexAudit {
    StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: reflex_id.clone(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Active,
        event_id: Some(trigger_event.seq.to_string()),
        audit_context: audit_context.cloned(),
        steps: Vec::new(),
        error_code: Some(error_codes::REFLEX_DEBOUNCED.to_owned()),
        details: json!({
            "kind": REFLEX_DEBOUNCED_KIND,
            "tick_index": tick_index,
            "trigger_kind": trigger_event.kind.as_str(),
            "debounce_ms": debounce_ms,
            "suppressed_count": suppressed_count,
            "reason": reason,
        }),
        redacted: false,
        redactions: Vec::new(),
    }
}

fn fired_audit(
    reflex_id: &ReflexId,
    tick_index: u64,
    trigger_event: &Event,
    actions: &[Action],
    audit_context: Option<&StoredAuditContext>,
) -> StoredReflexAudit {
    StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: reflex_id.clone(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Active,
        event_id: Some(trigger_event.seq.to_string()),
        audit_context: audit_context.cloned(),
        steps: completed_steps(actions),
        error_code: None,
        details: json!({
            "kind": REFLEX_FIRED_KIND,
            "tick_index": tick_index,
            "trigger_kind": trigger_event.kind.as_str(),
        }),
        redacted: false,
        redactions: Vec::new(),
    }
}

fn recursion_limit_audit(
    reflex_id: &ReflexId,
    tick_index: u64,
    trigger_event: &Event,
    audit_context: Option<&StoredAuditContext>,
) -> StoredReflexAudit {
    StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: reflex_id.clone(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Active,
        event_id: Some(trigger_event.seq.to_string()),
        audit_context: audit_context.cloned(),
        steps: Vec::new(),
        error_code: Some(error_codes::REFLEX_RECURSION_LIMIT.to_owned()),
        details: json!({
            "kind": REFLEX_RECURSION_LIMIT_KIND,
            "limit": MAX_ON_EVENT_FIRINGS_PER_TICK,
            "tick_index": tick_index,
            "trigger_kind": trigger_event.kind.as_str(),
        }),
        redacted: false,
        redactions: Vec::new(),
    }
}

fn completed_steps(actions: &[Action]) -> Vec<StoredReflexStep> {
    actions
        .iter()
        .enumerate()
        .map(|(index, action)| StoredReflexStep {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            action: action.clone(),
            status: "completed".to_owned(),
            error_code: None,
        })
        .collect()
}

fn write_audit_if_configured(audit_db: Option<&Db>, audit: &StoredReflexAudit) {
    let Some(db) = audit_db else {
        return;
    };
    if let Err(error) = write_audit(db, audit) {
        tracing::warn!(
            component = "reflex_on_event",
            reflex_id = %audit.reflex_id,
            audit_id = %audit.audit_id,
            detail = %error,
            "reflex audit write failed"
        );
    }
}

fn trigger_correlation(trigger_event: &Event) -> Vec<EventRef> {
    vec![EventRef {
        seq: trigger_event.seq,
        relation: "trigger".to_owned(),
    }]
}

fn now_ts_ns() -> u64 {
    Utc::now()
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}
