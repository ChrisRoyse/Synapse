#![deny(warnings)]

//! Directed graph primitives for Calyx grounding kernels.

pub mod betweenness;
pub mod clustering;
mod error;
pub mod graph_builder;
pub mod lp_scaffold;
pub mod pagerank;
pub mod scc;
pub mod spectral;
mod spectral_linalg;
pub mod structural;

pub use betweenness::{
    betweenness, betweenness_auto, betweenness_sampled, betweenness_top_k,
    betweenness_top_k_sampled,
};
pub use clustering::{clustering_coefficients, local_clustering_coefficient};
pub use error::{MincutError, Result};
pub use graph_builder::{AgreementEdge, CitationEdge, FrequencyEntry, build_assoc_graph};
pub use lp_scaffold::{
    ConstraintSense, LpConstraint, LpProblem, LpSolution, LpVariable, MFVS_LP_MAX_NODES,
    MFVS_LP_MAX_SEARCH_STATES, OptSense, SolveStatus, mfvs_lp_problem, solve_mfvs_lp,
    verify_feedback_vertex_set,
};
pub use pagerank::{
    PAGERANK_DEFAULT_DAMPING, PAGERANK_DEFAULT_MAX_ITER, PAGERANK_DEFAULT_TOL, pagerank,
    pagerank_default,
};
pub use scc::{CondensedEdge, CondensedGraph, SccResult, condensate, tarjan_scc};
pub use spectral::{
    EigenPair, SparseGraph, SpectralCache, SpectralCacheEntry, SpectralCacheKey, SpectralError,
    SpectralResult, eigenvector_centrality, gft_project, gft_reconstruct, laplacian_eigenmaps,
    laplacian_eigenmaps_with_max_iter, spectral_gap,
};
pub use structural::{
    StructuralParams, StructuralSignature, TransitionEdge, build_transition_graph,
    structural_signatures,
};
