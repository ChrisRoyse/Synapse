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
//! 3. Pin the vault's `CURRENT` manifest generation and copy the sacred vault
//!    state that snapshot names, under the native-compaction guard, at a
//!    consistent `durable_seq` into `<target>/vault`. The live daemon rotates a
//!    manifest generation every few seconds, so the copy is defined by the
//!    pinned snapshot rather than by a directory listing — a listing-based copy
//!    of a live vault loses that race eventually and certainly.
//! 4. Immediately re-derive the copy with `verify_restore` and refuse to publish
//!    a manifest for a backup whose ledger chain does not verify or whose sacred
//!    rows do not read back — a green report is the backup's proof of integrity.
//! 5. Copy the vault lineage journal in as a sidecar so the backup carries the
//!    vault's identity, not just its bytes.
//! 6. Write `<target>/backup_manifest.json` (per-file SHA-256 + the verify
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
/// Durable marker proving a target is still under construction.
pub const BACKUP_IN_PROGRESS_FILE: &str = "backup_in_progress.json";
/// Sidecar copy of the vault lineage journal, written at the backup target root.
///
/// The live journal is deliberately a *sibling* of the vault directory (#1875)
/// so deleting the vault cannot delete its own witness. That placement means a
/// vault-tree copy walks straight past it, so a backup captured the vault's
/// bytes and none of its identity. It is copied in explicitly here, next to the
/// manifest rather than inside `vault/`, so restoring `vault/` never drags a
/// foreign journal into place and the operator has to make the identity
/// decision consciously.
pub const BACKUP_LINEAGE_FILE: &str = "vault_lineage.json";

const MANIFEST_REMEDIATION: &str =
    "inspect the backup target volume free space and permissions, then retry the backup";
const LINEAGE_REMEDIATION: &str = "the vault lineage journal is the only record of vault identity that survives deleting the \
     vault directory, so a backup without it cannot prove which vault it restores; repair or \
     restore the journal named in this error beside the vault directory, then retake the backup";
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

/// One source-vault entry that was deliberately not copied because it is
/// runtime state of the process serving the vault rather than vault data.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxBackupExclusion {
    pub relative_path: String,
    pub reason: String,
}

/// One entry that was enumerated in the source vault, found absent when the copy
/// reached it, and tolerated on a stated justification.
///
/// A restore operator must be able to tell "this file is not here because the
/// live vault legitimately reclaimed it and nothing in the restored vault can
/// reach it" from "this file is not here and nobody looked". Only the first is
/// ever recorded here; everything else fails the backup closed.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxBackupToleratedAbsence {
    pub relative_path: String,
    pub reason: String,
}

/// The `CURRENT`-pinned manifest snapshot this backup restores to.
///
/// The live vault rotates its manifest generations continuously, so a backup
/// that enumerated the directory and then copied what it enumerated would
/// eventually always lose that race. The snapshot is pinned at one publication
/// boundary instead, and this records which point the backup is.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxBackupPinnedManifest {
    pub pointer: String,
    pub manifest_seq: u64,
    pub durable_seq: u64,
    pub generations_retained: u64,
    /// Vault-relative assets the pinned generation references. All were copied;
    /// any absence would have failed the backup.
    pub referenced_paths: Vec<String>,
    /// Where `CURRENT` had moved to by the end of the copy, when it moved. The
    /// pinned snapshot stays valid and is not retaken: restoring to a slightly
    /// older consistent point is what a backup is.
    pub current_advanced_to: Option<String>,
}

/// The vault lineage journal captured beside the backup, with the identity it
/// attests. Without this a backup proves only that *some* vault restored, never
/// *which* one (#1875).
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxBackupLineage {
    /// Path of the live journal that was copied.
    pub source_path: PathBuf,
    /// Path of the sidecar copy inside the backup target.
    pub sidecar_path: PathBuf,
    /// Name of the sidecar relative to the backup target root.
    pub relative_path: String,
    pub len_bytes: u64,
    pub sha256: String,
    /// 1-based lineage generation of the vault at backup time.
    pub generation: u64,
    /// Recorded vault replacements preceding this generation.
    pub reset_count: u64,
    /// `vault-genesis` | `lineage-seeded` | `post-reset`.
    pub chain_origin: String,
    /// False whenever the chain begins after a recorded replacement or before
    /// the journal existed: the backup attests the surviving chain, not the
    /// vault's whole history.
    pub covers_full_history: bool,
    /// Why coverage is what it is (#1884), so a restored backup states where its
    /// attested history begins instead of only that it is not the whole vault.
    pub history_coverage: String,
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
    /// Runtime artifacts present in the source vault and deliberately skipped.
    pub excluded_runtime: Vec<SynapseCalyxBackupExclusion>,
    /// The consistent point this backup pins, absent only for a vault that has
    /// never published a manifest.
    pub pinned_manifest: Option<SynapseCalyxBackupPinnedManifest>,
    /// Entries that vanished between enumeration and copy and were tolerated by
    /// name and stated justification. Empty on a quiescent vault.
    pub tolerated_absences: Vec<SynapseCalyxBackupToleratedAbsence>,
    pub lineage: SynapseCalyxBackupLineage,
    pub verify: SynapseCalyxVerifyReport,
}

