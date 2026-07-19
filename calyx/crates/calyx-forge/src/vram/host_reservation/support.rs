use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::{
    DEFAULT_HOST_CAP_MIB, HOST_CAP_MIB_ENV, HostGpuReservationRequest, HostGpuReservationSnapshot,
    PersistedState, REMEDIATION,
};
use crate::{ForgeError, Result};

const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct PhysicalDevice {
    pub(super) uuid: String,
    pub(super) name: String,
    pub(super) total_mib: u64,
    pub(super) free_mib: u64,
}

pub(super) fn read_physical_device(device_index: u32) -> Result<PhysicalDevice> {
    let library = if cfg!(windows) {
        "nvml.dll"
    } else {
        "libnvidia-ml.so.1"
    };
    let nvml = nvml_wrapper::Nvml::builder()
        .lib_path(std::ffi::OsStr::new(library))
        .init()
        .map_err(|error| {
            config_error(format!(
                "NVML init failed loading {library}; physical GPU state is unprovable: {error}"
            ))
        })?;
    let device = nvml.device_by_index(device_index).map_err(|error| {
        config_error(format!(
            "NVML device_by_index({device_index}) failed: {error}"
        ))
    })?;
    let memory = device.memory_info().map_err(|error| {
        config_error(format!(
            "NVML memory_info failed for device {device_index}: {error}"
        ))
    })?;
    let uuid = device.uuid().map_err(|error| {
        config_error(format!(
            "NVML UUID failed for device {device_index}: {error}"
        ))
    })?;
    let name = device.name().map_err(|error| {
        config_error(format!(
            "NVML name failed for device {device_index}: {error}"
        ))
    })?;
    Ok(PhysicalDevice {
        uuid,
        name,
        total_mib: memory.total / BYTES_PER_MIB,
        free_mib: memory.free / BYTES_PER_MIB,
    })
}

