use std::{
    error::Error,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::json;
use synapse_action::{ACTION_QUEUE_CAPACITY, ActionHandle, ActionMessage};
use synapse_core::{
    Action, Backend, ButtonAction, DetectedEntity, Event, EventFilter, EventSource, Key, KeyCode,
    MouseButton, Point, Rect, ReflexLifetime, ReflexState, SCHEMA_VERSION, StoredReflexAudit,
    error_codes,
};
use synapse_reflex::{
    AimTrackParams, AimTrackTarget, AimTrackTargetSnapshot, AimTrackTargetSource,
    DEFAULT_REFLEX_PRIORITY, EventBus, HoldMoveParams, REFLEX_ACTION_DENIED_STEP_STATUS,
    REFLEX_ACTION_PERMISSION_DENIED_KIND, REFLEX_AIM_TRACK_CORRECTION_KIND, REFLEX_DEBOUNCED_KIND,
    REFLEX_LIFETIME_EXPIRED_KIND, REFLEX_RECURSION_LIMIT_KIND, REFLEX_STARVED_KIND,
    REFLEX_TICK_LATE_KIND, REFLEX_TRACK_LOST_KIND, ReflexActionGate, ReflexActionPermissionDenied,
    ReflexScheduler, ScheduledReflex, ScheduledReflexDriver, SchedulerConfig, SchedulerTrigger,
};
use synapse_storage::{Db, cf, decode_json};
use tempfile::tempdir;

const WAIT_TIMEOUT: Duration = Duration::from_secs(3);

#[test]
fn zero_reflexes_tick_fires_without_dispatch() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    assert_eq!(action_rx.len(), 0);

    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        Vec::new(),
        SchedulerConfig::default().with_max_ticks(24),
    )?;
    let samples = scheduler.wait_for_samples(24, WAIT_TIMEOUT);
    scheduler.stop()?;

    assert_eq!(samples.len(), 24);
    assert!(samples.iter().all(|sample| sample.dispatched_actions == 0));
    assert_eq!(action_rx.len(), 0);
    Ok(())
}

#[test]
fn on_event_reflex_pulls_bus_event_and_dispatches() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event(
        "reflex-on-event",
        EventFilter::Kind {
            kind: "wanted".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    assert_eq!(action_rx.len(), 0);
    let mut scheduler = ReflexScheduler::spawn(
        bus.clone(),
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(8),
    )?;
    let _report = bus.publish(event(1, "wanted"));
    let samples = scheduler.wait_for_samples(8, WAIT_TIMEOUT);
    scheduler.stop()?;

    let pulled = samples
        .iter()
        .map(|sample| sample.pulled_events)
        .sum::<usize>();
    let dispatched = samples
        .iter()
        .map(|sample| sample.dispatched_actions)
        .sum::<usize>();

    assert!(pulled >= 1);
    assert_eq!(dispatched, 1);
    assert_eq!(action_rx.len(), 1);
    Ok(())
}

#[test]
fn on_event_ticks_do_not_sample_aim_track_target_source() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, _action_rx) = ActionHandle::channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn AimTrackTargetSource> = Arc::new(CountingAimTrackSource {
        calls: Arc::clone(&calls),
    });
    let reflex = ScheduledReflex::on_event(
        "reflex-on-event-no-aim-source",
        EventFilter::Kind {
            kind: "wanted".to_owned(),
        },
        vec![Action::ReleaseAll],
    );

    let mut scheduler = ReflexScheduler::spawn_with_audit_db_context_and_aim_track_source(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(3),
        db,
        None,
        source,
    )?;
    let samples = scheduler.wait_for_samples(3, WAIT_TIMEOUT);
    scheduler.stop()?;

    assert_eq!(samples.len(), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn one_shot_on_event_expires_after_first_fire() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event(
        "reflex-one-shot",
        EventFilter::Kind {
            kind: "once".to_owned(),
        },
        vec![Action::ReleaseAll],
    )
    .with_lifetime(ReflexLifetime::OneShot);
    assert_eq!(action_rx.len(), 0);

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_ticks_config(16),
        std::sync::Arc::clone(&db),
    )?;
    let _report = bus.publish(event(1, "once"));
    assert!(wait_for_status(
        &scheduler,
        "reflex-one-shot",
        ReflexState::Expired,
        WAIT_TIMEOUT
    ));
    let _report = bus.publish(event(2, "once"));
    let samples = scheduler.wait_for_samples(16, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let statuses = scheduler.statuses();
    let status = status(&statuses, "reflex-one-shot")?;
    let audits = read_audits(&db)?;
    assert_eq!(samples.len(), 16);
    assert_eq!(action_rx.len(), 1);
    assert_eq!(status.state, ReflexState::Expired);
    assert_eq!(status.fire_count, 1);
    assert!(audits.iter().any(|audit| {
        audit.status == ReflexState::Expired
            && audit.details["kind"].as_str() == Some(REFLEX_LIFETIME_EXPIRED_KIND)
    }));
    Ok(())
}

