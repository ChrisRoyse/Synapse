use calyx_core::{CalyxError, Result};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError as MutexTryLockError};
use std::time::Duration;

static PROCESS_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
const LOCK_RETRY_ATTEMPTS: u32 = 80;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);

pub(crate) struct FileLockGuard {
    _process_guard: MutexGuard<'static, ()>,
    _file: File,
}

impl FileLockGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let key = lock_key(path)?;
        let process_guard = process_mutex(&key)
            .lock()
            .map_err(|_| CalyxError::backpressure("file lock mutex poisoned"))?;
        let file = open_lock_file(path)?;
        acquire_os_lock(&file, path)?;
        Ok(Self {
            _process_guard: process_guard,
            _file: file,
        })
    }

    /// Attempts to acquire a durable lock without joining an in-process or
    /// cross-process waiter queue.
    ///
    /// Long-running maintenance uses this admission path so an already-active
    /// pass is observable as backpressure immediately instead of consuming an
    /// entire caller deadline while waiting to perform redundant work.
    pub(crate) fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let key = lock_key(path)?;
        let process_guard = match process_mutex(&key).try_lock() {
            Ok(guard) => guard,
            Err(MutexTryLockError::WouldBlock) => return Ok(None),
            Err(MutexTryLockError::Poisoned(_)) => {
                return Err(CalyxError::backpressure("file lock mutex poisoned"));
            }
        };
        let Some(file) = try_open_lock_file(path)? else {
            return Ok(None);
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                _process_guard: process_guard,
                _file: file,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) if is_windows_lock_contention(&error) => Ok(None),
            Err(TryLockError::Error(error)) => {
                tracing::error!(
                    path = %path.display(),
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "nonblocking durable file lock acquisition failed"
                );
                Err(CalyxError::backpressure(format!(
                    "try lock file {}: kind={:?} raw_os_error={:?}: {error}",
                    path.display(),
                    error.kind(),
                    error.raw_os_error()
                )))
            }
        }
    }
}

fn process_mutex(path: &Path) -> &'static Mutex<()> {
    let locks = PROCESS_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().expect("file lock registry poisoned");
    if let Some(lock) = locks.get(path) {
        return lock;
    }
    let lock = Box::leak(Box::new(Mutex::new(())));
    locks.insert(path.to_path_buf(), lock);
    lock
}

fn open_lock_file(path: &Path) -> Result<File> {
    let mut attempts = 0_u32;
    loop {
        match OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(error) if is_windows_lock_contention(&error) && attempts < LOCK_RETRY_ATTEMPTS => {
                attempts += 1;
                tracing::warn!(
                    path = %path.display(),
                    attempts,
                    retry_after_ms = LOCK_RETRY_DELAY.as_millis(),
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "waiting to open durable lock file"
                );
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    attempts,
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "open durable lock file failed"
                );
                return Err(CalyxError::disk_pressure(format!(
                    "open lock file {} attempts={attempts}: kind={:?} raw_os_error={:?}: {error}",
                    path.display(),
                    error.kind(),
                    error.raw_os_error()
                )));
            }
        }
    }
}

fn try_open_lock_file(path: &Path) -> Result<Option<File>> {
    match OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if is_windows_lock_contention(&error) => Ok(None),
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                kind = ?error.kind(),
                os_error = error.raw_os_error(),
                "open durable lock file for nonblocking acquisition failed"
            );
            Err(CalyxError::disk_pressure(format!(
                "open lock file {} for nonblocking acquisition: kind={:?} raw_os_error={:?}: {error}",
                path.display(),
                error.kind(),
                error.raw_os_error()
            )))
        }
    }
}

fn acquire_os_lock(file: &File, path: &Path) -> Result<()> {
    let mut attempts = 0_u32;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if attempts < LOCK_RETRY_ATTEMPTS => {
                attempts += 1;
                tracing::warn!(
                    path = %path.display(),
                    attempts,
                    retry_after_ms = LOCK_RETRY_DELAY.as_millis(),
                    "waiting for durable file lock"
                );
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(TryLockError::WouldBlock) => {
                tracing::error!(
                    path = %path.display(),
                    attempts,
                    "durable file lock acquisition timed out"
                );
                return Err(CalyxError::backpressure(format!(
                    "lock file {} attempts={attempts}: lock is held by another process",
                    path.display()
                )));
            }
            Err(TryLockError::Error(error))
                if is_windows_lock_contention(&error) && attempts < LOCK_RETRY_ATTEMPTS =>
            {
                attempts += 1;
                tracing::warn!(
                    path = %path.display(),
                    attempts,
                    retry_after_ms = LOCK_RETRY_DELAY.as_millis(),
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "waiting for Windows durable file lock"
                );
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(TryLockError::Error(error)) => {
                tracing::error!(
                    path = %path.display(),
                    attempts,
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "durable file lock acquisition failed"
                );
                return Err(CalyxError::backpressure(format!(
                    "lock file {} attempts={attempts}: kind={:?} raw_os_error={:?}: {error}",
                    path.display(),
                    error.kind(),
                    error.raw_os_error()
                )));
            }
        }
    }
}

#[cfg(windows)]
fn is_windows_lock_contention(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    error.raw_os_error().is_some_and(|code| {
        code == ERROR_SHARING_VIOLATION.cast_signed() || code == ERROR_LOCK_VIOLATION.cast_signed()
    })
}

#[cfg(not(windows))]
fn is_windows_lock_contention(_error: &io::Error) -> bool {
    false
}

fn lock_key(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| CalyxError::disk_pressure(format!("create lock dir: {error}")))?;
        let parent = parent.canonicalize().map_err(|error| {
            CalyxError::disk_pressure(format!("canonicalize lock dir: {error}"))
        })?;
        let name = path
            .file_name()
            .ok_or_else(|| CalyxError::disk_pressure("lock path has no file name"))?;
        return Ok(parent.join(name));
    }
    Ok(path.to_path_buf())
}
