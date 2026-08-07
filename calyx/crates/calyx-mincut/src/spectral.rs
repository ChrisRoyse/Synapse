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
    /// The shifted power iteration exhausted its budget on one connected
    /// component, reported with the residual it actually reached so the failure
    /// says *how far off* it was rather than only that it stopped.
    #[error(
        "CALYX_SPECTRAL_NOT_CONVERGED: spectral iteration did not converge after {iterations} \
         iterations: residual {residual:e} still exceeds tol {tol:e} on the {component_nodes}-node \
         connected component (component {component_index} of {components}, over {nodes} nodes) \
         whose adjacency spectral radius is {component_radius:e}"
    )]
    NotConverged {
        iterations: usize,
        residual: f32,
        tol: f32,
        nodes: usize,
        components: usize,
        component_index: usize,
        component_nodes: usize,
        component_radius: f32,
    },
    /// The Lanczos pass could not build the Krylov basis it was asked for. This
    /// is not a residual failure — no residual was ever measured — so it does
    /// not borrow [`Self::NotConverged`]'s vocabulary.
    #[error(
        "CALYX_SPECTRAL_KRYLOV_INCOMPLETE: Lanczos built {built} of {target} basis vectors for a \
         {nodes}-node operator within {max_iter} iterations"
    )]
    KrylovIncomplete {
        built: usize,
        target: usize,
        nodes: usize,
        max_iter: usize,
    },
    #[error(
        "CALYX_SPECTRAL_JACOBI_NOT_CONVERGED: Jacobi sweep left off-diagonal mass {residual:e} \
         above tol {tol:e} after {iterations} rotations on a {dim}x{dim} projected matrix"
    )]
    JacobiNotConverged {
        iterations: usize,
        residual: f32,
        tol: f32,
        dim: usize,
    },
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
            Self::KrylovIncomplete { .. } => "CALYX_SPECTRAL_KRYLOV_INCOMPLETE",
            Self::JacobiNotConverged { .. } => "CALYX_SPECTRAL_JACOBI_NOT_CONVERGED",
            Self::GraphTooSmall { .. } => "CALYX_SPECTRAL_GRAPH_TOO_SMALL",
            Self::SingularMatrix => "CALYX_SPECTRAL_SINGULAR_MATRIX",
            Self::InvalidOperator { .. } => "CALYX_SPECTRAL_INVALID_OPERATOR",
        }
    }
}

