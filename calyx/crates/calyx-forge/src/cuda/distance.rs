use std::str;
use std::sync::Arc;

use cudarc::driver::{CudaModule, CudaSlice, CudaView, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;

use crate::cpu::{check_finite, check_shape_2d};
use crate::cuda::kernels::{DISTANCE_CUBIN, DISTANCE_PTX};
use crate::cuda::validate::{check_device_f32, read_checked_device_f32};
use crate::{CudaContext, ForgeError, Result};

const BLOCK_THREADS: u32 = 256;
/// Grid `y` carries the query row for the batched kNN launches; CUDA caps it
/// at 65535.
pub(crate) const DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH: usize = 65_535;
const DISTANCE_REMEDIATION: &str =
    "Check CUDA distance kernel inputs and fail closed instead of returning invalid scores";
pub(crate) const DISTANCE_INPUT_REMEDIATION: &str =
    "Ensure all input vectors are normalized finite f32; check upstream embedding model output";
const DEVICE_REMEDIATION: &str =
    "Check CUDA, embedded distance PTX, and CUDA GPU device availability";

pub fn cosine_batch_gpu(
    ctx: &CudaContext,
    query: &CudaSlice<f32>,
    candidates: &CudaSlice<f32>,
    dim: usize,
    n_cands: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    launch_distance(
        ctx,
        "cosine_batch_gpu",
        "cosine_batch_f32",
        query,
        candidates,
        dim,
        n_cands,
        out,
    )?;
    check_device_output(ctx, "cosine_batch_gpu", out, true)
}

pub(crate) fn launch_cosine_batch_gpu(
    ctx: &CudaContext,
    query: &CudaSlice<f32>,
    candidates: &CudaSlice<f32>,
    dim: usize,
    n_cands: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    launch_distance(
        ctx,
        "cosine_batch_gpu",
        "cosine_batch_f32",
        query,
        candidates,
        dim,
        n_cands,
        out,
    )
}

pub fn dot_batch_gpu(
    ctx: &CudaContext,
    query: &CudaSlice<f32>,
    candidates: &CudaSlice<f32>,
    dim: usize,
    n_cands: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    launch_distance(
        ctx,
        "dot_batch_gpu",
        "dot_batch_f32",
        query,
        candidates,
        dim,
        n_cands,
        out,
    )?;
    check_device_output(ctx, "dot_batch_gpu", out, false)
}

pub fn l2_batch_gpu(
    ctx: &CudaContext,
    query: &CudaSlice<f32>,
    candidates: &CudaSlice<f32>,
    dim: usize,
    n_cands: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    launch_distance(
        ctx,
        "l2_batch_gpu",
        "l2_batch_f32",
        query,
        candidates,
        dim,
        n_cands,
        out,
    )?;
    check_device_output(ctx, "l2_batch_gpu", out, false)
}

pub(crate) struct L2GatherLaunch<'a> {
    pub queries: &'a CudaSlice<f32>,
    pub candidates: &'a CudaSlice<f32>,
    pub indices: &'a CudaSlice<u32>,
    pub dim: usize,
    pub n_cands: usize,
    pub query_count: usize,
    pub stride: usize,
    pub out: &'a mut CudaSlice<f32>,
}

pub(crate) fn launch_l2_gather_gpu(ctx: &CudaContext, request: L2GatherLaunch<'_>) -> Result<()> {
    let L2GatherLaunch {
        queries,
        candidates,
        indices,
        dim,
        n_cands,
        query_count,
        stride,
        out,
    } = request;
    check_device_shape(queries.len(), query_count, dim, "cuda gather queries")?;
    check_device_shape(candidates.len(), n_cands, dim, "cuda gather candidates")?;
    check_device_shape(indices.len(), query_count, stride, "cuda gather indices")?;
    check_device_shape(out.len(), query_count, stride, "cuda gather output")?;
    if query_count == 0 || stride == 0 {
        return Ok(());
    }
    if query_count > DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH],
            got: vec![query_count],
            remediation: "split resident gather queries into batches of at most 65535 rows"
                .to_string(),
        });
    }
    let dim_i32 = to_i32(dim, "dim")?;
    let n_cands_i32 = to_i32(n_cands, "n_cands")?;
    let stride_i32 = to_i32(stride, "stride")?;
    let stride_u32 = u32::try_from(stride).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![u32::MAX as usize],
        got: vec![stride],
        remediation: "resident gather stride exceeds the CUDA grid x limit".to_string(),
    })?;
    let query_count_u32 = u32::try_from(query_count).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH],
        got: vec![query_count],
        remediation: "resident gather query count exceeds the CUDA grid y limit".to_string(),
    })?;
    let module = distance_module(ctx)?;
    let func = ctx
        .cached_function(&module, "distance.l2_gather_f32", "l2_gather_f32")
        .map_err(|err| {
            device_unavailable(
                ctx,
                format!("resident L2 gather load function failed: {err}"),
            )
        })?;
    let cfg = LaunchConfig {
        grid_dim: (stride_u32, query_count_u32, 1),
        block_dim: (BLOCK_THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let stream = ctx.inner().default_stream();
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(queries)
            .arg(candidates)
            .arg(indices)
            .arg(&dim_i32)
            .arg(&n_cands_i32)
            .arg(&stride_i32)
            .arg(out)
            .launch(cfg)
    }
    .map_err(|err| {
        device_unavailable(
            ctx,
            format!("resident L2 gather kernel launch failed: {err}"),
        )
    })?;
    Ok(())
}

