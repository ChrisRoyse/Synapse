use std::time::Duration;

use synapse_core::{Action, ReflexId, error_codes};

use super::RuntimeState;
use crate::{
    dispatch::ReflexActionDispatchContext,
    error::ReflexResult,
    kinds::combo::{ComboContext, ComboController, ComboParams},
};

pub(super) fn step_active_combos(
    runtime: &mut RuntimeState,
    elapsed: Duration,
    dispatched_actions: &mut usize,
    dispatch_blocked: &mut bool,
) {
    let dispatch_context = dispatch_context(runtime);
    let mut next_active = Vec::with_capacity(runtime.active_combos.len());
    let active_combos = std::mem::take(&mut runtime.active_combos);
    let mut active_combos = active_combos.into_iter();
    while let Some(mut combo) = active_combos.next() {
        let reflex_id = combo.reflex_id().clone();
        match combo.step_dispatch_with(
            &ComboContext {
                tick_elapsed: elapsed,
            },
            &runtime.event_bus,
            |action| dispatch_context.dispatch_action(&reflex_id, action),
        ) {
            Ok(output) => {
                *dispatched_actions = dispatched_actions.saturating_add(output.action_count());
                if !combo.is_completed() {
                    next_active.push(combo);
                }
            }
            Err(error) => {
                *dispatch_blocked = true;
                let action_denied = error.code() == error_codes::REFLEX_ACTION_PERMISSION_DENIED;
                if action_denied
                    && let Some(index) =
                        super::scheduler_loop::status_index(&runtime.statuses, &reflex_id)
                {
                    super::mark_reflex_action_denied(runtime, index);
                }
                if !action_denied {
                    next_active.push(combo);
                    next_active.extend(active_combos);
                }
                tracing::warn!(
                    component = "reflex_scheduler",
                    error_code = error.code(),
                    detail = %error,
                    "combo action dispatch blocked"
                );
                break;
            }
        }
    }
    runtime.active_combos = next_active;
}

pub(super) fn dispatch_reflex_action(
    runtime: &mut RuntimeState,
    reflex_id: &ReflexId,
    action: Action,
) -> ReflexResult<usize> {
    let dispatch_context = dispatch_context(runtime);
    match action {
        Action::Combo { steps, backend } => {
            dispatch_context.ensure_action_allowed(
                reflex_id,
                &Action::Combo {
                    steps: steps.clone(),
                    backend,
                },
            )?;
            let mut combo =
                ComboController::new(reflex_id.clone(), ComboParams::new(steps, backend));
            let result = combo.start_dispatch_with(&runtime.event_bus, |action| {
                dispatch_context.dispatch_action(reflex_id, action)
            });
            let completed = combo.is_completed();
            let actions = match &result {
                Ok(output) => output.action_count(),
                Err(_error) => 0,
            };
            if !completed {
                runtime.active_combos.push(combo);
            }
            result?;
            Ok(actions)
        }
        action => {
            dispatch_context.dispatch_action(reflex_id, &action)?;
            Ok(1)
        }
    }
}

fn dispatch_context(runtime: &RuntimeState) -> ReflexActionDispatchContext {
    ReflexActionDispatchContext::new(
        runtime.action_handle.clone(),
        runtime.action_gate.clone(),
        runtime.audit_db.clone(),
        runtime.audit_context.clone(),
        runtime.tick_index,
    )
}
