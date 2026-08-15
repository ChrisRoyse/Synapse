//! Phase-4 Calyx intelligence wiring: Loom weave (cross-terms, agreement graph,
//! between-record kNN graph, abundance report).
//!
//! This module is the Synapse-side facade over the `calyx-loom` substrate
//! crate. It reads the physical `Base` CF constellations for one panel, computes
//! the base associations with the substrate math, persists derived rows into the
//! native `XTerm` and `Graph` CFs, and reads the physical CFs back so every
//! returned value is proven against the bytes. It never flattens slots, never
//! invents a fallback, and fails closed with structured `SynapseCalyxError`
//! values.

use std::collections::{BTreeMap, BTreeSet};

use calyx_assay::sufficiency::{AnchorLeakage, detect_anchor_leakage};
use calyx_assay::{
    AssayCacheKey, AssayStore, AssaySubject, ChangePointReport, CusumReport, Direction,
    EnsembleCard, EnsembleConfig, EnsembleLensInput, EstimatorKind, InterEventHazardReport,
    MIN_ENSEMBLE_PANEL_LENSES, MiEstimate, MiEstimator, MiEstimatorChoice, MiEstimatorPick,
    MmdConfig, PeriodogramConfig, RateShift, SIGNIFICANT_PEAK_FAP, SlotAttribution,
    SynergyEstimators, SynergyPairState, SynergyReport, TEResult, TeEstimator,
    TiedOccurrenceCollapse, TrustTag, autocorrelation, bin_event_counts, bits_report_with_anchor,
    collapse_tied_occurrences, ensemble_card, ensemble_nmi_signature, entropy_bits,
    inter_event_hazard_with_alpha, lomb_scargle_with_config, mi_about_labels, mmd_change_point,
    panel_sufficiency_with_anchor, partitioned_histogram_nmi, per_sensor_attribution,
    recurrence_rate_cusum, resolve_mi_estimator, stable_rank, synergy_pair, synergy_report,
    transfer_entropy_sweep, unmeasured_synergy_pair,
};
use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{
    AbsentReason, Anchor, AnchorKind, AnchorValue, Constellation, CxId,
    METADATA_SOURCE_EVENT_TIME_RAW, PanelSlotId, SlotId, SlotVector, SparseEntry, SystemClock, Ts,
};
use calyx_forge::{Backend, KnnMetric};
use calyx_lodestar::{
    AnswerDerivation, InMemoryAnnIndex, InMemoryCorpus, Kernel, KernelGraphParams, KernelIndex,
    KernelParams, LodestarError, LpRoundParams, RecallEvalParams, RecallQuery, build_kernel_index,
    build_kernel_pipeline, derive_kernel_answer, derive_kernel_answer_from_ranked_members,
    full_topk_support_set, measure_kernel_recall, measure_ranked_kernel_recall,
    refine_kernel_with_recall_support, seal_completed_kernel_identity, write_kernel_artifact,
};
use calyx_loom::{
    AbundanceReport, CeilingEstimate, LoomStore, MaterializationAction, MaterializationBatchReport,
    NeffEstimate, StaticPairGainGate, cross_term_upper_bound, dda_signal_yield, plan_cross_terms,
};
use calyx_oracle::{AnnealConfig, DomainId, SlotSet, WardCompletionRegion};
use calyx_paths::AssocGraph;
use calyx_ward::TrustedRegion;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::lens_provenance;
use crate::{
    SynapseCalyxCfCountProvenance, SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault,
};

/// Hard cap on records scanned per weave pass to bound one bounded,
/// pressure-aware maintenance operation. The caller may request fewer.
pub const SYNAPSE_INTELLIGENCE_MAX_RECORDS: usize = 20_000;
/// Default k for the between-record nearest-neighbor graph.
pub const SYNAPSE_KNN_DEFAULT_K: usize = 8;
/// Global cap on persisted between-record kNN edges per weave pass.
pub const SYNAPSE_KNN_MAX_EDGES: usize = 200_000;
/// Cap on the number of blind-spot lens pairs listed in one weave report.
pub const SYNAPSE_WEAVE_MAX_BLIND_SPOTS: usize = 32;
/// Structured code raised when an intelligence time window is empty/inverted.
pub const SYNAPSE_INTELLIGENCE_TIME_RANGE_INVALID: &str =
    "SYNAPSE_CALYX_INTELLIGENCE_TIME_RANGE_INVALID";
/// Structured code raised when a caller addresses a panel generation that the
/// durable Calyx allocator has never registered.
pub const SYNAPSE_INTELLIGENCE_PANEL_UNREGISTERED: &str =
    "SYNAPSE_CALYX_INTELLIGENCE_PANEL_UNREGISTERED";
/// Structured code raised when a synergy pass is asked for an anchor no record
/// in the panel carries.
pub const SYNAPSE_SYNERGY_NO_ANCHORED_RECORDS: &str = "SYNAPSE_CALYX_SYNERGY_NO_ANCHORED_RECORDS";
/// Structured code raised when a sufficiency assay is adjudicated by an anchor
/// whose determining record fields are read by a lens in the same panel (#1958).
///
/// This is the *structural* leakage refusal. It is deliberately distinct from
/// the statistical `AnchorLeakage` report, which fires only for a lens that IS
/// the label; this one fires for a lens that merely *contains* it, which no
/// statistical test can reach.
pub const SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE: &str = "SYNAPSE_CALYX_ASSAY_ANCHOR_SOURCE_LEAKAGE";
/// Structured code raised when an ensemble capability-card pass is asked for an
/// anchor no record in the panel carries.
pub const SYNAPSE_ENSEMBLE_NO_ANCHORED_RECORDS: &str = "SYNAPSE_CALYX_ENSEMBLE_NO_ANCHORED_RECORDS";
/// Structured code raised when the requested anchor is not binary. The ensemble
/// card's decision surrogate is a binary logistic probe; a three-outcome anchor
/// is refused rather than collapsed to one-versus-rest behind the operator's
/// back.
pub const SYNAPSE_ENSEMBLE_ANCHOR_NOT_BINARY: &str = "SYNAPSE_CALYX_ENSEMBLE_ANCHOR_NOT_BINARY";
/// Structured code raised when too few lenses are present on *every* anchored
/// record for a panel-level card to mean anything.
pub const SYNAPSE_ENSEMBLE_NO_COPRESENT_LENSES: &str = "SYNAPSE_CALYX_ENSEMBLE_NO_COPRESENT_LENSES";

const GRAPH_AGREEMENT_PREFIX: &[u8; 5] = b"GAGR1";
const GRAPH_KNN_PREFIX: &[u8; 5] = b"GKNN1";

/// Declares which physical math resource a weave is allowed to activate.
///
/// Scheduled maintenance is deliberately CPU-only so an idle background daemon
/// never creates CUDA contexts, VRAM reservations, or driver threads that can
/// interfere with foreground games. Interactive/explicit intelligence requests
/// retain the operator-configured backend and its fail-closed behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynapseCalyxMathExecutionClass {
    Configured,
    BackgroundCpu,
}

impl SynapseCalyxMathExecutionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::BackgroundCpu => "background_cpu",
        }
    }
}

/// Bounded request describing one Loom weave pass over a panel.
#[derive(Clone, Copy, Debug)]
pub struct SynapseCalyxWeaveParams {
    pub panel_version: u32,
    pub max_records: usize,
    pub knn_k: usize,
    pub cache_capacity: usize,
    /// Inclusive lower bound on a record's server-stamped `created_at`, in Unix
    /// nanoseconds. `None` leaves the window open at that end.
    pub since_ts_ns: Option<i64>,
    /// Exclusive upper bound on a record's server-stamped `created_at`, in Unix
    /// nanoseconds. `None` leaves the window open at that end.
    pub until_ts_ns: Option<i64>,
    pub math_execution_class: SynapseCalyxMathExecutionClass,
}

impl SynapseCalyxWeaveParams {
    #[must_use]
    pub const fn new(panel_version: u32) -> Self {
        Self {
            panel_version,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            knn_k: SYNAPSE_KNN_DEFAULT_K,
            cache_capacity: 4_096,
            since_ts_ns: None,
            until_ts_ns: None,
            math_execution_class: SynapseCalyxMathExecutionClass::Configured,
        }
    }
}

/// Half-open `[since, until)` filter over a record's server-stamped
/// `created_at`, expressed in Unix nanoseconds.
///
/// Calyx stamps `Constellation::created_at` in Unix **milliseconds**
/// (`calyx_core::Ts`), so the comparison is done in nanoseconds against
/// `created_at * 1_000_000`: a record is in the window iff
/// `since <= created_at_ns < until`. Sub-millisecond bounds therefore select
/// exactly the millisecond ticks they contain — no rounding, no ambiguity.
#[derive(Clone, Copy, Debug, Default)]
struct TimeWindowNs {
    since: Option<i64>,
    until: Option<i64>,
}

impl TimeWindowNs {
    /// Builds the window, failing closed on an empty or inverted range rather
    /// than silently returning zero records.
    fn new(since: Option<i64>, until: Option<i64>) -> Result<Self, SynapseCalyxError> {
        if let (Some(since), Some(until)) = (since, until)
            && since >= until
        {
            return Err(SynapseCalyxError::new(
                SYNAPSE_INTELLIGENCE_TIME_RANGE_INVALID,
                format!(
                    "intelligence time range is empty: since_ts_ns={since} must be strictly less \
                     than until_ts_ns={until}"
                ),
                "pass a half-open [since_ts_ns, until_ts_ns) window with since < until, or omit \
                 one bound to leave that end open",
            ));
        }
        Ok(Self { since, until })
    }

    /// True when a `created_at` stamp (Unix milliseconds) falls in the window.
    fn contains_created_at_ms(self, created_at_ms: u64) -> bool {
        let Some(stamp_ns) = i64::try_from(created_at_ms)
            .ok()
            .and_then(|millis| millis.checked_mul(1_000_000))
        else {
            // A stamp that cannot be expressed in nanoseconds cannot be placed
            // in the window; an open window still admits it.
            return self.since.is_none() && self.until.is_none();
        };
        self.since.is_none_or(|since| stamp_ns >= since)
            && self.until.is_none_or(|until| stamp_ns < until)
    }
}

/// Weighted undirected agreement edge between two panel slots, aggregated over
/// every record's per-slot-pair agreement cross-term.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxAgreementEdge {
    pub panel_version: u32,
    pub slot_a: u16,
    pub slot_b: u16,
    pub mean_agreement: f32,
    pub agreement_weight: f32,
    pub n: usize,
}

/// One directed nearest-neighbor edge in the between-record graph over a slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBetweenRecordEdge {
    pub slot: u16,
    pub cx_a: String,
    pub cx_b: String,
    pub score: f32,
    pub rank: usize,
}

/// Serializable effective-rank estimate mirror of the Loom `NeffEstimate`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxNeffEstimate {
    pub value: f32,
    pub provisional: bool,
    pub ci_low: Option<f32>,
    pub ci_high: Option<f32>,
}

/// Flattened, serializable abundance report tied to physical CF readbacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxAbundanceReport {
    pub panel_version: u32,
    /// Lenses the panel **declares** — the `N` in the DDA yield. A lens the
    /// association engine cannot consume is reported in `slot_states`, never
    /// subtracted from this count (#1939).
    pub n_lenses: usize,
    /// Lenses that actually reached the corpus. `measurable_lenses < n_lenses`
    /// means the panel is carrying dark lenses; `slot_states` names them.
    #[serde(default)]
    pub measurable_lenses: usize,
    /// Per declared lens: its vector kind, whether the engine could measure on
    /// it, and why not when it could not (#1939).
    #[serde(default)]
    pub slot_states: Vec<SynapseCalyxCorpusSlotState>,
    pub n_constellations: usize,
    pub c_n2_upper_bound: usize,
    pub materialized: usize,
    pub measured_count: usize,
    pub derived_count: usize,
    pub meaning_compression_yield: f32,
    /// DDA signal yield `n * (N + C(N,2) + 1)`: the derived signals a corpus of
    /// `n` inputs over `N` lenses can carry — every lens, every lens pair, and
    /// the whole-constellation term, per input.
    pub dda_signal_yield: usize,
    pub n_eff: SynapseCalyxNeffEstimate,
    pub dpi_ceiling_bits: Option<f32>,
    pub dpi_ceiling_provisional: bool,
    /// Anchor kind whose persisted Assay bits pass produced the computed DPI
    /// ceiling; `None` while the ceiling is still provisional.
    pub dpi_ceiling_anchor_kind: Option<String>,
    pub xterm_cf_rows: usize,
    pub graph_cf_rows: usize,
    /// How `xterm_cf_rows` was established: a physical baseline walk, an
    /// unchanged memo, or the exact commit-maintained aggregate.
    pub xterm_cf_rows_readback: SynapseCalyxCfCountProvenance,
    /// How `graph_cf_rows` was established.
    pub graph_cf_rows_readback: SynapseCalyxCfCountProvenance,
    /// Last logical commit incorporated into the exact `XTerm` count.
    pub xterm_cf_last_commit_seq: u64,
    /// Last logical commit incorporated into the exact `Graph` count.
    pub graph_cf_last_commit_seq: u64,
    /// Vault-wide sequence observed by the later `Graph` readback.
    pub vault_latest_seq: u64,
}

/// One *coverage* blind spot in a woven panel: a lens pair that never co-occurs
/// on any measured record, so no cross-term over it can ever be materialized.
///
/// This is a structural gap in what the weave could see. It is a different
/// quantity from the drift module's `SynapseCalyxBlindSpotAlert`, which flags a
/// per-record disagreement between two lenses that *did* both fire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxWeaveBlindSpotPair {
    pub slot_a: u16,
    pub slot_b: u16,
    pub records_with_a: usize,
    pub records_with_b: usize,
}

/// Result of one bounded Loom weave pass with physical CF readbacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxWeaveReport {
    pub panel_version: u32,
    pub records_scanned: usize,
    pub records_woven: usize,
    /// Lenses the panel declares (#1939).
    pub n_lenses: usize,
    /// Lenses that reached the corpus and could be woven.
    #[serde(default)]
    pub measurable_lenses: usize,
    /// Per declared lens: kind, measurability, and the reason when not (#1939).
    #[serde(default)]
    pub slot_states: Vec<SynapseCalyxCorpusSlotState>,
    pub cross_terms_materialized: usize,
    pub agreement_edges_persisted: usize,
    pub between_record_edges_persisted: usize,
    /// Valid exact measurements that have no direction and therefore cannot
    /// enter a cosine graph. They remain present for structured analysis and
    /// within-record cross-terms; only the geometric lane excludes them.
    #[serde(default)]
    pub knn_zero_norm_exclusions: Vec<SynapseCalyxKnnZeroNormExclusion>,
    /// Within-record **agreement** cross-terms skipped because one operand had
    /// no direction, on the same principle as `knn_zero_norm_exclusions` but on
    /// the other lane (#2076).
    ///
    /// This is the lane that actually stalls this vault, and it needed its own
    /// field rather than reusing the kNN one. The kNN lane excludes a *record*
    /// whose whole concatenated vector is zero, which on `syn-episode-v1` never
    /// happens — the text and identity lenses are always non-zero — so
    /// `knn_zero_norm_exclusions` reads empty on every pass. The agreement lane
    /// skips a *lens pair* on one record, and on a 4000-record read of the live
    /// episode panel that fired 42,963 times over 3,955 records. Reporting only
    /// the kNN lane therefore said "no zero-norm anywhere" about a corpus that
    /// is 88% zero-norm on slot 16 alone.
    #[serde(default)]
    pub agreement_zero_norm_skips: usize,
    /// Records that contributed at least one agreement skip.
    #[serde(default)]
    pub agreement_zero_norm_records: usize,
    /// Distinct `(slot_a, slot_b)` lens pairs seen in the recorded sample,
    /// ordered. Drawn from Loom's capped ledger, so it names the pairs without
    /// promising completeness — see `agreement_zero_norm_sample_truncated`.
    #[serde(default)]
    pub agreement_zero_norm_slot_pairs: Vec<(u16, u16)>,
    /// Whether Loom's skip ledger hit [`calyx_loom::MAX_RECORDED_ZERO_NORM_SKIPS`],
    /// so `agreement_zero_norm_slot_pairs` is a sample and not the full set.
    /// Never silent: `agreement_zero_norm_skips` stays an exact count either way.
    #[serde(default)]
    pub agreement_zero_norm_sample_truncated: bool,
    pub xterm_cf_rows_after: usize,
    pub graph_cf_rows_after: usize,
    /// How the two readback counts above were obtained on this pass (#2114):
    /// `walked` (physically re-walked), `walked_drift_check` (re-walked to
    /// confront the memo with the disk), or `unchanged_since_last_walk` (the
    /// vault has committed nothing since the walk that produced them, so the
    /// row set is provably the same one). Reported rather than implied — a
    /// count that was not re-walked must not read like one that was.
    #[serde(default)]
    pub xterm_cf_rows_readback: String,
    #[serde(default)]
    pub graph_cf_rows_readback: String,
    /// Effective half-open time window applied to `Base` rows, echoed back.
    pub since_ts_ns: Option<i64>,
    pub until_ts_ns: Option<i64>,
    /// Panel rows the time window excluded from this pass.
    pub records_outside_window: usize,
    /// DDA signal yield `n * (N + C(N,2) + 1)` for the woven corpus.
    pub dda_signal_yield: usize,
    /// Lens pairs the panel can express, `C(N,2)`.
    pub lens_pairs_possible: usize,
    /// Lens pairs that co-occur on at least one measured record.
    pub lens_pairs_co_present: usize,
    /// Lens pairs that never co-occur, so no cross-term over them can exist.
    pub blind_spot_pairs: usize,
    /// `blind_spot_pairs / lens_pairs_possible`; `0.0` when `N < 2`.
    pub blind_spot_fraction: f32,
    /// Measured records carrying fewer than two lenses: they contribute a lens
    /// reading but no within-record cross-term at all.
    pub blind_spot_records: usize,
    /// Lenses present on no measured record in the window, i.e. contributing
    /// nothing to this weave.
    pub blind_spot_slots: Vec<u16>,
    /// The blind lens pairs themselves, capped at
    /// [`SYNAPSE_WEAVE_MAX_BLIND_SPOTS`] and ordered by slot id.
    pub blind_spot_pair_details: Vec<SynapseCalyxWeaveBlindSpotPair>,
    pub agreement_edges: Vec<SynapseCalyxAgreementEdge>,
    pub abundance: SynapseCalyxAbundanceReport,
}

/// Per-slot evidence that zero-valued vectors were explicitly classified out
/// of cosine kNN rather than silently dropped or rewritten as `Absent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxKnnZeroNormExclusion {
    pub slot: u16,
    pub records: usize,
    pub sample_cx_ids: Vec<String>,
}

impl SynapseCalyxVault {
    /// Weaves the base associations for one panel: within-record cross-terms
    /// (agreement eager; interaction eager iff its pair-gain reaches the bit
    /// floor; delta/concat lazy), the slot-pair agreement graph, and the
    /// between-record nearest-neighbor graph. Derived rows are persisted to the
    /// native `XTerm`/`Graph` CFs and read back before returning.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the `Base` CF cannot be
    /// scanned, a constellation row fails to decode, the substrate math rejects
    /// a vector, the math backend is unavailable, or any CF write/readback
    /// fails.
    #[allow(clippy::too_many_lines)]
    pub fn weave_panel(
        &self,
        params: SynapseCalyxWeaveParams,
    ) -> Result<SynapseCalyxWeaveReport, SynapseCalyxError> {
        let weave_started = std::time::Instant::now();
        // Hot-path boundary (#1686): live Loom weave is an off-runtime
        // intelligence computation and must never be driven from a tagged tick.
        crate::lowering::hot_context::assert_cold_calyx("weave_panel");
        // A syntactically valid u32 is not necessarily a panel. The allocator is
        // the durable authority for both built-in and runtime-minted generations;
        // validating there keeps known-empty panels valid while refusing a made-up
        // generation before it can masquerade as a successful empty weave (#2238).
        let allocator = self.panel_generation_allocator()?;
        if !allocator.owners.contains_key(&params.panel_version) {
            tracing::error!(
                code = SYNAPSE_INTELLIGENCE_PANEL_UNREGISTERED,
                panel_version = params.panel_version,
                allocator_latest_seq = allocator.latest_seq,
                registered_generations = allocator.owner_count,
                "intelligence weave refused a panel generation absent from the durable Calyx allocator"
            );
            return Err(SynapseCalyxError::new(
                SYNAPSE_INTELLIGENCE_PANEL_UNREGISTERED,
                format!(
                    "panel_version={} is absent from the durable Calyx panel-generation allocator at seq {} (registered_generations={})",
                    params.panel_version, allocator.latest_seq, allocator.owner_count
                ),
                "read the durable Calyx panel-generation allocator and retry with one of its registered generation IDs",
            ));
        }
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let window = TimeWindowNs::new(params.since_ts_ns, params.until_ts_ns)?;
        let corpus_load_started = std::time::Instant::now();
        let corpus =
            self.load_panel_dense_corpus_in_window(params.panel_version, max_records, window)?;
        let corpus_load_secs = corpus_load_started.elapsed().as_secs_f64();
        let records_scanned = corpus.records_scanned;
        let records_outside_window = corpus.records_outside_window;
        let blind_spots = blind_spot_summary(&corpus);

        let mut store = LoomStore::new(params.cache_capacity.max(1));
        // interaction eager iff pair-gain >= bit floor; without grounded anchors
        // in this pass the gain is unmeasured, so the honest gate keeps
        // interaction lazy (0.0 < floor) and only agreement is eager.
        let gate = StaticPairGainGate { gain_bits: 0.0 };
        let mut records_woven = 0usize;
        let mut lens_ids: BTreeSet<SlotId> = BTreeSet::new();
        let mut measured_slot_instances = 0usize;
        let mut materialization_plans = Vec::with_capacity(corpus.records.len());
        let configured_backend = if corpus.records.is_empty()
            || params.math_execution_class != SynapseCalyxMathExecutionClass::Configured
        {
            None
        } else {
            Some(self.math_runtime.backend()?)
        };
        let background_backend = if corpus.records.is_empty()
            || params.math_execution_class != SynapseCalyxMathExecutionClass::BackgroundCpu
        {
            None
        } else {
            Some(crate::math::verified_background_cpu_backend()?)
        };
        let backend = configured_backend
            .as_ref()
            .map(|lease| &**lease as &dyn Backend)
            .or(background_backend.as_deref());
        let backend_used = if corpus.records.is_empty() {
            "none_empty_corpus"
        } else {
            params.math_execution_class.as_str()
        };
        tracing::info!(
            code = "SYNAPSE_CALYX_WEAVE_MATH_EXECUTION_CLASS",
            panel_version = params.panel_version,
            requested_execution_class = params.math_execution_class.as_str(),
            backend_used,
            corpus_records = corpus.records.len(),
            configured_runtime_activated = configured_backend.is_some(),
            background_cpu_activated = background_backend.is_some(),
            "selected the declared weave math execution class without runtime fallback"
        );
        let plan_started = std::time::Instant::now();
        for record in &corpus.records {
            for slot in record.slots.keys() {
                lens_ids.insert(*slot);
            }
            measured_slot_instances += record.slots.len();
            if record.slots.len() < 2 {
                // A single-slot record still contributes its measured slots to
                // the panel, but has no within-record cross-term to weave.
                materialization_plans.push(None);
                continue;
            }
            let mut slot_ids: Vec<SlotId> = record.slots.keys().copied().collect();
            slot_ids.sort_unstable();
            let mut plan = plan_cross_terms(&slot_ids, &gate);
            // Cross-term math is only defined for equal-dimension slot pairs;
            // demote every eager entry whose two slots differ in dimension to a
            // lazy entry so materialization never fails closed on a real panel
            // that mixes lens output shapes.
            for entry in &mut plan.entries {
                if entry.action == MaterializationAction::EagerStore
                    && record.slots.get(&entry.a).map(Vec::len)
                        != record.slots.get(&entry.b).map(Vec::len)
                {
                    entry.action = MaterializationAction::LazyCache;
                }
            }
            materialization_plans.push(Some(plan));
            records_woven += 1;
        }
        let plan_secs = plan_started.elapsed().as_secs_f64();

        let materialize_started = std::time::Instant::now();
        let materialization = if records_woven == 0 {
            MaterializationBatchReport::default()
        } else {
            let backend = backend.ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_WEAVE_MATH_BACKEND_MISSING",
                    format!(
                        "panel {} has {records_woven} math-bearing records but execution class {} produced no backend",
                        params.panel_version,
                        params.math_execution_class.as_str()
                    ),
                    "repair the declared execution-class backend initialization; Synapse refuses to omit association math",
                )
            })?;
            store
                .materialize_plans(
                    backend,
                    params.panel_version,
                    corpus
                        .records
                        .iter()
                        .zip(&materialization_plans)
                        .filter_map(|(record, plan)| {
                            plan.as_ref()
                                .map(|plan| (record.cx_id, &record.slots, plan))
                        }),
                )
                .map_err(|error| {
                    loom_math_error(
                        "materialize panel-batched within-record cross-terms",
                        &error,
                    )
                })?
        };
        let agreement_zero_norm_records = materialization.zero_norm_records;
        let materialize_secs = materialize_started.elapsed().as_secs_f64();
        tracing::info!(
            code = "SYNAPSE_CALYX_WEAVE_MATERIALIZATION_COMPLETED",
            panel_version = params.panel_version,
            records_woven,
            cross_terms_inserted = materialization.inserted,
            backend_dispatches = materialization.backend_dispatches,
            zero_norm_records = materialization.zero_norm_records,
            corpus_load_secs,
            plan_secs,
            materialize_secs,
            "completed bounded panel-wide Loom materialization"
        );

        // #2076: lift Loom's agreement-lane skip ledger onto the report so the
        // unattended maintainer can name the lens pairs it could not score.
        // Without this the only zero-norm field on the report is the kNN one,
        // which is structurally empty on this panel, and the log truthfully
        // reported nothing about a corpus that is overwhelmingly zero-norm.
        let agreement_zero_norm_skips = store.zero_norm_agreement_skip_total();
        let agreement_zero_norm_sample_truncated =
            agreement_zero_norm_skips > store.zero_norm_agreement_skips().len();
        let agreement_zero_norm_slot_pairs = store
            .zero_norm_agreement_skips()
            .iter()
            .map(|skip| (skip.a.slot_id.get(), skip.b.slot_id.get()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let cross_terms_materialized = store.xterm_count();
        let agreement_graph = store
            .agreement_graph()
            .map_err(|error| loom_math_error("aggregate agreement graph", &error))?;
        let xterm_rows = store
            .into_xterm_kv_rows()
            .map_err(|error| loom_math_error("encode XTerm rows", &error))?;

        let mut writes: Vec<SynapseCalyxCfWrite> = Vec::with_capacity(xterm_rows.len());
        for (key, value) in xterm_rows {
            writes.push(SynapseCalyxCfWrite {
                cf: ColumnFamily::XTerm,
                key,
                value,
            });
        }

        let mut agreement_edges = Vec::with_capacity(agreement_graph.len());
        for edge in &agreement_graph {
            let slot_a = edge.a.slot_id().get();
            let slot_b = edge.b.slot_id().get();
            let out = SynapseCalyxAgreementEdge {
                panel_version: params.panel_version,
                slot_a,
                slot_b,
                mean_agreement: edge.mean_agreement,
                agreement_weight: edge.agreement_weight,
                n: edge.n,
            };
            writes.push(SynapseCalyxCfWrite {
                cf: ColumnFamily::Graph,
                key: agreement_edge_key(params.panel_version, slot_a, slot_b),
                value: encode_json(&out)?,
            });
            agreement_edges.push(out);
        }

        let knn_started = std::time::Instant::now();
        let (between_record_edges, knn_zero_norm_exclusions) =
            Self::build_between_record_edges(backend, &corpus, params.knn_k)?;
        let between_record_edges_persisted = between_record_edges.len();
        for edge in between_record_edges {
            writes.push(SynapseCalyxCfWrite {
                cf: ColumnFamily::Graph,
                key: between_record_edge_key(&edge),
                value: encode_json(&edge)?,
            });
        }
        let knn_secs = knn_started.elapsed().as_secs_f64();

        let persist_started = std::time::Instant::now();
        if !writes.is_empty() {
            self.write_cf_batch(writes)?;
            self.flush()?;
        }
        let persist_secs = persist_started.elapsed().as_secs_f64();

        // #2114: still a physical readback of what this weave persisted, but no
        // longer re-walked when the family provably has not changed since the
        // last walk. The provenance of each number rides along on the report.
        //
        // #2139 sharpened "the vault has not changed" to "this family has not
        // changed": on the deployed daemon the vault-wide condition was defeated
        // by every unrelated transcript-ingest commit, so the memo reused
        // nothing and the 2.75 M-row walks continued exactly as before.
        let xterm_readback = self.count_cf_latest_bounded_memoized(ColumnFamily::XTerm)?;
        let graph_readback = self.count_cf_latest_bounded_memoized(ColumnFamily::Graph)?;
        tracing::debug!(
            code = "SYNAPSE_CALYX_WEAVE_CF_COUNT_READBACK",
            panel_version = params.panel_version,
            xterm_provenance = xterm_readback.provenance(),
            xterm_rows = xterm_readback.rows(),
            xterm_cf_last_commit_seq = xterm_readback.cf_last_commit_seq,
            graph_provenance = graph_readback.provenance(),
            graph_rows = graph_readback.rows(),
            graph_cf_last_commit_seq = graph_readback.cf_last_commit_seq,
            vault_latest_seq = graph_readback.vault_latest_seq,
            "post-weave CF row counts, with the per-CF signal each decision was made on"
        );
        let xterm_cf_rows_after = xterm_readback.rows();
        let graph_cf_rows_after = graph_readback.rows();
        let total_secs = weave_started.elapsed().as_secs_f64();
        tracing::info!(
            code = "SYNAPSE_CALYX_WEAVE_PHASES_COMPLETED",
            panel_version = params.panel_version,
            records_scanned,
            records_woven,
            backend_dispatches = materialization.backend_dispatches,
            cross_terms_materialized,
            between_record_edges_persisted,
            corpus_load_secs,
            plan_secs,
            materialize_secs,
            knn_secs,
            persist_secs,
            total_secs,
            xterm_cf_rows_after,
            graph_cf_rows_after,
            "completed every Loom weave phase and independent physical CF count readback"
        );

        let mut abundance = self.build_abundance_report(
            params.panel_version,
            corpus.n_lenses(),
            corpus.records.len(),
            cross_terms_materialized,
            measured_slot_instances,
            cross_terms_materialized,
            xterm_cf_rows_after,
            graph_cf_rows_after,
            xterm_readback.provenance_kind(),
            graph_readback.provenance_kind(),
            xterm_readback.cf_last_commit_seq,
            graph_readback.cf_last_commit_seq,
            graph_readback.vault_latest_seq,
        )?;
        abundance.measurable_lenses = lens_ids.len();
        abundance.slot_states = corpus.slot_states();

        Ok(SynapseCalyxWeaveReport {
            panel_version: params.panel_version,
            records_scanned,
            records_woven,
            n_lenses: corpus.n_lenses(),
            measurable_lenses: lens_ids.len(),
            slot_states: corpus.slot_states(),
            cross_terms_materialized,
            agreement_edges_persisted: agreement_edges.len(),
            between_record_edges_persisted,
            knn_zero_norm_exclusions,
            agreement_zero_norm_skips,
            agreement_zero_norm_records,
            agreement_zero_norm_slot_pairs,
            agreement_zero_norm_sample_truncated,
            xterm_cf_rows_after,
            graph_cf_rows_after,
            xterm_cf_rows_readback: xterm_readback.provenance().to_owned(),
            graph_cf_rows_readback: graph_readback.provenance().to_owned(),
            since_ts_ns: params.since_ts_ns,
            until_ts_ns: params.until_ts_ns,
            records_outside_window,
            dda_signal_yield: dda_signal_yield(corpus.records.len(), corpus.n_lenses()),
            lens_pairs_possible: blind_spots.pairs_possible,
            lens_pairs_co_present: blind_spots.pairs_co_present,
            blind_spot_pairs: blind_spots.blind_pairs,
            blind_spot_fraction: blind_spots.blind_fraction,
            blind_spot_records: blind_spots.records_without_pair,
            blind_spot_slots: blind_spots.dark_slots,
            blind_spot_pair_details: blind_spots.blind_pair_details,
            agreement_edges,
            abundance,
        })
    }

    /// Measures how many lenses the records of each named panel actually carry,
    /// so an absent lens layer is reportable as a named deficiency rather than
    /// only as a zero inside a report an operator must know to ask for
    /// (issue #1894, ask 2).
    ///
    /// `abundance` already computed the alarm — `blind_spot_records = 1740` on a
    /// panel with 1,745 constellations — and nothing raised it. The number only
    /// appeared to whoever ran an intelligence pass by hand, which is precisely
    /// the class of surface #1891 established should be reported by `health`
    /// instead. This is that measurement, bounded so it can run on a periodic
    /// maintenance tick.
    ///
    /// A record carrying fewer than two co-present dense lenses contributes no
    /// within-record cross-term at all, so it is blind for every association-
    /// derived surface: weave, abundance, bits, redundancy and kernel.
    ///
    /// # Errors
    ///
    /// Returns a structured error when a panel corpus cannot be scanned or a
    /// constellation cannot be decoded or hydrated.
    pub fn lens_coverage_status(
        &self,
        panel_versions: &[u32],
        max_records: usize,
    ) -> Result<SynapseCalyxLensCoverageStatus, SynapseCalyxError> {
        let max_records = max_records.clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let mut panels = Vec::new();
        for panel_version in panel_versions {
            let corpus = self.load_panel_dense_corpus(*panel_version, max_records)?;
            // A panel with no rows at all is not a lens deficiency; it is an
            // empty panel, and reporting it as a blind spot would raise a false
            // alarm on every panel Synapse has registered but not yet filled.
            if corpus.records_scanned == 0 {
                continue;
            }
            let records_measured = corpus.records.len();
            let blind_spot_records = corpus
                .records
                .iter()
                .filter(|record| record.slots.len() < 2)
                .count();
            #[allow(
                clippy::cast_precision_loss,
                reason = "record counts are bounded by SYNAPSE_INTELLIGENCE_MAX_RECORDS"
            )]
            let blind_spot_fraction = if records_measured == 0 {
                0.0
            } else {
                blind_spot_records as f32 / records_measured as f32
            };
            panels.push(SynapseCalyxPanelLensCoverage {
                panel_version: *panel_version,
                records_scanned: corpus.records_scanned,
                records_measured,
                n_lenses: corpus.n_lenses(),
                measurable_slots: corpus
                    .measurable_slots()
                    .iter()
                    .map(|slot| slot.get())
                    .collect(),
                slot_states: corpus.slot_states(),
                blind_spot_records,
                blind_spot_fraction,
                degenerate_lanes: degenerate_lanes(*panel_version, &corpus),
            });
        }
        let deficient_panels = panels
            .iter()
            .filter(|panel| {
                panel.n_lenses < 2 || panel.blind_spot_fraction > SYNAPSE_LENS_BLIND_SPOT_CEILING
            })
            .map(|panel| panel.panel_version)
            .collect();
        let degenerate_lanes: Vec<SynapseCalyxDegenerateLane> = panels
            .iter()
            .flat_map(|panel| panel.degenerate_lanes.iter().cloned())
            .collect();
        Ok(SynapseCalyxLensCoverageStatus {
            panels,
            max_records_per_panel: max_records,
            deficient_panels,
            degenerate_lanes,
            blind_spot_ceiling: SYNAPSE_LENS_BLIND_SPOT_CEILING,
            measured_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok()),
        })
    }

    /// Reads back the derived-data abundance report for one panel from the
    /// physical `Base`, `XTerm`, and `Graph` CFs without re-weaving.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when any CF cannot be scanned or
    /// a `Base` row fails to decode.
    pub fn abundance_report(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<SynapseCalyxAbundanceReport, SynapseCalyxError> {
        let max_records = max_records.clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(panel_version, max_records)?;
        let mut lens_ids: BTreeSet<SlotId> = BTreeSet::new();
        let mut measured_slot_instances = 0usize;
        for record in &corpus.records {
            for slot in record.slots.keys() {
                lens_ids.insert(*slot);
            }
            measured_slot_instances += record.slots.len();
        }
        let xterm_readback = self.count_cf_latest_bounded_memoized(ColumnFamily::XTerm)?;
        let graph_readback = self.count_cf_latest_bounded_memoized(ColumnFamily::Graph)?;
        let xterm_cf_rows = xterm_readback.rows();
        let graph_cf_rows = graph_readback.rows();
        // `N` is the panel contract, not the subset one loader accepted: the
        // DDA yield `n·(N + C(N,2) + 1)` is a statement about the panel, and
        // shrinking `N` to the carried subset understates the association
        // structure the panel is designed to hold instead of reporting the
        // shortfall (#1939).
        let mut report = self.build_abundance_report(
            panel_version,
            corpus.n_lenses(),
            corpus.records.len(),
            xterm_cf_rows,
            measured_slot_instances,
            xterm_cf_rows,
            xterm_cf_rows,
            graph_cf_rows,
            xterm_readback.provenance_kind(),
            graph_readback.provenance_kind(),
            xterm_readback.cf_last_commit_seq,
            graph_readback.cf_last_commit_seq,
            graph_readback.vault_latest_seq,
        )?;
        report.measurable_lenses = lens_ids.len();
        report.slot_states = corpus.slot_states();
        Ok(report)
    }

    /// Assembles the abundance report, reading the DPI ceiling back out of the
    /// physical Assay CF.
    ///
    /// The ceiling is the total grounded bits a completed bits pass measured
    /// over this panel's lenses: by the data-processing inequality no derived
    /// cross-term over those lenses can carry more information about the outcome
    /// than the lenses themselves do (Cover & Thomas; `I(f(X);Y) <= I(X;Y)`).
    /// With no bits pass on record the ceiling stays `Provisional` — it is never
    /// guessed.
    #[allow(clippy::too_many_arguments)]
    fn build_abundance_report(
        &self,
        panel_version: u32,
        n_lenses: usize,
        n_constellations: usize,
        materialized: usize,
        measured_count: usize,
        derived_count: usize,
        xterm_cf_rows: usize,
        graph_cf_rows: usize,
        xterm_cf_rows_readback: SynapseCalyxCfCountProvenance,
        graph_cf_rows_readback: SynapseCalyxCfCountProvenance,
        xterm_cf_last_commit_seq: u64,
        graph_cf_last_commit_seq: u64,
        vault_latest_seq: u64,
    ) -> Result<SynapseCalyxAbundanceReport, SynapseCalyxError> {
        let measured_ceiling = self.measured_dpi_ceiling(panel_version)?;
        let dpi_ceiling = measured_ceiling.as_ref().map_or(
            CeilingEstimate::Provisional { bits: 0.0 },
            |ceiling| CeilingEstimate::Computed {
                bits: ceiling.total_bits,
            },
        );
        // The effective rank still requires an anchored redundancy pass; it is
        // reported provisional here rather than faked.
        let report = AbundanceReport::new(
            n_lenses,
            n_constellations,
            materialized,
            NeffEstimate::Provisional { value: 0.0 },
            dpi_ceiling,
            measured_count,
            derived_count,
        );
        let (dpi_ceiling_bits, dpi_ceiling_provisional) = match report.dpi_ceiling {
            CeilingEstimate::Computed { bits } => (Some(bits), false),
            CeilingEstimate::Provisional { .. } => (None, true),
        };
        Ok(SynapseCalyxAbundanceReport {
            panel_version,
            n_lenses: report.n_lenses,
            // Overwritten by callers that hold the corpus; a caller without one
            // reports the declared count as also measurable rather than
            // inventing a shortfall.
            measurable_lenses: report.n_lenses,
            slot_states: Vec::new(),
            n_constellations: report.n_constellations,
            c_n2_upper_bound: report.c_n2_upper_bound,
            materialized: report.materialized,
            measured_count: report.measured_count,
            derived_count: report.derived_count,
            meaning_compression_yield: report.meaning_compression_yield,
            dda_signal_yield: dda_signal_yield(report.n_constellations, report.n_lenses),
            n_eff: neff_estimate(&report.n_eff),
            dpi_ceiling_bits,
            dpi_ceiling_provisional,
            dpi_ceiling_anchor_kind: measured_ceiling.map(|ceiling| ceiling.anchor_kind),
            xterm_cf_rows,
            graph_cf_rows,
            xterm_cf_rows_readback,
            graph_cf_rows_readback,
            xterm_cf_last_commit_seq,
            graph_cf_last_commit_seq,
            vault_latest_seq,
        })
    }

    /// Reads the physical Assay CF back and returns the largest completed
    /// per-lens bits total recorded for this panel, with the anchor kind it was
    /// measured against. `None` when no bits pass has run for the panel.
    fn measured_dpi_ceiling(
        &self,
        panel_version: u32,
    ) -> Result<Option<MeasuredCeiling>, SynapseCalyxError> {
        let store = AssayStore::load_from_vault(&self.vault)
            .map_err(|error| loom_math_error("load persisted Assay rows", &error))?;
        // One bits pass writes one Lens row per measurable lens under a single
        // cache key (panel + corpus shard + vault + anchor kind), so summing per
        // cache key reproduces that pass's `total_bits` exactly. Panels measured
        // against several anchors keep the largest total: the tightest bound the
        // vault can actually prove.
        let mut totals: BTreeMap<(String, u32), f32> = BTreeMap::new();
        for row in store.rows() {
            if row.cache_key.panel_version != panel_version
                || row.cache_key.corpus_shard != ASSAY_CORPUS_SHARD
                || !matches!(row.subject, AssaySubject::Lens { .. })
                || row.estimate.estimator != EstimatorKind::Ksg
            {
                continue;
            }
            let anchor_kind = crate::grounding::anchor_kind_label(&row.cache_key.anchor);
            *totals.entry((anchor_kind, panel_version)).or_default() += row.estimate.bits;
        }
        Ok(totals
            .into_iter()
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| left.0.cmp(&right.0))
            })
            .map(|((anchor_kind, _), total_bits)| MeasuredCeiling {
                anchor_kind,
                total_bits,
            }))
    }

    /// Reads at most `max_records` matching `Base` rows and returns their
    /// dense-slot corpus for a panel, restricted to the requested `created_at`
    /// window.
    fn load_panel_dense_corpus(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<DenseCorpus, SynapseCalyxError> {
        self.load_panel_dense_corpus_in_window(panel_version, max_records, TimeWindowNs::default())
    }

    /// Reads a bounded `Base` prefix and returns the panel corpus.
    ///
    /// `panel_slots` records **every** lens the corpus carries with the kind of
    /// vector it actually stores, so the lens count equals the panel contract
    /// rather than the subset one code path happened to accept, and a lens the
    /// stack cannot consume is reported with a named reason instead of
    /// vanishing (issue #1939). It is collected from the hydrated records,
    /// because a Base row alone cannot say which of its slots are dense — it
    /// stores only slot ids and hashes (issue #1894).
    ///
    /// # Sparse lenses (#1939)
    ///
    /// A sparse slot is carried, not dropped, by **densifying it over its
    /// corpus-observed support**: the set of indices some record in the corpus
    /// actually occupies. That transformation is exact, not an approximation —
    /// every excluded index is zero in every record, so it contributes nothing
    /// to a dot product, a norm, a Chebyshev distance, or an interned
    /// whole-value identity. The densified column is therefore the *same*
    /// measurement, expressed in the width the association engine can consume,
    /// and every existing dense consumer (cross-terms, kNN, the MI estimators,
    /// the kernel) becomes correct for sparse lenses without a second
    /// implementation of the same math.
    ///
    /// The observed support is bounded by [`SYNAPSE_SPARSE_SLOT_MAX_SUPPORT`].
    /// Above it the slot is reported unusable with the measured support in the
    /// reason — never silently narrowed, and never densified into a width the
    /// bounded estimators cannot finish.
    #[allow(
        clippy::too_many_lines,
        reason = "the bounded snapshot scan and its sparse-support postprocessing share one corpus accumulator; splitting them would obscure that the derived shapes come only from the pinned scan"
    )]
    fn load_panel_dense_corpus_in_window(
        &self,
        panel_version: u32,
        max_records: usize,
        window: TimeWindowNs,
    ) -> Result<DenseCorpus, SynapseCalyxError> {
        if max_records == 0 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_INTELLIGENCE_MAX_RECORDS_ZERO",
                format!(
                    "panel {panel_version} corpus loading requires at least one record; max_records=0 cannot produce a measurable corpus"
                ),
                "pass max_records in 1..=SYNAPSE_INTELLIGENCE_MAX_RECORDS",
            ));
        }
        // A Base row carries only `(slot_id, slot_hash)` pairs: every slot it
        // decodes to is `SlotVector::Absent`, by design, because the vectors live
        // in the per-slot CFs. Building the corpus straight from the decoded Base
        // row therefore produced records with zero usable slots on EVERY panel,
        // so `n_lenses` was structurally always 0 and weave/abundance/bits/
        // redundancy/kernel were all vacuously zero — reported honestly, but
        // measuring nothing (issue #1894). Hydrate the slots from their CFs.
        let mut records: Vec<DenseRecord> = Vec::new();
        let mut records_scanned = 0usize;
        let mut records_outside_window = 0usize;
        let mut panel_slots: BTreeMap<SlotId, SynapseCalyxSlotKind> = BTreeMap::new();
        let mut sparse_support: BTreeMap<SlotId, BTreeSet<u32>> = BTreeMap::new();
        // The sealed panel membership sidecar is the selective access path.
        // #2239: `max_records` is also a traversal bound, not merely a
        // retained-vector bound. Exact whole-vault population questions belong
        // to the explicit panel census; intelligence work is measured over the
        // bounded panel corpus it actually hydrates.
        self.with_panel_read_snapshot(
            panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| {
                self.walk_panel_base_snapshot(snapshot, panel_version, |snapshot, _key, value| {
                    let base = decode_constellation_base(value).map_err(|error| {
                        SynapseCalyxError::from_calyx("decode Base constellation", &error)
                    })?;
                    if base.panel_version != panel_version {
                        return Ok(crate::SynapseCalyxWalkStep::Continue);
                    }
                    if !window.contains_created_at_ms(base.created_at) {
                        records_outside_window += 1;
                        return Ok(crate::SynapseCalyxWalkStep::Continue);
                    }
                    records_scanned += 1;
                    let hydrated = self.hydrated_constellation_at_snapshot(base.cx_id, snapshot)?;
                    for (slot, vector) in &hydrated.slots {
                        let kind = SynapseCalyxSlotKind::of(vector);
                        // A slot that is `Absent` on this record but present on
                        // another must not be recorded as absent for the panel:
                        // `Absent` is an explicit per-record absence, never a
                        // statement about the lens.
                        let entry = panel_slots.entry(*slot).or_insert(kind);
                        if *entry == SynapseCalyxSlotKind::Absent {
                            *entry = kind;
                        }
                        if let SlotVector::Sparse { entries, .. } = vector {
                            let support = sparse_support.entry(*slot).or_default();
                            for entry in entries {
                                support.insert(entry.idx);
                            }
                        }
                    }
                    records.push(DenseRecord::from_constellation(hydrated));
                    if records.len() >= max_records {
                        Ok(crate::SynapseCalyxWalkStep::Stop)
                    } else {
                        Ok(crate::SynapseCalyxWalkStep::Continue)
                    }
                })
            },
        )?;

        // Densify every sparse slot whose observed support fits the bound. This
        // happens after the scan because the support is a property of the
        // corpus, not of any one record.
        let mut densified_sparse_slots: BTreeMap<SlotId, usize> = BTreeMap::new();
        let mut unusable_slots: BTreeMap<SlotId, String> = BTreeMap::new();
        for (slot, support) in &sparse_support {
            if support.is_empty() {
                unusable_slots.insert(
                    *slot,
                    format!(
                        "sparse lens has empty observed support across the {} loaded record(s). The stored all-zero sparse value remains exact, but an observed-support densifier cannot manufacture a zero-dimensional vector for Loom/KSG/kNN",
                        records.len()
                    ),
                );
                continue;
            }
            if support.len() > SYNAPSE_SPARSE_SLOT_MAX_SUPPORT {
                unusable_slots.insert(
                    *slot,
                    format!(
                        "sparse lens occupies {} distinct index(es) across the {} scanned \
                         record(s), above the densification bound of \
                         {SYNAPSE_SPARSE_SLOT_MAX_SUPPORT}. Densifying over the observed support \
                         is exact, but at this width the bounded association and \
                         mutual-information passes cannot complete: the kNN and KSG paths are \
                         quadratic in records and linear in width. Narrow the lens (coarser \
                         hashing/bucketing) or lower max_records",
                        support.len(),
                        records.len()
                    ),
                );
                continue;
            }
            let index_of: BTreeMap<u32, usize> = support
                .iter()
                .enumerate()
                .map(|(position, idx)| (*idx, position))
                .collect();
            let width = support.len();
            for record in &mut records {
                let Some(entries) = record.sparse.get(slot) else {
                    continue;
                };
                let mut dense = vec![0.0f32; width];
                for (idx, value) in entries {
                    // Every occupied index is in the map by construction: the
                    // support was built from these same entries.
                    if let Some(position) = index_of.get(idx) {
                        dense[*position] += *value;
                    }
                }
                record.slots.insert(*slot, dense);
            }
            densified_sparse_slots.insert(*slot, width);
        }

        Ok(DenseCorpus {
            records,
            records_scanned,
            records_outside_window,
            panel_slots,
            densified_sparse_slots,
            unusable_slots,
        })
    }

    /// Builds the between-record nearest-neighbor graph: for every dense slot
    /// with at least two records of a uniform dimension, a bounded cosine kNN
    /// graph over the records that carry that slot.
    fn build_between_record_edges(
        backend: Option<&dyn Backend>,
        corpus: &DenseCorpus,
        knn_k: usize,
    ) -> Result<
        (
            Vec<SynapseCalyxBetweenRecordEdge>,
            Vec<SynapseCalyxKnnZeroNormExclusion>,
        ),
        SynapseCalyxError,
    > {
        let knn_k = knn_k.clamp(1, 64);
        let mut by_slot: BTreeMap<SlotId, Vec<(CxId, &Vec<f32>)>> = BTreeMap::new();
        for record in &corpus.records {
            for (slot, vector) in &record.slots {
                by_slot
                    .entry(*slot)
                    .or_default()
                    .push((record.cx_id, vector));
            }
        }
        let mut edges = Vec::new();
        let mut zero_norm_exclusions = Vec::new();
        for (slot, members) in by_slot {
            if edges.len() >= SYNAPSE_KNN_MAX_EDGES {
                break;
            }
            // Only records that share one dimension can be compared by cosine.
            let mut by_dim: BTreeMap<usize, Vec<(CxId, &Vec<f32>)>> = BTreeMap::new();
            for (cx, vector) in members {
                by_dim.entry(vector.len()).or_default().push((cx, vector));
            }
            for (dim, group) in by_dim {
                if dim == 0 {
                    continue;
                }
                let mut geometric = Vec::with_capacity(group.len());
                let mut excluded = Vec::new();
                for (cx_id, vector) in group {
                    let norm_squared = vector.iter().try_fold(0.0f64, |sum, value| {
                        if !value.is_finite() {
                            return Err(SynapseCalyxError::new(
                                "SYNAPSE_CALYX_KNN_VECTOR_NON_FINITE",
                                format!(
                                    "panel slot {} record {cx_id} contains a non-finite value in its {dim}-dimensional between-record kNN vector",
                                    slot.get()
                                ),
                                "repair or re-measure the named physical slot row; non-finite measurements cannot enter any association calculation",
                            ));
                        }
                        let value = f64::from(*value);
                        Ok(value.mul_add(value, sum))
                    })?;
                    if norm_squared == 0.0 {
                        excluded.push(cx_id);
                    } else {
                        geometric.push((cx_id, vector));
                    }
                }
                if !excluded.is_empty() {
                    zero_norm_exclusions.push(SynapseCalyxKnnZeroNormExclusion {
                        slot: slot.get(),
                        records: excluded.len(),
                        sample_cx_ids: excluded.iter().take(8).map(ToString::to_string).collect(),
                    });
                }
                if geometric.len() < 2 {
                    continue;
                }
                let backend = backend.ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_WEAVE_MATH_BACKEND_MISSING",
                        format!(
                            "slot {} dimension {dim} has {} kNN candidates but the weave owns no math backend",
                            slot.get(),
                            geometric.len()
                        ),
                        "repair the declared execution-class backend initialization; Synapse refuses to omit between-record association math",
                    )
                })?;
                append_slot_knn_edges(backend, slot, dim, &geometric, knn_k, &mut edges)?;
                if edges.len() >= SYNAPSE_KNN_MAX_EDGES {
                    break;
                }
            }
        }
        edges.truncate(SYNAPSE_KNN_MAX_EDGES);
        Ok((edges, zero_norm_exclusions))
    }
}

