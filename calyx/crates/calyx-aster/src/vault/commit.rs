use super::{AsterVault, encode, raw_commitment};
use calyx_core::{CalyxError, Clock, Result, Seq};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The WAL append is durable, but the live MVCC/router apply failed and the
/// caller must reconcile the reported sequence before retrying.
pub const CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED: &str =
    "CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED";

/// Wait or hold above which one durable-commit-lock acquisition is reported as
/// a stall (issue #1806).
///
/// The durable commit lock serializes every vault write, so a hold beyond this
/// budget is directly visible to callers as a stalled MCP request. Before this
/// telemetry existed a 469 s hold could only be inferred from third-party
/// symptoms; the warning below names the exact call site responsible.
const DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS: u128 = 1_000;

/// Absolute floor below which one commit is never reported as slow (#1936).
///
/// Every MCP tool call pays exactly one grounded-observation commit before its
/// response returns, so this is directly on the caller's critical path. This
/// constant alone is *not* the gate — see [`CommitStageObserver`] for why a
/// fixed budget could not be one.
const COMMIT_STAGE_SLOW_FLOOR_US: u64 = 3_000;

/// Multiple of the recent typical commit above which a commit is an outlier.
///
/// The gate is `total >= FLOOR && total >= FACTOR * ewma`. The second term is
/// what carries the information: it is relative to what this vault is actually
/// doing right now, so it cannot be invalidated by the workload changing or by
/// the commit path itself getting faster or slower.
const COMMIT_STAGE_OUTLIER_FACTOR: u64 = 4;

/// Commits between periodic distribution summaries.
const COMMIT_STAGE_SUMMARY_INTERVAL: u64 = 512;

/// Smoothing shift for the commit-duration EWMA (alpha = 1/8).
const COMMIT_STAGE_EWMA_SHIFT: u32 = 3;

/// Self-calibrating outlier gate and running census for durable commits
/// (issue #1946).
///
/// The gate this replaces was a bare `total_us < 3_000 { return }`. Measured on
/// the deployment host it fired on **1,105 of 1,105 commits** in one 23.6-minute
/// run — 23.9% of the daemon log by bytes — with `min = 3,020 us` against a
/// 3,000 us budget. A threshold exceeded by 100% of the population it measures
/// cannot distinguish a slow commit from an ordinary one, which is the only
/// thing it exists to do. Every one of those 1,105 lines cost bytes and carried
/// no signal.
///
/// Raising the constant would only move the threshold, and would rot again the
/// moment the workload or the commit path changed — which is precisely what had
/// happened here. So the gate is relative instead of absolute: a commit is
/// reported when it is both above an absolute floor *and* several times the
/// recent typical commit for this vault. That is self-calibrating by
/// construction; it can neither go silent nor flood, whatever the base cost is.
///
/// The ordinary cost does not stop being worth knowing just because it is
/// ordinary — losing it is how the 46% `anchor_publish` term went unnoticed. So
/// the base rate is emitted separately as a bounded periodic census rather than
/// as one line per commit.
#[derive(Debug, Default)]
pub(super) struct CommitStageObserver {
    /// EWMA of total commit duration, in microseconds. Zero until seeded.
    ewma_us: AtomicU64,
    /// Commits observed since the last summary.
    commits: AtomicU64,
    /// Sum of total commit duration since the last summary.
    total_us: AtomicU64,
    /// Largest total commit duration since the last summary.
    max_us: AtomicU64,
    /// Commits above the absolute floor since the last summary.
    over_floor: AtomicU64,
    /// Commits reported as outliers since the last summary.
    reported: AtomicU64,
    /// Per-stage sums since the last summary.
    ///
    /// The census carries the stage split, not just the total, and that is the
    /// load-bearing part of this whole structure. #1946 exists because
    /// `anchor_publish` was 46% of commit cost — a fact that is invisible in a
    /// total. Replacing a per-commit stage event with a total-only summary
    /// would have quieted the log while destroying the exact observation that
    /// found the defect, which is the trade ask 3 of that issue forbids. These
    /// sums are what let the next such term be found without re-instrumenting.
    stage_sums: [AtomicU64; STAGE_COUNT],
    /// Per-sub-stage sums of the `mvcc` term since the last summary (#1948).
    ///
    /// The census already reported `dominant_stage=mvcc` at 66% of all commit
    /// time, which named the stage and stopped there. Carrying the sub-split in
    /// the same line is what turns that from a restatement of the problem into
    /// an answer, and it does so continuously rather than only on the commits
    /// that happen to trip the outlier gate — an outlier is by construction the
    /// least representative sample of ordinary cost.
    mvcc_sums: [AtomicU64; MVCC_STAGE_COUNT],
}

/// Stage names, in the order of [`CommitStageObserver::stage_sums`].
const STAGE_NAMES: [&str; STAGE_COUNT] = [
    "admission",
    "sidecar",
    "wal",
    "anchor_publish",
    "mvcc",
    "checkpoint_stage",
];
const STAGE_COUNT: usize = 6;

use crate::mvcc::{MVCC_STAGE_COUNT, MVCC_STAGE_NAMES};

