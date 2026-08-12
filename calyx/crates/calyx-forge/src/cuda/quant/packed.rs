mod binary;

use std::sync::Arc;

use cudarc::driver::CudaSlice;

use super::{CudaQuantContext, QuantCounters};
use crate::{ForgeError, Result};

pub use binary::{CudaBinaryBatch, CudaBinaryScores};

const MAX_PACKED_DIM: usize = 4_096;

fn validate_input(dim: usize, input: &[f32], op: &str) -> Result<usize> {
    if dim == 0 || dim > MAX_PACKED_DIM || input.is_empty() || !input.len().is_multiple_of(dim) {
        return Err(shape(format!(
            "{op} requires complete rows with dimension 1..={MAX_PACKED_DIM}"
        )));
    }
    if input.len() > i32::MAX as usize {
        return Err(shape(format!("{op} element count exceeds i32 indexing")));
    }
    if let Some(index) = input.iter().position(|value| !value.is_finite()) {
        return Err(ForgeError::NumericalInvariant {
            op: op.to_string(),
            detail: format!("non-finite input coefficient at index {index}"),
            remediation: "Reject NaN/Inf vectors before CUDA quantization".to_string(),
        });
    }
    Ok(input.len() / dim)
}

fn validate_status(status: &[i32], op: &str) -> Result<()> {
    if let Some((row, flags)) = status
        .iter()
        .copied()
        .enumerate()
        .find(|(_, flags)| *flags != 0)
    {
        return Err(ForgeError::NumericalInvariant {
            op: op.to_string(),
            detail: format!("device validation failed at row {row}: flags={flags}"),
            remediation: "Reject non-finite or overflowing packed quantization state".to_string(),
        });
    }
    Ok(())
}

fn record_encode(
    counters: Arc<QuantCounters>,
    h2d_bytes: usize,
    status_rows: usize,
    encoded_rows: usize,
) {
    counters.add_h2d(h2d_bytes);
    counters.add_d2h(status_rows.saturating_mul(size_of::<i32>()));
    counters.add_launches(1);
    counters.add_encoded_rows(encoded_rows);
}

fn alloc_f32(quant: &CudaQuantContext, len: usize, label: &str) -> Result<CudaSlice<f32>> {
    quant
        .context()
        .inner()
        .default_stream()
        .alloc_zeros(len)
        .map_err(|error| device(quant, format!("{label} allocation failed: {error}")))
}

fn alloc_u8(quant: &CudaQuantContext, len: usize, label: &str) -> Result<CudaSlice<u8>> {
    quant
        .context()
        .inner()
        .default_stream()
        .alloc_zeros(len)
        .map_err(|error| device(quant, format!("{label} allocation failed: {error}")))
}

fn alloc_i32(quant: &CudaQuantContext, len: usize, label: &str) -> Result<CudaSlice<i32>> {
    quant
        .context()
        .inner()
        .default_stream()
        .alloc_zeros(len)
        .map_err(|error| device(quant, format!("{label} allocation failed: {error}")))
}

fn checked_mul(left: usize, right: usize, label: &str) -> Result<usize> {
    left.checked_mul(right)
        .ok_or_else(|| shape(format!("packed CUDA {label} shape overflow")))
}

fn shape(detail: impl Into<String>) -> ForgeError {
    ForgeError::ShapeMismatch {
        expected: vec![1],
        got: vec![0],
        remediation: detail.into(),
    }
}

fn device(quant: &CudaQuantContext, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: format!("cuda:{}", quant.context().device_idx()),
        detail,
        remediation: "Use the embedded sm_120 packed quant kernels with sufficient VRAM"
            .to_string(),
    }
}
