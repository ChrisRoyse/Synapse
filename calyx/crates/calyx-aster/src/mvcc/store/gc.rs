use super::{RowGuardSite, RowTable, VersionChain, VersionedCfStore};
use crate::cf::ColumnFamily;
use crate::gc::{GcMetrics, GcRateLimit, GcResult, SnapshotVersionGc};
use calyx_core::{CalyxError, Clock, Result, Seq, Ts};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Instant;

/// Default cap on versions reclaimed by one pass.
///
/// Sized against the thing it has to keep up with, not against a round number:
/// the production daemon in #2122 grew its private commit by ~1.07 GB/hour
/// while quiet and ~6 GB/hour loaded, and the reclaimer runs on the 5-minute
/// storage-maintenance tick, so one pass has to be able to free ~90 MB (quiet)
/// to ~500 MB (loaded) of version chain to hold the line. At the vault's
/// observed mean row size that is hundreds of thousands of versions, not the
/// 1,000 that [`crate::gc::DEFAULT_GC_MAX_OPS_PER_RUN`] allows — 1,000 per
/// 5 minutes is 3.3 versions/second against a daemon committing continuously,
/// which cannot converge from any starting debt.
pub const DEFAULT_SNAPSHOT_VERSION_GC_MAX_VERSIONS: usize = 500_000;

/// Default cap on version chains *examined* by one pass.
///
/// Separate from the version cap because a clean chain costs a walk and frees
/// nothing: without this, a pass over a vault whose debt is already drained
/// would still walk every key in the vault looking for work that is not there.
pub const DEFAULT_SNAPSHOT_VERSION_GC_MAX_CHAINS: usize = 4_000_000;

/// Default wall-clock budget for one pass, in microseconds.
pub const DEFAULT_SNAPSHOT_VERSION_GC_MAX_PASS_US: u64 = 2_000_000;

/// Default cap on how long one shard's row-table **write** guard may be held.
///
/// Deliberately below [`super::ROW_READ_GUARD_WARN_US`] (25 ms): the write
/// guard blocks every commit routed to that shard, so a reclaimer that ran past
/// the budget the read guards are judged against would be trading a memory leak
/// for the commit stall #1950 and #2060 spent two issues removing.
///
/// It is a budget checked at [`CLOCK_CHECK_CHAINS`] granularity, not a hard
/// ceiling, and the difference is measured rather than hand-waved: an isolated
/// daemon under a 32 MB/min write load reported a maximum hold of **16.1 ms**
/// against this 15 ms budget (mean 650 us over 108 holds, **0** over-budget
/// holds against the 25 ms census budget). The ~1 ms overshoot is the work of
/// up to `CLOCK_CHECK_CHAINS` chains, which is what that constant buys.
pub const DEFAULT_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US: u64 = 15_000;

const MAX_VERSIONS_ENV: &str = "CALYX_SNAPSHOT_VERSION_GC_MAX_VERSIONS";
const MAX_CHAINS_ENV: &str = "CALYX_SNAPSHOT_VERSION_GC_MAX_CHAINS";
const MAX_PASS_US_ENV: &str = "CALYX_SNAPSHOT_VERSION_GC_MAX_PASS_US";
const MAX_SHARD_HOLD_US_ENV: &str = "CALYX_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US";

/// How often the shard walk consults the clock, in chains.
///
/// `Instant::now()` is a syscall-ish read on Windows (`QueryPerformanceCounter`);
/// doing it per chain would make the instrument a measurable fraction of the
/// work it measures. So the hold budget is enforced at this granularity, and
/// the overshoot it admits is bounded by this many chain visits rather than by
/// a time.
///
/// **Measured, not guessed.** At 256 this was too coarse to bind at all on the
/// shapes that matter: an isolated daemon under a 32 MB/min write load held
/// shards for up to **20.7 ms** against a 15 ms budget, because each shard held
/// only ~165 chains — fewer than one check interval — so the clock was never
/// consulted inside a shard and the hold ran to the shard's natural length. 32
/// makes the budget bind on those shards while still costing one clock read per
/// 32 chains, which is noise against the per-chain reclaim work (mean hold in
/// that same run was 599 us across 267 holds).
const CLOCK_CHECK_CHAINS: usize = 32;

