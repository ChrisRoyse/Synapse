//! Atomic host-wide GPU admission and reservation ledger.
//!
//! [`VramBudgeter`](super::VramBudgeter) protects allocations made inside one
//! process. This module closes the process boundary: every participating job
//! serializes admission through one device-scoped file lock, persists owner and
//! job identity, and holds a separately locked lease file for its lifetime.
//! A crashed process automatically releases the OS lease; the next mutation or
//! readback removes the stale ledger row.

mod state;
mod support;

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

const STATE_SCHEMA_VERSION: u32 = 1;
const STATE_FILE_NAME: &str = "reservations.json";
const STATE_LOCK_FILE_NAME: &str = "reservations.lock";
const REMEDIATION: &str = "wait for a named live reservation to release or reduce the declared measured peak; inspect `calyx gpu reservations` and never bypass admission, fall back to CPU, or kill an unrelated owner";

use self::support::{
    backpressure, config_error, create_lease_file, default_reservation_root,
    effective_reserved_mib, host_cap_mib, io_error, read_physical_device, reservation_id,
    reservation_identities, unix_ms, validate_request, validate_root,
};

/// Maximum aggregate Calyx reservation per device unless explicitly changed.
pub const DEFAULT_HOST_CAP_MIB: u64 = 12 * 1024;
/// Device memory that must remain free for CUDA/runtime safety.
pub const DEFAULT_REQUIRED_FREE_MIB: u64 = 4 * 1024;
/// Additional fragmentation/driver headroom below the hard free floor.
pub const DEFAULT_HOST_HEADROOM_MIB: u64 = 512;
/// Overrides [`DEFAULT_HOST_CAP_MIB`] with a positive MiB count.
pub const HOST_CAP_MIB_ENV: &str = "CALYX_GPU_HOST_CAP_MIB";
/// Overrides the host ledger directory.
pub const HOST_RESERVATION_ROOT_ENV: &str = "CALYX_GPU_RESERVATION_ROOT";

/// Immutable request for one host reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostGpuReservationRequest {
    pub device_index: u32,
    pub owner: String,
    pub job_id: String,
    pub command: String,
    pub requested_mib: u64,
    pub replaces_reservation_id: Option<String>,
}

impl HostGpuReservationRequest {
    pub fn new(
        owner: impl Into<String>,
        job_id: impl Into<String>,
        command: impl Into<String>,
        requested_mib: u64,
    ) -> Self {
        Self {
            device_index: 0,
            owner: owner.into(),
            job_id: job_id.into(),
            command: command.into(),
            requested_mib,
            replaces_reservation_id: None,
        }
    }

    /// Declares that this reservation is a live-process replacement for one
    /// exact existing reservation. Admission counts the pair at its maximum,
    /// while the physical-free gate still covers the new allocation in full.
    #[must_use]
    pub fn replacing(mut self, reservation_id: impl Into<String>) -> Self {
        self.replaces_reservation_id = Some(reservation_id.into());
        self
    }
}

/// One live reservation exposed by physical readback.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct HostGpuReservationView {
    pub reservation_id: String,
    pub owner: String,
    pub job_id: String,
    pub command: String,
    pub pid: u32,
    pub requested_mib: u64,
    pub acquired_unix_ms: u128,
    pub lease_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_reservation_id: Option<String>,
}

/// Last fail-closed admission decision.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct HostGpuReservationRejection {
    pub owner: String,
    pub job_id: String,
    pub command: String,
    pub pid: u32,
    pub requested_mib: u64,
    pub at_unix_ms: u128,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct PersistedState {
    schema_version: u32,
    device_index: u32,
    device_uuid: String,
    device_name: String,
    device_total_mib: u64,
    host_cap_mib: u64,
    required_free_mib: u64,
    headroom_mib: u64,
    epoch_free_mib: u64,
    epoch_capacity_mib: u64,
    last_physical_free_mib: u64,
    last_updated_unix_ms: u128,
    admitted_total: u64,
    rejected_total: u64,
    stale_reaped_total: u64,
    reservations: Vec<HostGpuReservationView>,
    last_rejection: Option<HostGpuReservationRejection>,
}

