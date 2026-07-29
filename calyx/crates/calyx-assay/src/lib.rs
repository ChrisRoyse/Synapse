//! Assay signal-bit measurement, panel sufficiency, and persistence contracts.

pub mod attribution;
pub mod bayesian;
pub mod bootstrap;
pub mod calibration;
pub mod categorical_association;
pub mod causal_pc;
pub mod ccm;
pub mod conditional_mi;
pub mod contract;
pub mod copula;
pub mod cross_correlation;
mod cuda_strict;
pub mod distance_correlation;
pub mod ensemble;
pub mod estimate;
pub mod formula_catalog;
pub mod formulas;
pub mod gate;
pub mod granger;
pub mod group_split;
pub mod hawkes;
pub mod hsic;
pub mod ksg;
pub mod logistic;
pub mod loom_adapter;
pub mod mic;
pub mod mmd;
pub mod n_eff;
pub mod nmi;
pub mod partial_correlation;
pub mod partial_network;
pub mod periodicity;
pub mod point_process;
pub mod projection;
pub mod rank_correlation;
pub mod recurrence_anchor;
pub mod recurrence_hazard;
pub mod resource_contract;
mod samples;
mod special_fn;
pub mod store;
pub mod stratified;
mod subsample;
pub mod sufficiency;
pub mod synergy;
pub mod total_correlation;
pub mod transfer_entropy;

