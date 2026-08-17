//! Estimator selection for mutual information about a discrete outcome.
//!
//! [`ksg`](crate::ksg) estimates `I(X; label)` with the Kraskov-Stögbauer-
//! Grassberger k-nearest-neighbour rule, which is only defined when the k-th
//! neighbour of every sample lies at a **strictly positive** radius. A lens
//! whose output is categorical by construction — a one-hot, a signed hash, a
//! cyclic encoding over a finite period, a bucketed ordinal — puts many samples
//! at exactly the same coordinate, so the k-th radius is zero and KSG is not
//! merely imprecise but undefined. Kraskov et al. (2004) state the requirement;
//! Gao, Kannan & Viswanath (2017, "Estimating Mutual Information for
//! Discrete-Continuous Mixtures") show the standard KSG rule is inconsistent
//! exactly when the k-th distance can be zero with non-zero probability.
//!
//! The fix is not to make KSG tolerate ties. For a column that really is
//! discrete, the contingency-table plug-in estimator is **exact** in the limit
//! and needs no continuity assumption at all — it is the right instrument, not
//! a degraded one. Its one flaw is a well-characterised finite-sample bias:
//! each empirical entropy is biased **downward** by `(K − 1) / (2N)`, so the
//! plug-in `I = H(X) + H(Y) − H(X,Y)` is biased **upward**. The leading-order
//! Miller-Madow correction removes exactly that term, and this module applies
//! it — the same correction, and the same `(K − 1) / (2N ln 2)` expression,
//! that [`transfer_entropy::discrete`](crate::transfer_entropy) already uses.
//!
//! Reporting an uncorrected plug-in number would manufacture bits: for a
//! 40-level lens over 2,000 samples the uncorrected bias is ~0.014 bits against
//! a 0.05-bit admission floor, and for a 2,048-bucket hash lens it is larger
//! than any signal the lens carries. So the correction is mandatory, and above
//! it sits a fail-closed support guard: a table too sparse for the leading-order
//! correction to be trustworthy is **refused**, never reported. Miller-Madow
//! removes only the first-order term, and higher-order terms survive in sparse
//! tables, so "not enough samples for this support" is a real answer.
//!
//! A third instrument is required for a multivariate column whose complete row
//! identities are categorical but whose shared coordinates are the information
//! carrier. Interning a 512-dimensional signed feature hash as one contingency-
//! table symbol discards that shared structure; when almost every row is unique,
//! the plug-in support guard correctly refuses it. For a binary outcome, a
//! deterministic multi-seed logistic probe instead learns from the coordinates
//! on training folds and measures only held-out predictions. Because the
//! prediction is a deterministic post-processing of the column, its measured
//! information is a data-processing-inequality-safe lower bound on `I(X;Y)`,
//! not a replacement estimate that can claim more information than `X` holds.
//! Power calibration and convergence checks remain binding.
//!
//! Selection mirrors [`transfer_entropy::resolve_estimator`] deliberately: the
//! auto rule keys on the exact data properties that select an instrument,
//! records the counts it saw in a human-readable reason, and **never re-tries a
//! different estimator after a failure**. A silent second attempt would make
//! the reported estimator a function of which one happened to fail first.

use std::collections::BTreeMap;

use calyx_core::{Anchor, CalyxError, Result};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::bootstrap::{BootstrapConfig, DEFAULT_BOOTSTRAP_RESAMPLES, DEFAULT_BOOTSTRAP_SEED};
use crate::estimate::{EstimatorKind, MiEstimate, TrustTag, trust_for_anchor};
use crate::ksg::{
    MIN_ASSAY_SAMPLES, ksg_mi_continuous_discrete, ksg_mi_continuous_discrete_with_anchor,
};
use crate::logistic::{
    logistic_probe_mi_multiseed_calibrated, logistic_probe_mi_multiseed_calibrated_with_anchor,
};
use crate::samples::validate_rectangular_finite;

/// Minimum paired samples per **occupied** joint cell for the Miller-Madow
/// corrected plug-in to be reported rather than refused.
///
/// The correction is first-order in `1/N`; the residual grows with table
/// sparsity, so a table whose occupied cells outnumber `N / 5` is refused
/// instead of being reported with an unquantified higher-order error.
pub const MIN_SAMPLES_PER_OCCUPIED_CELL: usize = 5;

