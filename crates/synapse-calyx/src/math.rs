use std::fmt;

use calyx_forge::{
    Backend, CUDA_COMPILED, CpuBackend, DeviceInfo, ForgeError, HostGpuReservation,
    HostGpuReservationRequest, HostGpuReservationSnapshot, HostGpuReservationStore, VramStats,
};
#[cfg(feature = "calyx-cuda")]
use calyx_forge::{CudaBackend, VramBudgetedCudaBackend};
use serde::Serialize;

use crate::{SynapseCalyxError, SynapseCalyxMathBackend, SynapseCalyxTuningConfig};

const MATH_BACKEND_REMEDIATION: &str = "inspect the SYNAPSE_CALYX_MATH_* structured events, health payload, CUDA driver state, and Calyx Forge error; use math_backend=\"cpu\" only when intentionally forcing CPU";
const BYTES_PER_MIB: u64 = 1024 * 1024;
// CUDA context/module initialization is not routed through Forge's dispatch
// allocator, so it needs a conservative, declared host-wide envelope while
// its retained footprint is measured. This is intentionally independent from
// the maximum runtime dispatch budget: reserving that entire ceiling makes a
// candidate-before-handoff install impossible while a healthy daemon retains
// its measured baseline.
const CUDA_STARTUP_ENVELOPE_MIB: u64 = 4 * 1024;
const CUDA_REPLACEMENT_RESERVATION_ENV: &str = "SYNAPSE_CALYX_GPU_REPLACEMENT_RESERVATION_ID";
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
    pub device_avx512: bool,
    pub cpu_avx512_available: bool,
    pub cpu_simd_path: String,
    pub vram_budget_bytes: u64,
    pub vram_dispatch: Option<SynapseCalyxVramDispatchStatus>,
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
        format!(
            "requested_backend={} selected_backend={} cuda_compiled={} device_name={} device_vram_mib={:?} device_avx512={} cpu_avx512_available={} cpu_simd_path={} vram_budget_bytes={} vram_budget_enforced={} dispatch_soft_cap_bytes={:?} dispatch_allocated_bytes={:?} dispatch_device_free_bytes={:?} host_reservation_basis={} host_reservation_state_path={} host_reservation_sha256={} host_reservation_id={} host_reservation_pid={:?} host_reservation_requested_mib={:?} runtime_readback_code={} runtime_readback_error={} fallback_code={} fallback_source_code={} probe_status={} probe_detail={}",
            self.requested_backend.as_str(),
            self.selected_backend,
            self.cuda_compiled,
            self.device_name,
            self.device_vram_mib,
            self.device_avx512,
            self.cpu_avx512_available,
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
/// probe. `auto` is strict when CUDA is compiled: it selects CUDA and never
/// degrades to CPU. CPU execution requires the explicit `cpu` selection.
pub fn math_backend(
    config: &SynapseCalyxTuningConfig,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
    config.validate()?;
    let cpu_reference = CpuBackend::new();
    let cpu_readback = CpuReadback::from_backend(&cpu_reference);
    match config.math_backend {
        SynapseCalyxMathBackend::Cpu => {
            runtime_from_backend(config, cpu_reference, cpu_readback, None, None)
        }
        SynapseCalyxMathBackend::Auto | SynapseCalyxMathBackend::Cuda => {
            cuda_runtime_candidate(config, cpu_readback)
        }
    }
}

#[derive(Clone, Debug)]
struct CpuReadback {
    avx512_available: bool,
    simd_path: String,
}

impl CpuReadback {
    fn from_backend(backend: &CpuBackend) -> Self {
        Self {
            avx512_available: backend.avx512_available(),
            simd_path: backend.simd_path().to_owned(),
        }
    }
}

trait SynapseMathBackend: Backend {
    fn strict_vram_status(&self) -> Result<Option<SynapseCalyxVramDispatchStatus>, ForgeError>;
}

impl SynapseMathBackend for CpuBackend {
    fn strict_vram_status(&self) -> Result<Option<SynapseCalyxVramDispatchStatus>, ForgeError> {
        Ok(None)
    }
}

#[cfg(feature = "calyx-cuda")]
impl SynapseMathBackend for VramBudgetedCudaBackend {
    fn strict_vram_status(&self) -> Result<Option<SynapseCalyxVramDispatchStatus>, ForgeError> {
        self.stats_strict()
            .map(SynapseCalyxVramDispatchStatus::from)
            .map(Some)
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
            "synapse-mcp temporary CUDA startup envelope; envelope_mib={startup_envelope_mib}; runtime_dispatch_ceiling_mib={runtime_ceiling_mib}; atomically resized after retained-footprint measurement"
        ),
        startup_envelope_mib,
    );
    if let Some(replacement_id) = cuda_replacement_reservation_id()? {
        request = request.replacing(replacement_id);
    }
    let mut reservation = reservation_store.acquire(request).map_err(|error| {
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
    let baseline_basis = match warm_and_measure_cuda_baseline(
        &backend,
        &mut reservation,
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
fn warm_and_measure_cuda_baseline(
    backend: &VramBudgetedCudaBackend,
    reservation: &mut HostGpuReservation,
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
    if free_after_warmup_mib > free_before_cuda_mib {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_BASELINE_MEASUREMENT_DRIFT",
            format!(
                "physical free VRAM increased during the exclusive participating-process startup guard: before_mib={free_before_cuda_mib} after_mib={free_after_warmup_mib}; retained CUDA footprint is not provable"
            ),
            MATH_BACKEND_REMEDIATION,
        ));
    }
    let retained_cuda_mib = free_before_cuda_mib
        .saturating_sub(free_after_warmup_mib)
        .max(1);
    if retained_cuda_mib > startup_envelope_mib {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MATH_STARTUP_ENVELOPE_EXCEEDED",
            format!(
                "measured retained CUDA startup footprint exceeds its admitted envelope: retained_mib={retained_cuda_mib} startup_envelope_mib={startup_envelope_mib} runtime_dispatch_ceiling_mib={runtime_ceiling_mib}; refusing a runtime whose context allocation was not fully covered by the declared host claim"
            ),
            MATH_BACKEND_REMEDIATION,
        ));
    }
    let basis = format!(
        "admitted_startup_envelope_mib={startup_envelope_mib}; runtime_dispatch_ceiling_mib={runtime_ceiling_mib}; nvml_free_delta_after_cuda_context_and_warm_probe_rounded_up_to_mib_with_minimum_1mib_resolution; free_before_mib={free_before_cuda_mib}; free_after_mib={free_after_warmup_mib}; retained_mib={retained_cuda_mib}; measured_retained_within_startup_envelope=true; every dispatch separately reserves its exact device-buffer shape"
    );
    let command = format!("synapse-mcp retained CUDA baseline; {basis}");
    reservation
        .resize(retained_cuda_mib, &command)
        .map_err(|error| {
            forge_error(
                "SYNAPSE_CALYX_MATH_BASELINE_RESIZE_FAILED",
                "atomically resize the startup guard to the measured retained CUDA footprint",
                &error,
            )
        })?;
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

#[cfg(not(feature = "calyx-cuda"))]
fn cuda_runtime_candidate(
    _config: &SynapseCalyxTuningConfig,
    _cpu_readback: CpuReadback,
) -> Result<SynapseCalyxMathRuntime, SynapseCalyxError> {
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_MATH_CUDA_NOT_COMPILED",
        "the selected auto/cuda path requires a CUDA-enabled synapse-calyx build; CPU execution is available only through the explicit math_backend=\"cpu\" selection",
        MATH_BACKEND_REMEDIATION,
    ))
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
        device_avx512 = status.device_avx512,
        cpu_avx512_available = status.cpu_avx512_available,
        cpu_simd_path = status.cpu_simd_path.as_str(),
        vram_budget_bytes = status.vram_budget_bytes,
        vram_budget_enforced = status.vram_dispatch.is_some(),
        vram_dispatch = ?status.vram_dispatch,
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

struct MathStatusInputs<'a> {
    cpu_readback: CpuReadback,
    vram_dispatch: Option<SynapseCalyxVramDispatchStatus>,
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
        device_avx512: info.avx512,
        cpu_avx512_available: cpu_readback.avx512_available,
        cpu_simd_path: cpu_readback.simd_path,
        vram_budget_bytes: config.vram_budget_bytes,
        vram_dispatch,
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

    Ok(SynapseCalyxMathProbeReport {
        status: "ok".to_owned(),
        detail: format!(
            "fixed vectors matched expected dot={EXPECTED_DOT:?} cosine={EXPECTED_COSINE:?} l2_squared={EXPECTED_L2_SQUARED:?} topk={EXPECTED_TOPK:?}"
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
