use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use crossbeam::channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use synapse_core::{Event, EventFilter, SubscriptionId, error_codes, new_subscription_id};
use thiserror::Error;

pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 4096;
pub const DEFAULT_MAX_SUBSCRIPTIONS: usize = 64;
pub const DEFAULT_MAX_SUBSCRIPTIONS_NONZERO: NonZeroUsize =
    match NonZeroUsize::new(DEFAULT_MAX_SUBSCRIPTIONS) {
        Some(value) => value,
        None => NonZeroUsize::MIN,
    };
pub const EVENTS_DROPPED_METRIC: &str = "events_dropped_for_subscriber";
const EVENTS_PUBLISHED_METRIC: &str = "events_published_total";

pub type EventBusResult<T> = Result<T, EventBusError>;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum EventBusError {
    #[error("subscription cap reached: limit {limit}")]
    SubscriptionCapReached { limit: usize },
    #[error("event filter invalid: {detail}")]
    FilterInvalid { detail: String },
}

impl EventBusError {
    #[must_use]
    #[tracing::instrument(skip_all, fields(event_bus_error = ?self))]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SubscriptionCapReached { .. } => error_codes::SUBSCRIPTION_CAP_REACHED,
            Self::FilterInvalid { .. } => error_codes::REFLEX_FILTER_INVALID,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

#[derive(Debug)]
struct EventBusInner {
    subscribers: ArcSwap<Vec<Arc<Subscriber>>>,
    updates: Mutex<()>,
    max_subscriptions: NonZeroUsize,
}

impl Default for EventBusInner {
    fn default() -> Self {
        Self {
            subscribers: ArcSwap::from_pointee(Vec::new()),
            updates: Mutex::new(()),
            max_subscriptions: DEFAULT_MAX_SUBSCRIPTIONS_NONZERO,
        }
    }
}

#[derive(Debug)]
struct Subscriber {
    id: SubscriptionId,
    filter: EventFilter,
    kinds: BTreeSet<String>,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    lossy: Arc<std::sync::atomic::AtomicBool>,
    dropped_since_read: Arc<AtomicU64>,
}

#[derive(Clone, Debug)]
pub struct SubscriberHandle {
    registration: Arc<SubscriberRegistration>,
    receiver: Receiver<Event>,
    lossy: Arc<std::sync::atomic::AtomicBool>,
    dropped_since_read: Arc<AtomicU64>,
    snapshot_first: bool,
}

#[derive(Debug)]
struct SubscriberRegistration {
    id: SubscriptionId,
    inner: Arc<EventBusInner>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PublishReport {
    pub matched: usize,
    pub queued: usize,
    pub dropped: u64,
}

impl EventBus {
    #[must_use]
    pub fn with_max_subscriptions(max_subscriptions: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(EventBusInner {
                subscribers: ArcSwap::from_pointee(Vec::new()),
                updates: Mutex::new(()),
                max_subscriptions,
            }),
        }
    }

    #[must_use]
    pub fn max_subscriptions(&self) -> usize {
        self.inner.max_subscriptions.get()
    }

