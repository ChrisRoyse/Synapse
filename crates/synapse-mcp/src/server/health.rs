use super::{BTreeMap, ErrorData, Health, SubsystemHealth, SynapseService};
use rmcp::model::Tool;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::sync::TryLockError;
use synapse_action::BackendResolutionPolicy;
use synapse_core::{Backend, CalyxMathProbeTopKEntry, ChromeBridgeDetail};

/// Verbosity control for the `health` tool response.
///
/// `health` is called frequently and its verbose per-subsystem `detail` prose
/// dominates the payload token cost (issue #1554). `Compact` (the default)
/// keeps every structured verdict field but drops the long human-readable
/// `detail` blobs, so callers still learn the health conclusion at a fraction
/// of the wire size. `Full` preserves the complete legacy output for
/// debugging.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HealthDetail {
    /// Drop verbose per-subsystem `detail` prose; keep structured verdicts.
    #[default]
    Compact,
    /// Preserve every `detail` string (the legacy behavior).
    Full,
}

/// Request parameters for the `health` tool.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct HealthParams {
    /// `compact` (default) trims verbose per-subsystem detail prose while
    /// keeping every structured status field; `full` returns the complete
    /// diagnostic detail for every subsystem.
    pub detail: HealthDetail,
}

fn state_lock_unavailable_health<T>(
    state_name: &'static str,
    error: TryLockError<T>,
) -> SubsystemHealth {
    let detail = match error {
        TryLockError::WouldBlock => format!(
            "{state_name} state lock is busy; health is fail-closed and does not wait behind in-flight work"
        ),
        TryLockError::Poisoned(_poisoned) => {
            format!("{state_name} state lock poisoned")
        }
    };
    SubsystemHealth {
        status: "error".to_owned(),
        detail: Some(detail),
        ..SubsystemHealth::default()
    }
}

fn runtime_lock_unavailable_health<T>(
    runtime_name: &'static str,
    error: TryLockError<T>,
) -> SubsystemHealth {
    let detail = match &error {
        TryLockError::WouldBlock => format!(
            "{runtime_name} runtime lock is busy; health reports busy and does not wait behind in-flight work"
        ),
        TryLockError::Poisoned(_poisoned) => {
            format!("{runtime_name} runtime lock poisoned")
        }
    };
    SubsystemHealth {
        status: match &error {
            TryLockError::WouldBlock => "busy",
            TryLockError::Poisoned(_poisoned) => "error",
        }
        .to_owned(),
        detail: Some(detail),
        ..SubsystemHealth::default()
    }
}

fn storage_pressure_status(level: synapse_storage::DiskPressureLevel) -> String {
    match level {
        synapse_storage::DiskPressureLevel::Normal => "ok",
        synapse_storage::DiskPressureLevel::Level1 => "disk_pressure_l1",
        synapse_storage::DiskPressureLevel::Level2 => "disk_pressure_l2",
        synapse_storage::DiskPressureLevel::Level3 => "disk_pressure_l3",
        synapse_storage::DiskPressureLevel::Level4 => "disk_pressure_l4",
    }
    .to_owned()
}

fn storage_maintenance_error(readback: &crate::m3::StorageMaintenanceReadback) -> Option<String> {
    let mut reasons = Vec::new();
    let pressure_probe_active = storage_pressure_probe_active(readback);
    if readback.maintenance_supported && !readback.gc_task_running {
        reasons.push("storage GC task is not running".to_owned());
    }
    if readback.maintenance_supported && !readback.checkpoint_task_running {
        reasons.push("storage checkpoint task is not running".to_owned());
    }
    if !readback.pressure_task_running {
        reasons.push("storage pressure task is not running".to_owned());
    }
    if !readback.pressure_probe.observed && !pressure_probe_active {
        reasons.push("storage pressure probe has not completed successfully".to_owned());
    }
    if readback.maintenance_supported
        && let Some(error) = &readback.gc_task.last_error
    {
        reasons.push(format!("storage GC last_error={error}"));
    }
    if readback.maintenance_supported
        && let Some(error) = &readback.checkpoint_task.last_error
    {
        reasons.push(format!("storage checkpoint last_error={error}"));
    }
    if let Some(error) = &readback.pressure_probe.last_error {
        reasons.push(format!("storage pressure last_error={error}"));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

fn storage_tick_active(started: Option<u64>, completed: Option<u64>) -> bool {
    started.is_some_and(|started| completed.is_none_or(|completed| started > completed))
}

fn storage_gc_tick_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_tick_active(
        readback.gc_task.last_started_unix_ms,
        readback.gc_task.last_completed_unix_ms,
    )
}

fn storage_checkpoint_tick_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_tick_active(
        readback.checkpoint_task.last_started_unix_ms,
        readback.checkpoint_task.last_completed_unix_ms,
    )
}

fn storage_pressure_probe_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_tick_active(
        readback.pressure_probe.last_started_unix_ms,
        readback.pressure_probe.last_completed_unix_ms,
    )
}

fn storage_maintenance_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_gc_tick_active(readback)
        || storage_checkpoint_tick_active(readback)
        || storage_pressure_probe_active(readback)
}

fn calyx_health_cf_sizes_skipped_reason() -> String {
    "calyx backend health skips scan-bound CF size estimates; use storage summary/inspect for explicit storage readback".to_owned()
}