/// Bounds on one snapshot-version GC pass.
///
/// Every field is a hard cap, not a target: a pass stops at the first one it
/// reaches and reports which (`SnapshotVersionGcPass::stopped_on`), then resumes
/// from its cursor on the next pass. There is no unbounded mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotVersionGcBudget {
    pub max_versions: usize,
    pub max_chains_scanned: usize,
    pub max_pass_us: u64,
    pub max_shard_hold_us: u64,
}

impl Default for SnapshotVersionGcBudget {
    fn default() -> Self {
        Self {
            max_versions: DEFAULT_SNAPSHOT_VERSION_GC_MAX_VERSIONS,
            max_chains_scanned: DEFAULT_SNAPSHOT_VERSION_GC_MAX_CHAINS,
            max_pass_us: DEFAULT_SNAPSHOT_VERSION_GC_MAX_PASS_US,
            max_shard_hold_us: DEFAULT_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US,
        }
    }
}

impl SnapshotVersionGcBudget {
    /// Reads the budget from the environment, failing closed on an unparseable
    /// or zero value rather than silently substituting a default.
    ///
    /// # Errors
    ///
    /// Returns `CALYX_GC_ERROR` when a variable is present but not a positive
    /// integer. A zero budget is refused because a pass that can do no work is
    /// indistinguishable from a reclaimer that is not wired up — which is
    /// exactly the state #2122 measured.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            max_versions: positive_env_usize(
                MAX_VERSIONS_ENV,
                DEFAULT_SNAPSHOT_VERSION_GC_MAX_VERSIONS,
            )?,
            max_chains_scanned: positive_env_usize(
                MAX_CHAINS_ENV,
                DEFAULT_SNAPSHOT_VERSION_GC_MAX_CHAINS,
            )?,
            max_pass_us: positive_env_u64(
                MAX_PASS_US_ENV,
                DEFAULT_SNAPSHOT_VERSION_GC_MAX_PASS_US,
            )?,
            max_shard_hold_us: positive_env_u64(
                MAX_SHARD_HOLD_US_ENV,
                DEFAULT_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US,
            )?,
        })
    }

    /// Scales the two budgets that decide how much a pass frees, leaving the
    /// per-shard hold budget alone.
    ///
    /// The hold budget is the commit-latency contract and is never scaled: a
    /// pressure response is allowed to spend more *total* time reclaiming, and
    /// is never allowed to hold one shard's write guard longer while doing it.
    #[must_use]
    pub fn scaled(self, factor: u32) -> Self {
        let factor = factor.max(1);
        Self {
            max_versions: self.max_versions.saturating_mul(factor as usize),
            max_chains_scanned: self.max_chains_scanned.saturating_mul(factor as usize),
            max_pass_us: self.max_pass_us.saturating_mul(u64::from(factor)),
            max_shard_hold_us: self.max_shard_hold_us,
        }
    }
}

/// Why one pass stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotVersionGcStop {
    /// Every shard and every chain was visited: the vault carries no reclaimable
    /// version below the floor as of this pass. This is the only outcome that
    /// licenses reporting a debt of zero.
    #[default]
    SweepCompleted,
    VersionBudget,
    ChainBudget,
    PassTimeBudget,
}

impl SnapshotVersionGcStop {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SweepCompleted => "sweep_completed",
            Self::VersionBudget => "version_budget",
            Self::ChainBudget => "chain_budget",
            Self::PassTimeBudget => "pass_time_budget",
        }
    }
}

