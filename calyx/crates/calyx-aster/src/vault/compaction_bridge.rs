use super::AsterVault;
use super::durable::DurableVault;
use crate::cf::ColumnFamily;
use crate::compaction::{
    CompactionCatalog, CompactionResult, CompactionScheduler, CompactionSchedulerOptions,
    CompactionThrottle, DEFAULT_COMPACTION_TARGET_BYTES, RollingSstWriter,
    catalog_from_vault_tiers, durable_compaction_slot_path,
};
use crate::mvcc::is_tombstone_value;
use crate::recurrence::{StoredRecurrenceRow, decode_recurrence_row};
use crate::sst::SstReader;
use crate::storage_names::{SstName, classify_sst};
use calyx_core::{CalyxError, Clock, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub struct VaultCompactionScheduler {
    catalog: Arc<CompactionCatalog>,
    scheduler: CompactionScheduler,
}

impl VaultCompactionScheduler {
    pub fn shard_count_for_cf(&self, cf: ColumnFamily) -> usize {
        self.catalog.shard_count_for_cf(cf)
    }

    pub fn stop(self) -> std::thread::Result<()> {
        self.scheduler.stop()
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn compaction_catalog(&self) -> Result<Option<Arc<CompactionCatalog>>> {
        let Some(durable) = &self.durable else {
            return Ok(None);
        };
        self.with_durable_commit_lock(|| {
            self.flush_locked()?;
            Ok(Some(Arc::new(catalog_from_vault_tiers(
                durable.root(),
                durable.tiering_policy(),
            )?)))
        })
    }

    pub fn compact_cf_once(&self, cf: ColumnFamily) -> Result<Option<CompactionResult>> {
        let Some(durable) = &self.durable else {
            return Ok(None);
        };
        self.with_durable_commit_lock(|| {
            self.flush_locked()?;
            let durable_seq = self.verified_durable_coverage_seq(durable)?;
            let catalog = catalog_from_vault_tiers(durable.root(), durable.tiering_policy())?;
            let output = durable.compaction_output_path(cf, durable_seq);
            let mut result = catalog
                .compact_cf(cf, output, CompactionThrottle::unlimited())
                .map(Some)?;
            if let Some(CompactionResult::Compacted(_report)) = &mut result {
                self.rows
                    .refresh_router_cfs_from_disk(&[cf], "refresh router after compaction")?;
            }
            if let Some(CompactionResult::Compacted(report)) = &mut result
                && cf == ColumnFamily::Recurrence
            {
                ensure_reclaim_outputs_manifest_bounded(&report.output_paths, durable_seq)?;
                report.reclaimed_input_files = self.rows.refresh_router_cfs_after_reclaim(
                    &[cf],
                    "reclaim recurrence compaction inputs",
                    || reclaim_recurrence_inputs(report),
                )?;
                self.rows.refresh_router_cfs_after_reclaim(
                    &[cf],
                    "rewrite recurrence tombstone compaction outputs",
                    || prune_recurrence_tombstones(report),
                )?;
            }
            Ok(result)
        })
    }

    /// Manifest durable coverage after a locked flush; fails closed when the
    /// manifest does not cover the latest committed seq, because naming a
    /// compaction output beyond `durable_seq` makes full-restore readback
    /// skip it while its inputs get reclaimed (issue #1132).
    pub(super) fn verified_durable_coverage_seq(&self, durable: &DurableVault) -> Result<u64> {
        let durable_seq = durable.manifest_durable_seq()?;
        let latest = self.latest_seq();
        if durable_seq < latest {
            return Err(CalyxError {
                code: "CALYX_ASTER_COMPACTION_COVERAGE_GAP",
                message: format!(
                    "manifest durable_seq {durable_seq} does not cover latest committed seq {latest} after flush; compacting now would strand rows invisible to full-restore opens"
                ),
                remediation: "flush the vault and retry; if the gap persists, the WAL tail was not re-staged for checkpointing — report with vault MANIFEST and wal/ listing",
            });
        }
        Ok(durable_seq)
    }

    /// Compacts the listed column families, prunes MVCC tombstone rows from the
    /// compacted SST, and reclaims superseded input SSTs for durable vaults.
    pub fn purge_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()> {
        self.with_durable_commit_lock(|| self.purge_tombstoned_cfs_locked(cfs))
    }

    pub(crate) fn purge_tombstoned_cfs_locked(&self, cfs: &[ColumnFamily]) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Ok(());
        };
        self.flush_locked()?;
        let durable_seq = self.verified_durable_coverage_seq(durable)?;
        let mut unique = Vec::new();
        for cf in cfs {
            if !unique.contains(cf) {
                unique.push(*cf);
            }
        }
        for cf in unique {
            let Some(report) = prepare_tombstoned_cf_compaction(
                durable.root(),
                durable.tiering_policy(),
                cf,
                durable_seq,
            )?
            else {
                continue;
            };
            self.rows.refresh_router_cfs_after_reclaim(
                &[cf],
                "reclaim tombstoned compaction inputs",
                || reclaim_compaction_inputs(&report),
            )?;
        }
        Ok(())
    }

    pub fn start_compaction_scheduler(
        &self,
        mut options: CompactionSchedulerOptions,
    ) -> Result<Option<VaultCompactionScheduler>> {
        if let Some(durable) = &self.durable
            && options.output_root == CompactionSchedulerOptions::default().output_root
        {
            options.output_root = durable.root().join("cf");
        }
        if let Some(durable) = &self.durable {
            options.tiering_policy = options
                .tiering_policy
                .or_else(|| durable.tiering_policy().cloned());
        }
        let Some(catalog) = self.compaction_catalog()? else {
            return Ok(None);
        };
        let scheduler = CompactionScheduler::start(catalog.clone(), options);
        Ok(Some(VaultCompactionScheduler { catalog, scheduler }))
    }
}

