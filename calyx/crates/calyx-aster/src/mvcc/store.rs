//! In-memory MVCC row table used to define the cross-CF snapshot contract.

mod gc;
mod read;
mod scan_pages;
use crate::cf::{CfRouter, ColumnFamily, KeyRange, RetiredCfPhysical, RouterPutCost};
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

/// Whole microseconds since `started`, saturating rather than wrapping.
fn elapsed_us(started: &Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Fixed points inside one [`VersionedCfStore::commit_batch_timed`].
///
/// The vault's commit stage split stopped at this method's boundary, so an
/// MVCC apply that took 180 ms — 98.9% of its commit, against 2 ms of durable
/// WAL I/O on the same commit — reported one number and no way to choose among
/// its candidate causes (#1948). Each field below is one candidate, timed
/// independently, and [`Self::unattributed_us`] is the explicit remainder: "the
/// cost is in none of these" has to be a reportable outcome rather than an
/// absence of data, which is the lesson #1947 recorded when a 20 ms stage
/// turned out not to be the write everyone could see.
#[derive(Debug, Default, Clone, Copy)]
pub struct MvccCommitTimings {
    /// Draining the caller's iterator into owned rows.
    pub materialize_us: u64,
    /// Waiting for the row-table write lock.
    pub row_lock_wait_us: u64,
    /// CPU the committing thread actually burned inside the locked region.
    ///
    /// `locked_us` is wall clock, so it conflates "this commit did a lot of
    /// work under the locks" with "this commit was descheduled while holding
    /// them" — and the second blocks every other writer for no reason at all.
    /// The read-guard side of this was #1955; this is the same blind spot on
    /// the write side.
    ///
    /// Deliberately NOT measured across the lock *wait*: a blocked thread
    /// consumes no CPU by definition, so a CPU figure there would be ~0
    /// regardless and would assert starvation unconditionally. Only regions
    /// where the thread is supposed to be running are informative.
    ///
    /// `None` when the platform cannot report thread CPU time.
    pub locked_cpu_us: Option<u64>,
    /// Waiting for router **shard** write guards, summed over the batch's rows.
    ///
    /// Was the wait for one vault-wide router `RwLock`; the router is sharded
    /// per column family now, so this is the sum of the per-row shard waits
    /// inside the apply loop rather than one acquisition before it (#1950).
    /// Subtracted out of `router_apply_us` so the stage partition stays exact.
    pub router_lock_wait_us: u64,
    /// Search-panel attribution, which may read the router's latest view.
    pub panel_attribution_us: u64,
    /// Content-watermark advance and sequence allocation.
    pub watermark_us: u64,
    /// Row-table version-chain apply, including every key and value copy.
    pub row_apply_us: u64,
    /// Router projection apply, inclusive of the `put` prologue and memtable
    /// seals, but **not** the SST writes, which happen after the locks are
    /// released (#1949).
    pub router_apply_us: u64,
    /// What the router writes did beyond inserting into a memtable.
    pub put: RouterPutCost,
    /// Writing and installing sealed memtables as SSTs, measured with the
    /// row-table and router write locks **released** (#1949).
    ///
    /// Reported separately from `total_us` precisely because it is no longer
    /// inside the locked region: it is still the committing thread's wall
    /// clock, but no other reader or writer is blocked by it.
    pub sst_write_us: u64,
    /// Rows in the batch, so per-row cost is derivable from the event alone.
    pub rows: u32,
    /// The whole method, against which the parts are checked.
    pub total_us: u64,
}

/// Sub-stage names for the `mvcc` commit term, in [`MvccCommitTimings::stage_values`] order.
pub const MVCC_STAGE_NAMES: [&str; MVCC_STAGE_COUNT] = [
    "materialize",
    "row_lock_wait",
    "router_lock_wait",
    "panel_attribution",
    "watermark",
    "row_apply",
    "router_apply",
    "sst_write_unlocked",
    "unattributed",
];
/// Number of sub-stages in [`MVCC_STAGE_NAMES`].
pub const MVCC_STAGE_COUNT: usize = 9;

impl MvccCommitTimings {
    /// This commit's MVCC sub-stages in [`MVCC_STAGE_NAMES`] order.
    ///
    /// Partitions `total_us` exactly: `router_apply` excludes the flushes it
    /// triggered so that adding `router_flush` alongside it cannot double-count,
    /// and `unattributed` absorbs whatever is left.
    #[must_use]
    pub fn stage_values(&self) -> [u64; MVCC_STAGE_COUNT] {
        [
            self.materialize_us,
            self.row_lock_wait_us,
            self.router_lock_wait_us,
            self.panel_attribution_us,
            self.watermark_us,
            self.row_apply_us,
            self.router_apply_us,
            self.sst_write_us,
            self.unattributed_us(),
        ]
    }

    /// Time inside the commit that no field above claims.
    #[must_use]
    pub fn unattributed_us(&self) -> u64 {
        self.total_us
            .saturating_sub(self.materialize_us)
            .saturating_sub(self.row_lock_wait_us)
            .saturating_sub(self.router_lock_wait_us)
            .saturating_sub(self.panel_attribution_us)
            .saturating_sub(self.watermark_us)
            .saturating_sub(self.row_apply_us)
            .saturating_sub(self.router_apply_us)
            .saturating_sub(self.sst_write_us)
    }

    /// How long this commit **held** the vault's row-table and router write
    /// locks — the number that matters for everyone who is not the committing
    /// thread.
    ///
    /// Every term outside the guarded region is subtracted, and each one is
    /// load-bearing:
    ///
    /// * `materialize` — the batch is copied *before* either lock is taken.
    /// * `row_lock_wait` / `router_lock_wait` — **waiting** for a lock is not
    ///   holding it. This is the correction that matters: these terms reach
    ///   1.0 s on a 3-row commit stalled behind some other holder (#1950), and
    ///   the first version of this method subtracted only `sst_write_us`, so
    ///   such a commit reported having held the vault shut for 1,010,561 us
    ///   when its true hold was ~60 us. A metric that names the *victim* of a
    ///   stall as its *cause* is worse than no metric, because #1950 is
    ///   actively looking for the holder.
    /// * `sst_write` — the point of #1949; the SST write runs with both guards
    ///   released.
    ///
    /// Teardown is not subtracted because it falls outside `total_us`
    /// altogether: the owned batch and the sealed memtables drop as the frame
    /// unwinds, after the last measurement.
    /// Whether the locked region spent most of its wall clock not running.
    ///
    /// Same rule and same quantisation caveat as the read-guard `starved` flag
    /// (#1955): only asserted once the region is long enough that the
    /// scheduler-tick granularity of thread CPU time cannot produce the verdict
    /// by rounding alone.
    #[must_use]
    pub fn locked_starved(&self) -> bool {
        let locked_us = self.locked_us();
        locked_us >= 4 * WINDOWS_SCHEDULER_TICK_US
            && self
                .locked_cpu_us
                .is_some_and(|cpu| cpu.saturating_mul(2) < locked_us)
    }

    #[must_use]
    pub fn locked_us(&self) -> u64 {
        self.total_us
            .saturating_sub(self.materialize_us)
            .saturating_sub(self.row_lock_wait_us)
            .saturating_sub(self.router_lock_wait_us)
            .saturating_sub(self.sst_write_us)
    }
}

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
/// The rows of every column family routed to **one shard** of the row table.
///
/// Before #1950 this was the whole table under one vault-wide lock. It keeps
/// the same shape so that a call site holding a single shard's guard reads
/// `table.get(&cf)` exactly as it always did.
type RowTable = BTreeMap<ColumnFamily, BTreeMap<Vec<u8>, VersionChain>>;

/// Row-table shards, one per [`ColumnFamily::shard_index`].
///
/// The same shard map the CF router uses, deliberately: a commit takes the row
/// shard and then the router shard for the same column family, so sharing one
/// map means a commit to `Kv` contends with a reader of `Kv` at both layers and
/// with a reader of `Base` at neither (#1950).
const ROW_SHARDS: usize = ColumnFamily::SHARDS;

/// The row table's shards, allocated once at construction.
fn new_row_shards() -> Vec<RwLock<RowTable>> {
    (0..ROW_SHARDS)
        .map(|_| RwLock::new(BTreeMap::new()))
        .collect()
}

/// Which shard owns a column family's rows.
fn row_shard_index(cf: ColumnFamily) -> usize {
    cf.shard_index()
}

/// How long a row-table **read** guard may be held before it is reported.
///
/// `VersionedCfStore::rows` is one vault-wide `RwLock<RowTable>`, so a commit
/// taking it for write must wait for every in-flight reader to drain. #1950
/// measured commits waiting **1.0 s** for it on 2-18 row batches, and the
/// write side could say "I waited 1,010,497 us" while nothing anywhere could
/// say who held it.
///
/// 25 ms is well above any point read (the stalled commits do 52-139 us of
/// actual work) and far below the 125 ms - 1.4 s holds being hunted, so a
/// report here is a real finding rather than noise.
pub const ROW_READ_GUARD_WARN_US: u64 = 25_000;

/// Granularity of `GetThreadTimes` on this host, measured rather than assumed.
/// See [`thread_cpu_us`].
const WINDOWS_SCHEDULER_TICK_US: u64 = 15_625;

/// Every call site that takes the vault-wide row-table read guard.
///
/// This replaced a `&'static str`. The string was fine for a log line but
/// useless as a key: it cannot be indexed without hashing, it cannot be
/// enumerated (so a site with zero holds is indistinguishable from a site that
/// does not exist), and a typo produces a plausible-looking new site rather
/// than a compile error.
///
/// Enumerability is the property #1952 ask 3 turned out to need. That ask could
/// not be answered because `count_cf_latest` produced **no** guard events after
/// its conversion, and two readings fit equally: the converted sites never ran
/// in the window, or they now complete under the 25 ms budget. An
/// exception-only instrument cannot separate those, by construction — the
/// absence of an event is the same absence in both cases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowGuardSite {
    ReadLatest,
    ReadBatchLatest,
    ScanCfLatest,
    CountCfLatest,
    ScanCfRangeLatest,
    ScanCfRangePageLatest,
    ReadAt,
    ReadBatch,
    SeqForKeyAt,
    ChangedKeysAfterAt,
    ChangedBaseKeysAfterAtForPanel,
    ScanCfRangePageAt,
    PredecessorCfAt,
    OverlayTableRows,
    OverlayTableKeys,
    SnapshotGcDebt,
    PinSnapshotForPanel,
    PanelContentSeqsSnapshot,
    MigratePanelContentSeqsToAtLeast,
}

