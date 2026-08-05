use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, Modality, Result, SlotShape};
use serde_json::Value;

mod fastembed_contract;

use fastembed_contract::{
    fastembed_bgem3_contract, fastembed_reranker_contract, fastembed_sparse_contract,
};

use crate::frozen::{FrozenLensContract, LensDType, NormPolicy, sha256_digest};
use crate::runtime::candle::{self, CandlePoolingPolicy, CandlePrecision};
use crate::runtime::common::DEFAULT_MAX_TOKENS;
use crate::runtime::onnx::{custom_contract_corpus_hash, custom_pooling_from_config};
use crate::{
    AlgorithmicEncoder, AlgorithmicLens, LensRuntime, LensSpec, MultimodalAdapterLens,
    Qwen3ModelFiles,
};

const DEFAULT_COLBERT_ONNX: &str = "onnx/model_fp16.onnx";
const DEFAULT_QWEN3_MODEL: &str = "Qwen/Qwen3-Embedding-0.6B";
const STATIC_LOOKUP_MAGIC: &[u8; 8] = b"CXLKUP1\0";
const STATIC_LOOKUP_HEADER_LEN: usize = 24;

/// Derive the exact frozen runtime contract without constructing a model session.
///
/// This is intentionally public so content-addressed deployment records can
/// snapshot and later compare the runtime identity before allocating a GPU or
/// mutating a registry. Runtime-specific configuration files are still read and
/// validated; callers must treat an error as a hard deployment failure.
pub fn derive_runtime_contract_from_spec(spec: &LensSpec) -> Result<FrozenLensContract> {
    match &spec.runtime {
        LensRuntime::Algorithmic { kind } => algorithmic_contract(spec, kind),
        LensRuntime::TeiHttp { endpoint } => tei_contract(spec, endpoint),
        LensRuntime::ExternalCmd { cmd, args } => external_contract(spec, cmd, args),
        LensRuntime::CandleLocal {
            model_id,
            files,
            dtype,
            pooling,
        } => candle_contract(spec, model_id, files, dtype, pooling),
        LensRuntime::Onnx { model_id, files } => onnx_contract(spec, model_id, files),
        LensRuntime::OnnxColbert { model_id, files } => {
            onnx_colbert_contract(spec, model_id, files)
        }
        LensRuntime::FastembedSparse { model_id, files } => {
            fastembed_sparse_contract(spec, model_id, files)
        }
        LensRuntime::FastembedBgem3 {
            model_id,
            files,
            output,
            engine,
        } => fastembed_bgem3_contract(spec, model_id, files, *output, *engine),
        LensRuntime::FastembedReranker { model_id, files } => {
            fastembed_reranker_contract(spec, model_id, files)
        }
        LensRuntime::FastembedQwen3 {
            model_id,
            files,
            dtype,
            max_tokens,
        } => qwen3_contract(spec, model_id, files, dtype, *max_tokens),
        LensRuntime::StaticLookup {
            embeddings_file,
            tokenizer,
            dim,
        } => static_lookup_contract(spec, embeddings_file, tokenizer, *dim),
        LensRuntime::MultimodalAdapter { .. } => {
            Ok(MultimodalAdapterLens::from_lens_spec(spec)?.contract())
        }
    }
}

/// Whether a persisted lens can answer a free-text query, decided from its
/// persisted [`LensSpec`] alone — without constructing a runtime (issue #1896).
///
/// The search query gate runs against a registry rebuilt lazily from the vault's
/// persisted registry snapshot, so the lens objects it holds are thin
/// placeholders that have not loaded (and must not be forced to load) their
/// runtime just to answer "could this lens answer text?". The spec's runtime
/// declaration already names the exact encoder, so the answer is derivable from
/// bytes that are already on disk.
///
/// Non-algorithmic runtimes fall back to the historical modality gate, so this
/// preserves the previous behaviour for every model-backed text lens.
#[must_use]
pub fn spec_text_queryable(spec: &LensSpec) -> bool {
    if spec.modality == Modality::Text {
        return true;
    }
    match &spec.runtime {
        LensRuntime::Algorithmic { kind } => algorithmic_encoder(kind, spec.output)
            .is_some_and(AlgorithmicEncoder::accepts_free_text),
        _ => false,
    }
}