/// Full source-of-truth readback, including a hash of the bytes reread after
/// the most recent write.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HostGpuReservationSnapshot {
    pub schema_version: u32,
    pub state_path: String,
    pub state_sha256: String,
    pub device_index: u32,
    pub device_uuid: String,
    pub device_name: String,
    pub device_total_mib: u64,
    pub host_cap_mib: u64,
    pub required_free_mib: u64,
    pub headroom_mib: u64,
    pub epoch_free_mib: u64,
    pub epoch_capacity_mib: u64,
    pub last_physical_free_mib: u64,
    pub reserved_mib: u64,
    pub available_reservation_mib: u64,
    pub admitted_total: u64,
    pub rejected_total: u64,
    pub stale_reaped_total: u64,
    pub reservations: Vec<HostGpuReservationView>,
    pub last_rejection: Option<HostGpuReservationRejection>,
}

/// Device-scoped reservation store.
#[derive(Clone, Debug)]
pub struct HostGpuReservationStore {
    root: PathBuf,
    device_index: u32,
}

impl HostGpuReservationStore {
    /// Opens the production store using an absolute environment override or
    /// the fixed OS-wide Calyx reservation directory.
    pub fn from_env(device_index: u32) -> Result<Self> {
        let root = match std::env::var_os(HOST_RESERVATION_ROOT_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            Some(_) => {
                return Err(config_error(format!(
                    "{HOST_RESERVATION_ROOT_ENV} is present but empty"
                )));
            }
            None => default_reservation_root()?,
        };
        validate_root(&root)?;
        Ok(Self { root, device_index })
    }

    pub fn at_root(root: impl Into<PathBuf>, device_index: u32) -> Result<Self> {
        let root = root.into();
        validate_root(&root)?;
        Ok(Self { root, device_index })
    }

    pub fn state_path(&self) -> PathBuf {
        self.device_root().join(STATE_FILE_NAME)
    }

