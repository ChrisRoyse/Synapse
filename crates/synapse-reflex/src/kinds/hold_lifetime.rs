use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use synapse_core::{Event, EventSource, ReflexId, ReflexLifetime, error_codes};

use crate::{EventBus, ReflexError, ReflexResult};

pub const REFLEX_LIFETIME_EXPIRED_KIND: &str = "reflex_lifetime_expired";

#[derive(Clone, Debug)]
pub struct HoldLifetimeContext<'a> {
    pub tick_elapsed: Duration,
    pub events: &'a [Event],
    pub cancelled: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HoldReleaseReason {
    Cancelled,
    Duration,
    Deadline,
    Event,
    OneShot,
    SafetyCap,
}

impl HoldReleaseReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Duration => "duration",
            Self::Deadline => "deadline",
            Self::Event => "event",
            Self::OneShot => "one_shot",
            Self::SafetyCap => "safety_cap",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HoldLifetimeTracker {
    lifetime: ReflexLifetime,
    elapsed: Duration,
    safety_cap: Option<Duration>,
}

impl HoldLifetimeTracker {
    /// Creates a lifetime tracker for one held reflex.
    ///
    /// # Errors
    ///
    /// Returns `REFLEX_FILTER_INVALID` when an `UntilEvent` lifetime carries an
    /// invalid event filter.
    pub fn new(lifetime: ReflexLifetime, safety_cap: Option<Duration>) -> ReflexResult<Self> {
        validate_lifetime(&lifetime)?;
        Ok(Self {
            lifetime,
            elapsed: Duration::ZERO,
            safety_cap,
        })
    }

    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    #[must_use]
    pub const fn lifetime(&self) -> &ReflexLifetime {
        &self.lifetime
    }

    pub fn step(&mut self, context: &HoldLifetimeContext<'_>) -> Option<HoldReleaseReason> {
        if context.cancelled {
            return Some(HoldReleaseReason::Cancelled);
        }
        if matches!(self.lifetime, ReflexLifetime::OneShot) {
            return Some(HoldReleaseReason::OneShot);
        }
        self.elapsed = self.elapsed.saturating_add(context.tick_elapsed);
        if self.safety_cap.is_some_and(|cap| self.elapsed >= cap) {
            return Some(HoldReleaseReason::SafetyCap);
        }
        match &self.lifetime {
            ReflexLifetime::Duration { ms }
                if self.elapsed >= Duration::from_millis(u64::from(*ms)) =>
            {
                Some(HoldReleaseReason::Duration)
            }
            ReflexLifetime::UntilDeadline { ms }
                if self.elapsed >= Duration::from_millis(u64::from(*ms)) =>
            {
                Some(HoldReleaseReason::Deadline)
            }
            ReflexLifetime::UntilEvent { filter }
                if context.events.iter().any(|event| filter.matches(event)) =>
            {
                Some(HoldReleaseReason::Event)
            }
            ReflexLifetime::UntilCancelled
            | ReflexLifetime::OneShot
            | ReflexLifetime::Duration { .. }
            | ReflexLifetime::UntilDeadline { .. }
            | ReflexLifetime::UntilEvent { .. } => None,
        }
    }
}

pub(crate) fn validate_lifetime(lifetime: &ReflexLifetime) -> ReflexResult<()> {
    if let ReflexLifetime::UntilEvent { filter } = lifetime {
        filter
            .validate()
            .map_err(|error| ReflexError::FilterInvalid {
                detail: error.to_string(),
            })?;
    }
    Ok(())
}

pub(crate) fn emit_lifetime_expired(
    event_bus: &EventBus,
    reflex_id: &ReflexId,
    reason: HoldReleaseReason,
    elapsed: Duration,
) {
    let event = Event {
        seq: 0,
        at: Utc::now(),
        source: EventSource::Reflex,
        kind: REFLEX_LIFETIME_EXPIRED_KIND.to_owned(),
        data: json!({
            "code": error_codes::REFLEX_LIFETIME_EXPIRED,
            "reflex_id": reflex_id,
            "reason": reason.as_str(),
            "elapsed_ms": elapsed.as_millis(),
        }),
        correlations: Vec::new(),
    };
    let _report = event_bus.publish(event);
}

pub(crate) fn lifetime_expired(reflex_id: &ReflexId) -> ReflexError {
    ReflexError::LifetimeExpired {
        reflex_id: reflex_id.clone(),
    }
}
