//! Synapse-owned lifecycle wrapper for the embedded Calyx Aster vault.

mod anneal;
pub use anneal::{
    SynapseCalyxAnnealChangeReport, SynapseCalyxAnnealRollbackReport,
    SynapseCalyxAnnealSearchReport, SynapseCalyxAnnealSearchSlotMetrics, SynapseCalyxAnnealStatus,
};
mod autonomy;
pub use autonomy::SynapseCalyxAutonomyDecisionReadback;
mod action_validation;
pub use action_validation::{
    ACTION_CAUSAL_GUARD_SLOTS, ACTION_CAUSAL_PREDICTOR_SLOTS, SynapseCalyxActionValidationEvidence,
    SynapseCalyxTypedActionPrediction,
};
mod async_vault;
pub mod backup;
mod causal_map;
mod causal_view_registry;
mod drift;
mod error_bridge;

pub use error_bridge::SYNAPSE_CALYX_BACKPRESSURE;
mod find;
mod grounding;
pub mod host_cuda;
mod intelligence;
pub mod kernel_maintenance;
pub mod lens_provenance;
pub mod lineage;
pub mod lowering;
mod math;
pub mod olap;
mod readiness;
mod search_commission;
pub mod timeseries;
pub use readiness::{
    SynapseCalyxReadinessEvidence, SynapseCalyxReadinessPredicate, SynapseCalyxReadinessSnapshot,
    SynapseCalyxReadinessSourceSignals, ensure_action_readiness_serving_admitted,
};
pub mod panel_lifecycle;
pub mod vault_runtime;
pub mod ward;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use calyx_assay::{AssayComputeBackend, configure_compute_backend, configured_compute_backend};
use calyx_aster::cf::{ColumnFamily, KeyRange, anchor_key, anchor_prefix_range};
use calyx_aster::compaction::CompactionResult;
use calyx_aster::dedup::EpochSecs;
use calyx_aster::erase::{EraseRegistry, EraseScope, subject_metadata_value};
use calyx_aster::mvcc::{
    Freshness, Snapshot, SnapshotDeltaRebaseReport, SnapshotVersionGcBudget, SnapshotVersionGcPass,
};
use calyx_aster::recurrence::{
    ConstellationRecurrenceAppendRequest, OccurrenceContext, RecurrenceAppendDisposition,
    RecurrenceAppendOnceRequest, RecurrenceSeriesReadback, RetentionPolicy, append_occurrence_once,
    append_occurrence_once_with_rows, append_occurrence_with_constellation_and_rows,
    read_series_readback,
};
pub use calyx_aster::vault::{
    AsterOrphanSlotCfRetirement, AsterOrphanSlotCfSkip, AsterOrphanSlotGcReport,
    EventTimeIndexBackfill, EventTimeIndexStatus,
};
use calyx_aster::vault::{
    AsterVault, MultiCxAnchorBatchOutcome, PutDisposition, RecoveryProgressHook,
    TemporalMetadataBackfill, TemporalMetadataMigration, VaultOptions, encode as vault_encode,
};
pub use calyx_core::TemporalPolicy;
use calyx_core::{
    Anchor, AnchorKind, AnchorValue, CalyxError, Clock, Constellation, CxId,
    METADATA_SOURCE_EVENT_TIME_RAW, METADATA_SOURCE_EVENT_TIME_SECS, METADATA_TEMPORAL_LANE_STATE,
    Panel, Seq, SystemClock, TEMPORAL_LANE_ACTIVE, Ts, VaultId, VaultStore,
};
use calyx_forge::{
    HostGpuReservation, HostGpuReservationRequest, HostGpuReservationSnapshot,
    HostGpuReservationStore,
};
use calyx_ledger::{ActorId, EntryKind, LedgerEntry, SubjectId, VerifyResult};
pub use calyx_registry::{
    CALYX_DYNAMIC_PANEL_GENERATION_FLOOR, CALYX_PANEL_GENERATION_UNCLAIMED,
    PanelGenerationAllocation, PanelGenerationAllocatorReadback, PanelGenerationClaim,
    PanelGenerationSupersession, VaultTemporalPanelRegistration,
    VaultTemporalPanelRegistrationWrite,
};
use calyx_registry::{
    CALYX_NO_ACTIVE_PANEL, Registry, VaultPanelState, VaultPanelWrite,
    allocate_vault_panel_generation, ensure_vault_panel_generation_claimed,
    list_vault_temporal_panels, load_vault_panel_state, persist_vault_panel_state,
    read_vault_panel_generation_allocator, read_vault_temporal_panel,
    register_vault_temporal_panel, reserve_vault_panel_generations,
    supersede_vault_panel_generations,
};
// Re-exported so a caller can hand a non-active panel contract to
// `find_similar_in_panel` / `rebuild_search_indexes_for_panel` (#1668) without
// depending on calyx-registry directly.
pub use calyx_registry::VaultPanelState as SynapseCalyxPanelState;
pub use calyx_search::{
    PersistedDenseIndexConfig, PersistedDenseQuantization, PersistedSearchGeneration,
    PersistedSearchSlot,
};
use calyx_sextant::{
    CausalConfidence, FreshnessTag, Hit, ProvenanceSource, TemporalScores, apply_temporal_boost,
};
use fs2::FileExt as _;
pub use host_cuda::{SynapseCalyxHostCudaProbe, host_cuda_device_probe};
pub use kernel_maintenance::{
    SYNAPSE_KERNEL_MAX_DOMAINS, SynapseCalyxKernelDomainOutcome, SynapseCalyxKernelHealthReport,
    SynapseCalyxKernelRebuildParams, SynapseCalyxKernelRebuildReport, VaultKernelArtifactStore,
};
pub use lineage::{
    ACKNOWLEDGE_RESET_ENV, SynapseCalyxVaultLineage, VaultLineageGeneration, VaultOpenGenesis,
    lineage_path,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ulid::Ulid;
pub use ward::{
    SYNAPSE_GUARD_DEFAULT_ALPHA, SYNAPSE_GUARD_DEFAULT_PROFILE_KEY, SYNAPSE_GUARD_MIN_GOOD_SCORES,
    SynapseCalyxGuardAspect, SynapseCalyxGuardCalibrateParams, SynapseCalyxGuardCalibrateReport,
    SynapseCalyxGuardSlotCalibration, SynapseCalyxGuardSlotSpec, SynapseCalyxGuardSlotVerdict,
    SynapseCalyxGuardVerifyParams, SynapseCalyxGuardVerifyReport, clopper_pearson_tail,
    min_certifiable_bad_scores,
};

pub use async_vault::{
    SynapseCalyxAsyncConfig, SynapseCalyxAsyncVault, SynapseCalyxAsyncVaultHandle,
    SynapseCalyxCfWrite, SynapseCalyxReaderLease,
};
pub use backup::{
    SynapseCalyxBackupExclusion, SynapseCalyxBackupFile, SynapseCalyxBackupLineage,
    SynapseCalyxBackupReport, SynapseCalyxVerifyReport, verify_vault_restore,
};
pub use causal_map::{
    SYNAPSE_CAUSAL_MAP_DEFAULT_FDR_ALPHA, SYNAPSE_CAUSAL_MAP_MAX_ALIGNED_CELLS,
    SYNAPSE_CAUSAL_MAP_MAX_ARTIFACT_BYTES, SYNAPSE_CAUSAL_MAP_MAX_PAIR_LAG_EVIDENCE_POINTS,
    SYNAPSE_CAUSAL_MAP_MAX_PAIR_ROWS, SYNAPSE_CAUSAL_MAP_MAX_PC_CI_TESTS,
    SYNAPSE_CAUSAL_MAP_PC_MAX_CONDITIONING, SynapseCalyxCausalEstimatorError,
    SynapseCalyxCausalEstimatorEvidence, SynapseCalyxCausalMapArtifact,
    SynapseCalyxCausalMapReport, SynapseCalyxCausalPairEvidence,
    SynapseCalyxCausalResourceAccounting, SynapseCalyxCausalStream, SynapseCalyxFdrDecision,
    SynapseCalyxFdrFamily,
};
pub use causal_view_registry::{
    SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES, SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_VIEWS_PER_PARENT,
    SYNAPSE_CAUSAL_VIEW_REGISTRY_MIN_VIEWS_PER_PARENT, SYNAPSE_CAUSAL_VIEW_REGISTRY_SCHEMA_VERSION,
    SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT, SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES,
    SynapseCalyxCausalViewAssociationPolicy, SynapseCalyxCausalViewAssociationScope,
    SynapseCalyxCausalViewAvailableUnmeasuredCode, SynapseCalyxCausalViewContract,
    SynapseCalyxCausalViewCpuCostClass, SynapseCalyxCausalViewDecision,
    SynapseCalyxCausalViewEstimatorCompatibility, SynapseCalyxCausalViewExclusionCode,
    SynapseCalyxCausalViewFamily, SynapseCalyxCausalViewLifecycle,
    SynapseCalyxCausalViewMeasurement, SynapseCalyxCausalViewMeasurementContract,
    SynapseCalyxCausalViewOutputContract, SynapseCalyxCausalViewOutputKind,
    SynapseCalyxCausalViewRegistry, SynapseCalyxCausalViewRegistryEvidence,
    SynapseCalyxCausalViewRegistryReadback, SynapseCalyxCausalViewRegistryScope,
    SynapseCalyxCausalViewResourceAccounting, SynapseCalyxCausalViewResult,
    SynapseCalyxCausalViewRuntime, SynapseCalyxCausalViewSelectionPowerState,
    SynapseCalyxCausalViewTransformSpec,
};
pub use drift::{
    SYNAPSE_BLIND_SPOT_ALPHA, SYNAPSE_BLIND_SPOT_MAX_ALERTS, SYNAPSE_BLIND_SPOT_MIN_SAMPLES,
    SYNAPSE_DRIFT_DEFAULT_RECENT_FRACTION, SYNAPSE_DRIFT_MAX_WINDOW, SYNAPSE_DRIFT_MIN_WINDOW,
    SynapseCalyxBlindSpotAlert, SynapseCalyxBlindSpotParams, SynapseCalyxBlindSpotReport,
    SynapseCalyxLensDrift, SynapseCalyxPanelDriftParams, SynapseCalyxPanelDriftReport,
    SynapseCalyxPersistedDriftFinding, SynapseCalyxPersistedNoveltyFinding,
    SynapseCalyxPersistedRecurrenceFinding, SynapseCalyxPersistedRegionFinding,
};
pub use find::{
    SYNAPSE_FIND_GUARD_DISABLED_CODE, SYNAPSE_FIND_MAX_K, SYNAPSE_FIND_RRF_K,
    SynapseCalyxDroppedGuardHit, SynapseCalyxFindFusion, SynapseCalyxFindGuard,
    SynapseCalyxFindGuardMode, SynapseCalyxFindGuardSlotVerdict, SynapseCalyxFindHit,
    SynapseCalyxFindLensContribution, SynapseCalyxFindParams, SynapseCalyxFindQuery,
    SynapseCalyxFindReport, SynapseCalyxFindTemporal, SynapseCalyxGuardVerdict,
    synapse_find_rrf_formula,
};
pub use grounding::{
    METADATA_SOURCE_CF, METADATA_SOURCE_KEY_HEX, SYNAPSE_GROUNDING_COVERAGE_FLOOR,
    SYNAPSE_GROUNDING_MAX_UNGROUNDED_SLOTS, SynapseCalyxAnchorKindCoverage,
    SynapseCalyxDomainGroundingVerdict, SynapseCalyxGroundingGapReport, SynapseCalyxPanelCensus,
    SynapseCalyxPanelCensusEntry, SynapseCalyxSlotGroundingCoverage, anchor_kind_label,
};
pub use intelligence::{
    SYNAPSE_ASSAY_ANCHOR_SOURCE_LEAKAGE, SYNAPSE_ASSAY_BIT_FLOOR,
    SYNAPSE_ASSAY_CORRELATION_CEILING, SYNAPSE_ASSAY_MIN_SAMPLES, SYNAPSE_ENSEMBLE_MAX_RECORDS,
    SYNAPSE_INTELLIGENCE_DELTA_RECORD_LIMIT_EXCEEDED, SYNAPSE_INTELLIGENCE_MAX_RECORDS,
    SYNAPSE_INTELLIGENCE_SOURCE_RANGE_INVALID, SYNAPSE_INTELLIGENCE_TIME_RANGE_UNINDEXED,
    SYNAPSE_KERNEL_DEFAULT_EDGE_COS, SYNAPSE_KERNEL_DEFAULT_KNN, SYNAPSE_KERNEL_DEFAULT_MAX_HOPS,
    SYNAPSE_KERNEL_DEFAULT_MIN_RECALL, SYNAPSE_KERNEL_MAX_REPORTED_MEMBERS, SYNAPSE_KNN_DEFAULT_K,
    SYNAPSE_KSG_DEFAULT_K, SYNAPSE_LENS_BLIND_SPOT_CEILING, SYNAPSE_SYNERGY_MAX_LENSES,
    SYNAPSE_SYNERGY_MAX_RECORDS, SYNAPSE_TEMPORAL_DEFAULT_BIN_SECS,
    SYNAPSE_TEMPORAL_DEFAULT_MAX_LAG, SYNAPSE_TEMPORAL_MAX_PEAKS, SYNAPSE_TEMPORAL_MIN_EVENTS,
    SynapseCalyxAbundanceReport, SynapseCalyxAgreementEdge, SynapseCalyxAnchorSourceCarrier,
    SynapseCalyxAssayParams, SynapseCalyxBetweenRecordEdge, SynapseCalyxBitsReport,
    SynapseCalyxCausalityLag, SynapseCalyxCausalityReport, SynapseCalyxCorpusSlotState,
    SynapseCalyxDriftReport, SynapseCalyxEnsembleCardReport, SynapseCalyxExcludedLens,
    SynapseCalyxExcludedLensCode, SynapseCalyxHazardReport, SynapseCalyxKernelAnswerHop,
    SynapseCalyxKernelAnswerReport, SynapseCalyxKernelParams, SynapseCalyxKernelReport,
    SynapseCalyxLensCoverageStatus, SynapseCalyxLowSignalLens, SynapseCalyxMathExecutionClass,
    SynapseCalyxNeffEstimate, SynapseCalyxPanelLensCoverage, SynapseCalyxPeriodicityReport,
    SynapseCalyxPeriodogramPeak, SynapseCalyxPhysicalLensBinding, SynapseCalyxRedundancyPair,
    SynapseCalyxRedundancyReport, SynapseCalyxRedundancySkip, SynapseCalyxSlotBits,
    SynapseCalyxSlotBitsState, SynapseCalyxSlotKind, SynapseCalyxSufficiencyDeficit,
    SynapseCalyxSufficiencyReport, SynapseCalyxSynergyReport, SynapseCalyxTemporalParams,
    SynapseCalyxWeaveBlindSpotPair, SynapseCalyxWeaveParams, SynapseCalyxWeaveReport,
};
pub use lowering::{
    LOWERED_ARTIFACT_MAGIC, LOWERED_ARTIFACT_SCHEMA_VERSION, LOWERED_DIR_NAME,
    LoadedLoweredArtifact, LoweredArtifactEnvelope, LoweredArtifactHandle, LoweredArtifactKind,
    LoweredArtifactState, LoweredFingerprint, LoweredGuardThresholds, LoweredPublishReport,
    LoweredRefreshOutcome, LoweredSafeDefault, LoweringParams, hot_context,
};
pub use math::{
    SynapseCalyxMathBackendStatus, SynapseCalyxMathProbeReport, SynapseCalyxMathProbeTopKEntry,
    SynapseCalyxMathRuntime, SynapseCalyxResidentL2GatherProbe, SynapseCalyxVramDispatchStatus,
    math_backend,
};
pub use search_commission::{
    SynapseCalyxSearchCommissionArtifact, SynapseCalyxSearchCommissionParams,
    SynapseCalyxSearchCommissionReport,
};

pub type SynapseCalyxCfRows = Vec<(Vec<u8>, Vec<u8>)>;

/// Process-scoped admission lease for non-Forge GPU consumers embedded in Synapse.
///
/// DirectML/ORT and other device runtimes must acquire this before creating a
/// device session; dropping the value releases the physical host reservation.
pub struct SynapseCalyxGpuReservation {
    inner: HostGpuReservation,
}

impl std::fmt::Debug for SynapseCalyxGpuReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SynapseCalyxGpuReservation")
            .field("admitted_snapshot", self.inner.admitted_snapshot())
            .finish_non_exhaustive()
    }
}

impl SynapseCalyxGpuReservation {
    /// Acquires an OS-wide Calyx reservation before a non-Forge GPU runtime starts.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx integration error when the physical ledger
    /// cannot be opened or the requested capacity is not admissible.
    pub fn acquire(
        device_index: u32,
        owner: impl Into<String>,
        job_id: impl Into<String>,
        command: impl Into<String>,
        requested_mib: u64,
    ) -> Result<Self, SynapseCalyxError> {
        let store = HostGpuReservationStore::from_env(device_index).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_GPU_RESERVATION_OPEN_FAILED",
                format!("open device-{device_index} host GPU reservation SoT: {error}"),
                "inspect the named Calyx GPU reservation directory and repair its permissions or state",
            )
        })?;
        let request = HostGpuReservationRequest::new(owner, job_id, command, requested_mib);
        let inner = store.acquire(request).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_GPU_RESERVATION_REFUSED",
                format!("device-{device_index} host GPU admission refused: {error}"),
                "wait for the named live GPU owner to release capacity or reduce a physically measured peak; never bypass admission or fall back to CPU",
            )
        })?;
        Ok(Self { inner })
    }

    #[must_use]
    pub fn admitted_snapshot(&self) -> &HostGpuReservationSnapshot {
        self.inner.admitted_snapshot()
    }
}

/// Rereads the OS-wide GPU reservation Source of Truth and transactionally
/// reaps lease rows whose owning process has released its file lock.
///
/// # Errors
///
/// Returns a structured integration error when the physical ledger or device
/// state cannot be read and verified.
pub fn readback_gpu_reservations(
    device_index: u32,
) -> Result<HostGpuReservationSnapshot, SynapseCalyxError> {
    HostGpuReservationStore::from_env(device_index)
        .and_then(|store| store.readback())
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_GPU_RESERVATION_READBACK_FAILED",
                format!("read back device-{device_index} host GPU reservation SoT: {error}"),
                "inspect the named Calyx GPU reservation directory and physical device state",
            )
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxSearchRawSidecar {
    pub path: PathBuf,
    pub layout: String,
    pub len_bytes: u64,
    pub file_count: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxSearchRebuildReport {
    pub expected_panel_version: u32,
    pub before_manifest_sha256: Option<String>,
    /// Process-private bytes immediately before the pinned rebuild starts.
    pub private_bytes_before: u64,
    /// Highest process-private byte count observed at rebuild progress boundaries.
    pub private_bytes_peak: u64,
    /// Exact rebuild boundary at which `private_bytes_peak` was observed.
    pub private_bytes_peak_phase: String,
    /// Process-private bytes after artifact reopen and independent validation.
    pub private_bytes_after: u64,
    /// Number of explicit allocator reclamation passes run at rebuild release boundaries.
    pub private_bytes_reclaim_calls: u64,
    /// Sum of process-private bytes observed returned after those passes.
    pub private_bytes_observed_reclaimed: u64,
    pub generation: PersistedSearchGeneration,
    pub manifest_path: PathBuf,
    pub raw_sidecars: Vec<SynapseCalyxSearchRawSidecar>,
}

/// Independently reopened physical state of one panel's exact membership
/// generation.
///
/// A membership-only generation contains the complete panel identity filter
/// but deliberately has no retrieval slots. It lets finite-only panels support
/// bounded exact analytics without falsely admitting them to semantic search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxPanelMembershipGenerationReport {
    pub panel_version: u32,
    pub built: bool,
    pub base_seq: u64,
    pub member_rows: u64,
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub sidecar_sha256: String,
}

/// Read-only state of the persisted search generation for the active panel.
///
/// Recall depends entirely on this generation, and nothing announced its state
/// before: an absent or badly-lagged generation was first observed by whoever
/// called `find` and read the error (issue #1891). This is the owning surface
/// that answers "is the search layer built, over how many rows, and when".
///
/// Reading it is cheap and side-effect free — the manifest, the rebuild marker
/// and the vault sequence, no CF scans — so `health` can report it every call.
// `Eq` is deliberately absent: `delta_coverage_ratio` is an `f64` (#1908), and a
// float has no total equality. `PartialEq` is what callers actually compare
// these with, and nothing uses this type as a key.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynapseCalyxSearchGenerationStatus {
    /// Active durable panel version, when one is published.
    pub panel_version: Option<u32>,
    /// Why no panel version is reported, when `panel_version` is `None`.
    pub panel_state_error: Option<String>,
    /// Expected manifest path for the active panel (reported even when absent,
    /// so the operator can look at the exact location).
    pub manifest_path: Option<String>,
    /// Whether the manifest file exists on disk.
    pub manifest_present: bool,
    pub manifest_sha256: Option<String>,
    /// Vault sequence the generation was built at.
    pub built_at_seq: Option<u64>,
    /// Current vault sequence. `latest_seq - built_at_seq` is how far behind the
    /// generation is.
    pub vault_latest_seq: u64,
    /// Exact-panel derived-content watermark observed atomically with
    /// `vault_latest_seq`.
    ///
    /// Panel-membership consumers reconcile an older immutable sidecar to this
    /// watermark through the same bounded panel delta as search queries. This
    /// remains distinct from `seq_lag`: only panel-scoped changed keys consume
    /// the reconciliation budget.
    pub panel_content_seq: Option<u64>,
    /// `latest_seq - built_at_seq`, saturating.
    ///
    /// Informational only. It is **not** the quantity the query-time limit is
    /// measured in, and it is not a proxy for it: 739 sequences carried 17,785
    /// changed keys on the production vault. Read `delta_changed_keys`.
    pub seq_lag: Option<u64>,
    /// Distinct changed keys between the generation's `base_seq` and the current
    /// snapshot — the exact quantity `MAX_RECONCILED_DELTA_KEYS` bounds, and so
    /// the only measurement that answers "can a query reconcile this generation".
    ///
    /// `None` when this status was read on the cheap path that does not scan for
    /// it; a `None` here is "not measured", never "zero".
    pub delta_changed_keys: Option<u64>,
    /// Where the measured delta came from: how many `Base` keys were scanned
    /// across all panels, how many belong to this panel, how many were excluded
    /// as another panel's churn, and each slot CF's contribution (#1901).
    ///
    /// Before the `Base` scan was panel-scoped, this generation's budget was
    /// charged for every other panel's ingest, so the count alone could not tell
    /// a genuinely stale generation from a bystander.
    pub delta_composition: Option<String>,
    /// When `delta_changed_keys` was measured, so a caller can tell a live
    /// measurement from a stale one.
    pub delta_measured_at_unix_ms: Option<u64>,
    /// Bounded delta-reconciliation limit. Once the changed-key count between
    /// `built_at_seq` and the pinned snapshot exceeds this, every query fails
    /// with `CALYX_SEARCH_DELTA_REBASE_REQUIRED` — so a `seq_lag` far above it
    /// is a strong predictor that recall is already dead.
    pub max_reconciled_delta_keys: u64,
    /// Largest per-slot row count in the generation — the rows recall can reach.
    pub rows_covered: Option<u64>,
    /// `delta_changed_keys / rows_covered` — how much of *this* generation has
    /// been superseded (#1908).
    ///
    /// The absolute key count cannot answer that on its own: 461 changed keys is
    /// negligible against the 8192 query-time limit and is **total** churn for a
    /// 461-row generation. Recall stops being served by the index and starts
    /// being served entirely by delta reconciliation at `1.0`, and every query
    /// then rebuilds the corpus statistics from the delta.
    ///
    /// This is the tombstone ratio that segment-based engines schedule
    /// consolidation on, and reporting it makes "this generation is fully
    /// superseded" readable rather than something a caller has to derive by
    /// dividing two fields. `None` when either input was not measured; values
    /// above `1.0` are possible and meaningful (rows changed more than once, or
    /// changed rows the generation never indexed).
    pub delta_coverage_ratio: Option<f64>,
    /// Per-slot index presence: dense and sparse lanes are reported separately
    /// because they fail independently.
    pub slots: Vec<SynapseCalyxSearchGenerationSlot>,
    pub dense_slot_count: u64,
    pub sparse_slot_count: u64,
    /// Manifest file modification time, as the build timestamp.
    pub built_at_unix_ms: Option<u64>,
    /// Age of the build in milliseconds, at read time.
    pub age_ms: Option<u64>,
    /// A staked rebuild-required intent, when one is present.
    pub rebuild_required: Option<String>,
    /// `absent` | `rebuild_required` | `lagging` | `built`. `absent` and
    /// `rebuild_required` mean recall cannot serve at all; `lagging` means the
    /// measured changed-key delta exceeds the bounded reconciliation limit.
    pub state: String,
    /// What to do about the reported state, or `none` when it is healthy.
    pub remediation: String,
}

/// One persisted-index slot's presence in the generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxSearchGenerationSlot {
    pub slot: u16,
    /// Index kind, e.g. `flat_dense`, `diskann`, `sparse_dot`.
    pub kind: String,
    /// `dense` or `sparse` — the retrieval lane this slot serves.
    pub lane: String,
    /// Rows indexed for this slot.
    pub len: u64,
    pub built_at_seq: u64,
}

/// Outcome of publishing an active durable `Panel` snapshot to the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxPanelPublishReport {
    /// Panel version whose contract was requested for publication.
    pub panel_version: u32,
    /// True when this call durably wrote a new active-panel manifest; false when
    /// the exact panel version was already published (idempotent no-op).
    pub published: bool,
    /// Manifest `panel_ref` logical path after the operation.
    pub panel_ref: String,
    /// Manifest `registry_ref` logical path after the operation, if present.
    pub registry_ref: Option<String>,
    /// Panel version proven by an independent `load_vault_panel_state` readback.
    pub readback_panel_version: u32,
}

fn same_panel_definition(left: &Panel, right: &Panel) -> bool {
    let Panel {
        version: left_version,
        slots: left_slots,
        created_at: _,
        kernel_ref: left_kernel_ref,
        guard_ref: left_guard_ref,
    } = left;
    let Panel {
        version: right_version,
        slots: right_slots,
        created_at: _,
        kernel_ref: right_kernel_ref,
        guard_ref: right_guard_ref,
    } = right;
    left_version == right_version
        && left_slots == right_slots
        && left_kernel_ref == right_kernel_ref
        && left_guard_ref == right_guard_ref
}

fn verify_published_panel_readback(
    state: &calyx_registry::VaultPanelState,
    panel: &Panel,
    registry: &Registry,
) -> Result<(), SynapseCalyxError> {
    if state.panel.version != panel.version || !same_panel_definition(&state.panel, panel) {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_PANEL_PUBLISH_READBACK_MISMATCH",
            format!(
                "published active panel {} but readback resolved panel {} with a different immutable definition",
                panel.version, state.panel.version
            ),
            "inspect the durable manifest panel_ref and immutable panel asset before retrying publication",
        ));
    }
    let desired_registry = registry.lens_snapshots();
    if !state
        .registry_snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.lenses == desired_registry)
    {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_PANEL_REGISTRY_READBACK_MISMATCH",
            format!(
                "published panel {} registry but the durable snapshot does not contain the exact {} requested lenses",
                panel.version,
                desired_registry.len()
            ),
            "inspect the manifest registry_ref and immutable registry asset before retrying publication",
        ));
    }
    Ok(())
}

fn search_rebuild_error(action: &str, error: calyx_search::SearchError) -> SynapseCalyxError {
    match error {
        calyx_search::SearchError::Calyx(error) => SynapseCalyxError::from_calyx(action, &error),
        error => SynapseCalyxError::new(
            error.code(),
            format!("{action}: {}", error.message()),
            "inspect the rebuild marker, durable panel state, and named physical search artifact before retrying",
        ),
    }
}

/// The generation status for a vault with no active durable panel published.
///
/// Without an active panel there is nothing a generation could be built *for*, so
/// the state is `absent` and the repair is to publish the panel first — not to run
/// a rebuild that would fail with `SYNAPSE_CALYX_NO_ACTIVE_PANEL`.
fn search_generation_status_without_panel(
    panel_state_error: Option<String>,
    vault_latest_seq: u64,
    max_reconciled_delta_keys: u64,
) -> SynapseCalyxSearchGenerationStatus {
    SynapseCalyxSearchGenerationStatus {
        panel_version: None,
        panel_state_error,
        manifest_path: None,
        manifest_present: false,
        manifest_sha256: None,
        built_at_seq: None,
        vault_latest_seq,
        panel_content_seq: None,
        seq_lag: None,
        delta_changed_keys: None,
        delta_composition: None,
        delta_measured_at_unix_ms: None,
        max_reconciled_delta_keys,
        rows_covered: None,
        delta_coverage_ratio: None,
        slots: Vec::new(),
        dense_slot_count: 0,
        sparse_slot_count: 0,
        built_at_unix_ms: None,
        age_ms: None,
        rebuild_required: None,
        state: "absent".to_owned(),
        remediation:
            "publish the active panel (publish_active_panel / boot panel publication), then build \
             the search generation; until then no recall path can serve a query"
                .to_owned(),
    }
}

/// Decides the reported state of a persisted search generation and what to do
/// about it (issue #1891).
///
/// `built` is the only healthy verdict. Everything else means recall either
/// cannot serve at all or is expected to fail closed, so the caller reports it as
/// an error rather than a warning — a search surface that presents as available
/// while being inert is the defect family this exists to close.
const fn classify_search_generation(
    rebuild_marker_staked: bool,
    manifest_present: bool,
    built_at_seq: Option<u64>,
    delta_changed_keys: Option<u64>,
    max_reconciled_delta_keys: u64,
) -> (&'static str, &'static str) {
    if rebuild_marker_staked {
        return (
            "rebuild_required",
            "a mutation staked a rebuild-required intent; the generation is stale until a rebuild \
             republishes the manifest. Run storage operation=search_rebuild.",
        );
    }
    if !manifest_present {
        return (
            "absent",
            "no search generation exists for the active panel, so no recall path can serve a \
             query. Build it with storage operation=search_rebuild.",
        );
    }
    if built_at_seq.is_none() {
        return (
            "rebuild_required",
            "the manifest exists but does not describe a usable generation for this panel \
             (format, panel, or slot shape mismatch). Rebuild it with storage \
             operation=search_rebuild.",
        );
    }
    // Classified on the measured changed-key count, because that is the exact
    // quantity `MAX_RECONCILED_DELTA_KEYS` bounds. Classifying on the sequence
    // lag instead reported `built` on a generation whose delta was 17,785 keys
    // — more than twice the limit — while every query failed closed (#1891).
    match delta_changed_keys {
        Some(keys) if keys > max_reconciled_delta_keys => (
            "lagging",
            "the generation's measured changed-key delta exceeds the bounded \
             delta-reconciliation limit, so queries fail closed with \
             CALYX_SEARCH_DELTA_REBASE_REQUIRED. Rebuild it with storage \
             operation=search_rebuild.",
        ),
        Some(_) => ("built", "none"),
        // Not measured is not the same as measured-and-fine. Reporting `built`
        // here would be the assertion that broke this surface once already.
        None => (
            "built_delta_unmeasured",
            "the manifest is present and parseable, but this read did not measure the \
             changed-key delta, so whether a query can reconcile the generation is \
             unknown. The derived-state maintainer measures it on every tick; read its \
             last measurement, or use the delta-measuring status path.",
        ),
    }
}

/// How many **changed keys** the persisted search generation is allowed to fall
/// behind by before the unattended maintainer refreshes it (issue #1891, ask 2).
///
/// This is deliberately **below** [`calyx_search::MAX_RECONCILED_DELTA_KEYS`],
/// which is the point at which a query *dies* with
/// `CALYX_SEARCH_DELTA_REBASE_REQUIRED`. Reusing the query-tolerance limit as
/// the maintenance trigger would mean the generation is only ever repaired
/// after recall has already failed. Keeping the consolidation threshold
/// separate from, and stricter than, the query-time limit is the standard shape
/// for incrementally-maintained retrieval indexes — Postgres BM25 extensions
/// separate `auto_rebuild_threshold` from their query overlay budget, and
/// FreshDiskANN-style engines consolidate at a pending fraction of the base
/// rather than at the point of failure.
///
/// **The unit matters, and it is not the sequence lag.** This threshold was
/// first written against `vault_latest_seq - built_at_seq`, on the assumption
/// that a bounded sequence lag bounds the reconciliation work. It does not: one
/// vault sequence can change many keys. Measured on the production vault
/// immediately after a successful rebuild, a lag of **739 sequences** carried
/// **17,785 changed keys** — more than twice the query-time limit — so `health`
/// reported `state=built` while every query failed closed. A maintenance
/// trigger has to be measured in the same quantity as the failure it prevents,
/// or it is a proxy that silently stops tracking.
/// Reader-lease lifetime for the changed-key delta scan.
///
/// Bounded and short: the scan is a bulk read on a maintenance tick, and a
/// reader lease held longer than it needs pins the GC frontier.
const SEARCH_DELTA_SCAN_LEASE_MS: u64 = 30_000;
/// Reader-lease lifetime for bounded off-runtime corpus scans that enumerate
/// Base rows and hydrate their slot rows from the same MVCC view.
pub(crate) const INTELLIGENCE_CORPUS_READER_LEASE_MS: u64 = 30_000;
/// Maximum panel-membership point reads between lease heartbeats.
///
/// Renewal preserves the same pinned sequence; it does not rebase the reader.
/// A finite row cadence lets a progressing whole-panel scan outlive the lease
/// TTL while a stalled or abandoned scan still expires normally.
const PANEL_BASE_SNAPSHOT_RENEW_ROWS: usize = 1_024;
/// Reader-lease lifetime for MMD drift's exact two-pass bounded corpus.
///
/// The public drift contract accepts up to 20,000 selected records and must
/// hydrate that bounded set twice at one coherent MVCC sequence. The generic
/// 30-second intelligence lease is sufficient for the scheduled 2,000-row
/// pass but expires during the supported maximum on the production vault.
/// Five minutes is the repository's finite coherent-scan hard ceiling; the
/// scoped snapshot still releases the lease immediately on every return path.
pub(crate) const MMD_DRIFT_CORPUS_READER_LEASE_MS: u64 = 5 * 60_000;
/// Reader-lease lifetime for native OLAP's whole-panel slot materialization.
///
/// The live timeline panel carries hundreds of thousands of rows, so the
/// generic five-second Aster point-read lease can expire during its bounded
/// Base walk. One finite caller-owned lease spans Base membership discovery,
/// slot hydration, durable column publication, and the aggregate scan; scope
/// exit releases it immediately.
pub(crate) const OLAP_MATERIALIZATION_READER_LEASE_MS: u64 = 5 * 60_000;
/// Reader-lease lifetime for an exact physical whole-CF census.
///
/// Unlike a bounded intelligence corpus, this operation deliberately walks
/// every live key state. The production Graph CF contains millions of rows, so
/// applying the short corpus lease made a correct first census fail just before
/// completion. The census still owns one pinned snapshot and still fails closed
/// at a finite boundary; it simply has a lease sized for the operation it
/// actually performs.
const CF_COUNT_READER_LEASE_MS: u64 = 5 * 60_000;

pub const SEARCH_GENERATION_REFRESH_DELTA_KEYS: u64 =
    (calyx_search::MAX_RECONCILED_DELTA_KEYS as u64) / 2;

/// Fraction of a generation's own rows that may be superseded before the
/// unattended maintainer refreshes it (issue #1908).
///
/// [`SEARCH_GENERATION_REFRESH_DELTA_KEYS`] is a fraction of the **query-time
/// reconciliation limit**, which is a property of the query path, not of the
/// generation being maintained. For any generation smaller than that limit the
/// absolute trigger cannot fire before the generation is *entirely* superseded:
/// the live timeline generation is 461 rows, so 100% churn is still 9x below
/// 4096 and the maintainer reported `none_needed` against a delta covering
/// every row it had indexed. That is the missing denominator this supplies —
/// the same lesson as #1891 (measure the trigger in the quantity of the failure
/// it prevents) applied one level up.
///
/// The unit is the ratio the doc above already reaches for when it says
/// FreshDiskANN-style engines "consolidate at a pending fraction of the base".
/// Segment engines schedule on exactly this quantity: Lucene records superseded
/// documents in a per-segment bitset and reclaims them at merge, and
/// Elasticsearch's tiered policy targets segments past ~10% deletions
/// (`only_expunge_deletes` uses the same default).
///
/// 0.25 rather than Lucene's 0.10 because a Synapse rebuild replaces the whole
/// generation rather than merging two segments, so each refresh costs more than
/// a tiered merge does; a quarter of the rows superseded is still ~18x earlier
/// than the absolute trigger on the live panel. Thrash is bounded independently
/// by the minimum gap between unattended builds.
pub const SEARCH_GENERATION_REFRESH_COVERAGE_RATIO: f64 = 0.25;

/// Superseded fraction of a generation, or `None` when either input is
/// unmeasured or the generation covers no rows.
///
/// A `None` must never read as zero: an unmeasured delta is unknown, and a
/// zero-row generation has no denominator to divide by.
// Both inputs are row/key counts for one panel generation, bounded by the vault's
// row count and orders of magnitude below f64's 2^53 exact-integer range, so the
// conversion is lossless in every reachable state.
#[allow(clippy::cast_precision_loss)]
fn delta_coverage_ratio(delta_changed_keys: Option<u64>, rows_covered: Option<u64>) -> Option<f64> {
    match (delta_changed_keys, rows_covered) {
        (Some(changed), Some(rows)) if rows > 0 => Some(changed as f64 / rows as f64),
        _ => None,
    }
}

/// Minimum wall-clock gap between two unattended generation builds.
///
/// A rebuild republishes the whole generation, so a busy write stream could
/// otherwise drive back-to-back rebuilds and spend the maintenance budget on
/// nothing else. This is the same "background naptime" bound managed vector
/// stores expose for automatic index maintenance. It does not weaken the
/// freshness guarantee: the seq-lag threshold above leaves a full budget of
/// headroom, so one skipped cadence cannot push the generation past the point
/// where queries fail.
pub const SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS: u64 = 10 * 60 * 1000;

/// What the unattended maintainer decided to do about the search generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchGenerationMaintenanceAction {
    /// No generation existed. Building one replaces nothing, so it is not a
    /// destructive maintenance mutation (issue #1891, ask 3).
    InitialBuild,
    /// A generation existed and was republished because it had drifted past
    /// [`SEARCH_GENERATION_REFRESH_SEQ_LAG`] or carried a staked
    /// rebuild-required marker. This one *does* replace a live artifact.
    RefreshOverExisting,
    /// The generation is inside its freshness budget; nothing was written.
    NoneNeeded,
    /// A build was due but was held back by
    /// [`SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS`].
    DeferredByInterval,
    // There is deliberately no `NoActivePanel` action. Maintenance is addressed
    // by generation, not by the active-panel pointer (#1938); "no active panel
    // is published" is a fact about the vault that the sweep reports as
    // `active_panel_version: None`, not an outcome of maintaining a generation.
}

impl SearchGenerationMaintenanceAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InitialBuild => "initial_build",
            Self::RefreshOverExisting => "refresh_over_existing",
            Self::NoneNeeded => "none_needed",
            Self::DeferredByInterval => "deferred_by_interval",
        }
    }

    /// Whether this action replaced a live artifact rather than creating a
    /// first one. The two are gated differently on purpose (#1891 ask 3).
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::RefreshOverExisting)
    }
}

/// One constellation's `Base` and slot row MVCC sequences (issue #1935).
///
/// The fact set that identifies which writer left a slot row newer than its own
/// `Base` row. Read straight off the version chains, never derived.
#[derive(Clone, Debug)]
pub struct ConstellationRowSequences {
    pub cx_id: String,
    pub latest_seq: u64,
    /// The panel the `Base` row declares, when the row is present.
    pub panel_version: Option<u32>,
    pub base_row_present: bool,
    pub base_row_seq: Option<u64>,
    /// `(slot id, row sequence)` for every slot the `Base` row declares, in slot
    /// order. `None` means the slot is declared but has no visible row.
    pub slot_row_seqs: Vec<(u16, Option<u64>)>,
}

/// The search generations physically published under a vault's index root
/// (issue #1938).
///
/// The set that exists is discovered from disk, never inferred from the active
/// panel pointer: those are different questions, and answering the second when
/// the first was asked is what left non-active generations with no maintainer.
#[derive(Clone, Debug, Default)]
pub struct PublishedSearchGenerations {
    /// `<vault_dir>/idx/search`, reported so an operator can go look even when
    /// the listing is empty.
    pub index_root: std::path::PathBuf,
    /// Panel versions holding a published `manifest.json`, ascending.
    pub panels: Vec<u32>,
    /// Entries under the index root that are not a published panel generation.
    /// Reported rather than skipped: a sweep that silently narrows its own scope
    /// is indistinguishable from a sweep that covered everything.
    pub unrecognized: Vec<String>,
}

/// Evidence that one published search generation was retired (#1972).
///
/// Carries the index root's published set from **before and after** the
/// removal, not just a success flag: "the directory is gone" is the claim, and
/// the two enumerations are the proof.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SynapseCalyxRetiredSearchGeneration {
    pub panel_version: u32,
    /// The exact directory removed.
    pub directory: String,
    /// Files it held, counted before removal.
    pub files_removed: u64,
    /// Bytes it held, counted before removal.
    pub bytes_reclaimed: u64,
    /// Published panel versions before the removal.
    pub published_before: Vec<u32>,
    /// Published panel versions re-enumerated after the removal.
    pub published_after: Vec<u32>,
    /// The vault's active panel, which is never retirable.
    pub active_panel_version: Option<u32>,
}

/// Total bytes of the files directly inside `directory`, or zero when it cannot
/// be listed. Used only to report what a retirement reclaimed, so an unreadable
/// directory reports nothing reclaimed rather than failing the retirement.
fn directory_bytes(directory: &std::path::Path) -> u64 {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

/// Count of the files directly inside `directory`. See [`directory_bytes`].
fn directory_file_count(directory: &std::path::Path) -> u64 {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.metadata().is_ok_and(|metadata| metadata.is_file()))
        .count() as u64
}

/// One unattended search-generation maintenance pass, with the state read back
/// from disk after the work rather than assumed from the return of the build.
#[derive(Clone, Debug)]
pub struct SearchGenerationMaintenanceReport {
    pub action: SearchGenerationMaintenanceAction,
    pub reason: String,
    /// Generation state observed before the pass decided anything.
    pub before: SynapseCalyxSearchGenerationStatus,
    /// Generation state re-read from disk after the pass, when it built.
    pub after: Option<SynapseCalyxSearchGenerationStatus>,
    pub rebuild_private_bytes_before: Option<u64>,
    pub rebuild_private_bytes_peak: Option<u64>,
    pub rebuild_private_bytes_peak_phase: Option<String>,
    pub rebuild_private_bytes_after: Option<u64>,
    pub rebuild_private_bytes_reclaim_calls: Option<u64>,
    pub rebuild_private_bytes_observed_reclaimed: Option<u64>,
    pub elapsed_ms: u64,
}

/// Classifies a persisted-index kind into the retrieval lane it serves.
///
/// Dense and sparse lanes fail independently — a vault can carry a built dense
/// ANN index and no BM25 lane at all — so an operational surface must report
/// them separately rather than as one "index present" bit (issue #1891).
fn search_slot_lane(kind: &str) -> &'static str {
    if kind.contains("sparse") {
        "sparse"
    } else if kind.contains("dense") || kind.contains("diskann") || kind.contains("multi") {
        "dense"
    } else {
        "unknown"
    }
}

/// File modification time in Unix milliseconds, or `None` when unavailable.
fn file_modified_unix_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

fn read_optional_sha256(path: &Path) -> Result<Option<String>, SynapseCalyxError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(sha256_hex(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
            format!("read {} for SHA-256: {error}", path.display()),
            "repair access to the exact artifact path and retry without deleting the last known-good generation",
        )),
    }
}

fn parse_cx_id(raw: &str) -> Result<CxId, SynapseCalyxError> {
    let trimmed = raw.trim();
    CxId::from_str(trimmed).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_CX_ID_INVALID",
            format!("invalid content-addressed cx_id {trimmed:?}: {error}"),
            "supply the exact lowercase 32-hex-character constellation id read back from a ledger/provenance readback",
        )
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn publish_panel_input_snapshot_chunk_checked(
    vault: &AsterVault<SynapseCalyxClock>,
    panel_version: u32,
    ids: &[CxId],
    prior_frontier: u64,
    chunk_number: usize,
) -> Result<u64, SynapseCalyxError> {
    let publication = vault
        .publish_panel_input_snapshot_chunk(panel_version, ids)
        .map_err(|error| {
            SynapseCalyxError::from_calyx(
                &format!(
                    "publish panel {panel_version} authoritative association-input snapshot chunk {chunk_number}"
                ),
                &error,
            )
        })?;
    if publication.committed_seq <= prior_frontier {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_SEQUENCE_INVALID",
            format!(
                "panel {panel_version} snapshot chunk {chunk_number} committed seq {} after prior frontier {prior_frontier}",
                publication.committed_seq
            ),
            "preserve the WAL and reconcile the sequence allocator before accepting the bootstrap stream",
        ));
    }
    Ok(publication.committed_seq)
}

fn inspect_search_raw_sidecars(
    panel_root: &Path,
) -> Result<Vec<SynapseCalyxSearchRawSidecar>, SynapseCalyxError> {
    let mut pending = vec![panel_root.to_path_buf()];
    let mut raw_paths = Vec::new();
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                format!("read search artifact directory {}: {error}", dir.display()),
                "inspect the published search generation and filesystem access before retrying",
            )
        })?;
        for entry in entries {
            let path = entry
                .map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                        format!("read search artifact entry in {}: {error}", dir.display()),
                        "inspect the published search generation and filesystem access before retrying",
                    )
                })?
                .path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                    format!("inspect search artifact {}: {error}", path.display()),
                    "repair the exact artifact and rebuild from authoritative Base rows",
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_ARTIFACT_UNSAFE",
                    format!("search artifact {} is a symlink", path.display()),
                    "replace the symlink with a real artifact rebuilt from authoritative Base rows",
                ));
            }
            if path.extension().is_some_and(|extension| extension == "raw") {
                raw_paths.push(path);
            } else if metadata.is_dir() {
                pending.push(path);
            }
        }
    }
    raw_paths.sort();
    raw_paths
        .into_iter()
        .map(|path| inspect_search_raw_sidecar(&path))
        .collect()
}

fn inspect_search_raw_sidecar(
    path: &Path,
) -> Result<SynapseCalyxSearchRawSidecar, SynapseCalyxError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
            format!("inspect raw sidecar {}: {error}", path.display()),
            "repair the exact artifact and rebuild from authoritative Base rows",
        )
    })?;
    if metadata.is_file() {
        let bytes = fs::read(path).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                format!("read packed raw sidecar {}: {error}", path.display()),
                "repair the exact artifact and rebuild from authoritative Base rows",
            )
        })?;
        validate_packed_raw_sidecar(path, &bytes)?;
        return Ok(SynapseCalyxSearchRawSidecar {
            path: path.to_path_buf(),
            layout: "packed_v2".to_owned(),
            len_bytes: metadata.len(),
            file_count: 1,
            sha256: Some(sha256_hex(&bytes)),
        });
    }
    if metadata.is_dir() {
        let mut file_count = 0_u64;
        let mut len_bytes = 0_u64;
        for entry in fs::read_dir(path).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                format!("read legacy raw sidecar {}: {error}", path.display()),
                "rebuild the exact panel to migrate the legacy sidecar",
            )
        })? {
            let child = entry
                .map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                        format!("read legacy raw sidecar entry: {error}"),
                        "rebuild the exact panel to migrate the legacy sidecar",
                    )
                })?
                .path();
            let child_metadata = fs::symlink_metadata(&child).map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_ARTIFACT_IO",
                    format!(
                        "inspect legacy raw sidecar row {}: {error}",
                        child.display()
                    ),
                    "rebuild the exact panel to migrate the legacy sidecar",
                )
            })?;
            if child_metadata.file_type().is_symlink() || !child_metadata.is_file() {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_ARTIFACT_UNSAFE",
                    format!(
                        "legacy raw sidecar child {} is not a real file",
                        child.display()
                    ),
                    "rebuild the exact panel from authoritative Base rows",
                ));
            }
            file_count = file_count.checked_add(1).ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_ARTIFACT_OVERFLOW",
                    format!("legacy raw sidecar {} file count overflow", path.display()),
                    "inspect the artifact tree and rebuild the exact panel",
                )
            })?;
            len_bytes = len_bytes.checked_add(child_metadata.len()).ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_ARTIFACT_OVERFLOW",
                    format!("legacy raw sidecar {} byte count overflow", path.display()),
                    "inspect the artifact tree and rebuild the exact panel",
                )
            })?;
        }
        return Ok(SynapseCalyxSearchRawSidecar {
            path: path.to_path_buf(),
            layout: "legacy_v1_directory".to_owned(),
            len_bytes,
            file_count,
            sha256: None,
        });
    }
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_SEARCH_ARTIFACT_UNSAFE",
        format!(
            "raw sidecar {} is neither a file nor directory",
            path.display()
        ),
        "rebuild the exact panel from authoritative Base rows",
    ))
}

fn validate_packed_raw_sidecar(path: &Path, bytes: &[u8]) -> Result<(), SynapseCalyxError> {
    const HEADER: usize = calyx_sextant::index::RAW_SIDECAR_HEADER_SIZE;
    if bytes.len() < HEADER || bytes[..8] != calyx_sextant::index::RAW_SIDECAR_MAGIC {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_SEARCH_ARTIFACT_CORRUPT",
            format!(
                "packed raw sidecar {} has a missing/invalid header",
                path.display()
            ),
            "rebuild the exact panel from authoritative Base rows",
        ));
    }
    let mut version_bytes = [0_u8; 4];
    version_bytes.copy_from_slice(&bytes[8..12]);
    let version = u32::from_le_bytes(version_bytes);
    let mut dim_bytes = [0_u8; 4];
    dim_bytes.copy_from_slice(&bytes[12..16]);
    let dim = u32::from_le_bytes(dim_bytes);
    let mut row_bytes = [0_u8; 8];
    row_bytes.copy_from_slice(&bytes[16..24]);
    let rows = u64::from_le_bytes(row_bytes);
    let payload = rows
        .checked_mul(u64::from(dim))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| value.checked_add(HEADER as u64));
    if version != calyx_sextant::index::RAW_SIDECAR_PACKED_VERSION
        || dim == 0
        || payload != u64::try_from(bytes.len()).ok()
    {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_SEARCH_ARTIFACT_CORRUPT",
            format!(
                "packed raw sidecar {} version/dim/length contract is invalid",
                path.display()
            ),
            "rebuild the exact panel from authoritative Base rows",
        ));
    }
    Ok(())
}

/// One raw Calyx value plus SHA-256 of its exact plaintext physical bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseCalyxRevisionedValue {
    pub value: Vec<u8>,
    pub revision_sha256: [u8; 32],
}

/// One physical Calyx CF revision precondition.
///
/// `expected_revision_sha256` is `None` only when the physical row must be
/// absent. Guards are evaluated in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseCalyxRevisionGuard {
    pub cf: ColumnFamily,
    pub key: Vec<u8>,
    pub expected_revision_sha256: Option<[u8; 32]>,
}

impl SynapseCalyxRevisionGuard {
    #[must_use]
    pub fn new(
        cf: ColumnFamily,
        key: impl Into<Vec<u8>>,
        expected_revision_sha256: Option<[u8; 32]>,
    ) -> Self {
        Self {
            cf,
            key: key.into(),
            expected_revision_sha256,
        }
    }
}

impl From<SynapseCalyxRevisionGuard> for calyx_aster::vault::CfRevisionGuard {
    fn from(guard: SynapseCalyxRevisionGuard) -> Self {
        Self::new(guard.cf, guard.key, guard.expected_revision_sha256)
    }
}

/// The first ordered Calyx revision guard that conflicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseCalyxConditionalWriteConflict {
    pub guard_index: usize,
    pub cf: ColumnFamily,
    pub key: Vec<u8>,
    pub expected_revision_sha256: Option<[u8; 32]>,
    pub actual_revision_sha256: Option<[u8; 32]>,
}

impl From<calyx_aster::vault::ConditionalCfWriteConflict> for SynapseCalyxConditionalWriteConflict {
    fn from(conflict: calyx_aster::vault::ConditionalCfWriteConflict) -> Self {
        Self {
            guard_index: conflict.guard_index,
            cf: conflict.cf,
            key: conflict.key,
            expected_revision_sha256: conflict.expected_revision,
            actual_revision_sha256: conflict.actual_revision,
        }
    }
}

/// Outcome of one atomic multi-key Calyx conditional mutation.
///
/// `actual_revisions_sha256` always follows guard-input order. On success,
/// `committed_revisions_sha256` has the same length; a deleted guard is
/// represented by `None`. On conflict, no row is mutated,
/// `committed_revisions_sha256` is empty, and `conflict` identifies the first
/// mismatching guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseCalyxMultiConditionalWriteOutcome {
    pub applied: bool,
    pub committed_seq: Seq,
    pub actual_revisions_sha256: Vec<Option<[u8; 32]>>,
    pub committed_revisions_sha256: Vec<Option<[u8; 32]>>,
    pub conflict: Option<SynapseCalyxConditionalWriteConflict>,
}

/// Structured failure from a guarded Calyx write.
///
/// `committed_seq` is present only when Calyx proved that the operation crossed
/// its irreversible commit boundary before the reported failure. Callers must
/// reconcile that exact sequence and must not infer it from a later global tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseCalyxConditionalWriteError {
    pub source: SynapseCalyxError,
    pub committed_seq: Option<Seq>,
}

impl std::fmt::Display for SynapseCalyxConditionalWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.committed_seq {
            Some(seq) => write!(formatter, "{}; committed_seq={seq}", self.source),
            None => self.source.fmt(formatter),
        }
    }
}

impl std::error::Error for SynapseCalyxConditionalWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<calyx_aster::vault::MultiConditionalCfWriteOutcome>
    for SynapseCalyxMultiConditionalWriteOutcome
{
    fn from(outcome: calyx_aster::vault::MultiConditionalCfWriteOutcome) -> Self {
        Self {
            applied: outcome.applied,
            committed_seq: outcome.seq,
            actual_revisions_sha256: outcome.actual_revisions,
            committed_revisions_sha256: outcome.committed_revisions,
            conflict: outcome.conflict.map(Into::into),
        }
    }
}

/// Compatibility outcome for a single-key Calyx conditional mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynapseCalyxConditionalWriteOutcome {
    pub applied: bool,
    pub committed_seq: Seq,
    pub previous_revision_sha256: Option<[u8; 32]>,
    pub committed_revision_sha256: Option<[u8; 32]>,
}

impl From<calyx_aster::vault::ConditionalCfWriteOutcome> for SynapseCalyxConditionalWriteOutcome {
    fn from(outcome: calyx_aster::vault::ConditionalCfWriteOutcome) -> Self {
        Self {
            applied: outcome.applied,
            committed_seq: outcome.seq,
            previous_revision_sha256: outcome.previous_revision,
            committed_revision_sha256: outcome.committed_revision,
        }
    }
}

/// Rows represented by one logical reporting group in a
/// [`SynapseCalyxVault::walk_cf_latest`] result.
///
/// #1968 established 256 as the bounded handoff. The current cursor lends one
/// row at a time and reuses its source buffers, so this value is provenance and
/// progress grouping only: it never allocates an output page and must never
/// become the unit at which SST readers are reopened or re-sought. #2243
/// physically measured the latter mistake taking 49,915 source reconstructions
/// to discover 2,000 matching records and driving a real daemon to a 2.824 GiB
/// lifetime peak.
pub const SYNAPSE_CALYX_CF_WALK_PAGE_ROWS: usize = 256;
/// Base-specific logical reporting group size.
///
/// Preserved for stable walk provenance. Base rows are now lent directly from
/// the allocation-reusing cursor; this value does not allocate a 16-row page.
pub const SYNAPSE_CALYX_BASE_CF_WALK_PAGE_ROWS: usize = 16;

/// What a [`SynapseCalyxVault::walk_cf_latest`] visitor asks for next.
///
/// A fold that has already found its answer must be able to stop without
/// paging the rest of the column family — two of the migrated #1968 callers
/// (`discover_panel_domains`, `load_record_slots`) break early, and turning
/// their `break` into "page to the end anyway" would have traded one whole-CF
/// cost for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynapseCalyxWalkStep {
    /// Keep paging.
    Continue,
    /// Stop the walk; `stopped_early` is reported as `true`.
    Stop,
}

enum SynapseCalyxSnapshotWalkControl {
    Error(SynapseCalyxError),
}

impl From<calyx_core::CalyxError> for SynapseCalyxSnapshotWalkControl {
    fn from(error: calyx_core::CalyxError) -> Self {
        Self::Error(SynapseCalyxError::from_calyx(
            "stream pinned Calyx CF snapshot",
            &error,
        ))
    }
}

/// Provenance of one bounded-handoff walk over a column family (#1968).
///
/// Current public walks retain one registered snapshot and one immutable merge
/// cursor, so every non-sentinel result describes one committed sequence.
/// Keeping both sequence fields makes that invariant externally auditable and
/// preserves compatibility with historical moving-window reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxCfWalk {
    /// Column family walked.
    pub column_family: String,
    /// Rows represented by each logical reporting group.
    pub page_rows: usize,
    /// Logical groups traversed; the physical cursor lends one row at a time.
    pub pages: usize,
    /// Logical candidates merged across serving layers, lookaheads included.
    pub rows_examined: usize,
    /// Live rows handed to the visitor.
    pub rows_visited: usize,
    /// Whether the visitor stopped the walk before the column family ended.
    pub stopped_early: bool,
    /// Committed sequence serving the first page.
    pub snapshot_seq_first: Seq,
    /// Committed sequence serving the last page.
    pub snapshot_seq_last: Seq,
}

/// Provenance of a panel-selective Base walk through the hash-sealed
/// `(panel_version, CxId)` membership generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxPanelBaseWalk {
    pub panel_version: u32,
    pub membership_base_seq: Seq,
    /// Snapshot sequence covered by the immutable membership plus its bounded
    /// panel-scoped Base delta.
    pub membership_covered_to_seq: Seq,
    pub snapshot_seq: Seq,
    pub panel_content_seq: Seq,
    pub manifest_sha256: String,
    pub sidecar_sha256: String,
    /// Rows physically named by the immutable sidecar before reconciliation.
    pub sidecar_rows: usize,
    /// Changed Base identities reconciled over that sidecar.
    pub reconciled_changed_keys: usize,
    /// Current-snapshot membership rows after reconciliation.
    pub indexed_rows: usize,
    pub rows_visited: usize,
    pub stopped_early: bool,
    /// Successful heartbeats that preserved this walk's exact snapshot pin.
    pub reader_lease_renewals: usize,
    /// Initial lease expiry before the first membership point read.
    pub reader_lease_initial_expires_at: u64,
    /// Authoritative expiry after the final successful renewal.
    pub reader_lease_final_expires_at: u64,
}

/// Durable bootstrap stream published from one exact panel membership view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxPanelInputSnapshotReport {
    pub panel_version: u32,
    /// Snapshot whose complete membership was captured before chunk commits.
    pub source_snapshot_seq: u64,
    /// Last durable CDC chunk sequence, or `source_snapshot_seq` for an empty
    /// panel.
    pub through_seq: u64,
    pub identities: usize,
    pub chunks: usize,
    /// Every physical Base row examined at the pinned source snapshot. This is
    /// deliberately not a search-sidecar count: recovery must remain possible
    /// when the sidecar delta itself is older than retained MVCC history.
    pub base_rows_scanned: usize,
    /// Ordered digest of the authoritative panel identities published.
    pub membership_sha256: String,
    /// Successful renewals of the same exact reader lease during the Base walk.
    pub reader_lease_renewals: usize,
}

/// Physical retention result for durable association-input CDC already covered
/// by the panel consumer cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxPanelInputPruneReport {
    pub panel_version: u32,
    pub through_seq: u64,
    pub rows_deleted: usize,
    pub committed_seq: Option<u64>,
    pub mutation_floor_seq: u64,
}

impl SynapseCalyxPanelBaseWalk {
    /// Whether membership and every Base point read describe the same pinned
    /// panel state.
    #[must_use]
    pub const fn atomic(&self) -> bool {
        self.membership_base_seq <= self.snapshot_seq
            && self.membership_covered_to_seq == self.snapshot_seq
            && self.panel_content_seq <= self.membership_covered_to_seq
    }
}

impl SynapseCalyxCfWalk {
    /// Whether every page was served by the same committed sequence.
    ///
    /// True means the fold's output describes one instant and is directly
    /// comparable with an unpaged `scan_cf_latest` fold. A physically empty
    /// walk is still atomic; [`Self::not_walked`] is distinguished by its zero
    /// `page_rows` provenance.
    #[must_use]
    pub const fn atomic(&self) -> bool {
        self.page_rows > 0 && self.snapshot_seq_first == self.snapshot_seq_last
    }

    /// A walk record for a value that was **not** folded from a real walk.
    ///
    /// `page_rows == 0` cannot describe a real walk. That makes this an
    /// unambiguous marker even when a real empty column family produces zero
    /// data pages: a hand-assembled census exercising selection logic must not
    /// pass itself off as a physical measurement, and [`Self::atomic`] is false
    /// here for the same reason.
    #[must_use]
    pub fn not_walked(column_family: impl Into<String>) -> Self {
        Self {
            column_family: column_family.into(),
            page_rows: 0,
            pages: 0,
            rows_examined: 0,
            rows_visited: 0,
            stopped_early: false,
            snapshot_seq_first: 0,
            snapshot_seq_last: 0,
        }
    }
}

/// One candidate-bounded page from a single atomic latest Calyx view.
///
/// `examined_rows` counts logical candidates merged across serving layers,
/// including at most one lookahead used to compute `more`; it is not a count
/// of physical SST rows, files, or bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynapseCalyxCfRangePage {
    pub snapshot_seq: Seq,
    pub rows: SynapseCalyxCfRows,
    pub resume_after: Option<Vec<u8>>,
    pub more: bool,
    pub examined_rows: usize,
}

impl From<calyx_aster::mvcc::LatestCfRangePage> for SynapseCalyxCfRangePage {
    fn from(page: calyx_aster::mvcc::LatestCfRangePage) -> Self {
        Self {
            snapshot_seq: page.snapshot_seq,
            rows: page.rows,
            resume_after: page.resume_after,
            more: page.more,
            examined_rows: page.examined_rows,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxPutDisposition {
    Inserted,
    ExistingIdentical,
    ExistingAnchorsMerged { added: usize },
    InBatchDuplicate { anchors_added: usize },
}

impl SynapseCalyxPutDisposition {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::ExistingIdentical => "existing_identical",
            Self::ExistingAnchorsMerged { .. } => "existing_anchors_merged",
            Self::InBatchDuplicate { .. } => "in_batch_duplicate",
        }
    }

    #[must_use]
    pub const fn inserted(self) -> bool {
        matches!(self, Self::Inserted)
    }

    #[must_use]
    pub const fn deduped(self) -> bool {
        !self.inserted()
    }
}

impl From<PutDisposition> for SynapseCalyxPutDisposition {
    fn from(disposition: PutDisposition) -> Self {
        match disposition {
            PutDisposition::Inserted => Self::Inserted,
            PutDisposition::ExistingIdentical => Self::ExistingIdentical,
            PutDisposition::ExistingAnchorsMerged { added } => {
                Self::ExistingAnchorsMerged { added }
            }
            PutDisposition::InBatchDuplicate { anchors_added } => {
                Self::InBatchDuplicate { anchors_added }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxObservationPutReadback {
    pub cx_id: String,
    pub disposition: SynapseCalyxPutDisposition,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxRecurrenceAppendDisposition {
    Inserted,
    ExistingIdentical,
}

impl From<RecurrenceAppendDisposition> for SynapseCalyxRecurrenceAppendDisposition {
    fn from(value: RecurrenceAppendDisposition) -> Self {
        match value {
            RecurrenceAppendDisposition::Inserted => Self::Inserted,
            RecurrenceAppendDisposition::ExistingIdentical => Self::ExistingIdentical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxRecurrenceAppendReadback {
    pub cx_id: String,
    pub occurrence_id: u64,
    pub disposition: SynapseCalyxRecurrenceAppendDisposition,
    pub frequency: u64,
    pub active_occurrences: usize,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxAtomicConstellationRecurrenceReadback {
    pub constellation_cx_id: String,
    pub recurrence_cx_id: String,
    pub occurrence_id: u64,
    pub committed_seq: Seq,
    pub latest_seq: Seq,
    pub source_row_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxRecurrenceRegionAppendReadback {
    pub occurrence: SynapseCalyxRecurrenceAppendReadback,
    pub region: Option<SynapseCalyxPersistedRegionFinding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRecurrenceSeriesReadback {
    pub cx_id: String,
    pub series: RecurrenceSeriesReadback,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxTemporalCandidate {
    pub cx_id: String,
    pub base_score: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxTemporalRankedHit {
    pub cx_id: String,
    pub event_time_secs: i64,
    pub original_rank: usize,
    pub rank: usize,
    pub base_score: f32,
    pub score: f32,
    pub temporal_scores: TemporalScores,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxTemporalRerankReadback {
    pub snapshot_seq: Seq,
    pub panel_name: String,
    pub panel_version: u32,
    pub panel_registered_at_unix_ms: u64,
    pub query_time_secs: i64,
    pub tz_offset_secs: i32,
    pub temporal_lenses: Vec<String>,
    pub policy: TemporalPolicy,
    pub pre_boost_ranking: Vec<String>,
    pub hits: Vec<SynapseCalyxTemporalRankedHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxAnchorWriteReadback {
    pub cx_id: String,
    pub anchor_count: usize,
    pub ledger_seq: Seq,
    pub ledger_hash: String,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxAnchorBatchWriteReadback {
    pub anchor_count: usize,
    pub written_anchor_count: usize,
    pub existing_anchor_count: usize,
    pub ledger_seq: Option<Seq>,
    pub ledger_hash: Option<String>,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxGroundedObservationReadback {
    pub cx_id: String,
    pub disposition: SynapseCalyxPutDisposition,
    pub ledger_seq: Seq,
    pub ledger_hash: String,
    pub source_row_count: usize,
    pub committed_seq: Seq,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxGroundedObservationBatchReadback {
    pub cx_ids: Vec<String>,
    pub ledger_seq: Seq,
    pub ledger_hash: String,
    pub source_row_count: usize,
    pub committed_seq: Seq,
    pub latest_seq: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxNativeFanoutReadback {
    pub attempted_cfs: usize,
    pub compacted_cfs: usize,
    pub skipped_cfs: usize,
    pub reclaimed_input_files: usize,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub compacted_cf_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxAnchorReadback {
    pub key: Vec<u8>,
    pub anchor: Anchor,
}

/// Fail-closed verdict of a live provenance-ledger hash-chain verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxLedgerVerifyReport {
    /// True only when the requested range re-walked and re-hashed intact.
    pub intact: bool,
    /// Stable verdict label: `intact` | `broken` | `corrupt`.
    pub verdict: String,
    /// Durable ledger head height (total entry count) at verify time.
    pub head_height: u64,
    /// Inclusive-exclusive sequence window that was verified.
    pub verified_from_seq: u64,
    pub verified_to_seq: u64,
    /// Number of entries confirmed intact (0 for a broken/corrupt verdict).
    pub entry_count: u64,
    /// Durable ledger tip hash, when a head anchor exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip_hash: Option<String>,
    /// The sequence that must be quarantined on a broken/corrupt verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantine_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken_expected_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken_found_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrupt_reason: Option<String>,
    /// True only when every periodic raw-write Merkle seal matches the
    /// append-only commitment rows retained in the physical commitment CF.
    pub raw_commitments_intact: bool,
    pub raw_commitment_seal_count: u64,
    pub raw_commitment_count: u64,
    pub raw_commitment_sealed_count: u64,
    /// Atomically committed raw batches awaiting the next periodic seal.
    pub raw_commitment_pending_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_commitment_coverage_from_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_commitment_sealed_through_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_commitment_first_pending_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_commitment_failure: Option<String>,
    /// Digest an operator must present to adjudicate `raw_commitment_failure`.
    ///
    /// Published so the guard for an irreversible governance write is read
    /// straight off the readback that reported the damage, rather than being
    /// recomputed by hand from a diagnostic that must match byte for byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_commitment_failure_sha256: Option<String>,
    /// Cohort seals an operator has recorded as permanently unverifiable.
    ///
    /// Never counted as verified. While this is non-zero the vault can never
    /// report `verified`, but the damage it names is reviewed and recorded
    /// rather than newly discovered.
    pub raw_commitment_adjudicated_count: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub raw_commitment_adjudicated_exceptions: Vec<String>,
    /// Bounded stall window for the exact-snapshot integrity scan.
    pub reader_lease_duration_ms: u64,
    /// Successful same-snapshot renewals across physical Ledger and raw
    /// commitment streams.
    pub reader_lease_renewal_count: u64,
    /// True only when the verified range provably covers the vault directory's
    /// whole recorded history. False whenever the chain begins after a recorded
    /// vault replacement, or before the lineage journal existed — in which case
    /// `intact` attests the surviving chain, not the full history (#1875).
    pub covers_full_history: bool,
    /// `vault-genesis` | `lineage-seeded` | `post-reset`.
    pub chain_origin: String,
    /// Why coverage is what it is (#1884): `full-from-genesis`,
    /// `partial-journal-seeded-over-pre-existing-vault`,
    /// `partial-begins-after-acknowledged-vault-replacement`. A partial verdict
    /// is a statement about the chain's *start*, not about its integrity —
    /// `intact` is the integrity answer and they are reported separately.
    pub history_coverage: String,
    /// Durable sequence, **in this generation's own numbering**, from which the
    /// lineage journal attests the chain — the `latest_seq` observed when this
    /// generation was recorded.
    ///
    /// It is 0 for a genesis vault and also 0 for a replacement vault whose
    /// bytes were recreated empty, so it is not a proxy for coverage: read
    /// `history_coverage` for that. What a `post-reset` generation does *not*
    /// attest is the predecessor's history, whose extent is
    /// `predecessor_high_water_seq`, not this field.
    pub attested_from_seq: u64,
    /// 1-based lineage generation of the vault this chain lives in.
    pub vault_generation: u64,
    /// Recorded vault replacements preceding this generation.
    pub vault_reset_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_vault_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_high_water_seq: Option<u64>,
}

/// Default incremental window for a scheduled vault verification: the most
/// recent 4096 ledger entries. Routine checks must not re-hash the whole Ledger
/// CF, or they stop being run at all.
pub const VAULT_VERIFY_DEFAULT_TAIL_ENTRIES: u64 = 4_096;

/// Combined verdict of one scheduled whole-vault verification.
#[derive(Debug, Clone, Serialize)]
pub struct SynapseCalyxVaultVerifyReport {
    pub vault_dir: PathBuf,
    pub vault_id: String,
    /// `incremental_tail` | `full_chain`.
    pub scan_mode: String,
    /// Durable ledger head height at verify time.
    pub ledger_head_height: u64,
    /// Size of the incremental window requested (0 for a full-chain scan).
    pub requested_tail_entries: u64,
    /// Path of the lineage journal that proves vault identity.
    pub lineage_path: PathBuf,
    /// False when the journal that survives deleting the vault is itself gone —
    /// the next open would silently re-seed instead of failing closed (#1875).
    pub lineage_present: bool,
    pub restore: SynapseCalyxVerifyReport,
    pub chain: SynapseCalyxLedgerVerifyReport,
}

/// What one scheduled vault verification actually established (#2059).
///
/// Three outcomes, not two. Before this existed, "the scan was refused by a
/// resource budget" and "the hash chain is broken" were the same non-green
/// verdict wearing the same restore-from-backup remediation, so the alarm that
/// must be believed fired routinely on a vault that was provably intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxVaultVerifyVerdict {
    /// Every checked surface verified.
    Verified,
    /// No integrity evidence failed, and at least one scan could not run to
    /// completion. The vault is unverified — not damaged.
    Unverifiable,
    /// Integrity evidence failed. This is the corruption alarm.
    Corrupt,
}

impl SynapseCalyxVaultVerifyVerdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverifiable => "unverifiable",
            Self::Corrupt => "corrupt",
        }
    }
}

impl SynapseCalyxLedgerVerifyReport {
    /// True when the raw-commitment walk carries only recorded, reviewed damage.
    ///
    /// This is the raw-commitment analogue of `restore.unverifiable_only()`: no
    /// seal failed unexpectedly, and at least one cohort is one an operator has
    /// already adjudicated as permanently unverifiable. Such a vault is not
    /// verified - it can never be, while the exception stands - but it is also
    /// not newly damaged, and the corruption alarm belongs to damage.
    #[must_use]
    pub const fn raw_commitments_adjudicated_only(&self) -> bool {
        self.raw_commitment_failure.is_none() && self.raw_commitment_adjudicated_count > 0
    }
}

impl SynapseCalyxVaultVerifyReport {
    /// Classifies the verdict from the evidence, fail-closed on integrity.
    ///
    /// `Corrupt` wins over `Unverifiable` whenever any integrity predicate is
    /// false, because a vault can be both damaged and too large for one of its
    /// scans to finish, and the damage is what an operator must act on.
    #[must_use]
    pub const fn verdict(&self) -> SynapseCalyxVaultVerifyVerdict {
        if self.green() {
            return SynapseCalyxVaultVerifyVerdict::Verified;
        }
        let integrity_failed = !self.chain.intact
            || (!self.chain.raw_commitments_intact
                && !self.chain.raw_commitments_adjudicated_only())
            || !self.lineage_present
            || (!self.restore.success && !self.restore.unverifiable_only());
        if integrity_failed {
            return SynapseCalyxVaultVerifyVerdict::Corrupt;
        }
        SynapseCalyxVaultVerifyVerdict::Unverifiable
    }

    /// Names every scan that was refused rather than answered.
    #[must_use]
    pub fn unverifiable_reasons(&self) -> Vec<String> {
        let mut reasons = self
            .restore
            .unverifiable_reason
            .iter()
            .map(|reason| format!("restore_verify: {reason}"))
            .collect::<Vec<_>>();
        for exception in &self.chain.raw_commitment_adjudicated_exceptions {
            reasons.push(format!("raw_commitments: adjudicated {exception}"));
        }
        reasons
    }

    /// Fraction of the durable ledger the chain re-walk actually covered.
    ///
    /// A verdict without its coverage is unreadable: `intact` over 4,096 of
    /// 1,056,804 entries and `intact` over all of them are different facts.
    #[must_use]
    pub fn chain_coverage_fraction(&self) -> f64 {
        if self.ledger_head_height == 0 {
            return 1.0;
        }
        let verified = self
            .chain
            .verified_to_seq
            .saturating_sub(self.chain.verified_from_seq);
        #[expect(
            clippy::cast_precision_loss,
            reason = "a coverage ratio is reported to four decimals; u64 precision is not meaningful here"
        )]
        {
            (verified as f64 / self.ledger_head_height as f64).clamp(0.0, 1.0)
        }
    }
    /// True only when every checked surface verified.
    ///
    /// `covers_full_history` is deliberately **not** part of this predicate, and
    /// after #1884 that exclusion is a real distinction rather than a
    /// workaround for a constant. Integrity ("every hash in the verified range
    /// links") and coverage ("where the attested range starts") are different
    /// questions. A vault whose journal was seeded over pre-existing data, or
    /// one continuing after an acknowledged replacement, has a chain that is
    /// genuinely intact and genuinely starts later than sequence 0; folding the
    /// second fact into a red verdict would report an intact chain as broken
    /// forever. Coverage is reported instead as `history_coverage` plus
    /// `attested_from_seq`, which name the start and the reason for it.
    #[must_use]
    pub const fn green(&self) -> bool {
        self.restore.success
            && self.chain.intact
            && self.chain.raw_commitments_intact
            && self.lineage_present
    }

    /// Names every unmet criterion, so a failure says exactly what failed.
    #[must_use]
    pub fn failure_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        for reason in &self.restore.failure_reasons {
            reasons.push(format!("restore_verify: {reason}"));
        }
        if !self.chain.intact {
            reasons.push(format!(
                "ledger_chain: verdict={} verified=[{}..{}) quarantine_seq={} corrupt_reason={}",
                self.chain.verdict,
                self.chain.verified_from_seq,
                self.chain.verified_to_seq,
                self.chain
                    .quarantine_seq
                    .map_or_else(|| "none".to_owned(), |seq| seq.to_string()),
                self.chain.corrupt_reason.as_deref().unwrap_or("none")
            ));
        }
        if !self.chain.raw_commitments_intact && !self.chain.raw_commitments_adjudicated_only() {
            reasons.push(format!(
                "raw_commitments: seals={} commitments={} sealed={} pending={} failure={}",
                self.chain.raw_commitment_seal_count,
                self.chain.raw_commitment_count,
                self.chain.raw_commitment_sealed_count,
                self.chain.raw_commitment_pending_count,
                self.chain
                    .raw_commitment_failure
                    .as_deref()
                    .unwrap_or("none")
            ));
        }
        if !self.lineage_present {
            reasons.push(format!(
                "vault_lineage: journal {} is missing, so a replaced vault would no longer fail \
                 closed on the next open",
                self.lineage_path.display()
            ));
        }
        reasons
    }
}

impl SynapseCalyxLedgerVerifyReport {
    fn from_aster(
        verification: calyx_aster::vault::AsterLedgerChainVerification,
        lineage: &SynapseCalyxVaultLineage,
    ) -> Self {
        let calyx_aster::vault::AsterLedgerChainVerification {
            result,
            head_height,
            tip_hash,
            verified_range,
            raw_commitments,
            reader_lease_duration_ms,
            reader_lease_renewal_count,
        } = verification;
        let raw_commitments_intact = raw_commitments.intact;
        let base = Self {
            intact: false,
            verdict: String::new(),
            head_height,
            verified_from_seq: verified_range.start,
            verified_to_seq: verified_range.end,
            entry_count: 0,
            tip_hash: tip_hash.map(|hash| hex_bytes(&hash)),
            quarantine_seq: None,
            broken_expected_hash: None,
            broken_found_hash: None,
            corrupt_reason: None,
            raw_commitments_intact,
            raw_commitment_seal_count: raw_commitments.seal_count,
            raw_commitment_count: raw_commitments.commitment_count,
            raw_commitment_sealed_count: raw_commitments.sealed_commitment_count,
            raw_commitment_pending_count: raw_commitments.pending_commitment_count,
            raw_commitment_coverage_from_seq: raw_commitments.coverage_from_seq,
            raw_commitment_sealed_through_seq: raw_commitments.sealed_through_seq,
            raw_commitment_first_pending_seq: raw_commitments.first_pending_seq,
            raw_commitment_failure: raw_commitments.failure.clone(),
            raw_commitment_failure_sha256: raw_commitments.failure_adjudication_sha256.clone(),
            raw_commitment_adjudicated_count: raw_commitments.adjudicated_exception_count,
            raw_commitment_adjudicated_exceptions: raw_commitments.adjudicated_exceptions.clone(),
            reader_lease_duration_ms,
            reader_lease_renewal_count,
            covers_full_history: lineage.chain_covers_full_history(),
            chain_origin: lineage.chain_origin.clone(),
            history_coverage: lineage.history_coverage().to_owned(),
            attested_from_seq: lineage.generation_origin_seq,
            vault_generation: lineage.generation,
            vault_reset_count: lineage.reset_count,
            predecessor_vault_id: lineage.predecessor_vault_id.clone(),
            predecessor_high_water_seq: lineage.predecessor_high_water_seq,
        };
        let mut report = match result {
            VerifyResult::Intact { count } => Self {
                intact: true,
                verdict: "intact".to_owned(),
                entry_count: count,
                ..base
            },
            VerifyResult::Broken {
                at_seq,
                expected,
                found,
            } => Self {
                verdict: "broken".to_owned(),
                quarantine_seq: Some(at_seq),
                broken_expected_hash: Some(hex_bytes(&expected)),
                broken_found_hash: Some(hex_bytes(&found)),
                ..base
            },
            VerifyResult::Corrupt { at_seq, reason } => Self {
                verdict: "corrupt".to_owned(),
                quarantine_seq: Some(at_seq),
                corrupt_reason: Some(reason),
                ..base
            },
        };
        if report.intact && !raw_commitments_intact {
            // A raw-commitment failure is a chain-level corruption verdict.
            // Cohorts an operator has adjudicated are not a failure: the hash
            // chain re-walked intact, and those cohorts are unverified rather
            // than damaged. `raw_commitments_intact` stays false either way, so
            // `green()` still refuses to call such a vault verified.
            if let Some(failure) = raw_commitments.failure {
                report.intact = false;
                "corrupt".clone_into(&mut report.verdict);
                report.corrupt_reason = Some(failure);
            }
        }
        report
    }
}

/// Physical receipt for one appended raw-commitment seal adjudication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxSealAdjudicationReceipt {
    /// The damaged cohort seal this exception covers.
    pub adjudicated_ledger_seq: u64,
    /// The byte-exact diagnostic the exception is bound to.
    pub diagnostic: String,
    pub diagnostic_sha256: String,
    pub reason: String,
    /// Ledger sequence of the appended `Admin` governance entry.
    pub adjudication_ledger_seq: u64,
    pub adjudication_entry_hash: String,
}

/// Decoded readback of one physical provenance-ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxLedgerEntryReadback {
    pub seq: u64,
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_verifies: Option<bool>,
}

impl SynapseCalyxLedgerEntryReadback {
    const fn absent(seq: u64) -> Self {
        Self {
            seq,
            present: false,
            kind: None,
            subject: None,
            actor: None,
            ts: None,
            prev_hash: None,
            entry_hash: None,
            payload_len: None,
            payload_sha256: None,
            self_verifies: None,
        }
    }

    fn from_entry(seq: u64, entry: &LedgerEntry) -> Self {
        Self {
            seq,
            present: true,
            kind: Some(entry.kind.as_str().to_owned()),
            subject: Some(subject_metadata_value(&entry.subject)),
            actor: Some(format!("{:?}", entry.actor)),
            ts: Some(entry.ts),
            prev_hash: Some(hex_bytes(&entry.prev_hash)),
            entry_hash: Some(hex_bytes(&entry.entry_hash)),
            payload_len: Some(entry.payload.len() as u64),
            payload_sha256: Some(sha256_hex(&entry.payload)),
            self_verifies: Some(entry.verify()),
        }
    }
}

/// Re-derivation verdict for a record's recorded provenance binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SynapseCalyxReproduceReport {
    pub cx_id: String,
    pub reproduced: bool,
    pub recorded_seq: u64,
    pub recorded_hash: String,
    pub input_hash: String,
    pub entry_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_hash: Option<String>,
    pub entry_self_verifies: bool,
    /// Literal `SubjectId::Cx` equality. Batch members legitimately leave this false.
    pub subject_matches: bool,
    /// Stable shared Base-row binding mode (`subject`, `enumerated_member`,
    /// `batch_member`, `batch_scope`, or `entry_absent`).
    pub coverage: String,
    /// Whether the referenced entry covers this record under the shared contract.
    pub coverage_matches: bool,
    /// `none` when the record reproduces, else a fail-closed drift reason.
    pub drift: String,
}

impl SynapseCalyxReproduceReport {
    fn from_aster(reproduction: &calyx_aster::vault::AsterProvenanceReproduction) -> Self {
        let drift = if reproduction.reproduced {
            "none".to_owned()
        } else if !reproduction.entry_present {
            format!(
                "recorded provenance seq {} has no physical ledger entry",
                reproduction.recorded_seq
            )
        } else if !reproduction.entry_self_verifies {
            format!(
                "ledger entry at seq {} does not re-hash to its stored hash",
                reproduction.recorded_seq
            )
        } else if !reproduction.coverage_matches {
            format!(
                "ledger entry at seq {} does not bind back to this record",
                reproduction.recorded_seq
            )
        } else {
            format!(
                "ledger entry hash does not match the record's recorded provenance hash at seq {}",
                reproduction.recorded_seq
            )
        };
        Self {
            cx_id: reproduction.cx_id.to_string(),
            reproduced: reproduction.reproduced,
            recorded_seq: reproduction.recorded_seq,
            recorded_hash: hex_bytes(&reproduction.recorded_hash),
            input_hash: hex_bytes(&reproduction.input_hash),
            entry_present: reproduction.entry_present,
            entry_hash: reproduction.entry_hash.map(|hash| hex_bytes(&hash)),
            entry_self_verifies: reproduction.entry_self_verifies,
            subject_matches: reproduction.subject_matches,
            coverage: reproduction
                .coverage
                .map_or("entry_absent", calyx_ledger::CxCoverage::tag)
                .to_owned(),
            coverage_matches: reproduction.coverage_matches,
            drift,
        }
    }
}

/// Readback of a lawful, ledger-stamped erasure plus a post-erase re-verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxErasureReport {
    pub scope: String,
    pub records_deleted: usize,
    pub shredded_at_ms: u64,
    pub tombstone_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone_hash: Option<String>,
    /// Full hash-chain re-verification after the erasure entry was appended.
    pub chain_verify: SynapseCalyxLedgerVerifyReport,
}

const SYNAPSE_DIR_NAME: &str = "synapse";
const VAULT_DIR_NAME: &str = "vault";
const IDENTITY_FILE_NAME: &str = "vault-identity.json";
const MACHINE_SALT_FILE_NAME: &str = "machine-salt.bin";
const LOCK_FILE_NAME: &str = "vault.lock";
const PID_FILE_NAME: &str = "vault.pid";
const SYNAPSE_PANEL_NAME_METADATA: &str = "synapse_panel_name";
const IDENTITY_SCHEMA_VERSION: u32 = 1;
const MACHINE_SALT_BYTES: usize = 32;

const APPDATA_MISSING_REMEDIATION: &str =
    "set APPDATA or configure SYNAPSE_CALYX_VAULT_DIR to an explicit durable directory";
const IDENTITY_REMEDIATION: &str =
    "restore the vault identity files from backup or inspect the exact file named in the error";
const LOCK_REMEDIATION: &str =
    "stop the process holding the vault lock or point Synapse at a different vault directory";
const OPEN_REMEDIATION: &str =
    "inspect the vault directory, recovery report, and Calyx error; repair storage before restart";
const CLOSE_REMEDIATION: &str = "inspect the vault directory and shutdown logs; do not start a successor until the lock and PID readback are clean";
const CONFIG_REMEDIATION: &str =
    "fix the [calyx] configuration file or unset SYNAPSE_CALYX_CONFIG to use handbook defaults";
/// Versioned maximum for the setup-owned Calyx tuning document.
///
/// The file is read through a single handle into at most this many bytes plus
/// one sentinel byte. Hashing and TOML parsing consume that exact bounded
/// buffer, so neither size checks nor identity checks introduce a second-open
/// race.
pub const SYNAPSE_CALYX_CONFIG_MAX_BYTES_V1: u64 = 64 * 1024;
const SYNAPSE_CALYX_CONFIG_READ_BYTES_V1: usize = 64 * 1024 + 1;

const DEFAULT_GUARD_FAR_IDENTITY: f32 = 0.01;
const DEFAULT_GUARD_FAR_CONTENT: f32 = 0.03;
const DEFAULT_GUARD_FAR_STYLISTIC: f32 = 0.05;
/// The vault's untuned `fusion_k`. Bound to the single workspace declaration
/// rather than restated, so this cannot drift from what actually scores (#1883).
const DEFAULT_FUSION_K: u32 = calyx_core::RRF_K_DEFAULT;
const DEFAULT_INDEX_M_MAX: usize = 32;
const DEFAULT_INDEX_EF_CONSTRUCTION: usize = 64;
const DEFAULT_INDEX_BEAMWIDTH: usize = 32;
const DEFAULT_INDEX_EF_SEARCH: usize = 64;
const DEFAULT_INDEX_ALPHA: f32 = 1.2;
/// CPU-only is the shipped default. A zero budget is an explicit assertion
/// that no Calyx caller may acquire a CUDA context or reserve device memory.
const DEFAULT_VRAM_BUDGET_BYTES: u64 = 0;
const DEFAULT_RNG_SEED: u64 = 0x5A17_5EED_CA1A_1696;

/// Compile-time accelerator closure exported for executable policy guards.
///
/// The installed daemon asserts this is false even if a caller attempts to
/// inject a namespaced dependency feature outside its closed feature surface.
pub const SYNAPSE_CALYX_CUDA_COMPILED: bool = calyx_forge::CUDA_COMPILED;

/// Fixes the process SST writer format before any vault is opened.
///
/// Setup uses v2 only for a pre-commit upgrade generation whose predecessor
/// cannot read v3; committed current generations use v3 compression.
///
/// # Errors
///
/// Returns a Calyx error if the writer format has already been fixed for this
/// process, or if the requested version is not a supported SST format.
pub fn configure_sst_write_version(version: u32) -> Result<(), SynapseCalyxError> {
    calyx_aster::sst::configure_sst_write_version(version)
        .map_err(|error| SynapseCalyxError::from_calyx("configure SST write version", &error))
}

/// Fixes whether this process may publish a migrated Anneal live pointer.
///
/// Pre-commit v2 generations preserve the authenticated predecessor-readable
/// pointer while still using the validated CPU-only effective policy in memory.
///
/// # Errors
///
/// Returns an invalid-config error if the policy was already configured for
/// this process.
pub fn configure_anneal_legacy_pointer_preservation(
    preserve: bool,
) -> Result<(), SynapseCalyxError> {
    anneal::configure_legacy_pointer_preservation(preserve)
}

/// Atomically converts pre-commit v3 SST output back to the v2 format accepted
/// by a rollback predecessor, preserving and rereading every durable row.
///
/// # Errors
///
/// Returns a Calyx error if the vault root cannot be read, if any SST cannot be
/// rewritten atomically, or if a rewritten file fails its readback check.
pub fn downgrade_v3_ssts_to_v2(
    vault_root: impl AsRef<std::path::Path>,
) -> Result<(u64, u64, u64), SynapseCalyxError> {
    calyx_aster::sst::downgrade_v3_ssts_to_v2(vault_root).map_err(|error| {
        SynapseCalyxError::from_calyx("downgrade Calyx SST v3 files to v2", &error)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxMathBackend {
    Auto,
    Cpu,
    Cuda,
}

impl SynapseCalyxMathBackend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxClockMode {
    System,
    Fixed,
}

impl SynapseCalyxClockMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Fixed => "fixed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynapseCalyxTuningConfig {
    pub guard_far_identity: f32,
    pub guard_far_content: f32,
    pub guard_far_stylistic: f32,
    pub fusion_k: u32,
    pub fusion_slot_weights: BTreeMap<u16, f32>,
    pub index_m_max: usize,
    pub index_ef_construction: usize,
    pub index_beamwidth: usize,
    pub index_ef_search: usize,
    pub index_alpha: f32,
    pub index_quant_bits_by_slot: BTreeMap<u16, u8>,
    pub vram_budget_bytes: u64,
    pub math_backend: SynapseCalyxMathBackend,
    pub clock_mode: SynapseCalyxClockMode,
    pub fixed_clock_unix_ms: Option<Ts>,
    pub rng_seed: u64,
}

impl Default for SynapseCalyxTuningConfig {
    fn default() -> Self {
        Self {
            guard_far_identity: DEFAULT_GUARD_FAR_IDENTITY,
            guard_far_content: DEFAULT_GUARD_FAR_CONTENT,
            guard_far_stylistic: DEFAULT_GUARD_FAR_STYLISTIC,
            fusion_k: DEFAULT_FUSION_K,
            fusion_slot_weights: BTreeMap::new(),
            index_m_max: DEFAULT_INDEX_M_MAX,
            index_ef_construction: DEFAULT_INDEX_EF_CONSTRUCTION,
            index_beamwidth: DEFAULT_INDEX_BEAMWIDTH,
            index_ef_search: DEFAULT_INDEX_EF_SEARCH,
            index_alpha: DEFAULT_INDEX_ALPHA,
            index_quant_bits_by_slot: BTreeMap::new(),
            vram_budget_bytes: DEFAULT_VRAM_BUDGET_BYTES,
            math_backend: SynapseCalyxMathBackend::Cpu,
            clock_mode: SynapseCalyxClockMode::System,
            fixed_clock_unix_ms: None,
            rng_seed: DEFAULT_RNG_SEED,
        }
    }
}

impl SynapseCalyxTuningConfig {
    /// Validates every exposed Calyx tuning knob before daemon startup.
    ///
    /// # Errors
    ///
    /// Returns a structured error when any value is non-finite, outside the
    /// accepted range, or when clock settings contradict each other.
    pub fn validate(self) -> Result<Self, SynapseCalyxError> {
        validate_f32(
            "guard_far_identity",
            self.guard_far_identity,
            0.0,
            DEFAULT_GUARD_FAR_IDENTITY,
        )?;
        self.dense_index_config()
            .validate()
            .map_err(|error| invalid_config(format!("persisted dense index tuning: {error}")))?;
        validate_f32(
            "guard_far_content",
            self.guard_far_content,
            0.0,
            DEFAULT_GUARD_FAR_CONTENT,
        )?;
        validate_f32(
            "guard_far_stylistic",
            self.guard_far_stylistic,
            0.0,
            DEFAULT_GUARD_FAR_STYLISTIC,
        )?;
        if self.fusion_k == 0 {
            return Err(invalid_config("fusion_k must be positive"));
        }
        if self
            .fusion_slot_weights
            .values()
            .all(|weight| *weight == 0.0)
            && !self.fusion_slot_weights.is_empty()
        {
            return Err(invalid_config(
                "fusion_slot_weights must contain at least one positive weight",
            ));
        }
        for (slot, weight) in &self.fusion_slot_weights {
            validate_f32(
                &format!("fusion_slot_weights[{slot}]"),
                *weight,
                0.0,
                f32::INFINITY,
            )?;
        }
        if self.math_backend != SynapseCalyxMathBackend::Cpu || self.vram_budget_bytes != 0 {
            return Err(invalid_config(format!(
                "the installed Synapse Calyx runtime requires math_backend = \"cpu\" and vram_budget_bytes = 0; got math_backend = {:?}, vram_budget_bytes = {}; accelerator/auto selection and nonzero device budgets are forbidden",
                self.math_backend, self.vram_budget_bytes
            )));
        }
        match (self.clock_mode, self.fixed_clock_unix_ms) {
            (SynapseCalyxClockMode::System, Some(_)) => {
                return Err(invalid_config(
                    "fixed_clock_unix_ms is only valid when clock_mode = \"fixed\"",
                ));
            }
            (SynapseCalyxClockMode::Fixed, None) => {
                return Err(invalid_config(
                    "clock_mode = \"fixed\" requires fixed_clock_unix_ms",
                ));
            }
            (SynapseCalyxClockMode::System, None) | (SynapseCalyxClockMode::Fixed, Some(_)) => {}
        }
        Ok(self)
    }

    fn dense_index_config(&self) -> PersistedDenseIndexConfig {
        PersistedDenseIndexConfig {
            m_max: self.index_m_max,
            ef_construction: self.index_ef_construction,
            beamwidth: self.index_beamwidth,
            ef_search: self.index_ef_search,
            alpha: self.index_alpha,
            quant_bits_by_slot: self.index_quant_bits_by_slot.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynapseCalyxClock {
    System,
    Fixed(Ts),
}

impl SynapseCalyxClock {
    pub(crate) fn from_tuning(
        config: &SynapseCalyxTuningConfig,
    ) -> Result<Self, SynapseCalyxError> {
        match config.clock_mode {
            SynapseCalyxClockMode::System => Ok(Self::System),
            SynapseCalyxClockMode::Fixed => {
                config.fixed_clock_unix_ms.map(Self::Fixed).ok_or_else(|| {
                    invalid_config("clock_mode = \"fixed\" requires fixed_clock_unix_ms")
                })
            }
        }
    }
}

impl Clock for SynapseCalyxClock {
    fn now(&self) -> Ts {
        match self {
            Self::System => SystemClock.now(),
            Self::Fixed(ts) => *ts,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SynapseCalyxConfigFile {
    calyx: SynapseCalyxTuningConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynapseCalyxConfig {
    pub vault_dir: PathBuf,
    pub machine_salt_path: PathBuf,
    pub tuning: SynapseCalyxTuningConfig,
}

impl SynapseCalyxConfig {
    /// Resolves the default roaming Synapse Calyx paths.
    ///
    /// This deliberately errors when `APPDATA` is absent. A transient temp-dir
    /// fallback would create an unannounced second vault, which is worse than a
    /// startup failure for durable state.
    ///
    /// # Errors
    ///
    /// Returns an error when `APPDATA` is absent.
    pub fn from_default_roaming() -> Result<Self, SynapseCalyxError> {
        let data_dir = roaming_synapse_dir()?;
        let tuning = SynapseCalyxTuningConfig::default().validate()?;
        Self::from_paths_with_tuning(
            data_dir.join(VAULT_DIR_NAME),
            data_dir.join(MACHINE_SALT_FILE_NAME),
            tuning,
        )
    }

    #[must_use]
    pub fn from_vault_dir(vault_dir: PathBuf) -> Self {
        Self::from_vault_dir_with_tuning(vault_dir, SynapseCalyxTuningConfig::default())
    }

    #[must_use]
    fn from_vault_dir_with_tuning(vault_dir: PathBuf, tuning: SynapseCalyxTuningConfig) -> Self {
        let salt_parent = vault_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        Self {
            vault_dir,
            machine_salt_path: salt_parent.join(MACHINE_SALT_FILE_NAME),
            tuning,
        }
    }

    /// Resolves the configured vault directory or the default roaming path.
    ///
    /// # Errors
    ///
    /// Returns an error when no explicit path is supplied and the default
    /// roaming path cannot be resolved, or when the explicit path is empty.
    pub fn from_optional_vault_dir(vault_dir: Option<PathBuf>) -> Result<Self, SynapseCalyxError> {
        Self::from_optional_vault_dir_and_config_path(vault_dir, None)
    }

    /// Resolves the configured vault directory and optional `[calyx]` config.
    ///
    /// # Errors
    ///
    /// Returns an error when the path/config is invalid or when the Calyx
    /// error-code bridge has drifted from the upstream catalog.
    pub fn from_optional_vault_dir_and_config_path(
        vault_dir: Option<PathBuf>,
        config_path: Option<PathBuf>,
    ) -> Result<Self, SynapseCalyxError> {
        Self::from_optional_vault_dir_and_config_path_with_expected_sha256(
            vault_dir,
            config_path,
            None,
        )
    }

    /// Resolves the configured vault directory and parses the exact config
    /// bytes whose SHA-256 identity was supplied by the launcher.
    ///
    /// The hash and TOML parse consume one in-memory byte buffer, closing the
    /// check/use race that exists when a launcher hashes a path before the
    /// daemon opens it independently.
    /// Opens the configured vault after bounded, hash-pinned config decoding.
    ///
    /// # Errors
    ///
    /// Returns a structured error for an absent/mismatched config hash,
    /// oversized or malformed config bytes, invalid resource policy, or vault
    /// lifecycle failure.
    pub fn from_optional_vault_dir_and_config_path_with_expected_sha256(
        vault_dir: Option<PathBuf>,
        config_path: Option<PathBuf>,
        expected_config_sha256: Option<&str>,
    ) -> Result<Self, SynapseCalyxError> {
        error_bridge::validate_calyx_error_bridge()?;
        let tuning = match config_path {
            Some(path) => read_tuning_config(&path, expected_config_sha256)?,
            None if expected_config_sha256.is_none() => {
                SynapseCalyxTuningConfig::default().validate()?
            }
            None => {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_CONFIG_IDENTITY_WITHOUT_PATH",
                    "an expected Calyx config SHA-256 was supplied without a config path",
                    "supply both --calyx-config and --calyx-config-sha256, or omit both",
                ));
            }
        };
        match vault_dir {
            Some(path) if path.as_os_str().is_empty() => Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_VAULT_DIR_EMPTY",
                "configured Calyx vault directory is empty",
                "set SYNAPSE_CALYX_VAULT_DIR to an absolute durable path or unset it for the default APPDATA path",
            )),
            Some(path) => Ok(Self::from_vault_dir_with_tuning(path, tuning)),
            None => {
                let data_dir = roaming_synapse_dir()?;
                Self::from_paths_with_tuning(
                    data_dir.join(VAULT_DIR_NAME),
                    data_dir.join(MACHINE_SALT_FILE_NAME),
                    tuning,
                )
            }
        }
    }

    fn from_paths_with_tuning(
        vault_dir: PathBuf,
        machine_salt_path: PathBuf,
        tuning: SynapseCalyxTuningConfig,
    ) -> Result<Self, SynapseCalyxError> {
        error_bridge::validate_calyx_error_bridge()?;
        Ok(Self {
            vault_dir,
            machine_salt_path,
            tuning: tuning.validate()?,
        })
    }
}

fn read_tuning_config(
    path: &Path,
    expected_sha256: Option<&str>,
) -> Result<SynapseCalyxTuningConfig, SynapseCalyxError> {
    let file = File::open(path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_CONFIG_READ_FAILED",
            "open Calyx config",
            path,
            &error,
            CONFIG_REMEDIATION,
        )
    })?;
    let mut bytes = Vec::with_capacity(SYNAPSE_CALYX_CONFIG_READ_BYTES_V1);
    file.take(SYNAPSE_CALYX_CONFIG_MAX_BYTES_V1 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_CONFIG_READ_FAILED",
                "read bounded Calyx config",
                path,
                &error,
                CONFIG_REMEDIATION,
            )
        })?;
    if bytes.len() == SYNAPSE_CALYX_CONFIG_READ_BYTES_V1 {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_CONFIG_TOO_LARGE",
            format!(
                "Calyx config {} exceeds the v1 limit of {} bytes (read at least {} bytes from one open handle)",
                path.display(),
                SYNAPSE_CALYX_CONFIG_MAX_BYTES_V1,
                bytes.len()
            ),
            "replace the setup-owned tuning document with a canonical [calyx] config no larger than 65536 bytes",
        ));
    }
    if let Some(expected) = expected_sha256 {
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CONFIG_EXPECTED_SHA256_INVALID",
                format!(
                    "expected Calyx config SHA-256 is not 64 hexadecimal characters: {expected:?}"
                ),
                "pass the exact 64-hex SHA-256 emitted by setup candidate validation",
            ));
        }
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CONFIG_SHA256_MISMATCH",
                format!(
                    "Calyx config {} expected SHA-256 {} but the single read buffer hashes to {}",
                    path.display(),
                    expected.to_ascii_uppercase(),
                    actual
                ),
                "stop the concurrent config writer and relaunch through setup with one candidate-validated config identity",
            ));
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_CONFIG_UTF8_INVALID",
            format!("decode Calyx config {} as UTF-8: {error}", path.display()),
            CONFIG_REMEDIATION,
        )
    })?;
    let file: SynapseCalyxConfigFile = toml::from_str(text).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_CONFIG_PARSE_FAILED",
            format!("parse Calyx config {}: {error}", path.display()),
            CONFIG_REMEDIATION,
        )
    })?;
    tracing::info!(
        code = "SYNAPSE_CALYX_CONFIG_DECODED",
        config_path = %path.display(),
        config_length = bytes.len(),
        config_sha256 = %sha256_hex(&bytes),
        math_backend = file.calyx.math_backend.as_str(),
        vram_budget_bytes = file.calyx.vram_budget_bytes,
        "decoded the hash-pinned Calyx tuning document before policy validation"
    );
    file.calyx.validate()
}

fn validate_f32(name: &str, value: f32, min: f32, max: f32) -> Result<(), SynapseCalyxError> {
    if value.is_finite() && value >= min && value <= max {
        return Ok(());
    }
    Err(invalid_config(format!(
        "{name} must be finite and in [{min}, {max}], got {value}"
    )))
}

fn invalid_config(message: impl Into<String>) -> SynapseCalyxError {
    SynapseCalyxError::new("SYNAPSE_CALYX_CONFIG_INVALID", message, CONFIG_REMEDIATION)
}

fn validated_source_event_time(constellation: &Constellation) -> Result<i64, SynapseCalyxError> {
    if constellation.metadata_value(METADATA_TEMPORAL_LANE_STATE) != Some(TEMPORAL_LANE_ACTIVE) {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_TEMPORAL_LANE_INACTIVE",
            format!(
                "candidate {} does not declare temporal_lane_state=active",
                constellation.cx_id
            ),
            "rebuild the source constellation from an authoritative event timestamp before temporal retrieval",
        ));
    }
    let event_time_secs = constellation
        .metadata_value(METADATA_SOURCE_EVENT_TIME_SECS)
        .ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_MISSING",
                format!(
                    "candidate {} is active but metadata {METADATA_SOURCE_EVENT_TIME_SECS:?} is absent",
                    constellation.cx_id
                ),
                "repair the source projection so active temporal lanes persist exact event-time seconds",
            )
        })?
        .parse::<i64>()
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_INVALID",
                format!(
                    "candidate {} has invalid {METADATA_SOURCE_EVENT_TIME_SECS}: {error}",
                    constellation.cx_id
                ),
                "rebuild the source constellation from a valid Unix event timestamp",
            )
        })?;
    let raw_ns = constellation
        .metadata_value(METADATA_SOURCE_EVENT_TIME_RAW)
        .ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_RAW_MISSING",
                format!(
                    "candidate {} is active but metadata {METADATA_SOURCE_EVENT_TIME_RAW:?} is absent",
                    constellation.cx_id
                ),
                "repair the source projection so active temporal lanes retain the verbatim source timestamp",
            )
        })?
        .parse::<u64>()
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_RAW_INVALID",
                format!(
                    "candidate {} has invalid nanosecond {METADATA_SOURCE_EVENT_TIME_RAW}: {error}",
                    constellation.cx_id
                ),
                "rebuild the source constellation from its authoritative unsigned nanosecond timestamp",
            )
        })?;
    let derived_secs = i64::try_from(raw_ns / 1_000_000_000).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_OVERFLOW",
            format!(
                "candidate {} raw event timestamp {raw_ns}ns cannot convert to Unix seconds: {error}",
                constellation.cx_id
            ),
            "repair the corrupt event timestamp at the source and rebuild its constellation",
        )
    })?;
    if derived_secs != event_time_secs {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_MISMATCH",
            format!(
                "candidate {} event_time_secs={event_time_secs} disagrees with raw_ns={raw_ns} -> {derived_secs}",
                constellation.cx_id
            ),
            "repair the source projection and rebuild the constellation; do not score contradictory time evidence",
        ));
    }
    Ok(event_time_secs)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxError {
    pub code: &'static str,
    pub message: String,
    pub remediation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_code: Option<&'static str>,
}

impl SynapseCalyxError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>, remediation: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            remediation,
            source_code: None,
        }
    }

    #[must_use]
    pub fn with_io(
        code: &'static str,
        action: &str,
        path: &Path,
        error: &std::io::Error,
        remediation: &'static str,
    ) -> Self {
        Self::new(
            code,
            format!("{action} {}: {error}", path.display()),
            remediation,
        )
    }

    #[must_use]
    pub fn from_calyx(action: &str, error: &calyx_core::CalyxError) -> Self {
        let code = error_bridge::map_calyx_error_code(error.code).unwrap_or_else(|| {
            if error.code.starts_with("CALYX_") {
                tracing::debug!(
                    code = error.code,
                    action,
                    "preserving subsystem-local Calyx error code across the Synapse bridge"
                );
                error.code
            } else {
                tracing::error!(
                    code = error_bridge::SYNAPSE_CALYX_UNMAPPED_ERROR,
                    calyx_code = error.code,
                    action,
                    "invalid non-Calyx error code reached the Synapse bridge"
                );
                error_bridge::SYNAPSE_CALYX_UNMAPPED_ERROR
            }
        });
        Self {
            code,
            message: format!("{action}: {}", error.message),
            remediation: error.remediation,
            source_code: Some(error.code),
        }
    }

    #[must_use]
    pub fn is_from_calyx_code(&self, calyx_code: &str) -> bool {
        self.source_code == Some(calyx_code)
    }
}

impl std::fmt::Display for SynapseCalyxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.source_code {
            Some(source_code) => write!(
                f,
                "{}: {}; source_code={}; remediation={}",
                self.code, self.message, source_code, self.remediation
            ),
            None => write!(
                f,
                "{}: {}; remediation={}",
                self.code, self.message, self.remediation
            ),
        }
    }
}

impl std::error::Error for SynapseCalyxError {}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SynapseCalyxVaultStatus {
    pub enabled: bool,
    pub phase: String,
    pub open: bool,
    pub open_mode: Option<String>,
    pub restore_mvcc_rows: Option<bool>,
    pub eager_router_lookup_on_open: Option<bool>,
    pub vault_dir: Option<PathBuf>,
    pub identity_path: Option<PathBuf>,
    pub machine_salt_path: Option<PathBuf>,
    pub lock_path: Option<PathBuf>,
    pub pid_path: Option<PathBuf>,
    pub vault_id: Option<String>,
    pub latest_seq: Option<u64>,
    pub last_recovered_seq: Option<u64>,
    pub torn_tail: Option<String>,
    pub last_error_code: Option<String>,
    pub last_calyx_error_code: Option<String>,
    pub last_error: Option<String>,
    pub remediation: Option<String>,
    pub mvcc_resident_keys: Option<u64>,
    pub mvcc_resident_versions: Option<u64>,
    pub mvcc_resident_key_bytes: Option<u64>,
    pub mvcc_resident_value_bytes: Option<u64>,
    pub mvcc_resident_payload_bytes: Option<u64>,
    pub memtable_used_bytes: Option<u64>,
    pub memtable_cap_bytes: Option<u64>,
    pub memtable_high_water_bytes: Option<u64>,
    pub sst_reader_cache_entries: Option<u64>,
    pub sst_reader_cache_estimated_heap_bytes: Option<u64>,
    pub sst_reader_cache_mapped_bytes: Option<u64>,
    pub sst_reader_cache_max_entries: Option<u64>,
    pub sst_reader_cache_max_estimated_heap_bytes: Option<u64>,
    pub sst_reader_cache_max_mapped_bytes: Option<u64>,
    pub retained_lookup_files: Option<u64>,
    pub retained_lookup_entries: Option<u64>,
    pub retained_lookup_estimated_heap_bytes: Option<u64>,
    pub retained_lookup_per_cf: Vec<SynapseCalyxRetainedLookupStatus>,
    pub tuning: Option<SynapseCalyxTuningConfig>,
    pub anneal: Option<SynapseCalyxAnnealStatus>,
    pub math_backend: Option<SynapseCalyxMathBackendStatus>,
    pub assay_compute_backend: Option<String>,
    /// Per-site row-table read-guard tallies since this vault was opened.
    ///
    /// Empty when the vault is not open. Every declared site appears when it
    /// is, including sites with zero holds — that zero is the observation
    /// #1952 ask 3 needed and could not get from an exception-only log.
    pub row_guard_census: Vec<SynapseCalyxRowGuardSiteCensus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxRetainedLookupStatus {
    pub cf: String,
    pub files: u64,
    pub entries: u64,
    pub estimated_heap_bytes: u64,
}

/// One row-guard call site's counters, as reported by `health`.
///
/// `holds` counts **every** acquisition; `over_budget_holds` counts only the
/// subset that also emitted `CALYX_ASTER_ROW_READ_GUARD_SLOW`. The pair is what
/// separates "this path never ran" from "this path ran and stayed inside the
/// 25 ms budget", which the log alone cannot do because both look like silence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxRowGuardSiteCensus {
    pub site: String,
    pub holds: u64,
    pub total_held_us: u64,
    pub max_held_us: u64,
    /// Mean hold in microseconds. `None` when the site never ran — a site with
    /// no holds has no mean, and reporting `0.0` would read as "instant".
    pub mean_held_us: Option<f64>,
    pub over_budget_holds: u64,
    /// Over-budget holds whose thread was not running for most of the hold
    /// (#1955). A non-zero count here means the window was load-contaminated
    /// and its latencies are not a measure of work.
    pub starved_holds: u64,
}

/// Bounds on one snapshot-version GC pass, as Synapse configures them (#2122).
///
/// Mirrors `calyx_aster::mvcc::SnapshotVersionGcBudget` so the storage crate can
/// size a pass without depending on `calyx-aster` directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SynapseCalyxSnapshotVersionGcBudget {
    pub max_versions: usize,
    pub max_chains_scanned: usize,
    pub max_pass_us: u64,
    pub max_shard_hold_us: u64,
}

impl Default for SynapseCalyxSnapshotVersionGcBudget {
    fn default() -> Self {
        SnapshotVersionGcBudget::default().into()
    }
}

impl SynapseCalyxSnapshotVersionGcBudget {
    /// Reads the budget from the environment, failing closed on an invalid or
    /// zero value.
    ///
    /// # Errors
    ///
    /// Returns a structured error when a `CALYX_SNAPSHOT_VERSION_GC_*` variable
    /// is set but not a positive integer.
    pub fn from_env() -> Result<Self, SynapseCalyxError> {
        SnapshotVersionGcBudget::from_env()
            .map(Into::into)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read snapshot-version GC budget from env", &error)
            })
    }

    /// Scales the work budgets by `factor`, never the per-shard hold budget.
    #[must_use]
    pub fn scaled(self, factor: u32) -> Self {
        SnapshotVersionGcBudget::from(self).scaled(factor).into()
    }
}

impl From<SnapshotVersionGcBudget> for SynapseCalyxSnapshotVersionGcBudget {
    fn from(budget: SnapshotVersionGcBudget) -> Self {
        Self {
            max_versions: budget.max_versions,
            max_chains_scanned: budget.max_chains_scanned,
            max_pass_us: budget.max_pass_us,
            max_shard_hold_us: budget.max_shard_hold_us,
        }
    }
}

impl From<SynapseCalyxSnapshotVersionGcBudget> for SnapshotVersionGcBudget {
    fn from(budget: SynapseCalyxSnapshotVersionGcBudget) -> Self {
        Self {
            max_versions: budget.max_versions,
            max_chains_scanned: budget.max_chains_scanned,
            max_pass_us: budget.max_pass_us,
            max_shard_hold_us: budget.max_shard_hold_us,
        }
    }
}

/// What one snapshot-version GC pass actually did (#2122).
///
/// Every field is a measurement. `sweep_completed` is the one that licenses a
/// claim: only a pass that visited every shard and every chain proves the
/// reclaimable debt is drained, so `versions_reclaimed == 0` on its own means
/// "this pass freed nothing", never "there was nothing to free".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxSnapshotVersionGcPass {
    pub floor_seq: u64,
    pub current_seq: u64,
    pub active_leases: usize,
    pub versions_reclaimed: u64,
    pub bytes_reclaimed: u64,
    pub chains_compacted: u64,
    pub chains_scanned: u64,
    pub shards_visited: usize,
    pub shards_total: usize,
    /// Row-table shard write guards acquired. Above `shards_visited` whenever
    /// the per-shard hold budget made the pass release and re-acquire mid-walk.
    pub shard_guard_holds: u64,
    pub sweep_completed: bool,
    pub stopped_on: String,
    pub elapsed_us: u64,
    pub max_shard_hold_us: u64,
    pub resume_shard: usize,
}

/// Independent point-in-time observation of the in-memory MVCC reclamation
/// Source of Truth.
///
/// The totals are process-lifetime monotonic counters. Reading this separately
/// from a GC trigger proves whether the vault's physical version table changed;
/// it does not merely repeat the trigger's return value (#2146/#2150).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxSnapshotGcObservation {
    pub floor_seq: u64,
    pub current_seq: u64,
    /// Number of unexpired readers in Aster's physical lease watchdog.
    #[serde(default)]
    pub active_reader_leases: u64,
    /// Oldest sequence still pinned by a live reader, if any.
    #[serde(default)]
    pub oldest_pinned_seq: Option<u64>,
    /// Process-lifetime count of expired readers physically aborted by Aster.
    #[serde(default)]
    pub reader_lease_expired_total: u64,
    pub versions_reclaimed_total: u64,
    pub bytes_reclaimed_total: u64,
    pub soft_deletes_purged_total: u64,
    /// Debt measured by the most recent GC metrics observation.
    pub last_measured_compaction_debt: u64,
}

/// Physical readback from a checkpoint-time process-local MVCC delta rebase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxSnapshotDeltaRebaseReport {
    pub rebased: bool,
    pub active_leases: usize,
    pub previous_floor_seq: u64,
    pub new_floor_seq: u64,
    pub flushed_ssts: usize,
    pub before_keys: u64,
    pub before_versions: u64,
    pub before_payload_bytes: u64,
    pub after_keys: u64,
    pub after_versions: u64,
    pub after_payload_bytes: u64,
}

impl From<SnapshotDeltaRebaseReport> for SynapseCalyxSnapshotDeltaRebaseReport {
    fn from(report: SnapshotDeltaRebaseReport) -> Self {
        Self {
            rebased: report.rebased,
            active_leases: report.active_leases,
            previous_floor_seq: report.previous_floor_seq,
            new_floor_seq: report.new_floor_seq,
            flushed_ssts: report.flushed_ssts,
            before_keys: report.before.keys,
            before_versions: report.before.versions,
            before_payload_bytes: report.before.payload_bytes(),
            after_keys: report.after.keys,
            after_versions: report.after.versions,
            after_payload_bytes: report.after.payload_bytes(),
        }
    }
}

impl From<SnapshotVersionGcPass> for SynapseCalyxSnapshotVersionGcPass {
    fn from(pass: SnapshotVersionGcPass) -> Self {
        Self {
            floor_seq: pass.floor_seq,
            current_seq: pass.current_seq,
            active_leases: pass.active_leases,
            versions_reclaimed: pass.versions_reclaimed,
            bytes_reclaimed: pass.bytes_reclaimed,
            chains_compacted: pass.chains_compacted,
            chains_scanned: pass.chains_scanned,
            shards_visited: pass.shards_visited,
            shards_total: pass.shards_total,
            shard_guard_holds: pass.shard_guard_holds,
            sweep_completed: pass.sweep_completed,
            stopped_on: pass.stopped_on.as_str().to_owned(),
            elapsed_us: pass.elapsed_us,
            max_shard_hold_us: pass.max_shard_hold_us,
            resume_shard: pass.resume_shard,
        }
    }
}

/// This process's committed private memory in bytes.
///
/// Re-exported through Synapse's Calyx bridge because it is the only
/// memory number a pressure decision may be keyed off on Windows: working set
/// (what `calyx_heap_rss_bytes` reports) is trimmed by the OS and fell while the
/// #2122 daemon leaked 1.07 GB/hour. On Linux this is `RssAnon`.
///
/// # Errors
///
/// Fails closed when the OS counter cannot be read. There is deliberately no
/// working-set fallback.
pub fn process_private_bytes() -> Result<u64, SynapseCalyxError> {
    calyx_aster::resource::process_private_bytes()
        .map_err(|error| SynapseCalyxError::from_calyx("read process private commit", &error))
}

/// This process's resident working set in bytes.
///
/// This is an observation surface only. Admission and pressure decisions remain
/// keyed to [`process_private_bytes`], because the operating system may trim a
/// working set without releasing committed private memory.
///
/// # Errors
///
/// Fails closed when the operating-system resident-set counter cannot be read.
pub fn process_working_set_bytes() -> Result<u64, SynapseCalyxError> {
    calyx_aster::resource::heap_rss_bytes()
        .map_err(|error| SynapseCalyxError::from_calyx("read process resident working set", &error))
}

/// Process allocator hook used to return dead transient rebuild pages to the OS.
///
/// Calyx is allocator-agnostic, so the executable that selects the global
/// allocator owns this hook. `synapse-mcp` installs its mimalloc collector once
/// before opening the vault. Keeping that ownership explicit prevents a library
/// consumer that uses the system allocator from accidentally invoking a foreign
/// allocator API.
type ProcessMemoryReclaimer = fn();
static PROCESS_MEMORY_RECLAIMER: OnceLock<ProcessMemoryReclaimer> = OnceLock::new();

/// Installs the executable-owned process memory reclaimer exactly once.
///
/// # Errors
///
/// Fails closed when process initialization attempts to install more than one
/// owner. Multiple allocator authorities would make reclamation unsound.
pub fn install_process_memory_reclaimer(
    reclaimer: ProcessMemoryReclaimer,
) -> Result<(), SynapseCalyxError> {
    PROCESS_MEMORY_RECLAIMER.set(reclaimer).map_err(|_| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_MEMORY_RECLAIMER_ALREADY_INSTALLED",
            "a process memory reclaimer was already installed",
            "install the allocator-owned reclaimer once, before opening any Calyx vault",
        )
    })
}

const PROCESS_MEMORY_RELEASE_GROWTH_BYTES: u64 = 32 * 1024 * 1024;
const CF_WALK_MEMORY_SAMPLE_ROWS: usize = 64 * 1024;

struct CfWalkMemoryTracker {
    column_family: String,
    track_progress: bool,
    private_bytes_before: u64,
    private_bytes_peak: u64,
    progress_memory_samples: u64,
}

impl CfWalkMemoryTracker {
    fn new(cf: ColumnFamily) -> Result<Option<Self>, SynapseCalyxError> {
        if PROCESS_MEMORY_RECLAIMER.get().is_none() {
            return Ok(None);
        }
        let private_bytes_before = process_private_bytes()?;
        Ok(Some(Self {
            column_family: cf.name(),
            track_progress: cf == ColumnFamily::Base,
            private_bytes_before,
            private_bytes_peak: private_bytes_before,
            progress_memory_samples: 0,
        }))
    }

    fn observe_progress(
        &mut self,
        logical_groups: usize,
        rows_examined: usize,
    ) -> Result<(), SynapseCalyxError> {
        // Only payload-heavy Base walks receive progress sampling. The cursor
        // owns no output page to reclaim: it reuses its source buffers until
        // the complete walk ends. Sampling at 64-Ki-row cadence preserves peak
        // observability without turning a multi-million-row scan into hundreds
        // of thousands of operating-system counter calls.
        if !self.track_progress {
            return Ok(());
        }
        let current = process_private_bytes()?;
        self.private_bytes_peak = self.private_bytes_peak.max(current);
        self.progress_memory_samples = self.progress_memory_samples.saturating_add(1);
        tracing::debug!(
            code = "SYNAPSE_CALYX_CF_WALK_MEMORY_PROGRESS",
            cf = self.column_family,
            logical_groups,
            rows_examined,
            private_bytes = current,
            "sampled process-private memory while advancing an allocation-reusing CF row cursor"
        );
        Ok(())
    }

    /// Reclaims after the streaming cursor and all of its file readers/frontier
    /// values have been destroyed.
    ///
    /// Progress callbacks cannot release cursor-owned allocations because the
    /// stream necessarily remains live until this boundary.
    fn walk_released(
        &mut self,
        pages: usize,
        rows_examined: usize,
    ) -> Result<(), SynapseCalyxError> {
        let before = process_private_bytes()?;
        self.private_bytes_peak = self.private_bytes_peak.max(before);
        if !self.track_progress
            && before.saturating_sub(self.private_bytes_before)
                < PROCESS_MEMORY_RELEASE_GROWTH_BYTES
        {
            return Ok(());
        }
        let reclaim = PROCESS_MEMORY_RECLAIMER.get().copied().ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_MEMORY_RECLAIMER_DISAPPEARED",
                format!(
                    "the process memory reclaimer disappeared after a {} CF walk",
                    self.column_family
                ),
                "repair process initialization; a once-installed allocator authority must remain available for the process lifetime",
            )
        })?;
        let started = Instant::now();
        reclaim();
        let after = process_private_bytes()?;
        let reclaimed = before.saturating_sub(after);
        tracing::info!(
            code = "SYNAPSE_CALYX_CF_WALK_MEMORY_RELEASED",
            cf = self.column_family,
            pages,
            rows_examined,
            private_bytes_start = self.private_bytes_before,
            private_bytes_peak = self.private_bytes_peak,
            private_bytes_before = before,
            private_bytes_after = after,
            private_bytes_reclaimed = reclaimed,
            progress_memory_samples = self.progress_memory_samples,
            reclaim_elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            "returned cursor-owned CF scan memory after the complete streaming owner was destroyed"
        );
        Ok(())
    }
}

/// One explicit allocator collection at a caller-owned transient boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxProcessMemoryRelease {
    pub private_bytes_before: u64,
    pub private_bytes_after: u64,
    pub private_bytes_reclaimed: u64,
    pub elapsed_us: u64,
}

/// Returns allocator pages made dead by a completed, caller-owned operation to
/// the operating system.
///
/// This is intentionally explicit rather than a memory limit. The caller must
/// invoke it only after every large transient it owns has been dropped; doing
/// so earlier merely scans live allocations and conceals the real ownership
/// boundary.
///
/// # Errors
///
/// Fails closed when the executable did not install its allocator authority or
/// the process-private-memory Source of Truth cannot be read before and after
/// collection.
pub fn release_process_memory(
    operation: &'static str,
) -> Result<SynapseCalyxProcessMemoryRelease, SynapseCalyxError> {
    let before = process_private_bytes()?;
    let reclaim = PROCESS_MEMORY_RECLAIMER.get().copied().ok_or_else(|| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_MEMORY_RECLAIMER_NOT_INSTALLED",
            format!(
                "operation {operation} reached its transient ownership boundary without an installed process allocator reclaimer"
            ),
            "install the executable-owned allocator reclaimer before opening the Calyx vault",
        )
    })?;
    let started = Instant::now();
    reclaim();
    let after = process_private_bytes()?;
    let release = SynapseCalyxProcessMemoryRelease {
        private_bytes_before: before,
        private_bytes_after: after,
        private_bytes_reclaimed: before.saturating_sub(after),
        elapsed_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
    };
    tracing::info!(
        code = "SYNAPSE_CALYX_PROCESS_MEMORY_OWNERSHIP_RELEASED",
        operation,
        private_bytes_before = release.private_bytes_before,
        private_bytes_after = release.private_bytes_after,
        private_bytes_reclaimed = release.private_bytes_reclaimed,
        elapsed_us = release.elapsed_us,
        "returned dead transient pages after their complete caller-owned operation ended"
    );
    Ok(release)
}

struct SearchRebuildMemoryTracker {
    private_bytes_peak: u64,
    private_bytes_peak_phase: String,
    private_bytes_after_last_reclaim: u64,
    reclaim_calls: u64,
    observed_reclaimed: u64,
    missing_reclaimer_reported: bool,
}

impl SearchRebuildMemoryTracker {
    fn new(private_bytes_before: u64) -> Self {
        Self {
            private_bytes_peak: private_bytes_before,
            private_bytes_peak_phase: "before_rebuild".to_owned(),
            private_bytes_after_last_reclaim: private_bytes_before,
            reclaim_calls: 0,
            observed_reclaimed: 0,
            missing_reclaimer_reported: false,
        }
    }

    fn observe(&mut self, progress: &calyx_search::RebuildProgress<'_>) -> Result<(), CalyxError> {
        let observed = calyx_aster::resource::process_private_bytes()?;
        let phase = rebuild_progress_phase(progress);
        self.record_peak(observed, &phase);

        // A completed Base page is also an exact ownership-release boundary:
        // its encoded values and decoded constellation payloads have been
        // consumed, while only compact CxId memberships remain live. Rayon can
        // free those payloads from threads other than the scanning thread, so
        // mimalloc may retain their now-unused pages until an explicit process
        // collection. Reclaim only after meaningful growth, not on every page;
        // this follows the allocator's guidance for long-running processes with
        // cross-thread frees while keeping the scan's live memberships intact.
        let base_page_growth_boundary = progress.phase == "base_scan_page"
            && observed.saturating_sub(self.private_bytes_after_last_reclaim)
                >= PROCESS_MEMORY_RELEASE_GROWTH_BYTES;
        let final_base_release_boundary = progress.phase == "load_docs_ok";

        // Completed slots and filters remain exact ownership-release boundaries:
        // their transient row buffers and encoders have been dropped.
        let structural_release_boundary = matches!(
            progress.phase,
            "slot_index_write_ok"
                | "slot_worker_quiesced"
                | "dense_slot_ok"
                | "sparse_slot_ok"
                | "multi_slot_ok"
                | "slot_build_ok"
                | "filter_ok"
                | "manifest_validate_ok"
                | "done"
        );
        let release_boundary =
            base_page_growth_boundary || final_base_release_boundary || structural_release_boundary;
        if !release_boundary {
            return Ok(());
        }

        let Some(reclaim) = PROCESS_MEMORY_RECLAIMER.get().copied() else {
            if !self.missing_reclaimer_reported {
                self.missing_reclaimer_reported = true;
                tracing::warn!(
                    code = "SYNAPSE_CALYX_PROCESS_MEMORY_RECLAIMER_NOT_INSTALLED",
                    phase,
                    observed_private_bytes = observed,
                    "the executable did not install an allocator-owned transient-memory reclaimer"
                );
            }
            return Ok(());
        };

        let started = Instant::now();
        reclaim();
        let after = calyx_aster::resource::process_private_bytes()?;
        self.private_bytes_after_last_reclaim = after;
        self.reclaim_calls = self.reclaim_calls.saturating_add(1);
        self.observed_reclaimed = self
            .observed_reclaimed
            .saturating_add(observed.saturating_sub(after));
        self.record_peak(after, &format!("{phase} after_allocator_reclaim"));
        tracing::info!(
            code = "SYNAPSE_CALYX_TRANSIENT_MEMORY_RECLAIMED",
            phase,
            release_boundary,
            base_page_growth_boundary,
            final_base_release_boundary,
            structural_release_boundary,
            private_bytes_before = observed,
            private_bytes_after = after,
            private_bytes_reclaimed = observed.saturating_sub(after),
            reclaim_elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            reclaim_calls = self.reclaim_calls,
            "returned allocator-owned transient rebuild pages to the operating system"
        );
        Ok(())
    }

    fn record_peak(&mut self, observed: u64, phase: &str) {
        if observed > self.private_bytes_peak {
            self.private_bytes_peak = observed;
            phase.clone_into(&mut self.private_bytes_peak_phase);
        }
    }
}

fn rebuild_progress_phase(progress: &calyx_search::RebuildProgress<'_>) -> String {
    progress.panel_slot.map_or_else(
        || progress.phase.to_owned(),
        |panel_slot| format!("{} panel_slot={panel_slot:?}", progress.phase),
    )
}

impl SynapseCalyxVaultStatus {
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            phase: "disabled".to_owned(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn not_opened(config: Option<&SynapseCalyxConfig>) -> Self {
        let mut status = Self {
            enabled: true,
            phase: "not_opened".to_owned(),
            ..Self::default()
        };
        if let Some(config) = config {
            status.apply_paths(config);
        }
        status
    }

    #[must_use]
    pub fn error(
        config: Option<&SynapseCalyxConfig>,
        phase: &'static str,
        error: &SynapseCalyxError,
    ) -> Self {
        let mut status = Self {
            enabled: true,
            phase: phase.to_owned(),
            last_error_code: Some(error.code.to_owned()),
            last_calyx_error_code: error.source_code.map(str::to_owned),
            last_error: Some(error.message.clone()),
            remediation: Some(error.remediation.to_owned()),
            ..Self::default()
        };
        if let Some(config) = config {
            status.apply_paths(config);
        }
        status
    }

    fn apply_paths(&mut self, config: &SynapseCalyxConfig) {
        self.vault_dir = Some(config.vault_dir.clone());
        self.identity_path = Some(identity_path(&config.vault_dir));
        self.machine_salt_path = Some(config.machine_salt_path.clone());
        self.lock_path = Some(lock_path(&config.vault_dir));
        self.pid_path = Some(pid_path(&config.vault_dir));
        self.tuning = Some(config.tuning.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the close readback preserves independent physical facts rather than collapsing them into one inferred state"
)]
pub struct SynapseCalyxVaultCloseReadback {
    pub enabled: bool,
    pub reason: &'static str,
    pub closed: bool,
    pub safe_to_unlock: bool,
    pub vault_dir: Option<PathBuf>,
    pub lock_path: Option<PathBuf>,
    pub pid_path: Option<PathBuf>,
    pub pid_sidecar_present_after_close: Option<bool>,
    pub re_lock_probe_succeeded: Option<bool>,
    pub latest_seq: Option<u64>,
    pub gpu_reservation_release: Option<HostGpuReservationSnapshot>,
    /// True when every durable close obligation completed and process
    /// termination is the remaining resource-reclamation boundary.
    pub safe_to_terminate: bool,
    /// True only when the terminal-process close deliberately retained the
    /// resident vault graph and exclusive vault lock for OS reclamation.
    pub terminal_process_reclaim: bool,
    pub lock_retained_until_process_exit: bool,
}

impl SynapseCalyxVaultCloseReadback {
    #[must_use]
    pub const fn disabled(reason: &'static str) -> Self {
        Self {
            enabled: false,
            reason,
            closed: true,
            safe_to_unlock: true,
            vault_dir: None,
            lock_path: None,
            pid_path: None,
            pid_sidecar_present_after_close: None,
            re_lock_probe_succeeded: None,
            latest_seq: None,
            gpu_reservation_release: None,
            safe_to_terminate: true,
            terminal_process_reclaim: false,
            lock_retained_until_process_exit: false,
        }
    }

    #[must_use]
    pub fn not_open(reason: &'static str, config: Option<&SynapseCalyxConfig>) -> Self {
        Self {
            enabled: true,
            reason,
            closed: true,
            safe_to_unlock: true,
            vault_dir: config.map(|config| config.vault_dir.clone()),
            lock_path: config.map(|config| lock_path(&config.vault_dir)),
            pid_path: config.map(|config| pid_path(&config.vault_dir)),
            pid_sidecar_present_after_close: None,
            re_lock_probe_succeeded: None,
            latest_seq: None,
            gpu_reservation_release: None,
            safe_to_terminate: true,
            terminal_process_reclaim: false,
            lock_retained_until_process_exit: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SynapseCalyxVaultOpenMode {
    FullMvccRestore,
    LatestReadback,
}

impl SynapseCalyxVaultOpenMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FullMvccRestore => "full_mvcc_restore",
            Self::LatestReadback => "latest_readback",
        }
    }

    const fn restore_mvcc_rows(self) -> bool {
        match self {
            Self::FullMvccRestore => true,
            Self::LatestReadback => false,
        }
    }

    const fn eager_router_lookup_on_open(self) -> bool {
        match self {
            Self::FullMvccRestore => true,
            Self::LatestReadback => false,
        }
    }
}

#[derive(Debug)]
pub struct SynapseCalyxVault {
    config: SynapseCalyxConfig,
    vault: AsterVault<SynapseCalyxClock>,
    anneal_ledger_index: std::sync::Mutex<calyx_anneal::AsterAnnealLedgerIndex>,
    lock: VaultLockGuard,
    math_runtime: SynapseCalyxMathRuntime,
    open_mode: SynapseCalyxVaultOpenMode,
    lineage: SynapseCalyxVaultLineage,
    /// Physical CF row counts kept alongside the **per-family** change signal
    /// they were measured at, so a repeat count can be *proved* redundant
    /// (#2114, sharpened by #2139).
    cf_count_memo: std::sync::Mutex<BTreeMap<ColumnFamily, MemoizedCfCount>>,
    /// Decoded immutable Ward serving generation. Reuse is licensed only by
    /// the Guard CF's exact `(last_commit_seq, out_of_band_epoch)` signal and
    /// the profile hash; see `ward::GuardServingMemo` (#2124).
    guard_serving_memo: std::sync::Mutex<Option<ward::GuardServingMemo>>,
}

/// One physical CF row count, the walk that produced it, and the bookkeeping
/// that decides when it must be re-measured (#2114, #2139).
#[derive(Clone, Debug)]
struct MemoizedCfCount {
    /// The walk exactly as `count_cf_latest_bounded` returned it. Only atomic
    /// walks are memoized: a paged walk that straddled a commit describes an
    /// interval rather than an instant, and carrying such a number forward
    /// would propagate an inexactness instead of retiring it.
    walk: SynapseCalyxCfWalk,
    /// The family's out-of-band content epoch sampled **before** the walk
    /// started (#2139).
    ///
    /// Before the walk, not after: a CF retire or a retention-GC input swap
    /// that lands *while* the walk is paging must invalidate this entry, and it
    /// only does so if the recorded epoch predates it.
    out_of_band_epoch_before_walk: u64,
}

/// A physical CF row count together with how it was obtained (#2114, #2139).
///
/// The provenance travels with the number so a log line can say which it is
/// rather than implying a fresh walk that may not have happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCfCountProvenance {
    Walked,
    MaintainedExact,
    UnchangedSinceLastWalk,
}

impl SynapseCalyxCfCountProvenance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Walked => "walked",
            Self::MaintainedExact => "maintained_exact",
            Self::UnchangedSinceLastWalk => "unchanged_since_last_walk",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemoizedCfCountReadback {
    /// The walk that produced the count — freshly measured, or the memoized
    /// one that the family's change signal proves still describes it.
    pub walk: SynapseCalyxCfWalk,
    /// Whether the column family was physically walked on this call.
    pub measured: bool,
    /// Whether the answer came from the exact commit-maintained aggregate.
    /// This is neither an estimate nor an unchanged memo: every logical
    /// insert/delete transition through the Aster commit boundary has already
    /// advanced it.
    pub maintained_exact: bool,
    /// When reused, the walk sequence the family has not committed past.
    pub unchanged_since_seq: Option<Seq>,
    /// The family's last-commit sequence at this call, the `O(1)` signal the
    /// reuse decision was made on (#2139). Always present, so a log line can
    /// show *why* a walk did or did not happen.
    pub cf_last_commit_seq: Seq,
    /// The vault-wide latest sequence at this call. Reported next to
    /// `cf_last_commit_seq` because the gap between them is exactly the traffic
    /// the whole-vault gate used to be defeated by.
    pub vault_latest_seq: Seq,
}

impl MemoizedCfCountReadback {
    /// Rows the column family holds.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.walk.rows_visited
    }

    /// A short label naming how the count was obtained, for the log line that
    /// reports it.
    #[must_use]
    pub const fn provenance(&self) -> &'static str {
        self.provenance_kind().as_str()
    }

    #[must_use]
    pub const fn provenance_kind(&self) -> SynapseCalyxCfCountProvenance {
        if self.measured {
            SynapseCalyxCfCountProvenance::Walked
        } else if self.maintained_exact {
            SynapseCalyxCfCountProvenance::MaintainedExact
        } else {
            SynapseCalyxCfCountProvenance::UnchangedSinceLastWalk
        }
    }
}

#[derive(Debug)]
pub struct SynapseCalyxReadOnlyVault {
    config: SynapseCalyxConfig,
    vault: AsterVault<SynapseCalyxClock>,
}

fn walk_cf_range_snapshot_stream<V>(
    vault: &AsterVault<SynapseCalyxClock>,
    snapshot: Snapshot,
    cf: ColumnFamily,
    range: &KeyRange,
    page_rows: usize,
    mut visit: V,
) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
where
    V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
{
    if page_rows == 0 {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_CF_WALK_PAGE_ROWS_ZERO",
            format!(
                "a pinned walk over {} needs a positive logical reporting-group size",
                cf.name()
            ),
            "pass SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, or another positive reporting-group size",
        ));
    }
    let mut walk = SynapseCalyxCfWalk {
        column_family: cf.name(),
        page_rows,
        pages: 0,
        rows_examined: 0,
        rows_visited: 0,
        stopped_early: false,
        snapshot_seq_first: snapshot.seq(),
        snapshot_seq_last: snapshot.seq(),
    };
    let mut walk_memory = CfWalkMemoryTracker::new(cf)?;
    let result = vault.walk_cf_range_rows_snapshot(snapshot, cf, range, |key, value| {
        walk.rows_examined = walk.rows_examined.checked_add(1).ok_or_else(|| {
            SynapseCalyxSnapshotWalkControl::Error(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CF_WALK_ROW_COUNT_OVERFLOW",
                format!("the {} CF walk exceeded usize on this host", cf.name()),
                "inspect the immutable manifest and repair the impossible row cardinality before retrying",
            ))
        })?;
        walk.rows_visited = walk.rows_visited.checked_add(1).ok_or_else(|| {
            SynapseCalyxSnapshotWalkControl::Error(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CF_WALK_ROW_COUNT_OVERFLOW",
                format!("the {} CF visitor count exceeded usize on this host", cf.name()),
                "inspect the immutable manifest and repair the impossible row cardinality before retrying",
            ))
        })?;
        if (walk.rows_examined - 1).is_multiple_of(page_rows) {
            walk.pages = walk.pages.checked_add(1).ok_or_else(|| {
                SynapseCalyxSnapshotWalkControl::Error(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_CF_WALK_PAGE_COUNT_OVERFLOW",
                    format!("the {} CF logical group count exceeded usize on this host", cf.name()),
                    "inspect the immutable manifest and repair the impossible row cardinality before retrying",
                ))
            })?;
        }
        let step = visit(key, value).map_err(SynapseCalyxSnapshotWalkControl::Error)?;
        if walk
            .rows_examined
            .is_multiple_of(CF_WALK_MEMORY_SAMPLE_ROWS)
            && let Some(memory) = walk_memory.as_mut()
        {
            memory
                .observe_progress(walk.pages, walk.rows_examined)
                .map_err(SynapseCalyxSnapshotWalkControl::Error)?;
        }
        match step {
            SynapseCalyxWalkStep::Continue => Ok(ControlFlow::Continue(())),
            SynapseCalyxWalkStep::Stop => {
                walk.stopped_early = true;
                Ok(ControlFlow::Break(()))
            }
        }
    });
    // The raw stream owns the immutable cursor. It has returned here, so every
    // reader, merge-frontier value, and reusable value buffer is dead before
    // collection.
    let release_result = walk_memory.as_mut().map_or(Ok(()), |memory| {
        memory.walk_released(walk.pages, walk.rows_examined)
    });
    if let Err(release_error) = release_result {
        return match result {
            Err(SynapseCalyxSnapshotWalkControl::Error(scan_error)) => Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CF_WALK_AND_MEMORY_RELEASE_FAILED",
                format!(
                    "the {} CF walk failed with {scan_error}; after destroying its streaming cursor, allocator release also failed with {release_error}",
                    cf.name()
                ),
                "repair the named scan failure and the process-memory read/reclaimer failure before retrying; both independent failures are preserved here",
            )),
            Ok(_) => Err(release_error),
        };
    }
    match result {
        Ok(outcome) => {
            ensure_cf_walk_outcome_matches(cf, outcome, &walk)?;
            // Preserve the historical one-group provenance for an empty CF.
            if walk.pages == 0 {
                walk.pages = 1;
            }
            Ok(walk)
        }
        Err(SynapseCalyxSnapshotWalkControl::Error(error)) => Err(error),
    }
}

fn ensure_cf_walk_outcome_matches(
    cf: ColumnFamily,
    outcome: calyx_aster::vault::AsterSnapshotCfRowWalk,
    walk: &SynapseCalyxCfWalk,
) -> Result<(), SynapseCalyxError> {
    if outcome.rows_visited == walk.rows_visited && outcome.stopped_early == walk.stopped_early {
        return Ok(());
    }
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_CF_WALK_OUTCOME_MISMATCH",
        format!(
            "the {} CF cursor reported rows={} stopped_early={}, but the visitor observed rows={} stopped_early={}",
            cf.name(),
            outcome.rows_visited,
            outcome.stopped_early,
            walk.rows_visited,
            walk.stopped_early
        ),
        "repair the Aster/Synapse row-walk contract before trusting any derived result",
    ))
}

impl SynapseCalyxReadOnlyVault {
    /// Reads physical snapshot-version GC state without mutating the vault.
    #[must_use]
    pub fn snapshot_gc_observation(&self) -> SynapseCalyxSnapshotGcObservation {
        let metrics = self.vault.snapshot_gc_counters_only();
        let leases = self.vault.reader_lease_view();
        let current_seq = self.vault.latest_seq();
        SynapseCalyxSnapshotGcObservation {
            floor_seq: leases.oldest_pinned_seq.unwrap_or(current_seq),
            current_seq,
            active_reader_leases: u64::try_from(leases.active_leases).unwrap_or(u64::MAX),
            oldest_pinned_seq: leases.oldest_pinned_seq,
            reader_lease_expired_total: leases.reader_lease_expired_total,
            versions_reclaimed_total: metrics.versions_reclaimed_total,
            bytes_reclaimed_total: metrics.bytes_freed_total,
            soft_deletes_purged_total: metrics.soft_deletes_purged_total,
            last_measured_compaction_debt: metrics.compaction_debt,
        }
    }

    /// Reads native `TimeSeries` rows for physical analytics verification.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical column family cannot be scanned.
    pub fn scan_timeseries_latest(&self) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_latest(ColumnFamily::TimeSeries)
    }

    /// Reads immutable collection descriptors for physical analytics verification.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical column family cannot be scanned.
    pub fn scan_collections_latest(&self) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_latest(ColumnFamily::Collections)
    }

    // `open_existing_reactive_only` / `scan_reactive_latest` were removed with
    // the `reactive_region_fsv` bin target in #2045. They were two-line wrappers
    // over `open_existing_with_cfs(.., [ColumnFamily::Reactive])` and
    // `scan_cf_latest(ColumnFamily::Reactive)`, and that bin was their only
    // caller anywhere in either workspace — so they were public API reachable
    // from nothing that ships. Call the two general methods directly.

    /// Reads content-addressed tuning artifacts for physical Anneal verification.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical artifact rows cannot be scanned.
    pub fn scan_anneal_tuning_artifacts_latest(
        &self,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_latest(ColumnFamily::Kv)
    }

    /// Reads native rollback snapshots and live pointers for physical verification.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical rollback rows cannot be scanned.
    pub fn scan_anneal_rollback_latest(&self) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_latest(ColumnFamily::AnnealRollback)
    }

    /// Opens an existing Calyx vault for physical inspection of the **`Kv`
    /// column family only**.
    ///
    /// This path does not create the vault directory, identity file, machine
    /// salt, lock file, or PID sidecar, and it does not acquire the Synapse
    /// exclusive writer lock. Mutating Aster operations fail closed because the
    /// underlying handle is opened with `read_only=true`.
    ///
    /// Named for what it selects (#1969 ask 2). It used to be `open_existing`,
    /// which reads as "open the vault" while opening one family of it, and a
    /// read of any other family then answered **zero rows** — a number a caller
    /// could not tell from "this family is empty". Reads outside `Kv` now fail
    /// closed with `CALYX_ASTER_CF_NOT_SELECTED`, and the name says which family
    /// you get at every call site. Use [`Self::open_existing_with_cfs`] for
    /// anything that touches `Base`, a slot CF, or `Assay`.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the vault directory, identity, machine
    /// salt, or read-only Aster recovery cannot be read.
    pub fn open_existing_kv_only(config: SynapseCalyxConfig) -> Result<Self, SynapseCalyxError> {
        Self::open_existing_with_cfs(config, Some(vec![ColumnFamily::Kv]))
    }

    /// Opens an existing Calyx vault for physical inspection of selected
    /// native Aster column families.
    ///
    /// This path is read-only and does not acquire the Synapse writer lock.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the vault directory, identity, machine
    /// salt, or read-only Aster recovery cannot be read.
    pub fn open_existing_with_cfs(
        config: SynapseCalyxConfig,
        selected_cfs: Option<Vec<ColumnFamily>>,
    ) -> Result<Self, SynapseCalyxError> {
        error_bridge::validate_calyx_error_bridge()?;
        let clock = SynapseCalyxClock::from_tuning(&config.tuning)?;
        if !config.vault_dir.is_dir() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READ_ONLY_VAULT_MISSING",
                format!(
                    "read-only Calyx vault inspection requires an existing directory: {}",
                    config.vault_dir.display()
                ),
                "point the inspector at an existing Calyx vault directory",
            ));
        }
        let identity = read_identity(&identity_path(&config.vault_dir))?;
        let vault_id = VaultId::from_str(&identity.vault_id).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_VAULT_ID_INVALID",
                format!(
                    "parse vault id {} from {}: {error}",
                    identity.vault_id,
                    identity_path(&config.vault_dir).display()
                ),
                IDENTITY_REMEDIATION,
            )
        })?;
        let machine_salt = read_machine_salt(&config.machine_salt_path)?;
        let options = VaultOptions {
            read_only: true,
            restore_mvcc_rows: false,
            // Candidate-bounded paging uses the SST reader's bounded sparse
            // index/Bloom path. Retaining every decoded SST index here made a
            // read-only inspection handle consume memory in proportion to the
            // complete physical history it was meant to inspect.
            eager_router_lookup_on_open: false,
            restore_ledger_hook: false,
            selected_cfs,
            ..VaultOptions::default()
        };
        let vault =
            AsterVault::open_with_clock(&config.vault_dir, vault_id, machine_salt, options, clock)
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("open read-only Calyx Aster vault", &error)
                })?;
        tracing::info!(
            code = "SYNAPSE_CALYX_READ_ONLY_VAULT_OPENED",
            vault_dir = %config.vault_dir.display(),
            vault_id = %vault.vault_id(),
            latest_seq = vault.latest_seq(),
            "opened read-only Calyx Aster vault for inspection"
        );
        Ok(Self { config, vault })
    }

    #[must_use]
    pub fn vault_dir(&self) -> &Path {
        &self.config.vault_dir
    }

    #[must_use]
    pub fn vault_id(&self) -> String {
        self.vault.vault_id().to_string()
    }

    #[must_use]
    pub fn vault_id_value(&self) -> VaultId {
        self.vault.vault_id()
    }

    #[must_use]
    pub fn latest_seq(&self) -> Seq {
        self.vault.latest_seq()
    }

    /// Returns the same millisecond clock source used by this opened vault.
    ///
    /// # Errors
    ///
    /// Returns a structured config error if the vault has an invalid fixed
    /// clock configuration after startup validation.
    pub fn clock_now_ms(&self) -> Result<Ts, SynapseCalyxError> {
        match self.config.tuning.clock_mode {
            SynapseCalyxClockMode::System => Ok(SystemClock.now()),
            SynapseCalyxClockMode::Fixed => {
                self.config
                    .tuning
                    .fixed_clock_unix_ms
                    .ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_CLOCK_INVALID",
                            "clock_mode=fixed is missing fixed_clock_unix_ms after validation",
                            "inspect the Calyx tuning config and restart after fixing the fixed clock fields",
                        )
                    })
            }
        }
    }

    /// Reads one raw CF row from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn read_cf_latest(
        &self,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.vault
            .read_cf_latest(cf, key)
            .map_err(|error| SynapseCalyxError::from_calyx("read latest Calyx CF row", &error))
    }

    /// Scans visible raw CF rows from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if a row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn scan_cf_latest(
        &self,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_latest(cf)
            .map_err(|error| SynapseCalyxError::from_calyx("scan latest Calyx CF", &error))
    }

    /// Scans a raw CF range from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if a row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn scan_cf_range_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_range_latest(cf, range)
            .map_err(|error| SynapseCalyxError::from_calyx("scan latest Calyx CF range", &error))
    }

    /// Reads one candidate-bounded raw CF page from an atomic latest view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error for an invalid range/cursor,
    /// blocked row, or unreadable physical serving view.
    pub fn scan_cf_range_page_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError> {
        self.vault
            .scan_cf_range_page_latest(cf, range, after_key, limit)
            .map(SynapseCalyxCfRangePage::from)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("scan latest Calyx CF range page", &error)
            })
    }

    /// Streams one range from one registered latest snapshot with one
    /// persistent immutable merge cursor.
    ///
    /// # Errors
    ///
    /// Returns a structured error when snapshot registration or validation,
    /// the physical stream, allocator accounting, or the visitor fails.
    pub fn walk_cf_range_latest_snapshot<V>(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        self.vault.with_scoped_latest_snapshot(
            Freshness::FreshDerived,
            INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |error| {
                SynapseCalyxError::from_calyx("pin the intelligence corpus walk snapshot", &error)
            },
            |snapshot| {
                walk_cf_range_snapshot_stream(&self.vault, snapshot, cf, range, page_rows, visit)
            },
        )
    }

    /// Reads one candidate-bounded range page through an existing pinned
    /// snapshot lease. The returned cursor names the last row included in the
    /// page; `more` is established with one bounded lookahead at the same
    /// pinned sequence.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the lease is expired or
    /// unavailable, the range/cursor is invalid, or the snapshot cannot be
    /// served by the opened recovery mode.
    pub fn scan_cf_range_page_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError> {
        if limit == 0 {
            return Ok(SynapseCalyxCfRangePage {
                snapshot_seq: snapshot.seq(),
                rows: Vec::new(),
                resume_after: None,
                more: false,
                examined_rows: 0,
            });
        }
        let candidate_limit = limit.checked_add(1).ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SNAPSHOT_PAGE_LIMIT_EXHAUSTED",
                "pinned Calyx page limit cannot be usize::MAX because continuation requires one lookahead row",
                "request a smaller bounded page",
            )
        })?;
        let mut rows = self
            .vault
            .scan_cf_range_page_snapshot(snapshot, cf, range, after_key, candidate_limit)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("scan pinned Calyx CF range page", &error)
            })?;
        let examined_rows = rows.len();
        let more = examined_rows > limit;
        if more {
            rows.truncate(limit);
        }
        let resume_after = rows.last().map(|(key, _value)| key.clone());
        Ok(SynapseCalyxCfRangePage {
            snapshot_seq: snapshot.seq(),
            rows,
            resume_after,
            more,
            examined_rows,
        })
    }

    /// Reads one raw CF row at a numeric snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the read-only handle cannot
    /// serve the requested snapshot/key.
    pub fn read_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.vault
            .read_cf_at(snapshot, cf, key)
            .map_err(|error| SynapseCalyxError::from_calyx("read Calyx CF row", &error))
    }

    /// Scans visible raw CF rows at a numeric snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the read-only handle cannot
    /// serve the requested snapshot/CF.
    pub fn scan_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_at(snapshot, cf)
            .map_err(|error| SynapseCalyxError::from_calyx("scan Calyx CF", &error))
    }

    /// Scans visible raw CF rows in a key range at a numeric snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the read-only handle cannot
    /// serve the requested snapshot/range.
    pub fn scan_cf_range_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_range_at(snapshot, cf, range)
            .map_err(|error| SynapseCalyxError::from_calyx("scan Calyx CF range", &error))
    }

    /// Decodes the physical `Anchors` CF rows currently visible for one
    /// constellation id.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the Anchors range cannot be
    /// read or any physical anchor row fails to decode.
    pub fn scan_anchors_for_cx(
        &self,
        cx_id: CxId,
    ) -> Result<Vec<SynapseCalyxAnchorReadback>, SynapseCalyxError> {
        scan_anchors_for_cx_from_vault(&self.vault, cx_id)
    }

    /// Reads one exact physical `Anchors` CF row by its canonical key.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the exact row cannot be
    /// read or its physical value fails to decode.
    pub fn read_anchor_exact(
        &self,
        cx_id: CxId,
        kind: &AnchorKind,
    ) -> Result<Option<SynapseCalyxAnchorReadback>, SynapseCalyxError> {
        read_anchor_exact_from_vault(&self.vault, cx_id, kind)
    }
}

/// Metadata key naming the authoritative source column family of a Base row.
///
/// Declared here as the literal the writer uses, because `synapse-storage`
/// depends on this crate and not the other way round.
const SYNAPSE_META_SOURCE_CF: &str = "synapse_source_cf";
/// Metadata key naming the hex-encoded authoritative source key of a Base row.
const SYNAPSE_META_SOURCE_KEY_HEX: &str = "synapse_source_key_hex";

/// Panel identity and authoritative source pointer read back from one Base row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxBaseSourcePointer {
    pub cx_id: String,
    pub panel_version: u32,
    /// `None` when the row records no source CF, which means it was not written
    /// by a Synapse source-row projection.
    pub source_cf: Option<String>,
    pub source_key_hex: Option<String>,
    /// Slot ids the Base row declares membership for.
    pub declared_slots: Vec<u16>,
}

impl SynapseCalyxVault {
    /// Publishes a complete, chunked `read` stream for one panel from its
    /// hash-sealed membership generation.
    ///
    /// Normal Base/slot commits keep flowing into the same durable CDC log
    /// while chunks are emitted.  Each chunk rechecks current Base presence
    /// under the commit lock, so a concurrent deletion cannot be resurrected.
    /// A consumer starts at `source_snapshot_seq` and drains through
    /// `through_seq`, receiving both the snapshot events and every concurrent
    /// real mutation in sequence order.
    ///
    /// # Errors
    ///
    /// Fails closed on an invalid panel/chunk bound, any malformed Base row,
    /// reader-lease failure, durable chunk failure, or sequence divergence.
    #[allow(
        clippy::too_many_lines,
        reason = "the exact snapshot walk, renewable lease, chunk commits, digest, and final report are one ordered bootstrap transaction"
    )]
    pub fn publish_panel_input_snapshot(
        &self,
        panel_version: u32,
        chunk_rows: usize,
    ) -> Result<SynapseCalyxPanelInputSnapshotReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("publish_panel_input_snapshot");
        if panel_version == 0
            || !(1..=calyx_aster::vault::PANEL_INPUT_SNAPSHOT_MAX_IDENTITIES).contains(&chunk_rows)
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_CHUNK_INVALID",
                format!(
                    "panel input snapshot requires panel_version>0 and chunk_rows in 1..={}; received panel_version={panel_version} chunk_rows={chunk_rows}",
                    calyx_aster::vault::PANEL_INPUT_SNAPSHOT_MAX_IDENTITIES
                ),
                "supply the exact registered panel and a bounded positive chunk size",
            ));
        }
        let (
            source_snapshot_seq,
            through_seq,
            identities,
            chunks,
            base_rows_scanned,
            membership_sha256,
            reader_lease_renewals,
        ) = self.with_read_snapshot(INTELLIGENCE_CORPUS_READER_LEASE_MS, |initial_snapshot| {
            let source_snapshot_seq = initial_snapshot.seq();
            let mut snapshot = initial_snapshot;
            let mut through_seq = source_snapshot_seq;
            let mut identities = 0usize;
            let mut chunks = 0usize;
            let mut base_rows_scanned = 0usize;
            let mut reader_lease_renewals = 0usize;
            let mut after_key = None::<Vec<u8>>;
            let mut pending = Vec::<CxId>::with_capacity(chunk_rows);
            let mut digest = Sha256::new();
            digest.update(b"synapse-calyx-panel-input-snapshot/v1");
            digest.update(panel_version.to_be_bytes());
            digest.update(source_snapshot_seq.to_be_bytes());

            loop {
                let page = self
                    .vault
                    .scan_cf_range_page_snapshot(
                        snapshot,
                        ColumnFamily::Base,
                        &KeyRange::all(),
                        after_key.as_deref(),
                        PANEL_BASE_SNAPSHOT_RENEW_ROWS,
                    )
                    .map_err(|error| {
                        SynapseCalyxError::from_calyx(
                            &format!(
                                "scan authoritative Base rows for panel {panel_version} association bootstrap"
                            ),
                            &error,
                        )
                    })?;
                if page.is_empty() {
                    break;
                }
                after_key = page.last().map(|(key, _)| key.clone());
                for (key, value) in page {
                    base_rows_scanned = base_rows_scanned.checked_add(1).ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_BASE_COUNT_OVERFLOW",
                            format!(
                                "panel {panel_version} authoritative Base row count overflowed usize"
                            ),
                            "preserve the vault and inspect the impossible Base cardinality",
                        )
                    })?;
                    let base = vault_encode::decode_constellation_base_projection(&value)
                        .map_err(|error| {
                            SynapseCalyxError::from_calyx(
                                &format!(
                                    "decode authoritative Base row while bootstrapping panel {panel_version}"
                                ),
                                &error,
                            )
                        })?;
                    if key.as_slice() != base.cx_id.as_bytes() {
                        return Err(SynapseCalyxError::new(
                            "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_BASE_ID_MISMATCH",
                            format!(
                                "panel {panel_version} bootstrap Base key (len={}) does not match payload {}",
                                key.len(),
                                base.cx_id
                            ),
                            "preserve and repair the malformed Base row; no recovery cursor was advanced",
                        ));
                    }
                    if base.panel_version != panel_version {
                        continue;
                    }
                    digest.update(base.cx_id.as_bytes());
                    identities = identities.checked_add(1).ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_IDENTITY_COUNT_OVERFLOW",
                            format!("panel {panel_version} identity count overflowed usize"),
                            "preserve the vault and inspect the impossible panel cardinality",
                        )
                    })?;
                    pending.push(base.cx_id);
                    if pending.len() == chunk_rows {
                        through_seq = publish_panel_input_snapshot_chunk_checked(
                            &self.vault,
                            panel_version,
                            &pending,
                            through_seq,
                            chunks + 1,
                        )?;
                        chunks = chunks.checked_add(1).ok_or_else(|| {
                            SynapseCalyxError::new(
                                "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_CHUNK_OVERFLOW",
                                format!(
                                    "panel {panel_version} snapshot chunk counter overflowed usize"
                                ),
                                "inspect the authoritative Base row count; no completion cursor was published",
                            )
                        })?;
                        pending.clear();
                    }
                }
                snapshot = self.renew_read_snapshot(snapshot)?;
                reader_lease_renewals = reader_lease_renewals.checked_add(1).ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_RENEWAL_OVERFLOW",
                        format!("panel {panel_version} reader renewal count overflowed usize"),
                        "preserve the vault and inspect the impossible scan duration",
                    )
                })?;
            }
            if !pending.is_empty() {
                through_seq = publish_panel_input_snapshot_chunk_checked(
                    &self.vault,
                    panel_version,
                    &pending,
                    through_seq,
                    chunks + 1,
                )?;
                chunks = chunks.checked_add(1).ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_CHUNK_OVERFLOW",
                        format!("panel {panel_version} snapshot chunk counter overflowed usize"),
                        "inspect the authoritative Base row count; no completion cursor was published",
                    )
                })?;
            }
            digest.update(identities.to_be_bytes());
            Ok((
                source_snapshot_seq,
                through_seq,
                identities,
                chunks,
                base_rows_scanned,
                lowercase_hex(&digest.finalize()),
                reader_lease_renewals,
            ))
        })?;
        tracing::info!(
            code = "SYNAPSE_CALYX_PANEL_INPUT_SNAPSHOT_PUBLISHED",
            panel_version,
            source_snapshot_seq,
            through_seq,
            identities,
            chunks,
            base_rows_scanned,
            membership_sha256 = %membership_sha256,
            reader_lease_renewals,
            "published a complete authoritative-Base association-input snapshot stream while ordinary change capture remained active"
        );
        Ok(SynapseCalyxPanelInputSnapshotReport {
            panel_version,
            source_snapshot_seq,
            through_seq,
            identities,
            chunks,
            base_rows_scanned,
            membership_sha256,
            reader_lease_renewals,
        })
    }

    /// Physically tombstones a bounded page of durable panel-input events only
    /// after their consumer acknowledgement has been committed and reread.
    ///
    /// # Errors
    ///
    /// Returns the exact Calyx error when the acknowledgement boundary is
    /// invalid or any tombstone commit/readback fails.
    pub fn prune_panel_input_changes(
        &self,
        panel_version: u32,
        through_seq: u64,
        mutation_through_seq: u64,
        max_rows: usize,
    ) -> Result<SynapseCalyxPanelInputPruneReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("prune_panel_input_changes");
        let report = self
            .vault
            .prune_panel_input_changes(
                panel_version,
                through_seq,
                mutation_through_seq,
                max_rows,
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!(
                        "prune panel {panel_version} association-input changes through durable cursor {through_seq}"
                    ),
                    &error,
                )
            })?;
        Ok(SynapseCalyxPanelInputPruneReport {
            panel_version: report.panel_version,
            through_seq: report.through_seq,
            rows_deleted: report.rows_deleted,
            committed_seq: report.committed_seq,
            mutation_floor_seq: report.mutation_floor_seq,
        })
    }

    /// Returns the fail-closed completeness state of the native ordered
    /// `(panel, source_event_ns, cx_id)` secondary index.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx error when the panel is invalid or the
    /// physical completeness marker is malformed or unreadable.
    pub fn event_time_index_status(
        &self,
        panel_version: u32,
    ) -> Result<EventTimeIndexStatus, SynapseCalyxError> {
        self.vault
            .event_time_index_status(panel_version)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("read panel {panel_version} event-time index status"),
                    &error,
                )
            })
    }

    /// Populates historical event-time rows resumably and publishes
    /// completeness only after an independent Base-vs-IndexBtree
    /// reconciliation at one exact panel watermark.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx error when Base/index bytes disagree,
    /// temporal metadata is malformed, the panel keeps changing through all
    /// reconciliation attempts, or durable publication fails.
    pub fn ensure_event_time_index(
        &self,
        panel_version: u32,
    ) -> Result<EventTimeIndexBackfill, SynapseCalyxError> {
        self.vault
            .backfill_event_time_index(panel_version)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("backfill panel {panel_version} event-time index"),
                    &error,
                )
            })
    }

    /// Reads the panel identity and source-row pointer recorded on one Base row.
    ///
    /// This is the exact evidence an exact-match confirmation needs (#1899): a
    /// hash-lane hit names a `cx_id`, and only the authoritative source field can
    /// distinguish a true whole-value match from a bucket collision. Reads the
    /// Base row alone — no slot hydration — because the confirmation compares
    /// source bytes, not vectors.
    ///
    /// `Ok(None)` means no Base row is visible for the id at the pinned snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the Base row cannot be read
    /// or decoded, or when the decoded row's identity differs from the requested
    /// `cx_id`.
    pub fn read_base_source_pointer(
        &self,
        cx_id: CxId,
    ) -> Result<Option<SynapseCalyxBaseSourcePointer>, SynapseCalyxError> {
        let snapshot = self.vault.snapshot();
        let Some(bytes) = self
            .vault
            .read_cf_at(snapshot, ColumnFamily::Base, cx_id.as_bytes())
            .map_err(|error| {
                SynapseCalyxError::from_calyx(&format!("read Base row {cx_id}"), &error)
            })?
        else {
            return Ok(None);
        };
        let constellation =
            calyx_aster::vault::encode::decode_constellation_base(&bytes).map_err(|error| {
                SynapseCalyxError::from_calyx(&format!("decode Base row {cx_id}"), &error)
            })?;
        if constellation.cx_id != cx_id {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_BASE_ROW_IDENTITY_MISMATCH",
                format!(
                    "Base row keyed {cx_id} decodes to constellation {}",
                    constellation.cx_id
                ),
                "the Base keyspace is corrupt for this id; reconcile the physical shard before trusting any read of it",
            ));
        }
        Ok(Some(SynapseCalyxBaseSourcePointer {
            cx_id: cx_id.to_string(),
            panel_version: constellation.panel_version,
            source_cf: constellation.metadata.get(SYNAPSE_META_SOURCE_CF).cloned(),
            source_key_hex: constellation
                .metadata
                .get(SYNAPSE_META_SOURCE_KEY_HEX)
                .cloned(),
            declared_slots: constellation.slots.keys().map(|slot| slot.get()).collect(),
        }))
    }

    /// Pins one read snapshot sequence for a whole intelligence pass.
    ///
    /// Every record in one pass must be hydrated against the same snapshot, or
    /// the corpus mixes sequences and the derived result describes no single
    /// state of the vault.
    pub(crate) fn read_snapshot(&self) -> u64 {
        self.vault.snapshot()
    }

    /// Runs a multi-read intelligence operation against one registered latest
    /// reader lease. The lease is released by Calyx on every exit path.
    pub(crate) fn with_read_snapshot<T>(
        &self,
        max_age_ms: u64,
        read: impl FnOnce(Snapshot) -> Result<T, SynapseCalyxError>,
    ) -> Result<T, SynapseCalyxError> {
        self.vault.with_scoped_latest_snapshot(
            Freshness::FreshDerived,
            max_age_ms,
            |error| SynapseCalyxError::from_calyx("pin the scoped Calyx read snapshot", &error),
            read,
        )
    }

    /// Runs a panel-selective read against one atomic `(seq, panel watermark)`
    /// snapshot. The Aster scoped handle releases the lease on success, error,
    /// or unwind.
    pub(crate) fn with_panel_read_snapshot<T>(
        &self,
        panel_version: u32,
        max_age_ms: u64,
        read: impl FnOnce(Snapshot) -> Result<T, SynapseCalyxError>,
    ) -> Result<T, SynapseCalyxError> {
        self.vault.with_scoped_latest_snapshot_for_panel(
            panel_version,
            Freshness::FreshDerived,
            max_age_ms,
            |error| {
                SynapseCalyxError::from_calyx(
                    &format!("pin the scoped panel {panel_version} Calyx read snapshot"),
                    &error,
                )
            },
            read,
        )
    }

    /// Renews a still-live scoped reader at the same exact pinned sequence.
    fn renew_read_snapshot(&self, snapshot: Snapshot) -> Result<Snapshot, SynapseCalyxError> {
        self.vault.renew_reader(snapshot).map_err(|error| {
            SynapseCalyxError::from_calyx(
                &format!(
                    "renew scoped Calyx reader lease {} at pinned seq {}",
                    snapshot.lease().id(),
                    snapshot.seq()
                ),
                &error,
            )
        })
    }

    fn panel_membership_at_snapshot(
        &self,
        snapshot: Snapshot,
        panel_version: u32,
    ) -> Result<calyx_search::ReconciledPanelMembership, SynapseCalyxError> {
        let generation =
            calyx_search::PersistedSearchIndexes::open(&self.config.vault_dir, panel_version)
                .map_err(|error| {
                    search_rebuild_error(
                        &format!("open panel {panel_version} membership generation"),
                        error,
                    )
                })?;
        calyx_search::reconcile_panel_membership(&self.vault, &generation, snapshot).map_err(
            |error| {
                search_rebuild_error(
                    &format!(
                        "reconcile panel {panel_version} membership to pinned snapshot {}",
                        snapshot.seq()
                    ),
                    error,
                )
            },
        )
    }

    fn verified_panel_base_row_at_snapshot(
        &self,
        snapshot: Snapshot,
        panel_version: u32,
        cx_id: CxId,
    ) -> Result<Vec<u8>, SynapseCalyxError> {
        let value = self
            .vault
            .read_cf_snapshot(snapshot, ColumnFamily::Base, cx_id.as_bytes())
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!(
                        "read panel {panel_version} membership Base row {cx_id} at snapshot {}",
                        snapshot.seq()
                    ),
                    &error,
                )
            })?
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_PANEL_MEMBERSHIP_ROW_MISSING",
                    format!(
                        "panel {panel_version} membership names Base row {cx_id}, but it is absent at snapshot {}",
                        snapshot.seq()
                    ),
                    "rebuild the exact panel generation and verify the Base CF before retrying",
                )
            })?;
        let base = calyx_aster::vault::encode::decode_constellation_base_projection(&value)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("decode panel membership Base row {cx_id}"),
                    &error,
                )
            })?;
        if base.cx_id != cx_id || base.panel_version != panel_version {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_MEMBERSHIP_ROW_MISMATCH",
                format!(
                    "panel {panel_version} membership identity {cx_id} read Base header cx_id={} panel_version={} at snapshot {}",
                    base.cx_id,
                    base.panel_version,
                    snapshot.seq()
                ),
                "rebuild the exact panel generation and repair the mismatched Base row before retrying",
            ));
        }
        Ok(value)
    }

    /// Walks only one panel's Base identities through its hash-sealed
    /// persistent membership sidecar, then point-reads each selected Base row
    /// at the caller's registered snapshot.
    ///
    /// The immutable sidecar is reconciled to the atomically pinned panel
    /// content watermark through the same bounded, panel-scoped Base delta as a
    /// search query. Missing, over-bound, corrupt, or cross-panel identities
    /// fail closed. There is intentionally no global Base scan fallback: that
    /// would recreate the unbounded cross-panel work this access path exists to
    /// eliminate.
    pub(crate) fn walk_panel_base_snapshot<V>(
        &self,
        mut snapshot: Snapshot,
        panel_version: u32,
        mut visit: V,
    ) -> Result<(SynapseCalyxPanelBaseWalk, Snapshot), SynapseCalyxError>
    where
        V: FnMut(Snapshot, &[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        let membership = self.panel_membership_at_snapshot(snapshot, panel_version)?;
        let indexed_rows = membership.ids.len();
        let mut rows_visited = 0usize;
        let mut stopped_early = false;
        let reader_lease_initial_expires_at = snapshot.lease().expires_at();
        let mut reader_lease_renewals = 0usize;
        for cx_id in membership.ids {
            if rows_visited > 0 && rows_visited.is_multiple_of(PANEL_BASE_SNAPSHOT_RENEW_ROWS) {
                snapshot = self.renew_read_snapshot(snapshot)?;
                reader_lease_renewals = reader_lease_renewals.checked_add(1).ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_READER_LEASE_RENEWAL_COUNT_OVERFLOW",
                        format!(
                            "panel {panel_version} reader lease renewal count overflowed usize"
                        ),
                        "preserve the vault and inspect the bounded panel-walk progress counter",
                    )
                })?;
            }
            let value = self.verified_panel_base_row_at_snapshot(snapshot, panel_version, cx_id)?;
            rows_visited = rows_visited.checked_add(1).ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_PANEL_MEMBERSHIP_COUNT_OVERFLOW",
                    format!("panel {panel_version} membership visit count overflowed usize"),
                    "inspect the membership manifest row count and repair the corrupt generation",
                )
            })?;
            if visit(snapshot, cx_id.as_bytes(), &value)? == SynapseCalyxWalkStep::Stop {
                stopped_early = true;
                break;
            }
        }
        let report = SynapseCalyxPanelBaseWalk {
            panel_version,
            membership_base_seq: membership.base_seq,
            membership_covered_to_seq: membership.covered_to_seq,
            snapshot_seq: snapshot.seq(),
            panel_content_seq: snapshot.derived_content_seq(),
            manifest_sha256: membership.manifest_sha256,
            sidecar_sha256: membership.sidecar_sha256,
            sidecar_rows: membership.sidecar_rows,
            reconciled_changed_keys: membership.changed_keys,
            indexed_rows,
            rows_visited,
            stopped_early,
            reader_lease_renewals,
            reader_lease_initial_expires_at,
            reader_lease_final_expires_at: snapshot.lease().expires_at(),
        };
        tracing::info!(
            code = "SYNAPSE_CALYX_PANEL_BASE_WALK_COMPLETED",
            panel_version = report.panel_version,
            membership_base_seq = report.membership_base_seq,
            membership_covered_to_seq = report.membership_covered_to_seq,
            snapshot_seq = report.snapshot_seq,
            panel_content_seq = report.panel_content_seq,
            manifest_sha256 = %report.manifest_sha256,
            sidecar_sha256 = %report.sidecar_sha256,
            sidecar_rows = report.sidecar_rows,
            reconciled_changed_keys = report.reconciled_changed_keys,
            indexed_rows = report.indexed_rows,
            rows_visited = report.rows_visited,
            stopped_early = report.stopped_early,
            reader_lease_renewals = report.reader_lease_renewals,
            reader_lease_initial_expires_at = report.reader_lease_initial_expires_at,
            reader_lease_final_expires_at = report.reader_lease_final_expires_at,
            "completed a freshness-proven panel-selective Base walk"
        );
        Ok((report, snapshot))
    }

    /// Reads one constellation with its slot vectors hydrated from the per-slot
    /// CFs (issue #1894).
    ///
    /// **A `Base` row does not contain slot vectors.** It stores
    /// `(slot_id, slot_hash)` pairs, and `decode_constellation_base` therefore
    /// yields `SlotVector::Absent { NotApplicable }` for *every* slot. Treating a
    /// Base-decoded constellation as if it carried measurements silently produces
    /// an empty lens set: it is not an error, no row is missing, and every
    /// downstream count is a truthful zero over nothing. That is what made
    /// `weave`/`abundance`/`bits`/`redundancy` report `n_lenses=0` and `kernel`
    /// report `0 embedded concepts` on a panel whose records carry twelve
    /// measured content lenses.
    ///
    /// Any caller that needs vectors — as opposed to anchors, scalars, metadata
    /// or slot *ids* — must go through this, not through the Base row.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the row or one of its slot
    /// CF rows cannot be read at `snapshot`.
    pub(crate) fn hydrated_constellation(
        &self,
        cx_id: CxId,
        snapshot: u64,
    ) -> Result<Constellation, SynapseCalyxError> {
        self.vault.get(cx_id, snapshot).map_err(|error| {
            SynapseCalyxError::from_calyx(
                &format!("hydrate slot vectors for constellation {cx_id}"),
                &error,
            )
        })
    }

    /// Hydrates one constellation through an already-registered snapshot
    /// lease, preserving the caller's exact multi-read MVCC view.
    pub(crate) fn hydrated_constellation_at_snapshot(
        &self,
        cx_id: CxId,
        snapshot: Snapshot,
    ) -> Result<Constellation, SynapseCalyxError> {
        self.vault
            .get_at_snapshot(cx_id, snapshot)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("hydrate slot vectors for constellation {cx_id}"),
                    &error,
                )
            })
    }

    /// Reads one latest physical Base row and hydrates every declared Slot CF
    /// vector. Lifecycle workers use this as the independent post-write source
    /// of truth before completing a durable backfill task.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Base row or any declared slot vector
    /// cannot be read and decoded at the latest snapshot.
    pub fn hydrate_constellation_latest(
        &self,
        cx_id: CxId,
    ) -> Result<Constellation, SynapseCalyxError> {
        self.hydrated_constellation(cx_id, self.read_snapshot())
    }
    /// Atomically replaces only the temporal metadata fields on one legacy
    /// Base row after exact panel and source-identity verification.
    ///
    /// # Errors
    ///
    /// Returns a typed Calyx error when the Base row is absent, its panel or
    /// source identity differs, temporal metadata is invalid, or the atomic
    /// Base-and-Ledger commit/readback fails.
    pub fn backfill_temporal_metadata(
        &self,
        cx_id: CxId,
        expected_panel_version: u32,
        expected_identity: &BTreeMap<String, String>,
        expected_temporal: &BTreeMap<String, String>,
    ) -> Result<TemporalMetadataMigration, SynapseCalyxError> {
        self.vault
            .backfill_temporal_metadata(
                cx_id,
                expected_panel_version,
                expected_identity,
                expected_temporal,
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx("backfill native Calyx temporal metadata", &error)
            })
    }

    /// Atomically replaces only the temporal metadata fields on an ordered set
    /// of legacy Base rows after exact panel and source-identity verification.
    ///
    /// Already-current rows remain read-only outcomes. Every changed row is
    /// stamped by one declared batch ledger entry and becomes visible through
    /// the same MVCC/WAL commit.
    ///
    /// # Errors
    ///
    /// Returns a typed Calyx error before mutation when any Base row is absent,
    /// duplicated in the request, or differs from its authoritative panel or
    /// identity. Durable commit and ledger-hook failures retain their exact
    /// reconciliation-required error contract.
    pub fn backfill_temporal_metadata_batch<I>(
        &self,
        requests: I,
    ) -> Result<Vec<TemporalMetadataMigration>, SynapseCalyxError>
    where
        I: IntoIterator<Item = TemporalMetadataBackfill>,
    {
        self.vault
            .backfill_temporal_metadata_batch(requests)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "backfill native Calyx temporal metadata batch",
                    &error,
                )
            })
    }

    /// Per-CF memtable ceiling handed to Aster's router.
    ///
    /// Aster's own default is 1 MiB (`DEFAULT_MEMTABLE_BYTES`), and Synapse
    /// never overrode it, so every column family ran with a 0.75-1.0 MiB cap
    /// after the deterministic per-CF stagger. Synapse writes rows far larger
    /// than that: a single observed causal-map row was 7,428,289 bytes against
    /// a cap of 891,301. A row that cannot fit its own memtable cannot be
    /// admitted at all, so writes backed up until `base` held its maximum two
    /// sealed memtables, the next flush was refused
    /// (CALYX_ASTER_ROUTER_SEALED_MEMTABLE_BACKLOG), restoring the sealed
    /// memtable then failed too (CALYX_BACKPRESSURE, "memtable byte cap 859846
    /// exceeded by projected 862164 bytes"), and the shard was declared
    /// corrupt. The vault then re-opened and replayed manifested durable
    /// batches, which drove private commit from ~460 MB to 5.7 GB in seconds,
    /// hit the Job process cap and killed the daemon - about every five
    /// minutes, indefinitely, because each restart re-entered the same state.
    ///
    /// 16 MiB clears the largest row observed on this corpus with roughly 2x
    /// headroom. This is a ceiling, not an allocation: a memtable only grows
    /// with the rows actually written to that CF, and only the few hot CFs
    /// approach it, so steady-state residency stays in the tens of megabytes.
    const SYNAPSE_MEMTABLE_BYTE_CAP: usize = 16 * 1024 * 1024;

    /// Opens the configured durable Aster vault after acquiring the Synapse
    /// process lock and loading the stable vault identity.
    ///
    /// # Errors
    ///
    /// Returns an error when directories, identity files, the machine-local
    /// salt, the single-instance lock, or Calyx recovery/open fail.
    pub fn open(config: SynapseCalyxConfig) -> Result<Self, SynapseCalyxError> {
        let options = VaultOptions {
            memtable_byte_cap: Self::SYNAPSE_MEMTABLE_BYTE_CAP,
            ..VaultOptions::default()
        };
        Self::open_with_mode(config, &options, SynapseCalyxVaultOpenMode::FullMvccRestore)
    }

    /// Opens the configured durable Aster vault for latest-state reads/writes.
    ///
    /// This mode uses Aster's router as the latest-state Source of Truth and
    /// does not reconstruct every historical MVCC row at startup. Calls that
    /// request historical snapshots still fail closed inside Aster.
    ///
    /// # Errors
    ///
    /// Returns an error when directories, identity files, the machine-local
    /// salt, the single-instance lock, or Calyx recovery/open fail.
    pub fn open_latest_readback(config: SynapseCalyxConfig) -> Result<Self, SynapseCalyxError> {
        let options = VaultOptions {
            restore_mvcc_rows: false,
            eager_router_lookup_on_open: false,
            memtable_byte_cap: Self::SYNAPSE_MEMTABLE_BYTE_CAP,
            ..VaultOptions::default()
        };
        Self::open_with_mode(config, &options, SynapseCalyxVaultOpenMode::LatestReadback)
    }

    #[allow(clippy::too_many_lines)]
    fn open_with_mode(
        config: SynapseCalyxConfig,
        options: &VaultOptions,
        open_mode: SynapseCalyxVaultOpenMode,
    ) -> Result<Self, SynapseCalyxError> {
        error_bridge::validate_calyx_error_bridge()?;
        let mut options = options.clone();
        let recovery_vault_dir = config.vault_dir.clone();
        options.recovery_progress = Some(RecoveryProgressHook::new(
            move |bytes_replayed, bytes_total| {
                tracing::info!(
                    code = "SYNAPSE_CALYX_WAL_RECOVERY_PROGRESS",
                    vault_dir = %recovery_vault_dir.display(),
                    bytes_replayed,
                    bytes_total,
                    "Calyx WAL recovery made physical byte progress"
                );
            },
        ));
        let clock = SynapseCalyxClock::from_tuning(&config.tuning)?;
        let started_at = Instant::now();
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_OPEN_START",
            vault_dir = %config.vault_dir.display(),
            open_mode = open_mode.as_str(),
            restore_mvcc_rows = options.restore_mvcc_rows,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            read_only = options.read_only,
            "opening Synapse Calyx vault"
        );
        create_dir_all(&config.vault_dir)?;
        // Classify the directory BEFORE taking the writer lock or writing a
        // vault identity into it. Opening a legacy RocksDB store used to get as
        // far as creating `vault.lock` and `vault-identity.json` inside that
        // foreign directory and only then fail deep inside Aster recovery with
        // `CALYX_ASTER_CORRUPT_SHARD: CURRENT does not point at immutable
        // manifest file` and `remediation=restore from restic/snapshot`. Both
        // halves were wrong: nothing was corrupt, and restoring a backup of a
        // RocksDB store cannot produce a Calyx vault. Detect and say so exactly,
        // without mutating the directory.
        detect_foreign_store(&config.vault_dir)?;
        create_parent_dir(&config.machine_salt_path)?;
        let lock = VaultLockGuard::acquire(&config.vault_dir)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_LOCK_ACQUIRED",
            vault_dir = %config.vault_dir.display(),
            lock_path = %lock.path.display(),
            pid_path = %lock.pid_path.display(),
            pid = std::process::id(),
            "acquired Synapse Calyx vault writer lock"
        );
        let identity = match load_or_create_identity(&config) {
            Ok(identity) => identity,
            Err(error) => return Err(cleanup_open_lock(lock, error)),
        };
        let vault_id = match identity.parse_vault_id() {
            Ok(vault_id) => vault_id,
            Err(error) => return Err(cleanup_open_lock(lock, error)),
        };
        tracing::info!(
            code = "SYNAPSE_CALYX_ASTER_OPEN_START",
            vault_dir = %config.vault_dir.display(),
            vault_id = %vault_id,
            open_mode = open_mode.as_str(),
            restore_mvcc_rows = options.restore_mvcc_rows,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            read_only = options.read_only,
            "opening durable Calyx Aster vault"
        );
        let vault = match AsterVault::open_with_clock(
            &config.vault_dir,
            vault_id,
            identity.machine_salt,
            options.clone(),
            clock,
        ) {
            Ok(vault) => vault,
            Err(error) => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_ASTER_OPEN_FAILED",
                    vault_dir = %config.vault_dir.display(),
                    vault_id = %vault_id,
                    open_mode = open_mode.as_str(),
                    restore_mvcc_rows = options.restore_mvcc_rows,
                    eager_router_lookup_on_open = options.eager_router_lookup_on_open,
                    read_only = options.read_only,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    error = %error,
                    "durable Calyx Aster vault open failed"
                );
                return Err(cleanup_open_lock(
                    lock,
                    SynapseCalyxError::from_calyx("open durable Calyx Aster vault", &error),
                ));
            }
        };
        // Compare the vault that is physically present against the lineage
        // journal kept OUTSIDE the vault directory, before any caller can treat
        // this open as normal. A vault whose substrate was replaced must never
        // open silently (#1875).
        let lineage = {
            let now_unix_ms = match SynapseCalyxClock::from_tuning(&config.tuning) {
                Ok(clock) => clock.now(),
                Err(error) => return Err(cleanup_open_lock(lock, error)),
            };
            match lineage::evaluate_and_record(
                &config.vault_dir,
                &vault.vault_id().to_string(),
                vault.latest_seq(),
                now_unix_ms,
                lineage::acknowledgement_from_env().as_deref(),
                if identity.created_this_open {
                    lineage::VaultOpenGenesis::CreatedThisOpen
                } else {
                    lineage::VaultOpenGenesis::PreExisting
                },
            ) {
                Ok(lineage) => lineage,
                Err(error) => {
                    drop(vault);
                    return Err(cleanup_open_lock(lock, error));
                }
            }
        };
        let math_runtime = match math_backend(&config.tuning) {
            Ok(runtime) => runtime,
            Err(error) => return Err(cleanup_open_lock(lock, error)),
        };
        let math_status = math_runtime.status_snapshot();
        let assay_compute_backend = match math_status.selected_backend.as_str() {
            "cpu" => AssayComputeBackend::Cpu,
            "cuda" => AssayComputeBackend::Cuda,
            selected => {
                let error = SynapseCalyxError::new(
                    "SYNAPSE_CALYX_ASSAY_BACKEND_UNKNOWN",
                    format!(
                        "Synapse selected math backend {selected:?}, which cannot be mapped to a Calyx Assay execution backend"
                    ),
                    "repair Synapse math backend selection so it resolves to exactly cpu or cuda before opening the Calyx vault",
                );
                drop(math_runtime);
                drop(vault);
                return Err(cleanup_open_lock(lock, error));
            }
        };
        if let Err(error) = configure_compute_backend(assay_compute_backend) {
            let error = SynapseCalyxError::from_calyx(
                "configure the process-wide Calyx Assay compute backend",
                &error,
            );
            drop(math_runtime);
            drop(vault);
            return Err(cleanup_open_lock(lock, error));
        }
        tracing::info!(
            code = "SYNAPSE_CALYX_ASSAY_BACKEND_CONFIGURED",
            requested_backend = math_status.requested_backend.as_str(),
            selected_backend = math_status.selected_backend.as_str(),
            assay_compute_backend = assay_compute_backend.as_str(),
            "configured every generic Calyx Assay estimator to use the selected Synapse serving backend"
        );
        let opened = Self {
            config,
            vault,
            anneal_ledger_index: std::sync::Mutex::default(),
            lock,
            math_runtime,
            open_mode,
            lineage,
            cf_count_memo: std::sync::Mutex::default(),
            guard_serving_memo: std::sync::Mutex::default(),
        };
        opened.initialize_anneal_tuning()?;
        let math_status = opened.math_runtime.status_snapshot();
        let status = status_from_vault(
            &opened.config,
            &opened.vault,
            &math_status,
            opened.open_mode,
        );
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_OPENED",
            vault_dir = %opened.config.vault_dir.display(),
            open_mode = opened.open_mode.as_str(),
            restore_mvcc_rows = options.restore_mvcc_rows,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            lock_path = %opened.lock.path.display(),
            pid_path = %opened.lock.pid_path.display(),
            vault_id = status.vault_id.as_deref().unwrap_or(""),
            latest_seq = status.latest_seq,
            last_recovered_seq = status.last_recovered_seq,
            torn_tail = status.torn_tail.as_deref().unwrap_or("none"),
            elapsed_ms = started_at.elapsed().as_millis(),
            clock_mode = ?opened.config.tuning.clock_mode,
            fixed_clock_unix_ms = opened.config.tuning.fixed_clock_unix_ms,
            rng_seed = opened.config.tuning.rng_seed,
            math_backend_requested = status
                .math_backend
                .as_ref()
                .map_or("none", |math| math.requested_backend.as_str()),
            math_backend_selected = status
                .math_backend
                .as_ref()
                .map_or("none", |math| math.selected_backend.as_str()),
            math_backend_device_name = status
                .math_backend
                .as_ref()
                .map_or("none", |math| math.device_name.as_str()),
            math_backend_device_vram_mib = status
                .math_backend
                .as_ref()
                .and_then(|math| math.device_vram_mib),
            math_backend_fallback_code = status
                .math_backend
                .as_ref()
                .and_then(|math| math.fallback_code.as_deref())
                .unwrap_or("none"),
            math_backend_probe_status = status
                .math_backend
                .as_ref()
                .map_or("none", |math| math.probe.status.as_str()),
            lineage_path = %opened.lineage.lineage_path.display(),
            vault_generation = opened.lineage.generation,
            vault_lineage_reset_count = opened.lineage.reset_count,
            chain_origin = %opened.lineage.chain_origin,
            "opened durable Calyx Aster vault"
        );
        Ok(opened)
    }

    /// Lineage of the vault directory this handle has open: which generation it
    /// is, how many recorded replacements precede it, and therefore how much
    /// history a verified chain can honestly claim to cover.
    #[must_use]
    pub const fn lineage(&self) -> &SynapseCalyxVaultLineage {
        &self.lineage
    }

    #[must_use]
    pub fn vault_id(&self) -> String {
        self.vault.vault_id().to_string()
    }

    #[must_use]
    pub fn vault_id_value(&self) -> VaultId {
        self.vault.vault_id()
    }

    #[must_use]
    pub fn latest_seq(&self) -> Seq {
        self.vault.latest_seq()
    }

    /// Reads the persisted search generation's state for the active panel,
    /// without building or mutating anything (issue #1891).
    ///
    /// Every recall path depends on this generation, and before this nothing
    /// announced its state: an absent generation, a staked rebuild marker, or a
    /// generation lagging past the bounded reconciliation limit were all silent
    /// until an operator called `find` and read the failure. This reports the
    /// facts — which lanes are built, over how many rows, at which sequence, how
    /// long ago — so `health` can raise it as a named deficiency instead.
    ///
    /// It never fails on an *absent* generation: absence is the state being
    /// reported, not an error in reporting it. It does fail when the vault's own
    /// panel state or a staked marker cannot be read, because those mean the
    /// reported state itself would be a guess.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the rebuild-required marker exists but
    /// cannot be parsed, or when the manifest exists but cannot be hashed.
    pub fn search_generation_status(
        &self,
    ) -> Result<SynapseCalyxSearchGenerationStatus, SynapseCalyxError> {
        let vault_dir = self.config.vault_dir.as_path();
        let (panel_version, panel_state_error) = match load_vault_panel_state(vault_dir) {
            Ok(state) => (Some(state.panel.version), None),
            Err(error) if error.code == CALYX_NO_ACTIVE_PANEL => (
                None,
                Some(format!(
                    "no active durable panel is published, so no search generation can exist: {}",
                    error.message
                )),
            ),
            Err(error) => (None, Some(format!("{}: {}", error.code, error.message))),
        };
        let Some(panel_version) = panel_version else {
            return Ok(search_generation_status_without_panel(
                panel_state_error,
                self.vault.latest_seq(),
                calyx_search::MAX_RECONCILED_DELTA_KEYS as u64,
            ));
        };
        self.search_generation_status_for_panel(panel_version, false)
    }

    /// Counts the distinct constellations changed since `base_seq` for exactly
    /// one panel, across the `Base` CF and the generation's own slot CFs.
    ///
    /// This follows the *same* control law as the query path's delta collector:
    /// pin the exact panel watermark with the read sequence, prove an empty
    /// delta without consulting history when that watermark is at or before the
    /// generation base, and otherwise call the same
    /// `calyx_search::measure_panel_delta`. The watermark gate matters after an
    /// Aster snapshot-delta rebase: generic changed-key history may begin after
    /// `base_seq` even though this exact panel provably did not change. Treating
    /// that state as stale made a just-rebuilt generation fail its own readback
    /// because of unrelated vault commits.
    ///
    /// A measured delta is a distinct-key count, not a sum, because the same
    /// constellation changing in the Base CF and in three slot CFs is one key to
    /// reconcile, not four. The `Base` share is scoped to `panel_version`
    /// (#1901): `Base` is shared by every panel, and counting all of it charged a
    /// 329-row timeline generation for 17,785 keys of unrelated
    /// agent-transcript ingest.
    fn measure_search_delta_changed_keys_at_snapshot(
        &self,
        snapshot: Snapshot,
        panel_version: u32,
        base_seq: u64,
        slots: &[SynapseCalyxSearchGenerationSlot],
    ) -> Result<calyx_search::PanelDeltaComposition, SynapseCalyxError> {
        let measured = if snapshot.derived_content_seq() <= base_seq {
            Ok(calyx_search::PanelDeltaComposition {
                panel_version,
                base_seq,
                pinned_seq: snapshot.seq(),
                ..calyx_search::PanelDeltaComposition::default()
            })
        } else {
            calyx_search::measure_panel_delta(
                &self.vault,
                snapshot,
                panel_version,
                base_seq,
                slots.iter().map(|slot| calyx_core::SlotId::new(slot.slot)),
            )
        };
        measured.map_err(|error| {
            search_rebuild_error(
                &format!(
                    "measure the panel {panel_version} search-generation delta after seq {base_seq}"
                ),
                error,
            )
        })
    }

    /// The generation report for **one named panel**, whether or not that
    /// panel is the vault's active one (issue #1938).
    ///
    /// Every read here is already panel-scoped on disk
    /// (`idx/search/panel_{version:010}/`) and in the delta measurement, so
    /// naming the panel is the whole difference. What the active-panel entry
    /// points add on top is resolving *which* version to ask about; that
    /// resolution is not part of reading a generation's state, and conflating
    /// the two is why a non-active generation's state was unreportable and
    /// therefore unmaintained.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the rebuild-required marker exists but
    /// cannot be parsed, when the manifest exists but cannot be hashed, or when
    /// a requested delta measurement fails.
    #[allow(
        clippy::too_many_lines,
        reason = "one status read over one pinned view; splitting it would let the manifest facts and the delta measurement come from different snapshots, which is the class of drift this surface exists to report"
    )]
    pub fn search_generation_status_for_panel(
        &self,
        panel_version: u32,
        measure_delta: bool,
    ) -> Result<SynapseCalyxSearchGenerationStatus, SynapseCalyxError> {
        self.with_panel_read_snapshot(panel_version, SEARCH_DELTA_SCAN_LEASE_MS, |snapshot| {
            let vault_dir = self.config.vault_dir.as_path();
            let vault_latest_seq = snapshot.seq();
            let panel_content_seq = snapshot.derived_content_seq();
            let max_reconciled_delta_keys = calyx_search::MAX_RECONCILED_DELTA_KEYS as u64;
            let panel_state_error = None;

            let manifest = calyx_search::manifest_path(vault_dir, panel_version);
            let manifest_present = manifest.is_file();
            let manifest_sha256 = read_optional_sha256(&manifest)?;
            let rebuild_required =
                calyx_search::read_rebuild_required_marker(vault_dir, panel_version)
                    .map_err(|error| {
                        search_rebuild_error(
                            "read rebuild-required marker for generation status",
                            error,
                        )
                    })?
                    .map(|marker| format!("source={} detail={}", marker.source, marker.detail));

            let (built_at_seq, rows_covered, slots) = if manifest_present {
                match calyx_search::PersistedSearchIndexes::open(vault_dir, panel_version)
                    .and_then(|indexes| indexes.generation())
                {
                    Ok(generation) => {
                        let slots: Vec<SynapseCalyxSearchGenerationSlot> = generation
                            .slots
                            .iter()
                            .map(|slot| SynapseCalyxSearchGenerationSlot {
                                slot: slot.panel_slot.slot_id().get(),
                                kind: slot.kind.clone(),
                                lane: search_slot_lane(&slot.kind).to_owned(),
                                len: slot.len as u64,
                                built_at_seq: slot.built_at_seq,
                            })
                            .collect();
                        let rows = slots.iter().map(|slot| slot.len).max();
                        (Some(generation.base_seq), rows, slots)
                    }
                    // The manifest exists but does not describe a usable generation
                    // (wrong format, wrong panel, malformed slot shape). That is a
                    // reportable state, not a reason to fail the health read.
                    Err(_) => (None, None, Vec::new()),
                }
            } else {
                (None, None, Vec::new())
            };

            let dense_slot_count = slots.iter().filter(|slot| slot.lane == "dense").count() as u64;
            let sparse_slot_count =
                slots.iter().filter(|slot| slot.lane == "sparse").count() as u64;
            let built_at_unix_ms = file_modified_unix_ms(&manifest);
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
            let age_ms = match (built_at_unix_ms, now_unix_ms) {
                (Some(built), Some(now)) => Some(now.saturating_sub(built)),
                _ => None,
            };
            let seq_lag = built_at_seq.map(|seq| vault_latest_seq.saturating_sub(seq));
            let (delta_changed_keys, delta_composition, delta_measured_at_unix_ms) =
                match (measure_delta, built_at_seq) {
                    (true, Some(base_seq)) => {
                        let measured = self.measure_search_delta_changed_keys_at_snapshot(
                            snapshot,
                            panel_version,
                            base_seq,
                            &slots,
                        )?;
                        (
                            Some(measured.changed_len() as u64),
                            Some(measured.composition()),
                            now_unix_ms,
                        )
                    }
                    _ => (None, None, None),
                };

            let (state, remediation) = classify_search_generation(
                rebuild_required.is_some(),
                manifest_present,
                built_at_seq,
                delta_changed_keys,
                max_reconciled_delta_keys,
            );

            Ok(SynapseCalyxSearchGenerationStatus {
                panel_version: Some(panel_version),
                panel_state_error,
                manifest_path: Some(manifest.display().to_string()),
                manifest_present,
                manifest_sha256,
                built_at_seq,
                vault_latest_seq,
                panel_content_seq: Some(panel_content_seq),
                seq_lag,
                delta_changed_keys,
                delta_composition,
                delta_measured_at_unix_ms,
                max_reconciled_delta_keys,
                rows_covered,
                delta_coverage_ratio: delta_coverage_ratio(delta_changed_keys, rows_covered),
                slots,
                dense_slot_count,
                sparse_slot_count,
                built_at_unix_ms,
                age_ms,
                rebuild_required,
                state: state.to_owned(),
                remediation: remediation.to_owned(),
            })
        })
    }

    /// The MVCC sequence of one constellation's `Base` row and of every slot
    /// row it declares (issue #1935).
    ///
    /// `CALYX_SEARCH_DELTA_INCOMPLETE` reports that a slot row changed after a
    /// generation's base sequence while its `Base` row did not. That names the
    /// broken invariant but not the writer that broke it. The shape of these
    /// sequences does:
    ///
    /// * every declared slot at one sequence with `Base` older — a
    ///   whole-constellation writer staged the slot rows and skipped `Base`;
    /// * one slot newer than the rest — a single-slot writer that did not
    ///   restate the `Base` integrity record (#1888);
    /// * `Base` at or after every slot — this constellation is not the cause.
    ///
    /// Read-only, one constellation, no scan.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the row sequences cannot be read — which
    /// includes a vault opened in latest-only recovery mode, where per-row
    /// sequences do not exist and reporting one would be a fabrication.
    pub fn diagnose_constellation_row_sequences(
        &self,
        cx_id: calyx_core::CxId,
    ) -> Result<ConstellationRowSequences, SynapseCalyxError> {
        let latest_seq = self.vault.latest_seq();
        let base_key = calyx_aster::cf::base_key(cx_id);
        let slot_key = calyx_aster::cf::slot_key(cx_id);
        let seq_of = |cf: calyx_aster::cf::ColumnFamily, key: &[u8]| {
            self.vault.seq_for_key_at(latest_seq, cf, key).map_err(|e| {
                SynapseCalyxError::from_calyx("read a constellation row's MVCC sequence", &e)
            })
        };
        let base_row_seq = seq_of(calyx_aster::cf::ColumnFamily::Base, &base_key)?;
        let base_row_present = self
            .vault
            .read_cf_at(latest_seq, calyx_aster::cf::ColumnFamily::Base, &base_key)
            .map_err(|e| SynapseCalyxError::from_calyx("read a constellation Base row", &e))?
            .is_some();
        // The declared slot set comes from the Base row itself, so this reports
        // the slots this constellation actually claims rather than a panel's
        // slot list, which may have moved on.
        let mut slot_row_seqs = Vec::new();
        let mut panel_version = None;
        if let Some(bytes) = self
            .vault
            .read_cf_at(latest_seq, calyx_aster::cf::ColumnFamily::Base, &base_key)
            .map_err(|e| SynapseCalyxError::from_calyx("read a constellation Base row", &e))?
        {
            // The slot membership comes from the row's own **slot-hash table**,
            // not from the decoded `Constellation`: a decoded Base row carries
            // no slot vectors at all (#1894), so its `slots` map is empty and
            // enumerating it would report every constellation as declaring
            // nothing. The hash table is the durable membership record, and it
            // is also the integrity record a qualified slot write must restate
            // (#1888) — so it is the right source of truth for this question.
            let (constellation, slot_hashes) =
                calyx_aster::vault::encode::decode_constellation_base_with_slot_hashes(&bytes)
                    .map_err(|e| {
                        SynapseCalyxError::from_calyx("decode a Base row with its slot hashes", &e)
                    })?;
            panel_version = Some(constellation.panel_version);
            for (slot, _) in slot_hashes {
                slot_row_seqs.push((
                    slot.get(),
                    seq_of(calyx_aster::cf::ColumnFamily::slot(slot), &slot_key)?,
                ));
            }
        }
        Ok(ConstellationRowSequences {
            cx_id: cx_id.to_string(),
            latest_seq,
            panel_version,
            base_row_present,
            base_row_seq,
            slot_row_seqs,
        })
    }

    /// How many keys of one native column family the MVCC changed-key history
    /// reports as changed after `after_seq` (issue #1935).
    ///
    /// Exposed next to a plain row count so the two can be compared. The
    /// changed-key history is the input to every generation-freshness decision,
    /// and if it reports a whole column family as changed then those decisions
    /// are being made on a quantity that is not "writes since".
    ///
    /// # Errors
    ///
    /// Returns a structured error when the CF name is not a native column
    /// family, or when the history cannot prove the requested range.
    pub fn changed_key_count_after(
        &self,
        cf_name: &str,
        after_seq: u64,
    ) -> Result<u64, SynapseCalyxError> {
        let cf = calyx_aster::cf::ColumnFamily::from_name(cf_name).ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_UNKNOWN_COLUMN_FAMILY",
                format!("{cf_name} is not a native Calyx column family"),
                "name a native column family (base, anchors, scalars, slot_<n>, kv, ...)",
            )
        })?;
        let snapshot = self
            .vault
            .pin_reader(Freshness::FreshDerived, 30_000)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("pin the changed-key history snapshot", &error)
            })?;
        let keys = self
            .vault
            .changed_cf_keys_after_snapshot(snapshot, cf, after_seq);
        let _released = self.vault.release_reader(snapshot.lease().id());
        let keys = keys.map_err(|error| {
            SynapseCalyxError::from_calyx("read the MVCC changed-key history", &error)
        })?;
        Ok(keys.len() as u64)
    }

    /// Rows one native column family holds at the latest snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the CF name is not a native column
    /// family or the scan fails.
    pub fn cf_row_count(&self, cf_name: &str) -> Result<u64, SynapseCalyxError> {
        let cf = calyx_aster::cf::ColumnFamily::from_name(cf_name).ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_UNKNOWN_COLUMN_FAMILY",
                format!("{cf_name} is not a native Calyx column family"),
                "name a native column family (base, anchors, scalars, slot_<n>, kv, ...)",
            )
        })?;
        let rows = self.vault.scan_cf_latest(cf).map_err(|error| {
            SynapseCalyxError::from_calyx("scan a native column family", &error)
        })?;
        Ok(rows.len() as u64)
    }

    /// The panel version the vault manifest currently publishes as active, or
    /// `None` when no active panel is published.
    ///
    /// `None` is a real, expected state (a vault before boot publication), not
    /// an error — but every *other* failure to read the durable panel state is,
    /// because then "which panel is active" would be a guess.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the durable panel state exists but
    /// cannot be read or decoded.
    pub fn active_panel_version(&self) -> Result<Option<u32>, SynapseCalyxError> {
        match load_vault_panel_state(&self.config.vault_dir) {
            Ok(state) => Ok(Some(state.panel.version)),
            Err(error) if error.code == CALYX_NO_ACTIVE_PANEL => Ok(None),
            Err(error) => Err(SynapseCalyxError::from_calyx(
                "read the durable active panel state",
                &error,
            )),
        }
    }

    /// Every panel generation that has a **published search generation on
    /// disk**, discovered by reading the index root rather than by asking the
    /// manifest which panel is active (issue #1938).
    ///
    /// This is the set that has to be *kept* usable. Before this, the only
    /// generation anything could name was the active panel's, so a generation
    /// built for any other panel had no maintainer and could only rot: every
    /// MCP tool call appends an `mcp-usage` row, which advances the vault
    /// sequence, which grows every generation's reconciliation delta — including
    /// the ones nothing was watching. The transcript corpus was reachable for
    /// about two hours after it was built and then required a break-glass
    /// rebuild to use again.
    ///
    /// Discovery is by directory listing because the directory *is* the
    /// publication: `rebuild_search_indexes_for_panel` writes
    /// `idx/search/panel_{version:010}/manifest.json` and nothing else records
    /// that a generation exists. Any entry under the index root that is not a
    /// recognised panel directory is returned in `unrecognized` rather than
    /// dropped, so an unreadable or unexpected artifact is a reported fact and
    /// never a silently narrower sweep.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the index root exists but cannot be
    /// listed. An index root that does not exist is not an error: it is the
    /// state "no generation has ever been published", which the empty result
    /// reports exactly.
    pub fn published_search_generations(
        &self,
    ) -> Result<PublishedSearchGenerations, SynapseCalyxError> {
        let index_root = self.config.vault_dir.join("idx").join("search");
        let entries = match std::fs::read_dir(&index_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PublishedSearchGenerations::default());
            }
            Err(error) => {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_INDEX_ROOT_UNREADABLE",
                    format!(
                        "list the persisted search index root {}: {error}",
                        index_root.display()
                    ),
                    "the set of published search generations cannot be enumerated, so the \
                     unattended maintainer cannot know which generations it owes maintenance to; \
                     check the vault directory's permissions and that the volume is mounted",
                ));
            }
        };
        let mut panels = Vec::new();
        let mut unrecognized = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_INDEX_ROOT_UNREADABLE",
                    format!(
                        "read an entry of the persisted search index root {}: {error}",
                        index_root.display()
                    ),
                    "the set of published search generations cannot be enumerated; check the \
                     vault directory's permissions and that the volume is mounted",
                )
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(digits) = name.strip_prefix("panel_") else {
                unrecognized.push(name);
                continue;
            };
            let Ok(version) = digits.parse::<u32>() else {
                unrecognized.push(name);
                continue;
            };
            // A directory without a manifest is not a published generation: the
            // rebuild creates the directory before it publishes, so treating the
            // directory alone as a publication would charge maintenance for a
            // generation that has never existed.
            if calyx_search::manifest_path(&self.config.vault_dir, version).is_file() {
                panels.push(version);
            } else {
                unrecognized.push(format!("{name} (no manifest.json)"));
            }
        }
        panels.sort_unstable();
        unrecognized.sort();
        Ok(PublishedSearchGenerations {
            index_root,
            panels,
            unrecognized,
        })
    }

    /// Retires one published search generation by removing its directory, and
    /// **proves** the removal by re-enumerating the index root afterwards
    /// (#1972).
    ///
    /// The caller has already established that this generation *should* be
    /// retired — that judgement needs the panel catalog, which lives above this
    /// crate. What is enforced here is everything that makes the removal
    /// physically safe, because these are facts about the vault:
    ///
    /// * the generation must actually be published (a directory with a
    ///   `manifest.json`), so a typo removes nothing;
    /// * it must **not** be the panel the vault manifest publishes as active,
    ///   because that generation is what every default query reads.
    ///
    /// This is deliberately not something the unattended maintainer does.
    /// Deleting an index directory is destructive and irreversible from inside
    /// the process, so it is an explicit, operator-owned act — the same shape as
    /// Lucene's `IndexDeletionPolicy`, where retention is a declared policy
    /// decision rather than an implicit side effect of a background merge. The
    /// sweep's job is to *name* the condition; this is the named action.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the generation is not published, when it
    /// is the active panel, when the directory cannot be removed, or when the
    /// post-removal readback still finds it published — the last of which means
    /// the filesystem accepted a removal that did not take effect, and is
    /// reported rather than assumed away.
    pub fn retire_search_generation(
        &self,
        panel_version: u32,
    ) -> Result<SynapseCalyxRetiredSearchGeneration, SynapseCalyxError> {
        let before = self.published_search_generations()?;
        if !before.panels.contains(&panel_version) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_GENERATION_NOT_PUBLISHED",
                format!(
                    "no search generation is published for panel version {panel_version} under {}; published generations are {:?}",
                    before.index_root.display(),
                    before.panels
                ),
                "name a panel version that appears in health.calyx_search_generations_retirable_panel_versions",
            ));
        }
        let active = self.active_panel_version()?;
        if active == Some(panel_version) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_GENERATION_ACTIVE",
                format!(
                    "panel version {panel_version} is the vault's active panel, so its search generation is what every default query reads"
                ),
                "retire a superseded generation, or publish a different active panel first",
            ));
        }
        let directory = before.index_root.join(format!("panel_{panel_version:010}"));
        let bytes_before = directory_bytes(&directory);
        let files_before = directory_file_count(&directory);
        std::fs::remove_dir_all(&directory).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_GENERATION_RETIRE_FAILED",
                format!(
                    "remove the search generation directory {}: {error}",
                    directory.display()
                ),
                "check the vault directory's permissions and that no process holds a file in that \
                 directory open",
            )
        })?;
        // The return value of `remove_dir_all` is a claim; the source of truth
        // is the index root. Re-enumerating it is the same read the sweep does,
        // so a retirement that did not take effect is caught here rather than
        // discovered as a generation that keeps reappearing every tick.
        let after = self.published_search_generations()?;
        if after.panels.contains(&panel_version) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_GENERATION_STILL_PUBLISHED",
                format!(
                    "the search generation directory for panel {panel_version} was removed without error, but re-enumerating {} still reports it published",
                    after.index_root.display()
                ),
                "inspect the index root directly; the filesystem accepted a removal that did not \
                 take effect",
            ));
        }
        Ok(SynapseCalyxRetiredSearchGeneration {
            panel_version,
            directory: directory.display().to_string(),
            files_removed: files_before,
            bytes_reclaimed: bytes_before,
            published_before: before.panels,
            published_after: after.panels,
            active_panel_version: active,
        })
    }

    /// Keeps **one named** persisted search generation inside its freshness
    /// budget, unattended (issue #1891 ask 2; extended to any panel by #1938).
    ///
    /// Before #1891, the generation could only ever be created or repaired by a
    /// human-driven break-glass ceremony. On the production vault that ceremony
    /// had run exactly once: the generation was built at seq 55908 and then
    /// silently allowed to expire, drifting 13,685 sequences past the bounded
    /// reconciliation limit of 8,192 — so every recall query failed closed and
    /// nothing brought it back. An always-on capability whose only repair is a
    /// manual ceremony is off by default, forever.
    ///
    /// **There is deliberately no active-panel-only entry point.** #1938 was the
    /// same defect one level up: maintenance was addressed by the active-panel
    /// pointer rather than by the generation, so every generation the pointer did
    /// not name had no maintainer and could only rot. A convenience wrapper that
    /// maintains "the" generation would reintroduce exactly that. The caller
    /// enumerates what exists ([`Self::published_search_generations`]) and asks
    /// for each one by name.
    ///
    /// `supplied` is that panel's slot contract, which the caller resolves
    /// because reconstructing it lives in `synapse-storage`, the crate that
    /// declares the panels. `None` means "this is the active panel", exactly as
    /// on [`Self::rebuild_search_indexes_for_panel`]; a non-active panel with no
    /// contract cannot be rebuilt and must not be asked to be — the caller
    /// classifies that case rather than discovering it as a rebuild failure.
    ///
    /// The two builds are classified separately, because they are not the same
    /// operation (#1891 ask 3): an **initial build** over an absent generation
    /// replaces nothing and is non-destructive, whereas a **refresh over an
    /// existing generation** republishes a live artifact. Both are admitted
    /// here, but they are reported and logged distinctly so the destructive one
    /// is never mistaken for the harmless one in an audit.
    ///
    /// The pass is triple-bounded so it can run on a periodic tick without
    /// becoming the thing that starves the vault:
    ///
    /// * it does nothing at all while the measured changed-key delta is inside
    ///   [`SEARCH_GENERATION_REFRESH_DELTA_KEYS`], which is half the query-time
    ///   reconciliation limit, so the generation is refreshed *before* recall
    ///   dies rather than after;
    /// * it refuses to build twice inside
    ///   [`SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS`] — a per-generation bound,
    ///   read from that generation's own manifest mtime, so one hot corpus
    ///   cannot spend another's maintenance budget;
    /// * it never builds a panel it has no contract for.
    ///
    /// The result is verified by re-reading **that panel's** generation state off
    /// disk after the build, not by trusting the build's own return value.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the generation state cannot be read, or
    /// when a build it decided to run fails. It does not error merely because
    /// no build was needed.
    pub fn maintain_search_generation_for_panel(
        &self,
        panel_version: u32,
        supplied: Option<&VaultPanelState>,
    ) -> Result<SearchGenerationMaintenanceReport, SynapseCalyxError> {
        let started = std::time::Instant::now();
        match self.search_generation_status_for_panel(panel_version, true) {
            Ok(before) => self.decide_and_maintain(panel_version, before, supplied, started, None),
            Err(error)
                if error.code == "SYNAPSE_CALYX_STALE_DERIVED"
                    && error.source_code == Some("CALYX_STALE_DERIVED") =>
            {
                // A restart reconstructs the latest durable rows, but it does
                // not invent the older per-key change history an already-
                // published generation may name as its base. This exact error
                // is positive proof that incremental reconciliation is no
                // longer possible. Read the artifact facts without pretending
                // to measure that missing interval, then perform the full
                // authoritative rebuild this unattended maintainer owns.
                let before = self.search_generation_status_for_panel(panel_version, false)?;
                tracing::warn!(
                    code = "SYNAPSE_CALYX_SEARCH_GENERATION_HISTORY_GAP_REBASE_REQUIRED",
                    panel_version,
                    error_code = error.code,
                    source_code = error.source_code.unwrap_or("none"),
                    detail = %error.message,
                    before_state = %before.state,
                    before_built_at_seq = ?before.built_at_seq,
                    vault_latest_seq = before.vault_latest_seq,
                    "the persisted generation predates the recovered change-history floor; a full rebuild from authoritative rows is required"
                );
                self.decide_and_maintain(panel_version, before, supplied, started, Some(&error))
            }
            Err(error) => Err(error),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the decision and its five named outcomes are one policy; splitting them would separate a branch from the state it was decided against, which is exactly how the generation was allowed to expire unobserved"
    )]
    fn decide_and_maintain(
        &self,
        panel_version: u32,
        before: SynapseCalyxSearchGenerationStatus,
        supplied: Option<&VaultPanelState>,
        started: std::time::Instant,
        delta_history_gap: Option<&SynapseCalyxError>,
    ) -> Result<SearchGenerationMaintenanceReport, SynapseCalyxError> {
        let elapsed = |started: std::time::Instant| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        };
        // Treat an unmeasured delta as unbounded rather than as zero: a missing
        // measurement must never read as "nothing to do".
        let delta_keys = before.delta_changed_keys.unwrap_or(u64::MAX);
        let seq_lag = before.seq_lag.unwrap_or(u64::MAX);
        let (action, reason) = if !before.manifest_present || before.built_at_seq.is_none() {
            (
                SearchGenerationMaintenanceAction::InitialBuild,
                format!(
                    "no usable generation exists for panel {panel_version} (manifest_present={}), and building a first one replaces nothing",
                    before.manifest_present
                ),
            )
        } else if before.rebuild_required.is_some() {
            (
                SearchGenerationMaintenanceAction::RefreshOverExisting,
                format!(
                    "a mutation staked a rebuild-required intent: {}",
                    before.rebuild_required.as_deref().unwrap_or("unknown")
                ),
            )
        } else if let Some(error) = delta_history_gap.as_ref() {
            (
                SearchGenerationMaintenanceAction::RefreshOverExisting,
                format!(
                    "the generation's incremental change interval is unavailable after vault recovery (code={} source_code={}): {}; full rebase from authoritative Base and slot rows is required",
                    error.code,
                    error.source_code.unwrap_or("none"),
                    error.message
                ),
            )
        } else if delta_keys > SEARCH_GENERATION_REFRESH_DELTA_KEYS {
            (
                SearchGenerationMaintenanceAction::RefreshOverExisting,
                format!(
                    "delta_changed_keys {delta_keys} exceeds the refresh threshold {SEARCH_GENERATION_REFRESH_DELTA_KEYS} (half the query-time reconciliation limit {}); seq_lag is {seq_lag}, which is why the lag alone is not the trigger",
                    before.max_reconciled_delta_keys
                ),
            )
        } else if before
            .delta_coverage_ratio
            .is_some_and(|ratio| ratio > SEARCH_GENERATION_REFRESH_COVERAGE_RATIO)
        {
            // The absolute threshold is a fraction of the *query* limit, so it
            // cannot fire for a generation smaller than that limit however
            // completely it has been superseded (#1908). This is the missing
            // denominator: refresh once this generation's own rows are stale
            // past the ratio, whatever the key count.
            (
                SearchGenerationMaintenanceAction::RefreshOverExisting,
                format!(
                    "delta_coverage_ratio {:.3} exceeds the refresh coverage ratio {SEARCH_GENERATION_REFRESH_COVERAGE_RATIO} \
                     ({delta_keys} changed keys against {} rows covered); the absolute threshold {SEARCH_GENERATION_REFRESH_DELTA_KEYS} \
                     could not fire here because it is a fraction of the query-time limit, not of this generation (seq_lag {seq_lag})",
                    before.delta_coverage_ratio.unwrap_or_default(),
                    before.rows_covered.unwrap_or_default(),
                ),
            )
        } else {
            (
                SearchGenerationMaintenanceAction::NoneNeeded,
                format!(
                    "delta_changed_keys {delta_keys} is inside the refresh threshold {SEARCH_GENERATION_REFRESH_DELTA_KEYS} and coverage {} is inside the ratio {SEARCH_GENERATION_REFRESH_COVERAGE_RATIO} (seq_lag {seq_lag})",
                    before
                        .delta_coverage_ratio
                        .map_or_else(|| "unmeasured".to_owned(), |ratio| format!("{ratio:.3}")),
                ),
            )
        };

        if action == SearchGenerationMaintenanceAction::NoneNeeded {
            return Ok(SearchGenerationMaintenanceReport {
                action,
                reason,
                before,
                after: None,
                rebuild_private_bytes_before: None,
                rebuild_private_bytes_peak: None,
                rebuild_private_bytes_peak_phase: None,
                rebuild_private_bytes_after: None,
                rebuild_private_bytes_reclaim_calls: None,
                rebuild_private_bytes_observed_reclaimed: None,
                elapsed_ms: elapsed(started),
            });
        }

        // The naptime bound exists to stop a hot write stream from spending the
        // maintenance budget on back-to-back rebuilds. It must never hold a
        // *dead* generation dead: once the delta is past the query-time limit,
        // every query is already failing closed, so waiting protects nothing and
        // costs recall. Two exemptions therefore apply — an absent generation
        // (no live artifact to protect) and an already-unusable one.
        let already_unusable =
            delta_history_gap.is_some() || delta_keys > before.max_reconciled_delta_keys;
        if action == SearchGenerationMaintenanceAction::RefreshOverExisting
            && !already_unusable
            && before
                .age_ms
                .is_some_and(|age| age < SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS)
        {
            return Ok(SearchGenerationMaintenanceReport {
                action: SearchGenerationMaintenanceAction::DeferredByInterval,
                reason: format!(
                    "{reason}, but the current generation is only {}ms old and the minimum unattended rebuild interval is {SEARCH_GENERATION_MIN_REBUILD_INTERVAL_MS}ms",
                    before.age_ms.unwrap_or_default()
                ),
                before,
                after: None,
                rebuild_private_bytes_before: None,
                rebuild_private_bytes_peak: None,
                rebuild_private_bytes_peak_phase: None,
                rebuild_private_bytes_after: None,
                rebuild_private_bytes_reclaim_calls: None,
                rebuild_private_bytes_observed_reclaimed: None,
                elapsed_ms: elapsed(started),
            });
        }

        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_GENERATION_MAINTENANCE_STARTED",
            panel_version,
            action = action.as_str(),
            destructive = action.is_destructive(),
            reason = %reason,
            before_state = %before.state,
            before_built_at_seq = ?before.built_at_seq,
            vault_latest_seq = before.vault_latest_seq,
            panel_content_seq = ?before.panel_content_seq,
            seq_lag = ?before.seq_lag,
            delta_changed_keys = ?before.delta_changed_keys,
            delta_history_gap_code = delta_history_gap.as_ref().map(|error| error.code),
            delta_history_gap_source_code = delta_history_gap
                .as_ref()
                .and_then(|error| error.source_code),
            refresh_threshold = SEARCH_GENERATION_REFRESH_DELTA_KEYS,
            max_reconciled_delta_keys = before.max_reconciled_delta_keys,
            already_unusable,
            "building the persisted search generation unattended"
        );
        let rebuild = self.rebuild_search_indexes_for_panel(panel_version, supplied)?;

        // Source of truth is the manifest on disk, re-read independently of the
        // build that just claimed to write it. Scoped to the exact panel that
        // was built: reading the *active* panel's state back after building a
        // non-active generation would report a healthy generation that has
        // nothing to do with the work just performed.
        let after = self.search_generation_status_for_panel(panel_version, true)?;
        if after.state != "built" {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_REBUILD_READBACK_STALE",
                format!(
                    "panel {panel_version} rebuild returned but independent status readback reports state={} (built_at_seq={:?}, panel_content_seq={:?}, delta_changed_keys={:?}, vault_latest_seq={})",
                    after.state,
                    after.built_at_seq,
                    after.panel_content_seq,
                    after.delta_changed_keys,
                    after.vault_latest_seq,
                ),
                "inspect concurrent panel-content writers and the persisted generation manifest; the post-build delta must remain inside the bounded reconciliation law",
            ));
        }
        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_GENERATION_MAINTENANCE_COMMITTED",
            panel_version,
            action = action.as_str(),
            destructive = action.is_destructive(),
            after_state = %after.state,
            after_built_at_seq = ?after.built_at_seq,
            after_panel_content_seq = ?after.panel_content_seq,
            after_seq_lag = ?after.seq_lag,
            after_delta_changed_keys = ?after.delta_changed_keys,
            after_rows_covered = ?after.rows_covered,
            after_dense_lanes = after.dense_slot_count,
            after_sparse_lanes = after.sparse_slot_count,
            private_bytes_before = rebuild.private_bytes_before,
            private_bytes_peak = rebuild.private_bytes_peak,
            private_bytes_peak_phase = %rebuild.private_bytes_peak_phase,
            private_bytes_after = rebuild.private_bytes_after,
            private_bytes_reclaim_calls = rebuild.private_bytes_reclaim_calls,
            private_bytes_observed_reclaimed = rebuild.private_bytes_observed_reclaimed,
            elapsed_ms = elapsed(started),
            "persisted search generation rebuilt unattended and re-read from disk"
        );
        Ok(SearchGenerationMaintenanceReport {
            action,
            reason,
            before,
            after: Some(after),
            rebuild_private_bytes_before: Some(rebuild.private_bytes_before),
            rebuild_private_bytes_peak: Some(rebuild.private_bytes_peak),
            rebuild_private_bytes_peak_phase: Some(rebuild.private_bytes_peak_phase),
            rebuild_private_bytes_after: Some(rebuild.private_bytes_after),
            rebuild_private_bytes_reclaim_calls: Some(rebuild.private_bytes_reclaim_calls),
            rebuild_private_bytes_observed_reclaimed: Some(
                rebuild.private_bytes_observed_reclaimed,
            ),
            elapsed_ms: elapsed(started),
        })
    }

    /// Rebuilds the immutable persisted-search generation for the exact active
    /// vault panel and then reopens the published manifest for independent
    /// validation. The raw Base rows remain authoritative.
    ///
    /// # Errors
    ///
    /// Fails closed when the durable panel state is absent/corrupt, its version
    /// differs from `expected_panel_version`, rebuilding fails, or the
    /// published manifest/sidecars cannot be read back.
    pub fn rebuild_search_indexes(
        &self,
        expected_panel_version: u32,
    ) -> Result<SynapseCalyxSearchRebuildReport, SynapseCalyxError> {
        self.rebuild_search_indexes_for_panel(expected_panel_version, None)
    }

    fn panel_membership_manifest_present(
        panel_version: u32,
        manifest_path: &Path,
    ) -> Result<bool, SynapseCalyxError> {
        match fs::metadata(manifest_path) {
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_MEMBERSHIP_MANIFEST_NOT_FILE",
                format!(
                    "panel {panel_version} membership manifest path {} exists but is not a regular file",
                    manifest_path.display()
                ),
                "preserve the path for diagnosis and replace it only through the owning rebuild workflow",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_MEMBERSHIP_MANIFEST_IO",
                format!(
                    "inspect panel {panel_version} membership manifest {}: {error}",
                    manifest_path.display()
                ),
                "repair the exact manifest path permissions or filesystem error, then retry",
            )),
        }
    }

    fn read_panel_membership_generation(
        &self,
        panel_version: u32,
        manifest_path: &Path,
        built: bool,
    ) -> Result<SynapseCalyxPanelMembershipGenerationReport, SynapseCalyxError> {
        let indexes =
            calyx_search::PersistedSearchIndexes::open(&self.config.vault_dir, panel_version)
                .map_err(|error| {
                    search_rebuild_error(
                        &format!("reopen panel {panel_version} membership generation"),
                        error,
                    )
                })?;
        let generation = indexes.generation().map_err(|error| {
            search_rebuild_error(
                &format!("validate panel {panel_version} membership manifest"),
                error,
            )
        })?;
        if generation.panel_version != panel_version || !generation.slots.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_MEMBERSHIP_GENERATION_INVALID",
                format!(
                    "membership-only generation requested panel {panel_version}, but reopened panel {} with {} retrieval slots",
                    generation.panel_version,
                    generation.slots.len()
                ),
                "preserve the manifest and sidecars for diagnosis; rebuild the panel through its correct queryable or finite-only owner",
            ));
        }
        let membership = indexes.panel_membership().map_err(|error| {
            search_rebuild_error(
                &format!("validate panel {panel_version} membership sidecar"),
                error,
            )
        })?;
        let member_rows = u64::try_from(membership.ids.len()).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_MEMBERSHIP_COUNT_OVERFLOW",
                format!("panel {panel_version} membership row count does not fit u64: {error}"),
                "preserve the membership sidecar and inspect its declared row count",
            )
        })?;
        Ok(SynapseCalyxPanelMembershipGenerationReport {
            panel_version,
            built,
            base_seq: membership.base_seq,
            member_rows,
            manifest_path: manifest_path.to_path_buf(),
            manifest_sha256: membership.manifest_sha256,
            sidecar_sha256: membership.sidecar_sha256,
        })
    }

    fn panel_membership_rebase_required(error: &SynapseCalyxError) -> bool {
        (error.code == "SYNAPSE_CALYX_STALE_DERIVED"
            && error.source_code == Some("CALYX_STALE_DERIVED"))
            || error.code == "CALYX_SEARCH_DELTA_REBASE_REQUIRED"
    }

    /// Ensures a finite-only panel has an exact hash-sealed membership
    /// generation, without manufacturing retrieval indexes for it.
    ///
    /// Existing generations are reopened and validated before any replacement.
    /// An absent manifest is built. A valid generation that cannot reconcile
    /// because its delta exceeds the hard bound or its base predates recovered
    /// changed-key history is authoritatively rebuilt, then reopened and
    /// reconciled again. Any corrupt, wrong-panel, future, or otherwise invalid
    /// generation fails closed and is preserved. Callers must use this only for
    /// panels that are not query-admissible.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the generation lock is contended, the
    /// manifest or membership sidecar is corrupt, the Base scan fails, a
    /// post-rebuild reconciliation still fails, or the published generation has
    /// the wrong panel identity or any retrieval slot.
    pub fn ensure_panel_membership_generation(
        &self,
        panel_version: u32,
    ) -> Result<SynapseCalyxPanelMembershipGenerationReport, SynapseCalyxError> {
        if panel_version == 0 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PANEL_VERSION_INVALID",
                "panel membership generation requires a non-zero panel version",
                "supply the exact positive panel version declared by the temporal population",
            ));
        }
        let _rebuild_lock = self.acquire_search_rebuild_lock(panel_version)?;
        let manifest_path = calyx_search::manifest_path(&self.config.vault_dir, panel_version);
        let manifest_present =
            Self::panel_membership_manifest_present(panel_version, &manifest_path)?;
        let mut built = if manifest_present {
            false
        } else {
            calyx_search::rebuild_panel_membership_for_vault(
                &self.config.vault_dir,
                &self.vault,
                panel_version,
            )
            .map_err(|error| {
                search_rebuild_error(
                    &format!("build panel {panel_version} membership-only generation"),
                    error,
                )
            })?;
            true
        };
        let mut report =
            self.read_panel_membership_generation(panel_version, &manifest_path, built)?;
        let reconcile = || {
            self.with_panel_read_snapshot(panel_version, SEARCH_DELTA_SCAN_LEASE_MS, |snapshot| {
                self.panel_membership_at_snapshot(snapshot, panel_version)
                    .map(drop)
            })
        };
        if let Err(error) = reconcile() {
            if !Self::panel_membership_rebase_required(&error) {
                return Err(error);
            }
            tracing::warn!(
                code = "SYNAPSE_CALYX_PANEL_MEMBERSHIP_REBASE_REQUIRED",
                panel_version,
                error_code = error.code,
                source_code = error.source_code.unwrap_or("none"),
                detail = %error.message,
                prior_base_seq = report.base_seq,
                prior_member_rows = report.member_rows,
                "finite-only membership generation cannot reconcile to the current panel snapshot; rebuilding from authoritative Base rows"
            );
            calyx_search::rebuild_panel_membership_for_vault(
                &self.config.vault_dir,
                &self.vault,
                panel_version,
            )
            .map_err(|error| {
                search_rebuild_error(
                    &format!("rebase panel {panel_version} membership-only generation"),
                    error,
                )
            })?;
            built = true;
            report = self.read_panel_membership_generation(panel_version, &manifest_path, built)?;
            reconcile()?;
        }
        tracing::info!(
            code = "SYNAPSE_CALYX_PANEL_MEMBERSHIP_GENERATION_READY",
            panel_version,
            built,
            base_seq = report.base_seq,
            member_rows = report.member_rows,
            manifest_sha256 = %report.manifest_sha256,
            sidecar_sha256 = %report.sidecar_sha256,
            "finite-only panel membership generation reopened and verified"
        );
        Ok(report)
    }

    /// [`Self::rebuild_search_indexes`], with an explicit panel contract for a
    /// generation that is not the durable active one (#1668).
    ///
    /// The published artifacts were already panel-scoped
    /// (`idx/search/panel_{version:010}/`), so building a non-active
    /// generation writes to its own directory and cannot overwrite the active
    /// one — what was missing was the *permission*, not the isolation. Supplying
    /// `None` restricts the rebuild to the active panel, unchanged.
    ///
    /// # Errors
    ///
    /// As [`Self::rebuild_search_indexes`], plus: when `supplied` disagrees with
    /// `expected_panel_version`, or when a non-active version is requested with
    /// no definition to rebuild it from.
    pub fn rebuild_search_indexes_for_panel(
        &self,
        expected_panel_version: u32,
        supplied: Option<&VaultPanelState>,
    ) -> Result<SynapseCalyxSearchRebuildReport, SynapseCalyxError> {
        let _rebuild_lock = self.acquire_search_rebuild_lock(expected_panel_version)?;
        let state = self.resolve_search_rebuild_panel_state(expected_panel_version, supplied)?;
        let panel_root = self
            .config
            .vault_dir
            .join("idx")
            .join("search")
            .join(format!("panel_{expected_panel_version:010}"));
        let manifest_path = panel_root.join("manifest.json");
        let before_manifest_sha256 = read_optional_sha256(&manifest_path)?;
        let private_bytes_before = process_private_bytes()?;
        let mut memory = SearchRebuildMemoryTracker::new(private_bytes_before);
        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_REBUILD_STARTED",
            panel_version = expected_panel_version,
            vault_dir = %self.config.vault_dir.display(),
            before_manifest_sha256 = ?before_manifest_sha256,
            "rebuilding persisted Calyx search indexes"
        );
        self.rebuild_search_artifacts_measured(&state, &mut memory)?;
        let generation =
            calyx_search::PersistedSearchIndexes::open(&self.config.vault_dir, state.panel.version)
                .and_then(|indexes| indexes.generation())
                .map_err(|error| {
                    search_rebuild_error("reopen published search generation", error)
                })?;
        if generation.panel_version != expected_panel_version {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_PANEL_MISMATCH",
                format!(
                    "published search generation panel {} != requested panel {expected_panel_version}",
                    generation.panel_version
                ),
                "remove no files; inspect the rebuild marker and durable panel state before retrying",
            ));
        }
        let raw_sidecars = inspect_search_raw_sidecars(&panel_root)?;
        let private_bytes_after = process_private_bytes()?;
        if private_bytes_after > memory.private_bytes_peak {
            memory.private_bytes_peak = private_bytes_after;
            "after_generation_reopen".clone_into(&mut memory.private_bytes_peak_phase);
        }
        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_REBUILD_COMMITTED",
            panel_version = expected_panel_version,
            base_seq = generation.base_seq,
            manifest_sha256 = %generation.manifest_sha256,
            raw_sidecar_count = raw_sidecars.len(),
            private_bytes_before,
            private_bytes_peak = memory.private_bytes_peak,
            private_bytes_peak_phase = %memory.private_bytes_peak_phase,
            private_bytes_after,
            private_bytes_reclaim_calls = memory.reclaim_calls,
            private_bytes_observed_reclaimed = memory.observed_reclaimed,
            "persisted Calyx search generation reopened and verified"
        );
        Ok(SynapseCalyxSearchRebuildReport {
            expected_panel_version,
            before_manifest_sha256,
            private_bytes_before,
            private_bytes_peak: memory.private_bytes_peak,
            private_bytes_peak_phase: memory.private_bytes_peak_phase,
            private_bytes_after,
            private_bytes_reclaim_calls: memory.reclaim_calls,
            private_bytes_observed_reclaimed: memory.observed_reclaimed,
            generation,
            manifest_path,
            raw_sidecars,
        })
    }

    fn acquire_search_rebuild_lock(
        &self,
        expected_panel_version: u32,
    ) -> Result<File, SynapseCalyxError> {
        let lock_path = self.config.vault_dir.join("search-rebuild.lock");
        let mut lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_REBUILD_LOCK_IO",
                    format!("open search rebuild lock {}: {error}", lock_path.display()),
                    "repair the exact lock-file path permissions or filesystem error, then retry; do not bypass single-writer admission",
                )
            })?;
        lock.try_lock_exclusive().map_err(|error| {
            let contended = error.kind() == io::ErrorKind::WouldBlock
                || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)));
            let (code, remediation) = if contended {
                (
                    "SYNAPSE_CALYX_SEARCH_REBUILD_IN_PROGRESS",
                    "wait for the in-flight persisted search rebuild to publish or fail, then retry; every manual and scheduled caller shares this lock",
                )
            } else {
                (
                    "SYNAPSE_CALYX_SEARCH_REBUILD_LOCK_IO",
                    "repair the exact lock-file path or filesystem error, then retry; do not bypass single-writer admission",
                )
            };
            SynapseCalyxError::new(
                code,
                format!(
                    "acquire exclusive search rebuild lock {} for panel {expected_panel_version}: {error}",
                    lock_path.display()
                ),
                remediation,
            )
        })?;
        lock.set_len(0).and_then(|()| {
            writeln!(
                lock,
                "pid={} panel_version={expected_panel_version}",
                std::process::id()
            )
        })
        .and_then(|()| lock.sync_data())
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_REBUILD_LOCK_IO",
                format!(
                    "persist search rebuild lock owner {} for panel {expected_panel_version}: {error}",
                    lock_path.display()
                ),
                "repair the exact lock-file path or filesystem error, then retry; no index artifacts were written",
            )
        })?;
        Ok(lock)
    }

    fn resolve_search_rebuild_panel_state(
        &self,
        expected_panel_version: u32,
        supplied: Option<&VaultPanelState>,
    ) -> Result<VaultPanelState, SynapseCalyxError> {
        if let Some(state) = supplied
            && state.panel.version != expected_panel_version
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_PANEL_MISMATCH",
                format!(
                    "requested search rebuild panel {expected_panel_version}, but the supplied panel definition is version {}",
                    state.panel.version
                ),
                "supply the panel contract for the exact version being rebuilt; an index built from another panel's slot map does not describe the rows it indexed",
            ));
        }
        let state = if let Some(state) = supplied {
            state.clone()
        } else {
            let active = load_vault_panel_state(&self.config.vault_dir).map_err(|error| {
                if error.code == CALYX_NO_ACTIVE_PANEL {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_NO_ACTIVE_PANEL",
                        format!(
                            "no active durable panel is published for search rebuild: {}",
                            error.message
                        ),
                        "publish the active panel for this constellation (publish_active_panel / boot panel publication) before requesting search_rebuild; this is an expected empty state, not shard corruption \u{2014} do not restore from backup",
                    )
                } else {
                    SynapseCalyxError::from_calyx(
                        "load durable panel state for search rebuild",
                        &error,
                    )
                }
            })?;
            if active.panel.version != expected_panel_version {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_PANEL_MISMATCH",
                    format!(
                        "requested search rebuild panel {expected_panel_version}, but no definition was supplied for it and the durable active panel is {}",
                        active.panel.version
                    ),
                    "supply the queryable panel contract for the requested version (Synapse reconstructs and capability-gates code-declared generations), or retry with the active panel's exact version",
                ));
            }
            active
        };
        Ok(state)
    }

    fn rebuild_search_artifacts_measured(
        &self,
        state: &VaultPanelState,
        memory: &mut SearchRebuildMemoryTracker,
    ) -> Result<(), SynapseCalyxError> {
        calyx_search::rebuild_for_vault_with_panel_state_dense_config_progress(
            &self.config.vault_dir,
            &self.vault,
            state,
            self.effective_tuning()?.dense_index_config(),
            |progress| Ok(memory.observe(&progress)?),
        )
        .map_err(|error| search_rebuild_error("rebuild persisted search indexes", error))
    }

    /// Publishes an active durable `Panel` snapshot into the Calyx manifest for
    /// one exact constellation panel version, following the vault
    /// commit-boundary discipline. The caller supplies the authoritative slot
    /// contract; the raw Base rows remain the source of truth.
    ///
    /// This is the step `search_rebuild` depends on: without an active panel,
    /// `load_vault_panel_state` returns `CALYX_NO_ACTIVE_PANEL` and no search
    /// generation can be built. The operation is idempotent — when the exact
    /// panel version is already the active manifest panel it is a no-op that
    /// still proves the published state by an independent readback.
    ///
    /// # Errors
    ///
    /// Fails closed when the manifest cannot be read, the durable write fails,
    /// or the post-write readback does not resolve to the requested version.
    pub fn publish_active_panel(
        &self,
        panel: &Panel,
        registry: &Registry,
    ) -> Result<SynapseCalyxPanelPublishReport, SynapseCalyxError> {
        let vault_dir = &self.config.vault_dir;
        // The panel version is an immutable contract identity, so a manifest
        // whose panel_ref already names `panel/panel-v<version>-*` has this
        // panel published. Re-writing would churn manifest_seq for no change.
        let published_prefix = format!("panel/panel-v{:08}-", panel.version);
        let current = calyx_aster::manifest::ManifestStore::open(vault_dir)
            .load_current()
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read manifest for active-panel publication", &error)
            })?;
        let desired_registry = registry.lens_snapshots();
        let panel_to_publish = if current
            .panel_ref
            .logical_path
            .starts_with(&published_prefix)
        {
            let state = load_vault_panel_state(vault_dir).map_err(|error| {
                SynapseCalyxError::from_calyx("read back already-published active panel", &error)
            })?;
            if !same_panel_definition(&state.panel, panel) {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_PANEL_VERSION_COLLISION",
                    format!(
                        "active panel version {} is already published with a different slot/kernel/guard definition",
                        panel.version
                    ),
                    "allocate a new panel version for the changed contract; never overwrite an immutable panel generation",
                ));
            }
            let registry_matches = state
                .registry_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.lenses == desired_registry);
            if registry_matches {
                return Ok(SynapseCalyxPanelPublishReport {
                    panel_version: panel.version,
                    published: false,
                    panel_ref: current.panel_ref.logical_path,
                    registry_ref: current.registry_ref.map(|reference| reference.logical_path),
                    readback_panel_version: state.panel.version,
                });
            }
            // Preserve the original server-stamped creation time when repairing
            // the registry beside an already-published immutable panel.
            state.panel
        } else {
            panel.clone()
        };
        let write: VaultPanelWrite =
            persist_vault_panel_state(vault_dir, &panel_to_publish, registry).map_err(|error| {
                SynapseCalyxError::from_calyx("publish active durable panel snapshot", &error)
            })?;
        // Prove the published state through the same loader search rebuild uses.
        let state = load_vault_panel_state(vault_dir).map_err(|error| {
            SynapseCalyxError::from_calyx("read back published active panel", &error)
        })?;
        verify_published_panel_readback(&state, panel, registry)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_ACTIVE_PANEL_PUBLISHED",
            panel_version = panel.version,
            manifest_seq = write.manifest_seq,
            durable_seq = write.durable_seq,
            panel_ref = %write.panel_ref.logical_path,
            registry_ref = %write.registry_ref.logical_path,
            "published active durable Calyx panel snapshot to the manifest"
        );
        Ok(SynapseCalyxPanelPublishReport {
            panel_version: panel.version,
            published: true,
            panel_ref: write.panel_ref.logical_path,
            registry_ref: Some(write.registry_ref.logical_path),
            readback_panel_version: state.panel.version,
        })
    }

    /// Retires orphaned physical `cf/slot_*` column families that no live panel
    /// references (issue #1776). Orphan-ness is derived from live Base
    /// membership (never a hardcoded slot range); each drop is fail-closed with
    /// readback, idempotent, and bounds its durable-lock hold to one CF.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the pass cannot derive the legitimate
    /// slot set, a candidate still resolves to a live Base row, or a physical
    /// removal/readback fails.
    pub fn retire_orphan_slot_cfs(&self) -> Result<AsterOrphanSlotGcReport, SynapseCalyxError> {
        self.vault.retire_orphan_slot_cfs().map_err(|error| {
            SynapseCalyxError::from_calyx("retire orphan physical slot column families", &error)
        })
    }

    #[must_use]
    pub fn status(&self) -> SynapseCalyxVaultStatus {
        let math_status = self.math_runtime.status_snapshot();
        let mut status = status_from_vault(&self.config, &self.vault, &math_status, self.open_mode);
        if let Some(code) = math_status.runtime_readback_code {
            "error".clone_into(&mut status.phase);
            status.last_error_code = Some(code);
            status.last_error = math_status.runtime_readback_error;
            status.remediation = Some(
                "repair the named process-local/host-wide GPU Source of Truth; never infer safety from a stale startup snapshot"
                    .to_owned(),
            );
        }
        match self.anneal_status() {
            Ok(anneal) => status.anneal = Some(anneal),
            Err(error) => {
                "error".clone_into(&mut status.phase);
                status.last_error_code = Some(error.code.to_owned());
                status.last_calyx_error_code = error.source_code.map(str::to_owned);
                status.last_error = Some(error.message);
                status.remediation = Some(error.remediation.to_owned());
            }
        }
        status
    }

    /// Returns the same millisecond clock source used by this opened vault.
    ///
    /// # Errors
    ///
    /// Returns a structured config error if the vault somehow contains an
    /// invalid fixed-clock configuration after startup validation.
    pub fn clock_now_ms(&self) -> Result<Ts, SynapseCalyxError> {
        match self.config.tuning.clock_mode {
            SynapseCalyxClockMode::System => Ok(SystemClock.now()),
            SynapseCalyxClockMode::Fixed => {
                self.config
                    .tuning
                    .fixed_clock_unix_ms
                    .ok_or_else(|| {
                        SynapseCalyxError::new(
                            "SYNAPSE_CALYX_CLOCK_INVALID",
                            "clock_mode=fixed is missing fixed_clock_unix_ms after validation",
                            "inspect the Calyx tuning config and restart after fixing the fixed clock fields",
                        )
                    })
            }
        }
    }

    #[must_use]
    pub fn cx_id_for_input(&self, input_bytes: &[u8], panel_version: u32) -> CxId {
        self.vault.cx_id_for_input(input_bytes, panel_version)
    }

    /// Durable root that owns this opened vault and its derived generations.
    #[must_use]
    pub fn vault_dir(&self) -> &Path {
        &self.config.vault_dir
    }

    /// Writes one content-addressed observation through Aster's native
    /// constellation ingestion path.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if schema validation,
    /// duplicate compatibility checks, ledger append, WAL commit, or any
    /// native Base/Slot/Scalars row write fails.
    pub fn put_observation_constellation(
        &self,
        constellation: Constellation,
    ) -> Result<SynapseCalyxObservationPutReadback, SynapseCalyxError> {
        let outcome = self
            .vault
            .put_observation_with_outcome(constellation)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("put native Calyx observation constellation", &error)
            })?;
        Ok(SynapseCalyxObservationPutReadback {
            cx_id: outcome.cx_id.to_string(),
            disposition: outcome.disposition.into(),
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Writes a batch of content-addressed observations through Aster's native
    /// constellation ingestion path under one durable commit lock.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if schema validation,
    /// duplicate compatibility checks, ledger append, WAL commit, or any
    /// native Base/Slot/Scalars row write fails.
    pub fn put_observation_constellation_batch<I>(
        &self,
        constellations: I,
    ) -> Result<Vec<SynapseCalyxObservationPutReadback>, SynapseCalyxError>
    where
        I: IntoIterator<Item = Constellation>,
    {
        let outcomes = self
            .vault
            .put_observation_batch_with_outcomes(constellations)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "put native Calyx observation constellation batch",
                    &error,
                )
            })?;
        let latest_seq = self.vault.latest_seq();
        Ok(outcomes
            .into_iter()
            .map(|outcome| SynapseCalyxObservationPutReadback {
                cx_id: outcome.cx_id.to_string(),
                disposition: outcome.disposition.into(),
                latest_seq,
            })
            .collect())
    }

    /// Projects one caller-identified physical event into a bounded native
    /// Calyx recurrence series exactly once.
    ///
    /// Replaying the same identity with the same timestamp/context is an
    /// idempotent success. Reusing the identity for different evidence fails
    /// closed in Aster before commit.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the subject Base row is absent, the
    /// event/context is invalid, identity evidence conflicts, or the atomic
    /// Base+Recurrence commit/readback fails.
    pub fn append_recurrence_occurrence_once(
        &self,
        cx_id: CxId,
        event_time_secs: i64,
        observed_at_secs: i64,
        context: Vec<u8>,
        occurrence_identity_sha256: [u8; 32],
    ) -> Result<SynapseCalyxRecurrenceAppendReadback, SynapseCalyxError> {
        let context = OccurrenceContext::new(context).map_err(|error| {
            SynapseCalyxError::from_calyx("validate native Calyx recurrence context", &error)
        })?;
        let outcome = append_occurrence_once(
            &self.vault,
            cx_id,
            EpochSecs(event_time_secs),
            context,
            EpochSecs(observed_at_secs),
            RetentionPolicy::default(),
            occurrence_identity_sha256,
        )
        .map_err(|error| {
            SynapseCalyxError::from_calyx("append native Calyx recurrence occurrence", &error)
        })?;
        let readback = read_series_readback(&self.vault, cx_id).map_err(|error| {
            SynapseCalyxError::from_calyx(
                "read native Calyx recurrence series after append",
                &error,
            )
        })?;
        Ok(SynapseCalyxRecurrenceAppendReadback {
            cx_id: cx_id.to_string(),
            occurrence_id: outcome.occurrence_id.0,
            disposition: outcome.disposition.into(),
            frequency: readback.series.frequency,
            active_occurrences: readback.series.occurrences.len(),
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Atomically publishes one new measured constellation, its physical KV
    /// source rows, and one outcome occurrence below an existing recurrence
    /// subject.
    ///
    /// # Errors
    ///
    /// Returns a structured error when recurrence validation or the atomic
    /// constellation, source-row, occurrence, and ledger commit fails.
    #[allow(clippy::too_many_arguments)]
    pub fn append_recurrence_occurrence_with_constellation_rows(
        &self,
        recurrence_cx_id: CxId,
        event_time_secs: i64,
        observed_at_secs: i64,
        context: Vec<u8>,
        occurrence_identity_sha256: [u8; 32],
        constellation: Constellation,
        source_rows: Vec<SynapseCalyxCfWrite>,
        ledger_payload: Vec<u8>,
    ) -> Result<SynapseCalyxAtomicConstellationRecurrenceReadback, SynapseCalyxError> {
        let context = OccurrenceContext::new(context).map_err(|error| {
            SynapseCalyxError::from_calyx("validate atomic recurrence context", &error)
        })?;
        let constellation_cx_id = constellation.cx_id;
        let source_row_count = source_rows.len();
        let outcome = append_occurrence_with_constellation_and_rows(
            &self.vault,
            ConstellationRecurrenceAppendRequest {
                recurrence: RecurrenceAppendOnceRequest {
                    cx_id: recurrence_cx_id,
                    t_k: EpochSecs(event_time_secs),
                    context,
                    observed_at: EpochSecs(observed_at_secs),
                    retention: RetentionPolicy::default(),
                    dedup_key_sha256: occurrence_identity_sha256,
                },
                constellation,
                source_rows: source_rows
                    .into_iter()
                    .map(|row| (row.cf, row.key, row.value))
                    .collect(),
                ledger_payload,
            },
        )
        .map_err(|error| {
            SynapseCalyxError::from_calyx(
                "atomically append recurrence occurrence with source constellation",
                &error,
            )
        })?;
        Ok(SynapseCalyxAtomicConstellationRecurrenceReadback {
            constellation_cx_id: constellation_cx_id.to_string(),
            recurrence_cx_id: recurrence_cx_id.to_string(),
            occurrence_id: outcome.occurrence_id.0,
            committed_seq: outcome.committed_seq,
            latest_seq: self.vault.latest_seq(),
            source_row_count,
        })
    }

    /// Runs honesty-gated Oracle consequence prediction over persisted action
    /// evidence and returns the exact serializable Calyx report.
    ///
    /// # Errors
    ///
    /// Returns a structured error when inputs are invalid, Oracle refuses the
    /// persisted evidence, or the report cannot be encoded.
    pub fn oracle_predict_action(
        &self,
        action_id: &str,
        domain: &str,
        panel: Panel,
    ) -> Result<serde_json::Value, SynapseCalyxError> {
        let action_id = nonblank(action_id, "Oracle action id")?;
        let domain = nonblank(domain, "Oracle domain")?;
        let prediction = calyx_oracle::oracle_predict(
            &self.vault,
            &calyx_oracle::Action {
                action_id: action_id.to_owned(),
                panel,
                guard: None,
            },
            calyx_oracle::DomainId::new(domain),
            &SystemClock,
        )
        .map_err(|error| oracle_error(&error))?;
        serde_json::to_value(prediction).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_ORACLE_RESPONSE_ENCODE_FAILED",
                format!("encode Oracle prediction response: {error}"),
                "preserve the vault and inspect the Oracle result schema",
            )
        })
    }

    /// Runs the persisted reverse Oracle walk for one exact outcome value.
    ///
    /// # Errors
    ///
    /// Returns a structured error when inputs are invalid, the reverse walk
    /// fails, or the report cannot be encoded.
    pub fn oracle_reverse_action(
        &self,
        outcome: &AnchorValue,
        domain: &str,
    ) -> Result<serde_json::Value, SynapseCalyxError> {
        let domain = nonblank(domain, "Oracle domain")?;
        let causes = calyx_oracle::reverse_query(
            &self.vault,
            outcome,
            calyx_oracle::DomainId::new(domain),
            &SystemClock,
        )
        .map_err(|error| oracle_error(&error))?;
        serde_json::to_value(causes).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_ORACLE_RESPONSE_ENCODE_FAILED",
                format!("encode Oracle reverse response: {error}"),
                "preserve the vault and inspect the Oracle result schema",
            )
        })
    }

    /// Appends one occurrence and, only when it is the subject's first, commits
    /// its exact region outbox row in the same durable batch.
    ///
    /// # Errors
    ///
    /// Returns a structured error when recurrence validation or the atomic
    /// occurrence and first-region commit fails.
    #[allow(clippy::too_many_arguments)]
    pub fn append_recurrence_occurrence_once_with_region(
        &self,
        cx_id: CxId,
        event_time_secs: i64,
        observed_at_secs: i64,
        context: Vec<u8>,
        occurrence_identity_sha256: [u8; 32],
        region_kind: &str,
        region_id: &str,
        trigger_cx_id: CxId,
    ) -> Result<SynapseCalyxRecurrenceRegionAppendReadback, SynapseCalyxError> {
        let context = OccurrenceContext::new(context).map_err(|error| {
            SynapseCalyxError::from_calyx("validate native Calyx recurrence context", &error)
        })?;
        let mut region = None;
        let outcome = append_occurrence_once_with_rows(
            &self.vault,
            RecurrenceAppendOnceRequest {
                cx_id,
                t_k: EpochSecs(event_time_secs),
                context,
                observed_at: EpochSecs(observed_at_secs),
                retention: RetentionPolicy::default(),
                dedup_key_sha256: occurrence_identity_sha256,
            },
            |occurrence_id, frequency, commit_seq| {
                if frequency != 1 {
                    return Ok(Vec::new());
                }
                let finding = SynapseCalyxPersistedRegionFinding {
                    region_kind: region_kind.to_owned(),
                    region_id: region_id.to_owned(),
                    subject_cx_id: cx_id.to_string(),
                    trigger_cx_id: trigger_cx_id.to_string(),
                    occurrence_id: occurrence_id.0,
                    frequency,
                    observed_seq: commit_seq,
                };
                let key = drift::reactive_region_key(&finding);
                let value = serde_json::to_vec(&finding).map_err(|error| {
                    CalyxError::aster_corrupt_shard(format!(
                        "encode atomic recurrence region finding: {error}"
                    ))
                })?;
                region = Some(finding);
                Ok(vec![(ColumnFamily::Reactive, key, value)])
            },
        )
        .map_err(|error| {
            SynapseCalyxError::from_calyx(
                "append native recurrence occurrence with atomic region finding",
                &error,
            )
        })?;
        let readback = read_series_readback(&self.vault, cx_id).map_err(|error| {
            SynapseCalyxError::from_calyx(
                "read native Calyx recurrence series after atomic region append",
                &error,
            )
        })?;
        if let Some(expected) = &region {
            let key = drift::reactive_region_key(expected);
            let bytes = self
                .read_cf_latest(ColumnFamily::Reactive, &key)?
                .ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_READBACK_MISSING",
                        "atomic first-region row is absent after recurrence commit",
                        "stop writers, preserve the vault, and inspect the recurrence commit",
                    )
                })?;
            let actual: SynapseCalyxPersistedRegionFinding = serde_json::from_slice(&bytes)
                .map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_REACTIVE_READBACK_CORRUPT",
                        format!("decode atomic first-region readback: {error}"),
                        "preserve the vault and inspect the named Reactive row",
                    )
                })?;
            if actual != *expected {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_REACTIVE_READBACK_MISMATCH",
                    "atomic first-region row differs from its committed cause",
                    "stop relay, preserve the vault, and inspect the Reactive row",
                ));
            }
        }
        Ok(SynapseCalyxRecurrenceRegionAppendReadback {
            occurrence: SynapseCalyxRecurrenceAppendReadback {
                cx_id: cx_id.to_string(),
                occurrence_id: outcome.occurrence_id.0,
                disposition: outcome.disposition.into(),
                frequency: readback.series.frequency,
                active_occurrences: readback.series.occurrences.len(),
                latest_seq: self.vault.latest_seq(),
            },
            region,
        })
    }

    /// Reads one physical native Calyx recurrence series.
    ///
    /// # Errors
    ///
    /// Returns a structured error when recurrence rows cannot be decoded or
    /// the subject Base frequency is corrupt.
    pub fn read_recurrence_series(
        &self,
        cx_id: CxId,
    ) -> Result<SynapseCalyxRecurrenceSeriesReadback, SynapseCalyxError> {
        let series = read_series_readback(&self.vault, cx_id).map_err(|error| {
            SynapseCalyxError::from_calyx("read native Calyx recurrence series", &error)
        })?;
        Ok(SynapseCalyxRecurrenceSeriesReadback {
            cx_id: cx_id.to_string(),
            series,
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Idempotently registers Calyx's canonical retrieval-only temporal
    /// sidecars for one exact source-panel generation in the native Registry
    /// CF. An incompatible registration for an existing generation fails.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx error when the contract is invalid, the
    /// immutable identity conflicts, or the Registry write/readback fails.
    pub fn register_temporal_panel(
        &self,
        registration: &VaultTemporalPanelRegistration,
    ) -> Result<VaultTemporalPanelRegistrationWrite, SynapseCalyxError> {
        register_vault_temporal_panel(&self.vault, registration).map_err(|error| {
            SynapseCalyxError::from_calyx("register native Calyx temporal panel", &error)
        })
    }

    /// Reserves immutable built-in panel generations in the vault-global
    /// allocator and advances the dynamic allocation watermark beyond them.
    ///
    /// # Errors
    ///
    /// Returns a structured conflict when one generation is already owned by
    /// another panel, or a durability error when CAS/readback fails.
    pub fn reserve_panel_generations(
        &self,
        reservations: &[(String, u32)],
    ) -> Result<PanelGenerationAllocatorReadback, SynapseCalyxError> {
        reserve_vault_panel_generations(&self.vault, reservations).map_err(|error| {
            SynapseCalyxError::from_calyx("reserve native Calyx panel generations", &error)
        })
    }

    /// Allocates one vault-global panel generation idempotently by operation
    /// identity.
    ///
    /// # Errors
    ///
    /// Returns a structured error for invalid identity, ownership conflict,
    /// exhaustion, or a failed atomic Registry-CF readback.
    pub fn allocate_panel_generation(
        &self,
        panel_name: &str,
        operation_id: &str,
    ) -> Result<PanelGenerationAllocation, SynapseCalyxError> {
        allocate_vault_panel_generation(&self.vault, panel_name, operation_id).map_err(|error| {
            SynapseCalyxError::from_calyx("allocate native Calyx panel generation", &error)
        })
    }

    /// Reads the validated vault-global panel generation allocator.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the Registry-CF state is malformed.
    pub fn panel_generation_allocator(
        &self,
    ) -> Result<PanelGenerationAllocatorReadback, SynapseCalyxError> {
        read_vault_panel_generation_allocator(&self.vault).map_err(|error| {
            SynapseCalyxError::from_calyx("read native Calyx panel generation allocator", &error)
        })
    }

    /// Retires every generation a dynamic panel owns other than the successor
    /// that just committed (#2062 ask 1).
    ///
    /// # Errors
    ///
    /// Returns a structured conflict when the successor is unowned, owned by a
    /// different panel, itself retired, or older than a live generation of the
    /// same panel.
    pub fn supersede_panel_generations(
        &self,
        panel_name: &str,
        successor: u32,
    ) -> Result<PanelGenerationSupersession, SynapseCalyxError> {
        supersede_vault_panel_generations(&self.vault, panel_name, successor).map_err(|error| {
            SynapseCalyxError::from_calyx("supersede native Calyx panel generations", &error)
        })
    }

    /// Fails closed when a generation has no durable ownership claim (#2062
    /// ask 2).
    ///
    /// # Errors
    ///
    /// Returns [`calyx_registry::CALYX_PANEL_GENERATION_UNCLAIMED`] when the
    /// allocator's owners map does not claim `panel_generation`.
    pub fn ensure_panel_generation_claimed(
        &self,
        panel_generation: u32,
    ) -> Result<PanelGenerationClaim, SynapseCalyxError> {
        ensure_vault_panel_generation_claimed(&self.vault, panel_generation).map_err(|error| {
            SynapseCalyxError::from_calyx("verify native Calyx panel generation claim", &error)
        })
    }

    /// Reads one exact native temporal panel registration.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the identity is invalid or Registry CF
    /// bytes cannot be read and validated.
    pub fn read_temporal_panel(
        &self,
        panel_name: &str,
        panel_version: u32,
    ) -> Result<Option<VaultTemporalPanelRegistration>, SynapseCalyxError> {
        read_vault_temporal_panel(&self.vault, panel_name, panel_version).map_err(|error| {
            SynapseCalyxError::from_calyx("read native Calyx temporal panel", &error)
        })
    }

    /// Lists every validated native temporal panel registration.
    ///
    /// # Errors
    ///
    /// Returns a structured error when any Registry CF row is malformed,
    /// mis-keyed, or unreadable.
    pub fn list_temporal_panels(
        &self,
    ) -> Result<Vec<VaultTemporalPanelRegistration>, SynapseCalyxError> {
        list_vault_temporal_panels(&self.vault).map_err(|error| {
            SynapseCalyxError::from_calyx("list native Calyx temporal panels", &error)
        })
    }

    /// Applies the exact immutable policy registered for the first candidate's
    /// source panel. Full candidate/panel validation still occurs inside
    /// [`Self::temporal_rerank`].
    ///
    /// # Errors
    ///
    /// Returns a structured error for an empty set, invalid/missing first Base
    /// row, missing panel metadata/registration, or any rerank invariant.
    pub fn temporal_rerank_registered(
        &self,
        candidates: &[SynapseCalyxTemporalCandidate],
        query_time_secs: i64,
        tz_offset_secs: i32,
    ) -> Result<SynapseCalyxTemporalRerankReadback, SynapseCalyxError> {
        let first = candidates.first().ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_CANDIDATE_COUNT_INVALID",
                "temporal rerank candidate set is empty",
                "supply the bounded, non-empty output of content-only primary retrieval",
            )
        })?;
        let cx_id = CxId::from_str(first.cx_id.trim()).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_CX_ID_INVALID",
                format!("first candidate CxId {:?} is invalid: {error}", first.cx_id),
                "supply the exact CxId returned by native Calyx content retrieval",
            )
        })?;
        let constellation = self
            .vault
            .get(cx_id, self.vault.snapshot())
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("read first temporal candidate Base row {cx_id}"),
                    &error,
                )
            })?;
        let panel_name = constellation
            .metadata
            .get(SYNAPSE_PANEL_NAME_METADATA)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_PANEL_NAME_MISSING",
                    format!(
                        "candidate {cx_id} has no non-empty {SYNAPSE_PANEL_NAME_METADATA} metadata"
                    ),
                    "rebuild the source constellation with its exact immutable panel name",
                )
            })?;
        let registration = read_vault_temporal_panel(
            &self.vault,
            panel_name,
            constellation.panel_version,
        )
        .map_err(|error| {
            SynapseCalyxError::from_calyx(
                &format!(
                    "read registered policy for panel {panel_name} generation {}",
                    constellation.panel_version
                ),
                &error,
            )
        })?
        .ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_PANEL_UNREGISTERED",
                format!(
                    "panel {panel_name} generation {} has no native Registry CF temporal contract",
                    constellation.panel_version
                ),
                "register and physically read back the exact immutable panel generation before serving temporal queries",
            )
        })?;
        self.temporal_rerank(
            candidates,
            query_time_secs,
            tz_offset_secs,
            registration.policy,
        )
    }

    /// Applies Calyx's AP-60 dynamic temporal scoring to an already-retrieved
    /// content candidate set at one coherent Base snapshot.
    ///
    /// Primary relevance is caller-supplied and never recomputed from time.
    /// Every candidate must carry explicit active event-time metadata written
    /// by its source projection. Recurrence scoring is intentionally rejected
    /// here because recurrence subjects are separate physical entities; a
    /// policy that claims otherwise would produce dishonest evidence.
    ///
    /// # Errors
    ///
    /// Returns a structured error for empty/oversized/duplicate candidates,
    /// invalid scores or policy, missing/malformed temporal metadata, mixed
    /// panel versions, absent Base rows, or a bound violation.
    #[allow(clippy::too_many_lines)]
    pub fn temporal_rerank(
        &self,
        candidates: &[SynapseCalyxTemporalCandidate],
        query_time_secs: i64,
        tz_offset_secs: i32,
        policy: TemporalPolicy,
    ) -> Result<SynapseCalyxTemporalRerankReadback, SynapseCalyxError> {
        if candidates.is_empty() || candidates.len() > 1_000 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_CANDIDATE_COUNT_INVALID",
                format!(
                    "temporal rerank candidate count must be in 1..=1000; got {}",
                    candidates.len()
                ),
                "supply the bounded, non-empty output of content-only primary retrieval",
            ));
        }
        if policy.recurrence_boost.is_some() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_RECURRENCE_SUBJECT_REQUIRED",
                "event-constellation rerank cannot apply recurrence boost because native recurrence series are keyed by stable app/routine subject CxIds",
                "disable recurrence_boost for event rerank or query the exact recurrence subject series",
            ));
        }
        policy.validate().map_err(|error| {
            SynapseCalyxError::from_calyx("validate Calyx temporal rerank policy", &error)
        })?;

        let snapshot = self.vault.snapshot();
        let mut seen = BTreeSet::new();
        let mut panel_name = None;
        let mut panel_version = None;
        let mut hits = Vec::with_capacity(candidates.len());
        let mut parsed_candidates = Vec::with_capacity(candidates.len());
        for (index, candidate) in candidates.iter().enumerate() {
            if !candidate.base_score.is_finite() || candidate.base_score <= 0.0 {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_BASE_SCORE_INVALID",
                    format!(
                        "candidate {} has base_score {}; expected a finite value greater than zero",
                        candidate.cx_id, candidate.base_score
                    ),
                    "repair content-only primary retrieval so every candidate has a positive finite score",
                ));
            }
            let cx_id = CxId::from_str(candidate.cx_id.trim()).map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_CX_ID_INVALID",
                    format!("candidate CxId {:?} is invalid: {error}", candidate.cx_id),
                    "supply the exact CxId returned by native Calyx content retrieval",
                )
            })?;
            if !seen.insert(cx_id) {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_CANDIDATE_DUPLICATE",
                    format!("candidate CxId {cx_id} occurs more than once"),
                    "deduplicate primary candidates before temporal rerank",
                ));
            }
            let constellation = self.vault.get(cx_id, snapshot).map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!("read temporal candidate Base row {cx_id} at snapshot {snapshot}"),
                    &error,
                )
            })?;
            let candidate_panel_name = constellation
                .metadata
                .get(SYNAPSE_PANEL_NAME_METADATA)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_TEMPORAL_PANEL_NAME_MISSING",
                        format!(
                            "candidate {cx_id} has no non-empty {SYNAPSE_PANEL_NAME_METADATA} metadata"
                        ),
                        "rebuild the source constellation with its exact immutable panel name",
                    )
                })?;
            if let Some(expected) = panel_name.as_deref() {
                if expected != candidate_panel_name {
                    return Err(SynapseCalyxError::new(
                        "SYNAPSE_CALYX_TEMPORAL_MIXED_PANEL",
                        format!(
                            "candidate {cx_id} uses panel {candidate_panel_name}, but the rerank set started with {expected}"
                        ),
                        "partition candidates by exact panel name and version before temporal rerank",
                    ));
                }
            } else {
                panel_name = Some(candidate_panel_name.clone());
            }
            if let Some(expected) = panel_version {
                if expected != constellation.panel_version {
                    return Err(SynapseCalyxError::new(
                        "SYNAPSE_CALYX_TEMPORAL_MIXED_PANEL",
                        format!(
                            "candidate {cx_id} uses panel {}, but the rerank set started with panel {expected}",
                            constellation.panel_version
                        ),
                        "partition candidates by exact panel version before temporal rerank",
                    ));
                }
            } else {
                panel_version = Some(constellation.panel_version);
            }
            let event_time_secs = validated_source_event_time(&constellation)?;
            hits.push(Hit {
                cx_id,
                score: candidate.base_score,
                rank: index + 1,
                event_time_secs: Some(event_time_secs),
                temporal_scores: None,
                causal_confidence: CausalConfidence::Absent,
                causal_gate: None,
                per_lens: Vec::new(),
                cross_terms_used: false,
                guard: None,
                provenance: constellation.provenance,
                provenance_source: ProvenanceSource::Stored,
                freshness: FreshnessTag::fresh(snapshot),
                explain: None,
            });
            parsed_candidates.push((cx_id, candidate.base_score, index + 1));
        }

        let panel_name = panel_name.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_PANEL_NAME_MISSING",
                "non-empty temporal candidate set did not establish a panel name",
                "inspect candidate validation; every persisted Base row must identify its panel",
            )
        })?;
        let panel_version = panel_version.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_PANEL_MISSING",
                "non-empty temporal candidate set did not establish a panel version",
                "inspect candidate validation; every persisted Base row must identify its panel",
            )
        })?;
        let registration = read_vault_temporal_panel(&self.vault, &panel_name, panel_version)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    &format!(
                        "read temporal registration for panel {panel_name} generation {panel_version}"
                    ),
                    &error,
                )
            })?
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_PANEL_UNREGISTERED",
                    format!(
                        "panel {panel_name} generation {panel_version} has no native Registry CF temporal contract"
                    ),
                    "register and physically read back the exact immutable panel generation before serving temporal queries",
                )
            })?;
        if registration.policy != policy {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_TEMPORAL_POLICY_DRIFT",
                format!(
                    "requested temporal policy differs from registered panel {panel_name} generation {panel_version} policy"
                ),
                "use the exact policy stored in the native Registry CF or publish a new panel generation",
            ));
        }

        let ranked =
            apply_temporal_boost(hits, &registration.policy, query_time_secs, tz_offset_secs)
                .map_err(|error| {
                    SynapseCalyxError::from_calyx(
                        "apply Calyx temporal post-retrieval boost",
                        &error,
                    )
                })?;
        let mut out = Vec::with_capacity(ranked.len());
        for hit in ranked {
            let (base_score, original_rank) = parsed_candidates
                .iter()
                .find(|(cx_id, _, _)| *cx_id == hit.cx_id)
                .map(|(_, score, rank)| (*score, *rank))
                .ok_or_else(|| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_TEMPORAL_RANKING_CORRUPT",
                        format!(
                            "ranked hit {} was not present in primary candidates",
                            hit.cx_id
                        ),
                        "inspect the Calyx temporal scorer and candidate identity handling",
                    )
                })?;
            let max_score = base_score * (1.0 + registration.policy.boost.post_retrieval_alpha);
            if hit.score > max_score.abs().mul_add(1.0e-6, max_score) {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_BOUND_VIOLATION",
                    format!(
                        "candidate {} base_score={base_score} scored {} above AP-60 maximum {max_score}",
                        hit.cx_id, hit.score
                    ),
                    "inspect temporal fusion weights and keep the post-retrieval multiplier within the validated alpha",
                ));
            }
            let temporal_scores = hit.temporal_scores.ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_EVIDENCE_MISSING",
                    format!("candidate {} returned without temporal score evidence", hit.cx_id),
                    "inspect the Calyx temporal scorer; never publish a rerank without per-component evidence",
                )
            })?;
            let event_time_secs = hit.event_time_secs.ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_TEMPORAL_EVENT_TIME_DROPPED",
                    format!(
                        "candidate {} lost its validated event time during temporal scoring",
                        hit.cx_id
                    ),
                    "inspect Calyx temporal scoring; scored hits must retain their source event time",
                )
            })?;
            out.push(SynapseCalyxTemporalRankedHit {
                cx_id: hit.cx_id.to_string(),
                event_time_secs,
                original_rank,
                rank: hit.rank,
                base_score,
                score: hit.score,
                temporal_scores,
            });
        }
        Ok(SynapseCalyxTemporalRerankReadback {
            snapshot_seq: snapshot,
            panel_name,
            panel_version,
            panel_registered_at_unix_ms: registration.registered_at_unix_ms,
            query_time_secs,
            tz_offset_secs,
            temporal_lenses: vec![
                "E2_Temporal_Recent".to_owned(),
                "E3_Temporal_Periodic".to_owned(),
                "E4_Temporal_Positional".to_owned(),
            ],
            policy: registration.policy,
            pre_boost_ranking: candidates
                .iter()
                .map(|candidate| candidate.cx_id.clone())
                .collect(),
            hits: out,
        })
    }

    /// Writes grounded anchors through Aster's ledger-stamped anchor path.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the target constellation is
    /// absent, anchor validation fails, a conflicting anchor already exists, the
    /// ledger entry cannot be appended, or the durable commit/readback fails.
    pub fn put_grounding_anchors(
        &self,
        cx_id: CxId,
        anchors: Vec<Anchor>,
        payload: Vec<u8>,
        actor_service: impl Into<String>,
    ) -> Result<SynapseCalyxAnchorWriteReadback, SynapseCalyxError> {
        let anchor_count = anchors.len();
        let actor = ActorId::Service(actor_service.into());
        let ledger_ref = self
            .vault
            .anchors_with_ledger_entry(
                cx_id,
                anchors,
                EntryKind::Grounding,
                SubjectId::Cx(cx_id),
                payload,
                actor,
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx("put ledger-stamped Calyx anchors", &error)
            })?;
        Ok(SynapseCalyxAnchorWriteReadback {
            cx_id: cx_id.to_string(),
            anchor_count,
            ledger_seq: ledger_ref.seq,
            ledger_hash: hex_bytes(&ledger_ref.hash),
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Writes grounded anchors for many content-addressed constellations in
    /// one durable commit.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if any target constellation is
    /// absent, anchor validation fails, a conflicting anchor already exists,
    /// ledger append fails, or the durable commit cannot apply the full batch.
    pub fn put_grounding_anchors_for_many(
        &self,
        entries: Vec<(CxId, Vec<Anchor>)>,
        payload: Vec<u8>,
        actor_service: impl Into<String>,
    ) -> Result<SynapseCalyxAnchorBatchWriteReadback, SynapseCalyxError> {
        let actor = ActorId::Service(actor_service.into());
        let outcome = self
            .vault
            .anchors_for_many_with_ledger_entry(
                entries,
                EntryKind::Grounding,
                SubjectId::Query(b"synapse.grounding_anchor.multi.v1".to_vec()),
                payload,
                actor,
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "put multi-constellation ledger-stamped Calyx anchors",
                    &error,
                )
            })?;
        Ok(anchor_batch_write_readback(
            &outcome,
            self.vault.latest_seq(),
        ))
    }

    /// Atomically publishes physical source rows and the grounded native
    /// observation derived from them under one Aster commit.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error before visibility when source
    /// rows are empty/duplicated/non-KV, the source or constellation already
    /// exists, schema/grounding validation fails, or the ledger/WAL commit
    /// cannot complete.
    pub fn put_grounded_observation_with_source_rows(
        &self,
        source_rows: Vec<SynapseCalyxCfWrite>,
        content_addressed_source_identity: Vec<u8>,
        constellation: Constellation,
        anchor: Anchor,
        ledger_payload: Vec<u8>,
        actor_service: impl Into<String>,
    ) -> Result<SynapseCalyxGroundedObservationReadback, SynapseCalyxError> {
        let outcome = self
            .vault
            .put_grounded_observation_with_source_rows(
                source_rows
                    .into_iter()
                    .map(|row| (row.cf, row.key, row.value))
                    .collect(),
                content_addressed_source_identity,
                constellation,
                anchor,
                ledger_payload,
                ActorId::Service(actor_service.into()),
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "put atomic grounded Calyx observation with source rows",
                    &error,
                )
            })?;
        Ok(SynapseCalyxGroundedObservationReadback {
            cx_id: outcome.cx_id.to_string(),
            disposition: outcome.disposition.into(),
            ledger_seq: outcome.ledger_ref.seq,
            ledger_hash: hex_bytes(&outcome.ledger_ref.hash),
            source_row_count: outcome.source_row_count,
            committed_seq: outcome.committed_seq,
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Atomically publishes revision-guarded physical source rows and the
    /// grounded native observation derived from them under one Aster commit.
    ///
    /// Every source row must have exactly one guard. The guard comparison,
    /// source rows, constellation, anchor, provenance ledger row, and WAL/MVCC
    /// sequence are one visibility boundary.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error before mutation when a guard
    /// differs or when validation fails, and reports any ledger/WAL/MVCC
    /// durability failure without publishing a partial source transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn put_guarded_grounded_observation_with_source_rows(
        &self,
        source_rows: Vec<SynapseCalyxCfWrite>,
        source_guards: Vec<SynapseCalyxRevisionGuard>,
        content_addressed_source_identity: Vec<u8>,
        constellation: Constellation,
        anchor: Anchor,
        ledger_payload: Vec<u8>,
        actor_service: impl Into<String>,
    ) -> Result<SynapseCalyxGroundedObservationReadback, SynapseCalyxError> {
        let outcome = self
            .vault
            .put_guarded_grounded_observation_with_source_rows(
                source_rows
                    .into_iter()
                    .map(|row| (row.cf, row.key, row.value))
                    .collect(),
                source_guards.into_iter().map(Into::into).collect(),
                content_addressed_source_identity,
                constellation,
                anchor,
                ledger_payload,
                ActorId::Service(actor_service.into()),
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "put atomic guarded grounded Calyx observation with source rows",
                    &error,
                )
            })?;
        Ok(SynapseCalyxGroundedObservationReadback {
            cx_id: outcome.cx_id.to_string(),
            disposition: outcome.disposition.into(),
            ledger_seq: outcome.ledger_ref.seq,
            ledger_hash: hex_bytes(&outcome.ledger_ref.hash),
            source_row_count: outcome.source_row_count,
            committed_seq: outcome.committed_seq,
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Atomically publishes revision-guarded physical source rows and several
    /// grounded native observations under one ledger and WAL/MVCC commit.
    ///
    /// # Errors
    ///
    /// Returns a structured error before visibility for invalid members,
    /// revision conflicts, or any ledger/WAL/MVCC durability failure.
    pub fn put_guarded_grounded_observation_batch_with_source_rows(
        &self,
        source_rows: Vec<SynapseCalyxCfWrite>,
        source_guards: Vec<SynapseCalyxRevisionGuard>,
        members: Vec<(Vec<u8>, Constellation, Anchor)>,
        ledger_payload: Vec<u8>,
        actor_service: impl Into<String>,
    ) -> Result<SynapseCalyxGroundedObservationBatchReadback, SynapseCalyxError> {
        let outcome = self
            .vault
            .put_guarded_grounded_observation_batch_with_source_rows(
                source_rows
                    .into_iter()
                    .map(|row| (row.cf, row.key, row.value))
                    .collect(),
                source_guards.into_iter().map(Into::into).collect(),
                members
                    .into_iter()
                    .map(
                        |(content_addressed_source_identity, constellation, anchor)| {
                            calyx_aster::vault::GroundedObservationBatchMember {
                                content_addressed_source_identity,
                                constellation,
                                anchor,
                            }
                        },
                    )
                    .collect(),
                ledger_payload,
                ActorId::Service(actor_service.into()),
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "put atomic guarded grounded Calyx observation batch with source rows",
                    &error,
                )
            })?;
        Ok(SynapseCalyxGroundedObservationBatchReadback {
            cx_ids: outcome.cx_ids.iter().map(ToString::to_string).collect(),
            ledger_seq: outcome.ledger_ref.seq,
            ledger_hash: hex_bytes(&outcome.ledger_ref.hash),
            source_row_count: outcome.source_row_count,
            committed_seq: outcome.committed_seq,
            latest_seq: self.vault.latest_seq(),
        })
    }

    /// Runs one physical Aster compaction attempt for the Synapse KV storage CF.
    ///
    /// Synapse maps its storage column families onto namespaces inside Aster's
    /// `ColumnFamily::Kv`, so a successful physical KV compaction covers the
    /// complete Synapse storage surface.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the compaction bridge cannot
    /// flush, verify manifest coverage, write compacted SST output, or reclaim
    /// superseded inputs.
    pub fn compact_kv_once(&self) -> Result<bool, SynapseCalyxError> {
        self.vault
            .compact_cf_once(ColumnFamily::Kv)
            .map(|result| matches!(result, Some(CompactionResult::Compacted(_))))
            .map_err(|error| SynapseCalyxError::from_calyx("compact Calyx KV CF", &error))
    }

    /// Produces a durable, self-verifying online backup of the live vault.
    ///
    /// The sacred vault state is copied at a consistent `durable_seq` under the
    /// native-compaction guard into `<target_root>/vault`, then immediately
    /// re-derived with the read-only restore verifier; a manifest with per-file
    /// SHA-256 digests is published only after verification passes. Vault
    /// residency is enforced: a pinned dataset refuses an off-dataset target.
    /// The vault lineage journal is captured as a sidecar so the backup records
    /// which vault it restores.
    ///
    /// # Errors
    ///
    /// Fails closed on WAL sync failure, a residency violation, a target inside
    /// the vault, a busy maintenance guard, any copy error, a missing lineage
    /// journal, or a backup that does not verify.
    pub fn backup(
        &self,
        target_root: &Path,
        include_regenerable: bool,
    ) -> Result<SynapseCalyxBackupReport, SynapseCalyxError> {
        let parent = target_root.parent().ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_BACKUP_TARGET_INVALID",
                format!(
                    "backup target {} has no parent directory",
                    target_root.display()
                ),
                "provide an absolute fresh backup target beneath an existing directory",
            )
        })?;
        target_root.file_name().ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_BACKUP_TARGET_INVALID",
                format!(
                    "backup target {} has no final directory name",
                    target_root.display()
                ),
                "provide an absolute fresh backup target beneath an existing directory",
            )
        })?;
        if target_root.exists() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_BACKUP_TARGET_EXISTS",
                format!(
                    "backup final target {} already exists; a final path is reserved exclusively for a completely verified backup",
                    target_root.display()
                ),
                "choose a fresh nonexistent target path; inspect or remove the existing artifact explicitly",
            ));
        }
        std::fs::create_dir(target_root).map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_BACKUP_TARGET_CREATE_FAILED",
                "create fresh backup target",
                target_root,
                &error,
                "ensure the target parent exists and permits directory creation, then retry with a fresh target",
            )
        })?;
        let marker_path = target_root.join(backup::BACKUP_IN_PROGRESS_FILE);
        let marker = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "synapse_backup_in_progress/v1",
            "pid": std::process::id(),
            "target": target_root,
        }))
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_BACKUP_MARKER_ENCODE_FAILED",
                format!("encode backup in-progress marker: {error}"),
                "repair marker serialization before retrying the backup",
            )
        })?;
        calyx_aster::durable_fs::write_atomic_replace(
            &marker_path,
            &marker,
            "backup in-progress marker",
        )
        .map_err(|error| {
            SynapseCalyxError::from_calyx("write backup in-progress marker", &error)
        })?;
        let result = self.backup_to_staging(target_root, target_root, parent, include_regenerable);
        if let Err(original) = &result
            && target_root.exists()
            && let Err(cleanup) = std::fs::remove_dir_all(target_root)
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_BACKUP_STAGING_CLEANUP_FAILED",
                format!(
                    "backup failed with [{original}], then cleanup of in-progress target {} also failed: {cleanup}",
                    target_root.display()
                ),
                "stop any process holding the named staging path, remove it explicitly, and retry the backup",
            ));
        }
        result
    }

    fn backup_to_staging(
        &self,
        target_root: &Path,
        staging_root: &Path,
        publication_parent: &Path,
        include_regenerable: bool,
    ) -> Result<SynapseCalyxBackupReport, SynapseCalyxError> {
        // 1. Barrier the WAL group-committer so every accepted write is durable
        //    before the consistent copy reads the tree.
        self.flush()?;
        let staging_vault_dir = staging_root.join(backup::BACKUP_VAULT_SUBDIR);
        let backup_vault_dir = target_root.join(backup::BACKUP_VAULT_SUBDIR);
        // 2. Enforce vault residency against the backup vault directory.
        let residency_enforced =
            backup::authorize_residency(&self.config.vault_dir, &backup_vault_dir)?;
        // 3. Consistent copy under the native-compaction guard. The daemon's own
        //    lock/pid/lifecycle files share the vault directory and are held with
        //    mandatory byte-range locks, so they are excluded by name — never by
        //    tolerating a read failure, which would let real data vanish.
        let runtime_names = vault_runtime::daemon_runtime_file_names();
        let runtime_refs = runtime_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<&str>>();
        let aster_report = self
            .vault
            .backup_consistent(&staging_vault_dir, include_regenerable, &runtime_refs)
            .map_err(|error| SynapseCalyxError::from_calyx("back up Calyx vault", &error))?;
        let latest_seq = self.vault.latest_seq();
        // 4. Prove the copy restores byte-for-byte before publishing a manifest.
        let mut verify = backup::verify_vault_restore(&staging_vault_dir)?;
        backup::require_verified(&verify)?;
        // 5. Capture vault identity. The lineage journal lives outside the vault
        //    directory by design, so the tree copy cannot reach it.
        let mut lineage = backup::capture_lineage(staging_root, &self.lineage)?;
        verify.vault_path.clone_from(&backup_vault_dir);
        lineage.sidecar_path = target_root.join(backup::BACKUP_LINEAGE_FILE);
        let copied = backup::CopiedVaultState::from_aster(aster_report);
        let mut report = SynapseCalyxBackupReport {
            vault_id: self.vault.vault_id().to_string(),
            source_vault_dir: copied.source_vault_dir,
            target_root: target_root.to_path_buf(),
            backup_vault_dir,
            manifest_path: PathBuf::new(),
            manifest_sha256: String::new(),
            durable_seq: copied.durable_seq,
            latest_seq,
            include_regenerable,
            file_count: copied.file_count,
            total_bytes: copied.total_bytes,
            residency_enforced,
            files: copied.files,
            excluded_runtime: copied.excluded_runtime,
            pinned_manifest: copied.pinned_manifest,
            tolerated_absences: copied.tolerated_absences,
            lineage,
            verify,
        };
        // 6. Publish the manifest and record its own hash.
        let (_, manifest_sha256) = backup::write_manifest(staging_root, &report)?;
        report.manifest_path = target_root.join(backup::BACKUP_MANIFEST_FILE);
        report.manifest_sha256 = manifest_sha256;
        let marker_path = staging_root.join(backup::BACKUP_IN_PROGRESS_FILE);
        std::fs::remove_file(&marker_path).map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_BACKUP_PUBLISH_FAILED",
                "remove backup in-progress marker after manifest verification",
                &marker_path,
                &error,
                "remove the marker only after independently verifying backup_manifest.json and the vault; until then the target remains in progress",
            )
        })?;
        sync_dir(
            publication_parent,
            "backup publication",
            "SYNAPSE_CALYX_BACKUP_PUBLISH_SYNC_FAILED",
            "inspect the final backup and parent volume health; do not treat publication as durable until the parent directory sync succeeds",
        )?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_BACKUP_COMPLETED",
            vault_dir = %report.source_vault_dir.display(),
            backup_vault_dir = %report.backup_vault_dir.display(),
            durable_seq = report.durable_seq,
            latest_seq = report.latest_seq,
            file_count = report.file_count,
            total_bytes = report.total_bytes,
            verify_success = report.verify.success,
            ledger_tip_hash = %report.verify.ledger_tip_hash,
            lineage_sha256 = %report.lineage.sha256,
            lineage_generation = report.lineage.generation,
            lineage_chain_origin = %report.lineage.chain_origin,
            covers_full_history = report.lineage.covers_full_history,
            excluded_runtime_count = report.excluded_runtime.len(),
            pinned_manifest = report
                .pinned_manifest
                .as_ref()
                .map_or("<none>", |pin| pin.pointer.as_str()),
            current_advanced_to = report
                .pinned_manifest
                .as_ref()
                .and_then(|pin| pin.current_advanced_to.as_deref())
                .unwrap_or("<unchanged>"),
            tolerated_absence_count = report.tolerated_absences.len(),
            "completed durable Calyx vault backup and restore verification"
        );
        Ok(report)
    }

    /// Drains urgent file-count debt across native Aster column families and
    /// returns physical reclamation readback. Each rewrite is file/byte
    /// bounded; the pass covers every CF near the shared page-source ceiling.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if manifest coverage cannot be
    /// proven, an SST is malformed, output publication fails, router refresh
    /// fails, or any proven superseded input cannot be reclaimed.
    pub fn compact_native_fanout_once(
        &self,
    ) -> Result<SynapseCalyxNativeFanoutReadback, SynapseCalyxError> {
        let results = self.vault.compact_native_fanout_once().map_err(|error| {
            SynapseCalyxError::from_calyx("compact native Calyx CF fan-out", &error)
        })?;
        Ok(Self::native_fanout_readback(results))
    }

    /// Open-time readiness variant (#1812): compacts only the CFs whose file
    /// count has reached the write-stall trigger, so a booting daemon can
    /// accept its first write (the activity recorder writes `CF_TIMELINE`
    /// immediately at startup) without paying a full-vault fan-out pass. The
    /// routine and tiny-file drain lanes stay owned by the periodic GC task.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if manifest coverage cannot be
    /// proven, an SST is malformed, output publication fails, router refresh
    /// fails, or any proven superseded input cannot be reclaimed.
    pub fn compact_write_stall_readiness_once(
        &self,
    ) -> Result<SynapseCalyxNativeFanoutReadback, SynapseCalyxError> {
        let results = self
            .vault
            .compact_write_stall_readiness_once()
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "compact write-stalled Calyx CFs for open readiness",
                    &error,
                )
            })?;
        Ok(Self::native_fanout_readback(results))
    }

    fn native_fanout_readback(results: Vec<CompactionResult>) -> SynapseCalyxNativeFanoutReadback {
        let attempted_cfs = results.len();
        let mut compacted_cfs = 0_usize;
        let mut reclaimed_input_files = 0_usize;
        let mut input_bytes = 0_u64;
        let mut output_bytes = 0_u64;
        let mut compacted_cf_names = Vec::new();
        for result in results {
            if let CompactionResult::Compacted(report) = result {
                compacted_cfs = compacted_cfs.saturating_add(1);
                reclaimed_input_files =
                    reclaimed_input_files.saturating_add(report.reclaimed_input_files);
                input_bytes = input_bytes.saturating_add(report.input_bytes);
                output_bytes = output_bytes.saturating_add(report.output_bytes);
                compacted_cf_names.push(report.cf.name().clone());
            }
        }
        SynapseCalyxNativeFanoutReadback {
            attempted_cfs,
            compacted_cfs,
            skipped_cfs: attempted_cfs.saturating_sub(compacted_cfs),
            reclaimed_input_files,
            input_bytes,
            output_bytes,
            compacted_cf_names,
        }
    }

    /// Prunes durable MVCC tombstones from the Synapse KV storage CF.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the vault cannot flush,
    /// compact, rewrite compacted SST output, or reclaim superseded inputs.
    pub fn purge_kv_tombstones(&self) -> Result<(), SynapseCalyxError> {
        self.vault
            .purge_tombstoned_cfs(&[ColumnFamily::Kv])
            .map_err(|error| SynapseCalyxError::from_calyx("purge Calyx KV tombstones", &error))
    }

    /// Records that one damaged raw-commitment cohort seal is permanently
    /// unverifiable, so verification of every later seal can resume.
    ///
    /// A cohort seal is the payload of an append-only Ledger entry whose hash
    /// every later entry chains to, so a seal torn by a crash can never be
    /// repaired in place. Left unrecorded it is not merely one lost cohort: the
    /// verifier latches on its first failure, so the vault stops being verified
    /// at all, permanently, including everything written afterwards.
    ///
    /// This appends one `Admin` governance entry. It repairs nothing and hides
    /// nothing - the damage stays in the chain and in every readback, and the
    /// vault can never again report `verified` while the exception stands.
    ///
    /// The write is guarded three ways. The vault must currently be failing on
    /// exactly `ledger_seq`; the failure's adjudication digest must equal
    /// `expected_failure_sha256`, which the caller must have read from a
    /// verification readback; and the recorded digest binds the sequence to the
    /// byte-exact diagnostic, so the exception dies the moment the damage
    /// changes.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the vault is not currently failing on
    /// that seal, when the presented digest does not match, or when the Ledger
    /// append fails.
    pub fn adjudicate_raw_commitment_seal(
        &self,
        ledger_seq: u64,
        expected_failure_sha256: &str,
        reason: &str,
    ) -> Result<SynapseCalyxSealAdjudicationReceipt, SynapseCalyxError> {
        let report = self.verify_ledger_chain(None)?;
        let Some(diagnostic) = report.raw_commitment_failure.clone() else {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEAL_ADJUDICATION_NO_FAILURE",
                format!(
                    "the vault reports no raw-commitment seal failure to adjudicate; verdict={} adjudicated_count={}",
                    report.verdict, report.raw_commitment_adjudicated_count
                ),
                "re-run audit operation=verify_chain; only a seal the verifier is currently failing on may be adjudicated",
            ));
        };
        let actual_sha256 = report.raw_commitment_failure_sha256.unwrap_or_default();
        // The operator must present the exact digest of the exact damage they
        // reviewed. A stale guard means the vault moved under them, and the
        // write must not proceed on a diagnostic nobody read.
        if !actual_sha256.eq_ignore_ascii_case(expected_failure_sha256.trim()) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEAL_ADJUDICATION_GUARD_MISMATCH",
                format!(
                    "presented failure digest does not match the vault's current raw-commitment failure; expected_failure_sha256={expected_failure_sha256} actual_failure_sha256={actual_sha256} failing_diagnostic={diagnostic}"
                ),
                "re-run audit operation=verify_chain and pass the raw_commitment_failure_sha256 it reports",
            ));
        }
        if !diagnostic.contains(&format!("Ledger seal {ledger_seq} ")) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEAL_ADJUDICATION_SEQ_MISMATCH",
                format!(
                    "the vault is not currently failing on Ledger seal {ledger_seq}; failing_diagnostic={diagnostic}"
                ),
                "adjudicate the exact seal named in the current raw_commitment_failure",
            ));
        }
        let ledger_ref = self
            .vault
            .adjudicate_raw_commitment_seal(ledger_seq, &diagnostic, reason)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("append raw-commitment seal adjudication", &error)
            })?;
        Ok(SynapseCalyxSealAdjudicationReceipt {
            adjudicated_ledger_seq: ledger_seq,
            diagnostic,
            diagnostic_sha256: actual_sha256,
            reason: reason.to_owned(),
            adjudication_ledger_seq: ledger_ref.seq,
            adjudication_entry_hash: hex_bytes(&ledger_ref.hash),
        })
    }

    /// Verifies the live provenance-ledger hash chain against the exact stored
    /// bytes, fail-closed. Pass `None` to verify the full chain, or an explicit
    /// `(from_seq, to_seq)` half-open window for an incremental re-walk.
    ///
    /// This is synchronous, CPU/IO-heavy over the whole physical Ledger CF, and
    /// must be driven off the async MCP runtime by its caller.
    ///
    /// # Errors
    ///
    /// Returns a structured error only when the physical Ledger cannot be read;
    /// a detected tamper is a normal broken/corrupt verdict in the report.
    pub fn verify_ledger_chain(
        &self,
        range: Option<(u64, u64)>,
    ) -> Result<SynapseCalyxLedgerVerifyReport, SynapseCalyxError> {
        let range = match range {
            Some((from, to)) if from > to => {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_LEDGER_VERIFY_RANGE_INVALID",
                    format!("ledger verify range start {from} is greater than end {to}"),
                    "request a half-open [from_seq, to_seq) window with from_seq <= to_seq",
                ));
            }
            Some((from, to)) => Some(from..to),
            None => None,
        };
        let verification = self
            .vault
            .verify_ledger_chain(range)
            .map_err(|error| SynapseCalyxError::from_calyx("verify Calyx ledger chain", &error))?;
        Ok(SynapseCalyxLedgerVerifyReport::from_aster(
            verification,
            &self.lineage,
        ))
    }

    /// Runs the scheduled whole-vault verification: the read-only restore
    /// verifier plus the provenance hash-chain verifier, both under the vault's
    /// maintenance guard so a concurrent compaction/GC/backup/erase pass cannot
    /// rewrite the tree mid-scan and be misreported as corruption.
    ///
    /// The chain verification is **incremental by default** — it re-hashes the
    /// most recent `tail_entries` entries rather than the whole Ledger CF. That
    /// is the same posture restic takes with `check --read-data-subset`: full
    /// data re-reads are the expensive exception, so routine scheduled checks
    /// sample recent state and the full scan is requested explicitly. Pass
    /// `full_chain` to re-hash everything.
    ///
    /// # Errors
    ///
    /// Fails closed when the maintenance guard is already held, when the vault
    /// path is not a readable vault, or when the physical ledger cannot be read.
    /// A non-green verdict is a normal report, not an error: the caller decides
    /// how loudly to alarm.
    pub fn verify_vault(
        &self,
        full_chain: bool,
        tail_entries: u64,
    ) -> Result<SynapseCalyxVaultVerifyReport, SynapseCalyxError> {
        self.vault
            .with_maintenance_guard("vault_verify_scan", || {
                Ok(self.verify_vault_under_guard(full_chain, tail_entries))
            })
            .map_err(|error| {
                SynapseCalyxError::from_calyx("acquire Calyx vault maintenance guard", &error)
            })?
    }

    fn verify_vault_under_guard(
        &self,
        full_chain: bool,
        tail_entries: u64,
    ) -> Result<SynapseCalyxVaultVerifyReport, SynapseCalyxError> {
        let vault_dir = self.config.vault_dir.clone();
        // #2059: the live vault appends continuously, so this path uses the
        // anchored-prefix discipline; strict exact-head stays reserved for
        // quiescent restore/backup verification.
        let restore = backup::verify_vault_restore_live(&vault_dir)?;
        let head_height = calyx_aster::ledger_head::read_head_anchor(&vault_dir)
            .map_err(|error| SynapseCalyxError::from_calyx("read Calyx ledger head", &error))?
            .map_or(0, |anchor| anchor.height);
        let tail_entries = tail_entries.max(1);
        let range = if full_chain {
            None
        } else {
            Some((head_height.saturating_sub(tail_entries), head_height))
        };
        let chain = self.verify_ledger_chain(range)?;
        let lineage_path = self.lineage.lineage_path.clone();
        Ok(SynapseCalyxVaultVerifyReport {
            vault_dir,
            vault_id: self.vault.vault_id().to_string(),
            scan_mode: if full_chain {
                "full_chain".to_owned()
            } else {
                "incremental_tail".to_owned()
            },
            ledger_head_height: head_height,
            requested_tail_entries: if full_chain { 0 } else { tail_entries },
            lineage_present: lineage_path.is_file(),
            lineage_path,
            restore,
            chain,
        })
    }

    /// Reads and decodes one physical provenance-ledger entry by sequence.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical row cannot be read or its
    /// bytes cannot be decoded.
    pub fn read_ledger_entry(
        &self,
        seq: u64,
    ) -> Result<SynapseCalyxLedgerEntryReadback, SynapseCalyxError> {
        let entry = self
            .vault
            .read_ledger_entry(seq)
            .map_err(|error| SynapseCalyxError::from_calyx("read Calyx ledger entry", &error))?;
        Ok(entry.map_or_else(
            || SynapseCalyxLedgerEntryReadback::absent(seq),
            |entry| SynapseCalyxLedgerEntryReadback::from_entry(seq, &entry),
        ))
    }

    /// Re-derives a record's recorded provenance binding from the bytes and
    /// bounds drift to a genuine, self-consistent ledger entry.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the record is absent or unreadable; a
    /// provenance mismatch is a normal `reproduced == false` verdict.
    pub fn reproduce_record(
        &self,
        cx_id: &str,
    ) -> Result<SynapseCalyxReproduceReport, SynapseCalyxError> {
        let cx_id = parse_cx_id(cx_id)?;
        let reproduction = self
            .vault
            .reproduce_record_provenance(cx_id)
            .map_err(|error| SynapseCalyxError::from_calyx("reproduce Calyx record", &error))?;
        Ok(SynapseCalyxReproduceReport::from_aster(&reproduction))
    }

    /// Lawfully erases one record (constellation) by content-addressed id:
    /// CF-row tombstone, an append-only `Erase` ledger entry, and physical
    /// purge. Then re-verifies the full hash chain to prove it stays intact
    /// with the new erasure entry sealed in.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the record is already tombstoned or the
    /// tombstone/commit/purge/re-verify path fails; fails closed before any
    /// partial visibility.
    pub fn erase_record(
        &self,
        cx_id: &str,
    ) -> Result<SynapseCalyxErasureReport, SynapseCalyxError> {
        let cx_id = parse_cx_id(cx_id)?;
        let registry = EraseRegistry::new();
        let result = self
            .vault
            .erase_scope_ledger_stamped(EraseScope::Cx(cx_id), &registry)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("erase Calyx record via ledger tombstone", &error)
            })?;
        let (tombstone_present, tombstone_seq, tombstone_hash) = match &result.tombstone {
            Some(tombstone) => {
                let hash = self
                    .vault
                    .read_ledger_entry(tombstone.seq)
                    .map_err(|error| {
                        SynapseCalyxError::from_calyx("read Calyx erasure ledger entry", &error)
                    })?
                    .map(|entry| hex_bytes(&entry.entry_hash));
                (true, Some(tombstone.seq), hash)
            }
            None => (false, None, None),
        };
        let chain_verify = self.verify_ledger_chain(None)?;
        Ok(SynapseCalyxErasureReport {
            scope: format!("cx:{cx_id}"),
            records_deleted: result.records_deleted,
            shredded_at_ms: result.shredded_at,
            tombstone_present,
            tombstone_seq,
            tombstone_hash,
            chain_verify,
        })
    }

    /// Flushes pending Aster checkpoints and performs one bounded physical WAL
    /// recycle pass using the manifest durable sequence as the sole reclaim
    /// authority.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when either bound is zero,
    /// checkpoint/manifest coverage cannot be proven, WAL inventory is
    /// corrupt, or a bounded segment truncate/fsync fails.
    pub fn recycle_durable_wal_once(
        &self,
        max_segments: usize,
        fsync_budget: usize,
    ) -> Result<calyx_aster::wal::WalRecycleReport, SynapseCalyxError> {
        self.vault
            .recycle_durable_wal_once(max_segments, fsync_budget)
            .map_err(|error| SynapseCalyxError::from_calyx("recycle durable Calyx WAL", &error))
    }

    /// Writes raw CF rows through Aster's durable WAL/MVCC commit path.
    ///
    /// This is synchronous by construction. Tokio callers must use
    /// [`SynapseCalyxAsyncVault`] so the call is owned by the vault worker
    /// thread, not an executor worker.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if admission, WAL append,
    /// MVCC apply, checkpoint staging, or any durability guard fails.
    pub fn write_cf_batch(&self, rows: Vec<SynapseCalyxCfWrite>) -> Result<Seq, SynapseCalyxError> {
        self.vault
            .write_cf_batch(rows.into_iter().map(|row| (row.cf, row.key, row.value)))
            .map_err(|error| SynapseCalyxError::from_calyx("write Calyx CF batch", &error))
    }

    /// Commits one raw CF batch only when every physical revision guard still
    /// matches.
    ///
    /// Guards must be non-empty and unique, every guard key must be non-empty,
    /// and every guarded `(cf, key)` may occur at most once in `rows`. A guard
    /// without a matching row is an atomic read-only precondition.
    /// Comparison and the single WAL/MVCC commit share Aster's process and
    /// cross-process commit boundary. A conflict is a non-mutating outcome.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error for malformed guards or rows,
    /// read barriers, read-only state, admission, WAL, MVCC, or durability
    /// failure.
    pub fn write_cf_batch_if_revisions(
        &self,
        guards: Vec<SynapseCalyxRevisionGuard>,
        rows: Vec<SynapseCalyxCfWrite>,
    ) -> Result<SynapseCalyxMultiConditionalWriteOutcome, SynapseCalyxConditionalWriteError> {
        self.vault
            .write_cf_batch_if_revisions(
                guards.into_iter().map(Into::into),
                rows.into_iter().map(|row| (row.cf, row.key, row.value)),
            )
            .map(SynapseCalyxMultiConditionalWriteOutcome::from)
            .map_err(|error| SynapseCalyxConditionalWriteError {
                committed_seq: error.committed_seq,
                source: SynapseCalyxError::from_calyx(
                    "write multi-key revision-guarded Calyx CF batch",
                    &error.source,
                ),
            })
    }

    /// Commits a raw CF batch only when one guarded value still has the
    /// expected physical revision.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the guarded mutation is
    /// malformed or admission, WAL, MVCC, or readback fails. A revision
    /// conflict is returned as `applied = false`.
    pub fn write_cf_batch_if_revision(
        &self,
        guard_cf: ColumnFamily,
        guard_key: &[u8],
        expected_revision_sha256: Option<[u8; 32]>,
        rows: Vec<SynapseCalyxCfWrite>,
    ) -> Result<SynapseCalyxConditionalWriteOutcome, SynapseCalyxError> {
        self.vault
            .write_cf_batch_if_revision(
                guard_cf,
                guard_key,
                expected_revision_sha256,
                rows.into_iter().map(|row| (row.cf, row.key, row.value)),
            )
            .map(SynapseCalyxConditionalWriteOutcome::from)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("write revision-guarded Calyx CF batch", &error)
            })
    }

    /// Reads one raw CF row from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn read_cf_latest(
        &self,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.vault
            .read_cf_latest(cf, key)
            .map_err(|error| SynapseCalyxError::from_calyx("read latest Calyx CF row", &error))
    }

    /// Reads one raw CF row and physical-value revision from one atomic latest
    /// committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the row is blocked or the
    /// serving view cannot be read.
    pub fn read_cf_latest_revisioned(
        &self,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<SynapseCalyxRevisionedValue>, SynapseCalyxError> {
        self.vault
            .read_cf_latest_revisioned(cf, key)
            .map(|value| {
                value.map(|(value, revision_sha256)| SynapseCalyxRevisionedValue {
                    value,
                    revision_sha256,
                })
            })
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read latest revisioned Calyx CF row", &error)
            })
    }

    /// Reads raw CF rows from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if any row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn read_cf_batch_latest(
        &self,
        reads: &[calyx_aster::mvcc::CfRead],
    ) -> Result<Vec<Option<Vec<u8>>>, SynapseCalyxError> {
        self.vault.read_cf_batch_latest(reads).map_err(|error| {
            SynapseCalyxError::from_calyx("read latest Calyx CF row batch", &error)
        })
    }

    /// Reads one raw CF row at a numeric snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the snapshot is stale,
    /// blocked by a read barrier, or unavailable from the opened recovery mode.
    pub fn read_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.vault
            .read_cf_at(snapshot, cf, key)
            .map_err(|error| SynapseCalyxError::from_calyx("read Calyx CF row", &error))
    }

    /// Reads one raw CF row through an explicit pinned snapshot lease.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the lease expired, the row is
    /// blocked by a read barrier, or the opened recovery mode cannot serve it.
    pub fn read_cf_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.vault
            .read_cf_snapshot(snapshot, cf, key)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read Calyx CF row from pinned snapshot", &error)
            })
    }

    /// Returns every key changed in one native column family after an exact
    /// committed sequence and no later than the supplied pinned snapshot.
    /// Tombstoned keys are included.
    ///
    /// This is the exact delta surface used by long-lived storage maintenance
    /// owners. It deliberately accepts the caller's pinned snapshot so the
    /// change list and every subsequent point read describe the same instant.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the requested range cannot
    /// be proven from the process's MVCC history, the lease expired, or the CF
    /// is unavailable in the opened vault mode.
    pub fn changed_cf_keys_after_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        after_exclusive: Seq,
    ) -> Result<Vec<Vec<u8>>, SynapseCalyxError> {
        self.vault
            .changed_cf_keys_after_snapshot(snapshot, cf, after_exclusive)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "read Calyx CF changed-key history from a pinned snapshot",
                    &error,
                )
            })
    }

    /// Scans visible raw CF rows at a numeric snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the snapshot is stale,
    /// blocked by a read barrier, or unavailable from the opened recovery mode.
    pub fn scan_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_at(snapshot, cf)
            .map_err(|error| SynapseCalyxError::from_calyx("scan Calyx CF", &error))
    }

    /// Scans visible raw CF rows through an explicit pinned snapshot lease.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the lease expired, any row is
    /// blocked by a read barrier, or the opened recovery mode cannot serve it.
    pub fn scan_cf_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault.scan_cf_snapshot(snapshot, cf).map_err(|error| {
            SynapseCalyxError::from_calyx("scan Calyx CF from pinned snapshot", &error)
        })
    }

    /// Oldest sequence from which this process can prove exact per-key change
    /// history after a latest-only durable recovery.
    #[must_use]
    pub fn changed_key_history_floor(&self) -> u64 {
        self.vault.changed_key_history_floor()
    }

    /// Scans visible raw CF rows from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if a row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn scan_cf_latest(
        &self,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_latest(cf)
            .map_err(|error| SynapseCalyxError::from_calyx("scan latest Calyx CF", &error))
    }

    /// Counts visible raw CF rows from one atomic latest committed view.
    ///
    /// Equal to `scan_cf_latest(cf)?.len()` without materialising any value
    /// (#1952). Use this for the readback counts that prove a write landed:
    /// `scan_cf_latest` holds the vault-wide row-table read guard for its whole
    /// scan, and #1950 measured that hold reaching 1.4 s, which stalls every
    /// committing writer for the duration.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if a row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn count_cf_latest(&self, cf: ColumnFamily) -> Result<usize, SynapseCalyxError> {
        self.vault
            .count_cf_latest(cf)
            .map_err(|error| SynapseCalyxError::from_calyx("count latest Calyx CF", &error))
    }

    /// Visible rows of one column family in the MVCC row table alone, with the
    /// CF router excluded.
    ///
    /// The physical evidence behind #1978's router gate: on a `full_mvcc_restore`
    /// vault this must equal the same family read through a second handle opened
    /// latest-only, whose only source *is* the router. Manual FSV compares those
    /// independent reads against the physical table count.
    #[must_use]
    pub fn count_cf_latest_table_only(&self, cf: ColumnFamily) -> usize {
        self.vault.latest_row_count_table_only(cf)
    }

    /// Reports whether this handle serves latest reads from the CF router
    /// rather than the in-memory MVCC row table (`restore_mvcc_rows: false`).
    #[must_use]
    pub fn router_latest_readback(&self) -> bool {
        self.vault.router_latest_readback()
    }

    /// Folds every visible row of one column family through one registered
    /// latest snapshot and one persistent immutable merge cursor.
    ///
    /// The row-table/router lock hand-off ends before visitor callbacks, so
    /// writers continue while the walk retains a coherent committed view. Each
    /// immutable SST reader is opened and positioned once, then advances its
    /// reusable buffer one row at a time. This is load-bearing: the previous
    /// `scan_cf_range_page_latest` loop rebuilt and re-sought every source for
    /// every 16 Base rows. A real #2243 kernel rebuild needed 49,915 such opens
    /// before its domain discovery ended and reached a 2.824 GiB process peak;
    /// the persistent scan immediately after it stayed near 831 MiB.
    ///
    /// The snapshot has the bounded intelligence-corpus lease. If I/O or a
    /// visitor cannot finish inside that declared lifetime, the walk fails with
    /// the lease error; it never restarts on a newer view or returns a partial
    /// moving-window result.
    ///
    /// # Errors
    ///
    /// Returns a structured error when `page_rows` is zero, snapshot
    /// registration/validation fails, the immutable stream fails, memory
    /// accounting fails, or the visitor rejects a row. The visitor's error is
    /// propagated verbatim.
    pub fn walk_cf_latest<V>(
        &self,
        cf: ColumnFamily,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        self.walk_cf_range_latest_snapshot(cf, &KeyRange::all(), page_rows, visit)
    }

    /// Walks one column family through an allocation-reusing cursor under an
    /// already-registered reader lease. Every visited row and any point reads
    /// performed by the visitor's enclosing scope therefore describe the same
    /// committed state.
    pub(crate) fn walk_cf_snapshot<V>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        self.walk_cf_range_snapshot(snapshot, cf, &KeyRange::all(), page_rows, visit)
    }

    /// Streams one range from one newly registered latest snapshot while
    /// retaining the immutable merge cursor for the complete walk.
    ///
    /// This is the cross-crate primitive for logical filters such as retention
    /// TTL: a caller can skip any number of physical candidates and stop after
    /// collecting its bounded logical page without reopening and re-seeking
    /// every SST for each all-filtered candidate page.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the page size is zero, snapshot
    /// registration or lease validation fails, an immutable record is corrupt,
    /// a read barrier blocks a row, or the visitor rejects a row.
    pub fn walk_cf_range_latest_snapshot<V>(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        self.with_read_snapshot(INTELLIGENCE_CORPUS_READER_LEASE_MS, |snapshot| {
            self.walk_cf_range_snapshot(snapshot, cf, range, page_rows, visit)
        })
    }

    /// Streams one range through a caller-owned registered snapshot and one
    /// persistent immutable merge cursor.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the supplied lease cannot serve the
    /// range, the stream or allocator accounting fails, or the visitor rejects
    /// a row.
    pub fn walk_cf_range_snapshot<V>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        walk_cf_range_snapshot_stream(&self.vault, snapshot, cf, range, page_rows, visit)
    }

    /// Counts the visible rows of one column family from one pinned physical
    /// snapshot, using one persistent immutable key-state cursor (#1973,
    /// #2239).
    ///
    /// [`Self::count_cf_latest`] reads as `O(1)` and is not: it is identical to
    /// `scan_cf_latest().len()` in time, walking every row of the family under a
    /// single hold of the vault-wide row-table read guard. On the live vault
    /// that measured a **127 ms** hold counting `Graph` (81,236 rows) against a
    /// 25 ms budget, which is the same defect #1968 removed from `Base`, on a
    /// different column family.
    ///
    /// The original bounded implementation reopened and re-sought every
    /// intersecting SST for every 256-row page. On the deployed 6.1-million-row
    /// Graph family that turned one logical count into roughly 24,000 cursor
    /// constructions over 233 immutable files and more than a terabyte of
    /// physical reads. The snapshot path captures the MVCC delta once, opens
    /// the immutable sources once, and carries their merge cursor across every
    /// page. A later deployment proved that reading only winning *values* was
    /// still pathological for exact cardinality: 14.4 million overwritten
    /// records made sparse 64 KiB buffered seeks turn a 5.7 GiB corpus into 64
    /// GiB of physical reads. The count cursor now consumes and CRC-validates
    /// every record sequentially, retains only key plus tombstone state, and
    /// never decrypts or materializes payloads. Physical work is linear in SST
    /// bytes with bounded state per source. The returned walk is always atomic
    /// because every page belongs to the same registered snapshot.
    ///
    /// # Errors
    ///
    /// Propagates snapshot registration, lease, or page-read failures. An
    /// expired count fails explicitly; it never restarts from a moving latest
    /// view or returns a partial count.
    pub fn count_cf_latest_bounded(
        &self,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError> {
        self.with_read_snapshot(CF_COUNT_READER_LEASE_MS, |snapshot| {
            let (rows, pages) = self
                .vault
                .count_cf_snapshot(snapshot, cf, SYNAPSE_CALYX_CF_WALK_PAGE_ROWS)
                .map_err(|error| {
                    SynapseCalyxError::from_calyx("count pinned Calyx CF key states", &error)
                })?;
            Ok(SynapseCalyxCfWalk {
                column_family: cf.name(),
                page_rows: SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
                pages,
                rows_examined: rows,
                rows_visited: rows,
                stopped_early: false,
                snapshot_seq_first: snapshot.seq(),
                snapshot_seq_last: snapshot.seq(),
            })
        })
    }

    /// The same physical count as [`Self::count_cf_latest_bounded`], reusing the
    /// previous measurement **only when nothing has entered or left *this*
    /// column family** since it was taken (#2114, #2139).
    ///
    /// # Why this is a physical readback and not a cache
    ///
    /// The weave's post-write readback is an assertion about what the weave
    /// persisted, and it was being paid at catastrophic granularity: three
    /// panels per maintenance tick, each re-walking `XTerm` + `Graph` in full —
    /// 2.75 M rows on the deployed vault — after every weave call, including
    /// the ones that wrote nothing. Four hours of that was 63.2 M rows/h and
    /// 639 s of wall clock for 483 woven records, and it was the dominant
    /// source of `Graph` row-guard holds, blocking every `Graph` commit.
    ///
    /// # The reuse condition is a proof, not a sample
    ///
    /// Written out in full because the whole value of this function is that the
    /// claim is checkable:
    ///
    /// 1. **Rows enter or leave a column family only through a commit.** Every
    ///    row mutation in the MVCC store goes through `commit_batch_timed` or
    ///    one of the two recovery restore paths; snapshot-version GC trims
    ///    superseded versions but never removes a key entry and always keeps the
    ///    newest version at or below the reader floor, so it cannot change a
    ///    family's latest row set.
    /// 2. **Every such commit publishes the family's `last_commit_seq` before
    ///    its rows become visible**, under the row-table write guard it already
    ///    holds for exactly the families it writes (`CfChangeSignal`, #2139).
    /// 3. **The memoized walk describes one instant.** It records the sequence
    ///    that served its pages and is memoized only when every page was served
    ///    by that same one ([`SynapseCalyxCfWalk::atomic`]).
    /// 4. Therefore, if `cf_change_signal(cf).last_commit_seq <=
    ///    walk.snapshot_seq_last`, **no commit has touched this family since the
    ///    walk's instant** — so the family holds the identical row set, and its
    ///    count is the identical count.
    /// 5. The one mutation that allocates no sequence — retiring a whole router
    ///    CF, or swapping the served SST level during compaction/retention GC —
    ///    is covered separately by `out_of_band_epoch`, which is bumped before
    ///    the new view is published and compared here for **equality**.
    ///
    /// # What changed in #2139, and why it mattered
    ///
    /// The condition used to be `latest_seq() == walk.snapshot_seq_last`: the
    /// **whole vault** quiescent. That is sound but almost never true — a single
    /// transcript-ingest commit to an unrelated family invalidated every entry,
    /// so on the deployed daemon the memo reused nothing and the 2.75 M-row
    /// walks continued. The per-family signal is the same proof against a
    /// condition that a busy vault can actually satisfy.
    ///
    /// Anything weaker re-measures: a non-atomic walk is never memoized, and a
    /// commit to this family or any out-of-band change to it invalidates. There
    /// is deliberately no request-count-driven periodic full rescan. The two
    /// monotone signals are the storage engine's publication invariant, not a
    /// probabilistic cache hint; re-reading an unchanged multi-gigabyte family
    /// after an arbitrary number of callers adds no evidence and recreated the
    /// resource failure this memo exists to prevent. Explicit storage audits
    /// remain able to call [`Self::count_cf_latest_bounded`] directly.
    ///
    /// # Errors
    ///
    /// Propagates any page-read failure from [`Self::walk_cf_latest`].
    pub fn count_cf_latest_bounded_memoized(
        &self,
        cf: ColumnFamily,
    ) -> Result<MemoizedCfCountReadback, SynapseCalyxError> {
        if let Some(cardinality) = self.vault.exact_cf_cardinality(cf).map_err(|error| {
            SynapseCalyxError::from_calyx("read exact maintained Calyx CF cardinality", &error)
        })? {
            let vault_latest_seq = self.vault.latest_seq();
            return Ok(MemoizedCfCountReadback {
                walk: SynapseCalyxCfWalk {
                    column_family: cf.name(),
                    page_rows: SYNAPSE_CALYX_CF_WALK_PAGE_ROWS,
                    pages: 0,
                    rows_examined: 0,
                    rows_visited: cardinality.rows,
                    stopped_early: false,
                    snapshot_seq_first: vault_latest_seq,
                    snapshot_seq_last: vault_latest_seq,
                },
                measured: false,
                maintained_exact: true,
                unchanged_since_seq: None,
                cf_last_commit_seq: cardinality.last_commit_seq,
                vault_latest_seq,
            });
        }
        let signal = self.vault.cf_change_signal(cf);
        let vault_latest_seq = self.vault.latest_seq();
        let memoized = {
            let memo = match self.cf_count_memo.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            memo.get(&cf).cloned()
        };
        let proof_holds = |entry: &MemoizedCfCount| {
            entry.out_of_band_epoch_before_walk == signal.out_of_band_epoch
                && signal.last_commit_seq <= entry.walk.snapshot_seq_last
        };
        if let Some(entry) = &memoized
            && proof_holds(entry)
        {
            return Ok(MemoizedCfCountReadback {
                walk: entry.walk.clone(),
                measured: false,
                maintained_exact: false,
                unchanged_since_seq: Some(entry.walk.snapshot_seq_last),
                cf_last_commit_seq: signal.last_commit_seq,
                vault_latest_seq,
            });
        }

        // Sampled BEFORE the walk so an out-of-band content change that lands
        // mid-walk leaves the stored epoch behind the live one and invalidates
        // this entry on the next call, rather than being memoized over.
        let out_of_band_epoch_before_walk = self.vault.cf_change_signal(cf).out_of_band_epoch;
        let walk = self.count_cf_latest_bounded(cf)?;
        if walk.atomic() {
            let installed = self
                .vault
                .install_exact_cf_cardinality(
                    cf,
                    walk.rows_visited,
                    walk.snapshot_seq_last,
                    out_of_band_epoch_before_walk,
                )
                .map_err(|error| {
                    SynapseCalyxError::from_calyx(
                        "install exact maintained Calyx CF cardinality",
                        &error,
                    )
                })?;
            tracing::info!(
                code = "SYNAPSE_CALYX_EXACT_CF_CARDINALITY_BASELINE",
                cf = cf.name(),
                rows = walk.rows_visited,
                snapshot_seq = walk.snapshot_seq_last,
                out_of_band_epoch_before_walk,
                installed,
                "physically measured an exact CF cardinality baseline for transaction maintenance"
            );
        }
        let mut memo = match self.cf_count_memo.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if walk.atomic() {
            memo.insert(
                cf,
                MemoizedCfCount {
                    walk: walk.clone(),
                    out_of_band_epoch_before_walk,
                },
            );
        } else {
            // A walk that straddled a commit cannot license a later skip.
            memo.remove(&cf);
        }
        drop(memo);
        let readback_signal = self.vault.cf_change_signal(cf);
        let readback_vault_latest_seq = self.vault.latest_seq();
        Ok(MemoizedCfCountReadback {
            walk,
            measured: true,
            maintained_exact: false,
            unchanged_since_seq: None,
            cf_last_commit_seq: readback_signal.last_commit_seq,
            vault_latest_seq: readback_vault_latest_seq,
        })
    }

    /// This column family's exact `O(1)` change signal (#2139).
    ///
    /// `(last_commit_seq, out_of_band_epoch)`. See
    /// [`Self::count_cf_latest_bounded_memoized`] for what the pair licenses; a
    /// harness needs it to state, physically, that a commit to one family moved
    /// that family's signal and nobody else's.
    #[must_use]
    pub fn cf_change_signal(&self, cf: ColumnFamily) -> (Seq, u64) {
        let signal = self.vault.cf_change_signal(cf);
        (signal.last_commit_seq, signal.out_of_band_epoch)
    }

    /// Per-site row-table read-guard counters, read at this instant.
    ///
    /// The same census `health` publishes, on the vault handle, so an in-process
    /// harness can bracket a call and read the hold it actually cost (#1960).
    /// Counting *every* hold rather than only the over-budget ones is what makes
    /// a sub-budget path distinguishable from one that never ran.
    #[must_use]
    pub fn row_guard_census(&self) -> Vec<SynapseCalyxRowGuardSiteCensus> {
        self.vault
            .row_guard_census()
            .into_iter()
            .map(|entry| SynapseCalyxRowGuardSiteCensus {
                site: entry.site.as_str().to_owned(),
                holds: entry.holds,
                total_held_us: entry.total_held_us,
                max_held_us: entry.max_held_us,
                mean_held_us: entry.mean_held_us(),
                over_budget_holds: entry.over_budget_holds,
                starved_holds: entry.starved_holds,
            })
            .collect()
    }

    /// Scans visible raw CF rows in a key range at a numeric snapshot.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the snapshot is stale,
    /// blocked by a read barrier, or unavailable from the opened recovery mode.
    pub fn scan_cf_range_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_range_at(snapshot, cf, range)
            .map_err(|error| SynapseCalyxError::from_calyx("scan Calyx CF range", &error))
    }

    /// Scans visible raw CF rows in a key range through an explicit pinned
    /// snapshot lease.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the lease expired, any row is
    /// blocked by a read barrier, or the opened recovery mode cannot serve it.
    pub fn scan_cf_range_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_range_snapshot(snapshot, cf, range)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("scan Calyx CF range from pinned snapshot", &error)
            })
    }

    /// Scans visible raw CF rows in a range from one atomic latest committed view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if a row is blocked by a read
    /// barrier or the latest physical serving view cannot be read.
    pub fn scan_cf_range_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.vault
            .scan_cf_range_latest(cf, range)
            .map_err(|error| SynapseCalyxError::from_calyx("scan latest Calyx CF range", &error))
    }

    /// Reads one candidate-bounded raw CF page from an atomic latest view.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error for an invalid range/cursor,
    /// blocked row, or unreadable physical serving view.
    pub fn scan_cf_range_page_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError> {
        self.vault
            .scan_cf_range_page_latest(cf, range, after_key, limit)
            .map(SynapseCalyxCfRangePage::from)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("scan latest Calyx CF range page", &error)
            })
    }

    /// Reads one candidate-bounded range page through an existing pinned
    /// snapshot lease, with one same-sequence lookahead for continuation.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the lease is expired or
    /// unavailable, or the requested page cannot be served.
    pub fn scan_cf_range_page_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError> {
        if limit == 0 {
            return Ok(SynapseCalyxCfRangePage {
                snapshot_seq: snapshot.seq(),
                rows: Vec::new(),
                resume_after: None,
                more: false,
                examined_rows: 0,
            });
        }
        let candidate_limit = limit.checked_add(1).ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_SNAPSHOT_PAGE_LIMIT_EXHAUSTED",
                "pinned Calyx page limit cannot be usize::MAX because continuation requires one lookahead row",
                "request a smaller bounded page",
            )
        })?;
        let mut rows = self
            .vault
            .scan_cf_range_page_snapshot(snapshot, cf, range, after_key, candidate_limit)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("scan pinned Calyx CF range page", &error)
            })?;
        let examined_rows = rows.len();
        let more = examined_rows > limit;
        if more {
            rows.truncate(limit);
        }
        let resume_after = rows.last().map(|(key, _value)| key.clone());
        Ok(SynapseCalyxCfRangePage {
            snapshot_seq: snapshot.seq(),
            rows,
            resume_after,
            more,
            examined_rows,
        })
    }

    /// Decodes the physical `Anchors` CF rows currently visible for one
    /// constellation id.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the Anchors range cannot be
    /// read or any physical anchor row fails to decode.
    pub fn scan_anchors_for_cx(
        &self,
        cx_id: CxId,
    ) -> Result<Vec<SynapseCalyxAnchorReadback>, SynapseCalyxError> {
        scan_anchors_for_cx_from_vault(&self.vault, cx_id)
    }

    /// Reads one exact physical `Anchors` CF row by its canonical key.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the exact row cannot be
    /// read or its physical value fails to decode.
    pub fn read_anchor_exact(
        &self,
        cx_id: CxId,
        kind: &AnchorKind,
    ) -> Result<Option<SynapseCalyxAnchorReadback>, SynapseCalyxError> {
        read_anchor_exact_from_vault(&self.vault, cx_id, kind)
    }

    /// Pins a bounded reader lease.
    ///
    /// # Errors
    ///
    /// Returns a structured error when `max_age_ms == 0`; zero-length leases
    /// are rejected so callers cannot accidentally create immediately expired
    /// snapshots and then misclassify the follow-on read failure.
    pub fn pin_reader(
        &self,
        freshness: Freshness,
        max_age_ms: u64,
    ) -> Result<Snapshot, SynapseCalyxError> {
        if max_age_ms == 0 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READER_LEASE_ZERO",
                "Calyx reader lease max_age_ms must be greater than zero",
                "request a bounded positive lease lifetime; use release_reader when the read is complete",
            ));
        }
        self.vault
            .pin_reader(freshness, max_age_ms)
            .map_err(|error| SynapseCalyxError::from_calyx("pin a Calyx reader", &error))
    }

    #[must_use]
    pub fn release_reader(&self, lease_id: u64) -> bool {
        self.vault.release_reader(lease_id)
    }

    /// Synchronizes Aster's WAL-backed group-commit batcher.
    ///
    /// A successful write has already crossed the fsynced WAL boundary. This
    /// method is an explicit ordering barrier and deliberately does not turn
    /// every staged commit sequence into a tiny checkpoint SST; owned storage
    /// maintenance checkpoints and compacts those batches as one lifecycle.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the WAL fsync or checkpoint
    /// flush fails.
    pub fn flush(&self) -> Result<(), SynapseCalyxError> {
        self.vault
            .sync_wal()
            .map_err(|error| SynapseCalyxError::from_calyx("sync Calyx Aster WAL", &error))
    }

    /// Reclaims snapshot-obsolete in-RAM MVCC version chains, one bounded pass.
    ///
    /// Every vault commit appends a full clone of its value bytes to an in-RAM
    /// version chain. Nothing in Synapse ever reclaimed them: #2122 measured the
    /// `snapshot_gc_debt` guard site at **zero holds since boot** against
    /// `read_latest`'s 193,269,889, while the daemon's private commit ratcheted
    /// 25,599 MB -> 28,206 MB with a 2.8 MB maximum drawdown. This is the call
    /// that was missing.
    ///
    /// RAM only — no durable commit lock, no flush, no SST, no WAL. See
    /// `AsterVault::snapshot_version_gc_memory_once` for the safety and
    /// boundedness arguments.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when the vault is closing
    /// (`CALYX_ASTER_VAULT_CLOSING`, which is close fencing rather than a fault)
    /// or a row-table shard lock is poisoned.
    pub fn reclaim_snapshot_versions_once(
        &self,
        budget: SynapseCalyxSnapshotVersionGcBudget,
    ) -> Result<SynapseCalyxSnapshotVersionGcPass, SynapseCalyxError> {
        let pass = self
            .vault
            .snapshot_version_gc_memory_once(budget.into())
            .map_err(|error| {
                SynapseCalyxError::from_calyx("reclaim Calyx MVCC snapshot versions", &error)
            })?;
        Ok(SynapseCalyxSnapshotVersionGcPass::from(pass))
    }

    /// The pinned-reader floor snapshot-version GC would reclaim below right
    /// now, next to the vault's current committed sequence.
    ///
    /// A floor that stops advancing is the one silent failure mode of the #2122
    /// fix: a leaked reader lease pins it, every pass then reclaims nothing, and
    /// the memory curve reverts to the bug while every counter still reports
    /// "ran, succeeded".
    #[must_use]
    pub fn snapshot_gc_floor(&self) -> (u64, u64) {
        (self.vault.snapshot_gc_floor_seq(), self.vault.latest_seq())
    }

    /// Reads physical snapshot-version GC state without mutating the vault.
    #[must_use]
    pub fn snapshot_gc_observation(&self) -> SynapseCalyxSnapshotGcObservation {
        let metrics = self.vault.snapshot_gc_counters_only();
        let leases = self.vault.reader_lease_view();
        let current_seq = self.vault.latest_seq();
        SynapseCalyxSnapshotGcObservation {
            floor_seq: leases.oldest_pinned_seq.unwrap_or(current_seq),
            current_seq,
            active_reader_leases: u64::try_from(leases.active_leases).unwrap_or(u64::MAX),
            oldest_pinned_seq: leases.oldest_pinned_seq,
            reader_lease_expired_total: leases.reader_lease_expired_total,
            versions_reclaimed_total: metrics.versions_reclaimed_total,
            bytes_reclaimed_total: metrics.bytes_freed_total,
            soft_deletes_purged_total: metrics.soft_deletes_purged_total,
            last_measured_compaction_debt: metrics.compaction_debt,
        }
    }

    /// Materializes pending durable checkpoints and advances the manifest
    /// `durable_seq` floor without running compaction.
    ///
    /// 2026-07-23 cold-start root cause: `durable_seq` previously advanced only
    /// on the 5-minute GC tick or a clean close, so a daemon kill stranded up
    /// to ~20k WAL sequences whose recovery replayed for minutes (one
    /// idempotent SST republish + directory flush per staged batch). A
    /// periodic caller of this method bounds the crash-stranded WAL tail to
    /// one checkpoint interval.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error if the checkpoint SST writes or
    /// the manifest advance fail.
    pub fn checkpoint(&self) -> Result<(), SynapseCalyxError> {
        self.vault
            .checkpoint()
            .map_err(|error| SynapseCalyxError::from_calyx("checkpoint Calyx Aster vault", &error))
    }

    /// Checkpoints and returns the exact changed-key journal rebase outcome.
    ///
    /// # Errors
    ///
    /// Returns a structured Calyx-backed error when a pending checkpoint,
    /// router flush, lease read, or physical snapshot-delta rebase fails.
    pub fn checkpoint_with_snapshot_delta_rebase(
        &self,
    ) -> Result<SynapseCalyxSnapshotDeltaRebaseReport, SynapseCalyxError> {
        self.vault
            .checkpoint_with_snapshot_delta_rebase()
            .map(SynapseCalyxSnapshotDeltaRebaseReport::from)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "checkpoint and rebase Calyx Aster snapshot delta",
                    &error,
                )
            })
    }

    /// Flushes and closes the durable vault, then proves the lock can be
    /// reacquired before reporting a safe shutdown readback.
    ///
    /// # Every phase is timed and reported (#2100)
    ///
    /// This used to emit three log lines across the whole close. On the
    /// production deploy in #2100 the last of them
    /// (`SYNAPSE_CALYX_VAULT_FLUSHED`, 00:16:29.22Z) was the last line the
    /// daemon ever wrote: the drain escalated 87 s later, mid-close, so the
    /// graceful exit record never landed and the next boot read
    /// `previous_shutdown=dirty`. The *successful* close one generation earlier
    /// shows the same shape — 63 s of complete silence between
    /// `SYNAPSE_CALYX_VAULT_FLUSHED` and
    /// `SYNAPSE_CALYX_VAULT_LINEAGE_CLOSE_RECORDED` — so the cost was structural,
    /// not a one-off, and no post-hoc analysis could attribute it because
    /// nothing in that region emits anything.
    ///
    /// Now every phase emits `SYNAPSE_CALYX_VAULT_CLOSE_PHASE` with its own
    /// elapsed time, and a phase past [`CLOSE_PHASE_SLOW_BUDGET_MS`] escalates
    /// to `SYNAPSE_CALYX_VAULT_CLOSE_PHASE_SLOW`. That serves two callers at
    /// once: an operator gets the attribution, and the deploy drain gets a
    /// **liveness heartbeat** — its exit-wait now extends while the daemon log's
    /// last write keeps advancing and escalates on a stall instead of on wall
    /// clock, which is only sound if a working close actually writes something.
    ///
    /// # Errors
    ///
    /// Returns an error when flush, the sealed-memtable drain, PID-sidecar
    /// cleanup, lock release, or the re-lock proof fails.
    pub fn close(
        self,
        reason: &'static str,
    ) -> Result<SynapseCalyxVaultCloseReadback, SynapseCalyxError> {
        self.close_with_mode(reason, false)
    }

    /// Completes every durable close obligation for a daemon that is committed
    /// to terminate, then retains the resident vault graph and exclusive vault
    /// lock until operating-system process reclamation.
    ///
    /// This path is intentionally separate from [`Self::close`]. Calling it for
    /// an in-process reopen would strand the vault lock and is a lifecycle bug.
    ///
    /// # Errors
    ///
    /// Returns an error when any durable flush/drain, lineage record, math
    /// runtime close, or exact retained-lock identity proof fails.
    pub fn close_for_process_exit(
        self,
        reason: &'static str,
    ) -> Result<SynapseCalyxVaultCloseReadback, SynapseCalyxError> {
        self.close_with_mode(reason, true)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "close fencing, durable drain, lock disposition, and re-lock proof are one ordered shutdown transaction"
    )]
    fn close_with_mode(
        self,
        reason: &'static str,
        terminal_process: bool,
    ) -> Result<SynapseCalyxVaultCloseReadback, SynapseCalyxError> {
        let Self {
            config,
            vault,
            anneal_ledger_index: _,
            lock,
            math_runtime,
            open_mode: _,
            lineage,
            cf_count_memo: _,
            guard_serving_memo: _,
        } = self;
        let close_started = std::time::Instant::now();
        let vault_dir = config.vault_dir.clone();
        let latest_seq = vault.latest_seq();
        let closing_vault_id = vault.vault_id().to_string();

        // Phase 1: declare the close and prepare the SST fan-out under the close
        // fence. `compact_native_fanout_for_close` raises the fence FIRST, so
        // every maintenance pass admitted after this instant is refused and
        // every parked waiter is released; the close then reserves at the head
        // of the admission queue on a 10 s budget instead of the periodic lane's
        // 120 s. A refused admission is reported, not fatal — see that method.
        let phase_started = begin_close_phase(reason, "fanout_prepare", close_started, &vault_dir)?;
        let close_compaction = vault
            .compact_native_fanout_for_close(reason)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "checkpoint and prepare Calyx SST fan-out before close",
                    &error,
                )
            })?;
        let fanout_prepared = close_compaction.is_some();
        let compaction_attempts = close_compaction.map_or(0, |results| results.len());
        complete_close_phase(
            reason,
            "fanout_prepare",
            phase_started,
            close_started,
            &vault_dir,
        )?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_CLOSE_FANOUT_READY",
            reason,
            vault_dir = %vault_dir.display(),
            compaction_attempts,
            fanout_prepared,
            "checkpointed pending commits and prepared native SST fan-out before final router flush"
        );

        // Phase 2: the final flush.
        let phase_started = begin_close_phase(reason, "flush", close_started, &vault_dir)?;
        vault.flush().map_err(|error| {
            SynapseCalyxError::from_calyx("flush durable Calyx Aster vault", &error)
        })?;
        complete_close_phase(reason, "flush", phase_started, close_started, &vault_dir)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_FLUSHED",
            reason,
            vault_dir = %vault_dir.display(),
            latest_seq,
            "flushed durable Calyx Aster vault before shutdown"
        );

        // Phase 3: the teardown. This is the 63-87 s region, and it used to be
        // the single statement `drop(vault)`.
        let phase_started = begin_close_phase(reason, "teardown", close_started, &vault_dir)?;
        let teardown = if terminal_process {
            vault.close_teardown_for_process_exit(reason)
        } else {
            vault.close_teardown(reason)
        };
        complete_close_phase(reason, "teardown", phase_started, close_started, &vault_dir)?;
        teardown.verdict().map_err(|error| {
            SynapseCalyxError::from_calyx(
                "drain sealed Calyx memtables during vault teardown",
                &error,
            )
        })?;

        // Phase 4: record the closing high-water mark while it is still
        // knowable. The vault is already flushed, so failing here cannot lose
        // rows — but it must fail loudly, because after the directory is deleted
        // this sibling journal is the only surviving evidence of how much was
        // there (#1875).
        let phase_started = begin_close_phase(reason, "lineage_record", close_started, &vault_dir)?;
        let lineage_after_close = lineage::evaluate_and_record(
            &config.vault_dir,
            &closing_vault_id,
            latest_seq,
            SynapseCalyxClock::from_tuning(&config.tuning)?.now(),
            None,
            // Close cannot be a genesis: the journal was already written at
            // open, so this call always takes the existing-generation path.
            lineage::VaultOpenGenesis::PreExisting,
        )?;
        complete_close_phase(
            reason,
            "lineage_record",
            phase_started,
            close_started,
            &vault_dir,
        )?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_LINEAGE_CLOSE_RECORDED",
            reason,
            vault_dir = %vault_dir.display(),
            lineage_path = %lineage_after_close.lineage_path.display(),
            vault_id = %closing_vault_id,
            generation_at_open = lineage.generation,
            generation = lineage_after_close.generation,
            latest_seq,
            "recorded the closing durable high-water mark in the vault lineage journal"
        );
        let phase_started =
            begin_close_phase(reason, "math_runtime_close", close_started, &vault_dir)?;
        let gpu_reservation_release = match math_runtime.close() {
            Ok(readback) => readback,
            Err(error) => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_MATH_CLOSE_FAILED",
                    reason,
                    vault_dir = %config.vault_dir.display(),
                    error = %error,
                    "retaining the Calyx vault lock and PID sidecar until process exit because the GPU reservation did not reach a verified terminal release"
                );
                std::mem::forget(lock);
                return Err(error);
            }
        };
        complete_close_phase(
            reason,
            "math_runtime_close",
            phase_started,
            close_started,
            &vault_dir,
        )?;
        let phase_started = begin_close_phase(reason, "lock_release", close_started, &vault_dir)?;
        let lock_readback = if terminal_process {
            lock.retain_until_process_exit(reason)?
        } else {
            lock.close(reason)?
        };
        complete_close_phase(
            reason,
            "lock_release",
            phase_started,
            close_started,
            &vault_dir,
        )?;
        let readback = SynapseCalyxVaultCloseReadback {
            enabled: true,
            reason,
            closed: true,
            safe_to_unlock: lock_readback.safe_to_unlock,
            vault_dir: Some(config.vault_dir.clone()),
            lock_path: Some(lock_readback.lock_path),
            pid_path: Some(lock_readback.pid_path),
            pid_sidecar_present_after_close: Some(lock_readback.pid_sidecar_present_after_close),
            re_lock_probe_succeeded: Some(lock_readback.re_lock_probe_succeeded),
            latest_seq: Some(latest_seq),
            gpu_reservation_release,
            safe_to_terminate: lock_readback.safe_to_terminate,
            terminal_process_reclaim: teardown.terminal_process_reclaim,
            lock_retained_until_process_exit: lock_readback.lock_retained_until_process_exit,
        };
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_CLOSED",
            reason,
            vault_dir = %config.vault_dir.display(),
            safe_to_unlock = readback.safe_to_unlock,
            pid_sidecar_present_after_close = readback.pid_sidecar_present_after_close,
            re_lock_probe_succeeded = readback.re_lock_probe_succeeded,
            gpu_reservation_release = ?readback.gpu_reservation_release,
            latest_seq,
            safe_to_terminate = readback.safe_to_terminate,
            terminal_process_reclaim = readback.terminal_process_reclaim,
            lock_retained_until_process_exit = readback.lock_retained_until_process_exit,
            close_total_ms = u64::try_from(close_started.elapsed().as_millis())
                .unwrap_or(u64::MAX),
            "closed durable Calyx Aster vault"
        );
        Ok(readback)
    }
}

const CLOSE_PHASE_SEQUENCE_SHIFT: u32 = 8;
const CLOSE_PHASE_INDEX_SHIFT: u32 = 1;
const CLOSE_PHASE_STATE_COMPLETE: u64 = 1;
static CLOSE_PHASE_BEACON: AtomicU64 = AtomicU64::new(0);

/// One lock-free, process-global view of the exact Calyx close phase (#2148,
/// #2149). One atomic word carries phase, begin/complete state, and a monotonic
/// sequence so readers cannot observe a torn pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxClosePhaseSnapshot {
    pub sequence: u64,
    pub phase: &'static str,
    pub transition: &'static str,
}

/// Reads the exact current close phase. `None` means no Calyx close has begun in
/// this process; callers must not substitute log volume as phase progress.
#[must_use]
pub fn current_close_phase_snapshot() -> Option<SynapseCalyxClosePhaseSnapshot> {
    decode_close_phase(CLOSE_PHASE_BEACON.load(Ordering::Acquire))
}

const fn close_phase_index(phase: &str) -> Option<u8> {
    match phase.as_bytes() {
        b"fanout_prepare" => Some(1),
        b"flush" => Some(2),
        b"teardown" => Some(3),
        b"lineage_record" => Some(4),
        b"math_runtime_close" => Some(5),
        b"lock_release" => Some(6),
        _ => None,
    }
}

const fn close_phase_name(index: u8) -> Option<&'static str> {
    match index {
        1 => Some("fanout_prepare"),
        2 => Some("flush"),
        3 => Some("teardown"),
        4 => Some("lineage_record"),
        5 => Some("math_runtime_close"),
        6 => Some("lock_release"),
        _ => None,
    }
}

fn decode_close_phase(packed: u64) -> Option<SynapseCalyxClosePhaseSnapshot> {
    if packed == 0 {
        return None;
    }
    let sequence = packed >> CLOSE_PHASE_SEQUENCE_SHIFT;
    let index = u8::try_from((packed >> CLOSE_PHASE_INDEX_SHIFT) & 0x7f).ok()?;
    let phase = close_phase_name(index)?;
    let transition = if packed & CLOSE_PHASE_STATE_COMPLETE == 0 {
        "begin"
    } else {
        "complete"
    };
    Some(SynapseCalyxClosePhaseSnapshot {
        sequence,
        phase,
        transition,
    })
}

fn publish_close_phase(
    phase: &'static str,
    complete: bool,
) -> Result<SynapseCalyxClosePhaseSnapshot, SynapseCalyxError> {
    let index = close_phase_index(phase).ok_or_else(|| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_CLOSE_PHASE_UNKNOWN",
            format!("close attempted to publish unknown phase {phase:?}"),
            "use the fixed ordered Calyx close-phase table",
        )
    })?;
    loop {
        let previous_packed = CLOSE_PHASE_BEACON.load(Ordering::Acquire);
        let previous = decode_close_phase(previous_packed);
        let valid = match (previous, complete) {
            (None, false) => index == 1,
            (Some(snapshot), true) => snapshot.phase == phase && snapshot.transition == "begin",
            (Some(snapshot), false) => {
                snapshot.transition == "complete"
                    && close_phase_index(snapshot.phase)
                        .is_some_and(|previous_index| index == previous_index.saturating_add(1))
            }
            (None, true) => false,
        };
        if !valid {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CLOSE_PHASE_REGRESSION",
                format!(
                    "refused close-phase transition phase={phase} transition={} previous={previous:?}",
                    if complete { "complete" } else { "begin" }
                ),
                "preserve the fixed begin/complete order for every Calyx close phase; restart after inspecting the last phase",
            ));
        }
        let previous_sequence = previous.map_or(0, |snapshot| snapshot.sequence);
        let sequence = previous_sequence.checked_add(1).ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_CLOSE_PHASE_SEQUENCE_EXHAUSTED",
                "the process-global Calyx close-phase sequence exhausted u64",
                "restart the daemon; a process performs at most one terminal vault close",
            )
        })?;
        if sequence > (u64::MAX >> CLOSE_PHASE_SEQUENCE_SHIFT) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_CLOSE_PHASE_SEQUENCE_EXHAUSTED",
                format!("close-phase sequence {sequence} cannot fit in the atomic beacon"),
                "restart the daemon; a process performs at most one terminal vault close",
            ));
        }
        let next = (sequence << CLOSE_PHASE_SEQUENCE_SHIFT)
            | (u64::from(index) << CLOSE_PHASE_INDEX_SHIFT)
            | u64::from(complete);
        if CLOSE_PHASE_BEACON
            .compare_exchange(previous_packed, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return decode_close_phase(next).ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_CLOSE_PHASE_ENCODE_FAILED",
                    format!("published undecodable close-phase word {next}"),
                    "inspect the close-phase encoder; never infer progress from logs",
                )
            });
        }
    }
}

fn begin_close_phase(
    reason: &'static str,
    phase: &'static str,
    close_started: std::time::Instant,
    vault_dir: &std::path::Path,
) -> Result<std::time::Instant, SynapseCalyxError> {
    let snapshot = publish_close_phase(phase, false)?;
    tracing::info!(
        code = "SYNAPSE_CALYX_VAULT_CLOSE_PHASE_BEGIN",
        reason,
        phase,
        phase_sequence = snapshot.sequence,
        close_elapsed_ms = u64::try_from(close_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        vault_dir = %vault_dir.display(),
        "began one exact Calyx vault close phase"
    );
    Ok(std::time::Instant::now())
}

/// Above this, one close phase is reported as slow rather than merely recorded.
///
/// Two seconds, not because a slower phase is a fault, but because the deploy
/// drain's stall detector needs a line to be *emitted* well inside its stall
/// budget: a phase that runs for a minute must say so while it is running, not
/// after. Under it the phase still emits its ordinary record, so the log always
/// advances at least once per phase.
const CLOSE_PHASE_SLOW_BUDGET_MS: u128 = 2_000;

/// Emits one close-phase record (issue #2100).
///
/// Every phase emits, unconditionally. That is deliberate: this is not only
/// telemetry, it is the liveness signal the deploy drain's stall-based exit-wait
/// reads off the daemon log's last-write timestamp. A phase that emitted only
/// when slow would leave the drain unable to tell a working close from a hung
/// one, which is the exact ambiguity #2100 ask 1 exists to remove.
fn complete_close_phase(
    reason: &'static str,
    phase: &'static str,
    phase_started: std::time::Instant,
    close_started: std::time::Instant,
    vault_dir: &std::path::Path,
) -> Result<(), SynapseCalyxError> {
    let snapshot = publish_close_phase(phase, true)?;
    let phase_ms = phase_started.elapsed().as_millis();
    let close_elapsed_ms = close_started.elapsed().as_millis();
    if phase_ms > CLOSE_PHASE_SLOW_BUDGET_MS {
        tracing::warn!(
            code = "SYNAPSE_CALYX_VAULT_CLOSE_PHASE_SLOW",
            reason,
            phase,
            phase_sequence = snapshot.sequence,
            phase_ms = u64::try_from(phase_ms).unwrap_or(u64::MAX),
            close_elapsed_ms = u64::try_from(close_elapsed_ms).unwrap_or(u64::MAX),
            slow_budget_ms = u64::try_from(CLOSE_PHASE_SLOW_BUDGET_MS).unwrap_or(u64::MAX),
            vault_dir = %vault_dir.display(),
            "a Calyx vault close phase exceeded its reporting budget; the drain's exit-wait extends \
             while these records keep advancing and escalates when they stop"
        );
        return Ok(());
    }
    tracing::info!(
        code = "SYNAPSE_CALYX_VAULT_CLOSE_PHASE",
        reason,
        phase,
        phase_sequence = snapshot.sequence,
        phase_ms = u64::try_from(phase_ms).unwrap_or(u64::MAX),
        close_elapsed_ms = u64::try_from(close_elapsed_ms).unwrap_or(u64::MAX),
        vault_dir = %vault_dir.display(),
        "completed one Calyx vault close phase"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VaultIdentityDisk {
    schema_version: u32,
    vault_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VaultIdentity {
    vault_id: String,
    machine_salt: Vec<u8>,
    /// True when this call minted `vault-identity.json` because none existed.
    /// This is the vault's `initdb` moment and the only point at which genesis
    /// is knowable (#1884); nothing later can recover it.
    created_this_open: bool,
}

impl VaultIdentity {
    fn parse_vault_id(&self) -> Result<VaultId, SynapseCalyxError> {
        VaultId::from_str(&self.vault_id).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_VAULT_ID_INVALID",
                format!("parse vault id {}: {error}", self.vault_id),
                IDENTITY_REMEDIATION,
            )
        })
    }
}

#[derive(Debug)]
struct VaultLockGuard {
    file: File,
    path: PathBuf,
    pid_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the internal lock readback preserves each separately observed lock/PID/relock fact"
)]
struct VaultLockCloseReadback {
    lock_path: PathBuf,
    pid_path: PathBuf,
    pid_sidecar_present_after_close: bool,
    re_lock_probe_succeeded: bool,
    safe_to_unlock: bool,
    safe_to_terminate: bool,
    lock_retained_until_process_exit: bool,
}

impl VaultLockGuard {
    fn acquire(vault_dir: &Path) -> Result<Self, SynapseCalyxError> {
        let path = lock_path(vault_dir);
        let pid_path = pid_path(vault_dir);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                SynapseCalyxError::with_io(
                    "SYNAPSE_CALYX_LOCK_OPEN_FAILED",
                    "open Calyx vault lock",
                    &path,
                    &error,
                    LOCK_REMEDIATION,
                )
            })?;
        if let Err(error) = file.try_lock_exclusive() {
            let holder = read_optional_to_string(&pid_path)
                .unwrap_or_else(|read_error| format!("pid sidecar read failed: {read_error}"));
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_LOCK_HELD",
                format!(
                    "Calyx vault lock {} is held or unavailable: {error}; holder_readback={holder}",
                    path.display()
                ),
                LOCK_REMEDIATION,
            ));
        }
        write_pid_sidecar(&pid_path).inspect_err(|_error| {
            let _ = file.unlock();
        })?;
        Ok(Self {
            file,
            path,
            pid_path,
        })
    }

    fn close(self, reason: &'static str) -> Result<VaultLockCloseReadback, SynapseCalyxError> {
        let path = self.path.clone();
        let pid_path = self.pid_path.clone();
        if let Err(error) = fs::remove_file(&pid_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(
                code = "SYNAPSE_CALYX_PID_SIDECAR_REMOVE_FAILED",
                reason,
                pid_path = %pid_path.display(),
                error = %error,
                "retaining Calyx vault lock until process exit because PID sidecar removal failed"
            );
            std::mem::forget(self);
            return Err(SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_PID_SIDECAR_REMOVE_FAILED",
                "remove Calyx vault PID sidecar",
                &pid_path,
                &error,
                CLOSE_REMEDIATION,
            ));
        }
        if let Err(error) = self.file.unlock() {
            tracing::error!(
                code = "SYNAPSE_CALYX_LOCK_RELEASE_FAILED",
                reason,
                lock_path = %path.display(),
                error = %error,
                "retaining Calyx vault lock file handle until process exit because unlock failed"
            );
            std::mem::forget(self);
            return Err(SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_LOCK_RELEASE_FAILED",
                "release Calyx vault lock",
                &path,
                &error,
                CLOSE_REMEDIATION,
            ));
        }
        let pid_sidecar_present_after_close = pid_path.exists();
        let re_lock_probe_succeeded = probe_relock(&path)?;
        let readback = VaultLockCloseReadback {
            lock_path: path,
            pid_path,
            pid_sidecar_present_after_close,
            re_lock_probe_succeeded,
            safe_to_unlock: !pid_sidecar_present_after_close && re_lock_probe_succeeded,
            safe_to_terminate: !pid_sidecar_present_after_close && re_lock_probe_succeeded,
            lock_retained_until_process_exit: false,
        };
        if !readback.safe_to_unlock {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_LOCK_CLOSE_READBACK_FAILED",
                format!("readback={readback:?}"),
                CLOSE_REMEDIATION,
            ));
        }
        Ok(readback)
    }

    fn retain_until_process_exit(
        self,
        reason: &'static str,
    ) -> Result<VaultLockCloseReadback, SynapseCalyxError> {
        let path = self.path.clone();
        let pid_path = self.pid_path.clone();
        let identity = self.verify_process_exit_owner();
        let (current_pid, canonical_current_exe) = match identity {
            Ok(identity) => identity,
            Err(error) => {
                tracing::error!(
                    code = "SYNAPSE_CALYX_PROCESS_EXIT_LOCK_RETENTION_FAILED",
                    reason,
                    lock_path = %path.display(),
                    pid_path = %pid_path.display(),
                    error = %error,
                    "retaining the unverified vault-lock handle until process exit and failing the close"
                );
                std::mem::forget(self);
                return Err(error);
            }
        };

        tracing::info!(
            code = "SYNAPSE_CALYX_LOCK_RETAINED_FOR_PROCESS_EXIT",
            reason,
            pid = current_pid,
            lock_path = %path.display(),
            pid_path = %pid_path.display(),
            executable = %canonical_current_exe.display(),
            "retaining the exact verified Calyx vault lock owner until operating-system process reclamation"
        );
        std::mem::forget(self);
        Ok(VaultLockCloseReadback {
            lock_path: path,
            pid_path,
            pid_sidecar_present_after_close: true,
            re_lock_probe_succeeded: false,
            safe_to_unlock: false,
            safe_to_terminate: true,
            lock_retained_until_process_exit: true,
        })
    }

    fn verify_process_exit_owner(&self) -> Result<(u64, PathBuf), SynapseCalyxError> {
        let sidecar = read_optional_to_string(&self.pid_path).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_PROCESS_EXIT_PID_SIDECAR_READ_FAILED",
                format!(
                    "read retained Calyx vault PID sidecar {} before process exit: {error}",
                    self.pid_path.display()
                ),
                CLOSE_REMEDIATION,
            )
        })?;
        if sidecar.trim().is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PROCESS_EXIT_PID_SIDECAR_EMPTY",
                format!(
                    "retained Calyx vault lock {} has an empty PID sidecar {}",
                    self.path.display(),
                    self.pid_path.display()
                ),
                CLOSE_REMEDIATION,
            ));
        }
        let parsed: serde_json::Value = serde_json::from_str(&sidecar).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_PROCESS_EXIT_PID_SIDECAR_INVALID",
                format!(
                    "decode retained Calyx vault PID sidecar {}: {error}",
                    self.pid_path.display()
                ),
                CLOSE_REMEDIATION,
            )
        })?;
        let recorded_pid = parsed.get("pid").and_then(serde_json::Value::as_u64);
        let recorded_exe = parsed
            .get("exe")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_PROCESS_EXIT_PID_SIDECAR_IDENTITY_MISSING",
                    format!(
                        "retained Calyx vault PID sidecar {} does not contain an executable identity",
                        self.pid_path.display()
                    ),
                    CLOSE_REMEDIATION,
                )
            })?;
        let current_pid = u64::from(std::process::id());
        let current_exe = std::env::current_exe().map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_PROCESS_EXIT_EXE_READ_FAILED",
                format!("read current executable before retaining vault lock: {error}"),
                CLOSE_REMEDIATION,
            )
        })?;
        let canonical_recorded_exe = fs::canonicalize(recorded_exe).map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_PROCESS_EXIT_RECORDED_EXE_CANONICALIZE_FAILED",
                "canonicalize recorded vault-owner executable",
                Path::new(recorded_exe),
                &error,
                CLOSE_REMEDIATION,
            )
        })?;
        let canonical_current_exe = fs::canonicalize(&current_exe).map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_PROCESS_EXIT_CURRENT_EXE_CANONICALIZE_FAILED",
                "canonicalize current vault-owner executable",
                &current_exe,
                &error,
                CLOSE_REMEDIATION,
            )
        })?;
        #[cfg(windows)]
        let executable_matches = canonical_recorded_exe
            .to_string_lossy()
            .eq_ignore_ascii_case(&canonical_current_exe.to_string_lossy());
        #[cfg(not(windows))]
        let executable_matches = canonical_recorded_exe == canonical_current_exe;
        if recorded_pid != Some(current_pid) || !executable_matches {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_PROCESS_EXIT_LOCK_IDENTITY_MISMATCH",
                format!(
                    "refused terminal lock retention: sidecar_pid={recorded_pid:?} current_pid={current_pid} sidecar_exe={} current_exe={}",
                    canonical_recorded_exe.display(),
                    canonical_current_exe.display()
                ),
                CLOSE_REMEDIATION,
            ));
        }
        Ok((current_pid, canonical_current_exe))
    }
}

fn status_from_vault(
    config: &SynapseCalyxConfig,
    vault: &AsterVault<SynapseCalyxClock>,
    math_backend: &SynapseCalyxMathBackendStatus,
    open_mode: SynapseCalyxVaultOpenMode,
) -> SynapseCalyxVaultStatus {
    let recovery_report = vault.recovery_report();
    let mvcc_resident = vault.mvcc_resident_status();
    let memtable = vault.memtable_status();
    let reader_cache = vault.sst_reader_cache_status();
    let retained_lookup_per_cf = vault
        .retained_lookup_usage_by_cf()
        .into_iter()
        .map(|(cf, value)| SynapseCalyxRetainedLookupStatus {
            cf: cf.name(),
            files: value.files as u64,
            entries: value.entries as u64,
            estimated_heap_bytes: value.estimated_heap_bytes as u64,
        })
        .collect::<Vec<_>>();
    let retained_lookup_files = retained_lookup_per_cf.iter().map(|value| value.files).sum();
    let retained_lookup_entries = retained_lookup_per_cf
        .iter()
        .map(|value| value.entries)
        .sum();
    let retained_lookup_estimated_heap_bytes = retained_lookup_per_cf
        .iter()
        .map(|value| value.estimated_heap_bytes)
        .sum();
    let mut status = SynapseCalyxVaultStatus {
        enabled: true,
        phase: "open".to_owned(),
        open: true,
        open_mode: Some(open_mode.as_str().to_owned()),
        restore_mvcc_rows: Some(open_mode.restore_mvcc_rows()),
        eager_router_lookup_on_open: Some(open_mode.eager_router_lookup_on_open()),
        vault_id: Some(vault.vault_id().to_string()),
        latest_seq: Some(vault.latest_seq()),
        last_recovered_seq: Some(recovery_report.last_recovered_seq),
        torn_tail: recovery_report
            .torn_tail
            .as_ref()
            .map(|tail| format!("{tail:?}")),
        mvcc_resident_keys: Some(mvcc_resident.keys),
        mvcc_resident_versions: Some(mvcc_resident.versions),
        mvcc_resident_key_bytes: Some(mvcc_resident.key_bytes),
        mvcc_resident_value_bytes: Some(mvcc_resident.value_bytes),
        mvcc_resident_payload_bytes: Some(mvcc_resident.payload_bytes()),
        memtable_used_bytes: Some(memtable.total_used_bytes),
        memtable_cap_bytes: Some(memtable.total_cap_bytes),
        memtable_high_water_bytes: Some(
            memtable
                .per_cf
                .iter()
                .map(|entry| entry.high_water_bytes)
                .sum(),
        ),
        sst_reader_cache_entries: Some(reader_cache.entries as u64),
        sst_reader_cache_estimated_heap_bytes: Some(reader_cache.estimated_heap_bytes as u64),
        sst_reader_cache_mapped_bytes: Some(reader_cache.mapped_bytes as u64),
        sst_reader_cache_max_entries: Some(reader_cache.max_entries as u64),
        sst_reader_cache_max_estimated_heap_bytes: Some(
            reader_cache.max_estimated_heap_bytes as u64,
        ),
        sst_reader_cache_max_mapped_bytes: Some(reader_cache.max_mapped_bytes as u64),
        retained_lookup_files: Some(retained_lookup_files),
        retained_lookup_entries: Some(retained_lookup_entries),
        retained_lookup_estimated_heap_bytes: Some(retained_lookup_estimated_heap_bytes),
        retained_lookup_per_cf,
        ..SynapseCalyxVaultStatus::default()
    };
    status.apply_paths(config);
    status.math_backend = Some(math_backend.clone());
    status.assay_compute_backend =
        configured_compute_backend().map(|backend| backend.as_str().to_owned());
    status.row_guard_census = vault
        .row_guard_census()
        .into_iter()
        .map(|entry| SynapseCalyxRowGuardSiteCensus {
            site: entry.site.as_str().to_owned(),
            holds: entry.holds,
            total_held_us: entry.total_held_us,
            max_held_us: entry.max_held_us,
            mean_held_us: entry.mean_held_us(),
            over_budget_holds: entry.over_budget_holds,
            starved_holds: entry.starved_holds,
        })
        .collect();
    status
}

fn cleanup_open_lock(lock: VaultLockGuard, primary: SynapseCalyxError) -> SynapseCalyxError {
    match lock.close("calyx_open_failed") {
        Ok(readback) => {
            tracing::info!(
                code = "SYNAPSE_CALYX_OPEN_FAILURE_LOCK_CLEANED",
                primary_code = primary.code,
                readback = ?readback,
                "closed Calyx vault lock after startup failure"
            );
            primary
        }
        Err(cleanup_error) => SynapseCalyxError::new(
            "SYNAPSE_CALYX_OPEN_FAILURE_LOCK_CLEANUP_FAILED",
            format!("primary={primary}; cleanup={cleanup_error}"),
            CLOSE_REMEDIATION,
        ),
    }
}

fn roaming_synapse_dir() -> Result<PathBuf, SynapseCalyxError> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_APPDATA_MISSING",
            "APPDATA is not set; refusing to create a non-durable fallback vault",
            APPDATA_MISSING_REMEDIATION,
        ));
    };
    Ok(PathBuf::from(appdata).join(SYNAPSE_DIR_NAME))
}

fn create_dir_all(path: &Path) -> Result<(), SynapseCalyxError> {
    fs::create_dir_all(path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_DIR_CREATE_FAILED",
            "create Calyx vault directory",
            path,
            &error,
            OPEN_REMEDIATION,
        )
    })
}

fn create_parent_dir(path: &Path) -> Result<(), SynapseCalyxError> {
    let Some(parent) = path.parent() else {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_PARENT_DIR_MISSING",
            format!("path {} has no parent directory", path.display()),
            OPEN_REMEDIATION,
        ));
    };
    fs::create_dir_all(parent).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_DIR_CREATE_FAILED",
            "create Calyx parent directory",
            parent,
            &error,
            OPEN_REMEDIATION,
        )
    })
}

fn load_or_create_identity(
    config: &SynapseCalyxConfig,
) -> Result<VaultIdentity, SynapseCalyxError> {
    let identity_path = identity_path(&config.vault_dir);
    let created_this_open = !identity_path.exists();
    if created_this_open {
        let disk = VaultIdentityDisk {
            schema_version: IDENTITY_SCHEMA_VERSION,
            vault_id: VaultId::from_ulid(Ulid::new()).to_string(),
        };
        write_identity_atomic(&identity_path, &disk)?;
    }
    let vault_id = read_identity(&identity_path)?.vault_id;
    let machine_salt = load_or_create_machine_salt(&config.machine_salt_path)?;
    Ok(VaultIdentity {
        vault_id,
        machine_salt,
        created_this_open,
    })
}

fn read_identity(path: &Path) -> Result<VaultIdentityDisk, SynapseCalyxError> {
    let raw = fs::read_to_string(path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_IDENTITY_READ_FAILED",
            "read Calyx vault identity",
            path,
            &error,
            IDENTITY_REMEDIATION,
        )
    })?;
    let identity = serde_json::from_str::<VaultIdentityDisk>(&raw).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_IDENTITY_INVALID",
            format!("parse Calyx vault identity {}: {error}", path.display()),
            IDENTITY_REMEDIATION,
        )
    })?;
    if identity.schema_version != IDENTITY_SCHEMA_VERSION {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_IDENTITY_SCHEMA_UNSUPPORTED",
            format!(
                "Calyx vault identity {} schema_version={} expected={IDENTITY_SCHEMA_VERSION}",
                path.display(),
                identity.schema_version
            ),
            IDENTITY_REMEDIATION,
        ));
    }
    VaultId::from_str(&identity.vault_id).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_VAULT_ID_INVALID",
            format!(
                "parse vault id {} from {}: {error}",
                identity.vault_id,
                path.display()
            ),
            IDENTITY_REMEDIATION,
        )
    })?;
    Ok(identity)
}

fn write_identity_atomic(
    path: &Path,
    identity: &VaultIdentityDisk,
) -> Result<(), SynapseCalyxError> {
    let encoded = serde_json::to_vec_pretty(identity).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_IDENTITY_ENCODE_FAILED",
            format!("encode Calyx vault identity {}: {error}", path.display()),
            IDENTITY_REMEDIATION,
        )
    })?;
    calyx_aster::durable_fs::write_atomic_replace(path, &encoded, "Calyx vault identity").map_err(
        |error| {
            durable_publish_error(
                "SYNAPSE_CALYX_IDENTITY_RENAME_FAILED",
                "Calyx vault identity",
                path,
                &error,
                IDENTITY_REMEDIATION,
            )
        },
    )
}

fn durable_publish_error(
    code: &'static str,
    label: &str,
    path: &Path,
    error: &CalyxError,
    remediation: &'static str,
) -> SynapseCalyxError {
    tracing::error!(
        code,
        label,
        path = %path.display(),
        calyx_code = error.code,
        calyx_remediation = error.remediation,
        "Calyx durable publish failed"
    );
    SynapseCalyxError::new(
        code,
        format!(
            "publish {label} {} failed with {}: {}",
            path.display(),
            error.code,
            error.message
        ),
        remediation,
    )
}

fn load_or_create_machine_salt(path: &Path) -> Result<Vec<u8>, SynapseCalyxError> {
    if path.exists() {
        return read_machine_salt(path);
    }
    let mut bytes = [0_u8; MACHINE_SALT_BYTES];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    write_machine_salt_atomic(path, &bytes)?;
    read_machine_salt(path)
}

fn read_machine_salt(path: &Path) -> Result<Vec<u8>, SynapseCalyxError> {
    let encoded = fs::read_to_string(path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_MACHINE_SALT_READ_FAILED",
            "read Calyx machine-local salt",
            path,
            &error,
            IDENTITY_REMEDIATION,
        )
    })?;
    let bytes = BASE64.decode(encoded.trim()).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_MACHINE_SALT_INVALID",
            format!(
                "decode Calyx machine-local salt {}: {error}",
                path.display()
            ),
            IDENTITY_REMEDIATION,
        )
    })?;
    if bytes.len() != MACHINE_SALT_BYTES {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_MACHINE_SALT_INVALID",
            format!(
                "Calyx machine-local salt {} has {} bytes expected {MACHINE_SALT_BYTES}",
                path.display(),
                bytes.len()
            ),
            IDENTITY_REMEDIATION,
        ));
    }
    Ok(bytes)
}

fn write_machine_salt_atomic(
    path: &Path,
    bytes: &[u8; MACHINE_SALT_BYTES],
) -> Result<(), SynapseCalyxError> {
    let encoded = BASE64.encode(bytes);
    calyx_aster::durable_fs::write_atomic_replace(
        path,
        encoded.as_bytes(),
        "Calyx machine-local salt",
    )
    .map_err(|error| {
        durable_publish_error(
            "SYNAPSE_CALYX_MACHINE_SALT_RENAME_FAILED",
            "Calyx machine-local salt",
            path,
            &error,
            IDENTITY_REMEDIATION,
        )
    })
}

fn anchor_batch_write_readback(
    outcome: &MultiCxAnchorBatchOutcome,
    latest_seq: Seq,
) -> SynapseCalyxAnchorBatchWriteReadback {
    SynapseCalyxAnchorBatchWriteReadback {
        anchor_count: outcome.requested_anchor_count,
        written_anchor_count: outcome.written_anchor_count,
        existing_anchor_count: outcome.existing_anchor_count,
        ledger_seq: outcome.ledger_ref.as_ref().map(|ledger_ref| ledger_ref.seq),
        ledger_hash: outcome
            .ledger_ref
            .as_ref()
            .map(|ledger_ref| hex_bytes(&ledger_ref.hash)),
        latest_seq,
    }
}

fn scan_anchors_for_cx_from_vault<C: Clock>(
    vault: &AsterVault<C>,
    cx_id: CxId,
) -> Result<Vec<SynapseCalyxAnchorReadback>, SynapseCalyxError> {
    let range = anchor_prefix_range(cx_id);
    let rows = vault
        .scan_cf_range_latest(ColumnFamily::Anchors, &range)
        .map_err(|error| SynapseCalyxError::from_calyx("scan Calyx Anchors CF", &error))?;
    rows.into_iter()
        .map(|(key, value)| {
            let anchor = vault_encode::decode_anchor(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Calyx Anchors CF row", &error)
            })?;
            Ok(SynapseCalyxAnchorReadback { key, anchor })
        })
        .collect()
}

fn read_anchor_exact_from_vault<C: Clock>(
    vault: &AsterVault<C>,
    cx_id: CxId,
    kind: &AnchorKind,
) -> Result<Option<SynapseCalyxAnchorReadback>, SynapseCalyxError> {
    let key = anchor_key(cx_id, kind);
    let value = vault
        .read_cf_latest(ColumnFamily::Anchors, &key)
        .map_err(|error| {
            SynapseCalyxError::from_calyx("read exact Calyx Anchors CF row", &error)
        })?;
    value
        .map(|value| {
            let anchor = vault_encode::decode_anchor(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode exact Calyx Anchors CF row", &error)
            })?;
            Ok(SynapseCalyxAnchorReadback { key, anchor })
        })
        .transpose()
}

fn nonblank<'a>(value: &'a str, label: &str) -> Result<&'a str, SynapseCalyxError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_ORACLE_INVALID_INPUT",
            format!("{label} must not be blank"),
            "supply the exact persisted Oracle domain and action identity",
        ));
    }
    Ok(value)
}

fn oracle_error(error: &calyx_oracle::OracleError) -> SynapseCalyxError {
    SynapseCalyxError::new(error.code(), error.to_string(), error.remediation())
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn write_pid_sidecar(path: &Path) -> Result<(), SynapseCalyxError> {
    let exe = std::env::current_exe().map_or_else(
        |error| format!("current_exe_read_failed:{error}"),
        |path| path.display().to_string(),
    );
    let body = serde_json::json!({
        "schema_version": 1,
        "pid": std::process::id(),
        "exe": exe,
    });
    let encoded = serde_json::to_vec_pretty(&body).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_PID_SIDECAR_ENCODE_FAILED",
            format!("encode Calyx vault PID sidecar {}: {error}", path.display()),
            LOCK_REMEDIATION,
        )
    })?;
    let mut file = File::create(path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_PID_SIDECAR_WRITE_FAILED",
            "create Calyx vault PID sidecar",
            path,
            &error,
            LOCK_REMEDIATION,
        )
    })?;
    file.write_all(&encoded).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_PID_SIDECAR_WRITE_FAILED",
            "write Calyx vault PID sidecar",
            path,
            &error,
            LOCK_REMEDIATION,
        )
    })?;
    file.sync_all().map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_PID_SIDECAR_SYNC_FAILED",
            "sync Calyx vault PID sidecar",
            path,
            &error,
            LOCK_REMEDIATION,
        )
    })?;
    drop(file);
    sync_parent_dir(
        path,
        "Calyx vault PID sidecar",
        "SYNAPSE_CALYX_PID_SIDECAR_PARENT_SYNC_FAILED",
        LOCK_REMEDIATION,
    )
}

fn retry_io<T>(
    code: &'static str,
    label: &str,
    operation: &'static str,
    path: &Path,
    remediation: &'static str,
    mut op: impl FnMut() -> io::Result<T>,
) -> Result<T, SynapseCalyxError> {
    let mut attempts = 0_u32;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_sharing_error(&error) && attempts < 7 => {
                attempts += 1;
                let delay = Duration::from_millis(10 * (1_u64 << (attempts - 1)));
                tracing::warn!(
                    code,
                    label,
                    operation,
                    path = %path.display(),
                    attempt = attempts,
                    retry_after_ms = delay.as_millis(),
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "retrying transient Windows Calyx durable filesystem operation"
                );
                std::thread::sleep(delay);
            }
            Err(error) => {
                tracing::error!(
                    code,
                    label,
                    operation,
                    path = %path.display(),
                    attempts,
                    kind = ?error.kind(),
                    os_error = error.raw_os_error(),
                    "Calyx durable filesystem operation failed"
                );
                return Err(SynapseCalyxError::new(
                    code,
                    format!(
                        "{operation} {label} path={} attempts={attempts} kind={:?} raw_os_error={:?}: {error}",
                        path.display(),
                        error.kind(),
                        error.raw_os_error()
                    ),
                    remediation,
                ));
            }
        }
    }
}

#[cfg(windows)]
fn is_retryable_sharing_error(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    error.raw_os_error().is_some_and(|code| {
        code == ERROR_SHARING_VIOLATION.cast_signed() || code == ERROR_LOCK_VIOLATION.cast_signed()
    })
}

#[cfg(not(windows))]
fn is_retryable_sharing_error(_error: &io::Error) -> bool {
    false
}

fn sync_parent_dir(
    path: &Path,
    label: &str,
    code: &'static str,
    remediation: &'static str,
) -> Result<(), SynapseCalyxError> {
    let Some(parent) = path.parent() else {
        return Err(SynapseCalyxError::new(
            code,
            format!("sync {label} parent for {}: no parent", path.display()),
            remediation,
        ));
    };
    sync_dir(parent, label, code, remediation)
}

#[cfg(unix)]
fn sync_dir(
    dir: &Path,
    label: &str,
    code: &'static str,
    remediation: &'static str,
) -> Result<(), SynapseCalyxError> {
    if !dir.is_dir() {
        return Err(SynapseCalyxError::new(
            code,
            format!(
                "sync {label} parent directory {}: not a directory",
                dir.display()
            ),
            remediation,
        ));
    }
    retry_io(
        code,
        label,
        "sync Calyx parent directory",
        dir,
        remediation,
        || File::open(dir).and_then(|handle| handle.sync_all()),
    )
}

#[cfg(windows)]
fn sync_dir(
    dir: &Path,
    label: &str,
    code: &'static str,
    remediation: &'static str,
) -> Result<(), SynapseCalyxError> {
    use std::os::windows::fs::OpenOptionsExt as _;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    if !dir.is_dir() {
        return Err(SynapseCalyxError::new(
            code,
            format!(
                "sync {label} parent directory {}: not a directory",
                dir.display()
            ),
            remediation,
        ));
    }
    retry_io(
        code,
        label,
        "sync Calyx parent directory",
        dir,
        remediation,
        || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(dir)
                .and_then(|handle| handle.sync_all())
        },
    )
}

#[cfg(not(any(unix, windows)))]
fn sync_dir(
    dir: &Path,
    label: &str,
    code: &'static str,
    remediation: &'static str,
) -> Result<(), SynapseCalyxError> {
    if !dir.is_dir() {
        return Err(SynapseCalyxError::new(
            code,
            format!(
                "sync {label} parent directory {}: not a directory",
                dir.display()
            ),
            remediation,
        ));
    }
    Err(SynapseCalyxError::new(
        code,
        format!(
            "sync {label} parent directory {}: unsupported platform",
            dir.display()
        ),
        remediation,
    ))
}

fn probe_relock(path: &Path) -> Result<bool, SynapseCalyxError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_LOCK_PROBE_OPEN_FAILED",
                "open Calyx vault lock for release probe",
                path,
                &error,
                CLOSE_REMEDIATION,
            )
        })?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock().map_err(|error| {
                SynapseCalyxError::with_io(
                    "SYNAPSE_CALYX_LOCK_PROBE_RELEASE_FAILED",
                    "release Calyx vault lock probe",
                    path,
                    &error,
                    CLOSE_REMEDIATION,
                )
            })?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_LOCK_PROBE_FAILED",
            "probe Calyx vault lock release",
            path,
            &error,
            CLOSE_REMEDIATION,
        )),
    }
}

fn read_optional_to_string(path: &Path) -> std::io::Result<String> {
    let mut raw = String::new();
    let mut file = File::open(path)?;
    file.read_to_string(&mut raw)?;
    Ok(raw)
}

fn identity_path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(IDENTITY_FILE_NAME)
}

/// Refuses to open a directory that physically holds a different storage
/// engine's database.
///
/// Calyx replaced `RocksDB` as the authoritative Synapse backend, so a daemon
/// pointed at a pre-migration `--db` path meets a complete `RocksDB` store. Aster
/// would read `RocksDB`'s `CURRENT`, find the uppercase `MANIFEST-<n>` pointer
/// instead of Calyx's `manifest-<20 digits>`, and report
/// `CALYX_ASTER_CORRUPT_SHARD` with `remediation=restore from restic/snapshot` —
/// a diagnosis that is both false (nothing is corrupt) and actively harmful
/// (no backup of a `RocksDB` store is a Calyx vault, and restoring one would
/// destroy the operator's real data while not fixing anything).
///
/// The `RocksDB` signature required here is deliberately conjunctive so it cannot
/// fire on a Calyx vault or on an unrelated file that merely shares a name:
/// `IDENTITY` and `CURRENT` must both exist, and `CURRENT` must point at an
/// existing uppercase `MANIFEST-<digits>` file. A Calyx vault never satisfies
/// this, because its own pointer is lowercase `manifest-`. Any other directory
/// content is left to the normal open path.
fn detect_foreign_store(vault_dir: &Path) -> Result<(), SynapseCalyxError> {
    let current_path = vault_dir.join("CURRENT");
    let identity_marker = vault_dir.join("IDENTITY");
    if !current_path.is_file() || !identity_marker.is_file() {
        return Ok(());
    }
    // A real Calyx vault carries its own identity file; never misclassify one.
    if identity_path(vault_dir).is_file() {
        return Ok(());
    }
    let Ok(pointer_raw) = read_optional_to_string(&current_path) else {
        return Ok(());
    };
    let pointer = pointer_raw.trim();
    let Some(digits) = pointer.strip_prefix("MANIFEST-") else {
        return Ok(());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    if !vault_dir.join(pointer).is_file() {
        return Ok(());
    }
    let sst_count = std::fs::read_dir(vault_dir).map_or(0_usize, |entries| {
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.to_ascii_lowercase().ends_with(".sst"))
            })
            .count()
    });
    Err(SynapseCalyxError::new(
        "SYNAPSE_CALYX_VAULT_DIR_HOLDS_FOREIGN_STORE",
        format!(
            "{} is a RocksDB database, not a Calyx vault: it has an IDENTITY file and a CURRENT pointing at {pointer} ({sst_count} .sst files). Calyx replaced RocksDB as the authoritative backend and this directory was never migrated. Nothing here is corrupt and no Calyx artifact was written to it.",
            vault_dir.display()
        ),
        "point --db / SYNAPSE_DB_PATH at a Calyx vault directory (an empty directory becomes a new vault), or migrate this RocksDB store first; do NOT restore a backup of this directory, because a RocksDB backup is not a Calyx vault and restoring it cannot make this path openable",
    ))
}

fn lock_path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(LOCK_FILE_NAME)
}

fn pid_path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(PID_FILE_NAME)
}
