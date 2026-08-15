//! Maximum mean discrepancy (MMD) drift tests (PRD `26 §7`, PH70).
//!
//! This module provides a deterministic Gaussian-kernel two-sample test plus a
//! simple change-point scan over an ordered feature stream. It is deliberately
//! small and fail-closed: invalid dimensions, non-finite values, too few rows,
//! or zero-signal inputs return cataloged `CALYX_ASSAY_*` errors.

use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use calyx_core::{CalyxError, Result};

#[cfg(not(feature = "cuda"))]
use crate::cuda_strict::cuda_unavailable;
use crate::cuda_strict::{deterministic_permutations, strict_cuda_requested};

mod cuda;
mod kernel;

#[cfg(feature = "cuda")]
use self::cuda::gaussian_mmd_cuda_budgeted_impl;
use self::cuda::{gaussian_mmd_cuda_strict_impl, mmd_change_point_cuda_strict_impl};
use self::kernel::{KernelMatrix, packed_matrix_len, quantile, squared_distance};

pub const MIN_MMD_SAMPLES: usize = 4;
pub const MAX_MMD_SAMPLES: usize = 2_048;
pub const DEFAULT_MMD_PERMUTATIONS: usize = 99;
pub const DEFAULT_MMD_ALPHA: f64 = 0.01;
pub const DEFAULT_MMD_SEED: u64 = 609;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MmdConfig {
    /// Gaussian sigma. When `None`, the median pairwise distance heuristic is
    /// used over the pooled sample.
    pub bandwidth: Option<f64>,
    pub permutations: usize,
    pub seed: u64,
    pub alpha: f64,
}

