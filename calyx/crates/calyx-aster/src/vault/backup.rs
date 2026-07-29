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
//! # The manifest set is *pinned*, never enumerated
//!
//! The maintenance guard covers the passes that rewrite and delete SSTs, and
//! nothing else. Manifest publication is a different subsystem on a different
//! lock: [`crate::manifest::ManifestStore::write_current`] serializes on
//! `locks/manifest.publish.lock` under the checkpoint lock, and reclaims every
//! generation beyond the newest [`MANIFEST_GENERATIONS_RETAINED`]. The backup's
//! guard therefore never suspended manifest rotation and was never meant to.
//!
//! Extending the guard to cover publication is the wrong repair: admission is
//! deliberately non-blocking and fail-closed, so a checkpoint that had to take
//! it would either fail outright or stall every durable commit for the whole
//! backup — and this backup deliberately *drives* checkpoints in its preflight.
//! RocksDB's checkpoint takes the same position: `DisableFileDeletions` there
//! "suspend[s] deleting obsolete files. Compactions will continue to occur",
//! and the snapshot itself comes from pinning a live-file set plus the `CURRENT`
//! and `MANIFEST` bytes, never from stalling the writer.
//!
//! So the copy pins instead. `CURRENT` and the generation it names are read at
//! one publication boundary into memory, `CURRENT`/`MANIFEST`/`manifest-<seq>`
//! in the backup are materialized from those pinned bytes, and everything the
//! pinned generation references is copied and required. A generation *older*
//! than the pin is unreachable from a restore (a restore reads `CURRENT`), so
//! its mid-copy reclamation is the vault doing its job: copied when present,
//! recorded in [`AsterBackupReport::tolerated_absences`] when not, and tolerated
//! on that specific justification alone. A generation *newer* than the pin is
//! excluded, because copying it would put two snapshots' state in one backup.
//!
//! Measured on the live daemon: a rolling window of exactly 32 generations
//! rotating one per ~12 s. A directory-listing copy of that vault does not have
//! a race it might lose, it has one it eventually always loses, and the larger
//! the vault the more certain the loss. Pinning removes the clock from the
//! copy's critical path entirely, so a backup that takes minutes still succeeds.
//!
//! # `CURRENT` advancing mid-copy does not invalidate the backup
//!
//! It is re-read at the end and recorded in
//! [`AsterBackupPinnedManifest::current_advanced_to`], and that is all. The
//! pinned snapshot stays valid and the copy is not retried: the copied tree is
//! internally consistent at the pinned generation, restoring to a slightly older
//! consistent point is exactly what a backup *is*, and retrying would chase a
//! pointer that advances again every twelve seconds.
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
use crate::manifest::{
    CURRENT_FILE, MANIFEST_FILE, MANIFEST_GENERATIONS_RETAINED, ManifestStore, PinnedManifest,
    manifest_generation_seq,
};
use calyx_core::{CalyxError, Clock, Result};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io;
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
/// The `CURRENT` pointer and the manifest generation it names could not be
/// pinned, so the backup has no authoritative snapshot to copy.
pub const CALYX_ASTER_BACKUP_MANIFEST_PIN_FAILED: &str = "CALYX_ASTER_BACKUP_MANIFEST_PIN_FAILED";
/// A file the pinned manifest references is absent from the source vault. This
/// is corruption, not rotation: a restore reads `CURRENT`, so the restored vault
/// would be unopenable. Never tolerated.
pub const CALYX_ASTER_BACKUP_MANIFEST_REF_MISSING: &str = "CALYX_ASTER_BACKUP_MANIFEST_REF_MISSING";
/// The pinned manifest's durable coverage is behind the coverage this backup
/// already proved under the durable commit lock.
pub const CALYX_ASTER_BACKUP_PIN_COVERAGE_REGRESSED: &str =
    "CALYX_ASTER_BACKUP_PIN_COVERAGE_REGRESSED";

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
const REASON_DURABLE_TEMP: &str =
    "unpublished temp file of an in-flight atomic write; not vault state until it is renamed";
