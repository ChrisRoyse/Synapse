//! In-memory MVCC row table used to define the cross-CF snapshot contract.

mod gc;
mod read;
mod scan_pages;
use crate::cf::{CfRouter, ColumnFamily, KeyRange};
use crate::gc::{SnapshotGcCounters, SnapshotGcReclaimer, SnapshotGcTick};
use crate::mvcc::{
    Freshness, ReadBarrier, ReaderLease, SeqAllocator, Snapshot, read_barrier::first_blocking,
};
use crate::resource::{
    LeaseRegistry, LeaseView, MemtableCfStatus, MemtableStatus, ResourceCounters,
};
use crate::sst::SstSummary;
use calyx_core::{CalyxError, Clock, Result, Seq, Ts};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

const TOMBSTONE_VALUE: &[u8] = b"\0CALYX_ASTER_TOMBSTONE_V1";

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
                tracing::info!(
                    code = "CALYX_ASTER_ROUTER_RECLAIM_REFRESH_DONE",
                    operation,
                    cfs = %cf_names,
                    eager_lookup_on_refresh = eager_lookup,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "completed exclusive Calyx router reclaim refresh"
                );
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
        if let Some(router) = self.router.write().expect("mvcc router poisoned").as_mut() {
            // Rows written here belong to the seq allocated below (current + 1,
            // exact because all allocations happen under the row write lock
            // held here). A memtable flush triggered by these puts must carry
            // that commit watermark so the flush SST orders correctly against
            // durable batches (issue #1138).
            let commit_watermark = self.current_seq() + 1;
            for (cf, key, value) in &rows {
                router.put_at(*cf, key, value, commit_watermark)?;
            }
        }
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
        let seq = self.seqs.allocate();
        for (cf, key, value) in rows {
            table
                .entry(cf)
                .or_default()
                .entry(key)
                .or_default()
                .push(VersionedValue { seq, value });
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
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(cf, key, value)| (cf, key.into(), value.into()))
            .collect();
        let mut table = self.rows.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC row-table lock was poisoned during atomic recovery restore",
            )
        })?;
        if rows
            .iter()
            .any(|(cf, _, _)| cf.feeds_persistent_search_index())
        {
            self.derived_content_seq.fetch_max(seq, Ordering::AcqRel);
        }
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
            if rows
                .iter()
                .any(|(cf, _, _)| cf.feeds_persistent_search_index())
            {
                self.derived_content_seq.fetch_max(seq, Ordering::AcqRel);
            }
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
        // Read the watermark while holding the router lock: every commit at or
        // below `current_seq()` has already routed its rows into the memtables
        // (puts happen before seq allocation), and an in-flight commit whose
        // seq is not yet allocated only understates the watermark, which is
        // the safe direction (issue #1138).
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

impl Default for VersionedCfStore {
    fn default() -> Self {
        Self::new(0)
    }
}
