//! Cold-open scan of a CF router's SST files: fail-closed name
//! classification, seq-domain-safe ordering (issue #1138), and the per-CF
//! flush-ordinal counter.

use super::ColumnFamily;
use super::router::{CfRouter, RouterShard};
use crate::sst::level::SstLevel;
use crate::storage_names::{
    SstName, classify_sst, ensure_unambiguous_sst_order, parse_cf_dir_name, sst_order_key,
};
use calyx_core::{CalyxError, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ROUTER_LOAD_PROGRESS_FILE_INTERVAL: usize = 10_000;

impl CfRouter {
    pub(super) fn load_existing_with_lookup_policy(
        &self,
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
        // A cold open discovers its column families from the filesystem, so
        // the lock set is not known until here — every shard, in index order.
        // This runs single-threaded at open; the exclusivity costs nothing and
        // keeps the "one writer installs a level" invariant total (#1950).
        let mut guards = self.write_all_shards()?;
        let mut cfs_loaded = 0_usize;
        let mut sst_files_loaded = 0_usize;
        for (cf, files) in by_cf {
            let file_count = files.len();
            let shard = guards.shard_mut(cf)?;
            self.load_cf_level(
                shard,
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
        &self,
        cfs: &[ColumnFamily],
        eager_lookup_on_open: bool,
    ) -> Result<()> {
        self.load_existing_cfs_excluding(cfs, eager_lookup_on_open, &BTreeSet::new())
    }

    /// Reloads the selected CF levels while treating `excluded` (canonical
    /// paths) as already gone.
    ///
    /// This is what lets a reclaim retire input SSTs from the served level
    /// *before* paying their deletion cost: the level installed here is
    /// byte-identical to the one a post-deletion rescan would produce, so the
    /// physical `remove_file` calls can run after the exclusive router lock is
    /// released instead of inside it (issue #1806).
    pub(crate) fn load_existing_cfs_excluding(
        &self,
        cfs: &[ColumnFamily],
        eager_lookup_on_open: bool,
        excluded: &BTreeSet<PathBuf>,
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
                    let mut files = list_sst_files(&cf_dir)?;
                    if !excluded.is_empty() {
                        // One canonicalize per directory, then pure path
                        // comparison: `list_sst_files` only returns entries
                        // directly under `cf_dir`, so joining the file name
                        // onto the canonical directory is exact.
                        let canonical_dir = fs::canonicalize(&cf_dir).map_err(|error| {
                            CalyxError::disk_pressure(format!(
                                "canonicalize CF dir {} for retired-input exclusion: {error}",
                                cf_dir.display()
                            ))
                        })?;
                        files.retain(|path| {
                            path.file_name()
                                .is_none_or(|name| !excluded.contains(&canonical_dir.join(name)))
                        });
                    }
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
        // Exactly the named families' shards, held for the whole load. This
        // is what `retire_then_purge_cf_inputs` needs in order to drain every
        // in-flight SST mapping for the CFs whose files it is about to unlink;
        // readers of any other family are untouched (#1806, #1950).
        let mut guards = self.write_shards_for(cfs.iter().copied())?;
        let mut cfs_loaded = 0_usize;
        let mut sst_files_loaded = 0_usize;
        for cf in cfs {
            let files = by_cf.remove(cf).unwrap_or_default();
            let file_count = files.len();
            // A selected-CF open with eager lookup explicitly requests that
            // every selected surface be pageable. The full-vault policy below
            // remains selective to avoid retaining every low-volume index.
            let retain_lookup = *cf == ColumnFamily::Kv || eager_lookup_on_open;
            let shard = guards.shard_mut(*cf)?;
            self.load_cf_level(shard, *cf, files, retain_lookup)?;
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

    /// Installs one CF's level through a shard guard the caller already holds.
    ///
    /// Takes the guard rather than acquiring one so the whole selected-CF load
    /// is a single exclusive window per shard, which is what makes the
    /// retire-then-purge ordering safe.
    fn load_cf_level(
        &self,
        shard: &mut RouterShard,
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
        let level = if retain_lookup {
            SstLevel::from_oldest_first_with_lookup(files)?
        } else {
            SstLevel::from_oldest_first(files)
        };
        shard.ensure_cf(&self.config, cf)?;
        shard.levels.insert(cf, level);
        shard.next_file.insert(cf, next);
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
    // The single declaration of "which families may be paged" (#1973). Retaining
    // a lookup is exactly what makes paging possible, so this policy must not
    // hold an independent opinion about the set — it reads it.
    cf.supports_paged_scan()
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
