//! VRAM budgeting + admission control for `calyx-forge`.
//!
//! `calyx-forge` can share a single CUDA GPU with co-resident embedding and
//! telemetry services. Every large GPU dispatch must therefore be
//! *admitted* against two independent limits before it runs:
//!
//! 1. a **soft cap** (`CALYX_FORGE_VRAM_BUDGET`) on Forge's own cumulative
//!    allocation, enforced by an atomic usage counter shared across subsystems;
//! 2. the **live device free-VRAM headroom**, queried per-dispatch via
//!    `cudaMemGetInfo` so the budgeter never assumes a fixed 32 GiB and always
//!    yields to whatever the TEI residents are currently using.
//!
//! The accounting logic ([`VramBudgeter`]) is intentionally generic over a
//! [`VramProbe`] — the hardware boundary. In production the probe is
//! [`CudaVramProbe`], which calls `cudaMemGetInfo` on the real device. In unit
//! tests a deterministic probe supplies a known free-VRAM value so the
//! soft-cap / headroom decision logic can be exercised with hand-computed
//! byte counts on a CPU-only box. The system under test (the accounting) runs
//! on real bytes; only the external GPU reading is injected.

pub mod admission;
pub mod budget;
#[cfg(feature = "cuda")]
pub mod budgeted_backend;
pub mod host_reservation;
pub mod lru_evict;
pub mod oom_guard;
pub mod yield_policy;

pub use admission::{
    AdmissionController, AdmissionOutput, AdmitDecision, LENS_VRAM_BUDGET_REMEDIATION,
    LensAdmission, LensAdmissionPlacement, LensAdmissionRequest, QueuedDispatch, admit_lens,
};
pub use budget::{
    Category, DEFAULT_SOFT_CAP_BYTES, RESERVED_HEADROOM_BYTES, VRAM_BUDGET_ENV,
    VRAM_BUDGET_REMEDIATION, VramBudgeter, VramGuard,
};
#[cfg(feature = "cuda")]
pub use budgeted_backend::VramBudgetedCudaBackend;
pub use host_reservation::{
    DEFAULT_HOST_CAP_MIB, DEFAULT_HOST_HEADROOM_MIB, DEFAULT_REQUIRED_FREE_MIB, HOST_CAP_MIB_ENV,
    HOST_RESERVATION_ROOT_ENV, HostCudaDeviceVerdict, HostGpuReservation,
    HostGpuReservationRequest, HostGpuReservationSnapshot, HostGpuReservationStore,
    HostGpuReservationView, PhysicalGpuDevice, probe_host_cuda_device,
};
pub use lru_evict::{
    BlockDeallocator, BlockId, BlockKind, DevicePtr, GpuBlockRegistry, GpuBlockStats,
};
#[cfg(feature = "cuda")]
pub use oom_guard::RawCudaMalloc;
pub use oom_guard::{CudaAllocError, CudaMalloc, DEFAULT_OOM_MAX_RETRIES, OomGuard, OomGuardStats};
#[cfg(feature = "cuda")]
pub use yield_policy::CudaStream;
pub use yield_policy::{
    ANNEAL_VRAM_BUDGET_ENV, DEFAULT_ANNEAL_THROTTLE_SLEEP, DEFAULT_ANNEAL_VRAM_CAP_BYTES,
    DEFAULT_POWER_BACKOFF_THRESHOLD_W, NvmlPowerProbe, PowerProbe, YieldPolicy,
};

use crate::Result;

/// The hardware boundary the budgeter consults for current free device VRAM.
///
/// Implementations return the number of bytes currently free on the GPU. The
/// only production implementation is [`CudaVramProbe`]; tests inject a
/// deterministic probe. A probe MUST fail loud (return `Err`) rather than
/// guess — the budgeter treats any probe error as "device state unknown =
/// over-budget" (fail-closed).
pub trait VramProbe: Send + Sync {
    /// Current free device VRAM in bytes. `Err` on any inability to read the
    /// device — never a zero-fill fallback.
    fn free_device_vram(&self) -> Result<usize>;
}

/// A point-in-time snapshot of the budgeter's accounting and the live device
/// free-VRAM reading. This is the Source-of-Truth surface for FSV readback:
/// `allocated_bytes` is Forge's own reserved total, `device_free_bytes` is the
/// `cudaMemGetInfo` reading at the moment [`VramBudgeter::stats`] was called.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct VramStats {
    /// Configured soft cap on Forge's cumulative allocation (bytes).
    pub soft_cap_bytes: usize,
    /// Forge's currently reserved total (sum of live [`VramGuard`]s), in bytes.
    pub allocated_bytes: usize,
    /// Serving/search/embed reserved bytes.
    pub serving_allocated_bytes: usize,
    /// Anneal/autotune/background reserved bytes.
    pub anneal_allocated_bytes: usize,
    /// Live free device VRAM (bytes) at snapshot time; `0` if the probe failed
    /// (a failure is logged at warn level — `0` is a visible alarm, never a
    /// silent success).
    pub device_free_bytes: usize,
    /// Cumulative admission decisions that proceeded immediately. A full-batch
    /// admission is recorded as a no-op split with `sub_batch_size == batch`.
    pub splits_total: u64,
    /// Legacy queue counter retained for metrics compatibility. The hidden
    /// admission queue is disabled, so this value is always zero.
    pub queued_total: u64,
    /// Cumulative admission decisions that failed closed with
    /// `CALYX_FORGE_VRAM_BUDGET`.
    pub failed_total: u64,
    /// Cumulative last-resort CUDA OOM guard counters.
    pub oom_guard: OomGuardStats,
    /// Anneal yield/cap counters.
    pub yield_stats: YieldStats,
}