fn prepare_tombstoned_cf_compaction(
    root: &Path,
    tiering_policy: Option<&crate::compaction::TieringPolicy>,
    cf: ColumnFamily,
    seq: u64,
) -> Result<Option<crate::compaction::CompactionReport>> {
    let catalog = catalog_from_vault_tiers(root, tiering_policy)?;
    let output = tiering_policy.map_or_else(
        || root.join("cf").join(cf.name()),
        |policy| policy.place_current_cf(cf).absolute_dir(),
    );
    let output = output.join(format!("compacted-{seq:020}.sst"));
    let mut result = catalog.compact_cf(cf, output, CompactionThrottle::unlimited())?;
    let CompactionResult::Compacted(report) = &mut result else {
        return Ok(None);
    };
    prune_mvcc_tombstones(report)?;
    ensure_reclaim_outputs_manifest_bounded(&report.output_paths, seq)?;
    Ok(Some(report.clone()))
}

/// Fails closed before input reclaim when the compaction output would not be
/// visible to full-restore readback (`seq > manifest durable_seq` is skipped
/// by `read_manifested_batches`), which would silently erase the merged rows
/// from every full-restore open once the inputs are deleted (issue #1132).
pub(super) fn ensure_reclaim_output_manifest_bounded(
    output_path: &Path,
    manifest_durable_seq: u64,
) -> Result<()> {
    let bounded = matches!(
        classify_sst(output_path)?,
        Some(SstName::Compacted { seq } | SstName::DurableBatch { seq, .. })
            if seq <= manifest_durable_seq
    );
    if bounded {
        return Ok(());
    }
    Err(CalyxError {
        code: "CALYX_ASTER_COMPACTION_COVERAGE_GAP",
        message: format!(
            "refusing to reclaim compaction inputs: output {} is not covered by manifest durable_seq {manifest_durable_seq}, so full-restore readback would silently skip the merged rows",
            output_path.display()
        ),
        remediation: "flush the vault so the manifest covers the compaction output seq, then retry; inputs were preserved",
    })
}

pub(super) fn ensure_reclaim_outputs_manifest_bounded(
    output_paths: &[PathBuf],
    manifest_durable_seq: u64,
) -> Result<()> {
    for output_path in output_paths {
        ensure_reclaim_output_manifest_bounded(output_path, manifest_durable_seq)?;
    }
    Ok(())
}

fn reclaim_compaction_inputs(report: &crate::compaction::CompactionReport) -> Result<usize> {
    let outputs = canonical_output_paths(report, "stat compacted SST")?;
    let mut reclaimed = 0;
    for input in &report.input_paths {
        let input = match fs::canonicalize(input) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if outputs.contains(&input) {
            continue;
        }
        if classify_sst(&input)?.is_none() {
            continue;
        }
        fs::remove_file(&input).map_err(|error| {
            CalyxError::disk_pressure(format!(
                "reclaim compaction input {}: {error}",
                input.display()
            ))
        })?;
        reclaimed += 1;
    }
    Ok(reclaimed)
}

