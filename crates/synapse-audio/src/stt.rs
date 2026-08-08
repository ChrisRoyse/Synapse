use std::{
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use calyx_forge::vram::{HostGpuReservation, HostGpuReservationRequest, HostGpuReservationStore};
use ort::session::Session;
use ort::value::{PrimitiveTensorElementType, Tensor};
use serde::{Deserialize, Serialize};
use synapse_models::{
    LoadedModel, ModelBackend, ModelDescriptor, ModelError, ModelLoader, SessionHandle,
    WHISPER_TINY_INT8_ONNX, WHISPER_TINY_INT8_ONNX_FILENAME, WHISPER_TINY_INT8_ONNX_LENGTH,
    WHISPER_TINY_INT8_ONNX_SHA256, default_model_dir,
};

mod window;

use window::{audio_seconds, wav_bytes_from_window};

use crate::{AudioError, AudioResult, AudioWindow};

pub const WHISPER_TINY_INT8_FILENAME: &str = WHISPER_TINY_INT8_ONNX_FILENAME;
pub const WHISPER_TINY_INT8_SHA256: &str = WHISPER_TINY_INT8_ONNX_SHA256;
/// Byte length of the pinned STT artifact.
///
/// Kept in step with `length` in `models/whisper-tiny-int8.pin.json`. Used only
/// as a cheap availability probe in health (#1863); every path that actually
/// loads the model verifies [`WHISPER_TINY_INT8_SHA256`] in full.
pub const WHISPER_TINY_INT8_EXPECTED_LEN: u64 = WHISPER_TINY_INT8_ONNX_LENGTH;

const SILENCE_RMS_DB: f32 = -70.0;
const DEFAULT_LANGUAGE: &str = "en";
/// `auto` (default) | `cuda` | `directml` | `cpu`.
///
/// Mirrors `SYNAPSE_DETECTION_BACKEND` so the two ORT consumers in the daemon
/// are configured the same way.
pub const STT_BACKEND_ENV: &str = "SYNAPSE_STT_BACKEND";
const STT_GPU_DEVICE_INDEX: u32 = 0;
/// Host GPU capacity declared for the whole life of a GPU-backed STT session.
///
/// Forge reserves the exact device buffers of each dispatch because forge
/// allocates and frees them per call. ORT does not work that way: the session
/// owns its device allocator, its cuDNN/cuBLAS workspaces and the whisper
/// weights from `commit_from_file` until the session is dropped, and this
/// session is built once and cached for the process lifetime. A per-inference
/// reservation would therefore claim capacity the session is already holding
/// and release capacity it has not given back, which is a lie in both
/// directions. A session-lifetime envelope is the only honest shape here.
///
/// The number is a measurement: manual FSV reads `nvidia-smi` before and after
/// session construction and refuses if the observed device delta exceeds this
/// envelope.
const STT_GPU_ADMISSION_MIB: u64 = 1_536;
// Exact prompt used by the pinned Olive graph's behavioral parity probe:
// decoder start followed by no-timestamps.
const EN_DECODER_PROMPT: [i32; 2] = [50_257, 50_362];
const AUDIO_STT_INFERENCES_TOTAL: &str = "audio_stt_inferences_total";
const AUDIO_STT_LATENCY_MS: &str = "audio_stt_latency_ms";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcription {
    pub text: String,
    pub confidence: f32,
    pub confidence_source: TranscriptionConfidenceSource,
    pub language: String,
    pub audio_seconds: f32,
    pub elapsed_ms: u128,
    pub model_path: PathBuf,
    pub backend: Option<ModelBackend>,
    pub session_id: Option<u64>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionConfidenceSource {
    NotApplicable,
    Model,
    Heuristic,
    #[default]
    Unsupported,
}

impl TranscriptionConfidenceSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Model => "model",
            Self::Heuristic => "heuristic",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Execution-provider policy for the pinned Whisper session.
///
/// `Auto` mirrors the Calyx math-backend policy (`crates/synapse-calyx/src/
/// math.rs`): prefer the GPU, and select CPU only when the failure is positive
/// proof that this host has no usable CUDA execution provider. A GPU that is
/// present but broken, an admission refusal, or any other load failure is a
/// hard error — never a silent demotion to CPU.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SttBackendPolicy {
    Auto,
    Pinned(ModelBackend),
}

/// What the process actually selected, for health and FSV readback.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SttBackendReadback {
    pub policy: String,
    pub loaded: bool,
    pub selected_backend: Option<ModelBackend>,
    pub gpu_reservation_id: Option<String>,
    pub gpu_reservation_mib: Option<u64>,
    /// Set when `Auto` demoted to CPU, with the proof that made it legal.
    pub fallback_code: Option<String>,
    pub fallback_detail: Option<String>,
}

