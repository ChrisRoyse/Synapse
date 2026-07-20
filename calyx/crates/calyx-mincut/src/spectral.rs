use std::collections::BTreeMap;

use calyx_core::CxId;
use calyx_paths::AssocGraph;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::spectral_linalg::{column, lanczos_eigen_operator};

pub type NodeId = CxId;
pub type SparseGraph = AssocGraph;
pub type SpectralResult<T> = std::result::Result<T, SpectralError>;

const EIGEN_EPS: f32 = 1.0e-6;
const DEFAULT_EIGEN_MAX_ITER: usize = 64;
const MIN_LANCZOS_DIM: usize = 32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EigenPair {
    pub eigenvalue: f32,
    pub eigenvector: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpectralCacheKey {
    pub scope: String,
    pub panel_version: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpectralCacheEntry {
    pub centrality: Vec<(NodeId, f32)>,
    pub eigenpairs: Vec<EigenPair>,
    pub refreshed_at_seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpectralCache {
    entries: BTreeMap<SpectralCacheKey, SpectralCacheEntry>,
}

impl SpectralCache {
    pub fn insert(&mut self, key: SpectralCacheKey, entry: SpectralCacheEntry) {
        self.entries.insert(key, entry);
    }

    pub fn get(&self, key: &SpectralCacheKey) -> Option<&SpectralCacheEntry> {
        self.entries.get(key)
    }

    pub fn invalidate(&mut self, key: &SpectralCacheKey) -> Option<SpectralCacheEntry> {
        self.entries.remove(key)
    }

    pub fn invalidate_scope(&mut self, scope: &str) {
        self.entries.retain(|key, _| key.scope != scope);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Error)]
pub enum SpectralError {
    #[error(
        "CALYX_SPECTRAL_NOT_CONVERGED: spectral iteration did not converge after {iterations} iterations"
    )]
    NotConverged { iterations: usize },
    #[error("CALYX_SPECTRAL_GRAPH_TOO_SMALL: graph has {n} nodes, requires at least {required}")]
    GraphTooSmall { n: usize, required: usize },
    #[error("CALYX_SPECTRAL_SINGULAR_MATRIX: graph has no positive spectral mass")]
    SingularMatrix,
    #[error(
        "CALYX_SPECTRAL_INVALID_OPERATOR: operator returned length {actual} for dimension {expected} with {non_finite} non-finite values"
    )]
    InvalidOperator {
        expected: usize,
        actual: usize,
        non_finite: usize,
    },
}

impl SpectralError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotConverged { .. } => "CALYX_SPECTRAL_NOT_CONVERGED",
            Self::GraphTooSmall { .. } => "CALYX_SPECTRAL_GRAPH_TOO_SMALL",
            Self::SingularMatrix => "CALYX_SPECTRAL_SINGULAR_MATRIX",
            Self::InvalidOperator { .. } => "CALYX_SPECTRAL_INVALID_OPERATOR",
        }
    }
}

pub fn eigenvector_centrality(
    graph: &SparseGraph,
    max_iter: usize,
    tol: f32,
) -> SpectralResult<Vec<(NodeId, f32)>> {
    ensure_min_nodes(graph, 2)?;
    if max_iter == 0 {
        return Err(SpectralError::NotConverged { iterations: 0 });
    }
    let sparse = SymmetricSparseGraph::from_assoc(graph);
    let n = sparse.len();
    let mut current = vec![1.0 / (n as f32).sqrt(); n];
    let mut iterations = 0;

    for step in 1..=max_iter {
        iterations = step;
        let mut next = sparse.shifted_adjacency_mat_vec(&current);
        normalize(&mut next)?;
        if l2_distance(&next, &current) < tol {
            return Ok(ranked_scores(graph, &next));
        }
        current = next;
    }
    Err(SpectralError::NotConverged { iterations })
}

pub fn laplacian_eigenmaps(graph: &SparseGraph, k: usize) -> SpectralResult<Vec<EigenPair>> {
    laplacian_eigenmaps_with_max_iter(graph, k, DEFAULT_EIGEN_MAX_ITER)
}

pub fn laplacian_eigenmaps_with_max_iter(
    graph: &SparseGraph,
    k: usize,
    max_iter: usize,
) -> SpectralResult<Vec<EigenPair>> {
    ensure_min_nodes(graph, 2)?;
    if k == 0 {
        return Ok(Vec::new());
    }
    if max_iter == 0 {
        return Err(SpectralError::NotConverged { iterations: 0 });
    }
    let sparse = SymmetricSparseGraph::from_assoc(graph);
    let target_dim = lanczos_target_dim(sparse.len(), k, max_iter)?;
    let shift = sparse.laplacian_shift();
    let (values, vectors) =
        lanczos_eigen_operator(sparse.len(), target_dim, target_dim, |vector| {
            sparse.shifted_laplacian_mat_vec(vector, shift)
        })?;
    let mut pairs: Vec<_> = values
        .into_iter()
        .enumerate()
        .map(|(index, eigenvalue)| EigenPair {
            eigenvalue: clean_zero(shift - eigenvalue),
            eigenvector: orient_vector(column(&vectors, index)),
        })
        .collect();
    pairs.sort_by(|left, right| left.eigenvalue.total_cmp(&right.eigenvalue));
    pairs.truncate(k.min(pairs.len()));
    Ok(pairs)
}