/// Largest corpus-observed sparse support the loader will densify (#1939).
///
/// Densifying a sparse slot over exactly the indices some record occupies is
/// lossless, so the bound is not about fidelity — it is about what the bounded
/// downstream passes can finish. The kNN and KSG paths are quadratic in records
/// and linear in slot width, and the widest dense lane any Syn* panel carries
/// today is 128, so 512 leaves four times that headroom while keeping one pass
/// bounded. A lens above it is reported unusable with its measured support,
/// never narrowed and never dropped.
const SYNAPSE_SPARSE_SLOT_MAX_SUPPORT: usize = 512;

/// What kind of vector a panel slot actually stores, measured from the hydrated
/// records rather than declared.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxSlotKind {
    Dense,
    Sparse,
    Multi,
    /// Explicitly absent on every record scanned. Never a zero vector (#1894).
    Absent,
}

impl SynapseCalyxSlotKind {
    const fn of(vector: &SlotVector) -> Self {
        match vector {
            SlotVector::Dense { .. } => Self::Dense,
            SlotVector::Sparse { .. } => Self::Sparse,
            SlotVector::Multi { .. } => Self::Multi,
            SlotVector::Absent { .. } => Self::Absent,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dense => "dense",
            Self::Sparse => "sparse",
            Self::Multi => "multi",
            Self::Absent => "absent",
        }
    }
}

/// One record's slot vectors in the form the association engine consumes.
///
/// `slots` holds every lens the engine can measure: the dense ones verbatim,
/// and the sparse ones densified over the corpus-observed support (#1939) —
/// which is exact, not lossy, because every excluded index is zero in every
/// record. `sparse` keeps the raw entries the densification is built from, so
/// the loader can widen a slot after the scan, when the corpus-wide support is
/// finally known.
struct DenseRecord {
    cx_id: CxId,
    slots: BTreeMap<SlotId, Vec<f32>>,
    sparse: BTreeMap<SlotId, Vec<(u32, f32)>>,
    anchors: Vec<Anchor>,
}

impl DenseRecord {
    fn from_constellation(constellation: Constellation) -> Self {
        let Constellation {
            cx_id,
            slots: constellation_slots,
            anchors,
            ..
        } = constellation;
        let mut slots = BTreeMap::new();
        let mut sparse = BTreeMap::new();
        for (slot, vector) in constellation_slots {
            match vector {
                SlotVector::Dense { data, .. } => {
                    slots.insert(slot, data);
                }
                SlotVector::Sparse { entries, .. } => {
                    sparse.insert(
                        slot,
                        entries
                            .into_iter()
                            .map(|entry| (entry.idx, entry.val))
                            .collect(),
                    );
                }
                SlotVector::Multi { .. } | SlotVector::Absent { .. } => {}
            }
        }
        Self {
            cx_id,
            slots,
            sparse,
            anchors,
        }
    }
}

struct DenseCorpus {
    records: Vec<DenseRecord>,
    records_scanned: usize,
    /// Panel rows excluded by the `created_at` window.
    records_outside_window: usize,
    /// **Every** lens the panel carries, with the kind of vector it stores.
    /// This is the lens count the panel contract declares; a surface that
    /// cannot consume one of them says so per slot (#1939).
    panel_slots: BTreeMap<SlotId, SynapseCalyxSlotKind>,
    /// Sparse slots carried into the corpus, mapped to the densified width
    /// (their corpus-observed support).
    densified_sparse_slots: BTreeMap<SlotId, usize>,
    /// Slots the loader could not carry, mapped to the named reason (#1939).
    unusable_slots: BTreeMap<SlotId, String>,
}

struct PersistedAnnealDefaults;

impl AnnealConfig for PersistedAnnealDefaults {
    fn energy_beta(&self, _domain: &DomainId) -> Option<f32> {
        Some(calyx_oracle::DEFAULT_BETA)
    }
}

impl SynapseCalyxVault {
    /// Completes selected action-panel slots from the bounded persisted trusted
    /// region and records both the measured self-consistency and completion in
    /// the append-only ledger.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the request or panel is invalid, the trusted
    /// corpus cannot be read, completion fails, or its ledger evidence cannot be appended.
    #[expect(
        clippy::too_many_lines,
        reason = "corpus selection, completion, self-consistency measurement, and ledger evidence form one ordered operation"
    )]
    pub fn oracle_complete(
        &self,
        cx_id: CxId,
        panel: &calyx_core::Panel,
        domain: &str,
        free_slot_ids: &BTreeSet<SlotId>,
    ) -> Result<serde_json::Value, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("oracle_complete");
        if free_slot_ids.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_ORACLE_COMPLETION_FREE_EMPTY",
                "Oracle completion requires at least one explicitly free slot",
                "supply one or more slot ids from the declared panel contract",
            ));
        }
        let by_slot = panel
            .slots
            .iter()
            .map(|slot| (slot.slot_id, slot.lens_id))
            .collect::<BTreeMap<_, _>>();
        for slot_id in free_slot_ids {
            if !by_slot.contains_key(slot_id) {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_ORACLE_COMPLETION_SLOT_UNKNOWN",
                    format!(
                        "slot {} is not declared by panel {}",
                        slot_id.get(),
                        panel.version
                    ),
                    "read the active panel contract and supply only one of its slot ids",
                ));
            }
        }
        let mut cx = self.hydrate_constellation_latest(cx_id)?;
        if cx.panel_version != panel.version {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_ORACLE_COMPLETION_PANEL_MISMATCH",
                format!(
                    "constellation {cx_id} belongs to panel {}, not {}",
                    cx.panel_version, panel.version
                ),
                "supply a constellation id read from the requested panel",
            ));
        }
        for slot_id in free_slot_ids {
            cx.slots.insert(
                *slot_id,
                SlotVector::Absent {
                    reason: AbsentReason::Deferred,
                },
            );
        }
        let corpus =
            self.load_panel_dense_corpus(panel.version, SYNAPSE_INTELLIGENCE_MAX_RECORDS)?;
        let regions = corpus
            .records
            .into_iter()
            .filter(|record| record.cx_id != cx_id && !record.anchors.is_empty())
            .map(|record| TrustedRegion {
                cx_id: record.cx_id,
                slots: record.slots,
            })
            .collect::<Vec<_>>();
        if regions.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_ORACLE_COMPLETION_REGION_EMPTY",
                format!(
                    "panel {} has no other anchored trusted-region members",
                    panel.version
                ),
                "ingest and ground other panel records before requesting completion",
            ));
        }
        let free = free_slot_ids
            .iter()
            .filter_map(|slot_id| by_slot.get(slot_id).copied())
            .collect::<SlotSet>();
        let clamp = by_slot
            .iter()
            .filter(|(slot_id, _)| !free_slot_ids.contains(slot_id))
            .map(|(_, lens_id)| *lens_id)
            .collect::<SlotSet>();
        let domain = DomainId::new(domain);
        let clock = SystemClock;
        let consistency =
            calyx_oracle::oracle_self_consistency(&self.vault, domain.clone(), &clock).map_err(
                |error| {
                    let error: calyx_core::CalyxError = error.into();
                    SynapseCalyxError::from_calyx(
                        "measure Oracle self-consistency before completion",
                        &error,
                    )
                },
            )?;
        let region = WardCompletionRegion::new(panel, &regions);
        let result = calyx_oracle::complete(
            &self.vault,
            &cx,
            panel,
            domain,
            clamp,
            free,
            &region,
            consistency,
            &PersistedAnnealDefaults,
            &clock,
        )
        .map_err(|error| {
            let error: calyx_core::CalyxError = error.into();
            SynapseCalyxError::from_calyx("complete Oracle constellation", &error)
        })?;
        serde_json::to_value(result).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_ORACLE_RESPONSE_ENCODE_FAILED",
                format!("encode Oracle completion response: {error}"),
                "preserve the vault and inspect the Oracle completion result schema",
            )
        })
    }
}

impl DenseCorpus {
    /// Slots the association engine can actually measure on: every dense lens
    /// plus every densified sparse lens.
    fn measurable_slots(&self) -> BTreeSet<SlotId> {
        self.panel_slots
            .iter()
            .filter(|(slot, kind)| {
                (**kind == SynapseCalyxSlotKind::Dense
                    || self.densified_sparse_slots.contains_key(*slot))
                    && !self.unusable_slots.contains_key(*slot)
            })
            .map(|(slot, _)| *slot)
            .collect()
    }

    /// The panel's declared lens count — what `n_lenses` must report, so a dark
    /// lens shows up as a blind spot rather than shrinking the denominator
    /// (#1939).
    fn n_lenses(&self) -> usize {
        self.panel_slots.len()
    }

    /// Per-slot report of what the loader did with each declared lens.
    fn slot_states(&self) -> Vec<SynapseCalyxCorpusSlotState> {
        self.panel_slots
            .iter()
            .map(|(slot, kind)| SynapseCalyxCorpusSlotState {
                slot: slot.get(),
                kind: kind.as_str().to_owned(),
                measurable: !self.unusable_slots.contains_key(slot)
                    && (*kind == SynapseCalyxSlotKind::Dense
                        || self.densified_sparse_slots.contains_key(slot)),
                densified_support: self.densified_sparse_slots.get(slot).copied(),
                unusable_reason: self.unusable_slots.get(slot).cloned(),
            })
            .collect()
    }
}

/// What the corpus loader did with one declared panel lens (#1939).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxCorpusSlotState {
    pub slot: u16,
    /// `dense` | `sparse` | `multi` | `absent`.
    pub kind: String,
    /// True when the association engine can measure on this slot.
    pub measurable: bool,
    /// For a carried sparse lens, the corpus-observed support it was densified
    /// over — the exact width, with every all-zero index excluded.
    pub densified_support: Option<usize>,
    /// Why this lens is not measurable; present exactly when `measurable` is
    /// false and the loader has a specific reason beyond its vector kind.
    pub unusable_reason: Option<String>,
}

/// What the kernel corpus loader saw on the requested content slot for records
/// it could not use.
///
/// Counted rather than discarded so that "0 embedded concepts" is never a bare
/// number. The same class as #1894: a decode path that returns nothing and a
/// caller that reads nothing as "there is nothing here".
#[derive(Debug, Default)]
struct ContentSlotRejects {
    multi: usize,
    absent: usize,
    empty: usize,
}

impl ContentSlotRejects {
    fn record(&mut self, kind: &str) {
        match kind {
            "multi" => self.multi += 1,
            "empty" => self.empty += 1,
            _ => self.absent += 1,
        }
    }

    /// Vectors of a kind that can never be a kernel content slot, as opposed to
    /// records that simply carry no measurement there.
    const fn wrong_kind(&self) -> usize {
        self.multi
    }

    fn describe(&self) -> String {
        format!(
            "{} multi, {} absent, {} empty",
            self.multi, self.absent, self.empty
        )
    }
}

fn append_slot_knn_edges(
    backend: &dyn Backend,
    slot: SlotId,
    dim: usize,
    group: &[(CxId, &Vec<f32>)],
    knn_k: usize,
    edges: &mut Vec<SynapseCalyxBetweenRecordEdge>,
) -> Result<(), SynapseCalyxError> {
    let count = group.len();
    let mut flat = Vec::with_capacity(count * dim);
    for (_, vector) in group {
        flat.extend_from_slice(vector);
    }
    // Ask for k+1 neighbors because the nearest neighbor of any record is
    // itself; the self match is dropped below.
    let k = (knn_k + 1).min(count);
    let batch = backend
        .knn(&flat, &flat, count, dim, k, KnnMetric::Cosine)
        .map_err(|error| forge_math_error("between-record kNN", &error))?;
    for (query_index, (query_cx, _)) in group.iter().enumerate() {
        let base = query_index * batch.k;
        let mut rank = 0usize;
        for offset in 0..batch.k {
            let candidate_index = batch.indices[base + offset];
            if candidate_index == query_index {
                continue;
            }
            let score = batch.scores[base + offset];
            let (candidate_cx, _) = group[candidate_index];
            rank += 1;
            edges.push(SynapseCalyxBetweenRecordEdge {
                slot: slot.get(),
                cx_a: query_cx.to_string(),
                cx_b: candidate_cx.to_string(),
                score,
                rank,
            });
            if rank >= knn_k || edges.len() >= SYNAPSE_KNN_MAX_EDGES {
                break;
            }
        }
    }
    Ok(())
}

/// The DPI ceiling proven by a completed bits pass, with the anchor it was
/// measured against.
struct MeasuredCeiling {
    anchor_kind: String,
    total_bits: f32,
}

/// Blind-spot summary for one woven panel.
struct BlindSpotSummary {
    pairs_possible: usize,
    pairs_co_present: usize,
    blind_pairs: usize,
    blind_fraction: f32,
    records_without_pair: usize,
    dark_slots: Vec<u16>,
    blind_pair_details: Vec<SynapseCalyxWeaveBlindSpotPair>,
}

/// Computes which lens pairs never co-occur on a measured record (so no
/// cross-term over them can ever exist), which lenses are dark inside the woven
/// window, and how many records carry too few lenses to weave at all.
#[allow(clippy::cast_precision_loss)]
fn blind_spot_summary(corpus: &DenseCorpus) -> BlindSpotSummary {
    let mut present_counts: BTreeMap<SlotId, usize> = BTreeMap::new();
    let mut co_present: BTreeSet<(SlotId, SlotId)> = BTreeSet::new();
    let mut records_without_pair = 0usize;
    for record in &corpus.records {
        let slots: Vec<SlotId> = record.slots.keys().copied().collect();
        if slots.len() < 2 {
            records_without_pair += 1;
        }
        for slot in &slots {
            *present_counts.entry(*slot).or_default() += 1;
        }
        for (index, a) in slots.iter().enumerate() {
            for b in &slots[index + 1..] {
                co_present.insert((*a, *b));
            }
        }
    }

    let lenses: Vec<SlotId> = present_counts.keys().copied().collect();
    let pairs_possible = cross_term_upper_bound(lenses.len());
    let pairs_co_present = co_present.len();
    let blind_pairs = pairs_possible.saturating_sub(pairs_co_present);
    let mut blind_pair_details = Vec::new();
    'outer: for (index, a) in lenses.iter().enumerate() {
        for b in &lenses[index + 1..] {
            if co_present.contains(&(*a, *b)) {
                continue;
            }
            if blind_pair_details.len() >= SYNAPSE_WEAVE_MAX_BLIND_SPOTS {
                break 'outer;
            }
            blind_pair_details.push(SynapseCalyxWeaveBlindSpotPair {
                slot_a: a.get(),
                slot_b: b.get(),
                records_with_a: present_counts.get(a).copied().unwrap_or_default(),
                records_with_b: present_counts.get(b).copied().unwrap_or_default(),
            });
        }
    }
    // Dark over the whole declared panel, not just the slots one code path
    // accepted: a lens the loader could not carry is exactly a blind spot, and
    // reporting it as "not dark" because it never entered the corpus is the
    // defect #1939 named (#1939).
    let dark_slots = corpus
        .panel_slots
        .keys()
        .filter(|slot| !present_counts.contains_key(*slot))
        .map(|slot| slot.get())
        .collect();

    BlindSpotSummary {
        pairs_possible,
        pairs_co_present,
        blind_pairs,
        blind_fraction: if pairs_possible == 0 {
            0.0
        } else {
            blind_pairs as f32 / pairs_possible as f32
        },
        records_without_pair,
        dark_slots,
        blind_pair_details,
    }
}

const fn neff_estimate(estimate: &NeffEstimate) -> SynapseCalyxNeffEstimate {
    match estimate {
        NeffEstimate::Provisional { value } => SynapseCalyxNeffEstimate {
            value: *value,
            provisional: true,
            ci_low: None,
            ci_high: None,
        },
        NeffEstimate::Computed {
            value,
            ci_low,
            ci_high,
        } => SynapseCalyxNeffEstimate {
            value: *value,
            provisional: false,
            ci_low: Some(*ci_low),
            ci_high: Some(*ci_high),
        },
    }
}

fn agreement_edge_key(panel_version: u32, slot_a: u16, slot_b: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(GRAPH_AGREEMENT_PREFIX.len() + 8);
    key.extend_from_slice(GRAPH_AGREEMENT_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(&slot_a.to_be_bytes());
    key.extend_from_slice(&slot_b.to_be_bytes());
    key
}

fn between_record_edge_key(edge: &SynapseCalyxBetweenRecordEdge) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(GRAPH_KNN_PREFIX.len() + 3 + edge.cx_a.len() + edge.cx_b.len());
    key.extend_from_slice(GRAPH_KNN_PREFIX);
    key.extend_from_slice(&edge.slot.to_be_bytes());
    key.extend_from_slice(edge.cx_a.as_bytes());
    key.push(0x00);
    key.extend_from_slice(edge.cx_b.as_bytes());
    key
}

fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, SynapseCalyxError> {
    serde_json::to_vec(value).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_INTELLIGENCE_ENCODE",
            format!("encode intelligence CF row: {error}"),
            "inspect the intelligence report shape; a derived CF row failed to serialize",
        )
    })
}

fn loom_math_error(action: &str, error: &calyx_core::CalyxError) -> SynapseCalyxError {
    SynapseCalyxError::from_calyx(action, error)
}

fn forge_math_error(action: &str, error: &calyx_forge::ForgeError) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_INTELLIGENCE_FORGE",
        format!("{action}: Forge math backend failed: {error}"),
        "repair the process-local/host GPU Source of Truth or select math_backend=cpu, then retry",
    )
}

// ---------------------------------------------------------------------------
// Assay bits / sufficiency / redundancy (#1672)
//
// Differentiate: measure in bits which lenses carry grounded signal about a
// real outcome anchor (KSG mutual information), whether the panel collectively
// explains the outcome (I(panel;anchor) >= H(anchor)), and how many lenses are
// truly non-redundant (effective rank over a pairwise normalized-MI matrix).
// Results persist to the native Assay CF and are read back.
// ---------------------------------------------------------------------------

/// Signal floor (bits) governing the sole-carrier attribution threshold and the
/// admission contract. Calyx default (handbook section 13).
pub const SYNAPSE_ASSAY_BIT_FLOOR: f32 = 0.05;
/// Pairwise-redundancy normalized-MI ceiling. Calyx default (handbook section 13).
pub const SYNAPSE_ASSAY_CORRELATION_CEILING: f32 = 0.6;
/// Minimum paired samples before a bits/sufficiency result is trusted rather
/// than tagged provisional (handbook section 11, `MIN_ASSAY_SAMPLES`).
pub const SYNAPSE_ASSAY_MIN_SAMPLES: usize = 50;
/// Default k for the KSG mutual-information estimator.
pub const SYNAPSE_KSG_DEFAULT_K: usize = 4;
/// Histogram bins for the pairwise normalized-MI redundancy sketch.
pub const SYNAPSE_REDUNDANCY_NMI_BINS: usize = 8;
/// Record cap for one synergy pass. Every pair costs three quadratic KSG
/// estimates, so the synergy budget is deliberately tighter than the shared
/// [`SYNAPSE_INTELLIGENCE_MAX_RECORDS`] ceiling.
pub const SYNAPSE_SYNERGY_MAX_RECORDS: usize = 2_000;
/// Lens cap for one synergy pass: the lenses with the highest marginal bits are
/// paired, and the truncation is reported (`n_lenses` vs `lenses_paired`).
pub const SYNAPSE_SYNERGY_MAX_LENSES: usize = 8;
/// Record cap for one ensemble capability-card pass. Each of the `C(N,2)` pairs
/// costs two multi-seed logistic probes (the estimate and its power control),
/// so the budget is tighter still than the synergy pass.
pub const SYNAPSE_ENSEMBLE_MAX_RECORDS: usize = 1_000;

const ASSAY_CORPUS_SHARD: &str = "synapse-intelligence";

/// Bounded request describing one Assay bits/sufficiency/redundancy pass.
#[derive(Clone, Debug)]
pub struct SynapseCalyxAssayParams {
    pub panel_version: u32,
    /// Exact persisted Assay scope. Oracle consumers use their domain id;
    /// general intelligence reports use `synapse-intelligence`.
    pub corpus_shard: String,
    /// Grounded outcome anchor to measure bits about. Synapse writes outcome
    /// anchors as `AnchorKind::Label(<name>)`; a few canonical names map to the
    /// native kinds.
    pub anchor_kind: String,
    pub max_records: usize,
    pub ksg_k: usize,
    /// Declared lens name per physical slot id, supplied by the layer that owns
    /// the panel declarations (issue #1897).
    ///
    /// This crate sits below the crate that declares the Synapse panels, so it
    /// cannot resolve "slot 29" to `syn.agent_event.error_onehot.v1` on its own,
    /// and the vault's published panel state only covers the single *active*
    /// panel — not the panel a redundancy pass is usually asked about. Without
    /// this the reports could only name bare slot numbers, which is exactly the
    /// localisation failure #1897 was filed about. An empty map is honest: every
    /// slot is then reported as unnamed rather than guessed at.
    pub lens_names: BTreeMap<u16, String>,
    /// Physical slot ids withheld from this measurement (issue #1953).
    ///
    /// A panel that contains its own label reports `sufficient=true` circularly:
    /// on `syn-mcp-usage-v1` the anchor is `record.status` and slot 86 is a
    /// one-hot of `record.status`, so the panel was measured as explaining an
    /// outcome it literally contained. The detector added for #1953 ask 2 makes
    /// that visible and refuses the verdict, but refusing is not measuring —
    /// the question the panel exists to answer ("how much do the *observable*
    /// features say about the outcome") stayed unmeasured because there was no
    /// way to withhold a lens.
    ///
    /// This is that way. Withholding is deliberately explicit and per-request
    /// rather than a permanent property of the slot: measuring a panel that
    /// contains its own label is legitimate when done knowingly, and which
    /// lenses leak depends on which anchor is adjudicating.
    ///
    /// Applied at sample-gathering time, so an excluded slot is absent from the
    /// per-lens bits, from the joint panel estimate, from the deficit split, and
    /// from the leakage detector alike. Excluding it from only some of those
    /// would produce a report whose parts describe different panels.
    pub excluded_slots: BTreeSet<u16>,
}

impl SynapseCalyxAssayParams {
    #[must_use]
    pub fn new(panel_version: u32, anchor_kind: String) -> Self {
        Self {
            panel_version,
            corpus_shard: ASSAY_CORPUS_SHARD.to_owned(),
            anchor_kind,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            ksg_k: SYNAPSE_KSG_DEFAULT_K,
            lens_names: BTreeMap::new(),
            excluded_slots: BTreeSet::new(),
        }
    }

