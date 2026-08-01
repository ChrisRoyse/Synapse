use calyx_core::SlotId;
use serde::{Deserialize, Serialize};

use crate::LogisticConditioningProvenance;
use crate::estimate::EstimatorKind;
use crate::sufficiency::PanelSufficiency;

use super::a37::A37DiversityGate;

/// Schema 4 (#1942): every pair row carries the unclamped gain, the visible
/// monotonicity floor, and the instrument behind each of its three terms.
pub const ENSEMBLE_CARD_SCHEMA_VERSION: u32 = 4;
pub const ENSEMBLE_CARD_PID_METHOD: &str = "bounded_decision_surrogate_v1";
pub const MIN_ENSEMBLE_PANEL_LENSES: usize = 3;
pub const DEFAULT_GATE_PANEL_LENSES: usize = 10;
pub const DEFAULT_MIN_MARGINAL_BITS: f32 = 0.05;
pub const DEFAULT_MAX_REDUNDANCY: f32 = 0.6;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsembleLensInput {
    pub name: String,
    pub slot: SlotId,
    #[serde(default)]
    pub role: EnsembleLensRole,
    pub vectors: Vec<Vec<f32>>,
}

impl EnsembleLensInput {
    pub fn new(name: impl Into<String>, slot: SlotId, vectors: Vec<Vec<f32>>) -> Self {
        Self {
            name: name.into(),
            slot,
            role: EnsembleLensRole::Content,
            vectors,
        }
    }

