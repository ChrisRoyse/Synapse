use super::{AsterVault, encode, raw_commitment};
use calyx_core::{CalyxError, Clock, Result, Seq};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The WAL append is durable, but the live MVCC/router apply failed and the
/// caller must reconcile the reported sequence before retrying.
pub const CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED: &str =
    "CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED";

/// Wait or hold above which one durable-commit-lock acquisition is reported as
/// a stall (issue #1806).
///
/// The durable commit lock serializes every vault write, so a hold beyond this
/// budget is directly visible to callers as a stalled MCP request. Before this
/// telemetry existed a 469 s hold could only be inferred from third-party
/// symptoms; the warning below names the exact call site responsible.
const DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS: u128 = 1_000;

/// Total durable-commit duration above which the per-stage split is logged
/// (issue #1936).
///
/// Every MCP tool call pays exactly one grounded-observation commit before its
/// response returns, so this is directly on the caller's critical path. A quiet
/// log is the evidence the commit is in budget; anything slower names which
/// stage spent the time rather than leaving one opaque `commit_us`.
const COMMIT_STAGE_SLOW_BUDGET_US: u64 = 3_000;

/// Per-stage split of one durable group commit.
///
/// Each field is the time spent in that stage alone, not a cumulative offset,
/// so the fields sum to `total_us` minus the unattributed remainder — and a
/// remainder that is not near zero is itself the finding.
#[derive(Default)]
struct CommitStageTimings {
    /// Writeability check plus memtable admission.
    admission_us: u64,
    /// Disk-pressure check and Ledger head/checkpoint sidecar derivation.
    sidecar_us: u64,
    /// Row sealing, batch encoding, the group-commit handoff and the WAL fsync.
    wal_us: u64,
    /// Ledger head/checkpoint anchor file publication.
    anchor_publish_us: u64,
    /// The in-memory MVCC row-table and router apply.
    mvcc_us: u64,
    /// Staging the batch for the checkpoint publisher.
    checkpoint_stage_us: u64,
    /// Start of the stage currently being timed, as an offset from the commit
    /// start, so each `split` reports one stage rather than a running total.
    consumed_us: u64,
}

impl CommitStageTimings {
    fn split(&mut self, started: &std::time::Instant) -> u64 {
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let stage = elapsed.saturating_sub(self.consumed_us);
        self.consumed_us = elapsed;
        stage
    }

    fn report(&self, row_count: usize, started: &std::time::Instant) {
        let total_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        if total_us < COMMIT_STAGE_SLOW_BUDGET_US {
            return;
        }
        let attributed = self.admission_us
            + self.sidecar_us
            + self.wal_us
            + self.anchor_publish_us
            + self.mvcc_us
            + self.checkpoint_stage_us;
        tracing::info!(
            code = "CALYX_ASTER_DURABLE_COMMIT_STAGE_TIMINGS",
            row_count,
            total_us,
            admission_us = self.admission_us,
            sidecar_us = self.sidecar_us,
            wal_us = self.wal_us,
            anchor_publish_us = self.anchor_publish_us,
            mvcc_us = self.mvcc_us,
            checkpoint_stage_us = self.checkpoint_stage_us,
            unattributed_us = total_us.saturating_sub(attributed),
            budget_us = COMMIT_STAGE_SLOW_BUDGET_US,
            "durable group commit exceeded its per-stage latency budget"
        );
    }
}

/// Waiter accounting for the durable commit lock. Decrements on every exit
/// path, including the error paths that abandon the acquisition.
struct CommitLockWaiterTicket<'a> {
    waiters: &'a AtomicUsize,
    released: bool,
}

impl<'a> CommitLockWaiterTicket<'a> {
    fn enqueue(waiters: &'a AtomicUsize) -> Self {
        waiters.fetch_add(1, Ordering::AcqRel);
        Self {
            waiters,
            released: false,
        }
    }

    fn admitted(&mut self) {
        if !self.released {
            self.waiters.fetch_sub(1, Ordering::AcqRel);
            self.released = true;
        }
    }
}