    /// Caps the records one pass reads. Each pass clamps this to its own
    /// budget; the clamp is a property of the pass, not of the request.
    #[must_use]
    pub const fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }

    /// Attaches the declared slot-to-lens-name catalog used to localise every
    /// per-lens finding in the resulting report.
    #[must_use]
    pub fn with_lens_names(mut self, lens_names: BTreeMap<u16, String>) -> Self {
        self.lens_names = lens_names;
        self
    }

    #[must_use]
    pub fn with_corpus_shard(mut self, corpus_shard: String) -> Self {
        self.corpus_shard = corpus_shard;
        self
    }

    /// The declared lens name for one slot, or an explicit unnamed marker.
    fn lens_name(&self, slot: u16) -> String {
        self.lens_names
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| format!("<unnamed slot {slot}>"))
    }
}

/// Why one lens carries no bits estimate (#1915).
///
/// A skipped lens used to be indistinguishable from a measured zero: both
/// reported `marginal_bits: 0.0` with a `[0,0]` interval. On the live vault
/// every slot sat below the sample floor, so **every** reported zero was a skip
/// and no `bits` value on this system had ever come from the estimator. Naming
/// the reason is what makes "we did not measure this" readable instead of
/// arriving in the same field, in the same units, as a finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxSlotBitsState {
    /// The estimator ran and its output is in `marginal_bits`/`ci_*`.
    Measured,
    /// Fewer than [`SYNAPSE_ASSAY_MIN_SAMPLES`] paired samples.
    InsufficientSamples,
    /// The estimator refused this column: too many exact same-class duplicates
    /// for a non-zero kth-neighbour radius. A categorical lens hits this by
    /// construction — a one-hot over C values and N same-class samples yields
    /// about N/C duplicates — so it is a property of the lens, not of the data.
    DegenerateColumn,
    /// The estimator refused this column for some other structured reason.
    ///
    /// #1915's fix caught exactly one code, `CALYX_ASSAY_DEGENERATE_INPUT`,
    /// because that was the one the fixture produced. The live vault then
    /// produced `CALYX_ASSAY_INSUFFICIENT_SAMPLES` — a minority anchor class
    /// smaller than `k+1` — through the whole-pass abort the same fix had
    /// otherwise removed, and every other slot went unreported again.
    ///
    /// Whitelisting codes was the mistake. A refusal from the estimator is a
    /// fact about ONE column whatever its code, so the classification is now
    /// total: named states for the codes we understand, this for the rest, and
    /// no path back to aborting the report. The originating code is carried in
    /// `unmeasured_reason` so a new failure mode is still diagnosable.
    EstimatorRefused,
}

impl SynapseCalyxSlotBitsState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::InsufficientSamples => "insufficient_samples",
            Self::DegenerateColumn => "degenerate_column",
            Self::EstimatorRefused => "estimator_refused",
        }
    }

    /// Whether `marginal_bits` on this slot is an estimate rather than a
    /// placeholder. Callers deriving anything from the number must check this.
    #[must_use]
    pub const fn is_measured(self) -> bool {
        matches!(self, Self::Measured)
    }
}

/// One lens whose declared source fields intersect the anchor's determining
/// fields — the lens is reading (part of) the label rather than evidence about
/// it (#1958, #1959).
///
/// This is the *structural* verdict from [`lens_provenance::syn_anchor_source_provenance`],
/// so it is a pure function of `(anchor kind, panel version, excluded slots)`:
/// it says the same thing on an empty vault and a full one, and no estimator or
/// sample count can change it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxAnchorSourceCarrier {
    /// Physical panel slot.
    pub slot: u16,
    /// The slot's declared lens name.
    pub lens: String,
    /// The record fields it shares with the anchor. Never empty.
    pub shared_fields: Vec<String>,
}

/// Renders the structural verdict as the report-facing carrier list.
fn anchor_source_carriers(
    provenance: &lens_provenance::AnchorSourceProvenance,
) -> Vec<SynapseCalyxAnchorSourceCarrier> {
    provenance
        .carriers
        .iter()
        .map(|carrier| SynapseCalyxAnchorSourceCarrier {
            slot: carrier.slot,
            lens: carrier.lens.to_owned(),
            shared_fields: carrier
                .shared_fields
                .iter()
                .map(|field| (*field).to_owned())
                .collect(),
        })
        .collect()
}

/// A synergy pass together with the structural anchor-source verdict it has to
/// be read under (#1959 ask 1).
///
/// `synergy` is the surface this matters most on after `sufficiency`: a pair
/// that includes a carrier reports interaction bits over a concatenated column
/// that *contains the label*, and unlike `bits` there is no per-slot row for a
/// reader to notice it in. So the verdict travels with the report, the pairs
/// that contain a carrier are counted, and the headline `max_gain_bits` gets a
/// carrier-free twin that is a claim about prediction rather than about reading
/// the outcome back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxSynergyReport {
    /// The measured pairs exactly as `calyx_assay` produced them.
    pub report: SynergyReport,
    /// Whether this (anchor kind, panel version) pair declares its determining
    /// fields at all, so the check could run. `false` means it did not run.
    pub anchor_source_declared: bool,
    /// Panel slots whose declared source fields intersect the anchor's.
    pub anchor_source_carriers: Vec<SynapseCalyxAnchorSourceCarrier>,
    /// Evaluated pairs with at least one carrier half.
    pub pairs_with_anchor_source_carrier: usize,
    /// Largest measured gain over the pairs containing **no** carrier; `0.0`
    /// when no such pair was measurable. Equal to `report.max_gain_bits` when
    /// [`Self::anchor_source_carriers`] is empty.
    pub max_gain_bits_carrier_free: f32,
}

impl SynapseCalyxSynergyReport {
    /// The declared carriers among a pair's two halves, in slot order.
    #[must_use]
    pub fn carrier_slots_in_pair(&self, slot_a: u16, slot_b: u16) -> Vec<u16> {
        let mut slots: Vec<u16> = self
            .anchor_source_carriers
            .iter()
            .map(|carrier| carrier.slot)
            .filter(|slot| *slot == slot_a || *slot == slot_b)
            .collect();
        slots.sort_unstable();
        slots
    }
}

/// Per-lens grounded bits about the requested anchor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxSlotBits {
    pub slot: u16,
    pub marginal_bits: f32,
    pub ci_low: f32,
    pub ci_high: f32,
    pub n_samples: usize,
    pub sole_carrier: bool,
    pub provisional: bool,
    /// Whether `marginal_bits` was estimated, and if not, why (#1915).
    ///
    /// `marginal_bits` is `0.0` for every non-`Measured` state. That zero is a
    /// placeholder and must not be read as "this lens carries no information".
    pub state: SynapseCalyxSlotBitsState,
    /// What this slot needs before it can be measured, when it was not.
    /// `None` when `state` is `Measured`.
    pub unmeasured_reason: Option<String>,
    /// Which estimator produced `marginal_bits` — `discrete_plugin` or
    /// `continuous_ksg` (#1672).
    ///
    /// A panel deliberately mixes explicit encoders (one-hot, hash, cyclic)
    /// with continuous ones (record vectors, rank scalars), and the two need
    /// different instruments: KSG's k-th neighbour radius is zero by
    /// construction on a categorical column, so it does not merely lose
    /// precision there, it is undefined. Naming the instrument on every slot
    /// keeps a bits number comparable across the panel and auditable.
    pub estimator: Option<String>,
    /// Why that estimator was chosen, with the counts the rule keyed on.
    pub estimator_selection: Option<String>,
    pub estimator_reason: Option<String>,
    /// Distinct exact coordinate tuples observed in this column.
    pub distinct_values: Option<usize>,
    /// Largest exact-duplicate class within one outcome label — the quantity
    /// that drives KSG's k-th radius to zero.
    pub max_same_label_multiplicity: Option<usize>,
    /// This lens's declared source fields intersect the anchor's determining
    /// fields, so `marginal_bits` here is the label reading itself (#1959).
    ///
    /// `bits` annotates rather than refuses, because a per-lens report is the
    /// legitimate way to *inspect* a carrier — but an unmarked carrier row is
    /// how #1958's circular measurement went unnoticed, and "a human can spot
    /// the implausibly exact number" is the standard that already failed.
    pub anchor_source_carrier: bool,
    /// The record fields this lens shares with the anchor. Empty unless
    /// [`Self::anchor_source_carrier`].
    pub anchor_source_shared_fields: Vec<String>,
}

/// Result of one Assay bits pass with the physical Assay CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the public assay readback preserves independent measured/provisional/grounded state instead of collapsing them into an ambiguous mode"
)]
pub struct SynapseCalyxBitsReport {
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: usize,
    pub distinct_outcomes: usize,
    pub total_bits: f32,
    /// [`Self::total_bits`] over the slots that are **not** declared anchor
    /// source carriers (#1959).
    ///
    /// Equal to `total_bits` when [`Self::anchor_source_carriers`] is empty.
    /// When it is not, this is the only one of the two that is a statement
    /// about prediction rather than about reading the label back.
    pub total_bits_carrier_free: f32,
    /// Whether this (anchor kind, panel version) pair has a **declared** set of
    /// determining record fields, so the structural carrier check could run.
    ///
    /// `false` means the check did not run — not that it ran and found nothing.
    /// Without this, "checked and clean" and "never checked" are the same empty
    /// list, which is the exact shape of #1953's original defect.
    pub anchor_source_declared: bool,
    /// Every slot in the panel whose declared source fields intersect the
    /// anchor's determining fields, whether or not this corpus yielded samples
    /// for it — the check is structural, not a function of traffic (#1958).
    ///
    /// Non-empty means at least one row below is the label reading itself, and
    /// [`Self::total_bits`] sums it in. `bits` reports rather than refuses,
    /// because inspecting a carrier is a legitimate reason to run it.
    pub anchor_source_carriers: Vec<SynapseCalyxAnchorSourceCarrier>,
    /// Whether the corpus could support a bits measurement at all (#1897).
    ///
    /// `false` means the zeros in this report are the absence of a measurement,
    /// not a measured zero — a distinction a caller previously had to infer by
    /// cross-reading `anchored_records`, `distinct_outcomes` and an empty
    /// `slots` list.
    pub measurable: bool,
    /// Why the corpus could not support a measurement, when `measurable` is
    /// false. Always `None` when it is true.
    pub unmeasurable_reason: Option<String>,
    /// Assay trust tag: whether *this measurement* had enough paired samples.
    /// Distinct from `domain_provisional`, which is about anchor coverage.
    pub grounded: bool,
    /// Control-doctrine marker (#1670): the domain's grounded anchor coverage is
    /// below [`crate::SYNAPSE_GROUNDING_COVERAGE_FLOOR`], so this result may only
    /// advise, never control.
    pub domain_provisional: bool,
    /// Fraction of the domain's measured records carrying a grounded anchor.
    pub domain_grounded_fraction: f32,
    pub slots: Vec<SynapseCalyxSlotBits>,
    pub assay_cf_rows_after: usize,
}

/// One localized sufficiency deficit routed to a logged propose-lens suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxSufficiencyDeficit {
    pub slot: Option<u16>,
    pub deficit_bits: f32,
    pub suggested_action: String,
    pub reason: String,
}

/// Result of one Assay panel-sufficiency pass with the physical Assay readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the public sufficiency readback preserves independent estimator, carrier, grounding, and verdict facts"
)]
pub struct SynapseCalyxSufficiencyReport {
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: usize,
    pub joint_records: usize,
    pub panel_bits: f32,
    /// Whether `panel_bits` is an estimate rather than a placeholder (#1915).
    ///
    /// `false` means the joint estimator never ran — too few paired samples, or
    /// a degenerate joint column. `panel_bits` is then `0.0` as a placeholder,
    /// and `sufficient`/`deficit_bits`/`deficits` are suppressed rather than
    /// derived from it, because subtracting a placeholder from a genuinely
    /// computed `anchor_entropy_bits` yields a real-looking deficit and a
    /// `ProposeLens` recommendation against lenses nobody measured.
    pub panel_measured: bool,
    /// Whether `panel_bits` was raised to the best single-lens estimate because
    /// the joint estimator returned less than a lens it contains (#1916).
    ///
    /// `true` means the reported value is a defensible lower bound rather than
    /// the raw joint estimate. Raising it silently would hide that the joint
    /// estimator is under-performing on this panel's dimensionality, which is
    /// itself worth seeing.
    pub panel_floor_applied: bool,
    /// Slots excluded from the attribution because they could not be measured.
    pub unmeasured_slots: usize,
    /// Exact physical slots represented by persisted per-lens Assay rows.
    pub measured_slots: Vec<SlotId>,
    pub anchor_entropy_bits: f32,
    /// `false` whenever [`Self::anchor_leakage`] is non-empty, regardless of
    /// the bits: a panel that contains its own label has not been shown to
    /// predict anything (#1953).
    pub sufficient: bool,
    pub deficit_bits: f32,
    /// Lenses that ARE the anchor rather than evidence about it.
    ///
    /// Non-empty means the sufficiency verdict above is circular and must not
    /// be read as a capability claim. Reported rather than fatal, because
    /// measuring a panel that includes its own label is a legitimate thing to
    /// do deliberately — what must not happen is doing it silently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchor_leakage: Vec<AnchorLeakage>,
    /// Whether this (anchor kind, panel version) pair has a **declared** set of
    /// determining record fields, so the structural leakage check (#1958) could
    /// run at all.
    ///
    /// A report can only reach a caller when the structural check found no
    /// carrier — a carrier is a refusal, not a field. So this is the difference
    /// between "checked and clean" and "not checked", and without it those two
    /// are the same empty result. That confusion is the exact shape of #1953's
    /// original defect, where a circular measurement was indistinguishable from
    /// a sound one.
    #[serde(default)]
    pub anchor_source_declared: bool,
    /// Assay trust tag for this measurement's sample count.
    pub grounded: bool,
    /// Control-doctrine marker (#1670): domain anchor coverage below the floor.
    pub domain_provisional: bool,
    /// Fraction of the domain's measured records carrying a grounded anchor.
    pub domain_grounded_fraction: f32,
    pub deficits: Vec<SynapseCalyxSufficiencyDeficit>,
    pub assay_cf_rows_after: usize,
}

/// Fraction of a panel's measured records that may carry fewer than two
/// co-present dense lenses before the panel is reported as deficient.
///
/// A record with fewer than two lenses contributes no within-record cross-term,
/// so every association-derived surface is blind to it. A small tail of such
/// records is normal — optional fields are legitimately absent on some rows —
/// but a majority means the panel is not carrying the lens layer the
/// intelligence stack is built on, which is the condition #1894 found reported
/// as a silent zero.
pub const SYNAPSE_LENS_BLIND_SPOT_CEILING: f32 = 0.5;

/// Lens coverage measured over one panel's records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxPanelLensCoverage {
    pub panel_version: u32,
    /// Panel rows seen in the Base scan.
    pub records_scanned: usize,
    /// Rows actually hydrated and measured, bounded by the pass budget.
    pub records_measured: usize,
    /// Lenses the panel **declares** — every slot the measured records carry,
    /// whatever its vector kind. A lens the stack cannot consume is reported in
    /// `slot_states`, never subtracted from this count (#1939).
    pub n_lenses: usize,
    /// Lenses the association engine can measure on: the dense ones plus the
    /// sparse ones the loader densified over their observed support.
    pub measurable_slots: Vec<u16>,
    /// What the loader did with each declared lens, per slot (#1939).
    pub slot_states: Vec<SynapseCalyxCorpusSlotState>,
    /// Measured records carrying fewer than two co-present measurable lenses.
    pub blind_spot_records: usize,
    pub blind_spot_fraction: f32,
    /// Dense lanes that took the **same value on every record that carries
    /// them**, so they cannot rank anything on this corpus (issue #1970).
    pub degenerate_lanes: Vec<SynapseCalyxDegenerateLane>,
}

/// A dense lane that is constant *by corpus* (issue #1970).
///
/// Distinct from the two gates that already exist, and caught by neither:
///
/// - `CALYX_PANEL_SLOT_COSINE_CONSTANT` (#1963) refuses an encoder that is
///   constant *by construction* — its image is a single direction whatever it is
///   fed. It fires at admission, before any data exists.
/// - `syn_record_vector_unit_fields` (#1964) refuses a *record vector* whose
///   fields are on incomparable scales.
///
/// Neither can see a graded encoder fed an input this corpus never populates.
/// `syn.agent_event.usage_total_log1p.v1` is the worked example: `syn_scalar_log1p`
/// is graded by construction, but all four `GenAI` token fields it sums are absent
/// on every one of the 8,241 rows in `CF_AGENT_EVENTS` — that CF is Synapse's
/// agent *lifecycle* log, not model-completion telemetry — so the lane emits one
/// value forever and contributes no bits about any anchor.
///
/// Measured off the hydrated vectors the coverage pass already holds, so naming
/// every such lane on the vault costs no additional read. That is the point: the
/// alternative is finding them one at a time, whenever someone thinks to look.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxDegenerateLane {
    pub panel_version: u32,
    pub slot: u16,
    /// Measured records on which this lane was present at all. Zero for an
    /// `ABSENT_BY_CORPUS` lane, which was never measured on any record.
    pub records_present: usize,
    /// Distinct vector values observed for `CONSTANT_BY_CORPUS` (degenerate
    /// exactly when 1); the densified support width for
    /// `SINGLE_SUPPORT_BY_CORPUS`; zero for `ABSENT_BY_CORPUS`.
    pub distinct_values: usize,
    /// Rows in the panel population, including rows not hydrated by this pass.
    pub population_records: usize,
    /// True only when every population row was hydrated. A sampled finding must
    /// never be presented as a whole-corpus fact.
    pub census_complete: bool,
    /// Most-common value count divided by second-most-common value count. None
    /// when fewer than two values were observed.
    pub frequency_ratio: Option<f64>,
    /// `100 * distinct_values / records_present`.
    pub percent_unique: f64,
    /// False until a grounded, stratified assay proves that no rare critical
    /// outcome depends on this lane. Distribution shape alone never authorizes
    /// parking a lens (#1983).
    pub lifecycle_action_allowed: bool,
    pub stratified_override_status: String,
    /// The single value every record projected to, when the lane is a scalar
    /// and the cause is `CONSTANT_BY_CORPUS`.
    pub constant_scalar: Option<f32>,
    /// `CALYX_LENS_CONSTANT_BY_CORPUS` | `CALYX_LENS_ABSENT_BY_CORPUS` |
    /// `CALYX_LENS_SINGLE_SUPPORT_BY_CORPUS`.
    pub code: String,
    pub detail: String,
    pub remediation: String,
}

/// Lens coverage across the panels a vault carries (issue #1894, ask 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxLensCoverageStatus {
    pub panels: Vec<SynapseCalyxPanelLensCoverage>,
    pub max_records_per_panel: usize,
    /// Panels carrying fewer than two lenses, or whose blind-spot fraction is
    /// above [`SYNAPSE_LENS_BLIND_SPOT_CEILING`].
    pub deficient_panels: Vec<u32>,
    /// Every lane on every measured panel that cannot rank over its corpus,
    /// flattened for a one-glance read (issue #1970). Carries all three causes
    /// — `CALYX_LENS_CONSTANT_BY_CORPUS`, `CALYX_LENS_ABSENT_BY_CORPUS` and
    /// `CALYX_LENS_SINGLE_SUPPORT_BY_CORPUS` — in one list, because an operator
    /// asking "which lenses are dead" should not have to know which of three
    /// ways they died to find them.
    pub degenerate_lanes: Vec<SynapseCalyxDegenerateLane>,
    pub blind_spot_ceiling: f32,
    pub measured_at_unix_ms: Option<u64>,
}

/// One pairwise redundancy measurement between two lenses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRedundancyPair {
    pub slot_a: u16,
    pub slot_b: u16,
    pub nmi: f32,
    pub mi_bits: f32,
    pub n_samples: usize,
    pub redundant: bool,
}

/// One lens pair that could not be measured, named with the reason.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRedundancySkip {
    pub slot_a: u16,
    pub slot_b: u16,
    pub lens_a: String,
    pub lens_b: String,
    /// Stable reason code: `constant_column` or `insufficient_paired_samples`.
    pub reason: String,
    /// Which of the two slots caused it, when the reason is slot-specific.
    pub offending_slot: Option<u16>,
    pub detail: String,
    pub n_paired: usize,
}

/// A lens carrying no information at all over the measured corpus.
///
/// A column with zero entropy cannot correlate with anything, which is why it
/// makes every pair it participates in unmeasurable — but it is a finding in its
/// own right, not merely the reason another report failed. Calyx's catalog
/// already has the code for it (`CALYX_ASSAY_LOW_SIGNAL`), and the honest
/// remediation is to park or retire the lens (issue #1897).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxLowSignalLens {
    pub slot: u16,
    pub lens: String,
    pub code: String,
    /// The single value every record projected to.
    pub constant_value: f32,
    pub records_observed: usize,
    pub distinct_values: usize,
    pub remediation: String,
}

/// Result of one Assay redundancy / effective-rank pass with Assay readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRedundancyReport {
    pub panel_version: u32,
    pub n_lenses: usize,
    pub records_scanned: usize,
    pub effective_rank: f32,
    /// `C(n_lenses, 2)` — every pair the panel could in principle offer.
    pub pairs_possible: usize,
    pub pairs_evaluated: usize,
    pub pairs_skipped: usize,
    /// Every skipped pair, named with its offending slot and reason (#1897).
    pub skipped_details: Vec<SynapseCalyxRedundancySkip>,
    /// The lenses the reported `effective_rank` is actually computed over, so
    /// the number is never silently taken over a smaller set than the caller
    /// believes it covers.
    pub effective_rank_slots: Vec<u16>,
    pub effective_rank_lenses: Vec<String>,
    /// Zero-entropy lenses found while measuring, recommended for parking.
    pub low_signal_lenses: Vec<SynapseCalyxLowSignalLens>,
    /// Control-doctrine marker (#1670): domain anchor coverage below the floor.
    pub domain_provisional: bool,
    /// Fraction of the domain's measured records carrying a grounded anchor.
    pub domain_grounded_fraction: f32,
    pub redundant_pairs: Vec<SynapseCalyxRedundancyPair>,
    pub assay_cf_rows_after: usize,
}

/// Rows a lane must be present on before "constant" is a claim about the corpus
/// rather than about the sample size (issue #1970).
///
/// A lane observed on one or two records is trivially constant and says nothing.
/// Two is the smallest count at which "every record agreed" is even capable of
/// being false, so the floor is set above it: below this the lane is simply not
/// reported, never reported as fine.
const SYNAPSE_DEGENERATE_LANE_MIN_RECORDS: usize = 8;
const SYNAPSE_NEAR_ZERO_VARIANCE_FREQUENCY_RATIO: f64 = 19.0;
const SYNAPSE_NEAR_ZERO_VARIANCE_PERCENT_UNIQUE: f64 = 10.0;

/// Names every dense lane that took one value across the whole measured corpus
/// (issue #1970).
///
/// Reads the vectors the coverage pass already hydrated, so this costs no
/// additional I/O — which is exactly why it can run over *every* lane on every
/// maintainer tick instead of over the lanes someone thought to check.
///
/// Values are compared on their exact `f32` bit patterns rather than with a
/// tolerance. A lane whose values differ only in the last mantissa bit is not
/// constant, and calling it constant would be this instrument inventing a
/// finding. `-0.0` is normalized to `+0.0` so the two do not read as distinct
/// directions, and any NaN is treated as one value: a lane that is entirely NaN
/// cannot rank either, and it should be reported rather than hidden by NaN's
/// inequality with itself.
fn degenerate_lanes(panel_version: u32, corpus: &DenseCorpus) -> Vec<SynapseCalyxDegenerateLane> {
    let mut lanes = constant_by_corpus_lanes(panel_version, corpus);
    let already: BTreeSet<u16> = lanes.iter().map(|lane| lane.slot).collect();
    lanes.extend(absent_by_corpus_lanes(panel_version, corpus));
    lanes.extend(single_support_lanes(panel_version, corpus, &already));
    lanes.sort_by_key(|lane| lane.slot);
    lanes
}

#[expect(
    clippy::cast_precision_loss,
    reason = "doctor corpus counts are bounded by SYNAPSE_INTELLIGENCE_MAX_RECORDS, far below f64's exact integer range"
)]
fn distribution_evidence(
    counts: &BTreeMap<Vec<u32>, usize>,
    records_present: usize,
) -> (usize, Option<f64>, f64) {
    let distinct = counts.len();
    let mut frequencies: Vec<usize> = counts.values().copied().collect();
    frequencies.sort_unstable_by(|left, right| right.cmp(left));
    let frequency_ratio = frequencies
        .get(1)
        .filter(|second| **second > 0)
        .map(|second| frequencies[0] as f64 / *second as f64);
    let percent_unique = if records_present == 0 {
        0.0
    } else {
        100.0 * distinct as f64 / records_present as f64
    };
    (distinct, frequency_ratio, percent_unique)
}

/// A lane the panel declares but which is `Absent` on **every** measured record.
///
/// This is the second way a lane cannot rank, and the one that #1970 was filed
/// about — yet the constant-by-corpus detector added for #1970 structurally
/// cannot see it. `optional_log1p_slot` and its siblings emit
/// `absent(AbsentReason::NotApplicable)` when their input field is `None`, so a
/// lens whose input the corpus never populates is never *measured*, never lands
/// in `record.slots`, and therefore can never reach the constant detector's
/// `records_present >= SYNAPSE_DEGENERATE_LANE_MIN_RECORDS` floor. It fails the
/// FIRST clause, not the second.
///
/// Absent is deliberately not folded into the constant detector by treating it
/// as a zero vector: `Absent` is an explicit absence and reading it as a zero
/// direction is the #1894 defect. It is a distinct finding with a distinct
/// remediation — no re-encoding can rescue a field that is not there, so the
/// lens must be parked or rebuilt around what the corpus actually carries.
///
/// `panel_slots` already holds exactly this fact: the loader only leaves a slot
/// marked `Absent` when no record in the scan carried a real vector for it.
fn absent_by_corpus_lanes(
    panel_version: u32,
    corpus: &DenseCorpus,
) -> Vec<SynapseCalyxDegenerateLane> {
    let records_measured = corpus.records.len();
    let census_complete = records_measured == corpus.records_scanned;
    // The same floor the constant detector uses. A lane absent across three
    // records is not evidence about the lens; across thousands it is.
    if records_measured < SYNAPSE_DEGENERATE_LANE_MIN_RECORDS {
        return Vec::new();
    }
    corpus
        .panel_slots
        .iter()
        .filter(|(_, kind)| **kind == SynapseCalyxSlotKind::Absent)
        .map(|(slot, _)| SynapseCalyxDegenerateLane {
            panel_version,
            slot: slot.get(),
            records_present: 0,
            distinct_values: 0,
            population_records: corpus.records_scanned,
            census_complete,
            frequency_ratio: None,
            percent_unique: 0.0,
            lifecycle_action_allowed: false,
            stratified_override_status: "GROUNDED_STRATIFIED_ASSAY_REQUIRED".to_owned(),
            constant_scalar: None,
            code: if census_complete {
                "CALYX_LENS_ABSENT_BY_CORPUS"
            } else {
                "CALYX_LENS_ABSENT_BY_SAMPLE"
            }
            .to_owned(),
            detail: format!(
                "slot {} is declared by panel {panel_version} but is explicitly Absent on all \
                 {records_measured} measured record(s) from a population of {}. census_complete={census_complete}. It \
                 contributes no bits about any anchor and cannot rank. This is NOT the same as \
                 being constant: there is no value here to re-encode, so no change to the \
                 encoder can rescue it. Absent is an explicit absence and is never read as a \
                 zero vector (#1894)",
                slot.get(), corpus.records_scanned
            ),
            remediation: if census_complete {
                "measure the lens input over its source CF and run a grounded stratified assay before any lifecycle change; rebuild it around a populated field if the absence is confirmed"
            } else {
                "run a full census before changing lens lifecycle; zero observations in a bounded sample does not prove population absence"
            }.to_owned(),
        })
        .collect()
}

/// A sparse lane whose corpus-observed support is a single index.
///
/// This one is provable rather than heuristic. Densification maps the lane onto
/// its observed support, so a support of 1 yields a **one-dimensional** vector
/// per record, and the cosine between any two same-sign one-dimensional vectors
/// is `ab / (|a| * |b|) = 1` exactly, whatever `a` and `b` are. So the lane's
/// nearest-neighbour ordering is degenerate even when its *values* vary — which
/// is precisely why the constant-by-corpus detector misses it: the values are
/// not constant, only the direction is.
///
/// Reported only when it is **not** already reported as constant. A width-1
/// lane whose values are all equal is both things at once, and constant is the
/// stricter, more directly actionable statement ("the field does not vary"), so
/// it wins. `syn.agent_event.provider_hash.v1` is that case on the live vault:
/// width 1 AND one distinct value over the 231 records carrying it. This cause
/// therefore fires only for the genuinely distinct case — a collapsed lane whose
/// values still vary, which no other detector can see.
///
/// Carries the same `SYNAPSE_DEGENERATE_LANE_MIN_RECORDS` floor as the constant
/// detector, and for the same reason. Without it this reported
/// `syn-observation-v1` slot 67 as dead from a corpus of **one** record — where
/// a support of 1 is arithmetically unavoidable and says nothing whatsoever
/// about the lens. A degeneracy claim needs a corpus to be a claim about.
#[expect(
    clippy::cast_precision_loss,
    reason = "doctor corpus widths and record counts are bounded by SYNAPSE_INTELLIGENCE_MAX_RECORDS, far below f64's exact integer range"
)]
fn single_support_lanes(
    panel_version: u32,
    corpus: &DenseCorpus,
    already_reported: &BTreeSet<u16>,
) -> Vec<SynapseCalyxDegenerateLane> {
    corpus
        .densified_sparse_slots
        .iter()
        .filter(|(slot, width)| **width <= 1 && !already_reported.contains(&slot.get()))
        .filter_map(|(slot, width)| {
            let records_present = corpus
                .records
                .iter()
                .filter(|record| record.slots.contains_key(slot))
                .count();
            if records_present < SYNAPSE_DEGENERATE_LANE_MIN_RECORDS {
                return None;
            }
            Some(SynapseCalyxDegenerateLane {
                panel_version,
                slot: slot.get(),
                records_present,
                distinct_values: *width,
                population_records: corpus.records_scanned,
                census_complete: corpus.records.len() == corpus.records_scanned,
                frequency_ratio: None,
                percent_unique: 100.0 * *width as f64 / records_present as f64,
                lifecycle_action_allowed: false,
                stratified_override_status: "GROUNDED_STRATIFIED_ASSAY_REQUIRED".to_owned(),
                constant_scalar: None,
                code: if corpus.records.len() == corpus.records_scanned {
                    "CALYX_LENS_SINGLE_SUPPORT_BY_CORPUS"
                } else {
                    "CALYX_LENS_SINGLE_SUPPORT_BY_SAMPLE"
                }.to_owned(),
                detail: format!(
                    "sparse slot {} occupies exactly {width} distinct index(es) across the \
                     {records_present} measured record(s) that carry it, so it densifies to a \
                     {width}-dimensional vector. The cosine between any two same-sign \
                     one-dimensional vectors is exactly 1.0 regardless of their magnitudes, so \
                     this lane cannot order neighbours even though its values are not constant \
                     — which is why the constant-by-corpus check does not fire on it",
                    slot.get()
                ),
                remediation: if corpus.records.len() == corpus.records_scanned {
                    "every record fell into one bucket; inspect source population and hashing, then run a grounded stratified assay before any lifecycle change"
                } else {
                    "run a full census before changing lens lifecycle; one observed support bucket in a bounded sample does not prove population collapse"
                }.to_owned(),
            })
        })
        .collect()
}

fn constant_by_corpus_lanes(
    panel_version: u32,
    corpus: &DenseCorpus,
) -> Vec<SynapseCalyxDegenerateLane> {
    let mut per_slot: BTreeMap<SlotId, (usize, BTreeMap<Vec<u32>, usize>)> = BTreeMap::new();
    for record in &corpus.records {
        for (slot, vector) in &record.slots {
            let key: Vec<u32> = vector
                .iter()
                .map(|value| {
                    if value.is_nan() {
                        f32::NAN.to_bits()
                    } else if *value == 0.0 {
                        0.0_f32.to_bits()
                    } else {
                        value.to_bits()
                    }
                })
                .collect();
            let entry = per_slot
                .entry(*slot)
                .or_insert_with(|| (0, BTreeMap::new()));
            entry.0 += 1;
            *entry.1.entry(key).or_insert(0) += 1;
        }
    }
    per_slot
        .into_iter()
        .filter(|(_, (records_present, counts))| {
            if *records_present < SYNAPSE_DEGENERATE_LANE_MIN_RECORDS {
                return false;
            }
            let (_, frequency_ratio, percent_unique) =
                distribution_evidence(counts, *records_present);
            counts.len() == 1
                || (frequency_ratio.is_some_and(|ratio| {
                    ratio > SYNAPSE_NEAR_ZERO_VARIANCE_FREQUENCY_RATIO
                }) && percent_unique < SYNAPSE_NEAR_ZERO_VARIANCE_PERCENT_UNIQUE)
        })
        .map(|(slot, (records_present, counts))| {
            let (distinct_values, frequency_ratio, percent_unique) =
                distribution_evidence(&counts, records_present);
            let census_complete = corpus.records.len() == corpus.records_scanned;
            let exact_constant = distinct_values == 1;
            let constant_scalar = exact_constant.then(|| counts.keys().next()).flatten()
                .filter(|bits| bits.len() == 1)
                .map(|bits| f32::from_bits(bits[0]));
            SynapseCalyxDegenerateLane {
                panel_version,
                slot: slot.get(),
                records_present,
                distinct_values,
                population_records: corpus.records_scanned,
                census_complete,
                frequency_ratio,
                percent_unique,
                lifecycle_action_allowed: false,
                stratified_override_status: "GROUNDED_STRATIFIED_ASSAY_REQUIRED".to_owned(),
                constant_scalar,
                code: if exact_constant && census_complete {
                    "CALYX_LENS_CONSTANT_BY_CORPUS"
                } else if exact_constant {
                    "CALYX_LENS_CONSTANT_BY_SAMPLE"
                } else if census_complete {
                    "CALYX_LENS_NEAR_ZERO_VARIANCE_BY_CORPUS"
                } else {
                    "CALYX_LENS_NEAR_ZERO_VARIANCE_BY_SAMPLE"
                }.to_owned(),
                detail: format!(
                    "slot {} distribution evidence over {records_present}/{} record(s) of panel \
                     {panel_version}: distinct_values={distinct_values} frequency_ratio={frequency_ratio:?} \
                     percent_unique={percent_unique:.6} census_complete={census_complete} \
                     exact_constant={exact_constant}",
                    slot.get(), corpus.records_scanned
                ),
                remediation: if census_complete {
                    "measure the lens input over its source CF and run a grounded stratified assay before any lifecycle change; rebuild around what varies when collision or collapse is confirmed"
                } else {
                    "run a full census before changing lens lifecycle; an exact-constant bounded sample does not prove a constant population"
                }.to_owned(),
            }
        })
        .collect()
}

struct AnchoredSlotSamples {
    x: Vec<Vec<f32>>,
    labels: Vec<usize>,
}

