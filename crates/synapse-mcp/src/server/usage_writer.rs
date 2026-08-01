//! Dedicated writer for the grounded MCP usage observation (#1936 ask 2).
//!
//! # Why this exists
//!
//! Every tool call publishes one grounded usage observation: the source `Kv`
//! row, the hash-chained `Ledger` entry, the `Base` constellation header, one
//! `Slot` row per lens on `syn-mcp-usage-v1`, the present `Scalars`, and the
//! outcome `Anchors` row — 29 durable rows in one Calyx group commit.
//!
//! That commit was synchronous on the response path. Measured on the deployment
//! host against the live daemon (pid 21648, 2026-08-01 19:34 UTC), for a tool
//! call rejected at parameter validation — a call that does *zero* requested
//! work — the per-call event timeline read out of the daemon's own log was:
//!
//! ```text
//! MCP_TOOL_CALL_ENTERED                   +0.0 ms
//! lifecycle started                       +6.5 ms
//! lifecycle finished                     +11.0 ms
//! CALYX_ASTER_DURABLE_COMMIT_STAGE_TIMINGS +19.6 ms   <- this commit
//! ATOMIC_PUBLICATION_COMMITTED            +1.0 ms
//! next MCP_TOOL_CALL_ENTERED              +6.0 ms
//! ```
//!
//! 44.4 ms in total, against a measured `ping` of 2.65 ms over the identical
//! transport, session and framing. The commit is the single largest term, and
//! its own stage split (`row_count=29`) shows why shrinking fsync would not
//! help — only ~20% of it is the durable write:
//!
//! ```text
//! wal_us 2131-2941   anchor_publish_us 4968-5404   mvcc_us 3769-8293
//! ```
//!
//! Nothing in the caller's response depends on any of it. The observation is
//! bookkeeping *about* the call, not part of its result. The one piece that is
//! genuinely coupled to the response — the steering block attached to the
//! result — was measured at `steering_ms=0` on every call, so it stays
//! synchronous and costs nothing.
//!
//! # What this deliberately does not do
//!
//! - **It does not drop observations, including for rejected calls.** A
//!   rejected call is the minority class of the very outcome the panel exists
//!   to predict; `status_onehot`, `error_onehot` and the `error_present` scalar
//!   carry exactly that case. Dropping them would push the failure rate toward
//!   zero and destroy the grounding of every bits, sufficiency and
//!   capability-card result computed from the corpus.
//! - **It is not fire-and-forget.** A full queue is an error the caller is told
//!   about, not a silently discarded row. An observation that may vanish
//!   without anyone learning of it is not evidence.
//! - **It does not touch `set_nodelay`, the group-commit window, or fsync
//!   policy.** All three were measured and refuted in #1936; the stage split
//!   above reinforces the refutation, since ~80% of the commit is not I/O.
//!
//! # The durability window this does open, stated plainly
//!
//! Before this change the observation was durable before the response was
//! written. Now it is durable shortly after. A hard kill of the daemon inside
//! that window loses the queued observations, where previously it would not
//! have. The window is bounded by the writer's drain rate (one commit, ~10-20
//! ms) and by [`UsageWriterHandle::drain_for_shutdown`], which is called on the
//! ordinary shutdown path and blocks until the queue is empty. It is a real
//! narrowing of a guarantee, not a free win, and it is recorded here rather
//! than in a commit message so the next reader finds it at the code.
//!
//! The hash chain itself is not at risk: `Ledger` entries are appended in
//! writer order, so a lost observation shortens the chain rather than breaking
//! its verification.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use serde_json::Value;
use synapse_storage::{Db, GroundingAnchor};

use super::{ErrorData, mcp_error};
use synapse_core::error_codes;

/// Queue capacity, in observations.
///
/// Sized so that a full queue means the writer is genuinely wedged rather than
/// merely behind: at the measured ~10-20 ms per commit, 1024 entries is ~10-20
/// seconds of backlog. No ordinary burst of agent tool calls reaches it — an
/// agent turn of 100 calls fills 10% of it — so a `Full` is a real fault
/// signal and not a load signal.
const USAGE_QUEUE_CAPACITY: usize = 1024;

/// Depth at which the queue starts warning that it is falling behind, so the
/// backlog is visible well before it becomes an error.
const USAGE_QUEUE_WARN_DEPTH: i64 = (USAGE_QUEUE_CAPACITY as i64) / 4;

