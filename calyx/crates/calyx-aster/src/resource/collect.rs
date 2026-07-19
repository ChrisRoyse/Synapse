//! Collector assembling [`ResourceStatus`] from an open vault store + its directory.

use crate::compaction::{
    DEFAULT_COMPACTION_TARGET_BYTES, DEFAULT_COMPACTION_TARGET_FILES, catalog_from_vault_dir,
};
use crate::mvcc::VersionedCfStore;
use crate::resource::heap::heap_rss_bytes;
use crate::resource::status::{
    CfCompactionDebt, CompactionDebtStatus, HeapStatus, PinnedSeqStatus,
    RESOURCE_STATUS_SCHEMA_VERSION, ResourceStatus, VramBudgetStatus, WalStatus,
};
use calyx_core::{CalyxError, Result, Ts};
use std::fs;
use std::path::Path;

/// Collects the full resource status for one vault.
///
/// Every section is read from its physical source of truth at call time:
/// heap from `/proc/self/status`, compaction debt from the on-disk SST shard
/// set, WAL bytes from `wal/*.wal` segment files, pinned-seq and backpressure
/// from the live store. Any unreadable source fails the whole call — a
/// partial status would be a silent fallback.
pub fn collect_resource_status(
    vault_dir: &Path,
    vram: VramBudgetStatus,
    store: &VersionedCfStore,
    now: Ts,
) -> Result<ResourceStatus> {
    ensure_vault_dir(vault_dir)?;
    let heap = HeapStatus {
        rss_bytes: heap_rss_bytes()?,
    };
    let compaction = collect_compaction(vault_dir)?;
    let gc = store.snapshot_gc_metrics(now);
    let wal = collect_wal(vault_dir)?;
    let pinned = collect_pinned(store, now);
    let backpressure = store.resource_counters().snapshot();
    let memtable = store.memtable_status();
    Ok(ResourceStatus {
        schema_version: RESOURCE_STATUS_SCHEMA_VERSION,
        vault_dir: vault_dir.display().to_string(),
        collected_at: now,
        heap,
        memtable,
        vram,
        compaction,
        gc,
        pinned,
        backpressure,
        wal,
    })
}

fn ensure_vault_dir(vault_dir: &Path) -> Result<()> {
    if !vault_dir.is_dir() {
        return Err(CalyxError::disk_pressure(format!(
            "resource_status vault dir {} is not a directory",
            vault_dir.display()
        )));
    }
    if !vault_dir.join("cf").is_dir() {
        return Err(CalyxError::disk_pressure(format!(
            "resource_status vault dir {} has no cf/ root; not an Aster vault",
            vault_dir.display()
        )));
    }
    Ok(())
}

pub(crate) fn collect_compaction(vault_dir: &Path) -> Result<CompactionDebtStatus> {
    let catalog = catalog_from_vault_dir(vault_dir)?;
    let target_bytes = DEFAULT_COMPACTION_TARGET_BYTES;
    let mut per_cf = Vec::new();
    for cf in catalog.column_families() {
        let debt = catalog.debt_for_cf(cf, target_bytes);
        per_cf.push(CfCompactionDebt {
            cf: cf.name().to_string(),
            sst_files: catalog.shard_count_for_cf(cf),
            pending_bytes: debt.pending_bytes,
            byte_score_milli: debt.byte_score_milli,
            file_score_milli: debt.file_score_milli,
            score_milli: debt.score_milli,
        });
    }
    per_cf.sort_by(|left, right| left.cf.cmp(&right.cf));
    let total_pending_bytes = per_cf.iter().map(|cf| cf.pending_bytes).sum();
    let total_pending_files = per_cf.iter().map(|cf| cf.sst_files).sum();
    let max_score_milli = per_cf.iter().map(|cf| cf.score_milli).max().unwrap_or(0);
    Ok(CompactionDebtStatus {
        target_bytes,
        target_files: DEFAULT_COMPACTION_TARGET_FILES,
        total_pending_bytes,
        total_pending_files,
        max_score_milli,
        per_cf,
    })
}

pub(crate) fn collect_wal(vault_dir: &Path) -> Result<WalStatus> {
    let wal_dir = vault_dir.join("wal");
    if !wal_dir.is_dir() {
        return Ok(WalStatus {
            segment_count: 0,
            bytes: 0,
        });
    }
    let mut segment_count = 0;
    let mut bytes = 0u64;
    for entry in fs::read_dir(&wal_dir)
        .map_err(|error| CalyxError::disk_pressure(format!("read WAL dir: {error}")))?
    {
        let path = entry
            .map_err(|error| CalyxError::disk_pressure(format!("read WAL entry: {error}")))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("wal") {
            continue;
        }
        let len = fs::metadata(&path)
            .map_err(|error| CalyxError::disk_pressure(format!("stat WAL segment: {error}")))?
            .len();
        segment_count += 1;
        bytes = bytes.saturating_add(len);
    }
    Ok(WalStatus {
        segment_count,
        bytes,
    })
}

fn collect_pinned(store: &VersionedCfStore, now: Ts) -> PinnedSeqStatus {
    let current_seq = store.current_seq();
    let view = store.lease_view(now);
    PinnedSeqStatus {
        current_seq,
        oldest_pinned_seq: view.oldest_pinned_seq,
        oldest_pinned_seq_gap: view
            .oldest_pinned_seq
            .map_or(0, |oldest| current_seq.saturating_sub(oldest)),
        active_leases: view.active_leases,
        reader_lease_expired_total: view.reader_lease_expired_total,
    }
}