const REASON_MANIFEST_AFTER_PIN: &str = "manifest generation published after this backup's pinned snapshot point; copying it would put \
     two snapshots' state in one backup";
const REASON_SUPERSEDED_MANIFEST_ROTATED: &str = "superseded manifest generation older than the pinned CURRENT target, reclaimed by the vault's \
     rolling generation window during the copy; a restore reads CURRENT, so this generation is \
     unreachable from the restored vault and its absence cannot affect it";

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

/// One entry that was expected on disk, found absent during the copy, and
/// tolerated on a stated justification rather than silently dropped.
///
/// This list exists so "tolerated" can never mean "unexamined". The only
/// justification the backup accepts is [`REASON_SUPERSEDED_MANIFEST_ROTATED`];
/// every other absence fails the backup closed.
#[derive(Debug, Clone, Serialize)]
pub struct AsterBackupToleratedAbsence {
    /// Path relative to the source vault root, using `/` separators.
    pub relative_path: String,
    /// Why this specific absence cannot affect the restored vault.
    pub reason: &'static str,
}

/// The `CURRENT`-pinned manifest snapshot the backup restores to.
#[derive(Debug, Clone, Serialize)]
pub struct AsterBackupPinnedManifest {
    /// `CURRENT`'s target at pin time, reproduced verbatim in the backup.
    pub pointer: String,
    pub manifest_seq: u64,
    /// Durable coverage the pinned generation vouches for.
    pub durable_seq: u64,
    /// Size of the source vault's rolling generation window, which is what makes
    /// a listing-then-copy backup lose eventually and a pinned one not.
    pub generations_retained: usize,
    /// Vault-relative paths the pinned generation references. Every one was
    /// copied, or the backup failed with
    /// [`CALYX_ASTER_BACKUP_MANIFEST_REF_MISSING`].
    pub referenced_paths: Vec<String>,
    /// `CURRENT`'s target when the copy finished, when it advanced past the pin.
    /// Informational: the pinned snapshot remains valid and is not retried.
    pub current_advanced_to: Option<String>,
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
    /// The pinned snapshot, absent only for a vault that has never published a
    /// manifest.
    pub pinned_manifest: Option<AsterBackupPinnedManifest>,
    /// Entries that vanished mid-copy and were tolerated by name and stated
    /// justification. Empty on a quiescent vault.
    pub tolerated_absences: Vec<AsterBackupToleratedAbsence>,
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
    /// guard, the checkpoint flush fails, the `CURRENT` snapshot cannot be
    /// pinned ([`CALYX_ASTER_BACKUP_MANIFEST_PIN_FAILED`]), an asset the pinned
    /// manifest references is absent
    /// ([`CALYX_ASTER_BACKUP_MANIFEST_REF_MISSING`]), or any file copy fails.
    ///
    /// The one absence it does *not* fail on is a manifest generation older than
    /// the pinned one, which the live vault reclaims on its own schedule and
    /// which no restore can reach; each such absence is reported in
    /// [`AsterBackupReport::tolerated_absences`].
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
            let source = durable.root().to_path_buf();
            // Prove manifest coverage and pin the snapshot inside one durable
            // commit critical section, so no commit can advance `latest_seq`
            // between the proof and the pin. Lock order is durable commit ->
            // manifest publish, and it cannot invert: checkpoint publication was
            // moved off the durable commit lock by #1806/#1832, so the manifest
            // publisher never reaches back for it.
            let (verified_seq, pinned) = self.with_durable_commit_lock(|| {
                let verified_seq = self.verified_durable_coverage_seq(durable)?;
                let pinned = ManifestStore::open(&source)
                    .pin_current()
                    .map_err(|error| pin_failed(&source, &error))?;
                Ok((verified_seq, pinned))
            })?;
            let durable_seq = pinned_durable_seq(&source, verified_seq, pinned.as_ref())?;
            let plan = CopyPlan {
                source_root: &source,
                target_root: target_vault_dir,
                include_regenerable,
                host_runtime_files,
                pinned: pinned.as_ref(),
            };
            let mut copy = CopyState::default();
            if let Some(pinned) = pinned.as_ref() {
                publish_pinned_manifest_state(&plan, pinned, &mut copy)?;
                copy_referenced_set(&plan, pinned, &mut copy)?;
            }
            copy_tree(&plan, &source, &mut copy)?;
            let CopyState {
                mut files,
                mut excluded_runtime,
                mut tolerated_absences,
                ..
            } = copy;
            files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            excluded_runtime.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            tolerated_absences.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            let total_bytes = files.iter().map(|file| file.len_bytes).sum();
            let pinned_manifest = pinned
                .as_ref()
                .map(|pinned| describe_pin(&source, pinned))
                .transpose()?;
            tracing::info!(
                code = "CALYX_ASTER_BACKUP_COPIED",
                source = %source.display(),
                target = %target_vault_dir.display(),
                durable_seq,
                include_regenerable,
                file_count = files.len(),
                total_bytes,
                excluded_runtime_count = excluded_runtime.len(),
                tolerated_absence_count = tolerated_absences.len(),
                pinned_pointer = pinned_manifest
                    .as_ref()
                    .map_or("<none>", |pin| pin.pointer.as_str()),
                current_advanced_to = pinned_manifest
                    .as_ref()
                    .and_then(|pin| pin.current_advanced_to.as_deref())
                    .unwrap_or("<unchanged>"),
                "copied the pinned sacred vault state under the native-compaction guard"
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
                pinned_manifest,
                tolerated_absences,
            })
        })
    }
}