fn apply_storage_maintenance_fields(
    health: &mut SubsystemHealth,
    readback: &crate::m3::StorageMaintenanceReadback,
) {
    health.storage_maintenance_supported = Some(readback.maintenance_supported);
    health.storage_maintenance_unsupported_reason = readback.unsupported_reason.clone();
    health.storage_gc_task_running = Some(readback.gc_task_running);
    health.storage_gc_tick_active = Some(storage_gc_tick_active(readback));
    health.storage_checkpoint_task_running = Some(readback.checkpoint_task_running);
    health.storage_checkpoint_tick_active = Some(storage_checkpoint_tick_active(readback));
    health.storage_pressure_task_running = Some(readback.pressure_task_running);
    health.storage_pressure_probe_active = Some(storage_pressure_probe_active(readback));
    health.storage_pressure_probe_observed = Some(readback.pressure_probe.observed);
    health.storage_pressure_last_free_bytes = readback.pressure_probe.last_free_bytes;
    health.storage_pressure_last_level = readback
        .pressure_probe
        .last_level
        .map(|level| format!("{level:?}"));
    health.storage_gc_last_started_unix_ms = readback.gc_task.last_started_unix_ms;
    health.storage_gc_last_completed_unix_ms = readback.gc_task.last_completed_unix_ms;
    health.storage_gc_last_duration_ms = readback.gc_task.last_duration_ms;
    health.storage_gc_last_error = readback.gc_task.last_error.clone();
    health.storage_gc_last_error_classification =
        readback.gc_task.last_error_classification.clone();
    health.storage_gc_last_attempt_count = Some(readback.gc_task.last_attempt_count);
    health.storage_gc_next_retry_unix_ms = readback.gc_task.next_retry_unix_ms;
    health.storage_gc_retry_exhausted = Some(readback.gc_task.retry_exhausted);
    health.storage_gc_last_successful_unix_ms = readback.gc_task.last_successful_unix_ms;
    health.storage_gc_last_successful_cf_readback_count =
        readback.gc_task.last_successful_cf_readback_count;
    health.storage_gc_last_successful_total_examined_rows =
        readback.gc_task.last_successful_total_examined_rows;
    health.storage_gc_last_successful_total_evicted_rows =
        readback.gc_task.last_successful_total_evicted_rows;
    health.storage_gc_last_successful_after_value_sum =
        readback.gc_task.last_successful_after_value_sum;
    health.storage_gc_last_unsupported_policy_skips =
        readback.gc_task.last_unsupported_policy_skips.clone();
    health.storage_checkpoint_last_started_unix_ms = readback.checkpoint_task.last_started_unix_ms;
    health.storage_checkpoint_last_completed_unix_ms =
        readback.checkpoint_task.last_completed_unix_ms;
    health.storage_checkpoint_last_duration_ms = readback.checkpoint_task.last_duration_ms;
    health.storage_checkpoint_last_error = readback.checkpoint_task.last_error.clone();
    health.storage_checkpoint_last_error_classification =
        readback.checkpoint_task.last_error_classification.clone();
    health.storage_checkpoint_last_attempt_count =
        Some(readback.checkpoint_task.last_attempt_count);
    health.storage_checkpoint_next_retry_unix_ms = readback.checkpoint_task.next_retry_unix_ms;
    health.storage_checkpoint_retry_exhausted = Some(readback.checkpoint_task.retry_exhausted);
    health.storage_checkpoint_last_successful_unix_ms =
        readback.checkpoint_task.last_successful_unix_ms;
    health.storage_checkpoint_last_successful_cf_readback_count =
        readback.checkpoint_task.last_successful_cf_readback_count;
    health.storage_checkpoint_last_successful_total_examined_rows =
        readback.checkpoint_task.last_successful_total_examined_rows;
    health.storage_checkpoint_last_successful_total_evicted_rows =
        readback.checkpoint_task.last_successful_total_evicted_rows;
    health.storage_checkpoint_last_successful_after_value_sum =
        readback.checkpoint_task.last_successful_after_value_sum;
    health.storage_pressure_last_started_unix_ms = readback.pressure_probe.last_started_unix_ms;
    health.storage_pressure_last_completed_unix_ms = readback.pressure_probe.last_completed_unix_ms;
    health.storage_pressure_last_duration_ms = readback.pressure_probe.last_duration_ms;
    health.storage_pressure_last_error = readback.pressure_probe.last_error.clone();
}

impl SynapseService {
    pub(crate) fn health_payload_for_session(
        &self,
        session_id: Option<&str>,
        detail: HealthDetail,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session_detail(None, session_id, detail, None)
    }

    pub(crate) fn health_payload_with_http_sessions_and_error(
        &self,
        active_sessions: Option<usize>,
        http_session_read_error: Option<String>,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session_detail(
            active_sessions,
            None,
            HealthDetail::Full,
            http_session_read_error,
        )
    }

