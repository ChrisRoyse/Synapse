mod a37;
mod compute;
mod model;
mod redundancy;

pub use a37::{
    A37_DIVERSITY_DIAGNOSTIC_ONLY, A37_DIVERSITY_GATE_PASSED, A37_DIVERSITY_SCHEMA_VERSION,
    A37DiversityGate, a37_association_family, a37_diversity_gate,
};
pub use compute::{CALYX_ASSAY_PANEL_TOO_SMALL, ensemble_card, ensemble_card_with_redundancy};
pub use model::{
    DEFAULT_GATE_PANEL_LENSES, DEFAULT_MAX_REDUNDANCY, DEFAULT_MIN_MARGINAL_BITS, DeficitProposal,
    ENSEMBLE_CARD_PID_METHOD, ENSEMBLE_CARD_SCHEMA_VERSION, EnsembleCard, EnsembleConfig,
    EnsembleDecision, EnsembleLensInput, EnsembleLensRole, EnsembleLensValue,
    EnsemblePairRedundancyEvidence, EnsemblePairValue, EnsembleRedundancyEvidence,
    EnsembleRedundancyMethod, LinearCkaEstimate, MIN_ENSEMBLE_PANEL_LENSES, PidBits,
};
pub use redundancy::{
    DEFAULT_LINEAR_CKA_SEED, EnsembleRedundancySketchInput, LINEAR_CKA_JACKKNIFE_BLOCKS,
    LINEAR_CKA_REDUNDANCY_METHOD, LINEAR_CKA_TUPLES_PER_ROW, LinearCkaSketch, LinearCkaTuplePlan,
    MAX_LINEAR_CKA_TUPLES, MIN_LINEAR_CKA_TUPLES, ensemble_nmi_signature,
    ensemble_redundancy_from_lenses, ensemble_redundancy_from_lenses_cuda_strict,
    ensemble_redundancy_from_sketches, linear_cka_sketch_from_row_fn, linear_cka_sketch_from_rows,
    linear_cka_tuple_plan, validate_ensemble_card_redundancy, validate_redundancy_method_metadata,
};
