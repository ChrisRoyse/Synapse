use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ForgeError;

pub type Result<T> = std::result::Result<T, ForgeError>;

/// Backend operations implemented by the Stage 2 `Backend` trait.
pub const FORGE_SHIPPED_BACKEND_OPS: &[&str] = &[
    "gemm",
    "cosine",
    "dot",
    "l2",
    "normalize",
    "topk",
    "knn",
    "paired_cosine",
    "device_info",
];

/// PRD-listed Forge operations that are intentionally not part of the Stage 2 trait yet.
pub const FORGE_DEFERRED_BACKEND_OPS: &[&str] = &[
    "histogram_nmi",
    "spmm_sparse_ops",
    "graph_ops",
    "colbert_maxsim",
];

/// PRD-listed operations that are deliberately CPU-routed until profile data
/// proves a dedicated GPU path beats the already-bounded production path.
pub const FORGE_CPU_ROUTED_BACKEND_OPS: &[&str] = &[
    "histogram_nmi",
    "spmm_sparse_ops",
    "graph_ops",
    "colbert_maxsim",
];

/// Exact CUDA `topk` is currently guaranteed only for global `k <= 1024`.
pub const CUDA_EXACT_TOPK_MAX_K: usize = 1024;

pub trait Backend: Send + Sync {
    fn gemm(
        &self,
        a: &[f32],
        b: &[f32],
        m: usize,
        k: usize,
        n: usize,
        out: &mut [f32],
    ) -> Result<()>;
    fn cosine(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()>;
    fn dot(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()>;
    fn l2(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()>;
    fn normalize(&self, vecs: &mut [f32], dim: usize) -> Result<()>;
    fn topk(&self, scores: &[f32], k: usize) -> Result<Vec<(usize, f32)>>;
    fn knn(
        &self,
        queries: &[f32],
        candidates: &[f32],
        query_count: usize,
        dim: usize,
        k: usize,
        metric: KnnMetric,
    ) -> Result<KnnBatch>;
    fn paired_cosine(
        &self,
        left: &[f32],
        right: &[f32],
        pair_count: usize,
        dim: usize,
        out: &mut [f32],
    ) -> Result<()>;
    fn device_info(&self) -> DeviceInfo;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Cpu,
    Cuda,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu => f.write_str("cpu"),
            Self::Cuda => f.write_str("cuda"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BestConfig {
    pub backend: BackendKind,
    pub tile_m: usize,
    pub tile_n: usize,
    pub tile_k: usize,
    pub extra: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceInfo {
    pub kind: BackendKind,
    pub name: String,
    pub vram_mib: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnnMetric {
    Cosine,
    Dot,
    L2Squared,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct KnnBatch {
    pub query_count: usize,
    pub k: usize,
    pub candidate_count: usize,
    pub metric: KnnMetric,
    pub indices: Vec<usize>,
    pub scores: Vec<f32>,
}

impl KnnBatch {
    pub fn new(
        query_count: usize,
        k: usize,
        candidate_count: usize,
        metric: KnnMetric,
        indices: Vec<usize>,
        scores: Vec<f32>,
    ) -> Result<Self> {
        let expected = query_count
            .checked_mul(k)
            .ok_or_else(|| ForgeError::ShapeMismatch {
                expected: vec![query_count, k],
                got: vec![indices.len(), scores.len()],
                remediation: "knn result shape overflows usize".to_string(),
            })?;
        if indices.len() != expected || scores.len() != expected {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![expected, expected],
                got: vec![indices.len(), scores.len()],
                remediation: "knn indices and scores must both be query_count*k".to_string(),
            });
        }
        for (offset, (index, score)) in indices.iter().zip(&scores).enumerate() {
            if *index >= candidate_count {
                return Err(ForgeError::NumericalInvariant {
                    op: "knn".to_string(),
                    detail: format!("result offset {offset} has out-of-range index {index}"),
                    remediation: "check knn top-k merge and candidate_count bounds".to_string(),
                });
            }
            if !score.is_finite() {
                return Err(ForgeError::NumericalInvariant {
                    op: "knn".to_string(),
                    detail: format!("result offset {offset} has non-finite score {score}"),
                    remediation: "check knn distance kernel output before ranking".to_string(),
                });
            }
        }
        Ok(Self {
            query_count,
            k,
            candidate_count,
            metric,
            indices,
            scores,
        })
    }

    pub fn row(&self, query_idx: usize) -> Option<impl Iterator<Item = (usize, f32)> + '_> {
        if query_idx >= self.query_count {
            return None;
        }
        let start = query_idx * self.k;
        let end = start + self.k;
        Some(
            self.indices[start..end]
                .iter()
                .copied()
                .zip(self.scores[start..end].iter().copied()),
        )
    }
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            kind: BackendKind::Cpu,
            name: "cpu".to_string(),
            vram_mib: None,
        }
    }
}
