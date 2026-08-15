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
const MIN_PERRON_RITZ_DIM: usize = 8;
const PERRON_RITZ_DIM_STEP: usize = 16;
const PROJECTED_JACOBI_ROTATIONS_PER_ENTRY: usize = 32;
/// Power used by the component-local infinity-norm spectral-radius bound.
/// Eight sparse mat-vecs are negligible beside the 256-step solve, while the
/// eighth root removes most of the max-degree bound's irregular-graph slack.
const COMPONENT_SCALE_POWER: usize = 8;

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
        "CALYX_SPECTRAL_ITERATION_BUDGET_INVALID: max_iter must be greater than zero; got {max_iter}"
    )]
    InvalidIterationBudget { max_iter: usize },
    #[error(
        "CALYX_SPECTRAL_TOLERANCE_INVALID: tol must be finite and greater than zero; got {tol:e}"
    )]
    InvalidTolerance { tol: f32 },
    /// The residual-certified component Krylov solve exhausted its basis
    /// budget. `residual` is the normalized eigenpair residual
    /// `||A_c x - lambda*x||_2 / |lambda|`, reported so the failure says *how
    /// far off* the candidate was rather than only that it stopped.
    #[error(
        "CALYX_SPECTRAL_NOT_CONVERGED: spectral Krylov solve did not converge after {iterations} \
         basis vectors: normalized eigenpair residual {residual:e} still exceeds tol {tol:e} on the \
         {component_nodes}-node connected component (component {component_index} of {components}, \
         over {nodes} nodes) whose adjacency spectral radius is {component_radius:e} and \
         normalization scale is {component_shift_scale:e}"
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
        component_shift_scale: f32,
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
            Self::InvalidIterationBudget { .. } => "CALYX_SPECTRAL_ITERATION_BUDGET_INVALID",
            Self::InvalidTolerance { .. } => "CALYX_SPECTRAL_TOLERANCE_INVALID",
            Self::NotConverged { .. } => "CALYX_SPECTRAL_NOT_CONVERGED",
            Self::KrylovIncomplete { .. } => "CALYX_SPECTRAL_KRYLOV_INCOMPLETE",
            Self::JacobiNotConverged { .. } => "CALYX_SPECTRAL_JACOBI_NOT_CONVERGED",
            Self::GraphTooSmall { .. } => "CALYX_SPECTRAL_GRAPH_TOO_SMALL",
            Self::SingularMatrix => "CALYX_SPECTRAL_SINGULAR_MATRIX",
            Self::InvalidOperator { .. } => "CALYX_SPECTRAL_INVALID_OPERATOR",
        }
    }
}

/// Eigenvector centrality by a residual-certified symmetric Krylov solve,
/// computed **one connected component at a time** and normalized by **each
/// component's own spectral scale** (#2076, #2081, #2130).
///
/// The component decomposition makes the requested Perron vector well-defined.
/// The component-local normalization keeps the sparse operator numerically
/// commensurate. The Krylov/Rayleigh-Ritz solve avoids the fixed spectral-gap
/// dependency of power iteration: connectedness makes the Perron root simple,
/// but it does not put a lower bound on its separation from the next root.
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
/// Reducibility and convergence are separate contracts. Component-local
/// normalization fixed #2081's edge-weight-unit pathology, but #2130 later
/// proved that even a tightly scaled connected block can defeat a fixed power
/// budget when its two leading roots are close. The component solve below
/// therefore uses Rayleigh-Ritz extraction over the retained Krylov space rather
/// than discarding every prior direction as power iteration does.
///
/// Restricted to one component the adjacency block *is* irreducible and its
/// Perron root is simple and strictly positive. Components are then placed on one scale by their
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
    if max_iter == 0 {
        return Err(SpectralError::InvalidIterationBudget { max_iter });
    }
    if !tol.is_finite() || tol <= 0.0 {
        return Err(SpectralError::InvalidTolerance { tol });
    }
    ensure_min_nodes(graph, 2)?;
    let sparse = SymmetricSparseGraph::from_assoc(graph);
    let n = sparse.len();
    let components = sparse.connected_components();
    let mut combined = vec![0.0_f32; n];
    let mut max_radius = 0.0_f32;

    for (component_index, nodes) in components.iter().enumerate() {
        let spectrum =
            sparse
                .component_perron(nodes, max_iter, tol)
                .map_err(|error| match error {
                    ComponentPerronError::Diverged(ComponentDivergence {
                        iterations,
                        residual,
                        radius,
                        shift_scale,
                    }) => SpectralError::NotConverged {
                        iterations,
                        residual,
                        tol,
                        nodes: n,
                        components: components.len(),
                        component_index,
                        component_nodes: nodes.len(),
                        component_radius: radius,
                        component_shift_scale: shift_scale,
                    },
                    ComponentPerronError::Spectral(error) => error,
                })?;
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
    shift_scale: f32,
}

