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

/// Lifecycle tasks claimed per five-minute maintenance tick. Each task fully
/// decodes and re-measures an authoritative row, so the bound controls both IO
/// and CPU independently of the older temporal-metadata page size.
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
pub const PANEL_BACKFILL_TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

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

/// Identities repaired between durable cursor writes.
///
/// The cursor is what makes progress survive a restart, so it is written often
/// enough that a crash costs at most this many repeated (idempotent) repairs,
/// and rarely enough that it is not a write per row.
const PANEL_ANCHOR_DEBT_CURSOR_PERSIST_EVERY: usize = 32;

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

/// Wall-clock budget for one panel's incremental weave in one maintenance tick.
pub const WEAVE_PANEL_TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// Kernel construction includes all-pairs graph work, so it runs daily rather
/// than on every five-minute maintenance tick.
pub const KERNEL_REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_hours(24);
pub const KERNEL_REBUILD_MAX_RECORDS: usize = 2_000;

/// Maximum bisection work items accepted for one panel interval.
const WEAVE_MAX_INTERVAL_PARTS: usize = 1_024;

static DERIVED_STATE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_SUCCESS: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_FAILURE: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_SKIPPED: AtomicU64 = AtomicU64::new(0);
static LAST_KERNEL_REBUILD_UNIX_MS: AtomicU64 = AtomicU64::new(0);

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

/// Exclusive end of the last completely woven ingest interval per panel.
///
/// Registration initializes these watermarks before the daemon accepts live
/// writes. A restart therefore cannot create an unwoven live-ingest gap: the
/// replacement process starts a fresh interval at registration, while all
/// historical rows remain available to the explicit full-corpus weave.
static WEAVE_WATERMARK_NS: LazyLock<Mutex<BTreeMap<u32, i64>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Externally readable outcome of the derived-state maintainer, as published for
/// `health` to read without touching the vault.
#[derive(Clone, Debug, Default)]
pub struct DerivedStateReadback {
    pub attempts_total: u64,
    pub success_total: u64,
    pub failure_total: u64,
    pub skipped_total: u64,
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
    pub last_anchor_debt_ms_per_identity: Option<u64>,
    pub last_anchor_debt_elapsed_ms: Option<u64>,
    /// Last incremental Loom action and physical readback (#1671).
    pub last_weave_actions: BTreeMap<u32, String>,
    pub last_weave_until_ns: BTreeMap<u32, i64>,
    pub last_weave_records: BTreeMap<u32, u64>,
    pub last_weave_xterm_rows: BTreeMap<u32, usize>,
    pub last_weave_graph_rows: BTreeMap<u32, usize>,
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
    let registered_at_ns = now_unix_ms()
        .and_then(|value| value.checked_mul(1_000_000))
        .and_then(|value| i64::try_from(value).ok());
    if let Some(registered_at_ns) = registered_at_ns {
        let mut watermarks = match WEAVE_WATERMARK_NS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for panel_version in [
            crate::constellations::SYN_TIMELINE_PANEL_VERSION,
            crate::constellations::SYN_EPISODE_PANEL_VERSION,
            crate::constellations::SYN_AGENT_EVENT_PANEL_VERSION,
        ] {
            watermarks.entry(panel_version).or_insert(registered_at_ns);
        }
    } else {
        record_failure(
            "STORAGE_DERIVED_STATE_WEAVE_CLOCK_INVALID",
            "system time could not be represented as signed Unix nanoseconds while initializing incremental Loom watermarks; unattended weave remains disabled until a valid clock is observed".to_owned(),
        );
    }
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
    readback.refresh_delta_keys_threshold = SEARCH_GENERATION_REFRESH_DELTA_KEYS;
    readback.min_rebuild_interval_ms = SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS;
    readback
}

/// Runs the same bounded pass owned by the periodic derived-state task and
/// returns the process-published readback after it completes.
///
/// This is the operator/FSV seam for proving a scheduled pass without changing
/// its production interval or constructing a second implementation.
pub fn run_derived_state_maintenance_once() -> DerivedStateReadback {
    run_derived_state_maintenance();
    derived_state_readback()
}

fn now_unix_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
}

fn record_skip(code: &'static str, detail: String) {
    DERIVED_STATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(code, detail, "derived-state maintenance pass skipped");
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_unix_ms = now_unix_ms();
    guard.last_skip_code = Some(code.to_owned());
    guard.last_skip_detail = Some(detail);
}