#[test]
fn on_event_recursion_guard_limits_same_tick_firings_and_audits() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let recursion_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_RECURSION_LIMIT_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event(
        "reflex-recursion",
        EventFilter::Kind {
            kind: "loop".to_owned(),
        },
        vec![Action::ReleaseAll],
    );
    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_one_tick_config(),
        std::sync::Arc::clone(&db),
    )?;
    for seq in 1..=5 {
        let _report = bus.publish(event(seq, "loop"));
    }
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let audits = db
        .scan_cf(cf::CF_REFLEX_AUDIT)?
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    let fired = audits
        .iter()
        .filter(|audit| audit.error_code.is_none())
        .count();
    let limited = audits
        .iter()
        .filter(|audit| audit.error_code.as_deref() == Some(error_codes::REFLEX_RECURSION_LIMIT))
        .count();

    assert_eq!(samples.len(), 1);
    assert_eq!(action_rx.len(), 4);
    assert_eq!(recursion_events.drain().len(), 1);
    assert_eq!(fired, 4);
    assert_eq!(limited, 1);
    Ok(())
}

#[test]
fn on_event_debounce_suppresses_same_tick_duplicates() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let debounced_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_DEBOUNCED_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event_with_debounce(
        "reflex-debounced",
        EventFilter::Kind {
            kind: "debounced".to_owned(),
        },
        vec![Action::ReleaseAll],
        Duration::from_secs(1),
    );
    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_one_tick_config(),
        std::sync::Arc::clone(&db),
    )?;
    let _report = bus.publish(event(1, "debounced"));
    let _report = bus.publish(event(2, "debounced"));
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let audits = read_audits(&db)?;
    let fired = audits
        .iter()
        .filter(|audit| audit.details["kind"].as_str() == Some(synapse_reflex::REFLEX_FIRED_KIND))
        .count();
    let debounced = audits
        .iter()
        .find(|audit| audit.details["kind"].as_str() == Some(REFLEX_DEBOUNCED_KIND))
        .ok_or_else(|| io::Error::other("missing debounce audit"))?;

    assert_eq!(samples.len(), 1);
    assert_eq!(action_rx.len(), 1);
    assert_eq!(debounced_events.drain().len(), 1);
    assert_eq!(fired, 1);
    assert_eq!(
        debounced.error_code.as_deref(),
        Some(error_codes::REFLEX_DEBOUNCED)
    );
    assert_eq!(debounced.details["reason"], "same_tick");
    assert_eq!(debounced.details["suppressed_count"], 1);
    Ok(())
}

#[test]
fn on_event_debounce_audits_later_window_suppression() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event_with_debounce(
        "reflex-debounce-window",
        EventFilter::Kind {
            kind: "debounce-window".to_owned(),
        },
        vec![Action::ReleaseAll],
        Duration::from_secs(5),
    );
    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_ticks_config(8),
        std::sync::Arc::clone(&db),
    )?;
    let _report = bus.publish(event(1, "debounce-window"));
    assert!(wait_for_fire_count(
        &scheduler,
        "reflex-debounce-window",
        1,
        WAIT_TIMEOUT
    ));
    let _report = bus.publish(event(2, "debounce-window"));
    let samples = scheduler.wait_for_samples(8, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let audits = read_audits(&db)?;
    let debounced = audits
        .iter()
        .find(|audit| {
            audit.details["kind"].as_str() == Some(REFLEX_DEBOUNCED_KIND)
                && audit.details["reason"].as_str() == Some("debounce_window")
        })
        .ok_or_else(|| io::Error::other("missing debounce-window audit"))?;

    assert_eq!(samples.len(), 8);
    assert_eq!(action_rx.len(), 1);
    assert_eq!(
        debounced.error_code.as_deref(),
        Some(error_codes::REFLEX_DEBOUNCED)
    );
    assert_eq!(debounced.details["suppressed_count"], 1);
    Ok(())
}

#[test]
fn on_event_until_event_lifetime_expires_before_future_triggers() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let lifetime_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_LIFETIME_EXPIRED_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event(
        "reflex-until-event",
        EventFilter::Kind {
            kind: "fire".to_owned(),
        },
        vec![Action::ReleaseAll],
    )
    .with_lifetime(ReflexLifetime::UntilEvent {
        filter: EventFilter::Kind {
            kind: "stop".to_owned(),
        },
    });
    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_ticks_config(10),
        std::sync::Arc::clone(&db),
    )?;
    let _report = bus.publish(event(1, "fire"));
    assert!(wait_for_fire_count(
        &scheduler,
        "reflex-until-event",
        1,
        WAIT_TIMEOUT
    ));
    let _report = bus.publish(event(2, "stop"));
    assert!(wait_for_status(
        &scheduler,
        "reflex-until-event",
        ReflexState::Expired,
        WAIT_TIMEOUT
    ));
    let _report = bus.publish(event(3, "fire"));
    let samples = scheduler.wait_for_samples(10, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let statuses = scheduler.statuses();
    let status = status(&statuses, "reflex-until-event")?;
    let audits = read_audits(&db)?;
    let lifetime_audit = audits
        .iter()
        .find(|audit| {
            audit.error_code.as_deref() == Some(error_codes::REFLEX_LIFETIME_EXPIRED)
                && audit.details["reason"].as_str() == Some("event")
        })
        .ok_or_else(|| io::Error::other("missing UntilEvent lifetime audit"))?;
    let lifetime_event = lifetime_events
        .drain()
        .into_iter()
        .find(|event| event.data["reason"] == "event")
        .ok_or_else(|| io::Error::other("missing UntilEvent lifetime bus event"))?;

    assert_eq!(samples.len(), 10);
    assert_eq!(action_rx.len(), 1);
    assert_eq!(status.state, ReflexState::Expired);
    assert_eq!(lifetime_audit.details["kind"], REFLEX_LIFETIME_EXPIRED_KIND);
    assert_eq!(
        lifetime_event.data["code"],
        error_codes::REFLEX_LIFETIME_EXPIRED
    );
    Ok(())
}

