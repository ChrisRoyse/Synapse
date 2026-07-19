use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use calyx_core::{CalyxError, Result};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishMode {
    CreateNew,
    ReplaceExisting,
}

pub(crate) fn write_atomic_create_new(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    write_atomic(path, bytes, label, PublishMode::CreateNew)
}

pub(crate) fn write_atomic_replace(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    write_atomic(path, bytes, label, PublishMode::ReplaceExisting)
}

pub(crate) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    label: &str,
    mode: PublishMode,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| durable_error(label, "resolve parent", path, None, 0))?;
    create_dir_all(parent, label)?;
    if mode == PublishMode::CreateNew
        && matches!(
            create_new_target_state(path, bytes, label, "preflight")?,
            CreateNewTargetState::Identical
        )
    {
        sync_parent(path, label)?;
        return Ok(());
    }
    let temp = temp_path(path)?;
    let result = (|| {
        let mut file = open_create_new(&temp, label, "create atomic temp")?;
        file.write_all(bytes)
            .map_err(|error| durable_error(label, "write atomic temp", &temp, Some(error), 0))?;
        file.sync_all()
            .map_err(|error| durable_error(label, "fsync atomic temp", &temp, Some(error), 0))?;
        drop(file);
        match publish_path(&temp, path, label, mode) {
            Ok(()) => {}
            Err(error) if mode == PublishMode::CreateNew => {
                match create_new_target_state(path, bytes, label, "post-publish")? {
                    CreateNewTargetState::Identical => {
                        tracing::warn!(
                            label,
                            path = %path.display(),
                            temp_path = %temp.display(),
                            original_error_code = error.code,
                            original_error = %error.message,
                            "create-new durable target already contains identical bytes after publish failure; accepting idempotent recovered publish"
                        );
                        cleanup_unpublished_temp(&temp, label);
                        sync_parent(path, label)?;
                        return Ok(());
                    }
                    CreateNewTargetState::Missing => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        sync_parent(path, label)
    })();
    if result.is_err() {
        cleanup_unpublished_temp(&temp, label);
    }
    result
}

pub(crate) fn create_dir_all(path: &Path, label: &str) -> Result<()> {
    retry_sharing(label, "create directory", path, || fs::create_dir_all(path))
}

pub(crate) fn publish_path(
    source: &Path,
    target: &Path,
    label: &str,
    mode: PublishMode,
) -> Result<()> {
    #[cfg(windows)]
    {
        publish_path_windows(source, target, label, mode)
    }
    #[cfg(not(windows))]
    {
        publish_path_portable(source, target, label, mode)
    }
}

pub(crate) fn sync_parent(path: &Path, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CalyxError::disk_pressure(format!("{label} path has no parent")))?;
    sync_dir(parent, label)
}

/// Removes one durable file and synchronizes its parent directory.
///
/// Missing is idempotent success. Other failures retain the same bounded
/// Windows sharing retry and structured diagnostics as durable publication.
pub(crate) fn remove_file_durable(path: &Path, label: &str) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(CalyxError::disk_pressure(format!(
            "{label} path has no parent"
        )));
    };
    if !parent.exists() {
        return Ok(());
    }
    retry_sharing(
        label,
        "remove durable file",
        path,
        || match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    )?;
    sync_dir(parent, label)
}

#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path, label: &str) -> Result<()> {
    use std::fs::File;

    ensure_directory(dir, label)?;
    retry_sharing(label, "sync parent directory", dir, || {
        File::open(dir).and_then(|handle| handle.sync_all())
    })
}

#[cfg(windows)]
pub(crate) fn sync_dir(dir: &Path, label: &str) -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    ensure_directory(dir, label)?;
    retry_sharing(label, "sync parent directory", dir, || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .and_then(|handle| handle.sync_all())
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_dir(dir: &Path, label: &str) -> Result<()> {
    ensure_directory(dir, label)?;
    Err(CalyxError::disk_pressure(format!(
        "sync {label} parent directory {}: unsupported platform",
        dir.display()
    )))
}

fn ensure_directory(dir: &Path, label: &str) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    Err(CalyxError::disk_pressure(format!(
        "sync {label} parent directory {}: not a directory",
        dir.display()
    )))
}

fn open_create_new(path: &Path, label: &str, operation: &'static str) -> Result<File> {
    retry_sharing(label, operation, path, || {
        OpenOptions::new().write(true).create_new(true).open(path)
    })
}

#[cfg(not(windows))]
fn publish_path_portable(
    source: &Path,
    target: &Path,
    label: &str,
    mode: PublishMode,
) -> Result<()> {
    if mode == PublishMode::CreateNew && target.exists() {
        return Err(create_new_publish_collision_error(
            label,
            "publish create-new target already exists",
            target,
        ));
    }
    retry_sharing(label, "publish durable path", target, || {
        fs::rename(source, target)
    })
}

#[cfg(windows)]
fn publish_path_windows(
    source: &Path,
    target: &Path,
    label: &str,
    mode: PublishMode,
) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source_wide = win32_path(source, label, "prepare source path")?;
    let target_wide = win32_path(target, label, "prepare target path")?;
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if mode == PublishMode::ReplaceExisting {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    let result = retry_sharing(label, "publish durable path", target, || {
        // SAFETY: both buffers are NUL-terminated and remain alive for the call.
        let ok = unsafe { MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), flags) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    });
    if result.is_err() && mode == PublishMode::CreateNew && target.exists() {
        return Err(create_new_publish_collision_error(
            label,
            "publish create-new target already exists",
            target,
        ));
    }
    result
}

