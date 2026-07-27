//! RoBERTa style lens adapter for PH39 identity slots.

mod tokenization;

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use calyx_core::{
    CalyxError, Input, Lens, LensId, Modality, Result as CalyxResult, SlotShape, SlotVector,
};
use ort::ep::{self, ArenaExtendStrategy, ExecutionProviderDispatch};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::{Tensor, TensorElementType, ValueType};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::error::WardError;

pub const DEFAULT_STYLE_MODEL_PATH: &str = "/var/lib/calyx/models/style/style-embed-v1.onnx";
pub const DEFAULT_STYLE_TOKENIZER_PATH: &str = "/var/lib/calyx/models/style/tokenizer.json";
pub const STYLE_DIM: usize = 768;
pub const STYLE_MAX_TOKENS: usize = 512;
/// Stable preprocessing contract included in the frozen lens identity.
pub const STYLE_INPUT_COVERAGE_POLICY: &str = "complete_input_or_error:max_encoded_tokens:v1";
const STYLE_LENS_NAME: &str = "style-embed-v1";
const STYLE_SOURCE_REPO: &str = "AnnaWegmann/Style-Embedding";
const STYLE_SOURCE_REVISION: &str = "d7d0f5ca829316a8f5695e49dfce80b86db5e76c";
const OUTPUT_SHAPE: &[u8] = b"dense:f32:text:style:768";

/// ONNX execution-provider policy for the style adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleProviderPolicy {
    CudaFailLoud,
    CpuExplicit,
}

impl StyleProviderPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CudaFailLoud => "cuda:0,error_on_failure,no_cpu_fallback",
            Self::CpuExplicit => "cpu_explicit,no_cuda",
        }
    }
}

/// Backend seam used by tests while production uses the pinned ONNX session.
pub trait StyleEmbeddingBackend: Send + Sync {
    /// Embeds the complete input or returns an error when full coverage is unavailable.
    /// Implementations must not silently truncate input.
    fn embed(&self, text: &str) -> Result<Vec<f32>, WardError>;
    fn output_dim(&self) -> usize;

    fn input_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn output_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn provider_policy(&self) -> &'static str {
        "test_backend"
    }
}

/// Frozen style/register lens. Runtime state is limited to ORT and tokenizer handles.
pub struct StyleLens {
    model_path: PathBuf,
    tokenizer_path: PathBuf,
    lens_id: LensId,
    dim: usize,
    backend: Box<dyn StyleEmbeddingBackend>,
}

impl fmt::Debug for StyleLens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StyleLens")
            .field("model_path", &self.model_path)
            .field("tokenizer_path", &self.tokenizer_path)
            .field("lens_id", &self.lens_id)
            .field("dim", &self.dim)
            .field("provider_policy", &self.provider_policy())
            .finish()
    }
}

impl StyleLens {
    pub fn new(model_path: &Path) -> Result<Self, WardError> {
        Self::new_with_provider_policy(model_path, StyleProviderPolicy::CudaFailLoud)
    }

    pub fn new_cpu_explicit(model_path: &Path) -> Result<Self, WardError> {
        Self::new_with_provider_policy(model_path, StyleProviderPolicy::CpuExplicit)
    }

    pub fn new_with_provider_policy(
        model_path: &Path,
        policy: StyleProviderPolicy,
    ) -> Result<Self, WardError> {
        let tokenizer_path = model_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("tokenizer.json");
        Self::new_with_tokenizer_and_provider_policy(model_path, &tokenizer_path, policy)
    }

    pub fn new_with_tokenizer_and_provider_policy(
        model_path: &Path,
        tokenizer_path: &Path,
        policy: StyleProviderPolicy,
    ) -> Result<Self, WardError> {
        let weights_hash = sha256_files(&[model_path, tokenizer_path])?;
        let backend = OnnxStyleBackend::new(model_path, tokenizer_path, policy)?;
        Self::from_backend(
            model_path.to_path_buf(),
            tokenizer_path.to_path_buf(),
            weights_hash,
            backend,
        )
    }

    pub fn from_backend<B>(
        model_path: PathBuf,
        tokenizer_path: PathBuf,
        weights_sha256: [u8; 32],
        backend: B,
    ) -> Result<Self, WardError>
    where
        B: StyleEmbeddingBackend + 'static,
    {
        let dim = backend.output_dim();
        if dim != STYLE_DIM {
            return Err(WardError::ModelDimMismatch {
                expected: STYLE_DIM,
                actual: dim,
            });
        }
        let max_tokens = (STYLE_MAX_TOKENS as u64).to_le_bytes();
        let corpus_hash = hash_parts(&[
            STYLE_SOURCE_REPO.as_bytes(),
            STYLE_SOURCE_REVISION.as_bytes(),
            b"input_ids",
            b"attention_mask",
            b"last_hidden_state",
            b"mean_pool_attention_mask",
            STYLE_INPUT_COVERAGE_POLICY.as_bytes(),
            &max_tokens,
        ]);
        let lens_id =
            LensId::from_parts(STYLE_LENS_NAME, &weights_sha256, &corpus_hash, OUTPUT_SHAPE);

        Ok(Self {
            model_path,
            tokenizer_path,
            lens_id,
            dim,
            backend: Box::new(backend),
        })
    }

