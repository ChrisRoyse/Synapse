//! In-memory MVCC row table used to define the cross-CF snapshot contract.

mod flusher;
mod gc;
mod read;
mod scan_pages;
use crate::cf::{CfRouter, ColumnFamily, KeyRange, RetiredCfPhysical, RouterPutCost};
use crate::gc::{SnapshotGcCounters, SnapshotGcReclaimer, SnapshotGcTick};
use crate::mvcc::{
    Freshness, ReadBarrier, ReaderLease, SeqAllocator, Snapshot, read_barrier::first_blocking,
};
use crate::resource::{
    LeaseRegistry, LeaseView, MemtableCfStatus, MemtableStatus, ReaderLeaseRenewal,
    ResourceCounters,
};
use crate::sst::SstSummary;
use calyx_core::{CalyxError, Clock, Result, Seq, SlotId, Ts};
pub use flusher::FlushStatus;
use flusher::RouterFlusher;
pub use gc::{
    DEFAULT_SNAPSHOT_VERSION_GC_MAX_CHAINS, DEFAULT_SNAPSHOT_VERSION_GC_MAX_PASS_US,
    DEFAULT_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US, DEFAULT_SNAPSHOT_VERSION_GC_MAX_VERSIONS,
    SnapshotVersionGcBudget, SnapshotVersionGcPass, SnapshotVersionGcStop,
};
pub(crate) use scan_pages::SnapshotCfRowStream;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

pub(crate) const TOMBSTONE_VALUE: &[u8] = b"\0CALYX_ASTER_TOMBSTONE_V1";

/// Maximum transient copy of the post-recovery MVCC delta retained by one
/// pinned router-backed scan.
///
/// The immutable corpus is streamed and never counts here. Sixty-four MiB is
/// one normal Aster SST target, large enough for a meaningful changed-key
/// journal but small enough that a scan cannot duplicate an unbounded row
/// table and push a lightweight daemon into allocator failure.
pub(crate) const SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES: usize = 64 << 20;

/// Rebase the process-local changed-key journal before a snapshot would need
/// to duplicate more than half of its hard transient-overlay budget.
///
/// This is a maintenance trigger, not a memory limit: rebasing first installs
/// the complete current router view as immutable SSTs, then discards only the
/// redundant in-memory journal when no reader lease can still observe an older
/// sequence. Keeping half the scan budget as headroom covers entry/container
/// overhead that [`MvccResidentStatus`] deliberately does not pretend to
/// measure.
pub const SNAPSHOT_DELTA_REBASE_TRIGGER_BYTES: u64 = (SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES / 2) as u64;

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
    /// Capturing the durable router's prior value for keys first changed after
    /// a disk-backed open. This is the bounded MVCC delta that preserves live
    /// snapshot semantics without copying the checkpointed corpus into heap.
    pub history_baseline_us: u64,
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
    "history_baseline",
    "watermark",
    "row_apply",
    "router_apply",
    "sst_write_unlocked",
    "unattributed",
];
/// Number of sub-stages in [`MVCC_STAGE_NAMES`].
pub const MVCC_STAGE_COUNT: usize = 10;

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
            self.history_baseline_us,
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
            .saturating_sub(self.history_baseline_us)
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

/// Row-table entries one **maintenance fold** examines per acquisition of the
/// row read guard (#2060).
///
/// This is not a result limit — every fold that uses it still visits the whole
/// family — it is the length of the critical section. The guard it bounds is
/// the one every commit must take exclusively, so a fold that holds it across a
/// million-row family stalls every writer in the process for as long as the
/// fold runs: the deployed daemon measured 757 ms on `scan_cf_at_overlay`,
/// 539 ms on `changed_base_keys_after_at_for_panel`, and 117 ms on
/// `changed_keys_after_at`, against a 25 ms budget.
///
/// 256 rather than a fresh guess: it is the page size #2041 swept against
/// [`ROW_READ_GUARD_WARN_US`] at six sizes on the real vault, which landed the
/// worst hold at 16% of budget with a budget cliff between 2,048 and 4,096.
/// Reusing the swept value keeps one measured number instead of two that drift.
pub const ROW_GUARD_FOLD_PAGE_ROWS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionedValue {
    seq: Seq,
    value: Vec<u8>,
}

/// One key's append-ordered MVCC history.
///
/// Commits append at the back while snapshot-version GC retires an old prefix.
/// `VecDeque` makes both physical operations amortized O(1); a `Vec` made every
/// prefix reclaim compact the retained tail and destroy value buffers while the
/// row-shard write guard was held (#2146).
type VersionChain = VecDeque<VersionedValue>;
/// The rows of every column family routed to **one shard** of the row table.
///
/// Before #1950 this was the whole table under one vault-wide lock. It keeps
/// the same shape so that a call site holding a single shard's guard reads
/// `table.get(&cf)` exactly as it always did.
type RowTable = BTreeMap<ColumnFamily, BTreeMap<Vec<u8>, VersionChain>>;

/// O(1) readback of the logical payload resident in the MVCC delta table.
///
/// These are the bytes the table itself owns, not allocator guesses: key
/// bytes are counted once per append-only key entry and value bytes once per
/// retained version (including tombstones and router-history baselines).
/// Container/node overhead remains visible in the independent OS process
/// counters rather than being represented as a misleading estimate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MvccResidentStatus {
    pub keys: u64,
    pub versions: u64,
    pub key_bytes: u64,
    pub value_bytes: u64,
}

/// Physical result of one checkpoint-time changed-key journal rebase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotDeltaRebaseReport {
    pub rebased: bool,
    pub active_leases: usize,
    pub previous_floor_seq: Seq,
    pub new_floor_seq: Seq,
    pub flushed_ssts: usize,
    pub before: MvccResidentStatus,
    pub after: MvccResidentStatus,
}

impl MvccResidentStatus {
    #[must_use]
    pub const fn payload_bytes(self) -> u64 {
        self.key_bytes.saturating_add(self.value_bytes)
    }
}

#[derive(Debug, Default)]
struct MvccResidentCounters {
    keys: AtomicU64,
    versions: AtomicU64,
    key_bytes: AtomicU64,
    value_bytes: AtomicU64,
}

impl MvccResidentCounters {
    fn record_insert(&self, new_key: bool, key_bytes: u64, value_bytes: u64) {
        if new_key {
            self.keys.fetch_add(1, Ordering::Relaxed);
            self.key_bytes.fetch_add(key_bytes, Ordering::Relaxed);
        }
        self.versions.fetch_add(1, Ordering::Relaxed);
        self.value_bytes.fetch_add(value_bytes, Ordering::Relaxed);
    }

    fn record_reclaim(&self, versions: u64, value_bytes: u64) -> Result<()> {
        let current_versions = self.versions.load(Ordering::Acquire);
        let current_value_bytes = self.value_bytes.load(Ordering::Acquire);
        if current_versions < versions || current_value_bytes < value_bytes {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "MVCC resident counters cannot cover reclaim: versions={current_versions}/{versions} value_bytes={current_value_bytes}/{value_bytes}"
            )));
        }
        // Snapshot-version reclaimers are serialized by `snapshot_gc_cursor`.
        // Commits may only increment these counters concurrently, so after the
        // paired validation neither subtraction can underflow and the two
        // counters cannot be left half-updated by an error.
        self.versions.fetch_sub(versions, Ordering::AcqRel);
        self.value_bytes.fetch_sub(value_bytes, Ordering::AcqRel);
        Ok(())
    }

    fn snapshot(&self) -> MvccResidentStatus {
        MvccResidentStatus {
            keys: self.keys.load(Ordering::Acquire),
            versions: self.versions.load(Ordering::Acquire),
            key_bytes: self.key_bytes.load(Ordering::Acquire),
            value_bytes: self.value_bytes.load(Ordering::Acquire),
        }
    }

    fn reset_after_rebase(&self) {
        // The caller holds every row-table shard for write, so no commit or
        // version reclaimer can change these counters between clearing the
        // tables and publishing zero here.
        self.keys.store(0, Ordering::Release);
        self.versions.store(0, Ordering::Release);
        self.key_bytes.store(0, Ordering::Release);
        self.value_bytes.store(0, Ordering::Release);
    }
}