    /// Atomically admits and persists one reservation, then separately rereads
    /// the bytes before returning a live lease guard.
    pub fn acquire(&self, request: HostGpuReservationRequest) -> Result<HostGpuReservation> {
        validate_request(&request)?;
        if request.device_index != self.device_index {
            return Err(config_error(format!(
                "reservation request device {} != store device {}",
                request.device_index, self.device_index
            )));
        }
        self.ensure_root()?;
        let global_lock = self.lock_global()?;
        let physical = read_physical_device(self.device_index)?;
        let now = unix_ms()?;
        let host_cap_mib = host_cap_mib()?;
        let mut state = self.load_or_initialize(&physical, host_cap_mib, now)?;
        self.ensure_same_device(&state, &physical)?;
        let stale = self.prune_stale(&mut state)?;
        state.stale_reaped_total = state.stale_reaped_total.saturating_add(stale);
        if state.reservations.is_empty() {
            state.host_cap_mib = host_cap_mib;
            state.epoch_free_mib = physical.free_mib;
            state.epoch_capacity_mib = host_cap_mib.min(
                physical
                    .free_mib
                    .saturating_sub(DEFAULT_REQUIRED_FREE_MIB)
                    .saturating_sub(DEFAULT_HOST_HEADROOM_MIB),
            );
        }
        state.last_physical_free_mib = physical.free_mib;
        state.last_updated_unix_ms = now;

        let reserved_mib = effective_reserved_mib(&state)?;
        let projected = match request.replaces_reservation_id.as_deref() {
            Some(target_id) => {
                let target = state
                    .reservations
                    .iter()
                    .find(|row| row.reservation_id == target_id)
                    .ok_or_else(|| {
                        config_error(format!(
                            "replacement target reservation {target_id} is absent"
                        ))
                    })?;
                if target.owner != request.owner
                    || target.pid == std::process::id()
                    || target.replaces_reservation_id.is_some()
                    || state
                        .reservations
                        .iter()
                        .any(|row| row.replaces_reservation_id.as_deref() == Some(target_id))
                {
                    return Err(config_error(format!(
                        "replacement target {target_id} is not an unclaimed live reservation owned by {} in another process",
                        request.owner
                    )));
                }
                reserved_mib
                    .checked_sub(target.requested_mib)
                    .and_then(|value| {
                        value.checked_add(target.requested_mib.max(request.requested_mib))
                    })
                    .ok_or_else(|| {
                        backpressure("replacement reservation arithmetic overflow".to_string())
                    })?
            }
            None => reserved_mib
                .checked_add(request.requested_mib)
                .ok_or_else(|| backpressure("host reservation arithmetic overflow".to_string()))?,
        };
        let physical_required = request
            .requested_mib
            .checked_add(DEFAULT_REQUIRED_FREE_MIB)
            .and_then(|value| value.checked_add(DEFAULT_HOST_HEADROOM_MIB))
            .ok_or_else(|| backpressure("physical headroom arithmetic overflow".to_string()))?;
        let refusal = if projected > state.epoch_capacity_mib {
            Some(format!(
                "device={} requested_mib={} + reserved_mib={} = {} exceeds epoch_capacity_mib={} (epoch_free_mib={} - required_free_mib={} - headroom_mib={}, host_cap_mib={}); live={}",
                self.device_index,
                request.requested_mib,
                reserved_mib,
                projected,
                state.epoch_capacity_mib,
                state.epoch_free_mib,
                DEFAULT_REQUIRED_FREE_MIB,
                DEFAULT_HOST_HEADROOM_MIB,
                host_cap_mib,
                reservation_identities(&state)
            ))
        } else if physical.free_mib < physical_required {
            Some(format!(
                "device={} physical_free_mib={} is below requested_mib={} + required_free_mib={} + headroom_mib={} = {}; live={}",
                self.device_index,
                physical.free_mib,
                request.requested_mib,
                DEFAULT_REQUIRED_FREE_MIB,
                DEFAULT_HOST_HEADROOM_MIB,
                physical_required,
                reservation_identities(&state)
            ))
        } else {
            None
        };
        if let Some(reason) = refusal {
            state.rejected_total = state.rejected_total.saturating_add(1);
            state.last_rejection = Some(HostGpuReservationRejection {
                owner: request.owner,
                job_id: request.job_id,
                command: request.command,
                pid: std::process::id(),
                requested_mib: request.requested_mib,
                at_unix_ms: now,
                reason: reason.clone(),
            });
            self.persist_and_verify(&state)?;
            drop(global_lock);
            return Err(backpressure(reason));
        }

        let reservation_id = reservation_id(&request, now);
        let lease_path = self
            .device_root()
            .join(format!("lease-{reservation_id}.lock"));
        let lease_file = create_lease_file(&lease_path)?;
        lease_file.lock().map_err(|error| {
            io_error(
                "lock new GPU lease",
                &lease_path,
                error,
                "another process unexpectedly owns a new reservation id",
            )
        })?;
        state.reservations.push(HostGpuReservationView {
            reservation_id: reservation_id.clone(),
            owner: request.owner,
            job_id: request.job_id,
            command: request.command,
            pid: std::process::id(),
            requested_mib: request.requested_mib,
            acquired_unix_ms: now,
            lease_file: lease_path.to_string_lossy().into_owned(),
            replaces_reservation_id: request.replaces_reservation_id,
        });
        state
            .reservations
            .sort_by(|left, right| left.reservation_id.cmp(&right.reservation_id));
        state.admitted_total = state.admitted_total.saturating_add(1);
        let snapshot = self.persist_and_verify(&state)?;
        drop(global_lock);
        Ok(HostGpuReservation {
            store: self.clone(),
            reservation_id,
            lease_file: Some(lease_file),
            released: false,
            admitted_snapshot: snapshot,
        })
    }

