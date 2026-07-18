use super::super::encode::WriteRow;
use super::{RecoveredBatch, storage_error};
use crate::compaction::TieringPolicy;
use crate::sst::SstReader;
use crate::storage_names::{SstName, classify_sst, parse_cf_dir_name};
use calyx_core::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFESTED_BATCH_PROGRESS_FILE_INTERVAL: usize = 10_000;

pub(super) fn read_manifested_batches(
    root: &Path,
    tiering_policy: Option<&TieringPolicy>,
    durable_seq: u64,
) -> Result<Vec<RecoveredBatch>> {
    let mut by_seq = BTreeMap::<u64, Vec<(usize, WriteRow)>>::new();
    if durable_seq == 0 {
        return Ok(Vec::new());
    }
    tracing::info!(
        code = "CALYX_ASTER_MANIFESTED_BATCHES_READ_START",
        vault_dir = %root.display(),
        durable_seq,
        "starting Calyx manifested durable-batch readback"
    );
    let mut sst_files_read = 0_usize;
    let mut rows_read = 0_usize;
    let mut bytes_read = 0_u64;
    for cf_root in tiered_cf_roots(root, tiering_policy) {
        if !cf_root.exists() {
            continue;
        }
        for entry in fs::read_dir(&cf_root).map_err(|error| storage_error("read CF root", error))? {
            let cf_dir = entry.map_err(|error| storage_error("read CF entry", error))?;
            if !cf_dir
                .file_type()
                .map_err(|error| storage_error("stat CF entry", error))?
                .is_dir()
            {
                continue;
            }
            let cf_name = cf_dir.file_name().to_string_lossy().to_string();
            let cf = parse_cf_dir_name(&cf_name)?;
            for file in
                fs::read_dir(cf_dir.path()).map_err(|error| storage_error("read CF dir", error))?
            {
                let path = file
                    .map_err(|error| storage_error("read SST entry", error))?
                    .path();
                let Some(name) = classify_sst(&path)? else {
                    continue;
                };
                let (seq, index) = match name {
                    SstName::DurableBatch { seq, index } => (seq, index),
                    SstName::Compacted { seq } => (seq, 0),
                    // Router memtable flushes (legacy and commit-anchored)
                    // hold merged latest-state rows whose exact per-row commit
                    // seqs are unknowable; they are recovered by
                    // `CfRouter::load_existing`, not by durable readback, and
                    // the router-coverage gate proves the restored state
                    // covers them (issue #1132).
                    SstName::RouterLegacy { .. } | SstName::Flush { .. } => continue,
                };
                if seq > durable_seq {
                    continue;
                }
                let file_len = fs::metadata(&path)
                    .map_err(|error| storage_error("stat SST for recovery readback", error))?
                    .len();
                let reader = SstReader::open(&path)?;
                for (row_offset, row) in reader.iter()?.into_iter().enumerate() {
                    by_seq.entry(seq).or_default().push((
                        index + row_offset,
                        WriteRow {
                            cf,
                            key: row.key,
                            value: row.value,
                        },
                    ));
                    rows_read += 1;
                }
                sst_files_read += 1;
                bytes_read = bytes_read.saturating_add(file_len);
                if sst_files_read.is_multiple_of(MANIFESTED_BATCH_PROGRESS_FILE_INTERVAL) {
                    tracing::info!(
                        code = "CALYX_ASTER_MANIFESTED_BATCHES_READ_PROGRESS",
                        vault_dir = %root.display(),
                        durable_seq,
                        sst_files_read,
                        rows_read,
                        bytes_read,
                        "Calyx manifested durable-batch readback progress"
                    );
                }
            }
        }
    }

    let batches = by_seq
        .into_iter()
        .map(|(seq, mut rows)| {
            rows.sort_by_key(|(index, _)| *index);
            RecoveredBatch {
                seq,
                rows: rows.into_iter().map(|(_, row)| row).collect(),
            }
        })
        .collect::<Vec<_>>();
    tracing::info!(
        code = "CALYX_ASTER_MANIFESTED_BATCHES_READ_DONE",
        vault_dir = %root.display(),
        durable_seq,
        sst_files_read,
        rows_read,
        bytes_read,
        batch_count = batches.len(),
        "completed Calyx manifested durable-batch readback"
    );
    Ok(batches)
}

/// Lists every on-disk CF that can feed the persistent search generation.
/// Legacy watermark migration must load all of these levels even when the
/// caller requested a narrower latest-read router, otherwise a slot-only
/// content commit could be silently omitted from the re-derived frontier.
pub(in crate::vault) fn persistent_search_cfs(
    root: &Path,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<Vec<crate::cf::ColumnFamily>> {
    let mut cfs = BTreeSet::new();
    for cf_root in tiered_cf_roots(root, tiering_policy) {
        if !cf_root.exists() {
            continue;
        }
        for entry in fs::read_dir(&cf_root).map_err(|error| storage_error("read CF root", error))? {
            let entry = entry.map_err(|error| storage_error("read CF entry", error))?;
            if !entry
                .file_type()
                .map_err(|error| storage_error("stat CF entry", error))?
                .is_dir()
            {
                continue;
            }
            let cf = parse_cf_dir_name(&entry.file_name().to_string_lossy())?;
            if cf.feeds_persistent_search_index() {
                cfs.insert(cf);
            }
        }
    }
    Ok(cfs.into_iter().collect())
}

pub(in crate::vault) fn tiered_cf_roots(
    root: &Path,
    tiering_policy: Option<&TieringPolicy>,
) -> Vec<PathBuf> {
    let mut roots = vec![root.join("cf")];
    if let Some(policy) = tiering_policy {
        for tier_root in policy.tier_roots() {
            let cf_root = tier_root.join("cf");
            if !roots.contains(&cf_root) {
                roots.push(cf_root);
            }
        }
    }
    roots
}
