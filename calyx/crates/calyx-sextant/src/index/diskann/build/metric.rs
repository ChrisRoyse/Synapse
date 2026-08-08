use rayon::prelude::*;

use crate::index::distance::l2_sq;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskAnnBuildMetric {
    UnitL2,
    RawL2,
}

/// The build-time vector space: one contiguous `len * dim` block, not a
/// `Vec<Vec<f32>>`.
///
/// Every distance in the Vamana build is a random-access read of one row, and
/// the build issues billions of them. A row-of-`Vec`s costs a dependent load
/// (heap pointer) before the coordinates can even be fetched, and scatters the
/// rows across the allocator so no two are adjacent. Flat storage removes the
/// indirection, makes row `i` a pure address computation, and is also the only
/// layout a batched `calyx_forge::Backend` call could ever consume without a
/// gather copy.
pub(in crate::index::diskann) struct BuildSpace {
    flat: Vec<f32>,
    dim: usize,
    len: usize,
}

impl BuildSpace {
    pub(in crate::index::diskann) fn len(&self) -> usize {
        self.len
    }

    pub(in crate::index::diskann) fn dim(&self) -> usize {
        self.dim
    }

    pub(in crate::index::diskann) fn row(&self, index: usize) -> &[f32] {
        let start = index * self.dim;
        &self.flat[start..start + self.dim]
    }

    /// Start the fetch of row `index` without waiting for it.
    ///
    /// The Vamana beam search reads one *random* row per candidate out of a
    /// space that is 144 MiB at production shape. Measured, that is where the
    /// build actually spends its time: 551M candidate distances at ~450 ns each,
    /// which is nowhere near the ~10 ns of AVX2 arithmetic a 96-lane `l2_sq`
    /// costs — it is a last-level-cache miss plus a TLB page walk, serialized
    /// one candidate at a time. The neighbour list of an expanded node is known
    /// in full *before* any of its distances are needed, so the misses can all
    /// be in flight at once instead of one after another. This is a pure timing
    /// hint: it reads nothing and changes no value.
    pub(in crate::index::diskann) fn prefetch(&self, index: usize) {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};

            let start = index * self.dim;
            debug_assert!(start + self.dim <= self.flat.len());
            let bytes = self.dim * size_of::<f32>();
            let mut offset = 0;
            while offset < bytes {
                // SAFETY: `_mm_prefetch` is a hint with no memory-safety
                // preconditions — it never faults and never reads a value, for
                // any address. The address is still derived from an in-bounds
                // row of `flat`, so it does not even leave the allocation.
                unsafe {
                    let base = self.flat.as_ptr().cast::<i8>().add(start * 4);
                    _mm_prefetch(base.add(offset), _MM_HINT_T0);
                }
                offset += 64;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = index;
        }
    }

    /// The backing block, for consumers that need the whole matrix contiguously
    /// (the cuVS dataset handoff, and any future batched-distance dispatch).
    #[cfg(sextant_cuvs)]
    pub(in crate::index::diskann) fn into_flat(self) -> Vec<f32> {
        self.flat
    }
}

/// L2-normalize every vector into the flat block; a zero vector stays all-zero
/// (dot == 0 with anything, i.e. distance 1, matching cosine's zero-vector
/// convention).
///
/// The magnitude is the same sequential `f32` sum-then-sqrt the row-wise
/// version used, so the normalized coordinates are bit-identical.
fn normalize_into(vectors: &[(u32, Vec<f32>)], dim: usize, flat: &mut [f32]) {
    flat.par_chunks_mut(dim)
        .zip(vectors.par_iter())
        .for_each(|(row, (_, v))| {
            let mag = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if mag == 0.0 {
                row.copy_from_slice(v);
            } else {
                for (dst, x) in row.iter_mut().zip(v) {
                    *dst = x / mag;
                }
            }
        });
}

pub(in crate::index::diskann) fn build_space(
    vectors: &[(u32, Vec<f32>)],
    dim: usize,
    metric: DiskAnnBuildMetric,
) -> BuildSpace {
    let len = vectors.len();
    let mut flat = vec![0.0_f32; len * dim];
    match metric {
        DiskAnnBuildMetric::UnitL2 => normalize_into(vectors, dim, &mut flat),
        DiskAnnBuildMetric::RawL2 => {
            flat.par_chunks_mut(dim)
                .zip(vectors.par_iter())
                .for_each(|(row, (_, v))| row.copy_from_slice(v));
        }
    }
    BuildSpace { flat, dim, len }
}

pub(super) fn dist(a: &[f32], b: &[f32], metric: DiskAnnBuildMetric) -> f32 {
    match metric {
        DiskAnnBuildMetric::UnitL2 => 0.5 * l2_sq(a, b),
        DiskAnnBuildMetric::RawL2 => l2_sq(a, b),
    }
}
