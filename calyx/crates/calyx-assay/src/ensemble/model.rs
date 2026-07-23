use calyx_core::SlotId;
use serde::{Deserialize, Serialize};

use crate::LogisticConditioningProvenance;
use crate::sufficiency::PanelSufficiency;

use super::a37::A37DiversityGate;

pub const ENSEMBLE_CARD_SCHEMA_VERSION: u32 = 3;
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
    pub synergy_gain_bits: f32,
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
