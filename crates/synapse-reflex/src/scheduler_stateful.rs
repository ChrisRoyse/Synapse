use std::{collections::HashSet, time::Duration};

use chrono::Utc;
use serde_json::{Value, json};
use synapse_core::{
    Action, ButtonAction, Event, Point, ReflexAimAxis, ReflexButtonTarget, ReflexState,
    SCHEMA_VERSION, StoredReflexAudit, StoredReflexStep, StrokeMotionModel, error_codes,
};
use uuid::Uuid;

use super::RuntimeState;
use crate::{
    ReflexError,
    conflict::{ConflictCandidate, ConflictLoser, resolve_conflicts},
    dispatch::ReflexActionDispatchContext,
    kinds::{
        aim_track::{
            AimTrackContext, AimTrackOutput, AimTrackParams, AimTrackTargetSnapshot,
            REFLEX_AIM_TRACK_CORRECTION_KIND,
        },
        combo::{ComboContext, ComboOutput, ComboPhase},
        hold_button::{HoldButtonOutput, HoldButtonPhase},
        hold_lifetime::HoldLifetimeContext,
        hold_move::{HoldMoveOutput, HoldMovePhase},
        path_follow::{
            PathFollowContext, PathFollowDispatchRecord, PathFollowOutput, PathFollowPhase,
            REFLEX_PATH_FOLLOW_TICK_KIND,
        },
    },
    scheduler::ScheduledReflexDriver,
    write_audit,
};