fn retry_sharing<T>(
    label: &str,
    operation: &'static str,
    path: &Path,
    mut op: impl FnMut() -> io::Result<T>,
) -> Result<T> {
    let mut attempts = 0_u32;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_sharing_error(&error) && attempts < 7 => {
                attempts += 1;
                let delay = Duration::from_millis(10 * (1_u64 << (attempts - 1)));
                tracing::warn!(
                    label,
                    operation,
                    path = %path.display(),
                    attempt = attempts,
                    retry_after_ms = delay.as_millis(),
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "retrying transient Windows filesystem sharing hazard"
                );
                std::thread::sleep(delay);
            }
            Err(error) => {
                tracing::error!(
                    label,
                    operation,
                    path = %path.display(),
                    attempts,
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "durable filesystem operation failed"
                );
                return Err(durable_error(label, operation, path, Some(error), attempts));
            }
        }
    }
}

#[cfg(windows)]
fn is_retryable_sharing_error(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    error.raw_os_error().is_some_and(|code| {
        code == ERROR_SHARING_VIOLATION.cast_signed() || code == ERROR_LOCK_VIOLATION.cast_signed()
    })
}

#[cfg(not(windows))]
fn is_retryable_sharing_error(_error: &io::Error) -> bool {
    false
}

fn durable_error(
    label: &str,
    operation: &str,
    path: &Path,
    error: Option<io::Error>,
    attempts: u32,
) -> CalyxError {
    let detail = error.map_or_else(
        || "no os error".to_string(),
        |error| {
            format!(
                "kind={:?} raw_os_error={:?} error={}",
                error.kind(),
                error.raw_os_error(),
                error
            )
        },
    );
    CalyxError::disk_pressure(format!(
        "{operation} for {label} path={} attempts={attempts}: {detail}",
        path.display()
    ))
}

fn create_new_publish_collision_error(label: &str, operation: &str, path: &Path) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!(
        "{operation} for {label} path={}: target already exists; create-new durable publish refuses to overwrite immutable artifacts",
        path.display()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateNewTargetState {
    Missing,
    Identical,
}

fn create_new_target_state(
    path: &Path,
    expected: &[u8],
    label: &str,
    phase: &'static str,
) -> Result<CreateNewTargetState> {
    match fs::read(path) {
        Ok(existing) if existing == expected => {
            tracing::warn!(
                label,
                phase,
                path = %path.display(),
                bytes = existing.len(),
                blake3 = %blake3_hex(expected),
                "create-new durable target already exists with identical bytes; treating as idempotent recovered publish"
            );
            Ok(CreateNewTargetState::Identical)
        }
        Ok(existing) => {
            let error = CalyxError::aster_corrupt_shard(format!(
                "create-new durable target collision for {label} path={} phase={phase}: \
                 existing_len={} expected_len={} existing_blake3={} expected_blake3={}; \
                 refusing to overwrite existing immutable artifact",
                path.display(),
                existing.len(),
                expected.len(),
                blake3_hex(&existing),
                blake3_hex(expected)
            ));
            tracing::error!(
                label,
                phase,
                path = %path.display(),
                existing_len = existing.len(),
                expected_len = expected.len(),
                existing_blake3 = %blake3_hex(&existing),
                expected_blake3 = %blake3_hex(expected),
                error_code = error.code,
                "create-new durable target collision; refusing to overwrite"
            );
            Err(error)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CreateNewTargetState::Missing),
        Err(error) => Err(durable_error(
            label,
            "read create-new target",
            path,
            Some(error),
            0,
        )),
    }
}

fn cleanup_unpublished_temp(path: &Path, label: &str) {
    if let Err(cleanup) = fs::remove_file(path)
        && cleanup.kind() != io::ErrorKind::NotFound
    {
        tracing::error!(
            label,
            path = %path.display(),
            kind = ?cleanup.kind(),
            os_error = cleanup.raw_os_error(),
            "failed to remove unpublished durable temp file"
        );
    }
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| durable_error("durable temp", "resolve parent", path, None, 0))?;
    let name = path
        .file_name()
        .ok_or_else(|| durable_error("durable temp", "resolve file name", path, None, 0))?;
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(parent.join(temp_name))
}

#[cfg(windows)]
fn win32_path(path: &Path, label: &str, operation: &'static str) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let absolute = std::path::absolute(path)
        .map_err(|error| durable_error(label, operation, path, Some(error), 0))?;
    let mut wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    for unit in &mut wide {
        if *unit == b'/' as u16 {
            *unit = b'\\' as u16;
        }
    }
    if starts_with_wide(&wide, r"\\?\") || starts_with_wide(&wide, r"\??\") {
        wide.push(0);
        return Ok(wide);
    }
    let mut prefixed = if starts_with_wide(&wide, r"\\") {
        let mut out = encode_wide(r"\\?\UNC\");
        out.extend_from_slice(&wide[2..]);
        out
    } else {
        let mut out = encode_wide(r"\\?\");
        out.extend_from_slice(&wide);
        out
    };
    prefixed.push(0);
    Ok(prefixed)
}

#[cfg(windows)]
fn starts_with_wide(value: &[u16], prefix: &str) -> bool {
    value.starts_with(&encode_wide(prefix))
}

#[cfg(windows)]
fn encode_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}
