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
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam::channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use serde_json::json;
use synapse_core::{
    ReflexState, ReflexStatus, SCHEMA_VERSION, StoredAuditContext, StoredReflexAudit,
};
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
const REFLEX_TERMINAL_COMMIT_FAILED_METRIC: &str = "reflex_terminal_commit_failed_total";

const TERMINAL_PREPARE_QUEUED: u8 = 1;
const TERMINAL_PREPARED: u8 = 2;
const TERMINAL_COMPLETION_QUEUED: u8 = 3;
const TERMINAL_COMPLETED: u8 = 4;
const TERMINAL_FAILED: u8 = 5;

/// One queued audit row plus its durability requirement.
enum AuditMessage {
    Telemetry {
        audit: Box<StoredReflexAudit>,
        /// Forces a vault flush after this row lands.
        flush: bool,
    },
    TerminalPrepare(TerminalLifecycleToken),
    TerminalComplete(TerminalLifecycleToken),
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalLifecycleToken {
    inner: Arc<TerminalLifecycleTokenInner>,
}

#[derive(Debug)]
struct TerminalLifecycleTokenInner {
    intent_id: String,
    final_status: ReflexStatus,
    final_audit: StoredReflexAudit,
    statuses: Arc<Mutex<Vec<ReflexStatus>>>,
    state: AtomicU8,
}

impl TerminalLifecycleToken {
    #[must_use]
    pub(crate) fn reflex_id(&self) -> &str {
        &self.inner.final_status.id
    }

    #[must_use]
    pub(crate) fn is_completed(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == TERMINAL_COMPLETED
    }
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
    terminal_pending: AtomicU64,
    terminal_prepared: AtomicU64,
    terminal_committed: AtomicU64,
    terminal_failed: AtomicU64,
    terminal_last_failure: Mutex<Option<TerminalLifecycleFailure>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalLifecycleFailure {
    reflex_id: String,
    intent_id: String,
    phase: String,
    detail: String,
}

/// Point-in-time copy of the queue counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
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
    pub terminal_pending: u64,
    pub terminal_prepared: u64,
    pub terminal_committed: u64,
    pub terminal_failed: u64,
    pub terminal_failure_reflex_id: Option<String>,
    pub terminal_failure_intent_id: Option<String>,
    pub terminal_failure_phase: Option<String>,
    pub terminal_failure_detail: Option<String>,
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
        self.offer_telemetry(audit, false);
    }

    /// Hands one audit row to the writer thread and requests a vault flush once
    /// it lands.
    pub fn enqueue_flushing(&self, audit: StoredReflexAudit) {
        self.offer_telemetry(audit, true);
    }

    fn offer_telemetry(&self, audit: StoredReflexAudit, flush: bool) {
        self.counters.offered.fetch_add(1, Ordering::Relaxed);
        if matches!(
            audit.status,
            ReflexState::Expired | ReflexState::ActionDenied
        ) {
            self.counters.write_failed.fetch_add(1, Ordering::Relaxed);
            metrics::counter!(REFLEX_AUDIT_WRITE_FAILED_METRIC).increment(1);
            tracing::error!(
                code = "REFLEX_TERMINAL_AUDIT_TELEMETRY_PATH_REFUSED",
                component = "reflex_audit_offload",
                reflex_id = %audit.reflex_id,
                audit_id = %audit.audit_id,
                terminal_state = ?audit.status,
                remediation = "route this exact transition through prepare_terminal so desired state, readback acknowledgement, and public status share one lifecycle protocol",
                "refused a terminal audit on the unacknowledged telemetry path"
            );
            return;
        }
        let Some(tx) = self.tx.as_ref() else {
            self.record_overflow(&audit, "sink_closed");
            return;
        };
        match tx.try_send(AuditMessage::Telemetry {
            audit: Box::new(audit),
            flush,
        }) {
            Ok(()) => {
                self.record_enqueued(tx);
            }
            Err(TrySendError::Full(AuditMessage::Telemetry { audit, .. })) => {
                self.record_overflow(&audit, "queue_full");
            }
            Err(TrySendError::Disconnected(AuditMessage::Telemetry { audit, .. })) => {
                self.record_overflow(&audit, "writer_thread_gone");
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                unreachable!("offer_telemetry submitted only the telemetry AuditMessage variant")
            }
        }
    }

