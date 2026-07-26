//! In-memory MVCC row table used to define the cross-CF snapshot contract.

mod gc;
mod read;
mod scan_pages;
use crate::cf::{CfRouter, ColumnFamily, KeyRange, RetiredCfPhysical};
use crate::gc::{SnapshotGcCounters, SnapshotGcReclaimer, SnapshotGcTick};
use crate::mvcc::{
    Freshness, ReadBarrier, ReaderLease, SeqAllocator, Snapshot, read_barrier::first_blocking,
};
use crate::resource::{
    LeaseRegistry, LeaseView, MemtableCfStatus, MemtableStatus, ResourceCounters,
};
use crate::sst::SstSummary;
use calyx_core::{CalyxError, Clock, Result, Seq, SlotId, Ts};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

const TOMBSTONE_VALUE: &[u8] = b"\0CALYX_ASTER_TOMBSTONE_V1";

/// Structured-warning budget for how long the exclusive CF-router write lock may
/// be held during a compaction reclaim+refresh swap. The streaming compaction
/// itself runs without this lock; the lock covers only input reclaim plus the
/// serving-view refresh, so exceeding this budget means reads on the affected
/// CFs were blocked longer than intended and warrants investigation (#1798).
const EXCLUSIVE_ROUTER_RECLAIM_WARN_MS: u64 = 250;

/// Hard row-request ceiling for one atomic latest-range page.
///
/// The implementation merges one bounded lookahead candidate in addition to
/// the requested rows. Keeping this ceiling explicit prevents caller-supplied
/// capacities from turning a read into an allocation panic or unbounded
/// synchronous work.
pub const LATEST_CF_RANGE_PAGE_MAX_ROWS: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionedValue {
    seq: Seq,
    value: Vec<u8>,
}

type VersionChain = Vec<VersionedValue>;
type RowTable = BTreeMap<ColumnFamily, BTreeMap<Vec<u8>, VersionChain>>;

/// One CF/key read requested against a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfRead {
    pub cf: ColumnFamily,
    pub key: Vec<u8>,
}

impl CfRead {
    pub fn new(cf: ColumnFamily, key: impl Into<Vec<u8>>) -> Self {
        Self {
            cf,
            key: key.into(),
        }
    }
}

/// One candidate-bounded page from an atomic latest committed view.
///
/// `resume_after` is the last emitted candidate key, including tombstones.
/// Callers must use it as an exclusive cursor when `more` is true, even when
/// `rows` is empty. `examined_rows` counts logical candidates merged across
/// the latest row/router view, including at most one lookahead candidate
/// beyond `resume_after`; it is not a count of physical SST rows or bytes.
/// `more` is exact from that bounded lookahead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestCfRangePage {
    pub snapshot_seq: Seq,
    pub rows: Vec<(Vec<u8>, Vec<u8>)>,
    pub resume_after: Option<Vec<u8>>,
    pub more: bool,
    pub examined_rows: usize,
}

pub fn tombstone_value() -> Vec<u8> {
    TOMBSTONE_VALUE.to_vec()
}

pub fn is_tombstone_value(value: &[u8]) -> bool {
    value == TOMBSTONE_VALUE
}

/// Versioned row table with a single vault-wide sequence.
#[derive(Debug)]
pub struct VersionedCfStore {
    seqs: SeqAllocator,
    /// Max committed seq whose batch wrote at least one row in a CF that
    /// feeds derived search content (issue #1100). Advances inside the row
    /// write lock *before* the seq becomes visible, so any reader that
    /// observes a content commit's seq also observes its watermark.
    derived_content_seq: AtomicU64,
    /// Max committed search-input sequence for each exact panel generation.
    ///
    /// Persisted search artifacts are panel-scoped, so freshness must not be
    /// invalidated by Base/Slot writes belonging to another panel. This map is
    /// rebuilt from the same ordered MVCC batches during recovery and advances
    /// under the row-table write lock before the corresponding sequence is
    /// published (#1841).
    panel_content_seqs: RwLock<BTreeMap<u32, Seq>>,
    next_lease_id: AtomicU64,
    rows: RwLock<RowTable>,
    router: RwLock<Option<CfRouter>>,
    router_latest_readback: AtomicBool,
    router_eager_lookup_on_refresh: AtomicBool,
    read_barriers: RwLock<Vec<ReadBarrier>>,
    leases: LeaseRegistry,
    resource_counters: Arc<ResourceCounters>,
    snapshot_gc: SnapshotGcReclaimer,
    snapshot_gc_counters: SnapshotGcCounters,
}

impl VersionedCfStore {
    pub fn new(start_seq: Seq) -> Self {
        Self {
            seqs: SeqAllocator::new(start_seq),
            derived_content_seq: AtomicU64::new(0),
            panel_content_seqs: RwLock::new(BTreeMap::new()),
            next_lease_id: AtomicU64::new(0),
            rows: RwLock::new(BTreeMap::new()),
            router: RwLock::new(None),
            router_latest_readback: AtomicBool::new(false),
            router_eager_lookup_on_refresh: AtomicBool::new(true),
            read_barriers: RwLock::new(Vec::new()),
            leases: LeaseRegistry::default(),
            resource_counters: Arc::new(ResourceCounters::default()),
            snapshot_gc: SnapshotGcReclaimer::default(),
            snapshot_gc_counters: SnapshotGcCounters::default(),
        }
    }

