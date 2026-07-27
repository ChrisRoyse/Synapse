//! RoBERTa prompt-injection guard lens for Ward's runtime defense (#697).
//!
//! The fine-tuned `RobertaForSequenceClassification` injection guard (#562,
//! `model_comb`, 2 labels: 0=benign, 1=injection) is exported to ONNX and run
//! here through the same pinned `ort` CUDA session used by [`super::style_lens`].
//! The lens emits a `benign_score = softmax(logits)[benign]` per input; Ward's
//! conformal `calibrate_slot` turns that into a block threshold `tau` (block iff
//! `benign_score < tau`), gated on BOTH injection block-rate AND benign FRR.
//!
//! This is the production seam: the classifier now scores inside the Rust Ward
//! runtime, not only in the offline Python validator, so injections are blocked
//! in-process. The CUDA execution provider is fail-loud — a missing/!working GPU
//! errors out rather than silently falling back to CPU.

use std::fmt;
use std::path::{Path, PathBuf};

use calyx_core::{
    CalyxError, Input, Lens, LensId, Modality, Result as CalyxResult, SlotShape, SlotVector,
};

use crate::error::WardError;

mod backend;

use backend::{OnnxInjectionBackend, external_data_path, hash_parts, sha256_files};

pub const DEFAULT_INJECTION_MODEL_PATH: &str = "/var/lib/calyx/models/injection-guard/model.onnx";
pub const DEFAULT_INJECTION_TOKENIZER_PATH: &str =
    "/var/lib/calyx/models/injection-guard/tokenizer.json";
/// Maximum RoBERTa sequence length, including tokenizer-added special tokens.
pub const INJECTION_MAX_TOKENS: usize = 512;
/// Stable preprocessing contract included in the frozen lens identity.
pub const INJECTION_INPUT_COVERAGE_POLICY: &str = "complete_input_or_error:max_encoded_tokens:v1";
/// `RobertaForSequenceClassification` injection head: 2 logits.
pub const INJECTION_LABELS: usize = 2;
const BENIGN_LABEL: usize = 0;
const INJECTION_LABEL: usize = 1;
const INJECTION_LENS_NAME: &str = "injection-guard-v1";
const INJECTION_SOURCE_REPO: &str = "calyx/injection_guard#562-model_comb";
const INJECTION_SOURCE_REVISION: &str = "roberta-base/safe-guard+deepset+jackhhao/8ep";
const OUTPUT_SHAPE: &[u8] = b"dense:f32:text:injection_benign_score:1";

/// ONNX execution-provider policy for the injection guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectionProviderPolicy {
    CudaFailLoud,
    CpuExplicit,
}

impl InjectionProviderPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CudaFailLoud => "cuda:0,error_on_failure,no_cpu_fallback",
            Self::CpuExplicit => "cpu_explicit,no_cuda",
        }
    }
}

/// Backend seam: production uses the pinned ONNX session; tests inject scores.
pub trait InjectionScoreBackend: Send + Sync {
    /// Probability the text is BENIGN (`softmax(logits)[benign]`), in `[0, 1]`.
    /// Implementations score the complete input or return an error when full
    /// coverage is unavailable; they must never silently truncate input.
    fn benign_score(&self, text: &str) -> Result<f32, WardError>;

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

/// Frozen prompt-injection guard lens. Runtime state is ORT + tokenizer handles.
pub struct InjectionLens {
    model_path: PathBuf,
    tokenizer_path: PathBuf,
    lens_id: LensId,
    backend: Box<dyn InjectionScoreBackend>,
}

impl fmt::Debug for InjectionLens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InjectionLens")
            .field("model_path", &self.model_path)
            .field("tokenizer_path", &self.tokenizer_path)
            .field("lens_id", &self.lens_id)
            .field("provider_policy", &self.provider_policy())
            .finish()
    }
}

impl InjectionLens {
    pub fn new(model_path: &Path) -> Result<Self, WardError> {
        Self::new_with_provider_policy(model_path, InjectionProviderPolicy::CudaFailLoud)
    }