    pub(crate) fn health_payload_with_http_sessions_and_session_detail(
        &self,
        active_sessions: Option<usize>,
        session_id: Option<&str>,
        detail: HealthDetail,
        http_session_read_error: Option<String>,
    ) -> Health {
        let mut subsystems = BTreeMap::new();
        subsystems.insert("storage".to_owned(), self.storage_health());
        subsystems.insert("calyx_vault".to_owned(), self.calyx_vault_health());
        subsystems.insert("reflex".to_owned(), self.reflex_health());
        subsystems.insert("profiles".to_owned(), self.profile_health());
        subsystems.insert("perception".to_owned(), self.perception_health());
        subsystems.insert("action".to_owned(), self.action_health());
        subsystems.insert("audio".to_owned(), self.audio_health());
        subsystems.insert(
            "chrome_bridge".to_owned(),
            crate::chrome_debugger_bridge::health_subsystem(),
        );
        subsystems.insert(
            "http".to_owned(),
            self.http_health(active_sessions, http_session_read_error),
        );
        subsystems.insert("daemon_drain".to_owned(), self.daemon_drain_health());
        subsystems.insert(
            "daemon_lifecycle".to_owned(),
            crate::daemon_lifecycle::health_subsystem(),
        );
        subsystems.insert(
            "public_tool_registry".to_owned(),
            self.public_tool_registry_health(),
        );
        subsystems.insert("facade_contract".to_owned(), self.facade_contract_health());
        let tool_surface = self.tool_surface_fingerprint(session_id);
        if let Some(error) = &tool_surface.error {
            subsystems.insert(
                "tool_surface".to_owned(),
                SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(error.clone()),
                    ..SubsystemHealth::default()
                },
            );
        }
        let ok = subsystems.values().all(|health| health.status != "error");
        apply_health_detail(&mut subsystems, detail);
        Health {
            ok,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: option_env!("VERGEN_GIT_SHA").unwrap_or("dev").to_owned(),
            pid: std::process::id(),
            uptime_s: self.started_at.elapsed().as_secs(),
            tool_count: tool_surface.names.len(),
            tool_surface_sha256: tool_surface.sha256,
            tool_names: tool_surface.names,
            subsystems,
        }
    }

    fn calyx_vault_health(&self) -> SubsystemHealth {
        let status = match self.m3_state.try_lock() {
            Ok(state) => state.calyx_vault_status(),
            Err(error) => return state_lock_unavailable_health("M3", error),
        };
        let health_status = if !status.enabled {
            "disabled"
        } else if status.last_error_code.is_some() {
            "error"
        } else if status.open {
            "ok"
        } else {
            "starting"
        };
        let tuning = status.tuning;
        let math_backend = status.math_backend;
        let gpu_reservation_snapshot = math_backend
            .as_ref()
            .and_then(|math| math.host_reservation.as_ref());
        let gpu_reservation = math_backend.as_ref().and_then(|math| {
            let reservation_id = math.host_reservation_id.as_deref()?;
            math.host_reservation
                .as_ref()?
                .reservations
                .iter()
                .find(|row| row.reservation_id == reservation_id)
        });
        SubsystemHealth {
            status: health_status.to_owned(),
            detail: Some(format!(
                "enabled={} phase={} open={} vault_dir={} vault_id={} latest_seq={:?} last_recovered_seq={:?} torn_tail={} last_error_code={} last_calyx_error_code={} clock_mode={} bit_floor_bits={:?} correlation_ceiling={:?} math={} remediation={}",
                status.enabled,
                status.phase,
                status.open,
                status
                    .vault_dir
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |path| path.display().to_string()),
                status.vault_id.as_deref().unwrap_or("none"),
                status.latest_seq,
                status.last_recovered_seq,
                status.torn_tail.as_deref().unwrap_or("none"),
                status.last_error_code.as_deref().unwrap_or("none"),
                status.last_calyx_error_code.as_deref().unwrap_or("none"),
                tuning.map_or("none", |config| config.clock_mode.as_str()),
                tuning.map(|config| config.bit_floor_bits),
                tuning.map(|config| config.correlation_ceiling),
                math_backend
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |math| math.detail()),
                status.remediation.as_deref().unwrap_or("none")
            )),
            calyx_vault_open: Some(status.open),
            calyx_vault_phase: Some(status.phase),
            calyx_vault_path: status.vault_dir.map(|path| path.display().to_string()),
            calyx_vault_identity_path: status.identity_path.map(|path| path.display().to_string()),
            calyx_machine_salt_path: status
                .machine_salt_path
                .map(|path| path.display().to_string()),
            calyx_vault_lock_path: status.lock_path.map(|path| path.display().to_string()),
            calyx_vault_pid_path: status.pid_path.map(|path| path.display().to_string()),
            calyx_vault_id: status.vault_id,
            calyx_vault_latest_seq: status.latest_seq,
            calyx_vault_last_recovered_seq: status.last_recovered_seq,
            calyx_vault_torn_tail: status.torn_tail,
            calyx_vault_last_error_code: status.last_error_code,
            calyx_vault_last_calyx_error_code: status.last_calyx_error_code,
            calyx_vault_last_error: status.last_error,
            calyx_vault_remediation: status.remediation,
            calyx_bit_floor_bits: tuning.map(|config| config.bit_floor_bits),
            calyx_correlation_ceiling: tuning.map(|config| config.correlation_ceiling),
            calyx_guard_far_identity: tuning.map(|config| config.guard_far_identity),
            calyx_guard_far_content: tuning.map(|config| config.guard_far_content),
            calyx_guard_far_stylistic: tuning.map(|config| config.guard_far_stylistic),
            calyx_guard_cold_start_tau: tuning.map(|config| config.guard_cold_start_tau),
            calyx_kernel_fraction: tuning.map(|config| config.kernel_fraction),
            calyx_kernel_recall_gate: tuning.map(|config| config.kernel_recall_gate),
            calyx_fusion_k: tuning.map(|config| config.fusion_k),
            calyx_temporal_boost_min: tuning.map(|config| config.temporal_boost_min),
            calyx_temporal_boost_max: tuning.map(|config| config.temporal_boost_max),
            calyx_vram_budget_bytes: tuning.map(|config| config.vram_budget_bytes),
            calyx_vram_budget_enforced: math_backend
                .as_ref()
                .map(|math| math.vram_dispatch.is_some()),
            calyx_vram_dispatch_soft_cap_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.soft_cap_bytes),
            calyx_vram_dispatch_allocated_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.allocated_bytes),
            calyx_vram_dispatch_serving_allocated_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.serving_allocated_bytes),
            calyx_vram_dispatch_anneal_allocated_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.anneal_allocated_bytes),
            calyx_vram_dispatch_device_free_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.device_free_bytes),
            calyx_gpu_reservation_basis: math_backend
                .as_ref()
                .and_then(|math| math.host_reservation_basis.clone()),
            calyx_gpu_reservation_state_path: gpu_reservation_snapshot
                .map(|snapshot| snapshot.state_path.clone()),
            calyx_gpu_reservation_state_sha256: gpu_reservation_snapshot
                .map(|snapshot| snapshot.state_sha256.clone()),
            calyx_gpu_reservation_device_index: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_index),
            calyx_gpu_reservation_device_uuid: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_uuid.clone()),
            calyx_gpu_reservation_device_name: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_name.clone()),
            calyx_gpu_reservation_device_total_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_total_mib),
            calyx_gpu_reservation_host_cap_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.host_cap_mib),
            calyx_gpu_reservation_required_free_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.required_free_mib),
            calyx_gpu_reservation_headroom_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.headroom_mib),
            calyx_gpu_reservation_last_physical_free_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.last_physical_free_mib),
            calyx_gpu_reservation_reserved_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.reserved_mib),
            calyx_gpu_reservation_available_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.available_reservation_mib),
            calyx_gpu_reservation_admitted_total: gpu_reservation_snapshot
                .map(|snapshot| snapshot.admitted_total),
            calyx_gpu_reservation_rejected_total: gpu_reservation_snapshot
                .map(|snapshot| snapshot.rejected_total),
            calyx_gpu_reservation_stale_reaped_total: gpu_reservation_snapshot
                .map(|snapshot| snapshot.stale_reaped_total),
            calyx_gpu_reservation_id: gpu_reservation
                .map(|reservation| reservation.reservation_id.clone()),
            calyx_gpu_reservation_owner: gpu_reservation
                .map(|reservation| reservation.owner.clone()),
            calyx_gpu_reservation_job_id: gpu_reservation
                .map(|reservation| reservation.job_id.clone()),
            calyx_gpu_reservation_command: gpu_reservation
                .map(|reservation| reservation.command.clone()),
            calyx_gpu_reservation_pid: gpu_reservation.map(|reservation| reservation.pid),
            calyx_gpu_reservation_requested_mib: gpu_reservation
                .map(|reservation| reservation.requested_mib),
            calyx_gpu_reservation_acquired_unix_ms: gpu_reservation
                .map(|reservation| reservation.acquired_unix_ms.to_string()),
            calyx_gpu_reservation_lease_file: gpu_reservation
                .map(|reservation| reservation.lease_file.clone()),
            calyx_gpu_reservation_last_rejection: gpu_reservation_snapshot
                .and_then(|snapshot| snapshot.last_rejection.as_ref())
                .map(|rejection| {
                    format!(
                        "owner={} job_id={} command={} pid={} requested_mib={} at_unix_ms={} reason={}",
                        rejection.owner,
                        rejection.job_id,
                        rejection.command,
                        rejection.pid,
                        rejection.requested_mib,
                        rejection.at_unix_ms,
                        rejection.reason
                    )
                }),
            calyx_gpu_runtime_readback_code: math_backend
                .as_ref()
                .and_then(|math| math.runtime_readback_code.clone()),
            calyx_gpu_runtime_readback_error: math_backend
                .as_ref()
                .and_then(|math| math.runtime_readback_error.clone()),
            calyx_math_backend: math_backend
                .as_ref()
                .map(|math| math.selected_backend.clone()),
            calyx_math_backend_requested: math_backend
                .as_ref()
                .map(|math| math.requested_backend.as_str().to_owned())
                .or_else(|| tuning.map(|config| config.math_backend.as_str().to_owned())),
            calyx_math_cuda_compiled: math_backend.as_ref().map(|math| math.cuda_compiled),
            calyx_math_device_name: math_backend.as_ref().map(|math| math.device_name.clone()),
            calyx_math_device_vram_mib: math_backend.as_ref().and_then(|math| math.device_vram_mib),
            calyx_math_device_avx512: math_backend.as_ref().map(|math| math.device_avx512),
            calyx_math_cpu_avx512_available: math_backend
                .as_ref()
                .map(|math| math.cpu_avx512_available),
            calyx_math_cpu_simd_path: math_backend.as_ref().map(|math| math.cpu_simd_path.clone()),
            calyx_math_fallback_code: math_backend
                .as_ref()
                .and_then(|math| math.fallback_code.clone()),
            calyx_math_fallback_source_code: math_backend
                .as_ref()
                .and_then(|math| math.fallback_source_code.clone()),
            calyx_math_fallback_error: math_backend
                .as_ref()
                .and_then(|math| math.fallback_error.clone()),
            calyx_math_probe_status: math_backend.as_ref().map(|math| math.probe.status.clone()),
            calyx_math_probe_detail: math_backend.as_ref().map(|math| math.probe.detail.clone()),
            calyx_math_probe_tolerance: math_backend.as_ref().map(|math| math.probe.tolerance),
            calyx_math_probe_dot: math_backend.as_ref().map(|math| math.probe.dot.clone()),
            calyx_math_probe_cosine: math_backend.as_ref().map(|math| math.probe.cosine.clone()),
            calyx_math_probe_l2_squared: math_backend
                .as_ref()
                .map(|math| math.probe.l2_squared.clone()),
            calyx_math_probe_topk: math_backend.as_ref().map(|math| {
                math.probe
                    .topk
                    .iter()
                    .map(|entry| CalyxMathProbeTopKEntry {
                        index: entry.index,
                        score: entry.score,
                    })
                    .collect()
            }),
            calyx_clock_mode: tuning.map(|config| config.clock_mode.as_str().to_owned()),
            calyx_fixed_clock_unix_ms: tuning.and_then(|config| config.fixed_clock_unix_ms),
            calyx_rng_seed: tuning.map(|config| config.rng_seed),
            ..SubsystemHealth::default()
        }
    }

    fn public_tool_registry_health(&self) -> SubsystemHealth {
        match self.public_tool_registry_snapshot() {
            Ok(snapshot) => {
                let missing_count = snapshot.registered_tools_missing.len();
                let status = if missing_count == 0 {
                    "ok"
                } else {
                    "pending_facades"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!(
                        "source_of_truth={} public_tool_count={} max_public_tool_count={} implementation_tool_count={} registered_tools_present={} registered_tools_missing={}",
                        snapshot.source_of_truth,
                        snapshot.public_tool_count,
                        snapshot.max_public_tool_count,
                        snapshot.implementation_tool_count,
                        snapshot.registered_tools_present.len(),
                        missing_count
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("{error:?}")),
                ..SubsystemHealth::default()
            },
        }
    }

    fn facade_contract_health(&self) -> SubsystemHealth {
        match Self::facade_contract_snapshot() {
            Ok(snapshot) => {
                let invalid_count = snapshot.missing_contract_tool_names.len()
                    + snapshot.unknown_contract_tool_names.len()
                    + snapshot.duplicate_contract_tool_names.len()
                    + snapshot.duplicate_operation_names.len()
                    + snapshot.invalid_contract_reasons.len();
                let status = if invalid_count == 0 { "ok" } else { "error" };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!(
                        "source_of_truth={} public_tool_count={} contract_tool_count={} operation_count={} mutating_operation_count={} invalid_count={} contract_sha256={}",
                        snapshot.source_of_truth,
                        snapshot.public_tool_count,
                        snapshot.contract_tool_count,
                        snapshot.operation_count,
                        snapshot.mutating_operation_count,
                        invalid_count,
                        snapshot.facade_contract_sha256
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("{error:?}")),
                ..SubsystemHealth::default()
            },
        }
    }

    fn daemon_drain_health(&self) -> SubsystemHealth {
        let snapshot = self.drain_state_handle().snapshot();
        let status = if snapshot.state_error.is_some() {
            "error"
        } else if snapshot.draining {
            "draining"
        } else {
            "ok"
        };
        let detail = if let Some(error) = snapshot.state_error {
            error
        } else if snapshot.draining {
            format!(
                "reason_code={} source={} started_at_unix_ms={}",
                snapshot.reason_code.unwrap_or("unknown"),
                snapshot.source.unwrap_or("unknown"),
                snapshot.started_at_unix_ms.unwrap_or_default()
            )
        } else {
            "daemon accepting work".to_owned()
        };
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(detail),
            ..SubsystemHealth::default()
        }
    }

    fn tool_surface_fingerprint(&self, session_id: Option<&str>) -> ToolSurfaceFingerprint {
        if session_id.is_none() {
            return self.immutable_tool_surface_fingerprint();
        }
        let tools = match self.health_tool_surface(session_id) {
            Ok(tools) => tools,
            Err(error) => {
                tracing::error!(
                    code = "MCP_TOOL_SURFACE_HEALTH_READ_FAILED",
                    session_id,
                    error = ?error,
                    "failed to resolve MCP health tool surface"
                );
                return ToolSurfaceFingerprint {
                    names: Vec::new(),
                    sha256: "TOOL_SURFACE_HEALTH_READ_FAILED".to_owned(),
                    error: Some(format!(
                        "failed to resolve MCP health tool surface: {error}"
                    )),
                };
            }
        };
        tool_surface_fingerprint_for_tools(tools)
    }

    /// Report the *exact* tool surface the client is served.
    ///
    /// Health's `tool_names`/`tool_count`/`tool_surface_sha256` must mirror what
    /// `tools/list` actually returns for the same session, or health lies about
    /// the surface. The served surface (see `ServerHandler::list_tools`) is
    /// `tools_for_session_profile(session_id)` for every session — including the
    /// unscoped stdio/admin case where `session_id` is `None` and the full
    /// break-glass surface (raw `act_*` primitives such as `act_run_shell_status`
    /// included) is served. Deriving health from that single source of truth
    /// makes the two surfaces identical by construction, closing the drift class
    /// where a hand-maintained parallel list (previously a `public_tool_names`
    /// filter for the `None` case) silently diverged from what was served
    /// (issue #1612).
    fn health_tool_surface(&self, session_id: Option<&str>) -> Result<Vec<Tool>, ErrorData> {
        self.tools_for_session_profile(session_id)
    }

    fn storage_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                let db_path = state
                    .db_path
                    .as_ref()
                    .map(|path| path.display().to_string());
                let storage_backend = state
                    .db
                    .as_ref()
                    .map_or(state.storage_backend.as_str(), |db| db.backend_name())
                    .to_owned();
                let maintenance = state.storage_maintenance_readback();
                if let Some(error) = &state.storage_last_error {
                    let mut health = SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        db_path,
                        storage_backend: Some(storage_backend),
                        ..SubsystemHealth::default()
                    };
                    apply_storage_maintenance_fields(&mut health, &maintenance);
                    return health;
                }
                let Some(runtime) = &state.reflex_runtime else {
                    if state.db.is_some() {
                        let maintenance_error = storage_maintenance_error(&maintenance);
                        let maintenance_unsupported = maintenance.unsupported_reason.clone();
                        let maintenance_active = storage_maintenance_active(&maintenance);
                        let cf_sizes_skipped_reason =
                            (storage_backend == "calyx").then(calyx_health_cf_sizes_skipped_reason);
                        let cf_sizes = if cf_sizes_skipped_reason.is_some() {
                            None
                        } else {
                            state.db.as_ref().and_then(|db| {
                                db.cf_live_data_size_estimates()
                                    .ok()
                                    .map(|(sizes, _)| sizes)
                            })
                        };
                        let mut health = SubsystemHealth {
                            status: if maintenance_error.is_some() {
                                "error".to_owned()
                            } else if maintenance_unsupported.is_some() {
                                "maintenance_unsupported".to_owned()
                            } else if maintenance_active {
                                "maintenance".to_owned()
                            } else {
                                "ok".to_owned()
                            },
                            detail: Some(match (
                                maintenance_error,
                                maintenance_unsupported,
                                maintenance_active,
                            ) {
                                (Some(error), _, _) => format!(
                                    "storage opened at daemon startup (reflex runtime idle); maintenance unhealthy: {error}"
                                ),
                                (None, Some(reason), _) => format!(
                                    "storage opened at daemon startup (reflex runtime idle); maintenance unsupported for this backend: {reason}"
                                ),
                                (None, None, true) => "storage opened at daemon startup (reflex runtime idle); maintenance tick active; health skipped scan-bound CF size readback".to_owned(),
                                (None, None, false) => "storage opened at daemon startup (reflex runtime idle); maintenance tasks running and pressure probe observed".to_owned(),
                            }),
                            db_path,
                            storage_backend: Some(storage_backend),
                            schema_version: Some(synapse_core::SCHEMA_VERSION),
                            cf_sizes,
                            storage_cf_sizes_skipped_reason: cf_sizes_skipped_reason,
                            ..SubsystemHealth::default()
                        };
                        apply_storage_maintenance_fields(&mut health, &maintenance);
                        return health;
                    }
                    let mut health = SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some("storage opens on first reflex tool call".to_owned()),
                        db_path,
                        storage_backend: Some(storage_backend),
                        ..SubsystemHealth::default()
                    };
                    apply_storage_maintenance_fields(&mut health, &maintenance);
                    return health;
                };
                match runtime.try_lock() {
                    Ok(runtime) => {
                        let runtime_backend = runtime.storage_backend_name().to_owned();
                        let runtime_db_path = runtime.storage_path().display().to_string();
                        let runtime_schema_version = runtime.schema_version();
                        let maintenance_error = storage_maintenance_error(&maintenance);
                        let maintenance_unsupported = maintenance.unsupported_reason.clone();
                        let maintenance_active = storage_maintenance_active(&maintenance);
                        let cf_sizes_skipped_reason =
                            (runtime_backend == "calyx").then(calyx_health_cf_sizes_skipped_reason);
                        let cf_sizes = if cf_sizes_skipped_reason.is_some() {
                            None
                        } else {
                            match runtime.storage_cf_live_data_size_estimates() {
                                Ok((sizes, _warnings)) => Some(sizes),
                                Err(error) => {
                                    let mut health = SubsystemHealth {
                                        status: "error".to_owned(),
                                        detail: Some(error.to_string()),
                                        db_path: Some(runtime_db_path),
                                        storage_backend: Some(runtime_backend),
                                        schema_version: Some(runtime_schema_version),
                                        ..SubsystemHealth::default()
                                    };
                                    apply_storage_maintenance_fields(&mut health, &maintenance);
                                    return health;
                                }
                            }
                        };
                        let mut health = SubsystemHealth {
                            status: if maintenance_error.is_some() {
                                "error".to_owned()
                            } else if maintenance_unsupported.is_some() {
                                "maintenance_unsupported".to_owned()
                            } else if maintenance_active {
                                "maintenance".to_owned()
                            } else {
                                storage_pressure_status(runtime.storage_pressure_level())
                            },
                            detail: Some(match (
                                maintenance_error,
                                maintenance_unsupported,
                                maintenance_active,
                            ) {
                                (Some(error), _, _) => format!(
                                    "storage runtime initialized; maintenance unhealthy: {error}"
                                ),
                                (None, Some(reason), _) => format!(
                                    "storage runtime initialized; maintenance unsupported for this backend: {reason}"
                                ),
                                (None, None, true) => "storage runtime initialized; maintenance tick active; health skipped scan-bound CF size readback".to_owned(),
                                (None, None, false) => "storage runtime initialized; cf_sizes use backend metrics; maintenance tasks running and pressure probe observed".to_owned(),
                            }),
                            db_path: Some(runtime_db_path),
                            storage_backend: Some(runtime_backend),
                            schema_version: Some(runtime_schema_version),
                            cf_sizes,
                            storage_cf_sizes_skipped_reason: cf_sizes_skipped_reason,
                            ..SubsystemHealth::default()
                        };
                        apply_storage_maintenance_fields(&mut health, &maintenance);
                        health
                    }
                    Err(error) => match error {
                        TryLockError::WouldBlock => {
                            let maintenance_error = storage_maintenance_error(&maintenance);
                            let maintenance_unsupported = maintenance.unsupported_reason.clone();
                            let maintenance_active = storage_maintenance_active(&maintenance);
                            let cf_sizes_skipped_reason = (storage_backend == "calyx")
                                .then(calyx_health_cf_sizes_skipped_reason);
                            let mut health = SubsystemHealth {
                                    status: if maintenance_error.is_some() {
                                        "error".to_owned()
                                    } else if maintenance_unsupported.is_some() {
                                        "maintenance_unsupported".to_owned()
                                    } else if maintenance_active {
                                        "maintenance".to_owned()
                                    } else {
                                        maintenance
                                            .pressure_probe
                                            .last_level
                                            .map(storage_pressure_status)
                                            .unwrap_or_else(|| "ok".to_owned())
                                    },
                                    detail: Some(match (
                                        maintenance_error,
                                        maintenance_unsupported,
                                        maintenance_active,
                                    ) {
                                        (Some(error), _, _) => format!(
                                            "storage runtime lock busy; daemon storage handle readback says maintenance unhealthy: {error}"
                                        ),
                                        (None, Some(reason), _) => format!(
                                            "storage runtime lock busy; daemon storage handle readback says maintenance unsupported for this backend: {reason}"
                                        ),
                                        (None, None, true) => "storage runtime lock busy; daemon storage handle is open and maintenance tick active; health skipped scan-bound CF size readback".to_owned(),
                                        (None, None, false) => "storage runtime lock busy; daemon storage handle is open, maintenance tasks are running, and pressure probe was observed".to_owned(),
                                    }),
                                    db_path,
                                    storage_backend: Some(storage_backend),
                                    schema_version: Some(synapse_core::SCHEMA_VERSION),
                                    cf_sizes: None,
                                    storage_cf_sizes_skipped_reason: cf_sizes_skipped_reason,
                                    ..SubsystemHealth::default()
                                };
                            apply_storage_maintenance_fields(&mut health, &maintenance);
                            health
                        }
                        TryLockError::Poisoned(poisoned) => {
                            let mut health = runtime_lock_unavailable_health(
                                "reflex storage",
                                TryLockError::Poisoned(poisoned),
                            );
                            health.db_path = db_path;
                            health.storage_backend = Some(storage_backend);
                            apply_storage_maintenance_fields(&mut health, &maintenance);
                            health
                        }
                    },
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn reflex_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                if let Some(error) = &state.reflex_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                if state.reflex_disabled {
                    return SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("reflex runtime disabled by operator".to_owned()),
                        active_count: Some(0),
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.reflex_runtime else {
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some("reflex runtime starts on first reflex tool call".to_owned()),
                        active_count: Some(0),
                        recursion_clamps_total: Some(0),
                        ..SubsystemHealth::default()
                    };
                };
                match runtime.try_lock() {
                    Ok(runtime) => match runtime.recursion_clamps_total() {
                        Ok(recursion_clamps_total) => SubsystemHealth {
                            status: if runtime.degraded_latency() {
                                "degraded_latency".to_owned()
                            } else {
                                "ok".to_owned()
                            },
                            detail: Some("reflex runtime initialized".to_owned()),
                            active_count: Some(runtime.active_count()),
                            sample_count: Some(runtime.sample_count()),
                            sample_limit: Some(runtime.sample_limit()),
                            last_tick_jitter_us: runtime.last_tick_jitter_us(),
                            p99_tick_jitter_us: runtime.p99_tick_jitter_us(),
                            late_tick_count: Some(runtime.late_tick_count()),
                            deadline_miss_streak: runtime.deadline_miss_streak(),
                            deadline_miss_audit_after: Some(runtime.deadline_miss_audit_after()),
                            severe_deadline_miss_after_us: Some(
                                runtime.severe_deadline_miss_after_us(),
                            ),
                            degraded_tick_count: Some(runtime.degraded_tick_count()),
                            recursion_clamps_total: Some(recursion_clamps_total),
                            ..SubsystemHealth::default()
                        },
                        Err(error) => SubsystemHealth {
                            status: "error".to_owned(),
                            detail: Some(error.to_string()),
                            active_count: Some(runtime.active_count()),
                            sample_count: Some(runtime.sample_count()),
                            sample_limit: Some(runtime.sample_limit()),
                            last_tick_jitter_us: runtime.last_tick_jitter_us(),
                            p99_tick_jitter_us: runtime.p99_tick_jitter_us(),
                            late_tick_count: Some(runtime.late_tick_count()),
                            deadline_miss_streak: runtime.deadline_miss_streak(),
                            deadline_miss_audit_after: Some(runtime.deadline_miss_audit_after()),
                            severe_deadline_miss_after_us: Some(
                                runtime.severe_deadline_miss_after_us(),
                            ),
                            degraded_tick_count: Some(runtime.degraded_tick_count()),
                            ..SubsystemHealth::default()
                        },
                    },
                    Err(error) => runtime_lock_unavailable_health("reflex", error),
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn profile_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                if let Some(error) = &state.profile_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                state.profile_runtime.as_ref().map_or_else(
                    || SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some(
                            "profile runtime initializes on first profile tool call".to_owned(),
                        ),
                        ..SubsystemHealth::default()
                    },
                    |runtime| {
                        let active_profile_id = runtime.active_profile_id();
                        let profiles = runtime.list(true);
                        let last_reload_at = runtime.last_reload_at();
                        match (active_profile_id, profiles, last_reload_at) {
                            (Ok(active_profile_id), Ok(profiles), Ok(last_reload_at)) => {
                                SubsystemHealth {
                                    status: "ok".to_owned(),
                                    detail: Some(format!(
                                        "profile_dir={}",
                                        runtime.profile_dir().display()
                                    )),
                                    active_profile_id,
                                    profile_count: Some(profiles.len()),
                                    last_reload_at,
                                    ..SubsystemHealth::default()
                                }
                            }
                            (active_profile_id, profiles, last_reload_at) => {
                                let detail = active_profile_id
                                    .err()
                                    .map(|error| error.to_string())
                                    .or_else(|| profiles.err().map(|error| error.to_string()))
                                    .or_else(|| last_reload_at.err().map(|error| error.to_string()))
                                    .unwrap_or_else(|| "profile runtime error".to_owned());
                                SubsystemHealth {
                                    status: "error".to_owned(),
                                    detail: Some(detail),
                                    ..SubsystemHealth::default()
                                }
                            }
                        }
                    },
                )
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn perception_health(&self) -> SubsystemHealth {
        match self.m1_state.try_lock() {
            Ok(state) => match crate::m1::detection_health_readback() {
                Ok(detection) => SubsystemHealth {
                    status: "ok".to_owned(),
                    detail: Some(format!("perception runtime initialized; {detection}")),
                    perception_mode: Some(state.perception_mode),
                    capture_config: Some(state.active_capture_config.clone()),
                    capture_runtime: Some(state.capture_runtime_readback()),
                    ..SubsystemHealth::default()
                },
                Err((code, detail)) => SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(format!("{code}: {detail}")),
                    perception_mode: Some(state.perception_mode),
                    capture_config: Some(state.active_capture_config.clone()),
                    capture_runtime: Some(state.capture_runtime_readback()),
                    ..SubsystemHealth::default()
                },
            },
            Err(error) => state_lock_unavailable_health("M1", error),
        }
    }

    fn action_health(&self) -> SubsystemHealth {
        match self.m2_state.try_lock() {
            Ok(state) => match state.backend_resolution_readback() {
                Ok((source, policy)) => {
                    let emitter_available = state.emitter_available();
                    let operator_hotkey = synapse_action::operator_hotkey_status().label();
                    let allow_shell = if self.m4_config.allow_shell_any() {
                        "any".to_owned()
                    } else {
                        self.m4_config.allow_shell_count().to_string()
                    };
                    let allow_launch = if self.m4_config.allow_launch_any() {
                        "any".to_owned()
                    } else {
                        self.m4_config.allow_launch_count().to_string()
                    };
                    let lease = synapse_action::lease::status();
                    let lease_detail = lease.owner_session_id.as_deref().map_or_else(
                        || "input_lease_held=false".to_owned(),
                        |owner| {
                            format!(
                                "input_lease_held=true input_lease_owner={owner} input_lease_expires_in_ms={}",
                                lease.expires_in_ms.unwrap_or(0)
                            )
                        },
                    );
                    let search_tools = self.m4_config.shell_search_tool_readback();
                    SubsystemHealth {
                        status: if emitter_available { "ok" } else { "error" }.to_owned(),
                        detail: Some(format!(
                            "emitter_available={} recording_enabled={} operator_hotkey={} allow_shell_patterns={} allow_launch_patterns={} {} {}",
                            emitter_available,
                            state.recording_enabled(),
                            operator_hotkey,
                            allow_shell,
                            allow_launch,
                            lease_detail,
                            search_tools
                        )),
                        backend_resolution: Some(backend_resolution_health(source, policy)),
                        run_shell_inline_await_limit_ms: Some(
                            self.m4_config.run_shell_inline_await_limit_ms(),
                        ),
                        run_shell_inline_client_call_budget_ms: Some(
                            self.m4_config.run_shell_inline_client_call_budget_ms(),
                        ),
                        run_shell_durable_default_timeout_ms: Some(
                            self.m4_config.run_shell_durable_default_timeout_ms(),
                        ),
                        run_shell_durable_max_timeout_ms: Some(
                            self.m4_config.run_shell_durable_max_timeout_ms(),
                        ),
                        ..SubsystemHealth::default()
                    }
                }
                Err(error) => SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(error),
                    ..SubsystemHealth::default()
                },
            },
            Err(error) => state_lock_unavailable_health("M2", error),
        }
    }

    fn audio_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                if let Some(error) = &state.audio_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                if !state.enable_audio {
                    return SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("audio is disabled; start with --enable-audio".to_owned()),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.audio_runtime else {
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some(
                            "audio runtime initializes on buffered audio or transcription requests"
                                .to_owned(),
                        ),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        ..SubsystemHealth::default()
                    };
                };
                let loopback_status = runtime.loopback_status();
                let status = if loopback_status.last_error_code.is_some() {
                    "error"
                } else {
                    "ok"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(loopback_status.last_error_code.map_or_else(
                        || {
                            if loopback_status.running {
                                "audio loopback running".to_owned()
                            } else {
                                "audio runtime initialized; loopback disabled".to_owned()
                            }
                        },
                        |code| format!("audio loopback error: {code}"),
                    )),
                    ring_buffer_seconds: Some(runtime.config().ring_seconds),
                    stt_model_loaded: Some(runtime.stt_model_loaded()),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn http_health(
        &self,
        active_sessions: Option<usize>,
        active_sessions_error: Option<String>,
    ) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                let session_detail = active_sessions_error
                    .as_deref()
                    .map_or_else(String::new, |error| {
                        format!(" active_session_readback_error={error}")
                    });
                if state.shutdown_reason == "http" {
                    let diagnostics = crate::http::http_transport_diagnostics_detail();
                    SubsystemHealth {
                        status: if active_sessions_error.is_some() {
                            "error".to_owned()
                        } else {
                            "ok".to_owned()
                        },
                        detail: Some(format!(
                            "HTTP transport initialized; {diagnostics}{session_detail}"
                        )),
                        bind_addr: Some(state.bind.clone()),
                        active_sessions,
                        sse_subscribers: Some(state.sse_state.active_subscription_count()),
                        ..SubsystemHealth::default()
                    }
                } else {
                    SubsystemHealth {
                        status: if active_sessions_error.is_some() {
                            "error".to_owned()
                        } else {
                            "disabled".to_owned()
                        },
                        detail: Some(format!(
                            "HTTP transport disabled in stdio mode{session_detail}"
                        )),
                        bind_addr: Some(state.bind.clone()),
                        active_sessions: Some(0),
                        sse_subscribers: Some(state.sse_state.active_subscription_count()),
                        ..SubsystemHealth::default()
                    }
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolSurfaceFingerprint {
    pub(crate) names: Vec<String>,
    pub(crate) sha256: String,
    pub(crate) error: Option<String>,
}

pub(crate) fn tool_surface_fingerprint_for_tools(mut tools: Vec<Tool>) -> ToolSurfaceFingerprint {
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let names = tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    let canonical = serde_json::json!({
        "mcp_surface": "tools/list",
        "tools": tools,
    });
    let bytes = match canonical_json_bytes(canonical) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(
                code = "MCP_TOOL_SURFACE_FINGERPRINT_SERIALIZE_FAILED",
                %error,
                "sanitized MCP tool surface failed to serialize for health fingerprinting"
            );
            return ToolSurfaceFingerprint {
                names,
                sha256: "TOOL_SURFACE_FINGERPRINT_ERROR".to_owned(),
                error: Some(format!(
                    "sanitized MCP tool surface failed to serialize for health fingerprinting: {error}"
                )),
            };
        }
    };
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    ToolSurfaceFingerprint {
        names,
        sha256: hex_lower(&hasher.finalize()),
        error: None,
    }
}

