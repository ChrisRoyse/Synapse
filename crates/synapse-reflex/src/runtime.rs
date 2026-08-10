use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use synapse_action::ActionHandle;
use synapse_core::{ReflexId, ReflexState, ReflexStatus, StoredAuditContext};
use synapse_storage::Db;

use crate::{
    AimTrackTargetSourceHandle, EventBus, ReflexActionGateHandle, ReflexError, ReflexResult,
    ScheduledReflex, SchedulerConfig, SchedulerHandle,
};

/// Runtime handle for the M3 reflex subsystem.
///
/// Reflex input controllers use the shared [`synapse_action::ActionHandle`] as
/// the `synapse-action::handle` interlock authority. Held input state remains
/// owned by the private `synapse-action` emitter `BitSet`; reflex must enqueue
/// `hold_*` down/up actions through this handle and must not mirror, read, or
/// mutate held state independently.
pub struct ReflexRuntime {
    pub(crate) db: Arc<Db>,
    pub(crate) action_handle: ActionHandle,
    pub(crate) event_bus: EventBus,
    pub(crate) scheduler_config: SchedulerConfig,
    pub(crate) audit_context: Option<StoredAuditContext>,
    pub(crate) action_gate: Option<ReflexActionGateHandle>,
    pub(crate) aim_track_target_source: Option<AimTrackTargetSourceHandle>,
    pub(crate) reflexes: Vec<ScheduledReflex>,
    pub(crate) disabled_reflex_ids: HashSet<ReflexId>,
    pub(crate) durable_records: HashMap<ReflexId, crate::durable_state::DurableReflexRecord>,
    pub(crate) recovered_statuses: Vec<ReflexStatus>,
    pub(crate) scheduler: Option<SchedulerHandle>,
}

impl fmt::Debug for ReflexRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReflexRuntime")
            .field("db", &self.db)
            .field("action_handle", &self.action_handle)
            .field("event_bus", &self.event_bus)
            .field("reflex_count", &self.reflexes.len())
            .finish_non_exhaustive()
    }
}

impl ReflexRuntime {
    /// Number of scheduler/offload audit timestamps rejected in this process.
    ///
    /// The counter is process-global because those paths can detect a failure
    /// without holding the runtime mutex. Health exposes it so omission is
    /// never visible only in a transient log stream.
    #[must_use]
    pub fn audit_timestamp_invalid_total(&self) -> u64 {
        crate::audit_timestamp::invalid_total()
    }

    #[must_use]
    pub fn audit_queue_snapshot(&self) -> Option<crate::ReflexAuditQueueSnapshot> {
        self.scheduler
            .as_ref()
            .and_then(SchedulerHandle::audit_queue_snapshot)
    }

