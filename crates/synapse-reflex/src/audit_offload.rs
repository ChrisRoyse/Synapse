//! Off-scheduler-thread persistence for reflex audit rows.
//!
//! # Why this exists (#1802)
//!
//! [`crate::write_audit`] performs two synchronous vault operations: a batched
//! `CF_REFLEX_AUDIT` put and a native Calyx constellation measurement
//! (`Db::put_reflex_audit_constellation` -> `put_observation_constellation`).
//! The measurement is real I/O plus lens math. Every non-nominal scheduler edge
//! (starvation, tick-late, lifetime-expiry, track-lost, recursion clamp,
//! action-denied) used to call it directly from `scheduler_tick::tick`, i.e. on
//! the 1 ms high-resolution scheduler thread — so the system paid an unbounded
//! storage stall precisely when it had already missed a deadline. That is a
//! textbook self-amplifying jitter loop: the audit that records lateness makes
//! the next tick later.
//!
//! # Design
//!
//! Standard real-time practice (audio callbacks, RT logging, `Ellipsis`-style
//! RT auditing): the latency-critical thread never touches I/O and never
//! blocks on a lock shared with an I/O thread. It hands the record to a bounded
//! lock-free queue with a non-blocking `try_send` and returns. A dedicated
//! writer thread — deliberately *not* a runtime worker, mirroring the
//! `run_admitted_maintenance` off-runtime pattern of `3df5fc9c` — drains the
//! queue and performs the vault writes. Because the hand-off is lock-free the
//! scheduler thread can never be priority-inverted behind the writer.
//!
//! # Fail-closed overflow accounting
//!
//! The tick must never block, so a full queue cannot apply backpressure to the
//! producer. It is also never allowed to lose a row *silently*:
//!
//! 1. every rejected row is logged at ERROR with `REFLEX_AUDIT_QUEUE_OVERFLOW`
//!    and its full identity (`reflex_id`, `audit_id`, `ts_ns`, `status`,
//!    `error_code`), so the record's content survives in the log stream;
//! 2. a monotonic `overflowed` counter maintains the accounting invariant
//!    `enqueued + overflowed == offered` and `written + write_failed <=
//!    enqueued`;
//! 3. the writer thread persists a synthetic `__scheduler__` gap row carrying
//!    the exact number of lost records, so the durable audit trail itself is
//!    self-describing rather than just short.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam::channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use serde_json::json;
use synapse_core::{ReflexState, SCHEMA_VERSION, StoredAuditContext, StoredReflexAudit};
use synapse_storage::Db;
use uuid::Uuid;

use crate::write_audit;

/// Bounded depth of the scheduler -> writer hand-off queue.
///
/// Sized so a multi-second vault stall on the writer thread is absorbed
/// without loss at the observed non-nominal edge rate, while keeping the
/// worst-case retained audit backlog bounded and small.
pub const REFLEX_AUDIT_QUEUE_CAPACITY: usize = 4096;

/// Structured error code for a rejected (never-persisted) audit row.
pub const REFLEX_AUDIT_QUEUE_OVERFLOW: &str = "REFLEX_AUDIT_QUEUE_OVERFLOW";

/// Reflex id used for synthetic scheduler-owned audit rows.
const SCHEDULER_REFLEX_ID: &str = "__scheduler__";

/// Kind tag on the synthetic durable gap row.
const REFLEX_AUDIT_QUEUE_OVERFLOW_KIND: &str = "reflex_audit_queue_overflow";

/// Writer wake-up cadence. Bounds how long an overflow gap row or a shutdown
/// can sit unobserved; it is not a latency budget for normal writes, which are
/// delivered by the channel immediately.
const WRITER_POLL_INTERVAL: Duration = Duration::from_millis(250);

const REFLEX_AUDIT_QUEUE_DEPTH_METRIC: &str = "reflex_audit_queue_depth";
const REFLEX_AUDIT_QUEUE_OVERFLOW_METRIC: &str = "reflex_audit_queue_overflow_total";
const REFLEX_AUDIT_WRITE_FAILED_METRIC: &str = "reflex_audit_write_failed_total";

