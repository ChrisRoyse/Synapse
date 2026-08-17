//! Unattended maintenance of the vault's **derived** layers.
//!
//! Synapse's recall and intelligence capabilities are not served by the Base
//! rows directly — they are served by layers derived from them: the persisted
//! search generation (dense `DiskANN` + sparse BM25 lanes) and the per-record lens
//! measurements the association surfaces read. Both were, until this module,
//! maintained by nothing.
//!
//! Two production findings drove it:
//!
//! * **Issue #1891.** The persisted search generation had been built exactly
//!   once, by hand, through a break-glass ceremony — then silently allowed to
//!   expire. It sat at seq 55,908 while the vault ran on to 67,346, i.e. 13,685
//!   sequences past the 8,192-key bounded reconciliation limit, so every fused
//!   recall query failed closed and nothing brought it back. An always-on
//!   capability whose only repair is a manual ceremony is off by default,
//!   forever.
//! * **Issue #1894.** `abundance` computed `blind_spot_records = 1740` on a
//!   panel of 1,745 constellations — the exact alarm that says the association
//!   layer is measuring nothing — and no surface raised it. It was visible only
//!   to someone who already knew to run an intelligence pass by hand.
//!
//! Both are fixed the same way: measure off the request path on a periodic tick,
//! publish the result, and let `health` read the published result. That keeps
//! `health` cheap (it reads a struct, never a corpus) while making the state it
//! reports a real measurement rather than an assumption.
//!
//! Every pass runs through [`crate::maintenance::run_admitted_maintenance`], so
//! it executes on the dedicated blocking pool under an admission permit and can
//! never park a runtime worker that is serving MCP requests.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use synapse_calyx::{
    SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS, SEARCH_GENERATION_REFRESH_DELTA_KEYS,
    SynapseCalyxLensCoverageStatus, SynapseCalyxPersistedDriftFinding,
    SynapseCalyxPersistedNoveltyFinding, SynapseCalyxPersistedRegionFinding,
    SynapseCalyxSearchGenerationStatus, hot_context,
};

use crate::Db;
use crate::cf;
use crate::constellations::{
    GraphPositionKind, SYN_GRAPHPOS_APP_PANEL_VERSION, SYN_PATH_HIERARCHY_PANEL_VERSION,
};
use crate::panel_coverage::StrandedAnchorIdentity;
use synapse_core::types::{AgentEventKind, AgentEventRecord, TimelineKind, TimelineRecord};

/// How often the derived-state maintainer runs.
///
/// Chosen against the freshness budget it defends, not for its own sake: the
/// generation is refreshed once its changed-key delta passes
/// [`SEARCH_GENERATION_REFRESH_DELTA_KEYS`], which is half the limit at which
/// queries fail, so the tick only has to be frequent enough that a burst of
/// writes cannot cross the whole remaining half budget between two ticks. Five minutes matches the storage GC
/// cadence and keeps the two heavy periodic passes on the same rhythm.
pub const DERIVED_STATE_INTERVAL: std::time::Duration = std::time::Duration::from_mins(5);

/// Idle/admission window between association-only recovery continuations.
///
/// A durable CDC bootstrap can span many bounded Loom calls. Waiting the full
/// five-minute inspection cadence between those calls makes recovery time a
/// function of the scheduler rather than of the measured work. Five seconds
/// leaves a real foreground-admission window after the previous blocking pass
/// releases every corpus allocation; it is never used after an error or when
/// no proven backlog remains.
pub const ASSOCIATION_BACKLOG_CONTINUATION_DELAY: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Records hydrated per panel when measuring lens coverage.
///
/// This is a **sample**, and it is reported as one. Lens coverage is a property
/// of how records are measured at ingest, not of any individual row, so a few
/// hundred records answer "is this panel carrying its lens layer?" exactly as
/// well as fifty thousand would — and a bounded sample is what makes the pass
/// affordable on a five-minute tick.
pub const LENS_COVERAGE_SAMPLE_RECORDS: usize = 256;

/// Source rows re-measured per backfill page (#1927 ask 2).
///
/// The `temporal_backfill` route caps a page at 1,000 rows, so this is that cap
/// rather than an independent choice; pages smaller than the cap only add
/// per-call overhead to the same total work.
pub const PANEL_BACKFILL_PAGE_ROWS: usize = 1_000;

/// Lifecycle tasks claimed per five-minute maintenance tick.
///
/// Each task fully decodes and re-measures an authoritative row, so the bound
/// controls both IO and CPU independently of the older temporal-metadata page size.
pub const LIFECYCLE_BACKFILL_BATCH_ROWS: usize = 64;

/// Wall-clock the maintainer will spend driving backfill pages in one tick.
///
/// Chosen from the measured rate, not picked: driving this by hand on #1927 took
/// **over 10 minutes for 20 pages** and still did not finish 23k rows, i.e.
/// roughly 30 s per 1,000-row page. A 60 s budget is therefore about two pages a
/// tick — enough that a 23k-row corpus converges in roughly an hour of unattended
/// running, and small enough that the pass never occupies a meaningful share of
/// the five-minute interval it shares with the search-generation rebuild and the
/// lens-coverage sample. It runs on the dedicated blocking maintenance pool under
/// an admission permit, so it can never park a runtime worker serving MCP
/// requests.
pub const PANEL_BACKFILL_TICK_BUDGET: std::time::Duration = std::time::Duration::from_mins(1);

/// Wall clock guaranteed to **every** owed coverage target on every tick
/// (#2061 ask 3).
///
/// The tick budget above says how much sweeping happens; this says that it is
/// shared. Without it, the most-owed target's sweep loop read the *tick's* clock
/// rather than its own share of it, so target one paged until the whole budget
/// was gone and the driver's loop over the remaining targets exited on its first
/// check. The deployed daemon reported `targets_owed=4 targets_attempted=1` on
/// every pass of two consecutive generations while `syn-process-v1` held 991
/// uncovered rows and `syn-agent-event-v1` held 32,494, both *exactly* flat: not
/// blocked by any fault, simply never selected. The rotation existed; the clock
/// it divided did not.
///
/// A slice is a floor and not a quota. The head of the queue still gets the
/// whole remainder — everyone else's floor is reserved from it, not taken from
/// it evenly — so the largest backlog keeps the bulk of the tick and the
/// smallest still advances. The sweep checks its deadline *before* each page, so
/// any positive slice buys at least one page and no target can be attempted for
/// zero work.
pub const PANEL_BACKFILL_TARGET_MIN_SLICE: std::time::Duration = std::time::Duration::from_secs(10);

/// Hard stop for the whole panel-backfill phase.
///
/// A page is atomic and can overrun the slice that admitted it, so the sum of
/// the floors plus one overrunning page can exceed [`PANEL_BACKFILL_TICK_BUDGET`].
/// That is intended: finishing the rotation matters more than the soft budget,
/// and the five-minute tick has the headroom. This is the bound that keeps
/// "finishing the rotation" from becoming unbounded if the owed-target set ever
/// grows large — past it, the remaining targets are **named** as unswept with
/// their reason rather than silently dropped.
pub const PANEL_BACKFILL_TICK_HARD_CEILING: std::time::Duration = std::time::Duration::from_mins(3);

/// Exact stranded anchor identities one tick will attempt to re-anchor (#1984).
///
/// A **work** bound, not a time bound, and that is the whole correction. The
/// anchor-debt repair used to be a wall-clock-budgeted rescan of the source CF:
/// on the deployed daemon it spent 60,089 ms over 44 pages, inserted 0 rows, and
/// left all five stranded panels exactly where they were, tick after tick.
/// Repair work proportional to the corpus cannot converge on a debt that is four
/// orders of magnitude smaller.
///
/// Sized against the measured debt: the whole vault carried 2,879 stranded
/// anchors across five panels, so this drains it in three ticks — roughly
/// fifteen minutes of unattended running — while keeping any single tick's write
/// volume bounded and predictable.
pub const PANEL_ANCHOR_DEBT_TICK_IDENTITIES: usize = 1_000;

/// Wall-clock backstop for the identity-driven anchor-debt phase.
///
/// Not the thing that bounds the work — [`PANEL_ANCHOR_DEBT_TICK_IDENTITIES`] is
/// — but the guarantee that one pathological row cannot take the whole tick and
/// starve the coverage sweep that shares it (#2061).
pub const PANEL_ANCHOR_DEBT_TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Per-identity cost above which the exact-identity repair primitive is not
/// amortizing its declared-lineage read, reported as a named fault (#1984).
///
/// An exact-identity repair is a point read of one source row, one constellation
/// put, and one anchor carry: single-digit milliseconds once the declared anchor
/// lineage is read once per sweep rather than once per row. A ratio far above
/// this ceiling means the lineage is being rebuilt per identity, which turns a
/// debt-proportional repair back into a corpus-proportional one. It is measured
/// and published every tick rather than assumed.
pub const PANEL_ANCHOR_DEBT_IDENTITY_MAX_MS: u64 = 250;

/// Identities a tick must have attempted before its per-identity cost is
/// compared against [`PANEL_ANCHOR_DEBT_IDENTITY_MAX_MS`] (#2080 defect 3).
///
/// A ceiling on *marginal* cost is meaningless over a sample of one. The
/// deployed daemon published a "150-500x regression" that was entirely this: the
/// same fixed per-panel cost, divided by two attempts instead of a thousand as
/// the debt drained. The advisory now needs a sample before it will speak, and
/// the fixed cost it used to absorb is measured and published separately.
pub const PANEL_ANCHOR_DEBT_COST_ADVISORY_MIN_SAMPLE: u64 = 8;

/// Exact identities one panel may hold in durable quarantine (#1984, #2061).
///
/// Quarantine is per identity, never per panel and never per tick. Three
/// unrepairable outcome anchors latched the whole maintainer and denied 208,496
/// rows of coverage backfill for the life of a process; the fault was the scope
/// of the refusal, not the refusal. Past this cap the panel's debt is
/// systematically unrepairable rather than pointwise, which is a different
/// finding and is reported as one.
pub const PANEL_ANCHOR_DEBT_QUARANTINE_CAP: usize = 256;

/// Failed exact repair attempts before an identity is quarantined.
///
/// More than one because a write shed under disk pressure is not evidence that a
/// row cannot be repaired. A carry that succeeds but yields no anchor needs no
/// retries at all — that is proof on the first attempt — and is quarantined
/// immediately.
pub const PANEL_ANCHOR_DEBT_IDENTITY_MAX_ATTEMPTS: u32 = 3;

/// `CF_KV` key prefix for the durable anchor-debt repair state, one row per
/// `(panel generation, backfill source)`.
const PANEL_ANCHOR_DEBT_STATE_KEY_PREFIX: &str = "syn/anchor-debt/v1/";

/// Schema of the durable anchor-debt repair row.
///
/// v2 adds the per-identity failed-attempt ledger (#1984). The key prefix is
/// deliberately unchanged: v1 rows decode into v2 through `serde(default)` and
/// keep their cursor and quarantine, so the upgrade costs no repeated work. A
/// new key would have silently restarted every panel's queue at the head while
/// looking like a clean first pass.
const ANCHOR_DEBT_REPAIR_STATE_SCHEMA: &str = "synapse_anchor_debt_repair_state/v2";

/// `CF_KV` key prefix for the durable coverage-sweep cursor.
const PANEL_COVERAGE_CURSOR_KEY_PREFIX: &str = "syn/panel-backfill-cursor/v1/";

/// Maximum records admitted to one unattended Loom pass.
///
/// The association and kNN work is quadratic in the records that share a slot.
/// A five-minute ingest interval is normally far smaller than this; when it is
/// not, [`drive_incremental_weave`] bisects the time range instead of silently
/// dropping everything beyond the cap.
pub const WEAVE_INTERVAL_MAX_RECORDS: usize = 2_000;

/// Authoritative Base identities published per atomic bootstrap commit.
///
/// This is deliberately independent from [`WEAVE_INTERVAL_MAX_RECORDS`]: the
/// publisher's transaction/memory bound and Loom's math bound are different
/// contracts even though their measured defaults currently coincide.
pub const WEAVE_SNAPSHOT_PUBLISH_MAX_RECORDS: usize = 2_000;

/// Records sampled by one settled-frontier post-ingest drift measurement.
///
/// Drift is a corpus assay rather than CDC consumption. Keeping its cap
/// independent prevents a future recovery tuning change from silently changing
/// the MMD population.
pub const POST_INGEST_DRIFT_MAX_RECORDS: usize = 2_000;

/// Durable consumer-offset row for one panel's association input stream.
const ASSOCIATION_WEAVE_CURSOR_KEY_PREFIX: &str = "syn/association-weave-cursor/v1/";
const ASSOCIATION_WEAVE_CURSOR_SCHEMA: &str = "synapse_association_weave_cursor/v1";
const ASSOCIATION_WEAVE_SOURCE_CONTRACT: &str = "calyx_panel_change_log/v1";
const ASSOCIATION_CHANGE_LOG_PRUNE_ROWS: usize = 2_000;

/// Wall-clock budget for one panel's incremental weave in one maintenance tick.
pub const WEAVE_PANEL_TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// Kernel construction includes all-pairs graph work, so it runs daily rather
/// than on every five-minute maintenance tick.
pub const KERNEL_REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_hours(24);
pub const KERNEL_REBUILD_MAX_RECORDS: usize = 2_000;

/// Causal maps are rolling operational intelligence, so refresh them more
/// often than the corpus kernel while keeping the estimator work off the
/// five-minute hot cadence.
pub const CAUSAL_MAP_REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_hours(1);
/// Six hours is the smallest built-in operational window that normally carries
/// enough one-minute bins for the longest declared eight-bin lag while staying
/// far below the 4,096-bin assay ceiling.
pub const CAUSAL_MAP_WINDOW: std::time::Duration = std::time::Duration::from_hours(6);
/// Event-time watermark applied to autonomous causal windows.
///
/// Two complete one-minute bins let normal ingestion settle before a window
/// becomes final. Later in-window data still conflicts atomically at
/// publication and remains a typed failure rather than being silently dropped.
pub const CAUSAL_MAP_FINALIZATION_LAG: std::time::Duration = std::time::Duration::from_mins(2);
pub const CAUSAL_MAP_MAX_RECORDS: usize = 20_000;
pub const CAUSAL_MAP_BIN_SECONDS: f64 = 60.0;
pub const CAUSAL_MAP_MAX_LAG: usize = 8;
pub const CAUSAL_MAP_FDR_ALPHA: f32 = 0.05;

/// Maximum bisection work items accepted for one panel interval.
const WEAVE_MAX_INTERVAL_PARTS: usize = 1_024;

// ---------------------------------------------------------------------------
// Derived pass resident-memory contract (#2239)
// ---------------------------------------------------------------------------
//
// Corpus scans are deliberately sequential. A bounded page limits transient IO
// but does not bound the owned maps accumulated from all pages. Running several
// scans or weaves concurrently therefore multiplies live heap by the number of
// lanes, even when their keys and locks are independent. The deployed daemon
// proved that distinction physically: its storage caches remained below their
// bounds while parallel derived passes drove the process above 4 GiB working
// set. Each complete outcome is now published or consumed before the next
// independent corpus pass starts. There is no concurrency override: permitting
// one would reintroduce the same architectural failure behind configuration.

/// Ticks started, ticks that ended with every sub-pass clean, ticks that ended
/// with at least one sub-pass failure, and ticks that never ran (#2080 ask 1).
///
/// **All four count the same physical thing: one maintenance tick.** They used
/// not to. `attempts` and `success` were per tick while `failure` was
/// incremented once per *sub-pass* failure, so the deployed daemon published
/// `attempts=6 success=0 failure=15` — a rollup in which the failure counter ran
/// at 2.5x the attempt counter it was supposed to be a subset of, and no ratio
/// computed from the three meant anything. Failure counts and attempt counts
/// belong to one denominator or they are not comparable; the per-sub-pass layer
/// is a genuinely different aggregation and is published as one, beside them,
/// rather than folded into them.
///
/// Invariant, checked by construction at the end of every tick:
/// `attempts == success + failure + skipped + (at most one tick in flight)`.
static DERIVED_STATE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_SUCCESS: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_FAILURE: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// Individual sub-pass failures, across every tick. The *diagnostic* layer:
/// which component broke and how often, which is a different question from how
/// many ticks failed and must never be reported through the same counter.
static DERIVED_STATE_SUBPASS_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Cost and quality advisories published by the maintainer. Visible, never a
/// failure (#2080 ask 2).
static DERIVED_STATE_ADVISORIES: AtomicU64 = AtomicU64::new(0);

/// Sub-pass failures whose text one tick's readback carries verbatim.
///
/// The evidence is the point, so it is bounded rather than summarised: past this
/// many the readback names how many more it is not printing instead of dropping
/// the count.
const DERIVED_STATE_SUBPASS_EVIDENCE_CAP: usize = 64;

/// Sub-pass failures recorded by the tick **currently in flight** (#2145).
///
/// # Why this is not `DerivedStateReadback::last_tick_subpass_failures`
///
/// It was, and that is the whole defect. The tick opened by clearing the
/// published verdict — `last_tick_subpass_failures.clear()` and
/// `last_tick_failed = None` — and only republished it at the end. Between those
/// two points the readback did not describe the last *completed* tick; it
/// described a tick that had not finished. `health` reads
/// `(has_run, last_tick_failed)` and maps `(true, None)` to `error`, which is
/// correct for its intended meaning ("the only thing that has ever happened is a
/// skip") and catastrophic for the meaning it was accidentally given ("a tick is
/// running right now").
///
/// The window was not small. `storage_derived_state` averaged 73 s and peaked at
/// 110 s on a 300 s cadence (#2116), so **a quarter to a third of all health
/// reads landed inside it** and reported `calyx_derived_state.status=error`
/// — with `last_tick_subpass_failures=[]` and `last_run_unix_ms ==
/// last_success_unix_ms` beside it, because those still held the previous,
/// successful tick's values. That is exactly the production reading #2145 was
/// filed on: `attempts=6 success=4 failure=1`, i.e. one tick in flight, and
/// every nested field truthful while the rollup said `error`.
///
/// The in-flight ledger therefore lives here, off the published readback, and
/// the readback's `last_tick_*` fields are written exactly once per tick, at the
/// end, from this list — so they always describe a tick that completed.
static DERIVED_STATE_TICK_LEDGER: LazyLock<Mutex<Vec<String>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Appends one failure to the in-flight tick's ledger, under its evidence cap.
fn append_tick_ledger(evidence: String) {
    let mut ledger = match DERIVED_STATE_TICK_LEDGER.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match ledger.len() {
        len if len < DERIVED_STATE_SUBPASS_EVIDENCE_CAP => ledger.push(evidence),
        len if len == DERIVED_STATE_SUBPASS_EVIDENCE_CAP => ledger.push(format!(
            "... further sub-pass failures this tick are counted in subpass_failures_total but not \
             printed past the {DERIVED_STATE_SUBPASS_EVIDENCE_CAP}-entry evidence cap"
        )),
        _ => {}
    }
}

/// Failures the in-flight tick has recorded so far.
fn tick_ledger_len() -> usize {
    match DERIVED_STATE_TICK_LEDGER.lock() {
        Ok(guard) => guard.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    }
}

/// Takes the in-flight tick's ledger, leaving it empty for the next tick.
fn take_tick_ledger() -> Vec<String> {
    let mut ledger = match DERIVED_STATE_TICK_LEDGER.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    std::mem::take(&mut ledger)
}
static LAST_KERNEL_REBUILD_UNIX_MS: AtomicU64 = AtomicU64::new(0);
static LAST_CAUSAL_MAP_REBUILD_UNIX_MS: AtomicU64 = AtomicU64::new(0);

/// Storage handle the derived-state pass reads the vault from.
///
/// A `Weak` for the same reason the lowering source is: this registry must not
/// be the reason a closed vault's handle stays alive.
static DERIVED_STATE_SOURCE: LazyLock<Mutex<Option<Weak<Db>>>> = LazyLock::new(|| Mutex::new(None));

type ReactiveDeliverySink = dyn Fn(&SynapseCalyxPersistedDriftFinding) -> Result<ReactiveDeliveryReadback, String>
    + Send
    + Sync;

static REACTIVE_DELIVERY_SINK: LazyLock<Mutex<Option<Arc<ReactiveDeliverySink>>>> =
    LazyLock::new(|| Mutex::new(None));
type RegionDeliverySink = dyn Fn(&SynapseCalyxPersistedRegionFinding) -> Result<ReactiveDeliveryReadback, String>
    + Send
    + Sync;
static REGION_DELIVERY_SINK: LazyLock<Mutex<Option<Arc<RegionDeliverySink>>>> =
    LazyLock::new(|| Mutex::new(None));
type NoveltyDeliverySink = dyn Fn(&SynapseCalyxPersistedNoveltyFinding) -> Result<NoveltyDeliveryReadback, String>
    + Send
    + Sync;