/// Apply the requested detail verbosity to the assembled subsystem map.
///
/// `Compact` (the default) drops every verbose per-subsystem `detail` string
/// but leaves the structured verdict fields (status, counts, schema versions,
/// timeouts, ...) untouched, so the health conclusion is unchanged — only the
/// human-readable prose is trimmed. `Full` preserves the `detail` strings.
///
/// The `chrome_bridge` subsystem's only structured information historically
/// lived inside its concatenated `detail` blob, so in both modes we parse that
/// blob into the typed `ChromeBridgeDetail`. `Full` keeps the original blob and
/// the fully-parsed struct; `Compact` keeps only the verdict-critical fields
/// and drops the blob.
fn apply_health_detail(subsystems: &mut BTreeMap<String, SubsystemHealth>, detail: HealthDetail) {
    for (name, subsystem) in subsystems.iter_mut() {
        if name == "chrome_bridge" {
            let parsed = subsystem.detail.as_deref().map(parse_chrome_bridge_detail);
            match detail {
                HealthDetail::Full => {
                    subsystem.chrome_bridge = parsed;
                }
                HealthDetail::Compact => {
                    subsystem.chrome_bridge = parsed.map(compact_chrome_bridge_detail);
                    subsystem.detail = None;
                }
            }
        } else if detail == HealthDetail::Compact {
            subsystem.detail = None;
        }
    }
}