struct LoadedStt {
    model: LoadedModel,
    /// Session-lifetime host GPU lease. Dropping this releases the ledger row,
    /// so it must outlive the ORT session it admitted.
    reservation: Option<HostGpuReservation>,
}

struct SttBackendFailure {
    error: AudioError,
    proves_gpu_absent: bool,
}

pub struct WhisperTinyStt {
    descriptor: ModelDescriptor,
    loaded: Mutex<Option<LoadedStt>>,
    fallback: Mutex<Option<(String, String)>>,
}

impl Debug for WhisperTinyStt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WhisperTinyStt")
            .field("descriptor", &self.descriptor)
            .field("loaded", &self.is_loaded())
            .finish_non_exhaustive()
    }
}

impl WhisperTinyStt {
    #[must_use]
    pub fn new(model_path: Option<PathBuf>) -> Self {
        let path = model_path.unwrap_or_else(default_model_path);
        Self {
            descriptor: ModelDescriptor {
                id: "whisper_tiny_int8".to_owned(),
                path,
                sha256: WHISPER_TINY_INT8_SHA256.to_owned(),
                input_shape: vec![1, 0],
                class_map: Vec::new(),
            },
            loaded: Mutex::new(None),
            fallback: Mutex::new(None),
        }
    }

    /// Reports the configured policy and, once a session exists, the provider
    /// it resolved to plus the GPU ledger row that admitted it.
    #[must_use]
    pub fn backend_readback(&self) -> SttBackendReadback {
        let policy = match stt_backend_policy() {
            Ok(SttBackendPolicy::Auto) => "auto".to_owned(),
            Ok(SttBackendPolicy::Pinned(backend)) => format!("pinned:{backend:?}"),
            Err(error) => format!("invalid:{error}"),
        };
        let (loaded, selected_backend, gpu_reservation_id, gpu_reservation_mib) =
            match self.loaded.lock() {
                Ok(guard) => guard.as_ref().map_or((false, None, None, None), |state| {
                    (
                        true,
                        Some(state.model.selected_backend()),
                        state
                            .reservation
                            .as_ref()
                            .map(|lease| lease.reservation_id().to_owned()),
                        state.reservation.as_ref().map(|_| STT_GPU_ADMISSION_MIB),
                    )
                }),
                Err(_poisoned) => (false, None, None, None),
            };
        let (fallback_code, fallback_detail) = self
            .fallback
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .map_or((None, None), |(code, detail)| (Some(code), Some(detail)));
        SttBackendReadback {
            policy,
            loaded,
            selected_backend,
            gpu_reservation_id,
            gpu_reservation_mib,
            fallback_code,
            fallback_detail,
        }
    }

