use std::{
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use synapse_action::{ActionHandle, ActionMessage};

use synapse_core::{
    Action, Backend, ComboInput, ComboStep, EventFilter, Key, KeyCode, ReflexState, SCHEMA_VERSION,
    StoredReflexAudit, error_codes,
};
use synapse_reflex::{
    ComboContext, ComboController, ComboOutput, ComboParams, ComboPhase, EventBus,
    REFLEX_COMBO_COMPLETED_KIND, REFLEX_LIFETIME_EXPIRED_KIND, ReflexRuntime, ReflexScheduler,
    ScheduledReflex, SchedulerConfig, install_action_combo_scheduler,
};
use synapse_storage::{Db, cf, decode_json};
use tempfile::tempdir;
use tokio::sync::mpsc;

#[test]
fn combo_keypress_steps_dispatch_at_due_offsets() -> Result<(), Box<dyn Error>> {
    let key_a = named_key("a");
    let key_b = named_key("b");
    let mut controller = ComboController::new(
        "combo-keypress",
        ComboParams::new(
            vec![
                ComboStep {
                    at_ms: 0,
                    input: ComboInput::KeyPress {
                        key: key_a.clone(),
                        hold_ms: 33,
                    },
                },
                ComboStep {
                    at_ms: 100,
                    input: ComboInput::KeyPress {
                        key: key_b.clone(),
                        hold_ms: 33,
                    },
                },
            ],
            Backend::Software,
        ),
    );
    let bus = EventBus::default();
    let (handle, mut rx) = ActionHandle::channel();

    assert!(drain(&mut rx).is_empty());
    assert_eq!(
        controller.start_dispatch(&handle, &bus)?,
        ComboOutput::Started {
            actions: 1,
            remaining: 3
        }
    );
    assert_eq!(
        drain(&mut rx),
        vec![Action::KeyDown {
            key: key_a.clone(),
            backend: Backend::Software,
        }]
    );

    assert_eq!(
        controller.step_dispatch(&context(33), &handle, &bus)?,
        ComboOutput::Dispatched {
            actions: 1,
            elapsed_ms: 33,
            remaining: 2
        }
    );
    assert_eq!(
        drain(&mut rx),
        vec![Action::KeyUp {
            key: key_a,
            backend: Backend::Software,
        }]
    );

    assert_eq!(
        controller.step_dispatch(&context(67), &handle, &bus)?,
        ComboOutput::Dispatched {
            actions: 1,
            elapsed_ms: 100,
            remaining: 1
        }
    );
    assert_eq!(
        drain(&mut rx),
        vec![Action::KeyDown {
            key: key_b.clone(),
            backend: Backend::Software,
        }]
    );

    assert_eq!(
        controller.step_dispatch(&context(33), &handle, &bus)?,
        ComboOutput::Completed {
            scheduled_actions: 4,
            dispatched_actions: 4,
            actions: 1
        }
    );
    assert_eq!(
        drain(&mut rx),
        vec![Action::KeyUp {
            key: key_b,
            backend: Backend::Software,
        }]
    );
    assert_eq!(controller.phase(), ComboPhase::Completed);
    Ok(())
}

#[test]
fn combo_empty_steps_complete_without_dispatch_and_emit_audit() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let subscriber = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_COMBO_COMPLETED_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let mut controller = ComboController::new(
        "combo-empty",
        ComboParams::new(Vec::new(), Backend::Software),
    );
    let (handle, mut rx) = ActionHandle::channel();

    assert_eq!(
        controller.start_dispatch(&handle, &bus)?,
        ComboOutput::Completed {
            scheduled_actions: 0,
            dispatched_actions: 0,
            actions: 0
        }
    );
    assert!(drain(&mut rx).is_empty());
    let events = subscriber.drain();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["status"], "completed");
    assert_eq!(events[0].data["scheduled_actions"], 0);
    assert_eq!(events[0].data["dispatched_actions"], 0);
    Ok(())
}

