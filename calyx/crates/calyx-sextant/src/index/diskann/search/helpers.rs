use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use calyx_core::{CxId, Result};

use super::DiskAnnSearchParams;
use crate::error::{
    CALYX_INDEX_DIM_MISMATCH, CALYX_INDEX_INVALID_PARAMS, CALYX_INDEX_IO, sextant_error,
};
use crate::index::diskann::graph::{DiskAnnGraphReader, DiskAnnVectorRef, open_diskann_graph};
use crate::index::distance::{cosine_distance, l2_sq, unit_l2_cosine_distance};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiskAnnDistanceMode {
    RawCosine,
    UnitL2,
    RawL2,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Candidate {
    pub(super) id: u32,
    pub(super) distance: f32,
}

impl Candidate {
    pub(super) fn new(id: u32, distance: f32) -> Self {
        Self { id, distance }
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.distance.to_bits() == other.distance.to_bits()
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl DiskAnnSearchParams {
    pub(super) fn validate(&self) -> Result<()> {
        if self.beamwidth == 0 || self.ef_search == 0 || self.rescore_k == 0 {
            return Err(invalid(
                "beamwidth, ef_search, and rescore_k must be positive",
            ));
        }
        Ok(())
    }
}

pub(super) fn distance(a: &[f32], b: &[f32], mode: DiskAnnDistanceMode) -> f32 {
    match mode {
        DiskAnnDistanceMode::RawCosine => cosine_distance(a, b),
        DiskAnnDistanceMode::UnitL2 => unit_l2_cosine_distance(a, b),
        DiskAnnDistanceMode::RawL2 => l2_sq(a, b),
    }
}

pub(super) fn distance_to_node(
    a: &[f32],
    b: DiskAnnVectorRef<'_>,
    mode: DiskAnnDistanceMode,
) -> f32 {
    match b {
        DiskAnnVectorRef::F32(values) => distance(a, values, mode),
        DiskAnnVectorRef::I8(values) => match mode {
            DiskAnnDistanceMode::RawL2 => l2_sq_i8(a, values),
            DiskAnnDistanceMode::RawCosine | DiskAnnDistanceMode::UnitL2 => cosine_i8(a, values),
        },
    }
}

fn cosine_i8(a: &[f32], b: &[i8]) -> f32 {
    let len = a.len().min(b.len());
    let (dot, an, bn) = dot_norms_i8(&a[..len], &b[..len]);
    if an == 0.0 || bn == 0.0 {
        1.0
    } else {
        ((1.0 - dot / (an.sqrt() * bn.sqrt())) as f32).max(0.0)
    }
}

fn l2_sq_i8(a: &[f32], b: &[i8]) -> f32 {
    let len = a.len().min(b.len());
    let (dot, an, bn) = dot_norms_i8(&a[..len], &b[..len]);
    ((an - 2.0 * dot + bn).max(0.0)) as f32
}

/// Asymmetric query-f32 by candidate-i8 fused dot product and squared norms.
/// Candidate bytes stay packed and are sign-extended directly into SIMD lanes;
/// accumulation uses f64 so dispatched and scalar paths agree at f32 precision.
fn dot_norms_i8(a: &[f32], b: &[i8]) -> (f64, f64, f64) {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: feature detection proves AVX2 support, and the helper only
        // loads within the equal-length slices established by the callers.
        return unsafe { dot_norms_i8_avx2(a, b) };
    }
    let mut dot = 0.0_f64;
    let mut an = 0.0_f64;
    let mut bn = 0.0_f64;
    for (x, y) in a.iter().zip(b) {
        let x = f64::from(*x);
        let y = f64::from(*y);
        dot += x * y;
        an += x * x;
        bn += y * y;
    }
    (dot, an, bn)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_norms_i8_avx2(a: &[f32], b: &[i8]) -> (f64, f64, f64) {
    use std::arch::x86_64::*;

    unsafe {
        let mut dot_lo = _mm256_setzero_pd();
        let mut dot_hi = _mm256_setzero_pd();
        let mut an_lo = _mm256_setzero_pd();
        let mut an_hi = _mm256_setzero_pd();
        let mut bn_lo = _mm256_setzero_pd();
        let mut bn_hi = _mm256_setzero_pd();
        let vectorized = a.len() / 8 * 8;
        let mut index = 0_usize;
        while index < vectorized {
            let packed = _mm_loadl_epi64(b.as_ptr().add(index).cast::<__m128i>());
            let widened = _mm256_cvtepi8_epi32(packed);
            let code_lo = _mm256_cvtepi32_pd(_mm256_castsi256_si128(widened));
            let code_hi = _mm256_cvtepi32_pd(_mm256_extracti128_si256::<1>(widened));
            let query_lo = _mm256_cvtps_pd(_mm_loadu_ps(a.as_ptr().add(index)));
            let query_hi = _mm256_cvtps_pd(_mm_loadu_ps(a.as_ptr().add(index + 4)));
            dot_lo = _mm256_add_pd(dot_lo, _mm256_mul_pd(query_lo, code_lo));
            dot_hi = _mm256_add_pd(dot_hi, _mm256_mul_pd(query_hi, code_hi));
            an_lo = _mm256_add_pd(an_lo, _mm256_mul_pd(query_lo, query_lo));
            an_hi = _mm256_add_pd(an_hi, _mm256_mul_pd(query_hi, query_hi));
            bn_lo = _mm256_add_pd(bn_lo, _mm256_mul_pd(code_lo, code_lo));
            bn_hi = _mm256_add_pd(bn_hi, _mm256_mul_pd(code_hi, code_hi));
            index += 8;
        }
        let mut dot_quad = [0.0_f64; 4];
        let mut an_quad = [0.0_f64; 4];
        let mut bn_quad = [0.0_f64; 4];
        _mm256_storeu_pd(dot_quad.as_mut_ptr(), _mm256_add_pd(dot_lo, dot_hi));
        _mm256_storeu_pd(an_quad.as_mut_ptr(), _mm256_add_pd(an_lo, an_hi));
        _mm256_storeu_pd(bn_quad.as_mut_ptr(), _mm256_add_pd(bn_lo, bn_hi));
        let mut dot = dot_quad.iter().sum::<f64>();
        let mut an = an_quad.iter().sum::<f64>();
        let mut bn = bn_quad.iter().sum::<f64>();
        for tail in index..a.len() {
            let x = f64::from(a[tail]);
            let y = f64::from(b[tail]);
            dot += x * y;
            an += x * x;
            bn += y * y;
        }
        (dot, an, bn)
    }
}

pub(super) fn sorted(mut hits: Vec<(u32, f32)>) -> Vec<(u32, f32)> {
    hits.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    hits
}

pub(super) fn dense_rows(rows: &[(CxId, Vec<f32>)], dim: usize) -> Result<Vec<(u32, Vec<f32>)>> {
    rows.iter()
        .enumerate()
        .map(|(idx, (_, vector))| {
            if vector.len() != dim {
                return Err(sextant_error(
                    CALYX_INDEX_DIM_MISMATCH,
                    format!("vector {idx} dim {} expected {dim}", vector.len()),
                ));
            }
            let id = u32::try_from(idx)
                .map_err(|_| invalid("diskann graph exceeds u32 node id space"))?;
            Ok((id, vector.clone()))
        })
        .collect()
}

pub(super) fn positions(ids: &[CxId]) -> HashMap<CxId, u32> {
    ids.iter()
        .enumerate()
        .filter_map(|(idx, cx_id)| u32::try_from(idx).ok().map(|id| (*cx_id, id)))
        .collect()
}

pub(super) fn open_for_search(path: &Path) -> Result<DiskAnnGraphReader> {
    open_diskann_graph(path)
}

pub(super) fn invalid(detail: impl std::fmt::Display) -> calyx_core::CalyxError {
    sextant_error(
        CALYX_INDEX_INVALID_PARAMS,
        format!("diskann search invalid params: {detail}"),
    )
}

pub(super) fn io(stage: &str, error: std::io::Error) -> calyx_core::CalyxError {
    sextant_error(CALYX_INDEX_IO, format!("diskann search {stage}: {error}"))
}

#[cfg(unix)]
pub(super) fn prefetch_node(file: &File, offset: u64, len: usize) {
    use std::os::fd::AsRawFd;

    const POSIX_FADV_WILLNEED: i32 = 3;
    unsafe extern "C" {
        fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
    }
    let _ = unsafe {
        posix_fadvise(
            file.as_raw_fd(),
            offset as i64,
            len as i64,
            POSIX_FADV_WILLNEED,
        )
    };
}

#[cfg(not(unix))]
pub(super) fn prefetch_node(_file: &File, _offset: u64, _len: usize) {}