/// One queued audit row plus its durability requirement.
struct AuditMessage {
    audit: StoredReflexAudit,
    /// Forces a vault flush after this row lands. Used for security-relevant
    /// rows (action-denied) that previously flushed inline on the tick.
    flush: bool,
}

/// Monotonic counters describing the hand-off queue.
#[derive(Debug, Default)]
struct ReflexAuditQueueCounters {
    offered: AtomicU64,
    enqueued: AtomicU64,
    overflowed: AtomicU64,
    written: AtomicU64,
    write_failed: AtomicU64,
    flush_failed: AtomicU64,
    high_water_depth: AtomicU64,
}

/// Point-in-time copy of the queue counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReflexAuditQueueSnapshot {
    /// Rows handed to the sink by the scheduler thread.
    pub offered: u64,
    /// Rows accepted into the bounded queue.
    pub enqueued: u64,
    /// Rows rejected because the queue was full or the writer was gone.
    pub overflowed: u64,
    /// Rows durably written by the writer thread.
    pub written: u64,
    /// Rows the writer thread failed to persist.
    pub write_failed: u64,
    /// Post-write flushes that failed.
    pub flush_failed: u64,
    /// Deepest observed queue occupancy.
    pub high_water_depth: u64,
    /// Configured bound of the queue.
    pub capacity: u64,
}

/// Non-blocking audit hand-off owned by the reflex scheduler.
///
/// Cloning is done through [`Arc`]; the writer thread is joined when the last
/// handle is dropped, so every queued row is persisted before the scheduler
/// tears down.
pub struct ReflexAuditSink {
    /// `None` only after [`Drop`] has taken the sender to close the channel.
    tx: Option<Sender<AuditMessage>>,
    counters: Arc<ReflexAuditQueueCounters>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ReflexAuditSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReflexAuditSink")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl ReflexAuditSink {
    /// Starts the dedicated writer thread and returns the sink used by the
    /// scheduler thread.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the writer thread cannot be spawned. The
    /// caller must fail the scheduler spawn rather than silently reverting to
    /// on-tick writes: a scheduler that writes audits inline is the exact
    /// defect this type exists to prevent.
    pub fn start(db: Arc<Db>, audit_context: Option<StoredAuditContext>) -> std::io::Result<Self> {
        let (tx, rx) = bounded::<AuditMessage>(REFLEX_AUDIT_QUEUE_CAPACITY);
        let counters = Arc::new(ReflexAuditQueueCounters::default());
        let worker_counters = Arc::clone(&counters);
        let worker = thread::Builder::new()
            .name("synapse-reflex-audit-writer".to_owned())
            .spawn(move || run_writer(&db, &rx, &worker_counters, audit_context.as_ref()))
            .inspect_err(|error| {
                tracing::error!(
                    code = "REFLEX_AUDIT_WRITER_SPAWN_FAILED",
                    detail = %error,
                    "could not spawn the reflex audit writer thread; the reflex scheduler must not start because audit rows would otherwise be written on the high-resolution tick thread"
                );
            })?;
        Ok(Self {
            tx: Some(tx),
            counters,
            worker: Some(worker),
        })
    }

    /// Hands one audit row to the writer thread. Never blocks, never performs
    /// I/O, never allocates a lock shared with the writer.
    pub fn enqueue(&self, audit: StoredReflexAudit) {
        self.offer(audit, false);
    }

    /// Hands one audit row to the writer thread and requests a vault flush once
    /// it lands.
    pub fn enqueue_flushing(&self, audit: StoredReflexAudit) {
        self.offer(audit, true);
    }

    fn offer(&self, audit: StoredReflexAudit, flush: bool) {
        self.counters.offered.fetch_add(1, Ordering::Relaxed);
        let Some(tx) = self.tx.as_ref() else {
            self.record_overflow(&audit, "sink_closed");
            return;
        };
        match tx.try_send(AuditMessage { audit, flush }) {
            Ok(()) => {
                self.counters.enqueued.fetch_add(1, Ordering::Relaxed);
                let depth = tx.len() as u64;
                self.counters
                    .high_water_depth
                    .fetch_max(depth, Ordering::Relaxed);
            }
            Err(TrySendError::Full(message)) => {
                self.record_overflow(&message.audit, "queue_full");
            }
            Err(TrySendError::Disconnected(message)) => {
                self.record_overflow(&message.audit, "writer_thread_gone");
            }
        }
    }

