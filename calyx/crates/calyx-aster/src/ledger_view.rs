//! Read-only Ledger column-family view over an Aster vault directory.
//!
//! Merges the on-disk `cf/ledger` SSTs with any unflushed WAL records into a
//! [`LedgerCfStore`] suitable for `calyx_ledger::verify_chain`. The view takes
//! the durable commit lock while copying rows and the head anchor so concurrent
//! writers cannot expose a mixed-time snapshot. It remains ledger-read-only:
//! any append attempt is a `CALYX_LEDGER_APPEND_ONLY_VIOLATION`.

mod point_read;
mod query_index;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, Result as CalyxResult};
use calyx_ledger::{
    LedgerCfStore, LedgerEntry, LedgerHeadAnchor, LedgerRow, LedgerSnapshot, decode,
};

use crate::cf::{CfRouter, ColumnFamily};
use crate::compaction::TieringPolicy;
use crate::manifest::ManifestStore;
use crate::sst::SstEntry;
use crate::vault::encode::decode_write_batch;
use crate::wal::{replay_dir_after, stream_records};
pub use point_read::{LedgerPointReadTierStats, LedgerPointReadTrace};
use point_read::{read_complete_sst_ledger_rows, read_sst_ledger_rows, unresolved_seqs};
pub use query_index::{LedgerQueryOpenStats, LedgerQuerySnapshot, LedgerQueryVisitStats};

/// Read-only snapshot of a vault's Ledger column family (SSTs + WAL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsterLedgerCfStore {
    rows: Vec<LedgerRow>,
    anchor: Option<LedgerHeadAnchor>,
}

/// Physical work performed by one stable reverse-ledger query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LedgerReverseReadStats {
    pub snapshot_height: u64,
    pub batches_read: u64,
    pub rows_visited: u64,
}

/// Visits Ledger rows from newest to oldest while holding the durable commit
/// lock for the complete query. SST, active-WAL, and retained-WAL duplicates
/// retain the same byte-equality and torn-tail checks as point reads.
///
/// Returning `true` from `visit` stops the query. At most `batch_size - 1`
/// extra physical rows are fetched beyond the logical stop point.
pub fn visit_ledger_reverse(
    vault: &Path,
    batch_size: usize,
    mut visit: impl FnMut(&LedgerEntry) -> CalyxResult<bool>,
) -> CalyxResult<LedgerReverseReadStats> {
    if batch_size == 0 {
        return Err(CalyxError::ledger_corrupt(
            "reverse ledger query batch_size must be > 0",
        ));
    }
    let _commit_guard = crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
    let anchor = crate::ledger_head::read_head_anchor(vault)?;
    let Some(anchor) = anchor else {
        let store =
            AsterLedgerCfStore::open_with_layout(vault, AsterVaultLayout::read(vault)?, None)?;
        if store.rows.is_empty() {
            return Ok(LedgerReverseReadStats::default());
        }
        return Err(CalyxError::ledger_corrupt(
            "ledger rows exist without a durable head anchor",
        ));
    };
    let mut stats = LedgerReverseReadStats {
        snapshot_height: anchor.height,
        ..LedgerReverseReadStats::default()
    };
    let mut end = anchor.height;
    while end != 0 {
        let start = end.saturating_sub(batch_size as u64);
        let wanted = (start..end).collect::<BTreeSet<_>>();
        let rows = read_ledger_seqs_unlocked_traced(vault, &wanted, None)?.0;
        stats.batches_read += 1;
        for seq in (start..end).rev() {
            let row = rows.get(&seq).ok_or_else(|| {
                CalyxError::ledger_chain_broken(format!(
                    "reverse ledger snapshot at height {} is missing seq {seq}",
                    anchor.height
                ))
            })?;
            let entry = decode(&row.bytes)?;
            if entry.seq != seq {
                return Err(CalyxError::ledger_corrupt(format!(
                    "reverse ledger row key {seq} does not match encoded seq {}",
                    entry.seq
                )));
            }
            if seq + 1 == anchor.height && entry.entry_hash != anchor.tip_hash {
                return Err(CalyxError::ledger_chain_broken(format!(
                    "reverse ledger head hash mismatch at seq {seq}"
                )));
            }
            stats.rows_visited += 1;
            if visit(&entry)? {
                return Ok(stats);
            }
        }
        end = start;
    }
    Ok(stats)
}

