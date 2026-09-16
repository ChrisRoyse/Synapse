use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::Utc;
use serde_json::json;
use synapse_core::{
    Action, Event, EventSource, ReflexId, ReflexLifetime, ReflexState, SCHEMA_VERSION,
    StoredReflexAudit, error_codes,
};
use uuid::Uuid;

use super::{
    REFLEX_TICK_LATE_KIND, RuntimeState, ScheduledReflexDriver, SchedulerTrigger, TickSample,
    scheduler_combo::{dispatch_reflex_action, step_active_combos},
    scheduler_loop::TickLateSignal,
    scheduler_stateful::step_stateful_controllers,
};
use crate::{
    ReflexError, ReflexResult,
    conflict::{ConflictCandidate, ConflictLoser, REFLEX_STARVED_KIND, resolve_conflicts},
    kinds::{
        hold_lifetime::{HoldReleaseReason, emit_lifetime_expired},
        on_event::{OnEventTickGuard, publish_debounced, publish_fired},
    },
    write_audit,
};

const REFLEX_TICK_JITTER_METRIC: &str = "reflex_tick_jitter_us";
const REFLEX_STARVED_METRIC: &str = "reflex_starved_total";

pub(super) fn tick(runtime: &mut RuntimeState, elapsed: Duration, degraded: bool) {
    let events = runtime.subscription.drain();
    expire_action_until_event_lifetimes(runtime, &events);
    let mut dispatched_actions = 0_usize;
    let mut dispatch_blocked = false;
    let mut starvation_losers = Vec::new();
    step_active_combos(
        runtime,
        elapsed,
        &mut dispatched_actions,
        &mut dispatch_blocked,
    );
    if !dispatch_blocked {
        step_stateful_controllers(
            runtime,
            &events,
            elapsed,
            &mut dispatched_actions,
            &mut dispatch_blocked,
            &mut starvation_losers,
        );
    }

    if !dispatch_blocked {
        dispatch_triggered_reflexes(
            runtime,
            &events,
            &mut dispatched_actions,
            &mut dispatch_blocked,
            &mut starvation_losers,
        );
    }

    if !dispatch_blocked || !starvation_losers.is_empty() {
        let controls = super::lock_controls(&runtime.controls).clone();
        record_starvation(runtime, &starvation_losers, elapsed, &controls);
    }

    record_tick_sample(
        runtime,
        elapsed,
        degraded,
        events.len(),
        dispatched_actions,
        dispatch_blocked,
    );
}

