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
pub fn local_ort_extensions_library() -> Option<PathBuf> {
    let dir = default_model_dir()
        .join("ort-extensions")
        .join("wheel")
        .join("onnxruntime_extensions");
    let mut candidates = std::fs::read_dir(std::path::Path::new(&dir))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("_extensions_pydll"))
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("pyd"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next()
}