    pub fn with_role(mut self, role: EnsembleLensRole) -> Self {
        self.role = role;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnsembleLensRole {
    #[default]
    Content,
    TemporalSidecar,
}

impl EnsembleLensRole {
    pub const fn is_content(self) -> bool {
        matches!(self, Self::Content)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsembleConfig {
    pub source: String,
    pub min_gate_lenses: usize,
    pub min_marginal_bits: f32,
    pub max_redundancy: f32,
    pub nmi_bins: usize,
}

impl Default for EnsembleConfig {
    fn default() -> Self {
        Self {
            source: "assay_ensemble_card".to_string(),
            min_gate_lenses: DEFAULT_GATE_PANEL_LENSES,
            min_marginal_bits: DEFAULT_MIN_MARGINAL_BITS,
            max_redundancy: DEFAULT_MAX_REDUNDANCY,
            nmi_bins: 10,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsembleCard {
    pub schema_version: u32,
    pub source: String,
    pub pid_method: String,
    pub panel_lens_count: usize,
    pub n_samples: usize,
    pub anchor_entropy_bits: f32,
    pub panel_bits: f32,
    pub panel_ci: [f32; 2],
    pub n_eff: f32,
    pub sufficient: bool,
    pub deficit_bits: f32,
    #[serde(default)]
    pub conditioning: LogisticConditioningProvenance,
    pub a37_diversity: A37DiversityGate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy_method: Option<EnsembleRedundancyMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deficit_proposal: Option<DeficitProposal>,
    pub sufficiency: PanelSufficiency,
    pub lenses: Vec<EnsembleLensValue>,
    pub pairs: Vec<EnsemblePairValue>,
    /// Pairs whose raw gain was negative and got floored at zero (#1942).
    ///
    /// Derived from the rows, so the summary can never disagree with them. A
    /// non-zero count is a statement about the *instrument*, not the panel:
    /// the joint fit came out below a marginal fit it cannot be below.
    pub pairs_monotonicity_floored: usize,
    pub keep_count: usize,
    pub park_count: usize,
    pub retire_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsembleLensValue {
    pub name: String,
    pub slot: SlotId,
    #[serde(default)]
    pub role: EnsembleLensRole,
    pub solo_bits: f32,
    pub solo_ci: [f32; 2],
    pub panel_without_bits: f32,
    pub marginal_bits: f32,
    pub marginal_ci: [f32; 2],
    pub pid: PidBits,
    pub max_pairwise_corr: f32,
    pub max_pairwise_nmi: f32,
    pub decision: EnsembleDecision,
    pub decision_reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsemblePairValue {
    pub a: String,
    pub b: String,
    pub slot_a: SlotId,
    pub slot_b: SlotId,
    /// Compatibility alias for `redundancy.mc_gate_upper_estimate` on schema v2 cards.
    pub corr: f32,
    pub nmi: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<LinearCkaEstimate>,
    pub pair_bits: f32,
    pub pair_ci: [f32; 2],
    /// `max(0, raw_synergy_gain_bits)` — the reported gain, floored by the
    /// data-processing inequality.
    ///
    /// It is a difference of two *lower bounds* (the logistic probe reports
    /// [`crate::EstimateBound::LowerBound`]), and a difference of two lower
    /// bounds is neither a lower nor an upper bound on the difference — see
    /// [`EnsemblePairValue::synergy_monotonicity_floor_applied`] for what that
    /// costs in practice. Read it as the bounded decision surrogate the card's
    /// `pid_method` names, never as a measured synergy.
    pub synergy_gain_bits: f32,
    /// `pair_bits - max(left_bits, right_bits)` exactly as measured, before the
    /// monotonicity floor (#1942).
    ///
    /// Negative here is not "the pair adds nothing". Variational MI estimators
    /// — of which the logistic probe is one — are known to *fail* the
    /// data-processing self-consistency test (Song & Ermon, *Understanding the
    /// Limitations of Variational Mutual Information Estimators*, ICLR 2020),
    /// so a negative raw gain is direct evidence that the joint fit was weaker
    /// than the marginal fit at the joint's own dimension. Silently clamping it
    /// destroys that evidence, which is why it is carried.
    pub raw_synergy_gain_bits: f32,
    /// True when `raw_synergy_gain_bits` was below zero and the floor moved it.
    pub synergy_monotonicity_floor_applied: bool,
    /// The instrument behind each of the three bit terms.
    ///
    /// Carried as a triple rather than a "homogeneous" flag so a consumer can
    /// *see* the three agree instead of taking the producer's word for it
    /// (#1941 ask 2, #1942 ask 1). Construction fails closed when they do not.
    pub synergy_estimators: EnsembleSynergyEstimators,
}

/// The instrument behind each of a pair's three bit terms.
///
/// A difference of information quantities is only interpretable when the
/// estimators' biases cancel, and that cancellation is a property of one
/// estimator applied at one scale — never of two (Kraskov, Stögbauer &
/// Grassberger, Phys. Rev. E 69 066138, 2004).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsembleSynergyEstimators {
    pub pair: EstimatorKind,
    pub left: EstimatorKind,
    pub right: EstimatorKind,
}

impl EnsembleSynergyEstimators {
    /// True when all three terms came from the same instrument, which is the
    /// precondition for their difference to be interpretable at all.
    #[must_use]
    pub fn is_homogeneous(&self) -> bool {
        self.pair == self.left && self.left == self.right
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsembleRedundancyMethod {
    pub metric: String,
    pub tuple_design: String,
    pub row_count: usize,
    pub tuple_count: usize,
    pub seed_hex: String,
    pub tuple_plan_blake3: String,
    pub exact: bool,
    pub uncertainty_method: String,
    pub uncertainty_blocks: usize,
    pub gate_score_method: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinearCkaEstimate {
    pub raw_signed_point: f32,
    pub redundancy_point: f32,
    pub mc_standard_error: f32,
    pub mc_gate_upper_estimate: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsemblePairRedundancyEvidence {
    pub a: String,
    pub b: String,
    pub slot_a: SlotId,
    pub slot_b: SlotId,
    pub linear_cka: LinearCkaEstimate,
    pub nmi: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnsembleRedundancyEvidence {
    pub method: EnsembleRedundancyMethod,
    pub pairs: Vec<EnsemblePairRedundancyEvidence>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PidBits {
    pub unique_bits: f32,
    pub redundant_bits: f32,
    pub synergistic_bits: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnsembleDecision {
    Keep,
    Park,
    Retire,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeficitProposal {
    pub action: String,
    pub deficit_bits: f32,
    pub weakest_slots: Vec<SlotId>,
    pub reason: String,
}
