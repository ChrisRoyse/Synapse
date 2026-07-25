use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use super::support::{
    PhysicalDevice, config_error, effective_reserved_mib, io_error, open_existing_lock_file,
    open_lock_file, reservation_identities, snapshot,
};
use super::{
    DEFAULT_HOST_HEADROOM_MIB, DEFAULT_REQUIRED_FREE_MIB, HOST_CAP_MIB_ENV,
    HostGpuReservationSnapshot, HostGpuReservationStore, PersistedState, STATE_LOCK_FILE_NAME,
    STATE_SCHEMA_VERSION,
};
use crate::Result;

impl HostGpuReservationStore {
    pub(super) fn ensure_root(&self) -> Result<()> {
        fs::create_dir_all(self.device_root()).map_err(|error| {
            io_error(
                "create GPU reservation directory",
                &self.device_root(),
                error,
                "fix directory ownership/permissions before retrying",
            )
        })
    }

    pub(super) fn device_root(&self) -> PathBuf {
        self.root.join(format!("device-{}", self.device_index))
    }

    pub(super) fn lock_global(&self) -> Result<File> {
        let path = self.device_root().join(STATE_LOCK_FILE_NAME);
        let file = open_lock_file(&path)?;
        file.lock().map_err(|error| {
            io_error(
                "lock GPU reservation source of truth",
                &path,
                error,
                "inspect the lock file and host filesystem health",
            )
        })?;
        Ok(file)
    }

    pub(super) fn load_or_initialize(
        &self,
        physical: &PhysicalDevice,
        host_cap_mib: u64,
        now: u128,
    ) -> Result<PersistedState> {
        if self.state_path().exists() {
            // The persisted cap is deliberately *not* reconciled here: a
            // reservation row is not evidence of a live owner until its lease
            // lock has been probed. `reconcile_host_cap` runs that check after
            // `prune_stale`.
            return self.load_existing();
        }
        let epoch_capacity_mib = host_cap_mib.min(
            physical
                .free_mib
                .saturating_sub(DEFAULT_REQUIRED_FREE_MIB)
                .saturating_sub(DEFAULT_HOST_HEADROOM_MIB),
        );
        Ok(PersistedState {
            schema_version: STATE_SCHEMA_VERSION,
            device_index: self.device_index,
            device_uuid: physical.uuid.clone(),
            device_name: physical.name.clone(),
            device_total_mib: physical.total_mib,
            host_cap_mib,
            required_free_mib: DEFAULT_REQUIRED_FREE_MIB,
            headroom_mib: DEFAULT_HOST_HEADROOM_MIB,
            epoch_free_mib: physical.free_mib,
            epoch_capacity_mib,
            last_physical_free_mib: physical.free_mib,
            last_updated_unix_ms: now,
            admitted_total: 0,
            rejected_total: 0,
            stale_reaped_total: 0,
            reservations: Vec::new(),
            last_rejection: None,
        })
    }