    pub fn embed_style(&self, text: &str) -> Result<Vec<f32>, WardError> {
        if text.trim().is_empty() {
            return Err(WardError::InvalidInput {
                reason: "empty style text".to_string(),
            });
        }
        let raw = self.backend.embed(text)?;
        normalize_unit(raw, self.dim)
    }

    pub fn embed_style_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, WardError> {
        texts.iter().map(|text| self.embed_style(text)).collect()
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn tokenizer_path(&self) -> &Path {
        &self.tokenizer_path
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn provider_policy(&self) -> &'static str {
        self.backend.provider_policy()
    }

    pub fn input_names(&self) -> Vec<String> {
        self.backend.input_names()
    }

    pub fn output_names(&self) -> Vec<String> {
        self.backend.output_names()
    }
}

impl Lens for StyleLens {
    fn id(&self) -> LensId {
        self.lens_id
    }

    fn shape(&self) -> SlotShape {
        SlotShape::Dense(self.dim as u32)
    }

    fn modality(&self) -> Modality {
        Modality::Text
    }

    fn measure(&self, input: &Input) -> CalyxResult<SlotVector> {
        if input.modality != Modality::Text {
            return Err(ward_as_calyx(WardError::InvalidInput {
                reason: format!("style lens expects text, got {:?}", input.modality),
            }));
        }
        let text = std::str::from_utf8(&input.bytes).map_err(|err| {
            ward_as_calyx(WardError::InvalidInput {
                reason: format!("style Input bytes must be UTF-8: {err}"),
            })
        })?;
        let data = self.embed_style(text).map_err(ward_as_calyx)?;
        Ok(SlotVector::Dense {
            dim: self.dim as u32,
            data,
        })
    }
}

struct OnnxStyleBackend {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_ids_name: String,
    attention_mask_name: String,
    output_name: String,
    input_names: Vec<String>,
    output_names: Vec<String>,
    output_dim: usize,
    policy: StyleProviderPolicy,
}

impl OnnxStyleBackend {
    fn new(
        model_path: &Path,
        tokenizer_path: &Path,
        policy: StyleProviderPolicy,
    ) -> Result<Self, WardError> {
        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|_| WardError::ModelNotFound {
                path: tokenizer_path.to_path_buf(),
            })?;
        tokenizer
            .with_truncation(None)
            .map(|_| ())
            .map_err(runtime_error)?;
        let session = build_session(model_path, policy)?;
        let input_names = session
            .inputs()
            .iter()
            .map(|input| input.name().to_string())
            .collect::<Vec<_>>();
        let output_names = session
            .outputs()
            .iter()
            .map(|output| output.name().to_string())
            .collect::<Vec<_>>();
        let input_ids_name = choose_name(&input_names, "input_ids", "input")?;
        let attention_mask_name = choose_name(&input_names, "attention_mask", "input")?;
        let output_name = choose_name(&output_names, "last_hidden_state", "output")?;
        let output_dim = output_dim(&session, &output_name)?;

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            input_ids_name,
            attention_mask_name,
            output_name,
            input_names,
            output_names,
            output_dim,
            policy,
        })
    }

    fn tokenize(&self, text: &str) -> Result<(Vec<i64>, Vec<i64>), WardError> {
        let encoding = self.tokenizer.encode(text, true).map_err(runtime_error)?;
        if !encoding.get_overflowing().is_empty() {
            return Err(WardError::InvalidInput {
                reason: format!(
                    "style tokenizer retained {} overflow encoding(s); complete input coverage is unavailable",
                    encoding.get_overflowing().len()
                ),
            });
        }
        tokenization::model_inputs(encoding.get_ids(), encoding.get_attention_mask())
    }
}

impl StyleEmbeddingBackend for OnnxStyleBackend {
    fn embed(&self, text: &str) -> Result<Vec<f32>, WardError> {
        let (ids, attention) = self.tokenize(text)?;
        let seq_len = ids.len();
        let ids_tensor = Tensor::from_array(([1usize, seq_len], ids)).map_err(runtime_error)?;
        let mask_tensor =
            Tensor::from_array(([1usize, seq_len], attention.clone())).map_err(runtime_error)?;
        let mut session = self.session.lock().map_err(|_| WardError::Runtime {
            reason: "style lens ORT session mutex poisoned".to_string(),
        })?;
        let outputs = session
            .run(ort::inputs! {
                self.input_ids_name.as_str() => ids_tensor,
                self.attention_mask_name.as_str() => mask_tensor
            })
            .map_err(runtime_error)?;
        let output = outputs
            .get(&self.output_name)
            .ok_or_else(|| WardError::Runtime {
                reason: format!("ONNX output {} missing", self.output_name),
            })?;
        let (_, data) = output.try_extract_tensor::<f32>().map_err(runtime_error)?;
        let flat = data.to_vec();
        mean_pool(&flat, &attention, self.output_dim)
    }