static NOVELTY_DELIVERY_SINK: LazyLock<Mutex<Option<Arc<NoveltyDeliverySink>>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Copy, Debug, Default)]
pub struct ReactiveDeliveryReadback {
    pub matched: u64,
    pub queued: u64,
    pub dropped: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoveltyDeliveryReadback {
    pub notification: ReactiveDeliveryReadback,
    pub quarantine_escalated: bool,
}

static DERIVED_STATE_LAST: LazyLock<Mutex<DerivedStateReadback>> =
    LazyLock::new(|| Mutex::new(DerivedStateReadback::default()));

/// Inclusive durable CDC sequence through which each panel's Base/slot input
/// changes were completely woven. A missing durable cursor is never interpreted
/// as current: registration leaves it absent and the first pass publishes an
/// authoritative Base snapshot stream before advancing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WeaveCursorState {
    covered_through_seq: u64,
    bootstrap_through_seq: Option<u64>,
    durable_present: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DurableWeaveCursor {
    schema: String,
    panel_version: u32,
    covered_through_seq: u64,
    #[serde(default)]
    bootstrap_through_seq: Option<u64>,
    source_contract: String,
    updated_at_unix_ms: Option<u64>,
}

static WEAVE_BASE_SEQ: LazyLock<Mutex<BTreeMap<u32, WeaveCursorState>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn association_weave_cursor_key(panel_version: u32) -> Vec<u8> {
    format!("{ASSOCIATION_WEAVE_CURSOR_KEY_PREFIX}{panel_version}").into_bytes()
}

fn load_durable_weave_cursor(
    db: &Arc<Db>,
    panel_version: u32,
) -> Result<Option<DurableWeaveCursor>, String> {
    let Some(cursor) = read_durable_maintenance_row::<DurableWeaveCursor>(
        db,
        &association_weave_cursor_key(panel_version),
        "association weave cursor",
    )?
    else {
        return Ok(None);
    };
    if cursor.schema != ASSOCIATION_WEAVE_CURSOR_SCHEMA
        || cursor.source_contract != ASSOCIATION_WEAVE_SOURCE_CONTRACT
        || cursor.panel_version != panel_version
        || cursor
            .bootstrap_through_seq
            .is_some_and(|through| through <= cursor.covered_through_seq)
    {
        return Err(format!(
            "cursor contract mismatch: expected schema={ASSOCIATION_WEAVE_CURSOR_SCHEMA} source={ASSOCIATION_WEAVE_SOURCE_CONTRACT} panel={panel_version} covered>0 bootstrap>covered; actual={cursor:?}"
        ));
    }
    Ok(Some(cursor))
}

fn persist_and_verify_weave_cursor(
    db: &Arc<Db>,
    cursor: &DurableWeaveCursor,
) -> Result<(), String> {
    let key = association_weave_cursor_key(cursor.panel_version);
    write_durable_maintenance_row(db, key.clone(), cursor, "association weave cursor")?;
    let physical = read_durable_maintenance_row::<DurableWeaveCursor>(
        db,
        &key,
        "association weave cursor readback",
    )?
    .ok_or_else(|| {
        format!(
            "panel {} association weave cursor write returned but the independent CF_KV readback found no row",
            cursor.panel_version
        )
    })?;
    if physical != *cursor {
        return Err(format!(
            "panel {} association weave cursor readback differs from the committed candidate: expected={cursor:?} actual={physical:?}",
            cursor.panel_version
        ));
    }
    Ok(())
}

fn prune_acknowledged_weave_history(
    db: &Arc<Db>,
    panel_version: u32,
    through_seq: u64,
) -> Result<(), String> {
    if through_seq == 0 {
        return Ok(());
    }
    // Association snapshots and real content mutations share one ordered CDC
    // stream, but search is an independent consumer. Snapshot signals can be
    // retired at the association cursor; real mutations may be retired only
    // once both the association cursor and the persisted search generation
    // cover them. This is the replication-slot `restart_lsn` rule applied to
    // the two durable consumers instead of assuming the faster one owns
    // retention.
    let search_status = db
        .calyx_search_generation_status_for_panel(panel_version, false)
        .map_err(|error| {
            format!(
                "read panel {panel_version} persisted search cursor before pruning association history: {error}"
            )
        })?;
    let mutation_through_seq = search_status.built_at_seq.unwrap_or(0).min(through_seq);
    let report = db
        .prune_panel_input_changes(
            panel_version,
            through_seq,
            mutation_through_seq,
            ASSOCIATION_CHANGE_LOG_PRUNE_ROWS,
        )
        .map_err(|error| {
            format!(
                "prune panel {panel_version} durable association-input history through acknowledged cursor {through_seq}: {error}"
            )
        })?;
    tracing::debug!(
        code = "STORAGE_DERIVED_STATE_WEAVE_HISTORY_PRUNED",
        panel_version,
        acknowledged_through_seq = through_seq,
        search_mutation_through_seq = mutation_through_seq,
        mutation_floor_seq = report.mutation_floor_seq,
        rows_deleted = report.rows_deleted,
        prune_commit_seq = report.committed_seq,
        "pruned one bounded page only after the durable association cursor was independently reread"
    );
    Ok(())
}

/// Externally readable outcome of the derived-state maintainer, as published for
/// `health` to read without touching the vault.
#[derive(Clone, Debug, Default)]
pub struct DerivedStateReadback {
    /// Tick-granularity outcome counters. `success + failure + skipped` equals
    /// `attempts` once the tick in flight completes, and `failure` can never
    /// exceed `attempts` (#2080 ask 1).
    pub attempts_total: u64,
    pub success_total: u64,
    pub failure_total: u64,
    pub skipped_total: u64,
    /// Sub-pass-granularity failure counter, published beside the tick counters
    /// rather than inside them. One tick can hold many of these.
    pub subpass_failures_total: u64,
    /// Cost and quality advisories. Loud, and deliberately not failures: their
    /// remediation is a code change, so they must never gate readiness.
    pub advisories_total: u64,
    /// Whether the last **completed** tick failed, and the exact sub-pass
    /// failures it recorded. This is what `health` reads: a lifetime counter
    /// cannot say whether the maintainer is broken *now*, and reading `error`
    /// off one is how a subsystem stayed red across a hundred clean ticks.
    pub last_tick_failed: Option<bool>,
    pub last_tick_subpass_failures: Vec<String>,
    pub last_run_unix_ms: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    /// What the last completed pass decided about the search generation.
    pub last_search_action: Option<String>,
    pub last_search_reason: Option<String>,
    /// Generation state read back **from disk** after the last pass.
    pub last_search_state_after: Option<SynapseCalyxSearchGenerationStatus>,
    pub last_search_elapsed_ms: Option<u64>,
    /// Every published search generation's maintenance outcome from the last
    /// pass (#1938), so `health` can report how close each generation is to the
    /// bound at which its queries start failing — before the first one does,
    /// rather than after.
    pub last_search_sweep: Option<crate::search_sweep::SearchGenerationSweep>,
    pub last_search_sweep_unix_ms: Option<u64>,
    /// Lens coverage measured by the last pass.
    pub last_lens_coverage: Option<SynapseCalyxLensCoverageStatus>,
    pub last_lens_coverage_unix_ms: Option<u64>,
    /// Per-panel coverage and grounding census from the last pass (#1927 ask 1,
    /// #1920 ask 1). `health` reads this rather than measuring a corpus.
    pub last_panel_coverage: Option<crate::panel_coverage::PanelCoverageReport>,
    pub last_panel_coverage_unix_ms: Option<u64>,
    /// What the last pass did about the panel most owed a backfill (#1927 ask 2).
    pub last_backfill_action: Option<String>,
    pub last_backfill_reason: Option<String>,
    pub last_backfill_panel: Option<String>,
    pub last_backfill_source_cf: Option<String>,
    pub last_backfill_pages: Option<u64>,
    pub last_backfill_examined_rows: Option<u64>,
    pub last_backfill_inserted_rows: Option<u64>,
    pub last_backfill_already_current_rows: Option<u64>,
    pub last_backfill_outcome_anchored_rows: Option<u64>,
    pub last_backfill_elapsed_ms: Option<u64>,
    /// True when the sweep reached the end of its source CF this tick, so the
    /// cursor reset to the start of the CF for the next generation bump.
    pub last_backfill_sweep_complete: Option<bool>,
    /// Coverage targets owed a sweep, and how many this tick actually swept
    /// (#2061 ask 3).
    ///
    /// These two are equal on a healthy tick, by construction. `targets_owed=4
    /// targets_attempted=1` on every pass of two consecutive daemon generations
    /// is what the head-of-queue starvation looked like from the outside, and it
    /// was invisible in every other field: the tick reported a panel, a page
    /// count and an elapsed time that all looked like work, because they were —
    /// for one panel out of four.
    pub last_backfill_targets_owed: u64,
    pub last_backfill_targets_attempted: u64,
    /// Owed targets this tick did **not** sweep, each with the evidenced reason.
    /// Empty on a healthy tick; never silently non-empty.
    pub last_backfill_targets_skipped: Vec<String>,
    /// Anchors the coverage sweep carried across a generation bump in passing.
    ///
    /// The sweep always did this and never reported it, so the one number that
    /// says whether the #1980 carry-forward is working at all was invisible
    /// while `inserted_rows=0` was read as "nothing happened" (#1984).
    pub last_backfill_anchors_carried_forward: Option<u64>,
    /// --- Identity-driven anchor-debt repair (#1984, #2061) ---
    ///
    /// What the last tick did about exact stranded anchor identities, reported
    /// apart from the coverage sweep because the two are different repairs with
    /// different meters: coverage debt is measured by rows inserted, anchor debt
    /// by anchors carried. Reading one by the other is how a 60-second pass that
    /// carried nothing was read as a pass that had nothing to carry.
    pub last_anchor_debt_action: Option<String>,
    /// One line per debt-bearing panel: debt, queue position, and what this tick
    /// did to it. Every panel every tick — never one target per tick (#2061).
    pub last_anchor_debt_panels: Vec<String>,
    pub last_anchor_debt_identities_attempted: u64,
    /// Attempts whose carry-forward wrote at least one anchor. The progress
    /// meter for anchor debt.
    pub last_anchor_debt_anchors_carried: u64,
    pub last_anchor_debt_identities_carried: u64,
    /// Constellations the exact-identity repair had to materialize because the
    /// active generation did not hold the row at all.
    pub last_anchor_debt_inserted_rows: u64,
    /// Panels whose enumerated identity queue was consumed to the end this tick,
    /// so the next census measures a complete repair attempt.
    pub last_anchor_debt_passes_completed: Vec<String>,
    /// Exact identities proven unrepairable, named in full: an attempt ran
    /// against the row and the declared lineage yielded no anchor to carry.
    /// Quarantined per identity so they can never starve another panel.
    pub last_anchor_debt_quarantined: Vec<String>,
    pub last_anchor_debt_quarantined_total: u64,
    /// Panels carrying stranded anchors with no re-measure path at all.
    pub last_anchor_debt_unbackfillable_panels: Vec<String>,
    /// Measured cost of one exact-identity repair this tick. Published rather
    /// than assumed, because it is the number that says whether the repair is
    /// debt-proportional or has quietly become corpus-proportional again.
    ///
    /// **Numerator and denominator now describe the same work (#2080 defect
    /// 3).** This used to be the whole phase's wall clock — every debt-bearing
    /// panel's durable state read, quarantine prune and cursor write, including
    /// panels that attempted nothing — divided by the identities actually
    /// attempted. Those per-panel fixed costs do not scale with identities, so
    /// as the debt drained from ~1,000 attempts a tick to 2 the same absolute
    /// cost was divided by a 500x smaller denominator and published as a 500x
    /// per-identity regression. It is now the sum of the exact-identity repair
    /// calls only; the fixed costs are `last_anchor_debt_elapsed_ms` minus
    /// `last_anchor_debt_repair_ms`, and the lineage index build is measured in
    /// its own right below.
    pub last_anchor_debt_ms_per_identity: Option<u64>,
    /// Wall clock spent inside the exact-identity repair calls themselves.
    pub last_anchor_debt_repair_ms: u64,
    /// Whole-phase wall clock, including every panel's fixed per-tick cost.
    pub last_anchor_debt_elapsed_ms: Option<u64>,
    /// Debt-bearing panels this tick actually attempted an identity on. The
    /// denominator the lineage-rebuild count is judged against: the grounded
    /// anchor lineage is one index per `(source CF, superseded set)`, so one
    /// build per attempted panel is amortized and more than that is not.
    pub last_anchor_debt_panels_attempted: u64,
    /// Grounded-anchor lineage index builds and their cost during this tick's
    /// anchor-debt phase, measured in the backend rather than inferred from a
    /// wall-clock ratio (#2080 defect 3).
    pub last_anchor_debt_lineage_rebuilds: u64,
    pub last_anchor_debt_lineage_reuses: u64,
    pub last_anchor_debt_lineage_rebuild_ms: u64,
    /// Last incremental Loom action and physical readback (#1671).
    pub last_weave_actions: BTreeMap<u32, String>,
    pub last_weave_through_seq: BTreeMap<u32, u64>,
    pub last_weave_records: BTreeMap<u32, u64>,
    /// Per-panel rows submitted and durably flushed by the completed interval
    /// parts in this pass. These are write counts, not claims that every upsert
    /// grew the physical CF cardinality.
    pub last_weave_xterm_rows_written: BTreeMap<u32, usize>,
    pub last_weave_graph_rows_written: BTreeMap<u32, usize>,
    /// Physical vault-global CF gauges observed after this panel's last
    /// completed interval part. Completion order can make these differ across
    /// otherwise equivalent parallel runs, so the names deliberately do not
    /// imply panel-local or additive state (#2151).
    pub last_weave_global_xterm_cf_rows_after: BTreeMap<u32, usize>,
    pub last_weave_global_graph_cf_rows_after: BTreeMap<u32, usize>,
    /// How each adjacent global gauge was obtained: a physical walk, a drift
    /// check, or proof that the CF commit sequence was unchanged (#2114).
    pub last_weave_global_xterm_cf_rows_readback: BTreeMap<u32, String>,
    pub last_weave_global_graph_cf_rows_readback: BTreeMap<u32, String>,
    /// Committed Base sequences this panel's weave has not reached yet, the
    /// parts still owed, and how many consecutive ticks that sequence-distance
    /// gauge has grown.
    ///
    /// `last_weave_through_seq` is the committed **frontier**, not the vault tip
    /// the tick aimed at, so these fields distinguish a frozen cursor from a
    /// converging one without consulting wall-clock timestamps.
    pub last_weave_backlog_seqs: BTreeMap<u32, u64>,
    pub last_weave_pending_parts: BTreeMap<u32, usize>,
    pub last_weave_backlog_growth_ticks: BTreeMap<u32, u32>,
    /// Scheduled post-ingest Reactive drift production and delivery (#1680).
    pub last_reactive_drift_rows: BTreeMap<u32, usize>,
    pub last_reactive_notifications_matched: BTreeMap<u32, u64>,
    pub last_reactive_notifications_queued: BTreeMap<u32, u64>,
    pub last_reactive_notifications_dropped: BTreeMap<u32, u64>,
    pub last_region_rows_read: u64,
    pub last_region_notifications_matched: u64,
    pub last_region_notifications_queued: u64,
    pub last_region_notifications_dropped: u64,
    pub last_region_delivery_watermark: u64,
    pub last_novelty_rows_read: u64,
    pub last_novelty_notifications_matched: u64,
    pub last_novelty_notifications_queued: u64,
    pub last_novelty_notifications_dropped: u64,
    pub last_novelty_delivery_watermark: u64,
    pub last_novelty_quarantines_escalated: u64,
    /// Last autonomous typed causal-map outcome per declared
    /// `panel_name:group_key` scope. Successful rows name both independently
    /// read Graph keys and hashes; non-applicable rows retain their exact Calyx
    /// refusal code instead of inventing an empty artifact.
    pub last_causal_map_actions: BTreeMap<String, String>,
    pub last_causal_map_pointer_keys: BTreeMap<String, String>,
    pub last_causal_map_artifact_keys: BTreeMap<String, String>,
    pub last_causal_map_artifact_sha256: BTreeMap<String, String>,
    pub last_causal_map_source_fingerprint_sha256: BTreeMap<String, String>,
    pub last_causal_map_source_records: BTreeMap<String, usize>,
    pub last_causal_map_latest_event_ns: BTreeMap<String, u64>,
    pub last_causal_map_rebuild_unix_ms: Option<u64>,
    pub last_causal_map_window_since_ns: Option<i64>,
    pub last_causal_map_window_until_ns: Option<i64>,
    pub last_causal_map_finalization_lag_ms: Option<u64>,
    /// Last scheduled per-panel kernel outcome, including the physical Kernel
    /// CF row count returned after persistence.
    pub last_kernel_actions: BTreeMap<u32, String>,
    pub last_kernel_recall: BTreeMap<u32, f32>,
    pub last_kernel_cf_rows: BTreeMap<u32, usize>,
    pub last_kernel_rebuild_unix_ms: Option<u64>,
    /// The last failure, retained across later successes so a lifetime failure
    /// counter can never outlive its own evidence (the #1889 lesson).
    pub last_failure_code: Option<String>,
    pub last_failure_detail: Option<String>,
    pub last_failure_unix_ms: Option<u64>,
    /// The most recent advisory, on its own channel so it can be loud without
    /// being counted as, or mistaken for, a failure (#2080 ask 2).
    pub last_advisory_code: Option<String>,
    pub last_advisory_detail: Option<String>,
    pub last_advisory_unix_ms: Option<u64>,
    /// The most recent skip reason, cleared by the next completed pass.
    pub last_skip_code: Option<String>,
    pub last_skip_detail: Option<String>,
    /// Thresholds this maintainer runs under, published so an operator reading
    /// health never has to guess which budget produced the decision.
    pub refresh_delta_keys_threshold: u64,
    pub min_rebuild_interval_ms: u64,
}

/// Registers the storage handle whose Calyx vault the derived-state pass reads.
pub fn register_derived_state_source(db: &Arc<Db>) {
    let weak = Arc::downgrade(db);
    match DERIVED_STATE_SOURCE.lock() {
        Ok(mut guard) => *guard = Some(weak),
        Err(poisoned) => *poisoned.into_inner() = Some(weak),
    }
    let mut watermarks = match WEAVE_BASE_SEQ.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    for &(panel_version, _) in crate::constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS {
        match load_durable_weave_cursor(db, panel_version) {
            Ok(Some(cursor)) => {
                watermarks.insert(
                    panel_version,
                    WeaveCursorState {
                        covered_through_seq: cursor.covered_through_seq,
                        bootstrap_through_seq: cursor.bootstrap_through_seq,
                        durable_present: true,
                    },
                );
            }
            Ok(None) => {
                // Absence is not permission to bless the current sequence as a
                // baseline. The first maintenance pass publishes a complete
                // durable snapshot stream and starts behind it.
                watermarks.insert(panel_version, WeaveCursorState::default());
            }
            Err(error) => record_failure(
                "STORAGE_DERIVED_STATE_WEAVE_CURSOR_INVALID",
                format!("panel {panel_version} durable weave cursor is unusable: {error}"),
            ),
        }
    }
    drop(watermarks);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_SOURCE_REGISTERED",
        db_path = %db.path.display(),
        refresh_delta_keys_threshold = SEARCH_GENERATION_REFRESH_DELTA_KEYS,
        min_rebuild_interval_ms = SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS,
        "registered the storage handle the unattended derived-state maintainer reads"
    );
}

/// Registers the daemon-owned delivery boundary for exact persisted Reactive
/// findings. Re-registration replaces the prior process-local sink.
pub fn register_reactive_delivery_sink<F>(sink: F)
where
    F: Fn(&SynapseCalyxPersistedDriftFinding) -> Result<ReactiveDeliveryReadback, String>
        + Send
        + Sync
        + 'static,
{
    let sink: Arc<ReactiveDeliverySink> = Arc::new(sink);
    match REACTIVE_DELIVERY_SINK.lock() {
        Ok(mut guard) => *guard = Some(sink),
        Err(poisoned) => *poisoned.into_inner() = Some(sink),
    }
}

/// Registers the daemon-owned subscriber delivery boundary for exact persisted
/// discrete-region findings.
pub fn register_region_delivery_sink<F>(sink: F)
where
    F: Fn(&SynapseCalyxPersistedRegionFinding) -> Result<ReactiveDeliveryReadback, String>
        + Send
        + Sync
        + 'static,
{
    let sink: Arc<RegionDeliverySink> = Arc::new(sink);
    match REGION_DELIVERY_SINK.lock() {
        Ok(mut guard) => *guard = Some(sink),
        Err(poisoned) => *poisoned.into_inner() = Some(sink),
    }
}

pub fn register_novelty_delivery_sink<F>(sink: F)
where
    F: Fn(&SynapseCalyxPersistedNoveltyFinding) -> Result<NoveltyDeliveryReadback, String>
        + Send
        + Sync
        + 'static,
{
    let sink: Arc<NoveltyDeliverySink> = Arc::new(sink);
    match NOVELTY_DELIVERY_SINK.lock() {
        Ok(mut guard) => *guard = Some(sink),
        Err(poisoned) => *poisoned.into_inner() = Some(sink),
    }
}

/// Current derived-state counters and last outcome.
#[must_use]
pub fn derived_state_readback() -> DerivedStateReadback {
    let mut readback = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    readback.attempts_total = DERIVED_STATE_ATTEMPTS.load(Ordering::Relaxed);
    readback.success_total = DERIVED_STATE_SUCCESS.load(Ordering::Relaxed);
    readback.failure_total = DERIVED_STATE_FAILURE.load(Ordering::Relaxed);
    readback.skipped_total = DERIVED_STATE_SKIPPED.load(Ordering::Relaxed);
    readback.subpass_failures_total = DERIVED_STATE_SUBPASS_FAILURES.load(Ordering::Relaxed);
    readback.advisories_total = DERIVED_STATE_ADVISORIES.load(Ordering::Relaxed);
    readback.refresh_delta_keys_threshold = SEARCH_GENERATION_REFRESH_DELTA_KEYS;
    readback.min_rebuild_interval_ms = SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS;
    readback
}

/// Runs the same bounded pass owned by the periodic derived-state task and
/// returns the process-published readback after it completes.
///
/// This is the operator/FSV seam for proving a scheduled pass without changing
/// its production interval or constructing a second implementation.
///
/// The tick's verdict is deliberately not returned twice. It is
/// [`DerivedStateReadback::last_tick_failed`] and
/// [`DerivedStateReadback::last_tick_subpass_failures`] on the value below, and
/// the `Err` that [`run_derived_state_maintenance`] hands the maintenance task
/// is derived from those same two fields — one ledger, two renderings of it.
/// A caller that needs the task's exact return value calls the pass directly.
#[must_use]
pub fn run_derived_state_maintenance_once() -> DerivedStateReadback {
    let _ = run_derived_state_maintenance();
    derived_state_readback()
}

fn now_unix_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
}

/// Records the outcome of a tick that completed **as a skip**.
///
/// A skip is a completed tick, not an in-flight one, so it publishes a verdict
/// like any other completion (#2145): `last_tick_failed = None` with an empty
/// evidence list, which is the state `health` reads as "this maintainer has run
/// and maintained nothing". That reading is unchanged and deliberate — a
/// maintainer that only ever skips is not `ok`. What changed is that it is now
/// reached only by an actual skip, and never by a tick that is merely still
/// running.
fn record_skip(code: &'static str, detail: String) {
    DERIVED_STATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(code, detail, "derived-state maintenance pass skipped");
    let ledgered = take_tick_ledger();
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_unix_ms = now_unix_ms();
    guard.last_tick_failed = None;
    guard.last_tick_subpass_failures = ledgered;
    guard.last_skip_code = Some(code.to_owned());
    guard.last_skip_detail = Some(detail);
}

/// Records one **sub-pass** failure into the tick's outcome ledger (#2080).
///
/// This is the only place a derived-state failure is recorded, and it no longer
/// touches the tick counters. The tick's verdict is computed once, at the end of
/// the tick, from the ledger this writes — so `success`, `failure` and the
/// evidence an operator reads are three views of one set of physical outcomes
/// rather than three independent tallies that can disagree. They did disagree:
/// `failure` counted sub-passes, `success` required a whole clean tick, and the
/// pair published `success=0 failure=15` over `attempts=6`.
fn record_failure(code: &'static str, detail: String) {
    DERIVED_STATE_SUBPASS_FAILURES.fetch_add(1, Ordering::Relaxed);
    tracing::error!(
        code,
        detail,
        "unattended derived-state maintenance sub-pass failed; the derived layer it maintains stays \
         at whatever state it was already in, and health reports that state"
    );
    // The evidence goes to the in-flight ledger, never to the published
    // `last_tick_*` fields (#2145): those describe the last **completed** tick
    // and are written exactly once, at the end of this one. `last_failure_*`
    // below are explicitly historical — they outlive their tick by design (the
    // #1889 lesson) — so they are written here, as they always were.
    append_tick_ledger(format!("{code}: {detail}"));
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_unix_ms = now_unix_ms();
    guard.last_failure_code = Some(code.to_owned());
    guard.last_failure_unix_ms = now_unix_ms();
    guard.last_failure_detail = Some(detail);
}

/// Returns allocator-owned pages after every complete corpus owner has been
/// dropped, before the next independent whole-corpus phase starts.
///
/// The phase boundary is correctness, not a memory cap: keeping dead graph,
/// search, coverage, or weave arenas committed until the next phase makes the
/// peak the sum of unrelated operations. A missing executable-owned reclaimer
/// or unreadable process-memory Source of Truth is an explicit tick failure.
fn release_completed_phase_memory(operation: &'static str) -> Result<(), String> {
    let release = synapse_calyx::release_process_memory(operation)
        .map_err(|error| format!("release allocator pages after {operation}: {error}"))?;
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_PHASE_MEMORY_RELEASED",
        operation,
        private_bytes_before = release.private_bytes_before,
        private_bytes_after = release.private_bytes_after,
        private_bytes_reclaimed = release.private_bytes_reclaimed,
        release_elapsed_us = release.elapsed_us,
        "released one completed derived-state phase before the next independent corpus owner"
    );
    Ok(())
}

/// Records a cost or quality **advisory** — loud, and never a failure (#2080
/// ask 2).
///
/// The distinction is not cosmetic. An advisory says a measurement crossed a
/// declared engineering ceiling; nothing was left stale, no derived layer is
/// wrong, and the remediation is a code change rather than an operator action.
/// Routing one through `record_failure` made
/// `STORAGE_DERIVED_STATE_ANCHOR_DEBT_REPAIR_UNAMORTIZED` — emitted on a tick
/// that *successfully carried two anchors*, and whose own text said "the
/// identity work queue is correct" — into `last_failure_code`, a failure count,
/// and a permanently false `health.ok`. Readiness must never be gated on a
/// condition no operator can clear.
fn record_advisory(code: &'static str, detail: String) {
    DERIVED_STATE_ADVISORIES.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        code,
        detail,
        "unattended derived-state maintenance published a cost advisory; nothing failed and no \
         derived layer is stale because of it, so it does not gate readiness"
    );
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_advisory_code = Some(code.to_owned());
    guard.last_advisory_detail = Some(detail);
    guard.last_advisory_unix_ms = now_unix_ms();
}

