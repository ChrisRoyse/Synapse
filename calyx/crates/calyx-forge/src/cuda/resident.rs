use cudarc::driver::CudaSlice;

use serde::{Deserialize, Serialize};

use crate::cpu::{self, check_finite, check_shape_2d};
use crate::cuda::distance::{
    L2GatherLaunch, launch_cosine_batch_gpu, launch_l2_gather_gpu, read_checked_device_output,
};
use crate::{
    BlockId, CudaContext, ForgeError, HostGpuReservation, Result, VramBudgeter, VramGuard,
    VramProbe,
};

const RESIDENT_REMEDIATION: &str =
    "Upload finite candidate blocks once, then score resident candidates with finite queries";
pub const L2_GATHER_NUMERIC_CONTRACT: &str =
    "gpu_f32_parallel_reduction_requires_cpu_reverification_for_topology_changing_near_ties";

pub struct DeviceCandidateBlock {
    block_id: BlockId,
    dim: usize,
    n_cands: usize,
    values: CudaSlice<f32>,
}

/// A device-resident candidate matrix whose lifetime owns the corresponding
/// Forge serving-budget reservation. Field order is intentional: the CUDA
/// allocation is dropped before its reservation is returned.
pub struct BudgetedDeviceCandidateBlock<'b, P: VramProbe> {
    block: DeviceCandidateBlock,
    _host_reservation: Option<HostGpuReservation>,
    _reservation: VramGuard<'b, P>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResidentGatherReadback {
    pub block_id: u64,
    pub dataset_rows: usize,
    pub dim: usize,
    pub query_count: usize,
    pub stride: usize,
    pub scores: usize,
    pub numeric_contract: &'static str,
    pub topology_exact: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CpuGatherReverificationReadback {
    pub output_cells_reverified: usize,
    pub numeric_contract: &'static str,
    pub topology_exact_for_reverified_cells: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CpuGatherReverificationRequest<'a> {
    pub queries: &'a [f32],
    pub query_count: usize,
    pub candidates: &'a [f32],
    pub dim: usize,
    pub indices: &'a [u32],
    pub stride: usize,
    pub output_offsets: &'a [u32],
}

impl DeviceCandidateBlock {
    pub fn block_id(&self) -> BlockId {
        self.block_id
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn n_cands(&self) -> usize {
        self.n_cands
    }
}

impl<P: VramProbe> BudgetedDeviceCandidateBlock<'_, P> {
    pub fn block(&self) -> &DeviceCandidateBlock {
        &self.block
    }

    pub fn reserved_bytes(&self) -> usize {
        self._reservation.bytes()
    }

    pub fn host_reservation_id(&self) -> Option<&str> {
        self._host_reservation
            .as_ref()
            .map(HostGpuReservation::reservation_id)
    }

    pub(crate) fn attach_host_reservation(
        mut self,
        reservation: Option<HostGpuReservation>,
    ) -> Self {
        self._host_reservation = reservation;
        self
    }
}

pub fn upload_candidate_block(
    ctx: &CudaContext,
    block_id: BlockId,
    candidates: &[f32],
    dim: usize,
) -> Result<DeviceCandidateBlock> {
    let n_cands = validate_candidates(candidates, dim)?;
    let values = ctx
        .inner()
        .default_stream()
        .clone_htod(candidates)
        .map_err(|err| {
            device_unavailable(ctx, format!("resident candidate upload failed: {err}"))
        })?;
    Ok(DeviceCandidateBlock {
        block_id,
        dim,
        n_cands,
        values,
    })
}

pub fn upload_candidate_block_budgeted<'b, P: VramProbe>(
    ctx: &CudaContext,
    budgeter: &'b VramBudgeter<P>,
    block_id: BlockId,
    candidates: &[f32],
    dim: usize,
) -> Result<BudgetedDeviceCandidateBlock<'b, P>> {
    let bytes = candidates
        .len()
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| ForgeError::ShapeMismatch {
            expected: vec![usize::MAX / std::mem::size_of::<f32>()],
            got: vec![candidates.len()],
            remediation: "resident candidate byte count overflows usize".to_string(),
        })?;
    let reservation = budgeter.reserve(bytes)?;
    let block = upload_candidate_block(ctx, block_id, candidates, dim)?;
    Ok(BudgetedDeviceCandidateBlock {
        block,
        _host_reservation: None,
        _reservation: reservation,
    })
}