    /// Rereads live state under the global lock and prunes crashed leases.
    pub fn readback(&self) -> Result<HostGpuReservationSnapshot> {
        self.ensure_root()?;
        let global_lock = self.lock_global()?;
        let physical = read_physical_device(self.device_index)?;
        let now = unix_ms()?;
        let host_cap_mib = host_cap_mib()?;
        let mut state = self.load_or_initialize(&physical, host_cap_mib, now)?;
        self.ensure_same_device(&state, &physical)?;
        let stale = self.prune_stale(&mut state)?;
        state.stale_reaped_total = state.stale_reaped_total.saturating_add(stale);
        if state.reservations.is_empty() {
            state.host_cap_mib = host_cap_mib;
            state.epoch_free_mib = physical.free_mib;
            state.epoch_capacity_mib = host_cap_mib.min(
                physical
                    .free_mib
                    .saturating_sub(DEFAULT_REQUIRED_FREE_MIB)
                    .saturating_sub(DEFAULT_HOST_HEADROOM_MIB),
            );
        }
        state.last_physical_free_mib = physical.free_mib;
        state.last_updated_unix_ms = now;
        let snapshot = self.persist_and_verify(&state)?;
        drop(global_lock);
        Ok(snapshot)
    }

    fn release(&self, reservation_id: &str) -> Result<HostGpuReservationSnapshot> {
        let global_lock = self.lock_global()?;
        let physical = read_physical_device(self.device_index)?;
        let now = unix_ms()?;
        let mut state = self.load_existing()?;
        self.ensure_same_device(&state, &physical)?;
        let before = state.reservations.len();
        state
            .reservations
            .retain(|entry| entry.reservation_id != reservation_id);
        if state.reservations.len() == before {
            return Err(config_error(format!(
                "reservation {reservation_id} is absent during release"
            )));
        }
        let stale = self.prune_stale(&mut state)?;
        state.stale_reaped_total = state.stale_reaped_total.saturating_add(stale);
        state.last_physical_free_mib = physical.free_mib;
        state.last_updated_unix_ms = now;
        if state.reservations.is_empty() {
            state.epoch_free_mib = physical.free_mib;
            state.epoch_capacity_mib = state.host_cap_mib.min(
                physical
                    .free_mib
                    .saturating_sub(state.required_free_mib)
                    .saturating_sub(state.headroom_mib),
            );
        }
        let snapshot = self.persist_and_verify(&state)?;
        drop(global_lock);
        Ok(snapshot)
    }

    fn resize(
        &self,
        reservation_id: &str,
        requested_mib: u64,
        command: &str,
    ) -> Result<HostGpuReservationSnapshot> {
        if requested_mib == 0 {
            return Err(config_error(
                "GPU reservation resized requested_mib must be > 0".to_string(),
            ));
        }
        if command.trim().is_empty() || command.len() > 512 || command.chars().any(char::is_control)
        {
            return Err(config_error(
                "GPU reservation resized command must be non-blank, control-free, and at most 512 bytes"
                    .to_string(),
            ));
        }
        let global_lock = self.lock_global()?;
        let physical = read_physical_device(self.device_index)?;
        let now = unix_ms()?;
        let mut state = self.load_existing()?;
        self.ensure_same_device(&state, &physical)?;
        let stale = self.prune_stale(&mut state)?;
        state.stale_reaped_total = state.stale_reaped_total.saturating_add(stale);

        let Some(index) = state
            .reservations
            .iter()
            .position(|entry| entry.reservation_id == reservation_id)
        else {
            return Err(config_error(format!(
                "reservation {reservation_id} is absent during atomic resize"
            )));
        };
        let previous_mib = state.reservations[index].requested_mib;
        let mut projected_state = state.clone();
        projected_state.reservations[index].requested_mib = requested_mib;
        let projected = effective_reserved_mib(&projected_state)?;
        let growth_mib = requested_mib.saturating_sub(previous_mib);
        let physical_required = growth_mib
            .checked_add(state.required_free_mib)
            .and_then(|value| value.checked_add(state.headroom_mib))
            .ok_or_else(|| {
                backpressure("physical resize headroom arithmetic overflow".to_string())
            })?;
        let refusal = if projected > state.epoch_capacity_mib {
            Some(format!(
                "device={} resized reservation={} previous_mib={} requested_mib={} effective_reserved_mib={} exceeds epoch_capacity_mib={}",
                self.device_index,
                reservation_id,
                previous_mib,
                requested_mib,
                projected,
                state.epoch_capacity_mib
            ))
        } else if growth_mib > 0 && physical.free_mib < physical_required {
            Some(format!(
                "device={} physical_free_mib={} is below growth_mib={} + required_free_mib={} + headroom_mib={} = {}",
                self.device_index,
                physical.free_mib,
                growth_mib,
                state.required_free_mib,
                state.headroom_mib,
                physical_required
            ))
        } else {
            None
        };
        if let Some(reason) = refusal {
            let row = &state.reservations[index];
            state.rejected_total = state.rejected_total.saturating_add(1);
            state.last_rejection = Some(HostGpuReservationRejection {
                owner: row.owner.clone(),
                job_id: row.job_id.clone(),
                command: command.to_owned(),
                pid: std::process::id(),
                requested_mib,
                at_unix_ms: now,
                reason: reason.clone(),
            });
            self.persist_and_verify(&state)?;
            drop(global_lock);
            return Err(backpressure(reason));
        }

        state.reservations[index].requested_mib = requested_mib;
        state.reservations[index].command = command.to_owned();
        state.last_physical_free_mib = physical.free_mib;
        state.last_updated_unix_ms = now;
        let snapshot = self.persist_and_verify(&state)?;
        drop(global_lock);
        Ok(snapshot)
    }
}

