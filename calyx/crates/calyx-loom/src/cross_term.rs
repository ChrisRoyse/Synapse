//! Cross-term value types and CPU/GPU-parity math kernels.

use calyx_core::{CxId, PanelSlotId, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::error::{
    CALYX_LOOM_DIM_MISMATCH, CALYX_LOOM_FORGE_UNAVAILABLE, CALYX_LOOM_NON_FINITE_VECTOR,
    CALYX_LOOM_ZERO_NORM_VECTOR, loom_error,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossTermKind {
    Agreement,
    Delta,
    Interaction,
    Concat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalProvenanceTag {
    Measured,
    Derived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CrossTermKey {
    pub cx_id: CxId,
    pub a: PanelSlotId,
    pub b: PanelSlotId,
    pub kind: CrossTermKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossTermValue {
    Scalar(f32),
    Vector(Vec<f32>),
}

pub fn canonical_pair(a: SlotId, b: SlotId) -> (SlotId, SlotId) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Cosine agreement between two slot vectors, on the shared dispatched kernel.
///
/// This is the widest cosine consumer in the system: weave computes one
/// agreement per record per lens pair, so it runs `n * C(N,2)` times, against
/// the bounded candidate sets the query-path cosines see. It was nonetheless
/// the last of the workspace's four cosine implementations to be a plain
/// scalar loop with no SIMD dispatch of any kind (#1917), because each site
/// answered "does this CPU have AVX2" independently and this one never asked.
///
/// Two passes became one. The reduction now routes through
/// [`calyx_forge::cpu::simd::dot_and_pair_norms`], which owns the fold order
/// and dispatches on AVX2 at runtime with portable and AVX2 paths that are
/// bit-identical by construction. The finiteness scan that `ensure_same_dim_finite`
/// performed as a *separate* full pass over both vectors is fused into that
/// reduction: a non-finite element poisons the accumulator it feeds and stays
/// poisoned, so a non-finite norm is an exact per-side detector. The dimension
/// check stays a pre-pass — it is not fusible, and it is cheap.
///
/// Classification order is preserved from the pre-fusion code: non-finite
/// before zero-norm. Each norm is consulted before `dot`, because `dot` mixes
/// both sides and so cannot say which one carried the bad value — the same
/// attribution bug `calyx-forge::cpu::distance::paired_cosine_batch` documents
/// having shipped once already.
///
/// One behaviour is strictly stronger than before: two finite vectors whose
/// squares overflow `f32` used to pass the finiteness pre-scan and then divide
/// by an infinite norm, silently yielding `0.0` or `NaN`. That now returns
/// [`CALYX_LOOM_NON_FINITE_VECTOR`] naming the overflow. A wrong agreement
/// written to the XTerm CF is worse than a refused one.
pub fn agreement_scalar(a: &[f32], b: &[f32]) -> Result<f32> {
    ensure_same_dim(a, b)?;
    let (dot, an, bn) = calyx_forge::cpu::simd::dot_and_pair_norms(a, b);
    if !an.is_finite() {
        return Err(non_finite_side_error("left", a));
    }
    if !bn.is_finite() {
        return Err(non_finite_side_error("right", b));
    }
    if !dot.is_finite() {
        return Err(loom_error(
            CALYX_LOOM_NON_FINITE_VECTOR,
            format!(
                "agreement dot product is non-finite over {} finite element(s) on both sides: the accumulation overflowed f32 range",
                a.len()
            ),
        ));
    }
    if an <= f32::EPSILON || bn <= f32::EPSILON {
        return Err(loom_error(
            CALYX_LOOM_ZERO_NORM_VECTOR,
            "agreement requires non-zero vectors",
        ));
    }
    Ok(dot / (an.sqrt() * bn.sqrt()))
}

/// Name the element that poisoned one side's norm accumulator.
///
/// Only reached on the failure path, so the scan it performs costs the happy
/// path nothing — which is the entire point of fusing the check into the
/// reduction rather than running it up front on every call.
fn non_finite_side_error(side: &'static str, values: &[f32]) -> calyx_core::CalyxError {
    for (index, value) in values.iter().enumerate() {
        if !value.is_finite() {
            return loom_error(
                CALYX_LOOM_NON_FINITE_VECTOR,
                format!("xterm {side} vector element {index} is {value}"),
            );
        }
    }
    loom_error(
        CALYX_LOOM_NON_FINITE_VECTOR,
        format!(
            "xterm {side} vector norm is non-finite even though all {} elements are finite: the accumulation overflowed f32 range",
            values.len()
        ),
    )
}

pub fn agreement_weight(raw_cosine: f32) -> Result<f32> {
    if !raw_cosine.is_finite() {
        return Err(loom_error(
            CALYX_LOOM_NON_FINITE_VECTOR,
            "agreement weight requires a finite raw cosine",
        ));
    }
    Ok(raw_cosine.clamp(0.0, 1.0))
}

pub fn agreement_batch_cpu(pairs: &[(&[f32], &[f32])]) -> Result<Vec<f32>> {
    pairs.iter().map(|(a, b)| agreement_scalar(a, b)).collect()
}

pub fn agreement_batch_gpu(pairs: &[(&[f32], &[f32])]) -> Result<Vec<f32>> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    #[cfg(feature = "cuda")]
    {
        agreement_batch_cuda(pairs)
    }
    #[cfg(not(feature = "cuda"))]
    {
        Err(loom_error(
            CALYX_LOOM_FORGE_UNAVAILABLE,
            "agreement_batch_gpu requires calyx-loom feature cuda",
        ))
    }
}

pub fn delta_vec(a: &[f32], b: &[f32]) -> Result<Vec<f32>> {
    ensure_same_dim_finite(a, b)?;
    Ok(a.iter().zip(b).map(|(x, y)| x - y).collect())
}

pub fn interaction_vec(a: &[f32], b: &[f32]) -> Result<Vec<f32>> {
    ensure_same_dim_finite(a, b)?;
    Ok(a.iter().zip(b).map(|(x, y)| x * y).collect())
}

pub fn concat_vec(a: &[f32], b: &[f32]) -> Result<Vec<f32>> {
    ensure_finite(a)?;
    ensure_finite(b)?;
    Ok(a.iter().chain(b).copied().collect())
}

fn ensure_same_dim_finite(a: &[f32], b: &[f32]) -> Result<()> {
    ensure_same_dim(a, b)?;
    ensure_finite(a)?;
    ensure_finite(b)
}

/// The half of [`ensure_same_dim_finite`] that cannot be fused into a
/// reduction, split out so `agreement_scalar` can keep it without paying for
/// the finiteness pass the reduction already detects (#1917).
fn ensure_same_dim(a: &[f32], b: &[f32]) -> Result<()> {
    if a.len() != b.len() || a.is_empty() {
        return Err(loom_error(
            CALYX_LOOM_DIM_MISMATCH,
            format!("xterm dims {} and {}", a.len(), b.len()),
        ));
    }
    Ok(())
}

fn ensure_finite(values: &[f32]) -> Result<()> {
    if values.iter().all(|value| value.is_finite()) {
        return Ok(());
    }
    Err(loom_error(
        CALYX_LOOM_NON_FINITE_VECTOR,
        "xterm vector contains NaN or infinity",
    ))
}

#[cfg(feature = "cuda")]
fn agreement_batch_cuda(pairs: &[(&[f32], &[f32])]) -> Result<Vec<f32>> {
    use calyx_forge::{Backend, CudaBackend};

    let backend = CudaBackend::new().map_err(|err| {
        loom_error(
            CALYX_LOOM_FORGE_UNAVAILABLE,
            format!("Forge CUDA backend unavailable for Loom agreement: {err}"),
        )
    })?;
    let dim = pairs[0].0.len();
    let row_values = pairs.len().checked_mul(dim).ok_or_else(|| {
        loom_error(
            CALYX_LOOM_DIM_MISMATCH,
            format!(
                "agreement_batch_gpu shape overflows usize: pairs={} dim={dim}",
                pairs.len()
            ),
        )
    })?;
    let mut left_rows = Vec::with_capacity(row_values);
    let mut right_rows = Vec::with_capacity(row_values);
    for (pair_idx, (left, right)) in pairs.iter().enumerate() {
        ensure_same_dim_finite(left, right)?;
        if left.len() != dim {
            return Err(loom_error(
                CALYX_LOOM_DIM_MISMATCH,
                format!(
                    "agreement_batch_gpu requires one dim per batch; pair {pair_idx} has dim {}, first pair dim {dim}",
                    left.len()
                ),
            ));
        }
        left_rows.extend_from_slice(left);
        right_rows.extend_from_slice(right);
    }

    let mut out = vec![0.0_f32; pairs.len()];
    backend
        .paired_cosine(&left_rows, &right_rows, pairs.len(), dim, &mut out)
        .map_err(|err| {
            loom_error(
                CALYX_LOOM_FORGE_UNAVAILABLE,
                format!("Forge CUDA paired cosine failed for Loom agreement: {err}"),
            )
        })?;
    Ok(out)
}