/// The substrate copy report translated into Synapse-typed values, so the
/// operator-facing flow stays a sequence of named steps rather than a field map.
pub(crate) struct CopiedVaultState {
    pub(crate) source_vault_dir: PathBuf,
    pub(crate) durable_seq: u64,
    pub(crate) file_count: u64,
    pub(crate) total_bytes: u64,
    pub(crate) files: Vec<SynapseCalyxBackupFile>,
    pub(crate) excluded_runtime: Vec<SynapseCalyxBackupExclusion>,
    pub(crate) pinned_manifest: Option<SynapseCalyxBackupPinnedManifest>,
    pub(crate) tolerated_absences: Vec<SynapseCalyxBackupToleratedAbsence>,
}

impl CopiedVaultState {
    pub(crate) fn from_aster(report: calyx_aster::vault::AsterBackupReport) -> Self {
        Self {
            source_vault_dir: report.source_vault_dir,
            durable_seq: report.durable_seq,
            file_count: report.file_count,
            total_bytes: report.total_bytes,
            files: report
                .files
                .into_iter()
                .map(|file| SynapseCalyxBackupFile {
                    relative_path: file.relative_path,
                    len_bytes: file.len_bytes,
                    sha256: file.sha256,
                })
                .collect(),
            excluded_runtime: report
                .excluded_runtime
                .into_iter()
                .map(|entry| SynapseCalyxBackupExclusion {
                    relative_path: entry.relative_path,
                    reason: entry.reason.to_owned(),
                })
                .collect(),
            pinned_manifest: report
                .pinned_manifest
                .map(|pin| SynapseCalyxBackupPinnedManifest {
                    pointer: pin.pointer,
                    manifest_seq: pin.manifest_seq,
                    durable_seq: pin.durable_seq,
                    generations_retained: pin.generations_retained as u64,
                    referenced_paths: pin.referenced_paths,
                    current_advanced_to: pin.current_advanced_to,
                }),
            tolerated_absences: report
                .tolerated_absences
                .into_iter()
                .map(|entry| SynapseCalyxBackupToleratedAbsence {
                    relative_path: entry.relative_path,
                    reason: entry.reason.to_owned(),
                })
                .collect(),
        }
    }
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
    lineage: &'a SynapseCalyxBackupLineage,
    pinned_manifest: Option<&'a SynapseCalyxBackupPinnedManifest>,
    excluded_runtime: &'a [SynapseCalyxBackupExclusion],
    tolerated_absences: &'a [SynapseCalyxBackupToleratedAbsence],
    files: &'a [SynapseCalyxBackupFile],
}

/// Manifest schema version.
///
/// * `1` — per-file SHA-256 digests, consistent point, verify report.
/// * `2` — adds the `lineage` object (the vault identity the backup restores)
///   and `excluded_runtime` (the runtime artifacts deliberately not copied).
/// * `3` — adds `pinned_manifest` (the exact `CURRENT` generation the backup is
///   a snapshot of, and everything it references) and `tolerated_absences` (what
///   vanished mid-copy and on what justification it was survivable).
///
/// Bumped rather than extended in place for the same reason `PostgreSQL` moved its
/// backup manifest to version `2` when it added `System-Identifier`: a reader
/// must be able to tell a manifest that *asserts* an identity from one that
/// merely never recorded it. The same argument applies to a tolerated absence: a
/// v2 manifest is silent about mid-copy rotation because it could not survive
/// one, and a v3 manifest with an empty `tolerated_absences` is a positive
/// assertion that nothing vanished.
const MANIFEST_SCHEMA_VERSION: u32 = 3;

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
        lineage: &report.lineage,
        pinned_manifest: report.pinned_manifest.as_ref(),
        excluded_runtime: &report.excluded_runtime,
        tolerated_absences: &report.tolerated_absences,
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

/// Copies the vault lineage journal into the backup target as
/// [`BACKUP_LINEAGE_FILE`] and hashes it, so the backup records *which* vault it
/// restores and at what history.
///
/// Fails closed when the journal is absent or unreadable: a backup that cannot
/// name its vault is exactly the artifact that made #1875 unrecoverable, and
/// publishing one silently would repeat that failure.
pub(crate) fn capture_lineage(
    target_root: &Path,
    lineage: &crate::SynapseCalyxVaultLineage,
) -> Result<SynapseCalyxBackupLineage, SynapseCalyxError> {
    let source_path = lineage.lineage_path.clone();
    let bytes = std::fs::read(&source_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_BACKUP_LINEAGE_MISSING",
                format!(
                    "vault lineage journal {} does not exist, so this backup cannot record which \
                     vault it restores",
                    source_path.display()
                ),
                LINEAGE_REMEDIATION,
            )
        } else {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_BACKUP_LINEAGE_READ_FAILED",
                "read vault lineage journal for backup",
                &source_path,
                &error,
                LINEAGE_REMEDIATION,
            )
        }
    })?;
    let sidecar_path = target_root.join(BACKUP_LINEAGE_FILE);
    std::fs::write(&sidecar_path, &bytes).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_BACKUP_LINEAGE_WRITE_FAILED",
            "write backup vault lineage sidecar",
            &sidecar_path,
            &error,
            MANIFEST_REMEDIATION,
        )
    })?;
    Ok(SynapseCalyxBackupLineage {
        source_path,
        sidecar_path,
        relative_path: BACKUP_LINEAGE_FILE.to_owned(),
        len_bytes: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
        generation: lineage.generation,
        reset_count: lineage.reset_count,
        chain_origin: lineage.chain_origin.clone(),
        covers_full_history: lineage.chain_covers_full_history(),
        history_coverage: lineage.history_coverage().to_owned(),
    })
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