/// Eigenvector centrality by shifted power iteration, computed **one connected
/// component at a time** and shifted by **each component's own spectral scale**
/// (#2076, #2081).
///
/// Two independent properties of this vault's graphs each defeat the textbook
/// `A + I` global iteration, and each is addressed at its own root: the
/// component decomposition below, and the scale-commensurate shift documented
/// on [`SymmetricSparseGraph::component_perron`]. Neither changes the value
/// computed — a shift and a positive scaling of a symmetric block leave every
/// eigenvector fixed — only whether a finite iteration budget can reach it.
///
/// # Why per component, and not one global power iteration
///
/// Perron–Frobenius gives a *simple* dominant eigenvalue only for an
/// irreducible non-negative matrix, i.e. a connected graph. A disconnected
/// graph's adjacency is reducible: its spectrum is the union of its components'
/// spectra, so the dominant eigenvalue generally has multiplicity greater than
/// one and the dominant eigenspace is a plane rather than a line.
///
/// **The answer is then not well defined.** The limit of the iteration is the
/// projection of the start vector onto the dominant eigenspace, so which of the
/// infinitely many dominant eigenvectors comes back is decided by round-off, and
/// every component but the widest one is driven towards zero regardless of its
/// internal structure — a "centrality" that reports nothing about 63 of this
/// vault's 64 components. NetworkX documents exactly this and now refuses
/// disconnected graphs outright in its dense solver for that reason
/// (networkx/networkx#6888, networkx/networkx#7549); its iterative solver uses
/// the same `A + I` shift this one does, for the same negative-eigenvalue
/// reason, and raises rather than guess.
///
/// Note what reducibility on its own does **not** explain, because the
/// distinction is what sent #2081's first diagnosis to the wrong remedy. This
/// block is symmetric, and for a symmetric matrix a *tie* among dominant
/// eigenvalues does not stop the successive-iterate residual from decaying: the
/// iterate still converges, to a fixed member of the dominant eigenspace, at the
/// rate set by the next *distinct* eigenvalue. Reducibility corrupts the answer;
/// it does not by itself exhaust the budget. What exhausts the budget is the
/// scale mismatch documented on
/// [`SymmetricSparseGraph::component_perron`], which reducibility then
/// multiplies across every component at once.
///
/// Restricted to one component the adjacency block *is* irreducible, the Perron
/// root is simple and strictly positive, and the iteration converges at the
/// component's own gap. Components are then placed on one scale by their
/// adjacency spectral radius, which is the only quantity the eigenproblem
/// supplies for comparing them: a component's unit Perron vector is scaled by
/// its radius before [`ranked_scores`] normalizes globally. On a connected
/// graph there is exactly one component, the single scale factor divides out in
/// that normalization, and the returned scores are identical to the pre-#2076
/// result — which is why `syn-graphpos-app-v1` (measured live at 113 nodes over
/// 682 transitions, one component, and converging within 32 iterations both
/// before and after this change) is unaffected.
///
/// # Errors
///
/// [`SpectralError::NotConverged`] naming the component, its size, its radius,
/// the iterations spent and the residual reached, when a component's own gap is
/// still too small for `max_iter`. A non-converged centrality is never returned
/// with a caveat, because [`crate::StructuralSignature`]'s consumer has nowhere
/// to carry one.
pub fn eigenvector_centrality(
    graph: &SparseGraph,
    max_iter: usize,
    tol: f32,
) -> SpectralResult<Vec<(NodeId, f32)>> {
    ensure_min_nodes(graph, 2)?;
    let sparse = SymmetricSparseGraph::from_assoc(graph);
    let n = sparse.len();
    let components = sparse.connected_components();
    let mut combined = vec![0.0_f32; n];
    let mut max_radius = 0.0_f32;

    for (component_index, nodes) in components.iter().enumerate() {
        let spectrum = sparse.component_perron(nodes, max_iter, tol).map_err(
            |ComponentDivergence {
                 iterations,
                 residual,
                 radius,
             }| SpectralError::NotConverged {
                iterations,
                residual,
                tol,
                nodes: n,
                components: components.len(),
                component_index,
                component_nodes: nodes.len(),
                component_radius: radius,
            },
        )?;
        max_radius = max_radius.max(spectrum.radius);
        for (local, global) in nodes.iter().copied().enumerate() {
            combined[global] = spectrum.vector[local] * spectrum.radius;
        }
    }

    if max_radius <= EIGEN_EPS {
        // Every component is an isolated node: there is no adjacency mass for a
        // centrality to be about. Refuse rather than return all-zero scores
        // that read like "measured, and uniformly unimportant".
        return Err(SpectralError::SingularMatrix);
    }
    Ok(ranked_scores(graph, &combined))
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
        return Err(SpectralError::KrylovIncomplete {
            built: 0,
            target: k,
            nodes: graph.node_count(),
            max_iter: 0,
        });
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
        return Err(SpectralError::KrylovIncomplete {
            built: 0,
            target: required,
            nodes: n,
            max_iter,
        });
    }
    Ok(target)
}

/// One component's converged shifted-Perron pair.
struct ComponentSpectrum {
    /// Unit-L2 Perron vector of `I + A_c`, in component-local index order.
    vector: Vec<f32>,
    /// Rayleigh quotient of the *unshifted* block: the component's adjacency
    /// spectral radius. The only quantity the eigenproblem offers for placing
    /// separate components on one scale.
    radius: f32,
}

/// Evidence from a component whose iteration budget expired.
struct ComponentDivergence {
    iterations: usize,
    residual: f32,
    radius: f32,
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