fn reclaim_recurrence_inputs(report: &crate::compaction::CompactionReport) -> Result<usize> {
    let outputs = canonical_output_paths(report, "stat compacted SST")?;
    let parent = fs::canonicalize(&report.staging_parent)
        .map_err(|error| CalyxError::disk_pressure(format!("stat compaction parent: {error}")))?;
    let mut reclaimed = 0;
    for input in &report.input_paths {
        let input = match fs::canonicalize(input) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if outputs.contains(&input) {
            continue;
        }
        if input.parent() != Some(parent.as_path()) {
            continue;
        }
        if input.extension().and_then(|value| value.to_str()) != Some("sst") {
            continue;
        }
        fs::remove_file(&input).map_err(|error| {
            CalyxError::disk_pressure(format!(
                "reclaim recurrence compaction input {}: {error}",
                input.display()
            ))
        })?;
        reclaimed += 1;
    }
    Ok(reclaimed)
}

fn canonical_output_paths(
    report: &crate::compaction::CompactionReport,
    context: &str,
) -> Result<Vec<PathBuf>> {
    report
        .output_paths
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .map_err(|error| CalyxError::disk_pressure(format!("{context}: {error}")))
        })
        .collect()
}

fn prune_mvcc_tombstones(report: &mut crate::compaction::CompactionReport) -> Result<()> {
    rewrite_compacted_without(
        report,
        |value| Ok(is_tombstone_value(value)),
        "mvcc tombstone",
    )
}

fn prune_recurrence_tombstones(report: &mut crate::compaction::CompactionReport) -> Result<()> {
    rewrite_compacted_without(
        report,
        |value| {
            Ok(matches!(
                decode_recurrence_row(value)?,
                StoredRecurrenceRow::Tombstone { .. }
            ))
        },
        "recurrence tombstone",
    )
}

fn rewrite_compacted_without(
    report: &mut crate::compaction::CompactionReport,
    should_prune: impl Fn(&[u8]) -> Result<bool>,
    reason: &str,
) -> Result<()> {
    let mut pruned = 0_u64;
    let original_outputs = report.output_paths.clone();
    for output_path in &original_outputs {
        for entry in SstReader::open(output_path)?.iter()? {
            if should_prune(&entry.value)? {
                pruned += 1;
            }
        }
    }
    if pruned == 0 {
        return Ok(());
    }

    let seq = compaction_output_seq(&report.output_path)?;
    let reclaimed_path = durable_compaction_slot_path(&report.staging_parent, seq)?;
    let mut writer = RollingSstWriter::new(&reclaimed_path, DEFAULT_COMPACTION_TARGET_BYTES)?;
    let mut retained = 0_u64;
    let mut logical_bytes = 0_u64;
    for output_path in &original_outputs {
        for entry in SstReader::open(output_path)?.iter()? {
            if should_prune(&entry.value)? {
                continue;
            }
            logical_bytes = logical_bytes.saturating_add(entry.value.len() as u64);
            retained = retained.saturating_add(1);
            writer.push(entry.key, entry.value)?;
        }
    }
    let summaries = writer.finish(retained == 0)?;

    for output_path in &original_outputs {
        fs::remove_file(output_path).map_err(|error| {
            CalyxError::disk_pressure(format!(
                "remove {reason} compaction file {}: {error}",
                output_path.display()
            ))
        })?;
    }
    report.output_paths = summaries
        .iter()
        .map(|summary| summary.path.clone())
        .collect::<Vec<_>>();
    report.output_path = report
        .output_paths
        .first()
        .cloned()
        .ok_or_else(|| CalyxError::disk_pressure("tombstone rewrite produced no output SST"))?;
    report.output_bytes = summaries
        .iter()
        .map(|summary| summary.bytes)
        .fold(0_u64, u64::saturating_add);
    report.logical_bytes = logical_bytes;
    report.write_amp_milli =
        report.output_bytes.saturating_mul(1_000) / report.logical_bytes.max(1);
    report.debt_after = crate::compaction::CompactionDebt::measure(
        &summaries
            .iter()
            .map(|summary| crate::compaction::SstShard {
                cf: report.cf,
                path: summary.path.clone(),
                level: 0,
                bytes: summary.bytes,
            })
            .collect::<Vec<_>>(),
        DEFAULT_COMPACTION_TARGET_BYTES,
    );
    Ok(())
}

fn compaction_output_seq(path: &Path) -> Result<u64> {
    match classify_sst(path)? {
        Some(SstName::Compacted { seq } | SstName::DurableBatch { seq, .. }) => Ok(seq),
        Some(SstName::RouterLegacy { .. } | SstName::Flush { .. }) | None => {
            Err(CalyxError::aster_corrupt_shard(format!(
                "unexpected compacted SST name {}",
                path.display()
            )))
        }
    }
}
