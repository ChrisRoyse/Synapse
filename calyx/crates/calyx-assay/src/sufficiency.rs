//! Panel sufficiency and deficit routing.

mod joint;

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{Anchor, AnchorKind, CalyxError, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::attribution::SlotAttribution;
use crate::calibration::{PowerCalibration, PowerCalibrationStatus, underpowered};
use crate::estimate::{
    EstimateBound, MiEstimate, TrustTag, provisional_without_anchor, trust_for_anchor,
};

pub use joint::{PanelJointBasis, panel_joint_with_union_floor};

pub const CALYX_ASSAY_INVALID_SCOPE: &str = "CALYX_ASSAY_INVALID_SCOPE";

/// Units in the last place within which two independently-rounded `f32`
/// estimates of the same quantity are treated as indistinguishable.
///
/// This is deliberately *not* a bit budget. The sufficiency contract is
/// `sufficient <=> I(panel;anchor) >= H(anchor)` with no slack, and that stays
/// exact. What this constant bounds is the *instrument*, not the threshold: a
/// panel whose probe reproduces the outcome on the held-out folds computes one
/// quantity twice, by two different code paths (`I` through the probe, `H`
/// through `entropy_bits`), and the two results differ by their own accumulated
/// rounding. #1945 measured that gap at 6.0e-8 bits on the real corpus — under
/// one ulp at that magnitude — and an exact `>=` turned it into a `false`
/// verdict on a panel that measures the outcome exactly.
///
/// Four ulps sits inside the 1–5 range the float-comparison literature
/// recommends for values separated by a handful of rounding steps. Because the
/// tolerance is `EPSILON * magnitude * ulps`, it scales with the operands: at
/// the ~1-bit magnitudes this code sees it is ~5e-7 bits, so the 0.001-bit
/// genuine shortfall the contract must still reject is three orders of
/// magnitude above it and reports insufficient exactly as before.
const SUFFICIENCY_RESOLUTION_ULPS: f32 = 4.0;

/// The numerical resolution shared by two `f32` estimates at their own
/// magnitude: `EPSILON * max(|left|,|right|) * ULPS`.
///
/// A relative bound rather than an absolute one, because the granularity of an
/// `f32` changes with its exponent — an absolute epsilon that is right at 1 bit
/// is far too coarse at 1e-3 bits and finer than the representation at 1e3.
/// Values at or below the smallest normal magnitude fall back to that
/// magnitude, so the bound never collapses to zero and never widens near zero.
pub fn estimator_resolution_bits(left: f32, right: f32) -> f32 {
    let magnitude = left.abs().max(right.abs()).max(f32::MIN_POSITIVE);
    f32::EPSILON * magnitude * SUFFICIENCY_RESOLUTION_ULPS
}

/// Render a bits deficit so that a non-zero shortfall can never display as
/// zero.
///
/// #1945's second, independent defect: the capability card printed
/// `deficit_bits={:.6}`, under which every real deficit below 5e-7 bits renders
/// as `0.000000`. That is not a rounding nicety — it produces "insufficient, by
/// nothing", which is unactionable in both directions: a reader cannot tell
/// whether the panel is short and by how much, or whether the verdict is noise.
///
/// Below the fixed-point resolution the value switches to scientific notation,
/// which has no such floor. An exact zero still prints as zero, because that
/// one is a real measurement rather than a rounded-away one.
///
/// Lives here rather than in the one instrument that had the bug because the
/// trap belongs to the quantity, not to that renderer: any consumer formatting
/// a bits deficit at fixed precision hits it.
pub fn format_deficit_bits(value: f32, precision: usize) -> String {
    if value == 0.0 {
        return format!("{value:.precision$}");
    }
    let smallest_representable = 0.5 * 10f32.powi(-(i32::try_from(precision).unwrap_or(i32::MAX)));
    if value.abs() < smallest_representable {
        format!("{value:.3e}")
    } else {
        format!("{value:.precision$}")
    }
}

/// Which of the three mutually exclusive numerical situations produced a
/// sufficiency verdict. Reported alongside the verdict so a consumer can tell a
/// panel that genuinely clears the anchor from one that ties it inside the
/// estimator's own resolution — the two are the same `sufficient=true` and are
/// not the same finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SufficiencyVerdictBasis {
    /// The basis exceeds anchor entropy by more than the estimators' shared
    /// resolution: sufficient, and resolvably so.
    ExceedsAnchorEntropy,
    /// The two estimates differ by no more than their shared resolution. A
    /// difference the instrument cannot resolve is not a difference, so this is
    /// sufficient, and the reported deficit is exactly zero.
    WithinEstimatorResolution,
    /// A shortfall the instrument can resolve: insufficient.
    ShortOfAnchorEntropy,
}

