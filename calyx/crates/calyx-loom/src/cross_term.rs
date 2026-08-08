//! Cross-term value types and CPU/GPU-parity math kernels.

use calyx_core::{CxId, PanelSlotId, Result, SlotId};
use calyx_forge::cpu::distance::{CosineFailure, CosineSide, cosine};
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
/// #1917 gave this site a dispatched reduction. #1922 finished the job: the
/// whole computation is now [`calyx_forge::cpu::distance::cosine`], and the
/// only thing left here is the map from that function's typed
/// [`CosineFailure`] onto Loom's own error codes.
///
/// That mapping was the actual barrier to consolidation, and it is worth naming
/// why. `paired_cosine_batch` was already the correct primitive, but it returned
/// a `ForgeError`, which Loom cannot emit — Loom must return `CALYX_LOOM_*`. The
/// only way to bridge that without a typed failure was to match on the error's
/// message text, so instead each of four sites re-derived shape validation,
/// finiteness classification, the zero-norm threshold and the range contract for
/// itself. Three of them then had to be found and fixed one at a time (#1908,
/// #1912, #1917), and the fusion argument had to be written out twice because
/// there was nowhere to put it once.
///
/// Two behaviours are strictly stronger than the pre-unification code, both
/// inherited from the shared entry point rather than re-implemented:
///
/// - two finite vectors whose squares overflow `f32` used to divide by an
///   infinite norm and silently yield `0.0` or `NaN`; that is now a named
///   refusal. A wrong agreement written to the XTerm CF is worse than a refused
///   one.
/// - the quotient now passes through [`calyx_core::clamp_cosine_quotient`], so
///   this site gets #1923's range contract that it never had — a pair of
///   identical vectors can no longer emit a score just above `1.0`.
pub fn agreement_scalar(a: &[f32], b: &[f32]) -> Result<f32> {
    // #1922: the computation is shared; only the error vocabulary is Loom's.
    // Everything this function used to own itself — the dimension check, the
    // dispatched reduction, the fused per-side finiteness attribution, the
    // zero-norm threshold, the `[-1,1]` range contract — now lives once in
    // `calyx_forge::cpu::distance::cosine`, and a fifth call site cannot get
    // any of it wrong because there is nothing left for it to re-decide.
    cosine(a, b).map_err(agreement_failure_error)
}

/// Maps the shared primitive's typed failure onto Loom's error codes.
///
/// This mapping is the entire remaining Loom-specific surface of an agreement
/// cosine, and it is deliberately exhaustive: a new [`CosineFailure`] variant
/// breaks this match rather than falling through to a generic code.
///
/// Every code is preserved from the pre-unification implementation so manual
/// FSV keeps measuring the same contract. `Empty` maps
/// to the **dimension** code, not zero-norm: Loom's `ensure_same_dim` refused
/// `a.is_empty()` in the same branch as a length disagreement, and a
/// consolidation that quietly moved an empty pair to a different code would be
/// exactly the silent behaviour change #1922 asks not to make.
fn agreement_failure_error(failure: CosineFailure) -> calyx_core::CalyxError {
    match failure {
        CosineFailure::DimMismatch { left, right } => loom_error(
            CALYX_LOOM_DIM_MISMATCH,
            format!("xterm dims {left} and {right}"),
        ),
        CosineFailure::Empty => loom_error(CALYX_LOOM_DIM_MISMATCH, "xterm dims 0 and 0"),
        CosineFailure::NonFinite { side, index } => loom_error(
            CALYX_LOOM_NON_FINITE_VECTOR,
            format!(
                "xterm {} vector element {index} is non-finite",
                side.label()
            ),
        ),
        CosineFailure::NormOverflow { side } => loom_error(
            CALYX_LOOM_NON_FINITE_VECTOR,
            format!(
                "xterm {} vector norm is non-finite even though every element is finite: the \
                 accumulation overflowed f32 range",
                side.label()
            ),
        ),
        CosineFailure::Overflow => loom_error(
            CALYX_LOOM_NON_FINITE_VECTOR,
            "agreement dot product is non-finite over finite elements on both sides: the \
             accumulation overflowed f32 range",
        ),
        CosineFailure::ZeroNorm { .. } => loom_error(
            CALYX_LOOM_ZERO_NORM_VECTOR,
            "agreement requires non-zero vectors",
        ),
        CosineFailure::OutOfRange { cosine } => loom_error(
            CALYX_LOOM_NON_FINITE_VECTOR,
            format!(
                "agreement produced cosine {cosine}, further outside [-1,1] than f32 rounding \
                 explains; this is a defect in the inputs or the reduction, not a value to clamp"
            ),
        ),
    }
}

/// Which side of an agreement pair measured to exactly zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZeroNormSide {
    A,
    B,
}

impl ZeroNormSide {
    pub const fn label(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

/// The outcome of one agreement cosine, with zero-norm split out from failure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AgreementOutcome {
    Scored(f32),
    /// One side is a valid, exact measurement that happens to have no
    /// direction, so no cosine over it exists. This is not a defect in the
    /// input and not a computation that failed — it is a pair that has no
    /// agreement to contribute.
    ZeroNorm(ZeroNormSide),
}

/// [`agreement_scalar`], with the zero-norm case returned rather than raised.
///
/// # Why this exists (#2076)
///
/// A slot vector of exactly zero is a legitimate measurement on this system's
/// frozen scalar lenses: `syn_scalar_zscore` with `mean_micros = 0` maps a
/// keystroke count of 0 to `[0.0]`, and `syn_scalar_raw` maps an interruption
/// ratio of 0.0 to `[0.0]`. Those are the true values, exactly encoded — an
/// episode really did have zero keystrokes. They have no *direction*, so no
/// cosine over them exists, but nothing about them is broken and there is
/// nothing for a caller to repair.
///
/// The between-record kNN lane already draws exactly this distinction and
/// excludes such records from the geometric lane with a counted, reported
/// exclusion. The within-record agreement lane did not, so one zero-valued
/// scalar slot on one episode aborted the weave of an entire panel — on this
/// vault, 301 in-window episodes produced no association rows at all, six ticks
/// out of six, because some of them had no keystrokes.
///
/// A caller asking for *one named* cross-term still gets the loud
/// [`crate::error::CALYX_LOOM_ZERO_NORM_VECTOR`] refusal from
/// [`agreement_scalar`]: it asked a question with no answer and must be told.
/// A bulk weave over an unattended corpus uses this, skips the pair, and
/// accounts for it.
///
/// # Errors
///
/// Every non-zero-norm [`CosineFailure`], mapped exactly as
/// [`agreement_scalar`] maps it. A dimension mismatch, a non-finite element or
/// an out-of-range quotient is still a defect and still fails closed.
pub fn agreement_scalar_classified(a: &[f32], b: &[f32]) -> Result<AgreementOutcome> {
    match cosine(a, b) {
        Ok(value) => Ok(AgreementOutcome::Scored(value)),
        Err(CosineFailure::ZeroNorm { side }) => Ok(AgreementOutcome::ZeroNorm(match side {
            CosineSide::Left => ZeroNormSide::A,
            CosineSide::Right => ZeroNormSide::B,
        })),
        Err(failure) => Err(agreement_failure_error(failure)),
    }
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