    /// Node index sets of the connected components, each ascending, ordered by
    /// their lowest member. Deterministic, so a component's identity in a
    /// [`SpectralError::NotConverged`] report is reproducible.
    fn connected_components(&self) -> Vec<Vec<usize>> {
        let n = self.len();
        let mut seen = vec![false; n];
        let mut components = Vec::new();
        let mut stack = Vec::new();
        for root in 0..n {
            if seen[root] {
                continue;
            }
            seen[root] = true;
            stack.push(root);
            let mut nodes = Vec::new();
            while let Some(node) = stack.pop() {
                nodes.push(node);
                for (neighbor, _) in &self.adjacency[node] {
                    if !seen[*neighbor] {
                        seen[*neighbor] = true;
                        stack.push(*neighbor);
                    }
                }
            }
            nodes.sort_unstable();
            components.push(nodes);
        }
        components
    }

    /// `(I + A_c / scale) v` for one component, in component-local index order.
    ///
    /// `local_of` maps a global node index to its position in `nodes`; entries
    /// outside the component are never read, because no edge leaves it.
    fn component_mat_vec(
        &self,
        nodes: &[usize],
        local_of: &[usize],
        scale: f32,
        vector: &[f32],
    ) -> Vec<f32> {
        let inverse_scale = scale.recip();
        nodes
            .par_iter()
            .enumerate()
            .map(|(local_index, global_index)| {
                self.adjacency[*global_index].iter().fold(
                    vector[local_index],
                    |acc, (col_index, weight)| {
                        acc + weight * inverse_scale * vector[local_of[*col_index]]
                    },
                )
            })
            .collect()
    }

    /// The component's Gershgorin bound on its own adjacency spectral radius:
    /// the largest weighted degree among its nodes.
    ///
    /// For a symmetric non-negative block, `rho(A_c) <= max_i sum_j |A_ij|`,
    /// and no edge leaves a connected component, so a node's full stored degree
    /// *is* its within-component row sum. This is the scale the shift is
    /// measured against; see [`Self::component_perron`] for why that matters.
    fn component_scale(&self, nodes: &[usize]) -> f32 {
        nodes
            .iter()
            .map(|global_index| self.degree[*global_index])
            .fold(0.0_f32, f32::max)
    }

