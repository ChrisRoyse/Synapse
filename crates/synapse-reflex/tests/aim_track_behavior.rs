#![allow(
    clippy::float_cmp,
    clippy::missing_const_for_fn,
    reason = "integration tests assert exact controller sentinel values and keep helpers simple"
)]

use std::time::Duration;

use chrono::Utc;
use synapse_action::ActionHandle;
use synapse_core::{
    Action, Backend, DEFAULT_AIM_TRACK_EMA_ALPHA, DetectedEntity, EventFilter, Point, Rect,
    ReflexAimAxis, error_codes,
};
use synapse_reflex::{
    AimTrackContext, AimTrackController, AimTrackOutput, AimTrackParams, AimTrackTarget, EventBus,
    REFLEX_TRACK_LOST_KIND, ReflexError, ResolvedElementBox,
};
use tokio::sync::mpsc;

#[test]
fn point_target_clamps_to_default_speed() -> Result<(), Box<dyn std::error::Error>> {
    let mut controller =
        AimTrackController::new("aim-default-speed", AimTrackParams::new(point(1000, 0)))?;
    let action = controller.step_action(&context(point_value(0, 0), &[], &[], 1, 16))?;

    assert_eq!(
        action,
        Some(Action::MouseMoveRelative {
            dx: 5.0,
            dy: 0.0,
            backend: Backend::Software,
        })
    );
    assert_eq!(controller.params().ema_alpha, DEFAULT_AIM_TRACK_EMA_ALPHA);
    Ok(())
}

#[test]
fn moving_track_target_uses_ema_smoothing() -> Result<(), Box<dyn std::error::Error>> {
    let mut params = AimTrackParams::new(AimTrackTarget::TrackId(42));
    params.deadzone_px = 0.0;
    params.max_speed_px_per_tick = 10.0;
    params.ema_alpha = 0.7;
    let mut controller = AimTrackController::new("aim-ema", params)?;

    let first_entities = vec![entity("first", 42, rect(10, 0, 0, 0))];
    let first = controller.step_action(&context(point_value(0, 0), &first_entities, &[], 1, 16))?;
    let second_entities = vec![entity("second", 42, rect(-10, 0, 0, 0))];
    let second =
        controller.step_action(&context(point_value(0, 0), &second_entities, &[], 2, 16))?;

    assert_eq!(
        first,
        Some(Action::MouseMoveRelative {
            dx: 10.0,
            dy: 0.0,
            backend: Backend::Software,
        })
    );
    let Some(Action::MouseMoveRelative { dx, dy, backend }) = second else {
        panic!("expected relative mouse action");
    };
    assert!((dx + 4.0).abs() <= f32::EPSILON * 2.0);
    assert_eq!(dy, 0.0);
    assert_eq!(backend, Backend::Software);
    Ok(())
}

#[test]
fn deadzone_and_axis_filter_suppress_expected_delta() -> Result<(), Box<dyn std::error::Error>> {
    let mut params = AimTrackParams::new(point(20, 3));
    params.axis = ReflexAimAxis::YOnly;
    params.deadzone_px = 4.0;
    let mut controller = AimTrackController::new("aim-axis-deadzone", params)?;

    let action = controller.step_action(&context(point_value(0, 0), &[], &[], 1, 16))?;

    assert_eq!(action, None);
    Ok(())
}

#[test]
fn step_dispatch_queues_action_and_reports_resolved_element()
-> Result<(), Box<dyn std::error::Error>> {
    let element_id = "0x10:2a".parse()?;
    let params = AimTrackParams::new(AimTrackTarget::ElementId(element_id));
    let mut controller = AimTrackController::new("aim-dispatch", params)?;
    let elements = vec![ResolvedElementBox {
        element_id: "0x10:2a".parse()?,
        bbox: rect(90, 40, 20, 20),
    }];
    let (handle, mut rx) = ActionHandle::channel();

    let output = controller.step_dispatch(
        &context(point_value(0, 0), &[], &elements, 7, 16),
        &handle,
        &EventBus::default(),
    )?;
    let (queued, _ack, _operator_panic_epoch_at_enqueue) = rx.try_recv()?;

    assert_eq!(
        queued,
        Action::MouseMoveRelative {
            dx: 4.472_136,
            dy: 2.236_068,
            backend: Backend::Software,
        }
    );
    assert!(matches!(
        output,
        AimTrackOutput::Dispatched {
            target: Point { x: 100, y: 50 },
            ..
        }
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn track_lost_emits_reflex_event_after_timeout() -> Result<(), Box<dyn std::error::Error>> {
    let mut controller = AimTrackController::new(
        "aim-lost",
        AimTrackParams::new(AimTrackTarget::EntityId("missing".to_owned())),
    )?;
    let bus = EventBus::default();
    let subscriber = bus.subscribe(
        EventFilter::Kind {
            kind: REFLEX_TRACK_LOST_KIND.to_owned(),
        },
        Vec::new(),
        false,
    )?;
    let (handle, mut rx) = ActionHandle::channel();

    assert!(matches!(
        controller.step_dispatch(&context(point_value(0, 0), &[], &[], 1, 250), &handle, &bus)?,
        AimTrackOutput::Idle {
            reason: "target_absent"
        }
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let error =
        controller.step_dispatch(&context(point_value(0, 0), &[], &[], 2, 251), &handle, &bus);

    assert!(matches!(
        error,
        Err(ReflexError::TrackLost { ref reflex_id }) if reflex_id == "aim-lost"
    ));
    let events = subscriber.drain();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, REFLEX_TRACK_LOST_KIND);
    assert_eq!(events[0].data["code"], error_codes::REFLEX_TRACK_LOST);
    assert_eq!(events[0].data["reflex_id"], "aim-lost");
    Ok(())
}

fn context<'a>(
    cursor: Point,
    entities: &'a [DetectedEntity],
    elements: &'a [ResolvedElementBox],
    tick_index: u64,
    elapsed_ms: u64,
) -> AimTrackContext<'a> {
    AimTrackContext {
        cursor,
        entities,
        elements,
        tick_index,
        tick_elapsed: Duration::from_millis(elapsed_ms),
    }
}

const fn point(x: i32, y: i32) -> AimTrackTarget {
    AimTrackTarget::Point(point_value(x, y))
}

const fn point_value(x: i32, y: i32) -> Point {
    Point { x, y }
}

const fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect { x, y, w, h }
}

fn entity(entity_id: &str, track_id: u64, bbox: Rect) -> DetectedEntity {
    let now = Utc::now();
    DetectedEntity {
        entity_id: entity_id.to_owned(),
        track_id,
        class_label: "target".to_owned(),
        bbox,
        confidence: 0.9,
        first_seen_at: now,
        last_seen_at: now,
        velocity_px_per_s: None,
    }
}
