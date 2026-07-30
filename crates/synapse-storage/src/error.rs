use std::path::PathBuf;

use synapse_core::error_codes;
use thiserror::Error;

pub type StorageResult<T> = Result<T, StorageError>;

/// A guarded mutation has an empty, duplicate, or unmutated logical guard.
pub const STORAGE_REVISION_GUARD_INVALID: &str = "STORAGE_REVISION_GUARD_INVALID";
/// A guarded mutation cannot fit in one atomic Calyx WAL record.
pub const STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE: &str =
    "STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE";
/// Calyx returned a guarded-mutation outcome that violated the bridge contract.
pub const STORAGE_REVISION_GUARDED_OUTCOME_INVALID: &str =
    "STORAGE_REVISION_GUARDED_OUTCOME_INVALID";

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
    #[error("revision-guarded storage mutation failed in {cf_name} [{code}]: {detail}")]
    RevisionGuardedMutationFailed {
        cf_name: String,
        code: &'static str,
        detail: String,
    },
    #[error("Calyx storage write failed in {cf_name}: {detail}")]
    CalyxWriteFailed {
        cf_name: String,
        code: &'static str,
        detail: String,
        /// The substrate error's own remediation, kept as a field rather than
        /// only as text inside `detail` (#1911).
        ///
        /// `{code, message, remediation}` is the wire contract and `code`
        /// already travels here; folding the remediation into a formatted
        /// string meant every caller mapping this onto its own surface had
        /// nothing to forward, so each one substituted a generic sentence about
        /// a different fault.
        remediation: &'static str,
        /// Exact applied sequence when Calyx proved the failure happened after
        /// its irreversible commit boundary. `None` means no applied sequence
        /// was proven and callers must not infer one from the global tip.
        committed_seq: Option<u64>,
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
        /// The substrate error's own remediation (#1911). See
        /// [`StorageError::CalyxWriteFailed`] for why this is a field.
        remediation: &'static str,
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
            Self::RevisionGuardedMutationFailed { code, .. }
            | Self::CalyxWriteFailed { code, .. }
            | Self::CalyxReadFailed { code, .. } => code,
            Self::UnsafeGcEvictionRefused { .. } => error_codes::STORAGE_GC_UNSAFE_EVICTION_REFUSED,
            Self::DecodeJson { .. } | Self::ReadFailed { .. } => error_codes::STORAGE_READ_FAILED,
            Self::SchemaMismatch { .. } => error_codes::STORAGE_SCHEMA_MISMATCH,
        }
    }

    /// Returns the substrate error's own remediation, when this failure carries
    /// one (#1911).
    ///
    /// `None` for the variants raised by this crate, whose remediation belongs
    /// to whatever surface is reporting them — a caller can then tell "this
    /// failure has no specific fix to forward" apart from "the fix is empty"
    /// and fall back deliberately instead of by accident.
    #[must_use]
    pub const fn remediation(&self) -> Option<&'static str> {
        match self {
            Self::CalyxWriteFailed { remediation, .. }
            | Self::CalyxReadFailed { remediation, .. } => Some(*remediation),
            _ => None,
        }
    }

    /// Returns the exact sequence of an operation that failed after commit.
    ///
    /// This is typed commit-outcome metadata, not a sequence parsed from an
    /// error message and not a potentially unrelated latest-vault sequence.
    #[must_use]
    pub const fn committed_seq(&self) -> Option<u64> {
        match self {
            Self::CalyxWriteFailed { committed_seq, .. } => *committed_seq,
            _ => None,
        }
    }
}
