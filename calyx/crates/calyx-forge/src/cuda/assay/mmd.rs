use super::*;

pub fn gaussian_mmd_host(
    ctx: &CudaContext,
    pooled: &[f64],
    n_a: usize,
    n_b: usize,
    dim: usize,
    bandwidth: f64,
    permutations: &[i32],
) -> Result<CudaMmdResult> {
    validate_mmd_inputs(pooled, n_a, n_b, dim, bandwidth)?;
    let n = n_a + n_b;
    let perm_count = validate_permutations(Some(permutations), n)?;
    let device_bytes = gaussian_mmd_device_bytes(pooled.len(), n, perm_count)?;
    ensure_device_room(ctx, "gaussian_mmd_host", device_bytes)?;

    let stream = ctx.inner().default_stream();
    let samples_dev = stream
        .clone_htod(pooled)
        .map_err(|err| device_unavailable(ctx, format!("MMD pooled upload failed: {err}")))?;
    let (kernel, _row_sums) = rbf_matrix_f64(ctx, &samples_dev, n, dim, bandwidth, "mmd")?;
    let observed = mmd_observed(ctx, &kernel, n, n_a)?;
    let null = if perm_count == 0 {
        Vec::new()
    } else {
        let null_dev = mmd_permutation_stats(ctx, &kernel, n, n_a, permutations, perm_count)?;
        read_device_f64(ctx, "MMD null distribution", &null_dev)?
    };
    Ok(CudaMmdResult {
        mmd2: observed,
        null,
    })
}

/// Exact peak device-buffer bytes held by [`gaussian_mmd_host`].
///
/// The budgeted CUDA backend calls this same shape function before dispatch,
/// so admission and the concrete kernel allocation cannot drift apart.
pub fn gaussian_mmd_device_bytes(
    pooled_len: usize,
    sample_count: usize,
    permutation_count: usize,
) -> Result<usize> {
    let matrix_len = checked_square(sample_count, "MMD kernel matrix length")?;
    let permutation_elements = permutation_count
        .checked_mul(sample_count)
        .ok_or_else(|| shape_overflow("MMD permutation matrix length"))?;
    checked_sum_bytes(&[
        bytes::<f64>(pooled_len, "MMD pooled samples")?,
        bytes::<f64>(matrix_len, "MMD kernel matrix")?,
        bytes::<f64>(sample_count, "MMD row sums")?,
        bytes::<i32>(permutation_elements, "MMD permutations")?,
        bytes::<f64>(permutation_count, "MMD permutation outputs")?,
        bytes::<f64>(1, "MMD observed output")?,
    ])
}

pub fn mmd_change_point_host(
    ctx: &CudaContext,
    samples: &[f64],
    n: usize,
    dim: usize,
    min_window: usize,
    bandwidth: f64,
    permutations: &[i32],
) -> Result<CudaMmdChangePointResult> {
    validate_mmd_change_inputs(samples, n, dim, min_window, bandwidth)?;
    let perm_count = validate_permutations(Some(permutations), n)?;
    let matrix_len = checked_square(n, "MMD change-point kernel matrix length")?;
    ensure_device_room(
        ctx,
        "mmd_change_point_host",
        checked_sum_bytes(&[
            bytes::<f64>(samples.len(), "MMD change-point samples")?,
            bytes::<f64>(matrix_len, "MMD change-point kernel matrix")?,
            bytes::<f64>(n, "MMD change-point row sums")?,
            bytes::<i32>(perm_count * n, "MMD change-point permutations")?,
            bytes::<f64>(perm_count, "MMD change-point null outputs")?,
            bytes::<f64>(1, "MMD change-point observed value")?,
            bytes::<i32>(1, "MMD change-point split")?,
        ])?,
    )?;

    let stream = ctx.inner().default_stream();
    let samples_dev = stream.clone_htod(samples).map_err(|err| {
        device_unavailable(ctx, format!("MMD change-point sample upload failed: {err}"))
    })?;
    let (kernel, _row_sums) = rbf_matrix_f64(ctx, &samples_dev, n, dim, bandwidth, "mmd_change")?;
    let (split_index, observed) = mmd_change_observed(ctx, &kernel, n, min_window)?;
    let null = if perm_count == 0 {
        Vec::new()
    } else {
        let null_dev =
            mmd_change_permutation_stats(ctx, &kernel, n, min_window, permutations, perm_count)?;
        read_device_f64(ctx, "MMD change-point null distribution", &null_dev)?
    };
    Ok(CudaMmdChangePointResult {
        split_index,
        mmd2: observed,
        null,
    })
}

pub(super) fn mmd_observed(
    ctx: &CudaContext,
    kernel: &CudaSlice<f64>,
    n: usize,
    n_a: usize,
) -> Result<f64> {
    let stream = ctx.inner().default_stream();
    let mut out: CudaSlice<f64> = stream
        .alloc_zeros(1)
        .map_err(|err| device_unavailable(ctx, format!("MMD observed allocation failed: {err}")))?;
    let mut flags = alloc_flags(ctx, "mmd_observed")?;
    let n_i32 = to_i32(n, "MMD n")?;
    let n_a_i32 = to_i32(n_a, "MMD n_a")?;
    let func = assay_function(ctx, "assay.mmd_observed_f64", "assay_mmd_observed_f64")?;
    let cfg = LaunchConfig {
        grid_dim: (1, 1, 1),
        block_dim: (THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(kernel)
            .arg(&n_i32)
            .arg(&n_a_i32)
            .arg(&mut out)
            .arg(&mut flags)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("MMD observed launch failed: {err}")))?;
    sync_and_decode(ctx, "mmd_observed", &flags)?;
    let values = read_device_f64(ctx, "MMD observed readback", &out)?;
    Ok(values[0])
}

