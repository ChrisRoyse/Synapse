//! Consistent online backup of a durable Aster vault directory.
//!
//! A backup is a byte-faithful physical copy of the vault's *sacred* on-disk
//! state (manifest generations, per-CF SSTs, WAL segments, ledger head anchor,
//! vault identity, residency pin) into a fresh target directory that opens as a
//! standalone vault. It is **not** a raw copy of a live, mutating directory:
//!
//! 1. The native-compaction maintenance guard is held for the whole copy, so no
//!    concurrent compaction, GC, or tombstone-purge pass can delete or rewrite a
//!    file underneath the enumeration (fail-closed: if a maintenance pass is
//!    already active the backup refuses rather than racing it).
//! 2. Under that guard, one checkpoint flush advances the durable manifest so
//!    every WAL-committed batch has a durable-batch SST home before the copy
//!    reads the tree — the copied `durable_seq` names the consistent point.
//! 3. SSTs are immutable once published (atomic create-new), so every file the
//!    copy observes is complete; the copy captures a superset that a later
//!    `verify_restore` re-derives and proves.
//!
//! Regenerable artifacts (`ann/`, `kernel/`, `guard/`) are excluded by default
//! because they rebuild deterministically from the sacred rows; the two runtime
//! sidecars (`vault.lock`, `vault.pid`) are never copied because they would
//! block reopening the restored copy or bind it to the source process.

use super::AsterVault;
use calyx_core::{CalyxError, Clock, Result};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Target directory for a backup is missing, non-empty, or otherwise unusable.
pub const CALYX_ASTER_BACKUP_TARGET_INVALID: &str = "CALYX_ASTER_BACKUP_TARGET_INVALID";
/// A backup filesystem read/copy failed.
pub const CALYX_ASTER_BACKUP_IO: &str = "CALYX_ASTER_BACKUP_IO";
/// Backup was requested against a read-only / non-durable vault handle.
pub const CALYX_ASTER_BACKUP_NOT_DURABLE: &str = "CALYX_ASTER_BACKUP_NOT_DURABLE";

/// Top-level regenerable subdirectories excluded from a sacred-only backup.
pub const REGENERABLE_DIRS: [&str; 3] = ["ann", "kernel", "guard"];
/// Runtime-only files never copied: they block reopen or bind to the source host.
pub const EXCLUDED_RUNTIME_FILES: [&str; 2] = ["vault.lock", "vault.pid"];

const COPY_CHUNK_BYTES: usize = 1 << 20;

/// One copied file plus its content hash, for manifest-level integrity proof.
#[derive(Debug, Clone, Serialize)]
pub struct AsterBackupFile {
    /// Path relative to the backup vault root, using `/` separators.
    pub relative_path: String,
    pub len_bytes: u64,
    pub sha256: String,
}

/// Structured result of a consistent vault backup.
#[derive(Debug, Clone, Serialize)]
pub struct AsterBackupReport {
    pub source_vault_dir: PathBuf,
    pub target_vault_dir: PathBuf,
    /// Durable manifest coverage sequence captured at the consistent point.
    pub durable_seq: u64,
    pub include_regenerable: bool,
    pub file_count: u64,
    pub total_bytes: u64,
    pub files: Vec<AsterBackupFile>,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Copies the vault's sacred on-disk state into `target_vault_dir` at a
    /// consistent durable point, holding the native-compaction guard so no
    /// maintenance pass mutates the tree during the copy.
    ///
    /// `target_vault_dir` must not already contain files. When
    /// `include_regenerable` is false (the default policy) the rebuildable
    /// `ann/`, `kernel/`, and `guard/` directories are skipped.
    ///
    /// # Errors
    ///
    /// Fails closed when the vault is not durable, the target is unusable, a
    /// maintenance pass already holds the compaction guard, the checkpoint
    /// flush fails, or any file copy fails.
    pub fn backup_consistent(
        &self,
        target_vault_dir: &Path,
        include_regenerable: bool,
    ) -> Result<AsterBackupReport> {
        let Some(durable) = self.durable.as_ref() else {
            return Err(CalyxError {
                code: CALYX_ASTER_BACKUP_NOT_DURABLE,
                message: "backup requires a durable (writable) vault handle; this handle is \
                          read-only and has no WAL/checkpoint state to snapshot"
                    .to_owned(),
                remediation: "run the backup through the live durable vault, not a read-only \
                              inspection handle",
            });
        };
        prepare_target_dir(target_vault_dir)?;
        self.drain_checkpoints_paced("backup preflight")?;
        self.with_native_compaction_guard(|| {
            let durable_seq = self.with_durable_commit_lock(|| {
                self.checkpoint_locked()?;
                self.verified_durable_coverage_seq(durable)
            })?;
            let source = durable.root().to_path_buf();
            let mut files = Vec::new();
            copy_tree(
                &source,
                &source,
                target_vault_dir,
                include_regenerable,
                &mut files,
            )?;
            files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            let total_bytes = files.iter().map(|file| file.len_bytes).sum();
            tracing::info!(
                code = "CALYX_ASTER_BACKUP_COPIED",
                source = %source.display(),
                target = %target_vault_dir.display(),
                durable_seq,
                include_regenerable,
                file_count = files.len(),
                total_bytes,
                "copied sacred vault state under the native-compaction guard"
            );
            Ok(AsterBackupReport {
                source_vault_dir: source,
                target_vault_dir: target_vault_dir.to_path_buf(),
                durable_seq,
                include_regenerable,
                file_count: files.len() as u64,
                total_bytes,
                files,
            })
        })
    }
}