/// What one commit's duration means relative to this vault's recent behaviour.
struct CommitStageVerdict {
    report: bool,
    ewma_us: u64,
    summary: Option<CommitStageSummary>,
}

/// One bounded census of commit durations.
struct CommitStageSummary {
    commits: u64,
    mean_us: u64,
    max_us: u64,
    over_floor: u64,
    reported: u64,
    /// Total microseconds spent in each stage over the census window, in
    /// [`STAGE_NAMES`] order. Rendered as `stage=sum_us` pairs so one line
    /// carries the whole split.
    stage_sums: [u64; STAGE_COUNT],
    /// The same, for the `mvcc` term's sub-stages, in [`MVCC_STAGE_NAMES`] order.
    mvcc_sums: [u64; MVCC_STAGE_COUNT],
}

impl CommitStageSummary {
    /// `admission=12 sidecar=8 wal=4108000 anchor_publish=1790000 ...`
    fn stage_split(&self) -> String {
        STAGE_NAMES
            .iter()
            .zip(self.stage_sums.iter())
            .map(|(name, sum)| format!("{name}={sum}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `row_lock_wait=… router_lock_wait=… row_apply=… router_flush=… …`
    fn mvcc_split(&self) -> String {
        MVCC_STAGE_NAMES
            .iter()
            .zip(self.mvcc_sums.iter())
            .map(|(name, sum)| format!("{name}={sum}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The largest `mvcc` sub-stage and its share of the `mvcc` total, so the
    /// census answers "which part of mvcc" in the line rather than requiring
    /// the reader to sum nine numbers.
    fn dominant_mvcc(&self) -> (&'static str, u64) {
        let total: u64 = self.mvcc_sums.iter().sum();
        let (index, sum) = self
            .mvcc_sums
            .iter()
            .enumerate()
            .max_by_key(|(_, sum)| **sum)
            .map_or((0, &0), |(index, sum)| (index, sum));
        let share = sum.saturating_mul(100).checked_div(total).unwrap_or(0);
        (MVCC_STAGE_NAMES[index], share)
    }

    /// The largest stage as `name` and its percentage share, so the dominant
    /// term is stated outright rather than left to be derived by whoever reads
    /// the line. #1946 went unnoticed for as long as it did because nobody had
    /// summed the column.
    fn dominant(&self) -> (&'static str, u64) {
        let total: u64 = self.stage_sums.iter().sum();
        let (index, sum) = self
            .stage_sums
            .iter()
            .enumerate()
            .max_by_key(|(_, sum)| **sum)
            .map_or((0, &0), |(index, sum)| (index, sum));
        let share = sum.saturating_mul(100).checked_div(total).unwrap_or(0);
        (STAGE_NAMES[index], share)
    }
}

impl CommitStageObserver {
    fn observe(&self, stage: &CommitStageTimings, total_us: u64) -> CommitStageVerdict {
        // Seed on the first commit so the very first sample is not compared
        // against a zero baseline and reported as a 4x outlier.
        let previous = self.ewma_us.load(Ordering::Relaxed);
        let ewma_us = if previous == 0 {
            self.ewma_us.store(total_us, Ordering::Relaxed);
            total_us
        } else {
            let next = previous - (previous >> COMMIT_STAGE_EWMA_SHIFT)
                + (total_us >> COMMIT_STAGE_EWMA_SHIFT);
            self.ewma_us.store(next, Ordering::Relaxed);
            previous
        };

        let over_floor = total_us >= COMMIT_STAGE_SLOW_FLOOR_US;
        let report =
            over_floor && total_us >= ewma_us.saturating_mul(COMMIT_STAGE_OUTLIER_FACTOR).max(1);

        self.total_us.fetch_add(total_us, Ordering::Relaxed);
        self.max_us.fetch_max(total_us, Ordering::Relaxed);
        for (slot, value) in self.stage_sums.iter().zip(stage.stage_values()) {
            slot.fetch_add(value, Ordering::Relaxed);
        }
        for (slot, value) in self.mvcc_sums.iter().zip(stage.mvcc.stage_values()) {
            slot.fetch_add(value, Ordering::Relaxed);
        }
        if over_floor {
            self.over_floor.fetch_add(1, Ordering::Relaxed);
        }
        if report {
            self.reported.fetch_add(1, Ordering::Relaxed);
        }
        let commits = self.commits.fetch_add(1, Ordering::Relaxed) + 1;

        // `==`, not `>=`: exactly one observer can see the boundary sample, so
        // a concurrent non-durable commit cannot emit a duplicate census.
        let summary = (commits == COMMIT_STAGE_SUMMARY_INTERVAL).then(|| {
            let commits = self.commits.swap(0, Ordering::Relaxed);
            let total = self.total_us.swap(0, Ordering::Relaxed);
            let mut stage_sums = [0_u64; STAGE_COUNT];
            for (out, slot) in stage_sums.iter_mut().zip(self.stage_sums.iter()) {
                *out = slot.swap(0, Ordering::Relaxed);
            }
            let mut mvcc_sums = [0_u64; MVCC_STAGE_COUNT];
            for (out, slot) in mvcc_sums.iter_mut().zip(self.mvcc_sums.iter()) {
                *out = slot.swap(0, Ordering::Relaxed);
            }
            CommitStageSummary {
                commits,
                mean_us: total.checked_div(commits).unwrap_or(0),
                max_us: self.max_us.swap(0, Ordering::Relaxed),
                over_floor: self.over_floor.swap(0, Ordering::Relaxed),
                reported: self.reported.swap(0, Ordering::Relaxed),
                stage_sums,
                mvcc_sums,
            }
        });

        CommitStageVerdict {
            report,
            ewma_us,
            summary,
        }
    }
}

/// Per-stage split of one durable group commit.
///
/// Each field is the time spent in that stage alone, not a cumulative offset,
/// so the fields sum to `total_us` minus the unattributed remainder — and a
/// remainder that is not near zero is itself the finding.
#[derive(Default)]
struct CommitStageTimings {
    /// Writeability check plus memtable admission.
    admission_us: u64,
    /// Disk-pressure check and Ledger head/checkpoint sidecar derivation.
    sidecar_us: u64,
    /// Row sealing, batch encoding, the group-commit handoff and the WAL fsync.
    wal_us: u64,
    /// Ledger head/checkpoint anchor file publication.
    anchor_publish_us: u64,
    /// The in-memory MVCC row-table and router apply.
    mvcc_us: u64,
    /// `mvcc_us` broken into its own fixed points (#1948). Kept beside the
    /// total rather than replacing it so the split can be checked against the
    /// number this issue was filed about.
    mvcc: crate::mvcc::MvccCommitTimings,
    /// Staging the batch for the checkpoint publisher.
    checkpoint_stage_us: u64,
    /// Start of the stage currently being timed, as an offset from the commit
    /// start, so each `split` reports one stage rather than a running total.
    consumed_us: u64,
}

impl CommitStageTimings {
    /// This commit's stages in [`STAGE_NAMES`] order.
    fn stage_values(&self) -> [u64; STAGE_COUNT] {
        [
            self.admission_us,
            self.sidecar_us,
            self.wal_us,
            self.anchor_publish_us,
            self.mvcc_us,
            self.checkpoint_stage_us,
        ]
    }

    fn split(&mut self, started: &std::time::Instant) -> u64 {
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let stage = elapsed.saturating_sub(self.consumed_us);
        self.consumed_us = elapsed;
        stage
    }

    fn report(
        &self,
        rows: &[encode::WriteRow],
        started: &std::time::Instant,
        observer: &CommitStageObserver,
    ) {
        let total_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let verdict = observer.observe(self, total_us);
        if let Some(summary) = &verdict.summary {
            let (dominant_stage, dominant_share_pct) = summary.dominant();
            let (dominant_mvcc_stage, dominant_mvcc_share_pct) = summary.dominant_mvcc();
            tracing::info!(
                code = "CALYX_ASTER_DURABLE_COMMIT_STAGE_CENSUS",
                commits = summary.commits,
                mean_us = summary.mean_us,
                max_us = summary.max_us,
                ewma_us = verdict.ewma_us,
                over_floor = summary.over_floor,
                reported_outliers = summary.reported,
                floor_us = COMMIT_STAGE_SLOW_FLOOR_US,
                outlier_factor = COMMIT_STAGE_OUTLIER_FACTOR,
                stage_sum_us = %summary.stage_split(),
                dominant_stage,
                dominant_share_pct,
                mvcc_sum_us = %summary.mvcc_split(),
                dominant_mvcc_stage,
                dominant_mvcc_share_pct,
                "durable group commit duration census"
            );
        }
        if !verdict.report {
            return;
        }
        let row_count = rows.len();
        let attributed = self.admission_us
            + self.sidecar_us
            + self.wal_us
            + self.anchor_publish_us
            + self.mvcc_us
            + self.checkpoint_stage_us;
        // The row count alone made the batch the obvious lever and gave no way
        // to pull it: 29 rows on a tool call that was rejected at parameter
        // validation is a finding only once you can say *which* 29 (#1936 ask
        // 1). Attribute the batch by column family, collapsing the per-slot
        // families into one `slot` bucket because their identity is the panel's
        // and the question here is how many vectors one commit carries.
        let mut per_cf: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for row in rows {
            let name = match row.cf {
                crate::cf::ColumnFamily::Slot { .. } => "slot".to_owned(),
                ref other => other.name(),
            };
            *per_cf.entry(name).or_default() += 1;
        }
        let row_families = per_cf
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            code = "CALYX_ASTER_DURABLE_COMMIT_STAGE_TIMINGS",
            row_count,
            row_families = %row_families,
            total_us,
            admission_us = self.admission_us,
            sidecar_us = self.sidecar_us,
            wal_us = self.wal_us,
            anchor_publish_us = self.anchor_publish_us,
            mvcc_us = self.mvcc_us,
            // #1948: `mvcc_us` alone was 91-99% of these commits and could not
            // say why. Each term below is one candidate cause, and they carry
            // different fixes: lock waits mean a concurrent vault operation is
            // blocking the commit, a non-zero flush means this commit
            // synchronously wrote an SST under both global write locks, and
            // row_apply is the key/value copying that must NOT be optimised on
            // the strength of being the visible allocation.
            // Emitted even though they are usually small: `materialize` is a
            // full copy of every key and value in the batch and measured 13 ms
            // on a dense-vector commit, which was invisible while it was only
            // ever folded into the remainder.
            mvcc_materialize_us = self.mvcc.materialize_us,
            mvcc_watermark_us = self.mvcc.watermark_us,
            mvcc_row_lock_wait_us = self.mvcc.row_lock_wait_us,
            mvcc_router_lock_wait_us = self.mvcc.router_lock_wait_us,
            mvcc_panel_attribution_us = self.mvcc.panel_attribution_us,
            mvcc_row_apply_us = self.mvcc.row_apply_us,
            mvcc_router_apply_us = self.mvcc.router_apply_us,
            mvcc_router_ensure_cf_us = self.mvcc.put.ensure_cf_us,
            mvcc_router_seal_us = self.mvcc.put.seal_us,
            mvcc_router_seals = self.mvcc.put.seals,
            // #1949: the SST write, measured with both write locks released.
            // `mvcc_locked_us` is what every other thread actually waits for,
            // and the gap between it and `mvcc_us` is the change's whole effect.
            mvcc_sst_write_unlocked_us = self.mvcc.sst_write_us,
            mvcc_locked_us = self.mvcc.locked_us(),
            // #1955 on the write side: `mvcc_locked_us` is wall clock, so a
            // committer descheduled while holding both write locks looks
            // identical to one doing heavy work — and it blocks every other
            // writer either way. `mvcc_locked_starved` says which.
            mvcc_locked_cpu_us = self.mvcc.locked_cpu_us,
            mvcc_locked_starved = self.mvcc.locked_starved(),
            mvcc_unattributed_us = self.mvcc.unattributed_us(),
            // Time inside the `mvcc` stage that the timed inner call did not
            // observe: the owned row batch and the sealed memtables are dropped
            // as the frame unwinds, after the last measurement. On a
            // dense-vector commit that is ~10 MB of deallocation and measured
            // 12-32 ms, so it is named rather than left as a silent difference
            // between `mvcc_us` and the parts. It runs with both write locks
            // already released.
            mvcc_teardown_us = self.mvcc_us.saturating_sub(self.mvcc.total_us),
            checkpoint_stage_us = self.checkpoint_stage_us,
            unattributed_us = total_us.saturating_sub(attributed),
            floor_us = COMMIT_STAGE_SLOW_FLOOR_US,
            // The baseline this commit was judged against, so the line carries
            // its own evidence of being an outlier rather than asserting it.
            typical_us = verdict.ewma_us,
            outlier_factor = COMMIT_STAGE_OUTLIER_FACTOR,
            "durable group commit is an outlier against this vault's recent commit duration"
        );
    }
}

/// Budget above which one commit's derived-projection publish is attributed.
///
/// The in-place publish measures 0.130-0.208 ms on this host's vault volume
/// (#1947), so a millisecond is roughly 5x the expected cost: high enough that
/// an ordinary publish stays silent, low enough that the 15-20 ms outliers
/// #1947 ask 2 could not explain cannot hide under it.
const LEDGER_PROJECTION_PUBLISH_SLOW_BUDGET_US: u64 = 1_000;

/// Attributes one commit's derived-projection publish when it exceeds budget
/// (#1947 ask 2).
///
/// #1947 measured a 147-byte publish costing 15-20 ms on large-batch commits
/// and could not reproduce it from outside the daemon, so the cost had no
/// owner. The publish now performs no namespace metadata work at all — the
/// file is pre-allocated and the handle is held — which makes these splits
/// decisive rather than merely informative: if the outlier survives, it is
/// either the open or the write, and `reopened` says whether the handle was
/// being reacquired. An outlier that is in neither is not in this code, and
/// that is a finding about the commit around it rather than about the publish.
fn report_projection_publish(
    head: Option<crate::ledger_projection::PublishTimings>,
    checkpoint: Option<crate::ledger_projection::PublishTimings>,
    stage_us: u64,
    row_count: usize,
) {
    if stage_us < LEDGER_PROJECTION_PUBLISH_SLOW_BUDGET_US {
        return;
    }
    let attributed = head.map_or(0, |timings| {
        timings.open_us + timings.write_us + timings.sync_us
    }) + checkpoint.map_or(0, |timings| {
        timings.open_us + timings.write_us + timings.sync_us
    });
    tracing::info!(
        code = "CALYX_ASTER_LEDGER_PROJECTION_PUBLISH_SLOW",
        row_count,
        stage_us,
        budget_us = LEDGER_PROJECTION_PUBLISH_SLOW_BUDGET_US,
        head_published = head.is_some(),
        head_open_us = head.map_or(0, |timings| timings.open_us),
        head_write_us = head.map_or(0, |timings| timings.write_us),
        head_reopened = head.is_some_and(|timings| timings.reopened),
        checkpoint_published = checkpoint.is_some(),
        checkpoint_open_us = checkpoint.map_or(0, |timings| timings.open_us),
        checkpoint_write_us = checkpoint.map_or(0, |timings| timings.write_us),
        checkpoint_reopened = checkpoint.is_some_and(|timings| timings.reopened),
        // A large remainder is the whole point of publishing this line: it
        // says the stage's cost is not in the syscalls this stage performs.
        unattributed_us = stage_us.saturating_sub(attributed),
        "derived Ledger projection publish exceeded its budget"
    );
}

/// Waiter accounting for the durable commit lock. Decrements on every exit
/// path, including the error paths that abandon the acquisition.
struct CommitLockWaiterTicket<'a> {
    waiters: &'a AtomicUsize,
    released: bool,
}

impl<'a> CommitLockWaiterTicket<'a> {
    fn enqueue(waiters: &'a AtomicUsize) -> Self {
        waiters.fetch_add(1, Ordering::AcqRel);
        Self {
            waiters,
            released: false,
        }
    }

    fn admitted(&mut self) {
        if !self.released {
            self.waiters.fetch_sub(1, Ordering::AcqRel);
            self.released = true;
        }
    }
}

impl Drop for CommitLockWaiterTicket<'_> {
    fn drop(&mut self) {
        self.admitted();
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Threads currently queued for the durable commit lock.
    pub(crate) fn durable_commit_lock_waiters(&self) -> usize {
        self.commit_lock_waiters.load(Ordering::Acquire)
    }

    /// The reason this vault is closing, once a close has been declared (#2100).
    #[must_use]
    pub fn close_intent(&self) -> Option<&'static str> {
        self.close_intent.get().copied()
    }

    /// Declares that this vault is closing, fencing new maintenance work.
    ///
    /// Returns the reason actually in force: a second declaration does not
    /// overwrite the first, because the first close is the one whose evidence
    /// the exit record carries.
    ///
    /// This is the root fix for #2100 ask 2. Before it, a commanded close could
    /// queue behind arbitrary fan-out work that had been admitted *after* the
    /// close was requested; the close had no way to say "no more maintenance",
    /// only to wait its turn like any other pass.
    pub fn declare_close_intent(&self, reason: &'static str) -> &'static str {
        let in_force = *self.close_intent.get_or_init(|| reason);
        tracing::info!(
            code = "CALYX_ASTER_VAULT_CLOSE_INTENT_DECLARED",
            reason,
            in_force,
            commit_lock_waiters = self.durable_commit_lock_waiters(),
            "fenced new maintenance admissions of the native compaction lock and of the durable \
             commit lock; the close no longer queues behind work admitted after it was commanded"
        );
        in_force
    }

    /// Fails closed when a close has been declared, naming it.
    pub(crate) fn ensure_not_closing(&self, operation: &'static str) -> Result<()> {
        let Some(reason) = self.close_intent() else {
            return Ok(());
        };
        Err(CalyxError {
            code: "CALYX_ASTER_VAULT_CLOSING",
            message: format!(
                "refusing to admit maintenance operation {operation} because this vault is closing (close_reason={reason})"
            ),
            remediation: "this is close fencing, not a storage fault: the vault was commanded to \
                          close and refuses new maintenance work so the close cannot queue behind \
                          it. Reopen the vault to resume maintenance",
        })
    }

    /// [`Self::with_durable_commit_lock`] for **maintenance** lanes only.
    ///
    /// The distinction this draws is the one #2100 needed and the vault did not
    /// have: a commit is work the caller is waiting on and must still complete
    /// while the daemon drains, whereas a background maintenance lane taking the
    /// one lock that serialises every vault write is exactly what a commanded
    /// close must be able to stop. Callers that are on the close's own path
    /// (checkpoint drain, final flush, the close fan-out preparation) keep using
    /// [`Self::with_durable_commit_lock`] — fencing those would fence the close
    /// against itself.
    #[track_caller]
    pub(crate) fn with_durable_commit_lock_maintenance<T>(
        &self,
        operation: &'static str,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.ensure_not_closing(operation)?;
        self.with_durable_commit_lock(f)
    }

    /// Runs `f` under the process + cross-process durable commit boundary,
    /// recording how long the caller queued and how long it held the lock.
    ///
    /// `#[track_caller]` is what makes the stall telemetry actionable: the
    /// warning below names the exact call site holding the only lock that
    /// serializes vault writes, so a future regression is attributable without
    /// re-deriving it from unrelated request timeouts (issue #1806).
    #[track_caller]
    pub(crate) fn with_durable_commit_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let caller = std::panic::Location::caller();
        let wait_started = std::time::Instant::now();
        let mut ticket = CommitLockWaiterTicket::enqueue(&self.commit_lock_waiters);
        let _process_guard = self
            .commit_lock
            .lock()
            .map_err(|_| CalyxError::backpressure("vault commit lock poisoned"))?;
        let _commit_guard = match &self.durable {
            Some(durable) => Some(crate::file_lock::FileLockGuard::acquire(
                &durable.commit_lock_path(),
            )?),
            None => None,
        };
        ticket.admitted();
        let wait_ms = wait_started.elapsed().as_millis();
        let hold_started = std::time::Instant::now();
        let outcome = self.durable_commit_lock_body(f);
        let hold_ms = hold_started.elapsed().as_millis();
        if hold_ms > DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS
            || wait_ms > DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS
        {
            tracing::warn!(
                code = "CALYX_ASTER_DURABLE_COMMIT_LOCK_SLOW",
                call_site = %caller,
                wait_ms,
                hold_ms,
                slow_budget_ms = DURABLE_COMMIT_LOCK_SLOW_BUDGET_MS,
                queued_waiters = self.commit_lock_waiters.load(Ordering::Acquire),
                ok = outcome.is_ok(),
                "durable commit lock wait or hold exceeded the stall budget; every vault write \
                 (including the MCP initialize activity write) is serialized behind this lock"
            );
        }
        outcome
    }

    fn durable_commit_lock_body<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(durable) = &self.durable else {
            if self
                .ledger_state_reconciliation_required
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(CalyxError::ledger_group_commit_failed(
                    "Ledger state reconciliation is required but the vault has no durable physical source of truth",
                ));
            }
            return f();
        };
        if self
            .ledger_state_reconciliation_required
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.reconcile_ledger_state_from_durable_locked()?;
            self.ledger_state_reconciliation_required
                .store(false, std::sync::atomic::Ordering::Release);
            tracing::info!(
                code = "CALYX_ASTER_LEDGER_STATE_RECONCILIATION_CLEARED",
                "repaired Ledger sidecars and the configured persistent hook from physical truth before admitting the next durable operation"
            );
        }
        let durable_tip = durable.durable_tip_seq()?;
        let live_tip = self.latest_seq();
        match durable_tip.cmp(&live_tip) {
            std::cmp::Ordering::Greater => {
                self.refresh_from_durable()?;
                let refreshed_live_tip = self.latest_seq();
                if refreshed_live_tip != durable_tip {
                    return Err(durable_live_sequence_divergence(
                        durable_tip,
                        refreshed_live_tip,
                        "foreign WAL refresh did not converge the live MVCC sequence",
                    ));
                }
            }
            std::cmp::Ordering::Less => {
                return Err(durable_live_sequence_divergence(
                    durable_tip,
                    live_tip,
                    "live MVCC sequence is ahead of durable WAL truth",
                ));
            }
            std::cmp::Ordering::Equal => {}
        }
        f()
    }

