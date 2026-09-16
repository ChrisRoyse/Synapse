use std::sync::{Arc, Mutex, Weak};

use synapse_action::{ActionComboScheduler, ActionError, ActionHandle, ActionResult};
use synapse_core::{Backend, ComboStep, ReflexState, new_reflex_id};

use crate::{ComboParams, ReflexRuntime, ScheduledReflex};

struct ReflexComboScheduler {
    runtime: Weak<Mutex<ReflexRuntime>>,
}

impl ReflexComboScheduler {
    fn schedule_combo_with_hooks<AfterLock, AfterRegister>(
        &self,
        steps: Vec<ComboStep>,
        backend: Backend,
        operator_panic_epoch_at_schedule: u64,
        after_lock: AfterLock,
        after_register: AfterRegister,
    ) -> ActionResult<()>
    where
        AfterLock: FnOnce(),
        AfterRegister: FnOnce(),
    {
        let runtime = self
            .runtime
            .upgrade()
            .ok_or_else(|| ActionError::BackendUnavailable {
                detail: "reflex runtime is unavailable for action combo scheduling".to_owned(),
            })?;
        let mut runtime = runtime
            .lock()
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "reflex runtime lock poisoned during action combo scheduling".to_owned(),
            })?;
        after_lock();
        ensure_combo_operator_panic_epoch(
            operator_panic_epoch_at_schedule,
            "immediately_before_reflex_registration",
        )?;
        let reflex_id = new_reflex_id();
        let reflex = ScheduledReflex::combo(reflex_id.clone(), ComboParams::new(steps, backend));
        runtime
            .register(&reflex)
            .map_err(|error| ActionError::BackendUnavailable {
                detail: format!("reflex combo scheduling failed: {error}"),
            })?;
        after_register();
        if let Err(error) = ensure_combo_operator_panic_epoch(
            operator_panic_epoch_at_schedule,
            "immediately_after_reflex_registration",
        ) {
            let rollback = runtime.disable_exact_for_operator_panic(&reflex_id);
            let status_after = runtime
                .statuses()
                .into_iter()
                .find(|status| status.id == reflex_id);
            let terminal = status_after.as_ref().is_some_and(|status| {
                matches!(
                    status.state,
                    ReflexState::Disabled
                        | ReflexState::Cancelled
                        | ReflexState::Expired
                        | ReflexState::ActionDenied
                )
            });
            if rollback.is_err() || !terminal {
                synapse_action::record_operator_panic_safety_incident();
            }
            return Err(ActionError::SafetyOperatorHotkeyFired {
                detail: format!(
                    "{}; exact_reflex_id={reflex_id}; rollback={rollback:?}; status_after={status_after:?}; rollback_terminal={terminal}",
                    error.detail()
                ),
            });
        }
        drop(runtime);
        Ok(())
    }
}

impl ActionComboScheduler for ReflexComboScheduler {
    fn schedule_combo(
        &self,
        steps: Vec<ComboStep>,
        backend: Backend,
        operator_panic_epoch_at_schedule: u64,
    ) -> ActionResult<()> {
        self.schedule_combo_with_hooks(
            steps,
            backend,
            operator_panic_epoch_at_schedule,
            || {},
            || {},
        )
    }
}

fn ensure_combo_operator_panic_epoch(expected_epoch: u64, stage: &'static str) -> ActionResult<()> {
    let readback = synapse_action::operator_panic_safety_readback();
    if !readback.pending && readback.epoch == expected_epoch {
        return Ok(());
    }
    Err(ActionError::SafetyOperatorHotkeyFired {
        detail: format!(
            "operator panic superseded combo scheduling at {stage}: expected_epoch={expected_epoch}, actual_epoch={}, pending={}, outstanding_generations={}, outstanding_finalizations={}, publications_in_flight={}, accounting_incident={}",
            readback.epoch,
            readback.pending,
            readback.outstanding_generations,
            readback.outstanding_finalizations,
            readback.publications_in_flight,
            readback.accounting_incident
        ),
    })
}

/// Installs a bridge so `ActionHandle::execute(Action::Combo)` schedules a
/// one-shot combo reflex when a reflex runtime owns the handle.
///
/// # Errors
///
/// Returns `ACTION_BACKEND_UNAVAILABLE` when the runtime or handle bridge slot
/// is poisoned.
pub fn install_action_combo_scheduler(runtime: &Arc<Mutex<ReflexRuntime>>) -> ActionResult<()> {
    let action_handle = action_handle(runtime)?;
    action_handle.install_combo_scheduler(Arc::new(ReflexComboScheduler {
        runtime: Arc::downgrade(runtime),
    }))
}

fn action_handle(runtime: &Arc<Mutex<ReflexRuntime>>) -> ActionResult<ActionHandle> {
    runtime
        .lock()
        .map(|runtime| runtime.action_handle().clone())
        .map_err(|_err| ActionError::BackendUnavailable {
            detail: "reflex runtime lock poisoned during action combo bridge install".to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_core::{ComboInput, Key, KeyCode};
    use synapse_storage::Db;

    #[tokio::test(flavor = "current_thread")]
    async fn operator_panic_crossing_registration_exactly_disables_generated_combo() {
        synapse_action::isolate_interrupt_epochs_for_test();
        let temp = tempfile::tempdir().expect("temp db");
        let db = Arc::new(Db::open(&temp.path().join("db"), 7).expect("open reflex db"));
        let (action_handle, _action_rx) = ActionHandle::channel();
        let runtime = Arc::new(Mutex::new(
            ReflexRuntime::spawn(db, action_handle, crate::EventBus::default())
                .expect("spawn reflex runtime"),
        ));
        let scheduler = ReflexComboScheduler {
            runtime: Arc::downgrade(&runtime),
        };
        let epoch_at_schedule = synapse_action::operator_panic_safety_readback().epoch;
        let result = scheduler.schedule_combo_with_hooks(
            vec![ComboStep {
                at_ms: 0,
                input: ComboInput::KeyPress {
                    key: Key {
                        code: KeyCode::Named {
                            value: "a".to_owned(),
                        },
                        use_scancode: false,
                    },
                    hold_ms: 10,
                },
            }],
            Backend::Software,
            epoch_at_schedule,
            || {},
            || {
                let mut token = synapse_action::request_operator_panic_interrupt();
                assert!(synapse_action::acknowledge_operator_panic_preemption(
                    &mut token
                ));
                let synapse_action::OperatorPanicSafetyCompletion::Finalize(finalization) =
                    synapse_action::complete_operator_panic_safety_generation(token)
                        .unwrap_or_else(|detail| panic!("complete synthetic panic: {detail}"))
                else {
                    panic!("isolated synthetic panic must own finalization");
                };
                assert!(synapse_action::finish_operator_panic_safety_finalization(
                    finalization,
                    true
                ));
            },
        );
        let error = result.expect_err("crossed registration must fail closed");
        assert!(matches!(
            error,
            ActionError::SafetyOperatorHotkeyFired { .. }
        ));
        let runtime = runtime.lock().expect("reflex runtime readback");
        assert_eq!(runtime.active_count(), 0);
        let statuses = runtime.statuses();
        drop(runtime);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ReflexState::Disabled);
    }
}