/// Parse the `chrome_bridge` `detail` blob (`key=value` tokens joined by
/// spaces, with trailing free-text guidance) into the typed
/// `ChromeBridgeDetail`.
///
/// Only whitespace-free `key=value` tokens are consumed, so the trailing
/// human-readable guidance strings (which contain spaces) are ignored rather
/// than corrupting fields. Full-mode responses retain the original `detail`
/// string, so no information is lost even for fields this parser does not
/// surface.
fn parse_chrome_bridge_detail(detail: &str) -> ChromeBridgeDetail {
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for token in detail.split_whitespace() {
        if let Some((key, value)) = token.split_once('=') {
            if !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                // First occurrence wins; blob keys are unique.
                fields.entry(key).or_insert(value);
            }
        }
    }
    let bool_field = |key: &str| {
        fields.get(key).and_then(|value| match *value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    };
    let u64_field = |key: &str| {
        fields
            .get(key)
            .and_then(|value| (*value).parse::<u64>().ok())
    };
    let string_field = |key: &str| fields.get(key).map(|value| (*value).to_owned());
    let tab_control_available = bool_field("tab_control_available");
    let reason = if tab_control_available == Some(false) && !fields.contains_key("active_host_id") {
        string_field("reason")
    } else {
        None
    };
    ChromeBridgeDetail {
        tab_control_available,
        extension_stale: bool_field("extension_stale"),
        extension_stale_reasons: string_field("extension_stale_reasons"),
        reason,
        host_count: u64_field("host_count"),
        queued_count: u64_field("queued_count"),
        pending_count: u64_field("pending_count"),
        extension_id: string_field("extension_id"),
        expected_extension_id: string_field("expected_extension_id"),
        extension_version: string_field("extension_version"),
        transport: string_field("transport"),
        endpoint: string_field("endpoint"),
    }
}

