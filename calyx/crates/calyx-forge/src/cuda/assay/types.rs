pub struct CudaDcorResult {
    pub dcor: f32,
    pub dcov2: f32,
    pub dvar_x: f32,
    pub dvar_y: f32,
    pub n_samples: usize,
    pub ge_count: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaHsicResult {
    pub hsic_biased: f32,
    pub hsic_unbiased: f32,
    pub tr_kc_lc: f64,
    pub off_diag_sum_k: f64,
    pub off_diag_sum_l: f64,
    pub sum_sq_centered_offdiag: f64,
    pub n_samples: usize,
    pub ge_count: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaMmdResult {
    pub mmd2: f64,
    pub null: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaMmdChangePointResult {
    pub split_index: usize,
    pub mmd2: f64,
    pub null: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaKsgContinuousCounts {
    pub radii: Vec<f32>,
    pub nx: Vec<usize>,
    pub ny: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaMixedKsgCounts {
    pub radii: Vec<f32>,
    pub same_class_counts: Vec<usize>,
    pub full_counts: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaCcmPredictions {
    pub library_predictions: Vec<Vec<f32>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaLogisticSummaries {
    pub bits: Vec<f32>,
    pub accuracy: Vec<f32>,
    pub iterations: Vec<usize>,
    pub final_relative_parameter_changes: Vec<f32>,
    pub converged: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaLinearCkaPairEstimates {
    pub raw_signed_point: Vec<f32>,
    pub redundancy_point: Vec<f32>,
    pub mc_standard_error: Vec<f32>,
    pub mc_gate_upper_estimate: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaCorrelationPrecision {
    pub corr: Vec<f64>,
    pub precision: Vec<f64>,
    pub n_samples: usize,
    pub n_variables: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaGrangerLagSummary {
    pub lag: usize,
    pub rss_restricted: f64,
    pub rss_unrestricted: f64,
    pub n_used: usize,
    pub df_den: usize,
    pub status: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaGrangerLagBatch {
    pub summaries: Vec<CudaGrangerLagSummary>,
    pub workspace_row_stride: usize,
    pub workspace_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaPeriodogramBatch {
    pub powers: Vec<f64>,
    pub permutation_max_powers: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaAutocorrelationSums {
    pub sums: Vec<f64>,
    pub counts: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaCrossCorrelationBatch {
    pub correlations: Vec<f32>,
    pub n_pairs: Vec<usize>,
}

/// Exact discrete transfer-entropy estimates for one fixed symbol table and a
/// caller-declared batch of equal-length index selections.
#[derive(Clone, Debug, PartialEq)]
pub struct CudaDiscreteTeBatch {
    pub estimates: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CudaHawkesFit {
    pub baseline_rates: Vec<f32>,
    pub branching_matrix: Vec<f32>,
    pub spectral_radius: f32,
}

pub const CUDA_GRANGER_STATUS_OK: i32 = 0;
pub const CUDA_GRANGER_STATUS_INVALID_LAG: i32 = 1;
pub const CUDA_GRANGER_STATUS_NONFINITE: i32 = 2;
pub const CUDA_GRANGER_STATUS_RANK_DEFICIENT: i32 = 3;
