use super::{RowTable, VersionChain, VersionedCfStore};
use crate::cf::ColumnFamily;
use crate::gc::{GcMetrics, GcRateLimit, GcResult, SnapshotVersionGc};
use calyx_core::{CalyxError, Clock, Result, Seq, Ts};
use std::collections::BTreeMap;

impl VersionedCfStore {
    pub fn set_snapshot_gc_rate_limit(&self, rate_limit: GcRateLimit) {
        self.snapshot_gc.set_rate_limit(rate_limit);
    }

    pub fn snapshot_version_gc_tick(&self, clock: &dyn Clock) -> Result<GcResult> {
        let safe_point = self.snapshot_gc_safe_point(clock.now());
        self.snapshot_gc
            .run_once_at_safe_point(self, clock, safe_point)
    }

    pub fn snapshot_gc_safe_point(&self, now: Ts) -> Seq {
        let view = self.leases.live_view(now);
        view.oldest_pinned_seq.unwrap_or_else(|| self.current_seq())
    }

    pub fn snapshot_gc_metrics(&self, now: Ts) -> GcMetrics {
        let safe_point = self.snapshot_gc_safe_point(now);
        let debt = self.snapshot_gc_debt(safe_point);
        self.snapshot_gc_counters.metrics_with_debt(debt)
    }

    pub fn record_snapshot_gc_physical_bytes_freed(&self, bytes: usize) {
        self.snapshot_gc_counters.record_physical_bytes_freed(bytes);
    }

    pub fn compact_router_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()> {
        let mut router = self.router.write().expect("mvcc router poisoned");
        let Some(router) = router.as_mut() else {
            return Err(CalyxError {
                code: "CALYX_ASTER_COMPACTION_UNAVAILABLE",
                message: "tombstone compaction requires a physical CF router".to_string(),
                remediation: "open the MVCC store with a CF router before requesting compaction",
            });
        };
        router.compact_tombstoned_cfs_at(cfs, self.current_seq())
    }
}

impl SnapshotVersionGc for VersionedCfStore {
    fn reclaim_snapshot_versions(&self, safe_point: Seq, max_versions: usize) -> Result<GcResult> {
        let mut table = self.rows.write().expect("mvcc row table poisoned");
        let mut remaining = max_versions;
        let mut versions_reclaimed = 0usize;
        let mut bytes_freed = 0usize;
        for versions in table.values_mut().flat_map(BTreeMap::values_mut) {
            if remaining == 0 {
                break;
            }
            let (chain_reclaimed, chain_bytes) =
                reclaim_chain(versions, safe_point, &mut remaining);
            versions_reclaimed += chain_reclaimed;
            bytes_freed += chain_bytes;
        }
        let compaction_debt = snapshot_gc_debt_for_table(&table, safe_point);
        let result = GcResult {
            safe_point_seq: safe_point,
            versions_reclaimed,
            bytes_freed,
            compaction_debt,
            rate_limited: compaction_debt > 0,
        };
        self.snapshot_gc_counters.record_result(result);
        Ok(result)
    }

    fn snapshot_gc_debt(&self, safe_point: Seq) -> u64 {
        let table = self.read_rows("snapshot_gc_debt");
        snapshot_gc_debt_for_table(&table, safe_point)
    }
}

fn reclaim_chain(
    versions: &mut VersionChain,
    safe_point: Seq,
    remaining: &mut usize,
) -> (usize, usize) {
    let keep_boundary = retained_boundary_index(versions, safe_point);
    let mut retained = Vec::with_capacity(versions.len());
    let mut reclaimed = 0usize;
    let mut bytes_freed = 0usize;
    for (index, version) in versions.drain(..).enumerate() {
        let can_reclaim =
            version.seq < safe_point && Some(index) != keep_boundary && *remaining > 0;
        if can_reclaim {
            *remaining -= 1;
            reclaimed += 1;
            bytes_freed += version.value.len();
        } else {
            retained.push(version);
        }
    }
    *versions = retained;
    (reclaimed, bytes_freed)
}

fn snapshot_gc_debt_for_table(table: &RowTable, safe_point: Seq) -> u64 {
    table
        .values()
        .flat_map(BTreeMap::values)
        .map(|versions| reclaimable_versions(versions, safe_point) as u64)
        .sum()
}

fn reclaimable_versions(versions: &VersionChain, safe_point: Seq) -> usize {
    let keep_boundary = retained_boundary_index(versions, safe_point);
    versions
        .iter()
        .enumerate()
        .filter(|(index, version)| version.seq < safe_point && Some(*index) != keep_boundary)
        .count()
}

fn retained_boundary_index(versions: &VersionChain, safe_point: Seq) -> Option<usize> {
    versions
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, version)| (version.seq <= safe_point).then_some(index))
}
