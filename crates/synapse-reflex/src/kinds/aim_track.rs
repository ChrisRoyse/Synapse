use std::{sync::Arc, time::Duration};

use chrono::Utc;
use serde_json::json;
use synapse_action::ActionHandle;
pub use synapse_core::DEFAULT_AIM_TRACK_EMA_ALPHA as DEFAULT_EMA_ALPHA;
use synapse_core::{
    Action, AimTarget, Backend, DetectedEntity, ElementId, EntityId, Event, EventSource, Point,
    Rect, ReflexAimAxis, ReflexId, error_codes,
};

use crate::{EventBus, ReflexError, ReflexResult};

pub const DEFAULT_MAX_SPEED_PX_PER_TICK: f32 = 5.0;
pub const DEFAULT_GAIN: f32 = 1.0;
pub const DEFAULT_DEADZONE_PX: f32 = 2.0;
pub const TRACK_LOST_AFTER: Duration = Duration::from_millis(500);
pub const REFLEX_TRACK_LOST_KIND: &str = "reflex_track_lost";
pub const REFLEX_AIM_TRACK_CORRECTION_KIND: &str = "aim_track_correction";

pub type AimTrackTargetSourceHandle = Arc<dyn AimTrackTargetSource>;

pub trait AimTrackTargetSource: Send + Sync {
    fn snapshot(&self) -> AimTrackTargetSnapshot;
}

#[derive(Clone, Debug, PartialEq)]
pub struct AimTrackParams {
    pub target: AimTrackTarget,
    pub axis: ReflexAimAxis,
    pub gain: f32,
    pub deadzone_px: f32,
    pub max_speed_px_per_tick: f32,
    pub ema_alpha: f32,
    pub backend: Backend,
}

impl AimTrackParams {
    #[must_use]
    pub const fn new(target: AimTrackTarget) -> Self {
        Self {
            target,
            axis: ReflexAimAxis::Xy,
            gain: DEFAULT_GAIN,
            deadzone_px: DEFAULT_DEADZONE_PX,
            max_speed_px_per_tick: DEFAULT_MAX_SPEED_PX_PER_TICK,
            ema_alpha: DEFAULT_EMA_ALPHA,
            backend: Backend::Software,
        }
    }

