use super::AsterVault;
use super::durable::DurableVault;
use crate::cf::ColumnFamily;
use crate::compaction::{
    CompactionCatalog, CompactionResult, CompactionScheduler, CompactionSchedulerOptions,
    CompactionThrottle, DEFAULT_COMPACTION_TARGET_BYTES, DEFAULT_COMPACTION_TARGET_FILES,
    RollingSstWriter, catalog_from_vault_tiers_through_seq, commit_domain_output_path,
    durable_compaction_slot_path,
};
use crate::mvcc::is_tombstone_value;
use crate::recurrence::{StoredRecurrenceRow, decode_recurrence_row};
use crate::sst::{invalidate_reader, shared_reader};
use crate::storage_names::{SstName, classify_sst, sst_order_key};
use calyx_core::{CalyxError, Clock, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Backstop against a fan-out reduction that never converges. Each pass makes
/// strictly positive progress (enforced below), so this can only be reached by
/// a genuinely pathological SST distribution.
const TOMBSTONE_PURGE_MAX_COMPACTION_PASSES: usize = 4096;

/// Maximum number of SST indexes one live compaction batch may retain.
///
/// The streaming reader drops each file mapping after validating and decoding
/// its index, so a high file ceiling is safe when the byte ceiling below is
/// also enforced. This lets one pass collapse pathological tiny-file fan-out
/// instead of preserving tens of thousands of point-read probes for hours.
pub const LIVE_COMPACTION_MAX_INPUT_FILES: usize = 32_768;
/// Maximum physical input bytes selected for one CF rewrite.
///
/// Selection is bounded by both bytes and files. The byte bound controls
/// checksum I/O and logical merge memory while the independent file bound
/// protects metadata/heap growth for extremely small SSTs.
pub const LIVE_COMPACTION_MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
/// Proactive native-CF fan-out trigger. This intentionally leaves 25% of the
/// range-page source budget as headroom for concurrent memtable flushes.
pub const LIVE_COMPACTION_TRIGGER_FILES: usize =
    crate::sst::MAX_INTERSECTING_SST_PAGE_SOURCES * 3 / 4;

/// Tiny-file fan-out admission floor (2026-07-23 cold-start fix). The debt
/// score (`pending_files * 1000 / DEFAULT_COMPACTION_TARGET_FILES`) reaches
/// the 1000-milli admission bar only at 256 files, so a CF made of hundreds
/// of ~114-byte per-batch checkpoint SSTs could sit below the bar forever —
/// the physical vault reached 7,801 SSTs across ~96 CFs (most CFs pinned at
/// 200-360 tiny files), inflating catalog scans, router loads, and recovery
/// preflight to minutes. A CF whose *average* file size is far below the
/// rolled-output target is pure fan-out debt regardless of its total bytes,
/// so it is admitted once it holds at least this many files.
pub const TINY_FILE_COMPACTION_MIN_FILES: usize = 32;

/// Average-file-size ceiling (bytes) below which a CF's fan-out counts as
/// tiny-file debt for `TINY_FILE_COMPACTION_MIN_FILES` admission. One MiB is
/// 1/64th of `DEFAULT_COMPACTION_TARGET_BYTES`: healthy rolled outputs never
/// average below it, per-batch checkpoint SSTs (bytes to a few KiB) always do.
pub const TINY_FILE_COMPACTION_AVG_BYTES_CEILING: u64 = 1024 * 1024;

/// Below-trigger ("routine") CFs folded per maintenance pass. The previous
/// single-CF routine lane could never drain a wide vault: with ~96 CFs and
/// one pass per 5-minute GC tick, a full sweep took ~8 hours while new
/// checkpoint SSTs accumulated faster — the backlog only ever grew. Eight per
/// pass drains the same vault in under an hour; each CF fold is bounded by
/// `LIVE_COMPACTION_MAX_INPUT_FILES`/`_BYTES` and runs off the commit lock.
pub const ROUTINE_COMPACTION_CFS_PER_PASS: usize = 8;

/// Whether a CF's pending fan-out qualifies as tiny-file debt: enough files to
/// matter and an average file size far below one rolled output.
fn is_tiny_file_fanout_debt(pending_files: usize, pending_bytes: u64) -> bool {
    if pending_files < TINY_FILE_COMPACTION_MIN_FILES {
        return false;
    }
    let files = u64::try_from(pending_files).unwrap_or(u64::MAX).max(1);
    pending_bytes / files < TINY_FILE_COMPACTION_AVG_BYTES_CEILING
}

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
        self.drain_checkpoints_paced("compaction catalog snapshot")?;
        let durable_seq =
            self.with_durable_commit_lock(|| self.published_compaction_snapshot_seq(durable))?;
        Ok(Some(Arc::new(catalog_from_vault_tiers_through_seq(
            durable.root(),
            durable.tiering_policy(),
            durable_seq,
        )?)))
    }

    pub fn compact_cf_once(&self, cf: ColumnFamily) -> Result<Option<CompactionResult>> {
        self.compact_cf_once_bounded(cf, LIVE_COMPACTION_MAX_INPUT_FILES)
    }

    pub fn compact_cf_once_bounded(
        &self,
        cf: ColumnFamily,
        max_input_files: usize,
    ) -> Result<Option<CompactionResult>> {
        if max_input_files < 2 {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "live compaction requires max_input_files >= 2, got {max_input_files}"
            )));
        }
        let Some(durable) = &self.durable else {
            return Ok(None);
        };
        let _maintenance_guard = try_acquire_native_compaction_guard(durable)?;
        // Drain the staged checkpoint backlog in bounded commit-lock holds
        // BEFORE taking the lock for the coverage snapshot (issue #1806); the
        // acquisition below then only has to absorb what was committed during
        // that paced drain.
        self.drain_checkpoints_paced("single-CF compaction preflight")?;
        let commit_lock_started = std::time::Instant::now();
        let durable_seq =
            self.with_durable_commit_lock(|| self.published_compaction_snapshot_seq(durable))?;
        let commit_lock_hold_us = commit_lock_started.elapsed().as_micros();
        let catalog_started = std::time::Instant::now();
        let catalog = catalog_from_vault_tiers_through_seq(
            durable.root(),
            durable.tiering_policy(),
            durable_seq,
        )?;
        tracing::info!(
            code = "CALYX_ASTER_NATIVE_COMPACTION_SNAPSHOT_READY",
            durable_seq,
            commit_lock_hold_us,
            catalog_scan_ms = catalog_started.elapsed().as_millis(),
            cf = cf.name(),
            "captured native compaction input view without retaining the durable commit lock"
        );
        self.compact_catalog_cf_batch(durable, &catalog, durable_seq, cf, max_input_files)
            .map(Some)
    }

    /// Runs one owned native-CF fan-out maintenance pass.
    ///
    /// Each physical rewrite remains independently file/byte bounded. The
    /// pass drains every CF at or above the proactive source trigger because
    /// servicing only one globally largest CF provides no readiness guarantee
    /// to the others. One lower-debt CF may also receive routine maintenance.
    /// Total CF bytes are telemetry, not debt: this flat immutable-file layer
    /// cannot reduce a healthy large dataset merely by rewriting it.
    pub fn compact_native_fanout_once(&self) -> Result<Vec<CompactionResult>> {
        self.compact_fanout_pass(false)
    }

    /// Open-time readiness pass (#1812): compacts ONLY the CFs whose file
    /// count has reached `LIVE_COMPACTION_TRIGGER_FILES` — the same threshold
    /// at which `ensure_cf_write_fanout_admitted` refuses writes
    /// (`CALYX_ASTER_SST_FANOUT_WRITE_STALL`) — and still verifies every CF
    /// ends below the hard range-page source limit. This is the minimum
    /// maintenance a booting daemon needs to accept its first write (the
    /// activity recorder writes CF_TIMELINE immediately at startup); the
    /// routine and tiny-file drain lanes stay owned by the periodic GC task,
    /// so a steady-state boot pays only the catalog scan here.
    pub fn compact_write_stall_readiness_once(&self) -> Result<Vec<CompactionResult>> {
        self.compact_fanout_pass(true)
    }

    fn compact_fanout_pass(&self, readiness_only: bool) -> Result<Vec<CompactionResult>> {
        let Some(durable) = &self.durable else {
            return Ok(Vec::new());
        };
        let _maintenance_guard = try_acquire_native_compaction_guard(durable)?;
        // Issue #1806: this pass already performed its physical rewrites off
        // the durable commit lock, but its *preflight* checkpoint drained the
        // entire staged backlog inside one acquisition — the actual 469 s /
        // 86 s holds attributed to `CALYX_ASTER_NATIVE_FANOUT`. Pace that
        // drain first so the snapshot acquisition below is short.
        self.drain_checkpoints_paced(if readiness_only {
            "write-stall readiness preflight"
        } else {
            "native fan-out maintenance preflight"
        })?;
        let commit_lock_started = std::time::Instant::now();
        let durable_seq =
            self.with_durable_commit_lock(|| self.published_compaction_snapshot_seq(durable))?;
        let commit_lock_hold_us = commit_lock_started.elapsed().as_micros();
        let catalog_started = std::time::Instant::now();
        let catalog = catalog_from_vault_tiers_through_seq(
            durable.root(),
            durable.tiering_policy(),
            durable_seq,
        )?;
        let catalog_scan_ms = catalog_started.elapsed().as_millis();
        let mut candidates = catalog
            .column_families()
            .into_iter()
            .map(|cf| (cf, catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES)))
            .filter(|(_cf, debt)| {
                debt.score_milli >= 1_000
                    || is_tiny_file_fanout_debt(debt.pending_files, debt.pending_bytes)
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .1
                .score_milli
                .cmp(&left.1.score_milli)
                .then_with(|| right.1.pending_files.cmp(&left.1.pending_files))
                .then_with(|| left.0.name().cmp(&right.0.name()))
        });
        let routine_cfs = if readiness_only {
            Vec::new()
        } else {
            candidates
                .iter()
                .filter(|(_cf, debt)| debt.pending_files < LIVE_COMPACTION_TRIGGER_FILES)
                .take(ROUTINE_COMPACTION_CFS_PER_PASS)
                .map(|(cf, _debt)| *cf)
                .collect::<Vec<_>>()
        };
        let selected = candidates
            .into_iter()
            .filter(|(cf, debt)| {
                debt.pending_files >= LIVE_COMPACTION_TRIGGER_FILES || routine_cfs.contains(cf)
            })
            .collect::<Vec<_>>();
        tracing::info!(
            code = "CALYX_ASTER_NATIVE_FANOUT_MAINTENANCE_START",
            durable_seq,
            commit_lock_hold_us,
            catalog_scan_ms,
            readiness_only,
            selected_cfs = selected.len(),
            max_input_files_per_cf = LIVE_COMPACTION_MAX_INPUT_FILES,
            max_input_bytes_per_cf = LIVE_COMPACTION_MAX_INPUT_BYTES,
            target_files_per_cf = DEFAULT_COMPACTION_TARGET_FILES,
            proactive_trigger_files = LIVE_COMPACTION_TRIGGER_FILES,
            hard_page_source_limit = crate::sst::MAX_INTERSECTING_SST_PAGE_SOURCES,
            selected = ?selected
                .iter()
                .map(|(cf, debt)| (cf.name(), debt.pending_files, debt.pending_bytes, debt.score_milli))
                .collect::<Vec<_>>(),
            "starting bounded native-CF fan-out maintenance without retaining the durable commit lock"
        );
        let started = std::time::Instant::now();
        let mut results = Vec::new();
        for (cf, initial_debt) in selected {
            let readiness_required = initial_debt.pending_files >= LIVE_COMPACTION_TRIGGER_FILES;
            loop {
                let before = catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES);
                if readiness_required && before.pending_files <= DEFAULT_COMPACTION_TARGET_FILES {
                    break;
                }
                let result = self.compact_catalog_cf_batch(
                    durable,
                    &catalog,
                    durable_seq,
                    cf,
                    LIVE_COMPACTION_MAX_INPUT_FILES,
                )?;
                let after = catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES);
                let made_progress = after.pending_files < before.pending_files;
                results.push(result);
                if !readiness_required || made_progress {
                    if !readiness_required {
                        break;
                    }
                    continue;
                }
                return Err(CalyxError {
                    code: "CALYX_ASTER_NATIVE_FANOUT_READINESS_NO_PROGRESS",
                    message: format!(
                        "native-CF readiness compaction for {} made no file-count progress: before_files={} after_files={} target_files={} proactive_trigger_files={} hard_page_source_limit={}",
                        cf.name(),
                        before.pending_files,
                        after.pending_files,
                        DEFAULT_COMPACTION_TARGET_FILES,
                        LIVE_COMPACTION_TRIGGER_FILES,
                        crate::sst::MAX_INTERSECTING_SST_PAGE_SOURCES,
                    ),
                    remediation: "inspect SST size distribution and the bounded productive-window decision; do not raise the page-source ceiling or bypass candidate paging",
                });
            }
        }
        let unsafe_cfs = catalog
            .column_families()
            .into_iter()
            .filter_map(|cf| {
                let files = catalog
                    .debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES)
                    .pending_files;
                (files > crate::sst::MAX_INTERSECTING_SST_PAGE_SOURCES).then_some((cf, files))
            })
            .collect::<Vec<_>>();
        if !unsafe_cfs.is_empty() {
            return Err(CalyxError {
                code: "CALYX_ASTER_NATIVE_FANOUT_READINESS_UNSAFE",
                message: format!(
                    "native-CF maintenance ended above the range-page source limit {}: {:?}",
                    crate::sst::MAX_INTERSECTING_SST_PAGE_SOURCES,
                    unsafe_cfs
                        .iter()
                        .map(|(cf, files)| (cf.name(), *files))
                        .collect::<Vec<_>>()
                ),
                remediation: "preserve the vault and inspect compaction logs/SST distribution; do not start range-serving workloads until every listed CF is physically below the shared source limit",
            });
        }
        tracing::info!(
            code = "CALYX_ASTER_NATIVE_FANOUT_MAINTENANCE_DONE",
            attempted_cfs = results.len(),
            compacted_cfs = results
                .iter()
                .filter(|result| matches!(result, CompactionResult::Compacted(_)))
                .count(),
            reclaimed_input_files = results
                .iter()
                .filter_map(|result| match result {
                    CompactionResult::Compacted(report) => Some(report.reclaimed_input_files),
                    CompactionResult::Skipped { .. } => None,
                })
                .sum::<usize>(),
            elapsed_ms = started.elapsed().as_millis(),
            "completed bounded native-CF fan-out maintenance"
        );
        Ok(results)
    }

    fn compact_catalog_cf_batch(
        &self,
        durable: &DurableVault,
        catalog: &CompactionCatalog,
        durable_seq: u64,
        cf: ColumnFamily,
        max_input_files: usize,
    ) -> Result<CompactionResult> {
        let all_inputs = catalog.shards_for_cf(cf);
        let before_files = all_inputs.len();
        let Some(window) = productive_compaction_window(
            &all_inputs,
            max_input_files,
            LIVE_COMPACTION_MAX_INPUT_BYTES,
            DEFAULT_COMPACTION_TARGET_BYTES,
        ) else {
            tracing::info!(
                code = "CALYX_ASTER_NATIVE_CF_COMPACTION_NO_PRODUCTIVE_WINDOW",
                cf = cf.name(),
                before_files,
                max_input_files,
                max_input_bytes = LIVE_COMPACTION_MAX_INPUT_BYTES,
                output_target_bytes = DEFAULT_COMPACTION_TARGET_BYTES,
                "skipping native-CF compaction because no bounded input window can reduce file count"
            );
            return Ok(CompactionResult::Skipped {
                debt: catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES),
            });
        };
        let selected_inputs =
            all_inputs[window.start..window.start.saturating_add(window.files)].to_vec();
        let output_dir = durable
            .compaction_output_path(cf, durable_seq)
            .parent()
            .ok_or_else(|| CalyxError::disk_pressure("compaction output has no parent"))?
            .to_path_buf();
        // A bounded range is only a subset of the CF. Its output must remain
        // in the selected rows' maximum commit domain so unselected newer
        // files continue to win. The adoption allocator and rolling writer
        // both scan for free reserved slots, so repeated old-domain rewrites
        // fail only on true exhaustion rather than colliding with a gap.
        let output = commit_domain_output_path(&output_dir, &selected_inputs)?;
        tracing::info!(
            code = "CALYX_ASTER_NATIVE_CF_COMPACTION_START",
            cf = cf.name(),
            before_files,
            selected_start_file = window.start,
            selected_input_files = selected_inputs.len(),
            input_bytes = window.bytes,
            estimated_max_output_files = window.estimated_output_files,
            estimated_file_reduction = window.estimated_file_reduction,
            max_input_files,
            max_input_bytes = LIVE_COMPACTION_MAX_INPUT_BYTES,
            output = %output.display(),
            "starting file-and-byte-bounded native-CF compaction"
        );
        let started = std::time::Instant::now();
        let mut result = catalog.compact_cf_file_range(
            cf,
            output,
            CompactionThrottle::max_input_bytes(LIVE_COMPACTION_MAX_INPUT_BYTES),
            window.start,
            selected_inputs.len(),
        )?;
        if let CompactionResult::Compacted(report) = &mut result {
            if report.output_paths.len() >= report.input_files {
                cleanup_unproductive_outputs(report)?;
                return Err(CalyxError {
                    code: "CALYX_ASTER_COMPACTION_NO_FILE_REDUCTION",
                    message: format!(
                        "native-CF compaction for {} produced {} files from {} inputs despite a productive admission estimate; staged outputs were removed and inputs remain authoritative",
                        cf.name(),
                        report.output_paths.len(),
                        report.input_files
                    ),
                    remediation: "inspect the selected input sizes and rolling SST estimator; do not retry until admission/output sizing is corrected",
                });
            }
            ensure_reclaim_outputs_manifest_bounded(&report.output_paths, durable_seq)?;
            let doomed = plan_compaction_input_reclaim(report)?;
            report.reclaimed_input_files = self.rows.retire_then_purge_cf_inputs(
                &[cf],
                "reclaim generic native-CF compaction inputs",
                &doomed,
            )?;
            if report.reclaimed_input_files != report.input_files {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "native-CF compaction reclaimed {} of {} proven input files for {}",
                    report.reclaimed_input_files,
                    report.input_files,
                    cf.name()
                )));
            }
            if cf == ColumnFamily::Recurrence {
                self.rows.refresh_router_cfs_after_reclaim(
                    &[cf],
                    "rewrite recurrence tombstone compaction outputs",
                    || prune_recurrence_tombstones(report),
                )?;
            }
            let after_files = before_files
                .saturating_sub(report.reclaimed_input_files)
                .saturating_add(report.output_paths.len());
            tracing::info!(
                code = "CALYX_ASTER_NATIVE_CF_COMPACTION_DONE",
                cf = cf.name(),
                before_files,
                selected_input_files = report.input_files,
                reclaimed_input_files = report.reclaimed_input_files,
                output_files = report.output_paths.len(),
                after_files,
                input_bytes = report.input_bytes,
                output_bytes = report.output_bytes,
                elapsed_ms = started.elapsed().as_millis(),
                "completed bounded native-CF compaction with physical input reclaim"
            );
        }
        Ok(result)
    }

    /// Captures the immutable, manifest-covered prefix for off-lock compaction.
    ///
    /// A paced checkpoint releases its publisher boundary before this short
    /// commit-lock read. Commits in that interval legitimately advance
    /// `latest_seq` beyond the published watermark. They stay in the WAL and
    /// are excluded by `catalog_from_vault_tiers_through_seq`; requiring tip
    /// equality here would turn every such concurrent commit into a false
    /// coverage failure. Outputs are independently required to remain at or
    /// below this watermark before any input is reclaimed (issue #1132).
    fn published_compaction_snapshot_seq(&self, durable: &DurableVault) -> Result<u64> {
        let durable_seq = durable.manifest_durable_seq()?;
        let latest = self.latest_seq();
        if durable_seq > latest {
            return Err(CalyxError {
                code: "CALYX_ASTER_COMPACTION_SNAPSHOT_AHEAD",
                message: format!(
                    "manifest durable_seq {durable_seq} exceeds latest committed seq {latest}; the in-memory vault cannot prove the published compaction prefix"
                ),
                remediation: "preserve the vault and inspect CURRENT, its manifest, and WAL replay telemetry; do not compact or reclaim any SST inputs",
            });
        }
        if durable_seq < latest {
            tracing::info!(
                code = "CALYX_ASTER_COMPACTION_SNAPSHOT_WITH_CONCURRENT_TAIL",
                durable_seq,
                latest_seq = latest,
                concurrent_tail_seqs = latest.saturating_sub(durable_seq),
                "captured a manifest-covered compaction prefix while newer commits remain WAL-authoritative"
            );
        }
        Ok(durable_seq)
    }

    /// Manifest durable coverage after a locked full flush; fails closed when
    /// the manifest does not cover the latest committed seq. Callers that copy
    /// or recycle the whole durable state require tip equality; bounded
    /// compaction uses [`Self::published_compaction_snapshot_seq`] instead.
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
    /// Captures a short manifest-covered snapshot per CF, then performs every
    /// physical merge, fsync, router retirement, and file purge off the global
    /// durable commit lock under the native-compaction guard.
    ///
    /// Every vault write — including the audit row an MCP `initialize` and each
    /// `act` command writes before doing anything — is serialized behind the
    /// durable commit lock. Taking it once around a whole multi-CF sweep
    /// therefore parks the entire tool surface for as long as the sweep runs:
    /// measured at 62 s here (and 469 s when #1806 was filed), which is what
    /// made `act operation=lease_acquire` take ~111 s in #1829.
    ///
    /// A wall-clock check between compaction calls cannot bound a call already
    /// inside the lock: the configured host measured one 747 MiB full-CF merge
    /// at 14.1 s and a 14.8 s commit-lock hold. The immutable manifest prefix is
    /// the compaction boundary; newer commits remain WAL-authoritative and win
    /// by sequence while this older prefix is rewritten (issues #1806/#1832).
    pub fn purge_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()> {
        self.drain_checkpoints_paced("tombstone purge preflight")?;
        let mut unique = Vec::new();
        for cf in cfs {
            if !unique.contains(cf) {
                unique.push(*cf);
            }
        }
        self.with_native_compaction_guard(|| {
            for cf in unique {
                self.purge_tombstoned_cf_snapshot(cf)?;
            }
            Ok(())
        })
    }

    /// Captures one manifest-covered CF view under a short commit-lock read and
    /// runs all physical tombstone-purge work after releasing that lock.
    fn purge_tombstoned_cf_snapshot(&self, cf: ColumnFamily) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Ok(());
        };
        let commit_lock_started = std::time::Instant::now();
        let durable_seq =
            self.with_durable_commit_lock(|| self.published_compaction_snapshot_seq(durable))?;
        let commit_lock_hold_us = commit_lock_started.elapsed().as_micros();
        let catalog_started = std::time::Instant::now();
        let catalog = catalog_from_vault_tiers_through_seq(
            durable.root(),
            durable.tiering_policy(),
            durable_seq,
        )?;
        tracing::info!(
            code = "CALYX_ASTER_TOMBSTONE_PURGE_SNAPSHOT_READY",
            cf = cf.name(),
            durable_seq,
            commit_lock_hold_us,
            catalog_scan_ms = catalog_started.elapsed().as_millis(),
            "captured tombstone-purge input view without retaining the durable commit lock"
        );
        let mut passes = 0_usize;
        loop {
            let before = catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES);
            if before.pending_files <= DEFAULT_COMPACTION_TARGET_FILES {
                break;
            }
            passes += 1;
            if passes > TOMBSTONE_PURGE_MAX_COMPACTION_PASSES {
                return Err(CalyxError {
                    code: "CALYX_ASTER_TOMBSTONE_PURGE_PASS_BUDGET_EXHAUSTED",
                    message: format!(
                        "tombstone purge for {} did not converge within {TOMBSTONE_PURGE_MAX_COMPACTION_PASSES} off-lock compaction passes",
                        cf.name()
                    ),
                    remediation: "inspect SST size/order distribution for this CF; a non-converging fan-out reduction means compaction admission is not making progress",
                });
            }
            let result = self.compact_catalog_cf_batch(
                durable,
                &catalog,
                durable_seq,
                cf,
                LIVE_COMPACTION_MAX_INPUT_FILES,
            )?;
            let after = catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES);
            if after.pending_files >= before.pending_files {
                return Err(CalyxError {
                    code: "CALYX_ASTER_TOMBSTONE_PURGE_FANOUT_NO_PROGRESS",
                    message: format!(
                        "tombstone purge could not reduce {} to a bounded complete-compaction input: before_files={} after_files={} target_files={} result={result:?}",
                        cf.name(),
                        before.pending_files,
                        after.pending_files,
                        DEFAULT_COMPACTION_TARGET_FILES,
                    ),
                    remediation: "preserve the vault and inspect SST size/order distribution; do not prune tombstones from only a partial oldest prefix",
                });
            }
        }
        let Some(report) = prepare_tombstoned_cf_compaction(durable, &catalog, cf, durable_seq)?
        else {
            return Ok(());
        };
        let doomed = plan_compaction_input_reclaim(&report)?;
        let reclaimed = self.rows.retire_then_purge_cf_inputs(
            &[cf],
            "reclaim tombstoned compaction inputs",
            &doomed,
        )?;
        if reclaimed != report.input_files {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "tombstone compaction reclaimed {reclaimed} of {} proven input files for {}",
                report.input_files,
                cf.name()
            )));
        }
        Ok(())
    }

    pub(crate) fn purge_tombstoned_cfs_locked(&self, cfs: &[ColumnFamily]) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Ok(());
        };
        self.checkpoint_locked()?;
        let durable_seq = self.verified_durable_coverage_seq(durable)?;
        let catalog = catalog_from_vault_tiers_through_seq(
            durable.root(),
            durable.tiering_policy(),
            durable_seq,
        )?;
        let mut unique = Vec::new();
        for cf in cfs {
            if !unique.contains(cf) {
                unique.push(*cf);
            }
        }
        for cf in unique {
            loop {
                let before = catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES);
                if before.pending_files <= DEFAULT_COMPACTION_TARGET_FILES {
                    break;
                }
                let result = self.compact_catalog_cf_batch(
                    durable,
                    &catalog,
                    durable_seq,
                    cf,
                    LIVE_COMPACTION_MAX_INPUT_FILES,
                )?;
                let after = catalog.debt_for_cf(cf, DEFAULT_COMPACTION_TARGET_BYTES);
                if after.pending_files >= before.pending_files {
                    return Err(CalyxError {
                        code: "CALYX_ASTER_TOMBSTONE_PURGE_FANOUT_NO_PROGRESS",
                        message: format!(
                            "tombstone purge could not reduce {} to a bounded complete-compaction input: before_files={} after_files={} target_files={} result={result:?}",
                            cf.name(),
                            before.pending_files,
                            after.pending_files,
                            DEFAULT_COMPACTION_TARGET_FILES,
                        ),
                        remediation: "preserve the vault and inspect SST size/order distribution; do not prune tombstones from only a partial oldest prefix",
                    });
                }
            }
            let Some(report) =
                prepare_tombstoned_cf_compaction(durable, &catalog, cf, durable_seq)?
            else {
                continue;
            };
            let doomed = plan_compaction_input_reclaim(&report)?;
            let reclaimed = self.rows.retire_then_purge_cf_inputs(
                &[cf],
                "reclaim tombstoned compaction inputs",
                &doomed,
            )?;
            if reclaimed != report.input_files {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "tombstone compaction reclaimed {reclaimed} of {} proven input files for {}",
                    report.input_files,
                    cf.name()
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn with_native_compaction_guard<T>(
        &self,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let Some(durable) = &self.durable else {
            return f();
        };
        let _maintenance_guard = try_acquire_native_compaction_guard(durable)?;
        f()
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

pub(super) fn try_acquire_native_compaction_guard(
    durable: &DurableVault,
) -> Result<crate::file_lock::FileLockGuard> {
    let path = durable.native_compaction_lock_path();
    match crate::file_lock::FileLockGuard::try_acquire(&path)? {
        Some(guard) => Ok(guard),
        None => {
            tracing::warn!(
                code = "CALYX_ASTER_NATIVE_COMPACTION_BUSY",
                path = %path.display(),
                "native compaction admission rejected because another maintenance pass is active"
            );
            Err(CalyxError {
                code: "CALYX_ASTER_NATIVE_COMPACTION_BUSY",
                message: format!(
                    "native compaction lock {} is held by another maintenance pass",
                    path.display()
                ),
                remediation: "inspect the active storage GC task and its structured compaction \
                              progress, then retry after that pass completes",
            })
        }
    }
}

fn prepare_tombstoned_cf_compaction(
    durable: &DurableVault,
    catalog: &CompactionCatalog,
    cf: ColumnFamily,
    seq: u64,
) -> Result<Option<crate::compaction::CompactionReport>> {
    let output_dir = durable.tiering_policy().map_or_else(
        || durable.root().join("cf").join(cf.name()),
        |policy| policy.place_current_cf(cf).absolute_dir(),
    );
    let all_inputs = catalog.shards_for_cf(cf);
    if all_inputs.is_empty() {
        return Ok(None);
    }
    if all_inputs.len() > DEFAULT_COMPACTION_TARGET_FILES {
        return Err(CalyxError {
            code: "CALYX_ASTER_TOMBSTONE_PURGE_INPUT_UNBOUNDED",
            message: format!(
                "complete tombstone purge for {} still has {} SST inputs after fan-out reduction; maximum complete-compaction inputs={}",
                cf.name(),
                all_inputs.len(),
                DEFAULT_COMPACTION_TARGET_FILES,
            ),
            remediation: "inspect the preceding fan-out maintenance telemetry; never prune a tombstone without every older overlapping source",
        });
    }
    let output = commit_domain_output_path(&output_dir, &all_inputs)?;
    tracing::info!(
        code = "CALYX_ASTER_TOMBSTONE_PURGE_FULL_COMPACTION_START",
        cf = cf.name(),
        input_files = all_inputs.len(),
        input_bytes = all_inputs.iter().map(|input| input.bytes).sum::<u64>(),
        output = %output.display(),
        "starting complete streaming CF compaction before physical tombstone removal"
    );
    let mut result = catalog.compact_cf(cf, output, CompactionThrottle::unlimited())?;
    let CompactionResult::Compacted(report) = &mut result else {
        return Ok(None);
    };
    let pruned_tombstones = prune_mvcc_tombstones(report)?;
    ensure_reclaim_outputs_manifest_bounded(&report.output_paths, seq)?;
    tracing::info!(
        code = "CALYX_ASTER_TOMBSTONE_PURGE_FULL_COMPACTION_DONE",
        cf = cf.name(),
        input_files = report.input_files,
        input_bytes = report.input_bytes,
        output_files = report.output_paths.len(),
        output_bytes = report.output_bytes,
        pruned_tombstones,
        "completed full-CF merge and removed physical MVCC tombstones before input reclaim"
    );
    Ok(Some((**report).clone()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProductiveCompactionWindow {
    start: usize,
    files: usize,
    bytes: u64,
    estimated_output_files: usize,
    estimated_file_reduction: usize,
}

/// Finds the highest-reduction contiguous window under both physical bounds.
///
/// Contiguity plus commit-domain output naming preserves canonical precedence.
/// Requiring `ceil(input_bytes / output_target) < input_files` prevents
/// already-sized SSTs from being rewritten forever without reducing fan-out.
fn productive_compaction_window(
    inputs: &[crate::compaction::SstShard],
    max_input_files: usize,
    max_input_bytes: u64,
    output_target_bytes: u64,
) -> Option<ProductiveCompactionWindow> {
    if inputs.len() < 2 || max_input_files < 2 || max_input_bytes == 0 {
        return None;
    }
    let output_target_bytes = output_target_bytes.max(1);
    let mut groups = Vec::<(usize, usize, u64)>::new();
    let mut group_start = 0usize;
    while group_start < inputs.len() {
        let domain = compaction_order_domain(&inputs[group_start]);
        let mut group_end = group_start.saturating_add(1);
        let mut group_bytes = inputs[group_start].bytes;
        while group_end < inputs.len() && compaction_order_domain(&inputs[group_end]) == domain {
            group_bytes = group_bytes.saturating_add(inputs[group_end].bytes);
            group_end = group_end.saturating_add(1);
        }
        groups.push((group_start, group_end, group_bytes));
        group_start = group_end;
    }

    let mut first_group = 0usize;
    let mut bytes = 0u64;
    let mut files = 0usize;
    let mut best: Option<ProductiveCompactionWindow> = None;
    for last_group in 0..groups.len() {
        let (group_start, group_end, group_bytes) = groups[last_group];
        let group_files = group_end.saturating_sub(group_start);
        if group_bytes > max_input_bytes || group_files > max_input_files {
            first_group = last_group.saturating_add(1);
            bytes = 0;
            files = 0;
            continue;
        }
        bytes = bytes.saturating_add(group_bytes);
        files = files.saturating_add(group_files);
        while first_group <= last_group && (files > max_input_files || bytes > max_input_bytes) {
            let (first_start, first_end, first_bytes) = groups[first_group];
            files = files.saturating_sub(first_end.saturating_sub(first_start));
            bytes = bytes.saturating_sub(first_bytes);
            first_group = first_group.saturating_add(1);
        }
        if files < 2 {
            continue;
        }
        let estimated_output_files_u64 = bytes.div_ceil(output_target_bytes).max(1);
        let estimated_output_files =
            usize::try_from(estimated_output_files_u64).unwrap_or(usize::MAX);
        if estimated_output_files >= files {
            continue;
        }
        let candidate = ProductiveCompactionWindow {
            start: groups[first_group].0,
            files,
            bytes,
            estimated_output_files,
            estimated_file_reduction: files.saturating_sub(estimated_output_files),
        };
        let replace = best.is_none_or(|current| {
            candidate.estimated_file_reduction > current.estimated_file_reduction
                || (candidate.estimated_file_reduction == current.estimated_file_reduction
                    && candidate.files > current.files)
                || (candidate.estimated_file_reduction == current.estimated_file_reduction
                    && candidate.files == current.files
                    && candidate.start < current.start)
        });
        if replace {
            best = Some(candidate);
        }
    }
    best
}

fn compaction_order_domain(input: &crate::compaction::SstShard) -> (u8, u64) {
    let order = sst_order_key(&input.path)
        .expect("compaction input has a validated SST name")
        .expect("compaction input has an SST order");
    (order.epoch, order.seq)
}

fn cleanup_unproductive_outputs(report: &crate::compaction::CompactionReport) -> Result<()> {
    for output in &report.output_paths {
        invalidate_reader(output);
        fs::remove_file(output).map_err(|error| {
            CalyxError::disk_pressure(format!(
                "remove unproductive staged compaction output {}: {error}",
                output.display()
            ))
        })?;
    }
    Ok(())
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

/// Resolves and proves the compaction inputs that may be physically reclaimed.
///
/// Every check here is filesystem I/O (`canonicalize` opens the file), so it
/// runs *before* the exclusive router lock is taken; the returned canonical
/// set is what [`VersionedCfStore::retire_then_purge_cf_inputs`] retires from
/// the served level and then deletes without the lock held (issue #1806).
pub(super) fn plan_compaction_input_reclaim(
    report: &crate::compaction::CompactionReport,
) -> Result<BTreeSet<PathBuf>> {
    let outputs = canonical_output_paths(report, "stat compacted SST")?;
    let mut doomed = BTreeSet::new();
    for input in &report.input_paths {
        let input = fs::canonicalize(input).map_err(|error| {
            CalyxError::disk_pressure(format!(
                "stat proven compaction input {} before reclaim: {error}",
                input.display()
            ))
        })?;
        if outputs.contains(&input) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "compaction input {} aliases an output path",
                input.display()
            )));
        }
        if classify_sst(&input)?.is_none() {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "compaction input {} is no longer a canonical SST",
                input.display()
            )));
        }
        doomed.insert(input);
    }
    Ok(doomed)
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

fn prune_mvcc_tombstones(report: &mut crate::compaction::CompactionReport) -> Result<u64> {
    rewrite_compacted_without(
        report,
        |value| Ok(is_tombstone_value(value)),
        "mvcc tombstone",
    )
}

fn prune_recurrence_tombstones(report: &mut crate::compaction::CompactionReport) -> Result<u64> {
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
) -> Result<u64> {
    let mut pruned = 0_u64;
    let original_outputs = report.output_paths.clone();
    for output_path in &original_outputs {
        for entry in shared_reader(output_path)?.iter()? {
            if should_prune(&entry.value)? {
                pruned += 1;
            }
        }
    }
    if pruned == 0 {
        return Ok(0);
    }

    let seq = compaction_output_seq(&report.output_path)?;
    let reclaimed_path = durable_compaction_slot_path(&report.staging_parent, seq)?;
    let mut writer = RollingSstWriter::new(&reclaimed_path, DEFAULT_COMPACTION_TARGET_BYTES)?;
    let mut retained = 0_u64;
    let mut logical_bytes = 0_u64;
    for output_path in &original_outputs {
        for entry in shared_reader(output_path)?.iter()? {
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
        invalidate_reader(output_path);
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
    Ok(pruned)
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