pub fn paired_cosine_gpu(
    ctx: &CudaContext,
    left: &CudaSlice<f32>,
    right: &CudaSlice<f32>,
    dim: usize,
    pair_count: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    launch_paired_cosine(ctx, left, right, dim, pair_count, out)?;
    check_device_output(ctx, "paired_cosine_gpu", out, true)
}

pub fn cosine_host(
    ctx: &CudaContext,
    query: &[f32],
    candidates: &[f32],
    dim: usize,
    out: &mut [f32],
) -> Result<()> {
    distance_host(
        ctx,
        ("cosine_batch_gpu", "cosine_batch_f32", true),
        query,
        candidates,
        dim,
        out,
    )
}

pub fn dot_host(
    ctx: &CudaContext,
    query: &[f32],
    candidates: &[f32],
    dim: usize,
    out: &mut [f32],
) -> Result<()> {
    distance_host(
        ctx,
        ("dot_batch_gpu", "dot_batch_f32", false),
        query,
        candidates,
        dim,
        out,
    )
}

pub fn l2_host(
    ctx: &CudaContext,
    query: &[f32],
    candidates: &[f32],
    dim: usize,
    out: &mut [f32],
) -> Result<()> {
    distance_host(
        ctx,
        ("l2_batch_gpu", "l2_batch_f32", false),
        query,
        candidates,
        dim,
        out,
    )
}

pub fn paired_cosine_host(
    ctx: &CudaContext,
    left: &[f32],
    right: &[f32],
    pair_count: usize,
    dim: usize,
    out: &mut [f32],
) -> Result<()> {
    validate_paired_host_inputs(left, right, pair_count, dim, out)?;
    out.fill(0.0);
    if pair_count == 0 {
        return Ok(());
    }

    let stream = ctx.inner().default_stream();
    let left_dev = stream
        .clone_htod(left)
        .map_err(|err| device_unavailable(ctx, format!("paired cosine left copy failed: {err}")))?;
    let right_dev = stream.clone_htod(right).map_err(|err| {
        device_unavailable(ctx, format!("paired cosine right copy failed: {err}"))
    })?;
    let mut out_dev = stream.alloc_zeros(pair_count).map_err(|err| {
        device_unavailable(
            ctx,
            format!("paired cosine output allocation failed: {err}"),
        )
    })?;

    launch_paired_cosine(ctx, &left_dev, &right_dev, dim, pair_count, &mut out_dev)?;
    let result = read_checked_device_output(ctx, "paired_cosine_gpu", &out_dev, true)?;
    out.copy_from_slice(&result);
    Ok(())
}

pub fn normalize_rows_gpu(
    ctx: &CudaContext,
    vecs: &mut CudaSlice<f32>,
    rows: usize,
    dim: usize,
) -> Result<()> {
    check_device_shape(vecs.len(), rows, dim, "cuda normalize input")?;
    if rows == 0 {
        return Ok(());
    }
    launch_normalize(ctx, vecs, rows, dim)?;
    check_device_output(ctx, "normalize_rows_gpu", vecs, false)
}

pub fn normalize_host(ctx: &CudaContext, vecs: &mut [f32], dim: usize) -> Result<()> {
    let rows = validate_normalize_host_inputs(vecs, dim)?;
    if vecs.is_empty() {
        return Ok(());
    }

    let stream = ctx.inner().default_stream();
    let mut vecs_dev = stream
        .clone_htod(vecs)
        .map_err(|err| device_unavailable(ctx, format!("normalize input copy failed: {err}")))?;
    launch_normalize(ctx, &mut vecs_dev, rows, dim)?;
    let result = read_checked_device_output(ctx, "normalize_rows_gpu", &vecs_dev, false)?;
    vecs.copy_from_slice(&result);
    Ok(())
}

