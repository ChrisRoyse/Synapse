use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{StorageError, StorageResult};

const GC_INTERVAL: Duration = Duration::from_mins(5);
const GC_RETRY_MAX_ATTEMPTS: u32 = 5;
const GC_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const GC_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const GC_RETRYABLE_CALYX_BACKPRESSURE: &str = "retryable_calyx_backpressure";
const GC_RETRY_EXHAUSTED_CALYX_BACKPRESSURE: &str = "retry_exhausted_calyx_backpressure";
const GC_TERMINAL_ERROR: &str = "terminal_non_retryable";

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
    pub last_error_classification: Option<String>,
    pub last_attempt_count: u32,
    pub next_retry_unix_ms: Option<u64>,
    pub retry_exhausted: bool,
    pub last_successful_unix_ms: Option<u64>,
    pub last_successful_cf_readback_count: Option<u64>,
    pub last_successful_total_examined_rows: Option<u64>,
    pub last_successful_total_evicted_rows: Option<u64>,
    pub last_successful_after_value_sum: Option<u64>,
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

    /// Requests terminal shutdown and retains the exact task owner until the
    /// periodic loop has joined.
    ///
    /// A maintenance attempt may already be running inside `spawn_blocking`.
    /// Tokio cannot abort a started blocking closure, so dropping/aborting only
    /// the async wrapper would let storage work outlive the vault it owns. This
    /// method instead signals the loop and awaits its `JoinHandle`; an in-flight
    /// attempt therefore finishes before the caller may close storage.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error if the task failed before reaching
    /// its terminal join boundary.
    pub async fn shutdown(mut self, task: &'static str) -> StorageResult<()> {
        let shutdown_signal_sent = self
            .shutdown
            .take()
            .is_some_and(|shutdown| shutdown.send(()).is_ok());
        tracing::info!(
            code = "STORAGE_MAINTENANCE_SHUTDOWN_REQUESTED",
            task,
            shutdown_signal_sent,
            task_finished_before_join = self.handle.is_finished(),
            "requested periodic storage-maintenance shutdown and retained its exact task owner"
        );
        let joined = (&mut self.handle).await;
        match joined {
            Ok(()) => {
                tracing::info!(
                    code = "STORAGE_MAINTENANCE_SHUTDOWN_JOINED",
                    task,
                    "periodic storage-maintenance task reached terminal state before vault close"
                );
                Ok(())
            }
            Err(error) => Err(StorageError::WriteFailed {
                cf_name: "storage_maintenance".to_owned(),
                detail: format!(
                    "join periodic {task} task before vault close: {error}; the task is terminal but shutdown is not clean"
                ),
            }),
        }
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

#[derive(Clone, Copy, Debug)]
pub enum MaintenanceTaskKind {
    GarbageCollection,
    Checkpoint,
    /// Keeps the vault's derived layers — the persisted search generation and
    /// the measured lens coverage — from silently expiring (#1891, #1894).
    DerivedState,
}

impl MaintenanceTaskKind {
    const fn operation(self) -> &'static str {
        match self {
            Self::GarbageCollection => "storage_gc",
            Self::Checkpoint => "storage_checkpoint",
            Self::DerivedState => "storage_derived_state",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::GarbageCollection => "garbage_collection",
            Self::Checkpoint => "checkpoint",
            Self::DerivedState => "derived_state",
        }
    }
}

pub fn spawn_runner(
    runner: Arc<dyn GcRunner>,
    interval: Duration,
    task_kind: MaintenanceTaskKind,
) -> StorageResult<GcTask> {
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
                biased;
                _ = &mut shutdown_rx => break,
                _ = interval.tick() => {
                    let started = mark_gc_tick_started(&task_state);
                    let mut attempt = 0_u32;
                    let result = loop {
                        attempt = attempt.saturating_add(1);
                        mark_gc_attempt_started(&task_state, attempt);
                        // The GC pass is synchronous and can run for minutes over
                        // hundreds of megabytes (native-CF compaction, tombstone
                        // purge). Admit each attempt onto the dedicated blocking
                        // pool so neither the pass nor retry backoff parks a
                        // runtime worker serving MCP requests (#1798/#1836).
                        let tick_runner = Arc::clone(&runner);
                        let attempt_result = crate::maintenance::run_admitted_maintenance(
                            task_kind.operation(),
                            move || tick_runner.run_once(),
                        )
                        .await;
                        let Some(classification) = retryable_gc_error(&attempt_result) else {
                            break attempt_result;
                        };
                        if attempt >= GC_RETRY_MAX_ATTEMPTS {
                            break attempt_result;
                        }
                        let delay = gc_retry_delay(started.unix_ms, attempt);
                        let next_retry_unix_ms = unix_time_ms_now().saturating_add(
                            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        );
                        mark_gc_retry_scheduled(
                            &task_state,
                            attempt,
                            next_retry_unix_ms,
                            classification,
                            &attempt_result,
                        );
                        tracing::warn!(
                            code = "STORAGE_MAINTENANCE_RETRY_SCHEDULED",
                            task = task_kind.label(),
                            attempt,
                            max_attempts = GC_RETRY_MAX_ATTEMPTS,
                            retry_after_ms = delay.as_millis(),
                            next_retry_unix_ms,
                            classification,
                            error = ?attempt_result.as_ref().err(),
                            "storage maintenance released admission after retryable contention and scheduled a bounded retry"
                        );
                        // Tokio sleep performs no work while pending. It is
                        // outside both the admitted blocking operation and every
                        // storage/commit/checkpoint lock.
                        tokio::time::sleep(delay).await;
                    };
                    mark_gc_tick_completed(&task_state, started, attempt, &result);
                    if let Err(error) = result {
                        tracing::warn!(
                            code = "STORAGE_MAINTENANCE_TICK_FAILED",
                            task = task_kind.label(),
                            attempts = attempt,
                            classification = final_error_classification(&error, attempt),
                            error = %error,
                            "storage maintenance tick failed"
                        );
                    }
                }
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
        readback.last_attempt_count = 0;
        readback.next_retry_unix_ms = None;
        readback.retry_exhausted = false;
    }
    started
}

