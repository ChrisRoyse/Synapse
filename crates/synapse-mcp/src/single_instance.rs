//! Single-instance guard for the Synapse daemon (`--mode http`).
//!
//! Guarantees that at most one daemon process owns a given RocksDB directory at
//! a time. The guard is acquired at startup **before** RocksDB is opened, so a
//! duplicate launch fails fast with a clear, actionable error that names the
//! current holder PID — instead of surfacing later as a cryptic RocksDB `LOCK`
//! failure deep inside a tool call (the exact symptom that motivated this work).
//!
//! Mechanism: an OS advisory exclusive file lock (`fs2`) on `<db>/daemon.lock`.
//! Chosen over a bare Win32 named mutex because the lock is released
//! automatically by the OS when the holding process dies, so a crashed daemon
//! never wedges future launches, and because it is cross-platform (so the
//! behavior is testable off-Windows).
//!
//! The holder PID is deliberately stored in a **separate** `<db>/daemon.pid`
//! file rather than inside the lock file. On Windows `fs2` uses `LockFileEx`,
//! whose exclusive lock is a *mandatory whole-file* lock: while held, no other
//! process can even read the locked file. Storing the PID in an unlocked
//! sidecar keeps it readable by duplicate launchers and by
//! `synapse-mcp doctor`, while the lock file itself stays empty.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process,
};

use fs2::FileExt;

/// Empty file created inside the RocksDB directory used purely as the daemon
/// single-instance advisory lock token.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// Unlocked sidecar file holding the current lock holder's PID (diagnostics).
pub const DAEMON_PID_FILE: &str = "daemon.pid";

/// Empty file inside the durable shell-job store used to exclude every other
/// daemon, even when those daemons use different RocksDB directories.
pub const SHELL_JOB_STORE_LOCK_FILE: &str = "shell-job-store.lock";

/// Unlocked sidecar identifying the process that owns the shell-job store.
pub const SHELL_JOB_STORE_PID_FILE: &str = "shell-job-store.pid";

/// Failure modes when acquiring the daemon single-instance lock.
#[derive(Debug)]
pub enum SingleInstanceError {
    /// Another daemon already holds the lock for this DB path.
    AlreadyRunning {
        lock_path: PathBuf,
        holder_pid: Option<u32>,
    },
    /// The lock file could not be created or locked for a reason other than an
    /// existing holder.
    Io { lock_path: PathBuf, detail: String },
}

impl std::fmt::Display for SingleInstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning {
                lock_path,
                holder_pid,
            } => write!(
                f,
                "another synapse-mcp daemon already owns {} (holder pid {}); stop the other daemon before starting a second one, or point this daemon at a different --db path",
                lock_path.display(),
                holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
            ),
            Self::Io { lock_path, detail } => write!(
                f,
                "failed to acquire daemon single-instance lock {}: {detail}",
                lock_path.display(),
            ),
        }
    }
}

impl std::error::Error for SingleInstanceError {}

/// Failure modes when acquiring exclusive ownership of the durable shell-job
/// store.
#[derive(Debug)]
pub enum ShellJobStoreLockError {
    /// Another daemon already owns the canonical store root.
    AlreadyOwned {
        store_root: PathBuf,
        lock_path: PathBuf,
        holder_pid: Option<u32>,
    },
    /// The store root or its lock token could not be created, resolved, or
    /// locked for a reason other than an existing holder.
    Io {
        store_root: PathBuf,
        lock_path: PathBuf,
        detail: String,
    },
}

impl std::fmt::Display for ShellJobStoreLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyOwned {
                store_root,
                lock_path,
                holder_pid,
            } => write!(
                f,
                "another synapse-mcp daemon already owns shell-job store {} via {} (holder pid {}); stop the other daemon or configure a different SYNAPSE_SHELL_JOB_ROOT",
                store_root.display(),
                lock_path.display(),
                holder_pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
            ),
            Self::Io {
                store_root,
                lock_path,
                detail,
            } => write!(
                f,
                "failed to acquire shell-job store lock {} for {}: {detail}",
                lock_path.display(),
                store_root.display(),
            ),
        }
    }
}

