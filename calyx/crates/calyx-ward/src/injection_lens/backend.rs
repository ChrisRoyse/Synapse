use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ort::ep::{self, ArenaExtendStrategy, ExecutionProviderDispatch};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::{Tensor, TensorElementType, ValueType};
use sha2::{Digest, Sha256};
use tokenizers::{Encoding, Tokenizer};

use crate::error::WardError;

use super::{
    BENIGN_LABEL, INJECTION_LABEL, INJECTION_LABELS, INJECTION_MAX_TOKENS, InjectionProviderPolicy,
    InjectionScoreBackend,
};

pub(super) struct OnnxInjectionBackend {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    input_ids_name: String,
    attention_mask_name: String,
    output_name: String,
    input_names: Vec<String>,
    output_names: Vec<String>,
    policy: InjectionProviderPolicy,
}

impl OnnxInjectionBackend {
    pub(super) fn new(
        model_path: &Path,
        tokenizer_path: &Path,
        policy: InjectionProviderPolicy,
    ) -> Result<Self, WardError> {
        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|_| WardError::ModelNotFound {
                path: tokenizer_path.to_path_buf(),
            })?;
        disable_truncation(&mut tokenizer)?;
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
        let output_name = choose_name(&output_names, "logits", "output")?;
        assert_logits_shape(&session, &output_name)?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            input_ids_name,
            attention_mask_name,
            output_name,
            input_names,
            output_names,
            policy,
        })
    }

    fn tokenize(&self, text: &str) -> Result<(Vec<i64>, Vec<i64>), WardError> {
        let encoding = self.tokenizer.encode(text, true).map_err(runtime_error)?;
        encoding_inputs(&encoding)
    }
}

fn disable_truncation(tokenizer: &mut Tokenizer) -> Result<(), WardError> {
    tokenizer
        .with_truncation(None)
        .map(|_| ())
        .map_err(runtime_error)
}

fn encoding_inputs(encoding: &Encoding) -> Result<(Vec<i64>, Vec<i64>), WardError> {
    if !encoding.get_overflowing().is_empty() {
        return Err(WardError::InvalidInput {
            reason: format!(
                "injection tokenizer retained {} overflow encoding(s); complete input coverage is unavailable",
                encoding.get_overflowing().len()
            ),
        });
    }
    model_inputs(encoding.get_ids(), encoding.get_attention_mask())
}

fn model_inputs(
    token_ids: &[u32],
    attention_mask: &[u32],
) -> Result<(Vec<i64>, Vec<i64>), WardError> {
    if token_ids.len() != attention_mask.len() {
        return Err(WardError::InvalidInput {
            reason: format!(
                "injection tokenizer emitted {} token IDs but {} attention values",
                token_ids.len(),
                attention_mask.len()
            ),
        });
    }

    let len = token_ids.len();
    if len == 0 {
        return Err(WardError::InvalidInput {
            reason: "injection tokenizer emitted no tokens".to_string(),
        });
    }
    if len > INJECTION_MAX_TOKENS {
        return Err(WardError::InvalidInput {
            reason: format!(
                "injection input encoded to {len} tokens, exceeding the complete-coverage limit \
                 of {INJECTION_MAX_TOKENS}; reject or shorten the input until its complete encoded \
                 sequence is within the limit"
            ),
        });
    }

    let ids = token_ids
        .iter()
        .map(|value| i64::from(*value))
        .collect::<Vec<_>>();
    let attention = attention_mask
        .iter()
        .map(|value| i64::from(*value))
        .collect::<Vec<_>>();
    Ok((ids, attention))
}

impl InjectionScoreBackend for OnnxInjectionBackend {
    fn benign_score(&self, text: &str) -> Result<f32, WardError> {
        let (ids, attention) = self.tokenize(text)?;
        let seq_len = ids.len();
        let ids_tensor = Tensor::from_array(([1usize, seq_len], ids)).map_err(runtime_error)?;
        let mask_tensor =
            Tensor::from_array(([1usize, seq_len], attention)).map_err(runtime_error)?;
        let mut session = self.session.lock().map_err(|_| WardError::Runtime {
            reason: "injection lens ORT session mutex poisoned".to_string(),
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
        if data.len() != INJECTION_LABELS {
            return Err(WardError::ModelDimMismatch {
                expected: INJECTION_LABELS,
                actual: data.len(),
            });
        }
        softmax_benign(data[BENIGN_LABEL], data[INJECTION_LABEL])
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

/// Numerically-stable 2-class softmax, returning `P(benign)`.
pub(super) fn softmax_benign(benign_logit: f32, injection_logit: f32) -> Result<f32, WardError> {
    if !benign_logit.is_finite() || !injection_logit.is_finite() {
        return Err(WardError::InvalidInput {
            reason: "injection logits contain NaN or Inf".to_string(),
        });
    }
    let max = benign_logit.max(injection_logit);
    let benign_exp = (benign_logit - max).exp();
    let injection_exp = (injection_logit - max).exp();
    let denom = benign_exp + injection_exp;
    if denom <= f32::EPSILON {
        return Err(WardError::InvalidInput {
            reason: "injection softmax denominator underflow".to_string(),
        });
    }
    Ok(benign_exp / denom)
}

fn build_session(model_path: &Path, policy: InjectionProviderPolicy) -> Result<Session, WardError> {
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

fn execution_providers(policy: InjectionProviderPolicy) -> Vec<ExecutionProviderDispatch> {
    match policy {
        InjectionProviderPolicy::CudaFailLoud => vec![
            // #1143: extend the BFC device arena exactly as requested;
            // kNextPowerOfTwo over-reserves on dynamic-shape workloads.
            ep::CUDA::default()
                .with_device_id(0)
                .with_arena_extend_strategy(ArenaExtendStrategy::SameAsRequested)
                .build()
                .error_on_failure(),
        ],
        InjectionProviderPolicy::CpuExplicit => vec![ep::CPU::default().build()],
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

/// The injection head must be an f32 tensor whose last static dim is 2.
fn assert_logits_shape(session: &Session, output_name: &str) -> Result<(), WardError> {
    let outlet = session
        .outputs()
        .iter()
        .find(|output| output.name() == output_name)
        .ok_or_else(|| WardError::Runtime {
            reason: format!("ONNX output {output_name} missing from metadata"),
        })?;
    match outlet.dtype() {
        ValueType::Tensor { ty, shape, .. } if *ty == TensorElementType::Float32 => {
            match shape.iter().rev().copied().find(|dim| *dim > 0) {
                Some(dim) if dim as usize == INJECTION_LABELS => Ok(()),
                Some(dim) => Err(WardError::ModelDimMismatch {
                    expected: INJECTION_LABELS,
                    actual: dim as usize,
                }),
                // Fully-dynamic logits dim: validated per-call against the
                // extracted tensor length instead.
                None => Ok(()),
            }
        }
        other => Err(WardError::Runtime {
            reason: format!("ONNX output {output_name} is not f32 tensor: {other:?}"),
        }),
    }
}

/// ONNX external-data sidecar path for `model.onnx` -> `model.onnx.data`.
pub(super) fn external_data_path(model_path: &Path) -> PathBuf {
    let mut name = model_path.file_name().unwrap_or_default().to_os_string();
    name.push(".data");
    model_path.with_file_name(name)
}

pub(super) fn sha256_files(paths: &[&Path]) -> Result<[u8; 32], WardError> {
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

pub(super) fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
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
