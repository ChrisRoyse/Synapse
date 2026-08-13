use std::{
    fmt,
    marker::PhantomData,
    ops::Deref,
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

use calyx_forge::{
    Backend, CUDA_COMPILED, CpuBackend, DeviceInfo, ForgeError, HostGpuReservation,
    HostGpuReservationSnapshot, VramStats,
};
// Host GPU admission is only exercised by the CUDA runtime candidate.
#[cfg(feature = "calyx-cuda")]
use calyx_forge::{
    CudaBackend, HostGpuReservationRequest, HostGpuReservationStore, VramBudgetedCudaBackend,
};
use serde::Serialize;

use crate::{SynapseCalyxError, SynapseCalyxMathBackend, SynapseCalyxTuningConfig};

const MATH_BACKEND_REMEDIATION: &str = "inspect the SYNAPSE_CALYX_MATH_* structured events, health payload, CUDA driver state, and Calyx Forge error; use math_backend=\"cpu\" only when intentionally forcing CPU";
#[cfg(feature = "calyx-cuda")]
const BYTES_PER_MIB: u64 = 1024 * 1024;
// CUDA context/module initialization is not routed through Forge's dispatch
// allocator, so it needs a conservative, declared host-wide envelope for its
// entire lifetime. On Windows WDDM, NVML reports device-global free memory but
// cannot report per-process used GPU memory. A before/after global delta can be
// changed by unrelated desktop GPU clients and therefore cannot prove a safe
// smaller claim. This envelope is intentionally independent from the maximum
// runtime dispatch budget: each dispatch still reserves its exact buffer shape.
#[cfg(feature = "calyx-cuda")]
const CUDA_STARTUP_ENVELOPE_MIB: u64 = 4 * 1024;
#[cfg(feature = "calyx-cuda")]
const CUDA_REPLACEMENT_RESERVATION_ENV: &str = "SYNAPSE_CALYX_GPU_REPLACEMENT_RESERVATION_ID";
/// Length of the CPU reduction-agreement probe: two full 8-lane chunks plus a
/// 3-element tail, so both the vector body and the scalar tail are covered.
const REDUCTION_PROBE_LEN: usize = 19;
const PROBE_TOLERANCE: f32 = 0.0001;
const PROBE_DIM: usize = 3;
const PROBE_QUERY: [f32; PROBE_DIM] = [1.0, 0.0, 0.0];
const PROBE_CANDIDATES: [f32; 9] = [
    1.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, //
    2.0, 0.0, 0.0,
];
const EXPECTED_DOT: [f32; 3] = [1.0, 0.0, 2.0];
const EXPECTED_COSINE: [f32; 3] = [1.0, 0.0, 1.0];
const EXPECTED_L2_SQUARED: [f32; 3] = [0.0, 2.0, 1.0];
const PROBE_TOPK_SCORES: [f32; 4] = [0.25, 1.5, -0.5, 1.5];
const EXPECTED_TOPK: [(usize, f32); 3] = [(1, 1.5), (3, 1.5), (0, 0.25)];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynapseCalyxMathBackendStatus {
    pub requested_backend: SynapseCalyxMathBackend,
    pub selected_backend: String,
    pub cuda_compiled: bool,
    pub device_name: String,
    pub device_vram_mib: Option<u64>,
    pub cpu_simd_path: String,
    pub vram_budget_bytes: u64,
    pub vram_dispatch: Option<SynapseCalyxVramDispatchStatus>,
    pub dispatch_telemetry: Option<SynapseCalyxMathDispatchTelemetry>,
    pub host_reservation_basis: Option<String>,
    pub host_reservation_id: Option<String>,
    pub host_reservation: Option<HostGpuReservationSnapshot>,
    pub runtime_readback_code: Option<String>,
    pub runtime_readback_error: Option<String>,
    pub fallback_code: Option<String>,
    pub fallback_source_code: Option<String>,
    pub fallback_error: Option<String>,
    pub probe: SynapseCalyxMathProbeReport,
}

impl SynapseCalyxMathBackendStatus {
    #[must_use]
    pub fn detail(&self) -> String {
        let reservation = self.host_reservation.as_ref().and_then(|snapshot| {
            let reservation_id = self.host_reservation_id.as_deref()?;
            snapshot
                .reservations
                .iter()
                .find(|row| row.reservation_id == reservation_id)
        });
        let dispatch_summary = self.dispatch_telemetry.as_ref().map_or_else(
            || "none".to_owned(),
            |telemetry| {
                telemetry
                    .operations
                    .iter()
                    .map(|operation| {
                        format!(
                            "{}:attempted={},in_flight={},succeeded={},refused={},failed={},bytes={},last_success={:?},last_error_code={}",
                            operation.operation,
                            operation.attempted_total,
                            operation.in_flight,
                            operation.succeeded_total,
                            operation.refused_total,
                            operation.failed_total,
                            operation.attempted_measured_bytes_total,
                            operation.last_success_unix_ms,
                            operation.last_error_code.as_deref().unwrap_or("none")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            },
        );
        format!(
            "requested_backend={} selected_backend={} cuda_compiled={} device_name={} device_vram_mib={:?} cpu_simd_path={} vram_budget_bytes={} vram_budget_enforced={} dispatch_soft_cap_bytes={:?} dispatch_allocated_bytes={:?} dispatch_device_free_bytes={:?} dispatch_epoch_started_unix_ms={:?} dispatch_sampled_at_unix_ms={:?} dispatch_ops={} host_reservation_basis={} host_reservation_state_path={} host_reservation_sha256={} host_reservation_id={} host_reservation_pid={:?} host_reservation_requested_mib={:?} runtime_readback_code={} runtime_readback_error={} fallback_code={} fallback_source_code={} probe_status={} probe_detail={}",
            self.requested_backend.as_str(),
            self.selected_backend,
            self.cuda_compiled,
            self.device_name,
            self.device_vram_mib,
            self.cpu_simd_path,
            self.vram_budget_bytes,
            self.vram_dispatch.is_some(),
            self.vram_dispatch
                .as_ref()
                .map(|status| status.soft_cap_bytes),
            self.vram_dispatch
                .as_ref()
                .map(|status| status.allocated_bytes),
            self.vram_dispatch
                .as_ref()
                .map(|status| status.device_free_bytes),
            self.dispatch_telemetry
                .as_ref()
                .map(|telemetry| telemetry.epoch_started_unix_ms),
            self.dispatch_telemetry
                .as_ref()
                .map(|telemetry| telemetry.sampled_at_unix_ms),
            dispatch_summary,
            self.host_reservation_basis.as_deref().unwrap_or("none"),
            self.host_reservation
                .as_ref()
                .map_or("none", |snapshot| snapshot.state_path.as_str()),
            self.host_reservation
                .as_ref()
                .map_or("none", |snapshot| snapshot.state_sha256.as_str()),
            reservation.map_or("none", |reservation| reservation.reservation_id.as_str()),
            reservation.map(|reservation| reservation.pid),
            reservation.map(|reservation| reservation.requested_mib),
            self.runtime_readback_code.as_deref().unwrap_or("none"),
            self.runtime_readback_error.as_deref().unwrap_or("none"),
            self.fallback_code.as_deref().unwrap_or("none"),
            self.fallback_source_code.as_deref().unwrap_or("none"),
            self.probe.status,
            self.probe.detail,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxVramDispatchStatus {
    pub soft_cap_bytes: u64,
    pub allocated_bytes: u64,
    pub serving_allocated_bytes: u64,
    pub anneal_allocated_bytes: u64,
    pub device_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxMathDispatchTelemetry {
    pub epoch_started_unix_ms: u64,
    pub sampled_at_unix_ms: u64,
    pub operations: Vec<SynapseCalyxMathDispatchOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxMathDispatchOperation {
    pub operation: String,
    pub attempted_total: u64,
    pub in_flight: u64,
    pub succeeded_total: u64,
    pub refused_total: u64,
    pub failed_total: u64,
    pub attempted_measured_bytes_total: u64,
    pub in_flight_measured_bytes: u64,
    pub succeeded_measured_bytes_total: u64,
    pub refused_measured_bytes_total: u64,
    pub failed_measured_bytes_total: u64,
    pub last_attempt_unix_ms: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    pub last_error_unix_ms: Option<u64>,
    pub last_error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynapseCalyxMathProbeReport {
    pub status: String,
    pub detail: String,
    pub tolerance: f32,
    pub dot: Vec<f32>,
    pub cosine: Vec<f32>,
    pub l2_squared: Vec<f32>,
    pub topk: Vec<SynapseCalyxMathProbeTopKEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynapseCalyxMathProbeTopKEntry {
    pub index: usize,
    pub score: f32,
}

struct InitializedSynapseCalyxMathRuntime {
    backend: Arc<dyn SynapseMathBackend>,
    status: SynapseCalyxMathBackendStatus,
    host_reservation: Option<HostGpuReservation>,
}

pub struct SynapseCalyxMathRuntime {
    shared: Arc<MathRuntimeShared>,
}

struct MathRuntimeShared {
    config: SynapseCalyxTuningConfig,
    cpu_readback: CpuReadback,
    release_when_idle: bool,
    lifecycle: Mutex<MathRuntimeLifecycle>,
    lifecycle_changed: Condvar,
}

enum MathRuntimeLifecycle {
    Dormant(SynapseCalyxMathBackendStatus),
    Initializing(SynapseCalyxMathBackendStatus),
    Ready {
        runtime: Box<InitializedSynapseCalyxMathRuntime>,
        active_leases: usize,
    },
    Releasing(SynapseCalyxMathBackendStatus),
    Failed {
        status: SynapseCalyxMathBackendStatus,
        error: SynapseCalyxError,
    },
    Closed(SynapseCalyxMathBackendStatus),
}

pub struct SynapseCalyxMathLease<'runtime> {
    shared: Arc<MathRuntimeShared>,
    backend: Option<Arc<dyn SynapseMathBackend>>,
    _runtime: PhantomData<&'runtime SynapseCalyxMathRuntime>,
}

impl fmt::Debug for SynapseCalyxMathRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SynapseCalyxMathRuntime")
            .field("status", &self.status_snapshot())
            .finish_non_exhaustive()
    }
}

impl SynapseCalyxMathRuntime {
    /// Leases the selected backend, activating a dormant CUDA runtime on the
    /// first concurrent math request and releasing it after the final caller.
    /// A failed selected GPU or failed release is retained as a hard error and
    /// is never replaced by CPU.
    ///
    /// # Errors
    ///
    /// Returns the retained structured CUDA initialization/probe/release
    /// failure, or a lifecycle synchronization failure.
    pub(crate) fn backend(&self) -> Result<SynapseCalyxMathLease<'_>, SynapseCalyxError> {
        loop {
            let mut lifecycle = self.shared.lock_lifecycle()?;
            match &mut *lifecycle {
                MathRuntimeLifecycle::Dormant(status) => {
                    let dormant_status = status.clone();
                    tracing::info!(
                        code = "SYNAPSE_CALYX_MATH_LAZY_INIT_STARTED",
                        requested_backend = self.shared.config.math_backend.as_str(),
                        selected_backend = dormant_status.selected_backend.as_str(),
                        device_name = dormant_status.device_name.as_str(),
                        "activating the selected CUDA runtime for a real math request"
                    );
                    *lifecycle = MathRuntimeLifecycle::Initializing(dormant_status.clone());
                    drop(lifecycle);

                    let result = initialize_cuda_runtime(
                        &self.shared.config,
                        self.shared.cpu_readback.clone(),
                    );
                    let mut lifecycle = self.shared.lock_lifecycle()?;
                    match result {
                        Ok(runtime) => {
                            tracing::info!(
                                code = "SYNAPSE_CALYX_MATH_LAZY_INIT_SUCCEEDED",
                                selected_backend = runtime.status.selected_backend.as_str(),
                                device_name = runtime.status.device_name.as_str(),
                                probe_status = runtime.status.probe.status.as_str(),
                                "activated and proved the selected CUDA runtime"
                            );
                            *lifecycle = MathRuntimeLifecycle::Ready {
                                runtime: Box::new(runtime),
                                active_leases: 0,
                            };
                        }
                        Err(error) => {
                            tracing::error!(
                                code = "SYNAPSE_CALYX_MATH_LAZY_INIT_FAILED",
                                error_code = error.code,
                                source_code = error.source_code.unwrap_or("none"),
                                error = %error,
                                remediation = error.remediation,
                                "the selected CUDA runtime failed activation; retaining the failure and refusing math requests"
                            );
                            *lifecycle = MathRuntimeLifecycle::Failed {
                                status: failed_math_status(dormant_status, &error),
                                error,
                            };
                        }
                    }
                    drop(lifecycle);
                    self.shared.lifecycle_changed.notify_all();
                }
                MathRuntimeLifecycle::Initializing(_) | MathRuntimeLifecycle::Releasing(_) => {
                    drop(self.shared.wait_for_lifecycle_change(lifecycle)?);
                }
                MathRuntimeLifecycle::Ready {
                    runtime,
                    active_leases,
                } => {
                    *active_leases = active_leases.checked_add(1).ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_MATH_LEASE_OVERFLOW",
                            "the concurrent math lease count overflowed usize",
                            MATH_BACKEND_REMEDIATION,
                        )
                    })?;
                    return Ok(SynapseCalyxMathLease {
                        shared: Arc::clone(&self.shared),
                        backend: Some(Arc::clone(&runtime.backend)),
                        _runtime: PhantomData,
                    });
                }
                MathRuntimeLifecycle::Failed { error, .. } => return Err(error.clone()),
                MathRuntimeLifecycle::Closed(_) => {
                    return Err(SynapseCalyxError::new(
                        "SYNAPSE_CALYX_MATH_RUNTIME_CLOSED",
                        "the Calyx math runtime is already closed",
                        MATH_BACKEND_REMEDIATION,
                    ));
                }
            }
        }
    }

    #[must_use]
    pub fn status_snapshot(&self) -> SynapseCalyxMathBackendStatus {
        let lifecycle = match self.shared.lifecycle.lock() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_MATH_LIFECYCLE_POISONED",
                    remediation = MATH_BACKEND_REMEDIATION,
                    "math lifecycle state was poisoned while producing health readback"
                );
                let mut status = poisoned.into_inner().status_snapshot();
                status.runtime_readback_code =
                    Some("SYNAPSE_CALYX_MATH_LIFECYCLE_POISONED".to_owned());
                status.runtime_readback_error = Some(
                    "math lifecycle state was poisoned; requests fail closed and daemon replacement is required"
                        .to_owned(),
                );
                return status;
            }
        };
        lifecycle.status_snapshot()
    }

    /// Closes a CPU runtime or a CUDA runtime that has not already returned to
    /// dormancy. The lifetime on every lease prevents close while a caller is
    /// using the backend.
    ///
    /// # Errors
    ///
    /// Returns a structured Forge-derived error if a live runtime's persisted
    /// reservation cannot be removed, reread, unlocked, or deleted.
    pub fn close(self) -> Result<Option<HostGpuReservationSnapshot>, SynapseCalyxError> {
        loop {
            let mut lifecycle = self.shared.lock_lifecycle()?;
            match &*lifecycle {
                MathRuntimeLifecycle::Initializing(_) | MathRuntimeLifecycle::Releasing(_) => {
                    drop(self.shared.wait_for_lifecycle_change(lifecycle)?);
                }
                MathRuntimeLifecycle::Ready { active_leases, .. } if *active_leases != 0 => {
                    let error = SynapseCalyxError::new(
                        "SYNAPSE_CALYX_MATH_CLOSE_WITH_ACTIVE_LEASES",
                        format!(
                            "refused to close the Calyx math runtime with {active_leases} active lease(s)"
                        ),
                        MATH_BACKEND_REMEDIATION,
                    );
                    drop(lifecycle);
                    return Err(error);
                }
                MathRuntimeLifecycle::Ready { .. } => {
                    let prior = std::mem::replace(
                        &mut *lifecycle,
                        MathRuntimeLifecycle::Closed(closed_placeholder_status()),
                    );
                    let MathRuntimeLifecycle::Ready { runtime, .. } = prior else {
                        unreachable!("ready lifecycle was matched before replacement");
                    };
                    drop(lifecycle);
                    return (*runtime).close();
                }
                MathRuntimeLifecycle::Dormant(status) => {
                    let status = status.clone();
                    tracing::info!(
                        code = "SYNAPSE_CALYX_MATH_DORMANT_RUNTIME_CLOSED",
                        selected_backend = status.selected_backend.as_str(),
                        device_name = status.device_name.as_str(),
                        "closed the dormant CUDA selection without a live context or host reservation"
                    );
                    *lifecycle = MathRuntimeLifecycle::Closed(status);
                    drop(lifecycle);
                    return Ok(None);
                }
                MathRuntimeLifecycle::Failed { status, error } => {
                    let status = status.clone();
                    tracing::warn!(
                        code = "SYNAPSE_CALYX_MATH_FAILED_RUNTIME_CLOSED",
                        error_code = error.code,
                        source_code = error.source_code.unwrap_or("none"),
                        error = %error,
                        "closed a vault whose CUDA activation or release had failed"
                    );
                    *lifecycle = MathRuntimeLifecycle::Closed(status);
                    drop(lifecycle);
                    return Ok(None);
                }
                MathRuntimeLifecycle::Closed(_) => {
                    drop(lifecycle);
                    return Ok(None);
                }
            }
        }
    }

    fn from_initialized(
        config: &SynapseCalyxTuningConfig,
        cpu_readback: CpuReadback,
        runtime: InitializedSynapseCalyxMathRuntime,
    ) -> Self {
        Self {
            shared: Arc::new(MathRuntimeShared {
                config: config.clone(),
                cpu_readback,
                release_when_idle: false,
                lifecycle: Mutex::new(MathRuntimeLifecycle::Ready {
                    runtime: Box::new(runtime),
                    active_leases: 0,
                }),
                lifecycle_changed: Condvar::new(),
            }),
        }
    }

    fn deferred_cuda(
        config: &SynapseCalyxTuningConfig,
        cpu_readback: CpuReadback,
        status: SynapseCalyxMathBackendStatus,
    ) -> Self {
        Self {
            shared: Arc::new(MathRuntimeShared {
                config: config.clone(),
                cpu_readback,
                release_when_idle: true,
                lifecycle: Mutex::new(MathRuntimeLifecycle::Dormant(status)),
                lifecycle_changed: Condvar::new(),
            }),
        }
    }
}

impl MathRuntimeShared {
    fn lock_lifecycle(&self) -> Result<MutexGuard<'_, MathRuntimeLifecycle>, SynapseCalyxError> {
        self.lifecycle.lock().map_err(|_| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_MATH_LIFECYCLE_POISONED",
                "math lifecycle state was poisoned; refusing to use an unprovable backend state",
                MATH_BACKEND_REMEDIATION,
            )
        })
    }

    fn wait_for_lifecycle_change<'a>(
        &self,
        lifecycle: MutexGuard<'a, MathRuntimeLifecycle>,
    ) -> Result<MutexGuard<'a, MathRuntimeLifecycle>, SynapseCalyxError> {
        self.lifecycle_changed.wait(lifecycle).map_err(|_| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_MATH_LIFECYCLE_POISONED",
                "math lifecycle state was poisoned while waiting for activation or release",
                MATH_BACKEND_REMEDIATION,
            )
        })
    }
}

