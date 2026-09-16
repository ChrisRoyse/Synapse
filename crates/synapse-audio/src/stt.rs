use std::{
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use ort::session::Session;
use ort::value::{PrimitiveTensorElementType, Tensor};
use serde::{Deserialize, Serialize};
use synapse_models::{
    LoadedModel, ModelBackend, ModelDescriptor, ModelLoader, SessionHandle, default_model_dir,
};

mod window;

use window::{audio_seconds, wav_bytes_from_window};

use crate::{AudioError, AudioResult, AudioWindow};

pub const WHISPER_TINY_INT8_FILENAME: &str = "whisper-tiny-int8.onnx";
pub const WHISPER_TINY_INT8_SHA256: &str =
    "147afac751f89ad8e8f82133464edc81ecff9391e98ccdcae2474384be68ec86";

const SILENCE_RMS_DB: f32 = -70.0;
const DEFAULT_LANGUAGE: &str = "en";
const EN_DECODER_PROMPT: [i32; 4] = [50_258, 50_259, 50_359, 50_363];
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

#[derive(Debug)]
pub struct WhisperTinyStt {
    descriptor: ModelDescriptor,
    loader: ModelLoader,
    loaded: Mutex<Option<LoadedModel>>,
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
            loader: ModelLoader::new(vec![ModelBackend::Cpu]),
            loaded: Mutex::new(None),
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
            if !self.descriptor.path.exists() {
                return Err(AudioError::SttModelNotLoaded {
                    detail: format!(
                        "side-load {} before calling audio STT",
                        self.descriptor.path.display()
                    ),
                });
            }
            *loaded = Some(self.loader.load(self.descriptor.clone())?);
        }

        let model = loaded
            .as_ref()
            .ok_or_else(|| AudioError::SttModelNotLoaded {
                detail: "STT model cache was empty after load".to_owned(),
            })?;
        let SessionHandle::Ort(session) = model.session() else {
            return Err(AudioError::ModelLoadFailed {
                path: self.descriptor.path.clone(),
                detail: "STT model loaded without an ORT session".to_owned(),
            });
        };
        let backend = model.selected_backend();
        let session_id = model.session_id();
        let session = Arc::clone(session);
        drop(loaded);
        Ok((backend, session_id, session))
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