impl Drop for CommitLockWaiterTicket<'_> {
    fn drop(&mut self) {
        self.admitted();
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Threads currently queued for the durable commit lock.
    pub(crate) fn durable_commit_lock_waiters(&self) -> usize {
        self.commit_lock_waiters.load(Ordering::Acquire)
    }

    /// Runs `f` under the process + cross-process durable commit boundary,
    /// recording how long the caller queued and how long it held the lock.
    ///
    /// `#[track_caller]` is what makes the stall telemetry actionable: the
    /// warning below names the exact call site holding the only lock that
    /// serializes vault writes, so a future regression is attributable without
    /// re-deriving it from unrelated request timeouts (issue #1806).
    #[track_caller]
    pub(crate) fn with_durable_commit_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let caller = std::panic::Location::caller();
        let wait_started = std::time::Instant::now();
        let mut ticket = CommitLockWaiterTicket::enqueue(&self.commit_lock_waiters);
        let _process_guard = self
            .commit_lock
            .lock()
            .map_err(|_| CalyxError::backpressure("vault commit lock poisoned"))?;
        let _commit_guard = match &self.durable {
            Some(durable) => Some(crate::file_lock::FileLockGuard::acquire(
                &durable.commit_lock_path(),
            )?),
            None => None,
        };
        ticket.admitted();
        let wait_ms = wait_started.elapsed().as_millis();
        let hold_started = std::time::Instant::now();
        let outcome = self.durable_commit_lock_body(f);
        let hold_ms = hold_started.elapsed().as_millis();
        if hold_ms > DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS
            || wait_ms > DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS
        {
            tracing::warn!(
                code = "CALYX_ASTER_DURABLE_COMMIT_LOCK_SLOW",
                call_site = %caller,
                wait_ms,
                hold_ms,
                slow_budget_ms = DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS,
                queued_waiters = self.commit_lock_waiters.load(Ordering::Acquire),
                ok = outcome.is_ok(),
                "durable commit lock wait or hold exceeded the stall budget; every vault write \
                 (including the MCP initialize activity write) is serialized behind this lock"
            );
        }
        outcome
    }

    fn durable_commit_lock_body<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(durable) = &self.durable else {
            if self
                .ledger_state_reconciliation_required
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(CalyxError::ledger_group_commit_failed(
                    "Ledger state reconciliation is required but the vault has no durable physical source of truth",
                ));
            }
            return f();
        };
        if self
            .ledger_state_reconciliation_required
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.reconcile_ledger_state_from_durable_locked()?;
            self.ledger_state_reconciliation_required
                .store(false, std::sync::atomic::Ordering::Release);
            tracing::info!(
                code = "CALYX_ASTER_LEDGER_STATE_RECONCILIATION_CLEARED",
                "repaired Ledger sidecars and the configured persistent hook from physical truth before admitting the next durable operation"
            );
        }
        let durable_tip = durable.durable_tip_seq()?;
        let live_tip = self.latest_seq();
        match durable_tip.cmp(&live_tip) {
            std::cmp::Ordering::Greater => {
                self.refresh_from_durable()?;
                let refreshed_live_tip = self.latest_seq();
                if refreshed_live_tip != durable_tip {
                    return Err(durable_live_sequence_divergence(
                        durable_tip,
                        refreshed_live_tip,
                        "foreign WAL refresh did not converge the live MVCC sequence",
                    ));
                }
            }
            std::cmp::Ordering::Less => {
                return Err(durable_live_sequence_divergence(
                    durable_tip,
                    live_tip,
                    "live MVCC sequence is ahead of durable WAL truth",
                ));
            }
            std::cmp::Ordering::Equal => {}
        }
        f()
    }

    pub(crate) fn with_recurrence_write_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self
            .recurrence_write_lock
            .lock()
            .map_err(|_| CalyxError::backpressure("recurrence write lock poisoned"))?;
        let _file_guard = self
            .durable
            .as_ref()
            .map(|durable| {
                crate::file_lock::FileLockGuard::acquire(&durable.recurrence_lock_path())
            })
            .transpose()?;
        // Recurrence operations are read/modify/write transactions. Acquire
        // the normal process + cross-process commit boundary before refreshing
        // or invoking the operation so its derived decision and commit share
        // one authoritative view. Callbacks must use `*_locked` commit helpers
        // and must not recursively acquire the non-reentrant commit lock.
        self.with_durable_commit_lock(f)
    }

    fn refresh_from_durable(&self) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Ok(());
        };
        let current = self.latest_seq();
        let recovered = durable.recover_current_batches()?;
        self.reconcile_ledger_state_from_recovery_locked(&recovered)?;
        self.replace_retention_horizon(recovered.retention_horizon.clone())?;
        self.rows
            .advance_derived_content_seq_to_at_least(recovered.derived_content_floor_seq);
        durable.advance_derived_content_watermark_to_at_least(recovered.derived_content_floor_seq);
        self.rows
            .advance_panel_content_seqs_to_at_least(&recovered.panel_content_floor_seqs)?;
        durable
            .advance_panel_content_watermarks_to_at_least(&recovered.panel_content_floor_seqs)?;
        // WAL-tail batches from a foreign writer have no durable-batch SSTs
        // yet; stage them here so this handle's next checkpoint flush cannot
        // advance the manifest past them if that writer dies (issue #1132).
        durable.stage_recovered_wal_batches(
            recovered
                .batches
                .iter()
                .filter(|batch| batch.seq > recovered.wal_replay_floor_seq)
                .map(|batch| (batch.seq, batch.rows.clone()))
                .collect(),
        )?;
        self.rows.restore_recovered_batches_and_advance(
            recovered
                .batches
                .iter()
                .filter(|batch| batch.seq > current)
                .map(|batch| {
                    (
                        batch.seq,
                        batch
                            .rows
                            .iter()
                            .map(|row| (row.cf, row.key.clone(), row.value.clone())),
                    )
                }),
            recovered.last_recovered_seq,
            recovered.wal_replay_floor_seq,
            recovered.migrate_derived_content_model,
        )?;
        if recovered.migrate_derived_content_model {
            self.rows.migrate_panel_content_seqs_to_at_least(
                recovered.derived_content_floor_seq,
                recovered.active_panel_version,
            )?;
        }
        durable.advance_panel_content_watermarks_to_at_least(
            &self.rows.panel_content_seqs_snapshot()?,
        )?;
        Ok(())
    }

    pub(super) fn commit_rows(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        self.with_durable_commit_lock(|| self.commit_rows_locked(rows))
    }

    pub(crate) fn commit_rows_locked(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if rows
            .iter()
            .any(|row| row.cf == crate::cf::ColumnFamily::TimeIndex)
        {
            return Err(CalyxError::aster_corrupt_shard(
                "time_index is a reserved derived column family; caller-supplied rows are forbidden because they can forge or corrupt the sole time-to-sequence mapping",
            ));
        }
        self.commit_rows_locked_inner(rows)
    }

    /// Trusted erasure path for tombstoning existing derived TimeIndex rows.
    /// It deliberately accepts only the MVCC tombstone value, never a forged
    /// live time-to-sequence mapping.
    pub(crate) fn commit_erasure_rows_locked(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if let Some(row) = rows.iter().find(|row| {
            row.cf == crate::cf::ColumnFamily::TimeIndex
                && !crate::mvcc::is_tombstone_value(&row.value)
        }) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "trusted erasure attempted a live time_index write: key_len={} value_len={}",
                row.key.len(),
                row.value.len()
            )));
        }
        self.commit_rows_locked_inner(rows)
    }

    fn commit_rows_locked_inner(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if rows.is_empty() {
            // Empty commit: do not advance the seq or stamp a time-index entry.
            return Ok(self.latest_seq());
        }
        // Time-travel (PH72 T04): stamp this group-commit with one time-index
        // entry in the SAME batch as the data, so the (millis -> seqno) mapping
        // is atomic with the write — a crash can never leave a write without its
        // time mapping (A15). We hold the durable commit lock here, so the next
        // allocated seq is exactly current_seq()+1; we assert that against the
        // committed seq below and fail loud on any divergence (never silent).
        let predicted = self.rows.current_seq().saturating_add(1);
        let (cf, key, value) = crate::timetravel::entry_row(self.clock.now(), predicted);
        let mut all_rows = rows.to_vec();
        all_rows.push(encode::WriteRow { cf, key, value });
        if !rows
            .iter()
            .any(|row| row.cf == crate::cf::ColumnFamily::Ledger)
        {
            all_rows.push(raw_commitment::commitment_row(predicted, &all_rows)?);
        }
        let committed = match self.commit_prepared_rows(&all_rows) {
            Ok(committed) => committed,
            Err(error) => {
                // A non-durable vault can publish the authoritative MVCC row
                // table and then fail its router projection. Under this
                // exclusive commit boundary, observing exactly the predicted
                // new MVCC sequence proves this operation applied. Preserve a
                // lower layer's durable marker when one already exists.
                if self.rows.current_seq() == predicted {
                    let _ = self.post_commit_error_seq.compare_exchange(
                        0,
                        predicted,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    );
                }
                return Err(error);
            }
        };
        if committed != predicted {
            // The batch crossed the irreversible commit boundary even though
            // the time-index invariant failed. Preserve that exact sequence
            // for guarded callers just like every other post-commit failure.
            self.post_commit_error_seq
                .store(committed, std::sync::atomic::Ordering::Release);
            return Err(CalyxError::aster_corrupt_shard(format!(
                "time-index seqno prediction {predicted} diverged from committed seq {committed}"
            )));
        }
        Ok(committed)
    }

    fn commit_prepared_rows(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        // #1936: a durable group commit on the deployment host measures ~6 ms
        // while its own fsync costs 0.26 ms — ~27x its durable I/O — and the
        // existing timing splits stop at the commit boundary, so the 6 ms was
        // one opaque number. These spans are the only fixed points inside it.
        // Reported only above a budget, so a healthy commit stays silent.
        let started = std::time::Instant::now();
        let mut stage = CommitStageTimings::default();
        if !rows.is_empty() {
            self.ensure_writeable("commit")?;
        }
        self.rows.ensure_memtable_admission(
            rows.iter()
                .map(|row| (row.cf, row.key.as_slice(), row.value.as_slice())),
        )?;
        stage.admission_us = stage.split(&started);
        let Some(durable) = &self.durable else {
            let seq = self.commit_rows_to_mvcc(rows);
            stage.mvcc_us = stage.split(&started);
            stage.report(rows.len(), &started);
            return seq;
        };

        durable.ensure_disk_write_allowed(self.rows.resource_counters())?;
        // Validate and derive Ledger sidecars before the irreversible WAL
        // append. After append succeeds, every failure is a committed-outcome
        // reconciliation event, never an ordinary retryable write error.
        let head_anchor = crate::ledger_head::newest_anchor_from_rows(rows)?;
        let checkpoint_anchor = crate::ledger_head::newest_checkpoint_from_rows(rows)?;
        stage.sidecar_us = stage.split(&started);
        // Seals, encodes and hands the batch to the group-commit thread, then
        // blocks on its reply. This span therefore covers the WAL fsync AND two
        // cross-thread handoffs, which on a hybrid CPU with parked E-cores is
        // not the same cost as the fsync alone.
        let durable_seq = durable.append_batch(rows)?;
        stage.wal_us = stage.split(&started);

        let publish = (|| -> Result<Seq> {
            if let Some(anchor) = &head_anchor {
                crate::ledger_head::write_head_anchor(durable.root(), anchor)?;
            }
            if let Some(anchor) = &checkpoint_anchor {
                crate::ledger_head::write_checkpoint_anchor(durable.root(), anchor)?;
            }
            stage.anchor_publish_us = stage.split(&started);
            let mvcc_seq = self.commit_rows_to_mvcc(rows)?;
            stage.mvcc_us = stage.split(&started);
            if mvcc_seq != durable_seq {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "durable WAL seq {durable_seq} diverged from MVCC seq {mvcc_seq}"
                )));
            }
            durable.stage_checkpoint_batch(durable_seq, rows)?;
            stage.checkpoint_stage_us = stage.split(&started);
            Ok(mvcc_seq)
        })();
        stage.report(rows.len(), &started);

        match publish {
            Ok(seq) => Ok(seq),
            Err(post_wal_error) => {
                // Preserve the exact irreversible sequence as typed state for
                // the outer guarded-write API. It consumes this marker before
                // releasing the durable commit lock, so another writer cannot
                // overwrite or misattribute the outcome.
                self.post_commit_error_seq
                    .store(durable_seq, std::sync::atomic::Ordering::Release);
                let restore = self.restore_committed_rows(durable_seq, rows);
                let head_repair = head_anchor.as_ref().map_or(Ok(()), |anchor| {
                    crate::ledger_head::write_head_anchor(durable.root(), anchor)
                });
                let checkpoint_anchor_repair =
                    checkpoint_anchor.as_ref().map_or(Ok(()), |anchor| {
                        crate::ledger_head::write_checkpoint_anchor(durable.root(), anchor)
                    });
                let panel_watermarks = self.rows.panel_content_seqs_snapshot();
                let checkpoint = panel_watermarks.and_then(|panel_watermarks| {
                    durable.advance_panel_content_watermarks_to_at_least(&panel_watermarks)?;
                    durable.checkpoint_committed_batch_with_pending(durable_seq, rows)
                });
                if rows
                    .iter()
                    .any(|row| row.cf == crate::cf::ColumnFamily::Ledger)
                {
                    self.ledger_state_reconciliation_required
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                Err(post_wal_commit_error(
                    durable_seq,
                    &post_wal_error,
                    &restore,
                    &head_repair,
                    &checkpoint_anchor_repair,
                    &checkpoint,
                ))
            }
        }
    }

    fn commit_rows_to_mvcc(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        self.rows.commit_batch(
            rows.iter()
                .map(|row| (row.cf, row.key.clone(), row.value.clone())),
        )
    }

    fn restore_committed_rows(&self, seq: Seq, rows: &[encode::WriteRow]) -> Result<()> {
        self.rows.restore_batches_and_advance(
            [(
                seq,
                rows.iter()
                    .map(|row| (row.cf, row.key.clone(), row.value.clone())),
            )],
            seq,
        )?;
        Ok(())
    }
}