    #[must_use]
    pub fn model_path(&self) -> &Path {
        &self.descriptor.path
    }

    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.loaded.lock().is_ok_and(|guard| guard.is_some())
    }

    /// Transcribes a WAV/encoded audio file.
    ///
    /// # Errors
    ///
    /// Returns structured model errors when the pinned model is missing,
    /// corrupted, rejected by ORT, or inference/output extraction fails.
    pub fn transcribe_file(
        &self,
        audio_path: impl AsRef<Path>,
        language: impl AsRef<str>,
    ) -> AudioResult<Transcription> {
        let bytes =
            fs::read(audio_path.as_ref()).map_err(|err| AudioError::LoopbackInitFailed {
                detail: format!(
                    "failed to read audio file {}: {err}",
                    audio_path.as_ref().display()
                ),
            })?;
        self.transcribe_bytes(bytes, language, 0.0)
    }

    /// Transcribes a captured audio window after 16 kHz mono conversion.
    ///
    /// # Errors
    ///
    /// Returns the same model and inference errors as [`Self::transcribe_file`].
    pub fn transcribe_window(
        &self,
        window: &AudioWindow,
        language: impl AsRef<str>,
    ) -> AudioResult<Transcription> {
        let seconds = audio_seconds(window);
        if window.frames == 0 || window.samples.is_empty() || window.rms_db <= SILENCE_RMS_DB {
            return Ok(self.blank(language, seconds));
        }
        self.transcribe_bytes(wav_bytes_from_window(window), language, seconds)
    }

    fn transcribe_bytes(
        &self,
        bytes: Vec<u8>,
        language: impl AsRef<str>,
        audio_seconds: f32,
    ) -> AudioResult<Transcription> {
        let language = normalize_language(language.as_ref())?;
        if bytes.is_empty() {
            return Ok(self.blank(language, audio_seconds));
        }

        let started = Instant::now();
        let result = (|| -> AudioResult<(String, ModelBackend, u64)> {
            let (backend, session_id, session) = self.load_session()?;
            let text = {
                let mut session = session.lock().map_err(|_| AudioError::ModelLoadFailed {
                    path: self.descriptor.path.clone(),
                    detail: "ORT session lock was poisoned".to_owned(),
                })?;
                self.run_session(&mut session, bytes)?
            };
            Ok((text, backend, session_id))
        })();
        let (text, backend, session_id) = match result {
            Ok(result) => {
                record_stt_inference("success", started.elapsed());
                result
            }
            Err(error) => {
                record_stt_inference("error", started.elapsed());
                return Err(error);
            }
        };

        Ok(Transcription {
            confidence: 0.0,
            confidence_source: TranscriptionConfidenceSource::Unsupported,
            text,
            language: language.to_owned(),
            audio_seconds,
            elapsed_ms: started.elapsed().as_millis(),
            model_path: self.descriptor.path.clone(),
            backend: Some(backend),
            session_id: Some(session_id),
        })
    }

    fn load_session(&self) -> AudioResult<(ModelBackend, u64, Arc<Mutex<Session>>)> {
        let mut loaded = self
            .loaded
            .lock()
            .map_err(|_| AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: "STT model cache lock was poisoned".to_owned(),
            })?;
        if loaded.is_none() {
            if self.descriptor.path == default_model_path() {
                let materialized = WHISPER_TINY_INT8_ONNX
                    .materialize_embedded()
                    .map_err(AudioError::from)?;
                if materialized.path != self.descriptor.path {
                    return Err(AudioError::ModelLoadFailed {
                        path: materialized.path,
                        detail: "embedded Whisper model materialized at an unexpected path"
                            .to_owned(),
                    });
                }
            } else if !self.descriptor.path.exists() {
                return Err(AudioError::SttModelNotLoaded {
                    detail: format!(
                        "STT model does not exist at {}",
                        self.descriptor.path.display()
                    ),
                });
            }
            *loaded = Some(self.build_session()?);
        }

        let state = loaded
            .as_ref()
            .ok_or_else(|| AudioError::SttModelNotLoaded {
                detail: "STT model cache was empty after load".to_owned(),
            })?;
        let SessionHandle::Ort(session) = state.model.session() else {
            return Err(AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: "STT model loaded without an ORT session".to_owned(),
            });
        };
        let backend = state.model.selected_backend();
        let session_id = state.model.session_id();
        let session = Arc::clone(session);
        drop(loaded);
        Ok((backend, session_id, session))
    }

    /// Builds the one persistent ORT session under the configured provider
    /// policy (#2109).
    ///
    /// The old code passed `vec![ModelBackend::Cpu]`, a single-element list that
    /// bypassed provider selection entirely on a host whose CUDA EP is compiled
    /// in and whose GPU is idle.
    fn build_session(&self) -> AudioResult<LoadedStt> {
        if let Ok(mut guard) = self.fallback.lock() {
            *guard = None;
        }
        match stt_backend_policy()? {
            SttBackendPolicy::Pinned(backend) => {
                self.load_with_backend(backend).map_err(|failure| {
                    tracing::error!(
                        code = "SYNAPSE_STT_PINNED_BACKEND_UNAVAILABLE",
                        backend = ?backend,
                        error = %failure.error,
                        "the pinned STT execution provider could not build a session"
                    );
                    failure.error
                })
            }
            SttBackendPolicy::Auto => match self.load_with_backend(ModelBackend::Cuda) {
                Ok(state) => Ok(state),
                Err(failure) if failure.proves_gpu_absent => {
                    let code = "SYNAPSE_STT_AUTO_CPU_NO_CUDA_PROVIDER";
                    let detail = failure.error.to_string();
                    // Loud, recorded, and readable back: an `auto` demotion is
                    // announced, never silent.
                    tracing::warn!(
                        code,
                        source_code = failure.error.code(),
                        source_error = %failure.error,
                        "STT auto policy selected the CPU execution provider because this host \
                         proves it has no usable CUDA execution provider"
                    );
                    if let Ok(mut guard) = self.fallback.lock() {
                        *guard = Some((code.to_owned(), detail));
                    }
                    self.load_with_backend(ModelBackend::Cpu)
                        .map_err(|cpu_failure| cpu_failure.error)
                }
                Err(failure) => {
                    tracing::error!(
                        code = "SYNAPSE_STT_CUDA_SESSION_FAILED",
                        source_code = failure.error.code(),
                        error = %failure.error,
                        "STT refused to demote to CPU: the CUDA failure is not proof that this \
                         host lacks a CUDA execution provider"
                    );
                    Err(failure.error)
                }
            },
        }
    }

    fn load_with_backend(
        &self,
        backend: ModelBackend,
    ) -> Result<LoadedStt, Box<SttBackendFailure>> {
        let reservation = self.acquire_gpu_reservation(backend).map_err(|error| {
            Box::new(SttBackendFailure {
                error,
                // Admission backpressure is capacity, not absence. Refuse.
                proves_gpu_absent: false,
            })
        })?;
        match ModelLoader::new(vec![backend]).load(self.descriptor.clone()) {
            Ok(model) => Ok(LoadedStt { model, reservation }),
            Err(error) => {
                let proves_gpu_absent = model_error_proves_gpu_absent(&error);
                // `reservation` drops here, releasing the ledger row we did not
                // end up using.
                Err(Box::new(SttBackendFailure {
                    error: AudioError::from(error),
                    proves_gpu_absent,
                }))
            }
        }
    }

    /// Registers the session's declared device envelope with the OS-wide Calyx
    /// GPU reservation ledger before ORT can allocate anything.
    fn acquire_gpu_reservation(
        &self,
        backend: ModelBackend,
    ) -> AudioResult<Option<HostGpuReservation>> {
        if backend == ModelBackend::Cpu {
            return Ok(None);
        }
        let store = HostGpuReservationStore::from_env(STT_GPU_DEVICE_INDEX).map_err(|error| {
            AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: format!(
                    "open device-{STT_GPU_DEVICE_INDEX} host GPU reservation SoT for the STT \
                     session: {error}"
                ),
            }
        })?;
        store
            .acquire(stt_gpu_reservation_request(backend))
            .map(Some)
            .map_err(|error| AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: format!(
                    "device-{STT_GPU_DEVICE_INDEX} host GPU admission refused the \
                     {STT_GPU_ADMISSION_MIB} MiB STT session envelope: {error}"
                ),
            })
    }

    fn run_session(&self, session: &mut Session, bytes: Vec<u8>) -> AudioResult<String> {
        let audio_tensor = self.tensor("audio_stream", [1, bytes.len()], bytes)?;
        let max_length = self.tensor("max_length", [1], vec![96_i32])?;
        let min_length = self.tensor("min_length", [1], vec![0_i32])?;
        let num_beams = self.tensor("num_beams", [1], vec![1_i32])?;
        let num_return_sequences = self.tensor("num_return_sequences", [1], vec![1_i32])?;
        let length_penalty = self.tensor("length_penalty", [1], vec![1.0_f32])?;
        let repetition_penalty = self.tensor("repetition_penalty", [1], vec![1.0_f32])?;
        let decoder_input_ids = self.tensor(
            "decoder_input_ids",
            [1, EN_DECODER_PROMPT.len()],
            EN_DECODER_PROMPT.to_vec(),
        )?;
        let outputs = session
            .run(ort::inputs! {
                "audio_stream" => audio_tensor,
                "max_length" => max_length,
                "min_length" => min_length,
                "num_beams" => num_beams,
                "num_return_sequences" => num_return_sequences,
                "length_penalty" => length_penalty,
                "repetition_penalty" => repetition_penalty,
                "decoder_input_ids" => decoder_input_ids,
            })
            .map_err(|err| AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: format!("STT inference failed: {err}"),
            })?;
        let text = outputs
            .get("str")
            .ok_or_else(|| AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: "STT model did not return `str` output".to_owned(),
            })?
            .try_extract_strings()
            .map_err(|err| AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: format!("STT output extraction failed: {err}"),
            })?
            .1
            .into_iter()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        Ok(text)
    }

    fn tensor<T, const N: usize>(
        &self,
        name: &str,
        shape: [usize; N],
        data: Vec<T>,
    ) -> AudioResult<Tensor<T>>
    where
        T: PrimitiveTensorElementType + Debug + Clone + 'static,
    {
        Tensor::from_array((shape, data.into_boxed_slice()))
            .map_err(|err| self.infer_error(format!("failed to create {name} tensor: {err}")))
    }

    fn blank(&self, language: impl AsRef<str>, audio_seconds: f32) -> Transcription {
        metrics::counter!(AUDIO_STT_INFERENCES_TOTAL, "outcome" => "not_applicable").increment(1);
        Transcription {
            text: String::new(),
            confidence: 0.0,
            confidence_source: TranscriptionConfidenceSource::NotApplicable,
            language: language.as_ref().trim().to_owned(),
            audio_seconds,
            elapsed_ms: 0,
            model_path: self.descriptor.path.clone(),
            backend: None,
            session_id: None,
        }
    }

    fn infer_error(&self, detail: String) -> AudioError {
        AudioError::ModelLoadFailed {
            path: self.descriptor.path.clone(),
            detail,
        }
    }
}

