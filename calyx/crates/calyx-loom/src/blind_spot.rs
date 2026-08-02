//! Cross-lens anomaly detector.

use calyx_core::{CxId, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::error::{CALYX_LOOM_UNCALIBRATED_BLINDSPOT, loom_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlindSpotAlert {
    pub cx_id: CxId,
    pub a: SlotId,
    pub b: SlotId,
    pub delta: f32,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<BlindSpotCalibrationEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlindSpotCalibrationParams {
    pub min_samples: usize,
    pub alpha: f32,
}

impl Default for BlindSpotCalibrationParams {
    fn default() -> Self {
        Self {
            min_samples: 50,
            alpha: 0.05,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlindSpotCalibrationEvidence {
    pub sample_count: usize,
    pub alpha: f32,
    pub threshold_delta: f32,
    pub percentile: f32,
    pub p_value: f32,
    pub score: f32,
    /// How many distinct delta values the calibration was built from. A
    /// conformal certificate is only as fine-grained as the variable it is
    /// computed over; this is the number a caller needs to see whether the
    /// certificate discriminates or merely enumerates the encoding.
    pub distinct_deltas: usize,
    /// Whether the calibration has enough distinct values for its `1 - alpha`
    /// quantile to be distinguishable from its maximum (see
    /// [`BlindSpotCalibration::resolves_alpha`]).
    pub resolves_alpha: bool,
}

/// Tolerance at which two `f32` similarity values count as the same value when
/// counting how many distinct values a lens actually produces. Cosine scores
/// computed through different code paths differ in the last ulp (a one-hot
/// lens returns both `1.0` and `1.000_000_1`), and counting those as distinct
/// would defeat the whole degeneracy test.
pub const SIMILARITY_DISTINCT_TOLERANCE: f32 = 1e-4;

/// A lens whose neighbor-similarity takes fewer than this many distinct values
/// over a corpus cannot single out any individual record: every record carries
/// the same value, so "this lens is confident about this record" is a property
/// of the encoding rather than an observation about the data.
pub const MIN_DISCRIMINATIVE_DISTINCT: usize = 3;

/// A lens whose modal neighbor-similarity covers at least this share of the
/// corpus is degenerate for the same reason even when a handful of outliers
/// give it a nominally larger distinct count.
pub const MAX_DISCRIMINATIVE_MODAL_SHARE: f32 = 0.98;

/// How much genuine variation a lens's similarity distribution carries over one
/// corpus.
///
/// The blind-spot rule is "lens A is confident that these two records are alike
/// while lens B disagrees". That claim is only informative when A's similarity
/// *varies* across the corpus: a one-hot lens returns cosine exactly `1.0` for
/// any two records sharing a category and cannot return anything else, so it is
/// "confident" about every same-category pair by construction. Measuring the
/// distribution first is what separates a finding from an artifact.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimilarityDiscrimination {
    pub n: usize,
    pub distinct_values: usize,
    pub modal_value: f32,
    pub modal_share: f32,
    pub min: f32,
    pub max: f32,
}

impl SimilarityDiscrimination {
    /// Measures the distinct-value structure of one similarity sample.
    ///
    /// # Errors
    ///
    /// Returns [`CALYX_LOOM_UNCALIBRATED_BLINDSPOT`] when the sample is empty or
    /// carries a non-finite value: a degeneracy verdict computed over `NaN` is
    /// exactly the kind of silently-wrong answer this test exists to prevent.
    #[allow(clippy::cast_precision_loss)]
    pub fn measure(values: &[f32]) -> Result<Self> {
        if values.is_empty() {
            return Err(loom_error(
                CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
                "similarity discrimination needs at least one sample",
            ));
        }
        let mut sorted = Vec::with_capacity(values.len());
        for value in values {
            if !value.is_finite() {
                return Err(loom_error(
                    CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
                    "similarity discrimination sample must be finite",
                ));
            }
            sorted.push(*value);
        }
        sorted.sort_by(f32::total_cmp);

        // Count runs of values within tolerance of the run's first element, and
        // remember the longest run so the modal share is exact rather than an
        // estimate from a histogram with arbitrary bucket edges.
        let mut distinct_values = 0usize;
        let mut modal_value = sorted[0];
        let mut modal_len = 0usize;
        let mut run_start = sorted[0];
        let mut run_len = 0usize;
        for value in &sorted {
            if run_len > 0 && (*value - run_start).abs() <= SIMILARITY_DISTINCT_TOLERANCE {
                run_len += 1;
                continue;
            }
            if run_len > modal_len {
                modal_len = run_len;
                modal_value = run_start;
            }
            distinct_values += 1;
            run_start = *value;
            run_len = 1;
        }
        if run_len > modal_len {
            modal_len = run_len;
            modal_value = run_start;
        }

        Ok(Self {
            n: sorted.len(),
            distinct_values,
            modal_value,
            modal_share: modal_len as f32 / sorted.len() as f32,
            min: sorted[0],
            max: sorted[sorted.len() - 1],
        })
    }

    /// Whether this lens's "confidence" is definitional rather than measured.
    #[must_use]
    pub fn is_definitional(&self) -> bool {
        self.distinct_values < MIN_DISCRIMINATIVE_DISTINCT
            || self.modal_share >= MAX_DISCRIMINATIVE_MODAL_SHARE
    }

    /// The span this encoding actually reached over the corpus. A disagreement
    /// has to be read against this, not against the theoretical `[-1, 1]` of
    /// cosine: antipodal on a 24-hour cyclic encoding is a routine relationship,
    /// not an anomaly.
    #[must_use]
    pub fn observed_range(&self) -> f32 {
        self.max - self.min
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlindSpotCalibration {
    params: BlindSpotCalibrationParams,
    sorted_deltas: Vec<f32>,
    threshold_delta: f32,
    discrimination: SimilarityDiscrimination,
}

impl BlindSpotCalibration {
    pub fn from_deltas(
        deltas: impl IntoIterator<Item = f32>,
        params: BlindSpotCalibrationParams,
    ) -> Result<Self> {
        validate_calibration_params(params)?;
        let mut sorted_deltas = Vec::new();
        for delta in deltas {
            if !delta.is_finite() {
                return Err(loom_error(
                    CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
                    "blind-spot calibration delta must be finite",
                ));
            }
            sorted_deltas.push(delta);
        }
        if sorted_deltas.len() < params.min_samples {
            return Err(loom_error(
                CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
                format!(
                    "blind-spot calibration needs at least {} samples; got {}",
                    params.min_samples,
                    sorted_deltas.len()
                ),
            ));
        }
        sorted_deltas.sort_by(f32::total_cmp);
        let threshold_delta = percentile_threshold(&sorted_deltas, 1.0 - params.alpha);
        let discrimination = SimilarityDiscrimination::measure(&sorted_deltas)?;
        Ok(Self {
            params,
            sorted_deltas,
            threshold_delta,
            discrimination,
        })
    }

    pub fn sample_count(&self) -> usize {
        self.sorted_deltas.len()
    }

    pub fn threshold_delta(&self) -> f32 {
        self.threshold_delta
    }

    /// The distinct-value structure of the deltas this calibration was built
    /// from — the evidence for whether its certificate discriminates.
    #[must_use]
    pub fn discrimination(&self) -> SimilarityDiscrimination {
        self.discrimination
    }

    /// Whether the empirical distribution is fine-grained enough for its
    /// `1 - alpha` quantile to be distinguishable from its maximum.
    ///
    /// An empirical distribution over `d` distinct values places all of its mass
    /// on `d` points, so its upper-tail quantiles can take at most `d` values.
    /// Asking such a distribution for a `1 - alpha` quantile when
    /// `d < ceil(1 / alpha)` returns the maximum: every observation at the top
    /// of the range is then certified at `p <= alpha` purely because the range
    /// has nowhere else to put them. The certificate is real arithmetic over a
    /// variable that is nearly constant, which certifies the encoding rather
    /// than the corpus.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    pub fn resolves_alpha(&self) -> bool {
        if !self.params.alpha.is_finite() || self.params.alpha <= 0.0 {
            return false;
        }
        let required = (1.0f32 / self.params.alpha).ceil();
        if !required.is_finite() || required < 0.0 {
            return false;
        }
        self.discrimination.distinct_values >= required as usize
    }

    fn evaluate(&self, delta: f32) -> BlindSpotCalibrationEvidence {
        let n = self.sorted_deltas.len();
        let less_or_equal = self.sorted_deltas.partition_point(|value| *value <= delta);
        let less_than = self.sorted_deltas.partition_point(|value| *value < delta);
        let greater_or_equal = n - less_than;
        let percentile = less_or_equal as f32 / n as f32;
        let p_value = greater_or_equal as f32 / n as f32;
        BlindSpotCalibrationEvidence {
            sample_count: n,
            alpha: self.params.alpha,
            threshold_delta: self.threshold_delta,
            percentile,
            p_value,
            score: percentile,
            distinct_deltas: self.discrimination.distinct_values,
            resolves_alpha: self.resolves_alpha(),
        }
    }
}

pub fn detect_blind_spot(
    cx_id: CxId,
    a: SlotId,
    b: SlotId,
    lens_a_similarity: f32,
    lens_b_neighbor_mean: f32,
) -> Option<BlindSpotAlert> {
    let delta = lens_a_similarity - lens_b_neighbor_mean;
    if delta < 0.5 {
        return None;
    }
    let severity = if delta >= 0.8 {
        Severity::High
    } else if delta >= 0.65 {
        Severity::Medium
    } else {
        Severity::Low
    };
    Some(BlindSpotAlert {
        cx_id,
        a,
        b,
        delta,
        severity,
        calibration: None,
    })
}

pub fn detect_blind_spot_calibrated(
    cx_id: CxId,
    a: SlotId,
    b: SlotId,
    lens_a_similarity: f32,
    lens_b_neighbor_mean: f32,
    calibration: &BlindSpotCalibration,
) -> Result<Option<BlindSpotAlert>> {
    let delta = lens_a_similarity - lens_b_neighbor_mean;
    if !delta.is_finite() {
        return Err(loom_error(
            CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
            "blind-spot delta must be finite",
        ));
    }
    let evidence = calibration.evaluate(delta);
    if evidence.p_value > evidence.alpha {
        return Ok(None);
    }
    let severity = calibrated_severity(&evidence);
    Ok(Some(BlindSpotAlert {
        cx_id,
        a,
        b,
        delta,
        severity,
        calibration: Some(evidence),
    }))
}

/// Severity is graded on the conformal p-value, not on the percentile.
///
/// `percentile` is `#{delta_i <= delta} / n`, which saturates at exactly `1.0`
/// for every observation tied with the maximum — so on a calibration with heavy
/// ties (a one-hot or cyclic encoding produces a handful of distinct deltas and
/// nothing else) every emitted alert graded `high`. The p-value
/// `#{delta_i >= delta} / n` is the tail mass and stays meaningful under ties.
///
/// A calibration that cannot resolve `alpha` never rates above `low`: the
/// certificate has established that the observation sits at the top of a nearly
/// constant variable, which is a fact about the encoding, not about the record.
fn calibrated_severity(evidence: &BlindSpotCalibrationEvidence) -> Severity {
    if !evidence.resolves_alpha {
        return Severity::Low;
    }
    let alpha = evidence.alpha;
    if evidence.p_value <= alpha / 3.0 {
        Severity::High
    } else if evidence.p_value <= 2.0 * alpha / 3.0 {
        Severity::Medium
    } else {
        Severity::Low
    }
}

fn validate_calibration_params(params: BlindSpotCalibrationParams) -> Result<()> {
    if params.min_samples == 0 {
        return Err(loom_error(
            CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
            "blind-spot calibration min_samples must be greater than zero",
        ));
    }
    if !params.alpha.is_finite() || params.alpha <= 0.0 || params.alpha >= 1.0 {
        return Err(loom_error(
            CALYX_LOOM_UNCALIBRATED_BLINDSPOT,
            "blind-spot calibration alpha must be finite and in (0,1)",
        ));
    }
    Ok(())
}

fn percentile_threshold(sorted_deltas: &[f32], percentile: f32) -> f32 {
    let last = sorted_deltas.len().saturating_sub(1);
    let index = (last as f32 * percentile).ceil() as usize;
    sorted_deltas[index.min(last)]
}