pub fn cosine_resident_host(
    ctx: &CudaContext,
    query: &[f32],
    block: &DeviceCandidateBlock,
    out: &mut [f32],
) -> Result<()> {
    validate_query(query, block.dim)?;
    check_shape_2d(out, block.n_cands, 1, "cuda resident cosine output")?;
    out.fill(0.0);
    if block.n_cands == 0 {
        return Ok(());
    }
    let stream = ctx.inner().default_stream();
    let query_dev = stream.clone_htod(query).map_err(|err| {
        device_unavailable(ctx, format!("resident cosine query upload failed: {err}"))
    })?;
    let mut out_dev = stream.alloc_zeros(out.len()).map_err(|err| {
        device_unavailable(
            ctx,
            format!("resident cosine output allocation failed: {err}"),
        )
    })?;
    launch_cosine_batch_gpu(
        ctx,
        &query_dev,
        &block.values,
        block.dim,
        block.n_cands,
        &mut out_dev,
    )?;
    let values = read_checked_device_output(ctx, "cosine_batch_gpu", &out_dev, true)?;
    out.copy_from_slice(&values);
    Ok(())
}

/// Scores selected rows of a device-resident dataset without gathering or
/// re-uploading candidate vectors on the host.
///
/// The returned scores use the CUDA kernel's parallel f32 reduction order.
/// They are suitable for approximate query-time ranking. A caller that may
/// change persisted graph topology MUST identify decision-boundary/near-tie
/// output cells and pass those offsets to [`reverify_l2_gather_cpu`] before
/// committing the decision. The explicit readback never labels raw GPU scores
/// topology-exact.
pub fn l2_gather_resident_host(
    ctx: &CudaContext,
    queries: &[f32],
    query_count: usize,
    block: &DeviceCandidateBlock,
    indices: &[u32],
    stride: usize,
    out: &mut [f32],
) -> Result<ResidentGatherReadback> {
    validate_gather_inputs(queries, query_count, block, indices, stride, out)?;
    out.fill(0.0);
    if query_count == 0 || stride == 0 {
        return Ok(gather_readback(block, query_count, stride));
    }
    let stream = ctx.inner().default_stream();
    let queries_dev = stream.clone_htod(queries).map_err(|err| {
        device_unavailable(
            ctx,
            format!("resident L2 gather query upload failed: {err}"),
        )
    })?;
    let indices_dev = stream.clone_htod(indices).map_err(|err| {
        device_unavailable(
            ctx,
            format!("resident L2 gather index upload failed: {err}"),
        )
    })?;
    let mut out_dev = stream.alloc_zeros(out.len()).map_err(|err| {
        device_unavailable(
            ctx,
            format!("resident L2 gather output allocation failed: {err}"),
        )
    })?;
    launch_l2_gather_gpu(
        ctx,
        L2GatherLaunch {
            queries: &queries_dev,
            candidates: &block.values,
            indices: &indices_dev,
            dim: block.dim,
            n_cands: block.n_cands,
            query_count,
            stride,
            out: &mut out_dev,
        },
    )?;
    let scores = read_checked_device_output(ctx, "l2_gather_gpu", &out_dev, false)?;
    out.copy_from_slice(&scores);
    Ok(gather_readback(block, query_count, stride))
}

/// Recomputes caller-selected gather output cells with the canonical CPU
/// reduction and overwrites those cells in-place.
///
/// `output_offsets` indexes the flattened `query_count * stride` result. This
/// lets graph builders reverify only threshold/ordering near-ties while the
/// resident CUDA pass handles the abundant unambiguous scores.
pub fn reverify_l2_gather_cpu(
    request: CpuGatherReverificationRequest<'_>,
    scores: &mut [f32],
) -> Result<CpuGatherReverificationReadback> {
    let CpuGatherReverificationRequest {
        queries,
        query_count,
        candidates,
        dim,
        indices,
        stride,
        output_offsets,
    } = request;
    validate_host_gather_inputs(
        queries,
        query_count,
        candidates,
        dim,
        indices,
        stride,
        scores,
    )?;
    let candidate_rows = candidates.len() / dim;
    for (position, output_offset) in output_offsets.iter().copied().enumerate() {
        let output_offset = usize::try_from(output_offset)
            .map_err(|_| resident_shape_error("CPU reverify output offset does not fit usize"))?;
        if output_offset >= scores.len() {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![scores.len()],
                got: vec![output_offset],
                remediation: format!(
                    "CPU reverify offset at position {position} must name a flattened gather output cell"
                ),
            });
        }
        let query_row = output_offset / stride;
        let candidate_row = usize::try_from(indices[output_offset])
            .map_err(|_| resident_shape_error("CPU reverify candidate index does not fit usize"))?;
        if candidate_row >= candidate_rows {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![candidate_rows],
                got: vec![candidate_row],
                remediation: format!(
                    "CPU reverify candidate index at output offset {output_offset} is out of range"
                ),
            });
        }
        let query_start = query_row * dim;
        let candidate_start = candidate_row * dim;
        let mut exact = [0.0_f32; 1];
        cpu::l2_batch(
            &queries[query_start..query_start + dim],
            &candidates[candidate_start..candidate_start + dim],
            dim,
            &mut exact,
        )?;
        scores[output_offset] = exact[0];
    }
    Ok(CpuGatherReverificationReadback {
        output_cells_reverified: output_offsets.len(),
        numeric_contract: "canonical_cpu_l2_reduction_for_named_output_cells",
        topology_exact_for_reverified_cells: true,
    })
}