fn prepare_target_dir(target: &Path) -> Result<()> {
    if target.exists() {
        if !target.is_dir() {
            return Err(target_invalid(format!(
                "backup target {} exists and is not a directory",
                target.display()
            )));
        }
        let mut entries = fs::read_dir(target)
            .map_err(|error| backup_io(target, "read backup target dir", &error.to_string()))?;
        if entries.next().is_some() {
            return Err(target_invalid(format!(
                "backup target {} is not empty; choose a fresh directory so the backup cannot \
                 mix with pre-existing state",
                target.display()
            )));
        }
        return Ok(());
    }
    fs::create_dir_all(target)
        .map_err(|error| backup_io(target, "create backup target dir", &error.to_string()))
}

fn copy_tree(
    source_root: &Path,
    dir: &Path,
    target_root: &Path,
    include_regenerable: bool,
    files: &mut Vec<AsterBackupFile>,
) -> Result<()> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|error| backup_io(dir, "read vault dir", &error.to_string()))?
    {
        let entry =
            entry.map_err(|error| backup_io(dir, "read vault dir entry", &error.to_string()))?;
        entries.push(entry.path());
    }
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| backup_io(&path, "stat vault entry", &error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(target_invalid(format!(
                "vault entry {} is a symlink; refusing to follow it into a backup",
                path.display()
            )));
        }
        let is_top_level = dir == source_root;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                target_invalid(format!(
                    "vault entry {} has a non-UTF-8 name",
                    path.display()
                ))
            })?;
        if is_top_level && EXCLUDED_RUNTIME_FILES.contains(&name) {
            continue;
        }
        if is_top_level && !include_regenerable && REGENERABLE_DIRS.contains(&name) {
            continue;
        }
        if metadata.is_dir() {
            copy_tree(source_root, &path, target_root, include_regenerable, files)?;
        } else {
            files.push(copy_file(source_root, &path, target_root)?);
        }
    }
    Ok(())
}

fn copy_file(source_root: &Path, path: &Path, target_root: &Path) -> Result<AsterBackupFile> {
    let relative = path.strip_prefix(source_root).map_err(|_| {
        target_invalid(format!(
            "vault entry {} escaped the vault root {}",
            path.display(),
            source_root.display()
        ))
    })?;
    let target_path = target_root.join(relative);
    if let Some(parent) = target_path.parent() {
        crate::fsync::create_dir_all(parent, "backup target subdir")?;
    }
    let bytes =
        fs::read(path).map_err(|error| backup_io(path, "read vault file", &error.to_string()))?;
    crate::fsync::write_atomic_create_new(&target_path, &bytes, "backup file")?;
    let mut hasher = Sha256::new();
    for chunk in bytes.chunks(COPY_CHUNK_BYTES) {
        hasher.update(chunk);
    }
    let relative_path = relative
        .to_str()
        .ok_or_else(|| {
            target_invalid(format!(
                "vault entry {} has a non-UTF-8 path",
                path.display()
            ))
        })?
        .replace('\\', "/");
    Ok(AsterBackupFile {
        relative_path,
        len_bytes: bytes.len() as u64,
        sha256: hex(hasher.finalize().as_slice()),
    })
}

fn target_invalid(message: String) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_BACKUP_TARGET_INVALID,
        message,
        remediation: "choose a fresh, empty, symlink-free target directory on a durable volume",
    }
}

fn backup_io(path: &Path, action: &str, detail: &str) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_BACKUP_IO,
        message: format!("{action} {}: {detail}", path.display()),
        remediation: "inspect the exact path, free space, and permissions, then retry the backup",
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
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
