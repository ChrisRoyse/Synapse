use std::collections::BTreeSet;

use calyx_core::{CxId, PanelSlotId};
use calyx_paths::AssocGraph;
use serde::{Deserialize, Serialize};

use crate::{LodestarError, LoomAssocEdgeProvenance, Result, groundedness_distance};

pub const LOOM_WEAVE_REPORT_SCHEMA_VERSION: u32 = 2;
const DEFAULT_MIN_GROUNDEDNESS_FRACTION: f32 = 0.000_001;
const DEFAULT_MAX_TOP_EDGES: usize = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoomWeaveReportParams {
    pub max_groundedness_distance: usize,
    pub min_groundedness_fraction: f32,
    pub max_top_edges: usize,
}

impl Default for LoomWeaveReportParams {
    fn default() -> Self {
        Self {
            max_groundedness_distance: 3,
            min_groundedness_fraction: DEFAULT_MIN_GROUNDEDNESS_FRACTION,
            max_top_edges: DEFAULT_MAX_TOP_EDGES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoomWeaveReport {
    pub schema_version: u32,
    pub node_count: usize,
    pub edge_count: usize,
    pub provenance_count: usize,
    pub unique_xterm_count: usize,
    pub anchor_count: usize,
    pub grounded_node_count: usize,
    pub groundedness_fraction: f32,
    pub min_groundedness_fraction: f32,
    pub gate_passed: bool,
    pub graph_density: f32,
    pub top_edges: Vec<LoomWeaveEdgeReadback>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoomWeaveEdgeReadback {
    pub src: CxId,
    pub dst: CxId,
    pub xterm_cx: CxId,
    pub src_slot: PanelSlotId,
    pub dst_slot: PanelSlotId,
    pub raw_agreement: f32,
    pub agreement: f32,
    pub directional_confidence: f32,
    pub edge_weight: f32,
}

pub fn loom_weave_report(
    graph: &AssocGraph,
    provenance: &[LoomAssocEdgeProvenance],
    anchors: &[CxId],
    params: &LoomWeaveReportParams,
) -> Result<LoomWeaveReport> {
    validate_inputs(graph, provenance, params)?;
    let grounded_node_count = grounded_node_count(graph, anchors, params)?;
    let groundedness_fraction = grounded_node_count as f32 / graph.node_count() as f32;
    let graph_density = graph_density(graph.node_count(), graph.edge_count());
    let unique_xterm_count = provenance
        .iter()
        .map(|entry| entry.xterm_cx)
        .collect::<BTreeSet<_>>()
        .len();

    Ok(LoomWeaveReport {
        schema_version: LOOM_WEAVE_REPORT_SCHEMA_VERSION,
        node_count: graph.node_count(),
        edge_count: graph.edge_count(),
        provenance_count: provenance.len(),
        unique_xterm_count,
        anchor_count: anchors.len(),
        grounded_node_count,
        groundedness_fraction,
        min_groundedness_fraction: params.min_groundedness_fraction,
        gate_passed: groundedness_fraction >= params.min_groundedness_fraction,
        graph_density,
        top_edges: top_edges(provenance, params.max_top_edges),
    })
}

fn validate_inputs(
    graph: &AssocGraph,
    provenance: &[LoomAssocEdgeProvenance],
    params: &LoomWeaveReportParams,
) -> Result<()> {
    if graph.is_empty() {
        return Err(LodestarError::KernelEmptyGraph);
    }
    if provenance.is_empty() {
        return invalid_params("loom weave report requires edge provenance");
    }
    if params.max_groundedness_distance == 0 {
        return invalid_params("max_groundedness_distance must be greater than zero");
    }
    if !params.min_groundedness_fraction.is_finite()
        || !(0.0..=1.0).contains(&params.min_groundedness_fraction)
    {
        return invalid_params("min_groundedness_fraction must be finite and in [0,1]");
    }
    if params.max_top_edges == 0 {
        return invalid_params("max_top_edges must be greater than zero");
    }
    Ok(())
}

fn grounded_node_count(
    graph: &AssocGraph,
    anchors: &[CxId],
    params: &LoomWeaveReportParams,
) -> Result<usize> {
    graph
        .node_ids()
        .map(|id| {
            groundedness_distance(graph, id, anchors, params.max_groundedness_distance)
                .map(|distance| usize::from(distance.is_some()))
        })
        .try_fold(0_usize, |sum, item| item.map(|value| sum + value))
}

fn graph_density(node_count: usize, edge_count: usize) -> f32 {
    let max_edges = node_count.saturating_mul(node_count.saturating_sub(1));
    if max_edges == 0 {
        0.0
    } else {
        edge_count as f32 / max_edges as f32
    }
}

fn top_edges(
    provenance: &[LoomAssocEdgeProvenance],
    max_edges: usize,
) -> Vec<LoomWeaveEdgeReadback> {
    let mut edges = provenance.to_vec();
    edges.sort_by(|left, right| {
        right
            .edge_weight
            .total_cmp(&left.edge_weight)
            .then_with(|| left.src_cx.cmp(&right.src_cx))
            .then_with(|| left.dst_cx.cmp(&right.dst_cx))
            .then_with(|| left.src_slot.cmp(&right.src_slot))
            .then_with(|| left.dst_slot.cmp(&right.dst_slot))
    });
    edges
        .into_iter()
        .take(max_edges)
        .map(|entry| LoomWeaveEdgeReadback {
            src: entry.src_cx,
            dst: entry.dst_cx,
            xterm_cx: entry.xterm_cx,
            src_slot: entry.src_slot,
            dst_slot: entry.dst_slot,
            raw_agreement: entry.raw_agreement,
            agreement: entry.agreement,
            directional_confidence: entry.directional_confidence,
            edge_weight: entry.edge_weight,
        })
        .collect()
}

fn invalid_params<T>(detail: impl Into<String>) -> Result<T> {
    Err(LodestarError::KernelInvalidParams {
        detail: detail.into(),
    })
}
