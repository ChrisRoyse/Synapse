//! Inter-event-time hazard ("overdue" anomaly) and CUSUM rate change-point
//! detection on recurrence series (PRD `26 §4` rows 3-4, PH52). Two rigorous,
//! bit-deterministic, fail-closed temporal-math primitives the cards left
//! unowned; every invalid input returns a cataloged `CALYX_ASSAY_*` error.
//!
//! 1. **Inter-event hazard** ([`inter_event_hazard`]). Gaps are modelled as a
//!    two-parameter **Gamma renewal process** (canonical renewal model — Corral
//!    2004; the hazard function uniquely defines the inter-event law) by
//!    method-of-moments `k = μ²/σ²`, `θ = σ²/μ`. The survival
//!    `S(d) = Q(k, d/θ)` (regularised upper incomplete gamma) at elapsed
//!    `d = now − t_last` is the probability the next event is not yet observed;
//!    `S(d) ≤ α` ⇒ **overdue** — the "expected recurrence didn't happen"
//!    anomaly (`25 §4b`). A perfectly regular series (CV ≈ 0) collapses to the
//!    deterministic renewal `S(d) = 1[d < μ]` — the correct model, not a
//!    fallback.
//! 2. **CUSUM rate change-point** ([`recurrence_rate_cusum`]). Page's two-sided
//!    tabular CUSUM (Page 1954; Montgomery SPC) over standardised gaps with
//!    reference `k = 0.5σ`, decision interval `h = 5σ`. An upward run flags a
//!    **slow-down** (rate ↓), a downward run a **speed-up** (rate ↑) — the
//!    drift/regime-change alarm (`17 §8`); localised to the last index the
//!    triggering cumulative left zero (standard CUSUM onset rule).

use serde::{Deserialize, Serialize};

use calyx_aster::dedup::EpochSecs;
use calyx_aster::recurrence::RecurrenceSeries;
use calyx_core::{CalyxError, Result};

use crate::estimate::TrustTag;
use crate::special_fn::{gammq, ln_gamma};

/// Minimum inter-event gaps (so ≥ this many + 1 occurrences) for a hazard fit.
pub const MIN_HAZARD_GAPS: usize = 3;
/// Minimum inter-event gaps for a CUSUM scan.
pub const MIN_CUSUM_GAPS: usize = 4;
/// Default survival threshold below which the next event is "overdue".
pub const DEFAULT_OVERDUE_ALPHA: f64 = 0.05;
/// Coefficient of variation at or below which the series is treated as a
/// deterministic (perfectly periodic) renewal process.
pub const CV_DETERMINISTIC: f64 = 1.0e-6;
/// Default CUSUM reference value `k` in σ units (detects ≥ 1σ shifts).
pub const DEFAULT_CUSUM_SLACK_K: f64 = 0.5;
/// Default CUSUM decision interval `h` in σ units.
pub const DEFAULT_CUSUM_THRESHOLD_H: f64 = 5.0;
/// Floor on the baseline σ as a fraction of the baseline mean gap, so a
/// perfectly regular baseline still yields a finite standardisation.
pub const DEFAULT_MIN_SIGMA_FRAC: f64 = 1.0e-3;

/// Fitted inter-event hazard readback (PRD `26 §4` row 3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterEventHazardReport {
    pub n_gaps: usize,
    pub mean_gap: f64,
    pub gap_variance: f64,
    /// Coefficient of variation `σ/μ` — 0 is perfectly regular, 1 is Poisson.
    pub coefficient_of_variation: f64,
    pub gamma_shape: f64,
    pub gamma_scale: f64,
    /// Whether the deterministic (CV ≈ 0) renewal branch was used.
    pub deterministic: bool,
    /// Elapsed time since the last occurrence (`now − t_last`).
    pub elapsed: f64,
    /// Modelled survival `S(elapsed) = P(gap > elapsed)` in `[0, 1]`.
    pub survival: f64,
    /// Modelled hazard rate `f(elapsed) / S(elapsed)` (finite; clamped).
    pub hazard: f64,
    /// Independent cross-check: fraction of observed gaps `≥ elapsed`.
    pub empirical_survival: f64,
    /// Mean-cadence next-occurrence estimate `t_last + μ`.
    pub expected_next: f64,
    /// Elapsed at which the modelled survival first drops to `alpha`.
    pub overdue_threshold_secs: f64,
    pub alpha: f64,
    /// `true` when `survival ≤ alpha` — the recurrence is overdue.
    pub overdue: bool,
    pub trust: TrustTag,
}

