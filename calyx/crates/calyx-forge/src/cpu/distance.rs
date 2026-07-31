use crate::Result;
use crate::cpu::guard::{check_finite, check_norm_positive, check_shape_2d, non_finite_row_error};
use crate::cpu::simd::kernels;

pub fn cosine_batch(query: &[f32], candidates: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
    validate_batch("cosine_batch", query, candidates, dim, out)?;
    if out.is_empty() {
        return Ok(());
    }

    let query_norm = sum_squares(query).sqrt();
    check_norm_positive(query_norm, "cosine_batch", 0)?;

    for (row, score) in out.iter_mut().enumerate() {
        let candidate = candidate_row(candidates, dim, row);
        let (dot, candidate_norm_sq) = dot_and_norm(query, candidate);
        // The candidate's own norm is checked first for the same reason as in
        // `paired_cosine_batch`: it is the exact detector for a non-finite
        // element of THIS row, whereas a non-finite `dot` only becomes
        // attributable once the row is known clean (the query was pre-scanned).
        if !candidate_norm_sq.is_finite() || !dot.is_finite() {
            return Err(non_finite_row_error("cosine_batch", row, candidate));
        }
        let candidate_norm = candidate_norm_sq.sqrt();
        check_norm_positive(candidate_norm, "cosine_batch", row)?;
        *score = dot / (query_norm * candidate_norm);
    }
    Ok(())
}

pub fn dot_batch(query: &[f32], candidates: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
    validate_batch("dot_batch", query, candidates, dim, out)?;
    for (row, score) in out.iter_mut().enumerate() {
        let candidate = candidate_row(candidates, dim, row);
        let value = dot(query, candidate);
        if !value.is_finite() {
            return Err(non_finite_row_error("dot_batch", row, candidate));
        }
        *score = value;
    }
    Ok(())
}

pub fn l2_batch(query: &[f32], candidates: &[f32], dim: usize, out: &mut [f32]) -> Result<()> {
    validate_batch("l2_batch", query, candidates, dim, out)?;
    for (row, score) in out.iter_mut().enumerate() {
        let candidate = candidate_row(candidates, dim, row);
        let value = l2_squared(query, candidate);
        if !value.is_finite() {
            return Err(non_finite_row_error("l2_batch", row, candidate));
        }
        *score = value;
    }
    Ok(())
}

pub fn paired_cosine_batch(
    left: &[f32],
    right: &[f32],
    pair_count: usize,
    dim: usize,
    out: &mut [f32],
) -> Result<()> {
    validate_paired(left, right, pair_count, dim, out)?;
    for (pair_idx, score) in out.iter_mut().enumerate().take(pair_count) {
        let left_row = candidate_row(left, dim, pair_idx);
        let right_row = candidate_row(right, dim, pair_idx);
        let (dot, left_norm_sq, right_norm_sq) = dot_and_pair_norms(left_row, right_row);
        // Order matters, and getting it wrong is how this first shipped. A
        // non-finite `dot` does not say WHICH side carried the non-finite value,
        // so testing it first attributed a `-inf` on the right to the left row —
        // whose elements were all finite — and the failure-path scan then
        // reported a spurious "overflow". Each norm, by contrast, is an exact
        // per-side detector: an element can only poison the accumulator it feeds,
        // and its square is non-finite whenever it is. So both norms are checked
        // first, and `dot` is only consulted once both sides are known clean, at
        // which point a non-finite dot really is an overflow of finite inputs.
        if !left_norm_sq.is_finite() {
            return Err(non_finite_row_error(
                "paired_cosine_batch",
                pair_idx,
                left_row,
            ));
        }
        if !right_norm_sq.is_finite() || !dot.is_finite() {
            return Err(non_finite_row_error(
                "paired_cosine_batch",
                pair_idx,
                right_row,
            ));
        }
        let left_norm = left_norm_sq.sqrt();
        let right_norm = right_norm_sq.sqrt();
        check_norm_positive(left_norm, "paired_cosine_batch", pair_idx)?;
        check_norm_positive(right_norm, "paired_cosine_batch", pair_idx)?;
        // The same range contract the scalar entry point enforces (#1923): this
        // site computed `dot / denom` inline and shipped the raw quotient, so a
        // pair of identical vectors could emit a score just over 1.0 exactly as
        // `dense_cosine` did. Single-sourced rather than restated (#1922).
        let quotient = dot / (left_norm * right_norm);
        *score = calyx_core::clamp_cosine_quotient(quotient).ok_or_else(|| {
            crate::ForgeError::NumericalInvariant {
                op: "paired_cosine_batch".to_string(),
                detail: format!(
                    "pair {pair_idx} produced cosine {quotient}, further outside [-1,1] than f32 rounding explains"
                ),
                remediation: "a cosine outside [-1,1] beyond the rounding tolerance is a real defect in the inputs or the reduction, not a value to clamp; inspect the pair's vectors".to_string(),
            }
        })?;
    }
    Ok(())
}