/// Reduce a fully-parsed `ChromeBridgeDetail` to the verdict-critical fields
/// retained in compact health responses. The verbose identity/version/endpoint
/// fields are dropped (they remain available via `detail=full`).
fn compact_chrome_bridge_detail(detail: ChromeBridgeDetail) -> ChromeBridgeDetail {
    ChromeBridgeDetail {
        tab_control_available: detail.tab_control_available,
        extension_stale: detail.extension_stale,
        extension_stale_reasons: detail.extension_stale_reasons,
        reason: detail.reason,
        host_count: detail.host_count,
        queued_count: detail.queued_count,
        pending_count: detail.pending_count,
        extension_id: None,
        expected_extension_id: None,
        extension_version: None,
        transport: None,
        endpoint: None,
    }
}

fn canonical_json_bytes(value: Value) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&canonical_json_value(value))
}

fn canonical_json_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut ordered = Map::new();
            for (key, child) in entries {
                ordered.insert(key, canonical_json_value(child));
            }
            Value::Object(ordered)
        }
        scalar => scalar,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn backend_resolution_health(
    source: String,
    policy: BackendResolutionPolicy,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("source".to_owned(), source),
        (
            "default_backend".to_owned(),
            backend_config_name(policy.default_backend).to_owned(),
        ),
        (
            "keyboard_default".to_owned(),
            backend_config_name(policy.keyboard_default).to_owned(),
        ),
        (
            "mouse_default".to_owned(),
            backend_config_name(policy.mouse_default).to_owned(),
        ),
        (
            "pad_default".to_owned(),
            backend_config_name(policy.pad_default).to_owned(),
        ),
        (
            "keyboard_auto".to_owned(),
            policy.keyboard_auto_backend().as_str().to_owned(),
        ),
        (
            "mouse_auto".to_owned(),
            policy.mouse_auto_backend().as_str().to_owned(),
        ),
        (
            "pad_auto".to_owned(),
            policy.pad_auto_backend().as_str().to_owned(),
        ),
        (
            "release_all_auto".to_owned(),
            policy.release_all_auto_backend().as_str().to_owned(),
        ),
    ])
}

const fn backend_config_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Software => "software",
        Backend::Vigem => "vigem",
        Backend::Hardware => "hardware",
        Backend::Auto => "auto",
    }
}
