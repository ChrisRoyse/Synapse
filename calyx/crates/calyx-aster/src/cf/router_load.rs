//! Cold-open scan of a CF router's SST files: fail-closed name
//! classification, seq-domain-safe ordering (issue #1138), and the per-CF
//! flush-ordinal counter.

use super::ColumnFamily;
use super::router::CfRouter;
use crate::sst::level::SstLevel;
use crate::storage_names::{
    SstName, classify_sst, ensure_unambiguous_sst_order, parse_cf_dir_name, sst_order_key,
};
use calyx_core::{CalyxError, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ROUTER_LOAD_PROGRESS_FILE_INTERVAL: usize = 10_000;

impl CfRouter {
    pub(super) fn load_existing_with_lookup_policy(
        &mut self,
        eager_lookup_on_open: bool,
    ) -> Result<()> {
        let started_at = Instant::now();
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_LOAD_START",
            vault_dir = %self.vault_dir().display(),
            eager_lookup_on_open,
            selected_cfs = false,
            "starting Calyx CF router load"
        );
        let mut by_cf = HashMap::<ColumnFamily, Vec<PathBuf>>::new();
        let mut cf_dirs_scanned = 0_usize;
        let mut sst_files_discovered = 0_usize;
        for cf_root in self.cf_roots() {
            if !cf_root.exists() {
                continue;
            }
            for entry in fs::read_dir(cf_root)
                .map_err(|error| CalyxError::disk_pressure(format!("read CF root: {error}")))?
            {
                let path = entry
                    .map_err(|error| CalyxError::disk_pressure(format!("read CF entry: {error}")))?
                    .path();
                if !path.is_dir() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .ok_or_else(|| {
                        CalyxError::aster_corrupt_shard(format!(
                            "CF directory entry {} has no name",
                            path.display()
                        ))
                    })?;
                let cf = parse_cf_dir_name(&name)?;
                cf_dirs_scanned += 1;
                let files = list_sst_files(&path)?;
                sst_files_discovered = sst_files_discovered.saturating_add(files.len());
                if sst_files_discovered >= ROUTER_LOAD_PROGRESS_FILE_INTERVAL
                    && sst_files_discovered % ROUTER_LOAD_PROGRESS_FILE_INTERVAL < files.len()
                {
                    tracing::info!(
                        code = "CALYX_ASTER_ROUTER_LOAD_DISCOVERY_PROGRESS",
                        vault_dir = %self.vault_dir().display(),
                        cf_dirs_scanned,
                        sst_files_discovered,
                        eager_lookup_on_open,
                        "Calyx CF router SST discovery progress"
                    );
                }
                by_cf.entry(cf).or_default().extend(files);
            }
        }
        let mut cfs_loaded = 0_usize;
        let mut sst_files_loaded = 0_usize;
        for (cf, files) in by_cf {
            let file_count = files.len();
            self.load_cf_level(
                cf,
                files,
                should_build_eager_lookup_on_open(cf, eager_lookup_on_open),
            )?;
            cfs_loaded += 1;
            sst_files_loaded = sst_files_loaded.saturating_add(file_count);
            tracing::debug!(
                code = "CALYX_ASTER_ROUTER_LOAD_CF_DONE",
                vault_dir = %self.vault_dir().display(),
                cf = cf.name(),
                file_count,
                cfs_loaded,
                sst_files_loaded,
                eager_lookup_on_open,
                elapsed_ms = started_at.elapsed().as_millis(),
                "loaded Calyx CF router level"
            );
        }
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_LOAD_DONE",
            vault_dir = %self.vault_dir().display(),
            cf_dirs_scanned,
            cfs_loaded,
            sst_files_discovered,
            sst_files_loaded,
            eager_lookup_on_open,
            elapsed_ms = started_at.elapsed().as_millis(),
            "completed Calyx CF router load"
        );
        Ok(())
    }

    pub(crate) fn load_existing_cfs_with_lookup_policy(
        &mut self,
        cfs: &[ColumnFamily],
        eager_lookup_on_open: bool,
    ) -> Result<()> {
        let started_at = Instant::now();
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_LOAD_START",
            vault_dir = %self.vault_dir().display(),
            eager_lookup_on_open,
            selected_cfs = true,
            selected_cf_count = cfs.len(),
            "starting selected Calyx CF router load"
        );
        let mut by_cf = HashMap::<ColumnFamily, Vec<PathBuf>>::new();
        let mut cf_dirs_scanned = 0_usize;
        let mut sst_files_discovered = 0_usize;
        for cf_root in self.cf_roots() {
            for cf in cfs {
                let cf_dir = cf_root.join(cf.name());
                if cf_dir.exists() {
                    cf_dirs_scanned += 1;
                    let files = list_sst_files(&cf_dir)?;
                    sst_files_discovered = sst_files_discovered.saturating_add(files.len());
                    if sst_files_discovered >= ROUTER_LOAD_PROGRESS_FILE_INTERVAL
                        && sst_files_discovered % ROUTER_LOAD_PROGRESS_FILE_INTERVAL < files.len()
                    {
                        tracing::info!(
                            code = "CALYX_ASTER_ROUTER_LOAD_DISCOVERY_PROGRESS",
                            vault_dir = %self.vault_dir().display(),
                            cf_dirs_scanned,
                            sst_files_discovered,
                            eager_lookup_on_open,
                            selected_cfs = true,
                            "Calyx selected CF router SST discovery progress"
                        );
                    }
                    by_cf.entry(*cf).or_default().extend(files);
                }
            }
        }
        let mut cfs_loaded = 0_usize;
        let mut sst_files_loaded = 0_usize;
        for cf in cfs {
            let files = by_cf.remove(cf).unwrap_or_default();
            let file_count = files.len();
            // A selected-CF open with eager lookup explicitly requests that
            // every selected surface be pageable. The full-vault policy below
            // remains selective to avoid retaining every low-volume index.
            let retain_lookup = *cf == ColumnFamily::Kv || eager_lookup_on_open;
            self.load_cf_level(*cf, files, retain_lookup)?;
            cfs_loaded += 1;
            sst_files_loaded = sst_files_loaded.saturating_add(file_count);
            tracing::debug!(
                code = "CALYX_ASTER_ROUTER_LOAD_CF_DONE",
                vault_dir = %self.vault_dir().display(),
                cf = cf.name(),
                file_count,
                cfs_loaded,
                sst_files_loaded,
                eager_lookup_on_open,
                selected_cfs = true,
                elapsed_ms = started_at.elapsed().as_millis(),
                "loaded selected Calyx CF router level"
            );
        }
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_LOAD_DONE",
            vault_dir = %self.vault_dir().display(),
            cf_dirs_scanned,
            cfs_loaded,
            sst_files_discovered,
            sst_files_loaded,
            eager_lookup_on_open,
            selected_cfs = true,
            elapsed_ms = started_at.elapsed().as_millis(),
            "completed selected Calyx CF router load"
        );
        Ok(())
    }

    fn load_cf_level(
        &mut self,
        cf: ColumnFamily,
        mut files: Vec<PathBuf>,
        retain_lookup: bool,
    ) -> Result<()> {
        sort_ssts_by_sequence(&mut files)?;
        files.dedup();
        // Only router-flushed SSTs (legacy and commit-anchored shapes)
        // participate in the next-file ordinal counter; durable batches and
        // compaction outputs use disjoint name shapes.
        let next = files
            .iter()
            .filter_map(|file| match classify_sst(file) {
                Ok(Some(SstName::RouterLegacy { ordinal })) => Some(ordinal),
                Ok(Some(SstName::Flush { ordinal, .. })) => Some(ordinal as u64),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1;
        self.ensure_cf(cf)?;
        self.levels.insert(
            cf,
            if retain_lookup {
                SstLevel::from_oldest_first_with_lookup(files)?
            } else {
                SstLevel::from_oldest_first(files)
            },
        );
        self.next_file.insert(cf, next);
        Ok(())
    }
}

fn should_build_eager_lookup_on_open(cf: ColumnFamily, eager_lookup_on_open: bool) -> bool {
    // Candidate-bounded paging is a hard contract for the shared Synapse KV
    // namespace. Its page path must never reopen and whole-file CRC-scan SSTs,
    // so retain validated key/offset metadata even in latest-readback mode.
    if cf == ColumnFamily::Kv {
        return true;
    }
    if !eager_lookup_on_open {
        return false;
    }
    // Base and slot CFs are the high-volume point-read surfaces. Retain their
    // exact validated key/offset indexes once so Bloom candidates never
    // reopen and whole-file validate large immutable SSTs per requested row.
    matches!(cf, ColumnFamily::Base) || cf.is_slot()
}

/// Lists SST files in a CF directory, failing closed on any `*.sst` file
/// whose name matches no canonical writer shape (such files were previously
/// loaded into levels while being invisible to the next-file counter).
pub(super) fn list_sst_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)
        .map_err(|error| CalyxError::disk_pressure(format!("read CF dir: {error}")))?
    {
        let path = entry
            .map_err(|error| CalyxError::disk_pressure(format!("read CF file: {error}")))?
            .path();
        if sst_order_key(&path)?.is_some() {
            files.push(path);
        }
    }
    Ok(files)
}

/// Sorts one CF's SST files into newest-wins read order, failing closed when
/// legacy flush ordinals and commit-domain seqs overlap (issue #1138) —
/// serving reads over an ambiguous order would silently return stale rows.
pub(super) fn sort_ssts_by_sequence(files: &mut [PathBuf]) -> Result<()> {
    ensure_unambiguous_sst_order(files.iter().map(PathBuf::as_path))?;
    let mut keyed = files
        .iter()
        .map(|path| {
            Ok((
                sst_order_key(path)?.ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "non-SST path {} in CF level",
                        path.display()
                    ))
                })?,
                path.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (slot, (_, path)) in files.iter_mut().zip(keyed) {
        *slot = path;
    }
    Ok(())
}
