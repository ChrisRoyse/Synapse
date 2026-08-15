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
const GC_DEFERRED_ON_MAINTENANCE_LOCK: &str = "deferred_on_maintenance_lock";
const GC_MAINTENANCE_LOCK_STARVED: &str = "maintenance_lock_starved";
const GC_TERMINAL_ERROR: &str = "terminal_non_retryable";

/// Calyx's admission-refusal code for the shared native maintenance lock.
///
/// Matched by value rather than imported because it is a Calyx subsystem-local
/// code that crosses the Synapse bridge verbatim (it has no `SYNAPSE_CALYX_*`
/// alias in the PRD-18 mapping table), so `StorageError::code()` reports exactly
/// this string.
const CALYX_ASTER_NATIVE_COMPACTION_BUSY: &str = "CALYX_ASTER_NATIVE_COMPACTION_BUSY";

/// In-tick attempts allowed when admission to the native maintenance lock was
/// refused.
///
/// Calyx's fair-handoff admission already waited out the active holder before
/// reporting this, so the retry allowance here is deliberately small: more
/// attempts would push one tick past the start of its successor without adding
/// any waiting that the fair handoff did not already do.
const GC_MAINTENANCE_LOCK_MAX_ATTEMPTS: u32 = 2;

/// How long continuous admission refusal stays *backpressure* before it is
/// escalated to a storage error.
///
/// Four GC intervals. Below this the maintenance lock is legitimately busy and
/// GC is deferred, which is a healthy, self-clearing state; at or above it the
/// lock is not being handed over at all and that is a real fault worth failing
/// health on — named with the exact holder and hold duration Calyx reported.
const GC_MAINTENANCE_LOCK_DEFER_BUDGET: Duration = Duration::from_mins(20);

/// One storage GC pass across all configured column families.
#[derive(Debug, Default)]
pub struct GcReport {
    pub cf_reports: Vec<GcCfReport>,
    /// The one pinned MVCC instant this pass decided deletions from (#2058).
    ///
    /// `None` for maintenance passes that take no deletion decision at all
    /// (checkpoint, derived state), which is why it is an `Option` rather than a
    /// zeroed struct: a reported `pinned_seq` of 0 would be indistinguishable
    /// from a real census that failed to record one.
    pub source_census: Option<DerivedSourceCensus>,
    /// One bounded in-RAM MVCC version-chain reclamation pass (#2122).
    ///
    /// `None` for maintenance kinds that run no such pass, for the same reason
    /// `source_census` is an `Option`: a zeroed pass would read as "reclamation
    /// ran and freed nothing", which is exactly the state a daemon with the
    /// reclaimer unwired reports, and telling those two apart is the whole point
    /// of publishing it.
    pub snapshot_version_gc: Option<SnapshotVersionGcPassReport>,
}

/// One snapshot-version GC pass plus the memory measurement that bracketed it.
///
/// The Calyx-side pass says what was reclaimed; the private-commit samples say
/// whether it mattered. Both are needed: #2122's whole failure mode was a
/// subsystem reporting success (a GC task that ran every 5 minutes, on time,
/// with no errors) while the number that actually moves — process committed
/// private memory — ratcheted up 1.07 GB/hour underneath it.
#[derive(Clone, Debug, Default)]
pub struct SnapshotVersionGcPassReport {
    pub pass: synapse_calyx::SynapseCalyxSnapshotVersionGcPass,
    /// Process committed private bytes sampled immediately before the pass.
    ///
    /// Private commit, never working set: on Windows the working set is trimmed
    /// by the OS and fell while this leak grew (see
    /// `synapse_calyx::process_private_bytes`).
    pub private_bytes_before: u64,
    /// The same counter immediately after the pass.
    ///
    /// Not expected to fall by `bytes_reclaimed`: freeing an allocation returns
    /// it to the process allocator, which decides separately whether to return
    /// the page to the OS. This measures the pass's effect on the number the
    /// operator sees, which is the honest thing to publish even when the two
    /// disagree.
    pub private_bytes_after: u64,
    /// Budget multiplier this pass ran at; 1 is the unescalated base.
    pub escalation_factor: u32,
    pub budget_max_versions: usize,
    pub budget_max_pass_us: u64,
}

