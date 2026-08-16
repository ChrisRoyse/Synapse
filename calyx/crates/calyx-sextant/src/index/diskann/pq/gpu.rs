use calyx_core::Result;

#[cfg(not(sextant_cuda_pq))]
use super::invalid;
use super::{BuildOutput, DiskAnnPqBuildExecution, DiskAnnPqBuildParams};

#[cfg(sextant_cuda_pq)]
mod cuda;
#[cfg(sextant_cuda_pq)]
mod launch;

#[cfg(sextant_cuda_pq)]
pub(super) fn build(
    rows: &[(u32, Vec<f32>)],
    params: DiskAnnPqBuildParams,
    requested: DiskAnnPqBuildExecution,
) -> Result<BuildOutput> {
    cuda::build(rows, params, requested)
}

#[cfg(not(sextant_cuda_pq))]
pub(super) fn build(
    rows: &[(u32, Vec<f32>)],
    _params: DiskAnnPqBuildParams,
    requested: DiskAnnPqBuildExecution,
) -> Result<BuildOutput> {
    Err(invalid(format!(
        "strict CUDA PQ execution ({}) was required for {} rows; refusing silent CPU training: {}",
        requested.as_str(),
        rows.len(),
        crate::cuda_pq_unavailable_reason()
    )))
}
