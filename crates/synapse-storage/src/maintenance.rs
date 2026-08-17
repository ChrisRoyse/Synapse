//! Off-runtime admission for long-running storage maintenance.
//!
//! Storage GC, disk-pressure compaction, and Calyx native-CF / tombstone-purge
//! passes are synchronous, CPU- and I/O-heavy, and can run for minutes over
//! hundreds of megabytes. Running them inline on a Tokio runtime worker (the
//! prior behaviour of the periodic GC/pressure loops) parked that worker for the
//! whole pass and starved every MCP request sharing the runtime — an
//! `initialize` handshake and even pre-storage typed-param validation could time
//! out because no worker was free to poll them (issue #1798).
//!
//! This module routes every heavy maintenance pass through
//! [`tokio::task::spawn_blocking`], which runs it on Tokio's dedicated blocking
//! thread pool instead of a runtime worker, guarded by a small dedicated
//! exclusive lane so whole-corpus passes queue instead of multiplying their
//! live row, SST-reader, and native-compaction working sets. This mirrors how
//! mature LSM engines isolate and explicitly admit background compaction work so
//! foreground request latency and host resources remain bounded.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use synapse_calyx::{
    LoweredArtifactHandle, LoweredArtifactKind, LoweringParams, SynapseCalyxVaultStatus,
    hot_context,
};
use tokio::sync::Semaphore;

use crate::{Db, StorageError, StorageResult};

/// Number of exclusive whole-corpus storage-maintenance lanes.
///
/// This is resource ownership, not a memory cap and not a data-ordering
/// primitive. Every caller traverses or compacts a substantial part of the same
/// vault. Allowing GC, derived-state/search rebuild, and disk-pressure
/// compaction to overlap multiplied independent bounded working sets into an
/// unbounded process total (#2243). One exclusive lane preserves every
/// capability while ensuring the daemon owns only one whole-corpus working set
/// at a time. Data dependencies still belong inside one admitted closure or
/// behind their own physical lock (#2150).
const STORAGE_HEAVY_MAINTENANCE_LANES: usize = 1;

static STORAGE_MAINTENANCE_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(STORAGE_HEAVY_MAINTENANCE_LANES)));

/// Maximum time a foreground MCP request may wait merely to begin owning the
/// whole-corpus lane. This is admission time, not an execution deadline.
///
/// A caller that cannot start within this budget receives a typed busy verdict
/// while the existing owner continues uninterrupted. Keeping this far below the
/// transport deadline prevents a queued request from being erased by a generic
/// client timeout without ever reaching its own code (#2245).
pub const STORAGE_FOREGROUND_ADMISSION_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct StorageMaintenanceOwner {
    operation: &'static str,
    generation: u64,
    started: Instant,
}

static STORAGE_MAINTENANCE_OWNER: LazyLock<Mutex<Option<StorageMaintenanceOwner>>> =
    LazyLock::new(|| Mutex::new(None));
static STORAGE_MAINTENANCE_OWNER_GENERATION: AtomicU64 = AtomicU64::new(0);

struct StorageMaintenanceOwnerGuard {
    operation: &'static str,
    generation: u64,
}

impl Drop for StorageMaintenanceOwnerGuard {
    fn drop(&mut self) {
        let mut owner = match STORAGE_MAINTENANCE_OWNER.lock() {
            Ok(owner) => owner,
            Err(poisoned) => poisoned.into_inner(),
        };
        if owner
            .as_ref()
            .is_some_and(|active| active.generation == self.generation)
        {
            *owner = None;
        } else {
            tracing::error!(
                code = "STORAGE_MAINTENANCE_OWNER_RELEASE_DIVERGED",
                operation = self.operation,
                owner_generation = self.generation,
                observed_operation = owner.as_ref().map(|active| active.operation),
                observed_generation = owner.as_ref().map(|active| active.generation),
                "whole-corpus permit owner registry diverged at release; the semaphore remains the authority and the registry was not overwritten"
            );
        }
    }
}