impl MathRuntimeLifecycle {
    fn status_snapshot(&self) -> SynapseCalyxMathBackendStatus {
        match self {
            Self::Dormant(status) | Self::Failed { status, .. } | Self::Closed(status) => {
                status.clone()
            }
            Self::Initializing(status) => transitional_math_status(
                status.clone(),
                "initializing",
                "a real math caller is activating and proving the selected CUDA runtime",
            ),
            Self::Ready { runtime, .. } => runtime.status_snapshot(),
            Self::Releasing(status) => transitional_math_status(
                status.clone(),
                "releasing",
                "the final math lease is destroying the CUDA context and rereading the host reservation Source of Truth",
            ),
        }
    }
}

impl Deref for SynapseCalyxMathLease<'_> {
    type Target = dyn Backend;

    fn deref(&self) -> &Self::Target {
        self.backend.as_deref().unwrap_or_else(|| {
            tracing::error!(
                code = "SYNAPSE_CALYX_MATH_LEASE_BACKEND_MISSING",
                remediation = MATH_BACKEND_REMEDIATION,
                "a live math lease lost its backend ownership invariant"
            );
            std::process::abort();
        })
    }
}

impl SynapseCalyxMathLease<'_> {
    fn release(&mut self) {
        let mut lifecycle = match self.shared.lifecycle.lock() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_MATH_LIFECYCLE_POISONED",
                    remediation = MATH_BACKEND_REMEDIATION,
                    "math lifecycle state was poisoned while dropping a lease; attempting exact resource cleanup before retaining failure"
                );
                poisoned.into_inner()
            }
        };
        let poisoned = self.shared.lifecycle.is_poisoned();
        let mut lifecycle_failure = poisoned.then(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_MATH_LIFECYCLE_POISONED",
                "CUDA resources were released, but poisoned lifecycle state prevents proving safe reactivation",
                MATH_BACKEND_REMEDIATION,
            )
        });
        let should_release = match &mut *lifecycle {
            MathRuntimeLifecycle::Ready { active_leases, .. } if *active_leases > 0 => {
                *active_leases -= 1;
                *active_leases == 0 && self.shared.release_when_idle
            }
            MathRuntimeLifecycle::Ready { .. } => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_MATH_LEASE_UNDERFLOW",
                    remediation = MATH_BACKEND_REMEDIATION,
                    "math lease dropped while the lifecycle recorded zero active leases"
                );
                lifecycle_failure = Some(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_MATH_LEASE_UNDERFLOW",
                    "a math lease dropped while the lifecycle recorded zero active leases; resources were cleaned and reactivation is refused",
                    MATH_BACKEND_REMEDIATION,
                ));
                self.shared.release_when_idle
            }
            _ => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_MATH_LEASE_STATE_INVALID",
                    lifecycle = lifecycle.label(),
                    remediation = MATH_BACKEND_REMEDIATION,
                    "math lease dropped outside the ready lifecycle"
                );
                false
            }
        };
        if !should_release {
            drop(lifecycle);
            drop(self.backend.take());
            return;
        }

        let prior = std::mem::replace(
            &mut *lifecycle,
            MathRuntimeLifecycle::Closed(closed_placeholder_status()),
        );
        let MathRuntimeLifecycle::Ready { runtime, .. } = prior else {
            tracing::error!(
                code = "SYNAPSE_CALYX_MATH_LEASE_STATE_INVALID",
                remediation = MATH_BACKEND_REMEDIATION,
                "the ready CUDA runtime disappeared before final-lease release"
            );
            drop(lifecycle);
            drop(self.backend.take());
            return;
        };
        let live_status = runtime.status_snapshot();
        *lifecycle = MathRuntimeLifecycle::Releasing(live_status.clone());
        tracing::info!(
            code = "SYNAPSE_CALYX_MATH_IDLE_RELEASE_STARTED",
            selected_backend = live_status.selected_backend.as_str(),
            device_name = live_status.device_name.as_str(),
            host_reservation_id = live_status.host_reservation_id.as_deref().unwrap_or("none"),
            "the final math lease ended; destroying the CUDA runtime before releasing its host reservation"
        );
        drop(lifecycle);

        drop(self.backend.take());
        self.finish_idle_release(runtime, live_status, lifecycle_failure);
    }

    fn finish_idle_release(
        &self,
        runtime: Box<InitializedSynapseCalyxMathRuntime>,
        live_status: SynapseCalyxMathBackendStatus,
        lifecycle_failure: Option<SynapseCalyxError>,
    ) {
        let release = (*runtime).close();
        let mut lifecycle = match self.shared.lifecycle.lock() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => poisoned.into_inner(),
        };
        match (release, lifecycle_failure) {
            (Ok(snapshot), None) => {
                let dormant = dormant_status_after_release(live_status);
                if let Some(snapshot) = snapshot.as_ref() {
                    tracing::info!(
                        code = "SYNAPSE_CALYX_MATH_IDLE_RELEASE_SUCCEEDED",
                        state_path = snapshot.state_path.as_str(),
                        state_sha256 = snapshot.state_sha256.as_str(),
                        reserved_mib = snapshot.reserved_mib,
                        reservation_count = snapshot.reservations.len(),
                        "destroyed the idle CUDA context and separately read back the host reservation Source of Truth"
                    );
                } else {
                    tracing::info!(
                        code = "SYNAPSE_CALYX_MATH_IDLE_RELEASE_SUCCEEDED",
                        "released an idle math runtime that owned no host GPU reservation"
                    );
                }
                *lifecycle = MathRuntimeLifecycle::Dormant(dormant);
            }
            (Ok(_), Some(error)) => {
                tracing::error!(
                    code = error.code,
                    error = %error,
                    remediation = error.remediation,
                    "released idle CUDA resources but retained a fail-closed lifecycle error"
                );
                *lifecycle = MathRuntimeLifecycle::Failed {
                    status: failed_math_status(live_status, &error),
                    error,
                };
            }
            (Err(error), _) => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_MATH_IDLE_RELEASE_FAILED",
                    error_code = error.code,
                    source_code = error.source_code.unwrap_or("none"),
                    error = %error,
                    remediation = error.remediation,
                    "failed to prove idle CUDA runtime release; retaining a hard math error"
                );
                *lifecycle = MathRuntimeLifecycle::Failed {
                    status: failed_math_status(live_status, &error),
                    error,
                };
            }
        }
        drop(lifecycle);
        self.shared.lifecycle_changed.notify_all();
    }
}

