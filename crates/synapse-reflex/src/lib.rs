mod action_combo_bridge;
pub mod audit;
mod audit_state;
pub mod bus;
pub mod conflict;
mod dispatch;
pub mod error;
pub mod kinds;
mod lifecycle;
mod listing;
mod runtime;
pub mod scheduler;
mod storage;

#[cfg(test)]
mod tests;

pub use action_combo_bridge::install_action_combo_scheduler;
pub use audit::write_audit;
pub use bus::{
    DEFAULT_MAX_SUBSCRIPTIONS, DEFAULT_MAX_SUBSCRIPTIONS_NONZERO, EVENTS_DROPPED_METRIC, EventBus,
    EventBusError, EventBusResult, PublishReport, SUBSCRIBER_QUEUE_CAPACITY, SubscriberHandle,
};
pub use conflict::{REFLEX_STARVED_KIND, STARVATION_AFTER};
pub use dispatch::{
    REFLEX_ACTION_DENIED_STEP_STATUS, REFLEX_ACTION_PERMISSION_DENIED_KIND, ReflexActionGate,
    ReflexActionGateHandle, ReflexActionPermissionDenied,
};
pub use error::{ReflexError, ReflexResult};
pub use kinds::aim_track::{
    AimTrackContext, AimTrackController, AimTrackOutput, AimTrackParams, AimTrackTarget,
    AimTrackTargetSnapshot, AimTrackTargetSource, AimTrackTargetSourceHandle, DEFAULT_EMA_ALPHA,
    DEFAULT_MAX_SPEED_PX_PER_TICK, REFLEX_AIM_TRACK_CORRECTION_KIND, REFLEX_TRACK_LOST_KIND,
    ResolvedElementBox, TRACK_LOST_AFTER,
};
pub use kinds::combo::{
    ComboContext, ComboController, ComboOutput, ComboParams, ComboPhase,
    REFLEX_COMBO_COMPLETED_KIND,
};
pub use kinds::hold_button::{
    HoldButtonController, HoldButtonOutput, HoldButtonParams, HoldButtonPhase,
};
pub use kinds::hold_lifetime::{
    HoldLifetimeContext, HoldReleaseReason, REFLEX_LIFETIME_EXPIRED_KIND,
};
pub use kinds::hold_move::{HoldMoveController, HoldMoveOutput, HoldMoveParams, HoldMovePhase};
pub use kinds::on_event::{
    MAX_ON_EVENT_FIRINGS_PER_TICK, REFLEX_DEBOUNCED_KIND, REFLEX_FIRED_KIND,
    REFLEX_RECURSION_LIMIT_KIND,
};
pub use kinds::path_follow::{
    MAX_PATH_FOLLOW_SAMPLES, PathFollowContext, PathFollowController, PathFollowOutput,
    PathFollowParams, PathFollowPhase, REFLEX_PATH_FOLLOW_COMPLETED_KIND,
    REFLEX_PATH_FOLLOW_TICK_KIND,
};
pub use runtime::ReflexRuntime;
pub use scheduler::{
    DEFAULT_REFLEX_PRIORITY, MAX_REFLEX_PRIORITY, MAX_SCHEDULED_REFLEXES, REFLEX_TICK_LATE_KIND,
    ReflexScheduler, ScheduledReflex, ScheduledReflexDriver, SchedulerConfig, SchedulerHandle,
    SchedulerTrigger, TickSample, p99_jitter_us,
};

use synapse_core::ReflexStatus;

pub const REFLEX_CANCELLED_KIND: &str = "reflex_cancelled";
pub const REFLEX_DISABLED_KIND: &str = "reflex_disabled_by_operator";
pub const REFLEX_REGISTERED_KIND: &str = "reflex_registered";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReflexCancelOutcome {
    Cancelled { status: ReflexStatus },
    NotFound,
    AlreadyExpired { status: ReflexStatus },
}
