use crate::spectral::{SpectralError, SpectralResult};

const EIGEN_EPS: f32 = 1.0e-6;
const JACOBI_TOL: f32 = 1.0e-6;
const JACOBI_MIN_MAX_ITER: usize = 256;
const JACOBI_ROTATIONS_PER_ENTRY: usize = 16;

pub(crate) fn lanczos_eigen_operator<F>(
    n: usize,
    target_dim: usize,
    max_iter: usize,
    mut mat_vec: F,
) -> SpectralResult<(Vec<f32>, Vec<Vec<f32>>)>
where
    F: FnMut(&[f32]) -> Vec<f32>,
{
    if target_dim == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    if max_iter == 0 || target_dim > n || target_dim > max_iter {
        return Err(SpectralError::KrylovIncomplete {
            built: 0,
            target: target_dim,
            nodes: n,
            max_iter,
        });
    }
    let decomposition = lanczos_decomposition_operator(n, target_dim, max_iter, &mut mat_vec)?;
    let LanczosDecomposition { basis, projected } = decomposition;
    let jacobi_max_iter = jacobi_max_iter(projected.len());
    let (values, ritz_vectors) = jacobi_eigen(projected, jacobi_max_iter)?;
    Ok((values, expand_ritz_vectors(&basis, &ritz_vectors)))
}

fn jacobi_max_iter(n: usize) -> usize {
    n.saturating_mul(n)
        .saturating_mul(JACOBI_ROTATIONS_PER_ENTRY)
        .max(JACOBI_MIN_MAX_ITER)
}

#[derive(Debug)]
struct LanczosDecomposition {
    basis: Vec<Vec<f32>>,
    projected: Vec<Vec<f32>>,
}

fn lanczos_decomposition_operator<F>(
    n: usize,
    target_dim: usize,
    max_iter: usize,
    mat_vec: &mut F,
) -> SpectralResult<LanczosDecomposition>
where
    F: FnMut(&[f32]) -> Vec<f32>,
{
    let mut basis = Vec::with_capacity(target_dim);
    let mut projected = vec![vec![0.0; target_dim]; target_dim];
    let mut seed_index = 0;
    let mut iterations = 0;
    while basis.len() < target_dim && iterations < max_iter {
        let Some(mut current) = next_lanczos_seed(n, &basis, &mut seed_index) else {
            break;
        };
        let mut previous = vec![0.0; n];
        let mut previous_beta = 0.0;
        loop {
            iterations += 1;
            basis.push(current.clone());
            let column = basis.len() - 1;
            let product = mat_vec(&current);
            validate_operator_product(n, &product)?;
            let mut residual = product;
            let mut coefficients = vec![0.0; basis.len()];
            if previous_beta > EIGEN_EPS {
                axpy(&mut residual, -previous_beta, &previous);
                coefficients[column - 1] += previous_beta;
            }
            let alpha = dot(&current, &residual);
            axpy(&mut residual, -alpha, &current);
            coefficients[column] += alpha;
            orthogonalize_against_recording(&mut residual, &basis, &mut coefficients);
            record_symmetric_projection(&mut projected, column, &coefficients)?;
            if basis.len() == target_dim || iterations == max_iter {
                break;
            }
            let beta = vector_norm(&residual);
            if beta <= EIGEN_EPS {
                break;
            }
            previous = current;
            previous_beta = beta;
            scale(&mut residual, 1.0 / beta);
            current = residual;
        }
    }
    if basis.len() == target_dim {
        projected.truncate(target_dim);
        for row in &mut projected {
            row.truncate(target_dim);
        }
        Ok(LanczosDecomposition { basis, projected })
    } else {
        Err(SpectralError::KrylovIncomplete {
            built: basis.len(),
            target: target_dim,
            nodes: n,
            max_iter,
        })
    }
}

fn next_lanczos_seed(n: usize, basis: &[Vec<f32>], seed_index: &mut usize) -> Option<Vec<f32>> {
    while *seed_index < n {
        let mut vector = vec![0.0; n];
        vector[*seed_index] = 1.0;
        *seed_index += 1;
        orthogonalize_against(&mut vector, basis);
        if normalize(&mut vector).is_ok() {
            return Some(vector);
        }
    }
    None
}

fn validate_operator_product(expected: usize, product: &[f32]) -> SpectralResult<()> {
    if product.len() != expected || product.iter().any(|value| !value.is_finite()) {
        return Err(SpectralError::InvalidOperator {
            expected,
            actual: product.len(),
            non_finite: product.iter().filter(|value| !value.is_finite()).count(),
        });
    }
    Ok(())
}

fn record_symmetric_projection(
    projected: &mut [Vec<f32>],
    column: usize,
    coefficients: &[f32],
) -> SpectralResult<()> {
    for (row, coefficient) in coefficients.iter().copied().enumerate() {
        if !coefficient.is_finite() {
            return Err(SpectralError::InvalidOperator {
                expected: projected.len(),
                actual: projected.len(),
                non_finite: 1,
            });
        }
        projected[row][column] = coefficient;
        projected[column][row] = coefficient;
    }
    Ok(())
}

