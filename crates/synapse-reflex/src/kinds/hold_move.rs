use std::{collections::HashSet, time::Duration};

use synapse_action::{ActionHandle, HELD_KEY_MAX_DURATION_MS};
use synapse_core::{Action, Backend, Key, ReflexId, ReflexLifetime};

use crate::{EventBus, ReflexError, ReflexResult};

use super::hold_lifetime::{
    HoldLifetimeContext, HoldLifetimeTracker, HoldReleaseReason, emit_lifetime_expired,
    lifetime_expired,
};

const HELD_KEY_REFLEX_SAFETY_GRACE_MS: u64 = 1_000;
const HOLD_MOVE_REASSERT_INTERVAL_MS: u64 = 50;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoldMoveParams {
    pub keys: Vec<Key>,
    pub backend: Backend,
    pub re_assert: bool,
}

impl HoldMoveParams {
    #[must_use]
    pub fn new(key: Key) -> Self {
        Self {
            keys: vec![key],
            backend: Backend::Software,
            re_assert: false,
        }
    }

    fn validate(&self) -> ReflexResult<()> {
        if self.keys.is_empty() {
            return Err(ReflexError::ParamsInvalid {
                detail: "hold_move requires at least one key".to_owned(),
            });
        }
        let mut seen = HashSet::with_capacity(self.keys.len());
        for key in &self.keys {
            if !seen.insert(key) {
                return Err(ReflexError::ParamsInvalid {
                    detail: "hold_move keys must be unique".to_owned(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HoldMovePhase {
    Pending,
    Holding,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HoldMoveOutput {
    Registered {
        actions: usize,
    },
    Holding {
        elapsed_ms: u128,
    },
    Reasserted {
        elapsed_ms: u128,
        actions: usize,
    },
    Released {
        reason: HoldReleaseReason,
        actions: usize,
    },
    Idle {
        reason: &'static str,
    },
}

#[derive(Clone, Debug)]
pub struct HoldMoveController {
    reflex_id: ReflexId,
    params: HoldMoveParams,
    lifetime: HoldLifetimeTracker,
    phase: HoldMovePhase,
    last_reassert_at: Option<Duration>,
}

impl HoldMoveController {
    /// Creates a hold-move controller in the pending phase.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` for an empty or duplicate key set, or
    /// `REFLEX_FILTER_INVALID` for an invalid `UntilEvent` lifetime filter.
    pub fn new(
        reflex_id: impl Into<ReflexId>,
        params: HoldMoveParams,
        lifetime: ReflexLifetime,
    ) -> ReflexResult<Self> {
        params.validate()?;
        Ok(Self {
            reflex_id: reflex_id.into(),
            params,
            lifetime: HoldLifetimeTracker::new(lifetime, Some(held_key_cap()))?,
            phase: HoldMovePhase::Pending,
            last_reassert_at: None,
        })
    }

    #[must_use]
    pub const fn phase(&self) -> HoldMovePhase {
        self.phase
    }

    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.lifetime.elapsed()
    }

    #[must_use]
    pub const fn params(&self) -> &HoldMoveParams {
        &self.params
    }

    #[must_use]
    pub const fn lifetime(&self) -> &ReflexLifetime {
        self.lifetime.lifetime()
    }

    /// Enqueues one `KeyDown` action for every configured key.
    ///
    /// # Errors
    ///
    /// Returns an action dispatch error mapped into `REFLEX_PARAMS_INVALID`
    /// when the shared action queue cannot accept the generated `KeyDown`.
    pub fn register_dispatch(
        &mut self,
        action_handle: &ActionHandle,
    ) -> ReflexResult<HoldMoveOutput> {
        self.register_dispatch_with(|action| dispatch(action_handle, action.clone()))
    }

    pub(crate) fn register_dispatch_with<F>(
        &mut self,
        mut dispatch_action: F,
    ) -> ReflexResult<HoldMoveOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        match self.phase {
            HoldMovePhase::Pending => {
                let actions = self.dispatch_down_with(&mut dispatch_action)?;
                self.phase = HoldMovePhase::Holding;
                self.last_reassert_at = Some(self.elapsed());
                Ok(HoldMoveOutput::Registered { actions })
            }
            HoldMovePhase::Holding => Ok(HoldMoveOutput::Idle {
                reason: "already_holding",
            }),
            HoldMovePhase::Released => Ok(HoldMoveOutput::Idle {
                reason: "already_released",
            }),
        }
    }

    /// Advances the lifetime clock and releases keys when the lifetime ends.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_LIFETIME_EXPIRED` after the release actions are queued,
    /// or `REFLEX_PARAMS_INVALID` when release dispatch fails.
    pub fn step_dispatch(
        &mut self,
        context: &HoldLifetimeContext<'_>,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<HoldMoveOutput> {
        self.step_dispatch_with(context, event_bus, |action| {
            dispatch(action_handle, action.clone())
        })
    }

    pub(crate) fn step_dispatch_with<F>(
        &mut self,
        context: &HoldLifetimeContext<'_>,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<HoldMoveOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        if !matches!(self.phase, HoldMovePhase::Holding) {
            return Ok(HoldMoveOutput::Idle {
                reason: "not_holding",
            });
        }
        let Some(reason) = self.lifetime.step(context) else {
            let elapsed_ms = self.elapsed().as_millis();
            if self.params.re_assert && self.reassert_due() {
                let actions = self.dispatch_down_with(&mut dispatch_action)?;
                self.last_reassert_at = Some(self.elapsed());
                return Ok(HoldMoveOutput::Reasserted {
                    elapsed_ms,
                    actions,
                });
            }
            return Ok(HoldMoveOutput::Holding { elapsed_ms });
        };
        let _output = self.release_with(event_bus, reason, &mut dispatch_action)?;
        Err(lifetime_expired(&self.reflex_id))
    }

    /// Releases held keys because the reflex was cancelled externally.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` when release dispatch fails.
    pub fn cancel_dispatch(
        &mut self,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<HoldMoveOutput> {
        self.cancel_dispatch_with(event_bus, |action| dispatch(action_handle, action.clone()))
    }

    pub(crate) fn cancel_dispatch_with<F>(
        &mut self,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<HoldMoveOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        self.release_with(
            event_bus,
            HoldReleaseReason::Cancelled,
            &mut dispatch_action,
        )
    }

    fn release_with<F>(
        &mut self,
        event_bus: &EventBus,
        reason: HoldReleaseReason,
        dispatch_action: &mut F,
    ) -> ReflexResult<HoldMoveOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        if !matches!(self.phase, HoldMovePhase::Holding) {
            return Ok(HoldMoveOutput::Idle {
                reason: "not_holding",
            });
        }
        let actions = self.dispatch_up_with(dispatch_action)?;
        self.phase = HoldMovePhase::Released;
        emit_lifetime_expired(event_bus, &self.reflex_id, reason, self.elapsed());
        Ok(HoldMoveOutput::Released { reason, actions })
    }

    fn dispatch_down_with<F>(&self, dispatch_action: &mut F) -> ReflexResult<usize>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        for key in &self.params.keys {
            dispatch_action(&Action::KeyDown {
                key: key.clone(),
                backend: self.params.backend,
            })?;
        }
        Ok(self.params.keys.len())
    }

    fn dispatch_up_with<F>(&self, dispatch_action: &mut F) -> ReflexResult<usize>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        for key in self.params.keys.iter().rev() {
            dispatch_action(&Action::KeyUp {
                key: key.clone(),
                backend: self.params.backend,
            })?;
        }
        Ok(self.params.keys.len())
    }

    fn reassert_due(&self) -> bool {
        let Some(last_reassert_at) = self.last_reassert_at else {
            return true;
        };
        self.elapsed().saturating_sub(last_reassert_at) >= reassert_interval()
    }
}

fn dispatch(action_handle: &ActionHandle, action: Action) -> ReflexResult<()> {
    action_handle
        .try_execute(action)
        .map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("hold_move action dispatch failed: {error}"),
        })
}

const fn held_key_cap() -> Duration {
    Duration::from_millis(HELD_KEY_MAX_DURATION_MS + HELD_KEY_REFLEX_SAFETY_GRACE_MS)
}

const fn reassert_interval() -> Duration {
    Duration::from_millis(HOLD_MOVE_REASSERT_INTERVAL_MS)
}