/// Runs one unattended derived-state maintenance pass and returns **the tick's
/// own outcome** (#2088).
///
/// The two halves are independent on purpose: a search-generation rebuild that
/// fails must not stop lens coverage from being measured and reported, because
/// they answer different questions and an operator needs both. Each half records
/// its own failure.
///
/// # Why this returns a `Result` at all
///
/// It did not, and the maintenance task's `GcRunner` therefore could not fail:
/// `run_once` closed over a `()` and returned `Ok(GcReport::default())`
/// unconditionally, so `STORAGE_MAINTENANCE_COMPLETED operation=
/// "storage_derived_state" is_ok=…` was structurally `true` on every tick
/// including the ones that failed. An operator grepping that log for a broken
/// maintainer would have found nothing, forever, however broken it was. The
/// verdict already existed after #2080 — it just stopped at the module boundary.
///
/// # Errors
///
/// Returns [`StorageError::WriteFailed`](crate::StorageError::WriteFailed)
/// naming this tick's sub-pass failure codes when the tick's outcome ledger is
/// non-empty. The error is **derived from** that ledger rather than recorded
/// beside it, so it cannot double-count: a sub-pass failure is counted once, by
/// [`record_failure`], and this is the same fact crossing a boundary.
///
/// A recorded *skip* — no storage handle registered — is `Ok`. Nothing was
/// attempted, nothing is stale, and there is no fault to report.
///
/// # The retry decision, stated (#2088 ask 2)
///
/// **A failed derived-state tick is not retried inside its tick.** The chosen
/// error variant carries `STORAGE_WRITE_FAILED`, which `gc::gc_failure_kind`
/// classifies `Terminal` — `retryable_gc_failure_kind` therefore returns `None`
/// and `gc::spawn_runner` breaks on the first attempt. That is a decision, and
/// the reasons are structural rather than stylistic:
///
/// * This tick is not one operation. It is ~15 independent sub-passes, and a
///   retry re-runs the ~14 that succeeded — including a search-generation
///   rebuild and a coverage sweep with a 60-second budget and a 3-minute hard
///   ceiling. That is a second full tick, not a retry of the failed work.
/// * Each sub-pass already isolates its own failure and leaves the layer it
///   maintains at its previous state. The five-minute cadence is the retry, and
///   it retries from a fresh coherent snapshot rather than the stale lease the
///   failing attempt held.
/// * The retry path exists for *contention* — Calyx backpressure and the shared
///   native maintenance lock — and both of those are already handled inside the
///   individual sub-passes that touch the vault. A sub-pass failure that
///   reaches here is a measurement or an invariant that did not hold, which
///   another immediate attempt cannot change.
#[allow(
    clippy::too_many_lines,
    reason = "one tick: the sub-pass groups, their spawn-site independence proofs, and the single \
              verdict read off one outcome ledger are a sequence whose order is the contract"
)]
pub fn run_derived_state_maintenance() -> crate::StorageResult<()> {
    // Hot-path boundary (#1686): every one of these reads is off-runtime
    // intelligence work and must never be driven from a tagged reflex tick.
    hot_context::assert_cold_calyx("maintenance_derived_state");
    DERIVED_STATE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    // The tick's outcome ledger starts empty. Every sub-pass failure below
    // appends to it, and the tick's verdict is read back off it — so the verdict
    // cannot be reached by a route that does not also leave its evidence behind.
    //
    // It is the **in-flight** ledger, off the published readback (#2145). This
    // used to clear `last_tick_subpass_failures` and null `last_tick_failed` on
    // the published struct, which erased the last completed tick's verdict for
    // the 73-110 s this tick then took to produce a new one — and `health` reads
    // a null verdict after a run as `error`. Nothing published is touched here
    // any more; the readback keeps describing the last tick that *finished*
    // until this one does.
    drop(take_tick_ledger());
    let source = match DERIVED_STATE_SOURCE.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(db) = source.as_ref().and_then(Weak::upgrade) else {
        record_skip(
            "STORAGE_DERIVED_STATE_SOURCE_UNREGISTERED",
            "no live storage handle is registered for derived-state maintenance; call \
             synapse_storage::derived_state::register_derived_state_source when storage opens"
                .to_owned(),
        );
        return Ok(());
    };

    let mut any_failed = false;
    let mut current_panel_coverage = None;

    // --- Graph lanes: consume each whole-corpus result at the earliest safe
    //     boundary (#2239) ---
    //
    // `scan_app_focus_lane` owns both an edge map and a unique-path corpus.
    // Publish it immediately so those allocations are gone before either agent
    // lane is built. The agent and process maps must coexist because their
    // publisher joins the two sources; no unrelated third corpus overlaps them.
    match scan_app_focus_lane(&db) {
        Ok(lane) => {
            lane.scan.lane_yield.report(lane.scan.source_seq);
            if let Err(error) = publish_app_transition_graph(&db, lane) {
                any_failed = true;
                record_failure("STORAGE_DERIVED_STATE_APP_GRAPH_FAILED", error.to_string());
            }
        }
        Err(error) => {
            any_failed = true;
            record_failure("STORAGE_DERIVED_STATE_APP_GRAPH_FAILED", error.to_string());
        }
    }
    match (scan_agent_spawn_lane(&db), scan_process_parent_lane(&db)) {
        (Ok(agent), Ok(process)) => {
            agent.lane_yield.report(agent.source_seq);
            process.lane_yield.report(process.source_seq);
            if let Err(error) = publish_agent_spawn_graph(&db, agent, process) {
                any_failed = true;
                record_failure(
                    "STORAGE_DERIVED_STATE_AGENT_GRAPH_FAILED",
                    error.to_string(),
                );
            }
        }
        (Err(error), _) | (Ok(_), Err(error)) => {
            any_failed = true;
            record_failure(
                "STORAGE_DERIVED_STATE_AGENT_GRAPH_FAILED",
                error.to_string(),
            );
        }
    }
    if let Err(error) = release_completed_phase_memory("scheduled derived-state graph lanes") {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_GRAPH_MEMORY_RELEASE_FAILED", error);
    }

    // --- Durable hot-added lens backfill (#1668) ---
    // Run before search maintenance so a generation rebuilt on this same tick
    // can include the newly materialized Slot CF rows.
    for entry in crate::constellations::builtin_panel_catalog() {
        let lifecycle = match db.read_panel_lifecycle(entry.panel_version) {
            Ok(state) => state,
            Err(error) => {
                any_failed = true;
                record_failure(
                    "STORAGE_DERIVED_STATE_LIFECYCLE_READ_FAILED",
                    format!("read lifecycle state for {}: {error}", entry.panel_name),
                );
                continue;
            }
        };
        let Some(state) = lifecycle else {
            continue;
        };
        let pending = state
            .controller
            .queue()
            .tasks()
            .filter(|task| task.state == calyx_registry::BackfillState::Pending)
            .count();
        if pending == 0 {
            continue;
        }
        match db.run_panel_backfill(entry.panel_version, LIFECYCLE_BACKFILL_BATCH_ROWS, false) {
            Ok(report) => tracing::info!(
                code = "STORAGE_DERIVED_STATE_LIFECYCLE_BACKFILL",
                panel_name = report.panel_name,
                source_panel_version = report.source_panel_version,
                target_panel_version = report.target_panel_version,
                claimed = report.claimed,
                completed = report.completed,
                pending = report.pending,
                registry_committed_seq = report.registry_committed_seq,
                registry_value_sha256 = report.registry_value_sha256,
                "completed a bounded durable panel lifecycle backfill batch"
            ),
            Err(error) => {
                any_failed = true;
                record_failure(
                    "STORAGE_DERIVED_STATE_LIFECYCLE_BACKFILL_FAILED",
                    format!("drive lifecycle backfill for {}: {error}", entry.panel_name),
                );
            }
        }
    }

    // --- Search generations (#1891 ask 2, extended to every published
    // generation by #1938) ---
    match db.maintain_calyx_search_generation() {
        Ok(mut sweep) => {
            // A generation nothing could maintain, and a generation whose
            // maintenance failed, both mean some corpus is heading for or
            // already past its reconciliation bound. Neither may be reported as
            // a clean pass.
            if sweep.any_failed() {
                any_failed = true;
                record_failure(
                    "STORAGE_DERIVED_STATE_SEARCH_GENERATION_FAILED",
                    format!(
                        "maintaining at least one published Calyx search generation failed: {}",
                        sweep.summary_line()
                    ),
                );
            }
            let unmaintainable: Vec<u32> = sweep
                .generations
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.disposition,
                        crate::search_sweep::GenerationDisposition::UnmaintainableNoContract
                    )
                })
                .map(|entry| entry.panel_version)
                .collect();
            if !unmaintainable.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_SEARCH_GENERATION_UNMAINTAINABLE",
                    unmaintainable_panels = ?unmaintainable,
                    detail = %sweep.summary_line(),
                    "one or more published search generations belong to panel versions with no \
                     code-declared slot contract; they can never be rebuilt into their \
                     reconciliation bound and no query can measure through them"
                );
            }
            let mut guard = match DERIVED_STATE_LAST.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if sweep.last_rebuild_memory.is_none() {
                sweep.last_rebuild_memory = guard
                    .last_search_sweep
                    .as_ref()
                    .and_then(|previous| previous.last_rebuild_memory.clone());
            }
            // The single-generation fields keep reporting the **active** panel,
            // so the pre-#1938 health field means exactly what it always meant.
            // The sweep is published alongside it rather than folded into it: a
            // per-generation fact collapsed into one number is how a degrading
            // generation stayed invisible in the first place.
            if let Some(active) = sweep.active_generation()
                && let crate::search_sweep::GenerationDisposition::Maintained(report) =
                    &active.disposition
            {
                guard.last_search_action = Some(report.action.as_str().to_owned());
                guard.last_search_reason = Some(report.reason.clone());
                guard.last_search_elapsed_ms = Some(report.elapsed_ms);
                guard.last_search_state_after = Some(
                    report
                        .after
                        .clone()
                        .unwrap_or_else(|| report.before.clone()),
                );
            }
            guard.last_search_sweep = Some(sweep);
            guard.last_search_sweep_unix_ms = now_unix_ms();
        }
        Err(error) => {
            any_failed = true;
            record_failure(
                "STORAGE_DERIVED_STATE_SEARCH_GENERATION_FAILED",
                format!("sweep the published Calyx search generations: {error}"),
            );
        }
    }

    // --- Lens coverage (#1894 ask 2) ---
    match db.measure_calyx_lens_coverage(LENS_COVERAGE_SAMPLE_RECORDS) {
        Ok(coverage) => {
            if !coverage.deficient_panels.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_LENS_COVERAGE_DEFICIENT",
                    deficient_panels = ?coverage.deficient_panels,
                    blind_spot_ceiling = coverage.blind_spot_ceiling,
                    detail = %coverage
                        .panels
                        .iter()
                        .map(|panel| format!(
                            "panel {} n_lenses={} blind_spot_records={}/{} ({:.4})",
                            panel.panel_version,
                            panel.n_lenses,
                            panel.blind_spot_records,
                            panel.records_measured,
                            panel.blind_spot_fraction
                        ))
                        .collect::<Vec<_>>()
                        .join("; "),
                    "one or more panels carry too few lenses for any association-derived surface \
                     to measure anything on them"
                );
            }
            // Distributional findings are separate from structural coverage.
            // Each lane carries its own sample/census-qualified code; do not
            // promote bounded evidence into a whole-corpus claim here (#1983).
            if !coverage.degenerate_lanes.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_LENS_DISTRIBUTION_FINDING",
                    degenerate_lane_count = coverage.degenerate_lanes.len(),
                    detail = %coverage
                        .degenerate_lanes
                        .iter()
                        .map(|lane| format!(
                            "panel {} slot {} code={} observed={}/{} distinct={} frequency_ratio={:?} percent_unique={:.6} census_complete={} lifecycle_action_allowed={} stratified_override_status={}",
                            lane.panel_version,
                            lane.slot,
                            lane.code,
                            lane.records_present,
                            lane.population_records,
                            lane.distinct_values,
                            lane.frequency_ratio,
                            lane.percent_unique,
                            lane.census_complete,
                            lane.lifecycle_action_allowed,
                            lane.stratified_override_status
                        ))
                        .collect::<Vec<_>>()
                        .join("; "),
                    "one or more lanes require distribution review; sampled findings are provisional and never authorize a lifecycle change"
                );
            }
            let mut guard = match DERIVED_STATE_LAST.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.last_lens_coverage_unix_ms = now_unix_ms();
            guard.last_lens_coverage = Some(coverage);
        }
        Err(error) => {
            any_failed = true;
            record_failure(
                "STORAGE_DERIVED_STATE_LENS_COVERAGE_FAILED",
                format!("measure Calyx panel lens coverage: {error}"),
            );
        }
    }

    // Search and lens measurements have no ownership relationship with the
    // exact panel census. Their products are now either published or dropped;
    // return the allocator's dead pages before the largest streaming pass starts.
    if let Err(error) =
        release_completed_phase_memory("scheduled derived-state pre-panel-coverage phases")
    {
        any_failed = true;
        record_failure(
            "STORAGE_DERIVED_STATE_PRE_COVERAGE_MEMORY_RELEASE_FAILED",
            error,
        );
    }

    // --- Panel coverage census + driven backfill (#1927 asks 1/2, #1920 ask 1) ---
    //
    // Deliberately last and deliberately independent of the two halves above:
    // this is the expensive half (a whole-Base scan plus up to a minute of
    // re-measure work), and a failure in it must not cost the search generation
    // or the lens-coverage sample that already succeeded.
    match db.measure_panel_coverage() {
        Ok(report) => {
            if !report.coverage_deficient_panels.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_PANEL_COVERAGE_DEFICIENT",
                    coverage_deficient_panels = ?report.coverage_deficient_panels,
                    unbackfillable_deficient_panels = ?report.unbackfillable_deficient_panels,
                    coverage_floor = report.coverage_floor,
                    superseded_records_total = report.superseded_records_total,
                    detail = %report.summary_line(),
                    "one or more panels measure less than the declared fraction of their source \
                     CF; every surface scoped to the active panel sees only that fraction of the \
                     corpus, and a grounded anchor cannot even be written for a source row whose \
                     constellation is absent at the active version"
                );
            }
            if !report.unknown_panel_versions.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_PANEL_VERSION_UNCLAIMED",
                    unknown_panel_versions = ?report.unknown_panel_versions,
                    "the Base CF holds panel generations that NEITHER the built-in catalog nor \
                     the Registry CF generation allocator claims; their records are read by no \
                     active-panel surface, no re-measure knows how to rebuild them, and they are \
                     counted as stranded"
                );
            }
            // #2062: attributed, but attributed to a panel that is minting a
            // generation per pass and retiring none. Distinct from the line
            // above, which is now the genuinely-unclaimed case.
            if !report.dynamic_panels_multi_live.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_DYNAMIC_GENERATIONS_UNRETIRED",
                    dynamic_panels_multi_live = ?report.dynamic_panels_multi_live,
                    "one or more dynamic panels hold more than one live generation; a publisher \
                     minted a generation without retiring its predecessor, so those records are \
                     stranded as they are created and no reclaim can see them"
                );
                any_failed |= sweep_unretired_derived_generations(&db, &report);
            }
            // #2093. Two authorities disagree about who wrote a generation, and
            // only one of them was written by the panel that wrote the rows.
            // This is a sub-pass failure rather than a warning: the catalog is a
            // compile-time constant, so nothing at runtime can clear it, and
            // while it stands every anchor-debt and reclaim decision about those
            // rows is being taken against the wrong panel's declarations.
            if !report.catalog_lineage_misattributed.is_empty() {
                any_failed = true;
                record_failure(
                    "STORAGE_DERIVED_STATE_CATALOG_LINEAGE_MISATTRIBUTED",
                    format!(
                        "builtin_panel_catalog declares generations the Registry CF allocator \
                         attributes to a different panel: {:?}; the allocator's claim was written \
                         by the panel that reserved the generation while it was active, so the \
                         catalog constant is the wrong one and must be corrected in \
                         crates/synapse-storage/src/constellations.rs",
                        report.catalog_lineage_misattributed
                    ),
                );
            }
            // #2081 ask 4. A live owner row with no Base row behind it is
            // invisible to every census-derived surface, so this divergence is
            // the only place it can appear at all. Raised on the *dynamic* half
            // only: `builtin:` reservations without rows are the designed steady
            // state of a young vault and of every derived-snapshot panel, and a
            // warning that fires on those would be permanently on.
            if !report.allocator_live_dynamic_without_records.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_ALLOCATOR_CENSUS_LIVE_DIVERGED",
                    allocator_live_count = report.allocator_live_count,
                    census_live_count = report.census_live_count,
                    live_dynamic_without_records =
                        ?report.allocator_live_dynamic_without_records,
                    "the generation allocator holds live DYNAMIC owner claims that the physical \
                     Base census cannot see; a successful derived publish always commits at least \
                     one constellation, so each of these named a generation that never published \
                     and each permanently consumes one of the allocator's bounded owner slots. \
                     They are reported and never auto-retired: a zero-row reading cannot \
                     distinguish a publish that failed from one that has allocated and not yet \
                     committed its batch"
                );
            }
            if !report.reserved_generations_absent_from_catalog.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_RESERVED_GENERATION_UNDECLARED",
                    reserved_generations_absent_from_catalog =
                        ?report.reserved_generations_absent_from_catalog,
                    "the allocator reserves built-in generations that builtin_panel_catalog does \
                     not name; their rows are attributable but no panel row measures them"
                );
            }
            // A failed backfill page must not leave the tick counted as a clean
            // success. It already records its own failure, but without this the
            // pass would still bump DERIVED_STATE_SUCCESS and refresh
            // last_success_unix_ms, so an operator reading the counters would see
            // a healthy cadence over a backfill that has been failing every tick.
            any_failed |= drive_panel_backfill(&db, &report);
            current_panel_coverage = Some(report.clone());
            let mut guard = match DERIVED_STATE_LAST.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.last_panel_coverage_unix_ms = now_unix_ms();
            guard.last_panel_coverage = Some(report);
        }
        Err(error) => {
            any_failed = true;
            record_failure(
                "STORAGE_DERIVED_STATE_PANEL_COVERAGE_FAILED",
                format!("measure Calyx panel coverage: {error}"),
            );
        }
    }

    // `measure_panel_coverage` owns the packed physical-key indexes and Base
    // cursor only through the call above. Reclaim them before Loom loads its
    // dense corpus; otherwise the steady-state peak is the sum of two unrelated
    // full-corpus operations.
    if let Err(error) = release_completed_phase_memory("scheduled panel coverage census") {
        any_failed = true;
        record_failure(
            "STORAGE_DERIVED_STATE_PANEL_COVERAGE_MEMORY_RELEASE_FAILED",
            error,
        );
    }

    // --- Incremental Loom weave (#1671) ---
    //
    // This runs after the coverage census so it never delays the cheaper
    // structural health signals above. Each target owns an independent
    // watermark; one failed panel cannot advance itself or suppress the other
    // panel's work.
    //
    // Each panel owns disjoint keys, but that does not make its resident set
    // free: a weave retains its dense corpus and graph products until its batch
    // is committed. Run and account for one panel completely before loading the
    // next. Fixed panel order also preserves the outcome-ledger and advisory
    // semantics without result slots that keep completed products alive.
    for &(panel_version, _) in crate::constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS {
        let outcome = weave_panel_subpass(&db, panel_version);
        if let Some((code, detail)) = outcome.advisory {
            record_advisory(code, detail);
        }
        match outcome.weave {
            Ok(_) => {
                if let Some(error) = outcome.drift_error {
                    any_failed = true;
                    record_failure("STORAGE_DERIVED_STATE_REACTIVE_DRIFT_FAILED", error);
                }
            }
            Err(error) => {
                any_failed = true;
                record_failure(
                    "STORAGE_DERIVED_STATE_WEAVE_FAILED",
                    format!("incrementally weave panel {panel_version}: {error}"),
                );
            }
        }
    }

    if let Err(error) = drive_region_relay(&db) {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_REACTIVE_REGION_FAILED", error);
    }
    if let Err(error) = drive_novelty_relay(&db) {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_WARD_NOVELTY_FAILED", error);
    }

    if let Err(error) = drive_scheduled_causal_maps(&db) {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_CAUSAL_MAP_FAILED", error);
    }

    if let Err(error) = drive_scheduled_kernels(&db, current_panel_coverage.as_ref()) {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_KERNEL_REBUILD_FAILED", error);
    }
    if let Err(error) = release_completed_phase_memory("scheduled derived-state tick") {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_FINAL_MEMORY_RELEASE_FAILED", error);
    }

    // --- One verdict, from one ledger (#2080 ask 1) ---
    //
    // The verdict is the tick's, the ledger is the tick's, and both are read
    // here rather than accumulated in parallel with the counters. A sub-pass
    // that returned a failure without recording one is a defect in this module,
    // not a tick to quietly pass: it is named and it fails the tick, which is
    // also how it lands in the ledger the verdict is then taken from.
    if any_failed && tick_ledger_len() == 0 {
        record_failure(
            "STORAGE_DERIVED_STATE_OUTCOME_LEDGER_DIVERGED",
            "a derived-state sub-pass reported failure to the tick driver but recorded no failure \
             in the tick's outcome ledger, so the published counters would have described a clean \
             tick over a sub-pass that did not complete; failing the tick on the driver's report \
             and naming the divergence"
                .to_owned(),
        );
    }
    // --- One atomic publish of this tick's whole outcome (#2145) ---
    //
    // The ledger is taken first, then the readback lock is taken once, and the
    // verdict, the evidence, the run stamp and the success stamp are all written
    // under it. There is no instant at which a reader can see a verdict that
    // disagrees with the evidence beside it, and no instant at which a completed
    // tick's outcome has been erased and not yet replaced. Both were reachable
    // before: the tick cleared the published fields on entry and only refilled
    // them here, minutes later.
    let ledgered = take_tick_ledger();
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_unix_ms = now_unix_ms();
    let tick_failed = !ledgered.is_empty();
    guard.last_tick_subpass_failures = ledgered;
    guard.last_tick_failed = Some(tick_failed);
    if tick_failed {
        DERIVED_STATE_FAILURE.fetch_add(1, Ordering::Relaxed);
    } else {
        DERIVED_STATE_SUCCESS.fetch_add(1, Ordering::Relaxed);
        guard.last_success_unix_ms = now_unix_ms();
        guard.last_skip_code = None;
        guard.last_skip_detail = None;
    }
    let subpass_failures = guard.last_tick_subpass_failures.clone();
    drop(guard);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_TICK_COMPLETED",
        tick_failed,
        subpass_failures = subpass_failures.len(),
        attempts_total = DERIVED_STATE_ATTEMPTS.load(Ordering::Relaxed),
        success_total = DERIVED_STATE_SUCCESS.load(Ordering::Relaxed),
        failure_total = DERIVED_STATE_FAILURE.load(Ordering::Relaxed),
        skipped_total = DERIVED_STATE_SKIPPED.load(Ordering::Relaxed),
        subpass_failures_total = DERIVED_STATE_SUBPASS_FAILURES.load(Ordering::Relaxed),
        advisories_total = DERIVED_STATE_ADVISORIES.load(Ordering::Relaxed),
        subpass_failure_codes = ?subpass_failures,
        "completed one unattended derived-state tick and published its outcome at tick granularity"
    );
    // The maintenance task's return value, taken from the same ledger the log
    // line above was taken from (#2088). Not a second recording of anything:
    // every sub-pass failure named here was counted exactly once, by
    // `record_failure`, before this line could observe it.
    if tick_failed {
        return Err(crate::StorageError::WriteFailed {
            cf_name: "storage_derived_state".to_owned(),
            detail: format!(
                "STORAGE_DERIVED_STATE_TICK_FAILED: {} sub-pass(es) of this derived-state \
                 maintenance tick did not complete, so the derived layers they maintain are at \
                 whatever state they were already in: {:?}. This tick is not retried in place — \
                 see run_derived_state_maintenance's retry decision — and the next five-minute \
                 tick reattempts every sub-pass from a fresh coherent snapshot",
                subpass_failures.len(),
                subpass_failures,
            ),
        });
    }
    Ok(())
}

/// One graph lane's coherent read, with every write left to the driver (#2116).
///
/// The split is what makes the three lanes safe to run concurrently. A lane
/// scans exactly one column family through its own bounded, paged coherent lease
/// and returns what it derived; it publishes nothing, mints no panel generation
/// and touches no shared mutable state. Everything that *does* — the derived
/// snapshot publishes, the generation allocator, the tick's outcome ledger —
/// happens afterwards on the driver thread, in the order it happens today.
struct GraphLaneScan {
    /// `(from, to) -> count` edges this lane derived.
    counts: BTreeMap<(String, String), u64>,
    /// The committed sequence this lane's lease pinned.
    source_seq: u64,
    read_at_unix_ms: u64,
    lane_yield: GraphLaneYield,
}

/// The app-focus lane's scan, which also harvests the path-hierarchy corpus.
struct AppFocusLaneScan {
    scan: GraphLaneScan,
    hierarchy_paths: Vec<String>,
}

fn scan_app_focus_lane(db: &Db) -> crate::StorageResult<AppFocusLaneScan> {
    let mut lease = db.pin_cf_physical_scan(cf::CF_TIMELINE, crate::COHERENT_SCAN_MAX_AGE_MS)?;
    let source_seq = lease.snapshot_seq;
    let mut previous = None::<String>;
    let mut counts = BTreeMap::<(String, String), u64>::new();
    let mut hierarchy_paths = std::collections::BTreeSet::<String>::new();
    let mut app_yield = GraphLaneYield::new(APP_FOCUS_LANE);
    loop {
        let page = match db.scan_cf_physical_page_coherent(&mut lease, 1_000) {
            Ok(page) => page,
            Err(error) => {
                let _ = db.release_coherent_scan(&mut lease);
                return Err(error);
            }
        };
        for (key, value) in page.rows {
            let record: TimelineRecord = match serde_json::from_slice(&value) {
                Ok(record) => record,
                Err(error) => {
                    let _ = db.release_coherent_scan(&mut lease);
                    return Err(crate::StorageError::BackendInvalidConfig {
                        value: format!("{key:02x?}"),
                        detail: format!(
                            "decode coherent CF_TIMELINE row for app-transition graph: {error}"
                        ),
                    });
                }
            };
            if matches!(
                record.kind,
                TimelineKind::BrowserNav | TimelineKind::FileActivity
            ) {
                for key in ["url", "path", "document"] {
                    if let Some(value) = record.payload.get(key).and_then(serde_json::Value::as_str)
                        && !value.trim().is_empty()
                    {
                        hierarchy_paths.insert(value.to_owned());
                    }
                }
            }
            if record.kind != TimelineKind::FocusChange {
                continue;
            }
            app_yield.observe_row();
            let Some(app) = record.app.filter(|value| !value.trim().is_empty()) else {
                app_yield.observe_skip("focus_row_has_no_app");
                continue;
            };
            if let Some(from) = previous.replace(app.clone()) {
                if from == app {
                    app_yield.observe_skip("focus_row_repeats_previous_app");
                } else {
                    app_yield.observe_edge();
                    *counts.entry((from, app)).or_default() += 1;
                }
            } else {
                app_yield.observe_skip("first_focus_row_has_no_predecessor");
            }
        }
        if !page.more {
            break;
        }
    }
    db.release_coherent_scan(&mut lease)?;
    Ok(AppFocusLaneScan {
        scan: GraphLaneScan {
            counts,
            source_seq,
            read_at_unix_ms: lease.read_at_unix_ms,
            lane_yield: app_yield,
        },
        hierarchy_paths: hierarchy_paths.into_iter().collect(),
    })
}

/// Publishes what [`scan_app_focus_lane`] derived.
///
/// Every write of the app lane lives here, on the driver thread: the
/// path-hierarchy snapshot, the app graph-position snapshot, and the
/// supersession each publish performs. Both mint a generation from the
/// vault-global panel-generation allocator, which is a shared write path and
/// therefore explicitly not something the lanes may do concurrently.
fn publish_app_transition_graph(db: &Db, lane: AppFocusLaneScan) -> crate::StorageResult<()> {
    let AppFocusLaneScan {
        scan,
        hierarchy_paths,
    } = lane;
    let GraphLaneScan {
        counts,
        source_seq,
        read_at_unix_ms,
        lane_yield: _,
    } = scan;
    drive_path_hierarchy(db, source_seq, read_at_unix_ms, &hierarchy_paths)?;
    if counts.is_empty() {
        tracing::debug!(
            code = "STORAGE_DERIVED_STATE_APP_GRAPH_INELIGIBLE",
            source_seq,
            "coherent timeline snapshot has fewer than two distinct consecutive focus apps"
        );
        return Ok(());
    }
    let transitions = counts
        .into_iter()
        .map(|((src, dst), count)| (src, dst, count))
        .collect::<Vec<_>>();
    let fingerprint = crate::constellations::graph_snapshot_fingerprint(&transitions);
    if let Some(state) = db.read_panel_lifecycle(SYN_GRAPHPOS_APP_PANEL_VERSION)? {
        let already_published = state.added_lenses.values().any(|added| {
            matches!(
                added.source_projection,
                synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection::DerivedSnapshot {
                    snapshot,
                    ..
                } if snapshot == fingerprint
            )
        });
        if already_published {
            tracing::debug!(
                code = "STORAGE_DERIVED_STATE_APP_GRAPH_CURRENT",
                source_seq,
                fingerprint,
                "app-transition graph snapshot is already physically published"
            );
            return Ok(());
        }
    }
    let readback = publish_graph_snapshot(
        db,
        GraphPositionKind::App,
        source_seq,
        read_at_unix_ms,
        &transitions,
    )?;
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_APP_GRAPH_PUBLISHED",
        panel_version = readback.panel_version,
        source_seq = readback.source_seq,
        snapshot = readback.snapshot,
        constellation_count = readback.constellation_count,
        graph_row_count = readback.graph_row_count,
        committed_seq = readback.committed_seq,
        lifecycle_sha256 = readback.lifecycle_sha256,
        "scheduled app-transition graph snapshot was atomically published and physically read back"
    );
    Ok(())
}

fn drive_path_hierarchy(
    db: &Db,
    source_seq: u64,
    read_at_unix_ms: u64,
    paths: &[String],
) -> crate::StorageResult<()> {
    if paths.is_empty() {
        tracing::debug!(
            code = "STORAGE_DERIVED_STATE_PATH_GRAPH_INELIGIBLE",
            source_seq,
            "coherent timeline snapshot has no document or URL hierarchy paths"
        );
        return Ok(());
    }
    let transitions = crate::constellations::path_hierarchy_transitions(paths)?;
    let fingerprint = crate::constellations::graph_snapshot_fingerprint(&transitions);
    if lifecycle_has_snapshot(db, SYN_PATH_HIERARCHY_PANEL_VERSION, fingerprint)? {
        return Ok(());
    }
    let readback = db
        .publish_path_hierarchy_snapshot(
            source_seq,
            now_unix_ms().unwrap_or(read_at_unix_ms),
            paths,
        )
        .inspect_err(|error| {
            name_orphaned_derived_generation(
                db,
                crate::constellations::SYN_PATH_HIERARCHY_PANEL_NAME,
                &format!("publish path hierarchy snapshot: {error}"),
            );
        })?;
    supersede_derived_generation(
        db,
        crate::constellations::SYN_PATH_HIERARCHY_PANEL_NAME,
        readback.panel_version,
    )?;
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_PATH_GRAPH_PUBLISHED",
        panel_version = readback.panel_version,
        source_seq = readback.source_seq,
        snapshot = readback.snapshot,
        constellation_count = readback.constellation_count,
        graph_row_count = readback.graph_row_count,
        committed_seq = readback.committed_seq,
        lifecycle_sha256 = readback.lifecycle_sha256,
        "scheduled path hierarchy snapshot was atomically published and physically read back"
    );
    Ok(())
}

/// Named ids one orphan report will print before it summarises.
const DERIVED_ORPHAN_NAME_CAP: usize = 16;

/// Publishes one graph-position snapshot and retires what it supersedes
/// (#1685, #2062).
///
/// The publish, the orphan report and the supersession are one unit here rather
/// than three at each call site: a publish that commits without retiring its
/// predecessor is exactly the defect #2062 reports, and a second copy of this
/// sequence is a second place for it to be omitted.
fn publish_graph_snapshot(
    db: &Db,
    kind: GraphPositionKind,
    source_seq: u64,
    read_at_unix_ms: u64,
    transitions: &[(String, String, u64)],
) -> crate::StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback> {
    let readback = db
        .publish_graph_position_snapshot(
            kind,
            source_seq,
            now_unix_ms().unwrap_or(read_at_unix_ms),
            transitions,
        )
        .inspect_err(|error| {
            name_orphaned_derived_generation(
                db,
                kind.panel_name(),
                &format!("publish {} graph snapshot: {error}", kind.panel_name()),
            );
        })?;
    supersede_derived_generation(db, kind.panel_name(), readback.panel_version)?;
    Ok(readback)
}

/// Retires the generations a derived snapshot publish has just superseded
/// (#2062 ask 1).
///
/// # The ordering, and why it is the only safe one
///
/// This runs **after** the publish has committed and read back, never before,
/// and that is the same discipline an LSM compaction uses on its inputs: the
/// output version is installed first and only then are the input files moved
/// onto the obsolete list. Retiring first would mark rows reclaimable against a
/// successor that might never exist, which is how a reclaimer deletes the only
/// copy of something.
///
/// # Why the predecessor is not computed here
///
/// The allocator selects it, by exact owner identity — every generation whose
/// owner string is `dynamic:<panel_name>:<operation_id>` and which is not the
/// successor. A caller-side "everything below the new one" would be a range
/// guess, and a wrong one: three publishers share one vault-global number line,
/// so the ids immediately below `successor` typically belong to the *other two*
/// panels and are live.
///
/// # What was missing
///
/// Nothing retired anything. Each pass minted a generation, wrote its whole
/// snapshot under it, and left the previous one owned, un-superseded and
/// invisible to `superseded_reclaim_candidates` — 98 generations and 62,566
/// rows on the deployed daemon, growing by ~200 generations a day, with no
/// mechanism that could ever have noticed.
fn supersede_derived_generation(
    db: &Db,
    panel_name: &str,
    successor: u32,
) -> crate::StorageResult<()> {
    let readback = db.supersede_panel_generations(panel_name, successor)?;
    if readback.retired.is_empty() {
        tracing::debug!(
            code = "STORAGE_DERIVED_SNAPSHOT_GENERATION_ALREADY_SOLE",
            panel_name,
            successor,
            already_retired = readback.already_retired.len(),
            "the published derived generation was already this panel's only live generation"
        );
        return Ok(());
    }
    tracing::info!(
        code = "STORAGE_DERIVED_SNAPSHOT_GENERATION_SUPERSEDED",
        panel_name,
        successor,
        retired_count = readback.retired.len(),
        retired = ?readback
            .retired
            .iter()
            .take(DERIVED_ORPHAN_NAME_CAP)
            .collect::<Vec<_>>(),
        already_retired = readback.already_retired.len(),
        committed_seq = readback.committed_seq,
        "predecessor derived generations were durably retired behind the committed successor and \
         are now visible to superseded reclaim"
    );
    Ok(())
}

