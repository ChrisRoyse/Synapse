//! Deterministic local clustering coefficient over the undirected projection of
//! an [`AssocGraph`].
//!
//! For a node `u` with neighbour set `N(u)` (union of in- and out-neighbours,
//! excluding `u` itself), the clustering coefficient is the fraction of the
//! `k*(k-1)/2` possible undirected neighbour pairs that are actually linked by an
//! edge in either direction (Watts & Strogatz 1998). It is 0 for nodes with
//! fewer than two neighbours. The value lies in `[0, 1]`: a "hub" that merely
//! fans out to unrelated leaves scores low, while a node embedded in a tightly
//! interconnected cluster scores high.

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::CxId;
use calyx_paths::AssocGraph;

/// Local clustering coefficient for every node, keyed by [`CxId`].
pub fn clustering_coefficients(graph: &AssocGraph) -> BTreeMap<CxId, f64> {
    (0..graph.node_count())
        .map(|index| {
            (
                graph.node_id(index).expect("clustering node id"),
                local_clustering_coefficient(graph, index),
            )
        })
        .collect()
}

/// Local clustering coefficient for a single node index.
pub fn local_clustering_coefficient(graph: &AssocGraph, index: usize) -> f64 {
    let neighbours = undirected_neighbours(graph, index);
    let k = neighbours.len();
    if k < 2 {
        return 0.0;
    }
    let neighbour_vec: Vec<usize> = neighbours.iter().copied().collect();
    let mut links = 0_usize;
    for (offset, &left) in neighbour_vec.iter().enumerate() {
        for &right in &neighbour_vec[offset + 1..] {
            if linked(graph, left, right) {
                links += 1;
            }
        }
    }
    let possible = k * (k - 1) / 2;
    links as f64 / possible as f64
}

fn undirected_neighbours(graph: &AssocGraph, index: usize) -> BTreeSet<usize> {
    let mut neighbours = BTreeSet::new();
    for edge in graph.out_edges_by_index(index) {
        if edge.dst != index {
            neighbours.insert(edge.dst);
        }
    }
    for edge in graph.incoming_edges_by_index(index) {
        if edge.src != index {
            neighbours.insert(edge.src);
        }
    }
    neighbours
}

fn linked(graph: &AssocGraph, left: usize, right: usize) -> bool {
    graph
        .out_edges_by_index(left)
        .iter()
        .any(|edge| edge.dst == right)
        || graph
            .out_edges_by_index(right)
            .iter()
            .any(|edge| edge.dst == left)
}
