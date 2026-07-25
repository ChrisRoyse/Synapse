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
use std::time::{Duration, Instant};

/// Maximum staged batches one bounded checkpoint drain may materialize before
/// it returns so the caller can release the durable commit lock (issue #1806).
///
/// Every durable group commit stages exactly one checkpoint batch, and each
/// batch is written as one `write_atomic_create_new` SST *per touched CF*
/// (temp write + fsync + rename + directory fsync). A derived-write fanout
/// backfill therefore stages tens of thousands of ~113-byte SSTs between
/// checkpoint ticks, and draining that backlog inside a single durable
/// commit-lock acquisition blocked every writer — including the MCP
/// `initialize` activity-recorder write — for multi-minute stretches.
pub(in crate::vault) const CHECKPOINT_DRAIN_MAX_BATCHES: usize = 256;

/// Wall-clock budget for one bounded checkpoint drain.
///
/// The batch ceiling alone cannot bound the hold time because per-SST fsync
/// cost varies by orders of magnitude across hosts and filesystems. The drain
/// therefore also stops at this elapsed budget (always after at least one
/// batch, so the manifest can never stall), which is what actually delivers
/// the sub-second commit-lock holds #1806 requires.
pub(in crate::vault) const CHECKPOINT_DRAIN_HOLD_BUDGET: Duration = Duration::from_millis(250);

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
    /// file**, all while the durable commit lock is held and every vault write
    /// (including the MCP `initialize` activity row) queues behind it.
    ///
    /// That is fsync amplification, not a throughput limit: the payload is
    /// bytes and the cost is barriers. It is why checkpoint materialization
    /// never caught up on this host — observed `manifest_seq=48,427` against
    /// `durable_seq=1,554,692` (#1832).
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
    fn write_coalesced_rows(&self, batches: &[(u64, Vec<WriteRow>)]) -> Result<()> {
        let Some((last_seq, _)) = batches.last() else {
            return Ok(());
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
                    write_sst(
                        &path,
                        entries
                            .iter()
                            .map(|(key, value)| (key.as_slice(), value.as_slice())),
                    )?;
                }
                None => {
                    write_sst(
                        &path,
                        rows.iter()
                            .map(|(key, value)| (key.as_slice(), value.as_slice())),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Materializes every staged checkpoint batch.
    ///
    /// This is the unbounded-completion contract required by the post-WAL
    /// reconciliation path and by deterministic close. It is implemented as a
    /// sequence of bounded drains so a single pass never builds one giant
    /// in-memory clone of the whole backlog; callers that must also bound the
    /// *durable commit lock hold* use `AsterVault::drain_checkpoints_paced`,
    /// which releases the lock between drains.
    pub(super) fn flush_pending_checkpoints(&self) -> Result<()> {
        loop {
            let chunk = self
                .flush_pending_checkpoints_bounded(CHECKPOINT_DRAIN_MAX_BATCHES, Duration::MAX)?;
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

    /// Materializes at most `max_batches` staged checkpoint batches, stopping
    /// early once `hold_budget` elapses (never before the first batch).
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
        hold_budget: Duration,
    ) -> Result<CheckpointDrainChunk> {
        if max_batches == 0 {
            return Err(CalyxError::disk_pressure(
                "bounded checkpoint drain requires max_batches >= 1; a zero batch budget can never advance the manifest and would strand every staged batch",
            ));
        }
        let chunk = self.take_staged_checkpoint_prefix(max_batches)?;
        if chunk.is_empty() {
            return Ok(CheckpointDrainChunk::default());
        }
        let started = Instant::now();
        // Select how many staged batches to materialize together. This loop is
        // pure in-memory accounting — the physical write happens once, below —
        // so the elapsed budget here only bounds selection, not I/O.
        let mut selected = 0_usize;
        let mut rows_written = 0_usize;
        let mut last_written_seq = 0_u64;
        let mut coalesced_bytes = 0_usize;
        for (index, (seq, rows)) in chunk.iter().enumerate() {
            if index > 0
                && (started.elapsed() >= hold_budget
                    || rows_written >= COALESCED_FLUSH_MAX_ROWS
                    || coalesced_bytes >= COALESCED_FLUSH_MAX_BYTES)
            {
                break;
            }
            coalesced_bytes = coalesced_bytes.saturating_add(
                rows.iter()
                    .map(|row| row.key.len().saturating_add(row.value.len()))
                    .sum::<usize>(),
            );
            selected = index.saturating_add(1);
            rows_written = rows_written.saturating_add(rows.len());
            last_written_seq = *seq;
        }
        let mut written = 0_usize;
        let mut write_error = None;
        match self.write_coalesced_rows(&chunk[..selected]) {
            Ok(()) => {
                for (seq, rows) in &chunk[..selected] {
                    self.advance_checkpointed_derived_content(*seq, rows);
                }
                written = selected;
            }
            Err(error) => {
                rows_written = 0;
                last_written_seq = 0;
                write_error = Some(error);
            }
        }
        if let Some(error) = write_error {
            // Preserve the pre-#1806 failure semantics exactly: the staging
            // vector is restored in full and the manifest is not advanced, so
            // a retry sees the identical staged set it would have seen when
            // the whole drain was one clone-and-write pass.
            self.restage_checkpoint_prefix(chunk)?;
            return Err(error);
        }
        let mut chunk = chunk;
        let deferred = chunk.split_off(written);
        self.restage_checkpoint_prefix(deferred)?;
        if written > 0 {
            self.write_manifest(last_written_seq)?;
        }
        let remaining = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?
            .len();
        Ok(CheckpointDrainChunk {
            batches_written: written,
            rows_written,
            last_written_seq,
            remaining_batches: remaining,
        })
    }

    /// Removes the oldest contiguous staged prefix, failing closed if the
    /// staging vector is not seq-ordered (which would let the manifest advance
    /// past an older staged batch and strand it — issue #1132).
    fn take_staged_checkpoint_prefix(
        &self,
        max_batches: usize,
    ) -> Result<Vec<(u64, Vec<WriteRow>)>> {
        let mut pending = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?;
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(window) = pending.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "staged checkpoint batches are not strictly seq-ordered ({} then {}); a bounded drain would advance the manifest past an older staged batch and strand its rows behind the WAL replay floor",
                window[0].0, window[1].0
            )));
        }
        let take = max_batches.min(pending.len());
        Ok(pending.drain(..take).collect())
    }

    /// Returns unwritten batches to the front of the staging vector, keeping
    /// it seq-ordered.
    fn restage_checkpoint_prefix(&self, batches: Vec<(u64, Vec<WriteRow>)>) -> Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        let mut pending = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?;
        let mut restored = batches;
        restored.append(&mut pending);
        *pending = restored;
        Ok(())
    }
}