/// Retires a dynamic panel's un-superseded predecessors even when it did not
/// publish this tick (#2062 residual).
///
/// # The gap this closes
///
/// Supersession ran only inside [`publish_graph_snapshot`], so a panel's history
/// was retired only by its *next publish*. That is correct and sufficient for a
/// publisher that publishes; it is a permanent stall for one that does not.
/// Measured on the deployed daemon: across four unattended ticks and 20 minutes,
/// `syn-graphpos-app-v1` published four times and retired its whole run, while
/// `drive_path_hierarchy` did not publish once — so `syn-path-hierarchy-v1` sat
/// at **6 live generations holding 41,220 rows**, live and unreclaimable, for as
/// long as its snapshot fingerprint stays stable. Which could be days.
///
/// # Why this is safe without a publish
///
/// The retirement ledger and the allocator's owner identity carry the whole
/// argument, and neither depends on a publish having just happened:
///
/// * **The successor holds rows.** `live_generations` comes from the physical
///   `Base` census, and a generation with no rows produces no census entry — so
///   the newest live generation named here has at least one record by
///   construction. This is exactly the guarantee the publish-time path gets from
///   its committed read-back, arrived at from the durable side instead.
/// * **Predecessors are selected by owner identity, never by range.** The
///   allocator picks every generation owned `dynamic:<panel>:<operation_id>`
///   other than the successor. Three publishers share one vault-global number
///   line, so a range guess would retire the other two panels' live generations.
/// * **A wrong successor is refused, not obeyed.** The allocator refuses when
///   the panel owns a live generation *above* the requested successor, so the
///   only way this can be wrong — picking a successor that is not the newest,
///   e.g. if the census's named-generation cap ever binds — fails closed and
///   loudly rather than retiring a generation that is still being written.
/// * **Retirement is a declaration, not a delete.** It makes the predecessors'
///   *ungrounded* rows visible to `superseded_reclaim_candidates`; grounded rows
///   stay sacred, and nothing here removes a byte.
///
/// Ordering against the publishers is unchanged: this runs on the coverage
/// census, which is taken after the publishers have run, so a generation minted
/// earlier in this same tick is already the newest live one when it is read.
fn sweep_unretired_derived_generations(
    db: &Db,
    report: &crate::panel_coverage::PanelCoverageReport,
) -> bool {
    let mut failed = false;
    for rollup in &report.owned_dynamic_generations {
        if rollup.owner_kind != "dynamic" || rollup.live_generations.len() <= 1 {
            continue;
        }
        let Some(successor) = rollup.live_generations.iter().copied().max() else {
            continue;
        };
        tracing::info!(
            code = "STORAGE_DERIVED_SNAPSHOT_GENERATION_SWEEP_STARTED",
            panel_name = %rollup.panel_name,
            successor,
            live_generations = ?rollup.live_generations,
            live_records = rollup.live_records,
            "a dynamic panel holds more than one live generation and did not publish this tick; \
             retiring its predecessors behind the newest live generation rather than waiting for a \
             publish that may not come"
        );
        if let Err(error) = supersede_derived_generation(db, &rollup.panel_name, successor) {
            failed = true;
            record_failure(
                "STORAGE_DERIVED_SNAPSHOT_GENERATION_SWEEP_FAILED",
                format!(
                    "retiring {}'s predecessors behind live generation {successor} failed: \
                     {error}; the panel keeps {} live generations and its predecessors stay \
                     invisible to reclaim",
                    rollup.panel_name,
                    rollup.live_generations.len(),
                ),
            );
        }
    }
    failed
}

/// Names the generation a failed publish may have already allocated (#2062
/// ask 1).
///
/// The allocation and the row batch are two commits. A publish that fails
/// between them leaves a generation owned and empty, and the ask is explicit:
/// an orphan must be **named loudly, not leaked silently**.
///
/// It is deliberately not retired here. Retirement records a *successor*, and a
/// failed publish produced none; inventing one would either point at a
/// generation that does not hold this snapshot or re-retire a live one. The
/// orphan is instead named now and swept by the next successful publish, whose
/// supersession retires every live generation of the panel other than itself —
/// which is exactly the "full scan on restart" half of `RocksDB`'s obsolete-file
/// tracking, there for the same reason: a reference dropped by a crash is not
/// recoverable from memory and must be reconciled against durable state later.
fn name_orphaned_derived_generation(db: &Db, panel_name: &str, cause: &str) {
    let owner_prefix = format!("dynamic:{panel_name}:");
    match db.panel_generation_allocator() {
        Ok(allocator) => {
            let live: Vec<u32> = allocator
                .owners
                .iter()
                .filter(|(generation, owner)| {
                    owner.starts_with(&owner_prefix) && !allocator.retired.contains_key(generation)
                })
                .map(|(generation, _)| *generation)
                .collect();
            tracing::error!(
                code = "STORAGE_DERIVED_SNAPSHOT_GENERATION_ORPHANED",
                panel_name,
                cause,
                live_generation_count = live.len(),
                live_generations = ?live.iter().rev().take(DERIVED_ORPHAN_NAME_CAP).collect::<Vec<_>>(),
                newest_live_generation = ?live.last(),
                "a derived snapshot publish failed; every generation named here is owned by this \
                 panel and un-retired, so the newest is an empty generation the failed publish \
                 allocated. It is swept by the next successful publish's supersession"
            );
        }
        Err(error) => {
            tracing::error!(
                code = "STORAGE_DERIVED_SNAPSHOT_GENERATION_ORPHAN_UNREADABLE",
                panel_name,
                cause,
                error = %error,
                "a derived snapshot publish failed AND the panel generation allocator could not \
                 be read, so whether it left an orphaned generation is unknown"
            );
        }
    }
}

fn lifecycle_has_snapshot(
    db: &Db,
    panel_version: u32,
    fingerprint: u64,
) -> crate::StorageResult<bool> {
    Ok(db
        .read_panel_lifecycle(panel_version)?
        .is_some_and(|state| {
            state.added_lenses.values().any(|added| {
                matches!(
                    added.source_projection,
                    synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection::DerivedSnapshot {
                        snapshot,
                        ..
                    } if snapshot == fingerprint
                )
            })
        }))
}

fn scan_agent_spawn_lane(db: &Db) -> crate::StorageResult<GraphLaneScan> {
    let mut lease =
        db.pin_cf_physical_scan(cf::CF_AGENT_EVENTS, crate::COHERENT_SCAN_MAX_AGE_MS)?;
    let source_seq = lease.snapshot_seq;
    let read_at_unix_ms = lease.read_at_unix_ms;
    let mut counts = BTreeMap::<(String, String), u64>::new();
    let mut agent_yield = GraphLaneYield::new(AGENT_SPAWN_LANE);
    loop {
        let page = match db.scan_cf_physical_page_coherent(&mut lease, 1_000) {
            Ok(page) => page,
            Err(error) => {
                let _ = db.release_coherent_scan(&mut lease);
                return Err(error);
            }
        };
        for (key, value) in page.rows {
            let record: AgentEventRecord = match serde_json::from_slice(&value) {
                Ok(record) => record,
                Err(error) => {
                    let _ = db.release_coherent_scan(&mut lease);
                    return Err(crate::StorageError::BackendInvalidConfig {
                        value: format!("{key:02x?}"),
                        detail: format!(
                            "decode coherent CF_AGENT_EVENTS row for spawn graph: {error}"
                        ),
                    });
                }
            };
            agent_yield.observe_row();
            if record.kind != AgentEventKind::SpawnRequested {
                agent_yield.observe_skip("event_is_not_spawn_requested");
                continue;
            }
            let parent_session_id = record
                .payload
                .get("started_by_session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or(record.session_id)
                .or(record.attributes.conversation_id);
            let (Some(session_id), Some(spawn_id)) = (parent_session_id, record.spawn_id) else {
                agent_yield.observe_skip("spawn_event_lacks_session_or_spawn_id");
                continue;
            };
            if session_id.trim().is_empty() || spawn_id.trim().is_empty() {
                agent_yield.observe_skip("spawn_event_session_or_spawn_id_blank");
                continue;
            }
            agent_yield.observe_edge();
            *counts
                .entry((
                    format!("agent-session:{session_id}"),
                    format!("agent-spawn:{spawn_id}"),
                ))
                .or_default() += 1;
        }
        if !page.more {
            break;
        }
    }
    db.release_coherent_scan(&mut lease)?;
    Ok(GraphLaneScan {
        counts,
        source_seq,
        read_at_unix_ms,
        lane_yield: agent_yield,
    })
}

/// Fuses the agent-spawn and process-parent lanes and publishes the one graph
/// panel they feed.
///
/// The two lanes were nested — the process lane was a tail call inside the agent
/// scan — which is why they could not run concurrently and why the process
/// lane's cost was invisible in the agent lane's timing. They are siblings now.
/// The fusion is unchanged: one edge set, one fingerprint, one publish, and the
/// same `source_seq`/`read_at_unix_ms` maxima across the two leases.
fn publish_agent_spawn_graph(
    db: &Db,
    agent: GraphLaneScan,
    process: GraphLaneScan,
) -> crate::StorageResult<()> {
    let agent_yield = agent.lane_yield;
    let process_yield = process.lane_yield;
    let mut counts = agent.counts;
    // Same fold the nested call performed in place: the process lane's keys are
    // `process:<pid>` and the agent lane's are `agent-session:`/`agent-spawn:`,
    // so the two key spaces are disjoint by construction and this can never
    // combine an edge from one lane with an edge from the other.
    for (edge, count) in process.counts {
        *counts.entry(edge).or_default() += count;
    }
    let source_seq = agent.source_seq.max(process.source_seq);
    let read_at_unix_ms = agent.read_at_unix_ms.max(process.read_at_unix_ms);
    if counts.is_empty() {
        tracing::debug!(
            code = "STORAGE_DERIVED_STATE_AGENT_GRAPH_INELIGIBLE",
            source_seq,
            "coherent agent-event snapshot has no session-to-spawn edges"
        );
        return Ok(());
    }
    let transitions = counts
        .into_iter()
        .map(|((src, dst), count)| (src, dst, count))
        .collect::<Vec<_>>();
    let fingerprint = crate::constellations::graph_snapshot_fingerprint(&transitions);
    if lifecycle_has_snapshot(
        db,
        crate::constellations::SYN_GRAPHPOS_PROCESS_PANEL_VERSION,
        fingerprint,
    )? {
        return Ok(());
    }
    let readback = publish_graph_snapshot(
        db,
        GraphPositionKind::Process,
        source_seq,
        read_at_unix_ms,
        &transitions,
    )?;
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_AGENT_GRAPH_PUBLISHED",
        panel_version = readback.panel_version,
        source_seq = readback.source_seq,
        snapshot = readback.snapshot,
        constellation_count = readback.constellation_count,
        graph_row_count = readback.graph_row_count,
        committed_seq = readback.committed_seq,
        lifecycle_sha256 = readback.lifecycle_sha256,
        // Per-lane attribution of the fused edge set (#2089): a published
        // snapshot must say which of its lanes actually paid for it.
        agent_lane_rows_examined = agent_yield.rows_examined,
        agent_lane_edges_derived = agent_yield.edges_derived,
        process_lane_rows_examined = process_yield.rows_examined,
        process_lane_edges_derived = process_yield.edges_derived,
        process_lane_skips = %process_yield.skip_summary(),
        "scheduled process and agent-spawn graph snapshot was atomically published and physically read back"
    );
    Ok(())
}

/// Lane names used by [`GraphLaneYield`]. They are stable strings: an operator
/// greps `lane=` to find which half of a fused graph panel went quiet.
const APP_FOCUS_LANE: &str = "app_focus_transitions";
const AGENT_SPAWN_LANE: &str = "agent_spawn_edges";
const PROCESS_PARENT_LANE: &str = "process_parent_edges";

/// Per-lane edge accounting for one graph build (#2089).
///
/// A graph panel is fed by more than one lane, and its published snapshot is a
/// single fused edge set. That fusion is exactly what let the process lane of
/// `syn-graphpos-process-v1` contribute **zero** edges for 1085 source rows
/// while the panel kept publishing normally on the agent-spawn lane's edges
/// alone: "this lane contributed nothing" and "this lane contributed normally"
/// produced identical logs. This type makes a lane's yield a first-class,
/// reported number, and [`GraphLaneYield::report`] raises a warning whenever a
/// lane examines rows and derives no edges from any of them.
#[derive(Clone, Debug)]
struct GraphLaneYield {
    lane: &'static str,
    rows_examined: u64,
    edges_derived: u64,
    /// Exact reason vocabulary for every row that did not become an edge, so a
    /// zero-yield lane says *why* it is zero rather than only that it is.
    skips: BTreeMap<&'static str, u64>,
}

impl GraphLaneYield {
    const fn new(lane: &'static str) -> Self {
        Self {
            lane,
            rows_examined: 0,
            edges_derived: 0,
            skips: BTreeMap::new(),
        }
    }

    const fn observe_row(&mut self) {
        self.rows_examined += 1;
    }

    const fn observe_edge(&mut self) {
        self.edges_derived += 1;
    }

    fn observe_skip(&mut self, reason: &'static str) {
        *self.skips.entry(reason).or_default() += 1;
    }

    fn skip_summary(&self) -> String {
        if self.skips.is_empty() {
            return "none".to_owned();
        }
        self.skips
            .iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Emit the lane's yield. A lane that examined rows and derived no edges is
    /// a defect until proven otherwise, so it is a warning, not a debug line.
    fn report(&self, source_seq: u64) {
        if self.rows_examined == 0 {
            tracing::debug!(
                code = "STORAGE_DERIVED_STATE_GRAPH_LANE_SOURCE_EMPTY",
                lane = self.lane,
                source_seq,
                "graph lane has no source rows to derive edges from"
            );
            return;
        }
        if self.edges_derived == 0 {
            tracing::warn!(
                code = "STORAGE_DERIVED_STATE_GRAPH_LANE_CONTRIBUTED_NO_EDGES",
                lane = self.lane,
                source_seq,
                rows_examined = self.rows_examined,
                edges_derived = 0_u64,
                skips = %self.skip_summary(),
                "graph lane examined source rows and derived zero edges; the panel fed by this \
                 lane is publishing without it, and any measurement attributed to this lane is \
                 absent rather than empty"
            );
            return;
        }
        tracing::debug!(
            code = "STORAGE_DERIVED_STATE_GRAPH_LANE_YIELD",
            lane = self.lane,
            source_seq,
            rows_examined = self.rows_examined,
            edges_derived = self.edges_derived,
            skips = %self.skip_summary(),
            "graph lane derived edges from its source rows"
        );
    }
}

/// What one `CF_PROCESS_HISTORY` row contributes to the process lane.
#[derive(Clone, Copy, Debug)]
enum ProcessEdgeDecision {
    /// `(parent_pid, pid)`.
    Edge(u64, u64),
    /// The row carries no trustworthy edge, for this exact reason.
    Skip(&'static str),
}

fn scan_process_parent_lane(db: &Db) -> crate::StorageResult<GraphLaneScan> {
    let mut lease =
        db.pin_cf_physical_scan(cf::CF_PROCESS_HISTORY, crate::COHERENT_SCAN_MAX_AGE_MS)?;
    let source_seq = lease.snapshot_seq;
    let read_at_unix_ms = lease.read_at_unix_ms;
    let mut counts = BTreeMap::<(String, String), u64>::new();
    let mut yield_report = GraphLaneYield::new(PROCESS_PARENT_LANE);
    loop {
        let page = match db.scan_cf_physical_page_coherent(&mut lease, 1_000) {
            Ok(page) => page,
            Err(error) => {
                let _ = db.release_coherent_scan(&mut lease);
                return Err(error);
            }
        };
        for (key, value) in page.rows {
            let record: serde_json::Value = match serde_json::from_slice(&value) {
                Ok(record) => record,
                Err(error) => {
                    let _ = db.release_coherent_scan(&mut lease);
                    return Err(crate::StorageError::BackendInvalidConfig {
                        value: format!("{key:02x?}"),
                        detail: format!(
                            "decode coherent CF_PROCESS_HISTORY row for process graph: {error}"
                        ),
                    });
                }
            };
            let Some(object) = record.as_object() else {
                let _ = db.release_coherent_scan(&mut lease);
                return Err(crate::StorageError::BackendInvalidConfig {
                    value: format!("{key:02x?}"),
                    detail: "CF_PROCESS_HISTORY row for process graph is not a JSON object"
                        .to_owned(),
                });
            };
            yield_report.observe_row();
            let decision = match process_parent_edge(object, &key) {
                Ok(decision) => decision,
                Err(error) => {
                    let _ = db.release_coherent_scan(&mut lease);
                    return Err(error);
                }
            };
            let ProcessEdgeDecision::Edge(parent_pid, pid) = decision else {
                if let ProcessEdgeDecision::Skip(reason) = decision {
                    yield_report.observe_skip(reason);
                }
                continue;
            };
            if pid == 0 || parent_pid == 0 || pid == parent_pid {
                yield_report.observe_skip("degenerate_pid_pair");
                continue;
            }
            yield_report.observe_edge();
            *counts
                .entry((format!("process:{parent_pid}"), format!("process:{pid}")))
                .or_default() += 1;
        }
        if !page.more {
            break;
        }
    }
    db.release_coherent_scan(&mut lease)?;
    Ok(GraphLaneScan {
        counts,
        source_seq,
        read_at_unix_ms,
        lane_yield: yield_report,
    })
}

/// The process lane's edge-derivation input contract (#2089).
///
/// A parent pid on its own is not evidence of parentage. Windows frees a pid for
/// reuse once the process object is gone, so a child's recorded parent pid may
/// name an unrelated process that started later
/// (`devblogs.microsoft.com/oldnewthing/20150403-00`). The observation written
/// by the launcher therefore carries the creation times of both processes, and
/// this reader re-checks the guard rather than trusting the writer:
///
/// * `parentage.state` must be `parent_verified`;
/// * the parent's creation time must be at or before the child's;
/// * the flat `parent_pid` the graph keys on must equal the observed parent pid.
///
/// Rows that carry a bare parent pid with no guard evidence are refused, not
/// used: an unguarded pid is indistinguishable from a recycled one, and a
/// fabricated edge is worse than a missing one. Every refusal returns an exact
/// reason that the lane's yield report counts and prints.
fn process_parent_edge(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &[u8],
) -> crate::StorageResult<ProcessEdgeDecision> {
    let Some(pid) = exact_json_u64(object.get("pid"), "pid", key)? else {
        return Ok(ProcessEdgeDecision::Skip("row_has_no_pid"));
    };
    let flat_parent_pid = exact_json_u64(object.get("parent_pid"), "parent_pid", key)?;
    let Some(parentage) = object.get("parentage") else {
        let legacy_claim = flat_parent_pid.is_some()
            || object.contains_key("ppid")
            || object.contains_key("inherited_from_pid");
        return Ok(ProcessEdgeDecision::Skip(if legacy_claim {
            "parent_pid_without_reuse_guard_evidence"
        } else {
            "row_carries_no_parentage_observation"
        }));
    };
    let Some(parentage) = parentage.as_object() else {
        return Err(crate::StorageError::BackendInvalidConfig {
            value: format!("{key:02x?}"),
            detail: "CF_PROCESS_HISTORY parentage field is not a JSON object".to_owned(),
        });
    };
    let state = parentage.get("state").and_then(serde_json::Value::as_str);
    match state {
        Some("parent_verified") => {}
        Some("parent_pid_recycled") => {
            return Ok(ProcessEdgeDecision::Skip("parentage_parent_pid_recycled"));
        }
        Some("parent_identity_unavailable") => {
            return Ok(ProcessEdgeDecision::Skip(
                "parentage_parent_identity_unavailable",
            ));
        }
        Some("no_parent_recorded") => {
            return Ok(ProcessEdgeDecision::Skip("parentage_no_parent_recorded"));
        }
        Some("child_absent") => {
            return Ok(ProcessEdgeDecision::Skip("parentage_child_absent"));
        }
        Some("snapshot_unavailable") => {
            return Ok(ProcessEdgeDecision::Skip("parentage_snapshot_unavailable"));
        }
        Some("platform_unsupported") => {
            return Ok(ProcessEdgeDecision::Skip("parentage_platform_unsupported"));
        }
        Some(_) => {
            return Ok(ProcessEdgeDecision::Skip("parentage_state_unrecognized"));
        }
        None => {
            return Ok(ProcessEdgeDecision::Skip("parentage_state_absent"));
        }
    }
    let observed_parent_pid = exact_json_u64(
        parentage.get("parent_pid_observed"),
        "parentage.parent_pid_observed",
        key,
    )?;
    let parent_start = exact_json_u64(
        parentage.get("parent_start_time_100ns"),
        "parentage.parent_start_time_100ns",
        key,
    )?;
    let child_start = exact_json_u64(
        parentage.get("child_start_time_100ns"),
        "parentage.child_start_time_100ns",
        key,
    )?;
    let (Some(observed_parent_pid), Some(parent_start), Some(child_start)) =
        (observed_parent_pid, parent_start, child_start)
    else {
        return Ok(ProcessEdgeDecision::Skip(
            "parentage_verified_without_reuse_guard_fields",
        ));
    };
    if parent_start > child_start {
        return Ok(ProcessEdgeDecision::Skip(
            "parentage_start_time_guard_violated",
        ));
    }
    let Some(flat_parent_pid) = flat_parent_pid else {
        return Ok(ProcessEdgeDecision::Skip(
            "parentage_verified_without_flat_parent_pid",
        ));
    };
    if flat_parent_pid != observed_parent_pid {
        return Ok(ProcessEdgeDecision::Skip(
            "parent_pid_disagrees_with_parentage_evidence",
        ));
    }
    Ok(ProcessEdgeDecision::Edge(flat_parent_pid, pid))
}

fn exact_json_u64(
    value: Option<&serde_json::Value>,
    field: &str,
    key: &[u8],
) -> crate::StorageResult<Option<u64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    value
        .as_u64()
        .map(Some)
        .ok_or_else(|| crate::StorageError::BackendInvalidConfig {
            value: format!("{key:02x?}"),
            detail: format!(
                "CF_PROCESS_HISTORY process-graph field {field} must be an unsigned JSON integer"
            ),
        })
}

fn causal_map_target_id(target: crate::constellations::SynCausalMapMaintenanceTarget) -> String {
    format!("{}:{}", target.panel_name, target.group_key)
}

fn clear_causal_map_physical_readback(target_id: &str) {
    let mut readback = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    readback.last_causal_map_pointer_keys.remove(target_id);
    readback.last_causal_map_artifact_keys.remove(target_id);
    readback.last_causal_map_artifact_sha256.remove(target_id);
    readback
        .last_causal_map_source_fingerprint_sha256
        .remove(target_id);
    readback.last_causal_map_source_records.remove(target_id);
    readback.last_causal_map_latest_event_ns.remove(target_id);
}

fn record_causal_map_action(target_id: String, action: String) {
    let mut readback = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    readback.last_causal_map_actions.insert(target_id, action);
}

fn causal_map_evidence_status_counts(
    artifact: &synapse_calyx::SynapseCalyxCausalMapArtifact,
) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    let global = [
        &artifact.pc_stable_skeleton,
        &artifact.partial_correlation_network,
        &artifact.hawkes_branching_graph,
    ];
    for evidence in global
        .into_iter()
        .chain(artifact.pairs.iter().flat_map(|pair| {
            [
                &pair.transfer_entropy,
                &pair.granger_a_to_b,
                &pair.granger_b_to_a,
                &pair.cross_correlation,
                &pair.convergent_cross_mapping,
                &pair.temporal_cross_k,
            ]
        }))
    {
        *counts.entry(evidence.status.as_str()).or_insert(0) += 1;
    }
    counts
}

fn causal_map_evidence_error_codes(
    artifact: &synapse_calyx::SynapseCalyxCausalMapArtifact,
) -> std::collections::BTreeSet<String> {
    let mut codes = std::collections::BTreeSet::new();
    let global = [
        &artifact.pc_stable_skeleton,
        &artifact.partial_correlation_network,
        &artifact.hawkes_branching_graph,
    ];
    for evidence in global
        .into_iter()
        .chain(artifact.pairs.iter().flat_map(|pair| {
            [
                &pair.transfer_entropy,
                &pair.granger_a_to_b,
                &pair.granger_b_to_a,
                &pair.cross_correlation,
                &pair.convergent_cross_mapping,
                &pair.temporal_cross_k,
            ]
        }))
    {
        if let Some(error) = &evidence.error {
            codes.insert(error.code.clone());
        }
    }
    codes
}