impl AsterLedgerCfStore {
    /// Opens the Ledger CF of the vault at `vault`, failing closed when the
    /// directory holds no real Aster ledger state.
    pub fn open(vault: &Path) -> CalyxResult<Self> {
        let layout = AsterVaultLayout::read(vault)?;
        let _commit_guard =
            crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
        Self::open_with_layout(vault, layout, None)
    }

    pub(crate) fn open_unlocked_with_tiering(
        vault: &Path,
        tiering_policy: Option<&TieringPolicy>,
    ) -> CalyxResult<Self> {
        let layout = AsterVaultLayout::read_with_tiering(vault, tiering_policy)?;
        Self::open_with_layout(vault, layout, tiering_policy)
    }

    fn open_with_layout(
        vault: &Path,
        layout: AsterVaultLayout,
        tiering_policy: Option<&TieringPolicy>,
    ) -> CalyxResult<Self> {
        let mut rows = BTreeMap::new();

        if !layout.ledger_cf_dirs.is_empty() {
            let router = CfRouter::open_selected_cfs_with_tiering(
                vault,
                0,
                [ColumnFamily::Ledger],
                tiering_policy.cloned(),
            )?;
            for entry in router.iter_cf(ColumnFamily::Ledger)? {
                insert_sst_entry(&mut rows, entry)?;
            }
        }

        if layout.has_wal {
            let replay = replay_dir_after(vault.join("wal"), layout.wal_replay_floor_seq)?;
            if let Some(torn) = replay.torn_tail {
                return Err(torn.error());
            }
            for record in replay.records {
                for row in decode_write_batch(&record.payload)? {
                    if row.cf == ColumnFamily::Ledger {
                        let seq = parse_aster_ledger_seq(&row.key)?;
                        insert_ledger_bytes(&mut rows, seq, row.value)?;
                    }
                }
            }
        }

        let rows = rows
            .into_iter()
            .map(|(seq, bytes)| LedgerRow { seq, bytes })
            .collect::<Vec<_>>();
        let anchor = crate::ledger_head::require_head_anchor_for_rows(
            vault,
            crate::ledger_head::read_head_anchor(vault)?,
            &rows,
        )?;
        Ok(Self { anchor, rows })
    }
}

/// Reads one Ledger CF row from a fresh physical view of `vault`.
///
/// This is the point-read counterpart to [`AsterLedgerCfStore::open`]: it takes
/// the same durable commit lock and merges SST plus WAL state, but it only
/// materializes the requested ledger sequence instead of cloning the full
/// ledger into memory.
pub fn read_ledger_seq(vault: &Path, seq: u64) -> CalyxResult<Option<LedgerRow>> {
    let wanted = BTreeSet::from([seq]);
    Ok(read_ledger_seqs(vault, &wanted)?.remove(&seq))
}

/// Reads a targeted set of Ledger CF rows from one stable physical snapshot.
///
/// This is the batch point-read counterpart to [`AsterLedgerCfStore::open`].
/// It takes the durable commit lock, reads physical Ledger SSTs first, and only
/// replays WAL rows for requested sequences not present in immutable SSTs.
/// Every physical duplicate observed on the rows it reads must match byte-for-byte.
pub fn read_ledger_seqs(
    vault: &Path,
    seqs: &BTreeSet<u64>,
) -> CalyxResult<BTreeMap<u64, LedgerRow>> {
    Ok(read_ledger_seqs_traced(vault, seqs)?.0)
}

/// [`read_ledger_seqs`] plus the per-tier resolution trace (#1112) so callers
/// can log — and FSV can assert — which point-read tier resolved the rows and
/// what it cost.
pub fn read_ledger_seqs_traced(
    vault: &Path,
    seqs: &BTreeSet<u64>,
) -> CalyxResult<(BTreeMap<u64, LedgerRow>, LedgerPointReadTrace)> {
    if seqs.is_empty() {
        return Ok((BTreeMap::new(), LedgerPointReadTrace::default()));
    }
    let _commit_guard = crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
    read_ledger_seqs_unlocked_traced(vault, seqs, None)
}

