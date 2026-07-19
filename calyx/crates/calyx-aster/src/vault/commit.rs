use super::{AsterVault, encode};
use calyx_core::{CalyxError, Clock, Result, Seq};

/// The WAL append is durable, but the live MVCC/router apply failed and the
/// caller must reconcile the reported sequence before retrying.
pub const CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED: &str =
    "CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED";

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub(crate) fn with_durable_commit_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _process_guard = self
            .commit_lock
            .lock()
            .map_err(|_| CalyxError::backpressure("vault commit lock poisoned"))?;
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
        let _commit_guard = crate::file_lock::FileLockGuard::acquire(&durable.commit_lock_path())?;
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
        self.rows.restore_batches_and_advance(
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
        let committed = self.commit_prepared_rows(&all_rows)?;
        if committed != predicted {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "time-index seqno prediction {predicted} diverged from committed seq {committed}"
            )));
        }
        Ok(committed)
    }

    fn commit_prepared_rows(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if !rows.is_empty() {
            self.ensure_writeable("commit")?;
        }
        self.rows.ensure_memtable_admission(
            rows.iter()
                .map(|row| (row.cf, row.key.as_slice(), row.value.as_slice())),
        )?;
        let Some(durable) = &self.durable else {
            return self.commit_rows_to_mvcc(rows);
        };

        durable.ensure_disk_write_allowed(self.rows.resource_counters())?;
        // Validate and derive Ledger sidecars before the irreversible WAL
        // append. After append succeeds, every failure is a committed-outcome
        // reconciliation event, never an ordinary retryable write error.
        let head_anchor = crate::ledger_head::newest_anchor_from_rows(rows)?;
        let checkpoint_anchor = crate::ledger_head::newest_checkpoint_from_rows(rows)?;
        let durable_seq = durable.append_batch(rows)?;

        let publish = (|| -> Result<Seq> {
            if let Some(anchor) = &head_anchor {
                crate::ledger_head::write_head_anchor(durable.root(), anchor)?;
            }
            if let Some(anchor) = &checkpoint_anchor {
                crate::ledger_head::write_checkpoint_anchor(durable.root(), anchor)?;
            }
            let mvcc_seq = self.commit_rows_to_mvcc(rows)?;
            if mvcc_seq != durable_seq {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "durable WAL seq {durable_seq} diverged from MVCC seq {mvcc_seq}"
                )));
            }
            durable.stage_checkpoint_batch(durable_seq, rows)?;
            Ok(mvcc_seq)
        })();

        match publish {
            Ok(seq) => Ok(seq),
            Err(post_wal_error) => {
                let restore = self.restore_committed_rows(durable_seq, rows);
                let head_repair = head_anchor.as_ref().map_or(Ok(()), |anchor| {
                    crate::ledger_head::write_head_anchor(durable.root(), anchor)
                });
                let checkpoint_anchor_repair =
                    checkpoint_anchor.as_ref().map_or(Ok(()), |anchor| {
                        crate::ledger_head::write_checkpoint_anchor(durable.root(), anchor)
                    });
                let checkpoint = durable.checkpoint_committed_batch_with_pending(durable_seq, rows);
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
