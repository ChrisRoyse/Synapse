use std::{
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use criterion::Criterion;
use synapse_capture::{CaptureConfig, CaptureError, spawn_capture_loop};

const CPU_LIMIT_PERCENT: f64 = 2.0;
const DEFAULT_STEADY_SECONDS: u64 = 30;

#[derive(Clone, Debug)]
struct CpuSample {
    process_time: Duration,
    wall: Instant,
}

#[derive(Debug)]
struct SteadyStateReport {
    source: &'static str,
    duration: Duration,
    cpu_percent: f64,
    frames_captured: u64,
    frames_dropped: u64,
    frames_consumed: u64,
    channel_len: usize,
}

impl SteadyStateReport {
    fn print(&self) {
        println!(
            "capture_loop_steady_state source={} duration_secs={:.3} cpu_percent={:.4} frames_captured={} frames_dropped={} frames_consumed={} channel_len={}",
            self.source,
            self.duration.as_secs_f64(),
            self.cpu_percent,
            self.frames_captured,
            self.frames_dropped,
            self.frames_consumed,
            self.channel_len
        );
    }
}

fn main() {
    {
        let mut criterion = Criterion::default()
            .warm_up_time(Duration::from_millis(100))
            .measurement_time(Duration::from_secs(1))
            .sample_size(10)
            .configure_from_args();

        capture_loop_start_stop(&mut criterion);
        criterion.final_summary();
    }

    let report = match steady_state_capture_cpu(steady_state_duration()) {
        Ok(report) => report,
        Err(err) => panic!("steady-state capture bench failed: {err}"),
    };
    report.print();
    assert!(
        report.cpu_percent <= CPU_LIMIT_PERCENT,
        "capture CPU {:.4}% exceeded {:.4}% budget",
        report.cpu_percent,
        CPU_LIMIT_PERCENT
    );
}

fn capture_loop_start_stop(c: &mut Criterion) {
    c.bench_function("capture_loop_60fps_start_stop", |b| {
        b.iter(|| {
            let handle = spawn_capture_loop(CaptureConfig {
                min_update_interval_ms: 16,
                dirty_region_only: false,
                ..CaptureConfig::default()
            })
            .unwrap_or_else(|err| panic!("capture loop should start: {err}"));
            thread::sleep(Duration::from_millis(64));
            let stats = handle.stats();
            black_box(stats.frames_captured());
            handle
                .stop()
                .unwrap_or_else(|err| panic!("capture loop should stop: {err}"));
        });
    });
}

fn steady_state_duration() -> Duration {
    std::env::var("SYNAPSE_CAPTURE_BENCH_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(
            Duration::from_secs(DEFAULT_STEADY_SECONDS),
            Duration::from_secs,
        )
}

fn steady_state_capture_cpu(duration: Duration) -> Result<SteadyStateReport, CaptureError> {
    let handle = spawn_capture_loop(CaptureConfig {
        min_update_interval_ms: 16,
        dirty_region_only: false,
        ..CaptureConfig::default().with_env_backend()
    })?;
    let stats = handle.stats();
    let rx = handle.receiver();
    let consumer_stop = Arc::new(AtomicBool::new(false));
    let frames_consumed = Arc::new(AtomicU64::new(0));
    let consumer_join = {
        let consumer_stop = consumer_stop.clone();
        let frames_consumed = frames_consumed.clone();
        thread::spawn(move || {
            while !consumer_stop.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame) => {
                        black_box(frame.frame_seq);
                        frames_consumed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
    };

    let before = cpu_sample().map_err(|detail| CaptureError::ThreadFailed { detail })?;
    thread::sleep(duration);
    let after = cpu_sample().map_err(|detail| CaptureError::ThreadFailed { detail })?;
    consumer_stop.store(true, Ordering::Relaxed);
    let frames_captured = stats.frames_captured();
    let frames_dropped = stats.frames_dropped();
    let frames_consumed = frames_consumed.load(Ordering::Relaxed);
    let channel_len = handle.receiver().len();
    handle.stop()?;
    consumer_join
        .join()
        .map_err(|_err| CaptureError::ThreadFailed {
            detail: "capture bench consumer panicked".to_owned(),
        })?;

    let elapsed = after.wall.duration_since(before.wall);
    let cpu_delta = after
        .process_time
        .checked_sub(before.process_time)
        .unwrap_or_default()
        .as_secs_f64();
    let cores = u32::try_from(
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .max(1),
    )
    .unwrap_or(u32::MAX);
    let cpu_percent = if elapsed.is_zero() {
        0.0
    } else {
        (cpu_delta / elapsed.as_secs_f64()) * 100.0 / f64::from(cores)
    };

    Ok(SteadyStateReport {
        source: cpu_sample_source(),
        duration: elapsed,
        cpu_percent,
        frames_captured,
        frames_dropped,
        frames_consumed,
        channel_len,
    })
}

#[cfg(windows)]
const fn cpu_sample_source() -> &'static str {
    "GetProcessTimes"
}

#[cfg(not(windows))]
const fn cpu_sample_source() -> &'static str {
    "/proc/self/stat"
}

#[cfg(windows)]
fn cpu_sample() -> Result<CpuSample, String> {
    use windows::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            std::ptr::addr_of_mut!(creation),
            std::ptr::addr_of_mut!(exit),
            std::ptr::addr_of_mut!(kernel),
            std::ptr::addr_of_mut!(user),
        )
    }
    .map_err(|err| err.to_string())?;

    Ok(CpuSample {
        process_time: ticks_to_duration(filetime_ticks(kernel) + filetime_ticks(user), 10_000_000),
        wall: Instant::now(),
    })
}

#[cfg(windows)]
fn filetime_ticks(value: windows::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(not(windows))]
fn cpu_sample() -> Result<CpuSample, String> {
    let stat = std::fs::read_to_string("/proc/self/stat").map_err(|err| err.to_string())?;
    let fields = stat
        .rsplit_once(") ")
        .ok_or_else(|| "unexpected /proc/self/stat shape".to_owned())?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let user_ticks = fields
        .get(11)
        .ok_or_else(|| "missing utime in /proc/self/stat".to_owned())?
        .parse::<u64>()
        .map_err(|err| err.to_string())?;
    let kernel_ticks = fields
        .get(12)
        .ok_or_else(|| "missing stime in /proc/self/stat".to_owned())?
        .parse::<u64>()
        .map_err(|err| err.to_string())?;

    Ok(CpuSample {
        process_time: ticks_to_duration(user_ticks + kernel_ticks, 100),
        wall: Instant::now(),
    })
}

fn ticks_to_duration(ticks: u64, ticks_per_second: u64) -> Duration {
    Duration::from_secs(ticks / ticks_per_second)
        + Duration::from_nanos((ticks % ticks_per_second) * 1_000_000_000 / ticks_per_second)
}