/// Sealed memtables that may be outstanding with the background flusher before
/// a commit waits for capacity (#1951).
///
/// Deliberately a **router-wide** bound rather than the per-CF
/// `MAX_SEALED_MEMTABLES_PER_CF`: the queue has one consumer, so a single CF
/// that cannot drain would otherwise let every other CF pile work behind it
/// until memory rather than a counter became the limit. Set equal to the per-CF
/// cap so the pre-existing per-CF error stays reachable as the inner guard.
const MAX_OUTSTANDING_FLUSHES: usize = 8;

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

/// The exact per-column-family change signal (#2139).
///
/// `changed_keys_after_at` costs `O(family)` whatever the answer is, so asking
/// "did anything change in this family?" cost the same as the walk it was meant
/// to avoid. Three consumers wanted that question answered in constant time —
/// the search-generation freshness trigger, the CF-count readback memo (#2114),
/// and the cost-rollup corpus gate (#2113) — and none could have it.
///
/// This is that answer, maintained where the ordering already exists: inside the
/// commit, under the row-table write guard it already holds, before the rows it
/// is about become visible to any reader.
///
/// # What each field licenses
///
/// * `last_commit_seq` — the greatest sequence allocated by a commit (or replayed
///   by recovery) that wrote at least one row into this family. **A reader that
///   observes `last_commit_seq <= S` may conclude that no row entered or left
///   the family after sequence `S`**, because every row mutation in this store
///   goes through one of the three write sites that publish this value first.
/// * `out_of_band_epoch` — a counter of the physical CF-content changes that
///   allocate no sequence at all: retiring a whole router CF, and retiring
///   compaction/GC input SSTs. These do not move `last_commit_seq`, so a memo
///   keyed only on sequences would survive them. Comparing the epoch for
///   equality closes that hole.
///
/// Snapshot-version GC is deliberately **not** a signal: it never removes a key
/// entry and always retains the newest version at or below the reader floor
/// (`mvcc::store::gc::reclaim_chain`), so it cannot change any family's latest
/// row set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CfChangeSignal {
    /// Greatest committed sequence that wrote a row into this family, or `0`
    /// when this process has committed none.
    pub last_commit_seq: Seq,
    /// Physical content changes to this family that allocated no sequence.
    pub out_of_band_epoch: u64,
}

/// Exact latest logical row count maintained from committed key-state
/// transitions after one physical baseline measurement.
///
/// This is deliberately a count, not a retained key set. LSM table entry
/// totals cannot answer logical cardinality under overwrites and tombstones;
/// once a physical merge establishes the baseline, the centralized MVCC
/// commit boundary has both the old and new logical state needed to maintain
/// the aggregate exactly in constant memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactCfCardinality {
    /// Current live logical rows in this column family.
    pub rows: usize,
    /// Greatest committed sequence incorporated into `rows` for this family.
    pub last_commit_seq: Seq,
    /// Out-of-band content epoch incorporated into `rows`.
    pub out_of_band_epoch: u64,
}

#[derive(Clone, Copy, Debug)]
struct PreparedCfCardinalityDelta {
    cf: ColumnFamily,
    delta: i64,
    previous_last_commit_seq: Seq,
    out_of_band_epoch: u64,
}

/// One family's change signal in its atomic form.
#[derive(Debug, Default)]
struct CfChangeCell {
    last_commit_seq: AtomicU64,
    out_of_band_epoch: AtomicU64,
}

/// One cell per [`ColumnFamily::shard_index`], allocated once at construction.
///
/// Indexed by shard rather than by family because `ColumnFamily` is not densely
/// indexable — `Slot { slot, .. }` carries a `u16`, so a true per-family array
/// would be 131,072 entries. Sharing the static families' one-cell-each mapping
/// costs nothing (`shard_index` is injective over [`ColumnFamily::STATIC`]) and
/// makes slot families alias in pools of 16.
///
/// **Aliasing is safe in the only direction that matters.** Two slot families
/// sharing a cell can make a *quiet* family look changed, never the reverse: the
/// cell is a maximum over the commits of every family that maps to it, so
/// `last_commit_seq <= S` still proves that none of them — including the one
/// asked about — committed after `S`. The cost of a collision is one extra
/// physical walk, never a stale answer.
fn new_cf_change_cells() -> Vec<CfChangeCell> {
    (0..ROW_SHARDS).map(|_| CfChangeCell::default()).collect()
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

/// Every call site that takes a row-table guard.
///
/// Read guards, with one deliberate exception: the snapshot-version reclaimer's
/// per-shard **write** hold is counted here too, because that is the hold whose
/// boundedness the census exists to prove (see [`Self::SnapshotVersionReclaim`]).
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
    /// One exact MVCC delta cloned before a persistent immutable SST walk.
    /// The guard is handed directly to the router read guard so no same-CF
    /// commit can enter between the two serving views.
    SnapshotPagedOverlay,
    PredecessorCfAt,
    /// The overlay half of a whole-family `scan_cf_at`.
    ///
    /// Split out of the single `overlay_table_rows` site because that site was
    /// shared by `scan_cf_at` and `scan_cf_range_at`, and #1973's first
    /// question — *which caller is holding the guard for 452 ms* — was
    /// unanswerable from the census as a result. A whole-family scan and a
    /// narrow prefix read have completely different remedies, so they are
    /// counted separately rather than adjudicated by inspection afterwards.
    ScanCfAtOverlay,
    /// The overlay half of a range-bounded `scan_cf_range_at`. See
    /// [`Self::ScanCfAtOverlay`].
    ScanCfRangeAtOverlay,
    OverlayTableKeys,
    SnapshotGcDebt,
    /// The per-shard **write** guard the snapshot-version reclaimer takes
    /// (#2122).
    ///
    /// The one write site in an otherwise read-only census, and deliberately so.
    /// #2122 was diagnosed from `snapshot_gc_debt` showing `holds=0` since boot
    /// against `read_latest`'s 193,269,889 — the census was the instrument that
    /// proved the reclaimer had never run. Reclamation now runs, and the same
    /// instrument has to answer the follow-up question the fix creates: does it
    /// stall commits? A write hold blocks every commit routed to that shard, so
    /// its duration is the number that matters, and putting it anywhere else
    /// would mean proving boundedness in a different instrument from the one
    /// that proved the absence.
    SnapshotVersionReclaim,
    /// Latest-snapshot registration. The read guard closes the race with a
    /// checkpoint-time delta rebase that takes every shard for write.
    PinSnapshot,
    /// Historical-snapshot registration. See [`Self::PinSnapshot`].
    PinSnapshotAt,
    PinSnapshotForPanel,
    PanelContentSeqsSnapshot,
    MigratePanelContentSeqsToAtLeast,
    /// The router-excluded row-table census that proves the #1978 gate is safe.
    ///
    /// Counted as its own site rather than folded into `count_cf_latest`
    /// because it is a *diagnostic* read with different semantics: it never
    /// consults the router, so a hold here is not evidence about the latest
    /// view any caller actually serves from.
    CountCfLatestTableOnly,
}

