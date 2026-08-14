use calyx_core::{CalyxError, CalyxErrorCode};

use crate::{DomainId, OracleError};

/// Whole-domain Oracle reads are cold persistent scans, not point reads. Keep
/// one bounded, caller-owned snapshot for the complete multi-CF operation and
/// release it as soon as the corpus is assembled.
pub(crate) const ORACLE_CORPUS_READER_LEASE_MS: u64 = 5 * 60_000;

pub(crate) enum ScanError {
    Storage(CalyxError),
    Oracle(OracleError),
}

impl From<CalyxError> for ScanError {
    fn from(error: CalyxError) -> Self {
        Self::Storage(error)
    }
}

impl From<OracleError> for ScanError {
    fn from(error: OracleError) -> Self {
        Self::Oracle(error)
    }
}

pub(crate) fn storage_read(
    error: &CalyxError,
    domain: &DomainId,
    operation: &'static str,
) -> OracleError {
    OracleError::StorageReadFailure {
        domain: domain.clone(),
        operation,
        source_code: error.code.to_owned(),
        source_message: error.message.clone(),
        source_remediation: error.remediation.to_owned(),
    }
}

pub(crate) fn corrupt(domain: &DomainId, evidence: &'static str) -> OracleError {
    OracleError::EvidenceCorrupt {
        domain: domain.clone(),
        evidence,
    }
}

pub(crate) fn recurrence_read(error: CalyxError, domain: &DomainId) -> OracleError {
    if error.code == CalyxErrorCode::AsterCorruptShard.code() {
        corrupt(domain, "recurrence series")
    } else {
        storage_read(&error, domain, "read recurrence series")
    }
}

pub(crate) fn scan_read(
    error: ScanError,
    domain: &DomainId,
    operation: &'static str,
) -> OracleError {
    match error {
        ScanError::Storage(error) => storage_read(&error, domain, operation),
        ScanError::Oracle(error) => error,
    }
}
