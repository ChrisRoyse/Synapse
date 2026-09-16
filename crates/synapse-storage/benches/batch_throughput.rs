use std::{
    error::Error,
    hint::black_box,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use criterion::{Criterion, Throughput};
use synapse_storage::{Db, cf};

const TEST_SCHEMA_VERSION: u32 = 7;
const SINGLE_PUT_ROWS: usize = 10_000;
const SINGLE_PUT_ELEMENTS: u64 = 10_000;
const SINGLE_PUT_UNITS: f64 = 10_000.0;
const BATCH_ROWS: usize = 1_000;
const BATCH_ELEMENTS: u64 = 1_000;
const BATCH_UNITS: f64 = 1_000.0;
const SCAN_ROWS: usize = 100_000;
const SCAN_ELEMENTS: u64 = 100_000;
const SCAN_UNITS: f64 = 100_000.0;
const SINGLE_PUT_TARGET: Duration = Duration::from_millis(200);
const BATCH_TARGET: Duration = Duration::from_millis(20);
const BATCH_TARGET_US_PER_ITEM: u32 = 20;
const SCAN_TARGET: Duration = Duration::from_millis(100);

fn main() -> Result<(), Box<dyn Error>> {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(2))
            .sample_size(20)
            .configure_from_args();

        bench_single_put_10k(&mut criterion);
        bench_put_batch_1k(&mut criterion);
        bench_scan_100k(&mut criterion)?;
        criterion.final_summary();
    }

    let reports = [
        measure_single_put_10k()?,
        measure_put_batch_1k()?,
        measure_scan_100k()?,
    ];
    for report in reports {
        report.print();
        assert!(report.passed(), "{} exceeded target", report.name);
    }
    Ok(())
}

fn bench_single_put_10k(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("synapse_storage");
    group.throughput(Throughput::Elements(SINGLE_PUT_ELEMENTS));
    group.bench_function("storage_events_single_put_10k", |bench| {
        bench.iter_custom(|iterations| {
            repeat_timed(iterations, || {
                timed_single_put_10k(false)
                    .unwrap_or_else(|error| panic!("single PUT bench failed: {error}"))
            })
        });
    });
    group.finish();
}

fn bench_put_batch_1k(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("synapse_storage");
    group.throughput(Throughput::Elements(BATCH_ELEMENTS));
    group.bench_function("storage_put_batch_1k", |bench| {
        bench.iter_custom(|iterations| {
            repeat_timed(iterations, || {
                timed_put_batch_1k(false)
                    .unwrap_or_else(|error| panic!("put_batch bench failed: {error}"))
            })
        });
    });
    group.finish();
}

fn bench_scan_100k(criterion: &mut Criterion) -> Result<(), Box<dyn Error>> {
    let root = unique_root("scan-criterion")?;
    let db = Db::open(&root.join("db"), TEST_SCHEMA_VERSION)?;
    db.put_batch(cf::CF_EVENTS, event_rows(SCAN_ROWS))?;
    db.flush()?;

    {
        let mut group = criterion.benchmark_group("synapse_storage");
        group.throughput(Throughput::Elements(SCAN_ELEMENTS));
        group.bench_function("storage_scan_100k_rows", |bench| {
            bench.iter_custom(|iterations| {
                let mut total = Duration::ZERO;
                for _ in 0..iterations {
                    let started = Instant::now();
                    let rows = db
                        .scan_cf(cf::CF_EVENTS)
                        .unwrap_or_else(|error| panic!("scan bench failed: {error}"));
                    total = total.saturating_add(started.elapsed());
                    assert_eq!(black_box(rows.len()), SCAN_ROWS);
                }
                total
            });
        });
        group.finish();
    }
    drop(db);
    std::fs::remove_dir_all(root)?;
    Ok(())
}

fn measure_single_put_10k() -> Result<BenchReport, Box<dyn Error>> {
    let elapsed = timed_single_put_10k(true)?;
    Ok(BenchReport {
        name: "storage_events_single_put_10k",
        elapsed,
        target: SINGLE_PUT_TARGET,
        unit_count: SINGLE_PUT_UNITS,
        final_rows: SINGLE_PUT_ROWS,
        unit_target_us: None,
    })
}

fn measure_put_batch_1k() -> Result<BenchReport, Box<dyn Error>> {
    let elapsed = timed_put_batch_1k(true)?;
    Ok(BenchReport {
        name: "storage_put_batch_1k",
        elapsed,
        target: BATCH_TARGET,
        unit_count: BATCH_UNITS,
        final_rows: BATCH_ROWS,
        unit_target_us: Some(BATCH_TARGET_US_PER_ITEM),
    })
}

