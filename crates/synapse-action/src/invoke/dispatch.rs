use synapse_core::MouseButton;
#[cfg(windows)]
use synapse_core::{Action, AimCurve, AimNaturalParams, Backend, ButtonAction, MouseTarget};

use crate::{ActionBackend, ActionResult, EmitState};

use super::CoordinateFallbackPlan;

#[cfg(windows)]
pub(super) const FALLBACK_MOVE_DURATION_MS: u32 = 50;

#[cfg(windows)]
pub(super) fn emit_coordinate_fallback_click<B>(
    backend: &B,
    state: &mut EmitState,
    button: MouseButton,
    plan: CoordinateFallbackPlan,
) -> ActionResult<()>
where
    B: ActionBackend,
{
    backend.execute(
        &Action::MouseMove {
            to: MouseTarget::Screen {
                point: plan.screen_point,
            },
            curve: AimCurve::Natural {
                params: AimNaturalParams::FAST,
            },
            duration_ms: FALLBACK_MOVE_DURATION_MS,
            backend: Backend::Software,
        },
        state,
    )?;
    backend.execute(
        &Action::MouseButton {
            button,
            action: ButtonAction::Down,
            hold_ms: 0,
            backend: Backend::Software,
        },
        state,
    )?;
    backend.execute(
        &Action::MouseButton {
            button,
            action: ButtonAction::Up,
            hold_ms: 0,
            backend: Backend::Software,
        },
        state,
    )
}
