use std::sync::Arc;

use calyx_core::{CalyxError, Lens, Result, SlotShape};

use crate::frozen::{FrozenLensContract, LensDType, NormPolicy, sha256_digest};
use crate::{
    AlgorithmicEncoder, AlgorithmicLens, CandleLens, ExternalCmdLens, FastembedBgem3Lens,
    FastembedQwen3Lens, FastembedRerankerLens, FastembedSparseLens, LensRuntime, LensSpec,
    MultimodalAdapterLens, OnnxColbertLens, OnnxLens, StaticLookupLens, TeiHttpLens,
};

pub(crate) fn load_runtime_lens_from_spec(
    spec: &LensSpec,
) -> Result<(Arc<dyn Lens>, FrozenLensContract)> {
    match &spec.runtime {
        LensRuntime::Algorithmic { kind } => {
            let lens = algorithmic_lens(spec, kind).ok_or_else(|| {
                lens_config_invalid(format!(
                    "unsupported algorithmic lens kind {kind} for persisted lens {}",
                    spec.name
                ))
            })?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::TeiHttp { endpoint } => {
            let dim = dense_dim(spec.output).ok_or_else(|| {
                lens_config_invalid(format!(
                    "TEI lens {} requires dense output shape, got {:?}",
                    spec.name, spec.output
                ))
            })?;
            let lens = TeiHttpLens::new(&spec.name, endpoint, spec.modality, dim);
            let contract = FrozenLensContract::tei_http(&spec.name, endpoint, spec.modality, dim);
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::ExternalCmd { cmd, args } => {
            let dim = dense_dim(spec.output).ok_or_else(|| {
                lens_config_invalid(format!(
                    "external command lens {} requires dense output shape, got {:?}",
                    spec.name, spec.output
                ))
            })?;
            let lens = ExternalCmdLens::new(&spec.name, cmd, args.clone(), spec.modality, dim);
            let args_text = args.join("\0");
            let contract = FrozenLensContract::new(
                spec.name.clone(),
                sha256_digest(&[cmd.as_bytes(), args_text.as_bytes()]),
                sha256_digest(&[b"external-cmd-runtime-v1"]),
                SlotShape::Dense(dim),
                spec.modality,
                LensDType::F32,
                NormPolicy::None,
            );
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::CandleLocal { .. } => {
            let lens = CandleLens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::Onnx { .. } => {
            let lens = OnnxLens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::OnnxColbert { .. } => {
            let lens = OnnxColbertLens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::FastembedSparse { .. } => {
            let lens = FastembedSparseLens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::FastembedBgem3 { .. } => {
            let lens = FastembedBgem3Lens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::FastembedReranker { .. } => {
            let lens = FastembedRerankerLens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::FastembedQwen3 { .. } => {
            let lens = FastembedQwen3Lens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::StaticLookup { .. } => {
            let lens = StaticLookupLens::from_lens_spec(spec)?;
            let contract = lens.contract().clone();
            Ok((Arc::new(lens), contract))
        }
        LensRuntime::MultimodalAdapter { .. } => {
            let lens = MultimodalAdapterLens::from_lens_spec(spec)?;
            let contract = lens.contract();
            Ok((Arc::new(lens), contract))
        }
    }
}

fn algorithmic_lens(spec: &LensSpec, kind: &str) -> Option<AlgorithmicLens> {
    let normalized = kind.replace('-', "_");
    let parts = normalized.split(':').collect::<Vec<_>>();
    let encoder = match parts.as_slice() {
        ["byte_features"] | ["byte"] => AlgorithmicEncoder::ByteFeatures,
        ["scalar"] => AlgorithmicEncoder::Scalar,
        ["ast_style"] => AlgorithmicEncoder::AstStyle,
        ["gdelt_cameo"] => AlgorithmicEncoder::GdeltCameo,
        ["gdelt_actor_geo"] => AlgorithmicEncoder::GdeltActorGeo {
            dim: sparse_dim(spec.output)?,
        },
        ["gdelt_source_domain"] => AlgorithmicEncoder::GdeltSourceDomain {
            dim: sparse_dim(spec.output)?,
        },
        ["gdelt_event_geo"] => AlgorithmicEncoder::GdeltEventGeo {
            dim: sparse_dim(spec.output)?,
        },
        ["gdelt_actor_pair"] => AlgorithmicEncoder::GdeltActorPair {
            dim: sparse_dim(spec.output)?,
        },
        ["gdelt_event_actor"] => AlgorithmicEncoder::GdeltEventActor {
            dim: sparse_dim(spec.output)?,
        },
        ["gdelt_tone_signal"] => AlgorithmicEncoder::GdeltToneSignal {
            dim: sparse_dim(spec.output)?,
        },
        ["gdelt_source_event"] => AlgorithmicEncoder::GdeltSourceEvent {
            dim: sparse_dim(spec.output)?,
        },
        ["sparse"] | ["sparse_keywords"] => AlgorithmicEncoder::SparseKeywords {
            dim: sparse_dim(spec.output)?,
        },
        ["sparse_keywords", dim] => AlgorithmicEncoder::SparseKeywords {
            dim: parse_u32(dim)?,
        },
        ["sparse_keywords_tf"] => AlgorithmicEncoder::SparseKeywordsTf {
            dim: sparse_dim(spec.output)?,
        },
        ["sparse_keywords_tf", dim] => AlgorithmicEncoder::SparseKeywordsTf {
            dim: parse_u32(dim)?,
        },
        ["token_hash"] | ["multi_hash"] => AlgorithmicEncoder::TokenHash {
            token_dim: token_dim(spec.output)?,
        },
        ["token_hash", token_dim] | ["multi_hash", token_dim] => AlgorithmicEncoder::TokenHash {
            token_dim: parse_u32(token_dim)?,
        },
        ["one_hot"] => AlgorithmicEncoder::OneHot {
            buckets: dense_dim(spec.output)?,
        },
        ["one_hot", buckets] => AlgorithmicEncoder::OneHot {
            buckets: parse_u32(buckets)?,
        },
        ["syn_cyclic_time", period] | ["syn_cyclical_time", period] => {
            AlgorithmicEncoder::SynCyclicTime {
                period: parse_u32(period)?,
            }
        }
        ["syn_scalar_raw"] => AlgorithmicEncoder::SynScalarRaw,
        ["syn_scalar_log1p"] => AlgorithmicEncoder::SynScalarLog1p,
        ["syn_scalar_zscore", mean_micros, std_micros] => AlgorithmicEncoder::SynScalarZScore {
            mean_micros: parse_i64(mean_micros)?,
            std_micros: parse_u64(std_micros)?,
        },
        ["syn_scalar_rank", min_micros, max_micros] => AlgorithmicEncoder::SynScalarRank {
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        },
        ["syn_scalar_rank_arc", min_micros, max_micros] => AlgorithmicEncoder::SynScalarRankArc {
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        },
        ["syn_one_hot"] | ["syn_onehot"] => AlgorithmicEncoder::SynOneHot {
            buckets: dense_dim(spec.output)?,
        },
        ["syn_one_hot", buckets] | ["syn_onehot", buckets] => AlgorithmicEncoder::SynOneHot {
            buckets: parse_u32(buckets)?,
        },
        ["syn_hash"] => AlgorithmicEncoder::SynHash {
            dim: sparse_dim(spec.output)?,
        },
        ["syn_hash", dim] => AlgorithmicEncoder::SynHash {
            dim: parse_u32(dim)?,
        },
        ["syn_sparse_text"] => AlgorithmicEncoder::SynSparseText {
            dim: sparse_dim(spec.output)?,
        },
        ["syn_sparse_text", dim] => AlgorithmicEncoder::SynSparseText {
            dim: parse_u32(dim)?,
        },
        ["syn_sparse_text_tf"] => AlgorithmicEncoder::SynSparseTextTf {
            dim: sparse_dim(spec.output)?,
        },
        ["syn_sparse_text_tf", dim] => AlgorithmicEncoder::SynSparseTextTf {
            dim: parse_u32(dim)?,
        },
        ["syn_token_slots"] => AlgorithmicEncoder::SynTokenSlots {
            token_dim: token_dim(spec.output)?,
        },
        ["syn_token_slots", token_dim] => AlgorithmicEncoder::SynTokenSlots {
            token_dim: parse_u32(token_dim)?,
        },
        ["syn_multi_hot"] | ["syn_multihot"] => AlgorithmicEncoder::SynMultiHot {
            dim: sparse_dim(spec.output)?,
        },
        ["syn_multi_hot", dim] | ["syn_multihot", dim] => AlgorithmicEncoder::SynMultiHot {
            dim: parse_u32(dim)?,
        },
        ["syn_record_vector"] => AlgorithmicEncoder::SynRecordVector {
            dim: dense_dim(spec.output)?,
        },
        ["syn_record_vector", dim] => AlgorithmicEncoder::SynRecordVector {
            dim: parse_u32(dim)?,
        },
        ["syn_bin", min_micros, max_micros] => AlgorithmicEncoder::SynBin {
            buckets: dense_dim(spec.output)?,
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        },
        ["syn_bin", buckets, min_micros, max_micros] => AlgorithmicEncoder::SynBin {
            buckets: parse_u32(buckets)?,
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        },
        ["syn_ordinal", levels] => AlgorithmicEncoder::SynOrdinal {
            levels: parse_u32(levels)?,
        },
        ["syn_frequency", count, total] => AlgorithmicEncoder::SynFrequency {
            count: parse_u64(count)?,
            total: parse_u64(total)?,
        },
        ["syn_target_mean", mean_micros, fold_count, outcome_hash] => {
            AlgorithmicEncoder::SynTargetMean {
                mean_micros: parse_i64(mean_micros)?,
                fold_count: parse_u32(fold_count)?,
                outcome_hash: parse_u32(outcome_hash)?,
            }
        }
        ["syn_delta", scale_micros] => AlgorithmicEncoder::SynDelta {
            scale_micros: parse_u64(scale_micros)?,
        },
        ["syn_rate", scale_micros] => AlgorithmicEncoder::SynRate {
            scale_micros: parse_u64(scale_micros)?,
        },
        ["syn_cross"] => AlgorithmicEncoder::SynCross {
            dim: sparse_dim(spec.output)?,
        },
        ["syn_cross", dim] => AlgorithmicEncoder::SynCross {
            dim: parse_u32(dim)?,
        },
        ["syn_aggregation"] => AlgorithmicEncoder::SynAggregation {
            dim: dense_dim(spec.output)?,
        },
        ["syn_aggregation", dim] => AlgorithmicEncoder::SynAggregation {
            dim: parse_u32(dim)?,
        },
        _ => return None,
    };
    Some(AlgorithmicLens::new(&spec.name, spec.modality, encoder))
}

fn dense_dim(shape: SlotShape) -> Option<u32> {
    match shape {
        SlotShape::Dense(dim) => Some(dim),
        SlotShape::Sparse(_) | SlotShape::Multi { .. } => None,
    }
}

fn sparse_dim(shape: SlotShape) -> Option<u32> {
    match shape {
        SlotShape::Sparse(dim) => Some(dim),
        SlotShape::Dense(_) | SlotShape::Multi { .. } => None,
    }
}

fn token_dim(shape: SlotShape) -> Option<u32> {
    match shape {
        SlotShape::Multi { token_dim } => Some(token_dim),
        SlotShape::Dense(_) | SlotShape::Sparse(_) => None,
    }
}

fn parse_u32(value: &str) -> Option<u32> {
    value.parse().ok()
}

fn parse_u64(value: &str) -> Option<u64> {
    value.parse().ok()
}

fn parse_i64(value: &str) -> Option<i64> {
    value.parse().ok()
}

fn lens_config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: "CALYX_LENS_CONFIG_INVALID",
        message: message.into(),
        remediation: "fix persisted LensSpec runtime fields or re-register the lens",
    }
}