#[test]
fn scheduler_runs_hold_move_duration_driver_to_keyup() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let key = named_key("w");
    let reflex =
        ScheduledReflex::hold_move("scheduler-hold-move", HoldMoveParams::new(key.clone()))
            .with_lifetime(ReflexLifetime::Duration { ms: 10 });

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![reflex],
        slow_ticks_config(8),
        std::sync::Arc::clone(&db),
    )?;
    assert!(wait_for_status(
        &scheduler,
        "scheduler-hold-move",
        ReflexState::Expired,
        WAIT_TIMEOUT
    ));
    let samples = scheduler.wait_for_samples(8, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let actions = drain_actions(&mut action_rx);
    let audits = read_audits(&db)?;

    assert_eq!(samples.len(), 8);
    assert_eq!(
        actions,
        vec![
            Action::KeyDown {
                key: key.clone(),
                backend: Backend::Software,
            },
            Action::KeyUp {
                key,
                backend: Backend::Software,
            },
        ]
    );
    assert!(audits.iter().any(|audit| {
        audit.status == ReflexState::Expired
            && audit.error_code.as_deref() == Some(error_codes::REFLEX_LIFETIME_EXPIRED)
    }));
    Ok(())
}

#[test]
fn thirty_two_reflexes_fire_same_tick_without_tick_late() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let late_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_TICK_LATE_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflexes = (0..32)
        .map(|index| {
            ScheduledReflex::every_tick(format!("reflex-{index:02}"), vec![Action::ReleaseAll])
        })
        .collect::<Vec<_>>();
    assert_eq!(reflexes.len(), 32);
    assert_eq!(action_rx.len(), 0);
    assert_eq!(late_events.len(), 0);

    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        reflexes,
        SchedulerConfig::default().with_max_ticks(1),
    )?;
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;

    let late = late_events.drain();
    let Some(sample) = samples.first().copied() else {
        return Err(Box::new(io::Error::other(
            "scheduler did not record the expected tick sample",
        )));
    };

    assert_eq!(sample.dispatched_actions, 32);
    assert!(!sample.late);
    assert_eq!(action_rx.len(), 32);
    assert!(late.is_empty());
    Ok(())
}

#[test]
fn blocked_dispatch_path_emits_reflex_tick_late() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let late_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_TICK_LATE_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, action_rx) = ActionHandle::channel();
    for _ in 0..ACTION_QUEUE_CAPACITY {
        action_handle.try_execute(Action::ReleaseAll)?;
    }
    assert_eq!(action_rx.len(), ACTION_QUEUE_CAPACITY);
    assert_eq!(late_events.len(), 0);

    let reflex = ScheduledReflex::every_tick("reflex-blocked", vec![Action::ReleaseAll]);
    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(1),
        std::sync::Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let late = late_events.drain();
    let audits = read_audits(&db)?;
    let Some(sample) = samples.first().copied() else {
        return Err(Box::new(io::Error::other(
            "scheduler did not record blocked-dispatch sample",
        )));
    };

    assert_eq!(action_rx.len(), ACTION_QUEUE_CAPACITY);
    assert_eq!(sample.dispatched_actions, 0);
    assert!(sample.late);
    assert_eq!(late.len(), 1);
    assert_eq!(late[0].data["code"], error_codes::REFLEX_TICK_LATE);
    assert_eq!(late[0].data["reason"], "dispatch_blocked");
    let tick_late_audit = audits
        .iter()
        .find(|audit| audit.error_code.as_deref() == Some(error_codes::REFLEX_TICK_LATE))
        .ok_or_else(|| io::Error::other("missing persisted tick-late audit row"))?;
    assert_eq!(tick_late_audit.reflex_id, "__scheduler__");
    assert_eq!(tick_late_audit.details["kind"], REFLEX_TICK_LATE_KIND);
    assert_eq!(tick_late_audit.details["reason"], "dispatch_blocked");
    assert_eq!(tick_late_audit.details["degraded"], false);
    assert!(tick_late_audit.details["elapsed_us"].as_u64().is_some());
    assert!(tick_late_audit.details["jitter_us"].as_u64().is_some());
    Ok(())
}