pub(super) fn step_stateful_controllers(
    runtime: &mut RuntimeState,
    events: &[Event],
    elapsed: Duration,
    dispatched_actions: &mut usize,
    dispatch_blocked: &mut bool,
    starvation_losers: &mut Vec<ConflictLoser>,
) {
    let controls = super::lock_controls(&runtime.controls).clone();
    let selection = resolve_stateful_conflicts(runtime, &controls);
    starvation_losers.extend(selection.losers);
    for index in 0..runtime.reflexes.len() {
        if !controls.get(index).is_some_and(|control| control.active) {
            continue;
        }
        if selection.blocked_slots.contains(&index) {
            continue;
        }

        for outcome in [
            step_aim_track(runtime, index, elapsed),
            step_hold_move(runtime, index, events, elapsed),
            step_hold_button(runtime, index, events, elapsed),
            step_combo(runtime, index, elapsed),
            step_path_follow(runtime, index, elapsed),
        ]
        .into_iter()
        .flatten()
        {
            match outcome {
                StatefulOutcome::Progressed { actions } => {
                    *dispatched_actions = dispatched_actions.saturating_add(actions);
                }
                StatefulOutcome::Fired { actions } => {
                    *dispatched_actions = dispatched_actions.saturating_add(actions);
                    super::mark_reflex_fired(runtime, index);
                }
                StatefulOutcome::Expired { actions, reason } => {
                    *dispatched_actions = dispatched_actions.saturating_add(actions);
                    super::mark_reflex_lifetime_expired(runtime, index, reason);
                }
                StatefulOutcome::ComboCompleted { actions, details } => {
                    *dispatched_actions = dispatched_actions.saturating_add(actions);
                    super::mark_reflex_combo_completed(runtime, index, details);
                }
                StatefulOutcome::PathFollowCompleted { actions, details } => {
                    *dispatched_actions = dispatched_actions.saturating_add(actions);
                    super::mark_reflex_path_follow_completed(runtime, index, details);
                }
                StatefulOutcome::TrackLost {
                    lost_for,
                    target_context,
                } => {
                    super::mark_reflex_track_lost(runtime, index, lost_for, &target_context);
                }
                StatefulOutcome::Idle => {}
                StatefulOutcome::Blocked { error } => {
                    *dispatch_blocked = true;
                    if error.code() == error_codes::REFLEX_ACTION_PERMISSION_DENIED {
                        super::mark_reflex_action_denied(runtime, index);
                    } else {
                        super::mark_reflex_error(runtime, index, error.code());
                    }
                    warn_stateful_dispatch_blocked(index, &error);
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct StatefulConflictSelection {
    blocked_slots: HashSet<usize>,
    losers: Vec<ConflictLoser>,
}

fn resolve_stateful_conflicts(
    runtime: &RuntimeState,
    controls: &[super::ReflexControl],
) -> StatefulConflictSelection {
    let mut plans = Vec::new();
    for index in 0..runtime.reflexes.len() {
        if !controls.get(index).is_some_and(|control| control.active) {
            continue;
        }
        let actions = stateful_conflict_actions(runtime, index);
        if actions.is_empty() {
            continue;
        }
        plans.push(StatefulConflictPlan {
            reflex_index: index,
            actions,
        });
    }

    let candidates = plans
        .iter()
        .enumerate()
        .map(|(candidate_index, plan)| {
            let runtime_reflex = &runtime.reflexes[plan.reflex_index];
            let control = &controls[plan.reflex_index];
            ConflictCandidate::new(
                candidate_index,
                plan.reflex_index,
                runtime_reflex.reflex.reflex_id.clone(),
                control.priority,
                runtime_reflex.registration_order,
                runtime_reflex.reflex.exclusive,
                &plan.actions,
            )
        })
        .collect::<Vec<_>>();
    let resolution = resolve_conflicts(&candidates);
    let blocked_slots = resolution
        .losers
        .iter()
        .map(|loser| loser.loser_slot)
        .collect::<HashSet<_>>();

    StatefulConflictSelection {
        blocked_slots,
        losers: resolution.losers,
    }
}

#[derive(Clone, Debug)]
struct StatefulConflictPlan {
    reflex_index: usize,
    actions: Vec<Action>,
}

fn stateful_conflict_actions(runtime: &RuntimeState, index: usize) -> Vec<Action> {
    match &runtime.reflexes[index].reflex.driver {
        ScheduledReflexDriver::Actions => Vec::new(),
        ScheduledReflexDriver::AimTrack(_) => aim_track_conflict_actions(runtime, index),
        ScheduledReflexDriver::HoldMove(_) => hold_move_conflict_actions(runtime, index),
        ScheduledReflexDriver::HoldButton(_) => hold_button_conflict_actions(runtime, index),
        ScheduledReflexDriver::Combo(_) => combo_conflict_actions(runtime, index),
        ScheduledReflexDriver::PathFollow(_) => path_follow_conflict_actions(runtime, index),
    }
}

fn aim_track_conflict_actions(runtime: &RuntimeState, index: usize) -> Vec<Action> {
    let Some(controller) = runtime.aim_track_states.get(index).and_then(Option::as_ref) else {
        return Vec::new();
    };
    let params = controller.params();
    let Ok(cursor) = synapse_action::backend::software::cursor_position() else {
        return Vec::new();
    };
    let snapshot = aim_track_target_snapshot(runtime);
    let context = AimTrackContext {
        cursor,
        entities: &snapshot.entities,
        elements: &snapshot.elements,
        tick_index: runtime.tick_index,
        tick_elapsed: runtime.config.target_interval,
    };
    let Some(target) = controller.resolved_target(&context) else {
        return Vec::new();
    };
    if !aim_outside_deadzone(cursor, target, params.axis, params.deadzone_px) {
        return Vec::new();
    }
    vec![Action::MouseMoveRelative {
        dx: 0.0,
        dy: 0.0,
        backend: params.backend,
    }]
}

fn hold_move_conflict_actions(runtime: &RuntimeState, index: usize) -> Vec<Action> {
    let Some(controller) = runtime.hold_move_states.get(index).and_then(Option::as_ref) else {
        return Vec::new();
    };
    if !matches!(
        controller.phase(),
        HoldMovePhase::Pending | HoldMovePhase::Holding
    ) {
        return Vec::new();
    }
    controller
        .params()
        .keys
        .iter()
        .cloned()
        .map(|key| Action::KeyDown {
            key,
            backend: controller.params().backend,
        })
        .collect()
}

fn hold_button_conflict_actions(runtime: &RuntimeState, index: usize) -> Vec<Action> {
    let Some(controller) = runtime
        .hold_button_states
        .get(index)
        .and_then(Option::as_ref)
    else {
        return Vec::new();
    };
    if !matches!(
        controller.phase(),
        HoldButtonPhase::Pending | HoldButtonPhase::Holding
    ) {
        return Vec::new();
    }
    vec![hold_button_action(
        &controller.params().button,
        controller.params().backend,
    )]
}

fn combo_conflict_actions(runtime: &RuntimeState, index: usize) -> Vec<Action> {
    let Some(controller) = runtime.combo_states.get(index).and_then(Option::as_ref) else {
        return Vec::new();
    };
    if !matches!(
        controller.phase(),
        ComboPhase::Pending | ComboPhase::Running
    ) {
        return Vec::new();
    }
    let ScheduledReflexDriver::Combo(params) = &runtime.reflexes[index].reflex.driver else {
        return Vec::new();
    };
    vec![Action::Combo {
        steps: params.steps.clone(),
        backend: params.backend,
    }]
}

fn path_follow_conflict_actions(runtime: &RuntimeState, index: usize) -> Vec<Action> {
    let Some(controller) = runtime
        .path_follow_states
        .get(index)
        .and_then(Option::as_ref)
    else {
        return Vec::new();
    };
    if !matches!(
        controller.phase(),
        PathFollowPhase::Pending | PathFollowPhase::Running
    ) {
        return Vec::new();
    }
    let params = controller.params();
    vec![Action::MouseStroke {
        path: params.path.clone(),
        button: params.button,
        profile: params.profile,
        timing: params.timing.clone(),
        motion_model: StrokeMotionModel::Path,
        humanize: params.humanize,
        backend: params.backend,
    }]
}

const fn hold_button_action(button: &ReflexButtonTarget, backend: synapse_core::Backend) -> Action {
    match button {
        ReflexButtonTarget::Mouse { button } => Action::MouseButton {
            button: *button,
            action: ButtonAction::Down,
            hold_ms: 0,
            backend,
        },
        ReflexButtonTarget::Pad { pad, button } => Action::PadButton {
            pad: *pad,
            button: *button,
            action: ButtonAction::Down,
            hold_ms: 0,
        },
    }
}

fn aim_outside_deadzone(
    cursor: Point,
    target: Point,
    axis: ReflexAimAxis,
    deadzone_px: f32,
) -> bool {
    let mut dx = f64::from(target.x) - f64::from(cursor.x);
    let mut dy = f64::from(target.y) - f64::from(cursor.y);
    match axis {
        ReflexAimAxis::Xy => {}
        ReflexAimAxis::XOnly => dy = 0.0,
        ReflexAimAxis::YOnly => dx = 0.0,
    }
    dx.hypot(dy) > f64::from(deadzone_px)
}

#[derive(Clone, Debug)]
enum StatefulOutcome {
    Progressed {
        actions: usize,
    },
    Fired {
        actions: usize,
    },
    Expired {
        actions: usize,
        reason: &'static str,
    },
    ComboCompleted {
        actions: usize,
        details: Value,
    },
    PathFollowCompleted {
        actions: usize,
        details: Value,
    },
    TrackLost {
        lost_for: Duration,
        target_context: Value,
    },
    Idle,
    Blocked {
        error: ReflexError,
    },
}

fn step_combo(
    runtime: &mut RuntimeState,
    index: usize,
    elapsed: Duration,
) -> Option<StatefulOutcome> {
    let dispatch_context = dispatch_context(runtime);
    let reflex_id = runtime.reflexes[index].reflex.reflex_id.clone();
    let controller = runtime.combo_states.get_mut(index)?.as_mut()?;
    let context = ComboContext {
        tick_elapsed: elapsed,
    };
    match controller.step_dispatch_with(&context, &runtime.event_bus, |action| {
        dispatch_context.dispatch_action(&reflex_id, action)
    }) {
        Ok(ComboOutput::Completed { actions, .. }) => Some(StatefulOutcome::ComboCompleted {
            actions,
            details: controller.completion_audit_details(),
        }),
        Ok(output) if output.action_count() > 0 => Some(StatefulOutcome::Progressed {
            actions: output.action_count(),
        }),
        Ok(
            ComboOutput::Idle { .. } | ComboOutput::Started { .. } | ComboOutput::Dispatched { .. },
        ) => Some(StatefulOutcome::Idle),
        Err(error) => Some(StatefulOutcome::Blocked { error }),
    }
}

fn step_path_follow(
    runtime: &mut RuntimeState,
    index: usize,
    elapsed: Duration,
) -> Option<StatefulOutcome> {
    let dispatch_context = dispatch_context(runtime);
    let reflex_id = runtime.reflexes[index].reflex.reflex_id.clone();
    let context = PathFollowContext {
        tick_elapsed: elapsed,
    };
    let (result, completion_details) = {
        let controller = runtime.path_follow_states.get_mut(index)?.as_mut()?;
        let result = controller.step_dispatch_with(&context, &runtime.event_bus, |action| {
            dispatch_context.dispatch_action(&reflex_id, action)
        });
        let completion_details = if matches!(result, Ok(PathFollowOutput::Completed { .. })) {
            Some(controller.completion_audit_details())
        } else {
            None
        };
        (result, completion_details)
    };
    match result {
        Ok(PathFollowOutput::Completed {
            actions, records, ..
        }) => {
            write_path_follow_tick_audit(runtime, &reflex_id, &records);
            Some(StatefulOutcome::PathFollowCompleted {
                actions,
                details: completion_details.unwrap_or_else(|| json!({})),
            })
        }
        Ok(output) if output.action_count() > 0 => {
            write_path_follow_tick_audit(runtime, &reflex_id, output.records());
            Some(StatefulOutcome::Progressed {
                actions: output.action_count(),
            })
        }
        Ok(
            PathFollowOutput::Idle { .. }
            | PathFollowOutput::Started { .. }
            | PathFollowOutput::Dispatched { .. },
        ) => Some(StatefulOutcome::Idle),
        Err(error) => Some(StatefulOutcome::Blocked { error }),
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

fn step_aim_track(
    runtime: &mut RuntimeState,
    index: usize,
    elapsed: Duration,
) -> Option<StatefulOutcome> {
    runtime.aim_track_states.get(index)?.as_ref()?;
    let dispatch_context = dispatch_context(runtime);
    let reflex_id = runtime.reflexes[index].reflex.reflex_id.clone();
    let cursor = match synapse_action::backend::software::cursor_position() {
        Ok(cursor) => cursor,
        Err(error) => {
            return Some(StatefulOutcome::Blocked {
                error: ReflexError::ParamsInvalid {
                    detail: format!("aim_track cursor read failed: {error}"),
                },
            });
        }
    };
    let snapshot = aim_track_target_snapshot(runtime);
    let context = AimTrackContext {
        cursor,
        entities: &snapshot.entities,
        elements: &snapshot.elements,
        tick_index: runtime.tick_index,
        tick_elapsed: elapsed,
    };
    let (result, lost_for, params) = {
        let controller = runtime.aim_track_states.get_mut(index)?.as_mut()?;
        let result = controller.step_dispatch_with(&context, &runtime.event_bus, |action| {
            dispatch_context.dispatch_action(&reflex_id, action)
        });
        (result, controller.lost_for(), controller.params().clone())
    };
    match result {
        Ok(AimTrackOutput::Dispatched {
            action,
            target,
            raw_delta,
            smoothed_delta,
        }) => {
            write_aim_track_correction_audit(
                runtime,
                &reflex_id,
                &params,
                *action,
                cursor,
                target,
                raw_delta,
                smoothed_delta,
                &snapshot,
            );
            Some(StatefulOutcome::Fired { actions: 1 })
        }
        Ok(AimTrackOutput::Idle { .. }) => Some(StatefulOutcome::Idle),
        Err(ReflexError::TrackLost { .. }) => Some(StatefulOutcome::TrackLost {
            lost_for,
            target_context: aim_track_target_context(&params, &snapshot),
        }),
        Err(error) => Some(StatefulOutcome::Blocked { error }),
    }
}

fn step_hold_move(
    runtime: &mut RuntimeState,
    index: usize,
    events: &[Event],
    elapsed: Duration,
) -> Option<StatefulOutcome> {
    let dispatch_context = dispatch_context(runtime);
    let reflex_id = runtime.reflexes[index].reflex.reflex_id.clone();
    let controller = runtime.hold_move_states.get_mut(index)?.as_mut()?;
    let mut actions = 0_usize;
    let mut registered = false;
    if matches!(controller.phase(), HoldMovePhase::Pending) {
        match controller
            .register_dispatch_with(|action| dispatch_context.dispatch_action(&reflex_id, action))
        {
            Ok(HoldMoveOutput::Registered {
                actions: registered_actions,
            }) => {
                actions = actions.saturating_add(registered_actions);
                registered = true;
            }
            Ok(
                HoldMoveOutput::Holding { .. }
                | HoldMoveOutput::Reasserted { .. }
                | HoldMoveOutput::Released { .. }
                | HoldMoveOutput::Idle { .. },
            ) => {}
            Err(error) => return Some(StatefulOutcome::Blocked { error }),
        }
    }

    let context = HoldLifetimeContext {
        tick_elapsed: elapsed,
        events,
        cancelled: false,
    };
    match controller.step_dispatch_with(&context, &runtime.event_bus, |action| {
        dispatch_context.dispatch_action(&reflex_id, action)
    }) {
        Ok(HoldMoveOutput::Reasserted {
            actions: reasserted_actions,
            ..
        }) if registered => Some(StatefulOutcome::Fired {
            actions: actions.saturating_add(reasserted_actions),
        }),
        Ok(HoldMoveOutput::Reasserted {
            actions: reasserted_actions,
            ..
        }) => Some(StatefulOutcome::Progressed {
            actions: reasserted_actions,
        }),
        Ok(
            HoldMoveOutput::Holding { .. }
            | HoldMoveOutput::Idle { .. }
            | HoldMoveOutput::Registered { .. },
        ) if registered => Some(StatefulOutcome::Fired { actions }),
        Ok(
            HoldMoveOutput::Holding { .. }
            | HoldMoveOutput::Idle { .. }
            | HoldMoveOutput::Registered { .. },
        ) => Some(StatefulOutcome::Idle),
        Ok(HoldMoveOutput::Released {
            actions: released_actions,
            ..
        }) => Some(StatefulOutcome::Expired {
            actions: actions.saturating_add(released_actions),
            reason: "released",
        }),
        Err(ReflexError::LifetimeExpired { .. }) => Some(StatefulOutcome::Expired {
            actions: actions.saturating_add(controller.params().keys.len()),
            reason: "lifetime",
        }),
        Err(error) => Some(StatefulOutcome::Blocked { error }),
    }
}

fn step_hold_button(
    runtime: &mut RuntimeState,
    index: usize,
    events: &[Event],
    elapsed: Duration,
) -> Option<StatefulOutcome> {
    let dispatch_context = dispatch_context(runtime);
    let reflex_id = runtime.reflexes[index].reflex.reflex_id.clone();
    let controller = runtime.hold_button_states.get_mut(index)?.as_mut()?;
    let mut actions = 0_usize;
    let mut registered = false;
    if matches!(controller.phase(), HoldButtonPhase::Pending) {
        match controller
            .register_dispatch_with(|action| dispatch_context.dispatch_action(&reflex_id, action))
        {
            Ok(HoldButtonOutput::Registered) => {
                actions = actions.saturating_add(1);
                registered = true;
            }
            Ok(
                HoldButtonOutput::Holding { .. }
                | HoldButtonOutput::Released { .. }
                | HoldButtonOutput::Idle { .. },
            ) => {}
            Err(error) => return Some(StatefulOutcome::Blocked { error }),
        }
    }

    let context = HoldLifetimeContext {
        tick_elapsed: elapsed,
        events,
        cancelled: false,
    };
    match controller.step_dispatch_with(&context, &runtime.event_bus, |action| {
        dispatch_context.dispatch_action(&reflex_id, action)
    }) {
        Ok(
            HoldButtonOutput::Holding { .. }
            | HoldButtonOutput::Idle { .. }
            | HoldButtonOutput::Registered,
        ) if registered => Some(StatefulOutcome::Fired { actions }),
        Ok(
            HoldButtonOutput::Holding { .. }
            | HoldButtonOutput::Idle { .. }
            | HoldButtonOutput::Registered,
        ) => Some(StatefulOutcome::Idle),
        Ok(HoldButtonOutput::Released { .. }) => Some(StatefulOutcome::Expired {
            actions: actions.saturating_add(1),
            reason: "released",
        }),
        Err(ReflexError::LifetimeExpired { .. }) => Some(StatefulOutcome::Expired {
            actions: actions.saturating_add(1),
            reason: "lifetime",
        }),
        Err(error) => Some(StatefulOutcome::Blocked { error }),
    }
}

fn write_path_follow_tick_audit(
    runtime: &RuntimeState,
    reflex_id: &str,
    records: &[PathFollowDispatchRecord],
) {
    if records.is_empty() {
        return;
    }
    let Some(db) = runtime.audit_db.as_deref() else {
        return;
    };
    let steps = records
        .iter()
        .enumerate()
        .map(|(index, record)| StoredReflexStep {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            action: record.action.clone(),
            status: "dispatched".to_owned(),
            error_code: None,
        })
        .collect::<Vec<_>>();
    let dispatches = records
        .iter()
        .map(PathFollowDispatchRecord::audit_value)
        .collect::<Vec<_>>();
    let audit = StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: reflex_id.to_owned(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Active,
        event_id: None,
        audit_context: runtime.audit_context.clone(),
        steps,
        error_code: None,
        details: json!({
            "kind": REFLEX_PATH_FOLLOW_TICK_KIND,
            "tick_index": runtime.tick_index,
            "dispatch_count": records.len(),
            "dispatches": dispatches,
        }),
        redacted: false,
        redactions: Vec::new(),
    };
    if let Err(error) = write_audit(db, &audit) {
        tracing::warn!(
            component = "reflex_path_follow",
            reflex_id = %audit.reflex_id,
            audit_id = %audit.audit_id,
            detail = %error,
            "path_follow dispatch audit write failed"
        );
    }
}

fn warn_stateful_dispatch_blocked(index: usize, error: &ReflexError) {
    tracing::warn!(
        component = "reflex_scheduler",
        reflex_index = index,
        error_code = error.code(),
        detail = %error,
        code = error_codes::REFLEX_TICK_LATE,
        "reflex stateful controller dispatch blocked"
    );
}

fn aim_track_target_snapshot(runtime: &RuntimeState) -> AimTrackTargetSnapshot {
    runtime
        .aim_track_target_source
        .as_ref()
        .map_or_else(AimTrackTargetSnapshot::default, |source| source.snapshot())
}

#[allow(clippy::too_many_arguments)]
fn write_aim_track_correction_audit(
    runtime: &RuntimeState,
    reflex_id: &str,
    params: &AimTrackParams,
    action: Action,
    cursor: Point,
    target: Point,
    raw_delta: (f64, f64),
    smoothed_delta: (f64, f64),
    snapshot: &AimTrackTargetSnapshot,
) {
    let Some(db) = runtime.audit_db.as_deref() else {
        return;
    };
    let audit = StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: reflex_id.to_owned(),
        ts_ns: now_ts_ns(),
        status: ReflexState::Active,
        event_id: None,
        audit_context: runtime.audit_context.clone(),
        steps: vec![StoredReflexStep {
            index: 0,
            action,
            status: "dispatched".to_owned(),
            error_code: None,
        }],
        error_code: None,
        details: json!({
            "kind": REFLEX_AIM_TRACK_CORRECTION_KIND,
            "tick_index": runtime.tick_index,
            "cursor": point_value(cursor),
            "target": point_value(target),
            "raw_delta": delta_value(raw_delta),
            "smoothed_delta": delta_value(smoothed_delta),
            "params": aim_track_params_value(params),
            "target_context": aim_track_target_context(params, snapshot),
        }),
        redacted: false,
        redactions: Vec::new(),
    };
    if let Err(error) = write_audit(db, &audit) {
        tracing::warn!(
            component = "reflex_aim_track",
            reflex_id = %audit.reflex_id,
            audit_id = %audit.audit_id,
            detail = %error,
            "aim_track correction audit write failed"
        );
    }
}

fn aim_track_params_value(params: &AimTrackParams) -> Value {
    json!({
        "target": aim_track_target_value(params),
        "axis": aim_axis_value(params.axis),
        "gain": params.gain,
        "deadzone_px": params.deadzone_px,
        "max_speed_px_per_tick": params.max_speed_px_per_tick,
        "ema_alpha": params.ema_alpha,
        "backend": params.backend,
    })
}

fn aim_track_target_context(params: &AimTrackParams, snapshot: &AimTrackTargetSnapshot) -> Value {
    json!({
        "target": aim_track_target_value(params),
        "source_label": snapshot.source_label.as_deref(),
        "source_seq": snapshot.source_seq,
        "source_error": snapshot.source_error.as_deref(),
        "entity_count": snapshot.entities.len(),
        "element_count": snapshot.elements.len(),
        "entity_track_ids": snapshot
            .entities
            .iter()
            .map(|entity| entity.track_id)
            .collect::<Vec<_>>(),
        "element_ids": snapshot
            .elements
            .iter()
            .map(|element| element.element_id.clone())
            .collect::<Vec<_>>(),
    })
}

fn aim_track_target_value(params: &AimTrackParams) -> Value {
    match &params.target {
        crate::AimTrackTarget::Point(point) => json!({
            "kind": "screen",
            "point": point_value(*point),
        }),
        crate::AimTrackTarget::EntityId(entity_id) => json!({
            "kind": "entity",
            "entity_id": entity_id,
        }),
        crate::AimTrackTarget::TrackId(track_id) => json!({
            "kind": "track",
            "track_id": track_id,
        }),
        crate::AimTrackTarget::ElementId(element_id) => json!({
            "kind": "element",
            "element_id": element_id,
        }),
        crate::AimTrackTarget::ElementRect(rect) => json!({
            "kind": "element_rect",
            "rect": {
                "x": rect.x,
                "y": rect.y,
                "w": rect.w,
                "h": rect.h,
            },
        }),
    }
}

fn point_value(point: Point) -> Value {
    json!({
        "x": point.x,
        "y": point.y,
    })
}

fn delta_value(delta: (f64, f64)) -> Value {
    json!({
        "x": delta.0,
        "y": delta.1,
    })
}

const fn aim_axis_value(axis: ReflexAimAxis) -> &'static str {
    match axis {
        ReflexAimAxis::Xy => "xy",
        ReflexAimAxis::XOnly => "x_only",
        ReflexAimAxis::YOnly => "y_only",
    }
}

fn now_ts_ns() -> u64 {
    Utc::now()
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default()
}
