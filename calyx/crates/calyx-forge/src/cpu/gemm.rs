use crate::Result;
use crate::cpu::guard::{check_finite, check_shape_2d};
use crate::cpu::simd::kernels;

pub const TILE_M: usize = 64;

pub fn gemm_f32(a: &[f32], b: &[f32], m: usize, k: usize, n: usize, out: &mut [f32]) -> Result<()> {
    validate_gemm_inputs(a, b, m, k, n, out)?;
    out.fill(0.0);
    if m == 0 || n == 0 {
        return Ok(());
    }

    gemm_tiled(a, b, m, k, n, out)
}

fn validate_gemm_inputs(
    a: &[f32],
    b: &[f32],
    m: usize,
    k: usize,
    n: usize,
    out: &[f32],
) -> Result<()> {
    check_shape_2d(a, m, k, "gemm A")?;
    check_shape_2d(b, k, n, "gemm B")?;
    check_shape_2d(out, m, n, "gemm output")?;
    check_finite(a, "cpu.gemm")?;
    check_finite(b, "cpu.gemm")?;
    Ok(())
}

/// One reduction order for every host.
///
/// The previous code chose between an AVX-512 path that folded strict
/// left-associative octets and a fallback that folded `wide` quads, so the same
/// binary computed two different products depending on the CPU it landed on.
/// Both are now replaced by the single contract in [`crate::cpu::simd`]: one
/// 8-lane accumulator over the whole depth, folded exactly once.
///
/// `TILE_M` still blocks the row loop for cache locality. `TILE_K` is gone: it
/// segmented the reduction into per-tile subtotals, which was never a numerical
/// requirement and made the answer depend on `k` relative to the tile width. A
/// public constant that governs nothing is the inert-knob anti-pattern, so it is
/// deleted rather than left to imply a tuning surface that does not exist.
fn gemm_tiled(a: &[f32], b: &[f32], m: usize, k: usize, n: usize, out: &mut [f32]) -> Result<()> {
    let mut packed_a = vec![0.0; k];
    for row_tile in (0..m).step_by(TILE_M) {
        let row_end = (row_tile + TILE_M).min(m);
        for row in row_tile..row_end {
            pack_a_row(a, row, m, &mut packed_a);
            for col in 0..n {
                let b_col = b_col_slice(b, col, k);
                out[col_major(row, col, m)] = (kernels().dot)(&packed_a, b_col);
            }
        }
    }
    Ok(())
}

fn pack_a_row(a: &[f32], row: usize, m: usize, packed: &mut [f32]) {
    for (depth, slot) in packed.iter_mut().enumerate() {
        *slot = a[col_major(row, depth, m)];
    }
}

fn b_col_slice(b: &[f32], col: usize, k: usize) -> &[f32] {
    let start = col_major(0, col, k);
    &b[start..start + k]
}

fn col_major(row: usize, col: usize, rows: usize) -> usize {
    col * rows + row
}