fn gather_readback(
    block: &DeviceCandidateBlock,
    query_count: usize,
    stride: usize,
) -> ResidentGatherReadback {
    ResidentGatherReadback {
        block_id: block.block_id.0,
        dataset_rows: block.n_cands,
        dim: block.dim,
        query_count,
        stride,
        scores: query_count.saturating_mul(stride),
        numeric_contract: L2_GATHER_NUMERIC_CONTRACT,
        topology_exact: false,
    }
}

fn validate_gather_inputs(
    queries: &[f32],
    query_count: usize,
    block: &DeviceCandidateBlock,
    indices: &[u32],
    stride: usize,
    out: &[f32],
) -> Result<()> {
    check_shape_2d(
        queries,
        query_count,
        block.dim,
        "cuda resident gather queries",
    )?;
    check_finite(queries, "cuda resident gather queries")?;
    check_index_shape(
        indices.len(),
        query_count,
        stride,
        "cuda resident gather indices",
    )?;
    check_shape_2d(out, query_count, stride, "cuda resident gather output")?;
    for (offset, index) in indices.iter().copied().enumerate() {
        if usize::try_from(index).map_or(true, |index| index >= block.n_cands) {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![block.n_cands],
                got: vec![index as usize],
                remediation: format!(
                    "resident gather index at flattened offset {offset} must be less than the uploaded dataset row count"
                ),
            });
        }
    }
    Ok(())
}

fn validate_host_gather_inputs(
    queries: &[f32],
    query_count: usize,
    candidates: &[f32],
    dim: usize,
    indices: &[u32],
    stride: usize,
    scores: &[f32],
) -> Result<()> {
    let candidate_rows = validate_candidates(candidates, dim)?;
    check_shape_2d(queries, query_count, dim, "CPU reverify gather queries")?;
    check_finite(queries, "CPU reverify gather queries")?;
    check_index_shape(
        indices.len(),
        query_count,
        stride,
        "CPU reverify gather indices",
    )?;
    check_shape_2d(scores, query_count, stride, "CPU reverify gather scores")?;
    for (offset, index) in indices.iter().copied().enumerate() {
        if usize::try_from(index).map_or(true, |index| index >= candidate_rows) {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![candidate_rows],
                got: vec![index as usize],
                remediation: format!(
                    "CPU reverify gather index at flattened offset {offset} must be less than the candidate row count"
                ),
            });
        }
    }
    Ok(())
}

fn check_index_shape(len: usize, rows: usize, cols: usize, name: &str) -> Result<()> {
    let expected = rows
        .checked_mul(cols)
        .ok_or_else(|| ForgeError::ShapeMismatch {
            expected: vec![rows, cols],
            got: vec![len],
            remediation: format!("{name} rows*cols overflows usize"),
        })?;
    if len != expected {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![expected],
            got: vec![len],
            remediation: format!("{name} must contain exactly rows*cols entries"),
        });
    }
    Ok(())
}

fn validate_candidates(candidates: &[f32], dim: usize) -> Result<usize> {
    if dim == 0 {
        return Err(resident_shape_error(
            "candidate dim must be non-zero for resident cosine blocks",
        ));
    }
    if !candidates.len().is_multiple_of(dim) {
        return Err(resident_shape_error(
            "candidate length must be an integer number of rows",
        ));
    }
    let n_cands = candidates.len() / dim;
    check_shape_2d(candidates, n_cands, dim, "cuda resident candidates")?;
    check_finite(candidates, "cuda resident candidates")?;
    Ok(n_cands)
}

fn validate_query(query: &[f32], dim: usize) -> Result<()> {
    check_shape_2d(query, 1, dim, "cuda resident cosine query")?;
    check_finite(query, "cuda resident cosine query")
}

fn resident_shape_error(detail: &str) -> ForgeError {
    ForgeError::ShapeMismatch {
        expected: vec![1],
        got: vec![0],
        remediation: detail.to_string(),
    }
}

fn device_unavailable(ctx: &CudaContext, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: format!("cuda:{}", ctx.device_idx()),
        detail,
        remediation: RESIDENT_REMEDIATION.to_string(),
    }
}