const MI_BOOTSTRAP_CONFIG: BootstrapConfig =
    BootstrapConfig::new(DEFAULT_BOOTSTRAP_RESAMPLES, DEFAULT_BOOTSTRAP_SEED);

/// Which estimator actually ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiEstimator {
    /// Contingency-table plug-in entropy decomposition with a Miller-Madow
    /// first-order bias correction.
    DiscretePlugin,
    /// Kraskov-Stögbauer-Grassberger k-nearest-neighbour mutual information.
    ContinuousKsg,
    /// Held-out, calibrated logistic-probe lower bound for a binary outcome.
    LogisticProbe,
}

impl MiEstimator {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DiscretePlugin => "discrete_plugin",
            Self::ContinuousKsg => "continuous_ksg",
            Self::LogisticProbe => "logistic_probe",
        }
    }
}

/// What the caller asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiEstimatorChoice {
    /// Choose from the measured column, and say why.
    #[default]
    Auto,
    /// Always use the Miller-Madow corrected discrete plug-in.
    DiscretePlugin,
    /// Always use the continuous KSG estimator.
    ContinuousKsg,
    /// Always use the held-out calibrated logistic probe. The outcome must be
    /// binary; unsupported outcomes fail closed.
    LogisticProbe,
}

/// Why the estimator that ran was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiEstimatorSelection {
    RequestedDiscretePlugin,
    RequestedContinuousKsg,
    RequestedLogisticProbe,
    /// At least one sample shares its exact coordinates with `k` or more
    /// same-label samples, so KSG's k-th radius is zero by construction.
    AutoDuplicateSaturatedColumn,
    /// No exact-duplicate class within a label reaches `k`, so KSG's k-th
    /// radius is strictly positive for every sample.
    AutoDistinctValuedColumn,
    /// The complete coordinate tuples are duplicate-saturated for KSG and too
    /// sparse for the plug-in support contract, while the multivariate column
    /// has a binary outcome that can be measured as a held-out lower bound.
    AutoSparseHighDimensionalBinary,
}

impl MiEstimatorSelection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestedDiscretePlugin => "requested_discrete_plugin",
            Self::RequestedContinuousKsg => "requested_continuous_ksg",
            Self::RequestedLogisticProbe => "requested_logistic_probe",
            Self::AutoDuplicateSaturatedColumn => "auto_duplicate_saturated_column",
            Self::AutoDistinctValuedColumn => "auto_distinct_valued_column",
            Self::AutoSparseHighDimensionalBinary => "auto_sparse_high_dimensional_binary",
        }
    }
}

/// The resolved estimator plus the measured column facts behind the choice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MiEstimatorPick {
    pub estimator: MiEstimator,
    pub selection: MiEstimatorSelection,
    pub reason: String,
    /// Distinct exact coordinate tuples observed in the column.
    pub distinct_values: usize,
    /// Largest exact-duplicate class **within one outcome label**. This is the
    /// quantity KSG's k-th radius depends on.
    pub max_same_label_multiplicity: usize,
    /// Occupied cells in the exact `(whole coordinate tuple, outcome)` table.
    pub occupied_joint_cells: usize,
    /// Number of independently addressable coordinates in each input row.
    pub input_dim: usize,
    /// Number of exact outcome levels observed.
    pub label_levels: usize,
}

/// Resolves the estimator for one column measured against discrete `labels`.
///
/// # Errors
///
/// Returns a structured error when `x` and `labels` disagree in length, when
/// `x` is not rectangular and finite, or when `k` is zero.
pub fn resolve_mi_estimator(
    choice: MiEstimatorChoice,
    x: &[Vec<f32>],
    labels: &[usize],
    k: usize,
) -> Result<MiEstimatorPick> {
    validate_paired(x, labels, k)?;
    let column = DiscreteColumn::intern(x, labels)?;
    Ok(resolve_from_column(choice, &column, x.len(), k))
}