#[test]
fn consecutive_tick_late_signals_are_coalesced() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let late_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_TICK_LATE_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, mut action_rx) = ActionHandle::channel();
    for _ in 0..ACTION_QUEUE_CAPACITY {
        action_handle.try_execute(Action::ReleaseAll)?;
    }

    let reflex = ScheduledReflex::every_tick("reflex-blocked", vec![Action::ReleaseAll]);
    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(3),
        std::sync::Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(3, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let late = late_events.drain();
    let tick_late_audit_count = read_audits(&db)?
        .into_iter()
        .filter(|audit| audit.error_code.as_deref() == Some(error_codes::REFLEX_TICK_LATE))
        .count();
    let queued_actions = drain_actions(&mut action_rx);

    assert_eq!(samples.len(), 3);
    assert!(samples.iter().all(|sample| sample.late));
    assert_eq!(late.len(), 1);
    assert_eq!(tick_late_audit_count, 1);
    assert_eq!(
        queued_actions.len(),
        ACTION_QUEUE_CAPACITY,
        "blocked ticks must not enqueue additional actions"
    );
    Ok(())
}

#[test]
fn action_gate_denies_triggered_reflex_and_writes_action_denied_audit() -> Result<(), Box<dyn Error>>
{
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let key = named_key("d");
    let reflex = ScheduledReflex::on_event(
        "reflex-denied",
        EventFilter::Kind {
            kind: "deny-trigger".to_owned(),
        },
        vec![Action::KeyDown {
            key,
            backend: Backend::Software,
        }],
    );

    let mut scheduler = ReflexScheduler::spawn_with_audit_db_context_and_action_gate(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_one_tick_config(),
        Arc::clone(&db),
        None,
        Arc::new(DenyUnknownScopeGate),
    )?;
    let _report = bus.publish(event(1, "deny-trigger"));
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let statuses = scheduler.statuses();
    let status = status(&statuses, "reflex-denied")?;
    let audits = read_audits(&db)?;
    let denied = audits
        .iter()
        .find(|audit| {
            audit.reflex_id == "reflex-denied"
                && audit.status == ReflexState::ActionDenied
                && audit.error_code.as_deref() == Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED)
        })
        .ok_or_else(|| io::Error::other("missing action-denied audit row"))?;

    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].dispatched_actions, 0);
    assert!(samples[0].late);
    assert_eq!(action_rx.len(), 0);
    assert_eq!(status.state, ReflexState::ActionDenied);
    assert_eq!(
        status.last_error_code.as_deref(),
        Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED)
    );
    assert_eq!(denied.details["kind"], REFLEX_ACTION_PERMISSION_DENIED_KIND);
    assert_eq!(
        denied.details["reason"],
        error_codes::REFLEX_ACTION_PERMISSION_DENIED
    );
    assert_eq!(denied.details["policy_reason"], "unknown_scope");
    assert_eq!(denied.details["profile_id"], "unknown");
    assert_eq!(denied.details["use_scope"], "unknown");
    assert_eq!(denied.steps.len(), 1);
    assert_eq!(denied.steps[0].status, REFLEX_ACTION_DENIED_STEP_STATUS);
    assert_eq!(
        denied.steps[0].error_code.as_deref(),
        Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED)
    );
    Ok(())
}

#[test]
fn action_gate_denies_combo_reflex_before_first_step() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let key = named_key("c");
    let reflex = ScheduledReflex::on_event(
        "reflex-denied-combo",
        EventFilter::Kind {
            kind: "deny-combo".to_owned(),
        },
        vec![Action::Combo {
            steps: vec![synapse_core::ComboStep {
                at_ms: 0,
                input: synapse_core::ComboInput::KeyDown { key },
            }],
            backend: Backend::Software,
        }],
    );

    let mut scheduler = ReflexScheduler::spawn_with_audit_db_context_and_action_gate(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_one_tick_config(),
        Arc::clone(&db),
        None,
        Arc::new(DenyUnknownScopeGate),
    )?;
    let _report = bus.publish(event(1, "deny-combo"));
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let audits = read_audits(&db)?;
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].dispatched_actions, 0);
    assert!(drain_actions(&mut action_rx).is_empty());
    assert!(audits.iter().any(|audit| {
        audit.reflex_id == "reflex-denied-combo"
            && audit.status == ReflexState::ActionDenied
            && audit.details["action_kind"] == "combo"
            && audit.error_code.as_deref() == Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED)
    }));
    Ok(())
}

#[test]
fn action_gate_denial_without_audit_db_still_suppresses_dispatch() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event(
        "reflex-denied-no-db",
        EventFilter::Kind {
            kind: "deny-no-db".to_owned(),
        },
        vec![Action::ReleaseAll],
    );

    let mut scheduler = ReflexScheduler::spawn_with_action_gate(
        bus.clone(),
        action_handle,
        vec![reflex],
        slow_one_tick_config(),
        Arc::new(DenyUnknownScopeGate),
    )?;
    let _report = bus.publish(event(1, "deny-no-db"));
    let samples = scheduler.wait_for_samples(1, WAIT_TIMEOUT);
    scheduler.stop()?;

    let statuses = scheduler.statuses();
    let status = status(&statuses, "reflex-denied-no-db")?;
    assert_eq!(samples.len(), 1);
    assert_eq!(action_rx.len(), 0);
    assert_eq!(status.state, ReflexState::ActionDenied);
    assert_eq!(
        status.last_error_code.as_deref(),
        Some(error_codes::REFLEX_ACTION_PERMISSION_DENIED)
    );
    Ok(())
}