/// Direction of a detected recurrence-rate change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateShift {
    /// Gaps shrank — events recur faster (rate ↑).
    SpeedUp,
    /// Gaps grew — events recur slower (rate ↓).
    SlowDown,
}

/// A localised CUSUM change-point on the gap series.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CusumChangePoint {
    /// Gap index where the new regime is estimated to begin (the onset, i.e.
    /// the last index the triggering cumulative left zero).
    pub gap_index: usize,
    /// Occurrence index at which the new regime begins (`= gap_index`; the gap
    /// `i` spans occurrences `i` and `i+1`).
    pub occurrence_index: usize,
    /// Timestamp of `occurrence_index` — the start of the new regime.
    pub change_time: f64,
    /// Gap index at which the cumulative first exceeded `h` (alarm, ≥ onset).
    pub alarm_gap_index: usize,
    pub direction: RateShift,
    /// Cumulative statistic value at the alarm.
    pub statistic: f64,
}

/// Configuration for [`recurrence_rate_cusum_with_config`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CusumConfig {
    /// Leading gaps used to estimate the in-control mean/σ (≥ 2, `< n_gaps`).
    pub baseline_gaps: usize,
    pub slack_k: f64,
    pub threshold_h: f64,
    pub min_sigma_frac: f64,
}

impl CusumConfig {
    /// Default config with a baseline of the leading `baseline_gaps` gaps.
    pub fn with_baseline(baseline_gaps: usize) -> Self {
        Self {
            baseline_gaps,
            slack_k: DEFAULT_CUSUM_SLACK_K,
            threshold_h: DEFAULT_CUSUM_THRESHOLD_H,
            min_sigma_frac: DEFAULT_MIN_SIGMA_FRAC,
        }
    }
}

/// CUSUM rate change-point readback (PRD `26 §4` row 4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CusumReport {
    pub n_gaps: usize,
    pub baseline_gaps: usize,
    pub baseline_mean_gap: f64,
    /// Standardisation σ actually used (`max(baseline σ, floor)`).
    pub baseline_sigma: f64,
    pub slack_k: f64,
    pub threshold_h: f64,
    pub max_c_pos: f64,
    pub max_c_neg: f64,
    /// `Some` when a change-point fired; `None` when the rate held steady.
    pub change_point: Option<CusumChangePoint>,
    pub trust: TrustTag,
}

/// Inter-event hazard at `now` with the default overdue threshold.
pub fn inter_event_hazard(event_times: &[f64], now: f64) -> Result<InterEventHazardReport> {
    inter_event_hazard_with_alpha(event_times, now, DEFAULT_OVERDUE_ALPHA)
}

/// Inter-event hazard at `now` with an explicit overdue threshold `alpha`.
pub fn inter_event_hazard_with_alpha(
    event_times: &[f64],
    now: f64,
    alpha: f64,
) -> Result<InterEventHazardReport> {
    let gaps = validate_gaps(event_times, MIN_HAZARD_GAPS)?;
    if !alpha.is_finite() || !(0.0..1.0).contains(&alpha) {
        return Err(insufficient(format!(
            "overdue alpha must be finite in [0, 1), got {alpha}"
        )));
    }
    if !now.is_finite() {
        return Err(insufficient("now must be finite"));
    }
    let last = *event_times.last().expect("validated non-empty");
    let elapsed = now - last;
    if elapsed < 0.0 {
        return Err(insufficient(format!(
            "now ({now}) precedes the last occurrence ({last}); elapsed is negative"
        )));
    }

    let n = gaps.len() as f64;
    let mean_gap = gaps.iter().sum::<f64>() / n;
    let gap_variance = gaps.iter().map(|g| (g - mean_gap).powi(2)).sum::<f64>() / n;
    let cv = gap_variance.sqrt() / mean_gap;
    let empirical_survival = gaps.iter().filter(|&&g| g >= elapsed).count() as f64 / n;
    let expected_next = last + mean_gap;

    let (shape, scale, deterministic, survival, hazard, overdue_threshold) =
        if cv <= CV_DETERMINISTIC {
            // Perfectly regular renewal: a step survival at the mean cadence.
            let survival = if elapsed < mean_gap { 1.0 } else { 0.0 };
            let hazard = if elapsed < mean_gap { 0.0 } else { f64::MAX };
            (f64::INFINITY, 0.0, true, survival, hazard, mean_gap)
        } else {
            let shape = mean_gap * mean_gap / gap_variance;
            let scale = gap_variance / mean_gap;
            let survival = gammq(shape, elapsed / scale)?;
            let density = gamma_pdf(elapsed, shape, scale)?;
            // Clamp the survival denominator so the hazard stays finite even
            // deep in the tail; an overdue event is flagged by `overdue`, not by
            // a NaN/inf hazard.
            let hazard = density / survival.max(f64::MIN_POSITIVE);
            let threshold = survival_quantile(shape, scale, alpha)?;
            (shape, scale, false, survival, hazard, threshold)
        };

    Ok(InterEventHazardReport {
        n_gaps: gaps.len(),
        mean_gap,
        gap_variance,
        coefficient_of_variation: cv,
        gamma_shape: shape,
        gamma_scale: scale,
        deterministic,
        elapsed,
        survival,
        hazard,
        empirical_survival,
        expected_next,
        overdue_threshold_secs: overdue_threshold,
        alpha,
        overdue: survival <= alpha,
        trust: TrustTag::Provisional,
    })
}