/// Which operand a cosine failure is attributable to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CosineSide {
    Left,
    Right,
}

impl CosineSide {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// Why one cosine could not be computed, in terms a caller maps to its own
/// error vocabulary (#1922).
///
/// The error-code mapping is what kept five cosine sites from sharing one
/// implementation: `calyx-loom` must return `CALYX_LOOM_*`, `calyx-sextant`
/// `CALYX_SEXTANT_*`, and the shared primitive returned a `ForgeError` that
/// neither could emit without string-matching it. A typed enum makes the
/// mapping the caller's one line of work and the *computation* shared, which is
/// the correct split.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CosineFailure {
    /// Operand lengths differ.
    DimMismatch { left: usize, right: usize },
    /// Both operands are empty; a cosine over no dimensions is undefined.
    Empty,
    /// One operand carries a non-finite element, with the offending index.
    NonFinite { side: CosineSide, index: usize },
    /// Both operands are finite but their dot product overflowed.
    Overflow,
    /// One operand has zero (or non-finite) norm; the quotient is undefined.
    ZeroNorm { side: CosineSide },
    /// The quotient landed further outside `[-1, 1]` than rounding explains.
    OutOfRange { cosine: f32 },
}

/// **The** cosine of two dense vectors: the one entry point every site should
/// reach for (#1922).
///
/// It owns, in one place, everything each site used to own separately:
///
/// - shape validation, naming both lengths;
/// - the runtime-dispatched reduction, so "does this CPU have AVX2" is not a
///   question a new call site can answer wrongly;
/// - **fused** finiteness classification with per-side attribution — a
///   non-finite element can only poison the accumulator it feeds, so each norm
///   is an exact per-side detector and the reduction result is a complete and
///   strictly stronger check than a second pre-scan pass over the input. The
///   index is recovered by scanning only on the failure path, where cost is
///   irrelevant;
/// - the zero-norm threshold;
/// - the `[-1, 1]` range contract, single-sourced from
///   [`calyx_core::clamp_cosine_quotient`] — the defect #1923 found, which the
///   sites that computed `dot / denom` inline each had to be fixed for
///   separately, or were never fixed at all.
///
/// Determinism is the dispatched kernels' contract and is proven once by
/// `reduction_paths_agree` (dispatched == portable, bit for bit); it is
/// referenced here rather than restated.
///
/// # Errors
///
/// Returns a [`CosineFailure`] the caller maps to its own domain error. This
/// function never returns a value it could not compute, and never a clamped
/// stand-in for one.
pub fn cosine(left: &[f32], right: &[f32]) -> core::result::Result<f32, CosineFailure> {
    if left.len() != right.len() {
        return Err(CosineFailure::DimMismatch {
            left: left.len(),
            right: right.len(),
        });
    }
    if left.is_empty() {
        return Err(CosineFailure::Empty);
    }
    let (dot_value, left_norm_sq, right_norm_sq) = dot_and_pair_norms(left, right);
    // Norms first, and per side, for the reason spelled out in
    // `paired_cosine_batch`: a non-finite `dot` does not say which operand
    // carried the non-finite value, so testing it first misattributes.
    if !left_norm_sq.is_finite() {
        return Err(CosineFailure::NonFinite {
            side: CosineSide::Left,
            index: first_non_finite(left),
        });
    }
    if !right_norm_sq.is_finite() {
        return Err(CosineFailure::NonFinite {
            side: CosineSide::Right,
            index: first_non_finite(right),
        });
    }
    if !dot_value.is_finite() {
        return Err(CosineFailure::Overflow);
    }
    let left_norm = left_norm_sq.sqrt();
    let right_norm = right_norm_sq.sqrt();
    if !left_norm.is_finite() || left_norm <= 0.0 {
        return Err(CosineFailure::ZeroNorm {
            side: CosineSide::Left,
        });
    }
    if !right_norm.is_finite() || right_norm <= 0.0 {
        return Err(CosineFailure::ZeroNorm {
            side: CosineSide::Right,
        });
    }
    let quotient = dot_value / (left_norm * right_norm);
    calyx_core::clamp_cosine_quotient(quotient)
        .ok_or(CosineFailure::OutOfRange { cosine: quotient })
}