    /// Subscribes to matching events with a bounded per-subscriber queue.
    ///
    /// An empty `kinds` list means all event kinds are allowed, subject to
    /// `filter`.
    ///
    /// # Errors
    ///
    /// Returns [`EventBusError::FilterInvalid`] when the filter fails schema
    /// validation, or [`EventBusError::SubscriptionCapReached`] when the
    /// configured active subscription cap is already reached.
    #[tracing::instrument(
        skip_all,
        fields(kinds_count = kinds.len(), snapshot_first)
    )]
    pub fn subscribe(
        &self,
        filter: EventFilter,
        kinds: Vec<String>,
        snapshot_first: bool,
    ) -> EventBusResult<SubscriberHandle> {
        filter
            .validate()
            .map_err(|error| EventBusError::FilterInvalid {
                detail: error.to_string(),
            })?;

        let _guard = self.lock_updates();
        let current = self.inner.subscribers.load_full();
        let max_subscriptions = self.max_subscriptions();
        if current.len() >= max_subscriptions {
            return Err(EventBusError::SubscriptionCapReached {
                limit: max_subscriptions,
            });
        }

        let id = new_subscription_id();
        let (sender, receiver) = bounded(SUBSCRIBER_QUEUE_CAPACITY);
        let lossy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped_since_read = Arc::new(AtomicU64::new(0));
        let subscriber = Arc::new(Subscriber {
            id: id.clone(),
            filter,
            kinds: kinds.into_iter().collect(),
            sender,
            receiver: receiver.clone(),
            lossy: Arc::clone(&lossy),
            dropped_since_read: Arc::clone(&dropped_since_read),
        });
        let mut next = current.as_ref().clone();
        next.push(subscriber);
        self.inner.subscribers.store(Arc::new(next));

        Ok(SubscriberHandle {
            registration: Arc::new(SubscriberRegistration {
                id,
                inner: Arc::clone(&self.inner),
            }),
            receiver,
            lossy,
            dropped_since_read,
            snapshot_first,
        })
    }

    /// Publishes one event to every matching subscriber without blocking.
    ///
    /// ADR-0007 keeps batching out of the bus so publishers do not wait to
    /// accumulate events. Downstream subscribers may batch after delivery.
    #[must_use]
    #[tracing::instrument(skip_all, fields(event_kind = %event.kind, event_seq = event.seq))]
    pub fn publish(&self, event: Event) -> PublishReport {
        metrics::counter!(
            EVENTS_PUBLISHED_METRIC,
            "source" => event_source_label(event.source),
            "kind" => event.kind.clone()
        )
        .increment(1);
        let subscribers = self.inner.subscribers.load();
        let mut report = PublishReport::default();
        for subscriber in subscribers.iter() {
            if !subscriber.matches(&event) {
                continue;
            }
            report.matched = report.matched.saturating_add(1);
            let dropped = enqueue_drop_oldest(subscriber, event.clone());
            report.dropped = report.dropped.saturating_add(dropped);
            report.queued = report.queued.saturating_add(1);
            if dropped > 0 {
                subscriber
                    .dropped_since_read
                    .fetch_add(dropped, Ordering::AcqRel);
                subscriber
                    .lossy
                    .store(true, std::sync::atomic::Ordering::Release);
                metrics::counter!(
                    EVENTS_DROPPED_METRIC,
                    "subscription_id" => subscriber.id.clone()
                )
                .increment(dropped);
            }
        }
        drop(event);
        report
    }

    /// Removes a subscriber. Returns `false` if the id was already absent.
    #[tracing::instrument(skip_all, fields(subscription_id = id))]
    pub fn unsubscribe(&self, id: &str) -> bool {
        unsubscribe_inner(&self.inner, id)
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.load().len()
    }

    fn lock_updates(&self) -> MutexGuard<'_, ()> {
        match self.inner.updates.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

const fn event_source_label(source: synapse_core::EventSource) -> &'static str {
    match source {
        synapse_core::EventSource::A11yUia => "a11y_uia",
        synapse_core::EventSource::A11yWinEvent => "a11y_win_event",
        synapse_core::EventSource::A11yCdp => "a11y_cdp",
        synapse_core::EventSource::Perception => "perception",
        synapse_core::EventSource::PerceptionDetection => "perception_detection",
        synapse_core::EventSource::PerceptionHud => "perception_hud",
        synapse_core::EventSource::PerceptionAudio => "perception_audio",
        synapse_core::EventSource::Filesystem => "filesystem",
        synapse_core::EventSource::Process => "process",
        synapse_core::EventSource::Clipboard => "clipboard",
        synapse_core::EventSource::ActionEmitter => "action_emitter",
        synapse_core::EventSource::Reflex => "reflex",
        synapse_core::EventSource::System => "system",
    }
}

impl Subscriber {
    fn matches(&self, event: &Event) -> bool {
        (self.kinds.is_empty() || self.kinds.contains(&event.kind)) && self.filter.matches(event)
    }
}

impl SubscriberHandle {
    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn id(&self) -> &str {
        &self.registration.id
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn snapshot_first(&self) -> bool {
        self.snapshot_first
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn take_lossy(&self) -> bool {
        self.lossy.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn take_dropped_since_read(&self) -> u64 {
        self.dropped_since_read.swap(0, Ordering::AcqRel)
    }

    #[must_use]
    #[tracing::instrument(skip_all)]
    pub fn drain(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for SubscriberRegistration {
    fn drop(&mut self) {
        let _removed = unsubscribe_inner(&self.inner, &self.id);
    }
}

fn unsubscribe_inner(inner: &Arc<EventBusInner>, id: &str) -> bool {
    let _guard = match inner.updates.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let current = inner.subscribers.load_full();
    let next = current
        .iter()
        .filter(|subscriber| subscriber.id != id)
        .cloned()
        .collect::<Vec<_>>();
    let removed = next.len() != current.len();
    if removed {
        inner.subscribers.store(Arc::new(next));
    }
    removed
}

fn enqueue_drop_oldest(subscriber: &Subscriber, mut event: Event) -> u64 {
    let mut dropped = 0_u64;
    loop {
        match subscriber.sender.try_send(event) {
            Ok(()) => return dropped,
            Err(TrySendError::Full(returned)) => {
                event = returned;
                match subscriber.receiver.try_recv() {
                    Ok(_oldest) => dropped = dropped.saturating_add(1),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return dropped,
                }
            }
            Err(TrySendError::Disconnected(_returned)) => return dropped,
        }
    }
}
