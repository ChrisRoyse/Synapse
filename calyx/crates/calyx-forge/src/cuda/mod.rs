pub mod algorithmic;
pub mod assay;
pub mod context;
pub mod distance;
pub mod gemm;
pub mod green_context;
pub mod grouped_gemm;
pub mod kernels;
pub mod postprocess;
pub mod profile;
pub mod quant;
pub mod ragged_gemm;
pub mod resident;
pub mod topk;
mod validate;

use crate::{Backend, CUDA_EXACT_TOPK_MAX_K, DeviceInfo, ForgeError, KnnBatch, KnnMetric, Result};

pub use crate::mxfp4;
pub use algorithmic::{
    ALGORITHMIC_SPARSE_MAX_TOKEN_BYTES, ALGORITHMIC_TOKEN_HASH_MAX_TOKEN_BYTES,
    CudaAlgorithmicContext, CudaAlgorithmicStats, CudaByteFeatureRaw, CudaByteRaggedBatch,
};
pub use assay::{
    CUDA_GRANGER_STATUS_INVALID_LAG, CUDA_GRANGER_STATUS_NONFINITE, CUDA_GRANGER_STATUS_OK,
    CUDA_GRANGER_STATUS_RANK_DEFICIENT, CudaAutocorrelationSums, CudaCcmPredictions,
    CudaCorrelationPrecision, CudaCrossCorrelationBatch, CudaDcorResult, CudaGrangerLagBatch,
    CudaGrangerLagSummary, CudaHawkesFit, CudaHsicResult, CudaKsgContinuousCounts,
    CudaLinearCkaPairEstimates, CudaLogisticConfig, CudaLogisticDataset, CudaLogisticSplits,
    CudaLogisticSummaries, CudaMixedKsgCounts, CudaMmdChangePointResult, CudaMmdResult,
    CudaPeriodogramBatch, autocorrelation_sums_host, ccm_simplex_predictions_host,
    correlation_precision_host, cross_correlation_batch_host, dcor_1d_host, entropy_radii_host,
    gaussian_mmd_device_bytes, gaussian_mmd_host, granger_lag_summaries_host, hawkes_em_host,
    hsic_1d_host, ksg_continuous_counts_host, linear_cka_pair_estimates_host,
    logistic_summaries_host, mixed_ksg_counts_host, mmd_change_point_host, periodogram_batch_host,
};
pub use context::{CudaContext, init_cuda, query_device_info};
pub use distance::{
    cosine_batch_gpu, dot_batch_gpu, l2_batch_gpu, normalize_rows_gpu, paired_cosine_gpu,
    paired_cosine_host,
};
pub use gemm::{
    bench_gemm_cublas, bench_gemm_reference_cublas, gemm_cublas, gemm_mxfp4_fp32_accum,
    gemm_mxfp8_fp32_accum, probe_allocation,
};
pub use green_context::CudaGreenContextStream;
pub use grouped_gemm::{
    AbsentSlotSentinel, GemmProblem, GroupedGemmExecutionMode, GroupedGemmPlan,
    build_grouped_gemm_plan, execute_grouped_gemm, execute_grouped_gemm_strict,
    read_grouped_gemm_output,
};
pub use postprocess::{
    CudaDenseTokenPostprocess, CudaMultiRows, CudaPostprocessPooling, CudaSparseRows,
    bgem3_colbert_tokens_from_external_f32, bgem3_sparse_from_external_f32,
    colbert_tokens_from_external_f32, dense_2d_from_external_f32, dense_tokens_from_external_f32,
    sparse_positive_from_external_f32,
};
pub use profile::{ProfilePairwiseCudaStats, pairwise_euclidean_gram_tiled_host};
pub use quant::{
    CudaBinaryBatch, CudaBinaryScores, CudaMxFpBatch, CudaQuantContext, CudaQuantScores,
    CudaQuantStats, CudaTurboQuantBatch, CudaTurboQuantScores,
};
pub use ragged_gemm::{
    RaggedBatch, build_ragged_batch, build_ragged_batch_from_slabs, extract_ragged_results,
    try_extract_ragged_results,
};
pub use resident::{DeviceCandidateBlock, cosine_resident_host, upload_candidate_block};
pub use topk::topk_gpu;

