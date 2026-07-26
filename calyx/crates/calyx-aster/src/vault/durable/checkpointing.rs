//! Checkpoint staging and durable-batch SST writes for `DurableVault`.
//!
//! Invariant (issue #1132): the manifest's `durable_seq` may only advance
//! past a committed batch after that batch's rows exist as durable-batch
//! SSTs, because the WAL replay floor (and later segment recycling) is
//! derived from `durable_seq`. A manifest that outruns durable-batch coverage
//! strands the covered rows: their only surviving physical home is whatever
//! Router memtable-flush SSTs happened to be written, which full-restore
//! opens can never read. `stage_recovered_wal_batches` closes the recovery
//! half of that invariant; `flush_pending_checkpoints` writes every staged
//! batch before the single manifest advance.

use super::super::encode::WriteRow;
use super::{DurableVault, storage_error};
use crate::cf::ColumnFamily;
use crate::security::value_crypto::seal_value;
use crate::sst::write_sst;
use calyx_core::{CalyxError, Result};
use std::collections::BTreeMap;
use std::fs;

/// Maximum staged batches one bounded checkpoint drain may materialize before
/// it returns (issue #1806).
///
/// Every durable group commit stages exactly one checkpoint batch, and each
/// batch is written as one `write_atomic_create_new` SST *per touched CF*
/// (temp write + fsync + rename + directory fsync). A derived-write fanout
/// backfill therefore stages tens of thousands of ~113-byte SSTs between
/// checkpoint ticks, and draining that backlog inside a single durable
/// commit-lock acquisition blocked every writer — including the MCP
/// `initialize` activity-recorder write — for multi-minute stretches.
pub(in crate::vault) const CHECKPOINT_DRAIN_MAX_BATCHES: usize = 256;

/// Row ceiling for one coalesced checkpoint flush.
///
/// Coalescing removes the per-batch fsync cost, so the remaining bound to
/// respect is the size of the single SST produced (and the transient memory
/// used to build it). These caps keep one drain's output to a normal SST rather
/// than an unbounded merge of the whole backlog.
const COALESCED_FLUSH_MAX_ROWS: usize = 200_000;

/// Byte ceiling (key + value, pre-encoding) for one coalesced checkpoint flush.
const COALESCED_FLUSH_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Name discriminator for a coalesced durable-batch SST.
///
/// Per-batch writes name files `{seq:020}-{first_index:04}.sst`, where the
/// index disambiguates several CFs written for the same seq. A coalesced flush
/// publishes at most one file per CF under the prefix's highest seq, in each
/// CF's own directory, so a fixed discriminator cannot collide.
const COALESCED_FLUSH_INDEX: usize = 0;

/// Outcome of one bounded checkpoint drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::vault) struct CheckpointDrainChunk {
    /// Staged batches whose durable-batch SSTs were written and manifested.
    pub batches_written: usize,
    /// Rows contained in those batches.
    pub rows_written: usize,
    /// Highest manifested seq, or 0 when nothing was written.
    pub last_written_seq: u64,
    /// Batches still staged after this drain (including any re-staged tail).
    pub remaining_batches: usize,
}

/// A contiguous staged prefix reserved under the global durable commit lock.
///
/// The bytes are then materialized while that global lock is released. The
/// prefix remains owned by this value until manifest publication succeeds or
/// the caller explicitly re-stages it after an error.
#[derive(Debug)]
pub(in crate::vault) struct PreparedCheckpoint {
    pub base_durable_seq: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub rows: usize,
    pub bytes: usize,
    batches: Vec<(u64, Vec<WriteRow>)>,
}

impl PreparedCheckpoint {
    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }
}

/// Physical output produced before a prepared checkpoint is manifested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::vault) struct CheckpointMaterialization {
    pub sst_files: usize,
    pub sst_bytes: u64,
}

impl DurableVault {
    /// Checkpoints a WAL-committed batch without allowing the manifest replay
    /// floor to jump past older staged batches. This is the post-WAL recovery
    /// path: stage the current committed batch alongside every predecessor,
    /// write the complete ordered set, then advance the manifest once.
    pub(in crate::vault) fn checkpoint_committed_batch_with_pending(
        &self,
        seq: u64,
        rows: &[WriteRow],
    ) -> Result<()> {
        self.stage_recovered_wal_batches(vec![(seq, rows.to_vec())])?;
        let Some(_checkpoint_guard) =
            crate::file_lock::FileLockGuard::try_acquire(&self.checkpoint_lock_path())?
        else {
            return Err(CalyxError::backpressure(format!(
                "checkpoint reconciliation for committed seq {seq} could not acquire the checkpoint publisher lock without waiting while the global commit lock is held; the WAL batch remains staged and must be reconciled by the active checkpoint publisher"
            )));
        };
        self.flush_pending_checkpoints()
    }