/// CUSUM rate change-point with the leading half of the gaps as the baseline.
pub fn recurrence_rate_cusum(event_times: &[f64]) -> Result<CusumReport> {
    let gaps = validate_gaps(event_times, MIN_CUSUM_GAPS)?;
    let baseline = (gaps.len() / 2).max(2);
    recurrence_rate_cusum_with_config(event_times, &CusumConfig::with_baseline(baseline))
}

/// CUSUM rate change-point over an explicit configuration.
pub fn recurrence_rate_cusum_with_config(
    event_times: &[f64],
    config: &CusumConfig,
) -> Result<CusumReport> {
    let gaps = validate_gaps(event_times, MIN_CUSUM_GAPS)?;
    if config.baseline_gaps < 2 || config.baseline_gaps >= gaps.len() {
        return Err(insufficient(format!(
            "baseline_gaps must be in [2, {}), got {}",
            gaps.len(),
            config.baseline_gaps
        )));
    }
    for (name, value) in [
        ("slack_k", config.slack_k),
        ("threshold_h", config.threshold_h),
        ("min_sigma_frac", config.min_sigma_frac),
    ] {
        if !value.is_finite() || value <= 0.0 {
            return Err(insufficient(format!(
                "cusum {name} must be finite and positive, got {value}"
            )));
        }
    }

    let baseline = &gaps[..config.baseline_gaps];
    let bn = baseline.len() as f64;
    let mu0 = baseline.iter().sum::<f64>() / bn;
    let var0 = baseline.iter().map(|g| (g - mu0).powi(2)).sum::<f64>() / bn;
    let sigma = var0.sqrt().max(config.min_sigma_frac * mu0);
    if !sigma.is_finite() || sigma <= 0.0 {
        return Err(insufficient(
            "baseline σ is non-finite or zero; cannot standardise the gap series",
        ));
    }

    let (k, h) = (config.slack_k, config.threshold_h);
    let (mut c_pos, mut c_neg) = (0.0_f64, 0.0_f64);
    let (mut max_c_pos, mut max_c_neg) = (0.0_f64, 0.0_f64);
    // Onset candidates: the index just after the cumulative last hit zero.
    let (mut pos_onset, mut neg_onset) = (0_usize, 0_usize);
    let mut change_point: Option<CusumChangePoint> = None;

    for (i, &gap) in gaps.iter().enumerate() {
        let z = (gap - mu0) / sigma;
        c_pos = (c_pos + z - k).max(0.0);
        c_neg = (c_neg - z - k).max(0.0);
        if c_pos == 0.0 {
            pos_onset = i + 1;
        }
        if c_neg == 0.0 {
            neg_onset = i + 1;
        }
        max_c_pos = max_c_pos.max(c_pos);
        max_c_neg = max_c_neg.max(c_neg);
        if change_point.is_none() {
            if c_pos > h {
                change_point = Some(localize(
                    event_times,
                    pos_onset,
                    i,
                    RateShift::SlowDown,
                    c_pos,
                ));
            } else if c_neg > h {
                change_point = Some(localize(
                    event_times,
                    neg_onset,
                    i,
                    RateShift::SpeedUp,
                    c_neg,
                ));
            }
        }
    }

    Ok(CusumReport {
        n_gaps: gaps.len(),
        baseline_gaps: config.baseline_gaps,
        baseline_mean_gap: mu0,
        baseline_sigma: sigma,
        slack_k: k,
        threshold_h: h,
        max_c_pos,
        max_c_neg,
        change_point,
        trust: TrustTag::Provisional,
    })
}

/// Inter-event hazard read directly from a vault recurrence series.
///
/// The occurrences are the on-disk source of truth (the Aster Recurrence CF);
/// this bridges them to [`inter_event_hazard`].
pub fn inter_event_hazard_from_series(
    series: &RecurrenceSeries,
    now: EpochSecs,
    alpha: f64,
) -> Result<InterEventHazardReport> {
    inter_event_hazard_with_alpha(&series_times(series), now.0 as f64, alpha)
}

