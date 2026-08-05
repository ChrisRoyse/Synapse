//! Anneal self-optimization contracts for reversible tuning loops.

mod budget;
mod heal;
mod j;
mod janitor;
mod learn;
mod ledger_anneal;
mod propose;
mod recurrence_schedule;
mod rollback;
mod rollback_codec;
mod shadow;
mod substrate;
mod tripwire;
mod tune;

pub use budget::{
    BACKGROUND_NICE, BudgetConfig, BudgetConfigReadback, BudgetEnforcer, BudgetHandle, BudgetProbe,
    BudgetProbeSample, BudgetStatus, CALYX_ANNEAL_BUDGET_CPU_UNAVAILABLE,
    CALYX_ANNEAL_BUDGET_EXHAUSTED, CALYX_ANNEAL_BUDGET_INVALID_CONFIG,
    CALYX_ANNEAL_BUDGET_NVML_UNAVAILABLE, ProcStatBudgetProbe, budget_config_path,
    read_budget_config_from_vault,
};
pub use heal::degrade::{
    ANNEAL_HEALTH_TAG, AsterHealthStore, CALYX_ANNEAL_HEAL_CONFIRMATION_REQUIRED,
    CALYX_ANNEAL_HEALTH_INVALID_ROW, ComponentHealth, ComponentKind, DegradeRegistry,
    HealthRowReadback, HealthStorage, LensRoute, ScopeId, decode_health_value,
};
pub use heal::rebuild::{
    AnnIndexRebuilder, AsterRebuildSource, CALYX_ANNEAL_REBUILD_INVALID_TARGET,
    CALYX_ANNEAL_REBUILD_IO, CALYX_ANNEAL_REBUILD_SOURCE_VIOLATION,
    CALYX_ANNEAL_REBUILD_TRIPWIRE_FAILED, CALYX_ASTER_SNAPSHOT_UNAVAILABLE, GuardProfileRebuilder,
    KernelIndexRebuilder, MvccSnapshot, RebuildOutcome, RebuildPriority, RebuildScheduler,
    RebuildTarget, Rebuilder,
};
pub use heal::recalibrate::{
    CALYX_ANNEAL_PARK_THRESHOLD_NOT_MET, CALYX_ANNEAL_TAU_INVALID,
    CALYX_ANNEAL_UNPARK_THRESHOLD_NOT_MET, CALYX_WARD_RECALIBRATE_FAILED, FileWardTauStore,
    LensParkOutcome, NewTau, RecalibrationOutcome, SIGNAL_DECAY_FLOOR_BITS, TauDriftEvent,
    WARD_TAU_TAG, WardRecalibrate, WardTauReadback, WardTauStore, park_decayed_lens,
    trigger_tau_recalibration, unpark_lens, ward_tau_path,
};
pub use heal::restore::{
    BASE_SHARD_CHECKSUM_TAG, BaseFaultEvent, BaseShard, CALYX_ANNEAL_ALERT_WRITE_FAILED,
    CALYX_ANNEAL_CHECKSUM_INVALID_ROW, CALYX_ANNEAL_RESTORE_FAILED, RestoreCommand, RestoreConfig,
    RestoreOutcome, ShardId, alert_operator, attempt_restore, base_shard_checksum,
    clear_reads_on_range, fail_reads_on_range, install_recorded_read_barriers, load_base_shards,
    record_base_shard_checksum, verify_base_shards, write_base_restored_event,
};
pub use heal::triggers::{
    AssayMetrics, CALYX_ANNEAL_FAULT_INVALID_EVENT, ChecksumDetector, ChecksumEntry, EndpointUrl,
    FaultDetector, FaultEvent, FaultKind, FaultMonitor, HttpProbe, LensProbeDetector, ProbeStatus,
    SignalDecayDetector, SignalSample, StaleDetector, StaleEntry, TauDriftDetector, TauDriftSample,
    WardMetrics,
};
pub use j::{
    ANNEAL_GROWTH_TAG, ANNEAL_REPORT_TAG, AsterGrowthCf, CALYX_ANNEAL_GOODHART_INVALID_CONFIG,
    CALYX_ANNEAL_GOODHART_INVALID_METRIC, CALYX_ANNEAL_GRADIENT_INVALID_CONFIG,
    CALYX_ANNEAL_GRADIENT_INVALID_METRIC, CALYX_ANNEAL_GROWTH_INVALID_CONFIG,
    CALYX_ANNEAL_GROWTH_INVALID_ROW, CALYX_ANNEAL_GROWTH_INVALID_SAMPLE,
    CALYX_ANNEAL_J_INVALID_CONFIG, CALYX_ANNEAL_J_INVALID_METRIC,
    CALYX_ANNEAL_J_SYNTHETIC_RECURSION, CALYX_ANNEAL_REPORT_INVALID_ROW, CandidateAction,
    DEFAULT_CROSS_LENS_DOMINANCE_THRESHOLD, DEFAULT_GOODHART_VIOLATION_PENALTY_WEIGHT,
    DEFAULT_GROWTH_MAX_SAMPLES, DEFAULT_GROWTH_WINDOW, DEFAULT_GTAU_THRESHOLD,
    DEFAULT_HELD_OUT_MIN_GAIN_FRACTION, DEFAULT_J_DOMAIN, GoodhartChecker, GoodhartLedgerContext,
    GoodhartReport, GoodhartState, GoodhartViolation, GradientCandidate, GradientEntry,
    GradientEntryReadback, GradientRefreshReport, GradientSnapshot, GradientWarning, GrowthCf,
    GrowthCurve, GrowthSample, GrowthSummary, HeldOutSet, IntelligenceGradient, IntelligenceReport,
    JGeneratedPositiveCredit, JMetricSources, JObjectiveContext, JTermDeltas, JTerms, JValue,
    JWeights, LensContributionDelta, PriorityReadback, REDUNDANCY_PENALTY, ReportAvailability,
    ReportDiff, TuneScopeKind, UNIT_PENALTY, WardGtau, add_goodhart_penalty_to_vault,
    anneal_growth_key, anneal_report_key, compute_j, decode_growth_row,
    decode_intelligence_report_row, encode_growth_row, estimate_dj, format_report,
    goodhart_state_path, gradient_state_path, intelligence_report, j_weights_path,
    latest_intelligence_report_snapshot, read_goodhart_state_from_vault,
    read_gradient_snapshot_from_vault, read_intelligence_report_snapshot,
    read_objective_weights_from_vault, record_goodhart_report, report_diff, set_objective_weights,
    to_json, write_goodhart_state, write_gradient_snapshot, write_intelligence_report_snapshot,
};
pub use janitor::{
    CALYX_IO_ERROR as CALYX_JANITOR_IO_ERROR, DatasetManifest, GcResult as JanitorGcResult,
    Janitor, JanitorConfig, JanitorErrorReadback, JanitorMetrics, JanitorReadback,
    MAX_JANITOR_BYTES_PER_TICK,
};
pub use learn::{
    AsterHeadStorage, AsterMistakeStorage, AsterOutcomeStorage, AsterReplayStorage,
    CALYX_ANNEAL_HEAD_FEATURE_SOURCE_UNAVAILABLE, CALYX_ANNEAL_HEAD_INVALID_ROW,
    CALYX_ANNEAL_HEAD_TOO_LARGE, CALYX_ANNEAL_HEAD_UPDATE_REVERTED, CALYX_ANNEAL_INVALID_CAPACITY,
    CALYX_ANNEAL_INVALID_WINDOW, CALYX_ANNEAL_MISTAKE_APPEND_ONLY,
    CALYX_ANNEAL_MISTAKE_INVALID_ROW, CALYX_ANNEAL_OUTCOME_APPEND_ONLY,
    CALYX_ANNEAL_OUTCOME_INVALID_ANCHOR, CALYX_ANNEAL_OUTCOME_INVALID_CONFIG,
    CALYX_ANNEAL_OUTCOME_INVALID_ROW, CALYX_ANNEAL_REGRESSION_INVALID_CONFIG,
    CALYX_ANNEAL_REGRESSION_NAN_PREDICTION, CALYX_ANNEAL_REGRESSION_RECURRED,
    CALYX_ANNEAL_REGRESSION_SOURCE_UNAVAILABLE, CALYX_ANNEAL_REPLAY_INVALID_ROW,
    CALYX_ANNEAL_SLEEP_PASS_INVALID_CONFIG, CALYX_REGISTRY_UNAVAILABLE,
    DEFAULT_MAX_REGRESSION_RATE, DEFAULT_MISTAKE_SURPRISE_THRESHOLD, DEFAULT_OUTCOME_ACTION_COST,
    DEFAULT_OUTCOME_FISHER_WEIGHT, DEFAULT_OUTCOME_LR, DEFAULT_REPLAY_CAPACITY,
    DEFAULT_REPLAY_CHECKPOINT_INTERVAL, DEFAULT_SLEEP_PASS_BATCH_SIZE,
    DEFAULT_SLEEP_PASS_MIN_SURPRISE, FrozenCheckReport, FrozenLensCheck, FrozenLensGuard,
    FrozenLensReportRow, FrozenLensSource, FrozenLensStatus, HeadKind, HeadPromotionGate,
    HeadReadback, HeadRegressionRollback, HeadStorage, HeadUpdateOutcome, HeadUpdateSummary,
    MAX_ONLINE_HEAD_PARAMS, MistakeEntry, MistakeLog, MistakeReadback, MistakeRef, MistakeStorage,
    NoFrozenLensGuard, OnlineHead, OnlineHeadState, OutcomePrediction, OutcomeQueue,
    OutcomeQueueEntry, OutcomeQueueReadback, OutcomeStorage, RecordOutcomeConfig,
    RecordOutcomeContext, RecordOutcomeContradiction, RecordOutcomeResult, RecordOutcomeReward,
    RegressionConfig, RegressionContextSource, RegressionPredictor, RegressionReport,
    RegressionResult, RegressionUpdateOutcome, ReplayBuffer, ReplayEntry, ReplaySnapshot,
    ReplayStorage, ReplayWrite, SleepPassConfig, SleepPassOutcome, SleepPassReplayRecord,
    assert_no_regression, decode_head_rows, decode_mistake_entry, decode_online_head,
    decode_outcome_queue_entry, decode_replay_rows, decode_replay_snapshot, encode_mistake_entry,
    encode_online_head, encode_outcome_queue_entry, encode_replay_snapshot, head_key,
    head_state_artifact_key, mistake_key, mistake_seq_from_key, outcome_queue_key,
    outcome_queue_seq_from_key, record_mistake_for_replay, record_outcome, record_regression,
    regression_rate, regression_recurred, replay_snapshot_key, run_sleep_pass,
};
pub use ledger_anneal::{
    ANNEAL_LEDGER_PAYLOAD_TAG, AnnealFaultLedgerDetails, AnnealLedger, AnnealLedgerAction,
    AnnealLedgerEntry, AnnealLedgerReadback, AsterAnnealLedgerStore,
    CALYX_ANNEAL_LEDGER_INVALID_ENTRY, CALYX_ASTER_CF_UNAVAILABLE, CALYX_LEDGER_ENTRY_TOO_LARGE,
    MAX_ANNEAL_LEDGER_PAYLOAD_BYTES, decode_anneal_ledger_payload,
};
pub use propose::{
    ANNEAL_OPERATOR_PROPOSAL_TAG, AdmissionRecord, AlgParams, AlgorithmicKind, AnchorGap, AnchorId,
    AssayAttribution, AsterOperatorProposalStorage, CALYX_ANNEAL_CANDIDATE_INVALID_DEFICIT,
    CALYX_ANNEAL_DEFICIT_INVALID_CONFIG, CALYX_ANNEAL_OPERATOR_INVALID_RECORD,
    CALYX_ANNEAL_OPERATOR_NO_GAIN, CALYX_ASSAY_INVALID_METRIC, CALYX_ASSAY_UNAVAILABLE,
    CALYX_REGISTRY_HOT_ADD_FAIL, CALYX_REGISTRY_PROFILE_TIMEOUT, CandidateLens, CommissionSpec,
    ConversionTarget, CorpusSampleSource, DEFAULT_DEFICIT_THRESHOLD_BITS, DIFFERENTIATION_MAX_CORR,
    DIFFERENTIATION_MIN_BITS, DeficitLocalizer, DeficitLocalizerConfig, DeficitMap,
    DifferentiationGate, ExpectedTargetCost, GateOutcome, HotAddPlan, HotAddReceipt,
    LensAdmittedEntry, LensHotAdder, LensProfiler, LensRejectedEntry, MAX_SYNTHESIS_CORPUS_SAMPLE,
    MODALITY_COVERAGE_THRESHOLD_BITS, ModalityId, OperatorPromotionGate, OperatorProposalConfig,
    OperatorProposalOutcome, OperatorProposalReadback, OperatorProposalRecord,
    OperatorProposalStorage, OperatorTerminalState, PROFILE_TIMEOUT_MS, PairNMI,
    ProposalHistoryReadback, ProposalOutcome, ProposalSubstrate, ProposalTerminalState,
    ProposeLens, ProposeLensRequest, ProposeOperator, ProposeOperatorRequest, ProposedOperator,
    RegistryHotAdder, RejectReason, build_commission_spec, decode_operator_proposal,
    decode_operator_proposal_rows, describe, describe_gate_outcome, encode_operator_proposal, gate,
    has_deficit, operator_proposal_key, proposal_history, proposal_history_with_refs, propose_lens,
    propose_operator, ranked_conversion_targets, record_admitted, record_from_entry,
    record_outcome as record_proposal_outcome, record_rejected, synthesize, synthesize_algorithmic,
    synthesize_from_source, top_gap_description,
};
pub use recurrence_schedule::{
    CALYX_ANNEAL_INVALID_CADENCE, FREQ_BONUS_MAX, RecurrenceSchedule, RefreshPriority,
    RetentionTier, anneal_retention_tier, frequency_kernel_bonus, recurrence_schedule_for,
};
pub use rollback::{
    ArtifactKey, ArtifactPtr, ArtifactSnapshot, AsterRollbackStorage,
    CALYX_ANNEAL_CHANGE_COMMITTED, CALYX_ANNEAL_INVALID_ROLLBACK_STATE,
    CALYX_ANNEAL_UNKNOWN_CHANGE_ID, ChangeId, LogicalTime, RollbackReadback, RollbackStorage,
    RollbackStore, rollback_live_key, rollback_snapshot_key,
};
pub use shadow::{
    ALL_SHADOW_METRICS, ActionMetricSnapshot, AnnealAction, ArtifactReplayMeasurer,
    CALYX_ANNEAL_SHADOW_MEASUREMENT_MISSING, HeldOutReplay, MetricComparison, MetricSide,
    MetricSnapshot, ReplayAnchor, ReplayQuery, ReplaySource, ShadowExecutor, ShadowRevertReason,
    ShadowVerdict, build_replay,
};
pub use substrate::{
    AnnealLedgerActionPair, AnnealProposalLedgerOptions, AnnealStatus, AnnealSubstrate,
    CALYX_LEDGER_WRITE_FAIL, ChangeOutcome,
};
pub use tripwire::{
    CALYX_TRIPWIRE_INVALID_CONFIG, CALYX_TRIPWIRE_INVALID_METRIC, ThresholdDir, ThresholdState,
    TripwireConfigReadback, TripwireMetric, TripwireRegistry, TripwireResult, TripwireStatus,
    TripwireThreshold, TripwireThresholdEntry, read_tripwire_config_from_vault,
    tripwire_config_path,
};
pub use tune::{
    ABLedgerEvent, ABLedgerWriter, ABPromotionConfig, ABResult, ABRunner, ABSummary, ABTrial,
    ABTrialBudget, ABVerdict, ABVerdictRecord, Arm, ArmStatus, AsterBanditStorage,
    AsterSoakStorage, BanditPolicy, BanditReadback, BanditStatus, BanditStorage,
    CALYX_ANNEAL_AB_CACHE_WRITE_FAIL, CALYX_ANNEAL_BANDIT_EMPTY,
    CALYX_ANNEAL_BANDIT_INVALID_CONFIG, CALYX_ANNEAL_BANDIT_INVALID_ROW,
    CALYX_ANNEAL_SOAK_INVALID_CONFIG, CALYX_ANNEAL_SOAK_INVALID_ROW,
    CALYX_ANNEAL_SOAK_LIVE_TRAFFIC_UNAVAILABLE, CALYX_ANNEAL_SOAK_TIME_BUDGET_EXHAUSTED,
    CALYX_ANNEAL_TRIAL_ALREADY_ACTIVE, CALYX_ANNEAL_TRIAL_INVALID_RESULT,
    CALYX_ANNEAL_TRIAL_NOT_ACTIVE, CALYX_FORGE_CACHE_WRITE_FAIL, CALYX_FORGE_SCOPE_INVALID_CONFIG,
    CALYX_INDEX_CACHE_WRITE_FAIL, CALYX_INDEX_SCOPE_INVALID_CONFIG, CALYX_LOOM_PLAN_WRITE_FAIL,
    CALYX_LOOM_SCOPE_INVALID_CONFIG, CALYX_STORAGE_CACHE_WRITE_FAIL,
    CALYX_STORAGE_SCOPE_INVALID_CONFIG, ConcatKey, ConfigBandit, ConfigBanditStore, ConfigVariant,
    DEFAULT_AB_MIN_SAMPLES, DEFAULT_FORGE_RECALL_TARGET, DEFAULT_HYSTERESIS_WINS,
    DEFAULT_INDEX_RECALL_TARGET, DEFAULT_INDEX_VRAM_BUDGET_BYTES, DEFAULT_LOOM_RECALL_TARGET,
    DEFAULT_SOAK_OSCILLATION_WINDOW, DEFAULT_SOAK_P99_TARGET_REDUCTION, DEFAULT_SOAK_QUERIES,
    DEFAULT_SOAK_SAMPLE_INTERVAL, DEFAULT_SOAK_SEED, DEFAULT_STORAGE_RECALL_TARGET, DType,
    ForgeBanditPersistence, ForgeConfig, ForgePromotionRecord, ForgePromotionWriter,
    ForgeScopeTuner, ForgeTuneDecision, IndexBanditPersistence, IndexConfig, IndexPromotionRecord,
    IndexPromotionWriter, IndexScopeTuner, IndexSlotHealth, IndexTuneDecision, IndexTuneSkip,
    LoomBanditPersistence, LoomMaterializer, LoomPromotionRecord, LoomPromotionWriter,
    LoomScopeTuner, LoomTuneDecision, MAX_BUCKETED_DIM, MAX_FORGE_CANDIDATES, MAX_INDEX_CANDIDATES,
    MAX_LOOM_CANDIDATES, MAX_LOOM_EAGER_PAIRS, MAX_STORAGE_CANDIDATES, MIN_BITS_PER_ANCHOR,
    MIN_LOOM_PAIR_BITS, MatPlanConfig, MetricSample, NoopABBudget, NoopABLedgerWriter,
    NoopForgeBanditStore, NoopForgePromotionWriter, NoopIndexAssayMetrics, NoopIndexBanditStore,
    NoopIndexPromotionWriter, NoopIndexSlotHealth, NoopLoomBanditStore, NoopLoomMaterializer,
    NoopLoomPromotionWriter, NoopSoakStorage, NoopStorageBanditStore, NoopStoragePromotionWriter,
    PlanScore, QuantPromotionEvidence, QueryLog, QueryObservation, SeededSoakProfile, ShapeKey,
    SoakConfig, SoakHarness, SoakMetrics, SoakMode, SoakReport, SoakRowKind, SoakStorage,
    SoakStoredRow, StorageBanditPersistence, StorageConfig, StorageMetrics, StoragePromotionRecord,
    StoragePromotionWriter, StorageScopeTuner, StorageShapeKey, StorageTuneDecision, bandit_key,
    bucket_dim, bucket_shape, candidate_configs, candidate_storage_configs, check_oscillation,
    decode_config_bandit, decode_forge_config, decode_index_config, decode_mat_plan_config,
    decode_soak_reports, decode_soak_row, decode_storage_config, encode_config_bandit,
    encode_forge_config, encode_index_config, encode_mat_plan_config, encode_soak_row,
    encode_storage_config, evaluate_plan, generate_candidate_plan, index_candidate_configs,
    index_slot_label, loom_plan_label, loom_plan_shape_key, loom_plan_tune_key, quant_win_check,
    shape_key_hash, slot_autotune_key, soak_report_key, soak_sample_key, storage_autotune_key,
    storage_shape_label, storage_win_check, validate_index_config, validate_mat_plan_config,
    validate_quant_promotion_evidence, validate_storage_config, validate_storage_metrics,
};