impl SnapshotVersionGcPassReport {
    /// One-line readback for health and logs.
    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "floor_seq={} current_seq={} active_leases={} versions_reclaimed={} bytes_reclaimed={} \
             chains_compacted={} chains_scanned={} shards={}/{} shard_guard_holds={} \n             sweep_completed={} stopped_on={} \
             elapsed_us={} max_shard_hold_us={} private_bytes_before={} private_bytes_after={} \
             escalation_factor={} budget_max_versions={} budget_max_pass_us={}",
            self.pass.floor_seq,
            self.pass.current_seq,
            self.pass.active_leases,
            self.pass.versions_reclaimed,
            self.pass.bytes_reclaimed,
            self.pass.chains_compacted,
            self.pass.chains_scanned,
            self.pass.shards_visited,
            self.pass.shards_total,
            self.pass.shard_guard_holds,
            self.pass.sweep_completed,
            self.pass.stopped_on,
            self.pass.elapsed_us,
            self.pass.max_shard_hold_us,
            self.private_bytes_before,
            self.private_bytes_after,
            self.escalation_factor,
            self.budget_max_versions,
            self.budget_max_pass_us,
        )
    }
}

/// What the #1882 GC protection set was derived from (#2058).
///
/// GC's authority to delete a source row is exactly "no live derived
/// constellation points at it", and that claim is only true relative to some
/// instant. This records which instant, so a health reader can tell a completed
/// census from a skipped one without inferring it from the absence of an error.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DerivedSourceCensus {
    /// Whether this pass established a full baseline, applied an exact MVCC
    /// delta, observed no Base changes, or rebuilt after explicit invalidation.
    pub mode: &'static str,
    /// The committed sequence the census pinned for its whole walk.
    pub pinned_seq: u64,
    /// Sequence of the prior exact census when this pass refreshed a cache.
    pub previous_pinned_seq: Option<u64>,
    /// Bounded pages read at that sequence.
    pub pages: u64,
    /// `Base` rows handed to the fold.
    pub base_rows_visited: u64,
    /// Exact Base keys changed after `previous_pinned_seq`.
    pub changed_base_keys: u64,
    /// Why incremental state was invalidated and fully rebuilt, when it was.
    pub rebase_reason: Option<&'static str>,
    /// Source column families with at least one protected row.
    pub referenced_column_families: u64,
    /// Source rows protected from eviction because a live derived
    /// constellation still points at them.
    pub referenced_rows: u64,
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
    /// Measured values. `None` means policy resolved the outcome before any
    /// scan; zero remains a real measured zero.
    pub before_value: Option<u64>,
    pub after_value: Option<u64>,
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
    /// True while the newest tick was refused admission to the shared Calyx
    /// native maintenance lock rather than failing (#2067).
    ///
    /// This is backpressure, not a fault: some other named pass owns the lock
    /// and will release it. `last_error` stays `None` in this state — and so
    /// storage health stays out of `error` — until the deferral outlives
    /// [`GC_MAINTENANCE_LOCK_DEFER_BUDGET`], at which point `last_error` names
    /// the holder and how long it has held.
    pub deferred_on_maintenance_lock: bool,
    /// When the current unbroken run of deferrals began.
    pub deferred_since_unix_ms: Option<u64>,
    /// Length of that unbroken run, in ticks.
    pub consecutive_maintenance_lock_deferrals: u32,
    /// Calyx's own refusal text for the newest deferral, which carries the
    /// holder's purpose and how many milliseconds it had held the lock.
    pub last_maintenance_lock_detail: Option<String>,
    /// The single pinned committed sequence the last successful pass took its
    /// #1882 protection set from (#2058).
    pub last_successful_source_census_pinned_seq: Option<u64>,
    /// Exact census refresh mode published by the last successful pass.
    pub last_successful_source_census_mode: Option<String>,
    /// Previous exact sequence used as this pass's delta lower bound.
    pub last_successful_source_census_previous_pinned_seq: Option<u64>,
    /// Bounded pages that census read at that one sequence.
    pub last_successful_source_census_pages: Option<u64>,
    /// `Base` rows that census folded at that one sequence.
    pub last_successful_source_census_base_rows: Option<u64>,
    /// Base keys in the exact MVCC delta for the last successful pass.
    pub last_successful_source_census_changed_base_keys: Option<u64>,
    /// Explicit reason the cache was rebuilt rather than incremented.
    pub last_successful_source_census_rebase_reason: Option<String>,
    /// Source rows that census protected from eviction.
    pub last_successful_source_census_referenced_rows: Option<u64>,
    /// In-RAM MVCC versions reclaimed by the last successful pass (#2122).
    ///
    /// The pass/fail number for the fix: it was structurally zero forever,
    /// because `snapshot_version_gc` had no caller anywhere in Synapse.
    pub last_successful_snapshot_versions_reclaimed: Option<u64>,
    /// Value bytes those versions held.
    pub last_successful_snapshot_version_bytes_reclaimed: Option<u64>,
    /// The pinned-reader floor that pass reclaimed strictly below, and the
    /// vault's committed sequence at the time.
    ///
    /// Published as a pair because a floor stuck far below `current_seq` is the
    /// one way this fix fails silently: a leaked reader lease pins it and every
    /// subsequent pass correctly reclaims nothing while reporting success.
    pub last_successful_snapshot_version_floor_seq: Option<u64>,
    pub last_successful_snapshot_version_current_seq: Option<u64>,
    /// Whether that pass visited every shard and every chain. Only a completed
    /// sweep licenses reading `versions_reclaimed = 0` as "nothing left to
    /// reclaim" rather than "this pass ran out of budget first".
    pub last_successful_snapshot_version_sweep_completed: Option<bool>,
    /// Longest single row-table shard write-guard hold in that pass, in
    /// microseconds. This is the commit-latency cost of reclamation and the
    /// number that has to stay bounded for it to be safe to run continuously.
    pub last_successful_snapshot_version_max_shard_hold_us: Option<u64>,
    /// Process committed private bytes sampled just after that pass.
    pub last_successful_snapshot_version_private_bytes: Option<u64>,
    /// The full pass readback, as one parseable line.
    pub last_successful_snapshot_version_detail: Option<String>,
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
        let cadence = interval;
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + cadence, cadence);
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
                        let Some(kind) = retryable_gc_failure_kind(&attempt_result) else {
                            break attempt_result;
                        };
                        if attempt >= kind.max_attempts() {
                            break attempt_result;
                        }
                        let classification = kind.retrying_classification();
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
                    let deferral = mark_gc_tick_completed(&task_state, started, attempt, &result);
                    // A maintenance pass may legitimately take longer than its
                    // cadence. `MissedTickBehavior::Delay` prevents a *burst* of
                    // overdue ticks, but the first already-due tick still
                    // resolves immediately when the loop reaches `tick()`
                    // again. That admitted a second multi-minute derived-state
                    // pass as soon as the first one released the whole-corpus
                    // lane, keeping foreground Calyx calls queued indefinitely.
                    //
                    // Maintenance is state-convergent: one completed pass has
                    // already observed everything committed before its own
                    // coherent snapshots. Discard cadence debt and schedule the
                    // next pass one full cadence after this terminal readback.
                    // This is completion-relative scheduling, not a disabled or
                    // skipped capability; every runner still executes forever,
                    // with a real idle/admission window between passes.
                    interval.reset_after(cadence);
                    if let Err(error) = result {
                        match deferral {
                            Some(deferral) if !deferral.escalated => tracing::warn!(
                                code = "STORAGE_MAINTENANCE_DEFERRED_ON_LOCK",
                                task = task_kind.label(),
                                attempts = attempt,
                                classification = GC_DEFERRED_ON_MAINTENANCE_LOCK,
                                deferred_since_unix_ms = deferral.since_unix_ms,
                                deferred_for_ms = deferral.deferred_for_ms,
                                consecutive_deferrals = deferral.consecutive,
                                escalate_after_ms = GC_MAINTENANCE_LOCK_DEFER_BUDGET.as_millis(),
                                error = %error,
                                "storage maintenance was refused admission to the shared Calyx maintenance lock; this tick is deferred, not failed"
                            ),
                            _ => tracing::warn!(
                                code = "STORAGE_MAINTENANCE_TICK_FAILED",
                                task = task_kind.label(),
                                attempts = attempt,
                                classification = final_error_classification(&error, attempt),
                                error = %error,
                                "storage maintenance tick failed"
                            ),
                        }
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
        // A deferral mid-tick must not flash `last_error` — a health read that
        // lands between attempts would otherwise see a self-clearing admission
        // refusal as a storage fault (#2067). The refusal text is still exposed,
        // as the deferral detail it is.
        if classification == GC_DEFERRED_ON_MAINTENANCE_LOCK {
            readback.last_error = None;
            readback.last_maintenance_lock_detail = result.as_ref().err().map(ToString::to_string);
        } else {
            readback.last_error = result.as_ref().err().map(ToString::to_string);
        }
        readback.last_error_classification = Some(classification.to_owned());
    }
}