pub use attribution::{
    BitsReport, CALYX_ASSAY_INVALID_COVERAGE, CoverageMask, SlotAttribution, bits_report,
    bits_report_with_anchor, per_sensor_attribution, per_sensor_attribution_with_coverage,
};
pub use bayesian::{
    BAYESIAN_POSTERIOR_KEY_PREFIX, BayesianPosteriorRow, BetaBernoulli,
    CALYX_BAYES_INVALID_INTERVAL, DEFAULT_BAYES_PRIOR_ALPHA, DEFAULT_BAYES_PRIOR_BETA,
    GammaPoisson, bayesian_posterior_for_domain, bayesian_posterior_key, beta_bernoulli_for_domain,
    gamma_poisson_for_domain, persist_bayesian_posterior,
};
pub use bootstrap::{
    BootstrapCi, BootstrapConfig, DEFAULT_BOOTSTRAP_RESAMPLES, DEFAULT_BOOTSTRAP_SEED,
    bootstrap_mean_ci, bootstrap_mean_ci_with_config, bootstrap_paired_ci,
};
pub use calibration::{
    CALYX_ASSAY_DEGENERATE_TARGET_ENTROPY, CALYX_ASSAY_ESTIMATOR_UNDERPOWERED,
    DEFAULT_MIN_POWER_RECOVERY_RATIO, MIN_INFORMATIVE_TARGET_ENTROPY_BITS, PowerCalibration,
    PowerCalibrationStatus, ensure_informative_binary_labels,
};
pub use categorical_association::{
    CategoricalReport, MIN_CATEGORICAL_SAMPLES, categorical_association, point_biserial,
};
pub use causal_pc::{
    DEFAULT_PC_ALPHA, PcEdge, PcRemovedEdge, PcSeries, PcStableReport, pc_stable_gaussian,
    pc_stable_gaussian_cuda_strict,
};
pub use ccm::{
    CcmConfig, CcmDirectionReport, CcmLibrarySkill, CcmReport, CcmVerdict,
    DEFAULT_CCM_EMBEDDING_DIM, DEFAULT_CCM_MIN_CONVERGENCE_DELTA, DEFAULT_CCM_MIN_SKILL_GAP,
    DEFAULT_CCM_TAU, convergent_cross_mapping, convergent_cross_mapping_cuda_strict,
};
pub use conditional_mi::{
    ConditionalIndependence, ConditionalMiReport, DEFAULT_CMI_ALPHA, GAUSSIAN_CMI_FORMULA,
    conditional_mutual_information_gaussian, conditional_mutual_information_gaussian_with_alpha,
    conditional_mutual_information_gaussian_with_alpha_cuda_strict,
};
pub use contract::{
    AdmissionDecision, CALYX_ASSAY_UNRESOLVED, CorrelationEvidence, admit_lens,
    admit_lens_estimate, admit_lens_estimate_with_signal_kind, admit_lens_with_strata,
};
pub use copula::{
    CopulaTailReport, DEFAULT_TAIL_Q, MIN_COPULA_SAMPLES, empirical_copula_tail_dependence,
    empirical_copula_tail_dependence_with_q,
};
pub use cross_correlation::{
    CCF_LAG_CONVENTION, CrossCorrelationPoint, CrossCorrelationReport, cross_correlation_profile,
    cross_correlation_profile_cuda_strict,
};
pub use distance_correlation::{
    DEFAULT_DCOR_PERMUTATIONS, DEFAULT_DCOR_SEED, DcorPermConfig, DcorReport, DcorTest,
    MIN_DCOR_SAMPLES, distance_correlation, distance_correlation_cuda_strict,
    distance_correlation_test, distance_correlation_test_cuda_strict,
};
pub use ensemble::{
    A37_DIVERSITY_DIAGNOSTIC_ONLY, A37_DIVERSITY_GATE_PASSED, A37_DIVERSITY_SCHEMA_VERSION,
    A37DiversityGate, CALYX_ASSAY_PANEL_TOO_SMALL, DEFAULT_GATE_PANEL_LENSES,
    DEFAULT_LINEAR_CKA_SEED, DEFAULT_MAX_REDUNDANCY, DEFAULT_MIN_MARGINAL_BITS, DeficitProposal,
    ENSEMBLE_CARD_PID_METHOD, ENSEMBLE_CARD_SCHEMA_VERSION, EnsembleCard, EnsembleConfig,
    EnsembleDecision, EnsembleLensInput, EnsembleLensRole, EnsembleLensValue,
    EnsemblePairRedundancyEvidence, EnsemblePairValue, EnsembleRedundancyEvidence,
    EnsembleRedundancyMethod, EnsembleRedundancySketchInput, LINEAR_CKA_JACKKNIFE_BLOCKS,
    LINEAR_CKA_REDUNDANCY_METHOD, LINEAR_CKA_TUPLES_PER_ROW, LinearCkaEstimate, LinearCkaSketch,
    LinearCkaTuplePlan, MAX_LINEAR_CKA_TUPLES, MIN_ENSEMBLE_PANEL_LENSES, MIN_LINEAR_CKA_TUPLES,
    PidBits, a37_association_family, a37_diversity_gate, ensemble_card,
    ensemble_card_with_redundancy, ensemble_redundancy_from_lenses,
    ensemble_redundancy_from_lenses_cuda_strict, ensemble_redundancy_from_sketches,
    linear_cka_sketch_from_row_fn, linear_cka_sketch_from_rows, linear_cka_tuple_plan,
    validate_ensemble_card_redundancy, validate_redundancy_method_metadata,
};
pub use estimate::{
    EstimateBound, EstimateReliability, EstimatorKind, MiEstimate, TrustTag,
    require_grounded_anchor, trust_for_anchor,
};
pub use formula_catalog::{
    CALYX_FORMULA_COVERAGE_MISSING, FORMULA_COVERAGE_ARTIFACT_KIND,
    FORMULA_COVERAGE_SCHEMA_VERSION, FORMULA_COVERAGE_SOT_KEY, FORMULA_COVERAGE_SURFACE,
    FormulaCoverageArtifact, FormulaCoverageRow, FormulaCoverageStatus, FormulaCoverageSummary,
    FormulaRowSpec, formula_coverage_artifact, formula_coverage_json, prd22_formula_specs,
    validate_formula_coverage,
};
pub use formulas::{dpi_ceiling, lens_signal, marginal_value, pair_redundancy};
pub use gate::{AssayGate, LensSignal, PairGain};
pub use granger::{
    DEFAULT_GRANGER_LAG_SWEEP, DEFAULT_GRANGER_LAGS, GrangerReport, granger_causality,
    granger_causality_cuda_strict, granger_causality_lags, granger_causality_lags_cuda_strict,
    granger_causality_sweep, granger_causality_sweep_lags,
    granger_causality_sweep_lags_cuda_strict,
};
pub use group_split::{GroupSplit, group_holdout_split, row_groups};
pub use hawkes::{
    DEFAULT_HAWKES_DECAY, DEFAULT_HAWKES_ITERATIONS, DEFAULT_HAWKES_MIN_EDGE_BRANCHING_RATIO,
    HawkesBaseline, HawkesConfig, HawkesEdge, HawkesEventSeries, HawkesReport, HawkesStability,
    exponential_hawkes_em, exponential_hawkes_em_cuda_strict,
};
pub use hsic::{
    DEFAULT_HSIC_PERMUTATIONS, DEFAULT_HSIC_SEED, HsicConfig, HsicEstimators, HsicPermConfig,
    HsicReport, HsicTest, MIN_HSIC_GAMMA_SAMPLES, MIN_HSIC_SAMPLES, hsic, hsic_cuda_strict,
    hsic_estimators, hsic_estimators_cuda_strict, hsic_estimators_with_config,
    hsic_estimators_with_config_cuda_strict, hsic_permutation_test,
    hsic_permutation_test_cuda_strict, hsic_with_config, hsic_with_config_cuda_strict,
};
pub use ksg::{
    MIN_ASSAY_SAMPLES, ksg_mi_continuous, ksg_mi_continuous_cuda_strict,
    ksg_mi_continuous_discrete, ksg_mi_continuous_discrete_cuda_strict,
    ksg_mi_continuous_discrete_with_anchor, ksg_mi_continuous_discrete_with_anchor_cuda_strict,
    ksg_mi_continuous_with_anchor, ksg_mi_continuous_with_anchor_cuda_strict,
};
pub use logistic::{
    DEFAULT_ASSAY_SEEDS, DEFAULT_HOLDOUT_FRACTION, LOGISTIC_CONDITIONING_METHOD,
    LOGISTIC_CONDITIONING_SCHEMA_VERSION, LogisticConditioningBlock,
    LogisticConditioningProvenance, LogisticConditioningScale, LogisticFoldConditioning,
    LogisticProbeReport, logistic_probe_mi, logistic_probe_mi_calibrated,
    logistic_probe_mi_calibrated_cuda_strict, logistic_probe_mi_cuda_strict,
    logistic_probe_mi_multiseed, logistic_probe_mi_multiseed_calibrated,
    logistic_probe_mi_multiseed_calibrated_cuda_strict,
    logistic_probe_mi_multiseed_calibrated_with_anchor,
    logistic_probe_mi_multiseed_calibrated_with_anchor_cuda_strict,
    logistic_probe_mi_multiseed_cuda_strict, logistic_probe_mi_multiseed_with_anchor,
    logistic_probe_mi_multiseed_with_anchor_cuda_strict, logistic_probe_mi_with_anchor,
    logistic_probe_mi_with_anchor_cuda_strict,
};
pub use loom_adapter::AsterAssayMaterializationGate;
pub use mic::{DEFAULT_MIC_ALPHA, MIN_MIC_SAMPLES, MicReport, mic, mic_with_alpha};
pub use mmd::{
    ChangePointReport, DEFAULT_MMD_ALPHA, DEFAULT_MMD_PERMUTATIONS, DEFAULT_MMD_SEED, MmdConfig,
    MmdReport, gaussian_mmd, gaussian_mmd_cuda_strict, gaussian_mmd_with_config,
    gaussian_mmd_with_config_cuda_strict, mmd_change_point, mmd_change_point_cuda_strict,
};
pub use n_eff::{NeffReport, stable_rank};
pub use nmi::{NmiReport, partitioned_histogram_nmi};
pub use partial_correlation::{
    MIN_PEARSON_SAMPLES, PartialReport, PearsonReport, partial_correlation,
    partial_correlation_controlling, partial_correlation_controlling_cuda_strict,
    partial_correlation_cuda_strict, pearson, pearson_cuda_strict,
};
pub use partial_network::{
    DEFAULT_PARTIAL_NETWORK_ALPHA, DEFAULT_PARTIAL_NETWORK_MIN_ABS_R, PartialNetworkEdge,
    PartialNetworkPrunedEdge, PartialNetworkReport, PartialNetworkSeries,
    partial_correlation_network, partial_correlation_network_cuda_strict,
};
pub use periodicity::{
    AutocorrelationReport, DEFAULT_FAP_PERMUTATIONS, DEFAULT_MAX_PEAKS, DEFAULT_PERIODICITY_SEED,
    DEFAULT_PERIODOGRAM_OVERSAMPLE, MAX_ACF_SAMPLES, MAX_FREQUENCY_GRID, MIN_PERIODICITY_SAMPLES,
    PeriodicityReport, PeriodogramConfig, PeriodogramPeak, SIGNIFICANT_PEAK_FAP, autocorrelation,
    autocorrelation_cuda_strict, bin_event_counts, lomb_scargle, lomb_scargle_cuda_strict,
    lomb_scargle_with_anchor, lomb_scargle_with_config, lomb_scargle_with_config_cuda_strict,
};
pub use point_process::{
    CoIntensityVerdict, CrossKPoint, CrossKReport, DEFAULT_CLUSTER_RATIO, DEFAULT_INHIBIT_RATIO,
    MIN_POINT_EVENTS, temporal_cross_k,
};
pub use projection::{
    ProjectionReport, ProjectionTransferBytes, project_cpu, project_gpu, projection_transfer_bytes,
    target_projection_dim,
};
pub use rank_correlation::{
    KendallReport, MIN_RANK_CORR_SAMPLES, SpearmanReport, kendall_tau_b, spearman_rho,
};
pub use recurrence_anchor::{
    CALYX_ASSAY_MISSING_OUTCOME_SLOT, CONSISTENT_AGREEMENT_THRESHOLD, DEFAULT_OUTCOME_ANCHOR_LABEL,
    Domain, OutcomeAgreement, RecurrenceAnchor, default_outcome_anchor, frequency_anchor_for,
    measure_outcome_agreement, measure_outcome_agreement_for, oracle_self_consistency,
    oracle_self_consistency_from_agreements, outcome_agreement_from_observations,
    outcome_occurrence_context,
};
pub use recurrence_hazard::{
    CV_DETERMINISTIC, CusumChangePoint, CusumConfig, CusumReport, DEFAULT_CUSUM_SLACK_K,
    DEFAULT_CUSUM_THRESHOLD_H, DEFAULT_MIN_SIGMA_FRAC, DEFAULT_OVERDUE_ALPHA,
    InterEventHazardReport, MIN_CUSUM_GAPS, MIN_HAZARD_GAPS, RateShift, inter_event_hazard,
    inter_event_hazard_from_series, inter_event_hazard_with_alpha, recurrence_rate_cusum,
    recurrence_rate_cusum_from_series, recurrence_rate_cusum_with_config,
};
pub use resource_contract::{
    CALYX_ASSAY_INVALID_RESOURCE, CALYX_ASSAY_RESOURCE_BUDGET_EXCEEDED, PanelAdmissionCandidate,
    PanelLensDecision, PanelPackingReport, PanelResourceBudget, ResourceAwareAdmissionDecision,
    ResourceDensity, ResourceUsage, admit_lens_with_resources, admit_lens_with_usage,
    pack_panel_by_density,
};
pub use store::{AssayCacheKey, AssayRow, AssayStore, AssaySubject};
pub use stratified::{StratifiedBits, StratumBits, stratified_bits};
pub use sufficiency::{
    CALYX_ASSAY_INVALID_SCOPE, DeficitRoutingContext, DeficitSuggestedAction, InMemoryDeficitSink,
    ObservationScope, PanelJointBasis, PanelSufficiency, ScopedSufficiencyReport,
    SufficiencyDeficit, SufficiencyDeficitSink, SufficiencyScopeInput, entropy_bits,
    panel_joint_with_union_floor, panel_sufficiency, panel_sufficiency_by_scope,
    panel_sufficiency_from_estimate, panel_sufficiency_from_estimate_with_context,
    panel_sufficiency_with_anchor, panel_sufficiency_with_anchor_and_context,
    panel_sufficiency_with_context,
};
pub use synergy::{
    CALYX_ASSAY_INVALID_SYNERGY, MIN_SYNERGY_GAIN_BITS, SynergyPair, SynergyReport, synergy_gain,
    synergy_pair, synergy_report, unmeasured_synergy_pair,
};
pub use total_correlation::{
    CALYX_TC_INSUFFICIENT_SAMPLES, DEFAULT_TC_BOOTSTRAP_RESAMPLES, DEFAULT_TC_K, IIResult, IISign,
    MIN_QUORUM_TC_PER_SLOT, SlotVectors, TCResult, TotalCorrelationConfig, interaction_information,
    interaction_information_with_config, interaction_information_with_config_cuda_strict,
    min_quorum_tc, n_eff_from_tc, total_correlation, total_correlation_with_config,
    total_correlation_with_config_cuda_strict,
};
pub use transfer_entropy::{
    CALYX_TE_INSUFFICIENT_SAMPLES, DEFAULT_TE_BOOTSTRAP_RESAMPLES, DEFAULT_TE_BOOTSTRAP_SEED,
    DEFAULT_TE_K, DEFAULT_TE_LAGS, DEFAULT_TE_WINDOW, Direction, MIN_TE_QUORUM, RecurrenceStream,
    TEResult, Timestamp, TransferEntropyConfig, max_transfer_entropy_lag, transfer_entropy,
    transfer_entropy_sweep, transfer_entropy_sweep_with_config, transfer_entropy_with_config,
    transfer_entropy_with_config_cuda_strict,
};