fn dispatch_triggered_reflexes(
    runtime: &mut RuntimeState,
    events: &[Event],
    dispatched_actions: &mut usize,
    dispatch_blocked: &mut bool,
    starvation_losers: &mut Vec<ConflictLoser>,
) {
    let now = Instant::now();
    let controls = super::lock_controls(&runtime.controls).clone();
    let triggered = collect_triggered_reflexes(runtime, events, now, &controls);
    let candidates = triggered
        .iter()
        .enumerate()
        .map(|(candidate_index, trigger)| {
            let runtime_reflex = &runtime.reflexes[trigger.reflex_index];
            let control = &controls[trigger.reflex_index];
            ConflictCandidate::new(
                candidate_index,
                trigger.reflex_index,
                trigger.reflex_id.clone(),
                control.priority,
                runtime_reflex.registration_order,
                runtime_reflex.reflex.exclusive,
                &trigger.actions,
            )
        })
        .collect::<Vec<_>>();
    let resolution = resolve_conflicts(&candidates);
    starvation_losers.extend(resolution.losers);

    let mut guard = OnEventTickGuard::default();

    for candidate_index in resolution.winners {
        let trigger = &triggered[candidate_index];
        if !controls
            .get(trigger.reflex_index)
            .is_some_and(|control| control.active)
        {
            continue;
        }
        match &trigger.trigger_event {
            None => match dispatch_actions(runtime, &trigger.reflex_id, trigger.actions.clone()) {
                Ok(action_count) => {
                    *dispatched_actions = dispatched_actions.saturating_add(action_count);
                    super::mark_reflex_fired(runtime, trigger.reflex_index);
                }
                Err(error) => {
                    *dispatch_blocked = true;
                    mark_dispatch_error(runtime, trigger.reflex_index, &error);
                    warn_dispatch_blocked(&trigger.reflex_id, &error);
                    break;
                }
            },
            Some(event) => {
                if !guard.can_fire() {
                    guard.report_limit_once(
                        &runtime.event_bus,
                        runtime.audit_db.as_deref(),
                        &trigger.reflex_id,
                        runtime.tick_index,
                        event,
                        runtime.audit_context.as_ref(),
                    );
                    break;
                }
                match dispatch_actions(runtime, &trigger.reflex_id, trigger.actions.clone()) {
                    Ok(action_count) => {
                        *dispatched_actions = dispatched_actions.saturating_add(action_count);
                        runtime.on_event_states[trigger.reflex_index].mark_fired(now);
                        guard.record_fire();
                        publish_fired(
                            &runtime.event_bus,
                            runtime.audit_db.as_deref(),
                            &trigger.reflex_id,
                            runtime.tick_index,
                            event,
                            &trigger.actions,
                            runtime.audit_context.as_ref(),
                        );
                        super::mark_reflex_fired(runtime, trigger.reflex_index);
                    }
                    Err(error) => {
                        *dispatch_blocked = true;
                        mark_dispatch_error(runtime, trigger.reflex_index, &error);
                        warn_dispatch_blocked(&trigger.reflex_id, &error);
                        break;
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct TriggeredReflex {
    reflex_index: usize,
    reflex_id: ReflexId,
    actions: Vec<Action>,
    trigger_event: Option<Event>,
}

fn collect_triggered_reflexes(
    runtime: &RuntimeState,
    events: &[Event],
    now: Instant,
    controls: &[super::ReflexControl],
) -> Vec<TriggeredReflex> {
    let mut triggered = Vec::new();
    for index in 0..runtime.reflexes.len() {
        if !controls.get(index).is_some_and(|control| control.active) {
            continue;
        }
        let reflex = &runtime.reflexes[index].reflex;
        if !matches!(reflex.driver, ScheduledReflexDriver::Actions) {
            continue;
        }
        match &reflex.trigger {
            SchedulerTrigger::EveryTick => {
                triggered.push(TriggeredReflex {
                    reflex_index: index,
                    reflex_id: reflex.reflex_id.clone(),
                    actions: reflex.then.clone(),
                    trigger_event: None,
                });
            }
            SchedulerTrigger::OnEvent(filter) => {
                let mut accepted_this_tick = false;
                let mut same_tick_suppression = DebounceSuppression::default();
                let mut window_suppression = DebounceSuppression::default();
                for event in events {
                    if !filter.matches(event) {
                        continue;
                    }
                    if accepted_this_tick {
                        same_tick_suppression.record(event);
                        continue;
                    }
                    if !runtime.on_event_states[index].allows_fire(now, reflex.debounce) {
                        window_suppression.record(event);
                        continue;
                    }
                    triggered.push(TriggeredReflex {
                        reflex_index: index,
                        reflex_id: reflex.reflex_id.clone(),
                        actions: reflex.then.clone(),
                        trigger_event: Some(event.clone()),
                    });
                    accepted_this_tick = !reflex.debounce.is_zero();
                }
                publish_debounce_suppression(runtime, reflex, same_tick_suppression, "same_tick");
                publish_debounce_suppression(
                    runtime,
                    reflex,
                    window_suppression,
                    "debounce_window",
                );
            }
        }
    }
    triggered
}

#[derive(Clone, Debug, Default)]
struct DebounceSuppression {
    first_event: Option<Event>,
    count: usize,
}

impl DebounceSuppression {
    fn record(&mut self, event: &Event) {
        if self.first_event.is_none() {
            self.first_event = Some(event.clone());
        }
        self.count = self.count.saturating_add(1);
    }
}

fn publish_debounce_suppression(
    runtime: &RuntimeState,
    reflex: &super::ScheduledReflex,
    suppression: DebounceSuppression,
    reason: &str,
) {
    let Some(first_event) = suppression.first_event else {
        return;
    };
    publish_debounced(
        &runtime.event_bus,
        runtime.audit_db.as_deref(),
        &reflex.reflex_id,
        runtime.tick_index,
        &first_event,
        reflex.debounce,
        suppression.count,
        reason,
        runtime.audit_context.as_ref(),
    );
}

fn expire_action_until_event_lifetimes(runtime: &RuntimeState, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    let controls = super::lock_controls(&runtime.controls).clone();
    for index in 0..runtime.reflexes.len() {
        let Some(reflex_id) = until_event_lifetime_expired(runtime, index, events, &controls)
        else {
            continue;
        };
        emit_lifetime_expired(
            &runtime.event_bus,
            &reflex_id,
            HoldReleaseReason::Event,
            Duration::ZERO,
        );
        super::mark_reflex_lifetime_expired(runtime, index, HoldReleaseReason::Event.as_str());
    }
}

fn until_event_lifetime_expired(
    runtime: &RuntimeState,
    index: usize,
    events: &[Event],
    controls: &[super::ReflexControl],
) -> Option<ReflexId> {
    if !controls.get(index).is_some_and(|control| control.active) {
        return None;
    }
    let reflex = &runtime.reflexes[index].reflex;
    if !matches!(reflex.driver, ScheduledReflexDriver::Actions) {
        return None;
    }
    let ReflexLifetime::UntilEvent { filter } = &reflex.lifetime else {
        return None;
    };
    events
        .iter()
        .any(|event| filter.matches(event))
        .then(|| reflex.reflex_id.clone())
}

fn record_starvation(
    runtime: &mut RuntimeState,
    losers: &[crate::conflict::ConflictLoser],
    elapsed: Duration,
    controls: &[super::ReflexControl],
) {
    let mut losing_slots = HashSet::with_capacity(losers.len());
    for loser in losers {
        losing_slots.insert(loser.loser_slot);
        if runtime.starvation_states[loser.loser_slot].record_loss(elapsed) {
            metrics::counter!(
                REFLEX_STARVED_METRIC,
                "reflex_id" => format!("slot:{}", loser.loser_slot)
            )
            .increment(1);
            publish_starved(
                &runtime.event_bus,
                runtime.audit_db.as_deref(),
                loser,
                runtime.tick_index,
                runtime.starvation_states[loser.loser_slot].contended_for(),
                runtime.audit_context.as_ref(),
            );
            super::mark_reflex_starved(runtime, loser.loser_slot);
        }
    }

    for index in 0..runtime.starvation_states.len() {
        if losing_slots.contains(&index) {
            continue;
        }
        runtime.starvation_states[index].reset();
        if controls.get(index).is_some_and(|control| control.active) {
            super::mark_reflex_active_if_starved(runtime, index);
        }
    }
}

fn publish_starved(
    event_bus: &crate::EventBus,
    audit_db: Option<&synapse_storage::Db>,
    loser: &ConflictLoser,
    tick_index: u64,
    starved_for: Duration,
    audit_context: Option<&synapse_core::StoredAuditContext>,
) {
    let starved_for_ms = u64::try_from(starved_for.as_millis()).unwrap_or(u64::MAX);
    let event = Event {
        seq: tick_index,
        at: Utc::now(),
        source: EventSource::Reflex,
        kind: REFLEX_STARVED_KIND.to_owned(),
        data: json!({
            "code": error_codes::REFLEX_STARVED,
            "reflex_id": loser.loser_reflex_id,
            "winner_reflex_id": loser.winner_reflex_id,
            "resource": loser.resource,
            "tick_index": tick_index,
            "starved_for_ms": starved_for_ms,
        }),
        correlations: Vec::new(),
    };
    let _report = event_bus.publish(event);
    let Some(db) = audit_db else {
        return;
    };
    let audit = StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: loser.loser_reflex_id.clone(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Starved,
        event_id: None,
        audit_context: audit_context.cloned(),
        steps: Vec::new(),
        error_code: Some(error_codes::REFLEX_STARVED.to_owned()),
        details: json!({
            "kind": REFLEX_STARVED_KIND,
            "winner_reflex_id": loser.winner_reflex_id,
            "resource": loser.resource,
            "tick_index": tick_index,
            "starved_for_ms": starved_for_ms,
        }),
        redacted: false,
        redactions: Vec::new(),
    };
    if let Err(error) = write_audit(db, &audit) {
        tracing::warn!(
            component = "reflex_conflict",
            reflex_id = %audit.reflex_id,
            audit_id = %audit.audit_id,
            detail = %error,
            "reflex starvation audit write failed"
        );
    }
}

fn dispatch_actions(
    runtime: &mut RuntimeState,
    reflex_id: &ReflexId,
    actions: Vec<Action>,
) -> ReflexResult<usize> {
    let mut dispatched = 0_usize;
    for action in actions {
        let action_count = dispatch_reflex_action(runtime, reflex_id, action)?;
        dispatched = dispatched.saturating_add(action_count);
    }
    Ok(dispatched)
}

fn record_tick_sample(
    runtime: &mut RuntimeState,
    elapsed: Duration,
    degraded: bool,
    event_count: usize,
    dispatched_actions: usize,
    dispatch_blocked: bool,
) {
    let elapsed_us = duration_us(elapsed);
    let target_us = duration_us(runtime.config.target_interval);
    let jitter_us = elapsed_us.abs_diff(target_us);
    let jitter_metric = f64::from(u32::try_from(jitter_us).unwrap_or(u32::MAX));
    metrics::histogram!(REFLEX_TICK_JITTER_METRIC).record(jitter_metric);
    let deadline_late = elapsed > runtime.config.late_after;
    let late = deadline_late || dispatch_blocked;
    if late {
        let reason = if dispatch_blocked {
            "dispatch_blocked"
        } else {
            "deadline_miss"
        };
        let signal = TickLateSignal { reason, degraded };
        if runtime.last_tick_late_signal != Some(signal) {
            emit_tick_late(runtime, elapsed_us, jitter_us, reason, degraded);
        }
        runtime.last_tick_late_signal = Some(signal);
    } else {
        runtime.last_tick_late_signal = None;
    }

    let sample = TickSample {
        tick_index: runtime.tick_index,
        elapsed_us,
        jitter_us,
        target_us,
        pulled_events: event_count,
        dispatched_actions,
        late,
        degraded,
    };
    tracing::trace!(
        component = "reflex_scheduler",
        tick_index = sample.tick_index,
        elapsed_us = sample.elapsed_us,
        jitter_us = sample.jitter_us,
        target_us = sample.target_us,
        pulled_events = sample.pulled_events,
        dispatched_actions = sample.dispatched_actions,
        late = sample.late,
        degraded = sample.degraded,
        "reflex scheduler tick"
    );
    push_sample(&runtime.samples, runtime.config.sample_limit, sample);
    runtime.tick_index = runtime.tick_index.saturating_add(1);
}

fn emit_tick_late(
    runtime: &RuntimeState,
    elapsed_us: u64,
    jitter_us: u64,
    reason: &str,
    degraded: bool,
) {
    let event = Event {
        seq: runtime.tick_index,
        at: Utc::now(),
        source: EventSource::Reflex,
        kind: REFLEX_TICK_LATE_KIND.to_owned(),
        data: json!({
            "code": error_codes::REFLEX_TICK_LATE,
            "elapsed_us": elapsed_us,
            "jitter_us": jitter_us,
            "target_us": duration_us(runtime.config.target_interval),
            "reason": reason,
            "degraded": degraded,
        }),
        correlations: Vec::new(),
    };
    let _report = runtime.event_bus.publish(event);
    write_tick_late_audit(runtime, elapsed_us, jitter_us, reason, degraded);
}

fn write_tick_late_audit(
    runtime: &RuntimeState,
    elapsed_us: u64,
    jitter_us: u64,
    reason: &str,
    degraded: bool,
) {
    let Some(db) = runtime.audit_db.as_deref() else {
        return;
    };
    let audit = StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: "__scheduler__".to_owned(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Active,
        event_id: None,
        audit_context: runtime.audit_context.clone(),
        steps: Vec::new(),
        error_code: Some(error_codes::REFLEX_TICK_LATE.to_owned()),
        details: json!({
            "kind": REFLEX_TICK_LATE_KIND,
            "tick_index": runtime.tick_index,
            "elapsed_us": elapsed_us,
            "jitter_us": jitter_us,
            "target_us": duration_us(runtime.config.target_interval),
            "late_after_us": duration_us(runtime.config.late_after),
            "fallback_interval_us": duration_us(runtime.config.fallback_interval),
            "reason": reason,
            "degraded": degraded,
        }),
        redacted: false,
        redactions: Vec::new(),
    };
    if let Err(error) = write_audit(db, &audit) {
        tracing::warn!(
            component = "reflex_scheduler",
            audit_id = %audit.audit_id,
            detail = %error,
            "reflex tick-late audit write failed"
        );
    }
}

fn warn_dispatch_blocked(reflex_id: &ReflexId, error: &ReflexError) {
    tracing::warn!(
        component = "reflex_scheduler",
        reflex_id = %reflex_id,
        error_code = error.code(),
        detail = %error,
        "reflex action dispatch blocked"
    );
}

fn mark_dispatch_error(runtime: &RuntimeState, index: usize, error: &ReflexError) {
    if error.code() == error_codes::REFLEX_ACTION_PERMISSION_DENIED {
        super::mark_reflex_action_denied(runtime, index);
    } else {
        super::mark_reflex_error(runtime, index, error.code());
    }
}

fn push_sample(
    samples: &Arc<Mutex<VecDeque<TickSample>>>,
    sample_limit: usize,
    sample: TickSample,
) {
    let mut samples = lock_samples(samples);
    if samples.len() >= sample_limit {
        let _oldest = samples.pop_front();
    }
    samples.push_back(sample);
}

fn lock_samples(
    samples: &Arc<Mutex<VecDeque<TickSample>>>,
) -> std::sync::MutexGuard<'_, VecDeque<TickSample>> {
    match samples.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn now_ts_ns() -> u64 {
    Utc::now()
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}