impl Drop for SynapseCalyxMathLease<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

impl MathRuntimeLifecycle {
    const fn label(&self) -> &'static str {
        match self {
            Self::Dormant(_) => "dormant",
            Self::Initializing(_) => "initializing",
            Self::Ready { .. } => "ready",
            Self::Releasing(_) => "releasing",
            Self::Failed { .. } => "failed",
            Self::Closed(_) => "closed",
        }
    }
}

fn transitional_math_status(
    mut status: SynapseCalyxMathBackendStatus,
    state: &str,
    detail: &str,
) -> SynapseCalyxMathBackendStatus {
    state.clone_into(&mut status.probe.status);
    detail.clone_into(&mut status.probe.detail);
    status
}

fn failed_math_status(
    mut status: SynapseCalyxMathBackendStatus,
    error: &SynapseCalyxError,
) -> SynapseCalyxMathBackendStatus {
    status.runtime_readback_code = Some(error.code.to_owned());
    status.runtime_readback_error = Some(error.to_string());
    "error".clone_into(&mut status.probe.status);
    status.probe.detail = format!("math runtime failed with {}: {}", error.code, error.message);
    status
}

fn dormant_status_after_release(
    mut status: SynapseCalyxMathBackendStatus,
) -> SynapseCalyxMathBackendStatus {
    status.vram_dispatch = None;
    status.host_reservation_basis = None;
    status.host_reservation_id = None;
    status.host_reservation = None;
    status.runtime_readback_code = None;
    status.runtime_readback_error = None;
    "dormant_verified".clone_into(&mut status.probe.status);
    "the selected CUDA backend passed its real operation probe; the final caller then destroyed the context and reread the host reservation Source of Truth, so no CUDA runtime or reservation is live while idle".clone_into(&mut status.probe.detail);
    status
}