    pub fn new_with_router(start_seq: Seq, router: CfRouter) -> Self {
        Self::new_with_router_and_policy(start_seq, router, false, true)
    }

    pub fn new_with_router_and_policy(
        start_seq: Seq,
        router: CfRouter,
        router_latest_readback: bool,
        eager_lookup_on_refresh: bool,
    ) -> Self {
        let resource_counters = router.resource_counters();
        Self {
            seqs: SeqAllocator::new(start_seq),
            derived_content_seq: AtomicU64::new(0),
            panel_content_seqs: RwLock::new(BTreeMap::new()),
            next_lease_id: AtomicU64::new(0),
            rows: RwLock::new(BTreeMap::new()),
            router: RwLock::new(Some(router)),
            router_latest_readback: AtomicBool::new(router_latest_readback),
            router_eager_lookup_on_refresh: AtomicBool::new(eager_lookup_on_refresh),
            read_barriers: RwLock::new(Vec::new()),
            leases: LeaseRegistry::default(),
            resource_counters,
            snapshot_gc: SnapshotGcReclaimer::default(),
            snapshot_gc_counters: SnapshotGcCounters::default(),
        }
    }

    pub fn new_with_router_latest_readback(start_seq: Seq, router: CfRouter) -> Self {
        Self::new_with_router_and_policy(start_seq, router, true, false)
    }