    pub(super) fn load_existing(&self) -> Result<PersistedState> {
        let path = self.state_path();
        let bytes = fs::read(&path).map_err(|error| {
            io_error(
                "read GPU reservation source of truth",
                &path,
                error,
                "restore a valid reservation ledger only after confirming no live lease files exist",
            )
        })?;
        let state: PersistedState = serde_json::from_slice(&bytes).map_err(|error| {
            config_error(format!(
                "GPU reservation source of truth {} is corrupt: {error}",
                path.display()
            ))
        })?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Err(config_error(format!(
                "GPU reservation schema {} != supported {}",
                state.schema_version, STATE_SCHEMA_VERSION
            )));
        }
        self.validate_persisted_state(&state)?;
        Ok(state)
    }

    fn validate_persisted_state(&self, state: &PersistedState) -> Result<()> {
        if state.device_name.trim().is_empty()
            || state.device_uuid.trim().is_empty()
            || state.device_total_mib == 0
            || state.host_cap_mib == 0
            || state.required_free_mib != DEFAULT_REQUIRED_FREE_MIB
            || state.headroom_mib != DEFAULT_HOST_HEADROOM_MIB
            || state.epoch_free_mib > state.device_total_mib
            || state.last_physical_free_mib > state.device_total_mib
            || state.last_updated_unix_ms == 0
        {
            return Err(config_error(
                "GPU reservation source of truth violates its device/capacity invariants"
                    .to_string(),
            ));
        }
        let expected_capacity = state.host_cap_mib.min(
            state
                .epoch_free_mib
                .saturating_sub(state.required_free_mib)
                .saturating_sub(state.headroom_mib),
        );
        if state.epoch_capacity_mib != expected_capacity {
            return Err(config_error(format!(
                "GPU reservation epoch_capacity_mib={} != derived capacity {}",
                state.epoch_capacity_mib, expected_capacity
            )));
        }
        let mut reservation_ids = BTreeSet::new();
        for reservation in &state.reservations {
            validate_identity("owner", &reservation.owner)?;
            validate_identity("job_id", &reservation.job_id)?;
            validate_identity("command", &reservation.command)?;
            if !valid_reservation_id(&reservation.reservation_id)
                || !reservation_ids.insert(reservation.reservation_id.as_str())
                || reservation.pid == 0
                || reservation.requested_mib == 0
                || reservation.acquired_unix_ms == 0
            {
                return Err(config_error(format!(
                    "GPU reservation row {} violates identity/lifetime invariants",
                    reservation.reservation_id
                )));
            }
            let expected_lease = self
                .device_root()
                .join(format!("lease-{}.lock", reservation.reservation_id));
            if Path::new(&reservation.lease_file) != expected_lease.as_path() {
                return Err(config_error(format!(
                    "GPU reservation {} lease path {} escapes or differs from expected {}",
                    reservation.reservation_id,
                    reservation.lease_file,
                    expected_lease.display()
                )));
            }
        }
        let mut replacement_targets = BTreeSet::new();
        for reservation in &state.reservations {
            let Some(target_id) = reservation.replaces_reservation_id.as_deref() else {
                continue;
            };
            let target = state
                .reservations
                .iter()
                .find(|candidate| candidate.reservation_id == target_id)
                .ok_or_else(|| {
                    config_error(format!(
                        "replacement reservation {} targets absent reservation {target_id}",
                        reservation.reservation_id
                    ))
                })?;
            if !replacement_targets.insert(target_id)
                || target.owner != reservation.owner
                || target.pid == reservation.pid
                || target.replaces_reservation_id.is_some()
            {
                return Err(config_error(format!(
                    "replacement reservation {} violates single-owner acyclic replacement invariants",
                    reservation.reservation_id
                )));
            }
        }
        if effective_reserved_mib(state)? > state.epoch_capacity_mib
            || state.admitted_total < state.reservations.len() as u64
            || state.stale_reaped_total > state.admitted_total
        {
            return Err(config_error(
                "GPU reservation source of truth violates aggregate/counter invariants".to_string(),
            ));
        }
        if let Some(rejection) = &state.last_rejection {
            validate_identity("last_rejection.owner", &rejection.owner)?;
            validate_identity("last_rejection.job_id", &rejection.job_id)?;
            validate_identity("last_rejection.command", &rejection.command)?;
            if rejection.pid == 0
                || rejection.requested_mib == 0
                || rejection.at_unix_ms == 0
                || rejection.reason.trim().is_empty()
            {
                return Err(config_error(
                    "GPU reservation last_rejection violates identity/lifetime invariants"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn ensure_same_device(
        &self,
        state: &PersistedState,
        physical: &PhysicalDevice,
    ) -> Result<()> {
        if state.device_index != self.device_index
            || state.device_uuid != physical.uuid
            || state.device_total_mib != physical.total_mib
        {
            return Err(config_error(format!(
                "GPU reservation device identity changed: stored index={} uuid={} total_mib={}, observed index={} uuid={} total_mib={}",
                state.device_index,
                state.device_uuid,
                state.device_total_mib,
                self.device_index,
                physical.uuid,
                physical.total_mib
            )));
        }
        Ok(())
    }

    /// Reconciles the persisted aggregate host cap against the cap observed for
    /// this process, *after* [`Self::prune_stale`] has proven which
    /// reservations are actually live.
    ///
    /// Changing the cap while real reservations are outstanding stays a hard
    /// error: their owners were admitted against the old budget, so silently
    /// re-basing it could let the live aggregate exceed a cap nobody was
    /// admitted under. The correctness hinge is *when* that guard is evaluated.
    /// A persisted row only proves a live owner once its lease lock has been
    /// probed — the OS releases those locks when the holder dies, including
    /// across a host reboot. Evaluating the guard against unprobed rows turned
    /// every ungraceful shutdown that also changed the effective cap into a
    /// permanent startup deadlock: the lease outlives the reboot, the cap no
    /// longer matches, admission is refused, and the refusal happens before the
    /// reaper that would have cleared the stale row ever runs.
    pub(super) fn reconcile_host_cap(
        &self,
        state: &mut PersistedState,
        physical: &PhysicalDevice,
        host_cap_mib: u64,
    ) -> Result<()> {
        if !state.reservations.is_empty() {
            if state.host_cap_mib != host_cap_mib {
                return Err(config_error(format!(
                    "{HOST_CAP_MIB_ENV} changed from {} to {} while {} reservation(s) remain live after stale-lease reaping: {}",
                    state.host_cap_mib,
                    host_cap_mib,
                    state.reservations.len(),
                    reservation_identities(state)
                )));
            }
            return Ok(());
        }
        if state.host_cap_mib != host_cap_mib {
            tracing::info!(
                code = "CALYX_FORGE_HOST_CAP_REBASED",
                device_index = self.device_index,
                previous_host_cap_mib = state.host_cap_mib,
                host_cap_mib,
                device_total_mib = physical.total_mib,
                physical_free_mib = physical.free_mib,
                "rebased the aggregate host GPU cap because no reservation remains live"
            );
        }
        state.host_cap_mib = host_cap_mib;
        state.epoch_free_mib = physical.free_mib;
        state.epoch_capacity_mib = host_cap_mib.min(
            physical
                .free_mib
                .saturating_sub(DEFAULT_REQUIRED_FREE_MIB)
                .saturating_sub(DEFAULT_HOST_HEADROOM_MIB),
        );
        Ok(())
    }

    pub(super) fn prune_stale(&self, state: &mut PersistedState) -> Result<u64> {
        let mut live = Vec::with_capacity(state.reservations.len());
        let mut stale = 0_u64;
        for reservation in state.reservations.drain(..) {
            let lease_path = PathBuf::from(&reservation.lease_file);
            let Some(lease) = open_existing_lock_file(&lease_path)? else {
                // No file means no lock: the row cannot correspond to a held
                // lease. Reaping it is the only outcome that lets the ledger
                // recover; refusing here would strand admission permanently.
                tracing::warn!(
                    code = "CALYX_FORGE_HOST_RESERVATION_LEASE_ABSENT",
                    device_index = self.device_index,
                    reservation_id = %reservation.reservation_id,
                    owner = %reservation.owner,
                    job_id = %reservation.job_id,
                    pid = reservation.pid,
                    requested_mib = reservation.requested_mib,
                    lease_file = %lease_path.display(),
                    "reaped a GPU reservation whose lease file is absent; no process can hold a lock on a file that does not exist"
                );
                stale = stale.saturating_add(1);
                continue;
            };
            match lease.try_lock() {
                Ok(()) => {
                    lease.unlock().map_err(|error| {
                        io_error(
                            "unlock stale GPU lease",
                            &lease_path,
                            error,
                            "inspect the host lease filesystem",
                        )
                    })?;
                    if let Err(error) = fs::remove_file(&lease_path)
                        && error.kind() != ErrorKind::NotFound
                    {
                        return Err(io_error(
                            "remove stale GPU lease",
                            &lease_path,
                            error,
                            "fix lease file permissions before retrying",
                        ));
                    }
                    tracing::info!(
                        code = "CALYX_FORGE_HOST_RESERVATION_REAPED",
                        device_index = self.device_index,
                        reservation_id = %reservation.reservation_id,
                        owner = %reservation.owner,
                        job_id = %reservation.job_id,
                        pid = reservation.pid,
                        requested_mib = reservation.requested_mib,
                        acquired_unix_ms = reservation.acquired_unix_ms,
                        lease_file = %lease_path.display(),
                        "reaped a GPU reservation whose lease lock was no longer held by any live process"
                    );
                    stale = stale.saturating_add(1);
                }
                Err(std::fs::TryLockError::WouldBlock) => live.push(reservation),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(io_error(
                        "probe GPU lease liveness",
                        &lease_path,
                        error,
                        "inspect the host lease filesystem; liveness is unknown so admission is refused",
                    ));
                }
            }
        }
        state.reservations = live;
        let live_ids = state
            .reservations
            .iter()
            .map(|row| row.reservation_id.clone())
            .collect::<BTreeSet<_>>();
        for reservation in &mut state.reservations {
            if reservation
                .replaces_reservation_id
                .as_deref()
                .is_some_and(|target| !live_ids.contains(target))
            {
                reservation.replaces_reservation_id = None;
            }
        }
        Ok(stale)
    }

    pub(super) fn persist_and_verify(
        &self,
        state: &PersistedState,
    ) -> Result<HostGpuReservationSnapshot> {
        let path = self.state_path();
        let mut bytes = serde_json::to_vec_pretty(state).map_err(|error| {
            config_error(format!(
                "serialize GPU reservation source of truth: {error}"
            ))
        })?;
        bytes.push(b'\n');
        self.remove_stale_state_temps()?;
        let temp_path = self.device_root().join(format!(
            ".reservations.json.tmp-{}-{}",
            std::process::id(),
            state.last_updated_unix_ms
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|error| {
                io_error(
                    "create GPU reservation state transaction",
                    &temp_path,
                    error,
                    "fix directory ownership/permissions before retrying",
                )
            })?;
        file.write_all(&bytes).map_err(|error| {
            io_error(
                "write GPU reservation state transaction",
                &temp_path,
                error,
                "inspect disk pressure and filesystem health",
            )
        })?;
        file.sync_all().map_err(|error| {
            io_error(
                "sync GPU reservation state transaction",
                &temp_path,
                error,
                "inspect disk pressure and filesystem health",
            )
        })?;
        drop(file);
        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(io_error(
                "atomically replace GPU reservation source of truth",
                &path,
                error,
                "inspect filesystem health; state remains at the previous committed generation",
            ));
        }
        #[cfg(unix)]
        File::open(self.device_root())
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                io_error(
                    "sync GPU reservation directory",
                    &self.device_root(),
                    error,
                    "inspect filesystem health; directory durability is unprovable",
                )
            })?;
        let mut readback = Vec::new();
        File::open(&path)
            .and_then(|mut source| source.read_to_end(&mut readback))
            .map_err(|error| {
                io_error(
                    "read back GPU reservation source of truth",
                    &path,
                    error,
                    "inspect filesystem health; the write is not considered committed",
                )
            })?;
        if readback != bytes {
            return Err(config_error(format!(
                "GPU reservation write/readback bytes differ for {}",
                path.display()
            )));
        }
        let decoded: PersistedState = serde_json::from_slice(&readback).map_err(|error| {
            config_error(format!(
                "GPU reservation readback decode failed for {}: {error}",
                path.display()
            ))
        })?;
        if &decoded != state {
            return Err(config_error(format!(
                "GPU reservation decoded readback differs for {}",
                path.display()
            )));
        }
        snapshot(state, &path, &readback)
    }

    fn remove_stale_state_temps(&self) -> Result<()> {
        for entry in fs::read_dir(self.device_root()).map_err(|error| {
            io_error(
                "scan GPU reservation state transactions",
                &self.device_root(),
                error,
                "fix directory ownership/permissions before retrying",
            )
        })? {
            let entry = entry.map_err(|error| {
                io_error(
                    "read GPU reservation state transaction entry",
                    &self.device_root(),
                    error,
                    "inspect filesystem health before retrying",
                )
            })?;
            let name = entry.file_name();
            if !name
                .to_str()
                .is_some_and(|value| value.starts_with(".reservations.json.tmp-"))
            {
                continue;
            }
            let path = entry.path();
            fs::remove_file(&path).map_err(|error| {
                io_error(
                    "remove stale GPU reservation state transaction",
                    &path,
                    error,
                    "fix directory ownership/permissions before retrying",
                )
            })?;
        }
        Ok(())
    }
}

fn validate_identity(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(config_error(format!(
            "persisted GPU reservation {field} must be non-blank, control-free, and at most 512 bytes"
        )));
    }
    Ok(())
}

fn valid_reservation_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
