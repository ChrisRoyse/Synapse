//! Background SST writer for sealed memtables (#1951).
//!
//! ## What this moves, and what it deliberately does not
//!
//! #1949 took the SST write out from under the vault's write locks, so nobody
//! else was blocked by it. It stayed on the **committing thread**, and every
//! MCP tool call pays a durable commit, so a 20-67 ms `sst_write_unlocked`
//! landed on a user-facing request. After #1950 removed the last lock term it
//! was 89-98% of the whole `mvcc` stage — the only one left.
//!
//! This is the consumer for the per-CF pending queues #1949 already built. The
//! seal, the queue, the seal-order install drain, the bounded backlog and the
//! abandon-on-failure path are unchanged; what changes is which thread calls
//! [`SealedFlush::write_sst`].
//!
//! RocksDB puts flushes on a dedicated HIGH-priority pool for exactly this
//! reason — "memtable flushes are in the critical code path where stalling
//! flushes can stall writes and increase latency" ([Thread
//! Pool](https://github.com/facebook/rocksdb/wiki/Thread-Pool)). One dedicated
//! thread here rather than a pool, because the install must not be reordered
//! and a single consumer makes that a property of the construction rather than
//! of the drain logic.
//!
//! ## Durability
//!
//! Deferring the write does **not** widen any recovery window, and that was
//! established before this was written rather than assumed. Router-flush SSTs
//! are excluded from every durability-evidence path: recovery never restores a
//! row from one (`vault/durable/recovery_readback.rs` skips `SstName::Flush`),
//! WAL recycling is gated on the *checkpoint* manifest rather than on flushes,
//! and `router_coverage` treats flush SSTs as the population being validated,
//! never as evidence. `router_flush_durability_window_fsv` deleted **every**
//! flush SST from a vault and still recovered 460/460 keys from the WAL alone.
//! Deferring produces *fewer* flush SSTs at any instant, so it can only make
//! those checks easier to satisfy.
//!
//! ## What it does owe
//!
//! Three obligations, all of which become load-bearing precisely because the
//! write is no longer synchronous:
//!
//! 1. **Read visibility.** A sealed memtable holds the newest state for its
//!    keys until its SST installs, so it must stay in the router read path for
//!    longer than it used to. `RouterShard::read_tables` already serves it;
//!    this only widens the window.
//! 2. **A shutdown drain that fails the readback.** Undrained seals are not
//!    lost — the WAL covers them — but a checkpoint that reports success while
//!    an SST write is outstanding or has *failed* would be claiming a physical
//!    projection that does not exist. [`RouterFlusher::drain`] is called by
//!    `checkpoint`, and it surfaces the first write failure rather than
//!    swallowing it.
//! 3. **Back-pressure.** `MAX_SEALED_MEMTABLES_PER_CF` was a safety net that
//!    should never fire; with a real flusher it is the throttle. It becomes a
//!    bounded *wait* taken with no lock held, and the pre-existing backlog
//!    error is kept as the timeout — the same shape as RocksDB's write stall,
//!    whose `no_slowdown` escape returns `Status::Incomplete` rather than
//!    blocking forever.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::cf::{CfRouter, SealedFlush};
use calyx_core::{CalyxError, Result};

/// How long a commit may wait for flush capacity before failing closed.
///
/// A wait rather than an immediate error is the point (#1951 ask 3), but an
/// unbounded wait would turn a stuck flusher into a hung daemon. At the bound
/// the caller gets the pre-existing backlog error, which names the condition
/// exactly.
const FLUSH_CAPACITY_WAIT_BUDGET: Duration = Duration::from_secs(30);

/// How long [`RouterFlusher::drain`] waits for in-flight writes to finish.
///
/// Generous relative to a single SST write (tens of milliseconds on the
/// deployment host) because a drain that gives up early would report a vault
/// as checkpointed while its projection is still being written.
const DRAIN_BUDGET: Duration = Duration::from_secs(120);

