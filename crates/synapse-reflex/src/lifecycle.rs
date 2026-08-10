use std::sync::Arc;

use chrono::Utc;
use synapse_core::{Action, ButtonAction, ReflexState, ReflexStatus};

use crate::{
    MAX_REFLEX_PRIORITY, ReflexCancelOutcome, ReflexError, ReflexResult, ReflexRuntime,
    ScheduledReflex, ScheduledReflexDriver, scheduler,
};

impl ReflexRuntime {
    /// Registers a new reflex into this runtime and persists the registration audit row.
    ///
    /// # Errors
    ///
    /// Returns a [`ReflexError`] when the runtime has reached the reflex cap,
    /// the reflex priority or trigger is invalid, the scheduler cannot be
    /// restarted, or the registration audit row cannot be persisted.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime", reflex_id = %reflex.reflex_id))]
    pub fn register(&mut self, reflex: &ScheduledReflex) -> ReflexResult<ReflexStatus> {
        self.register_with_activation(reflex, None)
    }

    /// Registers a reflex together with activation work required to reproduce
    /// its exact event source after process recovery.
    ///
    /// # Errors
    ///
    /// Returns a structured error before scheduler activation when validation,
    /// preparation, or the atomic durable publication fails.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime", reflex_id = %reflex.reflex_id))]
    pub fn register_with_activation(
        &mut self,
        reflex: &ScheduledReflex,
        activation: Option<crate::ReflexActivation>,
    ) -> ReflexResult<ReflexStatus> {
        if reflex.priority > MAX_REFLEX_PRIORITY {
            return Err(ReflexError::PriorityInvalid {
                detail: format!(
                    "priority {} exceeds maximum {MAX_REFLEX_PRIORITY}",
                    reflex.priority
                ),
            });
        }
        let terminal_ids = self.terminal_runtime_reflex_ids();
        let mut next = self
            .reflexes
            .iter()
            .filter(|reflex| !terminal_ids.contains(&reflex.reflex_id))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(existing) = next
            .iter()
            .find(|existing| same_reflex_definition(existing, reflex))
        {
            return Err(ReflexError::ParamsInvalid {
                detail: format!(
                    "duplicate active reflex registration matches existing reflex {}",
                    existing.reflex_id
                ),
            });
        }
        next.push(reflex.clone());
        scheduler::validate_reflexes(&next)?;
        let registered_at = Utc::now();
        let registered_at_ns =
            crate::audit_timestamp::unix_ns(&registered_at, crate::REFLEX_REGISTERED_KIND)?;

        let prior_statuses = self.statuses();
        let mut new_scheduler = scheduler::ReflexScheduler::spawn_replacement(
            self.event_bus.clone(),
            self.action_handle.clone(),
            next.clone(),
            self.scheduler_config.clone(),
            Arc::clone(&self.db),
            self.audit_context.clone(),
            self.action_gate.clone(),
            self.aim_track_target_source.clone(),
            &prior_statuses,
            registered_at,
        )?;
        if !self.disabled_reflex_ids.is_empty() {
            let disabled_reflex_ids = self.disabled_reflex_ids.iter().cloned().collect::<Vec<_>>();
            let _disabled_statuses = new_scheduler.disable_reflexes(&disabled_reflex_ids);
        }
        let status = new_scheduler
            .statuses()
            .into_iter()
            .find(|status| status.id == reflex.reflex_id)
            .ok_or_else(|| ReflexError::ParamsInvalid {
                detail: format!("registered reflex status missing: {}", reflex.reflex_id),
            })?;
        let durable_record = crate::durable_state::DurableReflexRecord::active(
            reflex.clone(),
            status.clone(),
            activation,
        )?;
        if let Err(commit_error) =
            self.write_registration_audit(&status, registered_at_ns, &durable_record)
        {
            let rollback = new_scheduler.stop();
            return Err(ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_REGISTRATION_TRANSACTION_ROLLED_BACK: phase=durable_commit scheduler_prepared=true scheduler_activated=false audit_committed=false prepared_scheduler_stopped={} commit_error={commit_error} rollback_error={}; remediation=repair the commit error and retry; the prior scheduler and runtime definition remain authoritative",
                    rollback.is_ok(),
                    rollback
                        .err()
                        .map_or_else(|| "none".to_owned(), |error| error.to_string())
                ),
            });
        }

        // The Calyx transaction is now the crash-recovery authority. Stop the
        // old generation before publishing the prepared candidate, preventing
        // an overlap where both scheduler threads could dispatch. A join error
        // means the old thread already terminated by panic; it is cleanup
        // evidence, not a reason to report the committed registration as
        // failed or leave the new durable state inactive.
        if let Some(mut old_scheduler) = self.scheduler.take()
            && let Err(error) = old_scheduler.stop()
        {
            tracing::error!(
                code = "REFLEX_REGISTRATION_OLD_SCHEDULER_STOP_FAILED_AFTER_COMMIT",
                reflex_id = %reflex.reflex_id,
                phase = "old_scheduler_cleanup",
                scheduler_prepared = true,
                scheduler_activated = false,
                audit_committed = true,
                detail = %error,
                remediation = "inspect the prior scheduler panic; the old thread is joined and terminated, and the committed replacement will now be activated",
                "old reflex scheduler terminated with a panic while committing its replacement"
            );
        }
        new_scheduler.activate_prepared();
        self.scheduler = Some(new_scheduler);
        self.reflexes = next;
        self.durable_records
            .insert(reflex.reflex_id.clone(), durable_record);
        Ok(status)
    }

    /// Cancels an active reflex and persists a cancellation audit row.
    ///
    /// # Errors
    ///
    /// Returns a [`ReflexError`] if the cancellation audit row cannot be
    /// persisted.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime", reflex_id = %reflex_id))]
    #[expect(
        clippy::too_many_lines,
        reason = "the cancellation prepare/commit/publish protocol keeps every failure phase and post-commit invariant explicit"
    )]
    pub fn cancel(&mut self, reflex_id: &str) -> ReflexResult<ReflexCancelOutcome> {
        let Some(status) = self
            .statuses()
            .into_iter()
            .find(|status| status.id == reflex_id)
        else {
            let Some(status) = self.terminal_status_from_audit(reflex_id)? else {
                return Ok(ReflexCancelOutcome::NotFound);
            };
            return Ok(cancel_outcome_for_terminal_status(status));
        };

        match status.state {
            ReflexState::ActionDenied | ReflexState::Expired => {
                return Ok(ReflexCancelOutcome::AlreadyExpired { status });
            }
            ReflexState::Cancelled => {
                return Ok(ReflexCancelOutcome::Cancelled { status });
            }
            ReflexState::Active
            | ReflexState::Paused
            | ReflexState::Disabled
            | ReflexState::Starved => {}
        }

        let Some(_scheduler) = &self.scheduler else {
            return Ok(ReflexCancelOutcome::NotFound);
        };
        let prior_record = self.durable_records.get(reflex_id).cloned().ok_or_else(|| {
            ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_DURABLE_DEFINITION_MISSING: reflex_id={reflex_id} phase=cancel_prepare scheduler=unchanged; remediation=run the explicit orphan reconciliation before cancelling this reflex"
                ),
            }
        })?;
        let mut cancelled_status = status;
        cancelled_status.state = ReflexState::Cancelled;
        cancelled_status.last_error_code = None;
        let next_record = prior_record.with_status(cancelled_status.clone())?;
        let next = self
            .reflexes
            .iter()
            .filter(|reflex| reflex.reflex_id.as_str() != reflex_id)
            .cloned()
            .collect::<Vec<_>>();
        scheduler::validate_reflexes(&next)?;
        let prior_statuses = self
            .statuses()
            .into_iter()
            .filter(|status| status.id != reflex_id)
            .collect::<Vec<_>>();
        let mut new_scheduler = scheduler::ReflexScheduler::spawn_replacement(
            self.event_bus.clone(),
            self.action_handle.clone(),
            next.clone(),
            self.scheduler_config.clone(),
            Arc::clone(&self.db),
            self.audit_context.clone(),
            self.action_gate.clone(),
            self.aim_track_target_source.clone(),
            &prior_statuses,
            Utc::now(),
        )?;
        if !self.disabled_reflex_ids.is_empty() {
            let disabled = self
                .disabled_reflex_ids
                .iter()
                .filter(|id| id.as_str() != reflex_id)
                .cloned()
                .collect::<Vec<_>>();
            let _disabled_statuses = new_scheduler.disable_reflexes(&disabled);
        }
        self.dispatch_cancel_release_actions(reflex_id)?;
        if let Err(commit_error) =
            self.write_cancellation_audit(&cancelled_status, &prior_record, &next_record)
        {
            let rollback = new_scheduler.stop();
            return Err(ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_CANCELLATION_TRANSACTION_ROLLED_BACK: phase=durable_commit scheduler_prepared=true scheduler_activated=false desired_state_committed=false prepared_scheduler_stopped={} commit_error={commit_error} rollback_error={}; remediation=repair the commit error and retry; the prior scheduler, definition, and desired-state row remain authoritative",
                    rollback.is_ok(),
                    rollback
                        .err()
                        .map_or_else(|| "none".to_owned(), |error| error.to_string())
                ),
            });
        }
        if let Some(mut old_scheduler) = self.scheduler.take()
            && let Err(error) = old_scheduler.stop()
        {
            tracing::error!(
                code = "REFLEX_CANCELLATION_OLD_SCHEDULER_STOP_FAILED_AFTER_COMMIT",
                reflex_id,
                phase = "old_scheduler_cleanup",
                scheduler_prepared = true,
                scheduler_activated = false,
                desired_state_committed = true,
                detail = %error,
                remediation = "inspect the prior scheduler panic; the thread is terminated and the committed replacement will now be activated",
                "old reflex scheduler terminated with a panic while publishing cancellation"
            );
        }
        new_scheduler.activate_prepared();
        self.scheduler = Some(new_scheduler);
        self.disabled_reflex_ids.remove(reflex_id);
        self.reflexes = next;
        self.durable_records
            .insert(reflex_id.to_owned(), next_record);
        Ok(ReflexCancelOutcome::Cancelled {
            status: cancelled_status,
        })
    }

    fn dispatch_cancel_release_actions(&self, reflex_id: &str) -> ReflexResult<()> {
        let Some(reflex) = self
            .reflexes
            .iter()
            .find(|reflex| reflex.reflex_id.as_str() == reflex_id)
        else {
            return Ok(());
        };
        for action in cancel_release_actions(reflex) {
            self.action_handle
                .try_execute(action)
                .map_err(|error| ReflexError::ParamsInvalid {
                    detail: format!(
                        "cancel release action dispatch failed for reflex {reflex_id}: {error}"
                    ),
                })?;
        }
        Ok(())
    }

    /// Disables every active scheduler reflex for the operator panic hotkey and
    /// stops the scheduler so no in-flight tick can reassert held input after
    /// the action emitter drains state.
    ///
    /// # Errors
    ///
    /// Returns a [`ReflexError`] when the disabled audit rows cannot be
    /// persisted.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn disable_all_by_operator(&mut self) -> ReflexResult<Vec<ReflexStatus>> {
        self.disable_all_with_reason("operator_hotkey")
    }

    /// Disables every active scheduler reflex for a tool-triggered `release_all`
    /// and stops the scheduler so no in-flight tick can reassert held input
    /// after the action emitter drains state.
    ///
    /// # Errors
    ///
    /// Returns a [`ReflexError`] when stopping the scheduler or writing the
    /// disabled audit rows fails.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn disable_all_for_release_all(&mut self) -> ReflexResult<Vec<ReflexStatus>> {
        self.disable_all_with_reason("release_all")
    }

    /// Disables one exact reflex registered by the `ActionHandle` combo bridge
    /// when an operator-panic epoch crosses its registration critical section.
    /// The caller holds the runtime lock across register, epoch recheck, and
    /// this rollback, so no later K2 sweep can miss the generated id.
    pub(crate) fn disable_exact_for_operator_panic(
        &mut self,
        reflex_id: &str,
    ) -> ReflexResult<Option<ReflexStatus>> {
        let Some(_scheduler) = self.scheduler.as_ref() else {
            return Ok(None);
        };
        let (new_scheduler, mut disabled) =
            self.prepare_disabled_replacement(&[reflex_id.to_owned()])?;
        let status = disabled.pop();
        let Some(status) = status else {
            drop(new_scheduler);
            return Ok(None);
        };
        self.commit_disabled_replacement(
            new_scheduler,
            std::slice::from_ref(&status),
            "operator_hotkey",
        )?;
        Ok(Some(status))
    }

    fn disable_all_with_reason(&mut self, reason: &'static str) -> ReflexResult<Vec<ReflexStatus>> {
        let Some(_scheduler) = self.scheduler.as_ref() else {
            return Ok(Vec::new());
        };
        let reflex_ids = self
            .statuses()
            .into_iter()
            .filter(|status| {
                matches!(
                    status.state,
                    ReflexState::Active | ReflexState::Paused | ReflexState::Starved
                )
            })
            .map(|status| status.id)
            .collect::<Vec<_>>();
        if reflex_ids.is_empty() {
            return Ok(Vec::new());
        }
        let (new_scheduler, disabled) = self.prepare_disabled_replacement(&reflex_ids)?;
        self.commit_disabled_replacement(new_scheduler, &disabled, reason)?;
        Ok(disabled)
    }

    fn prepare_disabled_replacement(
        &self,
        reflex_ids: &[String],
    ) -> ReflexResult<(scheduler::SchedulerHandle, Vec<ReflexStatus>)> {
        let prior_statuses = self.statuses();
        let mut new_scheduler = scheduler::ReflexScheduler::spawn_replacement(
            self.event_bus.clone(),
            self.action_handle.clone(),
            self.reflexes.clone(),
            self.scheduler_config.clone(),
            Arc::clone(&self.db),
            self.audit_context.clone(),
            self.action_gate.clone(),
            self.aim_track_target_source.clone(),
            &prior_statuses,
            Utc::now(),
        )?;
        let mut all_disabled = self.disabled_reflex_ids.iter().cloned().collect::<Vec<_>>();
        all_disabled.extend(reflex_ids.iter().cloned());
        all_disabled.sort();
        all_disabled.dedup();
        let _prepared_disabled = new_scheduler.disable_reflexes(&all_disabled);
        let requested = reflex_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let disabled = new_scheduler
            .statuses()
            .into_iter()
            .filter(|status| requested.contains(status.id.as_str()))
            .collect::<Vec<_>>();
        if disabled.len() != reflex_ids.len()
            || disabled
                .iter()
                .any(|status| status.state != ReflexState::Disabled)
        {
            let rollback = new_scheduler.stop();
            return Err(ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_DISABLE_PREPARED_STATUS_MISMATCH: phase=scheduler_prepare requested={} disabled={} prepared_scheduler_stopped={} rollback_error={}; remediation=inspect scheduler identity/status construction before retrying; runtime and durable state are unchanged",
                    reflex_ids.len(),
                    disabled.len(),
                    rollback.is_ok(),
                    rollback
                        .err()
                        .map_or_else(|| "none".to_owned(), |error| error.to_string())
                ),
            });
        }
        Ok((new_scheduler, disabled))
    }

    fn commit_disabled_replacement(
        &mut self,
        mut new_scheduler: scheduler::SchedulerHandle,
        disabled: &[ReflexStatus],
        reason: &'static str,
    ) -> ReflexResult<()> {
        let next_records = match self.write_disabled_audits_with_reason(disabled, reason) {
            Ok(records) => records,
            Err(commit_error) => {
                let rollback = new_scheduler.stop();
                return Err(ReflexError::ParamsInvalid {
                    detail: format!(
                        "REFLEX_DISABLE_TRANSACTION_ROLLED_BACK: phase=durable_commit scheduler_prepared=true scheduler_activated=false desired_state_committed=false prepared_scheduler_stopped={} commit_error={commit_error} rollback_error={}; remediation=repair the commit error and retry; every prior scheduler control, definition, and desired-state row remains authoritative",
                        rollback.is_ok(),
                        rollback
                            .err()
                            .map_or_else(|| "none".to_owned(), |error| error.to_string())
                    ),
                });
            }
        };
        if let Some(mut old_scheduler) = self.scheduler.take()
            && let Err(error) = old_scheduler.stop()
        {
            tracing::error!(
                code = "REFLEX_DISABLE_OLD_SCHEDULER_STOP_FAILED_AFTER_COMMIT",
                phase = "old_scheduler_cleanup",
                disabled_count = disabled.len(),
                scheduler_prepared = true,
                scheduler_activated = false,
                desired_state_committed = true,
                detail = %error,
                remediation = "inspect the prior scheduler panic; the thread is terminated and the committed disabled replacement will now be activated",
                "old reflex scheduler terminated with a panic while publishing disable"
            );
        }
        new_scheduler.activate_prepared();
        self.scheduler = Some(new_scheduler);
        for record in next_records {
            self.disabled_reflex_ids
                .insert(record.definition.reflex_id.clone());
            self.durable_records
                .insert(record.definition.reflex_id.clone(), record);
        }
        Ok(())
    }
}