    pub(crate) fn prepare_terminal(
        &self,
        mut final_audit: StoredReflexAudit,
        final_status: ReflexStatus,
        statuses: Arc<Mutex<Vec<ReflexStatus>>>,
    ) -> TerminalLifecycleToken {
        let intent_id = final_audit.audit_id.clone();
        final_audit.details["lifecycle_intent_id"] = serde_json::Value::String(intent_id.clone());
        final_audit.details["lifecycle_phase"] = serde_json::Value::String("completion".to_owned());
        let token = TerminalLifecycleToken {
            inner: Arc::new(TerminalLifecycleTokenInner {
                intent_id,
                final_status,
                final_audit,
                statuses,
                state: AtomicU8::new(TERMINAL_PREPARE_QUEUED),
            }),
        };
        self.counters
            .terminal_pending
            .fetch_add(1, Ordering::Relaxed);
        self.offer_terminal(
            AuditMessage::TerminalPrepare(token.clone()),
            &token,
            "prepare_queue",
        );
        token
    }

    pub(crate) fn advance_terminal(&self, token: &TerminalLifecycleToken) {
        if token
            .inner
            .state
            .compare_exchange(
                TERMINAL_PREPARED,
                TERMINAL_COMPLETION_QUEUED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        self.offer_terminal(
            AuditMessage::TerminalComplete(token.clone()),
            token,
            "completion_queue",
        );
    }

    fn offer_terminal(
        &self,
        message: AuditMessage,
        token: &TerminalLifecycleToken,
        phase: &'static str,
    ) {
        self.counters.offered.fetch_add(1, Ordering::Relaxed);
        let Some(tx) = self.tx.as_ref() else {
            self.record_terminal_failure(token, phase, "audit sink is closed");
            return;
        };
        match tx.try_send(message) {
            Ok(()) => self.record_enqueued(tx),
            Err(TrySendError::Full(_)) => self.record_terminal_failure(
                token,
                phase,
                "bounded reflex lifecycle queue is full; the terminal request was not accepted",
            ),
            Err(TrySendError::Disconnected(_)) => self.record_terminal_failure(
                token,
                phase,
                "reflex lifecycle writer thread is disconnected; the terminal request was not accepted",
            ),
        }
    }

    fn record_enqueued(&self, tx: &Sender<AuditMessage>) {
        self.counters.enqueued.fetch_add(1, Ordering::Relaxed);
        let depth = tx.len() as u64;
        self.counters
            .high_water_depth
            .fetch_max(depth, Ordering::Relaxed);
    }

    fn record_terminal_failure(
        &self,
        token: &TerminalLifecycleToken,
        phase: &str,
        detail: impl Into<String>,
    ) {
        let detail = detail.into();
        if token.inner.state.swap(TERMINAL_FAILED, Ordering::AcqRel) != TERMINAL_FAILED {
            self.counters
                .terminal_failed
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!(REFLEX_TERMINAL_COMMIT_FAILED_METRIC).increment(1);
        }
        let failure = TerminalLifecycleFailure {
            reflex_id: token.inner.final_status.id.clone(),
            intent_id: token.inner.intent_id.clone(),
            phase: phase.to_owned(),
            detail: detail.clone(),
        };
        match self.counters.terminal_last_failure.lock() {
            Ok(mut last) => *last = Some(failure),
            Err(poisoned) => *poisoned.into_inner() = Some(failure),
        }
        tracing::error!(
            code = "REFLEX_TERMINAL_LIFECYCLE_FAILED",
            component = "reflex_audit_offload",
            phase,
            reflex_id = %token.inner.final_status.id,
            intent_id = %token.inner.intent_id,
            detail,
            remediation = "keep this reflex non-dispatchable, preserve the vault and logs, repair the named queue/storage/readback failure, then restart so durable terminal-intent recovery can finish any prepared transition",
            "reflex terminal lifecycle transition failed closed"
        );
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
        let last_failure = match self.counters.terminal_last_failure.lock() {
            Ok(last) => last.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        ReflexAuditQueueSnapshot {
            offered: self.counters.offered.load(Ordering::Relaxed),
            enqueued: self.counters.enqueued.load(Ordering::Relaxed),
            overflowed: self.counters.overflowed.load(Ordering::Relaxed),
            written: self.counters.written.load(Ordering::Relaxed),
            write_failed: self.counters.write_failed.load(Ordering::Relaxed),
            flush_failed: self.counters.flush_failed.load(Ordering::Relaxed),
            high_water_depth: self.counters.high_water_depth.load(Ordering::Relaxed),
            capacity: REFLEX_AUDIT_QUEUE_CAPACITY as u64,
            terminal_pending: self.counters.terminal_pending.load(Ordering::Relaxed),
            terminal_prepared: self.counters.terminal_prepared.load(Ordering::Relaxed),
            terminal_committed: self.counters.terminal_committed.load(Ordering::Relaxed),
            terminal_failed: self.counters.terminal_failed.load(Ordering::Relaxed),
            terminal_failure_reflex_id: last_failure
                .as_ref()
                .map(|failure| failure.reflex_id.clone()),
            terminal_failure_intent_id: last_failure
                .as_ref()
                .map(|failure| failure.intent_id.clone()),
            terminal_failure_phase: last_failure.as_ref().map(|failure| failure.phase.clone()),
            terminal_failure_detail: last_failure.map(|failure| failure.detail),
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
            terminal_pending = snapshot.terminal_pending,
            terminal_prepared = snapshot.terminal_prepared,
            terminal_committed = snapshot.terminal_committed,
            terminal_failed = snapshot.terminal_failed,
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
            Ok(message) => write_one(db, message, counters),
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

fn write_one(db: &Db, message: AuditMessage, counters: &ReflexAuditQueueCounters) {
    match message {
        AuditMessage::Telemetry { audit, flush } => write_telemetry(db, &audit, flush, counters),
        AuditMessage::TerminalPrepare(token) => prepare_terminal(db, &token, counters),
        AuditMessage::TerminalComplete(token) => complete_terminal(db, &token, counters),
    }
}

fn write_telemetry(
    db: &Db,
    audit: &StoredReflexAudit,
    flush: bool,
    counters: &ReflexAuditQueueCounters,
) {
    match write_audit(db, audit) {
        Ok(()) => {
            counters.written.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            counters.write_failed.fetch_add(1, Ordering::Relaxed);
            metrics::counter!(REFLEX_AUDIT_WRITE_FAILED_METRIC).increment(1);
            tracing::error!(
                code = "REFLEX_AUDIT_WRITE_FAILED",
                component = "reflex_audit_offload",
                reflex_id = %audit.reflex_id,
                audit_id = %audit.audit_id,
                ts_ns = audit.ts_ns,
                detail = %format!("{error:#}"),
                "off-thread reflex audit write failed; the row was not persisted"
            );
            return;
        }
    }
    if flush && let Err(error) = db.flush() {
        counters.flush_failed.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            code = "REFLEX_AUDIT_FLUSH_FAILED",
            component = "reflex_audit_offload",
            reflex_id = %audit.reflex_id,
            audit_id = %audit.audit_id,
            detail = %format!("{error:#}"),
            "off-thread reflex audit flush failed after a durability-critical audit row"
        );
    }
}

fn prepare_terminal(db: &Db, token: &TerminalLifecycleToken, counters: &ReflexAuditQueueCounters) {
    let result = (|| -> Result<(), String> {
        let prior_record = crate::durable_state::load_record(db, &token.inner.final_status.id)
            .map_err(|error| error.to_string())?;
        if prior_record.terminal_intent.is_some() {
            return Err(format!(
                "REFLEX_TERMINAL_INTENT_ALREADY_PREPARED: reflex_id={} requested_intent_id={}; remediation=do not overwrite the existing intent; restart to run exact durable recovery",
                token.inner.final_status.id, token.inner.intent_id
            ));
        }
        let prepare_ts = token.inner.final_audit.ts_ns.checked_sub(1).ok_or_else(|| {
            "REFLEX_TERMINAL_INTENT_TIMESTAMP_INVALID: terminal audit timestamp cannot reserve an immediately preceding prepare timestamp; remediation=repair the timestamp source before restarting terminal reconciliation".to_owned()
        })?;
        let terminal_intent = crate::durable_state::DurableTerminalIntent {
            intent_id: token.inner.intent_id.clone(),
            prepared_at_ns: prepare_ts,
            terminal_status: token.inner.final_status.clone(),
            terminal_audit: token.inner.final_audit.clone(),
        };
        let prepared_record = prior_record
            .with_terminal_intent(terminal_intent)
            .map_err(|error| error.to_string())?;
        let prepare_audit = StoredReflexAudit {
            schema_version: SCHEMA_VERSION,
            audit_id: Uuid::now_v7().to_string(),
            reflex_id: token.inner.final_status.id.clone(),
            ts_ns: prepare_ts,
            status: ReflexState::Active,
            event_id: token.inner.final_audit.event_id.clone(),
            audit_context: token.inner.final_audit.audit_context.clone(),
            steps: Vec::new(),
            error_code: None,
            details: json!({
                "kind": "reflex_terminal_lifecycle_intent_prepared",
                "lifecycle_intent_id": token.inner.intent_id,
                "lifecycle_phase": "prepared",
                "terminal_state": token.inner.final_status.state,
                "terminal_audit_id": token.inner.final_audit.audit_id,
                "terminal_audit_sha256": synapse_storage::ordered_index::sha256_hex(
                    &synapse_storage::encode_json(&token.inner.final_audit)
                        .map_err(|error| error.to_string())?
                ),
            }),
            redacted: false,
            redactions: Vec::new(),
        };
        crate::audit::write_terminal_lifecycle_intent(
            db,
            &prepare_audit,
            &prior_record,
            &prepared_record,
        )
        .map_err(|error| error.to_string())?;
        db.flush().map_err(|error| error.to_string())?;
        crate::audit::verify_terminal_lifecycle_readback(
            db,
            &prepare_audit,
            &prepared_record,
            "prepare",
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            counters.written.fetch_add(1, Ordering::Relaxed);
            counters.terminal_prepared.fetch_add(1, Ordering::Relaxed);
            token
                .inner
                .state
                .store(TERMINAL_PREPARED, Ordering::Release);
            tracing::info!(
                code = "REFLEX_TERMINAL_INTENT_PREPARED",
                reflex_id = %token.inner.final_status.id,
                intent_id = %token.inner.intent_id,
                terminal_state = ?token.inner.final_status.state,
                "durably prepared and independently read back a reflex terminal intent"
            );
        }
        Err(error) => record_terminal_failure(counters, token, "prepare_commit_or_readback", error),
    }
}

fn complete_terminal(db: &Db, token: &TerminalLifecycleToken, counters: &ReflexAuditQueueCounters) {
    let result = (|| -> Result<(), String> {
        let prior_record = crate::durable_state::load_record(db, &token.inner.final_status.id)
            .map_err(|error| error.to_string())?;
        let intent = prior_record.terminal_intent.as_ref().ok_or_else(|| {
            format!(
                "REFLEX_TERMINAL_INTENT_MISSING_AT_COMPLETION: reflex_id={} expected_intent_id={}; remediation=keep the reflex non-dispatchable and inspect the exact desired-state row",
                token.inner.final_status.id, token.inner.intent_id
            )
        })?;
        if intent.intent_id != token.inner.intent_id
            || intent.terminal_status != token.inner.final_status
            || intent.terminal_audit != token.inner.final_audit
        {
            return Err(format!(
                "REFLEX_TERMINAL_INTENT_CHANGED_AT_COMPLETION: reflex_id={} expected_intent_id={} actual_intent_id={}; remediation=keep the reflex non-dispatchable and reconcile the competing lifecycle writer",
                token.inner.final_status.id, token.inner.intent_id, intent.intent_id
            ));
        }
        let next_record = prior_record
            .with_status(token.inner.final_status.clone())
            .map_err(|error| error.to_string())?;
        crate::audit::write_terminal_lifecycle_audit(
            db,
            &token.inner.final_audit,
            &prior_record,
            &next_record,
        )
        .map_err(|error| error.to_string())?;
        db.flush().map_err(|error| error.to_string())?;
        crate::audit::verify_terminal_lifecycle_readback(
            db,
            &token.inner.final_audit,
            &next_record,
            "completion",
        )
        .map_err(|error| error.to_string())?;
        match token.inner.statuses.lock() {
            Ok(mut statuses) => publish_terminal_status(&mut statuses, token)?,
            Err(_) => {
                return Err(format!(
                    "REFLEX_TERMINAL_STATUS_LOCK_POISONED_AFTER_COMMIT: reflex_id={} intent_id={}; remediation=restart the scheduler; durable state is terminal and will not replay",
                    token.inner.final_status.id, token.inner.intent_id
                ));
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            counters.written.fetch_add(1, Ordering::Relaxed);
            counters.terminal_committed.fetch_add(1, Ordering::Relaxed);
            counters.terminal_pending.fetch_sub(1, Ordering::Relaxed);
            token
                .inner
                .state
                .store(TERMINAL_COMPLETED, Ordering::Release);
            tracing::info!(
                code = "REFLEX_TERMINAL_LIFECYCLE_COMMITTED",
                reflex_id = %token.inner.final_status.id,
                intent_id = %token.inner.intent_id,
                terminal_state = ?token.inner.final_status.state,
                "durably committed and independently read back a reflex terminal transition before publishing runtime status"
            );
        }
        Err(error) => record_terminal_failure(
            counters,
            token,
            "completion_commit_readback_or_publish",
            error,
        ),
    }
}

fn publish_terminal_status(
    statuses: &mut [ReflexStatus],
    token: &TerminalLifecycleToken,
) -> Result<(), String> {
    let status = statuses
        .iter_mut()
        .find(|status| status.id == token.inner.final_status.id)
        .ok_or_else(|| {
            format!(
                "REFLEX_TERMINAL_STATUS_PUBLICATION_TARGET_MISSING: reflex_id={} intent_id={}; remediation=restart the scheduler generation from durable terminal state",
                token.inner.final_status.id, token.inner.intent_id
            )
        })?;
    if matches!(
        status.state,
        ReflexState::Expired | ReflexState::ActionDenied | ReflexState::Cancelled
    ) {
        return Err(format!(
            "REFLEX_TERMINAL_STATUS_PUBLISHED_BEFORE_ACK: reflex_id={} intent_id={} current_state={:?}; remediation=inspect all runtime status writers and restart from durable terminal state",
            token.inner.final_status.id, token.inner.intent_id, status.state
        ));
    }
    *status = token.inner.final_status.clone();
    Ok(())
}

fn record_terminal_failure(
    counters: &ReflexAuditQueueCounters,
    token: &TerminalLifecycleToken,
    phase: &str,
    detail: impl Into<String>,
) {
    let detail = detail.into();
    if token.inner.state.swap(TERMINAL_FAILED, Ordering::AcqRel) != TERMINAL_FAILED {
        counters.terminal_failed.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(REFLEX_TERMINAL_COMMIT_FAILED_METRIC).increment(1);
    }
    let failure = TerminalLifecycleFailure {
        reflex_id: token.inner.final_status.id.clone(),
        intent_id: token.inner.intent_id.clone(),
        phase: phase.to_owned(),
        detail: detail.clone(),
    };
    match counters.terminal_last_failure.lock() {
        Ok(mut last) => *last = Some(failure),
        Err(poisoned) => *poisoned.into_inner() = Some(failure),
    }
    tracing::error!(
        code = "REFLEX_TERMINAL_LIFECYCLE_FAILED",
        component = "reflex_audit_offload",
        phase,
        reflex_id = %token.inner.final_status.id,
        intent_id = %token.inner.intent_id,
        detail,
        remediation = "keep this reflex non-dispatchable, preserve the vault and logs, repair the named queue/storage/readback failure, then restart so durable terminal-intent recovery can finish any prepared transition",
        "reflex terminal lifecycle transition failed closed"
    );
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