/// Counters for Anneal's background-yield controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default, serde::Serialize)]
pub struct YieldStats {
    /// Times Anneal dispatch was throttled due to sustained high GPU power.
    pub anneal_throttle_events: u64,
    /// Anneal VRAM reservations rejected by the background sub-budget.
    pub anneal_vram_rejections: u64,
}

impl VramStats {
    /// Prometheus text readback for PH57 admission and OOM guard counters.
    pub fn admission_metrics_text(&self) -> String {
        format!(
            concat!(
                "# HELP calyx_forge_vram_admission_splits_total Forge VRAM dispatches admitted for immediate execution.\n",
                "# TYPE calyx_forge_vram_admission_splits_total counter\n",
                "calyx_forge_vram_admission_splits_total {}\n",
                "# HELP forge_vram_serving_allocated_bytes Serving VRAM bytes currently reserved by Forge.\n",
                "# TYPE forge_vram_serving_allocated_bytes gauge\n",
                "forge_vram_serving_allocated_bytes {}\n",
                "# HELP forge_vram_anneal_allocated_bytes Anneal VRAM bytes currently reserved by Forge.\n",
                "# TYPE forge_vram_anneal_allocated_bytes gauge\n",
                "forge_vram_anneal_allocated_bytes {}\n",
                "# HELP calyx_forge_vram_admission_queued_total Legacy queue counter; hidden admission queue is disabled.\n",
                "# TYPE calyx_forge_vram_admission_queued_total counter\n",
                "calyx_forge_vram_admission_queued_total {}\n",
                "# HELP calyx_forge_vram_budget_exceeded_total Forge VRAM dispatches failed closed by CALYX_FORGE_VRAM_BUDGET.\n",
                "# TYPE calyx_forge_vram_budget_exceeded_total counter\n",
                "calyx_forge_vram_budget_exceeded_total {}\n",
                "# HELP forge_oom_intercepts_total CUDA allocation OOM responses intercepted by Forge.\n",
                "# TYPE forge_oom_intercepts_total counter\n",
                "forge_oom_intercepts_total {}\n",
                "# HELP forge_oom_batch_reductions_total Dispatch retries that reduced batch size after a budget OOM.\n",
                "# TYPE forge_oom_batch_reductions_total counter\n",
                "forge_oom_batch_reductions_total {}\n",
                "# HELP forge_oom_final_failures_total OOM guard paths that failed closed with CALYX_FORGE_VRAM_BUDGET.\n",
                "# TYPE forge_oom_final_failures_total counter\n",
                "forge_oom_final_failures_total {}\n",
                "# HELP forge_anneal_throttle_events_total Anneal dispatch throttles due to GPU power backoff.\n",
                "# TYPE forge_anneal_throttle_events_total counter\n",
                "forge_anneal_throttle_events_total {}\n",
                "# HELP forge_anneal_vram_rejections_total Anneal VRAM reservations rejected by the Anneal sub-budget.\n",
                "# TYPE forge_anneal_vram_rejections_total counter\n",
                "forge_anneal_vram_rejections_total {}\n"
            ),
            self.splits_total,
            self.serving_allocated_bytes,
            self.anneal_allocated_bytes,
            self.queued_total,
            self.failed_total,
            self.oom_guard.oom_intercepts,
            self.oom_guard.batch_reductions,
            self.oom_guard.final_failures,
            self.yield_stats.anneal_throttle_events,
            self.yield_stats.anneal_vram_rejections
        )
    }
}

/// Production [`VramProbe`] backed by a real CUDA context.
///
/// Calls [`crate::cuda::CudaContext::free_device_vram_bytes`] (`cudaMemGetInfo`)
/// on every query, so the reading always reflects concurrent TEI residents.
#[cfg(feature = "cuda")]
#[derive(Clone)]
pub struct CudaVramProbe {
    ctx: std::sync::Arc<crate::cuda::CudaContext>,
}

#[cfg(feature = "cuda")]
impl CudaVramProbe {
    /// Build a probe over a shared CUDA context.
    pub fn new(ctx: std::sync::Arc<crate::cuda::CudaContext>) -> Self {
        Self { ctx }
    }
}

#[cfg(feature = "cuda")]
impl VramProbe for CudaVramProbe {
    fn free_device_vram(&self) -> Result<usize> {
        self.ctx.free_device_vram_bytes()
    }
}