#[derive(Clone, Debug)]
pub struct CudaBackend {
    ctx: CudaContext,
}

impl CudaBackend {
    pub fn new() -> Result<Self> {
        init_cuda(0, false).map(|ctx| Self { ctx })
    }

    pub fn with_context(ctx: CudaContext) -> Self {
        Self { ctx }
    }

    pub fn context(&self) -> &CudaContext {
        &self.ctx
    }

    pub fn grouped_gemm(&self, plan: &mut GroupedGemmPlan) -> Result<()> {
        grouped_gemm::execute_grouped_gemm(&self.ctx, plan)
    }

    pub fn grouped_gemm_strict(&self, plan: &mut GroupedGemmPlan) -> Result<()> {
        grouped_gemm::execute_grouped_gemm_strict(&self.ctx, plan)
    }
}

impl Backend for CudaBackend {
    fn gemm(
        &self,
        a: &[f32],
        b: &[f32],
        m: usize,
        k: usize,
        n: usize,
        out: &mut [f32],
    ) -> Result<()> {
        gemm::gemm_host(&self.ctx, a, b, m, k, n, out)
    }

    fn cosine(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        distance::cosine_host(&self.ctx, a, b, dim, out)
    }

    fn dot(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        distance::dot_host(&self.ctx, a, b, dim, out)
    }

    fn l2(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        distance::l2_host(&self.ctx, a, b, dim, out)
    }

    fn normalize(&self, vecs: &mut [f32], dim: usize) -> Result<()> {
        distance::normalize_host(&self.ctx, vecs, dim)
    }

    fn topk(&self, scores: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
        topk::topk_host(&self.ctx, scores, k)
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
        knn_cuda(&self.ctx, queries, candidates, query_count, dim, k, metric)
    }

    fn paired_cosine(
        &self,
        left: &[f32],
        right: &[f32],
        pair_count: usize,
        dim: usize,
        out: &mut [f32],
    ) -> Result<()> {
        distance::paired_cosine_host(&self.ctx, left, right, pair_count, dim, out)
    }

    fn device_info(&self) -> DeviceInfo {
        query_device_info(&self.ctx)
    }
}

/// Upper bound on the f32 score matrix one batched kNN launch materializes on
/// the device.
///
/// The batched path (#2107 H4) trades one score buffer per query for one score
/// matrix per tile. This constant is what keeps that matrix a bounded,
/// *measurable* shape: the VRAM admission measurement computes the identical
/// tile from [`knn_query_tile`], so the reserved bytes are the bytes the
/// kernels allocate.
pub const CUDA_KNN_TILE_SCORE_BYTES: usize = 256 * 1024 * 1024;

/// Query rows processed by one batched kNN launch for this candidate count.
#[must_use]
pub fn knn_query_tile(candidate_count: usize, query_count: usize) -> usize {
    if candidate_count == 0 || query_count == 0 {
        return 0;
    }
    let budget_rows = (CUDA_KNN_TILE_SCORE_BYTES / size_of::<f32>()) / candidate_count;
    budget_rows
        .max(1)
        .min(query_count)
        .min(distance::DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH)
        .min(topk::TOPK_MAX_ROWS_PER_LAUNCH)
}

