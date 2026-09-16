#[cfg(windows)]
use std::{collections::HashMap, sync::Arc, time::Duration};

use synapse_core::Action;

#[cfg(windows)]
use synapse_core::{
    ButtonAction, GamepadController, GamepadReport, PadButton, PadId, Stick, Trigger,
};

use crate::{ActionError, EmitState, recovery};

#[cfg(windows)]
use super::{
    error::{add_pad_context, map_vigem_error, retry_vigem_update, routed_non_gamepad_error},
    pad::VigemPad,
    reports::{ds4_report_snapshot, x360_report_snapshot, xgamepad_from_snapshot},
    state::{
        apply_pad_button, apply_pad_report, apply_pad_stick, apply_pad_trigger, is_neutral_report,
        neutral_gamepad_report, push_unique, report_for_pad,
    },
};

#[cfg(not(windows))]
use super::error::action_kind;

#[cfg(windows)]
#[derive(Debug, Default)]
pub(super) struct VigemBackendInner {
    client: Option<Arc<vigem_client::Client>>,
    pads: HashMap<PadId, VigemPad>,
}

#[cfg(windows)]
impl VigemBackendInner {
    pub(super) fn ensure_ready(&mut self) -> Result<(), ActionError> {
        self.ensure_client().map(|_client| ())
    }

    pub(super) fn execute(
        &mut self,
        action: &Action,
        state: &mut EmitState,
    ) -> Result<(), ActionError> {
        match action {
            Action::PadButton {
                pad,
                button,
                action: btn_action,
                hold_ms,
            } => self.pad_button(state, *pad, *button, *btn_action, *hold_ms),
            Action::PadStick { pad, stick, x, y } => self.pad_stick(state, *pad, *stick, *x, *y),
            Action::PadTrigger {
                pad,
                trigger,
                value,
            } => self.pad_trigger(state, *pad, *trigger, *value),
            Action::PadReport { pad, report } => self.pad_report(state, *pad, report.clone()),
            Action::ReleaseAll => self.release_all(state),
            _ => Err(routed_non_gamepad_error(action)),
        }
    }

    fn ensure_client(&mut self) -> Result<Arc<vigem_client::Client>, ActionError> {
        if let Some(client) = &self.client {
            return Ok(Arc::clone(client));
        }

        let client = Arc::new(
            vigem_client::Client::connect()
                .map_err(|err| map_vigem_error("connect_vigembus", err))?,
        );
        self.client = Some(Arc::clone(&client));
        Ok(client)
    }

    fn ensure_pad(
        &mut self,
        pad: PadId,
        controller: GamepadController,
    ) -> Result<&mut VigemPad, ActionError> {
        if self
            .pads
            .get(&pad)
            .is_some_and(|target| target.controller() != controller)
            && let Some(mut old_target) = self.pads.remove(&pad)
        {
            old_target.neutralize()?;
        }

        if !self.pads.contains_key(&pad) {
            let client = self.ensure_client()?;
            let target = match controller {
                GamepadController::X360 => Self::plug_x360(client)?,
                GamepadController::Ds4 => Self::plug_ds4(client)?,
            };
            self.pads.insert(pad, target);
        }

        self.pads
            .get_mut(&pad)
            .ok_or_else(|| ActionError::VigemPluginFailed {
                detail: format!("backend=vigem reason=pad target missing after plug-in pad={pad}"),
            })
    }

    fn plug_x360(client: Arc<vigem_client::Client>) -> Result<VigemPad, ActionError> {
        let mut target =
            vigem_client::Xbox360Wired::new(client, vigem_client::TargetId::XBOX360_WIRED);
        target
            .plugin()
            .map_err(|err| map_vigem_error("plugin_x360_target", err))?;
        target
            .wait_ready()
            .map_err(|err| map_vigem_error("wait_ready_x360_target", err))?;
        let neutral = neutral_gamepad_report(GamepadController::X360);
        let neutral_gamepad = xgamepad_from_snapshot(x360_report_snapshot(&neutral));
        retry_vigem_update("initial_neutral_x360_report", || {
            target.update(&neutral_gamepad)
        })?;
        Ok(VigemPad::new_x360(target, neutral))
    }

    fn plug_ds4(client: Arc<vigem_client::Client>) -> Result<VigemPad, ActionError> {
        let mut target =
            vigem_client::DualShock4Wired::new(client, vigem_client::TargetId::DUALSHOCK4_WIRED);
        target
            .plugin()
            .map_err(|err| map_vigem_error("plugin_ds4_target", err))?;
        target
            .wait_ready()
            .map_err(|err| map_vigem_error("wait_ready_ds4_target", err))?;
        let neutral = neutral_gamepad_report(GamepadController::Ds4);
        let neutral_report = ds4_report_snapshot(&neutral).into_vigem_report();
        retry_vigem_update("initial_neutral_ds4_report", || {
            target.update(&neutral_report)
        })?;
        Ok(VigemPad::new_ds4(target, neutral))
    }