impl RowGuardSite {
    /// Every site, in declaration order. The census is indexed by position
    /// here, so this array is the contract that makes a zero-hold site
    /// reportable rather than invisible.
    pub const ALL: [Self; 19] = [
        Self::ReadLatest,
        Self::ReadBatchLatest,
        Self::ScanCfLatest,
        Self::CountCfLatest,
        Self::ScanCfRangeLatest,
        Self::ScanCfRangePageLatest,
        Self::ReadAt,
        Self::ReadBatch,
        Self::SeqForKeyAt,
        Self::ChangedKeysAfterAt,
        Self::ChangedBaseKeysAfterAtForPanel,
        Self::ScanCfRangePageAt,
        Self::PredecessorCfAt,
        Self::OverlayTableRows,
        Self::OverlayTableKeys,
        Self::SnapshotGcDebt,
        Self::PinSnapshotForPanel,
        Self::PanelContentSeqsSnapshot,
        Self::MigratePanelContentSeqsToAtLeast,
    ];

    pub const COUNT: usize = Self::ALL.len();

    /// The name used in `CALYX_ASTER_ROW_READ_GUARD_SLOW`. Unchanged from the
    /// previous string literals, so windows collected before and after this
    /// change remain directly comparable.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadLatest => "read_latest",
            Self::ReadBatchLatest => "read_batch_latest",
            Self::ScanCfLatest => "scan_cf_latest",
            Self::CountCfLatest => "count_cf_latest",
            Self::ScanCfRangeLatest => "scan_cf_range_latest",
            Self::ScanCfRangePageLatest => "scan_cf_range_page_latest",
            Self::ReadAt => "read_at",
            Self::ReadBatch => "read_batch",
            Self::SeqForKeyAt => "seq_for_key_at",
            Self::ChangedKeysAfterAt => "changed_keys_after_at",
            Self::ChangedBaseKeysAfterAtForPanel => "changed_base_keys_after_at_for_panel",
            Self::ScanCfRangePageAt => "scan_cf_range_page_at",
            Self::PredecessorCfAt => "predecessor_cf_at",
            Self::OverlayTableRows => "overlay_table_rows",
            Self::OverlayTableKeys => "overlay_table_keys",
            Self::SnapshotGcDebt => "snapshot_gc_debt",
            Self::PinSnapshotForPanel => "pin_snapshot_for_panel",
            Self::PanelContentSeqsSnapshot => "panel_content_seqs_snapshot",
            Self::MigratePanelContentSeqsToAtLeast => "migrate_panel_content_seqs_to_at_least",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Live counters for one [`RowGuardSite`].
///
/// Recorded on **every** hold, not only over-budget ones. That is the whole
/// point: `over_budget_holds == 0` with `holds == 0` means the path never ran,
/// and `over_budget_holds == 0` with `holds == 4_812` means it ran 4,812 times
/// and stayed inside the budget every time. Those are the two readings #1952
/// ask 3 could not separate.
#[derive(Debug, Default)]
struct RowGuardSiteCounters {
    holds: AtomicU64,
    total_held_us: AtomicU64,
    max_held_us: AtomicU64,
    over_budget_holds: AtomicU64,
    starved_holds: AtomicU64,
}

/// One site's counters, read back at an instant.
#[derive(Clone, Copy, Debug)]
pub struct RowGuardSiteCensus {
    pub site: RowGuardSite,
    pub holds: u64,
    pub total_held_us: u64,
    pub max_held_us: u64,
    pub over_budget_holds: u64,
    pub starved_holds: u64,
}

impl RowGuardSiteCensus {
    /// Mean hold in microseconds, or `None` when the site never ran.
    ///
    /// Deliberately `None` rather than `0.0`: a site that never ran has no mean
    /// hold, and reporting one as zero is the same category error as
    /// `Intact { count: 0 }` (#1956).
    pub fn mean_held_us(&self) -> Option<f64> {
        (self.holds > 0).then(|| self.total_held_us as f64 / self.holds as f64)
    }
}

/// Per-site row-guard counters for the whole vault.
#[derive(Debug)]
struct RowGuardCensus {
    sites: [RowGuardSiteCounters; RowGuardSite::COUNT],
}

impl Default for RowGuardCensus {
    fn default() -> Self {
        Self {
            sites: std::array::from_fn(|_| RowGuardSiteCounters::default()),
        }
    }
}

impl RowGuardCensus {
    fn record(&self, site: RowGuardSite, held_us: u64, over_budget: bool, starved: bool) {
        let counters = &self.sites[site.index()];
        // Relaxed throughout: these are independent monotonic tallies read for
        // diagnosis, never to make a decision, so no ordering between them is
        // load-bearing. Anything stronger would put a fence on the hottest read
        // path in the vault to buy nothing.
        counters.holds.fetch_add(1, Ordering::Relaxed);
        counters.total_held_us.fetch_add(held_us, Ordering::Relaxed);
        counters.max_held_us.fetch_max(held_us, Ordering::Relaxed);
        if over_budget {
            counters.over_budget_holds.fetch_add(1, Ordering::Relaxed);
        }
        if starved {
            counters.starved_holds.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> Vec<RowGuardSiteCensus> {
        RowGuardSite::ALL
            .iter()
            .map(|&site| {
                let counters = &self.sites[site.index()];
                RowGuardSiteCensus {
                    site,
                    holds: counters.holds.load(Ordering::Relaxed),
                    total_held_us: counters.total_held_us.load(Ordering::Relaxed),
                    max_held_us: counters.max_held_us.load(Ordering::Relaxed),
                    over_budget_holds: counters.over_budget_holds.load(Ordering::Relaxed),
                    starved_holds: counters.starved_holds.load(Ordering::Relaxed),
                }
            })
            .collect()
    }
}

/// A row-table read guard that reports its own hold duration and call site.
///
/// #1950 ask 1: the commit path can already attribute its wait, but a wait
/// names the waiter, never the holder. This closes that gap from the other
/// side — every long read guard says which call site held it and for how
/// long, so the holder is *named* rather than inferred from the set of
/// plausible scan sites.
///
/// The timer starts when the guard is acquired, not when it is requested, so
/// a reader that itself waited behind a writer is not charged for that wait.
/// Charging it would reproduce exactly the defect that made `mvcc_locked_us`
/// name every stall's victim as its cause.
struct TimedRowRead<'a> {
    guard: std::sync::RwLockReadGuard<'a, RowTable>,
    site: RowGuardSite,
    /// Where every hold is tallied, not just the over-budget ones.
    census: &'a RowGuardCensus,
    acquired: Instant,
    /// Calling thread's kernel+user CPU time when the guard was taken.
    ///
    /// `held_us` alone cannot tell a slow scan from a descheduled thread, and
    /// on this host that is not a corner case: a `read_latest` — a point read
    /// of ONE key — was measured holding the guard for 743 ms while the machine
    /// was saturated by a compile (#1955). Every fix #1950 ask 2 considers
    /// (page the scan, snapshot it, shard the table) addresses *work*, so an
    /// instrument that cannot separate work from starvation can point at the
    /// wrong one with full confidence.
    ///
    /// `None` when the platform cannot report it; the event then omits the
    /// CPU fields rather than reporting a fabricated zero.
    acquired_cpu_us: Option<u64>,
}

/// Kernel+user CPU time consumed by the calling thread so far, in microseconds.
///
/// **Quantised to the scheduler tick.** `GetThreadTimes` accrues in 15.625 ms
/// units on this host — measured, not assumed: a 39,278 us guard hold reported
/// exactly 31,250 us of CPU and a 69,767 us hold reported exactly 62,500 us,
/// which are 2 and 4 ticks. So `cpu_us` is only meaningful for holds well above
/// ~50 ms, and cannot resolve anything near the 25 ms budget.
///
/// That is acceptable for what it is for. The holds this exists to explain are
/// the 743 ms `read_latest` and the 1.4 s `scan_cf_latest` of #1955/#1950,
/// where a tick is 2% of the measurement. A finer answer would need
/// `QueryThreadCycleTime`, which returns cycles rather than time and would need
/// a frequency calibration to interpret on a hybrid P/E-core part — more
/// machinery than the question currently justifies.
#[cfg(windows)]
fn thread_cpu_us() -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: all four out-pointers address live, aligned `FILETIME`s owned by
    // this frame for the whole call, and `GetCurrentThread` returns a
    // pseudo-handle that is valid for the current thread and must not be
    // closed. The call only writes through those pointers.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // FILETIME counts 100-nanosecond intervals.
    let micros = |time: FILETIME| {
        ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) / 10
    };
    Some(micros(kernel) + micros(user))
}

