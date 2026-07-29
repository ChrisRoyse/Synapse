//! Transfer entropy over recurrence streams (PRD `26 §4`, PH52).
//!
//! `T(A -> B) = I(B_future; A_past, B_past) - I(B_future; B_past)`.
//! This module keeps the estimator honest by returning provisional, code-tagged
//! readbacks below quorum instead of silently treating underpowered streams as
//! causal evidence.
//!
//! Two estimators live behind one entry point (issue #1673):
//!
//! * [`TeEstimator::ContinuousKsg`] — the Assay KSG k-nearest-neighbour MI path,
//!   valid for samples from a joint density.
//! * [`TeEstimator::DiscretePlugin`] — the plug-in / Miller-Madow estimator in
//!   [`discrete`], valid for symbol streams such as binned occurrence counts, on
//!   which KSG's k-th joint radius degenerates to zero.
//!
//! Which one ran is **always** reported on [`TEResult::estimator`], together
//! with [`TEResult::estimator_selection`] and [`TEResult::estimator_reason`].
//! An estimator is never swapped silently: [`TransferEntropyConfig::estimator`]
//! either pins one, or asks for the documented auto rule whose verdict and
//! reason then travel with the result.

mod cuda;
pub mod discrete;

use rand::{SeedableRng, seq::SliceRandom};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use calyx_core::{CalyxError, Clock, Result, Ts};

use crate::cuda_strict::strict_cuda_requested;
use crate::estimate::TrustTag;
use crate::ksg::{MIN_ASSAY_SAMPLES, ksg_mi_continuous_point};

use self::cuda::transfer_entropy_with_config_cuda_strict_impl;
use self::discrete::{integral_coordinate_counts, transfer_entropy_discrete};

pub type Timestamp = Ts;
pub type RecurrenceStream = [(Timestamp, f32)];

pub const CALYX_TE_INSUFFICIENT_SAMPLES: &str = "CALYX_TE_INSUFFICIENT_SAMPLES";
pub const MIN_TE_QUORUM: usize = 30;
pub const DEFAULT_TE_WINDOW: usize = 1;
pub const DEFAULT_TE_K: usize = 3;
pub const DEFAULT_TE_BOOTSTRAP_RESAMPLES: usize = 500;
pub const DEFAULT_TE_BOOTSTRAP_SEED: u64 = 52;
pub const DEFAULT_TE_LAGS: &[usize] = &[1, 2, 4, 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    #[serde(rename = "A_to_B")]
    AToB,
    #[serde(rename = "B_to_A")]
    BToA,
    Unclear,
}

/// The estimator a caller asks [`transfer_entropy_with_config`] to use.
///
/// `Auto` is a *declared* rule, not a hidden fallback: its verdict and reason
/// are reported on every [`TEResult`] it produces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeEstimatorChoice {
    /// Pick from the observed sample domain and report why.
    #[default]
    Auto,
    /// Always use the discrete plug-in / Miller-Madow estimator.
    DiscretePlugin,
    /// Always use the continuous KSG estimator.
    ContinuousKsg,
}

/// The estimator that actually produced a [`TEResult`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeEstimator {
    /// Plug-in entropy decomposition with a Miller-Madow bias correction.
    DiscretePlugin,
    /// Kraskov-Stögbauer-Grassberger k-nearest-neighbour mutual information.
    ContinuousKsg,
}

impl TeEstimator {
    /// Stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DiscretePlugin => "discrete_plugin",
            Self::ContinuousKsg => "continuous_ksg",
        }
    }
}

/// Why the reported estimator was the one used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeEstimatorSelection {
    /// The caller pinned the discrete plug-in estimator.
    RequestedDiscretePlugin,
    /// The caller pinned the continuous KSG estimator.
    RequestedContinuousKsg,
    /// Auto: every sampled coordinate is a finite integer, so the KSG k-th joint
    /// radius would be degenerate.
    AutoIntegralSamples,
    /// Auto: some sampled coordinates are non-integral, so KSG applies.
    AutoRealValuedSamples,
    /// The lag failed before any estimator could be applied.
    Unresolved,
}