/// CUSUM rate change-point read directly from a vault recurrence series.
pub fn recurrence_rate_cusum_from_series(
    series: &RecurrenceSeries,
    config: &CusumConfig,
) -> Result<CusumReport> {
    recurrence_rate_cusum_with_config(&series_times(series), config)
}

fn series_times(series: &RecurrenceSeries) -> Vec<f64> {
    let mut times: Vec<f64> = series
        .occurrences
        .iter()
        .map(|occurrence| occurrence.t_k.0 as f64)
        .collect();
    times.sort_by(f64::total_cmp);
    times
}

fn localize(
    event_times: &[f64],
    onset: usize,
    alarm: usize,
    direction: RateShift,
    statistic: f64,
) -> CusumChangePoint {
    // The onset is the start of the run; clamp to the alarm so it never points
    // past the change. The change-time is the absolute timestamp of the
    // occurrence where the new regime begins (gap `i` spans occ `i`..`i+1`).
    let gap_index = onset.min(alarm);
    CusumChangePoint {
        gap_index,
        occurrence_index: gap_index,
        change_time: event_times[gap_index],
        alarm_gap_index: alarm,
        direction,
        statistic,
    }
}

fn validate_gaps(event_times: &[f64], min_gaps: usize) -> Result<Vec<f64>> {
    if event_times.len() < min_gaps + 1 {
        return Err(insufficient(format!(
            "recurrence hazard needs ≥ {} occurrences, got {}",
            min_gaps + 1,
            event_times.len()
        )));
    }
    for (index, &time) in event_times.iter().enumerate() {
        if !time.is_finite() {
            return Err(insufficient(format!(
                "occurrence {index} is NaN or infinite"
            )));
        }
    }
    let mut gaps = Vec::with_capacity(event_times.len() - 1);
    for (index, pair) in event_times.windows(2).enumerate() {
        let gap = pair[1] - pair[0];
        if gap <= 0.0 {
            // An ordering violation is not a sample-count shortfall: no amount
            // of additional data changes it, so it carries its own code and
            // names the offending instants instead of telling the operator to
            // anchor more outcomes (issue #1893).
            return Err(not_monotonic(event_times, index));
        }
        gaps.push(gap);
    }
    Ok(gaps)
}

/// Builds `CALYX_ASSAY_OCCURRENCES_NOT_MONOTONIC` naming the offending instants.
///
/// `first_violation` is the index `i` where `t[i+1] - t[i] <= 0`. The message
/// distinguishes a tie from a descending step, quotes up to
/// [`NOT_MONOTONIC_REPORTED_VIOLATIONS`] offending `(index, t[i], t[i+1])`
/// triples, and states the total violation count, so the diagnosis needs no
/// code read.
fn not_monotonic(event_times: &[f64], first_violation: usize) -> CalyxError {
    let violations: Vec<(usize, f64, f64)> = event_times
        .windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[1] - pair[0] <= 0.0)
        .map(|(index, pair)| (index, pair[0], pair[1]))
        .collect();
    let tied = violations.iter().filter(|(_, a, b)| a == b).count();
    let descending = violations.len() - tied;
    let quoted = violations
        .iter()
        .take(NOT_MONOTONIC_REPORTED_VIOLATIONS)
        .map(|(index, a, b)| {
            let kind = if a == b { "tied" } else { "descending" };
            format!("[{index}]={a} -> [{}]={b} ({kind})", index + 1)
        })
        .collect::<Vec<_>>()
        .join(", ");
    CalyxError::assay_occurrences_not_monotonic(format!(
        "occurrence times must be strictly increasing; {} of {} adjacent pairs violate that \
         ({tied} tied, {descending} descending), first at index {first_violation}; first {} \
         violation(s): {quoted}",
        violations.len(),
        event_times.len().saturating_sub(1),
        violations.len().min(NOT_MONOTONIC_REPORTED_VIOLATIONS),
    ))
}

/// How many offending adjacent pairs an ordering error quotes verbatim.
pub const NOT_MONOTONIC_REPORTED_VIOLATIONS: usize = 3;