impl RowGuardSite {
    /// Every site, in declaration order. The census is indexed by position
    /// here, so this array is the contract that makes a zero-hold site
    /// reportable rather than invisible.
    pub const ALL: [Self; 25] = [
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
        Self::SnapshotPagedOverlay,
        Self::PredecessorCfAt,
        Self::ScanCfAtOverlay,
        Self::ScanCfRangeAtOverlay,
        Self::OverlayTableKeys,
        Self::SnapshotGcDebt,
        Self::SnapshotVersionReclaim,
        Self::PinSnapshot,
        Self::PinSnapshotAt,
        Self::PinSnapshotForPanel,
        Self::PanelContentSeqsSnapshot,
        Self::MigratePanelContentSeqsToAtLeast,
        Self::CountCfLatestTableOnly,
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
            Self::SnapshotPagedOverlay => "snapshot_paged_overlay",
            Self::PredecessorCfAt => "predecessor_cf_at",
            Self::ScanCfAtOverlay => "scan_cf_at_overlay",
            Self::ScanCfRangeAtOverlay => "scan_cf_range_at_overlay",
            Self::OverlayTableKeys => "overlay_table_keys",
            Self::SnapshotGcDebt => "snapshot_gc_debt",
            Self::SnapshotVersionReclaim => "snapshot_version_reclaim",
            Self::PinSnapshot => "pin_snapshot",
            Self::PinSnapshotAt => "pin_snapshot_at",
            Self::PinSnapshotForPanel => "pin_snapshot_for_panel",
            Self::PanelContentSeqsSnapshot => "panel_content_seqs_snapshot",
            Self::MigratePanelContentSeqsToAtLeast => "migrate_panel_content_seqs_to_at_least",
            Self::CountCfLatestTableOnly => "count_cf_latest_table_only",
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

    /// Atomically detaches every row table while all shards remain locked.
    ///
    /// Only checkpoint-time snapshot-delta rebasing may call this. The
    /// detached maps are destroyed after the guards are released so freeing a
    /// large journal cannot extend the all-shard write hold.
    fn take_all_for_snapshot_delta_rebase(&mut self) -> Vec<RowTable> {
        self.guards
            .iter_mut()
            .map(|(_, guard)| std::mem::take(&mut **guard))
            .collect()
    }

    // `iter`/`iter_mut` over every locked shard were removed with the
    // whole-table snapshot-version reclaim they existed for (#2122). Nothing
    // else wants a writer that spans shards: the recovery restore paths write
    // through `entry_mut` per column family, and the reclaimer now takes one
    // shard's guard at a time. Keeping a cross-shard iterator on the write set
    // would keep the wide hold one call away from being reintroduced.
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

type ChangedKeyCommit = Vec<(ColumnFamily, Vec<u8>)>;
type ChangedKeysBySeq = BTreeMap<Seq, ChangedKeyCommit>;

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
    /// Exact logical bytes and row/version counts owned by `rows`.
    mvcc_resident: MvccResidentCounters,
    /// Background SST writer for sealed memtables (#1951).
    ///
    /// Declared **before** `router` so it is dropped first: dropping it stops
    /// and joins the flush thread, and that thread holds its own `Arc` on the
    /// router, so joining before releasing this store's handle keeps the router
    /// alive for exactly as long as something can still be writing into it.
    ///
    /// Started lazily on the first seal rather than at construction, because
    /// spawning a thread can fail and every constructor here is infallible. A
    /// vault that never seals never starts it.
    flusher: OnceLock<RouterFlusher>,
    /// The CF router, or `None` for a store with no physical projection.
    ///
    /// Behind **no lock**. It was `RwLock<Option<CfRouter>>`, and that lock was
    /// never guarding the `Option` — nothing replaces it after construction —
    /// it existed only because every `CfRouter` method took `&mut self`. The
    /// router shards its own state per column family now, so the outer lock was
    /// pure contention: it serialised a `Kv` commit against a `Base` scan that
    /// shared nothing with it (#1950).
    router: Option<Arc<CfRouter>>,
    router_latest_readback: AtomicBool,
    /// Earliest sequence after which the in-memory MVCC version chains are a
    /// complete changed-key journal. Latest-only recovery serves older
    /// checkpoint rows from the router without their original per-row
    /// sequence, so a delta query below this floor must fail closed and rebase
    /// instead of silently omitting checkpointed changes (#1842).
    changed_key_history_floor: AtomicU64,
    /// Ordered logical change journal above `changed_key_history_floor`.
    ///
    /// The MVCC row table is authoritative for values and historical reads,
    /// but folding every row-table key to answer "what changed after seq N?"
    /// makes an empty delta cost O(process-lifetime changed keys).  This
    /// journal is published under the same row write guard and at the same
    /// sequence boundary as the version chains, so a delta read visits only
    /// commits in its requested sequence range.  It is process-local by
    /// design: latest-only recovery establishes `changed_key_history_floor`
    /// from the immutable router baseline and replays every later WAL batch
    /// through this journal before readers are admitted.
    changed_keys_by_seq: RwLock<ChangedKeysBySeq>,
    router_eager_lookup_on_refresh: AtomicBool,
    read_barriers: RwLock<Vec<ReadBarrier>>,
    leases: LeaseRegistry,
    resource_counters: Arc<ResourceCounters>,
    snapshot_gc: SnapshotGcReclaimer,
    snapshot_gc_counters: SnapshotGcCounters,
    /// Where the next bounded snapshot-version GC pass resumes (#2122).
    ///
    /// A pass that stops on its budget must resume where it stopped, not at the
    /// start: restarting would reclaim the low shards over and over and never
    /// reach the high ones whenever the debt exceeds one pass's budget.
    snapshot_gc_cursor: std::sync::Mutex<gc::SnapshotGcCursor>,
    /// Per-site tallies for every row-table guard taken on this vault.
    row_guard_census: RowGuardCensus,
    /// Exact `O(1)` per-family change signal (#2139). See [`CfChangeCell`] and
    /// [`new_cf_change_cells`]. Fixed length, allocated once, never resized.
    cf_change: Vec<CfChangeCell>,
    /// Exact logical cardinalities that have received one physical baseline.
    ///
    /// Only one fixed-size entry per measured column family is retained. Every
    /// normal logical write is advanced at the commit boundary; an out-of-band
    /// level replacement removes the entry before publishing its new view, so
    /// a caller either receives a proved exact count or must establish a new
    /// physical baseline.
    exact_cf_cardinality: Mutex<BTreeMap<ColumnFamily, ExactCfCardinality>>,
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

    /// The background SST writer, started on first use.
    ///
    /// # Errors
    ///
    /// Returns an error when this store has no router, or when the OS refuses
    /// the flush thread. Fails closed rather than writing inline: a vault that
    /// silently kept paying the SST write on the committing thread would look
    /// exactly like this change working.
    fn flusher(&self) -> Result<&RouterFlusher> {
        if let Some(started) = self.flusher.get() {
            return Ok(started);
        }
        let router = self.router.as_ref().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "a sealed memtable needs the background flusher, but this store has no CF router"
                    .to_owned(),
            )
        })?;
        // A racing caller may win `set`; the loser's flusher drops here, which
        // stops and joins the thread it just started. It can never have been
        // submitted to, so nothing is lost — only a thread spawn is wasted, and
        // only on the first seal of a vault's life.
        let started = RouterFlusher::start(Arc::clone(router))?;
        let _ = self.flusher.set(started);
        self.flusher.get().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "the background flusher was set and immediately read back absent".to_owned(),
            )
        })
    }

    /// Waits until every sealed memtable has been written and installed.
    ///
    /// The durability contract this exists for: router-flush SSTs are not the
    /// recovery authority (the WAL is), but a checkpoint that reported success
    /// while an SST write was outstanding — or had **failed** — would be
    /// claiming a physical projection it cannot show. Surfaces the flusher's
    /// first failure rather than swallowing it (#1951).
    ///
    /// # Errors
    ///
    /// Returns the first background write failure, or an error when the queue
    /// does not drain inside its budget.
    pub fn drain_pending_flushes(&self) -> Result<()> {
        match self.flusher.get() {
            Some(flusher) => flusher.drain(),
            None => Ok(()),
        }
    }

    /// What the background flusher has written, failed, and waited on (#1951).
    ///
    /// All zeroes before the first seal, which is the truth: the thread is
    /// started lazily and a vault that never sealed never had one.
    #[must_use]
    pub fn flush_status(&self) -> FlushStatus {
        self.flusher
            .get()
            .map_or_else(FlushStatus::default, RouterFlusher::status)
    }

    pub fn new(start_seq: Seq) -> Self {
        Self {
            seqs: SeqAllocator::new(start_seq),
            derived_content_seq: AtomicU64::new(0),
            panel_content_seqs: RwLock::new(BTreeMap::new()),
            next_lease_id: AtomicU64::new(0),
            rows: new_row_shards(),
            mvcc_resident: MvccResidentCounters::default(),
            flusher: OnceLock::new(),
            router: None,
            router_latest_readback: AtomicBool::new(false),
            changed_key_history_floor: AtomicU64::new(0),
            changed_keys_by_seq: RwLock::new(BTreeMap::new()),
            router_eager_lookup_on_refresh: AtomicBool::new(true),
            read_barriers: RwLock::new(Vec::new()),
            leases: LeaseRegistry::default(),
            resource_counters: Arc::new(ResourceCounters::default()),
            snapshot_gc: SnapshotGcReclaimer::default(),
            snapshot_gc_counters: SnapshotGcCounters::default(),
            snapshot_gc_cursor: std::sync::Mutex::new(gc::SnapshotGcCursor::default()),
            row_guard_census: RowGuardCensus::default(),
            cf_change: new_cf_change_cells(),
            exact_cf_cardinality: Mutex::new(BTreeMap::new()),
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
            mvcc_resident: MvccResidentCounters::default(),
            flusher: OnceLock::new(),
            router: Some(Arc::new(router)),
            router_latest_readback: AtomicBool::new(router_latest_readback),
            changed_key_history_floor: AtomicU64::new(if router_latest_readback {
                start_seq
            } else {
                0
            }),
            changed_keys_by_seq: RwLock::new(BTreeMap::new()),
            router_eager_lookup_on_refresh: AtomicBool::new(eager_lookup_on_refresh),
            read_barriers: RwLock::new(Vec::new()),
            leases: LeaseRegistry::default(),
            resource_counters,
            snapshot_gc: SnapshotGcReclaimer::default(),
            snapshot_gc_counters: SnapshotGcCounters::default(),
            snapshot_gc_cursor: std::sync::Mutex::new(gc::SnapshotGcCursor::default()),
            row_guard_census: RowGuardCensus::default(),
            cf_change: new_cf_change_cells(),
            exact_cf_cardinality: Mutex::new(BTreeMap::new()),
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
    /// 1. Point readers keep the router **shard** read lock across their SST
    ///    read. Streaming readers open every immutable handle while holding
    ///    that same guard, then retain the open handles plus Arc-backed lookup
    ///    metadata after releasing it. An open Unix file description survives
    ///    unlink, and Rust's Windows `OpenOptions` default includes
    ///    `FILE_SHARE_DELETE`, so a delete-pending file remains readable
    ///    through the already-open handle.
    /// 2. After the swap the retired paths are absent from every level, so no
    ///    reader can newly open them.
    ///
    /// Together those mean a reader has either finished its protected point
    /// read or already owns the exact immutable handle before a retired path
    /// can be purged. `doomed` must already be canonicalized and validated by
    /// the caller — that work is filesystem I/O and stays outside the lock too.
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
        // Before the level swap below makes the new view visible (#2139).
        // Retention GC reaches this path with inputs whose replacement dropped
        // expired rows, so this is a real content change that allocates no
        // sequence — the one case a sequence-keyed memo cannot see.
        self.note_cf_content_changed_outside_commit(&unique)?;

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
        // Before `reclaim()` touches a file (#2139); see
        // `retire_then_purge_cf_inputs`.
        self.note_cf_content_changed_outside_commit(&unique)?;
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

    /// Installs newly materialized durable checkpoint files into this
    /// process's live router before the durable manifest can advance past
    /// their WAL cohort.
    pub(crate) fn install_materialized_checkpoint_ssts(
        &self,
        files: &[(ColumnFamily, SstSummary)],
        operation: &'static str,
    ) -> Result<()> {
        let Some(router) = self.router.as_ref() else {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "{operation}: checkpoint publication requires a live CF router to install {} materialized SSTs",
                files.len()
            )));
        };
        router.install_materialized_ssts(files, operation)
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
        // Retiring the CF removes its rows outright, and allocates no sequence
        // (#2139). Published before the retire so no reader can see the emptied
        // family through an unchanged signal.
        self.note_cf_content_changed_outside_commit(&[cf])?;
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

    /// This family's change signal, read at this instant, in `O(1)` (#2139).
    ///
    /// See [`CfChangeSignal`] for what the two fields license. Reading is two
    /// relaxed-cost atomic loads and touches no lock, no row, and no shard, so
    /// a caller may ask on every cycle without the question costing what the
    /// answer avoids — which is the whole point of the issue this closes.
    pub fn cf_change_signal(&self, cf: ColumnFamily) -> CfChangeSignal {
        let cell = &self.cf_change[row_shard_index(cf)];
        // The two loads are not atomic together, and do not need to be. Both
        // counters are monotonically non-decreasing and both are published
        // *before* the change they describe becomes visible to any reader, so a
        // pair read at instants `t1 <= t2` proves "unchanged through `t1`" —
        // a real instant, which is all a physical walk gives either.
        let last_commit_seq = cell.last_commit_seq.load(Ordering::Acquire);
        let out_of_band_epoch = cell.out_of_band_epoch.load(Ordering::Acquire);
        CfChangeSignal {
            last_commit_seq,
            out_of_band_epoch,
        }
    }

    /// Returns the exact transaction-maintained latest cardinality when a
    /// physical baseline exists and still matches the family's publication
    /// signals.
    ///
    /// The row-shard read guard is the visibility boundary: a concurrent
    /// commit cannot publish its maintained count while its rows remain hidden
    /// (or vice versa). A mismatched signal removes the stale aggregate and
    /// returns `None`; it is never served approximately.
    pub fn exact_cf_cardinality(&self, cf: ColumnFamily) -> Result<Option<ExactCfCardinality>> {
        let _row_guard = self.read_rows(RowGuardSite::CountCfLatest, cf);
        let signal = self.cf_change_signal(cf);
        let mut cardinalities = self.exact_cf_cardinality.lock().map_err(|_| CalyxError {
            code: "CALYX_ASTER_EXACT_CARDINALITY_LOCK_POISONED",
            message: format!(
                "exact cardinality state lock is poisoned while reading {}",
                cf.name()
            ),
            remediation: "stop this vault process, inspect the panic that poisoned exact cardinality state, then reopen the durable vault and establish a new physical count baseline",
        })?;
        let Some(cardinality) = cardinalities.get(&cf).copied() else {
            return Ok(None);
        };
        if cardinality.last_commit_seq != signal.last_commit_seq
            || cardinality.out_of_band_epoch != signal.out_of_band_epoch
        {
            cardinalities.remove(&cf);
            return Ok(None);
        }
        Ok(Some(cardinality))
    }

    /// Installs one exact physical count as the maintained baseline when the
    /// family has not changed since that count's snapshot.
    ///
    /// Returns `false` only for a real concurrent change. The supplied count is
    /// still an exact statement about its pinned snapshot, but is not allowed
    /// to seed latest-state maintenance across the intervening transition.
    pub fn install_exact_cf_cardinality(
        &self,
        cf: ColumnFamily,
        rows: usize,
        snapshot_seq: Seq,
        out_of_band_epoch_before_walk: u64,
    ) -> Result<bool> {
        let _row_guard = self.read_rows(RowGuardSite::CountCfLatest, cf);
        let signal = self.cf_change_signal(cf);
        if signal.last_commit_seq > snapshot_seq
            || signal.out_of_band_epoch != out_of_band_epoch_before_walk
        {
            return Ok(false);
        }
        let mut cardinalities = self.exact_cf_cardinality.lock().map_err(|_| CalyxError {
            code: "CALYX_ASTER_EXACT_CARDINALITY_LOCK_POISONED",
            message: format!(
                "exact cardinality state lock is poisoned while installing {} baseline at snapshot {snapshot_seq}",
                cf.name()
            ),
            remediation: "stop this vault process, inspect the panic that poisoned exact cardinality state, then reopen the durable vault and establish a new physical count baseline",
        })?;
        cardinalities.insert(
            cf,
            ExactCfCardinality {
                rows,
                last_commit_seq: signal.last_commit_seq,
                out_of_band_epoch: signal.out_of_band_epoch,
            },
        );
        Ok(true)
    }

    /// Greatest sequence that wrote a row into `cf`, in `O(1)`.
    ///
    /// The narrow form of [`Self::cf_change_signal`] for callers comparing
    /// against a sequence they already hold.
    pub fn latest_seq_for_cf(&self, cf: ColumnFamily) -> Seq {
        self.cf_change_signal(cf).last_commit_seq
    }

    /// Oldest sequence from which this process can prove an exact per-key
    /// changed-key delta.
    ///
    /// A latest-only durable recovery restores authoritative current rows but
    /// cannot recreate their older per-key sequence history. Long-lived delta
    /// consumers compare their cached baseline with this floor and rebuild from
    /// one current snapshot when the baseline is older.
    pub fn changed_key_history_floor(&self) -> Seq {
        self.changed_key_history_floor.load(Ordering::Acquire)
    }

    /// Publishes `seq` as the family's last-commit sequence for every family in
    /// `rows`.
    ///
    /// **Called under the row-table write guard, before the rows are applied.**
    /// That ordering is the whole proof: a reader can only observe this commit's
    /// rows after the guard is released, and the guard is released after this
    /// store, so no reader can see a row whose family still reports an older
    /// sequence. The reverse skew — the sequence published while the row is not
    /// yet visible — is harmless: it can only make a reader re-measure.
    ///
    /// `fetch_max` rather than `store` because recovery replays batches whose
    /// sequences are ordered but whose *families* interleave, and because a
    /// regression here would be indistinguishable from a quiet family.
    fn publish_cf_commit_seq(&self, rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)], seq: Seq) {
        for (cf, _key, _value) in rows {
            self.cf_change[row_shard_index(*cf)]
                .last_commit_seq
                .fetch_max(seq, Ordering::AcqRel);
        }
    }

    /// Records a physical change to a family's content that allocated no
    /// sequence (#2139).
    ///
    /// Retiring a router CF removes its rows outright; retiring compaction or
    /// retention-GC input SSTs replaces the served level for the family. Neither
    /// moves [`Self::current_seq`], so neither can be detected by a sequence
    /// comparison, and a count memo keyed only on sequences would survive a
    /// retention GC that deleted half the family. Bumping an epoch the memo
    /// compares for equality closes that hole without pretending a sequence was
    /// allocated.
    fn note_cf_content_changed_outside_commit(&self, cfs: &[ColumnFamily]) -> Result<()> {
        self.invalidate_exact_cf_cardinalities(cfs)?;
        for cf in cfs {
            self.cf_change[row_shard_index(*cf)]
                .out_of_band_epoch
                .fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
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
    ) -> Result<Snapshot> {
        // The previous reader may have been the lease that deferred a rebase.
        // Retry before registering this one so a just-released journal cannot
        // make the next operation fail its transient overlay budget while the
        // periodic checkpoint is still asleep.
        self.rebase_snapshot_delta_if_needed(clock)?;
        // Registration happens while holding one row shard. A rebase takes
        // every shard for write and rechecks the lease registry only after all
        // are held, so it either observes this lease or this pin observes the
        // new history floor. There is no check/register race.
        let _table = self.read_rows(RowGuardSite::PinSnapshot, ColumnFamily::Base);
        let seq = self.current_seq();
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::AcqRel) + 1;
        let lease = ReaderLease::new(lease_id, seq, clock.now(), max_age_ms);
        self.leases.register(lease);
        Ok(Snapshot::new(seq, freshness, lease)
            .with_derived_content_seq(self.derived_content_seq_at(seq)))
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
        self.rebase_snapshot_delta_if_needed(clock)?;
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
    ) -> Result<Snapshot> {
        let _table = self.try_read_rows(
            RowGuardSite::PinSnapshotAt,
            ColumnFamily::Base,
            "MVCC row-table lock was poisoned while pinning a historical snapshot",
        )?;
        let history_floor = self.changed_key_history_floor.load(Ordering::Acquire);
        let latest = self.current_seq();
        if seq < history_floor || seq > latest {
            return Err(CalyxError {
                code: "CALYX_ASTER_SNAPSHOT_SEQUENCE_UNAVAILABLE",
                message: format!(
                    "snapshot sequence {seq} is outside the process-local readable range {history_floor}..={latest}"
                ),
                remediation: "rebase the reader inside the reported range; released pre-rebase history is no longer retained in process memory",
            });
        }
        let lease_id = self.next_lease_id.fetch_add(1, Ordering::AcqRel) + 1;
        let lease = ReaderLease::new(lease_id, seq, clock.now(), max_age_ms);
        self.leases.register(lease);
        Ok(Snapshot::new(seq, freshness, lease)
            .with_derived_content_seq(self.derived_content_seq_at(seq)))
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

    /// Renews one still-live snapshot lease without changing the pinned state.
    ///
    /// Renewal is atomic in the lease registry. Missing, expired, or mismatched
    /// identities fail closed; a caller can never resurrect an expired pin or
    /// renew a different sequence under the same reader id.
    pub fn renew_snapshot(&self, snapshot: Snapshot, clock: &dyn Clock) -> Result<Snapshot> {
        let lease = snapshot.lease();
        match self.leases.renew(lease, clock.now()) {
            ReaderLeaseRenewal::Renewed(renewed) => {
                Ok(Snapshot::new(snapshot.seq(), snapshot.freshness(), renewed)
                    .with_derived_content_seq(snapshot.derived_content_seq()))
            }
            ReaderLeaseRenewal::Missing => Err(CalyxError::reader_lease_expired(format!(
                "reader lease {} for seq {} is no longer registered and cannot be renewed",
                lease.id(),
                lease.pinned_seq()
            ))),
            ReaderLeaseRenewal::Expired => Err(CalyxError::reader_lease_expired(format!(
                "reader lease {} for seq {} expired at {} before renewal acquired the registry",
                lease.id(),
                lease.pinned_seq(),
                lease.expires_at()
            ))),
            ReaderLeaseRenewal::PinnedSeqMismatch {
                registered,
                requested,
            } => Err(CalyxError::aster_corrupt_shard(format!(
                "reader lease {} renewal identity mismatch: registry pins seq {registered}, caller requested seq {requested}",
                lease.id()
            ))),
        }
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

    /// Exact logical payload currently owned by the in-memory MVCC delta.
    ///
    /// This is an O(1) telemetry read. It deliberately excludes allocator and
    /// container overhead, which remains authoritative only at the process
    /// memory Source of Truth.
    #[must_use]
    pub fn mvcc_resident_status(&self) -> MvccResidentStatus {
        self.mvcc_resident.snapshot()
    }

    /// Installs and retires a checkpointed process-local snapshot delta.
    ///
    /// Latest-only recovery keeps the immutable router as its baseline and the
    /// row table as an exact changed-key journal above
    /// `changed_key_history_floor`. Once that journal reaches half of the
    /// transient snapshot-overlay budget, retaining it after the same keys are
    /// installed in immutable SSTs is duplicate state: every later snapshot
    /// would clone it again, and unique keys could otherwise accumulate for
    /// the daemon's entire lifetime.
    ///
    /// A rebase takes every row shard for write. Snapshot registration holds a
    /// row read guard until its lease is registered, so the lease recheck under
    /// these guards closes the check/register race. With no live lease, the
    /// method drains sealed memtables, flushes the current router view, advances
    /// the history floor to the now-stable current sequence, and detaches the
    /// redundant maps. Any flush failure occurs before the floor/table change.
    /// Detached maps are destroyed after unlocking.
    pub fn rebase_snapshot_delta_if_needed(
        &self,
        clock: &dyn Clock,
    ) -> Result<SnapshotDeltaRebaseReport> {
        let before = self.mvcc_resident.snapshot();
        let previous_floor_seq = self.changed_key_history_floor.load(Ordering::Acquire);
        let unchanged = |active_leases| SnapshotDeltaRebaseReport {
            rebased: false,
            active_leases,
            previous_floor_seq,
            new_floor_seq: previous_floor_seq,
            flushed_ssts: 0,
            before,
            after: self.mvcc_resident.snapshot(),
        };
        if !self.router_latest_readback.load(Ordering::Acquire)
            || before.payload_bytes() < SNAPSHOT_DELTA_REBASE_TRIGGER_BYTES
        {
            return Ok(unchanged(0));
        }

        let mut tables = self.write_rows_all(
            "MVCC row-table lock was poisoned while rebasing the checkpointed snapshot delta",
        )?;
        let locked_before = self.mvcc_resident.snapshot();
        if locked_before.payload_bytes() < SNAPSHOT_DELTA_REBASE_TRIGGER_BYTES {
            return Ok(SnapshotDeltaRebaseReport {
                before: locked_before,
                after: locked_before,
                ..unchanged(0)
            });
        }
        let leases = self.leases.live_view(clock.now());
        if leases.active_leases > 0 {
            tracing::info!(
                code = "CALYX_ASTER_SNAPSHOT_DELTA_REBASE_DEFERRED",
                active_leases = leases.active_leases,
                oldest_pinned_seq = leases.oldest_pinned_seq,
                current_seq = self.current_seq(),
                payload_bytes = locked_before.payload_bytes(),
                trigger_bytes = SNAPSHOT_DELTA_REBASE_TRIGGER_BYTES,
                "retained the changed-key journal for live snapshots; the next checkpoint retries after lease release or expiry"
            );
            return Ok(SnapshotDeltaRebaseReport {
                active_leases: leases.active_leases,
                before: locked_before,
                after: locked_before,
                ..unchanged(leases.active_leases)
            });
        }

        // Every row shard is held, so no MVCC commit can publish a sequence or
        // enter the router while this physical serving baseline is installed.
        self.drain_pending_flushes()?;
        let flushed_ssts = self.flush_all_cfs()?.len();
        let new_floor_seq = self.current_seq();
        let retired = tables.take_all_for_snapshot_delta_rebase();
        let retired_changed_keys = {
            let mut journal = self.changed_keys_by_seq.write().map_err(|_| {
                CalyxError::aster_corrupt_shard(
                    "MVCC changed-key journal lock was poisoned while rebasing the checkpointed snapshot delta"
                        .to_owned(),
                )
            })?;
            std::mem::take(&mut *journal)
        };
        self.changed_key_history_floor
            .store(new_floor_seq, Ordering::Release);
        self.mvcc_resident.reset_after_rebase();
        drop(tables);
        drop(retired);
        drop(retired_changed_keys);
        let after = self.mvcc_resident.snapshot();
        tracing::info!(
            code = "CALYX_ASTER_SNAPSHOT_DELTA_REBASED",
            previous_floor_seq,
            new_floor_seq,
            flushed_ssts,
            retired_keys = locked_before.keys,
            retired_versions = locked_before.versions,
            retired_payload_bytes = locked_before.payload_bytes(),
            remaining_keys = after.keys,
            remaining_versions = after.versions,
            remaining_payload_bytes = after.payload_bytes(),
            "installed the immutable router baseline and retired its redundant process-local changed-key journal"
        );
        Ok(SnapshotDeltaRebaseReport {
            rebased: true,
            active_leases: 0,
            previous_floor_seq,
            new_floor_seq,
            flushed_ssts,
            before: locked_before,
            after,
        })
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

        // Back-pressure, taken **before any lock** (#1951 ask 3).
        //
        // `MAX_SEALED_MEMTABLES_PER_CF` used to be a safety net that should
        // never fire, because the commit wrote its own SST before returning. It
        // is the throttle now, so hitting it must be a deliberate wait rather
        // than a failed user-facing commit — RocksDB stalls writers at
        // `max_write_buffer_number` for the same reason, and keeps an explicit
        // escape (`no_slowdown` -> `Status::Incomplete`) for callers that would
        // rather fail; the wait budget is that escape here.
        //
        // The ordering is not incidental. Waiting while holding a row shard or
        // a router shard would block the flusher's own install, so the waiter
        // would be waiting on progress it is itself preventing. Nothing is held
        // at this point.
        if let Some(flusher) = self.flusher.get() {
            flusher.await_capacity(MAX_OUTSTANDING_FLUSHES)?;
        }

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
            self.router.as_deref()
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
        let history_baseline_started = Instant::now();
        self.ensure_router_history_baselines(&mut table, &rows)?;
        let cardinality_deltas = self.prepare_exact_cf_cardinality_deltas(&mut table, &rows)?;
        timings.history_baseline_us = elapsed_us(&history_baseline_started);
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
        let seq = self.allocate_and_publish_changed_keys(&rows)?;
        // Publish the per-family change signal BEFORE the rows are applied and
        // while the row write guard for every touched family is still held
        // (#2139). A reader can reach these rows only after the guard is
        // released, so it can never observe a row whose family still reports a
        // sequence below this commit's.
        self.publish_cf_commit_seq(&rows, seq);
        timings.watermark_us = elapsed_us(&attribution_started)
            .saturating_sub(timings.panel_attribution_us)
            .saturating_sub(timings.history_baseline_us);
        let row_apply_started = Instant::now();
        for (cf, key, value) in &rows {
            self.append_mvcc_version(&mut table, *cf, key.clone(), seq, value.clone())?;
        }
        self.apply_exact_cf_cardinality_deltas(&cardinality_deltas, seq)?;
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
            // NO LOCK IS HELD HERE, AND NEITHER IS THIS THREAD ANY MORE. The
            // AEAD seal plus the file write measured 20-67 ms per commit and
            // was 89-98% of the whole `mvcc` stage once #1950 removed the lock
            // terms. #1949 took it off the locks; this takes it off the
            // caller (#1951).
            //
            // The rows are durable before this point regardless: the WAL
            // record is fsynced by the group commit above, and recovery never
            // restores a row from a router-flush SST. What the handoff defers
            // is the *projection*, and a sealed memtable stays in the router's
            // read path until its SST installs, so no reader can miss its rows
            // while it waits.
            //
            // A background write that fails is not swallowed: it returns its
            // rows to the active memtable, is logged against
            // CALYX_ASTER_ROUTER_BACKGROUND_FLUSH_FAILED, and fails the next
            // `drain_pending_flushes` — which `checkpoint` calls, so no
            // checkpoint can report a projection that was never written.
            let flush_count = sealed.len();
            self.flusher()?.submit(sealed)?;
            timings.sst_write_us = elapsed_us(&sst_write_started);
            tracing::debug!(
                code = "CALYX_ASTER_ROUTER_FLUSH_DEFERRED",
                committed_seq = seq,
                sealed = flush_count,
                enqueue_us = timings.sst_write_us,
                "handed sealed memtables to the background flusher"
            );
            timings.total_us = elapsed_us(&started);
            return Ok(seq);
        }
        timings.total_us = elapsed_us(&started);
        Ok(seq)
    }

    /// Captures the opening router view only for keys first changed during this
    /// process. The immutable SST/router state is the baseline Source of Truth;
    /// the row table is a delta journal above `changed_key_history_floor`.
    ///
    /// A baseline tombstone is just as important as a baseline value: without
    /// it, a snapshot pinned before a key's first insertion would fall through
    /// to the now-newer router and incorrectly observe that insertion. Values
    /// are captured before any router publication and while every touched row
    /// shard is held exclusively, so a reader sees either the old router or a
    /// complete baseline+new-version chain, never the transition between them.
    fn ensure_router_history_baselines(
        &self,
        table: &mut RowWriteSet<'_>,
        rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)],
    ) -> Result<()> {
        if !self.router_latest_readback.load(Ordering::Acquire) {
            return Ok(());
        }
        let router = self.router.as_ref().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "router-backed MVCC history requested without a CF router".to_owned(),
            )
        })?;
        let history_floor = self.changed_key_history_floor.load(Ordering::Acquire);
        for (cf, key, _value) in rows {
            let has_baseline = table
                .entry_mut(*cf)?
                .get(key)
                .is_some_and(|chain| chain.iter().any(|version| version.seq <= history_floor));
            if has_baseline {
                continue;
            }
            let baseline = router
                .get(*cf, key)?
                .unwrap_or_else(|| TOMBSTONE_VALUE.to_vec());
            let key_bytes = u64::try_from(key.len()).unwrap_or(u64::MAX);
            let value_bytes = u64::try_from(baseline.len()).unwrap_or(u64::MAX);
            let family = table.entry_mut(*cf)?;
            let new_key = match family.entry(key.clone()) {
                Entry::Vacant(entry) => {
                    let mut chain = VersionChain::new();
                    chain.push_front(VersionedValue {
                        seq: history_floor,
                        value: baseline,
                    });
                    entry.insert(chain);
                    true
                }
                Entry::Occupied(mut entry) => {
                    entry.get_mut().push_front(VersionedValue {
                        seq: history_floor,
                        value: baseline,
                    });
                    false
                }
            };
            self.mvcc_resident
                .record_insert(new_key, key_bytes, value_bytes);
        }
        Ok(())
    }

    /// Allocates one commit sequence and publishes its deduplicated changed
    /// identities into the ordered delta journal.
    ///
    /// The caller holds the write shard for every family in `rows`.  A delta
    /// reader takes that same family shard before reading the journal, so even
    /// though the sequence allocator itself is atomic, no reader can observe
    /// the allocated sequence without also observing this journal entry.
    fn allocate_and_publish_changed_keys(
        &self,
        rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)],
    ) -> Result<Seq> {
        let changes = rows
            .iter()
            .map(|(cf, key, _value)| (*cf, key.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut journal = self.changed_keys_by_seq.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC changed-key journal lock was poisoned while publishing a commit".to_owned(),
            )
        })?;
        let seq = self.seqs.allocate();
        if seq > self.changed_key_history_floor.load(Ordering::Acquire) {
            journal.insert(seq, changes);
        }
        Ok(seq)
    }

    /// Restores one already-sequenced batch into the process-local changed-key
    /// journal. Manifest-baseline rows at or below the history floor are not a
    /// delta and are deliberately excluded; every WAL row above it is retained.
    fn restore_changed_keys_at(
        &self,
        rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)],
        seq: Seq,
    ) -> Result<()> {
        if seq <= self.changed_key_history_floor.load(Ordering::Acquire) {
            return Ok(());
        }
        let changes = rows
            .iter()
            .map(|(cf, key, _value)| (*cf, key.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut journal = self.changed_keys_by_seq.write().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "MVCC changed-key journal lock was poisoned while restoring a durable batch"
                    .to_owned(),
            )
        })?;
        let entry = journal.entry(seq).or_default();
        entry.extend(changes);
        entry.sort();
        entry.dedup();
        Ok(())
    }

    fn invalidate_exact_cf_cardinalities(&self, cfs: &[ColumnFamily]) -> Result<()> {
        if cfs.is_empty() {
            return Ok(());
        }
        let mut cardinalities = self.exact_cf_cardinality.lock().map_err(|_| CalyxError {
            code: "CALYX_ASTER_EXACT_CARDINALITY_LOCK_POISONED",
            message: format!(
                "exact cardinality state lock is poisoned while invalidating {} family/families",
                cfs.len()
            ),
            remediation: "stop this vault process, inspect the panic that poisoned exact cardinality state, then reopen the durable vault and establish new physical count baselines",
        })?;
        for cf in cfs {
            cardinalities.remove(cf);
        }
        Ok(())
    }

    fn prepare_exact_cf_cardinality_deltas(
        &self,
        table: &mut RowWriteSet<'_>,
        rows: &[(ColumnFamily, Vec<u8>, Vec<u8>)],
    ) -> Result<Vec<PreparedCfCardinalityDelta>> {
        let current_seq = self.current_seq();
        let maintained = {
            let mut cardinalities = self.exact_cf_cardinality.lock().map_err(|_| CalyxError {
                code: "CALYX_ASTER_EXACT_CARDINALITY_LOCK_POISONED",
                message: "exact cardinality state lock is poisoned while preparing a commit"
                    .to_owned(),
                remediation: "stop this vault process, inspect the panic that poisoned exact cardinality state, then reopen the durable vault and establish new physical count baselines",
            })?;
            let touched = rows.iter().map(|(cf, _, _)| *cf).collect::<BTreeSet<_>>();
            let mut maintained = BTreeMap::new();
            for cf in touched {
                let signal = self.cf_change_signal(cf);
                match cardinalities.get(&cf).copied() {
                    Some(cardinality)
                        if cardinality.last_commit_seq == signal.last_commit_seq
                            && cardinality.out_of_band_epoch == signal.out_of_band_epoch =>
                    {
                        maintained.insert(cf, cardinality);
                    }
                    Some(_) => {
                        cardinalities.remove(&cf);
                    }
                    None => {}
                }
            }
            maintained
        };
        if maintained.is_empty() {
            return Ok(Vec::new());
        }

        // One batch may name the same key more than once. Cardinality depends
        // on the final state only, so collapse to the last value before
        // comparing old and new logical liveness.
        let mut final_states = BTreeMap::<(ColumnFamily, Vec<u8>), bool>::new();
        for (cf, key, value) in rows {
            if maintained.contains_key(cf) {
                final_states.insert((*cf, key.clone()), !is_tombstone_value(value));
            }
        }
        let mut deltas = maintained
            .iter()
            .map(|(cf, cardinality)| {
                (
                    *cf,
                    PreparedCfCardinalityDelta {
                        cf: *cf,
                        delta: 0,
                        previous_last_commit_seq: cardinality.last_commit_seq,
                        out_of_band_epoch: cardinality.out_of_band_epoch,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for ((cf, key), new_live) in final_states {
            let old_live = table
                .entry_mut(cf)?
                .get(&key)
                .and_then(|versions| read::visible_value_state(versions, current_seq))
                .is_some_and(|state| matches!(state, read::VisibleValue::Live(_)));
            let delta = match (old_live, new_live) {
                (false, true) => 1,
                (true, false) => -1,
                _ => 0,
            };
            let prepared = deltas.get_mut(&cf).ok_or_else(|| CalyxError {
                code: "CALYX_ASTER_EXACT_CARDINALITY_PREPARE_CORRUPT",
                message: format!(
                    "prepared exact cardinality family {} disappeared while folding commit transitions",
                    cf.name()
                ),
                remediation: "do not retry the write; inspect the exact-cardinality preparation path before reopening the vault",
            })?;
            prepared.delta = prepared.delta.checked_add(delta).ok_or_else(|| CalyxError {
                code: "CALYX_ASTER_EXACT_CARDINALITY_DELTA_OVERFLOW",
                message: format!(
                    "exact cardinality delta overflow while preparing {} commit",
                    cf.name()
                ),
                remediation: "reduce the atomic write batch below the platform integer bound and retry",
            })?;
        }
        Ok(deltas.into_values().collect())
    }

    fn apply_exact_cf_cardinality_deltas(
        &self,
        deltas: &[PreparedCfCardinalityDelta],
        committed_seq: Seq,
    ) -> Result<()> {
        if deltas.is_empty() {
            return Ok(());
        }
        let mut cardinalities = self.exact_cf_cardinality.lock().map_err(|_| CalyxError {
            code: "CALYX_ASTER_EXACT_CARDINALITY_RECONCILIATION_REQUIRED",
            message: format!(
                "MVCC rows committed at sequence {committed_seq}, but the exact cardinality state lock is poisoned"
            ),
            remediation: "treat committed_seq as applied; stop the process, inspect the poisoning panic, reopen the durable vault, and establish new physical count baselines before retrying any write",
        })?;
        for delta in deltas {
            let Some(cardinality) = cardinalities.get(&delta.cf).copied() else {
                // An out-of-band invalidation won the race. Absence is the
                // fail-closed state; no stale count can be served.
                continue;
            };
            if cardinality.last_commit_seq != delta.previous_last_commit_seq
                || cardinality.out_of_band_epoch != delta.out_of_band_epoch
            {
                cardinalities.remove(&delta.cf);
                continue;
            }
            let magnitude = usize::try_from(delta.delta.unsigned_abs()).map_err(|_| CalyxError {
                code: "CALYX_ASTER_EXACT_CARDINALITY_DELTA_OVERFLOW",
                message: format!(
                    "exact cardinality delta {} for {} exceeds this platform's usize",
                    delta.delta,
                    delta.cf.name()
                ),
                remediation: "stop the process and inspect the atomic write-batch bound; do not use the invalidated cardinality until a physical baseline is re-established",
            })?;
            let updated = if delta.delta >= 0 {
                cardinality.rows.checked_add(magnitude)
            } else {
                cardinality.rows.checked_sub(magnitude)
            };
            let Some(updated) = updated else {
                cardinalities.remove(&delta.cf);
                return Err(CalyxError {
                    code: "CALYX_ASTER_EXACT_CARDINALITY_RECONCILIATION_REQUIRED",
                    message: format!(
                        "MVCC rows committed at sequence {committed_seq}, but applying delta {} to {} count {} overflowed or underflowed",
                        delta.delta,
                        delta.cf.name(),
                        cardinality.rows
                    ),
                    remediation: "treat committed_seq as applied; stop the process, inspect old/new key-state transition accounting, reopen the durable vault, and establish a new physical count baseline before retrying any write",
                });
            };
            let Some(cardinality) = cardinalities.get_mut(&delta.cf) else {
                return Err(CalyxError {
                    code: "CALYX_ASTER_EXACT_CARDINALITY_RECONCILIATION_REQUIRED",
                    message: format!(
                        "MVCC rows committed at sequence {committed_seq}, but exact cardinality state for {} disappeared while applying its delta",
                        delta.cf.name()
                    ),
                    remediation: "treat committed_seq as applied; stop the process, inspect exact cardinality state mutation, reopen the durable vault, and establish a new physical count baseline before retrying any write",
                });
            };
            cardinality.rows = updated;
            cardinality.last_commit_seq = committed_seq;
        }
        Ok(())
    }

    fn append_mvcc_version(
        &self,
        table: &mut RowWriteSet<'_>,
        cf: ColumnFamily,
        key: Vec<u8>,
        seq: Seq,
        value: Vec<u8>,
    ) -> Result<()> {
        let key_bytes = u64::try_from(key.len()).unwrap_or(u64::MAX);
        let value_bytes = u64::try_from(value.len()).unwrap_or(u64::MAX);
        let family = table.entry_mut(cf)?;
        let new_key = match family.entry(key) {
            Entry::Vacant(entry) => {
                let mut chain = VersionChain::new();
                chain.push_back(VersionedValue { seq, value });
                entry.insert(chain);
                true
            }
            Entry::Occupied(mut entry) => {
                entry.get_mut().push_back(VersionedValue { seq, value });
                false
            }
        };
        self.mvcc_resident
            .record_insert(new_key, key_bytes, value_bytes);
        Ok(())
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
        let restored_cfs = rows.iter().map(|(cf, _, _)| *cf).collect::<BTreeSet<_>>();
        self.invalidate_exact_cf_cardinalities(&restored_cfs.into_iter().collect::<Vec<_>>())?;
        // Recovery is whole-table by nature and runs single-threaded at open.
        let mut table =
            self.write_rows_all("MVCC row-table lock was poisoned during atomic recovery restore")?;
        let latest_router = if matches!(
            attribution,
            PanelAttribution::Strict | PanelAttribution::ReplayedCommit
        ) && self.router_latest_readback.load(Ordering::Acquire)
        {
            self.router.as_deref()
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
        // Same publication point as the live commit path (#2139): recovery
        // restores rows at their original sequences, so the per-family signal
        // must carry them too or a family whose only writes were replayed would
        // report `last_commit_seq = 0` while holding rows at much higher ones.
        self.publish_cf_commit_seq(&rows, seq);
        self.restore_changed_keys_at(&rows, seq)?;
        for (cf, key, value) in rows {
            self.append_mvcc_version(&mut table, cf, key, seq, value)?;
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
        let restored_cfs = batches
            .iter()
            .flat_map(|(_seq, rows)| rows.iter().map(|(cf, _, _)| *cf))
            .collect::<BTreeSet<_>>();
        self.invalidate_exact_cf_cardinalities(&restored_cfs.into_iter().collect::<Vec<_>>())?;
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
                    router.map(std::convert::AsRef::as_ref)
                } else {
                    None
                },
                seq.saturating_sub(1),
                &rows,
                attribution,
            )?;
            self.ensure_router_history_baselines(&mut table, &rows)?;
            if rows
                .iter()
                .any(|(cf, _, _)| cf.feeds_persistent_search_index())
            {
                self.derived_content_seq.fetch_max(seq, Ordering::AcqRel);
            }
            self.advance_affected_panel_content_seqs(&affected_panels, seq)?;
            self.publish_cf_commit_seq(&rows, seq);
            self.restore_changed_keys_at(&rows, seq)?;
            for (cf, key, value) in rows {
                self.append_mvcc_version(&mut table, cf, key, seq, value)?;
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
        // A replacement can move an identity between panel generations. Both
        // memberships become stale: the old panel lost a row and the new panel
        // gained it. Reading the prior visible panel only for tombstones left
        // the old generation falsely fresh after an in-place panel move.
        let prior_panel = visible_base_panel(table, latest_router, key, visible_seq)?;
        if let Some(prior_panel) = prior_panel {
            panels.insert(prior_panel);
        }
        let panel_version = if tombstone {
            None
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
        .and_then(|base| base.get(key));
    if let Some(versions) = versions {
        for version in versions.iter().filter(|version| version.seq <= seq) {
            if is_tombstone_value(&version.value) {
                continue;
            }
            panels.insert(panel_from_base_value(key, &version.value)?);
        }
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