#[cfg(not(windows))]
const fn thread_cpu_us() -> Option<u64> {
    None
}

impl std::ops::Deref for TimedRowRead<'_> {
    type Target = RowTable;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl Drop for TimedRowRead<'_> {
    fn drop(&mut self) {
        record_row_guard_hold(self.census, self.site, self.acquired, self.acquired_cpu_us);
    }
}

/// A read guard over **every** shard, for the few call sites whose question
/// genuinely spans column families (a mixed `read_batch`, the snapshot-GC debt
/// sweep).
///
/// Shards are locked in index order, which is the same order every multi-shard
/// writer uses, so these cannot deadlock against a commit.
///
/// One census hold is recorded for the whole set rather than one per shard: the
/// thing #1952's census answers is "did this call site run, and for how long",
/// and a site that takes 52 locks at once ran once.
struct TimedRowReadAll<'a> {
    guards: Vec<std::sync::RwLockReadGuard<'a, RowTable>>,
    site: RowGuardSite,
    census: &'a RowGuardCensus,
    acquired: Instant,
    acquired_cpu_us: Option<u64>,
}

impl TimedRowReadAll<'_> {
    /// The rows of one column family, from whichever shard owns it.
    fn cf(&self, cf: ColumnFamily) -> Option<&BTreeMap<Vec<u8>, VersionChain>> {
        self.guards
            .get(row_shard_index(cf))
            .and_then(|g| g.get(&cf))
    }

    /// Every column family in the table, across all shards.
    fn iter(&self) -> impl Iterator<Item = (&ColumnFamily, &BTreeMap<Vec<u8>, VersionChain>)> {
        self.guards.iter().flat_map(|guard| guard.iter())
    }
}