    /// Shifted power iteration restricted to one connected component, on
    /// `I + A_c / scale_c` where `scale_c` is the component's own Gershgorin
    /// radius bound (#2081).
    ///
    /// # Why the block is restricted to one component
    ///
    /// Within a component the block is irreducible, so Perron–Frobenius makes
    /// the dominant eigenvalue simple and its eigenvector strictly positive —
    /// the two guarantees the global iteration forfeits on a disconnected
    /// graph.
    ///
    /// # Why the shift is `scale_c`, and not the fixed `1` it used to be
    ///
    /// Shifting a symmetric non-negative block by *any* positive multiple of
    /// the identity leaves every eigenvector untouched and moves the Perron
    /// root to the top of the spectrum by modulus, which is the only thing the
    /// shift is for: it defeats the bipartite case `lambda_min = -lambda_1`,
    /// where an unshifted iteration oscillates forever. NetworkX adopted `A + I`
    /// for exactly that reason and says in the same breath that *"adding other
    /// multiples of the identity to A will also work, but A+I is cheap to
    /// implement and sufficient"* (networkx/networkx#1704). The unstated premise
    /// is that `A` is an **unweighted** adjacency, where every non-zero entry is
    /// `1`, so `rho(A) >= 1` and a shift of `1` is commensurate with the
    /// spectrum it is shifting.
    ///
    /// That premise does not hold here. [`crate::build_transition_graph`]
    /// rescales every edge weight to `count / max_pair_count`, so a single
    /// heavily-observed transition pair anywhere in the graph divides every
    /// *other* edge weight — and hence every other component's spectral radius —
    /// by an arbitrary data-dependent factor. Against a fixed shift of `1` the
    /// contraction ratio of a depth-1 star then degrades to
    /// `1 / (1 + sqrt(k) / max_pair_count)`, which tends to 1. The graph's
    /// *shape* is unchanged and so is the centrality it defines; only the
    /// solver's ability to reach it decays. Measured on the frozen
    /// `max_iter = 256`, `tol = 1e-6` budget, an 8-leaf star sharing a graph with
    /// one dyad observed 256 times needed more than 256 iterations and refused
    /// with `CALYX_SPECTRAL_NOT_CONVERGED` — a refusal caused entirely by the
    /// units of an unrelated edge. The live app lane already runs at
    /// `max_pair_count = 2533` and a minimum edge weight of `3.9e-4`.
    ///
    /// Taking the shift to be the block's own Gershgorin bound
    /// `scale_c = max_i sum_j |A_ij| >= rho(A_c)` makes the contraction ratio
    /// `(scale_c + lambda_2) / (scale_c + lambda_1)`, which is invariant under
    /// any global rescaling of the weights: it depends on the component's shape
    /// alone. Equivalently, and as implemented, the block is divided by
    /// `scale_c` first — putting its spectrum in `[-1, 1]` — and then shifted by
    /// the same cheap `1`.
    ///
    /// The returned pair is mathematically identical either way: a shift and a
    /// positive scaling of a symmetric matrix change no eigenvector, and the
    /// radius is recovered by undoing both. Only the number of iterations
    /// needed to reach it changes.
    fn component_perron(
        &self,
        nodes: &[usize],
        max_iter: usize,
        tol: f32,
    ) -> std::result::Result<ComponentSpectrum, ComponentDivergence> {
        let size = nodes.len();
        let scale = self.component_scale(nodes);
        let unit = vec![1.0 / (size as f32).sqrt(); size];
        if !scale.is_finite() || scale <= 0.0 {
            // An edgeless component: `A_c` is the zero block, every vector is an
            // eigenvector, and the radius is exactly zero. There is nothing to
            // iterate towards and no scale to divide by, so answer in closed
            // form rather than dividing by zero to rediscover it.
            return Ok(ComponentSpectrum {
                vector: unit,
                radius: 0.0,
            });
        }
        let mut local_of = vec![0_usize; self.len()];
        for (local_index, global_index) in nodes.iter().copied().enumerate() {
            local_of[global_index] = local_index;
        }
        let mut current = unit;
        let mut residual = f32::INFINITY;
        for step in 1..=max_iter {
            let mut next = self.component_mat_vec(nodes, &local_of, scale, &current);
            let norm = next.iter().map(|value| value * value).sum::<f32>().sqrt();
            if !norm.is_finite() || norm <= EIGEN_EPS {
                // `I + A_c / scale` has a unit diagonal and non-negative
                // off-diagonal entries, so a non-negative unit input cannot map
                // to zero; a zero norm here is a corrupt weight, not a spectral
                // property.
                return Err(ComponentDivergence {
                    iterations: step,
                    residual: f32::INFINITY,
                    radius: 0.0,
                });
            }
            for value in &mut next {
                *value /= norm;
            }
            residual = l2_distance(&next, &current);
            current = next;
            if residual < tol {
                return Ok(ComponentSpectrum {
                    radius: self.component_radius(nodes, &local_of, scale, &current),
                    vector: current,
                });
            }
        }
        let radius = self.component_radius(nodes, &local_of, scale, &current);
        Err(ComponentDivergence {
            iterations: max_iter,
            residual,
            radius,
        })
    }

    /// Undoes the shift and the scaling to recover the component's adjacency
    /// spectral radius from a unit iterate: the iteration runs on
    /// `I + A_c / scale`, so `rho = (rayleigh - 1) * scale`.
    fn component_radius(
        &self,
        nodes: &[usize],
        local_of: &[usize],
        scale: f32,
        vector: &[f32],
    ) -> f32 {
        let product = self.component_mat_vec(nodes, local_of, scale, vector);
        ((rayleigh(&product, vector) - 1.0) * scale).max(0.0)
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

/// Rayleigh quotient `v.Mv` for an already unit-normalized `v`.
fn rayleigh(product: &[f32], vector: &[f32]) -> f32 {
    dot(product, vector)
}

fn l2_distance(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f32>()
        .sqrt()
}

fn clean_zero(value: f32) -> f32 {
    if value.abs() < EIGEN_EPS { 0.0 } else { value }
}

// IMPORTANT: spectral centrality is structure-only; the MFVS kernel is outcome-anchored (A2).
// Centrality proposes candidates; grounding through oracle anchors confirms them.