    pub(in crate::vault) fn stage_checkpoint_batch(
        &self,
        seq: u64,
        rows: &[WriteRow],
    ) -> Result<()> {
        self.pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?
            .push((seq, rows.to_vec()));
        Ok(())
    }

    /// Stages recovered WAL-tail batches (seq beyond the manifest floor) so
    /// the next checkpoint flush writes their durable-batch SSTs before any
    /// manifest advance can strand them behind the WAL replay floor (#1132).
    pub(in crate::vault) fn stage_recovered_wal_batches(
        &self,
        batches: Vec<(u64, Vec<WriteRow>)>,
    ) -> Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        let mut pending = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?;
        for (seq, rows) in batches {
            if let Some((_, staged_rows)) = pending.iter().find(|(staged, _)| *staged == seq) {
                if staged_rows != &rows {
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "checkpoint seq {seq} was staged twice with different rows: existing_rows={} incoming_rows={}",
                        staged_rows.len(),
                        rows.len()
                    )));
                }
                continue;
            }
            pending.push((seq, rows));
        }
        pending.sort_by_key(|(seq, _)| *seq);
        Ok(())
    }

    /// Materializes a whole staged prefix as **one SST per touched CF**.
    ///
    /// [`Self::write_rows`] writes one SST per `(seq, CF)`, and every durable
    /// group commit stages exactly one batch, so draining 256 staged batches
    /// across 3 CFs used to publish ~768 files of ~113 bytes each. Every one of
    /// those goes through `write_atomic_create_new`: temp create, `sync_all()`,
    /// rename, parent-directory fsync — **two fsyncs plus two metadata ops per
    /// file**. Issue #1832 moved this physical work outside the durable commit
    /// lock; the immutable files are published first and only then made
    /// authoritative by one short manifest publication section.
    ///
    /// That is fsync amplification, not a throughput limit: the payload is
    /// bytes and the cost is barriers. `manifest_seq` is a metadata generation
    /// counter and must never be compared with `durable_seq` as backlog;
    /// checkpoint lag is `durable_seq - derived_content_seq` (#1832).
    ///
    /// Coalescing is the standard answer. RocksDB group-commits concurrent
    /// writes into one WAL write with one fsync, and its
    /// "group multiple batch of flush into one manifest write" change fixed the
    /// identical per-batch-fsync bottleneck in `LogAndApply`; BoLT's group
    /// compaction merges several victim SSTables in one pass for the same
    /// reason. Here the same prefix becomes one SST per CF: the fsync count per
    /// drain drops from `O(batches x CFs)` to `O(CFs)`.
    ///
    /// Ordering is preserved exactly. The prefix is contiguous and ascending in
    /// seq, and everything newer is still staged, so publishing under the
    /// prefix's highest seq keeps newest-wins order intact. Within the prefix,
    /// later seqs must shadow earlier ones for the same key, which the
    /// per-CF `BTreeMap` does by construction: batches are applied in ascending
    /// seq order and a later insert replaces an earlier one.
    fn write_coalesced_rows(
        &self,
        batches: &[(u64, Vec<WriteRow>)],
    ) -> Result<CheckpointMaterialization> {
        let Some((last_seq, _)) = batches.last() else {
            return Ok(CheckpointMaterialization::default());
        };
        // CF -> key -> value, latest write wins.
        let mut by_cf: BTreeMap<ColumnFamily, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
        for (_, rows) in batches {
            for row in rows {
                by_cf
                    .entry(row.cf)
                    .or_default()
                    .insert(row.key.clone(), row.value.clone());
            }
        }
        let mut materialized = CheckpointMaterialization::default();
        for (cf, rows) in by_cf {
            if rows.is_empty() {
                continue;
            }
            let dir = self.cf_dir(cf);
            fs::create_dir_all(&dir).map_err(|error| storage_error("create CF dir", error))?;
            let path = dir.join(format!("{last_seq:020}-{COALESCED_FLUSH_INDEX:04}.sst"));
            match &self.value_crypto {
                Some(context) => {
                    let entries = rows
                        .iter()
                        .map(|(key, value)| Ok((key.clone(), seal_value(context, cf, key, value)?)))
                        .collect::<Result<Vec<_>>>()?;
                    let summary = write_sst(
                        &path,
                        entries
                            .iter()
                            .map(|(key, value)| (key.as_slice(), value.as_slice())),
                    )?;
                    materialized.sst_files = materialized.sst_files.saturating_add(1);
                    materialized.sst_bytes = materialized.sst_bytes.saturating_add(summary.bytes);
                }
                None => {
                    let summary = write_sst(
                        &path,
                        rows.iter()
                            .map(|(key, value)| (key.as_slice(), value.as_slice())),
                    )?;
                    materialized.sst_files = materialized.sst_files.saturating_add(1);
                    materialized.sst_bytes = materialized.sst_bytes.saturating_add(summary.bytes);
                }
            }
        }
        Ok(materialized)
    }

    /// Reserves the oldest complete WAL-tail prefix for off-lock materialization.
    ///
    /// This method must run under `AsterVault::with_durable_commit_lock`. It
    /// first discards entries already covered by the authoritative manifest,
    /// then proves that the local staging vector covers every sequence from
    /// `manifest.durable_seq + 1` through the WAL tip. A missing or duplicate
    /// sequence fails closed: publishing a later floor would strand that WAL
    /// batch after recycling.
    pub(in crate::vault) fn prepare_pending_checkpoint(
        &self,
        max_batches: usize,
    ) -> Result<Option<PreparedCheckpoint>> {
        if max_batches == 0 {
            return Err(CalyxError::disk_pressure(
                "checkpoint preparation requires max_batches >= 1; a zero batch budget can never advance the manifest",
            ));
        }
        let base_durable_seq = self.manifest_durable_seq()?;
        let wal_tip = self.durable_tip_seq()?;
        if wal_tip < base_durable_seq {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "checkpoint manifest floor {base_durable_seq} exceeds WAL tip {wal_tip}"
            )));
        }
        let mut pending = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?;
        if let Some(window) = pending.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "staged checkpoint batches are not strictly seq-ordered ({} then {}); refusing to reserve a prefix",
                window[0].0, window[1].0
            )));
        }
        let already_manifested = pending.partition_point(|(seq, _)| *seq <= base_durable_seq);
        pending.drain(..already_manifested);
        if pending.is_empty() {
            if wal_tip != base_durable_seq {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "checkpoint staging is empty but WAL tip {wal_tip} exceeds manifest floor {base_durable_seq}; publishing any later floor would strand committed WAL rows"
                )));
            }
            return Ok(None);
        }
        let expected_first = base_durable_seq.checked_add(1).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "checkpoint manifest floor reached u64::MAX while staged batches remain",
            )
        })?;
        if pending[0].0 != expected_first {
            return Err(CalyxError::backpressure(format!(
                "checkpoint prefix is not available from the manifest floor: expected first seq {expected_first}, staged first seq {}; another publisher may have reserved the prefix, so this caller refuses to publish across the gap",
                pending[0].0
            )));
        }
        if let Some(window) = pending
            .windows(2)
            .find(|pair| pair[0].0.checked_add(1) != Some(pair[1].0))
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "staged checkpoint sequence gap {} -> {}; a manifest advance would strand the missing WAL batch",
                window[0].0, window[1].0
            )));
        }
        let staged_tip = pending.last().map_or(base_durable_seq, |(seq, _)| *seq);
        if staged_tip != wal_tip {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "checkpoint staging tip {staged_tip} does not cover WAL tip {wal_tip}; refusing to publish incomplete coverage"
            )));
        }

        let mut selected = 0_usize;
        let mut selected_rows = 0_usize;
        let mut selected_bytes = 0_usize;
        for (_, rows) in pending.iter().take(max_batches) {
            let batch_rows = rows.len();
            let batch_bytes = rows.iter().fold(0_usize, |bytes, row| {
                bytes.saturating_add(row.key.len().saturating_add(row.value.len()))
            });
            if selected > 0
                && (selected_rows.saturating_add(batch_rows) > COALESCED_FLUSH_MAX_ROWS
                    || selected_bytes.saturating_add(batch_bytes) > COALESCED_FLUSH_MAX_BYTES)
            {
                break;
            }
            selected = selected.saturating_add(1);
            selected_rows = selected_rows.saturating_add(batch_rows);
            selected_bytes = selected_bytes.saturating_add(batch_bytes);
        }
        let batches = pending.drain(..selected).collect::<Vec<_>>();
        let first_seq = batches.first().map_or(0, |(seq, _)| *seq);
        let last_seq = batches.last().map_or(0, |(seq, _)| *seq);
        Ok(Some(PreparedCheckpoint {
            base_durable_seq,
            first_seq,
            last_seq,
            rows: selected_rows,
            bytes: selected_bytes,
            batches,
        }))
    }

    /// Writes immutable checkpoint SSTs without publishing a new replay floor.
    pub(in crate::vault) fn materialize_prepared_checkpoint(
        &self,
        prepared: &PreparedCheckpoint,
    ) -> Result<CheckpointMaterialization> {
        self.write_coalesced_rows(&prepared.batches)
    }

    /// Publishes an already-materialized prefix under the dedicated checkpoint
    /// publisher lock. The staging mutex below is the only boundary shared
    /// with concurrent commits; manifest filesystem I/O must never inherit the
    /// unrelated global writer lock (#1832).
    pub(in crate::vault) fn publish_prepared_checkpoint(
        &self,
        prepared: &PreparedCheckpoint,
    ) -> Result<CheckpointDrainChunk> {
        let current_floor = self.manifest_durable_seq()?;
        if current_floor < prepared.base_durable_seq {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "checkpoint manifest floor regressed from reserved base {} to {current_floor}",
                prepared.base_durable_seq
            )));
        }

        let mut pending = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?;
        // Validate before mutating. An early error after `drain(..)` would drop
        // the unvisited newer tail from in-memory staging even though its WAL
        // bytes remain authoritative.
        for batch in pending.iter() {
            if batch.0 <= current_floor || batch.0 > prepared.last_seq {
                continue;
            }
            let expected = prepared
                .batches
                .binary_search_by_key(&batch.0, |(seq, _)| *seq)
                .ok()
                .and_then(|index| prepared.batches.get(index))
                .ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "checkpoint publication encountered unexpected staged seq {} within reserved range {}..={}",
                        batch.0, prepared.first_seq, prepared.last_seq
                    ))
                })?;
            if expected.1 != batch.1 {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "checkpoint seq {} was re-staged with different rows while its SSTs were materialized: reserved_rows={} staged_rows={}",
                    batch.0,
                    expected.1.len(),
                    batch.1.len()
                )));
            }
        }
        let newly_covered_floor = current_floor.max(prepared.last_seq);
        pending.retain(|(seq, _)| *seq > newly_covered_floor);
        let remaining_batches = pending.len();
        drop(pending);

        for (seq, rows) in &prepared.batches {
            self.advance_checkpointed_derived_content(*seq, rows);
        }
        if current_floor < prepared.last_seq {
            self.write_manifest(prepared.last_seq)?;
        }
        Ok(CheckpointDrainChunk {
            batches_written: prepared.batch_count(),
            rows_written: prepared.rows,
            last_written_seq: prepared.last_seq,
            remaining_batches,
        })
    }

    /// Returns a reserved prefix to staging after any materialize/publish error.
    pub(in crate::vault) fn restage_prepared_checkpoint(
        &self,
        prepared: PreparedCheckpoint,
    ) -> Result<()> {
        self.stage_recovered_wal_batches(prepared.batches)
    }

    /// Materializes every staged checkpoint batch.
    ///
    /// This is the unbounded-completion contract used only by callers that
    /// already hold the global commit lock and have exclusively acquired the
    /// checkpoint publisher lock. Routine maintenance uses the split
    /// reserve/materialize/publish path above so physical I/O is off-lock.
    pub(super) fn flush_pending_checkpoints(&self) -> Result<()> {
        loop {
            let chunk = self.flush_pending_checkpoints_bounded(CHECKPOINT_DRAIN_MAX_BATCHES)?;
            if chunk.remaining_batches == 0 {
                return Ok(());
            }
            if chunk.batches_written == 0 {
                return Err(CalyxError::disk_pressure(format!(
                    "checkpoint drain made no progress with {} batches still staged; the manifest cannot advance and durable-batch SSTs would be stranded",
                    chunk.remaining_batches
                )));
            }
        }
    }

    /// Materializes at most `max_batches` staged checkpoint batches.
    ///
    /// The staged prefix is *moved* out of the staging vector rather than
    /// cloned, so a multi-gigabyte derived-write backlog is no longer deep
    /// copied on every flush. Anything not written is re-staged before this
    /// call returns, so no committed batch is ever dropped: on a write error
    /// the entire chunk is re-staged and the manifest is left untouched,
    /// exactly as before this call.
    ///
    /// Invariant (#1132): the staging vector is kept ordered by seq, the chunk
    /// is its oldest contiguous prefix, and the manifest advances only to the
    /// last seq actually written — so no older staged batch can ever end up
    /// behind the WAL replay floor.
    pub(in crate::vault) fn flush_pending_checkpoints_bounded(
        &self,
        max_batches: usize,
    ) -> Result<CheckpointDrainChunk> {
        let Some(prepared) = self.prepare_pending_checkpoint(max_batches)? else {
            return Ok(CheckpointDrainChunk::default());
        };
        if let Err(error) = self.materialize_prepared_checkpoint(&prepared) {
            self.restage_prepared_checkpoint(prepared)?;
            return Err(error);
        }
        match self.publish_prepared_checkpoint(&prepared) {
            Ok(chunk) => Ok(chunk),
            Err(error) => {
                self.restage_prepared_checkpoint(prepared)?;
                Err(error)
            }
        }
    }
}
