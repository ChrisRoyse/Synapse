//! Discrete plug-in transfer entropy with a Miller-Madow bias correction.
//!
//! # Why this module exists (issue #1673)
//!
//! The sibling continuous path estimates the two mutual-information terms with
//! the KSG k-nearest-neighbour estimator (Kraskov, Stögbauer & Grassberger,
//! *Estimating mutual information*, Phys. Rev. E **69**, 066138, 2004,
//! `arXiv:cond-mat/0305641`). That estimator is written for samples drawn from a
//! joint probability *density*: it reads the k-th nearest-neighbour radius in
//! the joint space and counts marginal neighbours inside it. When k or more
//! samples coincide exactly the k-th joint radius is zero and the estimator is
//! undefined — the KSG authors themselves note that such degeneracies have to be
//! broken by adding low-amplitude noise, which would be fabricating data here.
//! Binned occurrence counts are integer-valued and mostly 0/1, so *every* joint
//! radius collapses and the continuous path fails closed on every lag.
//!
//! Transfer entropy is not a continuous-only quantity. Schreiber defined it on
//! discrete transition probabilities (*Measuring information transfer*,
//! Phys. Rev. Lett. **85**, 461, 2000, doi:10.1103/PhysRevLett.85.461):
//!
//! ```text
//! T(X -> Y) = H(Yf | Yp) - H(Yf | Yp, Xp)
//!           = H(Yf, Yp) + H(Xp, Yp) - H(Yp) - H(Yf, Yp, Xp)
//! ```
//!
//! where `Yf` is the target future, `Yp` the target history and `Xp` the source
//! history. Each of the four terms is the plug-in (maximum-likelihood) Shannon
//! entropy of an observed joint symbol table, computed with the crate's existing
//! [`entropy_bits`].
//!
//! # Bias correction
//!
//! The plug-in entropy under-estimates `H` by roughly `(K - 1) / (2 N)` nats,
//! where `K` is the number of *occupied* states and `N` the sample count. The
//! Miller-Madow correction adds that term back to each entropy estimate; in bits
//! it is `(K - 1) / (2 N ln 2)`. Applied to the four-term decomposition the net
//! correction is
//!
//! ```text
//! (K_yf_yp + K_xp_yp - K_yp - K_yf_yp_xp) / (2 N ln 2)
//! ```
//!
//! which is exactly the leading-order positive bias of the plug-in transfer
//! entropy with the sign flipped: for two independent binary streams with a
//! one-bin history the correction is `(4 + 4 - 2 - 8) / (2 N ln 2)`, cancelling
//! the `2 / (2 N ln 2)` plug-in bias to first order. That is what makes an
//! independent pair report ~0 bits instead of a small invented signal.
//!
//! # Quorum, not a guess
//!
//! A plug-in estimate over a state space the sample cannot populate is noise
//! dressed as a number. This module therefore refuses — with
//! [`CALYX_TE_DISCRETE_STATE_QUORUM`] — whenever the sample count is below
//! [`MIN_TE_DISCRETE_SAMPLES_PER_STATE`] observations per *occupied* joint
//! state, and refuses with [`CALYX_TE_DISCRETE_ALPHABET_TOO_LARGE`] when a
//! coordinate's observed alphabet exceeds [`MAX_TE_DISCRETE_ALPHABET`] (which
//! also keeps the composed state codes inside `u64`). It never falls back to a
//! different estimator.

use std::collections::BTreeMap;

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use calyx_core::{CalyxError, Clock, Result};

use super::{
    EstimatorPick, LaggedSample, TEResult, TeEstimator, TransferEntropyConfig, dominant_direction,
    percentile_ci, subsample_indices,
};
use crate::estimate::TrustTag;
use crate::sufficiency::entropy_bits;