#[test]
fn lower_priority_number_wins_cursor_conflict_and_starves_loser() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let starved_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_STARVED_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let mut winner = mouse_move_reflex("reflex-priority-winner", 10.0);
    winner.priority = 10;
    let mut loser = mouse_move_reflex("reflex-priority-loser", 99.0);
    loser.priority = 100;

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![winner, loser],
        slow_ticks_config(45),
        std::sync::Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(45, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let actions = drain_actions(&mut action_rx);
    let statuses = scheduler.statuses();
    let audits = read_audits(&db)?;
    let starved_audits = audits
        .iter()
        .filter(|audit| audit.error_code.as_deref() == Some(error_codes::REFLEX_STARVED))
        .count();

    assert_eq!(samples.len(), 45);
    assert_eq!(actions.len(), 45);
    assert!(actions.iter().all(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, .. } if dx_is(*dx, 10.0)
    )));
    assert_eq!(starved_events.drain().len(), 1);
    assert_eq!(starved_audits, 1);
    let loser_status = status(&statuses, "reflex-priority-loser")?;
    assert_eq!(loser_status.state, ReflexState::Starved);
    assert_eq!(
        loser_status.last_error_code.as_deref(),
        Some(error_codes::REFLEX_STARVED)
    );
    let winner_status = status(&statuses, "reflex-priority-winner")?;
    assert_eq!(winner_status.fire_count, 45);
    Ok(())
}

#[test]
fn equal_priority_cursor_conflict_prefers_newer_registration() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let mut older = mouse_move_reflex("reflex-older", 1.0);
    older.priority = 50;
    let mut newer = mouse_move_reflex("reflex-newer", 2.0);
    newer.priority = 50;

    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        vec![older, newer],
        slow_ticks_config(45),
    )?;
    let samples = scheduler.wait_for_samples(45, WAIT_TIMEOUT);
    scheduler.stop()?;

    let actions = drain_actions(&mut action_rx);
    let statuses = scheduler.statuses();

    assert_eq!(samples.len(), 45);
    assert_eq!(actions.len(), 45);
    assert!(actions.iter().all(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, .. } if dx_is(*dx, 2.0)
    )));
    assert_eq!(
        status(&statuses, "reflex-older")?.state,
        ReflexState::Starved
    );
    assert_eq!(status(&statuses, "reflex-newer")?.fire_count, 45);
    Ok(())
}

#[test]
fn exclusive_mouse_reflex_blocks_lower_priority_same_device_class() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let mut cursor = mouse_move_reflex("reflex-exclusive-cursor", 7.0).with_exclusive(true);
    cursor.priority = 10;
    let mut button = ScheduledReflex::every_tick(
        "reflex-exclusive-button",
        vec![Action::MouseButton {
            button: MouseButton::Left,
            action: ButtonAction::Down,
            hold_ms: 0,
            backend: Backend::Software,
        }],
    )
    .with_exclusive(true);
    button.priority = 100;

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![cursor, button],
        slow_ticks_config(45),
        std::sync::Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(45, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let actions = drain_actions(&mut action_rx);
    let statuses = scheduler.statuses();
    let audits = read_audits(&db)?;
    let starved = audits
        .iter()
        .find(|audit| {
            audit.reflex_id == "reflex-exclusive-button"
                && audit.error_code.as_deref() == Some(error_codes::REFLEX_STARVED)
        })
        .ok_or_else(|| io::Error::other("missing exclusive starvation audit row"))?;

    assert_eq!(samples.len(), 45);
    assert_eq!(actions.len(), 45);
    assert!(actions.iter().all(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, .. } if dx_is(*dx, 7.0)
    )));
    assert_eq!(
        status(&statuses, "reflex-exclusive-button")?.state,
        ReflexState::Starved
    );
    assert_eq!(starved.details["resource"], "exclusive:mouse");
    Ok(())
}