fn closed_placeholder_status() -> SynapseCalyxMathBackendStatus {
    SynapseCalyxMathBackendStatus {
        requested_backend: SynapseCalyxMathBackend::Cpu,
        selected_backend: "closed".to_owned(),
        cuda_compiled: CUDA_COMPILED,
        device_name: "closed".to_owned(),
        device_vram_mib: None,
        cpu_simd_path: "closed".to_owned(),
        vram_budget_bytes: 0,
        vram_dispatch: None,
        dispatch_telemetry: None,
        host_reservation_basis: None,
        host_reservation_id: None,
        host_reservation: None,
        runtime_readback_code: None,
        runtime_readback_error: None,
        fallback_code: None,
        fallback_source_code: None,
        fallback_error: None,
        probe: SynapseCalyxMathProbeReport {
            status: "closed".to_owned(),
            detail: "the Calyx math runtime is closed and exposes no backend".to_owned(),
            tolerance: PROBE_TOLERANCE,
            dot: Vec::new(),
            cosine: Vec::new(),
            l2_squared: Vec::new(),
            topk: Vec::new(),
        },
    }
}

impl InitializedSynapseCalyxMathRuntime {
    fn status_snapshot(&self) -> SynapseCalyxMathBackendStatus {
        let mut status = self.status.clone();
        match self.backend.strict_vram_status() {
            Ok(vram_dispatch) => status.vram_dispatch = vram_dispatch,
            Err(error) => {
                status.runtime_readback_code = Some(error.code().to_owned());
                status.runtime_readback_error =
                    Some(format!("process-local CUDA VRAM readback failed: {error}"));
            }
        }
        match self.backend.strict_dispatch_telemetry() {
            Ok(dispatch_telemetry) => status.dispatch_telemetry = dispatch_telemetry,
            Err(error) => {
                status.runtime_readback_code = Some(error.code().to_owned());
                status.runtime_readback_error = Some(format!(
                    "process-local CUDA dispatch telemetry readback failed: {error}"
                ));
            }
        }
        if let Some(reservation) = self.host_reservation.as_ref() {
            match reservation.readback() {
                Ok(snapshot) => {
                    let reservation_id = reservation.reservation_id();
                    let own_row = snapshot
                        .reservations
                        .iter()
                        .find(|row| row.reservation_id == reservation_id);
                    if own_row.is_some_and(|row| row.pid == std::process::id()) {
                        status.host_reservation = Some(snapshot);
                    } else {
                        status.runtime_readback_code =
                            Some("SYNAPSE_CALYX_MATH_HOST_RESERVATION_MISSING".to_owned());
                        status.runtime_readback_error = Some(format!(
                            "live host ledger does not contain reservation_id={reservation_id} pid={}",
                            std::process::id()
                        ));
                    }
                }
                Err(error) => {
                    status.runtime_readback_code = Some(error.code().to_owned());
                    status.runtime_readback_error =
                        Some(format!("live host reservation readback failed: {error}"));
                }
            }
        }
        status
    }

    fn close(self) -> Result<Option<HostGpuReservationSnapshot>, SynapseCalyxError> {
        let Self {
            backend,
            status: _,
            host_reservation,
        } = self;
        drop(backend);
        host_reservation
            .map(HostGpuReservation::release)
            .transpose()
            .map_err(|error| {
                forge_error(
                    "SYNAPSE_CALYX_MATH_HOST_RESERVATION_RELEASE_FAILED",
                    "release and read back Calyx host GPU reservation",
                    &error,
                )
            })
    }
}