/// Deterministic collapse of tied occurrence instants (issue #1893).
///
/// The renewal and CUSUM estimators model the times **at which** a recurrence
/// occurred, so their gap series is only defined over *distinct* instants — an
/// orderly point process admits no tied event times (Meyer 2010, §"Breaking up
/// tied event times"). Real corpora tie anyway: Synapse writes several agent
/// events at one identical nanosecond (they share a commit), which is why the
/// `CF_AGENT_EVENTS` key codec carries a `seq` disambiguator.
///
/// The survival/point-process literature offers two accepted responses —
/// jitter the ties, or collapse them and treat time as discrete. **Jitter is
/// rejected here**: these primitives are contractually bit-deterministic, and a
/// random tie-break makes the same corpus yield different change-points across
/// runs. So tied instants collapse to one occurrence carrying its multiplicity.
///
/// Nothing is dropped silently: the returned report carries the input count, the
/// distinct-instant count, how many occurrences were absorbed, and the largest
/// multiplicity seen, for the caller to surface in its readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TiedOccurrenceCollapse {
    /// Distinct occurrence instants, strictly increasing.
    pub instants: Vec<f64>,
    /// Multiplicity of each entry in `instants` (parallel, ≥ 1).
    pub multiplicities: Vec<usize>,
    /// Occurrences supplied by the caller.
    pub n_occurrences: usize,
    /// Distinct instants after the collapse (`instants.len()`).
    pub n_distinct_instants: usize,
    /// Occurrences absorbed into an earlier instant
    /// (`n_occurrences - n_distinct_instants`).
    pub ties_collapsed: usize,
    /// Largest number of occurrences sharing one instant (1 when none tied).
    pub max_multiplicity: usize,
}

/// Collapses tied instants in an ascending occurrence series.
///
/// # Errors
///
/// Returns `CALYX_ASSAY_OCCURRENCES_NOT_MONOTONIC` when `times` is not sorted
/// ascending (a descending step is a caller defect that collapsing must not
/// mask), and `CALYX_ASSAY_INSUFFICIENT_SAMPLES` when a value is not finite or
/// the series is empty.
pub fn collapse_tied_occurrences(times: &[f64]) -> Result<TiedOccurrenceCollapse> {
    if times.is_empty() {
        return Err(insufficient(
            "the occurrence series is empty; nothing to collapse",
        ));
    }
    for (index, &time) in times.iter().enumerate() {
        if !time.is_finite() {
            return Err(insufficient(format!(
                "occurrence {index} is NaN or infinite"
            )));
        }
    }
    let mut instants: Vec<f64> = Vec::with_capacity(times.len());
    let mut multiplicities: Vec<usize> = Vec::with_capacity(times.len());
    for (index, &time) in times.iter().enumerate() {
        match instants.last().copied() {
            Some(previous) if time == previous => {
                *multiplicities
                    .last_mut()
                    .expect("instants and multiplicities stay parallel") += 1;
            }
            Some(previous) if time < previous => {
                // Collapsing must never hide unsortedness; that is a distinct
                // defect with a distinct cause.
                return Err(not_monotonic(times, index.saturating_sub(1)));
            }
            _ => {
                instants.push(time);
                multiplicities.push(1);
            }
        }
    }
    let n_distinct_instants = instants.len();
    let max_multiplicity = multiplicities.iter().copied().max().unwrap_or(1);
    Ok(TiedOccurrenceCollapse {
        instants,
        multiplicities,
        n_occurrences: times.len(),
        n_distinct_instants,
        ties_collapsed: times.len() - n_distinct_instants,
        max_multiplicity,
    })
}

/// Gamma pdf `f(x; k, θ) = x^{k-1} e^{-x/θ} / (θ^k Γ(k))`.
fn gamma_pdf(x: f64, shape: f64, scale: f64) -> Result<f64> {
    if x <= 0.0 {
        return Ok(0.0);
    }
    Ok(((shape - 1.0) * x.ln() - x / scale - shape * scale.ln() - ln_gamma(shape)).exp())
}

/// Elapsed time at which the modelled survival first drops to `alpha`, by
/// bisection on the monotone-decreasing survival (no inverse-gamma needed).
fn survival_quantile(shape: f64, scale: f64, alpha: f64) -> Result<f64> {
    let (mut lo, mut hi) = (0.0_f64, shape * scale);
    // Expand the upper bound until its survival is below alpha.
    let mut guard = 0;
    while gammq(shape, hi / scale)? > alpha {
        hi *= 2.0;
        guard += 1;
        if guard > 200 || !hi.is_finite() {
            return Err(insufficient(
                "survival quantile failed to bracket; degenerate gamma fit",
            ));
        }
    }
    for _ in 0..100 {
        let mid = 0.5 * (lo + hi);
        if gammq(shape, mid / scale)? > alpha {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(0.5 * (lo + hi))
}

fn insufficient(message: impl Into<String>) -> CalyxError {
    CalyxError::assay_insufficient_samples(message)
}
