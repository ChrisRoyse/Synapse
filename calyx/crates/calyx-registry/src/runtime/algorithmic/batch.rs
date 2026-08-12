use std::sync::Mutex;

#[cfg(feature = "cuda")]
use calyx_core::CalyxError;
use calyx_core::{Input, Result, SlotVector};
use serde::{Deserialize, Serialize};

#[cfg(feature = "cuda")]
use super::cpu::{byte_features_from_raw, sparse_keywords_from_hashes, token_vectors_from_words};
use super::cpu::{tokenize_sparse, tokenize_token_hash};
use super::{AlgorithmicEncoder, AlgorithmicLens};
use crate::lens::ensure_input_modality;

/// Calibrated production-GPU crossover guards for bulk encoder dispatch.
pub const BYTE_FEATURES_CUDA_MIN_INPUT_BYTES: usize = 16 * 1024;
pub const SPARSE_KEYWORDS_CUDA_MIN_TOKENS: usize = 512;
pub const TOKEN_HASH_CUDA_MIN_WORDS: usize = 2 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlgorithmicBatchProvider {
    #[default]
    Cpu,
    Cuda,
}

impl AlgorithmicBatchProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

/// Serializable evidence for the provider selected by the latest batch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlgorithmicBatchStats {
    pub provider: AlgorithmicBatchProvider,
    pub encoder: String,
    pub rows: u64,
    pub input_bytes: u64,
    pub work_items: u64,
    pub host_to_device_bytes: u64,
    pub device_to_host_bytes: u64,
    pub kernel_launches: u64,
    pub reason: String,
}

#[derive(Debug, Default)]
pub(super) struct BatchState {
    last_stats: Mutex<Option<AlgorithmicBatchStats>>,
    #[cfg(feature = "cuda")]
    cuda: Mutex<Option<calyx_forge::CudaAlgorithmicContext>>,
}

impl BatchState {
    pub(super) fn record(&self, stats: AlgorithmicBatchStats) {
        if stats.reason != "single-input CPU path" {
            eprintln!(
                "CALYX_ALGORITHMIC_BATCH provider={} encoder={} rows={} input_bytes={} work_items={} host_to_device_bytes={} device_to_host_bytes={} kernel_launches={} reason={:?}",
                stats.provider.as_str(),
                stats.encoder,
                stats.rows,
                stats.input_bytes,
                stats.work_items,
                stats.host_to_device_bytes,
                stats.device_to_host_bytes,
                stats.kernel_launches,
                stats.reason,
            );
        }
        *self
            .last_stats
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(stats);
    }

