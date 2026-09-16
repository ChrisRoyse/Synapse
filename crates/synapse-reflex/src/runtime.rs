use std::{collections::HashSet, fmt, sync::Arc};

use synapse_action::ActionHandle;
use synapse_core::{ReflexId, ReflexState, ReflexStatus, StoredAuditContext};
use synapse_storage::Db;

use crate::{
    AimTrackTargetSourceHandle, EventBus, ReflexActionGateHandle, ReflexResult, ScheduledReflex,
    SchedulerConfig, SchedulerHandle,
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
    /// Spawns the reflex runtime scaffold.
    ///
    /// # Errors
    ///
    /// The scaffold currently cannot fail after receiving initialized handles.
    /// Later M3 scheduler/bus work extends this result with OS-thread setup
    /// errors.
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
    /// The scaffold currently cannot fail after receiving initialized handles.
    #[tracing::instrument(skip_all, fields(component = "reflex_runtime"))]
    pub fn spawn_with_config(
        db: Arc<Db>,
        action_handle: ActionHandle,
        event_bus: EventBus,
        scheduler_config: SchedulerConfig,
    ) -> ReflexResult<Self> {
        Ok(Self {
            db,
            action_handle,
            event_bus,
            scheduler_config,
            audit_context: None,
            action_gate: None,
            aim_track_target_source: None,
            reflexes: Vec::new(),
            disabled_reflex_ids: HashSet::new(),
            scheduler: None,
        })
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
            .is_some_and(|sample| sample.degraded || sample.late)
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

    pub(crate) fn terminal_runtime_reflex_ids(&self) -> HashSet<ReflexId> {
        self.statuses()
            .into_iter()
            .filter(|status| {
                matches!(
                    status.state,
                    ReflexState::ActionDenied | ReflexState::Cancelled | ReflexState::Expired
                )
            })
            .map(|status| status.id)
            .collect()
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
}