impl Drop for TimedRowReadAll<'_> {
    fn drop(&mut self) {
        record_row_guard_hold(self.census, self.site, self.acquired, self.acquired_cpu_us);
    }
}

/// A write guard over a chosen set of shards, held in shard-index order.
///
/// **The ordering is the deadlock argument.** `ColumnFamily` is `Ord` and
/// `row_shard_index` is a pure function of it, so every writer that needs
/// several shards acquires them in the same total order; two commits with
/// overlapping shard sets therefore cannot each hold what the other wants.
struct RowWriteSet<'a> {
    /// `(shard_index, guard)`, ascending by shard index.
    guards: Vec<(usize, std::sync::RwLockWriteGuard<'a, RowTable>)>,
}

impl RowWriteSet<'_> {
    fn slot(&self, shard: usize) -> Option<usize> {
        self.guards
            .binary_search_by_key(&shard, |(index, _)| *index)
            .ok()
    }

    /// The rows of one column family, or `None` when this writer did not lock
    /// the shard that owns it.
    ///
    /// `None` is deliberately not the same as "the family is empty". A caller
    /// that reads a family it did not lock is a bug in the lock set, and every
    /// such read here fails closed rather than reporting an absence.
    fn cf(&self, cf: ColumnFamily) -> Option<&BTreeMap<Vec<u8>, VersionChain>> {
        let slot = self.slot(row_shard_index(cf))?;
        self.guards[slot].1.get(&cf)
    }

    /// Whether this writer holds the shard owning `cf`.
    fn holds(&self, cf: ColumnFamily) -> bool {
        self.slot(row_shard_index(cf)).is_some()
    }

    /// The mutable rows of one column family, created if absent.
    ///
    /// # Errors
    ///
    /// Fails closed when the shard owning `cf` is not in this writer's lock
    /// set, because writing through a lock that was never taken is a data race
    /// this type exists to prevent.
    fn entry_mut(&mut self, cf: ColumnFamily) -> Result<&mut BTreeMap<Vec<u8>, VersionChain>> {
        let shard = row_shard_index(cf);
        let slot = self.slot(shard).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "MVCC commit tried to write {} but did not lock row-table shard {shard}; the batch's lock set was computed without this column family (#1950)",
                cf.name()
            ))
        })?;
        Ok(self.guards[slot].1.entry(cf).or_default())
    }

    /// Every column family this writer holds.
    fn iter(&self) -> impl Iterator<Item = (&ColumnFamily, &BTreeMap<Vec<u8>, VersionChain>)> {
        self.guards.iter().flat_map(|(_, guard)| guard.iter())
    }

    /// Every column family this writer holds, mutably.
    fn iter_mut(
        &mut self,
    ) -> impl Iterator<Item = (&ColumnFamily, &mut BTreeMap<Vec<u8>, VersionChain>)> {
        self.guards
            .iter_mut()
            .flat_map(|(_, guard)| guard.iter_mut())
    }
}

/// The row-table shards one atomic commit must hold.
///
/// Every column family the batch writes, plus `Base` when the batch can reach
/// `visible_base_panel` — that is, when it carries a `Base` row or a quantized
/// `Slot` row. `search_panels_affected_by_batch` reads the table for nothing
/// else, so a batch of `Kv`/`Ledger`/`TimeIndex` rows takes no `Base` lock at
/// all, and no longer waits behind a `Base` scan (#1950).
///
/// Including `Base` unconditionally would be simpler and would silently undo
/// the fix: the production stall this issue measured was exactly a
/// `kv`/`raw_commitment`/`time_index` batch waiting on a `Base` reader.
fn commit_lock_set(rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)]) -> Vec<ColumnFamily> {
    let mut cfs: Vec<ColumnFamily> = rows.iter().map(|(cf, _, _)| *cf).collect();
    if cfs.iter().any(|cf| {
        matches!(
            cf,
            ColumnFamily::Base
                | ColumnFamily::Slot {
                    kind: crate::cf::SlotFamilyKind::Quantized,
                    ..
                }
        )
    }) {
        cfs.push(ColumnFamily::Base);
    }
    cfs.sort_unstable();
    cfs.dedup();
    cfs
}

/// The shared tail of every row-guard `Drop`: tally the hold, and report it if
/// it ran past the budget.
fn record_row_guard_hold(
    census: &RowGuardCensus,
    site: RowGuardSite,
    acquired: Instant,
    acquired_cpu_us: Option<u64>,
) {
    let held_us = elapsed_us(&acquired);
    if held_us < ROW_READ_GUARD_WARN_US {
        census.record(site, held_us, false, false);
        return;
    }
    let cpu_us = acquired_cpu_us
        .zip(thread_cpu_us())
        .map(|(before, after)| after.saturating_sub(before));
    let starved = held_us >= 4 * WINDOWS_SCHEDULER_TICK_US
        && cpu_us.is_some_and(|cpu| cpu.saturating_mul(2) < held_us);
    census.record(site, held_us, true, starved);
    tracing::warn!(
        code = "CALYX_ASTER_ROW_READ_GUARD_SLOW",
        site = site.as_str(),
        held_us,
        cpu_us,
        starved,
        budget_us = ROW_READ_GUARD_WARN_US,
        "row-table read guard held long enough to stall every commit waiting for the write lock"
    );
}

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
    /// The row table, split into [`ROW_SHARDS`] independently-locked shards
    /// routed by [`row_shard_index`] (#1950).
    ///
    /// Fixed length, allocated once, never resized.
    rows: Vec<RwLock<RowTable>>,
    /// The CF router, or `None` for a store with no physical projection.
    ///
    /// Behind **no lock**. It was `RwLock<Option<CfRouter>>`, and that lock was
    /// never guarding the `Option` — nothing replaces it after construction —
    /// it existed only because every `CfRouter` method took `&mut self`. The
    /// router shards its own state per column family now, so the outer lock was
    /// pure contention: it serialised a `Kv` commit against a `Base` scan that
    /// shared nothing with it (#1950).
    router: Option<CfRouter>,
    router_latest_readback: AtomicBool,
    /// Earliest sequence after which the in-memory MVCC version chains are a
    /// complete changed-key journal. Latest-only recovery serves older
    /// checkpoint rows from the router without their original per-row
    /// sequence, so a delta query below this floor must fail closed and rebase
    /// instead of silently omitting checkpointed changes (#1842).
    changed_key_history_floor: Seq,
    router_eager_lookup_on_refresh: AtomicBool,
    read_barriers: RwLock<Vec<ReadBarrier>>,
    leases: LeaseRegistry,
    resource_counters: Arc<ResourceCounters>,
    snapshot_gc: SnapshotGcReclaimer,
    snapshot_gc_counters: SnapshotGcCounters,
    /// Per-site tallies for every row-table read guard taken on this vault.
    row_guard_census: RowGuardCensus,
}

