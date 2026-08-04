//! Oracle consequence prediction and completion primitives.

mod butterfly;
mod complete;
mod energy;
mod error;
mod evidence;
mod evidence_error;
mod honesty_gate;
mod prd22;
mod predict;
mod reverse_query;
mod self_consistency;
mod super_intel;
mod super_intel_full;
mod super_intel_types;
mod time_prediction;
mod types;

pub use butterfly::{
    HOP_ATTENUATION, MAX_DEPTH, MIN_CONFIDENCE_THRESHOLD, build_tree, expand,
    is_provisional_ledger_ref, provisional_ledger_ref, select,
};
pub use complete::{
    COMPLETION_LEDGER_TAG, CompletionLedger, CompletionLedgerPayload, CompletionRegion,
    WardCompletionRegion, complete, complete_with_assay_and_region,
};
pub use energy::{
    AnnealConfig, CALYX_ORACLE_ENERGY_EMPTY_REGION, CALYX_ORACLE_ENERGY_INVALID_INPUT,
    DEFAULT_BETA, DEFAULT_EPS, DescentResult, MAX_STEPS, descend, descent_step, energy,
    energy_softmax_weights, get_beta,
};
pub use error::{
    CALYX_ORACLE_DOMAIN_NOT_FOUND, CALYX_ORACLE_EVIDENCE_CORRUPT, CALYX_ORACLE_FLAKY_ANCHOR,
    CALYX_ORACLE_INSUFFICIENT, CALYX_ORACLE_LEDGER_WRITE_FAILURE, CALYX_ORACLE_NO_CAUSES_FOUND,
    CALYX_ORACLE_NO_RECURRENCE, CALYX_ORACLE_SLOT_CONFLICT, CALYX_ORACLE_STORAGE_READ_FAILURE,
    OracleError,
};

pub use honesty_gate::{
    SufficiencyAssay, VaultSufficiencyAssay, check_sufficiency, check_sufficiency_with_assay,
};
pub use prd22::{
    ConsequenceExpansion, OracleCeiling, OraclePrediction, SuperIntelligenceEvidence,
    SuperIntelligenceVerdict, butterfly_expand, oracle_ceiling,
    oracle_predict as oracle_formula_predict, reverse_query as reverse_query_formula,
    super_intelligence as super_intelligence_formula,
};
pub use predict::{Action, ORACLE_ACTION_METADATA_KEY, oracle_predict};
pub use reverse_query::{
    MAX_REVERSE_DEPTH, ORACLE_EFFECT_METADATA_KEY, ORACLE_STRUCTURAL_CONFIDENCE_METADATA_KEY,
    reverse_query,
};
pub use self_consistency::{
    MIN_FLAKINESS_PAIRS, MIN_VALIDITY_SAMPLES, ORACLE_DOMAIN_METADATA_KEY,
    ORACLE_FALLBACK_DOMAIN_METADATA_KEY, oracle_self_consistency,
    oracle_self_consistency_read_only,
};
pub use super_intel::{
    HeldOutSplit, KERNEL_RECALL_RATIO, KernelRecallGate, KernelRecallSource,
    ORACLE_CLEAN_THRESHOLD, OracleConsistencySource, ShortCircuit, TierMeasurementRequest,
    measure_super_intelligence_tiers_1_to_3, measure_tier_kernel_exists, measure_tier_oracle_clean,
    measure_tier_oracle_clean_with_source, measure_tier_panel_sufficient,
    measure_tier_panel_sufficient_with_assay, measure_tiers_1_to_3,
};
pub use super_intel_full::{
    CALIBRATION_BUDGET, CalibrationMeasurement, CalibrationSource, GOODHART_THRESHOLD,
    GoodhartDefenseMeasurement, GoodhartDefenseSource, MistakeClosureMeasurement,
    MistakeClosureSource, SuperIntelligenceRequest, measure_super_intelligence_tiers,
    measure_tier_calibrated, measure_tier_goodhart_defended, measure_tier_mistake_closed,
    super_intelligence, super_intelligence_with_ledger, write_super_intelligence_ledger,
};
pub use super_intel_types::{Cause, SuperIntelReport, Tier, TierResult};
pub use time_prediction::{
    MIN_TIME_PREDICTION_OCCURRENCES, TimeBucket, TimePrediction, TimePredictionInterval,
    predict_next_occurrence, predict_next_occurrence_from_series,
    predict_next_occurrence_from_series_with_tz_offset, predict_next_occurrence_with_tz_offset,
    time_bucket,
};
pub use types::{
    Bits, CompletionResult, CompletionSlotPartition, Consequence, ConsequenceTree,
    DEFAULT_CONSEQUENCE_TREE_MAX_DEPTH, DomainId, OracleSelfConsistency, Prediction, SlotSet,
    SlotTag, SufficiencyBound, TaggedSlot, UnitInterval,
};
