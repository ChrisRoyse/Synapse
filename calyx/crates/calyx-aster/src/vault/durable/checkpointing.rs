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

    pub(super) fn write_rows(&self, seq: u64, rows: &[WriteRow]) -> Result<()> {
        let mut by_cf = Vec::<(ColumnFamily, Vec<(usize, &WriteRow)>)>::new();
        for (index, row) in rows.iter().enumerate() {
            if let Some((_, group)) = by_cf.iter_mut().find(|(cf, _)| *cf == row.cf) {
                group.push((index, row));
            } else {
                by_cf.push((row.cf, vec![(index, row)]));
            }
        }
        by_cf.sort_by_key(|(cf, _)| cf.name());
        for (cf, rows) in by_cf {
            let rows = latest_rows_by_key(rows);
            let first_index = rows.first().map_or(0, |(index, _)| *index);
            let dir = self.cf_dir(cf);
            fs::create_dir_all(&dir).map_err(|error| storage_error("create CF dir", error))?;
            let path = dir.join(format!("{seq:020}-{first_index:04}.sst"));
            match &self.value_crypto {
                Some(context) => {
                    let entries = rows
                        .iter()
                        .map(|(_, row)| {
                            Ok((
                                row.key.clone(),
                                seal_value(context, row.cf, &row.key, &row.value)?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    write_sst(
                        &path,
                        entries
                            .iter()
                            .map(|(key, value)| (key.as_slice(), value.as_slice())),
                    )?;
                }
                None => {
                    let entries = rows
                        .iter()
                        .map(|(_, row)| (row.key.as_slice(), row.value.as_slice()));
                    write_sst(&path, entries)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn flush_pending_checkpoints(&self) -> Result<()> {
        let batches = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?
            .clone();
        if batches.is_empty() {
            return Ok(());
        }
        for (seq, rows) in &batches {
            self.write_rows(*seq, rows)?;
            self.advance_checkpointed_derived_content(*seq, rows);
        }
        let last_seq = batches.last().map_or(0, |(seq, _)| *seq);
        self.write_manifest(last_seq)?;
        let mut pending = self
            .pending_checkpoint
            .lock()
            .map_err(|_| CalyxError::disk_pressure("checkpoint staging lock poisoned"))?;
        pending.retain(|(seq, _)| *seq > last_seq);
        Ok(())
    }
}

fn latest_rows_by_key<'a>(rows: Vec<(usize, &'a WriteRow)>) -> Vec<(usize, &'a WriteRow)> {
    let mut latest = BTreeMap::<Vec<u8>, (usize, &'a WriteRow)>::new();
    for (index, row) in rows {
        latest.insert(row.key.clone(), (index, row));
    }
    latest.into_values().collect()
}
