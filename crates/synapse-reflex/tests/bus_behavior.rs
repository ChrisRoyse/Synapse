use std::{
    collections::BTreeMap,
    error::Error,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use serde_json::json;
use synapse_core::{Event, EventFilter, EventSource, error_codes};
use synapse_reflex::{
    DEFAULT_MAX_SUBSCRIPTIONS, EVENTS_DROPPED_METRIC, EventBus, EventBusError,
    SUBSCRIBER_QUEUE_CAPACITY,
};

const OVERFLOW_EVENTS: u64 = 5_000;
const EXPECTED_DROPPED: u64 = OVERFLOW_EVENTS - 4_096;

#[test]
fn drop_oldest_5000_events_metric_and_lossy() -> Result<(), Box<dyn Error>> {
    let recorder = TestRecorder::default();
    metrics::with_local_recorder(&recorder, || -> Result<(), Box<dyn Error>> {
        assert_eq!(SUBSCRIBER_QUEUE_CAPACITY, 4_096);
        let bus = EventBus::default();
        let handle = bus.subscribe(EventFilter::All, Vec::new(), false)?;
        assert_eq!(handle.len(), 0);
        assert!(!handle.take_lossy());

        for seq in 0..OVERFLOW_EVENTS {
            let report = bus.publish(event(seq, "tick"));
            assert_eq!(report.matched, 1);
            assert_eq!(report.queued, 1);
        }

        let metric = recorder.counter_value(&metric_key_for(handle.id()))?;
        let queue_len = handle.len();
        let lossy = handle.take_lossy();
        let drained = handle.drain();
        let first_seq = drained.first().map(|event| event.seq);
        let last_seq = drained.last().map(|event| event.seq);

        assert_eq!(queue_len, SUBSCRIBER_QUEUE_CAPACITY);
        assert_eq!(drained.len(), SUBSCRIBER_QUEUE_CAPACITY);
        assert_eq!(metric, EXPECTED_DROPPED);
        assert!(lossy);
        assert_eq!(first_seq, Some(EXPECTED_DROPPED));
        assert_eq!(last_seq, Some(OVERFLOW_EVENTS - 1));
        Ok(())
    })
}

#[test]
fn subscription_cap_filter_and_unsubscribe_edges() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let mut handles = Vec::new();
    for _ in 0..DEFAULT_MAX_SUBSCRIPTIONS {
        handles.push(bus.subscribe(EventFilter::All, Vec::new(), false)?);
    }
    assert_eq!(bus.subscriber_count(), DEFAULT_MAX_SUBSCRIPTIONS);
    let cap_error = match bus.subscribe(EventFilter::All, Vec::new(), false) {
        Ok(_handle) => panic!("65th subscription should fail"),
        Err(error) => error,
    };
    assert_eq!(cap_error.code(), error_codes::SUBSCRIPTION_CAP_REACHED);
    assert_eq!(bus.subscriber_count(), DEFAULT_MAX_SUBSCRIPTIONS);

    let first_id = handles[0].id().to_owned();
    assert!(bus.unsubscribe(&first_id));
    assert!(!bus.unsubscribe(&first_id));
    assert_eq!(bus.subscriber_count(), DEFAULT_MAX_SUBSCRIPTIONS - 1);
    let retry = bus.subscribe(EventFilter::All, Vec::new(), false)?;
    assert!(!retry.id().is_empty());
    assert_eq!(bus.subscriber_count(), DEFAULT_MAX_SUBSCRIPTIONS);
    let _report = bus.publish(event(10_000, "tick"));
    assert!(handles[0].is_empty());

    let filter_bus = EventBus::default();
    let filter_only_handle = filter_bus.subscribe(
        EventFilter::Kind {
            kind: "wanted".to_owned(),
        },
        Vec::new(),
        true,
    )?;
    let kind_list_handle =
        filter_bus.subscribe(EventFilter::All, vec!["allowed".to_owned()], false)?;
    assert_eq!(filter_only_handle.len(), 0);
    assert_eq!(kind_list_handle.len(), 0);
    assert!(filter_only_handle.snapshot_first());
    let _ignored_report = filter_bus.publish(event(1, "ignored"));
    let _wanted_report = filter_bus.publish(event(2, "wanted"));
    let _allowed_report = filter_bus.publish(event(3, "allowed"));
    let filter_only_events = filter_only_handle.drain();
    let kind_list_events = kind_list_handle.drain();
    assert_eq!(
        filter_only_events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        kind_list_events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![3]
    );
    Ok(())
}