impl Default for MmdConfig {
    fn default() -> Self {
        Self {
            bandwidth: None,
            permutations: DEFAULT_MMD_PERMUTATIONS,
            seed: DEFAULT_MMD_SEED,
            alpha: DEFAULT_MMD_ALPHA,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MmdReport {
    pub n_a: usize,
    pub n_b: usize,
    pub dimension: usize,
    pub bandwidth: f64,
    pub mmd2: f64,
    pub null_mean: f64,
    pub critical_value: f64,
    pub p_value: f64,
    pub significant: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangePointReport {
    pub split_index: usize,
    pub left_n: usize,
    pub right_n: usize,
    pub report: MmdReport,
}

pub fn gaussian_mmd(x: &[Vec<f64>], y: &[Vec<f64>]) -> Result<MmdReport> {
    gaussian_mmd_with_config(x, y, &MmdConfig::default())
}

pub fn gaussian_mmd_with_config(
    x: &[Vec<f64>],
    y: &[Vec<f64>],
    config: &MmdConfig,
) -> Result<MmdReport> {
    if strict_cuda_requested() {
        return gaussian_mmd_with_config_cuda_strict(x, y, config);
    }
    let shape = validate_pair(x, y, config)?;
    let pooled = pooled_samples(x, y);
    gaussian_mmd_borrowed_with_config(&pooled, x.len(), y.len(), shape.dimension, config)
}

/// Runs the same Gaussian MMD estimator over one contiguous row-major arena.
///
/// This is the ownership-preserving entry point for callers that already hold
/// bounded flat samples. It avoids rebuilding thousands of separately
/// allocated rows solely to satisfy the estimator API; row slices borrow the
/// arena for the duration of the calculation.
pub fn gaussian_mmd_flat_with_config(
    pooled: &[f64],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    config: &MmdConfig,
) -> Result<MmdReport> {
    validate_flat_pair(pooled, n_a, n_b, dimension, config)?;
    let samples = pooled.chunks_exact(dimension).collect::<Vec<_>>();
    if strict_cuda_requested() {
        let (bandwidth, workspace) = resolve_bandwidth(&samples, config.bandwidth)?;
        drop(workspace);
        let permutations =
            deterministic_permutations(samples.len(), config.permutations, config.seed)?;
        let result =
            gaussian_mmd_cuda_strict_impl(pooled, n_a, n_b, dimension, bandwidth, &permutations)?;
        return Ok(report_from_null(
            n_a,
            n_b,
            dimension,
            bandwidth,
            result.mmd2,
            result.null,
            config.alpha,
        ));
    }
    gaussian_mmd_borrowed_with_config(&samples, n_a, n_b, dimension, config)
}

/// Runs flat Gaussian MMD on CPU regardless of the process-wide Assay backend.
///
/// This is an explicit execution class for background maintenance, not a
/// failure fallback. Invalid inputs and estimator failures remain hard errors.
pub fn gaussian_mmd_flat_with_config_cpu_strict(
    pooled: &[f64],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    config: &MmdConfig,
) -> Result<MmdReport> {
    validate_flat_pair(pooled, n_a, n_b, dimension, config)?;
    let samples = pooled.chunks_exact(dimension).collect::<Vec<_>>();
    gaussian_mmd_borrowed_with_config(&samples, n_a, n_b, dimension, config)
}

/// Runs flat Gaussian MMD through an already-leased, budgeted CUDA backend.
///
/// Synapse uses this entry point so Assay shares the application-owned CUDA
/// context, module cache, process-local VRAM admission, and host reservation
/// ledger instead of creating an untracked context per estimator call.
#[cfg(feature = "cuda")]
pub fn gaussian_mmd_flat_with_config_cuda_budgeted(
    backend: &calyx_forge::VramBudgetedCudaBackend,
    pooled: &[f64],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    config: &MmdConfig,
) -> Result<MmdReport> {
    validate_flat_pair(pooled, n_a, n_b, dimension, config)?;
    let samples = pooled.chunks_exact(dimension).collect::<Vec<_>>();
    let (bandwidth, workspace) = resolve_bandwidth(&samples, config.bandwidth)?;
    drop(workspace);
    let permutations = deterministic_permutations(samples.len(), config.permutations, config.seed)?;
    let result = gaussian_mmd_cuda_budgeted_impl(
        backend,
        pooled,
        n_a,
        n_b,
        dimension,
        bandwidth,
        &permutations,
    )?;
    Ok(report_from_null(
        n_a,
        n_b,
        dimension,
        bandwidth,
        result.mmd2,
        result.null,
        config.alpha,
    ))
}

fn gaussian_mmd_borrowed_with_config(
    pooled: &[&[f64]],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    config: &MmdConfig,
) -> Result<MmdReport> {
    let (bandwidth, workspace) = resolve_bandwidth(pooled, config.bandwidth)?;
    let kernel = KernelMatrix::new(pooled, bandwidth, workspace)?;
    let left = (0..n_a).collect::<Vec<_>>();
    let right = (n_a..pooled.len()).collect::<Vec<_>>();
    let observed = kernel.mmd2(&left, &right);
    let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
    let mut indices: Vec<usize> = (0..pooled.len()).collect();
    let mut null = Vec::with_capacity(config.permutations);
    for _ in 0..config.permutations {
        indices.shuffle(&mut rng);
        null.push(kernel.mmd2(&indices[..n_a], &indices[n_a..]));
    }
    Ok(report_from_null(
        n_a,
        n_b,
        dimension,
        bandwidth,
        observed,
        null,
        config.alpha,
    ))
}

pub fn mmd_change_point(
    samples: &[Vec<f64>],
    min_window: usize,
    config: &MmdConfig,
) -> Result<ChangePointReport> {
    if strict_cuda_requested() {
        return mmd_change_point_cuda_strict(samples, min_window, config);
    }
    validate_single(samples, config)?;
    if min_window < MIN_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD change-point min_window must be >= {MIN_MMD_SAMPLES}, got {min_window}"
        )));
    }
    if samples.len() < min_window * 2 {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD change-point requires at least {} samples, got {}",
            min_window * 2,
            samples.len()
        )));
    }
    let borrowed = samples.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let (bandwidth, workspace) = resolve_bandwidth(&borrowed, config.bandwidth)?;
    let kernel = KernelMatrix::new(&borrowed, bandwidth, workspace)?;
    let (best_split, best_mmd) = best_contiguous_split(&kernel, samples.len(), min_window);
    let null = change_point_max_null(&kernel, samples.len(), min_window, config);
    let report = report_from_null(
        best_split,
        samples.len() - best_split,
        samples[0].len(),
        bandwidth,
        best_mmd,
        null,
        config.alpha,
    );
    Ok(ChangePointReport {
        split_index: best_split,
        left_n: best_split,
        right_n: samples.len() - best_split,
        report,
    })
}

pub fn gaussian_mmd_cuda_strict(x: &[Vec<f64>], y: &[Vec<f64>]) -> Result<MmdReport> {
    gaussian_mmd_with_config_cuda_strict(x, y, &MmdConfig::default())
}

pub fn gaussian_mmd_with_config_cuda_strict(
    x: &[Vec<f64>],
    y: &[Vec<f64>],
    config: &MmdConfig,
) -> Result<MmdReport> {
    let shape = validate_pair(x, y, config)?;
    let pooled = pooled_samples(x, y);
    let (bandwidth, workspace) = resolve_bandwidth(&pooled, config.bandwidth)?;
    drop(workspace);
    let flat = flatten_samples(&pooled, shape.dimension)?;
    let permutations = deterministic_permutations(pooled.len(), config.permutations, config.seed)?;
    let result = gaussian_mmd_cuda_strict_impl(
        &flat,
        x.len(),
        y.len(),
        shape.dimension,
        bandwidth,
        &permutations,
    )?;
    Ok(report_from_null(
        x.len(),
        y.len(),
        shape.dimension,
        bandwidth,
        result.mmd2,
        result.null,
        config.alpha,
    ))
}

