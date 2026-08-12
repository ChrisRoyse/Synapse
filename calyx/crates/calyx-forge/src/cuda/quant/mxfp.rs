use std::sync::Arc;

use cudarc::driver::CudaSlice;

use super::{CudaQuantContext, CudaQuantScores, QuantCounters, mxfp_launch};
use crate::mxfp4::{MXFP4_BLOCK_SIZE, MXFP4_PACKED_BYTES};
use crate::mxfp8::MXFP8_BLOCK_BYTES;
use crate::quant::{AssayQuantSafety, MxFp4Codec, QuantLevel, QuantizedVec, Quantizer};
use crate::{ForgeError, Result};

const MXFP4_BLOCK_BYTES: usize = MXFP4_PACKED_BYTES + 1;
const MAX_MXFP_DIM: usize = 4_096;
const ZERO_SEED: [u8; 32] = [0; 32];

pub struct CudaMxFpBatch {
    quant: CudaQuantContext,
    rows: usize,
    dim: usize,
    level: QuantLevel,
    stride: usize,
    encoded: CudaSlice<u8>,
}

impl CudaMxFpBatch {
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn level(&self) -> QuantLevel {
        self.level
    }

    pub fn encoded_bytes_per_row(&self) -> usize {
        self.stride
    }

    pub fn read_encoded(&self) -> Result<Vec<QuantizedVec>> {
        let bytes = self
            .quant
            .context()
            .inner()
            .default_stream()
            .clone_dtoh(&self.encoded)
            .map_err(|error| device(&self.quant, format!("MXFP readback failed: {error}")))?;
        self.quant.counters().add_d2h(bytes.len());
        Ok(bytes
            .chunks_exact(self.stride)
            .map(|row| QuantizedVec {
                level: self.level,
                dim: self.dim,
                bytes: row.to_vec(),
                scale: 0.0,
                seed_id: ZERO_SEED,
            })
            .collect())
    }

    pub fn decode(&self) -> Result<Vec<f32>> {
        let output_len = checked_mul(self.rows, self.dim, "decoded output")?;
        let mut output = alloc_f32(&self.quant, output_len, "MXFP decoded output")?;
        mxfp_launch::decode(
            self.quant.context(),
            &self.encoded,
            self.dim,
            self.rows,
            level_code(self.level)?,
            &mut output,
        )?;
        self.quant.counters().add_launches(1);
        let decoded = self
            .quant
            .context()
            .inner()
            .default_stream()
            .clone_dtoh(&output)
            .map_err(|error| {
                device(&self.quant, format!("MXFP decode readback failed: {error}"))
            })?;
        self.quant
            .counters()
            .add_d2h(decoded.len() * size_of::<f32>());
        Ok(decoded)
    }

    pub fn score(&self, query: &Self) -> Result<CudaQuantScores> {
        validate_pair(query, self)?;
        let mut scores = alloc_f32(&self.quant, self.rows, "MXFP scores")?;
        mxfp_launch::score(
            self.quant.context(),
            &query.encoded,
            level_code(query.level)?,
            &self.encoded,
            level_code(self.level)?,
            self.dim,
            self.rows,
            &mut scores,
        )?;
        let counters = self.quant.counters();
        counters.add_launches(1);
        counters.add_scored_candidates(self.rows);
        Ok(CudaQuantScores::new(self.quant.clone(), scores, self.rows))
    }
}

impl std::fmt::Debug for CudaMxFpBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CudaMxFpBatch")
            .field("rows", &self.rows)
            .field("dim", &self.dim)
            .field("level", &self.level)
            .field("stride", &self.stride)
            .finish()
    }
}

impl CudaQuantContext {
    pub fn encode_mxfp4(
        &self,
        codec: &MxFp4Codec,
        slot_id: &str,
        safety: &AssayQuantSafety,
        input: &[f32],
    ) -> Result<CudaMxFpBatch> {
        if !safety.passes() {
            return Err(ForgeError::QuantIntelligenceLoss {
                slot: slot_id.to_string(),
                detail: "MXFP4 CUDA encode requires current passing Assay safety evidence"
                    .to_string(),
                remediation: "Run Assay and use MXFP4 only for a passing slot".to_string(),
            });
        }
        self.encode_mxfp(codec, input, QuantLevel::Bits4Fp)
    }

    pub fn encode_mxfp8(&self, codec: &MxFp4Codec, input: &[f32]) -> Result<CudaMxFpBatch> {
        self.encode_mxfp(codec, input, QuantLevel::Bits8Fp)
    }