#[test]
fn combo_single_step_dispatches_one_primitive_action() -> Result<(), Box<dyn Error>> {
    let mut controller = ComboController::new(
        "combo-single",
        ComboParams::new(
            vec![ComboStep {
                at_ms: 0,
                input: ComboInput::MouseMoveRel { dx: 3.0, dy: -2.0 },
            }],
            Backend::Software,
        ),
    );
    let bus = EventBus::default();
    let (handle, mut rx) = ActionHandle::channel();

    assert_eq!(
        controller.start_dispatch(&handle, &bus)?,
        ComboOutput::Completed {
            scheduled_actions: 1,
            dispatched_actions: 1,
            actions: 1
        }
    );
    assert_eq!(
        drain(&mut rx),
        vec![Action::MouseMoveRelative {
            dx: 3.0,
            dy: -2.0,
            backend: Backend::Software,
        }]
    );
    Ok(())
}

#[test]
fn combo_hundred_steps_fire_in_due_order() -> Result<(), Box<dyn Error>> {
    let steps = (0..100_u16)
        .map(|index| ComboStep {
            at_ms: u32::from(index),
            input: ComboInput::KeyDown {
                key: named_key(&format!("k{index:03}")),
            },
        })
        .collect::<Vec<_>>();
    let mut controller =
        ComboController::new("combo-hundred", ComboParams::new(steps, Backend::Software));
    let bus = EventBus::default();
    let (handle, mut rx) = ActionHandle::channel();

    controller.start_dispatch(&handle, &bus)?;
    assert_eq!(
        drain(&mut rx),
        vec![Action::KeyDown {
            key: named_key("k000"),
            backend: Backend::Software,
        }]
    );
    assert_eq!(
        controller.step_dispatch(&context(99), &handle, &bus)?,
        ComboOutput::Completed {
            scheduled_actions: 100,
            dispatched_actions: 100,
            actions: 99
        }
    );

    let observed = drain(&mut rx);
    assert_eq!(observed.len(), 99);
    for (offset, action) in observed.iter().enumerate() {
        assert_eq!(
            action,
            &Action::KeyDown {
                key: named_key(&format!("k{:03}", offset + 1)),
                backend: Backend::Software,
            }
        );
    }
    Ok(())
}

#[test]
fn scheduler_starts_combo_actions_when_trigger_fires() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let key = named_key("s");
    let reflex = ScheduledReflex::on_event(
        "scheduler-combo",
        EventFilter::Kind {
            kind: "combo-trigger".to_owned(),
        },
        vec![Action::Combo {
            steps: vec![ComboStep {
                at_ms: 0,
                input: ComboInput::KeyDown { key: key.clone() },
            }],
            backend: Backend::Software,
        }],
    );

    let mut scheduler = ReflexScheduler::spawn(
        bus.clone(),
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(8),
    )?;
    let _report = bus.publish(synapse_core::Event {
        seq: 1,
        at: chrono::Utc::now(),
        source: synapse_core::EventSource::System,
        kind: "combo-trigger".to_owned(),
        data: serde_json::json!({ "case": "combo" }),
        correlations: Vec::new(),
    });
    let samples = scheduler.wait_for_samples(8, Duration::from_secs(3));
    scheduler.stop()?;

    assert_eq!(
        drain(&mut action_rx),
        vec![Action::KeyDown {
            key,
            backend: Backend::Software,
        }]
    );
    assert!(samples.iter().any(|sample| sample.dispatched_actions == 1));
    Ok(())
}