impl SynapseCalyxVault {
    /// Measures grounded bits per lens about one outcome anchor over the panel
    /// corpus (KSG mutual information), persists each lens estimate to the native
    /// Assay CF, and reads the CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the Base CF cannot be
    /// scanned, a constellation fails to decode, the KSG estimator rejects the
    /// samples, or the Assay CF write/readback fails.
    #[expect(
        clippy::too_many_lines,
        reason = "one physical corpus pass must retain estimator refusals, carrier annotations, persistence, and readback evidence together"
    )]
    pub fn assay_bits(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> Result<SynapseCalyxBitsReport, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;
        let anchor_kind = parse_anchor_kind(&params.anchor_kind);
        let gathered = gather_anchored_slot_samples(&corpus, &anchor_kind, &params.excluded_slots);

        // #1959 ask 1: `bits` ANNOTATES where `sufficiency` refuses. A per-lens
        // report is the legitimate way to inspect a carrier — refusing it would
        // remove the very tool you use to understand one — but an unmarked
        // carrier row is how a circular number gets read as a capability claim.
        // Structural, so it is a pure function of (anchor, panel, excluded) and
        // returns the same verdict on an empty corpus as on a full one.
        let source_provenance = lens_provenance::syn_anchor_source_provenance(
            &params.anchor_kind,
            params.panel_version,
            &params.excluded_slots,
        );
        let carrier_slots: BTreeSet<u16> = source_provenance
            .carriers
            .iter()
            .map(|carrier| carrier.slot)
            .collect();

        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let ksg_k = params.ksg_k.max(1);
        let mut slots = Vec::new();
        let mut slot_bits: Vec<(SlotId, f32)> = Vec::new();
        for (slot, samples) in &gathered.by_slot {
            if samples.x.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                slots.push(unmeasured_slot_bits(
                    slot.get(),
                    samples.x.len(),
                    SynapseCalyxSlotBitsState::InsufficientSamples,
                    format!(
                        "{} paired sample(s); the KSG estimator requires {SYNAPSE_ASSAY_MIN_SAMPLES}",
                        samples.x.len()
                    ),
                ));
                continue;
            }
            let k = ksg_k.min(samples.x.len().saturating_sub(1)).max(1);
            // #1672: choose the estimator from the column, not from a default.
            // A panel deliberately mixes explicit encoders (one-hot, hash,
            // cyclic) with continuous ones, and KSG's k-th neighbour radius is
            // zero by construction on the explicit ones — measured on the live
            // vault, 7 of the 8 dense lenses on `syn-mcp-usage-v1` were refused
            // as degenerate and the whole panel reported the single continuous
            // lens's bits as its total. Encoding a value explicitly is supposed
            // to make it *more* measurable, not less; the missing piece was the
            // discrete instrument to measure it with.
            let outcome = match mi_about_labels(
                MiEstimatorChoice::Auto,
                &samples.x,
                &samples.labels,
                k,
                None,
            ) {
                Ok(outcome) => outcome,
                // The column could not even be validated, so there is no
                // instrument to name — report the refusal without a pick.
                Err(error) => {
                    let state = estimator_refusal_state(&error);
                    slots.push(unmeasured_slot_bits(
                        slot.get(),
                        samples.x.len(),
                        state,
                        estimator_refusal_reason(state, &error),
                    ));
                    continue;
                }
            };
            let pick = outcome.pick;
            // An estimator refusal is a fact about ONE lens, so it must not end
            // the whole pass (#1915). `assay_redundancy` already degrades
            // per-pair with a named reason and returns everything else; before
            // this, a single categorical lens `?`-ed out of the entire report
            // and no other slot — including the well-conditioned continuous
            // ones — was ever reported.
            //
            // #1915's fix caught exactly one code and left `Err(_) => return`
            // standing. The live vault then aborted the whole report again with
            // `CALYX_ASSAY_INSUFFICIENT_SAMPLES` (an anchor whose minority class
            // held 4 records against k=4), through the very arm that fix was
            // meant to remove. Classification is total now: no estimator error
            // reaches a `return`.
            let estimate = match outcome.estimate {
                Ok(estimate) => estimate,
                Err(error) => {
                    let state = estimator_refusal_state(&error);
                    slots.push(unmeasured_slot_bits_with_pick(
                        slot.get(),
                        samples.x.len(),
                        state,
                        estimator_refusal_reason(state, &error),
                        &pick,
                    ));
                    continue;
                }
            };
            slot_bits.push((*slot, estimate.bits));
            store.put(
                AssayCacheKey::scoped(
                    params.panel_version,
                    &params.corpus_shard,
                    vault_id,
                    anchor_kind.clone(),
                ),
                AssaySubject::Lens { slot: *slot },
                estimate.clone(),
                "synapse-assay-bits",
                seq,
            );
            let mut measured = SynapseCalyxSlotBits {
                slot: slot.get(),
                marginal_bits: estimate.bits,
                ci_low: estimate.ci_low,
                ci_high: estimate.ci_high,
                n_samples: estimate.n_samples,
                sole_carrier: false,
                provisional: false,
                state: SynapseCalyxSlotBitsState::Measured,
                unmeasured_reason: None,
                estimator: None,
                estimator_selection: None,
                estimator_reason: None,
                distinct_values: None,
                max_same_label_multiplicity: None,
                anchor_source_carrier: false,
                anchor_source_shared_fields: Vec::new(),
            };
            apply_estimator_pick(&mut measured, &pick);
            slots.push(measured);
        }

        let attributions = per_sensor_attribution(&slot_bits, SYNAPSE_ASSAY_BIT_FLOOR);
        mark_sole_carriers(&mut slots, &attributions);
        for slot in &mut slots {
            let Some(carrier) = source_provenance
                .carriers
                .iter()
                .find(|carrier| carrier.slot == slot.slot)
            else {
                continue;
            };
            slot.anchor_source_carrier = true;
            slot.anchor_source_shared_fields = carrier
                .shared_fields
                .iter()
                .map(|field| (*field).to_owned())
                .collect();
        }
        let grounded = gathered.representative.as_ref().is_some_and(|anchor| {
            matches!(
                bits_report_with_anchor(attributions.clone(), anchor).trust,
                TrustTag::Trusted
            )
        });
        // Summing an empty set of estimates yields `-0.0` on this path, which
        // renders as a negative zero and reads as a measurement rather than the
        // absence of one (#1897). Normalize the sign; the magnitude is untouched.
        let total_bits: f32 = slot_bits.iter().map(|(_, bits)| *bits).sum();
        let total_bits = if total_bits == 0.0 { 0.0 } else { total_bits };
        // The same sum with every declared carrier dropped (#1959). When a
        // carrier is present these two differ, and only this one is a claim
        // about predicting the outcome rather than about reading it back.
        let total_bits_carrier_free: f32 = slot_bits
            .iter()
            .filter(|(slot, _)| !carrier_slots.contains(&slot.get()))
            .map(|(_, bits)| *bits)
            .sum();
        let total_bits_carrier_free = if total_bits_carrier_free == 0.0 {
            0.0
        } else {
            total_bits_carrier_free
        };
        // #1670: a result over an under-anchored domain may only advise. This is
        // a different property from `grounded` above, which is the assay's own
        // sample-count trust tag.
        let verdict = self.domain_grounding_verdict(params.panel_version, max_records)?;

        // A zeroed report over zero anchored records is a different statement
        // from a measured zero, and the caller should not have to infer which
        // one it is holding by cross-reading three other fields (#1897).
        let unmeasurable_reason = if gathered.anchored_records == 0 {
            Some(format!(
                "no record in the {} scanned panel row(s) carries an anchor of kind '{}', so there is nothing to measure bits about; the panel's domain grounded fraction is {:.4}",
                corpus.records_scanned, params.anchor_kind, verdict.grounded_fraction
            ))
        } else if gathered.distinct_outcomes < 2 {
            Some(format!(
                "the {} anchored record(s) carry {} distinct outcome(s); mutual information about an outcome requires at least 2",
                gathered.anchored_records, gathered.distinct_outcomes
            ))
        } else {
            None
        };

        let assay_cf_rows_after = self.persist_assay_store(&store)?;
        Ok(SynapseCalyxBitsReport {
            panel_version: params.panel_version,
            anchor_kind: params.anchor_kind.clone(),
            anchored_records: gathered.anchored_records,
            distinct_outcomes: gathered.distinct_outcomes,
            total_bits,
            total_bits_carrier_free,
            anchor_source_declared: source_provenance.anchor_declared,
            anchor_source_carriers: anchor_source_carriers(&source_provenance),
            measurable: unmeasurable_reason.is_none(),
            unmeasurable_reason,
            grounded,
            domain_provisional: verdict.provisional,
            domain_grounded_fraction: verdict.grounded_fraction,
            slots,
            assay_cf_rows_after,
        })
    }

    /// Tests panel sufficiency `I(panel;anchor) >= H(anchor)` over the joint
    /// slot representation, routes each deficit to a logged propose-lens
    /// suggestion, persists the panel/outcome-entropy Assay rows, and reads the
    /// Assay CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the corpus cannot be read,
    /// the KSG estimator rejects the joint samples, or the Assay write/readback
    /// fails.
    #[allow(clippy::too_many_lines)]
    pub fn assay_sufficiency(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> Result<SynapseCalyxSufficiencyReport, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;
        let anchor_kind = parse_anchor_kind(&params.anchor_kind);
        let gathered = gather_anchored_slot_samples(&corpus, &anchor_kind, &params.excluded_slots);

        // #1958: refuse a circular measurement STRUCTURALLY, before spending a
        // single estimator call on it. `detect_anchor_leakage` below is a
        // statistical check and can only reach the degenerate case where a lens
        // IS the label; a lens that merely *contains* it passes cleanly. This
        // one is a set intersection between what each lens reads and what
        // determines the anchor, so it is exact, deterministic, and independent
        // of sample size.
        //
        // It is asked about the PANEL's slots, not the slots this corpus
        // happened to yield samples for. Deriving it from the gathered samples
        // made the verdict a function of traffic: `syn-outcome-v1` came back
        // clean on three anchors purely because those slots had no anchored rows
        // yet, and would have started refusing once rows arrived. A leakage
        // check that switches on with traffic is not a check.
        let source_provenance = lens_provenance::syn_anchor_source_provenance(
            &params.anchor_kind,
            params.panel_version,
            &params.excluded_slots,
        );
        if !source_provenance.carriers.is_empty() {
            return Err(anchor_source_leakage_error(params, &source_provenance));
        }

        // Per-slot attributions feed the deficit split; the joint panel bits are
        // the sufficiency numerator.
        let ksg_k = params.ksg_k.max(1);
        let mut slot_bits: Vec<(SlotId, f32)> = Vec::new();
        // (slot, bits, distinct_values) for the #1953 leakage check.
        let mut slot_shapes: Vec<(SlotId, f32, usize)> = Vec::new();
        // Slots the estimator could not measure are EXCLUDED, not pushed as
        // zero (#1915). A placeholder zero here became a concrete
        // `deficit_bits` and a `ProposeLens` recommendation against a lens
        // nobody had measured — advice manufactured out of an absence.
        let mut unmeasured_slots = 0usize;
        for (slot, samples) in &gathered.by_slot {
            if samples.x.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                unmeasured_slots += 1;
                continue;
            }
            let k = ksg_k.min(samples.x.len().saturating_sub(1)).max(1);
            // Same instrument choice as `assay_bits` (#1672): a categorical
            // lens excluded here is excluded from the joint and from the
            // deficit split, so measuring it with the wrong estimator did not
            // merely lose one number — it removed the lens from the sufficiency
            // verdict entirely and then blamed the deficit on the survivors.
            match mi_about_labels(
                MiEstimatorChoice::Auto,
                &samples.x,
                &samples.labels,
                k,
                None,
            ) {
                Ok(outcome) => match outcome.estimate {
                    Ok(estimate) => {
                        slot_bits.push((*slot, estimate.bits));
                        // Kept alongside the bits for the leakage check below
                        // (#1953): a lens that IS the anchor is identified by
                        // its cardinality as well as its score.
                        slot_shapes.push((*slot, estimate.bits, outcome.pick.distinct_values));
                    }
                    // Any estimator refusal excludes the slot from the joint and
                    // from the deficit split; none of them ends the pass (#1915).
                    Err(_) => unmeasured_slots += 1,
                },
                Err(_) => {
                    unmeasured_slots += 1;
                }
            }
        }
        let attributions = per_sensor_attribution(&slot_bits, SYNAPSE_ASSAY_BIT_FLOOR);

        let usable_slots: BTreeSet<SlotId> = slot_bits.iter().map(|(slot, _)| *slot).collect();
        let joint = build_joint_samples(&corpus, &anchor_kind, &usable_slots);
        let anchor_entropy_bits = entropy_bits(&joint.labels);
        let joint_records = joint.labels.len();
        // #1953: a lens whose marginal bits sit exactly at H(anchor), with the
        // anchor's own cardinality, is the anchor re-encoded rather than
        // evidence about it. On syn-mcp-usage-v1 that was slot 86
        // (`status_onehot`), built from the same `record.status` field the
        // anchor is built from, and it made `sufficient=true, deficit_bits=0`
        // circular on the one corpus chosen for being well grounded.
        let anchor_leakage: Vec<AnchorLeakage> = slot_shapes
            .iter()
            .filter_map(|(slot, bits, distinct_values)| {
                detect_anchor_leakage(
                    slot.get(),
                    *bits,
                    *distinct_values,
                    anchor_entropy_bits,
                    gathered.distinct_outcomes,
                )
            })
            .collect();
        // `panel_measured` is the load-bearing distinction (#1915): below the
        // floor there is no joint estimate, and `panel_bits = 0.0` is a
        // placeholder. Subtracting a placeholder from a genuinely computed
        // anchor entropy produced a real-looking `deficit_bits` and
        // `sufficient=false` on the live vault, where nothing had been measured
        // at all.
        let best_slot_bits = slot_bits
            .iter()
            .map(|(_, bits)| *bits)
            .fold(0.0_f32, f32::max);
        let (panel_bits, panel_measured) = if joint_records >= SYNAPSE_ASSAY_MIN_SAMPLES {
            let k = ksg_k.min(joint_records.saturating_sub(1)).max(1);
            match mi_about_labels(MiEstimatorChoice::Auto, &joint.x, &joint.labels, k, None)
                .ok()
                .and_then(|outcome| outcome.estimate.ok())
            {
                Some(estimate) => (estimate.bits, true),
                // An unestimable joint is reported as unmeasured, exactly like
                // an unestimable slot. Returning here would have made the whole
                // sufficiency verdict unavailable because one panel-shaped
                // corpus defeated the estimator (#1915).
                None => (0.0, false),
            }
        } else {
            (0.0, false)
        };
        // Monotonicity is a law, not a preference: conditioning cannot destroy
        // information, so `I(panel;A) >= I(slot_i;A)` for every measured lens.
        // A joint estimate below the best marginal one is a KSG
        // dimensionality artefact, and passing it through produced a
        // `sufficient=false` verdict with a concrete `deficit_bits` on a panel
        // whose own lens already carried 99% of the available bits (#1916).
        //
        // The floor is applied rather than the result discarded: the marginal
        // estimate IS a valid lower bound on the joint, so raising to it is the
        // tightest defensible answer, not a fudge.
        let (panel_bits, panel_floor_applied) = if panel_measured && panel_bits < best_slot_bits {
            (best_slot_bits, true)
        } else {
            (panel_bits, false)
        };

        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let cache_key = AssayCacheKey::scoped(
            params.panel_version,
            &params.corpus_shard,
            vault_id,
            anchor_kind,
        );
        let trust =
            if joint_records >= SYNAPSE_ASSAY_MIN_SAMPLES && gathered.representative.is_some() {
                TrustTag::Trusted
            } else {
                TrustTag::Provisional
            };
        // A placeholder must not enter the source of truth (#1915). When the
        // joint estimator never ran, `panel_bits` is `0.0` as a stand-in, and
        // persisting that as an `AssaySubject::Panel` row makes the Assay CF —
        // the thing every downstream reader treats as authoritative — assert a
        // measured zero. Worse, `trust` above is derived from the sample count
        // alone, so a corpus over the floor whose joint the estimator *refused*
        // wrote the placeholder tagged `Trusted`: the exact "0.0 that was never
        // measured, indistinguishable from a measured 0.0" this issue is about,
        // reached at the persistence layer instead of the report layer.
        //
        // `assay_bits` already handles this correctly by never storing a row
        // for an unmeasured slot. The panel row now follows the same rule: no
        // measurement, no row. Absence of the row is the honest encoding of
        // absence of the measurement, and `panel_measured` in the report says
        // so explicitly to anyone reading the report rather than the CF.
        if panel_measured {
            store.put(
                cache_key.clone(),
                AssaySubject::Panel,
                MiEstimate::point(
                    panel_bits,
                    joint_records,
                    EstimatorKind::PanelSufficiency,
                    trust,
                ),
                "synapse-assay-sufficiency",
                seq,
            );
        }
        // The outcome entropy is genuinely computed from the anchor labels and
        // is persisted on its own terms — but `entropy_bits` over an empty
        // label set is also `0.0`, and that zero is no more a measurement than
        // the panel one. Zero joint records means zero labels means nothing to
        // take the entropy of.
        if joint_records > 0 {
            store.put(
                cache_key,
                AssaySubject::OutcomeEntropy,
                MiEstimate::point(
                    anchor_entropy_bits,
                    joint_records,
                    EstimatorKind::OutcomeEntropy,
                    trust,
                ),
                "synapse-assay-sufficiency",
                seq,
            );
        }
        let assay_cf_rows_after = self.persist_assay_store(&store)?;
        // #1670 control-doctrine marker; independent of the `trust` tag above.
        let verdict = self.domain_grounding_verdict(params.panel_version, max_records)?;

        if let Some(anchor) = gathered.representative.as_ref() {
            let sufficiency = panel_sufficiency_with_anchor(
                panel_bits,
                anchor_entropy_bits,
                &attributions,
                anchor,
            );
            for deficit in &sufficiency.deficits {
                // Route each deficit to a logged, human-actioned propose-lens
                // suggestion (auto-commissioning is out of scope, #1672).
                tracing::warn!(
                    code = "SYNAPSE_ASSAY_PROPOSE_LENS",
                    panel_version = params.panel_version,
                    anchor_kind = %params.anchor_kind,
                    slot = deficit.slot.map(SlotId::get),
                    deficit_bits = deficit.deficit_bits,
                    suggested_action = ?deficit.suggested_action,
                    "panel sufficiency deficit — propose a lens to close the gap"
                );
            }
            let deficits = sufficiency
                .deficits
                .iter()
                .map(|deficit| SynapseCalyxSufficiencyDeficit {
                    slot: deficit.slot.map(SlotId::get),
                    deficit_bits: deficit.deficit_bits,
                    suggested_action: format!("{:?}", deficit.suggested_action),
                    reason: deficit.reason.clone(),
                })
                .collect::<Vec<_>>();
            return Ok(SynapseCalyxSufficiencyReport {
                panel_version: params.panel_version,
                anchor_kind: params.anchor_kind.clone(),
                anchored_records: gathered.anchored_records,
                joint_records,
                panel_bits,
                panel_measured,
                panel_floor_applied,
                unmeasured_slots,
                measured_slots: usable_slots.iter().copied().collect(),
                anchor_entropy_bits,
                sufficient: panel_measured && sufficiency.sufficient && anchor_leakage.is_empty(),
                deficit_bits: if panel_measured {
                    sufficiency.deficit_bits
                } else {
                    0.0
                },
                anchor_leakage,
                anchor_source_declared: source_provenance.anchor_declared,
                grounded: matches!(trust, TrustTag::Trusted),
                domain_provisional: verdict.provisional,
                domain_grounded_fraction: verdict.grounded_fraction,
                deficits: if panel_measured { deficits } else { Vec::new() },
                assay_cf_rows_after,
            });
        }
        Ok(SynapseCalyxSufficiencyReport {
            panel_version: params.panel_version,
            anchor_kind: params.anchor_kind.clone(),
            anchored_records: gathered.anchored_records,
            joint_records,
            panel_bits,
            panel_measured,
            panel_floor_applied,
            unmeasured_slots,
            measured_slots: usable_slots.iter().copied().collect(),
            anchor_entropy_bits,
            sufficient: panel_measured
                && panel_bits >= anchor_entropy_bits
                && anchor_leakage.is_empty(),
            deficit_bits: if panel_measured {
                (anchor_entropy_bits - panel_bits).max(0.0)
            } else {
                0.0
            },
            anchor_leakage,
            anchor_source_declared: source_provenance.anchor_declared,
            grounded: false,
            domain_provisional: verdict.provisional,
            domain_grounded_fraction: verdict.grounded_fraction,
            deficits: Vec::new(),
            assay_cf_rows_after,
        })
    }

    /// Measures pairwise lens redundancy (normalized MI over deterministic
    /// random-projection sketches) and the panel effective rank (stable rank of
    /// the redundancy matrix), persists redundant pairs to the Assay CF, and
    /// reads the CF back.
    ///
    /// # Fail-closed granularity (issue #1897)
    ///
    /// Redundancy over `N` lenses is `C(N,2)` **independent** measurements. NMI
    /// genuinely is undefined against a zero-entropy column, so refusing to
    /// fabricate one is correct — but refusing at the granularity of the whole
    /// pass discards every pair that *is* defined. On the production agent-event
    /// panel that meant one constant lens out of seven destroyed all 21 pairs,
    /// 15 of which were perfectly measurable.
    ///
    /// The boundary is therefore per pair, not per pass:
    ///
    /// * every pair whose two columns are non-constant and sufficiently sampled
    ///   is measured;
    /// * every skipped pair is reported with its reason and the exact offending
    ///   slot and lens name, rather than vanishing;
    /// * `effective_rank` is computed over the measurable submatrix and the
    ///   lenses it covers are named, so the rank is never silently over a
    ///   smaller set than the caller thinks;
    /// * the whole pass still fails closed when pairs were possible and **none**
    ///   was measurable — a panel of entirely dead lenses is still an error.
    ///
    /// This mirrors how mature statistical libraries treat degenerate columns
    /// (pandas returns a per-pair `NaN` rather than aborting the matrix) while
    /// improving on it: a skipped pair here carries a stated reason instead of a
    /// silent `NaN` a caller must interpret.
    ///
    /// A zero-entropy lens is additionally surfaced as a named
    /// `CALYX_ASSAY_LOW_SIGNAL` finding recommended for parking, because a lens
    /// that carries no information about anything is a defect in the panel — not
    /// merely the reason an unrelated report died.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the corpus cannot be read,
    /// the effective-rank math fails closed, no pair of a multi-lens panel is
    /// measurable, an NMI estimate fails for a reason the pre-checks did not
    /// classify, or the Assay write/readback fails.
    #[allow(
        clippy::too_many_lines,
        reason = "the per-pair classification, the per-lens low-signal finding and the covered-submatrix rank are one measurement pass over one corpus; splitting them would hand each part a separately-pinned view of the sketches"
    )]
    pub fn assay_redundancy(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> Result<SynapseCalyxRedundancyReport, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;

        // Reduce every dense lens to a deterministic 1-D random-projection sketch
        // per record (JL-style), keyed by record index, so heterogeneous-shape
        // lenses can be compared by a discrete normalized-MI estimator.
        let mut sketches: BTreeMap<SlotId, BTreeMap<usize, f32>> = BTreeMap::new();
        for (index, record) in corpus.records.iter().enumerate() {
            for (slot, vector) in &record.slots {
                sketches
                    .entry(*slot)
                    .or_default()
                    .insert(index, project_scalar(vector));
            }
        }
        // The lens set is the panel's **declared** slots, not the subset that
        // produced a sketch. A lens the corpus loader could not carry — a
        // sparse lens above the densification bound, a lens absent on every
        // record — must appear here with an empty column and a named skip on
        // every pair it touches, so `n_lenses` and `pairs_possible` state the
        // panel contract and the shortfall is visible (#1939). Shrinking the
        // denominator instead makes a 12-lens panel report a healthy 8.
        for slot in corpus.panel_slots.keys() {
            sketches.entry(*slot).or_default();
        }
        let slot_ids: Vec<SlotId> = sketches.keys().copied().collect();
        let n_lenses = slot_ids.len();

        // Per-lens degeneracy is a property of the lens over the whole corpus,
        // so it is measured once here and reported as its own finding, instead
        // of being rediscovered inside every pair it poisons.
        let low_signal_lenses: Vec<SynapseCalyxLowSignalLens> = slot_ids
            .iter()
            .filter_map(|slot| {
                let column = &sketches[slot];
                let stats = constant_column_stats(column)?;
                Some(SynapseCalyxLowSignalLens {
                    slot: slot.get(),
                    lens: params.lens_name(slot.get()),
                    code: "CALYX_ASSAY_LOW_SIGNAL".to_owned(),
                    constant_value: stats.value,
                    records_observed: stats.n,
                    distinct_values: 1,
                    remediation:
                        "park or retire this lens: a zero-entropy column over the measured corpus carries no information about any outcome and cannot correlate with any other lens"
                            .to_owned(),
                })
            })
            .collect();

        let mut matrix = vec![vec![0.0_f32; n_lenses]; n_lenses];
        for (index, row) in matrix.iter_mut().enumerate() {
            row[index] = 1.0;
        }
        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let mut redundant_pairs = Vec::new();
        let mut skipped_details: Vec<SynapseCalyxRedundancySkip> = Vec::new();
        let mut measured_slots: BTreeSet<SlotId> = BTreeSet::new();
        let mut pairs_evaluated = 0usize;
        let pairs_possible = n_lenses.saturating_mul(n_lenses.saturating_sub(1)) / 2;
        for i in 0..n_lenses {
            for j in (i + 1)..n_lenses {
                let (slot_a, slot_b) = (slot_ids[i], slot_ids[j]);
                let (paired_a, paired_b) = paired_sketches(&sketches[&slot_a], &sketches[&slot_b]);
                let mut skip = |reason: &str, offending: Option<SlotId>, detail: String| {
                    skipped_details.push(SynapseCalyxRedundancySkip {
                        slot_a: slot_a.get(),
                        slot_b: slot_b.get(),
                        lens_a: params.lens_name(slot_a.get()),
                        lens_b: params.lens_name(slot_b.get()),
                        reason: reason.to_owned(),
                        offending_slot: offending.map(SlotId::get),
                        detail,
                        n_paired: paired_a.len(),
                    });
                };
                // A lens the loader refused is named as such rather than
                // reported as a thin sample: the two call for different action
                // (narrow the lens vs widen the window), and #1939 was exactly
                // the case of a refusal that no surface could observe.
                if let Some(reason) = corpus.unusable_slots.get(&slot_a).or_else(|| {
                    corpus
                        .unusable_slots
                        .get(&slot_b)
                        .filter(|_| corpus.unusable_slots.contains_key(&slot_b))
                }) {
                    let offending = if corpus.unusable_slots.contains_key(&slot_a) {
                        slot_a
                    } else {
                        slot_b
                    };
                    skip(
                        "lens_not_carried_by_corpus",
                        Some(offending),
                        reason.clone(),
                    );
                    continue;
                }
                if paired_a.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                    skip(
                        "insufficient_paired_samples",
                        None,
                        format!(
                            "{} record(s) carry both lenses; the estimator requires {SYNAPSE_ASSAY_MIN_SAMPLES}",
                            paired_a.len()
                        ),
                    );
                    continue;
                }
                // Classified here rather than left to the estimator, so the
                // failure names the offending slot and only this pair is lost.
                if let Some(stats) = constant_slice_stats(&paired_a) {
                    skip(
                        "constant_column",
                        Some(slot_a),
                        format!(
                            "slot {} projects to the constant value {} across all {} paired record(s), so NMI is undefined for this pair",
                            slot_a.get(),
                            stats.value,
                            stats.n
                        ),
                    );
                    continue;
                }
                if let Some(stats) = constant_slice_stats(&paired_b) {
                    skip(
                        "constant_column",
                        Some(slot_b),
                        format!(
                            "slot {} projects to the constant value {} across all {} paired record(s), so NMI is undefined for this pair",
                            slot_b.get(),
                            stats.value,
                            stats.n
                        ),
                    );
                    continue;
                }
                // Any error surviving the pre-checks above is not an expected
                // data property; it fails the whole pass, naming the pair.
                let report =
                    partitioned_histogram_nmi(&paired_a, &paired_b, SYNAPSE_REDUNDANCY_NMI_BINS)
                        .map_err(|error| {
                            loom_math_error(
                                &format!(
                                    "estimate pairwise redundancy NMI for slot {} ({}) x slot {} ({}) over {} paired record(s)",
                                    slot_a.get(),
                                    params.lens_name(slot_a.get()),
                                    slot_b.get(),
                                    params.lens_name(slot_b.get()),
                                    paired_a.len()
                                ),
                                &error,
                            )
                        })?;
                pairs_evaluated += 1;
                measured_slots.insert(slot_a);
                measured_slots.insert(slot_b);
                let nmi = report.nmi.clamp(0.0, 1.0);
                matrix[i][j] = nmi;
                matrix[j][i] = nmi;
                let redundant = report.nmi >= SYNAPSE_ASSAY_CORRELATION_CEILING;
                if redundant {
                    store.put(
                        AssayCacheKey::scoped(
                            params.panel_version,
                            &params.corpus_shard,
                            vault_id,
                            AnchorKind::Reward,
                        ),
                        AssaySubject::Pair {
                            a: slot_a,
                            b: slot_b,
                        },
                        MiEstimate::point(
                            report.mi_bits,
                            paired_a.len(),
                            EstimatorKind::HistogramNmi,
                            TrustTag::Provisional,
                        ),
                        "synapse-assay-redundancy",
                        seq,
                    );
                    redundant_pairs.push(SynapseCalyxRedundancyPair {
                        slot_a: slot_a.get(),
                        slot_b: slot_b.get(),
                        nmi: report.nmi,
                        mi_bits: report.mi_bits,
                        n_samples: paired_a.len(),
                        redundant,
                    });
                }
            }
        }

        // The one place the pass is still allowed to fail wholesale: pairs were
        // available and not one of them could be measured.
        if pairs_possible > 0 && pairs_evaluated == 0 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_REDUNDANCY_NO_MEASURABLE_PAIR",
                format!(
                    "panel {} offers {pairs_possible} lens pair(s) over {n_lenses} lens(es) and {} record(s), and none is measurable: {}",
                    params.panel_version,
                    corpus.records_scanned,
                    format_redundancy_skips(&skipped_details)
                ),
                "park or retire the named zero-entropy lens(es), or widen the record window so enough records carry both lenses of at least one pair",
            ));
        }

        // The rank is reported over exactly the lenses that contributed a
        // measurement. Including a lens whose every pair was skipped would
        // inflate the rank with a row of zeros the pass never measured.
        let (rank_indices, effective_rank_slots): (Vec<usize>, Vec<u16>) = if pairs_possible == 0 {
            (
                (0..n_lenses).collect(),
                slot_ids.iter().map(|slot| slot.get()).collect(),
            )
        } else {
            slot_ids
                .iter()
                .enumerate()
                .filter(|(_, slot)| measured_slots.contains(slot))
                .map(|(index, slot)| (index, slot.get()))
                .unzip()
        };
        let submatrix: Vec<Vec<f32>> = rank_indices
            .iter()
            .map(|row| rank_indices.iter().map(|col| matrix[*row][*col]).collect())
            .collect();
        let effective_rank = if submatrix.is_empty() {
            0.0
        } else {
            stable_rank(&submatrix)
                .map_err(|error| loom_math_error("compute effective rank", &error))?
                .n_eff
        };
        let effective_rank_lenses = effective_rank_slots
            .iter()
            .map(|slot| params.lens_name(*slot))
            .collect();
        // #1670 control-doctrine marker for the domain this rank was read over.
        let verdict = self.domain_grounding_verdict(params.panel_version, max_records)?;
        let assay_cf_rows_after = self.persist_assay_store(&store)?;
        if !skipped_details.is_empty() || !low_signal_lenses.is_empty() {
            tracing::warn!(
                code = "SYNAPSE_CALYX_REDUNDANCY_PARTIAL",
                panel_version = params.panel_version,
                pairs_possible,
                pairs_evaluated,
                pairs_skipped = skipped_details.len(),
                low_signal_lens_count = low_signal_lenses.len(),
                low_signal_lenses = %low_signal_lenses
                    .iter()
                    .map(|lens| format!("slot {} ({})", lens.slot, lens.lens))
                    .collect::<Vec<_>>()
                    .join(", "),
                skipped = %format_redundancy_skips(&skipped_details),
                "redundancy measured the defined pairs and reported the rest by name"
            );
        }
        Ok(SynapseCalyxRedundancyReport {
            panel_version: params.panel_version,
            n_lenses,
            records_scanned: corpus.records_scanned,
            effective_rank,
            pairs_possible,
            pairs_evaluated,
            pairs_skipped: skipped_details.len(),
            skipped_details,
            effective_rank_slots,
            effective_rank_lenses,
            low_signal_lenses,
            domain_provisional: verdict.provisional,
            domain_grounded_fraction: verdict.grounded_fraction,
            redundant_pairs,
            assay_cf_rows_after,
        })
    }

    /// Measures pairwise lens **synergy** about one outcome anchor: for each
    /// evaluated lens pair, the bits the pair carries jointly minus the bits the
    /// better single lens carries alone (`WholeMinusMax`, Griffith & Koch
    /// arXiv:1205.4265), persists the synergistic pairs to the native Assay CF as
    /// `PairGain` rows, and reads the CF back.
    ///
    /// All three terms of every pair are measured over the *same* record subset
    /// — the records where both lenses and the anchor are present — so the
    /// difference is a synergy and not an artefact of differing coverage. Two
    /// lenses carrying identical vectors therefore give a gain of exactly
    /// `0.0`: the KSG estimator is Chebyshev-metric, and concatenating a
    /// duplicate coordinate block leaves every distance unchanged.
    ///
    /// The pass is bounded twice over: records are clamped to
    /// [`SYNAPSE_SYNERGY_MAX_RECORDS`] and the lens set to the
    /// [`SYNAPSE_SYNERGY_MAX_LENSES`] lenses with the highest marginal bits,
    /// because each pair costs three quadratic KSG estimates. Both bounds are
    /// reported (`n_lenses` vs `lenses_paired`), never silently applied.
    ///
    /// # Errors
    ///
    /// Returns [`SYNAPSE_SYNERGY_NO_ANCHORED_RECORDS`] when no record in the
    /// panel carries the requested anchor, or a structured Calyx-backed error
    /// when the corpus cannot be read, the KSG estimator rejects the samples, or
    /// the Assay CF write/readback fails.
    #[allow(clippy::too_many_lines)]
    pub fn assay_synergy(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> Result<SynapseCalyxSynergyReport, SynapseCalyxError> {
        let max_records = params.max_records.clamp(1, SYNAPSE_SYNERGY_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;
        let anchor_kind = parse_anchor_kind(&params.anchor_kind);
        let gathered = gather_anchored_slot_samples(&corpus, &anchor_kind, &params.excluded_slots);
        if gathered.anchored_records == 0 {
            return Err(SynapseCalyxError::new(
                SYNAPSE_SYNERGY_NO_ANCHORED_RECORDS,
                format!(
                    "no record in panel_version={} carries a discrete anchor of kind {}; synergy \
                     is undefined without a grounded outcome",
                    params.panel_version, params.anchor_kind
                ),
                "write grounded outcome anchors of the requested kind onto the panel's records \
                 (storage operation=anchors), then re-run operation=synergy",
            ));
        }
        let ksg_k = params.ksg_k.max(1);
        let n_lenses = gathered.by_slot.len();

        // Rank the lenses by their own marginal bits so the bounded pair budget
        // is spent on the lenses that actually carry signal.
        let mut ranked: Vec<(SlotId, f32)> = Vec::with_capacity(n_lenses);
        for (slot, samples) in &gathered.by_slot {
            if samples.x.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                continue;
            }
            let k = ksg_k.min(samples.x.len().saturating_sub(1)).max(1);
            // #1672: instrument chosen from the column, and a refusal ranks the
            // lens out instead of ending the pass. Propagating here meant one
            // categorical lens aborted the entire synergy report — the same
            // whole-pass-abort shape #1915 removed from `bits`, still standing
            // on this path.
            if let Some(estimate) = mi_about_labels(
                MiEstimatorChoice::Auto,
                &samples.x,
                &samples.labels,
                k,
                None,
            )
            .ok()
            .and_then(|outcome| outcome.estimate.ok())
            {
                ranked.push((*slot, estimate.bits));
            }
        }
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        ranked.truncate(SYNAPSE_SYNERGY_MAX_LENSES);
        let mut paired_slots: Vec<SlotId> = ranked.into_iter().map(|(slot, _)| slot).collect();
        paired_slots.sort_unstable();

        let anchored = anchored_records(&corpus, &anchor_kind);
        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let mut pairs = Vec::new();
        for (index, slot_a) in paired_slots.iter().enumerate() {
            for slot_b in &paired_slots[index + 1..] {
                let mut left = Vec::new();
                let mut right = Vec::new();
                let mut joint = Vec::new();
                let mut labels = Vec::new();
                for (slots, label) in &anchored {
                    let (Some(a), Some(b)) = (slots.get(slot_a), slots.get(slot_b)) else {
                        continue;
                    };
                    let mut concatenated = a.clone();
                    concatenated.extend_from_slice(b);
                    left.push(a.clone());
                    right.push(b.clone());
                    joint.push(concatenated);
                    labels.push(*label);
                }
                if labels.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                    pairs.push(unmeasured_synergy_pair(
                        *slot_a,
                        *slot_b,
                        labels.len(),
                        SynergyPairState::InsufficientSamples,
                        format!(
                            "only {} record(s) carry both lens {} and lens {} together with the \
                             anchor; the synergy floor is {SYNAPSE_ASSAY_MIN_SAMPLES} paired \
                             samples",
                            labels.len(),
                            slot_a.get(),
                            slot_b.get()
                        ),
                    ));
                    continue;
                }
                let k = ksg_k.min(labels.len().saturating_sub(1)).max(1);
                // All three terms must come from one instrument or the
                // difference is not a measurement (#1941). A refusal leaves the
                // pair unmeasured with a named reason rather than ending the
                // report (#1672, #1915).
                let terms = match synergy_pair_bits(&joint, &left, &right, &labels, k) {
                    Ok(terms) => terms,
                    Err((state, reason)) => {
                        pairs.push(unmeasured_synergy_pair(
                            *slot_a,
                            *slot_b,
                            labels.len(),
                            state.into(),
                            reason,
                        ));
                        continue;
                    }
                };
                let pair = synergy_pair(
                    *slot_a,
                    *slot_b,
                    terms.pair_bits,
                    terms.left_bits,
                    terms.right_bits,
                    labels.len(),
                    terms.estimators,
                )
                .map_err(|error| loom_math_error("compute pair synergy gain", &error))?;
                if pair.synergistic {
                    // Only a pair that clears the gain floor earns a durable
                    // PairGain row; a redundant pair is reported, not stored.
                    store.put(
                        AssayCacheKey::scoped(
                            params.panel_version,
                            &params.corpus_shard,
                            vault_id,
                            anchor_kind.clone(),
                        ),
                        AssaySubject::Pair {
                            a: *slot_a,
                            b: *slot_b,
                        },
                        MiEstimate::point(
                            pair.gain_bits,
                            pair.n_samples,
                            EstimatorKind::PairGain,
                            TrustTag::Provisional,
                        ),
                        "synapse-assay-synergy",
                        seq,
                    );
                }
                pairs.push(pair);
            }
        }
        self.persist_assay_store(&store)?;
        let report = synergy_report(
            params.panel_version,
            n_lenses,
            paired_slots.len(),
            gathered.anchored_records,
            pairs,
        );

        // #1959 ask 1: annotate, do not refuse. Structural and independent of
        // the corpus, so it is computed from the panel declaration rather than
        // from which slots this pass happened to pair.
        let source_provenance = lens_provenance::syn_anchor_source_provenance(
            &params.anchor_kind,
            params.panel_version,
            &params.excluded_slots,
        );
        let carrier_slots: BTreeSet<u16> = source_provenance
            .carriers
            .iter()
            .map(|carrier| carrier.slot)
            .collect();
        let contains_carrier = |pair: &calyx_assay::SynergyPair| {
            carrier_slots.contains(&pair.a.get()) || carrier_slots.contains(&pair.b.get())
        };
        let pairs_with_anchor_source_carrier = report
            .pairs
            .iter()
            .filter(|pair| contains_carrier(pair))
            .count();
        let max_gain_bits_carrier_free = report
            .pairs
            .iter()
            .filter(|pair| pair.state == SynergyPairState::Measured && !contains_carrier(pair))
            .map(|pair| pair.gain_bits)
            .fold(0.0_f32, f32::max);

        Ok(SynapseCalyxSynergyReport {
            report,
            anchor_source_declared: source_provenance.anchor_declared,
            anchor_source_carriers: anchor_source_carriers(&source_provenance),
            pairs_with_anchor_source_carrier,
            max_gain_bits_carrier_free,
        })
    }

    /// Measures the panel's **ensemble capability card**: per-lens marginal
    /// value, the PID triple, every pairwise gain, the A37 associational
    /// diversity gate, and the keep/park/retire verdict — the admission gate of
    /// the handbook's capability card, over the real vault corpus.
    ///
    /// This is the first Synapse surface onto `calyx_assay::ensemble_card`; the
    /// path previously existed with no caller, which is how the defect #1942
    /// records survived on it (a second, undisciplined implementation of the
    /// pair gain that nothing ever ran).
    ///
    /// Every term is measured by one instrument over one paired sample set, and
    /// the pass **fails closed** rather than reporting a number the corpus does
    /// not support: a non-binary anchor is refused, not collapsed; a lens absent
    /// from any anchored record is excluded from the panel vector rather than
    /// zero-filled; and a pair whose three terms did not come from one estimator
    /// aborts the pass.
    ///
    /// # Errors
    ///
    /// [`SYNAPSE_ENSEMBLE_NO_ANCHORED_RECORDS`] when no record carries the
    /// anchor, [`SYNAPSE_ENSEMBLE_ANCHOR_NOT_BINARY`] when it is not a
    /// two-outcome anchor, [`SYNAPSE_ENSEMBLE_NO_COPRESENT_LENSES`] when too few
    /// lenses are present on every anchored record, and the underlying Calyx
    /// error for any refusal inside the assay itself.
    #[expect(
        clippy::too_many_lines,
        reason = "the ensemble card is one fail-closed paired assay whose shared corpus and estimator provenance must remain co-located"
    )]
    pub fn assay_ensemble_card(
        &self,
        params: &SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> Result<SynapseCalyxEnsembleCardReport, SynapseCalyxError> {
        // #1959 ask 2 / #1958: the card's headline is a PANEL-level claim —
        // `sufficient`, `panel_bits`, and a keep/park/retire count — with no
        // per-slot breakdown a reader could inspect for a carrier. That is the
        // same shape as `sufficiency`, and it gets the same structural refusal.
        // Taken before the corpus scan so a circular configuration costs nothing.
        let source_provenance = lens_provenance::syn_anchor_source_provenance(
            &params.anchor_kind,
            params.panel_version,
            &params.excluded_slots,
        );
        if !source_provenance.carriers.is_empty() {
            return Err(anchor_source_leakage_error(params, &source_provenance));
        }
        let max_records = params.max_records.clamp(1, SYNAPSE_ENSEMBLE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;
        let anchor_kind = parse_anchor_kind(&params.anchor_kind);

        // One pass over the corpus: the anchored records, their interned
        // outcome, and the slots each one carries.
        let mut interner: BTreeMap<String, usize> = BTreeMap::new();
        let mut anchored: Vec<(&BTreeMap<SlotId, Vec<f32>>, usize)> = Vec::new();
        for record in &corpus.records {
            let Some(anchor) = anchor_of_kind(&record.anchors, &anchor_kind) else {
                continue;
            };
            let Some(label) = discrete_anchor_label(&anchor.value, &mut interner) else {
                continue;
            };
            anchored.push((&record.slots, label));
        }
        if anchored.is_empty() {
            return Err(SynapseCalyxError::new(
                SYNAPSE_ENSEMBLE_NO_ANCHORED_RECORDS,
                format!(
                    "no record in panel_version={} carries a discrete anchor of kind {}; a \
                     capability card measures lens value *about a grounded outcome* and is \
                     undefined without one",
                    params.panel_version, params.anchor_kind
                ),
                "write grounded outcome anchors of the requested kind onto the panel's records \
                 (storage operation=anchors), then re-run the ensemble card",
            ));
        }
        if interner.len() != 2 {
            let outcomes = interner.keys().cloned().collect::<Vec<_>>().join(", ");
            return Err(SynapseCalyxError::new(
                SYNAPSE_ENSEMBLE_ANCHOR_NOT_BINARY,
                format!(
                    "anchor kind {} carries {} distinct outcome(s) over {} anchored record(s) \
                     [{outcomes}]; the ensemble card's decision surrogate is a binary logistic \
                     probe and a non-binary outcome is refused rather than silently collapsed to \
                     one-versus-rest",
                    params.anchor_kind,
                    interner.len(),
                    anchored.len()
                ),
                "measure a two-outcome anchor, or add a one-versus-rest anchor kind explicitly \
                 so the collapse is a declared measurement rather than an implicit one",
            ));
        }

        // The panel vector is the intersection: a lens missing from an anchored
        // record cannot be zero-filled, because `Absent` is an explicit absence
        // and never a zero vector.
        let mut copresent: Option<BTreeSet<SlotId>> = None;
        for (slots, _) in &anchored {
            let present: BTreeSet<SlotId> = slots.keys().copied().collect();
            copresent = Some(match copresent.take() {
                Some(current) => current.intersection(&present).copied().collect(),
                None => present,
            });
        }
        // `excluded_slots` is honoured here, not only by the corpus loader.
        //
        // Manual FSV found that this path ignored the parameter entirely, so
        // withheld slots still entered the card. That is worse than
        // a missing feature — `excluded_slots` is the documented remediation for
        // the structural anchor-leakage refusal above, so a caller could lift the
        // refusal and still be measuring the label. The refusal and the way to
        // satisfy it have to act on the same set.
        let mut copresent = copresent.unwrap_or_default();
        copresent.retain(|slot| !params.excluded_slots.contains(&slot.get()));

        // Every declared slot the card will not carry is named with the reason
        // it cannot be carried. A lens that vanishes from a capability card is
        // indistinguishable from a lens the card judged worthless, which is the
        // exact failure #1939 removed from the corpus loader.
        let mut excluded: Vec<SynapseCalyxExcludedLens> = Vec::new();
        for (slot, kind) in &corpus.panel_slots {
            // A slot the loader already declared unusable is reported once,
            // with the loader's reason. Reporting it again as "not co-present"
            // would describe the *consequence* of the first exclusion as if it
            // were a second, independent finding.
            if copresent.contains(slot) || corpus.unusable_slots.contains_key(slot) {
                continue;
            }
            // A slot the caller withheld says so in its own words. Describing it
            // as "not co-present" would attribute a caller's decision to the
            // corpus, and a withheld label carrier is the one exclusion a reader
            // most needs to see stated.
            if params.excluded_slots.contains(&slot.get()) {
                excluded.push(SynapseCalyxExcludedLens {
                    slot: slot.get(),
                    name: params.lens_name(slot.get()),
                    reason: "withheld by excluded_slots on this request".to_owned(),
                });
                continue;
            }
            let carried = anchored
                .iter()
                .filter(|(slots, _)| slots.contains_key(slot))
                .count();
            excluded.push(SynapseCalyxExcludedLens {
                slot: slot.get(),
                name: params.lens_name(slot.get()),
                reason: format!(
                    "carried by {carried} of the {} anchored record(s) (kind {kind:?}); a lens \
                     absent from an anchored record cannot be zero-filled into the panel vector, \
                     because Absent is an explicit absence and never a zero measurement",
                    anchored.len()
                ),
            });
        }
        for (slot, reason) in &corpus.unusable_slots {
            excluded.push(SynapseCalyxExcludedLens {
                slot: slot.get(),
                name: params.lens_name(slot.get()),
                reason: reason.clone(),
            });
        }

        // A lens whose width differs across records cannot form a rectangular
        // column, and a lens whose every anchored row is the same vector has
        // zero centered energy — correlation, normalized MI and the logistic
        // probe are all undefined on it. Both are named, never padded and never
        // allowed to abort the pass: one degenerate lens must cost its own row,
        // not the whole card (#1915's discipline, on this path).
        let mut lenses: Vec<EnsembleLensInput> = Vec::new();
        for slot in &copresent {
            let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(anchored.len());
            let mut widths: BTreeSet<usize> = BTreeSet::new();
            for (slots, _) in &anchored {
                if let Some(vector) = slots.get(slot) {
                    widths.insert(vector.len());
                    vectors.push(vector.clone());
                }
            }
            if widths.len() != 1 || widths.contains(&0) {
                excluded.push(SynapseCalyxExcludedLens {
                    slot: slot.get(),
                    name: params.lens_name(slot.get()),
                    reason: format!(
                        "ragged column: widths {:?} across {} anchored record(s); a capability \
                         card needs one rectangular column per lens and padding one would \
                         manufacture measurements the lens never made",
                        widths.iter().collect::<Vec<_>>(),
                        vectors.len()
                    ),
                });
                continue;
            }
            let energy = centered_energy(&vectors);
            if !energy.is_finite() || energy <= 0.0 {
                excluded.push(SynapseCalyxExcludedLens {
                    slot: slot.get(),
                    name: params.lens_name(slot.get()),
                    reason: format!(
                        "constant column: centered energy {energy} over {} anchored record(s). \
                         Every row is the same vector, so this lens separates nothing about the \
                         outcome; linear CKA, normalized MI and the logistic probe are all \
                         undefined on it. Park or retire the lens",
                        vectors.len()
                    ),
                });
                continue;
            }
            // The redundancy term bins a 1-D sketch of each lens, and a sketch
            // that collapses to one value has zero entropy. Checked here with
            // the *same* function the assay uses, so the caller cannot disagree
            // with the engine about which lenses are measurable, and so one
            // degenerate lens costs its own row instead of the whole card.
            let signature = ensemble_nmi_signature(&vectors)
                .map_err(|error| loom_math_error("sketch the lens for NMI redundancy", &error))?;
            let distinct = signature
                .iter()
                .map(|value| value.to_bits())
                .collect::<BTreeSet<_>>();
            if distinct.len() < 2 {
                excluded.push(SynapseCalyxExcludedLens {
                    slot: slot.get(),
                    name: params.lens_name(slot.get()),
                    reason: format!(
                        "degenerate redundancy sketch: the 1-D NMI signature takes {} distinct \
                         value(s) over {} anchored record(s), so the pairwise normalized-MI term \
                         has zero entropy and is undefined. The lens varies too little for the \
                         binned sketch to separate its rows",
                        distinct.len(),
                        vectors.len()
                    ),
                });
                continue;
            }
            lenses.push(EnsembleLensInput::new(
                params.lens_name(slot.get()),
                *slot,
                vectors,
            ));
        }
        if lenses.len() < MIN_ENSEMBLE_PANEL_LENSES {
            let named = excluded
                .iter()
                .map(|lens| format!("slot {} ({}): {}", lens.slot, lens.name, lens.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SynapseCalyxError::new(
                SYNAPSE_ENSEMBLE_NO_COPRESENT_LENSES,
                format!(
                    "panel_version={} has {} measurable lens(es) over the {} anchored record(s); \
                     a capability card needs at least {MIN_ENSEMBLE_PANEL_LENSES}. Panel declares \
                     {} slot(s). Excluded: [{named}]",
                    params.panel_version,
                    lenses.len(),
                    anchored.len(),
                    corpus.panel_slots.len(),
                ),
                "backfill the panel so its lenses are co-present on the anchored records, or \
                 measure a panel version whose records carry the full slot set",
            ));
        }

        let labels: Vec<bool> = anchored.iter().map(|(_, label)| *label == 1).collect();
        let config = EnsembleConfig {
            source: "synapse-assay-ensemble".to_string(),
            min_gate_lenses,
            min_marginal_bits: SYNAPSE_ASSAY_BIT_FLOOR,
            max_redundancy: SYNAPSE_ASSAY_CORRELATION_CEILING,
            nmi_bins: SYNAPSE_REDUNDANCY_NMI_BINS,
        };
        let measured_slots = lenses
            .iter()
            .map(|lens| lens.slot.get())
            .collect::<Vec<_>>();
        let card = ensemble_card(&lenses, &labels, None, &config)
            .map_err(|error| loom_math_error("measure the ensemble capability card", &error))?;

        // Persist the complete calibrated evidence contract consumed by the
        // Oracle honesty gate, plus the card itself. Storing only the card made
        // its measured panel/lens evidence invisible to downstream consumers.
        let mut store = AssayStore::default();
        let cache_key = AssayCacheKey::scoped(
            params.panel_version,
            &params.corpus_shard,
            self.vault_id_value(),
            anchor_kind,
        );
        let calibration = card.sufficiency.power_calibration.clone().ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_ASSAY_CALIBRATION_ABSENT",
                "ensemble card returned no planted-signal power calibration",
                "preserve the corpus and inspect the calibrated logistic estimator",
            )
        })?;
        let trust = card.sufficiency.trust;
        store.put(
            cache_key.clone(),
            AssaySubject::EnsembleCard,
            MiEstimate::new(
                card.panel_bits,
                card.panel_ci[0],
                card.panel_ci[1],
                card.n_samples,
                EstimatorKind::LogisticProbe,
                TrustTag::Provisional,
            ),
            "synapse-assay-ensemble",
            self.read_snapshot(),
        );
        store.put(
            cache_key.clone(),
            AssaySubject::Panel,
            MiEstimate::new(
                card.panel_bits,
                card.panel_ci[0],
                card.panel_ci[1],
                card.n_samples,
                EstimatorKind::LogisticProbe,
                trust,
            )
            .with_power_calibration(calibration),
            "synapse-assay-ensemble",
            self.read_snapshot(),
        );
        store.put(
            cache_key.clone(),
            AssaySubject::OutcomeEntropy,
            MiEstimate::point(
                card.anchor_entropy_bits,
                card.n_samples,
                EstimatorKind::OutcomeEntropy,
                trust,
            ),
            "synapse-assay-ensemble",
            self.read_snapshot(),
        );
        for lens in &card.lenses {
            store.put(
                cache_key.clone(),
                AssaySubject::Lens { slot: lens.slot },
                MiEstimate::new(
                    lens.solo_bits,
                    lens.solo_ci[0],
                    lens.solo_ci[1],
                    card.n_samples,
                    EstimatorKind::LogisticProbe,
                    trust,
                ),
                "synapse-assay-ensemble",
                self.read_snapshot(),
            );
        }
        let assay_rows = self.persist_assay_store(&store)?;
        Ok(SynapseCalyxEnsembleCardReport {
            card,
            panel_version: params.panel_version,
            anchor_kind: params.anchor_kind.clone(),
            records_scanned: corpus.records_scanned,
            anchored_records: anchored.len(),
            declared_slots: corpus.panel_slots.len(),
            measured_slots,
            excluded_lenses: excluded,
            anchor_source_declared: source_provenance.anchor_declared,
            assay_cf_rows: assay_rows,
        })
    }

    /// Persists an in-memory Assay store to the native Assay CF and returns the
    /// physical Assay CF row count read back afterwards.
    fn persist_assay_store(&self, store: &AssayStore) -> Result<usize, SynapseCalyxError> {
        if !store.is_empty() {
            store
                .persist_to_vault(&self.vault)
                .map_err(|error| loom_math_error("persist Assay CF rows", &error))?;
        }
        Ok(self
            .count_cf_latest_bounded(ColumnFamily::Assay)?
            .rows_visited)
    }
}