/// How long [`UsageWriterHandle::drain_for_shutdown`] waits for the queue to
/// empty before reporting what it could not flush.
const USAGE_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// A commit slower than this is reported individually. The writer is off the
/// response path, so a slow commit no longer costs a caller latency — but it
/// still costs the machine, and an unexplained rise belongs in the log.
const USAGE_WRITER_SLOW_COMMIT_MS: u128 = 50;

/// One grounded usage observation, fully prepared on the calling thread and
/// owning everything the commit needs.
///
/// Prepared eagerly rather than as a closure so that the calling thread keeps
/// every failure it can detect — serialization, anchor construction — as a
/// synchronous error on the call that caused it, and the writer thread is left
/// with exactly one fallible operation: the commit itself.
pub(crate) struct UsageObservation {
    pub(crate) db: Arc<Db>,
    pub(crate) route: String,
    pub(crate) row_key: String,
    pub(crate) encoded: Vec<u8>,
    pub(crate) record_value: Value,
    pub(crate) anchor: GroundingAnchor,
    pub(crate) ledger_payload: Value,
    /// When the observation was handed to the queue, so the writer can report
    /// how long it waited rather than only how long it took to commit.
    pub(crate) queued_at: Instant,
}

/// Counters shared between the enqueueing threads and the writer thread.
#[derive(Debug, Default)]
struct UsageWriterMetrics {
    /// Observations accepted onto the queue but not yet committed. Signed
    /// because it is incremented and decremented from different threads and a
    /// negative value would be a bug worth seeing rather than wrapping.
    depth: AtomicI64,
    committed: AtomicU64,
    failed: AtomicU64,
}

/// Handle to the single usage-writer thread for this daemon.
///
/// Cloned with the service, but the thread and the queue are shared: the
/// `Arc` is the daemon-wide singleton, so a cloned `SynapseService` enqueues
/// onto the same writer rather than starting a second one.
pub(crate) struct UsageWriterHandle {
    sender: SyncSender<UsageObservation>,
    metrics: Arc<UsageWriterMetrics>,
}

impl UsageWriterHandle {
    /// Start the writer thread.
    ///
    /// A dedicated OS thread rather than a Tokio task on purpose: each
    /// observation is a blocking Calyx group commit that fsyncs, and parking a
    /// runtime worker on it for ~10-20 ms would trade this call's latency for
    /// every *other* task's. This host has 2 P-cores and 8 E-cores, so runtime
    /// workers are a scarcer resource than threads.
    pub(crate) fn start() -> Self {
        let (sender, receiver) = sync_channel::<UsageObservation>(USAGE_QUEUE_CAPACITY);
        let metrics = Arc::new(UsageWriterMetrics::default());
        let writer_metrics = Arc::clone(&metrics);
        match std::thread::Builder::new()
            .name("synapse-usage-writer".to_owned())
            .spawn(move || writer_loop(&receiver, &writer_metrics))
        {
            Ok(_handle) => {
                tracing::info!(
                    code = "MCP_USAGE_WRITER_STARTED",
                    capacity = USAGE_QUEUE_CAPACITY as u64,
                    warn_depth = USAGE_QUEUE_WARN_DEPTH,
                    "dedicated grounded-usage writer thread started"
                );
            }
            Err(error) => {
                // A daemon that cannot start its usage writer cannot ground any
                // of the calls it is about to serve. Refusing construction here
                // is the same contract the tool surface already holds: a daemon
                // that starts is one whose evidence path already works.
                panic!("grounded MCP usage writer thread must start: {error}");
            }
        }
        Self { sender, metrics }
    }