/// Queue state shared by the committing threads and the flusher.
#[derive(Default)]
struct Queue {
    /// Sealed memtables awaiting their SST, in submission order.
    ///
    /// One consumer pops from the front, so submission order **is** write
    /// order, and `install_sealed` then drains its per-CF queue in seal order.
    /// Two independent orderings agreeing is what keeps a newer SST from being
    /// published ahead of an older sibling and inverting read precedence.
    pending: VecDeque<SealedFlush>,
    /// Handles popped but not yet installed. Counted so a drain can tell "the
    /// queue is empty" from "the queue is empty and nothing is mid-write".
    in_flight: usize,
    stop: bool,
    /// The first write or install failure, kept until a drain reports it.
    ///
    /// Kept rather than logged-and-dropped because the committing thread has
    /// already returned by the time this happens: if nothing held the failure,
    /// a checkpoint could report success over a projection that was never
    /// written, which is the silent-loss mode this whole change has to avoid.
    failure: Option<CalyxError>,
}

/// What the background flusher has done, for readback and health (#1951).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushStatus {
    /// SSTs written and installed by the flusher.
    pub written: u64,
    /// Sealed memtables whose background write or install failed.
    pub failed: u64,
    /// Commits that had to wait for flush capacity.
    pub capacity_waits: u64,
    /// Microseconds committing threads spent waiting for capacity.
    pub capacity_wait_us: u64,
    /// Deepest the queue has been.
    pub max_depth: u64,
    /// Submitted but not yet installed, right now.
    pub outstanding: usize,
}

/// Counters readable without taking the queue lock.
#[derive(Debug, Default)]
pub struct FlusherCounters {
    /// SSTs written and installed.
    pub written: AtomicU64,
    /// Sealed memtables whose write or install failed.
    pub failed: AtomicU64,
    /// Commits that had to wait for flush capacity.
    pub capacity_waits: AtomicU64,
    /// Microseconds committing threads spent waiting for capacity.
    pub capacity_wait_us: AtomicU64,
    /// Deepest the pending queue has been.
    pub max_depth: AtomicU64,
}

struct Shared {
    router: Arc<CfRouter>,
    queue: Mutex<Queue>,
    /// Signalled when work arrives or `stop` is set.
    work: Condvar,
    /// Signalled when a handle finishes, for capacity waiters and drains.
    progress: Condvar,
    counters: FlusherCounters,
}

/// Owns the flusher thread and the queue it drains.
pub(crate) struct RouterFlusher {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for RouterFlusher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let depth = self
            .shared
            .queue
            .lock()
            .map(|queue| queue.pending.len() + queue.in_flight)
            .unwrap_or(usize::MAX);
        formatter
            .debug_struct("RouterFlusher")
            .field("outstanding", &depth)
            .finish()
    }
}