/// One ensemble capability-card pass, with every declared lens the card could
/// not carry named beside the card itself.
///
/// The exclusions are part of the result, not a log line: a lens missing from a
/// capability card is otherwise indistinguishable from a lens the card judged
/// worthless (#1939's discipline, on this path).
#[derive(Clone, Debug)]
pub struct SynapseCalyxEnsembleCardReport {
    pub card: EnsembleCard,
    pub panel_version: u32,
    pub anchor_kind: String,
    /// Records of this panel version seen by the bounded scan.
    pub records_scanned: usize,
    /// Of those, the records carrying the requested anchor — the card's sample.
    pub anchored_records: usize,
    /// Slots the corpus declares for this panel.
    pub declared_slots: usize,
    /// Slots that entered the card as lenses.
    pub measured_slots: Vec<u16>,
    pub excluded_lenses: Vec<SynapseCalyxExcludedLens>,
    /// Whether this (anchor kind, panel version) pair declares which record
    /// fields determine the anchor, so the structural leakage check could run
    /// (#1958).
    ///
    /// A card can only reach a caller when no carrier was found, so `false`
    /// means the check did not run rather than that it ran and found nothing.
    pub anchor_source_declared: bool,
    /// Physical Assay CF row count read back after the pass persisted its row.
    pub assay_cf_rows: usize,
}

/// A declared lens the capability card could not carry, and why.
#[derive(Clone, Debug)]
pub struct SynapseCalyxExcludedLens {
    pub slot: u16,
    pub name: String,
    pub reason: String,
}

/// Total centered energy `Σ_r ||x_r - mean||²` of a lens column.
///
/// Zero means every row is the same vector: the column is constant, and linear
/// CKA, normalized MI and the logistic probe are all undefined on it. Computed
/// in `f64` because the sum runs over every row and every dimension.
#[expect(
    clippy::cast_precision_loss,
    reason = "assay corpus rows are bounded by SYNAPSE_INTELLIGENCE_MAX_RECORDS, far below f64's exact integer range"
)]
fn centered_energy(vectors: &[Vec<f32>]) -> f64 {
    let Some(width) = vectors.first().map(Vec::len) else {
        return 0.0;
    };
    let rows = vectors.len() as f64;
    if rows == 0.0 || width == 0 {
        return 0.0;
    }
    let mut mean = vec![0.0f64; width];
    for vector in vectors {
        for (accumulator, value) in mean.iter_mut().zip(vector) {
            *accumulator += f64::from(*value);
        }
    }
    for value in &mut mean {
        *value /= rows;
    }
    let mut energy = 0.0f64;
    for vector in vectors {
        for (value, centre) in vector.iter().zip(&mean) {
            let delta = f64::from(*value) - centre;
            energy = delta.mul_add(delta, energy);
        }
    }
    energy
}

struct GatheredAnchoredSamples {
    by_slot: BTreeMap<SlotId, AnchoredSlotSamples>,
    representative: Option<Anchor>,
    anchored_records: usize,
    distinct_outcomes: usize,
}

struct JointAnchoredSamples {
    x: Vec<Vec<f32>>,
    labels: Vec<usize>,
}

/// A slot the estimator did not measure. `marginal_bits` is a placeholder zero
/// and `state`/`unmeasured_reason` say so, so nothing downstream can mistake it
/// for a measured zero (#1915).
const fn unmeasured_slot_bits(
    slot: u16,
    n_samples: usize,
    state: SynapseCalyxSlotBitsState,
    reason: String,
) -> SynapseCalyxSlotBits {
    SynapseCalyxSlotBits {
        slot,
        marginal_bits: 0.0,
        ci_low: 0.0,
        ci_high: 0.0,
        n_samples,
        sole_carrier: false,
        provisional: true,
        state,
        unmeasured_reason: Some(reason),
        estimator: None,
        estimator_selection: None,
        estimator_reason: None,
        distinct_values: None,
        max_same_label_multiplicity: None,
        anchor_source_carrier: false,
        anchor_source_shared_fields: Vec::new(),
    }
}

/// A slot the estimator did not measure, carrying the estimator-selection facts
/// that were already established before the refusal.
///
/// Selection runs before estimation, so when a column is refused we still know
/// which instrument was chosen and the column cardinality that chose it. That
/// is exactly the evidence an operator needs to act on the refusal — dropping
/// it would make "why did this lens not measure" unanswerable without a rerun.
fn unmeasured_slot_bits_with_pick(
    slot: u16,
    n_samples: usize,
    state: SynapseCalyxSlotBitsState,
    reason: String,
    pick: &MiEstimatorPick,
) -> SynapseCalyxSlotBits {
    let mut bits = unmeasured_slot_bits(slot, n_samples, state, reason);
    apply_estimator_pick(&mut bits, pick);
    bits
}

/// The three same-instrument bit terms of one synergy pair.
struct SynergyTerms {
    pair_bits: f32,
    left_bits: f32,
    right_bits: f32,
    estimators: SynergyEstimators,
}

/// Bits for a synergy pair and each of its halves, **all three from one
/// instrument**, or a named refusal.
///
/// Two conditions make `gain = pair − max(left, right)` a measurement rather
/// than arithmetic on two unrelated numbers (#1941):
///
/// *Same samples.* Enforced by the caller, which builds all three columns from
/// the same anchored record subset.
///
/// *Same instrument.* Enforced here. `Auto` resolves each column independently
/// from its own cardinality, so on a hybrid panel the concatenated pair and its
/// halves routinely land on different estimators — a one-hot half on the
/// Miller-Madow-corrected plug-in, the pair on KSG because a record-vector
/// component makes every row distinct. Subtracting one from the other
/// reintroduces exactly the non-cancelling bias KSG is constructed to remove
/// (Kraskov et al. 2004), and the difference estimates nothing.
///
/// So: resolve all three under `Auto`; if they already agree, measure with
/// `Auto`. If they disagree, try to *pin* all three to one instrument — the
/// discrete plug-in first (exact for a genuinely discrete column, and its
/// sparsity guard refuses a concatenation too wide for its bias correction),
/// then KSG (whose degenerate-radius guard refuses a categorical column). The
/// first pinning under which all three columns yield an estimate wins. When
/// neither does, the pair is genuinely unmeasurable as a gain and is refused
/// with the reason, never reported as a number.
#[allow(
    clippy::too_many_lines,
    reason = "the estimator agreement check, the ordered pinning attempts and the two refusal messages are one decision: splitting them would let a caller take a pinned measurement without the homogeneity check that makes it a measurement"
)]
fn synergy_pair_bits(
    joint: &[Vec<f32>],
    left: &[Vec<f32>],
    right: &[Vec<f32>],
    labels: &[usize],
    k: usize,
) -> Result<SynergyTerms, (SynapseCalyxSynergyRefusal, String)> {
    let columns = [("pair", joint), ("left", left), ("right", right)];
    let mut auto_picks = Vec::with_capacity(3);
    for (name, column) in columns {
        match resolve_mi_estimator(MiEstimatorChoice::Auto, column, labels, k) {
            Ok(pick) => auto_picks.push((name, pick)),
            Err(error) => {
                return Err((
                    SynapseCalyxSynergyRefusal::EstimatorRefused,
                    format!(
                        "estimator selection refused the {name} column of this pair: {error}. All \
                         three terms of a synergy gain must be measurable; the pair is reported \
                         unmeasured rather than partially"
                    ),
                ));
            }
        }
    }
    let auto_summary = auto_picks
        .iter()
        .map(|(name, pick)| format!("{name}={}", pick.estimator.as_str()))
        .collect::<Vec<_>>()
        .join(" ");
    let homogeneous_auto = auto_picks
        .iter()
        .all(|(_, pick)| pick.estimator == auto_picks[0].1.estimator);

    // Candidate pinnings, most-preferred first. `Auto` is first only when it is
    // already homogeneous, so a pin never overrides an instrument the columns
    // themselves agreed on.
    let mut attempts: Vec<MiEstimatorChoice> = Vec::with_capacity(3);
    if homogeneous_auto {
        attempts.push(MiEstimatorChoice::Auto);
    } else {
        attempts.push(MiEstimatorChoice::DiscretePlugin);
        attempts.push(MiEstimatorChoice::ContinuousKsg);
    }

    let mut refusals: Vec<String> = Vec::new();
    for choice in attempts {
        let mut measured: Vec<(f32, MiEstimator)> = Vec::with_capacity(3);
        let mut failed = false;
        for (name, column) in columns {
            match mi_about_labels(choice, column, labels, k, None) {
                Ok(outcome) => match outcome.estimate {
                    Ok(estimate) => measured.push((estimate.bits, outcome.pick.estimator)),
                    Err(error) => {
                        refusals.push(format!(
                            "pinning to {} refused the {name} column: {error}",
                            outcome.pick.estimator.as_str()
                        ));
                        failed = true;
                        break;
                    }
                },
                Err(error) => {
                    refusals.push(format!("pinning refused the {name} column: {error}"));
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        let estimators = SynergyEstimators {
            pair: measured[0].1,
            left: measured[1].1,
            right: measured[2].1,
        };
        // Belt and braces: `Auto` is only attempted when selection already
        // agreed, and a pin resolves every column to the pinned instrument, so
        // this cannot fire — but the invariant the whole function exists to
        // hold is checked rather than assumed.
        if !estimators.is_homogeneous() {
            refusals.push(format!(
                "a pinned pass still produced mixed instruments (pair={} left={} right={})",
                estimators.pair.as_str(),
                estimators.left.as_str(),
                estimators.right.as_str()
            ));
            continue;
        }
        return Ok(SynergyTerms {
            pair_bits: measured[0].0,
            left_bits: measured[1].0,
            right_bits: measured[2].0,
            estimators,
        });
    }

    let state = if homogeneous_auto {
        SynapseCalyxSynergyRefusal::EstimatorRefused
    } else {
        SynapseCalyxSynergyRefusal::CrossEstimatorUnpinnable
    };
    let reason = if homogeneous_auto {
        format!(
            "all three columns selected the same instrument ({auto_summary}) but it refused at \
             least one of them: {}. The pair is reported unmeasured rather than partially",
            refusals.join("; ")
        )
    } else {
        format!(
            "the three terms of this pair resolve to different instruments ({auto_summary}), and \
             no single instrument can measure all three: {}. `gain = pair - max(left, right)` \
             across two instruments subtracts estimates whose biases do not cancel (Kraskov et \
             al. 2004), so it estimates nothing and is refused rather than reported",
            refusals.join("; ")
        )
    };
    Err((state, reason))
}

/// Which refusal a synergy pair carries. Mirrors [`SynergyPairState`]'s
/// non-measured arms; the sample-count arm is raised by the caller before any
/// column is built.
#[derive(Clone, Copy, Debug)]
enum SynapseCalyxSynergyRefusal {
    EstimatorRefused,
    CrossEstimatorUnpinnable,
}

impl From<SynapseCalyxSynergyRefusal> for SynergyPairState {
    fn from(value: SynapseCalyxSynergyRefusal) -> Self {
        match value {
            SynapseCalyxSynergyRefusal::EstimatorRefused => Self::EstimatorRefused,
            SynapseCalyxSynergyRefusal::CrossEstimatorUnpinnable => Self::CrossEstimatorUnpinnable,
        }
    }
}

/// Stamp the resolved estimator and the measured column facts onto a slot row.
fn apply_estimator_pick(bits: &mut SynapseCalyxSlotBits, pick: &MiEstimatorPick) {
    bits.estimator = Some(pick.estimator.as_str().to_owned());
    bits.estimator_selection = Some(pick.selection.as_str().to_owned());
    bits.estimator_reason = Some(pick.reason.clone());
    bits.distinct_values = Some(pick.distinct_values);
    bits.max_same_label_multiplicity = Some(pick.max_same_label_multiplicity);
}

/// Classify an estimator refusal into a per-slot unmeasured state.
///
/// Total by construction: every error maps to a state and none maps back to a
/// whole-pass abort. See [`SynapseCalyxSlotBitsState::EstimatorRefused`] for why
/// matching a whitelist of codes was the wrong shape.
fn estimator_refusal_state(error: &calyx_core::CalyxError) -> SynapseCalyxSlotBitsState {
    match error.code {
        "CALYX_ASSAY_DEGENERATE_INPUT" => SynapseCalyxSlotBitsState::DegenerateColumn,
        "CALYX_ASSAY_INSUFFICIENT_SAMPLES" => SynapseCalyxSlotBitsState::InsufficientSamples,
        _ => SynapseCalyxSlotBitsState::EstimatorRefused,
    }
}

/// The operator-facing explanation for a per-slot estimator refusal.
fn estimator_refusal_reason(
    state: SynapseCalyxSlotBitsState,
    error: &calyx_core::CalyxError,
) -> String {
    match state {
        SynapseCalyxSlotBitsState::DegenerateColumn => format!(
            "the estimator refused this column: {error}. A categorical/one-hot lens is degenerate              for a continuous KSG estimator by construction; park or retire it, or measure it with              a discrete estimator"
        ),
        SynapseCalyxSlotBitsState::InsufficientSamples => format!(
            "the estimator refused this column: {error}. The paired-sample count cleared the              report-level floor, but a per-label class is still too small for this k; anchor more              outcomes in the minority class, or lower ksg_k"
        ),
        _ => format!(
            "the estimator refused this column: {error}. This code has no specific handling yet;              the slot is reported unmeasured rather than ending the pass"
        ),
    }
}

fn gather_anchored_slot_samples(
    corpus: &DenseCorpus,
    anchor_kind: &AnchorKind,
    excluded_slots: &BTreeSet<u16>,
) -> GatheredAnchoredSamples {
    let mut by_slot: BTreeMap<SlotId, AnchoredSlotSamples> = BTreeMap::new();
    let mut interner: BTreeMap<String, usize> = BTreeMap::new();
    let mut representative = None;
    let mut anchored_records = 0usize;
    for record in &corpus.records {
        let Some(anchor) = anchor_of_kind(&record.anchors, anchor_kind) else {
            continue;
        };
        let Some(label) = discrete_anchor_label(&anchor.value, &mut interner) else {
            continue;
        };
        if representative.is_none() {
            representative = Some(anchor.clone());
        }
        anchored_records += 1;
        for (slot, vector) in &record.slots {
            // Withheld before the sample exists, so an excluded slot cannot
            // reach the per-lens bits, the joint estimate, the deficit split or
            // the leakage detector by any path (#1953).
            if excluded_slots.contains(&slot.get()) {
                continue;
            }
            let entry = by_slot.entry(*slot).or_insert_with(|| AnchoredSlotSamples {
                x: Vec::new(),
                labels: Vec::new(),
            });
            entry.x.push(vector.clone());
            entry.labels.push(label);
        }
    }
    GatheredAnchoredSamples {
        by_slot,
        representative,
        anchored_records,
        distinct_outcomes: interner.len(),
    }
}

/// Collects the anchored records of a corpus once: their dense slot maps paired
/// with the interned discrete outcome label. Pair passes filter this list rather
/// than re-walking and re-interning the corpus per pair.
fn anchored_records<'corpus>(
    corpus: &'corpus DenseCorpus,
    anchor_kind: &AnchorKind,
) -> Vec<(&'corpus BTreeMap<SlotId, Vec<f32>>, usize)> {
    let mut interner: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchored = Vec::new();
    for record in &corpus.records {
        let Some(anchor) = anchor_of_kind(&record.anchors, anchor_kind) else {
            continue;
        };
        let Some(label) = discrete_anchor_label(&anchor.value, &mut interner) else {
            continue;
        };
        anchored.push((&record.slots, label));
    }
    anchored
}

/// Builds the joint panel samples for sufficiency: the concatenation of the
/// slots present in every anchored record (the coverage intersection), so the
/// joint feature vector has one consistent dimension.
/// Builds the joint sample for the panel sufficiency estimate.
///
/// `usable_slots` excludes every lens the marginal pass individually refused
/// (#1916). KSG is a k-nearest-neighbour estimator, so each concatenated
/// dimension that carries no usable geometry dilutes the neighbourhood and
/// biases the estimate **downward**. Carrying a lens the marginal estimator
/// already rejected as degenerate therefore cannot help and provably hurts:
/// on the known-MI fixture the joint came back at 0.800762 bits while one of
/// its own lenses measured 0.993596, which violates `I(panel;A) >= I(slot;A)`.
///
/// An empty `usable_slots` means nothing was measurable, and the caller must
/// treat the result as unmeasured rather than as a panel carrying no
/// information.
fn build_joint_samples(
    corpus: &DenseCorpus,
    anchor_kind: &AnchorKind,
    usable_slots: &BTreeSet<SlotId>,
) -> JointAnchoredSamples {
    let mut interner: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchored: Vec<(&BTreeMap<SlotId, Vec<f32>>, usize)> = Vec::new();
    for record in &corpus.records {
        let Some(anchor) = anchor_of_kind(&record.anchors, anchor_kind) else {
            continue;
        };
        let Some(label) = discrete_anchor_label(&anchor.value, &mut interner) else {
            continue;
        };
        anchored.push((&record.slots, label));
    }
    let mut required: Option<BTreeSet<SlotId>> = None;
    for (slots, _) in &anchored {
        let present: BTreeSet<SlotId> = slots.keys().copied().collect();
        required = Some(match required.take() {
            Some(current) => current.intersection(&present).copied().collect(),
            None => present,
        });
    }
    // Intersect co-presence with the lenses the marginal pass could actually
    // measure, so a refused lens cannot enter the joint vector as a dilution
    // dimension (#1916).
    let required: BTreeSet<SlotId> = required
        .unwrap_or_default()
        .intersection(usable_slots)
        .copied()
        .collect();
    let mut x = Vec::new();
    let mut labels = Vec::new();
    for (slots, label) in anchored {
        let mut joint = Vec::new();
        for slot in &required {
            if let Some(vector) = slots.get(slot) {
                joint.extend_from_slice(vector);
            }
        }
        if !joint.is_empty() {
            x.push(joint);
            labels.push(label);
        }
    }
    JointAnchoredSamples { x, labels }
}

fn mark_sole_carriers(slots: &mut [SynapseCalyxSlotBits], attributions: &[SlotAttribution]) {
    for attribution in attributions {
        if let Some(entry) = slots
            .iter_mut()
            .find(|slot| slot.slot == attribution.slot.get())
        {
            entry.sole_carrier = attribution.sole_carrier;
        }
    }
}

/// The observed constant value of a degenerate column, and how many records it
/// was observed over.
struct ConstantColumn {
    value: f32,
    n: usize,
}

/// Classifies a whole lens column as constant, for the per-lens low-signal
/// finding (#1897). An empty column is not "constant" — it is unobserved.
#[allow(
    clippy::float_cmp,
    reason = "zero entropy is exact equality, not approximate: a column whose values differ by any representable amount carries information, and an epsilon here would discard a real, measurable lens as degenerate"
)]
fn constant_column_stats(column: &BTreeMap<usize, f32>) -> Option<ConstantColumn> {
    let mut values = column.values();
    let first = *values.next()?;
    values
        .all(|value| *value == first)
        .then_some(ConstantColumn {
            value: first,
            n: column.len(),
        })
}

/// Classifies one side of an already-paired sample as constant.
///
/// This is checked on the *paired* slice rather than the full column because a
/// lens can be non-constant panel-wide yet constant across the exact records it
/// shares with the other lens — and it is that intersection the estimator sees.
#[allow(
    clippy::float_cmp,
    reason = "zero entropy is exact equality; see constant_column_stats"
)]
fn constant_slice_stats(values: &[f32]) -> Option<ConstantColumn> {
    let first = *values.first()?;
    values
        .iter()
        .all(|value| *value == first)
        .then_some(ConstantColumn {
            value: first,
            n: values.len(),
        })
}

