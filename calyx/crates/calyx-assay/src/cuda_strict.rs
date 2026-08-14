use calyx_core::{CalyxError, Result};
use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;
use std::sync::OnceLock;

pub const STRICT_CUDA_ENV: &str = "CALYX_ASSAY_CUDA_STRICT";
pub const CALYX_ASSAY_COMPUTE_BACKEND_CONFLICT: &str = "CALYX_ASSAY_COMPUTE_BACKEND_CONFLICT";
const COMPUTE_BACKEND_REMEDIATION: &str = "configure one Calyx Assay compute backend before serving estimator requests and restart the process to change it";

/// Process-wide execution backend for generic Calyx Assay estimators.
///
/// Synapse selects one serving math backend during vault open. Assay's generic
/// APIs must obey that same decision instead of independently consulting a
/// process environment variable and silently choosing CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssayComputeBackend {
    Cpu,
    Cuda,
}

impl AssayComputeBackend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

static COMPUTE_BACKEND: OnceLock<AssayComputeBackend> = OnceLock::new();

/// Configures the generic Assay execution backend exactly once per process.
///
/// Repeating the same selection is idempotent. A conflicting selection fails
/// closed because one process cannot honestly advertise two serving backends.
///
/// # Errors
///
/// Returns [`CALYX_ASSAY_COMPUTE_BACKEND_CONFLICT`] when another caller has
/// already selected a different backend for this process.
pub fn configure_compute_backend(backend: AssayComputeBackend) -> Result<()> {
    match COMPUTE_BACKEND.set(backend) {
        Ok(()) => Ok(()),
        Err(requested) => {
            let Some(configured) = COMPUTE_BACKEND.get().copied() else {
                return Err(CalyxError {
                    code: CALYX_ASSAY_COMPUTE_BACKEND_CONFLICT,
                    message: "Calyx Assay backend initialization reported a conflict without retaining the configured value".to_owned(),
                    remediation: COMPUTE_BACKEND_REMEDIATION,
                });
            };
            if configured == requested {
                Ok(())
            } else {
                Err(CalyxError {
                    code: CALYX_ASSAY_COMPUTE_BACKEND_CONFLICT,
                    message: format!(
                        "Calyx Assay process backend is already configured as {} and cannot be changed to {} while the process is running",
                        configured.as_str(),
                        requested.as_str()
                    ),
                    remediation: COMPUTE_BACKEND_REMEDIATION,
                })
            }
        }
    }
}

/// Returns the immutable process-wide Assay backend, when explicitly set.
#[must_use]
pub fn configured_compute_backend() -> Option<AssayComputeBackend> {
    COMPUTE_BACKEND.get().copied()
}

/// Returns whether generic Assay entry points must use CUDA.
///
/// Embedders should call [`configure_compute_backend`] before serving work.
/// The environment read remains only for standalone Calyx compatibility.
pub fn strict_cuda_requested() -> bool {
    configured_compute_backend().map_or_else(
        || {
            std::env::var(STRICT_CUDA_ENV)
                .map(|value| {
                    matches!(
                        value.to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                })
                .unwrap_or(false)
        },
        |backend| backend == AssayComputeBackend::Cuda,
    )
}

#[cfg(not(feature = "cuda"))]
pub fn cuda_unavailable(op: &str) -> CalyxError {
    CalyxError::forge_device_unavailable(format!(
        "{op} requires calyx-assay feature `cuda` and a working Forge CUDA runtime when {STRICT_CUDA_ENV}=1; strict mode does not fall back to CPU"
    ))
}

pub fn deterministic_permutations(n: usize, permutations: usize, seed: u64) -> Result<Vec<i32>> {
    if n > i32::MAX as usize {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "assay CUDA permutation rows exceed i32 kernel index range: n={n}"
        )));
    }
    let capacity = n
        .checked_mul(permutations)
        .ok_or_else(|| CalyxError::forge_vram_budget("assay CUDA permutation buffer overflow"))?;
    let mut out = Vec::with_capacity(capacity);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut perm: Vec<i32> = (0..n as i32).collect();
    for _ in 0..permutations {
        perm.shuffle(&mut rng);
        out.extend_from_slice(&perm);
    }
    Ok(out)
}

#[cfg(feature = "cuda")]
pub fn forge_to_calyx(op: &str, err: calyx_forge::ForgeError) -> CalyxError {
    let message = format!("{op} CUDA strict failure: {err}");
    match err {
        calyx_forge::ForgeError::NumericalInvariant { .. } => {
            CalyxError::forge_numerical_invariant(message)
        }
        calyx_forge::ForgeError::DeviceUnavailable { .. } => {
            CalyxError::forge_device_unavailable(message)
        }
        calyx_forge::ForgeError::VramBudget { .. }
        | calyx_forge::ForgeError::LensVramBudget { .. } => CalyxError::forge_vram_budget(message),
        calyx_forge::ForgeError::ShapeMismatch { .. } => {
            CalyxError::assay_insufficient_samples(message)
        }
        _ => CalyxError::forge_device_unavailable(message),
    }
}

#[cfg(feature = "cuda")]
pub fn forge_linear_algebra_to_calyx(op: &str, err: calyx_forge::ForgeError) -> CalyxError {
    let message = format!("{op} CUDA strict linear algebra failure: {err}");
    match err {
        calyx_forge::ForgeError::NumericalInvariant { .. } => {
            CalyxError::assay_degenerate_input(message)
        }
        calyx_forge::ForgeError::ShapeMismatch { .. } => {
            CalyxError::assay_insufficient_samples(message)
        }
        other => forge_to_calyx(op, other),
    }
}
