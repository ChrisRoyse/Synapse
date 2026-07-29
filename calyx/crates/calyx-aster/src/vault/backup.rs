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
//! because they rebuild deterministically from the sacred rows.
//!
//! # Runtime state is excluded by an explicit named list, never by "skip what
//! # cannot be read"
//!
//! The vault directory is also the runtime root of the process serving it, so it
//! holds lock tokens and pid sidecars that are *not* vault data. On Windows a
//! `LockFileEx` byte-range lock is mandatory and, per Microsoft's own contract,
//! "if the locking process opens the file a second time, it cannot access the
//! specified region through this second handle until it unlocks the region" —
//! and "locking a region that goes beyond the current end-of-file position is
//! not an error", so even a 0-byte lock token cannot be read while it is held.
//! A backup taken against a *live* daemon therefore died with
//! `ERROR_LOCK_VIOLATION` (os error 33) on `daemon.lock`, which is why the
//! shipped backup capability had never once succeeded in the only configuration
//! that matters.
//!
//! The fix is the one PostgreSQL uses for `postmaster.pid`/`postmaster.opts`:
//! omit the named runtime artifacts because they "record information about the
//! running postmaster, not about the postmaster which will eventually use this
//! backup", and omit the *contents* of directories that are re-initialised at
//! startup while keeping the directories themselves. It is deliberately **not**
//! a blanket "skip files that fail to read" rule: a genuinely unreadable data
//! file must still fail the backup closed rather than silently vanish from it.
//! Every skipped entry is reported in [`AsterBackupReport::excluded_runtime`] so
//! a restore operator can see the omission was intentional.

use super::AsterVault;
use calyx_core::{CalyxError, Clock, Result};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Target directory for a backup is missing, non-empty, or otherwise unusable.
pub const CALYX_ASTER_BACKUP_TARGET_INVALID: &str = "CALYX_ASTER_BACKUP_TARGET_INVALID";
/// Backup target resolves inside the vault being backed up: the copy would
/// recurse into its own output.
pub const CALYX_ASTER_BACKUP_TARGET_INSIDE_VAULT: &str = "CALYX_ASTER_BACKUP_TARGET_INSIDE_VAULT";
/// A backup filesystem read/copy failed.
pub const CALYX_ASTER_BACKUP_IO: &str = "CALYX_ASTER_BACKUP_IO";
/// Backup was requested against a read-only / non-durable vault handle.
pub const CALYX_ASTER_BACKUP_NOT_DURABLE: &str = "CALYX_ASTER_BACKUP_NOT_DURABLE";

/// Top-level regenerable subdirectories excluded from a sacred-only backup.
pub const REGENERABLE_DIRS: [&str; 3] = ["ann", "kernel", "guard"];
/// Substrate-owned runtime-only files never copied: they block reopen or bind
/// the restored copy to the source host.
pub const EXCLUDED_RUNTIME_FILES: [&str; 2] = ["vault.lock", "vault.pid"];
/// Top-level directories whose *contents* are byte-range lock tokens held by the
/// live process. The directory itself is recreated empty in the backup because
/// [`crate::file_lock::FileLockGuard`] opens lock files without creating their
/// parent, so a restored vault still needs the directory to exist.
pub const RUNTIME_LOCK_DIRS: [&str; 1] = ["locks"];
/// Lock token file names excluded wherever they appear in the tree, not just at
/// the root. `wal/.append.lock` is acquired and released around every WAL append,
/// so a live writer can hold it for the instant the copy reads it — an
/// intermittent `ERROR_LOCK_VIOLATION` that would fail an otherwise good backup
/// at random. Matched by exact file name, never by "the read failed".
pub const NESTED_RUNTIME_LOCK_FILES: [&str; 1] = [".append.lock"];

const COPY_CHUNK_BYTES: usize = 1 << 20;

const REASON_VAULT_RUNTIME: &str = "substrate runtime lock/pid sidecar";
const REASON_HOST_RUNTIME: &str = "host-process runtime lock/pid/lifecycle record";
const REASON_LOCK_DIR: &str = "byte-range lock token directory; recreated empty";
const REASON_NESTED_LOCK: &str = "byte-range lock token held around live writes";
const REASON_REGENERABLE: &str = "regenerable artifact directory";