    fn validate(&self) -> ReflexResult<()> {
        validate_non_negative_finite("gain", self.gain)?;
        validate_non_negative_finite("deadzone_px", self.deadzone_px)?;
        validate_positive_finite("max_speed_px_per_tick", self.max_speed_px_per_tick)?;
        if !(0.0..=1.0).contains(&self.ema_alpha) || !self.ema_alpha.is_finite() {
            return Err(ReflexError::ParamsInvalid {
                detail: "ema_alpha must be finite and within 0.0..=1.0".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AimTrackTarget {
    Point(Point),
    EntityId(EntityId),
    TrackId(u64),
    ElementId(ElementId),
    ElementRect(Rect),
}

impl From<AimTarget> for AimTrackTarget {
    fn from(target: AimTarget) -> Self {
        match target {
            AimTarget::Screen { point } => Self::Point(point),
            AimTarget::Element { element_id } => Self::ElementId(element_id),
            AimTarget::Track { track_id } => Self::TrackId(track_id),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedElementBox {
    pub element_id: ElementId,
    pub bbox: Rect,
}

#[derive(Clone, Debug, Default)]
pub struct AimTrackTargetSnapshot {
    pub entities: Vec<DetectedEntity>,
    pub elements: Vec<ResolvedElementBox>,
    pub source_label: Option<String>,
    pub source_seq: Option<u64>,
    pub source_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AimTrackContext<'a> {
    pub cursor: Point,
    pub entities: &'a [DetectedEntity],
    pub elements: &'a [ResolvedElementBox],
    pub tick_index: u64,
    pub tick_elapsed: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AimTrackOutput {
    Dispatched {
        action: Box<Action>,
        target: Point,
        raw_delta: (f64, f64),
        smoothed_delta: (f64, f64),
    },
    Idle {
        reason: &'static str,
    },
}

#[derive(Clone, Debug)]
pub struct AimTrackController {
    reflex_id: ReflexId,
    params: AimTrackParams,
    smoothed_delta: Option<(f64, f64)>,
    lost_for: Duration,
}

impl AimTrackController {
    /// Creates an aim-track controller after validating its parameters.
    ///
    /// # Errors
    ///
    /// Returns `ReflexError::ParamsInvalid` when a numeric parameter is outside
    /// its accepted finite range.
    pub fn new(reflex_id: impl Into<ReflexId>, params: AimTrackParams) -> ReflexResult<Self> {
        params.validate()?;
        Ok(Self {
            reflex_id: reflex_id.into(),
            params,
            smoothed_delta: None,
            lost_for: Duration::ZERO,
        })
    }

    #[must_use]
    pub const fn params(&self) -> &AimTrackParams {
        &self.params
    }

    #[must_use]
    pub const fn lost_for(&self) -> Duration {
        self.lost_for
    }

    /// Computes and dispatches one aim-track tick.
    ///
    /// # Errors
    ///
    /// Returns a reflex error when target tracking is lost, action dispatch
    /// fails, or the generated action shape is invalid.
    pub fn step_dispatch(
        &mut self,
        context: &AimTrackContext<'_>,
        action_handle: &ActionHandle,
        event_bus: &EventBus,
    ) -> ReflexResult<AimTrackOutput> {
        self.step_dispatch_with(context, event_bus, |action| {
            action_handle
                .try_execute(action.clone())
                .map_err(|error| ReflexError::ParamsInvalid {
                    detail: format!("aim_track action dispatch failed: {error}"),
                })
        })
    }

    pub(crate) fn step_dispatch_with<F>(
        &mut self,
        context: &AimTrackContext<'_>,
        event_bus: &EventBus,
        dispatch_action: F,
    ) -> ReflexResult<AimTrackOutput>
    where
        F: FnOnce(&Action) -> ReflexResult<()>,
    {
        let action = match self.step_action(context) {
            Ok(Some(action)) => action,
            Ok(None) => {
                let reason = if self.resolve_target(context).is_some() {
                    "deadzone"
                } else {
                    "target_absent"
                };
                return Ok(AimTrackOutput::Idle { reason });
            }
            Err(error @ ReflexError::TrackLost { .. }) => {
                self.emit_track_lost(event_bus, context);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        dispatch_action(&action)?;
        let Action::MouseMoveRelative { dx, dy, .. } = action else {
            return Err(ReflexError::ParamsInvalid {
                detail: "aim_track generated non-relative-mouse action".to_owned(),
            });
        };
        let target = self
            .resolve_target(context)
            .ok_or_else(|| ReflexError::TrackLost {
                reflex_id: self.reflex_id.clone(),
            })?;
        Ok(AimTrackOutput::Dispatched {
            action: Box::new(action),
            target,
            raw_delta: raw_delta(context.cursor, target, self.params.axis),
            smoothed_delta: (f64::from(dx), f64::from(dy)),
        })
    }

    /// Computes the action for one aim-track tick without dispatching it.
    ///
    /// # Errors
    ///
    /// Returns `ReflexError::TrackLost` once the target has been absent beyond
    /// `TRACK_LOST_AFTER`.
    pub fn step_action(&mut self, context: &AimTrackContext<'_>) -> ReflexResult<Option<Action>> {
        let Some(target) = self.resolve_target(context) else {
            self.lost_for = self.lost_for.saturating_add(context.tick_elapsed);
            if self.lost_for > TRACK_LOST_AFTER {
                return Err(ReflexError::TrackLost {
                    reflex_id: self.reflex_id.clone(),
                });
            }
            return Ok(None);
        };
        self.lost_for = Duration::ZERO;
        let (dx, dy) = self.next_delta(context.cursor, target);
        if dx == 0.0 && dy == 0.0 {
            return Ok(None);
        }
        Ok(Some(Action::MouseMoveRelative {
            dx: f64_to_f32(dx),
            dy: f64_to_f32(dy),
            backend: self.params.backend,
        }))
    }

    fn next_delta(&mut self, cursor: Point, target: Point) -> (f64, f64) {
        let (raw_x, raw_y) = raw_delta(cursor, target, self.params.axis);
        if raw_x.hypot(raw_y) <= f64::from(self.params.deadzone_px) {
            self.smoothed_delta = Some((0.0, 0.0));
            return (0.0, 0.0);
        }
        let gained = (
            raw_x * f64::from(self.params.gain),
            raw_y * f64::from(self.params.gain),
        );
        let capped = clamp_delta(gained, f64::from(self.params.max_speed_px_per_tick));
        let alpha = f64::from(self.params.ema_alpha);
        let smoothed = self.smoothed_delta.map_or(capped, |previous| {
            (
                alpha.mul_add(capped.0, (1.0 - alpha) * previous.0),
                alpha.mul_add(capped.1, (1.0 - alpha) * previous.1),
            )
        });
        self.smoothed_delta = Some(smoothed);
        smoothed
    }

    fn resolve_target(&self, context: &AimTrackContext<'_>) -> Option<Point> {
        match &self.params.target {
            AimTrackTarget::Point(point) => Some(*point),
            AimTrackTarget::EntityId(entity_id) => context
                .entities
                .iter()
                .find(|entity| &entity.entity_id == entity_id)
                .map(|entity| rect_center(entity.bbox)),
            AimTrackTarget::TrackId(track_id) => context
                .entities
                .iter()
                .find(|entity| entity.track_id == *track_id)
                .map(|entity| rect_center(entity.bbox)),
            AimTrackTarget::ElementId(element_id) => context
                .elements
                .iter()
                .find(|element| &element.element_id == element_id)
                .map(|element| rect_center(element.bbox)),
            AimTrackTarget::ElementRect(rect) => Some(rect_center(*rect)),
        }
    }

    pub(crate) fn resolved_target(&self, context: &AimTrackContext<'_>) -> Option<Point> {
        self.resolve_target(context)
    }

    fn emit_track_lost(&self, event_bus: &EventBus, context: &AimTrackContext<'_>) {
        let event = Event {
            seq: context.tick_index,
            at: Utc::now(),
            source: EventSource::Reflex,
            kind: REFLEX_TRACK_LOST_KIND.to_owned(),
            data: json!({
                "code": error_codes::REFLEX_TRACK_LOST,
                "reflex_id": self.reflex_id,
                "lost_for_ms": self.lost_for.as_millis(),
            }),
            correlations: Vec::new(),
        };
        let _report = event_bus.publish(event);
    }
}

fn validate_non_negative_finite(name: &'static str, value: f32) -> ReflexResult<()> {
    if value.is_finite() && value >= 0.0 {
        return Ok(());
    }
    Err(ReflexError::ParamsInvalid {
        detail: format!("{name} must be finite and non-negative"),
    })
}

fn validate_positive_finite(name: &'static str, value: f32) -> ReflexResult<()> {
    if value.is_finite() && value > 0.0 {
        return Ok(());
    }
    Err(ReflexError::ParamsInvalid {
        detail: format!("{name} must be finite and positive"),
    })
}

fn raw_delta(cursor: Point, target: Point, axis: ReflexAimAxis) -> (f64, f64) {
    let mut dx = f64::from(target.x) - f64::from(cursor.x);
    let mut dy = f64::from(target.y) - f64::from(cursor.y);
    match axis {
        ReflexAimAxis::Xy => {}
        ReflexAimAxis::XOnly => dy = 0.0,
        ReflexAimAxis::YOnly => dx = 0.0,
    }
    (dx, dy)
}

fn clamp_delta(delta: (f64, f64), max_speed: f64) -> (f64, f64) {
    let distance = delta.0.hypot(delta.1);
    if distance <= max_speed {
        return delta;
    }
    let scale = max_speed / distance;
    (delta.0 * scale, delta.1 * scale)
}

const fn rect_center(rect: Rect) -> Point {
    Point {
        x: rect.x.saturating_add(rect.w / 2),
        y: rect.y.saturating_add(rect.h / 2),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn f64_to_f32(value: f64) -> f32 {
    value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}
