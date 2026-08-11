//! The CPU float-reduction contract, and the runtime kernel dispatch that honours it.
//!
//! ## Why this module exists
//!
//! Before #1912 every CPU kernel in this crate reduced with `wide`'s
//! `reduce_add()` and annotated the call site with a `DETERMINISM:` comment. Both
//! halves of that arrangement were wrong.
//!
//! **The reduction order was not ours.** `wide::f32x16::reduce_add` lowers
//! differently depending on what the *compiler* was allowed to emit:
//!
//! | baseline | `f32x16` is | the 16 lanes fold as |
//! |---|---|---|
//! | `x86-64-v2` (this workspace's pin) | 4x `m128` | four left-associative quads, then `(q0+q1)+(q2+q3)` |
//! | `x86-64-v3` (AVX) | 2x `m256` | a `movehl`/`shuffle` pairwise tree per half |
//! | AVX-512 | 1x `m512` | `_mm512_reduce_add_ps` |
//!
//! Floating-point addition is not associative, so those three produce three
//! different numbers for the same input. A comment claiming a determinism
//! contract sat directly above an expression that broke it whenever `target-cpu`
//! moved — and `gemm` was worse still: its AVX-512 path folded strict
//! left-associative octets while its fallback folded `wide` quads, so one binary
//! computed two different answers on two different hosts.
//!
//! **The reduction order was also the performance ceiling.** A horizontal reduce
//! per chunk keeps the running total in a scalar, so every chunk pays a full
//! dependent add chain plus a store/load round trip through `cast`. That is why
//! `calyx-forge` measured *slower than a plain scalar loop* on this host: the
//! SIMD multiply was never the bottleneck, the per-chunk horizontal fold was.
//!
//! ## The contract this module defines
//!
//! One rule, owned here, independent of host and compile flags:
//!
//! > Accumulate into [`LANES`] lanes, in ascending element order, so lane `j`
//! > holds every element `j, j+8, j+16, ...`. Fold the lanes exactly once at the
//! > end with [`fold_lanes`]. Add any tail elements to the folded scalar in
//! > ascending order.
//!
//! Element-wise vector arithmetic is bit-exact no matter how wide the underlying
//! registers are — only the *horizontal* fold has an ordering choice, and this
//! module makes that choice exactly once. So the portable `wide` path and the
//! AVX2 intrinsic path below are not merely close, they are **bit-identical**,
//! and [`crate::cpu::simd::reduction_paths_agree`] proves it at runtime rather
//! than asserting it in a comment.
//!
//! This is strictly more determinism than the code had before, not less: the
//! answer no longer depends on `target-cpu`, on AVX-512 presence, or on which
//! `wide` version is in the lockfile.
//!
//! ## What is deliberately not done
//!
//! **FMA is not used**, though this host has it and #1912 asked for it. A fused
//! multiply-add rounds once where a separate multiply and add round twice, so an
//! FMA kernel cannot be bit-identical to the non-FMA fallback that hosts without
//! FMA must run. Buying ~1.3x on one class of host by making two hosts disagree
//! is the wrong trade for a database whose records are content-addressed and
//! whose results are expected to reproduce. The 8-lane accumulator already
//! removes the dependent-chain stall that was the actual cost.

/// Accumulator width for every reduction in this crate.
///
/// 8 is the AVX2 register width, so the dispatched path accumulates in exactly
/// one register. The portable path uses `wide::f32x8`, which lowers to two SSE
/// registers at the `x86-64-v2` baseline — a different instruction count, but
/// element-wise arithmetic is exact, so the same lane values either way.
pub const LANES: usize = 8;

/// The single horizontal fold, applied once per reduction.
///
/// A balanced pairwise tree: `((a0+a1)+(a2+a3)) + ((a4+a5)+(a6+a7))`. Pairwise
/// summation also has strictly better error growth than the left-associative
/// chain it replaces (`O(log n)` rather than `O(n)` in the worst case), so this
/// is more accurate as well as faster.
#[inline]
#[must_use]
pub fn fold_lanes(lanes: [f32; LANES]) -> f32 {
    ((lanes[0] + lanes[1]) + (lanes[2] + lanes[3]))
        + ((lanes[4] + lanes[5]) + (lanes[6] + lanes[7]))
}