impl std::error::Error for ShellJobStoreLockError {}

/// Holds the daemon single-instance advisory file lock for the lifetime of the
/// process. Dropping the guard releases the lock and removes the PID sidecar
/// (the OS also releases the lock automatically if the process dies).
#[must_use = "dropping the guard immediately releases the single-instance lock"]
pub struct SingleInstanceGuard {
    file: File,
    lock_path: PathBuf,
    pid_path: PathBuf,
    cleanup_attempted: bool,
}

/// Holds exclusive ownership of one canonical durable shell-job store for the
/// daemon lifetime. This is deliberately independent from the RocksDB guard:
/// two daemons with different DB paths must still not recover or mutate the
/// same durable shell jobs concurrently.
#[must_use = "dropping the guard immediately releases the shell-job store lock"]
pub struct ShellJobStoreLockGuard {
    file: File,
    store_root: PathBuf,
    lock_path: PathBuf,
    pid_path: PathBuf,
    cleanup_attempted: bool,
}

#[derive(Clone, Debug)]
pub struct LifetimeLockReleaseReadback {
    pub guard_kind: &'static str,
    pub lock_path: PathBuf,
    pub pid_path: PathBuf,
    pub pid_sidecar_absent: bool,
    pub unlock_succeeded: bool,
}

impl std::fmt::Display for LifetimeLockReleaseReadback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "guard={} lock_path={} pid_path={} pid_sidecar_absent={} unlock_succeeded={}",
            self.guard_kind,
            self.lock_path.display(),
            self.pid_path.display(),
            self.pid_sidecar_absent,
            self.unlock_succeeded
        )
    }
}

#[derive(Debug)]
pub struct LifetimeLockReleaseError {
    pub readback: LifetimeLockReleaseReadback,
    failures: Vec<String>,
}

impl std::fmt::Display for LifetimeLockReleaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} lifetime-lock release failed: {}; readback={}",
            self.readback.guard_kind,
            self.failures.join("; "),
            self.readback
        )
    }
}

impl std::error::Error for LifetimeLockReleaseError {}

#[derive(Clone, Debug)]
pub struct DaemonLifetimeLocksCloseReadback {
    pub shell_job_store: LifetimeLockReleaseReadback,
    pub single_instance: LifetimeLockReleaseReadback,
}

impl std::fmt::Display for DaemonLifetimeLocksCloseReadback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "shell_job_store=({}); single_instance=({})",
            self.shell_job_store, self.single_instance
        )
    }
}

#[derive(Debug)]
pub struct DaemonLifetimeLocksCloseError {
    pub readback: DaemonLifetimeLocksCloseReadback,
    failures: Vec<String>,
}

impl std::fmt::Display for DaemonLifetimeLocksCloseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "daemon lifetime-lock close failed: {}; readback={}",
            self.failures.join("; "),
            self.readback
        )
    }
}

impl std::error::Error for DaemonLifetimeLocksCloseError {}

