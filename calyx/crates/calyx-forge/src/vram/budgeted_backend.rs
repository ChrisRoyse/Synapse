//! CUDA backend wrapper that admits every host-backed dispatch through the
//! process-local [`VramBudgeter`] before the inner backend can allocate or
//! mutate a device output.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::vram::{
    CudaVramProbe, HostGpuReservation, HostGpuReservationRequest, HostGpuReservationStore,
    VramBudgeter, VramStats,
};
use crate::{
    Backend, CUDA_EXACT_TOPK_MAX_K, CudaBackend, DeviceInfo, ForgeError, KnnBatch, KnnMetric,
    Result,
};

const F32_BYTES: usize = size_of::<f32>();
const I32_BYTES: usize = size_of::<i32>();
const BYTES_PER_MIB: usize = 1024 * 1024;
const DISPATCH_REMEDIATION: &str = "reduce the exact input/batch dimensions or raise the explicit Forge VRAM budget only after measuring the operation; never bypass admission or move the dispatch to CPU implicitly";

/// CUDA backend whose every public [`Backend`] operation has a measured
/// buffer-shape reservation held until the inner CUDA call returns.
///
/// The reservation is based on the device buffers the concrete Forge kernels
/// allocate for that operation, not on the configured ceiling. The ceiling is
/// only the upper bound enforced by [`VramBudgeter`].
pub struct VramBudgetedCudaBackend {
    inner: CudaBackend,
    budgeter: VramBudgeter<CudaVramProbe>,
    host_dispatch: Option<HostDispatchReservations>,
}

struct HostDispatchReservations {
    store: HostGpuReservationStore,
    owner: String,
    job_id_prefix: String,
    next_dispatch_id: AtomicU64,
}

impl VramBudgetedCudaBackend {
    /// Wraps an initialized CUDA backend with an explicit process-local cap.
    ///
    /// # Errors
    ///
    /// Returns `CALYX_FORGE_VRAM_BUDGET` when the cap is zero or cannot fit in
    /// this process.
    pub fn new(inner: CudaBackend, soft_cap_bytes: u64) -> Result<Self> {
        let soft_cap_bytes = usize::try_from(soft_cap_bytes).map_err(|_| {
            budget_error(format!(
                "configured soft cap {soft_cap_bytes} does not fit usize"
            ))
        })?;
        if soft_cap_bytes == 0 {
            return Err(budget_error(
                "configured soft cap must be greater than zero".to_owned(),
            ));
        }
        let probe = CudaVramProbe::new(Arc::new(inner.context().clone()));
        Ok(Self {
            inner,
            budgeter: VramBudgeter::with_soft_cap(soft_cap_bytes, probe),
            host_dispatch: None,
        })
    }

    /// Enables measured host-wide admission for every subsequent backend
    /// dispatch. The process-local budget remains the concurrency ceiling;
    /// each host row represents only the concrete device buffers for one call.
    pub fn with_host_dispatch_reservations(
        mut self,
        store: HostGpuReservationStore,
        owner: impl Into<String>,
        job_id_prefix: impl Into<String>,
    ) -> Result<Self> {
        let owner = owner.into();
        let job_id_prefix = job_id_prefix.into();
        for (field, value) in [
            ("owner", owner.as_str()),
            ("job_id_prefix", job_id_prefix.as_str()),
        ] {
            if value.trim().is_empty() || value.len() > 384 || value.chars().any(char::is_control) {
                return Err(budget_error(format!(
                    "host dispatch {field} must be non-blank, control-free, and at most 384 bytes"
                )));
            }
        }
        self.host_dispatch = Some(HostDispatchReservations {
            store,
            owner,
            job_id_prefix,
            next_dispatch_id: AtomicU64::new(1),
        });
        Ok(self)
    }

    #[must_use]
    pub fn soft_cap_bytes(&self) -> usize {
        self.budgeter.soft_cap_bytes()
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.budgeter.allocated_bytes()
    }

    #[must_use]
    pub fn stats(&self) -> VramStats {
        self.budgeter.stats()
    }