/// Selects the single Calyx Forge math backend for this Synapse process.
///
/// # Errors
///
/// Returns a structured error when config is invalid or physical device
/// discovery cannot prove the requested selection. CPU is initialized and
/// proved immediately. CUDA remains dormant until the first real math request,
/// when it initializes exactly once, reserves physical GPU capacity, and runs
/// its probe. `auto` selects CPU only when NVML proves there is no CUDA device.
/// A present-but-broken GPU, admission failure, or CUDA execution failure is
/// retained as a hard error and never causes a runtime fallback.
pub fn math_backend(
    config: &SynapseCalyxTuningConfig,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
    config.clone().validate()?;
    let cpu_reference = CpuBackend::new();
    let cpu_readback = CpuReadback::from_backend(&cpu_reference);
    match config.math_backend {
        SynapseCalyxMathBackend::Cpu => runtime_from_backend(
            config,
            cpu_reference,
            cpu_readback.clone(),
            None,
            None,
            None,
        )
        .map(|runtime| SynapseCalyxMathRuntime::from_initialized(config, cpu_readback, runtime)),
        SynapseCalyxMathBackend::Cuda => deferred_cuda_runtime_candidate(config, cpu_readback),
        SynapseCalyxMathBackend::Auto => {
            match deferred_cuda_runtime_candidate(config, cpu_readback.clone()) {
                Ok(runtime) => Ok(runtime),
                Err(error) if error_proves_cuda_absent(&error) => {
                    let mut initialized = runtime_from_backend(
                        config,
                        cpu_reference,
                        cpu_readback.clone(),
                        None,
                        None,
                        None,
                    )?;
                    initialized.status.fallback_code =
                        Some("SYNAPSE_CALYX_MATH_AUTO_CPU_NO_CUDA_DEVICE".to_owned());
                    initialized.status.fallback_source_code = Some(error.code.to_owned());
                    initialized.status.fallback_error = Some(error.to_string());
                    tracing::warn!(
                        code = "SYNAPSE_CALYX_MATH_AUTO_CPU_NO_CUDA_DEVICE",
                        source_code = error.code,
                        source_error = %error,
                        cpu_simd_path = initialized.status.cpu_simd_path,
                        "auto selected the explicit CPU runtime because no supported CUDA device exists"
                    );
                    Ok(SynapseCalyxMathRuntime::from_initialized(
                        config,
                        cpu_readback,
                        initialized,
                    ))
                }
                Err(error) => Err(error),
            }
        }
    }
}

fn deferred_cuda_runtime_candidate(
    config: &SynapseCalyxTuningConfig,
    cpu_readback: CpuReadback,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
    let host = crate::host_cuda::host_cuda_device_probe();
    if host.device_absent_proven {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT",
            format!("NVML proved this host has no CUDA device: {}", host.basis),
            MATH_BACKEND_REMEDIATION,
        ));
    }
    if !host.device_present {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_CUDA_DEVICE_INDETERMINATE",
            format!(
                "NVML could not prove whether this host has CUDA device 0: {}",
                host.basis
            ),
            MATH_BACKEND_REMEDIATION,
        ));
    }
    if !CUDA_COMPILED {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_CUDA_NOT_COMPILED",
            format!(
                "{} but this synapse-calyx build has no CUDA kernels compiled in; rebuild with --features calyx-cuda (a CUDA 13.3 toolkit must be installed so calyx-forge can run nvcc), or select math_backend=\"cpu\" explicitly to accept CPU math on a GPU host",
                host.basis
            ),
            MATH_BACKEND_REMEDIATION,
        ));
    }

    let status = SynapseCalyxMathBackendStatus {
        requested_backend: config.math_backend,
        selected_backend: "cuda".to_owned(),
        cuda_compiled: CUDA_COMPILED,
        device_name: host
            .device_name
            .clone()
            .unwrap_or_else(|| "CUDA device 0".to_owned()),
        device_vram_mib: host.device_vram_mib,
        cpu_simd_path: cpu_readback.simd_path.clone(),
        vram_budget_bytes: config.vram_budget_bytes,
        vram_dispatch: None,
        dispatch_telemetry: None,
        host_reservation_basis: None,
        host_reservation_id: None,
        host_reservation: None,
        runtime_readback_code: None,
        runtime_readback_error: None,
        fallback_code: None,
        fallback_source_code: None,
        fallback_error: None,
        probe: SynapseCalyxMathProbeReport {
            status: "dormant".to_owned(),
            detail: format!(
                "{}; CUDA context, modules, host reservation, and fixed-vector proof are deferred until the first real math request",
                host.basis
            ),
            tolerance: PROBE_TOLERANCE,
            dot: Vec::new(),
            cosine: Vec::new(),
            l2_squared: Vec::new(),
            topk: Vec::new(),
        },
    };
    tracing::info!(
        code = "SYNAPSE_CALYX_MATH_BACKEND_SELECTED_DORMANT",
        requested_backend = status.requested_backend.as_str(),
        selected_backend = status.selected_backend.as_str(),
        cuda_compiled = status.cuda_compiled,
        device_name = status.device_name.as_str(),
        device_vram_mib = status.device_vram_mib,
        vram_budget_bytes = status.vram_budget_bytes,
        probe_status = status.probe.status.as_str(),
        device_probe_basis = host.basis.as_str(),
        "selected CUDA from physical device evidence without creating a context or reserving memory"
    );
    Ok(SynapseCalyxMathRuntime::deferred_cuda(
        config,
        cpu_readback,
        status,
    ))
}

/// Whether an error is positive proof that this host has no CUDA device — the
/// only justification for `math_backend="auto"` resolving to CPU.
///
/// `SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT` is matched by code because the
/// non-CUDA build raises it only after an NVML probe classified the host as
/// device-absent. Probe malfunctions raise
/// `SYNAPSE_CALYX_MATH_CUDA_DEVICE_INDETERMINATE` instead and deliberately do
/// NOT match here: uncertainty is not evidence.
fn error_proves_cuda_absent(error: &SynapseCalyxError) -> bool {
    error.code == "SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT"
}

#[derive(Clone, Debug)]
struct CpuReadback {
    simd_path: String,
}

impl CpuReadback {
    fn from_backend(backend: &CpuBackend) -> Self {
        Self {
            simd_path: backend.simd_path().to_owned(),
        }
    }
}

trait SynapseMathBackend: Backend {
    fn strict_vram_status(&self) -> Result<Option<SynapseCalyxVramDispatchStatus>, ForgeError>;
    fn strict_dispatch_telemetry(
        &self,
    ) -> Result<Option<SynapseCalyxMathDispatchTelemetry>, ForgeError>;
    fn reset_serving_dispatch_telemetry(&self) -> Result<(), ForgeError>;
}

impl SynapseMathBackend for CpuBackend {
    fn strict_vram_status(&self) -> Result<Option<SynapseCalyxVramDispatchStatus>, ForgeError> {
        Ok(None)
    }

    fn strict_dispatch_telemetry(
        &self,
    ) -> Result<Option<SynapseCalyxMathDispatchTelemetry>, ForgeError> {
        Ok(None)
    }

    fn reset_serving_dispatch_telemetry(&self) -> Result<(), ForgeError> {
        Ok(())
    }
}

#[cfg(feature = "calyx-cuda")]
impl SynapseMathBackend for VramBudgetedCudaBackend {
    fn strict_vram_status(&self) -> Result<Option<SynapseCalyxVramDispatchStatus>, ForgeError> {
        self.stats_strict()
            .map(SynapseCalyxVramDispatchStatus::from)
            .map(Some)
    }

    fn strict_dispatch_telemetry(
        &self,
    ) -> Result<Option<SynapseCalyxMathDispatchTelemetry>, ForgeError> {
        self.dispatch_telemetry()
            .map(SynapseCalyxMathDispatchTelemetry::from)
            .map(Some)
    }

    fn reset_serving_dispatch_telemetry(&self) -> Result<(), ForgeError> {
        self.reset_dispatch_telemetry()
    }
}

#[cfg(feature = "calyx-cuda")]
impl From<calyx_forge::CudaDispatchTelemetrySnapshot> for SynapseCalyxMathDispatchTelemetry {
    fn from(snapshot: calyx_forge::CudaDispatchTelemetrySnapshot) -> Self {
        Self {
            epoch_started_unix_ms: snapshot.epoch_started_unix_ms,
            sampled_at_unix_ms: snapshot.sampled_at_unix_ms,
            operations: snapshot
                .operations
                .into_iter()
                .map(|operation| SynapseCalyxMathDispatchOperation {
                    operation: operation.operation,
                    attempted_total: operation.attempted_total,
                    in_flight: operation.in_flight,
                    succeeded_total: operation.succeeded_total,
                    refused_total: operation.refused_total,
                    failed_total: operation.failed_total,
                    attempted_measured_bytes_total: operation.attempted_measured_bytes_total,
                    in_flight_measured_bytes: operation.in_flight_measured_bytes,
                    succeeded_measured_bytes_total: operation.succeeded_measured_bytes_total,
                    refused_measured_bytes_total: operation.refused_measured_bytes_total,
                    failed_measured_bytes_total: operation.failed_measured_bytes_total,
                    last_attempt_unix_ms: operation.last_attempt_unix_ms,
                    last_success_unix_ms: operation.last_success_unix_ms,
                    last_error_unix_ms: operation.last_error_unix_ms,
                    last_error_code: operation.last_error_code,
                })
                .collect(),
        }
    }
}