pub(crate) fn read_ledger_seqs_unlocked_traced(
    vault: &Path,
    seqs: &BTreeSet<u64>,
    tiering_policy: Option<&TieringPolicy>,
) -> CalyxResult<(BTreeMap<u64, LedgerRow>, LedgerPointReadTrace)> {
    let mut trace = LedgerPointReadTrace::default();
    if seqs.is_empty() {
        return Ok((BTreeMap::new(), trace));
    }
    let layout = AsterVaultLayout::read_with_tiering(vault, tiering_policy)?;
    let mut rows = BTreeMap::new();
    if !layout.ledger_cf_dirs.is_empty() {
        read_sst_ledger_rows(&layout.ledger_cf_dirs, seqs, &mut rows, &mut trace)?;
    }
    let unresolved = unresolved_seqs(seqs, &rows);
    if layout.has_wal && !unresolved.is_empty() {
        let started = std::time::Instant::now();
        let before = rows.len();
        read_wal_ledger_rows(vault, &unresolved, &mut rows)?;
        trace.record(
            "wal_tail",
            unresolved.len(),
            rows.len() - before,
            0,
            started,
        );
    }
    let unresolved = unresolved_seqs(seqs, &rows);
    if !layout.ledger_cf_dirs.is_empty() && !unresolved.is_empty() {
        read_complete_sst_ledger_rows(&layout.ledger_cf_dirs, &unresolved, &mut rows, &mut trace)?;
    }
    Ok((
        rows.into_iter()
            .map(|(seq, bytes)| (seq, LedgerRow { seq, bytes }))
            .collect(),
        trace,
    ))
}

fn read_wal_ledger_rows(
    vault: &Path,
    wanted: &BTreeSet<u64>,
    rows: &mut BTreeMap<u64, Vec<u8>>,
) -> CalyxResult<()> {
    read_wal_ledger_rows_after_floor(vault, wanted, rows)?;
    let unresolved = unresolved_seqs(wanted, rows);
    if !unresolved.is_empty() {
        read_retained_wal_ledger_rows(vault, &unresolved, rows)?;
    }
    Ok(())
}

fn read_wal_ledger_rows_after_floor(
    vault: &Path,
    wanted: &BTreeSet<u64>,
    rows: &mut BTreeMap<u64, Vec<u8>>,
) -> CalyxResult<()> {
    let replay = replay_dir_after(vault.join("wal"), wal_replay_floor_seq(vault)?)?;
    if let Some(torn) = replay.torn_tail {
        return Err(torn.error());
    }
    for record in replay.records {
        for write in decode_write_batch(&record.payload)? {
            if write.cf != ColumnFamily::Ledger {
                continue;
            }
            let seq = parse_aster_ledger_seq(&write.key)?;
            if wanted.contains(&seq) {
                insert_ledger_bytes(rows, seq, write.value)?;
            }
        }
    }
    Ok(())
}