    pub fn new_cpu_explicit(model_path: &Path) -> Result<Self, WardError> {
        Self::new_with_provider_policy(model_path, InjectionProviderPolicy::CpuExplicit)
    }

    pub fn new_with_provider_policy(
        model_path: &Path,
        policy: InjectionProviderPolicy,
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
        policy: InjectionProviderPolicy,
    ) -> Result<Self, WardError> {
        // ONNX stores large weights in a `<model>.data` external-data sidecar;
        // include it (when present) so the lens identity pins the actual weights,
        // not just the tiny graph file.
        let external_data = external_data_path(model_path);
        let mut hash_paths: Vec<&Path> = vec![model_path, tokenizer_path];
        if external_data.is_file() {
            hash_paths.push(external_data.as_path());
        }
        let weights_hash = sha256_files(&hash_paths)?;
        let backend = OnnxInjectionBackend::new(model_path, tokenizer_path, policy)?;
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
        B: InjectionScoreBackend + 'static,
    {
        let max_tokens = (INJECTION_MAX_TOKENS as u64).to_le_bytes();
        let corpus_hash = hash_parts(&[
            INJECTION_SOURCE_REPO.as_bytes(),
            INJECTION_SOURCE_REVISION.as_bytes(),
            b"input_ids",
            b"attention_mask",
            b"logits",
            b"softmax_benign",
            INJECTION_INPUT_COVERAGE_POLICY.as_bytes(),
            &max_tokens,
        ]);
        let lens_id = LensId::from_parts(
            INJECTION_LENS_NAME,
            &weights_sha256,
            &corpus_hash,
            OUTPUT_SHAPE,
        );
        Ok(Self {
            model_path,
            tokenizer_path,
            lens_id,
            backend: Box::new(backend),
        })
    }

    /// `P(benign)` for `text`, in `[0, 1]`. Higher = more benign; Ward blocks
    /// when this is below the calibrated `tau`.
    pub fn benign_score(&self, text: &str) -> Result<f32, WardError> {
        if text.trim().is_empty() {
            return Err(WardError::InvalidInput {
                reason: "empty injection-guard text".to_string(),
            });
        }
        let score = self.backend.benign_score(text)?;
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(WardError::InvalidInput {
                reason: format!("injection benign_score {score} outside [0,1]"),
            });
        }
        Ok(score)
    }

    /// `P(injection) = 1 - P(benign)`.
    pub fn injection_prob(&self, text: &str) -> Result<f32, WardError> {
        Ok(1.0 - self.benign_score(text)?)
    }

    pub fn benign_score_batch(&self, texts: &[&str]) -> Result<Vec<f32>, WardError> {
        texts.iter().map(|text| self.benign_score(text)).collect()
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn tokenizer_path(&self) -> &Path {
        &self.tokenizer_path
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

impl Lens for InjectionLens {
    fn id(&self) -> LensId {
        self.lens_id
    }

    fn shape(&self) -> SlotShape {
        SlotShape::Dense(1)
    }

    fn modality(&self) -> Modality {
        Modality::Text
    }

    fn measure(&self, input: &Input) -> CalyxResult<SlotVector> {
        if input.modality != Modality::Text {
            return Err(ward_as_calyx(WardError::InvalidInput {
                reason: format!("injection lens expects text, got {:?}", input.modality),
            }));
        }
        let text = std::str::from_utf8(&input.bytes).map_err(|err| {
            ward_as_calyx(WardError::InvalidInput {
                reason: format!("injection Input bytes must be UTF-8: {err}"),
            })
        })?;
        let score = self.benign_score(text).map_err(ward_as_calyx)?;
        Ok(SlotVector::Dense {
            dim: 1,
            data: vec![score],
        })
    }
}

fn ward_as_calyx(error: WardError) -> CalyxError {
    CalyxError {
        code: error.code(),
        message: error.to_string(),
        remediation: "fix Ward injection lens model/input and retry",
    }
}