    /// Hand one prepared observation to the writer.
    ///
    /// Returns an error, and does not lose the observation, when the queue is
    /// full. That is the fail-closed case: a full queue means the writer has
    /// stopped draining, and a daemon that cannot record what its calls did has
    /// lost the grounding its whole intelligence stack is computed from. It
    /// says so on the call rather than serving evidence-free work quietly.
    pub(crate) fn enqueue(&self, observation: UsageObservation) -> Result<(), ErrorData> {
        let route = observation.route.clone();
        // Claim the slot *before* handing the observation over, and release the
        // claim if the send is refused. Incrementing after a successful
        // `try_send` would let the writer receive, commit and decrement before
        // this thread ever incremented, driving `depth` transiently negative —
        // and `drain_for_shutdown` reads exactly that counter to decide the
        // queue is empty. It would then return "drained" while an observation
        // was still in flight, which is the one outcome this whole module
        // exists to prevent.
        let depth = self.metrics.depth.fetch_add(1, Ordering::AcqRel) + 1;
        match self.sender.try_send(observation) {
            Ok(()) => {
                if depth >= USAGE_QUEUE_WARN_DEPTH {
                    tracing::warn!(
                        code = "MCP_USAGE_WRITER_QUEUE_BACKLOG",
                        route = %route,
                        depth,
                        capacity = USAGE_QUEUE_CAPACITY as u64,
                        warn_depth = USAGE_QUEUE_WARN_DEPTH,
                        "grounded-usage writer is falling behind its enqueue rate"
                    );
                }
                Ok(())
            }
            Err(TrySendError::Full(_observation)) => {
                let depth = self.metrics.depth.fetch_sub(1, Ordering::AcqRel) - 1;
                tracing::error!(
                    code = "MCP_USAGE_WRITER_QUEUE_FULL",
                    route = %route,
                    depth,
                    capacity = USAGE_QUEUE_CAPACITY as u64,
                    committed = self.metrics.committed.load(Ordering::Acquire),
                    failed = self.metrics.failed.load(Ordering::Acquire),
                    "grounded-usage writer queue is full; the writer is not draining"
                );
                Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "MCP_USAGE_WRITER_QUEUE_FULL: route={route} depth={depth} capacity={USAGE_QUEUE_CAPACITY}. \
                         The grounded-usage writer thread has stopped draining, so this call's outcome cannot be \
                         recorded. Inspect MCP_USAGE_WRITER_COMMIT_FAILED events for the underlying storage error."
                    ),
                ))
            }
            Err(TrySendError::Disconnected(_observation)) => {
                self.metrics.depth.fetch_sub(1, Ordering::AcqRel);
                tracing::error!(
                    code = "MCP_USAGE_WRITER_DISCONNECTED",
                    route = %route,
                    committed = self.metrics.committed.load(Ordering::Acquire),
                    failed = self.metrics.failed.load(Ordering::Acquire),
                    "grounded-usage writer thread is gone; usage observations cannot be recorded"
                );
                Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "MCP_USAGE_WRITER_DISCONNECTED: route={route}. The grounded-usage writer thread has \
                         exited, so this call's outcome cannot be recorded. The daemon must be restarted."
                    ),
                ))
            }
        }
    }

    /// Current queue depth. Read by `health` so the backlog is inspectable
    /// without waiting for a threshold log to fire.
    pub(crate) fn depth(&self) -> i64 {
        self.metrics.depth.load(Ordering::Acquire)
    }

    pub(crate) fn committed(&self) -> u64 {
        self.metrics.committed.load(Ordering::Acquire)
    }

    pub(crate) fn failed(&self) -> u64 {
        self.metrics.failed.load(Ordering::Acquire)
    }

    /// Whether the backlog has reached the depth at which the writer is
    /// considered to be falling behind. Shares the one threshold with the
    /// backlog log, so `health` and the log can never disagree about it.
    pub(crate) fn is_backlogged(&self) -> bool {
        self.depth() >= USAGE_QUEUE_WARN_DEPTH
    }

    /// Block until the queue drains, so an orderly shutdown does not discard
    /// observations that were accepted from callers who already got a success.
    ///
    /// Returns the number of observations still queued when it gave up, which
    /// is `0` on a clean drain. Any other value is a real loss and is logged as
    /// one.
    pub(crate) fn drain_for_shutdown(&self) -> i64 {
        let started = Instant::now();
        loop {
            let depth = self.metrics.depth.load(Ordering::Acquire);
            if depth <= 0 {
                tracing::info!(
                    code = "MCP_USAGE_WRITER_DRAINED",
                    committed = self.metrics.committed.load(Ordering::Acquire),
                    failed = self.metrics.failed.load(Ordering::Acquire),
                    waited_ms = started.elapsed().as_millis() as u64,
                    "grounded-usage writer queue drained before shutdown"
                );
                return 0;
            }
            if started.elapsed() >= USAGE_DRAIN_TIMEOUT {
                tracing::error!(
                    code = "MCP_USAGE_WRITER_DRAIN_TIMEOUT",
                    depth,
                    waited_ms = started.elapsed().as_millis() as u64,
                    timeout_ms = USAGE_DRAIN_TIMEOUT.as_millis() as u64,
                    committed = self.metrics.committed.load(Ordering::Acquire),
                    failed = self.metrics.failed.load(Ordering::Acquire),
                    "grounded-usage writer did not drain before shutdown; queued observations are lost"
                );
                return depth;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl std::fmt::Debug for UsageWriterHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsageWriterHandle")
            .field("depth", &self.depth())
            .field("committed", &self.committed())
            .field("failed", &self.failed())
            .finish()
    }
}

/// Drain the queue, committing one observation per iteration.
///
/// Exits only when every sender is dropped, which happens when the last
/// `SynapseService` clone is released at shutdown.
fn writer_loop(receiver: &Receiver<UsageObservation>, metrics: &Arc<UsageWriterMetrics>) {
    while let Ok(observation) = receiver.recv() {
        let queue_wait_ms = observation.queued_at.elapsed().as_millis();
        let commit_started = Instant::now();
        let outcome = commit_observation(&observation);
        let commit_ms = commit_started.elapsed().as_millis();
        // Decrement after the commit, not before, so `depth` counts
        // observations that are not yet durable. A shutdown drain that reads
        // zero has therefore actually written them.
        metrics.depth.fetch_sub(1, Ordering::AcqRel);
        match outcome {
            Ok(committed_seq) => {
                metrics.committed.fetch_add(1, Ordering::AcqRel);
                if commit_ms >= USAGE_WRITER_SLOW_COMMIT_MS {
                    tracing::info!(
                        code = "MCP_USAGE_WRITER_SLOW_COMMIT",
                        route = %observation.route,
                        row_key = %observation.row_key,
                        commit_ms = commit_ms as u64,
                        queue_wait_ms = queue_wait_ms as u64,
                        threshold_ms = USAGE_WRITER_SLOW_COMMIT_MS as u64,
                        "grounded-usage commit exceeded its writer-side budget"
                    );
                } else {
                    tracing::debug!(
                        code = "MCP_USAGE_WRITER_COMMITTED",
                        route = %observation.route,
                        row_key = %observation.row_key,
                        committed_seq,
                        commit_ms = commit_ms as u64,
                        queue_wait_ms = queue_wait_ms as u64,
                        "grounded-usage observation committed off the response path"
                    );
                }
            }
            Err(detail) => {
                metrics.failed.fetch_add(1, Ordering::AcqRel);
                // The caller has already been answered, so this cannot fail a
                // tool call any more. It is a storage-integrity fault and the
                // log is now the only place it can be seen: report the whole
                // identity of the observation so the row can be found and the
                // failure reproduced, never a bare message.
                tracing::error!(
                    code = "MCP_USAGE_WRITER_COMMIT_FAILED",
                    route = %observation.route,
                    row_key = %observation.row_key,
                    value_len_bytes = observation.encoded.len() as u64,
                    commit_ms = commit_ms as u64,
                    queue_wait_ms = queue_wait_ms as u64,
                    failed_total = metrics.failed.load(Ordering::Acquire),
                    detail = %detail,
                    "grounded-usage observation could not be committed; this call's outcome is not in the corpus"
                );
            }
        }
    }
    tracing::info!(
        code = "MCP_USAGE_WRITER_STOPPED",
        committed = metrics.committed.load(Ordering::Acquire),
        failed = metrics.failed.load(Ordering::Acquire),
        "grounded-usage writer thread exited after its last sender was dropped"
    );
}

/// Perform the commit and verify it landed, returning the committed sequence.
///
/// The physical readback checks are the same ones the synchronous path ran.
/// They stay: moving the write off the response path must not also stop
/// checking that the write happened.
fn commit_observation(observation: &UsageObservation) -> Result<u64, String> {
    let source_rows = vec![(
        observation.row_key.as_bytes().to_vec(),
        observation.encoded.clone(),
    )];
    let publication = observation
        .db
        .put_mcp_usage_grounded_publication(
            source_rows,
            observation.row_key.as_bytes(),
            &observation.encoded,
            &observation.record_value,
            observation.anchor.clone(),
            &observation.ledger_payload,
        )
        .map_err(|error| format!("MCP_USAGE_STORAGE_FAILED: {error}"))?;
    if publication.source_row_count != publication.source_readback_exact_match_count {
        return Err(format!(
            "MCP_USAGE_ATOMIC_SOURCE_READBACK_MISMATCH: source_rows={} exact_readbacks={} committed_seq={}",
            publication.source_row_count,
            publication.source_readback_exact_match_count,
            publication.committed_seq
        ));
    }
    let expected_value_len = u64::try_from(observation.encoded.len()).unwrap_or(u64::MAX);
    if publication.source_value_len_bytes != expected_value_len {
        return Err(format!(
            "MCP_USAGE_ATOMIC_PHYSICAL_READBACK_MISMATCH: row_key={} expected_value_len_bytes={expected_value_len} actual_value_len_bytes={}",
            observation.row_key, publication.source_value_len_bytes
        ));
    }
    Ok(publication.committed_seq)
}
