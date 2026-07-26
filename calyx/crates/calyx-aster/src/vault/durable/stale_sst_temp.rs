use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use calyx_core::{CalyxError, Result};

use crate::compaction::TieringPolicy;
use crate::file_lock::FileLockGuard;
use crate::storage_names::classify_sst;

const STALE_TEMP_AMBIGUOUS: &str = "CALYX_ASTER_STALE_SST_TEMP_AMBIGUOUS";
const STALE_TEMP_SCAN_LIMIT: &str = "CALYX_ASTER_STALE_SST_TEMP_SCAN_LIMIT";
const STALE_TEMP_LIVENESS_UNRESOLVED: &str = "CALYX_ASTER_STALE_SST_TEMP_LIVENESS_UNRESOLVED";
const MAX_CF_DIRECTORIES: usize = 4_096;
const MAX_DIRECTORY_ENTRIES: usize = 1_000_000;

#[derive(Debug)]
struct AtomicSstTemp {
    target_name: String,
    owner_pid: u32,
    temp_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerState {
    Alive,
    Dead,
    PidReused,
}

#[derive(Debug, Default)]
struct ReclaimReport {
    roots_scanned: usize,
    cf_directories_scanned: usize,
    entries_scanned: usize,
    candidates: usize,
    active_owner_files_preserved: usize,
    files_reclaimed: usize,
    bytes_reclaimed: u64,
}

/// Reclaims crash-left `write_atomic_create_new` SST staging files before a
/// write-capable vault begins recovery.
///
/// The three cross-process locks are acquired in the same order as native
/// maintenance (native compaction -> checkpoint publisher -> durable commit).
/// Together they prove that no compliant Aster SST publisher is active while
/// the directories are inspected. The embedded PID is an independent safety
/// check for a foreign writer that predates or bypasses those locks: live
/// owners are preserved, and an indeterminate owner fails the open rather than
/// risking deletion.
pub(super) fn reclaim_stale_sst_temps(
    root: &Path,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<()> {
    let _native_compaction =
        FileLockGuard::acquire(&root.join("locks").join("native.compaction.lock"))?;
    let _checkpoint = FileLockGuard::acquire(&root.join("locks").join("durable.checkpoint.lock"))?;
    let _commit = FileLockGuard::acquire(&root.join("locks").join("durable.commit.lock"))?;

    let mut roots = BTreeSet::from([root.to_path_buf()]);
    if let Some(policy) = tiering_policy {
        roots.extend(policy.tier_roots());
    }

    let mut report = ReclaimReport::default();
    for storage_root in roots {
        scan_cf_root(&storage_root.join("cf"), &mut report)?;
        report.roots_scanned = report.roots_scanned.saturating_add(1);
    }

    tracing::info!(
        code = "CALYX_ASTER_STALE_SST_TEMP_SCAN_DONE",
        vault_dir = %root.display(),
        roots_scanned = report.roots_scanned,
        cf_directories_scanned = report.cf_directories_scanned,
        entries_scanned = report.entries_scanned,
        candidates = report.candidates,
        active_owner_files_preserved = report.active_owner_files_preserved,
        files_reclaimed = report.files_reclaimed,
        bytes_reclaimed = report.bytes_reclaimed,
        source_of_truth = "exact CF directory entries under native-compaction + checkpoint-publisher + durable-commit locks",
        "completed bounded crash-left atomic SST temp reclamation"
    );
    Ok(())
}

fn scan_cf_root(cf_root: &Path, report: &mut ReclaimReport) -> Result<()> {
    let metadata = match fs::symlink_metadata(cf_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("inspect CF root", cf_root, error)),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(ambiguous_error(
            cf_root,
            "CF root is not a physical directory",
        ));
    }

    let entries = fs::read_dir(cf_root)
        .map_err(|error| io_error("enumerate durable CF root", cf_root, error))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| io_error("read durable CF root entry", cf_root, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| io_error("inspect CF root entry type", &entry.path(), error))?;
        if file_type.is_symlink() {
            return Err(ambiguous_error(
                &entry.path(),
                "symlink inside the durable CF root",
            ));
        }
        if !file_type.is_dir() {
            continue;
        }
        report.cf_directories_scanned = report.cf_directories_scanned.saturating_add(1);
        if report.cf_directories_scanned > MAX_CF_DIRECTORIES {
            return Err(scan_limit_error(
                cf_root,
                "column-family directories",
                report.cf_directories_scanned,
                MAX_CF_DIRECTORIES,
            ));
        }
        scan_cf_directory(&entry.path(), report)?;
    }
    Ok(())
}