enum ComponentPerronError {
    Diverged(ComponentDivergence),
    Spectral(SpectralError),
}

impl From<ComponentDivergence> for ComponentPerronError {
    fn from(value: ComponentDivergence) -> Self {
        Self::Diverged(value)
    }
}

impl From<SpectralError> for ComponentPerronError {
    fn from(value: SpectralError) -> Self {
        Self::Spectral(value)
    }
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

    /// `(A_c / scale) v` for one component, in component-local index order.
    ///
    /// The persisted graph remains `f32`, but the iterative kernel is `f64`.
    /// A `1e-6` acceptance tolerance cannot be certified by repeatedly
    /// accumulating and normalizing in a format whose machine epsilon is
    /// already about `1.2e-7`: on a few hundred coordinates, roundoff in the
    /// old successive-vector distance plateaued above the tolerance forever.
    /// Widening only the bounded component-local work vectors avoids that
    /// numerical floor without widening persisted rows or public scores.
    ///
    /// `local_of` maps a global node index to its position in `nodes`; entries
    /// outside the component are never read, because no edge leaves it.
    fn component_scaled_adjacency_mat_vec(
        &self,
        nodes: &[usize],
        local_of: &[usize],
        scale: f64,
        vector: &[f64],
    ) -> Vec<f64> {
        let inverse_scale = scale.recip();
        nodes
            .par_iter()
            .map(|global_index| {
                self.adjacency[*global_index]
                    .iter()
                    .map(|(col_index, weight)| {
                        f64::from(*weight) * inverse_scale * vector[local_of[*col_index]]
                    })
                    .sum()
            })
            .collect()
    }

    /// A component-local upper bound on its adjacency spectral radius:
    /// `||A_c^8||_inf^(1/8)`.
    ///
    /// Every consistent matrix norm upper-bounds spectral radius, and Gelfand's
    /// formula makes the root of a power norm approach it. For a non-negative
    /// matrix, the infinity norm is exactly the largest entry of `A^8 * 1`, so
    /// this stays sparse: no matrix power is materialized.
    ///
    /// The calculation first divides by the one-hop Gershgorin bound. This
    /// prevents overflow and preserves exact invariance to a global rescaling
    /// of edge weights. On a `k`-leaf star the old max-degree scale was `k`
    /// while `rho(A) = sqrt(k)`, making the identity shift arbitrarily dominant;
    /// every even power norm recovers `sqrt(k)` exactly.
    fn component_scale(&self, nodes: &[usize], local_of: &[usize]) -> f64 {
        let gershgorin = nodes
            .iter()
            .map(|global_index| {
                self.adjacency[*global_index]
                    .iter()
                    .map(|(_, weight)| f64::from(*weight))
                    .sum::<f64>()
            })
            .fold(0.0_f64, f64::max);
        if !gershgorin.is_finite() || gershgorin <= 0.0 {
            return gershgorin;
        }

        let inverse_gershgorin = gershgorin.recip();
        let mut powered_row_sums = vec![1.0_f64; nodes.len()];
        for _ in 0..COMPONENT_SCALE_POWER {
            powered_row_sums = nodes
                .par_iter()
                .map(|global_index| {
                    self.adjacency[*global_index]
                        .iter()
                        .map(|(col_index, weight)| {
                            f64::from(*weight)
                                * inverse_gershgorin
                                * powered_row_sums[local_of[*col_index]]
                        })
                        .sum::<f64>()
                })
                .collect();
        }
        let powered_norm = powered_row_sums.iter().copied().fold(0.0_f64, f64::max);
        if !powered_norm.is_finite() || powered_norm <= 0.0 {
            return f64::NAN;
        }
        gershgorin * powered_norm.powf(1.0 / COMPONENT_SCALE_POWER as f64)
    }

