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
    /// Lens coverage measured by the last pass.
    pub last_lens_coverage: Option<SynapseCalyxLensCoverageStatus>,
    pub last_lens_coverage_unix_ms: Option<u64>,
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

    // --- Search generation (#1891 ask 2) ---
    match db.maintain_calyx_search_generation() {
        Ok(report) => {
            let mut guard = match DERIVED_STATE_LAST.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
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
        Err(error) => {
            any_failed = true;
            record_failure(
                "STORAGE_DERIVED_STATE_SEARCH_GENERATION_FAILED",
                format!("maintain the persisted Calyx search generation: {error}"),
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