/// Renders skipped pairs into one operator-readable line that always names the
/// offending slot and its lens.
fn format_redundancy_skips(skips: &[SynapseCalyxRedundancySkip]) -> String {
    if skips.is_empty() {
        return "none".to_owned();
    }
    skips
        .iter()
        .map(|skip| {
            format!(
                "[{}({}) x {}({}) reason={} offending_slot={} n_paired={} {}]",
                skip.slot_a,
                skip.lens_a,
                skip.slot_b,
                skip.lens_b,
                skip.reason,
                skip.offending_slot
                    .map_or_else(|| "none".to_owned(), |slot| slot.to_string()),
                skip.n_paired,
                skip.detail
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn paired_sketches(a: &BTreeMap<usize, f32>, b: &BTreeMap<usize, f32>) -> (Vec<f32>, Vec<f32>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    for (index, value_a) in a {
        if let Some(value_b) = b.get(index) {
            left.push(*value_a);
            right.push(*value_b);
        }
    }
    (left, right)
}

/// Deterministic JL-style random projection of a slot vector onto one pseudo-
/// random unit direction seeded by the slot id, reducing any-shape dense lens
/// to a single scalar per record for the discrete NMI redundancy estimator.
#[allow(clippy::cast_precision_loss)]
/// Reduces one dense lens vector to a deterministic 1-D random-projection
/// sketch (JL-style).
///
/// The projection weights are seeded by **coordinate index only**, never by slot
/// id: every lens is measured through the same linear functional, so two lenses
/// carrying identical vectors collapse to identical sketches and their
/// normalized MI is exactly `1.0`. A slot-seeded direction would have given two
/// exact duplicates a different sketch each and understated their redundancy.
fn project_scalar(vector: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for (index, value) in vector.iter().enumerate() {
        let seed = splitmix64((index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        // Map the hashed bits into a symmetric weight in [-1, 1].
        let unit = ((seed >> 11) as f32 / (1u64 << 53) as f32).mul_add(2.0, -1.0);
        acc = value.mul_add(unit, acc);
    }
    acc
}

const fn splitmix64(seed: u64) -> u64 {
    let seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Interns an anchor value into a stable discrete class index. Vector anchors
/// are not discrete labels and are skipped.
fn discrete_anchor_label(
    value: &AnchorValue,
    interner: &mut BTreeMap<String, usize>,
) -> Option<usize> {
    let token = match value {
        AnchorValue::Bool(flag) => format!("bool:{flag}"),
        AnchorValue::Enum(name) => format!("enum:{name}"),
        AnchorValue::Text(text) => format!("text:{text}"),
        AnchorValue::OneHot(values) => format!("onehot:{}", values.join("|")),
        AnchorValue::Number(number) => format!("num:{number}"),
        AnchorValue::Vector(_) => return None,
    };
    let next = interner.len();
    Some(*interner.entry(token).or_insert(next))
}

/// Returns the first grounded anchor of a given kind on a record.
fn anchor_of_kind<'a>(anchors: &'a [Anchor], kind: &AnchorKind) -> Option<&'a Anchor> {
    anchors
        .iter()
        .find(|anchor| &anchor.kind == kind && anchor.confidence > 0.0)
}

/// Maps an operator-supplied anchor-kind string to a native `AnchorKind`.
/// Synapse writes outcome anchors as `Label(<name>)`, so an unrecognized name
/// becomes a `Label`.
fn parse_anchor_kind(raw: &str) -> AnchorKind {
    match raw.trim() {
        "test_pass" => AnchorKind::TestPass,
        "tie_formed" => AnchorKind::TieFormed,
        "thumbs" => AnchorKind::Thumbs,
        "reward" => AnchorKind::Reward,
        "speaker_match" => AnchorKind::SpeakerMatch,
        "style_hold" => AnchorKind::StyleHold,
        "recurrence" => AnchorKind::Recurrence,
        other => AnchorKind::Label(other.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Temporal intelligence (#1673)
//
// Turn correlation into arrows and cadence into confidence over the panel's
// source-event-time series:
//   * causality  — KSG transfer entropy with a lag sweep between two activity
//                  streams (which app/agent/tool activity drives which).
//   * periodicity — Lomb-Scargle GLS periodogram with permutation false-alarm
//                  probability plus a slotted autocorrelation cross-check.
//   * drift      — Page two-sided CUSUM rate change-points plus an MMD two-
//                  sample drift test over the activity-count distribution.
//   * hazard     — Gamma-renewal inter-event overdue hazard ("overdue" anomaly).
//
// Every estimator is wired from the calyx-assay substrate (no reimplementation).
// Derived rows persist to the native TemporalXTerm CF (periodicity/drift/hazard)
// and the Graph CF (directed causality edge) and are read back so every returned
// count is proven against the bytes, matching the Loom/Assay persistence idioms
// above. Series are read from the physical Base CF `source_event_time_secs`
// (inactive temporal lanes are suppressed, never storage-time-substituted).
// ---------------------------------------------------------------------------

/// Minimum event occurrences before a temporal estimator runs rather than
/// failing closed. Below this a series cannot carry a trusted temporal statistic.
pub const SYNAPSE_TEMPORAL_MIN_EVENTS: usize = 8;
/// Default occurrence-count bin width (seconds) for the Lomb-Scargle and
/// transfer-entropy binned series: one hour, matching operator-cadence scales.
pub const SYNAPSE_TEMPORAL_DEFAULT_BIN_SECS: f64 = 3_600.0;
/// Default maximum lag (in bins) swept by the transfer-entropy estimator.
pub const SYNAPSE_TEMPORAL_DEFAULT_MAX_LAG: usize = 8;
/// Hard cap on the number of lags in one transfer-entropy sweep. The sweep is
/// contiguous (`1..=max_lag`) because the delay it has to find is an unknown
/// integer number of bins; a sparse power-of-two lag set silently cannot report
/// a true lag of, say, 3. Bounding the count keeps the bootstrap cost finite.
pub const SYNAPSE_TEMPORAL_MAX_LAGS: usize = 32;
/// Cap on reported periodogram peaks.
pub const SYNAPSE_TEMPORAL_MAX_PEAKS: usize = 4;
/// Hard cap on the aligned transfer-entropy timeline. This admits more than 29
/// years of hourly bins while preventing a sparse or hostile timestamp span
/// from turning one request into an unbounded allocation.
pub const SYNAPSE_TEMPORAL_MAX_BINS: usize = 262_144;

const MAX_EXACT_I64_IN_F64: u64 = 1_u64 << f64::MANTISSA_DIGITS;

const GRAPH_CAUSALITY_PREFIX: &[u8; 5] = b"GTE01";
const TEMPORAL_PERIODICITY_PREFIX: &[u8; 5] = b"TPER1";
const TEMPORAL_DRIFT_PREFIX: &[u8; 5] = b"TDRF1";
const TEMPORAL_HAZARD_PREFIX: &[u8; 5] = b"THAZ1";

/// Bounded request describing one temporal-intelligence pass over a panel.
#[derive(Clone, Debug)]
pub struct SynapseCalyxTemporalParams {
    pub panel_version: u32,
    pub max_records: usize,
    /// Metadata key that partitions the panel into activity streams (e.g. the
    /// app/agent/tool identifier). Required for causality; optional filter for
    /// periodicity/drift/hazard.
    pub group_key: Option<String>,
    /// Causality source-stream value under `group_key` (defaults to the most
    /// frequent stream when absent).
    pub group_a: Option<String>,
    /// Causality target-stream value under `group_key` (defaults to the second
    /// most frequent stream when absent).
    pub group_b: Option<String>,
    /// Restricts periodicity/drift/hazard to one `group_key` stream value.
    pub filter_value: Option<String>,
    /// Occurrence-count bin width in seconds.
    pub bin_seconds: f64,
    /// Maximum transfer-entropy lag (bins) in the sweep.
    pub max_lag: usize,
    /// Reference "now" (Unix seconds) for the overdue-hazard elapsed time.
    pub now_secs: Option<i64>,
    /// Survival threshold below which the next occurrence is overdue.
    pub overdue_alpha: f64,
}

impl SynapseCalyxTemporalParams {
    #[must_use]
    pub const fn new(panel_version: u32) -> Self {
        Self {
            panel_version,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            group_key: None,
            group_a: None,
            group_b: None,
            filter_value: None,
            bin_seconds: SYNAPSE_TEMPORAL_DEFAULT_BIN_SECS,
            max_lag: SYNAPSE_TEMPORAL_DEFAULT_MAX_LAG,
            now_secs: None,
            overdue_alpha: calyx_assay::DEFAULT_OVERDUE_ALPHA,
        }
    }
}

/// One transfer-entropy lag result surfaced from the sweep.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalityLag {
    pub lag: usize,
    pub t_a_to_b: f32,
    pub t_b_to_a: f32,
    pub difference_ci_low: f32,
    pub difference_ci_high: f32,
    pub direction: String,
    pub n_samples: usize,
    pub provisional: bool,
    /// The estimator this lag ran (`discrete_plugin` / `continuous_ksg`), or
    /// `unresolved` when the lag failed before one was applied.
    pub estimator: String,
    /// The concrete `CALYX_*` failure code for this lag, when it failed.
    pub error_code: Option<String>,
}

/// Directed transfer-entropy result between two activity streams with the
/// physical Graph CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalityReport {
    pub panel_version: u32,
    pub group_key: String,
    pub group_a: String,
    pub group_b: String,
    pub bin_seconds: f64,
    pub n_bins: usize,
    pub events_a: usize,
    pub events_b: usize,
    pub best_lag: usize,
    pub t_a_to_b: f32,
    pub t_b_to_a: f32,
    pub difference_ci_low: f32,
    pub difference_ci_high: f32,
    pub dominant_direction: String,
    pub grounded: bool,
    /// The transfer-entropy estimator behind `t_a_to_b` / `t_b_to_a`.
    pub estimator: String,
    /// Why that estimator was the one used. Never a silent choice.
    pub estimator_reason: String,
    pub lags: Vec<SynapseCalyxCausalityLag>,
    pub graph_cf_rows_after: usize,
}

/// One periodogram peak surfaced with its permutation false-alarm probability.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxPeriodogramPeak {
    pub period_seconds: f64,
    pub frequency: f64,
    pub power: f64,
    pub false_alarm_probability: f64,
}

/// Lomb-Scargle periodicity result plus slotted-autocorrelation cross-check,
/// with the physical `TemporalXTerm` CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxPeriodicityReport {
    pub panel_version: u32,
    pub filter_value: Option<String>,
    pub bin_seconds: f64,
    pub n_samples: usize,
    pub time_span_seconds: f64,
    pub dominant_period_seconds: Option<f64>,
    pub dominant_power: Option<f64>,
    pub dominant_false_alarm_probability: Option<f64>,
    pub significant: bool,
    pub peaks: Vec<SynapseCalyxPeriodogramPeak>,
    pub acf_dominant_period_seconds: Option<f64>,
    pub temporal_xterm_cf_rows_after: usize,
}

/// CUSUM + MMD drift result with the physical `TemporalXTerm` CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxDriftReport {
    pub panel_version: u32,
    pub filter_value: Option<String>,
    /// Occurrences read from the panel before the tie collapse (#1893).
    pub n_occurrences: usize,
    /// Distinct instants the CUSUM gap series was built over.
    pub n_distinct_instants: usize,
    /// Occurrences absorbed into an earlier simultaneous instant.
    pub ties_collapsed: usize,
    /// Largest number of occurrences sharing one instant.
    pub max_multiplicity: usize,
    pub n_gaps: usize,
    pub baseline_mean_gap: f64,
    pub baseline_sigma: f64,
    pub cusum_change_detected: bool,
    pub cusum_change_index: Option<usize>,
    pub cusum_change_time_seconds: Option<f64>,
    pub cusum_direction: Option<String>,
    pub cusum_statistic: Option<f64>,
    pub mmd_split_index: Option<usize>,
    pub mmd_p_value: Option<f64>,
    pub mmd_significant: Option<bool>,
    pub temporal_xterm_cf_rows_after: usize,
}

/// Gamma-renewal overdue-hazard result with the physical `TemporalXTerm` CF
/// readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxHazardReport {
    pub panel_version: u32,
    pub filter_value: Option<String>,
    /// Occurrences read from the panel before the tie collapse (#1893).
    pub n_occurrences: usize,
    /// Distinct instants the renewal gap series was built over.
    pub n_distinct_instants: usize,
    /// Occurrences absorbed into an earlier simultaneous instant.
    pub ties_collapsed: usize,
    /// Largest number of occurrences sharing one instant.
    pub max_multiplicity: usize,
    pub n_gaps: usize,
    pub mean_gap_seconds: f64,
    pub coefficient_of_variation: f64,
    pub deterministic: bool,
    pub elapsed_seconds: f64,
    pub survival: f64,
    pub hazard: f64,
    pub empirical_survival: f64,
    pub expected_next_seconds: f64,
    pub overdue_threshold_seconds: f64,
    pub alpha: f64,
    pub overdue: bool,
    pub temporal_xterm_cf_rows_after: usize,
}

/// One panel record reduced to its source event time and stream group.
struct EventRecord {
    secs: f64,
    group: Option<String>,
}

/// Nanoseconds per second, for reconstructing sub-second event times.
const NS_PER_SEC: u64 = 1_000_000_000;
/// `NS_PER_SEC` as an exactly-representable `f64` divisor.
const NS_PER_SEC_F64: f64 = 1.0e9;

/// Recovers the full-precision source event time in **fractional seconds** from
/// a Base row's nanosecond stamp, cross-checked against its whole-second stamp.
///
/// `activate_temporal_lane` writes `source_event_time_raw` (integer nanoseconds)
/// and `source_event_time_secs` (`raw / 1e9`) in the same call, so on any row
/// this daemon wrote the two agree by construction. Reading only the truncated
/// seconds quantised the occurrence series onto a 1-second grid, manufacturing
/// ties that the source does not contain (issue #1893).
///
/// The integer and fractional parts are summed in the *seconds* domain rather
/// than dividing `ns as f64`, so the magnitude fed to `f64` stays inside the
/// exactly-representable integer range that
/// `SYNAPSE_CALYX_TEMPORAL_TIMESTAMP_OUT_OF_RANGE` already guards. The resulting
/// resolution near 2026 epochs is ~2.4e-7 s — three orders of magnitude finer
/// than the millisecond stamps Calyx records, so distinct milliseconds stay
/// distinguishable.
///
/// A row whose temporal lane is active but whose raw stamp is absent,
/// unparseable, or inconsistent with its own seconds stamp is an integrity
/// defect. It is reported, not worked around: silently using the lossy stamp
/// would reintroduce exactly the manufactured ties this exists to remove.
fn sub_second_event_secs(
    constellation: &Constellation,
    whole_secs_i64: i64,
    whole_secs: f64,
) -> Result<f64, SynapseCalyxError> {
    let raw = constellation.metadata_value(METADATA_SOURCE_EVENT_TIME_RAW);
    let Some(raw) = raw else {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_RAW_TIMESTAMP_ABSENT",
            "a Base row has an active temporal lane and a source_event_time_secs stamp but no \
             source_event_time_raw nanosecond stamp; activate_temporal_lane writes both together, \
             so one without the other means the row was written by an unknown path or was mutated",
            "inspect the Base row (storage operation=anchors with its source cf_name/key_hex) and \
             repair or re-derive its temporal metadata; do not infer nanoseconds from the seconds \
             stamp, which would restore the 1-second quantisation that issue #1893 removed",
        ));
    };
    let Ok(nanos) = raw.parse::<u64>() else {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_RAW_TIMESTAMP_NOT_NANOS",
            "a Base row's source_event_time_raw is not an integer nanosecond count, so the \
             full-precision occurrence time cannot be recovered from it",
            "re-derive the row's temporal metadata through activate_temporal_lane, which writes \
             source_event_time_raw as integer nanoseconds alongside its whole-second truncation",
        ));
    };
    let raw_secs = i64::try_from(nanos / NS_PER_SEC).map_err(|_| {
        temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_TIMESTAMP_OUT_OF_RANGE",
            "a source event nanosecond stamp exceeds the representable second range",
            "repair the source timestamp to a valid Unix-nanosecond value",
        )
    })?;
    if raw_secs != whole_secs_i64 {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_TIMESTAMP_STAMPS_DISAGREE",
            "a Base row's source_event_time_raw nanosecond stamp does not truncate to its own \
             source_event_time_secs stamp; the two are written together and must agree, so one of \
             them has been corrupted or rewritten",
            "inspect the Base row (storage operation=anchors with its source cf_name/key_hex), \
             establish which stamp matches the source row, and re-derive its temporal metadata",
        ));
    }
    // The remainder is strictly below one second, so it fits `u32` and converts
    // to `f64` exactly; the divisor is a literal power of ten. Neither step loses
    // a bit — which is the whole point of summing in the seconds domain instead
    // of dividing `nanos as f64`, where a 2026 epoch already exceeds the
    // exactly-representable integer range.
    let sub_second_nanos = u32::try_from(nanos % NS_PER_SEC).map_err(|_| {
        temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_TIMESTAMP_CONVERSION_FAILED",
            "a nanosecond remainder modulo one second did not fit u32, which is arithmetically \
             impossible; the timestamp arithmetic itself is wrong",
            "inspect the persisted Base row's source_event_time_raw value",
        )
    })?;
    Ok(whole_secs + f64::from(sub_second_nanos) / NS_PER_SEC_F64)
}

impl SynapseCalyxVault {
    /// Measures directed transfer entropy (KSG, lag sweep) between two activity
    /// streams partitioned by a metadata `group_key`, persists the dominant
    /// directed edge to the native Graph CF, and reads the Graph CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured error when `group_key` is absent, either stream is
    /// empty, the Base CF cannot be scanned, a constellation fails to decode, or
    /// the Graph write/readback fails.
    pub fn temporal_causality(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> Result<SynapseCalyxCausalityReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("temporal_causality");
        let group_key = params
            .group_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        let group_key = group_key.ok_or_else(|| {
            temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_GROUP_KEY_REQUIRED",
                "transfer-entropy causality requires a group_key partitioning the panel into activity streams",
                "supply group_key (the app/agent/tool metadata field) so two directed streams can be built",
            )
        })?;
        let records = self.load_panel_event_records(params, Some(group_key))?;
        let mut by_group: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for record in &records {
            if let Some(group) = &record.group {
                by_group.entry(group.clone()).or_default().push(record.secs);
            }
        }
        let (a_val, b_val) = choose_causality_streams(params, &by_group)?;
        let a_times = by_group.get(&a_val).cloned().unwrap_or_default();
        let b_times = by_group.get(&b_val).cloned().unwrap_or_default();
        if a_times.len() < SYNAPSE_TEMPORAL_MIN_EVENTS
            || b_times.len() < SYNAPSE_TEMPORAL_MIN_EVENTS
        {
            return Err(temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS",
                "one or both activity streams have fewer than the minimum occurrences for transfer entropy",
                "widen the record window or pick two active streams (>= 8 occurrences each) under group_key",
            ));
        }

        let bin = validated_bin_seconds(params.bin_seconds)?;
        let (stream_a, stream_b) = paired_binned_streams(&a_times, &b_times, bin)?;
        let n_bins = stream_a.len();
        let lags: Vec<usize> = (1..=params.max_lag.clamp(1, SYNAPSE_TEMPORAL_MAX_LAGS)).collect();
        let clock = SystemClock;
        let results = transfer_entropy_sweep(&stream_a, &stream_b, &lags, &clock);
        let best = choose_dominant_te(&results)?;
        let estimator = estimator_label(best.estimator);
        let estimator_reason = best.estimator_reason.clone();

        let dominant_direction = direction_label(best.dominant_direction);
        let grounded = !best.provisional;
        let key = causality_edge_key(params.panel_version, &a_val, &b_val);
        let out_edge = serde_json::json!({
            "panel_version": params.panel_version,
            "group_key": group_key,
            "group_a": a_val,
            "group_b": b_val,
            "bin_seconds": bin,
            "best_lag": best.lag,
            "t_a_to_b": best.t_a_to_b,
            "t_b_to_a": best.t_b_to_a,
            "dominant_direction": dominant_direction,
            "provisional": best.provisional,
            "estimator": estimator,
        });
        self.persist_temporal_row(ColumnFamily::Graph, key, &out_edge)?;
        let graph_cf_rows_after = self
            .count_cf_latest_bounded_memoized(ColumnFamily::Graph)?
            .rows();

        Ok(SynapseCalyxCausalityReport {
            panel_version: params.panel_version,
            group_key: group_key.to_owned(),
            group_a: a_val,
            group_b: b_val,
            bin_seconds: bin,
            n_bins,
            events_a: a_times.len(),
            events_b: b_times.len(),
            best_lag: best.lag,
            t_a_to_b: best.t_a_to_b,
            t_b_to_a: best.t_b_to_a,
            difference_ci_low: best.difference_ci_95.0,
            difference_ci_high: best.difference_ci_95.1,
            dominant_direction,
            grounded,
            estimator,
            estimator_reason,
            lags: results.iter().map(causality_lag).collect(),
            graph_cf_rows_after,
        })
    }

    /// Runs the Lomb-Scargle GLS periodogram (with permutation false-alarm
    /// probability) and a slotted-autocorrelation cross-check over the panel's
    /// occurrence-count series, persists the result to the native `TemporalXTerm`
    /// CF, and reads it back.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the series is too short, the estimator
    /// fails closed, or the CF write/readback fails.
    pub fn temporal_periodicity(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> Result<SynapseCalyxPeriodicityReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("temporal_periodicity");
        let times = self.filtered_event_times(params)?;
        let bin = validated_bin_seconds(params.bin_seconds)?;
        let (centers, counts) = bin_event_counts(&times, bin)
            .map_err(|error| loom_math_error("bin occurrence counts", &error))?;
        let config = PeriodogramConfig {
            max_peaks: SYNAPSE_TEMPORAL_MAX_PEAKS,
            ..PeriodogramConfig::default()
        };
        let report = lomb_scargle_with_config(&centers, &counts, &config)
            .map_err(|error| loom_math_error("Lomb-Scargle periodogram", &error))?;
        let acf_dominant = autocorrelation(&centers, &counts)
            .ok()
            .and_then(|acf| acf.dominant_period);

        let dominant = report.dominant().copied();
        let significant = !report.significant_peaks(SIGNIFICANT_PEAK_FAP).is_empty();
        let peaks: Vec<SynapseCalyxPeriodogramPeak> = report
            .peaks
            .iter()
            .map(|peak| SynapseCalyxPeriodogramPeak {
                period_seconds: peak.period,
                frequency: peak.frequency,
                power: peak.power,
                false_alarm_probability: peak.false_alarm_probability,
            })
            .collect();

        let out = serde_json::json!({
            "panel_version": params.panel_version,
            "filter_value": params.filter_value,
            "bin_seconds": bin,
            "n_samples": report.n_samples,
            "time_span_seconds": report.time_span,
            "dominant_period_seconds": dominant.map(|peak| peak.period),
            "dominant_false_alarm_probability": dominant.map(|peak| peak.false_alarm_probability),
            "significant": significant,
            "acf_dominant_period_seconds": acf_dominant,
        });
        let key = temporal_report_key(
            *TEMPORAL_PERIODICITY_PREFIX,
            params.panel_version,
            params.filter_value.as_deref(),
        );
        self.persist_temporal_row(ColumnFamily::TemporalXTerm, key, &out)?;
        let temporal_xterm_cf_rows_after = self
            .count_cf_latest_bounded(ColumnFamily::TemporalXTerm)?
            .rows_visited;

        Ok(SynapseCalyxPeriodicityReport {
            panel_version: params.panel_version,
            filter_value: params.filter_value.clone(),
            bin_seconds: bin,
            n_samples: report.n_samples,
            time_span_seconds: report.time_span,
            dominant_period_seconds: dominant.map(|peak| peak.period),
            dominant_power: dominant.map(|peak| peak.power),
            dominant_false_alarm_probability: dominant.map(|peak| peak.false_alarm_probability),
            significant,
            peaks,
            acf_dominant_period_seconds: acf_dominant,
            temporal_xterm_cf_rows_after,
        })
    }

    /// Detects recurrence-rate change (Page two-sided CUSUM over the gap series)
    /// and distribution drift (MMD two-sample over the activity-count series),
    /// persists the result to the native `TemporalXTerm` CF, and reads it back.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the gap series is too short, an estimator
    /// fails closed, or the CF write/readback fails.
    pub fn temporal_drift(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> Result<SynapseCalyxDriftReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("temporal_drift");
        // The CUSUM runs over gaps between distinct instants; the MMD runs over
        // binned counts, where multiplicity is signal, so it keeps every
        // occurrence (issue #1893).
        let collapse = self.distinct_event_instants(params)?;
        let times = &collapse.instants;
        let cusum: CusumReport = recurrence_rate_cusum(times)
            .map_err(|error| loom_math_error("CUSUM rate change-point", &error))?;

        let bin = validated_bin_seconds(params.bin_seconds)?;
        let all_times = self.filtered_event_times(params)?;
        let mmd = match bin_event_counts(&all_times, bin) {
            Ok((_, counts)) => temporal_mmd_change_point(&counts),
            Err(_) => None,
        };

        let change = cusum.change_point;
        let out = serde_json::json!({
            "panel_version": params.panel_version,
            "filter_value": params.filter_value,
            "n_occurrences": collapse.n_occurrences,
            "n_distinct_instants": collapse.n_distinct_instants,
            "ties_collapsed": collapse.ties_collapsed,
            "max_multiplicity": collapse.max_multiplicity,
            "n_gaps": cusum.n_gaps,
            "baseline_mean_gap": cusum.baseline_mean_gap,
            "baseline_sigma": cusum.baseline_sigma,
            "cusum_change_detected": change.is_some(),
            "cusum_change_index": change.map(|point| point.occurrence_index),
            "cusum_direction": change.map(|point| rate_shift_label(point.direction)),
            "mmd_p_value": mmd.as_ref().map(|report| report.report.p_value),
        });
        let key = temporal_report_key(
            *TEMPORAL_DRIFT_PREFIX,
            params.panel_version,
            params.filter_value.as_deref(),
        );
        self.persist_temporal_row(ColumnFamily::TemporalXTerm, key, &out)?;
        let temporal_xterm_cf_rows_after = self
            .count_cf_latest_bounded(ColumnFamily::TemporalXTerm)?
            .rows_visited;

        Ok(SynapseCalyxDriftReport {
            panel_version: params.panel_version,
            filter_value: params.filter_value.clone(),
            n_occurrences: collapse.n_occurrences,
            n_distinct_instants: collapse.n_distinct_instants,
            ties_collapsed: collapse.ties_collapsed,
            max_multiplicity: collapse.max_multiplicity,
            n_gaps: cusum.n_gaps,
            baseline_mean_gap: cusum.baseline_mean_gap,
            baseline_sigma: cusum.baseline_sigma,
            cusum_change_detected: change.is_some(),
            cusum_change_index: change.map(|point| point.occurrence_index),
            cusum_change_time_seconds: change.map(|point| point.change_time),
            cusum_direction: change.map(|point| rate_shift_label(point.direction)),
            cusum_statistic: change.map(|point| point.statistic),
            mmd_split_index: mmd.as_ref().map(|report| report.split_index),
            mmd_p_value: mmd.as_ref().map(|report| report.report.p_value),
            mmd_significant: mmd.as_ref().map(|report| report.report.significant),
            temporal_xterm_cf_rows_after,
        })
    }

    /// Fits the Gamma-renewal inter-event hazard and evaluates the overdue
    /// survival at `now`, persists the result to the native `TemporalXTerm` CF, and
    /// reads it back.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the occurrence series is too short, the
    /// estimator fails closed, or the CF write/readback fails.
    pub fn temporal_hazard(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> Result<SynapseCalyxHazardReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("temporal_hazard");
        // The Gamma renewal fit is over gaps between distinct instants; tied
        // occurrences carry no gap and would make the fit undefined (#1893).
        let collapse = self.distinct_event_instants(params)?;
        let times = &collapse.instants;
        let last = *times.last().ok_or_else(|| {
            temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS",
                "the occurrence series is empty; overdue hazard needs a recent occurrence",
                "capture more occurrences for this stream before requesting overdue hazard",
            )
        })?;
        let now = resolve_now_secs(params.now_secs, self.clock_now_ms().ok(), last);
        let report: InterEventHazardReport =
            inter_event_hazard_with_alpha(times, now, params.overdue_alpha)
                .map_err(|error| loom_math_error("inter-event overdue hazard", &error))?;

        let out = serde_json::json!({
            "panel_version": params.panel_version,
            "filter_value": params.filter_value,
            "n_occurrences": collapse.n_occurrences,
            "n_distinct_instants": collapse.n_distinct_instants,
            "ties_collapsed": collapse.ties_collapsed,
            "max_multiplicity": collapse.max_multiplicity,
            "n_gaps": report.n_gaps,
            "mean_gap_seconds": report.mean_gap,
            "coefficient_of_variation": report.coefficient_of_variation,
            "elapsed_seconds": report.elapsed,
            "survival": report.survival,
            "expected_next_seconds": report.expected_next,
            "overdue": report.overdue,
            "alpha": report.alpha,
        });
        let key = temporal_report_key(
            *TEMPORAL_HAZARD_PREFIX,
            params.panel_version,
            params.filter_value.as_deref(),
        );
        self.persist_temporal_row(ColumnFamily::TemporalXTerm, key, &out)?;
        let temporal_xterm_cf_rows_after = self
            .count_cf_latest_bounded(ColumnFamily::TemporalXTerm)?
            .rows_visited;

        Ok(SynapseCalyxHazardReport {
            panel_version: params.panel_version,
            filter_value: params.filter_value.clone(),
            n_occurrences: collapse.n_occurrences,
            n_distinct_instants: collapse.n_distinct_instants,
            ties_collapsed: collapse.ties_collapsed,
            max_multiplicity: collapse.max_multiplicity,
            n_gaps: report.n_gaps,
            mean_gap_seconds: report.mean_gap,
            coefficient_of_variation: report.coefficient_of_variation,
            deterministic: report.deterministic,
            elapsed_seconds: report.elapsed,
            survival: report.survival,
            hazard: report.hazard,
            empirical_survival: report.empirical_survival,
            expected_next_seconds: report.expected_next,
            overdue_threshold_seconds: report.overdue_threshold_secs,
            alpha: report.alpha,
            overdue: report.overdue,
            temporal_xterm_cf_rows_after,
        })
    }

    /// Loads the panel's ascending source-event-time series (seconds) with the
    /// optional stream group. Records without an active source event time are
    /// suppressed (never storage-time-substituted).
    fn load_panel_event_records(
        &self,
        params: &SynapseCalyxTemporalParams,
        group_key: Option<&str>,
    ) -> Result<Vec<EventRecord>, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let mut records = Vec::new();
        // The panel-scoped membership sidecar is the selective access path.
        // `max_records` now bounds point reads within this panel instead of
        // decoding unrelated rows from the global CxId-ordered Base keyspace.
        self.with_panel_read_snapshot(
            params.panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| self.walk_panel_base_snapshot(snapshot, params.panel_version, |_snapshot, _key, value| {
                let constellation = decode_constellation_base(value).map_err(|error| {
                    SynapseCalyxError::from_calyx("decode Base constellation", &error)
                })?;
                if constellation.panel_version != params.panel_version {
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                }
                let Some(secs) = constellation.source_event_time_secs() else {
                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                };
                if secs.unsigned_abs() > MAX_EXACT_I64_IN_F64 {
                    return Err(temporal_error(
                        "SYNAPSE_CALYX_TEMPORAL_TIMESTAMP_OUT_OF_RANGE",
                        "a source event timestamp exceeds the exactly representable f64 integer range",
                        "repair the source timestamp to a valid Unix-second value within +/- 2^53",
                    ));
                }
                let whole_secs = secs.to_f64().ok_or_else(|| {
                    temporal_error(
                        "SYNAPSE_CALYX_TEMPORAL_TIMESTAMP_CONVERSION_FAILED",
                        "a validated source event timestamp could not be converted to f64",
                        "inspect the persisted Base row and repair its source event timestamp",
                    )
                })?;
                // `activate_temporal_lane` writes the nanosecond stamp and its
                // whole-second truncation together, so the sub-second part is
                // already on this row. Reading only the truncation quantised every
                // occurrence to a 1-second grid and manufactured ties that do not
                // exist in the source (issue #1893). Prefer the full-precision
                // stamp, and fail loudly rather than silently using the lossy one
                // when the two disagree — that is a Base-row integrity defect.
                let secs = sub_second_event_secs(&constellation, secs, whole_secs)?;
                let group =
                    group_key.and_then(|key| constellation.metadata_value(key).map(str::to_owned));
                records.push(EventRecord { secs, group });
                // The pre-paging form `break`s here, after the push, so the walk
                // stops on the same row with the same records loaded.
                if records.len() >= max_records {
                    return Ok(crate::SynapseCalyxWalkStep::Stop);
                }
                Ok(crate::SynapseCalyxWalkStep::Continue)
            }),
        )?;
        records.sort_by(|a, b| a.secs.total_cmp(&b.secs));
        Ok(records)
    }

    /// Ascending occurrence-time series (seconds) for periodicity/drift/hazard,
    /// optionally restricted to one `group_key` stream value.
    fn filtered_event_times(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> Result<Vec<f64>, SynapseCalyxError> {
        let group_key = params
            .group_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        let records = self.load_panel_event_records(params, group_key)?;
        let times: Vec<f64> = records
            .iter()
            .filter(|record| match (group_key, &params.filter_value) {
                (Some(_), Some(value)) => record.group.as_deref() == Some(value.as_str()),
                _ => true,
            })
            .map(|record| record.secs)
            .collect();
        if times.len() < SYNAPSE_TEMPORAL_MIN_EVENTS {
            return Err(temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS",
                "the filtered occurrence series has fewer than the minimum occurrences for a temporal estimate",
                "widen the record window, drop the filter, or capture more occurrences (>= 8) for this stream",
            ));
        }
        Ok(times)
    }

    /// Ascending series of **distinct** occurrence instants for the renewal and
    /// CUSUM estimators, with the tie collapse that produced it.
    ///
    /// `periodicity` and `causality` bin their occurrences, so multiplicity is
    /// signal there and they keep the full series. `drift` and `hazard` model
    /// inter-occurrence gaps, which are only defined between distinct instants —
    /// an orderly point process admits no tied times. Synapse genuinely writes
    /// several agent events at one identical nanosecond (they share a commit,
    /// which is why the `CF_AGENT_EVENTS` key carries a `seq` disambiguator), so
    /// the collapse is the normal path, not an exceptional one (issue #1893).
    fn distinct_event_instants(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> Result<TiedOccurrenceCollapse, SynapseCalyxError> {
        let times = self.filtered_event_times(params)?;
        let collapse = collapse_tied_occurrences(&times)
            .map_err(|error| loom_math_error("collapse tied occurrence instants", &error))?;
        if collapse.n_distinct_instants < SYNAPSE_TEMPORAL_MIN_EVENTS {
            return Err(temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_DISTINCT_INSTANTS",
                "the occurrence series has enough occurrences but too few DISTINCT instants for a \
                 gap-based estimate: inter-occurrence gaps exist only between distinct instants, \
                 and after collapsing simultaneous occurrences fewer than the minimum remain",
                "this is not a total-count shortfall — the occurrences are concentrated at a few \
                 instants. Widen the record window, drop the filter, or capture occurrences spread \
                 over more distinct instants (>= 8) for this stream",
            ));
        }
        Ok(collapse)
    }

    /// Persists one JSON temporal report row to a native CF and flushes it.
    fn persist_temporal_row(
        &self,
        cf: ColumnFamily,
        key: Vec<u8>,
        value: &serde_json::Value,
    ) -> Result<(), SynapseCalyxError> {
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf,
            key,
            value: encode_json(value)?,
        }])?;
        self.flush()
    }
}

/// Chooses the two causality streams: explicit `group_a`/`group_b` when given,
/// else the two most frequent streams under the group key.
fn choose_causality_streams(
    params: &SynapseCalyxTemporalParams,
    by_group: &BTreeMap<String, Vec<f64>>,
) -> Result<(String, String), SynapseCalyxError> {
    if let (Some(a), Some(b)) = (&params.group_a, &params.group_b) {
        if a == b {
            return Err(temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_STREAMS_IDENTICAL",
                "group_a and group_b name the same stream; transfer entropy needs two distinct streams",
                "pick two distinct group_key values for the directed causality test",
            ));
        }
        return Ok((a.clone(), b.clone()));
    }
    let mut ranked: Vec<(&String, usize)> = by_group
        .iter()
        .map(|(name, times)| (name, times.len()))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    if ranked.len() < 2 {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_STREAMS_INSUFFICIENT",
            "fewer than two activity streams exist under group_key; transfer entropy needs a lead/lag pair",
            "supply group_a/group_b explicitly or capture activity for at least two distinct streams",
        ));
    }
    Ok((ranked[0].0.clone(), ranked[1].0.clone()))
}

/// One activity stream as consecutive `(bin_index, event_count)` samples.
type BinnedStream = Vec<(u64, f32)>;

/// Builds two aligned integer-bin count streams over the union time range so the
/// transfer-entropy lag operates on consecutive bins (value 0 bins are kept so
/// the estimator's history lookups never straddle a gap).
fn paired_binned_streams(
    a_times: &[f64],
    b_times: &[f64],
    bin: f64,
) -> Result<(BinnedStream, BinnedStream), SynapseCalyxError> {
    let min_t = a_times
        .iter()
        .chain(b_times)
        .copied()
        .fold(f64::INFINITY, f64::min);
    let max_t = a_times
        .iter()
        .chain(b_times)
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if !min_t.is_finite() || !max_t.is_finite() || max_t < min_t {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_TIME_RANGE_INVALID",
            "the transfer-entropy time range is non-finite or reversed",
            "inspect the persisted source event timestamps and repair invalid values",
        ));
    }
    let max_index = checked_temporal_bin_index((max_t - min_t) / bin)?;
    let n_bins = max_index.checked_add(1).ok_or_else(|| {
        temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_BIN_RANGE_OVERFLOW",
            "the transfer-entropy bin count overflowed usize",
            "increase bin_seconds or narrow the source event time window",
        )
    })?;
    if n_bins > SYNAPSE_TEMPORAL_MAX_BINS {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_BIN_RANGE_TOO_LARGE",
            "the transfer-entropy timeline exceeds the bounded bin limit",
            "increase bin_seconds or narrow the source event time window",
        ));
    }
    let mut a_counts = vec![0.0_f32; n_bins];
    let mut b_counts = vec![0.0_f32; n_bins];
    for &time in a_times {
        let index = checked_temporal_bin_index((time - min_t) / bin)?;
        if index >= n_bins {
            return Err(temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_BIN_INDEX_INVALID",
                "an event mapped outside the validated transfer-entropy timeline",
                "inspect the persisted source event timestamps and bin_seconds",
            ));
        }
        a_counts[index] += 1.0;
    }
    for &time in b_times {
        let index = checked_temporal_bin_index((time - min_t) / bin)?;
        if index >= n_bins {
            return Err(temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_BIN_INDEX_INVALID",
                "an event mapped outside the validated transfer-entropy timeline",
                "inspect the persisted source event timestamps and bin_seconds",
            ));
        }
        b_counts[index] += 1.0;
    }
    let stream_a = a_counts
        .into_iter()
        .enumerate()
        .map(|(index, count)| (index as u64, count))
        .collect();
    let stream_b = b_counts
        .into_iter()
        .enumerate()
        .map(|(index, count)| (index as u64, count))
        .collect();
    Ok((stream_a, stream_b))
}

fn checked_temporal_bin_index(value: f64) -> Result<usize, SynapseCalyxError> {
    if !value.is_finite() || value < 0.0 {
        return Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_BIN_INDEX_INVALID",
            "a transfer-entropy bin index is negative or non-finite",
            "inspect the persisted source event timestamps and bin_seconds",
        ));
    }
    value.floor().to_usize().ok_or_else(|| {
        temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_BIN_INDEX_OVERFLOW",
            "a transfer-entropy bin index does not fit usize",
            "increase bin_seconds or narrow the source event time window",
        )
    })
}

