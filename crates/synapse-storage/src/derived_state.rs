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
    atomic::{AtomicU64, Ordering},
};

use synapse_calyx::{
    SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS, SEARCH_GENERATION_REFRESH_DELTA_KEYS,
    SynapseCalyxLensCoverageStatus, SynapseCalyxPersistedDriftFinding,
    SynapseCalyxPersistedNoveltyFinding, SynapseCalyxPersistedRegionFinding,
    SynapseCalyxSearchGenerationStatus, hot_context,
};

use crate::Db;
use crate::cf;
use crate::constellations::{GraphPositionKind, SYN_GRAPHPOS_APP_PANEL_VERSION};
use synapse_core::types::{TimelineKind, TimelineRecord};

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
                    "the Base CF holds panel generations that no catalog entry claims; their \
                     records are read by no active-panel surface and are counted as stranded"
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
    let readback = db.publish_graph_position_snapshot(
        GraphPositionKind::App,
        source_seq,
        now_unix_ms().unwrap_or(lease.read_at_unix_ms),
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

/// Where the last backfill page stopped, so the next tick resumes instead of
/// re-walking the corpus from the start.
///
/// Process-local on purpose. A restart resets it to the start of the CF, which
/// re-examines already-current rows — the cheap path in `backfill_temporal_metadata`,
/// which recognises them and does not rewrite — rather than skipping rows it has
/// not proven were measured. Persisting a cursor would trade that self-healing
/// property for a saved scan, and a cursor that outlives the panel generation it
/// was taken under is exactly how rows get silently skipped.
static BACKFILL_CURSOR: LazyLock<Mutex<Option<BackfillCursor>>> =
    LazyLock::new(|| Mutex::new(None));
static ANCHOR_DEBT_PROBE: LazyLock<Mutex<Option<AnchorDebtProbe>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Debug)]
struct BackfillCursor {
    panel_version: u32,
    source_cf: String,
    after_physical: Vec<u8>,
}

#[derive(Clone, Debug)]
struct AnchorDebtProbe {
    panel_version: u32,
    source_cf: String,
    stranded_before: usize,
}