/// Which kernel family [`dispatch`] selected on this host. Surfaced through
/// `CpuBackend::simd_path` so health readback states what actually ran instead
/// of what the compile baseline implies.
pub fn backend_name() -> &'static str {
    kernels().name
}

/// The dispatched three-way reduction `(dot, |left|^2, |right|^2)` in one pass.
///
/// Exported so a crate that needs a cosine does not hand-roll a fifth scalar
/// loop (#1917). Four independent cosine implementations existed in this
/// workspace, each answering "does this CPU have AVX2" its own way; three had
/// to be fixed separately (#1908, #1912) before the fourth — `calyx-loom`'s
/// `agreement_scalar`, on the widest cosine path in the system — was found
/// still undispatched. The dispatch decision belongs here, once.
///
/// Callers own the shape contract: elements past `min(left.len(),
/// right.len())` do not contribute. Check the dimensions before calling if
/// unequal lengths are an error in your domain — that check is not fusible
/// into the reduction and is cheap.
///
/// The finiteness scan **is** fusible and should not be a separate pass: a
/// non-finite element poisons the accumulator it feeds and stays poisoned, so
/// a non-finite `left_norm_sq` names the left row and a non-finite
/// `right_norm_sq` names the right row. See [`super::guard::non_finite_row_error`]
/// for why that detector is strictly stronger than a pre-scan.
#[must_use]
pub fn dot_and_pair_norms(left: &[f32], right: &[f32]) -> (f32, f32, f32) {
    (kernels().dot_and_pair_norms)(left, right)
}

type Reduce1 = fn(&[f32]) -> f32;
type Reduce2 = fn(&[f32], &[f32]) -> f32;
type ReduceDotNorm = fn(&[f32], &[f32]) -> (f32, f32);
type ReduceDotPairNorms = fn(&[f32], &[f32]) -> (f32, f32, f32);
type ScaleRow = fn(&mut [f32], f32);

#[derive(Clone, Copy)]
pub(crate) struct Kernels {
    name: &'static str,
    pub(crate) sum_squares: Reduce1,
    pub(crate) dot: Reduce2,
    pub(crate) l2_squared: Reduce2,
    pub(crate) dot_and_norm: ReduceDotNorm,
    pub(crate) dot_and_pair_norms: ReduceDotPairNorms,
    pub(crate) scale_row: ScaleRow,
}

static KERNELS: std::sync::OnceLock<Kernels> = std::sync::OnceLock::new();

pub(crate) fn kernels() -> &'static Kernels {
    KERNELS.get_or_init(dispatch)
}

fn dispatch() -> Kernels {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        // Runtime, not compile-time: this is the whole point. The workspace pins
        // `target-cpu=x86-64-v2` so the shipped binary starts on parts without
        // AVX2, and this check costs that decision nothing on parts that have it.
        if std::arch::is_x86_feature_detected!("avx2") {
            return Kernels {
                name: "avx2",
                sum_squares: avx2::sum_squares,
                dot: avx2::dot,
                l2_squared: avx2::l2_squared,
                dot_and_norm: avx2::dot_and_norm,
                dot_and_pair_norms: avx2::dot_and_pair_norms,
                scale_row: avx2::scale_row,
            };
        }
    }
    portable_kernels()
}

fn portable_kernels() -> Kernels {
    Kernels {
        name: "portable-f32x8",
        sum_squares: portable::sum_squares,
        dot: portable::dot,
        l2_squared: portable::l2_squared,
        dot_and_norm: portable::dot_and_norm,
        dot_and_pair_norms: portable::dot_and_pair_norms,
        scale_row: portable::scale_row,
    }
}

