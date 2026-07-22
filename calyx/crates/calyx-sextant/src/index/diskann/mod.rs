//! DiskANN on-disk graph index (PH68, server-only).
//!
//! Embedded vaults keep using the in-RAM HNSW from PH23; this module is the
//! NVMe-resident Vamana graph used by server-scale slots.

pub mod build;
pub mod concat;
#[cfg(sextant_cuvs)]
mod cuvs_cagra;
pub mod dual;
pub mod graph;
pub mod pq;
pub mod search;
pub mod token;
mod token_sidecar;

pub use build::{
    DiskAnnBuildBackend, DiskAnnBuildParams, DiskAnnBuildProgress, build_diskann_graph,
    build_diskann_graph_with_backend, build_diskann_graph_with_backend_and_progress,
};
pub use concat::{ConcatCrossTermDiskAnn, ConcatCrossTermHit, ConcatCrossTermKey};
pub use dual::{
    Direction, DirectionalBoost, DualDiskAnnSearch, build_dual, build_dual_with_search,
    dual_graph_path, open_dual,
};
pub use graph::{
    DiskAnnGraphReader, DiskAnnGraphWriter, DiskAnnHeader, DiskAnnNodeRef, node_block_size,
    open_diskann_graph,
};
pub use pq::{
    DISKANN_PQ_SMALL_CORPUS_ROWS, DiskAnnPqBuildDiagnostics, DiskAnnPqBuildExecution,
    DiskAnnPqBuildParams, DiskAnnPqIndex,
};
pub use search::{
    DiskAnnPqSearchBuild, DiskAnnSearch, DiskAnnSearchParams, RAW_SIDECAR_HEADER_SIZE,
    RAW_SIDECAR_MAGIC, RAW_SIDECAR_PACKED_VERSION,
};
pub use token::TokenDiskAnnMaxSim;