/// Durable coverage the backup reports: the pinned generation's own claim.
///
/// The pin is taken after the coverage proof inside the same commit critical
/// section, so the pinned generation is the same or newer and its `durable_seq`
/// is monotone. A regression would mean the manifest moved backwards under a
/// held lock, which is corruption rather than a race.
fn pinned_durable_seq(
    source: &Path,
    verified_seq: u64,
    pinned: Option<&PinnedManifest>,
) -> Result<u64> {
    let Some(pinned) = pinned else {
        return Ok(verified_seq);
    };
    if pinned.manifest.durable_seq < verified_seq {
        return Err(CalyxError {
            code: CALYX_ASTER_BACKUP_PIN_COVERAGE_REGRESSED,
            message: format!(
                "pinned manifest {} in {} vouches for durable_seq {} but this backup already \
                 proved durable coverage {verified_seq} under the durable commit lock; manifest \
                 coverage is monotone, so it cannot regress",
                pinned.pointer,
                source.display(),
                pinned.manifest.durable_seq
            ),
            remediation: "preserve the vault untouched and inspect CURRENT, the pinned manifest \
                          generation, and checkpoint telemetry; do not take or restore a backup \
                          until the regression is explained",
        });
    }
    Ok(pinned.manifest.durable_seq)
}

/// Records the pinned snapshot, re-reading `CURRENT` to report whether it moved.
///
/// Advancement is recorded, never acted on: the copied tree is internally
/// consistent at the pinned generation, and a restore to a slightly older
/// consistent point is precisely what a backup is for.
fn describe_pin(source: &Path, pinned: &PinnedManifest) -> Result<AsterBackupPinnedManifest> {
    let current_path = source.join(CURRENT_FILE);
    let after = fs::read_to_string(&current_path).map_err(|error| {
        backup_io(
            &current_path,
            "re-read CURRENT after the backup copy",
            &error.to_string(),
        )
    })?;
    let after = after.trim();
    Ok(AsterBackupPinnedManifest {
        pointer: pinned.pointer.clone(),
        manifest_seq: pinned.manifest_seq,
        durable_seq: pinned.manifest.durable_seq,
        generations_retained: MANIFEST_GENERATIONS_RETAINED,
        referenced_paths: pinned.referenced_paths(),
        current_advanced_to: (after != pinned.pointer).then(|| after.to_owned()),
    })
}