    pub(super) fn last_stats(&self) -> Option<AlgorithmicBatchStats> {
        self.last_stats
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    #[cfg(feature = "cuda")]
    fn cuda(&self) -> Result<calyx_forge::CudaAlgorithmicContext> {
        let mut context = self.cuda.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(context) = context.as_ref() {
            return Ok(context.clone());
        }
        let initialized = calyx_forge::CudaAlgorithmicContext::new(0).map_err(forge_error)?;
        *context = Some(initialized.clone());
        Ok(initialized)
    }
}

pub(super) fn measure_batch(lens: &AlgorithmicLens, inputs: &[Input]) -> Result<Vec<SlotVector>> {
    for input in inputs {
        ensure_input_modality(lens, input)?;
    }
    match lens.encoder {
        AlgorithmicEncoder::ByteFeatures => byte_batch(lens, inputs),
        AlgorithmicEncoder::SparseKeywords { dim } => sparse_batch(lens, inputs, dim),
        AlgorithmicEncoder::TokenHash { token_dim } => token_batch(lens, inputs, token_dim),
        // `SparseKeywordsTf` deliberately falls through to the CPU encoder
        // rather than reusing `sparse_batch`: that path calls
        // `sparse_keywords_from_hashes`, which L1-normalizes, and routing the
        // raw-count lane through it would reintroduce #1902 on exactly the
        // corpora large enough to cross the CUDA threshold. Giving it its own
        // GPU path is possible but cannot be verified on a host without an
        // NVIDIA device, so it stays CPU-native until it can be.
        _ => cpu_batch(lens, inputs, inputs.len(), "encoder is CPU-native"),
    }
}

pub(super) fn cpu_stats(
    encoder: AlgorithmicEncoder,
    inputs: &[Input],
    work_items: usize,
    reason: &str,
) -> AlgorithmicBatchStats {
    AlgorithmicBatchStats {
        provider: AlgorithmicBatchProvider::Cpu,
        encoder: format!("{encoder:?}"),
        rows: inputs.len() as u64,
        input_bytes: input_bytes(inputs) as u64,
        work_items: work_items as u64,
        reason: reason.to_string(),
        ..AlgorithmicBatchStats::default()
    }
}

fn cpu_batch(
    lens: &AlgorithmicLens,
    inputs: &[Input],
    work_items: usize,
    reason: &str,
) -> Result<Vec<SlotVector>> {
    let output = inputs
        .iter()
        .enumerate()
        .map(|(item_index, input)| {
            lens.measure_cpu(input).map_err(|mut error| {
                error.message = format!(
                    "algorithmic batch item_index={item_index}: {}",
                    error.message
                );
                error
            })
        })
        .collect::<Result<Vec<_>>>()?;
    lens.batch
        .record(cpu_stats(lens.encoder, inputs, work_items, reason));
    Ok(output)
}

fn byte_batch(lens: &AlgorithmicLens, inputs: &[Input]) -> Result<Vec<SlotVector>> {
    let bytes = input_bytes(inputs);
    if bytes < BYTE_FEATURES_CUDA_MIN_INPUT_BYTES {
        return cpu_batch(lens, inputs, bytes, "below byte-feature CUDA crossover");
    }
    #[cfg(not(feature = "cuda"))]
    return cpu_batch(lens, inputs, bytes, "registry CUDA feature is not compiled");
    #[cfg(feature = "cuda")]
    {
        let context = lens.batch.cuda()?;
        let rows = inputs
            .iter()
            .map(|input| input.bytes.as_slice())
            .collect::<Vec<_>>();
        let ragged = calyx_forge::CudaByteRaggedBatch::from_slices(&rows).map_err(forge_error)?;
        let (raw, forge_stats) = context.byte_features_raw(&ragged).map_err(forge_error)?;
        let output = raw
            .into_iter()
            .map(|row| SlotVector::Dense {
                dim: lens.encoder.dim(),
                data: byte_features_from_raw(row.values),
            })
            .collect();
        lens.batch.record(cuda_stats(
            lens.encoder,
            inputs,
            forge_stats,
            "byte batch met measured CUDA crossover",
        ));
        Ok(output)
    }
}

fn sparse_batch(lens: &AlgorithmicLens, inputs: &[Input], dim: u32) -> Result<Vec<SlotVector>> {
    let token_rows = inputs
        .iter()
        .map(|input| tokenize_sparse(&input.bytes))
        .collect::<Vec<_>>();
    let tokens = token_rows.iter().map(Vec::len).sum::<usize>();
    if tokens < SPARSE_KEYWORDS_CUDA_MIN_TOKENS {
        return cpu_batch(lens, inputs, tokens, "below sparse-keyword CUDA crossover");
    }
    let longest = token_rows
        .iter()
        .flatten()
        .map(Vec::len)
        .max()
        .unwrap_or_default();
    #[cfg(feature = "cuda")]
    if longest > calyx_forge::ALGORITHMIC_SPARSE_MAX_TOKEN_BYTES {
        return cpu_batch(
            lens,
            inputs,
            tokens,
            "keyword exceeds single-chunk BLAKE3 CUDA contract",
        );
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = dim;
        let _ = longest;
        cpu_batch(
            lens,
            inputs,
            tokens,
            "registry CUDA feature is not compiled",
        )
    }
    #[cfg(feature = "cuda")]
    {
        let flat = token_rows
            .iter()
            .flatten()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        let ragged = calyx_forge::CudaByteRaggedBatch::from_slices(&flat).map_err(forge_error)?;
        let context = lens.batch.cuda()?;
        let (hashes, forge_stats) = context
            .sparse_keyword_hashes(&ragged)
            .map_err(forge_error)?;
        let mut offset = 0;
        let output = token_rows
            .iter()
            .map(|row| {
                let end = offset + row.len();
                let output = sparse_keywords_from_hashes(&hashes[offset..end], dim);
                offset = end;
                output
            })
            .collect::<Result<Vec<_>>>()?;
        lens.batch.record(cuda_stats(
            lens.encoder,
            inputs,
            forge_stats,
            "sparse batch met measured CUDA crossover",
        ));
        Ok(output)
    }
}

fn token_batch(
    lens: &AlgorithmicLens,
    inputs: &[Input],
    token_dim: u32,
) -> Result<Vec<SlotVector>> {
    let token_dim = token_dim.max(1);
    let token_rows = inputs
        .iter()
        .map(|input| {
            let mut tokens = tokenize_token_hash(&input.bytes);
            if tokens.is_empty() {
                tokens.push(input.bytes.clone());
            }
            tokens
        })
        .collect::<Vec<_>>();
    let tokens = token_rows.iter().map(Vec::len).sum::<usize>();
    let words = tokens.saturating_mul(token_dim as usize);
    if words < TOKEN_HASH_CUDA_MIN_WORDS {
        return cpu_batch(lens, inputs, words, "below token-hash CUDA crossover");
    }
    let longest = token_rows
        .iter()
        .flatten()
        .map(Vec::len)
        .max()
        .unwrap_or_default();
    #[cfg(feature = "cuda")]
    if longest > calyx_forge::ALGORITHMIC_TOKEN_HASH_MAX_TOKEN_BYTES {
        return cpu_batch(
            lens,
            inputs,
            words,
            "token exceeds single-chunk BLAKE3 CUDA contract",
        );
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = longest;
        cpu_batch(lens, inputs, words, "registry CUDA feature is not compiled")
    }
    #[cfg(feature = "cuda")]
    {
        let flat = token_rows
            .iter()
            .flatten()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        let ragged = calyx_forge::CudaByteRaggedBatch::from_slices(&flat).map_err(forge_error)?;
        let context = lens.batch.cuda()?;
        let (raw, forge_stats) = context
            .token_hash_words(&ragged, token_dim)
            .map_err(forge_error)?;
        let mut vectors = token_vectors_from_words(&raw, token_dim).into_iter();
        let output = token_rows
            .iter()
            .map(|row| SlotVector::Multi {
                token_dim,
                tokens: vectors.by_ref().take(row.len()).collect(),
            })
            .collect();
        lens.batch.record(cuda_stats(
            lens.encoder,
            inputs,
            forge_stats,
            "token batch met measured CUDA crossover",
        ));
        Ok(output)
    }
}

fn input_bytes(inputs: &[Input]) -> usize {
    inputs.iter().fold(0_usize, |total, input| {
        total.saturating_add(input.bytes.len())
    })
}

#[cfg(feature = "cuda")]
fn cuda_stats(
    encoder: AlgorithmicEncoder,
    inputs: &[Input],
    stats: calyx_forge::CudaAlgorithmicStats,
    reason: &str,
) -> AlgorithmicBatchStats {
    AlgorithmicBatchStats {
        provider: AlgorithmicBatchProvider::Cuda,
        encoder: format!("{encoder:?}"),
        rows: inputs.len() as u64,
        input_bytes: input_bytes(inputs) as u64,
        work_items: stats.work_items,
        host_to_device_bytes: stats.host_to_device_bytes,
        device_to_host_bytes: stats.device_to_host_bytes,
        kernel_launches: stats.kernel_launches,
        reason: reason.to_string(),
    }
}

#[cfg(feature = "cuda")]
fn forge_error(error: calyx_forge::ForgeError) -> CalyxError {
    CalyxError {
        code: error.code(),
        message: error.to_string(),
        remediation: "restore algorithmic CUDA service or reduce the batch below its measured crossover",
    }
}