fn maintenance_owner_snapshot() -> Option<StorageMaintenanceOwner> {
    match STORAGE_MAINTENANCE_OWNER.lock() {
        Ok(owner) => owner.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn install_maintenance_owner(
    operation: &'static str,
) -> StorageResult<StorageMaintenanceOwnerGuard> {
    let generation = STORAGE_MAINTENANCE_OWNER_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    let mut owner = match STORAGE_MAINTENANCE_OWNER.lock() {
        Ok(owner) => owner,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(active) = owner.as_ref() {
        return Err(StorageError::WriteFailed {
            cf_name: "storage_maintenance".to_owned(),
            detail: format!(
                "STORAGE_MAINTENANCE_OWNER_DIVERGED requested_operation={operation} active_operation={} active_generation={} active_for_ms={}; acquired the exclusive semaphore while its owner registry was still occupied",
                active.operation,
                active.generation,
                active.started.elapsed().as_millis()
            ),
        });
    }
    *owner = Some(StorageMaintenanceOwner {
        operation,
        generation,
        started: Instant::now(),
    });
    drop(owner);
    Ok(StorageMaintenanceOwnerGuard {
        operation,
        generation,
    })
}

/// Number of concurrent bounded foreground storage reads.
///
/// This lane is intentionally separate from whole-corpus maintenance. Its
/// callers must have a statically enforced resident-corpus bound and must not
/// mutate the vault. A single permit prevents a client fan-out from multiplying
/// even those bounded working sets while allowing one foreground read to remain
/// servable during a long background pass. A caller may stream an exact
/// physical count without materializing that family; that I/O is reported
/// separately and does not turn this permit into a second corpus owner.
const STORAGE_BOUNDED_READ_LANES: usize = 1;

static STORAGE_BOUNDED_READ_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(STORAGE_BOUNDED_READ_LANES)));

/// Runs one foreground blocking storage-maintenance pass off the async runtime
/// workers with bounded admission time.
///
/// The closure executes on Tokio's blocking pool under a dedicated admission
/// permit, so it can never park a runtime worker that is polling MCP requests.
/// Structured telemetry records whether the lane was occupied when the request
/// arrived, the time spent waiting for its permit, and the execution time of
/// the pass itself. Occupancy is derived from the semaphore itself rather than
/// a second counter: a waiter can acquire a just-released permit before the
/// releasing task resumes to update a counter, which made the old telemetry
/// falsely report `in_flight=2` beside `max_concurrent=1` during handoff.
///
/// # Errors
///
/// Returns the closure's error, a typed busy error when another whole-corpus
/// owner outlives the foreground admission budget, or a structured storage
/// error if the semaphore closes or the blocking task fails to join. Scheduled
/// tasks must use [`run_background_admitted_maintenance`] instead.
async fn run_admitted_maintenance<T, F>(operation: &'static str, work: F) -> StorageResult<T>
where
    F: FnOnce() -> StorageResult<T> + Send + 'static,
    T: Send + 'static,
{
    run_admitted_maintenance_preserving_error(operation, work).await?
}

/// Runs one foreground blocking whole-corpus pass while preserving its domain
/// error type.
///
/// MCP facades have typed protocol errors which must not be flattened into a
/// generic storage failure merely to share the process-wide maintenance lane.
/// The outer [`StorageResult`] describes admission or blocking-task ownership;
/// the inner `Result<T, E>` is the operation's unchanged domain verdict.
///
/// The completion record is emitted by the blocking owner before it drops the
/// permit. `spawn_blocking` work cannot be cancelled once running, so logging
/// completion only after awaiting the join handle loses the authoritative end
/// record when an HTTP client times out and drops its request future (#2243).
/// Keeping both the permit and completion telemetry inside the worker proves
/// that detached work remains admitted until its real ownership boundary.
///
/// # Errors
///
/// Returns a typed busy error when another owner outlives the foreground
/// admission budget, or a structured storage error if the semaphore closes or
/// the blocking task fails to join. The operation's own error is returned
/// unchanged in the nested result. Scheduled tasks must use
/// [`run_background_admitted_maintenance_preserving_error`] instead.
async fn run_admitted_maintenance_preserving_error<T, E, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<Result<T, E>>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    run_admitted_maintenance_with_policy(operation, Some(STORAGE_FOREGROUND_ADMISSION_WAIT), work)
        .await
}

/// Runs one scheduled/background whole-corpus pass and waits fairly for the
/// exclusive lane.
///
/// Background maintainers have no client transport deadline and must converge,
/// so they remain queued until admitted. This explicit name prevents a public
/// MCP caller from accidentally inheriting unbounded admission again.
///
/// # Errors
///
/// Returns the operation error or a structured admission/worker error.
pub async fn run_background_admitted_maintenance<T, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<T>
where
    F: FnOnce() -> StorageResult<T> + Send + 'static,
    T: Send + 'static,
{
    run_background_admitted_maintenance_preserving_error(operation, work).await?
}

/// Background counterpart that preserves the closure's domain error type.
///
/// # Errors
///
/// Returns a structured admission/worker error, with the domain verdict nested
/// unchanged.
pub async fn run_background_admitted_maintenance_preserving_error<T, E, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<Result<T, E>>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    run_admitted_maintenance_with_policy(operation, None, work).await
}