impl ShellJobStoreLockGuard {
    /// Acquire the lock for `store_root` after creating and canonicalizing it.
    /// Canonicalization makes syntactic aliases and symlink aliases resolve to
    /// the same on-disk lock token.
    pub fn acquire(store_root: &Path) -> Result<Self, ShellJobStoreLockError> {
        let absolute_root = std::path::absolute(store_root).map_err(|error| {
            let requested = store_root.to_path_buf();
            ShellJobStoreLockError::Io {
                lock_path: requested.join(SHELL_JOB_STORE_LOCK_FILE),
                store_root: requested,
                detail: format!("resolve absolute store root: {error}"),
            }
        })?;
        fs::create_dir_all(&absolute_root).map_err(|error| ShellJobStoreLockError::Io {
            lock_path: absolute_root.join(SHELL_JOB_STORE_LOCK_FILE),
            store_root: absolute_root.clone(),
            detail: format!("create store root: {error}"),
        })?;
        let canonical_root =
            fs::canonicalize(&absolute_root).map_err(|error| ShellJobStoreLockError::Io {
                lock_path: absolute_root.join(SHELL_JOB_STORE_LOCK_FILE),
                store_root: absolute_root.clone(),
                detail: format!("canonicalize store root: {error}"),
            })?;
        let lock_path = canonical_root.join(SHELL_JOB_STORE_LOCK_FILE);
        let pid_path = canonical_root.join(SHELL_JOB_STORE_PID_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| ShellJobStoreLockError::Io {
                store_root: canonical_root.clone(),
                lock_path: lock_path.clone(),
                detail: format!("open lock file: {error}"),
            })?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                if let Err(error) = write_pid_file(&pid_path, process::id()) {
                    let cleanup = cleanup_failed_pid_write(&file, &lock_path, &pid_path);
                    return Err(ShellJobStoreLockError::Io {
                        store_root: canonical_root,
                        lock_path,
                        detail: format!(
                            "record holder pid at {}: {}; {cleanup}",
                            pid_path.display(),
                            describe_io_error(&error)
                        ),
                    });
                }
                Ok(Self {
                    file,
                    store_root: canonical_root,
                    lock_path,
                    pid_path,
                    cleanup_attempted: false,
                })
            }
            Err(error) if file_lock_error_is_contention(&error) => {
                Err(ShellJobStoreLockError::AlreadyOwned {
                    holder_pid: read_pid_file(&pid_path),
                    store_root: canonical_root,
                    lock_path,
                })
            }
            Err(error) => Err(ShellJobStoreLockError::Io {
                store_root: canonical_root,
                lock_path,
                detail: format!(
                    "try exclusive lock: {error}; kind={:?}; raw_os_error={:?}",
                    error.kind(),
                    error.raw_os_error()
                ),
            }),
        }
    }

    /// Canonical root whose contents this guard owns.
    #[must_use]
    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    /// Path of the advisory lock token backing this guard.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Remove the PID sidecar while ownership is still exclusive, read that
    /// filesystem Source of Truth back, and then release the advisory lock.
    /// Drop remains an unwind/early-return backstop, but graceful shutdown must
    /// use this checked path so cleanup failures affect the process verdict.
    pub fn close(mut self) -> Result<LifetimeLockReleaseReadback, LifetimeLockReleaseError> {
        let result = release_guard_resources(
            &self.file,
            &self.lock_path,
            &self.pid_path,
            "shell_job_store",
        );
        self.cleanup_attempted = true;
        result
    }
}

fn file_lock_error_is_contention(error: &std::io::Error) -> bool {
    let contention = fs2::lock_contended_error();
    error.kind() == std::io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .zip(contention.raw_os_error())
            .is_some_and(|(actual, expected)| actual == expected)
}

impl SingleInstanceGuard {
    /// Acquire the single-instance lock for `db_path`.
    ///
    /// # Errors
    ///
    /// Returns [`SingleInstanceError::AlreadyRunning`] (naming the current
    /// holder PID when readable) if another daemon already owns the lock, or
    /// [`SingleInstanceError::Io`] if the lock file cannot be created/locked.
    pub fn acquire(db_path: &Path) -> Result<Self, SingleInstanceError> {
        let lock_path = db_path.join(DAEMON_LOCK_FILE);
        let pid_path = db_path.join(DAEMON_PID_FILE);
        fs::create_dir_all(db_path).map_err(|err| SingleInstanceError::Io {
            lock_path: lock_path.clone(),
            detail: format!("create db directory: {err}"),
        })?;
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(err) => {
                return Err(SingleInstanceError::Io {
                    lock_path,
                    detail: format!("open lock file: {err}"),
                });
            }
        };

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                if let Err(error) = write_pid_file(&pid_path, process::id()) {
                    let cleanup = cleanup_failed_pid_write(&file, &lock_path, &pid_path);
                    return Err(SingleInstanceError::Io {
                        lock_path,
                        detail: format!(
                            "record holder pid at {}: {}; {cleanup}",
                            pid_path.display(),
                            describe_io_error(&error)
                        ),
                    });
                }
                Ok(Self {
                    file,
                    lock_path,
                    pid_path,
                    cleanup_attempted: false,
                })
            }
            Err(error) if file_lock_error_is_contention(&error) => {
                Err(SingleInstanceError::AlreadyRunning {
                    holder_pid: read_pid_file(&pid_path),
                    lock_path,
                })
            }
            Err(error) => Err(SingleInstanceError::Io {
                lock_path,
                detail: format!(
                    "try exclusive lock: {error}; kind={:?}; raw_os_error={:?}",
                    error.kind(),
                    error.raw_os_error()
                ),
            }),
        }
    }

    /// Path of the lock file backing this guard.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Read the PID recorded for `db_path`'s daemon, if any. Used by diagnostics
    /// (`doctor`); does not imply the holder is still alive.
    #[must_use]
    pub fn recorded_holder_pid(db_path: &Path) -> Option<u32> {
        read_pid_file(&db_path.join(DAEMON_PID_FILE))
    }

    /// Checked graceful-shutdown counterpart to the Drop backstop.
    pub fn close(mut self) -> Result<LifetimeLockReleaseReadback, LifetimeLockReleaseError> {
        let result = release_guard_resources(
            &self.file,
            &self.lock_path,
            &self.pid_path,
            "rocksdb_single_instance",
        );
        self.cleanup_attempted = true;
        result
    }
}