/// A coordinate's observed alphabet is too large for a plug-in estimate.
pub const CALYX_TE_DISCRETE_ALPHABET_TOO_LARGE: &str = "CALYX_TE_DISCRETE_ALPHABET_TOO_LARGE";
/// Too few observations per occupied joint state for a plug-in estimate.
pub const CALYX_TE_DISCRETE_STATE_QUORUM: &str = "CALYX_TE_DISCRETE_STATE_QUORUM";
/// A lagged sample carried a non-finite coordinate.
pub const CALYX_TE_DISCRETE_NON_FINITE_SAMPLE: &str = "CALYX_TE_DISCRETE_NON_FINITE_SAMPLE";
/// A lagged sample's joint history is shorter than its own history.
pub const CALYX_TE_DISCRETE_MALFORMED_SAMPLE: &str = "CALYX_TE_DISCRETE_MALFORMED_SAMPLE";
/// Minimum observations per *occupied* joint `(Yf, Yp, Xp)` state.
///
/// Five is the classical expected-count floor for a multinomial cell; below it
/// the plug-in entropy of that cell is dominated by its own sampling noise.
pub const MIN_TE_DISCRETE_SAMPLES_PER_STATE: usize = 5;

/// Maximum observed alphabet per coordinate group (`Yf`, `Yp`, `Xp`).
pub const MAX_TE_DISCRETE_ALPHABET: usize = 64;

/// One direction's lagged samples reduced to dense integer state codes.
#[derive(Debug)]
pub(super) struct DiscreteStreams {
    target_future: Vec<u64>,
    target_past: Vec<u64>,
    source_past: Vec<u64>,
    #[cfg(feature = "cuda")]
    target_future_states: u64,
    target_past_states: u64,
    source_past_states: u64,
}

impl DiscreteStreams {
    fn len(&self) -> usize {
        self.target_future.len()
    }
}

/// Estimates transfer entropy in both directions with the discrete plug-in
/// estimator and the shared subsample bootstrap.
pub(super) fn transfer_entropy_discrete(
    forward: &[LaggedSample],
    reverse: &[LaggedSample],
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
    pick: &EstimatorPick,
) -> Result<TEResult> {
    let n_samples = forward.len().min(reverse.len());
    let forward = symbolize("A -> B", forward)?;
    let reverse = symbolize("B -> A", reverse)?;
    let all: Vec<usize> = (0..n_samples).collect();
    let t_a_to_b = te_bits(&forward, &all);
    let t_b_to_a = te_bits(&reverse, &all);
    let ci_95 = bootstrap_ci(&forward, t_a_to_b, config, config.bootstrap_seed);
    let t_b_to_a_ci_95 = bootstrap_ci(
        &reverse,
        t_b_to_a,
        config,
        config.bootstrap_seed ^ 0x0B17_B1D5,
    );
    let difference_ci_95 = bootstrap_difference_ci(
        &forward,
        &reverse,
        t_a_to_b - t_b_to_a,
        config,
        config.bootstrap_seed ^ 0x00D1_FFC1,
    );
    Ok(TEResult {
        t_a_to_b,
        t_b_to_a,
        dominant_direction: dominant_direction(t_a_to_b, t_b_to_a, ci_95, t_b_to_a_ci_95),
        ci_95,
        t_b_to_a_ci_95,
        difference_ci_95,
        lag,
        window_size: config.window_size,
        provisional: false,
        n_samples,
        error_code: None,
        estimator: Some(TeEstimator::DiscretePlugin),
        estimator_selection: pick.selection,
        estimator_reason: pick.reason.clone(),
        trust: TrustTag::Provisional,
        computed_at: clock.now(),
    })
}

/// Reports the fraction of coordinates that are finite integers, which is the
/// property that decides whether the KSG joint radius can be non-degenerate.
pub(super) fn integral_coordinate_counts(samples: &[LaggedSample]) -> (usize, usize) {
    let mut total = 0usize;
    let mut integral = 0usize;
    for sample in samples {
        for value in sample.future.iter().chain(sample.joint_past.iter()) {
            total += 1;
            if value.is_finite() && *value == value.trunc() {
                integral += 1;
            }
        }
    }
    (total, integral)
}