/// Picks the transfer-entropy lag with the largest absolute directed asymmetry
/// among the lags that actually produced an estimate.
///
/// When no lag produced one, the failure is *classified* and every lag's
/// `{lag, error_code, n_samples}` is carried into the message. The previous
/// version collapsed all of that into one `TE_UNRESOLVED` code whose remediation
/// ("increase the binned series length") was wrong for the failure that actually
/// happened — a degenerate KSG radius on integer counts, which more samples make
/// worse, not better (issue #1673).
fn choose_dominant_te(results: &[TEResult]) -> Result<TEResult, SynapseCalyxError> {
    if let Some(best) = results
        .iter()
        .filter(|result| !result.provisional && result.error_code.is_none())
        .max_by(|left, right| {
            (left.t_a_to_b - left.t_b_to_a)
                .abs()
                .total_cmp(&(right.t_a_to_b - right.t_b_to_a).abs())
        })
    {
        return Ok(best.clone());
    }
    let detail = per_lag_failure_detail(results);
    let estimator_failed = results.iter().any(|result| {
        result
            .error_code
            .as_deref()
            .is_some_and(|code| code != calyx_assay::CALYX_TE_INSUFFICIENT_SAMPLES)
    });
    if estimator_failed {
        Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_TEMPORAL_TE_ESTIMATOR_FAILED",
            format!("every transfer-entropy lag failed inside the estimator: {detail}"),
            "read the per-lag error_code: CALYX_TE_DISCRETE_STATE_QUORUM or CALYX_TE_DISCRETE_ALPHABET_TOO_LARGE means the count alphabet is too rich for the sample (widen bin_seconds), CALYX_ASSAY_DEGENERATE_INPUT means the continuous KSG estimator was applied to tied samples",
        ))
    } else {
        Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_TEMPORAL_TE_UNRESOLVED",
            format!("no transfer-entropy lag reached sample quorum: {detail}"),
            "increase the binned series length (more occurrences or a smaller bin) so a lag reaches quorum",
        ))
    }
}

/// Renders `{lag, error_code, n_samples}` for every lag tried, in sweep order.
fn per_lag_failure_detail(results: &[TEResult]) -> String {
    if results.is_empty() {
        return "no lags were tried".to_string();
    }
    results
        .iter()
        .map(|result| {
            format!(
                "{{lag={}, error_code={}, n_samples={}, estimator={}}}",
                result.lag,
                result.error_code.as_deref().unwrap_or("none"),
                result.n_samples,
                estimator_label(result.estimator),
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn estimator_label(estimator: Option<TeEstimator>) -> String {
    estimator.map_or_else(|| "unresolved".to_owned(), |kind| kind.as_str().to_owned())
}

/// Runs an MMD change-point over the 1-D activity-count series when it is long
/// enough for a two-sided split; returns `None` (never fabricates) otherwise.
fn temporal_mmd_change_point(counts: &[f64]) -> Option<ChangePointReport> {
    let samples: Vec<Vec<f64>> = counts.iter().map(|count| vec![*count]).collect();
    if samples.len() < calyx_assay::mmd::MIN_MMD_SAMPLES * 2 {
        return None;
    }
    mmd_change_point(
        &samples,
        calyx_assay::mmd::MIN_MMD_SAMPLES,
        &MmdConfig::default(),
    )
    .ok()
}

fn causality_lag(result: &TEResult) -> SynapseCalyxCausalityLag {
    SynapseCalyxCausalityLag {
        lag: result.lag,
        t_a_to_b: result.t_a_to_b,
        t_b_to_a: result.t_b_to_a,
        difference_ci_low: result.difference_ci_95.0,
        difference_ci_high: result.difference_ci_95.1,
        direction: direction_label(result.dominant_direction),
        n_samples: result.n_samples,
        provisional: result.provisional,
        estimator: estimator_label(result.estimator),
        error_code: result.error_code.clone(),
    }
}

fn direction_label(direction: Direction) -> String {
    match direction {
        Direction::AToB => "a_to_b".to_owned(),
        Direction::BToA => "b_to_a".to_owned(),
        Direction::Unclear => "unclear".to_owned(),
    }
}

fn rate_shift_label(shift: RateShift) -> String {
    match shift {
        RateShift::SpeedUp => "speed_up".to_owned(),
        RateShift::SlowDown => "slow_down".to_owned(),
    }
}

/// Chooses the overdue-hazard reference `now`: the explicit request, else the
/// vault clock, clamped forward to the last occurrence (elapsed must be >= 0).
#[allow(clippy::cast_precision_loss)]
fn resolve_now_secs(now_secs: Option<i64>, clock_ms: Option<Ts>, last: f64) -> f64 {
    let candidate = now_secs
        .map(|secs| secs as f64)
        .or_else(|| clock_ms.map(|ms| ms as f64 / 1_000.0))
        .unwrap_or(last);
    candidate.max(last)
}

fn validated_bin_seconds(bin_seconds: f64) -> Result<f64, SynapseCalyxError> {
    if bin_seconds.is_finite() && bin_seconds > 0.0 {
        Ok(bin_seconds)
    } else {
        Err(temporal_error(
            "SYNAPSE_CALYX_TEMPORAL_BIN_INVALID",
            "bin_seconds must be finite and positive",
            "supply a positive occurrence-count bin width in seconds",
        ))
    }
}

fn causality_edge_key(panel_version: u32, group_a: &str, group_b: &str) -> Vec<u8> {
    let mut key =
        Vec::with_capacity(GRAPH_CAUSALITY_PREFIX.len() + 5 + group_a.len() + group_b.len());
    key.extend_from_slice(GRAPH_CAUSALITY_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(group_a.as_bytes());
    key.push(0x00);
    key.extend_from_slice(group_b.as_bytes());
    key
}

fn temporal_report_key(prefix: [u8; 5], panel_version: u32, filter: Option<&str>) -> Vec<u8> {
    let filter = filter.unwrap_or("");
    let mut key = Vec::with_capacity(prefix.len() + 4 + filter.len());
    key.extend_from_slice(&prefix);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(filter.as_bytes());
    key
}

fn temporal_error(
    code: &'static str,
    message: &'static str,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message.to_owned(), remediation)
}

/// Builds the fail-closed refusal for a panel that contains the anchor it is
/// being adjudicated by (#1958 ask 3).
///
/// It names the slot, the lens, and the **shared field** — the last of which is
/// the part that makes the refusal actionable rather than an accusation. A
/// reader can go to the construction site, see that the lens reads that field,
/// see that the field determines the anchor, and either exclude the slot or
/// decide the declaration is wrong. Neither is possible from a bits number.
fn anchor_source_leakage_error(
    params: &SynapseCalyxAssayParams,
    provenance: &lens_provenance::AnchorSourceProvenance,
) -> SynapseCalyxError {
    let carriers = provenance
        .carriers
        .iter()
        .map(|carrier| {
            format!(
                "slot {} ({}) reads [{}]",
                carrier.slot,
                carrier.lens,
                carrier.shared_fields.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let slots = provenance
        .carriers
        .iter()
        .map(|carrier| carrier.slot.to_string())
        .collect::<Vec<_>>()
        .join(",");
    SynapseCalyxError::new(
        SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE,
        format!(
            "panel_version={} contains {} lens(es) whose declared source fields are among the \
             fields that determine anchor {} ([{}]), so any sufficiency verdict over this panel \
             would be measuring the anchor against itself: {}. Remediation set: \
             excluded_slots=[{}]",
            params.panel_version,
            provenance.carriers.len(),
            params.anchor_kind,
            provenance.anchor_fields.join(", "),
            carriers,
            slots
        ),
        "re-run with excluded_slots covering every named slot to measure the panel's genuinely \
         predictive lenses, or correct the declaration in synapse_calyx::lens_provenance if a \
         named lens does not in fact read that field",
    )
}

// ---------------------------------------------------------------------------
// Grounding kernel + grounded kernel_answer (#1675)
//
// Phase-5 "Compose": per-domain (per-panel) grounding kernel — the minimal
// generating core (~feedback-vertex-set on the embedding-proximity association
// graph, greedy on 0.40*degree + 0.40*betweenness + 0.20*groundedness), MEASURED
// against the corpus with the recall gate (~0.95). An ungrounded kernel is a
// structured error, never silently served. The kernel doubles as index and
// answer path (hop score = edge_weight * 0.9^hop): `kernel_answer` walks the
// kernel from the query record to its nearest anchored kernel node and returns
// the evidence path with hop scores. The honesty gate is load-bearing:
//   grounded (recall gate met AND an anchored path exists) => answer;
//   insufficient grounding => a structured refusal naming the gap, NEVER a
//   confabulated answer.
//
// Every estimator is wired from the calyx-lodestar substrate (kernel selection,
// recall measurement, answer derivation — no reimplementation). The selected
// kernel persists to the native Kernel CF with a corpus fingerprint and is read
// back so oracle (#1678)/ward (#1677)/hygiene can consume it. Answers load that
// exact persisted kernel and require its corpus fingerprint to match the current
// bounded vault corpus before deriving a grounded path, bounded and off-runtime.
// ---------------------------------------------------------------------------

/// Default k for the embedding-proximity association graph the kernel selects on.
pub const SYNAPSE_KERNEL_DEFAULT_KNN: usize = 8;
/// Default cosine floor for an association edge in the kernel graph.
pub const SYNAPSE_KERNEL_DEFAULT_EDGE_COS: f32 = 0.25;
/// Default kernel-only recall gate ratio (the epic's ~0.95).
pub const SYNAPSE_KERNEL_DEFAULT_MIN_RECALL: f32 = 0.95;
/// Default maximum hops walked from an anchored kernel node to the query.
pub const SYNAPSE_KERNEL_DEFAULT_MAX_HOPS: usize = 4;
/// Cap on member `cx_ids` echoed in a kernel report (the full set persists to CF).
pub const SYNAPSE_KERNEL_MAX_REPORTED_MEMBERS: usize = 256;

pub const KERNEL_ROW_PREFIX: &[u8; 5] = b"KERN1";

/// Bounded request describing one grounding-kernel build or grounded answer.
#[derive(Clone, Debug)]
pub struct SynapseCalyxKernelParams {
    pub panel_version: u32,
    /// Dense or sparse content-lens slot read per kernel concept.
    pub content_slot: u16,
    pub max_records: usize,
    pub knn: usize,
    pub edge_cos_threshold: f32,
    pub min_recall_ratio: f32,
    /// Physical math resource this kernel build is allowed to activate.
    /// Explicit MCP requests keep the configured backend; unattended
    /// maintenance selects `BackgroundCpu` so it cannot create a CUDA context
    /// behind a foreground game.
    pub math_execution_class: SynapseCalyxMathExecutionClass,
    /// Optional grounded outcome anchor kind. This is a real domain *scope*
    /// (calyx-lodestar `Scope::Domain { anchor_kind }`), not a cosmetic label:
    /// when set, only concepts anchored on that outcome axis count as anchors,
    /// so the selected kernel is the minimal generating core of that one domain
    /// rather than of every anchored concept in the panel. It is also stamped
    /// onto the kernel identity so the persisted artifact names its own scope.
    pub anchor_kind: Option<String>,
}

impl SynapseCalyxKernelParams {
    #[must_use]
    pub const fn new(panel_version: u32, content_slot: u16) -> Self {
        Self {
            panel_version,
            content_slot,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            knn: SYNAPSE_KERNEL_DEFAULT_KNN,
            edge_cos_threshold: SYNAPSE_KERNEL_DEFAULT_EDGE_COS,
            min_recall_ratio: SYNAPSE_KERNEL_DEFAULT_MIN_RECALL,
            math_execution_class: SynapseCalyxMathExecutionClass::Configured,
            anchor_kind: None,
        }
    }
}

/// Validates one kernel request without silently correcting any field.
///
/// # Errors
///
/// Returns `SYNAPSE_CALYX_KERNEL_PARAMS_INVALID` with all measured fields when
/// any bound, finite-number requirement, or domain-label invariant is violated.
pub fn validate_kernel_params(params: &SynapseCalyxKernelParams) -> Result<(), SynapseCalyxError> {
    let anchor_kind_valid = params
        .anchor_kind
        .as_deref()
        .is_none_or(|kind| !kind.trim().is_empty());
    if (1..=SYNAPSE_INTELLIGENCE_MAX_RECORDS).contains(&params.max_records)
        && (1..=64).contains(&params.knn)
        && params.edge_cos_threshold.is_finite()
        && (-1.0..=1.0).contains(&params.edge_cos_threshold)
        && params.min_recall_ratio.is_finite()
        && (0.0..=1.0).contains(&params.min_recall_ratio)
        && anchor_kind_valid
    {
        return Ok(());
    }
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_KERNEL_PARAMS_INVALID",
        format!(
            "kernel parameters are outside the serving contract: max_records={} (required 1..={SYNAPSE_INTELLIGENCE_MAX_RECORDS}), knn={} (required 1..=64), edge_cos_threshold={} (required finite -1..=1), min_recall_ratio={} (required finite 0..=1), anchor_kind_nonempty={anchor_kind_valid}",
            params.max_records, params.knn, params.edge_cos_threshold, params.min_recall_ratio
        ),
        "repair the exact invalid parameter; kernel operations never clamp, default, or reinterpret an invalid value",
    ))
}

/// A derived per-domain grounding kernel with its measured recall and the
/// physical Kernel CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxKernelReport {
    pub panel_version: u32,
    pub content_slot: u16,
    pub kernel_id: String,
    /// Semantic corpus fingerprint over sorted identities, exact vectors, and
    /// the sorted anchored set, proving what the kernel was selected against.
    pub corpus_fingerprint: String,
    pub members: usize,
    pub kernel_graph_nodes: usize,
    pub corpus_size: usize,
    pub vault_corpus_size: usize,
    pub recall_kernel_only: f32,
    pub recall_ratio: f32,
    pub min_recall_ratio: f32,
    pub grounded: bool,
    pub reached_anchor: f32,
    pub unanchored_members: usize,
    pub anchored_members: usize,
    pub member_cx_ids: Vec<String>,
    pub kernel_cf_rows_after: usize,
}

/// One hop on a grounded answer's evidence path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxKernelAnswerHop {
    pub from: String,
    pub to: String,
    pub edge_weight: f32,
    pub hop_index: u32,
    pub hop_score: f32,
}

/// A grounded kernel answer: the evidence path from the query record to its
/// nearest anchored kernel node, with hop scores and grounding tags.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxKernelAnswerReport {
    pub panel_version: u32,
    pub content_slot: u16,
    pub query_cx_id: String,
    pub grounded: bool,
    pub kernel_id: String,
    pub anchor_kernel_node: String,
    pub total_score: f32,
    pub hop_count: usize,
    pub hops: Vec<SynapseCalyxKernelAnswerHop>,
    pub kernel_members: usize,
    pub recall_ratio: f32,
    pub min_recall_ratio: f32,
}

/// Reusable kernel inputs assembled from native dense or sparse measurements.
pub struct DomainKernelInputs {
    rows: KernelContentRows,
    pub anchors: Vec<CxId>,
    graph: AssocGraph,
    pub kernel: Kernel,
    kernel_index: Option<KernelIndex>,
    pub recall_kernel_only: f32,
    pub recall_ratio: f32,
    pub corpus_size: usize,
    pub vault_corpus_size: usize,
    pub corpus_fingerprint: String,
}

struct DomainKernelCorpus {
    rows: KernelContentRows,
    anchors: Vec<CxId>,
    graph: AssocGraph,
    corpus_size: usize,
    vault_corpus_size: usize,
    corpus_fingerprint: String,
}

enum KernelContentRows {
    Dense(Vec<RecallQuery>),
    Sparse(SparseCosineIndex),
}

/// One MiB keeps sparse survivor ownership independent of the allocator pages
/// used by transient constellation hydration. It is an allocation topology,
/// not a corpus cap: additional exact chunks are allocated as required.
const KERNEL_SPARSE_ARENA_CHUNK_BYTES: usize = 1024 * 1024;

struct KernelSparseArena {
    chunks: Vec<Vec<SparseEntry>>,
    len: usize,
}

impl KernelSparseArena {
    const fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    fn append(
        &mut self,
        panel_version: u32,
        slot: u16,
        entries: &[SparseEntry],
    ) -> Result<(), SynapseCalyxError> {
        let entries_per_chunk =
            (KERNEL_SPARSE_ARENA_CHUNK_BYTES / std::mem::size_of::<SparseEntry>()).max(1);
        let new_len = self.len.checked_add(entries.len()).ok_or_else(|| {
            kernel_corpus_allocation_error(
                panel_version,
                slot,
                "sparse entry count overflowed usize",
            )
        })?;
        let mut remaining = entries;
        while !remaining.is_empty() {
            let needs_chunk = self
                .chunks
                .last()
                .is_none_or(|chunk| chunk.len() == chunk.capacity());
            if needs_chunk {
                self.chunks.try_reserve(1).map_err(|error| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        &format!("reserve sparse chunk index: {error}"),
                    )
                })?;
                let mut chunk = Vec::new();
                chunk.try_reserve_exact(entries_per_chunk).map_err(|error| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        &format!(
                            "reserve {entries_per_chunk} entries for a sparse survivor chunk: {error}"
                        ),
                    )
                })?;
                self.chunks.push(chunk);
            }
            let chunk = self.chunks.last_mut().ok_or_else(|| {
                kernel_corpus_allocation_error(
                    panel_version,
                    slot,
                    "sparse chunk allocation completed without a writable chunk",
                )
            })?;
            let take = remaining.len().min(chunk.capacity() - chunk.len());
            chunk.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
        }
        self.len = new_len;
        Ok(())
    }

    fn capacity(&self, panel_version: u32, slot: u16) -> Result<usize, SynapseCalyxError> {
        self.chunks.iter().try_fold(0usize, |total, chunk| {
            total.checked_add(chunk.capacity()).ok_or_else(|| {
                kernel_corpus_allocation_error(
                    panel_version,
                    slot,
                    "sparse chunk capacity accounting overflowed usize",
                )
            })
        })
    }
}

/// Survivor ownership for the one slot a kernel actually consumes. Hydrated
/// row allocations are copied into one exact dense arena or fixed-size sparse
/// chunks and then destroyed immediately, so they cannot pin allocator pages
/// full of dead vectors from unrelated slots.
enum KernelHydrationRows {
    Empty,
    Dense {
        dim: usize,
        ids: Vec<CxId>,
        flat: Vec<f32>,
    },
    Sparse {
        dim: u32,
        ids: Vec<CxId>,
        entries: KernelSparseArena,
        row_ends: Vec<usize>,
    },
}

impl KernelHydrationRows {
    const fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Dense { ids, .. } | Self::Sparse { ids, .. } => ids.len(),
        }
    }

    fn push(
        &mut self,
        panel_version: u32,
        slot: u16,
        max_records: usize,
        cx_id: CxId,
        vector: SlotVector,
    ) -> Result<(), SynapseCalyxError> {
        if self.len() >= max_records {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_SURVIVOR_BOUND_EXCEEDED",
                format!(
                    "panel {panel_version} slot {slot} attempted to retain row {cx_id} after the declared max_records={max_records} boundary"
                ),
                "inspect the kernel Base walk stop condition; never retain more content rows than the public request permits",
            ));
        }
        match vector {
            SlotVector::Dense { dim, data } => {
                self.push_dense(panel_version, slot, max_records, cx_id, dim, &data)
            }
            SlotVector::Sparse { dim, entries } => {
                self.push_sparse(panel_version, slot, max_records, cx_id, dim, &entries)
            }
            SlotVector::Multi { .. } | SlotVector::Absent { .. } => Err(kernel_corpus_shape_error(
                panel_version,
                slot,
                "unsupported vector entered the kernel survivor collector",
            )),
        }
    }

    fn push_dense(
        &mut self,
        panel_version: u32,
        slot: u16,
        max_records: usize,
        cx_id: CxId,
        row_dim: u32,
        data: &[f32],
    ) -> Result<(), SynapseCalyxError> {
        let row_dim = usize::try_from(row_dim).map_err(|error| {
            kernel_corpus_allocation_error(
                panel_version,
                slot,
                &format!("dense dimension does not fit usize: {error}"),
            )
        })?;
        if row_dim == 0 || data.len() != row_dim {
            return Err(kernel_corpus_shape_error(
                panel_version,
                slot,
                format!(
                    "dense row {cx_id} declares dimension {row_dim} but stores {} values",
                    data.len()
                ),
            ));
        }
        match self {
            Self::Empty => {
                let elements = max_records.checked_mul(row_dim).ok_or_else(|| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        "dense survivor capacity overflowed usize",
                    )
                })?;
                let mut ids = Vec::new();
                ids.try_reserve_exact(max_records).map_err(|error| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        &format!("reserve {max_records} dense row identities: {error}"),
                    )
                })?;
                let mut flat = Vec::new();
                flat.try_reserve_exact(elements).map_err(|error| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        &format!("reserve {elements} dense survivor elements: {error}"),
                    )
                })?;
                ids.push(cx_id);
                flat.extend_from_slice(data);
                *self = Self::Dense {
                    dim: row_dim,
                    ids,
                    flat,
                };
                Ok(())
            }
            Self::Dense { dim, ids, flat } if *dim == row_dim => {
                ids.push(cx_id);
                flat.extend_from_slice(data);
                Ok(())
            }
            Self::Dense { dim, .. } => Err(kernel_corpus_shape_error(
                panel_version,
                slot,
                format!(
                    "dense row {cx_id} has dimension {row_dim} but the frozen survivor arena dimension is {dim}"
                ),
            )),
            Self::Sparse { .. } => Err(kernel_corpus_kind_mixed_error(panel_version, slot)),
        }
    }

    fn push_sparse(
        &mut self,
        panel_version: u32,
        slot: u16,
        max_records: usize,
        cx_id: CxId,
        row_dim: u32,
        row: &[SparseEntry],
    ) -> Result<(), SynapseCalyxError> {
        if row_dim == 0 {
            return Err(kernel_corpus_shape_error(
                panel_version,
                slot,
                format!("sparse row {cx_id} has zero dimension"),
            ));
        }
        match self {
            Self::Empty => {
                let mut ids = Vec::new();
                ids.try_reserve_exact(max_records).map_err(|error| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        &format!("reserve {max_records} sparse row identities: {error}"),
                    )
                })?;
                let mut row_ends = Vec::new();
                row_ends.try_reserve_exact(max_records).map_err(|error| {
                    kernel_corpus_allocation_error(
                        panel_version,
                        slot,
                        &format!("reserve {max_records} sparse row offsets: {error}"),
                    )
                })?;
                let mut entries = KernelSparseArena::new();
                entries.append(panel_version, slot, row)?;
                ids.push(cx_id);
                row_ends.push(entries.len);
                *self = Self::Sparse {
                    dim: row_dim,
                    ids,
                    entries,
                    row_ends,
                };
                Ok(())
            }
            Self::Sparse {
                dim,
                ids,
                entries,
                row_ends,
            } if *dim == row_dim => {
                entries.append(panel_version, slot, row)?;
                ids.push(cx_id);
                row_ends.push(entries.len);
                Ok(())
            }
            Self::Sparse { dim, .. } => Err(kernel_corpus_shape_error(
                panel_version,
                slot,
                format!(
                    "sparse row {cx_id} has dimension {row_dim} but the frozen survivor arena dimension is {dim}"
                ),
            )),
            Self::Dense { .. } => Err(kernel_corpus_kind_mixed_error(panel_version, slot)),
        }
    }

    fn ownership(
        &self,
        panel_version: u32,
        slot: u16,
    ) -> Result<(&'static str, usize, usize, usize), SynapseCalyxError> {
        match self {
            Self::Empty => Ok(("empty", 0, 0, 0)),
            Self::Dense { ids, flat, .. } => Ok((
                "dense",
                flat.len(),
                flat.capacity(),
                usize::from(ids.capacity() > 0) + usize::from(flat.capacity() > 0),
            )),
            Self::Sparse {
                ids,
                entries,
                row_ends,
                ..
            } => Ok((
                "sparse",
                entries.len,
                entries.capacity(panel_version, slot)?,
                entries.chunks.len()
                    + usize::from(ids.capacity() > 0)
                    + usize::from(row_ends.capacity() > 0),
            )),
        }
    }

    fn into_rows(
        self,
        panel_version: u32,
        slot: u16,
    ) -> Result<KernelContentRows, SynapseCalyxError> {
        match self {
            Self::Empty => Err(kernel_corpus_shape_error(
                panel_version,
                slot,
                "kernel survivor collector is empty",
            )),
            Self::Dense { dim, ids, flat } => {
                kernel_dense_rows_from_arena(panel_version, slot, dim, ids, &flat)
            }
            Self::Sparse {
                dim: _,
                ids,
                entries,
                row_ends,
            } => kernel_sparse_rows_from_arena(panel_version, slot, ids, entries, row_ends),
        }
    }
}

fn kernel_dense_rows_from_arena(
    panel_version: u32,
    slot: u16,
    dim: usize,
    ids: Vec<CxId>,
    flat: &[f32],
) -> Result<KernelContentRows, SynapseCalyxError> {
    let expected = ids.len().checked_mul(dim).ok_or_else(|| {
        kernel_corpus_allocation_error(
            panel_version,
            slot,
            "dense survivor length readback overflowed usize",
        )
    })?;
    if flat.len() != expected {
        return Err(kernel_corpus_shape_error(
            panel_version,
            slot,
            format!(
                "dense survivor arena has {} elements; {} rows x dimension {dim} requires {expected}",
                flat.len(),
                ids.len()
            ),
        ));
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(ids.len()).map_err(|error| {
        kernel_corpus_allocation_error(
            panel_version,
            slot,
            &format!("reserve {} dense recall rows: {error}", ids.len()),
        )
    })?;
    for (cx_id, values) in ids.into_iter().zip(flat.chunks_exact(dim)) {
        let mut vector = Vec::new();
        vector.try_reserve_exact(dim).map_err(|error| {
            kernel_corpus_allocation_error(
                panel_version,
                slot,
                &format!("reserve dense recall row {cx_id} dimension {dim}: {error}"),
            )
        })?;
        vector.extend_from_slice(values);
        rows.push(RecallQuery { cx_id, vector });
    }
    Ok(KernelContentRows::Dense(rows))
}

fn kernel_sparse_rows_from_arena(
    panel_version: u32,
    slot: u16,
    ids: Vec<CxId>,
    entries: KernelSparseArena,
    row_ends: Vec<usize>,
) -> Result<KernelContentRows, SynapseCalyxError> {
    if ids.len() != row_ends.len() {
        return Err(kernel_corpus_shape_error(
            panel_version,
            slot,
            format!(
                "sparse survivor identity/offset counts diverged: {} vs {}",
                ids.len(),
                row_ends.len()
            ),
        ));
    }
    let KernelSparseArena {
        chunks,
        len: entry_count,
    } = entries;
    let mut source = chunks.into_iter().flatten();
    let mut previous_end = 0usize;
    let mut rows = Vec::new();
    rows.try_reserve_exact(ids.len()).map_err(|error| {
        kernel_corpus_allocation_error(
            panel_version,
            slot,
            &format!("reserve {} sparse recall rows: {error}", ids.len()),
        )
    })?;
    for (cx_id, end) in ids.into_iter().zip(row_ends) {
        let row_len = end.checked_sub(previous_end).ok_or_else(|| {
            kernel_corpus_shape_error(
                panel_version,
                slot,
                format!("sparse row {cx_id} end offset {end} precedes {previous_end}"),
            )
        })?;
        let mut row = Vec::new();
        row.try_reserve_exact(row_len).map_err(|error| {
            kernel_corpus_allocation_error(
                panel_version,
                slot,
                &format!("reserve sparse recall row {cx_id} entries={row_len}: {error}"),
            )
        })?;
        for _ in 0..row_len {
            row.push(source.next().ok_or_else(|| {
                kernel_corpus_shape_error(
                    panel_version,
                    slot,
                    format!("sparse survivor arena ended inside row {cx_id}"),
                )
            })?);
        }
        rows.push((cx_id, row));
        previous_end = end;
    }
    if previous_end != entry_count || source.next().is_some() {
        return Err(kernel_corpus_shape_error(
            panel_version,
            slot,
            format!("sparse survivor readback consumed {previous_end} of {entry_count} entries"),
        ));
    }
    Ok(KernelContentRows::Sparse(SparseCosineIndex::new(rows)?))
}

fn kernel_corpus_allocation_error(
    panel_version: u32,
    slot: u16,
    detail: &str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_KERNEL_CORPUS_ALLOCATION_FAILED",
        format!("panel {panel_version} slot {slot} kernel survivor allocation failed: {detail}"),
        "preserve the vault and inspect the named panel/slot corpus shape and process allocator before retrying",
    )
}

fn kernel_corpus_shape_error(
    panel_version: u32,
    slot: u16,
    detail: impl std::fmt::Display,
) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_SHAPE_INVALID",
        format!("panel {panel_version} slot {slot} kernel content shape is invalid: {detail}"),
        "repair or remeasure the frozen content slot so every physical row has one consistent dense or sparse shape",
    )
}

fn kernel_corpus_kind_mixed_error(panel_version: u32, slot: u16) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_KIND_MIXED",
        format!(
            "panel {panel_version} slot {slot} mixes dense and sparse vectors; one frozen slot must have one physical shape"
        ),
        "repair or remeasure the slot so every record agrees with its frozen lens shape",
    )
}

impl KernelContentRows {
    const fn len(&self) -> usize {
        match self {
            Self::Dense(rows) => rows.len(),
            Self::Sparse(index) => index.rows.len(),
        }
    }

    fn ids(&self) -> Vec<CxId> {
        match self {
            Self::Dense(rows) => rows.iter().map(|row| row.cx_id).collect(),
            Self::Sparse(index) => index.rows.iter().map(|row| row.cx_id).collect(),
        }
    }
}

#[derive(Clone)]
struct SparseKernelRow {
    cx_id: CxId,
    entries: Vec<SparseEntry>,
    norm: f32,
}

struct SparseCosineIndex {
    rows: Vec<SparseKernelRow>,
    by_id: BTreeMap<CxId, usize>,
    postings: BTreeMap<u32, Vec<(usize, f32)>>,
}

impl SparseCosineIndex {
    fn new(source: Vec<(CxId, Vec<SparseEntry>)>) -> Result<Self, SynapseCalyxError> {
        let mut rows = Vec::with_capacity(source.len());
        let mut by_id = BTreeMap::new();
        let mut postings = BTreeMap::<u32, Vec<(usize, f32)>>::new();
        for (cx_id, entries) in source {
            let ordinal = rows.len();
            let norm_sq = entries.iter().try_fold(0.0_f32, |sum, entry| {
                let next = entry.val.mul_add(entry.val, sum);
                next.is_finite().then_some(next).ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_KERNEL_SPARSE_NORM_INVALID",
                        format!("sparse content row {cx_id} norm overflowed"),
                        "repair or remeasure the sparse content slot before building a kernel",
                    )
                })
            })?;
            if norm_sq == 0.0 {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_KERNEL_SPARSE_NORM_INVALID",
                    format!("sparse content row {cx_id} has zero norm"),
                    "measure at least one non-zero term for every sparse kernel concept",
                ));
            }
            for entry in &entries {
                postings
                    .entry(entry.idx)
                    .or_default()
                    .push((ordinal, entry.val));
            }
            by_id.insert(cx_id, ordinal);
            rows.push(SparseKernelRow {
                cx_id,
                entries,
                norm: norm_sq.sqrt(),
            });
        }
        Ok(Self {
            rows,
            by_id,
            postings,
        })
    }

    fn search(
        &self,
        query: CxId,
        top_k: usize,
        allowed: Option<&BTreeSet<CxId>>,
    ) -> Result<Vec<(CxId, f32)>, SynapseCalyxError> {
        let ordinal = self.by_id.get(&query).copied().ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_QUERY_UNMEASURED",
                format!("query record {query} is absent from the sparse kernel corpus"),
                "supply a query record carrying the requested sparse content slot",
            )
        })?;
        let query_row = &self.rows[ordinal];
        let mut dots = BTreeMap::<usize, f32>::new();
        for query_entry in &query_row.entries {
            if let Some(postings) = self.postings.get(&query_entry.idx) {
                for &(candidate, value) in postings {
                    let candidate_id = self.rows[candidate].cx_id;
                    if allowed.is_some_and(|set| !set.contains(&candidate_id)) {
                        continue;
                    }
                    let score = dots.entry(candidate).or_default();
                    *score = query_entry.val.mul_add(value, *score);
                    if !score.is_finite() {
                        return Err(SynapseCalyxError::new(
                            "SYNAPSE_CALYX_KERNEL_SPARSE_SCORE_INVALID",
                            format!(
                                "sparse cosine accumulator overflowed for query {query} and candidate {candidate_id}"
                            ),
                            "repair or remeasure non-finite or excessively weighted sparse vectors",
                        ));
                    }
                }
            }
        }
        let scored_ordinals = dots.keys().copied().collect::<BTreeSet<_>>();
        let mut ranked = dots
            .into_iter()
            .map(|(candidate, dot)| {
                let row = &self.rows[candidate];
                (
                    row.cx_id,
                    (dot / (query_row.norm * row.norm)).clamp(-1.0, 1.0),
                )
            })
            .collect::<Vec<_>>();
        // An unseen row has exact cosine 0. Include enough of them to displace
        // negative-overlap rows and to make top-k exact for rare terms, without
        // materializing the vocabulary or every zero score.
        for (candidate, row) in self.rows.iter().enumerate() {
            if ranked.len() >= top_k
                && ranked.iter().filter(|(_, score)| *score >= 0.0).count() >= top_k
            {
                break;
            }
            if scored_ordinals.contains(&candidate)
                || allowed.is_some_and(|set| !set.contains(&row.cx_id))
            {
                continue;
            }
            ranked.push((row.cx_id, 0.0));
        }
        ranked.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
        });
        ranked.truncate(top_k);
        Ok(ranked)
    }
}

impl SynapseCalyxVault {
    /// Builds the per-domain grounding kernel for one panel, enforces the recall
    /// gate (an ungrounded kernel is a structured error, never served), persists
    /// the kernel with its corpus fingerprint to the native Kernel CF, and reads
    /// the Kernel CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the panel has fewer than two embedded
    /// concepts, no anchored concept, the substrate kernel/recall math fails
    /// closed, the recall gate is not met, or the Kernel CF write/readback fails.
    pub fn build_domain_kernel(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> Result<SynapseCalyxKernelReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("build_domain_kernel");
        finish_kernel_memory_release(
            "kernel domain build",
            self.build_domain_kernel_owned(params),
        )
    }

    fn build_domain_kernel_owned(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> Result<SynapseCalyxKernelReport, SynapseCalyxError> {
        validate_kernel_params(params)?;
        let inputs = self.build_domain_kernel_inputs(params)?;
        // The honesty gate: an ungrounded kernel (recall below the gate) is a
        // structured error, never persisted or served.
        if inputs.recall_ratio < params.min_recall_ratio {
            return Err(SynapseCalyxError::new(
                LodestarError::RecallBelowGate {
                    ratio: inputs.recall_ratio,
                    min: params.min_recall_ratio,
                }
                .code(),
                format!(
                    "domain kernel recall {:.4} is below the gate {:.4} for panel {} slot {}; the kernel does not explain the corpus",
                    inputs.recall_ratio,
                    params.min_recall_ratio,
                    params.panel_version,
                    params.content_slot
                ),
                "widen the corpus, raise knn/lower edge_cos_threshold, or lower min_recall_ratio only with justification; never serve an ungrounded kernel",
            ));
        }

        let anchored_members = inputs
            .kernel
            .members
            .iter()
            .filter(|member| inputs.anchors.contains(member))
            .count();
        let member_cx_ids: Vec<String> = inputs
            .kernel
            .members
            .iter()
            .take(SYNAPSE_KERNEL_MAX_REPORTED_MEMBERS)
            .map(CxId::to_string)
            .collect();

        let row = serde_json::json!({
            "serving_contract_version": crate::kernel_maintenance::KERNEL_SERVING_CONTRACT_VERSION,
            "panel_version": params.panel_version,
            "content_slot": params.content_slot,
            "kernel_id": inputs.kernel.kernel_id.to_string(),
            "max_records": params.max_records,
            "knn": params.knn,
            "edge_cos_threshold": params.edge_cos_threshold,
            "math_execution_class": params.math_execution_class.as_str(),
            "corpus_fingerprint": inputs.corpus_fingerprint,
            "anchor_kind": inputs.kernel.anchor_kind,
            "members": inputs.kernel.members.iter().map(CxId::to_string).collect::<Vec<_>>(),
            "kernel_graph": inputs.kernel.kernel_graph.iter().map(CxId::to_string).collect::<Vec<_>>(),
            "recall_kernel_only": inputs.recall_kernel_only,
            "recall_ratio": inputs.recall_ratio,
            "min_recall_ratio": params.min_recall_ratio,
            "reached_anchor": inputs.kernel.groundedness.reached_anchor,
            "built_at_millis": inputs.kernel.built_at_millis,
            "estimator_provenance": inputs.kernel.estimator_provenance,
        });
        // Publish immutable data before its discoverable index pointer. A crash
        // may leave an unreachable artifact, but can never expose an index row
        // whose artifact has not reached the Kernel CF yet.
        write_kernel_artifact(
            &inputs.kernel,
            &crate::kernel_maintenance::VaultKernelArtifactStore::new(self),
        )
        .map_err(|error| kernel_math_error("persist the Kernel artifact", &error))?;
        self.persist_temporal_row(
            ColumnFamily::Kernel,
            kernel_row_key(params.panel_version, params.content_slot),
            &row,
        )?;
        let kernel_cf_rows_after = self
            .count_cf_latest_bounded(ColumnFamily::Kernel)?
            .rows_visited;

        Ok(SynapseCalyxKernelReport {
            panel_version: params.panel_version,
            content_slot: params.content_slot,
            kernel_id: inputs.kernel.kernel_id.to_string(),
            corpus_fingerprint: inputs.corpus_fingerprint,
            members: inputs.kernel.members.len(),
            kernel_graph_nodes: inputs.kernel.kernel_graph.len(),
            corpus_size: inputs.corpus_size,
            vault_corpus_size: inputs.vault_corpus_size,
            recall_kernel_only: inputs.recall_kernel_only,
            recall_ratio: inputs.recall_ratio,
            min_recall_ratio: params.min_recall_ratio,
            grounded: true,
            reached_anchor: inputs.kernel.groundedness.reached_anchor,
            unanchored_members: inputs.kernel.groundedness.unanchored_members.len(),
            anchored_members,
            member_cx_ids,
            kernel_cf_rows_after,
        })
    }

    /// Answers a grounded query through the persisted domain kernel. The Kernel
    /// CF supplies the immutable selection and measured recall; current Base/slot
    /// rows must reproduce the exact semantic corpus before the query graph is
    /// hydrated. Missing, stale, or mismatched state is never rebuilt in-read.
    ///
    /// # Errors
    ///
    /// Returns a structured refusal when the kernel is ungrounded, the query
    /// record is missing/unembedded, or no grounded path exists; or a structured
    /// error when the substrate math or a CF scan fails closed.
    pub fn kernel_answer(
        &self,
        params: &SynapseCalyxKernelParams,
        query_cx_id: &str,
        max_hops: usize,
    ) -> Result<SynapseCalyxKernelAnswerReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("kernel_answer");
        finish_kernel_memory_release(
            "kernel grounded answer",
            self.kernel_answer_owned(params, query_cx_id, max_hops),
        )
    }