/// Prove, on this host, that the dispatched kernels and the portable kernels
/// return **bit-identical** results for the supplied vectors.
///
/// The determinism claim in this module's documentation is falsifiable, so it is
/// checked rather than asserted. Synapse calls it over a fixed startup probe;
/// other consumers can call it over their own representative corpus. Returns
/// the name of the first kernel that disagreed, or `None` when every kernel
/// agrees.
#[must_use]
pub fn reduction_paths_agree(left: &[f32], right: &[f32]) -> Option<&'static str> {
    let dispatched = kernels();
    let portable = portable_kernels();
    if (dispatched.sum_squares)(left).to_bits() != (portable.sum_squares)(left).to_bits() {
        return Some("sum_squares");
    }
    if (dispatched.dot)(left, right).to_bits() != (portable.dot)(left, right).to_bits() {
        return Some("dot");
    }
    if (dispatched.l2_squared)(left, right).to_bits()
        != (portable.l2_squared)(left, right).to_bits()
    {
        return Some("l2_squared");
    }
    let (dispatched_dot, dispatched_norm) = (dispatched.dot_and_norm)(left, right);
    let (portable_dot, portable_norm) = (portable.dot_and_norm)(left, right);
    if dispatched_dot.to_bits() != portable_dot.to_bits()
        || dispatched_norm.to_bits() != portable_norm.to_bits()
    {
        return Some("dot_and_norm");
    }
    let dispatched_triple = (dispatched.dot_and_pair_norms)(left, right);
    let portable_triple = (portable.dot_and_pair_norms)(left, right);
    if dispatched_triple.0.to_bits() != portable_triple.0.to_bits()
        || dispatched_triple.1.to_bits() != portable_triple.1.to_bits()
        || dispatched_triple.2.to_bits() != portable_triple.2.to_bits()
    {
        return Some("dot_and_pair_norms");
    }
    let mut dispatched_scaled = left.to_vec();
    let mut portable_scaled = left.to_vec();
    (dispatched.scale_row)(&mut dispatched_scaled, 0.5);
    (portable.scale_row)(&mut portable_scaled, 0.5);
    if dispatched_scaled
        .iter()
        .zip(&portable_scaled)
        .any(|(a, b)| a.to_bits() != b.to_bits())
    {
        return Some("scale_row");
    }
    None
}

pub(crate) mod portable {
    use wide::f32x8;

    use super::{LANES, fold_lanes};

    /// Borrow the next `LANES` elements directly. No zero-init, no `copy_from_slice`
    /// — the previous `load16` helper allocated 64 bytes of stack, zeroed it, and
    /// memcpy'd into it on every single chunk of every single row.
    #[inline]
    fn chunk(values: &[f32], offset: usize) -> f32x8 {
        let lanes: &[f32; LANES] = values[offset..offset + LANES]
            .try_into()
            .expect("chunk() is only called with LANES elements remaining");
        f32x8::from(*lanes)
    }

    pub(crate) fn sum_squares(values: &[f32]) -> f32 {
        let mut acc = f32x8::ZERO;
        let mut offset = 0;
        while offset + LANES <= values.len() {
            let v = chunk(values, offset);
            acc += v * v;
            offset += LANES;
        }
        let mut sum = fold_lanes(acc.to_array());
        while offset < values.len() {
            sum += values[offset] * values[offset];
            offset += 1;
        }
        sum
    }

    pub(crate) fn dot(left: &[f32], right: &[f32]) -> f32 {
        let len = left.len().min(right.len());
        let mut acc = f32x8::ZERO;
        let mut offset = 0;
        while offset + LANES <= len {
            acc += chunk(left, offset) * chunk(right, offset);
            offset += LANES;
        }
        let mut sum = fold_lanes(acc.to_array());
        while offset < len {
            sum += left[offset] * right[offset];
            offset += 1;
        }
        sum
    }

    pub(crate) fn l2_squared(left: &[f32], right: &[f32]) -> f32 {
        let len = left.len().min(right.len());
        let mut acc = f32x8::ZERO;
        let mut offset = 0;
        while offset + LANES <= len {
            let diff = chunk(left, offset) - chunk(right, offset);
            acc += diff * diff;
            offset += LANES;
        }
        let mut sum = fold_lanes(acc.to_array());
        while offset < len {
            let diff = left[offset] - right[offset];
            sum += diff * diff;
            offset += 1;
        }
        sum
    }

    pub(crate) fn dot_and_norm(query: &[f32], candidate: &[f32]) -> (f32, f32) {
        let len = query.len().min(candidate.len());
        let mut dot_acc = f32x8::ZERO;
        let mut norm_acc = f32x8::ZERO;
        let mut offset = 0;
        while offset + LANES <= len {
            let q = chunk(query, offset);
            let c = chunk(candidate, offset);
            dot_acc += q * c;
            norm_acc += c * c;
            offset += LANES;
        }
        let mut dot_sum = fold_lanes(dot_acc.to_array());
        let mut norm_sum = fold_lanes(norm_acc.to_array());
        while offset < len {
            dot_sum += query[offset] * candidate[offset];
            norm_sum += candidate[offset] * candidate[offset];
            offset += 1;
        }
        (dot_sum, norm_sum)
    }