fn mark_gc_attempt_started(state: &GcTaskState, attempt: u32) {
    if let Ok(mut readback) = state.readback.lock() {
        readback.last_attempt_count = attempt;
        readback.next_retry_unix_ms = None;
    }
}

fn mark_gc_retry_scheduled(
    state: &GcTaskState,
    attempt: u32,
    next_retry_unix_ms: u64,
    classification: &'static str,
    result: &StorageResult<GcReport>,
) {
    if let Ok(mut readback) = state.readback.lock() {
        readback.last_attempt_count = attempt;
        readback.next_retry_unix_ms = Some(next_retry_unix_ms);
        readback.retry_exhausted = false;
        readback.last_error = result.as_ref().err().map(ToString::to_string);
        readback.last_error_classification = Some(classification.to_owned());
    }
}

fn mark_gc_tick_completed(
    state: &GcTaskState,
    started: TickStarted,
    attempts: u32,
    result: &StorageResult<GcReport>,
) {
    if let Ok(mut readback) = state.readback.lock() {
        readback.last_completed_unix_ms = Some(unix_time_ms_now());
        readback.last_duration_ms = Some(duration_millis_u64(started.instant.elapsed()));
        readback.last_attempt_count = attempts;
        readback.next_retry_unix_ms = None;
        readback.last_error = result.as_ref().err().map(ToString::to_string);
        readback.last_error_classification = result
            .as_ref()
            .err()
            .map(|error| final_error_classification(error, attempts).to_owned());
        readback.retry_exhausted = result.as_ref().err().is_some_and(|error| {
            retryable_storage_error(error) && attempts >= GC_RETRY_MAX_ATTEMPTS
        });
        if let Ok(report) = result {
            readback.last_successful_unix_ms = readback.last_completed_unix_ms;
            readback.last_successful_cf_readback_count =
                Some(u64::try_from(report.cf_reports.len()).unwrap_or(u64::MAX));
            readback.last_successful_total_examined_rows = Some(
                report
                    .cf_reports
                    .iter()
                    .map(|cf| cf.examined_rows)
                    .fold(0_u64, u64::saturating_add),
            );
            readback.last_successful_total_evicted_rows = Some(report.total_evicted_rows());
            readback.last_successful_after_value_sum = Some(
                report
                    .cf_reports
                    .iter()
                    .map(|cf| cf.after_value)
                    .fold(0_u64, u64::saturating_add),
            );
        }
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

fn retryable_gc_error(result: &StorageResult<GcReport>) -> Option<&'static str> {
    result
        .as_ref()
        .err()
        .filter(|error| retryable_storage_error(error))
        .map(|_| GC_RETRYABLE_CALYX_BACKPRESSURE)
}

fn retryable_storage_error(error: &StorageError) -> bool {
    error.code() == synapse_calyx::SYNAPSE_CALYX_BACKPRESSURE
}

fn final_error_classification(error: &StorageError, attempts: u32) -> &'static str {
    if retryable_storage_error(error) && attempts >= GC_RETRY_MAX_ATTEMPTS {
        GC_RETRY_EXHAUSTED_CALYX_BACKPRESSURE
    } else if retryable_storage_error(error) {
        GC_RETRYABLE_CALYX_BACKPRESSURE
    } else {
        GC_TERMINAL_ERROR
    }
}

fn gc_retry_delay(tick_unix_ms: u64, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    let exponential_ms = u64::try_from(GC_RETRY_BASE_DELAY.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_mul(multiplier);
    let cap_ms = u64::try_from(GC_RETRY_MAX_DELAY.as_millis()).unwrap_or(u64::MAX);
    let bounded_ms = exponential_ms.min(cap_ms);
    let jitter_window_ms = (bounded_ms / 4).max(1);
    let jitter_ms =
        tick_unix_ms.wrapping_add(u64::from(attempt).wrapping_mul(0x9E37_79B9)) % jitter_window_ms;
    Duration::from_millis(bounded_ms.saturating_add(jitter_ms).min(cap_ms))
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