/// The readback of one snapshot-version GC pass.
///
/// Reports what the pass *did*, never what it assumes. `sweep_completed` is the
/// load-bearing field: only a completed sweep proves the reclaimable debt is
/// zero, so a consumer that wants to claim "the chains are drained" has to read
/// it rather than infer it from `versions_reclaimed == 0` (which is equally
/// consistent with a pass that stopped on its first shard).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotVersionGcPass {
    /// The pinned-reader floor this pass reclaimed strictly below.
    pub floor_seq: Seq,
    /// The vault's committed sequence when the floor was taken.
    pub current_seq: Seq,
    /// Live reader leases at floor time. A non-zero count with
    /// `floor_seq < current_seq` is the signature of a reader holding the floor
    /// down, which is the one legitimate reason for a pass to free nothing.
    pub active_leases: usize,
    pub versions_reclaimed: u64,
    /// Sum of the reclaimed versions' value lengths. A lower bound on the RAM
    /// returned: it excludes the `Vec` headers and the allocator's per-block
    /// overhead, both of which are real and neither of which this can measure
    /// without lying about precision.
    pub bytes_reclaimed: u64,
    /// Chains that lost at least one version.
    pub chains_compacted: u64,
    /// Chains examined, whether or not they lost anything.
    pub chains_scanned: u64,
    /// Shards this pass walked all the way to their end.
    pub shards_visited: usize,
    pub shards_total: usize,
    /// Row-table shard write guards acquired. Larger than `shards_visited`
    /// whenever the per-shard hold budget made the pass release and re-acquire
    /// a shard mid-walk, which is the mechanism that keeps a long reclamation
    /// from becoming a long hold.
    pub shard_guard_holds: u64,
    /// Whether **this one pass** covered every chain in the vault.
    ///
    /// False whenever the pass started from a mid-shard cursor, even if it then
    /// walked every shard: the head of its starting shard belongs to the pass
    /// before it. Only a pass that both started at a shard boundary and finished
    /// every shard has seen the whole table, and only that pass may be read as
    /// "the debt is drained".
    pub sweep_completed: bool,
    pub stopped_on: SnapshotVersionGcStop,
    pub elapsed_us: u64,
    /// Longest single shard write-guard hold in this pass. This is the number
    /// that has to stay bounded for the reclaimer to be safe to run on a
    /// committing daemon. It can exceed `budget.max_shard_hold_us` by up to one
    /// clock-check interval's work — see
    /// [`DEFAULT_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US`] for the measured size
    /// of that overshoot.
    pub max_shard_hold_us: u64,
    /// Shard the next pass resumes from.
    pub resume_shard: usize,
}

