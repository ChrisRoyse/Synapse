use crate::cpu::guard::{check_finite, check_norm_positive, check_shape_2d};
use crate::cpu::simd::kernels;
use crate::{ForgeError, Result};

pub fn normalize_f32(vecs: &mut [f32], dim: usize) -> Result<()> {
    if dim == 0 {
        if vecs.is_empty() {
            return Ok(());
        }
        return Err(ForgeError::ShapeMismatch {
            expected: vec![0],
            got: vec![vecs.len()],
            remediation: "dim=0 is valid only for an empty matrix".to_string(),
        });
    }
    if !vecs.len().is_multiple_of(dim) {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![dim],
            got: vec![vecs.len()],
            remediation: "normalize input length must be an integer number of rows".to_string(),
        });
    }
    let rows = vecs.len() / dim;
    check_shape_2d(vecs, rows, dim, "normalize input")?;
    check_finite(vecs, "normalize")?;

    for row in 0..rows {
        let start = row * dim;
        let end = start + dim;
        let norm_sq = sum_squares(&vecs[start..end]);
        let norm = norm_sq.sqrt();
        check_norm_positive(norm, "normalize", row)?;
        scale_row(&mut vecs[start..end], 1.0 / norm);
    }
    Ok(())
}

/// Both reductions route through the runtime-dispatched kernels in
/// [`crate::cpu::simd`], which own the fold order.
fn sum_squares(values: &[f32]) -> f32 {
    (kernels().sum_squares)(values)
}

fn scale_row(values: &mut [f32], scale: f32) {
    (kernels().scale_row)(values, scale);
}
