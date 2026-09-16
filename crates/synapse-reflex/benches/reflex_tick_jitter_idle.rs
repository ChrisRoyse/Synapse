use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use synapse_action::ActionHandle;
use synapse_reflex::{EventBus, ReflexScheduler, SchedulerConfig, p99_jitter_us};

const WARMUP_TICKS: usize = 32;
const SAMPLE_TICKS: usize = 512;
const TOTAL_TICKS_U64: u64 = 544;
const P99_LIMIT_US: u64 = 200;

fn bench_reflex_tick_jitter_idle(c: &mut Criterion) {
    let mut group = c.benchmark_group("reflex_tick_jitter_idle");
    group.sample_size(10);
    group.bench_function("p99_idle", |bench| {
        bench.iter_custom(|iterations| {
            let start = Instant::now();
            for _ in 0..iterations {
                let p99 = run_idle_sample();
                assert!(
                    p99 <= P99_LIMIT_US,
                    "idle scheduler jitter p99 {p99}us exceeded {P99_LIMIT_US}us"
                );
            }
            start.elapsed()
        });
    });
    group.finish();
}

fn run_idle_sample() -> u64 {
    let bus = EventBus::default();
    let (action_handle, _action_rx) = ActionHandle::channel();
    let mut scheduler = ReflexScheduler::spawn(
        bus,
        action_handle,
        Vec::new(),
        SchedulerConfig::default().with_max_ticks(TOTAL_TICKS_U64),
    )
    .unwrap_or_else(|error| panic!("scheduler should spawn for idle bench: {error}"));
    let samples = scheduler.wait_for_samples(WARMUP_TICKS + SAMPLE_TICKS, Duration::from_secs(5));
    scheduler
        .stop()
        .unwrap_or_else(|error| panic!("scheduler should stop for idle bench: {error}"));
    let measured = samples
        .get(WARMUP_TICKS..)
        .unwrap_or_else(|| panic!("idle bench should collect warmup and measured samples"));
    let p99 = p99_jitter_us(measured);
    println!(
        "benchmark=reflex_tick_jitter_idle samples:{} p99_jitter_us:{p99}",
        measured.len()
    );
    p99
}

criterion_group!(benches, bench_reflex_tick_jitter_idle);
criterion_main!(benches);