/// Where the next pass resumes.
///
/// A cursor rather than a restart because a pass that stops on its budget must
/// not re-walk the shards it already cleaned before reaching the ones it did
/// not: with a restart, a vault whose debt exceeds one pass's budget would
/// reclaim the same low shards forever and never reach the high ones.
#[derive(Debug, Default)]
pub(super) struct SnapshotGcCursor {
    shard: usize,
    /// Resume point inside `shard`: continue **at** this key of this column
    /// family. `None` means "start of the shard".
    ///
    /// Inclusive, not exclusive, and that is a correctness property rather than
    /// a preference. A pass stops when its budget runs out *before* processing
    /// the key it is looking at, so an exclusive cursor would resume after a key
    /// that was never processed and skip it — verified by
    /// `snapshot_version_gc_fsv`, which caught exactly that as 72 versions
    /// reclaimed short of the debt census across 196 paged passes. Resuming *at*
    /// the key re-walks it, which is idempotent: reclamation of an already-clean
    /// chain frees nothing.
    resume: Option<(ColumnFamily, Vec<u8>)>,
}

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

    /// Lifetime snapshot-GC counters **without** the whole-table debt census.
    ///
    /// [`Self::snapshot_gc_metrics`] recomputes `compaction_debt` by walking
    /// every chain in the vault under a read guard covering every shard. That is
    /// the right answer for an operator asking "how much is left", and the wrong
    /// thing to put on a health read that runs every few seconds — it is
    /// `O(all versions)` and it blocks every commit queued behind it. Health
    /// reads this; the exact census stays available for callers willing to pay.
    pub fn snapshot_gc_counters_only(&self) -> GcMetrics {
        self.snapshot_gc_counters.metrics()
    }

    pub fn record_snapshot_gc_physical_bytes_freed(&self, bytes: usize) {
        self.snapshot_gc_counters.record_physical_bytes_freed(bytes);
    }

    pub fn compact_router_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()> {
        let Some(router) = self.router.as_ref() else {
            return Err(CalyxError {
                code: "CALYX_ASTER_COMPACTION_UNAVAILABLE",
                message: "tombstone compaction requires a physical CF router".to_string(),
                remediation: "open the MVCC store with a CF router before requesting compaction",
            });
        };
        router.compact_tombstoned_cfs_at(cfs, self.current_seq())
    }

    /// Reclaims snapshot-obsolete in-RAM MVCC versions, one shard at a time.
    ///
    /// # What it frees
    ///
    /// Every commit appends a full clone of its value bytes to the version chain
    /// for its key (`store.rs` `row_apply`). Nothing in Synapse ever called the
    /// reclaimer, so those clones accumulated for the life of the process —
    /// #2122 measured a 1.07 GB/hour ratchet with a maximum drawdown of 2.8 MB,
    /// and the `snapshot_gc_debt` guard site at zero holds since boot. This is
    /// the pass that makes that memory become freeable.
    ///
    /// # Why it is safe
    ///
    /// `floor` is the oldest sequence pinned by any live reader lease, or the
    /// current committed sequence when nothing is pinned. Only versions
    /// **strictly below** the floor are candidates, and within each chain the
    /// newest version at or below the floor is retained unconditionally. So:
    ///
    /// * A snapshot pinned at `S >= floor` still resolves correctly. Its answer
    ///   is the newest version at or below `S`; every version in `(floor, S]` is
    ///   above the floor and untouched, and if none exists the retained boundary
    ///   version is that answer.
    /// * No snapshot can be pinned below the floor — the floor is the minimum
    ///   over live leases — and a lease that has expired fails
    ///   `ensure_snapshot_live` before it can read.
    /// * A lease registered after the floor is taken pins the *current*
    ///   sequence, which is at or above the floor, so it falls under the first
    ///   case.
    /// * **No key entry is ever removed from the map.** `changed_keys_after_at`
    ///   pages its walk across guard releases and its cursor is exact only
    ///   because the key set is append-only (`store/read.rs`); removing an
    ///   emptied key would silently break that proof. Chains are trimmed in
    ///   place, exactly as that comment already promises.
    ///
    /// # Why it is bounded
    ///
    /// One shard's write guard at a time, in shard-index order, released between
    /// shards and released mid-shard when the hold budget expires. It takes no
    /// durable commit lock, writes no WAL, and touches no SST, so it cannot
    /// queue behind or in front of a commit — the only thing it can delay is a
    /// commit routed to the one shard it currently holds, for at most
    /// `budget.max_shard_hold_us`. Every hold is tallied at
    /// [`RowGuardSite::SnapshotVersionReclaim`], so "did reclamation stall
    /// commits" is a number in the census rather than an argument.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-shard error when a row-table shard lock is poisoned.
    pub fn reclaim_snapshot_versions_paged(
        &self,
        floor: Seq,
        budget: SnapshotVersionGcBudget,
        now: Ts,
    ) -> Result<SnapshotVersionGcPass> {
        let started = Instant::now();
        let view = self.leases.live_view(now);
        let mut walk = PassWalk::new(budget);
        let shards_total = self.rows.len();
        // The cursor guard is held for the whole pass, which makes reclamation
        // singly-threaded by construction. That is the intent: two concurrent
        // passes would share one resume cursor and each would advance it past
        // ground the other had not covered. A second caller waits at most one
        // pass budget, and it waits *here* — outside every row-table guard — so
        // it delays no commit while it waits. This is the outermost lock the
        // reclaimer takes; nothing acquires it while holding a row shard.
        let mut cursor = self
            .snapshot_gc_cursor
            .lock()
            .map_err(|_| CalyxError::aster_corrupt_shard("MVCC snapshot GC cursor poisoned"))?;
        let mut shard = cursor.shard.min(shards_total.saturating_sub(1));
        let mut resume = cursor.resume.take();
        // Whether this pass inherits a position inside a shard. It decides
        // whether the pass may claim a completed sweep: the head of that shard
        // belongs to whichever pass stopped there, not to this one.
        let started_mid_shard = resume.is_some();

        let mut shards_visited = 0usize;
        let mut shard_guard_holds = 0u64;
        while shards_visited < shards_total {
            // The pass budget is checked between guard acquisitions, never
            // inside a held guard: the per-shard hold budget governs how long
            // one hold may last, this one governs how long the pass may last.
            if walk.pass_budget_spent(&started) {
                break;
            }
            let acquired_cpu_us = super::thread_cpu_us();
            let acquired = Instant::now();
            let stopped_at = {
                let mut table = self.rows[shard].write().map_err(|_| {
                    CalyxError::aster_corrupt_shard(format!(
                        "MVCC row-table shard {shard} lock was poisoned during snapshot version GC"
                    ))
                })?;
                walk.reclaim_shard(&mut table, resume.as_ref(), floor, &acquired)
            };
            let held_us = super::elapsed_us(&acquired);
            walk.max_shard_hold_us = walk.max_shard_hold_us.max(held_us);
            shard_guard_holds += 1;
            super::record_row_guard_hold(
                &self.row_guard_census,
                RowGuardSite::SnapshotVersionReclaim,
                acquired,
                acquired_cpu_us,
            );
            let Some(key_cursor) = stopped_at else {
                // Shard walked to its end.
                shards_visited += 1;
                shard = (shard + 1) % shards_total;
                resume = None;
                continue;
            };
            resume = Some(key_cursor);
            if walk.stopped_on != SnapshotVersionGcStop::SweepCompleted {
                // A work budget, not a hold budget: the pass itself is done.
                break;
            }
            // The hold budget expired. Release, re-acquire, continue the same
            // shard from the same key — this is the whole point of paging, and
            // stopping the pass here instead would let one large shard cap what
            // a whole tick can reclaim.
        }

        cursor.shard = shard;
        cursor.resume = resume;
        drop(cursor);

        let sweep_completed = !started_mid_shard
            && shards_visited == shards_total
            && walk.stopped_on == SnapshotVersionGcStop::SweepCompleted;
        let pass = SnapshotVersionGcPass {
            floor_seq: floor,
            current_seq: self.current_seq(),
            active_leases: view.active_leases,
            versions_reclaimed: walk.versions_reclaimed,
            bytes_reclaimed: walk.bytes_reclaimed,
            chains_compacted: walk.chains_compacted,
            chains_scanned: walk.chains_scanned,
            shards_visited,
            shards_total,
            shard_guard_holds,
            sweep_completed,
            stopped_on: walk.stopped_on,
            elapsed_us: super::elapsed_us(&started),
            max_shard_hold_us: walk.max_shard_hold_us,
            resume_shard: shard,
        };
        self.snapshot_gc_counters.record_result(GcResult {
            safe_point_seq: floor,
            versions_reclaimed: usize::try_from(pass.versions_reclaimed).unwrap_or(usize::MAX),
            bytes_freed: usize::try_from(pass.bytes_reclaimed).unwrap_or(usize::MAX),
            compaction_debt: 0,
            rate_limited: !sweep_completed,
        });
        Ok(pass)
    }
}

