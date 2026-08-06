use std::{env, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::ModelError;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub id: String,
    pub path: PathBuf,
    pub sha256: String,
    pub input_shape: Vec<usize>,
    pub class_map: Vec<String>,
}

impl ModelDescriptor {
    #[must_use]
    pub fn yolov10n_general(sha256: impl Into<String>, class_map: Vec<String>) -> Self {
        Self {
            id: "yolov10n_general".to_owned(),
            path: default_model_dir().join("yolov10n_general.onnx"),
            sha256: sha256.into(),
            input_shape: vec![1, 3, 640, 640],
            class_map,
        }
    }
}

#[must_use]
pub fn default_model_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("synapse")
        .join("models")
}

#[must_use]
pub fn model_download_failed(source: &str) -> ModelError {
    let source = source.trim();
    let detail = if source.is_empty() {
        "model download source was empty".to_owned()
    } else {
        format!("model downloads are disabled in M1; side-load a verified model from {source}")
    };
    ModelError::DownloadFailed { detail }
}

#[cfg(feature = "ort")]
pub fn local_ort_extensions_library() -> crate::ModelResult<PathBuf> {
    let path = default_model_dir()
        .join("ort-extensions")
        .join(crate::ORT_EXTENSIONS_WHISPER_FILENAME);
    let metadata = std::fs::metadata(&path).map_err(|err| crate::ModelError::LoadFailed {
        path: path.clone(),
        detail: format!("pinned ONNX Runtime Extensions library is missing: {err}"),
    })?;
    if metadata.len() != crate::ORT_EXTENSIONS_WHISPER_LENGTH {
        return Err(crate::ModelError::LoadFailed {
            path,
            detail: format!(
                "ONNX Runtime Extensions length mismatch: expected {}, got {}",
                crate::ORT_EXTENSIONS_WHISPER_LENGTH,
                metadata.len()
            ),
        });
    }
    let actual = crate::sha256_file(&path).map_err(|err| crate::ModelError::LoadFailed {
        path: path.clone(),
        detail: format!("failed to hash ONNX Runtime Extensions library: {err}"),
    })?;
    let expected = crate::normalize_sha256(crate::ORT_EXTENSIONS_WHISPER_SHA256);
    if actual != expected {
        return Err(crate::ModelError::HashMismatch {
            path,
            expected,
            actual,
        });
    }
    Ok(path)
}