/// What a tick that ended in maintenance-lock deferral looked like.
#[derive(Clone, Copy, Debug)]
struct GcDeferral {
    since_unix_ms: u64,
    deferred_for_ms: u64,
    consecutive: u32,
    /// The unbroken deferral outlived [`GC_MAINTENANCE_LOCK_DEFER_BUDGET`], so
    /// it was promoted from backpressure to a reported storage error.
    escalated: bool,
}

#[expect(
    clippy::too_many_lines,
    reason = "one tick completion must atomically classify success, deferral escalation, counters, and operator-visible readback"
)]
fn mark_gc_tick_completed(
    state: &GcTaskState,
    started: TickStarted,
    attempts: u32,
    result: &StorageResult<GcReport>,
) -> Option<GcDeferral> {
    let mut deferral = None;
    if let Ok(mut readback) = state.readback.lock() {
        let completed_unix_ms = unix_time_ms_now();
        readback.last_completed_unix_ms = Some(completed_unix_ms);
        readback.last_duration_ms = Some(duration_millis_u64(started.instant.elapsed()));
        readback.last_attempt_count = attempts;
        readback.next_retry_unix_ms = None;
        let failure_kind = result.as_ref().err().map(gc_failure_kind);
        if failure_kind == Some(GcFailureKind::MaintenanceLockBusy) {
            // Admission backpressure: some other named pass legitimately owns
            // the lock. Report it as the deferral it is and leave `last_error`
            // clear so health does not call a self-clearing contention a
            // storage fault — unless the deferral has outlived its budget, in
            // which case the lock is genuinely not being handed over (#2067).
            let since_unix_ms = *readback
                .deferred_since_unix_ms
                .get_or_insert(started.unix_ms);
            let consecutive = readback
                .consecutive_maintenance_lock_deferrals
                .saturating_add(1);
            let deferred_for_ms = completed_unix_ms.saturating_sub(since_unix_ms);
            let detail = result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            let escalated = deferred_for_ms
                >= u64::try_from(GC_MAINTENANCE_LOCK_DEFER_BUDGET.as_millis()).unwrap_or(u64::MAX);
            readback.deferred_on_maintenance_lock = true;
            readback.consecutive_maintenance_lock_deferrals = consecutive;
            readback.last_maintenance_lock_detail = Some(detail.clone());
            readback.retry_exhausted = false;
            readback.last_unsupported_policy_skips = Vec::new();
            if escalated {
                readback.last_error = Some(format!(
                    "storage maintenance has been unable to acquire the Calyx native maintenance lock for {deferred_for_ms} ms across {consecutive} consecutive ticks (escalation budget {} ms); the refusal names its holder: {detail}",
                    GC_MAINTENANCE_LOCK_DEFER_BUDGET.as_millis()
                ));
                readback.last_error_classification = Some(GC_MAINTENANCE_LOCK_STARVED.to_owned());
            } else {
                readback.last_error = None;
                readback.last_error_classification =
                    Some(GC_DEFERRED_ON_MAINTENANCE_LOCK.to_owned());
            }
            deferral = Some(GcDeferral {
                since_unix_ms,
                deferred_for_ms,
                consecutive,
                escalated,
            });
            return deferral;
        }
        readback.deferred_on_maintenance_lock = false;
        readback.deferred_since_unix_ms = None;
        readback.consecutive_maintenance_lock_deferrals = 0;
        readback.last_maintenance_lock_detail = None;
        readback.last_error = result.as_ref().err().map(ToString::to_string);
        readback.last_error_classification = result
            .as_ref()
            .err()
            .map(|error| final_error_classification(error, attempts).to_owned());
        readback.retry_exhausted =
            failure_kind.is_some_and(|kind| kind.is_retryable() && attempts >= kind.max_attempts());
        if let Ok(report) = result {
            readback.last_successful_unix_ms = readback.last_completed_unix_ms;
            // Only when this pass actually reported on column families (#2088
            // ask 3). `storage_checkpoint` and `storage_derived_state` sweep no
            // CF and evict no row, so a zeroed set of aggregates here would be
            // indistinguishable from a GC pass that examined a corpus and found
            // nothing — a report describing nothing, published as a measurement.
            // Left untouched instead, exactly as `source_census` below already
            // is, so whatever a real pass last measured stays readable.
            if !report.cf_reports.is_empty() {
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
                        .filter_map(|cf| cf.after_value)
                        .fold(0_u64, u64::saturating_add),
                );
            }
            // Left untouched when this pass carried no census, so the last real
            // census a GC pass ran is still readable after a checkpoint or
            // derived-state tick reports through the same readback (#2058).
            if let Some(census) = report.source_census {
                readback.last_successful_source_census_pinned_seq = Some(census.pinned_seq);
                readback.last_successful_source_census_mode = Some(census.mode.to_owned());
                readback.last_successful_source_census_previous_pinned_seq =
                    census.previous_pinned_seq;
                readback.last_successful_source_census_pages = Some(census.pages);
                readback.last_successful_source_census_base_rows = Some(census.base_rows_visited);
                readback.last_successful_source_census_changed_base_keys =
                    Some(census.changed_base_keys);
                readback.last_successful_source_census_rebase_reason =
                    census.rebase_reason.map(str::to_owned);
                readback.last_successful_source_census_referenced_rows =
                    Some(census.referenced_rows);
            }
            // Same discipline: left untouched by maintenance kinds that run no
            // reclamation pass, so a checkpoint tick reporting through this
            // readback cannot erase what the last real pass measured (#2122).
            if let Some(reclaim) = report.snapshot_version_gc.as_ref() {
                mark_snapshot_version_gc_pass(&mut readback, reclaim);
            }
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
    deferral
}

/// Publishes one snapshot-version GC pass into the task readback (#2122).
fn mark_snapshot_version_gc_pass(
    readback: &mut GcTaskReadback,
    reclaim: &SnapshotVersionGcPassReport,
) {
    readback.last_successful_snapshot_versions_reclaimed = Some(reclaim.pass.versions_reclaimed);
    readback.last_successful_snapshot_version_bytes_reclaimed = Some(reclaim.pass.bytes_reclaimed);
    readback.last_successful_snapshot_version_floor_seq = Some(reclaim.pass.floor_seq);
    readback.last_successful_snapshot_version_current_seq = Some(reclaim.pass.current_seq);
    readback.last_successful_snapshot_version_sweep_completed = Some(reclaim.pass.sweep_completed);
    readback.last_successful_snapshot_version_max_shard_hold_us =
        Some(reclaim.pass.max_shard_hold_us);
    readback.last_successful_snapshot_version_private_bytes = Some(reclaim.private_bytes_after);
    readback.last_successful_snapshot_version_detail = Some(reclaim.detail());
}

/// Why a maintenance attempt failed, at the granularity the tick loop acts on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GcFailureKind {
    /// Calyx asked the caller to slow down. The work itself is sound.
    CalyxBackpressure,
    /// Admission to the shared native maintenance lock was refused because
    /// another named pass owns it. Transient by construction (#2067).
    MaintenanceLockBusy,
    /// Anything else: a real storage failure that retrying cannot fix.
    Terminal,
}