fn record_stt_inference(outcome: &'static str, elapsed: std::time::Duration) {
    metrics::counter!(AUDIO_STT_INFERENCES_TOTAL, "outcome" => outcome).increment(1);
    metrics::histogram!(AUDIO_STT_LATENCY_MS).record(elapsed.as_secs_f64() * 1000.0);
}

#[must_use]
pub fn default_model_path() -> PathBuf {
    default_model_dir().join(WHISPER_TINY_INT8_FILENAME)
}

/// The exact host GPU admission request a GPU-backed STT session makes.
///
/// Public so a field verification can acquire the identical row against the
/// live ledger rather than a hand-rolled approximation of it.
#[must_use]
pub fn stt_gpu_reservation_request(backend: ModelBackend) -> HostGpuReservationRequest {
    HostGpuReservationRequest::new(
        "synapse-audio-stt",
        format!("synapse-stt-pid-{}", std::process::id()),
        format!(
            "ORT {backend:?} whisper-tiny-int8 session; \
             declared_session_lifetime_envelope_mib={STT_GPU_ADMISSION_MIB}"
        ),
        STT_GPU_ADMISSION_MIB,
    )
}

/// Declared session-lifetime GPU envelope in MiB.
#[must_use]
pub const fn stt_gpu_admission_mib() -> u64 {
    STT_GPU_ADMISSION_MIB
}