/// Immutable inputs of one copy walk, bundled so the recursion keeps a small
/// signature as the exclusion policy grows.
struct CopyPlan<'a> {
    source_root: &'a Path,
    target_root: &'a Path,
    include_regenerable: bool,
    host_runtime_files: &'a [&'a str],
    /// The pinned snapshot, or `None` for a vault that has never published a
    /// manifest and therefore has no generation window to race.
    pinned: Option<&'a PinnedManifest>,
}

/// Everything one copy walk accumulates, including the set of vault-relative
/// keys already written so the walk never copies a pinned file twice.
#[derive(Default)]
struct CopyState {
    files: Vec<AsterBackupFile>,
    excluded_runtime: Vec<AsterBackupExclusion>,
    tolerated_absences: Vec<AsterBackupToleratedAbsence>,
    copied: BTreeSet<String>,
}

impl CopyState {
    fn record(&mut self, file: AsterBackupFile) {
        self.copied.insert(file.relative_path.clone());
        self.files.push(file);
    }
}

/// Materializes `CURRENT`, `MANIFEST`, and the pinned generation from the pinned
/// bytes instead of copying whatever the walk finds on disk.
///
/// Every publication rewrites all three, and the live vault publishes one every
/// few seconds. Copied independently they can disagree — `CURRENT` naming one
/// generation while `MANIFEST` already mirrors the next — which is a torn
/// mixture of two snapshots even when no file is missing. Written from one
/// pinned read they cannot.
fn publish_pinned_manifest_state(
    plan: &CopyPlan<'_>,
    pinned: &PinnedManifest,
    state: &mut CopyState,
) -> Result<()> {
    publish_pinned_file(plan, state, CURRENT_FILE, &pinned.current_bytes)?;
    publish_pinned_file(plan, state, MANIFEST_FILE, &pinned.manifest_bytes)?;
    publish_pinned_file(plan, state, &pinned.pointer, &pinned.manifest_bytes)
}

fn publish_pinned_file(
    plan: &CopyPlan<'_>,
    state: &mut CopyState,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    let target_path = plan.target_root.join(name);
    crate::fsync::write_atomic_create_new(&target_path, bytes, "backup pinned manifest state")?;
    state.record(hashed_file(name.to_owned(), bytes));
    Ok(())
}

/// Copies exactly what the pinned generation references.
///
/// A restore reads `CURRENT`, which names the pinned generation, and opening it
/// re-verifies every immutable ref by blake3. A referenced asset that is absent
/// therefore makes the restored vault unopenable, so absence here is corruption
/// and fails the backup closed naming both the path and the manifest that
/// references it. This is the one class of missing file that must never be
/// tolerated, and it is why the backup cannot simply skip what vanished.
fn copy_referenced_set(
    plan: &CopyPlan<'_>,
    pinned: &PinnedManifest,
    state: &mut CopyState,
) -> Result<()> {
    for logical in pinned.referenced_paths() {
        let source_path = plan.source_root.join(&logical);
        let bytes = match fs::read(&source_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(manifest_ref_missing(
                    &source_path,
                    &logical,
                    &pinned.pointer,
                ));
            }
            Err(error) => {
                return Err(backup_io(
                    &source_path,
                    "read manifest-referenced vault file",
                    &error.to_string(),
                ));
            }
        };
        let file = write_copied_file(plan.source_root, &source_path, plan.target_root, bytes)?;
        state.record(file);
    }
    Ok(())
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