/// Drives `temporal_backfill` pages for the panel most owed one, under
/// [`PANEL_BACKFILL_TICK_BUDGET`] (#1927 ask 2).
///
/// The backfill mechanism already existed and was already correct; nothing drove
/// it, so a panel could sit at 1.7% coverage indefinitely with `health`
/// reporting `ok`. This is the driver, and it is shaped exactly like the search
/// generation half above: measure the state, decide, act within a bounded
/// budget, publish what happened.
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
fn drive_panel_backfill(db: &Arc<Db>, report: &crate::panel_coverage::PanelCoverageReport) -> bool {
    let started = std::time::Instant::now();
    let Some(target) = report.most_owed_backfill() else {
        let unbackfillable_anchor_debt = report.panels.iter().any(|panel| {
            panel.anchors_stranded_on_superseded > 0 && panel.backfill_source_cf.is_none()
        });
        let action = if unbackfillable_anchor_debt {
            "anchor_debt_unbackfillable"
        } else if report.coverage_deficient_panels.is_empty() {
            "none_owed"
        } else {
            // Deficient, but no panel that is deficient has a re-measure path.
            // Reporting this as "none owed" would be the silent-success failure
            // this whole issue is about.
            "owed_but_unbackfillable"
        };
        let mut guard = match DERIVED_STATE_LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.last_backfill_action = Some(action.to_owned());
        guard.last_backfill_reason = None;
        guard.last_backfill_panel = None;
        guard.last_backfill_source_cf = None;
        guard.last_backfill_pages = Some(0);
        guard.last_backfill_elapsed_ms = Some(0);
        // Not a failure: "nothing is owed" and "nothing owed is repairable" are
        // both correct outcomes for this pass. The unrepairable case is a
        // deficiency of the panel catalog, already raised by health, not a
        // failure of this driver.
        return false;
    };
    let Some(source_cf) = target.backfill_source_cf.clone() else {
        // `most_owed_backfill` already filtered on this being present; reaching
        // here means the two predicates disagree, which is a code defect worth
        // shouting about rather than a state worth handling.
        record_failure(
            "STORAGE_DERIVED_STATE_BACKFILL_TARGET_HAS_NO_SOURCE",
            format!(
                "panel {} was selected as most-owed backfill but declares no backfill_source_cf; \
                 PanelCoverageRow::backfill_owed and PanelCoverageReport::most_owed_backfill \
                 disagree",
                target.panel_name
            ),
        );
        return true;
    };
    let backfill_reason = target.backfill_reason().unwrap_or("unknown_debt");

    // #1984: a completed whole-source sweep is one exact repair attempt. On
    // the next census, prove it reduced the source-identity debt before ever
    // starting another sweep. A missing lineage declaration cannot heal by
    // retrying the same bytes forever, so latch the fault until the measured
    // debt or panel generation changes.
    if target.anchors_stranded_on_superseded > 0 {
        let mut probe = match ANCHOR_DEBT_PROBE.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = probe.as_ref().filter(|previous| {
            previous.panel_version == target.panel_version
                && previous.source_cf == source_cf
                && target.anchors_stranded_on_superseded >= previous.stranded_before
        }) {
            let detail = format!(
                "panel {} completed a full anchor-debt sweep but exact stranded source identities did not decrease: before={} after={}; refusing an unattended retry until the debt or panel generation changes; inspect superseded_versions and historical Base source metadata",
                target.panel_name, previous.stranded_before, target.anchors_stranded_on_superseded,
            );
            drop(probe);
            record_failure("STORAGE_DERIVED_STATE_ANCHOR_DEBT_NO_PROGRESS", detail);
            let mut guard = match DERIVED_STATE_LAST.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.last_backfill_action = Some("anchor_debt_no_progress".to_owned());
            guard.last_backfill_reason = Some(backfill_reason.to_owned());
            guard.last_backfill_panel = Some(target.panel_name.clone());
            guard.last_backfill_source_cf = Some(source_cf);
            guard.last_backfill_pages = Some(0);
            guard.last_backfill_elapsed_ms = Some(0);
            return true;
        }
        if probe.as_ref().is_some_and(|previous| {
            previous.panel_version != target.panel_version
                || previous.source_cf != source_cf
                || target.anchors_stranded_on_superseded < previous.stranded_before
        }) {
            *probe = None;
        }
    }

    // A cursor taken under a different panel generation or a different CF says
    // nothing about where this sweep should resume, so it is discarded rather
    // than reused.
    let mut after_physical = {
        let guard = match BACKFILL_CURSOR.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .filter(|cursor| {
                cursor.panel_version == target.panel_version && cursor.source_cf == source_cf
            })
            .map(|cursor| cursor.after_physical.clone())
    };

    let mut pages = 0_u64;
    let mut examined = 0_u64;
    let mut inserted = 0_u64;
    let mut already_current = 0_u64;
    let mut outcome_anchored = 0_u64;
    let mut sweep_complete = false;
    let mut action = "budget_exhausted";

    while started.elapsed() < PANEL_BACKFILL_TICK_BUDGET {
        let page = match db.backfill_temporal_metadata(
            &source_cf,
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
                record_failure(
                    "STORAGE_DERIVED_STATE_BACKFILL_PAGE_FAILED",
                    format!(
                        "backfill page for panel {} from {source_cf}: {error}",
                        target.panel_name
                    ),
                );
                action = "page_failed";
                break;
            }
        };
        pages += 1;
        examined += page.examined_rows;
        inserted += page.inserted_rows;
        already_current += page.already_current_rows;
        outcome_anchored += page.outcome_anchored_rows;

        if page.more {
            match page.resume_after_physical {
                Some(cursor) => after_physical = Some(cursor),
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

    {
        let mut guard = match BACKFILL_CURSOR.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = if sweep_complete {
            // The next sweep starts from the head of the CF. That is the correct
            // posture after a generation bump: the rows re-measured first are
            // the oldest, which are the ones no live write will ever reach.
            None
        } else {
            after_physical.map(|cursor| BackfillCursor {
                panel_version: target.panel_version,
                source_cf: source_cf.clone(),
                after_physical: cursor,
            })
        };
    }

    if sweep_complete && target.anchors_stranded_on_superseded > 0 {
        let mut probe = match ANCHOR_DEBT_PROBE.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *probe = Some(AnchorDebtProbe {
            panel_version: target.panel_version,
            source_cf: source_cf.clone(),
            stranded_before: target.anchors_stranded_on_superseded,
        });
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_BACKFILL_PASS",
        action,
        panel = %target.panel_name,
        panel_version = target.panel_version,
        source_cf = %source_cf,
        reason = backfill_reason,
        coverage_fraction = ?target.coverage_fraction,
        uncovered_rows = ?target.uncovered_rows(),
        pages,
        examined,
        inserted,
        already_current,
        outcome_anchored,
        sweep_complete,
        elapsed_ms,
        "drove the unattended panel backfill toward the active generation"
    );

    let mut guard = match DERIVED_STATE_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_backfill_action = Some(action.to_owned());
    guard.last_backfill_reason = Some(backfill_reason.to_owned());
    guard.last_backfill_panel = Some(target.panel_name.clone());
    guard.last_backfill_source_cf = Some(source_cf);
    guard.last_backfill_pages = Some(pages);
    guard.last_backfill_examined_rows = Some(examined);
    guard.last_backfill_inserted_rows = Some(inserted);
    guard.last_backfill_already_current_rows = Some(already_current);
    guard.last_backfill_outcome_anchored_rows = Some(outcome_anchored);
    guard.last_backfill_elapsed_ms = Some(elapsed_ms);
    guard.last_backfill_sweep_complete = Some(sweep_complete);

    // `page_failed` and `cursor_absent` are the two exits that already called
    // record_failure; report them so the tick is not also counted a success.
    matches!(action, "page_failed" | "cursor_absent")
}