fn resolve_from_column(
    choice: MiEstimatorChoice,
    column: &DiscreteColumn,
    n: usize,
    k: usize,
) -> MiEstimatorPick {
    let distinct_values = column.value_levels;
    let max_same_label_multiplicity = column.max_same_label_multiplicity;
    let (estimator, selection, reason) = match choice {
        MiEstimatorChoice::DiscretePlugin => (
            MiEstimator::DiscretePlugin,
            MiEstimatorSelection::RequestedDiscretePlugin,
            "the caller pinned the Miller-Madow corrected discrete plug-in estimator".to_string(),
        ),
        MiEstimatorChoice::ContinuousKsg => (
            MiEstimator::ContinuousKsg,
            MiEstimatorSelection::RequestedContinuousKsg,
            "the caller pinned the continuous KSG estimator".to_string(),
        ),
        MiEstimatorChoice::LogisticProbe => (
            MiEstimator::LogisticProbe,
            MiEstimatorSelection::RequestedLogisticProbe,
            "the caller pinned the held-out calibrated logistic-probe lower bound".to_string(),
        ),
        MiEstimatorChoice::Auto => {
            if max_same_label_multiplicity >= k {
                let required_plugin_samples = column
                    .occupied_joint_cells
                    .saturating_mul(MIN_SAMPLES_PER_OCCUPIED_CELL);
                if column.input_dim > 1 && column.label_levels == 2 && n < required_plugin_samples {
                    (
                        MiEstimator::LogisticProbe,
                        MiEstimatorSelection::AutoSparseHighDimensionalBinary,
                        format!(
                            "auto: the {dim}-dimensional column holds {distinct_values} distinct whole-row value(s) and {occupied} occupied exact joint cell(s) over {n} sample(s); its largest same-label duplicate multiplicity is {max_same_label_multiplicity} at k={k}, so KSG has zero radius, while the discrete plug-in requires {required_plugin_samples} samples. The binary outcome selects a calibrated multi-seed logistic probe over held-out predictions, whose information is a DPI-safe lower bound",
                            dim = column.input_dim,
                            occupied = column.occupied_joint_cells,
                        ),
                    )
                } else {
                    (
                        MiEstimator::DiscretePlugin,
                        MiEstimatorSelection::AutoDuplicateSaturatedColumn,
                        format!(
                            "auto: the column holds {distinct_values} distinct value(s) over {n} sample(s) and its largest exact-duplicate class within one outcome label holds {max_same_label_multiplicity} sample(s) at k={k}, so the continuous KSG k-th radius is zero by construction"
                        ),
                    )
                }
            } else {
                (
                    MiEstimator::ContinuousKsg,
                    MiEstimatorSelection::AutoDistinctValuedColumn,
                    format!(
                        "auto: the column holds {distinct_values} distinct value(s) over {n} sample(s) and its largest exact-duplicate class within one outcome label holds {max_same_label_multiplicity} sample(s), below k={k}, so the KSG k-th radius is strictly positive"
                    ),
                )
            }
        }
    };
    MiEstimatorPick {
        estimator,
        selection,
        reason,
        distinct_values,
        max_same_label_multiplicity,
        occupied_joint_cells: column.occupied_joint_cells,
        input_dim: column.input_dim,
        label_levels: column.label_levels,
    }
}

/// Estimates `I(x; labels)` in bits with the estimator the column warrants.
///
/// # Errors
///
/// Propagates the chosen estimator's structured refusal unchanged. The other
/// estimator is never attempted afterwards: a refusal is a fact about the
/// column, and re-trying would make the reported estimator depend on which one
/// happened to fail first.
pub fn mi_about_labels(
    choice: MiEstimatorChoice,
    x: &[Vec<f32>],
    labels: &[usize],
    k: usize,
    anchor: Option<&Anchor>,
) -> Result<MiOutcome> {
    validate_paired(x, labels, k)?;
    // Intern once. Selection and the discrete estimator both need the dense
    // codes, and interning is the expensive step: the key is the whole
    // coordinate tuple, so a 128-dimensional column costs 128 comparisons per
    // probe. Doing it per call site made a synergy pass over 28 slot pairs
    // re-derive the same codes 84 times.
    let column = DiscreteColumn::intern(x, labels)?;
    let pick = resolve_from_column(choice, &column, x.len(), k);
    let estimate = match pick.estimator {
        MiEstimator::DiscretePlugin => {
            mi_discrete_plugin(&column, x.len(), trust_for_anchor(anchor))
        }
        MiEstimator::ContinuousKsg => match anchor {
            Some(anchor) => ksg_mi_continuous_discrete_with_anchor(x, labels, k, anchor),
            None => ksg_mi_continuous_discrete(x, labels, k),
        },
        MiEstimator::LogisticProbe => mi_logistic_probe(x, &column, anchor),
    };
    Ok(MiOutcome { pick, estimate })
}