fn scan_cf_directory(cf_dir: &Path, report: &mut ReclaimReport) -> Result<()> {
    let entries = fs::read_dir(cf_dir)
        .map_err(|error| io_error("enumerate durable CF directory", cf_dir, error))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| io_error("read durable CF directory entry", cf_dir, error))?;
        report.entries_scanned = report.entries_scanned.saturating_add(1);
        if report.entries_scanned > MAX_DIRECTORY_ENTRIES {
            return Err(scan_limit_error(
                cf_dir,
                "physical CF entries",
                report.entries_scanned,
                MAX_DIRECTORY_ENTRIES,
            ));
        }

        let file_type = entry
            .file_type()
            .map_err(|error| io_error("inspect physical CF entry type", &entry.path(), error))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ambiguous_error(&entry.path(), "non-Unicode physical CF entry name"))?;
        let Some(candidate) = parse_atomic_sst_temp_name(&name, &entry.path())? else {
            continue;
        };
        report.candidates = report.candidates.saturating_add(1);

        if !file_type.is_file() || file_type.is_symlink() {
            return Err(ambiguous_error(
                &entry.path(),
                "atomic SST temp candidate is not a physical regular file",
            ));
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| io_error("read atomic SST temp metadata", &entry.path(), error))?;
        let bytes = metadata.len();
        let modified_at = metadata.modified().map_err(|error| {
            io_error(
                "read atomic SST temp modification time",
                &entry.path(),
                error,
            )
        })?;
        let owner_state = owner_state(candidate.owner_pid, modified_at, &entry.path())?;
        match owner_state {
            OwnerState::Alive => {
                report.active_owner_files_preserved =
                    report.active_owner_files_preserved.saturating_add(1);
                tracing::info!(
                    code = "CALYX_ASTER_ACTIVE_SST_TEMP_PRESERVED",
                    path = %entry.path().display(),
                    target_name = candidate.target_name,
                    owner_pid = candidate.owner_pid,
                    temp_id = candidate.temp_id,
                    bytes,
                    "preserved an atomic SST temp whose owning process is still active"
                );
            }
            OwnerState::Dead | OwnerState::PidReused => {
                let owner_state = if owner_state == OwnerState::PidReused {
                    "pid_reused_after_temp_write"
                } else {
                    "owner_process_absent"
                };
                crate::fsync::remove_file_durable(&entry.path(), "stale atomic SST temp")?;
                match fs::symlink_metadata(entry.path()) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(io_error(
                            "verify reclaimed atomic SST temp",
                            &entry.path(),
                            error,
                        ));
                    }
                    Ok(_) => {
                        return Err(CalyxError {
                            code: STALE_TEMP_AMBIGUOUS,
                            message: format!(
                                "stale atomic SST temp still exists after durable removal and parent sync: {}",
                                entry.path().display()
                            ),
                            remediation: "inspect the exact path and filesystem sharing state before reopening the vault",
                        });
                    }
                }
                report.files_reclaimed = report.files_reclaimed.saturating_add(1);
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
                tracing::info!(
                    code = "CALYX_ASTER_STALE_SST_TEMP_RECLAIMED",
                    path = %entry.path().display(),
                    target_name = candidate.target_name,
                    owner_pid = candidate.owner_pid,
                    temp_id = candidate.temp_id,
                    owner_state,
                    bytes,
                    "reclaimed crash-left atomic SST temp and verified physical absence"
                );
            }
        }
    }
    Ok(())
}

fn parse_atomic_sst_temp_name(name: &str, path: &Path) -> Result<Option<AtomicSstTemp>> {
    if !name.starts_with('.') || !name.ends_with(".tmp") {
        return Ok(None);
    }
    let body = &name[1..name.len() - ".tmp".len()];
    let (owner_and_target, temp_id) = body
        .rsplit_once('.')
        .ok_or_else(|| ambiguous_error(path, "missing atomic temp counter"))?;
    let (target_name, owner_pid) = owner_and_target
        .rsplit_once('.')
        .ok_or_else(|| ambiguous_error(path, "missing atomic temp owner PID"))?;
    let owner_pid = owner_pid
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| ambiguous_error(path, "atomic temp owner PID is not a non-zero u32"))?;
    let temp_id = temp_id
        .parse::<u64>()
        .map_err(|_| ambiguous_error(path, "atomic temp counter is not a u64"))?;
    if classify_sst(Path::new(target_name))?.is_none() {
        return Err(ambiguous_error(
            path,
            "atomic temp target is not a canonical Aster SST name",
        ));
    }
    Ok(Some(AtomicSstTemp {
        target_name: target_name.to_owned(),
        owner_pid,
        temp_id,
    }))
}

