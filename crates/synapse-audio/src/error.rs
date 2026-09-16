use std::path::PathBuf;

use synapse_core::error_codes;
use synapse_models::{ModelBackend, ModelError};
use thiserror::Error;

pub type AudioResult<T> = Result<T, AudioError>;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum AudioError {
    #[error("audio device lost: {detail}")]
    DeviceLost { detail: String },
    #[error("audio loopback init failed: {detail}")]
    LoopbackInitFailed { detail: String },
    #[error("audio STT model not loaded: {detail}")]
    SttModelNotLoaded { detail: String },
    #[error("audio STT model hash mismatch for {path}: expected {expected}, got {actual}")]
    ModelHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("audio STT model load failed for {path}: {detail}")]
    ModelLoadFailed { path: PathBuf, detail: String },
    #[error("audio STT model backend unavailable; attempted {attempted:?}")]
    ModelBackendUnavailable { attempted: Vec<ModelBackend> },
}

impl AudioError {
    #[must_use]
    #[tracing::instrument(skip_all, fields(audio_error = ?self))]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DeviceLost { .. } => error_codes::AUDIO_DEVICE_LOST,
            Self::LoopbackInitFailed { .. } => error_codes::AUDIO_LOOPBACK_INIT_FAILED,
            Self::SttModelNotLoaded { .. } => error_codes::AUDIO_STT_MODEL_NOT_LOADED,
            Self::ModelHashMismatch { .. } => error_codes::MODEL_HASH_MISMATCH,
            Self::ModelLoadFailed { .. } => error_codes::MODEL_LOAD_FAILED,
            Self::ModelBackendUnavailable { .. } => error_codes::MODEL_BACKEND_UNAVAILABLE,
        }
    }
}

impl From<ModelError> for AudioError {
    fn from(error: ModelError) -> Self {
        match error {
            ModelError::HashMismatch {
                path,
                expected,
                actual,
            } => Self::ModelHashMismatch {
                path,
                expected,
                actual,
            },
            ModelError::LoadFailed { path, detail } => Self::ModelLoadFailed { path, detail },
            ModelError::BackendUnavailable { attempted } => {
                Self::ModelBackendUnavailable { attempted }
            }
            other => Self::ModelLoadFailed {
                path: PathBuf::from("<unknown>"),
                detail: other.to_string(),
            },
        }
    }
}