/// Maps each coordinate group of every lagged sample to a dense state code and
/// enforces the alphabet and per-state quorum gates.
fn symbolize(direction: &str, samples: &[LaggedSample]) -> Result<DiscreteStreams> {
    let n = samples.len();
    let mut future_table = SymbolTable::default();
    let mut own_table = SymbolTable::default();
    let mut source_table = SymbolTable::default();
    let mut target_future = Vec::with_capacity(n);
    let mut target_past = Vec::with_capacity(n);
    let mut source_past = Vec::with_capacity(n);
    for (index, sample) in samples.iter().enumerate() {
        if sample.joint_past.len() < sample.own_past.len() {
            return Err(malformed(format!(
                "{direction} lagged sample {index} has joint history {} shorter than own history {}",
                sample.joint_past.len(),
                sample.own_past.len()
            )));
        }
        let source_len = sample.joint_past.len() - sample.own_past.len();
        target_future.push(future_table.intern(direction, index, &sample.future)?);
        target_past.push(own_table.intern(direction, index, &sample.own_past)?);
        source_past.push(source_table.intern(
            direction,
            index,
            &sample.joint_past[..source_len],
        )?);
    }
    check_alphabet(direction, "target future", future_table.len())?;
    check_alphabet(direction, "target history", own_table.len())?;
    check_alphabet(direction, "source history", source_table.len())?;
    let streams = DiscreteStreams {
        target_future,
        target_past,
        source_past,
        #[cfg(feature = "cuda")]
        target_future_states: future_table.len() as u64,
        target_past_states: own_table.len() as u64,
        source_past_states: source_table.len() as u64,
    };
    let all: Vec<usize> = (0..n).collect();
    let occupied = distinct_count(&joint_codes(&streams, &all));
    let required = occupied.saturating_mul(MIN_TE_DISCRETE_SAMPLES_PER_STATE);
    if n < required {
        return Err(CalyxError {
            code: CALYX_TE_DISCRETE_STATE_QUORUM,
            message: format!(
                "discrete transfer entropy {direction} needs >= {MIN_TE_DISCRETE_SAMPLES_PER_STATE} samples per occupied joint state: {occupied} states occupied, {required} samples required, {n} available"
            ),
            remediation: "widen the bin so fewer distinct counts occur, shorten window_size, or supply more bins",
        });
    }
    Ok(streams)
}

fn check_alphabet(direction: &str, coordinate: &str, observed: usize) -> Result<()> {
    if observed > MAX_TE_DISCRETE_ALPHABET {
        return Err(CalyxError {
            code: CALYX_TE_DISCRETE_ALPHABET_TOO_LARGE,
            message: format!(
                "discrete transfer entropy {direction} observed {observed} distinct {coordinate} states, above the {MAX_TE_DISCRETE_ALPHABET} plug-in ceiling"
            ),
            remediation: "widen the bin so fewer distinct counts occur, shorten window_size, or pin the continuous KSG estimator",
        });
    }
    Ok(())
}

/// Plug-in transfer entropy in bits over the selected sample indices, with the
/// Miller-Madow correction applied to every entropy term.
fn te_bits(streams: &DiscreteStreams, indices: &[usize]) -> f32 {
    let n = indices.len();
    if n == 0 {
        return 0.0;
    }
    let future_past = gather(indices, |index| {
        streams.target_future[index] * streams.target_past_states + streams.target_past[index]
    });
    let source_target_past = gather(indices, |index| {
        streams.source_past[index] * streams.target_past_states + streams.target_past[index]
    });
    let own_past = gather(indices, |index| streams.target_past[index]);
    let joint = joint_codes(streams, indices);
    let h_future_past = corrected_entropy_bits(&future_past, n);
    let h_source_target_past = corrected_entropy_bits(&source_target_past, n);
    let h_own_past = corrected_entropy_bits(&own_past, n);
    let h_joint = corrected_entropy_bits(&joint, n);
    (h_future_past + h_source_target_past - h_own_past - h_joint).max(0.0)
}

