use std::path::PathBuf;

use synapse_core::error_codes;
use thiserror::Error;

use crate::ModelBackend;

pub type ModelResult<T> = Result<T, ModelError>;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model download failed: {detail}")]
    DownloadFailed { detail: String },
    #[error("model hash mismatch for {path}: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("model load failed for {path}: {detail}")]
    LoadFailed { path: PathBuf, detail: String },
    #[error("no model backend was available; attempted {attempted:?}")]
    BackendUnavailable { attempted: Vec<ModelBackend> },
    #[error("detection model is not loaded: {detail}")]
    DetectionModelNotLoaded { detail: String },
    #[error("no detection frame available: {detail}")]
    DetectionNoFrame { detail: String },
    #[error("detection inference failed: {detail}")]
    DetectionInferFailed { detail: String },
}

impl ModelError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::DownloadFailed { .. } => error_codes::MODEL_DOWNLOAD_FAILED,
            Self::HashMismatch { .. } => error_codes::MODEL_HASH_MISMATCH,
            Self::LoadFailed { .. } => error_codes::MODEL_LOAD_FAILED,
            Self::BackendUnavailable { .. } => error_codes::MODEL_BACKEND_UNAVAILABLE,
            Self::DetectionModelNotLoaded { .. } => error_codes::DETECTION_MODEL_NOT_LOADED,
            Self::DetectionNoFrame { .. } => error_codes::DETECTION_NO_FRAME,
            Self::DetectionInferFailed { .. } => error_codes::DETECTION_MODEL_INFER_FAILED,
        }
    }
}

#[must_use]
pub fn detection_model_not_loaded(detail: impl Into<String>) -> ModelError {
    ModelError::DetectionModelNotLoaded {
        detail: detail.into(),
    }
}

#[must_use]
pub fn detection_no_frame(detail: impl Into<String>) -> ModelError {
    ModelError::DetectionNoFrame {
        detail: detail.into(),
    }
}

#[must_use]
pub fn detection_infer_failed(detail: impl Into<String>) -> ModelError {
    ModelError::DetectionInferFailed {
        detail: detail.into(),
    }
}
