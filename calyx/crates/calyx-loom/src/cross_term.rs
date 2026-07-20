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

pub fn agreement_scalar(a: &[f32], b: &[f32]) -> Result<f32> {
    ensure_same_dim_finite(a, b)?;
    let mut dot = 0.0;
    let mut an = 0.0;
    let mut bn = 0.0;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        an += x * x;
        bn += y * y;
    }
    if an <= f32::EPSILON || bn <= f32::EPSILON {
        return Err(loom_error(
            CALYX_LOOM_ZERO_NORM_VECTOR,
            "agreement requires non-zero vectors",
        ));
    }
    Ok(dot / (an.sqrt() * bn.sqrt()))
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
    if a.len() != b.len() || a.is_empty() {
        return Err(loom_error(
            CALYX_LOOM_DIM_MISMATCH,
            format!("xterm dims {} and {}", a.len(), b.len()),
        ));
    }
    ensure_finite(a)?;
    ensure_finite(b)
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
