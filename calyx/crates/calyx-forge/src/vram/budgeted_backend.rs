//! CUDA backend wrapper that admits every host-backed dispatch through the
//! process-local [`VramBudgeter`] before the inner backend can allocate or
//! mutate a device output.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::vram::{
    CudaVramProbe, HostGpuReservation, HostGpuReservationRequest, HostGpuReservationStore,
    VramBudgeter, VramStats,
};
use crate::{
    Backend, BlockId, BudgetedDeviceCandidateBlock, CpuGatherReverificationReadback,
    CpuGatherReverificationRequest, CudaBackend, CudaMmdResult, DeviceInfo, ForgeError, KnnBatch,
    KnnMetric, ResidentGatherReadback, Result,
};

const F32_BYTES: usize = size_of::<f32>();
const I32_BYTES: usize = size_of::<i32>();
const BYTES_PER_MIB: usize = 1024 * 1024;
const DISPATCH_REMEDIATION: &str = "reduce the exact input/batch dimensions or raise the explicit Forge VRAM budget only after measuring the operation; never bypass admission or move the dispatch to CPU implicitly";
const DISPATCH_OPERATIONS: [&str; 10] = [
    "gemm",
    "cosine",
    "dot",
    "l2",
    "normalize",
    "topk",
    "knn",
    "paired_cosine",
    "gaussian_mmd",
    "l2_gather",
];