pub(super) fn mmd_permutation_stats(
    ctx: &CudaContext,
    kernel: &CudaSlice<f64>,
    n: usize,
    n_a: usize,
    permutations: &[i32],
    perm_count: usize,
) -> Result<CudaSlice<f64>> {
    let n_i32 = to_i32(n, "MMD n")?;
    let n_a_i32 = to_i32(n_a, "MMD n_a")?;
    let perm_i32 = to_i32(perm_count, "MMD permutations")?;
    let stream = ctx.inner().default_stream();
    let perms_dev = stream
        .clone_htod(permutations)
        .map_err(|err| device_unavailable(ctx, format!("MMD permutations upload failed: {err}")))?;
    let mut out: CudaSlice<f64> = stream.alloc_zeros(perm_count).map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD permutation output allocation failed: {err}"),
        )
    })?;
    let mut flags = alloc_flags(ctx, "mmd_permutations")?;
    let func = assay_function(
        ctx,
        "assay.mmd_permutations_f64",
        "assay_mmd_permutations_f64",
    )?;
    let cfg = LaunchConfig {
        grid_dim: (to_u32(perm_count, "MMD permutation grid")?, 1, 1),
        block_dim: (THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(kernel)
            .arg(&perms_dev)
            .arg(&n_i32)
            .arg(&n_a_i32)
            .arg(&perm_i32)
            .arg(&mut out)
            .arg(&mut flags)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("MMD permutation launch failed: {err}")))?;
    sync_and_decode(ctx, "mmd_permutations", &flags)?;
    Ok(out)
}

pub(super) fn mmd_change_observed(
    ctx: &CudaContext,
    kernel: &CudaSlice<f64>,
    n: usize,
    min_window: usize,
) -> Result<(usize, f64)> {
    let stream = ctx.inner().default_stream();
    let mut value_out: CudaSlice<f64> = stream.alloc_zeros(1).map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point value allocation failed: {err}"),
        )
    })?;
    let mut split_out: CudaSlice<i32> = stream.alloc_zeros(1).map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point split allocation failed: {err}"),
        )
    })?;
    let mut flags = alloc_flags(ctx, "mmd_change_observed")?;
    let n_i32 = to_i32(n, "MMD change-point n")?;
    let min_window_i32 = to_i32(min_window, "MMD change-point min_window")?;
    let func = assay_function(
        ctx,
        "assay.mmd_change_observed_f64",
        "assay_mmd_change_observed_f64",
    )?;
    let cfg = LaunchConfig {
        grid_dim: (1, 1, 1),
        block_dim: (THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(kernel)
            .arg(&n_i32)
            .arg(&min_window_i32)
            .arg(&mut value_out)
            .arg(&mut split_out)
            .arg(&mut flags)
            .launch(cfg)
    }
    .map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point observed launch failed: {err}"),
        )
    })?;
    sync_and_decode(ctx, "mmd_change_observed", &flags)?;
    let value = read_device_f64(ctx, "MMD change-point observed value", &value_out)?;
    let split = stream.clone_dtoh(&split_out).map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point split readback failed: {err}"),
        )
    })?;
    let split = split.first().copied().unwrap_or_default();
    if split < 0 {
        return Err(numerical(
            "mmd_change_observed",
            format!("negative split index returned by CUDA: {split}"),
        ));
    }
    Ok((split as usize, value[0]))
}

pub(super) fn mmd_change_permutation_stats(
    ctx: &CudaContext,
    kernel: &CudaSlice<f64>,
    n: usize,
    min_window: usize,
    permutations: &[i32],
    perm_count: usize,
) -> Result<CudaSlice<f64>> {
    let n_i32 = to_i32(n, "MMD change-point n")?;
    let min_window_i32 = to_i32(min_window, "MMD change-point min_window")?;
    let perm_i32 = to_i32(perm_count, "MMD change-point permutations")?;
    let stream = ctx.inner().default_stream();
    let perms_dev = stream.clone_htod(permutations).map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point permutations upload failed: {err}"),
        )
    })?;
    let mut out: CudaSlice<f64> = stream.alloc_zeros(perm_count).map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point null allocation failed: {err}"),
        )
    })?;
    let mut flags = alloc_flags(ctx, "mmd_change_permutations")?;
    let func = assay_function(
        ctx,
        "assay.mmd_change_permutations_f64",
        "assay_mmd_change_permutations_f64",
    )?;
    let cfg = LaunchConfig {
        grid_dim: (
            to_u32(perm_count, "MMD change-point permutation grid")?,
            1,
            1,
        ),
        block_dim: (THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(kernel)
            .arg(&perms_dev)
            .arg(&n_i32)
            .arg(&min_window_i32)
            .arg(&perm_i32)
            .arg(&mut out)
            .arg(&mut flags)
            .launch(cfg)
    }
    .map_err(|err| {
        device_unavailable(
            ctx,
            format!("MMD change-point permutation launch failed: {err}"),
        )
    })?;
    sync_and_decode(ctx, "mmd_change_permutations", &flags)?;
    Ok(out)
}