#[expect(
    clippy::too_many_lines,
    reason = "one target's publish, independent physical read, identity comparison, and atomic health projection form one verification boundary"
)]
fn drive_one_scheduled_causal_map(
    db: &Arc<Db>,
    target: crate::constellations::SynCausalMapMaintenanceTarget,
    since_ns: i64,
    until_ns: i64,
) -> Result<(), String> {
    let target_id = causal_map_target_id(target);
    let mut params = synapse_calyx::SynapseCalyxTemporalParams::new(target.panel_version);
    params.max_records = CAUSAL_MAP_MAX_RECORDS;
    params.since_ts_ns = Some(since_ns);
    params.until_ts_ns = Some(until_ns);
    params.group_key = Some(target.group_key.to_owned());
    params.bin_seconds = CAUSAL_MAP_BIN_SECONDS;
    params.max_lag = CAUSAL_MAP_MAX_LAG;

    let mut source_recompute_attempts = 0_u8;
    let published = loop {
        source_recompute_attempts = source_recompute_attempts.saturating_add(1);
        match db.temporal_causal_map_intelligence(&params, CAUSAL_MAP_FDR_ALPHA) {
            Ok(report) => break report,
            Err(error)
                if error.code() == "SYNAPSE_CALYX_CAUSAL_MAP_SOURCE_STALE"
                    && source_recompute_attempts == 1 =>
            {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_CAUSAL_MAP_SOURCE_RECOMPUTE",
                    target = %target_id,
                    panel_version = target.panel_version,
                    group_key = target.group_key,
                    since_ts_ns = since_ns,
                    until_ts_ns = until_ns,
                    detail = %error,
                    "the bounded source changed before its atomic publication; recomputing the same finalized contract exactly once"
                );
            }
            Err(error)
                if matches!(
                    error.code(),
                    "SYNAPSE_CALYX_CAUSAL_MAP_EMPTY_SCOPE"
                        | "SYNAPSE_CALYX_CAUSAL_MAP_STREAMS_INSUFFICIENT"
                        | "SYNAPSE_CALYX_CAUSAL_MAP_PAIR_WORK_BUDGET_EXCEEDED"
                        | "SYNAPSE_CALYX_CAUSAL_MAP_ALIGNED_CELL_BUDGET_EXCEEDED"
                        | "SYNAPSE_CALYX_CAUSAL_MAP_PAIR_LAG_WORK_BUDGET_EXCEEDED"
                        | "SYNAPSE_CALYX_CAUSAL_MAP_PC_WORK_BUDGET_EXCEEDED"
                        | "SYNAPSE_CALYX_CAUSAL_MAP_ARTIFACT_BYTE_BUDGET_EXCEEDED"
                ) =>
            {
                clear_causal_map_physical_readback(&target_id);
                record_causal_map_action(
                    target_id,
                    format!(
                        "not_published source_recompute_attempts={source_recompute_attempts} code={} detail={} remediation={}",
                        error.code(),
                        error,
                        error
                            .remediation()
                            .unwrap_or("inspect the exact typed failure")
                    ),
                );
                return Ok(());
            }
            Err(error) => {
                clear_causal_map_physical_readback(&target_id);
                return Err(format!(
                    "target={target_id} panel={} group_key={} source_recompute_attempts={source_recompute_attempts}: code={} detail={} remediation={}",
                    target.panel_version,
                    target.group_key,
                    error.code(),
                    error,
                    error
                        .remediation()
                        .unwrap_or("inspect the exact typed failure")
                ));
            }
        }
    };

    // A producer's return is not the verdict. Resolve the normalized-scope
    // pointer through the independent reader, hash the immutable artifact, and
    // re-fingerprint the exact closed source window before publishing health.
    let readback = db
        .read_temporal_causal_map_intelligence(&params, CAUSAL_MAP_FDR_ALPHA)
        .map_err(|error| {
            clear_causal_map_physical_readback(&target_id);
            format!(
                "target={target_id} causal-map publication could not be independently read: code={} detail={} remediation={}",
                error.code(),
                error,
                error.remediation().unwrap_or("inspect the exact typed failure")
            )
        })?;
    let identity_matches = published.graph_key_hex == readback.graph_key_hex
        && published.graph_value_sha256 == readback.graph_value_sha256
        && published.pointer_key_hex == readback.pointer_key_hex
        && published.artifact.source_fingerprint_sha256
            == readback.artifact.source_fingerprint_sha256
        && published.artifact.source_records == readback.artifact.source_records
        && published.artifact.latest_event_ns == readback.artifact.latest_event_ns
        && published.physical_readback_matches
        && published.pointer_readback_matches
        && readback.physical_readback_matches
        && readback.pointer_readback_matches;
    if !identity_matches {
        clear_causal_map_physical_readback(&target_id);
        return Err(format!(
            "target={target_id} independent Graph read did not reproduce the publication identity; published_pointer={} read_pointer={} published_artifact={} read_artifact={}",
            published.pointer_key_hex,
            readback.pointer_key_hex,
            published.graph_value_sha256,
            readback.graph_value_sha256
        ));
    }

    let status_counts = causal_map_evidence_status_counts(&readback.artifact);
    let error_codes = causal_map_evidence_error_codes(&readback.artifact);
    if !error_codes.is_empty() {
        tracing::warn!(
            code = "STORAGE_DERIVED_STATE_CAUSAL_MAP_ESTIMATOR_FAILURES",
            target = %target_id,
            evidence_statuses = ?status_counts,
            estimator_error_codes = ?error_codes,
            "the complete causal-map artifact was published with typed failed or unresolved estimator lanes; no substitute estimator was used"
        );
    }
    let mut state = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.last_causal_map_actions.insert(
        target_id.clone(),
        format!(
            "published source_recompute_attempts={source_recompute_attempts} window_since_ns={since_ns} window_until_ns={until_ns} streams={} pairs={} aligned_cells={} pair_lag_evidence_points_upper_bound={} pc_ci_tests_upper_bound={} evidence_statuses={status_counts:?} estimator_error_codes={error_codes:?} source_records={} latest_event_ns={}",
            readback.artifact.streams.len(),
            readback.artifact.pairs.len(),
            readback.artifact.resource_accounting.aligned_cells,
            readback
                .artifact
                .resource_accounting
                .pair_lag_evidence_points_upper_bound,
            readback
                .artifact
                .resource_accounting
                .pc_ci_tests_upper_bound,
            readback.artifact.source_records,
            readback.artifact.latest_event_ns
        ),
    );
    state
        .last_causal_map_pointer_keys
        .insert(target_id.clone(), readback.pointer_key_hex);
    state
        .last_causal_map_artifact_keys
        .insert(target_id.clone(), readback.graph_key_hex);
    state
        .last_causal_map_artifact_sha256
        .insert(target_id.clone(), readback.graph_value_sha256);
    state.last_causal_map_source_fingerprint_sha256.insert(
        target_id.clone(),
        readback.artifact.source_fingerprint_sha256,
    );
    state
        .last_causal_map_source_records
        .insert(target_id.clone(), readback.artifact.source_records);
    state
        .last_causal_map_latest_event_ns
        .insert(target_id, readback.artifact.latest_event_ns);
    drop(state);
    Ok(())
}

fn drive_scheduled_causal_maps(db: &Arc<Db>) -> Result<(), String> {
    let now_ms = now_unix_ms().ok_or_else(|| "system clock precedes Unix epoch".to_owned())?;
    let interval_ms = u64::try_from(CAUSAL_MAP_REBUILD_INTERVAL.as_millis())
        .map_err(|_| "causal-map rebuild interval exceeds u64 milliseconds".to_owned())?;
    let last = LAST_CAUSAL_MAP_REBUILD_UNIX_MS.load(Ordering::Acquire);
    if last != 0 && now_ms.saturating_sub(last) < interval_ms {
        return Ok(());
    }
    // Latch the attempt before any corpus scan. A reproducible data/schema
    // fault remains visible for the whole refresh interval instead of burning
    // the maintenance pool every five minutes.
    LAST_CAUSAL_MAP_REBUILD_UNIX_MS.store(now_ms, Ordering::Release);
    let wall_now_ns = i64::try_from(now_ms)
        .ok()
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or_else(|| "system time exceeds signed Unix-nanosecond range".to_owned())?;
    let finalization_lag_ns = i64::try_from(CAUSAL_MAP_FINALIZATION_LAG.as_nanos())
        .map_err(|_| "causal-map finalization lag exceeds signed nanoseconds".to_owned())?;
    let until_ns = wall_now_ns
        .checked_sub(finalization_lag_ns)
        .ok_or_else(|| "causal-map finalized upper window bound underflowed i64".to_owned())?;
    let window_ns = i64::try_from(CAUSAL_MAP_WINDOW.as_nanos())
        .map_err(|_| "causal-map window exceeds signed nanoseconds".to_owned())?;
    let since_ns = until_ns
        .checked_sub(window_ns)
        .ok_or_else(|| "causal-map lower window bound underflowed i64".to_owned())?;

    let mut failures = Vec::new();
    for &target in crate::constellations::SYN_CAUSAL_MAP_MAINTENANCE_TARGETS {
        if let Err(error) = drive_one_scheduled_causal_map(db, target, since_ns, until_ns) {
            let target_id = causal_map_target_id(target);
            clear_causal_map_physical_readback(&target_id);
            record_causal_map_action(target_id, format!("failed {error}"));
            failures.push(error);
        }
        if let Err(error) = release_completed_phase_memory("one scheduled causal-map target") {
            failures.push(format!(
                "target={}: release completed target memory: {error}",
                causal_map_target_id(target)
            ));
        }
    }
    let mut state = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.last_causal_map_rebuild_unix_ms = Some(now_ms);
    state.last_causal_map_window_since_ns = Some(since_ns);
    state.last_causal_map_window_until_ns = Some(until_ns);
    state.last_causal_map_finalization_lag_ms =
        u64::try_from(CAUSAL_MAP_FINALIZATION_LAG.as_millis()).ok();
    drop(state);

    if !failures.is_empty() {
        return Err(format!(
            "{} scheduled causal-map target(s) failed while independent targets continued: {}",
            failures.len(),
            failures.join("; ")
        ));
    }
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_CAUSAL_MAP_PASS",
        targets = crate::constellations::SYN_CAUSAL_MAP_MAINTENANCE_TARGETS.len(),
        since_ts_ns = since_ns,
        until_ts_ns = until_ns,
        finalization_lag_ms = CAUSAL_MAP_FINALIZATION_LAG.as_millis(),
        max_records = CAUSAL_MAP_MAX_RECORDS,
        bin_seconds = CAUSAL_MAP_BIN_SECONDS,
        max_lag = CAUSAL_MAP_MAX_LAG,
        fdr_alpha = CAUSAL_MAP_FDR_ALPHA,
        "autonomous causal-map targets were either physically published and independently read or explicitly classified as non-applicable"
    );
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one scheduled pass must preserve shared coverage eligibility, per-panel actions, and aggregate failure evidence"
)]
fn drive_scheduled_kernels(
    db: &Arc<Db>,
    coverage: Option<&crate::panel_coverage::PanelCoverageReport>,
) -> Result<(), String> {
    let coverage = coverage.ok_or_else(|| {
        "current panel coverage census is unavailable; kernel eligibility cannot be inferred from stale state"
            .to_owned()
    })?;
    let now = now_unix_ms().ok_or_else(|| "system clock precedes Unix epoch".to_owned())?;
    let last = LAST_KERNEL_REBUILD_UNIX_MS.load(Ordering::Acquire);
    let interval_ms = u64::try_from(KERNEL_REBUILD_INTERVAL.as_millis())
        .map_err(|_| "kernel rebuild interval exceeds u64 milliseconds".to_owned())?;
    if last != 0 && now.saturating_sub(last) < interval_ms {
        return Ok(());
    }
    // Record the attempt before the expensive work. Re-running a broken
    // all-pairs build every five minutes would starve unrelated maintenance.
    LAST_KERNEL_REBUILD_UNIX_MS.store(now, Ordering::Release);

    let mut failures = Vec::new();
    let mut eligible_targets = 0usize;
    for &(panel_version, content_slot) in crate::constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS
    {
        let panel = coverage
            .panels
            .iter()
            .find(|panel| panel.panel_version == panel_version)
            .ok_or_else(|| {
                format!(
                    "panel={panel_version} slot={content_slot}: current coverage census has no declared panel row"
                )
            })?;
        let ineligible_reason = if !panel.outcome_bearing {
            Some("ineligible_observation_panel")
        } else if panel.active_version_records == 0 {
            Some("ineligible_empty_panel")
        } else if panel.grounded_records == 0 || panel.anchor_kind_records.is_empty() {
            Some("ineligible_no_grounded_domain")
        } else {
            None
        };
        if let Some(reason) = ineligible_reason {
            let mut readback = match DERIVED_STATE_LAST.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            readback.last_kernel_actions.insert(
                panel_version,
                format!(
                    "{reason} active_records={} grounded_records={} anchor_kinds={}",
                    panel.active_version_records,
                    panel.grounded_records,
                    panel.anchor_kind_records.len()
                ),
            );
            drop(readback);
            continue;
        }
        eligible_targets += 1;
        let mut params =
            synapse_calyx::SynapseCalyxKernelRebuildParams::new(panel_version, content_slot);
        params.max_records = KERNEL_REBUILD_MAX_RECORDS;
        // Scheduled kernel maintenance is an explicit background execution
        // class. It must never initialize or reserve the configured CUDA
        // runtime behind a foreground game; a CPU probe failure is a hard,
        // named maintenance failure rather than permission to fall back.
        params.math_execution_class = synapse_calyx::SynapseCalyxMathExecutionClass::BackgroundCpu;
        let report = match db.rebuild_domain_kernels_intelligence(&params) {
            Ok(report) => report,
            Err(error) => {
                failures.push(format!(
                    "panel={panel_version} slot={content_slot}: {error}"
                ));
                continue;
            }
        };
        let min_recall = report
            .domains
            .iter()
            .filter(|domain| domain.built)
            .map(|domain| domain.recall_ratio)
            .reduce(f32::min)
            .unwrap_or(0.0);
        if report.domains_built == 0 {
            failures.push(format!(
                "panel={panel_version} slot={content_slot}: persisted no grounded domain"
            ));
            continue;
        }
        let mut readback = match DERIVED_STATE_LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        readback.last_kernel_actions.insert(
            panel_version,
            format!("built_domains={}", report.domains_built),
        );
        readback
            .last_kernel_recall
            .insert(panel_version, min_recall);
        readback
            .last_kernel_cf_rows
            .insert(panel_version, report.kernel_cf_rows_after);
    }
    let mut readback = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    readback.last_kernel_rebuild_unix_ms = Some(now);
    drop(readback);
    if !failures.is_empty() {
        return Err(format!(
            "{} scheduled kernel target(s) failed while independent targets continued: {}",
            failures.len(),
            failures.join("; ")
        ));
    }
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_KERNEL_REBUILD_PASS",
        targets = crate::constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS.len(),
        eligible_targets,
        max_records = KERNEL_REBUILD_MAX_RECORDS,
        math_execution_class =
            synapse_calyx::SynapseCalyxMathExecutionClass::BackgroundCpu.as_str(),
        "eligible grounding kernels persisted and physically counted; ineligible targets were explicitly classified"
    );
    Ok(())
}

/// Why one tick stopped weaving before it reached the captured vault tip.
///
/// Three stops, deliberately distinguished, because two of them are progress and
/// one of them is a wall. Collapsing them into "the interval did not finish" is
/// what turned a bounded backlog into a permanent one.
#[derive(Debug)]
enum WeaveStop {
    /// The tick's wall clock ran out. Everything before the frontier is woven
    /// and durable; the rest is next tick's work.
    BudgetExhausted { pending_parts: usize },
    /// The bisection needed more bounded parts than one tick may hold.
    MaxParts { pending_parts: usize },
    /// One commit holds more Base identities than the cap, so no sequence split
    /// can separate them. The frontier cannot pass this commit without a
    /// measured cap change — a wall, not a backlog.
    IndivisibleInterval {
        after_seq: u64,
        through_seq: u64,
        detail: String,
    },
}

/// One panel's incremental-weave backlog across ticks (#2085).
///
/// The trend is operational pressure telemetry, not a terminal convergence
/// verdict. A commit sequence has variable cardinality: one old sequence can
/// contain thousands of Base identities while many new sequences contain one
/// identity each. Therefore sequence distance may grow while the consumer is
/// retiring much more real work than ingest creates. The physically verified
/// frontier is the binding progress signal.
#[derive(Clone, Copy, Debug, Default)]
struct WeaveBacklogTrend {
    backlog_seqs: u64,
    consecutive_growth: u32,
}

static WEAVE_BACKLOG_TREND: LazyLock<Mutex<BTreeMap<u32, WeaveBacklogTrend>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Consecutive growing sequence-distance observations after which the advisory
/// explicitly calls out sustained pressure. This never stops a physically
/// advancing durable consumer; zero frontier progress and indivisible
/// over-cap commits are the fail-closed conditions.
const WEAVE_BACKLOG_GROWTH_TICKS_BEFORE_PRESSURE_ADVISORY: u32 = 3;

/// Records the sequence-distance pressure observed after this tick's physically
/// verified frontier advance.
///
/// Returns the backlog trend after this tick.
fn record_weave_backlog(panel_version: u32, backlog_seqs: u64) -> WeaveBacklogTrend {
    let mut trends = match WEAVE_BACKLOG_TREND.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let entry = trends.entry(panel_version).or_default();
    entry.consecutive_growth = if backlog_seqs > entry.backlog_seqs {
        entry.consecutive_growth.saturating_add(1)
    } else {
        0
    };
    entry.backlog_seqs = backlog_seqs;
    let trend = *entry;
    drop(trends);
    trend
}

/// Commits the frontier of contiguously woven MVCC sequences for one panel.
///
/// # Why a frontier and not a completion flag
///
/// The interval queue is consumed in strictly ascending sequence order — a
/// bisection pushes `(after,mid]` ahead of `(mid,through]`, and both ahead of
/// everything already queued — so completed parts form one prefix with no
/// hole. That contiguity is what makes
/// advancing to a partial frontier safe: the invariant the old code actually
/// needed was *never advance past an unprocessed suffix*, and it enforced that
/// with the much stronger *never advance unless the whole interval finished*.
///
/// The stronger rule is what diverged: the target advances every tick while
/// the budget is fixed, so discarding a completed prefix makes the work grow
/// forever.
/// Committing the frontier makes every tick start strictly ahead of the last,
/// which is the difference between a loop that converges and one that cannot.
fn commit_weave_frontier(
    db: &Arc<Db>,
    panel_version: u32,
    after_seq: u64,
    frontier_seq: u64,
) -> Result<(), String> {
    if frontier_seq <= after_seq {
        return Ok(());
    }
    let mut watermarks = match WEAVE_BASE_SEQ.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let state = watermarks
        .get_mut(&panel_version)
        .ok_or_else(|| format!("panel {panel_version} watermark disappeared before commit"))?;
    if state.covered_through_seq != after_seq {
        return Err(format!(
            "panel {panel_version} Base-sequence cursor changed concurrently: expected={after_seq} \
             actual={}; refusing to overwrite a newer owner",
            state.covered_through_seq
        ));
    }
    let bootstrap_through_seq = state
        .bootstrap_through_seq
        .filter(|through| frontier_seq < *through);
    let durable = DurableWeaveCursor {
        schema: ASSOCIATION_WEAVE_CURSOR_SCHEMA.to_owned(),
        panel_version,
        covered_through_seq: frontier_seq,
        bootstrap_through_seq,
        source_contract: ASSOCIATION_WEAVE_SOURCE_CONTRACT.to_owned(),
        updated_at_unix_ms: now_unix_ms(),
    };
    persist_and_verify_weave_cursor(db, &durable)?;
    state.covered_through_seq = frontier_seq;
    state.bootstrap_through_seq = bootstrap_through_seq;
    state.durable_present = true;
    drop(watermarks);
    prune_acknowledged_weave_history(db, panel_version, frontier_seq)
}