fn measure_scan_100k() -> Result<BenchReport, Box<dyn Error>> {
    let root = unique_root("scan-manual")?;
    let db = Db::open(&root.join("db"), TEST_SCHEMA_VERSION)?;
    db.put_batch(cf::CF_EVENTS, event_rows(SCAN_ROWS))?;
    db.flush()?;
    let before = db.scan_cf(cf::CF_EVENTS)?.len();
    let started = Instant::now();
    let rows = db.scan_cf(cf::CF_EVENTS)?;
    let elapsed = started.elapsed();
    println!(
        "regression_state=bench_cf_scan bench=storage_scan_100k_rows before={before} after={} observed=rows:{}",
        rows.len(),
        rows.len()
    );
    assert_eq!(rows.len(), SCAN_ROWS);
    drop(db);
    std::fs::remove_dir_all(root)?;
    Ok(BenchReport {
        name: "storage_scan_100k_rows",
        elapsed,
        target: SCAN_TARGET,
        unit_count: SCAN_UNITS,
        final_rows: SCAN_ROWS,
        unit_target_us: None,
    })
}

fn timed_single_put_10k(emit_readback: bool) -> Result<Duration, Box<dyn Error>> {
    let root = unique_root("single")?;
    let db = Db::open(&root.join("db"), TEST_SCHEMA_VERSION)?;
    let before = db.scan_cf(cf::CF_EVENTS)?.len();
    let started = Instant::now();
    for row in event_rows(SINGLE_PUT_ROWS) {
        db.put_batch(cf::CF_EVENTS, [row])?;
    }
    db.flush()?;
    let elapsed = started.elapsed();
    let after = db.scan_cf(cf::CF_EVENTS)?;
    if emit_readback {
        println!(
            "regression_state=bench_cf_scan bench=storage_events_single_put_10k before={before} after={} observed=rows:{}",
            after.len(),
            after.len()
        );
    }
    assert_eq!(after.len(), SINGLE_PUT_ROWS);
    drop(db);
    std::fs::remove_dir_all(root)?;
    Ok(elapsed)
}

fn timed_put_batch_1k(emit_readback: bool) -> Result<Duration, Box<dyn Error>> {
    let root = unique_root("batch")?;
    let db = Db::open(&root.join("db"), TEST_SCHEMA_VERSION)?;
    let kvs = event_rows(BATCH_ROWS);
    let before = db.scan_cf(cf::CF_EVENTS)?.len();
    let started = Instant::now();
    db.put_batch(cf::CF_EVENTS, black_box(kvs))?;
    db.flush()?;
    let elapsed = started.elapsed();
    let after = db.scan_cf(cf::CF_EVENTS)?;
    if emit_readback {
        println!(
            "regression_state=bench_cf_scan bench=storage_put_batch_1k before={before} after={} observed=rows:{}",
            after.len(),
            after.len()
        );
    }
    assert_eq!(after.len(), BATCH_ROWS);
    drop(db);
    std::fs::remove_dir_all(root)?;
    Ok(elapsed)
}

fn repeat_timed<F>(iterations: u64, mut run_once: F) -> Duration
where
    F: FnMut() -> Duration,
{
    let mut total = Duration::ZERO;
    for _ in 0..iterations {
        total = total.saturating_add(run_once());
    }
    total
}

fn event_rows(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|index| {
            (
                format!("{index:016x}").into_bytes(),
                format!(r#"{{"ts_ns":{index},"event":"bench"}}"#).into_bytes(),
            )
        })
        .collect()
}

fn unique_root(name: &str) -> Result<std::path::PathBuf, Box<dyn Error>> {
    Ok(std::env::temp_dir().join(format!(
        "synapse-storage-{name}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )))
}

struct BenchReport {
    name: &'static str,
    elapsed: Duration,
    target: Duration,
    unit_count: f64,
    final_rows: usize,
    unit_target_us: Option<u32>,
}

impl BenchReport {
    fn print(&self) {
        let elapsed_ms = self.elapsed.as_secs_f64() * 1_000.0;
        let target_ms = self.target.as_secs_f64() * 1_000.0;
        let per_item_us = (self.elapsed.as_secs_f64() * 1_000_000.0) / self.unit_count;
        println!(
            "regression_state=bench_report bench={} elapsed_ms={elapsed_ms:.3} target_ms={target_ms:.3} per_item_us={per_item_us:.3} unit_target_us={:?} after=pass:{} observed=rows:{}",
            self.name,
            self.unit_target_us,
            self.passed(),
            self.final_rows
        );
    }

    fn passed(&self) -> bool {
        self.elapsed <= self.target
    }
}