pub fn mmd_change_point_cuda_strict(
    samples: &[Vec<f64>],
    min_window: usize,
    config: &MmdConfig,
) -> Result<ChangePointReport> {
    validate_single(samples, config)?;
    if min_window < MIN_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD change-point min_window must be >= {MIN_MMD_SAMPLES}, got {min_window}"
        )));
    }
    if samples.len() < min_window * 2 {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD change-point requires at least {} samples, got {}",
            min_window * 2,
            samples.len()
        )));
    }
    let borrowed = samples.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let (bandwidth, workspace) = resolve_bandwidth(&borrowed, config.bandwidth)?;
    drop(workspace);
    let dimension = samples[0].len();
    let flat = flatten_samples(&borrowed, dimension)?;
    let permutations = deterministic_permutations(samples.len(), config.permutations, config.seed)?;
    let result = mmd_change_point_cuda_strict_impl(
        &flat,
        samples.len(),
        dimension,
        min_window,
        bandwidth,
        &permutations,
    )?;
    let report = report_from_null(
        result.split_index,
        samples.len() - result.split_index,
        dimension,
        bandwidth,
        result.mmd2,
        result.null,
        config.alpha,
    );
    Ok(ChangePointReport {
        split_index: result.split_index,
        left_n: result.split_index,
        right_n: samples.len() - result.split_index,
        report,
    })
}

struct Shape {
    dimension: usize,
}

fn validate_pair(x: &[Vec<f64>], y: &[Vec<f64>], config: &MmdConfig) -> Result<Shape> {
    validate_config(config)?;
    if x.len() < MIN_MMD_SAMPLES || y.len() < MIN_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD requires >= {MIN_MMD_SAMPLES} samples per side, got {} and {}",
            x.len(),
            y.len()
        )));
    }
    if x.len() + y.len() > MAX_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD input has {} pooled samples (max {MAX_MMD_SAMPLES})",
            x.len() + y.len()
        )));
    }
    let dimension = x
        .first()
        .ok_or_else(|| CalyxError::assay_insufficient_samples("MMD side A is empty"))?
        .len();
    if dimension == 0 {
        return Err(CalyxError::assay_insufficient_samples(
            "MMD vectors must have at least one dimension",
        ));
    }
    validate_rows(x, dimension, "A")?;
    validate_rows(y, dimension, "B")?;
    Ok(Shape { dimension })
}

fn validate_flat_pair(
    pooled: &[f64],
    n_a: usize,
    n_b: usize,
    dimension: usize,
    config: &MmdConfig,
) -> Result<()> {
    validate_config(config)?;
    if n_a < MIN_MMD_SAMPLES || n_b < MIN_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD requires >= {MIN_MMD_SAMPLES} samples per side, got {n_a} and {n_b}"
        )));
    }
    let rows = n_a
        .checked_add(n_b)
        .ok_or_else(|| CalyxError::forge_vram_budget("MMD flat input row count overflow"))?;
    if rows > MAX_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD input has {rows} pooled samples (max {MAX_MMD_SAMPLES})"
        )));
    }
    if dimension == 0 {
        return Err(CalyxError::assay_insufficient_samples(
            "MMD vectors must have at least one dimension",
        ));
    }
    let expected_len = rows
        .checked_mul(dimension)
        .ok_or_else(|| CalyxError::forge_vram_budget("MMD flat input element count overflow"))?;
    if pooled.len() != expected_len {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD flat input has {} elements, expected {rows} rows x {dimension} dimensions = {expected_len}",
            pooled.len()
        )));
    }
    for (index, value) in pooled.iter().enumerate() {
        if !value.is_finite() {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "MMD flat input row {} col {} is NaN or infinity",
                index / dimension,
                index % dimension
            )));
        }
    }
    Ok(())
}

fn validate_single(samples: &[Vec<f64>], config: &MmdConfig) -> Result<()> {
    validate_config(config)?;
    if samples.len() < MIN_MMD_SAMPLES * 2 {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD change-point requires >= {} samples, got {}",
            MIN_MMD_SAMPLES * 2,
            samples.len()
        )));
    }
    if samples.len() > MAX_MMD_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD change-point input has {} samples (max {MAX_MMD_SAMPLES})",
            samples.len()
        )));
    }
    let dimension = samples[0].len();
    if dimension == 0 {
        return Err(CalyxError::assay_insufficient_samples(
            "MMD vectors must have at least one dimension",
        ));
    }
    validate_rows(samples, dimension, "stream")
}

