use std::{
    collections::BTreeMap,
    error::Error,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use synapse_core::{Event, EventFilter, EventSource};
use synapse_reflex::{EVENTS_DROPPED_METRIC, SUBSCRIBER_QUEUE_CAPACITY};
use synapse_telemetry::metrics::{SSE_ACTIVE_SUBSCRIBERS, SSE_BUFFER_OVERFLOWS_TOTAL};

use super::{
    EventsQuery, LAST_EVENT_ID, SUBSCRIPTION_ID_HEADER, SseState, replay,
    ring::BufferedEvent,
    stream::{SseFrame, event_data},
};

const OVERFLOW_EVENTS: u64 = 5_000;
const EXPECTED_DROPPED: u64 = OVERFLOW_EVENTS - 4_096;

fn event(seq: u64, kind: &str) -> Event {
    Event {
        seq,
        at: Utc::now(),
        source: EventSource::System,
        kind: kind.to_owned(),
        data: serde_json::json!({"value": seq}),
        correlations: Vec::new(),
    }
}

#[test]
fn event_frame_is_stable_for_known_input() {
    let event = event(7, "tick");
    let data = event_data("sub-1", 1, &event, true).to_string();
    assert!(data.contains("\"subscription_id\":\"sub-1\""));
    assert!(data.contains("\"stream_seq\":1"));
    assert!(data.contains("\"seq\":7"));
    assert!(data.contains("\"lossy\":true"));
    assert_eq!(
        SseFrame::event(
            "sub-1",
            BufferedEvent {
                stream_seq: 1,
                event,
            },
            true,
        )
        .seq(),
        Some(1)
    );
}

#[test]
fn state_creates_subscription_with_empty_initial_body() {
    let state = SseState::from_env();
    let response = state.open(&axum::http::HeaderMap::new(), EventsQuery::default());
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[test]
fn last_event_id_zero_reuses_empty_existing_subscription() {
    let state = SseState::from_env();
    let subscription = state
        .create_subscription_with(
            EventFilter::Kind {
                kind: "reality_delta".to_owned(),
            },
            Vec::new(),
            false,
            None,
        )
        .expect("subscription should register");
    assert_eq!(state.active_subscription_count(), 1);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(LAST_EVENT_ID, axum::http::HeaderValue::from_static("0"));

    let response = state.open(
        &headers,
        EventsQuery {
            subscription_id: Some(subscription.id().to_owned()),
        },
    );

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(state.active_subscription_count(), 1);
    assert_eq!(
        response
            .headers()
            .get(SUBSCRIPTION_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(subscription.id())
    );
}

#[test]
fn sparse_domain_seq_gets_contiguous_stream_seq_without_loss() {
    let state = SseState::from_env();
    let subscription = state
        .create_subscription_with(
            EventFilter::Kind {
                kind: "reflex_fired".to_owned(),
            },
            Vec::new(),
            false,
            None,
        )
        .expect("subscription should register");

    state.publish_events(vec![event(62_488, "reflex_fired")]);
    SseState::sync_subscription(&subscription);
    let stats = subscription.stats();
    assert_eq!(stats.ring_len, 1);
    assert_eq!(stats.oldest_seq, Some(1));
    assert_eq!(stats.latest_seq, Some(1));
    assert_eq!(stats.oldest_event_seq, Some(62_488));
    assert_eq!(stats.latest_event_seq, Some(62_488));
    assert_eq!(stats.dropped_total, 0);
    assert!(!stats.lossy_pending);

    let frames = replay::frames_after(&subscription, None);
    assert_eq!(frames.len(), 1);
    match frames.front().expect("one event frame") {
        SseFrame::Event {
            stream_seq,
            event,
            lossy,
            ..
        } => {
            assert_eq!(*stream_seq, 1);
            assert_eq!(event.seq, 62_488);
            assert!(!lossy);
        }
        other => panic!("expected event frame, got {other:?}"),
    }
}

#[test]
fn last_event_id_uses_stream_seq_not_domain_event_seq() {
    let state = SseState::from_env();
    let subscription = state
        .create_subscription_with(EventFilter::All, Vec::new(), false, None)
        .expect("subscription should register");

    state.publish_events(vec![event(10, "first"), event(1_000, "second")]);
    let frames = replay::frames_after(&subscription, Some(1));
    assert_eq!(frames.len(), 1);
    match frames.front().expect("second event frame") {
        SseFrame::Event {
            stream_seq,
            event,
            lossy,
            ..
        } => {
            assert_eq!(*stream_seq, 2);
            assert_eq!(event.seq, 1_000);
            assert!(!lossy);
        }
        other => panic!("expected event frame, got {other:?}"),
    }
}

#[test]
fn ring_overflow_reports_drop_metric_and_lossy_frame() -> Result<(), Box<dyn Error>> {
    let recorder = TestRecorder::default();
    metrics::with_local_recorder(&recorder, || -> Result<(), Box<dyn Error>> {
        let state = SseState::from_env();
        let subscription = state
            .create_subscription_with(EventFilter::All, Vec::new(), false, None)
            .expect("subscription should register");
        let events = (0..OVERFLOW_EVENTS)
            .map(|seq| event(seq, "firehose"))
            .collect::<Vec<_>>();

        subscription.push_events(events);

        let stats = subscription.stats();
        assert_eq!(stats.ring_len, SUBSCRIBER_QUEUE_CAPACITY);
        assert_eq!(stats.oldest_seq, Some(EXPECTED_DROPPED + 1));
        assert_eq!(stats.latest_seq, Some(OVERFLOW_EVENTS));
        assert_eq!(stats.oldest_event_seq, Some(EXPECTED_DROPPED));
        assert_eq!(stats.latest_event_seq, Some(OVERFLOW_EVENTS - 1));
        assert_eq!(stats.dropped_total, EXPECTED_DROPPED);
        assert_eq!(stats.events_dropped_for_subscriber, EXPECTED_DROPPED);
        assert!(stats.lossy_pending);
        assert_eq!(
            recorder.counter_value(&metric_key_for(subscription.id()))?,
            EXPECTED_DROPPED
        );
        assert_eq!(
            recorder.counter_value(unlabeled_metric_key(SSE_BUFFER_OVERFLOWS_TOTAL))?,
            EXPECTED_DROPPED
        );

        let frames = replay::frames_after(&subscription, None);
        assert_eq!(frames.len(), SUBSCRIBER_QUEUE_CAPACITY + 1);
        assert!(matches!(
            frames.front(),
            Some(SseFrame::SubscriptionStarted { lossy: true, .. })
        ));
        match frames.get(1).expect("first replayed event") {
            SseFrame::Event {
                stream_seq,
                event,
                lossy,
                ..
            } => {
                assert_eq!(*stream_seq, EXPECTED_DROPPED + 1);
                assert_eq!(event.seq, EXPECTED_DROPPED);
                assert!(*lossy);
            }
            other => panic!("expected first replayed event, got {other:?}"),
        }
        Ok(())
    })
}

#[test]
fn active_subscriber_gauge_tracks_subscribe_and_cancel() -> Result<(), Box<dyn Error>> {
    let recorder = TestRecorder::default();
    metrics::with_local_recorder(&recorder, || -> Result<(), Box<dyn Error>> {
        let state = SseState::from_env();

        assert_eq!(state.active_subscription_count(), 0);
        assert_eq!(
            recorder.gauge_value(unlabeled_metric_key(SSE_ACTIVE_SUBSCRIBERS))?,
            0.0
        );

        let first = state
            .create_subscription_with(EventFilter::All, Vec::new(), false, None)
            .expect("first subscription should register")
            .id()
            .to_owned();
        assert_eq!(
            recorder.gauge_value(unlabeled_metric_key(SSE_ACTIVE_SUBSCRIBERS))?,
            1.0
        );

        let second = state
            .create_subscription_with(EventFilter::All, Vec::new(), false, None)
            .expect("second subscription should register")
            .id()
            .to_owned();
        assert_eq!(
            recorder.gauge_value(unlabeled_metric_key(SSE_ACTIVE_SUBSCRIBERS))?,
            2.0
        );

        state.cancel(&first).expect("first cancel should work");
        assert_eq!(
            recorder.gauge_value(unlabeled_metric_key(SSE_ACTIVE_SUBSCRIBERS))?,
            1.0
        );

        state.cancel(&second).expect("second cancel should work");
        assert_eq!(
            recorder.gauge_value(unlabeled_metric_key(SSE_ACTIVE_SUBSCRIBERS))?,
            0.0
        );

        Ok(())
    })
}

fn metric_key_for(subscription_id: &str) -> String {
    format!("{EVENTS_DROPPED_METRIC}{{subscription_id={subscription_id}}}")
}

fn unlabeled_metric_key(name: &str) -> &str {
    match name {
        SSE_ACTIVE_SUBSCRIBERS => "sse_active_subscribers{}",
        SSE_BUFFER_OVERFLOWS_TOTAL => "sse_buffer_overflows_total{}",
        _ => panic!("unexpected unlabeled metric {name}"),
    }
}

#[derive(Clone, Default)]
struct TestRecorder {
    counters: Arc<Mutex<BTreeMap<String, u64>>>,
    gauges: Arc<Mutex<BTreeMap<String, f64>>>,
}

impl TestRecorder {
    fn counter_value(&self, key: &str) -> Result<u64, Box<dyn Error>> {
        let counters = self
            .counters
            .lock()
            .map_err(|error| format!("metric recorder lock poisoned: {error}"))?;
        Ok(counters.get(key).copied().unwrap_or_default())
    }

    fn gauge_value(&self, key: &str) -> Result<f64, Box<dyn Error>> {
        let gauges = self
            .gauges
            .lock()
            .map_err(|error| format!("metric recorder lock poisoned: {error}"))?;
        Ok(gauges.get(key).copied().unwrap_or_default())
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

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(Arc::new(TestGauge {
            key: metric_key(key),
            gauges: Arc::clone(&self.gauges),
        }))
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

struct TestGauge {
    key: String,
    gauges: Arc<Mutex<BTreeMap<String, f64>>>,
}

impl GaugeFn for TestGauge {
    fn increment(&self, value: f64) {
        if let Ok(mut gauges) = self.gauges.lock() {
            let gauge = gauges.entry(self.key.clone()).or_default();
            *gauge += value;
        }
    }

    fn decrement(&self, value: f64) {
        if let Ok(mut gauges) = self.gauges.lock() {
            let gauge = gauges.entry(self.key.clone()).or_default();
            *gauge -= value;
        }
    }

    fn set(&self, value: f64) {
        if let Ok(mut gauges) = self.gauges.lock() {
            gauges.insert(self.key.clone(), value);
        }
    }
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
