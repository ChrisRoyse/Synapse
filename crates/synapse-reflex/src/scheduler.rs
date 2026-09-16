use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::Duration,
};

use chrono::Utc;
use synapse_action::ActionHandle;
use synapse_core::{Action, EventFilter, ReflexId, ReflexLifetime, StoredAuditContext};
use synapse_storage::Db;

use crate::{
    EventBus, ReflexActionGateHandle,
    error::{ReflexError, ReflexResult},
    kinds::hold_lifetime::validate_lifetime,
    kinds::on_event::OnEventState,
    kinds::{
        aim_track::{AimTrackParams, AimTrackTargetSourceHandle},
        combo::ComboParams,
        hold_button::HoldButtonParams,
        hold_move::HoldMoveParams,
        path_follow::PathFollowParams,
    },
};
pub use scheduler_handle::SchedulerHandle;
use scheduler_loop::{
    ReflexControl, RuntimeReflex, RuntimeState, aim_track_states, combo_states, hold_button_states,
    hold_move_states, lock_controls, mark_reflex_action_denied, mark_reflex_active_if_starved,
    mark_reflex_combo_completed, mark_reflex_error, mark_reflex_fired,
    mark_reflex_lifetime_expired, mark_reflex_path_follow_completed, mark_reflex_starved,
    mark_reflex_track_lost, path_follow_states, run_scheduler_thread, status_for_reflex,
};

pub const MAX_SCHEDULED_REFLEXES: usize = 32;
pub const MAX_REFLEX_PRIORITY: u32 = 1000;
pub const REFLEX_TICK_LATE_KIND: &str = "reflex_tick_late";
pub const DEFAULT_SAMPLE_LIMIT: usize = 4096;
pub const DEFAULT_REFLEX_PRIORITY: u32 = 100;

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    pub target_interval: Duration,
    pub fallback_interval: Duration,
    pub late_after: Duration,
    pub sample_limit: usize,
    pub max_ticks: Option<u64>,
    pub force_degraded: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        let target_interval = Duration::from_millis(1);
        Self {
            target_interval,
            fallback_interval: Duration::from_millis(2),
            late_after: target_interval.saturating_mul(2),
            sample_limit: DEFAULT_SAMPLE_LIMIT,
            max_ticks: None,
            force_degraded: false,
        }
    }
}

impl SchedulerConfig {
    #[must_use]
    pub const fn with_max_ticks(mut self, max_ticks: u64) -> Self {
        self.max_ticks = Some(max_ticks);
        self
    }

