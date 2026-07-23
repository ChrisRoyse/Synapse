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

use calyx_assay::{
    AssayCacheKey, AssayStore, AssaySubject, ChangePointReport, CusumReport, DEFAULT_TE_LAGS,
    Direction, EstimatorKind, InterEventHazardReport, MiEstimate, MmdConfig, PeriodogramConfig,
    RateShift, SIGNIFICANT_PEAK_FAP, SlotAttribution, TEResult, TrustTag, autocorrelation,
    bin_event_counts, bits_report_with_anchor, entropy_bits, inter_event_hazard_with_alpha,
    ksg_mi_continuous_discrete, lomb_scargle_with_config, mmd_change_point,
    panel_sufficiency_with_anchor, partitioned_histogram_nmi, per_sensor_attribution,
    recurrence_rate_cusum, stable_rank, transfer_entropy_sweep,
};
use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{
    Anchor, AnchorKind, AnchorValue, Constellation, CxId, SlotId, SlotVector, SystemClock, Ts,
};
use calyx_forge::{Backend, KnnMetric};
use calyx_loom::{
    AbundanceReport, CeilingEstimate, LoomStore, MaterializationAction, NeffEstimate,
    StaticPairGainGate, plan_cross_terms,
};
use serde::{Deserialize, Serialize};

use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

/// Hard cap on records scanned per weave pass to bound one bounded,
/// pressure-aware maintenance operation. The caller may request fewer.
pub const SYNAPSE_INTELLIGENCE_MAX_RECORDS: usize = 20_000;
/// Default k for the between-record nearest-neighbor graph.
pub const SYNAPSE_KNN_DEFAULT_K: usize = 8;
/// Global cap on persisted between-record kNN edges per weave pass.
pub const SYNAPSE_KNN_MAX_EDGES: usize = 200_000;

const GRAPH_AGREEMENT_PREFIX: &[u8; 5] = b"GAGR1";
const GRAPH_KNN_PREFIX: &[u8; 5] = b"GKNN1";

/// Bounded request describing one Loom weave pass over a panel.
#[derive(Clone, Copy, Debug)]
pub struct SynapseCalyxWeaveParams {
    pub panel_version: u32,
    pub max_records: usize,
    pub knn_k: usize,
    pub cache_capacity: usize,
}

impl SynapseCalyxWeaveParams {
    #[must_use]
    pub const fn new(panel_version: u32) -> Self {
        Self {
            panel_version,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            knn_k: SYNAPSE_KNN_DEFAULT_K,
            cache_capacity: 4_096,
        }
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
    pub n_lenses: usize,
    pub n_constellations: usize,
    pub c_n2_upper_bound: usize,
    pub materialized: usize,
    pub measured_count: usize,
    pub derived_count: usize,
    pub meaning_compression_yield: f32,
    pub n_eff: SynapseCalyxNeffEstimate,
    pub dpi_ceiling_bits: Option<f32>,
    pub dpi_ceiling_provisional: bool,
    pub xterm_cf_rows: usize,
    pub graph_cf_rows: usize,
}

/// Result of one bounded Loom weave pass with physical CF readbacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxWeaveReport {
    pub panel_version: u32,
    pub records_scanned: usize,
    pub records_woven: usize,
    pub n_lenses: usize,
    pub cross_terms_materialized: usize,
    pub agreement_edges_persisted: usize,
    pub between_record_edges_persisted: usize,
    pub xterm_cf_rows_after: usize,
    pub graph_cf_rows_after: usize,
    pub agreement_edges: Vec<SynapseCalyxAgreementEdge>,
    pub abundance: SynapseCalyxAbundanceReport,
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
        // Hot-path boundary (#1686): live Loom weave is an off-runtime
        // intelligence computation and must never be driven from a tagged tick.
        crate::lowering::hot_context::assert_cold_calyx("weave_panel");
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;
        let records_scanned = corpus.records_scanned;