fn cancel_release_actions(reflex: &ScheduledReflex) -> Vec<Action> {
    match &reflex.driver {
        ScheduledReflexDriver::HoldMove(params) => params
            .keys
            .iter()
            .rev()
            .cloned()
            .map(|key| Action::KeyUp {
                key,
                backend: params.backend,
            })
            .collect(),
        ScheduledReflexDriver::HoldButton(params) => match params.button {
            synapse_core::ReflexButtonTarget::Mouse { button } => vec![Action::MouseButton {
                button,
                action: ButtonAction::Up,
                hold_ms: 0,
                backend: params.backend,
            }],
            synapse_core::ReflexButtonTarget::Pad { pad, button } => {
                vec![Action::PadButton {
                    pad,
                    button,
                    action: ButtonAction::Up,
                    hold_ms: 0,
                }]
            }
        },
        ScheduledReflexDriver::Actions
        | ScheduledReflexDriver::AimTrack(_)
        | ScheduledReflexDriver::Combo(_) => Vec::new(),
        ScheduledReflexDriver::PathFollow(params) => {
            params.button.map_or_else(Vec::new, |button| {
                vec![Action::MouseButton {
                    button,
                    action: ButtonAction::Up,
                    hold_ms: 0,
                    backend: params.backend,
                }]
            })
        }
    }
}

fn cancel_outcome_for_terminal_status(status: ReflexStatus) -> ReflexCancelOutcome {
    match status.state {
        ReflexState::ActionDenied | ReflexState::Expired => {
            ReflexCancelOutcome::AlreadyExpired { status }
        }
        ReflexState::Cancelled => ReflexCancelOutcome::Cancelled { status },
        ReflexState::Active
        | ReflexState::Paused
        | ReflexState::Disabled
        | ReflexState::Starved => ReflexCancelOutcome::NotFound,
    }
}

fn same_reflex_definition(left: &ScheduledReflex, right: &ScheduledReflex) -> bool {
    left.trigger == right.trigger
        && left.then == right.then
        && left.driver == right.driver
        && left.priority == right.priority
        && left.lifetime == right.lifetime
        && left.exclusive == right.exclusive
        && left.debounce == right.debounce
}