    pub(crate) fn with_recurrence_write_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self
            .recurrence_write_lock
            .lock()
            .map_err(|_| CalyxError::backpressure("recurrence write lock poisoned"))?;
        let _file_guard = self
            .durable
            .as_ref()
            .map(|durable| {
                crate::file_lock::FileLockGuard::acquire(&durable.recurrence_lock_path())
            })
            .transpose()?;
        // Recurrence operations are read/modify/write transactions. Acquire
        // the normal process + cross-process commit boundary before refreshing
        // or invoking the operation so its derived decision and commit share
        // one authoritative view. Callbacks must use `*_locked` commit helpers
        // and must not recursively acquire the non-reentrant commit lock.
        self.with_durable_commit_lock(f)
    }

    fn refresh_from_durable(&self) -> Result<()> {
        let Some(durable) = &self.durable else {
            return Ok(());
        };
        let current = self.latest_seq();
        let recovered = durable.recover_current_batches_under_commit_lock()?;
        self.reconcile_ledger_state_from_recovery_locked(&recovered)?;
        self.replace_retention_horizon(recovered.retention_horizon.clone())?;
        self.rows
            .advance_derived_content_seq_to_at_least(recovered.derived_content_floor_seq);
        durable.advance_derived_content_watermark_to_at_least(recovered.derived_content_floor_seq);
        self.rows
            .advance_panel_content_seqs_to_at_least(&recovered.panel_content_floor_seqs)?;
        durable
            .advance_panel_content_watermarks_to_at_least(&recovered.panel_content_floor_seqs)?;
        // WAL-tail batches from a foreign writer have no durable-batch SSTs
        // yet; stage them here so this handle's next checkpoint flush cannot
        // advance the manifest past them if that writer dies (issue #1132).
        durable.stage_recovered_wal_batches(
            recovered
                .batches
                .iter()
                .filter(|batch| batch.seq > recovered.wal_replay_floor_seq)
                .map(|batch| (batch.seq, batch.rows.clone()))
                .collect(),
        )?;
        self.rows.restore_recovered_batches_and_advance(
            recovered
                .batches
                .iter()
                .filter(|batch| batch.seq > current)
                .map(|batch| {
                    (
                        batch.seq,
                        batch
                            .rows
                            .iter()
                            .map(|row| (row.cf, row.key.clone(), row.value.clone())),
                    )
                }),
            recovered.last_recovered_seq,
            recovered.wal_replay_floor_seq,
            recovered.migrate_derived_content_model,
        )?;
        if recovered.migrate_derived_content_model {
            self.rows.migrate_panel_content_seqs_to_at_least(
                recovered.derived_content_floor_seq,
                recovered.active_panel_version,
            )?;
        }
        durable.advance_panel_content_watermarks_to_at_least(
            &self.rows.panel_content_seqs_snapshot()?,
        )?;
        Ok(())
    }

    pub(super) fn commit_rows(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        self.with_durable_commit_lock(|| self.commit_rows_locked(rows))
    }

    pub(crate) fn commit_rows_locked(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if rows
            .iter()
            .any(|row| row.cf == crate::cf::ColumnFamily::TimeIndex)
        {
            return Err(CalyxError::aster_corrupt_shard(
                "time_index is a reserved derived column family; caller-supplied rows are forbidden because they can forge or corrupt the sole time-to-sequence mapping",
            ));
        }
        self.commit_rows_locked_inner(rows)
    }

    /// Trusted erasure path for tombstoning existing derived TimeIndex rows.
    /// It deliberately accepts only the MVCC tombstone value, never a forged
    /// live time-to-sequence mapping.
    pub(crate) fn commit_erasure_rows_locked(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if let Some(row) = rows.iter().find(|row| {
            row.cf == crate::cf::ColumnFamily::TimeIndex
                && !crate::mvcc::is_tombstone_value(&row.value)
        }) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "trusted erasure attempted a live time_index write: key_len={} value_len={}",
                row.key.len(),
                row.value.len()
            )));
        }
        self.commit_rows_locked_inner(rows)
    }

    fn commit_rows_locked_inner(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        if rows.is_empty() {
            // Empty commit: do not advance the seq or stamp a time-index entry.
            return Ok(self.latest_seq());
        }
        // Time-travel (PH72 T04): stamp this group-commit with one time-index
        // entry in the SAME batch as the data, so the (millis -> seqno) mapping
        // is atomic with the write — a crash can never leave a write without its
        // time mapping (A15). We hold the durable commit lock here, so the next
        // allocated seq is exactly current_seq()+1; we assert that against the
        // committed seq below and fail loud on any divergence (never silent).
        let predicted = self.rows.current_seq().saturating_add(1);
        let (cf, key, value) = crate::timetravel::entry_row(self.clock.now(), predicted);
        let mut all_rows = rows.to_vec();
        all_rows.push(encode::WriteRow { cf, key, value });
        if !rows
            .iter()
            .any(|row| row.cf == crate::cf::ColumnFamily::Ledger)
        {
            all_rows.push(raw_commitment::commitment_row(predicted, &all_rows)?);
        }
        let committed = match self.commit_prepared_rows(&all_rows) {
            Ok(committed) => committed,
            Err(error) => {
                // A non-durable vault can publish the authoritative MVCC row
                // table and then fail its router projection. Under this
                // exclusive commit boundary, observing exactly the predicted
                // new MVCC sequence proves this operation applied. Preserve a
                // lower layer's durable marker when one already exists.
                if self.rows.current_seq() == predicted {
                    let _ = self.post_commit_error_seq.compare_exchange(
                        0,
                        predicted,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    );
                }
                return Err(error);
            }
        };
        if committed != predicted {
            // The batch crossed the irreversible commit boundary even though
            // the time-index invariant failed. Preserve that exact sequence
            // for guarded callers just like every other post-commit failure.
            self.post_commit_error_seq
                .store(committed, std::sync::atomic::Ordering::Release);
            return Err(CalyxError::aster_corrupt_shard(format!(
                "time-index seqno prediction {predicted} diverged from committed seq {committed}"
            )));
        }
        Ok(committed)
    }

    fn commit_prepared_rows(&self, rows: &[encode::WriteRow]) -> Result<Seq> {
        // #1936: a durable group commit on the deployment host measures ~6 ms
        // while its own fsync costs 0.26 ms — ~27x its durable I/O — and the
        // existing timing splits stop at the commit boundary, so the 6 ms was
        // one opaque number. These spans are the only fixed points inside it.
        // Reported only above a budget, so a healthy commit stays silent.
        let started = std::time::Instant::now();
        let mut stage = CommitStageTimings::default();
        if !rows.is_empty() {
            self.ensure_writeable("commit")?;
        }
        self.rows.ensure_memtable_admission(
            rows.iter()
                .map(|row| (row.cf, row.key.as_slice(), row.value.as_slice())),
        )?;
        stage.admission_us = stage.split(&started);
        let Some(durable) = &self.durable else {
            let seq = self.commit_rows_to_mvcc(rows, &mut stage.mvcc);
            stage.mvcc_us = stage.split(&started);
            stage.report(rows, &started, &self.commit_stage_observer);
            return seq;
        };

        durable.ensure_disk_write_allowed(self.rows.resource_counters())?;
        // Validate and derive Ledger sidecars before the irreversible WAL
        // append. After append succeeds, every failure is a committed-outcome
        // reconciliation event, never an ordinary retryable write error.
        let head_anchor = crate::ledger_head::newest_anchor_from_rows(rows)?;
        let checkpoint_anchor = crate::ledger_head::newest_checkpoint_from_rows(rows)?;
        stage.sidecar_us = stage.split(&started);
        // Seals, encodes and hands the batch to the group-commit thread, then
        // blocks on its reply. This span therefore covers the WAL fsync AND two
        // cross-thread handoffs, which on a hybrid CPU with parked E-cores is
        // not the same cost as the fsync alone.
        let durable_seq = durable.append_batch(rows)?;
        stage.wal_us = stage.split(&started);

        let mut head_publish = None;
        let mut checkpoint_publish = None;
        let publish = (|| -> Result<Seq> {
            if head_anchor.is_some() || checkpoint_anchor.is_some() {
                let mut guard = self.ledger_projections.lock().map_err(|_| {
                    CalyxError::backpressure("Ledger projection writer mutex poisoned")
                })?;
                let projections = guard.get_or_insert_with(|| {
                    crate::ledger_head::LedgerProjections::new(durable.root())
                });
                if let Some(anchor) = &head_anchor {
                    head_publish = Some(projections.publish_head(anchor)?);
                }
                if let Some(anchor) = &checkpoint_anchor {
                    checkpoint_publish = projections.publish_checkpoint(anchor)?;
                }
            }
            stage.anchor_publish_us = stage.split(&started);
            let mvcc_seq = self.commit_rows_to_mvcc(rows, &mut stage.mvcc)?;
            stage.mvcc_us = stage.split(&started);
            if mvcc_seq != durable_seq {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "durable WAL seq {durable_seq} diverged from MVCC seq {mvcc_seq}"
                )));
            }
            durable.stage_checkpoint_batch(durable_seq, rows)?;
            stage.checkpoint_stage_us = stage.split(&started);
            Ok(mvcc_seq)
        })();
        stage.report(rows, &started, &self.commit_stage_observer);
        report_projection_publish(
            head_publish,
            checkpoint_publish,
            stage.anchor_publish_us,
            rows.len(),
        );

        match publish {
            Ok(seq) => Ok(seq),
            Err(post_wal_error) => {
                // Preserve the exact irreversible sequence as typed state for
                // the outer guarded-write API. It consumes this marker before
                // releasing the durable commit lock, so another writer cannot
                // overwrite or misattribute the outcome.
                self.post_commit_error_seq
                    .store(durable_seq, std::sync::atomic::Ordering::Release);
                let restore = self.restore_committed_rows(durable_seq, rows);
                let head_repair = head_anchor.as_ref().map_or(Ok(()), |anchor| {
                    crate::ledger_head::write_head_anchor(durable.root(), anchor)
                });
                let checkpoint_anchor_repair =
                    checkpoint_anchor.as_ref().map_or(Ok(()), |anchor| {
                        crate::ledger_head::write_checkpoint_anchor(durable.root(), anchor)
                    });
                let panel_watermarks = self.rows.panel_content_seqs_snapshot();
                let checkpoint = panel_watermarks.and_then(|panel_watermarks| {
                    durable.advance_panel_content_watermarks_to_at_least(&panel_watermarks)?;
                    durable.checkpoint_committed_batch_with_pending(durable_seq, rows)
                });
                if rows
                    .iter()
                    .any(|row| row.cf == crate::cf::ColumnFamily::Ledger)
                {
                    self.ledger_state_reconciliation_required
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                Err(post_wal_commit_error(
                    durable_seq,
                    &post_wal_error,
                    &restore,
                    &head_repair,
                    &checkpoint_anchor_repair,
                    &checkpoint,
                ))
            }
        }
    }

    fn commit_rows_to_mvcc(
        &self,
        rows: &[encode::WriteRow],
        timings: &mut crate::mvcc::MvccCommitTimings,
    ) -> Result<Seq> {
        self.rows.commit_batch_timed(
            rows.iter()
                .map(|row| (row.cf, row.key.clone(), row.value.clone())),
            timings,
        )
    }

    fn restore_committed_rows(&self, seq: Seq, rows: &[encode::WriteRow]) -> Result<()> {
        self.rows.restore_batches_and_advance(
            [(
                seq,
                rows.iter()
                    .map(|row| (row.cf, row.key.clone(), row.value.clone())),
            )],
            seq,
        )?;
        Ok(())
    }
}

