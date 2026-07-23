//! Per-node structural signatures over an [`AssocGraph`].
//!
//! Aggregates the frozen centrality/degree/clustering primitives in this crate
//! into one deterministic [`StructuralSignature`] per node, plus a helper that
//! builds a directed transition graph (app→app focus transitions, process
//! parent/child edges, agent spawn edges) from aggregated `(src, dst, count)`
//! observations. The signature is the raw substrate that Synapse's
//! `syn-graphpos-*` frozen encoder lenses (#1685) consume; it carries no
//! encoding policy of its own.

use std::collections::BTreeMap;

use calyx_core::CxId;
use calyx_paths::AssocGraph;

use crate::{
    MincutError, Result, betweenness_auto, clustering::clustering_coefficients,
    eigenvector_centrality, pagerank::pagerank_default,
};

/// One directed, count-weighted transition observation between two nodes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransitionEdge {
    pub src: CxId,
    pub dst: CxId,
    /// Number of observed transitions src→dst; must be finite and > 0.
    pub count: f64,
}

/// Tunables for a structural-signature pass. [`Default`] pins the values used by
/// the frozen `syn-graphpos-*` panels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StructuralParams {
    /// Compute exact betweenness at or below this node count, else sampled.
    pub betweenness_exact_max_nodes: usize,
    /// Pivot count for the sampled betweenness estimator on larger graphs.
    pub betweenness_pivots: usize,
    /// Power-iteration cap for eigenvector centrality.
    pub eigenvector_max_iter: usize,
    /// Convergence tolerance for eigenvector centrality.
    pub eigenvector_tol: f32,
}

impl Default for StructuralParams {
    fn default() -> Self {
        Self {
            betweenness_exact_max_nodes: 2_048,
            betweenness_pivots: 256,
            eigenvector_max_iter: 256,
            eigenvector_tol: 1.0e-6,
        }
    }
}

/// Deterministic structural position of one node in a graph snapshot.
///
/// `betweenness`, `eigenvector`, `pagerank` and `clustering` are all normalized
/// to `[0, 1]`; degrees are raw counts. A central "hub" node measurably exceeds
/// a leaf node on the centrality fields — the #1685 acceptance probe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StructuralSignature {
    pub in_degree: usize,
    pub out_degree: usize,
    pub total_degree: usize,
    pub betweenness: f64,
    pub eigenvector: f64,
    pub pagerank: f64,
    pub clustering: f64,
}

/// Builds a directed transition graph from aggregated `(src, dst, count)` edges.
///
/// Duplicate `(src, dst)` pairs are summed. Node frequency weights are the total
/// observed occurrences (in + out). Edge weights are the pair counts rescaled
/// into `(0, 1]` by the global maximum pair count, so the strongest transition
/// has weight 1 and every edge stays within the [`AssocGraph`] weight contract
/// while preserving relative transition strength for shortest-path and PageRank.
pub fn build_transition_graph(edges: &[TransitionEdge]) -> Result<AssocGraph> {
    if edges.is_empty() {
        return Err(MincutError::lp_invalid(
            "build_transition_graph requires at least one edge",
        ));
    }
    let mut pair_counts = BTreeMap::<(CxId, CxId), f64>::new();
    let mut node_occurrences = BTreeMap::<CxId, f64>::new();
    for edge in edges {
        if !edge.count.is_finite() || edge.count <= 0.0 {
            return Err(MincutError::lp_invalid(
                "transition edge count must be finite and > 0",
            ));
        }
        *pair_counts.entry((edge.src, edge.dst)).or_default() += edge.count;
        *node_occurrences.entry(edge.src).or_default() += edge.count;
        *node_occurrences.entry(edge.dst).or_default() += edge.count;
    }
    let max_pair = pair_counts
        .values()
        .copied()
        .fold(0.0_f64, f64::max)
        .max(1.0);

    let mut builder = AssocGraph::builder();
    for (id, occurrences) in &node_occurrences {
        let weight = (*occurrences as f32).max(1.0);
        builder
            .add_node(*id, weight)
            .map_err(|error| MincutError::lp_invalid(error.to_string()))?;
    }
    for ((src, dst), count) in &pair_counts {
        let weight = ((count / max_pair) as f32).clamp(f32::MIN_POSITIVE, 1.0);
        builder
            .add_edge(*src, *dst, weight)
            .map_err(|error| MincutError::lp_invalid(error.to_string()))?;
    }
    Ok(builder.build())
}

/// Computes a [`StructuralSignature`] for every node in the snapshot.
pub fn structural_signatures(
    graph: &AssocGraph,
    params: StructuralParams,
) -> Result<BTreeMap<CxId, StructuralSignature>> {
    if graph.is_empty() {
        return Err(MincutError::BetweennessEmptyGraph);
    }
    let n = graph.node_count();

    let betweenness = betweenness_auto(
        graph,
        params.betweenness_exact_max_nodes,
        params.betweenness_pivots,
    )?;
    let pagerank = pagerank_default(graph)?;
    let clustering = clustering_coefficients(graph);
    let eigenvector: BTreeMap<CxId, f64> = if n < 2 {
        graph.node_ids().map(|id| (id, 0.0)).collect()
    } else {
        eigenvector_centrality(graph, params.eigenvector_max_iter, params.eigenvector_tol)
            .map_err(|error| MincutError::lp_invalid(error.to_string()))?
            .into_iter()
            .map(|(id, value)| (id, f64::from(value)))
            .collect()
    };

    let mut signatures = BTreeMap::new();
    for index in 0..n {
        let id = graph.node_id(index).expect("structural node id");
        let in_degree = graph
            .in_degree(id)
            .map_err(|error| MincutError::lp_invalid(error.to_string()))?;
        let out_degree = graph.out_edges_by_index(index).len();
        signatures.insert(
            id,
            StructuralSignature {
                in_degree,
                out_degree,
                total_degree: in_degree + out_degree,
                betweenness: betweenness.get(&id).copied().unwrap_or(0.0),
                eigenvector: eigenvector.get(&id).copied().unwrap_or(0.0),
                pagerank: pagerank.get(&id).copied().unwrap_or(0.0),
                clustering: clustering.get(&id).copied().unwrap_or(0.0),
            },
        );
    }
    Ok(signatures)
}
