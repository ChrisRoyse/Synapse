use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Mutex;

use calyx_core::{CalyxError, Input, Lens, LensId, Modality, Result, SlotShape, SlotVector};
use fastembed::{EmbeddingModel, TextEmbedding};
use serde::{Deserialize, Serialize};

use crate::frozen::{FrozenLensContract, NormPolicy};
use crate::runtime::common::DEFAULT_MAX_TOKENS;
use crate::runtime::common::{normalize_unit, text_from_input};
use crate::spec::{LensRuntime, LensSpec};

mod arena;
mod colbert;
mod colbert_files;
mod colbert_tokens;
mod cpu_fallback_audit;
mod cuda_graphs;
mod cuda_guard;
mod custom;
#[cfg(feature = "cuda")]
mod device_postprocess;
mod dynamic_ort;
mod fastembed_runtime;
mod green_context;
mod io_binding;
mod session;
mod special;
mod windows_cuda_dlls;

pub(in crate::runtime::onnx) use crate::runtime::batch_scope::scoped_max_batch;
pub use colbert::{DEFAULT_ANSWERAI_COLBERT_MODEL, OnnxColbertFileSpec, OnnxColbertLens};
pub(crate) use custom::{
    contract_corpus_hash as custom_contract_corpus_hash,
    pooling_from_config as custom_pooling_from_config,
};
pub use special::{
    Bgem3RuntimeStats, FastembedBgem3Lens, FastembedRerankerLens, FastembedSparseLens,
};

/// Configured and required ONNX `(batch, sequence)` shape-domain budget for a
/// resident runtime. The required count is derived from the same stable bucket
/// functions used to build tensors, preventing admission checks from drifting
/// away from the production batching path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnnxShapeBucketBudget {
    pub configured_shape_limit: usize,
    pub required_shape_count: usize,
    pub sequence_bucket_count: usize,
    pub batch_bucket_count: usize,
    pub max_sequence_tokens: usize,
    pub max_runtime_batch: usize,
}

pub fn onnx_shape_bucket_budget(max_runtime_batch: usize) -> Result<OnnxShapeBucketBudget> {
    let sequence_bucket_count = custom::batch::stable_bucket_count(DEFAULT_MAX_TOKENS)?;
    let batch_bucket_count = custom::batch::stable_bucket_count(max_runtime_batch)?;
    let required_shape_count = sequence_bucket_count
        .checked_mul(batch_bucket_count)
        .ok_or_else(|| CalyxError {
            code: "CALYX_ONNX_SHAPE_BUDGET_OVERFLOW",
            message: format!(
                "ONNX stable shape budget overflowed for {sequence_bucket_count} sequence buckets and {batch_bucket_count} batch buckets"
            ),
            remediation: "lower the configured runtime batch maximum to a representable positive value",
        })?;
    let configured_shape_limit = arena::configured_max_distinct_shapes()?;
    if configured_shape_limit < required_shape_count {
        return Err(CalyxError {
            code: "CALYX_ONNX_SHAPE_LIMIT_BELOW_BUCKET_DOMAIN",
            message: format!(
                "CALYX_ONNX_MAX_DISTINCT_SHAPES={configured_shape_limit} cannot cover the stable ONNX bucket domain: required={required_shape_count} sequence_buckets={sequence_bucket_count} batch_buckets={batch_bucket_count} max_sequence_tokens={DEFAULT_MAX_TOKENS} max_runtime_batch={max_runtime_batch}"
            ),
            remediation: "set CALYX_ONNX_MAX_DISTINCT_SHAPES to at least the reported required count (default 64), then restart; do not weaken sequence or batch bucketing",
        });
    }
    Ok(OnnxShapeBucketBudget {
        configured_shape_limit,
        required_shape_count,
        sequence_bucket_count,
        batch_bucket_count,
        max_sequence_tokens: DEFAULT_MAX_TOKENS,
        max_runtime_batch,
    })
}

pub struct OnnxLens {
    id: LensId,
    dim: u32,
    contract: FrozenLensContract,
    files: OnnxModelFiles,
    provider_policy: OnnxProviderPolicy,
    max_batch: Option<usize>,
    backend: Option<OnnxBackend>,
}

