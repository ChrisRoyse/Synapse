use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};
use synapse_action::ActionHandle;
use synapse_core::{Action, Backend, ComboInput, ComboStep, Event, EventSource, Key, ReflexId};

use crate::{EventBus, ReflexError, ReflexResult};

pub const REFLEX_COMBO_COMPLETED_KIND: &str = "reflex_combo_completed";

#[derive(Clone, Debug, PartialEq)]
pub struct ComboParams {
    pub steps: Vec<ComboStep>,
    pub backend: Backend,
}

impl ComboParams {
    #[must_use]
    pub const fn new(steps: Vec<ComboStep>, backend: Backend) -> Self {
        Self { steps, backend }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ComboPhase {
    Pending,
    Running,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComboContext {
    pub tick_elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComboOutput {
    Started {
        actions: usize,
        remaining: usize,
    },
    Dispatched {
        actions: usize,
        elapsed_ms: u128,
        remaining: usize,
    },
    Completed {
        scheduled_actions: usize,
        dispatched_actions: usize,
        actions: usize,
    },
    Idle {
        reason: &'static str,
    },
}

impl ComboOutput {
    #[must_use]
    pub const fn action_count(&self) -> usize {
        match self {
            Self::Started { actions, .. }
            | Self::Dispatched { actions, .. }
            | Self::Completed { actions, .. } => *actions,
            Self::Idle { .. } => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct TimedComboAction {
    due_ms: u32,
    sequence: usize,
    action: Action,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct ComboDispatchRecord {
    due_ms: u32,
    sequence: usize,
    elapsed_ms: u128,
    action: Value,
}

#[derive(Clone, Debug)]
pub struct ComboController {
    reflex_id: ReflexId,
    scheduled: Vec<TimedComboAction>,
    dispatched: Vec<ComboDispatchRecord>,
    cursor: usize,
    elapsed: Duration,
    phase: ComboPhase,
    completion_emitted: bool,
}

impl ComboController {
    #[must_use]
    pub fn new(reflex_id: impl Into<ReflexId>, params: ComboParams) -> Self {
        Self {
            reflex_id: reflex_id.into(),
            scheduled: build_schedule(params),
            dispatched: Vec::new(),
            cursor: 0,
            elapsed: Duration::ZERO,
            phase: ComboPhase::Pending,
            completion_emitted: false,
        }
    }

    #[must_use]
    pub const fn phase(&self) -> ComboPhase {
        self.phase
    }

    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self.phase, ComboPhase::Completed)
    }

    #[must_use]
    pub const fn reflex_id(&self) -> &ReflexId {
        &self.reflex_id
    }

    #[must_use]
    pub(crate) fn completion_audit_details(&self) -> Value {
        let dispatches = self
            .dispatched
            .iter()
            .map(|record| {
                json!({
                    "due_ms": record.due_ms,
                    "sequence": record.sequence,
                    "elapsed_ms": record.elapsed_ms,
                    "jitter_ms": record.elapsed_ms.abs_diff(u128::from(record.due_ms)),
                    "action": record.action,
                })
            })
            .collect::<Vec<_>>();
        let max_jitter_ms = dispatches
            .iter()
            .filter_map(|dispatch| dispatch.get("jitter_ms").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        json!({
            "kind": REFLEX_COMBO_COMPLETED_KIND,
            "status": "completed",
            "scheduled_actions": self.scheduled.len(),
            "dispatched_actions": self.cursor,
            "elapsed_ms": self.elapsed.as_millis(),
            "max_jitter_ms": max_jitter_ms,
            "dispatches": dispatches,
        })
    }

    /// Starts a combo and dispatches all actions due at trigger offset `0`.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` when the shared action queue cannot
    /// accept a due primitive action.
    pub fn start_dispatch(
        &mut self,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<ComboOutput> {
        self.start_dispatch_with(event_bus, |action| {
            action_handle
                .try_execute(action.clone())
                .map_err(|error| ReflexError::ParamsInvalid {
                    detail: format!("combo action dispatch failed: {error}"),
                })
        })
    }

    pub(crate) fn start_dispatch_with<F>(
        &mut self,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<ComboOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        match self.phase {
            ComboPhase::Pending => {
                self.phase = ComboPhase::Running;
                let actions = self.dispatch_due_with(&mut dispatch_action)?;
                if self.finish_if_complete(event_bus) {
                    return Ok(self.completed_output(actions));
                }
                Ok(ComboOutput::Started {
                    actions,
                    remaining: self.remaining(),
                })
            }
            ComboPhase::Running => Ok(ComboOutput::Idle {
                reason: "already_running",
            }),
            ComboPhase::Completed => Ok(ComboOutput::Idle {
                reason: "already_completed",
            }),
        }
    }

    /// Advances elapsed time and dispatches every newly due combo action.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` when the shared action queue cannot
    /// accept a due primitive action.
    pub fn step_dispatch(
        &mut self,
        context: &ComboContext,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<ComboOutput> {
        self.step_dispatch_with(context, event_bus, |action| {
            action_handle
                .try_execute(action.clone())
                .map_err(|error| ReflexError::ParamsInvalid {
                    detail: format!("combo action dispatch failed: {error}"),
                })
        })
    }

    pub(crate) fn step_dispatch_with<F>(
        &mut self,
        context: &ComboContext,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<ComboOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        match self.phase {
            ComboPhase::Pending => self.start_dispatch_with(event_bus, dispatch_action),
            ComboPhase::Running => {
                self.elapsed = self.elapsed.saturating_add(context.tick_elapsed);
                let actions = self.dispatch_due_with(&mut dispatch_action)?;
                if self.finish_if_complete(event_bus) {
                    return Ok(self.completed_output(actions));
                }
                Ok(ComboOutput::Dispatched {
                    actions,
                    elapsed_ms: self.elapsed.as_millis(),
                    remaining: self.remaining(),
                })
            }
            ComboPhase::Completed => Ok(ComboOutput::Idle {
                reason: "already_completed",
            }),
        }
    }

    fn dispatch_due_with<F>(&mut self, dispatch_action: &mut F) -> ReflexResult<usize>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        let mut dispatched = 0_usize;
        while self
            .scheduled
            .get(self.cursor)
            .is_some_and(|action| u128::from(action.due_ms) <= self.elapsed.as_millis())
        {
            let scheduled = &self.scheduled[self.cursor];
            dispatch_action(&scheduled.action).map_err(|error| match error {
                ReflexError::ParamsInvalid { detail } => ReflexError::ParamsInvalid {
                    detail: format!(
                        "combo action dispatch failed at due_ms={} sequence={}: {detail}",
                        scheduled.due_ms, scheduled.sequence
                    ),
                },
                other => other,
            })?;
            self.dispatched.push(ComboDispatchRecord {
                due_ms: scheduled.due_ms,
                sequence: scheduled.sequence,
                elapsed_ms: self.elapsed.as_millis(),
                action: combo_action_summary(&scheduled.action),
            });
            self.cursor = self.cursor.saturating_add(1);
            dispatched = dispatched.saturating_add(1);
        }
        Ok(dispatched)
    }

    fn finish_if_complete(&mut self, event_bus: &EventBus) -> bool {
        if self.cursor < self.scheduled.len() {
            return false;
        }
        self.phase = ComboPhase::Completed;
        self.emit_completed(event_bus);
        true
    }

    fn emit_completed(&mut self, event_bus: &EventBus) {
        if self.completion_emitted {
            return;
        }
        self.completion_emitted = true;
        let event = Event {
            seq: 0,
            at: Utc::now(),
            source: EventSource::Reflex,
            kind: REFLEX_COMBO_COMPLETED_KIND.to_owned(),
            data: json!({
                "reflex_id": self.reflex_id,
                "status": "completed",
                "scheduled_actions": self.scheduled.len(),
                "dispatched_actions": self.cursor,
                "elapsed_ms": self.elapsed.as_millis(),
                "dispatches": &self.dispatched,
            }),
            correlations: Vec::new(),
        };
        let _report = event_bus.publish(event);
    }

    const fn remaining(&self) -> usize {
        self.scheduled.len().saturating_sub(self.cursor)
    }

    const fn completed_output(&self, actions: usize) -> ComboOutput {
        ComboOutput::Completed {
            scheduled_actions: self.scheduled.len(),
            dispatched_actions: self.cursor,
            actions,
        }
    }
}

fn combo_action_summary(action: &Action) -> Value {
    match action {
        Action::KeyDown { key, .. } => keyed_action_summary("key_down", key),
        Action::KeyUp { key, .. } => keyed_action_summary("key_up", key),
        Action::MouseButton { button, action, .. } => json!({
            "kind": "mouse_button",
            "button": button,
            "action": action,
        }),
        Action::MouseMoveRelative { dx, dy, .. } => json!({
            "kind": "mouse_move_relative",
            "dx": dx,
            "dy": dy,
        }),
        other => json!({
            "kind": "other",
            "debug": format!("{other:?}"),
        }),
    }
}

fn keyed_action_summary(kind: &'static str, key: &Key) -> Value {
    json!({
        "kind": kind,
        "key": key,
    })
}

fn build_schedule(params: ComboParams) -> Vec<TimedComboAction> {
    let mut scheduled = Vec::new();
    let mut sequence = 0_usize;
    for step in params.steps {
        append_step(&mut scheduled, &mut sequence, step, params.backend);
    }
    scheduled.sort_by_key(|action| (action.due_ms, action.sequence));
    scheduled
}

fn append_step(
    scheduled: &mut Vec<TimedComboAction>,
    sequence: &mut usize,
    step: ComboStep,
    backend: Backend,
) {
    let due_ms = step.at_ms;
    match step.input {
        ComboInput::KeyDown { key } => {
            let action = Action::KeyDown { key, backend };
            push_action(scheduled, due_ms, sequence, action);
        }
        ComboInput::KeyUp { key } => {
            let action = Action::KeyUp { key, backend };
            push_action(scheduled, due_ms, sequence, action);
        }
        ComboInput::KeyPress { key, hold_ms } => {
            let down = Action::KeyDown {
                key: key.clone(),
                backend,
            };
            push_action(scheduled, due_ms, sequence, down);
            let up = Action::KeyUp { key, backend };
            push_action(
                scheduled,
                due_ms.saturating_add(u32::from(hold_ms)),
                sequence,
                up,
            );
        }
        ComboInput::MouseButton { button, action } => {
            let action = Action::MouseButton {
                button,
                action,
                hold_ms: 0,
                backend,
            };
            push_action(scheduled, due_ms, sequence, action);
        }
        ComboInput::MouseMoveRel { dx, dy } => {
            let action = Action::MouseMoveRelative { dx, dy, backend };
            push_action(scheduled, due_ms, sequence, action);
        }
        ComboInput::PadButton {
            pad,
            button,
            action,
        } => {
            let action = Action::PadButton {
                pad,
                button,
                action,
                hold_ms: 0,
            };
            push_action(scheduled, due_ms, sequence, action);
        }
        ComboInput::PadStick { pad, stick, x, y } => {
            let action = Action::PadStick { pad, stick, x, y };
            push_action(scheduled, due_ms, sequence, action);
        }
    }
}

fn push_action(
    scheduled: &mut Vec<TimedComboAction>,
    due_ms: u32,
    sequence: &mut usize,
    action: Action,
) {
    scheduled.push(TimedComboAction {
        due_ms,
        sequence: *sequence,
        action,
    });
    *sequence = sequence.saturating_add(1);
}