/// Process-local CUDA dispatch telemetry since the serving epoch began.
///
/// The startup probes are deliberately removed from this epoch by Synapse after
/// they pass. Unlike the host reservation ledger, these counters describe this
/// backend instance, preserve the exact Forge operation, and classify the final
/// result only after the host reservation release has been read back.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CudaDispatchTelemetrySnapshot {
    pub epoch_started_unix_ms: u64,
    pub sampled_at_unix_ms: u64,
    pub operations: Vec<CudaDispatchOperationSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CudaDispatchOperationSnapshot {
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

#[derive(Debug)]
struct DispatchTelemetry {
    epoch_started_unix_ms: u64,
    serving_epoch_started: bool,
    operations: [DispatchOperationState; DISPATCH_OPERATIONS.len()],
}

#[derive(Clone, Debug, Default)]
struct DispatchOperationState {
    attempted_total: u64,
    in_flight: u64,
    succeeded_total: u64,
    refused_total: u64,
    failed_total: u64,
    attempted_measured_bytes_total: u64,
    in_flight_measured_bytes: u64,
    succeeded_measured_bytes_total: u64,
    refused_measured_bytes_total: u64,
    failed_measured_bytes_total: u64,
    last_attempt_unix_ms: Option<u64>,
    last_success_unix_ms: Option<u64>,
    last_error_unix_ms: Option<u64>,
    last_error_code: Option<String>,
}

impl DispatchTelemetry {
    fn new(epoch_started_unix_ms: u64) -> Self {
        Self {
            epoch_started_unix_ms,
            serving_epoch_started: false,
            operations: std::array::from_fn(|_| DispatchOperationState::default()),
        }
    }
}

enum DispatchOutcome<'a> {
    Succeeded,
    Refused(&'a ForgeError),
    Failed(&'a ForgeError),
}

/// CUDA backend whose every public [`Backend`] operation has a measured
/// buffer-shape reservation held until the inner CUDA call returns.
///
/// The reservation is based on the device buffers the concrete Forge kernels
/// allocate for that operation, not on the configured ceiling. The ceiling is
/// only the upper bound enforced by [`VramBudgeter`].
pub struct VramBudgetedCudaBackend {
    inner: CudaBackend,
    budgeter: VramBudgeter<CudaVramProbe>,
    host_dispatch: Option<HostDispatchReservations>,
    dispatch_telemetry: Mutex<DispatchTelemetry>,
}

struct HostDispatchReservations {
    store: HostGpuReservationStore,
    owner: String,
    job_id_prefix: String,
    next_dispatch_id: AtomicU64,
}

impl VramBudgetedCudaBackend {
    /// Wraps an initialized CUDA backend with an explicit process-local cap.
    ///
    /// # Errors
    ///
    /// Returns `CALYX_FORGE_VRAM_BUDGET` when the cap is zero or cannot fit in
    /// this process.
    pub fn new(inner: CudaBackend, soft_cap_bytes: u64) -> Result<Self> {
        let soft_cap_bytes = usize::try_from(soft_cap_bytes).map_err(|_| {
            budget_error(format!(
                "configured soft cap {soft_cap_bytes} does not fit usize"
            ))
        })?;
        if soft_cap_bytes == 0 {
            return Err(budget_error(
                "configured soft cap must be greater than zero".to_owned(),
            ));
        }
        let probe = CudaVramProbe::new(Arc::new(inner.context().clone()));
        let telemetry_epoch = unix_ms()?;
        Ok(Self {
            inner,
            budgeter: VramBudgeter::with_soft_cap(soft_cap_bytes, probe),
            host_dispatch: None,
            dispatch_telemetry: Mutex::new(DispatchTelemetry::new(telemetry_epoch)),
        })
    }

    /// Enables measured host-wide admission for every subsequent backend
    /// dispatch. The process-local budget remains the concurrency ceiling;
    /// each host row represents only the concrete device buffers for one call.
    pub fn with_host_dispatch_reservations(
        mut self,
        store: HostGpuReservationStore,
        owner: impl Into<String>,
        job_id_prefix: impl Into<String>,
    ) -> Result<Self> {
        let owner = owner.into();
        let job_id_prefix = job_id_prefix.into();
        for (field, value) in [
            ("owner", owner.as_str()),
            ("job_id_prefix", job_id_prefix.as_str()),
        ] {
            if value.trim().is_empty() || value.len() > 384 || value.chars().any(char::is_control) {
                return Err(budget_error(format!(
                    "host dispatch {field} must be non-blank, control-free, and at most 384 bytes"
                )));
            }
        }
        self.host_dispatch = Some(HostDispatchReservations {
            store,
            owner,
            job_id_prefix,
            next_dispatch_id: AtomicU64::new(1),
        });
        Ok(self)
    }

    #[must_use]
    pub fn soft_cap_bytes(&self) -> usize {
        self.budgeter.soft_cap_bytes()
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.budgeter.allocated_bytes()
    }

    #[must_use]
    pub fn stats(&self) -> VramStats {
        self.budgeter.stats()
    }

    /// Returns process-local allocation counters plus a fail-closed live CUDA
    /// free-memory read.
    pub fn stats_strict(&self) -> Result<VramStats> {
        self.budgeter.stats_strict()
    }

    /// Resets process-local dispatch telemetry after startup validation so the
    /// published epoch contains serving work only.
    pub fn reset_dispatch_telemetry(&self) -> Result<()> {
        let mut telemetry = self.dispatch_telemetry.lock().map_err(|_| {
            telemetry_error("dispatch telemetry mutex is poisoned during serving-epoch reset")
        })?;
        if telemetry.serving_epoch_started {
            return Err(telemetry_error(
                "serving dispatch telemetry epoch was already started; refusing to erase live counters",
            ));
        }
        let epoch_started_unix_ms = unix_ms()?;
        *telemetry = DispatchTelemetry::new(epoch_started_unix_ms);
        telemetry.serving_epoch_started = true;
        Ok(())
    }

    /// Reads and validates the complete process-local dispatch Source of Truth.
    pub fn dispatch_telemetry(&self) -> Result<CudaDispatchTelemetrySnapshot> {
        let telemetry = self.dispatch_telemetry.lock().map_err(|_| {
            telemetry_error("dispatch telemetry mutex is poisoned during health readback")
        })?;
        let sampled_at_unix_ms = unix_ms()?;
        let mut operations = Vec::with_capacity(DISPATCH_OPERATIONS.len());
        for (operation, state) in DISPATCH_OPERATIONS.iter().zip(&telemetry.operations) {
            validate_operation_telemetry(operation, state)?;
            operations.push(CudaDispatchOperationSnapshot {
                operation: (*operation).to_owned(),
                attempted_total: state.attempted_total,
                in_flight: state.in_flight,
                succeeded_total: state.succeeded_total,
                refused_total: state.refused_total,
                failed_total: state.failed_total,
                attempted_measured_bytes_total: state.attempted_measured_bytes_total,
                in_flight_measured_bytes: state.in_flight_measured_bytes,
                succeeded_measured_bytes_total: state.succeeded_measured_bytes_total,
                refused_measured_bytes_total: state.refused_measured_bytes_total,
                failed_measured_bytes_total: state.failed_measured_bytes_total,
                last_attempt_unix_ms: state.last_attempt_unix_ms,
                last_success_unix_ms: state.last_success_unix_ms,
                last_error_unix_ms: state.last_error_unix_ms,
                last_error_code: state.last_error_code.clone(),
            });
        }
        Ok(CudaDispatchTelemetrySnapshot {
            epoch_started_unix_ms: telemetry.epoch_started_unix_ms,
            sampled_at_unix_ms,
            operations,
        })
    }

    /// Shares this proved backend's exact CUDA context and module/function
    /// caches with nested strict Calyx Assay entry points for one synchronous
    /// caller-owned operation.
    ///
    /// This is a context-lifetime scope, not a device-buffer reservation. Each
    /// nested Assay kernel retains its own fail-closed live-VRAM check. The
    /// outer Synapse math lease remains alive for the entire call and owns the
    /// context/module host reservation; no process-global context survives it.
    pub fn with_assay_context_scope<T>(
        &self,
        operation: &'static str,
        dispatch: impl FnOnce() -> T,
    ) -> Result<T> {
        crate::cuda::context::with_cuda_context_scope(self.inner.context(), operation, dispatch)
    }

    /// Uploads one immutable candidate matrix and retains both its exact
    /// process-local VRAM reservation and, when configured, its host-wide
    /// reservation for the handle's lifetime.
    pub fn upload_candidate_block(
        &self,
        block_id: BlockId,
        candidates: &[f32],
        dim: usize,
    ) -> Result<BudgetedDeviceCandidateBlock<'_, CudaVramProbe>> {
        let measured_bytes = self.measured_f32_bytes("resident_dataset", &[candidates.len()])?;
        self.budgeter.can_allocate(measured_bytes)?;
        let host_reservation = self.acquire_host_dispatch("resident_dataset", measured_bytes)?;
        crate::cuda::upload_candidate_block_budgeted(
            self.inner.context(),
            &self.budgeter,
            block_id,
            candidates,
            dim,
        )
        .map(|block| block.attach_host_reservation(host_reservation))
    }

    /// Scores a row-major list of candidate indices against an immutable
    /// device-resident matrix. Only queries, indices, and output scores cross
    /// the host/device boundary for each call.
    pub fn l2_gather(
        &self,
        queries: &[f32],
        query_count: usize,
        block: &BudgetedDeviceCandidateBlock<'_, CudaVramProbe>,
        indices: &[u32],
        stride: usize,
        out: &mut [f32],
    ) -> Result<ResidentGatherReadback> {
        let measured_bytes = self.measured_l2_gather_bytes(queries, indices, out)?;
        self.run_reserved("l2_gather", measured_bytes, |inner| {
            crate::cuda::l2_gather_resident_host(
                inner.context(),
                queries,
                query_count,
                block.block(),
                indices,
                stride,
                out,
            )
        })
    }

    /// Recomputes caller-selected decision-boundary cells with Forge's
    /// canonical CPU reduction. This is the mandatory topology-changing
    /// near-tie contract for raw parallel-f32 gather scores.
    pub fn reverify_l2_gather_cpu(
        &self,
        request: CpuGatherReverificationRequest<'_>,
        scores: &mut [f32],
    ) -> Result<CpuGatherReverificationReadback> {
        crate::cuda::reverify_l2_gather_cpu(request, scores)
    }

    /// Runs Gaussian MMD through the same process-local and host-wide
    /// admission contract as every [`Backend`] dispatch.
    ///
    /// The exact byte shape is shared with the concrete kernel allocator. No
    /// unbudgeted raw CUDA context is created by this path.
    pub fn gaussian_mmd(
        &self,
        pooled: &[f64],
        n_a: usize,
        n_b: usize,
        dimension: usize,
        bandwidth: f64,
        permutations: &[i32],
    ) -> Result<CudaMmdResult> {
        let sample_count = n_a.checked_add(n_b).ok_or_else(|| {
            budget_error(format!(
                "gaussian_mmd sample count overflow: n_a={n_a} n_b={n_b}"
            ))
        })?;
        if sample_count == 0 || dimension == 0 {
            return Err(budget_error(format!(
                "gaussian_mmd requires non-zero sample_count and dimension: sample_count={sample_count} dimension={dimension}"
            )));
        }
        if !permutations.len().is_multiple_of(sample_count) {
            return Err(budget_error(format!(
                "gaussian_mmd permutation arena length {} is not divisible by sample_count={sample_count}",
                permutations.len()
            )));
        }
        let permutation_count = permutations.len() / sample_count;
        let measured_bytes =
            crate::gaussian_mmd_device_bytes(pooled.len(), sample_count, permutation_count)?;
        self.run_reserved("gaussian_mmd", measured_bytes, |inner| {
            crate::gaussian_mmd_host(
                inner.context(),
                pooled,
                n_a,
                n_b,
                dimension,
                bandwidth,
                permutations,
            )
        })
    }

    fn record_dispatch_attempt(
        &self,
        operation: &'static str,
        measured_bytes: u64,
        at_unix_ms: u64,
    ) -> Result<()> {
        let mut telemetry = self.dispatch_telemetry.lock().map_err(|_| {
            telemetry_error("dispatch telemetry mutex is poisoned while recording an attempt")
        })?;
        let state = operation_state_mut(&mut telemetry, operation)?;
        let attempted_total =
            checked_counter_add(operation, "attempted_total", state.attempted_total, 1)?;
        let attempted_measured_bytes_total = checked_counter_add(
            operation,
            "attempted_measured_bytes_total",
            state.attempted_measured_bytes_total,
            measured_bytes,
        )?;
        let in_flight = checked_counter_add(operation, "in_flight", state.in_flight, 1)?;
        let in_flight_measured_bytes = checked_counter_add(
            operation,
            "in_flight_measured_bytes",
            state.in_flight_measured_bytes,
            measured_bytes,
        )?;
        state.attempted_total = attempted_total;
        state.attempted_measured_bytes_total = attempted_measured_bytes_total;
        state.in_flight = in_flight;
        state.in_flight_measured_bytes = in_flight_measured_bytes;
        state.last_attempt_unix_ms = Some(
            state
                .last_attempt_unix_ms
                .map_or(at_unix_ms, |current| current.max(at_unix_ms)),
        );
        Ok(())
    }

    fn record_dispatch_outcome(
        &self,
        operation: &'static str,
        measured_bytes: u64,
        at_unix_ms: u64,
        outcome: DispatchOutcome<'_>,
    ) -> Result<()> {
        let mut telemetry = self.dispatch_telemetry.lock().map_err(|_| {
            telemetry_error("dispatch telemetry mutex is poisoned while recording an outcome")
        })?;
        let state = operation_state_mut(&mut telemetry, operation)?;
        let in_flight = state.in_flight.checked_sub(1).ok_or_else(|| {
            telemetry_error(format!(
                "{operation} dispatch outcome has no matching in-flight attempt"
            ))
        })?;
        let in_flight_measured_bytes = state
            .in_flight_measured_bytes
            .checked_sub(measured_bytes)
            .ok_or_else(|| {
            telemetry_error(format!(
                "{operation} dispatch outcome bytes {measured_bytes} exceed in-flight bytes {}",
                state.in_flight_measured_bytes
            ))
        })?;
        match outcome {
            DispatchOutcome::Succeeded => {
                let total =
                    checked_counter_add(operation, "succeeded_total", state.succeeded_total, 1)?;
                let bytes = checked_counter_add(
                    operation,
                    "succeeded_measured_bytes_total",
                    state.succeeded_measured_bytes_total,
                    measured_bytes,
                )?;
                state.succeeded_total = total;
                state.succeeded_measured_bytes_total = bytes;
                state.in_flight = in_flight;
                state.in_flight_measured_bytes = in_flight_measured_bytes;
                state.last_success_unix_ms = Some(
                    state
                        .last_success_unix_ms
                        .map_or(at_unix_ms, |current| current.max(at_unix_ms)),
                );
            }
            DispatchOutcome::Refused(error) => {
                let total =
                    checked_counter_add(operation, "refused_total", state.refused_total, 1)?;
                let bytes = checked_counter_add(
                    operation,
                    "refused_measured_bytes_total",
                    state.refused_measured_bytes_total,
                    measured_bytes,
                )?;
                state.refused_total = total;
                state.refused_measured_bytes_total = bytes;
                state.in_flight = in_flight;
                state.in_flight_measured_bytes = in_flight_measured_bytes;
                if state
                    .last_error_unix_ms
                    .is_none_or(|current| at_unix_ms >= current)
                {
                    state.last_error_unix_ms = Some(at_unix_ms);
                    state.last_error_code = Some(error.code().to_owned());
                }
            }
            DispatchOutcome::Failed(error) => {
                let total = checked_counter_add(operation, "failed_total", state.failed_total, 1)?;
                let bytes = checked_counter_add(
                    operation,
                    "failed_measured_bytes_total",
                    state.failed_measured_bytes_total,
                    measured_bytes,
                )?;
                state.failed_total = total;
                state.failed_measured_bytes_total = bytes;
                state.in_flight = in_flight;
                state.in_flight_measured_bytes = in_flight_measured_bytes;
                if state
                    .last_error_unix_ms
                    .is_none_or(|current| at_unix_ms >= current)
                {
                    state.last_error_unix_ms = Some(at_unix_ms);
                    state.last_error_code = Some(error.code().to_owned());
                }
            }
        }
        Ok(())
    }

    fn measured_f32_bytes(
        &self,
        operation: &'static str,
        element_counts: &[usize],
    ) -> Result<usize> {
        let elements = checked_sum(operation, element_counts)?;
        elements.checked_mul(F32_BYTES).ok_or_else(|| {
            budget_error(format!(
                "{operation} f32 device-buffer byte count overflow: elements={elements}"
            ))
        })
    }

    fn measured_l2_gather_bytes(
        &self,
        queries: &[f32],
        indices: &[u32],
        out: &[f32],
    ) -> Result<usize> {
        let f32_bytes = self.measured_f32_bytes("l2_gather", &[queries.len(), out.len()])?;
        let index_bytes = indices.len().checked_mul(size_of::<u32>()).ok_or_else(|| {
            budget_error(format!(
                "l2_gather u32 index byte count overflow: elements={}",
                indices.len()
            ))
        })?;
        f32_bytes.checked_add(index_bytes).ok_or_else(|| {
            budget_error("l2_gather aggregate device-buffer byte count overflow".to_owned())
        })
    }

    fn measured_topk_bytes(
        &self,
        operation: &'static str,
        score_count: usize,
        k: usize,
        already_device_resident_scores: bool,
    ) -> Result<usize> {
        let k_eff = k.min(score_count);
        // The device merge ladder (#2107 H1) holds two levels at once while it
        // collapses; `topk_device_entry_peak` walks the same loop the kernel
        // driver walks, so the reserved shape is the allocated shape.
        let output_entries = crate::cuda::topk::topk_device_entry_peak(1, score_count, k_eff);
        if output_entries == 0 && k_eff > 0 && score_count > 0 {
            return Err(budget_error(format!(
                "{operation} top-k temporary shape overflow: scores={score_count} k={k_eff}"
            )));
        }
        let score_bytes = if already_device_resident_scores {
            0
        } else {
            score_count.checked_mul(F32_BYTES).ok_or_else(|| {
                budget_error(format!(
                    "{operation} score byte count overflow: scores={score_count}"
                ))
            })?
        };
        let output_bytes = output_entries
            .checked_mul(F32_BYTES + I32_BYTES)
            .ok_or_else(|| {
                budget_error(format!(
                    "{operation} top-k output byte count overflow: entries={output_entries}"
                ))
            })?;
        score_bytes.checked_add(output_bytes).ok_or_else(|| {
            budget_error(format!(
                "{operation} aggregate device-buffer byte count overflow"
            ))
        })
    }

    /// Device-buffer bytes one batched kNN dispatch holds at its peak.
    ///
    /// After #2107 the CUDA path uploads the whole query matrix once, and each
    /// launch materializes a `tile x candidate_count` score matrix plus the
    /// device top-k merge ladder — for *every* metric, including `L2Squared`,
    /// which used to rank on the host. The tile is computed with the same
    /// [`crate::cuda::knn_query_tile`] the dispatch uses, so this is a
    /// measurement rather than an estimate.
    fn measured_knn_bytes(
        &self,
        queries: &[f32],
        candidates: &[f32],
        dim: usize,
        k: usize,
        _metric: KnnMetric,
    ) -> Result<usize> {
        if queries.is_empty() || candidates.is_empty() || dim == 0 || k == 0 {
            return Ok(0);
        }
        let candidate_count = candidates.len() / dim;
        let query_count = queries.len() / dim.max(1);
        let k_eff = k.min(candidate_count);
        let tile = crate::cuda::knn_query_tile(candidate_count, query_count);
        let score_elements = tile.checked_mul(candidate_count).ok_or_else(|| {
            budget_error(format!(
                "knn score matrix shape overflow: tile={tile} candidates={candidate_count}"
            ))
        })?;
        let topk_entries = crate::cuda::topk::topk_device_entry_peak(tile, candidate_count, k_eff);
        let resident_f32 = checked_sum(
            "knn",
            &[
                candidates.len(),
                queries.len(),
                score_elements,
                topk_entries,
            ],
        )?;
        let f32_bytes = resident_f32.checked_mul(F32_BYTES).ok_or_else(|| {
            budget_error(format!(
                "knn f32 device-buffer byte count overflow: elements={resident_f32}"
            ))
        })?;
        let index_bytes = topk_entries.checked_mul(I32_BYTES).ok_or_else(|| {
            budget_error(format!(
                "knn index device-buffer byte count overflow: entries={topk_entries}"
            ))
        })?;
        f32_bytes.checked_add(index_bytes).ok_or_else(|| {
            budget_error("knn aggregate device-buffer byte count overflow".to_owned())
        })
    }

    fn acquire_host_dispatch(
        &self,
        operation: &'static str,
        measured_bytes: usize,
    ) -> Result<Option<HostGpuReservation>> {
        if measured_bytes == 0 {
            return Ok(None);
        }
        let Some(host) = self.host_dispatch.as_ref() else {
            return Ok(None);
        };
        let dispatch_id = host.next_dispatch_id.fetch_add(1, Ordering::Relaxed);
        let requested_mib = u64::try_from(measured_bytes.div_ceil(BYTES_PER_MIB))
            .map_err(|_| budget_error(format!("{operation} measured MiB does not fit u64")))?;
        host.store
            .acquire(HostGpuReservationRequest::new(
                host.owner.clone(),
                format!("{}-{dispatch_id}", host.job_id_prefix),
                format!(
                    "forge operation={operation} measured_device_buffer_bytes={measured_bytes}"
                ),
                requested_mib,
            ))
            .map(Some)
    }

    fn run_reserved<T>(
        &self,
        operation: &'static str,
        measured_bytes: usize,
        dispatch: impl FnOnce(&CudaBackend) -> Result<T>,
    ) -> Result<T> {
        let observed = measured_bytes > 0;
        let measured_bytes_u64 = u64::try_from(measured_bytes).map_err(|_| {
            telemetry_error(format!(
                "{operation} measured dispatch bytes {measured_bytes} do not fit u64"
            ))
        })?;
        let attempted_at = if observed {
            let at = unix_ms()?;
            self.record_dispatch_attempt(operation, measured_bytes_u64, at)?;
            Some(at)
        } else {
            None
        };
        let _process_reservation = match self.budgeter.reserve(measured_bytes) {
            Ok(reservation) => reservation,
            Err(error) => {
                if let Some(at) = attempted_at {
                    self.record_dispatch_outcome(
                        operation,
                        measured_bytes_u64,
                        at,
                        DispatchOutcome::Refused(&error),
                    )?;
                }
                tracing::warn!(
                    target: "calyx_forge::vram::budgeted_backend",
                    code = "CALYX_FORGE_DISPATCH_REFUSED",
                    operation,
                    measured_bytes,
                    source_code = error.code(),
                    source_error = %error,
                    "process-local CUDA dispatch admission refused"
                );
                return Err(error);
            }
        };
        let host_reservation = match self.acquire_host_dispatch(operation, measured_bytes) {
            Ok(reservation) => reservation,
            Err(error) => {
                if let Some(at) = attempted_at {
                    self.record_dispatch_outcome(
                        operation,
                        measured_bytes_u64,
                        at,
                        DispatchOutcome::Refused(&error),
                    )?;
                }
                tracing::warn!(
                    target: "calyx_forge::vram::budgeted_backend",
                    code = "CALYX_FORGE_DISPATCH_REFUSED",
                    operation,
                    measured_bytes,
                    source_code = error.code(),
                    source_error = %error,
                    "host-wide CUDA dispatch admission refused"
                );
                return Err(error);
            }
        };
        let operation_result = dispatch(&self.inner);
        let release_result = host_reservation
            .map(HostGpuReservation::release)
            .transpose();
        let final_result = match (operation_result, release_result) {
            (Ok(value), Ok(_readback)) => Ok(value),
            (Err(operation_error), Ok(_readback)) => Err(operation_error),
            (Ok(_value), Err(release_error)) => Err(release_error),
            (Err(operation_error), Err(release_error)) => {
                tracing::error!(
                    target: "calyx_forge::vram::budgeted_backend",
                    operation,
                    operation_code = operation_error.code(),
                    operation_error = %operation_error,
                    release_code = release_error.code(),
                    release_error = %release_error,
                    "CUDA dispatch failed and its exact host reservation also failed to release"
                );
                Err(release_error)
            }
        };
        if observed {
            let completed_at = match unix_ms() {
                Ok(at) => at,
                Err(clock_error) => {
                    self.record_dispatch_outcome(
                        operation,
                        measured_bytes_u64,
                        attempted_at.expect("observed dispatch has an attempt timestamp"),
                        DispatchOutcome::Failed(&clock_error),
                    )?;
                    return Err(clock_error);
                }
            };
            match &final_result {
                Ok(_) => self.record_dispatch_outcome(
                    operation,
                    measured_bytes_u64,
                    completed_at,
                    DispatchOutcome::Succeeded,
                )?,
                Err(error) => {
                    self.record_dispatch_outcome(
                        operation,
                        measured_bytes_u64,
                        completed_at,
                        DispatchOutcome::Failed(error),
                    )?;
                    tracing::error!(
                        target: "calyx_forge::vram::budgeted_backend",
                        code = "CALYX_FORGE_DISPATCH_FAILED",
                        operation,
                        measured_bytes,
                        source_code = error.code(),
                        source_error = %error,
                        "CUDA operation or mandatory host-reservation release failed"
                    );
                }
            }
        }
        final_result
    }
}

impl Backend for VramBudgetedCudaBackend {
    fn gemm(
        &self,
        a: &[f32],
        b: &[f32],
        m: usize,
        k: usize,
        n: usize,
        out: &mut [f32],
    ) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("gemm", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("gemm", measured_bytes, |inner| {
            inner.gemm(a, b, m, k, n, out)
        })
    }

    fn cosine(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("cosine", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("cosine", measured_bytes, |inner| {
            inner.cosine(a, b, dim, out)
        })
    }

    fn dot(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("dot", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("dot", measured_bytes, |inner| inner.dot(a, b, dim, out))
    }

    fn l2(&self, a: &[f32], b: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("l2", &[a.len(), b.len(), out.len()])?;
        self.run_reserved("l2", measured_bytes, |inner| inner.l2(a, b, dim, out))
    }

    fn normalize(&self, vecs: &mut [f32], dim: usize) -> Result<()> {
        let measured_bytes = self.measured_f32_bytes("normalize", &[vecs.len()])?;
        self.run_reserved("normalize", measured_bytes, |inner| {
            inner.normalize(vecs, dim)
        })
    }

    fn topk(&self, scores: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
        let measured_bytes = self.measured_topk_bytes("topk", scores.len(), k, false)?;
        self.run_reserved("topk", measured_bytes, |inner| inner.topk(scores, k))
    }

    fn knn(
        &self,
        queries: &[f32],
        candidates: &[f32],
        query_count: usize,
        dim: usize,
        k: usize,
        metric: KnnMetric,
    ) -> Result<KnnBatch> {
        let measured_bytes = self.measured_knn_bytes(queries, candidates, dim, k, metric)?;
        self.run_reserved("knn", measured_bytes, |inner| {
            inner.knn(queries, candidates, query_count, dim, k, metric)
        })
    }

    fn paired_cosine(
        &self,
        left: &[f32],
        right: &[f32],
        pair_count: usize,
        dim: usize,
        out: &mut [f32],
    ) -> Result<()> {
        let measured_bytes =
            self.measured_f32_bytes("paired_cosine", &[left.len(), right.len(), out.len()])?;
        self.run_reserved("paired_cosine", measured_bytes, |inner| {
            inner.paired_cosine(left, right, pair_count, dim, out)
        })
    }

    fn device_info(&self) -> DeviceInfo {
        self.inner.device_info()
    }
}

fn checked_sum(operation: &'static str, values: &[usize]) -> Result<usize> {
    values.iter().copied().try_fold(0_usize, |sum, value| {
        sum.checked_add(value).ok_or_else(|| {
            budget_error(format!(
                "{operation} device-buffer element count overflow: accumulated={sum} next={value}"
            ))
        })
    })
}

fn operation_state_mut<'a>(
    telemetry: &'a mut DispatchTelemetry,
    operation: &'static str,
) -> Result<&'a mut DispatchOperationState> {
    let index = DISPATCH_OPERATIONS
        .iter()
        .position(|candidate| *candidate == operation)
        .ok_or_else(|| telemetry_error(format!("unknown Forge dispatch operation {operation}")))?;
    Ok(&mut telemetry.operations[index])
}

fn checked_counter_add(operation: &str, field: &str, current: u64, delta: u64) -> Result<u64> {
    current.checked_add(delta).ok_or_else(|| {
        telemetry_error(format!(
            "{operation} dispatch telemetry {field} overflow: current={current} delta={delta}"
        ))
    })
}

fn validate_operation_telemetry(operation: &str, state: &DispatchOperationState) -> Result<()> {
    let outcomes = state
        .succeeded_total
        .checked_add(state.refused_total)
        .and_then(|total| total.checked_add(state.failed_total))
        .ok_or_else(|| telemetry_error(format!("{operation} outcome counter sum overflow")))?;
    let terminal_or_in_flight = outcomes
        .checked_add(state.in_flight)
        .ok_or_else(|| telemetry_error(format!("{operation} in-flight counter sum overflow")))?;
    let outcome_bytes = state
        .succeeded_measured_bytes_total
        .checked_add(state.refused_measured_bytes_total)
        .and_then(|total| total.checked_add(state.failed_measured_bytes_total))
        .ok_or_else(|| telemetry_error(format!("{operation} outcome byte sum overflow")))?;
    let terminal_or_in_flight_bytes = outcome_bytes
        .checked_add(state.in_flight_measured_bytes)
        .ok_or_else(|| telemetry_error(format!("{operation} in-flight byte sum overflow")))?;
    if terminal_or_in_flight != state.attempted_total
        || terminal_or_in_flight_bytes != state.attempted_measured_bytes_total
    {
        return Err(telemetry_error(format!(
            "{operation} dispatch telemetry invariant failed: attempts={} outcomes={outcomes} in_flight={} attempted_bytes={} outcome_bytes={outcome_bytes} in_flight_bytes={}",
            state.attempted_total,
            state.in_flight,
            state.attempted_measured_bytes_total,
            state.in_flight_measured_bytes
        )));
    }
    Ok(())
}

fn unix_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            telemetry_error(format!(
                "system clock is before Unix epoch while recording dispatch telemetry: {error}"
            ))
        })?;
    u64::try_from(elapsed.as_millis()).map_err(|_| {
        telemetry_error("dispatch telemetry Unix timestamp does not fit u64 milliseconds")
    })
}

fn telemetry_error(detail: impl Into<String>) -> ForgeError {
    ForgeError::NumericalInvariant {
        op: "dispatch_telemetry".to_owned(),
        detail: detail.into(),
        remediation: "repair the process clock or Forge dispatch telemetry invariant; health and subsequent dispatches never substitute guessed counters".to_owned(),
    }
}

fn budget_error(detail: String) -> ForgeError {
    ForgeError::VramBudget {
        detail,
        remediation: DISPATCH_REMEDIATION.to_owned(),
    }
}
