use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use synapse_core::{Event, EventFilter};
use synapse_reflex::{EventBus, EventBusError, PublishReport};

mod lossy;
mod replay;
mod ring;
mod stream;

use ring::Subscription;

const LAST_EVENT_ID: &str = "Last-Event-ID";
const SUBSCRIPTION_ID_HEADER: &str = "Synapse-Subscription-Id";
const MANUAL_ENV: &str = "SYNAPSE_HTTP_SSE_MANUAL";

#[derive(Clone, Debug)]
pub struct SseState {
    inner: Arc<SseStateInner>,
}

#[derive(Debug)]
struct SseStateInner {
    event_bus: EventBus,
    subscriptions: Mutex<BTreeMap<String, Arc<Subscription>>>,
    subscription_owners: Mutex<BTreeMap<String, String>>,
    manual_routes_enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventsQuery {
    pub subscription_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StatsQuery {
    pub subscription_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublishRequest {
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Serialize)]
struct PublishResponse {
    matched: usize,
    queued: usize,
    dropped: u64,
    subscriptions_synced: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SseOpenError {
    BadLastEventId,
    SubscribeUnavailable(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SseSubscribeError {
    CapReached { limit: usize },
    FilterInvalid { detail: String },
    StateUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SseCancelError {
    NotFound,
    StateUnavailable,
}

impl SseState {
    pub(crate) fn with_max_subscriptions(max_subscriptions: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(SseStateInner {
                event_bus: EventBus::with_max_subscriptions(max_subscriptions),
                subscriptions: Mutex::new(BTreeMap::new()),
                subscription_owners: Mutex::new(BTreeMap::new()),
                manual_routes_enabled: manual_routes_enabled(),
            }),
        }
    }

    pub(super) fn open(&self, headers: &HeaderMap, query: EventsQuery) -> Response {
        let last_event_id = match replay::parse_last_event_id(headers) {
            Ok(value) => value,
            Err(error) => return error.into_response(),
        };
        let subscription = match self.subscription_for(query.subscription_id, last_event_id) {
            Ok(subscription) => subscription,
            Err(error) => return error.into_response(),
        };
        let frames = replay::frames_after(&subscription, last_event_id);
        stream::sse_response(subscription, frames, last_event_id)
    }

    pub(crate) fn subscribe(
        &self,
        filter: EventFilter,
        kinds: Vec<String>,
        snapshot_first: bool,
        owner_session_id: Option<String>,
    ) -> Result<String, SseSubscribeError> {
        self.create_subscription_with(filter, kinds, snapshot_first, owner_session_id)
            .map(|subscription| subscription.id().to_owned())
    }

    pub(crate) fn event_bus(&self) -> EventBus {
        self.inner.event_bus.clone()
    }

    pub(crate) fn active_subscription_count(&self) -> usize {
        match self.inner.subscriptions.lock() {
            Ok(subscriptions) => {
                let count = subscriptions.len();
                emit_sse_active_subscribers(count);
                count
            }
            Err(_poisoned) => 0,
        }
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<(), SseCancelError> {
        let (removed_from_map, active_count) = {
            let mut subscriptions = self
                .inner
                .subscriptions
                .lock()
                .map_err(|_| SseCancelError::StateUnavailable)?;
            let removed = subscriptions.remove(id).is_some();
            (removed, subscriptions.len())
        };
        emit_sse_active_subscribers(active_count);
        {
            let mut owners = self
                .inner
                .subscription_owners
                .lock()
                .map_err(|_| SseCancelError::StateUnavailable)?;
            owners.remove(id);
        }
        let removed_from_bus = self.inner.event_bus.unsubscribe(id);
        if removed_from_map || removed_from_bus {
            Ok(())
        } else {
            Err(SseCancelError::NotFound)
        }
    }

    pub(crate) fn subscription_ids_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<String>, SseCancelError> {
        let owners = self
            .inner
            .subscription_owners
            .lock()
            .map_err(|_| SseCancelError::StateUnavailable)?;
        Ok(owners
            .iter()
            .filter_map(|(subscription_id, owner)| {
                (owner == session_id).then(|| subscription_id.clone())
            })
            .collect())
    }

    pub(crate) fn subscription_owner_session_ids(&self) -> Result<Vec<String>, SseCancelError> {
        let owners = self
            .inner
            .subscription_owners
            .lock()
            .map_err(|_| SseCancelError::StateUnavailable)?;
        Ok(owners.values().cloned().collect())
    }

    pub(crate) fn cancel_session_subscriptions(
        &self,
        session_id: &str,
    ) -> Result<Vec<String>, SseCancelError> {
        let ids = self.subscription_ids_for_session(session_id)?;
        for id in &ids {
            self.cancel(id)?;
        }
        Ok(ids)
    }

    pub(super) fn publish(&self, request: PublishRequest) -> Response {
        if !self.inner.manual_routes_enabled {
            return StatusCode::NOT_FOUND.into_response();
        }
        let report = self.publish_events(request.events);
        let subscriptions_synced = self.sync_all();
        axum::Json(PublishResponse {
            matched: report.matched,
            queued: report.queued,
            dropped: report.dropped,
            subscriptions_synced,
        })
        .into_response()
    }

    fn publish_events(&self, events: Vec<Event>) -> PublishReport {
        let mut total = PublishReport::default();
        // ADR-0007: the manual HTTP route accepts a JSON array for operator
        // convenience, but every item is still published as an individual event.
        for event in events {
            let report = self.inner.event_bus.publish(event);
            total.matched = total.matched.saturating_add(report.matched);
            total.queued = total.queued.saturating_add(report.queued);
            total.dropped = total.dropped.saturating_add(report.dropped);
        }
        total
    }

    pub(super) fn stats(&self, query: &StatsQuery) -> Response {
        if !self.inner.manual_routes_enabled {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(subscription) = self.existing_subscription(&query.subscription_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        Self::sync_subscription(&subscription);
        axum::Json(subscription.stats()).into_response()
    }

    fn subscription_for(
        &self,
        subscription_id: Option<String>,
        last_event_id: Option<u64>,
    ) -> Result<Arc<Subscription>, SseOpenError> {
        if let Some(id) = subscription_id
            && let Some(subscription) = self.existing_subscription(&id)
        {
            Self::sync_subscription(&subscription);
            match last_event_id {
                None => return Ok(subscription),
                Some(last_id)
                    if subscription
                        .latest_seq()
                        .map_or(last_id == 0, |latest_seq| last_id <= latest_seq) =>
                {
                    return Ok(subscription);
                }
                Some(_) => {}
            }
        }
        self.create_subscription()
    }

    fn existing_subscription(&self, id: &str) -> Option<Arc<Subscription>> {
        let subscriptions = self.inner.subscriptions.lock().ok()?;
        subscriptions.get(id).cloned()
    }

    fn create_subscription(&self) -> Result<Arc<Subscription>, SseOpenError> {
        self.create_subscription_with(EventFilter::All, Vec::new(), false, None)
            .map_err(|error| SseOpenError::SubscribeUnavailable(error.code()))
    }

    fn create_subscription_with(
        &self,
        filter: EventFilter,
        kinds: Vec<String>,
        snapshot_first: bool,
        owner_session_id: Option<String>,
    ) -> Result<Arc<Subscription>, SseSubscribeError> {
        let handle = self
            .inner
            .event_bus
            .subscribe(filter, kinds, snapshot_first)
            .map_err(SseSubscribeError::from)?;
        let id = handle.id().to_owned();
        let subscription = Arc::new(Subscription::new(handle));
        {
            let mut subscriptions = self
                .inner
                .subscriptions
                .lock()
                .map_err(|_| SseSubscribeError::StateUnavailable)?;
            subscriptions.insert(id, Arc::clone(&subscription));
            emit_sse_active_subscribers(subscriptions.len());
        }
        if let Some(owner_session_id) = owner_session_id {
            let mut owners = self
                .inner
                .subscription_owners
                .lock()
                .map_err(|_| SseSubscribeError::StateUnavailable)?;
            owners.insert(subscription.id().to_owned(), owner_session_id);
        }
        Ok(subscription)
    }

    fn sync_all(&self) -> usize {
        let subscriptions = self
            .inner
            .subscriptions
            .lock()
            .map(|items| items.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for subscription in &subscriptions {
            Self::sync_subscription(subscription);
        }
        subscriptions.len()
    }

    fn sync_subscription(subscription: &Subscription) {
        let events = subscription.handle.drain();
        if events.is_empty() {
            let bus_dropped = subscription.handle.take_dropped_since_read();
            if bus_dropped > 0 {
                subscription.record_dropped(bus_dropped);
            }
            if subscription.handle.take_lossy() {
                subscription.record_lossy();
            }
            return;
        }
        let bus_dropped = subscription.handle.take_dropped_since_read();
        if bus_dropped > 0 {
            subscription.record_dropped(bus_dropped);
        }
        if subscription.handle.take_lossy() {
            subscription.record_lossy();
        }
        subscription.push_events(events);
    }
}

fn emit_sse_active_subscribers(count: usize) {
    synapse_telemetry::metrics::gauge!(synapse_telemetry::metrics::SSE_ACTIVE_SUBSCRIBERS)
        .set(usize_metric_value(count));
}

fn usize_metric_value(value: usize) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

impl SseOpenError {
    fn into_response(self) -> Response {
        match self {
            Self::BadLastEventId => {
                (StatusCode::BAD_REQUEST, "malformed Last-Event-ID").into_response()
            }
            Self::SubscribeUnavailable(code) => {
                (StatusCode::SERVICE_UNAVAILABLE, code).into_response()
            }
        }
    }
}

impl SseSubscribeError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::CapReached { .. } => synapse_core::error_codes::SUBSCRIPTION_CAP_REACHED,
            Self::FilterInvalid { .. } => synapse_core::error_codes::TOOL_PARAMS_INVALID,
            Self::StateUnavailable => synapse_core::error_codes::TOOL_INTERNAL_ERROR,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::CapReached { limit } => {
                format!("subscription cap reached: limit {limit}")
            }
            Self::FilterInvalid { detail } => format!("event filter invalid: {detail}"),
            Self::StateUnavailable => "subscription state lock poisoned".to_owned(),
        }
    }
}

impl SseCancelError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::NotFound => synapse_core::error_codes::SUBSCRIPTION_NOT_FOUND,
            Self::StateUnavailable => synapse_core::error_codes::TOOL_INTERNAL_ERROR,
        }
    }

    pub(crate) fn message(&self, subscription_id: &str) -> String {
        match self {
            Self::NotFound => format!("subscription not found: {subscription_id}"),
            Self::StateUnavailable => "subscription state lock poisoned".to_owned(),
        }
    }
}

impl From<EventBusError> for SseSubscribeError {
    fn from(value: EventBusError) -> Self {
        match value {
            EventBusError::SubscriptionCapReached { limit } => Self::CapReached { limit },
            EventBusError::FilterInvalid { detail } => Self::FilterInvalid { detail },
        }
    }
}

fn manual_routes_enabled() -> bool {
    std::env::var(MANUAL_ENV).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}
