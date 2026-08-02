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
    sparse_keywords, sparse_keywords_tf, token_hash,
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
    /// Hashed whitespace terms in a sparse ambient space, L1-normalized.
    ///
    /// The normalization makes this a *similarity* lane, not a ranking lane:
    /// every stored vector sums to 1.0, so a BM25 document length is 1.0 for
    /// every row and the `b`/avgdl correction cannot act (#1902). Use
    /// [`AlgorithmicEncoder::SparseKeywordsTf`] for a lexically-rankable lane.
    SparseKeywords { dim: u32 },
    /// Hashed whitespace terms as raw, unnormalized term-frequency counts.
    ///
    /// The BM25-scorable sibling of [`AlgorithmicEncoder::SparseKeywords`]:
    /// identical hashing, no normalization, so `tf` is a genuine count and the
    /// document length is a genuine length. This is the HashingTF-then-IDF
    /// split -- the encoder stays data-oblivious (a count depends only on the
    /// input bytes) and the corpus statistics stay in the index (#1902).
    SparseKeywordsTf { dim: u32 },
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
    /// Frozen bounded rank scalar placed on a unit half-circle (#1963).
    ///
    /// [`Self::SynScalarRank`] emits the rank as a **1-dimensional** dense
    /// vector. Cosine of two 1-D vectors is `sign(a*b)`, and a rank is confined
    /// to `[0, 1]`, so every pair of records is at cosine exactly `+1`: as a
    /// similarity lane the encoding is constant by construction and carries
    /// zero information, whatever the corpus. That is what
    /// [`AlgorithmicEncoder::dense_cosine_grading`] now declares and what a
    /// panel admission check refuses.
    ///
    /// This encoder keeps the same frozen bounds and the same exact rank, and
    /// places it on the unit half-circle instead:
    ///
    /// ```text
    /// u     = (value - min) / (max - min)          in [0, 1]
    /// phi(u) = [cos(pi * u), sin(pi * u)]
    /// cos(phi(a), phi(b)) = cos(pi * (u_a - u_b))
    /// ```
    ///
    /// The similarity therefore depends only on the rank *difference* and is
    /// strictly decreasing in `|u_a - u_b|` over the whole range, spanning
    /// `[-1, +1]` — the standard construction for making a bounded scalar
    /// comparable by inner product (the same `alpha * delta_max <= pi` scaling
    /// rule used for rotary spatio-temporal encodings).
    ///
    /// **Sizing is the caller's job and it is not optional.** Resolution is set
    /// by the frozen range: two values are indistinguishable once
    /// `cos(pi * du) > 1 - tol`, i.e. once `du < sqrt(2 * tol) / pi`. At the
    /// `1e-4` tolerance the discrimination measurement uses, that is
    /// `du < 0.0045`, so a range 130 years wide cannot separate two events a
    /// week apart. Freeze the range to the analysis window, not to the widest
    /// representable value.
    SynScalarRankArc { min_micros: i64, max_micros: i64 },
    /// Content-addressed categorical one-hot feature.
    SynOneHot { buckets: u32 },
    /// Signed feature hash in a power-of-two sparse space.
    SynHash { dim: u32 },
    /// Signed sparse text hash in a power-of-two sparse space.
    SynSparseText { dim: u32 },
    /// Raw hashed term frequencies over free text: unsigned, unnormalized, and
    /// therefore the only Syn* text lane a real BM25 scorer can rank (#1900).
    SynSparseTextTf { dim: u32 },
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
            Self::SynCyclicTime { .. } | Self::SynScalarRankArc { .. } => 2,
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
            | Self::SparseKeywordsTf { dim }
            | Self::SynHash { dim }
            | Self::SynSparseText { dim }
            | Self::SynSparseTextTf { dim }
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
    /// Whether this encoder content-addresses the **whole** input value into one
    /// cell, which is what makes an exact-match query mode possible (#1899).
    ///
    /// True only for `syn_hash`: `syn::hash` digests the entire byte string and
    /// lights exactly one bucket, so a query equal to a stored value lands in
    /// that same bucket and every record carrying that value is returned. That
    /// is genuine exact-match recall, and it is the capability excluded from
    /// free-text fusion by [`Self::accepts_free_text`] rather than made
    /// unreachable.
    ///
    /// One-hot encoders are excluded even though they also light one cell: their
    /// vocabulary is closed, so an out-of-vocabulary value still produces a
    /// bucket, and the "exact match" would be against a value the encoder never
    /// saw. Tokenizing lanes are excluded because a multi-token vector is not an
    /// assertion about the whole field.
    pub const fn accepts_exact_value(self) -> bool {
        matches!(self, Self::SynHash { .. })
    }

    pub const fn accepts_free_text(self) -> bool {
        match self {
            // Word-token lanes: the query is tokenized and hashed by exactly the
            // same code that tokenized and hashed the stored text.
            Self::SparseKeywords { .. }
            | Self::SparseKeywordsTf { .. }
            | Self::TokenHash { .. }
            | Self::SynSparseTextTf { .. }
            | Self::SynTokenSlots { .. }
            | Self::SynMultiHot { .. }
            // Character/byte-shape lanes over arbitrary text.
            | Self::ByteFeatures
            | Self::AstStyle => true,
            // Whole-string hashes are excluded, and this is the one call in the
            // table that could have gone either way. A hash lane looks
            // attractive — a query identical to a stored app name lands in the
            // same cell, which is exact-match recall. But it hashes *any* query
            // to exactly one cell, so it always returns a full ranked list: for
            // an unrelated phrase, whatever happens to share that bucket. On a
            // dim-1024 lane that is a roughly 1-in-1024 chance, per query, of
            // injecting an entire spurious lane into the rank fusion at full
            // weight, with nothing in the result to distinguish a real exact
            // match from a collision. Free-text recall must not be occasionally
            // and invisibly wrong, so exact-match-by-hash needs its own explicit
            // query mode rather than silent participation in every text query.
            Self::SynHash { .. }
            // A normalized signed hash measures the right tokens and cannot
            // *rank* them. `signed_sparse` L1-normalizes, so every stored vector
            // sums to 1.0 and each token's weight is 1/n_tokens: there is no
            // term frequency left to saturate and no document length left to
            // compare against a corpus average, and no IDF can be applied to a
            // weight that already absorbed the normalization. Measured on the
            // live index, that made a one-word title `Notepad` score exactly as
            // high on a record's own verbatim title as the record itself, and
            // the arbitrary tie decided recall (#1900). It stays a real
            // similarity feature — signed hashing is what makes a dot product
            // unbiased, which is what `by_example` wants — but free-text ranking
            // belongs to `SynSparseTextTf`, whose raw counts BM25 can rank.
            | Self::SynSparseText { .. }
            // Closed vocabularies: an out-of-vocabulary phrase still lands in a
            // bucket, so a match here would be fabricated, not measured.
            | Self::OneHot { .. }
            | Self::SynOneHot { .. }
            // Numeric, temporal and derived-statistic encoders: a phrase is not
            // one of their inputs.
            | Self::Scalar
            | Self::SynCyclicTime { .. }
            | Self::SynScalarRaw
            | Self::SynScalarLog1p
            | Self::SynScalarZScore { .. }
            | Self::SynScalarRank { .. }
            | Self::SynScalarRankArc { .. }
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
            | Self::SparseKeywordsTf { dim }
            | Self::SynHash { dim }
            | Self::SynSparseText { dim }
            | Self::SynSparseTextTf { dim }
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

    /// What this encoder's **dense cosine** can express, before any corpus is
    /// consulted (#1963).
    ///
    /// Every neighbourhood analysis Calyx runs over a dense slot — find-similar
    /// ranking, the agreement cross-term, the between-record graph, the
    /// blind-spot rule, the guard's per-slot threshold — reads one number: the
    /// cosine between two slot vectors. That number is only a *measurement*
    /// when the encoder's image is rich enough to produce more than a handful
    /// of values. For several encoders it provably is not, and no amount of
    /// data changes that:
    ///
    /// * a dense vector of dimension 1 has cosine `sign(a * b)`, so at most two
    ///   values, and exactly **one** when the encoder's output is confined to
    ///   one sign — which is the case for every bounded `[0, 1]` transform;
    /// * a one-hot or bin over `k` buckets returns cosine `1` for a shared
    ///   bucket and `0` otherwise, whatever `k` is.
    ///
    /// #1963 found all five dense lenses on the active operator panel returning
    /// nearest-neighbour cosine exactly `1.0` for all 924 records. Four of those
    /// were corpus-driven saturation of a finite image; the fifth
    /// (`syn_scalar_rank` at `dim = 1`) could never have returned anything else.
    /// Declaring the distinction is what lets a panel refuse "I carry no graded
    /// view" at admission rather than three analyses later.
    ///
    /// This is deliberately a statement about the **encoder**, not the corpus.
    /// [`Graded`](DenseCosineGrading::Graded) means "the image is a continuum,
    /// so grading is *possible*" — whether it is *realised* is a corpus fact
    /// that only `calyx_loom::SimilarityDiscrimination` can answer.
    pub const fn dense_cosine_grading(self) -> DenseCosineGrading {
        match self.shape() {
            // Sparse and multi-vector lanes are scored by overlap and late
            // interaction, not by a fixed-dimension cosine: their image is the
            // token set, which is a corpus property this declaration cannot and
            // must not pre-judge.
            SlotShape::Sparse(_) | SlotShape::Multi { .. } => DenseCosineGrading::NotDense,
            SlotShape::Dense(dim) => match self {
                // Bounded non-negative scalars: the image is a single ray, so
                // cosine is identically +1 for every pair of records.
                Self::SynScalarRank { .. }
                | Self::SynOrdinal { .. }
                | Self::SynFrequency { .. } => DenseCosineGrading::Constant,
                // Signed scalars: the image is two opposite rays, so cosine is
                // +1 or -1 and nothing between.
                Self::Scalar
                | Self::SynScalarRaw
                | Self::SynScalarLog1p
                | Self::SynScalarZScore { .. }
                | Self::SynTargetMean { .. }
                | Self::SynDelta { .. }
                | Self::SynRate { .. } => DenseCosineGrading::Finite(2),
                // Closed vocabularies: cosine is 1 on a shared bucket, 0
                // otherwise, so the image is `buckets` orthogonal directions.
                Self::OneHot { buckets } | Self::SynOneHot { buckets } => {
                    DenseCosineGrading::Finite(if buckets == 0 { 1 } else { buckets })
                }
                Self::SynBin { buckets, .. } => {
                    DenseCosineGrading::Finite(if buckets == 0 { 1 } else { buckets })
                }
                // A periodic encoder's declared domain is `period` positions on
                // a cycle — an hour of the day, a day of the week — and every
                // built-in caller passes exactly that: an integer position. So
                // its similarity takes `period` values and saturates as soon as
                // the record count exceeds them, which is why `hour_cyclic` and
                // `dow_cyclic` both returned nearest-neighbour cosine 1.0 for
                // all 924 records of #1963.
                //
                // A fractional input would reach more directions, so this is a
                // bound on the *declared* use rather than on every reachable
                // one. Under-stating is the safe direction for an admission
                // gate: it can only make the gate stricter, never let a
                // panel with no graded view through.
                Self::SynCyclicTime { period } => {
                    DenseCosineGrading::Finite(if period == 0 { 1 } else { period })
                }
                // Everything else with more than one dimension mixes its input
                // continuously across components, so the reachable direction
                // set is a continuum.
                _ => {
                    if dim <= 1 {
                        DenseCosineGrading::Finite(2)
                    } else {
                        DenseCosineGrading::Graded
                    }
                }
            },
        }
    }
}

/// What an encoder's dense cosine can express before any corpus is consulted.
///
/// See [`AlgorithmicEncoder::dense_cosine_grading`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenseCosineGrading {
    /// Not a dense lane: grading is a corpus property, not an encoder one.
    NotDense,
    /// Cosine is identically `+1` for every pair. The lane carries no
    /// similarity information at all and must never be offered as one.
    Constant,
    /// Cosine takes at most this many values, whatever the corpus. Usable, but
    /// it saturates as soon as the record count exceeds the image size.
    Finite(u32),
    /// The image is a continuum, so a graded similarity is possible. Whether it
    /// is realised over a given corpus is a separate, measured question.
    Graded,
}