/// Reads [`STT_BACKEND_ENV`]. Unset or `auto` means the GPU-preferring policy.
///
/// # Errors
///
/// Returns `MODEL_LOAD_FAILED` for an unrecognized or non-Unicode value rather
/// than silently defaulting — a misconfigured provider must be visible.
pub fn stt_backend_policy() -> AudioResult<SttBackendPolicy> {
    let value = match std::env::var(STT_BACKEND_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(SttBackendPolicy::Auto),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(AudioError::ModelLoadFailed {
                path: default_model_path(),
                detail: format!("{STT_BACKEND_ENV} is not valid Unicode"),
            });
        }
    };
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(SttBackendPolicy::Auto);
    }
    if trimmed.eq_ignore_ascii_case("cuda") {
        return Ok(SttBackendPolicy::Pinned(ModelBackend::Cuda));
    }
    if trimmed.eq_ignore_ascii_case("directml") {
        return Ok(SttBackendPolicy::Pinned(ModelBackend::DirectMl));
    }
    if trimmed.eq_ignore_ascii_case("cpu") {
        return Ok(SttBackendPolicy::Pinned(ModelBackend::Cpu));
    }
    Err(AudioError::ModelLoadFailed {
        path: default_model_path(),
        detail: format!("{STT_BACKEND_ENV} must be auto, cuda, directml, or cpu; got {value:?}"),
    })
}

/// Whether an ORT session failure is positive proof that this host has no
/// usable GPU execution provider, which is the only justification for `auto`
/// resolving to CPU.
///
/// Deliberately narrow. "The provider DLL is not on this host" and "the driver
/// reports no CUDA device" are absence. Everything else — an EP that loaded and
/// then failed, an out-of-memory, a rejected graph — is a present-but-broken
/// GPU, and uncertainty is not evidence.
fn model_error_proves_gpu_absent(error: &ModelError) -> bool {
    let ModelError::BackendUnavailable { failures, .. } = error else {
        return false;
    };
    failures.iter().any(|(_backend, detail)| {
        let detail = detail.to_ascii_lowercase();
        detail.contains("no cuda-capable device")
            || detail.contains("cuda_error_no_device")
            || detail.contains("cudaerrornodevice")
            || detail.contains("onnxruntime_providers_cuda")
            || detail.contains("directml.dll")
            || (detail.contains("libraries are not found") && detail.contains("cuda"))
    })
}

fn normalize_language(language: &str) -> AudioResult<&str> {
    let language = language.trim();
    let language = if language.is_empty() {
        DEFAULT_LANGUAGE
    } else {
        language
    };
    if language.eq_ignore_ascii_case(DEFAULT_LANGUAGE) {
        Ok(DEFAULT_LANGUAGE)
    } else {
        Err(AudioError::LoopbackInitFailed {
            detail: format!("unsupported STT language `{language}`; only `en` is wired in M3"),
        })
    }
}
