pub(super) struct KernelMatrix {
    pub(super) n: usize,
    pub(super) values: Vec<f64>,
}

impl KernelMatrix {
    /// Builds the symmetric kernel into one packed upper-triangular arena.
    ///
    /// `workspace` is the exact pair-distance arena used to choose the median
    /// bandwidth. Reusing it here prevents the distance vector and an `n x n`
    /// kernel allocation from overlapping at the estimator's peak.
    pub(super) fn new(
        samples: &[&[f64]],
        bandwidth: f64,
        mut workspace: Vec<f64>,
    ) -> calyx_core::Result<Self> {
        let n = samples.len();
        let packed_len = packed_matrix_len(n)?;
        workspace.clear();
        if workspace.capacity() < packed_len {
            workspace
                .try_reserve_exact(packed_len - workspace.capacity())
                .map_err(|error| {
                    calyx_core::CalyxError::forge_vram_budget(format!(
                        "MMD packed kernel reserve failed for {n} samples ({packed_len} f64 values): {error}"
                    ))
                })?;
        }
        for i in 0..n {
            for j in i..n {
                workspace.push(if i == j {
                    1.0
                } else {
                    gaussian_kernel(samples[i], samples[j], bandwidth)
                });
            }
        }
        debug_assert_eq!(workspace.len(), packed_len);
        Ok(Self {
            n,
            values: workspace,
        })
    }

    pub(super) fn mmd2(&self, x: &[usize], y: &[usize]) -> f64 {
        self.mean(x, x) + self.mean(y, y) - 2.0 * self.mean(x, y)
    }

    pub(super) fn mmd2_unbiased(&self, x: &[usize], y: &[usize]) -> f64 {
        self.off_diagonal_mean(x) + self.off_diagonal_mean(y) - 2.0 * self.mean(x, y)
    }

    pub(super) fn off_diagonal_mean(&self, indices: &[usize]) -> f64 {
        debug_assert!(indices.len() > 1);
        let mut sum = 0.0;
        for &i in indices {
            for &j in indices {
                if i != j {
                    sum += self.get(i, j);
                }
            }
        }
        sum / (indices.len() * (indices.len() - 1)) as f64
    }

    pub(super) fn mean(&self, left: &[usize], right: &[usize]) -> f64 {
        let mut sum = 0.0;
        for &i in left {
            for &j in right {
                sum += self.get(i, j);
            }
        }
        sum / (left.len() * right.len()) as f64
    }

    fn get(&self, left: usize, right: usize) -> f64 {
        let (row, column) = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        // Construction proves n*(n+1)/2 fits usize. Both indices are drawn
        // from 0..n, so this packed-row calculation is bounded by that same
        // proven allocation length.
        let row_start = row * self.n - row * row.saturating_sub(1) / 2;
        self.values[row_start + column - row]
    }
}

pub(super) fn packed_matrix_len(n: usize) -> calyx_core::Result<usize> {
    n.checked_add(1)
        .and_then(|next| n.checked_mul(next))
        .map(|twice| twice / 2)
        .ok_or_else(|| {
            calyx_core::CalyxError::forge_vram_budget(format!(
                "MMD packed kernel length overflow for {n} samples"
            ))
        })
}

fn gaussian_kernel(a: &[f64], b: &[f64], bandwidth: f64) -> f64 {
    (-squared_distance(a, b) / (2.0 * bandwidth * bandwidth)).exp()
}

pub(super) fn squared_distance(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let delta = x - y;
            delta * delta
        })
        .sum()
}

pub(super) fn quantile(sorted_values: &[f64], q: f64) -> f64 {
    debug_assert!(!sorted_values.is_empty());
    let rank = ((sorted_values.len() - 1) as f64 * q).ceil() as usize;
    sorted_values[rank.min(sorted_values.len() - 1)]
}