fn read_retained_wal_ledger_rows(
    vault: &Path,
    wanted: &BTreeSet<u64>,
    rows: &mut BTreeMap<u64, Vec<u8>>,
) -> CalyxResult<()> {
    stream_records(vault.join("wal"), |record| {
        for write in decode_write_batch(&record.payload)? {
            if write.cf != ColumnFamily::Ledger {
                continue;
            }
            let seq = parse_aster_ledger_seq(&write.key)?;
            if wanted.contains(&seq) {
                insert_ledger_bytes(rows, seq, write.value)?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn durable_commit_lock_path(vault: &Path) -> PathBuf {
    vault.join("locks").join("durable.commit.lock")
}

impl LedgerCfStore for AsterLedgerCfStore {
    fn scan(&self) -> CalyxResult<Vec<LedgerRow>> {
        Ok(self.rows.clone())
    }

    fn snapshot(&self) -> CalyxResult<LedgerSnapshot<'_>> {
        Ok(LedgerSnapshot::borrowed(&self.rows, self.anchor.as_ref()))
    }

    fn read_seq(&self, seq: u64) -> CalyxResult<Option<LedgerRow>> {
        Ok(self.rows.iter().find(|row| row.seq == seq).cloned())
    }

    fn put_new(&mut self, seq: u64, _bytes: &[u8]) -> CalyxResult<()> {
        Err(CalyxError::ledger_append_only_violation(format!(
            "read-only Aster ledger view rejected append for seq {seq}"
        )))
    }

    fn head_anchor(&self) -> CalyxResult<Option<LedgerHeadAnchor>> {
        Ok(self.anchor.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AsterVaultLayout {
    ledger_cf_dirs: Vec<PathBuf>,
    has_wal: bool,
    wal_replay_floor_seq: u64,
}

impl AsterVaultLayout {
    fn read(vault: &Path) -> CalyxResult<Self> {
        Self::read_with_tiering(vault, None)
    }

    fn read_with_tiering(
        vault: &Path,
        tiering_policy: Option<&TieringPolicy>,
    ) -> CalyxResult<Self> {
        if !vault.is_dir() {
            return Err(CalyxError::ledger_corrupt(format!(
                "vault path {} is not an Aster vault directory",
                vault.display()
            )));
        }

        let layout = Self {
            ledger_cf_dirs: ledger_cf_dirs(vault, tiering_policy)?,
            has_wal: vault.join("wal").is_dir(),
            wal_replay_floor_seq: wal_replay_floor_seq(vault)?,
        };
        if layout.ledger_cf_dirs.is_empty() && !layout.has_wal {
            return Err(CalyxError::ledger_corrupt(format!(
                "vault requires real Aster ledger state under {}/cf/ledger, tiered CF roots, or {}/wal",
                vault.display(),
                vault.display()
            )));
        }
        Ok(layout)
    }
}

fn ledger_cf_dirs(
    vault: &Path,
    tiering_policy: Option<&TieringPolicy>,
) -> CalyxResult<Vec<PathBuf>> {
    let mut roots = vec![vault.join("cf")];
    if let Some(policy) = tiering_policy {
        for tier_root in policy.tier_roots() {
            let cf_root = tier_root.join("cf");
            if !roots.contains(&cf_root) {
                roots.push(cf_root);
            }
        }
    }

    let mut dirs = Vec::new();
    for root in roots {
        let dir = root.join(ColumnFamily::Ledger.name());
        match std::fs::metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => dirs.push(dir),
            Ok(_) => {
                return Err(CalyxError::ledger_corrupt(format!(
                    "ledger CF path {} exists but is not a directory",
                    dir.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CalyxError::disk_pressure(format!(
                    "stat ledger CF path {}: {error}",
                    dir.display()
                )));
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn wal_replay_floor_seq(vault: &Path) -> CalyxResult<u64> {
    if !vault.join("CURRENT").exists() {
        return Ok(0);
    }
    Ok(ManifestStore::open(vault).load_current()?.durable_seq)
}

fn insert_sst_entry(rows: &mut BTreeMap<u64, Vec<u8>>, entry: SstEntry) -> CalyxResult<()> {
    let seq = parse_aster_ledger_seq(&entry.key)?;
    insert_ledger_bytes(rows, seq, entry.value)
}

fn insert_ledger_bytes(
    rows: &mut BTreeMap<u64, Vec<u8>>,
    seq: u64,
    bytes: Vec<u8>,
) -> CalyxResult<()> {
    if let Some(existing) = rows.get(&seq) {
        if existing == &bytes {
            return Ok(());
        }
        return Err(CalyxError::ledger_corrupt(format!(
            "divergent Aster ledger bytes for seq {seq}"
        )));
    }
    rows.insert(seq, bytes);
    Ok(())
}

/// Parses a big-endian u64 Ledger CF key, failing closed on any other width.
pub fn parse_aster_ledger_seq(key: &[u8]) -> CalyxResult<u64> {
    let key: [u8; 8] = key.try_into().map_err(|_| {
        CalyxError::ledger_corrupt(format!(
            "Aster ledger CF key has {} bytes, expected 8",
            key.len()
        ))
    })?;
    Ok(u64::from_be_bytes(key))
}
