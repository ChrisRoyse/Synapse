use calyx_core::{CalyxError, Modality, Result, SlotShape};

use crate::frozen::FrozenLensContract;
use crate::{AlgorithmicEncoder, AlgorithmicLens};

const CONFIG_INVALID: &str = "CALYX_LENS_CONFIG_INVALID";

pub(super) fn is_algorithmic_runtime(runtime: &str) -> bool {
    runtime == "algorithmic" || runtime.starts_with("algorithmic:")
}

pub(super) fn algorithmic_kind(runtime: &str) -> Option<&str> {
    if runtime == "algorithmic" {
        Some("byte-features")
    } else {
        runtime.strip_prefix("algorithmic:")
    }
}

pub(super) fn output_shape(runtime: &str, dim: u32) -> Result<SlotShape> {
    let Some(kind) = algorithmic_kind(runtime) else {
        return learned_output_shape(runtime, dim);
    };
    if let Some(shape) = syn_output_shape(kind, dim)? {
        return Ok(shape);
    }
    let shape = match kind {
        "byte" | "byte-features" => checked_dense(kind, dim, 16)?,
        "ast-style" | "ast_style" => checked_dense(kind, dim, 8)?,
        "gdelt-cameo" | "gdelt_cameo" => checked_dense(kind, dim, 16)?,
        "gdelt-actor-geo"
        | "gdelt_actor_geo"
        | "gdelt-source-domain"
        | "gdelt_source_domain"
        | "gdelt-event-geo"
        | "gdelt_event_geo"
        | "gdelt-actor-pair"
        | "gdelt_actor_pair"
        | "gdelt-event-actor"
        | "gdelt_event_actor"
        | "gdelt-tone-signal"
        | "gdelt_tone_signal"
        | "gdelt-source-event"
        | "gdelt_source_event" => SlotShape::Sparse(checked_positive(kind, dim)?),
        "scalar" => checked_dense(kind, dim, 1)?,
        "sparse" | "sparse-keywords" | "sparse_keywords" | "sparse-keywords-tf"
        | "sparse_keywords_tf" => SlotShape::Sparse(checked_positive(kind, dim)?),
        "token-hash" | "token_hash" | "multi-hash" | "multi_hash" => SlotShape::Multi {
            token_dim: checked_positive(kind, dim)?,
        },
        value if value.starts_with("one-hot:") || value.starts_with("one_hot:") => {
            checked_dense(kind, dim, parse_dim(value)?)?
        }
        value
            if value.starts_with("sparse-keywords:")
                || value.starts_with("sparse_keywords:")
                || value.starts_with("sparse-keywords-tf:")
                || value.starts_with("sparse_keywords_tf:") =>
        {
            let parsed = parse_dim(value)?;
            checked_match(kind, dim, parsed)?;
            SlotShape::Sparse(parsed)
        }
        value
            if value.starts_with("token-hash:")
                || value.starts_with("token_hash:")
                || value.starts_with("multi-hash:")
                || value.starts_with("multi_hash:") =>
        {
            let parsed = parse_dim(value)?;
            checked_match(kind, dim, parsed)?;
            SlotShape::Multi { token_dim: parsed }
        }
        other => {
            return Err(config_invalid(format!(
                "unsupported algorithmic lens kind {other}"
            )));
        }
    };
    Ok(shape)
}

pub(super) fn frozen_contract(
    name: &str,
    runtime: &str,
    modality: Modality,
    shape: SlotShape,
) -> Result<Option<FrozenLensContract>> {
    let Some(kind) = algorithmic_kind(runtime) else {
        return Ok(None);
    };
    let encoder = encoder_from_kind(kind, shape)?;
    Ok(Some(
        AlgorithmicLens::new(name, modality, encoder)
            .contract()
            .clone(),
    ))
}