/// Weaves every changed Base identity in one panel's new sequence interval
/// without permitting a record cap to become data loss.
///
/// Bounded by a wall-clock budget, and **convergent** under it (#2085): a tick
/// that cannot reach the captured sequence tip commits its frontier and reports
/// the remaining backlog, rather than discarding the work and starting the same
/// growing interval again on the next tick.
/// What one panel's weave achieved, plus any advisory it wants published.
///
/// The advisory is **returned rather than recorded** (#2116). `record_advisory`
/// writes the single-valued `last_advisory_*` fields, so recording it from a
/// worker thread would make "which panel's advisory is the last one" depend on
/// which thread finished first. The driver applies these in fixed panel order
/// instead, which is what keeps a parallel tick's published state identical to a
/// serial tick's.
struct WeaveProgress {
    records_woven: u64,
    backlog_seqs: u64,
    pending_parts: usize,
    advisory: Option<(&'static str, String)>,
}

/// One panel's whole weave sub-pass: the incremental weave and, when it wove
/// anything, the post-ingest drift measurement that consumes it.
///
/// Both halves belong to the same unit because the drift pass is scoped to the
/// same panel and reads what the weave just wrote; splitting them would put a
/// join between two steps that have a real ordering dependency.
struct WeaveSubpass {
    weave: Result<u64, String>,
    advisory: Option<(&'static str, String)>,
    drift_error: Option<String>,
}

fn weave_panel_subpass(db: &Arc<Db>, panel_version: u32) -> WeaveSubpass {
    match drive_incremental_weave(db, panel_version) {
        Ok(progress) => {
            // Every weave-owned corpus, Loom store, write batch, and exact CF
            // count cursor has died when `drive_incremental_weave` returns.
            // Release those pages before the following MMD pass opens another
            // whole-Base cursor. Without this ownership boundary the real
            // unattended tick began drift near 1 GiB and retained each lens's
            // matrix arenas until the next panel scan forced a collection.
            let release = match synapse_calyx::release_process_memory("scheduled incremental weave")
            {
                Ok(release) => release,
                Err(error) => {
                    return WeaveSubpass {
                        weave: Err(format!(
                            "panel {panel_version} committed its incremental weave frontier but could not release dead weave-owned memory before post-ingest drift: {error}"
                        )),
                        advisory: progress.advisory,
                        drift_error: None,
                    };
                }
            };
            tracing::info!(
                code = "STORAGE_DERIVED_STATE_WEAVE_MEMORY_RELEASED",
                panel_version,
                records_woven = progress.records_woven,
                private_bytes_before = release.private_bytes_before,
                private_bytes_after = release.private_bytes_after,
                private_bytes_reclaimed = release.private_bytes_reclaimed,
                release_elapsed_us = release.elapsed_us,
                "released completed weave ownership before starting post-ingest drift"
            );
            // Drift is meaningful only at a settled association frontier. A
            // bootstrap prefix is knowingly incomplete; measuring MMD there
            // both spends a second corpus pass on every recovery chunk and
            // publishes a transient prefix as though it were the panel. The
            // final continuation (backlog=0) measures exactly once.
            let drift_error = if progress.records_woven == 0 || progress.backlog_seqs > 0 {
                if progress.backlog_seqs > 0 {
                    tracing::info!(
                        code = "STORAGE_DERIVED_STATE_DRIFT_DEFERRED_FOR_WEAVE_BACKLOG",
                        panel_version,
                        records_woven = progress.records_woven,
                        backlog_seqs = progress.backlog_seqs,
                        pending_parts = progress.pending_parts,
                        "deferred post-ingest drift until the durable association frontier is settled"
                    );
                }
                None
            } else {
                drive_post_ingest_drift(db, panel_version).err()
            };
            WeaveSubpass {
                weave: Ok(progress.records_woven),
                advisory: progress.advisory,
                drift_error,
            }
        }
        Err(error) => WeaveSubpass {
            weave: Err(error),
            advisory: None,
            drift_error: None,
        },
    }
}

/// Whether the last physical association pass proved that a registered panel
/// still owes CDC sequence coverage.
///
/// Absence is not guessed into backlog: before the first full derived-state
/// pass the normal five-minute cadence performs bootstrap discovery. This
/// predicate becomes true only from numbers published by
/// [`drive_incremental_weave`].
#[must_use]
pub fn association_weave_backlog_pending() -> bool {
    let readback = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    crate::constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS
        .iter()
        .any(|(panel_version, _)| {
            readback
                .last_weave_backlog_seqs
                .get(panel_version)
                .is_some_and(|backlog| *backlog > 0)
                || readback
                    .last_weave_pending_parts
                    .get(panel_version)
                    .is_some_and(|parts| *parts > 0)
        })
}

/// Runs only the durable association consumer while a prior full tick has
/// proved bounded backlog remains.
///
/// This is not a retry of a failed full tick. It omits the unrelated graph,
/// search, coverage, kernel, causal-map, and relay phases that already
/// completed, advances each panel's own durable cursor once, and lets the
/// scheduler restore the normal cadence as soon as both frontiers settle.
/// Every Loom write is still committed and physically read back by the same
/// [`drive_incremental_weave`] path as a normal tick.
///
/// # Errors
///
/// Returns one fail-closed storage error naming every panel sub-pass that did
/// not complete. The scheduler never requests an immediate continuation after
/// this error.
pub fn run_association_weave_catch_up() -> crate::StorageResult<()> {
    hot_context::assert_cold_calyx("maintenance_association_weave_catch_up");
    let source = match DERIVED_STATE_SOURCE.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let db = source.as_ref().and_then(Weak::upgrade).ok_or_else(|| {
        crate::StorageError::WriteFailed {
            cf_name: "storage_derived_state".to_owned(),
            detail: "STORAGE_DERIVED_STATE_SOURCE_UNREGISTERED: a scheduled association catch-up was requested after a prior pass proved backlog, but the registered storage handle is no longer live; reopen storage and let the normal derived-state tick re-establish its durable cursors"
                .to_owned(),
        }
    })?;

    let started = std::time::Instant::now();
    let mut records_woven = 0_u64;
    let mut failures = Vec::new();
    for &(panel_version, _) in crate::constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS {
        let outcome = weave_panel_subpass(&db, panel_version);
        if let Some((code, detail)) = outcome.advisory {
            record_advisory(code, detail);
        }
        match outcome.weave {
            Ok(records) => {
                records_woven = records_woven.saturating_add(records);
                if let Some(error) = outcome.drift_error {
                    failures.push(format!(
                        "panel {panel_version} settled its association frontier but post-ingest drift failed: {error}"
                    ));
                }
            }
            Err(error) => failures.push(format!("panel {panel_version} weave failed: {error}")),
        }
    }
    let backlog_remaining = association_weave_backlog_pending();
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_ASSOCIATION_CATCH_UP_COMPLETED",
        records_woven,
        backlog_remaining,
        failures = failures.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "completed one association-only durable CDC continuation and independently published each panel frontier"
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(crate::StorageError::WriteFailed {
            cf_name: "storage_derived_state".to_owned(),
            detail: format!(
                "STORAGE_DERIVED_STATE_ASSOCIATION_CATCH_UP_FAILED: {} panel sub-pass(es) did not complete: {failures:?}; no failed panel cursor advanced past its last physically verified frontier",
                failures.len()
            ),
        })
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one weave pass: interval bisection, the three stop conditions, frontier commit and \
              convergence classification are a single ordered sequence"
)]
fn drive_incremental_weave(db: &Arc<Db>, panel_version: u32) -> Result<WeaveProgress, String> {
    let mut cursor = {
        let watermarks = match WEAVE_BASE_SEQ.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *watermarks.get(&panel_version).ok_or_else(|| {
            format!(
                "panel {panel_version} has no incremental weave watermark; re-register the live storage source before running maintenance"
            )
        })?
    };
    if !cursor.durable_present {
        let bootstrap = db
            .publish_panel_input_snapshot(panel_version, WEAVE_SNAPSHOT_PUBLISH_MAX_RECORDS)
            .map_err(|error| {
                format!(
                    "publish panel {panel_version} durable association bootstrap stream: {error}"
                )
            })?;
        let bootstrap_through_seq = (bootstrap.through_seq > bootstrap.source_snapshot_seq)
            .then_some(bootstrap.through_seq);
        let durable = DurableWeaveCursor {
            schema: ASSOCIATION_WEAVE_CURSOR_SCHEMA.to_owned(),
            panel_version,
            covered_through_seq: bootstrap.source_snapshot_seq,
            bootstrap_through_seq,
            source_contract: ASSOCIATION_WEAVE_SOURCE_CONTRACT.to_owned(),
            updated_at_unix_ms: now_unix_ms(),
        };
        persist_and_verify_weave_cursor(db, &durable)?;
        let mut watermarks = match WEAVE_BASE_SEQ.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let state = watermarks.get_mut(&panel_version).ok_or_else(|| {
            format!("panel {panel_version} watermark disappeared during durable bootstrap")
        })?;
        if state.durable_present {
            return Err(format!(
                "panel {panel_version} weave cursor was concurrently initialized while its bootstrap stream was being published"
            ));
        }
        *state = WeaveCursorState {
            covered_through_seq: bootstrap.source_snapshot_seq,
            bootstrap_through_seq,
            durable_present: true,
        };
        cursor = *state;
        drop(watermarks);
        tracing::warn!(
            code = "STORAGE_DERIVED_STATE_WEAVE_BOOTSTRAP_PUBLISHED",
            panel_version,
            source_snapshot_seq = bootstrap.source_snapshot_seq,
            bootstrap_through_seq = bootstrap.through_seq,
            identities = bootstrap.identities,
            chunks = bootstrap.chunks,
            base_rows_scanned = bootstrap.base_rows_scanned,
            membership_sha256 = %bootstrap.membership_sha256,
            reader_lease_renewals = bootstrap.reader_lease_renewals,
            "the durable association cursor was absent; published a complete resumable panel snapshot stream instead of silently treating current state as covered"
        );
    }
    let panel_status = db
        .calyx_search_generation_status_for_panel(panel_version, false)
        .map_err(|error| format!("read panel {panel_version} content watermark: {error}"))?;
    let panel_content_seq = panel_status.panel_content_seq.ok_or_else(|| {
        format!("panel {panel_version} status omitted its exact panel_content_seq")
    })?;
    let until_seq = cursor
        .bootstrap_through_seq
        .map_or(panel_content_seq, |bootstrap| {
            bootstrap.max(panel_content_seq)
        });
    let after_seq = cursor.covered_through_seq;
    if until_seq <= after_seq {
        prune_acknowledged_weave_history(db, panel_version, after_seq)?;
        return Ok(WeaveProgress {
            records_woven: 0,
            backlog_seqs: 0,
            pending_parts: 0,
            advisory: None,
        });
    }

    let started = std::time::Instant::now();
    let mut intervals = VecDeque::from([(after_seq, until_seq)]);
    let mut completed_parts = 0usize;
    let mut records_woven = 0u64;
    let mut xterm_rows_written = 0usize;
    let mut graph_rows_written = 0usize;
    let mut last_global_xterm_cf_rows_after = 0usize;
    let mut last_global_graph_cf_rows_after = 0usize;
    // #2114: how the two vault-global counts were obtained on the last
    // completed part.
    // The weave no longer re-walks `XTerm` + `Graph` (2.75 M rows on the
    // deployed vault) after a pass that the commit sequence proves changed
    // nothing, so this log line must say which of the two it is reporting
    // rather than let "readback=physical ... counts" imply a walk that did not
    // run. Starts as `not_woven`: a tick that completes zero interval parts
    // reports no readback at all, which is exactly what happened.
    let mut last_xterm_readback = "not_woven".to_owned();
    let mut last_graph_readback = "not_woven".to_owned();
    let mut walked_readbacks = 0_usize;
    // Inclusive tip of the contiguous sequence prefix woven so far.
    let mut frontier_seq = after_seq;
    let mut stop: Option<WeaveStop> = None;

    while let Some((part_after, part_through)) = intervals.pop_front() {
        if started.elapsed() >= WEAVE_PANEL_TICK_BUDGET {
            stop = Some(WeaveStop::BudgetExhausted {
                pending_parts: intervals.len() + 1,
            });
            break;
        }
        if completed_parts + intervals.len() >= WEAVE_MAX_INTERVAL_PARTS {
            stop = Some(WeaveStop::MaxParts {
                pending_parts: intervals.len() + 1,
            });
            break;
        }

        let mut params = synapse_calyx::SynapseCalyxWeaveParams::new(panel_version);
        params.max_records = WEAVE_INTERVAL_MAX_RECORDS;
        params.after_base_seq = Some(part_after);
        params.through_base_seq = Some(part_through);
        // Scheduled derived-state maintenance is an explicit background class.
        // It must not initialize a CUDA context or reserve GPU resources while
        // the operator is gaming; CPU probe failure remains a hard error.
        params.math_execution_class = synapse_calyx::SynapseCalyxMathExecutionClass::BackgroundCpu;
        let report = match db.weave_panel_intelligence(params) {
            Ok(report) => report,
            Err(error)
                if error.code()
                    == synapse_calyx::SYNAPSE_INTELLIGENCE_DELTA_RECORD_LIMIT_EXCEEDED =>
            {
                if part_through.saturating_sub(part_after) <= 1 {
                    stop = Some(WeaveStop::IndivisibleInterval {
                        after_seq: part_after,
                        through_seq: part_through,
                        detail: error.to_string(),
                    });
                    break;
                }
                let midpoint = part_after + (part_through - part_after) / 2;
                intervals.push_front((midpoint, part_through));
                intervals.push_front((part_after, midpoint));
                continue;
            }
            Err(error) => {
                // The parts already woven are durable and contiguous. Discarding
                // the frontier because a LATER part failed would make every
                // retry redo them, which is the divergence this function was
                // repaired for — the failure itself is unchanged and still loud.
                commit_weave_frontier(db, panel_version, after_seq, frontier_seq)?;
                return Err(format!(
                    "{error}; Base-sequence frontier committed at {frontier_seq} after {completed_parts} \
                     completed interval part(s), so the retry resumes there rather than at \
                     {after_seq}"
                ));
            }
        };

        // #2076: the weave already classifies directionless (zero-norm) slot
        // vectors out of the cosine lane and reports which records they came
        // from, but nothing on the unattended path had ever read that field, so
        // the corpus property that stalled this maintainer for six ticks was
        // invisible in the maintainer's own log. Report it where it happens.
        if !report.knn_zero_norm_exclusions.is_empty() {
            let excluded_records: usize = report
                .knn_zero_norm_exclusions
                .iter()
                .map(|exclusion| exclusion.records)
                .sum();
            let slots = report
                .knn_zero_norm_exclusions
                .iter()
                .map(|exclusion| exclusion.slot.to_string())
                .collect::<Vec<_>>()
                .join(",");
            tracing::info!(
                code = "STORAGE_DERIVED_STATE_WEAVE_ZERO_NORM_EXCLUSIONS",
                panel_version,
                after_base_seq = part_after,
                through_base_seq = part_through,
                excluded_slot_count = report.knn_zero_norm_exclusions.len(),
                excluded_records,
                slots = %slots,
                "slot vectors that measure to exactly zero carry no direction and were classified \
                 out of the geometric lane; they remain measured, and every other lane wove"
            );
        }

        // #2076: the *agreement* lane, which is the one that actually stalled
        // this maintainer. The block above reports the kNN lane, whose exclusion
        // predicate is "the record's whole vector is zero" — structurally never
        // true on `syn-episode-v1`, so it logged nothing on every pass while the
        // agreement lane was skipping tens of thousands of lens pairs. Two
        // lanes, two predicates, two reports.
        if report.agreement_zero_norm_skips > 0 {
            let pairs = report
                .agreement_zero_norm_slot_pairs
                .iter()
                .map(|(a, b)| format!("{a}:{b}"))
                .collect::<Vec<_>>()
                .join(",");
            tracing::info!(
                code = "STORAGE_DERIVED_STATE_WEAVE_AGREEMENT_ZERO_NORM_SKIPS",
                panel_version,
                after_base_seq = part_after,
                through_base_seq = part_through,
                skipped_pairs = report.agreement_zero_norm_skips,
                affected_records = report.agreement_zero_norm_records,
                records_woven = report.records_woven,
                sample_truncated = report.agreement_zero_norm_sample_truncated,
                slot_pairs = %pairs,
                "within-record agreement cross-terms whose operand measures to exactly zero have \
                 no cosine; the named lens pairs were skipped on those records and every other \
                 pair on the same records still wove"
            );
        }

        completed_parts += 1;
        frontier_seq = part_through;
        records_woven = records_woven.saturating_add(report.records_woven as u64);
        xterm_rows_written = xterm_rows_written
            .checked_add(report.cross_terms_materialized)
            .ok_or_else(|| {
                format!(
                    "panel {panel_version} XTerm write-count overflow after sequence interval \
                     ({part_after},{part_through}]: prior={xterm_rows_written} \
                     part={}",
                    report.cross_terms_materialized
                )
            })?;
        let part_graph_rows_written = report
            .agreement_edges_persisted
            .checked_add(report.between_record_edges_persisted)
            .ok_or_else(|| {
                format!(
                    "panel {panel_version} Graph interval write-count overflow for \
                     ({part_after},{part_through}]: agreement={} between_record={}",
                    report.agreement_edges_persisted, report.between_record_edges_persisted
                )
            })?;
        graph_rows_written = graph_rows_written
            .checked_add(part_graph_rows_written)
            .ok_or_else(|| {
                format!(
                    "panel {panel_version} Graph pass write-count overflow after interval \
                     ({part_after},{part_through}]: prior={graph_rows_written} \
                     part={part_graph_rows_written}"
                )
            })?;
        last_global_xterm_cf_rows_after = report.xterm_cf_rows_after;
        last_global_graph_cf_rows_after = report.graph_cf_rows_after;
        for provenance in [
            report.xterm_cf_rows_readback.as_str(),
            report.graph_cf_rows_readback.as_str(),
        ] {
            if provenance.starts_with("walked") {
                walked_readbacks = walked_readbacks.saturating_add(1);
            }
        }
        last_xterm_readback = report.xterm_cf_rows_readback;
        last_graph_readback = report.graph_cf_rows_readback;
    }

    commit_weave_frontier(db, panel_version, after_seq, frontier_seq)?;
    let backlog_seqs = until_seq.saturating_sub(frontier_seq);
    let trend = record_weave_backlog(panel_version, backlog_seqs);
    let action = match &stop {
        None => "interval_complete",
        Some(WeaveStop::BudgetExhausted { .. }) => "interval_partial_budget",
        Some(WeaveStop::MaxParts { .. }) => "interval_partial_max_parts",
        Some(WeaveStop::IndivisibleInterval { .. }) => "interval_blocked_indivisible",
    };
    let pending_parts = match &stop {
        Some(
            WeaveStop::BudgetExhausted { pending_parts } | WeaveStop::MaxParts { pending_parts },
        ) => *pending_parts,
        Some(WeaveStop::IndivisibleInterval { .. }) => 1,
        None => 0,
    };
    {
        let mut readback = match DERIVED_STATE_LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        readback
            .last_weave_actions
            .insert(panel_version, action.to_owned());
        readback
            .last_weave_through_seq
            .insert(panel_version, frontier_seq);
        readback
            .last_weave_records
            .insert(panel_version, records_woven);
        readback
            .last_weave_xterm_rows_written
            .insert(panel_version, xterm_rows_written);
        readback
            .last_weave_graph_rows_written
            .insert(panel_version, graph_rows_written);
        readback
            .last_weave_global_xterm_cf_rows_after
            .insert(panel_version, last_global_xterm_cf_rows_after);
        readback
            .last_weave_global_graph_cf_rows_after
            .insert(panel_version, last_global_graph_cf_rows_after);
        readback
            .last_weave_global_xterm_cf_rows_readback
            .insert(panel_version, last_xterm_readback.clone());
        readback
            .last_weave_global_graph_cf_rows_readback
            .insert(panel_version, last_graph_readback.clone());
        readback
            .last_weave_backlog_seqs
            .insert(panel_version, backlog_seqs);
        readback
            .last_weave_pending_parts
            .insert(panel_version, pending_parts);
        readback
            .last_weave_backlog_growth_ticks
            .insert(panel_version, trend.consecutive_growth);
    }
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_WEAVE_PASS",
        panel_version,
        after_base_seq = after_seq,
        until_base_seq = until_seq,
        frontier_base_seq = frontier_seq,
        action,
        completed_parts,
        pending_parts,
        backlog_seqs,
        backlog_growth_ticks = trend.consecutive_growth,
        records_woven,
        xterm_rows_written,
        graph_rows_written,
        global_xterm_cf_rows_after = last_global_xterm_cf_rows_after,
        global_graph_cf_rows_after = last_global_graph_cf_rows_after,
        global_xterm_cf_rows_readback = %last_xterm_readback,
        global_graph_cf_rows_readback = %last_graph_readback,
        cf_readback_walks = walked_readbacks,
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "readback=per-panel rows durably written plus explicit vault-global XTerm/Graph CF \
         gauges, each either walked on this pass or proved unchanged by an unmoved commit \
         sequence (see *_readback); the cursor now stands at frontier_base_seq"
    );

    // --- Classifying the stop: progress, or a wall (#2085) ---
    //
    // A partial pass that moved the frontier is *progress under a budget*.
    // Commit-sequence distance cannot prove non-convergence because commits have
    // variable record cardinality. Stopping a verified advancing consumer after
    // three growing distance observations permanently strands exactly the
    // recovery backlog this loop owns. A pass that moved nothing, or an
    // indivisible over-cap commit, remains a loud fail-closed wall.
    match stop {
        None => Ok(WeaveProgress {
            records_woven,
            backlog_seqs,
            pending_parts,
            advisory: None,
        }),
        Some(WeaveStop::IndivisibleInterval {
            after_seq: blocked_after,
            through_seq: blocked_through,
            detail,
        }) => Err(format!(
            "panel {panel_version} has an indivisible over-cap Base commit in sequence interval \
             ({blocked_after},{blocked_through}]: {detail}; the frontier is committed at \
             {frontier_seq} so the prefix is not re-woven, but no sequence split can separate \
             one commit; increase the cap only with a measured memory/latency budget"
        )),
        Some(stop) => {
            if frontier_seq <= after_seq {
                return Err(format!(
                    "panel {panel_version} completed ZERO bounded interval parts within \
                     budget_ms={} ({stop:?}); the Base cursor stands at {after_seq} and the backlog \
                     is {backlog_seqs} sequence(s) after {} consecutive ticks of growth, so this \
                     panel is not converging on its own ingest",
                    WEAVE_PANEL_TICK_BUDGET.as_millis(),
                    trend.consecutive_growth,
                ));
            }
            let (advisory_code, advisory_detail) = if trend.consecutive_growth
                >= WEAVE_BACKLOG_GROWTH_TICKS_BEFORE_PRESSURE_ADVISORY
            {
                (
                    "STORAGE_DERIVED_STATE_WEAVE_BACKLOG_PRESSURE",
                    format!(
                        "panel {panel_version} wove {completed_parts} bounded interval part(s), \
                         physically advanced its Base cursor from {after_seq} to {frontier_seq}, \
                         and left {backlog_seqs} sequence(s) across {pending_parts} pending part(s) \
                         ({stop:?}); the sequence-distance gauge has grown on {} consecutive \
                         observations, but commit cardinality is variable, so this is sustained \
                         pressure rather than proof of non-convergence; the successful \
                         completion-relative continuation remains scheduled until the durable \
                         frontier settles",
                        trend.consecutive_growth,
                    ),
                )
            } else {
                (
                    "STORAGE_DERIVED_STATE_WEAVE_BACKLOG",
                    format!(
                        "panel {panel_version} wove {completed_parts} bounded interval part(s) and \
                         advanced its Base cursor from {after_seq} to {frontier_seq}, leaving \
                         {backlog_seqs} sequence(s) of backlog across {pending_parts} pending part(s) \
                         ({stop:?}); the next continuation resumes at the frontier rather than \
                         re-weaving the prefix, so this is bounded catch-up work and not a \
                         maintenance failure"
                    ),
                )
            };
            Ok(WeaveProgress {
                records_woven,
                backlog_seqs,
                pending_parts,
                advisory: Some((advisory_code, advisory_detail)),
            })
        }
    }
}

fn drive_post_ingest_drift(db: &Arc<Db>, panel_version: u32) -> Result<(), String> {
    let mut params = synapse_calyx::SynapseCalyxPanelDriftParams::new(panel_version);
    params.max_records = POST_INGEST_DRIFT_MAX_RECORDS;
    params.math_execution_class = synapse_calyx::SynapseCalyxMathExecutionClass::BackgroundCpu;
    let report = db
        .panel_drift_intelligence(&params)
        .map_err(|error| format!("measure panel {panel_version} post-ingest drift: {error}"))?;
    let mut delivery = ReactiveDeliveryReadback::default();
    if !report.persisted_findings.is_empty() {
        let sink = match REACTIVE_DELIVERY_SINK.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
        .ok_or_else(|| {
            format!(
                "panel {panel_version} persisted {} Reactive drift row(s), but no daemon delivery sink is registered; remediation=repair derived-state startup registration and replay the durable Reactive rows",
                report.drift_rows_persisted
            )
        })?;
        for finding in &report.persisted_findings {
            let readback = sink(finding)?;
            delivery.matched = delivery.matched.saturating_add(readback.matched);
            delivery.queued = delivery.queued.saturating_add(readback.queued);
            delivery.dropped = delivery.dropped.saturating_add(readback.dropped);
        }
    }
    if delivery.dropped > 0 {
        return Err(format!(
            "panel {panel_version} dropped {} subscription delivery item(s) after persisting {} Reactive drift row(s); remediation=consume or recreate the saturated subscription and replay the durable Reactive rows",
            delivery.dropped, report.drift_rows_persisted
        ));
    }
    let mut readback = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    readback
        .last_reactive_drift_rows
        .insert(panel_version, report.drift_rows_persisted);
    readback
        .last_reactive_notifications_matched
        .insert(panel_version, delivery.matched);
    readback
        .last_reactive_notifications_queued
        .insert(panel_version, delivery.queued);
    readback
        .last_reactive_notifications_dropped
        .insert(panel_version, delivery.dropped);
    drop(readback);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_REACTIVE_DRIFT_PASS",
        panel_version,
        records_scanned = report.records_scanned,
        drifted_lenses = report.drifted_lenses,
        drift_rows_persisted = report.drift_rows_persisted,
        math_execution_class = report.math_execution_class,
        math_backend_used = report.math_backend_used,
        notifications_matched = delivery.matched,
        notifications_queued = delivery.queued,
        notifications_dropped = delivery.dropped,
        "scheduled post-ingest drift findings were persisted, read back, and delivered"
    );
    Ok(())
}

fn drive_region_relay(db: &Arc<Db>) -> Result<(), String> {
    const MAX_REGION_DELIVERIES_PER_TICK: usize = 256;
    let after = db
        .region_delivery_cursor()
        .map_err(|error| format!("read durable region delivery cursor: {error}"))?;
    let findings = db
        .persisted_region_findings(after, MAX_REGION_DELIVERIES_PER_TICK)
        .map_err(|error| {
            format!("read persisted Reactive new-region rows after {after}: {error}")
        })?;
    if findings.is_empty() {
        return Ok(());
    }
    let sink = match REGION_DELIVERY_SINK.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
    .ok_or_else(|| {
        format!(
            "{} persisted Reactive new-region row(s) await delivery after seq {after}, but no daemon region sink is registered; remediation=repair derived-state startup registration and rerun maintenance",
            findings.len()
        )
    })?;
    let mut delivery = ReactiveDeliveryReadback::default();
    let mut watermark = after;
    for finding in &findings {
        let readback = sink(finding)?;
        delivery.matched = delivery.matched.saturating_add(readback.matched);
        delivery.queued = delivery.queued.saturating_add(readback.queued);
        delivery.dropped = delivery.dropped.saturating_add(readback.dropped);
        if readback.dropped > 0 {
            return Err(format!(
                "new-region delivery for observed_seq={} dropped {} item(s); watermark remains at {watermark}; remediation=consume or recreate the saturated subscription and rerun maintenance",
                finding.observed_seq, readback.dropped
            ));
        }
        if readback.matched == 0 {
            break;
        }
        watermark = finding.observed_seq;
        watermark = db
            .persist_region_delivery_cursor(watermark)
            .map_err(|error| format!("persist durable region delivery cursor: {error}"))?;
    }
    let mut state = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.last_region_rows_read = findings.len() as u64;
    state.last_region_notifications_matched = delivery.matched;
    state.last_region_notifications_queued = delivery.queued;
    state.last_region_notifications_dropped = delivery.dropped;
    state.last_region_delivery_watermark = watermark;
    drop(state);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_REACTIVE_REGION_PASS",
        rows_read = findings.len(),
        notifications_matched = delivery.matched,
        notifications_queued = delivery.queued,
        notifications_dropped = delivery.dropped,
        delivery_watermark = watermark,
        "persisted exact-identity new-region findings were relayed in durable sequence order"
    );
    Ok(())
}

fn drive_novelty_relay(db: &Arc<Db>) -> Result<(), String> {
    const MAX_DELIVERIES_PER_TICK: usize = 256;
    let after = db
        .novelty_delivery_cursor()
        .map_err(|error| format!("read durable Ward novelty delivery cursor: {error}"))?;
    let findings = db
        .persisted_novelty_findings(after, MAX_DELIVERIES_PER_TICK)
        .map_err(|error| format!("read persisted Ward novelty rows after {after}: {error}"))?;
    if findings.is_empty() {
        return Ok(());
    }
    let sink = match NOVELTY_DELIVERY_SINK.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
    .ok_or_else(|| {
        format!(
            "{} persisted Ward novelty row(s) await delivery after ledger seq {after}, but no daemon novelty sink is registered; remediation=repair startup registration and rerun maintenance",
            findings.len()
        )
    })?;
    let mut aggregate = ReactiveDeliveryReadback::default();
    let mut escalated = 0_u64;
    let mut watermark = after;
    for finding in &findings {
        let readback = sink(finding)?;
        aggregate.matched = aggregate
            .matched
            .saturating_add(readback.notification.matched);
        aggregate.queued = aggregate
            .queued
            .saturating_add(readback.notification.queued);
        aggregate.dropped = aggregate
            .dropped
            .saturating_add(readback.notification.dropped);
        if readback.notification.dropped > 0 {
            return Err(format!(
                "Ward novelty delivery at ledger_seq={} dropped {} item(s); watermark remains at {watermark}; remediation=consume or recreate the saturated subscription and rerun maintenance",
                finding.ledger_seq, readback.notification.dropped
            ));
        }
        if finding.action == "quarantine" && !readback.quarantine_escalated {
            return Err(format!(
                "Ward quarantine at ledger_seq={} was not durably escalated; watermark remains at {watermark}; remediation=repair escalation persistence and rerun maintenance",
                finding.ledger_seq
            ));
        }
        if readback.notification.matched == 0 {
            break;
        }
        escalated = escalated.saturating_add(u64::from(readback.quarantine_escalated));
        watermark = db
            .persist_novelty_delivery_cursor(finding.ledger_seq)
            .map_err(|error| format!("persist durable Ward novelty delivery cursor: {error}"))?;
    }
    let mut state = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.last_novelty_rows_read = findings.len() as u64;
    state.last_novelty_notifications_matched = aggregate.matched;
    state.last_novelty_notifications_queued = aggregate.queued;
    state.last_novelty_notifications_dropped = aggregate.dropped;
    state.last_novelty_delivery_watermark = watermark;
    state.last_novelty_quarantines_escalated = escalated;
    drop(state);
    Ok(())
}

/// Runs only the Ward novelty outbox relay and returns its physical-delivery
/// counters. This is used by the synchronous guard facade so its response does
/// not race the background maintenance interval.
///
/// # Errors
///
/// Returns the structured relay failure when physical outbox read, delivery,
/// escalation, cursor persistence, or readback fails.
pub fn run_novelty_relay_once(db: &Arc<Db>) -> Result<DerivedStateReadback, String> {
    drive_novelty_relay(db)?;
    Ok(derived_state_readback())
}

/// One panel's durable anchor-debt repair state (#1984, #2061).
///
/// Durable, in `CF_KV`, keyed by `(panel generation, backfill source)`. The
/// cursor this replaces was process-local, justified as self-healing: a restart
/// reset it to the head of the CF and re-examined already-current rows rather
/// than skipping rows it had not proven were measured. That reasoning holds for
/// a sweep whose pass is short. It does not hold for one whose pass is the whole
/// corpus — the deployed daemon restarts far more often than a 939,605-row pass
/// completes, so the pass restarted at the head every time and the stranded rows
/// were never reached. A checkpoint that does not survive the process silently
/// unbounds the scan it exists to bound.
///
/// Resumability is not the only thing this row carries. It also holds the
/// per-identity quarantine, and that is what makes refusal safe: an identity an
/// exact attempt proved unrepairable is excluded by name, so the panel keeps
/// repairing its other rows and every other panel keeps its own budget. The
/// refusal that latched the whole maintainer on three outcome anchors was right
/// about the rows and wrong about the scope (#2061).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AnchorDebtRepairState {
    schema: String,
    issue: u32,
    panel_name: String,
    panel_version: u32,
    source_cf: String,
    /// Exclusive resume position in the census's sorted identity queue. `None`
    /// means the next pass starts at the head.
    after_source_cf: Option<String>,
    after_source_key_hex: Option<String>,
    /// Identities an exact attempt proved cannot be re-anchored.
    quarantined: Vec<QuarantinedAnchorIdentity>,
    /// Per-identity failed-attempt tallies, durable across ticks and restarts
    /// (#1984).
    ///
    /// This row is the only place an attempt count can live. The count used to
    /// be read out of `quarantined`, which is written *at* the quarantine
    /// threshold and therefore never holds an identity below it — so every tick
    /// found no entry, reported "attempt 1 of 3", and
    /// `PANEL_ANCHOR_DEBT_IDENTITY_MAX_ATTEMPTS` was unreachable by
    /// construction. The deployed daemon logged `failed on attempt 1 of 3` for
    /// the same `syn-outcome-v1` row on every one of three consecutive ticks
    /// with `quarantined_total=0`: an error repeating forever with no path to
    /// the escape hatch the design had already built.
    ///
    /// `serde(default)` so a v1 row upgrades in place. An absent ledger says
    /// exactly what was true before it existed — nothing has failed yet — rather
    /// than being assumed.
    #[serde(default)]
    attempts: Vec<AnchorDebtIdentityAttempt>,
    passes_completed: u64,
    identities_attempted_total: u64,
    anchors_carried_total: u64,
    updated_at_unix_ms: Option<u64>,
}

/// One identity's durable failed-attempt tally (#1984).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AnchorDebtIdentityAttempt {
    source_cf: String,
    source_key_hex: String,
    attempts: u32,
    first_failed_unix_ms: Option<u64>,
    last_failed_unix_ms: Option<u64>,
    /// The most recent refusal, verbatim. Kept beside the count because "failed
    /// three times" and "failed three times for three different reasons" call
    /// for different remediation, and the quarantine record that eventually
    /// absorbs this entry is written from it.
    last_error: String,
}

impl AnchorDebtRepairState {
    fn new(panel_name: &str, panel_version: u32, source_cf: &str) -> Self {
        Self {
            schema: ANCHOR_DEBT_REPAIR_STATE_SCHEMA.to_owned(),
            issue: 1984,
            panel_name: panel_name.to_owned(),
            panel_version,
            source_cf: source_cf.to_owned(),
            after_source_cf: None,
            after_source_key_hex: None,
            quarantined: Vec::new(),
            attempts: Vec::new(),
            passes_completed: 0,
            identities_attempted_total: 0,
            anchors_carried_total: 0,
            updated_at_unix_ms: None,
        }
    }

    fn quarantines(&self, identity: &StrandedAnchorIdentity) -> bool {
        self.quarantined.iter().any(|entry| {
            entry.source_cf == identity.source_cf && entry.source_key_hex == identity.source_key_hex
        })
    }

    /// Records one failed attempt against `identity` and returns the durable
    /// running total, creating the tally on first failure.
    fn record_failed_attempt(&mut self, identity: &StrandedAnchorIdentity, error: &str) -> u32 {
        if let Some(entry) = self.attempts.iter_mut().find(|entry| {
            entry.source_cf == identity.source_cf && entry.source_key_hex == identity.source_key_hex
        }) {
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_failed_unix_ms = now_unix_ms();
            error.clone_into(&mut entry.last_error);
            return entry.attempts;
        }
        self.attempts.push(AnchorDebtIdentityAttempt {
            source_cf: identity.source_cf.clone(),
            source_key_hex: identity.source_key_hex.clone(),
            attempts: 1,
            first_failed_unix_ms: now_unix_ms(),
            last_failed_unix_ms: now_unix_ms(),
            last_error: error.to_owned(),
        });
        1
    }

