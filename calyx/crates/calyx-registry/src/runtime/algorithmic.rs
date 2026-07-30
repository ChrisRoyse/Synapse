use std::sync::Arc;

use calyx_core::{Input, Lens, LensId, Modality, Result, SlotShape, SlotVector};

use crate::frozen::{FrozenLensContract, LensDType, NormPolicy, sha256_digest};
use crate::lens::ensure_input_modality;

mod batch;
mod cpu;
mod gdelt;
mod syn;

pub use batch::{
    AlgorithmicBatchProvider, AlgorithmicBatchStats, BYTE_FEATURES_CUDA_MIN_INPUT_BYTES,
    SPARSE_KEYWORDS_CUDA_MIN_TOKENS, TOKEN_HASH_CUDA_MIN_WORDS,
};
use cpu::{
    ast_style_features, byte_features, hash_part, one_hot_features, scalar_features,
    sparse_keywords, token_hash,
};

const BYTE_FEATURE_DIM: u32 = 16;

/// Deterministic, data-local feature encoders with no model weights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlgorithmicEncoder {
    /// Byte and character-class features for text/code/structured inputs.
    ByteFeatures,
    /// Single scalar summary.
    Scalar,
    /// Hash-selected one-hot feature vector.
    OneHot { buckets: u32 },
    /// Small AST/code-style feature vector.
    AstStyle,
    /// Hashed whitespace terms in a sparse ambient space.
    SparseKeywords { dim: u32 },
    /// Hashed whitespace terms as per-token vectors for MaxSim.
    TokenHash { token_dim: u32 },
    /// Dense CAMEO/event-code features from GDELT text rows.
    GdeltCameo,
    /// Sparse actor/country/geography entity features from GDELT text rows.
    GdeltActorGeo { dim: u32 },
    /// Sparse source URL host/path features from GDELT text rows.
    GdeltSourceDomain { dim: u32 },
    /// Sparse event-code/geography interaction features from GDELT text rows.
    GdeltEventGeo { dim: u32 },
    /// Sparse directed actor-pair features from GDELT text rows.
    GdeltActorPair { dim: u32 },
    /// Sparse event-code/actor interaction features from GDELT text rows.
    GdeltEventActor { dim: u32 },
    /// Sparse Goldstein/tone bucket features from GDELT text rows.
    GdeltToneSignal { dim: u32 },
    /// Sparse source-domain/event-code interaction features from GDELT text rows.
    GdeltSourceEvent { dim: u32 },
    /// Sparse action-geo-only features from GDELT text rows.
    GdeltActionGeo { dim: u32 },
    /// Sparse actor-country/presence-only features from GDELT text rows.
    GdeltActorCountry { dim: u32 },
    /// Sparse source-host-only features from GDELT text rows.
    GdeltSourceHost { dim: u32 },
    /// Sparse SQLDATE/date bucket features from GDELT text rows.
    GdeltSqlDate { dim: u32 },
    /// Sparse event-code-only features from GDELT text rows.
    GdeltEventCode { dim: u32 },
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
            Self::ByteFeatures => BYTE_FEATURE_DIM,
            Self::Scalar => 1,
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
            Self::OneHot { buckets } => {
                if buckets == 0 {
                    1
                } else {
                    buckets
                }
            }
            Self::SynOneHot { buckets } | Self::SynBin { buckets, .. } => buckets,
            Self::AstStyle => 8,
            Self::SparseKeywords { dim }
            | Self::SynHash { dim }
            | Self::SynSparseText { dim }
            | Self::SynMultiHot { dim }
            | Self::SynRecordVector { dim }
            | Self::SynCross { dim }
            | Self::SynAggregation { dim }
            | Self::GdeltActorGeo { dim }
            | Self::GdeltSourceDomain { dim }
            | Self::GdeltEventGeo { dim }
            | Self::GdeltActorPair { dim }
            | Self::GdeltEventActor { dim }
            | Self::GdeltToneSignal { dim }
            | Self::GdeltSourceEvent { dim }
            | Self::GdeltActionGeo { dim }
            | Self::GdeltActorCountry { dim }
            | Self::GdeltSourceHost { dim }
            | Self::GdeltSqlDate { dim }
            | Self::GdeltEventCode { dim } => {
                if dim == 0 {
                    1
                } else {
                    dim
                }
            }
            Self::TokenHash { token_dim } => {
                if token_dim == 0 {
                    1
                } else {
                    token_dim
                }
            }
            Self::SynTokenSlots { token_dim } => token_dim,
            Self::GdeltCameo => 16,
        }
    }

    /// Whether measuring a free-text query string through this encoder produces
    /// a vector comparable to the vectors it produced at ingest (issue #1896).
    ///
    /// This is the declared, per-encoder answer to "is this lens text-queryable?"
    /// that the coarse [`Modality`] tag cannot give. It is true exactly for the
    /// encoders whose input *is* a string of words or an opaque byte string —
    /// the lexical lanes a text query is meant to probe — and false for every
    /// encoder whose input is a number, a timestamp, or a value drawn from a
    /// closed vocabulary, because measuring a phrase through those either fails
    /// outright (the numeric encoders reject non-numeric bytes) or silently
    /// hashes the phrase into a bucket and reports a confident false match (the
    /// one-hot encoders).
    ///
    /// The GDELT encoders are `false` here: they parse a fixed GDELT row layout,
    /// not a phrase. They are declared `Modality::Text` and stay reachable
    /// through the modality arm of [`AlgorithmicLens::text_queryable`], so this
    /// classification cannot narrow existing recall — it only widens it to the
    /// structured-tagged text encoders that were previously unreachable.
    pub const fn accepts_free_text(self) -> bool {
        match self {
            // Word-token lanes: the query is tokenized and hashed by exactly the
            // same code that tokenized and hashed the stored text.
            Self::SparseKeywords { .. }
            | Self::TokenHash { .. }
            | Self::SynSparseText { .. }
            | Self::SynTokenSlots { .. }
            | Self::SynMultiHot { .. }
            // Whole-string lanes: the query hashes to the same cell as an
            // identical stored string, which is exact-match recall.
            | Self::SynHash { .. }
            // Character/byte-shape lanes over arbitrary text.
            | Self::ByteFeatures
            | Self::AstStyle => true,
            // Closed vocabularies: an out-of-vocabulary phrase still lands in a
            // bucket, so a match here would be fabricated, not measured.
            Self::OneHot { .. }
            | Self::SynOneHot { .. }
            // Numeric, temporal and derived-statistic encoders: a phrase is not
            // one of their inputs.
            | Self::Scalar
            | Self::SynCyclicTime { .. }
            | Self::SynScalarRaw
            | Self::SynScalarLog1p
            | Self::SynScalarZScore { .. }
            | Self::SynScalarRank { .. }
            | Self::SynRecordVector { .. }
            | Self::SynBin { .. }
            | Self::SynOrdinal { .. }
            | Self::SynFrequency { .. }
            | Self::SynTargetMean { .. }
            | Self::SynDelta { .. }
            | Self::SynRate { .. }
            | Self::SynCross { .. }
            | Self::SynAggregation { .. }
            // Fixed-layout GDELT row parsers, reachable via their Text modality.
            | Self::GdeltCameo
            | Self::GdeltActorGeo { .. }
            | Self::GdeltSourceDomain { .. }
            | Self::GdeltEventGeo { .. }
            | Self::GdeltActorPair { .. }
            | Self::GdeltEventActor { .. }
            | Self::GdeltToneSignal { .. }
            | Self::GdeltSourceEvent { .. }
            | Self::GdeltActionGeo { .. }
            | Self::GdeltActorCountry { .. }
            | Self::GdeltSourceHost { .. }
            | Self::GdeltSqlDate { .. }
            | Self::GdeltEventCode { .. } => false,
        }
    }

    pub const fn shape(self) -> SlotShape {
        match self {
            Self::SparseKeywords { dim }
            | Self::SynHash { dim }
            | Self::SynSparseText { dim }
            | Self::SynMultiHot { dim }
            | Self::SynCross { dim }
            | Self::GdeltActorGeo { dim }
            | Self::GdeltSourceDomain { dim }
            | Self::GdeltEventGeo { dim }
            | Self::GdeltActorPair { dim }
            | Self::GdeltEventActor { dim }
            | Self::GdeltToneSignal { dim }
            | Self::GdeltSourceEvent { dim }
            | Self::GdeltActionGeo { dim }
            | Self::GdeltActorCountry { dim }
            | Self::GdeltSourceHost { dim }
            | Self::GdeltSqlDate { dim }
            | Self::GdeltEventCode { dim } => SlotShape::Sparse(if dim == 0 { 1 } else { dim }),
            Self::TokenHash { token_dim } => SlotShape::Multi {
                token_dim: if token_dim == 0 { 1 } else { token_dim },
            },
            Self::SynTokenSlots { token_dim } => SlotShape::Multi { token_dim },
            Self::SynRecordVector { dim } | Self::SynAggregation { dim } => SlotShape::Dense(dim),
            _ => SlotShape::Dense(self.dim()),
        }
    }
}