    fn output_dim(&self) -> usize {
        self.output_dim
    }

    fn input_names(&self) -> Vec<String> {
        self.input_names.clone()
    }

    fn output_names(&self) -> Vec<String> {
        self.output_names.clone()
    }

    fn provider_policy(&self) -> &'static str {
        self.policy.as_str()
    }
}

fn build_session(model_path: &Path, policy: StyleProviderPolicy) -> Result<Session, WardError> {
    if !model_path.exists() {
        return Err(WardError::ModelNotFound {
            path: model_path.to_path_buf(),
        });
    }
    let _ort_dylib = crate::ort_runtime::ensure_dynamic_ort()?;
    let builder = Session::builder()
        .map_err(runtime_error)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(runtime_error)?;
    let mut builder = builder
        .with_execution_providers(execution_providers(policy))
        .map_err(runtime_error)?;
    builder.commit_from_file(model_path).map_err(runtime_error)
}

fn execution_providers(policy: StyleProviderPolicy) -> Vec<ExecutionProviderDispatch> {
    match policy {
        StyleProviderPolicy::CudaFailLoud => vec![
            // #1143: kNextPowerOfTwo over-reserves the BFC device arena.
            ep::CUDA::default()
                .with_device_id(0)
                .with_arena_extend_strategy(ArenaExtendStrategy::SameAsRequested)
                .build()
                .error_on_failure(),
        ],
        StyleProviderPolicy::CpuExplicit => vec![ep::CPU::default().build()],
    }
}

fn choose_name(names: &[String], preferred: &str, kind: &str) -> Result<String, WardError> {
    names
        .iter()
        .find(|name| name.as_str() == preferred)
        .cloned()
        .ok_or_else(|| WardError::Runtime {
            reason: format!("ONNX session has no {kind} named {preferred}"),
        })
}

fn output_dim(session: &Session, output_name: &str) -> Result<usize, WardError> {
    let outlet = session
        .outputs()
        .iter()
        .find(|output| output.name() == output_name)
        .ok_or_else(|| WardError::Runtime {
            reason: format!("ONNX output {output_name} missing from metadata"),
        })?;
    match outlet.dtype() {
        ValueType::Tensor { ty, shape, .. } if *ty == TensorElementType::Float32 => shape
            .iter()
            .rev()
            .copied()
            .find(|dim| *dim > 0)
            .map(|dim| dim as usize)
            .ok_or_else(|| WardError::Runtime {
                reason: format!("ONNX output {output_name} has no static positive dim"),
            }),
        other => Err(WardError::Runtime {
            reason: format!("ONNX output {output_name} is not f32 tensor: {other:?}"),
        }),
    }
}

fn mean_pool(
    token_embeddings: &[f32],
    attention: &[i64],
    dim: usize,
) -> Result<Vec<f32>, WardError> {
    if token_embeddings.len() != attention.len() * dim {
        return Err(WardError::ModelDimMismatch {
            expected: attention.len() * dim,
            actual: token_embeddings.len(),
        });
    }
    let mut pooled = vec![0.0_f32; dim];
    let mut active = 0.0_f32;
    for (token_idx, mask) in attention.iter().enumerate() {
        if *mask != 0 {
            active += 1.0;
            let start = token_idx * dim;
            for dim_idx in 0..dim {
                pooled[dim_idx] += token_embeddings[start + dim_idx];
            }
        }
    }
    if active <= 0.0 {
        return Err(WardError::InvalidInput {
            reason: "style attention mask has no active tokens".to_string(),
        });
    }
    pooled.iter_mut().for_each(|value| *value /= active);
    Ok(pooled)
}

fn normalize_unit(mut data: Vec<f32>, expected_dim: usize) -> Result<Vec<f32>, WardError> {
    if data.len() != expected_dim {
        return Err(WardError::ModelDimMismatch {
            expected: expected_dim,
            actual: data.len(),
        });
    }
    if data.iter().any(|value| !value.is_finite()) {
        return Err(WardError::InvalidInput {
            reason: "style embedding contains NaN or Inf".to_string(),
        });
    }
    let norm = data.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return Err(WardError::InvalidInput {
            reason: "style embedding has zero norm".to_string(),
        });
    }
    data.iter_mut().for_each(|value| *value /= norm);
    Ok(data)
}

fn sha256_files(paths: &[&Path]) -> Result<[u8; 32], WardError> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    for path in paths {
        let mut file = File::open(path).map_err(|_| WardError::ModelNotFound {
            path: (*path).to_path_buf(),
        })?;
        loop {
            let n = file.read(&mut buf).map_err(runtime_error)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(hasher.finalize().into())
}

fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn runtime_error(error: impl fmt::Display) -> WardError {
    WardError::Runtime {
        reason: error.to_string(),
    }
}

fn ward_as_calyx(error: WardError) -> CalyxError {
    CalyxError {
        code: error.code(),
        message: error.to_string(),
        remediation: "fix Ward style lens model/input and retry",
    }
}