    /// Drops any failed-attempt tally for `identity`.
    ///
    /// Called when an attempt succeeds: the tally exists to distinguish a row
    /// that keeps failing from one that failed once under transient pressure,
    /// and a success proves the latter. Leaving it would let three unrelated
    /// transients across three days quarantine a healthy row.
    fn clear_failed_attempts(&mut self, identity: &StrandedAnchorIdentity) {
        self.attempts.retain(|entry| {
            entry.source_cf != identity.source_cf || entry.source_key_hex != identity.source_key_hex
        });
    }

    /// True when `identity` sorts at or before the durable resume position, so
    /// the pass in flight has already attempted it.
    fn already_passed(&self, identity: &StrandedAnchorIdentity) -> bool {
        match (
            self.after_source_cf.as_deref(),
            self.after_source_key_hex.as_deref(),
        ) {
            (Some(source_cf), Some(source_key_hex)) => {
                (
                    identity.source_cf.as_str(),
                    identity.source_key_hex.as_str(),
                ) <= (source_cf, source_key_hex)
            }
            _ => false,
        }
    }
}

/// One exact identity excluded from the work queue, with the evidence (#1984).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct QuarantinedAnchorIdentity {
    source_cf: String,
    source_key_hex: String,
    superseded_panel_version: u32,
    /// What the attempt proved. Never a guess: every entry here was written
    /// after a repair ran against this exact row.
    reason: String,
    attempts: u32,
    first_seen_unix_ms: Option<u64>,
    last_attempt_unix_ms: Option<u64>,
}

impl QuarantinedAnchorIdentity {
    fn describe(&self, panel_name: &str, panel_version: u32) -> String {
        format!(
            "{panel_name}@{panel_version} source_cf={} source_key_hex={} from_generation={} \
             attempts={} reason={}",
            self.source_cf,
            self.source_key_hex,
            self.superseded_panel_version,
            self.attempts,
            self.reason,
        )
    }
}

/// The durable coverage-sweep cursor, one row per `(panel generation, source)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CoverageSweepCursor {
    schema: String,
    issue: u32,
    panel_name: String,
    panel_version: u32,
    source_cf: String,
    /// The opaque exclusive physical Calyx cursor the last page stopped at, hex
    /// encoded because this row is JSON. Passed back unchanged, never
    /// interpreted.
    after_physical_hex: Option<String>,
    sweeps_completed: u64,
    /// Consecutive ticks whose first attempted page of this sweep failed
    /// (#2070).
    ///
    /// Durable because the condition it describes is durable: a page that cannot
    /// be measured because the stored row and a fresh measurement disagree fails
    /// identically on every tick and across every restart. The count is what
    /// turns "this tick failed" into "this panel is blocked", which is the
    /// distinction that lets the scheduler stop spending the head of its queue
    /// on it. Reset to zero by any page that succeeds.
    #[serde(default)]
    consecutive_page_failures: u32,
    /// The most recent page refusal, verbatim, so a blocked panel names *why*
    /// rather than only that it is blocked.
    #[serde(default)]
    last_page_failure: Option<String>,
    #[serde(default)]
    blocked_since_unix_ms: Option<u64>,
    updated_at_unix_ms: Option<u64>,
}

/// Consecutive failed ticks after which a coverage target is deferred behind
/// every healthy one (#2070).
///
/// Three, matching [`PANEL_ANCHOR_DEBT_IDENTITY_MAX_ATTEMPTS`]: the two are the
/// same judgement — "this has now failed often enough to be a property of the
/// work rather than of the moment" — and using one number for both means an
/// operator learns the rule once.
///
/// A blocked target is **still attempted**, last, every tick. It is never
/// silently dropped: the refusal is the loudest evidence there is that the panel
/// needs a version bump or a lens repair, and a sweep that stopped trying would
/// also stop reporting.
const COVERAGE_PAGE_FAILURE_BLOCK_THRESHOLD: u32 = 3;

/// Schema of the durable coverage-sweep cursor. v2 adds the page-failure
/// counters (#2070); v1 rows upgrade in place through `serde(default)` and keep
/// their physical position, because a new key would restart every sweep at the
/// head while looking like a clean start.
const COVERAGE_SWEEP_CURSOR_SCHEMA: &str = "synapse_panel_coverage_sweep_cursor/v2";

/// Whether the un-amortized-primitive finding has already been raised in this
/// process, so it is loud once rather than once per tick.
static ANCHOR_DEBT_UNAMORTIZED_REPORTED: AtomicBool = AtomicBool::new(false);

fn anchor_debt_state_key(panel_version: u32, source_cf: &str) -> Vec<u8> {
    format!("{PANEL_ANCHOR_DEBT_STATE_KEY_PREFIX}{panel_version}/{source_cf}").into_bytes()
}

fn coverage_cursor_key(panel_version: u32, source_cf: &str) -> Vec<u8> {
    format!("{PANEL_COVERAGE_CURSOR_KEY_PREFIX}{panel_version}/{source_cf}").into_bytes()
}

/// Reads one durable maintenance row, failing loudly on undecodable bytes.
///
/// A decode failure is never treated as "no state". Silently starting from the
/// head after failing to read a cursor is exactly the restart amnesia this row
/// exists to remove, and it would do it while looking like a fresh start.
fn read_durable_maintenance_row<T: serde::de::DeserializeOwned>(
    db: &Arc<Db>,
    key: &[u8],
    label: &str,
) -> Result<Option<T>, String> {
    let Some(raw) = db
        .get_cf(cf::CF_KV, key)
        .map_err(|error| format!("read durable {label} row: {error}"))?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|error| format!("decode durable {label} row: {error}"))
}

fn write_durable_maintenance_row<T: Serialize>(
    db: &Arc<Db>,
    key: Vec<u8>,
    value: &T,
    label: &str,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| format!("encode durable {label} row: {error}"))?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(key, encoded)])
        .map_err(|error| format!("persist durable {label} row: {error}"))
}

/// Decodes a census source-key hex back into the authoritative row key.
///
/// The census reports identities as hex because that is how Calyx stores source
/// provenance; the repair needs the bytes. Malformed hex is a corrupt census
/// row, not a row to skip quietly.
fn decode_key_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!(
            "key hex has odd length {}; a physical key is whole bytes",
            hex.len()
        ));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let mut byte = 0u8;
        for digit in pair {
            let value = char::from(*digit).to_digit(16).ok_or_else(|| {
                format!(
                    "key hex contains the non-hex digit {:?}",
                    char::from(*digit)
                )
            })?;
            byte = byte
                .checked_mul(16)
                .and_then(|byte| {
                    u8::try_from(value)
                        .ok()
                        .and_then(|value| byte.checked_add(value))
                })
                .ok_or_else(|| "key hex digit is out of byte range".to_owned())?;
        }
        bytes.push(byte);
    }
    Ok(bytes)
}

/// What one tick's identity-driven anchor-debt phase did.
#[derive(Debug, Default)]
struct AnchorDebtPass {
    identities_attempted: u64,
    identities_carried: u64,
    anchors_carried: u64,
    inserted_rows: u64,
    /// Debt-bearing panels this tick attempted at least one identity on.
    panels_attempted: u64,
    /// Wall clock inside the exact-identity repair primitive only, excluding
    /// every per-panel fixed cost (#2080 defect 3).
    repair_ms: u64,
    panels: Vec<String>,
    passes_completed: Vec<String>,
    quarantined: Vec<String>,
    quarantined_total: u64,
    failed: bool,
}

/// Re-anchors exactly the rows the census named as stranded (#1984, #2061).
///
/// # Why selection, not scanning
///
/// The repair this replaces asked the wrong question. It knew a panel owed 2,120
/// anchors and had no idea which rows they were, so the only move available was
/// to re-measure the whole source CF under a wall-clock budget and hope the
/// stranded rows fell inside it. On the deployed daemon that produced
/// `budget_exhausted` at 44 pages and 60,089 ms with `inserted_rows=0`, every
/// tick, while five panels held their debts unchanged. The corpus is 939,605
/// `Base` rows; the debt was 2,879. Work proportional to the first cannot
/// converge on the second, at any budget — which is why the answer was never a
/// larger budget.
///
/// The census already computed the exact answer and discarded it behind a count.
/// It now carries [`crate::panel_coverage::PanelCoverageRow::anchors_stranded_identities`]
/// — source CF, source key, and the declared superseded generation holding the
/// anchors — and this drives exactly those rows through the exact-identity path
/// `backfill_temporal_metadata` already exposes. Repair cost becomes the debt
/// rather than the corpus, which is the move a visibility map makes for VACUUM:
/// pay for the dirty pages, not for the table.
///
/// # Why every panel, every tick
///
/// One target per tick made an unrepairable head-of-queue item a denial of
/// service: three outcome anchors nothing could re-anchor latched the maintainer
/// and stopped 208,496 rows of coverage backfill for the life of a process
/// (#2061; #2030 was the same structure with a different cause). Debt-
/// proportional repair removes the reason to pick one — every debt-bearing panel
/// gets a share of one bounded budget, and a panel that cannot progress cannot
/// take anyone else's.
///
/// # Why quarantine is per identity
///
/// The refusal this replaces latched a panel on a count comparison, and that
/// count could only change through something the daemon cannot do unattended, so
/// it never cleared. Here the evidence is per row and immediate: an attempt that
/// runs against the exact source row and carries no anchor has *proven* the
/// declared lineage holds nothing for it. That identity is quarantined by name
/// with its reason; the panel's other rows, including ones that appear later,
/// are still repaired.
#[allow(
    clippy::too_many_lines,
    reason = "one repair pass: selection, budget, attempt, quarantine and publication read as one \
              sequence and splitting them would hide the order the guarantees depend on"
)]
fn drive_anchor_debt_repair(
    db: &Arc<Db>,
    report: &crate::panel_coverage::PanelCoverageReport,
    tick_started: std::time::Instant,
) -> AnchorDebtPass {
    let mut pass = AnchorDebtPass::default();
    let targets = report.anchor_debt_targets();
    if targets.is_empty() {
        db.release_temporal_backfill_lineage(None);
        publish_anchor_debt_readback(
            &pass,
            None,
            0,
            crate::backend::AnchorCarryLineageCounters::default(),
            report,
        );
        return pass;
    }
    let started = std::time::Instant::now();
    // Measured across the phase, not inferred from its wall clock: whether the
    // declared-lineage read is amortized is a question about how many indexes
    // were built, and only the backend knows that (#2080 defect 3).
    let lineage_before = crate::backend::anchor_carry_lineage_counters();
    // One shared budget, divided evenly, floor of one: the smallest debt is
    // never crowded out by the largest, and the largest is never starved by the
    // number of panels sharing the tick.
    let per_panel_budget = (PANEL_ANCHOR_DEBT_TICK_IDENTITIES / targets.len()).max(1);
    let mut remaining_global = PANEL_ANCHOR_DEBT_TICK_IDENTITIES;

    for target in targets {
        if remaining_global == 0
            || started.elapsed() >= PANEL_ANCHOR_DEBT_TICK_BUDGET
            || tick_started.elapsed() >= PANEL_BACKFILL_TICK_BUDGET
        {
            break;
        }
        let Some(source_cf) = target.backfill_source_cf.clone() else {
            // `anchor_debt_targets` filtered on this being present, so reaching
            // here means two predicates disagree — a code defect, not a state.
            record_failure(
                "STORAGE_DERIVED_STATE_ANCHOR_DEBT_TARGET_HAS_NO_SOURCE",
                format!(
                    "panel {} was selected for anchor-debt repair but declares no \
                     backfill_source_cf; PanelCoverageRow::anchor_debt_owed and \
                     PanelCoverageReport::anchor_debt_targets disagree",
                    target.panel_name
                ),
            );
            pass.failed = true;
            continue;
        };
        let state_key = anchor_debt_state_key(target.panel_version, &source_cf);
        let mut state = match read_durable_maintenance_row::<AnchorDebtRepairState>(
            db,
            &state_key,
            "anchor-debt repair state",
        ) {
            Ok(Some(state)) => state,
            Ok(None) => {
                AnchorDebtRepairState::new(&target.panel_name, target.panel_version, &source_cf)
            }
            Err(detail) => {
                // Never fall back to a fresh state on a read failure: that
                // discards the quarantine and re-attempts rows already proven
                // unrepairable, while presenting as a clean first pass.
                record_failure(
                    "STORAGE_DERIVED_STATE_ANCHOR_DEBT_STATE_UNREADABLE",
                    format!(
                        "panel {} anchor-debt repair state at CF_KV key {} is unreadable: \
                         {detail}; the repair refuses to start a pass it can neither resume nor \
                         quarantine against",
                        target.panel_name,
                        String::from_utf8_lossy(&state_key)
                    ),
                );
                pass.failed = true;
                continue;
            }
        };

        // Prune quarantine entries the census no longer names as stranded. An
        // identity that left the debt was repaired, or its source row is gone,
        // or the generation moved — none of which are reasons to keep refusing
        // it, and a quarantine that only grows is a repair that decays.
        state.quarantined.retain(|entry| {
            target.anchors_stranded_identities.iter().any(|identity| {
                identity.source_cf == entry.source_cf
                    && identity.source_key_hex == entry.source_key_hex
            })
        });
        // The attempt ledger is pruned on exactly the same predicate, for
        // exactly the same reason: an identity the census no longer names left
        // the debt, so its failure history is about a row that is no longer
        // owed. A tally that only grows would eventually quarantine a row on
        // evidence collected against a different generation.
        state.attempts.retain(|entry| {
            target.anchors_stranded_identities.iter().any(|identity| {
                identity.source_cf == entry.source_cf
                    && identity.source_key_hex == entry.source_key_hex
            })
        });

        let queue: Vec<&StrandedAnchorIdentity> = target
            .anchors_stranded_identities
            .iter()
            .filter(|identity| !state.quarantines(identity))
            .collect();
        let mut pending: Vec<&StrandedAnchorIdentity> = queue
            .iter()
            .copied()
            .filter(|identity| !state.already_passed(identity))
            .collect();
        let mut pass_completed_this_tick = false;
        if pending.is_empty() && !queue.is_empty() {
            // The durable cursor sits past everything this census still names,
            // so the pass covered its whole queue. Record the completion, reset
            // to the head, and keep working in this tick rather than idling.
            state.after_source_cf = None;
            state.after_source_key_hex = None;
            state.passes_completed = state.passes_completed.saturating_add(1);
            pass_completed_this_tick = true;
            pending.clone_from(&queue);
        }

        let budget = per_panel_budget.min(remaining_global).min(pending.len());
        let panel_started = std::time::Instant::now();
        let mut attempted = 0usize;
        let mut carried_rows = 0u64;
        let mut carried_anchors = 0u64;
        let mut inserted = 0u64;
        let mut newly_quarantined = 0usize;
        let mut queue_exhausted = !pending.is_empty();

        let mut decoded = Vec::with_capacity(budget);
        for identity in pending.iter().take(budget) {
            if started.elapsed() >= PANEL_ANCHOR_DEBT_TICK_BUDGET {
                queue_exhausted = false;
                break;
            }
            if state.quarantined.len() >= PANEL_ANCHOR_DEBT_QUARANTINE_CAP {
                record_failure(
                    "STORAGE_DERIVED_STATE_ANCHOR_DEBT_QUARANTINE_FULL",
                    format!(
                        "panel {} holds {} quarantined stranded identities, at the declared cap \
                         {PANEL_ANCHOR_DEBT_QUARANTINE_CAP}; its anchor debt is systematically \
                         unrepairable rather than pointwise, so the repair stops enqueuing it and \
                         reports the panel instead of growing an unbounded refusal list",
                        target.panel_name,
                        state.quarantined.len(),
                    ),
                );
                pass.failed = true;
                queue_exhausted = false;
                break;
            }

            attempted += 1;
            // The cursor advances past every identity the batch touches,
            // including one it refuses, so a pathological row can delay a pass
            // but can never stop it. It is persisted once after the atomic batch.
            state.after_source_cf = Some(identity.source_cf.clone());
            state.after_source_key_hex = Some(identity.source_key_hex.clone());
            match decode_key_hex(&identity.source_key_hex) {
                Ok(source_key) => decoded.push((*identity, source_key)),
                Err(detail) => {
                    record_failure(
                        "STORAGE_DERIVED_STATE_ANCHOR_DEBT_IDENTITY_UNDECODABLE",
                        format!(
                            "panel {} stranded identity source_cf={} source_key_hex={} \
                             from_generation={} does not decode to a physical key: {detail}; the \
                             census row naming it is corrupt",
                            target.panel_name,
                            identity.source_cf,
                            identity.source_key_hex,
                            identity.superseded_panel_version,
                        ),
                    );
                    pass.failed = true;
                    state.quarantined.push(QuarantinedAnchorIdentity {
                        source_cf: identity.source_cf.clone(),
                        source_key_hex: identity.source_key_hex.clone(),
                        superseded_panel_version: identity.superseded_panel_version,
                        reason: "source_key_hex_undecodable".to_owned(),
                        attempts: 1,
                        first_seen_unix_ms: now_unix_ms(),
                        last_attempt_unix_ms: now_unix_ms(),
                    });
                    newly_quarantined += 1;
                }
            }
        }

        if !decoded.is_empty() {
            let source_keys = decoded
                .iter()
                .map(|(_identity, source_key)| source_key.clone())
                .collect::<Vec<_>>();
            let repair_started = std::time::Instant::now();
            let repair_outcomes =
                db.backfill_temporal_metadata_exact_batch(&source_cf, &source_keys);
            pass.repair_ms = pass.repair_ms.saturating_add(
                u64::try_from(repair_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            let outcomes = match repair_outcomes {
                Ok(outcomes) if outcomes.len() == decoded.len() => outcomes,
                Ok(outcomes) => {
                    let error = format!(
                        "exact anchor-debt batch returned {} ordered dispositions for {} requested identities",
                        outcomes.len(),
                        decoded.len()
                    );
                    decoded
                        .iter()
                        .map(|_| {
                            Err(crate::StorageError::ReadFailed {
                                cf_name: source_cf.clone(),
                                detail: error.clone(),
                            })
                        })
                        .collect()
                }
                Err(error) => {
                    let error = error.to_string();
                    decoded
                        .iter()
                        .map(|_| {
                            Err(crate::StorageError::WriteFailed {
                                cf_name: source_cf.clone(),
                                detail: format!("shared exact anchor-debt batch failed: {error}"),
                            })
                        })
                        .collect()
                }
            };

            for ((identity, _source_key), repair_outcome) in decoded.into_iter().zip(outcomes) {
                match repair_outcome {
                    Ok(row) => {
                        inserted = inserted.saturating_add(row.inserted_rows);
                        carried_anchors =
                            carried_anchors.saturating_add(row.anchors_carried_forward);
                        state.clear_failed_attempts(identity);
                        if row.anchors_carried_forward > 0 {
                            carried_rows = carried_rows.saturating_add(1);
                        } else {
                            record_failure(
                                "STORAGE_DERIVED_STATE_ANCHOR_DEBT_IDENTITY_UNREPAIRABLE",
                                format!(
                                    "panel {} stranded identity source_cf={} source_key_hex={} \
                                     from_generation={} was re-measured at generation {} in the \
                                     exact-key batch and its declared anchor lineage carried \
                                     nothing; quarantining this one identity so the remaining \
                                     debt continues; inspect that superseded Base row's anchors \
                                     and their confidence",
                                    target.panel_name,
                                    identity.source_cf,
                                    identity.source_key_hex,
                                    identity.superseded_panel_version,
                                    target.panel_version,
                                ),
                            );
                            pass.failed = true;
                            state.quarantined.push(QuarantinedAnchorIdentity {
                                source_cf: identity.source_cf.clone(),
                                source_key_hex: identity.source_key_hex.clone(),
                                superseded_panel_version: identity.superseded_panel_version,
                                reason: "carry_forward_yielded_no_anchor".to_owned(),
                                attempts: 1,
                                first_seen_unix_ms: now_unix_ms(),
                                last_attempt_unix_ms: now_unix_ms(),
                            });
                            newly_quarantined += 1;
                        }
                    }
                    Err(error) => {
                        let attempts = state.record_failed_attempt(identity, &error.to_string());
                        record_failure(
                            "STORAGE_DERIVED_STATE_ANCHOR_DEBT_IDENTITY_FAILED",
                            format!(
                                "panel {} exact batched anchor-debt repair for source_cf={} \
                                 source_key_hex={} from_generation={} failed on attempt {attempts} \
                                 of {PANEL_ANCHOR_DEBT_IDENTITY_MAX_ATTEMPTS}: {error}",
                                target.panel_name,
                                identity.source_cf,
                                identity.source_key_hex,
                                identity.superseded_panel_version,
                            ),
                        );
                        pass.failed = true;
                        if attempts >= PANEL_ANCHOR_DEBT_IDENTITY_MAX_ATTEMPTS
                            && !state.quarantines(identity)
                        {
                            state.quarantined.push(QuarantinedAnchorIdentity {
                                source_cf: identity.source_cf.clone(),
                                source_key_hex: identity.source_key_hex.clone(),
                                superseded_panel_version: identity.superseded_panel_version,
                                reason: format!(
                                    "batched_repair_failed_after_{attempts}_attempts: {error}"
                                ),
                                attempts,
                                first_seen_unix_ms: now_unix_ms(),
                                last_attempt_unix_ms: now_unix_ms(),
                            });
                            newly_quarantined += 1;
                        }
                    }
                }
            }
        }

        if queue_exhausted && attempted >= pending.len() {
            // The whole enumerated queue was attempted, so the next census
            // measures a complete repair attempt rather than a partial one. A
            // truncated enumeration never reaches here: calling a pass complete
            // over a work queue that was silently shortened would report rows as
            // attempted that were never named.
            if target.anchors_stranded_identities_truncated {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_ANCHOR_DEBT_QUEUE_TRUNCATED",
                    panel = %target.panel_name,
                    panel_version = target.panel_version,
                    stranded = target.anchors_stranded_on_superseded,
                    enumerated = target.anchors_stranded_identities.len(),
                    cap = crate::panel_coverage::SYN_ANCHOR_DEBT_IDENTITY_CAP,
                    "the census named fewer stranded identities than it counted, so no pass over \
                     this panel is called complete; the queue drains across censuses instead"
                );
            } else {
                state.after_source_cf = None;
                state.after_source_key_hex = None;
                state.passes_completed = state.passes_completed.saturating_add(1);
                pass_completed_this_tick = true;
            }
        }
        if pass_completed_this_tick {
            pass.passes_completed.push(format!(
                "{}@{} pass={} stranded_at_pass_end={}",
                target.panel_name,
                target.panel_version,
                state.passes_completed,
                target.anchors_stranded_on_superseded,
            ));
        }
        state.identities_attempted_total = state
            .identities_attempted_total
            .saturating_add(attempted as u64);
        state.anchors_carried_total = state.anchors_carried_total.saturating_add(carried_anchors);
        state.updated_at_unix_ms = now_unix_ms();
        for entry in &state.quarantined {
            pass.quarantined
                .push(entry.describe(&target.panel_name, target.panel_version));
        }
        pass.quarantined_total += state.quarantined.len() as u64;
        if let Err(detail) =
            write_durable_maintenance_row(db, state_key, &state, "anchor-debt repair state")
        {
            record_failure(
                "STORAGE_DERIVED_STATE_ANCHOR_DEBT_CURSOR_UNWRITABLE",
                format!(
                    "panel {} anchor-debt resume cursor could not be persisted: {detail}; without \
                     it a restart repeats work already done and no pass can be proven complete",
                    target.panel_name
                ),
            );
            pass.failed = true;
        }

        let panel_elapsed_ms =
            u64::try_from(panel_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        pass.panels.push(format!(
            "{}@{} stranded={} enumerated={} truncated={} attempted={attempted} \
             carried_rows={carried_rows} carried_anchors={carried_anchors} inserted={inserted} \
             quarantined={} new_quarantine={newly_quarantined} source_absent={} \
             source_cf_unmeasured={} elapsed_ms={panel_elapsed_ms}",
            target.panel_name,
            target.panel_version,
            target.anchors_stranded_on_superseded,
            target.anchors_stranded_identities.len(),
            target.anchors_stranded_identities_truncated,
            state.quarantined.len(),
            target.anchors_stranded_source_absent,
            target.anchors_stranded_source_cf_unmeasured,
        ));
        pass.identities_attempted += attempted as u64;
        pass.identities_carried += carried_rows;
        pass.anchors_carried += carried_anchors;
        pass.inserted_rows += inserted;
        if attempted > 0 {
            pass.panels_attempted += 1;
        }
        remaining_global = remaining_global.saturating_sub(attempted);
    }

    db.release_temporal_backfill_lineage(None);
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let lineage = crate::backend::anchor_carry_lineage_counters().since(lineage_before);
    // --- Per-identity cost, measured against the work it actually describes ---
    //
    // The numerator is the repair primitive only. The old numerator was the
    // whole phase, so every panel's durable state read, quarantine prune and
    // cursor write — costs that exist per panel per tick and not per identity —
    // were divided by the identity count. That ratio necessarily explodes as a
    // debt drains: the same absolute phase cost over 1,000 attempts reads 7 ms
    // and over 2 attempts reads 1,752 ms, which is precisely the "150-500x
    // regression" #2080 measured. Nothing had regressed; the denominator had
    // collapsed while the numerator stayed fixed.
    let ms_per_identity =
        (pass.identities_attempted > 0).then(|| pass.repair_ms / pass.identities_attempted.max(1));
    // The lineage index is built once per (source CF, superseded set). One build
    // per attempted panel is the amortized shape; more than that is the defect
    // the UNAMORTIZED code names, and it is now decided by counting builds
    // rather than by reading a wall-clock ratio that cannot tell the two apart.
    if lineage.builds > pass.panels_attempted
        && !ANCHOR_DEBT_UNAMORTIZED_REPORTED.swap(true, Ordering::Relaxed)
    {
        record_advisory(
            "STORAGE_DERIVED_STATE_ANCHOR_DEBT_REPAIR_UNAMORTIZED",
            format!(
                "the exact-identity anchor repair built the grounded anchor lineage index {} times \
                 across {} attempted panels and {} identities ({} ms of index building out of {} \
                 ms of repair); the index is one per (source CF, superseded generation set), so \
                 more builds than attempted panels means the primitive is rebuilding it per row \
                 and a debt-proportional repair is doing corpus-proportional work; \
                 remediation=widen the CalyxBackend::anchor_carry_lineage cache so interleaved \
                 source CFs cannot evict one another (crates/synapse-storage/src/backend.rs). \
                 This is an ADVISORY: every identity this tick attempted was still repaired or \
                 quarantined with its own evidence, so it does not gate readiness",
                lineage.builds,
                pass.panels_attempted,
                pass.identities_attempted,
                lineage.build_ms,
                pass.repair_ms,
            ),
        );
    }
    // A marginal cost above the ceiling, over a sample large enough for the
    // per-panel fixed cost not to dominate it. Both guards matter: without the
    // first this reports index-build time as if it were per-identity time, and
    // without the second a single-identity tick's fixed cost trips a ceiling
    // that describes steady-state marginal work.
    if let Some(ms_per_identity) = ms_per_identity
        && pass.identities_attempted >= PANEL_ANCHOR_DEBT_COST_ADVISORY_MIN_SAMPLE
        && pass.repair_ms.saturating_sub(lineage.build_ms) / pass.identities_attempted.max(1)
            > PANEL_ANCHOR_DEBT_IDENTITY_MAX_MS
    {
        record_advisory(
            "STORAGE_DERIVED_STATE_ANCHOR_DEBT_REPAIR_COST_ADVISORY",
            format!(
                "exact-identity anchor repair cost {ms_per_identity} ms per identity over {} \
                 identities ({} ms in the repair primitive, {} ms of it building the lineage \
                 index), above the declared marginal ceiling \
                 {PANEL_ANCHOR_DEBT_IDENTITY_MAX_MS} ms; the work queue is correct and every \
                 identity was still attempted, so this is a cost finding about the primitive and \
                 not a maintenance failure",
                pass.identities_attempted, pass.repair_ms, lineage.build_ms,
            ),
        );
    }

    publish_anchor_debt_readback(&pass, ms_per_identity, elapsed_ms, lineage, report);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_ANCHOR_DEBT_PASS",
        identities_attempted = pass.identities_attempted,
        identities_carried = pass.identities_carried,
        anchors_carried = pass.anchors_carried,
        inserted_rows = pass.inserted_rows,
        panels_attempted = pass.panels_attempted,
        quarantined_total = pass.quarantined_total,
        passes_completed = ?pass.passes_completed,
        panels = ?pass.panels,
        unbackfillable_panels = ?report.anchor_debt_unbackfillable_panels,
        ms_per_identity = ?ms_per_identity,
        repair_ms = pass.repair_ms,
        lineage_rebuilds = lineage.builds,
        lineage_reuses = lineage.hits,
        lineage_rebuild_ms = lineage.build_ms,
        elapsed_ms,
        "re-anchored exactly the source identities the census named as stranded"
    );
    pass
}

fn publish_anchor_debt_readback(
    pass: &AnchorDebtPass,
    ms_per_identity: Option<u64>,
    elapsed_ms: u64,
    lineage: crate::backend::AnchorCarryLineageCounters,
    report: &crate::panel_coverage::PanelCoverageReport,
) {
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_anchor_debt_action = Some(
        if report.anchors_stranded_total == 0 {
            "none_owed"
        } else if pass.identities_attempted == 0 {
            // Debt exists but nothing repairable was left to attempt: every
            // remaining identity is quarantined or has no re-measure path. A
            // standing, named condition — never reported as completion.
            "all_remaining_debt_quarantined_or_unbackfillable"
        } else if pass.anchors_carried > 0 {
            "identities_repaired"
        } else {
            "identities_attempted_none_carried"
        }
        .to_owned(),
    );
    guard.last_anchor_debt_panels.clone_from(&pass.panels);
    guard.last_anchor_debt_identities_attempted = pass.identities_attempted;
    guard.last_anchor_debt_identities_carried = pass.identities_carried;
    guard.last_anchor_debt_anchors_carried = pass.anchors_carried;
    guard.last_anchor_debt_inserted_rows = pass.inserted_rows;
    guard
        .last_anchor_debt_passes_completed
        .clone_from(&pass.passes_completed);
    guard
        .last_anchor_debt_quarantined
        .clone_from(&pass.quarantined);
    guard.last_anchor_debt_quarantined_total = pass.quarantined_total;
    guard
        .last_anchor_debt_unbackfillable_panels
        .clone_from(&report.anchor_debt_unbackfillable_panels);
    guard.last_anchor_debt_ms_per_identity = ms_per_identity;
    guard.last_anchor_debt_repair_ms = pass.repair_ms;
    guard.last_anchor_debt_panels_attempted = pass.panels_attempted;
    guard.last_anchor_debt_lineage_rebuilds = lineage.builds;
    guard.last_anchor_debt_lineage_reuses = lineage.hits;
    guard.last_anchor_debt_lineage_rebuild_ms = lineage.build_ms;
    guard.last_anchor_debt_elapsed_ms = Some(elapsed_ms);
}

/// Drives `temporal_backfill` pages for the panel most owed a **coverage**
/// sweep, under the remainder of [`PANEL_BACKFILL_TICK_BUDGET`] (#1927 ask 2).
///
/// The backfill mechanism already existed and was already correct; nothing drove
/// it, so a panel could sit at 1.7% coverage indefinitely with `health`
/// reporting `ok`. This is the driver, and it is shaped exactly like the search
/// generation half above: measure the state, decide, act within a bounded
/// budget, publish what happened.
///
/// Two things changed with #1984/#2061. It no longer competes with anchor debt
/// for one target slot — that is a separate, debt-proportional phase now, so
/// three unrepairable anchors can never again outrank 174,993 uncovered rows.
/// And its cursor is durable, because a process-local one restarted the sweep at
/// the head of the CF after every restart and a corpus-length pass never
/// survived long enough to finish.
///
/// Every exit records a named action. "Nothing was owed" and "the budget ran out
/// mid-corpus" and "the page failed" are three different outcomes and none of
/// them is allowed to look like the others.
///
/// Returns `true` when this pass failed, so the caller can withhold the tick's
/// success. Without that, a backfill failing on every tick would still refresh
/// `last_success_unix_ms` and bump the success counter, and the counters would
/// read as a healthy cadence over a repair that is not happening — the same
/// shape as the `health`-says-ok-while-coverage-is-1.7% failure this whole
/// issue is about.
#[allow(
    clippy::too_many_lines,
    reason = "one sweep: cursor recovery, paging, cursor persistence and publication are a single \
              ordered sequence whose correctness depends on that order"
)]
fn drive_coverage_backfill(
    db: &Arc<Db>,
    report: &crate::panel_coverage::PanelCoverageReport,
    started: std::time::Instant,
    anchor: &AnchorDebtPass,
) -> bool {
    let targets = report.coverage_backfill_targets();
    if targets.is_empty() {
        let action = if !report.anchor_debt_unbackfillable_panels.is_empty() {
            "anchor_debt_unbackfillable"
        } else if report.coverage_deficient_panels.is_empty() {
            "none_owed"
        } else {
            // Deficient, but no panel that is deficient has a re-measure path.
            // Reporting this as "none owed" would be the silent-success failure
            // this whole issue is about.
            "owed_but_unbackfillable"
        };
        publish_backfill_readback(&BackfillReadback {
            action,
            reason: None,
            panel: None,
            source_cf: None,
            sweep_complete: None,
            ..BackfillReadback::empty(anchor)
        });
        // Not a failure: "nothing is owed" and "nothing owed is repairable" are
        // both correct outcomes for this pass. The unrepairable case is a
        // deficiency of the panel catalog, already raised by health, not a
        // failure of this driver.
        return false;
    }

    let mut failed = false;
    let mut ready: Vec<CoverageTarget<'_>> = Vec::new();
    let mut deferred: Vec<CoverageTarget<'_>> = Vec::new();
    let mut blocked_panels: Vec<String> = Vec::new();

    for target in targets {
        let Some(source_cf) = target.backfill_source_cf.clone() else {
            // `coverage_backfill_targets` already filtered on this being
            // present; reaching here means the two predicates disagree, which is
            // a code defect worth shouting about rather than a state worth
            // handling. It disqualifies one target, never the rotation.
            record_failure(
                "STORAGE_DERIVED_STATE_BACKFILL_TARGET_HAS_NO_SOURCE",
                format!(
                    "panel {} was selected as a coverage backfill target but declares no \
                     backfill_source_cf; PanelCoverageRow::coverage_backfill_owed and \
                     PanelCoverageReport::coverage_backfill_targets disagree",
                    target.panel_name
                ),
            );
            failed = true;
            continue;
        };
        let cursor_key = coverage_cursor_key(target.panel_version, &source_cf);
        let persisted = match read_durable_maintenance_row::<CoverageSweepCursor>(
            db,
            &cursor_key,
            "coverage sweep cursor",
        ) {
            Ok(persisted) => persisted,
            Err(detail) => {
                // A cursor that cannot be read must not be silently replaced by
                // a sweep from the head: that is exactly the restart amnesia
                // that kept a corpus-length pass from ever completing, and it
                // would present as a clean start.
                record_failure(
                    "STORAGE_DERIVED_STATE_BACKFILL_CURSOR_UNREADABLE",
                    format!(
                        "panel {} coverage sweep cursor at CF_KV key {} is unreadable: {detail}; \
                         the sweep refuses to restart from the head of {source_cf} on an unproven \
                         position",
                        target.panel_name,
                        String::from_utf8_lossy(&cursor_key)
                    ),
                );
                failed = true;
                continue;
            }
        };
        // A cursor taken under a different panel generation or a different CF
        // says nothing about where this sweep should resume, so it is discarded
        // rather than reused.
        let persisted = persisted.filter(|persisted| {
            persisted.panel_version == target.panel_version && persisted.source_cf == source_cf
        });
        let failures = persisted
            .as_ref()
            .map_or(0, |persisted| persisted.consecutive_page_failures);
        let entry = CoverageTarget {
            target,
            source_cf,
            cursor_key,
            persisted,
        };
        if failures >= COVERAGE_PAGE_FAILURE_BLOCK_THRESHOLD {
            blocked_panels.push(format!(
                "{}@{} source_cf={} consecutive_page_failures={failures} uncovered={:?} \
                 last_failure={}",
                entry.target.panel_name,
                entry.target.panel_version,
                entry.source_cf,
                entry.target.uncovered_rows(),
                entry
                    .persisted
                    .as_ref()
                    .and_then(|persisted| persisted.last_page_failure.as_deref())
                    .unwrap_or("<unrecorded>"),
            ));
            deferred.push(entry);
        } else {
            ready.push(entry);
        }
    }
    if !blocked_panels.is_empty() {
        // Named, not dropped. A blocked panel is still swept — last — every
        // tick, so the refusal keeps being reported and clears itself the moment
        // the underlying lens or panel-version fault is fixed.
        tracing::warn!(
            code = "STORAGE_DERIVED_STATE_COVERAGE_PANEL_BLOCKED",
            threshold = COVERAGE_PAGE_FAILURE_BLOCK_THRESHOLD,
            blocked_panels = ?blocked_panels,
            "coverage targets whose first page has failed on every recent tick are deferred behind \
             every healthy target so their backlog cannot hold the others hostage"
        );
    }
    // Blocked targets go to the back of the rotation, never off it.
    ready.append(&mut deferred);

    let mut totals = CoverageTickTotals::default();
    let mut panel_lines: Vec<String> = Vec::new();
    let mut primary: Option<CoveragePrimary> = None;
    let mut targets_attempted = 0_usize;
    let mut targets_skipped: Vec<String> = Vec::new();
    let owed = ready.len();
    for (index, entry) in ready.iter().enumerate() {
        let elapsed = started.elapsed();
        if elapsed >= PANEL_BACKFILL_TICK_HARD_CEILING {
            // Named, with the reason and the position it was reached at. The
            // only permitted way for `targets_attempted` to fall below
            // `targets_owed` is an entry in this list.
            targets_skipped.push(format!(
                "{}@{} source_cf={} uncovered={:?} reason=tick_hard_ceiling_exhausted \
                 queue_position={} elapsed_ms={}",
                entry.target.panel_name,
                entry.target.panel_version,
                entry.source_cf,
                entry.target.uncovered_rows(),
                index,
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ));
            continue;
        }
        // Reserve every target behind this one its floor, and give this one
        // everything else that is left. The queue is ordered most-owed first,
        // so the largest backlog keeps the bulk of the tick while the smallest
        // is guaranteed to run.
        let behind =
            u32::try_from(owed.saturating_sub(index).saturating_sub(1)).unwrap_or(u32::MAX);
        let reserved_for_the_rest = PANEL_BACKFILL_TARGET_MIN_SLICE.saturating_mul(behind);
        let slice = PANEL_BACKFILL_TICK_BUDGET
            .saturating_sub(elapsed)
            .saturating_sub(reserved_for_the_rest)
            .max(PANEL_BACKFILL_TARGET_MIN_SLICE);
        targets_attempted += 1;
        let pass = sweep_coverage_target(db, entry, started, slice);
        failed |= pass.failed;
        totals.merge(&pass);
        panel_lines.push(pass.line.clone());
        if primary.is_none() {
            primary = Some(CoveragePrimary {
                panel: entry.target.panel_name.clone(),
                source_cf: entry.source_cf.clone(),
                action: pass.action,
                reason: pass.reason,
                sweep_complete: pass.sweep_complete,
            });
        }
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let primary = primary.unwrap_or(CoveragePrimary {
        panel: String::new(),
        source_cf: String::new(),
        action: "budget_exhausted",
        reason: "coverage_debt",
        sweep_complete: false,
    });
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_BACKFILL_PASS",
        action = primary.action,
        panel = %primary.panel,
        source_cf = %primary.source_cf,
        reason = primary.reason,
        targets_owed = owed,
        targets_attempted,
        targets_skipped = ?targets_skipped,
        target_min_slice_ms =
            u64::try_from(PANEL_BACKFILL_TARGET_MIN_SLICE.as_millis()).unwrap_or(u64::MAX),
        panels = ?panel_lines,
        blocked_panels = ?blocked_panels,
        pages = totals.pages,
        examined = totals.examined,
        inserted = totals.inserted,
        already_current = totals.already_current,
        outcome_anchored = totals.outcome_anchored,
        anchors_carried = totals.anchors_carried,
        sweeps_completed = totals.sweeps_completed,
        elapsed_ms,
        "drove the unattended panel coverage backfill toward the active generation across every \
         owed target"
    );

    publish_backfill_readback(&BackfillReadback {
        action: primary.action,
        reason: Some(primary.reason),
        panel: (!primary.panel.is_empty()).then(|| primary.panel.clone()),
        source_cf: (!primary.source_cf.is_empty()).then(|| primary.source_cf.clone()),
        pages: totals.pages,
        examined: totals.examined,
        inserted: totals.inserted,
        already_current: totals.already_current,
        outcome_anchored: totals.outcome_anchored,
        anchors_carried: totals.anchors_carried,
        sweep_complete: Some(primary.sweep_complete),
        elapsed_ms,
        targets_owed: owed as u64,
        targets_attempted: targets_attempted as u64,
        targets_skipped,
        anchor,
    });

    // `page_failed`, `cursor_absent` and an unpersistable cursor are the exits
    // that already called record_failure; report them so the tick is not also
    // counted a success.
    failed
}