#[test]
fn scheduler_combo_driver_expires_after_final_step_and_writes_audit() -> Result<(), Box<dyn Error>>
{
    let temp = tempdir()?;
    let db_path = temp.path().join("db");
    let db = Arc::new(Db::open(&db_path, SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let key = named_key("x");
    let reflex = ScheduledReflex::combo(
        "combo-driver",
        ComboParams::new(
            vec![
                ComboStep {
                    at_ms: 0,
                    input: ComboInput::KeyDown { key: key.clone() },
                },
                ComboStep {
                    at_ms: 5,
                    input: ComboInput::KeyUp { key: key.clone() },
                },
            ],
            Backend::Software,
        ),
    );

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(32),
        Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(32, Duration::from_secs(3));
    let statuses = scheduler.statuses();
    scheduler.stop()?;

    assert_eq!(
        drain(&mut action_rx),
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
    assert!(samples.iter().any(|sample| sample.dispatched_actions > 0));
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, ReflexState::Expired);
    assert_eq!(statuses[0].fire_count, 1);

    db.flush()?;
    let rows = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
    let audits = rows
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    assert!(audits.iter().any(|audit| {
        audit.reflex_id == "combo-driver"
            && audit.status == ReflexState::Expired
            && audit.error_code.as_deref() == Some(error_codes::REFLEX_LIFETIME_EXPIRED)
            && audit.details["kind"] == REFLEX_LIFETIME_EXPIRED_KIND
            && audit.details["reason"] == "completed"
            && audit.details["combo_completion"]["kind"] == REFLEX_COMBO_COMPLETED_KIND
            && audit.details["combo_completion"]["scheduled_actions"] == 2
            && audit.details["combo_completion"]["dispatched_actions"] == 2
            && audit.details["combo_completion"]["dispatches"]
                .as_array()
                .is_some_and(|items| {
                    items.len() == 2
                        && items[0]["due_ms"] == 0
                        && items[0]["elapsed_ms"] == 0
                        && items[1]["due_ms"] == 5
                        && items[1]["jitter_ms"].as_u64().is_some()
                })
    }));
    Ok(())
}

#[test]
fn action_handle_combo_bridge_schedules_reflex_and_audit() -> Result<(), Box<dyn Error>> {
    let temp = tempdir()?;
    let db_path = temp.path().join("db");
    let db = Arc::new(Db::open(&db_path, SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let runtime = Arc::new(Mutex::new(ReflexRuntime::spawn(
        Arc::clone(&db),
        action_handle.clone(),
        bus,
    )?));
    install_action_combo_scheduler(&runtime)?;
    let key = named_key("bridge");
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    executor.block_on(action_handle.execute(Action::Combo {
        steps: vec![
            ComboStep {
                at_ms: 0,
                input: ComboInput::KeyDown { key: key.clone() },
            },
            ComboStep {
                at_ms: 5,
                input: ComboInput::KeyUp { key: key.clone() },
            },
        ],
        backend: Backend::Software,
    }))?;

    wait_for_runtime_state(&runtime, ReflexState::Expired)?;

    assert_eq!(
        drain(&mut action_rx),
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
    db.flush()?;
    let rows = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
    let audits = rows
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    assert!(audits.iter().any(|audit| {
        audit.status == ReflexState::Expired
            && audit.error_code.as_deref() == Some(error_codes::REFLEX_LIFETIME_EXPIRED)
            && audit.details["kind"] == REFLEX_LIFETIME_EXPIRED_KIND
            && audit.details["reason"] == "completed"
    }));
    Ok(())
}

const fn context(tick_ms: u64) -> ComboContext {
    ComboContext {
        tick_elapsed: Duration::from_millis(tick_ms),
    }
}

fn drain(rx: &mut mpsc::Receiver<ActionMessage>) -> Vec<Action> {
    let mut actions = Vec::new();
    while let Ok((action, _ack, _operator_panic_epoch_at_enqueue)) = rx.try_recv() {
        actions.push(action);
    }
    actions
}

fn wait_for_runtime_state(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    expected: ReflexState,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let statuses = runtime
            .lock()
            .map_err(|_err| "runtime lock poisoned")?
            .statuses();
        if statuses.iter().any(|status| status.state == expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for runtime state {expected:?}").into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn named_key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}
