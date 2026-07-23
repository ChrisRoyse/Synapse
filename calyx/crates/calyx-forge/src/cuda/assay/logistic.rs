use super::*;

#[derive(Clone, Copy, Debug)]
pub struct CudaLogisticDataset<'a> {
    /// Lens-block-major values: every block stores all rows contiguously.
    pub samples: &'a [f32],
    pub labels: &'a [i32],
    pub rows: usize,
    /// Total feature dimensions across the distinct lens blocks.
    pub dim: usize,
    /// Value offsets into `samples`, length `block_dims.len() + 1`.
    pub block_value_offsets: &'a [i32],
    /// Feature offsets into the model weights, length `block_dims.len() + 1`.
    pub block_feature_offsets: &'a [i32],
    /// Per-lens dimensions.
    pub block_dims: &'a [i32],
    /// Training-fold fitted per-lens scales, fit-major then block-major.
    pub block_scales: &'a [f32],
}

#[derive(Clone, Copy, Debug)]
pub struct CudaLogisticSplits<'a> {
    pub train_offsets: &'a [i32],
    pub train_indices: &'a [i32],
    pub test_offsets: &'a [i32],
    pub test_indices: &'a [i32],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CudaLogisticConfig {
    pub max_iterations: usize,
    pub convergence_check_interval: usize,
    pub relative_parameter_tolerance: f32,
    pub learning_rate: f32,
    pub l2_penalty: f32,
}