fn distance_host(
    ctx: &CudaContext,
    kernel: (&'static str, &'static str, bool),
    query: &[f32],
    candidates: &[f32],
    dim: usize,
    out: &mut [f32],
) -> Result<()> {
    let (op, kernel_name, sentinel) = kernel;
    validate_host_inputs(op, query, candidates, dim, out)?;
    out.fill(0.0);
    if out.is_empty() {
        return Ok(());
    }

    let stream = ctx.inner().default_stream();
    let query_dev = stream
        .clone_htod(query)
        .map_err(|err| device_unavailable(ctx, format!("{op} query copy failed: {err}")))?;
    let candidates_dev = stream
        .clone_htod(candidates)
        .map_err(|err| device_unavailable(ctx, format!("{op} candidates copy failed: {err}")))?;
    let mut out_dev = stream
        .alloc_zeros(out.len())
        .map_err(|err| device_unavailable(ctx, format!("{op} output allocation failed: {err}")))?;

    launch_distance(
        ctx,
        op,
        kernel_name,
        &query_dev,
        &candidates_dev,
        dim,
        out.len(),
        &mut out_dev,
    )?;
    let result = read_checked_device_output(ctx, op, &out_dev, sentinel)?;
    out.copy_from_slice(&result);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn launch_distance(
    ctx: &CudaContext,
    op: &'static str,
    kernel_name: &'static str,
    query: &CudaSlice<f32>,
    candidates: &CudaSlice<f32>,
    dim: usize,
    n_cands: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    launch_distance_batched(
        ctx,
        op,
        kernel_name,
        &query.as_view(),
        candidates,
        dim,
        n_cands,
        1,
        out,
    )
}

/// Launches one distance kernel for `n_queries` query rows against the shared
/// candidate matrix, writing a row-major `[n_queries, n_cands]` score matrix.
///
/// #2107 H4: the caller used to run this once per query, paying a separate
/// upload, allocation, launch and readback each time. `gridDim.y` now carries
/// the query row, and `n_queries == 1` reproduces the previous launch geometry
/// exactly, so single-query results are unchanged bit-for-bit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_distance_batched(
    ctx: &CudaContext,
    op: &'static str,
    kernel_name: &'static str,
    queries: &CudaView<'_, f32>,
    candidates: &CudaSlice<f32>,
    dim: usize,
    n_cands: usize,
    n_queries: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    check_device_shape(queries.len(), n_queries, dim, "cuda distance query")?;
    check_device_shape(candidates.len(), n_cands, dim, "cuda distance candidates")?;
    check_device_shape(out.len(), n_queries, n_cands, "cuda distance output")?;
    if n_cands == 0 || n_queries == 0 {
        return Ok(());
    }

    let dim_i32 = to_i32(dim, "dim")?;
    let n_cands_i32 = to_i32(n_cands, "n_cands")?;
    let n_cands_u32 = u32::try_from(n_cands).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![u32::MAX as usize],
        got: vec![n_cands],
        remediation: "cuda distance n_cands exceeds grid dimension limit".to_string(),
    })?;
    let n_queries_u32 = u32::try_from(n_queries)
        .ok()
        .filter(|_| n_queries <= DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH)
        .ok_or_else(|| ForgeError::ShapeMismatch {
            expected: vec![DISTANCE_MAX_QUERY_ROWS_PER_LAUNCH],
            got: vec![n_queries],
            remediation:
                "cuda distance query batch exceeds the CUDA grid.y limit; tile the query batch"
                    .to_string(),
        })?;
    let module = distance_module(ctx)?;
    let func = ctx
        .cached_function(&module, distance_cache_key(kernel_name), kernel_name)
        .map_err(|err| device_unavailable(ctx, format!("{op} load function failed: {err}")))?;
    let stream = ctx.inner().default_stream();
    let cfg = LaunchConfig {
        grid_dim: (n_cands_u32, n_queries_u32, 1),
        block_dim: (BLOCK_THREADS, 1, 1),
        shared_mem_bytes: 0,
    };

    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(queries)
            .arg(candidates)
            .arg(&dim_i32)
            .arg(&n_cands_i32)
            .arg(out)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("{op} kernel launch failed: {err}")))?;
    Ok(())
}