enum OnnxBackend {
    FastEmbed(Box<Mutex<TextEmbedding>>),
    Custom(Box<Mutex<custom::CustomOnnxRuntime>>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnnxProviderPolicy {
    CudaFailLoud,
    CpuExplicit,
}

impl OnnxProviderPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CudaFailLoud => {
                "cuda:0,error_on_failure,no_cpu_fallback,cudnn_conv_algo=HEURISTIC,cudnn_workspace_cap=32MiB"
            }
            Self::CpuExplicit => "cpu_explicit,no_cuda",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolingPolicy {
    Mean,
    Cls,
    LastToken,
}

impl PoolingPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::Cls => "cls",
            Self::LastToken => "last_token",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnnxModelFiles {
    pub cache_dir: PathBuf,
    pub model_code: String,
    pub model_file: PathBuf,
    pub tokenizer: PathBuf,
    pub config: PathBuf,
    pub special_tokens_map: PathBuf,
    pub tokenizer_config: PathBuf,
    pub contract_paths: Vec<PathBuf>,
}

impl OnnxModelFiles {
    pub fn artifact_paths(&self) -> Vec<PathBuf> {
        if !self.contract_paths.is_empty() {
            return self.contract_paths.clone();
        }
        let mut paths = vec![
            self.model_file.clone(),
            self.tokenizer.clone(),
            self.config.clone(),
        ];
        if !paths.contains(&self.tokenizer_config) {
            paths.push(self.tokenizer_config.clone());
        }
        if !paths.contains(&self.special_tokens_map) {
            paths.push(self.special_tokens_map.clone());
        }
        paths
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OnnxFileSpec {
    pub name: String,
    pub model_id: String,
    pub model_file: PathBuf,
    pub tokenizer: PathBuf,
    pub config: PathBuf,
    pub modality: Modality,
    pub pooling: PoolingPolicy,
    pub norm_policy: NormPolicy,
    pub max_batch: Option<usize>,
    pub provider_policy: OnnxProviderPolicy,
    pub expected_shape: Option<SlotShape>,
    pub expected_weights_sha256: Option<[u8; 32]>,
    pub contract_paths: Vec<PathBuf>,
}

impl OnnxFileSpec {
    pub fn text(
        name: impl Into<String>,
        model_id: impl Into<String>,
        model_file: impl Into<PathBuf>,
        tokenizer: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        pooling: PoolingPolicy,
        norm_policy: NormPolicy,
    ) -> Self {
        Self {
            name: name.into(),
            model_id: model_id.into(),
            model_file: model_file.into(),
            tokenizer: tokenizer.into(),
            config: config.into(),
            modality: Modality::Text,
            pooling,
            norm_policy,
            max_batch: None,
            provider_policy: OnnxProviderPolicy::CudaFailLoud,
            expected_shape: None,
            expected_weights_sha256: None,
            contract_paths: Vec::new(),
        }
    }

    pub fn from_lens_spec(spec: &LensSpec) -> Result<Self> {
        let LensRuntime::Onnx { model_id, files } = &spec.runtime else {
            return Err(config_invalid("LensSpec runtime is not onnx"));
        };
        let [model_file, tokenizer, config, ..] = files.as_slice() else {
            return Err(config_invalid(
                "LensRuntime::Onnx requires model, tokenizer, and config paths",
            ));
        };
        let pooling = custom::pooling_from_config(config)?;
        if spec.max_batch == Some(0) {
            return Err(config_invalid("LensSpec max_batch must be > 0"));
        }
        Ok(Self {
            name: spec.name.clone(),
            model_id: model_id.clone(),
            model_file: model_file.clone(),
            tokenizer: tokenizer.clone(),
            config: config.clone(),
            modality: spec.modality,
            pooling,
            norm_policy: spec.norm_policy,
            max_batch: spec.max_batch,
            provider_policy: OnnxProviderPolicy::CudaFailLoud,
            expected_shape: Some(spec.output),
            expected_weights_sha256: Some(spec.weights_sha256),
            contract_paths: files.clone(),
        })
    }

    pub fn with_provider_policy(mut self, policy: OnnxProviderPolicy) -> Self {
        self.provider_policy = policy;
        self
    }

    pub fn with_expected_shape(mut self, shape: SlotShape) -> Self {
        self.expected_shape = Some(shape);
        self
    }

    pub fn with_expected_weights_sha256(mut self, hash: [u8; 32]) -> Self {
        self.expected_weights_sha256 = Some(hash);
        self
    }

    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = Some(max_batch.max(1));
        self
    }
}

impl OnnxLens {
    pub fn all_minilm_l6_v2(name: impl Into<String>) -> Result<Self> {
        fastembed_runtime::from_hf_cache(name, fastembed_runtime::default_cache_root())
    }

    pub fn all_minilm_l6_v2_cpu_explicit(name: impl Into<String>) -> Result<Self> {
        fastembed_runtime::from_hf_cache_with_policy(
            name,
            fastembed_runtime::default_cache_root(),
            OnnxProviderPolicy::CpuExplicit,
        )
    }

    pub fn from_hf_cache(name: impl Into<String>, cache_dir: impl Into<PathBuf>) -> Result<Self> {
        fastembed_runtime::from_hf_cache(name, cache_dir.into())
    }

    pub fn from_hf_cache_with_policy(
        name: impl Into<String>,
        cache_dir: impl Into<PathBuf>,
        provider_policy: OnnxProviderPolicy,
    ) -> Result<Self> {
        fastembed_runtime::from_hf_cache_with_policy(name, cache_dir.into(), provider_policy)
    }

    pub fn from_model(
        name: impl Into<String>,
        model_name: EmbeddingModel,
        cache_dir: PathBuf,
    ) -> Result<Self> {
        fastembed_runtime::from_model_with_policy(
            name,
            model_name,
            cache_dir,
            OnnxProviderPolicy::CudaFailLoud,
        )
    }

    pub fn from_model_with_policy(
        name: impl Into<String>,
        model_name: EmbeddingModel,
        cache_dir: PathBuf,
        provider_policy: OnnxProviderPolicy,
    ) -> Result<Self> {
        fastembed_runtime::from_model_with_policy(name, model_name, cache_dir, provider_policy)
    }

    pub fn from_model_name_with_policy(
        name: impl Into<String>,
        model_name: &str,
        cache_dir: PathBuf,
        provider_policy: OnnxProviderPolicy,
    ) -> Result<Self> {
        fastembed_runtime::from_model_name_with_policy(name, model_name, cache_dir, provider_policy)
    }

    pub fn from_files(spec: OnnxFileSpec) -> Result<Self> {
        custom::from_files(spec)
    }

    pub fn from_lens_spec(spec: &LensSpec) -> Result<Self> {
        let LensRuntime::Onnx { model_id: _, files } = &spec.runtime else {
            return Err(config_invalid("LensSpec runtime is not onnx"));
        };
        if is_fastembed_manifest(files) {
            return Self::from_files(OnnxFileSpec::from_lens_spec(spec)?);
        }
        Self::from_files(OnnxFileSpec::from_lens_spec(spec)?)
    }

    pub(crate) fn from_fastembed_parts(
        id: LensId,
        dim: u32,
        contract: FrozenLensContract,
        files: OnnxModelFiles,
        provider_policy: OnnxProviderPolicy,
        model: TextEmbedding,
    ) -> Self {
        Self {
            id,
            dim,
            contract,
            files,
            provider_policy,
            max_batch: None,
            backend: Some(OnnxBackend::FastEmbed(Box::new(Mutex::new(model)))),
        }
    }

    pub(crate) fn from_custom_parts(
        contract: FrozenLensContract,
        files: OnnxModelFiles,
        provider_policy: OnnxProviderPolicy,
        max_batch: Option<usize>,
        runtime: custom::CustomOnnxRuntime,
    ) -> Self {
        Self {
            id: contract.lens_id(),
            dim: runtime.dim(),
            contract,
            files,
            provider_policy,
            max_batch,
            backend: Some(OnnxBackend::Custom(Box::new(Mutex::new(runtime)))),
        }
    }

    pub fn contract(&self) -> &FrozenLensContract {
        &self.contract
    }

    pub fn files(&self) -> &OnnxModelFiles {
        &self.files
    }

    pub fn provider_policy(&self) -> &'static str {
        self.provider_policy.as_str()
    }

    pub fn runtime_name(&self) -> &'static str {
        match self.backend_ref() {
            OnnxBackend::FastEmbed(_) => "onnx-fastembed",
            OnnxBackend::Custom(_) => "onnx-custom",
        }
    }

    pub fn lens_spec(&self) -> LensSpec {
        LensSpec {
            name: self.contract.name().to_string(),
            runtime: LensRuntime::Onnx {
                model_id: self.files.model_code.clone(),
                files: self.files.artifact_paths(),
            },
            output: self.contract.shape(),
            modality: self.contract.modality(),
            weights_sha256: self.contract.weights_sha256(),
            corpus_hash: self.contract.corpus_hash(),
            norm_policy: self.contract.norm_policy(),
            max_batch: self.max_batch,
            axis: None,
            asymmetry: calyx_core::Asymmetry::None,
            quant_default: calyx_core::QuantPolicy::turboquant_default(),
            truncate_dim: None,
            recall_delta: crate::spec::default_recall_delta(),
            retrieval_only: false,
            excluded_from_dedup: false,
        }
    }
}

fn is_fastembed_manifest(files: &[PathBuf]) -> bool {
    files.iter().any(|path| {
        path.components()
            .any(|component| component.as_os_str() == OsStr::new("fastembed-artifacts"))
    })
}

impl Lens for OnnxLens {
    fn id(&self) -> LensId {
        self.id
    }