    fn encode_mxfp(
        &self,
        codec: &MxFp4Codec,
        input: &[f32],
        level: QuantLevel,
    ) -> Result<CudaMxFpBatch> {
        let dim = codec.dim();
        let rows = validate_input(dim, input, "cuda_mxfp_encode")?;
        let stride = row_stride(dim, level)?;
        let encoded_len = checked_mul(rows, stride, "encoded rows")?;
        let stream = self.context().inner().default_stream();
        let input_device = stream
            .clone_htod(input)
            .map_err(|error| device(self, format!("MXFP input upload failed: {error}")))?;
        let mut encoded = alloc_u8(self, encoded_len, "MXFP encoded rows")?;
        let mut bad = alloc_i32(self, rows, "MXFP status")?;
        mxfp_launch::encode(
            self.context(),
            &input_device,
            dim,
            rows,
            level_code(level)?,
            &mut encoded,
            &mut bad,
        )?;
        let status = stream
            .clone_dtoh(&bad)
            .map_err(|error| device(self, format!("MXFP status readback failed: {error}")))?;
        validate_status(&status)?;
        record_encode(self.counters(), input.len(), rows);
        Ok(CudaMxFpBatch {
            quant: self.clone(),
            rows,
            dim,
            level,
            stride,
            encoded,
        })
    }
}

fn validate_input(dim: usize, input: &[f32], op: &str) -> Result<usize> {
    validate_dim(dim, op)?;
    if input.is_empty() || !input.len().is_multiple_of(dim) {
        return Err(shape(format!("{op} requires at least one complete row")));
    }
    if input.len() > i32::MAX as usize {
        return Err(shape(format!("{op} element count exceeds i32 indexing")));
    }
    if let Some(index) = input.iter().position(|value| !value.is_finite()) {
        return Err(ForgeError::NumericalInvariant {
            op: op.to_string(),
            detail: format!("non-finite input coefficient at index {index}"),
            remediation: "Reject NaN/Inf vectors before MXFP CUDA encoding".to_string(),
        });
    }
    Ok(input.len() / dim)
}

fn validate_dim(dim: usize, op: &str) -> Result<()> {
    if dim == 0 || dim > MAX_MXFP_DIM {
        return Err(shape(format!("{op} requires dimension 1..={MAX_MXFP_DIM}")));
    }
    Ok(())
}

fn validate_status(status: &[i32]) -> Result<()> {
    if let Some((row, flags)) = status.iter().enumerate().find(|(_, flags)| **flags != 0) {
        return Err(ForgeError::NumericalInvariant {
            op: "cuda_mxfp_encode".to_string(),
            detail: format!("device validation failed at row {row}: flags={flags}"),
            remediation: "Reject non-finite MXFP input before device encoding".to_string(),
        });
    }
    Ok(())
}

fn validate_pair(query: &CudaMxFpBatch, corpus: &CudaMxFpBatch) -> Result<()> {
    if query.rows != 1 || query.dim != corpus.dim {
        return Err(shape(
            "MXFP scoring requires one query with the corpus dimension",
        ));
    }
    if !Arc::ptr_eq(
        query.quant.context().inner(),
        corpus.quant.context().inner(),
    ) {
        return Err(shape("MXFP query and corpus must share one CUDA context"));
    }
    Ok(())
}

fn row_stride(dim: usize, level: QuantLevel) -> Result<usize> {
    let block_bytes = match level {
        QuantLevel::Bits4Fp => MXFP4_BLOCK_BYTES,
        QuantLevel::Bits8Fp => MXFP8_BLOCK_BYTES,
        _ => return Err(quant_error("MXFP level must be Bits4Fp or Bits8Fp")),
    };
    dim.div_ceil(MXFP4_BLOCK_SIZE)
        .checked_mul(block_bytes)
        .ok_or_else(|| shape("MXFP row stride overflow"))
}

fn level_code(level: QuantLevel) -> Result<i32> {
    match level {
        QuantLevel::Bits4Fp => Ok(4),
        QuantLevel::Bits8Fp => Ok(8),
        _ => Err(quant_error("MXFP level must be Bits4Fp or Bits8Fp")),
    }
}

fn record_encode(counters: Arc<QuantCounters>, elements: usize, rows: usize) {
    counters.add_h2d(elements.saturating_mul(size_of::<f32>()));
    counters.add_d2h(rows.saturating_mul(size_of::<i32>()));
    counters.add_launches(1);
    counters.add_encoded_rows(rows);
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
        .ok_or_else(|| shape(format!("MXFP CUDA {label} shape overflow")))
}

fn quant_error(detail: impl Into<String>) -> ForgeError {
    ForgeError::QuantError {
        op: "cuda_mxfp_upload".to_string(),
        level: "Bits4Fp/Bits8Fp".to_string(),
        detail: detail.into(),
        remediation: "Upload canonical finite MXFP blocks with zero scale metadata and seed"
            .to_string(),
    }
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
        remediation: "Use the embedded sm_120 MXFP quant kernels with sufficient VRAM".to_string(),
    }
}
