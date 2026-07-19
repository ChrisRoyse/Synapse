use std::path::PathBuf;

use synapse_core::error_codes;
use thiserror::Error;

pub type StorageResult<T> = Result<T, StorageError>;

/// Storage failures with stable Synapse error codes.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage open failed for {path:?}: {detail}")]
    OpenFailed { path: PathBuf, detail: String },
    #[error("storage backend config invalid for {value:?}: {detail}")]
    BackendInvalidConfig { value: String, detail: String },
    #[error("storage backend {backend:?} unavailable: {detail}")]
    BackendUnavailable { backend: String, detail: String },
    #[error("storage write failed while encoding {type_name}: {source}")]
    EncodeJson {
        type_name: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("storage read failed while decoding {type_name}: {source}")]
    DecodeJson {
        type_name: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("storage write failed in {cf_name}: {detail}")]
    WriteFailed { cf_name: String, detail: String },
    #[error("Calyx storage write failed in {cf_name}: {detail}")]
    CalyxWriteFailed {
        cf_name: String,
        code: &'static str,
        detail: String,
    },
    #[error("storage write shed in {cf_name} under disk pressure {pressure_level}: {rows} rows")]
    WriteShed {
        cf_name: String,
        pressure_level: String,
        rows: usize,
    },
    #[error("storage GC refused unsafe eviction in {cf_name}: {detail}")]
    UnsafeGcEvictionRefused { cf_name: String, detail: String },
    #[error("storage read failed in {cf_name}: {detail}")]
    ReadFailed { cf_name: String, detail: String },
    #[error("Calyx storage read failed in {cf_name}: {detail}")]
    CalyxReadFailed {
        cf_name: String,
        code: &'static str,
        detail: String,
    },
    #[error("storage schema mismatch: expected {expected}, actual {actual}")]
    SchemaMismatch { expected: u32, actual: u32 },
}

impl StorageError {
    /// Returns the stable Synapse error code for this storage failure.
    #[tracing::instrument(skip_all, fields(storage_error = ?self))]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OpenFailed { .. } => error_codes::STORAGE_OPEN_FAILED,
            Self::BackendInvalidConfig { .. } => error_codes::STORAGE_BACKEND_INVALID_CONFIG,
            Self::BackendUnavailable { .. } => error_codes::STORAGE_BACKEND_UNIMPLEMENTED,
            Self::EncodeJson { .. } | Self::WriteFailed { .. } | Self::WriteShed { .. } => {
                error_codes::STORAGE_WRITE_FAILED
            }
            Self::CalyxWriteFailed { code, .. } | Self::CalyxReadFailed { code, .. } => code,
            Self::UnsafeGcEvictionRefused { .. } => error_codes::STORAGE_GC_UNSAFE_EVICTION_REFUSED,
            Self::DecodeJson { .. } | Self::ReadFailed { .. } => error_codes::STORAGE_READ_FAILED,
            Self::SchemaMismatch { .. } => error_codes::STORAGE_SCHEMA_MISMATCH,
        }
    }
}