fn joint_codes(streams: &DiscreteStreams, indices: &[usize]) -> Vec<u64> {
    gather(indices, |index| {
        (streams.target_future[index] * streams.target_past_states + streams.target_past[index])
            * streams.source_past_states
            + streams.source_past[index]
    })
}

fn gather(indices: &[usize], code: impl Fn(usize) -> u64) -> Vec<u64> {
    indices.iter().map(|&index| code(index)).collect()
}

/// Plug-in Shannon entropy in bits plus the Miller-Madow `(K - 1) / (2 N ln 2)`
/// first-order bias correction over the occupied support `K`.
fn corrected_entropy_bits(codes: &[u64], n: usize) -> f32 {
    entropy_bits(codes) + miller_madow_bits(distinct_count(codes), n)
}

fn miller_madow_bits(support: usize, n: usize) -> f32 {
    if n == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let support = support.saturating_sub(1) as f32;
    #[allow(clippy::cast_precision_loss)]
    let n = n as f32;
    support / (2.0 * n * std::f32::consts::LN_2)
}

fn distinct_count(codes: &[u64]) -> usize {
    let mut sorted = codes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.len()
}

fn bootstrap_ci(
    streams: &DiscreteStreams,
    point: f32,
    config: &TransferEntropyConfig,
    seed: u64,
) -> (f32, f32) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(config.bootstrap_resamples);
    for _ in 0..config.bootstrap_resamples {
        let indices = subsample_indices(streams.len(), &mut rng);
        estimates.push(te_bits(streams, &indices));
    }
    percentile_ci(estimates, point)
}

fn bootstrap_difference_ci(
    forward: &DiscreteStreams,
    reverse: &DiscreteStreams,
    point: f32,
    config: &TransferEntropyConfig,
    seed: u64,
) -> (f32, f32) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(config.bootstrap_resamples);
    for _ in 0..config.bootstrap_resamples {
        let forward_indices = subsample_indices(forward.len(), &mut rng);
        let reverse_indices = subsample_indices(reverse.len(), &mut rng);
        estimates.push(te_bits(forward, &forward_indices) - te_bits(reverse, &reverse_indices));
    }
    percentile_ci(estimates, point)
}

/// Interns exact coordinate tuples as dense state ids. Values are matched by
/// canonical IEEE bits, so no binning, rounding or noise is invented.
#[derive(Default)]
struct SymbolTable {
    ids: BTreeMap<Vec<u32>, u64>,
}