impl From<VramStats> for SynapseCalyxVramDispatchStatus {
    fn from(stats: VramStats) -> Self {
        Self {
            soft_cap_bytes: stats.soft_cap_bytes as u64,
            allocated_bytes: stats.allocated_bytes as u64,
            serving_allocated_bytes: stats.serving_allocated_bytes as u64,
            anneal_allocated_bytes: stats.anneal_allocated_bytes as u64,
            device_free_bytes: stats.device_free_bytes as u64,
        }
    }
}

#[cfg(feature = "calyx-cuda")]
fn initialize_cuda_runtime(
    config: &SynapseCalyxTuningConfig,
    cpu_readback: CpuReadback,
) -> Result<InitializedSynapseCalyxMathRuntime, SynapseCalyxError> {
    let reservation_store = HostGpuReservationStore::from_env(0).map_err(|error| {
        forge_error(
            "SYNAPSE_CALYX_MATH_HOST_RESERVATION_OPEN_FAILED",
            "open the device-0 host GPU reservation Source of Truth",
            &error,
        )
    })?;
    let runtime_ceiling_mib = config.vram_budget_bytes.div_ceil(BYTES_PER_MIB);
    let startup_envelope_mib = runtime_ceiling_mib.min(CUDA_STARTUP_ENVELOPE_MIB);
    let mut request = HostGpuReservationRequest::new(
        "synapse-mcp",
        format!("synapse-daemon-pid-{}", std::process::id()),
        format!(
            "synapse-mcp CUDA context/module lifetime envelope; envelope_mib={startup_envelope_mib}; runtime_dispatch_ceiling_mib={runtime_ceiling_mib}; retained conservatively because WDDM has no exact per-process VRAM readback"
        ),
        startup_envelope_mib,
    );
    if let Some(replacement_id) = cuda_replacement_reservation_id()? {
        request = request.replacing(replacement_id);
    }
    let reservation = reservation_store.acquire(request).map_err(|error| {
        forge_error(
            "SYNAPSE_CALYX_MATH_HOST_RESERVATION_REFUSED",
            "atomically admit the embedded Synapse CUDA startup envelope",
            &error,
        )
    })?;
    let backend = match CudaBackend::new() {
        Ok(backend) => backend,
        Err(error) => {
            return Err(cleanup_host_reservation_after_startup_failure(
                reservation,
                forge_error(
                    "SYNAPSE_CALYX_MATH_CUDA_UNAVAILABLE",
                    "initialize Calyx CUDA backend",
                    &error,
                ),
            ));
        }
    };
    let backend = match VramBudgetedCudaBackend::new(backend, config.vram_budget_bytes) {
        Ok(backend) => backend,
        Err(error) => {
            return Err(cleanup_host_reservation_after_startup_failure(
                reservation,
                forge_error(
                    "SYNAPSE_CALYX_MATH_VRAM_BUDGET_INIT_FAILED",
                    "construct the measured per-dispatch Forge VRAM budget gate",
                    &error,
                ),
            ));
        }
    };
    let (baseline_basis, probe) = match warm_and_verify_cuda_startup_envelope(
        &backend,
        &reservation,
        startup_envelope_mib,
        runtime_ceiling_mib,
    ) {
        Ok(basis) => basis,
        Err(error) => {
            return Err(cleanup_host_reservation_after_startup_failure(
                reservation,
                error,
            ));
        }
    };
    let backend = match backend.with_host_dispatch_reservations(
        reservation_store,
        "synapse-mcp/forge-dispatch",
        format!("synapse-daemon-pid-{}-dispatch", std::process::id()),
    ) {
        Ok(backend) => backend,
        Err(error) => {
            return Err(cleanup_host_reservation_after_startup_failure(
                reservation,
                forge_error(
                    "SYNAPSE_CALYX_MATH_HOST_DISPATCH_INIT_FAILED",
                    "enable measured host-wide reservation for every Forge CUDA dispatch",
                    &error,
                ),
            ));
        }
    };
    runtime_from_backend(
        config,
        backend,
        cpu_readback,
        Some(reservation),
        Some(baseline_basis),
        Some(probe),
    )
}

#[cfg(feature = "calyx-cuda")]
fn cuda_replacement_reservation_id() -> Result<Option<String>, SynapseCalyxError> {
    match std::env::var(CUDA_REPLACEMENT_RESERVATION_ENV) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_GPU_REPLACEMENT_ID_INVALID",
            format!("{CUDA_REPLACEMENT_RESERVATION_ENV} is present but empty"),
            MATH_BACKEND_REMEDIATION,
        )),
        Err(std::env::VarError::NotUnicode(_)) => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_GPU_REPLACEMENT_ID_INVALID",
            format!("{CUDA_REPLACEMENT_RESERVATION_ENV} is not valid Unicode"),
            MATH_BACKEND_REMEDIATION,
        )),
        Err(std::env::VarError::NotPresent) => Ok(None),
    }
}

#[cfg(feature = "calyx-cuda")]
fn warm_and_verify_cuda_startup_envelope(
    backend: &VramBudgetedCudaBackend,
    reservation: &HostGpuReservation,
    startup_envelope_mib: u64,
    runtime_ceiling_mib: u64,
) -> Result<(String, SynapseCalyxMathProbeReport), SynapseCalyxError> {
    let probe = run_startup_probe(backend)?;
    let free_before_cuda_mib = reservation.admitted_snapshot().last_physical_free_mib;
    let measured_snapshot = reservation.readback().map_err(|error| {
        forge_error(
            "SYNAPSE_CALYX_MATH_BASELINE_MEASUREMENT_FAILED",
            "reread physical GPU memory after the warm CUDA probe",
            &error,
        )
    })?;
    let free_after_warmup_mib = measured_snapshot.last_physical_free_mib;
    let global_free_delta_mib = if free_after_warmup_mib >= free_before_cuda_mib {
        i64::try_from(free_after_warmup_mib - free_before_cuda_mib).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(free_before_cuda_mib - free_after_warmup_mib).unwrap_or(i64::MAX)
    };
    let basis = format!(
        "admitted_cuda_context_lifetime_envelope_mib={startup_envelope_mib}; runtime_dispatch_ceiling_mib={runtime_ceiling_mib}; wddm_per_process_vram_readback=unavailable; nvml_device_global_free_before_mib={free_before_cuda_mib}; nvml_device_global_free_after_mib={free_after_warmup_mib}; nvml_device_global_free_delta_mib={global_free_delta_mib}; global_delta_used_for_attribution=false; conservative_envelope_retained=true; every_dispatch_separately_reserves_its_exact_device_buffer_shape=true"
    );
    tracing::info!(
        code = "SYNAPSE_CALYX_MATH_STARTUP_ENVELOPE_RETAINED",
        free_before_cuda_mib,
        free_after_warmup_mib,
        global_free_delta_mib,
        startup_envelope_mib,
        runtime_ceiling_mib,
        reservation_id = reservation.reservation_id(),
        source_of_truth = measured_snapshot.state_path,
        "retained the pre-admitted CUDA context envelope because device-global NVML samples cannot isolate this process on WDDM"
    );
    Ok((basis, probe))
}