#[test]
fn stateful_aim_track_conflicts_by_priority_and_starves_loser() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = std::sync::Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let starved_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_STARVED_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let mut winner = ScheduledReflex::aim_track(
        "stateful-aim-winner",
        aim_track_params(point_value(20_000, 20_000)),
    )
    .with_exclusive(true);
    winner.priority = 10;
    let mut loser = ScheduledReflex::aim_track(
        "stateful-aim-loser",
        aim_track_params(point_value(-20_000, -20_000)),
    )
    .with_exclusive(true);
    loser.priority = 100;

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![winner, loser],
        slow_ticks_config(45),
        std::sync::Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(45, WAIT_TIMEOUT);
    scheduler.stop()?;
    db.flush()?;

    let actions = drain_actions(&mut action_rx);
    let statuses = scheduler.statuses();
    let audits = read_audits(&db)?;
    let starved = audits
        .iter()
        .find(|audit| {
            audit.reflex_id == "stateful-aim-loser"
                && audit.error_code.as_deref() == Some(error_codes::REFLEX_STARVED)
        })
        .ok_or_else(|| io::Error::other("missing stateful aim starvation audit row"))?;

    assert_eq!(samples.len(), 45);
    assert_eq!(actions.len(), 45);
    assert!(actions.iter().all(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, dy, .. } if *dx > 0.0 && *dy > 0.0
    )));
    assert_eq!(starved_events.drain().len(), 1);
    assert_eq!(
        status(&statuses, "stateful-aim-loser")?.state,
        ReflexState::Starved
    );
    assert_eq!(status(&statuses, "stateful-aim-loser")?.fire_count, 0);
    assert_eq!(status(&statuses, "stateful-aim-winner")?.fire_count, 45);
    assert_eq!(starved.details["resource"], "mouse_cursor");
    Ok(())
}

#[test]
fn aim_track_uses_dynamic_target_source_and_audits_corrections_and_loss()
-> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db = Arc::new(Db::open(&temp.path().join("db"), SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let lost_events = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_TRACK_LOST_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let source: Arc<dyn AimTrackTargetSource> = Arc::new(ScriptedMovingTrackSource::new(12));
    let mut params = AimTrackParams::new(AimTrackTarget::TrackId(42));
    params.deadzone_px = 0.0;
    params.max_speed_px_per_tick = 3.0;
    params.ema_alpha = 1.0;
    let reflex = ScheduledReflex::aim_track("dynamic-aim-track", params);

    let mut scheduler = ReflexScheduler::spawn_with_audit_db_context_and_aim_track_source(
        bus,
        action_handle,
        vec![reflex],
        slow_ticks_config(40),
        Arc::clone(&db),
        None,
        source,
    )?;
    assert!(wait_for_status(
        &scheduler,
        "dynamic-aim-track",
        ReflexState::Expired,
        Duration::from_secs(5)
    ));
    let samples = scheduler.wait_for_samples(40, Duration::from_secs(5));
    scheduler.stop()?;
    db.flush()?;

    let actions = drain_actions(&mut action_rx);
    let statuses = scheduler.statuses();
    let dynamic_status = status(&statuses, "dynamic-aim-track")?;
    let audits = read_audits(&db)?;
    let correction_audits = audits
        .iter()
        .filter(|audit| audit.details["kind"] == REFLEX_AIM_TRACK_CORRECTION_KIND)
        .collect::<Vec<_>>();
    let lost_audit = audits
        .iter()
        .find(|audit| {
            audit.reflex_id == "dynamic-aim-track"
                && audit.error_code.as_deref() == Some(error_codes::REFLEX_TRACK_LOST)
        })
        .ok_or_else(|| io::Error::other("missing track-lost audit row"))?;

    assert_eq!(samples.len(), 40);
    assert!(!actions.is_empty());
    assert_eq!(correction_audits.len(), actions.len());
    assert!(actions.iter().all(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, dy, .. } if (*dx).hypot(*dy) <= 3.01
    )));
    assert_eq!(dynamic_status.state, ReflexState::Expired);
    assert_eq!(
        dynamic_status.last_error_code.as_deref(),
        Some(error_codes::REFLEX_TRACK_LOST)
    );
    assert_eq!(lost_events.drain().len(), 1);

    let first_correction = correction_audits
        .first()
        .ok_or_else(|| io::Error::other("missing correction audit row"))?;
    assert_eq!(
        first_correction.details["target_context"]["source_label"],
        "scripted_track_source"
    );
    assert_eq!(
        first_correction.details["target_context"]["entity_track_ids"],
        json!([42])
    );
    assert_eq!(first_correction.steps.len(), 1);
    assert_eq!(first_correction.steps[0].status, "dispatched");
    assert_eq!(lost_audit.details["kind"], REFLEX_TRACK_LOST_KIND);
    assert!(
        lost_audit.details["lost_for_ms"]
            .as_u64()
            .unwrap_or_default()
            >= u64::try_from(synapse_reflex::TRACK_LOST_AFTER.as_millis()).unwrap_or(u64::MAX)
    );
    assert_eq!(lost_audit.details["target_context"]["entity_count"], 0);
    Ok(())
}

#[test]
fn priority_change_is_used_on_later_ticks() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let mut first = mouse_move_reflex("reflex-first", 1.0);
    first.priority = 10;
    let mut second = mouse_move_reflex("reflex-second", 2.0);
    second.priority = 100;

    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        vec![first, second],
        slow_ticks_config(8),
    )?;
    let before = scheduler.wait_for_samples(2, WAIT_TIMEOUT);
    assert!(before.len() >= 2);
    assert!(scheduler.set_priority("reflex-second", 1));
    let samples = scheduler.wait_for_samples(8, WAIT_TIMEOUT);
    scheduler.stop()?;

    let actions = drain_actions(&mut action_rx);
    assert_eq!(samples.len(), 8);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, .. } if dx_is(*dx, 1.0)
    )));
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, .. } if dx_is(*dx, 2.0)
    )));
    assert_eq!(status(&scheduler.statuses(), "reflex-second")?.priority, 1);
    Ok(())
}

