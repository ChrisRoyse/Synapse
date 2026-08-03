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

use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

use synapse_calyx::{
    SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS, SEARCH_GENERATION_REFRESH_DELTA_KEYS,
    SynapseCalyxLensCoverageStatus, SynapseCalyxSearchGenerationStatus, hot_context,
};

use crate::Db;

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

static DERIVED_STATE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_SUCCESS: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_FAILURE: AtomicU64 = AtomicU64::new(0);
static DERIVED_STATE_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Storage handle the derived-state pass reads the vault from.
///
/// A `Weak` for the same reason the lowering source is: this registry must not
/// be the reason a closed vault's handle stays alive.
static DERIVED_STATE_SOURCE: LazyLock<Mutex<Option<Weak<Db>>>> = LazyLock::new(|| Mutex::new(None));

static DERIVED_STATE_LAST: LazyLock<Mutex<DerivedStateReadback>> =
    LazyLock::new(|| Mutex::new(DerivedStateReadback::default()));

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
    tracing::info!(
        code = "STORAGE_DERIVED_STATE_SOURCE_REGISTERED",
        db_path = %db.path.display(),
        refresh_delta_keys_threshold = SEARCH_GENERATION_REFRESH_DELTA_KEYS,
        min_rebuild_interval_ms = SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS,
        "registered the storage handle the unattended derived-state maintainer reads"
    );
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
            // #1970: a lane that is constant over its corpus cannot rank, and no
            // admission gate can see it — the encoder is graded, the *input* is
            // never populated. Warned separately from a coverage deficiency
            // because the remediation is different: coverage is fixed by
            // backfilling rows, this is fixed by rebuilding or parking the lens.
            if !coverage.degenerate_lanes.is_empty() {
                tracing::warn!(
                    code = "STORAGE_DERIVED_STATE_LENS_CONSTANT_BY_CORPUS",
                    degenerate_lane_count = coverage.degenerate_lanes.len(),
                    detail = %coverage
                        .degenerate_lanes
                        .iter()
                        .map(|lane| format!(
                            "panel {} slot {} constant across {} record(s)",
                            lane.panel_version, lane.slot, lane.records_present
                        ))
                        .collect::<Vec<_>>()
                        .join("; "),
                    "one or more dense lanes took a single value across every measured record that \
                     carries them, so they cannot rank and contribute no bits about any anchor"
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