fn cleanup_host_reservation_after_startup_failure(
    reservation: HostGpuReservation,
    primary: SynapseCalyxError,
) -> SynapseCalyxError {
    match reservation.release() {
        Ok(snapshot) => {
            tracing::info!(
                code = "SYNAPSE_CALYX_MATH_STARTUP_RESERVATION_RELEASED",
                primary_code = primary.code,
                state_path = snapshot.state_path,
                state_sha256 = snapshot.state_sha256,
                reserved_mib = snapshot.reserved_mib,
                "released host GPU reservation after math startup failure"
            );
            primary
        }
        Err(cleanup_error) => SynapseCalyxError {
            code: "SYNAPSE_CALYX_MATH_STARTUP_RESERVATION_CLEANUP_FAILED",
            message: format!(
                "math startup failed and the exact host reservation could not be explicitly released: primary={primary}; cleanup={cleanup_error}"
            ),
            remediation: MATH_BACKEND_REMEDIATION,
            source_code: Some(cleanup_error.code()),
        },
    }
}

fn cleanup_optional_host_reservation_after_startup_failure(
    reservation: Option<HostGpuReservation>,
    primary: SynapseCalyxError,
) -> SynapseCalyxError {
    match reservation {
        Some(reservation) => cleanup_host_reservation_after_startup_failure(reservation, primary),
        None => primary,
    }
}

/// CUDA kernels are not compiled into this binary. That alone does not decide
/// the outcome: the answer depends on whether this HOST physically has a CUDA
/// device, which NVML can prove independently of the CUDA toolkit because it
/// ships in the display driver.
///
///   * device present  -> hard failure. A CUDA-capable host running a CPU-only
///     binary is a deployment error that must be surfaced, never absorbed.
///   * device absent    -> `SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT`, which
///     `error_proves_cuda_absent` recognises, so `math_backend="auto"` resolves
///     to CPU on physical evidence and records the basis. Explicit
///     `math_backend="cuda"` still fails.
///   * probe malfunction -> hard failure. Uncertainty is never treated as absence.
#[cfg(not(feature = "calyx-cuda"))]
fn initialize_cuda_runtime(
    _config: &SynapseCalyxTuningConfig,
    _cpu_readback: CpuReadback,
) -> Result<InitializedSynapseCalyxMathRuntime, SynapseCalyxError> {
    match calyx_forge::probe_host_cuda_device(0) {
        calyx_forge::HostCudaDeviceVerdict::Present(device) => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_CUDA_NOT_COMPILED",
            format!(
                "this host exposes CUDA device 0 (name={}, uuid={}, total_mib={}) but this synapse-calyx build has no CUDA kernels compiled in; rebuild with --features calyx-cuda (a CUDA 13.3 toolkit must be installed so calyx-forge can run nvcc), or select math_backend=\"cpu\" explicitly to accept CPU math on a GPU host",
                device.name, device.uuid, device.total_mib
            ),
            MATH_BACKEND_REMEDIATION,
        )),
        calyx_forge::HostCudaDeviceVerdict::Absent { basis } => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT",
            format!(
                "this synapse-calyx build has no CUDA kernels compiled in and NVML proved this host has no CUDA device: {basis}"
            ),
            MATH_BACKEND_REMEDIATION,
        )),
        calyx_forge::HostCudaDeviceVerdict::Indeterminate { basis } => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_CUDA_DEVICE_INDETERMINATE",
            format!(
                "this synapse-calyx build has no CUDA kernels compiled in and the NVML device probe could not prove whether this host has a CUDA device: {basis}"
            ),
            MATH_BACKEND_REMEDIATION,
        )),
    }
}

fn runtime_from_backend<B>(
    config: &SynapseCalyxTuningConfig,
    backend: B,
    cpu_readback: CpuReadback,
    host_reservation: Option<HostGpuReservation>,
    host_reservation_basis: Option<String>,
    validated_probe: Option<SynapseCalyxMathProbeReport>,
) -> Result<InitializedSynapseCalyxMathRuntime, SynapseCalyxError>
where
    B: SynapseMathBackend + 'static,
{
    let device_info = backend.device_info();
    let probe = match validated_probe.map_or_else(|| run_startup_probe(&backend), Ok) {
        Ok(probe) => probe,
        Err(error) => {
            return Err(cleanup_optional_host_reservation_after_startup_failure(
                host_reservation,
                error,
            ));
        }
    };
    let dispatch_telemetry = match reset_and_read_dispatch_telemetry(&backend) {
        Ok(status) => status,
        Err(error) => {
            return Err(cleanup_optional_host_reservation_after_startup_failure(
                host_reservation,
                error,
            ));
        }
    };
    let vram_dispatch = match backend.strict_vram_status() {
        Ok(status) => status,
        Err(error) => {
            return Err(cleanup_optional_host_reservation_after_startup_failure(
                host_reservation,
                forge_error(
                    "SYNAPSE_CALYX_MATH_VRAM_READBACK_FAILED",
                    "read process-local VRAM budget and physical CUDA free memory after probe",
                    &error,
                ),
            ));
        }
    };
    let host_reservation_snapshot = match host_reservation
        .as_ref()
        .map(HostGpuReservation::readback)
        .transpose()
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Err(cleanup_optional_host_reservation_after_startup_failure(
                host_reservation,
                forge_error(
                    "SYNAPSE_CALYX_MATH_HOST_RESERVATION_READBACK_FAILED",
                    "reread the measured retained CUDA reservation after the host-admitted probe",
                    &error,
                ),
            ));
        }
    };
    let status = status_from_device_info(
        config,
        &device_info,
        MathStatusInputs {
            cpu_readback,
            vram_dispatch,
            dispatch_telemetry,
            host_reservation: host_reservation.as_ref(),
            host_reservation_snapshot,
            host_reservation_basis,
            probe,
        },
    );
    tracing::info!(
        code = "SYNAPSE_CALYX_MATH_BACKEND_SELECTED",
        requested_backend = status.requested_backend.as_str(),
        selected_backend = status.selected_backend.as_str(),
        cuda_compiled = status.cuda_compiled,
        device_name = status.device_name.as_str(),
        device_vram_mib = status.device_vram_mib,
        cpu_simd_path = status.cpu_simd_path.as_str(),
        vram_budget_bytes = status.vram_budget_bytes,
        vram_budget_enforced = status.vram_dispatch.is_some(),
        vram_dispatch = ?status.vram_dispatch,
        dispatch_telemetry = ?status.dispatch_telemetry,
        host_reservation_basis = status.host_reservation_basis.as_deref().unwrap_or("none"),
        host_reservation = ?status.host_reservation,
        fallback_code = status.fallback_code.as_deref().unwrap_or("none"),
        fallback_source_code = status.fallback_source_code.as_deref().unwrap_or("none"),
        probe_status = status.probe.status.as_str(),
        probe_dot = ?status.probe.dot,
        probe_cosine = ?status.probe.cosine,
        probe_l2_squared = ?status.probe.l2_squared,
        probe_topk = ?status.probe.topk,
        "selected fail-closed Calyx Forge math backend"
    );
    Ok(InitializedSynapseCalyxMathRuntime {
        backend: Arc::new(backend),
        status,
        host_reservation,
    })
}

fn reset_and_read_dispatch_telemetry(
    backend: &impl SynapseMathBackend,
) -> Result<Option<SynapseCalyxMathDispatchTelemetry>, SynapseCalyxError> {
    backend
        .reset_serving_dispatch_telemetry()
        .map_err(|error| {
            forge_error(
                "SYNAPSE_CALYX_MATH_DISPATCH_TELEMETRY_RESET_FAILED",
                "reset process-local Forge dispatch telemetry after every startup probe passed",
                &error,
            )
        })?;
    backend.strict_dispatch_telemetry().map_err(|error| {
        forge_error(
            "SYNAPSE_CALYX_MATH_DISPATCH_TELEMETRY_READBACK_FAILED",
            "read back the zeroed serving dispatch epoch after startup probes",
            &error,
        )
    })
}