/// Device-side finiteness gate for a buffer that was just uploaded (#2107 H3).
///
/// The host scan this replaces walked the whole candidate matrix with a scalar
/// loop immediately before handing the same bytes to the GPU. The device
/// kernel reads the buffer at device bandwidth instead. The refusal is
/// identical: the same `ForgeError::NumericalInvariant` code, from the same
/// call, before any result is produced — and the exact index-bearing message is
/// still recovered, because on the refusal path (and only there) the host copy
/// the caller already holds is scanned to name the offending element.
pub(crate) fn check_uploaded_finite(
    ctx: &CudaContext,
    op: &'static str,
    host: &[f32],
    device: &CudaSlice<f32>,
) -> Result<()> {
    match check_device_f32(ctx, op, device, false, DISTANCE_INPUT_REMEDIATION) {
        Ok(()) => Ok(()),
        Err(ForgeError::NumericalInvariant { .. }) => {
            check_finite(host, op)?;
            Err(ForgeError::NumericalInvariant {
                op: op.to_string(),
                detail: format!(
                    "device finiteness gate refused a {} element buffer that the host scan found \
                     entirely finite: device and host disagree about the uploaded bytes",
                    host.len()
                ),
                remediation: "Re-run with CALYX_FORGE_AUDIT_OUTPUT=1 and check the host-to-device \
                              transfer and device memory integrity"
                    .to_string(),
            })
        }
        Err(other) => Err(other),
    }
}

fn launch_paired_cosine(
    ctx: &CudaContext,
    left: &CudaSlice<f32>,
    right: &CudaSlice<f32>,
    dim: usize,
    pair_count: usize,
    out: &mut CudaSlice<f32>,
) -> Result<()> {
    check_device_shape(left.len(), pair_count, dim, "cuda paired cosine left")?;
    check_device_shape(right.len(), pair_count, dim, "cuda paired cosine right")?;
    check_device_shape(out.len(), pair_count, 1, "cuda paired cosine output")?;
    if pair_count == 0 {
        return Ok(());
    }

    let dim_i32 = to_i32(dim, "dim")?;
    let pair_count_i32 = to_i32(pair_count, "pair_count")?;
    let pair_count_u32 = u32::try_from(pair_count).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![u32::MAX as usize],
        got: vec![pair_count],
        remediation: "cuda paired cosine pair_count exceeds grid dimension limit".to_string(),
    })?;
    let module = distance_module(ctx)?;
    let func = ctx
        .cached_function(&module, "distance.paired_cosine_f32", "paired_cosine_f32")
        .map_err(|err| {
            device_unavailable(ctx, format!("paired cosine load function failed: {err}"))
        })?;
    let stream = ctx.inner().default_stream();
    let cfg = LaunchConfig {
        grid_dim: (pair_count_u32, 1, 1),
        block_dim: (BLOCK_THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(left)
            .arg(right)
            .arg(&dim_i32)
            .arg(&pair_count_i32)
            .arg(out)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("paired cosine kernel launch failed: {err}")))?;
    Ok(())
}

fn launch_normalize(
    ctx: &CudaContext,
    vecs: &mut CudaSlice<f32>,
    rows: usize,
    dim: usize,
) -> Result<()> {
    let rows_i32 = to_i32(rows, "rows")?;
    let rows_u32 = u32::try_from(rows).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![u32::MAX as usize],
        got: vec![rows],
        remediation: "cuda normalize row count exceeds grid dimension limit".to_string(),
    })?;
    let dim_i32 = to_i32(dim, "dim")?;
    let module = distance_module(ctx)?;
    let func = ctx
        .cached_function(&module, "distance.normalize_rows_f32", "normalize_rows_f32")
        .map_err(|err| device_unavailable(ctx, format!("normalize load function failed: {err}")))?;
    let stream = ctx.inner().default_stream();
    let cfg = LaunchConfig {
        grid_dim: (rows_u32, 1, 1),
        block_dim: (BLOCK_THREADS, 1, 1),
        shared_mem_bytes: 0,
    };

    let mut launch = stream.launch_builder(func.as_ref());
    unsafe { launch.arg(vecs).arg(&dim_i32).arg(&rows_i32).launch(cfg) }
        .map_err(|err| device_unavailable(ctx, format!("normalize kernel launch failed: {err}")))?;
    Ok(())
}

pub(crate) fn distance_module(ctx: &CudaContext) -> Result<Arc<CudaModule>> {
    if let Some(module) = ctx.distance_module_cache().get() {
        return Ok(module.clone());
    }
    match ctx
        .inner()
        .load_module(Ptx::from_binary(DISTANCE_CUBIN.to_vec()))
    {
        Ok(module) => {
            let _ = ctx.distance_module_cache().set(module.clone());
            Ok(module)
        }
        Err(cubin_err) => {
            let module = distance_ptx_module(ctx, cubin_err)?;
            let _ = ctx.distance_module_cache().set(module.clone());
            Ok(module)
        }
    }
}