#[test]
fn cancelling_winner_allows_starved_loser_to_fire_again() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let mut winner = mouse_move_reflex("reflex-cancel-winner", 3.0);
    winner.priority = 1;
    let mut loser = mouse_move_reflex("reflex-resumes", 4.0);
    loser.priority = 100;

    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        vec![winner, loser],
        slow_ticks_config(55),
    )?;
    let starved = wait_for_status(
        &scheduler,
        "reflex-resumes",
        ReflexState::Starved,
        WAIT_TIMEOUT,
    );
    assert!(starved);
    assert!(scheduler.cancel_reflex("reflex-cancel-winner"));
    let samples = scheduler.wait_for_samples(55, WAIT_TIMEOUT);
    scheduler.stop()?;

    let actions = drain_actions(&mut action_rx);
    let statuses = scheduler.statuses();
    assert_eq!(samples.len(), 55);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::MouseMoveRelative { dx, .. } if dx_is(*dx, 4.0)
    )));
    assert_eq!(
        status(&statuses, "reflex-cancel-winner")?.state,
        ReflexState::Cancelled
    );
    assert_eq!(
        status(&statuses, "reflex-resumes")?.state,
        ReflexState::Active
    );
    Ok(())
}

#[test]
fn scheduler_rejects_invalid_trigger_filter() {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex {
        reflex_id: "reflex-invalid-filter".to_owned(),
        trigger: SchedulerTrigger::OnEvent(EventFilter::And { args: Vec::new() }),
        then: vec![Action::ReleaseAll],
        driver: ScheduledReflexDriver::Actions,
        priority: 0,
        lifetime: ReflexLifetime::UntilCancelled,
        exclusive: false,
        debounce: Duration::ZERO,
    };
    assert_eq!(action_rx.len(), 0);

    let error = match ReflexScheduler::spawn(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default(),
    ) {
        Ok(_scheduler) => panic!("invalid event filter must prevent scheduler spawn"),
        Err(error) => error,
    };

    assert_eq!(error.code(), error_codes::REFLEX_FILTER_INVALID);
    assert_eq!(action_rx.len(), 0);
}

#[test]
fn scheduler_rejects_invalid_lifetime_filter() {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::on_event(
        "reflex-invalid-lifetime-filter",
        EventFilter::Kind {
            kind: "fire".to_owned(),
        },
        vec![Action::ReleaseAll],
    )
    .with_lifetime(ReflexLifetime::UntilEvent {
        filter: EventFilter::And { args: Vec::new() },
    });

    let error = match ReflexScheduler::spawn(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default(),
    ) {
        Ok(_scheduler) => panic!("invalid lifetime filter must prevent scheduler spawn"),
        Err(error) => error,
    };

    assert_eq!(error.code(), error_codes::REFLEX_FILTER_INVALID);
    assert_eq!(action_rx.len(), 0);
}

#[test]
fn scheduler_rejects_duplicate_reflex_ids() {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let reflexes = vec![
        ScheduledReflex::every_tick("reflex-duplicate-id", vec![Action::ReleaseAll]),
        ScheduledReflex::every_tick("reflex-duplicate-id", vec![Action::ReleaseAll]),
    ];

    let error =
        match ReflexScheduler::spawn(bus, action_handle, reflexes, SchedulerConfig::default()) {
            Ok(_scheduler) => panic!("duplicate reflex id must prevent scheduler spawn"),
            Err(error) => error,
        };

    assert_eq!(error.code(), error_codes::REFLEX_PARAMS_INVALID);
    assert_eq!(action_rx.len(), 0);
}

fn event(seq: u64, kind: &str) -> Event {
    Event {
        seq,
        at: chrono::Utc::now(),
        source: EventSource::System,
        kind: kind.to_owned(),
        data: json!({ "seq": seq, "kind": kind }),
        correlations: Vec::new(),
    }
}

struct DenyUnknownScopeGate;

impl ReflexActionGate for DenyUnknownScopeGate {
    fn ensure_action_allowed(
        &self,
        _reflex_id: &synapse_core::ReflexId,
        _action: &Action,
    ) -> Result<(), ReflexActionPermissionDenied> {
        Err(ReflexActionPermissionDenied {
            policy_code: Some(error_codes::SAFETY_PROFILE_ACTION_DENIED.to_owned()),
            policy_reason: Some("unknown_scope".to_owned()),
            profile_id: Some("unknown".to_owned()),
            use_scope: Some("unknown".to_owned()),
            detail: "active profile has use_scope=\"unknown\"".to_owned(),
        })
    }
}

struct ScriptedMovingTrackSource {
    calls: Mutex<u64>,
    present_calls: u64,
}

