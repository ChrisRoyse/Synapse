use synapse_action::ActionHandle;
use synapse_core::{Action, Backend, ButtonAction, ReflexButtonTarget, ReflexId, ReflexLifetime};

use crate::{EventBus, ReflexError, ReflexResult};

use super::hold_lifetime::{
    HoldLifetimeContext, HoldLifetimeTracker, HoldReleaseReason, emit_lifetime_expired,
    lifetime_expired,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoldButtonParams {
    pub button: ReflexButtonTarget,
    pub backend: Backend,
}

impl HoldButtonParams {
    #[must_use]
    pub const fn new(button: ReflexButtonTarget) -> Self {
        Self {
            button,
            backend: Backend::Software,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HoldButtonPhase {
    Pending,
    Holding,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HoldButtonOutput {
    Registered,
    Holding { elapsed_ms: u128 },
    Released { reason: HoldReleaseReason },
    Idle { reason: &'static str },
}

#[derive(Clone, Debug)]
pub struct HoldButtonController {
    reflex_id: ReflexId,
    params: HoldButtonParams,
    lifetime: HoldLifetimeTracker,
    phase: HoldButtonPhase,
}

impl HoldButtonController {
    /// Creates a hold-button controller in the pending phase.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_FILTER_INVALID` when an `UntilEvent` lifetime carries an
    /// invalid event filter.
    pub fn new(
        reflex_id: impl Into<ReflexId>,
        params: HoldButtonParams,
        lifetime: ReflexLifetime,
    ) -> ReflexResult<Self> {
        Ok(Self {
            reflex_id: reflex_id.into(),
            params,
            lifetime: HoldLifetimeTracker::new(lifetime, None)?,
            phase: HoldButtonPhase::Pending,
        })
    }

    #[must_use]
    pub const fn phase(&self) -> HoldButtonPhase {
        self.phase
    }

    #[must_use]
    pub const fn params(&self) -> &HoldButtonParams {
        &self.params
    }

    /// Enqueues the button-down action.
    ///
    /// # Errors
    ///
    /// Returns an action dispatch error mapped into `REFLEX_PARAMS_INVALID`
    /// when the shared action queue cannot accept the generated action.
    pub fn register_dispatch(
        &mut self,
        action_handle: &ActionHandle,
    ) -> ReflexResult<HoldButtonOutput> {
        self.register_dispatch_with(|action| dispatch(action_handle, action.clone()))
    }

    pub(crate) fn register_dispatch_with<F>(
        &mut self,
        mut dispatch_action: F,
    ) -> ReflexResult<HoldButtonOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        match self.phase {
            HoldButtonPhase::Pending => {
                dispatch_action(&self.action(ButtonAction::Down))?;
                self.phase = HoldButtonPhase::Holding;
                Ok(HoldButtonOutput::Registered)
            }
            HoldButtonPhase::Holding => Ok(HoldButtonOutput::Idle {
                reason: "already_holding",
            }),
            HoldButtonPhase::Released => Ok(HoldButtonOutput::Idle {
                reason: "already_released",
            }),
        }
    }

    /// Advances the lifetime clock and releases the button when the lifetime ends.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_LIFETIME_EXPIRED` after the release action is queued, or
    /// `REFLEX_PARAMS_INVALID` when release dispatch fails.
    pub fn step_dispatch(
        &mut self,
        context: &HoldLifetimeContext<'_>,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<HoldButtonOutput> {
        self.step_dispatch_with(context, event_bus, |action| {
            dispatch(action_handle, action.clone())
        })
    }

    pub(crate) fn step_dispatch_with<F>(
        &mut self,
        context: &HoldLifetimeContext<'_>,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<HoldButtonOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        if !matches!(self.phase, HoldButtonPhase::Holding) {
            return Ok(HoldButtonOutput::Idle {
                reason: "not_holding",
            });
        }
        let Some(reason) = self.lifetime.step(context) else {
            return Ok(HoldButtonOutput::Holding {
                elapsed_ms: self.lifetime.elapsed().as_millis(),
            });
        };
        let _output = self.release_with(event_bus, reason, &mut dispatch_action)?;
        Err(lifetime_expired(&self.reflex_id))
    }

    /// Releases the button because the reflex was cancelled externally.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` when release dispatch fails.
    pub fn cancel_dispatch(
        &mut self,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<HoldButtonOutput> {
        self.cancel_dispatch_with(event_bus, |action| dispatch(action_handle, action.clone()))
    }

    pub(crate) fn cancel_dispatch_with<F>(
        &mut self,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<HoldButtonOutput>
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
    ) -> ReflexResult<HoldButtonOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        if !matches!(self.phase, HoldButtonPhase::Holding) {
            return Ok(HoldButtonOutput::Idle {
                reason: "not_holding",
            });
        }
        dispatch_action(&self.action(ButtonAction::Up))?;
        self.phase = HoldButtonPhase::Released;
        emit_lifetime_expired(event_bus, &self.reflex_id, reason, self.lifetime.elapsed());
        Ok(HoldButtonOutput::Released { reason })
    }

    const fn action(&self, action: ButtonAction) -> Action {
        match self.params.button {
            ReflexButtonTarget::Mouse { button } => Action::MouseButton {
                button,
                action,
                hold_ms: 0,
                backend: self.params.backend,
            },
            ReflexButtonTarget::Pad { pad, button } => Action::PadButton {
                pad,
                button,
                action,
                hold_ms: 0,
            },
        }
    }
}

fn dispatch(action_handle: &ActionHandle, action: Action) -> ReflexResult<()> {
    action_handle
        .try_execute(action)
        .map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("hold_button action dispatch failed: {error}"),
        })
}