    fn pad_button(
        &mut self,
        state: &mut EmitState,
        pad: PadId,
        button: PadButton,
        action: ButtonAction,
        hold_ms: u32,
    ) -> Result<(), ActionError> {
        match action {
            ButtonAction::Down => {
                let mut report = report_for_pad(state, pad);
                push_unique(&mut report.buttons, button);
                self.send_report(pad, &report)?;
                apply_pad_button(state, pad, button, ButtonAction::Down);
                Ok(())
            }
            ButtonAction::Up => {
                let mut report = report_for_pad(state, pad);
                report.buttons.retain(|held| *held != button);
                self.send_report(pad, &report)?;
                apply_pad_button(state, pad, button, ButtonAction::Up);
                Ok(())
            }
            ButtonAction::Press => {
                let mut report = report_for_pad(state, pad);
                push_unique(&mut report.buttons, button);
                self.send_report(pad, &report)?;
                apply_pad_button(state, pad, button, ButtonAction::Down);
                if hold_ms > 0 {
                    std::thread::sleep(Duration::from_millis(u64::from(hold_ms)));
                }
                report.buttons.retain(|held| *held != button);
                self.send_report(pad, &report)?;
                apply_pad_button(state, pad, button, ButtonAction::Up);
                Ok(())
            }
        }
    }

    fn pad_stick(
        &mut self,
        state: &mut EmitState,
        pad: PadId,
        stick: Stick,
        x: f32,
        y: f32,
    ) -> Result<(), ActionError> {
        let mut report = report_for_pad(state, pad);
        match stick {
            Stick::Left => report.thumb_l = (x, y),
            Stick::Right => report.thumb_r = (x, y),
        }
        self.send_report(pad, &report)?;
        apply_pad_stick(state, pad, stick, x, y);
        Ok(())
    }

    fn pad_trigger(
        &mut self,
        state: &mut EmitState,
        pad: PadId,
        trigger: Trigger,
        value: f32,
    ) -> Result<(), ActionError> {
        let mut report = report_for_pad(state, pad);
        match trigger {
            Trigger::Left => report.lt = value,
            Trigger::Right => report.rt = value,
        }
        self.send_report(pad, &report)?;
        apply_pad_trigger(state, pad, trigger, value);
        Ok(())
    }

    fn pad_report(
        &mut self,
        state: &mut EmitState,
        pad: PadId,
        report: GamepadReport,
    ) -> Result<(), ActionError> {
        if is_neutral_report(&report) {
            self.send_report(pad, &report)?;
            apply_pad_report(state, pad, report);
            recovery::clear_held_pad(pad)?;
            return Ok(());
        }
        recovery::record_held_pad_report(pad, &report)?;
        self.send_report(pad, &report)?;
        apply_pad_report(state, pad, report);
        Ok(())
    }

    fn send_report(&mut self, pad: PadId, report: &GamepadReport) -> Result<(), ActionError> {
        let target = self.ensure_pad(pad, report.controller)?;
        target.update(report)
    }

    fn release_all(&mut self, state: &mut EmitState) -> Result<(), ActionError> {
        for (pad, target) in &mut self.pads {
            target
                .neutralize()
                .map_err(|err| add_pad_context(*pad, err))?;
            recovery::clear_held_pad(*pad)?;
        }
        state.pad_state.clear();
        Ok(())
    }

    pub(super) fn neutral_all_for_drop(&mut self) {
        for target in self.pads.values_mut() {
            let _neutral_result = target.neutralize();
        }
    }
}

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub(super) struct VigemBackendInner;

#[cfg(not(windows))]
impl VigemBackendInner {
    #[allow(clippy::needless_pass_by_ref_mut, clippy::unused_self)]
    pub(super) fn ensure_ready(&mut self) -> Result<(), ActionError> {
        Err(ActionError::BackendUnavailable {
            detail: "vigem backend requires Windows and the ViGEmBus driver".to_owned(),
        })
    }

    #[allow(clippy::needless_pass_by_ref_mut, clippy::unused_self)]
    pub(super) fn execute(
        &mut self,
        action: &Action,
        state: &mut EmitState,
    ) -> Result<(), ActionError> {
        if matches!(action, Action::ReleaseAll) && state.pad_state.is_empty() {
            return Ok(());
        }
        Err(ActionError::BackendUnavailable {
            detail: format!(
                "vigem backend requires Windows and the ViGEmBus driver; action_kind={}",
                action_kind(action)
            ),
        })
    }
}