    /// Spawns the reflex runtime scaffold.
    ///
    /// # Errors
    ///
    /// Fails closed when the schema-owned reflex audit migration cannot prove
    /// every legacy retention-corrupted row was repaired and read back.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn spawn(
        db: Arc<Db>,
        action_handle: ActionHandle,
        event_bus: EventBus,
    ) -> ReflexResult<Self> {
        Self::spawn_with_config(db, action_handle, event_bus, SchedulerConfig::default())
    }

    /// Spawns the reflex runtime with an explicit scheduler config.
    ///
    /// # Errors
    ///
    /// Fails closed when reflex audit storage contains unrecognized invalid
    /// rows or a schema-owned migration cannot be durably read back.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn spawn_with_config(
        db: Arc<Db>,
        action_handle: ActionHandle,
        event_bus: EventBus,
        scheduler_config: SchedulerConfig,
    ) -> ReflexResult<Self> {
        crate::audit_migration::repair_reflex_audit_retention_corruption(&db)?;
        crate::audit_projection::ensure(&db)?;
        crate::durable_state::reconcile_legacy_active_orphans(&db)?;
        let recovered_terminal_intents =
            crate::durable_state::reconcile_prepared_terminal_intents(&db)?;
        let durable_records = crate::durable_state::load_records(&db)?;
        let mut reflexes = Vec::new();
        let mut disabled_reflex_ids = HashSet::new();
        let mut recovered_statuses = Vec::new();
        let mut durable_records_by_id = HashMap::with_capacity(durable_records.len());
        for record in durable_records {
            match record.status.state {
                ReflexState::Active => {
                    reflexes.push(record.definition.clone());
                    recovered_statuses.push(record.status.clone());
                }
                ReflexState::Disabled => {
                    disabled_reflex_ids.insert(record.definition.reflex_id.clone());
                    reflexes.push(record.definition.clone());
                    recovered_statuses.push(record.status.clone());
                }
                ReflexState::Cancelled | ReflexState::ActionDenied | ReflexState::Expired => {}
                ReflexState::Paused | ReflexState::Starved => {
                    return Err(ReflexError::ParamsInvalid {
                        detail: format!(
                            "REFLEX_DURABLE_STATE_RECONCILIATION_INVALID: reflex_id={} state={:?}; remediation=preserve and repair the exact desired-state row before starting reflex",
                            record.definition.reflex_id, record.status.state
                        ),
                    });
                }
            }
            durable_records_by_id.insert(record.definition.reflex_id.clone(), record);
        }
        reflexes.sort_by(|left, right| left.reflex_id.cmp(&right.reflex_id));
        recovered_statuses.sort_by(|left, right| left.id.cmp(&right.id));
        if recovered_terminal_intents > 0 {
            tracing::warn!(
                code = "REFLEX_TERMINAL_INTENTS_RECONCILED",
                recovered_terminal_intents,
                "completed every prepared reflex terminal intent before runtime construction"
            );
        }
        Ok(Self {
            db,
            action_handle,
            event_bus,
            scheduler_config,
            audit_context: None,
            action_gate: None,
            aim_track_target_source: None,
            reflexes,
            disabled_reflex_ids,
            durable_records: durable_records_by_id,
            recovered_statuses,
            scheduler: None,
        })
    }

    /// Reconciles validated durable desired state into one prepared scheduler
    /// generation, then publishes it. This is separate from storage recovery so
    /// callers can install action and aim-target authorities first.
    ///
    /// # Errors
    ///
    /// Fails before activation when a required authority is missing or the
    /// scheduler generation cannot be prepared.
    pub fn activate_recovered(&mut self) -> ReflexResult<()> {
        if self.scheduler.is_some() || self.reflexes.is_empty() {
            return Ok(());
        }
        if self.action_gate.is_none() {
            return Err(ReflexError::ParamsInvalid {
                detail: "REFLEX_RECOVERY_ACTION_GATE_MISSING: phase=prepare durable_state=validated scheduler=inactive; remediation=install the configured reflex action permission gate before activating recovered definitions".to_owned(),
            });
        }
        if self
            .reflexes
            .iter()
            .any(|reflex| matches!(&reflex.driver, crate::ScheduledReflexDriver::AimTrack(_)))
            && self.aim_track_target_source.is_none()
        {
            return Err(ReflexError::ParamsInvalid {
                detail: "REFLEX_RECOVERY_AIM_TARGET_SOURCE_MISSING: phase=prepare durable_state=validated scheduler=inactive; remediation=install the M1 aim-target source before activating recovered aim_track definitions".to_owned(),
            });
        }
        let registered_at = self
            .recovered_statuses
            .iter()
            .map(|status| status.registered_at)
            .min()
            .unwrap_or_else(chrono::Utc::now);
        let mut scheduler = crate::scheduler::ReflexScheduler::spawn_replacement(
            self.event_bus.clone(),
            self.action_handle.clone(),
            self.reflexes.clone(),
            self.scheduler_config.clone(),
            Arc::clone(&self.db),
            self.audit_context.clone(),
            self.action_gate.clone(),
            self.aim_track_target_source.clone(),
            &self.recovered_statuses,
            registered_at,
        )?;
        if !self.disabled_reflex_ids.is_empty() {
            let disabled = self.disabled_reflex_ids.iter().cloned().collect::<Vec<_>>();
            let actual = scheduler.disable_reflexes(&disabled);
            if actual.len() != disabled.len() {
                let stopped = scheduler.stop();
                return Err(ReflexError::ParamsInvalid {
                    detail: format!(
                        "REFLEX_RECOVERY_DISABLED_SET_MISMATCH: phase=prepare expected={} actual={} scheduler_stopped={} stop_error={}; remediation=preserve desired-state rows and inspect scheduler status construction",
                        disabled.len(),
                        actual.len(),
                        stopped.is_ok(),
                        stopped
                            .err()
                            .map_or_else(|| "none".to_owned(), |error| error.to_string())
                    ),
                });
            }
        }
        scheduler.activate_prepared();
        self.scheduler = Some(scheduler);
        self.recovered_statuses.clear();
        tracing::info!(
            code = "REFLEX_DURABLE_STATE_RECONCILED",
            definition_count = self.reflexes.len(),
            disabled_count = self.disabled_reflex_ids.len(),
            "reconciled durable reflex desired state into the executable scheduler"
        );
        Ok(())
    }

    /// Returns activation definitions that a host integration must reconcile
    /// before it declares recovered reflex readiness.
    #[must_use]
    pub fn recovered_activations(&self) -> Vec<(ReflexId, crate::ReflexActivation)> {
        let mut activations = self
            .durable_records
            .iter()
            .filter(|(_id, record)| record.status.state == ReflexState::Active)
            .filter_map(|(id, record)| {
                record
                    .activation
                    .clone()
                    .map(|activation| (id.clone(), activation))
            })
            .collect::<Vec<_>>();
        activations.sort_by(|left, right| left.0.cmp(&right.0));
        activations
    }

    /// Stops a scheduler generation whose host activation reconciliation
    /// failed, leaving durable desired state intact for an exact retry.
    ///
    /// # Errors
    ///
    /// Reports a scheduler panic with durable/runtime phase evidence.
    pub fn deactivate_after_recovery_failure(&mut self) -> ReflexResult<()> {
        if let Some(mut scheduler) = self.scheduler.take() {
            scheduler.stop().map_err(|error| ReflexError::ParamsInvalid {
                detail: format!(
                    "REFLEX_RECOVERY_SCHEDULER_STOP_FAILED: phase=host_activation_rollback durable_state=unchanged scheduler=terminated detail={error}; remediation=inspect the scheduler panic and retry reconciliation from durable desired state"
                ),
            })?;
        }
        self.recovered_statuses = self
            .durable_records
            .values()
            .filter(|record| {
                matches!(
                    record.status.state,
                    ReflexState::Active | ReflexState::Disabled
                )
            })
            .map(|record| record.status.clone())
            .collect();
        self.recovered_statuses
            .sort_by(|left, right| left.id.cmp(&right.id));
        Ok(())
    }

    /// Returns the current scheduler status snapshot for active reflexes.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn statuses(&self) -> Vec<ReflexStatus> {
        self.scheduler
            .as_ref()
            .map_or_else(Vec::new, SchedulerHandle::statuses)
    }

    /// Returns the number of currently active reflexes.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn active_count(&self) -> usize {
        self.statuses()
            .into_iter()
            .filter(|status| status.state == ReflexState::Active)
            .count()
    }

    /// Returns the most recent scheduler tick jitter.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn last_tick_jitter_us(&self) -> Option<u64> {
        self.scheduler
            .as_ref()
            .and_then(|scheduler| scheduler.samples().last().map(|sample| sample.jitter_us))
    }

    /// Returns the number of retained scheduler tick samples.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn sample_count(&self) -> usize {
        self.scheduler
            .as_ref()
            .map_or(0, |scheduler| scheduler.samples().len())
    }

    /// Returns the configured scheduler tick sample ring limit.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn sample_limit(&self) -> usize {
        self.scheduler_config.sample_limit
    }

    /// Returns p99 jitter across retained scheduler tick samples.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn p99_tick_jitter_us(&self) -> Option<u64> {
        self.scheduler
            .as_ref()
            .map(|scheduler| crate::scheduler::p99_jitter_us(&scheduler.samples()))
    }

    /// Returns retained tick samples marked late.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn late_tick_count(&self) -> usize {
        self.scheduler.as_ref().map_or(0, |scheduler| {
            scheduler
                .samples()
                .iter()
                .filter(|sample| sample.late)
                .count()
        })
    }

    /// Returns the latest consecutive non-degraded scheduler deadline-miss streak.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn deadline_miss_streak(&self) -> Option<u32> {
        self.scheduler.as_ref().and_then(|scheduler| {
            scheduler
                .samples()
                .last()
                .map(|sample| sample.deadline_miss_streak)
        })
    }

    /// Returns the configured consecutive deadline misses required for a
    /// non-degraded jitter episode to become a durable audit row.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn deadline_miss_audit_after(&self) -> u32 {
        self.scheduler_config.deadline_miss_audit_after
    }

    /// Returns the severe non-degraded deadline miss threshold in microseconds.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn severe_deadline_miss_after_us(&self) -> u64 {
        duration_us(self.scheduler_config.severe_deadline_miss_after)
    }

    /// Returns retained tick samples that ran through the degraded fallback interval.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn degraded_tick_count(&self) -> usize {
        self.scheduler.as_ref().map_or(0, |scheduler| {
            scheduler
                .samples()
                .iter()
                .filter(|sample| sample.degraded)
                .count()
        })
    }

    /// Returns true when the latest tick ran in degraded mode or missed its deadline.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn degraded_latency(&self) -> bool {
        self.scheduler
            .as_ref()
            .and_then(|scheduler| scheduler.samples().last().copied())
            .is_some_and(|sample| {
                sample.degraded
                    || (sample.late
                        && (sample.deadline_miss_streak
                            >= self.scheduler_config.deadline_miss_audit_after
                            || sample.elapsed_us
                                >= duration_us(self.scheduler_config.severe_deadline_miss_after)))
            })
    }

    /// Returns the action emitter handle used by reflex controllers.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn action_handle(&self) -> &ActionHandle {
        &self.action_handle
    }

    /// Returns the event bus handle used by this runtime.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }

    pub(crate) fn terminal_runtime_reflex_ids(&self) -> ReflexResult<HashSet<ReflexId>> {
        if let Some(scheduler) = &self.scheduler {
            let mut pending_ids = scheduler
                .pending_terminal_ids()?
                .into_iter()
                .collect::<Vec<_>>();
            if !pending_ids.is_empty() {
                pending_ids.sort();
                return Err(ReflexError::ParamsInvalid {
                    detail: format!(
                        "REFLEX_TERMINAL_LIFECYCLE_PENDING: phase=scheduler_replacement_prepare pending_reflex_ids={pending_ids:?}; remediation=leave the current scheduler generation running until health terminal_lifecycle_pending returns zero, then retry the registration"
                    ),
                });
            }
        }
        Ok(self
            .statuses()
            .into_iter()
            .filter(|status| {
                matches!(
                    status.state,
                    ReflexState::ActionDenied | ReflexState::Cancelled | ReflexState::Expired
                )
            })
            .map(|status| status.id)
            .collect::<HashSet<_>>())
    }

    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn set_audit_context(&mut self, audit_context: Option<StoredAuditContext>) {
        self.audit_context = audit_context;
    }

    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn set_action_gate(&mut self, action_gate: Option<ReflexActionGateHandle>) {
        self.action_gate = action_gate;
    }

    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn set_aim_track_target_source(
        &mut self,
        target_source: Option<AimTrackTargetSourceHandle>,
    ) {
        self.aim_track_target_source = target_source;
    }

    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn audit_context(&self) -> Option<StoredAuditContext> {
        self.audit_context.clone()
    }

    /// Externally readable state of the tick's lowered guard-threshold feed
    /// (#1686).
    ///
    /// `None` before the scheduler has been started; there is no tick and
    /// therefore no hot path to describe.
    #[must_use]
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn lowered_guard_thresholds_snapshot(&self) -> Option<crate::LoweredFeedSnapshot> {
        self.scheduler
            .as_ref()
            .map(|scheduler| scheduler.lowered_guard_thresholds().snapshot())
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