impl SymbolTable {
    fn intern(&mut self, direction: &str, index: usize, values: &[f32]) -> Result<u64> {
        let mut key = Vec::with_capacity(values.len());
        for value in values {
            if !value.is_finite() {
                return Err(CalyxError {
                    code: CALYX_TE_DISCRETE_NON_FINITE_SAMPLE,
                    message: format!(
                        "discrete transfer entropy {direction} sample {index} has a non-finite coordinate"
                    ),
                    remediation: "repair the source stream so every binned value is finite",
                });
            }
            // Canonicalise -0.0 to +0.0 so the two spellings share one symbol.
            let canonical = if *value == 0.0 { 0.0_f32 } else { *value };
            key.push(canonical.to_bits());
        }
        let next = self.ids.len() as u64;
        Ok(*self.ids.entry(key).or_insert(next))
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

fn malformed(message: String) -> CalyxError {
    CalyxError {
        code: CALYX_TE_DISCRETE_MALFORMED_SAMPLE,
        message,
        remediation: "report this: the lagged-sample builder produced inconsistent history lengths",
    }
}

/// Native strict-CUDA implementation of the same discrete plug-in estimator.
/// Symbol interning and deterministic bootstrap-index generation are control
/// work; every entropy histogram and Miller-Madow reduction is executed by the
/// CUDA kernel. There is no CPU estimator retry and no KSG substitution.
#[cfg(feature = "cuda")]
pub(super) fn transfer_entropy_discrete_cuda(
    ctx: &calyx_forge::CudaContext,
    forward: &[LaggedSample],
    reverse: &[LaggedSample],
    lag: usize,
    clock: &dyn Clock,
    config: &TransferEntropyConfig,
    pick: &EstimatorPick,
) -> Result<TEResult> {
    let n_samples = forward.len().min(reverse.len());
    let forward = symbolize("A -> B", &forward[..n_samples])?;
    let reverse = symbolize("B -> A", &reverse[..n_samples])?;
    let all = (0..n_samples)
        .map(|index| i32::try_from(index).map_err(|_| cuda_index_overflow(index)))
        .collect::<Result<Vec<_>>>()?;
    let t_a_to_b = cuda_estimates(ctx, &forward, &all, n_samples)?[0];
    let t_b_to_a = cuda_estimates(ctx, &reverse, &all, n_samples)?[0];

    let selection_len = (n_samples
        .checked_mul(4)
        .ok_or_else(|| cuda_size_overflow("subsample length"))?
        / 5)
    .max(super::MIN_ASSAY_SAMPLES)
    .min(n_samples);
    let selection_capacity = config
        .bootstrap_resamples
        .checked_mul(2)
        .and_then(|count| count.checked_mul(selection_len))
        .ok_or_else(|| cuda_size_overflow("bootstrap selection capacity"))?;
    let mut forward_selections = Vec::with_capacity(selection_capacity);
    let mut reverse_selections = Vec::with_capacity(selection_capacity);
    let mut forward_rng = ChaCha8Rng::seed_from_u64(config.bootstrap_seed);
    for _ in 0..config.bootstrap_resamples {
        append_cuda_selection(
            &mut forward_selections,
            super::subsample_indices(n_samples, &mut forward_rng),
        )?;
    }
    let mut reverse_rng = ChaCha8Rng::seed_from_u64(config.bootstrap_seed ^ 0x0B17_B1D5);
    for _ in 0..config.bootstrap_resamples {
        append_cuda_selection(
            &mut reverse_selections,
            super::subsample_indices(n_samples, &mut reverse_rng),
        )?;
    }
    let mut difference_rng = ChaCha8Rng::seed_from_u64(config.bootstrap_seed ^ 0x00D1_FFC1);
    for _ in 0..config.bootstrap_resamples {
        append_cuda_selection(
            &mut forward_selections,
            super::subsample_indices(n_samples, &mut difference_rng),
        )?;
        append_cuda_selection(
            &mut reverse_selections,
            super::subsample_indices(n_samples, &mut difference_rng),
        )?;
    }
    let forward_estimates = cuda_estimates(ctx, &forward, &forward_selections, selection_len)?;
    let reverse_estimates = cuda_estimates(ctx, &reverse, &reverse_selections, selection_len)?;
    let split = config.bootstrap_resamples;
    let ci_95 = percentile_ci(forward_estimates[..split].to_vec(), t_a_to_b);
    let t_b_to_a_ci_95 = percentile_ci(reverse_estimates[..split].to_vec(), t_b_to_a);
    let difference_estimates = forward_estimates[split..]
        .iter()
        .zip(&reverse_estimates[split..])
        .map(|(left, right)| left - right)
        .collect();
    let difference_ci_95 = percentile_ci(difference_estimates, t_a_to_b - t_b_to_a);
    Ok(TEResult {
        t_a_to_b,
        t_b_to_a,
        dominant_direction: dominant_direction(t_a_to_b, t_b_to_a, ci_95, t_b_to_a_ci_95),
        ci_95,
        t_b_to_a_ci_95,
        difference_ci_95,
        lag,
        window_size: config.window_size,
        provisional: false,
        n_samples,
        error_code: None,
        estimator: Some(TeEstimator::DiscretePlugin),
        estimator_selection: pick.selection,
        estimator_reason: format!(
            "{}; executed by the native strict-CUDA dense-histogram Miller-Madow kernel",
            pick.reason
        ),
        trust: TrustTag::Provisional,
        computed_at: clock.now(),
    })
}

#[cfg(feature = "cuda")]
fn cuda_estimates(
    ctx: &calyx_forge::CudaContext,
    streams: &DiscreteStreams,
    selections: &[i32],
    selection_len: usize,
) -> Result<Vec<f32>> {
    let (tables, bin_counts) = cuda_code_tables(streams)?;
    calyx_forge::cuda::discrete_te_batch_host(
        ctx,
        &tables,
        streams.len(),
        bin_counts,
        selections,
        selection_len,
    )
    .map(|batch| batch.estimates)
    .map_err(|error| crate::cuda_strict::forge_to_calyx("discrete transfer entropy", error))
}

#[cfg(feature = "cuda")]
fn cuda_code_tables(streams: &DiscreteStreams) -> Result<(Vec<u32>, [usize; 4])> {
    let all: Vec<usize> = (0..streams.len()).collect();
    let future_past = gather(&all, |index| {
        streams.target_future[index] * streams.target_past_states + streams.target_past[index]
    });
    let source_target_past = gather(&all, |index| {
        streams.source_past[index] * streams.target_past_states + streams.target_past[index]
    });
    let own_past = gather(&all, |index| streams.target_past[index]);
    let joint = joint_codes(streams, &all);
    let table_capacity = streams
        .len()
        .checked_mul(4)
        .ok_or_else(|| cuda_size_overflow("entropy code-table capacity"))?;
    let mut tables = Vec::with_capacity(table_capacity);
    for code in future_past
        .into_iter()
        .chain(source_target_past)
        .chain(own_past)
        .chain(joint)
    {
        tables.push(u32::try_from(code).map_err(|_| cuda_code_overflow(code))?);
    }
    let bin_counts_u64 = [
        streams
            .target_future_states
            .checked_mul(streams.target_past_states),
        streams
            .source_past_states
            .checked_mul(streams.target_past_states),
        Some(streams.target_past_states),
        streams
            .target_future_states
            .checked_mul(streams.target_past_states)
            .and_then(|value| value.checked_mul(streams.source_past_states)),
    ];
    let mut bin_counts = [0usize; 4];
    for (index, count) in bin_counts_u64.into_iter().enumerate() {
        let count = count.ok_or_else(|| cuda_code_overflow(u64::MAX))?;
        bin_counts[index] = usize::try_from(count).map_err(|_| cuda_code_overflow(count))?;
    }
    Ok((tables, bin_counts))
}

#[cfg(feature = "cuda")]
fn append_cuda_selection(destination: &mut Vec<i32>, indices: Vec<usize>) -> Result<()> {
    for index in indices {
        destination.push(i32::try_from(index).map_err(|_| cuda_index_overflow(index))?);
    }
    Ok(())
}

#[cfg(feature = "cuda")]
fn cuda_index_overflow(index: usize) -> CalyxError {
    CalyxError {
        code: CALYX_TE_DISCRETE_MALFORMED_SAMPLE,
        message: format!("discrete transfer-entropy sample index {index} exceeds CUDA i32"),
        remediation: "reduce the bounded temporal sample window below the CUDA index ceiling",
    }
}

#[cfg(feature = "cuda")]
fn cuda_code_overflow(code: u64) -> CalyxError {
    CalyxError {
        code: CALYX_TE_DISCRETE_ALPHABET_TOO_LARGE,
        message: format!("discrete transfer-entropy dense state code {code} exceeds CUDA bounds"),
        remediation: "widen temporal bins or shorten the declared history window",
    }
}

#[cfg(feature = "cuda")]
fn cuda_size_overflow(context: &str) -> CalyxError {
    CalyxError {
        code: CALYX_TE_DISCRETE_MALFORMED_SAMPLE,
        message: format!("discrete transfer-entropy {context} overflowed usize"),
        remediation: "reduce bootstrap_resamples or the bounded temporal sample window",
    }
}