#[test]
fn custom_subscription_cap_is_enforced_and_reusable() -> Result<(), Box<dyn Error>> {
    let max = NonZeroUsize::new(2).ok_or("fixture max must be non-zero")?;
    let bus = EventBus::with_max_subscriptions(max);
    assert_eq!(bus.max_subscriptions(), 2);

    let first = bus.subscribe(EventFilter::All, Vec::new(), false)?;
    let second = bus.subscribe(EventFilter::All, Vec::new(), false)?;
    assert_eq!(bus.subscriber_count(), 2);

    let capped = match bus.subscribe(EventFilter::All, Vec::new(), false) {
        Ok(_handle) => panic!("third subscription should fail at custom cap 2"),
        Err(error) => error,
    };
    assert_eq!(capped.code(), error_codes::SUBSCRIPTION_CAP_REACHED);
    assert!(matches!(
        capped,
        EventBusError::SubscriptionCapReached { limit: 2 }
    ));
    assert_eq!(bus.subscriber_count(), 2);

    assert!(bus.unsubscribe(first.id()));
    assert_eq!(bus.subscriber_count(), 1);
    let retry = bus.subscribe(EventFilter::All, Vec::new(), false)?;
    assert!(!retry.id().is_empty());
    assert_eq!(bus.subscriber_count(), 2);
    assert!(bus.unsubscribe(second.id()));
    Ok(())
}

#[test]
fn dropped_subscription_handle_unregisters_from_bus() -> Result<(), Box<dyn Error>> {
    let bus = EventBus::default();
    let first = bus.subscribe(EventFilter::All, Vec::new(), false)?;
    let first_clone = first.clone();
    assert_eq!(bus.subscriber_count(), 1);

    drop(first);
    assert_eq!(
        bus.subscriber_count(),
        1,
        "cloned handles should keep the bus registration alive"
    );

    drop(first_clone);
    assert_eq!(bus.subscriber_count(), 0);

    let replacement = bus.subscribe(EventFilter::All, Vec::new(), false)?;
    assert!(!replacement.id().is_empty());
    assert_eq!(bus.subscriber_count(), 1);
    Ok(())
}

#[test]
fn invalid_filter_returns_reflex_filter_invalid() {
    let bus = EventBus::default();
    let invalid = EventFilter::And { args: Vec::new() };
    let result = bus.subscribe(invalid, Vec::new(), false);
    let error = match result {
        Ok(_handle) => panic!("empty AND must fail validation"),
        Err(error) => error,
    };
    assert!(matches!(error, EventBusError::FilterInvalid { .. }));
    assert_eq!(error.code(), error_codes::REFLEX_FILTER_INVALID);
}

fn event(seq: u64, kind: &str) -> Event {
    Event {
        seq,
        at: Utc::now(),
        source: EventSource::System,
        kind: kind.to_owned(),
        data: json!({ "seq": seq, "kind": kind }),
        correlations: Vec::new(),
    }
}

fn metric_key_for(subscription_id: &str) -> String {
    format!("{EVENTS_DROPPED_METRIC}{{subscription_id={subscription_id}}}")
}

#[derive(Clone, Default)]
struct TestRecorder {
    counters: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl TestRecorder {
    fn counter_value(&self, key: &str) -> Result<u64, Box<dyn Error>> {
        let counters = self
            .counters
            .lock()
            .map_err(|error| format!("metric recorder lock poisoned: {error}"))?;
        Ok(counters.get(key).copied().unwrap_or_default())
    }
}

impl Recorder for TestRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(TestCounter {
            key: metric_key(key),
            counters: Arc::clone(&self.counters),
        }))
    }

    fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(Arc::new(NoopGauge))
    }

    fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(Arc::new(NoopHistogram))
    }
}

struct TestCounter {
    key: String,
    counters: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl CounterFn for TestCounter {
    fn increment(&self, value: u64) {
        if let Ok(mut counters) = self.counters.lock() {
            let counter = counters.entry(self.key.clone()).or_default();
            *counter = counter.saturating_add(value);
        }
    }

    fn absolute(&self, value: u64) {
        if let Ok(mut counters) = self.counters.lock() {
            counters.insert(self.key.clone(), value);
        }
    }
}

struct NoopGauge;

impl GaugeFn for NoopGauge {
    fn increment(&self, _value: f64) {}

    fn decrement(&self, _value: f64) {}

    fn set(&self, _value: f64) {}
}

struct NoopHistogram;

impl HistogramFn for NoopHistogram {
    fn record(&self, _value: f64) {}
}

fn metric_key(key: &Key) -> String {
    let mut labels = key
        .labels()
        .map(|label| format!("{}={}", label.key(), label.value()))
        .collect::<Vec<_>>();
    labels.sort();
    format!("{}{{{}}}", key.name(), labels.join(","))
}