/// Runs one foreground whole-corpus operation with bounded admission time.
///
/// Unlike scheduled maintenance, an MCP request has a finite transport
/// lifetime. Waiting unboundedly behind a multi-minute autonomous pass makes
/// the transport timeout erase the request before the operation starts. This
/// entry point retains the same exclusive semaphore and blocking owner, but
/// refuses with [`StorageError::MaintenanceBusy`] when it cannot start within
/// [`STORAGE_FOREGROUND_ADMISSION_WAIT`]. No closure is dispatched on refusal.
///
/// # Errors
///
/// Returns a typed busy error on admission expiry, a structured storage error
/// if the lane closes or ownership diverges, or the unchanged nested domain
/// outcome after admitted execution.
pub async fn run_foreground_admitted_maintenance_preserving_error<T, E, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<Result<T, E>>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    run_admitted_maintenance_preserving_error(operation, work).await
}

/// Foreground counterpart to [`run_admitted_maintenance`].
///
/// # Errors
///
/// Returns the operation error or a typed admission/worker error.
pub async fn run_foreground_admitted_maintenance<T, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<T>
where
    F: FnOnce() -> StorageResult<T> + Send + 'static,
    T: Send + 'static,
{
    run_admitted_maintenance(operation, work).await
}

async fn run_admitted_maintenance_with_policy<T, E, F>(
    operation: &'static str,
    admission_wait: Option<Duration>,
    work: F,
) -> StorageResult<Result<T, E>>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let semaphore = Arc::clone(&STORAGE_MAINTENANCE_PERMITS);
    let lane_occupied_at_request = semaphore.available_permits() == 0;
    let owner_at_request = maintenance_owner_snapshot();
    let admission_started = Instant::now();
    let acquire = Arc::clone(&semaphore).acquire_owned();
    let acquired = if let Some(wait_budget) = admission_wait {
        match tokio::time::timeout(wait_budget, acquire).await {
            Ok(acquired) => acquired,
            Err(_elapsed) => {
                let active = maintenance_owner_snapshot().or(owner_at_request);
                let active_operation = active.as_ref().map_or_else(
                    || "semaphore_handoff_pending".to_owned(),
                    |owner| owner.operation.to_owned(),
                );
                let active_for_ms = active.as_ref().map_or(0, |owner| {
                    u64::try_from(owner.started.elapsed().as_millis()).unwrap_or(u64::MAX)
                });
                let wait_budget_ms = u64::try_from(wait_budget.as_millis()).unwrap_or(u64::MAX);
                tracing::warn!(
                    code = synapse_core::error_codes::STORAGE_MAINTENANCE_BUSY,
                    requested_operation = operation,
                    active_operation,
                    active_for_ms,
                    wait_budget_ms,
                    lane_occupied_at_request,
                    foreground_work_dispatched = false,
                    exclusive_whole_corpus_lane = true,
                    "foreground whole-corpus storage operation refused after bounded admission wait; the active owner continues uninterrupted"
                );
                return Err(StorageError::MaintenanceBusy {
                    requested_operation: operation,
                    active_operation,
                    active_for_ms,
                    wait_budget_ms,
                });
            }
        }
    } else {
        acquire.await
    };
    let permit = acquired.map_err(|_closed| StorageError::WriteFailed {
        cf_name: "storage_maintenance".to_owned(),
        detail: format!(
            "{operation}: storage maintenance admission semaphore was unexpectedly closed"
        ),
    })?;
    let owner_guard = install_maintenance_owner(operation)?;
    let admission_wait_ms =
        u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let active_after_admission =
        STORAGE_HEAVY_MAINTENANCE_LANES.saturating_sub(semaphore.available_permits());
    tracing::info!(
        code = "STORAGE_MAINTENANCE_ADMITTED",
        operation,
        admission_wait_ms,
        lane_occupied_at_request,
        active_after_admission = active_after_admission as u64,
        max_concurrent = STORAGE_HEAVY_MAINTENANCE_LANES as u64,
        exclusive_whole_corpus_lane = true,
        "admitted storage maintenance onto the exclusive whole-corpus blocking lane off the async runtime workers"
    );
    let exec_started = Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _owner_guard = owner_guard;
        let outcome = work();
        // Hot-path boundary (#1686). Lowering the guard-threshold hot set is
        // off-runtime work by construction, so it rides the same admitted
        // blocking pass rather than acquiring a cadence of its own. It runs
        // after `work()` so the artifact it freezes reflects the state that
        // pass just produced, and its own failures never mask the maintenance
        // result.
        publish_lowered_guard_thresholds();
        let exec_ms = u64::try_from(exec_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            code = "STORAGE_MAINTENANCE_COMPLETED",
            operation,
            exec_ms,
            admission_wait_ms,
            is_ok = outcome.is_ok(),
            completion_emitted_by_blocking_owner = true,
            "completed off-runtime storage maintenance pass"
        );
        outcome
    })
    .await;
    match joined {
        Ok(result) => Ok(result),
        Err(join_error) => Err(StorageError::WriteFailed {
            cf_name: "storage_maintenance".to_owned(),
            detail: format!(
                "{operation}: storage maintenance blocking task failed to join: {join_error}"
            ),
        }),
    }
}