fn record_failure(code: &'static str, detail: String) {
    DERIVED_STATE_FAILURE.fetch_add(1, Ordering::Relaxed);
    tracing::error!(
        code,
        detail,
        "unattended derived-state maintenance failed; the derived layer it maintains stays at \
         whatever state it was already in, and health reports that state"
    );
    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_unix_ms = now_unix_ms();
    guard.last_failure_code = Some(code.to_owned());
    guard.last_failure_detail = Some(detail);
    guard.last_failure_unix_ms = now_unix_ms();
}

/// Runs one unattended derived-state maintenance pass.
///
/// The two halves are independent on purpose: a search-generation rebuild that
/// fails must not stop lens coverage from being measured and reported, because
/// they answer different questions and an operator needs both. Each half records
/// its own failure.
pub(crate) fn run_derived_state_maintenance() {
    // Hot-path boundary (#1686): every one of these reads is off-runtime
    // intelligence work and must never be driven from a tagged reflex tick.
    hot_context::assert_cold_calyx("maintenance_derived_state");
    DERIVED_STATE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
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
        return;
    };

    let mut any_failed = false;
    let mut current_panel_coverage = None;

    if let Err(error) = drive_app_transition_graph(&db) {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_APP_GRAPH_FAILED", error.to_string());
    }
    if let Err(error) = drive_agent_spawn_graph(&db) {
        any_failed = true;
        record_failure(
            "STORAGE_DERIVED_STATE_AGENT_GRAPH_FAILED",
            error.to_string(),
        );
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
        Ok(sweep) => {
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

    // --- Incremental Loom weave (#1671) ---
    //
    // This runs after the coverage census so it never delays the cheaper
    // structural health signals above. Each target owns an independent
    // watermark; one failed panel cannot advance itself or suppress the other
    // two panels' work.
    for panel_version in [
        crate::constellations::SYN_TIMELINE_PANEL_VERSION,
        crate::constellations::SYN_EPISODE_PANEL_VERSION,
        crate::constellations::SYN_AGENT_EVENT_PANEL_VERSION,
    ] {
        match drive_incremental_weave(&db, panel_version) {
            Ok(0) => {}
            Ok(_) => {
                if let Err(error) = drive_post_ingest_drift(&db, panel_version) {
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

    if let Err(error) = drive_scheduled_kernels(&db, current_panel_coverage.as_ref()) {
        any_failed = true;
        record_failure("STORAGE_DERIVED_STATE_KERNEL_REBUILD_FAILED", error);
    }

    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_unix_ms = now_unix_ms();
    if !any_failed {
        DERIVED_STATE_SUCCESS.fetch_add(1, Ordering::Relaxed);
        guard.last_success_unix_ms = now_unix_ms();
        guard.last_skip_code = None;
        guard.last_skip_detail = None;
    }
}

fn drive_app_transition_graph(db: &Db) -> crate::StorageResult<()> {
    let mut lease = db.pin_cf_physical_scan(cf::CF_TIMELINE, crate::COHERENT_SCAN_MAX_AGE_MS)?;
    let source_seq = lease.snapshot_seq;
    let mut previous = None::<String>;
    let mut counts = BTreeMap::<(String, String), u64>::new();
    let mut hierarchy_paths = std::collections::BTreeSet::<String>::new();
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
            let Some(app) = record.app.filter(|value| !value.trim().is_empty()) else {
                continue;
            };
            if let Some(from) = previous.replace(app.clone())
                && from != app
            {
                *counts.entry((from, app)).or_default() += 1;
            }
        }
        if !page.more {
            break;
        }
    }
    db.release_coherent_scan(&mut lease)?;
    drive_path_hierarchy(
        db,
        source_seq,
        lease.read_at_unix_ms,
        hierarchy_paths.into_iter().collect(),
    )?;
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
        lease.read_at_unix_ms,
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
    paths: Vec<String>,
) -> crate::StorageResult<()> {
    if paths.is_empty() {
        tracing::debug!(
            code = "STORAGE_DERIVED_STATE_PATH_GRAPH_INELIGIBLE",
            source_seq,
            "coherent timeline snapshot has no document or URL hierarchy paths"
        );
        return Ok(());
    }
    let transitions = crate::constellations::path_hierarchy_transitions(&paths)?;
    let fingerprint = crate::constellations::graph_snapshot_fingerprint(&transitions);
    if lifecycle_has_snapshot(db, SYN_PATH_HIERARCHY_PANEL_VERSION, fingerprint)? {
        return Ok(());
    }
    let readback = db
        .publish_path_hierarchy_snapshot(
            source_seq,
            now_unix_ms().unwrap_or(read_at_unix_ms),
            &paths,
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

fn drive_agent_spawn_graph(db: &Db) -> crate::StorageResult<()> {
    let mut lease =
        db.pin_cf_physical_scan(cf::CF_AGENT_EVENTS, crate::COHERENT_SCAN_MAX_AGE_MS)?;
    let source_seq = lease.snapshot_seq;
    let read_at_unix_ms = lease.read_at_unix_ms;
    let mut counts = BTreeMap::<(String, String), u64>::new();
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
            if record.kind != AgentEventKind::SpawnRequested {
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
                continue;
            };
            if session_id.trim().is_empty() || spawn_id.trim().is_empty() {
                continue;
            }
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
    let (process_source_seq, process_read_at_unix_ms) =
        collect_process_parent_edges(db, &mut counts)?;
    let source_seq = source_seq.max(process_source_seq);
    let read_at_unix_ms = read_at_unix_ms.max(process_read_at_unix_ms);
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
        "scheduled process and agent-spawn graph snapshot was atomically published and physically read back"
    );
    Ok(())
}

fn collect_process_parent_edges(
    db: &Db,
    counts: &mut BTreeMap<(String, String), u64>,
) -> crate::StorageResult<(u64, u64)> {
    let mut lease =
        db.pin_cf_physical_scan(cf::CF_PROCESS_HISTORY, crate::COHERENT_SCAN_MAX_AGE_MS)?;
    let source_seq = lease.snapshot_seq;
    let read_at_unix_ms = lease.read_at_unix_ms;
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
            let edge = match process_parent_edge(object, &key) {
                Ok(edge) => edge,
                Err(error) => {
                    let _ = db.release_coherent_scan(&mut lease);
                    return Err(error);
                }
            };
            let Some((parent_pid, pid)) = edge else {
                continue;
            };
            if pid == 0 || parent_pid == 0 || pid == parent_pid {
                continue;
            }
            *counts
                .entry((format!("process:{parent_pid}"), format!("process:{pid}")))
                .or_default() += 1;
        }
        if !page.more {
            break;
        }
    }
    db.release_coherent_scan(&mut lease)?;
    Ok((source_seq, read_at_unix_ms))
}

fn process_parent_edge(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &[u8],
) -> crate::StorageResult<Option<(u64, u64)>> {
    let pid = exact_json_u64(object.get("pid"), "pid", key)?;
    let parent_pid = exact_json_u64(
        object
            .get("parent_pid")
            .or_else(|| object.get("ppid"))
            .or_else(|| object.get("inherited_from_pid")),
        "parent_pid|ppid|inherited_from_pid",
        key,
    )?;
    Ok(pid
        .zip(parent_pid)
        .map(|(pid, parent_pid)| (parent_pid, pid)))
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
    for &(panel_version, content_slot) in crate::constellations::SYN_KERNEL_MAINTENANCE_TARGETS {
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
            continue;
        }
        eligible_targets += 1;
        let mut params =
            synapse_calyx::SynapseCalyxKernelRebuildParams::new(panel_version, content_slot);
        params.max_records = KERNEL_REBUILD_MAX_RECORDS;
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
        targets = crate::constellations::SYN_KERNEL_MAINTENANCE_TARGETS.len(),
        eligible_targets,
        max_records = KERNEL_REBUILD_MAX_RECORDS,
        "eligible grounding kernels persisted and physically counted; ineligible targets were explicitly classified"
    );
    Ok(())
}

fn calyx_time_boundary_ns_now() -> Result<i64, String> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?;
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| "system time exceeds signed millisecond range".to_owned())?;
    millis
        .checked_mul(1_000_000)
        .ok_or_else(|| "system time exceeds signed nanosecond range".to_owned())
}