fn encoder_from_kind(kind: &str, shape: SlotShape) -> Result<AlgorithmicEncoder> {
    if let Some(encoder) = syn_encoder_from_kind(kind, shape)? {
        return Ok(encoder);
    }
    let encoder = match kind {
        "byte" | "byte-features" | "byte_features" => AlgorithmicEncoder::ByteFeatures,
        "ast-style" | "ast_style" => AlgorithmicEncoder::AstStyle,
        "gdelt-cameo" | "gdelt_cameo" => AlgorithmicEncoder::GdeltCameo,
        "gdelt-actor-geo" | "gdelt_actor_geo" => AlgorithmicEncoder::GdeltActorGeo {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "gdelt-source-domain" | "gdelt_source_domain" => AlgorithmicEncoder::GdeltSourceDomain {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "gdelt-event-geo" | "gdelt_event_geo" => AlgorithmicEncoder::GdeltEventGeo {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "gdelt-actor-pair" | "gdelt_actor_pair" => AlgorithmicEncoder::GdeltActorPair {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "gdelt-event-actor" | "gdelt_event_actor" => AlgorithmicEncoder::GdeltEventActor {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "gdelt-tone-signal" | "gdelt_tone_signal" => AlgorithmicEncoder::GdeltToneSignal {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "gdelt-source-event" | "gdelt_source_event" => AlgorithmicEncoder::GdeltSourceEvent {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "scalar" => AlgorithmicEncoder::Scalar,
        "sparse" | "sparse-keywords" | "sparse_keywords" => AlgorithmicEncoder::SparseKeywords {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "sparse-keywords-tf" | "sparse_keywords_tf" => AlgorithmicEncoder::SparseKeywordsTf {
            dim: sparse_shape_dim(kind, shape)?,
        },
        "token-hash" | "token_hash" | "multi-hash" | "multi_hash" => {
            AlgorithmicEncoder::TokenHash {
                token_dim: multi_shape_dim(kind, shape)?,
            }
        }
        value if value.starts_with("one-hot:") || value.starts_with("one_hot:") => {
            AlgorithmicEncoder::OneHot {
                buckets: dense_shape_dim(kind, shape)?,
            }
        }
        value
            if value.starts_with("sparse-keywords-tf:")
                || value.starts_with("sparse_keywords_tf:") =>
        {
            AlgorithmicEncoder::SparseKeywordsTf {
                dim: sparse_shape_dim(kind, shape)?,
            }
        }
        value if value.starts_with("sparse-keywords:") || value.starts_with("sparse_keywords:") => {
            AlgorithmicEncoder::SparseKeywords {
                dim: sparse_shape_dim(kind, shape)?,
            }
        }
        value
            if value.starts_with("token-hash:")
                || value.starts_with("token_hash:")
                || value.starts_with("multi-hash:")
                || value.starts_with("multi_hash:") =>
        {
            AlgorithmicEncoder::TokenHash {
                token_dim: multi_shape_dim(kind, shape)?,
            }
        }
        other => {
            return Err(config_invalid(format!(
                "unsupported algorithmic lens kind {other}"
            )));
        }
    };
    Ok(encoder)
}

fn syn_output_shape(kind: &str, dim: u32) -> Result<Option<SlotShape>> {
    let normalized = kind.replace('-', "_");
    let parts = normalized.split(':').collect::<Vec<_>>();
    let shape = match parts.as_slice() {
        ["syn_cyclic_time", _] | ["syn_cyclical_time", _] => checked_dense(kind, dim, 2)?,
        ["syn_scalar_raw"]
        | ["syn_scalar_log1p"]
        | ["syn_scalar_zscore", _, _]
        | ["syn_scalar_rank", _, _]
        | ["syn_ordinal", _]
        | ["syn_frequency", _, _]
        | ["syn_target_mean", _, _, _]
        | ["syn_delta", _]
        | ["syn_rate", _] => checked_dense(kind, dim, 1)?,
        ["syn_one_hot"] | ["syn_onehot"] => SlotShape::Dense(checked_positive(kind, dim)?),
        ["syn_one_hot", buckets] | ["syn_onehot", buckets] => {
            let buckets = parse_u32_value(kind, buckets)?;
            checked_dense(kind, dim, buckets)?
        }
        ["syn_hash"]
        | ["syn_sparse_text"]
        | ["syn_sparse_text_tf"]
        | ["syn_multi_hot"]
        | ["syn_multihot"]
        | ["syn_cross"] => SlotShape::Sparse(checked_power_of_two(kind, dim)?),
        ["syn_hash", parsed]
        | ["syn_sparse_text", parsed]
        | ["syn_sparse_text_tf", parsed]
        | ["syn_multi_hot", parsed]
        | ["syn_multihot", parsed]
        | ["syn_cross", parsed] => {
            let parsed = parse_u32_value(kind, parsed)?;
            checked_match(kind, dim, parsed)?;
            SlotShape::Sparse(checked_power_of_two(kind, parsed)?)
        }
        ["syn_token_slots"] => SlotShape::Multi {
            token_dim: checked_power_of_two(kind, dim)?,
        },
        ["syn_token_slots", parsed] => {
            let parsed = parse_u32_value(kind, parsed)?;
            checked_match(kind, dim, parsed)?;
            SlotShape::Multi {
                token_dim: checked_power_of_two(kind, parsed)?,
            }
        }
        ["syn_record_vector"] | ["syn_aggregation"] => {
            SlotShape::Dense(checked_positive(kind, dim)?)
        }
        ["syn_record_vector", parsed] | ["syn_aggregation", parsed] => {
            let parsed = parse_u32_value(kind, parsed)?;
            checked_match(kind, dim, parsed)?;
            SlotShape::Dense(checked_positive(kind, parsed)?)
        }
        ["syn_bin", min_micros, max_micros] => {
            parse_i64_value(kind, min_micros)?;
            parse_i64_value(kind, max_micros)?;
            SlotShape::Dense(checked_positive(kind, dim)?)
        }
        ["syn_bin", buckets, min_micros, max_micros] => {
            parse_i64_value(kind, min_micros)?;
            parse_i64_value(kind, max_micros)?;
            let buckets = parse_u32_value(kind, buckets)?;
            checked_dense(kind, dim, buckets)?
        }
        _ => return Ok(None),
    };
    Ok(Some(shape))
}

fn syn_encoder_from_kind(kind: &str, shape: SlotShape) -> Result<Option<AlgorithmicEncoder>> {
    let normalized = kind.replace('-', "_");
    let parts = normalized.split(':').collect::<Vec<_>>();
    let encoder = match parts.as_slice() {
        ["syn_cyclic_time", period] | ["syn_cyclical_time", period] => {
            AlgorithmicEncoder::SynCyclicTime {
                period: parse_u32_value(kind, period)?,
            }
        }
        ["syn_scalar_raw"] => AlgorithmicEncoder::SynScalarRaw,
        ["syn_scalar_log1p"] => AlgorithmicEncoder::SynScalarLog1p,
        ["syn_scalar_zscore", mean_micros, std_micros] => AlgorithmicEncoder::SynScalarZScore {
            mean_micros: parse_i64_value(kind, mean_micros)?,
            std_micros: parse_u64_value(kind, std_micros)?,
        },
        ["syn_scalar_rank", min_micros, max_micros] => AlgorithmicEncoder::SynScalarRank {
            min_micros: parse_i64_value(kind, min_micros)?,
            max_micros: parse_i64_value(kind, max_micros)?,
        },
        ["syn_one_hot"] | ["syn_onehot"] => AlgorithmicEncoder::SynOneHot {
            buckets: dense_shape_dim(kind, shape)?,
        },
        ["syn_one_hot", buckets] | ["syn_onehot", buckets] => AlgorithmicEncoder::SynOneHot {
            buckets: parse_u32_value(kind, buckets)?,
        },
        ["syn_hash"] => AlgorithmicEncoder::SynHash {
            dim: sparse_shape_dim(kind, shape)?,
        },
        ["syn_hash", dim] => AlgorithmicEncoder::SynHash {
            dim: parse_u32_value(kind, dim)?,
        },
        ["syn_sparse_text"] => AlgorithmicEncoder::SynSparseText {
            dim: sparse_shape_dim(kind, shape)?,
        },
        ["syn_sparse_text", dim] => AlgorithmicEncoder::SynSparseText {
            dim: parse_u32_value(kind, dim)?,
        },
        ["syn_sparse_text_tf"] => AlgorithmicEncoder::SynSparseTextTf {
            dim: sparse_shape_dim(kind, shape)?,
        },
        ["syn_sparse_text_tf", dim] => AlgorithmicEncoder::SynSparseTextTf {
            dim: parse_u32_value(kind, dim)?,
        },
        ["syn_token_slots"] => AlgorithmicEncoder::SynTokenSlots {
            token_dim: multi_shape_dim(kind, shape)?,
        },
        ["syn_token_slots", token_dim] => AlgorithmicEncoder::SynTokenSlots {
            token_dim: parse_u32_value(kind, token_dim)?,
        },
        ["syn_multi_hot"] | ["syn_multihot"] => AlgorithmicEncoder::SynMultiHot {
            dim: sparse_shape_dim(kind, shape)?,
        },
        ["syn_multi_hot", dim] | ["syn_multihot", dim] => AlgorithmicEncoder::SynMultiHot {
            dim: parse_u32_value(kind, dim)?,
        },
        ["syn_record_vector"] => AlgorithmicEncoder::SynRecordVector {
            dim: dense_shape_dim(kind, shape)?,
        },
        ["syn_record_vector", dim] => AlgorithmicEncoder::SynRecordVector {
            dim: parse_u32_value(kind, dim)?,
        },
        ["syn_bin", min_micros, max_micros] => AlgorithmicEncoder::SynBin {
            buckets: dense_shape_dim(kind, shape)?,
            min_micros: parse_i64_value(kind, min_micros)?,
            max_micros: parse_i64_value(kind, max_micros)?,
        },
        ["syn_bin", buckets, min_micros, max_micros] => AlgorithmicEncoder::SynBin {
            buckets: parse_u32_value(kind, buckets)?,
            min_micros: parse_i64_value(kind, min_micros)?,
            max_micros: parse_i64_value(kind, max_micros)?,
        },
        ["syn_ordinal", levels] => AlgorithmicEncoder::SynOrdinal {
            levels: parse_u32_value(kind, levels)?,
        },
        ["syn_frequency", count, total] => AlgorithmicEncoder::SynFrequency {
            count: parse_u64_value(kind, count)?,
            total: parse_u64_value(kind, total)?,
        },
        ["syn_target_mean", mean_micros, fold_count, outcome_hash] => {
            AlgorithmicEncoder::SynTargetMean {
                mean_micros: parse_i64_value(kind, mean_micros)?,
                fold_count: parse_u32_value(kind, fold_count)?,
                outcome_hash: parse_u32_value(kind, outcome_hash)?,
            }
        }
        ["syn_delta", scale_micros] => AlgorithmicEncoder::SynDelta {
            scale_micros: parse_u64_value(kind, scale_micros)?,
        },
        ["syn_rate", scale_micros] => AlgorithmicEncoder::SynRate {
            scale_micros: parse_u64_value(kind, scale_micros)?,
        },
        ["syn_cross"] => AlgorithmicEncoder::SynCross {
            dim: sparse_shape_dim(kind, shape)?,
        },
        ["syn_cross", dim] => AlgorithmicEncoder::SynCross {
            dim: parse_u32_value(kind, dim)?,
        },
        ["syn_aggregation"] => AlgorithmicEncoder::SynAggregation {
            dim: dense_shape_dim(kind, shape)?,
        },
        ["syn_aggregation", dim] => AlgorithmicEncoder::SynAggregation {
            dim: parse_u32_value(kind, dim)?,
        },
        _ => return Ok(None),
    };
    Ok(Some(encoder))
}

fn dense_shape_dim(kind: &str, shape: SlotShape) -> Result<u32> {
    match shape {
        SlotShape::Dense(dim) => Ok(dim),
        other => Err(config_invalid(format!(
            "algorithmic lens {kind} requires dense shape, got {other:?}"
        ))),
    }
}

fn sparse_shape_dim(kind: &str, shape: SlotShape) -> Result<u32> {
    match shape {
        SlotShape::Sparse(dim) => Ok(dim),
        other => Err(config_invalid(format!(
            "algorithmic lens {kind} requires sparse shape, got {other:?}"
        ))),
    }
}

fn multi_shape_dim(kind: &str, shape: SlotShape) -> Result<u32> {
    match shape {
        SlotShape::Multi { token_dim } => Ok(token_dim),
        other => Err(config_invalid(format!(
            "algorithmic lens {kind} requires multi shape, got {other:?}"
        ))),
    }
}

fn learned_output_shape(runtime: &str, dim: u32) -> Result<SlotShape> {
    match runtime {
        "fastembed-sparse" | "fastembed-bgem3-sparse" | "onnx-bgem3-sparse" | "onnx-splade" => {
            Ok(SlotShape::Sparse(checked_positive(runtime, dim)?))
        }
        "fastembed-bgem3-colbert" | "onnx-bgem3-colbert" | "onnx-colbert" => Ok(SlotShape::Multi {
            token_dim: checked_positive(runtime, dim)?,
        }),
        _ => Ok(SlotShape::Dense(dim)),
    }
}

fn checked_dense(kind: &str, got: u32, expected: u32) -> Result<SlotShape> {
    checked_match(kind, got, expected)?;
    Ok(SlotShape::Dense(expected))
}

fn checked_match(kind: &str, got: u32, expected: u32) -> Result<()> {
    if got == expected {
        return Ok(());
    }
    Err(config_invalid(format!(
        "algorithmic lens {kind} dim {got} != expected {expected}"
    )))
}

fn checked_positive(kind: &str, dim: u32) -> Result<u32> {
    if dim > 0 {
        return Ok(dim);
    }
    Err(config_invalid(format!(
        "algorithmic lens {kind} dim must be greater than zero"
    )))
}

fn checked_power_of_two(kind: &str, dim: u32) -> Result<u32> {
    let dim = checked_positive(kind, dim)?;
    if dim.is_power_of_two() {
        return Ok(dim);
    }
    Err(config_invalid(format!(
        "algorithmic lens {kind} dim must be a power of two"
    )))
}

fn parse_dim(kind: &str) -> Result<u32> {
    kind.split_once(':')
        .and_then(|(_, dim)| dim.parse::<u32>().ok())
        .filter(|dim| *dim > 0)
        .ok_or_else(|| config_invalid(format!("invalid algorithmic dim in {kind}")))
}

fn parse_u32_value(kind: &str, value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .map_err(|_| config_invalid(format!("invalid u32 parameter {value} in {kind}")))
}

fn parse_u64_value(kind: &str, value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| config_invalid(format!("invalid u64 parameter {value} in {kind}")))
}

fn parse_i64_value(kind: &str, value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .map_err(|_| config_invalid(format!("invalid i64 parameter {value} in {kind}")))
}

fn config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CONFIG_INVALID,
        message: message.into(),
        remediation: "fix the lensforge manifest or regenerated artifacts",
    }
}
