use std::fs;
use std::path::{Path, PathBuf};

use calyx_core::{Asymmetry, CalyxError, Result};

use crate::frozen::{NormPolicy, sha256_digest};
use crate::runtime::adapters::{allow_noncommercial_from_env, ensure_license_allowed};
use crate::runtime::common::DEFAULT_QWEN3_MAX_TOKENS;
use crate::spec::{Bgem3Engine, FastembedBgem3Output, LensRuntime, LensSpec};

use super::algorithmic_manifest::{
    algorithmic_kind, frozen_contract as algorithmic_frozen_contract, is_algorithmic_runtime,
};
use super::manifest::{LensForgeFile, LensForgeManifest};

const CONFIG_INVALID: &str = "CALYX_LENS_CONFIG_INVALID";

pub fn lens_spec_metadata_from_manifest_path(path: impl AsRef<Path>) -> Result<LensSpec> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|err| {
        config_invalid(format!(
            "read lensforge manifest {} failed: {err}",
            path.display()
        ))
    })?;
    let manifest: LensForgeManifest = serde_json::from_slice(&bytes).map_err(|err| {
        config_invalid(format!(
            "parse lensforge manifest {} failed: {err}",
            path.display()
        ))
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    lens_spec_metadata_from_manifest(&manifest, base)
}

pub fn lens_spec_metadata_from_manifest(
    manifest: &LensForgeManifest,
    base_dir: &Path,
) -> Result<LensSpec> {
    validate_required(manifest)?;
    ensure_license_allowed(
        manifest.license.as_deref(),
        manifest.non_commercial,
        allow_noncommercial_from_env(),
    )?;
    let output = manifest.output_shape()?;
    let algorithmic_contract =
        algorithmic_frozen_contract(&manifest.name, &manifest.runtime, manifest.modality, output)?;
    let max_tokens_hash = manifest
        .max_tokens
        .map(|value| value.to_string())
        .unwrap_or_default();
    let (output, weights_sha256, corpus_hash, norm_policy) =
        if let Some(contract) = algorithmic_contract {
            (
                contract.shape(),
                contract.weights_sha256(),
                contract.corpus_hash(),
                contract.norm_policy(),
            )
        } else {
            (
                output,
                metadata_weights_sha256(manifest)?,
                sha256_digest(&[
                    b"lensforge-manifest-v1",
                    manifest.name.as_bytes(),
                    manifest.source_hf_id.as_bytes(),
                    manifest.runtime.as_bytes(),
                    modality_token(manifest.modality).as_bytes(),
                    manifest.pooling.as_bytes(),
                    manifest.norm.as_bytes(),
                    max_tokens_hash.as_bytes(),
                ]),
                norm_policy(&manifest.norm)?,
            )
        };
    let retrieval_only = is_retrieval_only_runtime(&manifest.runtime);
    Ok(LensSpec {
        name: manifest.name.clone(),
        runtime: metadata_runtime_from_manifest(manifest, base_dir)?,
        output,
        modality: manifest.modality,
        weights_sha256,
        corpus_hash,
        norm_policy,
        max_batch: manifest.max_batch,
        axis: Some(manifest.name.clone()),
        asymmetry: Asymmetry::None,
        quant_default: manifest.quant_default,
        truncate_dim: manifest.truncate_dim,
        recall_delta: manifest.recall_delta,
        retrieval_only,
        excluded_from_dedup: retrieval_only,
    })
}

fn is_retrieval_only_runtime(runtime: &str) -> bool {
    matches!(runtime, "fastembed-reranker")
}

fn validate_required(manifest: &LensForgeManifest) -> Result<()> {
    if manifest.name.trim().is_empty() {
        return Err(config_invalid("lensforge manifest name is required"));
    }
    if manifest.source_hf_id.trim().is_empty() {
        return Err(config_invalid(
            "lensforge manifest source_hf_id is required",
        ));
    }
    if manifest.runtime.trim().is_empty() {
        return Err(config_invalid("lensforge manifest runtime is required"));
    }
    if is_tei_runtime(&manifest.runtime)
        && manifest
            .endpoint
            .as_deref()
            .is_none_or(|endpoint| endpoint.trim().is_empty())
    {
        return Err(config_invalid(
            "lensforge TEI manifest endpoint is required",
        ));
    }
    if manifest.dim == 0 {
        return Err(config_invalid("lensforge manifest dim must be > 0"));
    }
    let _ = manifest.output_shape()?;
    if let Some(max_batch) = manifest.max_batch
        && max_batch == 0
    {
        return Err(config_invalid("lensforge manifest max_batch must be > 0"));
    }
    if let Some(max_tokens) = manifest.max_tokens
        && max_tokens == 0
    {
        return Err(config_invalid("lensforge manifest max_tokens must be > 0"));
    }
    if let Some(truncate_dim) = manifest.truncate_dim
        && (truncate_dim == 0 || truncate_dim > manifest.dim)
    {
        return Err(config_invalid(format!(
            "truncate_dim {truncate_dim} must be in 1..={}",
            manifest.dim
        )));
    }
    if !manifest.recall_delta.is_finite() || manifest.recall_delta < 0.0 {
        return Err(config_invalid(
            "recall_delta must be finite and non-negative",
        ));
    }
    if manifest.files.is_empty() && !is_algorithmic_runtime(&manifest.runtime) {
        return Err(config_invalid("lensforge manifest files are required"));
    }
    Ok(())
}

fn metadata_weights_sha256(manifest: &LensForgeManifest) -> Result<[u8; 32]> {
    if is_algorithmic_runtime(&manifest.runtime) && manifest.files.is_empty() {
        return Ok(sha256_digest(&[
            b"lensforge-algorithmic-v1",
            manifest.name.as_bytes(),
            manifest.runtime.as_bytes(),
            &manifest.dim.to_be_bytes(),
            modality_token(manifest.modality).as_bytes(),
        ]));
    }
    parse_hex_32(
        manifest
            .artifact_set_sha256
            .as_deref()
            .unwrap_or(&manifest.weights_sha256),
    )
}

fn metadata_runtime_from_manifest(
    manifest: &LensForgeManifest,
    base_dir: &Path,
) -> Result<LensRuntime> {
    if let Some(kind) = algorithmic_kind(&manifest.runtime) {
        return Ok(LensRuntime::Algorithmic {
            kind: kind.to_string(),
        });
    }
    let files = ordered_manifest_files(&manifest.files)
        .into_iter()
        .map(|file| ManifestFileRef {
            role: file.role.clone(),
            path: resolve_manifest_path(base_dir, &file.path),
        })
        .collect::<Vec<_>>();
    let file_paths = files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    match manifest.runtime.as_str() {
        "onnx" | "onnx-int8" | "onnx-custom" | "onnx-fastembed" | "onnx-splade" => {
            Ok(LensRuntime::Onnx {
                model_id: manifest.source_hf_id.clone(),
                files: file_paths,
            })
        }
        "onnx-colbert" => Ok(LensRuntime::OnnxColbert {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
        }),
        "fastembed-sparse" => Ok(LensRuntime::FastembedSparse {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
        }),
        "fastembed-bgem3-dense" => Ok(LensRuntime::FastembedBgem3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            output: FastembedBgem3Output::Dense,
            engine: Bgem3Engine::FastembedCpu,
        }),
        "fastembed-bgem3-sparse" => Ok(LensRuntime::FastembedBgem3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            output: FastembedBgem3Output::Sparse,
            engine: Bgem3Engine::FastembedCpu,
        }),
        "fastembed-bgem3-colbert" => Ok(LensRuntime::FastembedBgem3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            output: FastembedBgem3Output::Colbert,
            engine: Bgem3Engine::FastembedCpu,
        }),
        "onnx-bgem3-dense" => Ok(LensRuntime::FastembedBgem3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            output: FastembedBgem3Output::Dense,
            engine: Bgem3Engine::OnnxCuda,
        }),
        "onnx-bgem3-sparse" => Ok(LensRuntime::FastembedBgem3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            output: FastembedBgem3Output::Sparse,
            engine: Bgem3Engine::OnnxCuda,
        }),
        "onnx-bgem3-colbert" => Ok(LensRuntime::FastembedBgem3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            output: FastembedBgem3Output::Colbert,
            engine: Bgem3Engine::OnnxCuda,
        }),
        "fastembed-reranker" => Ok(LensRuntime::FastembedReranker {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
        }),
        "fastembed-qwen3" => Ok(LensRuntime::FastembedQwen3 {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            dtype: manifest.dtype.clone(),
            max_tokens: manifest.max_tokens.unwrap_or(DEFAULT_QWEN3_MAX_TOKENS),
        }),
        "candle" | "candle-fp16" | "candle-local" => Ok(LensRuntime::CandleLocal {
            model_id: manifest.source_hf_id.clone(),
            files: file_paths,
            dtype: manifest.dtype.clone(),
            pooling: manifest.pooling.clone(),
        }),
        "tei" | "tei-http" | "tei_http" => Ok(LensRuntime::TeiHttp {
            endpoint: manifest
                .endpoint
                .clone()
                .ok_or_else(|| config_invalid("lensforge TEI endpoint is required"))?,
        }),
        "model2vec" | "static_lookup" | "static-lookup" => Ok(LensRuntime::StaticLookup {
            embeddings_file: file_by_role(&files, is_model_role)?,
            tokenizer: file_by_role(&files, |role| role == "tokenizer")?,
            dim: manifest.dim,
        }),
        "external-cmd" | "external_cmd" => Ok(LensRuntime::ExternalCmd {
            cmd: manifest.source_hf_id.clone(),
            args: file_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        }),
        "adapter" | "multimodal-adapter" | "multimodal_adapter" => {
            Ok(LensRuntime::MultimodalAdapter {
                axis: modality_token(manifest.modality).to_string(),
                model_id: manifest.source_hf_id.clone(),
                adapter_config: Some(file_by_role(&files, |role| role == "adapter")?),
                files: file_paths,
            })
        }
        "model2vec-external" => Ok(LensRuntime::ExternalCmd {
            cmd: "model2vec".to_string(),
            args: file_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        }),
        other => Err(config_invalid(format!(
            "unsupported lensforge runtime {other}"
        ))),
    }
}