/// Index of the first non-finite element, for failure-path attribution only.
///
/// Returns `values.len()` when every element is finite, which the callers above
/// can only reach if a reduction reported non-finite over finite inputs — an
/// overflow, already classified separately.
fn first_non_finite(values: &[f32]) -> usize {
    values
        .iter()
        .position(|value| !value.is_finite())
        .unwrap_or(values.len())
}

fn validate_batch(
    op: &'static str,
    query: &[f32],
    candidates: &[f32],
    dim: usize,
    out: &[f32],
) -> Result<()> {
    check_shape_2d(query, 1, dim, "distance query")?;
    check_shape_2d(candidates, out.len(), dim, "distance candidates")?;
    // The query is one row, so scanning it up front is free and keeps the error
    // attributable to the query rather than to whichever row reduced first. The
    // candidate matrix is NOT scanned: see `non_finite_row_error` for why the
    // reduction result is a complete and strictly stronger detector.
    check_finite(query, op)?;
    Ok(())
}

fn validate_paired(
    left: &[f32],
    right: &[f32],
    pair_count: usize,
    dim: usize,
    out: &[f32],
) -> Result<()> {
    check_shape_2d(left, pair_count, dim, "paired cosine left")?;
    check_shape_2d(right, pair_count, dim, "paired cosine right")?;
    check_shape_2d(out, pair_count, 1, "paired cosine output")?;
    // Both sides here are full matrices, so neither is pre-scanned; the per-pair
    // reduction result detects a non-finite element and names it.
    Ok(())
}

fn candidate_row(candidates: &[f32], dim: usize, row: usize) -> &[f32] {
    let start = row * dim;
    &candidates[start..start + dim]
}

/// Every reduction below routes through the runtime-dispatched kernels in
/// [`crate::cpu::simd`], which own the fold order. See that module for why the
/// previous per-chunk `reduce_add()` was both non-deterministic across compile
/// baselines and slower than a scalar loop.
fn sum_squares(values: &[f32]) -> f32 {
    (kernels().sum_squares)(values)
}

fn dot(query: &[f32], candidate: &[f32]) -> f32 {
    (kernels().dot)(query, candidate)
}

fn dot_and_norm(query: &[f32], candidate: &[f32]) -> (f32, f32) {
    (kernels().dot_and_norm)(query, candidate)
}

fn dot_and_pair_norms(left: &[f32], right: &[f32]) -> (f32, f32, f32) {
    (kernels().dot_and_pair_norms)(left, right)
}

fn l2_squared(query: &[f32], candidate: &[f32]) -> f32 {
    (kernels().l2_squared)(query, candidate)
}