/// Whether a persisted lens can answer an exact whole-value query, decided from
/// its persisted [`LensSpec`] alone (#1899).
///
/// Answered without constructing a runtime, for the same reason
/// [`spec_text_queryable`] is: the gate runs against a registry rebuilt lazily
/// from the vault's persisted snapshot. Unlike text-queryability there is no
/// modality fallback — a modality tag says nothing about whether an encoder
/// content-addresses the whole value, so an undeclarable runtime is `false`.
#[must_use]
pub fn spec_exact_value_queryable(spec: &LensSpec) -> bool {
    match &spec.runtime {
        LensRuntime::Algorithmic { kind } => algorithmic_encoder(kind, spec.output)
            .is_some_and(AlgorithmicEncoder::accepts_exact_value),
        _ => false,
    }
}

fn algorithmic_contract(spec: &LensSpec, kind: &str) -> Result<FrozenLensContract> {
    let encoder = algorithmic_encoder(kind, spec.output).ok_or_else(|| {
        lens_config_invalid(format!(
            "unsupported algorithmic lens kind {kind} for persisted lens {}",
            spec.name
        ))
    })?;
    Ok(AlgorithmicLens::new(&spec.name, spec.modality, encoder)
        .contract()
        .clone())
}

/// Parses a persisted algorithmic runtime kind back into its encoder.
///
/// Public so a caller holding only a persisted [`LensSpec`] can ask the
/// encoder-level questions — notably
/// [`AlgorithmicEncoder::dense_cosine_grading`] (#1963) — about what the
/// **stored** declaration says, rather than about an in-memory object that may
/// not be what was written.
pub fn algorithmic_encoder(kind: &str, shape: SlotShape) -> Option<AlgorithmicEncoder> {
    let normalized = kind.replace('-', "_");
    let parts = normalized.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        ["byte_features"] | ["byte"] => Some(AlgorithmicEncoder::ByteFeatures),
        ["scalar"] => Some(AlgorithmicEncoder::Scalar),
        ["ast_style"] => Some(AlgorithmicEncoder::AstStyle),
        ["gdelt_cameo"] => Some(AlgorithmicEncoder::GdeltCameo),
        ["gdelt_actor_geo"] => Some(AlgorithmicEncoder::GdeltActorGeo {
            dim: sparse_dim(shape)?,
        }),
        ["gdelt_source_domain"] => Some(AlgorithmicEncoder::GdeltSourceDomain {
            dim: sparse_dim(shape)?,
        }),
        ["gdelt_event_geo"] => Some(AlgorithmicEncoder::GdeltEventGeo {
            dim: sparse_dim(shape)?,
        }),
        ["gdelt_actor_pair"] => Some(AlgorithmicEncoder::GdeltActorPair {
            dim: sparse_dim(shape)?,
        }),
        ["gdelt_event_actor"] => Some(AlgorithmicEncoder::GdeltEventActor {
            dim: sparse_dim(shape)?,
        }),
        ["gdelt_tone_signal"] => Some(AlgorithmicEncoder::GdeltToneSignal {
            dim: sparse_dim(shape)?,
        }),
        ["gdelt_source_event"] => Some(AlgorithmicEncoder::GdeltSourceEvent {
            dim: sparse_dim(shape)?,
        }),
        ["sparse"] | ["sparse_keywords"] => Some(AlgorithmicEncoder::SparseKeywords {
            dim: sparse_dim(shape)?,
        }),
        ["sparse_keywords", dim] => Some(AlgorithmicEncoder::SparseKeywords {
            dim: parse_u32(dim)?,
        }),
        ["sparse_keywords_tf"] => Some(AlgorithmicEncoder::SparseKeywordsTf {
            dim: sparse_dim(shape)?,
        }),
        ["sparse_keywords_tf", dim] => Some(AlgorithmicEncoder::SparseKeywordsTf {
            dim: parse_u32(dim)?,
        }),
        ["token_hash"] | ["multi_hash"] => Some(AlgorithmicEncoder::TokenHash {
            token_dim: token_dim(shape)?,
        }),
        ["token_hash", token_dim] | ["multi_hash", token_dim] => {
            Some(AlgorithmicEncoder::TokenHash {
                token_dim: parse_u32(token_dim)?,
            })
        }
        ["one_hot"] => Some(AlgorithmicEncoder::OneHot {
            buckets: dense_dim(shape)?,
        }),
        ["one_hot", buckets] => Some(AlgorithmicEncoder::OneHot {
            buckets: parse_u32(buckets)?,
        }),
        ["syn_cyclic_time", period] | ["syn_cyclical_time", period] => {
            Some(AlgorithmicEncoder::SynCyclicTime {
                period: parse_u32(period)?,
            })
        }
        ["syn_scalar_raw"] => Some(AlgorithmicEncoder::SynScalarRaw),
        ["syn_scalar_log1p"] => Some(AlgorithmicEncoder::SynScalarLog1p),
        ["syn_scalar_zscore", mean_micros, std_micros] => {
            Some(AlgorithmicEncoder::SynScalarZScore {
                mean_micros: parse_i64(mean_micros)?,
                std_micros: parse_u64(std_micros)?,
            })
        }
        ["syn_scalar_rank", min_micros, max_micros] => Some(AlgorithmicEncoder::SynScalarRank {
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        }),
        ["syn_scalar_rank_arc", min_micros, max_micros] => {
            Some(AlgorithmicEncoder::SynScalarRankArc {
                min_micros: parse_i64(min_micros)?,
                max_micros: parse_i64(max_micros)?,
            })
        }
        ["syn_one_hot"] | ["syn_onehot"] => Some(AlgorithmicEncoder::SynOneHot {
            buckets: dense_dim(shape)?,
        }),
        ["syn_one_hot", buckets] | ["syn_onehot", buckets] => Some(AlgorithmicEncoder::SynOneHot {
            buckets: parse_u32(buckets)?,
        }),
        ["syn_one_hot_index", levels] => Some(AlgorithmicEncoder::SynOneHotIndex {
            levels: parse_u32(levels)?,
        }),
        ["syn_hash"] => Some(AlgorithmicEncoder::SynHash {
            dim: sparse_dim(shape)?,
        }),
        ["syn_hash", dim] => Some(AlgorithmicEncoder::SynHash {
            dim: parse_u32(dim)?,
        }),
        ["syn_sparse_text"] => Some(AlgorithmicEncoder::SynSparseText {
            dim: sparse_dim(shape)?,
        }),
        ["syn_sparse_text", dim] => Some(AlgorithmicEncoder::SynSparseText {
            dim: parse_u32(dim)?,
        }),
        ["syn_sparse_text_tf"] => Some(AlgorithmicEncoder::SynSparseTextTf {
            dim: sparse_dim(shape)?,
        }),
        ["syn_sparse_text_tf", dim] => Some(AlgorithmicEncoder::SynSparseTextTf {
            dim: parse_u32(dim)?,
        }),
        ["syn_token_slots"] => Some(AlgorithmicEncoder::SynTokenSlots {
            token_dim: token_dim(shape)?,
        }),
        ["syn_token_slots", token_dim] => Some(AlgorithmicEncoder::SynTokenSlots {
            token_dim: parse_u32(token_dim)?,
        }),
        ["syn_multi_hot"] | ["syn_multihot"] => Some(AlgorithmicEncoder::SynMultiHot {
            dim: sparse_dim(shape)?,
        }),
        ["syn_multi_hot", dim] | ["syn_multihot", dim] => Some(AlgorithmicEncoder::SynMultiHot {
            dim: parse_u32(dim)?,
        }),
        ["syn_record_vector"] => Some(AlgorithmicEncoder::SynRecordVector {
            dim: dense_dim(shape)?,
        }),
        ["syn_record_vector", dim] => Some(AlgorithmicEncoder::SynRecordVector {
            dim: parse_u32(dim)?,
        }),
        ["syn_record_vector_unit_fields"] => Some(AlgorithmicEncoder::SynRecordVectorUnitFields {
            dim: dense_dim(shape)?,
        }),
        ["syn_record_vector_unit_fields", dim] => {
            Some(AlgorithmicEncoder::SynRecordVectorUnitFields {
                dim: parse_u32(dim)?,
            })
        }
        ["syn_bin", buckets, min_micros, max_micros] => Some(AlgorithmicEncoder::SynBin {
            buckets: parse_u32(buckets)?,
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        }),
        ["syn_bin", min_micros, max_micros] => Some(AlgorithmicEncoder::SynBin {
            buckets: dense_dim(shape)?,
            min_micros: parse_i64(min_micros)?,
            max_micros: parse_i64(max_micros)?,
        }),
        ["syn_ordinal", levels] => Some(AlgorithmicEncoder::SynOrdinal {
            levels: parse_u32(levels)?,
        }),
        ["syn_frequency", count, total] => Some(AlgorithmicEncoder::SynFrequency {
            count: parse_u64(count)?,
            total: parse_u64(total)?,
        }),
        ["syn_target_mean", mean_micros, fold_count, outcome_hash] => {
            Some(AlgorithmicEncoder::SynTargetMean {
                mean_micros: parse_i64(mean_micros)?,
                fold_count: parse_u32(fold_count)?,
                outcome_hash: parse_u32(outcome_hash)?,
            })
        }
        ["syn_delta", scale_micros] => Some(AlgorithmicEncoder::SynDelta {
            scale_micros: parse_u64(scale_micros)?,
        }),
        ["syn_rate", scale_micros] => Some(AlgorithmicEncoder::SynRate {
            scale_micros: parse_u64(scale_micros)?,
        }),
        ["syn_cross"] => Some(AlgorithmicEncoder::SynCross {
            dim: sparse_dim(shape)?,
        }),
        ["syn_cross", dim] => Some(AlgorithmicEncoder::SynCross {
            dim: parse_u32(dim)?,
        }),
        ["syn_aggregation"] => Some(AlgorithmicEncoder::SynAggregation {
            dim: dense_dim(shape)?,
        }),
        ["syn_aggregation", dim] => Some(AlgorithmicEncoder::SynAggregation {
            dim: parse_u32(dim)?,
        }),
        ["syn_graph_signature", snapshot] => Some(AlgorithmicEncoder::SynGraphSignature {
            snapshot: parse_u64(snapshot)?,
        }),
        ["syn_path_signature", snapshot] => Some(AlgorithmicEncoder::SynPathSignature {
            snapshot: parse_u64(snapshot)?,
        }),
        _ => None,
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

fn tei_contract(spec: &LensSpec, endpoint: &str) -> Result<FrozenLensContract> {
    let dim = dense_dim(spec.output).ok_or_else(|| {
        lens_config_invalid(format!(
            "TEI lens {} requires dense output shape, got {:?}",
            spec.name, spec.output
        ))
    })?;
    Ok(FrozenLensContract::tei_http(
        &spec.name,
        endpoint,
        spec.modality,
        dim,
    ))
}

fn external_contract(spec: &LensSpec, cmd: &str, args: &[String]) -> Result<FrozenLensContract> {
    let dim = dense_dim(spec.output).ok_or_else(|| {
        lens_config_invalid(format!(
            "external command lens {} requires dense output shape, got {:?}",
            spec.name, spec.output
        ))
    })?;
    let args_text = args.join("\0");
    Ok(FrozenLensContract::new(
        spec.name.clone(),
        sha256_digest(&[cmd.as_bytes(), args_text.as_bytes()]),
        sha256_digest(&[b"external-cmd-runtime-v1"]),
        SlotShape::Dense(dim),
        spec.modality,
        LensDType::F32,
        NormPolicy::None,
    ))
}

fn candle_contract(
    spec: &LensSpec,
    model_id: &str,
    files: &[PathBuf],
    dtype: &str,
    pooling: &str,
) -> Result<FrozenLensContract> {
    let [weights, tokenizer, config, ..] = files else {
        return Err(lens_config_invalid(
            "LensRuntime::CandleLocal requires weights, tokenizer, and config paths",
        ));
    };
    ensure_file("candle weights", weights)?;
    ensure_file("candle tokenizer", tokenizer)?;
    ensure_file("candle config", config)?;
    let dim = dense_hidden_size(config, "candle")?;
    let precision = CandlePrecision::parse(dtype)?;
    let pooling = CandlePoolingPolicy::parse(pooling)?;
    // Same formula and same warm-path device policy as CandleLens::from_lens_spec,
    // so the session-free derivation is byte-identical to the runtime contract.
    let finite_replay =
        candle::contract_finite_replay_precision(candle::LENS_SPEC_DEVICE_POLICY, precision);
    let corpus_hash = candle::contract_corpus_hash(
        model_id,
        DEFAULT_MAX_TOKENS,
        precision,
        pooling,
        spec.norm_policy,
        finite_replay,
    );
    Ok(FrozenLensContract::new(
        spec.name.clone(),
        spec.weights_sha256,
        corpus_hash,
        SlotShape::Dense(dim),
        Modality::Text,
        LensDType::F32,
        spec.norm_policy,
    ))
}

fn onnx_contract(spec: &LensSpec, model_id: &str, files: &[PathBuf]) -> Result<FrozenLensContract> {
    let [_model, _tokenizer, config, ..] = files else {
        return Err(lens_config_invalid(
            "LensRuntime::Onnx requires model, tokenizer, and config paths",
        ));
    };
    for path in files {
        ensure_file("ONNX contract artifact", path)?;
    }
    // Same pooling parser and same corpus-hash formula as the custom ONNX
    // runtime constructor. The sparse decision mirrors `output_from_session`:
    // a custom ONNX lens is sparse if and only if its declared output shape is
    // sparse (SPLADE-style lenses).
    let pooling = custom_pooling_from_config(config)?;
    let sparse = matches!(spec.output, SlotShape::Sparse(_));
    Ok(FrozenLensContract::new(
        spec.name.clone(),
        spec.weights_sha256,
        custom_contract_corpus_hash(model_id, sparse, pooling, spec.norm_policy),
        spec.output,
        spec.modality,
        LensDType::F32,
        spec.norm_policy,
    ))
}

fn onnx_colbert_contract(
    spec: &LensSpec,
    model_id: &str,
    files: &[PathBuf],
) -> Result<FrozenLensContract> {
    let [_model, _tokenizer, _config, ..] = files else {
        return Err(lens_config_invalid(
            "LensRuntime::OnnxColbert requires model, tokenizer, and config paths",
        ));
    };
    for path in files {
        ensure_file("ONNX ColBERT contract artifact", path)?;
    }
    Ok(FrozenLensContract::new(
        spec.name.clone(),
        spec.weights_sha256,
        sha256_digest(&[
            b"onnx-colbert-token-v1",
            model_id.as_bytes(),
            DEFAULT_COLBERT_ONNX.as_bytes(),
            b"attention-mask-unpooled-finite",
        ]),
        spec.output,
        Modality::Text,
        LensDType::F32,
        NormPolicy::Finite,
    ))
}

fn qwen3_contract(
    spec: &LensSpec,
    model_id: &str,
    files: &[PathBuf],
    dtype: &str,
    max_tokens: usize,
) -> Result<FrozenLensContract> {
    if max_tokens == 0 {
        return Err(lens_config_invalid(
            "fastembed-qwen3 max_tokens must be > 0",
        ));
    }
    let model_id = qwen3_model_id(model_id)?;
    let files = Qwen3ModelFiles::from_paths(model_id.clone(), files.to_vec())?;
    for path in files.artifact_paths() {
        ensure_file("fastembed-qwen3 contract artifact", &path)?;
    }
    let precision = CandlePrecision::parse(dtype)?;
    let max_tokens = max_tokens.to_string();
    let dim = dense_hidden_size(&files.config, "Qwen3")?;
    Ok(FrozenLensContract::new(
        spec.name.clone(),
        spec.weights_sha256,
        sha256_digest(&[
            b"fastembed-qwen3-text-v1",
            model_id.as_bytes(),
            precision.as_str().as_bytes(),
            max_tokens.as_bytes(),
            b"left-padding,last-token,l2",
        ]),
        SlotShape::Dense(dim),
        Modality::Text,
        LensDType::F32,
        NormPolicy::unit(),
    ))
}

fn static_lookup_contract(
    spec: &LensSpec,
    embeddings_file: &Path,
    tokenizer: &Path,
    expected_dim: u32,
) -> Result<FrozenLensContract> {
    let (dim, dtype) = static_lookup_header(embeddings_file)?;
    if dim != expected_dim {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "static lookup matrix dim {dim} != expected {expected_dim}"
        )));
    }
    ensure_file("static lookup tokenizer", tokenizer)?;
    Ok(FrozenLensContract::new(
        spec.name.clone(),
        spec.weights_sha256,
        sha256_digest(&[
            b"static-lookup-model2vec-v1",
            dim.to_string().as_bytes(),
            dtype.as_bytes(),
        ]),
        SlotShape::Dense(dim),
        Modality::Text,
        LensDType::F32,
        spec.norm_policy,
    ))
}