/// Measures a binary outcome through held-out predictions from the complete
/// multivariate column. This is an explicit estimator, not a retry path: the
/// selection is fixed before training and every training/calibration refusal is
/// returned unchanged.
fn mi_logistic_probe(
    x: &[Vec<f32>],
    column: &DiscreteColumn,
    anchor: Option<&Anchor>,
) -> Result<MiEstimate> {
    if column.label_levels != 2 {
        return Err(CalyxError::assay_degenerate_input(format!(
            "logistic-probe mutual information requires exactly two outcome levels; got {}",
            column.label_levels
        )));
    }
    let labels = column
        .labels
        .iter()
        .map(|label| *label == 1)
        .collect::<Vec<_>>();
    let report = match anchor {
        Some(anchor) => {
            logistic_probe_mi_multiseed_calibrated_with_anchor(x, &labels, None, anchor)
        }
        None => logistic_probe_mi_multiseed_calibrated(x, &labels, None),
    }?;
    Ok(report.estimate)
}

/// One measurement attempt: which instrument was chosen, and what it returned.
///
/// The estimate is a nested `Result` on purpose. An estimator refusal is a fact
/// about the column, and the caller reporting that refusal still needs the
/// instrument and the cardinality that chose it — that is the evidence an
/// operator acts on. Collapsing the two would make "why did this lens not
/// measure" unanswerable without a rerun.
pub struct MiOutcome {
    pub pick: MiEstimatorPick,
    pub estimate: Result<MiEstimate>,
}

/// A column and its outcome labels, interned to dense codes exactly once.
struct DiscreteColumn {
    /// Dense value code per sample, in `0..value_levels`.
    codes: Vec<u32>,
    /// Dense label code per sample, in `0..label_levels`.
    labels: Vec<u32>,
    value_levels: usize,
    label_levels: usize,
    /// Largest exact-duplicate class within one outcome label, minus one — the
    /// count of *other* samples KSG would find at radius zero.
    max_same_label_multiplicity: usize,
    /// Occupied cells of the joint contingency table.
    occupied_joint_cells: usize,
    /// Width of the original rectangular input matrix.
    input_dim: usize,
}

/// Miller-Madow corrected contingency-table plug-in `I(x; labels)` in bits.
///
/// # Errors
///
/// Returns `CALYX_ASSAY_INSUFFICIENT_SAMPLES` when the paired sample count is
/// below the assay floor or when the occupied joint support is too large for
/// the first-order bias correction to be trustworthy, and
/// `CALYX_ASSAY_DEGENERATE_INPUT` when either margin has a single level, for
/// which mutual information is identically zero and carries no information
/// about the lens.
fn mi_discrete_plugin(column: &DiscreteColumn, n: usize, trust: TrustTag) -> Result<MiEstimate> {
    if n < MIN_ASSAY_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "discrete plug-in mutual information needs at least {MIN_ASSAY_SAMPLES} paired samples; got {n}"
        )));
    }
    if column.value_levels < 2 || column.label_levels < 2 {
        return Err(CalyxError::assay_degenerate_input(format!(
            "discrete plug-in mutual information is undefined for a single-level margin: distinct column values={} distinct outcome labels={}",
            column.value_levels, column.label_levels
        )));
    }
    let occupied = column.occupied_joint_cells;
    let required = occupied.saturating_mul(MIN_SAMPLES_PER_OCCUPIED_CELL);
    if n < required {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "discrete plug-in mutual information refuses a table too sparse for its first-order bias correction: {occupied} occupied joint cell(s) over {n} sample(s) needs at least {required} ({MIN_SAMPLES_PER_OCCUPIED_CELL} per occupied cell). The Miller-Madow correction removes only the leading 1/N term, so a sparser table would be reported with an unquantified residual bias. Reduce the lens support (coarser bucketing) or anchor more outcomes"
        )));
    }

    let mut scratch = ContingencyScratch::new(column.value_levels, column.label_levels);
    let indices: Vec<u32> = (0..u32::try_from(n).unwrap_or(u32::MAX)).collect();
    let bits = corrected_mi_bits(column, &indices, &mut scratch);
    let (ci_low, ci_high) = bootstrap_ci(column, bits, MI_BOOTSTRAP_CONFIG, &mut scratch);
    Ok(MiEstimate::new(
        bits,
        ci_low,
        ci_high,
        n,
        EstimatorKind::DiscretePlugin,
        trust,
    ))
}

