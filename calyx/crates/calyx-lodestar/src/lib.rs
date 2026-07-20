#![deny(warnings)]

//! Lodestar grounding-kernel discovery and maintenance.

pub mod aster_bridge;
pub mod blind_spot_sweep;
pub mod chain_walks;
pub mod corpus_weave_report;
pub mod cross_vault_chain;
pub mod dfvs;
pub mod discovery_chain;
pub mod discovery_run_manifest;
pub mod domain_bridges;
mod error;
pub mod grounding_gaps;
pub mod hierarchical;
pub mod hypothesis_evaluation;
pub mod hypothesis_evidence;
pub mod incremental;
pub mod kernel;
pub mod kernel_answer;
pub mod kernel_graph;
pub mod kernel_health;
pub mod kernel_index;
pub mod label_propagation;
pub mod loom_assoc;
pub mod loom_weave_report;
pub mod molecular_bridges;
pub mod multi_scope;
pub mod probe_matrix;
pub mod provenance;
pub mod ranked_hypotheses;
pub mod recall_eval;
pub mod refusal_expansion;
pub mod scope;
pub mod scope_cache;
pub mod scope_report;
pub mod spectral_communities;
pub mod summarize;
pub mod temporal_kernel;
pub mod vault_kernel;

pub use aster_bridge::{
    ASTER_ASSOC_METADATA_KEY, AsterAssocMetadata, AsterAssocNodeProps, AsterAssocSnapshot,
    AsterSummarizeRequest, DEFAULT_ASTER_ASSOC_COLLECTION, PhysicalAsterAssocSnapshot,
    encode_assoc_node_props, summarize_vault_as_of, summarize_vault_latest, write_assoc_metadata,
};
pub use blind_spot_sweep::{
    BLIND_SPOT_SWEEP_SCHEMA_VERSION, BlindSpotCandidate, BlindSpotGateVerdict, BlindSpotNeighbor,
    BlindSpotObservation, BlindSpotSweepLog, BlindSpotSweepParams, sweep_blind_spots,
};
pub use chain_walks::{
    AbcHypothesis, CHAIN_WALK_SCHEMA_VERSION, ChainWalkParams, ChainWalkReport, ChainWalkResult,
    ChainWalkSeed, ChainWalkSeedKind, run_chain_walks_with_gate, run_grounded_chain_walks,
};
pub use corpus_weave_report::{
    CORPUS_WEAVE_REPORT_SCHEMA_VERSION, CorpusWeaveReport, CorpusWeaveReportParams,
    corpus_weave_report,
};
pub use cross_vault_chain::{
    CROSS_VAULT_CHAIN_SCHEMA_VERSION, ClinicalFrontier, CrossVaultChainCandidate,
    CrossVaultChainParams, CrossVaultChainReport, CrossVaultDeficit, CrossVaultMolecularCandidate,
    CrossVaultMolecularGateVerdict, MolecularEndpoint, MolecularKernelState,
    run_cross_vault_grounded_chain,
};
pub use dfvs::{
    DfvsMethod, DfvsResult, bounded_genus_approx, dfvs_approx, genus_estimate, is_tournament,
    tournament_2approx,
};
pub use discovery_chain::{
    DISCOVERY_CHAIN_SCHEMA_VERSION, DiscoveryAcceptedHop, DiscoveryCandidate,
    DiscoveryCandidateLog, DiscoveryChainLog, DiscoveryChainParams, DiscoveryGateVerdict,
    DiscoveryTermination, reachability_prior_gate, run_discovery_chain_with_gate,
    run_grounded_discovery_chain,
};
pub use discovery_run_manifest::{
    DISCOVERY_RUN_MANIFEST_SCHEMA_VERSION, DiscoveryRunManifest, DiscoveryRunReproductionReport,
    DiscoveryRunReproductionStatus, DiscoveryRunSeal, DiscoveryRunStage, ObservedStageOutput,
    build_discovery_run_manifest, manifest_sha256, reproduce_discovery_run_manifest,
    seal_discovery_run_manifest, validate_discovery_run_manifest,
};
pub use domain_bridges::{
    DOMAIN_BRIDGE_SCHEMA_VERSION, DomainBridgeCandidate, DomainBridgeGateVerdict,
    DomainBridgeInput, DomainBridgeMiningParams, DomainBridgePairReport, DomainBridgeParams,
    DomainBridgeReport, DomainBridgeScopePair, DomainPair, mine_domain_bridges,
    rank_domain_bridges,
};
pub use error::{LodestarError, Result};
pub use grounding_gaps::{
    CALYX_KERNEL_EMPTY, CALYX_KERNEL_UNGROUNDED, GroundingGapReport, grounding_gaps,
};
pub use hierarchical::{
    HierarchicalKernel, HierarchicalKernelParams, RegionDescriptor, RegionId, RegionStore,
    build_hierarchical_kernel,
};
pub use hypothesis_evaluation::{
    EvaluatorRun, HYPOTHESIS_EVALUATION_SCHEMA_VERSION, HypothesisEvaluation,
    HypothesisEvaluationInput, HypothesisEvaluationParams, HypothesisEvaluationReport,
    HypothesisEvaluationVerdict, RetrievedEvidence, aggregate_hypothesis_evaluations,
};
pub use hypothesis_evidence::{
    EvidenceSource, HYPOTHESIS_EVIDENCE_ASSEMBLER_VERSION, assemble_hypothesis_evaluation_inputs,
    chain_report_evidence_cx_ids, hypothesis_evidence_cx_ids,
};
pub use incremental::{IncrementalKernelEval, IncrementalResult, NodeAddEdge};
pub use kernel::{
    GroundednessReport, Kernel, KernelParams, RecallReport, build_kernel_pipeline,
    build_kernel_pipeline_with_frequency, refine_kernel_with_recall_support,
    seal_completed_kernel_identity,
};
pub use kernel_answer::{
    AnswerDerivation, AnswerDerivationHop, AnswerHop, AnswerPath, AsterKernelAnswerRequest,
    derive_kernel_answer, kernel_answer, kernel_answer_derivation_hash,
    kernel_answer_with_aster_ledger, kernel_answer_with_ledger,
};
pub use kernel_graph::{
    KernelGraph, KernelGraphParams, KernelNodeScore, LpRoundParams, NodeScore,
    groundedness_distance, lp_round_kernel_graph, lp_round_kernel_graph_from_solution,
    select_kernel_graph,
};
pub use kernel_health::{
    KERNEL_ARTIFACT_FORMAT_VERSION, KernelArtifactStore, KernelHealth, KernelRecallHealth,
    KernelTrust, RecallPassMode, kernel_health, kernel_health_from_kernel, read_kernel_artifact,
    write_kernel_artifact,
};
pub use kernel_index::{
    EmbeddingStore, FsKernelStore, KernelIndex, KernelStore, KernelVectorRow, build_kernel_index,
    kernel_search, load_kernel_index, write_kernel_index,
};
pub use label_propagation::{
    CALYX_PROP_GRAPH_EMPTY, CALYX_PROP_INVALID_INPUT, CALYX_PROP_NO_KERNEL_NODES,
    CALYX_PROP_NOT_CONVERGED, DEFAULT_PROPAGATION_DECAY_LAMBDA, LabelPropagationReceipt, NodeId,
    PropagatedLabel, PropagationError, SparseGraph, append_label_propagation_entry,
    kernel_labels_hash, propagate_labels, propagate_labels_with_decay,
    propagate_labels_with_ledger,
};
pub use loom_assoc::{
    LoomAssocEdgeProvenance, LoomAssocGraphInput, LoomDirectionalConfidence, LoomSlotNode,
    build_assoc_graph_from_loom, loom_assoc_graph_input,
};
pub use loom_weave_report::{
    LOOM_WEAVE_REPORT_SCHEMA_VERSION, LoomWeaveEdgeReadback, LoomWeaveReport,
    LoomWeaveReportParams, loom_weave_report,
};
pub use molecular_bridges::{
    ClinicalMolecularSeed, MOLECULAR_BRIDGE_SCHEMA_VERSION, MolecularBridgeCandidate,
    MolecularBridgeParams, MolecularBridgeReport, MolecularEvidenceRow, rank_molecular_bridges,
};
pub use multi_scope::{
    anchors_for_scope, bridges, build_kernel, kernel_answer_scoped,
    kernel_answer_scoped_with_ledger,
};
pub use probe_matrix::{
    PROBE_MATRIX_SCHEMA_VERSION, ProbeFusionMode, ProbeHit, ProbeLength, ProbeLensEmphasis,
    ProbeMatrixLog, ProbeMatrixSpec, ProbePhrasing, ProbeProductivity, ProbeRecord, ProbeRefusal,
    ProbeResponse, ProbeVariant, build_probe_matrix, run_probe_matrix,
};
pub use provenance::{
    AnswerHopEvidence, KernelAnswerRecordContext, KernelBuildReceipt, append_answer_hop_entry,
    append_kernel_build_entry, build_kernel_pipeline_with_ledger, kernel_members_hash,
};
pub use ranked_hypotheses::{
    RANKED_HYPOTHESIS_SCHEMA_VERSION, RankedHypothesis, RankedHypothesisParams,
    RankedHypothesisReport, TraceableHypothesisInput, rank_traceable_hypotheses,
};
pub use recall_eval::{
    AnnIndex, CALYX_KERNEL_RECALL_BELOW_GATE, CorpusReader, InMemoryAnnIndex, InMemoryCorpus,
    RecallEvalParams, RecallEvaluationReport, RecallQuery, RecallSupportReport,
    enforce_recall_gate, full_topk_support_set, kernel_recall_gate, kernel_recall_gate_with_clock,
    measure_kernel_recall, measure_kernel_recall_with_clock,
};
pub use refusal_expansion::{
    REFUSAL_EXPANSION_SCHEMA_VERSION, RefusalExpansionAction, RefusalExpansionActionKind,
    RefusalExpansionParams, RefusalExpansionPlan, RefusalExpansionVerification,
    plan_refusal_expansion, verify_refusal_expansion,
};
pub use scope::{
    AssocStore, CollectionId, FilterExpr, Scope, TenantId, materialize_scope, root_nodes_for_scope,
    scope_hash,
};
pub use scope_cache::{CacheStats, ScopeCache, ScopeCacheKey, scope_cache_anchor_identity};
pub use scope_report::{ScopeKernelReport, report_all_scopes};
pub use spectral_communities::{
    InterCommunityBridgeCandidate, SPECTRAL_COMMUNITY_SCHEMA_VERSION, SpectralCentralityCandidate,
    SpectralCommunityMember, SpectralCommunityParams, SpectralCommunityReport,
    SpectralCommunitySummary, spectral_community_report,
};
pub use summarize::{
    CALYX_SCOPE_INVALID_TIME_WINDOW, CALYX_SUMMARIZE_EMPTY_SCOPE,
    CALYX_SUMMARIZE_INSUFFICIENT_GROUNDING, CALYX_TIMETRAVEL_BEFORE_HORIZON,
    SUMMARIZE_INVOKED_MARKER, SummarizeCtx, SummarizeParams, SummarizeRecall, SummarizeResult,
    summarize, summarize_as_of, summarize_with_ledger, summarize_with_recall,
};
pub use temporal_kernel::{
    CALYX_LODESTAR_INVALID_FREQUENCY, CALYX_LODESTAR_INVALID_WINDOW,
    CALYX_LODESTAR_MISSING_FREQUENCY, FREQ_BONUS_MAX, FREQ_WEIGHT, FrequencyRead, KernelResult,
    KernelScope, KernelWeight, TimeWindow, active_cxids_in_window, apply_frequency_bonuses,
    frequency_kernel_bonus, kernel_for_window, kernel_for_window_from_graph, kernel_weight_rows,
};

pub use vault_kernel::{
    MeasuredVaultKernel, measured_kernel_from_vault, measured_kernel_with_contributions_from_vault,
};
