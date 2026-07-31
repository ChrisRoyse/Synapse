use crate::{ForgeError, Result};

const NUMERICAL_REMEDIATION: &str =
    "Ensure all input vectors are normalized finite f32; check upstream embedding model output";

pub fn check_finite(slice: &[f32], op: &str) -> Result<()> {
    for (index, value) in slice.iter().enumerate() {
        if !value.is_finite() {
            return Err(ForgeError::NumericalInvariant {
                op: op.to_string(),
                detail: format!("non-finite f32 at index {index}: {value}"),
                remediation: NUMERICAL_REMEDIATION.to_string(),
            });
        }
    }
    Ok(())
}

/// Explain a non-finite reduction result by finding the input that produced it.
///
/// The batch kernels used to scan every candidate for finiteness *before*
/// reducing, which is a second full pass over the candidate matrix on every
/// call — at the live 32-dim x 20k shape that is 2.5 MB read twice instead of
/// once, and it was most of why `calyx-forge` could not reach a plain scalar
/// loop.
///
/// The pass is unnecessary because the reduction already carries the signal. A
/// non-finite input poisons its lane accumulator and stays poisoned: `NaN`
/// propagates through every subsequent add, and `inf` either stays infinite or
/// becomes `NaN` when it meets its opposite. So `result.is_finite()` is false
/// whenever any contributing input was non-finite, and this function is called
/// only on that failure path to recover the exact index the old scan reported.
///
/// It is also strictly *stronger* than the scan it replaces. Finite inputs large
/// enough to overflow the accumulator produced a non-finite score that the
/// pre-scan passed without comment; that case is now named rather than returned.
pub fn non_finite_row_error(op: &str, row: usize, candidate: &[f32]) -> ForgeError {
    for (index, value) in candidate.iter().enumerate() {
        if !value.is_finite() {
            return ForgeError::NumericalInvariant {
                op: op.to_string(),
                detail: format!(
                    "non-finite reduction at row {row}: candidate element {index} is {value}"
                ),
                remediation: NUMERICAL_REMEDIATION.to_string(),
            };
        }
    }
    ForgeError::NumericalInvariant {
        op: op.to_string(),
        detail: format!(
            "non-finite reduction at row {row} even though all {} candidate elements are finite: \
             the accumulation overflowed f32 range",
            candidate.len()
        ),
        remediation: "reduce the magnitude of the input vectors, or rescale the lens output; a \
                      finite input set whose squares overflow f32 cannot be scored in f32"
            .to_string(),
    }
}

pub fn check_norm_positive(norm: f32, op: &str, row: usize) -> Result<()> {
    if norm > 0.0 && norm.is_finite() {
        return Ok(());
    }
    Err(ForgeError::NumericalInvariant {
        op: op.to_string(),
        detail: format!("zero or non-finite norm at row {row}"),
        remediation: NUMERICAL_REMEDIATION.to_string(),
    })
}

pub fn check_shape_2d(slice: &[f32], rows: usize, cols: usize, name: &str) -> Result<()> {
    let expected_len = rows
        .checked_mul(cols)
        .ok_or_else(|| ForgeError::ShapeMismatch {
            expected: vec![rows, cols],
            got: vec![slice.len()],
            remediation: format!("{name} shape overflows usize"),
        })?;
    if slice.len() == expected_len {
        return Ok(());
    }
    Err(ForgeError::ShapeMismatch {
        expected: vec![rows, cols],
        got: vec![slice.len()],
        remediation: format!("{name} length does not match rows*cols"),
    })
}