fn knn_cuda(
    ctx: &CudaContext,
    queries: &[f32],
    candidates: &[f32],
    query_count: usize,
    dim: usize,
    k: usize,
    metric: KnnMetric,
) -> Result<KnnBatch> {
    let candidate_count =
        crate::cpu::validate_knn_shape_only(queries, candidates, query_count, dim)?;
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
    // Every metric now ranks on the device, so every metric carries the exact
    // top-k bound. `L2Squared` used to escape it by sorting all candidates on
    // the host (#2107 H2); that host sort is gone.
    if k_eff > CUDA_EXACT_TOPK_MAX_K {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![CUDA_EXACT_TOPK_MAX_K],
            got: vec![k_eff],
            remediation: format!(
                "cuda knn uses exact device topk for every metric and is bounded to k <= {CUDA_EXACT_TOPK_MAX_K}"
            ),
        });
    }
    let total_hits = query_count
        .checked_mul(k_eff)
        .ok_or_else(|| ForgeError::ShapeMismatch {
            expected: vec![query_count, k_eff],
            got: vec![usize::MAX],
            remediation: "cuda knn output shape overflows usize".to_string(),
        })?;
    let mut indices = Vec::with_capacity(total_hits);
    let mut scores = Vec::with_capacity(total_hits);
    let stream = ctx.inner().default_stream();

    // One upload for the whole call, then a device-side finiteness gate over
    // the uploaded bytes (#2107 H3) instead of a host scalar pre-scan.
    let candidates_dev = stream
        .clone_htod(candidates)
        .map_err(|err| cuda_knn_device(ctx, format!("candidate upload failed: {err}")))?;
    distance::check_uploaded_finite(ctx, "knn", candidates, &candidates_dev)?;
    let queries_dev = stream
        .clone_htod(queries)
        .map_err(|err| cuda_knn_device(ctx, format!("query upload failed: {err}")))?;
    distance::check_uploaded_finite(ctx, "knn", queries, &queries_dev)?;

    let (kernel_name, op, sentinel, order) = match metric {
        KnnMetric::Cosine => (
            "cosine_batch_f32",
            "cosine_batch_gpu",
            true,
            topk::TopkOrder::MaxK,
        ),
        KnnMetric::Dot => (
            "dot_batch_f32",
            "dot_batch_gpu",
            false,
            topk::TopkOrder::MaxK,
        ),
        KnnMetric::L2Squared => ("l2_batch_f32", "l2_batch_gpu", false, topk::TopkOrder::MinK),
    };

    let tile = knn_query_tile(candidate_count, query_count);
    let mut first_query = 0_usize;
    while first_query < query_count {
        let rows = tile.min(query_count - first_query);
        let query_offset = first_query * dim;
        let tile_queries = queries_dev
            .try_slice(query_offset..query_offset + rows * dim)
            .ok_or_else(|| {
                cuda_knn_device(ctx, format!("query tile slice at row {first_query} failed"))
            })?;
        let score_len =
            rows.checked_mul(candidate_count)
                .ok_or_else(|| ForgeError::ShapeMismatch {
                    expected: vec![rows, candidate_count],
                    got: vec![usize::MAX],
                    remediation: "cuda knn score matrix shape overflows usize".to_string(),
                })?;
        let mut scores_dev = stream
            .alloc_zeros(score_len)
            .map_err(|err| cuda_knn_device(ctx, format!("score allocation failed: {err}")))?;
        distance::launch_distance_batched(
            ctx,
            op,
            kernel_name,
            &tile_queries,
            &candidates_dev,
            dim,
            candidate_count,
            rows,
            &mut scores_dev,
        )?;
        distance::check_device_output(ctx, op, &scores_dev, sentinel)?;
        let ranked = topk::topk_batched_gpu(ctx, &scores_dev, rows, candidate_count, k_eff, order)?;
        indices.extend_from_slice(&ranked.indices);
        scores.extend_from_slice(&ranked.scores);
        first_query += rows;
    }
    KnnBatch::new(query_count, k_eff, candidate_count, metric, indices, scores)
}

fn cuda_knn_device(ctx: &CudaContext, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: format!("cuda:{}", ctx.device_idx()),
        detail: format!("cuda knn {detail}"),
        remediation: "Check CUDA KNN inputs, VRAM, and device availability".to_string(),
    }
}
