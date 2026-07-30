//! The shared search query path: measure the query through the active text
//! lenses, recall per slot from the persisted indexes, fuse (RRF / weighted /
//! pipeline / single-lens), attach stored provenance, optionally apply the
//! in-region guard, then rank+truncate. Extracted from the CLI (#573) so the
//! CLI and `calyx-web-api` run the IDENTICAL path. Takes an already-opened
//! vault + panel state (the caller owns vault lifecycle); never resolves a CLI
//! home and never prints — failures are structured errors.

use std::collections::BTreeSet;
use std::path::Path;

use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, SlotId, SlotVector};

use crate::engine_measure::measure_query_vectors_with_slots_traced;
pub use crate::engine_measure::{
    EXACT_QUERY_LENS_NOT_EXACT_VALUE_QUERYABLE, EXACT_QUERY_SLOT_NOT_ON_PANEL,
    ExactValueMeasurement, QUERY_SKIP_LENS_NOT_REGISTERED, QUERY_SKIP_LENS_NOT_TEXT_QUERYABLE,
    QUERY_SKIP_NOT_SELECTED, QUERY_SKIP_SLOT_NOT_ACTIVE, QUERY_SKIP_VECTOR_NOT_INDEXABLE,
    QueryMeasurement, QuerySlotSkip, measure_exact_value, measure_query, measure_query_vectors,
    measure_query_vectors_with_slots,
};
pub use crate::engine_slot_cache::{SearchSlotCache, SearchSlotCacheDiagnostic};
pub use crate::engine_trace::SearchTraceEvent;
use crate::engine_trace::SearchTracer;
use crate::error::CliResult;

mod budget;
pub mod delta;
mod guard;
mod hydration;
mod hydration_cache;
mod search;
mod support;
mod types;
pub use budget::SearchBudget;
pub use delta::{MAX_RECONCILED_DELTA_KEYS, PanelDeltaComposition, measure_panel_delta};
use search::search_outcome_with_measured_slots;
pub use types::{
    FusionChoice, FusionResolution, FusionTuning, GuardChoice, SearchFreshness, SearchOutcome,
};

/// Historical flat in-region cosine threshold. Since #1094 this is NEVER
/// applied implicitly: `--guard in-region` without an operator tau loads the
/// calibrated Ward guard profile instead (fail-closed
/// `CALYX_GUARD_PROVISIONAL` when absent). The constant remains only as the
/// documented strict near-duplicate reference value for operators choosing an
/// explicit flat tau (issue #1088) and for the flat-path unit tests.
pub const DEFAULT_IN_REGION_GUARD_TAU: f32 = 0.999;

/// Bounded MVCC reader lease for a whole search readback pass.
const SEARCH_READER_LEASE_MS: u64 = 300_000;

/// Run the real search over `vault` (already opened) using its persisted
/// indexes at `vault_dir`. `state` is the loaded panel state (the query is
/// measured through its active text lenses). Returns ranked hits with stored
/// provenance. An empty/uningested vault yields an empty outcome (not an error);
/// a query with no indexable lens vectors, or stored vectors that don't match
/// the active query lenses, is a structured error (no silent empty result).
#[allow(clippy::too_many_arguments)]
pub fn search_outcome<C: Clock>(
    vault: &AsterVault<C>,
    state: &calyx_registry::VaultPanelState,
    vault_dir: &Path,
    query: &str,
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    filter: Option<&str>,
    explain: bool,
) -> CliResult<SearchOutcome> {
    search_outcome_with_freshness(
        vault,
        state,
        vault_dir,
        query,
        k,
        fusion,
        guard,
        filter,
        explain,
        SearchFreshness::Fresh,
    )
}

/// Run search with an explicit freshness policy. `Fresh` serves one pinned
/// snapshot by reconciling a complete bounded MVCC delta onto the immutable
/// generation, and fails closed when that proof is unavailable. `StaleOk`
/// permits lag only while tagging every hit with the index build seq and
/// current Base snapshot seq.
#[allow(clippy::too_many_arguments)]
pub fn search_outcome_with_freshness<C: Clock>(
    vault: &AsterVault<C>,
    state: &calyx_registry::VaultPanelState,
    vault_dir: &Path,
    query: &str,
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    filter: Option<&str>,
    explain: bool,
    freshness: SearchFreshness,
) -> CliResult<SearchOutcome> {
    search_outcome_with_slots(
        vault, state, vault_dir, query, k, fusion, guard, filter, explain, None, freshness,
    )
}

/// Slot-scoped variant of [`search_outcome`]. Normal search measures every
/// active text lens, but matrix/probe callers sometimes need a physically exact
/// subset: only those slots may be measured, searched, fused, and guarded.
#[allow(clippy::too_many_arguments)]
pub fn search_outcome_with_slots<C: Clock>(
    vault: &AsterVault<C>,
    state: &calyx_registry::VaultPanelState,
    vault_dir: &Path,
    query: &str,
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    filter: Option<&str>,
    explain: bool,
    allowed_slots: Option<&BTreeSet<SlotId>>,
    freshness: SearchFreshness,
) -> CliResult<SearchOutcome> {
    search_outcome_with_slots_traced(
        vault,
        state,
        vault_dir,
        query,
        k,
        fusion,
        guard,
        filter,
        explain,
        allowed_slots,
        freshness,
        None,
    )
}