    /// Retires already-proven compaction input SSTs from the served CF levels
    /// and only then deletes them physically.
    ///
    /// Physical SST deletion costs 10–300 ms per file on a Windows host with
    /// real-time scanning, and a bounded compaction reclaims up to 512 of them.
    /// [`Self::refresh_router_cfs_after_reclaim`] paid that entire cost inside
    /// the single global router write lock, which every read *and* every
    /// `commit_batch` must pass through — one reclaim blocked the whole vault
    /// for as long as 108 s (issue #1806, #1829).
    ///
    /// The split here follows the standard LSM contract (RocksDB's
    /// `PurgeObsoleteFiles` is explicitly documented as *"not necessary to hold
    /// the mutex"*): make the new view visible under a short exclusive hold,
    /// then purge the now-unreferenced files with no lock held.
    ///
    /// Safety of the ordering rests on two facts:
    /// 1. Readers hold the router **read** lock across their SST reads, so
    ///    taking the write lock drains every in-flight mapping.
    /// 2. After the swap the retired paths are absent from every level, so no
    ///    reader can newly open them.
    ///
    /// Together those mean no mapping can exist for a retired file once the
    /// lock is released, which is exactly the precondition `remove_file` needs
    /// on Windows. `doomed` must already be canonicalized and validated by the
    /// caller — that work is filesystem I/O and stays outside the lock too.
    pub(crate) fn retire_then_purge_cf_inputs(
        &self,
        cfs: &[ColumnFamily],
        operation: &'static str,
        doomed: &BTreeSet<std::path::PathBuf>,
    ) -> Result<usize> {
        let mut unique = cfs.to_vec();
        unique.sort();
        unique.dedup();
        let cf_names = unique
            .iter()
            .map(|cf| cf.name())
            .collect::<Vec<_>>()
            .join(",");
        let eager_lookup = self.router_eager_lookup_on_refresh.load(Ordering::Acquire);

        // ---- Phase A: exclusive, metadata only ----------------------------
        // The guard lives only inside this block: Phase B below MUST run with
        // the router lock released, which is the entire point of the split.
        let lock_wait_started = Instant::now();
        let (lock_wait_ms, held_ms) = {
            let mut guard = self.router.write().expect("mvcc router poisoned");
            let lock_wait_ms = lock_wait_started.elapsed().as_millis();
            let held_started = Instant::now();
            let Some(router) = guard.as_mut() else {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "{operation}: physical SST reclaim requires a live CF router; cfs={cf_names}"
                )));
            };
            router.load_existing_cfs_excluding(&unique, eager_lookup, doomed)?;
            for path in doomed {
                crate::sst::invalidate_reader_canonical(path);
            }
            (lock_wait_ms, held_started.elapsed().as_millis())
        };
        tracing::info!(
            code = "CALYX_ASTER_CF_INPUT_RETIRE_DONE",
            operation,
            cfs = %cf_names,
            retired_files = doomed.len(),
            eager_lookup_on_refresh = eager_lookup,
            lock_wait_ms,
            exclusive_hold_ms = held_ms,
            exclusive_hold_warn_ms = EXCLUSIVE_ROUTER_RECLAIM_WARN_MS,
            "retired compaction inputs from the served CF level under a metadata-only exclusive hold"
        );
        if held_ms > u128::from(EXCLUSIVE_ROUTER_RECLAIM_WARN_MS) {
            tracing::warn!(
                code = "CALYX_ASTER_CF_INPUT_RETIRE_SLOW",
                operation,
                cfs = %cf_names,
                retired_files = doomed.len(),
                exclusive_hold_ms = held_ms,
                exclusive_hold_warn_ms = EXCLUSIVE_ROUTER_RECLAIM_WARN_MS,
                "exclusive Calyx router write lock exceeded the maintenance budget during the metadata-only level swap"
            );
        }

        // ---- Phase B: physical deletion, no lock held ---------------------
        let purge_started = Instant::now();
        let mut reclaimed = 0_usize;
        let mut retries = 0_u32;
        for path in doomed {
            retries = retries.saturating_add(purge_retired_sst(path, operation)?);
            reclaimed += 1;
        }
        tracing::info!(
            code = "CALYX_ASTER_CF_INPUT_PURGE_DONE",
            operation,
            cfs = %cf_names,
            reclaimed_files = reclaimed,
            sharing_retries = retries,
            elapsed_ms = purge_started.elapsed().as_millis(),
            "purged retired compaction inputs with no router lock held"
        );
        Ok(reclaimed)
    }

    pub(crate) fn refresh_router_cfs_after_reclaim<T>(
        &self,
        cfs: &[ColumnFamily],
        operation: &'static str,
        reclaim: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut unique = cfs.to_vec();
        unique.sort();
        unique.dedup();
        if unique.is_empty() {
            return reclaim();
        }
        let eager_lookup = self.router_eager_lookup_on_refresh.load(Ordering::Acquire);
        let cf_names = unique
            .iter()
            .map(|cf| cf.name())
            .collect::<Vec<_>>()
            .join(",");
        let started_at = std::time::Instant::now();
        let mut router = self.router.write().expect("mvcc router poisoned");
        let Some(router) = router.as_mut() else {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "{operation}: physical SST reclaim requires a live CF router; cfs={cf_names}"
            )));
        };
        tracing::info!(
            code = "CALYX_ASTER_ROUTER_RECLAIM_REFRESH_START",
            operation,
            cfs = %cf_names,
            eager_lookup_on_refresh = eager_lookup,
            "starting exclusive Calyx router reclaim refresh"
        );
        let reclaim_result = reclaim();
        let refresh_result = router.load_existing_cfs_with_lookup_policy(&unique, eager_lookup);
        match (reclaim_result, refresh_result) {
            (Ok(value), Ok(())) => {
                let elapsed = started_at.elapsed();
                let elapsed_ms = elapsed.as_millis();
                tracing::info!(
                    code = "CALYX_ASTER_ROUTER_RECLAIM_REFRESH_DONE",
                    operation,
                    cfs = %cf_names,
                    eager_lookup_on_refresh = eager_lookup,
                    elapsed_ms,
                    exclusive_hold_warn_ms = EXCLUSIVE_ROUTER_RECLAIM_WARN_MS,
                    "completed exclusive Calyx router reclaim refresh"
                );
                if elapsed_ms > u128::from(EXCLUSIVE_ROUTER_RECLAIM_WARN_MS) {
                    tracing::warn!(
                        code = "CALYX_ASTER_ROUTER_RECLAIM_REFRESH_SLOW",
                        operation,
                        cfs = %cf_names,
                        elapsed_ms,
                        exclusive_hold_warn_ms = EXCLUSIVE_ROUTER_RECLAIM_WARN_MS,
                        "exclusive Calyx router write lock was held beyond the maintenance budget during reclaim+refresh; reads on these CFs were blocked for the duration"
                    );
                }
                Ok(value)
            }
            (Err(error), Ok(())) => {
                tracing::error!(
                    code = error.code,
                    operation,
                    cfs = %cf_names,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    error = %error,
                    "Calyx physical SST reclaim failed; router was refreshed to the post-attempt disk state"
                );
                Err(error)
            }
            (Ok(_), Err(error)) => {
                tracing::error!(
                    code = error.code,
                    operation,
                    cfs = %cf_names,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    error = %error,
                    "Calyx router refresh failed after physical SST reclaim"
                );
                Err(error)
            }
            (Err(reclaim_error), Err(refresh_error)) => {
                tracing::error!(
                    code = "CALYX_ASTER_ROUTER_RECLAIM_REFRESH_FAILED",
                    operation,
                    cfs = %cf_names,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    reclaim_error = %reclaim_error,
                    refresh_error = %refresh_error,
                    "Calyx physical SST reclaim and router refresh both failed"
                );
                Err(CalyxError::aster_corrupt_shard(format!(
                    "{operation}: physical SST reclaim failed and router refresh could not prove the serving view; reclaim_error=[{}: {}] refresh_error=[{}: {}]",
                    reclaim_error.code,
                    reclaim_error.message,
                    refresh_error.code,
                    refresh_error.message
                )))
            }
        }
    }

    /// Lists the physical `SlotId`s present as `cf/slot_*` directories.
    pub(crate) fn present_slot_cf_ids(&self) -> Result<BTreeSet<SlotId>> {
        let router = self.router.read().expect("mvcc router poisoned");
        let Some(router) = router.as_ref() else {
            return Err(CalyxError::aster_corrupt_shard(
                "slot CF enumeration requires a live CF router",
            ));
        };
        router.present_slot_cf_ids()
    }

    /// Retires one column family under the exclusive router write lock,
    /// mirroring the compaction reclaim hold-warn budget so an over-long
    /// exclusive hold is surfaced. The lock is taken per call, so a caller
    /// retiring several CFs bounds each exclusive hold to one CF (#1806).
    pub(crate) fn retire_router_cf(
        &self,
        cf: ColumnFamily,
        operation: &'static str,
    ) -> Result<RetiredCfPhysical> {
        let started_at = Instant::now();
        let mut router = self.router.write().expect("mvcc router poisoned");
        let Some(router) = router.as_mut() else {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "{operation}: physical CF retire requires a live CF router"
            )));
        };
        let physical = router.retire_cf(cf)?;
        let elapsed_ms = started_at.elapsed().as_millis();
        if elapsed_ms > u128::from(EXCLUSIVE_ROUTER_RECLAIM_WARN_MS) {
            tracing::warn!(
                code = "CALYX_ASTER_ROUTER_RETIRE_SLOW",
                operation,
                cf = cf.name(),
                elapsed_ms,
                exclusive_hold_warn_ms = EXCLUSIVE_ROUTER_RECLAIM_WARN_MS,
                "exclusive Calyx router write lock was held beyond the maintenance budget during CF retire; reads on this CF were blocked for the duration"
            );
        }
        Ok(physical)
    }

    /// Latest committed sequence.
    pub fn current_seq(&self) -> Seq {
        self.seqs.current()
    }

    pub fn set_start_seq(&self, seq: Seq) -> Result<()> {
        self.seqs.set_start_seq(seq)
    }

    pub fn advance_to_at_least(&self, seq: Seq) {
        self.seqs.advance_to_at_least(seq);
    }

    /// Latest committed seq whose batch wrote derived-search-content inputs.
    /// See [`crate::cf::ColumnFamily::feeds_persistent_search_index`].
    pub fn derived_content_seq(&self) -> Seq {
        self.derived_content_seq.load(Ordering::Acquire)
    }

    /// Raises the derived-content watermark to a durably recorded floor
    /// (vault MANIFEST readback, foreign-process checkpoint refresh).
    pub fn advance_derived_content_seq_to_at_least(&self, seq: Seq) {
        self.derived_content_seq.fetch_max(seq, Ordering::AcqRel);
    }

    /// Pins a snapshot at the latest committed sequence.
    ///
    /// The lease is registered for oldest-pinned-seq gap accounting; it leaves
    /// the registry on [`Self::release_lease`] or when its `max_age_ms` expires.
    pub fn pin_snapshot(
        &self,
        freshness: Freshness,
        clock: &dyn Clock,
        max_age_ms: u64,
    ) -> Snapshot {
        let seq = self.current_seq();
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::AcqRel) + 1;
        let lease = ReaderLease::new(lease_id, seq, clock.now(), max_age_ms);
        self.leases.register(lease);
        Snapshot::new(seq, freshness, lease)
            .with_derived_content_seq(self.derived_content_seq_at(seq))
    }

    /// Pins the latest committed view with the search-input watermark for one
    /// exact panel generation.
    ///
    /// The row-table read lock makes `(seq, panel_content_seq)` one atomic
    /// observation with respect to commits. Latest-only recovery is supported:
    /// model-3 manifests provide its exact checkpointed panel baseline and WAL
    /// replay derives every later change.
    pub fn pin_snapshot_for_panel(
        &self,
        panel_version: u32,
        freshness: Freshness,
        clock: &dyn Clock,
        max_age_ms: u64,
    ) -> Result<Snapshot> {
        let _table = self.rows.read().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC row-table lock was poisoned while pinning panel search freshness",
            )
        })?;
        let seq = self.current_seq();
        let panel_content_seq = self
            .panel_content_seqs
            .read()
            .map_err(|_| {
                CalyxError::aster_corrupt_shard(
                    "MVCC panel-content watermark lock was poisoned while pinning search freshness",
                )
            })?
            .get(&panel_version)
            .copied()
            .unwrap_or_default()
            .min(seq);
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::AcqRel) + 1;
        let lease = ReaderLease::new(lease_id, seq, clock.now(), max_age_ms);
        self.leases.register(lease);
        Ok(Snapshot::new(seq, freshness, lease).with_derived_content_seq(panel_content_seq))
    }

    /// Atomic copy of the panel-scoped search-input watermarks. Manifest
    /// publication clamps these values to its durable sequence before writing
    /// them, so observing a newer concurrent commit can only conservatively
    /// overstate a panel watermark, never accept a stale index (#1841).
    pub(crate) fn panel_content_seqs_snapshot(&self) -> Result<BTreeMap<u32, Seq>> {
        let _table = self.rows.read().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC row-table lock was poisoned while snapshotting panel watermarks",
            )
        })?;
        self.panel_content_seqs
            .read()
            .map_err(|_| {
                CalyxError::aster_corrupt_shard(
                    "MVCC panel-content watermark lock was poisoned while snapshotting manifest state",
                )
            })
            .map(|watermarks| watermarks.clone())
    }

    /// Installs a manifest-vouched panel watermark floor before WAL replay.
    pub(crate) fn advance_panel_content_seqs_to_at_least(
        &self,
        floors: &BTreeMap<u32, Seq>,
    ) -> Result<()> {
        let mut watermarks = self.panel_content_seqs.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC panel-content watermark lock was poisoned while adopting manifest state",
            )
        })?;
        for (panel_version, seq) in floors {
            let watermark = watermarks.entry(*panel_version).or_default();
            *watermark = (*watermark).max(*seq);
        }
        Ok(())
    }

    /// One-time migration from a vault-global model. Every panel physically
    /// observed in restored Base history, plus the configured active panel, is
    /// conservatively advanced to the proven global floor. This may force one
    /// rebuild but cannot let stale panel content pass after migration.
    pub(crate) fn migrate_panel_content_seqs_to_at_least(
        &self,
        floor: Seq,
        active_panel_version: Option<u32>,
    ) -> Result<()> {
        let table = self.rows.read().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC row-table lock was poisoned while migrating panel watermarks",
            )
        })?;
        let mut panels = self
            .panel_content_seqs
            .read()
            .map_err(|_| {
                CalyxError::aster_corrupt_shard(
                    "MVCC panel-content watermark lock was poisoned while reading migration state",
                )
            })?
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        if let Some(panel_version) = active_panel_version {
            panels.insert(panel_version);
        }
        if let Some(base_rows) = table.get(&ColumnFamily::Base) {
            for (key, versions) in base_rows {
                for version in versions
                    .iter()
                    .filter(|version| version.seq <= self.current_seq())
                {
                    if !is_tombstone_value(&version.value) {
                        panels.insert(panel_from_base_value(key, &version.value)?);
                    }
                }
            }
        }
        drop(table);
        self.advance_affected_panel_content_seqs(&panels, floor)
    }

    /// Pins a reader lease at an explicit historical `seq` (time-travel). The
    /// lease participates in oldest-pinned-seq accounting so version GC cannot
    /// reclaim versions at or below `seq` until it is released.
    pub fn pin_snapshot_at(
        &self,
        seq: Seq,
        freshness: Freshness,
        clock: &dyn Clock,
        max_age_ms: u64,
    ) -> Snapshot {
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::AcqRel) + 1;
        let lease = ReaderLease::new(lease_id, seq, clock.now(), max_age_ms);
        self.leases.register(lease);
        Snapshot::new(seq, freshness, lease)
            .with_derived_content_seq(self.derived_content_seq_at(seq))
    }

    /// Derived-content watermark as knowable for a pin at `seq`, clamped
    /// fail-closed: if the live watermark exceeds `seq` (content committed
    /// after the pin, or a historical pin below the watermark), the watermark
    /// at `seq` is unknowable from the live counter and the pin falls back to
    /// `seq` itself — the pre-#1100 exact-equality behavior, never laxer.
    fn derived_content_seq_at(&self, seq: Seq) -> Seq {
        self.derived_content_seq().min(seq)
    }

    /// Releases one pinned reader lease; returns whether it was still live.
    pub fn release_lease(&self, lease_id: u64) -> bool {
        self.leases.release(lease_id)
    }

    /// Live reader-lease view at `now` for resource accounting.
    pub fn lease_view(&self, now: Ts) -> LeaseView {
        self.leases.live_view(now)
    }

    /// Background snapshot-GC tick hook, intended for the 1 s GC scheduler.
    pub fn snapshot_gc_tick(&self, clock: &dyn Clock, max_gap_seqs: u64) -> SnapshotGcTick {
        let now = clock.now();
        let aborted_readers = self.leases.check_and_abort_expired(now);
        let gap_alert = self.leases.check_gap(self.current_seq(), now, max_gap_seqs);
        let metrics = self.leases.metrics(self.current_seq(), now);
        SnapshotGcTick {
            aborted_readers,
            gap_alert,
            metrics,
        }
    }

    /// Backpressure counters shared with this store's CF router.
    pub fn resource_counters(&self) -> &ResourceCounters {
        &self.resource_counters
    }

    /// Live memtable byte-cap status shared with resource readback.
    pub fn memtable_status(&self) -> MemtableStatus {
        let router = self.router.read().expect("mvcc router poisoned");
        let Some(router) = router.as_ref() else {
            return MemtableStatus::default();
        };
        let per_cf = router
            .memtable_usage_by_cf()
            .into_iter()
            .map(|(cf, usage)| MemtableCfStatus {
                cf: cf.name().to_string(),
                used_bytes: usage.used_bytes as u64,
                cap_bytes: usage.cap_bytes as u64,
                high_water_bytes: usage.high_water_bytes as u64,
                flush_triggered: usage.flush_triggered,
            })
            .collect::<Vec<_>>();
        let total_used_bytes = per_cf.iter().map(|cf| cf.used_bytes).sum();
        let total_cap_bytes = per_cf.iter().map(|cf| cf.cap_bytes).sum();
        MemtableStatus {
            total_used_bytes,
            total_cap_bytes,
            per_cf,
        }
    }

    /// Admission check for rows that cannot fit even in an empty memtable.
    pub fn ensure_memtable_admission<I, K, V>(&self, rows: I) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.router
            .read()
            .expect("mvcc router poisoned")
            .as_ref()
            .map_or(Ok(()), |router| router.ensure_batch_admitted(rows))
    }

    /// Atomically commits one write group across any number of CFs.
    pub fn commit_batch<I, K, V>(&self, rows: I) -> Result<Seq>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(cf, key, value)| (cf, key.into(), value.into()))
            .collect();
        if rows.is_empty() {
            return Ok(self.current_seq());
        }

        let mut table = self.rows.write().map_err(|_| {
            CalyxError::aster_corrupt_shard("MVCC row-table lock was poisoned during atomic commit")
        })?;
        let mut router = self.router.write().map_err(|_| {
            CalyxError::aster_corrupt_shard("MVCC router lock was poisoned during atomic commit")
        })?;
        let latest_router = if self.router_latest_readback.load(Ordering::Acquire) {
            router.as_ref()
        } else {
            None
        };
        let affected_panels = search_panels_affected_by_batch(
            &table,
            latest_router,
            self.current_seq(),
            &rows,
            PanelAttribution::Strict,
        )?;
        // Advance the derived-content watermark BEFORE allocating the seq:
        // readers pin without taking the row lock, so a reader that observes
        // this commit's seq must already observe its watermark (issue #1100).
        // All allocations happen under the row write lock held here, so the
        // next allocated seq is exactly current + 1 (asserted by the vault
        // commit path's time-index seqno prediction).
        if rows
            .iter()
            .any(|(cf, _, _)| cf.feeds_persistent_search_index())
        {
            self.derived_content_seq
                .fetch_max(self.current_seq() + 1, Ordering::AcqRel);
        }
        self.advance_affected_panel_content_seqs(&affected_panels, self.current_seq() + 1)?;
        let seq = self.seqs.allocate();
        for (cf, key, value) in &rows {
            table
                .entry(*cf)
                .or_default()
                .entry(key.clone())
                .or_default()
                .push(VersionedValue {
                    seq,
                    value: value.clone(),
                });
        }

        if let Some(router) = router.as_mut() {
            // Publish the authoritative version chains and their sequence
            // before attempting the fallible router projection. Both write
            // guards remain held, so readers still observe one atomic latest
            // transition. If a put or flush fails part-way through the router
            // batch, every logical row already exists at `seq` and masks any
            // partial router state as soon as the guards are released.
            for (cf, key, value) in &rows {
                if let Err(error) = router.put_at(*cf, key, value, seq) {
                    tracing::error!(
                        code = "CALYX_MVCC_ROUTER_PUBLICATION_RECONCILIATION_REQUIRED",
                        committed_seq = seq,
                        cf = cf.name(),
                        key_len = key.len(),
                        value_len = value.len(),
                        router_error_code = error.code,
                        router_error = %error,
                        "MVCC rows committed atomically but the router projection failed; the committed sequence must be reconciled before retry"
                    );
                    return Err(CalyxError {
                        code: "CALYX_MVCC_ROUTER_PUBLICATION_RECONCILIATION_REQUIRED",
                        message: format!(
                            "MVCC batch is committed at seq {seq}, but router publication failed at {} key_len={} value_len={}: error[{}]: {}",
                            cf.name(),
                            key.len(),
                            value.len(),
                            error.code,
                            error.message
                        ),
                        remediation: "treat committed_seq as applied; reconcile from the authoritative WAL/row-table state before retrying, and inspect the router disk/crypto error",
                    });
                }
            }
        }
        Ok(seq)
    }

    /// Restores one durable write group at its original sequence before live writes begin.
    pub fn restore_batch<I, K, V>(&self, seq: Seq, rows: I) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        self.restore_batch_with_attribution(seq, rows, PanelAttribution::Strict)
    }

    /// Restores checkpoint SST rows. A model-3 manifest is the watermark SoT,
    /// because coalesced SST rows carry the file's maximum sequence rather than
    /// each original commit sequence. Older models re-derive conservatively
    /// while tolerating only an unreferenced historical quantized slot.
    pub(crate) fn restore_manifested_batch<I, K, V>(
        &self,
        seq: Seq,
        rows: I,
        migrate_panel_model: bool,
    ) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let attribution = if migrate_panel_model {
            PanelAttribution::HistoricalMigration
        } else {
            PanelAttribution::ManifestBaseline
        };
        self.restore_batch_with_attribution(seq, rows, attribution)
    }

    fn restore_batch_with_attribution<I, K, V>(
        &self,
        seq: Seq,
        rows: I,
        attribution: PanelAttribution,
    ) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(cf, key, value)| (cf, key.into(), value.into()))
            .collect();
        let mut table = self.rows.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC row-table lock was poisoned during atomic recovery restore",
            )
        })?;
        let router = self.router.read().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC router lock was poisoned during atomic recovery restore",
            )
        })?;
        let latest_router = if attribution == PanelAttribution::Strict
            && self.router_latest_readback.load(Ordering::Acquire)
        {
            router.as_ref()
        } else {
            None
        };
        let affected_panels = search_panels_affected_by_batch(
            &table,
            latest_router,
            seq.saturating_sub(1),
            &rows,
            attribution,
        )?;
        if rows
            .iter()
            .any(|(cf, _, _)| cf.feeds_persistent_search_index())
        {
            self.derived_content_seq.fetch_max(seq, Ordering::AcqRel);
        }
        self.advance_affected_panel_content_seqs(&affected_panels, seq)?;
        for (cf, key, value) in rows {
            table
                .entry(cf)
                .or_default()
                .entry(key)
                .or_default()
                .push(VersionedValue { seq, value });
        }
        Ok(())
    }

    /// Publishes recovered foreign-process batches and their final sequence as
    /// one atomic latest-view transition.
    ///
    /// Keeping the row write lock until `final_seq` is visible prevents a
    /// latest reader from observing restored version chains whose sequence is
    /// still in the future relative to [`Self::current_seq`].
    pub(crate) fn restore_batches_and_advance<I, R>(&self, batches: I, final_seq: Seq) -> Result<()>
    where
        I: IntoIterator<Item = (Seq, R)>,
        R: IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
    {
        self.restore_batches_and_advance_with_manifest_floor(batches, final_seq, None, false)
    }

    pub(crate) fn restore_recovered_batches_and_advance<I, R>(
        &self,
        batches: I,
        final_seq: Seq,
        manifested_through_seq: Seq,
        migrate_panel_model: bool,
    ) -> Result<()>
    where
        I: IntoIterator<Item = (Seq, R)>,
        R: IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
    {
        self.restore_batches_and_advance_with_manifest_floor(
            batches,
            final_seq,
            Some(manifested_through_seq),
            migrate_panel_model,
        )
    }

    fn restore_batches_and_advance_with_manifest_floor<I, R>(
        &self,
        batches: I,
        final_seq: Seq,
        manifested_through_seq: Option<Seq>,
        migrate_panel_model: bool,
    ) -> Result<()>
    where
        I: IntoIterator<Item = (Seq, R)>,
        R: IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
    {
        let batches = batches
            .into_iter()
            .map(|(seq, rows)| (seq, rows.into_iter().collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        if batches.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            || batches.iter().any(|(seq, _rows)| *seq > final_seq)
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "recovered MVCC batches must have strictly increasing sequences at or below final sequence {final_seq}"
            )));
        }
        let mut table = self.rows.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC row-table lock was poisoned during atomic recovery restore",
            )
        })?;
        let router = self.router.read().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC router lock was poisoned during recovered-batch publication",
            )
        })?;
        let published_seq = self.current_seq();
        if final_seq < published_seq {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "recovered MVCC final sequence {final_seq} regresses published sequence {published_seq}"
            )));
        }
        for (seq, rows) in batches
            .into_iter()
            .filter(|(seq, _rows)| *seq > published_seq)
        {
            let attribution = if manifested_through_seq.is_some_and(|floor| seq <= floor) {
                if migrate_panel_model {
                    PanelAttribution::HistoricalMigration
                } else {
                    PanelAttribution::ManifestBaseline
                }
            } else {
                PanelAttribution::Strict
            };
            let affected_panels = search_panels_affected_by_batch(
                &table,
                if attribution == PanelAttribution::Strict
                    && self.router_latest_readback.load(Ordering::Acquire)
                {
                    router.as_ref()
                } else {
                    None
                },
                seq.saturating_sub(1),
                &rows,
                attribution,
            )?;
            if rows
                .iter()
                .any(|(cf, _, _)| cf.feeds_persistent_search_index())
            {
                self.derived_content_seq.fetch_max(seq, Ordering::AcqRel);
            }
            self.advance_affected_panel_content_seqs(&affected_panels, seq)?;
            for (cf, key, value) in rows {
                table
                    .entry(cf)
                    .or_default()
                    .entry(key)
                    .or_default()
                    .push(VersionedValue { seq, value });
            }
        }
        self.seqs.advance_to_at_least(final_seq);
        Ok(())
    }

    fn advance_affected_panel_content_seqs(&self, panels: &BTreeSet<u32>, seq: Seq) -> Result<()> {
        if panels.is_empty() {
            return Ok(());
        }
        let mut watermarks = self.panel_content_seqs.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC panel-content watermark lock was poisoned during commit",
            )
        })?;
        for panel_version in panels {
            let watermark = watermarks.entry(*panel_version).or_default();
            *watermark = (*watermark).max(seq);
        }
        Ok(())
    }

    /// Whether any version (live or tombstone) exists for `cf`/`key` in the
    /// row table. Recovery-time physical coverage checks only (issue #1132);
    /// snapshot reads must keep using the seq-visible accessors.
    pub(crate) fn has_any_version(&self, cf: ColumnFamily, key: &[u8]) -> bool {
        self.rows
            .read()
            .expect("mvcc row table poisoned")
            .get(&cf)
            .is_some_and(|rows| rows.contains_key(key))
    }

    pub fn flush_all_cfs(&self) -> Result<Vec<SstSummary>> {
        let mut router = self.router.write().expect("mvcc router poisoned");
        let Some(router) = router.as_mut() else {
            return Ok(Vec::new());
        };
        // Read the watermark while holding the router lock. Commits acquire
        // that same write lock before sequence publication and retain it until
        // every router put completes, so every commit at or below
        // `current_seq()` has already routed its rows. A commit still waiting
        // for this lock has not published its sequence (issue #1138).
        let commit_watermark = self.current_seq();
        router.flush_pending_at(commit_watermark)
    }

    pub fn install_read_barrier(&self, barrier: ReadBarrier) {
        let mut barriers = self
            .read_barriers
            .write()
            .expect("mvcc read barriers poisoned");
        barriers.retain(|existing| existing.id() != barrier.id());
        barriers.push(barrier);
    }

    pub fn remove_read_barrier(&self, id: &str) -> bool {
        let mut barriers = self
            .read_barriers
            .write()
            .expect("mvcc read barriers poisoned");
        let before = barriers.len();
        barriers.retain(|existing| existing.id() != id);
        barriers.len() != before
    }

    pub fn read_barriers(&self) -> Vec<ReadBarrier> {
        self.read_barriers
            .read()
            .expect("mvcc read barriers poisoned")
            .clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelAttribution {
    /// Live or WAL-tail mutation: every quantized slot must resolve to Base.
    Strict,
    /// Legacy checkpoint rows: derive a conservative migration baseline, but
    /// tolerate an already-orphaned slot awaiting physical GC.
    HistoricalMigration,
    /// Model-3 checkpoint rows: the manifest map is authoritative, and SST
    /// coalescing has erased per-row commit sequences, so do not re-attribute.
    ManifestBaseline,
}

fn search_panels_affected_by_batch(
    table: &RowTable,
    latest_router: Option<&CfRouter>,
    visible_seq: Seq,
    rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)],
    attribution: PanelAttribution,
) -> Result<BTreeSet<u32>> {
    if attribution == PanelAttribution::ManifestBaseline {
        return Ok(BTreeSet::new());
    }
    let mut panels = BTreeSet::new();
    let mut staged_base_panels = BTreeMap::<Vec<u8>, Option<u32>>::new();

    for (cf, key, value) in rows {
        if *cf != ColumnFamily::Base {
            continue;
        }
        if staged_base_panels.contains_key(key) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "atomic batch contains duplicate Base writes for one CxId key (key_len={}); exact panel freshness attribution is ambiguous",
                key.len()
            )));
        }
        let tombstone = is_tombstone_value(value);
        let panel_version = if tombstone {
            visible_base_panel(table, latest_router, key, visible_seq)?
        } else {
            Some(panel_from_base_value(key, value)?)
        };
        if let Some(panel_version) = panel_version {
            panels.insert(panel_version);
        }
        // A tombstone still invalidates the row's previous panel, but it does
        // not provide same-batch Base membership for a live Slot write.
        staged_base_panels.insert(key.clone(), if tombstone { None } else { panel_version });
    }

    for (cf, key, value) in rows {
        if !matches!(
            cf,
            ColumnFamily::Slot {
                kind: crate::cf::SlotFamilyKind::Quantized,
                ..
            }
        ) {
            continue;
        }
        let panel_version = match staged_base_panels.get(key) {
            Some(panel_version) => *panel_version,
            None => visible_base_panel(table, latest_router, key, visible_seq)?,
        };
        match panel_version {
            Some(panel_version) => {
                panels.insert(panel_version);
            }
            None if is_tombstone_value(value) => {
                // Deleting an already-unreferenced physical slot row cannot
                // change any panel-scoped search input.
            }
            None if attribution == PanelAttribution::HistoricalMigration => {
                tracing::debug!(
                    code = "CALYX_MVCC_ORPHAN_SLOT_RECOVERY_IGNORED",
                    cf = cf.name(),
                    key_len = key.len(),
                    visible_seq,
                    "ignored checkpointed quantized-slot residue with no visible Base row while migrating panel freshness; the row is not a search input and remains eligible for orphan-slot GC"
                );
            }
            None => {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "live quantized slot write has no visible or same-batch Base row (cf={}, key_len={}); refusing unscoped search-index mutation",
                    cf.name(),
                    key.len()
                )));
            }
        }
    }

    Ok(panels)
}

