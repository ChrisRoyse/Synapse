use std::path::PathBuf;
use std::sync::Mutex;

use calyx_core::{CalyxError, Input, Lens, LensId, Modality, Result, SlotShape, SlotVector};
use fastembed::Qwen3TextEmbedding;

use crate::frozen::{FrozenLensContract, LensDType, NormPolicy, sha256_digest};
use crate::runtime::candle::{CandleDevicePolicy, CandlePrecision};
use crate::runtime::common::{hash_files, text_from_input};
use crate::spec::{LensRuntime, LensSpec, default_recall_delta};

mod files;
mod load;

pub use files::Qwen3ModelFiles;
use load::{dense_batch, qwen3_model_id, read_config, read_model, read_tokenizer};

pub const DEFAULT_QWEN3_MODEL: &str = "Qwen/Qwen3-Embedding-0.6B";
pub use crate::runtime::common::DEFAULT_QWEN3_MAX_TOKENS;

const OPTIONAL_QWEN3_FILES: &[&str] = &[
    "tokenizer_config.json",
    "special_tokens_map.json",
    "generation_config.json",
    "merges.txt",
    "vocab.json",
    "modules.json",
    "config_sentence_transformers.json",
    "1_Pooling/config.json",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Qwen3FileSpec {
    pub name: String,
    pub model_id: String,
    pub files: Qwen3ModelFiles,
    pub max_tokens: usize,
    pub device_policy: CandleDevicePolicy,
    pub precision: CandlePrecision,
    pub expected_shape: Option<SlotShape>,
    pub expected_weights_sha256: Option<[u8; 32]>,
}

pub struct FastembedQwen3Lens {
    id: LensId,
    dim: u32,
    contract: FrozenLensContract,
    files: Qwen3ModelFiles,
    device_policy: CandleDevicePolicy,
    precision: CandlePrecision,
    max_tokens: usize,
    model: Mutex<Qwen3TextEmbedding>,
}

impl FastembedQwen3Lens {
    pub fn from_model_id_with_policy(
        name: impl Into<String>,
        model_id: &str,
        cache_dir: PathBuf,
        device_policy: CandleDevicePolicy,
        precision: CandlePrecision,
    ) -> Result<Self> {
        Self::from_model_id_with_policy_and_max_tokens(
            name,
            model_id,
            cache_dir,
            device_policy,
            precision,
            DEFAULT_QWEN3_MAX_TOKENS,
        )
    }

    pub fn from_model_id_with_policy_and_max_tokens(
        name: impl Into<String>,
        model_id: &str,
        cache_dir: PathBuf,
        device_policy: CandleDevicePolicy,
        precision: CandlePrecision,
        max_tokens: usize,
    ) -> Result<Self> {
        let model_id = qwen3_model_id(model_id)?;
        let files = files::fetch_files(&cache_dir, &model_id)?;
        Self::from_files(Qwen3FileSpec {
            name: name.into(),
            model_id,
            files,
            max_tokens,
            device_policy,
            precision,
            expected_shape: None,
            expected_weights_sha256: None,
        })
    }

    pub fn from_files(spec: Qwen3FileSpec) -> Result<Self> {
        if spec.max_tokens == 0 {
            return Err(config_invalid("fastembed-qwen3 max_tokens must be > 0"));
        }
        for path in spec.files.artifact_paths() {
            ensure_file("artifact", &path)?;
        }
        let weights_sha256 = hash_files(&spec.files.artifact_paths())?;
        if let Some(expected) = spec.expected_weights_sha256
            && weights_sha256 != expected
        {
            return Err(CalyxError::lens_frozen_violation(format!(
                "fastembed-qwen3 artifact hash drift for {}",
                spec.model_id
            )));
        }
        let config = read_config(&spec.files.config)?;
        let dim = u32::try_from(config.hidden_size).map_err(|_| {
            CalyxError::lens_dim_mismatch(format!(
                "Qwen3 hidden size {} exceeds u32",
                config.hidden_size
            ))
        })?;
        if let Some(expected) = spec.expected_shape
            && expected != SlotShape::Dense(dim)
        {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "Qwen3 output shape Dense({dim}) != declared {expected:?}"
            )));
        }
        let tokenizer = read_tokenizer(&spec.files.tokenizer, spec.max_tokens)?;
        let model = read_model(
            &spec.files.weights,
            config,
            tokenizer,
            spec.device_policy,
            spec.precision,
        )?;
        let max_tokens = spec.max_tokens.to_string();
        let corpus_hash = sha256_digest(&[
            b"fastembed-qwen3-text-v1",
            spec.model_id.as_bytes(),
            spec.precision.as_str().as_bytes(),
            max_tokens.as_bytes(),
            b"left-padding,last-token,l2",
        ]);
        let contract = FrozenLensContract::new(
            spec.name,
            weights_sha256,
            corpus_hash,
            SlotShape::Dense(dim),
            Modality::Text,
            LensDType::F32,
            NormPolicy::unit(),
        );
        Ok(Self {
            id: contract.lens_id(),
            dim,
            contract,
            files: spec.files,
            device_policy: spec.device_policy,
            precision: spec.precision,
            max_tokens: spec.max_tokens,
            model: Mutex::new(model),
        })
    }

    pub fn from_lens_spec(spec: &LensSpec) -> Result<Self> {
        let LensRuntime::FastembedQwen3 {
            model_id,
            files,
            dtype,
            max_tokens,
        } = &spec.runtime
        else {
            return Err(config_invalid("LensSpec runtime is not fastembed-qwen3"));
        };
        if *max_tokens == 0 {
            return Err(config_invalid("fastembed-qwen3 max_tokens must be > 0"));
        }
        let model_id = qwen3_model_id(model_id)?;
        Self::from_files(Qwen3FileSpec {
            name: spec.name.clone(),
            model_id: model_id.clone(),
            files: Qwen3ModelFiles::from_paths(model_id, files.clone())?,
            max_tokens: *max_tokens,
            device_policy: CandleDevicePolicy::CudaFailLoud { ordinal: 0 },
            precision: CandlePrecision::parse(dtype)?,
            expected_shape: Some(spec.output),
            expected_weights_sha256: Some(spec.weights_sha256),
        })
    }

    pub fn contract(&self) -> &FrozenLensContract {
        &self.contract
    }

    pub fn files(&self) -> &Qwen3ModelFiles {
        &self.files
    }

    pub const fn device_policy(&self) -> CandleDevicePolicy {
        self.device_policy
    }

    pub const fn precision(&self) -> CandlePrecision {
        self.precision
    }

    pub const fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    pub const fn runtime_name(&self) -> &'static str {
        "fastembed-qwen3"
    }

    pub fn lens_spec(&self) -> LensSpec {
        LensSpec {
            name: self.contract.name().to_string(),
            runtime: LensRuntime::FastembedQwen3 {
                model_id: self.files.model_id.clone(),
                files: self.files.artifact_paths(),
                dtype: self.precision.as_str().to_string(),
                max_tokens: self.max_tokens,
            },
            output: self.contract.shape(),
            modality: self.contract.modality(),
            weights_sha256: self.contract.weights_sha256(),
            corpus_hash: self.contract.corpus_hash(),
            norm_policy: self.contract.norm_policy(),
            max_batch: None,
            axis: None,
            asymmetry: calyx_core::Asymmetry::None,
            quant_default: calyx_core::QuantPolicy::turboquant_default(),
            truncate_dim: None,
            recall_delta: default_recall_delta(),
            retrieval_only: false,
            excluded_from_dedup: false,
        }
    }
}

