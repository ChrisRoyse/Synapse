use super::*;

#[cfg(feature = "cuda")]
use crate::ksg::ksg_mi_continuous_point_cuda_with_context;

#[cfg(feature = "cuda")]
pub(super) fn transfer_entropy_with_config_cuda_strict_impl(
    stream_a: &RecurrenceStream,
    stream_b: &RecurrenceStream,
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
) -> Result<TEResult> {
    validate_config(config)?;
    let forward = lagged_samples(stream_a, stream_b, lag, config.window_size)?;
    let reverse = lagged_samples(stream_b, stream_a, lag, config.window_size)?;
    let n_samples = forward.len().min(reverse.len());
    let forward = &forward[..n_samples];
    let reverse = &reverse[..n_samples];
    let mut pick = resolve_estimator(config, forward, reverse);
    pick.n_samples = n_samples;
    if n_samples < MIN_TE_QUORUM || n_samples < MIN_ASSAY_SAMPLES {
        return Ok(provisional_result(
            lag,
            config.window_size,
            n_samples,
            clock,
            &pick,
        ));
    }
    let backend = calyx_forge::CudaBackend::new()
        .map_err(|err| crate::cuda_strict::forge_to_calyx("transfer entropy", err))?;
    if pick.estimator == Some(TeEstimator::DiscretePlugin) {
        return super::discrete::transfer_entropy_discrete_cuda(
            backend.context(),
            forward,
            reverse,
            lag,
            clock,
            config,
            &pick,
        );
    }
    let t_a_to_b = estimate_te_cuda(backend.context(), forward, config.k)?;
    let t_b_to_a = estimate_te_cuda(backend.context(), reverse, config.k)?;
    let ci_95 = bootstrap_ci_cuda(
        backend.context(),
        forward,
        t_a_to_b,
        config,
        config.bootstrap_seed,
    )?;
    let reverse_ci_95 = bootstrap_ci_cuda(
        backend.context(),
        reverse,
        t_b_to_a,
        config,
        config.bootstrap_seed ^ 0x0B17_B1D5,
    )?;
    let difference_ci_95 = bootstrap_difference_ci_cuda(
        backend.context(),
        forward,
        reverse,
        t_a_to_b - t_b_to_a,
        config,
        config.bootstrap_seed ^ 0x00D1_FFC1,
    )?;
    Ok(TEResult {
        t_a_to_b,
        t_b_to_a,
        dominant_direction: dominant_direction(t_a_to_b, t_b_to_a, ci_95, reverse_ci_95),
        ci_95,
        t_b_to_a_ci_95: reverse_ci_95,
        difference_ci_95,
        lag,
        window_size: config.window_size,
        provisional: false,
        n_samples,
        error_code: None,
        estimator: Some(TeEstimator::ContinuousKsg),
        estimator_selection: pick.selection,
        estimator_reason: pick.reason.clone(),
        trust: TrustTag::Provisional,
        computed_at: clock.now(),
    })
}

#[cfg(not(feature = "cuda"))]
pub(super) fn transfer_entropy_with_config_cuda_strict_impl(
    _stream_a: &RecurrenceStream,
    _stream_b: &RecurrenceStream,
    _lag: usize,
    _clock: &dyn Clock,
    _config: &TransferEntropyConfig,
) -> Result<TEResult> {
    Err(crate::cuda_strict::cuda_unavailable("transfer entropy"))
}

#[cfg(feature = "cuda")]
fn estimate_te_cuda(
    ctx: &calyx_forge::CudaContext,
    samples: &[LaggedSample],
    k: usize,
) -> Result<f32> {
    let future: Vec<_> = samples.iter().map(|sample| sample.future.clone()).collect();
    let joint_past: Vec<_> = samples
        .iter()
        .map(|sample| sample.joint_past.clone())
        .collect();
    let own_past: Vec<_> = samples
        .iter()
        .map(|sample| sample.own_past.clone())
        .collect();
    let joint = ksg_mi_continuous_point_cuda_with_context(ctx, &future, &joint_past, k)?;
    let own = ksg_mi_continuous_point_cuda_with_context(ctx, &future, &own_past, k)?;
    Ok((joint - own).max(0.0))
}

#[cfg(feature = "cuda")]
fn bootstrap_ci_cuda(
    ctx: &calyx_forge::CudaContext,
    samples: &[LaggedSample],
    point: f32,
    config: &TransferEntropyConfig,
    seed: u64,
) -> Result<(f32, f32)> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(config.bootstrap_resamples);
    for _ in 0..config.bootstrap_resamples {
        let resampled = subsample_without_replacement(samples, &mut rng);
        estimates.push(estimate_te_cuda(ctx, &resampled, config.k)?);
    }
    Ok(percentile_ci(estimates, point))
}

#[cfg(feature = "cuda")]
fn bootstrap_difference_ci_cuda(
    ctx: &calyx_forge::CudaContext,
    forward: &[LaggedSample],
    reverse: &[LaggedSample],
    point: f32,
    config: &TransferEntropyConfig,
    seed: u64,
) -> Result<(f32, f32)> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(config.bootstrap_resamples);
    for _ in 0..config.bootstrap_resamples {
        let f = subsample_without_replacement(forward, &mut rng);
        let r = subsample_without_replacement(reverse, &mut rng);
        estimates.push(estimate_te_cuda(ctx, &f, config.k)? - estimate_te_cuda(ctx, &r, config.k)?);
    }
    Ok(percentile_ci(estimates, point))
}
