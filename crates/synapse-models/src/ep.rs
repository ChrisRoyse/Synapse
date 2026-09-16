use serde::{Deserialize, Serialize};

use crate::ModelBackend::{Cpu, Cuda, DirectMl};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelBackend {
    Cuda,
    DirectMl,
    #[default]
    Cpu,
}

#[must_use]
pub fn default_provider_order() -> Vec<ModelBackend> {
    vec![Cuda, DirectMl, Cpu]
}

#[cfg(feature = "ort")]
pub fn create_ort_session(
    descriptor: &crate::ModelDescriptor,
    provider: ModelBackend,
) -> crate::ModelResult<ort::session::Session> {
    use ort::{ep, session::Session};

    let mut builder = Session::builder().map_err(|err| crate::ModelError::LoadFailed {
        path: descriptor.path.clone(),
        detail: err.to_string(),
    })?;
    if descriptor.id == "whisper_tiny_int8" {
        if let Some(library) = crate::download::local_ort_extensions_library() {
            builder = builder.with_operator_library(&library).map_err(|err| {
                crate::ModelError::LoadFailed {
                    path: descriptor.path.clone(),
                    detail: format!(
                        "failed to register ONNX Runtime extensions library {}: {err}",
                        library.display()
                    ),
                }
            })?;
        } else {
            builder = builder.with_extensions().map_err(|err| {
                crate::ModelError::LoadFailed {
                    path: descriptor.path.clone(),
                    detail: format!(
                        "ONNX Runtime extensions unavailable and no local extension library found: {err}"
                    ),
                }
            })?;
        }
    }
    let execution_provider = match provider {
        Cuda => ep::CUDA::default().build().error_on_failure(),
        DirectMl => ep::DirectML::default().build().error_on_failure(),
        Cpu => ep::CPU::default()
            .with_arena_allocator(false)
            .build()
            .error_on_failure(),
    };
    builder = match builder.with_execution_providers([execution_provider]) {
        Ok(builder) => builder,
        Err(err) => {
            tracing::warn!(backend = ?provider, error = %err, "execution provider unavailable");
            return Err(crate::ModelError::BackendUnavailable {
                attempted: vec![provider],
            });
        }
    };
    builder
        .commit_from_file(&descriptor.path)
        .map_err(|err| crate::ModelError::LoadFailed {
            path: descriptor.path.clone(),
            detail: err.to_string(),
        })
}