impl RouterFlusher {
    /// Starts the flusher thread for `router`.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when the OS refuses the thread. Fails
    /// closed rather than silently writing inline: a vault that quietly kept
    /// paying the SST write on the committing thread would look like this
    /// change had simply not worked.
    pub(crate) fn start(router: Arc<CfRouter>) -> Result<Self> {
        let shared = Arc::new(Shared {
            router,
            queue: Mutex::new(Queue::default()),
            work: Condvar::new(),
            progress: Condvar::new(),
            counters: FlusherCounters::default(),
        });
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("calyx-router-flush".to_owned())
            .spawn(move || run(&worker))
            .map_err(|error| {
                CalyxError::aster_corrupt_shard(format!(
                    "could not start the Calyx router flush thread: {error}"
                ))
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    /// A point-in-time readback of what the flusher has actually done.
    ///
    /// `written` is the number this change has to be judged on: it must equal
    /// the growth in `flush-*.sst` files on disk. Two independent counters that
    /// have to agree is the same check #1948/#1949 used, and it is the only one
    /// that can tell a deferred write from a dropped one.
    pub(crate) fn status(&self) -> FlushStatus {
        let counters = &self.shared.counters;
        FlushStatus {
            written: counters.written.load(Ordering::Relaxed),
            failed: counters.failed.load(Ordering::Relaxed),
            capacity_waits: counters.capacity_waits.load(Ordering::Relaxed),
            capacity_wait_us: counters.capacity_wait_us.load(Ordering::Relaxed),
            max_depth: counters.max_depth.load(Ordering::Relaxed),
            outstanding: self.outstanding(),
        }
    }

    /// Hands sealed memtables to the flusher, oldest first.
    ///
    /// Returns immediately. This is the entire latency win: the caller pays an
    /// enqueue instead of an AEAD-seal-plus-file-write.
    ///
    /// # Errors
    ///
    /// Returns the flusher's first unreported failure, or a corrupt-shard error
    /// when the queue lock is poisoned. Submitting after a failure fails closed
    /// so the vault does not accumulate work behind a broken writer.
    pub(crate) fn submit(&self, sealed: Vec<SealedFlush>) -> Result<()> {
        if sealed.is_empty() {
            return Ok(());
        }
        let mut queue = self
            .shared
            .queue
            .lock()
            .map_err(|_| poisoned("submitting sealed memtables"))?;
        if let Some(failure) = queue.failure.take() {
            return Err(failure);
        }
        if queue.stop {
            return Err(CalyxError::aster_corrupt_shard(
                "the Calyx router flush thread is stopping; a sealed memtable cannot be submitted"
                    .to_owned(),
            ));
        }
        queue.pending.extend(sealed);
        let depth = queue.pending.len() + queue.in_flight;
        self.shared
            .counters
            .max_depth
            .fetch_max(depth as u64, Ordering::Relaxed);
        drop(queue);
        self.shared.work.notify_one();
        Ok(())
    }

    /// Blocks until fewer than `limit` handles are outstanding for `cf`.
    ///
    /// Called with **no lock held**, before the commit takes anything. That
    /// ordering is the whole reason this is a wait rather than the error it
    /// replaces: waiting while holding a shard guard would block the very
    /// flusher whose progress the waiter is waiting for.
    ///
    /// # Errors
    ///
    /// Returns the flusher's first unreported failure, or the backlog error
    /// once [`FLUSH_CAPACITY_WAIT_BUDGET`] elapses.
    pub(crate) fn await_capacity(&self, cf_limit: usize) -> Result<()> {
        let started = Instant::now();
        let mut waited = false;
        let mut queue = self
            .shared
            .queue
            .lock()
            .map_err(|_| poisoned("waiting for flush capacity"))?;
        loop {
            if let Some(failure) = queue.failure.take() {
                return Err(failure);
            }
            let outstanding = queue.pending.len() + queue.in_flight;
            if outstanding < cf_limit || queue.stop {
                break;
            }
            waited = true;
            let remaining = FLUSH_CAPACITY_WAIT_BUDGET.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(CalyxError {
                    code: "CALYX_ASTER_ROUTER_SEALED_MEMTABLE_BACKLOG",
                    message: format!(
                        "{outstanding} sealed memtable(s) are still awaiting their SST after waiting {:?} (cap {cf_limit}); the flusher is not draining",
                        started.elapsed()
                    ),
                    remediation: "inspect CALYX_ASTER_ROUTER_FLUSH_* telemetry and the vault volume for write failures",
                });
            }
            let (next, _) = self
                .shared
                .progress
                .wait_timeout(queue, remaining)
                .map_err(|_| poisoned("waiting for flush capacity"))?;
            queue = next;
        }
        drop(queue);
        if waited {
            self.shared
                .counters
                .capacity_waits
                .fetch_add(1, Ordering::Relaxed);
            self.shared.counters.capacity_wait_us.fetch_add(
                u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        Ok(())
    }

    /// Waits until every submitted handle has been written and installed.
    ///
    /// # Errors
    ///
    /// Returns the flusher's first failure, or a corrupt-shard error if the
    /// queue has not drained within [`DRAIN_BUDGET`]. **Both are fatal to the
    /// caller on purpose**: a checkpoint that swallowed either would be
    /// reporting a durable projection it cannot show.
    pub(crate) fn drain(&self) -> Result<()> {
        let started = Instant::now();
        let mut queue = self
            .shared
            .queue
            .lock()
            .map_err(|_| poisoned("draining sealed memtables"))?;
        while !queue.pending.is_empty() || queue.in_flight > 0 {
            let remaining = DRAIN_BUDGET.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                let outstanding = queue.pending.len() + queue.in_flight;
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "the Calyx router flush queue still holds {outstanding} sealed memtable(s) after {:?}; refusing to report a drained vault",
                    started.elapsed()
                )));
            }
            let (next, _) = self
                .shared
                .progress
                .wait_timeout(queue, remaining)
                .map_err(|_| poisoned("draining sealed memtables"))?;
            queue = next;
        }
        queue.failure.take().map_or(Ok(()), Err)
    }