struct MathStatusInputs<'a> {
    cpu_readback: CpuReadback,
    vram_dispatch: Option<SynapseCalyxVramDispatchStatus>,
    dispatch_telemetry: Option<SynapseCalyxMathDispatchTelemetry>,
    host_reservation: Option<&'a HostGpuReservation>,
    host_reservation_snapshot: Option<HostGpuReservationSnapshot>,
    host_reservation_basis: Option<String>,
    probe: SynapseCalyxMathProbeReport,
}

fn status_from_device_info(
    config: &SynapseCalyxTuningConfig,
    info: &DeviceInfo,
    inputs: MathStatusInputs<'_>,
) -> SynapseCalyxMathBackendStatus {
    let MathStatusInputs {
        cpu_readback,
        vram_dispatch,
        dispatch_telemetry,
        host_reservation,
        host_reservation_snapshot,
        host_reservation_basis,
        probe,
    } = inputs;
    SynapseCalyxMathBackendStatus {
        requested_backend: config.math_backend,
        selected_backend: info.kind.to_string(),
        cuda_compiled: CUDA_COMPILED,
        device_name: info.name.clone(),
        device_vram_mib: info.vram_mib,
        cpu_simd_path: cpu_readback.simd_path,
        vram_budget_bytes: config.vram_budget_bytes,
        vram_dispatch,
        dispatch_telemetry,
        host_reservation_basis,
        host_reservation_id: host_reservation
            .map(|reservation| reservation.reservation_id().to_owned()),
        host_reservation: host_reservation_snapshot,
        runtime_readback_code: None,
        runtime_readback_error: None,
        fallback_code: None,
        fallback_source_code: None,
        fallback_error: None,
        probe,
    }
}

fn run_startup_probe(
    backend: &dyn Backend,
) -> Result<SynapseCalyxMathProbeReport, SynapseCalyxError> {
    let mut dot = vec![0.0; EXPECTED_DOT.len()];
    backend
        .dot(&PROBE_QUERY, &PROBE_CANDIDATES, PROBE_DIM, &mut dot)
        .map_err(|error| probe_error("dot", &error))?;
    assert_close_vec("dot", &dot, &EXPECTED_DOT)?;

    let mut cosine = vec![0.0; EXPECTED_COSINE.len()];
    backend
        .cosine(&PROBE_QUERY, &PROBE_CANDIDATES, PROBE_DIM, &mut cosine)
        .map_err(|error| probe_error("cosine", &error))?;
    assert_close_vec("cosine", &cosine, &EXPECTED_COSINE)?;

    let mut l2_squared = vec![0.0; EXPECTED_L2_SQUARED.len()];
    backend
        .l2(&PROBE_QUERY, &PROBE_CANDIDATES, PROBE_DIM, &mut l2_squared)
        .map_err(|error| probe_error("l2", &error))?;
    assert_close_vec("l2", &l2_squared, &EXPECTED_L2_SQUARED)?;

    let topk = backend
        .topk(&PROBE_TOPK_SCORES, EXPECTED_TOPK.len())
        .map_err(|error| probe_error("topk", &error))?;
    assert_topk(&topk)?;

    let reduction_backend = assert_reduction_paths_agree()?;

    Ok(SynapseCalyxMathProbeReport {
        status: "ok".to_owned(),
        detail: format!(
            "fixed vectors matched expected dot={EXPECTED_DOT:?} cosine={EXPECTED_COSINE:?} l2_squared={EXPECTED_L2_SQUARED:?} topk={EXPECTED_TOPK:?}; cpu_reduction_probe_kernel={reduction_backend} agrees bit-for-bit with the portable path over {REDUCTION_PROBE_LEN} elements"
        ),
        tolerance: PROBE_TOLERANCE,
        dot,
        cosine,
        l2_squared,
        topk: topk
            .into_iter()
            .map(|(index, score)| SynapseCalyxMathProbeTopKEntry { index, score })
            .collect(),
    })
}

/// Prove on this host that `calyx-forge`'s runtime-dispatched CPU kernels return
/// bit-identical results to its portable kernels.
///
/// The fixed probe above runs at `PROBE_DIM = 3`, which is shorter than the
/// 8-lane accumulator, so it exercises only the scalar tail and would not notice
/// a dispatched kernel that disagreed in its vector body. This check uses a
/// length that is deliberately **not** a multiple of the lane width, so both the
/// vector body and the tail are covered, and it fails closed: a host where the
/// two paths disagree has a broken reduction contract and must not open a vault
/// whose derived artifacts would then depend on which kernel happened to run.
fn assert_reduction_paths_agree() -> Result<&'static str, SynapseCalyxError> {
    let probe_len = u16::try_from(REDUCTION_PROBE_LEN).map_err(|error| {
        probe_mismatch(format!(
            "fixed reduction probe length {REDUCTION_PROBE_LEN} exceeds the exact u16/f32 construction range: {error}"
        ))
    })?;
    let left: Vec<f32> = (0..probe_len)
        .map(|index| f32::from(index).mul_add(0.37, -2.5))
        .collect();
    let right: Vec<f32> = (0..probe_len)
        .map(|index| f32::from(index).mul_add(-0.11, 1.75))
        .collect();
    calyx_forge::cpu::simd::reduction_paths_agree(&left, &right).map_or_else(
        || Ok(calyx_forge::cpu::simd::backend_name()),
        |kernel| Err(probe_mismatch(format!(
            "cpu reduction kernel `{kernel}` disagrees bit-for-bit between the runtime-dispatched \
             backend `{}` and the portable f32x8 backend over {REDUCTION_PROBE_LEN} elements; the \
             two paths are required to fold identically (calyx-forge cpu::simd), so a mismatch \
             means the dispatched kernel changed the reduction order and every derived artifact \
             would depend on which kernel ran",
            calyx_forge::cpu::simd::backend_name()
        ))),
    )
}

fn assert_close_vec(
    label: &'static str,
    actual: &[f32],
    expected: &[f32],
) -> Result<(), SynapseCalyxError> {
    if actual.len() != expected.len() {
        return Err(probe_mismatch(format!(
            "{label} length mismatch actual_len={} expected_len={}",
            actual.len(),
            expected.len()
        )));
    }
    for (index, (actual_value, expected_value)) in actual.iter().zip(expected).enumerate() {
        if (*actual_value - *expected_value).abs() > PROBE_TOLERANCE {
            return Err(probe_mismatch(format!(
                "{label}[{index}] actual={actual_value} expected={expected_value} tolerance={PROBE_TOLERANCE}; actual={actual:?} expected={expected:?}"
            )));
        }
    }
    Ok(())
}

fn assert_topk(actual: &[(usize, f32)]) -> Result<(), SynapseCalyxError> {
    if actual.len() != EXPECTED_TOPK.len() {
        return Err(probe_mismatch(format!(
            "topk length mismatch actual_len={} expected_len={}",
            actual.len(),
            EXPECTED_TOPK.len()
        )));
    }
    for (rank, ((actual_index, actual_score), (expected_index, expected_score))) in
        actual.iter().zip(EXPECTED_TOPK).enumerate()
    {
        if *actual_index != expected_index
            || (*actual_score - expected_score).abs() > PROBE_TOLERANCE
        {
            return Err(probe_mismatch(format!(
                "topk[{rank}] actual=({actual_index}, {actual_score}) expected=({expected_index}, {expected_score}) tolerance={PROBE_TOLERANCE}; actual={actual:?} expected={EXPECTED_TOPK:?}"
            )));
        }
    }
    Ok(())
}

fn probe_error(op: &'static str, error: &ForgeError) -> SynapseCalyxError {
    forge_error(
        "SYNAPSE_CALYX_MATH_PROBE_FAILED",
        format!("run Calyx math startup probe op={op}"),
        error,
    )
}

fn probe_mismatch(message: String) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_MATH_PROBE_MISMATCH",
        message,
        MATH_BACKEND_REMEDIATION,
    )
}

fn forge_error(
    code: &'static str,
    action: impl AsRef<str>,
    error: &ForgeError,
) -> SynapseCalyxError {
    SynapseCalyxError {
        code,
        message: format!("{}: {error}", action.as_ref()),
        remediation: MATH_BACKEND_REMEDIATION,
        source_code: Some(error.code()),
    }
}
