//! Durable online backup + restore-verification for the Synapse Calyx vault.
//!
//! This orchestrates the aster substrate's consistent-backup primitive
//! ([`calyx_aster::vault::AsterVault::backup_consistent`]) and its read-only
//! [`calyx_aster::verify_restore::verify_restore`] verifier into one
//! operator-facing, fail-closed flow:
//!
//! 1. Barrier the WAL group-committer so every accepted write is durable.
//! 2. Enforce vault residency: a pinned dataset refuses an off-dataset target
//!    (`CALYX_RESIDENCY_VIOLATION`) unless the pin explicitly allows it.
//! 3. Copy the sacred vault state under the native-compaction guard at a
//!    consistent `durable_seq` into `<target>/vault`.
//! 4. Immediately re-derive the copy with `verify_restore` and refuse to publish
//!    a manifest for a backup whose ledger chain does not verify or whose sacred
//!    rows do not read back — a green report is the backup's proof of integrity.
//! 5. Write `<target>/backup_manifest.json` (per-file SHA-256 + the verify
//!    report + the consistent point) and hash the manifest itself.

use std::path::{Path, PathBuf};

use calyx_aster::verify_restore::{VerifyRestoreReport, verify_restore};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::SynapseCalyxError;

/// Sub-directory of the backup target that holds the restorable vault copy.
pub const BACKUP_VAULT_SUBDIR: &str = "vault";
/// Sidecar manifest file name written at the backup target root.
pub const BACKUP_MANIFEST_FILE: &str = "backup_manifest.json";

const MANIFEST_REMEDIATION: &str =
    "inspect the backup target volume free space and permissions, then retry the backup";
const VERIFY_REMEDIATION: &str = "the freshly written backup failed byte-level restore verification; discard this target and \
     retry the backup, and inspect the source vault's ledger chain if it recurs";

/// One copied file plus its content hash.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxBackupFile {
    pub relative_path: String,
    pub len_bytes: u64,
    pub sha256: String,
}

/// Byte-level restore verification of the freshly written backup copy.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxVerifyReport {
    pub vault_path: PathBuf,
    pub success: bool,
    pub chain_intact: bool,
    pub constellation_count: u64,
    pub anchor_count: u64,
    pub ledger_entry_count: u64,
    pub ledger_tip_hash: String,
    pub wal_bytes_present: u64,
    pub first_cx_id: Option<String>,
    pub failure_reasons: Vec<String>,
}

impl SynapseCalyxVerifyReport {
    #[must_use]
    pub fn from_aster(report: &VerifyRestoreReport) -> Self {
        Self {
            vault_path: report.vault_path.clone(),
            success: report.success(),
            chain_intact: report.chain_intact,
            constellation_count: report.constellation_count,
            anchor_count: report.anchor_count,
            ledger_entry_count: report.ledger_entry_count,
            ledger_tip_hash: report.ledger_tip_hash.clone(),
            wal_bytes_present: report.wal_bytes_present,
            first_cx_id: report.first_cx_id.clone(),
            failure_reasons: report.failure_reasons(),
        }
    }
}

/// Structured, self-verifying backup result.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxBackupReport {
    pub vault_id: String,
    pub source_vault_dir: PathBuf,
    pub target_root: PathBuf,
    pub backup_vault_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub durable_seq: u64,
    pub latest_seq: u64,
    pub include_regenerable: bool,
    pub file_count: u64,
    pub total_bytes: u64,
    pub residency_enforced: bool,
    pub files: Vec<SynapseCalyxBackupFile>,
    pub verify: SynapseCalyxVerifyReport,
}

/// Manifest bytes written to `<target>/backup_manifest.json`. Kept separate from
/// [`SynapseCalyxBackupReport`] so the manifest never has to embed its own hash.
#[derive(Debug, Clone, Serialize)]
struct BackupManifest<'a> {
    schema_version: u32,
    vault_id: &'a str,
    source_vault_dir: &'a Path,
    backup_vault_dir: &'a Path,
    durable_seq: u64,
    latest_seq: u64,
    include_regenerable: bool,
    file_count: u64,
    total_bytes: u64,
    residency_enforced: bool,
    verify: &'a SynapseCalyxVerifyReport,
    files: &'a [SynapseCalyxBackupFile],
}

const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Runs the read-only aster restore verifier over a vault directory and maps its
/// report into the Synapse-typed structure.
///
/// # Errors
///
/// Returns a structured error when the path is not a readable Aster vault.
pub fn verify_vault_restore(
    vault_path: &Path,
) -> Result<SynapseCalyxVerifyReport, SynapseCalyxError> {
    let report = verify_restore(vault_path)
        .map_err(|error| SynapseCalyxError::from_calyx("verify restored Calyx vault", &error))?;
    Ok(SynapseCalyxVerifyReport::from_aster(&report))
}

/// Enforces vault residency for a backup target: a pinned dataset root refuses an
/// off-dataset target unless the pin explicitly permits it.
pub(crate) fn authorize_residency(
    vault_dir: &Path,
    target: &Path,
) -> Result<bool, SynapseCalyxError> {
    let Some(residency) = calyx_aster::residency::Residency::load(vault_dir)
        .map_err(|error| SynapseCalyxError::from_calyx("load vault residency pin", &error))?
    else {
        return Ok(false);
    };
    residency.authorize(target).map_err(|error| {
        SynapseCalyxError::from_calyx("authorize backup target residency", &error)
    })?;
    Ok(true)
}

/// Publishes the backup manifest and returns `(manifest_path, manifest_sha256)`.
pub(crate) fn write_manifest(
    target_root: &Path,
    report: &SynapseCalyxBackupReport,
) -> Result<(PathBuf, String), SynapseCalyxError> {
    let manifest = BackupManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        vault_id: &report.vault_id,
        source_vault_dir: &report.source_vault_dir,
        backup_vault_dir: &report.backup_vault_dir,
        durable_seq: report.durable_seq,
        latest_seq: report.latest_seq,
        include_regenerable: report.include_regenerable,
        file_count: report.file_count,
        total_bytes: report.total_bytes,
        residency_enforced: report.residency_enforced,
        verify: &report.verify,
        files: &report.files,
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_BACKUP_MANIFEST_ENCODE_FAILED",
            format!("encode backup manifest JSON: {error}"),
            MANIFEST_REMEDIATION,
        )
    })?;
    let manifest_path = target_root.join(BACKUP_MANIFEST_FILE);
    std::fs::write(&manifest_path, &bytes).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_BACKUP_MANIFEST_WRITE_FAILED",
            "write backup manifest",
            &manifest_path,
            &error,
            MANIFEST_REMEDIATION,
        )
    })?;
    Ok((manifest_path, sha256_hex(&bytes)))
}

/// Fails closed unless the freshly written backup passes byte-level restore
/// verification.
pub(crate) fn require_verified(verify: &SynapseCalyxVerifyReport) -> Result<(), SynapseCalyxError> {
    if verify.success {
        return Ok(());
    }
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_BACKUP_VERIFY_FAILED",
        format!(
            "backup at {} failed restore verification: chain_intact={} constellations={} \
             anchors={} wal_bytes={} reasons=[{}]",
            verify.vault_path.display(),
            verify.chain_intact,
            verify.constellation_count,
            verify.anchor_count,
            verify.wal_bytes_present,
            verify.failure_reasons.join("; ")
        ),
        VERIFY_REMEDIATION,
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from(hex_digit(byte >> 4)));
        out.push(char::from(hex_digit(byte & 0x0f)));
    }
    out
}

const fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + value - 10,
    }
}