/// Close the independent shell-job lock first and the RocksDB single-instance
/// lock second. Both attempts always run, and either failure rejects a graceful
/// daemon verdict while retaining both physical readbacks.
pub fn close_daemon_lifetime_locks(
    shell_job_store: ShellJobStoreLockGuard,
    single_instance: SingleInstanceGuard,
) -> Result<DaemonLifetimeLocksCloseReadback, DaemonLifetimeLocksCloseError> {
    let shell_job_store = shell_job_store.close();
    let single_instance = single_instance.close();
    let (shell_job_store, shell_error) = match shell_job_store {
        Ok(readback) => (readback, None),
        Err(error) => (error.readback.clone(), Some(error.to_string())),
    };
    let (single_instance, single_error) = match single_instance {
        Ok(readback) => (readback, None),
        Err(error) => (error.readback.clone(), Some(error.to_string())),
    };
    let readback = DaemonLifetimeLocksCloseReadback {
        shell_job_store,
        single_instance,
    };
    let failures = [shell_error, single_error]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if failures.is_empty() {
        tracing::info!(
            code = "MCP_DAEMON_LIFETIME_LOCKS_CLOSED",
            readback = %readback,
            "daemon lifetime-lock PID sidecars were absent and both advisory locks were released"
        );
        Ok(readback)
    } else {
        let error = DaemonLifetimeLocksCloseError { readback, failures };
        tracing::error!(
            code = "MCP_DAEMON_LIFETIME_LOCKS_CLOSE_FAILED",
            error = %error,
            "daemon lifetime-lock close completed both attempts but failed its postconditions"
        );
        Err(error)
    }
}

fn write_pid_file(pid_path: &Path, pid: u32) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(pid_path)?;
    file.write_all(pid.to_string().as_bytes())?;
    file.flush()
}