impl VersionedCfStore {
    /// Row-table read guard that reports itself if held past
    /// [`ROW_READ_GUARD_WARN_US`] (#1950). Panicking variant, matching the
    /// `.expect("mvcc row table poisoned")` these call sites already used.
    ///
    /// Every row-table read goes through this or [`Self::try_read_rows`]; a
    /// bare `self.rows.read()` is a hold nothing can attribute, which is the
    /// state #1950 is stuck in.
    fn read_rows(&self, site: RowGuardSite, cf: ColumnFamily) -> TimedRowRead<'_> {
        let guard = self.rows[row_shard_index(cf)]
            .read()
            .expect("mvcc row table poisoned");
        TimedRowRead {
            guard,
            site,
            census: &self.row_guard_census,
            acquired: Instant::now(),
            acquired_cpu_us: thread_cpu_us(),
        }
    }

    /// A read guard over every shard, for the call sites whose question spans
    /// column families. Shards are taken in index order.
    fn read_rows_all(&self, site: RowGuardSite) -> TimedRowReadAll<'_> {
        let guards = self
            .rows
            .iter()
            .map(|shard| shard.read().expect("mvcc row table poisoned"))
            .collect();
        TimedRowReadAll {
            guards,
            site,
            census: &self.row_guard_census,
            acquired: Instant::now(),
            acquired_cpu_us: thread_cpu_us(),
        }
    }

    /// Write guards for exactly the shards owning `cfs`, in shard-index order.
    ///
    /// Taking only what the batch touches is the whole of #1950: a commit to
    /// `Kv` no longer waits behind a scan of `Base`.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when any shard's lock is poisoned.
    fn write_rows_for(
        &self,
        cfs: impl IntoIterator<Item = ColumnFamily>,
        poisoned: &str,
    ) -> Result<RowWriteSet<'_>> {
        let mut shards: Vec<usize> = cfs.into_iter().map(row_shard_index).collect();
        shards.sort_unstable();
        shards.dedup();
        let mut guards = Vec::with_capacity(shards.len());
        for shard in shards {
            let guard = self.rows[shard]
                .write()
                .map_err(|_| CalyxError::aster_corrupt_shard(poisoned.to_owned()))?;
            guards.push((shard, guard));
        }
        Ok(RowWriteSet { guards })
    }

    /// Write guards for every shard, in index order. Used by the recovery and
    /// GC paths, which are whole-table by nature.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when any shard's lock is poisoned.
    fn write_rows_all(&self, poisoned: &str) -> Result<RowWriteSet<'_>> {
        let mut guards = Vec::with_capacity(self.rows.len());
        for (shard, lock) in self.rows.iter().enumerate() {
            let guard = lock
                .write()
                .map_err(|_| CalyxError::aster_corrupt_shard(poisoned.to_owned()))?;
            guards.push((shard, guard));
        }
        Ok(RowWriteSet { guards })
    }

    /// [`Self::read_rows`] for call sites that surface poisoning as an error
    /// rather than panicking.
    fn try_read_rows(
        &self,
        site: RowGuardSite,
        cf: ColumnFamily,
        poisoned: &str,
    ) -> Result<TimedRowRead<'_>> {
        let guard = self.rows[row_shard_index(cf)]
            .read()
            .map_err(|_| CalyxError::aster_corrupt_shard(poisoned.to_owned()))?;
        Ok(TimedRowRead {
            guard,
            site,
            census: &self.row_guard_census,
            acquired: Instant::now(),
            acquired_cpu_us: thread_cpu_us(),
        })
    }

    /// Per-site row-guard counters, read back at this instant.
    ///
    /// Answers "did this read path run, and how often" directly, instead of
    /// inferring it from the presence or absence of an over-budget log event.
    /// Every site in [`RowGuardSite::ALL`] is present in the result even when it
    /// has never been taken, so a zero is an observation rather than a gap
    /// (#1952 ask 3).
    pub fn row_guard_census(&self) -> Vec<RowGuardSiteCensus> {
        self.row_guard_census.snapshot()
    }

    pub fn new(start_seq: Seq) -> Self {
        Self {
            seqs: SeqAllocator::new(start_seq),
            derived_content_seq: AtomicU64::new(0),
            panel_content_seqs: RwLock::new(BTreeMap::new()),
            next_lease_id: AtomicU64::new(0),
            rows: new_row_shards(),
            router: None,
            router_latest_readback: AtomicBool::new(false),
            changed_key_history_floor: 0,
            router_eager_lookup_on_refresh: AtomicBool::new(true),
            read_barriers: RwLock::new(Vec::new()),
            leases: LeaseRegistry::default(),
            resource_counters: Arc::new(ResourceCounters::default()),
            snapshot_gc: SnapshotGcReclaimer::default(),
            snapshot_gc_counters: SnapshotGcCounters::default(),
            row_guard_census: RowGuardCensus::default(),
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
            rows: new_row_shards(),
            router: Some(router),
            router_latest_readback: AtomicBool::new(router_latest_readback),
            changed_key_history_floor: if router_latest_readback { start_seq } else { 0 },
            router_eager_lookup_on_refresh: AtomicBool::new(eager_lookup_on_refresh),
            read_barriers: RwLock::new(Vec::new()),
            leases: LeaseRegistry::default(),
            resource_counters,
            snapshot_gc: SnapshotGcReclaimer::default(),
            snapshot_gc_counters: SnapshotGcCounters::default(),
            row_guard_census: RowGuardCensus::default(),
        }
    }

    pub fn new_with_router_latest_readback(start_seq: Seq, router: CfRouter) -> Self {
        Self::new_with_router_and_policy(start_seq, router, true, false)
    }

    /// Reports whether this store serves latest reads from the CF router
    /// instead of the in-memory MVCC row table.
    ///
    /// The mode is chosen once at open from `restore_mvcc_rows` and then
    /// silently changes which code path several read entry points take
    /// (`read_latest`, `scan_cf_range_page_at`, `predecessor_cf_at`). A harness
    /// that means to exercise the router-backed branch previously had no way to
    /// confirm it did, so a run that quietly took the row-table branch instead
    /// looked exactly like a passing test of the branch it never reached
    /// (#1954). Making the mode readable is what lets such a harness fail
    /// closed rather than report a vacuous pass.
    pub fn router_latest_readback(&self) -> bool {
        self.router_latest_readback.load(Ordering::Acquire)
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
    /// 1. Readers hold the router **shard** read lock across their SST reads,
    ///    so taking that shard for write drains every in-flight mapping. The
    ///    router is sharded per column family now (#1950), and this argument
    ///    survives it only because the refresh below takes the write guard for
    ///    exactly the CFs being reclaimed, and because every read path holds
    ///    its shard guard across the reads rather than snapshotting the level
    ///    and releasing early.
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
            let lock_wait_ms = lock_wait_started.elapsed().as_millis();
            let held_started = Instant::now();
            let Some(router) = self.router.as_ref() else {
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
        let Some(router) = self.router.as_ref() else {
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
        let Some(router) = self.router.as_ref() else {
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
        let Some(router) = self.router.as_ref() else {
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
    ///
    /// **Why the `Base` shard specifically (#1950).** The row table is sharded
    /// per column family, so a read guard no longer excludes every commit — it
    /// excludes commits that touch the shard it holds. That is exactly the set
    /// that matters here: `search_panels_affected_by_batch` derives
    /// `affected_panels` only from `Base` rows and quantized `Slot` rows, and
    /// `advance_affected_panel_content_seqs` is a no-op on an empty set, so a
    /// commit can only move a panel watermark if its lock set includes the
    /// `Base` shard (`commit_lock_set` guarantees it does).
    ///
    /// A commit that touches neither — a `Kv`/`Ledger` batch — may now allocate
    /// a sequence concurrently with this call. That is safe in the direction
    /// that matters: it can only make the observed `seq` *older* than the true
    /// latest, never make `panel_content_seq` exceed `seq`, which is the
    /// invariant a stale index would need in order to pass.
    pub fn pin_snapshot_for_panel(
        &self,
        panel_version: u32,
        freshness: Freshness,
        clock: &dyn Clock,
        max_age_ms: u64,
    ) -> Result<Snapshot> {
        let _table = self.try_read_rows(
            RowGuardSite::PinSnapshotForPanel,
            ColumnFamily::Base,
            "MVCC row-table lock was poisoned while pinning panel search freshness",
        )?;
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
    ///
    /// Holds the `Base` shard for the same reason [`Self::pin_snapshot_for_panel`]
    /// does: only a `Base`/`Slot`-bearing commit can move these watermarks, and
    /// only such a commit locks that shard (#1950).
    pub(crate) fn panel_content_seqs_snapshot(&self) -> Result<BTreeMap<u32, Seq>> {
        let _table = self.try_read_rows(
            RowGuardSite::PanelContentSeqsSnapshot,
            ColumnFamily::Base,
            "MVCC row-table lock was poisoned while snapshotting panel watermarks",
        )?;
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
        // Reads restored `Base` history and excludes concurrent panel-moving
        // commits; both are the `Base` shard (#1950).
        let table = self.try_read_rows(
            RowGuardSite::MigratePanelContentSeqsToAtLeast,
            ColumnFamily::Base,
            "MVCC row-table lock was poisoned while migrating panel watermarks",
        )?;
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
        let Some(router) = self.router.as_ref() else {
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
        self.commit_batch_timed(rows, &mut MvccCommitTimings::default())
    }

    /// [`Self::commit_batch`] with its internal fixed points attributed.
    ///
    /// The caller's stage split reported this whole method as one `mvcc`
    /// number, which measured 91-99% of commit cost without being able to say
    /// why (#1948). The candidate causes have materially different fixes —
    /// waiting for a lock held by a concurrent compaction is not the same
    /// defect as synchronously writing a 6.8 MB SST, and neither is the same as
    /// copying every key and value — so each is timed separately and anything
    /// left over is reported as an explicit remainder rather than vanishing.
    pub fn commit_batch_timed<I, K, V>(
        &self,
        rows: I,
        timings: &mut MvccCommitTimings,
    ) -> Result<Seq>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let started = Instant::now();
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(cf, key, value)| (cf, key.into(), value.into()))
            .collect();
        timings.materialize_us = elapsed_us(&started);
        if rows.is_empty() {
            timings.total_us = elapsed_us(&started);
            return Ok(self.current_seq());
        }
        timings.rows = u32::try_from(rows.len()).unwrap_or(u32::MAX);

        let row_lock_started = Instant::now();
        let mut table = self.write_rows_for(
            commit_lock_set(&rows),
            "MVCC row-table lock was poisoned during atomic commit",
        )?;
        timings.row_lock_wait_us = elapsed_us(&row_lock_started);
        // No router lock is taken here any more. The router is sharded per
        // column family and every one of its methods takes `&self`, so the
        // commit reaches a shard only inside `put_at` below, and only the shard
        // of the family it is writing (#1950).
        //
        // Atomicity is unchanged, and the row lock is what carries it: this
        // commit holds the row-table write shard for **every** family it will
        // project into the router (`commit_lock_set`, enforced by `entry_mut`),
        // and every reader takes the row shard before the router shard. So no
        // reader can observe the row applied without the router projection.
        let locked_cpu_started = thread_cpu_us();
        let attribution_started = Instant::now();
        let latest_router = if self.router_latest_readback.load(Ordering::Acquire) {
            self.router.as_ref()
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
        timings.panel_attribution_us = elapsed_us(&attribution_started);
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
        timings.watermark_us = elapsed_us(&attribution_started) - timings.panel_attribution_us;
        let row_apply_started = Instant::now();
        for (cf, key, value) in &rows {
            table
                .entry_mut(*cf)?
                .entry(key.clone())
                .or_default()
                .push(VersionedValue {
                    seq,
                    value: value.clone(),
                });
        }
        timings.row_apply_us = elapsed_us(&row_apply_started);

        let router_apply_started = Instant::now();
        let mut sealed: Vec<crate::cf::SealedFlush> = Vec::new();
        if let Some(router) = self.router.as_ref() {
            // Publish the authoritative version chains and their sequence
            // before attempting the fallible router projection. The row-table
            // write shards are still held for every family below, so readers
            // still observe one atomic latest transition. If a put or flush
            // fails part-way through the router batch, every logical row
            // already exists at `seq` and masks any partial router state as
            // soon as the guards are released.
            for (cf, key, value) in &rows {
                match router.put_at(*cf, key, value, seq, &mut sealed) {
                    Ok(outcome) => timings.put.absorb(outcome),
                    Err(error) => {
                        tracing::error!(
                            code = "CALYX_MVCC_ROUTER_PUBLICATION_RECONCILIATION_REQUIRED",
                            committed_seq = seq,
                            cf = cf.name(),
                            key_len = key.len(),
                            value_len = value.len(),
                            router_error_code = error.code,
                            router_error = %error,
                            sealed_to_abandon = sealed.len(),
                            "MVCC rows committed atomically but the router projection failed; the committed sequence must be reconciled before retry"
                        );
                        // Anything this batch already sealed is still sitting in
                        // the router's pending queue holding the only in-memory
                        // copy of its rows. Leaving it there would wedge the CF:
                        // the queue head never becomes installable, so no later
                        // seal for that CF could ever publish its SST, until the
                        // backlog cap failed the CF outright. Return the rows to
                        // their active memtables before the error escapes.
                        for handle in &sealed {
                            if let Err(abandon_error) = router.abandon_sealed(handle, &error) {
                                tracing::error!(
                                    code = abandon_error.code,
                                    committed_seq = seq,
                                    cf = handle.cf().name(),
                                    rows = handle.row_count(),
                                    error = %abandon_error,
                                    "sealed memtable could not be returned to its active memtable after a failed router projection"
                                );
                            }
                        }
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
        }
        // The per-row shard waits are inside the loop just measured, so they
        // are lifted out of `router_apply` rather than added beside it: the
        // stage values must still partition `total_us` exactly, and a commit
        // that waited must not be reported as a commit that worked (#1950).
        timings.router_lock_wait_us = timings.put.lock_wait_us;
        timings.router_apply_us =
            elapsed_us(&router_apply_started).saturating_sub(timings.router_lock_wait_us);

        // ---- switch-then-flush: everything above ran under both write locks,
        // everything below must not (#1949). ------------------------------
        //
        // Dropping the guards here is the change. The rows are already in the
        // authoritative row table AND in the sealed memtables, which stay in
        // the router's read path until their SSTs install, so the view a reader
        // sees is complete at every instant across this boundary. What is no
        // longer true is that a multi-hundred-millisecond AEAD-and-write holds
        // the whole vault shut.
        timings.locked_cpu_us = locked_cpu_started
            .zip(thread_cpu_us())
            .map(|(before, after)| after.saturating_sub(before));
        drop(table);
        if !sealed.is_empty() {
            let sst_write_started = Instant::now();
            // NO LOCK IS HELD HERE. This is the AEAD seal plus the file write
            // that measured 652 ms in one commit (#1948). Writing before
            // touching the router again is the entire fix; `install_sealed`
            // below takes only the one shard it publishes into.
            let mut written = Vec::with_capacity(sealed.len());
            let mut write_failure = None;
            for handle in &sealed {
                match handle.write_sst() {
                    Ok(entry) => written.push(entry),
                    Err(error) => {
                        write_failure = Some(error);
                        break;
                    }
                }
            }
            timings.sst_write_us = elapsed_us(&sst_write_started);

            let router = self.router.as_ref().ok_or_else(|| {
                CalyxError::aster_corrupt_shard("sealed-SST install requires a live CF router")
            })?;
            // Each install takes only the shard of the family it publishes
            // into, and only for the metadata swap: the bytes are already on
            // disk.
            let mut installed = 0_usize;
            let mut outcome = Ok(());
            for (handle, entry) in sealed.iter().zip(written) {
                match router.install_sealed(handle, entry) {
                    Ok(()) => installed += 1,
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
            if outcome.is_ok()
                && let Some(error) = write_failure
            {
                outcome = Err(error);
            }
            if let Err(error) = outcome.as_ref() {
                // Every seal that did not install — whether its SST write
                // failed or its install did — still holds the only in-memory
                // copy of its rows and still occupies the pending queue. Each
                // must go back into its active memtable before the error
                // escapes, or the CF is wedged for the life of the process.
                for handle in sealed.iter().skip(installed) {
                    if let Err(abandon_error) = router.abandon_sealed(handle, error) {
                        tracing::error!(
                            code = abandon_error.code,
                            committed_seq = seq,
                            cf = handle.cf().name(),
                            rows = handle.row_count(),
                            error = %abandon_error,
                            "sealed memtable could not be returned to its active memtable after a failed SST write or install"
                        );
                    }
                }
            }
            outcome?;
            timings.total_us = elapsed_us(&started);
            return Ok(seq);
        }
        timings.total_us = elapsed_us(&started);
        Ok(seq)
    }

    /// Restores one durable write group at its original sequence before live writes begin.
    pub fn restore_batch<I, K, V>(&self, seq: Seq, rows: I) -> Result<()>
    where
        I: IntoIterator<Item = (ColumnFamily, K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        self.restore_batch_with_attribution(seq, rows, PanelAttribution::ReplayedCommit)
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
        // Recovery is whole-table by nature and runs single-threaded at open.
        let mut table =
            self.write_rows_all("MVCC row-table lock was poisoned during atomic recovery restore")?;
        let latest_router = if matches!(
            attribution,
            PanelAttribution::Strict | PanelAttribution::ReplayedCommit
        ) && self.router_latest_readback.load(Ordering::Acquire)
        {
            self.router.as_ref()
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
                .entry_mut(cf)?
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
        // Recovery is whole-table by nature and runs single-threaded at open.
        let mut table =
            self.write_rows_all("MVCC row-table lock was poisoned during atomic recovery restore")?;
        let router = self.router.as_ref();
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
                    router
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
                    .entry_mut(cf)?
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
        self.rows[row_shard_index(cf)]
            .read()
            .expect("mvcc row table poisoned")
            .get(&cf)
            .is_some_and(|rows| rows.contains_key(key))
    }

    pub fn flush_all_cfs(&self) -> Result<Vec<SstSummary>> {
        let Some(router) = self.router.as_ref() else {
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

    /// Freezes the selected router memtables at the current commit watermark.
    ///
    /// The caller must serialize this with durable commit publication. The
    /// returned watermark is the minimum manifest sequence that a subsequent
    /// physical compaction must prove before reclaiming the flushed prefix.
    pub(crate) fn flush_cfs_at_current_seq(
        &self,
        cfs: &[ColumnFamily],
    ) -> Result<(Seq, Vec<SstSummary>)> {
        let Some(router) = self.router.as_ref() else {
            return Ok((self.current_seq(), Vec::new()));
        };
        // Keep this read under the router lock for the same reason as
        // `flush_all_cfs`: a commit cannot publish its sequence until every
        // row has entered this router.
        let commit_watermark = self.current_seq();
        let summaries = router.flush_pending_cfs_at(cfs, commit_watermark)?;
        Ok((commit_watermark, summaries))
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
    /// Live mutation: every quantized slot must resolve to Base **and** carry
    /// its own Base row in the same batch (#1935).
    Strict,
    /// WAL-tail replay of an already-committed batch: same resolution as
    /// [`Self::Strict`], but a missing same-batch `Base` row is reported rather
    /// than refused.
    ///
    /// A commit-time gate cannot retroactively reject history. Refusing here
    /// would make a vault whose WAL tail predates the gate impossible to open,
    /// which converts a derived-index defect into total data unavailability.
    /// The condition is logged at `error` with the exact CF and key length, so a
    /// pre-existing violation is loud rather than silent.
    ReplayedCommit,
    /// Legacy checkpoint rows: derive a conservative migration baseline, but
    /// tolerate an already-orphaned slot awaiting physical GC.
    HistoricalMigration,
    /// Model-3 checkpoint rows: the manifest map is authoritative, and SST
    /// coalescing has erased per-row commit sequences, so do not re-attribute.
    ManifestBaseline,
}

fn search_panels_affected_by_batch(
    table: &RowWriteSet<'_>,
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
        // A live quantized slot write must carry its own `Base` row in THIS
        // batch — a merely *visible* one is not enough (issue #1935).
        //
        // Two invariants rest on this and neither survives a slot-only write:
        //
        // * the `Base` slot hash **is** the vector's integrity record (#1888),
        //   so a slot row written without restating it leaves every later
        //   verifier checking the new vector against the old hash;
        // * `calyx_search::measure_panel_delta` scopes a generation's
        //   reconciliation set to the panel's changed `Base` keys, which is
        //   complete only if every slot write moves its `Base` row with it.
        //
        // The read side cannot police this: bounded native-CF compaction
        // re-stamps a whole column family's rows into the changed-key history
        // (the SST row format has no per-row commit sequence — see
        // `PanelAttribution::ManifestBaseline` below), so "this slot key
        // changed and its `Base` row did not" is the *expected* state after a
        // compaction and says nothing about any writer. Measured on the
        // production vault, that made 100.00% of `slot_35`'s 68,691 rows look
        // like contract violations and raised CALYX_ASTER_CORRUPT_SHARD —
        // whose catalog remediation is `restore from restic/snapshot` — against
        // a healthy vault. So the invariant is enforced here, at the only place
        // that can tell a write from a rewrite: the write itself.
        //
        // Tombstones are exempt below: a slot deletion carries no vector and no
        // integrity record to restate.
        if !is_tombstone_value(value) && !staged_base_panels.contains_key(key) {
            match attribution {
                PanelAttribution::Strict => {
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "live quantized slot write has no same-batch Base row (cf={}, key_len={}); the Base slot hash is this vector's integrity record and the panel-scoped search delta is scoped to changed Base keys, so a slot row must be staged atomically with its own Base row",
                        cf.name(),
                        key.len()
                    )));
                }
                PanelAttribution::ReplayedCommit => {
                    tracing::error!(
                        code = "CALYX_MVCC_REPLAYED_SLOT_WRITE_WITHOUT_SAME_BATCH_BASE",
                        cf = cf.name(),
                        key_len = key.len(),
                        visible_seq,
                        "replayed an already-committed batch whose live quantized slot write \
                         carries no same-batch Base row; the row's Base slot hash no longer \
                         describes its vector and this generation's reconciliation delta cannot \
                         be proven complete from Base alone. Rebuild the affected panel search \
                         generation and repair the writer that produced this batch"
                    );
                }
                PanelAttribution::HistoricalMigration | PanelAttribution::ManifestBaseline => {}
            }
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
    table: &RowWriteSet<'_>,
    latest_router: Option<&CfRouter>,
    key: &[u8],
    seq: Seq,
) -> Result<Option<u32>> {
    // Fails closed rather than reading an absence: `commit_lock_set` puts the
    // `Base` shard in every lock set that can reach here, so a miss means the
    // lock set was computed wrong, not that the row is gone (#1950).
    if !table.holds(ColumnFamily::Base) {
        return Err(CalyxError::aster_corrupt_shard(
            "panel attribution needs visible Base rows but the commit did not lock the Base row-table shard; the batch's lock set is wrong (#1950)",
        ));
    }
    let version = table
        .cf(ColumnFamily::Base)
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

/// One panel's own share of a `Base` changed-key delta, with the composition
/// that produced it (#1901).
///
/// `scanned` is every key that changed in the range across all panels;
/// `panel + other_panels + unattributed == scanned`. `keys` carries the
/// panel's keys plus the unattributable ones, which is exactly the set a
/// panel-scoped reconciliation must mask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelScopedChangedKeys {
    /// Panel version the delta was scoped to.
    pub panel_version: u32,
    /// Changed `Base` keys in the range, before scoping.
    pub scanned: usize,
    /// Keys attributed to `panel_version`.
    pub panel: usize,
    /// Keys attributed to some other panel and excluded.
    pub other_panels: usize,
    /// Keys whose visible history is entirely tombstoned, so no panel can be
    /// proven; included in `keys` conservatively.
    pub unattributed: usize,
    /// `panel` + `unattributed` keys, in key order.
    pub keys: Vec<Vec<u8>>,
}

impl PanelScopedChangedKeys {
    /// Keys this delta must reconcile.
    #[must_use]
    pub fn reconcile_len(&self) -> usize {
        self.keys.len()
    }

    /// One-line composition, for an error or trace that must say where the
    /// count came from rather than only how large it is.
    #[must_use]
    pub fn composition(&self) -> String {
        format!(
            "panel_version={} scanned_base_keys={} panel_keys={} other_panel_keys={} unattributed_keys={}",
            self.panel_version, self.scanned, self.panel, self.other_panels, self.unattributed
        )
    }
}

/// Panels a single `Base` key's visible version chain can be attributed to.
enum ChainPanels {
    /// Every panel version the chain's live versions declared. Never empty.
    Panels(BTreeSet<u32>),
    /// Every visible version is a tombstone, so the row's panel is unprovable
    /// from the overlay alone.
    Unattributable,
}

/// Attributes one `Base` key to every panel its visible chain ever declared.
///
/// Walks the whole chain rather than only the newest visible version: a row
/// deleted after the generation was built, or moved between panels, still
/// belongs to the delta of the panel that indexed it.
fn visible_base_panels_in_chain(table: &RowTable, key: &[u8], seq: Seq) -> Result<ChainPanels> {
    let mut panels = BTreeSet::new();
    let versions = table
        .get(&ColumnFamily::Base)
        .and_then(|base| base.get(key))
        .map(Vec::as_slice)
        .unwrap_or_default();
    for version in versions.iter().filter(|version| version.seq <= seq) {
        if is_tombstone_value(&version.value) {
            continue;
        }
        panels.insert(panel_from_base_value(key, &version.value)?);
    }
    if panels.is_empty() {
        return Ok(ChainPanels::Unattributable);
    }
    Ok(ChainPanels::Panels(panels))
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
