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

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{Constellation, CxId, SlotId, SlotVector};
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
