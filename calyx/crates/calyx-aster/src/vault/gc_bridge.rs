//! Vault-facing bridge for snapshot GC scheduler ticks.

use crate::cf::ColumnFamily;
use crate::compaction::{
    CompactionResult, CompactionThrottle, catalog_from_vault_tiers, compact_shards,
};
use crate::gc::{GcRateLimit, GcResult, SnapshotGcTick};
use crate::mvcc::Snapshot;
use crate::storage_names::sst_order_key;
use crate::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Result};

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Runs one snapshot-pin watchdog tick.
    ///
    /// The background GC scheduler should call this at its 1 s cadence once the
    /// scheduler exists. Until then, resource-status and tests use the same
    /// underlying store hook to abort expired reader pins fail-closed.
    pub fn snapshot_gc_tick(&self, max_gap_seqs: u64) -> SnapshotGcTick {
        self.rows.snapshot_gc_tick(&self.clock, max_gap_seqs)
    }

    /// Runs one MVCC snapshot-version GC tick and physically compacts obsolete SSTs.
    pub fn snapshot_version_gc_once(&self, rate_limit: GcRateLimit) -> Result<GcResult> {
        self.rows.set_snapshot_gc_rate_limit(rate_limit);
        let mut result = self.rows.snapshot_version_gc_tick(&self.clock)?;
        if result.versions_reclaimed == 0 && result.rate_limited {
            return Ok(result);
        }
        let physical =
            self.reclaim_snapshot_ssts(result.safe_point_seq, rate_limit.max_ops_per_run)?;
        if physical.bytes_freed > 0 {
            self.rows
                .record_snapshot_gc_physical_bytes_freed(physical.bytes_freed);
            result.bytes_freed = result.bytes_freed.saturating_add(physical.bytes_freed);
        }
        result.rate_limited |= physical.rate_limited;
        Ok(result)
    }

    /// Runs snapshot GC using env-configured anti-storm limits.
    pub fn snapshot_version_gc_once_from_env(&self) -> Result<GcResult> {
        self.snapshot_version_gc_once(GcRateLimit::from_env()?)
    }

    /// Reads one CF row through an explicit tracked reader snapshot.
    pub fn read_pinned_cf(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.rows.read_at(snapshot, cf, key, &self.clock)
    }

    fn reclaim_snapshot_ssts(&self, safe_point: u64, max_input_files: usize) -> Result<GcResult> {
        // Issue #1806: `reclaim_snapshot_ssts_locked` starts with a full
        // `flush_locked`, so the staged checkpoint backlog used to be drained
        // inside this single acquisition (the "storage GC holds the lock ~64 s"
        // symptom). Pace that drain first; the acquisition below then only
        // absorbs commits that landed during the pacing loop.
        self.drain_checkpoints_paced("snapshot GC preflight")?;
        // Maintenance lane: fenced once a close is declared (#2100).
        self.with_durable_commit_lock_maintenance("snapshot GC SST reclaim", || {
            self.reclaim_snapshot_ssts_locked(safe_point, max_input_files)
        })
    }

    fn reclaim_snapshot_ssts_locked(
        &self,
        safe_point: u64,
        max_input_files: usize,
    ) -> Result<GcResult> {
        let Some(durable) = &self.durable else {
            return Ok(GcResult {
                safe_point_seq: safe_point,
                ..GcResult::default()
            });
        };
        if max_input_files == 0 || safe_point == 0 {
            return Ok(GcResult {
                safe_point_seq: safe_point,
                rate_limited: max_input_files == 0,
                ..GcResult::default()
            });
        }

        self.flush_locked()?;
        let manifest_durable_seq = self.verified_durable_coverage_seq(durable)?;
        let catalog = catalog_from_vault_tiers(durable.root(), durable.tiering_policy())?;
        let mut bytes_freed = 0usize;
        let mut files_seen = 0usize;
        for cf in catalog.column_families() {
            if cf == ColumnFamily::Ledger || files_seen >= max_input_files {
                continue;
            }
            let mut inputs = Vec::new();
            for shard in catalog.shards_for_cf(cf) {
                let Some(order) = sst_order_key(&shard.path)? else {
                    continue;
                };
                // `safe_point` is a commit seq; only commit-domain files
                // (epoch 1) can be compared against it. Legacy router flushes
                // carry flush ordinals (issue #1138) and are adopted via the
                // CLI compact path instead of snapshot GC.
                if order.epoch == 1 && order.seq < safe_point {
                    inputs.push(shard);
                    files_seen += 1;
                    if files_seen == max_input_files {
                        break;
                    }
                }
            }
            if inputs.len() < 2 {
                continue;
            }
            let output_seq = safe_point.saturating_sub(1);
            let output = durable.compaction_output_path(cf, output_seq);
            if let CompactionResult::Compacted(report) =
                compact_shards(cf, &inputs, output, CompactionThrottle::unlimited())?
            {
                super::compaction_bridge::ensure_reclaim_outputs_manifest_bounded(
                    &report.output_paths,
                    manifest_durable_seq,
                )?;
                let net = report.input_bytes.saturating_sub(report.output_bytes) as usize;
                let doomed = super::compaction_bridge::plan_compaction_input_reclaim(&report)?;
                let reclaimed = self.rows.retire_then_purge_cf_inputs(
                    &[cf],
                    "reclaim snapshot GC inputs",
                    &doomed,
                )?;
                if reclaimed != report.input_files {
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "snapshot GC reclaimed {reclaimed} of {} proven input files for {}",
                        report.input_files,
                        cf.name()
                    )));
                }
                bytes_freed = bytes_freed.saturating_add(net);
            }
        }
        Ok(GcResult {
            safe_point_seq: safe_point,
            bytes_freed,
            rate_limited: files_seen >= max_input_files,
            ..GcResult::default()
        })
    }
}