fn expand_ritz_vectors(basis: &[Vec<f32>], ritz_vectors: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let n = basis.first().map_or(0, Vec::len);
    let mut expanded = vec![vec![0.0; basis.len()]; n];
    for eigen_col in 0..basis.len() {
        for (basis_index, basis_vector) in basis.iter().enumerate() {
            let coefficient = ritz_vectors[basis_index][eigen_col];
            for (row, value) in basis_vector.iter().enumerate() {
                expanded[row][eigen_col] += coefficient * value;
            }
        }
    }
    expanded
}

fn jacobi_eigen(
    mut matrix: Vec<Vec<f32>>,
    max_iter: usize,
) -> SpectralResult<(Vec<f32>, Vec<Vec<f32>>)> {
    let n = matrix.len();
    let mut vectors = identity(n);
    let mut residual = f32::INFINITY;
    for iteration in 0..max_iter {
        let Some((p, q, value)) = max_offdiag(&matrix) else {
            return Ok((diagonal(&matrix), vectors));
        };
        residual = value.abs();
        if residual < JACOBI_TOL {
            return Ok((diagonal(&matrix), vectors));
        }
        rotate(&mut matrix, &mut vectors, p, q);
        if iteration % 10 == 9 {
            orthonormalize_columns(&mut vectors)?;
        }
    }
    Err(SpectralError::JacobiNotConverged {
        iterations: max_iter,
        residual,
        tol: JACOBI_TOL,
        dim: n,
    })
}

fn max_offdiag(matrix: &[Vec<f32>]) -> Option<(usize, usize, f32)> {
    let mut best: Option<(usize, usize, f32)> = None;
    for (row, values) in matrix.iter().enumerate() {
        for (col, value) in values.iter().copied().enumerate().skip(row + 1) {
            if best.is_none_or(|(_, _, current)| value.abs() > current.abs()) {
                best = Some((row, col, value));
            }
        }
    }
    best
}

fn rotate(matrix: &mut [Vec<f32>], vectors: &mut [Vec<f32>], p: usize, q: usize) {
    let theta = 0.5 * (2.0 * matrix[p][q]).atan2(matrix[q][q] - matrix[p][p]);
    let (s, c) = theta.sin_cos();
    for row in matrix.iter_mut() {
        let ap = row[p];
        let aq = row[q];
        row[p] = c * ap - s * aq;
        row[q] = s * ap + c * aq;
    }
    let (before_q, from_q) = matrix.split_at_mut(q);
    let row_p = &mut before_q[p];
    let row_q = &mut from_q[0];
    for (ap, aq) in row_p.iter_mut().zip(row_q.iter_mut()) {
        let prior_p = *ap;
        let prior_q = *aq;
        *ap = c * prior_p - s * prior_q;
        *aq = s * prior_p + c * prior_q;
    }
    matrix[p][q] = 0.0;
    matrix[q][p] = 0.0;
    for row in vectors {
        let vp = row[p];
        let vq = row[q];
        row[p] = c * vp - s * vq;
        row[q] = s * vp + c * vq;
    }
}

fn orthonormalize_columns(matrix: &mut [Vec<f32>]) -> SpectralResult<()> {
    let n = matrix.len();
    for col in 0..n {
        let mut vector = column(matrix, col);
        for prior in 0..col {
            let basis = column(matrix, prior);
            let projection = dot(&vector, &basis);
            for (value, basis_value) in vector.iter_mut().zip(basis) {
                *value -= projection * basis_value;
            }
        }
        normalize(&mut vector)?;
        for (row, value) in vector.into_iter().enumerate() {
            matrix[row][col] = value;
        }
    }
    Ok(())
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn axpy(dst: &mut [f32], alpha: f32, x: &[f32]) {
    for (dst_value, x_value) in dst.iter_mut().zip(x) {
        *dst_value += alpha * x_value;
    }
}

fn scale(vector: &mut [f32], alpha: f32) {
    for value in vector {
        *value *= alpha;
    }
}

fn normalize(vector: &mut [f32]) -> SpectralResult<()> {
    let norm = vector_norm(vector);
    if !norm.is_finite() || norm <= EIGEN_EPS {
        return Err(SpectralError::SingularMatrix);
    }
    scale(vector, 1.0 / norm);
    Ok(())
}

fn vector_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}

fn orthogonalize_against(vector: &mut [f32], basis: &[Vec<f32>]) {
    for basis_vector in basis {
        let projection = dot(vector, basis_vector);
        axpy(vector, -projection, basis_vector);
    }
}

fn orthogonalize_against_recording(
    vector: &mut [f32],
    basis: &[Vec<f32>],
    coefficients: &mut [f32],
) {
    for (index, basis_vector) in basis.iter().enumerate() {
        let projection = dot(vector, basis_vector);
        coefficients[index] += projection;
        axpy(vector, -projection, basis_vector);
    }
}

fn identity(n: usize) -> Vec<Vec<f32>> {
    let mut matrix = vec![vec![0.0; n]; n];
    for (index, row) in matrix.iter_mut().enumerate() {
        row[index] = 1.0;
    }
    matrix
}

fn diagonal(matrix: &[Vec<f32>]) -> Vec<f32> {
    (0..matrix.len())
        .map(|index| matrix[index][index])
        .collect()
}

pub(crate) fn column(matrix: &[Vec<f32>], index: usize) -> Vec<f32> {
    matrix.iter().map(|row| row[index]).collect()
}
