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
        *score = dot / (left_norm * right_norm);
    }
    Ok(())
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