/// Weaves every record in one panel's new ingest interval without permitting a
/// record cap to become data loss.
fn drive_incremental_weave(db: &Arc<Db>, panel_version: u32) -> Result<u64, String> {
    // Constellation `created_at` is millisecond-granular. Both ends must be
    // aligned to that same grid: a fractional watermark would exclude records
    // created later in the same millisecond but carrying the same stored stamp.
    // The current millisecond stays open and is picked up on the next tick.
    let until_ns = calyx_time_boundary_ns_now()?;
    let since_ns = {
        let watermarks = match WEAVE_WATERMARK_NS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *watermarks.get(&panel_version).ok_or_else(|| {
            format!(
                "panel {panel_version} has no incremental weave watermark; re-register the live storage source before running maintenance"
            )
        })?
    };
    if until_ns == since_ns {
        return Ok(0);
    }
    if until_ns < since_ns {
        return Err(format!(
            "regressed system clock at weave boundary: since_ns={since_ns} until_ns={until_ns}"
        ));
    }

    let started = std::time::Instant::now();
    let mut intervals = VecDeque::from([(since_ns, until_ns)]);
    let mut completed_parts = 0usize;
    let mut records_woven = 0u64;
    let mut last_xterm_rows = 0usize;
    let mut last_graph_rows = 0usize;

    while let Some((part_since, part_until)) = intervals.pop_front() {
        if started.elapsed() >= WEAVE_PANEL_TICK_BUDGET {
            return Err(format!(
                "weave budget exhausted after {completed_parts} completed interval part(s); watermark remains at {since_ns} so the entire interval is retried; budget_ms={} pending_parts={}",
                WEAVE_PANEL_TICK_BUDGET.as_millis(),
                intervals.len() + 1
            ));
        }
        if completed_parts + intervals.len() >= WEAVE_MAX_INTERVAL_PARTS {
            return Err(format!(
                "weave interval required more than {WEAVE_MAX_INTERVAL_PARTS} bounded parts; watermark remains at {since_ns}; narrow the maintenance interval or raise the record cap only after measuring association cost"
            ));
        }

        let mut params = synapse_calyx::SynapseCalyxWeaveParams::new(panel_version);
        params.max_records = WEAVE_INTERVAL_MAX_RECORDS;
        params.since_ts_ns = Some(part_since);
        params.until_ts_ns = Some(part_until);
        let report = db
            .weave_panel_intelligence(params)
            .map_err(|error| error.to_string())?;

        if report.records_scanned > WEAVE_INTERVAL_MAX_RECORDS {
            // Calyx timestamps are millisecond-granular. Once the interval is a
            // single millisecond, another split cannot separate its records;
            // refuse instead of advancing past an unprocessed suffix.
            if part_until - part_since <= 1_000_000 {
                return Err(format!(
                    "panel {panel_version} contains {} records in the indivisible interval [{part_since},{part_until}), above cap {WEAVE_INTERVAL_MAX_RECORDS}; watermark remains at {since_ns}; increase the cap only with a measured memory/latency budget or add a Base-key cursor",
                    report.records_scanned
                ));
            }
            let midpoint = part_since + (part_until - part_since) / 2;
            intervals.push_front((midpoint, part_until));
            intervals.push_front((part_since, midpoint));
            continue;
        }

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
                since_ns = part_since,
                until_ns = part_until,
                excluded_slot_count = report.knn_zero_norm_exclusions.len(),
                excluded_records,
                slots = %slots,
                "slot vectors that measure to exactly zero carry no direction and were classified \
                 out of the geometric lane; they remain measured, and every other lane wove"
            );
        }

        completed_parts += 1;
        records_woven = records_woven.saturating_add(report.records_woven as u64);
        last_xterm_rows = report.xterm_cf_rows_after;
        last_graph_rows = report.graph_cf_rows_after;
    }

    {
        let mut watermarks = match WEAVE_WATERMARK_NS.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let watermark = watermarks
            .get_mut(&panel_version)
            .ok_or_else(|| format!("panel {panel_version} watermark disappeared before commit"))?;
        if *watermark != since_ns {
            return Err(format!(
                "panel {panel_version} watermark changed concurrently: expected={since_ns} actual={watermark}; refusing to overwrite a newer owner"
            ));
        }
        *watermark = until_ns;
    }
    {
        let mut readback = match DERIVED_STATE_LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        readback
            .last_weave_actions
            .insert(panel_version, "interval_complete".to_owned());
        readback.last_weave_until_ns.insert(panel_version, until_ns);
        readback
            .last_weave_records
            .insert(panel_version, records_woven);
        readback
            .last_weave_xterm_rows
            .insert(panel_version, last_xterm_rows);
        readback
            .last_weave_graph_rows
            .insert(panel_version, last_graph_rows);
    }
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_WEAVE_PASS",
        panel_version,
        since_ns,
        until_ns,
        completed_parts,
        records_woven,
        xterm_cf_rows = last_xterm_rows,
        graph_cf_rows = last_graph_rows,
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "readback=physical XTerm/Graph CF counts after every new record in the bounded ingest interval was woven"
    );
    Ok(records_woven)
}

