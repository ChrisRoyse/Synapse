use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use synapse_core::{ReflexId, ReflexState, ReflexStatus, error_codes};

use crate::error::{ReflexError, ReflexResult};

use super::{
    ReflexControl, TickSample,
    scheduler_loop::{lock_controls, lock_samples, lock_statuses, status_index},
};

pub struct SchedulerHandle {
    pub(super) stop: Arc<AtomicBool>,
    pub(super) start: Arc<AtomicBool>,
    pub(super) join: Option<thread::JoinHandle<()>>,
    pub(super) samples: Arc<Mutex<VecDeque<TickSample>>>,
    pub(super) controls: Arc<Mutex<Vec<ReflexControl>>>,
    pub(super) statuses: Arc<Mutex<Vec<ReflexStatus>>>,
    pub(super) pending_terminals:
        Arc<Mutex<Vec<Option<crate::audit_offload::TerminalLifecycleToken>>>>,
    pub(super) audit_sink: Option<Arc<crate::ReflexAuditSink>>,
    /// Owns the off-tick refresher for the tick's lowered artifact (#1686).
    /// Held here so it is torn down with the scheduler it feeds.
    pub(super) lowered_refresher: crate::lowered::LoweredRefresher,
}

impl SchedulerHandle {
    /// Publishes a prepared replacement after its durable registration commit.
    ///
    /// This is deliberately infallible: the thread and every dependency were
    /// created during preparation, so the post-commit transition is one atomic
    /// store plus an unpark notification.
    pub(crate) fn activate_prepared(&self) {
        self.start.store(true, Ordering::Release);
        if let Some(join) = self.join.as_ref() {
            join.thread().unpark();
        }
    }

    /// The frozen guard-threshold feed the tick reads at tick start (#1686).
    #[must_use]
    pub const fn lowered_guard_thresholds(
        &self,
    ) -> &Arc<crate::lowered::LoweredGuardThresholdFeed> {
        self.lowered_refresher.feed()
    }

    #[must_use]
    pub fn samples(&self) -> Vec<TickSample> {
        lock_samples(&self.samples).iter().copied().collect()
    }

    #[must_use]
    pub fn wait_for_samples(&self, count: usize, timeout: Duration) -> Vec<TickSample> {
        let deadline = Instant::now() + timeout;
        loop {
            let samples = self.samples();
            if samples.len() >= count || Instant::now() >= deadline {
                return samples;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<ReflexStatus> {
        lock_statuses(&self.statuses).clone()
    }

    #[must_use]
    pub fn audit_queue_snapshot(&self) -> Option<crate::ReflexAuditQueueSnapshot> {
        self.audit_sink
            .as_deref()
            .map(crate::ReflexAuditSink::snapshot)
    }

    pub(crate) fn pending_terminal_ids(&self) -> ReflexResult<HashSet<ReflexId>> {
        self.pending_terminals
            .lock()
            .map(|pending| {
                pending
                    .iter()
                    .filter_map(Option::as_ref)
                    .filter(|token| !token.is_completed())
                    .map(|token| token.reflex_id().to_owned())
                    .collect()
            })
            .map_err(|_| ReflexError::ParamsInvalid {
                detail: "REFLEX_TERMINAL_PENDING_LOCK_POISONED: phase=scheduler_replacement_prepare; remediation=refuse scheduler replacement and restart from durable desired state"
                    .to_owned(),
            })
    }

    #[must_use]
    pub fn set_priority(&self, reflex_id: &str, priority: u32) -> bool {
        let Some(index) = status_index(&self.statuses, reflex_id) else {
            return false;
        };
        if let Some(control) = lock_controls(&self.controls).get_mut(index) {
            control.priority = priority;
        }
        if let Some(status) = lock_statuses(&self.statuses).get_mut(index) {
            status.priority = priority;
        }
        true
    }

    #[must_use]
    pub fn cancel_reflex(&self, reflex_id: &str) -> bool {
        let Some(index) = status_index(&self.statuses, reflex_id) else {
            return false;
        };
        if let Some(control) = lock_controls(&self.controls).get_mut(index) {
            control.active = false;
        }
        if let Some(status) = lock_statuses(&self.statuses).get_mut(index) {
            status.state = ReflexState::Cancelled;
        }
        true
    }

    #[must_use]
    pub fn disable_reflexes(&self, reflex_ids: &[ReflexId]) -> Vec<ReflexStatus> {
        if reflex_ids.is_empty() {
            return Vec::new();
        }
        let reflex_ids = reflex_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let indexes = {
            let statuses = lock_statuses(&self.statuses);
            statuses
                .iter()
                .enumerate()
                .filter_map(|(index, status)| {
                    (reflex_ids.contains(status.id.as_str())
                        && is_operator_disable_candidate(status.state))
                    .then_some(index)
                })
                .collect::<Vec<_>>()
        };
        if indexes.is_empty() {
            return Vec::new();
        }

        {
            let mut controls = lock_controls(&self.controls);
            for index in &indexes {
                if let Some(control) = controls.get_mut(*index) {
                    control.active = false;
                }
            }
        }

        let mut disabled = Vec::with_capacity(indexes.len());
        let mut statuses = lock_statuses(&self.statuses);
        for index in indexes {
            if let Some(status) = statuses.get_mut(index) {
                status.state = ReflexState::Disabled;
                status.last_error_code = Some(error_codes::REFLEX_DISABLED_BY_OPERATOR.to_owned());
                disabled.push(status.clone());
            }
        }
        disabled
    }

    #[must_use]
    pub fn disable_all_reflexes(&self) -> Vec<ReflexStatus> {
        let reflex_ids = lock_statuses(&self.statuses)
            .iter()
            .filter(|status| is_operator_disable_candidate(status.state))
            .map(|status| status.id.clone())
            .collect::<Vec<_>>();
        self.disable_reflexes(&reflex_ids)
    }

    /// Stops the scheduler thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the scheduler thread panicked before joining.
    pub fn stop(&mut self) -> ReflexResult<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.as_ref() {
            join.thread().unpark();
        }
        self.lowered_refresher.stop();
        if let Some(join) = self.join.take() {
            join.join().map_err(|error| ReflexError::ParamsInvalid {
                detail: format!("scheduler thread panicked: {error:?}"),
            })?;
        }
        Ok(())
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

const fn is_operator_disable_candidate(state: ReflexState) -> bool {
    matches!(
        state,
        ReflexState::Active | ReflexState::Paused | ReflexState::Starved
    )
}