impl GcFailureKind {
    const fn is_retryable(self) -> bool {
        matches!(self, Self::CalyxBackpressure | Self::MaintenanceLockBusy)
    }

    const fn max_attempts(self) -> u32 {
        match self {
            Self::CalyxBackpressure => GC_RETRY_MAX_ATTEMPTS,
            Self::MaintenanceLockBusy => GC_MAINTENANCE_LOCK_MAX_ATTEMPTS,
            Self::Terminal => 1,
        }
    }

    const fn retrying_classification(self) -> &'static str {
        match self {
            Self::CalyxBackpressure => GC_RETRYABLE_CALYX_BACKPRESSURE,
            Self::MaintenanceLockBusy => GC_DEFERRED_ON_MAINTENANCE_LOCK,
            Self::Terminal => GC_TERMINAL_ERROR,
        }
    }
}

/// How the maintenance tick loop will treat one failure: the classification it
/// publishes, whether it retries in place, and how many attempts it allows.
///
/// This is the same verdict [`mark_gc_tick_completed`] writes into
/// [`GcTaskReadback::last_error_classification`], exposed so an operator — or a
/// manual FSV — can ask it of an error without waiting five minutes for a tick
/// to publish one. It reads the error's own `code()` and nothing else, so the
/// answer it gives is the answer the loop gives.
///
/// Load-bearing for #2088 ask 2: `storage_derived_state` now returns real
/// errors, and the decision that a failed derived-state tick is **not** retried
/// in place is expressed by the error variant it returns rather than by the
/// absence of an error. This is where that decision is observable.
#[must_use]
pub fn maintenance_failure_classification(error: &StorageError) -> (&'static str, bool, u32) {
    let kind = gc_failure_kind(error);
    (
        kind.retrying_classification(),
        kind.is_retryable(),
        kind.max_attempts(),
    )
}