impl ScriptedMovingTrackSource {
    const fn new(present_calls: u64) -> Self {
        Self {
            calls: Mutex::new(0),
            present_calls,
        }
    }
}

impl AimTrackTargetSource for ScriptedMovingTrackSource {
    fn snapshot(&self) -> AimTrackTargetSnapshot {
        let call = {
            let mut calls = lock_source_calls(&self.calls);
            *calls = calls.saturating_add(1);
            *calls
        };
        if call > self.present_calls {
            return AimTrackTargetSnapshot {
                source_label: Some("scripted_track_source".to_owned()),
                source_seq: Some(call),
                ..AimTrackTargetSnapshot::default()
            };
        }
        let now = chrono::Utc::now();
        let offset = i32::try_from(call).unwrap_or(i32::MAX).saturating_mul(4);
        AimTrackTargetSnapshot {
            entities: vec![DetectedEntity {
                entity_id: "moving-target".to_owned(),
                track_id: 42,
                class_label: "marker".to_owned(),
                bbox: Rect {
                    x: 20_000_i32.saturating_add(offset),
                    y: 20_000_i32.saturating_add(offset),
                    w: 20,
                    h: 20,
                },
                confidence: 1.0,
                first_seen_at: now,
                last_seen_at: now,
                velocity_px_per_s: Some((240.0, 240.0)),
            }],
            source_label: Some("scripted_track_source".to_owned()),
            source_seq: Some(call),
            source_error: None,
            elements: Vec::new(),
        }
    }
}

struct CountingAimTrackSource {
    calls: Arc<AtomicUsize>,
}

impl AimTrackTargetSource for CountingAimTrackSource {
    fn snapshot(&self) -> AimTrackTargetSnapshot {
        self.calls.fetch_add(1, Ordering::SeqCst);
        AimTrackTargetSnapshot::default()
    }
}

fn lock_source_calls(calls: &Mutex<u64>) -> std::sync::MutexGuard<'_, u64> {
    match calls.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

const fn slow_one_tick_config() -> SchedulerConfig {
    SchedulerConfig {
        target_interval: Duration::from_millis(50),
        fallback_interval: Duration::from_millis(50),
        late_after: Duration::from_millis(250),
        sample_limit: 16,
        max_ticks: Some(1),
        force_degraded: false,
    }
}

const fn slow_ticks_config(max_ticks: u64) -> SchedulerConfig {
    SchedulerConfig {
        target_interval: Duration::from_millis(50),
        fallback_interval: Duration::from_millis(50),
        late_after: Duration::from_millis(250),
        sample_limit: 128,
        max_ticks: Some(max_ticks),
        force_degraded: false,
    }
}

fn mouse_move_reflex(id: &str, dx: f32) -> ScheduledReflex {
    ScheduledReflex::every_tick(
        id,
        vec![Action::MouseMoveRelative {
            dx,
            dy: 0.0,
            backend: Backend::Software,
        }],
    )
}

fn named_key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

const fn aim_track_params(target: Point) -> AimTrackParams {
    let mut params = AimTrackParams::new(AimTrackTarget::Point(target));
    params.deadzone_px = 0.0;
    params.max_speed_px_per_tick = 1.0;
    params.ema_alpha = 1.0;
    params
}

const fn point_value(x: i32, y: i32) -> Point {
    Point { x, y }
}

fn drain_actions(action_rx: &mut tokio::sync::mpsc::Receiver<ActionMessage>) -> Vec<Action> {
    let mut actions = Vec::new();
    while let Ok((action, _ack, _operator_panic_epoch_at_enqueue)) = action_rx.try_recv() {
        actions.push(action);
    }
    actions
}

const fn dx_is(actual: f32, expected: f32) -> bool {
    actual.to_bits() == expected.to_bits()
}

fn read_audits(db: &Db) -> Result<Vec<StoredReflexAudit>, Box<dyn Error>> {
    db.scan_cf(cf::CF_REFLEX_AUDIT)?
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn status<'a>(
    statuses: &'a [synapse_core::ReflexStatus],
    id: &str,
) -> Result<&'a synapse_core::ReflexStatus, Box<dyn Error>> {
    statuses
        .iter()
        .find(|status| status.id == id)
        .ok_or_else(|| Box::new(io::Error::other(format!("missing status {id}"))) as Box<dyn Error>)
}

fn wait_for_status(
    scheduler: &synapse_reflex::SchedulerHandle,
    id: &str,
    expected: ReflexState,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if scheduler
            .statuses()
            .iter()
            .any(|status| status.id == id && status.state == expected)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_fire_count(
    scheduler: &synapse_reflex::SchedulerHandle,
    id: &str,
    expected: u64,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if scheduler
            .statuses()
            .iter()
            .any(|status| status.id == id && status.fire_count >= expected)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn scheduled_reflex_default_priority_matches_priority_adr() {
    let reflex = ScheduledReflex::every_tick("reflex-default-priority", vec![Action::ReleaseAll]);

    assert_eq!(reflex.priority, DEFAULT_REFLEX_PRIORITY);
    assert_eq!(DEFAULT_REFLEX_PRIORITY, 100);
}