fn distance_cache_key(kernel_name: &'static str) -> &'static str {
    match kernel_name {
        "cosine_batch_f32" => "distance.cosine_batch_f32",
        "dot_batch_f32" => "distance.dot_batch_f32",
        "l2_batch_f32" => "distance.l2_batch_f32",
        "l2_gather_f32" => "distance.l2_gather_f32",
        "paired_cosine_f32" => "distance.paired_cosine_f32",
        _ => kernel_name,
    }
}

fn distance_ptx_module(
    ctx: &CudaContext,
    cubin_err: cudarc::driver::DriverError,
) -> Result<Arc<CudaModule>> {
    let ptx = str::from_utf8(DISTANCE_PTX)
        .map_err(|err| device_unavailable(ctx, format!("distance PTX is not UTF-8: {err}")))?;
    ctx.inner()
        .load_module(Ptx::from_src(ptx))
        .map_err(|ptx_err| {
            device_unavailable(
                ctx,
                format!(
                    "distance CUBIN load failed: {cubin_err}; PTX fallback load failed: {ptx_err}"
                ),
            )
        })
}

pub(crate) fn check_device_output(
    ctx: &CudaContext,
    op: &'static str,
    out: &CudaSlice<f32>,
    sentinel: bool,
) -> Result<()> {
    check_device_f32(ctx, op, out, sentinel, DISTANCE_REMEDIATION)
}

pub(crate) fn read_checked_device_output(
    ctx: &CudaContext,
    op: &'static str,
    out: &CudaSlice<f32>,
    sentinel: bool,
) -> Result<Vec<f32>> {
    read_checked_device_f32(ctx, op, out, sentinel, DISTANCE_REMEDIATION)
}

fn validate_host_inputs(
    op: &'static str,
    query: &[f32],
    candidates: &[f32],
    dim: usize,
    out: &[f32],
) -> Result<()> {
    check_shape_2d(query, 1, dim, "cuda distance query")?;
    check_shape_2d(candidates, out.len(), dim, "cuda distance candidates")?;
    check_finite(query, op)?;
    check_finite(candidates, op)?;
    Ok(())
}

fn validate_paired_host_inputs(
    left: &[f32],
    right: &[f32],
    pair_count: usize,
    dim: usize,
    out: &[f32],
) -> Result<()> {
    check_shape_2d(left, pair_count, dim, "cuda paired cosine left")?;
    check_shape_2d(right, pair_count, dim, "cuda paired cosine right")?;
    check_shape_2d(out, pair_count, 1, "cuda paired cosine output")?;
    check_finite(left, "paired_cosine_gpu")?;
    check_finite(right, "paired_cosine_gpu")?;
    Ok(())
}

fn validate_normalize_host_inputs(vecs: &[f32], dim: usize) -> Result<usize> {
    if dim == 0 {
        if vecs.is_empty() {
            return Ok(0);
        }
        return Err(ForgeError::ShapeMismatch {
            expected: vec![0],
            got: vec![vecs.len()],
            remediation: "dim=0 is valid only for an empty matrix".to_string(),
        });
    }
    if !vecs.len().is_multiple_of(dim) {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![dim],
            got: vec![vecs.len()],
            remediation: "normalize input length must be an integer number of rows".to_string(),
        });
    }
    let rows = vecs.len() / dim;
    check_shape_2d(vecs, rows, dim, "cuda normalize input")?;
    check_finite(vecs, "cuda normalize")?;
    Ok(rows)
}

fn check_device_shape(len: usize, rows: usize, cols: usize, name: &str) -> Result<()> {
    let expected_len = rows
        .checked_mul(cols)
        .ok_or_else(|| ForgeError::ShapeMismatch {
            expected: vec![rows, cols],
            got: vec![len],
            remediation: format!("{name} shape overflows usize"),
        })?;
    if len == expected_len {
        return Ok(());
    }
    Err(ForgeError::ShapeMismatch {
        expected: vec![rows, cols],
        got: vec![len],
        remediation: format!("{name} length does not match rows*cols"),
    })
}

fn to_i32(value: usize, name: &str) -> Result<i32> {
    i32::try_from(value).map_err(|_| ForgeError::ShapeMismatch {
        expected: vec![i32::MAX as usize],
        got: vec![value],
        remediation: format!("cuda distance {name} exceeds i32 kernel argument limit"),
    })
}

fn device_unavailable(ctx: &CudaContext, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: format!("cuda:{}", ctx.device_idx()),
        detail,
        remediation: DEVICE_REMEDIATION.to_string(),
    }
}
