use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use synapse_action::{ActionHandle, ActionMessage};
use synapse_core::Action;
use synapse_reflex::{EventBus, ReflexScheduler, ScheduledReflex, SchedulerConfig, p99_jitter_us};

const WARMUP_TICKS: usize = 32;
const SAMPLE_TICKS: usize = 512;
const TOTAL_TICKS_U64: u64 = 544;
const P99_LIMIT_US: u64 = 500;

fn bench_reflex_tick_jitter_under_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("reflex_tick_jitter_under_load");
    group.sample_size(10);
    group.bench_function("p99_under_load", |bench| {
        bench.iter_custom(|iterations| {
            let start = Instant::now();
            for _ in 0..iterations {
                let p99 = run_under_load_sample();
                assert!(
                    p99 <= P99_LIMIT_US,
                    "loaded scheduler jitter p99 {p99}us exceeded {P99_LIMIT_US}us"
                );
            }
            start.elapsed()
        });
    });
    group.finish();
}

fn run_under_load_sample() -> u64 {
    let load_stop = Arc::new(AtomicBool::new(false));
    let load_thread = spawn_load_thread(Arc::clone(&load_stop));
    let bus = EventBus::default();
    let (action_handle, action_rx) = ActionHandle::channel();
    let drain_thread = spawn_action_drain_thread(action_rx);
    let reflexes = (0..32)
        .map(|index| {
            ScheduledReflex::every_tick(format!("reflex-load-{index}"), vec![Action::ReleaseAll])
        })
        .collect::<Vec<_>>();
    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        reflexes,
        SchedulerConfig::default().with_max_ticks(TOTAL_TICKS_U64),
    )
    .unwrap_or_else(|error| panic!("scheduler should spawn for loaded bench: {error}"));
    let samples = scheduler.wait_for_samples(WARMUP_TICKS + SAMPLE_TICKS, Duration::from_secs(5));
    scheduler
        .stop()
        .unwrap_or_else(|error| panic!("scheduler should stop for loaded bench: {error}"));
    drain_thread
        .join()
        .unwrap_or_else(|error| panic!("action drain thread should join: {error:?}"));
    load_stop.store(true, Ordering::Release);
    load_thread
        .join()
        .unwrap_or_else(|error| panic!("load thread should join: {error:?}"));
    let measured = samples
        .get(WARMUP_TICKS..)
        .unwrap_or_else(|| panic!("loaded bench should collect warmup and measured samples"));
    let p99 = p99_jitter_us(measured);
    println!(
        "benchmark=reflex_tick_jitter_under_load samples:{} p99_jitter_us:{p99}",
        measured.len()
    );
    p99
}

fn spawn_load_thread(stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("synapse-reflex-bench-load".to_owned())
        .spawn(move || {
            let mut value = 0_u64;
            while !stop.load(Ordering::Acquire) {
                for next in 0..10_000_u64 {
                    value = value.wrapping_add(next.rotate_left(7));
                }
                std::hint::black_box(value);
            }
        })
        .unwrap_or_else(|error| panic!("load thread should spawn: {error}"))
}

fn spawn_action_drain_thread(
    mut action_rx: tokio::sync::mpsc::Receiver<ActionMessage>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("synapse-reflex-bench-action-drain".to_owned())
        .spawn(move || {
            while let Some((_action, ack, _operator_panic_epoch_at_enqueue)) =
                action_rx.blocking_recv()
            {
                let _ = ack.send(Ok(()));
            }
        })
        .unwrap_or_else(|error| panic!("action drain thread should spawn: {error}"))
}

criterion_group!(benches, bench_reflex_tick_jitter_under_load);
criterion_main!(benches);