/// Reusable count buffers for the contingency table.
///
/// The bootstrap re-estimates the same table hundreds of times, so the counts
/// are held in flat arrays indexed by the dense codes and cleared between
/// resamples. An associative container here made the estimate `O(n log n)` with
/// a comparison per probe, which on a 128-dimensional column was the difference
/// between a synergy sweep finishing and not.
struct ContingencyScratch {
    x_counts: Vec<u32>,
    y_counts: Vec<u32>,
    joint_counts: Vec<u32>,
    label_levels: usize,
}

impl ContingencyScratch {
    fn new(value_levels: usize, label_levels: usize) -> Self {
        Self {
            x_counts: vec![0; value_levels],
            y_counts: vec![0; label_levels],
            joint_counts: vec![0; value_levels.saturating_mul(label_levels)],
            label_levels,
        }
    }

    fn clear(&mut self) {
        self.x_counts.fill(0);
        self.y_counts.fill(0);
        self.joint_counts.fill(0);
    }
}

/// `I(X;Y) = H(X) + H(Y) − H(X,Y)`, each entropy Miller-Madow corrected.
fn corrected_mi_bits(
    column: &DiscreteColumn,
    indices: &[u32],
    scratch: &mut ContingencyScratch,
) -> f32 {
    scratch.clear();
    for &index in indices {
        let index = index as usize;
        let value = column.codes[index] as usize;
        let label = column.labels[index] as usize;
        scratch.x_counts[value] += 1;
        scratch.y_counts[label] += 1;
        scratch.joint_counts[value * scratch.label_levels + label] += 1;
    }
    let n = indices.len();
    let (h_x, k_x) = entropy_and_support(&scratch.x_counts, n);
    let (h_y, k_y) = entropy_and_support(&scratch.y_counts, n);
    let (h_joint, k_joint) = entropy_and_support(&scratch.joint_counts, n);
    let corrected = (h_x + miller_madow_bits(k_x, n)) + (h_y + miller_madow_bits(k_y, n))
        - (h_joint + miller_madow_bits(k_joint, n));
    // `I(X;Y) <= min(H(X), H(Y))` is a law, not a preference, and the
    // Miller-Madow correction can breach it: under perfect dependence the two
    // marginal supports are corrected upward while the joint support is the
    // same size, so the corrected difference lands just above the ceiling
    // (`1 + 1/(2N ln 2)` bits for a balanced binary pair). Reporting that would
    // claim more information about the outcome than the outcome contains, and
    // the sufficiency gate `I(panel;anchor) >= H(anchor)` reads exactly this
    // number. Bound it by the *uncorrected* marginals, which are the exact
    // entropies of the sample actually in hand.
    corrected.max(0.0).min(h_x.min(h_y)).max(0.0)
}

fn bootstrap_ci(
    column: &DiscreteColumn,
    point: f32,
    config: BootstrapConfig,
    scratch: &mut ContingencyScratch,
) -> (f32, f32) {
    if config.resamples == 0 {
        return (point, point);
    }
    let n = column.codes.len();
    let mut rng = ChaCha8Rng::seed_from_u64(config.seed);
    let mut estimates = Vec::with_capacity(config.resamples);
    let mut indices = vec![0u32; n];
    for _ in 0..config.resamples {
        resample_indices(&mut indices, n, &mut rng);
        estimates.push(corrected_mi_bits(column, &indices, scratch));
    }
    estimates.sort_by(f32::total_cmp);
    let low = percentile(&estimates, 0.025);
    let high = percentile(&estimates, 0.975);
    (low.min(point), high.max(point))
}

