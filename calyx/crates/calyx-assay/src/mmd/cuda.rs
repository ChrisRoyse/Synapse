use super::*;

#[cfg(feature = "cuda")]
pub(super) fn gaussian_mmd_cuda_strict_impl(
    flat: &[f64],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    bandwidth: f64,
    permutations: &[i32],
) -> Result<calyx_forge::CudaMmdResult> {
    let backend = calyx_forge::CudaBackend::new()
        .map_err(|err| crate::cuda_strict::forge_to_calyx("MMD", err))?;
    calyx_forge::gaussian_mmd_host(
        backend.context(),
        flat,
        n_a,
        n_b,
        dimension,
        bandwidth,
        permutations,
    )
    .map_err(|err| crate::cuda_strict::forge_to_calyx("MMD", err))
}

#[cfg(feature = "cuda")]
pub(super) fn gaussian_mmd_cuda_budgeted_impl(
    backend: &calyx_forge::VramBudgetedCudaBackend,
    flat: &[f64],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    bandwidth: f64,
    permutations: &[i32],
) -> Result<calyx_forge::CudaMmdResult> {
    backend
        .gaussian_mmd(flat, n_a, n_b, dimension, bandwidth, permutations)
        .map_err(|err| crate::cuda_strict::forge_to_calyx("MMD", err))
}

#[cfg(not(feature = "cuda"))]
pub(super) fn gaussian_mmd_cuda_strict_impl(
    _flat: &[f64],
    _n_a: usize,
    _n_b: usize,
    _dimension: usize,
    _bandwidth: f64,
    _permutations: &[i32],
) -> Result<MmdCudaUnavailable> {
    Err(cuda_unavailable("MMD"))
}

#[cfg(not(feature = "cuda"))]
pub(super) struct MmdCudaUnavailable {
    pub(super) mmd2: f64,
    pub(super) null: Vec<f64>,
}

#[cfg(feature = "cuda")]
pub(super) fn mmd_change_point_cuda_strict_impl(
    flat: &[f64],
    n: usize,
    dimension: usize,
    min_window: usize,
    bandwidth: f64,
    permutations: &[i32],
) -> Result<calyx_forge::CudaMmdChangePointResult> {
    let backend = calyx_forge::CudaBackend::new()
        .map_err(|err| crate::cuda_strict::forge_to_calyx("MMD change-point", err))?;
    calyx_forge::mmd_change_point_host(
        backend.context(),
        flat,
        n,
        dimension,
        min_window,
        bandwidth,
        permutations,
    )
    .map_err(|err| crate::cuda_strict::forge_to_calyx("MMD change-point", err))
}

#[cfg(not(feature = "cuda"))]
pub(super) fn mmd_change_point_cuda_strict_impl(
    _flat: &[f64],
    _n: usize,
    _dimension: usize,
    _min_window: usize,
    _bandwidth: f64,
    _permutations: &[i32],
) -> Result<MmdChangePointCudaUnavailable> {
    Err(cuda_unavailable("MMD change-point"))
}

#[cfg(not(feature = "cuda"))]
pub(super) struct MmdChangePointCudaUnavailable {
    pub(super) split_index: usize,
    pub(super) mmd2: f64,
    pub(super) null: Vec<f64>,
}