    fn kernel_answer_owned(
        &self,
        params: &SynapseCalyxKernelParams,
        query_cx_id: &str,
        max_hops: usize,
    ) -> Result<SynapseCalyxKernelAnswerReport, SynapseCalyxError> {
        if !(1..=64).contains(&max_hops) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_PARAMS_INVALID",
                format!("kernel answer max_hops={max_hops} is outside 1..=64"),
                "pass max_hops within the declared bound; kernel answers never clamp an invalid request",
            ));
        }
        let query_cx = crate::parse_cx_id(query_cx_id)?;
        let inputs = self.load_persisted_domain_kernel_inputs(params)?;
        if inputs.recall_ratio < params.min_recall_ratio {
            return Err(SynapseCalyxError::new(
                LodestarError::RecallBelowGate {
                    ratio: inputs.recall_ratio,
                    min: params.min_recall_ratio,
                }
                .code(),
                format!(
                    "cannot answer: domain kernel recall {:.4} is below the gate {:.4}; the kernel does not explain the corpus",
                    inputs.recall_ratio, params.min_recall_ratio
                ),
                "rebuild the kernel over a wider corpus or repair the panel embeddings before answering grounded queries",
            ));
        }
        // The query embedding is the query record's own dense content-slot vector
        // (encoders only — there is no query embedder). A query record without an
        // embedding in this panel is a named refusal, never a guessed vector.
        let anchored_kernel_nodes: Vec<CxId> = inputs
            .kernel
            .members
            .iter()
            .copied()
            .filter(|member| inputs.anchors.contains(member))
            .collect();
        if anchored_kernel_nodes.is_empty() {
            return Err(SynapseCalyxError::new(
                LodestarError::KernelNoAnchoredNode.code(),
                format!(
                    "cannot answer: kernel for panel {} slot {} has no anchored member to ground an answer",
                    params.panel_version, params.content_slot
                ),
                "anchor at least one kernel concept (a grounded outcome) before answering grounded queries",
            ));
        }

        let derivation: AnswerDerivation = match (&inputs.rows, &inputs.kernel_index) {
            (KernelContentRows::Dense(rows), Some(index)) => {
                let query_vec = rows.iter().find(|row| row.cx_id == query_cx)
                    .map(|row| row.vector.clone()).ok_or_else(|| SynapseCalyxError::new(
                        "SYNAPSE_CALYX_KERNEL_QUERY_UNMEASURED",
                        format!("query record {query_cx} has no dense measurement in panel {} slot {}", params.panel_version, params.content_slot),
                        "supply a query cx_id that exists in this panel and carries the requested content slot",
                    ))?;
                derive_kernel_answer(index, &inputs.graph, query_cx, &query_vec, &anchored_kernel_nodes, max_hops)
            }
            (KernelContentRows::Sparse(index), None) => {
                let allowed = inputs.kernel.members.iter().copied().collect::<BTreeSet<_>>();
                let ranked = index.search(query_cx, allowed.len(), Some(&allowed))?;
                derive_kernel_answer_from_ranked_members(inputs.kernel.kernel_id, &inputs.graph, query_cx, &ranked, &anchored_kernel_nodes, max_hops)
            }
            _ => return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_INDEX_KIND_MISMATCH",
                "kernel content rows and retrieval index kinds disagree",
                "rebuild the kernel inputs from the vault; do not reuse a mismatched derived index",
            )),
        }.map_err(|error| kernel_refusal("derive grounded kernel answer", &error))?;

        let hops: Vec<SynapseCalyxKernelAnswerHop> = derivation
            .hops
            .iter()
            .map(|hop| SynapseCalyxKernelAnswerHop {
                from: hop.from.to_string(),
                to: hop.to.to_string(),
                edge_weight: hop.edge_weight,
                hop_index: hop.hop_index,
                hop_score: hop.hop_score,
            })
            .collect();

        Ok(SynapseCalyxKernelAnswerReport {
            panel_version: params.panel_version,
            content_slot: params.content_slot,
            query_cx_id: query_cx.to_string(),
            grounded: true,
            kernel_id: derivation.kernel_id.to_string(),
            anchor_kernel_node: derivation.anchor_kernel_node.to_string(),
            total_score: derivation.total_score,
            hop_count: hops.len(),
            hops,
            kernel_members: inputs.kernel.members.len(),
            recall_ratio: inputs.recall_ratio,
            min_recall_ratio: params.min_recall_ratio,
        })
    }

    /// Hydrates the bounded current corpus and association graph used by both
    /// cold kernel builds and persisted-kernel serving. Selection and recall do
    /// not belong here: a read must be able to hydrate current query data without
    /// silently constructing a second, unpersisted kernel.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the panel has fewer than two embedded
    /// concepts, no anchored concept in the requested domain scope, or when the
    /// substrate graph/kernel/recall math fails closed.
    #[allow(clippy::too_many_lines)]
    fn hydrate_domain_kernel_corpus(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> Result<DomainKernelCorpus, SynapseCalyxError> {
        validate_kernel_params(params)?;
        let max_records = params.max_records;
        let content_slot = PanelSlotId::new(params.panel_version, SlotId::new(params.content_slot));
        let hydration = (|| {
            let mut measured_rows = KernelHydrationRows::Empty;
            let mut anchors: Vec<CxId> = Vec::new();
            anchors.try_reserve_exact(max_records).map_err(|error| {
                kernel_corpus_allocation_error(
                    params.panel_version,
                    params.content_slot,
                    &format!("reserve {max_records} anchored row identities: {error}"),
                )
            })?;
            let mut vault_corpus_size = 0usize;
            let mut rejects = ContentSlotRejects::default();
            // The panel membership sidecar prevents a cross-panel Base scan.
            // #2243 narrows pinned hydration to the one vector consumed by this
            // kernel; hydrating every unrelated slot made short-lived vectors
            // share allocator pages with retained content rows and produced
            // multi-GiB peaks.
            self.with_panel_read_snapshot(
                params.panel_version,
                crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
                |snapshot| {
                    self.walk_panel_base_snapshot(
                        snapshot,
                        params.panel_version,
                        |snapshot, _key, value| {
                            let base = decode_constellation_base(value).map_err(|error| {
                                SynapseCalyxError::from_calyx("decode Base constellation", &error)
                            })?;
                            if base.panel_version != params.panel_version {
                                return Ok(crate::SynapseCalyxWalkStep::Continue);
                            }
                            vault_corpus_size =
                                vault_corpus_size.checked_add(1).ok_or_else(|| {
                                    kernel_corpus_allocation_error(
                                        params.panel_version,
                                        params.content_slot,
                                        "panel corpus count overflowed usize",
                                    )
                                })?;
                            // Base identifies slot presence but the vector lives in
                            // its slot CF. The selected-slot read preserves the same
                            // pinned snapshot and anchors without allocating every
                            // other slot in the constellation.
                            if !base.slots.contains_key(&content_slot.slot_id()) {
                                return Ok(crate::SynapseCalyxWalkStep::Continue);
                            }
                            let mut constellation = self
                                .vault
                                .get_selected_slots_at_snapshot(
                                    base.cx_id,
                                    snapshot,
                                    [content_slot.slot_id()],
                                )
                                .map_err(|error| {
                                    SynapseCalyxError::from_calyx(
                                        "hydrate pinned kernel content slot",
                                        &error,
                                    )
                                })?;
                            let Some(vector) = constellation.slots.remove(&content_slot.slot_id())
                            else {
                                rejects.record("absent");
                                return Ok(crate::SynapseCalyxWalkStep::Continue);
                            };
                            match &vector {
                                SlotVector::Dense { data, .. }
                                    if data.iter().all(|value| *value == 0.0) =>
                                {
                                    rejects.record("empty");
                                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                                }
                                SlotVector::Sparse { entries, .. } if entries.is_empty() => {
                                    rejects.record("empty");
                                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                                }
                                SlotVector::Multi { .. } => {
                                    rejects.record("multi");
                                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                                }
                                SlotVector::Absent { .. } => {
                                    rejects.record("absent");
                                    return Ok(crate::SynapseCalyxWalkStep::Continue);
                                }
                                SlotVector::Dense { .. } | SlotVector::Sparse { .. } => {}
                            }
                            let has_anchor = constellation.anchors.iter().any(|anchor| {
                                anchor.confidence > 0.0
                                    && params.anchor_kind.as_deref().is_none_or(|kind| {
                                        crate::grounding::anchor_kind_label(&anchor.kind) == kind
                                    })
                            });
                            let cx_id = constellation.cx_id;
                            measured_rows.push(
                                params.panel_version,
                                params.content_slot,
                                max_records,
                                cx_id,
                                vector,
                            )?;
                            if has_anchor {
                                anchors.push(cx_id);
                            }
                            if measured_rows.len() >= max_records {
                                return Ok(crate::SynapseCalyxWalkStep::Stop);
                            }
                            Ok(crate::SynapseCalyxWalkStep::Continue)
                        },
                    )
                },
            )?;

            // Multi vectors require MaxSim and are deliberately not coerced
            // into cosine. Sparse vectors remain native kernel content.
            if rejects.wrong_kind() > 0 {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_KERNEL_CONTENT_SLOT_UNSUPPORTED",
                    format!(
                        "panel {} slot {} stores an unsupported vector on {} of {} measured or excluded record(s) ({}); dense cosine and sparse cosine are supported, but multi vectors require an explicit MaxSim kernel",
                        params.panel_version,
                        params.content_slot,
                        rejects.wrong_kind(),
                        rejects.wrong_kind() + rejects.absent + rejects.empty + measured_rows.len(),
                        rejects.describe()
                    ),
                    "name a dense or sparse content slot; add an explicit MaxSim association path before using a multi-vector slot",
                ));
            }
            if measured_rows.len() < 2 {
                return Err(SynapseCalyxError::new(
                    LodestarError::KernelEmptyResult.code(),
                    format!(
                        "panel {} slot {} has {} embedded concept(s) out of {} record(s) in the panel; a kernel needs at least two (excluded: {})",
                        params.panel_version,
                        params.content_slot,
                        measured_rows.len(),
                        vault_corpus_size,
                        rejects.describe()
                    ),
                    "capture more grounded concepts with the content-slot embedding for this domain; if every record is 'absent' the slot CF rows were never hydrated (see #1894) rather than never measured",
                ));
            }
            if anchors.is_empty() {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_KERNEL_NO_ANCHOR",
                    format!(
                        "panel {} slot {} has no anchored concept in domain scope {}; a grounded kernel needs at least one outcome anchor",
                        params.panel_version,
                        params.content_slot,
                        params
                            .anchor_kind
                            .as_deref()
                            .unwrap_or("<all anchor kinds>")
                    ),
                    "anchor at least one concept (a grounded outcome) in this domain before building a kernel",
                ));
            }
            let ownership = measured_rows.ownership(params.panel_version, params.content_slot)?;
            Ok((measured_rows, anchors, vault_corpus_size, ownership))
        })();

        // At this boundary every Base decode, selected-slot temporary, cursor,
        // and page is dead. The only live corpus ownership is the explicitly
        // logged survivor arena, so allocator release is both effective and
        // auditable. Preserve both causes if hydration and release fail.
        let release = crate::release_process_memory("kernel content-slot hydration");
        let (measured_rows, anchors, vault_corpus_size) = match (hydration, release) {
            (Err(hydration_error), Err(release_error)) => {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_KERNEL_HYDRATION_AND_MEMORY_RELEASE_FAILED",
                    format!(
                        "panel {} slot {} content hydration failed with {}: {}; after hydration ownership ended, allocator release also failed with {release_error}",
                        params.panel_version,
                        params.content_slot,
                        hydration_error.code,
                        hydration_error.message
                    ),
                    "repair both the named content hydration failure and the process memory reclaimer before retrying",
                ));
            }
            (Err(hydration_error), Ok(_)) => return Err(hydration_error),
            (Ok(_), Err(release_error)) => return Err(release_error),
            (Ok(hydration), Ok(release)) => {
                tracing::info!(
                    code = "SYNAPSE_CALYX_KERNEL_CORPUS_MEMORY_RELEASED",
                    panel_version = params.panel_version,
                    slot = params.content_slot,
                    records = hydration.0.len(),
                    survivor_kind = hydration.3.0,
                    survivor_vector_elements = hydration.3.1,
                    survivor_vector_capacity_elements = hydration.3.2,
                    survivor_allocations = hydration.3.3,
                    private_bytes_before = release.private_bytes_before,
                    private_bytes_after = release.private_bytes_after,
                    private_bytes_reclaimed = release.private_bytes_reclaimed,
                    release_elapsed_us = release.elapsed_us,
                    "released dead selected-slot hydration ownership while preserving the exact bounded kernel corpus"
                );
                (hydration.0, hydration.1, hydration.2)
            }
        };

        let rows = measured_rows.into_rows(params.panel_version, params.content_slot)?;
        let conversion_release = crate::release_process_memory("kernel survivor arena conversion")?;
        tracing::info!(
            code = "SYNAPSE_CALYX_KERNEL_SURVIVOR_CONVERSION_MEMORY_RELEASED",
            panel_version = params.panel_version,
            slot = params.content_slot,
            rows = rows.len(),
            private_bytes_before = conversion_release.private_bytes_before,
            private_bytes_after = conversion_release.private_bytes_after,
            private_bytes_reclaimed = conversion_release.private_bytes_reclaimed,
            release_elapsed_us = conversion_release.elapsed_us,
            "released the flat/chunked hydration arena after moving the exact survivors into kernel-native rows"
        );

        let graph = self.build_kernel_assoc_graph(&rows, params)?;
        let corpus_fingerprint = corpus_fingerprint(&rows, &anchors);
        let corpus_size = rows.len();
        Ok(DomainKernelCorpus {
            rows,
            anchors,
            graph,
            corpus_size,
            vault_corpus_size,
            corpus_fingerprint,
        })
    }

    fn load_persisted_domain_kernel_inputs(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> Result<DomainKernelInputs, SynapseCalyxError> {
        validate_kernel_params(params)?;
        let persisted = self.read_domain_kernel_for_serving(
            params.panel_version,
            params.content_slot,
            params.anchor_kind.as_deref(),
        )?;
        if persisted.max_records != params.max_records
            || persisted.knn != params.knn
            || persisted.edge_cos_threshold.to_bits() != params.edge_cos_threshold.to_bits()
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_SERVING_CONTRACT_MISMATCH",
                format!(
                    "persisted kernel graph contract max_records={} knn={} edge_cos_threshold={} does not match requested max_records={} knn={} edge_cos_threshold={} for panel {} slot {}",
                    persisted.max_records,
                    persisted.knn,
                    persisted.edge_cos_threshold,
                    params.max_records,
                    params.knn,
                    params.edge_cos_threshold,
                    params.panel_version,
                    params.content_slot
                ),
                "query with the exact persisted graph contract or rebuild the kernel under the intended parameters; reads never recompute a replacement kernel",
            ));
        }

        let mut serving_params = params.clone();
        serving_params.math_execution_class = persisted.math_execution_class;
        let DomainKernelCorpus {
            rows,
            anchors,
            graph,
            corpus_size,
            vault_corpus_size,
            corpus_fingerprint,
        } = self.hydrate_domain_kernel_corpus(&serving_params)?;
        if persisted.corpus_fingerprint != corpus_fingerprint {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_CORPUS_STALE",
                format!(
                    "persisted kernel {} corpus fingerprint {} does not match current bounded panel {} slot {} corpus fingerprint {}",
                    persisted.kernel.kernel_id,
                    persisted.corpus_fingerprint,
                    params.panel_version,
                    params.content_slot,
                    corpus_fingerprint
                ),
                "run the cold kernel rebuild for this exact panel/slot/domain; serving never combines an old kernel selection and recall certificate with a different corpus",
            ));
        }
        let available = rows.ids().into_iter().collect::<BTreeSet<_>>();
        if let Some(missing) = persisted
            .kernel
            .members
            .iter()
            .find(|member| !available.contains(member))
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_ARTIFACT_MEMBER_MISSING",
                format!(
                    "persisted kernel {} member {missing} is absent from the current bounded panel {} slot {} corpus",
                    persisted.kernel.kernel_id, params.panel_version, params.content_slot
                ),
                "rebuild the persisted kernel against the current corpus; never silently drop an artifact member",
            ));
        }

        verify_kernel_serving_identity(&persisted.kernel, &serving_params, &rows, &anchors)?;

        let kernel = persisted.kernel;
        let kernel_index = match &rows {
            KernelContentRows::Dense(dense) => {
                let members = kernel.members.iter().copied().collect::<BTreeSet<_>>();
                let embeddings = dense
                    .iter()
                    .filter(|row| members.contains(&row.cx_id))
                    .map(|row| (row.cx_id, row.vector.clone()))
                    .collect::<BTreeMap<_, _>>();
                Some(build_kernel_index(&kernel, &embeddings).map_err(|error| {
                    kernel_math_error("build persisted dense kernel serving index", &error)
                })?)
            }
            KernelContentRows::Sparse(_) => None,
        };
        Ok(DomainKernelInputs {
            rows,
            anchors,
            graph,
            recall_kernel_only: kernel.recall.kernel_only,
            recall_ratio: kernel.recall.ratio,
            kernel,
            kernel_index,
            corpus_size,
            vault_corpus_size,
            corpus_fingerprint,
        })
    }

    /// Assembles a new kernel from the current vault corpus for cold build and
    /// persistence. Read-only answer serving uses
    /// [`Self::load_persisted_domain_kernel_inputs`] instead.
    ///
    /// # Errors
    ///
    /// Returns a structured error when request bounds are invalid, corpus
    /// hydration fails, kernel selection cannot ground the corpus, or recall
    /// measurement fails.
    #[allow(clippy::too_many_lines)]
    pub fn build_domain_kernel_inputs(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> Result<DomainKernelInputs, SynapseCalyxError> {
        let DomainKernelCorpus {
            rows,
            anchors,
            graph,
            corpus_size,
            vault_corpus_size,
            corpus_fingerprint,
        } = self.hydrate_domain_kernel_corpus(params)?;
        let kernel_params = KernelParams {
            panel_version: params.panel_version,
            anchor_kind: params.anchor_kind.clone(),
            corpus_shard_hash: corpus_hash_bytes(&rows, &anchors),
            built_at_millis: self.clock_now_ms().unwrap_or(0),
            kernel_graph: KernelGraphParams::default(),
            lp_round: LpRoundParams::default(),
        };
        let mut kernel = build_kernel_pipeline(&graph, &anchors, &kernel_params)
            .map_err(|error| kernel_math_error("select domain kernel", &error))?;
        if kernel.members.is_empty() {
            return Err(SynapseCalyxError::new(
                LodestarError::KernelEmptyResult.code(),
                format!(
                    "kernel selection produced no members for panel {} slot {}",
                    params.panel_version, params.content_slot
                ),
                "inspect the association graph density (knn/edge_cos_threshold) and the anchored set",
            ));
        }
        let recall_params = RecallEvalParams {
            min_recall_ratio: params.min_recall_ratio,
            ..RecallEvalParams::default()
        };
        let (kernel_index, mut recall) = match &rows {
            KernelContentRows::Dense(dense) => {
                let embeddings = dense
                    .iter()
                    .map(|row| (row.cx_id, row.vector.clone()))
                    .collect::<BTreeMap<_, _>>();
                let mut kernel_index = build_kernel_index(&kernel, &embeddings)
                    .map_err(|error| kernel_math_error("build dense kernel index", &error))?;
                let full = InMemoryAnnIndex::new(dense.clone())
                    .map_err(|error| kernel_math_error("build dense full-corpus index", &error))?;
                let corpus = InMemoryCorpus::new("synapse-domain-kernel-dense", dense.clone());
                let mut recall =
                    measure_kernel_recall(&kernel_index, &full, &corpus, &recall_params).map_err(
                        |error| kernel_math_error("measure dense kernel-only recall", &error),
                    )?;
                if recall.ratio < params.min_recall_ratio {
                    let support =
                        full_topk_support_set(&full, &corpus, &recall_params).map_err(|error| {
                            kernel_math_error("collect dense recall support", &error)
                        })?;
                    kernel = refine_kernel_with_recall_support(
                        kernel,
                        &support.members,
                        &graph,
                        &anchors,
                        &kernel_params,
                        "held-out exact dense top-k support",
                    )
                    .map_err(|error| kernel_math_error("refine dense kernel for recall", &error))?;
                    kernel_index = build_kernel_index(&kernel, &embeddings).map_err(|error| {
                        kernel_math_error("rebuild refined dense kernel index", &error)
                    })?;
                    recall = measure_kernel_recall(&kernel_index, &full, &corpus, &recall_params)
                        .map_err(|error| {
                        kernel_math_error("remeasure refined dense kernel recall", &error)
                    })?;
                }
                (Some(kernel_index), recall)
            }
            KernelContentRows::Sparse(index) => {
                let ids = rows.ids();
                let mut members = kernel.members.iter().copied().collect::<BTreeSet<_>>();
                let mut recall = measure_ranked_kernel_recall(
                    "synapse-domain-kernel-sparse-cosine",
                    &ids,
                    &recall_params,
                    |query, top_k| {
                        index.search(query, top_k, None).map_err(|error| {
                            LodestarError::KernelIndexBuild {
                                detail: error.to_string(),
                            }
                        })
                    },
                    |query, top_k| {
                        index.search(query, top_k, Some(&members)).map_err(|error| {
                            LodestarError::KernelIndexBuild {
                                detail: error.to_string(),
                            }
                        })
                    },
                )
                .map_err(|error| kernel_math_error("measure sparse kernel-only recall", &error))?;
                if recall.ratio < params.min_recall_ratio {
                    let mut support = BTreeSet::new();
                    for query in &recall.held_out {
                        for (candidate, _score) in
                            index.search(*query, recall_params.top_k, None)?
                        {
                            support.insert(candidate);
                        }
                    }
                    let support = support.into_iter().collect::<Vec<_>>();
                    kernel = refine_kernel_with_recall_support(
                        kernel,
                        &support,
                        &graph,
                        &anchors,
                        &kernel_params,
                        "held-out exact sparse-cosine top-k support",
                    )
                    .map_err(|error| {
                        kernel_math_error("refine sparse kernel for recall", &error)
                    })?;
                    members = kernel.members.iter().copied().collect();
                    recall = measure_ranked_kernel_recall(
                        "synapse-domain-kernel-sparse-cosine",
                        &ids,
                        &recall_params,
                        |query, top_k| {
                            index.search(query, top_k, None).map_err(|error| {
                                LodestarError::KernelIndexBuild {
                                    detail: error.to_string(),
                                }
                            })
                        },
                        |query, top_k| {
                            index.search(query, top_k, Some(&members)).map_err(|error| {
                                LodestarError::KernelIndexBuild {
                                    detail: error.to_string(),
                                }
                            })
                        },
                    )
                    .map_err(|error| {
                        kernel_math_error("remeasure refined sparse kernel recall", &error)
                    })?;
                }
                (None, recall)
            }
        };

        // Preserve the selection algorithm's DFVS provenance while replacing
        // its placeholder recall fields with the value actually measured over
        // this physical corpus. The completed identity is sealed only after
        // refinement and recall are final, so health cannot read a default
        // report from an id minted for a different semantic artifact (#1999).
        recall.approx_factor = kernel.recall.approx_factor;
        recall.tau_star_estimate = kernel.recall.tau_star_estimate;
        recall.tau_star_exact = kernel.recall.tau_star_exact;
        kernel.recall = recall.clone();
        let physical_contract = kernel_physical_contract(params, &rows, &anchors);
        seal_completed_kernel_identity(&mut kernel, &physical_contract)
            .map_err(|error| kernel_math_error("seal completed domain kernel identity", &error))?;

        Ok(DomainKernelInputs {
            rows,
            anchors,
            graph,
            kernel,
            kernel_index,
            recall_kernel_only: recall.kernel_only,
            recall_ratio: recall.ratio,
            corpus_size,
            vault_corpus_size,
            corpus_fingerprint,
        })
    }

    /// Builds the embedding-proximity association graph: a node per embedded
    /// concept, an edge to each of its `knn` cosine-nearest neighbours at or above
    /// `edge_cos_threshold` through the declared Forge execution class.
    fn build_kernel_assoc_graph(
        &self,
        rows: &KernelContentRows,
        params: &SynapseCalyxKernelParams,
    ) -> Result<AssocGraph, SynapseCalyxError> {
        match rows {
            KernelContentRows::Dense(rows) => self.build_dense_kernel_assoc_graph(rows, params),
            KernelContentRows::Sparse(index) => build_sparse_kernel_assoc_graph(index, params),
        }
    }

    fn build_dense_kernel_assoc_graph(
        &self,
        rows: &[RecallQuery],
        params: &SynapseCalyxKernelParams,
    ) -> Result<AssocGraph, SynapseCalyxError> {
        let mut builder = AssocGraph::builder();
        for row in rows {
            builder
                .add_node(row.cx_id, 1.0)
                .map_err(|error| paths_error("add kernel graph node", &error))?;
        }
        let knn = params.knn;
        // Only equal-dimension vectors can be compared by cosine; group by dim.
        let mut by_dim: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, row) in rows.iter().enumerate() {
            by_dim.entry(row.vector.len()).or_default().push(index);
        }
        let configured_backend = (params.math_execution_class
            == SynapseCalyxMathExecutionClass::Configured)
            .then(|| self.math_runtime.backend())
            .transpose()?;
        let background_backend = (params.math_execution_class
            == SynapseCalyxMathExecutionClass::BackgroundCpu)
            .then(crate::math::verified_background_cpu_backend)
            .transpose()?;
        let backend = configured_backend
            .as_ref()
            .map(|lease| &**lease as &dyn Backend)
            .or(background_backend.as_deref())
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_KERNEL_MATH_BACKEND_MISSING",
                    format!(
                        "panel {} slot {} execution class {} produced no dense kNN backend",
                        params.panel_version,
                        params.content_slot,
                        params.math_execution_class.as_str()
                    ),
                    "repair the declared kernel math backend; the kernel graph never falls back to another execution class",
                )
            })?;
        tracing::info!(
            code = "SYNAPSE_CALYX_KERNEL_MATH_EXECUTION_CLASS",
            panel_version = params.panel_version,
            slot = params.content_slot,
            requested_execution_class = params.math_execution_class.as_str(),
            backend_used = params.math_execution_class.as_str(),
            configured_runtime_activated = configured_backend.is_some(),
            background_cpu_activated = background_backend.is_some(),
            "selected the declared dense kernel math execution class without runtime fallback"
        );
        for (dim, group) in by_dim {
            append_dense_kernel_group_edges(&mut builder, backend, rows, &group, dim, knn, params)?;
        }
        Ok(builder.build())
    }
}

fn build_sparse_kernel_assoc_graph(
    index: &SparseCosineIndex,
    params: &SynapseCalyxKernelParams,
) -> Result<AssocGraph, SynapseCalyxError> {
    let mut builder = AssocGraph::builder();
    for row in &index.rows {
        builder
            .add_node(row.cx_id, 1.0)
            .map_err(|error| paths_error("add sparse kernel graph node", &error))?;
    }
    let knn = params.knn;
    tracing::info!(
        code = "SYNAPSE_CALYX_KERNEL_MATH_EXECUTION_CLASS",
        panel_version = params.panel_version,
        slot = params.content_slot,
        requested_execution_class = params.math_execution_class.as_str(),
        backend_used = "sparse_cpu_exact",
        configured_runtime_activated = false,
        background_cpu_activated = false,
        "selected the native sparse CPU path without activating the configured math runtime"
    );
    for row in &index.rows {
        for (candidate, score) in index
            .search(row.cx_id, knn + 1, None)?
            .into_iter()
            .filter(|(candidate, _)| *candidate != row.cx_id)
            .take(knn)
        {
            let score = kernel_graph_cosine(score)?;
            if score < params.edge_cos_threshold {
                continue;
            }
            builder
                .add_edge(row.cx_id, candidate, score)
                .map_err(|error| paths_error("add sparse kernel graph edge", &error))?;
        }
    }
    Ok(builder.build())
}

fn append_dense_kernel_group_edges(
    builder: &mut calyx_paths::AssocGraphBuilder,
    backend: &dyn Backend,
    rows: &[RecallQuery],
    group: &[usize],
    dim: usize,
    knn: usize,
    params: &SynapseCalyxKernelParams,
) -> Result<(), SynapseCalyxError> {
    if dim == 0 || group.len() < 2 {
        return Ok(());
    }
    let count = group.len();
    let elements = count.checked_mul(dim).ok_or_else(|| {
        kernel_corpus_allocation_error(
            params.panel_version,
            params.content_slot,
            "dense kernel graph arena size overflowed usize",
        )
    })?;
    let mut flat = Vec::new();
    flat.try_reserve_exact(elements).map_err(|error| {
        kernel_corpus_allocation_error(
            params.panel_version,
            params.content_slot,
            &format!("reserve {elements} dense kernel graph elements: {error}"),
        )
    })?;
    for &index in group {
        flat.extend_from_slice(&rows[index].vector);
    }
    let k = (knn + 1).min(count);
    let batch = backend
        .knn(&flat, &flat, count, dim, k, KnnMetric::Cosine)
        .map_err(|error| forge_math_error("kernel-graph kNN", &error))?;
    for (query_offset, &query_index) in group.iter().enumerate() {
        let base = query_offset.checked_mul(batch.k).ok_or_else(|| {
            kernel_corpus_shape_error(
                params.panel_version,
                params.content_slot,
                "dense kNN result offset overflowed usize",
            )
        })?;
        let mut added = 0usize;
        for slot in 0..batch.k {
            let result_index = base.checked_add(slot).ok_or_else(|| {
                kernel_corpus_shape_error(
                    params.panel_version,
                    params.content_slot,
                    "dense kNN result index overflowed usize",
                )
            })?;
            let candidate_offset = *batch.indices.get(result_index).ok_or_else(|| {
                kernel_corpus_shape_error(
                    params.panel_version,
                    params.content_slot,
                    format!(
                        "dense kNN backend returned {} indices; result {result_index} is absent",
                        batch.indices.len()
                    ),
                )
            })?;
            if candidate_offset == query_offset {
                continue;
            }
            let score = *batch.scores.get(result_index).ok_or_else(|| {
                kernel_corpus_shape_error(
                    params.panel_version,
                    params.content_slot,
                    format!(
                        "dense kNN backend returned {} scores; result {result_index} is absent",
                        batch.scores.len()
                    ),
                )
            })?;
            let score = kernel_graph_cosine(score)?;
            if score < params.edge_cos_threshold {
                continue;
            }
            let candidate_index = *group.get(candidate_offset).ok_or_else(|| {
                kernel_corpus_shape_error(
                    params.panel_version,
                    params.content_slot,
                    format!(
                        "dense kNN backend returned candidate offset {candidate_offset} for group size {count}"
                    ),
                )
            })?;
            builder
                .add_edge(rows[query_index].cx_id, rows[candidate_index].cx_id, score)
                .map_err(|error| paths_error("add kernel graph edge", &error))?;
            added += 1;
            if added >= knn {
                break;
            }
        }
    }
    Ok(())
}

fn finish_kernel_memory_release<T>(
    operation: &'static str,
    result: Result<T, SynapseCalyxError>,
) -> Result<T, SynapseCalyxError> {
    // The inner operation has returned, so every graph, recall index, corpus,
    // and query workspace it owned is dead before the allocator is asked to
    // release pages. Preserve both independent causes if work and release fail.
    let release = crate::release_process_memory(operation);
    match (result, release) {
        (Err(operation_error), Err(release_error)) => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_KERNEL_OPERATION_AND_MEMORY_RELEASE_FAILED",
            format!(
                "{operation} failed with {}: {}; after all kernel-owned transients were dropped, allocator release also failed with {release_error}",
                operation_error.code, operation_error.message
            ),
            "repair both the named kernel operation failure and the process memory reclaimer before retrying",
        )),
        (Err(operation_error), Ok(_)) => Err(operation_error),
        (Ok(_), Err(release_error)) => Err(release_error),
        (Ok(value), Ok(_)) => Ok(value),
    }
}

fn kernel_graph_cosine(score: f32) -> Result<f32, SynapseCalyxError> {
    if !score.is_finite() {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_KERNEL_COSINE_NON_FINITE",
            format!("kernel association cosine is non-finite: {score}"),
            "repair or remeasure the content slot; non-finite similarity cannot enter the association graph",
        ));
    }
    // Cosine is mathematically within [-1, 1], but f32 accumulation may
    // overshoot by an ulp. Graph agreement weights are defined in [0, 1].
    Ok(score.clamp(0.0, 1.0))
}

/// Semantic fingerprint of every input that defines kernel selection: vector
/// kind, sorted row identities, exact vector bits, and sorted anchored members.
fn corpus_fingerprint(rows: &KernelContentRows, anchors: &[CxId]) -> String {
    let bytes = corpus_hash_bytes(rows, anchors);
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

fn corpus_hash_bytes(rows: &KernelContentRows, anchors: &[CxId]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-domain-kernel-semantic-corpus-v2");
    match rows {
        KernelContentRows::Dense(rows) => {
            hasher.update(b"dense");
            let mut sorted = rows.iter().collect::<Vec<_>>();
            sorted.sort_unstable_by_key(|row| row.cx_id);
            for row in sorted {
                hasher.update(row.cx_id.to_bytes());
                hasher.update(row.vector.len().to_be_bytes());
                for value in &row.vector {
                    hasher.update(value.to_bits().to_be_bytes());
                }
            }
        }
        KernelContentRows::Sparse(index) => {
            hasher.update(b"sparse");
            let mut sorted = index.rows.iter().collect::<Vec<_>>();
            sorted.sort_unstable_by_key(|row| row.cx_id);
            for row in sorted {
                hasher.update(row.cx_id.to_bytes());
                hasher.update(row.entries.len().to_be_bytes());
                for entry in &row.entries {
                    hasher.update(entry.idx.to_be_bytes());
                    hasher.update(entry.val.to_bits().to_be_bytes());
                }
            }
        }
    }
    let mut sorted_anchors = anchors.to_vec();
    sorted_anchors.sort_unstable();
    for anchor in sorted_anchors {
        hasher.update(anchor.to_bytes());
    }
    hasher.finalize().into()
}

fn kernel_physical_contract(
    params: &SynapseCalyxKernelParams,
    rows: &KernelContentRows,
    anchors: &[CxId],
) -> [u8; 32] {
    let mut physical_contract = Sha256::new();
    physical_contract.update(b"synapse-domain-kernel-physical-contract-v2");
    physical_contract.update(params.panel_version.to_be_bytes());
    physical_contract.update(params.content_slot.to_be_bytes());
    physical_contract.update(params.max_records.to_be_bytes());
    physical_contract.update(params.knn.to_be_bytes());
    physical_contract.update(params.edge_cos_threshold.to_bits().to_be_bytes());
    physical_contract.update(params.math_execution_class.as_str().as_bytes());
    physical_contract.update(corpus_hash_bytes(rows, anchors));
    physical_contract.finalize().into()
}

fn verify_kernel_serving_identity(
    kernel: &Kernel,
    params: &SynapseCalyxKernelParams,
    rows: &KernelContentRows,
    anchors: &[CxId],
) -> Result<(), SynapseCalyxError> {
    let mut identity_readback = kernel.clone();
    let physical_contract = kernel_physical_contract(params, rows, anchors);
    let expected_kernel_id =
        seal_completed_kernel_identity(&mut identity_readback, &physical_contract)
            .map_err(|error| kernel_math_error("verify persisted kernel identity", &error))?;
    if expected_kernel_id == kernel.kernel_id {
        return Ok(());
    }
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_KERNEL_SERVING_CONTRACT_ID_MISMATCH",
        format!(
            "persisted kernel id {} does not match recomputed id {expected_kernel_id} for its durable graph/corpus contract",
            kernel.kernel_id
        ),
        "rebuild the persisted kernel; never serve an artifact whose id is not bound to its serving contract",
    ))
}

pub fn kernel_row_key(panel_version: u32, content_slot: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(KERNEL_ROW_PREFIX.len() + 6);
    key.extend_from_slice(KERNEL_ROW_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(&content_slot.to_be_bytes());
    key
}

pub fn kernel_math_error(action: &str, error: &LodestarError) -> SynapseCalyxError {
    SynapseCalyxError::new(
        error.code(),
        format!("{action}: {error}"),
        "inspect the panel embeddings, anchored set, and kernel parameters at the vault, then retry",
    )
}

/// Maps a substrate answer-derivation failure to a structured refusal that names
/// the grounding gap (no anchored node reachable / no path within the hop budget)
/// — the honesty gate's refusal branch, never a confabulated answer.
fn kernel_refusal(action: &str, error: &LodestarError) -> SynapseCalyxError {
    SynapseCalyxError::new(
        error.code(),
        format!("refused: {action}: {error}: the query is not grounded through the kernel"),
        "the query record does not reach a grounded anchor within the hop budget; widen the kernel/hops or accept the refusal — never a confabulated answer",
    )
}

fn paths_error(action: &str, error: &calyx_paths::PathsError) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_KERNEL_GRAPH",
        format!("{action}: kernel association graph failed: {error}"),
        "inspect the embedded concept set for duplicate or invalid ids before retrying",
    )
}