impl DenseCosineGrading {
    /// Whether this lane can carry a graded similarity at all.
    #[must_use]
    pub const fn is_graded(self) -> bool {
        matches!(self, Self::Graded)
    }

    /// Whether this lane provably carries no similarity information.
    #[must_use]
    pub const fn is_constant(self) -> bool {
        matches!(self, Self::Constant)
    }

    /// A stable, human-readable name for error text and readbacks.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotDense => "not_dense",
            Self::Constant => "constant",
            Self::Finite(_) => "finite",
            Self::Graded => "graded",
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

    /// The BM25-scorable, raw-count sibling of [`Self::sparse_keywords`].
    pub fn sparse_keywords_tf(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SparseKeywordsTf { dim })
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

    /// The graded-cosine sibling of [`Self::syn_scalar_rank`] (#1963).
    ///
    /// Read [`AlgorithmicEncoder::SynScalarRankArc`] before choosing the
    /// bounds: the range is what sets the resolution.
    pub fn syn_scalar_rank_arc(
        name: impl Into<String>,
        modality: Modality,
        min_micros: i64,
        max_micros: i64,
    ) -> Self {
        Self::new(
            name,
            modality,
            AlgorithmicEncoder::SynScalarRankArc {
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

    /// A raw term-frequency lexical lane (#1900).
    ///
    /// Use this, not [`Self::syn_sparse_text`], wherever the lane is meant to be
    /// ranked by BM25: the normalized signed lane cannot carry a term frequency
    /// or a document length, so IDF and length saturation have nothing to work
    /// with there.
    pub fn syn_sparse_text_tf(name: impl Into<String>, modality: Modality, dim: u32) -> Self {
        Self::new(name, modality, AlgorithmicEncoder::SynSparseTextTf { dim })
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
            AlgorithmicEncoder::SparseKeywordsTf { dim } => sparse_keywords_tf(&input.bytes, dim)?,
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
            AlgorithmicEncoder::SynScalarRankArc {
                min_micros,
                max_micros,
            } => syn::scalar_rank_arc(&input.bytes, min_micros, max_micros)?,
            AlgorithmicEncoder::SynOneHot { buckets } => syn::one_hot(&input.bytes, buckets)?,
            AlgorithmicEncoder::SynHash { dim } => syn::hash(&input.bytes, dim)?,
            AlgorithmicEncoder::SynSparseText { dim } => syn::sparse_text(&input.bytes, dim)?,
            AlgorithmicEncoder::SynSparseTextTf { dim } => syn::sparse_text_tf(&input.bytes, dim)?,
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
        // Both emit unit-length vectors by construction: the record vector
        // normalizes explicitly, the rank arc lands on the unit circle.
        AlgorithmicEncoder::SynRecordVector { .. }
        | AlgorithmicEncoder::SynScalarRankArc { .. } => NormPolicy::unit(),
        _ => NormPolicy::None,
    }
}