pub fn gft_project(signal: &[f32], eigenvectors: &[EigenPair]) -> Vec<f32> {
    eigenvectors
        .iter()
        .map(|pair| {
            assert_eq!(
                signal.len(),
                pair.eigenvector.len(),
                "GFT signal/eigenvector dimension mismatch"
            );
            dot(signal, &pair.eigenvector)
        })
        .collect()
}

pub fn gft_reconstruct(coefficients: &[f32], eigenvectors: &[EigenPair]) -> Vec<f32> {
    assert_eq!(
        coefficients.len(),
        eigenvectors.len(),
        "GFT coefficient/eigenvector count mismatch"
    );
    let Some(first) = eigenvectors.first() else {
        return Vec::new();
    };
    let mut signal = vec![0.0; first.eigenvector.len()];
    for (coefficient, pair) in coefficients.iter().zip(eigenvectors) {
        assert_eq!(
            signal.len(),
            pair.eigenvector.len(),
            "GFT eigenvector basis dimension mismatch"
        );
        for (dst, value) in signal.iter_mut().zip(&pair.eigenvector) {
            *dst += coefficient * value;
        }
    }
    signal
}

pub fn spectral_gap(eigenmaps: &[EigenPair]) -> f32 {
    if eigenmaps.len() < 2 {
        return 0.0;
    }
    (eigenmaps[1].eigenvalue - eigenmaps[0].eigenvalue).max(0.0)
}

fn ensure_min_nodes(graph: &SparseGraph, required: usize) -> SpectralResult<()> {
    let n = graph.node_count();
    if n < required {
        Err(SpectralError::GraphTooSmall { n, required })
    } else {
        Ok(())
    }
}

fn lanczos_target_dim(n: usize, k: usize, max_iter: usize) -> SpectralResult<usize> {
    let required = k.min(n);
    let target = if n <= max_iter {
        n
    } else {
        MIN_LANCZOS_DIM
            .max(k.saturating_mul(4).saturating_add(8))
            .min(max_iter)
            .min(n)
    };
    if target < required {
        return Err(SpectralError::NotConverged {
            iterations: max_iter,
        });
    }
    Ok(target)
}

struct SymmetricSparseGraph {
    adjacency: Vec<Vec<(usize, f32)>>,
    degree: Vec<f32>,
}

impl SymmetricSparseGraph {
    fn from_assoc(graph: &SparseGraph) -> Self {
        let n = graph.node_count();
        let mut rows = vec![BTreeMap::<usize, f32>::new(); n];
        for edge in graph.edges() {
            insert_max(&mut rows[edge.src], edge.dst, edge.weight);
            insert_max(&mut rows[edge.dst], edge.src, edge.weight);
        }
        let adjacency = rows
            .into_iter()
            .map(|row| row.into_iter().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let degree = adjacency
            .iter()
            .map(|row| row.iter().map(|(_, weight)| *weight).sum::<f32>())
            .collect();
        Self { adjacency, degree }
    }

    fn len(&self) -> usize {
        self.adjacency.len()
    }

    fn laplacian_shift(&self) -> f32 {
        self.degree.iter().copied().fold(0.0_f32, f32::max) * 2.0 + EIGEN_EPS
    }

    fn shifted_adjacency_mat_vec(&self, vector: &[f32]) -> Vec<f32> {
        self.adjacency
            .par_iter()
            .enumerate()
            .map(|(row_index, row)| {
                row.iter()
                    .fold(vector[row_index], |acc, (col_index, weight)| {
                        acc + weight * vector[*col_index]
                    })
            })
            .collect()
    }

    fn shifted_laplacian_mat_vec(&self, vector: &[f32], shift: f32) -> Vec<f32> {
        self.adjacency
            .par_iter()
            .enumerate()
            .map(|(row_index, row)| {
                let laplacian_value = row.iter().fold(
                    self.degree[row_index] * vector[row_index],
                    |acc, (col_index, weight)| acc - weight * vector[*col_index],
                );
                shift * vector[row_index] - laplacian_value
            })
            .collect()
    }
}

fn insert_max(row: &mut BTreeMap<usize, f32>, col: usize, weight: f32) {
    row.entry(col)
        .and_modify(|stored| *stored = (*stored).max(weight))
        .or_insert(weight);
}

fn ranked_scores(graph: &SparseGraph, vector: &[f32]) -> Vec<(NodeId, f32)> {
    let max = vector
        .iter()
        .map(|value| value.abs())
        .fold(0.0_f32, f32::max);
    let mut ranked: Vec<_> = vector
        .iter()
        .enumerate()
        .map(|(index, value)| {
            (
                graph.node_id(index).expect("spectral node id"),
                if max <= EIGEN_EPS {
                    0.0
                } else {
                    value.abs() / max
                },
            )
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.as_bytes().cmp(right.0.as_bytes()))
    });
    ranked
}

fn orient_vector(mut vector: Vec<f32>) -> Vec<f32> {
    if let Some(first) = vector.iter().find(|value| value.abs() > EIGEN_EPS)
        && *first < 0.0
    {
        for value in &mut vector {
            *value = -*value;
        }
    }
    vector
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn l2_distance(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f32>()
        .sqrt()
}

fn normalize(vector: &mut [f32]) -> SpectralResult<()> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= EIGEN_EPS {
        return Err(SpectralError::SingularMatrix);
    }
    for value in vector {
        *value /= norm;
    }
    Ok(())
}

fn clean_zero(value: f32) -> f32 {
    if value.abs() < EIGEN_EPS { 0.0 } else { value }
}

// IMPORTANT: spectral centrality is structure-only; the MFVS kernel is outcome-anchored (A2).
// Centrality proposes candidates; grounding through oracle anchors confirms them.