/// Slot-scoped search with optional structured phase events.
#[allow(clippy::too_many_arguments)]
pub fn search_outcome_with_slots_traced<C: Clock>(
    vault: &AsterVault<C>,
    state: &calyx_registry::VaultPanelState,
    vault_dir: &Path,
    query: &str,
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    filter: Option<&str>,
    explain: bool,
    allowed_slots: Option<&BTreeSet<SlotId>>,
    freshness: SearchFreshness,
    trace_sink: Option<&mut dyn FnMut(SearchTraceEvent)>,
) -> CliResult<SearchOutcome> {
    let mut trace = SearchTracer::new(trace_sink);
    let query_vectors =
        measure_query_vectors_with_slots_traced(state, query, allowed_slots, Some(&mut trace))?;
    search_outcome_with_measured_slots(
        vault,
        vault_dir,
        &query_vectors,
        k,
        fusion,
        guard,
        None,
        &state.panel,
        filter,
        explain,
        allowed_slots,
        freshness,
        SearchBudget::disabled(),
        None,
        FusionTuning::default(),
        Some(&mut trace),
    )
}

/// Run search with query vectors measured by the caller. This is used by warm
/// resident-service callers so query embedding does not cold-load GPU runtimes
/// inside the search process.
#[allow(clippy::too_many_arguments)]
pub fn search_outcome_with_query_vectors<C: Clock>(
    vault: &AsterVault<C>,
    vault_dir: &Path,
    panel: &calyx_core::Panel,
    query_vectors: &[(SlotId, SlotVector)],
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    filter: Option<&str>,
    explain: bool,
    trace_sink: Option<&mut dyn FnMut(SearchTraceEvent)>,
) -> CliResult<SearchOutcome> {
    search_outcome_with_query_vectors_freshness(
        vault,
        vault_dir,
        panel,
        query_vectors,
        k,
        fusion,
        guard,
        filter,
        explain,
        SearchFreshness::Fresh,
        SearchBudget::disabled(),
        FusionTuning::default(),
        trace_sink,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn search_outcome_with_query_vectors_freshness<C: Clock>(
    vault: &AsterVault<C>,
    vault_dir: &Path,
    panel: &calyx_core::Panel,
    query_vectors: &[(SlotId, SlotVector)],
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    filter: Option<&str>,
    explain: bool,
    freshness: SearchFreshness,
    budget: SearchBudget<'_>,
    tuning: FusionTuning,
    trace_sink: Option<&mut dyn FnMut(SearchTraceEvent)>,
) -> CliResult<SearchOutcome> {
    search_outcome_with_query_vectors_freshness_cached(
        vault,
        vault_dir,
        panel,
        query_vectors,
        k,
        fusion,
        guard,
        None,
        filter,
        explain,
        freshness,
        budget,
        None,
        tuning,
        trace_sink,
    )
}

/// As [`search_outcome_with_query_vectors_freshness`] but with a per-run slot
/// cache and an optional operator-supplied in-region guard tau. `Some(tau)`
/// requires a finite value in `(0.0, 1.0]` and is used verbatim as the flat
/// in-region cosine threshold (#1088). `guard_tau = None` with
/// `guard = in-region` applies the calibrated Ward guard profile from the
/// Guard CF with per-slot conformal taus, failing closed with
/// `CALYX_GUARD_PROVISIONAL` when the profile is absent/uncalibrated or (when
/// `guard_panel_version` is supplied) calibrated for a different panel
/// version (#1094). There is no silent default tau.
#[allow(clippy::too_many_arguments)]
pub fn search_outcome_with_query_vectors_freshness_cached<C: Clock>(
    vault: &AsterVault<C>,
    vault_dir: &Path,
    panel: &calyx_core::Panel,
    query_vectors: &[(SlotId, SlotVector)],
    k: usize,
    fusion: FusionChoice,
    guard: GuardChoice,
    guard_tau: Option<f32>,
    filter: Option<&str>,
    explain: bool,
    freshness: SearchFreshness,
    budget: SearchBudget<'_>,
    slot_cache: Option<&mut SearchSlotCache>,
    tuning: FusionTuning,
    trace_sink: Option<&mut dyn FnMut(SearchTraceEvent)>,
) -> CliResult<SearchOutcome> {
    let allowed_slots = query_vectors
        .iter()
        .map(|(slot, _)| *slot)
        .collect::<BTreeSet<_>>();
    let mut trace = SearchTracer::new(trace_sink);
    search_outcome_with_measured_slots(
        vault,
        vault_dir,
        query_vectors,
        k,
        fusion,
        guard,
        guard_tau,
        panel,
        filter,
        explain,
        Some(&allowed_slots),
        freshness,
        budget,
        slot_cache,
        tuning,
        Some(&mut trace),
    )
}