/// A frozen algorithmic lens.
#[derive(Clone, Debug)]
pub struct AlgorithmicLens {
    id: LensId,
    modality: Modality,
    encoder: AlgorithmicEncoder,
    contract: FrozenLensContract,
    batch: Arc<batch::BatchState>,
}

impl AlgorithmicLens {
    /// Creates an algorithmic byte-feature lens.
    pub fn byte_features(name: impl Into<String>, modality: Modality) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::ByteFeatures)
    }

    pub fn scalar(name: impl Into<String>, modality: Modality) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::Scalar)
    }

    pub fn one_hot(name: impl Into<String>, modality: Modality, buckets: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::OneHot { buckets })
    }

    pub fn ast_style(name: impl Into<String>, modality: Modality) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::AstStyle)
    }

    pub fn sparse_keywords(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SparseKeywords { dim })
    }

    pub fn token_hash(name: impl Into<String>, modality: Modality, token_dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::TokenHash { token_dim })
    }

    pub fn gdelt_cameo(name: impl Into<String>, modality: Modality) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltCameo)
    }

    pub fn gdelt_actor_geo(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltActorGeo { dim })
    }

    pub fn gdelt_source_domain(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::GdeltSourceDomain { dim },
        )
    }

    pub fn gdelt_event_geo(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltEventGeo { dim })
    }

    pub fn gdelt_actor_pair(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltActorPair { dim })
    }

    pub fn gdelt_event_actor(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltEventActor { dim })
    }

    pub fn gdelt_tone_signal(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltToneSignal { dim })
    }

    pub fn gdelt_source_event(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltSourceEvent { dim })
    }

    pub fn gdelt_action_geo(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltActionGeo { dim })
    }

    pub fn gdelt_actor_country(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::GdeltActorCountry { dim },
        )
    }

    pub fn gdelt_source_host(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltSourceHost { dim })
    }

    pub fn gdelt_sql_date(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltSqlDate { dim })
    }

    pub fn gdelt_event_code(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::GdeltEventCode { dim })
    }

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

    /// Creates an algorithmic lens from an encoder.
    pub fn new(name: impl Into<String>, modality: Modality, encoder: AlgorithmicEncoder) -> Self {
        let name = name.into();
        let contract = algorithmic_contract(&name, modality, encoder);
        let id = contract.lens_id();
        Self {
            id,
            modality,
            encoder,
            contract,
            batch: Arc::new(batch::BatchState::default()),
        }
    }

    /// Returns the frozen contract that produced this lens id.
    pub fn contract(&self) -> &FrozenLensContract {
        &self.contract
    }

    /// Returns the exact frozen encoder discriminator used by this lens.
    ///
    /// Durable panel publishers use this to serialize a reconstructable
    /// `LensRuntime::Algorithmic` contract beside the panel instead of
    /// publishing an empty registry snapshot.
    pub const fn encoder(&self) -> AlgorithmicEncoder {
        self.encoder
    }

    /// Returns the most recent serializable batch provider/transfer evidence.
    pub fn last_batch_stats(&self) -> Option<AlgorithmicBatchStats> {
        self.batch.last_stats()
    }

    fn measure_cpu(&self, input: &Input) -> Result<SlotVector> {
        Ok(match self.encoder {
            AlgorithmicEncoder::ByteFeatures => SlotVector::Dense {
                dim: self.encoder.dim(),
                data: byte_features(&input.bytes),
            },
            AlgorithmicEncoder::Scalar => SlotVector::Dense {
                dim: self.encoder.dim(),
                data: scalar_features(&input.bytes),
            },
            AlgorithmicEncoder::OneHot { buckets } => SlotVector::Dense {
                dim: self.encoder.dim(),
                data: one_hot_features(&input.bytes, buckets),
            },
            AlgorithmicEncoder::AstStyle => SlotVector::Dense {
                dim: self.encoder.dim(),
                data: ast_style_features(&input.bytes),
            },
            AlgorithmicEncoder::SparseKeywords { dim } => sparse_keywords(&input.bytes, dim)?,
            AlgorithmicEncoder::TokenHash { token_dim } => token_hash(&input.bytes, token_dim)?,
            AlgorithmicEncoder::GdeltCameo => SlotVector::Dense {
                dim: self.encoder.dim(),
                data: gdelt::cameo_features(&input.bytes),
            },
            AlgorithmicEncoder::GdeltActorGeo { dim } => gdelt::actor_geo(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltSourceDomain { dim } => {
                gdelt::source_domain(&input.bytes, dim)?
            }
            AlgorithmicEncoder::GdeltEventGeo { dim } => gdelt::event_geo(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltActorPair { dim } => gdelt::actor_pair(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltEventActor { dim } => gdelt::event_actor(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltToneSignal { dim } => gdelt::tone_signal(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltSourceEvent { dim } => gdelt::source_event(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltActionGeo { dim } => gdelt::action_geo(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltActorCountry { dim } => {
                gdelt::actor_country(&input.bytes, dim)?
            }
            AlgorithmicEncoder::GdeltSourceHost { dim } => {
                gdelt::source_host_lens(&input.bytes, dim)?
            }
            AlgorithmicEncoder::GdeltSqlDate { dim } => gdelt::sql_date(&input.bytes, dim)?,
            AlgorithmicEncoder::GdeltEventCode { dim } => gdelt::event_code(&input.bytes, dim)?,
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

    fn text_queryable(&self) -> bool {
        // Additive by construction (#1896): anything the modality gate already
        // admitted stays admitted, and the encoder declaration only adds the
        // text lanes that a coarse `Modality::Structured` tag was hiding.
        self.modality == Modality::Text || self.encoder.accepts_free_text()
    }

    fn measure(&self, input: &Input) -> Result<SlotVector> {
        ensure_input_modality(self, input)?;
        let output = self.measure_cpu(input)?;
        self.batch.record(batch::cpu_stats(
            self.encoder,
            std::slice::from_ref(input),
            1,
            "single-input CPU path",
        ));
        Ok(output)
    }

    fn measure_batch(&self, inputs: &[Input]) -> Result<Vec<SlotVector>> {
        batch::measure_batch(self, inputs)
    }
}

fn algorithmic_contract(
    name: &str,
    modality: Modality,
    encoder: AlgorithmicEncoder,
) -> FrozenLensContract {
    if encoder == AlgorithmicEncoder::ByteFeatures {
        return FrozenLensContract::algorithmic_byte_features(name, modality);
    }
    let encoder_text = format!("{encoder:?}:{}", encoder.dim());
    FrozenLensContract::new(
        name,
        sha256_digest(&[b"algorithmic-runtime-v2", encoder_text.as_bytes()]),
        sha256_digest(&[b"algorithmic-data-oblivious"]),
        encoder.shape(),
        modality,
        LensDType::F32,
        algorithmic_norm_policy(encoder),
    )
}

fn algorithmic_norm_policy(encoder: AlgorithmicEncoder) -> NormPolicy {
    match encoder {
        AlgorithmicEncoder::SynRecordVector { .. } => NormPolicy::unit(),
        _ => NormPolicy::None,
    }
}