fn post_wal_commit_error(
    durable_seq: Seq,
    post_wal_error: &CalyxError,
    restore: &Result<()>,
    head_repair: &Result<()>,
    checkpoint_anchor_repair: &Result<()>,
    checkpoint: &Result<()>,
) -> CalyxError {
    CalyxError {
        code: CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
        message: format!(
            "WAL commit is durable but post-WAL publication failed; wal_seq={durable_seq} \
             post_wal=error[{}]: {} restore={} head_repair={} checkpoint_anchor_repair={} checkpoint={}",
            post_wal_error.code,
            post_wal_error.message,
            reconciliation_outcome(restore),
            reconciliation_outcome(head_repair),
            reconciliation_outcome(checkpoint_anchor_repair),
            reconciliation_outcome(checkpoint),
        ),
        remediation: "treat wal_seq as durably committed; reconcile by idempotency/readback before retrying, and allow the next durable boundary to rebuild any latched Ledger sidecar/hook state from physical truth",
    }
}

fn durable_live_sequence_divergence(
    durable_tip: Seq,
    live_tip: Seq,
    context: &'static str,
) -> CalyxError {
    CalyxError {
        code: CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
        message: format!(
            "{context}; durable_wal_tip={durable_tip} live_mvcc_tip={live_tip}; refusing to admit another durable write"
        ),
        remediation: "close and reopen this vault from durable physical truth, inspect the preceding post-WAL failure, and do not retry the rejected logical operation without idempotency/readback",
    }
}

fn reconciliation_outcome(result: &Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(error) => format!("error[{}]: {}", error.code, error.message),
    }
}
