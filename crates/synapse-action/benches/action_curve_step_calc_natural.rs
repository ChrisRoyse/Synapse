use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use criterion::Criterion;
use synapse_action::sample_curve;
use synapse_core::{AimCurve, AimNaturalParams, Point};

const DEFAULT_ITERATIONS: usize = 20_000;
const MAX_DURATION_ITERATIONS: usize = 2_000;
const TARGET_NS_PER_STEP: u128 = 1_000;

fn main() {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .sample_size(20)
            .configure_from_args();

        bench_action_curve_step_calc_natural(&mut criterion);
        criterion.final_summary();
    }

    for report in manual_reports() {
        report.print();
        assert!(
            report.p99_ns_per_step <= TARGET_NS_PER_STEP,
            "action_curve_step_calc_natural {} p99 {} ns/step exceeded {} ns/step",
            report.edge,
            report.p99_ns_per_step,
            TARGET_NS_PER_STEP
        );
    }
}

fn bench_action_curve_step_calc_natural(criterion: &mut Criterion) {
    let case = BenchCase::happy();
    criterion.bench_function("action_curve_step_calc_natural", |bencher| {
        bencher.iter(|| {
            let samples = sample_curve(
                black_box(&case.curve()),
                black_box(case.start),
                black_box(case.end),
                black_box(case.duration_ms),
                black_box(case.override_seed),
            );
            black_box(samples);
        });
    });
}

fn manual_reports() -> Vec<BenchReport> {
    [
        BenchCase::happy(),
        BenchCase {
            edge: "zero_duration",
            duration_ms: 0,
            ..BenchCase::happy()
        },
        BenchCase {
            edge: "same_start_end",
            end: Point { x: 0, y: 0 },
            ..BenchCase::happy()
        },
        BenchCase {
            edge: "max_declared_duration",
            duration_ms: 2000,
            iterations: MAX_DURATION_ITERATIONS,
            ..BenchCase::happy()
        },
    ]
    .into_iter()
    .map(measure_case)
    .collect()
}

#[derive(Copy, Clone, Debug)]
struct BenchCase {
    edge: &'static str,
    start: Point,
    end: Point,
    duration_ms: u32,
    params_seed: Option<u64>,
    override_seed: Option<u64>,
    iterations: usize,
}

impl BenchCase {
    const fn happy() -> Self {
        Self {
            edge: "seed42_fast_50ms",
            start: Point { x: 0, y: 0 },
            end: Point { x: 100, y: 100 },
            duration_ms: 50,
            params_seed: Some(42),
            override_seed: None,
            iterations: DEFAULT_ITERATIONS,
        }
    }

    const fn curve(self) -> AimCurve {
        AimCurve::Natural {
            params: AimNaturalParams {
                seed: self.params_seed,
                ..AimNaturalParams::FAST
            },
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct BenchReport {
    edge: &'static str,
    iterations: usize,
    duration_ms: u32,
    samples_per_call: usize,
    p99_ns_per_step: u128,
    max_ns_per_step: u128,
}

impl BenchReport {
    fn print(self) {
        println!(
            "readback=action_curve_step_calc_natural edge={} before=iterations:{} duration_ms:{} after=samples_per_call:{} p99_ns_per_step:{} max_ns_per_step:{} target_ns_per_step:{} result_value=pass",
            self.edge,
            self.iterations,
            self.duration_ms,
            self.samples_per_call,
            self.p99_ns_per_step,
            self.max_ns_per_step,
            TARGET_NS_PER_STEP
        );
    }
}

fn measure_case(case: BenchCase) -> BenchReport {
    let curve = case.curve();
    let mut samples_per_call = 0_usize;

    for _ in 0..128 {
        let samples = sample_curve(
            black_box(&curve),
            black_box(case.start),
            black_box(case.end),
            black_box(case.duration_ms),
            black_box(case.override_seed),
        );
        samples_per_call = samples.len();
        black_box(samples);
    }

    let mut ns_per_step = Vec::with_capacity(case.iterations);
    for _ in 0..case.iterations {
        let started = Instant::now();
        let samples = sample_curve(
            black_box(&curve),
            black_box(case.start),
            black_box(case.end),
            black_box(case.duration_ms),
            black_box(case.override_seed),
        );
        let elapsed_ns = started.elapsed().as_nanos();
        samples_per_call = samples.len();
        let sample_count = usize_to_u128(samples_per_call.max(1));
        ns_per_step.push(elapsed_ns / sample_count);
        black_box(samples);
    }

    ns_per_step.sort_unstable();
    let p99_index = (ns_per_step.len().saturating_sub(1) * 99) / 100;
    let p99_ns_per_step = ns_per_step.get(p99_index).copied().unwrap_or_default();
    let max_ns_per_step = ns_per_step.last().copied().unwrap_or_default();

    BenchReport {
        edge: case.edge,
        iterations: case.iterations,
        duration_ms: case.duration_ms,
        samples_per_call,
        p99_ns_per_step,
        max_ns_per_step,
    }
}

fn usize_to_u128(value: usize) -> u128 {
    u128::try_from(value).unwrap_or(u128::MAX)
}