fn validate_config(config: &MmdConfig) -> Result<()> {
    if config.permutations == 0 {
        return Err(CalyxError::assay_insufficient_samples(
            "MMD permutations must be >= 1",
        ));
    }
    if !config.alpha.is_finite() || config.alpha <= 0.0 || config.alpha >= 1.0 {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD alpha must be in (0, 1), got {}",
            config.alpha
        )));
    }
    if let Some(bandwidth) = config.bandwidth
        && (!bandwidth.is_finite() || bandwidth <= 0.0)
    {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "MMD bandwidth must be finite and positive, got {bandwidth}"
        )));
    }
    Ok(())
}

fn validate_rows(rows: &[Vec<f64>], dimension: usize, side: &str) -> Result<()> {
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != dimension {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "MMD side {side} row {row_index} has dimension {}, expected {dimension}",
                row.len()
            )));
        }
        for (col_index, value) in row.iter().enumerate() {
            if !value.is_finite() {
                return Err(CalyxError::assay_insufficient_samples(format!(
                    "MMD side {side} row {row_index} col {col_index} is NaN or infinity"
                )));
            }
        }
    }
    Ok(())
}

fn pooled_samples<'a>(x: &'a [Vec<f64>], y: &'a [Vec<f64>]) -> Vec<&'a [f64]> {
    x.iter().chain(y.iter()).map(Vec::as_slice).collect()
}

fn flatten_samples(samples: &[&[f64]], dimension: usize) -> Result<Vec<f64>> {
    let len = samples
        .len()
        .checked_mul(dimension)
        .ok_or_else(|| CalyxError::forge_vram_budget("MMD CUDA flattened input length overflow"))?;
    let mut flat = Vec::with_capacity(len);
    for row in samples {
        if row.len() != dimension {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "MMD row has dimension {}, expected {dimension}",
                row.len()
            )));
        }
        flat.extend_from_slice(row);
    }
    Ok(flat)
}

fn resolve_bandwidth(samples: &[&[f64]], configured: Option<f64>) -> Result<(f64, Vec<f64>)> {
    let packed_len = packed_matrix_len(samples.len())?;
    let mut distances = Vec::new();
    distances.try_reserve_exact(packed_len).map_err(|error| {
        CalyxError::forge_vram_budget(format!(
            "MMD shared distance/kernel workspace reserve failed for {} samples ({packed_len} f64 values): {error}",
            samples.len()
        ))
    })?;
    if let Some(bandwidth) = configured {
        return Ok((bandwidth, distances));
    }
    for i in 0..samples.len() {
        for j in (i + 1)..samples.len() {
            let distance = squared_distance(samples[i], samples[j]).sqrt();
            if distance > 0.0 {
                distances.push(distance);
            }
        }
    }
    if distances.is_empty() {
        return Err(CalyxError::assay_low_signal(
            "MMD pooled samples have zero pairwise distance; no distribution shift is measurable",
        ));
    }
    distances.sort_by(|a, b| a.total_cmp(b));
    let bandwidth = quantile(&distances, 0.5);
    Ok((bandwidth, distances))
}

fn report_from_null(
    n_a: usize,
    n_b: usize,
    dimension: usize,
    bandwidth: f64,
    observed: f64,
    mut null: Vec<f64>,
    alpha: f64,
) -> MmdReport {
    null.sort_by(|a, b| a.total_cmp(b));
    let ge_count = null.iter().filter(|&&sample| sample >= observed).count();
    let p_value = (ge_count + 1) as f64 / (null.len() + 1) as f64;
    let critical_value = quantile(&null, 1.0 - alpha);
    let null_mean = null.iter().sum::<f64>() / null.len() as f64;
    MmdReport {
        n_a,
        n_b,
        dimension,
        bandwidth,
        mmd2: observed,
        null_mean,
        critical_value,
        p_value,
        significant: p_value <= alpha && observed > critical_value,
    }
}

fn best_contiguous_split(kernel: &KernelMatrix, n: usize, min_window: usize) -> (usize, f64) {
    let mut best_split = min_window;
    let mut best_mmd = f64::NEG_INFINITY;
    for split in min_window..=(n - min_window) {
        let left = (0..split).collect::<Vec<_>>();
        let right = (split..n).collect::<Vec<_>>();
        let value = kernel.mmd2_unbiased(&left, &right);
        if value > best_mmd {
            best_mmd = value;
            best_split = split;
        }
    }
    (best_split, best_mmd)
}

fn change_point_max_null(
    kernel: &KernelMatrix,
    n: usize,
    min_window: usize,
    config: &MmdConfig,
) -> Vec<f64> {
    let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
    let mut indices = (0..n).collect::<Vec<_>>();
    let mut null = Vec::with_capacity(config.permutations);
    for _ in 0..config.permutations {
        indices.shuffle(&mut rng);
        let mut max_stat = f64::NEG_INFINITY;
        for split in min_window..=(n - min_window) {
            max_stat = max_stat.max(kernel.mmd2_unbiased(&indices[..split], &indices[split..]));
        }
        null.push(max_stat);
    }
    null
}