fn copy_tree(plan: &CopyPlan<'_>, dir: &Path, state: &mut CopyState) -> Result<()> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|error| backup_io(dir, "read vault dir", &error.to_string()))?
    {
        let entry =
            entry.map_err(|error| backup_io(dir, "read vault dir entry", &error.to_string()))?;
        entries.push(entry.path());
    }
    entries.sort();
    let is_top_level = dir == plan.source_root;
    for path in entries {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                target_invalid(format!(
                    "vault entry {} has a non-UTF-8 name",
                    path.display()
                ))
            })?
            .to_owned();
        // Every name-based exclusion is decided *before* the stat.
        //
        // These are exactly the entries a live process creates and destroys
        // while the walk runs: an atomic-write temp exists for microseconds
        // between write and rename, the daemon rotates its lifecycle ledgers by
        // rename, and the WAL append lock is taken and dropped around every
        // append. Stat-then-classify would turn a normal rename into a fatal
        // `os error 2` on an entry the backup was never going to copy. The name
        // is the whole policy here, so consulting it first changes no outcome
        // and removes the race — this is still a named exclusion list, never a
        // "skip whatever fails to read" rule.
        let name = name.as_str();
        if is_durable_temp_name(name) {
            state.excluded_runtime.push(AsterBackupExclusion {
                relative_path: relative_key(plan.source_root, &path)?,
                reason: REASON_DURABLE_TEMP,
            });
            continue;
        }
        if NESTED_RUNTIME_LOCK_FILES.contains(&name) {
            state.excluded_runtime.push(AsterBackupExclusion {
                relative_path: relative_key(plan.source_root, &path)?,
                reason: REASON_NESTED_LOCK,
            });
            continue;
        }
        if is_top_level && EXCLUDED_RUNTIME_FILES.contains(&name) {
            state
                .excluded_runtime
                .push(exclusion(name, REASON_VAULT_RUNTIME));
            continue;
        }
        if is_top_level && plan.host_runtime_files.contains(&name) {
            state
                .excluded_runtime
                .push(exclusion(name, REASON_HOST_RUNTIME));
            continue;
        }
        if is_top_level
            && let Some(pinned) = plan.pinned
            && copy_pinned_generation_entry(plan, pinned, &path, name, state)?
        {
            continue;
        }
        if state
            .copied
            .contains(&relative_key(plan.source_root, &path)?)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| backup_io(&path, "stat vault entry", &error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(target_invalid(format!(
                "vault entry {} is a symlink; refusing to follow it into a backup",
                path.display()
            )));
        }
        if is_top_level && metadata.is_dir() && RUNTIME_LOCK_DIRS.contains(&name) {
            // Keep the directory, drop its contents: the lock tokens are held by
            // the live process (unreadable while locked) and carry no state, but
            // `FileLockGuard` will not create the parent directory on reopen.
            crate::fsync::create_dir_all(&plan.target_root.join(name), "backup lock dir")?;
            state
                .excluded_runtime
                .push(exclusion(name, REASON_LOCK_DIR));
            continue;
        }
        if is_top_level && !plan.include_regenerable && REGENERABLE_DIRS.contains(&name) {
            state
                .excluded_runtime
                .push(exclusion(name, REASON_REGENERABLE));
            continue;
        }
        if metadata.is_dir() {
            copy_tree(plan, &path, state)?;
        } else {
            let file = copy_file(plan.source_root, &path, plan.target_root)?;
            state.record(file);
        }
    }
    Ok(())
}

