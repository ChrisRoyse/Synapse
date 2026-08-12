#![allow(clippy::too_many_arguments)]

use std::str;
use std::sync::Arc;

use cudarc::driver::{CudaModule, CudaSlice, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;

use crate::cuda::kernels::{PACKED_QUANT_CUBIN, PACKED_QUANT_PTX};
use crate::{CudaContext, ForgeError, Result};

const BLOCK: u32 = 256;

pub(super) fn binary_encode(
    ctx: &CudaContext,
    input: &CudaSlice<f32>,
    diagonal: &CudaSlice<f32>,
    dim: usize,
    rows: usize,
    encoded: &mut CudaSlice<u8>,
    bad: &mut CudaSlice<i32>,
) -> Result<()> {
    let (dim_i32, rows_i32) = dims(dim, rows)?;
    let function = function(ctx, "packed.binary_encode", "pq_binary_encode_f32")?;
    let stream = ctx.inner().default_stream();
    let mut launch = stream.launch_builder(function.as_ref());
    unsafe {
        launch
            .arg(input)
            .arg(diagonal)
            .arg(&dim_i32)
            .arg(&rows_i32)
            .arg(encoded)
            .arg(bad)
            .launch(row_blocks(rows, shared_bytes(dim)?)?)
    }
    .map_err(|error| device(ctx, format!("binary encode launch failed: {error}")))?;
    sync(ctx, "binary encode")
}

pub(super) fn binary_decode(
    ctx: &CudaContext,
    encoded: &CudaSlice<u8>,
    diagonal: &CudaSlice<f32>,
    dim: usize,
    rows: usize,
    amplitude: f32,
    output: &mut CudaSlice<f32>,
) -> Result<()> {
    let (dim_i32, rows_i32) = dims(dim, rows)?;
    let function = function(ctx, "packed.binary_decode", "pq_binary_decode_f32")?;
    let stream = ctx.inner().default_stream();
    let mut launch = stream.launch_builder(function.as_ref());
    unsafe {
        launch
            .arg(encoded)
            .arg(diagonal)
            .arg(&dim_i32)
            .arg(&rows_i32)
            .arg(&amplitude)
            .arg(output)
            .launch(row_blocks(rows, shared_bytes(dim)?)?)
    }
    .map_err(|error| device(ctx, format!("binary decode launch failed: {error}")))?;
    sync(ctx, "binary decode")
}

pub(super) fn binary_score(
    ctx: &CudaContext,
    query: &CudaSlice<u8>,
    candidates: &CudaSlice<u8>,
    dim: usize,
    rows: usize,
    mismatches: &mut CudaSlice<i32>,
    scores: &mut CudaSlice<f32>,
) -> Result<()> {
    let (dim_i32, rows_i32) = dims(dim, rows)?;
    let function = function(ctx, "packed.binary_score", "pq_binary_score")?;
    let stream = ctx.inner().default_stream();
    let mut launch = stream.launch_builder(function.as_ref());
    unsafe {
        launch
            .arg(query)
            .arg(candidates)
            .arg(&dim_i32)
            .arg(&rows_i32)
            .arg(mismatches)
            .arg(scores)
            .launch(flat_threads(rows)?)
    }
    .map_err(|error| device(ctx, format!("binary score launch failed: {error}")))?;
    sync(ctx, "binary score")
}

fn function(
    ctx: &CudaContext,
    cache_key: &'static str,
    name: &'static str,
) -> Result<Arc<cudarc::driver::CudaFunction>> {
    let module = packed_module(ctx)?;
    ctx.cached_function(&module, cache_key, name)
        .map_err(|error| device(ctx, format!("load {name} failed: {error}")))
}

fn packed_module(ctx: &CudaContext) -> Result<Arc<CudaModule>> {
    if let Some(module) = ctx.packed_quant_module_cache().get() {
        return Ok(module.clone());
    }
    let module = match ctx
        .inner()
        .load_module(Ptx::from_binary(PACKED_QUANT_CUBIN.to_vec()))
    {
        Ok(module) => module,
        Err(cubin_error) => {
            let ptx = str::from_utf8(PACKED_QUANT_PTX)
                .map_err(|error| device(ctx, format!("packed quant PTX is not UTF-8: {error}")))?;
            ctx.inner().load_module(Ptx::from_src(ptx)).map_err(|error| {
                device(
                    ctx,
                    format!(
                        "packed quant CUBIN load failed: {cubin_error}; PTX fallback failed: {error}"
                    ),
                )
            })?
        }
    };
    let _ = ctx.packed_quant_module_cache().set(module.clone());
    Ok(module)
}

fn dims(dim: usize, rows: usize) -> Result<(i32, i32)> {
    let count = dim.checked_mul(rows).ok_or_else(shape_overflow)?;
    if count > i32::MAX as usize {
        return Err(shape("packed CUDA element count exceeds i32 indexing"));
    }
    Ok((as_i32(dim, "dimension")?, as_i32(rows, "row count")?))
}

fn row_blocks(rows: usize, shared_mem_bytes: u32) -> Result<LaunchConfig> {
    Ok(LaunchConfig {
        grid_dim: (as_u32(rows, "row count")?, 1, 1),
        block_dim: (BLOCK, 1, 1),
        shared_mem_bytes,
    })
}

fn flat_threads(items: usize) -> Result<LaunchConfig> {
    Ok(LaunchConfig {
        grid_dim: (as_u32(items.div_ceil(BLOCK as usize), "grid blocks")?, 1, 1),
        block_dim: (BLOCK, 1, 1),
        shared_mem_bytes: 0,
    })
}

fn shared_bytes(dim: usize) -> Result<u32> {
    u32::try_from(
        dim.checked_mul(size_of::<f32>())
            .ok_or_else(shape_overflow)?,
    )
    .map_err(|_| shape("packed CUDA shared-memory request exceeds u32"))
}

fn sync(ctx: &CudaContext, operation: &str) -> Result<()> {
    ctx.inner()
        .default_stream()
        .synchronize()
        .map_err(|error| device(ctx, format!("{operation} synchronization failed: {error}")))
}

fn as_i32(value: usize, name: &str) -> Result<i32> {
    i32::try_from(value).map_err(|_| shape(format!("{name} exceeds i32")))
}

fn as_u32(value: usize, name: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| shape(format!("{name} exceeds u32")))
}

fn shape_overflow() -> ForgeError {
    shape("packed CUDA allocation shape overflow")
}

fn shape(detail: impl Into<String>) -> ForgeError {
    ForgeError::ShapeMismatch {
        expected: vec![1],
        got: vec![0],
        remediation: detail.into(),
    }
}

fn device(ctx: &CudaContext, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: format!("cuda:{}", ctx.device_idx()),
        detail,
        remediation: "Use the embedded sm_120 packed quant kernels on available CUDA".to_string(),
    }
}