/// Mutable state of one pass, shared across the shards it visits.
struct PassWalk {
    budget: SnapshotVersionGcBudget,
    versions_remaining: usize,
    chains_remaining: usize,
    versions_reclaimed: u64,
    bytes_reclaimed: u64,
    chains_compacted: u64,
    chains_scanned: u64,
    max_shard_hold_us: u64,
    stopped_on: SnapshotVersionGcStop,
}

impl PassWalk {
    const fn new(budget: SnapshotVersionGcBudget) -> Self {
        Self {
            budget,
            versions_remaining: budget.max_versions,
            chains_remaining: budget.max_chains_scanned,
            versions_reclaimed: 0,
            bytes_reclaimed: 0,
            chains_compacted: 0,
            chains_scanned: 0,
            max_shard_hold_us: 0,
            stopped_on: SnapshotVersionGcStop::SweepCompleted,
        }
    }

    fn pass_budget_spent(&mut self, started: &Instant) -> bool {
        if self.stopped_on != SnapshotVersionGcStop::SweepCompleted {
            return true;
        }
        if super::elapsed_us(started) >= self.budget.max_pass_us {
            self.stopped_on = SnapshotVersionGcStop::PassTimeBudget;
            return true;
        }
        false
    }

    /// Walks one shard under an already-held write guard, starting **at**
    /// `resume`.
    ///
    /// Returns `None` when it reached the end of the shard, and `Some((cf,
    /// key))` when it stopped early — naming the key it was about to process,
    /// which the next acquisition resumes *at*, not after.
    ///
    /// The inclusive cursor is load-bearing. Every early return here happens
    /// *before* the current chain is touched, so an exclusive cursor would skip
    /// it: `snapshot_version_gc_fsv` caught precisely that as 196 paged passes
    /// reclaiming 18,872 of 18,944 reclaimable versions. Re-walking the boundary
    /// key costs one chain visit and is idempotent, because reclaiming an
    /// already-clean chain frees nothing.
    ///
    /// Forward progress is guaranteed for the hold-budget return: the clock is
    /// consulted only every [`CLOCK_CHECK_CHAINS`] chains, so that return cannot
    /// fire until `CLOCK_CHECK_CHAINS - 1` chains of this acquisition have been
    /// processed. Without that floor a tight hold budget would re-acquire the
    /// same shard forever at the same key. The work-budget returns can fire
    /// immediately and do not need the floor — they end the pass rather than
    /// re-acquiring.
    fn reclaim_shard(
        &mut self,
        table: &mut RowTable,
        resume: Option<&(ColumnFamily, Vec<u8>)>,
        floor: Seq,
        acquired: &Instant,
    ) -> Option<(ColumnFamily, Vec<u8>)> {
        let resume_cf = resume.map(|(cf, _)| *cf);
        let mut since_clock_check = 0usize;
        for (cf, rows) in table.iter_mut() {
            // The outer map holds one entry per live column family — tens, not
            // millions — so skipping to the resume family linearly costs
            // nothing worth a range query.
            if resume_cf.is_some_and(|start| *cf < start) {
                continue;
            }
            let from: Option<&[u8]> = match resume {
                Some((start, key)) if *cf == *start => Some(key.as_slice()),
                _ => None,
            };
            let range = from.map_or(
                (Bound::<&[u8]>::Unbounded, Bound::<&[u8]>::Unbounded),
                |key| (Bound::Included(key), Bound::Unbounded),
            );
            for (key, versions) in rows.range_mut::<[u8], _>(range) {
                if self.chains_remaining == 0 {
                    self.stopped_on = SnapshotVersionGcStop::ChainBudget;
                    return Some((*cf, key.clone()));
                }
                if self.versions_remaining == 0 {
                    self.stopped_on = SnapshotVersionGcStop::VersionBudget;
                    return Some((*cf, key.clone()));
                }
                since_clock_check += 1;
                if since_clock_check >= CLOCK_CHECK_CHAINS {
                    since_clock_check = 0;
                    if super::elapsed_us(acquired) >= self.budget.max_shard_hold_us {
                        // Not a budget *stop*: the pass is healthy, this shard's
                        // guard has simply been held long enough. Leave
                        // `stopped_on` alone so the caller releases, re-acquires,
                        // and continues this shard from this key.
                        return Some((*cf, key.clone()));
                    }
                }
                self.chains_remaining -= 1;
                self.chains_scanned += 1;
                let (reclaimed, bytes) =
                    reclaim_chain(versions, floor, &mut self.versions_remaining);
                if reclaimed > 0 {
                    self.chains_compacted += 1;
                    self.versions_reclaimed += reclaimed as u64;
                    self.bytes_reclaimed += bytes as u64;
                }
            }
        }
        None
    }
}