    /// Residual-certified full-reorthogonalized Arnoldi/Lanczos solve restricted
    /// to one connected symmetric component.
    ///
    /// # Why the block is restricted to one component
    ///
    /// Within a component the block is irreducible, so Perron–Frobenius makes
    /// the dominant eigenvalue simple and its eigenvector strictly positive —
    /// the two guarantees the global iteration forfeits on a disconnected
    /// graph.
    ///
    /// # Why this is a Krylov/Rayleigh-Ritz solve, not power iteration
    ///
    /// Power iteration's contraction factor is the ratio of the two leading
    /// eigenvalue magnitudes. Connectedness makes the Perron root simple but
    /// places no lower bound on that spectral gap, so no fixed step count can
    /// make the old iteration operational for every real connected graph. The
    /// live 1,184-node agent component in #2130 reached only `4.1e-6` residual
    /// after its frozen 256 steps despite a tight normalization scale.
    ///
    /// A Krylov subspace retains all generated directions and extracts the
    /// largest *algebraic* Ritz pair of the symmetric adjacency. Targeting the
    /// largest algebraic value removes the bipartite `+rho/-rho` modulus tie
    /// without an identity shift. Full double-precision reorthogonalization
    /// keeps the projected operator trustworthy; every proposed Ritz vector is
    /// checked against the original sparse operator and is returned only when
    /// its relative eigenpair residual satisfies the caller's exact tolerance.
    ///
    /// The block is divided by
    /// `scale_c = ||A_c^8||_inf^(1/8) >= rho(A_c)` before projection. That
    /// normalization is invariant to global edge-weight units and bounds every
    /// operator product; it no longer controls convergence by acting as a
    /// shift. The physical adjacency radius is recovered by multiplication.
    ///
    /// `max_iter` is a strict Krylov-basis ceiling. Memory is
    /// `O(component_nodes * min(component_nodes, max_iter))`, bounded by the
    /// same public parameter that bounds sparse operator evaluations. Exhaustion
    /// returns the actual residual and never publishes a partial component.
    fn component_perron(
        &self,
        nodes: &[usize],
        max_iter: usize,
        tol: f32,
    ) -> std::result::Result<ComponentSpectrum, ComponentPerronError> {
        let size = nodes.len();
        let mut local_of = vec![0_usize; self.len()];
        for (local_index, global_index) in nodes.iter().copied().enumerate() {
            local_of[global_index] = local_index;
        }
        let scale = self.component_scale(nodes, &local_of);
        let unit = vec![1.0 / (size as f64).sqrt(); size];
        if scale == 0.0 {
            // An edgeless component: `A_c` is the zero block, every vector is an
            // eigenvector, and the radius is exactly zero. There is nothing to
            // iterate towards and no scale to divide by, so answer in closed
            // form rather than dividing by zero to rediscover it.
            return Ok(ComponentSpectrum {
                vector: unit.into_iter().map(|value| value as f32).collect(),
                radius: 0.0,
            });
        }
        if !scale.is_finite() || scale < 0.0 {
            return Err(ComponentDivergence {
                iterations: 0,
                residual: f32::INFINITY,
                radius: 0.0,
                shift_scale: scale as f32,
            }
            .into());
        }

        let budget = size.min(max_iter);
        let mut basis = vec![unit];
        let mut products = Vec::<Vec<f64>>::with_capacity(budget);
        let mut residual = f64::INFINITY;
        let mut normalized_radius = 0.0_f64;

        loop {
            let current_index = products.len();
            let product = self.component_scaled_adjacency_mat_vec(
                nodes,
                &local_of,
                scale,
                &basis[current_index],
            );
            validate_f64_operator_product(size, &product)?;
            products.push(product.clone());

            let dim = basis.len();
            let check_ritz = dim == 1
                || dim == 2
                || dim == 4
                || (dim >= MIN_PERRON_RITZ_DIM
                    && (dim == budget || dim.is_multiple_of(PERRON_RITZ_DIM_STEP)));
            if check_ritz {
                let (ritz_value, ritz_coefficients) =
                    projected_largest_ritz_pair(&basis, &products, f64::from(tol))?;
                let mut candidate = expand_ritz_vector_f64(&basis, &ritz_coefficients);
                normalize_f64(&mut candidate)?;
                let candidate_product =
                    self.component_scaled_adjacency_mat_vec(nodes, &local_of, scale, &candidate);
                validate_f64_operator_product(size, &candidate_product)?;
                let (rayleigh, candidate_residual) =
                    eigenpair_residual_f64(&candidate_product, &candidate);
                if !ritz_value.is_finite()
                    || !rayleigh.is_finite()
                    || rayleigh <= 0.0
                    || !candidate_residual.is_finite()
                {
                    return Err(ComponentDivergence {
                        iterations: dim,
                        residual: f32::INFINITY,
                        radius: 0.0,
                        shift_scale: scale as f32,
                    }
                    .into());
                }
                normalized_radius = rayleigh;
                residual = candidate_residual;
                if residual < f64::from(tol) {
                    return Ok(ComponentSpectrum {
                        radius: (normalized_radius * scale) as f32,
                        vector: candidate.into_iter().map(|value| value as f32).collect(),
                    });
                }
            }

            if dim == budget {
                break;
            }

            let mut next = product;
            reorthogonalize_f64(&mut next, &basis);
            reorthogonalize_f64(&mut next, &basis);
            let norm = l2_norm_f64(&next);
            if !norm.is_finite() {
                return Err(ComponentDivergence {
                    iterations: dim,
                    residual: f32::INFINITY,
                    radius: (normalized_radius * scale) as f32,
                    shift_scale: scale as f32,
                }
                .into());
            }
            if norm <= f64::EPSILON * (size as f64).sqrt() {
                if !check_ritz {
                    let (_, ritz_coefficients) =
                        projected_largest_ritz_pair(&basis, &products, f64::from(tol))?;
                    let mut candidate = expand_ritz_vector_f64(&basis, &ritz_coefficients);
                    normalize_f64(&mut candidate)?;
                    let candidate_product = self
                        .component_scaled_adjacency_mat_vec(nodes, &local_of, scale, &candidate);
                    validate_f64_operator_product(size, &candidate_product)?;
                    let (rayleigh, candidate_residual) =
                        eigenpair_residual_f64(&candidate_product, &candidate);
                    if !rayleigh.is_finite() || rayleigh <= 0.0 || !candidate_residual.is_finite() {
                        return Err(ComponentDivergence {
                            iterations: dim,
                            residual: f32::INFINITY,
                            radius: 0.0,
                            shift_scale: scale as f32,
                        }
                        .into());
                    }
                    normalized_radius = rayleigh;
                    residual = candidate_residual;
                    if residual < f64::from(tol) {
                        return Ok(ComponentSpectrum {
                            radius: (normalized_radius * scale) as f32,
                            vector: candidate.into_iter().map(|value| value as f32).collect(),
                        });
                    }
                }
                break;
            }
            for value in &mut next {
                *value /= norm;
            }
            basis.push(next);
        }

        Err(ComponentDivergence {
            iterations: basis.len(),
            residual: residual as f32,
            radius: (normalized_radius * scale) as f32,
            shift_scale: scale as f32,
        }
        .into())
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

fn validate_f64_operator_product(expected: usize, product: &[f64]) -> SpectralResult<()> {
    let non_finite = product.iter().filter(|value| !value.is_finite()).count();
    if product.len() != expected || non_finite != 0 {
        return Err(SpectralError::InvalidOperator {
            expected,
            actual: product.len(),
            non_finite,
        });
    }
    Ok(())
}

fn reorthogonalize_f64(vector: &mut [f64], basis: &[Vec<f64>]) {
    for basis_vector in basis {
        let projection = dot_f64(vector, basis_vector);
        for (value, basis_value) in vector.iter_mut().zip(basis_vector) {
            *value -= projection * basis_value;
        }
    }
}

fn normalize_f64(vector: &mut [f64]) -> SpectralResult<()> {
    let norm = l2_norm_f64(vector);
    if !norm.is_finite() || norm <= f64::MIN_POSITIVE {
        return Err(SpectralError::SingularMatrix);
    }
    for value in vector {
        *value /= norm;
    }
    Ok(())
}

fn dot_f64(left: &[f64], right: &[f64]) -> f64 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn projected_largest_ritz_pair(
    basis: &[Vec<f64>],
    products: &[Vec<f64>],
    requested_tol: f64,
) -> SpectralResult<(f64, Vec<f64>)> {
    let dim = basis.len();
    if dim == 0 || products.len() != dim {
        return Err(SpectralError::InvalidOperator {
            expected: dim,
            actual: products.len(),
            non_finite: 0,
        });
    }
    let mut projected = vec![vec![0.0_f64; dim]; dim];
    for row in 0..dim {
        for col in row..dim {
            let forward = dot_f64(&basis[row], &products[col]);
            let reverse = dot_f64(&basis[col], &products[row]);
            let value = 0.5 * (forward + reverse);
            if !value.is_finite() {
                return Err(SpectralError::InvalidOperator {
                    expected: dim,
                    actual: dim,
                    non_finite: 1,
                });
            }
            projected[row][col] = value;
            projected[col][row] = value;
        }
    }
    projected_symmetric_largest_f64(projected, requested_tol)
}

fn projected_symmetric_largest_f64(
    mut matrix: Vec<Vec<f64>>,
    requested_tol: f64,
) -> SpectralResult<(f64, Vec<f64>)> {
    let dim = matrix.len();
    if dim == 1 {
        return Ok((matrix[0][0], vec![1.0]));
    }
    let mut vectors = identity_f64(dim);
    let matrix_scale = matrix
        .iter()
        .flat_map(|row| row.iter())
        .map(|value| value.abs())
        .fold(0.0_f64, f64::max)
        .max(f64::MIN_POSITIVE);
    let jacobi_tol = (requested_tol * 0.01).max(f64::EPSILON * dim as f64 * 32.0) * matrix_scale;
    let max_rotations = dim
        .saturating_mul(dim)
        .saturating_mul(PROJECTED_JACOBI_ROTATIONS_PER_ENTRY);
    let mut final_offdiag = f64::INFINITY;
    let mut rotations = 0_usize;
    while rotations < max_rotations {
        let Some((p, q, offdiag)) = max_offdiag_f64(&matrix) else {
            break;
        };
        final_offdiag = offdiag.abs();
        if final_offdiag <= jacobi_tol {
            break;
        }
        rotate_symmetric_f64(&mut matrix, &mut vectors, p, q);
        rotations += 1;
    }
    if final_offdiag > jacobi_tol {
        return Err(SpectralError::JacobiNotConverged {
            iterations: rotations,
            residual: final_offdiag as f32,
            tol: jacobi_tol as f32,
            dim,
        });
    }
    let largest = (0..dim)
        .max_by(|left, right| matrix[*left][*left].total_cmp(&matrix[*right][*right]))
        .ok_or(SpectralError::SingularMatrix)?;
    let mut eigenvector = vectors.iter().map(|row| row[largest]).collect::<Vec<_>>();
    normalize_f64(&mut eigenvector)?;
    Ok((matrix[largest][largest], eigenvector))
}

fn identity_f64(dim: usize) -> Vec<Vec<f64>> {
    let mut identity = vec![vec![0.0_f64; dim]; dim];
    for (index, row) in identity.iter_mut().enumerate() {
        row[index] = 1.0;
    }
    identity
}

fn max_offdiag_f64(matrix: &[Vec<f64>]) -> Option<(usize, usize, f64)> {
    let mut best = None::<(usize, usize, f64)>;
    for (row, values) in matrix.iter().enumerate() {
        for (col, value) in values.iter().copied().enumerate().skip(row + 1) {
            if best.is_none_or(|(_, _, current)| value.abs() > current.abs()) {
                best = Some((row, col, value));
            }
        }
    }
    best
}

fn rotate_symmetric_f64(matrix: &mut [Vec<f64>], vectors: &mut [Vec<f64>], p: usize, q: usize) {
    let theta = 0.5 * (2.0 * matrix[p][q]).atan2(matrix[q][q] - matrix[p][p]);
    let (sin, cos) = theta.sin_cos();
    for row in matrix.iter_mut() {
        let prior_p = row[p];
        let prior_q = row[q];
        row[p] = cos * prior_p - sin * prior_q;
        row[q] = sin * prior_p + cos * prior_q;
    }
    let (before_q, from_q) = matrix.split_at_mut(q);
    let row_p = &mut before_q[p];
    let row_q = &mut from_q[0];
    for (value_p, value_q) in row_p.iter_mut().zip(row_q.iter_mut()) {
        let prior_p = *value_p;
        let prior_q = *value_q;
        *value_p = cos * prior_p - sin * prior_q;
        *value_q = sin * prior_p + cos * prior_q;
    }
    matrix[p][q] = 0.0;
    matrix[q][p] = 0.0;
    for row in vectors {
        let prior_p = row[p];
        let prior_q = row[q];
        row[p] = cos * prior_p - sin * prior_q;
        row[q] = sin * prior_p + cos * prior_q;
    }
}

fn expand_ritz_vector_f64(basis: &[Vec<f64>], coefficients: &[f64]) -> Vec<f64> {
    let mut expanded = vec![0.0_f64; basis.first().map_or(0, Vec::len)];
    for (basis_vector, coefficient) in basis.iter().zip(coefficients) {
        for (value, basis_value) in expanded.iter_mut().zip(basis_vector) {
            *value += coefficient * basis_value;
        }
    }
    expanded
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

/// Relative backward-error certificate for a unit vector and its product.
/// The positive Perron candidate makes `mu` positive; the absolute value keeps
/// the helper correct if its scope widens.
fn eigenpair_residual_f64(product: &[f64], vector: &[f64]) -> (f64, f64) {
    let mu = product
        .iter()
        .zip(vector)
        .map(|(product_value, vector_value)| product_value * vector_value)
        .sum::<f64>();
    let residual = product
        .iter()
        .zip(vector)
        .map(|(product_value, vector_value)| {
            let delta = product_value - mu * vector_value;
            delta * delta
        })
        .sum::<f64>()
        .sqrt();
    (mu, residual / mu.abs().max(f64::MIN_POSITIVE))
}

fn l2_norm_f64(vector: &[f64]) -> f64 {
    vector.iter().map(|value| value * value).sum::<f64>().sqrt()
}

fn clean_zero(value: f32) -> f32 {
    if value.abs() < EIGEN_EPS { 0.0 } else { value }
}

// IMPORTANT: spectral centrality is structure-only; the MFVS kernel is outcome-anchored (A2).
// Centrality proposes candidates; grounding through oracle anchors confirms them.
