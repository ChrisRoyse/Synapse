use std::{
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use synapse_action::{ActionHandle, ActionMessage};
use synapse_core::{Action, Backend, ComboInput, ComboStep, Event, EventFilter, EventSource};
use synapse_reflex::{EventBus, ReflexScheduler, ScheduledReflex, SchedulerConfig};

const TRIGGER_KIND: &str = "bench_combo_step_interval";
const SAMPLE_COMBOS: usize = 64;
const EXPECTED_INTERVAL_MS: u32 = 8;
const ERROR_LIMIT_US: u64 = 500;
const MAX_TICKS: u64 = 2_000;

fn bench_reflex_combo_step_interval(c: &mut Criterion) {
    let max_observed_p99 = Arc::new(AtomicU64::new(0));
    {
        let observed = Arc::clone(&max_observed_p99);
        let mut group = c.benchmark_group("reflex_combo_step_interval");
        group.sample_size(10);
        group.bench_function("p99_step_interval_error", |bench| {
            bench.iter_custom(|iterations| {
                let start = Instant::now();
                for _ in 0..iterations {
                    let p99 = run_combo_interval_sample();
                    observed.fetch_max(p99, Ordering::Relaxed);
                    assert!(
                        p99 <= ERROR_LIMIT_US,
                        "combo interval p99 error {p99}us exceeded {ERROR_LIMIT_US}us"
                    );
                }
                start.elapsed()
            });
        });
        group.finish();
    }
    println!(
        "benchmark=reflex_combo_step_interval max_observed_p99_error_us:{} limit_us:{ERROR_LIMIT_US}",
        max_observed_p99.load(Ordering::Relaxed)
    );
}

fn run_combo_interval_sample() -> u64 {
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let (time_tx, time_rx) = std::sync::mpsc::channel();
    let drain_thread = spawn_action_time_drain(action_rx, time_tx);
    let reflex = ScheduledReflex::on_event(
        "combo-interval-bench",
        EventFilter::Kind {
            kind: TRIGGER_KIND.to_owned(),
        },
        vec![Action::Combo {
            steps: combo_steps(),
            backend: Backend::Software,
        }],
    );
    let mut scheduler = ReflexScheduler::spawn(
        bus.clone(),
        action_handle,
        vec![reflex],
        SchedulerConfig::default().with_max_ticks(MAX_TICKS),
    )
    .unwrap_or_else(|error| panic!("scheduler should spawn for combo bench: {error}"));
    let mut errors = Vec::with_capacity(SAMPLE_COMBOS);

    for seq in 0..SAMPLE_COMBOS {
        let report = bus.publish(Event {
            seq: u64::try_from(seq).unwrap_or(u64::MAX),
            at: Utc::now(),
            source: EventSource::System,
            kind: TRIGGER_KIND.to_owned(),
            data: json!({ "seq": seq, "known": "reflex_combo_step_interval" }),
            correlations: Vec::new(),
        });
        assert_eq!(
            report.matched, 1,
            "scheduler subscriber should match trigger"
        );
        let times = receive_action_times(&time_rx, 2, Duration::from_millis(100));
        let actual = times[1].duration_since(times[0]);
        let expected = Duration::from_millis(u64::from(EXPECTED_INTERVAL_MS));
        errors.push(duration_us(actual.abs_diff(expected)));
    }

    scheduler
        .stop()
        .unwrap_or_else(|error| panic!("scheduler should stop for combo bench: {error}"));
    drain_thread
        .join()
        .unwrap_or_else(|error| panic!("action drain thread should join: {error:?}"));

    p99_us(&mut errors)
}

fn combo_steps() -> Vec<ComboStep> {
    vec![
        ComboStep {
            at_ms: 0,
            input: ComboInput::MouseMoveRel { dx: 1.0, dy: 0.0 },
        },
        ComboStep {
            at_ms: EXPECTED_INTERVAL_MS,
            input: ComboInput::MouseMoveRel { dx: 2.0, dy: 0.0 },
        },
    ]
}

fn spawn_action_time_drain(
    mut action_rx: tokio::sync::mpsc::Receiver<ActionMessage>,
    time_tx: Sender<Instant>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("synapse-reflex-bench-combo-drain".to_owned())
        .spawn(move || {
            while let Some((_action, ack, _operator_panic_epoch_at_enqueue)) =
                action_rx.blocking_recv()
            {
                let _ = time_tx.send(Instant::now());
                let _ = ack.send(Ok(()));
            }
        })
        .unwrap_or_else(|error| panic!("action drain thread should spawn: {error}"))
}

fn receive_action_times(
    time_rx: &Receiver<Instant>,
    count: usize,
    timeout: Duration,
) -> Vec<Instant> {
    let deadline = Instant::now() + timeout;
    let mut times = Vec::with_capacity(count);
    while times.len() < count {
        let now = Instant::now();
        assert!(
            now < deadline,
            "received {} of {count} action timestamps",
            times.len()
        );
        match time_rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(time) => times.push(time),
            Err(RecvTimeoutError::Timeout) => {
                panic!("received {} of {count} action timestamps", times.len());
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "action timestamp channel disconnected after {}",
                    times.len()
                );
            }
        }
    }
    times
}

fn p99_us(values: &mut [u64]) -> u64 {
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

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

criterion_group!(benches, bench_reflex_combo_step_interval);
criterion_main!(benches);