fn resample_indices(out: &mut [u32], n: usize, rng: &mut ChaCha8Rng) {
    use rand::Rng as _;
    for slot in out.iter_mut() {
        *slot = rng.random_range(0..n as u32);
    }
}

fn percentile(sorted: &[f32], quantile: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let index = ((sorted.len() - 1) as f32 * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Plug-in Shannon entropy in bits and the occupied support behind it.
fn entropy_and_support(counts: &[u32], n: usize) -> (f32, usize) {
    if n == 0 {
        return (0.0, 0);
    }
    #[allow(clippy::cast_precision_loss)]
    let nf = f64::from(u32::try_from(n).unwrap_or(u32::MAX));
    let mut h = 0.0f64;
    let mut support = 0usize;
    for &count in counts {
        if count == 0 {
            continue;
        }
        support += 1;
        let p = f64::from(count) / nf;
        h -= p * p.log2();
    }
    #[allow(clippy::cast_possible_truncation)]
    let h = h as f32;
    (h, support)
}

/// The Miller-Madow `(K − 1) / (2N ln 2)` first-order entropy bias correction
/// over the occupied support `K`.
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

impl DiscreteColumn {
    /// Interns exact coordinate tuples and outcome labels as dense codes.
    /// Values are matched by canonical IEEE bits, so no binning, rounding or
    /// noise is invented.
    fn intern(x: &[Vec<f32>], labels: &[usize]) -> Result<Self> {
        let mut value_ids: BTreeMap<Vec<u32>, u32> = BTreeMap::new();
        let mut label_ids: BTreeMap<usize, u32> = BTreeMap::new();
        let mut codes = Vec::with_capacity(x.len());
        let mut dense_labels = Vec::with_capacity(labels.len());
        for (row, label) in x.iter().zip(labels) {
            let mut key = Vec::with_capacity(row.len());
            for value in row {
                if !value.is_finite() {
                    return Err(CalyxError::assay_degenerate_input(
                        "mutual-information estimator selection requires finite coordinates",
                    ));
                }
                // Canonicalise -0.0 to +0.0 so the two spellings share one symbol.
                let canonical = if *value == 0.0 { 0.0_f32 } else { *value };
                key.push(canonical.to_bits());
            }
            let next_value = u32::try_from(value_ids.len()).unwrap_or(u32::MAX);
            codes.push(*value_ids.entry(key).or_insert(next_value));
            let next_label = u32::try_from(label_ids.len()).unwrap_or(u32::MAX);
            dense_labels.push(*label_ids.entry(*label).or_insert(next_label));
        }
        let value_levels = value_ids.len();
        let label_levels = label_ids.len();
        // One pass over the joint table gives both the KSG-degeneracy quantity
        // and the occupied-cell count that the sparsity guard reads.
        let mut cells: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for (code, label) in codes.iter().zip(&dense_labels) {
            *cells.entry((*code, *label)).or_default() += 1;
        }
        // A sample's own duplicate class excludes itself when KSG counts
        // neighbours, so the comparable quantity is the class size minus one.
        let max_same_label_multiplicity =
            cells.values().copied().max().unwrap_or(0).saturating_sub(1);
        Ok(Self {
            codes,
            labels: dense_labels,
            value_levels,
            label_levels,
            max_same_label_multiplicity,
            occupied_joint_cells: cells.len(),
            input_dim: x.first().map_or(0, Vec::len),
        })
    }
}

fn validate_paired(x: &[Vec<f32>], labels: &[usize], k: usize) -> Result<()> {
    if x.len() != labels.len() {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "mutual information requires paired samples: x={} labels={}",
            x.len(),
            labels.len()
        )));
    }
    if k == 0 {
        return Err(CalyxError::assay_insufficient_samples(
            "mutual-information estimator selection requires k > 0",
        ));
    }
    validate_rectangular_finite("x", x)?;
    Ok(())
}