    /// Records a row that will never reach the vault. Logs the complete record
    /// identity so the loss is recoverable from the log stream, and bumps the
    /// counter the writer thread turns into a durable gap row.
    fn record_overflow(&self, audit: &StoredReflexAudit, reason: &'static str) {
        let overflowed = self
            .counters
            .overflowed
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        tracing::error!(
            code = REFLEX_AUDIT_QUEUE_OVERFLOW,
            component = "reflex_audit_offload",
            reason,
            reflex_id = %audit.reflex_id,
            audit_id = %audit.audit_id,
            ts_ns = audit.ts_ns,
            status = ?audit.status,
            error_code = audit.error_code.as_deref().unwrap_or("<none>"),
            capacity = REFLEX_AUDIT_QUEUE_CAPACITY,
            overflowed_total = overflowed,
            details = %audit.details,
            remediation = "the reflex audit writer thread is not draining fast enough (vault stall or storage backpressure); inspect CALYX/storage maintenance logs — this row was NOT persisted and exists only in this log line plus the durable REFLEX_AUDIT_QUEUE_OVERFLOW gap row",
            "reflex audit row was rejected by the bounded off-thread audit queue and was not persisted"
        );
    }

    /// Reads the accounting counters.
    #[must_use]
    pub fn snapshot(&self) -> ReflexAuditQueueSnapshot {
        ReflexAuditQueueSnapshot {
            offered: self.counters.offered.load(Ordering::Relaxed),
            enqueued: self.counters.enqueued.load(Ordering::Relaxed),
            overflowed: self.counters.overflowed.load(Ordering::Relaxed),
            written: self.counters.written.load(Ordering::Relaxed),
            write_failed: self.counters.write_failed.load(Ordering::Relaxed),
            flush_failed: self.counters.flush_failed.load(Ordering::Relaxed),
            high_water_depth: self.counters.high_water_depth.load(Ordering::Relaxed),
            capacity: REFLEX_AUDIT_QUEUE_CAPACITY as u64,
        }
    }
}

impl Drop for ReflexAuditSink {
    fn drop(&mut self) {
        // Closing the sender lets the writer drain the remainder and exit.
        drop(self.tx.take());
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!(
                code = "REFLEX_AUDIT_WRITER_PANICKED",
                component = "reflex_audit_offload",
                "the reflex audit writer thread panicked; queued audit rows may not have been persisted"
            );
        }
        let snapshot = self.snapshot();
        tracing::info!(
            code = "REFLEX_AUDIT_QUEUE_SHUTDOWN",
            component = "reflex_audit_offload",
            offered = snapshot.offered,
            enqueued = snapshot.enqueued,
            overflowed = snapshot.overflowed,
            written = snapshot.written,
            write_failed = snapshot.write_failed,
            flush_failed = snapshot.flush_failed,
            high_water_depth = snapshot.high_water_depth,
            capacity = snapshot.capacity,
            "reflex audit offload queue drained and shut down"
        );
    }
}

fn run_writer(
    db: &Db,
    rx: &Receiver<AuditMessage>,
    counters: &ReflexAuditQueueCounters,
    audit_context: Option<&StoredAuditContext>,
) {
    let mut recorded_overflow = 0_u64;
    loop {
        match rx.recv_timeout(WRITER_POLL_INTERVAL) {
            Ok(message) => write_one(db, &message, counters),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // crossbeam reports `Disconnected` only once the queue is
                // empty, so every accepted row has already been written.
                record_overflow_gap(db, counters, audit_context, &mut recorded_overflow);
                return;
            }
        }
        publish_queue_metrics(rx, counters);
        record_overflow_gap(db, counters, audit_context, &mut recorded_overflow);
    }
}