    pub(crate) fn dot_and_pair_norms(left: &[f32], right: &[f32]) -> (f32, f32, f32) {
        let len = left.len().min(right.len());
        let mut dot_acc = f32x8::ZERO;
        let mut left_acc = f32x8::ZERO;
        let mut right_acc = f32x8::ZERO;
        let mut offset = 0;
        while offset + LANES <= len {
            let l = chunk(left, offset);
            let r = chunk(right, offset);
            dot_acc += l * r;
            left_acc += l * l;
            right_acc += r * r;
            offset += LANES;
        }
        let mut dot_sum = fold_lanes(dot_acc.to_array());
        let mut left_sum = fold_lanes(left_acc.to_array());
        let mut right_sum = fold_lanes(right_acc.to_array());
        while offset < len {
            let l = left[offset];
            let r = right[offset];
            dot_sum += l * r;
            left_sum += l * l;
            right_sum += r * r;
            offset += 1;
        }
        (dot_sum, left_sum, right_sum)
    }

    pub(crate) fn scale_row(values: &mut [f32], scale: f32) {
        let scale_vec = f32x8::splat(scale);
        let mut offset = 0;
        while offset + LANES <= values.len() {
            let scaled = chunk(values, offset) * scale_vec;
            values[offset..offset + LANES].copy_from_slice(&scaled.to_array());
            offset += LANES;
        }
        while offset < values.len() {
            values[offset] *= scale;
            offset += 1;
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) mod avx2 {
    //! AVX2 twins of [`super::portable`], lane-for-lane and fold-for-fold.
    //!
    //! Every function here accumulates into one `__m256` in the same ascending
    //! order the portable path accumulates into a `f32x8`, stores it, and folds
    //! it with the same [`super::fold_lanes`]. Element-wise multiply, add and
    //! subtract are exact, so the two paths agree bit-for-bit — which
    //! [`super::reduction_paths_agree`] verifies at startup on the real host
    //! rather than trusting this paragraph.

    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    use super::{LANES, fold_lanes};

    #[inline]
    #[target_feature(enable = "avx2")]
    fn horizontal(acc: __m256) -> f32 {
        let mut lanes = [0.0_f32; LANES];
        unsafe { _mm256_storeu_ps(lanes.as_mut_ptr(), acc) };
        fold_lanes(lanes)
    }

    pub(crate) fn sum_squares(values: &[f32]) -> f32 {
        unsafe { sum_squares_avx2(values) }
    }

    pub(crate) fn dot(left: &[f32], right: &[f32]) -> f32 {
        unsafe { dot_avx2(left, right) }
    }

    pub(crate) fn l2_squared(left: &[f32], right: &[f32]) -> f32 {
        unsafe { l2_squared_avx2(left, right) }
    }

    pub(crate) fn dot_and_norm(query: &[f32], candidate: &[f32]) -> (f32, f32) {
        unsafe { dot_and_norm_avx2(query, candidate) }
    }

    pub(crate) fn dot_and_pair_norms(left: &[f32], right: &[f32]) -> (f32, f32, f32) {
        unsafe { dot_and_pair_norms_avx2(left, right) }
    }

    pub(crate) fn scale_row(values: &mut [f32], scale: f32) {
        unsafe { scale_row_avx2(values, scale) }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn sum_squares_avx2(values: &[f32]) -> f32 {
        let mut acc = _mm256_setzero_ps();
        let mut offset = 0;
        while offset + LANES <= values.len() {
            let v = unsafe { _mm256_loadu_ps(values.as_ptr().add(offset)) };
            acc = _mm256_add_ps(acc, _mm256_mul_ps(v, v));
            offset += LANES;
        }
        let mut sum = horizontal(acc);
        while offset < values.len() {
            sum += values[offset] * values[offset];
            offset += 1;
        }
        sum
    }

    #[target_feature(enable = "avx2")]
    unsafe fn dot_avx2(left: &[f32], right: &[f32]) -> f32 {
        let len = left.len().min(right.len());
        let mut acc = _mm256_setzero_ps();
        let mut offset = 0;
        while offset + LANES <= len {
            let l = unsafe { _mm256_loadu_ps(left.as_ptr().add(offset)) };
            let r = unsafe { _mm256_loadu_ps(right.as_ptr().add(offset)) };
            acc = _mm256_add_ps(acc, _mm256_mul_ps(l, r));
            offset += LANES;
        }
        let mut sum = horizontal(acc);
        while offset < len {
            sum += left[offset] * right[offset];
            offset += 1;
        }
        sum
    }

    #[target_feature(enable = "avx2")]
    unsafe fn l2_squared_avx2(left: &[f32], right: &[f32]) -> f32 {
        let len = left.len().min(right.len());
        let mut acc = _mm256_setzero_ps();
        let mut offset = 0;
        while offset + LANES <= len {
            let l = unsafe { _mm256_loadu_ps(left.as_ptr().add(offset)) };
            let r = unsafe { _mm256_loadu_ps(right.as_ptr().add(offset)) };
            let diff = _mm256_sub_ps(l, r);
            acc = _mm256_add_ps(acc, _mm256_mul_ps(diff, diff));
            offset += LANES;
        }
        let mut sum = horizontal(acc);
        while offset < len {
            let diff = left[offset] - right[offset];
            sum += diff * diff;
            offset += 1;
        }
        sum
    }

    #[target_feature(enable = "avx2")]
    unsafe fn dot_and_norm_avx2(query: &[f32], candidate: &[f32]) -> (f32, f32) {
        let len = query.len().min(candidate.len());
        let mut dot_acc = _mm256_setzero_ps();
        let mut norm_acc = _mm256_setzero_ps();
        let mut offset = 0;
        while offset + LANES <= len {
            let q = unsafe { _mm256_loadu_ps(query.as_ptr().add(offset)) };
            let c = unsafe { _mm256_loadu_ps(candidate.as_ptr().add(offset)) };
            dot_acc = _mm256_add_ps(dot_acc, _mm256_mul_ps(q, c));
            norm_acc = _mm256_add_ps(norm_acc, _mm256_mul_ps(c, c));
            offset += LANES;
        }
        let mut dot_sum = horizontal(dot_acc);
        let mut norm_sum = horizontal(norm_acc);
        while offset < len {
            dot_sum += query[offset] * candidate[offset];
            norm_sum += candidate[offset] * candidate[offset];
            offset += 1;
        }
        (dot_sum, norm_sum)
    }

    #[target_feature(enable = "avx2")]
    unsafe fn dot_and_pair_norms_avx2(left: &[f32], right: &[f32]) -> (f32, f32, f32) {
        let len = left.len().min(right.len());
        let mut dot_acc = _mm256_setzero_ps();
        let mut left_acc = _mm256_setzero_ps();
        let mut right_acc = _mm256_setzero_ps();
        let mut offset = 0;
        while offset + LANES <= len {
            let l = unsafe { _mm256_loadu_ps(left.as_ptr().add(offset)) };
            let r = unsafe { _mm256_loadu_ps(right.as_ptr().add(offset)) };
            dot_acc = _mm256_add_ps(dot_acc, _mm256_mul_ps(l, r));
            left_acc = _mm256_add_ps(left_acc, _mm256_mul_ps(l, l));
            right_acc = _mm256_add_ps(right_acc, _mm256_mul_ps(r, r));
            offset += LANES;
        }
        let mut dot_sum = horizontal(dot_acc);
        let mut left_sum = horizontal(left_acc);
        let mut right_sum = horizontal(right_acc);
        while offset < len {
            let l = left[offset];
            let r = right[offset];
            dot_sum += l * r;
            left_sum += l * l;
            right_sum += r * r;
            offset += 1;
        }
        (dot_sum, left_sum, right_sum)
    }

    #[target_feature(enable = "avx2")]
    unsafe fn scale_row_avx2(values: &mut [f32], scale: f32) {
        let scale_vec = _mm256_set1_ps(scale);
        let mut offset = 0;
        while offset + LANES <= values.len() {
            let v = unsafe { _mm256_loadu_ps(values.as_ptr().add(offset)) };
            let scaled = _mm256_mul_ps(v, scale_vec);
            unsafe { _mm256_storeu_ps(values.as_mut_ptr().add(offset), scaled) };
            offset += LANES;
        }
        while offset < values.len() {
            values[offset] *= scale;
            offset += 1;
        }
    }
}
