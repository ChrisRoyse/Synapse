use std::str;
use std::sync::Arc;

use cudarc::driver::{CudaModule, CudaSlice, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;

use crate::cuda::kernels::{ASSAY_CUBIN, ASSAY_PTX};
use crate::{CudaContext, ForgeError, Result};

mod ccm;
mod common;
mod dependence;
mod dependence_support;
mod hawkes;
mod ksg;
mod linalg;
mod linear_cka;
mod logistic;
mod mmd;
mod reductions;
mod temporal;
mod types;
mod validation_general;
mod validation_linalg;
mod validation_neighbors;
mod validation_splits_cka;
mod validation_temporal;

use self::common::*;
use self::dependence_support::*;
use self::reductions::*;
use self::validation_general::*;
use self::validation_linalg::*;
use self::validation_neighbors::*;
use self::validation_splits_cka::*;
use self::validation_temporal::*;

pub use self::ccm::ccm_simplex_predictions_host;
pub use self::dependence::{dcor_1d_host, hsic_1d_host};
pub use self::hawkes::hawkes_em_host;
pub use self::ksg::{entropy_radii_host, ksg_continuous_counts_host, mixed_ksg_counts_host};
pub use self::linalg::{correlation_precision_host, granger_lag_summaries_host};
pub use self::linear_cka::linear_cka_pair_estimates_host;
pub use self::logistic::{
    CudaLogisticConfig, CudaLogisticDataset, CudaLogisticSplits, logistic_summaries_host,
};
pub use self::mmd::{gaussian_mmd_device_bytes, gaussian_mmd_host, mmd_change_point_host};
pub use self::temporal::{
    autocorrelation_sums_host, cross_correlation_batch_host, periodogram_batch_host,
};
pub use self::types::*;