    fn shape(&self) -> SlotShape {
        self.contract.shape()
    }

    fn modality(&self) -> Modality {
        self.contract.modality()
    }

    fn measure(&self, input: &Input) -> Result<SlotVector> {
        let mut batch = self.measure_batch(std::slice::from_ref(input))?;
        batch.pop().ok_or_else(|| {
            CalyxError::lens_dim_mismatch(format!("lens {} returned no ONNX vector", self.id))
        })
    }

    fn measure_batch(&self, inputs: &[Input]) -> Result<Vec<SlotVector>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        match self.backend_ref() {
            OnnxBackend::FastEmbed(model) => self.measure_fastembed(model, inputs),
            OnnxBackend::Custom(runtime) => {
                let mut runtime = runtime.lock().map_err(|_| {
                    CalyxError::lens_unreachable("custom ONNX session mutex was poisoned")
                })?;
                let max_batch = scoped_max_batch(self.max_batch)?;
                runtime.measure_batch(self, inputs, max_batch)
            }
        }
    }
}

impl OnnxLens {
    fn backend_ref(&self) -> &OnnxBackend {
        self.backend
            .as_ref()
            .expect("ONNX backend is present until OnnxLens::drop")
    }

    fn measure_fastembed(
        &self,
        model: &Mutex<TextEmbedding>,
        inputs: &[Input],
    ) -> Result<Vec<SlotVector>> {
        if self.provider_policy == OnnxProviderPolicy::CudaFailLoud {
            return Err(fastembed_runtime::device_postprocess_unavailable(
                "onnx-fastembed",
            ));
        }
        let mut texts = Vec::with_capacity(inputs.len());
        for input in inputs {
            texts.push(text_from_input(self, input)?.to_string());
        }
        let mut model = model.lock().map_err(|_| {
            CalyxError::lens_unreachable("ONNX model mutex was poisoned during inference")
        })?;
        let embeddings = model
            .embed(texts, None)
            .map_err(|err| CalyxError::lens_unreachable(format!("ONNX inference failed: {err}")))?;
        if embeddings.len() != inputs.len() {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "ONNX returned {} vectors for {} inputs",
                embeddings.len(),
                inputs.len()
            )));
        }
        embeddings
            .into_iter()
            .map(|mut data| {
                if data.len() != self.dim as usize {
                    return Err(CalyxError::lens_dim_mismatch(format!(
                        "ONNX dim {} != expected {}",
                        data.len(),
                        self.dim
                    )));
                }
                normalize_unit(&mut data)?;
                Ok(SlotVector::Dense {
                    dim: self.dim,
                    data,
                })
            })
            .collect()
    }
}

impl Drop for OnnxLens {
    fn drop(&mut self) {
        if self.provider_policy == OnnxProviderPolicy::CudaFailLoud
            && let Some(backend) = self.backend.take()
        {
            // ORT CUDA provider teardown can corrupt glibc heap in a manual verification run after
            // successful inference. Keep CUDA sessions process-resident; setup and
            // inference still fail loudly, and the OS reclaims pages at exit.
            std::mem::forget(backend);
        }
    }
}

pub(crate) fn config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: "CALYX_LENS_CONFIG_INVALID",
        message: message.into(),
        remediation: "fix ONNX model/tokenizer/config or register a supported lens spec",
    }
}