/// Runs one statically bounded, read-only storage operation off the async
/// runtime while preserving its domain error type.
///
/// This is not a second whole-corpus lane. Callers must prove both properties at
/// their dispatch boundary: the operation cannot write, and every resident
/// corpus input is bounded independently of vault size. A streaming,
/// constant-resident-memory exact-count walk is allowed and must expose whether
/// it walked or used maintained metadata. Operations that materialize an
/// entire physical family, even when read-only, remain on
/// [`run_admitted_maintenance_preserving_error`].
///
/// The permit and completion record remain inside the blocking owner because a
/// disconnected MCP client cannot abort work after `spawn_blocking` starts.
///
/// # Errors
///
/// Returns a structured storage error if bounded-read admission closes or the
/// blocking task fails to join. The operation's own error remains unchanged in
/// the nested result.
pub async fn run_admitted_bounded_read_preserving_error<T, E, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<Result<T, E>>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let semaphore = Arc::clone(&STORAGE_BOUNDED_READ_PERMITS);
    let lane_occupied_at_request = semaphore.available_permits() == 0;
    let admission_started = Instant::now();
    let permit = Arc::clone(&semaphore)
        .acquire_owned()
        .await
        .map_err(|_closed| StorageError::WriteFailed {
            cf_name: "storage_bounded_read".to_owned(),
            detail: format!(
                "{operation}: bounded storage-read admission semaphore was unexpectedly closed"
            ),
        })?;
    let admission_wait_ms =
        u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let active_after_admission =
        STORAGE_BOUNDED_READ_LANES.saturating_sub(semaphore.available_permits());
    tracing::info!(
        code = "STORAGE_BOUNDED_READ_ADMITTED",
        operation,
        admission_wait_ms,
        lane_occupied_at_request,
        active_after_admission = active_after_admission as u64,
        max_concurrent = STORAGE_BOUNDED_READ_LANES as u64,
        exclusive_whole_corpus_lane = false,
        read_only = true,
        "admitted a bounded read-only storage operation off the async runtime workers"
    );
    let exec_started = Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let outcome = work();
        let exec_ms = u64::try_from(exec_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            code = "STORAGE_BOUNDED_READ_COMPLETED",
            operation,
            exec_ms,
            admission_wait_ms,
            is_ok = outcome.is_ok(),
            completion_emitted_by_blocking_owner = true,
            "completed a bounded read-only storage operation"
        );
        outcome
    })
    .await;
    match joined {
        Ok(result) => Ok(result),
        Err(join_error) => Err(StorageError::WriteFailed {
            cf_name: "storage_bounded_read".to_owned(),
            detail: format!(
                "{operation}: bounded storage-read blocking task failed to join: {join_error}"
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Off-tick lowering of the guard-threshold hot set (#1686)
// ---------------------------------------------------------------------------
//
// Doctrine (epic #1684): the reflex tick may not issue a live Calyx call, so
// every piece of Calyx-derived intelligence it consumes must be *lowered* --
// computed off-runtime, frozen into a content-fingerprinted artifact, published
// atomically, and read through a pointer swap. This is the producer half. It
// deliberately lives on the admitted maintenance pass, which already runs on
// Tokio's blocking pool under a dedicated permit, so publishing can never park
// a runtime worker and can never be reached from a tick.
//
// Scope is `GuardThresholds` only. The armed-routine constellation,
// next-occurrence window and kernel hot-set families named in the epic depend on
// #1677/#1678, and `calyx-ward` / `calyx-oracle` are not dependencies of any
// Synapse crate yet, so there is nothing real to lower for them; inventing
// artifact kinds for them now would publish empty files that a hot path could
// mistake for intelligence.
//
// The published bytes are verified by re-reading them through the *consumer*
// (`LoweredArtifactHandle::refresh`) before the publish is counted a success.
// That makes the reader -- the thing that actually has to trust the file -- the
// arbiter of a correct publish, rather than the writer grading its own work.

/// Reader-side wall-clock staleness bound stamped into published artifacts.
///
/// Comfortably longer than the maintenance cadence so an ordinary scheduling
/// delay does not flip a healthy hot path onto the fail-closed defaults, while
/// still bounding how long a vault that stopped running maintenance can keep a
/// tick on frozen values.
pub const LOWERED_GUARD_THRESHOLDS_STALENESS_BOUND_MS: u64 = 30 * 60 * 1000;

/// Publish attempts made from the maintenance pass.
static LOWERING_PUBLISH_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
/// Publishes that wrote an artifact and re-verified it through the consumer.
static LOWERING_PUBLISH_SUCCESS: AtomicU64 = AtomicU64::new(0);
/// Publishes that failed.
static LOWERING_PUBLISH_FAILURE: AtomicU64 = AtomicU64::new(0);
/// Publishes skipped because no open, vault-backed storage handle is registered.
static LOWERING_PUBLISH_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// Monotonic artifact generation, advanced once per successful publish.
static LOWERING_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Storage handle the lowering pass reads vault state from.
///
/// A `Weak` on purpose: the registry must not be the reason a closed vault's
/// handle stays alive. When the daemon drops storage, the next pass observes a
/// dead weak reference and reports the skip rather than resurrecting anything.
static LOWERING_SOURCE: LazyLock<Mutex<Option<Weak<Db>>>> = LazyLock::new(|| Mutex::new(None));

static LOWERING_LAST: LazyLock<Mutex<LoweringPublishReadback>> =
    LazyLock::new(|| Mutex::new(LoweringPublishReadback::default()));

/// Externally readable outcome of the lowering publisher.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoweringPublishReadback {
    pub attempts_total: u64,
    pub success_total: u64,
    pub failure_total: u64,
    pub skipped_total: u64,
    pub last_success_unix_ms: Option<u64>,
    pub last_content_sha256: Option<String>,
    pub last_path: Option<PathBuf>,
    pub last_generation: Option<u64>,
    pub last_source_ledger_seq: Option<u64>,
    pub last_error_code: Option<String>,
    pub last_error: Option<String>,
    /// The last **failure**, retained across later successes.
    ///
    /// `failure_total` is a lifetime counter, so it stays non-zero forever once
    /// a publish fails, while `last_error` is cleared by the next success. That
    /// combination made health report `has 2 failures (last unknown: unknown)`
    /// on the live daemon — a permanent alarm with its own evidence erased
    /// (#1889). These fields are written only by a failure and never cleared.
    pub last_failure_code: Option<String>,
    pub last_failure_detail: Option<String>,
    pub last_failure_unix_ms: Option<u64>,
}

/// Registers the storage handle whose Calyx vault the lowering pass reads.
///
/// Called once by the daemon when it starts the storage maintenance tasks.
/// Re-registration replaces the previous handle, which is what a vault reopen
/// requires.
pub fn register_lowering_source(db: &Arc<Db>) {
    let weak = Arc::downgrade(db);
    match LOWERING_SOURCE.lock() {
        Ok(mut guard) => *guard = Some(weak),
        Err(poisoned) => *poisoned.into_inner() = Some(weak),
    }
    tracing::info!(
        code = "STORAGE_LOWERING_SOURCE_REGISTERED",
        db_path = %db.path.display(),
        "registered the storage handle the off-tick guard-threshold lowering pass reads"
    );
}

/// Current publisher counters and last outcome.
#[must_use]
pub fn lowering_publish_readback() -> LoweringPublishReadback {
    let mut readback = match LOWERING_LAST.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    readback.attempts_total = LOWERING_PUBLISH_ATTEMPTS.load(Ordering::Relaxed);
    readback.success_total = LOWERING_PUBLISH_SUCCESS.load(Ordering::Relaxed);
    readback.failure_total = LOWERING_PUBLISH_FAILURE.load(Ordering::Relaxed);
    readback.skipped_total = LOWERING_PUBLISH_SKIPPED.load(Ordering::Relaxed);
    readback
}

fn record_lowering_skip(code: &'static str, detail: String) {
    LOWERING_PUBLISH_SKIPPED.fetch_add(1, Ordering::Relaxed);
    let mut guard = match LOWERING_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_error_code = Some(code.to_owned());
    guard.last_error = Some(detail);
}

fn record_lowering_failure(code: &'static str, detail: &str) {
    LOWERING_PUBLISH_FAILURE.fetch_add(1, Ordering::Relaxed);
    let failed_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok());
    tracing::error!(
        code,
        detail,
        "off-tick guard-threshold lowering pass failed; the reflex hot path stays on its \
         documented fail-closed defaults until a publish succeeds"
    );
    let mut guard = match LOWERING_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_error_code = Some(code.to_owned());
    guard.last_error = Some(detail.to_owned());
    guard.last_failure_code = Some(code.to_owned());
    guard.last_failure_detail = Some(detail.to_owned());
    guard.last_failure_unix_ms = failed_at_unix_ms;
}

/// Runs one lowering publish, if a vault-backed source is registered.
///
/// Never returns an error: a lowering failure must not turn a successful GC,
/// checkpoint or pressure pass into a failed one. It is recorded in the
/// counters and the structured log instead, and `health` surfaces both.
pub(crate) fn publish_lowered_guard_thresholds() {
    hot_context::assert_cold_calyx("maintenance_lower_guard_thresholds");
    LOWERING_PUBLISH_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    let source = match LOWERING_SOURCE.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(db) = source.as_ref().and_then(Weak::upgrade) else {
        record_lowering_skip(
            "STORAGE_LOWERING_SOURCE_UNREGISTERED",
            "no live storage handle is registered for guard-threshold lowering; call \
             synapse_storage::maintenance::register_lowering_source when storage opens"
                .to_owned(),
        );
        return;
    };
    let status = match db.calyx_vault_status() {
        Ok(status) => status,
        Err(error) => {
            record_lowering_failure(
                "STORAGE_LOWERING_VAULT_STATUS_FAILED",
                &format!("read Calyx vault status for guard-threshold lowering: {error}"),
            );
            return;
        }
    };
    if let Err((code, detail)) = publish_through_vault(&db, &status) {
        record_lowering_failure(code, &detail);
    }
}

/// Decides whether this vault state permits a publish, and where the artifact
/// belongs.
///
/// `Ok(None)` is a recorded skip (a closed or disabled vault has nothing to
/// lower and must not be treated as a failure); `Err` is an open vault that
/// cannot describe itself, which is a real fault.
fn lowering_target_dir(
    status: &SynapseCalyxVaultStatus,
) -> Result<Option<PathBuf>, (&'static str, String)> {
    if !status.open {
        record_lowering_skip(
            "STORAGE_LOWERING_VAULT_NOT_OPEN",
            format!(
                "Calyx vault is not open (enabled={} phase={}); nothing to lower",
                status.enabled, status.phase
            ),
        );
        return Ok(None);
    }
    let Some(vault_dir) = status.vault_dir.as_ref() else {
        return Err((
            "STORAGE_LOWERING_VAULT_STATE_INCOMPLETE",
            "open Calyx vault reported no vault_dir, so the lowered artifact has no home"
                .to_owned(),
        ));
    };
    Ok(Some(vault_dir.clone()))
}

/// Publishes through the vault's own producer and then re-reads the bytes
/// through the consumer.
///
/// #1885: this function does not construct an envelope. `SynapseCalyxVault::
/// lower_guard_thresholds` is the single producer of the frozen payload, its
/// content fingerprint, the vault clock stamp and the atomic publish; this
/// caller only supplies the off-runtime lowering inputs and proves the result
/// through the reader that will actually trust the file.
fn publish_through_vault(
    db: &Db,
    status: &SynapseCalyxVaultStatus,
) -> Result<(), (&'static str, String)> {
    let Some(vault_dir) = lowering_target_dir(status)? else {
        return Ok(());
    };
    let params = LoweringParams {
        generation: LOWERING_GENERATION
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1),
        // The guard-threshold hot set is read straight off the vault tuning
        // config; no panel generation or lens produces it, so claiming
        // producing versions here would fabricate provenance.
        producing_panel_versions: Vec::new(),
        producing_lens_ids: Vec::new(),
        staleness_bound_ms: LOWERED_GUARD_THRESHOLDS_STALENESS_BOUND_MS,
    };
    let report = db.lower_guard_thresholds(&params).map_err(|error| {
        (
            "STORAGE_LOWERING_PUBLISH_FAILED",
            format!("lower Calyx guard thresholds through the vault producer: {error}"),
        )
    })?;
    verify_published_artifact(&vault_dir, &report.path, report.produced_at_unix_ms)?;

    LOWERING_PUBLISH_SUCCESS.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = match LOWERING_LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.last_success_unix_ms = Some(report.produced_at_unix_ms);
        guard.last_content_sha256 = Some(report.content_sha256.clone());
        guard.last_path = Some(report.path.clone());
        guard.last_generation = Some(report.generation);
        guard.last_source_ledger_seq = Some(report.source_ledger_seq);
        guard.last_error_code = None;
        guard.last_error = None;
    }
    tracing::info!(
        code = "STORAGE_LOWERED_ARTIFACT_PUBLISHED",
        kind = %report.kind,
        path = %report.path.display(),
        content_sha256 = %report.content_sha256,
        generation = report.generation,
        source_ledger_seq = report.source_ledger_seq,
        bytes_len = report.bytes_len,
        "re-verified the lowered guard-threshold artifact published by the vault producer"
    );
    Ok(())
}

/// Proves the just-written bytes through the consumer that will actually read
/// them.
///
/// `refresh` re-parses the envelope, re-checks magic/kind/schema, recomputes the
/// SHA-256 over the frozen payload and compares it against the recorded
/// fingerprint. Anything short of `Fresh` means the file on disk is not
/// something a hot path may trust, so the publish is a failure even though the
/// write itself returned success.
fn verify_published_artifact(
    vault_dir: &Path,
    path: &Path,
    now_unix_ms: u64,
) -> Result<(), (&'static str, String)> {
    let verifier = LoweredArtifactHandle::unloaded(vault_dir, LoweredArtifactKind::GuardThresholds);
    let outcome = verifier.refresh(now_unix_ms);
    if outcome.became_fresh {
        return Ok(());
    }
    let reason = outcome.safe_default.map_or_else(
        || "unknown".to_owned(),
        |default| format!("{}: {}", default.code, default.message),
    );
    Err((
        "STORAGE_LOWERING_VERIFY_FAILED",
        format!(
            "published lowered guard-threshold artifact at {} did not read back as fresh through \
             the artifact consumer: {reason}",
            path.display()
        ),
    ))
}