fn static_lookup_header(path: &Path) -> Result<(u32, &'static str)> {
    let mut file = File::open(path).map_err(|err| {
        lens_config_invalid(format!(
            "open static lookup matrix {} failed: {err}",
            path.display()
        ))
    })?;
    let len = file
        .metadata()
        .map_err(|err| {
            lens_config_invalid(format!(
                "stat static lookup matrix {} failed: {err}",
                path.display()
            ))
        })?
        .len() as usize;
    let mut header = [0_u8; STATIC_LOOKUP_HEADER_LEN];
    file.read_exact(&mut header).map_err(|err| {
        lens_config_invalid(format!(
            "read static lookup matrix header {} failed: {err}",
            path.display()
        ))
    })?;
    if len < STATIC_LOOKUP_HEADER_LEN || &header[..8] != STATIC_LOOKUP_MAGIC {
        return Err(lens_config_invalid(format!(
            "static lookup matrix {} has invalid magic/header",
            path.display()
        )));
    }
    let rows = u32::from_le_bytes(header[8..12].try_into().expect("rows"));
    let dim = u32::from_le_bytes(header[12..16].try_into().expect("dim"));
    let (dtype, width) = match header[16] {
        1 => ("int8", 1usize),
        2 => ("f16", 2usize),
        3 => ("f32", 4usize),
        other => {
            return Err(lens_config_invalid(format!(
                "unsupported static lookup dtype {other}"
            )));
        }
    };
    let expected = STATIC_LOOKUP_HEADER_LEN
        .checked_add(rows as usize * dim as usize * width)
        .ok_or_else(|| CalyxError::lens_dim_mismatch("static lookup matrix size overflow"))?;
    if len != expected {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "static lookup matrix byte length {len} != expected {expected}"
        )));
    }
    Ok((dim, dtype))
}