/// A live lease. Call [`Self::release`] to make normal release errors visible;
/// `Drop` remains a crash/unwind safety net and logs any cleanup failure.
pub struct HostGpuReservation {
    store: HostGpuReservationStore,
    reservation_id: String,
    lease_file: Option<File>,
    released: bool,
    admitted_snapshot: HostGpuReservationSnapshot,
}

impl HostGpuReservation {
    pub fn reservation_id(&self) -> &str {
        &self.reservation_id
    }

    pub fn admitted_snapshot(&self) -> &HostGpuReservationSnapshot {
        &self.admitted_snapshot
    }

    /// Atomically changes the persisted MiB claim while retaining the same
    /// locked lease and reservation identity.
    pub fn resize(
        &mut self,
        requested_mib: u64,
        command: &str,
    ) -> Result<HostGpuReservationSnapshot> {
        let snapshot = self
            .store
            .resize(&self.reservation_id, requested_mib, command)?;
        self.admitted_snapshot = snapshot.clone();
        Ok(snapshot)
    }

    /// Rereads the physical device and persisted host ledger under its global
    /// lock without relying on the acquisition return value.
    pub fn readback(&self) -> Result<HostGpuReservationSnapshot> {
        self.store.readback()
    }

    pub fn release(mut self) -> Result<HostGpuReservationSnapshot> {
        let snapshot = self.release_inner()?;
        self.released = true;
        Ok(snapshot)
    }

    fn release_inner(&mut self) -> Result<HostGpuReservationSnapshot> {
        let snapshot = self.store.release(&self.reservation_id)?;
        if let Some(lease) = self.lease_file.take() {
            lease.unlock().map_err(|error| {
                io_error(
                    "unlock GPU reservation lease",
                    Path::new("<open lease>"),
                    error,
                    "inspect the host lease filesystem",
                )
            })?;
        }
        let lease_path = self
            .store
            .device_root()
            .join(format!("lease-{}.lock", self.reservation_id));
        if let Err(error) = fs::remove_file(&lease_path)
            && error.kind() != ErrorKind::NotFound
        {
            return Err(io_error(
                "remove released GPU lease",
                &lease_path,
                error,
                "fix lease file permissions",
            ));
        }
        Ok(snapshot)
    }
}

impl Drop for HostGpuReservation {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Err(error) = self.release_inner() {
            tracing::error!(
                target: "calyx_forge::vram::host_reservation",
                reservation_id = %self.reservation_id,
                code = error.code(),
                error = %error,
                "GPU reservation Drop cleanup failed; next readback will reap the unlocked stale lease"
            );
        }
    }
}
