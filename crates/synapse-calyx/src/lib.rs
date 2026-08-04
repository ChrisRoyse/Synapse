//! Synapse-owned lifecycle wrapper for the embedded Calyx Aster vault.

mod async_vault;
pub mod backup;
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
pub mod panel_lifecycle;
pub mod vault_runtime;
pub mod ward;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use calyx_aster::cf::{ColumnFamily, KeyRange, anchor_key, anchor_prefix_range};
use calyx_aster::compaction::CompactionResult;
use calyx_aster::dedup::EpochSecs;
use calyx_aster::erase::{EraseRegistry, EraseScope, subject_metadata_value};
use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::recurrence::{
    OccurrenceContext, RecurrenceAppendDisposition, RecurrenceAppendOnceRequest,
    RecurrenceSeriesReadback, RetentionPolicy, append_occurrence_once,
    append_occurrence_once_with_rows, read_series_readback,
};
pub use calyx_aster::vault::{
    AsterOrphanSlotCfRetirement, AsterOrphanSlotCfSkip, AsterOrphanSlotGcReport,
};
use calyx_aster::vault::{
    AsterVault, MultiCxAnchorBatchOutcome, PutDisposition, RecoveryProgressHook,
    TemporalMetadataMigration, VaultOptions, encode as vault_encode,
};
pub use calyx_core::TemporalPolicy;
use calyx_core::{
    Anchor, AnchorKind, CalyxError, Clock, Constellation, CxId, METADATA_SOURCE_EVENT_TIME_RAW,
    METADATA_SOURCE_EVENT_TIME_SECS, METADATA_TEMPORAL_LANE_STATE, Panel, Seq, SystemClock,
    TEMPORAL_LANE_ACTIVE, Ts, VaultId, VaultStore,
};
use calyx_forge::{
    HostGpuReservation, HostGpuReservationRequest, HostGpuReservationSnapshot,
    HostGpuReservationStore,
};
use calyx_ledger::{ActorId, EntryKind, LedgerEntry, SubjectId, VerifyResult};
use calyx_registry::{
    CALYX_NO_ACTIVE_PANEL, Registry, VaultPanelState, VaultPanelWrite,
    allocate_vault_panel_generation, list_vault_temporal_panels, load_vault_panel_state,
    persist_vault_panel_state, read_vault_panel_generation_allocator, read_vault_temporal_panel,
    register_vault_temporal_panel, reserve_vault_panel_generations,
};
pub use calyx_registry::{
    PanelGenerationAllocation, PanelGenerationAllocatorReadback, VaultTemporalPanelRegistration,
    VaultTemporalPanelRegistrationWrite,
};
// Re-exported so a caller can hand a non-active panel contract to
// `find_similar_in_panel` / `rebuild_search_indexes_for_panel` (#1668) without
// depending on calyx-registry directly.
pub use calyx_registry::VaultPanelState as SynapseCalyxPanelState;
pub use calyx_search::{PersistedSearchGeneration, PersistedSearchSlot};
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
    SYNAPSE_ASSAY_CORRELATION_CEILING, SYNAPSE_ASSAY_MIN_SAMPLES, SYNAPSE_INTELLIGENCE_MAX_RECORDS,
    SYNAPSE_KERNEL_DEFAULT_EDGE_COS, SYNAPSE_KERNEL_DEFAULT_KNN, SYNAPSE_KERNEL_DEFAULT_MAX_HOPS,
    SYNAPSE_KERNEL_DEFAULT_MIN_RECALL, SYNAPSE_KERNEL_MAX_REPORTED_MEMBERS, SYNAPSE_KNN_DEFAULT_K,
    SYNAPSE_KNN_MAX_EDGES, SYNAPSE_KSG_DEFAULT_K, SYNAPSE_LENS_BLIND_SPOT_CEILING,
    SYNAPSE_SYNERGY_MAX_LENSES, SYNAPSE_SYNERGY_MAX_RECORDS, SYNAPSE_TEMPORAL_DEFAULT_BIN_SECS,
    SYNAPSE_TEMPORAL_DEFAULT_MAX_LAG, SYNAPSE_TEMPORAL_MAX_PEAKS, SYNAPSE_TEMPORAL_MIN_EVENTS,
    SynapseCalyxAbundanceReport, SynapseCalyxAgreementEdge, SynapseCalyxAnchorSourceCarrier,
    SynapseCalyxAssayParams, SynapseCalyxBetweenRecordEdge, SynapseCalyxBitsReport,
    SynapseCalyxCausalityLag, SynapseCalyxCausalityReport, SynapseCalyxCorpusSlotState,
    SynapseCalyxDriftReport, SynapseCalyxEnsembleCardReport, SynapseCalyxExcludedLens,
    SynapseCalyxHazardReport, SynapseCalyxKernelAnswerHop, SynapseCalyxKernelAnswerReport,
    SynapseCalyxKernelParams, SynapseCalyxKernelReport, SynapseCalyxLensCoverageStatus,
    SynapseCalyxLowSignalLens, SynapseCalyxNeffEstimate, SynapseCalyxPanelLensCoverage,
    SynapseCalyxPeriodicityReport, SynapseCalyxPeriodogramPeak, SynapseCalyxRedundancyPair,
    SynapseCalyxRedundancyReport, SynapseCalyxRedundancySkip, SynapseCalyxSlotBits,
    SynapseCalyxSlotKind, SynapseCalyxSufficiencyDeficit, SynapseCalyxSufficiencyReport,
    SynapseCalyxSynergyReport, SynapseCalyxTemporalParams, SynapseCalyxWeaveBlindSpotPair,
    SynapseCalyxWeaveParams, SynapseCalyxWeaveReport,
};
pub use lowering::{
    LOWERED_ARTIFACT_MAGIC, LOWERED_ARTIFACT_SCHEMA_VERSION, LOWERED_DIR_NAME,
    LoadedLoweredArtifact, LoweredArtifactEnvelope, LoweredArtifactHandle, LoweredArtifactKind,
    LoweredArtifactState, LoweredFingerprint, LoweredGuardThresholds, LoweredPublishReport,
    LoweredRefreshOutcome, LoweredSafeDefault, LoweringParams, hot_context,
};
pub use math::{
    SynapseCalyxMathBackendStatus, SynapseCalyxMathProbeReport, SynapseCalyxMathProbeTopKEntry,
    SynapseCalyxMathRuntime, SynapseCalyxVramDispatchStatus, math_backend,
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
    pub generation: PersistedSearchGeneration,
    pub manifest_path: PathBuf,
    pub raw_sidecars: Vec<SynapseCalyxSearchRawSidecar>,
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
    /// seq lag exceeds the bounded reconciliation limit, so it very likely
    /// cannot either.
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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
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

/// Rows per page for [`SynapseCalyxVault::walk_cf_latest`].
///
/// Chosen so one page's row-table read guard is bounded by a small constant
/// rather than by the size of the column family. #1968 measured
/// `scan_cf_latest(Base)` at a 290 ms mean hold and a 2.65 s maximum, over
/// budget on 335 of 335 holds.
///
/// **This value was swept, not picked.** `base_walk_hold_fsv` walks the real
/// 106,787-row `Base` CF at six page sizes and reports the worst single hold
/// against `ROW_READ_GUARD_WARN_US` (25,000 us):
///
/// ```text
/// page_rows   pages  worst_hold_us  mean_hold_us   total_us  over_budget
///       256     418           4,062       2,394.9  1,001,082            0
///       512     209           5,978       4,057.0    847,918            0
///     1,024     105          11,167       7,866.4    825,970            0
///     2,048      53          16,828      14,897.4    789,560            0
///     4,096      27          33,690      28,940.9    781,404           26
///     8,192      14          67,221      56,785.9    795,002           13
/// ```
///
/// Two things decide it. **Total CPU is nearly flat** across the whole sweep
/// (1.00 s to 0.78 s), so a smaller page buys a shorter hold almost for free —
/// the per-page merge is not a fixed cost worth amortizing. And the budget has
/// a **cliff between 2,048 and 4,096**, so 2,048 would have shipped a value one
/// step from going over on a busier machine. 256 lands the worst hold at 16% of
/// budget, which is also the regime the existing `scan_cf_range_page_latest`
/// callers already operate in on the live daemon (1.91 ms mean).
///
/// The cost this does pay is ~27% more total CPU than the unpaged scan, spent
/// re-cloning each page's candidates through the merge. That is the trade: a
/// maintenance pass uses a little more CPU in total, and stops holding a lock
/// that every constellation writer needs for a third of a second.
pub const SYNAPSE_CALYX_CF_WALK_PAGE_ROWS: usize = 256;

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

/// Provenance of one bounded-hold walk over a column family (#1968).
///
/// A walk trades one long atomic view for many short ones, so the window it
/// observed is a property of the result and is reported rather than assumed.
/// `snapshot_seq_first == snapshot_seq_last` ([`Self::atomic`]) means no commit
/// landed between the first and last page, and only then is the fold's output
/// an exact census of one instant; otherwise it is a census over an interval.
///
/// This distinction is not cosmetic. Comparing a paged census against an
/// unpaged one on a *live* vault compares two different windows and disagrees
/// for a reason that is not a defect, so an equivalence check is only
/// well-posed when [`Self::atomic`] holds on both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxCfWalk {
    /// Column family walked.
    pub column_family: String,
    /// Rows requested per page.
    pub page_rows: usize,
    /// Pages actually read.
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

impl SynapseCalyxCfWalk {
    /// Whether every page was served by the same committed sequence.
    ///
    /// True means the fold's output describes one instant and is directly
    /// comparable with an unpaged `scan_cf_latest` fold. False means one or
    /// more commits landed mid-walk and the output describes an interval, or
    /// that no walk ran at all ([`Self::not_walked`]).
    #[must_use]
    pub const fn atomic(&self) -> bool {
        self.pages > 0 && self.snapshot_seq_first == self.snapshot_seq_last
    }

    /// A walk record for a value that was **not** folded from a real walk.
    ///
    /// [`SynapseCalyxVault::walk_cf_latest`] always reads at least one page, so
    /// `pages == 0` cannot describe any real walk. That makes it an unambiguous
    /// marker rather than a plausible-looking fabrication, which is the point:
    /// a hand-assembled census exercising the selection logic downstream of the
    /// fold must not be able to pass itself off as a measurement, and
    /// [`Self::atomic`] is false here for the same reason.
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

impl SynapseCalyxVaultVerifyReport {
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
        if !self.chain.raw_commitments_intact {
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
            report.intact = false;
            "corrupt".clone_into(&mut report.verdict);
            report.corrupt_reason = raw_commitments.failure;
        }
        report
    }
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
    pub subject_matches: bool,
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
        } else if !reproduction.subject_matches {
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

const DEFAULT_BIT_FLOOR_BITS: f32 = 0.05;
const DEFAULT_CORRELATION_CEILING: f32 = 0.6;
const DEFAULT_GUARD_FAR_IDENTITY: f32 = 0.01;
const DEFAULT_GUARD_FAR_CONTENT: f32 = 0.03;
const DEFAULT_GUARD_FAR_STYLISTIC: f32 = 0.05;
const DEFAULT_GUARD_COLD_START_TAU: f32 = 0.7;
const DEFAULT_KERNEL_FRACTION: f32 = 0.01;
const DEFAULT_KERNEL_RECALL_GATE: f32 = 0.95;
/// The vault's untuned `fusion_k`. Bound to the single workspace declaration
/// rather than restated, so this cannot drift from what actually scores (#1883).
const DEFAULT_FUSION_K: u32 = calyx_core::RRF_K_DEFAULT;
const DEFAULT_TEMPORAL_BOOST_MIN: f32 = 0.0;
const DEFAULT_TEMPORAL_BOOST_MAX: f32 = 0.10;
const DEFAULT_VRAM_BUDGET_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const DEFAULT_RNG_SEED: u64 = 0x5A17_5EED_CA1A_1696;

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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynapseCalyxTuningConfig {
    pub bit_floor_bits: f32,
    pub correlation_ceiling: f32,
    pub guard_far_identity: f32,
    pub guard_far_content: f32,
    pub guard_far_stylistic: f32,
    pub guard_cold_start_tau: f32,
    pub kernel_fraction: f32,
    pub kernel_recall_gate: f32,
    pub fusion_k: u32,
    pub temporal_boost_min: f32,
    pub temporal_boost_max: f32,
    pub vram_budget_bytes: u64,
    pub math_backend: SynapseCalyxMathBackend,
    pub clock_mode: SynapseCalyxClockMode,
    pub fixed_clock_unix_ms: Option<Ts>,
    pub rng_seed: u64,
}

impl Default for SynapseCalyxTuningConfig {
    fn default() -> Self {
        Self {
            bit_floor_bits: DEFAULT_BIT_FLOOR_BITS,
            correlation_ceiling: DEFAULT_CORRELATION_CEILING,
            guard_far_identity: DEFAULT_GUARD_FAR_IDENTITY,
            guard_far_content: DEFAULT_GUARD_FAR_CONTENT,
            guard_far_stylistic: DEFAULT_GUARD_FAR_STYLISTIC,
            guard_cold_start_tau: DEFAULT_GUARD_COLD_START_TAU,
            kernel_fraction: DEFAULT_KERNEL_FRACTION,
            kernel_recall_gate: DEFAULT_KERNEL_RECALL_GATE,
            fusion_k: DEFAULT_FUSION_K,
            temporal_boost_min: DEFAULT_TEMPORAL_BOOST_MIN,
            temporal_boost_max: DEFAULT_TEMPORAL_BOOST_MAX,
            vram_budget_bytes: DEFAULT_VRAM_BUDGET_BYTES,
            math_backend: SynapseCalyxMathBackend::Auto,
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
        validate_f32("bit_floor_bits", self.bit_floor_bits, 0.0, f32::INFINITY)?;
        validate_f32("correlation_ceiling", self.correlation_ceiling, 0.0, 1.0)?;
        validate_f32(
            "guard_far_identity",
            self.guard_far_identity,
            0.0,
            DEFAULT_GUARD_FAR_IDENTITY,
        )?;
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
        validate_f32("guard_cold_start_tau", self.guard_cold_start_tau, 0.0, 1.0)?;
        validate_f32("kernel_fraction", self.kernel_fraction, 0.0, 1.0)?;
        if self.kernel_fraction == 0.0 {
            return Err(invalid_config("kernel_fraction must be greater than 0.0"));
        }
        validate_f32("kernel_recall_gate", self.kernel_recall_gate, 0.0, 1.0)?;
        if self.fusion_k == 0 {
            return Err(invalid_config("fusion_k must be positive"));
        }
        validate_f32(
            "temporal_boost_min",
            self.temporal_boost_min,
            0.0,
            DEFAULT_TEMPORAL_BOOST_MAX,
        )?;
        validate_f32(
            "temporal_boost_max",
            self.temporal_boost_max,
            self.temporal_boost_min,
            DEFAULT_TEMPORAL_BOOST_MAX,
        )?;
        if self.vram_budget_bytes == 0 {
            return Err(invalid_config("vram_budget_bytes must be positive"));
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynapseCalyxClock {
    System,
    Fixed(Ts),
}

impl SynapseCalyxClock {
    fn from_tuning(config: &SynapseCalyxTuningConfig) -> Result<Self, SynapseCalyxError> {
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
        error_bridge::validate_calyx_error_bridge()?;
        let tuning = match config_path {
            Some(path) => read_tuning_config(&path)?,
            None => SynapseCalyxTuningConfig::default().validate()?,
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

fn read_tuning_config(path: &Path) -> Result<SynapseCalyxTuningConfig, SynapseCalyxError> {
    let text = fs::read_to_string(path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_CONFIG_READ_FAILED",
            "read Calyx config",
            path,
            &error,
            CONFIG_REMEDIATION,
        )
    })?;
    let file: SynapseCalyxConfigFile = toml::from_str(&text).map_err(|error| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_CONFIG_PARSE_FAILED",
            format!("parse Calyx config {}: {error}", path.display()),
            CONFIG_REMEDIATION,
        )
    })?;
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
    pub tuning: Option<SynapseCalyxTuningConfig>,
    pub math_backend: Option<SynapseCalyxMathBackendStatus>,
    /// Per-site row-table read-guard tallies since this vault was opened.
    ///
    /// Empty when the vault is not open. Every declared site appears when it
    /// is, including sites with zero holds — that zero is the observation
    /// #1952 ask 3 needed and could not get from an exception-only log.
    pub row_guard_census: Vec<SynapseCalyxRowGuardSiteCensus>,
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
        self.tuning = Some(config.tuning);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    lock: VaultLockGuard,
    math_runtime: SynapseCalyxMathRuntime,
    open_mode: SynapseCalyxVaultOpenMode,
    lineage: SynapseCalyxVaultLineage,
}

#[derive(Debug)]
pub struct SynapseCalyxReadOnlyVault {
    config: SynapseCalyxConfig,
    vault: AsterVault<SynapseCalyxClock>,
}

impl SynapseCalyxReadOnlyVault {
    /// Opens an existing vault with only the native Reactive outbox selected.
    pub fn open_existing_reactive_only(
        config: SynapseCalyxConfig,
    ) -> Result<Self, SynapseCalyxError> {
        Self::open_existing_with_cfs(config, Some(vec![ColumnFamily::Reactive]))
    }

    /// Reads all visible native Reactive outbox rows from the read-only handle.
    pub fn scan_reactive_latest(&self) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_latest(ColumnFamily::Reactive)
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
            // The public read-only handle exposes candidate-bounded paging for
            // every selected native CF, so all selected SST indexes must be
            // validated and retained at open. There is no page-time fallback
            // to whole-file scanning.
            eager_router_lookup_on_open: true,
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

    /// Reads one latest physical Base row and hydrates every declared Slot CF
    /// vector. Lifecycle workers use this as the independent post-write source
    /// of truth before completing a durable backfill task.
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

    /// Opens the configured durable Aster vault after acquiring the Synapse
    /// process lock and loading the stable vault identity.
    ///
    /// # Errors
    ///
    /// Returns an error when directories, identity files, the machine-local
    /// salt, the single-instance lock, or Calyx recovery/open fail.
    pub fn open(config: SynapseCalyxConfig) -> Result<Self, SynapseCalyxError> {
        let options = VaultOptions::default();
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
        let status = status_from_vault(&config, &vault, math_runtime.status(), open_mode);
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_OPENED",
            vault_dir = %config.vault_dir.display(),
            open_mode = open_mode.as_str(),
            restore_mvcc_rows = options.restore_mvcc_rows,
            eager_router_lookup_on_open = options.eager_router_lookup_on_open,
            lock_path = %lock.path.display(),
            pid_path = %lock.pid_path.display(),
            vault_id = status.vault_id.as_deref().unwrap_or(""),
            latest_seq = status.latest_seq,
            last_recovered_seq = status.last_recovered_seq,
            torn_tail = status.torn_tail.as_deref().unwrap_or("none"),
            elapsed_ms = started_at.elapsed().as_millis(),
            clock_mode = ?config.tuning.clock_mode,
            fixed_clock_unix_ms = config.tuning.fixed_clock_unix_ms,
            rng_seed = config.tuning.rng_seed,
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
            math_backend_cpu_avx512_available = status
                .math_backend
                .as_ref()
                .map(|math| math.cpu_avx512_available),
            math_backend_fallback_code = status
                .math_backend
                .as_ref()
                .and_then(|math| math.fallback_code.as_deref())
                .unwrap_or("none"),
            math_backend_probe_status = status
                .math_backend
                .as_ref()
                .map_or("none", |math| math.probe.status.as_str()),
            lineage_path = %lineage.lineage_path.display(),
            vault_generation = lineage.generation,
            vault_lineage_reset_count = lineage.reset_count,
            chain_origin = %lineage.chain_origin,
            "opened durable Calyx Aster vault"
        );
        Ok(Self {
            config,
            vault,
            lock,
            math_runtime,
            open_mode,
            lineage,
        })
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
    /// This calls the *same* `calyx_search::measure_panel_delta` the query
    /// path's delta collector calls, rather than mirroring it: it must count the
    /// same keys the query path counts, or the maintenance trigger and the
    /// query-time limit drift apart. It is a distinct-key count, not a sum,
    /// because the same constellation changing in the Base CF and in three slot
    /// CFs is one key to reconcile, not four. The `Base` share is scoped to
    /// `panel_version` (#1901): `Base` is shared by every panel, and counting
    /// all of it charged a 329-row timeline generation for 17,785 keys of
    /// unrelated agent-transcript ingest.
    fn measure_search_delta_changed_keys(
        &self,
        panel_version: u32,
        base_seq: u64,
        slots: &[SynapseCalyxSearchGenerationSlot],
    ) -> Result<calyx_search::PanelDeltaComposition, SynapseCalyxError> {
        // One pinned snapshot for the whole count, so every column family is
        // read against the same view and the number cannot mix sequences.
        let snapshot = self
            .vault
            .pin_reader(Freshness::FreshDerived, SEARCH_DELTA_SCAN_LEASE_MS);
        let measured = calyx_search::measure_panel_delta(
            &self.vault,
            snapshot,
            panel_version,
            base_seq,
            slots.iter().map(|slot| calyx_core::SlotId::new(slot.slot)),
        );
        // Release the lease on every path: a leaked reader lease pins the GC
        // frontier, which is a far worse outcome than a failed measurement.
        let _released = self.vault.release_reader(snapshot.lease().id());
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
        let vault_dir = self.config.vault_dir.as_path();
        let vault_latest_seq = self.vault.latest_seq();
        let max_reconciled_delta_keys = calyx_search::MAX_RECONCILED_DELTA_KEYS as u64;
        let panel_state_error = None;

        let manifest = calyx_search::manifest_path(vault_dir, panel_version);
        let manifest_present = manifest.is_file();
        let manifest_sha256 = read_optional_sha256(&manifest)?;
        let rebuild_required = calyx_search::read_rebuild_required_marker(vault_dir, panel_version)
            .map_err(|error| {
                search_rebuild_error("read rebuild-required marker for generation status", error)
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
        let sparse_slot_count = slots.iter().filter(|slot| slot.lane == "sparse").count() as u64;
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
                    let measured =
                        self.measure_search_delta_changed_keys(panel_version, base_seq, &slots)?;
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
        let snapshot = self.vault.pin_reader(Freshness::FreshDerived, 30_000);
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
        let before = self.search_generation_status_for_panel(panel_version, true)?;
        self.decide_and_maintain(panel_version, before, supplied, started)
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
                elapsed_ms: elapsed(started),
            });
        }

        // The naptime bound exists to stop a hot write stream from spending the
        // maintenance budget on back-to-back rebuilds. It must never hold a
        // *dead* generation dead: once the delta is past the query-time limit,
        // every query is already failing closed, so waiting protects nothing and
        // costs recall. Two exemptions therefore apply — an absent generation
        // (no live artifact to protect) and an already-unusable one.
        let already_unusable = delta_keys > before.max_reconciled_delta_keys;
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
            seq_lag = ?before.seq_lag,
            delta_changed_keys = ?before.delta_changed_keys,
            refresh_threshold = SEARCH_GENERATION_REFRESH_DELTA_KEYS,
            max_reconciled_delta_keys = before.max_reconciled_delta_keys,
            already_unusable,
            "building the persisted search generation unattended"
        );
        self.rebuild_search_indexes_for_panel(panel_version, supplied)?;

        // Source of truth is the manifest on disk, re-read independently of the
        // build that just claimed to write it. Scoped to the exact panel that
        // was built: reading the *active* panel's state back after building a
        // non-active generation would report a healthy generation that has
        // nothing to do with the work just performed.
        let after = self.search_generation_status_for_panel(panel_version, true)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_GENERATION_MAINTENANCE_COMMITTED",
            panel_version,
            action = action.as_str(),
            destructive = action.is_destructive(),
            after_state = %after.state,
            after_built_at_seq = ?after.built_at_seq,
            after_seq_lag = ?after.seq_lag,
            after_delta_changed_keys = ?after.delta_changed_keys,
            after_rows_covered = ?after.rows_covered,
            after_dense_lanes = after.dense_slot_count,
            after_sparse_lanes = after.sparse_slot_count,
            elapsed_ms = elapsed(started),
            "persisted search generation rebuilt unattended and re-read from disk"
        );
        Ok(SearchGenerationMaintenanceReport {
            action,
            reason,
            before,
            after: Some(after),
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
        // A supplied contract must be for exactly the requested generation:
        // rebuilding panel A's index from panel B's slot map would publish a
        // manifest whose lanes do not describe the rows it indexed.
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
        let state = match supplied {
            Some(state) => state.clone(),
            None => {
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
                        "supply the panel contract for the requested version (a code-declared generation can be reconstructed with syn_active_panel_contract), or retry with the active panel's exact version",
                    ));
                }
                active
            }
        };
        let panel_root = self
            .config
            .vault_dir
            .join("idx")
            .join("search")
            .join(format!("panel_{expected_panel_version:010}"));
        let manifest_path = panel_root.join("manifest.json");
        let before_manifest_sha256 = read_optional_sha256(&manifest_path)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_REBUILD_STARTED",
            panel_version = expected_panel_version,
            vault_dir = %self.config.vault_dir.display(),
            before_manifest_sha256 = ?before_manifest_sha256,
            "rebuilding persisted Calyx search indexes"
        );
        calyx_search::rebuild_for_vault_with_panel_state(
            &self.config.vault_dir,
            &self.vault,
            &state,
        )
        .map_err(|error| search_rebuild_error("rebuild persisted search indexes", error))?;
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
        tracing::info!(
            code = "SYNAPSE_CALYX_SEARCH_REBUILD_COMMITTED",
            panel_version = expected_panel_version,
            base_seq = generation.base_seq,
            manifest_sha256 = %generation.manifest_sha256,
            raw_sidecar_count = raw_sidecars.len(),
            "persisted Calyx search generation reopened and verified"
        );
        Ok(SynapseCalyxSearchRebuildReport {
            expected_panel_version,
            before_manifest_sha256,
            generation,
            manifest_path,
            raw_sidecars,
        })
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

    /// Appends one occurrence and, only when it is the subject's first, commits
    /// its exact region outbox row in the same durable batch.
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
            .with_maintenance_guard(|| Ok(self.verify_vault_under_guard(full_chain, tail_entries)))
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
        let restore = backup::verify_vault_restore(&vault_dir)?;
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
    /// latest-only, whose only source *is* the router. See
    /// `examples/page_walk_router_parity_fsv.rs`.
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

    /// Folds over every visible row of one column family with a **bounded**
    /// row-table read-guard hold (#1968).
    ///
    /// `scan_cf_latest` holds the `cf`'s row-table shard for the whole scan:
    /// the router's full-CF materialization, the tombstone merge, the table
    /// overlay and the barrier check all run inside one guard whose duration
    /// grows with the column family. On the live daemon that measured a 290 ms
    /// mean and a 2.65 s maximum on `Base`, over budget on 335 of 335 holds —
    /// and `Base` is the family every constellation write lands in, so a
    /// publishing MCP call could queue behind a maintenance scan for seconds.
    ///
    /// Every one of those callers was a *streaming aggregate written as a
    /// collect-then-iterate*: a 106,409-row `Vec<(Vec<u8>, Vec<u8>)>` of dense
    /// slot payloads built under the guard and then walked exactly once. This
    /// walks the same rows through `scan_cf_range_page_latest`, which bounds
    /// both serving layers per page and releases the guard between pages.
    ///
    /// This is the standard remedy rather than a local trick: RocksDB's own
    /// guidance is that long-running scans must not pin engine resources, and
    /// it shipped `Iterator::Refresh()` so a long scan can release what it
    /// pinned and re-derive the current state — the same release-between-chunks
    /// shape a resume cursor gives here.
    ///
    /// # Which column families this works on
    ///
    /// Candidate-bounded paging needs a **retained, validated SST lookup
    /// index**, and the router's open policy retains one only for `Kv`
    /// (always), and for `Base` and the slot CFs when the vault was opened with
    /// `eager_router_lookup_on_open`. `SynapseCalyxVault::open` sets that, so
    /// the daemon's live handle can page `Base`; `open_latest_readback` does
    /// not, and is only ever used as a transient adoption step that closes and
    /// reopens through `open` before any backend is published.
    ///
    /// A walk over a CF without a retained index **fails closed** with
    /// `CALYX_ASTER_SST_PAGE_INDEX_MISSING` rather than returning a partial or
    /// empty result — which is the correct outcome and is asserted by
    /// `base_walk_hold_fsv`, because an empty answer for a populated CF is
    /// exactly the class of silent wrong answer this issue exists to remove.
    /// It does mean this is not a drop-in replacement for `scan_cf_latest` on
    /// an arbitrary CF: check the open policy before migrating a new caller.
    ///
    /// # The window this trades away, and why it is reported
    ///
    /// The guard is released between pages, so a walk sees a *moving* window
    /// where `scan_cf_latest` saw one instant. That is not hidden: the returned
    /// [`SynapseCalyxCfWalk`] carries the committed sequence of the first and
    /// last page, and [`SynapseCalyxCfWalk::atomic`] is true exactly when no
    /// commit landed mid-walk. A caller publishing an exact census must report
    /// that flag rather than assume it.
    ///
    /// # Errors
    ///
    /// Returns a structured error when `page_rows` is zero, when a page reports
    /// more rows without a resume cursor, when a cursor fails to advance (both
    /// of which would spin forever), when the visitor errors, or when the
    /// underlying page read fails. The visitor's error is propagated verbatim —
    /// a fold that cannot decode a row decides that itself.
    pub fn walk_cf_latest<V>(
        &self,
        cf: ColumnFamily,
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
                    "a bounded-hold walk over {} needs a positive page size; zero rows per page cannot make forward progress",
                    cf.name()
                ),
                "pass SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, or another positive page size",
            ));
        }
        let range = KeyRange::all();
        let mut cursor: Option<Vec<u8>> = None;
        let mut walk = SynapseCalyxCfWalk {
            column_family: cf.name(),
            page_rows,
            pages: 0,
            rows_examined: 0,
            rows_visited: 0,
            stopped_early: false,
            snapshot_seq_first: 0,
            snapshot_seq_last: 0,
        };
        loop {
            let page = self.scan_cf_range_page_latest(cf, &range, cursor.as_deref(), page_rows)?;
            if walk.pages == 0 {
                walk.snapshot_seq_first = page.snapshot_seq;
            }
            walk.snapshot_seq_last = page.snapshot_seq;
            walk.pages += 1;
            walk.rows_examined += page.examined_rows;
            for (key, value) in &page.rows {
                walk.rows_visited += 1;
                if visit(key, value)? == SynapseCalyxWalkStep::Stop {
                    walk.stopped_early = true;
                    return Ok(walk);
                }
            }
            if !page.more {
                return Ok(walk);
            }
            // `more` without a cursor, or a cursor that does not advance, would
            // re-read the same page forever. Both are impossible against the
            // documented pager contract (`resume_after` is the last candidate
            // key and `after_key` is exclusive), which is exactly why they are
            // worth failing closed on rather than trusting: an unbounded silent
            // loop inside a maintenance pass is a worse outcome than an error.
            let Some(resume) = page.resume_after else {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_CF_WALK_CURSOR_MISSING",
                    format!(
                        "page {} of the {} walk reported more rows but returned no resume cursor, so the walk cannot advance",
                        walk.pages,
                        cf.name()
                    ),
                    "repair the range pager so a page reporting `more` always carries `resume_after`",
                ));
            };
            if cursor
                .as_deref()
                .is_some_and(|previous| resume.as_slice() <= previous)
            {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_CF_WALK_CURSOR_STALLED",
                    format!(
                        "page {} of the {} walk returned a resume cursor that does not advance past the previous one, so the walk would re-read the same page forever",
                        walk.pages,
                        cf.name()
                    ),
                    "repair the range pager so `resume_after` is strictly greater than the exclusive `after_key` it was given",
                ));
            }
            cursor = Some(resume);
        }
    }

    /// Counts the visible rows of one column family with a **bounded** row-guard
    /// hold, by folding [`Self::walk_cf_latest`] pages (#1973).
    ///
    /// [`Self::count_cf_latest`] reads as `O(1)` and is not: it is identical to
    /// `scan_cf_latest().len()` in time, walking every row of the family under a
    /// single hold of the vault-wide row-table read guard. On the live vault
    /// that measured a **127 ms** hold counting `Graph` (81,236 rows) against a
    /// 25 ms budget, which is the same defect #1968 removed from `Base`, on a
    /// different column family.
    ///
    /// The trade is the one #1968 already made and is stated rather than
    /// hidden: many short holds instead of one long one, so the count describes
    /// an *interval* unless [`SynapseCalyxCfWalk::atomic`] holds. Callers that
    /// use the count as physical evidence a write landed get the walk back and
    /// must report that flag rather than assume it.
    ///
    /// # Errors
    ///
    /// Propagates any page-read failure from [`Self::walk_cf_latest`].
    pub fn count_cf_latest_bounded(
        &self,
        cf: ColumnFamily,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError> {
        self.walk_cf_latest(cf, SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, |_key, _value| {
            Ok(SynapseCalyxWalkStep::Continue)
        })
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
        Ok(self.vault.pin_reader(freshness, max_age_ms))
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

    /// Flushes and closes the durable vault, then proves the lock can be
    /// reacquired before reporting a safe shutdown readback.
    ///
    /// # Errors
    ///
    /// Returns an error when flush, PID-sidecar cleanup, lock release, or the
    /// re-lock proof fails.
    pub fn close(
        self,
        reason: &'static str,
    ) -> Result<SynapseCalyxVaultCloseReadback, SynapseCalyxError> {
        let Self {
            config,
            vault,
            lock,
            math_runtime,
            open_mode: _,
            lineage,
        } = self;
        let latest_seq = vault.latest_seq();
        let closing_vault_id = vault.vault_id().to_string();
        let close_compaction = vault.compact_native_fanout_once().map_err(|error| {
            SynapseCalyxError::from_calyx(
                "checkpoint and prepare Calyx SST fan-out before close",
                &error,
            )
        })?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_CLOSE_FANOUT_READY",
            reason,
            vault_dir = %config.vault_dir.display(),
            compaction_attempts = close_compaction.len(),
            "checkpointed pending commits and prepared native SST fan-out before final router flush"
        );
        vault.flush().map_err(|error| {
            SynapseCalyxError::from_calyx("flush durable Calyx Aster vault", &error)
        })?;
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_FLUSHED",
            reason,
            vault_dir = %config.vault_dir.display(),
            latest_seq,
            "flushed durable Calyx Aster vault before shutdown"
        );
        drop(vault);
        // Record the closing high-water mark while it is still knowable. The
        // vault is already flushed, so failing here cannot lose rows — but it
        // must fail loudly, because after the directory is deleted this sibling
        // journal is the only surviving evidence of how much was there (#1875).
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
        tracing::info!(
            code = "SYNAPSE_CALYX_VAULT_LINEAGE_CLOSE_RECORDED",
            reason,
            vault_dir = %config.vault_dir.display(),
            lineage_path = %lineage_after_close.lineage_path.display(),
            vault_id = %closing_vault_id,
            generation_at_open = lineage.generation,
            generation = lineage_after_close.generation,
            latest_seq,
            "recorded the closing durable high-water mark in the vault lineage journal"
        );
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
        let lock_readback = lock.close(reason)?;
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
            "closed durable Calyx Aster vault"
        );
        Ok(readback)
    }
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
struct VaultLockCloseReadback {
    lock_path: PathBuf,
    pid_path: PathBuf,
    pid_sidecar_present_after_close: bool,
    re_lock_probe_succeeded: bool,
    safe_to_unlock: bool,
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
}

fn status_from_vault(
    config: &SynapseCalyxConfig,
    vault: &AsterVault<SynapseCalyxClock>,
    math_backend: &SynapseCalyxMathBackendStatus,
    open_mode: SynapseCalyxVaultOpenMode,
) -> SynapseCalyxVaultStatus {
    let recovery_report = vault.recovery_report();
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
        ..SynapseCalyxVaultStatus::default()
    };
    status.apply_paths(config);
    status.math_backend = Some(math_backend.clone());
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