fn post_wal_commit_error(
    durable_seq: Seq,
    post_wal_error: &CalyxError,
    restore: &Result<()>,
    head_repair: &Result<()>,
    checkpoint_anchor_repair: &Result<()>,
    checkpoint: &Result<()>,
) -> CalyxError {
    CalyxError {
        code: CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
        message: format!(
            "WAL commit is durable but post-WAL publication failed; wal_seq={durable_seq} \
             post_wal=error[{}]: {} restore={} head_repair={} checkpoint_anchor_repair={} checkpoint={}",
            post_wal_error.code,
            post_wal_error.message,
            reconciliation_outcome(restore),
            reconciliation_outcome(head_repair),
            reconciliation_outcome(checkpoint_anchor_repair),
            reconciliation_outcome(checkpoint),
        ),
        remediation: "treat wal_seq as durably committed; reconcile by idempotency/readback before retrying, and allow the next durable boundary to rebuild any latched Ledger sidecar/hook state from physical truth",
    }
}

fn durable_live_sequence_divergence(
    durable_tip: Seq,
    live_tip: Seq,
    context: &'static str,
) -> CalyxError {
    CalyxError {
        code: CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
        message: format!(
            "{context}; durable_wal_tip={durable_tip} live_mvcc_tip={live_tip}; refusing to admit another durable write"
        ),
        remediation: "close and reopen this vault from durable physical truth, inspect the preceding post-WAL failure, and do not retry the rejected logical operation without idempotency/readback",
    }
}

fn reconciliation_outcome(result: &Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(error) => format!("error[{}]: {}", error.code, error.message),
    }
}