fn dense_hidden_size(path: &Path, label: &str) -> Result<u32> {
    let value = read_json(path, label)?;
    let hidden = value
        .get("hidden_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| lens_config_invalid(format!("{label} config missing hidden_size")))?;
    u32::try_from(hidden).map_err(|_| CalyxError::lens_dim_mismatch("hidden_size exceeds u32"))
}

fn read_json(path: &Path, label: &str) -> Result<Value> {
    let bytes = fs::read(path).map_err(|err| {
        lens_config_invalid(format!(
            "read {label} config {} failed: {err}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|err| lens_config_invalid(format!("parse {label} config failed: {err}")))
}

fn qwen3_model_id(raw: &str) -> Result<String> {
    match normalized(raw).as_str() {
        "qwen/qwen3-embedding-0.6b" | "qwen3-embedding-0.6b" | "qwen3-0.6b" => {
            Ok(DEFAULT_QWEN3_MODEL.to_string())
        }
        other => Err(CalyxError::lens_unreachable(format!(
            "unsupported fastembed-qwen3 model {other}; expected {DEFAULT_QWEN3_MODEL}"
        ))),
    }
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

pub(super) fn ensure_file(label: &str, path: &Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    Err(lens_config_invalid(format!(
        "{label} {} is missing",
        path.display()
    )))
}

fn normalized(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

pub(super) fn lens_config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: "CALYX_LENS_CONFIG_INVALID",
        message: message.into(),
        remediation: "fix persisted LensSpec runtime fields or re-register the lens",
    }
}