impl SufficiencyVerdictBasis {
    pub fn is_sufficient(self) -> bool {
        !matches!(self, Self::ShortOfAnchorEntropy)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExceedsAnchorEntropy => "exceeds_anchor_entropy",
            Self::WithinEstimatorResolution => "within_estimator_resolution",
            Self::ShortOfAnchorEntropy => "short_of_anchor_entropy",
        }
    }
}

/// Decide `basis_bits >= anchor_entropy_bits` on a resolution-aware basis.
///
/// Returns the verdict basis, the resolution that decided it, and the deficit —
/// which is exactly `0.0` whenever the verdict is sufficient, so no consumer can
/// observe the self-contradicting "insufficient by nothing" state of #1945.
pub fn sufficiency_verdict(
    basis_bits: f32,
    anchor_entropy_bits: f32,
) -> (SufficiencyVerdictBasis, f32, f32) {
    let resolution_bits = estimator_resolution_bits(basis_bits, anchor_entropy_bits);
    let gap_bits = anchor_entropy_bits - basis_bits;
    if gap_bits.abs() <= resolution_bits {
        return (
            SufficiencyVerdictBasis::WithinEstimatorResolution,
            resolution_bits,
            0.0,
        );
    }
    if gap_bits < 0.0 {
        return (
            SufficiencyVerdictBasis::ExceedsAnchorEntropy,
            resolution_bits,
            0.0,
        );
    }
    (
        SufficiencyVerdictBasis::ShortOfAnchorEntropy,
        resolution_bits,
        gap_bits,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeficitSuggestedAction {
    AddOutcomeAnchor,
    ProposeLens,
    IncreaseSamples,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeficitRoutingContext {
    pub panel_id: String,
    pub anchor: AnchorKind,
    pub computed_at_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_scope: Option<ObservationScope>,
}

impl Default for DeficitRoutingContext {
    fn default() -> Self {
        Self {
            panel_id: "panel:unspecified".to_string(),
            anchor: AnchorKind::Reward,
            computed_at_seq: 0,
            observation_scope: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationScope {
    pub id: String,
    pub observed: usize,
    pub total: usize,
}

impl ObservationScope {
    pub fn new(id: impl Into<String>, observed: usize, total: usize) -> Result<ObservationScope> {
        let scope = Self {
            id: id.into(),
            observed,
            total,
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn coverage_rate(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.observed as f32 / self.total as f32
        }
    }

    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(invalid_scope("observation scope id must not be empty"));
        }
        if self.observed > self.total {
            return Err(invalid_scope(format!(
                "scope {} observed {} rows but total is {}",
                self.id, self.observed, self.total
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SufficiencyDeficit {
    pub panel_id: String,
    pub anchor: AnchorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_scope: Option<ObservationScope>,
    pub slot: Option<SlotId>,
    pub per_slot_gaps: BTreeMap<SlotId, f32>,
    pub deficit_bits: f32,
    pub suggested_action: DeficitSuggestedAction,
    pub computed_at_seq: u64,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelSufficiency {
    pub panel_bits: f32,
    pub sufficiency_basis_bits: f32,
    pub anchor_entropy_bits: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_scope: Option<ObservationScope>,
    pub sufficient: bool,
    pub deficit_bits: f32,
    /// Which numerical situation produced `sufficient`. A `sufficient=true`
    /// carried by `WithinEstimatorResolution` is a tie inside the instrument's
    /// own precision, not a panel that clears the anchor with room to spare.
    #[serde(default = "default_verdict_basis")]
    pub verdict_basis: SufficiencyVerdictBasis,
    /// The `f32` resolution the two estimates share at their own magnitude,
    /// which is the quantity the verdict was decided against.
    #[serde(default)]
    pub estimator_resolution_bits: f32,
    pub deficits: Vec<SufficiencyDeficit>,
    pub trust: TrustTag,
    pub estimate_bound: EstimateBound,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_calibration: Option<PowerCalibration>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SufficiencyScopeInput {
    pub scope: ObservationScope,
    pub panel_bits: f32,
    pub anchor_entropy_bits: f32,
    pub slots: Vec<SlotAttribution>,
    pub trust: TrustTag,
    pub context: DeficitRoutingContext,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopedSufficiencyReport {
    pub scopes: Vec<PanelSufficiency>,
    pub best_scope: Option<ObservationScope>,
    pub sufficient_scopes: Vec<ObservationScope>,
}

pub trait SufficiencyDeficitSink {
    fn record_deficit(&mut self, deficit: SufficiencyDeficit);
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InMemoryDeficitSink {
    pub routed: Vec<SufficiencyDeficit>,
}

impl SufficiencyDeficitSink for InMemoryDeficitSink {
    fn record_deficit(&mut self, deficit: SufficiencyDeficit) {
        self.routed.push(deficit);
    }
}

impl PanelSufficiency {
    pub fn route_to<S: SufficiencyDeficitSink>(&self, sink: &mut S) {
        for deficit in &self.deficits {
            sink.record_deficit(deficit.clone());
        }
    }
}

pub fn panel_sufficiency(
    panel_bits: f32,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    trust: TrustTag,
) -> PanelSufficiency {
    panel_sufficiency_with_trust(
        panel_bits,
        anchor_entropy_bits,
        slots,
        provisional_without_anchor(trust),
        DeficitRoutingContext::default(),
    )
}

pub fn panel_sufficiency_from_estimate(
    estimate: &MiEstimate,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    trust: TrustTag,
) -> Result<PanelSufficiency> {
    panel_sufficiency_from_estimate_with_context(
        estimate,
        anchor_entropy_bits,
        slots,
        trust,
        DeficitRoutingContext::default(),
    )
}

pub fn panel_sufficiency_from_estimate_with_context(
    estimate: &MiEstimate,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    trust: TrustTag,
    context: DeficitRoutingContext,
) -> Result<PanelSufficiency> {
    let calibration = passing_calibration(estimate)?;
    Ok(panel_sufficiency_with_trust_and_basis(
        SufficiencyBasis {
            panel_bits: estimate.bits,
            sufficiency_basis_bits: estimate.ci_low,
            estimate_bound: estimate.bound,
            power_calibration: Some(calibration),
        },
        anchor_entropy_bits,
        slots,
        provisional_without_anchor(trust),
        context,
    ))
}

pub fn panel_sufficiency_with_anchor(
    panel_bits: f32,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    anchor: &Anchor,
) -> PanelSufficiency {
    panel_sufficiency_with_trust(
        panel_bits,
        anchor_entropy_bits,
        slots,
        trust_for_anchor(Some(anchor)),
        DeficitRoutingContext::default(),
    )
}

pub fn panel_sufficiency_with_context(
    panel_bits: f32,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    trust: TrustTag,
    context: DeficitRoutingContext,
) -> PanelSufficiency {
    panel_sufficiency_with_trust(
        panel_bits,
        anchor_entropy_bits,
        slots,
        provisional_without_anchor(trust),
        context,
    )
}

pub fn panel_sufficiency_with_anchor_and_context(
    panel_bits: f32,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    anchor: &Anchor,
    context: DeficitRoutingContext,
) -> PanelSufficiency {
    panel_sufficiency_with_trust(
        panel_bits,
        anchor_entropy_bits,
        slots,
        trust_for_anchor(Some(anchor)),
        context,
    )
}

pub fn panel_sufficiency_by_scope(
    inputs: Vec<SufficiencyScopeInput>,
) -> Result<ScopedSufficiencyReport> {
    if inputs.is_empty() {
        return Err(invalid_scope(
            "sufficiency scope report requires at least one scope",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut scopes = Vec::with_capacity(inputs.len());
    for input in inputs {
        input.scope.validate()?;
        if !seen.insert(input.scope.id.clone()) {
            return Err(invalid_scope(format!(
                "duplicate observation scope {}",
                input.scope.id
            )));
        }
        let mut context = input.context;
        context.observation_scope = Some(input.scope);
        scopes.push(panel_sufficiency_with_trust(
            input.panel_bits,
            input.anchor_entropy_bits,
            &input.slots,
            provisional_without_anchor(input.trust),
            context,
        ));
    }
    let best_scope = scopes
        .iter()
        .min_by(|left, right| left.deficit_bits.total_cmp(&right.deficit_bits))
        .and_then(|scope| scope.observation_scope.clone());
    let sufficient_scopes = scopes
        .iter()
        .filter(|scope| scope.sufficient)
        .filter_map(|scope| scope.observation_scope.clone())
        .collect();
    Ok(ScopedSufficiencyReport {
        scopes,
        best_scope,
        sufficient_scopes,
    })
}

fn panel_sufficiency_with_trust(
    panel_bits: f32,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    trust: TrustTag,
    context: DeficitRoutingContext,
) -> PanelSufficiency {
    panel_sufficiency_with_trust_and_basis(
        SufficiencyBasis::diagnostic(panel_bits),
        anchor_entropy_bits,
        slots,
        trust,
        context,
    )
}

struct SufficiencyBasis {
    panel_bits: f32,
    sufficiency_basis_bits: f32,
    estimate_bound: EstimateBound,
    power_calibration: Option<PowerCalibration>,
}

impl SufficiencyBasis {
    fn diagnostic(panel_bits: f32) -> Self {
        Self {
            panel_bits,
            sufficiency_basis_bits: panel_bits,
            estimate_bound: EstimateBound::Point,
            power_calibration: None,
        }
    }
}

fn panel_sufficiency_with_trust_and_basis(
    basis: SufficiencyBasis,
    anchor_entropy_bits: f32,
    slots: &[SlotAttribution],
    trust: TrustTag,
    context: DeficitRoutingContext,
) -> PanelSufficiency {
    // #1945: the two sides are two f32 estimates of one quantity produced by
    // two code paths. An exact `>=` between them decides the verdict on their
    // accumulated rounding, and it fails in the direction that matters — a
    // panel that recovers the outcome exactly is reported as falling short.
    // The threshold itself is unchanged and carries no slack; only the
    // instrument's resolution is accounted for.
    let (verdict_basis, estimator_resolution_bits, deficit_bits) =
        sufficiency_verdict(basis.sufficiency_basis_bits, anchor_entropy_bits);
    let sufficient = verdict_basis.is_sufficient();
    let deficits = if sufficient {
        Vec::new()
    } else {
        localized_deficits(deficit_bits, slots, &context)
    };
    PanelSufficiency {
        panel_bits: basis.panel_bits,
        sufficiency_basis_bits: basis.sufficiency_basis_bits,
        anchor_entropy_bits,
        observation_scope: context.observation_scope.clone(),
        sufficient,
        deficit_bits,
        verdict_basis,
        estimator_resolution_bits,
        deficits,
        trust,
        estimate_bound: basis.estimate_bound,
        power_calibration: basis.power_calibration,
    }
}

fn passing_calibration(estimate: &MiEstimate) -> Result<PowerCalibration> {
    let calibration = estimate.power_calibration.clone().ok_or_else(|| {
        underpowered("panel sufficiency requires a passing planted-signal power calibration")
    })?;
    if calibration.status != PowerCalibrationStatus::Passed {
        return Err(underpowered(format!(
            "panel sufficiency estimator calibration status is {:?}",
            calibration.status
        )));
    }
    calibration.require_passed()?;
    Ok(calibration)
}

pub fn entropy_bits<T>(labels: &[T]) -> f32
where
    T: Ord + Copy,
{
    let mut counts = BTreeMap::<T, usize>::new();
    for label in labels {
        *counts.entry(*label).or_default() += 1;
    }
    let n = labels.len().max(1) as f32;
    counts
        .values()
        .map(|count| {
            let p = *count as f32 / n;
            -p * p.log2()
        })
        .sum()
}

fn localized_deficits(
    deficit_bits: f32,
    slots: &[SlotAttribution],
    context: &DeficitRoutingContext,
) -> Vec<SufficiencyDeficit> {
    if slots.is_empty() {
        return vec![SufficiencyDeficit {
            panel_id: context.panel_id.clone(),
            anchor: context.anchor.clone(),
            observation_scope: context.observation_scope.clone(),
            slot: None,
            per_slot_gaps: BTreeMap::new(),
            deficit_bits,
            suggested_action: DeficitSuggestedAction::AddOutcomeAnchor,
            computed_at_seq: context.computed_at_seq,
            reason: "panel below anchor entropy".to_string(),
        }];
    }
    let per_slot_gaps = per_slot_gap_map(deficit_bits, slots);
    let total_missing_weight: f32 = slots
        .iter()
        .map(|slot| 1.0 / (slot.marginal_bits + 0.01))
        .sum();
    slots
        .iter()
        .map(|slot| {
            let weight = 1.0 / (slot.marginal_bits + 0.01);
            SufficiencyDeficit {
                panel_id: context.panel_id.clone(),
                anchor: context.anchor.clone(),
                observation_scope: context.observation_scope.clone(),
                slot: Some(slot.slot),
                per_slot_gaps: per_slot_gaps.clone(),
                deficit_bits: deficit_bits * weight / total_missing_weight,
                suggested_action: DeficitSuggestedAction::ProposeLens,
                computed_at_seq: context.computed_at_seq,
                reason: "slot marginal bits below sufficiency need".to_string(),
            }
        })
        .collect()
}

fn per_slot_gap_map(deficit_bits: f32, slots: &[SlotAttribution]) -> BTreeMap<SlotId, f32> {
    let total_missing_weight: f32 = slots
        .iter()
        .map(|slot| 1.0 / (slot.marginal_bits + 0.01))
        .sum();
    slots
        .iter()
        .map(|slot| {
            let weight = 1.0 / (slot.marginal_bits + 0.01);
            (slot.slot, deficit_bits * weight / total_missing_weight)
        })
        .collect()
}

fn default_verdict_basis() -> SufficiencyVerdictBasis {
    SufficiencyVerdictBasis::ShortOfAnchorEntropy
}

fn invalid_scope(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ASSAY_INVALID_SCOPE,
        message: message.into(),
        remediation: "provide unique observation scopes with observed <= total",
    }
}

/// A lens that **is** the anchor, rather than one that predicts it.
///
/// Returned by [`detect_anchor_leakage`]; see that function for what the fields
/// mean and why each is part of the signature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnchorLeakage {
    /// Panel slot carrying the leaking lens.
    pub slot: u16,
    /// The lens's measured marginal information about the anchor.
    pub lens_bits: f32,
    /// The anchor's own entropy. Leakage is `lens_bits == anchor_entropy_bits`.
    pub anchor_entropy_bits: f32,
    /// The resolution at which those two were judged equal.
    pub resolution_bits: f32,
    /// Distinct values the lens takes over the measured corpus.
    pub lens_distinct_values: usize,
    /// Distinct outcome labels the anchor takes over the same corpus.
    pub anchor_distinct_outcomes: usize,
}

/// Detects a lens that carries the anchor itself rather than evidence about it.
///
/// ## Why this exists
///
/// Measured 2026-08-02 on `syn-mcp-usage-v1 @ 1776006` — the corpus chosen
/// *because* it clears every grounding floor. `sufficiency` reported
/// `sufficient = true, deficit_bits = 0`, with `panel_bits` equal to
/// `anchor_entropy_bits` to every digit (0.969319224357605). The carrier was
/// slot 86, `syn.mcp_usage.status_onehot.v1`.
///
/// The anchor `synapse:mcp_tool_call_outcome` is built from `&record.status`;
/// slot 86 is a one-hot of `record["status"]`. The same field. The panel
/// "predicted" the outcome because it contained the outcome (#1953).
///
/// Nothing in the estimator was wrong — it correctly reported the bits in the
/// column it was handed. The defect is that a circular result was indis-
/// tinguishable from a real one, and only a human noticing an implausibly exact
/// equality caught it. That is what this turns into a check.
///
/// ## The signature, and why each clause is required
///
/// All must hold:
///
/// 1. **`lens_bits == anchor_entropy_bits` within estimator resolution.** A
///    lens cannot carry more about the anchor than the anchor carries about
///    itself, so equality is the ceiling — reaching it exactly means the column
///    determines the label.
/// 2. **Matching cardinality.** A lens can legitimately reach the ceiling by
///    being a perfect *predictor* with different structure (many values mapping
///    onto few outcomes). Requiring `lens_distinct_values ==
///    anchor_distinct_outcomes` separates "is the label" from "predicts the
///    label perfectly", and only the first is leakage.
/// 3. **A non-degenerate anchor.** With `anchor_entropy_bits == 0` every lens
///    trivially measures 0 bits and clause 1 would match everything. A
///    single-valued anchor is its own problem and is not this one.
///
/// Deliberately reports rather than decides: a caller may have a legitimate
/// reason to measure a panel that includes its own label (auditing the encoder,
/// for instance). What it must not do is report `sufficient` from it silently.
#[must_use]
pub fn detect_anchor_leakage(
    slot: u16,
    lens_bits: f32,
    lens_distinct_values: usize,
    anchor_entropy_bits: f32,
    anchor_distinct_outcomes: usize,
) -> Option<AnchorLeakage> {
    if !lens_bits.is_finite() || !anchor_entropy_bits.is_finite() {
        return None;
    }
    // Clause 3: a degenerate anchor makes every lens look like a leak.
    if anchor_entropy_bits <= 0.0 || anchor_distinct_outcomes < 2 {
        return None;
    }
    // Clause 2: same shape, not merely the same score.
    if lens_distinct_values != anchor_distinct_outcomes {
        return None;
    }
    // Clause 1: at the ceiling, within the resolution those two f32s share.
    let resolution_bits = estimator_resolution_bits(lens_bits, anchor_entropy_bits);
    if (anchor_entropy_bits - lens_bits).abs() > resolution_bits {
        return None;
    }
    Some(AnchorLeakage {
        slot,
        lens_bits,
        anchor_entropy_bits,
        resolution_bits,
        lens_distinct_values,
        anchor_distinct_outcomes,
    })
}