fn remove_pid_sidecar(pid_path: &Path) -> std::io::Result<()> {
    match fs::remove_file(pid_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn describe_io_error(error: &std::io::Error) -> String {
    format!(
        "{error}; kind={:?}; raw_os_error={:?}",
        error.kind(),
        error.raw_os_error()
    )
}

fn cleanup_failed_pid_write(file: &File, lock_path: &Path, pid_path: &Path) -> String {
    let mut failures = Vec::new();
    if let Err(error) = remove_pid_sidecar(pid_path) {
        failures.push(format!(
            "remove partial/stale pid sidecar {}: {}",
            pid_path.display(),
            describe_io_error(&error)
        ));
    }
    if let Err(error) = FileExt::unlock(file) {
        failures.push(format!(
            "unlock {} after pid-write failure: {}",
            lock_path.display(),
            describe_io_error(&error)
        ));
    }
    if failures.is_empty() {
        "cleanup=pid-sidecar-absent,lock-released".to_owned()
    } else {
        format!("cleanup failures: {}", failures.join("; "))
    }
}

fn report_cleanup_error(
    code: &'static str,
    guard_kind: &'static str,
    action: &'static str,
    lock_path: &Path,
    pid_path: &Path,
    error: &std::io::Error,
) {
    tracing::error!(
        code,
        guard_kind,
        action,
        lock_path = %lock_path.display(),
        pid_path = %pid_path.display(),
        error = %error,
        error_kind = ?error.kind(),
        raw_os_error = ?error.raw_os_error(),
        "daemon lifetime-lock cleanup failed"
    );

    // Daemon transports declare telemetry before their lifetime-lock guards,
    // so ordinary reverse-order unwind preserves this structured event. Keep
    // stderr as a secondary fail-safe for panic/partial-startup contexts where
    // telemetry may nevertheless be unavailable, and never panic while
    // reporting cleanup failure.
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(
        stderr,
        "synapse-mcp cleanup error: code={code} guard={guard_kind} action={action} lock_path={} pid_path={} error={} kind={:?} raw_os_error={:?}",
        lock_path.display(),
        pid_path.display(),
        error,
        error.kind(),
        error.raw_os_error()
    );
}

fn release_guard_resources(
    file: &File,
    lock_path: &Path,
    pid_path: &Path,
    guard_kind: &'static str,
) -> Result<LifetimeLockReleaseReadback, LifetimeLockReleaseError> {
    let mut failures = Vec::new();
    // Keep ownership exclusive while removing diagnostics. Unlocking first
    // lets a successor publish its PID and creates a race where this guard can
    // delete the successor's truthful sidecar.
    if let Err(error) = remove_pid_sidecar(pid_path) {
        report_cleanup_error(
            "MCP_LIFETIME_LOCK_PID_REMOVE_FAILED",
            guard_kind,
            "remove_pid_sidecar",
            lock_path,
            pid_path,
            &error,
        );
        failures.push(format!(
            "remove PID sidecar {}: {}",
            pid_path.display(),
            describe_io_error(&error)
        ));
    }
    let pid_sidecar_absent = match pid_path.try_exists() {
        Ok(false) => true,
        Ok(true) => {
            let error = std::io::Error::other("PID sidecar still exists after removal attempt");
            report_cleanup_error(
                "MCP_LIFETIME_LOCK_PID_READBACK_FAILED",
                guard_kind,
                "read_pid_sidecar_absence",
                lock_path,
                pid_path,
                &error,
            );
            failures.push(format!(
                "PID sidecar {} still exists after removal attempt",
                pid_path.display()
            ));
            false
        }
        Err(error) => {
            report_cleanup_error(
                "MCP_LIFETIME_LOCK_PID_READBACK_FAILED",
                guard_kind,
                "read_pid_sidecar_absence",
                lock_path,
                pid_path,
                &error,
            );
            failures.push(format!(
                "read PID sidecar absence {}: {}",
                pid_path.display(),
                describe_io_error(&error)
            ));
            false
        }
    };
    let unlock_succeeded = match FileExt::unlock(file) {
        Ok(()) => true,
        Err(error) => {
            report_cleanup_error(
                "MCP_LIFETIME_LOCK_UNLOCK_FAILED",
                guard_kind,
                "unlock",
                lock_path,
                pid_path,
                &error,
            );
            failures.push(format!(
                "unlock {}: {}",
                lock_path.display(),
                describe_io_error(&error)
            ));
            false
        }
    };
    let readback = LifetimeLockReleaseReadback {
        guard_kind,
        lock_path: lock_path.to_path_buf(),
        pid_path: pid_path.to_path_buf(),
        pid_sidecar_absent,
        unlock_succeeded,
    };
    if failures.is_empty() {
        Ok(readback)
    } else {
        Err(LifetimeLockReleaseError { readback, failures })
    }
}

fn read_pid_file(pid_path: &Path) -> Option<u32> {
    fs::read_to_string(pid_path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _release = release_guard_resources(
                &self.file,
                &self.lock_path,
                &self.pid_path,
                "rocksdb_single_instance",
            );
        }
    }
}

impl Drop for ShellJobStoreLockGuard {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _release = release_guard_resources(
                &self.file,
                &self.lock_path,
                &self.pid_path,
                "shell_job_store",
            );
        }
    }
}