impl Lens for FastembedQwen3Lens {
    fn id(&self) -> LensId {
        self.id
    }

    fn shape(&self) -> SlotShape {
        SlotShape::Dense(self.dim)
    }

    fn modality(&self) -> Modality {
        Modality::Text
    }

    fn measure(&self, input: &Input) -> Result<SlotVector> {
        let mut batch = self.measure_batch(std::slice::from_ref(input))?;
        batch.pop().ok_or_else(|| {
            CalyxError::lens_dim_mismatch(format!("lens {} returned no Qwen3 vector", self.id))
        })
    }

    fn measure_batch(&self, inputs: &[Input]) -> Result<Vec<SlotVector>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let texts = inputs
            .iter()
            .map(|input| text_from_input(self, input).map(str::to_string))
            .collect::<Result<Vec<_>>>()?;
        let model = self
            .model
            .lock()
            .map_err(|_| CalyxError::lens_unreachable("Qwen3 model mutex was poisoned"))?;
        let rows = model.embed(&texts).map_err(qwen3_error)?;
        let vectors = dense_batch(self.dim, rows, inputs.len())?;
        for vector in &vectors {
            self.contract.verify_vector(self.id, vector)?;
        }
        Ok(vectors)
    }
}

fn ensure_file(label: &str, path: &std::path::Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    Err(config_invalid(format!(
        "fastembed-qwen3 {label} file {} is missing",
        path.display()
    )))
}

pub(crate) fn qwen3_error(err: candle_core::Error) -> CalyxError {
    let message = format!("Qwen3 runtime failed: {err}");
    let lower = message.to_ascii_lowercase();
    if lower.contains("out of memory") || lower.contains("memoryallocation") {
        return CalyxError {
            code: "CALYX_VRAM_OOM",
            message,
            remediation: "free VRAM, reduce batch size, or evict lower-priority GPU lenses",
        };
    }
    CalyxError::lens_unreachable(message)
}

pub(crate) fn config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: "CALYX_LENS_CONFIG_INVALID",
        message: message.into(),
        remediation: "fix Qwen3 model/tokenizer/config or register a supported lens spec",
    }
}