impl SnapshotVersionGc for VersionedCfStore {
    /// Trait entry point, over the same paged walker as
    /// [`VersionedCfStore::reclaim_snapshot_versions_paged`].
    ///
    /// `compaction_debt` is reported as zero and `rate_limited` carries whether
    /// the sweep completed. The exact residual debt is deliberately **not**
    /// recomputed here: it is an `O(all versions)` walk under a guard covering
    /// every shard, which is precisely the wide hold this rewrite removed, and
    /// paying it on every reclaim would reintroduce the stall to report a number
    /// no caller on this path acts on. [`SnapshotVersionGc::snapshot_gc_debt`]
    /// still answers it exactly for callers that want it and accept the cost.
    fn reclaim_snapshot_versions(&self, safe_point: Seq, max_versions: usize) -> Result<GcResult> {
        let budget = SnapshotVersionGcBudget {
            max_versions,
            ..SnapshotVersionGcBudget::default()
        };
        // `Ts::default()` is 0, which expires no lease, so the `active_leases`
        // readback on this path is the raw registered count rather than the
        // live one. That is deliberate and harmless: this entry point takes the
        // floor from its caller, so nothing it decides depends on the lease
        // view. Callers that want a live lease count use
        // `AsterVault::snapshot_version_gc_memory_once`, which passes the
        // vault's own clock.
        let pass = self.reclaim_snapshot_versions_paged(safe_point, budget, Ts::default())?;
        Ok(GcResult {
            safe_point_seq: safe_point,
            versions_reclaimed: usize::try_from(pass.versions_reclaimed).unwrap_or(usize::MAX),
            bytes_freed: usize::try_from(pass.bytes_reclaimed).unwrap_or(usize::MAX),
            compaction_debt: 0,
            rate_limited: !pass.sweep_completed,
        })
    }