pub fn logistic_summaries_host(
    ctx: &CudaContext,
    dataset: CudaLogisticDataset<'_>,
    splits: CudaLogisticSplits<'_>,
    config: CudaLogisticConfig,
) -> Result<CudaLogisticSummaries> {
    let CudaLogisticDataset {
        samples,
        labels,
        rows: n,
        dim,
        block_value_offsets,
        block_feature_offsets,
        block_dims,
        block_scales,
    } = dataset;
    let CudaLogisticSplits {
        train_offsets,
        train_indices,
        test_offsets,
        test_indices,
    } = splits;
    let CudaLogisticConfig {
        max_iterations,
        convergence_check_interval,
        relative_parameter_tolerance,
        learning_rate: lr,
        l2_penalty: l2,
    } = config;
    validate_flat_matrix("logistic samples", samples, n, dim)?;
    validate_binary_labels(labels, n)?;
    let fit_count = validate_split_buffers(
        "logistic",
        n,
        train_offsets,
        train_indices,
        test_offsets,
        test_indices,
    )?;
    validate_logistic_block_layout(
        samples,
        n,
        dim,
        fit_count,
        block_value_offsets,
        block_feature_offsets,
        block_dims,
        block_scales,
    )?;
    if max_iterations == 0
        || convergence_check_interval == 0
        || convergence_check_interval > max_iterations
        || max_iterations % convergence_check_interval != 0
        || !relative_parameter_tolerance.is_finite()
        || relative_parameter_tolerance <= 0.0
        || !lr.is_finite()
        || lr <= 0.0
        || !l2.is_finite()
        || l2 < 0.0
    {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![1, 1, 1],
            got: vec![
                max_iterations,
                convergence_check_interval,
                usize::from(relative_parameter_tolerance.is_finite()),
            ],
            remediation: "logistic CUDA requires a positive divisible max-iteration/check interval, positive finite relative-parameter tolerance and learning rate, and non-negative finite L2 penalty".to_string(),
        });
    }
    let workspace_len = fit_count
        .checked_mul(dim)
        .ok_or_else(|| shape_overflow("logistic fit workspace span overflow"))?;
    ensure_device_room(
        ctx,
        "logistic_summaries_host",
        checked_sum_bytes(&[
            bytes::<f32>(samples.len(), "logistic samples")?,
            bytes::<i32>(labels.len(), "logistic labels")?,
            bytes::<i32>(block_value_offsets.len(), "logistic block value offsets")?,
            bytes::<i32>(
                block_feature_offsets.len(),
                "logistic block feature offsets",
            )?,
            bytes::<i32>(block_dims.len(), "logistic block dimensions")?,
            bytes::<f32>(block_scales.len(), "logistic block scales")?,
            bytes::<i32>(train_offsets.len(), "logistic train offsets")?,
            bytes::<i32>(train_indices.len(), "logistic train indices")?,
            bytes::<i32>(test_offsets.len(), "logistic test offsets")?,
            bytes::<i32>(test_indices.len(), "logistic test indices")?,
            bytes::<f32>(workspace_len, "logistic weights workspace")?,
            bytes::<f32>(workspace_len, "logistic gradient workspace")?,
            bytes::<f32>(3 * fit_count, "logistic floating summaries")?,
            bytes::<i32>(2 * fit_count, "logistic integer summaries")?,
        ])?,
    )?;

    let stream = ctx.inner().default_stream();
    let samples_dev = stream
        .clone_htod(samples)
        .map_err(|err| device_unavailable(ctx, format!("logistic samples upload failed: {err}")))?;
    let labels_dev = stream
        .clone_htod(labels)
        .map_err(|err| device_unavailable(ctx, format!("logistic labels upload failed: {err}")))?;
    let block_value_offsets_dev = stream.clone_htod(block_value_offsets).map_err(|err| {
        device_unavailable(
            ctx,
            format!("logistic block value offsets upload failed: {err}"),
        )
    })?;
    let block_feature_offsets_dev = stream.clone_htod(block_feature_offsets).map_err(|err| {
        device_unavailable(
            ctx,
            format!("logistic block feature offsets upload failed: {err}"),
        )
    })?;
    let block_dims_dev = stream.clone_htod(block_dims).map_err(|err| {
        device_unavailable(
            ctx,
            format!("logistic block dimensions upload failed: {err}"),
        )
    })?;
    let block_scales_dev = stream.clone_htod(block_scales).map_err(|err| {
        device_unavailable(ctx, format!("logistic block scales upload failed: {err}"))
    })?;
    let train_offsets_dev = stream.clone_htod(train_offsets).map_err(|err| {
        device_unavailable(ctx, format!("logistic train offsets upload failed: {err}"))
    })?;
    let train_indices_dev = stream.clone_htod(train_indices).map_err(|err| {
        device_unavailable(ctx, format!("logistic train indices upload failed: {err}"))
    })?;
    let test_offsets_dev = stream.clone_htod(test_offsets).map_err(|err| {
        device_unavailable(ctx, format!("logistic test offsets upload failed: {err}"))
    })?;
    let test_indices_dev = stream.clone_htod(test_indices).map_err(|err| {
        device_unavailable(ctx, format!("logistic test indices upload failed: {err}"))
    })?;
    let mut bits: CudaSlice<f32> = stream.alloc_zeros(fit_count).map_err(|err| {
        device_unavailable(ctx, format!("logistic bits allocation failed: {err}"))
    })?;
    let mut accuracy: CudaSlice<f32> = stream.alloc_zeros(fit_count).map_err(|err| {
        device_unavailable(ctx, format!("logistic accuracy allocation failed: {err}"))
    })?;
    let mut iterations: CudaSlice<i32> = stream.alloc_zeros(fit_count).map_err(|err| {
        device_unavailable(ctx, format!("logistic iterations allocation failed: {err}"))
    })?;
    let mut final_relative_parameter_changes: CudaSlice<f32> =
        stream.alloc_zeros(fit_count).map_err(|err| {
            device_unavailable(
                ctx,
                format!("logistic relative-change allocation failed: {err}"),
            )
        })?;
    let mut converged: CudaSlice<i32> = stream.alloc_zeros(fit_count).map_err(|err| {
        device_unavailable(
            ctx,
            format!("logistic convergence flags allocation failed: {err}"),
        )
    })?;
    let mut weights_workspace: CudaSlice<f32> =
        stream.alloc_zeros(workspace_len).map_err(|err| {
            device_unavailable(
                ctx,
                format!("logistic weights workspace allocation failed: {err}"),
            )
        })?;
    let mut gradient_workspace: CudaSlice<f32> =
        stream.alloc_zeros(workspace_len).map_err(|err| {
            device_unavailable(
                ctx,
                format!("logistic gradient workspace allocation failed: {err}"),
            )
        })?;
    let mut flags = alloc_flags(ctx, "logistic_summaries")?;
    let func = assay_function(
        ctx,
        "assay.logistic_summaries_f32",
        "assay_logistic_summaries_f32",
    )?;
    let fit_count_i32 = to_i32(fit_count, "logistic fit count")?;
    let n_i32 = to_i32(n, "logistic sample count")?;
    let dim_i32 = to_i32(dim, "logistic dimension")?;
    let block_count_i32 = to_i32(block_dims.len(), "logistic block count")?;
    let max_iterations_i32 = to_i32(max_iterations, "logistic max iterations")?;
    let convergence_check_interval_i32 = to_i32(
        convergence_check_interval,
        "logistic convergence check interval",
    )?;
    let cfg = LaunchConfig {
        grid_dim: (to_u32(fit_count, "logistic fit grid")?, 1, 1),
        block_dim: (THREADS, 1, 1),
        shared_mem_bytes: 0,
    };
    let mut launch = stream.launch_builder(func.as_ref());
    unsafe {
        launch
            .arg(&samples_dev)
            .arg(&labels_dev)
            .arg(&block_value_offsets_dev)
            .arg(&block_feature_offsets_dev)
            .arg(&block_dims_dev)
            .arg(&block_scales_dev)
            .arg(&block_count_i32)
            .arg(&train_offsets_dev)
            .arg(&train_indices_dev)
            .arg(&test_offsets_dev)
            .arg(&test_indices_dev)
            .arg(&fit_count_i32)
            .arg(&n_i32)
            .arg(&dim_i32)
            .arg(&max_iterations_i32)
            .arg(&convergence_check_interval_i32)
            .arg(&relative_parameter_tolerance)
            .arg(&lr)
            .arg(&l2)
            .arg(&mut weights_workspace)
            .arg(&mut gradient_workspace)
            .arg(&mut bits)
            .arg(&mut accuracy)
            .arg(&mut iterations)
            .arg(&mut final_relative_parameter_changes)
            .arg(&mut converged)
            .arg(&mut flags)
            .launch(cfg)
    }
    .map_err(|err| device_unavailable(ctx, format!("logistic summaries launch failed: {err}")))?;
    sync_and_decode(ctx, "logistic_summaries", &flags)?;
    let bits = read_f32(ctx, &bits, "logistic bits")?;
    let accuracy = read_f32(ctx, &accuracy, "logistic accuracy")?;
    let raw_iterations = read_device_i32(ctx, "logistic iterations", &iterations)?;
    let final_relative_parameter_changes = read_f32(
        ctx,
        &final_relative_parameter_changes,
        "logistic final relative parameter changes",
    )?;
    let raw_converged = read_device_i32(ctx, "logistic convergence flags", &converged)?;
    let mut iterations = Vec::with_capacity(fit_count);
    let mut converged = Vec::with_capacity(fit_count);
    for fit in 0..fit_count {
        let raw_iteration = raw_iterations[fit];
        if raw_iteration <= 0 || raw_iteration as usize > max_iterations {
            return Err(numerical(
                "logistic iterations",
                format!("logistic iteration readback out of range at fit {fit}: {raw_iteration}"),
            ));
        }
        let raw_status = raw_converged[fit];
        if raw_status != 0 && raw_status != 1 {
            return Err(numerical(
                "logistic convergence",
                format!(
                    "logistic convergence flag readback out of range at fit {fit}: {raw_status}"
                ),
            ));
        }
        let final_relative_change = final_relative_parameter_changes[fit];
        if !final_relative_change.is_finite() || final_relative_change < 0.0 {
            return Err(numerical(
                "logistic convergence",
                format!(
                    "logistic final relative parameter change readback is invalid at fit {fit}: {final_relative_change}"
                ),
            ));
        }
        iterations.push(raw_iteration as usize);
        converged.push(raw_status == 1);
    }
    for (idx, value) in accuracy.iter().copied().enumerate() {
        if !(0.0..=1.0).contains(&value) {
            return Err(numerical(
                "logistic accuracy",
                format!("logistic accuracy readback out of range at fit {idx}: {value}"),
            ));
        }
    }
    Ok(CudaLogisticSummaries {
        bits,
        accuracy,
        iterations,
        final_relative_parameter_changes,
        converged,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_logistic_block_layout(
    samples: &[f32],
    rows: usize,
    total_dim: usize,
    fit_count: usize,
    value_offsets: &[i32],
    feature_offsets: &[i32],
    dims: &[i32],
    scales: &[f32],
) -> Result<()> {
    if dims.is_empty()
        || value_offsets.len() != dims.len() + 1
        || feature_offsets.len() != dims.len() + 1
    {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![dims.len() + 1, dims.len() + 1],
            got: vec![value_offsets.len(), feature_offsets.len()],
            remediation:
                "logistic CUDA requires non-empty lens blocks with value/feature offset sentinels"
                    .to_string(),
        });
    }
    if value_offsets.first() != Some(&0) || feature_offsets.first() != Some(&0) {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![0, 0],
            got: vec![
                value_offsets.first().copied().unwrap_or(-1).max(0) as usize,
                feature_offsets.first().copied().unwrap_or(-1).max(0) as usize,
            ],
            remediation: "logistic CUDA block offsets must start at zero".to_string(),
        });
    }
    let mut expected_values = 0_usize;
    let mut expected_features = 0_usize;
    for (block, &raw_dim) in dims.iter().enumerate() {
        let dim = usize::try_from(raw_dim).map_err(|_| ForgeError::ShapeMismatch {
            expected: vec![1],
            got: vec![0],
            remediation: format!("logistic CUDA block {block} dimension must be positive"),
        })?;
        if dim == 0 {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![1],
                got: vec![0],
                remediation: format!("logistic CUDA block {block} dimension must be positive"),
            });
        }
        expected_values = expected_values
            .checked_add(
                rows.checked_mul(dim)
                    .ok_or_else(|| shape_overflow("logistic block value span overflow"))?,
            )
            .ok_or_else(|| shape_overflow("logistic block value offset overflow"))?;
        expected_features = expected_features
            .checked_add(dim)
            .ok_or_else(|| shape_overflow("logistic block feature offset overflow"))?;
        let actual_value =
            usize::try_from(value_offsets[block + 1]).map_err(|_| ForgeError::ShapeMismatch {
                expected: vec![expected_values],
                got: vec![0],
                remediation: format!(
                    "logistic CUDA block {block} value offset must be non-negative"
                ),
            })?;
        let actual_feature =
            usize::try_from(feature_offsets[block + 1]).map_err(|_| ForgeError::ShapeMismatch {
                expected: vec![expected_features],
                got: vec![0],
                remediation: format!(
                    "logistic CUDA block {block} feature offset must be non-negative"
                ),
            })?;
        if actual_value != expected_values || actual_feature != expected_features {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![expected_values, expected_features],
                got: vec![actual_value, actual_feature],
                remediation: format!(
                    "logistic CUDA block {block} offsets must exactly preserve the lens-block-major layout"
                ),
            });
        }
    }
    if expected_values != samples.len() || expected_features != total_dim {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![samples.len(), total_dim],
            got: vec![expected_values, expected_features],
            remediation:
                "logistic CUDA lens-block spans must cover every value and total feature dimension"
                    .to_string(),
        });
    }
    let expected_scales = fit_count
        .checked_mul(dims.len())
        .ok_or_else(|| shape_overflow("logistic block scale span overflow"))?;
    if scales.len() != expected_scales {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![expected_scales],
            got: vec![scales.len()],
            remediation: "logistic CUDA requires one positive fitted scale per fit and lens block"
                .to_string(),
        });
    }
    for (idx, &scale) in scales.iter().enumerate() {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(numerical(
                "logistic block conditioning",
                format!("logistic block scale {idx} must be positive finite, got {scale}"),
            ));
        }
    }
    Ok(())
}
