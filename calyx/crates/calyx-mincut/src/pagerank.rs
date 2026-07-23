//! Deterministic weighted PageRank centrality over an [`AssocGraph`].
//!
//! Power iteration with dangling-mass redistribution and a uniform teleport
//! term. Node ids are iterated in the graph's canonical (id-sorted) index order
//! so the result is reproducible across runs with no RNG state — required by the
//! honesty/repro contract shared with [`crate::betweenness`].
//!
//! ## Pinned algorithmic choices (recorded for the frozen structural contract)
//! - **Damping `d = 0.85`** — the canonical Brin & Page value (Brin & Page 1998,
//!   "The Anatomy of a Large-Scale Hypertextual Web Search Engine"). Synapse's
//!   app-transition and process graphs are small (tens–hundreds of nodes); 0.85
//!   keeps the stationary distribution dominated by graph structure rather than
//!   teleport, and matches every reference implementation (NetworkX, igraph).
//! - **`max_iter = 100`, `tol = 1e-8`** — on graphs this small the L1 residual
//!   falls below 1e-8 in well under 100 iterations; the cap is a fail-safe, not
//!   the expected exit. Convergence is tested on the L1 delta between successive
//!   rank vectors, the standard stopping rule.

use std::collections::BTreeMap;

use calyx_core::CxId;
use calyx_paths::AssocGraph;

use crate::{MincutError, Result};

/// Canonical PageRank damping factor (Brin & Page 1998).
pub const PAGERANK_DEFAULT_DAMPING: f64 = 0.85;
/// Fail-safe iteration cap; small graphs converge far earlier.
pub const PAGERANK_DEFAULT_MAX_ITER: usize = 100;
/// L1 residual convergence tolerance between successive rank vectors.
pub const PAGERANK_DEFAULT_TOL: f64 = 1.0e-8;

/// Weighted PageRank with the pinned default damping/iteration/tolerance.
pub fn pagerank_default(graph: &AssocGraph) -> Result<BTreeMap<CxId, f64>> {
    pagerank(
        graph,
        PAGERANK_DEFAULT_DAMPING,
        PAGERANK_DEFAULT_MAX_ITER,
        PAGERANK_DEFAULT_TOL,
    )
}

/// Weighted PageRank centrality normalized so scores sum to 1.
///
/// Out-edge weights are treated as unnormalized transition affinities and are
/// row-normalized per source node. Nodes with no out-edges are dangling and
/// redistribute their mass uniformly (the standard "sink" correction).
pub fn pagerank(
    graph: &AssocGraph,
    damping: f64,
    max_iter: usize,
    tol: f64,
) -> Result<BTreeMap<CxId, f64>> {
    if graph.is_empty() {
        return Err(MincutError::BetweennessEmptyGraph);
    }
    if !damping.is_finite() || !(0.0..1.0).contains(&damping) {
        return Err(MincutError::lp_invalid(
            "pagerank damping must be finite in [0, 1)",
        ));
    }
    if max_iter == 0 {
        return Err(MincutError::lp_invalid("pagerank max_iter must be > 0"));
    }
    if !tol.is_finite() || tol <= 0.0 {
        return Err(MincutError::lp_invalid(
            "pagerank tol must be finite and > 0",
        ));
    }

    let n = graph.node_count();
    let inv_n = 1.0 / n as f64;

    // Per-source total out-weight for row normalization.
    let out_sum: Vec<f64> = (0..n)
        .map(|index| {
            graph
                .out_edges_by_index(index)
                .iter()
                .map(|edge| f64::from(edge.weight))
                .sum::<f64>()
        })
        .collect();

    let mut rank = vec![inv_n; n];
    for _ in 0..max_iter {
        let teleport = (1.0 - damping) * inv_n;
        let mut dangling = 0.0_f64;
        for index in 0..n {
            if out_sum[index] <= 0.0 {
                dangling += rank[index];
            }
        }
        let dangling_share = damping * dangling * inv_n;
        let mut next = vec![teleport + dangling_share; n];
        for index in 0..n {
            if out_sum[index] > 0.0 {
                let base = damping * rank[index] / out_sum[index];
                for edge in graph.out_edges_by_index(index) {
                    next[edge.dst] += base * f64::from(edge.weight);
                }
            }
        }
        let diff: f64 = rank
            .iter()
            .zip(&next)
            .map(|(before, after)| (before - after).abs())
            .sum();
        rank = next;
        if diff < tol {
            break;
        }
    }

    Ok((0..n)
        .map(|index| (graph.node_id(index).expect("pagerank node id"), rank[index]))
        .collect())
}
