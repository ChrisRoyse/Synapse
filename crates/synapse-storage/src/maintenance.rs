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
//! semaphore so overlapping periodic ticks queue instead of piling onto the
//! blocking pool. This mirrors how mature LSM engines isolate background
//! compaction onto a dedicated, lower-priority thread pool so foreground request
//! latency is unaffected.

use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::{StorageError, StorageResult};

/// Maximum heavy storage-maintenance passes admitted concurrently onto the
/// blocking pool. Kept small on purpose: native-CF compaction and tombstone
/// purge already serialize cross-process on Calyx's native-compaction file lock,
/// so this only needs to keep the periodic GC and disk-pressure loops from
/// stacking long passes onto the blocking pool at once while still letting an
/// urgent pressure pass proceed alongside a routine GC pass.
const MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS: usize = 2;

static STORAGE_MAINTENANCE_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS)));

/// In-flight admitted maintenance passes, published as queue-depth telemetry.
static STORAGE_MAINTENANCE_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

/// Runs one blocking storage-maintenance pass off the async runtime workers.
///
/// The closure executes on Tokio's blocking pool under a dedicated admission
/// permit, so it can never park a runtime worker that is polling MCP requests.
/// Structured telemetry records the admission queue depth, the time spent
/// waiting for a permit, and the execution time of the pass itself.
///
/// # Errors
///
/// Returns the closure's error, or a structured storage error if the admission
/// semaphore was closed or the blocking task failed to join.
pub async fn run_admitted_maintenance<T, F>(
    operation: &'static str,
    work: F,
) -> StorageResult<T>
where
    F: FnOnce() -> StorageResult<T> + Send + 'static,
    T: Send + 'static,
{
    let semaphore = Arc::clone(&STORAGE_MAINTENANCE_PERMITS);
    let waiters_before = MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS
        .saturating_sub(semaphore.available_permits());
    let admission_started = Instant::now();
    let permit = semaphore.acquire_owned().await.map_err(|_closed| {
        StorageError::WriteFailed {
            cf_name: "storage_maintenance".to_owned(),
            detail: format!(
                "{operation}: storage maintenance admission semaphore was unexpectedly closed"
            ),
        }
    })?;
    let admission_wait_ms = u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let in_flight = STORAGE_MAINTENANCE_IN_FLIGHT
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    tracing::info!(
        code = "STORAGE_MAINTENANCE_ADMITTED",
        operation,
        admission_wait_ms,
        already_running = waiters_before as u64,
        in_flight,
        max_concurrent = MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS as u64,
        "admitted storage maintenance onto the dedicated blocking pool off the async runtime workers"
    );
    let exec_started = Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await;
    let exec_ms = u64::try_from(exec_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let remaining = STORAGE_MAINTENANCE_IN_FLIGHT
        .fetch_sub(1, Ordering::AcqRel)
        .saturating_sub(1);
    match joined {
        Ok(result) => {
            tracing::info!(
                code = "STORAGE_MAINTENANCE_COMPLETED",
                operation,
                exec_ms,
                admission_wait_ms,
                in_flight = remaining,
                is_ok = result.is_ok(),
                "completed off-runtime storage maintenance pass"
            );
            result
        }
        Err(join_error) => Err(StorageError::WriteFailed {
            cf_name: "storage_maintenance".to_owned(),
            detail: format!(
                "{operation}: storage maintenance blocking task failed to join: {join_error}"
            ),
        }),
    }
}
