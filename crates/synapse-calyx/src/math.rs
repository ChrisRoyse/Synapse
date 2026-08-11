use std::fmt;

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

pub struct SynapseCalyxMathRuntime {
    backend: Box<dyn SynapseMathBackend>,
    status: SynapseCalyxMathBackendStatus,
    host_reservation: Option<HostGpuReservation>,
}

impl fmt::Debug for SynapseCalyxMathRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SynapseCalyxMathRuntime")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl SynapseCalyxMathRuntime {
    #[must_use]
    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    #[must_use]
    pub const fn status(&self) -> &SynapseCalyxMathBackendStatus {
        &self.status
    }

    #[must_use]
    pub fn status_snapshot(&self) -> SynapseCalyxMathBackendStatus {
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

    /// Drops the CUDA backend/context first, then explicitly removes and
    /// rereads the host reservation row before shutdown may release the vault
    /// lifetime lock.
    ///
    /// # Errors
    ///
    /// Returns a structured Forge-derived error if the persisted reservation
    /// cannot be removed, reread, unlocked, or physically deleted.
    pub fn close(self) -> Result<Option<HostGpuReservationSnapshot>, SynapseCalyxError> {
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

/// Builds the single Calyx Forge math backend for this Synapse process.
///
/// # Errors
///
/// Returns a structured error when config is invalid or the selected runtime
/// cannot initialize, reserve its physical GPU capacity, or pass the startup
/// probe. `auto` prefers CUDA and selects CPU only when the failure proves
/// that no supported CUDA device/runtime exists. A present-but-broken GPU,
/// admission failure, or CUDA execution failure remains a hard error.
pub fn math_backend(
    config: &SynapseCalyxTuningConfig,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
    config.clone().validate()?;
    let cpu_reference = CpuBackend::new();
    let cpu_readback = CpuReadback::from_backend(&cpu_reference);
    match config.math_backend {
        SynapseCalyxMathBackend::Cpu => {
            runtime_from_backend(config, cpu_reference, cpu_readback, None, None)
        }
        SynapseCalyxMathBackend::Cuda => cuda_runtime_candidate(config, cpu_readback),
        SynapseCalyxMathBackend::Auto => {
            match cuda_runtime_candidate(config, cpu_readback.clone()) {
                Ok(runtime) => Ok(runtime),
                Err(error) if error_proves_cuda_absent(&error) => {
                    let mut runtime =
                        runtime_from_backend(config, cpu_reference, cpu_readback, None, None)?;
                    runtime.status.fallback_code =
                        Some("SYNAPSE_CALYX_MATH_AUTO_CPU_NO_CUDA_DEVICE".to_owned());
                    runtime.status.fallback_source_code = Some(error.code.to_owned());
                    runtime.status.fallback_error = Some(error.to_string());
                    tracing::warn!(
                        code = "SYNAPSE_CALYX_MATH_AUTO_CPU_NO_CUDA_DEVICE",
                        source_code = error.code,
                        source_error = %error,
                        cpu_simd_path = runtime.status.cpu_simd_path,
                        "auto selected the explicit CPU runtime because no supported CUDA device exists"
                    );
                    Ok(runtime)
                }
                Err(error) => Err(error),
            }
        }
    }
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
    if error.code == "SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT" {
        return true;
    }
    let detail = error.message.to_ascii_lowercase();
    detail.contains("cuda_error_no_device")
        || detail.contains("no cuda-capable device is detected")
        || detail.contains("nvml init failed loading nvml.dll")
        || (detail.contains("nvml device_by_index(0) failed")
            && (detail.contains("not found") || detail.contains("no device")))
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
fn cuda_runtime_candidate(
    config: &SynapseCalyxTuningConfig,
    cpu_readback: CpuReadback,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
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
    let baseline_basis = match warm_and_verify_cuda_startup_envelope(
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
) -> Result<String, SynapseCalyxError> {
    run_startup_probe(backend)?;
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
    Ok(basis)
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
fn cuda_runtime_candidate(
    _config: &SynapseCalyxTuningConfig,
    _cpu_readback: CpuReadback,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
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
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError>
where
    B: SynapseMathBackend + 'static,
{
    let device_info = backend.device_info();
    let probe = match run_startup_probe(&backend) {
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
    Ok(SynapseCalyxMathRuntime {
        backend: Box::new(backend),
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