fn drive_post_ingest_drift(db: &Arc<Db>, panel_version: u32) -> Result<(), String> {
    let mut params = synapse_calyx::SynapseCalyxPanelDriftParams::new(panel_version);
    params.max_records = WEAVE_INTERVAL_MAX_RECORDS;
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
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_REACTIVE_DRIFT_PASS",
        panel_version,
        records_scanned = report.records_scanned,
        drifted_lenses = report.drifted_lenses,
        drift_rows_persisted = report.drift_rows_persisted,
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
    Ok(())
}

/// Runs only the Ward novelty outbox relay and returns its physical-delivery
/// counters. This is used by the synchronous guard facade so its response does
/// not race the background maintenance interval.
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
        publish_anchor_debt_readback(&pass, None, 0, report);
        return pass;
    }
    let started = std::time::Instant::now();
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
        let mut since_persist = 0usize;
        let mut queue_exhausted = !pending.is_empty();

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
            since_persist += 1;
            // The cursor advances past every identity the pass touches,
            // including one it refuses, so a pathological row can delay a pass
            // but can never stop it.
            state.after_source_cf = Some(identity.source_cf.clone());
            state.after_source_key_hex = Some(identity.source_key_hex.clone());

            let source_key = match decode_key_hex(&identity.source_key_hex) {
                Ok(key) => key,
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
                    continue;
                }
            };

            match db.backfill_temporal_metadata(&source_cf, Some(&source_key), None, 1) {
                Ok(page) => {
                    inserted = inserted.saturating_add(page.inserted_rows);
                    carried_anchors = carried_anchors.saturating_add(page.anchors_carried_forward);
                    // The attempt ran to completion, so whatever transient made
                    // earlier attempts fail is not a property of this row.
                    state.clear_failed_attempts(identity);
                    if page.anchors_carried_forward > 0 {
                        carried_rows = carried_rows.saturating_add(1);
                    } else {
                        // Proof, not suspicion: the exact row was re-measured at
                        // the active generation and its declared lineage carried
                        // nothing. Retrying the same bytes cannot change that,
                        // so this identity is quarantined by name — and only it.
                        // This is the #2061 population, held to three rows on
                        // `syn-outcome-v1` instead of a whole maintainer.
                        record_failure(
                            "STORAGE_DERIVED_STATE_ANCHOR_DEBT_IDENTITY_UNREPAIRABLE",
                            format!(
                                "panel {} stranded identity source_cf={} source_key_hex={} \
                                 from_generation={} was re-measured at generation {} and its \
                                 declared anchor lineage carried nothing; quarantining this one \
                                 identity so the panel's remaining debt and every other panel keep \
                                 being repaired; inspect that superseded Base row's anchors and \
                                 their confidence",
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
                    // A failed attempt is not proof the row cannot be repaired —
                    // a write shed under disk pressure is not a lineage fault —
                    // so it accrues attempts in the DURABLE ledger and is
                    // quarantined only once it has failed often enough to be a
                    // property of the row. Reading the count out of
                    // `quarantined` (which is only written at the threshold)
                    // made every tick report "attempt 1 of 3" forever (#1984).
                    let attempts = state.record_failed_attempt(identity, &error.to_string());
                    record_failure(
                        "STORAGE_DERIVED_STATE_ANCHOR_DEBT_IDENTITY_FAILED",
                        format!(
                            "panel {} exact anchor-debt repair for source_cf={} source_key_hex={} \
                             from_generation={} failed on attempt {attempts} of \
                             {PANEL_ANCHOR_DEBT_IDENTITY_MAX_ATTEMPTS}: {error}",
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
                            reason: format!("repair_failed_after_{attempts}_attempts: {error}"),
                            attempts,
                            first_seen_unix_ms: now_unix_ms(),
                            last_attempt_unix_ms: now_unix_ms(),
                        });
                        newly_quarantined += 1;
                    }
                    // Continue to the next identity, and do not abandon the
                    // panel's queue (#1984). One erroring identity is evidence
                    // about ONE row: the deployed daemon enumerated 3 stranded
                    // identities for `syn-outcome-v1`, attempted 1, broke, and
                    // reported `enumerated=3 attempted=1` on every tick — so the
                    // disposition of the other two was unknown for the life of
                    // the process. That is the same head-of-queue denial of
                    // service #2030 fixed for a record and #2061 fixed for a
                    // panel, one level further in. Each failure is recorded
                    // independently above and the budget still bounds the tick.
                    continue;
                }
            }

            if since_persist >= PANEL_ANCHOR_DEBT_CURSOR_PERSIST_EVERY {
                since_persist = 0;
                state.updated_at_unix_ms = now_unix_ms();
                if let Err(detail) = write_durable_maintenance_row(
                    db,
                    state_key.clone(),
                    &state,
                    "anchor-debt repair state",
                ) {
                    record_failure(
                        "STORAGE_DERIVED_STATE_ANCHOR_DEBT_CURSOR_UNWRITABLE",
                        format!(
                            "panel {} anchor-debt resume cursor could not be persisted: {detail}; \
                             without it a restart repeats work already done and no pass can be \
                             proven complete",
                            target.panel_name
                        ),
                    );
                    pass.failed = true;
                    queue_exhausted = false;
                    break;
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
        remaining_global = remaining_global.saturating_sub(attempted);
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ms_per_identity =
        (pass.identities_attempted > 0).then(|| elapsed_ms / pass.identities_attempted.max(1));
    if let Some(ms_per_identity) = ms_per_identity
        && ms_per_identity > PANEL_ANCHOR_DEBT_IDENTITY_MAX_MS
        && !ANCHOR_DEBT_UNAMORTIZED_REPORTED.swap(true, Ordering::Relaxed)
    {
        // Measured, not assumed. An exact-identity repair reads one source row,
        // writes one constellation and carries one anchor; a cost far above that
        // means the declared anchor lineage is being rebuilt per identity rather
        // than once per sweep, which quietly returns this repair to being
        // corpus-proportional — the exact defect it was written to remove.
        record_failure(
            "STORAGE_DERIVED_STATE_ANCHOR_DEBT_REPAIR_UNAMORTIZED",
            format!(
                "exact-identity anchor repair cost {ms_per_identity} ms per identity over {} \
                 identities, above the declared ceiling {PANEL_ANCHOR_DEBT_IDENTITY_MAX_MS} ms; \
                 the identity work queue is correct but its primitive is not amortizing the \
                 declared-lineage read, so a debt-proportional repair is doing \
                 corpus-proportional work and convergence is far slower than the debt implies; \
                 remediation=let the exact-key branch of Db::backfill_temporal_metadata reuse the \
                 cached anchor lineage instead of rebuilding it per row \
                 (crates/synapse-storage/src/backend.rs, anchor_carry_lineage(source_cf, \
                 after_physical.is_none()))",
                pass.identities_attempted,
            ),
        );
    }

    publish_anchor_debt_readback(&pass, ms_per_identity, elapsed_ms, report);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_ANCHOR_DEBT_PASS",
        identities_attempted = pass.identities_attempted,
        identities_carried = pass.identities_carried,
        anchors_carried = pass.anchors_carried,
        inserted_rows = pass.inserted_rows,
        quarantined_total = pass.quarantined_total,
        passes_completed = ?pass.passes_completed,
        panels = ?pass.panels,
        unbackfillable_panels = ?report.anchor_debt_unbackfillable_panels,
        ms_per_identity = ?ms_per_identity,
        elapsed_ms,
        "re-anchored exactly the source identities the census named as stranded"
    );
    pass
}

fn publish_anchor_debt_readback(
    pass: &AnchorDebtPass,
    ms_per_identity: Option<u64>,
    elapsed_ms: u64,
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
    for entry in &ready {
        if started.elapsed() >= PANEL_BACKFILL_TICK_BUDGET {
            break;
        }
        targets_attempted += 1;
        let pass = sweep_coverage_target(db, entry, started);
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
        targets_owed = ready.len(),
        targets_attempted,
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
) -> CoverageTargetPass {
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

    while started.elapsed() < PANEL_BACKFILL_TICK_BUDGET {
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
            match page.resume_after_physical {
                Some(resume) => after_physical = Some(resume),
                None => {
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
             consecutive_page_failures={consecutive_page_failures}",
            target.panel_name,
            target.panel_version,
            target.coverage_fraction,
            target.uncovered_rows(),
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
