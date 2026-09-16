use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use synapse_core::{Event, EventFilter, EventSource};
use synapse_reflex::{EventBus, SubscriberHandle};

const EVENT_KIND: &str = "bench_event_to_subscriber";
const SAMPLE_EVENTS: u64 = 512;
const P99_LIMIT_NS: u64 = 50_000_000;

fn bench_event_to_subscriber(c: &mut Criterion) {
    let max_observed_p99 = Arc::new(AtomicU64::new(0));
    {
        let observed = Arc::clone(&max_observed_p99);
        let mut group = c.benchmark_group("event_to_subscriber");
        group.sample_size(10);
        group.bench_function("p99_publish_to_receive", |bench| {
            bench.iter_custom(|iterations| {
                let start = Instant::now();
                for _ in 0..iterations {
                    let p99 = run_event_to_subscriber_sample();
                    observed.fetch_max(p99, Ordering::Relaxed);
                    assert!(
                        p99 <= P99_LIMIT_NS,
                        "event_to_subscriber p99 {p99}ns exceeded {P99_LIMIT_NS}ns"
                    );
                }
                start.elapsed()
            });
        });
        group.finish();
    }
    println!(
        "benchmark=event_to_subscriber max_observed_p99_latency_ns:{} limit_ns:{P99_LIMIT_NS}",
        max_observed_p99.load(Ordering::Relaxed)
    );
}

fn run_event_to_subscriber_sample() -> u64 {
    let bus = EventBus::default();
    let subscriber = bus
        .subscribe(
            EventFilter::Kind {
                kind: EVENT_KIND.to_owned(),
            },
            vec![EVENT_KIND.to_owned()],
            false,
        )
        .unwrap_or_else(|error| panic!("event subscriber should register: {error}"));
    let mut latencies = Vec::with_capacity(
        usize::try_from(SAMPLE_EVENTS)
            .unwrap_or_else(|error| panic!("sample event count should fit usize: {error}")),
    );

    for seq in 0..SAMPLE_EVENTS {
        let event = Event {
            seq,
            at: Utc::now(),
            source: EventSource::System,
            kind: EVENT_KIND.to_owned(),
            data: json!({ "seq": seq, "known": "event_to_subscriber" }),
            correlations: Vec::new(),
        };
        let started = Instant::now();
        let report = bus.publish(event);
        assert_eq!(report.matched, 1, "one subscriber should match");
        assert_eq!(report.queued, 1, "one event should queue");
        assert_eq!(report.dropped, 0, "benchmark subscriber should not drop");
        let received = receive_one(&subscriber, Duration::from_millis(50));
        assert_eq!(received.seq, seq, "subscriber readback seq should match");
        assert_eq!(
            received.kind, EVENT_KIND,
            "subscriber readback kind should match"
        );
        latencies.push(duration_ns(started.elapsed()));
        std::hint::black_box(received);
    }

    p99(&mut latencies)
}

fn receive_one(subscriber: &SubscriberHandle, timeout: Duration) -> Event {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(event) = subscriber.drain().into_iter().next() {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "subscriber did not receive event within {timeout:?}"
        );
        std::thread::yield_now();
    }
}

fn p99(values: &mut [u64]) -> u64 {
    assert!(!values.is_empty(), "p99 needs at least one value");
    values.sort_unstable();
    let rank = values
        .len()
        .saturating_mul(99)
        .saturating_add(99)
        .saturating_div(100)
        .saturating_sub(1);
    values[rank.min(values.len().saturating_sub(1))]
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

criterion_group!(benches, bench_event_to_subscriber);
criterion_main!(benches);
