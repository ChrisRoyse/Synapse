//! Lightweight deterministic Calyx lenses.
//!
//! This crate intentionally contains no model, embedding, ONNX, Candle,
//! registry, panel, placement, or profiling runtime. It exists for hot storage
//! paths that only need frozen Syn* algorithmic measurements.

use calyx_core::{Input, Lens, LensId, Modality, Result, SlotShape, SlotVector};
use sha2::{Digest as _, Sha256};

mod syn;

pub mod measure;

/// Deterministic Syn* encoders with no model weights or registry runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlgorithmicEncoder {
    /// Periodic time value encoded as sin/cos over a frozen period.
    SynCyclicTime { period: u32 },
    /// Strict numeric scalar pass-through.
    SynScalarRaw,
    /// Strict numeric log1p scalar.
    SynScalarLog1p,
    /// Frozen z-score scalar transform with micro-unit parameters.
    SynScalarZScore { mean_micros: i64, std_micros: u64 },
    /// Frozen bounded rank scalar transform with micro-unit bounds.
    SynScalarRank { min_micros: i64, max_micros: i64 },
    /// Content-addressed categorical one-hot feature.
    SynOneHot { buckets: u32 },
    /// Signed feature hash in a power-of-two sparse space.
    SynHash { dim: u32 },
    /// Signed sparse text hash in a power-of-two sparse space.
    SynSparseText { dim: u32 },
    /// Hashed text token slots for multi-vector retrieval.
    SynTokenSlots { token_dim: u32 },
    /// Signed multi-hot flag hash in a power-of-two sparse space.
    SynMultiHot { dim: u32 },
    /// Unit-normalized structured numeric record vector.
    SynRecordVector { dim: u32 },
    /// Frozen numeric bin one-hot feature with micro-unit bounds.
    SynBin {
        buckets: u32,
        min_micros: i64,
        max_micros: i64,
    },
    /// Frozen ordinal level encoded on [0, 1].
    SynOrdinal { levels: u32 },
    /// Frozen frequency statistic.
    SynFrequency { count: u64, total: u64 },
    /// Frozen held-out target mean statistic.
    SynTargetMean {
        mean_micros: i64,
        fold_count: u32,
        outcome_hash: u32,
    },
    /// Frozen-scale delta transform.
    SynDelta { scale_micros: u64 },
    /// Frozen-scale rate transform.
    SynRate { scale_micros: u64 },
    /// Signed pairwise crossed features in a power-of-two sparse space.
    SynCross { dim: u32 },
    /// Dense deterministic aggregation summary over structured numerics.
    SynAggregation { dim: u32 },
}

impl AlgorithmicEncoder {
    /// Returns the primary output dimension.
    pub const fn dim(self) -> u32 {
        match self {
            Self::SynCyclicTime { .. } => 2,
            Self::SynScalarRaw
            | Self::SynScalarLog1p
            | Self::SynScalarZScore { .. }
            | Self::SynScalarRank { .. }
            | Self::SynOrdinal { .. }
            | Self::SynFrequency { .. }
            | Self::SynTargetMean { .. }
            | Self::SynDelta { .. }
            | Self::SynRate { .. } => 1,
            Self::SynOneHot { buckets } | Self::SynBin { buckets, .. } => buckets,
            Self::SynHash { dim }
            | Self::SynSparseText { dim }
            | Self::SynMultiHot { dim }
            | Self::SynRecordVector { dim }
            | Self::SynCross { dim }
            | Self::SynAggregation { dim } => {
                if dim == 0 {
                    1
                } else {
                    dim
                }
            }
            Self::SynTokenSlots { token_dim } => token_dim,
        }
    }

    /// Returns the exact output shape used by the registry Syn* contract.
    pub const fn shape(self) -> SlotShape {
        match self {
            Self::SynHash { dim }
            | Self::SynSparseText { dim }
            | Self::SynMultiHot { dim }
            | Self::SynCross { dim } => SlotShape::Sparse(if dim == 0 { 1 } else { dim }),
            Self::SynTokenSlots { token_dim } => SlotShape::Multi { token_dim },
            Self::SynRecordVector { dim } | Self::SynAggregation { dim } => SlotShape::Dense(dim),
            _ => SlotShape::Dense(self.dim()),
        }
    }
}