#[cfg(windows)]
fn owner_state(
    pid: u32,
    temp_modified_at: std::time::SystemTime,
    path: &Path,
) -> Result<OwnerState> {
    use std::time::UNIX_EPOCH;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, WAIT_FAILED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };

    // SAFETY: OpenProcess returns an owned HANDLE or null. The handle is
    // closed on every success path, and WaitForSingleObject receives it only
    // while valid. A zero timeout performs a non-mutating liveness probe.
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(OwnerState::Dead);
        }
        return Err(liveness_error(path, pid, &error.to_string()));
    }
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    let wait_error = (wait == WAIT_FAILED).then(io::Error::last_os_error);
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let times_ok =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let times_error = (times_ok == 0).then(io::Error::last_os_error);
    let close_result = unsafe { CloseHandle(handle) };
    if close_result == 0 {
        return Err(liveness_error(
            path,
            pid,
            &format!("CloseHandle failed: {}", io::Error::last_os_error()),
        ));
    }
    if wait == WAIT_OBJECT_0 {
        Ok(OwnerState::Dead)
    } else if wait == windows_sys::Win32::Foundation::WAIT_TIMEOUT {
        if times_ok == 0 {
            return Err(liveness_error(
                path,
                pid,
                &format!(
                    "GetProcessTimes failed: {}",
                    times_error
                        .as_ref()
                        .map_or_else(|| "unknown OS error".to_owned(), ToString::to_string)
                ),
            ));
        }
        let process_created_ticks =
            (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        let modified_since_unix = temp_modified_at
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                liveness_error(
                    path,
                    pid,
                    &format!("temp modification time predates Unix epoch: {error}"),
                )
            })?;
        const WINDOWS_TO_UNIX_EPOCH_SECONDS: u64 = 11_644_473_600;
        const HUNDRED_NS_PER_SECOND: u64 = 10_000_000;
        let temp_modified_ticks = modified_since_unix
            .as_secs()
            .checked_add(WINDOWS_TO_UNIX_EPOCH_SECONDS)
            .and_then(|seconds| seconds.checked_mul(HUNDRED_NS_PER_SECOND))
            .and_then(|ticks| {
                ticks.checked_add(u64::from(modified_since_unix.subsec_nanos()) / 100)
            })
            .ok_or_else(|| {
                liveness_error(
                    path,
                    pid,
                    "temp modification time overflows Windows FILETIME",
                )
            })?;
        if process_created_ticks > temp_modified_ticks {
            Ok(OwnerState::PidReused)
        } else {
            Ok(OwnerState::Alive)
        }
    } else {
        Err(liveness_error(
            path,
            pid,
            &format!(
                "WaitForSingleObject returned {wait:#x}: {}",
                wait_error
                    .as_ref()
                    .map_or_else(|| "no OS error".to_owned(), ToString::to_string)
            ),
        ))
    }
}

#[cfg(unix)]
fn owner_state(
    pid: u32,
    _temp_modified_at: std::time::SystemTime,
    path: &Path,
) -> Result<OwnerState> {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let pid = i32::try_from(pid)
        .map(Pid::from_raw)
        .map_err(|_| liveness_error(path, pid, "PID exceeds the platform i32 range"))?;
    match kill(pid, None) {
        Ok(()) | Err(Errno::EPERM) => Ok(OwnerState::Alive),
        Err(Errno::ESRCH) => Ok(OwnerState::Dead),
        Err(error) => Err(liveness_error(
            path,
            pid.as_raw() as u32,
            &error.to_string(),
        )),
    }
}

#[cfg(not(any(windows, unix)))]
fn owner_state(
    pid: u32,
    _temp_modified_at: std::time::SystemTime,
    path: &Path,
) -> Result<OwnerState> {
    if pid == std::process::id() {
        Ok(OwnerState::Alive)
    } else {
        Err(liveness_error(
            path,
            pid,
            "this platform has no implemented process-liveness primitive",
        ))
    }
}

fn ambiguous_error(path: &Path, detail: &str) -> CalyxError {
    CalyxError {
        code: STALE_TEMP_AMBIGUOUS,
        message: format!(
            "refusing to classify or delete an ambiguous durable CF temp entry: path={} detail={detail}",
            path.display()
        ),
        remediation: "inspect the exact CF entry; remove it only after proving it is not an active writer or canonical SST",
    }
}

fn scan_limit_error(path: &Path, unit: &str, observed: usize, limit: usize) -> CalyxError {
    CalyxError {
        code: STALE_TEMP_SCAN_LIMIT,
        message: format!(
            "bounded stale SST temp scan exceeded its limit: path={} unit={unit} observed={observed} limit={limit}",
            path.display()
        ),
        remediation: "inspect CF fan-out and compact or repair the vault before retrying open",
    }
}

fn liveness_error(path: &Path, pid: u32, detail: &str) -> CalyxError {
    CalyxError {
        code: STALE_TEMP_LIVENESS_UNRESOLVED,
        message: format!(
            "could not prove atomic SST temp owner liveness: path={} owner_pid={pid} detail={detail}",
            path.display()
        ),
        remediation: "inspect the named PID and temp path; retry only after owner liveness can be established",
    }
}

fn io_error(operation: &str, path: &Path, error: io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!(
        "{operation}: path={} kind={:?} raw_os_error={:?}: {error}",
        path.display(),
        error.kind(),
        error.raw_os_error()
    ))
}
