use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use synapse_action::{ActionHandle, StrokePlan, plan_timed_stroke, screen_point_from_path_point};
use synapse_core::{
    Action, AimCurve, Backend, ButtonAction, Event, EventSource, HumanizeParams, MouseButton,
    MouseTarget, PathSpec, ReflexId, StrokeMotionModel, StrokeTiming, VelocityProfile,
};

use crate::{EventBus, ReflexError, ReflexResult};

pub const REFLEX_PATH_FOLLOW_TICK_KIND: &str = "reflex_path_follow_tick";
pub const REFLEX_PATH_FOLLOW_COMPLETED_KIND: &str = "reflex_path_follow_completed";
pub const MAX_PATH_FOLLOW_SAMPLES: usize = 60_001;
const MAX_PATH_FOLLOW_DURATION_MS: f64 = 60_000.0;
const MAX_PATH_FOLLOW_PATH_POINTS: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct PathFollowParams {
    pub path: PathSpec,
    pub button: Option<MouseButton>,
    pub profile: VelocityProfile,
    pub timing: StrokeTiming,
    pub humanize: Option<HumanizeParams>,
    pub backend: Backend,
}

impl PathFollowParams {
    #[must_use]
    pub const fn new(
        path: PathSpec,
        button: Option<MouseButton>,
        profile: VelocityProfile,
        timing: StrokeTiming,
        humanize: Option<HumanizeParams>,
        backend: Backend,
    ) -> Self {
        Self {
            path,
            button,
            profile,
            timing,
            humanize,
            backend,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PathFollowPhase {
    Pending,
    Running,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathFollowContext {
    pub tick_elapsed: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathFollowOutput {
    Started {
        actions: usize,
        remaining: usize,
        records: Vec<PathFollowDispatchRecord>,
    },
    Dispatched {
        actions: usize,
        elapsed_ms: u128,
        remaining: usize,
        records: Vec<PathFollowDispatchRecord>,
    },
    Completed {
        scheduled_actions: usize,
        dispatched_actions: usize,
        actions: usize,
        records: Vec<PathFollowDispatchRecord>,
    },
    Idle {
        reason: &'static str,
    },
}

impl PathFollowOutput {
    #[must_use]
    pub const fn action_count(&self) -> usize {
        match self {
            Self::Started { actions, .. }
            | Self::Dispatched { actions, .. }
            | Self::Completed { actions, .. } => *actions,
            Self::Idle { .. } => 0,
        }
    }

    #[must_use]
    pub fn records(&self) -> &[PathFollowDispatchRecord] {
        match self {
            Self::Started { records, .. }
            | Self::Dispatched { records, .. }
            | Self::Completed { records, .. } => records,
            Self::Idle { .. } => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct TimedPathFollowAction {
    due_ms: u32,
    sequence: usize,
    sample_index: Option<usize>,
    action: Action,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathFollowDispatchRecord {
    pub due_ms: u32,
    pub sequence: usize,
    pub sample_index: Option<usize>,
    pub elapsed_ms: u128,
    pub action: Action,
}

impl PathFollowDispatchRecord {
    #[must_use]
    pub(crate) fn audit_value(&self) -> Value {
        json!({
            "due_ms": self.due_ms,
            "sequence": self.sequence,
            "sample_index": self.sample_index,
            "elapsed_ms": self.elapsed_ms,
            "jitter_ms": self.elapsed_ms.abs_diff(u128::from(self.due_ms)),
            "action": action_summary(&self.action),
        })
    }
}

#[derive(Clone, Debug)]
pub struct PathFollowController {
    reflex_id: ReflexId,
    params: PathFollowParams,
    plan: StrokePlan,
    scheduled: Vec<TimedPathFollowAction>,
    dispatched: Vec<PathFollowDispatchRecord>,
    cursor: usize,
    elapsed: Duration,
    phase: PathFollowPhase,
    completion_emitted: bool,
}

impl PathFollowController {
    /// Builds a precomputed 1 ms stroke-follow schedule for the scheduler.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` when the path or timing cannot produce
    /// a bounded point stream.
    pub fn new(reflex_id: impl Into<ReflexId>, params: PathFollowParams) -> ReflexResult<Self> {
        validate_path_point_cap(&params.path)?;
        let plan = plan_timed_stroke(
            &params.path,
            params.profile,
            &params.timing,
            StrokeMotionModel::Path,
            params.humanize,
        )
        .map_err(|error| ReflexError::ParamsInvalid {
            detail: format!("path_follow planning failed: {error}"),
        })?;
        validate_plan(&plan)?;
        let scheduled = build_schedule(&params, &plan)?;
        Ok(Self {
            reflex_id: reflex_id.into(),
            params,
            plan,
            scheduled,
            dispatched: Vec::new(),
            cursor: 0,
            elapsed: Duration::ZERO,
            phase: PathFollowPhase::Pending,
            completion_emitted: false,
        })
    }

    #[must_use]
    pub const fn phase(&self) -> PathFollowPhase {
        self.phase
    }

    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self.phase, PathFollowPhase::Completed)
    }

    #[must_use]
    pub const fn reflex_id(&self) -> &ReflexId {
        &self.reflex_id
    }

    #[must_use]
    pub const fn params(&self) -> &PathFollowParams {
        &self.params
    }

    #[must_use]
    pub(crate) fn completion_audit_details(&self) -> Value {
        let dispatches = self
            .dispatched
            .iter()
            .map(PathFollowDispatchRecord::audit_value)
            .collect::<Vec<_>>();
        let max_jitter_ms = dispatches
            .iter()
            .filter_map(|dispatch| dispatch.get("jitter_ms").and_then(Value::as_u64))
            .max()
            .unwrap_or(0);
        json!({
            "kind": REFLEX_PATH_FOLLOW_COMPLETED_KIND,
            "status": "completed",
            "path_kind": path_kind(&self.params.path),
            "button": self.params.button,
            "velocity_profile": self.params.profile,
            "humanized": self.params.humanize.is_some(),
            "point_stream_count": self.plan.samples.len(),
            "path_length_px": self.plan.path_length_px,
            "duration_ms": self.plan.duration_ms,
            "scheduled_actions": self.scheduled.len(),
            "dispatched_actions": self.cursor,
            "elapsed_ms": self.elapsed.as_millis(),
            "max_jitter_ms": max_jitter_ms,
            "dispatches": dispatches,
        })
    }

    pub(crate) fn step_dispatch_with<F>(
        &mut self,
        context: &PathFollowContext,
        event_bus: &EventBus,
        mut dispatch_action: F,
    ) -> ReflexResult<PathFollowOutput>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        match self.phase {
            PathFollowPhase::Pending => {
                self.phase = PathFollowPhase::Running;
                let records = self.dispatch_due_with(&mut dispatch_action)?;
                if self.finish_if_complete(event_bus) {
                    return Ok(self.completed_output(records));
                }
                Ok(PathFollowOutput::Started {
                    actions: records.len(),
                    remaining: self.remaining(),
                    records,
                })
            }
            PathFollowPhase::Running => {
                self.elapsed = self.elapsed.saturating_add(context.tick_elapsed);
                let records = self.dispatch_due_with(&mut dispatch_action)?;
                if self.finish_if_complete(event_bus) {
                    return Ok(self.completed_output(records));
                }
                Ok(PathFollowOutput::Dispatched {
                    actions: records.len(),
                    elapsed_ms: self.elapsed.as_millis(),
                    remaining: self.remaining(),
                    records,
                })
            }
            PathFollowPhase::Completed => Ok(PathFollowOutput::Idle {
                reason: "already_completed",
            }),
        }
    }

    /// Advances elapsed time and dispatches every newly due path action.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_PARAMS_INVALID` when the shared action queue cannot
    /// accept a due primitive action.
    pub fn step_dispatch(
        &mut self,
        context: &PathFollowContext,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<PathFollowOutput> {
        self.step_dispatch_with(context, event_bus, |action| {
            action_handle
                .try_execute(action.clone())
                .map_err(|error| ReflexError::ParamsInvalid {
                    detail: format!("path_follow action dispatch failed: {error}"),
                })
        })
    }

    fn dispatch_due_with<F>(
        &mut self,
        dispatch_action: &mut F,
    ) -> ReflexResult<Vec<PathFollowDispatchRecord>>
    where
        F: FnMut(&Action) -> ReflexResult<()>,
    {
        let mut records = Vec::new();
        while self
            .scheduled
            .get(self.cursor)
            .is_some_and(|action| u128::from(action.due_ms) <= self.elapsed.as_millis())
        {
            let scheduled = &self.scheduled[self.cursor];
            dispatch_action(&scheduled.action).map_err(|error| match error {
                ReflexError::ParamsInvalid { detail } => ReflexError::ParamsInvalid {
                    detail: format!(
                        "path_follow action dispatch failed at due_ms={} sequence={}: {detail}",
                        scheduled.due_ms, scheduled.sequence
                    ),
                },
                other => other,
            })?;
            let record = PathFollowDispatchRecord {
                due_ms: scheduled.due_ms,
                sequence: scheduled.sequence,
                sample_index: scheduled.sample_index,
                elapsed_ms: self.elapsed.as_millis(),
                action: scheduled.action.clone(),
            };
            self.dispatched.push(record.clone());
            records.push(record);
            self.cursor = self.cursor.saturating_add(1);
        }
        Ok(records)
    }

    fn finish_if_complete(&mut self, event_bus: &EventBus) -> bool {
        if self.cursor < self.scheduled.len() {
            return false;
        }
        self.phase = PathFollowPhase::Completed;
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
            kind: REFLEX_PATH_FOLLOW_COMPLETED_KIND.to_owned(),
            data: json!({
                "reflex_id": self.reflex_id,
                "status": "completed",
                "scheduled_actions": self.scheduled.len(),
                "dispatched_actions": self.cursor,
                "elapsed_ms": self.elapsed.as_millis(),
                "path_kind": path_kind(&self.params.path),
                "point_stream_count": self.plan.samples.len(),
            }),
            correlations: Vec::new(),
        };
        let _report = event_bus.publish(event);
    }

    const fn remaining(&self) -> usize {
        self.scheduled.len().saturating_sub(self.cursor)
    }

    const fn completed_output(&self, records: Vec<PathFollowDispatchRecord>) -> PathFollowOutput {
        PathFollowOutput::Completed {
            scheduled_actions: self.scheduled.len(),
            dispatched_actions: self.cursor,
            actions: records.len(),
            records,
        }
    }
}

fn validate_plan(plan: &StrokePlan) -> ReflexResult<()> {
    if plan.samples.is_empty() {
        return Err(ReflexError::ParamsInvalid {
            detail: "path_follow planner returned an empty point stream".to_owned(),
        });
    }
    if plan.samples.len() > MAX_PATH_FOLLOW_SAMPLES {
        return Err(ReflexError::ParamsInvalid {
            detail: format!(
                "path_follow planned point stream count {} exceeds max {MAX_PATH_FOLLOW_SAMPLES}",
                plan.samples.len()
            ),
        });
    }
    if !plan.duration_ms.is_finite() || plan.duration_ms <= 0.0 {
        return Err(ReflexError::ParamsInvalid {
            detail: format!(
                "path_follow duration_ms must be finite and greater than zero, got {}",
                plan.duration_ms
            ),
        });
    }
    if plan.duration_ms > MAX_PATH_FOLLOW_DURATION_MS {
        return Err(ReflexError::ParamsInvalid {
            detail: format!(
                "path_follow planned duration_ms {:.3} exceeds max {MAX_PATH_FOLLOW_DURATION_MS:.0}",
                plan.duration_ms
            ),
        });
    }
    Ok(())
}

fn validate_path_point_cap(path: &PathSpec) -> ReflexResult<()> {
    let count = control_point_count(path);
    if count > MAX_PATH_FOLLOW_PATH_POINTS {
        return Err(ReflexError::ParamsInvalid {
            detail: format!(
                "path_follow path control point count {count} exceeds max {MAX_PATH_FOLLOW_PATH_POINTS}"
            ),
        });
    }
    Ok(())
}

fn build_schedule(
    params: &PathFollowParams,
    plan: &StrokePlan,
) -> ReflexResult<Vec<TimedPathFollowAction>> {
    let mut scheduled = Vec::with_capacity(
        plan.samples
            .len()
            .saturating_add(params.button.map_or(0, |_| 2)),
    );
    let mut sequence = 0_usize;

    for (index, sample) in plan.samples.iter().enumerate() {
        let point = screen_point_from_path_point(sample.point, index).map_err(|error| {
            ReflexError::ParamsInvalid {
                detail: format!("path_follow sample {index} is invalid: {error}"),
            }
        })?;
        push_action(
            &mut scheduled,
            due_ms(sample.elapsed_ms)?,
            &mut sequence,
            Some(index),
            Action::MouseMove {
                to: MouseTarget::Screen { point },
                curve: AimCurve::Instant,
                duration_ms: 0,
                backend: params.backend,
            },
        );

        if index == 0
            && let Some(button) = params.button
        {
            push_action(
                &mut scheduled,
                due_ms(sample.elapsed_ms)?,
                &mut sequence,
                None,
                Action::MouseButton {
                    button,
                    action: ButtonAction::Down,
                    hold_ms: 0,
                    backend: params.backend,
                },
            );
        }
    }

    if let Some(button) = params.button {
        let final_due = plan
            .samples
            .last()
            .map_or(Ok(0), |sample| due_ms(sample.elapsed_ms))?;
        push_action(
            &mut scheduled,
            final_due,
            &mut sequence,
            None,
            Action::MouseButton {
                button,
                action: ButtonAction::Up,
                hold_ms: 0,
                backend: params.backend,
            },
        );
    }

    scheduled.sort_by_key(|action| (action.due_ms, action.sequence));
    Ok(scheduled)
}

fn push_action(
    scheduled: &mut Vec<TimedPathFollowAction>,
    due_ms: u32,
    sequence: &mut usize,
    sample_index: Option<usize>,
    action: Action,
) {
    scheduled.push(TimedPathFollowAction {
        due_ms,
        sequence: *sequence,
        sample_index,
        action,
    });
    *sequence = sequence.saturating_add(1);
}

fn due_ms(elapsed_ms: f64) -> ReflexResult<u32> {
    if !elapsed_ms.is_finite() || elapsed_ms < 0.0 || elapsed_ms > f64::from(u32::MAX) {
        return Err(ReflexError::ParamsInvalid {
            detail: format!("path_follow elapsed_ms is outside u32 range: {elapsed_ms}"),
        });
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "validated finite u32-range elapsed value is rounded up to scheduler milliseconds"
    )]
    Ok(elapsed_ms.ceil() as u32)
}

fn action_summary(action: &Action) -> Value {
    match action {
        Action::MouseMove {
            to,
            curve,
            duration_ms,
            ..
        } => json!({
            "kind": "mouse_move",
            "to": to,
            "curve": curve,
            "duration_ms": duration_ms,
        }),
        Action::MouseButton { button, action, .. } => json!({
            "kind": "mouse_button",
            "button": button,
            "action": action,
        }),
        other => json!({
            "kind": "other",
            "debug": format!("{other:?}"),
        }),
    }
}

const fn control_point_count(path: &PathSpec) -> usize {
    match path {
        PathSpec::Line { .. } => 2,
        PathSpec::Arc { .. } | PathSpec::Circle { .. } => 1,
        PathSpec::CubicBezier { .. } => 4,
        PathSpec::Polyline { points, .. } => points.len(),
        PathSpec::CatmullRom { waypoints, .. } => waypoints.len(),
    }
}

const fn path_kind(path: &PathSpec) -> &'static str {
    match path {
        PathSpec::Line { .. } => "line",
        PathSpec::Arc { .. } => "arc",
        PathSpec::Circle { .. } => "circle",
        PathSpec::CubicBezier { .. } => "cubic_bezier",
        PathSpec::Polyline { .. } => "polyline",
        PathSpec::CatmullRom { .. } => "catmull_rom",
    }
}