#[derive(Clone, Debug)]
struct ManifestFileRef {
    role: String,
    path: PathBuf,
}

fn ordered_manifest_files(files: &[LensForgeFile]) -> Vec<&LensForgeFile> {
    let mut ordered = files.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|file| (role_rank(&file.role), file.path.clone()));
    ordered
}

fn role_rank(role: &str) -> u8 {
    match role {
        "model" | "weights" | "embeddings" => 0,
        "tokenizer" => 1,
        "config" => 2,
        "preprocessor" => 3,
        "tokenizer_config" => 4,
        "special_tokens_map" => 5,
        _ => 9,
    }
}

fn file_by_role(files: &[ManifestFileRef], predicate: impl Fn(&str) -> bool) -> Result<PathBuf> {
    files
        .iter()
        .find(|file| predicate(&file.role))
        .map(|file| file.path.clone())
        .ok_or_else(|| config_invalid("lensforge manifest missing static lookup artifact"))
}

fn is_model_role(role: &str) -> bool {
    matches!(role, "model" | "weights" | "embeddings")
}

fn is_tei_runtime(runtime: &str) -> bool {
    matches!(runtime, "tei" | "tei-http" | "tei_http")
}

fn resolve_manifest_path(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn norm_policy(raw: &str) -> Result<NormPolicy> {
    match raw {
        "l2" | "unit" => Ok(NormPolicy::unit()),
        "finite" => Ok(NormPolicy::Finite),
        "none" => Ok(NormPolicy::None),
        other => Err(config_invalid(format!(
            "unsupported lensforge norm {other}"
        ))),
    }
}

fn modality_token(modality: calyx_core::Modality) -> &'static str {
    match modality {
        calyx_core::Modality::Text => "text",
        calyx_core::Modality::Code => "code",
        calyx_core::Modality::Image => "image",
        calyx_core::Modality::Audio => "audio",
        calyx_core::Modality::Video => "video",
        calyx_core::Modality::Protein => "protein",
        calyx_core::Modality::Dna => "dna",
        calyx_core::Modality::Molecule => "molecule",
        calyx_core::Modality::Structured => "structured",
        calyx_core::Modality::Mixed => "mixed",
    }
}

fn parse_hex_32(raw: &str) -> Result<[u8; 32]> {
    let value = raw.trim();
    if value.len() != 64 {
        return Err(config_invalid(format!(
            "expected 64 hex chars, got {}",
            value.len()
        )));
    }
    let mut out = [0u8; 32];
    for (idx, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(chunk)
            .map_err(|err| config_invalid(format!("invalid hex utf8: {err}")))?;
        out[idx] = u8::from_str_radix(text, 16)
            .map_err(|err| config_invalid(format!("invalid hex digest: {err}")))?;
    }
    Ok(out)
}

fn config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CONFIG_INVALID,
        message: message.into(),
        remediation: "fix the lensforge manifest or regenerated artifacts",
    }
}