    fn validate(&self) -> ReflexResult<()> {
        if self.target_interval.is_zero() {
            return Err(ReflexError::ParamsInvalid {
                detail: "scheduler target interval must be non-zero".to_owned(),
            });
        }
        if self.fallback_interval.is_zero() {
            return Err(ReflexError::ParamsInvalid {
                detail: "scheduler fallback interval must be non-zero".to_owned(),
            });
        }
        if self.sample_limit == 0 {
            return Err(ReflexError::ParamsInvalid {
                detail: "scheduler sample limit must be non-zero".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScheduledReflex {
    pub reflex_id: ReflexId,
    pub trigger: SchedulerTrigger,
    pub then: Vec<Action>,
    pub driver: ScheduledReflexDriver,
    pub priority: u32,
    pub lifetime: ReflexLifetime,
    pub exclusive: bool,
    pub debounce: Duration,
}

impl ScheduledReflex {
    #[must_use]
    pub fn every_tick(reflex_id: impl Into<ReflexId>, then: Vec<Action>) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::EveryTick,
            then,
            driver: ScheduledReflexDriver::Actions,
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::UntilCancelled,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn on_event(
        reflex_id: impl Into<ReflexId>,
        filter: EventFilter,
        then: Vec<Action>,
    ) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::OnEvent(filter),
            then,
            driver: ScheduledReflexDriver::Actions,
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::UntilCancelled,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn on_event_with_debounce(
        reflex_id: impl Into<ReflexId>,
        filter: EventFilter,
        then: Vec<Action>,
        debounce: Duration,
    ) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::OnEvent(filter),
            then,
            driver: ScheduledReflexDriver::Actions,
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::UntilCancelled,
            exclusive: false,
            debounce,
        }
    }

    #[must_use]
    pub fn aim_track(reflex_id: impl Into<ReflexId>, params: AimTrackParams) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::EveryTick,
            then: Vec::new(),
            driver: ScheduledReflexDriver::AimTrack(params),
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::UntilCancelled,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn hold_move(reflex_id: impl Into<ReflexId>, params: HoldMoveParams) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::EveryTick,
            then: Vec::new(),
            driver: ScheduledReflexDriver::HoldMove(params),
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::UntilCancelled,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn hold_button(reflex_id: impl Into<ReflexId>, params: HoldButtonParams) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::EveryTick,
            then: Vec::new(),
            driver: ScheduledReflexDriver::HoldButton(params),
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::UntilCancelled,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn combo(reflex_id: impl Into<ReflexId>, params: ComboParams) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::EveryTick,
            then: Vec::new(),
            driver: ScheduledReflexDriver::Combo(params),
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::OneShot,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn path_follow(reflex_id: impl Into<ReflexId>, params: PathFollowParams) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            trigger: SchedulerTrigger::EveryTick,
            then: Vec::new(),
            driver: ScheduledReflexDriver::PathFollow(params),
            priority: DEFAULT_REFLEX_PRIORITY,
            lifetime: ReflexLifetime::OneShot,
            exclusive: false,
            debounce: Duration::ZERO,
        }
    }

    #[must_use]
    pub const fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub fn with_lifetime(mut self, lifetime: ReflexLifetime) -> Self {
        self.lifetime = lifetime;
        self
    }

    #[must_use]
    pub const fn with_exclusive(mut self, exclusive: bool) -> Self {
        self.exclusive = exclusive;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScheduledReflexDriver {
    Actions,
    AimTrack(AimTrackParams),
    HoldMove(HoldMoveParams),
    HoldButton(HoldButtonParams),
    Combo(ComboParams),
    PathFollow(PathFollowParams),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerTrigger {
    EveryTick,
    OnEvent(EventFilter),
}

impl SchedulerTrigger {
    fn validate(&self) -> ReflexResult<()> {
        match self {
            Self::EveryTick => Ok(()),
            Self::OnEvent(filter) => {
                filter
                    .validate()
                    .map_err(|error| ReflexError::FilterInvalid {
                        detail: error.to_string(),
                    })
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TickSample {
    pub tick_index: u64,
    pub elapsed_us: u64,
    pub jitter_us: u64,
    pub target_us: u64,
    pub pulled_events: usize,
    pub dispatched_actions: usize,
    pub late: bool,
    pub degraded: bool,
}

pub struct ReflexScheduler;

impl ReflexScheduler {
    /// Spawns the dedicated reflex scheduler thread.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid timing config, invalid reflex filters, reflex cap overflow,
    /// event-bus subscription failure, or scheduler thread spawn failure.
    pub fn spawn(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            None,
            None,
            None,
            None,
        )
    }

    /// Spawns the scheduler with an action permission gate.
    ///
    /// # Errors
    ///
    /// Returns the same setup errors as [`Self::spawn`].
    pub fn spawn_with_action_gate(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        action_gate: ReflexActionGateHandle,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            None,
            None,
            Some(action_gate),
            None,
        )
    }

    /// Spawns the scheduler and writes reflex audit rows into `audit_db`.
    ///
    /// # Errors
    ///
    /// Returns the same setup errors as [`Self::spawn`].
    pub fn spawn_with_audit_db(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        audit_db: Arc<Db>,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            Some(audit_db),
            None,
            None,
            None,
        )
    }

    /// Spawns the scheduler and writes reflex audit rows with an attached audit context.
    ///
    /// # Errors
    ///
    /// Returns the same setup errors as [`Self::spawn`].
    pub fn spawn_with_audit_db_and_context(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        audit_db: Arc<Db>,
        audit_context: Option<StoredAuditContext>,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            Some(audit_db),
            audit_context,
            None,
            None,
        )
    }

    /// Spawns the scheduler with audit persistence, context, and an action permission gate.
    ///
    /// # Errors
    ///
    /// Returns the same setup errors as [`Self::spawn`].
    pub fn spawn_with_audit_db_context_and_action_gate(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        audit_db: Arc<Db>,
        audit_context: Option<StoredAuditContext>,
        action_gate: ReflexActionGateHandle,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            Some(audit_db),
            audit_context,
            Some(action_gate),
            None,
        )
    }

    /// Spawns the scheduler with audit persistence, context, and a dynamic
    /// `aim_track` target source.
    ///
    /// # Errors
    ///
    /// Returns the same setup errors as [`Self::spawn`].
    pub fn spawn_with_audit_db_context_and_aim_track_source(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        audit_db: Arc<Db>,
        audit_context: Option<StoredAuditContext>,
        aim_track_target_source: AimTrackTargetSourceHandle,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            Some(audit_db),
            audit_context,
            None,
            Some(aim_track_target_source),
        )
    }

    /// Spawns the scheduler with audit persistence, context, an action gate,
    /// and a dynamic `aim_track` target source.
    ///
    /// # Errors
    ///
    /// Returns the same setup errors as [`Self::spawn`].
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_audit_db_context_action_gate_and_aim_track_source(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        audit_db: Arc<Db>,
        audit_context: Option<StoredAuditContext>,
        action_gate: ReflexActionGateHandle,
        aim_track_target_source: AimTrackTargetSourceHandle,
    ) -> ReflexResult<SchedulerHandle> {
        Self::spawn_inner(
            event_bus,
            action_handle,
            reflexes,
            config,
            Some(audit_db),
            audit_context,
            Some(action_gate),
            Some(aim_track_target_source),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_inner(
        event_bus: EventBus,
        action_handle: ActionHandle,
        reflexes: Vec<ScheduledReflex>,
        config: SchedulerConfig,
        audit_db: Option<Arc<Db>>,
        audit_context: Option<StoredAuditContext>,
        action_gate: Option<ReflexActionGateHandle>,
        aim_track_target_source: Option<AimTrackTargetSourceHandle>,
    ) -> ReflexResult<SchedulerHandle> {
        config.validate()?;
        validate_reflexes(&reflexes)?;
        let subscription = event_bus
            .subscribe(EventFilter::All, Vec::new(), false)
            .map_err(|error| ReflexError::CapReached {
                detail: format!("scheduler event subscription failed: {error}"),
            })?;
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(VecDeque::with_capacity(config.sample_limit)));
        let registered_at = Utc::now();
        let statuses = Arc::new(Mutex::new(
            reflexes
                .iter()
                .map(|reflex| status_for_reflex(reflex, registered_at))
                .collect::<Vec<_>>(),
        ));
        let controls = Arc::new(Mutex::new(
            reflexes
                .iter()
                .map(|reflex| ReflexControl {
                    priority: reflex.priority,
                    active: true,
                })
                .collect::<Vec<_>>(),
        ));
        let aim_track_states = aim_track_states(&reflexes)?;
        let hold_move_states = hold_move_states(&reflexes)?;
        let hold_button_states = hold_button_states(&reflexes)?;
        let combo_states = combo_states(&reflexes);
        let path_follow_states = path_follow_states(&reflexes)?;
        let reflexes = reflexes
            .into_iter()
            .enumerate()
            .map(|(registration_order, reflex)| RuntimeReflex {
                registration_order,
                reflex,
            })
            .collect::<Vec<_>>();
        let on_event_states = reflexes
            .iter()
            .map(|_| OnEventState::default())
            .collect::<Vec<_>>();
        let starvation_states = reflexes
            .iter()
            .map(|_| crate::conflict::StarvationState::default())
            .collect::<Vec<_>>();

        let runtime = RuntimeState {
            event_bus,
            action_handle,
            reflexes,
            active_combos: Vec::new(),
            aim_track_states,
            hold_move_states,
            hold_button_states,
            combo_states,
            path_follow_states,
            on_event_states,
            starvation_states,
            aim_track_target_source,
            subscription,
            stop: Arc::clone(&stop),
            samples: Arc::clone(&samples),
            controls: Arc::clone(&controls),
            statuses: Arc::clone(&statuses),
            config,
            audit_db,
            audit_context,
            action_gate,
            tick_index: 0,
            last_tick_late_signal: None,
        };

        let join = thread::Builder::new()
            .name("synapse-reflex-scheduler".to_owned())
            .spawn(move || run_scheduler_thread(runtime))
            .map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("scheduler thread spawn failed: {error}"),
            })?;

        Ok(SchedulerHandle {
            stop,
            join: Some(join),
            samples,
            controls,
            statuses,
        })
    }
}

pub(crate) fn validate_reflexes(reflexes: &[ScheduledReflex]) -> ReflexResult<()> {
    if reflexes.len() > MAX_SCHEDULED_REFLEXES {
        return Err(ReflexError::CapReached {
            detail: format!(
                "scheduler reflex cap {MAX_SCHEDULED_REFLEXES} exceeded by {}",
                reflexes.len()
            ),
        });
    }
    let mut seen_ids = HashSet::with_capacity(reflexes.len());
    for reflex in reflexes {
        if !seen_ids.insert(reflex.reflex_id.as_str()) {
            return Err(ReflexError::ParamsInvalid {
                detail: format!("duplicate reflex id: {}", reflex.reflex_id),
            });
        }
        if reflex.priority > MAX_REFLEX_PRIORITY {
            return Err(ReflexError::PriorityInvalid {
                detail: format!(
                    "priority {} exceeds maximum {MAX_REFLEX_PRIORITY}",
                    reflex.priority
                ),
            });
        }
        reflex.trigger.validate()?;
        validate_lifetime(&reflex.lifetime)?;
    }
    Ok(())
}

#[path = "scheduler_stats.rs"]
mod scheduler_stats;
pub use scheduler_stats::p99_jitter_us;

#[path = "scheduler_combo.rs"]
mod scheduler_combo;

#[path = "scheduler_stateful.rs"]
mod scheduler_stateful;

#[path = "scheduler_handle.rs"]
mod scheduler_handle;

#[path = "scheduler_loop.rs"]
mod scheduler_loop;

#[path = "scheduler_tick.rs"]
mod scheduler_tick;

#[cfg(windows)]
#[path = "scheduler_windows.rs"]
mod windows_timer;
