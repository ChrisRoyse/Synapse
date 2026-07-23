use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{StorageError, StorageResult};

const GC_INTERVAL: Duration = Duration::from_mins(5);

/// One storage GC pass across all configured column families.
#[derive(Debug, Default)]
pub struct GcReport {
    pub cf_reports: Vec<GcCfReport>,
}

impl GcReport {
    /// Total rows evicted by this pass.
    #[must_use]
    pub fn total_evicted_rows(&self) -> u64 {
        self.cf_reports
            .iter()
            .map(|report| report.evicted_rows)
            .sum()
    }

    /// Finds the report for one column family.
    #[must_use]
    pub fn cf(&self, cf_name: &str) -> Option<&GcCfReport> {
        self.cf_reports
            .iter()
            .find(|report| report.cf_name == cf_name)
    }
}

/// Per-column-family GC outcome.
#[derive(Debug)]
pub struct GcCfReport {
    pub cf_name: String,
    pub before_value: u64,
    pub after_value: u64,
    pub before_estimated_num_keys: Option<u64>,
    pub after_estimated_num_keys: Option<u64>,
    pub examined_rows: u64,
    pub scan_limited: bool,
    pub evicted_rows: u64,
    pub eviction_skipped_reason: Option<&'static str>,
    pub hard_cap_reached: bool,
    pub hard_cap_code: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct GcTaskReadback {
    pub running: bool,
    pub last_started_unix_ms: Option<u64>,
    pub last_completed_unix_ms: Option<u64>,
    pub last_duration_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_unsupported_policy_skips: Vec<String>,
}

#[derive(Debug, Default)]
struct GcTaskState {
    readback: Mutex<GcTaskReadback>,
}

/// Handle for the periodic storage GC task.
#[derive(Debug)]
pub struct GcTask {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
    state: Arc<GcTaskState>,
}

impl Drop for GcTask {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.abort();
    }
}

impl GcTask {
    #[must_use]
    pub fn readback(&self) -> GcTaskReadback {
        let mut readback = self
            .state
            .readback
            .lock()
            .map_or_else(|_error| GcTaskReadback::default(), |guard| guard.clone());
        readback.running = !self.handle.is_finished();
        readback
    }
}

#[derive(Clone, Debug)]
pub struct GcConfig {
    interval: Duration,
}

impl GcConfig {
    pub const fn from_retention_defaults() -> Self {
        Self {
            interval: GC_INTERVAL,
        }
    }
}

impl GcConfig {
    pub(crate) const fn interval(&self) -> Duration {
        self.interval
    }
}

pub trait GcRunner: Send + Sync + 'static {
    fn run_once(&self) -> StorageResult<GcReport>;
}

pub fn spawn_runner(runner: Arc<dyn GcRunner>, interval: Duration) -> StorageResult<GcTask> {
    let handle =
        tokio::runtime::Handle::try_current().map_err(|error| StorageError::WriteFailed {
            cf_name: "storage_gc".to_owned(),
            detail: error.to_string(),
        })?;
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(GcTaskState::default());
    let task_state = Arc::clone(&state);
    let task = handle.spawn(async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let started = mark_gc_tick_started(&task_state);
                    // The GC pass is synchronous and can run for minutes over
                    // hundreds of megabytes (native-CF compaction, tombstone
                    // purge). Admit it onto the dedicated blocking pool so it
                    // never parks a runtime worker serving MCP requests (#1798).
                    let tick_runner = Arc::clone(&runner);
                    let result = crate::maintenance::run_admitted_maintenance(
                        "storage_gc",
                        move || tick_runner.run_once(),
                    )
                    .await;
                    mark_gc_tick_completed(&task_state, started, &result);
                    if let Err(error) = result {
                        tracing::warn!(error = %error, "storage GC tick failed");
                    }
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });
    Ok(GcTask {
        shutdown: Some(shutdown),
        handle: task,
        state,
    })
}

#[derive(Clone, Copy, Debug)]
struct TickStarted {
    unix_ms: u64,
    instant: Instant,
}

fn mark_gc_tick_started(state: &GcTaskState) -> TickStarted {
    let started = TickStarted {
        unix_ms: unix_time_ms_now(),
        instant: Instant::now(),
    };
    if let Ok(mut readback) = state.readback.lock() {
        readback.running = true;
        readback.last_started_unix_ms = Some(started.unix_ms);
    }
    started
}

fn mark_gc_tick_completed(
    state: &GcTaskState,
    started: TickStarted,
    result: &StorageResult<GcReport>,
) {
    if let Ok(mut readback) = state.readback.lock() {
        readback.last_completed_unix_ms = Some(unix_time_ms_now());
        readback.last_duration_ms = Some(duration_millis_u64(started.instant.elapsed()));
        readback.last_error = result.as_ref().err().map(ToString::to_string);
        readback.last_unsupported_policy_skips = result
            .as_ref()
            .ok()
            .map(|report| {
                report
                    .cf_reports
                    .iter()
                    .filter(|cf| cf.eviction_skipped_reason.is_some())
                    .map(|cf| cf.cf_name.clone())
                    .collect()
            })
            .unwrap_or_default();
    }
}

fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