/// One coverage target with its durable cursor already recovered (#2070).
struct CoverageTarget<'a> {
    target: &'a crate::panel_coverage::PanelCoverageRow,
    source_cf: String,
    cursor_key: Vec<u8>,
    persisted: Option<CoverageSweepCursor>,
}

/// The tick's aggregate over every target it swept.
#[derive(Default)]
struct CoverageTickTotals {
    pages: u64,
    examined: u64,
    inserted: u64,
    already_current: u64,
    outcome_anchored: u64,
    anchors_carried: u64,
    sweeps_completed: u64,
}

impl CoverageTickTotals {
    fn merge(&mut self, pass: &CoverageTargetPass) {
        self.pages = self.pages.saturating_add(pass.pages);
        self.examined = self.examined.saturating_add(pass.examined);
        self.inserted = self.inserted.saturating_add(pass.inserted);
        self.already_current = self.already_current.saturating_add(pass.already_current);
        self.outcome_anchored = self.outcome_anchored.saturating_add(pass.outcome_anchored);
        self.anchors_carried = self.anchors_carried.saturating_add(pass.anchors_carried);
        self.sweeps_completed = self
            .sweeps_completed
            .saturating_add(u64::from(pass.sweep_complete));
    }
}

/// The highest-priority target actually worked this tick, which is what the
/// single-panel readback fields report.
struct CoveragePrimary {
    panel: String,
    source_cf: String,
    action: &'static str,
    reason: &'static str,
    sweep_complete: bool,
}

/// What one target's sweep did within a tick.
struct CoverageTargetPass {
    action: &'static str,
    reason: &'static str,
    pages: u64,
    examined: u64,
    inserted: u64,
    already_current: u64,
    outcome_anchored: u64,
    anchors_carried: u64,
    sweep_complete: bool,
    failed: bool,
    line: String,
}

/// Sweeps one coverage target from its durable cursor, within the tick budget.
///
/// Extracted from the driver so that a target's failure is a value the driver
/// can record and move past. When this was inline, `break` on a page failure
/// ended the *tick*, and one unmeasurable page therefore denied service to every
/// panel behind it (#2070) — the same head-of-queue defect as #2030 and #2061,
/// on the queue those two fixes handed the work to.
#[allow(
    clippy::too_many_lines,
    reason = "one sweep: cursor recovery, paging, cursor persistence and reporting are a single \
              ordered sequence whose correctness depends on that order"
)]
fn sweep_coverage_target(
    db: &Arc<Db>,
    entry: &CoverageTarget<'_>,
    started: std::time::Instant,
    slice: std::time::Duration,
) -> CoverageTargetPass {
    // This target's own deadline, not the tick's. Reading the tick's clock here
    // is what let the head of the queue spend the whole budget and leave
    // `targets_attempted=1` against `targets_owed=4` (#2061).
    let deadline = std::time::Instant::now() + slice;
    let target = entry.target;
    let source_cf = entry.source_cf.as_str();
    let reason = target.backfill_reason().unwrap_or("coverage_debt");
    let resume_hex = entry
        .persisted
        .as_ref()
        .and_then(|persisted| persisted.after_physical_hex.clone());
    let mut after_physical = match resume_hex.as_deref().map(decode_key_hex).transpose() {
        Ok(after_physical) => after_physical,
        Err(detail) => {
            record_failure(
                "STORAGE_DERIVED_STATE_BACKFILL_CURSOR_UNREADABLE",
                format!(
                    "panel {} coverage sweep cursor holds an undecodable physical position: \
                     {detail}",
                    target.panel_name
                ),
            );
            return CoverageTargetPass {
                action: "cursor_undecodable",
                reason,
                pages: 0,
                examined: 0,
                inserted: 0,
                already_current: 0,
                outcome_anchored: 0,
                anchors_carried: 0,
                sweep_complete: false,
                failed: true,
                line: format!(
                    "{}@{} source_cf={source_cf} action=cursor_undecodable",
                    target.panel_name, target.panel_version
                ),
            };
        }
    };

    let mut pages = 0_u64;
    let mut examined = 0_u64;
    let mut inserted = 0_u64;
    let mut already_current = 0_u64;
    let mut outcome_anchored = 0_u64;
    let mut anchors_carried = 0_u64;
    let mut sweep_complete = false;
    let mut action = "budget_exhausted";
    let mut page_failure: Option<String> = None;

    while std::time::Instant::now() < deadline
        && started.elapsed() < PANEL_BACKFILL_TICK_HARD_CEILING
    {
        let page = match db.backfill_temporal_metadata(
            source_cf,
            None,
            after_physical.as_deref(),
            PANEL_BACKFILL_PAGE_ROWS,
        ) {
            Ok(page) => page,
            Err(error) => {
                // The cursor is left exactly where it was. A failing page must
                // be retried from the same offset next tick, never skipped:
                // advancing past a page that did not complete would leave rows
                // unmeasured with nothing recording that they were passed over.
                // What changed in #2070 is only the SCOPE of the abandonment:
                // this target stops, the tick does not.
                record_failure(
                    "STORAGE_DERIVED_STATE_BACKFILL_PAGE_FAILED",
                    format!(
                        "backfill page for panel {} from {source_cf}: {error}",
                        target.panel_name
                    ),
                );
                page_failure = Some(error.to_string());
                action = "page_failed";
                break;
            }
        };
        pages += 1;
        examined += page.examined_rows;
        inserted += page.inserted_rows;
        already_current += page.already_current_rows;
        outcome_anchored += page.outcome_anchored_rows;
        anchors_carried += page.anchors_carried_forward;

        if page.more {
            if let Some(resume) = page.resume_after_physical {
                after_physical = Some(resume);
            } else {
                // `more` without a resume cursor would make the next page
                // restart from the beginning and loop forever.
                record_failure(
                    "STORAGE_DERIVED_STATE_BACKFILL_CURSOR_ABSENT",
                    format!(
                        "backfill page for {source_cf} reported more=true with no \
                         resume_after_physical; the sweep cannot advance"
                    ),
                );
                action = "cursor_absent";
                break;
            }
        } else {
            sweep_complete = true;
            action = "sweep_complete";
            break;
        }
    }

    let previous = entry.persisted.as_ref();
    let sweeps_completed = previous
        .map_or(0, |persisted| persisted.sweeps_completed)
        .saturating_add(u64::from(sweep_complete));
    // A page that succeeded proves the panel is measurable, so the failure run
    // resets. Only an unbroken run of failed ticks blocks a target, which is
    // what keeps one bad afternoon from deprioritising a healthy panel.
    let consecutive_page_failures = if page_failure.is_some() {
        previous
            .map_or(0, |persisted| persisted.consecutive_page_failures)
            .saturating_add(1)
    } else if pages > 0 {
        0
    } else {
        previous.map_or(0, |persisted| persisted.consecutive_page_failures)
    };
    let blocked_since_unix_ms =
        if consecutive_page_failures >= COVERAGE_PAGE_FAILURE_BLOCK_THRESHOLD {
            previous
                .and_then(|persisted| persisted.blocked_since_unix_ms)
                .or_else(now_unix_ms)
        } else {
            None
        };
    let cursor = CoverageSweepCursor {
        schema: COVERAGE_SWEEP_CURSOR_SCHEMA.to_owned(),
        issue: 1984,
        panel_name: target.panel_name.clone(),
        panel_version: target.panel_version,
        source_cf: source_cf.to_owned(),
        // A completed sweep resets to the head of the CF. That is the correct
        // posture after a generation bump: the rows re-measured first are the
        // oldest, which are the ones no live write will ever reach.
        after_physical_hex: if sweep_complete {
            None
        } else {
            after_physical
                .as_deref()
                .map(crate::constellations::hex_encode)
        },
        sweeps_completed,
        consecutive_page_failures,
        last_page_failure: page_failure
            .or_else(|| previous.and_then(|persisted| persisted.last_page_failure.clone()))
            .filter(|_| consecutive_page_failures > 0),
        blocked_since_unix_ms,
        updated_at_unix_ms: now_unix_ms(),
    };
    let cursor_persisted = write_durable_maintenance_row(
        db,
        entry.cursor_key.clone(),
        &cursor,
        "coverage sweep cursor",
    );
    if let Err(detail) = &cursor_persisted {
        record_failure(
            "STORAGE_DERIVED_STATE_BACKFILL_CURSOR_UNWRITABLE",
            format!(
                "panel {} coverage sweep cursor could not be persisted: {detail}; the next tick \
                 would restart at the head of {source_cf} and the sweep would never complete",
                target.panel_name
            ),
        );
    }
    let failed = cursor_persisted.is_err() || matches!(action, "page_failed" | "cursor_absent");

    CoverageTargetPass {
        action,
        reason,
        pages,
        examined,
        inserted,
        already_current,
        outcome_anchored,
        anchors_carried,
        sweep_complete,
        failed,
        line: format!(
            "{}@{} source_cf={source_cf} action={action} reason={reason} \
             coverage_fraction={:?} uncovered={:?} pages={pages} examined={examined} \
             inserted={inserted} already_current={already_current} \
             outcome_anchored={outcome_anchored} anchors_carried={anchors_carried} \
             sweep_complete={sweep_complete} sweeps_completed={sweeps_completed} \
             consecutive_page_failures={consecutive_page_failures} slice_ms={}",
            target.panel_name,
            target.panel_version,
            target.coverage_fraction,
            target.uncovered_rows(),
            u64::try_from(slice.as_millis()).unwrap_or(u64::MAX),
        ),
    }
}

/// The coverage sweep's published outcome, plus the anchor phase sharing its
/// tick.
struct BackfillReadback<'a> {
    action: &'a str,
    reason: Option<&'a str>,
    panel: Option<String>,
    source_cf: Option<String>,
    pages: u64,
    examined: u64,
    inserted: u64,
    already_current: u64,
    outcome_anchored: u64,
    anchors_carried: u64,
    sweep_complete: Option<bool>,
    elapsed_ms: u64,
    /// Owed vs swept, published so the two can be compared without reading the
    /// log (#2061 ask 3).
    targets_owed: u64,
    targets_attempted: u64,
    targets_skipped: Vec<String>,
    anchor: &'a AnchorDebtPass,
}

impl<'a> BackfillReadback<'a> {
    const fn empty(anchor: &'a AnchorDebtPass) -> Self {
        Self {
            action: "none_owed",
            reason: None,
            panel: None,
            source_cf: None,
            pages: 0,
            examined: 0,
            inserted: 0,
            already_current: 0,
            outcome_anchored: 0,
            anchors_carried: 0,
            sweep_complete: None,
            elapsed_ms: 0,
            targets_owed: 0,
            targets_attempted: 0,
            targets_skipped: Vec::new(),
            anchor,
        }
    }
}

/// Publishes both phases through the one action field `health` already prints.
///
/// Compound rather than split, because this readback is where an operator sees
/// what the maintainer did, and a single-phase string is how a tick that carried
/// a thousand stranded anchors would still read as `budget_exhausted`.
fn publish_backfill_readback(readback: &BackfillReadback<'_>) {
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let anchor = readback.anchor;
    guard.last_backfill_action = Some(format!(
        "anchor_debt={} coverage={}",
        if anchor.identities_attempted == 0 {
            "no_identities_attempted".to_owned()
        } else {
            format!(
                "carried_{}_anchors_over_{}_identities",
                anchor.anchors_carried, anchor.identities_attempted
            )
        },
        readback.action,
    ));
    guard.last_backfill_reason = Some(format!(
        "anchor_debt_quarantined={} coverage={}",
        anchor.quarantined_total,
        readback.reason.unwrap_or("none"),
    ));
    guard.last_backfill_panel.clone_from(&readback.panel);
    guard
        .last_backfill_source_cf
        .clone_from(&readback.source_cf);
    guard.last_backfill_pages = Some(readback.pages);
    guard.last_backfill_examined_rows = Some(readback.examined);
    // Both phases insert constellations: the sweep when it reaches an unmeasured
    // row, the exact-identity repair when the stranded row is missing at the
    // active generation entirely.
    guard.last_backfill_inserted_rows =
        Some(readback.inserted.saturating_add(anchor.inserted_rows));
    guard.last_backfill_already_current_rows = Some(readback.already_current);
    guard.last_backfill_outcome_anchored_rows = Some(readback.outcome_anchored);
    guard.last_backfill_anchors_carried_forward = Some(
        readback
            .anchors_carried
            .saturating_add(anchor.anchors_carried),
    );
    guard.last_backfill_elapsed_ms = Some(readback.elapsed_ms);
    guard.last_backfill_sweep_complete = readback.sweep_complete;
    guard.last_backfill_targets_owed = readback.targets_owed;
    guard.last_backfill_targets_attempted = readback.targets_attempted;
    guard
        .last_backfill_targets_skipped
        .clone_from(&readback.targets_skipped);
}

/// Runs both panel repairs for one maintenance tick (#1927 ask 2, #1984, #2061).
///
/// Order and independence are both load-bearing. Anchor debt runs first because
/// it is bounded by the debt rather than by the corpus, so it costs a small,
/// predictable slice of the tick; coverage runs second with the remainder, and
/// runs *whatever* the anchor phase did. An unrepairable anchor can no longer
/// return before the coverage sweep is reached, which is the fault that stopped
/// 208,496 rows of backfill for the life of a process.
fn drive_panel_backfill(db: &Arc<Db>, report: &crate::panel_coverage::PanelCoverageReport) -> bool {
    let started = std::time::Instant::now();
    let anchor = drive_anchor_debt_repair(db, report, started);
    let coverage_failed = drive_coverage_backfill(db, report, started, &anchor);
    anchor.failed || coverage_failed
}
