use std::env;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use calyx_core::{
    Asymmetry, CalyxError, LensId, Modality, QuantPolicy, Result, SlotShape, content_address,
};
use serde::{Deserialize, Serialize};

use crate::frozen::{FrozenLensContract, LensDType, NormPolicy};

const LENS_UNREACHABLE: &str = "CALYX_LENS_UNREACHABLE";
#[cfg(not(feature = "candle-cuda"))]
const CANDLE_CUDA_FEATURE_MISSING_REASON: &str =
    "candle CUDA requested but calyx-registry was built without feature `candle-cuda`";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastembedBgem3Output {
    Dense,
    Sparse,
    Colbert,
}

/// Frozen execution engine for a joint BGE-M3 projection set.
///
/// `FastembedCpu` is retained only to decode existing persisted specs.  Its
/// INT8 graph is CPU-only and must never be admitted as a CUDA resident.
/// New GPU panels use `OnnxCuda`, which binds the three joint outputs to CUDA
/// memory and performs projection postprocessing on the device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bgem3Engine {
    #[default]
    FastembedCpu,
    OnnxCuda,
}

impl Bgem3Engine {
    pub const fn is_legacy_cpu(self) -> bool {
        matches!(self, Self::FastembedCpu)
    }
}

const fn is_default_bgem3_engine(engine: &Bgem3Engine) -> bool {
    matches!(engine, Bgem3Engine::FastembedCpu)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LensRuntime {
    Algorithmic {
        kind: String,
    },
    TeiHttp {
        endpoint: String,
    },
    CandleLocal {
        model_id: String,
        files: Vec<PathBuf>,
        #[serde(default = "default_candle_dtype")]
        dtype: String,
        #[serde(default = "default_candle_pooling")]
        pooling: String,
    },
    Onnx {
        model_id: String,
        files: Vec<PathBuf>,
    },
    OnnxColbert {
        model_id: String,
        files: Vec<PathBuf>,
    },
    FastembedSparse {
        model_id: String,
        files: Vec<PathBuf>,
    },
    FastembedBgem3 {
        model_id: String,
        files: Vec<PathBuf>,
        output: FastembedBgem3Output,
        // Keep the legacy default absent on serialization. Persisted specs and
        // immutable template snapshots created before the engine discriminator
        // existed must deserialize and reserialize to the same canonical JSON.
        // Non-default engines remain explicit and therefore content-addressed.
        #[serde(default, skip_serializing_if = "is_default_bgem3_engine")]
        engine: Bgem3Engine,
    },
    FastembedReranker {
        model_id: String,
        files: Vec<PathBuf>,
    },
    FastembedQwen3 {
        model_id: String,
        files: Vec<PathBuf>,
        #[serde(default = "default_qwen3_dtype")]
        dtype: String,
        #[serde(
            default = "default_qwen3_max_tokens",
            skip_serializing_if = "is_default_qwen3_max_tokens"
        )]
        max_tokens: usize,
    },
    StaticLookup {
        embeddings_file: PathBuf,
        tokenizer: PathBuf,
        dim: u32,
    },
    MultimodalAdapter {
        axis: String,
        model_id: String,
        #[serde(default)]
        adapter_config: Option<PathBuf>,
        #[serde(default)]
        files: Vec<PathBuf>,
    },
    ExternalCmd {
        cmd: String,
        args: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LensSpec {
    pub name: String,
    pub runtime: LensRuntime,
    pub output: SlotShape,
    pub modality: Modality,
    pub weights_sha256: [u8; 32],
    pub corpus_hash: [u8; 32],
    pub norm_policy: NormPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch: Option<usize>,
    pub axis: Option<String>,
    pub asymmetry: Asymmetry,
    #[serde(default = "default_quant_default")]
    pub quant_default: QuantPolicy,
    #[serde(default)]
    pub truncate_dim: Option<u32>,
    #[serde(default = "default_recall_delta")]
    pub recall_delta: f32,
    pub retrieval_only: bool,
    pub excluded_from_dedup: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LensHealth {
    Loaded,
    Cold,
    Failing { code: String, reason: String },
}

impl LensSpec {
    pub fn declared_contract(&self) -> FrozenLensContract {
        FrozenLensContract::new(
            self.name.clone(),
            self.weights_sha256,
            self.corpus_hash,
            self.output,
            self.modality,
            LensDType::F32,
            self.norm_policy,
        )
    }

    pub fn lens_id(&self) -> LensId {
        let output = format!(
            "shape={:?};norm={:?};runtime={}",
            self.output,
            self.norm_policy,
            runtime_identity(&self.runtime)
        );
        LensId::from_bytes(content_address([
            self.name.as_bytes(),
            &self.weights_sha256,
            &self.corpus_hash,
            output.as_bytes(),
        ]))
    }

    pub fn health(&self) -> LensHealth {
        match &self.runtime {
            LensRuntime::Algorithmic { .. } => LensHealth::Loaded,
            LensRuntime::MultimodalAdapter {
                adapter_config,
                files,
                ..
            } => multimodal_adapter_health(adapter_config.as_ref(), files),
            LensRuntime::TeiHttp { endpoint } => probe_http(endpoint),
            LensRuntime::CandleLocal { files, .. } => candle_local_health(files),
            LensRuntime::Onnx { files, .. }
            | LensRuntime::OnnxColbert { files, .. }
            | LensRuntime::FastembedSparse { files, .. }
            | LensRuntime::FastembedBgem3 { files, .. }
            | LensRuntime::FastembedReranker { files, .. }
            | LensRuntime::FastembedQwen3 { files, .. } => files_runtime_health(files),
            LensRuntime::StaticLookup {
                embeddings_file,
                tokenizer,
                ..
            } => {
                if embeddings_file.is_file() && tokenizer.is_file() {
                    LensHealth::Loaded
                } else {
                    LensHealth::Cold
                }
            }
            LensRuntime::ExternalCmd { cmd, .. } => {
                if command_exists(cmd) {
                    LensHealth::Loaded
                } else {
                    LensHealth::Failing {
                        code: LENS_UNREACHABLE.to_string(),
                        reason: format!("external command {cmd} is not executable"),
                    }
                }
            }
        }
    }

    pub fn health_result(&self) -> Result<LensHealth> {
        let health = self.health();
        match &health {
            LensHealth::Failing { reason, .. } => Err(CalyxError::lens_unreachable(reason)),
            _ => Ok(health),
        }
    }
}

fn runtime_identity(runtime: &LensRuntime) -> String {
    match runtime {
        // Lens identities historically used Rust's Debug representation. The
        // engine discriminator was added after legacy BGE-M3 specs had been
        // persisted, so preserve that exact pre-field representation for the
        // default CPU engine. CUDA remains explicit and domain-separated by
        // the ordinary Debug representation containing `engine: OnnxCuda`.
        LensRuntime::FastembedBgem3 {
            model_id,
            files,
            output,
            engine: Bgem3Engine::FastembedCpu,
        } => format!(
            "FastembedBgem3 {{ model_id: {model_id:?}, files: {files:?}, output: {output:?} }}"
        ),
        runtime => format!("{runtime:?}"),
    }
}

fn default_candle_dtype() -> String {
    "f32".to_string()
}

fn default_candle_pooling() -> String {
    "mean".to_string()
}

fn default_qwen3_dtype() -> String {
    "f16".to_string()
}

pub const fn default_qwen3_max_tokens() -> usize {
    crate::runtime::common::DEFAULT_QWEN3_MAX_TOKENS
}

fn is_default_qwen3_max_tokens(value: &usize) -> bool {
    *value == default_qwen3_max_tokens()
}

pub const fn default_quant_default() -> QuantPolicy {
    QuantPolicy::turboquant_default()
}

pub const fn default_recall_delta() -> f32 {
    0.02
}

fn candle_local_health(files: &[PathBuf]) -> LensHealth {
    match files_runtime_health(files) {
        LensHealth::Loaded => candle_cuda_runtime_health(),
        health => health,
    }
}

fn files_runtime_health(files: &[PathBuf]) -> LensHealth {
    if files.is_empty() {
        return LensHealth::Cold;
    }
    if files.iter().all(|path| path.exists()) {
        LensHealth::Loaded
    } else {
        LensHealth::Cold
    }
}

fn multimodal_adapter_health(adapter_config: Option<&PathBuf>, files: &[PathBuf]) -> LensHealth {
    let Some(adapter_config) = adapter_config else {
        return LensHealth::Cold;
    };
    if !adapter_config.is_file() {
        return LensHealth::Cold;
    }
    files_runtime_health(files)
}

#[cfg(feature = "candle-cuda")]
fn candle_cuda_runtime_health() -> LensHealth {
    LensHealth::Loaded
}

#[cfg(not(feature = "candle-cuda"))]
fn candle_cuda_runtime_health() -> LensHealth {
    LensHealth::Failing {
        code: LENS_UNREACHABLE.to_string(),
        reason: CANDLE_CUDA_FEATURE_MISSING_REASON.to_string(),
    }
}

fn probe_http(endpoint: &str) -> LensHealth {
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return LensHealth::Failing {
            code: LENS_UNREACHABLE.to_string(),
            reason: "endpoint is not http://".to_string(),
        };
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
        .unwrap_or((authority, 80));
    let address = match (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    {
        Some(address) => address,
        None => {
            return LensHealth::Failing {
                code: LENS_UNREACHABLE.to_string(),
                reason: format!("{endpoint} resolved no socket address"),
            };
        }
    };
    match TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
        Ok(_) => LensHealth::Loaded,
        Err(err) => LensHealth::Failing {
            code: LENS_UNREACHABLE.to_string(),
            reason: format!("connect {endpoint} failed: {err}"),
        },
    }
}

fn command_exists(cmd: &str) -> bool {
    let path = PathBuf::from(cmd);
    if path.components().count() > 1 {
        return path.is_file();
    }
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .any(|dir| dir.join(cmd).is_file())
}
