//! Registry runtimes for frozen Calyx lenses.

/// True when this build compiled the candle/fastembed CUDA embedder paths
/// (`candle-cuda` feature). Exported for build-info capability readback
/// (#1130): deploy gates assert this resolved value, not a feature spelling.
pub const CANDLE_CUDA_COMPILED: bool = cfg!(feature = "candle-cuda");

pub mod backfill;
pub mod commission;
pub mod compression;
pub mod drift;
pub mod explain;
pub mod frozen;
pub mod ingest_microbatch;
pub mod lens;
pub mod measure;
mod panel_generation;
pub mod panel_ops;
pub mod panels;
pub mod persistence;
mod persistence_contracts;
pub mod placement;
pub mod profile;
pub mod runtime;
mod runtime_limit;
pub mod spec;
pub mod structured_record;
pub mod swap;
pub mod temporal;
mod temporal_panel_catalog;

pub use backfill::{
    BackfillBatch, BackfillConfig, BackfillPriority, BackfillRequest, BackfillScheduler,
    BackfillWatermark,
};
pub use calyx_core::{Input, Lens};
pub use commission::{
    CommissionRequest, CommissionedLens, CommissionedLensArtifact, LensForgeBatchPolicy,
    LensForgeBatchProbeLevel, LensForgeFile, LensForgeManifest, LensForgeShape, commission_lens,
    lens_spec_from_manifest, lens_spec_from_manifest_path,
    lens_spec_from_manifest_with_license_override, lens_spec_metadata_from_manifest,
    lens_spec_metadata_from_manifest_path, register_commissioned,
};
pub use compression::{
    CALYX_VECTOR_COMPRESSION_EMPTY, CALYX_VECTOR_COMPRESSION_INVALID, COMPRESSED_SLOT_TAG,
    MxFp4AssayEvidence, SlotCompressionReport, SlotCompressionRow, StoredSlotCodec,
    StoredSlotEnvelope, compress_slot_batch, compress_slot_batch_with_assay_evidence,
    decode_stored_slot_envelope, matryoshka_truncate_renormalize, write_compressed_slot_batch,
    write_compressed_slot_batch_with_assay_evidence,
};
pub use drift::{
    CALYX_LENS_RUNTIME_DRIFT, DriftDecision, PROCESS_RUNTIME_GOLDEN_TOLERANCE, RuntimeGolden,
};
pub use explain::{LensExplanation, explain_lens, explain_lens_from_card};
pub use frozen::{FrozenLensContract, LensDType, NormPolicy};
pub use ingest_microbatch::{
    DEFAULT_INGEST_MICROBATCH_CAP_BYTES, INGEST_MICROBATCH_INPUT_OVERHEAD_BYTES, IngestLensOutcome,
    IngestLensOutcomeStatus, IngestMicrobatchConfig, IngestMicrobatchController,
    IngestMicrobatchPermit, IngestMicrobatchStats, IngestPanelReadout, estimate_microbatch_bytes,
};
pub use lens::{
    DeterminismProof, DualMeasurement, FrozenLensSnapshot, Registry, RegistryLensSnapshot,
    ensure_input_modality, ensure_vector_shape,
};
pub use panel_generation::{
    CALYX_PANEL_GENERATION_CONFLICT, CALYX_PANEL_GENERATION_EXHAUSTED,
    CALYX_PANEL_GENERATION_INVALID, PanelGenerationAllocation, PanelGenerationAllocatorReadback,
    allocate_vault_panel_generation, read_vault_panel_generation_allocator,
    reserve_vault_panel_generations,
};
pub use panel_ops::{
    AppliedPanelTemplate, CALYX_PANEL_LENS_MISSING, PanelCapabilityGateOutcome, PanelDiff,
    PanelSlotListing, ResolvedPanelLens, apply_capability_gate, apply_panel_template, list_panel,
    list_panel_with_assay, swap_panel, swap_panel_to_target,
};
pub use panels::{
    AlgorithmicPanelLens, InstantiatedPanel, MaterializedPanelTemplate, PanelLensRuntime,
    PanelSlotSpec, PanelTemplate, bio_default, civic_default, code_default, instantiate_panel,
    legal_default, materialize_panel_template, media_default, medical_default, text_default,
};
pub use persistence::{
    CALYX_NO_ACTIVE_PANEL, LoadedRegistrySnapshotLens, RegistryBatchLimitChange,
    RegistryBatchLimitUpdate, RegistrySnapshotMeasureStats, VaultPanelState, VaultPanelWrite,
    VaultRegistryBatchLimitWrite, VaultRegistrySnapshot, apply_registry_snapshot_batch_limits,
    load_vault_panel_state, measure_registry_snapshot_lens_batch,
    measure_registry_snapshot_lens_batch_with_stats, persist_vault_panel_state,
    set_vault_registry_batch_limits,
};
pub use persistence_contracts::{
    RegistryContractAudit, RegistryContractDiff, RegistryContractFieldDiff,
    RegistryContractRepairChange, VaultRegistryContractRepairAllWrite,
    VaultRegistryContractRepairWrite, algorithmic_encoder, audit_registry_snapshot_contracts,
    audit_vault_registry_contracts, derive_runtime_contract_from_spec,
    lens_spec_with_frozen_contract, repair_vault_registry_contracts_from_specs,
    repair_vault_registry_slot_from_spec, require_vault_registry_contracts,
};
pub use placement::{
    CALYX_BGE_M3_CPU_GRAPH_GPU_PLACEMENT, CALYX_RAM_BUDGET_EXCEEDED, CALYX_VRAM_BUDGET_EXCEEDED,
    CpuLensPool, CpuPoolAdmission, LENS_RAM_REMEDIATION, LENS_VRAM_REMEDIATION, PlacementBudget,
    PlacementPlan, choose_placement,
};
pub use profile::{
    CALYX_PROFILE_CUDA_MIN_ROWS_ENV, CALYX_PROFILE_REQUIRE_CUDA_ENV,
    CAPABILITY_MAX_PAIRWISE_CORR_ENV, CAPABILITY_MIN_SIGNAL_BITS_ENV, CapabilityCard,
    CapabilityGateDecision, CapabilityGateEvaluation, CapabilityGateThresholds,
    CapabilitySignalKind, CapabilitySignalReliability, CostMetrics, CoverageMetrics,
    DEFAULT_PROFILE_CUDA_MIN_ROWS, DenseProfileRequest, MetricSource, ProfileExecutionStats,
    ProfileMathBackend, ProfileOptions, ProfileProbe, Profiler, SeparationMetrics, SpreadMetrics,
    append_capability_gate_ledger, apply_assay_metrics, capability_gate_json,
    evaluate_capability_gate, max_panel_pairwise_correlation, profile_dense_vectors, profile_lens,
    profile_slot_with_assay, signal_kind_from_spec,
};
pub use runtime::adapters::{
    CALYX_ALLOW_NONCOMMERCIAL_LENSES_ENV, CALYX_LICENSE_DENIED, MultimodalAdapterLens,
    MultimodalAdapterProvider, MultimodalAdapterSpec, MultimodalAxis, MultimodalLensPackEntry,
    allow_noncommercial_from_env, default_multimodal_lens_specs, ensure_license_allowed,
    is_non_commercial_license, register_multimodal_lens_pack, shutdown_multimodal_gpu_workers,
};
pub use runtime::algorithmic::{
    AlgorithmicBatchProvider, AlgorithmicBatchStats, AlgorithmicEncoder, AlgorithmicLens,
    BYTE_FEATURES_CUDA_MIN_INPUT_BYTES, DenseCosineGrading, SPARSE_KEYWORDS_CUDA_MIN_TOKENS,
    TOKEN_HASH_CUDA_MIN_WORDS,
};
pub use runtime::candle::{
    CandleDevicePolicy, CandleFileSpec, CandleLens, CandleModelFiles, CandlePoolingPolicy,
    CandlePrecision, DEFAULT_CANDLE_MODEL,
};
pub use runtime::external_cmd::ExternalCmdLens;
pub use runtime::onnx::{
    DEFAULT_ANSWERAI_COLBERT_MODEL, FastembedBgem3Lens, FastembedRerankerLens, FastembedSparseLens,
    OnnxColbertFileSpec, OnnxColbertLens, OnnxFileSpec, OnnxLens, OnnxModelFiles,
    OnnxProviderPolicy, OnnxShapeBucketBudget, PoolingPolicy, onnx_shape_bucket_budget,
};
pub use runtime::qwen3::{
    DEFAULT_QWEN3_MAX_TOKENS, DEFAULT_QWEN3_MODEL, FastembedQwen3Lens, Qwen3FileSpec,
    Qwen3ModelFiles,
};
pub use runtime::static_lookup::{
    StaticLookupDType, StaticLookupFileSpec, StaticLookupFiles, StaticLookupLens,
};
pub use runtime::tei_http::{DEFAULT_TEI_ENDPOINT, TeiHttpLens};
pub use runtime_limit::{
    measure_registry_batch_with_runtime_limit, measure_registry_group_with_runtime_limit,
};
pub use spec::{
    Bgem3Engine, FastembedBgem3Output, LensHealth, LensRuntime, LensSpec, default_recall_delta,
};
pub use structured_record::{
    CALYX_STRUCTURED_FIELD_MISSING, CALYX_STRUCTURED_FIELD_TYPE_MISMATCH,
    CALYX_STRUCTURED_RECORD_INVALID, CALYX_STRUCTURED_SCHEMA_INVALID, StructuredBatchMeasurement,
    StructuredFieldSchema, StructuredFieldType, StructuredMetadataRoute,
    StructuredRecordMeasurement, StructuredRecordSchema, StructuredScalarRoute,
    StructuredSlotMeasure, canonical_json_bytes, measure_structured_record,
    measure_structured_record_batch,
};
pub use swap::{BackfillCandidate, BackfillQueue, DurableAddLensOptions, SlotSpec, SwapController};
pub use temporal::{
    DecayFunction, E2RecencyConfig, E2RecencyLens, E3PeriodicConfig, E3PeriodicLens,
    E4PositionalConfig, E4PositionalLens, MultiAnchorMode, PeriodicOptions, SequenceDirection,
    SequenceOptions, TEMPORAL_FLAGS, TemporalLensFlags,
};
pub use temporal_panel_catalog::{
    CALYX_TEMPORAL_PANEL_CONFLICT, CALYX_TEMPORAL_PANEL_INVALID, CALYX_TEMPORAL_PANEL_MISSING,
    TemporalPanelRegistrationDisposition, VaultTemporalPanelRegistration,
    VaultTemporalPanelRegistrationWrite, list_vault_temporal_panels, read_vault_temporal_panel,
    register_vault_temporal_panel,
};