/// A frozen Syn* algorithmic lens.
#[derive(Clone, Debug)]
pub struct AlgorithmicLens {
    id: LensId,
    modality: Modality,
    encoder: AlgorithmicEncoder,
}

impl AlgorithmicLens {
    pub fn syn_cyclic_time(name: impl Into<String>, modality: Modality, period: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynCyclicTime { period })
    }

    pub fn syn_scalar_raw(name: impl Into<String>, modality: Modality) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynScalarRaw)
    }

    pub fn syn_scalar_log1p(name: impl Into<String>, modality: Modality) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynScalarLog1p)
    }

    pub fn syn_scalar_zscore(
        name: impl Into<String>,
        modality: Modality,
        mean_micros: i64,
        std_micros: u64,
    ) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynScalarZScore {
                mean_micros,
                std_micros,
            },
        )
    }

    pub fn syn_scalar_rank(
        name: impl Into<String>,
        modality: Modality,
        min_micros: i64,
        max_micros: i64,
    ) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynScalarRank {
                min_micros,
                max_micros,
            },
        )
    }

    pub fn syn_one_hot(name: impl Into<String>, modality: Modality, buckets: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynOneHot { buckets })
    }

    pub fn syn_hash(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynHash { dim })
    }

    pub fn syn_sparse_text(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynSparseText { dim })
    }

    pub fn syn_token_slots(name: impl Into<String>, modality: Modality, token_dim: u32) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynTokenSlots { token_dim },
        )
    }

    pub fn syn_multi_hot(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynMultiHot { dim })
    }

    pub fn syn_record_vector(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynRecordVector { dim })
    }

    pub fn syn_bin(
        name: impl Into<String>,
        modality: Modality,
        buckets: u32,
        min_micros: i64,
        max_micros: i64,
    ) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynBin {
                buckets,
                min_micros,
                max_micros,
            },
        )
    }

    pub fn syn_ordinal(name: impl Into<String>, modality: Modality, levels: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynOrdinal { levels })
    }

    pub fn syn_frequency(
        name: impl Into<String>,
        modality: Modality,
        count: u64,
        total: u64,
    ) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynFrequency { count, total },
        )
    }

    pub fn syn_target_mean(
        name: impl Into<String>,
        modality: Modality,
        mean_micros: i64,
        fold_count: u32,
        outcome_hash: u32,
    ) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynTargetMean {
                mean_micros,
                fold_count,
                outcome_hash,
            },
        )
    }

    pub fn syn_delta(name: impl Into<String>, modality: Modality, scale_micros: u64) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynDelta { scale_micros },
        )
    }

    pub fn syn_rate(name: impl Into<String>, modality: Modality, scale_micros: u64) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynRate { scale_micros })
    }

    pub fn syn_cross(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynCross { dim })
    }

    pub fn syn_aggregation(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynAggregation { dim })
    }

    /// Creates a Syn* algorithmic lens from an encoder.
    pub fn new(name: impl Into<String>, modality: Modality, encoder: AlgorithmicEncoder) -> Self {
        let name = name.into();
        let id = algorithmic_lens_id(&name, encoder);
        Self {
            id,
            modality,
            encoder,
        }
    }

    fn measure_cpu(&self, input: &Input) -> Result<SlotVector> {
        Ok(match self.encoder {
            AlgorithmicEncoder::SynCyclicTime { period } => syn::cyclic_time(&input.bytes, period)?,
            AlgorithmicEncoder::SynScalarRaw => syn::scalar_raw(&input.bytes)?,
            AlgorithmicEncoder::SynScalarLog1p => syn::scalar_log1p(&input.bytes)?,
            AlgorithmicEncoder::SynScalarZScore {
                mean_micros,
                std_micros,
            } => syn::scalar_zscore(&input.bytes, mean_micros, std_micros)?,
            AlgorithmicEncoder::SynScalarRank {
                min_micros,
                max_micros,
            } => syn::scalar_rank(&input.bytes, min_micros, max_micros)?,
            AlgorithmicEncoder::SynOneHot { buckets } => syn::one_hot(&input.bytes, buckets)?,
            AlgorithmicEncoder::SynHash { dim } => syn::hash(&input.bytes, dim)?,
            AlgorithmicEncoder::SynSparseText { dim } => syn::sparse_text(&input.bytes, dim)?,
            AlgorithmicEncoder::SynTokenSlots { token_dim } => {
                syn::token_slots(&input.bytes, token_dim)?
            }
            AlgorithmicEncoder::SynMultiHot { dim } => syn::multi_hot(&input.bytes, dim)?,
            AlgorithmicEncoder::SynRecordVector { dim } => syn::record_vector(&input.bytes, dim)?,
            AlgorithmicEncoder::SynBin {
                buckets,
                min_micros,
                max_micros,
            } => syn::bin(&input.bytes, buckets, min_micros, max_micros)?,
            AlgorithmicEncoder::SynOrdinal { levels } => syn::ordinal(&input.bytes, levels)?,
            AlgorithmicEncoder::SynFrequency { count, total } => syn::frequency(count, total)?,
            AlgorithmicEncoder::SynTargetMean {
                mean_micros,
                fold_count,
                outcome_hash,
            } => syn::target_mean(mean_micros, fold_count, outcome_hash)?,
            AlgorithmicEncoder::SynDelta { scale_micros } => {
                syn::delta(&input.bytes, scale_micros)?
            }
            AlgorithmicEncoder::SynRate { scale_micros } => syn::rate(&input.bytes, scale_micros)?,
            AlgorithmicEncoder::SynCross { dim } => syn::cross(&input.bytes, dim)?,
            AlgorithmicEncoder::SynAggregation { dim } => syn::aggregation(&input.bytes, dim)?,
        })
    }
}