    fn snapshot_gc_debt(&self, safe_point: Seq) -> u64 {
        let table = self.read_rows_all(RowGuardSite::SnapshotGcDebt);
        snapshot_gc_debt_for_rows(table.iter(), safe_point)
    }
}

/// Trims one chain in place, keeping the newest version at or below `safe_point`.
///
/// In place is the point. This used to `drain(..)` into a freshly allocated
/// `Vec::with_capacity(versions.len())` and assign it back, which allocated a
/// second full-size chain *during* the pass and then kept its oversized capacity
/// afterwards — a reclaimer whose peak footprint rose with the debt it was
/// clearing. `retain` drops each removed `VersionedValue` (and therefore its
/// value buffer) as it goes and allocates nothing, and the chain's own backing
/// buffer is returned when it has become mostly empty.
fn reclaim_chain(
    versions: &mut VersionChain,
    safe_point: Seq,
    remaining: &mut usize,
) -> (usize, usize) {
    let keep_boundary = retained_boundary_index(versions, safe_point);
    let mut index = 0usize;
    let mut reclaimed = 0usize;
    let mut bytes_freed = 0usize;
    versions.retain(|version| {
        let position = index;
        index += 1;
        let can_reclaim =
            version.seq < safe_point && Some(position) != keep_boundary && *remaining > 0;
        if can_reclaim {
            *remaining -= 1;
            reclaimed += 1;
            bytes_freed += version.value.len();
        }
        !can_reclaim
    });
    // Returning the chain `Vec`'s own buffer matters at this scale: a vault with
    // millions of keys holds millions of these, and a chain trimmed from 40
    // versions to 1 otherwise keeps room for 40 forever. Only when the slack is
    // worth a reallocation, so a hot key that regrows its chain does not pay a
    // realloc per pass.
    if reclaimed > 0 && versions.capacity() >= versions.len().saturating_mul(2).max(4) {
        versions.shrink_to_fit();
    }
    (reclaimed, bytes_freed)
}

/// Reclaimable versions across whatever set of column families the caller
/// holds, rather than over one table value — the row table is sharded (#1950),
/// so "the table" is now an iterator over the shards a guard covers.
fn snapshot_gc_debt_for_rows<'a>(
    rows: impl Iterator<Item = (&'a ColumnFamily, &'a BTreeMap<Vec<u8>, VersionChain>)>,
    safe_point: Seq,
) -> u64 {
    rows.flat_map(|(_, cf_rows)| cf_rows.values())
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

fn positive_env_usize(name: &str, default: usize) -> Result<usize> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let text = raw.to_string_lossy().trim().to_owned();
    let parsed: usize = text
        .parse()
        .map_err(|error| crate::gc::gc_error(format!("invalid {name}={text:?}: {error}")))?;
    if parsed == 0 {
        return Err(crate::gc::gc_error(format!(
            "{name} is 0, which disables snapshot-version reclamation without saying so; unset it to use the default {default}"
        )));
    }
    Ok(parsed)
}

fn positive_env_u64(name: &str, default: u64) -> Result<u64> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let text = raw.to_string_lossy().trim().to_owned();
    let parsed: u64 = text
        .parse()
        .map_err(|error| crate::gc::gc_error(format!("invalid {name}={text:?}: {error}")))?;
    if parsed == 0 {
        return Err(crate::gc::gc_error(format!(
            "{name} is 0, which disables snapshot-version reclamation without saying so; unset it to use the default {default}"
        )));
    }
    Ok(parsed)
}