fn visible_base_panel(
    table: &RowTable,
    latest_router: Option<&CfRouter>,
    key: &[u8],
    seq: Seq,
) -> Result<Option<u32>> {
    let version = table
        .get(&ColumnFamily::Base)
        .and_then(|base| base.get(key))
        .and_then(|versions| versions.iter().rev().find(|version| version.seq <= seq));
    if let Some(version) = version {
        if is_tombstone_value(&version.value) {
            return Ok(None);
        }
        return panel_from_base_value(key, &version.value).map(Some);
    }
    let Some(value) = latest_router
        .map(|router| router.get(ColumnFamily::Base, key))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    if is_tombstone_value(&value) {
        return Ok(None);
    }
    panel_from_base_value(key, &value).map(Some)
}

fn panel_from_base_value(key: &[u8], value: &[u8]) -> Result<u32> {
    let header = crate::vault::encode::decode_header(value)?;
    if header.cx_id.as_bytes() != key {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "Base row key/header CxId mismatch while attributing panel freshness (key_len={})",
            key.len()
        )));
    }
    Ok(header.panel_version)
}

impl Default for VersionedCfStore {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Attempts on a retired SST before a purge is declared failed.
const RETIRED_SST_PURGE_ATTEMPTS: u32 = 5;

/// Deletes one retired SST, returning how many retries it needed.
///
/// The level swap in [`VersionedCfStore::retire_then_purge_cf_inputs`] already
/// drained every mapping, so the first attempt is expected to succeed. The
/// bounded retry only covers a straggler mapping that a non-router code path
/// still owns on Windows, where an open mapping makes `remove_file` fail. A
/// file that survives every attempt is reported as an error with the exact
/// path and OS error rather than being skipped: leaving a retired input on
/// disk lets a later cold open re-adopt it alongside its own compaction
/// output, so this must fail closed.
fn purge_retired_sst(path: &std::path::Path, operation: &'static str) -> Result<u32> {
    let mut last_error = None;
    for attempt in 0..RETIRED_SST_PURGE_ATTEMPTS {
        if attempt > 0 {
            crate::sst::invalidate_reader_canonical(path);
            std::thread::sleep(std::time::Duration::from_millis(20 << (attempt - 1)));
        }
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(attempt),
            // Already gone: the retirement is what makes this idempotent, so a
            // concurrent purge of the same path is success, not an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(attempt),
            Err(error) => {
                tracing::warn!(
                    code = "CALYX_ASTER_CF_INPUT_PURGE_RETRY",
                    operation,
                    path = %path.display(),
                    attempt = attempt + 1,
                    max_attempts = RETIRED_SST_PURGE_ATTEMPTS,
                    error = %error,
                    "retired SST is still mapped or locked; retrying purge"
                );
                last_error = Some(error);
            }
        }
    }
    Err(CalyxError::disk_pressure(format!(
        "{operation}: retired compaction input {} survived {RETIRED_SST_PURGE_ATTEMPTS} purge attempts: {}",
        path.display(),
        last_error.map_or_else(|| "unknown error".to_owned(), |error| error.to_string())
    )))
}