    /// Returns process-local allocation counters plus a fail-closed live CUDA
    /// free-memory read.
    pub fn stats_strict(&self) -> Result<VramStats> {
        self.budgeter.stats_strict()
    }

    fn measured_f32_bytes(
        &self,
        operation: &'static str,
        element_counts: &[usize],
    ) -> Result<usize> {
        let elements = checked_sum(operation, element_counts)?;
        elements.checked_mul(F32_BYTES).ok_or_else(|| {
            budget_error(format!(
                "{operation} f32 device-buffer byte count overflow: elements={elements}"
            ))
        })
    }

    fn measured_topk_bytes(
        &self,
        operation: &'static str,
        score_count: usize,
        k: usize,
        already_device_resident_scores: bool,
    ) -> Result<usize> {
        let k_eff = k.min(score_count);
        let chunks = score_count.div_ceil(CUDA_EXACT_TOPK_MAX_K);
        let output_entries = chunks.checked_mul(k_eff).ok_or_else(|| {
            budget_error(format!(
                "{operation} top-k temporary shape overflow: chunks={chunks} k={k_eff}"
            ))
        })?;
        let score_bytes = if already_device_resident_scores {
            0
        } else {
            score_count.checked_mul(F32_BYTES).ok_or_else(|| {
                budget_error(format!(
                    "{operation} score byte count overflow: scores={score_count}"
                ))
            })?
        };
        let output_bytes = output_entries
            .checked_mul(F32_BYTES + I32_BYTES)
            .ok_or_else(|| {
                budget_error(format!(
                    "{operation} top-k output byte count overflow: entries={output_entries}"
                ))
            })?;
        score_bytes.checked_add(output_bytes).ok_or_else(|| {
            budget_error(format!(
                "{operation} aggregate device-buffer byte count overflow"
            ))
        })
    }

    fn measured_knn_bytes(
        &self,
        queries: &[f32],
        candidates: &[f32],
        dim: usize,
        k: usize,
        metric: KnnMetric,
    ) -> Result<usize> {
        if queries.is_empty() || candidates.is_empty() || dim == 0 || k == 0 {
            return Ok(0);
        }
        let candidate_count = candidates.len() / dim;
        let k_eff = k.min(candidate_count);
        let topk_entries = if matches!(metric, KnnMetric::Cosine | KnnMetric::Dot) {
            let chunks = candidate_count.div_ceil(CUDA_EXACT_TOPK_MAX_K);
            chunks.checked_mul(k_eff).ok_or_else(|| {
                budget_error(format!(
                    "knn top-k temporary shape overflow: chunks={chunks} k={k_eff}"
                ))
            })?
        } else {
            0
        };
        let resident_f32 = checked_sum(
            "knn",
            &[
                candidates.len(),
                dim.min(queries.len()),
                candidate_count,
                topk_entries,
            ],
        )?;
        let f32_bytes = resident_f32.checked_mul(F32_BYTES).ok_or_else(|| {
            budget_error(format!(
                "knn f32 device-buffer byte count overflow: elements={resident_f32}"
            ))
        })?;
        let index_bytes = topk_entries.checked_mul(I32_BYTES).ok_or_else(|| {
            budget_error(format!(
                "knn index device-buffer byte count overflow: entries={topk_entries}"
            ))
        })?;
        f32_bytes.checked_add(index_bytes).ok_or_else(|| {
            budget_error("knn aggregate device-buffer byte count overflow".to_owned())
        })
    }

    fn acquire_host_dispatch(
        &self,
        operation: &'static str,
        measured_bytes: usize,
    ) -> Result<Option<HostGpuReservation>> {
        if measured_bytes == 0 {
            return Ok(None);
        }
        let Some(host) = self.host_dispatch.as_ref() else {
            return Ok(None);
        };
        let dispatch_id = host.next_dispatch_id.fetch_add(1, Ordering::Relaxed);
        let requested_mib = u64::try_from(measured_bytes.div_ceil(BYTES_PER_MIB))
            .map_err(|_| budget_error(format!("{operation} measured MiB does not fit u64")))?;
        host.store
            .acquire(HostGpuReservationRequest::new(
                host.owner.clone(),
                format!("{}-{dispatch_id}", host.job_id_prefix),
                format!(
                    "forge operation={operation} measured_device_buffer_bytes={measured_bytes}"
                ),
                requested_mib,
            ))
            .map(Some)
    }