        let mut store = LoomStore::new(params.cache_capacity.max(1));
        // interaction eager iff pair-gain >= bit floor; without grounded anchors
        // in this pass the gain is unmeasured, so the honest gate keeps
        // interaction lazy (0.0 < floor) and only agreement is eager.
        let gate = StaticPairGainGate { gain_bits: 0.0 };
        let mut records_woven = 0usize;
        let mut lens_ids: BTreeSet<SlotId> = BTreeSet::new();
        let mut measured_slot_instances = 0usize;
        for record in &corpus.records {
            for slot in record.slots.keys() {
                lens_ids.insert(*slot);
            }
            measured_slot_instances += record.slots.len();
            if record.slots.len() < 2 {
                // A single-slot record still contributes its measured slots to
                // the panel, but has no within-record cross-term to weave.
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
            store
                .materialize_plan(params.panel_version, record.cx_id, &record.slots, &plan)
                .map_err(|error| {
                    loom_math_error("materialize within-record cross-terms", &error)
                })?;
            records_woven += 1;
        }

        let cross_terms_materialized = store.xterm_count();
        let xterm_rows = store
            .xterm_kv_rows()
            .map_err(|error| loom_math_error("encode XTerm rows", &error))?;
        let agreement_graph = store
            .agreement_graph()
            .map_err(|error| loom_math_error("aggregate agreement graph", &error))?;

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

        let between_record_edges = self.build_between_record_edges(&corpus, params.knn_k)?;
        for edge in &between_record_edges {
            writes.push(SynapseCalyxCfWrite {
                cf: ColumnFamily::Graph,
                key: between_record_edge_key(edge),
                value: encode_json(edge)?,
            });
        }

        if !writes.is_empty() {
            self.write_cf_batch(writes)?;
            self.flush()?;
        }

        let xterm_cf_rows_after = self.scan_cf_latest(ColumnFamily::XTerm)?.len();
        let graph_cf_rows_after = self.scan_cf_latest(ColumnFamily::Graph)?.len();

        let abundance = build_abundance_report(
            params.panel_version,
            lens_ids.len(),
            corpus.records.len(),
            cross_terms_materialized,
            measured_slot_instances,
            cross_terms_materialized,
            xterm_cf_rows_after,
            graph_cf_rows_after,
        );

        Ok(SynapseCalyxWeaveReport {
            panel_version: params.panel_version,
            records_scanned,
            records_woven,
            n_lenses: lens_ids.len(),
            cross_terms_materialized,
            agreement_edges_persisted: agreement_edges.len(),
            between_record_edges_persisted: between_record_edges.len(),
            xterm_cf_rows_after,
            graph_cf_rows_after,
            agreement_edges,
            abundance,
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
        let xterm_cf_rows = self.scan_cf_latest(ColumnFamily::XTerm)?.len();
        let graph_cf_rows = self.scan_cf_latest(ColumnFamily::Graph)?.len();
        Ok(build_abundance_report(
            panel_version,
            lens_ids.len(),
            corpus.records.len(),
            xterm_cf_rows,
            measured_slot_instances,
            xterm_cf_rows,
            xterm_cf_rows,
            graph_cf_rows,
        ))
    }

    /// Scans the `Base` CF once and returns the dense-slot corpus for a panel.
    fn load_panel_dense_corpus(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<DenseCorpus, SynapseCalyxError> {
        let rows = self.scan_cf_latest(ColumnFamily::Base)?;
        let mut records = Vec::new();
        let mut records_scanned = 0usize;
        for (_, value) in rows {
            let constellation = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if constellation.panel_version != panel_version {
                continue;
            }
            records_scanned += 1;
            if records.len() >= max_records {
                continue;
            }
            records.push(DenseRecord::from_constellation(&constellation));
        }
        Ok(DenseCorpus {
            records,
            records_scanned,
        })
    }

    /// Builds the between-record nearest-neighbor graph: for every dense slot
    /// with at least two records of a uniform dimension, a bounded cosine kNN
    /// graph over the records that carry that slot.
    fn build_between_record_edges(
        &self,
        corpus: &DenseCorpus,
        knn_k: usize,
    ) -> Result<Vec<SynapseCalyxBetweenRecordEdge>, SynapseCalyxError> {
        let knn_k = knn_k.clamp(1, 64);
        let backend = self.math_runtime.backend();
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
                if dim == 0 || group.len() < 2 {
                    continue;
                }
                append_slot_knn_edges(backend, slot, dim, &group, knn_k, &mut edges)?;
                if edges.len() >= SYNAPSE_KNN_MAX_EDGES {
                    break;
                }
            }
        }
        edges.truncate(SYNAPSE_KNN_MAX_EDGES);
        Ok(edges)
    }
}

/// One record reduced to its dense slot vectors (sparse/multi/absent slots are
/// excluded: cosine/KSG over huge sparse lenses is impractical and their signal
/// is served by the search/BM25 path, not the association engine).
struct DenseRecord {
    cx_id: CxId,
    slots: BTreeMap<SlotId, Vec<f32>>,
    anchors: Vec<Anchor>,
}

impl DenseRecord {
    fn from_constellation(constellation: &Constellation) -> Self {
        let slots = constellation
            .slots
            .iter()
            .filter_map(|(slot, vector)| dense_vector(vector).map(|dense| (*slot, dense)))
            .collect();
        Self {
            cx_id: constellation.cx_id,
            slots,
            anchors: constellation.anchors.clone(),
        }
    }
}

struct DenseCorpus {
    records: Vec<DenseRecord>,
    records_scanned: usize,
}

fn dense_vector(vector: &SlotVector) -> Option<Vec<f32>> {
    match vector {
        SlotVector::Dense { data, .. } => Some(data.clone()),
        SlotVector::Sparse { .. } | SlotVector::Multi { .. } | SlotVector::Absent { .. } => None,
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

#[allow(clippy::too_many_arguments)]
fn build_abundance_report(
    panel_version: u32,
    n_lenses: usize,
    n_constellations: usize,
    materialized: usize,
    measured_count: usize,
    derived_count: usize,
    xterm_cf_rows: usize,
    graph_cf_rows: usize,
) -> SynapseCalyxAbundanceReport {
    // The DPI ceiling and effective rank require grounded bits; without an
    // anchored assay pass they are reported provisional (honest, never faked).
    let report = AbundanceReport::new(
        n_lenses,
        n_constellations,
        materialized,
        NeffEstimate::Provisional { value: 0.0 },
        CeilingEstimate::Provisional { bits: 0.0 },
        measured_count,
        derived_count,
    );
    SynapseCalyxAbundanceReport {
        panel_version,
        n_lenses: report.n_lenses,
        n_constellations: report.n_constellations,
        c_n2_upper_bound: report.c_n2_upper_bound,
        materialized: report.materialized,
        measured_count: report.measured_count,
        derived_count: report.derived_count,
        meaning_compression_yield: report.meaning_compression_yield,
        n_eff: neff_estimate(&report.n_eff),
        dpi_ceiling_bits: None,
        dpi_ceiling_provisional: matches!(report.dpi_ceiling, CeilingEstimate::Provisional { .. }),
        xterm_cf_rows,
        graph_cf_rows,
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

const ASSAY_CORPUS_SHARD: &str = "synapse-intelligence";

/// Bounded request describing one Assay bits/sufficiency/redundancy pass.
#[derive(Clone, Debug)]
pub struct SynapseCalyxAssayParams {
    pub panel_version: u32,
    /// Grounded outcome anchor to measure bits about. Synapse writes outcome
    /// anchors as `AnchorKind::Label(<name>)`; a few canonical names map to the
    /// native kinds.
    pub anchor_kind: String,
    pub max_records: usize,
    pub ksg_k: usize,
}

impl SynapseCalyxAssayParams {
    #[must_use]
    pub const fn new(panel_version: u32, anchor_kind: String) -> Self {
        Self {
            panel_version,
            anchor_kind,
            max_records: SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            ksg_k: SYNAPSE_KSG_DEFAULT_K,
        }
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
}

/// Result of one Assay bits pass with the physical Assay CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxBitsReport {
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: usize,
    pub distinct_outcomes: usize,
    pub total_bits: f32,
    pub grounded: bool,
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
pub struct SynapseCalyxSufficiencyReport {
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: usize,
    pub joint_records: usize,
    pub panel_bits: f32,
    pub anchor_entropy_bits: f32,
    pub sufficient: bool,
    pub deficit_bits: f32,
    pub grounded: bool,
    pub deficits: Vec<SynapseCalyxSufficiencyDeficit>,
    pub assay_cf_rows_after: usize,
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

/// Result of one Assay redundancy / effective-rank pass with Assay readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRedundancyReport {
    pub panel_version: u32,
    pub n_lenses: usize,
    pub records_scanned: usize,
    pub effective_rank: f32,
    pub pairs_evaluated: usize,
    pub redundant_pairs: Vec<SynapseCalyxRedundancyPair>,
    pub assay_cf_rows_after: usize,
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
    pub fn assay_bits(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> Result<SynapseCalyxBitsReport, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let corpus = self.load_panel_dense_corpus(params.panel_version, max_records)?;
        let anchor_kind = parse_anchor_kind(&params.anchor_kind);
        let gathered = gather_anchored_slot_samples(&corpus, &anchor_kind);

        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let ksg_k = params.ksg_k.max(1);
        let mut slots = Vec::new();
        let mut slot_bits: Vec<(SlotId, f32)> = Vec::new();
        for (slot, samples) in &gathered.by_slot {
            if samples.x.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                slots.push(SynapseCalyxSlotBits {
                    slot: slot.get(),
                    marginal_bits: 0.0,
                    ci_low: 0.0,
                    ci_high: 0.0,
                    n_samples: samples.x.len(),
                    sole_carrier: false,
                    provisional: true,
                });
                continue;
            }
            let k = ksg_k.min(samples.x.len().saturating_sub(1)).max(1);
            let estimate = ksg_mi_continuous_discrete(&samples.x, &samples.labels, k)
                .map_err(|error| loom_math_error("estimate KSG lens bits", &error))?;
            slot_bits.push((*slot, estimate.bits));
            store.put(
                AssayCacheKey::scoped(
                    params.panel_version,
                    ASSAY_CORPUS_SHARD,
                    vault_id,
                    anchor_kind.clone(),
                ),
                AssaySubject::Lens { slot: *slot },
                estimate.clone(),
                "synapse-assay-bits",
                seq,
            );
            slots.push(SynapseCalyxSlotBits {
                slot: slot.get(),
                marginal_bits: estimate.bits,
                ci_low: estimate.ci_low,
                ci_high: estimate.ci_high,
                n_samples: estimate.n_samples,
                sole_carrier: false,
                provisional: false,
            });
        }

        let attributions = per_sensor_attribution(&slot_bits, SYNAPSE_ASSAY_BIT_FLOOR);
        mark_sole_carriers(&mut slots, &attributions);
        let grounded = gathered.representative.as_ref().is_some_and(|anchor| {
            matches!(
                bits_report_with_anchor(attributions.clone(), anchor).trust,
                TrustTag::Trusted
            )
        });
        let total_bits = slot_bits.iter().map(|(_, bits)| *bits).sum();

        let assay_cf_rows_after = self.persist_assay_store(&store)?;
        Ok(SynapseCalyxBitsReport {
            panel_version: params.panel_version,
            anchor_kind: params.anchor_kind.clone(),
            anchored_records: gathered.anchored_records,
            distinct_outcomes: gathered.distinct_outcomes,
            total_bits,
            grounded,
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
        let gathered = gather_anchored_slot_samples(&corpus, &anchor_kind);

        // Per-slot attributions feed the deficit split; the joint panel bits are
        // the sufficiency numerator.
        let ksg_k = params.ksg_k.max(1);
        let mut slot_bits: Vec<(SlotId, f32)> = Vec::new();
        for (slot, samples) in &gathered.by_slot {
            if samples.x.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                slot_bits.push((*slot, 0.0));
                continue;
            }
            let k = ksg_k.min(samples.x.len().saturating_sub(1)).max(1);
            let estimate = ksg_mi_continuous_discrete(&samples.x, &samples.labels, k)
                .map_err(|error| loom_math_error("estimate KSG lens bits", &error))?;
            slot_bits.push((*slot, estimate.bits));
        }
        let attributions = per_sensor_attribution(&slot_bits, SYNAPSE_ASSAY_BIT_FLOOR);

        let joint = build_joint_samples(&corpus, &anchor_kind);
        let anchor_entropy_bits = entropy_bits(&joint.labels);
        let joint_records = joint.labels.len();
        let panel_bits = if joint_records >= SYNAPSE_ASSAY_MIN_SAMPLES {
            let k = ksg_k.min(joint_records.saturating_sub(1)).max(1);
            ksg_mi_continuous_discrete(&joint.x, &joint.labels, k)
                .map_err(|error| loom_math_error("estimate KSG panel joint bits", &error))?
                .bits
        } else {
            0.0
        };

        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let cache_key = AssayCacheKey::scoped(
            params.panel_version,
            ASSAY_CORPUS_SHARD,
            vault_id,
            anchor_kind,
        );
        let trust =
            if joint_records >= SYNAPSE_ASSAY_MIN_SAMPLES && gathered.representative.is_some() {
                TrustTag::Trusted
            } else {
                TrustTag::Provisional
            };
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
        let assay_cf_rows_after = self.persist_assay_store(&store)?;

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
                anchor_entropy_bits,
                sufficient: sufficiency.sufficient,
                deficit_bits: sufficiency.deficit_bits,
                grounded: matches!(trust, TrustTag::Trusted),
                deficits,
                assay_cf_rows_after,
            });
        }
        Ok(SynapseCalyxSufficiencyReport {
            panel_version: params.panel_version,
            anchor_kind: params.anchor_kind.clone(),
            anchored_records: gathered.anchored_records,
            joint_records,
            panel_bits,
            anchor_entropy_bits,
            sufficient: panel_bits >= anchor_entropy_bits,
            deficit_bits: (anchor_entropy_bits - panel_bits).max(0.0),
            grounded: false,
            deficits: Vec::new(),
            assay_cf_rows_after,
        })
    }

    /// Measures pairwise lens redundancy (normalized MI over deterministic
    /// random-projection sketches) and the panel effective rank (stable rank of
    /// the redundancy matrix), persists redundant pairs to the Assay CF, and
    /// reads the CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the corpus cannot be read,
    /// the NMI/effective-rank math fails closed, or the Assay write/readback
    /// fails.
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
                    .insert(index, project_scalar(slot.get(), vector));
            }
        }
        let slot_ids: Vec<SlotId> = sketches.keys().copied().collect();
        let n_lenses = slot_ids.len();

        let mut matrix = vec![vec![0.0_f32; n_lenses]; n_lenses];
        for (index, row) in matrix.iter_mut().enumerate() {
            row[index] = 1.0;
        }
        let mut store = AssayStore::default();
        let vault_id = self.vault_id_value();
        let seq = self.latest_seq();
        let mut redundant_pairs = Vec::new();
        let mut pairs_evaluated = 0usize;
        for i in 0..n_lenses {
            for j in (i + 1)..n_lenses {
                let (paired_a, paired_b) =
                    paired_sketches(&sketches[&slot_ids[i]], &sketches[&slot_ids[j]]);
                if paired_a.len() < SYNAPSE_ASSAY_MIN_SAMPLES {
                    continue;
                }
                let report =
                    partitioned_histogram_nmi(&paired_a, &paired_b, SYNAPSE_REDUNDANCY_NMI_BINS)
                        .map_err(|error| {
                            loom_math_error("estimate pairwise redundancy NMI", &error)
                        })?;
                pairs_evaluated += 1;
                let nmi = report.nmi.clamp(0.0, 1.0);
                matrix[i][j] = nmi;
                matrix[j][i] = nmi;
                let redundant = report.nmi >= SYNAPSE_ASSAY_CORRELATION_CEILING;
                if redundant {
                    store.put(
                        AssayCacheKey::scoped(
                            params.panel_version,
                            ASSAY_CORPUS_SHARD,
                            vault_id,
                            AnchorKind::Reward,
                        ),
                        AssaySubject::Pair {
                            a: slot_ids[i],
                            b: slot_ids[j],
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
                        slot_a: slot_ids[i].get(),
                        slot_b: slot_ids[j].get(),
                        nmi: report.nmi,
                        mi_bits: report.mi_bits,
                        n_samples: paired_a.len(),
                        redundant,
                    });
                }
            }
        }
        let effective_rank = stable_rank(&matrix)
            .map_err(|error| loom_math_error("compute effective rank", &error))?
            .n_eff;
        let assay_cf_rows_after = self.persist_assay_store(&store)?;
        Ok(SynapseCalyxRedundancyReport {
            panel_version: params.panel_version,
            n_lenses,
            records_scanned: corpus.records_scanned,
            effective_rank,
            pairs_evaluated,
            redundant_pairs,
            assay_cf_rows_after,
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
        Ok(self.scan_cf_latest(ColumnFamily::Assay)?.len())
    }
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

fn gather_anchored_slot_samples(
    corpus: &DenseCorpus,
    anchor_kind: &AnchorKind,
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

/// Builds the joint panel samples for sufficiency: the concatenation of the
/// slots present in every anchored record (the coverage intersection), so the
/// joint feature vector has one consistent dimension.
fn build_joint_samples(corpus: &DenseCorpus, anchor_kind: &AnchorKind) -> JointAnchoredSamples {
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
    let required = required.unwrap_or_default();
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
fn project_scalar(slot: u16, vector: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for (index, value) in vector.iter().enumerate() {
        let seed = splitmix64(
            (u64::from(slot) << 40) ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
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
/// Cap on reported periodogram peaks.
pub const SYNAPSE_TEMPORAL_MAX_PEAKS: usize = 4;

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
    pub fn new(panel_version: u32) -> Self {
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
/// with the physical TemporalXTerm CF readback.
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

/// CUSUM + MMD drift result with the physical TemporalXTerm CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxDriftReport {
    pub panel_version: u32,
    pub filter_value: Option<String>,
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

/// Gamma-renewal overdue-hazard result with the physical TemporalXTerm CF
/// readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxHazardReport {
    pub panel_version: u32,
    pub filter_value: Option<String>,
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
        let (stream_a, stream_b) = paired_binned_streams(&a_times, &b_times, bin);
        let n_bins = stream_a.len();
        let mut lags: Vec<usize> = DEFAULT_TE_LAGS
            .iter()
            .copied()
            .filter(|lag| *lag <= params.max_lag.max(1))
            .collect();
        if lags.is_empty() {
            lags.push(1);
        }
        let clock = SystemClock;
        let results = transfer_entropy_sweep(&stream_a, &stream_b, &lags, &clock);
        let best = choose_dominant_te(&results).ok_or_else(|| {
            temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_TE_UNRESOLVED",
                "transfer-entropy sweep returned no usable lag (all provisional or errored)",
                "increase the binned series length (more occurrences or a smaller bin) so a lag reaches quorum",
            )
        })?;

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
        });
        self.persist_temporal_row(ColumnFamily::Graph, key, &out_edge)?;
        let graph_cf_rows_after = self.scan_cf_latest(ColumnFamily::Graph)?.len();

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
            lags: results.iter().map(causality_lag).collect(),
            graph_cf_rows_after,
        })
    }

    /// Runs the Lomb-Scargle GLS periodogram (with permutation false-alarm
    /// probability) and a slotted-autocorrelation cross-check over the panel's
    /// occurrence-count series, persists the result to the native TemporalXTerm
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
            TEMPORAL_PERIODICITY_PREFIX,
            params.panel_version,
            params.filter_value.as_deref(),
        );
        self.persist_temporal_row(ColumnFamily::TemporalXTerm, key, &out)?;
        let temporal_xterm_cf_rows_after = self.scan_cf_latest(ColumnFamily::TemporalXTerm)?.len();

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
    /// persists the result to the native TemporalXTerm CF, and reads it back.
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
        let times = self.filtered_event_times(params)?;
        let cusum: CusumReport = recurrence_rate_cusum(&times)
            .map_err(|error| loom_math_error("CUSUM rate change-point", &error))?;

        let bin = validated_bin_seconds(params.bin_seconds)?;
        let mmd = match bin_event_counts(&times, bin) {
            Ok((_, counts)) => temporal_mmd_change_point(&counts),
            Err(_) => None,
        };

        let change = cusum.change_point;
        let out = serde_json::json!({
            "panel_version": params.panel_version,
            "filter_value": params.filter_value,
            "n_gaps": cusum.n_gaps,
            "baseline_mean_gap": cusum.baseline_mean_gap,
            "baseline_sigma": cusum.baseline_sigma,
            "cusum_change_detected": change.is_some(),
            "cusum_change_index": change.map(|point| point.occurrence_index),
            "cusum_direction": change.map(|point| rate_shift_label(point.direction)),
            "mmd_p_value": mmd.as_ref().map(|report| report.report.p_value),
        });
        let key = temporal_report_key(
            TEMPORAL_DRIFT_PREFIX,
            params.panel_version,
            params.filter_value.as_deref(),
        );
        self.persist_temporal_row(ColumnFamily::TemporalXTerm, key, &out)?;
        let temporal_xterm_cf_rows_after = self.scan_cf_latest(ColumnFamily::TemporalXTerm)?.len();

        Ok(SynapseCalyxDriftReport {
            panel_version: params.panel_version,
            filter_value: params.filter_value.clone(),
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
    /// survival at `now`, persists the result to the native TemporalXTerm CF, and
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
        let times = self.filtered_event_times(params)?;
        let last = *times.last().ok_or_else(|| {
            temporal_error(
                "SYNAPSE_CALYX_TEMPORAL_INSUFFICIENT_EVENTS",
                "the occurrence series is empty; overdue hazard needs a recent occurrence",
                "capture more occurrences for this stream before requesting overdue hazard",
            )
        })?;
        let now = resolve_now_secs(params.now_secs, self.clock_now_ms().ok(), last);
        let report: InterEventHazardReport =
            inter_event_hazard_with_alpha(&times, now, params.overdue_alpha)
                .map_err(|error| loom_math_error("inter-event overdue hazard", &error))?;

        let out = serde_json::json!({
            "panel_version": params.panel_version,
            "filter_value": params.filter_value,
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
            TEMPORAL_HAZARD_PREFIX,
            params.panel_version,
            params.filter_value.as_deref(),
        );
        self.persist_temporal_row(ColumnFamily::TemporalXTerm, key, &out)?;
        let temporal_xterm_cf_rows_after = self.scan_cf_latest(ColumnFamily::TemporalXTerm)?.len();

        Ok(SynapseCalyxHazardReport {
            panel_version: params.panel_version,
            filter_value: params.filter_value.clone(),
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
        let rows = self.scan_cf_latest(ColumnFamily::Base)?;
        let mut records = Vec::new();
        for (_, value) in rows {
            let constellation = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if constellation.panel_version != params.panel_version {
                continue;
            }
            let Some(secs) = constellation.source_event_time_secs() else {
                continue;
            };
            let group =
                group_key.and_then(|key| constellation.metadata_value(key).map(str::to_owned));
            records.push(EventRecord {
                secs: secs as f64,
                group,
            });
            if records.len() >= max_records {
                break;
            }
        }
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

/// Builds two aligned integer-bin count streams over the union time range so the
/// transfer-entropy lag operates on consecutive bins (value 0 bins are kept so
/// the estimator's history lookups never straddle a gap).
fn paired_binned_streams(
    a_times: &[f64],
    b_times: &[f64],
    bin: f64,
) -> (Vec<(u64, f32)>, Vec<(u64, f32)>) {
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
        return (Vec::new(), Vec::new());
    }
    let n_bins = (((max_t - min_t) / bin).floor() as usize) + 1;
    let mut a_counts = vec![0.0_f32; n_bins];
    let mut b_counts = vec![0.0_f32; n_bins];
    for &time in a_times {
        let index = (((time - min_t) / bin).floor() as usize).min(n_bins - 1);
        a_counts[index] += 1.0;
    }
    for &time in b_times {
        let index = (((time - min_t) / bin).floor() as usize).min(n_bins - 1);
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
    (stream_a, stream_b)
}

/// Picks the transfer-entropy lag with the largest absolute directed asymmetry
/// among non-provisional results, preferring a resolved direction.
fn choose_dominant_te(results: &[TEResult]) -> Option<TEResult> {
    results
        .iter()
        .filter(|result| !result.provisional && result.error_code.is_none())
        .max_by(|left, right| {
            (left.t_a_to_b - left.t_b_to_a)
                .abs()
                .total_cmp(&(right.t_a_to_b - right.t_b_to_a).abs())
        })
        .or_else(|| results.iter().find(|result| result.error_code.is_none()))
        .cloned()
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

fn temporal_report_key(prefix: &[u8; 5], panel_version: u32, filter: Option<&str>) -> Vec<u8> {
    let filter = filter.unwrap_or("");
    let mut key = Vec::with_capacity(prefix.len() + 4 + filter.len());
    key.extend_from_slice(prefix);
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