fn write_one(db: &Db, message: &AuditMessage, counters: &ReflexAuditQueueCounters) {
    match write_audit(db, &message.audit) {
        Ok(()) => {
            counters.written.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            counters.write_failed.fetch_add(1, Ordering::Relaxed);
            metrics::counter!(REFLEX_AUDIT_WRITE_FAILED_METRIC).increment(1);
            tracing::error!(
                code = "REFLEX_AUDIT_WRITE_FAILED",
                component = "reflex_audit_offload",
                reflex_id = %message.audit.reflex_id,
                audit_id = %message.audit.audit_id,
                ts_ns = message.audit.ts_ns,
                detail = %format!("{error:#}"),
                "off-thread reflex audit write failed; the row was not persisted"
            );
            return;
        }
    }
    if message.flush
        && let Err(error) = db.flush()
    {
        counters.flush_failed.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            code = "REFLEX_AUDIT_FLUSH_FAILED",
            component = "reflex_audit_offload",
            reflex_id = %message.audit.reflex_id,
            audit_id = %message.audit.audit_id,
            detail = %format!("{error:#}"),
            "off-thread reflex audit flush failed after a durability-critical audit row"
        );
    }
}

fn publish_queue_metrics(rx: &Receiver<AuditMessage>, counters: &ReflexAuditQueueCounters) {
    // Metrics are emitted from the writer thread only; the scheduler thread
    // touches nothing but relaxed atomics and a lock-free try_send.
    let depth = u32::try_from(rx.len()).unwrap_or(u32::MAX);
    let overflowed = u32::try_from(counters.overflowed.load(Ordering::Relaxed)).unwrap_or(u32::MAX);
    metrics::gauge!(REFLEX_AUDIT_QUEUE_DEPTH_METRIC).set(f64::from(depth));
    metrics::gauge!(REFLEX_AUDIT_QUEUE_OVERFLOW_METRIC).set(f64::from(overflowed));
}

/// Persists a synthetic row describing rows that were rejected by the bounded
/// queue, so the durable trail records the gap instead of being silently short.
fn record_overflow_gap(
    db: &Db,
    counters: &ReflexAuditQueueCounters,
    audit_context: Option<&StoredAuditContext>,
    recorded_overflow: &mut u64,
) {
    let overflowed = counters.overflowed.load(Ordering::Relaxed);
    let lost = overflowed.saturating_sub(*recorded_overflow);
    if lost == 0 {
        return;
    }
    let Some(ts_ns) = crate::audit_timestamp::try_now_unix_ns(REFLEX_AUDIT_QUEUE_OVERFLOW_KIND)
    else {
        return;
    };
    let audit = StoredReflexAudit {
        schema_version: SCHEMA_VERSION,
        audit_id: Uuid::now_v7().to_string(),
        reflex_id: SCHEDULER_REFLEX_ID.to_owned(),
        ts_ns,
        status: ReflexState::Active,
        event_id: None,
        audit_context: audit_context.cloned(),
        steps: Vec::new(),
        error_code: Some(REFLEX_AUDIT_QUEUE_OVERFLOW.to_owned()),
        details: json!({
            "kind": REFLEX_AUDIT_QUEUE_OVERFLOW_KIND,
            "lost_rows": lost,
            "lost_rows_total": overflowed,
            "queue_capacity": REFLEX_AUDIT_QUEUE_CAPACITY,
            "reason": "bounded reflex audit hand-off queue rejected rows; the scheduler tick is never allowed to block on the vault",
            "recovery": "the full identity of each lost row was logged at ERROR with code REFLEX_AUDIT_QUEUE_OVERFLOW",
        }),
        redacted: false,
        redactions: Vec::new(),
    };
    match write_audit(db, &audit) {
        Ok(()) => {
            *recorded_overflow = overflowed;
            tracing::error!(
                code = REFLEX_AUDIT_QUEUE_OVERFLOW,
                component = "reflex_audit_offload",
                audit_id = %audit.audit_id,
                lost_rows = lost,
                lost_rows_total = overflowed,
                "persisted a durable reflex audit gap row for rows the bounded queue rejected"
            );
        }
        Err(error) => {
            // Leave `recorded_overflow` behind so the gap row is retried on the
            // next writer wake-up instead of being lost.
            tracing::error!(
                code = "REFLEX_AUDIT_GAP_ROW_WRITE_FAILED",
                component = "reflex_audit_offload",
                lost_rows = lost,
                detail = %format!("{error:#}"),
                "could not persist the reflex audit gap row; it will be retried"
            );
        }
    }
}