/// Applies the pinned-snapshot policy to one top-level entry, returning whether
/// the entry was consumed here.
///
/// `CURRENT`, `MANIFEST`, and the pinned generation were already written from
/// the pinned bytes. A newer generation is excluded so the backup cannot hold
/// two snapshots at once. An older generation is superseded: the restore reads
/// `CURRENT`, which names the pinned generation, so nothing in the restored
/// vault can reach it — which is the sole reason its mid-copy reclamation by the
/// vault's rolling window is survivable. It is copied when still present and
/// recorded as a tolerated absence when not, and every other read failure on it
/// still fails the backup closed.
fn copy_pinned_generation_entry(
    plan: &CopyPlan<'_>,
    pinned: &PinnedManifest,
    path: &Path,
    name: &str,
    state: &mut CopyState,
) -> Result<bool> {
    if name == CURRENT_FILE || name == MANIFEST_FILE {
        return Ok(true);
    }
    let Some(seq) = manifest_generation_seq(name) else {
        return Ok(false);
    };
    if seq == pinned.manifest_seq {
        return Ok(true);
    }
    if seq > pinned.manifest_seq {
        state
            .excluded_runtime
            .push(exclusion(name, REASON_MANIFEST_AFTER_PIN));
        return Ok(true);
    }
    match fs::read(path) {
        Ok(bytes) => {
            let file = write_copied_file(plan.source_root, path, plan.target_root, bytes)?;
            state.record(file);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::info!(
                code = "CALYX_ASTER_BACKUP_SUPERSEDED_MANIFEST_ROTATED",
                path = %path.display(),
                superseded_seq = seq,
                pinned_pointer = pinned.pointer,
                pinned_seq = pinned.manifest_seq,
                generations_retained = MANIFEST_GENERATIONS_RETAINED,
                "superseded manifest generation was reclaimed by the live vault during the backup \
                 copy; the pinned snapshot is unaffected"
            );
            state.tolerated_absences.push(AsterBackupToleratedAbsence {
                relative_path: name.to_owned(),
                reason: REASON_SUPERSEDED_MANIFEST_ROTATED,
            });
        }
        Err(error) => {
            return Err(backup_io(
                path,
                "read superseded manifest generation",
                &error.to_string(),
            ));
        }
    }
    Ok(true)
}

/// Exact shape of [`crate::fsync`]'s unpublished atomic-write temporary:
/// `.<name>.<pid>.<counter>.tmp`.
///
/// One exists only between the write and the rename inside a single durable
/// publication, so an enumeration can see one and find it gone microseconds
/// later. It is not vault state — it holds bytes no reader has ever been told
/// about, and `stale_sst_temp` reclaims leaked ones — so it is excluded by exact
/// name shape rather than by tolerating a read failure.
fn is_durable_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp")
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
    let bytes =
        fs::read(path).map_err(|error| backup_io(path, "read vault file", &error.to_string()))?;
    write_copied_file(source_root, path, target_root, bytes)
}

/// Publishes already-read source bytes at the mirrored path under the target and
/// hashes exactly the bytes written, so every digest in the report reproduces
/// against the backup file it names.
fn write_copied_file(
    source_root: &Path,
    path: &Path,
    target_root: &Path,
    bytes: Vec<u8>,
) -> Result<AsterBackupFile> {
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
    crate::fsync::write_atomic_create_new(&target_path, &bytes, "backup file")?;
    let relative_path = relative
        .to_str()
        .ok_or_else(|| {
            target_invalid(format!(
                "vault entry {} has a non-UTF-8 path",
                path.display()
            ))
        })?
        .replace('\\', "/");
    Ok(hashed_file(relative_path, &bytes))
}

fn hashed_file(relative_path: String, bytes: &[u8]) -> AsterBackupFile {
    let mut hasher = Sha256::new();
    for chunk in bytes.chunks(COPY_CHUNK_BYTES) {
        hasher.update(chunk);
    }
    AsterBackupFile {
        relative_path,
        len_bytes: bytes.len() as u64,
        sha256: hex(hasher.finalize().as_slice()),
    }
}

fn pin_failed(source: &Path, error: &CalyxError) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_BACKUP_MANIFEST_PIN_FAILED,
        message: format!(
            "could not pin the CURRENT manifest snapshot of {} for backup: {} ({})",
            source.display(),
            error.message,
            error.code
        ),
        remediation: "inspect CURRENT, the manifest generation it names, and the immutable \
                      panel/registry/codebook assets that generation references; a backup without \
                      an authoritative snapshot would not restore",
    }
}

fn manifest_ref_missing(path: &Path, logical: &str, pointer: &str) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_BACKUP_MANIFEST_REF_MISSING,
        message: format!(
            "pinned manifest {pointer} references {logical}, but {} does not exist; a restore \
             reads CURRENT and re-verifies every referenced asset, so this is missing vault data, \
             not the manifest generation rotation the backup tolerates",
            path.display()
        ),
        remediation: "preserve the vault untouched and inspect the referenced asset and its \
                      directory; restore it from an earlier backup before taking a new one",
    }
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