impl TeEstimatorSelection {
    /// Stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestedDiscretePlugin => "requested_discrete_plugin",
            Self::RequestedContinuousKsg => "requested_continuous_ksg",
            Self::AutoIntegralSamples => "auto_integral_samples",
            Self::AutoRealValuedSamples => "auto_real_valued_samples",
            Self::Unresolved => "unresolved",
        }
    }
}

/// What a single lag attempted: the resolved estimator, why it was resolved that
/// way, and how many paired samples reached it. Carried out of a *failed* lag
/// too, so a terminal diagnostic keeps per-lag detail instead of erasing it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EstimatorPick {
    pub(crate) estimator: Option<TeEstimator>,
    pub(crate) selection: TeEstimatorSelection,
    pub(crate) reason: String,
    pub(crate) n_samples: usize,
}

impl EstimatorPick {
    fn unresolved() -> Self {
        Self {
            estimator: None,
            selection: TeEstimatorSelection::Unresolved,
            reason: "the lag failed before an estimator was applied".to_string(),
            n_samples: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TEResult {
    pub t_a_to_b: f32,
    pub t_b_to_a: f32,
    pub dominant_direction: Direction,
    pub ci_95: (f32, f32),
    pub t_b_to_a_ci_95: (f32, f32),
    pub difference_ci_95: (f32, f32),
    pub lag: usize,
    pub window_size: usize,
    pub provisional: bool,
    pub n_samples: usize,
    pub error_code: Option<String>,
    /// The estimator that produced this row; `None` only when the lag failed
    /// before one could be applied.
    pub estimator: Option<TeEstimator>,
    /// How that estimator came to be used.
    pub estimator_selection: TeEstimatorSelection,
    /// Human-readable justification for `estimator_selection`.
    pub estimator_reason: String,
    pub trust: TrustTag,
    pub computed_at: Ts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferEntropyConfig {
    pub window_size: usize,
    pub k: usize,
    pub bootstrap_resamples: usize,
    pub bootstrap_seed: u64,
    /// Explicit estimator selection. Never silently overridden.
    pub estimator: TeEstimatorChoice,
}

impl Default for TransferEntropyConfig {
    fn default() -> Self {
        Self {
            window_size: DEFAULT_TE_WINDOW,
            k: DEFAULT_TE_K,
            bootstrap_resamples: DEFAULT_TE_BOOTSTRAP_RESAMPLES,
            bootstrap_seed: DEFAULT_TE_BOOTSTRAP_SEED,
            estimator: TeEstimatorChoice::Auto,
        }
    }
}

pub fn transfer_entropy(
    stream_a: &RecurrenceStream,
    stream_b: &RecurrenceStream,
    lag: usize,
    clock: &dyn Clock,
) -> Result<TEResult> {
    transfer_entropy_with_config(
        stream_a,
        stream_b,
        lag,
        clock,
        &TransferEntropyConfig::default(),
    )
}

pub fn transfer_entropy_with_config(
    stream_a: &RecurrenceStream,
    stream_b: &RecurrenceStream,
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
) -> Result<TEResult> {
    let mut pick = EstimatorPick::unresolved();
    transfer_entropy_inner(stream_a, stream_b, lag, clock, config, &mut pick)
}

/// Shared body. `pick` is filled the moment the estimator is resolved so a later
/// failure still reports which estimator was applied instead of erasing it.
fn transfer_entropy_inner(
    stream_a: &RecurrenceStream,
    stream_b: &RecurrenceStream,
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
    pick: &mut EstimatorPick,
) -> Result<TEResult> {
    if strict_cuda_requested() {
        return transfer_entropy_with_config_cuda_strict(stream_a, stream_b, lag, clock, config);
    }
    validate_config(config)?;
    let forward = lagged_samples(stream_a, stream_b, lag, config.window_size)?;
    let reverse = lagged_samples(stream_b, stream_a, lag, config.window_size)?;
    let n_samples = forward.len().min(reverse.len());
    let forward = &forward[..n_samples];
    let reverse = &reverse[..n_samples];
    *pick = resolve_estimator(config, forward, reverse);
    pick.n_samples = n_samples;
    if n_samples < MIN_TE_QUORUM || n_samples < MIN_ASSAY_SAMPLES {
        return Ok(provisional_result(
            lag,
            config.window_size,
            n_samples,
            clock,
            pick,
        ));
    }
    match pick.estimator {
        Some(TeEstimator::DiscretePlugin) => {
            transfer_entropy_discrete(forward, reverse, lag, clock, config, pick)
        }
        Some(TeEstimator::ContinuousKsg) | None => {
            transfer_entropy_continuous(forward, reverse, lag, clock, config, pick)
        }
    }
}

fn transfer_entropy_continuous(
    forward: &[LaggedSample],
    reverse: &[LaggedSample],
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
    pick: &EstimatorPick,
) -> Result<TEResult> {
    let n_samples = forward.len().min(reverse.len());
    let t_a_to_b = estimate_te(forward, config.k)?;
    let t_b_to_a = estimate_te(reverse, config.k)?;
    let ci_95 = bootstrap_ci(forward, t_a_to_b, config, config.bootstrap_seed)?;
    let reverse_ci_95 = bootstrap_ci(
        reverse,
        t_b_to_a,
        config,
        config.bootstrap_seed ^ 0x0B17_B1D5,
    )?;
    let difference_ci_95 = bootstrap_difference_ci(
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

/// The declared estimator-selection rule.
///
/// KSG needs a non-zero k-th joint radius, which integer-valued coordinates
/// cannot supply once k or more samples coincide (Kraskov et al. 2004). So the
/// auto rule keys on exactly that data property — are all sampled coordinates
/// finite integers — and records the counts it saw in the reason. It never
/// re-tries the other estimator after a failure.
fn resolve_estimator(
    config: &TransferEntropyConfig,
    forward: &[LaggedSample],
    reverse: &[LaggedSample],
) -> EstimatorPick {
    match config.estimator {
        TeEstimatorChoice::DiscretePlugin => EstimatorPick {
            estimator: Some(TeEstimator::DiscretePlugin),
            selection: TeEstimatorSelection::RequestedDiscretePlugin,
            reason: "the caller pinned the discrete plug-in estimator".to_string(),
            n_samples: 0,
        },
        TeEstimatorChoice::ContinuousKsg => EstimatorPick {
            estimator: Some(TeEstimator::ContinuousKsg),
            selection: TeEstimatorSelection::RequestedContinuousKsg,
            reason: "the caller pinned the continuous KSG estimator".to_string(),
            n_samples: 0,
        },
        TeEstimatorChoice::Auto => {
            let (forward_total, forward_integral) = integral_coordinate_counts(forward);
            let (reverse_total, reverse_integral) = integral_coordinate_counts(reverse);
            let total = forward_total + reverse_total;
            let integral = forward_integral + reverse_integral;
            if total == 0 {
                EstimatorPick::unresolved()
            } else if integral == total {
                EstimatorPick {
                    estimator: Some(TeEstimator::DiscretePlugin),
                    selection: TeEstimatorSelection::AutoIntegralSamples,
                    reason: format!(
                        "auto: all {total} sampled coordinates are finite integers, so the continuous KSG k-th joint radius would be degenerate"
                    ),
                    n_samples: 0,
                }
            } else {
                EstimatorPick {
                    estimator: Some(TeEstimator::ContinuousKsg),
                    selection: TeEstimatorSelection::AutoRealValuedSamples,
                    reason: format!(
                        "auto: {} of {total} sampled coordinates are non-integral, so the continuous KSG estimator applies",
                        total - integral
                    ),
                    n_samples: 0,
                }
            }
        }
    }
}

pub fn transfer_entropy_with_config_cuda_strict(
    stream_a: &RecurrenceStream,
    stream_b: &RecurrenceStream,
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
) -> Result<TEResult> {
    transfer_entropy_with_config_cuda_strict_impl(stream_a, stream_b, lag, clock, config)
}

pub fn transfer_entropy_sweep(
    a: &RecurrenceStream,
    b: &RecurrenceStream,
    lags: &[usize],
    clock: &dyn Clock,
) -> Vec<TEResult> {
    transfer_entropy_sweep_with_config(a, b, lags, clock, &TransferEntropyConfig::default())
}

pub fn transfer_entropy_sweep_with_config(
    a: &RecurrenceStream,
    b: &RecurrenceStream,
    lags: &[usize],
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
) -> Vec<TEResult> {
    lags.iter()
        .map(|&lag| {
            let mut pick = EstimatorPick::unresolved();
            match transfer_entropy_inner(a, b, lag, clock, config, &mut pick) {
                Ok(result) => result,
                Err(error) => error_result(lag, config.window_size, clock, &error, &pick),
            }
        })
        .collect()
}

pub fn max_transfer_entropy_lag(results: &[TEResult]) -> Option<usize> {
    results
        .iter()
        .filter(|result| !result.provisional)
        .max_by(|left, right| left.t_a_to_b.total_cmp(&right.t_a_to_b))
        .map(|result| result.lag)
}

fn validate_config(config: &TransferEntropyConfig) -> Result<()> {
    if config.window_size == 0 || config.k == 0 || config.bootstrap_resamples == 0 {
        return Err(insufficient(
            "transfer entropy requires window_size > 0, k > 0, and bootstrap_resamples > 0",
        ));
    }
    Ok(())
}

fn provisional_result(
    lag: usize,
    window_size: usize,
    n_samples: usize,
    clock: &dyn Clock,
    pick: &EstimatorPick,
) -> TEResult {
    TEResult {
        t_a_to_b: 0.0,
        t_b_to_a: 0.0,
        dominant_direction: Direction::Unclear,
        ci_95: (0.0, 0.0),
        t_b_to_a_ci_95: (0.0, 0.0),
        difference_ci_95: (0.0, 0.0),
        lag,
        window_size,
        provisional: true,
        n_samples,
        error_code: Some(CALYX_TE_INSUFFICIENT_SAMPLES.to_string()),
        estimator: pick.estimator,
        estimator_selection: pick.selection,
        estimator_reason: pick.reason.clone(),
        trust: TrustTag::Provisional,
        computed_at: clock.now(),
    }
}

/// A failed lag still reports what was attempted: the estimator that had been
/// resolved, the sample count that reached it, and the concrete failure code.
fn error_result(
    lag: usize,
    window_size: usize,
    clock: &dyn Clock,
    error: &CalyxError,
    pick: &EstimatorPick,
) -> TEResult {
    let mut result = provisional_result(lag, window_size, pick.n_samples, clock, pick);
    result.error_code = Some(error.code.to_string());
    result.estimator_reason = format!("{}; failed: {}", pick.reason, error.message);
    result
}

#[derive(Clone, Debug)]
struct LaggedSample {
    future: Vec<f32>,
    joint_past: Vec<f32>,
    own_past: Vec<f32>,
}

fn lagged_samples(
    source: &RecurrenceStream,
    target: &RecurrenceStream,
    lag: usize,
    window_size: usize,
) -> Result<Vec<LaggedSample>> {
    let source = validated_map("source", source)?;
    let target = validated_map("target", target)?;
    let mut samples = Vec::new();
    for &time in source.keys() {
        let Some(future_time) = time.checked_add(lag as u64) else {
            continue;
        };
        let Some(&future) = target.get(&future_time) else {
            continue;
        };
        let Some(source_past) = history(&source, time, window_size) else {
            continue;
        };
        let Some(target_history_time) = future_time.checked_sub(1) else {
            continue;
        };
        let Some(target_past) = history(&target, target_history_time, window_size) else {
            continue;
        };
        let mut joint_past = source_past.clone();
        joint_past.extend_from_slice(&target_past);
        samples.push(LaggedSample {
            future: vec![future],
            joint_past,
            own_past: target_past,
        });
    }
    Ok(samples)
}

fn validated_map(
    name: &'static str,
    stream: &RecurrenceStream,
) -> Result<std::collections::BTreeMap<Timestamp, f32>> {
    let mut map = std::collections::BTreeMap::new();
    for (index, &(time, value)) in stream.iter().enumerate() {
        if !value.is_finite() {
            return Err(insufficient(format!(
                "{name} sample {index} has non-finite value"
            )));
        }
        if map.insert(time, value).is_some() {
            return Err(insufficient(format!(
                "{name} has duplicate timestamp {time}"
            )));
        }
    }
    Ok(map)
}

fn history(
    map: &std::collections::BTreeMap<Timestamp, f32>,
    time: Timestamp,
    window_size: usize,
) -> Option<Vec<f32>> {
    let start = time.checked_sub(window_size.saturating_sub(1) as u64)?;
    let mut values = Vec::with_capacity(window_size);
    for t in start..=time {
        values.push(*map.get(&t)?);
    }
    Some(values)
}

fn estimate_te(samples: &[LaggedSample], k: usize) -> Result<f32> {
    let future: Vec<_> = samples.iter().map(|sample| sample.future.clone()).collect();
    let joint_past: Vec<_> = samples
        .iter()
        .map(|sample| sample.joint_past.clone())
        .collect();
    let own_past: Vec<_> = samples
        .iter()
        .map(|sample| sample.own_past.clone())
        .collect();
    let joint = ksg_mi_continuous_point(&future, &joint_past, k)?;
    let own = ksg_mi_continuous_point(&future, &own_past, k)?;
    Ok((joint - own).max(0.0))
}

fn bootstrap_ci(
    samples: &[LaggedSample],
    point: f32,
    config: &TransferEntropyConfig,
    seed: u64,
) -> Result<(f32, f32)> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(config.bootstrap_resamples);
    for _ in 0..config.bootstrap_resamples {
        let resampled = subsample_without_replacement(samples, &mut rng);
        estimates.push(estimate_te(&resampled, config.k)?);
    }
    Ok(percentile_ci(estimates, point))
}

fn bootstrap_difference_ci(
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
        estimates.push(estimate_te(&f, config.k)? - estimate_te(&r, config.k)?);
    }
    Ok(percentile_ci(estimates, point))
}

fn subsample_without_replacement(
    samples: &[LaggedSample],
    rng: &mut ChaCha8Rng,
) -> Vec<LaggedSample> {
    subsample_indices(samples.len(), rng)
        .into_iter()
        .map(|index| samples[index].clone())
        .collect()
}

/// The one m-out-of-n subsample sizing rule, shared by both estimators so their
/// confidence intervals stay comparable.
fn subsample_indices(n: usize, rng: &mut ChaCha8Rng) -> Vec<usize> {
    let mut indices = (0..n).collect::<Vec<_>>();
    indices.shuffle(rng);
    indices.truncate((n * 4 / 5).max(MIN_ASSAY_SAMPLES).min(n));
    indices
}

fn percentile_ci(mut estimates: Vec<f32>, point: f32) -> (f32, f32) {
    estimates.sort_by(f32::total_cmp);
    let low = estimates[percentile_index(estimates.len(), 0.025)].min(point);
    let high = estimates[percentile_index(estimates.len(), 0.975)].max(point);
    (low, high)
}

fn percentile_index(len: usize, p: f32) -> usize {
    let last = len.saturating_sub(1);
    ((last as f32 * p).round() as usize).min(last)
}

fn dominant_direction(
    forward: f32,
    reverse: f32,
    forward_ci: (f32, f32),
    reverse_ci: (f32, f32),
) -> Direction {
    if forward > reverse && forward_ci.0 > reverse_ci.1 {
        Direction::AToB
    } else if reverse > forward && reverse_ci.0 > forward_ci.1 {
        Direction::BToA
    } else {
        Direction::Unclear
    }
}

fn insufficient(message: impl Into<String>) -> CalyxError {
    CalyxError::assay_insufficient_samples(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use calyx_core::FixedClock;

    /// Deterministic Bernoulli(p) draws — a small xorshift so the analytic
    /// expectations below are reproducible without a random seed dependency.
    fn bernoulli_stream(n: usize, p: f64, seed: u64) -> Vec<f32> {
        let mut state = seed | 1;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                #[allow(clippy::cast_precision_loss)]
                let uniform = (state >> 11) as f64 / (1u64 << 53) as f64;
                f32::from(uniform < p)
            })
            .collect()
    }

    fn as_stream(values: &[f32]) -> Vec<(Timestamp, f32)> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| (index as Timestamp, *value))
            .collect()
    }

    /// `B[t] = A[t-3]` over Bernoulli(0.3) bins. At lag 3 the target future is
    /// exactly the source past, so `T(A -> B) = H(0.3) = 0.8813` bits and
    /// `T(B -> A) = 0`. Every other lag is analytically 0 in both directions.
    #[test]
    fn shifted_copy_resolves_lag_three_a_to_b() {
        let a = bernoulli_stream(2000, 0.3, 0x5EED_1673);
        let mut b = vec![0.0_f32; a.len()];
        b[3..].copy_from_slice(&a[..a.len() - 3]);
        let stream_a = as_stream(&a);
        let stream_b = as_stream(&b);
        let clock = FixedClock::new(0);
        let config = TransferEntropyConfig {
            bootstrap_resamples: 64,
            ..TransferEntropyConfig::default()
        };
        let results = transfer_entropy_sweep_with_config(
            &stream_a,
            &stream_b,
            &[1, 2, 3, 4],
            &clock,
            &config,
        );
        for result in &results {
            assert_eq!(result.error_code, None, "lag {} errored", result.lag);
            assert_eq!(result.estimator, Some(TeEstimator::DiscretePlugin));
            assert_eq!(
                result.estimator_selection,
                TeEstimatorSelection::AutoIntegralSamples
            );
        }
        let best = results
            .iter()
            .max_by(|left, right| {
                (left.t_a_to_b - left.t_b_to_a)
                    .abs()
                    .total_cmp(&(right.t_a_to_b - right.t_b_to_a).abs())
            })
            .expect("a lag");
        assert_eq!(best.lag, 3);
        assert_eq!(best.dominant_direction, Direction::AToB);
        assert!(
            (best.t_a_to_b - 0.881_3).abs() < 0.02,
            "expected H(0.3)=0.8813 bits, got {}",
            best.t_a_to_b
        );
        assert!(best.t_b_to_a < 0.01, "reverse leaked {}", best.t_b_to_a);
        assert!(best.t_a_to_b - best.t_b_to_a > 0.0);
    }

    /// Two independent Bernoulli streams carry no directed information, so the
    /// estimator must report `Unclear` rather than invent a direction.
    #[test]
    fn independent_streams_stay_unclear() {
        let a = bernoulli_stream(2000, 0.3, 0x5EED_1673);
        let b = bernoulli_stream(2000, 0.3, 0x0BAD_C0DE);
        let clock = FixedClock::new(0);
        let config = TransferEntropyConfig {
            bootstrap_resamples: 64,
            ..TransferEntropyConfig::default()
        };
        let results = transfer_entropy_sweep_with_config(
            &as_stream(&a),
            &as_stream(&b),
            &[1, 2, 3, 4],
            &clock,
            &config,
        );
        for result in &results {
            assert_eq!(result.error_code, None, "lag {} errored", result.lag);
            assert_eq!(
                result.dominant_direction,
                Direction::Unclear,
                "lag {} invented a direction",
                result.lag
            );
            assert!(result.t_a_to_b < 0.01 && result.t_b_to_a < 0.01);
        }
    }

    /// Pinning the continuous estimator on the same integer-valued streams must
    /// fail loudly (degenerate KSG radius), never silently fall back.
    #[test]
    fn pinned_continuous_estimator_fails_closed_on_counts() {
        let a = bernoulli_stream(400, 0.3, 0x5EED_1673);
        let mut b = vec![0.0_f32; a.len()];
        b[3..].copy_from_slice(&a[..a.len() - 3]);
        let clock = FixedClock::new(0);
        let config = TransferEntropyConfig {
            estimator: TeEstimatorChoice::ContinuousKsg,
            bootstrap_resamples: 8,
            ..TransferEntropyConfig::default()
        };
        let results = transfer_entropy_sweep_with_config(
            &as_stream(&a),
            &as_stream(&b),
            &[3],
            &clock,
            &config,
        );
        let result = &results[0];
        assert!(result.error_code.is_some());
        assert_eq!(result.estimator, Some(TeEstimator::ContinuousKsg));
        assert_eq!(
            result.estimator_selection,
            TeEstimatorSelection::RequestedContinuousKsg
        );
        assert_eq!(result.n_samples, 397);
    }
}