impl Lens for AlgorithmicLens {
    fn id(&self) -> LensId {
        self.id
    }

    fn shape(&self) -> SlotShape {
        self.encoder.shape()
    }

    fn modality(&self) -> Modality {
        self.modality
    }

    fn measure(&self, input: &Input) -> Result<SlotVector> {
        ensure_input_modality(self, input)?;
        self.measure_cpu(input)
    }
}

fn algorithmic_lens_id(name: &str, encoder: AlgorithmicEncoder) -> LensId {
    let encoder_text = format!("{encoder:?}:{}", encoder.dim());
    let weights_sha256 = sha256_digest(&[b"algorithmic-runtime-v2", encoder_text.as_bytes()]);
    let corpus_hash = sha256_digest(&[b"algorithmic-data-oblivious"]);
    LensId::from_parts(
        name,
        &weights_sha256,
        &corpus_hash,
        output_shape_fingerprint(encoder).as_bytes(),
    )
}

fn output_shape_fingerprint(encoder: AlgorithmicEncoder) -> String {
    format!(
        "dtype=f32;shape={};norm={}",
        shape_fingerprint(encoder.shape()),
        norm_fingerprint(encoder)
    )
}

fn shape_fingerprint(shape: SlotShape) -> String {
    match shape {
        SlotShape::Dense(dim) => format!("dense:{dim}"),
        SlotShape::Sparse(dim) => format!("sparse:{dim}"),
        SlotShape::Multi { token_dim } => format!("multi:{token_dim}"),
    }
}

fn norm_fingerprint(encoder: AlgorithmicEncoder) -> &'static str {
    match encoder {
        AlgorithmicEncoder::SynRecordVector { .. } => "unit",
        _ => "finite",
    }
}

fn sha256_digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn ensure_input_modality(lens: &dyn Lens, input: &Input) -> Result<()> {
    if input.modality == lens.modality() {
        return Ok(());
    }

    Err(calyx_core::CalyxError::lens_dim_mismatch(format!(
        "lens {} accepts {:?}, got {:?}",
        lens.id(),
        lens.modality(),
        input.modality
    )))
}