    fn run_reserved<T>(
        &self,
        operation: &'static str,
        measured_bytes: usize,
        dispatch: impl FnOnce(&CudaBackend) -> Result<T>,
    ) -> Result<T> {
        let _process_reservation = self.budgeter.reserve(measured_bytes)?;
        let host_reservation = self.acquire_host_dispatch(operation, measured_bytes)?;
        let operation_result = dispatch(&self.inner);
        let release_result = host_reservation
            .map(HostGpuReservation::release)
            .transpose();
        match (operation_result, release_result) {
            (Ok(value), Ok(_readback)) => Ok(value),
            (Err(operation_error), Ok(_readback)) => Err(operation_error),
            (Ok(_value), Err(release_error)) => Err(release_error),
            (Err(operation_error), Err(release_error)) => {
                tracing::error!(
                    target: "calyx_forge::vram::budgeted_backend",
                    operation,
                    operation_code = operation_error.code(),
                    operation_error = %operation_error,
                    release_code = release_error.code(),
                    release_error = %release_error,
                    "CUDA dispatch failed and its exact host reservation also failed to release"
                );
                Err(release_error)
            }
        }
    }
}

impl Backend for VramBudgetedCudaBackend {
    fn gemm(
        &self,
        a: &[f32],
        b: &[f32],
        m: usize,
        k: usize,
        n: usize,
        out: &mut [f32],
    ) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("gemm", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("gemm", measured_bytes, |inner| {
            inner.gemm(a, b, m, k, n, out)
        })
    }

    fn cosine(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("cosine", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("cosine", measured_bytes, |inner| {
            inner.cosine(a, b, dim, out)
        })
    }

    fn dot(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("dot", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("dot", measured_bytes, |inner| inner.dot(a, b, dim, out))
    }

    fn l2(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("l2", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("l2", measured_bytes, |inner| inner.l2(a, b, dim, out))
    }

    fn normalize(&self, vecs: &mut [f32], dim: usize) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("normalize", &[vecs.len()])?;
        self.run_reserved("normalize", measured_bytes, |inner| {
            inner.normalize(vecs, dim)
        })
    }

    fn topk(&self, scores: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
        let measured_bytes = self.measured_topk_bytes("topk", scores.len(), k, false)?;
        self.run_reserved("topk", measured_bytes, |inner| inner.topk(scores, k))
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
        let measured_bytes = self.measured_knn_bytes(queries, candidates, dim, k, metric)?;
        self.run_reserved("knn", measured_bytes, |inner| {
            inner.knn(queries, candidates, query_count, dim, k, metric)
        })
    }

    fn paired_cosine(
        &self,
        left: &[f32],
        right: &[f32],
        pair_count: usize,
        dim: usize,
        out: &mut [f32],
    ) -> Result<()> {
        let measured_bytes =
            self.measured_f32_bytes("paired_cosine", &[left.len(), right.len(), out.len()])?;
        self.run_reserved("paired_cosine", measured_bytes, |inner| {
            inner.paired_cosine(left, right, pair_count, dim, out)
        })
    }

    fn device_info(&self) -> DeviceInfo {
        self.inner.device_info()
    }
}

fn checked_sum(operation: &'static str, values: &[usize]) -> Result<usize> {
    values.iter().copied().try_fold(0_usize, |sum, value| {
        sum.checked_add(value).ok_or_else(|| {
            budget_error(format!(
                "{operation} device-buffer element count overflow: accumulated={sum} next={value}"
            ))
        })
    })
}

fn budget_error(detail: String) -> ForgeError {
    ForgeError::VramBudget {
        detail,
        remediation: DISPATCH_REMEDIATION.to_owned(),
    }
}