    /// Outstanding handles: submitted, not yet installed.
    pub(crate) fn outstanding(&self) -> usize {
        self.shared
            .queue
            .lock()
            .map(|queue| queue.pending.len() + queue.in_flight)
            .unwrap_or(0)
    }
}

impl Drop for RouterFlusher {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.stop = true;
        }
        self.shared.work.notify_all();
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!(
                code = "CALYX_ASTER_ROUTER_FLUSH_THREAD_PANICKED",
                "the Calyx router flush thread panicked; sealed memtables it held are covered by the WAL but their SSTs were not written"
            );
        }
    }
}

/// Drains the queue until stopped, writing each sealed memtable's SST with no
/// vault lock held and installing it into the one shard it belongs to.
///
/// On stop it finishes what is already queued rather than discarding it: the
/// rows are WAL-covered either way, but abandoning them would leave the next
/// open replaying more WAL than it needs to, and a coverage readback can see
/// the CF short.
fn run(shared: &Arc<Shared>) {
    loop {
        let Ok(mut queue) = shared.queue.lock() else {
            tracing::error!(
                code = "CALYX_ASTER_ROUTER_FLUSH_QUEUE_POISONED",
                "the Calyx router flush queue lock is poisoned; the flush thread is exiting"
            );
            return;
        };
        while queue.pending.is_empty() {
            if queue.stop {
                return;
            }
            let Ok(next) = shared.work.wait(queue) else {
                return;
            };
            queue = next;
        }
        let Some(handle) = queue.pending.pop_front() else {
            continue;
        };
        queue.in_flight += 1;
        drop(queue);

        // NO LOCK IS HELD HERE, and not the caller's thread either. This is the
        // AEAD seal plus the file write that measured 20-67 ms per commit.
        let outcome = handle
            .write_sst()
            .and_then(|written| shared.router.install_sealed(&handle, written));

        let failure = match outcome {
            Ok(()) => {
                shared.counters.written.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(error) => {
                shared.counters.failed.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    code = "CALYX_ASTER_ROUTER_BACKGROUND_FLUSH_FAILED",
                    cf = handle.cf().name(),
                    rows = handle.row_count(),
                    error_code = error.code,
                    error = %error,
                    "background SST write or install failed; returning the sealed rows to the active memtable and failing the next drain"
                );
                // Returns the rows to the active memtable so the CF is not
                // wedged behind a queue head that can never install. The rows
                // remain WAL-covered throughout, so this is a projection
                // repair rather than a recovery.
                if let Err(abandon) = shared.router.abandon_sealed(&handle, &error) {
                    tracing::error!(
                        code = "CALYX_ASTER_ROUTER_BACKGROUND_FLUSH_ABANDON_FAILED",
                        cf = handle.cf().name(),
                        error_code = abandon.code,
                        error = %abandon,
                        "background flush failed and its sealed memtable could not be restored"
                    );
                    Some(abandon)
                } else {
                    Some(error)
                }
            }
        };

        if let Ok(mut queue) = shared.queue.lock() {
            queue.in_flight -= 1;
            if let Some(error) = failure
                && queue.failure.is_none()
            {
                // First failure wins: it is the one with the original cause.
                queue.failure = Some(error);
            }
        }
        shared.progress.notify_all();
    }
}

fn poisoned(action: &str) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!(
        "the Calyx router flush queue lock is poisoned while {action}"
    ))
}
