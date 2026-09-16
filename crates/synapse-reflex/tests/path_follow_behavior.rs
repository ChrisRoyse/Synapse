use std::{error::Error, sync::Arc, time::Duration};

use synapse_action::{ActionHandle, ActionMessage};
use synapse_core::{
    Action, AimCurve, Backend, ButtonAction, MouseButton, MouseTarget, PathPoint, PathSpec, Point,
    ReflexState, SCHEMA_VERSION, StoredReflexAudit, StrokeTiming, VelocityProfile, error_codes,
};
use synapse_reflex::{
    EventBus, PathFollowContext, PathFollowController, PathFollowOutput, PathFollowParams,
    REFLEX_LIFETIME_EXPIRED_KIND, REFLEX_PATH_FOLLOW_COMPLETED_KIND, REFLEX_PATH_FOLLOW_TICK_KIND,
    ReflexScheduler, ScheduledReflex, SchedulerConfig,
};
use synapse_storage::{Db, cf, decode_json};
use tempfile::tempdir;
use tokio::sync::mpsc;

#[test]
fn path_follow_controller_streams_absolute_points_and_button_edges() -> Result<(), Box<dyn Error>> {
    let params = line_params(3, Some(MouseButton::Left));
    let mut controller = PathFollowController::new("path-follow-controller", params)?;
    let bus = EventBus::default();
    let (handle, mut rx) = ActionHandle::channel();

    let first = controller.step_dispatch(&context(1), &handle, &bus)?;
    assert!(matches!(
        first,
        PathFollowOutput::Started {
            actions: 2,
            remaining: 4,
            ..
        }
    ));
    assert_eq!(
        drain(&mut rx),
        vec![mouse_move(0, 0), mouse_button(ButtonAction::Down)]
    );

    let second = controller.step_dispatch(&context(1), &handle, &bus)?;
    assert_eq!(second.action_count(), 1);
    assert_eq!(drain(&mut rx), vec![mouse_move(1, 0)]);

    let third = controller.step_dispatch(&context(1), &handle, &bus)?;
    assert_eq!(third.action_count(), 1);
    assert_eq!(drain(&mut rx), vec![mouse_move(2, 0)]);

    let final_output = controller.step_dispatch(&context(1), &handle, &bus)?;
    assert!(matches!(
        final_output,
        PathFollowOutput::Completed {
            scheduled_actions: 6,
            dispatched_actions: 6,
            actions: 2,
            ..
        }
    ));
    assert_eq!(
        drain(&mut rx),
        vec![mouse_move(3, 0), mouse_button(ButtonAction::Up)]
    );
    assert!(controller.is_completed());
    Ok(())
}

#[test]
fn scheduler_path_follow_driver_expires_after_stream_and_writes_audit() -> Result<(), Box<dyn Error>>
{
    let temp = tempdir()?;
    let db_path = temp.path().join("db");
    let db = Arc::new(Db::open(&db_path, SCHEMA_VERSION)?);
    let bus = EventBus::default();
    let (action_handle, mut action_rx) = ActionHandle::channel();
    let reflex = ScheduledReflex::path_follow(
        "path-follow-driver",
        line_params(3, Some(MouseButton::Left)),
    );

    let mut scheduler = ReflexScheduler::spawn_with_audit_db(
        bus,
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(32),
        Arc::clone(&db),
    )?;
    let samples = scheduler.wait_for_samples(32, Duration::from_secs(3));
    let statuses = scheduler.statuses();
    scheduler.stop()?;

    let actions = drain(&mut action_rx);
    assert_eq!(
        actions,
        vec![
            mouse_move(0, 0),
            mouse_button(ButtonAction::Down),
            mouse_move(1, 0),
            mouse_move(2, 0),
            mouse_move(3, 0),
            mouse_button(ButtonAction::Up),
        ]
    );
    assert!(samples.iter().any(|sample| sample.dispatched_actions > 0));
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, ReflexState::Expired);
    assert_eq!(statuses[0].fire_count, 1);

    db.flush()?;
    let rows = db.scan_cf(cf::CF_REFLEX_AUDIT)?;
    let audits = rows
        .iter()
        .map(|(_key, value)| decode_json::<StoredReflexAudit>(value))
        .collect::<Result<Vec<_>, _>>()?;
    let tick_step_count = audits
        .iter()
        .filter(|audit| {
            audit.reflex_id == "path-follow-driver"
                && audit.details["kind"] == REFLEX_PATH_FOLLOW_TICK_KIND
        })
        .map(|audit| audit.steps.len())
        .sum::<usize>();
    assert_eq!(tick_step_count, actions.len());
    assert!(audits.iter().any(|audit| {
        audit.reflex_id == "path-follow-driver"
            && audit.status == ReflexState::Expired
            && audit.error_code.as_deref() == Some(error_codes::REFLEX_LIFETIME_EXPIRED)
            && audit.details["kind"] == REFLEX_LIFETIME_EXPIRED_KIND
            && audit.details["reason"] == "completed"
            && audit.details["path_follow_completion"]["kind"] == REFLEX_PATH_FOLLOW_COMPLETED_KIND
            && audit.details["path_follow_completion"]["point_stream_count"] == 4
            && audit.details["path_follow_completion"]["scheduled_actions"] == 6
            && audit.details["path_follow_completion"]["dispatched_actions"] == 6
    }));
    Ok(())
}

fn line_params(duration_ms: u32, button: Option<MouseButton>) -> PathFollowParams {
    PathFollowParams::new(
        PathSpec::Line {
            from: PathPoint::new(0.0, 0.0),
            to: PathPoint::new(f64::from(duration_ms), 0.0),
        },
        button,
        VelocityProfile::Constant,
        StrokeTiming::DurationMs { duration_ms },
        None,
        Backend::Software,
    )
}

const fn context(tick_ms: u64) -> PathFollowContext {
    PathFollowContext {
        tick_elapsed: Duration::from_millis(tick_ms),
    }
}

const fn mouse_move(x: i32, y: i32) -> Action {
    Action::MouseMove {
        to: MouseTarget::Screen {
            point: Point { x, y },
        },
        curve: AimCurve::Instant,
        duration_ms: 0,
        backend: Backend::Software,
    }
}

const fn mouse_button(action: ButtonAction) -> Action {
    Action::MouseButton {
        button: MouseButton::Left,
        action,
        hold_ms: 0,
        backend: Backend::Software,
    }
}

fn drain(rx: &mut mpsc::Receiver<ActionMessage>) -> Vec<Action> {
    let mut actions = Vec::new();
    while let Ok((action, _ack, _operator_panic_epoch_at_enqueue)) = rx.try_recv() {
        actions.push(action);
    }
    actions
}