/// One copied file plus its content hash, for manifest-level integrity proof.
#[derive(Debug, Clone, Serialize)]
pub struct AsterBackupFile {
    /// Path relative to the backup vault root, using `/` separators.
    pub relative_path: String,
    pub len_bytes: u64,
    pub sha256: String,
}

/// One vault entry that was present on disk and deliberately not copied.
#[derive(Debug, Clone, Serialize)]
pub struct AsterBackupExclusion {
    /// Path relative to the source vault root, using `/` separators.
    pub relative_path: String,
    /// Why this entry is runtime state rather than vault data.
    pub reason: &'static str,
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
    /// Entries present in the source vault that were deliberately skipped as
    /// runtime state. Recorded so a restore operator can tell an intentional
    /// omission from a gap.
    pub excluded_runtime: Vec<AsterBackupExclusion>,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Copies the vault's sacred on-disk state into `target_vault_dir` at a
    /// consistent durable point, holding the native-compaction guard so no
    /// maintenance pass mutates the tree during the copy.
    ///
    /// `target_vault_dir` must not already contain files and must resolve
    /// outside the vault being copied. When `include_regenerable` is false (the
    /// default policy) the rebuildable `ann/`, `kernel/`, and `guard/`
    /// directories are skipped.
    ///
    /// `host_runtime_files` names the top-level files the *embedding process*
    /// owns inside the vault directory (its own lock/pid/lifecycle records).
    /// The substrate cannot know those names, and they must be excluded by name
    /// rather than by tolerating read failures.
    ///
    /// # Errors
    ///
    /// Fails closed when the vault is not durable, the target is unusable or
    /// lies inside the vault, a maintenance pass already holds the compaction
    /// guard, the checkpoint flush fails, or any file copy fails.
    pub fn backup_consistent(
        &self,
        target_vault_dir: &Path,
        include_regenerable: bool,
        host_runtime_files: &[&str],
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
        prepare_target_dir(target_vault_dir, durable.root())?;
        self.drain_checkpoints_paced("backup preflight")?;
        self.with_native_compaction_guard(|| {
            let durable_seq =
                self.with_durable_commit_lock(|| self.verified_durable_coverage_seq(durable))?;
            let source = durable.root().to_path_buf();
            let plan = CopyPlan {
                source_root: &source,
                target_root: target_vault_dir,
                include_regenerable,
                host_runtime_files,
            };
            let mut files = Vec::new();
            let mut excluded_runtime = Vec::new();
            copy_tree(&plan, &source, &mut files, &mut excluded_runtime)?;
            files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            excluded_runtime.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            let total_bytes = files.iter().map(|file| file.len_bytes).sum();
            tracing::info!(
                code = "CALYX_ASTER_BACKUP_COPIED",
                source = %source.display(),
                target = %target_vault_dir.display(),
                durable_seq,
                include_regenerable,
                file_count = files.len(),
                total_bytes,
                excluded_runtime_count = excluded_runtime.len(),
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
                excluded_runtime,
            })
        })
    }
}

/// Immutable inputs of one copy walk, bundled so the recursion keeps a small
/// signature as the exclusion policy grows.
struct CopyPlan<'a> {
    source_root: &'a Path,
    target_root: &'a Path,
    include_regenerable: bool,
    host_runtime_files: &'a [&'a str],
}

/// Refuses a target that resolves inside the vault being copied. Without this a
/// target under the vault root would be enumerated by the very walk that writes
/// it, copying the backup into itself until the volume filled.
///
/// Both sides are resolved before comparison so `..` segments and symlinked
/// parents cannot smuggle the target back inside the vault. The target usually
/// does not exist yet, so its deepest existing ancestor is canonicalised and the
/// remaining components are normalised onto it.
fn ensure_target_outside_vault(source_vault_dir: &Path, target: &Path) -> Result<()> {
    let source_resolved = fs::canonicalize(source_vault_dir).map_err(|error| {
        backup_io(
            source_vault_dir,
            "resolve source vault dir for backup containment check",
            &error.to_string(),
        )
    })?;
    let target_resolved = resolve_for_containment(target)?;
    if target_resolved.starts_with(&source_resolved) {
        return Err(CalyxError {
            code: CALYX_ASTER_BACKUP_TARGET_INSIDE_VAULT,
            message: format!(
                "backup target {} resolves to {} which is inside the vault being backed up {} \
                 (resolved {}); the copy would enumerate its own output",
                target.display(),
                target_resolved.display(),
                source_vault_dir.display(),
                source_resolved.display()
            ),
            remediation: "choose a backup target on a path that is not under the vault directory \
                          (a separate volume or a sibling directory tree)",
        });
    }
    Ok(())
}