fn gc_failure_kind(error: &StorageError) -> GcFailureKind {
    match error.code() {
        code if code == synapse_calyx::SYNAPSE_CALYX_BACKPRESSURE => {
            GcFailureKind::CalyxBackpressure
        }
        CALYX_ASTER_NATIVE_COMPACTION_BUSY => GcFailureKind::MaintenanceLockBusy,
        _ => GcFailureKind::Terminal,
    }
}

fn retryable_gc_failure_kind(result: &StorageResult<GcReport>) -> Option<GcFailureKind> {
    result
        .as_ref()
        .err()
        .map(gc_failure_kind)
        .filter(|kind| kind.is_retryable())
}

fn final_error_classification(error: &StorageError, attempts: u32) -> &'static str {
    let kind = gc_failure_kind(error);
    match kind {
        GcFailureKind::CalyxBackpressure if attempts >= kind.max_attempts() => {
            GC_RETRY_EXHAUSTED_CALYX_BACKPRESSURE
        }
        GcFailureKind::CalyxBackpressure => GC_RETRYABLE_CALYX_BACKPRESSURE,
        // Reached only through the non-deferral branch (a deferral classifies
        // itself against the escalation budget instead of the attempt count).
        GcFailureKind::MaintenanceLockBusy => GC_DEFERRED_ON_MAINTENANCE_LOCK,
        GcFailureKind::Terminal => GC_TERMINAL_ERROR,
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
