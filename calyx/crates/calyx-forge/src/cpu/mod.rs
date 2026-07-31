pub mod distance;
pub mod gemm;
pub mod guard;
pub mod normalize;
pub mod simd;
pub mod topk;

use crate::{Backend, DeviceInfo, ForgeError, KnnBatch, KnnMetric, Result};

#[derive(Clone, Debug)]
pub struct CpuBackend {
    avx512: bool,
}

impl CpuBackend {
    pub fn new() -> Self {
        let avx512 = avx512_available();
        if !avx512 {
            tracing::warn!(
                "CALYX_FORGE_CPU_AVX512_UNAVAILABLE falling back to f32x8-compatible path"
            );
        }
        Self { avx512 }
    }

    pub fn avx512_available(&self) -> bool {
        self.avx512
    }

    /// Which CPU kernel family the reductions actually dispatched to on this host.
    ///
    /// This used to answer `"f32x16"` / `"f32x8"` from the AVX-512 flag alone,
    /// which was wrong in both directions: the crate had no AVX-512 kernels for
    /// distance at all, and `"f32x8"` described a `wide` type that lowers to two
    /// 128-bit SSE operations at this workspace's `x86-64-v2` baseline. Health
    /// surfaced that string as `calyx_math_cpu_simd_path`, so the daemon reported
    /// a width it was not running. It now reports the runtime dispatch decision.
    pub fn simd_path(&self) -> &'static str {
        simd::backend_name()
    }
}

impl Default for CpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for CpuBackend {
    fn gemm(
        &self,
        a: &[f32],
        b: &[f32],
        m: usize,
        k: usize,
        n: usize,
        out: &mut [f32],
    ) -> Result<()> {
        gemm::gemm_f32(a, b, m, k, n, out)
    }

    fn cosine(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        distance::cosine_batch(a, b, dim, out)
    }

    fn dot(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        distance::dot_batch(a, b, dim, out)
    }

    fn l2(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        distance::l2_batch(a, b, dim, out)
    }

    fn normalize(&self, vecs: &mut [f32], dim: usize) -> Result<()> {
        normalize::normalize_f32(vecs, dim)
    }

    fn topk(&self, scores: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
        topk::topk_f32(scores, k)
    }

    fn knn(
        &self,
        queries: &[f32],
        candidates: &[f32],
        query_count: usize,
        dim: usize,
        k: usize,
        metric: KnnMetric,
    ) -> Result<KnnBatch> {
        knn_cpu(self, queries, candidates, query_count, dim, k, metric)
    }

    fn paired_cosine(
        &self,
        left: &[f32],
        right: &[f32],
        pair_count: usize,
        dim: usize,
        out: &mut [f32],
    ) -> Result<()> {
        distance::paired_cosine_batch(left, right, pair_count, dim, out)
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            kind: crate::BackendKind::Cpu,
            name: "calyx-cpu".to_string(),
            avx512: self.avx512,
            vram_mib: None,
        }
    }
}

pub use distance::{cosine_batch, dot_batch, l2_batch, paired_cosine_batch};
pub use gemm::gemm_f32;
pub use guard::{check_finite, check_norm_positive, check_shape_2d};
pub use normalize::normalize_f32;
pub use topk::topk_f32;

fn knn_cpu(
    backend: &CpuBackend,
    queries: &[f32],
    candidates: &[f32],
    query_count: usize,
    dim: usize,
    k: usize,
    metric: KnnMetric,
) -> Result<KnnBatch> {
    let candidate_count = validate_knn_shape(queries, candidates, query_count, dim)?;
    let k_eff = k.min(candidate_count);
    if query_count == 0 || k_eff == 0 {
        return KnnBatch::new(
            query_count,
            0,
            candidate_count,
            metric,
            Vec::new(),
            Vec::new(),
        );
    }
    let mut indices = Vec::with_capacity(query_count * k_eff);
    let mut scores_out = Vec::with_capacity(query_count * k_eff);
    let mut scores = vec![0.0_f32; candidate_count];
    for query in queries.chunks_exact(dim) {
        match metric {
            KnnMetric::Cosine => backend.cosine(query, candidates, dim, &mut scores)?,
            KnnMetric::Dot => backend.dot(query, candidates, dim, &mut scores)?,
            KnnMetric::L2Squared => backend.l2(query, candidates, dim, &mut scores)?,
        }
        let ranked = rank_scores(&scores, k_eff, metric)?;
        for (index, score) in ranked {
            indices.push(index);
            scores_out.push(score);
        }
    }
    KnnBatch::new(
        query_count,
        k_eff,
        candidate_count,
        metric,
        indices,
        scores_out,
    )
}

pub(crate) fn validate_knn_shape(
    queries: &[f32],
    candidates: &[f32],
    query_count: usize,
    dim: usize,
) -> Result<usize> {
    if dim == 0 {
        if queries.is_empty() && candidates.is_empty() && query_count == 0 {
            return Ok(0);
        }
        return Err(ForgeError::ShapeMismatch {
            expected: vec![0, 0],
            got: vec![query_count, queries.len(), candidates.len()],
            remediation: "knn dim=0 is valid only for an empty query and candidate set".to_string(),
        });
    }
    check_shape_2d(queries, query_count, dim, "knn queries")?;
    if !candidates.len().is_multiple_of(dim) {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![dim],
            got: vec![candidates.len()],
            remediation: "knn candidates length must be an integer number of dim-wide rows"
                .to_string(),
        });
    }
    check_finite(queries, "knn")?;
    check_finite(candidates, "knn")?;
    Ok(candidates.len() / dim)
}

fn rank_scores(scores: &[f32], k: usize, metric: KnnMetric) -> Result<Vec<(usize, f32)>> {
    match metric {
        KnnMetric::Cosine | KnnMetric::Dot => topk::topk_f32(scores, k),
        KnnMetric::L2Squared => {
            let negated = scores.iter().map(|score| -*score).collect::<Vec<_>>();
            Ok(topk::topk_f32(&negated, k)?
                .into_iter()
                .map(|(index, score)| (index, -score))
                .collect())
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn avx512_available() -> bool {
    std::arch::is_x86_feature_detected!("avx512f")
}

#[cfg(not(target_arch = "x86_64"))]
fn avx512_available() -> bool {
    false
}