/// Canonicalises the deepest existing ancestor of `path` and re-applies the
/// remaining components, so a not-yet-created target still resolves to a
/// comparable absolute path.
fn resolve_for_containment(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .map_err(|error| backup_io(path, "absolutize backup target", &error.to_string()))?;
    let mut existing = absolute.as_path();
    let mut tail: Vec<Component<'_>> = Vec::new();
    let resolved_root = loop {
        match fs::canonicalize(existing) {
            Ok(resolved) => break resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.components().next_back() else {
                    return Err(backup_io(
                        path,
                        "resolve backup target",
                        "no existing ancestor of the backup target could be resolved",
                    ));
                };
                tail.push(name);
                let Some(parent) = existing.parent() else {
                    return Err(backup_io(
                        path,
                        "resolve backup target",
                        "no existing ancestor of the backup target could be resolved",
                    ));
                };
                existing = parent;
            }
            Err(error) => {
                return Err(backup_io(
                    existing,
                    "resolve backup target ancestor",
                    &error.to_string(),
                ));
            }
        }
    };
    let mut resolved = resolved_root;
    for component in tail.iter().rev() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(resolved)
}

fn prepare_target_dir(target: &Path, source_vault_dir: &Path) -> Result<()> {
    ensure_target_outside_vault(source_vault_dir, target)?;
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
    plan: &CopyPlan<'_>,
    dir: &Path,
    files: &mut Vec<AsterBackupFile>,
    excluded: &mut Vec<AsterBackupExclusion>,
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
        let is_top_level = dir == plan.source_root;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                target_invalid(format!(
                    "vault entry {} has a non-UTF-8 name",
                    path.display()
                ))
            })?;
        if !metadata.is_dir() && NESTED_RUNTIME_LOCK_FILES.contains(&name) {
            excluded.push(AsterBackupExclusion {
                relative_path: relative_key(plan.source_root, &path)?,
                reason: REASON_NESTED_LOCK,
            });
            continue;
        }
        if is_top_level && !metadata.is_dir() {
            if EXCLUDED_RUNTIME_FILES.contains(&name) {
                excluded.push(exclusion(name, REASON_VAULT_RUNTIME));
                continue;
            }
            if plan.host_runtime_files.contains(&name) {
                excluded.push(exclusion(name, REASON_HOST_RUNTIME));
                continue;
            }
        }
        if is_top_level && metadata.is_dir() && RUNTIME_LOCK_DIRS.contains(&name) {
            // Keep the directory, drop its contents: the lock tokens are held by
            // the live process (unreadable while locked) and carry no state, but
            // `FileLockGuard` will not create the parent directory on reopen.
            crate::fsync::create_dir_all(&plan.target_root.join(name), "backup lock dir")?;
            excluded.push(exclusion(name, REASON_LOCK_DIR));
            continue;
        }
        if is_top_level && !plan.include_regenerable && REGENERABLE_DIRS.contains(&name) {
            excluded.push(exclusion(name, REASON_REGENERABLE));
            continue;
        }
        if metadata.is_dir() {
            copy_tree(plan, &path, files, excluded)?;
        } else {
            files.push(copy_file(plan.source_root, &path, plan.target_root)?);
        }
    }
    Ok(())
}

fn exclusion(name: &str, reason: &'static str) -> AsterBackupExclusion {
    AsterBackupExclusion {
        relative_path: name.to_owned(),
        reason,
    }
}

/// Vault-root-relative path of `path`, using `/` separators.
fn relative_key(source_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(source_root).map_err(|_| {
        target_invalid(format!(
            "vault entry {} escaped the vault root {}",
            path.display(),
            source_root.display()
        ))
    })?;
    let key = relative
        .to_str()
        .ok_or_else(|| {
            target_invalid(format!(
                "vault entry {} has a non-UTF-8 path",
                path.display()
            ))
        })?
        .replace('\\', "/");
    Ok(key)
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