pub(super) fn validate_request(request: &HostGpuReservationRequest) -> Result<()> {
    for (field, value) in [
        ("owner", request.owner.as_str()),
        ("job_id", request.job_id.as_str()),
        ("command", request.command.as_str()),
    ] {
        if value.trim().is_empty() || value.chars().any(char::is_control) || value.len() > 512 {
            return Err(config_error(format!(
                "GPU reservation {field} must be non-blank, control-free, and at most 512 bytes"
            )));
        }
    }
    if request.requested_mib == 0 {
        return Err(config_error(
            "GPU reservation requested_mib must be > 0".to_string(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn default_reservation_root() -> Result<PathBuf> {
    Ok(PathBuf::from("/tmp/calyx-gpu-reservations"))
}

#[cfg(windows)]
pub(super) fn default_reservation_root() -> Result<PathBuf> {
    let program_data = std::env::var_os("ProgramData").ok_or_else(|| {
        config_error(
            "ProgramData is unavailable; set CALYX_GPU_RESERVATION_ROOT to an absolute host-wide directory"
                .to_string(),
        )
    })?;
    Ok(PathBuf::from(program_data)
        .join("Calyx")
        .join("gpu-reservations"))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn default_reservation_root() -> Result<PathBuf> {
    Err(config_error(
        "no host-wide GPU reservation root exists for this operating system; set CALYX_GPU_RESERVATION_ROOT to an absolute directory"
            .to_string(),
    ))
}

pub(super) fn validate_root(root: &Path) -> Result<()> {
    if root.as_os_str().is_empty() {
        return Err(config_error("GPU reservation root is empty".to_string()));
    }
    if !root.is_absolute() {
        return Err(config_error(format!(
            "GPU reservation root {} is relative and could split host admission by process cwd",
            root.display()
        )));
    }
    if root.to_str().is_none() {
        return Err(config_error(
            "GPU reservation root must be valid Unicode for exact persisted lease identity"
                .to_string(),
        ));
    }
    Ok(())
}

pub(super) fn host_cap_mib() -> Result<u64> {
    match std::env::var(HOST_CAP_MIB_ENV) {
        Ok(raw) => {
            let value = raw.trim().parse::<u64>().map_err(|error| {
                config_error(format!(
                    "{HOST_CAP_MIB_ENV}={raw:?} is not a positive MiB count: {error}"
                ))
            })?;
            if value == 0 {
                return Err(config_error(format!("{HOST_CAP_MIB_ENV} must be > 0")));
            }
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_HOST_CAP_MIB),
        Err(error) => Err(config_error(format!(
            "{HOST_CAP_MIB_ENV} is not valid Unicode: {error}"
        ))),
    }
}

pub(super) fn reserved_mib(state: &PersistedState) -> Result<u64> {
    state.reservations.iter().try_fold(0_u64, |sum, row| {
        sum.checked_add(row.requested_mib)
            .ok_or_else(|| config_error("persisted reservation sum overflow".to_string()))
    })
}

pub(super) fn reservation_id(request: &HostGpuReservationRequest, now: u128) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.device_index.to_be_bytes());
    hasher.update(std::process::id().to_be_bytes());
    hasher.update(now.to_be_bytes());
    hasher.update(request.owner.as_bytes());
    hasher.update(request.job_id.as_bytes());
    hasher.update(request.command.as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn reservation_identities(state: &PersistedState) -> String {
    if state.reservations.is_empty() {
        return "none".to_string();
    }
    state
        .reservations
        .iter()
        .map(|row| {
            format!(
                "{}:{}:{}:{}MiB:pid{}",
                row.owner, row.job_id, row.reservation_id, row.requested_mib, row.pid
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn snapshot(
    state: &PersistedState,
    state_path: &Path,
    bytes: &[u8],
) -> Result<HostGpuReservationSnapshot> {
    let reserved_mib = reserved_mib(state)?;
    Ok(HostGpuReservationSnapshot {
        schema_version: state.schema_version,
        state_path: state_path.to_string_lossy().into_owned(),
        state_sha256: hex_sha256(bytes),
        device_index: state.device_index,
        device_uuid: state.device_uuid.clone(),
        device_name: state.device_name.clone(),
        device_total_mib: state.device_total_mib,
        host_cap_mib: state.host_cap_mib,
        required_free_mib: state.required_free_mib,
        headroom_mib: state.headroom_mib,
        epoch_free_mib: state.epoch_free_mib,
        epoch_capacity_mib: state.epoch_capacity_mib,
        last_physical_free_mib: state.last_physical_free_mib,
        reserved_mib,
        available_reservation_mib: state.epoch_capacity_mib.saturating_sub(reserved_mib),
        admitted_total: state.admitted_total,
        rejected_total: state.rejected_total,
        stale_reaped_total: state.stale_reaped_total,
        reservations: state.reservations.clone(),
        last_rejection: state.last_rejection.clone(),
    })
}

pub(super) fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            io_error(
                "open GPU reservation lock",
                path,
                error,
                "fix lease directory ownership/permissions",
            )
        })
}

pub(super) fn create_lease_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            io_error(
                "create unique GPU reservation lease",
                path,
                error,
                "inspect the device ledger for a reservation-id collision or stale orphan lease; never reuse an ambiguous lease",
            )
        })
}

pub(super) fn open_existing_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            io_error(
                "open persisted GPU reservation lease",
                path,
                error,
                "the ledger names a missing/unreadable lease; inspect process state and repair the source of truth rather than assuming the reservation is stale",
            )
        })
}

pub(super) fn unix_ms() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| config_error(format!("system clock is before Unix epoch: {error}")))
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn backpressure(detail: String) -> ForgeError {
    ForgeError::HostVramBackpressure {
        detail,
        remediation: REMEDIATION.to_string(),
    }
}

pub(super) fn config_error(detail: String) -> ForgeError {
    ForgeError::VramBudget {
        detail,
        remediation: "repair the named host reservation configuration/source of truth; never assume unknown GPU state is safe".to_string(),
    }
}

pub(super) fn io_error(
    operation: &str,
    path: &Path,
    error: std::io::Error,
    remediation: &str,
) -> ForgeError {
    ForgeError::CacheError {
        op: operation.to_string(),
        path: path.to_string_lossy().into_owned(),
        detail: error.to_string(),
        remediation: remediation.to_string(),
    }
}
